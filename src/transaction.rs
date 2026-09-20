use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::dvc;
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::hex::encode_lower;
use crate::lock::RepositoryLock;
use crate::manifest::{
    AdditionalScope, ResolvedTask, TaskKind, one_line, published_history_path,
    published_task_paths, validate_additional_scopes,
};
use crate::path::{allowed, repo_path, resolved_under};
use crate::policy::{
    AUTO_S3_ABOVE_BYTES, BULK_PUBLICATION_BYTES, BULK_PUBLICATION_FILES, BULK_PUBLICATION_MIB,
    REPOSITORY_IGNORE_MODULE, REVIEW_INITIAL_STATE, REVIEW_MANAGED_BY, REVIEW_MERGE_AUTHORITY,
    REVIEW_PULL_REQUEST, ROOT_IGNORE_NAME, TASK_MANIFEST_NAME,
};
use crate::s3_purge;
use crate::scaffold::{PRODUCT_IGNORE_RULES, task_readme_directory_map};
use crate::storage::{self, PLACEMENT_SUFFIX};

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
    pub review: ReviewHandoff,
    pub changed_paths: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<TransactionWarning>,
    pub storage: serde_json::Value,
    pub ignored_entries: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ignored_paths: Vec<String>,
    pub tree_oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TransactionWarning {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewHandoff {
    pub pull_request: &'static str,
    pub initial_state: &'static str,
    pub managed_by: &'static str,
    pub merge_authority: &'static str,
    pub remote: String,
    pub base_branch: String,
    pub head_branch: String,
}

pub fn execute(options: &TransactionOptions) -> Result<TransactionReport> {
    let repo = if let Some(manifest) = &options.manifest {
        GitRepo::discover_for_manifest(manifest)?
    } else {
        GitRepo::discover(&options.start)?
    };
    let _repository_lock = RepositoryLock::acquire(&repo)?;
    let config = Config::load_compatible(&repo)?;
    let manifest_path = match &options.manifest {
        Some(path) => path.clone(),
        None => ResolvedTask::discover(&repo, &options.start)?,
    };
    let task = ResolvedTask::load(&repo, &config, &manifest_path)?;
    if task.kind == TaskKind::Deliverable {
        let task_path = task
            .task_path
            .as_deref()
            .ok_or_else(|| Error::message("deliverable task is missing its task path"))?;
        if !resolved_under(&repo.root, &format!("{task_path}/README.md")).is_file() {
            return Err(Error::message(format!(
                "task README is required but missing: {task_path}/README.md"
            )));
        }
    }
    repo.validate_branch(&task.branch)?;
    repo.validate_remote_name(&task.remote)?;
    if task.kind == TaskKind::Deliverable {
        repo.ensure_branch_not_checked_out(&task.branch)?;
    }
    let (mut scopes, authorizations) = resolve_scopes(&task, options)?;
    validate_checkout(&repo, &task, &scopes, options)?;
    let message = match options.operation {
        Operation::Plan => None,
        Operation::Publish => Some(one_line(
            options
                .message
                .as_deref()
                .ok_or_else(|| Error::message("publish requires -m/--message"))?,
            "commit message",
        )?),
    };
    if options.operation == Operation::Publish && !options.dry_run {
        for identity_name in ["GIT_AUTHOR_IDENT", "GIT_COMMITTER_IDENT"] {
            let identity = repo.run_unchecked(["var", identity_name])?;
            if !identity.success() {
                return Err(Error::message(
                    "publication author and committer names and emails must be configured before publish",
                ));
            }
        }
    }
    let dry_run = options.operation.dry_run(options.dry_run);
    let common_dir = repo.common_dir()?;
    let state_dir = task_state_dir(&common_dir, &task);
    fs::create_dir_all(&state_dir).at(&state_dir)?;
    let _task_lock = LockGuard::acquire(
        &state_dir.join("transaction.lock"),
        &format!(
            "another workspace-mgr transaction is running for {}",
            task.task_id
        ),
    )?;

    let remote_base_oid = repo.fetch_branch(&task.remote, &task.base_branch)?;
    let remote_target_oid = repo.remote_branch_oid(&task.remote, &task.branch)?;
    let has_remote_target = remote_target_oid.is_some();
    let (base_ref, base_oid) = if let Some(remote_target_oid) = remote_target_oid {
        let fetched = repo.fetch_branch(&task.remote, &task.branch)?;
        if fetched != remote_target_oid {
            return Err(Error::message(
                "target branch changed while it was being fetched; retry",
            ));
        }
        validate_remote_task_identity(&repo, &task, &fetched)?;
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
    let mut published_task_path = None;
    if task.kind == TaskKind::Deliverable && has_remote_target {
        let published_paths = published_task_paths(&repo, &base_oid, &task)?;
        if published_paths.len() != 1 {
            return Err(Error::message(format!(
                "published deliverable branch must contain exactly one manifest for task {:?}; found {}",
                task.task_id,
                published_paths.len()
            )));
        }
        published_task_path = published_paths.first().cloned();
        for retired in published_paths {
            if task.task_path.as_deref() == Some(&retired) {
                continue;
            }
            if local_path_exists(&resolved_under(&repo.root, &retired))? {
                return Err(Error::message(format!(
                    "previously published task path {retired:?} reappeared locally after rename; move or remove the conflicting path before publication"
                )));
            }
            scopes.push(retired);
        }
        scopes.sort();
        scopes.dedup();
    }

    let local_only = storage::local_boundaries(&repo, &scopes)?;
    for boundary in &local_only {
        let pointer = format!("{boundary}.dvc");
        if local_path_exists(&resolved_under(&repo.root, &pointer))? {
            return Err(Error::message(format!(
                "managed-storage metadata {pointer:?} conflicts with local-only content {boundary:?}; run `workspace-mgr untrack {boundary}` again to reconcile local retention, or run `workspace-mgr storage set {boundary} --to git|s3 --reason <reason>` to restore tracking explicitly"
            )));
        }
    }
    let initial_dvc = dvc::discover(&repo, &scopes)?;
    dvc::require_addressable_metadata(&initial_dvc)?;
    let initial_outputs = dvc::output_paths(&repo, &initial_dvc)?;
    let placement_preview = storage::apply_automatic(&repo, &config, &scopes, &base_oid, true)?;
    let preview_automatic_s3 = placement_preview.automatic_s3().to_vec();
    let preview_index = state_dir.join("preview-index");
    if preview_index.exists() {
        fs::remove_file(&preview_index).at(&preview_index)?;
    }
    repo.run_with_index(&preview_index, ["read-tree", &base_oid], None, true)?;
    remove_output_paths_from_index(&repo, &preview_index, local_only.iter())?;
    stage_scopes(&repo, &preview_index, &scopes)?;
    remove_stored_outputs_from_index(&repo, &preview_index, &initial_outputs)?;
    remove_output_paths_from_index(&repo, &preview_index, preview_automatic_s3.iter())?;
    remove_output_paths_from_index(&repo, &preview_index, local_only.iter())?;
    let preview_paths = changed_paths(&repo, &preview_index, &base_oid)?;
    validate_private_index(
        &repo,
        &preview_index,
        &base_oid,
        &scopes,
        &preview_paths,
        &PrivateIndexPolicy {
            task: &task,
            published_task_path: published_task_path.as_deref(),
            large_file_threshold: AUTO_S3_ABOVE_BYTES,
            automatic_s3: &preview_automatic_s3,
        },
    )?;
    // Decided on the preview, before any placement change or upload, so `plan`
    // and `publish` refuse the same machine-local ignore rule at the same point.
    let preview_ignored = check_untracked_ignore_sources(&repo, &preview_index, &scopes, &task)?;
    let mut lock_names = initial_dvc
        .iter()
        .map(|path| format!("pointer:{path}"))
        .chain(
            preview_automatic_s3
                .iter()
                .map(|path| format!("output:{path}")),
        )
        .collect::<Vec<_>>();
    lock_names.sort();
    lock_names.dedup();
    let _dvc_locks = acquire_dvc_locks(&common_dir, &lock_names)?;
    if config.requires_object_versioning()
        && (!initial_dvc.is_empty() || !preview_automatic_s3.is_empty())
    {
        dvc::verify_object_versioning(&repo, &config)?;
    }

    let placement = if dry_run {
        placement_preview
    } else {
        storage::apply_automatic(&repo, &config, &scopes, &base_oid, false)?
    };
    let automatic_s3 = placement.automatic_s3().to_vec();
    let pointers = dvc::discover(&repo, &scopes)?;
    if !dry_run {
        let preflight_outputs = dvc::output_paths(&repo, &pointers)?;
        let preflight_index = state_dir.join("preflight-index");
        if preflight_index.exists() {
            fs::remove_file(&preflight_index).at(&preflight_index)?;
        }
        repo.run_with_index(&preflight_index, ["read-tree", &base_oid], None, true)?;
        remove_output_paths_from_index(&repo, &preflight_index, local_only.iter())?;
        stage_scopes(&repo, &preflight_index, &scopes)?;
        remove_stored_outputs_from_index(&repo, &preflight_index, &preflight_outputs)?;
        remove_output_paths_from_index(&repo, &preflight_index, local_only.iter())?;
        let preflight_paths = changed_paths(&repo, &preflight_index, &base_oid)?;
        validate_private_index(
            &repo,
            &preflight_index,
            &base_oid,
            &scopes,
            &preflight_paths,
            &PrivateIndexPolicy {
                task: &task,
                published_task_path: published_task_path.as_deref(),
                large_file_threshold: AUTO_S3_ABOVE_BYTES,
                automatic_s3: &automatic_s3,
            },
        )?;
    }
    let s3 = dvc::reconcile(&repo, &config, &pointers, dry_run)?;
    let mut purge_preview = s3_purge::preview(&repo)?;
    if !local_only.is_empty() {
        let retired_pointers = local_only
            .iter()
            .map(|path| {
                format!(
                    "{}.dvc",
                    published_history_path(
                        path,
                        task.task_path.as_deref(),
                        published_task_path.as_deref(),
                    )
                )
            })
            .collect::<Vec<_>>();
        purge_preview.queued =
            s3_purge::candidates_for_revision(&repo, &config, &base_oid, &retired_pointers)?;
        if !purge_preview.queued.is_empty() {
            purge_preview.status = "pending_publication".to_owned();
        }
    }
    let storage_report = serde_json::json!({
        "placement": placement,
        "local_only": local_only,
        "s3": s3,
        "purge": purge_preview,
    });

    let index = state_dir.join("index");
    if index.exists() {
        fs::remove_file(&index).at(&index)?;
    }
    repo.run_with_index(&index, ["read-tree", &base_oid], None, true)?;
    remove_output_paths_from_index(&repo, &index, local_only.iter())?;
    stage_scopes(&repo, &index, &scopes)?;
    remove_stored_outputs_from_index(&repo, &index, &s3.outputs)?;
    remove_output_paths_from_index(&repo, &index, automatic_s3.iter())?;
    remove_output_paths_from_index(&repo, &index, local_only.iter())?;
    let paths = changed_paths(&repo, &index, &base_oid)?;
    validate_private_index(
        &repo,
        &index,
        &base_oid,
        &scopes,
        &paths,
        &PrivateIndexPolicy {
            task: &task,
            published_task_path: published_task_path.as_deref(),
            large_file_threshold: AUTO_S3_ABOVE_BYTES,
            automatic_s3: &automatic_s3,
        },
    )?;
    // A plan applies no placement and writes nothing, so its final index is
    // built from the same inputs as the preview and the ignore listing taken
    // there is still the listing now. A publication may have written pointers
    // and their ignore rules since, so it pays for a second walk.
    let (ignored_entries, ignored_paths) = if dry_run {
        summarize_ignored(preview_ignored)
    } else {
        count_ignored(&repo, &index, &scopes)?
    };
    let mut warnings = Vec::new();
    if let Some(task_path) = deliverable_task_path(&task) {
        if changes_task_content(task_path, &paths, &automatic_s3) {
            let projection = TaskProjection::resolve(&repo, &index, task_path)?;
            warnings.extend(documentation_warning(
                task_path,
                &projection.documentation,
                &paths,
                &automatic_s3,
            ));
            let (files, bytes) =
                bulk_publication_volume(&repo, &index, &base_oid, task_path, &automatic_s3)?;
            warnings.extend(bulk_publication_warning(files, bytes));
        }
    }
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
    let placement_pending = !automatic_s3.is_empty();
    let mut report = TransactionReport {
        status: if dry_run { "dry_run" } else { "pending" }.to_owned(),
        operation: options.operation.name().to_owned(),
        head: repo.current_branch()?,
        branch: task.branch.clone(),
        base: base_ref,
        base_oid: base_oid.clone(),
        remote_base_oid,
        scopes: scopes.clone(),
        review: ReviewHandoff {
            pull_request: REVIEW_PULL_REQUEST,
            initial_state: REVIEW_INITIAL_STATE,
            managed_by: REVIEW_MANAGED_BY,
            merge_authority: REVIEW_MERGE_AUTHORITY,
            remote: task.remote.clone(),
            base_branch: task.base_branch.clone(),
            head_branch: task.branch.clone(),
        },
        changed_paths: paths.clone(),
        warnings,
        storage: storage_report,
        ignored_entries,
        ignored_paths,
        tree_oid: tree_oid.clone(),
        commit_oid: None,
        remote_oid: None,
        push: None,
    };
    if paths.is_empty() && !storage_dirty && !placement_pending {
        if dry_run {
            report.status = "no_changes".to_owned();
        } else {
            let purge = s3_purge::purge_pending(&repo, &config, &task.remote)?;
            report.status = if purge.deleted.is_empty() {
                "no_changes"
            } else {
                "s3_purged"
            }
            .to_owned();
            report.storage["purge"] = serde_json::to_value(purge).map_err(|error| {
                Error::message(format!("failed to encode S3 purge report: {error}"))
            })?;
        }
        return Ok(report);
    }
    if dry_run {
        return Ok(report);
    }
    if paths.is_empty() {
        report.status = "no_changes".to_owned();
        return Ok(report);
    }

    let commit_message = build_commit_message(
        message
            .as_deref()
            .ok_or_else(|| Error::message("publish requires -m/--message"))?,
        &task.task_id,
        &scopes,
        &authorizations,
    );
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
    let purge_candidates =
        s3_purge::candidates_between(&repo, &config, &base_oid, &commit_oid, &scopes)?;
    s3_purge::queue(&repo, &purge_candidates)?;
    let local_ref = format!("refs/heads/{}", task.branch);
    let old_local_oid = repo.optional_oid(&local_ref)?;
    repo.run([
        "update-ref",
        "-m",
        &format!("workspace-mgr publish for {}", task.task_id),
        &local_ref,
        &commit_oid,
        old_local_oid.as_deref().unwrap_or(ZERO_OID),
    ])?;
    if task.kind == TaskKind::Infrastructure {
        repo.run(["read-tree", &commit_oid])?;
    }
    let refspec = format!("{commit_oid}:refs/heads/{}", task.branch);
    repo.run(["push", "--porcelain", &task.remote, &refspec])?;
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
        &format!("record push for {}", task.task_id),
        &format!("refs/remotes/{}/{}", task.remote, task.branch),
        &commit_oid,
    ])?;
    let mut purge = s3_purge::purge_pending(&repo, &config, &task.remote)?;
    purge.queued = purge_candidates;
    report.storage["purge"] = serde_json::to_value(purge)
        .map_err(|error| Error::message(format!("failed to encode S3 purge report: {error}")))?;
    report.status = "pushed".to_owned();
    report.commit_oid = Some(commit_oid);
    report.remote_oid = Some(observed);
    report.push = Some("explicit refspec pushed and remote object ID verified".to_owned());
    Ok(report)
}

