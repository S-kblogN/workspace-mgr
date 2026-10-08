//! A local archive is reversible until its Git tree is published. Payloads are
//! renamed, never checked out or cleaned; only tool-mutated metadata is saved.
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::archive_migration::RECEIPT_NAME;
use crate::config::Config;
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::lock::RepositoryLock;
use crate::manifest::{ResolvedTask, TaskKind};
use crate::path::{allowed, reject_symlink_traversal, repo_path, resolved_under};
use crate::policy::TASK_MANIFEST_NAME;
use crate::relocation::RelocationPlan;
use crate::s3_purge::{self, ObjectVersion};
use crate::storage_metadata;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Metadata {
    path: String,
    before: Option<Vec<u8>>,
    #[serde(default)]
    unix_mode: Option<u32>,
    #[serde(default)]
    generated: Vec<Vec<u8>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Attempt {
    schema_version: u32,
    root: PathBuf,
    owner: String,
    branch: String,
    source: String,
    destination: String,
    base: String,
    previous_local: Option<String>,
    previous_remote: Option<String>,
    status: String,
    metadata: Vec<Metadata>,
    relocation: RelocationPlan,
    previous_purge: Vec<ObjectVersion>,
    #[serde(default)]
    previous_purge_prefixes: BTreeMap<String, Value>,
    #[serde(default)]
    remote_cleanup_complete: bool,
    #[serde(default)]
    publication_commits: Vec<String>,
    #[serde(default)]
    created_parents: Vec<String>,
    #[serde(default)]
    expected_receipt: Option<Value>,
}

pub fn copy_journal(repo: &GitRepo, source: &str, destination: &str) -> Result<PathBuf> {
    let digest = crate::hex::encode_lower(Sha256::digest(
        format!("{source}\0{destination}").as_bytes(),
    ));
    Ok(repo
        .local_state_dir()?
        .join("archive")
        .join(format!("{digest}.json")))
}

fn attempt_path(repo: &GitRepo, source: &str, destination: &str) -> Result<PathBuf> {
    let digest = crate::hex::encode_lower(Sha256::digest(
        format!("{source}\0{destination}").as_bytes(),
    ));
    Ok(repo
        .local_state_dir()?
        .join("archive-attempts")
        .join(format!("{digest}.json")))
}

/// Must be durable before the first local rename. New attempts carry opaque
/// relocation plans; old reference snapshots remain readable for cancellation.
pub fn record_attempt(
    repo: &GitRepo,
    owner: &ResolvedTask,
    source: &str,
    destination: &str,
    relocation: &RelocationPlan,
    planned_receipt: &Value,
) -> Result<()> {
    let path = attempt_path(repo, source, destination)?;
    if path.exists() {
        let prior: Attempt = read(&path)?;
        if prior.status != "cancelled" {
            return Err(Error::message(
                "an archive attempt already exists; cancel it before starting again",
            ));
        }
    }
    let mut paths = crate::storage_metadata::discover(repo, &[source.to_owned()])?
        .into_iter()
        .map(|p| p[source.len() + 1..].to_owned())
        .collect::<Vec<_>>();
    paths.extend([TASK_MANIFEST_NAME.to_owned(), RECEIPT_NAME.to_owned()]);
    let metadata = paths
        .into_iter()
        .map(|relative| {
            let absolute = resolved_under(&repo.root, source).join(&relative);
            let before = match fs::read(&absolute) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(Error::Io {
                        path: absolute,
                        source: error,
                    });
                }
            };
            #[cfg(unix)]
            let unix_mode = {
                use std::os::unix::fs::PermissionsExt;
                fs::metadata(&absolute).ok().map(|m| m.permissions().mode())
            };
            #[cfg(not(unix))]
            let unix_mode = None;
            Ok(Metadata {
                path: relative,
                before,
                unix_mode,
                generated: Vec::new(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let attempt = Attempt {
        schema_version: 1,
        root: repo.root.clone(),
        owner: owner.task_id.clone(),
        branch: owner.branch.clone(),
        source: source.to_owned(),
        destination: destination.to_owned(),
        base: repo
            .optional_oid("HEAD")?
            .ok_or_else(|| Error::message("archive requires a committed checkout"))?,
        previous_local: repo.optional_oid(&format!("refs/heads/{}", owner.branch))?,
        previous_remote: repo.remote_branch_oid(&owner.remote, &owner.branch)?,
        status: "prepared".to_owned(),
        metadata,
        relocation: relocation.clone(),
        previous_purge: s3_purge::preview(repo)?.pending,
        previous_purge_prefixes: s3_purge::archive_prefixes(repo)?,
        remote_cleanup_complete: false,
        publication_commits: Vec::new(),
        created_parents: missing_parents(repo, destination)?,
        expected_receipt: Some(planned_receipt.clone()),
    };
    save(&path, &attempt)
}

fn missing_parents(repo: &GitRepo, destination: &str) -> Result<Vec<String>> {
    let target = resolved_under(&repo.root, destination);
    let mut parent = target.parent().expect("archive task parent");
    let mut missing = Vec::new();
    while parent != repo.root && !parent.exists() {
        missing.push(
            parent
                .strip_prefix(&repo.root)
                .map_err(|e| Error::message(e.to_string()))?
                .to_string_lossy()
                .into_owned(),
        );
        parent = parent
            .parent()
            .ok_or_else(|| Error::message("archive parent escaped checkout"))?;
    }
    Ok(missing)
}

pub fn moved(repo: &GitRepo, source: &str, destination: &str) -> Result<()> {
    let path = attempt_path(repo, source, destination)?;
    let mut attempt: Attempt = read(&path)?;
    attempt.status = "moved".to_owned();
    save(&path, &attempt)
}

/// Bind an unpublished migration to its local attempt when one was recorded.
/// Older receipts without an undo journal still use current manifest and
/// exact-version transport validation, but cannot promise lossless cancel.
pub(crate) fn validate_migration(repo: &GitRepo, receipt: &Value) -> Result<()> {
    let source = receipt["source"].as_str().ok_or_else(receipt_edit_error)?;
    let destination = receipt["destination"]
        .as_str()
        .ok_or_else(receipt_edit_error)?;
    let path = attempt_path(repo, source, destination)?;
    let attempt: Attempt = match fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw)
            .map_err(|error| Error::message(format!("invalid archive attempt: {error}")))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(Error::Io { path, source }),
    };
    if attempt.schema_version != 1
        || attempt.root != repo.root
        || attempt.source != source
        || attempt.destination != destination
        || !matches!(attempt.status.as_str(), "prepared" | "moved" | "published")
    {
        return Err(Error::message(
            "archive migration does not match an active attempt",
        ));
    }
    validate_current_receipt(repo, &attempt, receipt)?;
    validate_metadata(repo, &resolved_under(&repo.root, destination), &attempt)
}

/// Used after an in-process apply rollback has already restored the directory.
pub fn rolled_back(repo: &GitRepo, source: &str, destination: &str) -> Result<()> {
    let path = attempt_path(repo, source, destination)?;
    let mut attempt: Attempt = read(&path)?;
    attempt.status = "cancelled".to_owned();
    save(&path, &attempt)
}

/// Journal generated commits before their ref update, and verified pushes
/// before source cleanup, so cancel can reject independently changed refs.
pub fn record_publication(
    repo: &GitRepo,
    owner: &ResolvedTask,
    receipts: &[Value],
    commit: &str,
    pushed: bool,
) -> Result<()> {
    for receipt in receipts {
        let (Some(source), Some(destination)) =
            (receipt["source"].as_str(), receipt["destination"].as_str())
        else {
            continue;
        };
        let path = attempt_path(repo, source, destination)?;
        if !path.exists() {
            continue;
        }
        let mut attempt: Attempt = read(&path)?;
        if attempt.owner != owner.task_id || attempt.status == "cancelled" {
            continue;
        }
        if !attempt.publication_commits.iter().any(|c| c == commit) {
            attempt.publication_commits.push(commit.to_owned());
        }
        if pushed {
            attempt.status = "published".to_owned();
        }
        save(&path, &attempt)?;
    }
    Ok(())
}

/// Persist the exact authorized pointer rewrite before changing local bytes.
/// Binding-only edits made independently must never be mistaken for output
/// generated by the archive transport.
pub fn record_pointer_rewrite(
    repo: &GitRepo,
    receipt: &Value,
    pointer: &str,
    rendered_bytes: &[u8],
) -> Result<()> {
    let source = receipt["source"]
        .as_str()
        .ok_or_else(|| Error::message("archive rewrite lacks its source"))?;
    let destination = receipt["destination"]
        .as_str()
        .ok_or_else(|| Error::message("archive rewrite lacks its destination"))?;
    let path = attempt_path(repo, source, destination)?;
    if !path.exists() {
        return Ok(());
    }
    let mut attempt: Attempt = read(&path)?;
    if matches!(attempt.status.as_str(), "published" | "cancelled") {
        return Ok(());
    }
    if attempt.schema_version != 1
        || attempt.root != repo.root
        || attempt.source != source
        || attempt.destination != destination
        || !matches!(attempt.status.as_str(), "prepared" | "moved")
        || attempt
            .expected_receipt
            .as_ref()
            .map(|expected| &expected["task_id"])
            != Some(&receipt["task_id"])
    {
        return Err(Error::message(
            "archive pointer rewrite does not match an active attempt",
        ));
    }
    let pointer = repo_path(pointer, "archive rewritten pointer")?;
    reject_symlink_traversal(&repo.root, &pointer, "archive rewritten pointer")?;
    let relative = pointer
        .strip_prefix(&format!("{destination}/"))
        .filter(|path| storage_metadata::is_pointer(path))
        .ok_or_else(|| Error::message("archive rewritten pointer escaped its destination"))?;
    let metadata = attempt
        .metadata
        .iter_mut()
        .find(|metadata| metadata.path == relative)
        .ok_or_else(|| {
            Error::message("archive rewritten pointer was not captured before moving")
        })?;
    let absolute = resolved_under(&repo.root, &pointer);
    reject_symlink_file(&absolute)?;
    validate_metadata_mode(metadata, &absolute)?;
    let current = fs::read(&absolute).at(&absolute)?;
    validate_pointer_bytes(metadata, &absolute, &current)?;
    if metadata.before.as_deref() != Some(rendered_bytes)
        && !metadata
            .generated
            .iter()
            .any(|generated| generated == rendered_bytes)
    {
        metadata.generated.push(rendered_bytes.to_vec());
        save(&path, &attempt)?;
    }
    Ok(())
}

fn validate_branch(repo: &GitRepo, attempt: &Attempt) -> Result<Option<String>> {
    let current = repo.optional_oid(&format!("refs/heads/{}", attempt.branch))?;
    if current != attempt.previous_local
        && !current
            .as_ref()
            .is_some_and(|c| attempt.publication_commits.contains(c))
    {
        return Err(Error::message(
            "archive publication branch changed independently; preserve its commits before cancel",
        ));
    }
    Ok(current)
}

fn record_cancel_commit(repo: &GitRepo, attempt: &Attempt, commit: &str) -> Result<()> {
    let directory = repo.local_state_dir()?.join("archive-attempts");
    for entry in fs::read_dir(&directory).at(&directory)? {
        let path = entry.at(&directory)?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let mut other: Attempt = read(&path)?;
        if other.root == attempt.root
            && other.owner == attempt.owner
            && other.branch == attempt.branch
            && matches!(other.status.as_str(), "prepared" | "moved" | "canceling")
            && !other
                .publication_commits
                .iter()
                .any(|value| value == commit)
        {
            other.publication_commits.push(commit.to_owned());
            save(&path, &other)?;
        }
    }
    Ok(())
}

pub fn cancel(
    start: &Path,
    manifest: Option<&Path>,
    selected: &[String],
    dry_run: bool,
) -> Result<Value> {
    let repo = match manifest {
        Some(path) => GitRepo::discover_for_manifest(path)?,
        None => GitRepo::discover(start)?,
    };
    let _lock = RepositoryLock::acquire(&repo)?;
    let config = Config::load_compatible(&repo)?;
    let manifest = manifest.ok_or_else(|| {
        Error::message("archive --cancel requires the owning infrastructure --manifest")
    })?;
    let owner = ResolvedTask::load(&repo, &config, manifest)?;
    if owner.kind != TaskKind::Infrastructure {
        return Err(Error::message(
            "archive --cancel requires an infrastructure task",
        ));
    }
    crate::task_rename::validate_checkout(&repo, &owner, "archive cancel")?;
    let selected = selected
        .iter()
        .map(|p| repo_path(p, "archive cancel path"))
        .collect::<Result<Vec<_>>>()?;
    let dir = repo.local_state_dir()?.join("archive-attempts");
    let mut attempts = Vec::new();
    if dir.exists() {
        for entry in fs::read_dir(&dir).at(&dir)? {
            let path = entry.at(&dir)?.path();
            if path.extension().and_then(|v| v.to_str()) != Some("json") {
                continue;
            }
            let attempt: Attempt = read(&path)?;
            if attempt.owner == owner.task_id
                && (selected.is_empty()
                    || selected
                        .iter()
                        .any(|p| p == &attempt.source || p == &attempt.destination))
            {
                if attempt.schema_version != 1
                    || attempt.root != repo.root
                    || attempt.branch != owner.branch
                    || path != attempt_path(&repo, &attempt.source, &attempt.destination)?
                {
                    return Err(Error::message(
                        "archive cancel journal identity or checkout differs",
                    ));
                }
                attempts.push((path, attempt));
            }
        }
    }
    for requested in &selected {
        if !attempts
            .iter()
            .any(|(_, a)| requested == &a.source || requested == &a.destination)
        {
            return Err(Error::message(format!(
                "no lossless archive attempt for {requested}; old receipts alone cannot reconstruct prior local metadata"
            )));
        }
    }
    attempts.sort_by(|a, b| a.1.source.cmp(&b.1.source));
    let mut parents = attempts
        .iter()
        .filter(|(_, attempt)| attempt.status != "cancelled")
        .flat_map(|(_, attempt)| attempt.created_parents.iter().cloned())
        .collect::<Vec<_>>();
    parents.sort_by_key(|value| std::cmp::Reverse(value.split('/').count()));
    parents.dedup();
    // Preflight the entire batch before moving any task or aborting an upload.
    for (_, attempt) in &attempts {
        if attempt.status == "cancelled" {
            continue;
        }
        if attempt.status == "published" {
            return Err(Error::message(
                "archive cancel refuses an attempt whose publication branch was pushed; revert it through review",
            ));
        }
        if !matches!(attempt.status.as_str(), "prepared" | "moved" | "canceling") {
            return Err(Error::message("unsupported archive attempt state"));
        }
        validate_branch(&repo, attempt)?;
        for path in [&attempt.source, &attempt.destination] {
            repo_path(path, "archive cancel path")?;
            reject_symlink_traversal(&repo.root, path, "archive cancel path")?;
            if !allowed(path, &owner.scopes()) {
                return Err(Error::message(
                    "archive cancel requires both source and destination scopes",
                ));
            }
        }
        let source = resolved_under(&repo.root, &attempt.source);
        let destination = resolved_under(&repo.root, &attempt.destination);
        if source.exists() == destination.exists() {
            return Err(Error::message(
                "archive cancel requires exactly one original or moved directory; refusing a collision",
            ));
        }
        attempt.relocation.validate_applied()?;
        validate_metadata(
            &repo,
            if destination.exists() {
                &destination
            } else {
                &source
            },
            attempt,
        )?;
        if repo.remote_branch_oid(&owner.remote, &owner.branch)? != attempt.previous_remote {
            return Err(Error::message(
                "archive cancel refuses an attempt whose publication branch was pushed; revert it through review",
            ));
        }
        // Also catch a merged archive whose publication branch has been deleted.
        let base = repo.fetch_branch(&owner.remote, &owner.base_branch)?;
        let journal = copy_journal(&repo, &attempt.source, &attempt.destination)?;
        let remote_cancelled = attempt.remote_cleanup_complete
            || (journal.exists() && read::<Value>(&journal)?["status"] == "cancelled");
        // Once our remote cancellation completed, this path can belong to a
        // later reviewed attempt. Local metadata/ref checks above still apply.
        if !remote_cancelled
            && repo
                .run_unchecked([
                    "cat-file",
                    "-e",
                    &format!("{base}:{}/{}", attempt.destination, RECEIPT_NAME),
                ])?
                .success()
        {
            return Err(Error::message(
                "archive cancel refuses an archive already present on the shared branch",
            ));
        }
    }
    let mut reports = Vec::new();
    let mut completed_journals = Vec::new();
    for (path, _) in attempts {
        // Earlier tasks in this batch may have journaled a cancellation commit
        // for the same owner ref. Never overwrite that durable allowlist with
        // the snapshot loaded before the batch began.
        let mut attempt: Attempt = read(&path)?;
        if attempt.status == "cancelled" {
            reports.push(json!({"source":attempt.source,"destination":attempt.destination,"status":"already_cancelled"}));
            continue;
        }
        let journal = copy_journal(&repo, &attempt.source, &attempt.destination)?;
        let mut canonical_to_release = None;
        let mut terminal_remote = attempt.remote_cleanup_complete;
        let remote = if config.s3_enabled() && journal.exists() {
            crate::storage_metadata::ensure_ready(&repo, &config)?;
            let transport_request = json!({
                "source":attempt.source,"destination":attempt.destination,"state_path":journal.to_string_lossy()
            });
            // Prove ownership/source preservation before withdrawing a mapping.
            let preview = crate::storage_metadata::version_archive_adapter(
                &repo,
                "cancel-preview",
                &transport_request,
            )?;
            let mut receipt: Value = read(&journal)?;
            normalize_copy_schema(&mut receipt)?;
            terminal_remote |= receipt["status"] == "cancelled";
            if attempt.remote_cleanup_complete && receipt["status"] != "cancelled" {
                return Err(Error::message(
                    "completed archive cancellation has an inconsistent copy journal",
                ));
            }
            let complete = matches!(
                receipt["status"].as_str(),
                Some("copied" | "canceling" | "cancelled")
            ) && receipt["versions"].as_array().is_some_and(|rows| {
                rows.iter()
                    .all(|row| row["destination_version_id"].is_string())
            });
            if complete {
                receipt["status"] = "copied".into();
                receipt["source_cleanup"] = "after_verified_git_publication".into();
                for row in receipt["versions"].as_array_mut().into_iter().flatten() {
                    if let Some(object) = row.as_object_mut() {
                        for key in [
                            "started",
                            "multipart_upload_id",
                            "cancel_started",
                            "cancel_deleted",
                            "cancel_owned_versions",
                        ] {
                            object.remove(key);
                        }
                    }
                }
                if let Some(expected) = &attempt.expected_receipt {
                    for key in crate::archive_migration::RECEIPT_METADATA_FIELDS {
                        if let Some(value) = expected.get(key) {
                            receipt[key] = value.clone();
                        }
                    }
                }
            }
            let coordinated = !terminal_remote
                && complete
                && (crate::archive_registry::has_binding(&repo, &receipt)? || !dry_run);
            let proof = if coordinated {
                crate::archive_registry::coordinate(&repo, &receipt, !dry_run)?
            } else {
                Value::Null
            };
            let registry = if terminal_remote {
                json!({"status":"already_cancelled"})
            } else {
                crate::storage_metadata::archive_registry_adapter(
                    &repo,
                    if dry_run { "cancel-preview" } else { "cancel" },
                    &json!({"receipt":receipt,"coordination":proof}),
                )?
            };
            let mut result = if dry_run {
                preview
            } else {
                let result = crate::storage_metadata::version_archive_adapter(
                    &repo,
                    "cancel",
                    &transport_request,
                )?;
                if !matches!(
                    result["status"].as_str(),
                    Some("cancelled" | "already_cancelled" | "no_remote_copy")
                ) {
                    return Err(Error::message(
                        "archive cancellation is incomplete: unrelated destination versions or uploads remain; their bytes were preserved",
                    ));
                }
                if coordinated || (terminal_remote && complete) {
                    canonical_to_release = Some(receipt.clone());
                }
                result
            };
            result["registry"] = registry;
            result
        } else {
            json!({"status":"no_remote_copy","retained_versions":[]})
        };
        if !dry_run {
            // Remote cleanup is durable before touching local bytes. Keep both
            // Git claims until local undo completes, including parent cleanup.
            attempt.remote_cleanup_complete = true;
            attempt.status = "canceling".to_owned();
            save(&path, &attempt)?;
            let source = resolved_under(&repo.root, &attempt.source);
            let destination = resolved_under(&repo.root, &attempt.destination);
            let was_moved = destination.exists();
            let current_directory = if was_moved { &destination } else { &source };
            attempt.relocation.restore()?;
            for metadata in &attempt.metadata {
                repo_path(&metadata.path, "archive original metadata")?;
                reject_symlink_traversal(current_directory, &metadata.path, "archive metadata")?;
                restore_metadata(
                    &current_directory.join(&metadata.path),
                    metadata.before.as_deref(),
                )?;
                #[cfg(unix)]
                if let Some(mode) = metadata.unix_mode {
                    use std::os::unix::fs::PermissionsExt;
                    let metadata_path = current_directory.join(&metadata.path);
                    fs::set_permissions(&metadata_path, fs::Permissions::from_mode(mode))
                        .at(&metadata_path)?;
                }
            }
            if was_moved {
                fs::rename(&destination, &source).at(&destination)?;
            }
            restore_branch(&repo, &mut attempt)?;
            s3_purge::cancel_archive(
                &repo,
                &attempt.source,
                &attempt.destination,
                &attempt.previous_purge,
                &attempt.previous_purge_prefixes,
            )?;
            // Keep the durable attempt resumable until the batch's recorded
            // parent cleanup has also completed.
            completed_journals.push((path, canonical_to_release, terminal_remote));
        }
        reports.push(
            json!({"source":attempt.source,"destination":attempt.destination,
            "status":if dry_run {"would_cancel"} else {"cancelled"},"remote":remote}),
        );
    }
    if !dry_run {
        for parent in parents {
            let parent = repo_path(&parent, "archive created parent")?;
            reject_symlink_traversal(&repo.root, &parent, "archive created parent")?;
            let path = resolved_under(&repo.root, &parent);
            match fs::remove_dir(&path) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                    ) => {}
                Err(source) => return Err(Error::Io { path, source }),
            }
        }
        for (path, receipt, terminal_remote) in completed_journals {
            // Other tasks in this batch may have extended the undo allowlist.
            let mut attempt: Attempt = read(&path)?;
            if config.s3_enabled() {
                if let Some(receipt) = receipt {
                    if terminal_remote {
                        crate::archive_registry::release_if_owned(&repo, &receipt)?;
                    } else {
                        crate::archive_registry::release(&repo, &receipt)?;
                    }
                }
                if let Some(planned) = &attempt.expected_receipt {
                    crate::archive_reservation::release(&repo, planned)?;
                }
            }
            attempt.status = "cancelled".to_owned();
            save(&path, &attempt)?;
        }
    }
    Ok(
        json!({"operation":"archive_cancel","status":if dry_run {"dry_run"} else if reports.iter().all(|r| r["status"] == "already_cancelled") {"no_changes"} else {"cancelled"},
        "tasks":reports,"remote_writes":!dry_run && config.s3_enabled()}),
    )
}

