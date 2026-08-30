use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::dvc;
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::lock::RepositoryLock;
use crate::manifest::{INFRASTRUCTURE_MANIFEST_NAME, ResolvedTask, TaskKind};
use crate::path::{repo_path, resolved_under};
use crate::transaction::{task_state_dir, validate_remote_task_identity};

const DISCARD_PLAN_SCHEMA: u32 = 1;
const DISCARD_PLAN_NAME: &str = "discard-plan.json";
const ZERO_OID: &str = "0000000000000000000000000000000000000000";

#[derive(Debug, Clone)]
pub struct TaskDiscardOptions {
    pub start: PathBuf,
    pub manifest: Option<PathBuf>,
    pub dry_run: bool,
    pub confirm: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskDiscardReport {
    pub status: String,
    pub operation: String,
    pub kind: TaskKind,
    pub task_id: String,
    pub branch: String,
    pub remote: String,
    pub base_branch: String,
    pub manifest: String,
    pub local_branch_oid: Option<String>,
    pub remote_branch_oid: Option<String>,
    pub remote_base_oid: String,
    pub working_changes: Vec<String>,
    pub local_actions: Vec<LocalDiscardAction>,
    pub review: DiscardReview,
    pub retained_s3: Vec<RetainedS3Boundary>,
    pub remote_writes: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmation_plan: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cleanup_warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocalDiscardAction {
    pub path: String,
    pub action: String,
    pub currently_present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restored_from: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiscardReview {
    pub managed_by: &'static str,
    pub required_before_confirm: String,
    pub must_be_unmerged: bool,
    pub provider_state_verified_by_cli: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RetainedS3Boundary {
    pub boundary: String,
    pub sources: Vec<String>,
    pub version_ids: Vec<String>,
    pub disposition: &'static str,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DiscardPlan {
    schema_version: u32,
    snapshot: DiscardSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DiscardSnapshot {
    task_id: String,
    branch: String,
    local_branch_oid: Option<String>,
    remote_branch_oid: Option<String>,
    remote_tracking_oid: Option<String>,
    local_base_oid: String,
    remote_base_oid: String,
}

struct DiscardContext {
    task_repo: GitRepo,
    admin_repo: GitRepo,
    task: ResolvedTask,
    state_dir: PathBuf,
    snapshot: DiscardSnapshot,
    working_changes: Vec<String>,
    local_actions: Vec<LocalDiscardAction>,
    retained_s3: Vec<RetainedS3Boundary>,
}

pub fn discard(options: &TaskDiscardOptions) -> Result<TaskDiscardReport> {
    validate_mode(options)?;
    let task_repo = match &options.manifest {
        Some(path) => GitRepo::discover_for_manifest(path)?,
        None => GitRepo::discover(&options.start)?,
    };
    let _repository_lock = RepositoryLock::acquire(&task_repo)?;
    let config = Config::load_compatible(&task_repo)?;
    let manifest = match &options.manifest {
        Some(path) => path.clone(),
        None => ResolvedTask::discover(&task_repo, &options.start)?,
    };
    let task = ResolvedTask::load(&task_repo, &config, &manifest)?;
    let mut context = build_context(task_repo, task)?;
    let plan_path = context.state_dir.join(DISCARD_PLAN_NAME);

    if options.dry_run {
        fs::create_dir_all(&context.state_dir).at(&context.state_dir)?;
        write_plan(
            &plan_path,
            &DiscardPlan {
                schema_version: DISCARD_PLAN_SCHEMA,
                snapshot: context.snapshot.clone(),
            },
        )?;
        return Ok(report(&context, "dry_run", Some(plan_path), Vec::new()));
    }

    let confirmation = options
        .confirm
        .as_deref()
        .ok_or_else(|| Error::message("task discard requires --confirm <task-id>"))?;
    if confirmation != context.task.task_id {
        return Err(Error::message(format!(
            "discard confirmation must exactly match task id {:?}",
            context.task.task_id
        )));
    }
    let planned = read_plan(&plan_path)?;
    if planned.snapshot != context.snapshot {
        return Err(Error::message(
            "task or branch state changed after discard dry-run; rerun `workspace-mgr task discard --dry-run`, re-verify the pull request, and confirm again",
        ));
    }
    if invocation_would_be_deleted(&context)? {
        return Err(Error::message(format!(
            "confirm task discard from the shared checkout, using `--manifest {}`; removing the current workspace would leave the invoking shell inside a deleted directory",
            context.task.manifest_path.display()
        )));
    }

    let cleanup_warnings = match context.task.kind {
        TaskKind::Deliverable => discard_deliverable(&mut context)?,
        TaskKind::Infrastructure => discard_infrastructure(&mut context)?,
    };
    Ok(report(&context, "discarded", None, cleanup_warnings))
}

fn validate_mode(options: &TaskDiscardOptions) -> Result<()> {
    match (options.dry_run, options.confirm.is_some()) {
        (true, true) => Err(Error::message(
            "--dry-run and --confirm are mutually exclusive",
        )),
        (false, false) => Err(Error::message(
            "task discard requires either --dry-run or --confirm <task-id>",
        )),
        _ => Ok(()),
    }
}

fn build_context(task_repo: GitRepo, task: ResolvedTask) -> Result<DiscardContext> {
    task_repo.validate_branch(&task.branch)?;
    task_repo.validate_remote_name(&task.remote)?;
    let admin_repo = administrative_repo(&task_repo, &task)?;
    let common_dir = admin_repo.common_dir()?;
    let state_dir = task_state_dir(&common_dir, &task);
    let snapshot = snapshot(&admin_repo, &task)?;
    reject_merged_task(&admin_repo, &task, &snapshot)?;
    let working_changes = working_changes(&task_repo, &task)?;
    let local_actions = local_actions(&task_repo, &task, &snapshot.local_base_oid)?;
    let retained_s3 = retained_s3(&task_repo, &admin_repo, &task, &snapshot)?;
    Ok(DiscardContext {
        task_repo,
        admin_repo,
        task,
        state_dir,
        snapshot,
        working_changes,
        local_actions,
        retained_s3,
    })
}

fn administrative_repo(task_repo: &GitRepo, task: &ResolvedTask) -> Result<GitRepo> {
    let base_worktrees = task_repo.branch_worktrees(&task.base_branch)?;
    if base_worktrees.len() != 1 {
        return Err(Error::message(format!(
            "configured shared branch {:?} must be checked out in exactly one worktree before discarding a task",
            task.base_branch
        )));
    }
    let admin = GitRepo::discover(&base_worktrees[0])?;
    if admin.current_branch()?.as_deref() != Some(&task.base_branch) {
        return Err(Error::message(
            "shared checkout branch changed during discovery",
        ));
    }
    match task.kind {
        TaskKind::Deliverable => {
            if admin.root != task_repo.root {
                return Err(Error::message(
                    "deliverable task discard must resolve from the shared checkout",
                ));
            }
            task_repo.ensure_branch_not_checked_out(&task.branch)?;
        }
        TaskKind::Infrastructure => {
            let expected = task_repo
                .common_dir()?
                .join("workspace-mgr/checkouts")
                .join(&task.task_id);
            if task_repo.root != expected {
                return Err(Error::message(format!(
                    "infrastructure worktree must be the workspace-mgr-owned checkout {}; got {}",
                    expected.display(),
                    task_repo.root.display()
                )));
            }
            let task_worktrees = task_repo.branch_worktrees(&task.branch)?;
            if task_worktrees != vec![task_repo.root.clone()] {
                return Err(Error::message(format!(
                    "infrastructure branch {:?} must be checked out only in its managed worktree",
                    task.branch
                )));
            }
        }
    }
    Ok(admin)
}

fn snapshot(repo: &GitRepo, task: &ResolvedTask) -> Result<DiscardSnapshot> {
    let local_base_oid = repo
        .optional_oid(&format!("refs/heads/{}", task.base_branch))?
        .ok_or_else(|| Error::message("configured local shared branch does not exist"))?;
    let remote_base_oid = repo.fetch_branch(&task.remote, &task.base_branch)?;
    let remote_branch_oid = repo.remote_branch_oid(&task.remote, &task.branch)?;
    if let Some(observed) = &remote_branch_oid {
        let fetched = repo.fetch_branch(&task.remote, &task.branch)?;
        if &fetched != observed {
            return Err(Error::message(
                "task branch changed while discard state was being inspected; retry",
            ));
        }
        validate_remote_task_identity(repo, task, observed)?;
    }
    Ok(DiscardSnapshot {
        task_id: task.task_id.clone(),
        branch: task.branch.clone(),
        local_branch_oid: repo.optional_oid(&format!("refs/heads/{}", task.branch))?,
        remote_branch_oid,
        remote_tracking_oid: repo
            .optional_oid(&format!("refs/remotes/{}/{}", task.remote, task.branch))?,
        local_base_oid,
        remote_base_oid,
    })
}

fn reject_merged_task(repo: &GitRepo, task: &ResolvedTask, state: &DiscardSnapshot) -> Result<()> {
    if let Some(remote) = &state.remote_branch_oid {
        let ancestor = repo.run_unchecked([
            "merge-base",
            "--is-ancestor",
            remote,
            &state.remote_base_oid,
        ])?;
        match ancestor.code {
            0 => {
                return Err(Error::message(
                    "task branch is already contained in the remote shared branch; merged tasks cannot be discarded",
                ));
            }
            1 => {}
            _ => {
                return Err(Error::message(
                    "failed to determine whether the task branch was merged",
                ));
            }
        }
    }
    if let Some(path) = &task.task_path {
        for oid in [&state.local_base_oid, &state.remote_base_oid] {
            if repo
                .run_unchecked(["cat-file", "-e", &format!("{oid}:{path}")])?
                .success()
            {
                return Err(Error::message(format!(
                    "task path {path:?} already exists in a shared-branch tree; merged tasks cannot be discarded"
                )));
            }
        }
    }
    Ok(())
}

fn working_changes(repo: &GitRepo, task: &ResolvedTask) -> Result<Vec<String>> {
    let mut args = vec![
        "status".to_owned(),
        "--short".to_owned(),
        "--untracked-files=all".to_owned(),
    ];
    if task.kind == TaskKind::Deliverable {
        args.push("--".to_owned());
        args.extend(task.scopes());
    }
    Ok(repo
        .run(args)?
        .stdout
        .lines()
        .map(ToOwned::to_owned)
        .collect())
}

fn local_actions(
    repo: &GitRepo,
    task: &ResolvedTask,
    local_base_oid: &str,
) -> Result<Vec<LocalDiscardAction>> {
    if task.kind == TaskKind::Infrastructure {
        return Ok(vec![LocalDiscardAction {
            path: repo.root.display().to_string(),
            action: "delete-worktree".to_owned(),
            currently_present: repo.root.is_dir(),
            restored_from: None,
        }]);
    }
    let task_path = task
        .task_path
        .as_deref()
        .ok_or_else(|| Error::message("deliverable task has no task path"))?;
    let mut actions = vec![LocalDiscardAction {
        path: task_path.to_owned(),
        action: "delete".to_owned(),
        currently_present: path_exists(&resolved_under(&repo.root, task_path))?,
        restored_from: None,
    }];
    for scope in &task.additional_scopes {
        actions.push(LocalDiscardAction {
            path: scope.path.clone(),
            action: "restore".to_owned(),
            currently_present: path_exists(&resolved_under(&repo.root, &scope.path))?,
            restored_from: Some(format!("{local_base_oid}:{}", scope.path)),
        });
    }
    Ok(actions)
}

fn retained_s3(
    task_repo: &GitRepo,
    admin_repo: &GitRepo,
    task: &ResolvedTask,
    state: &DiscardSnapshot,
) -> Result<Vec<RetainedS3Boundary>> {
    let scopes = task.scopes();
    let mut records: BTreeMap<String, (BTreeSet<String>, BTreeSet<String>)> = BTreeMap::new();
    for pointer in dvc::discover(task_repo, &scopes)? {
        let raw = fs::read_to_string(resolved_under(&task_repo.root, &pointer))
            .at(resolved_under(&task_repo.root, &pointer))?;
        add_s3_record(&mut records, &pointer, "local", &raw)?;
    }
    if let Some(oid) = &state.remote_branch_oid {
        let mut args = vec![
            "ls-tree".to_owned(),
            "-r".to_owned(),
            "-z".to_owned(),
            "--name-only".to_owned(),
            oid.clone(),
            "--".to_owned(),
        ];
        args.extend(scopes);
        for pointer in admin_repo
            .run(args)?
            .stdout
            .split('\0')
            .filter(|path| path.ends_with(".dvc"))
        {
            let raw = admin_repo
                .run(["show", &format!("{oid}:{pointer}")])?
                .stdout;
            add_s3_record(&mut records, pointer, "remote-branch", &raw)?;
        }
    }
    Ok(records
        .into_iter()
        .map(|(pointer, (sources, versions))| RetainedS3Boundary {
            boundary: pointer.trim_end_matches(".dvc").to_owned(),
            sources: sources.into_iter().collect(),
            version_ids: versions.into_iter().collect(),
            disposition: "retained-not-purged",
        })
        .collect())
}

fn add_s3_record(
    records: &mut BTreeMap<String, (BTreeSet<String>, BTreeSet<String>)>,
    pointer: &str,
    source: &str,
    raw: &str,
) -> Result<()> {
    let parsed: serde_yaml::Value = serde_yaml::from_str(raw).map_err(|error| {
        Error::message(format!(
            "failed to inspect managed-storage versions in {pointer}: {error}"
        ))
    })?;
    let entry = records.entry(pointer.to_owned()).or_default();
    entry.0.insert(source.to_owned());
    collect_version_ids(&parsed, &mut entry.1);
    Ok(())
}

fn collect_version_ids(value: &serde_yaml::Value, found: &mut BTreeSet<String>) {
    match value {
        serde_yaml::Value::Mapping(mapping) => {
            for (key, value) in mapping {
                if key.as_str() == Some("version_id") {
                    if let Some(version) = value.as_str() {
                        found.insert(version.to_owned());
                    }
                }
                collect_version_ids(value, found);
            }
        }
        serde_yaml::Value::Sequence(values) => {
            for value in values {
                collect_version_ids(value, found);
            }
        }
        _ => {}
    }
}

fn report(
    context: &DiscardContext,
    status: &str,
    confirmation_plan: Option<PathBuf>,
    cleanup_warnings: Vec<String>,
) -> TaskDiscardReport {
    let required_before_confirm = if context.snapshot.remote_branch_oid.is_some() {
        "agent must close and verify the unmerged pull request for this branch".to_owned()
    } else {
        "agent must verify that no pull request exists for this unpublished branch".to_owned()
    };
    TaskDiscardReport {
        status: status.to_owned(),
        operation: "task-discard".to_owned(),
        kind: context.task.kind,
        task_id: context.task.task_id.clone(),
        branch: context.task.branch.clone(),
        remote: context.task.remote.clone(),
        base_branch: context.task.base_branch.clone(),
        manifest: context.task.manifest_path.display().to_string(),
        local_branch_oid: context.snapshot.local_branch_oid.clone(),
        remote_branch_oid: context.snapshot.remote_branch_oid.clone(),
        remote_base_oid: context.snapshot.remote_base_oid.clone(),
        working_changes: context.working_changes.clone(),
        local_actions: context.local_actions.clone(),
        review: DiscardReview {
            managed_by: "agent",
            required_before_confirm,
            must_be_unmerged: true,
            provider_state_verified_by_cli: false,
        },
        retained_s3: context.retained_s3.clone(),
        remote_writes: status == "discarded" && context.snapshot.remote_branch_oid.is_some(),
        confirmation_plan: confirmation_plan.map(|path| path.display().to_string()),
        cleanup_warnings,
    }
}

fn write_plan(path: &Path, plan: &DiscardPlan) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::message("discard plan has no parent"))?;
    fs::create_dir_all(parent).at(parent)?;
    let bytes = serde_json::to_vec_pretty(plan)
        .map_err(|error| Error::message(format!("failed to encode discard plan: {error}")))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).at(parent)?;
    use std::io::Write;
    temporary.write_all(&bytes).at(path)?;
    temporary.flush().at(path)?;
    temporary.persist(path).map_err(|error| Error::Io {
        path: path.to_path_buf(),
        source: error.error,
    })?;
    Ok(())
}

fn read_plan(path: &Path) -> Result<DiscardPlan> {
    let raw = fs::read_to_string(path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            Error::message(
                "no current discard confirmation plan; run `workspace-mgr task discard --dry-run` first",
            )
        } else {
            Error::Io {
                path: path.to_path_buf(),
                source,
            }
        }
    })?;
    let plan: DiscardPlan = serde_json::from_str(&raw)
        .map_err(|error| Error::message(format!("invalid private discard plan: {error}")))?;
    if plan.schema_version != DISCARD_PLAN_SCHEMA {
        return Err(Error::message(
            "private discard plan has an unsupported schema; rerun discard dry-run",
        ));
    }
    Ok(plan)
}

