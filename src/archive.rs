use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::archive_migration::{self, RECEIPT_NAME};
use crate::config::{Config, require_supported_cli_at};
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::lock::RepositoryLock;
use crate::manifest::{ResolvedTask, TaskKind, TaskManifest, build_task_path, parse_task_identity};
use crate::path::{allowed, reject_symlink_traversal, repo_path, resolved_under};
use crate::policy::TASK_MANIFEST_NAME;
use crate::process;
use crate::storage_metadata;
use crate::task_rename::validate_checkout;

#[derive(Debug, Clone)]
pub struct ArchiveOptions {
    pub start: PathBuf,
    pub manifest: Option<PathBuf>,
    pub paths: Vec<String>,
    pub layout: String,
    pub dry_run: bool,
}

#[derive(Debug, Serialize)]
pub struct ArchiveReport {
    pub status: &'static str,
    pub operation: &'static str,
    pub layout: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub base_oid: String,
    pub tasks: Vec<ArchiveTask>,
    pub skipped: Vec<SkippedTask>,
    pub required_scopes: Vec<String>,
    pub remote_writes: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<crate::relocation::RelocationNotice>,
}

#[derive(Debug, Serialize)]
pub struct ArchiveTask {
    pub task_id: String,
    pub branch: String,
    pub source: String,
    pub destination: String,
    pub pull_request: Option<ArchivePullRequest>,
    pub receipt: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct SkippedTask {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MergedPullRequest {
    pub number: u64,
    pub url: String,
    pub merged_at: String,
    pub merge_commit: String,
    pub head_commit: String,
}

/// Live state of a pull request associated with the current task configuration.
/// A closed, unmerged request does not have merge evidence.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArchivePullRequest {
    pub number: u64,
    pub url: String,
    pub state: String,
    pub head_commit: String,
    pub merged_at: Option<String>,
    pub merge_commit: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HostingPullRequest {
    number: u64,
    url: String,
    state: String,
    merged_at: Option<String>,
    merge_commit: Option<HostingCommit>,
    head_ref_name: String,
    head_ref_oid: String,
    is_cross_repository: bool,
}

#[derive(Debug, Deserialize)]
struct HostingCommit {
    oid: String,
}

/// Only a live open review keeps a task pending. A successful lookup with no
/// matching review is done, without inventing a historical review association.
enum TaskReviewState {
    Pending,
    Done(Option<ArchivePullRequest>),
}

struct PreparedTask {
    report: ArchiveTask,
    original_manifest: String,
    next_manifest: String,
    original_receipt: Option<String>,
    relocation: crate::relocation::RelocationPlan,
}

pub fn archive(options: &ArchiveOptions) -> Result<ArchiveReport> {
    let repo = match &options.manifest {
        Some(path) => GitRepo::discover_for_manifest(path)?,
        None => GitRepo::discover(&options.start)?,
    };
    let _lock = RepositoryLock::acquire(&repo)?;
    let config = Config::load_compatible(&repo)?;
    let installed = crate::config::installed_cli_version();
    let required = crate::policy::ARCHIVE_STORAGE_PROTOCOL_MINIMUM_CLI_VERSION;
    if !crate::config::cli_version_satisfies(&installed, &required) {
        return Err(Error::message(format!(
            "archive protocol requires workspace-mgr {required} or newer; this is workspace-mgr {installed}; update the CLI before preparing an archive"
        )));
    }
    repo.validate_remote_name(&config.git.remote)?;
    repo.validate_branch(&config.git.branch)?;
    // Validate the template before inspecting tasks or contacting a host.
    grouping(&options.layout, "20260918-120000")?;
    let owner = infrastructure_task(&repo, &config, options)?;
    let base_oid = repo.fetch_branch(&config.git.remote, &config.git.branch)?;
    require_supported_cli_at(
        &repo,
        &base_oid,
        &format!("{}/{}", config.git.remote, config.git.branch),
    )?;
    let sources = task_sources(&repo, &options.paths)?;
    let mut prepared = Vec::new();
    let mut skipped = Vec::new();
    let mut destinations = BTreeSet::new();
    let mut required_scopes = BTreeSet::new();
    let host = if sources.is_empty() {
        None
    } else {
        Some(hosting_repository(&repo, &config.git.remote)?)
    };
    for source in &sources {
        reject_symlink_traversal(&repo.root, source, "archive source")?;
        let source_dir = resolved_under(&repo.root, source);
        let metadata = fs::symlink_metadata(&source_dir).at(&source_dir)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(Error::message(format!(
                "archive source must be an ordinary task directory: {source}"
            )));
        }
        let manifest_path = source_dir.join(TASK_MANIFEST_NAME);
        if !manifest_path.exists() {
            let reason = "legacy task has no manifest; explicitly adopt it with task adopt before archiving; no pull request is required";
            if !options.paths.is_empty() {
                return Err(Error::message(format!(
                    "archive refuses {source}: {reason}"
                )));
            }
            skipped.push(SkippedTask {
                path: source.clone(),
                reason: reason.to_owned(),
            });
            continue;
        }
        reject_symlink_traversal(
            &repo.root,
            &format!("{source}/{TASK_MANIFEST_NAME}"),
            "archive task manifest",
        )?;
        let task = ResolvedTask::load(&repo, &config, &manifest_path)?;
        if task.kind != TaskKind::Deliverable || task.task_path.as_deref() != Some(source) {
            return Err(Error::message(format!(
                "archive source must be a deliverable task's declared directory: {source}"
            )));
        }
        let review_state = task_review_state(
            &repo,
            host.as_deref().expect("a source has a hosting repository"),
            &task,
        )?;
        let TaskReviewState::Done(completion) = review_state else {
            if !options.paths.is_empty() {
                return Err(Error::message(format!(
                    "archive refuses pending task {source}; its associated pull request is open"
                )));
            }
            skipped.push(SkippedTask {
                path: source.clone(),
                reason: "task has an open pull request (pending)".to_owned(),
            });
            continue;
        };
        let identity = parse_task_identity(task.kind, &task.task_id)?;
        let date = identity
            .timestamp
            .as_deref()
            .expect("deliverable timestamp");
        let destination = format!(
            "{}/{}",
            grouping(&options.layout, date)?,
            build_task_path(&identity, &task.slug)
        );
        if destination == *source {
            skipped.push(SkippedTask {
                path: source.clone(),
                reason: "already uses the requested date grouping".to_owned(),
            });
            continue;
        }
        if !destinations.insert(destination.clone()) {
            return Err(Error::message(format!(
                "archive tasks collide at destination {destination}"
            )));
        }
        reject_overlapping_moves(&sources, &destination)?;
        validate_destination(&repo, &base_oid, &destination)?;
        required_scopes.insert(source.clone());
        required_scopes.insert(destination.clone());
        if !options.dry_run {
            let scopes = owner
                .as_ref()
                .expect("apply requires infrastructure")
                .scopes();
            for path in [source, &destination] {
                if !allowed(path, &scopes) {
                    return Err(Error::message(format!(
                        "archive path {path:?} escapes the infrastructure task's declared scopes; inspect archive --dry-run and declare both source and destination first"
                    )));
                }
            }
        }
        crate::nested_git::validate_move(&repo, source, &destination)?;
        validate_materialized(&repo, &config, source)?;
        let relocation = crate::relocation::RelocationPlan::opaque(
            &source_dir,
            &resolved_under(&repo.root, &destination),
        )?;
        let original_manifest = fs::read_to_string(&manifest_path).at(&manifest_path)?;
        let mut next: TaskManifest =
            toml::from_str(&original_manifest).map_err(|source| Error::Toml {
                path: manifest_path.clone(),
                source,
            })?;
        next.path = Some(destination.clone());
        let next_manifest = toml::to_string_pretty(&next).map_err(|error| {
            Error::message(format!("failed to render archived task manifest: {error}"))
        })?;
        let receipt_path = source_dir.join(RECEIPT_NAME);
        reject_symlink_traversal(
            &repo.root,
            &format!("{source}/{RECEIPT_NAME}"),
            "archive receipt",
        )?;
        let original_receipt = match fs::read_to_string(&receipt_path) {
            Ok(raw) => Some(raw),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(Error::Io {
                    path: receipt_path,
                    source,
                });
            }
        };
        let previous_receipt = original_receipt
            .as_deref()
            .map(serde_json::from_str::<serde_json::Value>)
            .transpose()
            .map_err(|error| {
                Error::message(format!("invalid archive receipt in {source}: {error}"))
            })?;
        if previous_receipt
            .as_ref()
            .is_some_and(|receipt| receipt["status"] == "planned")
        {
            return Err(Error::message(format!(
                "archive task {source} has an unpublished migration; publish it before reorganizing it again"
            )));
        }
        let mut receipt = archive_migration::plan(&repo, &config, source, &destination)?;
        let object = receipt
            .as_object_mut()
            .ok_or_else(|| Error::message("archive migration plan must be an object"))?;
        object.insert("task_id".to_owned(), task.task_id.clone().into());
        object.insert("status".to_owned(), "planned".into());
        object.insert(
            "closed_pull_request".to_owned(),
            serde_json::to_value(&completion).map_err(|error| {
                Error::message(format!(
                    "failed to render archive pull request state: {error}"
                ))
            })?,
        );
        if let Some(previous) = previous_receipt {
            object.insert("previous_receipt".to_owned(), previous);
        }
        prepared.push(PreparedTask {
            report: ArchiveTask {
                task_id: task.task_id,
                branch: task.branch,
                source: source.clone(),
                destination,
                pull_request: completion,
                receipt,
            },
            original_manifest,
            next_manifest,
            original_receipt,
            relocation,
        });
    }
    if !options.dry_run {
        apply(&repo, &config, &prepared, owner.as_ref())?;
    }
    let notices = if !options.dry_run && !prepared.is_empty() {
        vec![crate::relocation::RelocationNotice::archived_directory()]
    } else {
        Vec::new()
    };
    Ok(ArchiveReport {
        status: if prepared.is_empty() {
            "no_changes"
        } else if options.dry_run {
            "dry_run"
        } else {
            "archived"
        },
        operation: "archive",
        layout: options.layout.clone(),
        task_id: owner.map(|task| task.task_id),
        base_oid,
        tasks: prepared.into_iter().map(|task| task.report).collect(),
        skipped,
        required_scopes: required_scopes.into_iter().collect(),
        remote_writes: false,
        notices,
    })
}

