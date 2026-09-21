use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::Config;
use crate::dvc::{self, PreparedRevision};
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::lock::RepositoryLock;
use crate::path::{reject_symlink_traversal, repo_path, resolved_under};
use crate::s3_purge::{self, PurgeReport};
use crate::storage;

#[derive(Debug, Clone)]
pub struct RefreshOptions {
    pub repo: PathBuf,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefreshReport {
    pub status: String,
    pub repo: String,
    pub branch: String,
    pub remote: String,
    pub old_oid: String,
    pub new_oid: String,
    pub incoming_paths: Vec<String>,
    pub working_changes_before: Vec<String>,
    pub working_changes_after: Vec<String>,
    pub materialized_git_paths: Vec<String>,
    pub storage: RefreshStorageReport,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<RefreshWarning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefreshWarning {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefreshStorageReport {
    pub mode: String,
    pub changed_files: Vec<String>,
    pub old_files: Vec<String>,
    pub new_files: Vec<String>,
    /// Incoming boundaries the storage engine cannot address. Their metadata
    /// advances with the shared branch like any other Git file. Refresh
    /// neither hydrates nor verifies their payload rather than fail the whole
    /// refresh, and it keeps one this checkout holds only when that payload
    /// already matches the incoming metadata.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unaddressable: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_prepared: Option<PreparedRevision>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_prepared: Option<PreparedRevision>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub materialized: Vec<String>,
    pub purge: PurgeReport,
}

struct StorageOverlay {
    contents: Option<String>,
    checkout_output: bool,
}

pub fn execute(options: &RefreshOptions) -> Result<RefreshReport> {
    let repo = GitRepo::discover(&options.repo)?;
    let _repository_lock = RepositoryLock::acquire(&repo)?;
    let config = Config::load_compatible(&repo)?;
    let remote = config.git.remote.clone();
    let branch = config.git.branch.clone();
    repo.validate_remote_name(&remote)?;
    repo.validate_branch(&branch)?;
    let head = repo.current_branch()?;
    if head.as_deref() != Some(&branch) {
        return Err(Error::message(format!(
            "refresh requires the checkout on {branch:?}; current HEAD is {:?}",
            head.as_deref().unwrap_or("detached")
        )));
    }
    ensure_shared_index_clean(&repo)?;
    let local_ref = format!("refs/heads/{branch}");
    let old_oid = repo
        .optional_oid(&local_ref)?
        .ok_or_else(|| Error::message(format!("local branch does not exist: {branch}")))?;
    let new_oid = repo.fetch_branch(&remote, &branch)?;
    // Refuse an incoming revision that requires a newer workspace-mgr before
    // anything in the checkout changes.
    crate::config::require_supported_cli_at(&repo, &new_oid, &format!("{remote}/{branch}"))?;
    if old_oid != new_oid {
        let ancestor = repo.run_unchecked(["merge-base", "--is-ancestor", &old_oid, &new_oid])?;
        if ancestor.code == 1 {
            return Err(Error::message(format!(
                "{branch} cannot fast-forward from {old_oid} to {new_oid}"
            )));
        }
        if !ancestor.success() {
            let detail = ancestor.stderr.trim();
            return Err(Error::message(if detail.is_empty() {
                "failed to verify fast-forward ancestry".to_owned()
            } else {
                detail.to_owned()
            }));
        }
    }
    let incoming_paths = if old_oid == new_oid {
        Vec::new()
    } else {
        tree_changed_paths(&repo, &old_oid, &new_oid)?
    };
    let incoming_dvc: Vec<String> = incoming_paths
        .iter()
        .filter(|path| path.ends_with(".dvc"))
        .cloned()
        .collect();
    let incoming_git: Vec<String> = incoming_paths
        .iter()
        .filter(|path| !path.ends_with(".dvc"))
        .cloned()
        .collect();
    let old_local_boundaries = storage::local_boundaries_at(&repo, &old_oid)?;
    let mut local_boundaries = storage::local_boundaries_at(&repo, &new_oid)?;
    // A pending local choice must survive an unrelated upstream publication.
    // A published local choice may be explicitly replaced upstream; normal
    // overlay checks still preserve any existing payload when that happens.
    local_boundaries.extend(
        storage::local_boundaries(&repo, &[])?
            .difference(&old_local_boundaries)
            .cloned(),
    );
    let incoming_git: Vec<String> = incoming_git
        .into_iter()
        .filter(|path| !overlaps_local_boundary(path, &local_boundaries))
        .collect();
    let materialized_git_paths = safe_git_materialization_paths(&repo, &old_oid, &incoming_git)?;
    let old_dvc = existing_at(&repo, &old_oid, &incoming_dvc)?;
    let new_dvc = existing_at(&repo, &new_oid, &incoming_dvc)?;
    let retired_local_pointers: BTreeSet<String> = incoming_dvc
        .iter()
        .filter(|pointer| {
            !new_dvc.contains(pointer)
                && pointer
                    .strip_suffix(".dvc")
                    .is_some_and(|path| overlaps_local_boundary(path, &local_boundaries))
        })
        .cloned()
        .collect();
    for pointer in &new_dvc {
        if pointer
            .strip_suffix(".dvc")
            .is_some_and(|path| overlaps_local_boundary(path, &local_boundaries))
        {
            return Err(Error::message(format!(
                "incoming storage metadata {pointer:?} conflicts with a local-only placement; resolve that placement before refreshing"
            )));
        }
    }
    // A shared branch published before the addressability refusal can still
    // carry metadata the storage engine cannot address, and the engine cannot
    // verify such a boundary, so handing it over would fail and roll back the
    // whole refresh. Refusing the whole refresh instead would freeze inbound
    // synchronization for every checkout of this repository, while recovering
    // one boundary needs no refresh at all. So detect those boundaries here,
    // before the ref, index, worktree, purge queue, or engine state changes,
    // then advance everything else and report them.
    let (addressable_new_dvc, unaddressable_new_dvc): (Vec<String>, Vec<String>) = new_dvc
        .iter()
        .cloned()
        .partition(|pointer| dvc::is_addressable(pointer));
    let unaddressable_boundaries: Vec<String> = unaddressable_new_dvc
        .iter()
        .map(|pointer| {
            pointer
                .strip_suffix(".dvc")
                .expect("incoming metadata was selected by that extension above")
                .to_owned()
        })
        .collect();
    let warnings = unaddressable_warnings(&unaddressable_boundaries);
    let working_before = working_changes(&repo)?;
    let purge_candidates = if old_oid == new_oid {
        Vec::new()
    } else {
        s3_purge::candidates_between(&repo, &config, &old_oid, &new_oid, &[])?
    };
    let mut report = RefreshReport {
        status: if old_oid == new_oid {
            "no_changes"
        } else {
            "pending"
        }
        .to_owned(),
        repo: repo.root.display().to_string(),
        branch: branch.clone(),
        remote: remote.clone(),
        old_oid: old_oid.clone(),
        new_oid: new_oid.clone(),
        incoming_paths,
        working_changes_before: working_before.clone(),
        working_changes_after: working_before,
        materialized_git_paths: materialized_git_paths.clone(),
        storage: RefreshStorageReport {
            mode: "hydrate".to_owned(),
            changed_files: incoming_dvc.clone(),
            old_files: old_dvc.clone(),
            new_files: new_dvc.clone(),
            unaddressable: unaddressable_boundaries.clone(),
            old_prepared: None,
            new_prepared: None,
            materialized: Vec::new(),
            purge: s3_purge::preview(&repo)?,
        },
        warnings,
        method: None,
    };
    report.storage.purge.queued = purge_candidates.clone();
    if old_oid == new_oid {
        if !options.dry_run {
            report.storage.purge = s3_purge::purge_pending(&repo, &config, &remote)?;
            if !report.storage.purge.deleted.is_empty() {
                report.status = "s3_purged".to_owned();
            }
        }
        return Ok(report);
    }

    let overlays = if incoming_dvc.is_empty() {
        BTreeMap::new()
    } else {
        let overlays = capture_overlays(
            &repo,
            &old_oid,
            &new_oid,
            &incoming_dvc,
            &retired_local_pointers,
        )?;
        let updated_pointers: Vec<String> = incoming_dvc
            .iter()
            .filter(|pointer| {
                !retired_local_pointers.contains(*pointer) && dvc::is_addressable(pointer)
            })
            .cloned()
            .collect();
        if !updated_pointers.is_empty() {
            dvc::validate_worktree(&repo, &config, &updated_pointers)?;
        }
        refuse_unreconcilable_payloads(&repo, &new_oid, &unaddressable_new_dvc)?;
        overlays
    };
    if options.dry_run {
        report.status = "dry_run".to_owned();
        return Ok(report);
    }

    s3_purge::queue(&repo, &purge_candidates)?;

    let mut outputs_absent_before = Vec::new();
    if !incoming_dvc.is_empty() {
        // Retiring a local-only boundary changes its metadata, never its
        // payload. Its old remote versions may already have been purged, and
        // neither refresh nor rollback needs them to retain the local bytes.
        let old_checkout_pointers: Vec<String> = old_dvc
            .iter()
            .filter(|pointer| {
                !retired_local_pointers.contains(*pointer) && dvc::is_addressable(pointer)
            })
            .cloned()
            .collect();
        let old_prepared = dvc::prepare_revision(&repo, &config, &old_oid, &old_checkout_pointers)?;
        let new_prepared = dvc::prepare_revision(&repo, &config, &new_oid, &addressable_new_dvc)?;
        for output in new_prepared.outputs.values().flatten() {
            reject_symlink_traversal(&repo.root, output, "incoming managed-storage output")?;
        }
        let unsafe_outputs: Vec<String> = addressable_new_dvc
            .iter()
            .filter(|pointer| !resolved_under(&repo.root, pointer).is_file())
            .flat_map(|pointer| new_prepared.outputs.get(pointer).into_iter().flatten())
            .filter(|output| resolved_under(&repo.root, output).exists())
            .cloned()
            .collect();
        if !unsafe_outputs.is_empty() {
            return Err(Error::message(format!(
                "incoming stored outputs already exist without matching local metadata and will not be overwritten: {}",
                unsafe_outputs.join(", ")
            )));
        }
        outputs_absent_before = addressable_new_dvc
            .iter()
            .flat_map(|pointer| new_prepared.outputs.get(pointer).into_iter().flatten())
            .filter(|output| !resolved_under(&repo.root, output).exists())
            .cloned()
            .collect();
        report.storage.old_prepared = Some(old_prepared);
        report.storage.new_prepared = Some(new_prepared);
    }

    if repo.optional_oid(&local_ref)?.as_deref() != Some(&old_oid) {
        return Err(Error::message(format!(
            "{branch} moved during refresh; expected {old_oid}"
        )));
    }
    repo.run([
        "update-ref",
        "-m",
        &format!("workspace-mgr refresh from {remote}/{branch}"),
        &local_ref,
        &new_oid,
        &old_oid,
    ])?;
    let refreshed = (|| {
        repo.run(["read-tree", "--reset", &new_oid])?;
        materialize_git_paths(&repo, &new_oid, &materialized_git_paths)?;
        if !incoming_dvc.is_empty() {
            report.storage.materialized = materialize_metadata(&repo, &new_oid, &incoming_dvc)?;
            if !addressable_new_dvc.is_empty() {
                let args = ["checkout".to_owned(), "--".to_owned()]
                    .into_iter()
                    .chain(addressable_new_dvc.iter().cloned())
                    .collect::<Vec<_>>();
                dvc::execute_engine(&repo.root, args)?;
                dvc::verify(&repo, &config, &addressable_new_dvc)?;
            }
        }
        if repo.optional_oid(&local_ref)?.as_deref() != Some(&new_oid) {
            return Err(Error::message("refresh ref verification failed"));
        }
        ensure_shared_index_clean(&repo)?;
        Ok(())
    })();
    if let Err(refresh_error) = refreshed {
        let rollback_errors = rollback(
            &repo,
            &local_ref,
            &old_oid,
            &new_oid,
            &materialized_git_paths,
            &overlays,
            &outputs_absent_before,
        );
        return match rollback_errors {
            Ok(()) => Err(Error::message(format!(
                "refresh failed and was rolled back: {refresh_error}"
            ))),
            Err(rollback_error) => Err(Error::message(format!(
                "refresh failed after advancing {branch}: {refresh_error}; rollback also failed: {rollback_error}"
            ))),
        };
    }
    report.status = "updated".to_owned();
    report.method = Some(
        if !incoming_dvc.is_empty() {
            "prefetch managed storage, compare-and-swap the repository revision, then hydrate stored outputs"
        } else {
            "compare-and-swap the repository revision and safely materialize ordinary Git paths"
        }
        .to_owned(),
    );
    report.working_changes_after = working_changes(&repo)?;
    report.storage.purge = s3_purge::purge_pending(&repo, &config, &remote)?;
    report.storage.purge.queued = purge_candidates;
    Ok(report)
}

/// Refresh cannot hydrate, replace, or verify the payload of a boundary the
/// storage engine cannot address, so it advances that boundary's metadata only
/// where that leaves no payload in this checkout the metadata does not
/// describe. Without a payload the boundary is simply unhydrated, as in every
/// other checkout, and a payload that already matches the incoming metadata
/// byte for byte needs nothing. Any other payload would sit silently under
/// metadata that no longer describes it, and a later rename would carry it
/// into publication, so refresh refuses before anything changes, as it refuses
/// to overwrite the local output of a boundary the engine can address.
fn refuse_unreconcilable_payloads(
    repo: &GitRepo,
    new_oid: &str,
    pointers: &[String],
) -> Result<()> {
    let mut without_metadata = Vec::new();
    let mut mismatched = Vec::new();
    for pointer in pointers {
        let metadata = file_at(repo, new_oid, pointer)?.ok_or_else(|| {
            Error::message(format!(
                "incoming storage metadata is missing from {new_oid}: {pointer}"
            ))
        })?;
        let boundary = dvc::metadata_output(repo, pointer, &metadata)?;
        let payload = resolved_under(&repo.root, &boundary);
        match fs::symlink_metadata(&payload) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(Error::Io {
                    path: payload,
                    source,
                });
            }
        }
        if !resolved_under(&repo.root, pointer).is_file() {
            without_metadata.push(boundary);
        } else if !dvc::payload_matches_metadata(repo, pointer, &metadata)? {
            mismatched.push(boundary);
        }
    }
    if !without_metadata.is_empty() {
        return Err(Error::message(format!(
            "incoming stored outputs already exist without matching local metadata and will not be overwritten: {}",
            without_metadata.join(", ")
        )));
    }
    if !mismatched.is_empty() {
        return Err(Error::message(format!(
            "incoming storage metadata does not describe the payload this checkout holds for {}, and the storage engine cannot address that path to replace it; preserve that payload elsewhere or remove it, with the user's approval, then refresh again",
            mismatched.join(", ")
        )));
    }
    Ok(())
}

/// Reports incoming boundaries the storage engine cannot address, and the
/// recovery that works for them. Until one is renamed, `storage hydrate`
/// refuses it like any other unaddressable metadata, and a scope-wide hydrate
/// refuses its whole scope, so the warning says how to hydrate the rest. The
/// recovery's scope is the directory holding the boundary: the rename rewrites
/// that directory's ignore file and both metadata files, and publication stages
/// a declared path with `git add`, which refuses an exact path naming the
/// ignored payload.
fn unaddressable_warnings(boundaries: &[String]) -> Vec<RefreshWarning> {
    if boundaries.is_empty() {
        return Vec::new();
    }
    let at_root = boundaries.iter().any(|boundary| !boundary.contains('/'));
    let mut directories = boundaries
        .iter()
        .filter_map(|boundary| boundary.rsplit_once('/'))
        .map(|(directory, _)| format!("`{directory}`"))
        .collect::<Vec<_>>();
    directories.sort();
    directories.dedup();
    let mut message = format!(
        "the storage engine cannot address {}: {} a backslash, which the engine reads as a path separator. Refresh advanced the shared branch and hydrated every other incoming boundary, but did not hydrate, replace, or verify these, so they stay unhydrated in every checkout that does not already hold their exact payload. Until one is renamed, a scope-wide `workspace-mgr storage hydrate` over a scope that contains it refuses; name the other boundaries to hydrate them. Recover each one with the user's authorization for its directory, in an infrastructure task, which starts from the fetched base branch and so needs no refresh. Declare as the task's scope the directory that holds the boundary",
        boundaries.join(", "),
        if boundaries.len() == 1 {
            "its path contains"
        } else {
            "each path contains"
        },
    );
    if !directories.is_empty() {
        message.push_str(&format!(" ({})", directories.join(", ")));
    }
    message.push_str(
        ", and the directory that will hold the destination if that differs, because the rename rewrites each directory's `.gitignore` and both metadata files. In the task's worktree run `workspace-mgr move <boundary> <destination>` to a destination without backslashes, which fetches the payload through the old metadata and materializes it at the destination; hydrate the other boundaries in those directories by naming them, as in `workspace-mgr storage hydrate <path> ...`, because publication requires every boundary in its scope to be present; then publish the task and merge it. A later refresh hydrates the renamed boundary in every checkout",
    );
    if at_root {
        message.push_str(
            ". A boundary at the repository root has no directory a task can declare, so report it to the user instead",
        );
    }
    vec![RefreshWarning {
        code: "unaddressable-storage-metadata".to_owned(),
        message,
    }]
}

fn ensure_shared_index_clean(repo: &GitRepo) -> Result<()> {
    if !repo.run(["ls-files", "--unmerged"])?.stdout.is_empty() {
        return Err(Error::message("shared index has unresolved merge entries"));
    }
    let cached = repo.run_unchecked(["diff", "--cached", "--quiet"])?;
    match cached.code {
        0 => Ok(()),
        1 => Err(Error::message(
            "shared index has staged changes; refresh will not discard them",
        )),
        _ => {
            let detail = cached.stderr.trim();
            Err(Error::message(if detail.is_empty() {
                "failed to inspect shared index".to_owned()
            } else {
                detail.to_owned()
            }))
        }
    }
}

fn tree_changed_paths(repo: &GitRepo, old: &str, new: &str) -> Result<Vec<String>> {
    let output = repo.run(["diff", "--name-only", "--no-renames", "-z", old, new, "--"])?;
    let mut paths: Vec<String> = output
        .stdout
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    paths.sort();
    Ok(paths)
}

fn file_at(repo: &GitRepo, oid: &str, path: &str) -> Result<Option<String>> {
    let tree = repo.run(["ls-tree", "-z", oid, "--", path])?;
    let mut exact = tree
        .stdout
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| entry.split_once('\t'));
    let Some((metadata, actual_path)) = exact.find(|(_, actual_path)| *actual_path == path) else {
        return Ok(None);
    };
    if exact.any(|(_, duplicate)| duplicate == path) {
        return Err(Error::message(format!(
            "repository tree repeats path {path:?} at {oid}"
        )));
    }
    if metadata.split_whitespace().nth(1) != Some("blob") {
        return Err(Error::message(format!(
            "expected a file at {path:?} in {oid}"
        )));
    }
    Ok(Some(
        repo.run(["show", &format!("{oid}:{actual_path}")])?.stdout,
    ))
}

