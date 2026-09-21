use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use semver::Version;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::cloud_usage::{
    CloudUsageReport, LocalUsageStatus, UsageGate, UsageInputs, blobs_at, read_blobs,
    retires_content_only,
};
use crate::config::{
    CONFIG_NAME, Config, cli_version_satisfies, declared_minimum_cli_version,
    installed_cli_version, require_supported_cli_at,
};
use crate::dvc::{self, DataStatus};
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::hex::encode_lower;
use crate::lock::RepositoryLock;
use crate::manifest::{
    AdditionalScope, CloudUsageApproval, ResolvedTask, TaskKind, one_line, published_history_path,
    published_task_paths, validate_additional_scopes,
};
use crate::path::{allowed, repo_path, resolved_under};
use crate::policy::{
    AUTO_S3_ABOVE_BYTES, BULK_PUBLICATION_BYTES, BULK_PUBLICATION_FILES, BULK_PUBLICATION_MIB,
    REPOSITORY_IGNORE_MODULE, REVIEW_INITIAL_STATE, REVIEW_MANAGED_BY, REVIEW_MERGE_AUTHORITY,
    REVIEW_PULL_REQUEST, ROOT_IGNORE_NAME, TASK_MANIFEST_NAME, minimum_cli_version_for_task_schema,
};
use crate::s3_purge;
use crate::scaffold::{product_ignore_rules, task_readme_directory_map};
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
    pub cloud_usage: CloudUsageReport,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_requirement: Option<RepositoryRequirement>,
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

/// A change of the repository's `minimum_cli_version` that a publication
/// carries relative to the task branch it updates. Only the private index
/// changes; the shared worktree keeps its configuration until the
/// publication is merged and refreshed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepositoryRequirement {
    pub path: String,
    pub change: RequirementChange,
    /// The published declaration, or `None` when the publication removes it.
    pub minimum_cli_version: Option<String>,
    /// The declaration the publication's `.workspace-mgr.toml` carried before
    /// workspace-mgr reconciled it.
    pub previous_minimum_cli_version: Option<String>,
    /// The task manifest schema that needs the newer release; `None` when the
    /// change is not driven by a manifest.
    pub task_manifest_schema: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementChange {
    /// A task manifest in the publication needs a newer release.
    Raise,
    /// The publication needs a newer release and adopts the higher
    /// declaration of the base branch, so both carry the same configuration.
    Follow,
    /// No task manifest in the publication needs the task branch's earlier
    /// raise any more, so the declaration returns to the base branch's value
    /// at the task's fork point.
    Withdraw,
}

impl RepositoryRequirement {
    /// The audit trailer for this change. `base` names the base branch, such
    /// as `origin/main`.
    fn trailer(&self, base: &str) -> String {
        let key = crate::config::MINIMUM_CLI_VERSION_KEY;
        let value = match &self.minimum_cli_version {
            Some(version) => format!("{key}={version}"),
            None => format!("{key} removed"),
        };
        let reason = match (self.change, self.task_manifest_schema) {
            (RequirementChange::Raise, Some(schema)) => format!("task manifest schema {schema}"),
            (RequirementChange::Follow, Some(schema)) => {
                format!("task manifest schema {schema}; follows {base}")
            }
            (RequirementChange::Raise | RequirementChange::Follow, None) => {
                format!("follows {base}")
            }
            (RequirementChange::Withdraw, _) => format!(
                "withdraws this branch's raise to {}; no task manifest in this publication needs it",
                self.previous_minimum_cli_version
                    .as_deref()
                    .unwrap_or("an earlier version")
            ),
        };
        format!("Workspace-Requirement: {value} ({reason})")
    }
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
    // The shared branch may already require a newer workspace-mgr than the
    // local checkout declares.
    let remote_base_minimum = require_supported_cli_at(
        &repo,
        &remote_base_oid,
        &format!("{}/{}", task.remote, task.base_branch),
    )?;
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
        // A newer release may have published the task branch with a
        // declaration this one does not meet.
        require_supported_cli_at(&repo, &fetched, &format!("{}/{}", task.remote, task.branch))?;
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

    let installed = installed_cli_version();
    let base_location = format!("{}/{}", task.remote, task.base_branch);
    let own_manifest = task
        .task_path
        .as_ref()
        .map(|path| format!("{path}/{TASK_MANIFEST_NAME}"));
    let requirement_inputs = RequirementInputs {
        base_oid: &base_oid,
        remote_base_oid: &remote_base_oid,
        remote_base_minimum: remote_base_minimum.as_ref(),
        config_in_scope: allowed(CONFIG_NAME, &scopes),
        own_manifest: own_manifest.as_deref(),
        installed: &installed,
    };

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
    // The preview runs first, so a publication this build cannot declare is
    // refused before anything is placed or uploaded.
    reconcile_repository_requirement(&repo, &preview_index, &requirement_inputs)?;
    let preview_paths = changed_paths(&repo, &preview_index, &base_oid)?;
    let preview_policy = PrivateIndexPolicy {
        task: &task,
        published_task_path: published_task_path.as_deref(),
        large_file_threshold: AUTO_S3_ABOVE_BYTES,
        automatic_s3: &preview_automatic_s3,
        local_only: &local_only,
        config: &config,
        uncommitted: &initial_dvc,
        // Decided once the gate has measured, below.
        withheld_records_are_content: false,
    };
    validate_private_index(
        &repo,
        &preview_index,
        &base_oid,
        &scopes,
        &preview_paths,
        &preview_policy,
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
    } else if config.s3_enabled() && !initial_dvc.is_empty() {
        // The usage gate trusts the storage engine's report of changed outputs.
        dvc::ensure_ready(&repo, &config)?;
    }
    // Nothing has been placed, committed, or uploaded yet: this is the last
    // point where a publication can be refused without leaving local changes.
    let projected_tree_oid = repo
        .run_with_index(&preview_index, ["write-tree"], None, true)?
        .stdout
        .trim()
        .to_owned();
    let usage_inputs = UsageInputs {
        state_dir: &state_dir,
        remote_base_oid: &remote_base_oid,
        remote_target_oid: has_remote_target.then_some(base_oid.as_str()),
        projected_tree_oid: &projected_tree_oid,
        pointers: &initial_dvc,
        automatic_s3: &preview_automatic_s3,
        inspect_outputs: true,
    };
    let mut usage = UsageGate::open(
        &repo,
        &config,
        &task,
        &usage_inputs,
        crate::cloud_usage::effective_threshold(),
    )?;
    // The placement record of a result kept local before it was ever
    // published is that result's only durable trace, so the documentation
    // guard counts it as content, except while the task waits for the user's
    // cloud-usage decision: then the question comes first, and a record added
    // to a cleanup the limit allows would make it growth the limit refuses.
    // Only the measurement tells the two apart, so the preview left these
    // records to this point.
    let withheld_records_are_content = !usage.report().approval_required();
    if withheld_records_are_content
        && preview_paths
            .iter()
            .any(|path| records_local_retention(path, &local_only))
    {
        check_task_documentation(
            &repo,
            &preview_index,
            &base_oid,
            &preview_paths,
            &PrivateIndexPolicy {
                // The preview already asked the storage engine.
                uncommitted: &[],
                withheld_records_are_content,
                ..preview_policy
            },
        )?;
    }
    if options.operation == Operation::Publish {
        usage.enforce()?;
    }