fn local_path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(Error::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

struct PrivateIndexPolicy<'a> {
    task: &'a ResolvedTask,
    published_task_path: Option<&'a str>,
    large_file_threshold: u64,
    automatic_s3: &'a [String],
}

fn validate_private_index(
    repo: &GitRepo,
    index: &Path,
    base_oid: &str,
    scopes: &[String],
    paths: &[String],
    policy: &PrivateIndexPolicy<'_>,
) -> Result<()> {
    let escaped: Vec<String> = paths
        .iter()
        .filter(|path| !allowed(path, scopes))
        .cloned()
        .collect();
    if !escaped.is_empty() {
        return Err(Error::message(format!(
            "private index escaped the declared scope: {}",
            escaped.join(", ")
        )));
    }
    check_staged_entry_modes(repo, index, paths, policy)?;
    check_large_files(repo, scopes, base_oid, policy)?;
    repo.run_with_index(
        index,
        ["diff", "--cached", "--check", base_oid, "--"],
        None,
        true,
    )?;
    check_task_documentation(repo, index, paths, policy)?;
    Ok(())
}

fn stage_scopes(repo: &GitRepo, index: &Path, scopes: &[String]) -> Result<()> {
    let mut present = Vec::new();
    for scope in scopes {
        if storage::is_local(repo, scope)? {
            continue;
        }
        let exists = match fs::symlink_metadata(resolved_under(&repo.root, scope)) {
            Ok(_) => true,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                false
            }
            Err(source) => {
                return Err(Error::Io {
                    path: resolved_under(&repo.root, scope),
                    source,
                });
            }
        };
        let tracked = !repo
            .run_with_index(index, ["ls-files", "-z", "--", scope], None, true)?
            .stdout
            .is_empty();
        if exists || tracked {
            present.push(scope.clone());
        }
    }
    if present.is_empty() {
        return Ok(());
    }
    let mut add = vec!["add".to_owned(), "-A".to_owned(), "--".to_owned()];
    add.extend(present);
    repo.run_with_index(index, add, None, true)?;
    Ok(())
}

