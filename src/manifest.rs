use std::fs;
use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::path::repo_path;
use crate::policy::{TASK_BRANCH_PREFIX, TASK_MANIFEST_NAME};

pub const INFRASTRUCTURE_MANIFEST_NAME: &str = "workspace-mgr/task.toml";
pub const TASK_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum TaskKind {
    Deliverable,
    Infrastructure,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskManifest {
    pub schema_version: u32,
    pub kind: TaskKind,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub branch: String,
    pub title: String,
    pub purpose: String,
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
    pub title: String,
    pub purpose: String,
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
        let additional_scopes =
            validate_additional_scopes(task_path.as_deref(), manifest.additional_scopes)?;
        if manifest.kind == TaskKind::Infrastructure && additional_scopes.is_empty() {
            return Err(Error::message(
                "infrastructure task manifest requires at least one declared scope",
            ));
        }
        let branch = one_line(&manifest.branch, "task branch")?;
        let expected_branch = expected_task_branch(manifest.kind, &task_id)?;
        if branch != expected_branch {
            return Err(Error::message(format!(
                "task branch must be {expected_branch:?} for task {task_id:?}; got {branch:?}"
            )));
        }
        let title = one_line(&manifest.title, "task title")?;
        let purpose = one_line(&manifest.purpose, "task purpose")?;
        Ok(Self {
            manifest_path: absolute,
            kind: manifest.kind,
            task_id,
            task_path,
            branch,
            title,
            purpose,
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

pub fn validate_additional_scopes(
    task_path: Option<&str>,
    scopes: Vec<AdditionalScope>,
) -> Result<Vec<AdditionalScope>> {
    let mut scopes = scopes
        .into_iter()
        .map(|entry| {
            Ok(AdditionalScope {
                path: repo_path(&entry.path, "additional scope path")?,
                reason: one_line(&entry.reason, "additional scope reason")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    scopes.sort_by(|left, right| left.path.cmp(&right.path));
    let mut declared = task_path
        .into_iter()
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    for entry in &scopes {
        if let Some(existing) = declared
            .iter()
            .find(|existing| paths_overlap(existing, &entry.path))
        {
            return Err(Error::message(format!(
                "declared task scopes must not overlap: {:?} conflicts with {:?}",
                entry.path, existing
            )));
        }
        declared.push(entry.path.clone());
    }
    Ok(scopes)
}

fn paths_overlap(left: &str, right: &str) -> bool {
    left == right
        || left.starts_with(&format!("{right}/"))
        || right.starts_with(&format!("{left}/"))
}

pub fn validate_task_slug(slug: &str) -> Result<()> {
    let valid = !slug.is_empty()
        && !slug.starts_with('-')
        && !slug.ends_with('-')
        && !slug.contains("--")
        && slug
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if !valid {
        return Err(Error::message(
            "task slug must be lowercase kebab case using ASCII letters and digits",
        ));
    }
    Ok(())
}

pub fn validate_task_timestamp(timestamp: &str) -> Result<()> {
    let shape_is_valid = timestamp.len() == 15
        && timestamp.as_bytes().get(8) == Some(&b'-')
        && timestamp
            .bytes()
            .enumerate()
            .all(|(index, byte)| index == 8 || byte.is_ascii_digit());
    if !shape_is_valid || NaiveDateTime::parse_from_str(timestamp, "%Y%m%d-%H%M%S").is_err() {
        return Err(Error::message(
            "timestamp must be a valid local date and time using YYYYMMDD-HHMMSS",
        ));
    }
    Ok(())
}

pub fn build_task_id(kind: TaskKind, slug: &str, timestamp: Option<&str>) -> Result<String> {
    validate_task_slug(slug)?;
    match kind {
        TaskKind::Deliverable => {
            let timestamp = timestamp
                .ok_or_else(|| Error::message("deliverable task identity requires a timestamp"))?;
            validate_task_timestamp(timestamp)?;
            Ok(format!("{timestamp}-{slug}"))
        }
        TaskKind::Infrastructure => {
            if timestamp.is_some() {
                return Err(Error::message("infrastructure tasks do not use timestamps"));
            }
            Ok(format!("infra-{slug}"))
        }
    }
}

pub fn build_task_branch(kind: TaskKind, slug: &str) -> Result<String> {
    validate_task_slug(slug)?;
    Ok(match kind {
        TaskKind::Deliverable => format!("{TASK_BRANCH_PREFIX}{slug}"),
        TaskKind::Infrastructure => format!("{TASK_BRANCH_PREFIX}infra-{slug}"),
    })
}

fn expected_task_branch(kind: TaskKind, task_id: &str) -> Result<String> {
    let slug = match kind {
        TaskKind::Deliverable => {
            let timestamp = task_id.get(..15).ok_or_else(|| {
                Error::message(
                    "deliverable task id must use YYYYMMDD-HHMMSS-<lowercase-kebab-slug>",
                )
            })?;
            if task_id.get(15..16) != Some("-") {
                return Err(Error::message(
                    "deliverable task id must use YYYYMMDD-HHMMSS-<lowercase-kebab-slug>",
                ));
            }
            validate_task_timestamp(timestamp)?;
            task_id.get(16..).ok_or_else(|| {
                Error::message(
                    "deliverable task id must use YYYYMMDD-HHMMSS-<lowercase-kebab-slug>",
                )
            })?
        }
        TaskKind::Infrastructure => task_id.strip_prefix("infra-").ok_or_else(|| {
            Error::message("infrastructure task id must use infra-<lowercase-kebab-slug>")
        })?,
    };
    build_task_branch(kind, slug)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_task_identities_determine_their_branches() {
        assert_eq!(
            expected_task_branch(TaskKind::Deliverable, "20260829-170000-sample-task").unwrap(),
            "codex/sample-task"
        );
        assert_eq!(
            expected_task_branch(TaskKind::Infrastructure, "infra-shared-policy").unwrap(),
            "codex/infra-shared-policy"
        );
        assert!(expected_task_branch(TaskKind::Deliverable, "sample-task").is_err());
        assert!(
            expected_task_branch(TaskKind::Deliverable, "20261329-170000-sample-task").is_err()
        );
        assert!(expected_task_branch(TaskKind::Deliverable, "20260829-17000-sample-task").is_err());
        assert!(expected_task_branch(TaskKind::Infrastructure, "shared-policy").is_err());
    }

    #[test]
    fn additional_scopes_must_be_distinct_and_non_overlapping() {
        let scopes = vec![AdditionalScope {
            path: "task/output".to_owned(),
            reason: "Already part of the task.".to_owned(),
        }];
        assert!(validate_additional_scopes(Some("task"), scopes).is_err());

        let scopes = vec![
            AdditionalScope {
                path: "shared".to_owned(),
                reason: "Shared directory.".to_owned(),
            },
            AdditionalScope {
                path: "shared/file.txt".to_owned(),
                reason: "Redundant child.".to_owned(),
            },
        ];
        assert!(validate_additional_scopes(None, scopes).is_err());
    }
}