fn discard_deliverable(context: &mut DiscardContext) -> Result<Vec<String>> {
    let quarantine = create_quarantine(&context.admin_repo, &context.task.task_id)?;
    let scopes = context.task.scopes();
    if let Err(error) = prepare_base_scopes(
        &context.admin_repo,
        &context.task.additional_scopes,
        &context.snapshot.local_base_oid,
        &quarantine,
    ) {
        let _ = fs::remove_dir_all(&quarantine);
        return Err(error);
    }
    if let Err(error) = backup_shared_index(&context.admin_repo, &quarantine) {
        let _ = fs::remove_dir_all(&quarantine);
        return Err(error);
    }
    if let Err(error) = quarantine_scopes(&context.admin_repo, &scopes, &quarantine) {
        let rollback = restore_quarantined_scopes(&context.admin_repo, &scopes, &quarantine).err();
        if rollback.is_none() {
            let _ = fs::remove_dir_all(&quarantine);
        }
        return Err(combine_rollback(error, rollback));
    }
    if let Err(error) = install_base_scopes(
        &context.admin_repo,
        &context.task.additional_scopes,
        &quarantine,
    ) {
        return Err(rollback_deliverable(context, &quarantine, error));
    }
    if let Err(error) = reset_shared_index(context) {
        return Err(rollback_deliverable(context, &quarantine, error));
    }
    if let Err(error) = delete_remote_branch(context) {
        let remote = restore_remote_branch(context).err();
        let local = rollback_deliverable(context, &quarantine, error);
        return Err(combine_rollback(local, remote));
    }
    if let Err(error) = delete_local_branch(context) {
        let remote = restore_remote_branch(context).err();
        let local = rollback_deliverable(context, &quarantine, error);
        return Err(combine_rollback(local, remote));
    }
    Ok(finish_cleanup(context, &quarantine))
}