    let placement = if dry_run {
        placement_preview
    } else {
        storage::apply_automatic(&repo, &config, &scopes, &base_oid, false)?
    };
    let automatic_s3 = placement.automatic_s3().to_vec();
    let publication_policy = PrivateIndexPolicy {
        automatic_s3: &automatic_s3,
        uncommitted: &[],
        withheld_records_are_content,
        ..preview_policy
    };
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
        reconcile_repository_requirement(&repo, &preflight_index, &requirement_inputs)?;
        let preflight_paths = changed_paths(&repo, &preflight_index, &base_oid)?;
        validate_private_index(
            &repo,
            &preflight_index,
            &base_oid,
            &scopes,
            &preflight_paths,
            &publication_policy,
        )?;
    }
    // The metadata as the storage engine finds it, so that a refusal before
    // the upload leaves the worktree as the publication found it.
    let uncommitted_metadata = if dry_run {
        Vec::new()
    } else {
        read_metadata(&repo, &pointers)?
    };
    let mut s3 = dvc::reconcile(&repo, &config, &pointers, dry_run)?;
    if !dry_run {
        // Background writers may have changed outputs since the preview and
        // the gate; the committed metadata is exactly what the upload would
        // send, so both judge it again before anything is uploaded.
        let judged = check_committed_documentation(
            &repo,
            &state_dir,
            &base_oid,
            &scopes,
            &s3,
            &publication_policy,
        )
        .and_then(|()| {
            usage.recheck_storage(
                &repo,
                &config,
                &UsageInputs {
                    pointers: &pointers,
                    automatic_s3: &[],
                    inspect_outputs: false,
                    ..usage_inputs
                },
            )
        })
        .and_then(|()| usage.enforce());
        if let Err(error) = judged {
            // Metadata left naming the refused content would make a retry
            // judge that content again after it is gone.
            return Err(
                match restore_metadata(&repo, &uncommitted_metadata, &s3.committed) {
                    Ok(()) => error,
                    Err(restore) => Error::message(format!(
                        "{error}; restoring the storage metadata the refused publication committed also failed: {restore}"
                    )),
                },
            );
        }
        dvc::push_outputs(&repo, &config, &mut s3)?;
    }
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
    let requirement = reconcile_repository_requirement(&repo, &index, &requirement_inputs)?;
    let paths = changed_paths(&repo, &index, &base_oid)?;
    validate_private_index(
        &repo,
        &index,
        &base_oid,
        &scopes,
        &paths,
        &publication_policy,
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
        // A plan leaves the S3 metadata of changed outputs as it is, and
        // `publish` commits it, so the record advice counts that metadata in
        // both.
        let mut touched = paths.clone();
        if dry_run {
            touched.extend(pending_metadata_rewrites(&repo, &s3, task_path)?);
            touched.sort();
            touched.dedup();
        }
        if changes_task_content(task_path, &touched, &automatic_s3) {
            let projection = TaskProjection::resolve(&repo, &index, task_path)?;
            warnings.extend(
                documentation_warning(
                    task_path,
                    &projection.documentation,
                    &touched,
                    &automatic_s3,
                )
                .map(|warning| defer_record_past_the_limit(warning, usage.report())),
            );
        }
        if changes_task_content(task_path, &paths, &automatic_s3) {
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
        cloud_usage: usage.report().clone(),
        head: repo.current_branch()?,
        branch: task.branch.clone(),
        base: base_ref,
        base_oid: base_oid.clone(),
        remote_base_oid: remote_base_oid.clone(),
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
        repository_requirement: requirement.clone(),
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
    usage.recheck_git(
        &repo,
        &UsageInputs {
            projected_tree_oid: &tree_oid,
            pointers: &pointers,
            automatic_s3: &[],
            inspect_outputs: false,
            ..usage_inputs
        },
    )?;
    report.cloud_usage = usage.report().clone();
    usage.enforce()?;

    let commit_message = build_commit_message(
        message
            .as_deref()
            .ok_or_else(|| Error::message("publish requires -m/--message"))?,
        &task.task_id,
        &scopes,
        &authorizations,
        requirement
            .as_ref()
            .map(|requirement| requirement.trailer(&base_location)),
        usage.approval(),
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
        if requirement.is_some()
            || (!requirement_inputs.config_in_scope && paths.iter().any(|path| path == CONFIG_NAME))
        {
            // The isolated worktree checks out the task branch, so it must
            // match the configuration the branch now carries.
            sync_worktree_config(&repo, &commit_oid)?;
        }
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

#[derive(Clone, Copy)]
struct PrivateIndexPolicy<'a> {
    task: &'a ResolvedTask,
    published_task_path: Option<&'a str>,
    large_file_threshold: u64,
    automatic_s3: &'a [String],
    /// Boundaries whose placement record keeps their payload on this machine.
    local_only: &'a BTreeSet<String>,
    config: &'a Config,
    /// In-scope S3 metadata whose outputs may hold changes the storage engine
    /// has not committed yet. Only the preview passes it: its staged metadata
    /// cannot show those changes, which `publish` commits after placement and
    /// just before the upload, so the documentation guard asks the engine.
    uncommitted: &'a [String],
    /// Whether the placement record of a result kept local before it was ever
    /// published counts as content for the documentation guard; see where
    /// `execute` decides it after the cloud-usage measurement.
    withheld_records_are_content: bool,
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
        // Outside the scopes, only the requirement reconciliation stages the
        // repository configuration.
        .filter(|path| path.as_str() != CONFIG_NAME)
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
    check_task_documentation(repo, index, base_oid, paths, policy)?;
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
    /// Staged blob of each S3 metadata file the projection tracks.
    metadata_blobs: BTreeMap<String, String>,
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
            metadata_blobs: BTreeMap::new(),
        };
        for entry in listed.stdout.split('\0').filter(|entry| !entry.is_empty()) {
            let Some((attributes, path)) = entry.split_once('\t') else {
                continue;
            };
            projection.tracked.insert(path.to_owned());
            let metadata_blob = attributes
                .split_whitespace()
                .nth(1)
                .filter(|_| path.ends_with(".dvc"));
            if let Some(oid) = metadata_blob {
                projection
                    .metadata_blobs
                    .insert(path.to_owned(), oid.to_owned());
            }
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

/// Changed paths the projection still tracks that retire content rather than
/// publish it, so that a task that documents nothing can still carry out a
/// cleanup the user chose, which a cloud-usage limit may leave as the only
/// publication allowed:
///
/// - the placement record of a local-only boundary whose previously published
///   payload or S3 metadata this publication takes out of Git, which is what
///   `untrack` of published content writes (see [`retires_published_payload`]),
///   and, while the task waits for the user's cloud-usage decision, the record
///   of any local-only boundary;
/// - S3 metadata rewritten to name only content its published version already
///   names, which is what removing files from a directory boundary in S3
///   leaves once the storage engine commits the boundary.
fn retiring_paths(
    repo: &GitRepo,
    base_oid: &str,
    task_path: &str,
    paths: &[String],
    projection: &TaskProjection,
    policy: &PrivateIndexPolicy<'_>,
) -> Result<BTreeSet<String>> {
    let removed = paths
        .iter()
        .filter(|path| !projection.tracked.contains(path.as_str()))
        .map(String::as_str)
        .collect::<Vec<_>>();
    let mut retiring = BTreeSet::new();
    let mut rewritten = Vec::new();
    for path in paths.iter().filter(|path| {
        is_task_content(task_path, path) && projection.tracked.contains(path.as_str())
    }) {
        if let Some(boundary) = path.strip_suffix(PLACEMENT_SUFFIX) {
            let published = published_history_path(
                boundary,
                policy.task.task_path.as_deref(),
                policy.published_task_path,
            );
            if policy.local_only.contains(boundary)
                && (!policy.withheld_records_are_content
                    || retires_published_payload(boundary, &published, &removed))
            {
                retiring.insert(path.clone());
            }
        } else if let Some(blob) = projection.metadata_blobs.get(path) {
            rewritten.push((path.clone(), blob.clone()));
        }
    }
    if rewritten.is_empty() {
        return Ok(retiring);
    }
    let names = rewritten
        .iter()
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    let published = blobs_at(repo, base_oid, &names)?;
    let pairs = rewritten
        .into_iter()
        .filter_map(|(path, staged)| {
            let old = published.get(&path)?.clone()?;
            Some((path, old, staged))
        })
        .collect::<Vec<_>>();
    let oids = pairs
        .iter()
        .flat_map(|(_, old, staged)| [old.clone(), staged.clone()])
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut contents = BTreeMap::new();
    read_blobs(repo, &oids, |oid, content| {
        contents.insert(oid.to_owned(), content.to_vec());
        Ok(())
    })?;
    for (path, old, staged) in pairs {
        let (Some(old), Some(staged)) = (contents.get(&old), contents.get(&staged)) else {
            continue;
        };
        if retires_content_only(repo, policy.config, &path, old, staged) {
            retiring.insert(path);
        }
    }
    Ok(retiring)
}

/// Whether the `removed` paths take a local-only boundary's previously
/// published payload or S3 metadata out of Git, at its current path or, for a
/// renamed task, at the path it was `published` under. That removal is what
/// the boundary's placement record stands for. A result kept local before it
/// was ever published retires nothing: its record is the only durable trace of
/// it, so the record stays content the task has to document, unless the task
/// waits for the user's cloud-usage decision.
fn retires_published_payload(boundary: &str, published: &str, removed: &[&str]) -> bool {
    removed.iter().any(|path| {
        [boundary, published].into_iter().any(|candidate| {
            *path == candidate
                || inside(candidate, path)
                || path.strip_suffix(".dvc") == Some(candidate)
        })
    })
}

/// Whether a changed path is the placement record that keeps a local-only
/// boundary's payload on this machine.
fn records_local_retention(path: &str, local_only: &BTreeSet<String>) -> bool {
    path.strip_suffix(PLACEMENT_SUFFIX)
        .is_some_and(|boundary| local_only.contains(boundary))
}

/// Judges the documentation guard on the S3 metadata that `publish` has just
/// committed, before anything is uploaded. The preview asked the storage
/// engine about uncommitted output changes, but a background writer may change
/// a boundary's outputs between that answer and the commit; judging only the
/// final index would upload such content and then refuse it. Metadata the
/// engine did not commit is what the preflight already judged, so a
/// publication that committed none inside the task directory builds no index.
fn check_committed_documentation(
    repo: &GitRepo,
    state_dir: &Path,
    base_oid: &str,
    scopes: &[String],
    s3: &dvc::DvcReport,
    policy: &PrivateIndexPolicy<'_>,
) -> Result<()> {
    let Some(task_path) = deliverable_task_path(policy.task) else {
        return Ok(());
    };
    if !s3
        .committed
        .iter()
        .any(|pointer| is_task_content(task_path, pointer))
    {
        return Ok(());
    }
    let index = state_dir.join("committed-index");
    if index.exists() {
        fs::remove_file(&index).at(&index)?;
    }
    repo.run_with_index(&index, ["read-tree", base_oid], None, true)?;
    remove_output_paths_from_index(repo, &index, policy.local_only.iter())?;
    stage_scopes(repo, &index, scopes)?;
    remove_stored_outputs_from_index(repo, &index, &s3.outputs)?;
    remove_output_paths_from_index(repo, &index, policy.local_only.iter())?;
    let paths = changed_paths(repo, &index, base_oid)?;
    check_task_documentation(repo, &index, base_oid, &paths, policy)
}

fn read_metadata(repo: &GitRepo, pointers: &[String]) -> Result<Vec<(String, Vec<u8>)>> {
    pointers
        .iter()
        .map(|pointer| {
            let path = resolved_under(&repo.root, pointer);
            Ok((pointer.clone(), fs::read(&path).at(&path)?))
        })
        .collect()
}

/// Writes back the content that each `committed` metadata file had before the
/// storage engine committed it.
fn restore_metadata(
    repo: &GitRepo,
    original: &[(String, Vec<u8>)],
    committed: &[String],
) -> Result<()> {
    for (pointer, content) in original
        .iter()
        .filter(|(pointer, _)| committed.contains(pointer))
    {
        let path = resolved_under(&repo.root, pointer);
        if fs::read(&path).at(&path)? != *content {
            storage::atomic_write_bytes(&path, content)?;
        }
    }
    Ok(())
}

/// The S3 metadata inside the task directory that `publish` would rewrite
/// when it commits: a dirty pointer whose outputs the storage engine reports
/// files added to, changed in, removed from, or renamed in. A pointer that is
/// dirty only because its objects are missing from the local cache is
/// committed back to the same metadata, and a file the engine cannot compare
/// because its directory's manifest is missing changes nothing on its own.
fn pending_metadata_rewrites(
    repo: &GitRepo,
    s3: &dvc::DvcReport,
    task_path: &str,
) -> Result<Vec<String>> {
    let dirty = s3
        .dirty_files
        .iter()
        .filter(|pointer| is_task_content(task_path, pointer))
        .collect::<Vec<_>>();
    if dirty.is_empty() {
        return Ok(Vec::new());
    }
    let outputs = dirty
        .iter()
        .flat_map(|pointer| s3.outputs.get(pointer.as_str()).into_iter().flatten())
        .cloned()
        .collect::<Vec<_>>();
    let status = dvc::data_status(repo, &outputs)?;
    let changes = &status.uncommitted;
    let rows = changes
        .added
        .iter()
        .chain(&changes.modified)
        .chain(&changes.deleted)
        .chain(
            changes
                .renamed
                .iter()
                .flat_map(|rename| [&rename.old, &rename.new]),
        )
        .collect::<Vec<_>>();
    Ok(dirty
        .into_iter()
        .filter(|pointer| {
            s3.outputs.get(pointer.as_str()).is_some_and(|outputs| {
                outputs.iter().any(|output| {
                    rows.iter().any(|row| {
                        row.strip_prefix(output.as_str())
                            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
                    })
                })
            })
        })
        .cloned()
        .collect())
}

/// Whether the storage engine reports content added or changed, rather than
/// only removed, inside the outputs of these S3 boundaries. Staged metadata
/// shows such a change only once `publish` has committed it, after placement
/// and just before the upload, so the preview asks the engine and the guard is
/// decided where `plan` decides it.
fn uncommitted_content(repo: &GitRepo, config: &Config, pointers: &[String]) -> Result<bool> {
    if pointers.is_empty() {
        return Ok(false);
    }
    dvc::ensure_ready(repo, config)?;
    let outputs = pointers
        .iter()
        .filter_map(|pointer| pointer.strip_suffix(".dvc"))
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    Ok(adds_uncommitted_content(&dvc::data_status(repo, &outputs)?))
}

/// Directory rows are skipped: the engine reports a directory boundary as
/// modified when a file inside it is only removed. A file the engine reports
/// as unknown sits in a directory whose recorded listing it cannot load, so it
/// cannot be compared on its own. It counts only when that directory's
/// aggregate changed: an unchanged aggregate proves every file unchanged, while
/// a changed one may hide an addition.
fn adds_uncommitted_content(status: &DataStatus) -> bool {
    let changes = &status.uncommitted;
    let changed_directories = changes
        .modified
        .iter()
        .filter(|path| path.ends_with('/'))
        .collect::<Vec<_>>();
    changes
        .added
        .iter()
        .chain(&changes.modified)
        .chain(changes.renamed.iter().map(|rename| &rename.new))
        .any(|path| !path.ends_with('/'))
        || changes.unknown.iter().any(|path| {
            changed_directories
                .iter()
                .any(|directory| path.starts_with(directory.as_str()))
        })
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
    base_oid: &str,
    paths: &[String],
    policy: &PrivateIndexPolicy<'_>,
) -> Result<()> {
    let Some(task_path) = deliverable_task_path(policy.task) else {
        return Ok(());
    };
    let uncommitted = policy
        .uncommitted
        .iter()
        .filter(|pointer| is_task_content(task_path, pointer))
        .cloned()
        .collect::<Vec<_>>();
    // A transaction that touches nothing inside the task directory, and holds
    // no S3 boundary there whose outputs may have changed, cannot fail this
    // check, so it does not pay for the projection listing either.
    if uncommitted.is_empty() && !changes_task_content(task_path, paths, policy.automatic_s3) {
        return Ok(());
    }
    let projection = TaskProjection::resolve(repo, index, task_path)?;
    if !projection.documentation.is_empty() {
        return Ok(());
    }
    let retiring = retiring_paths(repo, base_oid, task_path, paths, &projection, policy)?;
    let published = paths
        .iter()
        .filter(|path| !retiring.contains(path.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    // The engine is asked last: only a task that documents nothing and
    // publishes nothing else pays for it.
    if !publishes_task_content(
        task_path,
        &published,
        policy.automatic_s3,
        &projection.tracked,
    ) && !uncommitted_content(repo, policy.config, &uncommitted)?
    {
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

/// While a task is over its cloud-usage limit, a publication is allowed only
/// when it adds no content, so the record the warning asks for would turn the
/// cleanup the user chose into growth the limit refuses. The warning then says
/// where the record goes instead of inviting that refusal.
fn defer_record_past_the_limit(
    mut warning: TransactionWarning,
    usage: &CloudUsageReport,
) -> TransactionWarning {
    if usage.approval_required() && usage.cleanup_only {
        warning.message.push_str(
            "; while the task is over its cloud-usage limit this publication is allowed only because it adds no content, so publish it as it is and record the decision in the first publication the limit allows",
        );
    }
    warning
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
    rule.source == ROOT_IGNORE_NAME && product_ignore_rules().any(|product| product == rule.pattern)
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
    requirement_trailer: Option<String>,
    approval: Option<&CloudUsageApproval>,
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
    lines.extend(requirement_trailer);
    lines.extend(approval.map(crate::cloud_usage::approval_trailer));
    lines.join("\n") + "\n"
}

/// What the requirement reconciliation compares a publication with.
struct RequirementInputs<'a> {
    /// The fetched task-branch tip, or the fetched base tip for a first
    /// publication; every private index starts from it.
    base_oid: &'a str,
    remote_base_oid: &'a str,
    remote_base_minimum: Option<&'a Version>,
    /// `.workspace-mgr.toml` lies inside the publication's user-authorized
    /// scopes, so its staged content is the user's.
    config_in_scope: bool,
    /// The repository path of this task's own manifest, which only a
    /// deliverable publishes.
    own_manifest: Option<&'a str>,
    installed: &'a Version,
}

/// Reconciles `minimum_cli_version` in a private publication index with the
/// task manifests the index holds.
///
/// Outside the publication's scopes the declaration belongs to
/// workspace-mgr while the task branch's configuration is exactly what
/// workspace-mgr wrote there: the configuration at the branch's fork point
/// with the base branch, or that configuration rendered canonically with the
/// branch's declaration. The index then receives the fork point's
/// configuration exactly when the branch never changed it and no manifest
/// needs more than the fork point declares. When a manifest needs more, it
/// receives the fork point's configuration rendered canonically with the
/// higher of the requirement and the fetched base branch's declaration. When
/// the branch raised the declaration earlier and no manifest needs more than
/// the fork point any more, it withdraws that raise, but never below the
/// fetched base branch's declaration. A branch that never needed a newer
/// release therefore never changes the configuration, a branch raised
/// earlier follows a base branch that was raised further, and no publication
/// lowers a declaration the base branch already carries.
///
/// Inside the scopes, or when the task branch carries any other change of
/// the configuration, the staged declaration is only ever raised, to the
/// highest of the requirement and the fetched base branch's declaration,
/// and never lowered.
///
/// The publication is refused when this build does not meet a declaration it
/// would raise. Only a declaration that differs from the task-branch tip's is
/// reported.
fn reconcile_repository_requirement(
    repo: &GitRepo,
    index: &Path,
    inputs: &RequirementInputs<'_>,
) -> Result<Option<RepositoryRequirement>> {
    let needs = manifest_requirement(repo, index, inputs.own_manifest)?;
    let staged = staged_config(repo, index)?;
    if !inputs.config_in_scope {
        let fork = fork_point(repo, inputs.base_oid, inputs.remote_base_oid)?;
        let fork_config = tree_config(repo, &fork)?;
        if managed_since_fork_point(repo, staged.as_ref(), fork_config.as_ref())? {
            return reconcile_from_fork_point(
                repo,
                index,
                inputs,
                staged.as_ref(),
                fork_config.as_ref(),
                &needs,
            );
        }
    }
    raise_staged_requirement(repo, index, inputs, staged.as_ref(), &needs)
}

fn reconcile_from_fork_point(
    repo: &GitRepo,
    index: &Path,
    inputs: &RequirementInputs<'_>,
    staged: Option<&IndexEntry>,
    fork_config: Option<&IndexEntry>,
    needs: &ManifestNeeds,
) -> Result<Option<RepositoryRequirement>> {
    let previous = match staged {
        Some(entry) => declared_minimum_cli_version(&blob_text(repo, &entry.oid)?),
        None => None,
    };
    let fork = match fork_config {
        Some(entry) => Some((entry, blob_text(repo, &entry.oid)?)),
        None => None,
    };
    let fork_declared = fork
        .as_ref()
        .and_then(|(_, raw)| declared_minimum_cli_version(raw));
    let trigger = needs.highest().filter(|(version, _)| {
        fork_declared
            .as_ref()
            .is_none_or(|declared| version.cmp_precedence(declared).is_gt())
    });
    let published = match (&trigger, &fork) {
        (Some((version, _)), Some((entry, raw))) => {
            let raised = highest(version, inputs.remote_base_minimum);
            require_publishable(inputs.installed, needs, &raised)?;
            let rendered = render_declaring(raw, &raised)?;
            stage_config_content(repo, index, &entry.mode, &rendered)?;
            Some(raised)
        }
        (Some((version, schema)), None) => {
            require_publishable(inputs.installed, needs, version)?;
            return Err(missing_configuration(version, *schema));
        }
        (None, Some((entry, raw))) => {
            let untouched = staged.is_some_and(|staged| staged.oid == entry.oid);
            // A withdrawal never goes below the base branch's declaration,
            // so no merge of this branch, not even one that replays its
            // commits onto the base branch, lowers what the base branch
            // already requires.
            let floor = match (&fork_declared, inputs.remote_base_minimum) {
                (Some(fork), Some(base)) if base.cmp_precedence(fork).is_gt() => Some(base),
                (None, Some(base)) => Some(base),
                _ => None,
            };
            match floor {
                Some(floor) if !untouched => {
                    require_publishable(inputs.installed, &ManifestNeeds::default(), floor)?;
                    let rendered = render_declaring(raw, floor)?;
                    stage_config_content(repo, index, &entry.mode, &rendered)?;
                    Some(floor.clone())
                }
                _ => {
                    stage_config_entry(repo, index, &entry.mode, &entry.oid)?;
                    fork_declared
                }
            }
        }
        (None, None) => {
            if staged.is_some() {
                repo.run_with_index(
                    index,
                    ["update-index", "--force-remove", "--", CONFIG_NAME],
                    None,
                    true,
                )?;
            }
            None
        }
    };
    Ok(describe_requirement_change(previous, published, trigger))
}

/// Raises the staged declaration to the highest of what the manifests need
/// and the fetched base branch's declaration, keeping every other part of the
/// staged configuration and never lowering it.
fn raise_staged_requirement(
    repo: &GitRepo,
    index: &Path,
    inputs: &RequirementInputs<'_>,
    staged: Option<&IndexEntry>,
    needs: &ManifestNeeds,
) -> Result<Option<RepositoryRequirement>> {
    let required = needs.highest();
    let Some(entry) = staged else {
        if let Some((version, schema)) = &required {
            require_publishable(inputs.installed, needs, version)?;
            return Err(missing_configuration(version, *schema));
        }
        return Ok(None);
    };
    if required.is_none() && inputs.remote_base_minimum.is_none() {
        return Ok(None);
    }
    if !entry.is_regular_file() {
        if required.is_none() {
            return Ok(None);
        }
        return Err(Error::message(format!(
            "{CONFIG_NAME} must be a regular file in the publication"
        )));
    }
    let raw = blob_text(repo, &entry.oid)?;
    let declared = declared_minimum_cli_version(&raw);
    let target = [
        required.as_ref().map(|(version, _)| version),
        inputs.remote_base_minimum,
    ]
    .into_iter()
    .flatten()
    .max_by(|left, right| left.cmp_precedence(right))
    .cloned();
    let Some(target) = target else {
        return Ok(None);
    };
    if declared
        .as_ref()
        .is_some_and(|declared| declared.cmp_precedence(&target).is_ge())
    {
        return Ok(None);
    }
    require_publishable(inputs.installed, needs, &target)?;
    let rendered = render_declaring(&raw, &target)?;
    stage_config_content(repo, index, &entry.mode, &rendered)?;
    // A configuration inside the task's scope may stage a lower declaration
    // than the branch already publishes; restoring it is not a change.
    if crate::config::minimum_cli_version_at(repo, inputs.base_oid)?
        .is_some_and(|published| published.cmp_precedence(&target).is_ge())
    {
        return Ok(None);
    }
    let raise = required
        .as_ref()
        .is_some_and(|(version, _)| target.cmp_precedence(version).is_le());
    Ok(Some(RepositoryRequirement {
        path: CONFIG_NAME.to_owned(),
        change: if raise {
            RequirementChange::Raise
        } else {
            RequirementChange::Follow
        },
        minimum_cli_version: Some(target.to_string()),
        previous_minimum_cli_version: declared.map(|version| version.to_string()),
        task_manifest_schema: required.map(|(_, schema)| schema),
    }))
}

/// Describes how the published declaration differs from `previous`, the
/// declaration staged before reconciliation.
fn describe_requirement_change(
    previous: Option<Version>,
    published: Option<Version>,
    trigger: Option<(Version, u32)>,
) -> Option<RepositoryRequirement> {
    let ordering = match (&previous, &published) {
        (None, None) => return None,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(previous), Some(published)) => published.cmp_precedence(previous),
    };
    let (change, task_manifest_schema) = match ordering {
        std::cmp::Ordering::Equal => return None,
        std::cmp::Ordering::Greater => {
            let raise = match (&trigger, &published) {
                (Some((required, _)), Some(published)) => {
                    published.cmp_precedence(required).is_le()
                }
                _ => false,
            };
            (
                if raise {
                    RequirementChange::Raise
                } else {
                    RequirementChange::Follow
                },
                trigger.map(|(_, schema)| schema),
            )
        }
        std::cmp::Ordering::Less => (RequirementChange::Withdraw, None),
    };
    Some(RepositoryRequirement {
        path: CONFIG_NAME.to_owned(),
        change,
        minimum_cli_version: published.map(|version| version.to_string()),
        previous_minimum_cli_version: previous.map(|version| version.to_string()),
        task_manifest_schema,
    })
}

/// Refuses to publish a declaration this build does not meet, because the
/// repository would then refuse the very build that raised it. The advice
/// depends on whose manifest needs the newer release: only this task's own
/// approval can be removed by this task.
fn require_publishable(
    installed: &Version,
    needs: &ManifestNeeds,
    declaration: &Version,
) -> Result<()> {
    if let Some(other) = &needs.others {
        if !cli_version_satisfies(installed, &other.version) {
            return Err(Error::message(format!(
                "this build (workspace-mgr {installed}) cannot publish {}, another task's manifest in this publication, because its schema {} requires workspace-mgr {} or newer; update workspace-mgr. Tell the user both versions and ask before updating with `cargo install --locked workspace-mgr`.",
                other.path, other.schema, other.version
            )));
        }
    }
    if let Some(own) = &needs.own {
        if !cli_version_satisfies(installed, &own.version) {
            return Err(Error::message(format!(
                "this build (workspace-mgr {installed}) cannot publish task manifest schema {}, which requires workspace-mgr {} or newer; update workspace-mgr, or record the default limit to remove the approval. Tell the user both versions and ask before updating with `cargo install --locked workspace-mgr` or removing the approval.",
                own.schema, own.version
            )));
        }
    }
    if !cli_version_satisfies(installed, declaration) {
        return Err(Error::message(format!(
            "this build (workspace-mgr {installed}) cannot publish `{}` {declaration}; update workspace-mgr. Tell the user both versions and ask before updating with `cargo install --locked workspace-mgr`.",
            crate::config::MINIMUM_CLI_VERSION_KEY
        )));
    }
    Ok(())
}

fn missing_configuration(required: &Version, schema: u32) -> Error {
    Error::message(format!(
        "task manifest schema {schema} requires workspace-mgr {required} or newer, but the publication has no {CONFIG_NAME} to record that in; publish the repository configuration first"
    ))
}

/// A task manifest that needs a newer workspace-mgr than schemas without a
/// requirement.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ManifestNeed {
    version: Version,
    schema: u32,
    path: String,
}

/// What the task manifests in a private index need, split by whether the
/// manifest is this task's own, since only this task's approval can be
/// withdrawn by this task.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ManifestNeeds {
    own: Option<ManifestNeed>,
    /// The most demanding other task manifest, such as one merged on the base
    /// branch.
    others: Option<ManifestNeed>,
}

impl ManifestNeeds {
    /// The newest workspace-mgr any manifest needs, with the schema that
    /// needs it.
    fn highest(&self) -> Option<(Version, u32)> {
        [&self.own, &self.others]
            .into_iter()
            .flatten()
            .max_by(|left, right| left.version.cmp_precedence(&right.version))
            .map(|need| (need.version.clone(), need.schema))
    }
}

/// What the task manifests one directory below the root of the index need.
fn manifest_requirement(
    repo: &GitRepo,
    index: &Path,
    own_manifest: Option<&str>,
) -> Result<ManifestNeeds> {
    let manifests = index_entries(repo, index, &format!(":(glob)*/{TASK_MANIFEST_NAME}"))?
        .into_iter()
        .filter(IndexEntry::is_regular_file)
        .collect::<Vec<_>>();
    let oids = manifests
        .iter()
        .map(|entry| entry.oid.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut schemas = std::collections::BTreeMap::new();
    crate::cloud_usage::read_blobs(repo, &oids, |oid, content| {
        if let Some(schema) = manifest_schema(content) {
            schemas.insert(oid.to_owned(), schema);
        }
        Ok(())
    })?;
    let mut needs = ManifestNeeds::default();
    for entry in manifests {
        let Some(schema) = schemas.get(&entry.oid).copied() else {
            continue;
        };
        let Some(version) = minimum_cli_version_for_task_schema(schema) else {
            continue;
        };
        let slot = if own_manifest == Some(entry.path.as_str()) {
            &mut needs.own
        } else {
            &mut needs.others
        };
        let higher = slot
            .as_ref()
            .is_none_or(|current| version.cmp_precedence(&current.version).is_gt());
        if higher {
            *slot = Some(ManifestNeed {
                version,
                schema,
                path: entry.path,
            });
        }
    }
    Ok(needs)
}

fn highest(required: &Version, other: Option<&Version>) -> Version {
    match other {
        Some(other) if other.cmp_precedence(required).is_gt() => other.clone(),
        _ => required.clone(),
    }
}

fn render_declaring(raw: &str, version: &Version) -> Result<String> {
    let mut config = Config::parse_ignoring_cli_requirement(raw, Path::new(CONFIG_NAME))?;
    config.minimum_cli_version = Some(version.to_string());
    config.render()
}

/// The commit where the task branch left the base branch; for a first
/// publication both are the fetched base tip.
fn fork_point(repo: &GitRepo, tip: &str, base: &str) -> Result<String> {
    if tip == base {
        return Ok(tip.to_owned());
    }
    let output = repo.run_unchecked(["merge-base", tip, base])?;
    match output.code {
        0 => Ok(output.stdout.trim().to_owned()),
        // Unrelated histories share no fork point; the task branch is then
        // the only reference.
        1 => Ok(tip.to_owned()),
        _ => Err(Error::message(format!(
            "failed to find where the task branch left the base branch: {}",
            output.stderr.trim()
        ))),
    }
}

fn staged_config(repo: &GitRepo, index: &Path) -> Result<Option<IndexEntry>> {
    Ok(
        index_entries(repo, index, &format!(":(literal){CONFIG_NAME}"))?
            .into_iter()
            .find(|entry| entry.path == CONFIG_NAME),
    )
}

fn tree_config(repo: &GitRepo, revision: &str) -> Result<Option<IndexEntry>> {
    let listed = repo.run(["ls-tree", "-z", revision, "--", CONFIG_NAME])?;
    for record in listed
        .stdout
        .split('\0')
        .filter(|record| !record.is_empty())
    {
        let parsed = record.split_once('\t').and_then(|(metadata, path)| {
            let mut fields = metadata.split(' ');
            let mode = fields.next()?.to_owned();
            let _kind = fields.next()?;
            Some(IndexEntry {
                mode,
                oid: fields.next()?.to_owned(),
                path: path.to_owned(),
            })
        });
        let entry =
            parsed.ok_or_else(|| Error::message(format!("unexpected tree entry {record:?}")))?;
        if entry.path == CONFIG_NAME {
            return Ok(Some(entry));
        }
    }
    Ok(None)
}

/// Whether the task branch's configuration is exactly what workspace-mgr
/// writes there: the fork point's blob, or the fork point's configuration
/// rendered canonically with the branch's declaration. Any other difference,
/// even one in comments or formatting only, is a user-authorized change.
fn managed_since_fork_point(
    repo: &GitRepo,
    staged: Option<&IndexEntry>,
    fork: Option<&IndexEntry>,
) -> Result<bool> {
    let (staged, fork) = match (staged, fork) {
        (None, None) => return Ok(true),
        (Some(staged), Some(fork)) => (staged, fork),
        _ => return Ok(false),
    };
    if staged.mode != fork.mode || !staged.is_regular_file() {
        return Ok(false);
    }
    if staged.oid == fork.oid {
        return Ok(true);
    }
    let staged_raw = blob_text(repo, &staged.oid)?;
    let Some(declared) = declared_minimum_cli_version(&staged_raw) else {
        return Ok(false);
    };
    let fork_raw = blob_text(repo, &fork.oid)?;
    // workspace-mgr only ever renders a declaration higher than the fork
    // point's, so a canonical copy at or below it is the user's own change.
    if declared_minimum_cli_version(&fork_raw).is_some_and(|forked| declared <= forked) {
        return Ok(false);
    }
    Ok(render_declaring(&fork_raw, &declared).is_ok_and(|rendered| rendered == staged_raw))
}

fn blob_text(repo: &GitRepo, oid: &str) -> Result<String> {
    Ok(repo.run(["cat-file", "blob", oid])?.stdout)
}

fn stage_config_content(repo: &GitRepo, index: &Path, mode: &str, content: &str) -> Result<()> {
    let blob = repo.run_bytes(["hash-object", "-w", "--stdin"], Some(content.as_bytes()))?;
    let blob = String::from_utf8_lossy(&blob.stdout).trim().to_owned();
    stage_config_entry(repo, index, mode, &blob)
}

fn stage_config_entry(repo: &GitRepo, index: &Path, mode: &str, oid: &str) -> Result<()> {
    repo.run_with_index(
        index,
        [
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("{mode},{oid},{CONFIG_NAME}"),
        ],
        None,
        true,
    )?;
    Ok(())
}

/// Brings an isolated infrastructure worktree's configuration to the
/// published commit, whose index `read-tree` has already loaded.
fn sync_worktree_config(repo: &GitRepo, commit_oid: &str) -> Result<()> {
    let object = format!("{commit_oid}:{CONFIG_NAME}");
    if repo.run_unchecked(["cat-file", "-e", &object])?.success() {
        repo.run(["checkout-index", "--force", "--", CONFIG_NAME])?;
        return Ok(());
    }
    let path = repo.root.join(CONFIG_NAME);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Io { path, source }),
    }
}

/// The schema a task manifest declares, read without validating the rest.
fn manifest_schema(content: &[u8]) -> Option<u32> {
    let table = toml::from_str::<toml::Table>(std::str::from_utf8(content).ok()?).ok()?;
    u32::try_from(table.get("schema_version")?.as_integer()?).ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexEntry {
    mode: String,
    oid: String,
    path: String,
}

impl IndexEntry {
    fn is_regular_file(&self) -> bool {
        matches!(self.mode.as_str(), "100644" | "100755")
    }
}

fn index_entries(repo: &GitRepo, index: &Path, pathspec: &str) -> Result<Vec<IndexEntry>> {
    let listed = repo.run_with_index(
        index,
        ["ls-files", "--stage", "-z", "--", pathspec],
        None,
        true,
    )?;
    listed
        .stdout
        .split('\0')
        .filter(|record| !record.is_empty())
        .map(|record| {
            let parsed = record.split_once('\t').and_then(|(metadata, path)| {
                let mut fields = metadata.split(' ');
                Some(IndexEntry {
                    mode: fields.next()?.to_owned(),
                    oid: fields.next()?.to_owned(),
                    path: path.to_owned(),
                })
            });
            parsed.ok_or_else(|| Error::message(format!("unexpected index entry {record:?}")))
        })
        .collect()
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
    let cloud_usage = crate::cloud_usage::local_status(&repo, &task)?;
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
        cloud_usage,
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
    pub cloud_usage: LocalUsageStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requirement(
        change: RequirementChange,
        minimum: Option<&str>,
        previous: Option<&str>,
        schema: Option<u32>,
    ) -> RepositoryRequirement {
        RepositoryRequirement {
            path: CONFIG_NAME.to_owned(),
            change,
            minimum_cli_version: minimum.map(ToOwned::to_owned),
            previous_minimum_cli_version: previous.map(ToOwned::to_owned),
            task_manifest_schema: schema,
        }
    }

    #[test]
    fn requirement_and_approval_trailers_follow_the_scope_trailers() {
        let approval = CloudUsageApproval {
            limit_bytes: 3_221_225_472,
            note: "The user approved 3 GiB in chat".to_owned(),
        };
        let raise = requirement(RequirementChange::Raise, Some("0.4.0"), None, Some(3));
        let scopes = vec![
            "20260918-120000-demo".to_owned(),
            "docs/shared.md".to_owned(),
        ];
        let authorizations = vec![AdditionalScope {
            path: "docs/shared.md".to_owned(),
            reason: "The user requested this update".to_owned(),
        }];
        let message = build_commit_message(
            "Publish checkpoints",
            "20260918-120000-demo",
            &scopes,
            &authorizations,
            Some(raise.trailer("origin/main")),
            Some(&approval),
        );
        assert_eq!(
            message,
            "Publish checkpoints\n\nWorkspace-Task: 20260918-120000-demo\nWorkspace-Scope: 20260918-120000-demo, docs/shared.md\nScope-Authorization: docs/shared.md -- The user requested this update\nWorkspace-Requirement: minimum_cli_version=0.4.0 (task manifest schema 3)\nCloud-Usage-Approval: limit_bytes=3221225472; note=The user approved 3 GiB in chat\n"
        );
        let plain = build_commit_message(
            "Publish checkpoints",
            "20260918-120000-demo",
            &scopes,
            &authorizations,
            None,
            None,
        );
        assert!(!plain.contains(crate::cloud_usage::APPROVAL_TRAILER));
        assert!(!plain.contains("Workspace-Requirement"));
    }

    #[test]
    fn requirement_trailers_name_the_change() {
        for (change, expected) in [
            (
                requirement(
                    RequirementChange::Raise,
                    Some("0.4.0"),
                    Some("0.2.0"),
                    Some(3),
                ),
                "Workspace-Requirement: minimum_cli_version=0.4.0 (task manifest schema 3)",
            ),
            (
                requirement(
                    RequirementChange::Follow,
                    Some("0.5.0"),
                    Some("0.4.0"),
                    Some(3),
                ),
                "Workspace-Requirement: minimum_cli_version=0.5.0 (task manifest schema 3; follows origin/main)",
            ),
            (
                requirement(
                    RequirementChange::Follow,
                    Some("0.5.0"),
                    Some("0.2.0"),
                    None,
                ),
                "Workspace-Requirement: minimum_cli_version=0.5.0 (follows origin/main)",
            ),
            (
                requirement(
                    RequirementChange::Withdraw,
                    Some("0.2.0"),
                    Some("0.4.0"),
                    None,
                ),
                "Workspace-Requirement: minimum_cli_version=0.2.0 (withdraws this branch's raise to 0.4.0; no task manifest in this publication needs it)",
            ),
            (
                requirement(RequirementChange::Withdraw, None, Some("0.4.0"), None),
                "Workspace-Requirement: minimum_cli_version removed (withdraws this branch's raise to 0.4.0; no task manifest in this publication needs it)",
            ),
        ] {
            assert_eq!(change.trailer("origin/main"), expected);
        }
        assert_eq!(
            serde_json::to_value(requirement(
                RequirementChange::Withdraw,
                None,
                Some("0.4.0"),
                None
            ))
            .unwrap(),
            serde_json::json!({
                "path": CONFIG_NAME,
                "change": "withdraw",
                "minimum_cli_version": null,
                "previous_minimum_cli_version": "0.4.0",
                "task_manifest_schema": null,
            })
        );
    }

    const PLAIN_CONFIG: &str = "[git]\nremote = \"origin\"\nbranch = \"main\"\n";
    const TASK: &str = "20260918-120000-approved";

    fn declaring(version: &str, config: &str) -> String {
        format!("minimum_cli_version = \"{version}\"\n\n{config}")
    }

    /// A repository whose `main` holds a configuration, and a private index
    /// built the way publication builds it: from a base revision plus the
    /// staged task paths.
    struct Fixture {
        _temp: tempfile::TempDir,
        repo: GitRepo,
        index: PathBuf,
        main: String,
    }

    impl Fixture {
        fn new(config: Option<&str>) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("repo");
            fs::create_dir(&root).unwrap();
            let repo = GitRepo { root };
            repo.run(["init", "-q", "-b", "main"]).unwrap();
            repo.run(["config", "user.name", "workspace-mgr test"])
                .unwrap();
            repo.run(["config", "user.email", "test@example.invalid"])
                .unwrap();
            let index = temp.path().join("index");
            let mut fixture = Self {
                _temp: temp,
                repo,
                index,
                main: String::new(),
            };
            fixture.write("README.md", "base\n");
            if let Some(config) = config {
                fixture.write(CONFIG_NAME, config);
            }
            fixture.main = fixture.commit();
            fixture
        }

        fn write(&self, path: &str, content: &str) {
            let path = self.repo.root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }

        fn manifest(&self, directory: &str, schema: u32) {
            self.write(
                &format!("{directory}/{TASK_MANIFEST_NAME}"),
                &format!("schema_version = {schema}\nkind = \"deliverable\"\n"),
            );
        }

        /// Commits the worktree to the checked-out branch.
        fn commit(&self) -> String {
            self.repo.run(["add", "-A"]).unwrap();
            self.repo
                .run(["commit", "-q", "--allow-empty", "-m", "change"])
                .unwrap();
            self.repo
                .run(["rev-parse", "HEAD"])
                .unwrap()
                .stdout
                .trim()
                .to_owned()
        }

        fn stage(&self, base: &str, paths: &[&str]) {
            let _ = fs::remove_file(&self.index);
            self.repo
                .run_with_index(&self.index, ["read-tree", base], None, true)
                .unwrap();
            let mut add = vec!["add", "-A", "--"];
            add.extend(paths);
            self.repo
                .run_with_index(&self.index, add, None, true)
                .unwrap();
        }

        /// Records the private index as a task-branch commit on `parent`.
        fn publish(&self, parent: &str) -> String {
            let tree = self
                .repo
                .run_with_index(&self.index, ["write-tree"], None, true)
                .unwrap()
                .stdout
                .trim()
                .to_owned();
            self.repo
                .run(["commit-tree", &tree, "-p", parent, "-m", "publish"])
                .unwrap()
                .stdout
                .trim()
                .to_owned()
        }

        fn reconcile(
            &self,
            base: &str,
            remote_base: &str,
            remote_minimum: Option<&str>,
            config_in_scope: bool,
            installed: &str,
        ) -> Result<Option<RepositoryRequirement>> {
            let remote_minimum = remote_minimum.map(|version| Version::parse(version).unwrap());
            let installed = Version::parse(installed).unwrap();
            let own_manifest = format!("{TASK}/{TASK_MANIFEST_NAME}");
            reconcile_repository_requirement(
                &self.repo,
                &self.index,
                &RequirementInputs {
                    base_oid: base,
                    remote_base_oid: remote_base,
                    remote_base_minimum: remote_minimum.as_ref(),
                    config_in_scope,
                    own_manifest: Some(&own_manifest),
                    installed: &installed,
                },
            )
        }

        fn staged_config(&self) -> Option<String> {
            staged_config(&self.repo, &self.index)
                .unwrap()
                .map(|entry| blob_text(&self.repo, &entry.oid).unwrap())
        }

        fn staged_config_oid(&self) -> Option<String> {
            staged_config(&self.repo, &self.index)
                .unwrap()
                .map(|entry| entry.oid)
        }

        fn config_oid_at(&self, revision: &str) -> Option<String> {
            tree_config(&self.repo, revision)
                .unwrap()
                .map(|entry| entry.oid)
        }
    }

    #[test]
    fn schema_3_manifests_raise_only_the_private_configuration() {
        let fixture = Fixture::new(Some(PLAIN_CONFIG));
        let main = fixture.main.clone();
        fixture.manifest("20260918-120000-old", 2);
        fixture.stage(&main, &["20260918-120000-old"]);
        assert_eq!(
            fixture
                .reconcile(&main, &main, None, false, "0.4.0")
                .unwrap(),
            None
        );
        assert_eq!(fixture.staged_config().unwrap(), PLAIN_CONFIG);

        // Nested files that merely share the manifest name are not tasks.
        fixture.manifest("20260918-120000-old/copy", 3);
        fixture.stage(&main, &["20260918-120000-old"]);
        assert_eq!(
            fixture
                .reconcile(&main, &main, None, false, "0.4.0")
                .unwrap(),
            None
        );

        fixture.manifest(TASK, 3);
        fixture.stage(&main, &[TASK]);
        assert_eq!(
            fixture
                .reconcile(&main, &main, None, false, "0.4.0")
                .unwrap(),
            Some(requirement(
                RequirementChange::Raise,
                Some("0.4.0"),
                None,
                Some(3)
            ))
        );
        assert_eq!(
            fixture.staged_config().unwrap(),
            declaring("0.4.0", PLAIN_CONFIG)
        );
        // The worktree configuration is untouched.
        assert_eq!(
            fs::read_to_string(fixture.repo.root.join(CONFIG_NAME)).unwrap(),
            PLAIN_CONFIG
        );

        // Once the task branch declares it, nothing changes again.
        let tip = fixture.publish(&main);
        fixture.write(&format!("{TASK}/notes.md"), "notes\n");
        fixture.stage(&tip, &[TASK]);
        assert_eq!(
            fixture
                .reconcile(&tip, &main, None, false, "0.4.0")
                .unwrap(),
            None
        );
        assert_eq!(fixture.staged_config_oid(), fixture.config_oid_at(&tip));
    }

    #[test]
    fn a_build_never_publishes_a_declaration_it_does_not_meet() {
        let fixture = Fixture::new(Some(PLAIN_CONFIG));
        let main = fixture.main.clone();
        fixture.manifest(TASK, 3);
        fixture.stage(&main, &[TASK]);
        let error = fixture
            .reconcile(&main, &main, None, false, "0.3.0")
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "this build (workspace-mgr 0.3.0) cannot publish task manifest schema 3, which requires workspace-mgr 0.4.0 or newer; update workspace-mgr, or record the default limit to remove the approval. Tell the user both versions and ask before updating with `cargo install --locked workspace-mgr` or removing the approval."
        );
        assert_eq!(fixture.staged_config().unwrap(), PLAIN_CONFIG);
        // An authorized configuration change is refused the same way.
        let error = fixture
            .reconcile(&main, &main, None, true, "0.3.9")
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with(
                "this build (workspace-mgr 0.3.9) cannot publish task manifest schema 3"
            ),
            "{error}"
        );
        assert_eq!(fixture.staged_config().unwrap(), PLAIN_CONFIG);

        // A release candidate publishes the declaration of its release.
        assert_eq!(
            fixture
                .reconcile(&main, &main, None, false, "0.4.0-rc.1")
                .unwrap()
                .unwrap()
                .minimum_cli_version
                .as_deref(),
            Some("0.4.0")
        );

        // Defensively, a base declaration this build does not meet is never
        // copied either; fetching such a base already refuses.
        fixture.stage(&main, &[TASK]);
        let error = fixture
            .reconcile(&main, &main, Some("0.7.1"), false, "0.4.0")
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with(
                "this build (workspace-mgr 0.4.0) cannot publish `minimum_cli_version` 0.7.1"
            ),
            "{error}"
        );
        assert_eq!(fixture.staged_config().unwrap(), PLAIN_CONFIG);
    }

    #[test]
    fn a_raise_takes_the_higher_declaration_of_the_base_branch() {
        let lower = declaring("0.3.5", PLAIN_CONFIG);
        let fixture = Fixture::new(Some(&lower));
        let main = fixture.main.clone();
        fixture.manifest(TASK, 3);
        fixture.stage(&main, &[TASK]);
        // The shared branch already requires more, so every task branch
        // writes the same value and merges cleanly.
        assert_eq!(
            fixture
                .reconcile(&main, &main, Some("0.7.1"), false, "1.0.0")
                .unwrap(),
            Some(requirement(
                RequirementChange::Follow,
                Some("0.7.1"),
                Some("0.3.5"),
                Some(3)
            ))
        );
        assert_eq!(
            fixture.staged_config().unwrap(),
            declaring("0.7.1", PLAIN_CONFIG)
        );

        // A declaration that already meets the manifests stays as it is.
        let higher = declaring("0.5.0", PLAIN_CONFIG);
        let fixture = Fixture::new(Some(&higher));
        let main = fixture.main.clone();
        fixture.manifest(TASK, 3);
        fixture.stage(&main, &[TASK]);
        assert_eq!(
            fixture
                .reconcile(&main, &main, Some("0.5.0"), false, "0.5.0")
                .unwrap(),
            None
        );
        assert_eq!(fixture.staged_config().unwrap(), higher);

        // Declarations are release versions, so a pre-release one cannot be
        // raised from.
        let prerelease = declaring("0.4.0-rc.1", PLAIN_CONFIG);
        let fixture = Fixture::new(Some(&prerelease));
        let main = fixture.main.clone();
        fixture.manifest(TASK, 3);
        fixture.stage(&main, &[TASK]);
        let error = fixture
            .reconcile(&main, &main, None, false, "0.4.0")
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a pre-release"), "{error}");
    }

    #[test]
    fn a_raised_branch_follows_a_base_branch_raised_further() {
        let fixture = Fixture::new(Some(PLAIN_CONFIG));
        let fork = fixture.main.clone();
        fixture.manifest(TASK, 3);
        fixture.stage(&fork, &[TASK]);
        fixture
            .reconcile(&fork, &fork, None, false, "0.4.0")
            .unwrap();
        let tip = fixture.publish(&fork);

        // A later release raises main for its own schema.
        fixture.write(CONFIG_NAME, &declaring("0.5.0", PLAIN_CONFIG));
        fixture.write("later.md", "later\n");
        let main = fixture.commit();
        fixture.write(&format!("{TASK}/notes.md"), "notes\n");
        fixture.stage(&tip, &[TASK]);
        assert_eq!(
            fixture
                .reconcile(&tip, &main, Some("0.5.0"), false, "0.5.0")
                .unwrap(),
            Some(requirement(
                RequirementChange::Follow,
                Some("0.5.0"),
                Some("0.4.0"),
                Some(3)
            ))
        );
        // The branch now carries main's configuration blob exactly.
        assert_eq!(fixture.staged_config_oid(), fixture.config_oid_at(&main));
    }

    #[test]
    fn a_branch_withdraws_a_raise_its_manifests_no_longer_need() {
        for fork_config in [PLAIN_CONFIG.to_owned(), declaring("0.2.0", PLAIN_CONFIG)] {
            let fixture = Fixture::new(Some(&fork_config));
            let fork = fixture.main.clone();
            let fork_declared =
                declared_minimum_cli_version(&fork_config).map(|version| version.to_string());
            fixture.manifest(TASK, 3);
            fixture.stage(&fork, &[TASK]);
            let raised = fixture
                .reconcile(&fork, &fork, None, false, "0.4.0")
                .unwrap()
                .unwrap();
            assert_eq!(raised.change, RequirementChange::Raise);
            let tip = fixture.publish(&fork);

            // Main moves on without touching the configuration.
            fixture.write("later.md", "later\n");
            let main = fixture.commit();
            fixture.manifest(TASK, 2);
            fixture.stage(&tip, &[TASK]);
            assert_eq!(
                fixture
                    .reconcile(
                        &tip,
                        &main,
                        fork_declared.as_deref().map(|_| "0.2.0"),
                        false,
                        "0.4.0"
                    )
                    .unwrap(),
                Some(requirement(
                    RequirementChange::Withdraw,
                    fork_declared.as_deref(),
                    Some("0.4.0"),
                    None
                ))
            );
            // The branch configuration is the fork point's blob again.
            assert_eq!(fixture.staged_config_oid(), fixture.config_oid_at(&fork));
            let withdrawn = fixture.publish(&tip);
            fixture.stage(&withdrawn, &[TASK]);
            assert_eq!(
                fixture
                    .reconcile(&withdrawn, &main, None, false, "0.4.0")
                    .unwrap(),
                None
            );
        }
    }

    #[test]
    fn branches_that_need_no_raise_never_touch_the_configuration() {
        // Main already declares what its merged schema 3 manifests need.
        let fixture = Fixture::new(Some(&declaring("0.4.0", PLAIN_CONFIG)));
        fixture.manifest("20260918-110000-merged", 3);
        let fork = fixture.commit();
        fixture.manifest(TASK, 2);
        fixture.stage(&fork, &[TASK]);
        assert_eq!(
            fixture
                .reconcile(&fork, &fork, Some("0.4.0"), false, "0.4.0")
                .unwrap(),
            None
        );
        let tip = fixture.publish(&fork);

        // Main later changes its configuration and raises it further; the
        // untouched branch keeps its fork point's configuration.
        fixture.write(
            CONFIG_NAME,
            &format!(
                "{}\n[s3]\nurl = \"s3://bucket/prefix\"\n",
                declaring("0.5.0", PLAIN_CONFIG)
            ),
        );
        let main = fixture.commit();
        fixture.write(&format!("{TASK}/notes.md"), "notes\n");
        fixture.stage(&tip, &[TASK]);
        assert_eq!(
            fixture
                .reconcile(&tip, &main, Some("0.5.0"), false, "0.5.0")
                .unwrap(),
            None
        );
        assert_eq!(fixture.staged_config_oid(), fixture.config_oid_at(&fork));
    }

    #[test]
    fn authorized_configuration_changes_are_only_ever_raised() {
        let published = declaring("0.4.0", PLAIN_CONFIG);
        let fixture = Fixture::new(Some(&published));
        fixture.manifest(TASK, 3);
        let base = fixture.commit();
        // An authorized configuration change in the task's scope stages an
        // older declaration than the branch already publishes.
        fixture.write(CONFIG_NAME, &declaring("0.2.0", PLAIN_CONFIG));
        fixture.stage(&base, &[TASK, CONFIG_NAME]);
        assert_eq!(
            fixture
                .reconcile(&base, &base, None, true, "0.4.0")
                .unwrap(),
            None
        );
        assert_eq!(fixture.staged_config().unwrap(), published);

        // Against a branch that declares less, the same staging is a raise.
        let plain = Fixture::new(Some(PLAIN_CONFIG));
        let base = plain.main.clone();
        plain.manifest(TASK, 3);
        plain.write(CONFIG_NAME, &declaring("0.2.0", PLAIN_CONFIG));
        plain.stage(&base, &[TASK, CONFIG_NAME]);
        assert_eq!(
            plain.reconcile(&base, &base, None, true, "0.4.0").unwrap(),
            Some(requirement(
                RequirementChange::Raise,
                Some("0.4.0"),
                Some("0.2.0"),
                Some(3)
            ))
        );

        // A branch that published an authorized change of more than the
        // declaration keeps it, even when a later publication leaves the
        // configuration outside its scopes.
        let s3 = format!("{PLAIN_CONFIG}\n[s3]\nurl = \"s3://bucket/prefix\"\n");
        plain.write(CONFIG_NAME, &declaring("0.4.0", &s3));
        plain.stage(&base, &[TASK, CONFIG_NAME]);
        let tip = plain.publish(&base);
        plain.manifest(TASK, 2);
        plain.stage(&tip, &[TASK]);
        assert_eq!(
            plain.reconcile(&tip, &base, None, false, "0.4.0").unwrap(),
            None
        );
        assert_eq!(plain.staged_config_oid(), plain.config_oid_at(&tip));
    }

    #[test]
    fn a_publication_without_configuration_cannot_record_the_requirement() {
        let fixture = Fixture::new(None);
        let main = fixture.main.clone();
        fixture.manifest(TASK, 3);
        fixture.stage(&main, &[TASK]);
        let error = fixture
            .reconcile(&main, &main, None, false, "0.4.0")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("requires workspace-mgr 0.4.0 or newer, but the publication has no .workspace-mgr.toml"),
            "{error}"
        );
        let error = fixture
            .reconcile(&main, &main, None, false, "0.3.0")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("cannot publish task manifest schema 3"),
            "{error}"
        );
        assert_eq!(fixture.staged_config(), None);
    }

    #[test]
    fn a_withdrawal_never_lowers_the_base_branch_declaration() {
        let fixture = Fixture::new(Some(PLAIN_CONFIG));
        let fork = fixture.main.clone();
        fixture.manifest(TASK, 3);
        fixture.stage(&fork, &[TASK]);
        fixture
            .reconcile(&fork, &fork, None, false, "0.4.0")
            .unwrap();
        let tip = fixture.publish(&fork);

        // Hosting rebased the branch onto main, so main carries the raise in
        // commits the branch does not share, and another task's manifest
        // there still needs it.
        fixture.write(CONFIG_NAME, &declaring("0.4.0", PLAIN_CONFIG));
        fixture.manifest("20260918-110000-merged", 3);
        let main = fixture.commit();

        // The user resets the approval and the branch keeps publishing: its
        // commit keeps the declaration main already carries.
        fixture.manifest(TASK, 2);
        fixture.stage(&tip, &[TASK]);
        assert_eq!(
            fixture
                .reconcile(&tip, &main, Some("0.4.0"), false, "0.4.0")
                .unwrap(),
            None
        );
        assert_eq!(fixture.staged_config_oid(), fixture.config_oid_at(&tip));

        // After a later release raised main further, the branch follows it
        // instead of withdrawing.
        fixture.write(CONFIG_NAME, &declaring("0.5.0", PLAIN_CONFIG));
        let later = fixture.commit();
        fixture.stage(&tip, &[TASK]);
        assert_eq!(
            fixture
                .reconcile(&tip, &later, Some("0.5.0"), false, "0.5.0")
                .unwrap(),
            Some(requirement(
                RequirementChange::Follow,
                Some("0.5.0"),
                Some("0.4.0"),
                None
            ))
        );
        assert_eq!(fixture.staged_config_oid(), fixture.config_oid_at(&later));
        let followed = fixture.publish(&tip);
        fixture.stage(&followed, &[TASK]);
        assert_eq!(
            fixture
                .reconcile(&followed, &later, Some("0.5.0"), false, "0.5.0")
                .unwrap(),
            None
        );
    }

    #[test]
    fn authorized_comment_and_formatting_changes_are_never_reverted() {
        let commented_declaring = format!(
            "# Owned by the data team.\n{}",
            declaring("0.4.0", PLAIN_CONFIG)
        );
        for (fork_config, authorized) in [
            (
                PLAIN_CONFIG.to_owned(),
                format!("# Owned by the data team.\n{PLAIN_CONFIG}"),
            ),
            (
                PLAIN_CONFIG.to_owned(),
                "[git]\nremote = 'origin'\nbranch = 'main'\n".to_owned(),
            ),
            // Removing a comment from a configuration that already declares a
            // version yields exactly what workspace-mgr would render, but
            // workspace-mgr never renders a declaration that is not higher
            // than the fork point's, so the change is still the user's.
            (commented_declaring, declaring("0.4.0", PLAIN_CONFIG)),
        ] {
            let fixture = Fixture::new(Some(&fork_config));
            let fork = fixture.main.clone();
            fixture.manifest(TASK, 2);
            // An earlier publication carried the user-authorized change.
            fixture.write(CONFIG_NAME, &authorized);
            fixture.stage(&fork, &[TASK, CONFIG_NAME]);
            assert_eq!(
                fixture
                    .reconcile(&fork, &fork, None, true, "0.4.0")
                    .unwrap(),
                None
            );
            let tip = fixture.publish(&fork);
            // A later publication leaves the configuration outside its
            // scopes and keeps the change.
            fixture.write(&format!("{TASK}/notes.md"), "notes\n");
            fixture.stage(&tip, &[TASK]);
            assert_eq!(
                fixture
                    .reconcile(&tip, &fork, None, false, "0.4.0")
                    .unwrap(),
                None
            );
            assert_eq!(fixture.staged_config().unwrap(), authorized);
        }

        // What workspace-mgr itself wrote is recognized by its exact text, so
        // a raise of a commented configuration still withdraws to the fork
        // point's blob, comment included.
        let commented = format!("# Owned by the data team.\n{PLAIN_CONFIG}");
        let fixture = Fixture::new(Some(&commented));
        let fork = fixture.main.clone();
        fixture.manifest(TASK, 3);
        fixture.stage(&fork, &[TASK]);
        fixture
            .reconcile(&fork, &fork, None, false, "0.4.0")
            .unwrap();
        assert_eq!(
            fixture.staged_config().unwrap(),
            declaring("0.4.0", PLAIN_CONFIG)
        );
        let tip = fixture.publish(&fork);
        fixture.manifest(TASK, 2);
        fixture.stage(&tip, &[TASK]);
        assert_eq!(
            fixture
                .reconcile(&tip, &fork, None, false, "0.4.0")
                .unwrap(),
            Some(requirement(
                RequirementChange::Withdraw,
                None,
                Some("0.4.0"),
                None
            ))
        );
        assert_eq!(fixture.staged_config().unwrap(), commented);
    }

    #[test]
    fn authorized_configuration_changes_follow_the_base_branch() {
        let s3 = format!("{PLAIN_CONFIG}\n[s3]\nurl = \"s3://bucket/prefix\"\n");
        let fixture = Fixture::new(Some(PLAIN_CONFIG));
        let fork = fixture.main.clone();
        fixture.manifest(TASK, 3);
        fixture.write(CONFIG_NAME, &s3);
        fixture.stage(&fork, &[TASK, CONFIG_NAME]);
        assert_eq!(
            fixture
                .reconcile(&fork, &fork, None, true, "0.4.0")
                .unwrap(),
            Some(requirement(
                RequirementChange::Raise,
                Some("0.4.0"),
                None,
                Some(3)
            ))
        );
        assert_eq!(fixture.staged_config().unwrap(), declaring("0.4.0", &s3));
        let tip = fixture.publish(&fork);

        // A later release raises main further; the branch's next publication
        // follows it although the configuration is outside its scopes.
        fixture.write(CONFIG_NAME, &declaring("0.5.0", PLAIN_CONFIG));
        let main = fixture.commit();
        fixture.write(&format!("{TASK}/notes.md"), "notes\n");
        fixture.stage(&tip, &[TASK]);
        assert_eq!(
            fixture
                .reconcile(&tip, &main, Some("0.5.0"), false, "0.5.0")
                .unwrap(),
            Some(requirement(
                RequirementChange::Follow,
                Some("0.5.0"),
                Some("0.4.0"),
                Some(3)
            ))
        );
        assert_eq!(fixture.staged_config().unwrap(), declaring("0.5.0", &s3));

        // An authorized configuration that declares less than the base
        // branch, such as a stale checkout's copy, is raised to the base
        // branch's declaration even when no manifest needs it.
        fixture.manifest(TASK, 2);
        fixture.write(CONFIG_NAME, &s3);
        fixture.stage(&tip, &[TASK, CONFIG_NAME]);
        assert_eq!(
            fixture
                .reconcile(&tip, &main, Some("0.5.0"), true, "0.5.0")
                .unwrap(),
            Some(requirement(
                RequirementChange::Follow,
                Some("0.5.0"),
                None,
                None
            ))
        );
        assert_eq!(fixture.staged_config().unwrap(), declaring("0.5.0", &s3));

        // A higher authorized declaration is never lowered.
        let higher = declaring("0.6.0", &s3);
        fixture.write(CONFIG_NAME, &higher);
        fixture.stage(&tip, &[TASK, CONFIG_NAME]);
        assert_eq!(
            fixture
                .reconcile(&tip, &main, Some("0.5.0"), true, "0.6.0")
                .unwrap(),
            None
        );
        assert_eq!(fixture.staged_config().unwrap(), higher);
    }

    #[test]
    fn only_this_tasks_approval_is_offered_for_removal() {
        // Another task's schema 3 manifest reached main, whose declaration
        // was later removed by hand.
        let fixture = Fixture::new(Some(PLAIN_CONFIG));
        fixture.manifest("20260918-110000-merged", 3);
        let main = fixture.commit();
        let other_task = "this build (workspace-mgr 0.3.0) cannot publish 20260918-110000-merged/.workspace-mgr-task.toml, another task's manifest in this publication, because its schema 3 requires workspace-mgr 0.4.0 or newer; update workspace-mgr. Tell the user both versions and ask before updating with `cargo install --locked workspace-mgr`.";
        fixture.manifest(TASK, 2);
        fixture.stage(&main, &[TASK]);
        let error = fixture
            .reconcile(&main, &main, None, false, "0.3.0")
            .unwrap_err()
            .to_string();
        assert_eq!(error, other_task);
        assert_eq!(fixture.staged_config().unwrap(), PLAIN_CONFIG);

        // Removing this task's own approval would not help either.
        fixture.manifest(TASK, 3);
        fixture.stage(&main, &[TASK]);
        let error = fixture
            .reconcile(&main, &main, None, false, "0.3.0")
            .unwrap_err()
            .to_string();
        assert_eq!(error, other_task);

        // A build that reads the schema restores the declaration.
        fixture.manifest(TASK, 2);
        fixture.stage(&main, &[TASK]);
        assert_eq!(
            fixture
                .reconcile(&main, &main, None, false, "0.4.0")
                .unwrap(),
            Some(requirement(
                RequirementChange::Raise,
                Some("0.4.0"),
                None,
                Some(3)
            ))
        );
    }
}

