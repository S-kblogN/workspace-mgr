use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{Config, SCHEMA_VERSION};
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::path::{relative_to, repo_path};

pub const LEGACY_MANIFEST_NAME: &str = ".chat-sync.json";

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

#[derive(Debug, Deserialize)]
struct LegacyManifest {
    version: u32,
    task_path: String,
    branch: String,
    #[serde(default = "default_remote")]
    remote: String,
    #[serde(default = "default_main")]
    base_branch: String,
    #[serde(default = "default_main")]
    shared_head: String,
    #[serde(default)]
    additional_paths: Vec<LegacyAdditionalPath>,
    #[serde(default)]
    #[allow(dead_code)]
    pr: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct LegacyAdditionalPath {
    path: String,
    reason: String,
}

fn default_remote() -> String {
    "origin".to_owned()
}

fn default_main() -> String {
    "main".to_owned()
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
    pub legacy: bool,
}

impl TaskManifest {
    pub fn render(&self) -> Result<String> {
        toml::to_string_pretty(self)
            .map_err(|error| Error::message(format!("failed to render task manifest: {error}")))
    }
}

impl ResolvedTask {
    pub fn load(repo: &GitRepo, config: Option<&Config>, path: &Path) -> Result<Self> {
        let absolute = path.canonicalize().map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let file_name = absolute
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| Error::message("manifest file name is not valid UTF-8"))?;
        if file_name == LEGACY_MANIFEST_NAME {
            return Self::load_legacy(repo, &absolute);
        }
        let config = config.ok_or_else(|| {
            Error::message(format!(
                "{} requires repository config {}",
                absolute.display(),
                crate::config::CONFIG_NAME
            ))
        })?;
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
            remote: config.git.remote.clone(),
            base_branch: config.git.base_branch.clone(),
            shared_head: config.git.shared_checkout_branch.clone(),
            additional_scopes,
            legacy: false,
        })
    }

    fn load_legacy(repo: &GitRepo, path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path).at(path)?;
        let manifest: LegacyManifest =
            serde_json::from_str(&raw).map_err(|source| Error::Json {
                path: path.to_path_buf(),
                source,
            })?;
        if manifest.version != 1 {
            return Err(Error::message("legacy manifest version must be 1"));
        }
        let task_path = repo_path(&manifest.task_path, "task_path")?;
        let expected = repo.root.join(&task_path).join(LEGACY_MANIFEST_NAME);
        if path != expected {
            return Err(Error::message(format!(
                "legacy manifest must be located at {}; got {}",
                expected.display(),
                path.display()
            )));
        }
        let additional_scopes = validate_scopes(
            manifest
                .additional_paths
                .into_iter()
                .map(|entry| AdditionalScope {
                    path: entry.path,
                    reason: entry.reason,
                })
                .collect(),
        )?;
        Ok(Self {
            manifest_path: path.to_path_buf(),
            task_id: task_path.clone(),
            task_path,
            branch: one_line(&manifest.branch, "branch")?,
            remote: one_line(&manifest.remote, "remote")?,
            base_branch: one_line(&manifest.base_branch, "base_branch")?,
            shared_head: one_line(&manifest.shared_head, "shared_head")?,
            additional_scopes,
            legacy: true,
        })
    }

    pub fn discover(repo: &GitRepo, config: Option<&Config>, start: &Path) -> Result<PathBuf> {
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
            if let Some(config) = config {
                let candidate = current.join(&config.tasks.manifest_name);
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
            let legacy = current.join(LEGACY_MANIFEST_NAME);
            if legacy.is_file() {
                return Ok(legacy);
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
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.is_empty() {
        return Err(Error::message(format!("{field} must not be empty")));
    }
    Ok(value)
}

pub fn manifest_relative_path(repo: &GitRepo, path: &Path) -> Result<String> {
    relative_to(path, &repo.root, "manifest path")
}