fn discard_infrastructure(context: &mut DiscardContext) -> Result<Vec<String>> {
    let quarantine = create_quarantine(&context.admin_repo, &context.task.task_id)?;
    let manifest =
        match fs::read_to_string(&context.task.manifest_path).at(&context.task.manifest_path) {
            Ok(manifest) => manifest,
            Err(error) => {
                let _ = fs::remove_dir_all(&quarantine);
                return Err(error);
            }
        };
    if let Err(error) = quarantine_worktree(&context.task_repo.root, &quarantine) {
        let rollback = restore_moved_worktree_entries(&context.task_repo.root, &quarantine).err();
        if rollback.is_none() {
            let _ = fs::remove_dir_all(&quarantine);
        }
        return Err(combine_rollback(error, rollback));
    }
    if let Err(error) = delete_remote_branch(context) {
        let restore = restore_moved_worktree_entries(&context.task_repo.root, &quarantine).err();
        let remote = restore_remote_branch(context).err();
        let cleanup = cleanup_restored_quarantine(&quarantine, &restore, &remote).err();
        return Err(combine_rollbacks(error, [restore, remote, cleanup]));
    }
    let removed = context.admin_repo.run_unchecked([
        "worktree",
        "remove",
        "--force",
        &context.task_repo.root.to_string_lossy(),
    ])?;
    if !removed.success() {
        let error = Error::message(format!(
            "failed to remove infrastructure worktree: {}",
            removed.stderr.trim()
        ));
        let worktree = restore_infrastructure(context, &quarantine, &manifest).err();
        let remote = restore_remote_branch(context).err();
        let cleanup = cleanup_restored_quarantine(&quarantine, &worktree, &remote).err();
        return Err(combine_rollbacks(error, [worktree, remote, cleanup]));
    }
    if let Err(error) = delete_local_branch(context) {
        let worktree = restore_infrastructure(context, &quarantine, &manifest).err();
        let remote = restore_remote_branch(context).err();
        let cleanup = cleanup_restored_quarantine(&quarantine, &worktree, &remote).err();
        return Err(combine_rollbacks(error, [worktree, remote, cleanup]));
    }
    Ok(finish_cleanup(context, &quarantine))
}

