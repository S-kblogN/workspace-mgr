use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::config::Config;
use crate::dvc;
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::manifest::{AdditionalScope, ResolvedTask, one_line};
use crate::path::{allowed, relative_to, repo_path, resolved_under};
use crate::storage;

const ZERO_OID: &str = "0000000000000000000000000000000000000000";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Plan,
    Publish,
}

impl Operation {
    fn name(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Publish => "publish",
        }
    }

    fn dry_run(self, explicit: bool) -> bool {
        self == Self::Plan || explicit
    }
}

#[derive(Debug, Clone)]
pub struct TransactionOptions {
    pub start: PathBuf,
    pub manifest: Option<PathBuf>,
    pub message: Option<String>,
    pub include: Vec<String>,
    pub scope_note: Option<String>,
    pub allow_non_shared_head: bool,
    pub dry_run: bool,
    pub operation: Operation,
}

#[derive(Debug, Clone, Serialize)]
pub struct TransactionReport {
    pub status: String,
    pub operation: String,
    pub head: Option<String>,
    pub branch: String,
    pub base: String,
    pub base_oid: String,
    pub remote_base_oid: String,
    pub scopes: Vec<String>,
    pub changed_paths: Vec<String>,
    pub storage: serde_json::Value,
    pub ignored_entries: usize,
    pub tree_oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push: Option<String>,
}

