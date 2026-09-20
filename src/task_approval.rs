use std::fs;
use std::path::PathBuf;

use serde::Serialize;

use crate::cloud_usage::{
    self, PendingDecision, effective_limit, effective_threshold, format_bytes,
};
use crate::config::Config;
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::lock::RepositoryLock;
use crate::manifest::{CloudUsageApproval, ResolvedTask, TaskKind, one_line};
use crate::task_rename::{atomic_write, validate_checkout};
use crate::transaction::task_state_dir;

/// The largest limit a TOML integer in the task manifest can hold.
const LARGEST_LIMIT_BYTES: u64 = i64::MAX as u64;

#[derive(Debug, Clone)]
pub struct CloudUsageApprovalOptions {
    pub start: PathBuf,
    pub manifest: Option<PathBuf>,
    pub limit_bytes: u64,
    pub note: String,
    /// Describes the user's authorization of an alternate checkout head.
    pub scope_note: Option<String>,
    pub allow_non_shared_head: bool,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CloudUsageApprovalReport {
    pub status: String,
    pub operation: String,
    pub task_id: String,
    pub manifest: String,
    pub schema_version: u32,
    pub threshold_bytes: u64,
    pub previous_limit_bytes: u64,
    pub limit_bytes: u64,
    pub limit: String,
    pub note: String,
    pub pending: Option<PendingDecision>,
    pub blocked: bool,
    pub remote_writes: bool,
    pub next_step: String,
}

/// Records the user's decision about this task's cloud-usage limit in the
/// task manifest. A limit equal to the threshold removes the approval.
pub fn approve(options: &CloudUsageApprovalOptions) -> Result<CloudUsageApprovalReport> {
    let threshold_bytes = effective_threshold();
    if options.limit_bytes < threshold_bytes {
        return Err(Error::message(format!(
            "approved cloud-usage limit {} is below the threshold {}; an approval can only raise the limit",
            format_bytes(options.limit_bytes),
            format_bytes(threshold_bytes)
        )));
    }
    if options.limit_bytes > LARGEST_LIMIT_BYTES {
        return Err(Error::message(format!(
            "approved cloud-usage limit {} is larger than the largest recordable limit {}",
            format_bytes(options.limit_bytes),
            format_bytes(LARGEST_LIMIT_BYTES)
        )));
    }
    let note = one_line(&options.note, "approval note")?;
    let repo = match &options.manifest {
        Some(path) => GitRepo::discover_for_manifest(path)?,
        None => GitRepo::discover(&options.start)?,
    };
    let _repository_lock = RepositoryLock::acquire(&repo)?;
    let config = Config::load_compatible(&repo)?;
    let manifest_path = match &options.manifest {
        Some(path) => path.clone(),
        None => ResolvedTask::discover(&repo, &options.start)?,
    };
    let task = ResolvedTask::load(&repo, &config, &manifest_path)?;
    validate_approval_checkout(&repo, &task, options)?;
    let state_dir = task_state_dir(&repo.common_dir()?, &task);
    let state = cloud_usage::load_state(&state_dir, &task.task_id, &task.branch)?;
    let previous_limit_bytes = effective_limit(threshold_bytes, task.cloud_usage_approval.as_ref());
    let mut manifest = task.manifest();
    manifest.cloud_usage_approval =
        (options.limit_bytes > threshold_bytes).then(|| CloudUsageApproval {
            limit_bytes: options.limit_bytes,
            note: note.clone(),
        });
    let schema_version = manifest.minimal_schema_version();
    let rendered = manifest.render()?;
    let current = fs::read_to_string(&task.manifest_path).at(&task.manifest_path)?;
    let changed = current != rendered;
    let blocked = state
        .pending
        .as_ref()
        .is_some_and(|pending| pending.exceeds(options.limit_bytes));
    if !options.dry_run && changed {
        write_manifest(&repo, &config, &task, &current, &rendered)?;
    }
    let next_step = next_step(&Outcome {
        dry_run: options.dry_run,
        changed,
        blocked,
        pending: state.pending.is_some(),
        kind: task.kind,
        previously_approved: task.cloud_usage_approval.is_some(),
        approved: manifest.cloud_usage_approval.is_some(),
    });
    Ok(CloudUsageApprovalReport {
        status: if options.dry_run {
            "dry_run"
        } else if changed {
            "recorded"
        } else {
            "unchanged"
        }
        .to_owned(),
        operation: "task-approve-cloud-usage".to_owned(),
        task_id: task.task_id,
        manifest: task.manifest_path.display().to_string(),
        schema_version,
        threshold_bytes,
        previous_limit_bytes,
        limit_bytes: options.limit_bytes,
        limit: format_bytes(options.limit_bytes),
        note,
        pending: state.pending,
        blocked,
        remote_writes: false,
        next_step,
    })
}

/// Only a checkout that publishes this manifest may record the decision, so a
/// copy in another checkout, such as an infrastructure worktree's copy of a
/// merged deliverable, is never written by accident. A deliverable follows
/// publication's rules for its checkout head: the shared checkout on the
/// shared branch, or another head in an explicitly authorized alternate
/// workflow.
fn validate_approval_checkout(
    repo: &GitRepo,
    task: &ResolvedTask,
    options: &CloudUsageApprovalOptions,
) -> Result<()> {
    if let Some(note) = &options.scope_note {
        one_line(note, "scope note")?;
    }
    if task.kind == TaskKind::Infrastructure {
        return validate_checkout(repo, task, "cloud-usage approval");
    }
    let head = repo.current_branch()?;
    if head.as_deref() != Some(&task.shared_head) {
        if !options.allow_non_shared_head {
            return Err(Error::message(format!(
                "deliverable cloud-usage approval must run from the shared checkout on {:?}; current branch is {:?}; use an explicitly authorized alternate workflow or --allow-non-shared-head with --scope-note",
                task.shared_head,
                head.as_deref().unwrap_or("detached HEAD")
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
    repo.ensure_branch_not_checked_out(&task.branch)
}

/// Replaces the manifest atomically and keeps it only if it still resolves to
/// a valid task.
fn write_manifest(
    repo: &GitRepo,
    config: &Config,
    task: &ResolvedTask,
    original: &str,
    rendered: &str,
) -> Result<()> {
    atomic_write(&task.manifest_path, rendered)?;
    if let Err(error) = ResolvedTask::load(repo, config, &task.manifest_path) {
        return Err(match atomic_write(&task.manifest_path, original) {
            Ok(()) => error,
            Err(rollback) => Error::message(format!(
                "cloud-usage approval failed: {error}; manifest rollback also failed: {rollback}"
            )),
        });
    }
    Ok(())
}

/// What an approval did, as far as the agent's next step depends on it.
struct Outcome {
    dry_run: bool,
    /// The rendered manifest differs from the file.
    changed: bool,
    blocked: bool,
    pending: bool,
    kind: TaskKind,
    previously_approved: bool,
    approved: bool,
}

/// What the agent does after recording, or rehearsing, the user's approval.
/// `blocked` compares with the pending projection of the last measurement,
/// which only `plan` and `publish` refresh.
fn next_step(outcome: &Outcome) -> String {
    const STILL_EXCEEDS: &str = "The pending projection from the last `workspace-mgr plan` or `workspace-mgr publish` still exceeds this limit.";
    const CHOOSE: &str = "If the user also chose cleanup, perform it and run `workspace-mgr plan`; otherwise ask the user for a larger limit or cleanup.";
    if outcome.dry_run {
        let mut step = "Nothing was recorded. Rerun without `--dry-run` only after the user approved this limit in this chat.".to_owned();
        if !outcome.changed {
            step.push_str(" The task manifest already records this decision, so recording it would change nothing.");
        }
        if outcome.blocked {
            step.push(' ');
            step.push_str(STILL_EXCEEDS);
        }
        return step;
    }
    if !outcome.changed {
        let unchanged = match (outcome.kind, outcome.approved) {
            (TaskKind::Deliverable, true) => {
                "The task manifest already records this approval, so this command changed nothing."
            }
            (TaskKind::Deliverable, false) => {
                "The task manifest already records no approval, so the task keeps the default limit and this command changed nothing."
            }
            (TaskKind::Infrastructure, true) => {
                "The private infrastructure task manifest already records this approval, so this command changed nothing."
            }
            (TaskKind::Infrastructure, false) => {
                "The private infrastructure task manifest already records no approval, so the task keeps the default limit and this command changed nothing."
            }
        };
        if outcome.blocked {
            return format!("{STILL_EXCEEDS} {CHOOSE} {unchanged}");
        }
        // An earlier run may have recorded the same decision without a
        // publication since, so only `plan` knows whether a deliverable
        // manifest change is still waiting to be published.
        let plan = match (outcome.kind, outcome.pending) {
            (TaskKind::Deliverable, true) => {
                " Run `workspace-mgr plan` to re-measure the task and to see whether earlier manifest changes are still unpublished."
            }
            (TaskKind::Deliverable, false) => {
                " Run `workspace-mgr plan` to see whether earlier manifest changes are still unpublished."
            }
            (TaskKind::Infrastructure, true) => " Run `workspace-mgr plan` to re-measure the task.",
            (TaskKind::Infrastructure, false) => "",
        };
        return format!("{unchanged}{plan}");
    }
    let publication = match (outcome.kind, outcome.previously_approved, outcome.approved) {
        (TaskKind::Deliverable, _, true) => {
            "The next publication carries the task manifest with this approval and a `Cloud-Usage-Approval` commit trailer, and raises the repository's `minimum_cli_version` when the manifest needs a newer workspace-mgr."
        }
        (TaskKind::Deliverable, true, false) => {
            "The task manifest no longer records an approval; the next publication carries that change and withdraws the task branch's `minimum_cli_version` raise, down to the base branch's declaration, when no task manifest in it still needs one."
        }
        (TaskKind::Deliverable, false, false) => {
            "The task manifest records no approval; the next publication carries its rewritten canonical form."
        }
        (TaskKind::Infrastructure, _, true) => {
            "The infrastructure task manifest stays private; each publication records this approval as a `Cloud-Usage-Approval` commit trailer."
        }
        (TaskKind::Infrastructure, true, false) => {
            "The private infrastructure task manifest no longer records an approval."
        }
        (TaskKind::Infrastructure, false, false) => {
            "The private infrastructure task manifest records no approval and was rewritten in its canonical form."
        }
    };
    if outcome.blocked {
        return format!("{STILL_EXCEEDS} {CHOOSE} {publication}");
    }
    format!("Run `workspace-mgr plan` to re-measure the task, then publish. {publication}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(limit_bytes: u64, note: &str) -> CloudUsageApprovalOptions {
        CloudUsageApprovalOptions {
            start: PathBuf::from("/nonexistent/workspace-mgr-approval"),
            manifest: None,
            limit_bytes,
            note: note.to_owned(),
            scope_note: None,
            allow_non_shared_head: false,
            dry_run: true,
        }
    }

    #[test]
    fn approvals_below_the_threshold_or_without_a_one_line_note_are_refused() {
        let threshold = effective_threshold();
        if let Some(below) = threshold.checked_sub(1) {
            let error = approve(&options(below, "Approved in chat"))
                .unwrap_err()
                .to_string();
            assert!(error.contains("below the threshold"), "{error}");
        }
        let error = approve(&options(threshold, "first\nsecond"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("approval note must be a single line"),
            "{error}"
        );
        let error = approve(&options(threshold, "  ")).unwrap_err().to_string();
        assert!(error.contains("approval note must not be empty"), "{error}");
        let error = approve(&options(LARGEST_LIMIT_BYTES + 1, "Approved in chat"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("larger than the largest recordable limit"),
            "{error}"
        );
    }

    fn outcome(kind: TaskKind, previously_approved: bool, approved: bool) -> Outcome {
        Outcome {
            dry_run: false,
            changed: true,
            blocked: false,
            pending: false,
            kind,
            previously_approved,
            approved,
        }
    }

    #[test]
    fn next_steps_match_what_was_recorded() {
        let recorded = next_step(&outcome(TaskKind::Deliverable, false, true));
        assert!(recorded.contains("then publish"), "{recorded}");
        assert!(recorded.contains("the task manifest with this approval"));
        assert!(recorded.contains("`Cloud-Usage-Approval` commit trailer"));
        assert!(recorded.contains("`minimum_cli_version`"));

        let reset = next_step(&outcome(TaskKind::Deliverable, true, false));
        assert!(reset.contains("no longer records an approval"), "{reset}");
        assert!(reset.contains("withdraws the task branch's `minimum_cli_version` raise"));
        assert!(!reset.contains("Cloud-Usage-Approval"), "{reset}");

        // A legacy manifest rewritten without an approval never had one.
        let rewritten = next_step(&outcome(TaskKind::Deliverable, false, false));
        assert!(!rewritten.contains("no longer"), "{rewritten}");
        assert!(rewritten.contains("records no approval"), "{rewritten}");

        let infrastructure = next_step(&outcome(TaskKind::Infrastructure, false, true));
        assert!(infrastructure.contains("stays private"), "{infrastructure}");
        assert!(!infrastructure.contains("minimum_cli_version"));

        let blocked = next_step(&Outcome {
            blocked: true,
            pending: true,
            ..outcome(TaskKind::Deliverable, false, true)
        });
        assert!(blocked.contains("still exceeds this limit"), "{blocked}");
        assert!(
            blocked.contains(
                "If the user also chose cleanup, perform it and run `workspace-mgr plan`"
            )
        );
        assert!(blocked.contains("otherwise ask the user for a larger limit or cleanup"));

        // A dry run records nothing, so it never says to go on and publish.
        for (blocked, changed, rehearsal) in [
            (
                false,
                true,
                next_step(&Outcome {
                    dry_run: true,
                    ..outcome(TaskKind::Deliverable, false, true)
                }),
            ),
            (
                true,
                true,
                next_step(&Outcome {
                    dry_run: true,
                    blocked: true,
                    pending: true,
                    ..outcome(TaskKind::Infrastructure, false, true)
                }),
            ),
            (
                false,
                false,
                next_step(&Outcome {
                    dry_run: true,
                    changed: false,
                    ..outcome(TaskKind::Deliverable, true, true)
                }),
            ),
        ] {
            assert!(
                rehearsal.starts_with("Nothing was recorded."),
                "{rehearsal}"
            );
            assert!(rehearsal.contains(
                "Rerun without `--dry-run` only after the user approved this limit in this chat."
            ));
            assert!(!rehearsal.contains("then publish"), "{rehearsal}");
            assert!(
                !rehearsal.contains("run `workspace-mgr plan`"),
                "{rehearsal}"
            );
            assert_eq!(rehearsal.contains("still exceeds this limit"), blocked);
            assert_eq!(rehearsal.contains("would change nothing"), !changed);
        }
    }

    #[test]
    fn unchanged_manifests_report_that_this_command_changed_nothing() {
        for (kind, approved, expected, idle, pending) in [
            (
                TaskKind::Deliverable,
                true,
                "The task manifest already records this approval, so this command changed nothing.",
                " Run `workspace-mgr plan` to see whether earlier manifest changes are still unpublished.",
                " Run `workspace-mgr plan` to re-measure the task and to see whether earlier manifest changes are still unpublished.",
            ),
            (
                TaskKind::Deliverable,
                false,
                "The task manifest already records no approval, so the task keeps the default limit and this command changed nothing.",
                " Run `workspace-mgr plan` to see whether earlier manifest changes are still unpublished.",
                " Run `workspace-mgr plan` to re-measure the task and to see whether earlier manifest changes are still unpublished.",
            ),
            (
                TaskKind::Infrastructure,
                true,
                "The private infrastructure task manifest already records this approval, so this command changed nothing.",
                "",
                " Run `workspace-mgr plan` to re-measure the task.",
            ),
            (
                TaskKind::Infrastructure,
                false,
                "The private infrastructure task manifest already records no approval, so the task keeps the default limit and this command changed nothing.",
                "",
                " Run `workspace-mgr plan` to re-measure the task.",
            ),
        ] {
            let unchanged = Outcome {
                changed: false,
                ..outcome(kind, approved, approved)
            };
            // An earlier recording of the same decision may still be
            // unpublished, so nothing claims that no change is pending.
            assert_eq!(next_step(&unchanged), format!("{expected}{idle}"));
            assert_eq!(
                next_step(&Outcome {
                    pending: true,
                    ..unchanged
                }),
                format!("{expected}{pending}")
            );
            let blocked = next_step(&Outcome {
                pending: true,
                blocked: true,
                ..outcome(kind, approved, approved)
            });
            let blocked_unchanged = next_step(&Outcome {
                pending: true,
                blocked: true,
                changed: false,
                ..outcome(kind, approved, approved)
            });
            assert!(blocked.starts_with("The pending projection"), "{blocked}");
            assert!(
                blocked_unchanged.starts_with("The pending projection")
                    && blocked_unchanged.ends_with(expected),
                "{blocked_unchanged}"
            );
            for step in [next_step(&unchanged), blocked_unchanged] {
                assert!(!step.contains("no manifest change is pending"), "{step}");
                assert!(!step.contains("next publication"), "{step}");
                assert!(!step.contains("then publish"), "{step}");
            }
        }
    }
}