pub(crate) fn infrastructure_task(
    repo: &GitRepo,
    config: &Config,
    options: &ArchiveOptions,
) -> Result<Option<ResolvedTask>> {
    let path = options.manifest.as_ref().cloned();
    if let Some(path) = path {
        let task = ResolvedTask::load(repo, config, &path)?;
        if task.kind != TaskKind::Infrastructure {
            return Err(Error::message(
                "archive must use a repository-infrastructure task, not a deliverable task",
            ));
        }
        validate_checkout(repo, &task, "archive")?;
        crate::cloud_usage::remind(repo, &task);
        return Ok(Some(task));
    }
    if !options.dry_run {
        return Err(Error::message(
            "archive apply requires a managed repository-infrastructure task; run archive --dry-run to inspect its required source and destination scopes first",
        ));
    }
    if repo.current_branch()?.as_deref() != Some(&config.git.branch) {
        return Err(Error::message(
            "archive --dry-run outside an infrastructure task must run from the shared checkout on its configured base branch",
        ));
    }
    Ok(None)
}

fn task_sources(repo: &GitRepo, selected: &[String]) -> Result<Vec<String>> {
    let mut sources = BTreeSet::new();
    if !selected.is_empty() {
        for path in selected {
            sources.insert(repo_path(path, "archive task path")?);
        }
    } else {
        for entry in fs::read_dir(&repo.root).at(&repo.root)? {
            let entry = entry.at(&repo.root)?;
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| Error::message("archive task directory name is not UTF-8"))?;
            if entry.path().join(TASK_MANIFEST_NAME).is_file()
                || (entry.file_type().at(entry.path())?.is_dir()
                    && parse_task_identity(TaskKind::Deliverable, name).is_ok())
            {
                let name = entry
                    .file_name()
                    .to_str()
                    .ok_or_else(|| Error::message("archive task directory name is not UTF-8"))?
                    .to_owned();
                sources.insert(repo_path(&name, "archive task path")?);
            }
        }
    }
    Ok(sources.into_iter().collect())
}