fn existing_at(repo: &GitRepo, oid: &str, paths: &[String]) -> Result<Vec<String>> {
    let mut result = Vec::new();
    for path in paths {
        if file_at(repo, oid, path)?.is_some() {
            result.push(path.clone());
        }
    }
    Ok(result)
}

fn capture_overlays(
    repo: &GitRepo,
    old: &str,
    new: &str,
    paths: &[String],
    retired_local_pointers: &BTreeSet<String>,
) -> Result<BTreeMap<String, StorageOverlay>> {
    let mut conflicts = Vec::new();
    let mut overlays = BTreeMap::new();
    for path in paths {
        reject_symlink_traversal(&repo.root, path, "incoming managed-storage metadata")?;
        let candidate = resolved_under(&repo.root, path);
        let old_content = file_at(repo, old, path)?;
        let new_content = file_at(repo, new, path)?;
        if !candidate.exists() {
            overlays.insert(
                path.clone(),
                StorageOverlay {
                    contents: None,
                    checkout_output: false,
                },
            );
            if old_content.is_some() && !retired_local_pointers.contains(path) {
                conflicts.push(path.clone());
            }
            continue;
        }
        if !candidate.is_file() || candidate.is_symlink() {
            conflicts.push(path.clone());
            continue;
        }
        let current = fs::read_to_string(&candidate).at(&candidate)?;
        overlays.insert(
            path.clone(),
            StorageOverlay {
                contents: Some(current.clone()),
                // Restoring the metadata is enough for a boundary the engine
                // cannot address: refresh never checked its output out, so
                // there is nothing for a rollback to put back.
                checkout_output: !retired_local_pointers.contains(path)
                    && dvc::is_addressable(path),
            },
        );
        if Some(&current) != old_content.as_ref() && Some(&current) != new_content.as_ref() {
            conflicts.push(path.clone());
        }
    }
    if !conflicts.is_empty() {
        return Err(Error::message(format!(
            "incoming storage metadata conflicts with active working overlays: {}",
            conflicts.join(", ")
        )));
    }
    Ok(overlays)
}

