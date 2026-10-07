use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::archive_migration::{self, RECEIPT_NAME};
use crate::config::{Config, require_supported_cli_at};
use crate::dvc;
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::lock::RepositoryLock;
use crate::manifest::{ResolvedTask, TaskKind, TaskManifest, build_task_path, parse_task_identity};
use crate::path::{allowed, reject_symlink_traversal, repo_path, resolved_under};
use crate::policy::TASK_MANIFEST_NAME;
use crate::process;
use crate::storage;
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
}

#[derive(Debug, Serialize)]
pub struct ArchiveTask {
    pub task_id: String,
    pub branch: String,
    pub source: String,
    pub destination: String,
    pub pull_request: MergedPullRequest,
    pub review_history: Vec<MergedPullRequest>,
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
    base_ref_name: String,
    is_cross_repository: bool,
}

#[derive(Debug, Deserialize)]
struct AssociatedPullRequest {
    number: u64,
    html_url: String,
    state: String,
    merged_at: Option<String>,
    merge_commit_sha: Option<String>,
    head: AssociatedRef,
    base: AssociatedRef,
}

#[derive(Debug, Deserialize)]
struct AssociatedRef {
    #[serde(rename = "ref")]
    name: String,
    sha: String,
    repo: Option<AssociatedRepository>,
}

#[derive(Debug, Deserialize)]
struct AssociatedRepository {
    full_name: String,
}

#[derive(Debug)]
struct HistoricalManifest {
    commit: String,
    directory: String,
    manifest: TaskManifest,
    tree: String,
}

struct CompletionProof {
    pull_request: MergedPullRequest,
    reviews: Vec<MergedPullRequest>,
}

