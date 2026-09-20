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
pub const TASK_SCHEMA_VERSION: u32 = 2;
/// Schema 2 plus the optional `[cloud_usage_approval]` table.
pub const CLOUD_USAGE_APPROVAL_TASK_SCHEMA_VERSION: u32 = 3;
const LEGACY_TASK_SCHEMA_VERSION: u32 = 1;

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
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub slug: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub branch: String,
    pub title: String,
    pub purpose: String,
    #[serde(default)]
    pub additional_scopes: Vec<AdditionalScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_usage_approval: Option<CloudUsageApproval>,
}

/// The user's decision to let this task keep more cloud data than the fixed
/// threshold, recorded by `task approve-cloud-usage`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CloudUsageApproval {
    pub limit_bytes: u64,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AdditionalScope {
    pub path: String,
    pub reason: String,
}

/// The identity fields of a task manifest published on some branch. Unknown
/// fields are ignored so that optional fields added by newer releases cannot
/// break the repository-wide scan; a task's own manifest is parsed strictly.
#[derive(Debug, Deserialize)]
struct PublishedTaskIdentity {
    id: String,
    kind: String,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedTask {
    pub manifest_path: PathBuf,
    pub kind: TaskKind,
    pub task_id: String,
    pub slug: String,
    pub task_path: Option<String>,
    pub branch: String,
    pub title: String,
    pub purpose: String,
    pub remote: String,
    pub base_branch: String,
    pub shared_head: String,
    pub additional_scopes: Vec<AdditionalScope>,
    pub cloud_usage_approval: Option<CloudUsageApproval>,
}

impl TaskManifest {
    /// Renders the manifest with the lowest schema that represents it, so a
    /// manifest without an approval never requires a newer workspace-mgr.
    pub fn render(&self) -> Result<String> {
        let mut manifest = self.clone();
        manifest.schema_version = manifest.minimal_schema_version();
        toml::to_string_pretty(&manifest)
            .map_err(|error| Error::message(format!("failed to render task manifest: {error}")))
    }