fn materialize_metadata(repo: &GitRepo, oid: &str, paths: &[String]) -> Result<Vec<String>> {
    let mut materialized = Vec::new();
    for path in paths {
        reject_symlink_traversal(&repo.root, path, "incoming managed-storage metadata")?;
        let candidate = resolved_under(&repo.root, path);
        match file_at(repo, oid, path)? {
            Some(content) => {
                if let Some(parent) = candidate.parent() {
                    fs::create_dir_all(parent).at(parent)?;
                }
                fs::write(&candidate, content).at(&candidate)?;
                materialized.push(path.clone());
            }
            None => {
                if candidate.exists() {
                    fs::remove_file(&candidate).at(&candidate)?;
                }
            }
        }
    }
    Ok(materialized)
}

fn rollback(
    repo: &GitRepo,
    local_ref: &str,
    old_oid: &str,
    new_oid: &str,
    materialized_git_paths: &[String],
    overlays: &BTreeMap<String, StorageOverlay>,
    outputs_absent_before: &[String],
) -> Result<()> {
    repo.run([
        "update-ref",
        "-m",
        "workspace-mgr refresh rollback",
        local_ref,
        old_oid,
        new_oid,
    ])?;
    repo.run(["read-tree", "--reset", old_oid])?;
    materialize_git_paths(repo, old_oid, materialized_git_paths)?;
    let mut restored = Vec::new();
    for (path, overlay) in overlays {
        reject_symlink_traversal(&repo.root, path, "rollback managed-storage metadata")?;
        let candidate = resolved_under(&repo.root, path);
        match &overlay.contents {
            Some(content) => {
                if let Some(parent) = candidate.parent() {
                    fs::create_dir_all(parent).at(parent)?;
                }
                fs::write(&candidate, content).at(&candidate)?;
                if overlay.checkout_output {
                    restored.push(path.clone());
                }
            }
            None => {
                if candidate.exists() {
                    fs::remove_file(&candidate).at(&candidate)?;
                }
            }
        }
    }
    if !restored.is_empty() {
        let args = ["checkout".to_owned(), "--".to_owned()]
            .into_iter()
            .chain(restored)
            .collect::<Vec<_>>();
        dvc::execute_engine(&repo.root, args)?;
    }
    for output in outputs_absent_before {
        let normalized = repo_path(output, "rollback storage output")?;
        reject_symlink_traversal(&repo.root, &normalized, "rollback managed-storage output")?;
        let candidate = resolved_under(&repo.root, &normalized);
        if candidate.is_symlink() || candidate.is_file() {
            fs::remove_file(&candidate).at(&candidate)?;
        } else if candidate.is_dir() {
            fs::remove_dir_all(&candidate).at(&candidate)?;
        }
    }
    Ok(())
}