fn grouping(layout: &str, timestamp: &str) -> Result<String> {
    let rendered = layout
        .replace("{year}", &timestamp[..4])
        .replace("{month}", &timestamp[4..6]);
    if rendered.contains('{') || rendered.contains('}') {
        return Err(Error::message(
            "archive --layout accepts only {year} and {month} placeholders",
        ));
    }
    if !layout.contains("{year}") {
        return Err(Error::message(
            "archive --layout must include {year} to group tasks by time",
        ));
    }
    let path = repo_path(&rendered, "archive date grouping")?;
    if path.split('/').any(|part| part.starts_with('.')) {
        return Err(Error::message(
            "archive date grouping may not use hidden repository-control directories",
        ));
    }
    Ok(path)
}

fn hosting_command() -> String {
    #[cfg(feature = "test-storage")]
    if let Ok(path) = std::env::var("WORKSPACE_MGR_TEST_GH") {
        return path;
    }
    "gh".to_owned()
}

pub(crate) fn hosting_repository(repo: &GitRepo, remote: &str) -> Result<String> {
    #[cfg(feature = "test-storage")]
    if std::env::var_os("WORKSPACE_MGR_TEST_GH").is_some() {
        return Ok("example.invalid/owner/archive-fixture".to_owned());
    }
    let url = repo.run(["remote", "get-url", remote])?.stdout;
    let raw = url.trim();
    let (host, path) = if let Some(with_scheme) = raw.split_once("://").map(|(_, rest)| rest) {
        let (authority, path) = with_scheme.split_once('/').ok_or_else(|| {
            Error::message("archive requires a GitHub repository remote that gh can query")
        })?;
        (authority.rsplit('@').next().unwrap_or(authority), path)
    } else if let Some((authority, path)) = raw.split_once(':') {
        (authority.rsplit('@').next().unwrap_or(authority), path)
    } else {
        return Err(Error::message(
            "archive requires a GitHub repository remote that gh can query",
        ));
    };
    let path = path
        .trim_end_matches('/')
        .strip_suffix(".git")
        .unwrap_or(path.trim_end_matches('/'));
    let pieces: Vec<_> = path.split('/').collect();
    if pieces.len() != 2
        || pieces.iter().any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        })
        || host.is_empty()
        || !host
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-.".contains(&c))
    {
        return Err(Error::message(
            "archive requires a GitHub repository remote that gh can query",
        ));
    }
    Ok(format!("{host}/{path}"))
}

