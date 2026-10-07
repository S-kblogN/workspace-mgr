//! Explicit, reviewed adoption of pre-manifest task directories.
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::archive::{self, ArchiveOptions, MergedPullRequest};
use crate::config::{Config, require_supported_cli_at};
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::lock::RepositoryLock;
use crate::manifest::{
    ResolvedTask, TaskKind, TaskManifest, build_task_branch, parse_task_identity,
};
use crate::path::{allowed, reject_symlink_traversal, repo_path};
use crate::policy::TASK_MANIFEST_NAME;
use crate::process;

pub(crate) const LEGACY_RECORD: &str = ".workspace-mgr-legacy.json";

pub struct ArchiveAdoptionOptions {
    pub start: PathBuf,
    pub manifest: Option<PathBuf>,
    pub path: String,
    pub pull_request: u64,
    pub title: String,
    pub purpose: String,
    pub dry_run: bool,
}

#[derive(Debug, Serialize)]
pub struct AdoptionReport {
    pub status: &'static str,
    pub operation: &'static str,
    pub task_id: String,
    pub path: String,
    pub branch: String,
    pub legacy_branch: String,
    pub pull_request: MergedPullRequest,
    pub required_scopes: Vec<String>,
    pub review_required_before_archive: bool,
    pub remote_writes: bool,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyRecord {
    pub schema_version: u32,
    pub task_id: String,
    pub path: String,
    pub branch: String,
    pub pull_request: MergedPullRequest,
    pub tree: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Review {
    number: u64,
    url: String,
    state: String,
    merged_at: Option<String>,
    merge_commit: Option<Commit>,
    head_ref_name: String,
    head_ref_oid: String,
    base_ref_name: String,
    is_cross_repository: bool,
}

#[derive(Deserialize)]
struct Commit {
    oid: String,
}

pub fn adopt(options: &ArchiveAdoptionOptions) -> Result<AdoptionReport> {
    let repo = match &options.manifest {
        Some(path) => GitRepo::discover_for_manifest(path)?,
        None => GitRepo::discover(&options.start)?,
    };
    let _lock = RepositoryLock::acquire(&repo)?;
    let config = Config::load_compatible(&repo)?;
    let path = repo_path(&options.path, "legacy task path")?;
    reject_symlink_traversal(&repo.root, &path, "legacy task adoption")?;
    let directory = repo.root.join(&path);
    if !directory.is_dir() {
        return Err(Error::message(
            "legacy adoption requires an existing task directory",
        ));
    }
    let basename = path.rsplit('/').next().expect("nonempty task path");
    let identity = parse_task_identity(TaskKind::Deliverable, basename)?;
    let task_id = basename.to_owned();
    let owner = archive::infrastructure_task(
        &repo,
        &config,
        &ArchiveOptions {
            start: options.start.clone(),
            manifest: options.manifest.clone(),
            paths: vec![path.clone()],
            layout: "{year}/{month}".to_owned(),
            dry_run: options.dry_run,
            historical_records: Vec::new(),
        },
    )?;
    if let Some(owner) = &owner {
        if !allowed(&path, &owner.scopes()) {
            return Err(Error::message(
                "legacy adoption path escapes the infrastructure task's declared scopes",
            ));
        }
    }
    let base = repo.fetch_branch(&config.git.remote, &config.git.branch)?;
    require_supported_cli_at(
        &repo,
        &base,
        &format!("{}/{}", config.git.remote, config.git.branch),
    )?;
    let host = archive::hosting_repository(&repo, &config.git.remote)?;
    let (review, legacy_branch) =
        legacy_review(&repo, &config, &host, options.pull_request, &base)?;
    let original_tree = task_tree(&repo, &review.merge_commit, &path)?;
    if original_tree.is_empty()
        || repo
            .run_unchecked([
                "cat-file",
                "-e",
                &format!("{}:{path}/{TASK_MANIFEST_NAME}", review.merge_commit),
            ])?
            .success()
    {
        return Err(Error::message(
            "legacy adoption PR must contain the selected directory without a task manifest",
        ));
    }
    if original_tree != task_tree(&repo, &base, &path)? {
        return Err(Error::message(
            "legacy task content differs from its reviewed merge; its ownership or newer work cannot be verified",
        ));
    }
    let next = TaskManifest {
        schema_version: 2,
        kind: TaskKind::Deliverable,
        id: task_id.clone(),
        slug: identity.original_slug.clone(),
        path: Some(path.clone()),
        branch: build_task_branch(TaskKind::Deliverable, &identity.original_slug)?,
        title: one_line(&options.title, "adoption title")?,
        purpose: one_line(&options.purpose, "adoption purpose")?,
        additional_scopes: Vec::new(),
        cloud_usage_approval: None,
        archive_completion: None,
    };
    let record = LegacyRecord {
        schema_version: 1,
        task_id: task_id.clone(),
        path: path.clone(),
        branch: legacy_branch.clone(),
        pull_request: review.clone(),
        tree: original_tree,
    };
    let manifest_path = directory.join(TASK_MANIFEST_NAME);
    let record_path = directory.join(LEGACY_RECORD);
    for metadata in [TASK_MANIFEST_NAME, LEGACY_RECORD] {
        reject_symlink_traversal(
            &repo.root,
            &format!("{path}/{metadata}"),
            "legacy adoption metadata",
        )?;
    }
    let next_raw = next.render()?;
    let record_raw = serde_json::to_string_pretty(&record).map_err(|error| {
        Error::message(format!("failed to render legacy adoption record: {error}"))
    })? + "\n";
    let already = manifest_path.exists() || record_path.exists();
    if already {
        if fs::read_to_string(&manifest_path).at(&manifest_path)? != next_raw
            || fs::read_to_string(&record_path).at(&record_path)? != record_raw
        {
            return Err(Error::message(
                "legacy adoption metadata already exists with different evidence; preserve it before retrying",
            ));
        }
    } else {
        archive::validate_clean_paths(&repo, &base, &path, &path)?;
        if !options.dry_run {
            // Create both files without replacing any existing local content.
            create_new(&manifest_path, &next_raw)?;
            if let Err(error) = create_new(&record_path, &record_raw) {
                let _ = fs::remove_file(&manifest_path);
                return Err(error);
            }
        }
    }
    Ok(AdoptionReport {
        status: if already {
            "no_changes"
        } else if options.dry_run {
            "dry_run"
        } else {
            "adopted"
        },
        operation: "task-adopt",
        task_id,
        path: path.clone(),
        branch: next.branch,
        legacy_branch,
        pull_request: review,
        required_scopes: vec![path],
        review_required_before_archive: true,
        remote_writes: false,
    })
}

pub(crate) fn verify_record(
    repo: &GitRepo,
    config: &Config,
    host: &str,
    base: &str,
    task: &ResolvedTask,
    adoption: (&str, &str),
) -> Result<Option<(String, MergedPullRequest)>> {
    let path = task.task_path.as_deref().expect("a deliverable task path");
    let (adoption_commit, adoption_path) = adoption;
    let raw = repo.run_unchecked(["show", &format!("{base}:{path}/{LEGACY_RECORD}")])?;
    if !raw.success() {
        if repo
            .run_unchecked([
                "cat-file",
                "-e",
                &format!("{adoption_commit}:{adoption_path}/{LEGACY_RECORD}"),
            ])?
            .success()
        {
            return Err(Error::message(
                "legacy adoption record was removed; historical review ownership must remain verifiable",
            ));
        }
        return Ok(None);
    }
    let record: LegacyRecord = serde_json::from_str(&raw.stdout)
        .map_err(|error| Error::message(format!("invalid legacy adoption record: {error}")))?;
    if record.schema_version != 1 || record.task_id != task.task_id {
        return Err(Error::message(
            "legacy adoption record has an unverifiable task identity",
        ));
    }
    let original = repo.run([
        "show",
        &format!("{adoption_commit}:{adoption_path}/{LEGACY_RECORD}"),
    ])?;
    let original: LegacyRecord = serde_json::from_str(&original.stdout).map_err(|error| {
        Error::message(format!("invalid original legacy adoption record: {error}"))
    })?;
    if original != record {
        return Err(Error::message(
            "legacy adoption evidence changed after adoption; original review ownership must remain immutable",
        ));
    }
    let (review, branch) = legacy_review(repo, config, host, record.pull_request.number, base)?;
    if review != record.pull_request
        || branch != record.branch
        || task_tree(repo, &review.merge_commit, &record.path)? != record.tree
        || task_tree(repo, adoption_commit, adoption_path)? != record.tree
    {
        return Err(Error::message(
            "legacy adoption does not match its immutable review or adopted task content",
        ));
    }
    Ok(Some((branch, review)))
}

fn legacy_review(
    repo: &GitRepo,
    config: &Config,
    host: &str,
    number: u64,
    base: &str,
) -> Result<(MergedPullRequest, String)> {
    #[cfg(feature = "test-storage")]
    let command = std::env::var("WORKSPACE_MGR_TEST_GH").unwrap_or_else(|_| "gh".to_owned());
    #[cfg(not(feature = "test-storage"))]
    let command = "gh".to_owned();
    let output = process::run(
        &command,
        [
            "pr",
            "view",
            &number.to_string(),
            "--repo",
            host,
            "--json",
            "number,url,state,mergedAt,mergeCommit,headRefName,headRefOid,baseRefName,isCrossRepository",
        ],
        &repo.root,
    )?;
    let pr: Review = serde_json::from_str(&output.stdout).map_err(|error| {
        Error::message(format!(
            "legacy adoption review returned invalid JSON: {error}"
        ))
    })?;
    let (Some(merged_at), Some(merge)) = (pr.merged_at, pr.merge_commit) else {
        return Err(Error::message(
            "legacy adoption requires a verified merged same-repository pull request",
        ));
    };
    if pr.number != number
        || pr.state != "MERGED"
        || pr.is_cross_repository
        || pr.base_ref_name != config.git.branch
        || chrono::DateTime::parse_from_rfc3339(&merged_at).is_err()
        || !valid_oid(&merge.oid)
        || !valid_oid(&pr.head_ref_oid)
        || !archive::ancestor(repo, &merge.oid, base)?
    {
        return Err(Error::message(
            "legacy adoption requires a verified merged same-repository pull request on the configured base",
        ));
    }
    repo.validate_branch(&pr.head_ref_name)?;
    // An open review on the original branch keeps the legacy task active even
    // when its last visible ref happens to equal an older merged head.
    let output = process::run(
        &command,
        [
            "pr",
            "list",
            "--repo",
            host,
            "--head",
            &pr.head_ref_name,
            "--state",
            "all",
            "--limit",
            "100",
            "--json",
            "number,url,state,mergedAt,mergeCommit,headRefName,headRefOid,baseRefName,isCrossRepository",
        ],
        &repo.root,
    )?;
    let rows: Vec<Review> = serde_json::from_str(&output.stdout).map_err(|error| {
        Error::message(format!(
            "legacy adoption branch reviews returned invalid JSON: {error}"
        ))
    })?;
    if rows.len() >= 100
        || rows.iter().any(|row| {
            row.head_ref_name == pr.head_ref_name && !row.is_cross_repository && row.state == "OPEN"
        })
    {
        return Err(Error::message(
            "legacy adoption refuses an open or ambiguous review history",
        ));
    }
    let local = repo.optional_oid(&format!("refs/heads/{}", pr.head_ref_name))?;
    let remote = repo.remote_branch_oid(&config.git.remote, &pr.head_ref_name)?;
    if let Some(oid) = &remote {
        repo.fetch_branch_objects(&config.git.remote, &pr.head_ref_name, oid)?;
    }
    for oid in local.iter().chain(remote.iter()) {
        archive::ensure_review_head(
            repo,
            &config.git.remote,
            &MergedPullRequest {
                number: pr.number,
                url: pr.url.clone(),
                merged_at: merged_at.clone(),
                merge_commit: merge.oid.clone(),
                head_commit: pr.head_ref_oid.clone(),
            },
        )?;
        if !archive::ancestor(repo, oid, &pr.head_ref_oid)? {
            return Err(Error::message(
                "legacy adoption branch contains unmerged or divergent commits",
            ));
        }
    }
    Ok((
        MergedPullRequest {
            number: pr.number,
            url: pr.url,
            merged_at,
            merge_commit: merge.oid,
            head_commit: pr.head_ref_oid,
        },
        pr.head_ref_name,
    ))
}

fn valid_oid(oid: &str) -> bool {
    oid.len() == 40 && oid.bytes().all(|c| c.is_ascii_hexdigit())
}

pub(crate) fn task_tree(repo: &GitRepo, oid: &str, directory: &str) -> Result<Vec<String>> {
    let raw = repo
        .run(["ls-tree", "-r", "-z", oid, "--", directory])?
        .stdout;
    let prefix = format!("{directory}/");
    let mut entries = Vec::new();
    for row in raw.split('\0').filter(|row| !row.is_empty()) {
        let (metadata, path) = row
            .split_once('\t')
            .ok_or_else(|| Error::message("invalid legacy task tree"))?;
        let relative = path
            .strip_prefix(&prefix)
            .ok_or_else(|| Error::message("legacy task tree escapes selected directory"))?;
        if relative == TASK_MANIFEST_NAME || relative == LEGACY_RECORD {
            continue;
        }
        entries.push(format!("{metadata}\t{relative}"));
    }
    entries.sort();
    Ok(entries)
}

fn one_line(raw: &str, label: &str) -> Result<String> {
    if raw.trim().is_empty() || raw.contains(['\n', '\r', '\0']) {
        return Err(Error::message(format!("{label} must be one nonempty line")));
    }
    Ok(raw.to_owned())
}

fn create_new(path: &std::path::Path, raw: &str) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .at(path)?;
    let result = (|| {
        file.write_all(raw.as_bytes()).at(path)?;
        file.sync_all().at(path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}