fn create_quarantine(repo: &GitRepo, task_id: &str) -> Result<PathBuf> {
    let parent = repo.common_dir()?.join("workspace-mgr/discard-quarantine");
    fs::create_dir_all(&parent).at(&parent)?;
    let directory = tempfile::Builder::new()
        .prefix(&format!("{task_id}-"))
        .tempdir_in(&parent)
        .at(&parent)?;
    Ok(directory.keep())
}

fn prepare_base_scopes(
    repo: &GitRepo,
    scopes: &[crate::manifest::AdditionalScope],
    base_oid: &str,
    quarantine: &Path,
) -> Result<()> {
    if scopes.is_empty() {
        return Ok(());
    }
    let index = quarantine.join("base-index");
    repo.run_with_index(&index, ["read-tree", base_oid], None, true)?;
    let mut list_args = vec!["ls-files".to_owned(), "-z".to_owned(), "--".to_owned()];
    list_args.extend(scopes.iter().map(|scope| scope.path.clone()));
    let paths = repo.run_with_index(&index, list_args, None, true)?.stdout;
    if paths.is_empty() {
        return Ok(());
    }
    let base = quarantine.join("base");
    fs::create_dir_all(&base).at(&base)?;
    let prefix = format!("{}/", base.to_string_lossy());
    repo.run_with_index(
        &index,
        [
            "checkout-index",
            "--force",
            "--stdin",
            "-z",
            &format!("--prefix={prefix}"),
        ],
        Some(&paths),
        true,
    )?;
    Ok(())
}