/// Archive eligibility depends on current metadata and live PR state. Saved
/// branch names are lookup hints, never requirements for historical matches.
fn task_review_state(repo: &GitRepo, host: &str, task: &ResolvedTask) -> Result<TaskReviewState> {
    let mut branches = BTreeSet::from([task.branch.clone()]);
    if let Some(record) = &task.archive_completion {
        branches.extend(record.branches.iter().cloned());
        branches.extend(record.reviews.iter().map(|review| review.branch.clone()));
    }
    // Existing adoption metadata can name another associated branch. Check
    // its live state alongside the current branch, even when that branch has
    // a closed PR. No historical branch or payload needs to be reconstructed.
    let source = task.task_path.as_deref().expect("deliverable path");
    let path = resolved_under(&repo.root, source).join(crate::archive_adoption::LEGACY_RECORD);
    if fs::symlink_metadata(&path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() <= 16 * 1024 * 1024)
    {
        let hint = fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
        if let Some(branch) = hint
            .as_ref()
            .filter(|hint| hint["task_id"].as_str() == Some(task.task_id.as_str()))
            .and_then(|hint| hint["branch"].as_str())
            .filter(|branch| repo.validate_branch(branch).is_ok())
        {
            branches.insert(branch.to_owned());
        }
    }
    let mut closed = Vec::new();
    for branch in branches {
        // Query open state separately: a large closed history must not hide
        // an open review. Failed hosting queries propagate as errors, rather
        // than being mistaken for a successful empty result.
        if current_branch_requests(repo, host, &branch, "open")?
            .iter()
            .any(|request| request.state == "OPEN")
        {
            return Ok(TaskReviewState::Pending);
        }
        for request in current_branch_requests(repo, host, &branch, "all")? {
            if request.state == "OPEN" {
                return Ok(TaskReviewState::Pending);
            }
            closed.push(request);
        }
    }
    closed.sort_by_key(|request| request.number);
    Ok(TaskReviewState::Done(closed.pop().map(|request| {
        ArchivePullRequest {
            number: request.number,
            url: request.url,
            state: request.state,
            head_commit: request.head_ref_oid,
            merged_at: request.merged_at,
            merge_commit: request.merge_commit.map(|commit| commit.oid),
        }
    })))
}