fn remove_stored_outputs_from_index(
    repo: &GitRepo,
    index: &Path,
    outputs: &std::collections::BTreeMap<String, Vec<String>>,
) -> Result<()> {
    remove_output_paths_from_index(repo, index, outputs.values().flatten())
}

fn remove_output_paths_from_index<'a>(
    repo: &GitRepo,
    index: &Path,
    outputs: impl IntoIterator<Item = &'a String>,
) -> Result<()> {
    let mut tracked = BTreeSet::new();
    for output in outputs {
        let literal = format!(":(literal){output}");
        let listed = repo.run_with_index(index, ["ls-files", "-z", "--", &literal], None, true)?;
        tracked.extend(
            listed
                .stdout
                .split('\0')
                .filter(|path| !path.is_empty())
                .map(ToOwned::to_owned),
        );
    }
    for path in tracked {
        repo.run_with_index(
            index,
            ["update-index", "--force-remove", "--", &path],
            None,
            true,
        )?;
    }
    Ok(())
}

fn validate_checkout(
    repo: &GitRepo,
    task: &ResolvedTask,
    scopes: &[String],
    options: &TransactionOptions,
) -> Result<()> {
    let head = repo.current_branch()?;
    if task.kind == TaskKind::Infrastructure {
        if head.as_deref() != Some(&task.branch) {
            return Err(Error::message(format!(
                "infrastructure task must run in its isolated worktree on {:?}; current branch is {:?}",
                task.branch,
                head.as_deref().unwrap_or("detached HEAD")
            )));
        }
        let root = repo.root.canonicalize().map_err(|source| Error::Io {
            path: repo.root.clone(),
            source,
        })?;
        let worktrees = repo.branch_worktrees(&task.branch)?;
        if worktrees.len() != 1
            || worktrees[0].canonicalize().map_err(|source| Error::Io {
                path: worktrees[0].clone(),
                source,
            })? != root
        {
            return Err(Error::message(
                "infrastructure branch is not mounted only in the current isolated worktree",
            ));
        }
        let staged = repo.run_unchecked(["diff", "--cached", "--quiet", "--"])?;
        if staged.code != 0 && staged.code != 1 {
            return Err(Error::message(
                "failed to inspect the infrastructure worktree index",
            ));
        }
        if staged.code == 1 {
            return Err(Error::message(
                "infrastructure worktree index has staged changes; unstage them before workspace-mgr publication",
            ));
        }
        let tracked = repo.run(["diff", "--name-only", "--no-renames", "-z", "HEAD", "--"])?;
        let untracked = repo.run(["ls-files", "--others", "--exclude-standard", "-z", "--"])?;
        let mut escaped = tracked
            .stdout
            .split('\0')
            .chain(untracked.stdout.split('\0'))
            .filter(|path| !path.is_empty() && !allowed(path, scopes))
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        escaped.sort();
        escaped.dedup();
        if !escaped.is_empty() {
            return Err(Error::message(format!(
                "infrastructure worktree has changes outside its declared scope: {}",
                escaped.join(", ")
            )));
        }
        return Ok(());
    }
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
    let additional = validate_additional_scopes(task.task_path.as_deref(), additional)?;
    let scopes = task
        .task_path
        .iter()
        .cloned()
        .chain(additional.iter().map(|entry| entry.path.clone()))
        .collect();
    Ok((scopes, additional))
}

pub(crate) fn task_state_dir(common_dir: &Path, task: &ResolvedTask) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(task.task_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(task.branch.as_bytes());
    common_dir
        .join("workspace-mgr/state")
        .join(encode_lower(hasher.finalize()))
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
        let path = lock_dir.join(format!("{}.lock", encode_lower(hasher.finalize())));
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

/// The largest pathspec argument payload handed to one `git` invocation. Modes
/// are read for every changed path at once rather than one process per path,
/// because on a large publication the per-path process cost dominates the
/// transaction; the budget keeps the argument list well inside every platform's
/// limit without reintroducing that cost.
const PATHSPEC_ARGUMENT_BYTES: usize = 96 * 1024;

fn pathspec_batches(paths: &[String]) -> Vec<&[String]> {
    let mut batches = Vec::new();
    let mut start = 0;
    let mut budget = 0;
    for (position, path) in paths.iter().enumerate() {
        let cost = path.len() + 1;
        if position > start && budget + cost > PATHSPEC_ARGUMENT_BYTES {
            batches.push(&paths[start..position]);
            start = position;
            budget = 0;
        }
        budget += cost;
    }
    if start < paths.len() {
        batches.push(&paths[start..]);
    }
    batches
}

/// Refuses the two staged entry modes that point at content no other checkout
/// has: a nested Git checkout (gitlink) and a symbolic link whose target leaves
/// the repository. Both are read from one staged-mode listing per batch of
/// paths, so a publication pays one process per batch rather than two per path.
///
/// Only what reaches the private index is inspected. Content removed from it
/// before validation — an S3-placed boundary or an `untrack`ed path — is not
/// classified here; see `escapes_repository`.
fn check_staged_entry_modes(
    repo: &GitRepo,
    index: &Path,
    paths: &[String],
    policy: &PrivateIndexPolicy<'_>,
) -> Result<()> {
    let destination = retained_content_destination(policy.task);
    for batch in pathspec_batches(paths) {
        let mut args = vec![
            "ls-files".to_owned(),
            "--stage".to_owned(),
            "-z".to_owned(),
            "--".to_owned(),
        ];
        args.extend(batch.iter().cloned());
        let output = repo.run_with_index(index, args, None, true)?;
        for entry in output.stdout.split('\0').filter(|entry| !entry.is_empty()) {
            let Some((attributes, path)) = entry.split_once('\t') else {
                continue;
            };
            let mut fields = attributes.split_whitespace();
            let mode = fields.next();
            if mode == Some("160000") {
                return Err(Error::message(format!(
                    "{path:?} is staged as a nested Git checkout/gitlink; ignore the checkout instead, or copy the files this task needs into {destination}"
                )));
            }
            if mode != Some("120000") {
                continue;
            }
            let Some(oid) = fields.next() else {
                continue;
            };
            let target = repo
                .run_with_index(index, ["cat-file", "blob", oid], None, true)?
                .stdout;
            let target = target.trim();
            if escapes_repository(path, target) {
                return Err(Error::message(format!(
                    "{path:?} is a symbolic link to {target:?}, which is outside the repository; keep the work inside {destination} and copy retained content into it instead of linking to it"
                )));
            }
        }
    }
    Ok(())
}

/// Where a refusal tells this task to keep content it must retain. An
/// infrastructure task has no repository task directory, so naming one would
/// send it looking for a path it does not have.
fn retained_content_destination(task: &ResolvedTask) -> &'static str {
    match task.kind {
        TaskKind::Deliverable => "the task directory",
        TaskKind::Infrastructure => "a declared scope",
    }
}

