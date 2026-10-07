//! Durable task configuration upgrades without changing task contents.
use std::fs;
use std::path::PathBuf;

use serde::Serialize;

use crate::config::{Config, require_supported_cli_at};
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::lock::RepositoryLock;
use crate::manifest::{ResolvedTask, TaskKind, TaskManifest};
use crate::policy::TASK_MANIFEST_NAME;
use crate::task_rename::{atomic_write, validate_checkout};

pub struct TaskUpgradeOptions {
    pub start: PathBuf,
    pub manifest: Option<PathBuf>,
    pub dry_run: bool,
}

#[derive(Debug, Serialize)]
pub struct TaskUpgradeReport {
    pub status: &'static str,
    pub operation: &'static str,
    pub task_id: String,
    pub manifest: String,
    pub previous_schema_version: u32,
    pub schema_version: u32,
    pub completion_recorded: bool,
    pub remote_writes: bool,
    pub next_step: &'static str,
}

pub fn upgrade(options: &TaskUpgradeOptions) -> Result<TaskUpgradeReport> {
    let repo = match &options.manifest {
        Some(path) => GitRepo::discover_for_manifest(path)?,
        None => GitRepo::discover(&options.start)?,
    };
    let _lock = RepositoryLock::acquire(&repo)?;
    let config = Config::load_compatible(&repo)?;
    let manifest_path = match &options.manifest {
        Some(path) => path.clone(),
        None => ResolvedTask::discover(&repo, &options.start)?,
    };
    let task = ResolvedTask::load(&repo, &config, &manifest_path)?;
    validate_checkout(&repo, &task, "task upgrade")?;
    let current = fs::read_to_string(&task.manifest_path).at(&task.manifest_path)?;
    let previous: TaskManifest = toml::from_str(&current).map_err(|source| Error::Toml {
        path: task.manifest_path.clone(),
        source,
    })?;
    let mut next = task.manifest();
    if task.kind == TaskKind::Deliverable {
        let base = repo.fetch_branch(&config.git.remote, &config.git.branch)?;
        require_supported_cli_at(
            &repo,
            &base,
            &format!("{}/{}", config.git.remote, config.git.branch),
        )?;
        let path = format!(
            "{}/{}",
            task.task_path.as_deref().expect("deliverable task path"),
            TASK_MANIFEST_NAME
        );
        let published = repo.run_unchecked(["show", &format!("{base}:{path}")])?;
        if !published.success() {
            return Err(Error::message(
                "task upgrade requires the task manifest on the fetched shared branch; publish or refresh it first",
            ));
        }
        let published: TaskManifest = toml::from_str(&published.stdout).map_err(|error| {
            Error::message(format!("invalid current published task manifest: {error}"))
        })?;
        // Permit an idempotent retry of a local upgrade, but no independent
        // control metadata edits. Validate against the current shared manifest.
        let mut local_identity = previous.clone();
        let mut published_identity = published.clone();
        local_identity.archive_completion = None;
        published_identity.archive_completion = None;
        if local_identity.render()? != published_identity.render()? {
            return Err(Error::message(
                "task upgrade refuses unpublished task manifest changes; publish or refresh them first",
            ));
        }
        let staged = repo.run_unchecked(["diff", "--cached", "--quiet", "--", &path])?;
        match staged.code {
            0 => {}
            1 => {
                return Err(Error::message(
                    "task upgrade refuses staged task manifest changes; unstage them first",
                ));
            }
            _ => {
                return Err(Error::message(
                    "failed to inspect staged task manifest changes",
                ));
            }
        }
        // Only current shared control metadata supplies branch association
        // hints. Upgrade never evaluates ordinary task trees or reconstructs
        // completion proofs from their history.
        next.archive_completion = published.archive_completion;
        if previous.archive_completion.is_some()
            && previous.archive_completion != next.archive_completion
        {
            return Err(Error::message(
                "task upgrade refuses unpublished completion metadata changes; publish or refresh them first",
            ));
        }
    }
    let rendered = next.render()?;
    let changed = rendered != current;
    if !options.dry_run && changed {
        atomic_write(&task.manifest_path, &rendered)?;
    }
    Ok(TaskUpgradeReport {
        status: if options.dry_run {
            "dry_run"
        } else if changed {
            "upgraded"
        } else {
            "no_changes"
        },
        operation: "task-upgrade",
        task_id: task.task_id,
        manifest: task.manifest_path.display().to_string(),
        previous_schema_version: previous.schema_version,
        schema_version: next.minimal_schema_version(),
        completion_recorded: next.archive_completion.is_some(),
        remote_writes: false,
        next_step: if changed {
            "Publish the upgraded task manifest through its declared scope."
        } else {
            "Task configuration is current; archive will check current associated pull-request state."
        },
    })
}