fn current_branch_requests(
    repo: &GitRepo,
    host: &str,
    branch: &str,
    state: &str,
) -> Result<Vec<HostingPullRequest>> {
    let args = vec![
        "pr",
        "list",
        "--repo",
        host,
        "--head",
        branch,
        "--state",
        state,
        "--limit",
        "100",
        "--json",
        "number,url,state,mergedAt,mergeCommit,headRefName,headRefOid,baseRefName,isCrossRepository",
    ];
    let output = process::run(&hosting_command(), args, &repo.root)?;
    let requests: Vec<HostingPullRequest> =
        serde_json::from_str(&output.stdout).map_err(|error| {
            Error::message(format!(
                "archive current pull request lookup returned invalid JSON: {error}"
            ))
        })?;
    if state == "open" && requests.len() >= 100 {
        return Err(Error::message(
            "archive open pull request lookup reached its limit; cannot confirm this task has no open pull request",
        ));
    }
    Ok(requests
        .into_iter()
        .filter(|request| request.head_ref_name == branch && !request.is_cross_repository)
        .collect())
}

pub(crate) fn ensure_review_head(
    repo: &GitRepo,
    remote: &str,
    proof: &MergedPullRequest,
) -> Result<()> {
    if repo
        .run_unchecked([
            "cat-file",
            "-e",
            &format!("{}^{{commit}}", proof.head_commit),
        ])?
        .success()
    {
        return Ok(());
    }
    let reference = format!("refs/pull/{}/head", proof.number);
    let observed = repo
        .run(["ls-remote", "--refs", remote, &reference])?
        .stdout;
    let rows = observed
        .lines()
        .filter(|row| !row.is_empty())
        .collect::<Vec<_>>();
    if rows.len() != 1
        || rows[0].split_once('\t') != Some((proof.head_commit.as_str(), reference.as_str()))
    {
        return Err(Error::message(
            "archive cannot fetch the immutable reviewed PR head; its provider ref is absent or changed",
        ));
    }
    repo.run([
        "fetch",
        "--quiet",
        "--no-tags",
        "--no-write-fetch-head",
        "--refmap=",
        remote,
        &reference,
    ])?;
    if !repo
        .run_unchecked([
            "cat-file",
            "-e",
            &format!("{}^{{commit}}", proof.head_commit),
        ])?
        .success()
    {
        return Err(Error::message(
            "archive immutable review head changed while it was fetched; retry verification",
        ));
    }
    Ok(())
}

pub(crate) fn ancestor(repo: &GitRepo, commit: &str, descendant: &str) -> Result<bool> {
    let output = repo.run_unchecked(["merge-base", "--is-ancestor", commit, descendant])?;
    match output.code {
        0 => Ok(true),
        1 => Ok(false),
        _ => Err(Error::message(
            "archive cannot verify commit ancestry; fetch the immutable review history before archiving",
        )),
    }
}

fn reject_overlapping_moves(sources: &[String], destination: &str) -> Result<()> {
    if sources.iter().any(|source| {
        destination == source
            || destination.starts_with(&format!("{source}/"))
            || source.starts_with(&format!("{destination}/"))
    }) {
        return Err(Error::message(format!(
            "archive destination overlaps a selected source: {destination}"
        )));
    }
    Ok(())
}