/// Classifies a staged symbolic link from its recorded target alone. This is a
/// workspace-discipline guard, not a security boundary: it never reads the
/// filesystem, so an unreadable or ignored directory is never inspected and a
/// dangling target is classified like any other. It also sees only the staged
/// tree, so a link inside a boundary placed in S3 or kept local with `untrack`
/// is never classified, because that content is removed from the private index
/// before validation runs.
fn escapes_repository(link_path: &str, target: &str) -> bool {
    if target.starts_with('/') || target.starts_with('~') {
        return true;
    }
    let bytes = target.as_bytes();
    let windows_drive = bytes.first().is_some_and(u8::is_ascii_alphabetic)
        && bytes.get(1) == Some(&b':')
        && matches!(bytes.get(2), None | Some(b'/') | Some(b'\\'));
    if target.starts_with("\\\\") || windows_drive {
        return true;
    }
    let mut depth = link_path.split('/').count() - 1;
    for component in target.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if depth == 0 {
                    return true;
                }
                depth -= 1;
            }
            _ => depth += 1,
        }
    }
    false
}

fn deliverable_task_path(task: &ResolvedTask) -> Option<&str> {
    if task.kind != TaskKind::Deliverable {
        return None;
    }
    task.task_path.as_deref()
}

/// The task's own control surface: its README, its manifest, and its ignore
/// rules. Every other path inside the task directory is content the task
/// produced.
fn is_housekeeping_name(name: &str) -> bool {
    name == "README.md" || name == TASK_MANIFEST_NAME || name == ".gitignore"
}

fn is_markdown(path: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
}