#[cfg(test)]
mod curation_tests {
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
    fn a_local_placement_record_retires_only_content_that_was_published() {
        let boundary = "20260829-170100-task/results.bin";
        let directory = "20260829-170100-task/outputs";
        // `untrack` of a result published in Git removes its payload, and of
        // one published in S3 removes its metadata.
        assert!(retires_published_payload(boundary, boundary, &[boundary]));
        assert!(retires_published_payload(
            boundary,
            boundary,
            &["20260829-170100-task/results.bin.dvc"]
        ));
        assert!(retires_published_payload(
            directory,
            directory,
            &["20260829-170100-task/outputs/a.csv"]
        ));
        // A renamed task removes them where they were published.
        assert!(retires_published_payload(
            boundary,
            "20260829-170000-old/results.bin",
            &["20260829-170000-old/results.bin.dvc"]
        ));
        // A result kept local before it was ever published removes nothing,
        // and neither does a removal of a neighbour that shares a prefix.
        assert!(!retires_published_payload(boundary, boundary, &[]));
        assert!(!retires_published_payload(
            boundary,
            boundary,
            &[
                "20260829-170100-task/results.bin.bak",
                "20260829-170100-task/results.bin2.dvc",
                "20260829-170100-task/other.bin.dvc",
            ]
        ));
        assert!(!retires_published_payload(
            directory,
            directory,
            &["20260829-170100-task/outputs-old/a.csv"]
        ));
    }

