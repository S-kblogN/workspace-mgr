use std::fs;
use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::path::{reject_symlink_traversal, repo_path};
use crate::policy::{TASK_BRANCH_PREFIX, TASK_MANIFEST_NAME};

pub const INFRASTRUCTURE_MANIFEST_NAME: &str = "workspace-mgr/task.toml";
pub const INFRASTRUCTURE_TASK_MANIFEST_FILE: &str = ".workspace-mgr-infrastructure.toml";
pub const TASK_SCHEMA_VERSION: u32 = 2;
/// Schema 2 plus the optional `[cloud_usage_approval]` table.
pub const CLOUD_USAGE_APPROVAL_TASK_SCHEMA_VERSION: u32 = 3;
/// Schema 3 plus durable, verified archive completion evidence.
pub const ARCHIVE_COMPLETION_TASK_SCHEMA_VERSION: u32 = 4;
const LEGACY_TASK_SCHEMA_VERSION: u32 = 1;

/// Infrastructure tasks share the main checkout. Their explicit manifests
/// live in per-task shared local state, allowing concurrent tasks without an
/// implicit active-task selector or a branch-mounted workplace.
pub fn infrastructure_manifest_path(repo: &GitRepo, task_id: &str) -> Result<PathBuf> {
    parse_task_identity(TaskKind::Infrastructure, task_id)?;
    let local_state_dir = repo.local_state_dir()?;
    let relative = format!("infrastructure-tasks/{task_id}/{INFRASTRUCTURE_TASK_MANIFEST_FILE}");
    reject_symlink_traversal(
        &local_state_dir,
        &relative,
        "private infrastructure manifest",
    )?;
    Ok(local_state_dir.join(relative))
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_completion: Option<ArchiveCompletion>,
}

/// A verified lifecycle checkpoint retained across task configuration upgrades.
/// Archive revalidates its hosting facts and any later task changes before use.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArchiveCompletion {
    pub schema_version: u32,
    pub task_id: String,
    pub repository: String,
    pub base_branch: String,
    pub checkpoint_commit: String,
    pub checkpoint_path: String,
    pub checkpoint_tree: String,
    pub branches: Vec<String>,
    pub reviews: Vec<ArchiveCompletionReview>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArchiveCompletionReview {
    pub branch: String,
    pub number: u64,
    pub url: String,
    pub merged_at: String,
    pub merge_commit: String,
    pub head_commit: String,
}

fn validate_archive_completion(
    repo: &GitRepo,
    kind: TaskKind,
    task_id: &str,
    completion: &ArchiveCompletion,
) -> Result<()> {
    let valid_oid =
        |value: &str| value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit());
    if kind != TaskKind::Deliverable
        || completion.schema_version != 1
        || completion.task_id != task_id
        || !valid_oid(&completion.checkpoint_commit)
        || !valid_oid(&completion.checkpoint_tree)
        || completion.branches.is_empty()
        || completion.reviews.is_empty()
    {
        return Err(Error::message("invalid archive completion checkpoint"));
    }
    one_line(&completion.repository, "archive completion repository")?;
    repo.validate_branch(&completion.base_branch)?;
    repo_path(
        &completion.checkpoint_path,
        "archive completion checkpoint path",
    )?;
    let mut branches = std::collections::BTreeSet::new();
    for branch in &completion.branches {
        repo.validate_branch(branch)?;
        if !branches.insert(branch) {
            return Err(Error::message("duplicate archive completion branch"));
        }
    }
    let mut numbers = std::collections::BTreeSet::new();
    for review in &completion.reviews {
        repo.validate_branch(&review.branch)?;
        one_line(&review.url, "archive completion pull request URL")?;
        if review.number == 0
            || !numbers.insert(review.number)
            || !branches.contains(&review.branch)
            || chrono::DateTime::parse_from_rfc3339(&review.merged_at).is_err()
            || !valid_oid(&review.merge_commit)
            || !valid_oid(&review.head_commit)
        {
            return Err(Error::message("invalid archive completion review"));
        }
    }
    Ok(())
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
    pub archive_completion: Option<ArchiveCompletion>,
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
        if self.archive_completion.is_some() {
            ARCHIVE_COMPLETION_TASK_SCHEMA_VERSION
        } else if self.cloud_usage_approval.is_some() {
            CLOUD_USAGE_APPROVAL_TASK_SCHEMA_VERSION
        } else {
            TASK_SCHEMA_VERSION
        }
    }
}

impl ResolvedTask {
    pub fn load(repo: &GitRepo, config: &Config, path: &Path) -> Result<Self> {
        let absolute = repo.resolve_manifest_path(path)?;
        Self::load_at(repo, config, absolute, None)
    }

    /// Inspect current metadata without migrating any private state. The
    /// catalog supplies a verified current or legacy private-state root.
    pub(crate) fn load_read_only(
        repo: &GitRepo,
        config: &Config,
        path: &Path,
        infrastructure_root: &Path,
    ) -> Result<Self> {
        let absolute = path.canonicalize().at(path)?;
        Self::load_at(repo, config, absolute, Some(infrastructure_root))
    }