fn validate_metadata(repo: &GitRepo, destination: &Path, attempt: &Attempt) -> Result<()> {
    for item in &attempt.metadata {
        repo_path(&item.path, "archive original metadata")?;
        reject_symlink_traversal(destination, &item.path, "archive metadata")?;
        let path = destination.join(&item.path);
        reject_symlink_file(&path)?;
        if item.path == RECEIPT_NAME && !path.exists() && item.before.is_none() {
            continue;
        }
        let current = fs::read(&path).at(&path)?;
        validate_metadata_mode(item, &path)?;
        if item.before.as_deref() == Some(current.as_slice()) {
            continue;
        }
        if item.path == RECEIPT_NAME {
            let receipt: Value =
                serde_json::from_slice(&current).map_err(|e| Error::message(e.to_string()))?;
            validate_current_receipt(repo, attempt, &receipt)?;
            let mut generated = serde_json::to_vec_pretty(&receipt)
                .map_err(|error| Error::message(error.to_string()))?;
            generated.push(b'\n');
            if current != generated {
                return Err(receipt_edit_error());
            }
        } else if item.path == TASK_MANIFEST_NAME {
            let mut expected: crate::manifest::TaskManifest = toml::from_str(
                std::str::from_utf8(
                    item.before
                        .as_deref()
                        .ok_or_else(|| Error::message("archive has no original manifest"))?,
                )
                .map_err(|e| Error::message(e.to_string()))?,
            )
            .map_err(|e| Error::message(e.to_string()))?;
            expected.path = Some(attempt.destination.clone());
            let generated = toml::to_string_pretty(&expected)
                .map_err(|error| Error::message(error.to_string()))?;
            if current != generated.as_bytes() {
                return Err(Error::message(
                    "archive manifest was edited after moving; preserve that edit before cancel",
                ));
            }
        } else {
            validate_pointer_bytes(item, &path, &current)?;
        }
    }
    Ok(())
}