fn validate_destination(repo: &GitRepo, base: &str, destination: &str) -> Result<()> {
    reject_symlink_traversal(&repo.root, destination, "archive destination")?;
    let path = resolved_under(&repo.root, destination);
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            return Err(Error::message(format!(
                "archive destination already exists: {destination}"
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(Error::Io { path, source }),
    }
    if repo
        .run_unchecked(["cat-file", "-e", &format!("{base}:{destination}")])?
        .success()
    {
        return Err(Error::message(format!(
            "archive destination already exists on the fetched shared branch: {destination}"
        )));
    }
    Ok(())
}

fn validate_materialized(repo: &GitRepo, config: &Config, source: &str) -> Result<()> {
    let scopes = vec![source.to_owned()];
    let pointers = storage_metadata::discover(repo, &scopes)?;
    for pointer in &pointers {
        let raw = fs::read_to_string(resolved_under(&repo.root, pointer)).at(pointer)?;
        let output = storage_metadata::metadata_output(repo, pointer, &raw)?;
        if resolved_under(&repo.root, &output).exists()
            && !storage_metadata::payload_matches_metadata(repo, pointer, &raw)?
        {
            return Err(Error::message(format!(
                "archive refuses locally changed materialized S3 output {output} at {}; publish or preserve it first",
                repo.root.display()
            )));
        }
    }
    // Missing local payloads are allowed. Their metadata still needs a
    // supported digest and complete directory manifest, and exact remote
    // versions must exist before moving any local directory.
    for entry in crate::native_engine::metadata_entries(repo, None, &pointers)? {
        crate::native_versions::validate_digest(&entry)?;
    }
    if config.s3_enabled() && !pointers.is_empty() {
        crate::native_versions::read(repo, &pointers, "--verify", &[])?;
    }
    Ok(())
}

fn apply(
    repo: &GitRepo,
    config: &Config,
    tasks: &[PreparedTask],
    owner: Option<&ResolvedTask>,
) -> Result<()> {
    let mut moved = Vec::new();
    let mut created = Vec::new();
    let mut recorded = Vec::new();
    let result = (|| {
        for (index, task) in tasks.iter().enumerate() {
            let old = resolved_under(&repo.root, &task.report.source);
            let new = resolved_under(&repo.root, &task.report.destination);
            if let Some(owner) = owner {
                crate::archive_cancel::record_attempt(
                    repo,
                    owner,
                    &task.report.source,
                    &task.report.destination,
                    &task.relocation,
                    &task.report.receipt,
                )?;
                recorded.push(index);
            }
            create_parents(&repo.root, new.parent().expect("task parent"), &mut created)?;
            fs::rename(&old, &new).at(&old)?;
            moved.push(index);
            task.relocation.apply()?;
            atomic_write(&new.join(TASK_MANIFEST_NAME), &task.next_manifest)?;
            let receipt = serde_json::to_string_pretty(&task.report.receipt).map_err(|error| {
                Error::message(format!("failed to render archive receipt: {error}"))
            })? + "\n";
            atomic_write(&new.join(RECEIPT_NAME), &receipt)?;
            ResolvedTask::load(repo, config, &new.join(TASK_MANIFEST_NAME))?;
            if owner.is_some() {
                crate::archive_cancel::moved(repo, &task.report.source, &task.report.destination)?;
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        let mut failures = Vec::new();
        for index in moved.into_iter().rev() {
            let task = &tasks[index];
            let new = resolved_under(&repo.root, &task.report.destination);
            let restored = (|| {
                task.relocation.restore()?;
                atomic_write(&new.join(TASK_MANIFEST_NAME), &task.original_manifest)?;
                match &task.original_receipt {
                    Some(raw) => atomic_write(&new.join(RECEIPT_NAME), raw)?,
                    None => match fs::remove_file(new.join(RECEIPT_NAME)) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(source) => {
                            return Err(Error::Io {
                                path: new.join(RECEIPT_NAME),
                                source,
                            });
                        }
                    },
                }
                fs::rename(&new, resolved_under(&repo.root, &task.report.source)).at(&new)
            })();
            if let Err(failure) = restored {
                failures.push(failure.to_string());
            }
        }
        if failures.is_empty() {
            for index in recorded {
                let task = &tasks[index];
                if let Err(error) = crate::archive_cancel::rolled_back(
                    repo,
                    &task.report.source,
                    &task.report.destination,
                ) {
                    failures.push(error.to_string());
                }
            }
        }
        for path in created.into_iter().rev() {
            match fs::remove_dir(&path) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                    ) => {}
                Err(failure) => failures.push(format!("{}: {failure}", path.display())),
            }
        }
        if failures.is_empty() {
            return Err(error);
        }
        return Err(Error::message(format!(
            "archive failed: {error}; rollback also failed: {}",
            failures.join("; ")
        )));
    }
    Ok(())
}

fn create_parents(root: &Path, parent: &Path, created: &mut Vec<PathBuf>) -> Result<()> {
    let mut missing = Vec::new();
    let mut current = parent;
    while current != root && !current.exists() {
        missing.push(current.to_path_buf());
        current = current
            .parent()
            .ok_or_else(|| Error::message("archive parent escaped the repository"))?;
    }
    for path in missing.into_iter().rev() {
        fs::create_dir(&path).at(&path)?;
        created.push(path);
    }
    Ok(())
}

fn atomic_write(path: &Path, text: &str) -> Result<()> {
    let parent = path.parent().expect("metadata parent");
    let permissions = fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());
    let mut file = tempfile::NamedTempFile::new_in(parent).at(path)?;
    file.write_all(text.as_bytes()).at(path)?;
    file.flush().at(path)?;
    if let Some(permissions) = permissions {
        file.as_file().set_permissions(permissions).at(path)?;
    }
    file.as_file().sync_all().at(path)?;
    file.persist(path).map_err(|error| Error::Io {
        path: path.to_path_buf(),
        source: error.error,
    })?;
    fs::File::open(parent).at(parent)?.sync_all().at(parent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_archive_restores_all_directories_and_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: temp.path().canonicalize().unwrap(),
        };
        let mut prepared = Vec::new();
        for (index, name) in ["first", "second"].into_iter().enumerate() {
            let source = format!("20260712-120000-{name}");
            let destination = format!("2026/07/{source}");
            let directory = repo.root.join(&source);
            fs::create_dir(&directory).unwrap();
            let original = format!(
                "schema_version = 2\nkind = \"deliverable\"\nid = \"{source}\"\nslug = \"{name}\"\npath = \"{source}\"\nbranch = \"codex/{name}\"\ntitle = \"{name}\"\npurpose = \"Keep the task\"\nadditional_scopes = []\n"
            );
            fs::write(directory.join(TASK_MANIFEST_NAME), &original).unwrap();
            fs::write(directory.join("model.bin.dvc"), "original metadata\n").unwrap();
            let next_path = if index == 0 {
                destination.clone()
            } else {
                "2026/07/wrong-task-name".to_owned()
            };
            let next_manifest = original.replace(
                &format!("path = \"{source}\""),
                &format!("path = \"{next_path}\""),
            );
            prepared.push(PreparedTask {
                report: ArchiveTask {
                    task_id: source.clone(),
                    branch: format!("codex/{name}"),
                    source,
                    destination: destination.clone(),
                    pull_request: Some(ArchivePullRequest {
                        number: index as u64 + 1,
                        url: "https://example.invalid/pull/1".to_owned(),
                        state: "MERGED".to_owned(),
                        merged_at: Some("2026-07-12T20:00:00Z".to_owned()),
                        merge_commit: Some("a".repeat(40)),
                        head_commit: "b".repeat(40),
                    }),
                    receipt: serde_json::json!({"status": "planned"}),
                },
                original_manifest: original,
                next_manifest,
                original_receipt: None,
                relocation: crate::relocation::RelocationPlan::opaque(
                    &directory,
                    &repo.root.join(&destination),
                )
                .unwrap(),
            });
        }
        let error = apply(&repo, &Config::default(), &prepared, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("task directory must be"), "{error}");
        for task in prepared {
            let source = repo.root.join(&task.report.source);
            assert_eq!(
                fs::read_to_string(source.join(TASK_MANIFEST_NAME)).unwrap(),
                task.original_manifest
            );
            assert_eq!(
                fs::read_to_string(source.join("model.bin.dvc")).unwrap(),
                "original metadata\n"
            );
            assert!(!source.join(RECEIPT_NAME).exists());
            assert!(!repo.root.join(&task.report.destination).exists());
        }
        assert!(!repo.root.join("2026").exists());
    }

    #[test]
    fn archive_layouts_are_relative_and_time_based() {
        assert_eq!(
            grouping("{year}/{month}", "20260712-120000").unwrap(),
            "2026/07"
        );
        assert_eq!(
            grouping("{year}{month}", "20260712-120000").unwrap(),
            "202607"
        );
        for layout in [
            "../{year}",
            "/{year}",
            "{month}",
            "{year}/{unknown}",
            ".dvc/{year}",
        ] {
            assert!(grouping(layout, "20260712-120000").is_err());
        }
    }
}