fn overlaps_local_boundary(path: &str, boundaries: &BTreeSet<String>) -> bool {
    boundaries.iter().any(|boundary| {
        path == boundary
            || path
                .strip_prefix(boundary)
                .is_some_and(|suffix| suffix.starts_with('/'))
            || boundary
                .strip_prefix(path)
                .is_some_and(|suffix| suffix.starts_with('/'))
    })
}

fn safe_git_materialization_paths(
    repo: &GitRepo,
    old_oid: &str,
    paths: &[String],
) -> Result<Vec<String>> {
    let mut safe = std::collections::BTreeSet::new();
    for path in paths {
        match tree_entry_kind(repo, old_oid, path)? {
            TreeEntryKind::WorktreeEntry => {
                if worktree_file_matches_tree(repo, old_oid, path)? {
                    safe.insert(path.clone());
                }
            }
            TreeEntryKind::Tree => {
                let status = repo.run([
                    "status",
                    "--porcelain=v1",
                    "--untracked-files=all",
                    "--",
                    path,
                ])?;
                if status.stdout.is_empty() {
                    safe.insert(path.clone());
                }
            }
            TreeEntryKind::Missing => {}
        }
    }
    for path in paths {
        if tree_entry_kind(repo, old_oid, path)? == TreeEntryKind::Missing
            && path_is_absent_without_symlink_ancestors(repo, path, &safe)?
        {
            safe.insert(path.clone());
        }
    }
    Ok(paths
        .iter()
        .filter(|path| safe.contains(*path))
        .cloned()
        .collect())
}