fn inside(task_path: &str, path: &str) -> bool {
    path.strip_prefix(task_path)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Whether a changed path is content the task itself produced. Only the task's
/// own directory counts: a user-authorized additional scope belongs to the
/// request that authorized it, not to the task's record. A storage pointer or
/// placement record stands in for the boundary it addresses, so routing a
/// result to S3 does not make it invisible here — otherwise the guard would be
/// strongest for a small CSV and absent for the expensive dataset it exists to
/// protect.
fn is_task_content(task_path: &str, path: &str) -> bool {
    if !inside(task_path, path) {
        return false;
    }
    let boundary = path
        .strip_suffix(PLACEMENT_SUFFIX)
        .or_else(|| path.strip_suffix(".dvc"))
        .unwrap_or(path);
    let name = boundary.rsplit('/').next().unwrap_or(boundary);
    !name.is_empty() && !is_housekeeping_name(name)
}

/// The task's own documentation in the projected tree, and every task path that
/// projection still tracks. The documentation is every tracked Markdown file
/// inside the task directory, except a README that is still the creation
/// scaffold. The product does not prescribe which files hold the record, only
/// that the task has one.
struct TaskProjection {
    documentation: Vec<String>,
    tracked: BTreeSet<String>,
}

impl TaskProjection {
    fn resolve(repo: &GitRepo, index: &Path, task_path: &str) -> Result<Self> {
        let listed = repo.run_with_index(
            index,
            ["ls-files", "--stage", "-z", "--", task_path],
            None,
            true,
        )?;
        let scaffold_readme = format!("{task_path}/README.md");
        let mut projection = Self {
            documentation: Vec::new(),
            tracked: BTreeSet::new(),
        };
        for entry in listed.stdout.split('\0').filter(|entry| !entry.is_empty()) {
            let Some((attributes, path)) = entry.split_once('\t') else {
                continue;
            };
            projection.tracked.insert(path.to_owned());
            if !is_markdown(path) {
                continue;
            }
            if path == scaffold_readme {
                let Some(oid) = attributes.split_whitespace().nth(1) else {
                    continue;
                };
                let content = repo
                    .run_with_index(index, ["cat-file", "blob", oid], None, true)?
                    .stdout;
                if is_scaffold_readme(&content) {
                    continue;
                }
            }
            projection.documentation.push(path.to_owned());
        }
        Ok(projection)
    }
}

/// Whether a README is still exactly what `task create` wrote. Only the fixed
/// directory map is compared, plus the shape of the heading and purpose line
/// the command interpolated. The title and purpose live in a tracked manifest
/// the agent may edit, so comparing against a rendering of them would let one
/// unrelated manifest edit silently retire the guard for that task.
fn is_scaffold_readme(content: &str) -> bool {
    let Some(prelude) = content.strip_suffix(&task_readme_directory_map()) else {
        return false;
    };
    let mut lines = prelude.split('\n');
    let (Some(heading), Some(""), Some(purpose), Some(""), Some(""), None) = (
        lines.next(),
        lines.next(),
        lines.next(),
        lines.next(),
        lines.next(),
        lines.next(),
    ) else {
        return false;
    };
    heading.starts_with("# ") && !purpose.is_empty()
}

/// Whether this publication adds or changes content inside the task directory.
/// A path the projection no longer tracks is a deletion: a publication that
/// only retires content cannot be an undocumented addition, so it is not judged
/// here. Content on its way to S3 is counted from the placement decision,
/// because its payload never reaches the private index.
fn publishes_task_content(
    task_path: &str,
    paths: &[String],
    automatic_s3: &[String],
    tracked: &BTreeSet<String>,
) -> bool {
    automatic_s3
        .iter()
        .any(|path| is_task_content(task_path, path))
        || paths
            .iter()
            .any(|path| is_task_content(task_path, path) && tracked.contains(path.as_str()))
}

/// Whether this publication changes content inside the task directory in any
/// direction, including retiring it. Retiring a result is a decision worth
/// recording, so the advisory warning counts it; the refusal does not.
fn changes_task_content(task_path: &str, paths: &[String], automatic_s3: &[String]) -> bool {
    paths
        .iter()
        .chain(automatic_s3.iter())
        .any(|path| is_task_content(task_path, path))
}

fn removes_task_documentation(
    task_path: &str,
    paths: &[String],
    tracked: &BTreeSet<String>,
) -> bool {
    paths.iter().any(|path| {
        inside(task_path, path) && is_markdown(path) && !tracked.contains(path.as_str())
    })
}

fn check_task_documentation(
    repo: &GitRepo,
    index: &Path,
    paths: &[String],
    policy: &PrivateIndexPolicy<'_>,
) -> Result<()> {
    let Some(task_path) = deliverable_task_path(policy.task) else {
        return Ok(());
    };
    // A transaction that touches nothing inside the task directory cannot fail
    // this check, so it does not pay for the projection listing either.
    if !changes_task_content(task_path, paths, policy.automatic_s3) {
        return Ok(());
    }
    let projection = TaskProjection::resolve(repo, index, task_path)?;
    if !publishes_task_content(task_path, paths, policy.automatic_s3, &projection.tracked) {
        return Ok(());
    }
    if !projection.documentation.is_empty() {
        return Ok(());
    }
    if removes_task_documentation(task_path, paths, &projection.tracked) {
        return Err(Error::message(format!(
            "deliverable task {task_path:?} removes the last of its own documentation while publishing content; a published record is durable, so edit it instead of deleting it, or keep another Markdown file recording this task's decisions, process, tools, and hard-to-reproduce results inside {task_path:?}"
        )));
    }
    Err(Error::message(format!(
        "deliverable task {task_path:?} publishes content but documents nothing; record this task's decisions, process, tools, and hard-to-reproduce results in Markdown files of your choosing inside {task_path:?}, then list them in its README directory map"
    )))
}

fn documentation_warning(
    task_path: &str,
    documentation: &[String],
    paths: &[String],
    automatic_s3: &[String],
) -> Option<TransactionWarning> {
    if !changes_task_content(task_path, paths, automatic_s3) {
        return None;
    }
    if paths
        .iter()
        .any(|path| documentation.iter().any(|entry| entry == path))
    {
        return None;
    }
    Some(TransactionWarning {
        code: "task-record-unchanged".to_owned(),
        message: format!(
            "this publication changes task content but none of the task documentation in {task_path}; record the decision, tool, process step, or hard-to-reproduce result when the work produced one, and ignore this warning otherwise"
        ),
    })
}

/// The new content this publication adds inside the task directory, as a file
/// count and a byte total.
///
/// Content routed to S3 is counted from the placement decision and measured on
/// disk, because its payload never reaches the private index; the guard would
/// otherwise be loudest for a directory of small notes and silent for the
/// dataset beside them. An explicitly selected boundary appears as its pointer
/// and counts as one file: that placement was already a deliberate decision,
/// which is exactly what this warning asks for.
///
/// An automatically placed boundary is counted from the decision alone. By the
/// time `publish` reaches this point the pointer it writes is staged too, so
/// counting both would make `publish` report more content than the `plan` that
/// preceded it for a task nothing had touched in between.
fn bulk_publication_volume(
    repo: &GitRepo,
    index: &Path,
    base_oid: &str,
    task_path: &str,
    automatic_s3: &[String],
) -> Result<(u64, u64)> {
    let added = added_paths(repo, index, base_oid)?;
    let automatic_metadata: BTreeSet<String> = automatic_s3
        .iter()
        .flat_map(|path| [format!("{path}.dvc"), format!("{path}{PLACEMENT_SUFFIX}")])
        .collect();
    let mut files = 0;
    let mut bytes = 0;
    for path in added
        .iter()
        .filter(|path| !automatic_metadata.contains(path.as_str()))
        .chain(automatic_s3.iter())
        .filter(|path| is_task_content(task_path, path))
    {
        files += 1;
        let absolute = resolved_under(&repo.root, path);
        // A staged path always exists, and an automatic S3 candidate was just
        // measured; a path that disappeared under a concurrent edit simply adds
        // nothing to the total rather than failing an advisory count.
        if let Ok(metadata) = fs::symlink_metadata(&absolute) {
            if metadata.is_file() && !metadata.file_type().is_symlink() {
                bytes += metadata.len();
            }
        }
    }
    Ok((files, bytes))
}

fn bulk_publication_warning(files: u64, bytes: u64) -> Option<TransactionWarning> {
    if files <= BULK_PUBLICATION_FILES && bytes <= BULK_PUBLICATION_BYTES {
        return None;
    }
    Some(TransactionWarning {
        code: "bulk-publication".to_owned(),
        message: format!(
            "this publication adds {files} new files and {bytes} bytes of new content inside the task directory, above the {BULK_PUBLICATION_FILES} file or {BULK_PUBLICATION_MIB} MiB ({BULK_PUBLICATION_BYTES} bytes) threshold; confirm that they are retained inputs, tools, evidence, or deliverables, and otherwise ignore the regenerable ones with the narrowest rule or keep bulk content on this machine with `workspace-mgr untrack`, then re-plan; this is a check rather than a refusal, so ignore it when the content is genuinely retained"
        ),
    })
}

/// The paths this publication adds, as opposed to the ones it edits, moves, or
/// retires. The bulk check is about content arriving in the task, so an edit to
/// a file the task already published is not part of it, and neither is the same
/// file under a new path: `workspace-mgr task rename` moves every published
/// file at once, which would otherwise read as the largest arrival the task
/// ever made.
fn added_paths(repo: &GitRepo, index: &Path, base: &str) -> Result<Vec<String>> {
    let output = repo.run_with_index(
        index,
        [
            "diff",
            "--cached",
            "--name-only",
            "--find-renames",
            "--diff-filter=A",
            "-z",
            base,
            "--",
        ],
        None,
        true,
    )?;
    Ok(output
        .stdout
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

fn check_large_files(
    repo: &GitRepo,
    scopes: &[String],
    base_oid: &str,
    policy: &PrivateIndexPolicy<'_>,
) -> Result<()> {
    for relative in repo.visible_paths(scopes)? {
        if storage::is_local(repo, &relative)? {
            continue;
        }
        let absolute = resolved_under(&repo.root, &relative);
        let metadata = fs::symlink_metadata(&absolute).at(&absolute)?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() <= policy.large_file_threshold
        {
            continue;
        }
        if policy.automatic_s3.contains(&relative) {
            continue;
        }
        if storage::explicit_target(repo, &relative)? == Some(crate::config::StorageTarget::Git) {
            continue;
        }
        let history_path = published_history_path(
            &relative,
            policy.task.task_path.as_deref(),
            policy.published_task_path,
        );
        let object = format!("{base_oid}:{history_path}");
        if repo.run_unchecked(["cat-file", "-e", &object])?.success() {
            continue;
        }
        return Err(Error::message(format!(
            "retained file {relative:?} is larger than {} bytes and has no valid placement; run `workspace-mgr storage set {relative} --to git|s3 --reason <reason>`",
            policy.large_file_threshold
        )));
    }
    Ok(())
}

/// How many ignored paths a report lists beside the exact `ignored_entries`
/// count. `git status --ignored` collapses an ignored directory to one entry,
/// but a pattern rule such as `*.log` yields one entry per file, and the report
/// is emitted at every plan and publication. The list stays a sample the agent
/// can read; the count stays complete.
const REPORTED_IGNORED_PATHS: usize = 50;

/// Every path inside the scopes that Git ignores. An ignored directory arrives
/// collapsed to one entry, which is what keeps this from descending into a tree
/// the checkout may not even be able to read.
fn ignored_paths(repo: &GitRepo, index: &Path, scopes: &[String]) -> Result<Vec<String>> {
    let mut args = vec![
        "status".to_owned(),
        "--ignored".to_owned(),
        "--short".to_owned(),
        "-z".to_owned(),
        "--untracked-files=normal".to_owned(),
        "--".to_owned(),
    ];
    args.extend(scopes.iter().cloned());
    let mut paths: Vec<String> = repo
        .run_with_index(index, args, None, true)?
        .stdout
        .split('\0')
        .filter_map(|entry| entry.strip_prefix("!! "))
        .map(ToOwned::to_owned)
        .collect();
    paths.sort();
    Ok(paths)
}

fn count_ignored(repo: &GitRepo, index: &Path, scopes: &[String]) -> Result<(usize, Vec<String>)> {
    Ok(summarize_ignored(ignored_paths(repo, index, scopes)?))
}

fn summarize_ignored(mut paths: Vec<String>) -> (usize, Vec<String>) {
    let entries = paths.len();
    paths.truncate(REPORTED_IGNORED_PATHS);
    (entries, paths)
}

/// How many machine-local ignore rules a refusal names before it counts the
/// rest. One untracked rule can hide a whole tree, so the message lists enough
/// to diagnose the cause and then stops.
const REPORTED_UNTRACKED_IGNORE_SOURCES: usize = 5;

/// One decided ignore rule as `git check-ignore -v -z` reports it: the file the
/// rule lives in, the pattern, and the path it hid. The line number is parsed
/// and dropped, because the fix is to move the rule, not to edit that line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct IgnoreRule<'a> {
    source: &'a str,
    pattern: &'a str,
    path: &'a str,
}

fn parse_ignore_rules(raw: &str) -> Vec<IgnoreRule<'_>> {
    let mut fields = raw.split('\0');
    let mut rules = Vec::new();
    loop {
        let (Some(source), Some(_line), Some(pattern), Some(path)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return rules;
        };
        if source.is_empty() || path.is_empty() {
            return rules;
        }
        rules.push(IgnoreRule {
            source,
            pattern,
            path,
        });
    }
}

/// The rules this publication does not carry: the user's global excludes,
/// `.git/info/exclude`, or an ignore file whose matching bytes are not in the
/// projected publication. Git resolves the deepest matching ignore file first
/// and only then falls back to `.git/info/exclude` and the global excludes, so
/// a repository or task rule that also matches is the reported source and the
/// common case never reaches here.
fn machine_local_ignores<'a, 'b>(
    rules: &'b [IgnoreRule<'a>],
    carried: &BTreeSet<String>,
) -> Vec<&'b IgnoreRule<'a>> {
    rules
        .iter()
        // A negated pattern decides that the path is not ignored at all, so it
        // hides nothing and names no rule to move.
        .filter(|rule| !rule.pattern.starts_with('!'))
        .filter(|rule| !is_product_ignore_rule(rule))
        .filter(|rule| !carried.contains(rule.source))
        .collect()
}

/// A rule the product itself writes into the root ignore file. `init`
/// regenerates that file in every clone from the installed CLI, so such a rule
/// is carried wherever the product is, including in the window between `init`
/// and the publication of the generated file. Without this, the first plan of
/// every freshly initialized repository would be refused over a stray
/// `.DS_Store` hidden by the product's own wildcard, and the remedies the
/// refusal offers would all regenerate the same unpublished file.
fn is_product_ignore_rule(rule: &IgnoreRule<'_>) -> bool {
    rule.source == ROOT_IGNORE_NAME && PRODUCT_IGNORE_RULES.contains(&rule.pattern)
}