fn backup_shared_index(repo: &GitRepo, quarantine: &Path) -> Result<()> {
    let index = repo.git_dir()?.join("index");
    match fs::symlink_metadata(&index) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            fs::copy(&index, quarantine.join("shared-index")).at(&index)?;
            Ok(())
        }
        Ok(_) => Err(Error::message(
            "shared Git index is not a regular file and cannot be protected during discard",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Io {
            path: index,
            source,
        }),
    }
}

fn reset_shared_index(context: &DiscardContext) -> Result<()> {
    let mut args = vec![
        "reset".to_owned(),
        "-q".to_owned(),
        context.snapshot.local_base_oid.clone(),
        "--".to_owned(),
    ];
    args.extend(context.task.scopes());
    context.admin_repo.run(args)?;
    Ok(())
}

fn restore_shared_index(repo: &GitRepo, quarantine: &Path) -> Result<()> {
    let backup = quarantine.join("shared-index");
    if !backup.is_file() {
        return Ok(());
    }
    let index = repo.git_dir()?.join("index");
    fs::copy(&backup, &index).at(&index)?;
    Ok(())
}

fn quarantine_scopes(repo: &GitRepo, scopes: &[String], quarantine: &Path) -> Result<()> {
    for scope in scopes {
        ensure_safe_parent(&repo.root, scope)?;
        let source = resolved_under(&repo.root, scope);
        if !path_exists(&source)? {
            continue;
        }
        let destination = resolved_under(&quarantine.join("current"), scope);
        let parent = destination
            .parent()
            .ok_or_else(|| Error::message("quarantine destination has no parent"))?;
        fs::create_dir_all(parent).at(parent)?;
        fs::rename(&source, &destination).at(&source)?;
    }
    Ok(())
}