fn worktree_file_matches_tree(repo: &GitRepo, oid: &str, path: &str) -> Result<bool> {
    let candidate = resolved_under(&repo.root, path);
    let metadata = match fs::symlink_metadata(&candidate) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(Error::Io {
                path: candidate,
                source: error,
            });
        }
    };
    // Symlinks and gitlinks require type-specific comparison. Preserve them as
    // overlays rather than guessing that an incoming change is safe.
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Ok(false);
    }
    let tree = repo.run(["ls-tree", oid, "--", path])?;
    let Some((entry, actual_path)) = tree.stdout.trim_end().split_once('\t') else {
        return Ok(false);
    };
    if actual_path != path {
        return Ok(false);
    }
    let fields: Vec<&str> = entry.split_whitespace().collect();
    if fields.len() != 3 || fields[1] != "blob" {
        return Ok(false);
    }
    let hashed = repo.run_unchecked([
        "hash-object",
        &format!("--path={path}"),
        "--filters",
        "--",
        path,
    ])?;
    match hashed.code {
        0 => Ok(hashed.stdout.trim() == fields[2]),
        _ => Err(Error::message(format!(
            "failed to hash working-tree path {path:?}: {}",
            hashed.stderr.trim()
        ))),
    }
}

fn path_is_absent_without_symlink_ancestors(
    repo: &GitRepo,
    path: &str,
    replaceable: &std::collections::BTreeSet<String>,
) -> Result<bool> {
    let candidate = resolved_under(&repo.root, path);
    match fs::symlink_metadata(&candidate) {
        Ok(_) => return Ok(false),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) => {}
        Err(error) => {
            return Err(Error::Io {
                path: candidate,
                source: error,
            });
        }
    }
    let relative = Path::new(path);
    let mut current = repo.root.clone();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            current.push(component.as_os_str());
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                    let ancestor = current
                        .strip_prefix(&repo.root)
                        .map(crate::path::to_slash)
                        .map_err(|_| Error::message("working-tree ancestor escaped repository"))?;
                    return Ok(replaceable.contains(&ancestor));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
                Err(error) => {
                    return Err(Error::Io {
                        path: current,
                        source: error,
                    });
                }
            }
        }
    }
    Ok(true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TreeEntryKind {
    Missing,
    Tree,
    WorktreeEntry,
}

fn tree_entry_kind(repo: &GitRepo, oid: &str, path: &str) -> Result<TreeEntryKind> {
    let output = repo.run(["ls-tree", "-z", oid, "--", path])?;
    for entry in output.stdout.split('\0').filter(|entry| !entry.is_empty()) {
        let Some((metadata, actual_path)) = entry.split_once('\t') else {
            return Err(Error::message(format!(
                "Git returned invalid tree metadata for {path:?}"
            )));
        };
        if actual_path != path {
            continue;
        }
        let mode = metadata.split_whitespace().next().unwrap_or_default();
        return Ok(if mode == "040000" {
            TreeEntryKind::Tree
        } else {
            TreeEntryKind::WorktreeEntry
        });
    }
    Ok(TreeEntryKind::Missing)
}

fn materialize_git_paths(repo: &GitRepo, oid: &str, paths: &[String]) -> Result<()> {
    let mut deleted = Vec::new();
    let mut present = Vec::new();
    for path in paths {
        match tree_entry_kind(repo, oid, path)? {
            TreeEntryKind::WorktreeEntry => present.push(path.clone()),
            TreeEntryKind::Missing | TreeEntryKind::Tree => deleted.push(path.clone()),
        }
    }
    deleted.sort_by_key(|path| std::cmp::Reverse(Path::new(path).components().count()));
    for path in &deleted {
        remove_worktree_path(repo, path)?;
        prune_empty_parents(repo, path)?;
    }
    present.sort_by_key(|path| Path::new(path).components().count());
    for path in &present {
        let candidate = resolved_under(&repo.root, path);
        if candidate.is_dir() && !candidate.is_symlink() {
            fs::remove_dir(&candidate).at(&candidate)?;
        }
        repo.run(["checkout-index", "--force", "--", path])?;
    }
    Ok(())
}

fn remove_worktree_path(repo: &GitRepo, path: &str) -> Result<()> {
    let candidate = resolved_under(&repo.root, path);
    match fs::symlink_metadata(&candidate) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir(&candidate).at(&candidate)?;
        }
        Ok(_) => fs::remove_file(&candidate).at(&candidate)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(Error::Io {
                path: candidate,
                source: error,
            });
        }
    }
    Ok(())
}