fn validate_pointer_bytes(metadata: &Metadata, path: &Path, current: &[u8]) -> Result<()> {
    if metadata.before.as_deref() == Some(current)
        || metadata
            .generated
            .iter()
            .any(|generated| generated == current)
    {
        return Ok(());
    }
    Err(Error::message(format!(
        "archive metadata {} was edited after moving; preserve it before cancel",
        path.display()
    )))
}

fn validate_metadata_mode(metadata: &Metadata, path: &Path) -> Result<()> {
    #[cfg(unix)]
    if metadata.before.is_some()
        && let Some(mode) = metadata.unix_mode
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(path).at(path)?.permissions().mode() != mode {
            return Err(Error::message(format!(
                "archive metadata permissions changed independently: {}",
                path.display()
            )));
        }
    }
    #[cfg(not(unix))]
    let _ = (metadata, path);
    Ok(())
}

fn validate_current_receipt(repo: &GitRepo, attempt: &Attempt, receipt: &Value) -> Result<()> {
    let expected = attempt.expected_receipt.as_ref().ok_or_else(|| {
        Error::message("archive attempt lacks its original planned receipt; cannot verify receipt edits before cancel")
    })?;
    if receipt == expected {
        return Ok(());
    }
    let mut copied_without_storage = expected.clone();
    copied_without_storage["status"] = "copied".into();
    if expected.get("bucket").is_none() && receipt == &copied_without_storage {
        return Ok(());
    }
    if receipt["status"] != "copied" {
        return Err(receipt_edit_error());
    }
    let journal_path = copy_journal(repo, &attempt.source, &attempt.destination)?;
    if !journal_path.exists() {
        return Err(receipt_edit_error());
    }
    let mut generated: Value = read(&journal_path)?;
    normalize_copy_schema(&mut generated)?;
    if !matches!(
        generated["status"].as_str(),
        Some("copied" | "canceling" | "cancelled")
    ) {
        return Err(receipt_edit_error());
    }
    generated["status"] = "copied".into();
    let versions = generated["versions"]
        .as_array_mut()
        .ok_or_else(receipt_edit_error)?;
    for version in versions {
        let object = version.as_object_mut().ok_or_else(receipt_edit_error)?;
        object.remove("started");
        object.remove("multipart_upload_id");
        object.remove("cancel_started");
        object.remove("cancel_deleted");
        object.remove("cancel_owned_versions");
    }
    generated["source_cleanup"] = "after_verified_git_publication".into();
    for key in crate::archive_migration::RECEIPT_METADATA_FIELDS {
        if let Some(value) = expected.get(key) {
            generated[key] = value.clone();
        }
    }
    // Every copied binding must be the exact one durably recorded by the
    // transport; immutable source history and review evidence remain frozen.
    if receipt != &generated || frozen_receipt(&generated)? != frozen_receipt(expected)? {
        return Err(receipt_edit_error());
    }
    Ok(())
}