    pub fn minimal_schema_version(&self) -> u32 {
        if self.cloud_usage_approval.is_some() {
            CLOUD_USAGE_APPROVAL_TASK_SCHEMA_VERSION
        } else {
            TASK_SCHEMA_VERSION
        }
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
        if !matches!(
            manifest.schema_version,
            LEGACY_TASK_SCHEMA_VERSION
                | TASK_SCHEMA_VERSION
                | CLOUD_USAGE_APPROVAL_TASK_SCHEMA_VERSION
        ) {
            return Err(Error::message(format!(
                "unsupported task schema {}, expected {}, {}, or {}",
                manifest.schema_version,
                LEGACY_TASK_SCHEMA_VERSION,
                TASK_SCHEMA_VERSION,
                CLOUD_USAGE_APPROVAL_TASK_SCHEMA_VERSION
            )));
        }
        let task_id = one_line(&manifest.id, "task id")?;
        let identity = parse_task_identity(manifest.kind, &task_id)?;
        let slug = match manifest.schema_version {
            LEGACY_TASK_SCHEMA_VERSION => {
                if !manifest.slug.is_empty() {
                    return Err(Error::message(
                        "task schema 1 must not declare the schema 2 slug field",
                    ));
                }
                identity.original_slug.clone()
            }
            TASK_SCHEMA_VERSION | CLOUD_USAGE_APPROVAL_TASK_SCHEMA_VERSION => {
                validate_task_slug(&manifest.slug)?;
                manifest.slug.clone()
            }
            _ => unreachable!(),
        };
        let cloud_usage_approval = match manifest.cloud_usage_approval {
            Some(_) if manifest.schema_version < CLOUD_USAGE_APPROVAL_TASK_SCHEMA_VERSION => {
                return Err(Error::message(format!(
                    "task schema {} must not declare the schema {} cloud_usage_approval table",
                    manifest.schema_version, CLOUD_USAGE_APPROVAL_TASK_SCHEMA_VERSION
                )));
            }
            Some(approval) => Some(CloudUsageApproval {
                limit_bytes: approval.limit_bytes,
                note: one_line(&approval.note, "cloud-usage approval note")?,
            }),
            None => None,
        };
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
                let expected_path = build_task_path(&identity, &slug);
                if task_path != expected_path {
                    return Err(Error::message(format!(
                        "deliverable task path must be {expected_path:?} for slug {slug:?}; got {task_path:?}"
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
        let expected_branch = build_task_branch(manifest.kind, &identity.original_slug)?;
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
            slug,
            task_path,
            branch,
            title,
            purpose,
            remote: config.git.remote.clone(),
            base_branch: config.git.branch.clone(),
            shared_head: config.git.branch.clone(),
            additional_scopes,
            cloud_usage_approval,
        })
    }

    /// The manifest that represents this task, in its lowest schema.
    pub fn manifest(&self) -> TaskManifest {
        let mut manifest = TaskManifest {
            schema_version: TASK_SCHEMA_VERSION,
            kind: self.kind,
            id: self.task_id.clone(),
            slug: self.slug.clone(),
            path: self.task_path.clone(),
            branch: self.branch.clone(),
            title: self.title.clone(),
            purpose: self.purpose.clone(),
            additional_scopes: self.additional_scopes.clone(),
            cloud_usage_approval: self.cloud_usage_approval.clone(),
        };
        manifest.schema_version = manifest.minimal_schema_version();
        manifest
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

pub(crate) fn published_task_paths(
    repo: &GitRepo,
    oid: &str,
    task: &ResolvedTask,
) -> Result<Vec<String>> {
    if task.kind != TaskKind::Deliverable {
        return Ok(Vec::new());
    }
    let listed = repo.run(["ls-tree", "-r", "-z", "--name-only", oid, "--"])?;
    let suffix = format!("/{TASK_MANIFEST_NAME}");
    let mut matches = Vec::new();
    for manifest_path in listed
        .stdout
        .split('\0')
        .filter(|path| path.ends_with(&suffix))
    {
        let raw = repo
            .run(["show", &format!("{oid}:{manifest_path}")])?
            .stdout;
        let manifest: PublishedTaskIdentity = toml::from_str(&raw).map_err(|error| {
            Error::message(format!(
                "failed to inspect published task manifest {manifest_path:?}: {error}"
            ))
        })?;
        if manifest.id != task.task_id {
            continue;
        }
        if manifest.kind != "deliverable" {
            return Err(Error::message(format!(
                "published manifest {manifest_path:?} has the task ID but the wrong task kind"
            )));
        }
        let declared = manifest
            .path
            .as_deref()
            .ok_or_else(|| Error::message("published deliverable manifest has no task path"))?;
        let path = repo_path(declared, "published task path")?;
        let containing = manifest_path
            .strip_suffix(&suffix)
            .ok_or_else(|| Error::message("published task manifest has an invalid path"))?;
        if path != containing {
            return Err(Error::message(format!(
                "published task manifest path mismatch: declared {path:?}, stored under {containing:?}"
            )));
        }
        matches.push(path);
    }
    matches.sort();
    matches.dedup();
    Ok(matches)
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskIdentity {
    pub timestamp: Option<String>,
    pub original_slug: String,
}

pub fn parse_task_identity(kind: TaskKind, task_id: &str) -> Result<TaskIdentity> {
    let (timestamp, slug) = match kind {
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
            let slug = task_id.get(16..).ok_or_else(|| {
                Error::message(
                    "deliverable task id must use YYYYMMDD-HHMMSS-<lowercase-kebab-slug>",
                )
            })?;
            (Some(timestamp.to_owned()), slug)
        }
        TaskKind::Infrastructure => (
            None,
            task_id.strip_prefix("infra-").ok_or_else(|| {
                Error::message("infrastructure task id must use infra-<lowercase-kebab-slug>")
            })?,
        ),
    };
    validate_task_slug(slug)?;
    Ok(TaskIdentity {
        timestamp,
        original_slug: slug.to_owned(),
    })
}

pub fn build_task_path(identity: &TaskIdentity, slug: &str) -> String {
    match &identity.timestamp {
        Some(timestamp) => format!("{timestamp}-{slug}"),
        None => format!("infra-{slug}"),
    }
}

pub(crate) fn published_history_path(
    path: &str,
    current_task_path: Option<&str>,
    published_task_path: Option<&str>,
) -> String {
    let (Some(current_root), Some(published_root)) = (current_task_path, published_task_path)
    else {
        return path.to_owned();
    };
    if path == current_root {
        return published_root.to_owned();
    }
    match path.strip_prefix(&format!("{current_root}/")) {
        Some(relative) => format!("{published_root}/{relative}"),
        None => path.to_owned(),
    }
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
    fn fixed_task_identities_determine_their_branches_and_mutable_paths() {
        let deliverable =
            parse_task_identity(TaskKind::Deliverable, "20260829-170000-sample-task").unwrap();
        assert_eq!(
            build_task_branch(TaskKind::Deliverable, &deliverable.original_slug).unwrap(),
            "codex/sample-task"
        );
        assert_eq!(
            build_task_path(&deliverable, "renamed-task"),
            "20260829-170000-renamed-task"
        );
        let infrastructure =
            parse_task_identity(TaskKind::Infrastructure, "infra-shared-policy").unwrap();
        assert_eq!(
            build_task_branch(TaskKind::Infrastructure, &infrastructure.original_slug).unwrap(),
            "codex/infra-shared-policy"
        );
        assert!(parse_task_identity(TaskKind::Deliverable, "sample-task").is_err());
        assert!(parse_task_identity(TaskKind::Deliverable, "20261329-170000-sample-task").is_err());
        assert!(parse_task_identity(TaskKind::Deliverable, "20260829-17000-sample-task").is_err());
        assert!(parse_task_identity(TaskKind::Infrastructure, "shared-policy").is_err());
        assert_eq!(
            published_history_path(
                "20260829-170000-renamed-task/output.bin",
                Some("20260829-170000-renamed-task"),
                Some("20260829-170000-sample-task"),
            ),
            "20260829-170000-sample-task/output.bin"
        );
    }

    #[test]
    fn published_manifests_of_other_tasks_may_carry_unknown_fields() {
        let temp = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: temp.path().to_path_buf(),
        };
        repo.run(["init", "-q", "-b", "main"]).unwrap();
        let task_id = "20260829-170000-sample-task";
        let write = |path: &str, raw: &str| {
            let absolute = repo.root.join(path);
            fs::create_dir_all(absolute.parent().unwrap()).unwrap();
            fs::write(absolute, raw).unwrap();
        };
        write(
            &format!("{task_id}/{TASK_MANIFEST_NAME}"),
            &format!(
                "schema_version = 2\nkind = \"deliverable\"\nid = \"{task_id}\"\nslug = \"sample-task\"\npath = \"{task_id}\"\nbranch = \"codex/sample-task\"\ntitle = \"Sample\"\npurpose = \"Sample\"\nadditional_scopes = []\n"
            ),
        );
        write(
            &format!("20260829-170001-newer/{TASK_MANIFEST_NAME}"),
            "schema_version = 3\nkind = \"future-kind\"\nid = \"20260829-170001-newer\"\npath = \"20260829-170001-newer\"\nbranch = \"codex/newer\"\ntitle = \"Newer\"\npurpose = \"Newer\"\n\n[future_table]\nlimit_bytes = 3000000000\n",
        );
        repo.run(["add", "-A"]).unwrap();
        let tree = repo.run(["write-tree"]).unwrap().stdout.trim().to_owned();
        let task = ResolvedTask {
            manifest_path: repo.root.join(task_id).join(TASK_MANIFEST_NAME),
            kind: TaskKind::Deliverable,
            task_id: task_id.to_owned(),
            slug: "sample-task".to_owned(),
            task_path: Some(task_id.to_owned()),
            branch: "codex/sample-task".to_owned(),
            title: "Sample".to_owned(),
            purpose: "Sample".to_owned(),
            remote: "origin".to_owned(),
            base_branch: "main".to_owned(),
            shared_head: "main".to_owned(),
            additional_scopes: Vec::new(),
            cloud_usage_approval: None,
        };
        assert_eq!(
            published_task_paths(&repo, &tree, &task).unwrap(),
            vec![task_id.to_owned()]
        );
    }

    fn deliverable_manifest(schema: u32, approval: &str) -> String {
        format!(
            "schema_version = {schema}\nkind = \"deliverable\"\nid = \"20260918-120000-demo\"\nslug = \"demo\"\npath = \"20260918-120000-demo\"\nbranch = \"codex/demo\"\ntitle = \"Demo\"\npurpose = \"Demo\"\nadditional_scopes = []\n{approval}"
        )
    }

    fn load_manifest(raw: &str) -> Result<ResolvedTask> {
        let temp = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: temp.path().canonicalize().unwrap(),
        };
        let path = repo
            .root
            .join("20260918-120000-demo")
            .join(TASK_MANIFEST_NAME);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, raw).unwrap();
        ResolvedTask::load(&repo, &Config::default(), &path)
    }

    const APPROVAL_TABLE: &str = "\n[cloud_usage_approval]\nlimit_bytes = 2147483648\nnote = \"The user approved 2 GiB for the training checkpoints\"\n";

    #[test]
    fn schema_3_manifests_carry_the_cloud_usage_approval() {
        let raw = deliverable_manifest(3, APPROVAL_TABLE);
        let task = load_manifest(&raw).unwrap();
        let approval = CloudUsageApproval {
            limit_bytes: 2_147_483_648,
            note: "The user approved 2 GiB for the training checkpoints".to_owned(),
        };
        assert_eq!(task.cloud_usage_approval, Some(approval));
        // Rendering keeps the documented layout and the lowest schema.
        assert_eq!(task.manifest().render().unwrap(), raw);

        let mut reset = task.manifest();
        reset.cloud_usage_approval = None;
        assert_eq!(reset.minimal_schema_version(), TASK_SCHEMA_VERSION);
        assert_eq!(reset.render().unwrap(), deliverable_manifest(2, ""));

        // Schema 3 without the table is still readable.
        assert_eq!(
            load_manifest(&deliverable_manifest(3, ""))
                .unwrap()
                .cloud_usage_approval,
            None
        );
    }

    #[test]
    fn older_schemas_and_malformed_approvals_are_rejected() {
        let error = load_manifest(&deliverable_manifest(2, APPROVAL_TABLE))
            .unwrap_err()
            .to_string();
        assert!(
            error
                .contains("task schema 2 must not declare the schema 3 cloud_usage_approval table"),
            "{error}"
        );
        for table in [
            "\n[cloud_usage_approval]\nlimit_bytes = 2147483648\n",
            "\n[cloud_usage_approval]\nnote = \"approved\"\n",
            "\n[cloud_usage_approval]\nlimit_bytes = -1\nnote = \"approved\"\n",
            "\n[cloud_usage_approval]\nlimit_bytes = 2147483648\nnote = \"approved\"\nrecorded_at = \"2026-09-18T12:00:00Z\"\n",
            "\n[cloud_usage_approval]\nlimit_bytes = 2147483648\nnote = \" \"\n",
        ] {
            assert!(
                load_manifest(&deliverable_manifest(3, table)).is_err(),
                "{table:?} was accepted"
            );
        }
        let error = load_manifest(&deliverable_manifest(4, ""))
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsupported task schema 4"), "{error}");
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