    #[test]
    fn only_added_or_changed_files_are_uncommitted_content() {
        let status = |raw: &str| dvc::parse_data_status(raw).unwrap();
        // Removing a file from a directory boundary also reports the directory.
        let removed = status(
            r#"{"committed": {"added": ["out/", "out/a.bin"]}, "uncommitted": {"modified": ["out/"], "deleted": ["out/b.bin"]}}"#,
        );
        assert!(!adds_uncommitted_content(&removed));
        assert!(!adds_uncommitted_content(&status("{}")));
        assert!(!adds_uncommitted_content(&status(
            r#"{"not_in_cache": ["out/a.bin"]}"#
        )));
        // Without the directory's recorded listing, its files are unknown;
        // an unchanged aggregate proves them unchanged.
        assert!(!adds_uncommitted_content(&status(
            r#"{"not_in_cache": ["out/"], "committed": {"added": ["out/"]}, "uncommitted": {"unknown": ["out/b.bin", "out/a.bin"]}}"#
        )));
        for raw in [
            r#"{"uncommitted": {"modified": ["out/"], "added": ["out/c.bin"], "deleted": ["out/b.bin"]}}"#,
            r#"{"uncommitted": {"modified": ["out/", "out/a.bin"]}}"#,
            r#"{"uncommitted": {"modified": ["stored.bin"]}}"#,
            // A changed aggregate over files the engine cannot compare may
            // hide an addition, here `out/c.bin` next to a removal.
            r#"{"not_in_cache": ["out/"], "uncommitted": {"modified": ["out/"], "unknown": ["out/a.bin", "out/c.bin"]}}"#,
            r#"{"uncommitted": {"modified": ["out/"], "unknown": ["out/sub/a.bin"]}}"#,
            r#"{"uncommitted": {"renamed": [{"old": "out/a.bin", "new": "out/b.bin"}]}}"#,
        ] {
            assert!(adds_uncommitted_content(&status(raw)), "{raw}");
        }
    }