pub fn execute(options: &TransactionOptions) -> Result<TransactionReport> {
    let repo = if let Some(manifest) = &options.manifest {
        GitRepo::discover(
            manifest
                .parent()
                .ok_or_else(|| Error::message("manifest path has no parent"))?,
        )?
    } else {
        GitRepo::discover(&options.start)?
    };
    let config = Config::load(&repo)?;
    let manifest_path = match &options.manifest {
        Some(path) => path.clone(),
        None => ResolvedTask::discover(&repo, &config, &options.start)?,
    };
    let task = ResolvedTask::load(&repo, &config, &manifest_path)?;
    if config.tasks.require_readme
        && !resolved_under(&repo.root, &format!("{}/README.md", task.task_path)).is_file()
    {
        return Err(Error::message(format!(
            "task README is required but missing: {}/README.md",
            task.task_path
        )));
    }
    repo.validate_branch(&task.branch)?;
    repo.ensure_branch_not_checked_out(&task.branch)?;
    validate_checkout(&repo, &task, options)?;

    let (scopes, authorizations) = resolve_scopes(&task, options)?;
    let dry_run = options.operation.dry_run(options.dry_run);
    let common_dir = repo.common_dir()?;
    let state_dir = state_dir(&common_dir, &task);
    fs::create_dir_all(&state_dir).at(&state_dir)?;
    let _task_lock = LockGuard::acquire(
        &state_dir.join("transaction.lock"),
        &format!(
            "another workspace-mgr transaction is running for {}",
            task.task_path
        ),
    )?;

    let remote_base_oid = repo.fetch_branch(&task.remote, &task.base_branch)?;
    let remote_target_oid = repo.remote_branch_oid(&task.remote, &task.branch)?;
    let (base_ref, base_oid) = if let Some(remote_target_oid) = remote_target_oid {
        let fetched = repo.fetch_branch(&task.remote, &task.branch)?;
        if fetched != remote_target_oid {
            return Err(Error::message(
                "target branch changed while it was being fetched; retry",
            ));
        }
        (
            format!("refs/remotes/{}/{}", task.remote, task.branch),
            fetched,
        )
    } else {
        (
            format!("refs/remotes/{}/{}", task.remote, task.base_branch),
            remote_base_oid.clone(),
        )
    };

    let placement_preview = storage::apply_automatic(&repo, &config, &scopes, &base_oid, true)?;
    let initial_dvc = dvc::discover(&repo, &scopes)?;
    let mut lock_names = initial_dvc
        .iter()
        .map(|path| format!("pointer:{path}"))
        .chain(
            placement_preview
                .would_place_in_s3
                .iter()
                .map(|path| format!("output:{path}")),
        )
        .collect::<Vec<_>>();
    lock_names.sort();
    lock_names.dedup();
    let _dvc_locks = acquire_dvc_locks(&common_dir, &lock_names)?;

    let placement = if dry_run {
        placement_preview
    } else {
        storage::apply_automatic(&repo, &config, &scopes, &base_oid, false)?
    };
    let pointers = dvc::discover(&repo, &scopes)?;
    let s3 = dvc::reconcile(&repo, &config, &pointers, dry_run)?;
    let storage_report = serde_json::json!({"placement": placement, "s3": s3});

    let index = state_dir.join("index");
    if index.exists() {
        fs::remove_file(&index).at(&index)?;
    }
    repo.run_with_index(&index, ["read-tree", &base_oid], None, true)?;
    let mut add = vec!["add".to_owned(), "-A".to_owned(), "--".to_owned()];
    add.extend(scopes.iter().cloned());
    repo.run_with_index(&index, add, None, true)?;
    let paths = changed_paths(&repo, &index, &base_oid)?;
    let escaped: Vec<String> = paths
        .iter()
        .filter(|path| !allowed(path, &scopes))
        .cloned()
        .collect();
    if !escaped.is_empty() {
        return Err(Error::message(format!(
            "private index escaped the declared scope: {}",
            escaped.join(", ")
        )));
    }
    check_gitlinks(&repo, &index, &paths)?;
    check_large_files(
        &repo,
        &scopes,
        &base_oid,
        config.storage.auto_s3_above_bytes,
        &placement.would_place_in_s3,
    )?;
    repo.run_with_index(
        &index,
        ["diff", "--cached", "--check", &base_oid, "--"],
        None,
        true,
    )?;
    let ignored_entries = count_ignored(&repo, &index, &scopes)?;
    let tree_oid = repo
        .run_with_index(&index, ["write-tree"], None, true)?
        .stdout
        .trim()
        .to_owned();
    let storage_dirty = storage_report
        .get("s3")
        .and_then(|value| value.get("dirty_files"))
        .and_then(|value| value.as_array())
        .is_some_and(|files| !files.is_empty());
    let mut report = TransactionReport {
        status: if dry_run { "dry_run" } else { "pending" }.to_owned(),
        operation: options.operation.name().to_owned(),
        head: repo.current_branch()?,
        branch: task.branch.clone(),
        base: base_ref,
        base_oid: base_oid.clone(),
        remote_base_oid,
        scopes: scopes.clone(),
        changed_paths: paths.clone(),
        storage: storage_report,
        ignored_entries,
        tree_oid: tree_oid.clone(),
        commit_oid: None,
        remote_oid: None,
        push: None,
    };
    if paths.is_empty() && !storage_dirty {
        report.status = "no_changes".to_owned();
        return Ok(report);
    }
    if dry_run {
        return Ok(report);
    }
    if paths.is_empty() {
        report.status = "no_changes".to_owned();
        return Ok(report);
    }

    let message = one_line(
        options
            .message
            .as_deref()
            .ok_or_else(|| Error::message("publish requires -m/--message"))?,
        "commit message",
    )?;
    let commit_message = build_commit_message(&message, &scopes, &authorizations);
    let commit_oid = repo
        .run_with_index(
            &index,
            ["commit-tree", &tree_oid, "-p", &base_oid],
            Some(&commit_message),
            true,
        )?
        .stdout
        .trim()
        .to_owned();
    let local_ref = format!("refs/heads/{}", task.branch);
    let old_local_oid = repo.optional_oid(&local_ref)?;
    repo.run([
        "update-ref",
        "-m",
        &format!("workspace-mgr publish for {}", task.task_path),
        &local_ref,
        &commit_oid,
        old_local_oid.as_deref().unwrap_or(ZERO_OID),
    ])?;
    let refspec = format!("{commit_oid}:refs/heads/{}", task.branch);
    let push = repo.run(["push", "--porcelain", &task.remote, &refspec])?;
    let observed = repo
        .remote_branch_oid(&task.remote, &task.branch)?
        .ok_or_else(|| Error::message("remote branch disappeared after push"))?;
    if observed != commit_oid {
        return Err(Error::message(format!(
            "push verification failed: remote has {observed}, expected {commit_oid}"
        )));
    }
    repo.run([
        "update-ref",
        "-m",
        &format!("record push for {}", task.task_path),
        &format!("refs/remotes/{}/{}", task.remote, task.branch),
        &commit_oid,
    ])?;
    report.status = "pushed".to_owned();
    report.commit_oid = Some(commit_oid);
    report.remote_oid = Some(observed);
    let _ = push;
    report.push = Some("explicit refspec pushed and remote object ID verified".to_owned());
    Ok(report)
}