fn restore_quarantined_scopes(repo: &GitRepo, scopes: &[String], quarantine: &Path) -> Result<()> {
    for scope in scopes.iter().rev() {
        let backup = resolved_under(&quarantine.join("current"), scope);
        if !path_exists(&backup)? {
            continue;
        }
        let destination = resolved_under(&repo.root, scope);
        if path_exists(&destination)? {
            return Err(Error::message(format!(
                "cannot restore quarantined scope because its path reappeared: {scope}"
            )));
        }
        let parent = destination
            .parent()
            .ok_or_else(|| Error::message("restored quarantine scope has no parent"))?;
        fs::create_dir_all(parent).at(parent)?;
        fs::rename(&backup, &destination).at(&backup)?;
    }
    Ok(())
}

fn install_base_scopes(
    repo: &GitRepo,
    scopes: &[crate::manifest::AdditionalScope],
    quarantine: &Path,
) -> Result<()> {
    for scope in scopes {
        let source = resolved_under(&quarantine.join("base"), &scope.path);
        if !path_exists(&source)? {
            continue;
        }
        let destination = resolved_under(&repo.root, &scope.path);
        let parent = destination
            .parent()
            .ok_or_else(|| Error::message("restored scope has no parent"))?;
        fs::create_dir_all(parent).at(parent)?;
        fs::rename(&source, &destination).at(&source)?;
    }
    Ok(())
}

fn rollback_deliverable(context: &DiscardContext, quarantine: &Path, cause: Error) -> Error {
    let rollback: Result<()> = (|| {
        for scope in context.task.scopes().into_iter().rev() {
            let path = resolved_under(&context.admin_repo.root, &scope);
            remove_path(&path)?;
            let backup = resolved_under(&quarantine.join("current"), &scope);
            if path_exists(&backup)? {
                let parent = path
                    .parent()
                    .ok_or_else(|| Error::message("restored task scope has no parent"))?;
                fs::create_dir_all(parent).at(parent)?;
                fs::rename(&backup, &path).at(&backup)?;
            }
        }
        restore_shared_index(&context.admin_repo, quarantine)?;
        fs::remove_dir_all(quarantine).at(quarantine)?;
        Ok(())
    })();
    match rollback {
        Ok(()) => Error::message(format!(
            "task discard failed and local scopes were restored: {cause}"
        )),
        Err(error) => Error::message(format!(
            "task discard failed: {cause}; local rollback also failed: {error}; preserved quarantine: {}",
            quarantine.display()
        )),
    }
}

fn quarantine_worktree(worktree: &Path, quarantine: &Path) -> Result<()> {
    let git_file = worktree.join(".git");
    let metadata = fs::symlink_metadata(&git_file).at(&git_file)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(Error::message(
            "managed infrastructure checkout does not have a regular .git pointer",
        ));
    }
    let current = quarantine.join("current-worktree");
    fs::create_dir(&current).at(&current)?;
    for entry in fs::read_dir(worktree).at(worktree)? {
        let entry = entry.at(worktree)?;
        if entry.file_name() == ".git" {
            continue;
        }
        fs::rename(entry.path(), current.join(entry.file_name())).at(entry.path())?;
    }
    Ok(())
}

fn restore_moved_worktree_entries(worktree: &Path, quarantine: &Path) -> Result<()> {
    let current = quarantine.join("current-worktree");
    if !current.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(&current).at(&current)? {
        let entry = entry.at(&current)?;
        let destination = worktree.join(entry.file_name());
        if path_exists(&destination)? {
            return Err(Error::message(format!(
                "cannot restore quarantined worktree entry because its path reappeared: {}",
                destination.display()
            )));
        }
        fs::rename(entry.path(), destination).at(entry.path())?;
    }
    Ok(())
}