    fn usage(status: &str, cleanup_only: bool) -> CloudUsageReport {
        CloudUsageReport {
            status: status.to_owned(),
            publish_allowed: status == "within_limit" || cleanup_only,
            cleanup_only,
            git_history_exceeds_limit: false,
            threshold_bytes: 1_000,
            limit_bytes: 1_000,
            approval: None,
            published: Default::default(),
            projected: Default::default(),
            git_measure: "uncompressed".to_owned(),
            headroom_bytes: 0,
            suggested_limit_bytes: 268_435_456,
            contributors: Vec::new(),
            message: None,
        }
    }

    #[test]
    fn the_record_warning_defers_the_record_only_for_a_cleanup_past_the_limit() {
        let documentation = owned(&["20260829-170100-task/notes/process.md"]);
        let warning = || {
            documentation_warning(
                TASK,
                &documentation,
                &owned(&["20260829-170100-task/results.bin.dvc"]),
                &[],
            )
            .expect("a retirement without documentation warns")
        };
        let plain = warning().message;
        for (status, cleanup_only) in [
            ("within_limit", false),
            ("within_limit", true),
            ("approval_required", false),
        ] {
            assert_eq!(
                defer_record_past_the_limit(warning(), &usage(status, cleanup_only)).message,
                plain,
                "{status} {cleanup_only}"
            );
        }
        let deferred = defer_record_past_the_limit(warning(), &usage("approval_required", true));
        assert_eq!(deferred.code, "task-record-unchanged");
        assert!(deferred.message.starts_with(&plain), "{}", deferred.message);
        assert!(
            deferred
                .message
                .ends_with("record the decision in the first publication the limit allows"),
            "{}",
            deferred.message
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
        let product = product_ignore_rules()
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
        assert_eq!(rules.len(), product_ignore_rules().count());
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