fn prune_empty_parents(repo: &GitRepo, path: &str) -> Result<()> {
    let mut current = resolved_under(&repo.root, path);
    while let Some(parent) = current.parent() {
        if parent == repo.root {
            break;
        }
        match fs::remove_dir(parent) {
            Ok(()) => current = parent.to_path_buf(),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::NotFound
                ) =>
            {
                break;
            }
            Err(error) => {
                return Err(Error::Io {
                    path: parent.to_path_buf(),
                    source: error,
                });
            }
        }
    }
    Ok(())
}

fn working_changes(repo: &GitRepo) -> Result<Vec<String>> {
    Ok(repo
        .run(["status", "--short", "--untracked-files=normal"])?
        .stdout
        .lines()
        .map(ToOwned::to_owned)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refresh_with_nothing_to_report_carries_no_warning() {
        assert!(unaddressable_warnings(&[]).is_empty());
    }

    #[test]
    fn unaddressable_boundaries_are_reported_once_with_the_move_recovery() {
        let boundaries = ["task/top\\level.bin", "other/d\\x/big.bin"].map(str::to_owned);

        let warnings = unaddressable_warnings(&boundaries);

        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code, "unaddressable-storage-metadata");
        let message = &warnings[0].message;
        for boundary in &boundaries {
            assert!(message.contains(boundary), "{message}");
        }
        assert!(
            message.contains("each path contains a backslash"),
            "{message}"
        );
        // The message is the only place the recovery is stated. Its scope is
        // what `move` and publication check: a scope of the boundary alone
        // refuses the destination, and the rename also rewrites the ignore file
        // beside the boundary. tests/refresh_unaddressable.rs runs the whole
        // sequence against the engine exactly as stated here.
        for step in [
            "infrastructure task",
            "(`other/d\\x`, `task`)",
            "`workspace-mgr move <boundary> <destination>`",
            "`workspace-mgr storage hydrate <path> ...`",
            "publish the task",
        ] {
            assert!(
                message.contains(step),
                "recovery is missing {step:?}: {message}"
            );
        }
        // A scope-wide hydrate refuses while one of these is in scope, so the
        // warning says so rather than leave it to be discovered.
        assert!(message.contains("scope-wide `workspace-mgr storage hydrate`"));
        assert!(!message.contains("repository root"), "{message}");
    }

    #[test]
    fn a_boundary_at_the_repository_root_is_named_as_unrecoverable_by_a_task() {
        let warnings = unaddressable_warnings(&["top\\level.bin".to_owned()]);

        let message = &warnings[0].message;
        assert!(
            message.contains("its path contains a backslash"),
            "{message}"
        );
        assert!(
            message
                .contains("A boundary at the repository root has no directory a task can declare"),
            "{message}"
        );
    }
}