fn frozen_receipt(receipt: &Value) -> Result<Value> {
    let mut value = receipt.clone();
    let object = value.as_object_mut().ok_or_else(receipt_edit_error)?;
    object.remove("transaction_id");
    object.insert("status".to_owned(), "planned".into());
    let versions = object
        .get_mut("versions")
        .and_then(Value::as_array_mut)
        .ok_or_else(receipt_edit_error)?;
    for version in versions {
        let object = version.as_object_mut().ok_or_else(receipt_edit_error)?;
        for key in [
            "destination_version_id",
            "destination_etag",
            "destination_last_modified",
        ] {
            object.remove(key);
        }
    }
    Ok(value)
}

fn normalize_copy_schema(receipt: &mut Value) -> Result<()> {
    if !matches!(receipt["schema_version"].as_u64(), Some(1 | 2)) {
        return Err(Error::message(
            "private archive copy journal has an unsupported schema",
        ));
    }
    receipt["schema_version"] = 1.into();
    Ok(())
}

fn receipt_edit_error() -> Error {
    Error::message("archive receipt was edited after moving; preserve that edit before cancel")
}

fn restore_branch(repo: &GitRepo, attempt: &mut Attempt) -> Result<()> {
    let reference = format!("refs/heads/{}", attempt.branch);
    // Another selected task may have generated a cancellation commit on this
    // shared owner ref. Reload the durable allowlist, then use the checked OID
    // as update-ref's compare-and-swap value; external Git writers cannot be
    // mistaken for a workspace-mgr publication after preflight.
    let latest: Attempt = read(&attempt_path(repo, &attempt.source, &attempt.destination)?)?;
    attempt.publication_commits = latest.publication_commits;
    let current = validate_branch(repo, attempt)?;
    if current == attempt.previous_local {
        return Ok(());
    }
    let Some(current) = current else {
        return Err(Error::message(
            "archive publication branch disappeared locally",
        ));
    };
    let baseline = attempt.previous_local.as_deref().unwrap_or(&attempt.base);
    let changes = repo
        .run(["diff", "--name-only", baseline, &current])?
        .stdout;
    if changes
        .lines()
        .all(|p| allowed(p, &[attempt.source.clone(), attempt.destination.clone()]))
    {
        if let Some(previous) = &attempt.previous_local {
            repo.run(["update-ref", &reference, previous, &current])?;
        } else {
            repo.run(["update-ref", "-d", &reference, &current])?;
        }
    } else {
        if repo
            .run([
                "diff",
                "--name-only",
                baseline,
                &current,
                "--",
                &attempt.source,
                &attempt.destination,
            ])?
            .stdout
            .is_empty()
        {
            return Ok(());
        }
        // Keep other paths from an infrastructure publication; reverse only
        // the archive's tree in a new local commit using a disposable index.
        let temp = tempfile::tempdir().at(&repo.root)?;
        let index = temp.path().join("index");
        repo.run_with_index(&index, ["read-tree", &current], None, true)?;
        repo.run_with_index(
            &index,
            [
                "restore",
                "--source",
                baseline,
                "--staged",
                "--",
                &attempt.source,
                &attempt.destination,
            ],
            None,
            true,
        )?;
        let tree = repo
            .run_with_index(&index, ["write-tree"], None, true)?
            .stdout
            .trim()
            .to_owned();
        let commit = repo
            .run_with_index(
                &index,
                ["commit-tree", &tree, "-p", &current],
                Some("Cancel unpublished workspace-mgr archive\n"),
                true,
            )?
            .stdout
            .trim()
            .to_owned();
        attempt.publication_commits.push(commit.clone());
        save(
            &attempt_path(repo, &attempt.source, &attempt.destination)?,
            attempt,
        )?;
        record_cancel_commit(repo, attempt, &commit)?;
        repo.run(["update-ref", &reference, &commit, &current])?;
    }
    Ok(())
}

