use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::Config;
use crate::dvc;
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::lock::RepositoryLock;
use crate::manifest::{
    ResolvedTask, TASK_SCHEMA_VERSION, TaskKind, TaskManifest, build_task_path,
    parse_task_identity, validate_additional_scopes, validate_task_slug,
};
use crate::path::{reject_symlink_traversal, resolved_under};
use crate::policy::TASK_MANIFEST_NAME;
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
    if task.slug == options.new_slug {
        return Err(Error::message(format!(
            "task already uses slug {:?}",
            options.new_slug
        )));
    }
    task_repo.validate_branch(&task.branch)?;
    task_repo.validate_remote_name(&task.remote)?;
    validate_checkout(&task_repo, &task)?;

    let identity = parse_task_identity(task.kind, &task.task_id)?;
    let new_task_path =
        (task.kind == TaskKind::Deliverable).then(|| build_task_path(&identity, &options.new_slug));
    validate_additional_scopes(new_task_path.as_deref(), task.additional_scopes.clone())?;
    let local_branch_oid = task_repo
        .optional_oid(&format!("refs/heads/{}", task.branch))?
        .ok_or_else(|| Error::message("task local branch does not exist"))?;
    let remote_base_oid = task_repo.fetch_branch(&task.remote, &task.base_branch)?;
    let remote_branch_oid = inspect_remote_task(&task_repo, &task, &remote_base_oid)?;
    validate_paths(
        &task_repo,
        &task,
        new_task_path.as_deref(),
        &remote_base_oid,
        remote_branch_oid.as_deref(),
    )?;

    let new_manifest = TaskManifest {
        schema_version: TASK_SCHEMA_VERSION,
        kind: task.kind,
        id: task.task_id.clone(),
        slug: options.new_slug.clone(),
        path: new_task_path.clone(),
        branch: task.branch.clone(),
        title: task.title.clone(),
        purpose: task.purpose.clone(),
        additional_scopes: task.additional_scopes.clone(),
    };
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
        review: TaskRenameReview {
            head_branch_unchanged: true,
            pull_request: "reuse-existing-draft",
            agent_action: "update the existing pull request title and description after the renamed task is published",
        },
    })
}

fn validate_checkout(repo: &GitRepo, task: &ResolvedTask) -> Result<()> {
    let current = repo.current_branch()?;
    match task.kind {
        TaskKind::Deliverable => {
            if current.as_deref() != Some(&task.base_branch) {
                return Err(Error::message(format!(
                    "deliverable task rename must run from the shared checkout on {:?}; current branch is {:?}",
                    task.base_branch,
                    current.as_deref().unwrap_or("detached HEAD")
                )));
            }
            repo.ensure_branch_not_checked_out(&task.branch)?;
        }
        TaskKind::Infrastructure => {
            if current.as_deref() != Some(&task.branch) {
                return Err(Error::message(format!(
                    "infrastructure task rename must run from its isolated worktree on {:?}",
                    task.branch
                )));
            }
            let expected = repo
                .common_dir()?
                .join("workspace-mgr/checkouts")
                .join(&task.task_id);
            if repo.root != expected {
                return Err(Error::message(format!(
                    "infrastructure task is not in its managed worktree: expected {}, got {}",
                    expected.display(),
                    repo.root.display()
                )));
            }
            if repo.branch_worktrees(&task.branch)? != vec![repo.root.clone()] {
                return Err(Error::message(
                    "infrastructure task branch must be checked out only in its managed worktree",
                ));
            }
        }
    }
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
) -> Result<()> {
    match (task.task_path.as_deref(), new_task_path) {
        (Some(old_path), Some(new_path)) => {
            let old = resolved_under(&repo.root, old_path);
            let new = resolved_under(&repo.root, new_path);
            let original = fs::read_to_string(&task.manifest_path).at(&task.manifest_path)?;
            let pointer_snapshots = moved_pointer_snapshots(repo, old_path, new_path)?;
            fs::rename(&old, &new).at(&old)?;
            let new_manifest = new.join(TASK_MANIFEST_NAME);
            let mut changed_pointers = Vec::new();
            let mut manifest_rewritten = false;
            let result = (|| {
                for (index, snapshot) in pointer_snapshots.iter().enumerate() {
                    if dvc::reset_moved_pointer_cloud_metadata(repo, &snapshot.new_path)? {
                        changed_pointers.push(index);
                    }
                }
                atomic_write(&new_manifest, rendered)?;
                manifest_rewritten = true;
                ResolvedTask::load(repo, config, &new_manifest).map(|_| ())
            })();
            if let Err(error) = result {
                let rollback = rollback_deliverable(
                    &new,
                    &old,
                    manifest_rewritten.then_some((&new_manifest, original.as_str())),
                    &pointer_snapshots,
                    &changed_pointers,
                );
                return Err(rollback_error(error, rollback));
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
    dvc::discover(repo, &[old_task_path.to_owned()])?
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

fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::message("task manifest has no parent"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).at(parent)?;
    temporary.write_all(contents.as_bytes()).at(path)?;
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
