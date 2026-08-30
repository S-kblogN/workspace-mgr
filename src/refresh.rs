use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::Config;
use crate::dvc::{self, PreparedRevision};
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::lock::RepositoryLock;
use crate::path::{repo_path, resolved_under};
use crate::process::run;

#[derive(Debug, Clone)]
pub struct RefreshOptions {
    pub repo: PathBuf,
    pub remote: Option<String>,
    pub branch: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefreshStorageReport {
    pub mode: String,
    pub changed_files: Vec<String>,
    pub old_files: Vec<String>,
    pub new_files: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_prepared: Option<PreparedRevision>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_prepared: Option<PreparedRevision>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub materialized: Vec<String>,
}

pub fn execute(options: &RefreshOptions) -> Result<RefreshReport> {
    let repo = GitRepo::discover(&options.repo)?;
    let _repository_lock = RepositoryLock::acquire(&repo)?;
    let config = Config::load_compatible(&repo)?;
    let remote = options
        .remote
        .clone()
        .unwrap_or_else(|| config.publication.remote.clone());
    let branch = options
        .branch
        .clone()
        .unwrap_or_else(|| config.publication.shared_checkout_branch.clone());
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
    let materialized_git_paths = safe_git_materialization_paths(&repo, &old_oid, &incoming_git)?;
    let old_dvc = existing_at(&repo, &old_oid, &incoming_dvc)?;
    let new_dvc = existing_at(&repo, &new_oid, &incoming_dvc)?;
    let working_before = working_changes(&repo)?;
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
            old_prepared: None,
            new_prepared: None,
            materialized: Vec::new(),
        },
        method: None,
    };
    if old_oid == new_oid {
        return Ok(report);
    }

    let overlays = if incoming_dvc.is_empty() {
        BTreeMap::new()
    } else {
        let overlays = capture_overlays(&repo, &old_oid, &new_oid, &incoming_dvc)?;
        dvc::validate_worktree(&repo, &config, &incoming_dvc)?;
        overlays
    };
    if options.dry_run {
        report.status = "dry_run".to_owned();
        return Ok(report);
    }

    let mut outputs_absent_before = Vec::new();
    if !incoming_dvc.is_empty() {
        let old_prepared = dvc::prepare_revision(&repo, &config, &old_oid, &old_dvc)?;
        let new_prepared = dvc::prepare_revision(&repo, &config, &new_oid, &new_dvc)?;
        let unsafe_outputs: Vec<String> = new_dvc
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
        outputs_absent_before = new_dvc
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
            if !new_dvc.is_empty() {
                let args = ["checkout".to_owned(), "--".to_owned()]
                    .into_iter()
                    .chain(new_dvc.iter().cloned())
                    .collect::<Vec<_>>();
                run(&dvc::dvc_program(), args, &repo.root)?;
                dvc::verify(&repo, &config, &new_dvc)?;
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
    Ok(report)
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
    let output = repo.run_unchecked(["show", &format!("{oid}:{path}")])?;
    match output.code {
        0 => Ok(Some(output.stdout)),
        128 => Ok(None),
        _ => Err(Error::message(format!(
            "failed to inspect {path} at {oid}: {}",
            output.stderr.trim()
        ))),
    }
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
) -> Result<BTreeMap<String, Option<String>>> {
    let mut conflicts = Vec::new();
    let mut overlays = BTreeMap::new();
    for path in paths {
        let candidate = resolved_under(&repo.root, path);
        let old_content = file_at(repo, old, path)?;
        let new_content = file_at(repo, new, path)?;
        if !candidate.exists() {
            overlays.insert(path.clone(), None);
            if old_content.is_some() {
                conflicts.push(path.clone());
            }
            continue;
        }
        if !candidate.is_file() || candidate.is_symlink() {
            conflicts.push(path.clone());
            continue;
        }
        let current = fs::read_to_string(&candidate).at(&candidate)?;
        overlays.insert(path.clone(), Some(current.clone()));
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
    overlays: &BTreeMap<String, Option<String>>,
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
    for (path, content) in overlays {
        let candidate = resolved_under(&repo.root, path);
        match content {
            Some(content) => {
                if let Some(parent) = candidate.parent() {
                    fs::create_dir_all(parent).at(parent)?;
                }
                fs::write(&candidate, content).at(&candidate)?;
                restored.push(path.clone());
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
        run(&dvc::dvc_program(), args, &repo.root)?;
    }
    for output in outputs_absent_before {
        let normalized = repo_path(output, "rollback storage output")?;
        let candidate = resolved_under(&repo.root, &normalized);
        if candidate.is_symlink() || candidate.is_file() {
            fs::remove_file(&candidate).at(&candidate)?;
        } else if candidate.is_dir() {
            fs::remove_dir_all(&candidate).at(&candidate)?;
        }
    }
    Ok(())
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