fn replace_worktree_entries(worktree: &Path, quarantine: &Path) -> Result<()> {
    for entry in fs::read_dir(worktree).at(worktree)? {
        let entry = entry.at(worktree)?;
        if entry.file_name() != ".git" {
            remove_path(&entry.path())?;
        }
    }
    restore_moved_worktree_entries(worktree, quarantine)
}

fn restore_infrastructure(
    context: &DiscardContext,
    quarantine: &Path,
    manifest: &str,
) -> Result<()> {
    let mut recreated = false;
    if !context.task_repo.root.join(".git").exists() {
        context.admin_repo.run([
            "worktree",
            "add",
            "--quiet",
            "--force",
            &context.task_repo.root.to_string_lossy(),
            &context.task.branch,
        ])?;
        recreated = true;
    }
    if recreated {
        replace_worktree_entries(&context.task_repo.root, quarantine)?;
    } else {
        restore_moved_worktree_entries(&context.task_repo.root, quarantine)?;
    }
    let restored = GitRepo::discover(&context.task_repo.root)?;
    let manifest_path = restored.git_dir()?.join(INFRASTRUCTURE_MANIFEST_NAME);
    let parent = manifest_path
        .parent()
        .ok_or_else(|| Error::message("restored manifest has no parent"))?;
    fs::create_dir_all(parent).at(parent)?;
    fs::write(&manifest_path, manifest).at(&manifest_path)?;
    Ok(())
}

fn delete_remote_branch(context: &DiscardContext) -> Result<()> {
    let Some(expected) = &context.snapshot.remote_branch_oid else {
        return Ok(());
    };
    let reference = format!("refs/heads/{}", context.task.branch);
    let lease = format!("--force-with-lease={reference}:{expected}");
    let deletion = format!(":{reference}");
    context.admin_repo.run([
        "push",
        "--porcelain",
        &lease,
        &context.task.remote,
        &deletion,
    ])?;
    if context
        .admin_repo
        .remote_branch_oid(&context.task.remote, &context.task.branch)?
        .is_some()
    {
        return Err(Error::message(
            "remote task branch still exists after discard deletion",
        ));
    }
    Ok(())
}

fn restore_remote_branch(context: &DiscardContext) -> Result<()> {
    let Some(oid) = &context.snapshot.remote_branch_oid else {
        return Ok(());
    };
    if let Some(existing) = context
        .admin_repo
        .remote_branch_oid(&context.task.remote, &context.task.branch)?
    {
        return if &existing == oid {
            Ok(())
        } else {
            Err(Error::message(format!(
                "remote task branch changed during discard rollback: found {existing}, expected {oid}"
            )))
        };
    }
    let reference = format!("refs/heads/{}", context.task.branch);
    let lease = format!("--force-with-lease={reference}:{ZERO_OID}");
    let refspec = format!("{oid}:{reference}");
    context.admin_repo.run([
        "push",
        "--porcelain",
        &lease,
        &context.task.remote,
        &refspec,
    ])?;
    let observed = context
        .admin_repo
        .remote_branch_oid(&context.task.remote, &context.task.branch)?;
    if observed.as_deref() != Some(oid) {
        return Err(Error::message(
            "failed to restore remote task branch after discard rollback",
        ));
    }
    Ok(())
}

fn cleanup_restored_quarantine(
    quarantine: &Path,
    first: &Option<Error>,
    second: &Option<Error>,
) -> Result<()> {
    if first.is_none() && second.is_none() {
        fs::remove_dir_all(quarantine).at(quarantine)?;
    }
    Ok(())
}

fn delete_local_branch(context: &DiscardContext) -> Result<()> {
    if let Some(expected) = &context.snapshot.local_branch_oid {
        context.admin_repo.run([
            "update-ref",
            "-d",
            &format!("refs/heads/{}", context.task.branch),
            expected,
        ])?;
    }
    if context
        .admin_repo
        .optional_oid(&format!("refs/heads/{}", context.task.branch))?
        .is_some()
    {
        return Err(Error::message(
            "local task branch still exists after discard deletion",
        ));
    }
    Ok(())
}

