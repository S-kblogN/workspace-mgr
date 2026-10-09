use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::config::{Config, require_supported_cli_at};
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::lock::RepositoryLock;
use crate::manifest::{
    ResolvedTask, TaskKind, build_task_path, parse_task_identity, validate_additional_scopes,
    validate_task_slug,
};
use crate::path::{reject_symlink_traversal, resolved_under};
use crate::policy::TASK_MANIFEST_NAME;
use crate::relocation::{RelocationNotice, RelocationPlan};
use crate::storage_metadata;
use crate::transaction::validate_remote_task_identity;

#[derive(Debug, Clone)]
pub struct TaskRenameOptions {
    pub start: PathBuf,
    pub manifest: Option<PathBuf>,
    pub new_slug: String,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskRenameReport {
    pub status: String,
    pub operation: String,
    pub kind: TaskKind,
    pub task_id: String,
    pub old_slug: String,
    pub new_slug: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_path: Option<String>,
    pub old_manifest: String,
    pub new_manifest: String,
    pub branch: String,
    pub local_branch_oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_branch_oid: Option<String>,
    pub local_actions: Vec<TaskRenameAction>,
    pub remote_writes: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_migration: Option<TaskRenameStorageMigration>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<RelocationNotice>,
    pub review: TaskRenameReview,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskRenameAction {
    pub action: &'static str,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskRenameReview {
    pub head_branch_unchanged: bool,
    pub pull_request: &'static str,
    pub agent_action: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskRenameStorageMigration {
    pub status: &'static str,
    pub source: String,
    pub destination: String,
    pub preserved_versions: usize,
    pub payload_versions: usize,
    pub delete_markers: usize,
    pub publication_action: &'static str,
}

pub fn rename(options: &TaskRenameOptions) -> Result<TaskRenameReport> {
    validate_task_slug(&options.new_slug)?;
    let task_repo = match &options.manifest {
        Some(path) => GitRepo::discover_for_manifest(path)?,
        None => GitRepo::discover(&options.start)?,
    };
    let _repository_lock = RepositoryLock::acquire(&task_repo)?;
    let config = Config::load_compatible(&task_repo)?;
    let manifest_path = match &options.manifest {
        Some(path) => path.clone(),
        None => ResolvedTask::discover(&task_repo, &options.start)?,
    };
    let task = ResolvedTask::load(&task_repo, &config, &manifest_path)?;
    crate::cloud_usage::remind(&task_repo, &task);
    if task.slug == options.new_slug {
        return Err(Error::message(format!(
            "task already uses slug {:?}",
            options.new_slug
        )));
    }
    task_repo.validate_branch(&task.branch)?;
    task_repo.validate_remote_name(&task.remote)?;
    validate_checkout(&task_repo, &task, "task rename")?;

    let identity = parse_task_identity(task.kind, &task.task_id)?;
    let new_task_path =
        (task.kind == TaskKind::Deliverable).then(|| build_task_path(&identity, &options.new_slug));
    validate_additional_scopes(new_task_path.as_deref(), task.additional_scopes.clone())?;
    let local_branch_oid = task_repo
        .optional_oid(&format!("refs/heads/{}", task.branch))?
        .ok_or_else(|| Error::message("task local branch does not exist"))?;
    let remote_base_oid = task_repo.fetch_branch(&task.remote, &task.base_branch)?;
    // The shared branch may already require a newer workspace-mgr than the
    // local checkout declares.
    require_supported_cli_at(
        &task_repo,
        &remote_base_oid,
        &format!("{}/{}", task.remote, task.base_branch),
    )?;
    let remote_branch_oid = inspect_remote_task(&task_repo, &task, &remote_base_oid)?;
    validate_paths(
        &task_repo,
        &task,
        new_task_path.as_deref(),
        &remote_base_oid,
        remote_branch_oid.as_deref(),
    )?;
    let relocation = match (&task.task_path, &new_task_path) {
        (Some(old), Some(new)) => {
            crate::nested_git::validate_move(&task_repo, old, new)?;
            Some(RelocationPlan::opaque(
                &resolved_under(&task_repo.root, old),
                &resolved_under(&task_repo.root, new),
            )?)
        }
        _ => None,
    };
    let migration = match (&task.task_path, &new_task_path) {
        (Some(old), Some(new)) => plan_storage_rename(&task_repo, &config, &task, old, new)?,
        _ => None,
    };
    let storage_migration = migration.as_ref().map(|migration| {
        let rows = migration.receipt["versions"].as_array().expect("validated versions");
        let delete_markers = rows.iter().filter(|row| row["delete_marker"] == true).count();
        TaskRenameStorageMigration {
            status: "planned",
            source: migration.receipt["source"].as_str().expect("validated source").to_owned(),
            destination: migration.receipt["destination"].as_str().expect("validated destination").to_owned(),
            preserved_versions: rows.len(),
            payload_versions: rows.len() - delete_markers,
            delete_markers,
            publication_action: "server-copy history; upload changed payloads only; retire source after verified Git publication",
        }
    });

    // The rewritten manifest keeps every other field, including a recorded
    // cloud-usage approval, and uses the lowest schema that represents it.
    let mut new_manifest = task.manifest();
    new_manifest.slug = options.new_slug.clone();
    new_manifest.path = new_task_path.clone();
    let rendered = new_manifest.render()?;
    let new_manifest_path = match &new_task_path {
        Some(path) => resolved_under(&task_repo.root, &format!("{path}/{TASK_MANIFEST_NAME}")),
        None => task.manifest_path.clone(),
    };
    let local_actions = match (&task.task_path, &new_task_path) {
        (Some(old), Some(new)) => vec![
            TaskRenameAction {
                action: "move-directory",
                from: old.clone(),
                to: new.clone(),
            },
            TaskRenameAction {
                action: "rewrite-manifest",
                from: task.manifest_path.display().to_string(),
                to: new_manifest_path.display().to_string(),
            },
        ],
        (None, None) => vec![TaskRenameAction {
            action: "rewrite-private-manifest",
            from: task.manifest_path.display().to_string(),
            to: new_manifest_path.display().to_string(),
        }],
        _ => unreachable!(),
    };

    if !options.dry_run {
        apply_rename(
            &task_repo,
            &config,
            &task,
            new_task_path.as_deref(),
            &rendered,
            relocation.as_ref(),
            migration.as_ref(),
        )?;
    }

    Ok(TaskRenameReport {
        status: if options.dry_run {
            "dry_run"
        } else {
            "renamed"
        }
        .to_owned(),
        operation: "task-rename".to_owned(),
        kind: task.kind,
        task_id: task.task_id,
        old_slug: task.slug,
        new_slug: options.new_slug.clone(),
        old_path: task.task_path,
        new_path: new_task_path,
        old_manifest: task.manifest_path.display().to_string(),
        new_manifest: new_manifest_path.display().to_string(),
        branch: task.branch,
        local_branch_oid,
        remote_branch_oid,
        local_actions,
        remote_writes: false,
        storage_migration,
        notices: if !options.dry_run && relocation.is_some() {
            vec![RelocationNotice::renamed_directory()]
        } else {
            Vec::new()
        },
        review: TaskRenameReview {
            head_branch_unchanged: true,
            pull_request: "reuse-existing-draft",
            agent_action: "update the existing pull request title and description after the renamed task is published",
        },
    })
}

struct StorageRename {
    receipt: Value,
    previous: Option<Value>,
}

/// Freeze exact remote history before moving the local directory. Publication
/// copies those versions on the server and then rebases their local bindings.
fn plan_storage_rename(
    repo: &GitRepo,
    config: &Config,
    task: &ResolvedTask,
    source: &str,
    destination: &str,
) -> Result<Option<StorageRename>> {
    let receipt_path = format!("{source}/{}", crate::archive_migration::RECEIPT_NAME);
    let absolute = resolved_under(&repo.root, &receipt_path);
    if absolute.exists() {
        reject_symlink_traversal(&repo.root, &receipt_path, "task rename receipt")?;
        let raw = fs::read(&absolute).at(&absolute)?;
        let previous: Value = serde_json::from_slice(&raw)
            .map_err(|error| Error::message(format!("invalid task rename receipt: {error}")))?;
        crate::archive_migration::validate(&receipt_path, &previous)?;
        crate::archive_cancel::validate_planned_rename_retarget(repo, task, &previous)?;
        let mut receipt = previous.clone();
        receipt["destination"] = destination.into();
        let original_source = previous["source"].as_str().expect("validated source");
        for row in receipt["versions"]
            .as_array_mut()
            .expect("validated versions")
        {
            let object = row["source_object"]
                .as_str()
                .expect("validated source object");
            row["destination_object"] =
                format!("{destination}{}", &object[original_source.len()..]).into();
        }
        crate::archive_migration::validate(
            &format!("{destination}/{}", crate::archive_migration::RECEIPT_NAME),
            &receipt,
        )?;
        return Ok(Some(StorageRename {
            receipt,
            previous: Some(previous),
        }));
    }
    if !config.requires_object_versioning() {
        return Ok(None);
    }
    let pointers = storage_metadata::discover(repo, &[source.to_owned()])?;
    // A normalized MD5 cannot prove raw-byte equality after changing the
    // physical cache key. Keep the exact source cache as the trusted raw-byte
    // association; preparation rebinds it after verifying the copy.
    for pointer in &pointers {
        let path = resolved_under(&repo.root, pointer);
        let raw = fs::read_to_string(&path).at(&path)?;
        if storage_metadata::hash_algorithm(&raw, pointer)? == "md5-dos2unix" {
            for entry in storage_metadata::parse_pointer_document(&raw, pointer)?.entries(pointer) {
                if entry.version_id.is_some() && entry.verification.is_none() {
                    let source = crate::native_engine::StorageEntry {
                        pointer: pointer.clone(),
                        object: entry.key,
                        md5: entry.md5,
                        size: entry.size,
                        version_id: entry.version_id,
                        etag: entry.etag,
                        verification: None,
                        hash_name: "md5-dos2unix".to_owned(),
                    };
                    let cache = crate::native_engine::cache_path_for_entry(repo, &source)?;
                    if !cache.is_file() {
                        return Err(Error::message(
                            "S3 task rename requires the exact source-version cache for legacy normalized storage; hydrate or migrate its storage metadata before renaming",
                        ));
                    }
                }
            }
        }
    }
    let mut receipt = crate::archive_migration::plan(repo, config, source, destination)?;
    receipt["task_id"] = task.task_id.clone().into();
    receipt["migration_kind"] = "task-rename".into();
    crate::archive_migration::validate(
        &format!("{destination}/{}", crate::archive_migration::RECEIPT_NAME),
        &receipt,
    )?;
    for pointer in pointers {
        let path = resolved_under(&repo.root, &pointer);
        let raw = fs::read_to_string(&path).at(&path)?;
        let document = storage_metadata::parse_pointer_document(&raw, &pointer)?;
        for entry in document.entries(&pointer) {
            if let Some(version) = entry.version_id {
                let bound = receipt["versions"]
                    .as_array()
                    .expect("validated versions")
                    .iter()
                    .any(|row| {
                        row["source_object"] == entry.key
                            && row["source_version_id"] == version
                            && row["delete_marker"] == false
                    });
                if !bound {
                    return Err(Error::message(format!(
                        "task rename cannot preserve missing exact source version for {}",
                        entry.key
                    )));
                }
            }
        }
    }
    Ok(Some(StorageRename {
        receipt,
        previous: None,
    }))
}

/// Every writable task uses the shared checkout on the base branch.
/// `operation` names the command in refusals.
pub(crate) fn validate_checkout(
    repo: &GitRepo,
    task: &ResolvedTask,
    operation: &str,
) -> Result<()> {
    let current = repo.current_branch()?;
    if current.as_deref() != Some(&task.base_branch) {
        return Err(Error::message(format!(
            "{:?} {operation} must run from the shared checkout on {:?}; current branch is {:?}",
            task.kind,
            task.base_branch,
            current.as_deref().unwrap_or("detached HEAD")
        )));
    }
    repo.ensure_branch_not_checked_out(&task.branch)?;
    Ok(())
}

fn inspect_remote_task(
    repo: &GitRepo,
    task: &ResolvedTask,
    remote_base_oid: &str,
) -> Result<Option<String>> {
    let Some(observed) = repo.remote_branch_oid(&task.remote, &task.branch)? else {
        return Ok(None);
    };
    let fetched = repo.fetch_branch(&task.remote, &task.branch)?;
    if fetched != observed {
        return Err(Error::message(
            "task branch changed while rename state was being inspected; retry",
        ));
    }
    validate_remote_task_identity(repo, task, &fetched)?;
    // A newer release may have published the task branch with a declaration
    // this one does not meet.
    require_supported_cli_at(repo, &fetched, &format!("{}/{}", task.remote, task.branch))?;
    let merged = repo.run_unchecked(["merge-base", "--is-ancestor", &fetched, remote_base_oid])?;
    match merged.code {
        0 => Err(Error::message(
            "task branch is already contained in the remote shared branch; merged tasks cannot be renamed",
        )),
        1 => Ok(Some(fetched)),
        _ => Err(Error::message(
            "failed to determine whether the task branch was merged",
        )),
    }
}

fn validate_paths(
    repo: &GitRepo,
    task: &ResolvedTask,
    new_task_path: Option<&str>,
    remote_base_oid: &str,
    remote_branch_oid: Option<&str>,
) -> Result<()> {
    let (Some(old_path), Some(new_path)) = (task.task_path.as_deref(), new_task_path) else {
        return Ok(());
    };
    reject_symlink_traversal(&repo.root, old_path, "task rename source")?;
    reject_symlink_traversal(&repo.root, new_path, "task rename destination")?;
    let old = resolved_under(&repo.root, old_path);
    let metadata = fs::symlink_metadata(&old).at(&old)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Error::message(format!(
            "task rename source must be an ordinary directory: {old_path}"
        )));
    }
    let new = resolved_under(&repo.root, new_path);
    if path_exists(&new)? {
        return Err(Error::message(format!(
            "task rename destination already exists: {new_path}"
        )));
    }
    let staged = repo.run_unchecked(["diff", "--cached", "--quiet", "--", old_path, new_path])?;
    match staged.code {
        0 => {}
        1 => {
            return Err(Error::message(
                "task rename refuses staged changes in the source or destination; unstage them first",
            ));
        }
        _ => return Err(Error::message("failed to inspect staged task changes")),
    }
    if tree_path_exists(repo, remote_base_oid, old_path)? {
        return Err(Error::message(
            "task directory already exists in the remote shared branch; merged tasks cannot be renamed",
        ));
    }
    let destination_is_published = tree_path_exists(repo, remote_base_oid, new_path)?
        || match remote_branch_oid {
            Some(oid) => tree_path_exists(repo, oid, new_path)?,
            None => false,
        };
    if destination_is_published {
        return Err(Error::message(format!(
            "task rename destination already exists in published Git history: {new_path}"
        )));
    }
    Ok(())
}

fn apply_rename(
    repo: &GitRepo,
    config: &Config,
    task: &ResolvedTask,
    new_task_path: Option<&str>,
    rendered: &str,
    relocation: Option<&RelocationPlan>,
    migration: Option<&StorageRename>,
) -> Result<()> {
    match (task.task_path.as_deref(), new_task_path) {
        (Some(old_path), Some(new_path)) => {
            let old = resolved_under(&repo.root, old_path);
            let new = resolved_under(&repo.root, new_path);
            let original = fs::read_to_string(&task.manifest_path).at(&task.manifest_path)?;
            let pointer_snapshots = moved_pointer_snapshots(repo, old_path, new_path)?;
            let receipt_name = crate::archive_migration::RECEIPT_NAME;
            let original_receipt = match fs::read_to_string(old.join(receipt_name)) {
                Ok(raw) => Some(raw),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(source) => {
                    return Err(Error::Io {
                        path: old.join(receipt_name),
                        source,
                    });
                }
            };
            if let Some(migration) = migration {
                if let Some(previous) = &migration.previous {
                    crate::archive_cancel::record_retargeted_rename(
                        repo,
                        task,
                        previous,
                        &migration.receipt,
                    )?;
                } else {
                    crate::archive_cancel::record_attempt(
                        repo,
                        task,
                        old_path,
                        new_path,
                        relocation.expect("deliverable relocation"),
                        &migration.receipt,
                    )?;
                }
            }
            if let Err(error) = fs::rename(&old, &new).at(&old) {
                if let Some(migration) = migration {
                    crate::archive_cancel::rolled_back(
                        repo,
                        migration.receipt["source"]
                            .as_str()
                            .expect("validated source"),
                        new_path,
                    )?;
                }
                return Err(error);
            }
            let new_manifest = new.join(TASK_MANIFEST_NAME);
            let mut changed_pointers = Vec::new();
            let mut manifest_rewritten = false;
            let mut receipt_rewritten = false;
            let result = (|| {
                if let Some(relocation) = relocation {
                    relocation.apply()?;
                }
                for (index, snapshot) in pointer_snapshots
                    .iter()
                    .enumerate()
                    .filter(|_| migration.is_none())
                {
                    if storage_metadata::reset_moved_pointer_cloud_metadata(
                        repo,
                        &snapshot.new_path,
                    )? {
                        changed_pointers.push(index);
                    }
                }
                if let Some(migration) = migration {
                    let raw = format!(
                        "{}\n",
                        serde_json::to_string_pretty(&migration.receipt)
                            .map_err(|error| Error::message(error.to_string()))?
                    );
                    atomic_write(&new.join(receipt_name), &raw)?;
                    receipt_rewritten = true;
                    crate::archive_cancel::record_pointer_rewrite(
                        repo,
                        &migration.receipt,
                        &format!("{new_path}/{TASK_MANIFEST_NAME}"),
                        rendered.as_bytes(),
                    )?;
                }
                atomic_write(&new_manifest, rendered)?;
                manifest_rewritten = true;
                ResolvedTask::load(repo, config, &new_manifest)?;
                if let Some(migration) = migration {
                    crate::archive_cancel::moved(
                        repo,
                        migration.receipt["source"]
                            .as_str()
                            .expect("validated source"),
                        new_path,
                    )?;
                }
                Ok(())
            })();
            if let Err(error) = result {
                let receipt_rollback = if receipt_rewritten {
                    match &original_receipt {
                        Some(raw) => atomic_write(&new.join(receipt_name), raw),
                        None => fs::remove_file(new.join(receipt_name)).at(new.join(receipt_name)),
                    }
                } else {
                    Ok(())
                };
                let rollback = combine_rollbacks(
                    receipt_rollback,
                    rollback_deliverable(
                        &new,
                        &old,
                        manifest_rewritten.then_some((&new_manifest, original.as_str())),
                        &pointer_snapshots,
                        &changed_pointers,
                        relocation,
                    ),
                );
                let rollback = if let Some(migration) = migration {
                    combine_rollbacks(
                        rollback,
                        crate::archive_cancel::rolled_back(
                            repo,
                            migration.receipt["source"]
                                .as_str()
                                .expect("validated source"),
                            new_path,
                        ),
                    )
                } else {
                    rollback
                };
                return Err(rollback_error(error, rollback));
            }
            if let Some(migration) = migration
                && let Some(previous) = &migration.previous
            {
                crate::archive_cancel::rolled_back(
                    repo,
                    previous["source"].as_str().expect("validated source"),
                    old_path,
                )?;
            }
            Ok(())
        }
        (None, None) => {
            let original = fs::read_to_string(&task.manifest_path).at(&task.manifest_path)?;
            atomic_write(&task.manifest_path, rendered)?;
            if let Err(error) = ResolvedTask::load(repo, config, &task.manifest_path) {
                return Err(rollback_error(
                    error,
                    atomic_write(&task.manifest_path, &original),
                ));
            }
            Ok(())
        }
        _ => unreachable!(),
    }
}

#[derive(Debug)]
struct MovedPointerSnapshot {
    new_path: String,
    new_absolute: PathBuf,
    contents: String,
}

fn moved_pointer_snapshots(
    repo: &GitRepo,
    old_task_path: &str,
    new_task_path: &str,
) -> Result<Vec<MovedPointerSnapshot>> {
    storage_metadata::discover(repo, &[old_task_path.to_owned()])?
        .into_iter()
        .map(|old_pointer| {
            let relative = old_pointer
                .strip_prefix(&format!("{old_task_path}/"))
                .ok_or_else(|| Error::message("managed-storage metadata escaped the task"))?;
            let new_path = format!("{new_task_path}/{relative}");
            let contents =
                fs::read_to_string(resolved_under(&repo.root, &old_pointer)).at(&old_pointer)?;
            let new_absolute = resolved_under(&repo.root, &new_path);
            Ok(MovedPointerSnapshot {
                new_path,
                new_absolute,
                contents,
            })
        })
        .collect()
}

fn rollback_deliverable(
    new_directory: &Path,
    old_directory: &Path,
    manifest: Option<(&Path, &str)>,
    pointer_snapshots: &[MovedPointerSnapshot],
    changed_pointers: &[usize],
    relocation: Option<&RelocationPlan>,
) -> Result<()> {
    let mut rollback = Ok(());
    for index in changed_pointers {
        let snapshot = &pointer_snapshots[*index];
        rollback = combine_rollbacks(
            rollback,
            atomic_write(&snapshot.new_absolute, &snapshot.contents),
        );
    }
    if let Some((path, contents)) = manifest {
        rollback = combine_rollbacks(rollback, atomic_write(path, contents));
    }
    if let Some(relocation) = relocation
        && let Err(error) = relocation.restore()
    {
        return combine_rollbacks(rollback, Err(error));
    }
    combine_rollbacks(
        rollback,
        fs::rename(new_directory, old_directory).at(new_directory),
    )
}

fn tree_path_exists(repo: &GitRepo, oid: &str, path: &str) -> Result<bool> {
    let checked = repo.run_unchecked(["cat-file", "-e", &format!("{oid}:{path}")])?;
    match checked.code {
        0 => Ok(true),
        1 | 128 => Ok(false),
        _ => Err(Error::message(format!(
            "failed to inspect published task path {path:?}"
        ))),
    }
}

fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(Error::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

pub(crate) fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::message("task manifest has no parent"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).at(parent)?;
    temporary.write_all(contents.as_bytes()).at(path)?;
    if let Ok(metadata) = fs::metadata(path) {
        temporary
            .as_file()
            .set_permissions(metadata.permissions())
            .at(path)?;
    }
    temporary.flush().at(path)?;
    temporary.persist(path).map_err(|error| Error::Io {
        path: path.to_path_buf(),
        source: error.error,
    })?;
    Ok(())
}

fn rollback_error(error: Error, rollback: Result<()>) -> Error {
    match rollback {
        Ok(()) => error,
        Err(rollback) => Error::message(format!(
            "task rename failed: {error}; rollback also failed: {rollback}"
        )),
    }
}

fn combine_rollbacks(first: Result<()>, second: Result<()>) -> Result<()> {
    match (first, second) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(first), Ok(())) => Err(first),
        (Ok(()), Err(second)) => Err(second),
        (Err(first), Err(second)) => Err(Error::message(format!(
            "manifest rollback failed: {first}; directory rollback failed: {second}"
        ))),
    }
}

#[cfg(test)]
#[path = "task_rename_tests.rs"]
mod tests;