fn untracked_ignore_message(rules: &[&IgnoreRule<'_>], destination: &str) -> String {
    let listed = rules
        .iter()
        .take(REPORTED_UNTRACKED_IGNORE_SOURCES)
        .map(|rule| {
            format!(
                "{:?} by rule {:?} in {:?}",
                rule.path, rule.pattern, rule.source
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let remaining = rules
        .len()
        .saturating_sub(REPORTED_UNTRACKED_IGNORE_SOURCES);
    let more = if remaining == 0 {
        String::new()
    } else {
        format!(", and {remaining} more")
    };
    format!(
        "this task's scopes hold content that only an ignore rule this publication does not carry hides, so the rule keeps it out of every other clone and out of review: {listed}{more}; put the rule in {destination}, which stays inside this task's write boundary and reaches review with the task, or let the content be published when the task retains it; a rule the whole repository needs belongs in `{REPOSITORY_IGNORE_MODULE}`, which `workspace-mgr init` imports into the root `{ROOT_IGNORE_NAME}`, but both are shared root paths whose change needs the user's explicit authorization and counts only once it is published on the shared branch"
    )
}

/// Where a refusal tells this task to put an ignore rule it must keep. A
/// deliverable owns one directory; an infrastructure task owns only the paths
/// its manifest declares.
fn ignore_rule_destination(task: &ResolvedTask) -> String {
    match deliverable_task_path(task) {
        Some(task_path) => format!("`{task_path}/.gitignore`"),
        None => "the `.gitignore` of a declared scope".to_owned(),
    }
}

/// Refuses content that Git hides because of a rule this publication does not
/// carry. Such a rule is invisible to every other clone and to review, so the
/// file it hides is in neither of the two states a task's content may be in:
/// selected for publication, or ignored by a rule the repository carries.
///
/// Returns the ignored listing it produced, so the report does not pay for a
/// second walk of the same scopes when nothing between the two points can have
/// changed it.
///
/// Resolution is batched: one ignore listing, one rule resolution over all of
/// its paths, one expansion of the directories that resolution could not
/// explain, and one carried-source listing.
fn check_untracked_ignore_sources(
    repo: &GitRepo,
    index: &Path,
    scopes: &[String],
    task: &ResolvedTask,
) -> Result<Vec<String>> {
    let ignored = ignored_paths(repo, index, scopes)?;
    if ignored.is_empty() {
        return Ok(ignored);
    }
    let mut resolved = resolve_ignore_rules(repo, index, &ignored)?;
    // `git status --ignored` collapses a directory whose every entry is ignored
    // into the directory itself, and a file-level rule such as `*.log` does not
    // match that directory, so the entry arrives with no rule at all — which is
    // exactly the shape this refusal exists for: a `results/` or `logs/` tree
    // of by-products hidden by one personal rule. Only those directories are
    // expanded, and `--ignored=matching` leaves a directory that a directory
    // rule already covers collapsed, so a wholesale-ignored tree is still never
    // walked.
    let decided: BTreeSet<String> = parse_ignore_rules(&resolved)
        .into_iter()
        .map(|rule| rule.path.to_owned())
        .collect();
    let undecided: Vec<String> = ignored
        .iter()
        .filter(|path| path.ends_with('/') && !decided.contains(path.as_str()))
        .cloned()
        .collect();
    if !undecided.is_empty() {
        let expanded = matching_ignored_paths(repo, index, &undecided)?;
        if !expanded.is_empty() {
            let inner = resolve_ignore_rules(repo, index, &expanded)?;
            resolved.push_str(&inner);
        }
    }
    let rules = parse_ignore_rules(&resolved);
    let carried = carried_ignore_sources(repo, index, &rules)?;
    let machine_local = machine_local_ignores(&rules, &carried);
    if machine_local.is_empty() {
        return Ok(ignored);
    }
    Err(Error::message(untracked_ignore_message(
        &machine_local,
        &ignore_rule_destination(task),
    )))
}

/// The rule that decides each of these paths, in `git check-ignore -v -z`
/// framing, resolved in one process for the whole listing.
fn resolve_ignore_rules(repo: &GitRepo, index: &Path, paths: &[String]) -> Result<String> {
    let mut request = paths.join("\0");
    request.push('\0');
    let resolved = repo.run_with_index(
        index,
        ["check-ignore", "-v", "-z", "--no-index", "--stdin"],
        Some(&request),
        false,
    )?;
    // 0 reports at least one decided rule; 1 reports none, which a concurrent
    // edit can produce between the two commands. Anything else is a failure.
    if resolved.code != 0 && resolved.code != 1 {
        return Err(Error::message(format!(
            "cannot resolve the Git ignore rules for this task: {}",
            resolved.stderr.trim()
        )));
    }
    Ok(resolved.stdout)
}

/// The individual entries hidden inside directories the collapsed listing could
/// not explain. `--ignored=matching` reports only paths that a pattern matches,
/// so every entry it returns is one `check-ignore` can decide.
fn matching_ignored_paths(
    repo: &GitRepo,
    index: &Path,
    directories: &[String],
) -> Result<Vec<String>> {
    let literal: Vec<String> = directories
        .iter()
        .map(|path| format!(":(literal){}", path.trim_end_matches('/')))
        .collect();
    let mut paths = BTreeSet::new();
    for batch in pathspec_batches(&literal) {
        let mut args = vec![
            "status".to_owned(),
            "--ignored=matching".to_owned(),
            "--short".to_owned(),
            "-z".to_owned(),
            "--untracked-files=all".to_owned(),
            "--".to_owned(),
        ];
        args.extend(batch.iter().cloned());
        paths.extend(
            repo.run_with_index(index, args, None, true)?
                .stdout
                .split('\0')
                .filter_map(|entry| entry.strip_prefix("!! "))
                .map(ToOwned::to_owned),
        );
    }
    Ok(paths.into_iter().collect())
}

/// The ignore files among these rules whose rules this publication carries.
///
/// Being a path in the projected index is not enough. `git check-ignore`
/// resolves against the work tree, so a rule written into a tracked ignore file
/// and never staged decides here while reaching no other clone — which is how
/// the remedy for this very refusal could otherwise silence it without
/// publishing anything. The publication carries a source only when its
/// work-tree bytes are the bytes the projected index holds for that path, which
/// is true of a task's own `.gitignore` because the scopes are staged from the
/// work tree, and false of a local edit to a shared root file.
///
/// A source outside the work tree — the global excludes file, or anything below
/// `.git` — can never be carried, so it is never asked about.
fn carried_ignore_sources(
    repo: &GitRepo,
    index: &Path,
    rules: &[IgnoreRule<'_>],
) -> Result<BTreeSet<String>> {
    let mut candidates = BTreeSet::new();
    for rule in rules {
        if Path::new(rule.source).is_absolute() || rule.source.starts_with(".git/") {
            continue;
        }
        candidates.insert(format!(":(literal){}", rule.source));
    }
    if candidates.is_empty() {
        return Ok(BTreeSet::new());
    }
    let candidates: Vec<String> = candidates.into_iter().collect();
    let mut carried = BTreeSet::new();
    for batch in pathspec_batches(&candidates) {
        let mut listing = vec!["ls-files".to_owned(), "-z".to_owned(), "--".to_owned()];
        listing.extend(batch.iter().cloned());
        carried.extend(
            repo.run_with_index(index, listing, None, true)?
                .stdout
                .split('\0')
                .filter(|path| !path.is_empty())
                .map(ToOwned::to_owned),
        );
        // `status` refreshes the index against the work tree, so the
        // work-tree column is a content comparison rather than a stat one.
        let mut compare = vec![
            "status".to_owned(),
            "--porcelain".to_owned(),
            "-z".to_owned(),
            "--untracked-files=no".to_owned(),
            "--no-renames".to_owned(),
            "--".to_owned(),
        ];
        compare.extend(batch.iter().cloned());
        for entry in repo
            .run_with_index(index, compare, None, true)?
            .stdout
            .split('\0')
        {
            // `XY <path>`: X is this publication against the base tree, which
            // is the whole point of the publication; Y is the work tree against
            // this publication, which is what must be empty.
            let Some(path) = entry.get(3..) else { continue };
            if entry.as_bytes().get(1).is_some_and(|state| *state != b' ') {
                carried.remove(path);
            }
        }
    }
    Ok(carried)
}

fn build_commit_message(
    message: &str,
    task_id: &str,
    scopes: &[String],
    authorizations: &[AdditionalScope],
) -> String {
    let mut lines = vec![
        message.to_owned(),
        String::new(),
        format!("Workspace-Task: {task_id}"),
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

pub(crate) fn validate_remote_task_identity(
    repo: &GitRepo,
    task: &ResolvedTask,
    remote_oid: &str,
) -> Result<()> {
    if commit_belongs_to_task(repo, remote_oid, task)? {
        return Ok(());
    }
    Err(Error::message(format!(
        "target branch {:?} already belongs to another task; choose a different task slug",
        task.branch
    )))
}

fn commit_belongs_to_task(repo: &GitRepo, oid: &str, task: &ResolvedTask) -> Result<bool> {
    let message = repo.run(["show", "-s", "--format=%B", oid])?.stdout;
    Ok(message.lines().any(|line| {
        line.strip_prefix("Workspace-Task:")
            .is_some_and(|value| value.trim() == task.task_id)
    }))
}

pub fn task_status(start: &Path, manifest: Option<&Path>) -> Result<TaskStatus> {
    let repo = match manifest {
        Some(path) => GitRepo::discover_for_manifest(path)?,
        None => GitRepo::discover(start)?,
    };
    let config = Config::load_compatible(&repo)?;
    let path = match manifest {
        Some(path) => path.to_path_buf(),
        None => ResolvedTask::discover(&repo, start)?,
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
        kind: task.kind,
        task_id: task.task_id,
        slug: task.slug,
        title: task.title,
        purpose: task.purpose,
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
    pub kind: TaskKind,
    pub task_id: String,
    pub slug: String,
    pub title: String,
    pub purpose: String,
    pub manifest: String,
    pub branch: String,
    pub remote: String,
    pub base_branch: String,
    pub scopes: Vec<String>,
    pub working_changes: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scaffold::task_readme;

    fn owned(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| (*path).to_owned()).collect()
    }

    #[test]
    fn classifies_symbolic_link_targets_from_the_recorded_target_alone() {
        for (link, target) in [
            ("task/link", "/private/tmp/scratch"),
            ("task/link", "~/scratch"),
            ("task/link", "C:\\scratch"),
            ("task/link", "\\\\server\\share"),
            ("task/link", "../../outside"),
            ("task/link", "./../.."),
            ("link", "../outside"),
            ("task/sub/link", "../../../outside"),
        ] {
            assert!(
                escapes_repository(link, target),
                "{target:?} from {link:?} must be refused"
            );
        }
        for (link, target) in [
            ("task/link", "notes.md"),
            ("task/link", "./notes.md"),
            ("task/link", "sub/notes.md"),
            ("task/link", "sub/"),
            ("task/link", "../sibling"),
            ("task/sub/link", "../../outside"),
            ("task/link", "a:b.txt"),
            ("task/link", ""),
        ] {
            assert!(
                !escapes_repository(link, target),
                "{target:?} from {link:?} must be allowed"
            );
        }
    }

    const TASK: &str = "20260829-170100-task";

    fn tracked(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|path| (*path).to_owned()).collect()
    }

    #[test]
    fn separates_task_housekeeping_from_task_content() {
        for path in [
            "20260829-170100-task/README.md",
            "20260829-170100-task/.workspace-mgr-task.toml",
            "20260829-170100-task/.gitignore",
            "20260829-170101-other/tools/run.py",
            "shared-area/config.yaml",
            "docs/guide.md",
            "20260829-170100-task",
        ] {
            assert!(!is_task_content(TASK, path), "{path} must not be content");
        }
        for path in [
            "20260829-170100-task/notes.md",
            "20260829-170100-task/tools/run.py",
            "20260829-170100-task/data.bin",
            "20260829-170100-task/data.bin.dvc",
            "20260829-170100-task/data.bin.workspace-mgr-storage.toml",
            "20260829-170100-task/dataset.dvc",
        ] {
            assert!(is_task_content(TASK, path), "{path} must be task content");
        }
    }

    #[test]
    fn a_storage_pointer_keeps_the_boundary_housekeeping_classification() {
        for path in [
            "20260829-170100-task/README.md.dvc",
            "20260829-170100-task/.gitignore.workspace-mgr-storage.toml",
        ] {
            assert!(!is_task_content(TASK, path), "{path} must not be content");
        }
    }

    #[test]
    fn only_additions_and_edits_count_as_publishing_task_content() {
        let projected = tracked(&[
            "20260829-170100-task/README.md",
            "20260829-170100-task/tools/run.py",
        ]);
        assert!(!publishes_task_content(TASK, &[], &[], &projected));
        assert!(!publishes_task_content(
            TASK,
            &owned(&[
                "20260829-170100-task/README.md",
                "20260829-170100-task/.workspace-mgr-task.toml",
            ]),
            &[],
            &projected,
        ));
        assert!(publishes_task_content(
            TASK,
            &owned(&["20260829-170100-task/tools/run.py"]),
            &[],
            &projected,
        ));
        // A path the projection no longer tracks is a deletion.
        assert!(!publishes_task_content(
            TASK,
            &owned(&[
                "20260829-170100-task/notes.md",
                "20260829-170100-task/result.csv",
            ]),
            &[],
            &projected,
        ));
        // Content on its way to S3 never reaches the index, so it is counted
        // from the placement decision instead.
        assert!(publishes_task_content(
            TASK,
            &[],
            &owned(&["20260829-170100-task/expensive.bin"]),
            &projected,
        ));
        // A user-authorized additional scope is not the task's own content.
        assert!(!publishes_task_content(
            TASK,
            &owned(&["shared-area/config.yaml"]),
            &owned(&["shared-area/large.bin"]),
            &tracked(&["shared-area/config.yaml"]),
        ));
    }

    #[test]
    fn a_pristine_readme_survives_a_manifest_title_or_purpose_edit() {
        assert!(is_scaffold_readme(&task_readme("Demo", "Demo purpose")));
        assert!(is_scaffold_readme(&task_readme(
            "Demo v2",
            "Another purpose"
        )));
        assert!(!is_scaffold_readme(&format!(
            "{}\n## Process\n\nRan the analysis.\n",
            task_readme("Demo", "Demo purpose")
        )));
        assert!(!is_scaffold_readme(&format!(
            "# Demo\n\nDemo purpose\n\nExtra prose.\n\n{}",
            task_readme_directory_map()
        )));
        assert!(!is_scaffold_readme(&task_readme_directory_map()));
        assert!(!is_scaffold_readme(""));
    }

    #[test]
    fn detects_a_publication_that_removes_the_last_task_documentation() {
        let projected = tracked(&["20260829-170100-task/result.csv"]);
        assert!(removes_task_documentation(
            TASK,
            &owned(&[
                "20260829-170100-task/notes.md",
                "20260829-170100-task/result.csv",
            ]),
            &projected,
        ));
        assert!(!removes_task_documentation(
            TASK,
            &owned(&["20260829-170100-task/result.csv"]),
            &projected,
        ));
        assert!(!removes_task_documentation(
            TASK,
            &owned(&["docs/guide.md"]),
            &projected,
        ));
    }

    #[test]
    fn warns_only_when_task_content_changes_without_its_documentation() {
        let documentation = owned(&[
            "20260829-170100-task/README.md",
            "20260829-170100-task/notes/process.md",
        ]);
        assert!(documentation_warning(TASK, &documentation, &[], &[]).is_none());
        assert!(
            documentation_warning(
                TASK,
                &documentation,
                &owned(&["20260829-170100-task/README.md"]),
                &[],
            )
            .is_none()
        );
        assert!(
            documentation_warning(
                TASK,
                &documentation,
                &owned(&["shared-area/config.yaml"]),
                &[],
            )
            .is_none()
        );
        assert!(
            documentation_warning(
                TASK,
                &documentation,
                &owned(&[
                    "20260829-170100-task/notes/process.md",
                    "20260829-170100-task/tools/run.py",
                ]),
                &[],
            )
            .is_none()
        );
        let warning = documentation_warning(
            TASK,
            &documentation,
            &owned(&[
                "20260829-170100-task/.gitignore",
                "20260829-170100-task/tools/run.py",
            ]),
            &[],
        )
        .expect("content without documentation warns");
        assert_eq!(warning.code, "task-record-unchanged");
        assert!(warning.message.contains(TASK));
        assert!(
            documentation_warning(
                TASK,
                &documentation,
                &[],
                &owned(&["20260829-170100-task/expensive.bin"]),
            )
            .is_some()
        );
    }

    #[test]
    fn batches_pathspec_arguments_without_dropping_any_path() {
        let paths: Vec<String> = (0..5_000)
            .map(|index| format!("20260829-170100-task/data/f{index:05}.txt"))
            .collect();
        let batches = pathspec_batches(&paths);
        assert!(batches.len() > 1);
        for batch in &batches {
            assert!(!batch.is_empty());
            let bytes: usize = batch.iter().map(|path| path.len() + 1).sum();
            assert!(bytes <= PATHSPEC_ARGUMENT_BYTES || batch.len() == 1);
        }
        let flattened: Vec<&String> = batches.iter().flat_map(|batch| batch.iter()).collect();
        assert_eq!(flattened.len(), paths.len());
        assert!(flattened.iter().zip(paths.iter()).all(|(a, b)| *a == b));
        assert!(pathspec_batches(&[]).is_empty());
        let single = owned(&["a"]);
        assert_eq!(pathspec_batches(&single).len(), 1);
    }

    fn rule_record(source: &str, line: u32, pattern: &str, path: &str) -> String {
        format!("{source}\0{line}\0{pattern}\0{path}\0")
    }

    #[test]
    fn parses_the_machine_readable_ignore_rule_framing() {
        let raw = format!(
            "{}{}{}",
            rule_record(
                ".gitignore",
                1,
                ".DS_Store",
                "20260829-170100-task/.DS_Store"
            ),
            rule_record(
                "/home/dev/.config/git/ignore",
                2,
                "*.scratch",
                "20260829-170100-task/notes.scratch"
            ),
            rule_record(
                ".git/info/exclude",
                1,
                "bulk/",
                "20260829-170100-task/bulk/"
            ),
        );

        let rules = parse_ignore_rules(&raw);

        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0].source, ".gitignore");
        assert_eq!(rules[0].pattern, ".DS_Store");
        assert_eq!(rules[0].path, "20260829-170100-task/.DS_Store");
        assert_eq!(rules[1].source, "/home/dev/.config/git/ignore");
        assert_eq!(rules[2].path, "20260829-170100-task/bulk/");
        // `check-ignore` reports nothing when no path is ignored, and a
        // truncated record is never half-believed.
        assert!(parse_ignore_rules("").is_empty());
        assert!(parse_ignore_rules("*.log\0.gitignore\0").is_empty());
    }

    #[test]
    fn only_a_rule_the_publication_does_not_carry_is_machine_local() {
        let raw = format!(
            "{}{}{}{}",
            rule_record(
                ".gitignore",
                1,
                ".DS_Store",
                "20260829-170100-task/.DS_Store"
            ),
            rule_record(
                "20260829-170100-task/.gitignore",
                1,
                "*.tmp",
                "20260829-170100-task/scratch.tmp"
            ),
            rule_record(
                "/home/dev/.config/git/ignore",
                2,
                "*.scratch",
                "20260829-170100-task/notes.scratch"
            ),
            rule_record(
                ".git/info/exclude",
                1,
                "bulk/",
                "20260829-170100-task/bulk/"
            ),
        );
        let rules = parse_ignore_rules(&raw);
        let carried = tracked(&[".gitignore", "20260829-170100-task/.gitignore"]);

        let machine_local = machine_local_ignores(&rules, &carried);

        assert_eq!(
            machine_local
                .iter()
                .map(|rule| rule.source)
                .collect::<Vec<_>>(),
            vec!["/home/dev/.config/git/ignore", ".git/info/exclude"]
        );
        // A negated rule decides that the path is not ignored, so it hides
        // nothing even when its own file is untracked.
        let negation = rule_record(
            "vendor/.gitignore",
            3,
            "!keep.log",
            "20260829-170100-task/keep.log",
        );
        let negated = parse_ignore_rules(&negation);
        assert!(machine_local_ignores(&negated, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn a_rule_the_product_itself_ships_is_never_machine_local() {
        // The generated root file is unpublished between `init` and the first
        // scaffold publication, and on macOS a `.DS_Store` appears in a browsed
        // directory on its own. Refusing there would block the bootstrap the
        // guide prescribes, with no remedy the task could apply.
        let product = PRODUCT_IGNORE_RULES
            .iter()
            .map(|pattern| {
                rule_record(
                    ROOT_IGNORE_NAME,
                    1,
                    pattern,
                    &format!("20260829-170100-task/{}", pattern.trim_end_matches('/')),
                )
            })
            .collect::<String>();
        let rules = parse_ignore_rules(&product);
        assert_eq!(rules.len(), PRODUCT_IGNORE_RULES.len());
        assert!(machine_local_ignores(&rules, &BTreeSet::new()).is_empty());

        // Only the product's own list, and only in the file the product owns.
        let repository = format!(
            "{}{}",
            rule_record(
                ROOT_IGNORE_NAME,
                20,
                "*.scratchlog",
                "20260829-170100-task/run.scratchlog"
            ),
            rule_record(
                "vendor/.gitignore",
                1,
                ".DS_Store",
                "20260829-170100-task/vendor/.DS_Store"
            ),
        );
        let rules = parse_ignore_rules(&repository);
        assert_eq!(
            machine_local_ignores(&rules, &BTreeSet::new())
                .iter()
                .map(|rule| rule.pattern)
                .collect::<Vec<_>>(),
            vec!["*.scratchlog", ".DS_Store"]
        );
    }

    #[test]
    fn a_refusal_names_the_rule_its_source_and_both_fixes() {
        let raw = (0..7)
            .map(|index| {
                rule_record(
                    ".git/info/exclude",
                    1,
                    "bulk/",
                    &format!("20260829-170100-task/bulk{index}/"),
                )
            })
            .collect::<String>();
        let rules = parse_ignore_rules(&raw);
        let machine_local = machine_local_ignores(&rules, &BTreeSet::new());

        let message = untracked_ignore_message(&machine_local, "`20260829-170100-task/.gitignore`");

        assert!(
            message.contains("\"20260829-170100-task/bulk0/\""),
            "{message}"
        );
        assert!(message.contains("by rule \"bulk/\""), "{message}");
        assert!(message.contains("in \".git/info/exclude\""), "{message}");
        assert!(message.contains(", and 2 more"), "{message}");
        assert!(
            !message.contains("20260829-170100-task/bulk5/"),
            "the list is capped: {message}"
        );
        assert!(
            message.contains("`20260829-170100-task/.gitignore`")
                && message.contains(REPOSITORY_IGNORE_MODULE),
            "{message}"
        );
        // The fix inside the task's own write boundary is the one the message
        // leads with, and the repository layer states its cost rather than
        // reading as an equally cheap alternative.
        assert!(
            message.contains("stays inside this task's write boundary"),
            "{message}"
        );
        assert!(
            message
                .contains("shared root paths whose change needs the user's explicit authorization")
                && message.contains("published on the shared branch"),
            "{message}"
        );
        let single = machine_local_ignores(&rules[..1], &BTreeSet::new());
        assert!(!untracked_ignore_message(&single, "a scope").contains("more"));
    }

    #[test]
    fn bulk_publication_warns_above_either_threshold_and_stays_silent_below() {
        assert!(bulk_publication_warning(BULK_PUBLICATION_FILES, BULK_PUBLICATION_BYTES).is_none());
        assert!(bulk_publication_warning(0, 0).is_none());
        let many = bulk_publication_warning(BULK_PUBLICATION_FILES + 1, 1_024)
            .expect("one file above the file threshold warns");
        assert_eq!(many.code, "bulk-publication");
        assert!(many.message.contains(&BULK_PUBLICATION_FILES.to_string()));
        let large = bulk_publication_warning(1, BULK_PUBLICATION_BYTES + 1)
            .expect("one byte above the byte threshold warns");
        assert_eq!(large.code, "bulk-publication");
        // The threshold is read by an agent deciding whether it is close to it,
        // so it carries the unit the rest of the policy states sizes in.
        assert!(
            large.message.contains(&format!(
                "{BULK_PUBLICATION_MIB} MiB ({BULK_PUBLICATION_BYTES} bytes)"
            )),
            "{}",
            large.message
        );
        assert!(large.message.contains("rather than a refusal"));
    }
}