fn validate_checkout(
    repo: &GitRepo,
    task: &ResolvedTask,
    options: &TransactionOptions,
) -> Result<()> {
    let head = repo.current_branch()?;
    if head.as_deref() != Some(&task.shared_head) {
        if !options.allow_non_shared_head {
            return Err(Error::message(format!(
                "checkout is on {:?}, expected {:?}; use an explicitly authorized alternate workflow or --allow-non-shared-head with --scope-note",
                head.as_deref().unwrap_or("detached HEAD"),
                task.shared_head
            )));
        }
        if options.scope_note.is_none() {
            return Err(Error::message(
                "--allow-non-shared-head requires --scope-note",
            ));
        }
    }
    if head.as_deref() == Some(&task.branch) {
        return Err(Error::message(
            "target branch may not be the checkout's current branch",
        ));
    }
    Ok(())
}

fn resolve_scopes(
    task: &ResolvedTask,
    options: &TransactionOptions,
) -> Result<(Vec<String>, Vec<AdditionalScope>)> {
    let mut additional = task.additional_scopes.clone();
    if !options.include.is_empty() && options.scope_note.is_none() {
        return Err(Error::message(
            "--include requires --scope-note describing its authorization",
        ));
    }
    if let Some(reason) = &options.scope_note {
        let reason = one_line(reason, "scope note")?;
        for path in &options.include {
            additional.push(AdditionalScope {
                path: repo_path(path, "included scope")?,
                reason: reason.clone(),
            });
        }
    }
    let mut scopes = vec![task.task_path.clone()];
    scopes.extend(additional.iter().map(|entry| entry.path.clone()));
    let mut seen = BTreeSet::new();
    scopes.retain(|scope| seen.insert(scope.clone()));
    Ok((scopes, additional))
}

fn state_dir(common_dir: &Path, task: &ResolvedTask) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(task.task_path.as_bytes());
    hasher.update(b"\0");
    hasher.update(task.branch.as_bytes());
    common_dir
        .join("workspace-mgr/state")
        .join(format!("{:x}", hasher.finalize()))
}

struct LockGuard {
    _file: File,
}

impl LockGuard {
    fn acquire(path: &Path, busy_message: &str) -> Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| Error::message("lock path has no parent"))?;
        fs::create_dir_all(parent).at(parent)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .at(path)?;
        file.try_lock_exclusive()
            .map_err(|_| Error::message(busy_message))?;
        Ok(Self { _file: file })
    }
}

fn acquire_dvc_locks(common_dir: &Path, names: &[String]) -> Result<Vec<LockGuard>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let lock_dir = common_dir.join("workspace-mgr/dvc-locks");
    let mut guards = vec![LockGuard::acquire(
        &lock_dir.join("transaction.lock"),
        "another repository transaction is running",
    )?];
    for name in names {
        let mut hasher = Sha256::new();
        hasher.update(name.as_bytes());
        let path = lock_dir.join(format!("{:x}.lock", hasher.finalize()));
        guards.push(LockGuard::acquire(
            &path,
            &format!("another transaction is updating the same storage boundary: {name}"),
        )?);
    }
    Ok(guards)
}

fn changed_paths(repo: &GitRepo, index: &Path, base: &str) -> Result<Vec<String>> {
    let output = repo.run_with_index(
        index,
        [
            "diff",
            "--cached",
            "--name-only",
            "--no-renames",
            "-z",
            base,
            "--",
        ],
        None,
        true,
    )?;
    let mut paths: Vec<String> = output
        .stdout
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    paths.sort();
    Ok(paths)
}