fn reject_symlink_file(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink() || !m.is_file()) {
        return Err(Error::message(format!(
            "archive metadata must be a regular file: {}",
            path.display()
        )));
    }
    Ok(())
}

fn restore_metadata(path: &Path, bytes: Option<&[u8]>) -> Result<()> {
    reject_symlink_file(path)?;
    match bytes {
        Some(bytes) => atomic_write(path, bytes),
        None => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(Error::Io {
                path: path.to_owned(),
                source,
            }),
        },
    }
}

fn read<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    reject_symlink_file(path)?;
    serde_json::from_slice(&fs::read(path).at(path)?)
        .map_err(|e| Error::message(format!("invalid archive attempt journal: {e}")))
}

fn save(path: &Path, attempt: &Attempt) -> Result<()> {
    fs::create_dir_all(path.parent().expect("journal parent")).at(path)?;
    atomic_write(
        path,
        &serde_json::to_vec_pretty(attempt).map_err(|e| Error::message(e.to_string()))?,
    )
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::message("archive metadata has no parent"))?;
    let permissions = fs::metadata(path).ok().map(|m| m.permissions());
    let mut file = tempfile::NamedTempFile::new_in(parent).at(parent)?;
    file.write_all(bytes).at(path)?;
    if let Some(permissions) = permissions {
        file.as_file().set_permissions(permissions).at(path)?;
    }
    file.as_file().sync_all().at(path)?;
    file.persist(path).map_err(|e| Error::Io {
        path: path.to_owned(),
        source: e.error,
    })?;
    fs::File::open(parent).at(parent)?.sync_all().at(parent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copied_receipt_fixture() -> (tempfile::TempDir, GitRepo, Attempt, Value, Value) {
        let temporary = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: temporary.path().canonicalize().unwrap(),
        };
        repo.run(["init", "-b", "main"]).unwrap();
        let source = "20260712-120000-completed";
        let destination = "2026/07/20260712-120000-completed";
        fs::create_dir(repo.root.join(source)).unwrap();
        let expected = json!({
            "schema_version":1,"remote":"storage","bucket":"isolated-fixture",
            "remote_prefix":"prefix","source":source,"destination":destination,
            "status":"planned","source_cleanup":"after_verified_git_publication",
            "task_id":source,"previous_receipt":{"retained_note":"original historical evidence"},
            "completion_reviews":[{"number":1,"head_commit":"reviewed-head"}],
            "historical_records":[{"path":format!("{source}/logs/history.log"),"sha256":"original-digest","unix_mode":33060,"role":"historical-record"}],
            "versions":[
                {"source_object":format!("{source}/payload.bin"),"destination_object":format!("{destination}/payload.bin"),
                 "source_version_id":"source-payload","source_last_modified":"2026-07-12T20:00:00+00:00",
                 "source_is_latest":false,"source_list_order":0,"delete_marker":false,"size":42,
                 "source_etag":"source-etag","destination_etag":null},
                {"source_object":format!("{source}/payload.bin"),"destination_object":format!("{destination}/payload.bin"),
                 "source_version_id":"source-marker","source_last_modified":"2026-07-12T20:01:00+00:00",
                 "source_is_latest":true,"source_list_order":1,"delete_marker":true,"size":null,
                 "source_etag":null,"destination_etag":null}
            ]
        });
        let mut copied = expected.clone();
        copied["status"] = "copied".into();
        copied["transaction_id"] = "isolated-copy-transaction".into();
        copied["versions"][0]["destination_version_id"] = "destination-payload".into();
        copied["versions"][0]["destination_etag"] = "copied-etag".into();
        copied["versions"][0]["destination_last_modified"] = "2026-10-06T20:00:00+00:00".into();
        copied["versions"][1]["destination_version_id"] = "destination-marker".into();
        copied["versions"][1]["destination_last_modified"] = "2026-10-06T20:00:01+00:00".into();
        let mut journal = copied.clone();
        for key in [
            "task_id",
            "previous_receipt",
            "completion_reviews",
            "historical_records",
            "source_cleanup",
        ] {
            journal.as_object_mut().unwrap().remove(key);
        }
        // Internal upload state is omitted from the public receipt.
        journal["versions"][0]["started"] = false.into();
        journal["versions"][0]["multipart_upload_id"] = "finished-upload".into();
        let path = copy_journal(&repo, source, destination).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, serde_json::to_vec(&journal).unwrap()).unwrap();
        let attempt = Attempt {
            schema_version: 1,
            root: repo.root.clone(),
            owner: "infra-archive".to_owned(),
            branch: "codex/infra-archive".to_owned(),
            source: source.to_owned(),
            destination: destination.to_owned(),
            base: "unused-base".to_owned(),
            previous_local: None,
            previous_remote: None,
            status: "moved".to_owned(),
            metadata: Vec::new(),
            relocation: crate::relocation::RelocationPlan::opaque(
                &repo.root.join(source),
                &repo.root.join(destination),
            )
            .unwrap(),
            previous_purge: Vec::new(),
            previous_purge_prefixes: BTreeMap::new(),
            remote_cleanup_complete: false,
            publication_commits: Vec::new(),
            created_parents: Vec::new(),
            expected_receipt: Some(expected),
        };
        (temporary, repo, attempt, copied, journal)
    }

    #[test]
    fn copied_s3_receipt_accepts_exact_journal_payload_and_delete_marker_bindings() {
        let (_temporary, repo, attempt, copied, _journal) = copied_receipt_fixture();
        validate_current_receipt(&repo, &attempt, attempt.expected_receipt.as_ref().unwrap())
            .unwrap();
        validate_current_receipt(&repo, &attempt, &copied).unwrap();
    }

    #[test]
    fn cancellation_audit_does_not_change_the_frozen_public_receipt() {
        let (_temporary, repo, attempt, copied, mut journal) = copied_receipt_fixture();
        journal["status"] = "canceling".into();
        journal["versions"][0]["cancel_started"] = true.into();
        journal["versions"][0]["cancel_owned_versions"] = json!([
            {"version_id":"sdk-retry-copy","delete_marker":false,
             "etag":"same-payload","started":true,"deleted":true}
        ]);
        fs::write(
            copy_journal(&repo, &attempt.source, &attempt.destination).unwrap(),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        validate_current_receipt(&repo, &attempt, &copied).unwrap();
        assert!(crate::archive_migration::trusted_copy_journal(&repo, &copied).unwrap());
        let mut legacy = serde_json::to_value(&attempt).unwrap();
        let object = legacy.as_object_mut().unwrap();
        object.remove("remote_cleanup_complete");
        object.remove("previous_purge_prefixes");
        let restored: Attempt = serde_json::from_value(legacy).unwrap();
        assert!(!restored.remote_cleanup_complete);
        assert!(restored.previous_purge_prefixes.is_empty());
    }

    #[test]
    fn private_schema_2_preserves_receipt_review_and_cancel_provenance() {
        let (_temporary, repo, attempt, copied, mut journal) = copied_receipt_fixture();
        journal["schema_version"] = crate::policy::ARCHIVE_COPY_JOURNAL_SCHEMA_VERSION.into();
        journal["status"] = "canceling".into();
        journal["versions"][0]["cancel_owned_versions"] = json!([
            {"version_id":"sdk-retry-copy","delete_marker":false,"etag":"copied-etag"}
        ]);
        fs::write(
            copy_journal(&repo, &attempt.source, &attempt.destination).unwrap(),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        validate_current_receipt(&repo, &attempt, &copied).unwrap();
        assert!(crate::archive_migration::trusted_copy_journal(&repo, &copied).unwrap());
        let mut reconstructed = journal.clone();
        normalize_copy_schema(&mut reconstructed).unwrap();
        assert_eq!(reconstructed["schema_version"], 1);
        assert_eq!(journal["schema_version"], 2);
        let mut unknown = journal;
        unknown["schema_version"] = 3.into();
        fs::write(
            copy_journal(&repo, &attempt.source, &attempt.destination).unwrap(),
            serde_json::to_vec(&unknown).unwrap(),
        )
        .unwrap();
        assert!(validate_current_receipt(&repo, &attempt, &copied).is_err());
        assert!(crate::archive_migration::trusted_copy_journal(&repo, &copied).is_err());
    }

    #[test]
    fn copied_s3_receipt_refuses_inventory_review_and_mapping_edits() {
        let (_temporary, repo, attempt, copied, mut journal) = copied_receipt_fixture();
        for edited in [
            {
                let mut value = copied.clone();
                value["note"] = "independent edit".into();
                value
            },
            {
                let mut value = copied.clone();
                value["completion_reviews"][0]["number"] = 999.into();
                value
            },
            {
                let mut value = copied.clone();
                value["versions"][0]["destination_version_id"] = "unrecorded-version".into();
                value
            },
            {
                let mut value = copied.clone();
                value["previous_receipt"]["retained_note"] = "independent edit".into();
                value
            },
            {
                let mut value = copied.clone();
                value["historical_records"][0]["sha256"] = "independent edit".into();
                value
            },
        ] {
            assert!(validate_current_receipt(&repo, &attempt, &edited).is_err());
        }
        // Even coordinated edits of the receipt and journal cannot bless a
        // different source inventory than the snapshot captured before move.
        let mut edited = copied;
        edited["versions"][0]["source_version_id"] = "different-source-version".into();
        journal["versions"][0]["source_version_id"] = "different-source-version".into();
        fs::write(
            copy_journal(&repo, &attempt.source, &attempt.destination).unwrap(),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        assert!(validate_current_receipt(&repo, &attempt, &edited).is_err());
    }

    #[test]
    fn pointer_rewrites_are_journaled_exactly_and_independent_bindings_are_preserved() {
        let (_temporary, repo, mut attempt, copied, _journal) = copied_receipt_fixture();
        let relative = "metadata/data.bin.dvc";
        let source = repo.root.join(&attempt.source);
        let destination = repo.root.join(&attempt.destination);
        fs::create_dir(source.join("metadata")).unwrap();
        let before = b"outs:\n- path: data.bin\n  cloud:\n    workspace-mgr:\n      version_id: original\n      etag: original-etag\n";
        let original_pointer = source.join(relative);
        fs::write(&original_pointer, before).unwrap();
        #[cfg(unix)]
        let unix_mode = {
            use std::os::unix::fs::PermissionsExt;
            Some(
                fs::metadata(&original_pointer)
                    .unwrap()
                    .permissions()
                    .mode(),
            )
        };
        #[cfg(not(unix))]
        let unix_mode = None;
        attempt.metadata.push(Metadata {
            path: relative.to_owned(),
            before: Some(before.to_vec()),
            unix_mode,
            generated: Vec::new(),
        });
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::rename(&source, &destination).unwrap();
        save(
            &attempt_path(&repo, &attempt.source, &attempt.destination).unwrap(),
            &attempt,
        )
        .unwrap();
        let pointer = format!("{}/{relative}", attempt.destination);
        let absolute = destination.join(relative);
        let generated = String::from_utf8(before.to_vec())
            .unwrap()
            .replace("version_id: original", "version_id: copied")
            .replace("etag: original-etag", "etag: copied-etag");
        record_pointer_rewrite(&repo, &copied, &pointer, generated.as_bytes()).unwrap();
        record_pointer_rewrite(&repo, &copied, &pointer, generated.as_bytes()).unwrap();
        let recorded: Attempt =
            read(&attempt_path(&repo, &attempt.source, &attempt.destination).unwrap()).unwrap();
        assert_eq!(recorded.metadata[0].generated, [generated.as_bytes()]);
        fs::write(&absolute, &generated).unwrap();
        validate_metadata(&repo, &destination, &recorded).unwrap();
        let edited = generated.replace("etag: copied-etag", "etag: independent-etag");
        fs::write(&absolute, &edited).unwrap();
        assert!(validate_metadata(&repo, &destination, &recorded).is_err());
        assert!(record_pointer_rewrite(&repo, &copied, &pointer, generated.as_bytes()).is_err());
        assert_eq!(fs::read_to_string(&absolute).unwrap(), edited);
        let uncaptured = format!("{}/uncaptured.dvc", attempt.destination);
        fs::write(repo.root.join(&uncaptured), before).unwrap();
        assert!(record_pointer_rewrite(&repo, &copied, &uncaptured, generated.as_bytes()).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::write(&absolute, before).unwrap();
            let changed_mode = (unix_mode.unwrap() & 0o777) ^ 0o040;
            fs::set_permissions(&absolute, fs::Permissions::from_mode(changed_mode)).unwrap();
            assert!(
                validate_metadata(&repo, &destination, &recorded)
                    .unwrap_err()
                    .to_string()
                    .contains("permissions changed independently")
            );
            assert_eq!(
                fs::metadata(&absolute).unwrap().permissions().mode() & 0o777,
                changed_mode
            );
        }
    }
}