#[derive(Debug, Deserialize)]
struct HostingCommit {
    oid: String,
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
            let reason = "legacy task has no manifest; explicitly adopt it with task adopt and a verified merged pull request before archiving";
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
        let eligible = merged_pull_request(
            &repo,
            &config.git.remote,
            host.as_deref().expect("a source has a hosting repository"),
            &task,
            &base_oid,
        )?;
        let Some(completion) = eligible else {
            if !options.paths.is_empty() {
                return Err(Error::message(format!(
                    "archive refuses active or unverified task {source}; its matching pull request must be merged into {:?}",
                    config.git.branch
                )));
            }
            skipped.push(SkippedTask {
                path: source.clone(),
                reason: "no verified merged pull request for this task on the configured base"
                    .to_owned(),
            });
            continue;
        };
        if !tree_manifest_matches(&repo, &base_oid, source, &task)? {
            return Err(Error::message(format!(
                "archive task {source} is absent or has a different identity on the fetched shared branch; refresh or resolve the task state first"
            )));
        }
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
        validate_clean_paths(&repo, &base_oid, source, &destination)?;
        for shared in repo.branch_worktrees(&config.git.branch)? {
            if shared != repo.root {
                let shared_repo = GitRepo { root: shared };
                validate_overlays(&shared_repo, source, &destination)?;
                validate_materialized(&shared_repo, source)?;
            }
        }
        validate_materialized(&repo, source)?;
        let relocation =
            crate::relocation::prepare(&source_dir, &resolved_under(&repo.root, &destination))?;
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
            "completion_reviews".to_owned(),
            serde_json::to_value(&completion.reviews).map_err(|error| {
                Error::message(format!(
                    "failed to render archive completion evidence: {error}"
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
                pull_request: completion.pull_request,
                review_history: completion.reviews,
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

fn merged_pull_request(
    repo: &GitRepo,
    remote: &str,
    host: &str,
    task: &ResolvedTask,
    base: &str,
) -> Result<Option<CompletionProof>> {
    if repo
        .run(["rev-parse", "--is-shallow-repository"])?
        .stdout
        .trim()
        == "true"
    {
        return Err(Error::message(
            "archive requires complete Git history to verify historical identities and reviews; fetch the repository with --unshallow before archiving",
        ));
    }
    let history = manifest_history(repo, base, task)?;
    if history.is_empty() {
        return Ok(None);
    }
    // The stable ID connects historical schema/path/branch migrations. A
    // current branch name alone cannot identify the original review.
    let mut branches = history
        .iter()
        .map(|item| item.manifest.branch.clone())
        .collect::<BTreeSet<_>>();
    branches.insert(task.branch.clone());
    let oldest = history.last().expect("a nonempty identity history");
    let legacy = crate::archive_adoption::verify_record(
        repo,
        &Config::load_compatible(repo)?,
        host,
        base,
        task,
        (&oldest.commit, &oldest.directory),
    )?;
    if let Some((branch, _)) = &legacy {
        branches.insert(branch.clone());
    }
    let mut requests = Vec::new();
    for branch in &branches {
        let rows = branch_pull_requests(repo, host, branch)?;
        if rows.iter().any(|pr| pr.state == "OPEN") {
            return Ok(None);
        }
        requests.extend(rows);
    }
    let mut reviewed = Vec::new();
    for (index, snapshot) in history.iter().enumerate() {
        // Most task creation commits are the recorded PR merge itself. Other
        // identity changes require the hosting provider's commit-to-PR
        // association, so an unrelated later review cannot bless a direct edit.
        let mut matching = requests
            .iter()
            .filter(|pr| {
                pr.merge_commit
                    .as_ref()
                    .is_some_and(|merge| merge.oid == snapshot.commit)
            })
            .collect::<Vec<_>>();
        let associated;
        if matching.is_empty() {
            associated = associated_pull_requests(repo, host, &snapshot.commit)?;
            if associated.iter().any(|pr| pr.state == "OPEN") {
                return Ok(None);
            }
            matching = associated.iter().collect();
        }
        let mut proofs = Vec::new();
        for request in matching {
            let Some(proof) = verified_review(repo, task, base, request)? else {
                continue;
            };
            let merged = historical_manifest(repo, &proof.merge_commit, task)?;
            if merged.as_ref().is_none_or(|merged| {
                (index == 0 || proof.merge_commit == snapshot.commit)
                    && merged.tree != snapshot.tree
            }) {
                continue;
            }
            // Squash commits are the merge itself. A commit associated with a
            // regular/fast-forward PR must be part of its reviewed head.
            if snapshot.commit != proof.merge_commit {
                ensure_review_head(repo, remote, &proof)?;
            }
            if snapshot.commit != proof.merge_commit
                && !ancestor(repo, &snapshot.commit, &proof.head_commit)?
            {
                continue;
            }
            branches.insert(request.head_ref_name.clone());
            proofs.push(proof);
        }
        proofs.sort_by_key(|proof| proof.number);
        proofs.dedup_by_key(|proof| proof.number);
        if proofs.len() != 1 {
            return Ok(None);
        }
        reviewed.push((snapshot.manifest.branch.clone(), proofs.remove(0)));
    }
    if let Some(legacy) = legacy {
        reviewed.push(legacy);
    }
    for branch in &branches {
        // A migration's own review branch is part of the verified lifecycle,
        // even when the task's manifest retains a different canonical branch.
        if !requests
            .iter()
            .any(|request| &request.head_ref_name == branch)
        {
            let rows = branch_pull_requests(repo, host, branch)?;
            if rows.iter().any(|request| request.state == "OPEN") {
                return Ok(None);
            }
            requests.extend(rows);
        }
    }
    // Repeated reviews may change only task contents. They still establish
    // that a newer retained branch head was merged even when no identity
    // field changed. Every accepted review must contain a verified historical
    // identity state on the synchronized base.
    for request in &requests {
        let Some(proof) = verified_review(repo, task, base, request)? else {
            continue;
        };
        if historical_manifest(repo, &proof.merge_commit, task)?.is_some_and(|merged| {
            history.iter().any(|snapshot| {
                snapshot.directory == merged.directory && snapshot.tree == merged.tree
            })
        }) {
            reviewed.push((request.head_ref_name.clone(), proof));
        }
    }
    // A retained ref may lag behind its merged review; it must never contain
    // commits that the review did not contain. Check both current and historic
    // names against the reviewed head for that identity state.
    for branch in branches {
        let local = repo.optional_oid(&format!("refs/heads/{branch}"))?;
        let remote_oid = repo.remote_branch_oid(remote, &branch)?;
        if let Some(oid) = &remote_oid {
            repo.fetch_branch_objects(remote, &branch, oid)?;
        }
        for oid in local.iter().chain(remote_oid.iter()) {
            let mut contained = false;
            for (_, proof) in &reviewed {
                ensure_review_head(repo, remote, proof)?;
                if ancestor(repo, oid, &proof.head_commit)? {
                    contained = true;
                    break;
                }
            }
            if !contained {
                return Ok(None);
            }
        }
    }
    let pull_request = reviewed
        .first()
        .expect("every history snapshot has a review")
        .1
        .clone();
    let mut reviews = reviewed
        .into_iter()
        .map(|(_, proof)| proof)
        .collect::<Vec<_>>();
    reviews.sort_by_key(|review| review.number);
    reviews.dedup_by_key(|review| review.number);
    Ok(Some(CompletionProof {
        pull_request,
        reviews,
    }))
}

fn branch_pull_requests(
    repo: &GitRepo,
    host: &str,
    branch: &str,
) -> Result<Vec<HostingPullRequest>> {
    let output = process::run(
        &hosting_command(),
        [
            "pr",
            "list",
            "--repo",
            host,
            "--head",
            branch,
            "--state",
            "all",
            "--limit",
            "100",
            "--json",
            "number,url,state,mergedAt,mergeCommit,headRefName,headRefOid,baseRefName,isCrossRepository",
        ],
        &repo.root,
    )?;
    let requests: Vec<HostingPullRequest> =
        serde_json::from_str(&output.stdout).map_err(|error| {
            Error::message(format!(
                "archive pull-request verification returned invalid JSON: {error}"
            ))
        })?;
    if requests.len() >= 100 {
        return Err(Error::message(
            "archive pull-request query reached its limit; resolve ambiguous task branch history first",
        ));
    }
    Ok(requests
        .into_iter()
        .filter(|pr| pr.head_ref_name == branch && !pr.is_cross_repository)
        .collect())
}

fn associated_pull_requests(
    repo: &GitRepo,
    host: &str,
    commit: &str,
) -> Result<Vec<HostingPullRequest>> {
    let (hostname, repository) = host
        .split_once('/')
        .ok_or_else(|| Error::message("invalid archive hosting repository"))?;
    let endpoint = format!("repos/{repository}/commits/{commit}/pulls?per_page=100");
    let output = process::run(
        &hosting_command(),
        ["api", "--hostname", hostname, &endpoint],
        &repo.root,
    )?;
    let rows: Vec<AssociatedPullRequest> =
        serde_json::from_str(&output.stdout).map_err(|error| {
            Error::message(format!(
                "archive commit review verification returned invalid JSON: {error}"
            ))
        })?;
    if rows.len() >= 100 {
        return Err(Error::message(
            "archive commit review query reached its limit; review history is ambiguous",
        ));
    }
    Ok(rows
        .into_iter()
        .filter_map(|pr| {
            let same_repository = pr
                .head
                .repo
                .as_ref()
                .zip(pr.base.repo.as_ref())
                .is_some_and(|(head, base)| {
                    head.full_name == repository && base.full_name == repository
                });
            same_repository.then(|| HostingPullRequest {
                number: pr.number,
                url: pr.html_url,
                state: if pr.merged_at.is_some() {
                    "MERGED".to_owned()
                } else {
                    pr.state.to_ascii_uppercase()
                },
                merged_at: pr.merged_at,
                merge_commit: pr.merge_commit_sha.map(|oid| HostingCommit { oid }),
                head_ref_name: pr.head.name,
                head_ref_oid: pr.head.sha,
                base_ref_name: pr.base.name,
                is_cross_repository: false,
            })
        })
        .collect())
}

fn verified_review(
    repo: &GitRepo,
    task: &ResolvedTask,
    base: &str,
    request: &HostingPullRequest,
) -> Result<Option<MergedPullRequest>> {
    if request.state != "MERGED"
        || request.base_ref_name != task.base_branch
        || request.is_cross_repository
    {
        return Ok(None);
    }
    let (Some(merged_at), Some(commit)) = (&request.merged_at, &request.merge_commit) else {
        return Err(Error::message(
            "archive merged pull request lacks a merge timestamp or commit",
        ));
    };
    if chrono::DateTime::parse_from_rfc3339(merged_at).is_err()
        || !commit_oid(&commit.oid)
        || !commit_oid(&request.head_ref_oid)
    {
        return Err(Error::message(
            "archive merged pull request contains invalid merge evidence",
        ));
    }
    if !ancestor(repo, &commit.oid, base)? {
        return Ok(None);
    }
    Ok(Some(MergedPullRequest {
        number: request.number,
        url: request.url.clone(),
        merged_at: merged_at.clone(),
        merge_commit: commit.oid.clone(),
        head_commit: request.head_ref_oid.clone(),
    }))
}

fn commit_oid(oid: &str) -> bool {
    oid.len() == 40 && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
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

fn manifest_history(
    repo: &GitRepo,
    base: &str,
    task: &ResolvedTask,
) -> Result<Vec<HistoricalManifest>> {
    let pathspec = format!(":(glob)**/{TASK_MANIFEST_NAME}");
    let manifest_commits = repo
        .run([
            "log",
            "--first-parent",
            "--format=%H",
            base,
            "--",
            &pathspec,
        ])?
        .stdout;
    let mut directories = BTreeSet::new();
    for commit in manifest_commits.lines() {
        if let Some(snapshot) = historical_manifest(repo, commit, task)? {
            directories.insert(snapshot.directory);
        }
    }
    if directories.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec!["log", "--first-parent", "--format=%H", base, "--"];
    args.extend(directories.iter().map(String::as_str));
    let commits = repo.run(args)?.stdout;
    let mut history: Vec<HistoricalManifest> = Vec::new();
    let mut gap = false;
    for commit in commits.lines() {
        let Some(snapshot) = historical_manifest(repo, commit, task)? else {
            if !history.is_empty() {
                gap = true;
            }
            continue;
        };
        if gap {
            return Err(Error::message(
                "archive task identity disappeared and was reintroduced; historical ownership cannot be verified",
            ));
        }
        if let Some(previous) = history.last_mut().filter(|previous| {
            previous.directory == snapshot.directory && previous.tree == snapshot.tree
        }) {
            previous.commit = snapshot.commit;
        } else {
            history.push(snapshot);
        }
    }
    Ok(history)
}

fn historical_manifest(
    repo: &GitRepo,
    commit: &str,
    task: &ResolvedTask,
) -> Result<Option<HistoricalManifest>> {
    let paths = crate::manifest::published_task_paths(repo, commit, task)?;
    if paths.len() > 1 {
        return Err(Error::message(
            "archive PR merge tree contains multiple directories for this task identity",
        ));
    }
    let Some(directory) = paths.into_iter().next() else {
        return Ok(None);
    };
    let raw = repo
        .run([
            "show",
            &format!("{commit}:{directory}/{TASK_MANIFEST_NAME}"),
        ])?
        .stdout;
    let manifest: TaskManifest = toml::from_str(&raw).map_err(|error| {
        Error::message(format!("invalid historical archive task manifest: {error}"))
    })?;
    let identity = parse_task_identity(manifest.kind, &manifest.id)?;
    let slug = match manifest.schema_version {
        1 if manifest.slug.is_empty() => &identity.original_slug,
        2 | 3 => {
            crate::manifest::validate_task_slug(&manifest.slug)?;
            &manifest.slug
        }
        _ => {
            return Err(Error::message(
                "archive historical task manifest has an unsupported schema or invalid slug",
            ));
        }
    };
    if directory.rsplit('/').next() != Some(build_task_path(&identity, slug).as_str())
        || manifest.kind != TaskKind::Deliverable
    {
        return Err(Error::message(
            "archive historical task manifest has an unverifiable directory identity",
        ));
    }
    repo.validate_branch(&manifest.branch)?;
    let tree = repo
        .optional_oid(&format!("{commit}:{directory}"))?
        .ok_or_else(|| Error::message("archive historical task tree is unavailable"))?;
    Ok(Some(HistoricalManifest {
        commit: commit.to_owned(),
        directory,
        manifest,
        tree,
    }))
}

fn tree_manifest_matches(
    repo: &GitRepo,
    oid: &str,
    directory: &str,
    task: &ResolvedTask,
) -> Result<bool> {
    let output =
        repo.run_unchecked(["show", &format!("{oid}:{directory}/{TASK_MANIFEST_NAME}")])?;
    if !output.success() {
        return Ok(false);
    }
    let value: toml::Value = toml::from_str(&output.stdout).map_err(|error| {
        Error::message(format!("invalid published archive task manifest: {error}"))
    })?;
    Ok(
        value.get("kind").and_then(toml::Value::as_str) == Some("deliverable")
            && value.get("id").and_then(toml::Value::as_str) == Some(task.task_id.as_str())
            && value.get("branch").and_then(toml::Value::as_str) == Some(task.branch.as_str())
            && value.get("path").and_then(toml::Value::as_str) == Some(directory),
    )
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

pub(crate) fn validate_clean_paths(
    repo: &GitRepo,
    base: &str,
    source: &str,
    destination: &str,
) -> Result<()> {
    validate_overlays(repo, source, destination)?;
    let difference = repo.run_unchecked(["diff", "--quiet", base, "HEAD", "--", source])?;
    if difference.code != 0 {
        return Err(Error::message(format!(
            "archive source {source} differs from the fetched shared branch; refresh or preserve those changes first"
        )));
    }
    Ok(())
}

fn validate_overlays(repo: &GitRepo, source: &str, destination: &str) -> Result<()> {
    let status = repo.run([
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--",
        source,
        destination,
    ])?;
    if !status.stdout.is_empty() {
        return Err(Error::message(format!(
            "archive refuses staged, modified, or untracked overlays in {source} or {destination} at {}; preserve those changes first",
            repo.root.display()
        )));
    }
    Ok(())
}

fn validate_materialized(repo: &GitRepo, source: &str) -> Result<()> {
    let scopes = vec![source.to_owned()];
    for boundary in storage::local_boundaries(repo, &scopes)? {
        if resolved_under(&repo.root, &boundary).exists() {
            return Err(Error::message(format!(
                "archive task {source} contains retained local-only payload {boundary} at {}; preserve or restore tracking before organizing it",
                repo.root.display()
            )));
        }
    }
    for pointer in dvc::discover(repo, &scopes)? {
        let raw = fs::read_to_string(resolved_under(&repo.root, &pointer)).at(&pointer)?;
        let output = dvc::metadata_output(repo, &pointer, &raw)?;
        if resolved_under(&repo.root, &output).exists()
            && !dvc::payload_matches_metadata(repo, &pointer, &raw)?
        {
            return Err(Error::message(format!(
                "archive refuses locally changed materialized S3 output {output} at {}; publish or preserve it first",
                repo.root.display()
            )));
        }
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
                    pull_request: MergedPullRequest {
                        number: index as u64 + 1,
                        url: "https://example.invalid/pull/1".to_owned(),
                        merged_at: "2026-07-12T20:00:00Z".to_owned(),
                        merge_commit: "a".repeat(40),
                        head_commit: "b".repeat(40),
                    },
                    review_history: Vec::new(),
                    receipt: serde_json::json!({"status": "planned"}),
                },
                original_manifest: original,
                next_manifest,
                original_receipt: None,
                relocation: crate::relocation::prepare(&directory, &repo.root.join(&destination))
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