fn finish_cleanup(context: &DiscardContext, quarantine: &Path) -> Vec<String> {
    let mut warnings = Vec::new();
    if let Some(expected) = &context.snapshot.remote_tracking_oid {
        let reference = format!(
            "refs/remotes/{}/{}",
            context.task.remote, context.task.branch
        );
        match context.admin_repo.optional_oid(&reference) {
            Ok(None) => {}
            Ok(Some(observed)) if &observed == expected => {
                if let Err(error) = context
                    .admin_repo
                    .run(["update-ref", "-d", &reference, expected])
                {
                    warnings.push(format!("failed to remove remote-tracking ref: {error}"));
                }
            }
            Ok(Some(observed)) => warnings.push(format!(
                "preserved changed remote-tracking ref {reference}: found {observed}, expected {expected}"
            )),
            Err(error) => warnings.push(format!(
                "failed to inspect remote-tracking ref before cleanup: {error}"
            )),
        }
    }
    if let Err(error) = fs::remove_dir_all(&context.state_dir) {
        if error.kind() != std::io::ErrorKind::NotFound {
            warnings.push(format!(
                "failed to remove private task state {}: {error}",
                context.state_dir.display()
            ));
        }
    }
    if let Err(error) = fs::remove_dir_all(quarantine) {
        if error.kind() != std::io::ErrorKind::NotFound {
            warnings.push(format!(
                "failed to remove private discard quarantine {}: {error}",
                quarantine.display()
            ));
        }
    }
    warnings
}

fn invocation_would_be_deleted(context: &DiscardContext) -> Result<bool> {
    let current = std::env::current_dir().map_err(|source| Error::Io {
        path: PathBuf::from("."),
        source,
    })?;
    let current = current.canonicalize().map_err(|source| Error::Io {
        path: current.clone(),
        source,
    })?;
    let deleted_root = match context.task.kind {
        TaskKind::Deliverable => {
            let task_path = context
                .task
                .task_path
                .as_deref()
                .ok_or_else(|| Error::message("deliverable task has no task path"))?;
            resolved_under(&context.task_repo.root, task_path)
        }
        TaskKind::Infrastructure => context.task_repo.root.clone(),
    };
    Ok(current.starts_with(deleted_root))
}

fn ensure_safe_parent(root: &Path, relative: &str) -> Result<()> {
    let relative = repo_path(relative, "discard scope")?;
    let parent = Path::new(&relative)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    let mut current = root.to_path_buf();
    for component in parent.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(Error::message(format!(
                    "discard scope may not traverse a symlink: {relative:?}"
                )));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(Error::message(format!(
                    "discard scope has a non-directory ancestor: {relative:?}"
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(source) => {
                return Err(Error::Io {
                    path: current,
                    source,
                });
            }
        }
    }
    Ok(())
}

fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(Error::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn remove_path(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path).at(path)
        }
        Ok(_) => fs::remove_file(path).at(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn combine_rollback(cause: Error, rollback: Option<Error>) -> Error {
    match rollback {
        Some(rollback) => Error::message(format!(
            "{cause}; an additional rollback step failed: {rollback}"
        )),
        None => cause,
    }
}

fn combine_rollbacks<const N: usize>(cause: Error, rollbacks: [Option<Error>; N]) -> Error {
    let details = rollbacks
        .into_iter()
        .flatten()
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    if details.is_empty() {
        cause
    } else {
        Error::message(format!(
            "{cause}; rollback also failed: {}",
            details.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{replace_worktree_entries, restore_moved_worktree_entries};

    #[test]
    fn partial_worktree_restore_preserves_entries_that_never_moved() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let worktree = temporary.path().join("worktree");
        let quarantine = temporary.path().join("quarantine");
        let moved = quarantine.join("current-worktree");
        std::fs::create_dir_all(&worktree).expect("worktree directory");
        std::fs::create_dir_all(&moved).expect("quarantine directory");
        std::fs::write(worktree.join("still-here.txt"), "original\n").expect("unmoved entry");
        std::fs::write(moved.join("moved.txt"), "moved\n").expect("moved entry");

        restore_moved_worktree_entries(&worktree, &quarantine).expect("restore moved entries");

        assert_eq!(
            std::fs::read_to_string(worktree.join("still-here.txt"))
                .expect("preserved unmoved entry"),
            "original\n"
        );
        assert_eq!(
            std::fs::read_to_string(worktree.join("moved.txt")).expect("restored moved entry"),
            "moved\n"
        );
    }

    #[test]
    fn recreated_worktree_restore_replaces_checkout_materialization() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let worktree = temporary.path().join("worktree");
        let quarantine = temporary.path().join("quarantine");
        let moved = quarantine.join("current-worktree");
        std::fs::create_dir_all(&worktree).expect("worktree directory");
        std::fs::create_dir_all(&moved).expect("quarantine directory");
        std::fs::write(worktree.join(".git"), "gitdir: elsewhere\n").expect("git pointer");
        std::fs::write(worktree.join("tracked.txt"), "checkout copy\n")
            .expect("checkout materialization");
        std::fs::write(moved.join("tracked.txt"), "local copy\n").expect("quarantined entry");

        replace_worktree_entries(&worktree, &quarantine).expect("replace checkout entries");

        assert!(worktree.join(".git").is_file());
        assert_eq!(
            std::fs::read_to_string(worktree.join("tracked.txt")).expect("restored local entry"),
            "local copy\n"
        );
    }
}