    fn load_at(
        repo: &GitRepo,
        config: &Config,
        absolute: PathBuf,
        infrastructure_root: Option<&Path>,
    ) -> Result<Self> {
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
                | ARCHIVE_COMPLETION_TASK_SCHEMA_VERSION
        ) {
            return Err(Error::message(format!(
                "unsupported task schema {}, expected {}, {}, {}, or {}",
                manifest.schema_version,
                LEGACY_TASK_SCHEMA_VERSION,
                TASK_SCHEMA_VERSION,
                CLOUD_USAGE_APPROVAL_TASK_SCHEMA_VERSION,
                ARCHIVE_COMPLETION_TASK_SCHEMA_VERSION
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
            TASK_SCHEMA_VERSION
            | CLOUD_USAGE_APPROVAL_TASK_SCHEMA_VERSION
            | ARCHIVE_COMPLETION_TASK_SCHEMA_VERSION => {
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
                let expected_directory = build_task_path(&identity, &slug);
                if task_path.rsplit('/').next() != Some(expected_directory.as_str()) {
                    return Err(Error::message(format!(
                        "deliverable task directory must be {expected_directory:?} for slug {slug:?}; got {task_path:?}"
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
                let (expected, legacy) = match infrastructure_root {
                    Some(root) => (
                        root.join("infrastructure-tasks")
                            .join(&task_id)
                            .join(INFRASTRUCTURE_TASK_MANIFEST_FILE),
                        root.join("task.toml"),
                    ),
                    None => (
                        infrastructure_manifest_path(repo, &task_id)?,
                        repo.git_dir()?.join(INFRASTRUCTURE_MANIFEST_NAME),
                    ),
                };
                if absolute != expected && legacy.canonicalize().ok().as_ref() != Some(&absolute) {
                    return Err(Error::message(format!(
                        "infrastructure task manifest must be private per-task local state at {}; got {}",
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
        if let Some(completion) = &manifest.archive_completion {
            if manifest.schema_version < ARCHIVE_COMPLETION_TASK_SCHEMA_VERSION {
                return Err(Error::message(format!(
                    "task schema {} must not declare the schema {} archive_completion table",
                    manifest.schema_version, ARCHIVE_COMPLETION_TASK_SCHEMA_VERSION
                )));
            }
            validate_archive_completion(repo, manifest.kind, &task_id, completion)?;
        }
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
            archive_completion: manifest.archive_completion,
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
            archive_completion: self.archive_completion.clone(),
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
        Err(Error::message(format!(
            "no task manifest found from {} to {}; infrastructure tasks require explicit --manifest selection",
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
            archive_completion: None,
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
        load_manifest_at(raw, "20260918-120000-demo")
    }

    fn load_manifest_at(raw: &str, directory: &str) -> Result<ResolvedTask> {
        let temp = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: temp.path().canonicalize().unwrap(),
        };
        let path = repo.root.join(directory).join(TASK_MANIFEST_NAME);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, raw).unwrap();
        ResolvedTask::load(&repo, &Config::default(), &path)
    }

    #[test]
    fn infrastructure_manifests_are_per_task_and_require_explicit_selection() {
        let temp = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: temp.path().canonicalize().unwrap(),
        };
        repo.run(["init", "-q", "-b", "main"]).unwrap();
        let raw = |slug: &str| {
            format!(
                "schema_version = 2\nkind = \"infrastructure\"\nid = \"infra-{slug}\"\nslug = \"{slug}\"\nbranch = \"codex/infra-{slug}\"\ntitle = \"Repository update\"\npurpose = \"Keep shared infrastructure current\"\n\n[[additional_scopes]]\npath = \".github\"\nreason = \"The user requested this shared scope\"\n"
            )
        };
        let first = infrastructure_manifest_path(&repo, "infra-first").unwrap();
        let second = infrastructure_manifest_path(&repo, "infra-second").unwrap();
        for (slug, path) in [("first", &first), ("second", &second)] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, raw(slug)).unwrap();
            let task = ResolvedTask::load(&repo, &Config::default(), path).unwrap();
            assert_eq!(task.task_id, format!("infra-{slug}"));
            assert_eq!(task.task_path, None);
            assert_eq!(task.scopes(), vec![".github"]);
        }
        assert_ne!(first, second);
        fs::write(&second, raw("first")).unwrap();
        let error = ResolvedTask::load(&repo, &Config::default(), &second)
            .unwrap_err()
            .to_string();
        assert!(error.contains("private per-task local state"), "{error}");

        // A singular legacy manifest remains explicitly readable, but never
        // chooses an infrastructure task implicitly from the shared root.
        let legacy = repo.git_dir().unwrap().join(INFRASTRUCTURE_MANIFEST_NAME);
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, raw("first")).unwrap();
        ResolvedTask::load(&repo, &Config::default(), &legacy).unwrap();
        let error = ResolvedTask::discover(&repo, &repo.root)
            .unwrap_err()
            .to_string();
        assert!(error.contains("explicit --manifest"), "{error}");
    }

    const APPROVAL_TABLE: &str = "\n[cloud_usage_approval]\nlimit_bytes = 2147483648\nnote = \"The user approved 2 GiB for the training checkpoints\"\n";

    #[test]
    fn archived_manifest_paths_preserve_directory_identity_and_exact_location() {
        let original = deliverable_manifest(3, APPROVAL_TABLE);
        for grouping in ["2026", "202609", "2026/09"] {
            let directory = format!("{grouping}/20260918-120000-demo");
            let raw = original.replace(
                "path = \"20260918-120000-demo\"",
                &format!("path = \"{directory}\""),
            );
            let task = load_manifest_at(&raw, &directory).unwrap();
            assert_eq!(task.task_path.as_deref(), Some(directory.as_str()));
            assert_eq!(task.task_id, "20260918-120000-demo");
            assert_eq!(task.branch, "codex/demo");
            assert!(task.cloud_usage_approval.is_some());
            assert_eq!(task.manifest().render().unwrap(), raw);

            let error = load_manifest_at(&original, &directory)
                .unwrap_err()
                .to_string();
            assert!(error.contains("manifest must be located at"), "{error}");

            let wrong_name = format!("{grouping}/20260918-120000-other");
            let raw = raw.replace(&directory, &wrong_name);
            let error = load_manifest_at(&raw, &wrong_name).unwrap_err().to_string();
            assert!(
                error.contains("deliverable task directory must be"),
                "{error}"
            );
        }
    }

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
        let error = load_manifest(&deliverable_manifest(5, ""))
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsupported task schema 5"), "{error}");
    }

    fn completion() -> ArchiveCompletion {
        ArchiveCompletion {
            schema_version: 1,
            task_id: "20260918-120000-demo".to_owned(),
            repository: "github.com/example/workspace".to_owned(),
            base_branch: "main".to_owned(),
            checkpoint_commit: "a".repeat(40),
            checkpoint_path: "20260918-120000-demo".to_owned(),
            checkpoint_tree: "b".repeat(40),
            branches: vec!["codex/demo".to_owned(), "historical/demo".to_owned()],
            reviews: vec![ArchiveCompletionReview {
                branch: "historical/demo".to_owned(),
                number: 12,
                url: "https://github.com/example/workspace/pull/12".to_owned(),
                merged_at: "2026-09-18T12:10:00Z".to_owned(),
                merge_commit: "c".repeat(40),
                head_commit: "d".repeat(40),
            }],
        }
    }

    #[test]
    fn completion_survives_manifest_rewrites_and_approval_removal() {
        let task = load_manifest(&deliverable_manifest(3, APPROVAL_TABLE)).unwrap();
        let mut manifest = task.manifest();
        manifest.archive_completion = Some(completion());
        let raw = manifest.render().unwrap();
        assert!(raw.starts_with("schema_version = 4\n"));
        let loaded = load_manifest(&raw).unwrap();
        assert_eq!(loaded.archive_completion, Some(completion()));
        assert_eq!(loaded.manifest().render().unwrap(), raw);
        let mut rewritten = loaded.manifest();
        rewritten.cloud_usage_approval = None;
        assert_eq!(rewritten.minimal_schema_version(), 4);
        let rewritten = load_manifest(&rewritten.render().unwrap()).unwrap();
        assert_eq!(rewritten.archive_completion, Some(completion()));
        assert_eq!(rewritten.cloud_usage_approval, None);
    }

    #[test]
    fn completion_requires_its_schema_and_rejects_invalid_identity_or_reviews() {
        let task = load_manifest(&deliverable_manifest(2, "")).unwrap();
        let mut manifest = task.manifest();
        manifest.archive_completion = Some(completion());
        let raw = manifest.render().unwrap();
        let old_schema = raw.replacen("schema_version = 4", "schema_version = 3", 1);
        assert!(
            load_manifest(&old_schema)
                .unwrap_err()
                .to_string()
                .contains("must not declare the schema 4 archive_completion table")
        );
        for invalid in [
            raw.replace("checkpoint_tree =", "unknown_checkpoint_tree ="),
            raw.replace("number = 12", "number = 0"),
            raw.replace("2026-09-18T12:10:00Z", "yesterday"),
            raw.replace(
                "checkpoint_path = \"20260918-120000-demo\"",
                "checkpoint_path = \"../outside\"",
            ),
            raw.replace("branch = \"historical/demo\"", "branch = \"unknown/demo\""),
        ] {
            assert!(load_manifest(&invalid).is_err(), "accepted {invalid}");
        }
        let mut wrong_id = manifest.clone();
        wrong_id.archive_completion.as_mut().unwrap().task_id = "20260918-120000-other".to_owned();
        assert!(load_manifest(&wrong_id.render().unwrap()).is_err());
        let mut duplicate_review = manifest;
        let checkpoint = duplicate_review.archive_completion.as_mut().unwrap();
        checkpoint.reviews.push(checkpoint.reviews[0].clone());
        assert!(load_manifest(&duplicate_review.render().unwrap()).is_err());
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
