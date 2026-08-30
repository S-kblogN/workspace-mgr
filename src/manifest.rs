use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::path::repo_path;
use crate::policy::{TASK_BRANCH_PREFIX, TASK_MANIFEST_NAME};

pub const INFRASTRUCTURE_MANIFEST_NAME: &str = "workspace-mgr/task.toml";
pub const TASK_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum TaskKind {
    #[default]
    Deliverable,
    Infrastructure,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskManifest {
    pub schema_version: u32,
    #[serde(default)]
    pub kind: TaskKind,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    #[serde(default)]
    pub additional_scopes: Vec<AdditionalScope>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AdditionalScope {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedTask {
    pub manifest_path: PathBuf,
    pub kind: TaskKind,
    pub task_id: String,
    pub task_path: Option<String>,
    pub branch: String,
    pub title: Option<String>,
    pub purpose: Option<String>,
    pub remote: String,
    pub base_branch: String,
    pub shared_head: String,
    pub additional_scopes: Vec<AdditionalScope>,
}

impl TaskManifest {
    pub fn render(&self) -> Result<String> {
        toml::to_string_pretty(self)
            .map_err(|error| Error::message(format!("failed to render task manifest: {error}")))
    }
}

impl ResolvedTask {
    pub fn load(repo: &GitRepo, config: &Config, path: &Path) -> Result<Self> {
        let absolute = path.canonicalize().map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let raw = fs::read_to_string(&absolute).at(&absolute)?;
        let manifest: TaskManifest = toml::from_str(&raw).map_err(|source| Error::Toml {
            path: absolute.clone(),
            source,
        })?;
        if manifest.schema_version != TASK_SCHEMA_VERSION {
            return Err(Error::message(format!(
                "unsupported task schema {}, expected {}",
                manifest.schema_version, TASK_SCHEMA_VERSION
            )));
        }
        let task_id = one_line(&manifest.id, "task id")?;
        let additional_scopes = validate_scopes(manifest.additional_scopes)?;
        let task_path = match manifest.kind {
            TaskKind::Deliverable => {
                let raw_path = manifest
                    .path
                    .as_deref()
                    .ok_or_else(|| Error::message("deliverable task manifest requires path"))?;
                let task_path = repo_path(raw_path, "task path")?;
                if task_path.contains('/') {
                    return Err(Error::message(
                        "task path must be a directory directly below the repository root",
                    ));
                }
                if task_id != task_path {
                    return Err(Error::message(format!(
                        "task id must match task path {task_path:?}; got {task_id:?}"
                    )));
                }
                let expected = repo.root.join(&task_path).join(TASK_MANIFEST_NAME);
                if absolute != expected {
                    return Err(Error::message(format!(
                        "deliverable task manifest must be located at {}; got {}",
                        expected.display(),
                        absolute.display()
                    )));
                }
                Some(task_path)
            }
            TaskKind::Infrastructure => {
                if manifest.path.is_some() {
                    return Err(Error::message(
                        "infrastructure task manifest must not declare a task path",
                    ));
                }
                if additional_scopes.is_empty() {
                    return Err(Error::message(
                        "infrastructure task manifest requires at least one declared scope",
                    ));
                }
                let expected = repo.git_dir()?.join(INFRASTRUCTURE_MANIFEST_NAME);
                if absolute != expected {
                    return Err(Error::message(format!(
                        "infrastructure task manifest must be private worktree state at {}; got {}",
                        expected.display(),
                        absolute.display()
                    )));
                }
                None
            }
        };
        let branch = one_line(&manifest.branch, "task branch")?;
        if !branch.starts_with(TASK_BRANCH_PREFIX) {
            return Err(Error::message(format!(
                "task branch must use the fixed {TASK_BRANCH_PREFIX:?} prefix"
            )));
        }
        Ok(Self {
            manifest_path: absolute,
            kind: manifest.kind,
            task_id,
            task_path,
            branch,
            title: manifest
                .title
                .map(|value| one_line(&value, "task title"))
                .transpose()?,
            purpose: manifest
                .purpose
                .map(|value| one_line(&value, "task purpose"))
                .transpose()?,
            remote: config.git.remote.clone(),
            base_branch: config.git.branch.clone(),
            shared_head: config.git.branch.clone(),
            additional_scopes,
        })
    }

    pub fn discover(repo: &GitRepo, start: &Path) -> Result<PathBuf> {
        let mut current = if start.is_dir() {
            start.canonicalize().map_err(|source| Error::Io {
                path: start.to_path_buf(),
                source,
            })?
        } else {
            start
                .parent()
                .ok_or_else(|| Error::message("cannot discover manifest from this path"))?
                .canonicalize()
                .map_err(|source| Error::Io {
                    path: start.to_path_buf(),
                    source,
                })?
        };
        loop {
            let candidate = current.join(TASK_MANIFEST_NAME);
            if candidate.is_file() {
                return Ok(candidate);
            }
            if current == repo.root {
                break;
            }
            if !current.pop() || !current.starts_with(&repo.root) {
                break;
            }
        }
        let infrastructure = repo.git_dir()?.join(INFRASTRUCTURE_MANIFEST_NAME);
        if infrastructure.is_file() {
            return Ok(infrastructure);
        }
        Err(Error::message(format!(
            "no task manifest found from {} to {}",
            start.display(),
            repo.root.display()
        )))
    }

    pub fn scopes(&self) -> Vec<String> {
        self.task_path
            .iter()
            .cloned()
            .chain(
                self.additional_scopes
                    .iter()
                    .map(|entry| entry.path.clone()),
            )
            .collect()
    }
}

fn validate_scopes(scopes: Vec<AdditionalScope>) -> Result<Vec<AdditionalScope>> {
    scopes
        .into_iter()
        .map(|entry| {
            Ok(AdditionalScope {
                path: repo_path(&entry.path, "additional scope path")?,
                reason: one_line(&entry.reason, "additional scope reason")?,
            })
        })
        .collect()
}

pub fn one_line(value: &str, field: &str) -> Result<String> {
    if value.contains(['\n', '\r']) {
        return Err(Error::message(format!("{field} must be a single line")));
    }
    let value = value.trim().to_owned();
    if value.is_empty() {
        return Err(Error::message(format!("{field} must not be empty")));
    }
    Ok(value)
}
