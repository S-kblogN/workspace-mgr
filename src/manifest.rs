use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{Config, SCHEMA_VERSION};
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::path::{relative_to, repo_path};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskManifest {
    pub schema_version: u32,
    pub id: String,
    pub path: String,
    pub branch: String,
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
    pub task_id: String,
    pub task_path: String,
    pub branch: String,
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
        let file_name = absolute
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| Error::message("manifest file name is not valid UTF-8"))?;
        if file_name != config.tasks.manifest_name {
            return Err(Error::message(format!(
                "task manifest must be named {:?}",
                config.tasks.manifest_name
            )));
        }
        let raw = fs::read_to_string(&absolute).at(&absolute)?;
        let manifest: TaskManifest = toml::from_str(&raw).map_err(|source| Error::Toml {
            path: absolute.clone(),
            source,
        })?;
        if manifest.schema_version != SCHEMA_VERSION {
            return Err(Error::message(format!(
                "unsupported task schema {}, expected {}",
                manifest.schema_version, SCHEMA_VERSION
            )));
        }
        let task_path = repo_path(&manifest.path, "task path")?;
        if task_path.contains('/') {
            return Err(Error::message(
                "task path must be a directory directly below the repository root",
            ));
        }
        let task_id = one_line(&manifest.id, "task id")?;
        if task_id != task_path {
            return Err(Error::message(format!(
                "task id must match task path {task_path:?}; got {task_id:?}"
            )));
        }
        let expected = repo.root.join(&task_path).join(&config.tasks.manifest_name);
        if absolute != expected {
            return Err(Error::message(format!(
                "task manifest must be located at {}; got {}",
                expected.display(),
                absolute.display()
            )));
        }
        let additional_scopes = validate_scopes(manifest.additional_scopes)?;
        Ok(Self {
            manifest_path: absolute,
            task_id,
            task_path,
            branch: one_line(&manifest.branch, "task branch")?,
            remote: config.publication.remote.clone(),
            base_branch: config.publication.base_branch.clone(),
            shared_head: config.publication.shared_checkout_branch.clone(),
            additional_scopes,
        })
    }

    pub fn discover(repo: &GitRepo, config: &Config, start: &Path) -> Result<PathBuf> {
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
            let candidate = current.join(&config.tasks.manifest_name);
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
        Err(Error::message(format!(
            "no task manifest found from {} to {}",
            start.display(),
            repo.root.display()
        )))
    }

    pub fn scopes(&self) -> Vec<String> {
        std::iter::once(self.task_path.clone())
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

pub fn manifest_relative_path(repo: &GitRepo, path: &Path) -> Result<String> {
    relative_to(path, &repo.root, "manifest path")
}