fn check_gitlinks(repo: &GitRepo, index: &Path, paths: &[String]) -> Result<()> {
    for path in paths {
        let output = repo.run_with_index(index, ["ls-files", "--stage", "--", path], None, true)?;
        for line in output.stdout.lines() {
            if line.split_whitespace().next() == Some("160000") {
                return Err(Error::message(format!(
                    "{path:?} is staged as a nested Git checkout/gitlink; ignore the checkout instead"
                )));
            }
        }
    }
    Ok(())
}

fn check_large_files(
    repo: &GitRepo,
    scopes: &[String],
    base_oid: &str,
    threshold: u64,
    automatic_s3: &[String],
) -> Result<()> {
    for scope in scopes {
        let root = resolved_under(&repo.root, scope);
        if !root.exists() || root.is_symlink() {
            continue;
        }
        let walker = if root.is_file() {
            WalkDir::new(&root).max_depth(0)
        } else {
            WalkDir::new(&root)
        };
        for entry in walker.follow_links(false) {
            let entry = entry.map_err(|error| Error::message(format!("walk failed: {error}")))?;
            if !entry.file_type().is_file() || entry.path().is_symlink() {
                continue;
            }
            if entry
                .metadata()
                .map_err(|error| Error::message(error.to_string()))?
                .len()
                <= threshold
            {
                continue;
            }
            let relative = relative_to(entry.path(), &repo.root, "large file")?;
            let ignored = repo.run_unchecked(["check-ignore", "--quiet", "--", &relative])?;
            if ignored.code == 0 {
                continue;
            }
            if ignored.code != 1 {
                return Err(Error::message(format!(
                    "git check-ignore failed for {relative}"
                )));
            }
            if automatic_s3.contains(&relative) {
                continue;
            }
            if storage::explicit_target(repo, &relative)? == Some(crate::config::StorageTarget::Git)
            {
                continue;
            }
            let object = format!("{base_oid}:{relative}");
            if repo.run_unchecked(["cat-file", "-e", &object])?.success() {
                continue;
            }
            return Err(Error::message(format!(
                "retained file {relative:?} is larger than {threshold} bytes and has no valid placement; run `workspace-mgr storage set {relative} --to git|s3 --reason <reason>`"
            )));
        }
    }
    Ok(())
}

fn count_ignored(repo: &GitRepo, index: &Path, scopes: &[String]) -> Result<usize> {
    let mut args = vec![
        "status".to_owned(),
        "--ignored".to_owned(),
        "--short".to_owned(),
        "--untracked-files=all".to_owned(),
        "--".to_owned(),
    ];
    args.extend(scopes.iter().cloned());
    Ok(repo
        .run_with_index(index, args, None, true)?
        .stdout
        .lines()
        .filter(|line| line.starts_with("!!"))
        .count())
}

fn build_commit_message(
    message: &str,
    scopes: &[String],
    authorizations: &[AdditionalScope],
) -> String {
    let mut lines = vec![
        message.to_owned(),
        String::new(),
        format!("Workspace-Scope: {}", scopes.join(", ")),
    ];
    for authorization in authorizations {
        lines.push(format!(
            "Scope-Authorization: {} -- {}",
            authorization.path, authorization.reason
        ));
    }
    lines.join("\n") + "\n"
}

pub fn task_status(start: &Path, manifest: Option<&Path>) -> Result<TaskStatus> {
    let repo = GitRepo::discover(start)?;
    let config = Config::load(&repo)?;
    let path = match manifest {
        Some(path) => path.to_path_buf(),
        None => ResolvedTask::discover(&repo, &config, start)?,
    };
    let task = ResolvedTask::load(&repo, &config, &path)?;
    let scopes = task.scopes();
    let mut args = vec!["status".to_owned(), "--short".to_owned(), "--".to_owned()];
    args.extend(scopes.iter().cloned());
    let working_changes = repo
        .run(args)?
        .stdout
        .lines()
        .map(ToOwned::to_owned)
        .collect();
    Ok(TaskStatus {
        task_id: task.task_id,
        manifest: task.manifest_path.display().to_string(),
        branch: task.branch,
        remote: task.remote,
        base_branch: task.base_branch,
        scopes,
        working_changes,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskStatus {
    pub task_id: String,
    pub manifest: String,
    pub branch: String,
    pub remote: String,
    pub base_branch: String,
    pub scopes: Vec<String>,
    pub working_changes: Vec<String>,
}
