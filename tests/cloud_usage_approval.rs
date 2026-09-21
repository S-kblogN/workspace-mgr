mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Output;

use common::*;
use serde_json::{Value, json};

const REAL_THRESHOLD: u64 = 1_073_741_824;
const TRAILER: &str = "Cloud-Usage-Approval";
const CONFIG: &str = ".workspace-mgr.toml";
const MANIFEST: &str = ".workspace-mgr-task.toml";
const REMINDER: &str = "is waiting for the user's cloud-usage decision";

fn managed_fixture(storage: bool) -> GitFixture {
    let fixture = GitFixture::new();
    if storage {
        workspace(
            &fixture.seed,
            [
                "init",
                "--s3-url",
                fixture.root.join("storage-remote").to_str().unwrap(),
            ],
        );
    } else {
        workspace(&fixture.seed, ["init"]);
    }
    fixture.commit_seed("Initialize workspace");
    fixture.clone_shared();
    fixture
}

fn create_task(fixture: &GitFixture, slug: &str, timestamp: &str) -> (String, PathBuf) {
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            slug,
            "--title",
            "Cloud usage",
            "--purpose",
            "Verify the cloud-usage approval gate.",
            "--timestamp",
            timestamp,
        ],
    );
    let task_id = format!("{timestamp}-{slug}");
    let task = fixture.shared.join(&task_id);
    (task_id, task)
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_refused(output: &Output, needles: &[&str]) {
    let message = stderr(output);
    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout:\n{}\nstderr:\n{message}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(output.stdout.is_empty(), "a refusal prints no report");
    for needle in needles {
        assert!(
            message.contains(needle),
            "missing {needle:?} in:\n{message}"
        );
    }
}

fn assert_no_reminder(output: &Output) {
    let message = stderr(output);
    assert!(
        !message.contains(REMINDER),
        "unexpected reminder:\n{message}"
    );
}

fn commit_message(repo: &Path, oid: &str) -> String {
    String::from_utf8_lossy(&git(repo, ["show", "-s", "--format=%B", oid]).stdout).into_owned()
}

#[cfg(feature = "test-storage")]
fn last_line(message: &str) -> Option<&str> {
    message.trim_end().lines().last()
}

/// Uncompressed size of every object `git rev-list --objects` lists.
fn object_bytes(repo: &Path, revisions: &[&str]) -> u64 {
    let mut args = vec!["rev-list", "--objects"];
    args.extend_from_slice(revisions);
    let listing = git(repo, args);
    String::from_utf8_lossy(&listing.stdout)
        .lines()
        .filter_map(|line| line.split(' ').next())
        .filter(|oid| !oid.is_empty())
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>()
        .iter()
        .map(|oid| {
            let size = git(repo, ["cat-file", "-s", oid]);
            String::from_utf8_lossy(&size.stdout)
                .trim()
                .parse::<u64>()
                .unwrap()
        })
        .sum()
}

fn show(repo: &Path, object: &str) -> String {
    String::from_utf8_lossy(&git(repo, ["show", object]).stdout).into_owned()
}

/// The private per-task state directories plan and publish create.
fn task_state_dirs(repo: &Path) -> Vec<PathBuf> {
    let root = repo.join(".git/workspace-mgr/state");
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect()
}

fn cloud_usage_state(repo: &Path) -> Vec<PathBuf> {
    let root = repo.join(".git/workspace-mgr/state");
    if !root.exists() {
        return Vec::new();
    }
    walkdir::WalkDir::new(root)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_name() == "cloud-usage.json")
        .map(|entry| entry.into_path())
        .collect()
}

fn file_contributor(task: &Path, task_id: &str, name: &str, state: &str) -> Value {
    json!({
        "path": format!("{task_id}/{name}"),
        "store": "git",
        "bytes": std::fs::metadata(task.join(name)).unwrap().len(),
        "versions": 1,
        "state": state,
    })
}

#[test]
fn usage_within_the_limit_is_reported_without_private_state() {
    let fixture = managed_fixture(false);
    let (task_id, task) = create_task(&fixture, "usage-report", "20260918-110000");
    std::fs::write(task.join("notes.md"), "notes\n").unwrap();

    let output = workspace(&task, ["plan"]);
    assert_no_reminder(&output);
    let text = String::from_utf8_lossy(&output.stdout);
    let operation = text.find("\"operation\"").unwrap();
    let usage_key = text.find("\"cloud_usage\"").unwrap();
    assert!(operation < usage_key && usage_key < text.find("\"head\"").unwrap());
    let plan = json(&output);
    let base = plan["remote_base_oid"].as_str().unwrap();
    let base_tree = format!("{base}^{{tree}}");
    let tree = plan["tree_oid"].as_str().unwrap();
    let projected = object_bytes(&fixture.shared, &[tree, "--not", base, &base_tree]);
    let zero = json!({
        "git_bytes": 0,
        "git_uncompressed_bytes": 0,
        "git_lfs_bytes": 0,
        "s3_bytes": 0,
        "total_bytes": 0,
    });
    let mut contributors = [".workspace-mgr-task.toml", "README.md", "notes.md"]
        .map(|name| file_contributor(&task, &task_id, name, "pending"));
    contributors.sort_by_key(|contributor| {
        (
            std::cmp::Reverse(contributor["bytes"].as_u64()),
            contributor["path"].to_string(),
        )
    });
    let expected = json!({
        "status": "within_limit",
        "publish_allowed": true,
        "cleanup_only": false,
        "git_history_exceeds_limit": false,
        "threshold_bytes": REAL_THRESHOLD,
        "limit_bytes": REAL_THRESHOLD,
        "approval": null,
        "published": zero,
        "projected": {
            "git_bytes": projected,
            "git_uncompressed_bytes": projected,
            "git_lfs_bytes": 0,
            "s3_bytes": 0,
            "total_bytes": projected,
        },
        "git_measure": "uncompressed",
        "headroom_bytes": REAL_THRESHOLD - projected,
        "suggested_limit_bytes": 536_870_912,
        "contributors": contributors,
    });
    assert_eq!(plan["cloud_usage"], expected);
    assert!(cloud_usage_state(&fixture.shared).is_empty());

    let published = json(&workspace(&task, ["publish", "-m", "Publish notes"]));
    assert_eq!(published["status"], "pushed");
    assert_eq!(published["cloud_usage"], expected);
    let tip = published["remote_oid"].as_str().unwrap();
    assert!(!commit_message(&fixture.remote, tip).contains(TRAILER));
    assert!(cloud_usage_state(&fixture.shared).is_empty());
    assert_eq!(
        json(&workspace(&task, ["task", "status"]))["cloud_usage"],
        json!({
            "threshold_bytes": REAL_THRESHOLD,
            "limit_bytes": REAL_THRESHOLD,
            "approval": null,
            "pending": null,
        })
    );

    let after = json(&workspace(&task, ["plan"]));
    assert_eq!(after["status"], "no_changes");
    let usage = &after["cloud_usage"];
    let history = object_bytes(&fixture.shared, &[tip, "--not", base, &base_tree]);
    let totals = json!({
        "git_bytes": history,
        "git_uncompressed_bytes": history,
        "git_lfs_bytes": 0,
        "s3_bytes": 0,
        "total_bytes": history,
    });
    assert_eq!(usage["status"], "within_limit");
    assert_eq!(usage["published"], totals);
    assert_eq!(usage["projected"], totals);
    assert_eq!(usage["cleanup_only"], true);
    assert!(
        usage["contributors"]
            .as_array()
            .unwrap()
            .iter()
            .all(|contributor| contributor["state"] == "published")
    );
}

#[test]
fn approvals_are_validated_recorded_in_the_manifest_and_published() {
    let fixture = managed_fixture(false);
    let (task_id, task) = create_task(&fixture, "usage-approval", "20260918-113000");
    let manifest = task.join(".workspace-mgr-task.toml");
    let manifest_path = format!("{task_id}/.workspace-mgr-task.toml");
    let manifest_before = std::fs::read_to_string(&manifest).unwrap();
    let config_before = std::fs::read_to_string(fixture.shared.join(CONFIG)).unwrap();
    let note = "The user approved 1.5 GiB for evaluation outputs in this chat";
    let approve = |limit: &str, note: &str| {
        workspace_unchecked(
            &task,
            [
                "task",
                "approve-cloud-usage",
                "--limit",
                limit,
                "--note",
                note,
            ],
        )
    };

    let below = approve("1073741823", note);
    assert_refused(&below, &[]);
    assert_eq!(
        stderr(&below),
        "workspace-mgr: approved cloud-usage limit 1023.99 MiB (1073741823 bytes) is below the threshold 1 GiB (1073741824 bytes); an approval can only raise the limit\n"
    );
    for (limit, error) in [
        ("1G", "invalid value '1G' for '--limit <LIMIT>'"),
        ("1.5", "a fractional size needs a unit"),
        ("1.0000000005GB", "is not a whole number of bytes"),
        ("20000000 TB", "is too large"),
        (
            "9223372036854775808",
            "larger than the largest recordable limit",
        ),
    ] {
        assert_refused(&approve(limit, note), &[error]);
    }
    assert_refused(
        &approve("1.5GiB", "The user approved\nin two lines"),
        &["approval note must be a single line"],
    );
    assert_refused(
        &approve("1.5GiB", "  "),
        &["approval note must not be empty"],
    );
    assert_refused(
        &workspace_unchecked(&task, ["task", "approve-cloud-usage", "--limit", "2GiB"]),
        &["--note <NOTE>"],
    );
    assert_eq!(std::fs::read_to_string(&manifest).unwrap(), manifest_before);

    let dry = json(&workspace(
        &task,
        [
            "task",
            "approve-cloud-usage",
            "--limit",
            "1.5 GiB",
            "--note",
            note,
            "--dry-run",
        ],
    ));
    assert_eq!(dry["status"], "dry_run");
    assert_eq!(dry["operation"], "task-approve-cloud-usage");
    assert_eq!(dry["task_id"], task_id);
    assert_eq!(
        dry["manifest"],
        manifest.canonicalize().unwrap().to_str().unwrap()
    );
    assert_eq!(dry["schema_version"], 3);
    assert_eq!(dry["threshold_bytes"], REAL_THRESHOLD);
    assert_eq!(dry["previous_limit_bytes"], REAL_THRESHOLD);
    assert_eq!(dry["limit_bytes"], 1_610_612_736_u64);
    assert_eq!(dry["limit"], "1.5 GiB (1610612736 bytes)");
    assert_eq!(dry["note"], note);
    assert_eq!(dry["pending"], Value::Null);
    assert_eq!(dry["blocked"], false);
    assert_eq!(dry["remote_writes"], false);
    // A rehearsal records nothing, so it never sends the agent on to publish.
    assert_eq!(
        dry["next_step"],
        "Nothing was recorded. Rerun without `--dry-run` only after the user approved this limit in this chat."
    );
    assert_eq!(std::fs::read_to_string(&manifest).unwrap(), manifest_before);

    let recorded = approve("1.5GiB", note);
    assert_no_reminder(&recorded);
    let recorded = json(&recorded);
    assert_eq!(recorded["status"], "recorded");
    assert_eq!(recorded["schema_version"], 3);
    assert_eq!(recorded["limit_bytes"], 1_610_612_736_u64);
    assert!(recorded.get("recorded_at").is_none());
    let next_step = recorded["next_step"].as_str().unwrap();
    assert!(
        next_step.contains("`Cloud-Usage-Approval` commit trailer"),
        "{next_step}"
    );
    assert!(next_step.contains("`minimum_cli_version`"), "{next_step}");
    let approval = json!({"limit_bytes": 1_610_612_736_u64, "note": note});
    // The decision lives in the task manifest, not in private state.
    let approved_manifest = std::fs::read_to_string(&manifest).unwrap();
    assert_eq!(
        approved_manifest,
        format!(
            "{}\n[cloud_usage_approval]\nlimit_bytes = 1610612736\nnote = \"{note}\"\n",
            manifest_before.replace("schema_version = 2", "schema_version = 3")
        )
    );
    assert!(cloud_usage_state(&fixture.shared).is_empty());
    let status = workspace(&task, ["task", "status"]);
    assert_no_reminder(&status);
    assert_eq!(
        json(&status)["cloud_usage"],
        json!({
            "threshold_bytes": REAL_THRESHOLD,
            "limit_bytes": 1_610_612_736_u64,
            "approval": approval,
            "pending": null,
        })
    );

    std::fs::write(task.join("notes.md"), "notes\n").unwrap();
    if cfg!(not(feature = "test-storage")) {
        // Only test builds can stand in for the releases below.
        return;
    }
    // A build older than the release that reads schema 3 never publishes
    // the approval: plan, the rehearsal, and publish refuse before anything
    // is placed, committed, or pushed.
    let older = [(CLI_VERSION_ENV, "0.3.0")];
    let release = [(CLI_VERSION_ENV, "0.4.0")];
    let branch = "refs/heads/codex/usage-approval";
    let local_branch = git(&fixture.shared, ["rev-parse", branch]).stdout;
    for args in [
        &["plan"][..],
        &["publish", "-m", "Publish notes", "--dry-run"][..],
        &["publish", "-m", "Publish notes"][..],
    ] {
        let output = workspace_env_unchecked(&task, args, &older);
        assert_refused(&output, &[]);
        assert_eq!(
            stderr(&output),
            "workspace-mgr: this build (workspace-mgr 0.3.0) cannot publish task manifest schema 3, which requires workspace-mgr 0.4.0 or newer; update workspace-mgr, or record the default limit to remove the approval. Tell the user both versions and ask before updating with `cargo install --locked workspace-mgr` or removing the approval.\n",
            "{args:?}"
        );
    }
    assert!(
        !git_unchecked(&fixture.remote, ["rev-parse", "-q", "--verify", branch])
            .status
            .success()
    );
    assert_eq!(
        git(&fixture.shared, ["rev-parse", branch]).stdout,
        local_branch
    );
    assert_eq!(
        std::fs::read_to_string(&manifest).unwrap(),
        approved_manifest
    );
    assert_eq!(
        std::fs::read_to_string(fixture.shared.join(CONFIG)).unwrap(),
        config_before
    );
    assert!(cloud_usage_state(&fixture.shared).is_empty());

    let plan = json(&workspace_env(&task, ["plan"], &release));
    assert_eq!(
        plan["repository_requirement"],
        json!({
            "path": CONFIG,
            "change": "raise",
            "minimum_cli_version": "0.4.0",
            "previous_minimum_cli_version": null,
            "task_manifest_schema": 3,
        })
    );
    assert_eq!(
        plan["changed_paths"],
        json!([
            CONFIG,
            manifest_path,
            format!("{task_id}/README.md"),
            format!("{task_id}/notes.md"),
        ])
    );
    let published = json(&workspace_env(
        &task,
        ["publish", "-m", "Publish notes"],
        &release,
    ));
    assert_eq!(published["status"], "pushed");
    assert_eq!(published["cloud_usage"]["approval"], approval);
    assert_eq!(published["cloud_usage"]["limit_bytes"], 1_610_612_736_u64);
    assert_eq!(published["cloud_usage"]["threshold_bytes"], REAL_THRESHOLD);
    assert_eq!(
        published["repository_requirement"],
        plan["repository_requirement"]
    );
    assert_eq!(published["tree_oid"], plan["tree_oid"]);
    let tip = published["remote_oid"].as_str().unwrap().to_owned();
    assert_eq!(
        commit_message(&fixture.remote, &tip),
        format!(
            "Publish notes\n\nWorkspace-Task: {task_id}\nWorkspace-Scope: {task_id}\nWorkspace-Requirement: minimum_cli_version=0.4.0 (task manifest schema 3)\n{TRAILER}: limit_bytes=1610612736; note={note}\n\n"
        )
    );
    // The published tree carries the manifest and the raised requirement;
    // the shared checkout's configuration is untouched.
    assert_eq!(
        show(&fixture.remote, &format!("{tip}:{manifest_path}")),
        approved_manifest
    );
    assert_eq!(
        show(&fixture.remote, &format!("{tip}:{CONFIG}")),
        format!("minimum_cli_version = \"0.4.0\"\n\n{config_before}")
    );
    assert_eq!(
        std::fs::read_to_string(fixture.shared.join(CONFIG)).unwrap(),
        config_before
    );
    assert!(
        git(&fixture.shared, ["status", "--porcelain", "--", CONFIG])
            .stdout
            .is_empty()
    );
    // The task branch now requires 0.4.0, so an older build refuses it.
    assert_refused(
        &workspace_env_unchecked(&task, ["plan"], &older),
        &[
            "requires workspace-mgr 0.4.0 or newer (`minimum_cli_version` in .workspace-mgr.toml on origin/codex/usage-approval); this is workspace-mgr 0.3.0.",
        ],
    );
    // Once published, later publications leave the requirement alone.
    std::fs::write(task.join("notes.md"), "more notes\n").unwrap();
    let again = json(&workspace_env(
        &task,
        ["publish", "-m", "Publish more notes"],
        &release,
    ));
    assert_eq!(again["status"], "pushed");
    assert!(again.get("repository_requirement").is_none());
    assert_eq!(
        again["changed_paths"],
        json!([format!("{task_id}/notes.md")])
    );
    let again_message = commit_message(&fixture.remote, again["remote_oid"].as_str().unwrap());
    assert!(!again_message.contains("Workspace-Requirement"));
    assert!(again_message.contains(&format!("{TRAILER}: limit_bytes=1610612736; note={note}")));

    let raise_to = |limit: &str, note: &str| {
        json(&workspace(
            &fixture.shared,
            [
                "task",
                "approve-cloud-usage",
                "--manifest",
                manifest.to_str().unwrap(),
                "--limit",
                limit,
                "--note",
                note,
            ],
        ))
    };
    let raised = raise_to("3GiB", "The user raised the limit to 3 GiB");
    assert_eq!(raised["status"], "recorded");
    assert_eq!(raised["previous_limit_bytes"], 1_610_612_736_u64);
    assert_eq!(raised["limit_bytes"], 3_221_225_472_u64);
    assert_eq!(raised["limit"], "3 GiB (3221225472 bytes)");
    // Recording the same decision again changes nothing and says so.
    let raised_manifest = std::fs::read_to_string(&manifest).unwrap();
    let same = raise_to("3GiB", "The user raised the limit to 3 GiB");
    assert_eq!(same["status"], "unchanged");
    assert_eq!(same["previous_limit_bytes"], 3_221_225_472_u64);
    assert_eq!(same["schema_version"], 3);
    assert_eq!(
        same["next_step"],
        "The task manifest already records this approval, so this command changed nothing. Run `workspace-mgr plan` to see whether earlier manifest changes are still unpublished."
    );
    assert_eq!(std::fs::read_to_string(&manifest).unwrap(), raised_manifest);
    // The first recording is still unpublished, which plan shows.
    assert_eq!(
        json(&workspace_env(&task, ["plan"], &release))["changed_paths"],
        json!([manifest_path])
    );
    let reset_to_default = || {
        json(&workspace(
            &task,
            [
                "task",
                "approve-cloud-usage",
                "--limit",
                "1073741824",
                "--note",
                "The user reset the limit to the default",
            ],
        ))
    };
    let reset = reset_to_default();
    assert_eq!(reset["status"], "recorded");
    assert_eq!(reset["previous_limit_bytes"], 3_221_225_472_u64);
    assert_eq!(reset["limit_bytes"], REAL_THRESHOLD);
    assert_eq!(reset["schema_version"], 2);
    assert_eq!(
        reset["next_step"],
        "Run `workspace-mgr plan` to re-measure the task, then publish. The task manifest no longer records an approval; the next publication carries that change and withdraws the task branch's `minimum_cli_version` raise, down to the base branch's declaration, when no task manifest in it still needs one."
    );
    assert_eq!(std::fs::read_to_string(&manifest).unwrap(), manifest_before);
    assert_eq!(
        json(&workspace(&task, ["task", "status"]))["cloud_usage"]["approval"],
        Value::Null
    );
    // Resetting again, with no approval left, changes nothing either.
    let again_reset = reset_to_default();
    assert_eq!(again_reset["status"], "unchanged");
    assert_eq!(again_reset["previous_limit_bytes"], REAL_THRESHOLD);
    assert_eq!(
        again_reset["next_step"],
        "The task manifest already records no approval, so the task keeps the default limit and this command changed nothing. Run `workspace-mgr plan` to see whether earlier manifest changes are still unpublished."
    );
    assert_eq!(std::fs::read_to_string(&manifest).unwrap(), manifest_before);
    // Publishing the reset returns the manifest to schema 2 without the
    // approval trailer and withdraws the branch's raise, so the branch no
    // longer changes the repository's configuration.
    let withdrawn = json!({
        "path": CONFIG,
        "change": "withdraw",
        "minimum_cli_version": null,
        "previous_minimum_cli_version": "0.4.0",
        "task_manifest_schema": null,
    });
    let unapproved = json(&workspace_env(
        &task,
        ["publish", "-m", "Reset the limit"],
        &release,
    ));
    assert_eq!(unapproved["status"], "pushed");
    assert_eq!(unapproved["changed_paths"], json!([CONFIG, manifest_path]));
    assert_eq!(unapproved["repository_requirement"], withdrawn);
    assert_eq!(unapproved["cloud_usage"]["approval"], Value::Null);
    let reset_tip = unapproved["remote_oid"].as_str().unwrap();
    assert_eq!(
        commit_message(&fixture.remote, reset_tip),
        format!(
            "Reset the limit\n\nWorkspace-Task: {task_id}\nWorkspace-Scope: {task_id}\nWorkspace-Requirement: minimum_cli_version removed (withdraws this branch's raise to 0.4.0; no task manifest in this publication needs it)\n\n"
        )
    );
    assert_eq!(
        show(&fixture.remote, &format!("{reset_tip}:{manifest_path}")),
        manifest_before
    );
    assert_eq!(
        show(&fixture.remote, &format!("{reset_tip}:{CONFIG}")),
        config_before
    );
    assert!(
        git(
            &fixture.remote,
            ["diff", "--name-only", "main", reset_tip, "--", CONFIG]
        )
        .stdout
        .is_empty()
    );
    // Older builds may continue the task again.
    assert_eq!(
        json(&workspace_env(&task, ["plan"], &older))["status"],
        "no_changes"
    );

    // The reminder is best-effort, but decisions never ignore corrupt state.
    let state_dirs = task_state_dirs(&fixture.shared);
    assert_eq!(state_dirs.len(), 1);
    let state = state_dirs[0].join("cloud-usage.json");
    std::fs::write(&state, "{").unwrap();
    let storage_status = workspace(&task, ["storage", "status"]);
    assert!(stderr(&storage_status).is_empty());
    for args in [&["task", "status"][..], &["plan"][..]] {
        assert_refused(
            &workspace_unchecked(&task, args),
            &["invalid private cloud-usage state"],
        );
    }
    std::fs::remove_file(&state).unwrap();
    assert_eq!(
        json(&workspace(&task, ["plan"]))["cloud_usage"]["limit_bytes"],
        REAL_THRESHOLD
    );
}

#[cfg(feature = "test-storage")]
#[test]
fn rename_preserves_the_cloud_usage_approval() {
    let fixture = managed_fixture(false);
    let release = [(CLI_VERSION_ENV, "0.4.0")];
    let (task_id, task) = create_task(&fixture, "usage-rename", "20260918-114000");
    let note = "The user approved 2 GiB for the renamed outputs";
    let approval = json!({"limit_bytes": 2_147_483_648_u64, "note": note});
    let table = format!(
        "additional_scopes = []\n\n[cloud_usage_approval]\nlimit_bytes = 2147483648\nnote = \"{note}\"\n"
    );
    let recorded = json(&workspace(
        &task,
        [
            "task",
            "approve-cloud-usage",
            "--limit",
            "2GiB",
            "--note",
            note,
        ],
    ));
    assert_eq!(recorded["schema_version"], 3);

    // Before publication the rename moves the task and keeps its approval.
    let renamed = json(&workspace(&task, ["task", "rename", "usage-draft"]));
    assert_eq!(renamed["status"], "renamed");
    let draft_id = "20260918-114000-usage-draft";
    let draft = fixture.shared.join(draft_id);
    assert!(!task.exists());
    let draft_manifest = std::fs::read_to_string(draft.join(MANIFEST)).unwrap();
    assert!(draft_manifest.starts_with("schema_version = 3\n"));
    assert!(draft_manifest.contains(&format!("id = \"{task_id}\"\n")));
    assert!(draft_manifest.contains("slug = \"usage-draft\"\n"));
    assert!(draft_manifest.ends_with(&table), "{draft_manifest}");
    assert_eq!(
        json(&workspace(&draft, ["task", "status"]))["cloud_usage"]["approval"],
        approval
    );

    let published = json(&workspace_env(
        &draft,
        ["publish", "-m", "Publish the draft"],
        &release,
    ));
    assert_eq!(published["status"], "pushed");
    assert_eq!(published["repository_requirement"]["change"], "raise");
    assert_eq!(
        published["repository_requirement"]["minimum_cli_version"],
        "0.4.0"
    );
    let tip = published["remote_oid"].as_str().unwrap().to_owned();
    assert_eq!(
        show(&fixture.remote, &format!("{tip}:{draft_id}/{MANIFEST}")),
        draft_manifest
    );

    // After publication the rename still keeps it, and the next publication
    // moves the manifest without raising the requirement again. The task
    // branch now requires a release that reads schema 3.
    workspace_env(&draft, ["task", "rename", "usage-final"], &release);
    let final_id = "20260918-114000-usage-final";
    let renamed_task = fixture.shared.join(final_id);
    let final_manifest = std::fs::read_to_string(renamed_task.join(MANIFEST)).unwrap();
    assert_eq!(
        final_manifest,
        draft_manifest.replace("usage-draft", "usage-final")
    );
    let plan = json(&workspace_env(&renamed_task, ["plan"], &release));
    assert!(plan.get("repository_requirement").is_none());
    assert_eq!(plan["cloud_usage"]["approval"], approval);
    assert_eq!(
        plan["changed_paths"],
        json!([
            format!("{draft_id}/{MANIFEST}"),
            format!("{draft_id}/README.md"),
            format!("{final_id}/{MANIFEST}"),
            format!("{final_id}/README.md"),
        ])
    );
    let moved = json(&workspace_env(
        &renamed_task,
        ["publish", "-m", "Rename the task"],
        &release,
    ));
    assert_eq!(moved["status"], "pushed");
    let moved_tip = moved["remote_oid"].as_str().unwrap();
    assert_eq!(
        show(
            &fixture.remote,
            &format!("{moved_tip}:{final_id}/{MANIFEST}")
        ),
        final_manifest
    );
    assert!(
        !git_unchecked(
            &fixture.remote,
            ["cat-file", "-e", &format!("{moved_tip}:{draft_id}")]
        )
        .status
        .success()
    );
    assert_eq!(
        show(&fixture.remote, &format!("{moved_tip}:{CONFIG}")),
        show(&fixture.remote, &format!("{tip}:{CONFIG}"))
    );
    let message = commit_message(&fixture.remote, moved_tip);
    assert!(!message.contains("Workspace-Requirement"));
    assert_eq!(
        last_line(&message),
        Some(format!("{TRAILER}: limit_bytes=2147483648; note={note}").as_str())
    );

    // Infrastructure tasks keep the approval in their private manifest.
    let created = json(&workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "usage-assets",
            "--kind",
            "infrastructure",
            "--title",
            "Usage assets",
            "--purpose",
            "Keep repository-wide assets.",
            "--scope",
            "assets",
            "--scope-note",
            "The user requested shared assets.",
        ],
    ));
    let worktree = PathBuf::from(created["path"].as_str().unwrap());
    let private = PathBuf::from(created["manifest"].as_str().unwrap());
    let recorded = json(&workspace(
        &worktree,
        [
            "task",
            "approve-cloud-usage",
            "--limit",
            "2GiB",
            "--note",
            note,
        ],
    ));
    assert_eq!(recorded["schema_version"], 3);
    workspace(&worktree, ["task", "rename", "usage-archive"]);
    let raw = std::fs::read_to_string(&private).unwrap();
    assert!(raw.starts_with("schema_version = 3\n"), "{raw}");
    assert!(raw.contains("slug = \"usage-archive\"\n"));
    assert!(
        raw.ends_with(&format!(
            "\n\n[cloud_usage_approval]\nlimit_bytes = 2147483648\nnote = \"{note}\"\n"
        )),
        "{raw}"
    );
    assert_eq!(
        json(&workspace(&worktree, ["task", "status"]))["cloud_usage"]["approval"],
        approval
    );
}

#[test]
fn approvals_are_recorded_only_in_the_checkout_that_publishes_the_manifest() {
    let fixture = managed_fixture(false);
    let (task_id, task) = create_task(&fixture, "usage-owner", "20260918-115500");
    workspace(&task, ["publish", "-m", "Publish the scaffold"]);
    // The user merges the deliverable and the shared checkout follows.
    git(&fixture.seed, ["fetch", "-q", "origin"]);
    git(
        &fixture.seed,
        ["merge", "--no-edit", "-q", "origin/codex/usage-owner"],
    );
    git(&fixture.seed, ["push", "-q", "origin", "main"]);
    workspace(&fixture.shared, ["refresh"]);
    let created = json(&workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "usage-tools",
            "--kind",
            "infrastructure",
            "--title",
            "Usage tools",
            "--purpose",
            "Maintain repository tools.",
            "--scope",
            "tools",
            "--scope-note",
            "The user requested the tools update.",
        ],
    ));
    let worktree = PathBuf::from(created["path"].as_str().unwrap());
    // The infrastructure worktree holds its own copy of the merged
    // deliverable, which no publication of the deliverable carries.
    let copy = worktree.join(&task_id);
    let copy_manifest = copy.join(MANIFEST);
    let copy_before = std::fs::read_to_string(&copy_manifest).unwrap();
    let shared_before = std::fs::read_to_string(task.join(MANIFEST)).unwrap();
    let approve = |cwd: &Path, dry_run: bool| {
        let mut args = vec![
            "task",
            "approve-cloud-usage",
            "--limit",
            "2GiB",
            "--note",
            "The user approved 2 GiB",
        ];
        if dry_run {
            args.push("--dry-run");
        }
        workspace_unchecked(cwd, args)
    };
    for dry_run in [true, false] {
        let refused = approve(&copy, dry_run);
        assert_refused(&refused, &[]);
        assert_eq!(
            stderr(&refused),
            "workspace-mgr: deliverable cloud-usage approval must run from the shared checkout on \"main\"; current branch is \"codex/infra-usage-tools\"; use an explicitly authorized alternate workflow or --allow-non-shared-head with --scope-note\n"
        );
    }
    assert_eq!(
        std::fs::read_to_string(&copy_manifest).unwrap(),
        copy_before
    );
    assert!(
        git(
            &worktree,
            ["status", "--porcelain", "--untracked-files=all"]
        )
        .stdout
        .is_empty()
    );
    // The shared checkout must be on its shared branch, too.
    git(&fixture.shared, ["switch", "-q", "-c", "elsewhere"]);
    let refused = approve(&task, true);
    assert_refused(&refused, &[]);
    assert_eq!(
        stderr(&refused),
        "workspace-mgr: deliverable cloud-usage approval must run from the shared checkout on \"main\"; current branch is \"elsewhere\"; use an explicitly authorized alternate workflow or --allow-non-shared-head with --scope-note\n"
    );
    git(&fixture.shared, ["switch", "-q", "main"]);
    assert_eq!(
        std::fs::read_to_string(task.join(MANIFEST)).unwrap(),
        shared_before
    );
    // From the checkout that publishes it, the approval is recorded.
    let recorded = json(&approve(&task, false));
    assert_eq!(recorded["status"], "recorded");
    assert_eq!(recorded["schema_version"], 3);
    assert_eq!(
        std::fs::read_to_string(&copy_manifest).unwrap(),
        copy_before
    );
}

/// The user may authorize a deliverable's publication from another checkout
/// head; the approval then follows the same rules as `plan` and `publish`.
#[test]
fn approvals_follow_publication_in_an_authorized_alternate_workflow() {
    let fixture = managed_fixture(false);
    let (task_id, task) = create_task(&fixture, "alternate", "20260918-115800");
    let manifest = task.join(MANIFEST);
    let manifest_before = std::fs::read_to_string(&manifest).unwrap();
    git(&fixture.shared, ["switch", "-q", "-c", "alt"]);
    let release = [(CLI_VERSION_ENV, "0.4.0")];
    let scope_note = "The user authorized working on alt";
    let approve = |extra: &[&str]| {
        let mut args = vec![
            "task",
            "approve-cloud-usage",
            "--limit",
            "2GiB",
            "--note",
            "The user approved 2 GiB",
        ];
        args.extend(extra);
        workspace_env_unchecked(&task, args, &release)
    };

    // Without the override, the checkout head is refused like publication.
    let refused = approve(&[]);
    assert_refused(&refused, &[]);
    assert_eq!(
        stderr(&refused),
        "workspace-mgr: deliverable cloud-usage approval must run from the shared checkout on \"main\"; current branch is \"alt\"; use an explicitly authorized alternate workflow or --allow-non-shared-head with --scope-note\n"
    );
    let refused = approve(&["--allow-non-shared-head"]);
    assert_refused(&refused, &[]);
    assert_eq!(
        stderr(&refused),
        "workspace-mgr: --allow-non-shared-head requires --scope-note\n"
    );
    let refused = approve(&["--allow-non-shared-head", "--scope-note", "first\nsecond"]);
    assert_refused(&refused, &["scope note must be a single line"]);
    assert_eq!(std::fs::read_to_string(&manifest).unwrap(), manifest_before);

    // With the same override that plan and publish accept, the approval is
    // rehearsed and recorded.
    let override_args = ["--allow-non-shared-head", "--scope-note", scope_note];
    let mut rehearsal_args = override_args.to_vec();
    rehearsal_args.push("--dry-run");
    let rehearsal = json(&approve(&rehearsal_args));
    assert_eq!(rehearsal["status"], "dry_run");
    assert_eq!(std::fs::read_to_string(&manifest).unwrap(), manifest_before);
    let recorded = json(&approve(&override_args));
    assert_eq!(recorded["status"], "recorded");
    assert_eq!(recorded["schema_version"], 3);
    assert!(
        std::fs::read_to_string(&manifest)
            .unwrap()
            .contains("[cloud_usage_approval]\nlimit_bytes = 2147483648\n")
    );
    let mut plan_args = vec!["plan"];
    plan_args.extend(override_args);
    let plan = json(&workspace_env(&task, plan_args, &release));
    assert_eq!(plan["head"], "alt");
    assert_eq!(plan["cloud_usage"]["limit_bytes"], 2_147_483_648_u64);
    assert_eq!(
        plan["changed_paths"],
        json!([
            CONFIG,
            format!("{task_id}/{MANIFEST}"),
            format!("{task_id}/README.md"),
        ])
    );
    assert_eq!(plan["repository_requirement"]["change"], "raise");

    // The target branch itself is never the checkout head.
    git(&fixture.shared, ["switch", "-q", "codex/alternate"]);
    let refused = approve(&override_args);
    assert_refused(&refused, &[]);
    assert_eq!(
        stderr(&refused),
        "workspace-mgr: target branch may not be the checkout's current branch\n"
    );
}

#[test]
fn only_schema_3_manifests_may_carry_the_approval_table() {
    let fixture = managed_fixture(false);
    let (_, task) = create_task(&fixture, "usage-schema", "20260918-115000");
    let manifest = task.join(MANIFEST);
    let note = "The user approved 2 GiB for the schema check";
    workspace(
        &task,
        [
            "task",
            "approve-cloud-usage",
            "--limit",
            "2GiB",
            "--note",
            note,
        ],
    );
    let approved = std::fs::read_to_string(&manifest).unwrap();
    assert!(approved.starts_with("schema_version = 3\n"));
    for (content, error) in [
        (
            approved.replace("schema_version = 3", "schema_version = 2"),
            "task schema 2 must not declare the schema 3 cloud_usage_approval table",
        ),
        (
            approved
                .replace("schema_version = 3", "schema_version = 1")
                .replace("slug = \"usage-schema\"\n", ""),
            "task schema 1 must not declare the schema 3 cloud_usage_approval table",
        ),
        (
            approved.replace("schema_version = 3", "schema_version = 4"),
            "unsupported task schema 4, expected 1, 2, or 3",
        ),
        (
            format!("{approved}recorded_at = \"2026-09-18T12:00:00Z\"\n"),
            "unknown field `recorded_at`",
        ),
        (
            approved.replace("limit_bytes = 2147483648\n", ""),
            "missing field `limit_bytes`",
        ),
        (
            approved.replace("limit_bytes = 2147483648", "limit_bytes = -1"),
            "invalid value: integer `-1`, expected u64",
        ),
        (
            approved.replace(note, "  "),
            "cloud-usage approval note must not be empty",
        ),
    ] {
        std::fs::write(&manifest, &content).unwrap();
        for args in [
            &["task", "status"][..],
            &["plan"][..],
            &["publish", "-m", "Publish the manifest"][..],
            &[
                "task",
                "approve-cloud-usage",
                "--limit",
                "3GiB",
                "--note",
                "The user approved 3 GiB",
            ][..],
            &["task", "rename", "usage-other", "--dry-run"][..],
        ] {
            assert_refused(&workspace_unchecked(&task, args), &[error]);
        }
        assert_eq!(std::fs::read_to_string(&manifest).unwrap(), content);
    }
    assert!(
        !git_unchecked(
            &fixture.remote,
            [
                "rev-parse",
                "-q",
                "--verify",
                "refs/heads/codex/usage-schema"
            ]
        )
        .status
        .success()
    );

    // A schema 3 manifest without the table is readable, and the next
    // recorded decision rewrites it at the lowest schema that fits.
    let reset = approved.replace(
        &format!("\n[cloud_usage_approval]\nlimit_bytes = 2147483648\nnote = \"{note}\"\n"),
        "",
    );
    std::fs::write(&manifest, &reset).unwrap();
    assert_eq!(
        json(&workspace(&task, ["task", "status"]))["cloud_usage"]["approval"],
        Value::Null
    );
    let rewritten = json(&workspace(
        &task,
        [
            "task",
            "approve-cloud-usage",
            "--limit",
            "1GiB",
            "--note",
            "The user kept the default limit",
        ],
    ));
    assert_eq!(rewritten["schema_version"], 2);
    assert_eq!(
        std::fs::read_to_string(&manifest).unwrap(),
        reset.replace("schema_version = 3", "schema_version = 2")
    );
}

// Only test-storage builds can lower the threshold or use filesystem storage.
#[cfg(feature = "test-storage")]
mod test_storage {
    use super::*;

    // Publishing an approval needs a release that reads task manifest
    // schema 3, so these tests stand in for one explicitly.
    pub const LIMIT_10MB: [(&str, &str); 2] = [
        (CLOUD_USAGE_THRESHOLD_ENV, "10000000"),
        (CLI_VERSION_ENV, "0.4.0"),
    ];
    pub const LIMIT_300KB: [(&str, &str); 2] = [
        (CLOUD_USAGE_THRESHOLD_ENV, "300000"),
        (CLI_VERSION_ENV, "0.4.0"),
    ];
    pub const LARGE: usize = 10_485_761;

    pub fn rev(repo: &Path, reference: &str) -> Option<String> {
        let output = git_unchecked(repo, ["rev-parse", "-q", "--verify", reference]);
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    pub fn tree_contains(repo: &Path, oid: &str, path: &str) -> bool {
        git_unchecked(repo, ["cat-file", "-e", &format!("{oid}:{path}")])
            .status
            .success()
    }

    pub fn dvc_available() -> bool {
        if which::which("dvc").is_err() {
            eprintln!("skipping: dvc is unavailable");
            return false;
        }
        true
    }

    pub fn remote_snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        if !root.exists() {
            return Vec::new();
        }
        let mut files = walkdir::WalkDir::new(root)
            .into_iter()
            .map(Result::unwrap)
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                (
                    entry.path().strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(entry.path()).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        files.sort();
        files
    }

    pub fn cached_files(repo: &Path) -> usize {
        let cache = repo.join(".dvc/cache");
        if !cache.exists() {
            return 0;
        }
        walkdir::WalkDir::new(cache)
            .into_iter()
            .map(Result::unwrap)
            .filter(|entry| entry.file_type().is_file())
            .count()
    }

    /// Deterministic bytes that Git cannot compress.
    pub fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 32) as u8
            })
            .collect()
    }

    /// Comment-only ignore rules of about `len` bytes that Git cannot pack
    /// much smaller than half their size.
    pub fn ignore_rules(len: usize, seed: u64) -> Vec<u8> {
        let mut rules = Vec::with_capacity(len + len / 16);
        for chunk in noise(len / 2, seed).chunks(32) {
            rules.extend_from_slice(b"# ");
            for byte in chunk {
                rules.extend_from_slice(format!("{byte:02x}").as_bytes());
            }
            rules.push(b'\n');
        }
        rules
    }

    /// A 400 KB incompressible Git file, reported by its compressed size
    /// because the totals are packed.
    pub fn assert_git_contributor(contributor: &Value, path: &str, state: &str) {
        assert_eq!(contributor["path"], path, "{contributor}");
        assert_eq!(contributor["store"], "git");
        assert_eq!(contributor["versions"], 1);
        assert_eq!(contributor["state"], state);
        let bytes = contributor["bytes"].as_u64().unwrap();
        assert!((399_000..402_000).contains(&bytes), "{contributor}");
    }

    pub fn reminder(task_id: &str, projected: &str, limit: &str) -> String {
        format!(
            "workspace-mgr: task {task_id} {REMINDER}, as last measured by `workspace-mgr plan` or `workspace-mgr publish`: projected {projected} exceeds limit {limit}. Unless the user already answered, stop task work and ask the user; after carrying out the user's answer, run `workspace-mgr plan` to re-measure.\n"
        )
    }

    pub fn approve(task: &Path, env: &[(&str, &str)], limit: &str, note: &str) -> Value {
        json(&workspace_env(
            task,
            [
                "task",
                "approve-cloud-usage",
                "--limit",
                limit,
                "--note",
                note,
            ],
            env,
        ))
    }

    /// The report of a publication that withdraws its branch's raise to
    /// 0.4.0 from a base branch that declares nothing.
    pub fn withdrawn_requirement() -> Value {
        json!({
            "path": CONFIG,
            "change": "withdraw",
            "minimum_cli_version": null,
            "previous_minimum_cli_version": "0.4.0",
            "task_manifest_schema": null,
        })
    }

    pub fn trailer(approval: &Value) -> String {
        format!(
            "{TRAILER}: limit_bytes={}; note={}",
            approval["limit_bytes"],
            approval["note"].as_str().unwrap()
        )
    }

    /// Continues a published task in another clone, as a second chat would.
    pub fn checkout_task(clone: &Path, branch: &str, task_id: &str) -> PathBuf {
        git(clone, ["fetch", "-q", "origin", branch]);
        git(
            clone,
            [
                "restore",
                "--source=FETCH_HEAD",
                "--worktree",
                "--",
                task_id,
            ],
        );
        clone.join(task_id)
    }
}

#[cfg(feature = "test-storage")]
use test_storage::*;

#[cfg(feature = "test-storage")]
#[test]
fn growth_past_the_limit_is_refused_before_tracking_until_the_user_approves() {
    if !dvc_available() {
        return;
    }
    let fixture = managed_fixture(true);
    let (task_id, task) = create_task(&fixture, "usage-gate", "20260918-120000");
    document_task(&task);
    let branch = "refs/heads/codex/usage-gate";
    let env = &LIMIT_10MB[..];
    workspace_env(&task, ["publish", "-m", "Publish scaffold"], env);
    let tip = rev(&fixture.remote, branch).unwrap();
    let local_branch = rev(&fixture.shared, branch);
    let large = format!("{task_id}/large.bin");
    std::fs::write(task.join("large.bin"), vec![7_u8; LARGE]).unwrap();
    let storage_remote = fixture.root.join("storage-remote");
    let storage_before = remote_snapshot(&storage_remote);

    let output = workspace_env(&task, ["plan"], env);
    assert_no_reminder(&output);
    let plan = json(&output);
    let usage = &plan["cloud_usage"];
    assert_eq!(plan["status"], "dry_run");
    assert_eq!(usage["status"], "approval_required");
    assert_eq!(usage["publish_allowed"], false);
    assert_eq!(usage["cleanup_only"], false);
    assert_eq!(usage["git_history_exceeds_limit"], false);
    assert_eq!(usage["threshold_bytes"], 10_000_000);
    assert_eq!(usage["limit_bytes"], 10_000_000);
    assert_eq!(usage["approval"], Value::Null);
    assert_eq!(usage["git_measure"], "packed");
    assert_eq!(usage["headroom_bytes"], 0);
    assert_eq!(usage["suggested_limit_bytes"], 536_870_912);
    assert_eq!(usage["published"]["s3_bytes"], 0);
    assert_eq!(usage["projected"]["s3_bytes"], LARGE);
    let base = plan["remote_base_oid"].as_str().unwrap();
    let base_tree = format!("{base}^{{tree}}");
    assert_eq!(
        usage["published"]["git_uncompressed_bytes"],
        object_bytes(&fixture.shared, &[&tip, "--not", base, &base_tree])
    );
    let projected_git = usage["projected"]["git_bytes"].as_u64().unwrap();
    let projected_total = usage["projected"]["total_bytes"].as_u64().unwrap();
    assert_eq!(projected_total, projected_git + LARGE as u64);
    assert!(projected_total < 10_490_000, "{projected_total}");
    assert_eq!(
        usage["contributors"][0],
        json!({"path": large, "store": "s3", "bytes": LARGE, "versions": 1, "state": "pending"})
    );
    assert!(
        usage["message"]
            .as_str()
            .unwrap()
            .contains("exceeds the limit 9.53 MiB (10000000 bytes)")
    );
    assert!(!task.join("large.bin.dvc").exists());

    let status = workspace_env(&task, ["task", "status"], env);
    assert!(stderr(&status).is_empty());
    let status = json(&status);
    let pending = &status["cloud_usage"]["pending"];
    assert_eq!(status["cloud_usage"]["approval"], Value::Null);
    assert_eq!(status["cloud_usage"]["limit_bytes"], 10_000_000);
    assert_eq!(pending["limit_bytes"], 10_000_000);
    assert_eq!(pending["remote_base_oid"], base);
    assert_eq!(pending["remote_target_oid"], tip.as_str());
    assert_eq!(pending["projected_tree_oid"], plan["tree_oid"]);
    assert_eq!(pending["published"], usage["published"]);
    assert_eq!(pending["projected"], usage["projected"]);

    let refusal = [
        format!(
            "workspace-mgr: cloud usage for task {task_id} needs the user's approval: published Git "
        ),
        "S3 0 bytes, total ".to_owned(),
        "; projected Git ".to_owned(),
        "S3 10 MiB (10485761 bytes)".to_owned(),
        format!("total 10 MiB ({projected_total} bytes); limit 9.53 MiB (10000000 bytes)."),
        "The task is waiting for the user's decision.".to_owned(),
        "only after the user gives it in this chat".to_owned(),
        "Run `workspace-mgr plan` for the largest contributors.".to_owned(),
    ];
    let refusal = refusal.iter().map(String::as_str).collect::<Vec<_>>();
    for args in [
        &["publish", "-m", "Publish large data"][..],
        &["publish", "-m", "Publish large data", "--dry-run"][..],
    ] {
        let refused = workspace_env_unchecked(&task, args, env);
        assert_refused(&refused, &refusal);
        assert!(!stderr(&refused).contains("approve-cloud-usage"));
        assert!(!task.join("large.bin.dvc").exists());
        assert!(!task.join(".gitignore").exists());
        assert_eq!(cached_files(&fixture.shared), 0);
        assert_eq!(rev(&fixture.remote, branch).as_deref(), Some(tip.as_str()));
        assert_eq!(rev(&fixture.shared, branch), local_branch);
        assert_eq!(remote_snapshot(&storage_remote), storage_before);
    }

    // Every refused publication records the decision again.
    let status = json(&workspace_env(&task, ["task", "status"], env));
    let refreshed = &status["cloud_usage"]["pending"];
    assert_ne!(refreshed["measured_at"], Value::Null);
    for field in [
        "limit_bytes",
        "remote_base_oid",
        "remote_target_oid",
        "projected_tree_oid",
        "published",
        "projected",
    ] {
        assert_eq!(refreshed[field], pending[field], "{field}");
    }
    let pending = refreshed;

    let waiting = reminder(
        &task_id,
        &format!("10 MiB ({projected_total} bytes)"),
        "9.53 MiB (10000000 bytes)",
    );
    let manifest = task.join(".workspace-mgr-task.toml");
    for (cwd, args) in [
        (&task, vec!["storage", "status", large.as_str()]),
        (&task, vec!["untrack", large.as_str(), "--dry-run"]),
        (&task, vec!["task", "rename", "renamed-gate", "--dry-run"]),
        (
            &fixture.shared,
            vec![
                "task",
                "discard",
                "--manifest",
                manifest.to_str().unwrap(),
                "--dry-run",
            ],
        ),
    ] {
        let output = workspace_env(cwd, &args, env);
        assert_eq!(stderr(&output), waiting, "{args:?}");
    }

    assert_refused(
        &workspace_env_unchecked(
            &task,
            [
                "task",
                "approve-cloud-usage",
                "--limit",
                "9MB",
                "--note",
                "Approved",
            ],
            env,
        ),
        &[
            "approved cloud-usage limit 8.58 MiB (9000000 bytes) is below the threshold 9.53 MiB (10000000 bytes)",
        ],
    );
    let dry = json(&workspace_env(
        &task,
        [
            "task",
            "approve-cloud-usage",
            "--limit",
            "10.4MB",
            "--note",
            "The user approved 10.4 MB",
            "--dry-run",
        ],
        env,
    ));
    assert_eq!(dry["status"], "dry_run");
    assert_eq!(dry["previous_limit_bytes"], 10_000_000);
    assert_eq!(dry["limit_bytes"], 10_400_000);
    assert_eq!(dry["blocked"], true);
    assert_eq!(dry["pending"], *pending);
    let next_step = dry["next_step"].as_str().unwrap();
    assert!(
        next_step.starts_with("Nothing was recorded."),
        "{next_step}"
    );
    assert!(
        next_step.contains("still exceeds this limit"),
        "{next_step}"
    );
    assert!(!next_step.contains("then publish"), "{next_step}");
    assert_eq!(
        json(&workspace_env(&task, ["task", "status"], env))["cloud_usage"]["approval"],
        Value::Null
    );

    let note = "The user approved 20 MB for the large checkpoint in this chat";
    let recorded = approve(&task, env, "20MB", note);
    assert_eq!(recorded["status"], "recorded");
    assert_eq!(recorded["previous_limit_bytes"], 10_000_000);
    assert_eq!(recorded["limit"], "19.07 MiB (20000000 bytes)");
    assert_eq!(recorded["blocked"], false);
    let approval = json!({
        "limit_bytes": 20_000_000,
        "note": note,
    });
    let covered = workspace_env(&task, ["storage", "status", large.as_str()], env);
    assert!(stderr(&covered).is_empty(), "{}", stderr(&covered));
    let status = json(&workspace_env(&task, ["task", "status"], env));
    assert_eq!(status["cloud_usage"]["approval"], approval);
    assert_eq!(status["cloud_usage"]["limit_bytes"], 20_000_000);
    assert_eq!(status["cloud_usage"]["pending"], *pending);

    let plan = json(&workspace_env(&task, ["plan"], env));
    assert_eq!(plan["cloud_usage"]["status"], "within_limit");
    assert_eq!(plan["cloud_usage"]["publish_allowed"], true);
    assert_eq!(plan["cloud_usage"]["limit_bytes"], 20_000_000);
    assert_eq!(plan["cloud_usage"]["approval"], approval);
    assert_eq!(plan["cloud_usage"]["projected"]["s3_bytes"], LARGE);
    assert_eq!(
        json(&workspace_env(&task, ["task", "status"], env))["cloud_usage"]["pending"],
        Value::Null
    );

    let published = json(&workspace_env(
        &task,
        ["publish", "-m", "Publish large data"],
        env,
    ));
    assert_eq!(published["status"], "pushed");
    assert_eq!(published["cloud_usage"]["status"], "within_limit");
    assert!(task.join("large.bin.dvc").is_file());
    assert_ne!(remote_snapshot(&storage_remote), storage_before);
    let tip = published["remote_oid"].as_str().unwrap().to_owned();
    assert!(tree_contains(
        &fixture.remote,
        &tip,
        &format!("{large}.dvc")
    ));
    assert_eq!(
        last_line(&commit_message(&fixture.remote, &tip)),
        Some(trailer(&approval).as_str())
    );
    let settled = json(&workspace_env(&task, ["plan"], env));
    assert_eq!(settled["status"], "no_changes");
    assert_eq!(settled["cloud_usage"]["status"], "within_limit");
    assert_eq!(settled["cloud_usage"]["published"]["s3_bytes"], LARGE);
    assert_eq!(settled["cloud_usage"]["projected"]["s3_bytes"], LARGE);

    // Growth past the approved limit waits for the user again.
    let more = format!("{task_id}/more.bin");
    std::fs::write(task.join("more.bin"), vec![8_u8; LARGE]).unwrap();
    let storage_published = remote_snapshot(&storage_remote);
    let plan = json(&workspace_env(&task, ["plan"], env));
    let usage = &plan["cloud_usage"];
    assert_eq!(usage["status"], "approval_required");
    assert_eq!(usage["publish_allowed"], false);
    assert_eq!(usage["limit_bytes"], 20_000_000);
    assert_eq!(usage["approval"], approval);
    assert_eq!(usage["published"]["s3_bytes"], LARGE);
    assert_eq!(usage["projected"]["s3_bytes"], 2 * LARGE);
    assert_eq!(usage["suggested_limit_bytes"], 536_870_912);
    let contributors = usage["contributors"].as_array().unwrap();
    assert!(contributors.contains(
        &json!({"path": more, "store": "s3", "bytes": LARGE, "versions": 1, "state": "pending"})
    ));
    assert!(contributors.contains(
        &json!({"path": large, "store": "s3", "bytes": LARGE, "versions": 1, "state": "published"})
    ));
    assert_refused(
        &workspace_env_unchecked(&task, ["publish", "-m", "Publish more data"], env),
        &[
            "S3 10 MiB (10485761 bytes), total",
            "S3 20 MiB (20971522 bytes)",
            "limit 19.07 MiB (20000000 bytes).",
        ],
    );
    assert!(!task.join("more.bin.dvc").exists());
    assert_eq!(rev(&fixture.remote, branch).as_deref(), Some(tip.as_str()));
    assert_eq!(remote_snapshot(&storage_remote), storage_published);
}

/// Placement is previewed before usage is measured, so a boundary the storage
/// engine cannot address is refused first and no decision is recorded for it.
#[cfg(all(unix, feature = "test-storage"))]
#[test]
fn an_unaddressable_boundary_is_refused_before_the_cloud_usage_gate() {
    if !dvc_available() {
        return;
    }
    let fixture = managed_fixture(true);
    let (task_id, task) = create_task(&fixture, "gate-order", "20260918-130000");
    document_task(&task);
    let branch = "refs/heads/codex/gate-order";
    let env = &LIMIT_10MB[..];
    workspace_env(&task, ["publish", "-m", "Publish scaffold"], env);
    let tip = rev(&fixture.remote, branch).unwrap();
    let storage_remote = fixture.root.join("storage-remote");
    let storage_before = remote_snapshot(&storage_remote);

    // This file is both over the task's limit and at a path the storage engine
    // reads as a directory.
    let unaddressable = format!("{task_id}/top\\level.bin");
    std::fs::write(task.join("top\\level.bin"), noise(LARGE, 7)).unwrap();

    for args in [
        &["plan"][..],
        &["publish", "-m", "Publish an unaddressable boundary"][..],
        &[
            "publish",
            "-m",
            "Publish an unaddressable boundary",
            "--dry-run",
        ][..],
    ] {
        let refused = workspace_env_unchecked(&task, args, env);
        assert_refused(
            &refused,
            &[&format!(
                "automatic S3 placement path {unaddressable:?} contains a backslash"
            )],
        );
        let message = stderr(&refused);
        assert!(!message.contains("needs the user's approval"), "{message}");
        assert!(!message.contains(REMINDER), "{message}");
        assert!(cloud_usage_state(&fixture.shared).is_empty(), "{args:?}");
        assert!(!task.join("top\\level.bin.dvc").exists());
        assert_eq!(cached_files(&fixture.shared), 0);
        assert_eq!(rev(&fixture.remote, branch).as_deref(), Some(tip.as_str()));
        assert_eq!(remote_snapshot(&storage_remote), storage_before);
    }

    // Once the recovery rename makes the boundary addressable, the same
    // content leaves only the usage decision.
    let renamed = format!("{task_id}/top-level.bin");
    workspace_env(&task, ["move", &unaddressable, &renamed], env);
    let usage = json(&workspace_env(&task, ["plan"], env))["cloud_usage"].clone();
    assert_eq!(usage["status"], "approval_required");
    assert_eq!(usage["projected"]["s3_bytes"], LARGE);
    assert_eq!(
        usage["contributors"][0],
        json!({"path": renamed, "store": "s3", "bytes": LARGE, "versions": 1, "state": "pending"})
    );
    let refused = workspace_env_unchecked(
        &task,
        ["publish", "-m", "Publish the renamed boundary"],
        env,
    );
    assert_refused(
        &refused,
        &[&format!(
            "cloud usage for task {task_id} needs the user's approval"
        )],
    );
    assert!(!stderr(&refused).contains("backslash"));
    assert!(!task.join("top-level.bin.dvc").exists());
    assert_eq!(rev(&fixture.remote, branch).as_deref(), Some(tip.as_str()));
    assert_eq!(remote_snapshot(&storage_remote), storage_before);

    // A backslash name that automatic policy keeps in Git is charged under
    // exactly the path Git holds.
    let in_git = format!("{task_id}/keep\\me.bin");
    std::fs::write(task.join("keep\\me.bin"), noise(400_000, 11)).unwrap();
    let usage = json(&workspace_env(&task, ["plan"], env))["cloud_usage"].clone();
    assert_eq!(usage["git_measure"], "packed");
    let contributor = usage["contributors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|contributor| contributor["path"] == in_git.as_str())
        .unwrap_or_else(|| panic!("{usage}"));
    assert_git_contributor(contributor, &in_git, "pending");
}

#[cfg(feature = "test-storage")]
#[test]
fn declined_growth_is_cleaned_up_and_cleanup_only_publications_stay_allowed() {
    if !dvc_available() {
        return;
    }
    let fixture = managed_fixture(true);
    let (task_id, task) = create_task(&fixture, "usage-cleanup", "20260918-130000");
    document_task(&task);
    let branch = "refs/heads/codex/usage-cleanup";
    let env = &LIMIT_10MB[..];
    let large = format!("{task_id}/large.bin");
    let more = format!("{task_id}/more.bin");
    let storage_remote = fixture.root.join("storage-remote");

    // A proactive approval before the content exists is allowed.
    let proactive = approve(
        &task,
        env,
        "20MB",
        "The user approved 20 MB before the checkpoint was produced",
    );
    assert_eq!(proactive["pending"], Value::Null);
    assert_eq!(proactive["blocked"], false);
    std::fs::write(task.join("large.bin"), vec![7_u8; LARGE]).unwrap();
    let published = json(&workspace_env(
        &task,
        ["publish", "-m", "Publish large data"],
        env,
    ));
    assert_eq!(published["status"], "pushed");
    let storage_published = remote_snapshot(&storage_remote);

    // The user declines further growth and keeps the new file local only.
    std::fs::write(task.join("more.bin"), vec![8_u8; LARGE]).unwrap();
    assert_refused(
        &workspace_env_unchecked(&task, ["publish", "-m", "Publish more data"], env),
        &[
            "S3 20 MiB (20971522 bytes)",
            "limit 19.07 MiB (20000000 bytes).",
        ],
    );
    let untracked = workspace_env(&task, ["untrack", more.as_str()], env);
    assert!(stderr(&untracked).contains(REMINDER));
    let plan = json(&workspace_env(&task, ["plan"], env));
    let usage = &plan["cloud_usage"];
    assert_eq!(usage["status"], "within_limit");
    assert_eq!(usage["publish_allowed"], true);
    assert_eq!(usage["cleanup_only"], true);
    assert_eq!(usage["published"]["s3_bytes"], LARGE);
    assert_eq!(usage["projected"]["s3_bytes"], LARGE);
    assert_eq!(
        json(&workspace_env(&task, ["task", "status"], env))["cloud_usage"]["pending"],
        Value::Null
    );
    let kept = json(&workspace_env(
        &task,
        ["publish", "-m", "Keep more data local"],
        env,
    ));
    assert_eq!(kept["status"], "pushed");
    let tip = kept["remote_oid"].as_str().unwrap();
    assert!(!tree_contains(&fixture.remote, tip, &more));
    assert!(!tree_contains(&fixture.remote, tip, &format!("{more}.dvc")));
    assert!(tree_contains(
        &fixture.remote,
        tip,
        &format!("{more}.workspace-mgr-storage.toml")
    ));
    assert_eq!(remote_snapshot(&storage_remote), storage_published);
    assert_eq!(
        std::fs::read(task.join("more.bin")).unwrap(),
        vec![8_u8; LARGE]
    );
    let quiet = workspace_env(&task, ["storage", "status", large.as_str()], env);
    assert!(stderr(&quiet).is_empty(), "{}", stderr(&quiet));

    // After the user lowers the limit, only publications that shrink the
    // footprint remain allowed.
    let note = "The user reset the limit to the default";
    let reset = approve(&task, env, "10MB", note);
    assert_eq!(reset["previous_limit_bytes"], 20_000_000);
    assert_eq!(reset["limit_bytes"], 10_000_000);
    assert_eq!(reset["schema_version"], 2);
    assert_eq!(reset["blocked"], false);
    let plan = json(&workspace_env(&task, ["plan"], env));
    let usage = &plan["cloud_usage"];
    // Removing the approval changes the task manifest and withdraws the
    // requirement raise that only the approval needed.
    assert_eq!(plan["status"], "dry_run");
    assert_eq!(
        plan["changed_paths"],
        json!([CONFIG, format!("{task_id}/.workspace-mgr-task.toml")])
    );
    assert_eq!(plan["repository_requirement"], withdrawn_requirement());
    assert_eq!(usage["status"], "approval_required");
    assert_eq!(usage["cleanup_only"], true);
    assert_eq!(usage["publish_allowed"], true);
    assert_eq!(usage["git_history_exceeds_limit"], false);
    assert_eq!(usage["approval"], Value::Null);
    assert!(
        usage["message"]
            .as_str()
            .unwrap()
            .ends_with(
                "This publication only removes content, apart from at most 1 MiB (1048576 bytes) of new workspace-mgr control-file content, where metadata that only drops entries is free, so it remains allowed."
            )
    );
    std::fs::write(task.join("notes.md"), "notes\n").unwrap();
    assert_refused(
        &workspace_env_unchecked(&task, ["publish", "-m", "Publish notes"], env),
        &["limit 9.53 MiB (10000000 bytes)."],
    );
    std::fs::remove_file(task.join("notes.md")).unwrap();
    let removed = workspace_env(&task, ["remove", large.as_str()], env);
    assert!(stderr(&removed).contains(REMINDER));
    let cleaned = json(&workspace_env(
        &task,
        ["publish", "-m", "Remove large data"],
        env,
    ));
    let usage = &cleaned["cloud_usage"];
    assert_eq!(cleaned["status"], "pushed");
    assert_eq!(usage["status"], "approval_required");
    assert_eq!(usage["cleanup_only"], true);
    assert_eq!(usage["publish_allowed"], true);
    // Test storage is content-addressed and never purged.
    assert_eq!(usage["projected"]["s3_bytes"], LARGE);
    let tip = cleaned["remote_oid"].as_str().unwrap();
    assert_eq!(rev(&fixture.remote, branch).as_deref(), Some(tip));
    assert!(!tree_contains(
        &fixture.remote,
        tip,
        &format!("{large}.dvc")
    ));
    assert!(!commit_message(&fixture.remote, tip).contains(TRAILER));
    assert_ne!(
        json(&workspace_env(&task, ["task", "status"], env))["cloud_usage"]["pending"],
        Value::Null
    );
}

#[cfg(feature = "test-storage")]
#[test]
fn git_history_counts_toward_the_limit_and_cleanup_cannot_shrink_it() {
    let fixture = managed_fixture(false);
    let (task_id, task) = create_task(&fixture, "usage-history", "20260918-140000");
    document_task(&task);
    let branch = "refs/heads/codex/usage-history";
    let env = &LIMIT_300KB[..];
    let weights = format!("{task_id}/weights.bin");
    std::fs::write(task.join("weights.bin"), noise(400_000, 1)).unwrap();

    let plan = json(&workspace_env(&task, ["plan"], env));
    let usage = &plan["cloud_usage"];
    assert_eq!(usage["status"], "approval_required");
    assert_eq!(usage["publish_allowed"], false);
    assert_eq!(usage["cleanup_only"], false);
    assert_eq!(usage["git_history_exceeds_limit"], false);
    assert_eq!(usage["git_measure"], "packed");
    assert_eq!(usage["projected"]["s3_bytes"], 0);
    assert!(usage["projected"]["git_bytes"].as_u64().unwrap() > 400_000);
    let base = plan["remote_base_oid"].as_str().unwrap();
    let base_tree = format!("{base}^{{tree}}");
    let tree = plan["tree_oid"].as_str().unwrap();
    assert_eq!(
        usage["projected"]["git_uncompressed_bytes"],
        object_bytes(&fixture.shared, &[tree, "--not", base, &base_tree])
    );
    assert_git_contributor(&usage["contributors"][0], &weights, "pending");
    assert_refused(
        &workspace_env_unchecked(&task, ["publish", "-m", "Publish weights"], env),
        &["S3 0 bytes", "limit 292.96 KiB (300000 bytes)."],
    );
    assert_eq!(rev(&fixture.remote, branch), None);

    approve(&task, env, "1MB", "The user approved 1 MB for weights");
    let published = json(&workspace_env(
        &task,
        ["publish", "-m", "Publish weights"],
        env,
    ));
    assert_eq!(published["status"], "pushed");
    let tip = published["remote_oid"].as_str().unwrap().to_owned();
    assert_eq!(
        last_line(&commit_message(&fixture.remote, &tip)),
        Some(
            trailer(&json!({
                "limit_bytes": 1_000_000,
                "note": "The user approved 1 MB for weights",
            }))
            .as_str()
        )
    );

    approve(
        &task,
        env,
        "300000",
        "The user reset the limit to the default",
    );
    let removed = workspace_env(&task, ["remove", weights.as_str()], env);
    assert_no_reminder(&removed);
    let plan = json(&workspace_env(&task, ["plan"], env));
    let usage = &plan["cloud_usage"];
    assert_eq!(usage["status"], "approval_required");
    assert_eq!(usage["cleanup_only"], true);
    assert_eq!(usage["publish_allowed"], true);
    assert_eq!(usage["git_history_exceeds_limit"], true);
    assert!(usage["published"]["git_bytes"].as_u64().unwrap() > 400_000);
    let history = object_bytes(&fixture.shared, &[&tip, "--not", base, &base_tree]);
    assert_eq!(usage["published"]["git_uncompressed_bytes"], history);
    assert!(usage["message"].as_str().unwrap().contains(
        "Published Git history alone exceeds the limit and cleanup cannot shrink it; only an approval or discarding the task resolves it."
    ));
    let published_weights = usage["contributors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|contributor| contributor["path"] == weights.as_str())
        .unwrap();
    assert_git_contributor(published_weights, &weights, "published");
    let cleaned = json(&workspace_env(
        &task,
        ["publish", "-m", "Remove weights"],
        env,
    ));
    assert_eq!(cleaned["status"], "pushed");
    assert_eq!(cleaned["cloud_usage"]["publish_allowed"], true);
    let cleaned_tip = cleaned["remote_oid"].as_str().unwrap();
    assert!(!tree_contains(&fixture.remote, cleaned_tip, &weights));
    let after = json(&workspace_env(&task, ["plan"], env));
    let usage = &after["cloud_usage"];
    assert_eq!(after["status"], "no_changes");
    assert_eq!(usage["status"], "approval_required");
    assert_eq!(usage["git_history_exceeds_limit"], true);
    assert!(
        usage["published"]["git_uncompressed_bytes"]
            .as_u64()
            .unwrap()
            > history
    );

    std::fs::write(task.join("notes.md"), "notes\n").unwrap();
    assert_refused(
        &workspace_env_unchecked(&task, ["publish", "-m", "Publish notes"], env),
        &[
            "Published Git history alone exceeds the limit, so cleanup cannot resolve it; only an approval or discarding the task can.",
        ],
    );
    std::fs::remove_file(task.join("notes.md")).unwrap();

    // Discarding the task is the other way out, and it drops the private state.
    let manifest = task.join(".workspace-mgr-task.toml");
    let preview = workspace_env(&task, ["task", "discard", "--dry-run"], env);
    assert!(stderr(&preview).contains(REMINDER));
    assert_eq!(json(&preview)["status"], "dry_run");
    assert_eq!(cloud_usage_state(&fixture.shared).len(), 1);
    let discarded = json(&workspace_env(
        &fixture.shared,
        [
            "task",
            "discard",
            "--manifest",
            manifest.to_str().unwrap(),
            "--confirm",
            &task_id,
        ],
        env,
    ));
    assert_eq!(discarded["status"], "discarded");
    assert!(cloud_usage_state(&fixture.shared).is_empty());
    assert_eq!(rev(&fixture.remote, branch), None);
}

#[cfg(feature = "test-storage")]
#[test]
fn packed_git_contributors_report_compressed_bytes() {
    let fixture = managed_fixture(false);
    let (task_id, task) = create_task(&fixture, "usage-packed", "20260918-143000");
    document_task(&task);
    let env = &LIMIT_300KB[..];
    std::fs::write(task.join("weights.bin"), noise(400_000, 9)).unwrap();
    std::fs::write(task.join("zeros.log"), vec![0_u8; 2_000_000]).unwrap();

    let plan = json(&workspace_env(&task, ["plan"], env));
    let usage = &plan["cloud_usage"];
    assert_eq!(usage["status"], "approval_required");
    assert_eq!(usage["git_measure"], "packed");
    assert!(
        usage["projected"]["git_uncompressed_bytes"]
            .as_u64()
            .unwrap()
            > 2_400_000
    );
    assert!(usage["projected"]["git_bytes"].as_u64().unwrap() < 450_000);
    // Removing the compressible file would barely change the gated total, so
    // it must not look like the largest contributor.
    let contributors = usage["contributors"].as_array().unwrap();
    assert_git_contributor(
        &contributors[0],
        &format!("{task_id}/weights.bin"),
        "pending",
    );
    let zeros = contributors
        .iter()
        .find(|contributor| contributor["path"] == format!("{task_id}/zeros.log").as_str())
        .unwrap();
    assert!(zeros["bytes"].as_u64().unwrap() < 20_000, "{zeros}");
}

#[cfg(feature = "test-storage")]
#[test]
fn new_control_file_content_is_not_a_cleanup() {
    let fixture = managed_fixture(false);
    let (task_id, task) = create_task(&fixture, "usage-control", "20260918-145000");
    document_task(&task);
    let branch = "refs/heads/codex/usage-control";
    let env = &LIMIT_300KB[..];
    workspace_env(&task, ["publish", "-m", "Publish scaffold"], env);
    let padded = ignore_rules(2_000_000, 7);
    let ignore = task.join("notes/.gitignore");
    std::fs::create_dir(task.join("notes")).unwrap();

    // A task within its limit cannot cross it through control files.
    std::fs::write(&ignore, &padded).unwrap();
    let tip = rev(&fixture.remote, branch).unwrap();
    let plan = json(&workspace_env(&task, ["plan"], env));
    let usage = &plan["cloud_usage"];
    assert_eq!(usage["status"], "approval_required");
    assert_eq!(usage["cleanup_only"], false);
    assert_eq!(usage["publish_allowed"], false);
    assert!(usage["projected"]["git_bytes"].as_u64().unwrap() > 300_000);
    assert_refused(
        &workspace_env_unchecked(&task, ["publish", "-m", "Publish ignore rules"], env),
        &["limit 292.96 KiB (300000 bytes)."],
    );
    assert_eq!(rev(&fixture.remote, branch).as_deref(), Some(tip.as_str()));
    std::fs::remove_file(&ignore).unwrap();

    // Nor can a task that is already over its limit.
    approve(&task, env, "1MB", "The user approved 1 MB for weights");
    std::fs::write(task.join("weights.bin"), noise(400_000, 8)).unwrap();
    let published = json(&workspace_env(
        &task,
        ["publish", "-m", "Publish weights"],
        env,
    ));
    assert_eq!(published["status"], "pushed");
    approve(
        &task,
        env,
        "300000",
        "The user reset the limit to the default",
    );
    let tip = rev(&fixture.remote, branch).unwrap();
    let over = json(&workspace_env(&task, ["plan"], env));
    assert_eq!(over["cloud_usage"]["status"], "approval_required");
    assert_eq!(over["cloud_usage"]["cleanup_only"], true);
    std::fs::write(&ignore, &padded).unwrap();
    let plan = json(&workspace_env(&task, ["plan"], env));
    let usage = &plan["cloud_usage"];
    assert_eq!(usage["status"], "approval_required");
    assert_eq!(usage["cleanup_only"], false);
    assert_eq!(usage["publish_allowed"], false);
    assert!(
        usage["message"]
            .as_str()
            .unwrap()
            .contains("exceeds the limit")
    );
    assert!(
        !usage["message"]
            .as_str()
            .unwrap()
            .contains("remains allowed")
    );
    assert_refused(
        &workspace_env_unchecked(&task, ["publish", "-m", "Publish ignore rules"], env),
        &["limit 292.96 KiB (300000 bytes)."],
    );
    assert_eq!(rev(&fixture.remote, branch).as_deref(), Some(tip.as_str()));

    // Small control-file edits still count as cleanup.
    std::fs::write(&ignore, "# Local scratch output\n*.tmp\n").unwrap();
    let plan = json(&workspace_env(&task, ["plan"], env));
    let usage = &plan["cloud_usage"];
    assert_eq!(usage["status"], "approval_required");
    assert_eq!(usage["cleanup_only"], true);
    assert_eq!(usage["publish_allowed"], true);
    let cleaned = json(&workspace_env(
        &task,
        ["publish", "-m", "Ignore scratch output"],
        env,
    ));
    assert_eq!(cleaned["status"], "pushed");
    assert_ne!(rev(&fixture.remote, branch).as_deref(), Some(tip.as_str()));

    // Each publication may carry at most 1 MiB of new control-file content,
    // whether the rules grow, keep their size, or shrink: a ratchet that
    // rewrites them with ever more new content stops at the allowance.
    let cleanup = |content: &[u8], message: &str| {
        std::fs::write(&ignore, content).unwrap();
        let plan = json(&workspace_env(&task, ["plan"], env));
        assert_eq!(plan["cloud_usage"]["status"], "approval_required");
        plan["cloud_usage"]["cleanup_only"].as_bool().unwrap()
            && json(&workspace_env(&task, ["publish", "-m", message], env))["status"] == "pushed"
    };
    let refused = |content: &[u8], message: &str| {
        std::fs::write(&ignore, content).unwrap();
        let tip = rev(&fixture.remote, branch).unwrap();
        let plan = json(&workspace_env(&task, ["plan"], env));
        let usage = &plan["cloud_usage"];
        assert_eq!(usage["cleanup_only"], false, "{message}");
        assert_eq!(usage["publish_allowed"], false, "{message}");
        assert_refused(
            &workspace_env_unchecked(&task, ["publish", "-m", message], env),
            &["limit 292.96 KiB (300000 bytes)."],
        );
        assert_eq!(rev(&fixture.remote, branch).as_deref(), Some(tip.as_str()));
    };
    let step = ignore_rules(900_000, 9);
    assert!(step.len() < 1_000_000);
    assert!(cleanup(&step, "Ratchet 1"));
    refused(&ignore_rules(1_800_000, 10), "Ratchet 2");

    // Rewriting large published rules at the same size stores new content.
    approve(&task, env, "5MB", "The user approved 5 MB for ignore rules");
    std::fs::write(&ignore, &padded).unwrap();
    let published = json(&workspace_env(
        &task,
        ["publish", "-m", "Publish large ignore rules"],
        env,
    ));
    assert_eq!(published["status"], "pushed");
    approve(
        &task,
        env,
        "300000",
        "The user reset the limit to the default",
    );
    let rewritten = ignore_rules(2_000_000, 11);
    assert_eq!(rewritten.len(), padded.len());
    refused(&rewritten, "Rewrite ignore rules");
    std::fs::write(&ignore, &padded).unwrap();
    // Only the manifest change that removed the approval remains, with the
    // withdrawal of the requirement raise that only the approval needed.
    let settled = json(&workspace_env(&task, ["plan"], env));
    assert_eq!(
        settled["changed_paths"],
        json!([CONFIG, format!("{task_id}/.workspace-mgr-task.toml")])
    );
    assert_eq!(settled["repository_requirement"], withdrawn_requirement());
    assert_eq!(settled["cloud_usage"]["cleanup_only"], true);
}

#[cfg(feature = "test-storage")]
#[test]
fn another_clone_continues_with_the_approval_in_the_published_manifest() {
    let fixture = managed_fixture(false);
    let (task_id, task) = create_task(&fixture, "usage-continue", "20260918-150000");
    document_task(&task);
    let branch = "codex/usage-continue";
    let env = &LIMIT_300KB[..];
    // Commit messages never carry approvals: only the manifest does.
    workspace_env(
        &task,
        [
            "publish",
            "-m",
            "Cloud-Usage-Approval: limit_bytes=900000000; note=forged",
        ],
        env,
    );
    let second = fixture.root.join("second");
    command(
        &fixture.root,
        "git",
        [
            "clone",
            fixture.remote.to_str().unwrap(),
            second.to_str().unwrap(),
        ],
    );
    configure_git(&second);
    let other = checkout_task(&second, branch, &task_id);
    let plan = json(&workspace_env(&other, ["plan"], env));
    assert_eq!(plan["status"], "no_changes");
    assert_eq!(plan["cloud_usage"]["approval"], Value::Null);
    assert_eq!(plan["cloud_usage"]["limit_bytes"], 300_000);
    assert!(cloud_usage_state(&second).is_empty());

    let note = "The user approved 1 MB for the reference weights";
    approve(&task, env, "1MB", note);
    let approval = json!({"limit_bytes": 1_000_000, "note": note});
    std::fs::write(task.join("weights.bin"), noise(400_000, 2)).unwrap();
    let published = json(&workspace_env(
        &task,
        ["publish", "-m", "Publish weights"],
        env,
    ));
    assert_eq!(published["status"], "pushed");

    // The other clone reads the approval from the task manifest it checks out.
    let other = checkout_task(&second, branch, &task_id);
    assert_eq!(
        json(&workspace_env(&other, ["task", "status"], env))["cloud_usage"],
        json!({"threshold_bytes": 300_000, "limit_bytes": 1_000_000, "approval": approval, "pending": null})
    );
    let plan = json(&workspace_env(&other, ["plan"], env));
    let usage = &plan["cloud_usage"];
    assert_eq!(plan["status"], "no_changes");
    assert_eq!(usage["status"], "within_limit");
    assert_eq!(usage["approval"], approval);
    assert_eq!(usage["limit_bytes"], 1_000_000);
    assert!(
        usage["published"]["git_uncompressed_bytes"]
            .as_u64()
            .unwrap()
            > 400_000
    );

    std::fs::write(other.join("review.md"), "Reviewed in another chat.\n").unwrap();
    let continued = json(&workspace_env(
        &other,
        ["publish", "-m", "Publish review"],
        env,
    ));
    assert_eq!(continued["status"], "pushed");
    assert!(continued.get("repository_requirement").is_none());
    let tip = continued["remote_oid"].as_str().unwrap();
    assert!(tree_contains(
        &fixture.remote,
        tip,
        &format!("{task_id}/weights.bin")
    ));
    assert_eq!(
        last_line(&commit_message(&fixture.remote, tip)),
        Some(trailer(&approval).as_str())
    );

    // A later decision recorded in either clone is a manifest change.
    let raised = approve(&other, env, "2MB", "The user raised the limit to 2 MB");
    assert_eq!(raised["previous_limit_bytes"], 1_000_000);
    let plan = json(&workspace_env(&other, ["plan"], env));
    assert_eq!(plan["cloud_usage"]["limit_bytes"], 2_000_000);
    assert_eq!(
        plan["cloud_usage"]["approval"]["note"],
        "The user raised the limit to 2 MB"
    );
    assert_eq!(
        plan["changed_paths"],
        json!([format!("{task_id}/.workspace-mgr-task.toml")])
    );
}

#[cfg(all(unix, feature = "test-storage"))]
#[test]
fn late_rechecks_refuse_growth_that_appears_after_the_gate() {
    use std::os::unix::fs::PermissionsExt;

    if !dvc_available() {
        return;
    }
    let fixture = managed_fixture(true);
    let (task_id, task) = create_task(&fixture, "usage-late", "20260918-160000");
    document_task(&task);
    let branch = "refs/heads/codex/usage-late";
    let env = &LIMIT_300KB[..];
    let storage_remote = fixture.root.join("storage-remote");
    let small = format!("{task_id}/small.dat");
    std::fs::write(task.join("small.dat"), "small\n").unwrap();
    workspace_env(
        &task,
        [
            "storage",
            "set",
            small.as_str(),
            "--to",
            "s3",
            "--reason",
            "Keep measurements in storage.",
        ],
        env,
    );
    let first = json(&workspace_env(
        &task,
        ["publish", "-m", "Publish measurements"],
        env,
    ));
    assert_eq!(first["status"], "pushed");
    assert_eq!(first["cloud_usage"]["projected"]["s3_bytes"], 6);

    // A background writer changes the worktree right before an engine step.
    let engine = which::which("dvc").unwrap();
    let writer = fixture.root.join("background-writer");
    std::fs::write(
        &writer,
        format!(
            "#!/bin/sh\nif [ \"$LATE_MODE\" = grow ] && [ \"$1\" = commit ]; then\n  head -c 400000 /dev/zero >> \"$LATE_TARGET\"\nfi\nif [ \"$LATE_MODE\" = add ] && [ \"$1\" = push ]; then\n  head -c 400000 /dev/urandom > \"$LATE_TARGET\"\nfi\nexec '{}' \"$@\"\n",
            engine.display()
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&writer).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&writer, permissions).unwrap();
    let late_publish = |mode: &str, target: &Path, message: &str| {
        workspace_env_unchecked(
            &task,
            ["publish", "-m", message],
            &[
                LIMIT_300KB[0],
                LIMIT_300KB[1],
                ("WORKSPACE_MGR_STORAGE_DVC", writer.to_str().unwrap()),
                ("LATE_MODE", mode),
                ("LATE_TARGET", target.to_str().unwrap()),
            ],
        )
    };

    // (i) An output grows after the gate: refused before anything is uploaded.
    std::fs::write(task.join("small.dat"), "changed\n").unwrap();
    let tip = rev(&fixture.remote, branch).unwrap();
    let storage_before = remote_snapshot(&storage_remote);
    let plan = json(&workspace_env(&task, ["plan"], env));
    assert_eq!(plan["cloud_usage"]["status"], "within_limit");
    assert_eq!(plan["cloud_usage"]["projected"]["s3_bytes"], 14);
    let refused = late_publish("grow", &task.join("small.dat"), "Publish changed data");
    assert_refused(
        &refused,
        &[
            "S3 6 bytes, total",
            "S3 390.63 KiB (400014 bytes), total",
            "limit 292.96 KiB (300000 bytes).",
        ],
    );
    assert_eq!(rev(&fixture.remote, branch).as_deref(), Some(tip.as_str()));
    assert_eq!(remote_snapshot(&storage_remote), storage_before);
    assert_eq!(
        std::fs::metadata(task.join("small.dat")).unwrap().len(),
        400_008
    );
    let pending =
        json(&workspace_env(&task, ["task", "status"], env))["cloud_usage"]["pending"].clone();
    assert_eq!(pending["limit_bytes"], 300_000);
    assert_eq!(pending["published"]["s3_bytes"], 6);
    assert_eq!(pending["projected"]["s3_bytes"], 400_014);

    std::fs::write(task.join("small.dat"), "changed\n").unwrap();
    let restored = json(&workspace_env(
        &task,
        ["publish", "-m", "Publish changed data"],
        env,
    ));
    assert_eq!(restored["status"], "pushed");
    assert_eq!(restored["cloud_usage"]["status"], "within_limit");
    assert_eq!(
        json(&workspace_env(&task, ["task", "status"], env))["cloud_usage"]["pending"],
        Value::Null
    );

    // (ii) Git content appears after the upload: refused before any commit.
    std::fs::write(task.join("small.dat"), "changed again\n").unwrap();
    let tip = rev(&fixture.remote, branch).unwrap();
    let local_branch = rev(&fixture.shared, branch);
    let refused = late_publish("add", &task.join("late.bin"), "Publish more data");
    assert_refused(
        &refused,
        &[
            "S3 14 bytes, total",
            "S3 28 bytes, total",
            "limit 292.96 KiB (300000 bytes).",
        ],
    );
    assert_eq!(rev(&fixture.remote, branch).as_deref(), Some(tip.as_str()));
    assert_eq!(rev(&fixture.shared, branch), local_branch);
    let pending =
        json(&workspace_env(&task, ["task", "status"], env))["cloud_usage"]["pending"].clone();
    assert!(
        pending["projected"]["git_uncompressed_bytes"]
            .as_u64()
            .unwrap()
            > 400_000
    );
    assert!(pending["projected"]["git_bytes"].as_u64().unwrap() > 300_000);
    assert_eq!(pending["projected"]["s3_bytes"], 28);
}

#[cfg(feature = "test-storage")]
#[test]
fn content_addressed_directories_are_charged_per_file_at_every_check() {
    if !dvc_available() {
        return;
    }
    let fixture = managed_fixture(true);
    let (task_id, task) = create_task(&fixture, "usage-directory", "20260918-163000");
    document_task(&task);
    let branch = "refs/heads/codex/usage-directory";
    let storage = fixture.root.join("storage-remote");
    let env = &LIMIT_10MB[..];
    let data = task.join("data");
    let path = |name: &str| format!("{task_id}/data/{name}");
    std::fs::create_dir(&data).unwrap();
    for (name, fill) in [("a.bin", 1_u8), ("b.bin", 2), ("c.bin", 3)] {
        std::fs::write(data.join(name), vec![fill; 2_000_000]).unwrap();
    }
    workspace_env(
        &task,
        [
            "storage",
            "set",
            &format!("{task_id}/data"),
            "--to",
            "s3",
            "--reason",
            "Keep the dataset in storage.",
        ],
        env,
    );
    let first = json(&workspace_env(
        &task,
        ["publish", "-m", "Publish data"],
        env,
    ));
    assert_eq!(first["status"], "pushed");
    assert_eq!(first["cloud_usage"]["projected"]["s3_bytes"], 6_000_000);

    // The gate, the check after the storage commit, and the replay of the
    // published commit charge the same new file digests.
    let publish_and_settle = |message: &str, projected: u64| {
        let plan = json(&workspace_env(&task, ["plan"], env));
        let usage = &plan["cloud_usage"];
        assert_eq!(usage["status"], "within_limit", "{message}: {usage}");
        assert_eq!(usage["projected"]["s3_bytes"], projected, "{message}");
        let published = json(&workspace_env(&task, ["publish", "-m", message], env));
        assert_eq!(published["status"], "pushed", "{message}");
        assert_eq!(
            published["cloud_usage"]["projected"]["s3_bytes"], projected,
            "{message}"
        );
        let settled = json(&workspace_env(&task, ["plan"], env));
        assert_eq!(settled["status"], "no_changes", "{message}");
        let usage = &settled["cloud_usage"];
        assert_eq!(usage["published"]["s3_bytes"], projected, "{message}");
        assert_eq!(usage["projected"]["s3_bytes"], projected, "{message}");
        settled
    };

    // Adding a file charges the file.
    std::fs::write(data.join("notes.txt"), "notes\n").unwrap();
    let settled = publish_and_settle("Add notes", 6_000_006);
    let contributors = settled["cloud_usage"]["contributors"].as_array().unwrap();
    for (name, bytes) in [("a.bin", 2_000_000), ("notes.txt", 6)] {
        assert!(
            contributors.contains(&json!({"path": path(name), "store": "s3", "bytes": bytes, "versions": 1, "state": "published"})),
            "{contributors:?}"
        );
    }
    assert_eq!(
        json(&workspace_env(&task, ["task", "status"], env))["cloud_usage"]["pending"],
        Value::Null
    );

    // Rewriting a file at the same size, or shrinking it with new content,
    // stores the new content next to the old.
    std::fs::write(data.join("a.bin"), vec![4_u8; 2_000_000]).unwrap();
    let settled = publish_and_settle("Rewrite a.bin", 8_000_006);
    assert!(
        settled["cloud_usage"]["contributors"]
            .as_array()
            .unwrap()
            .contains(&json!({"path": path("a.bin"), "store": "s3", "bytes": 4_000_000, "versions": 2, "state": "published"}))
    );
    std::fs::write(data.join("b.bin"), vec![5_u8; 1_000_000]).unwrap();
    publish_and_settle("Shrink b.bin", 9_000_006);

    // A same-size rewrite that would cross the limit is refused at the gate.
    let tip = rev(&fixture.remote, branch).unwrap();
    let before = remote_snapshot(&storage);
    std::fs::write(data.join("c.bin"), vec![6_u8; 2_000_000]).unwrap();
    let plan = json(&workspace_env(&task, ["plan"], env));
    let usage = &plan["cloud_usage"];
    assert_eq!(usage["status"], "approval_required");
    assert_eq!(usage["cleanup_only"], false);
    assert_eq!(usage["projected"]["s3_bytes"], 11_000_006);
    assert_refused(
        &workspace_env_unchecked(&task, ["publish", "-m", "Rewrite c.bin"], env),
        &["limit 9.53 MiB (10000000 bytes)."],
    );
    assert_eq!(rev(&fixture.remote, branch).as_deref(), Some(tip.as_str()));
    assert_eq!(remote_snapshot(&storage), before);
    approve(
        &task,
        env,
        "20MB",
        "The user approved 20 MB for the dataset",
    );
    let published = json(&workspace_env(
        &task,
        ["publish", "-m", "Rewrite c.bin"],
        env,
    ));
    assert_eq!(published["status"], "pushed");
    assert_eq!(
        published["cloud_usage"]["projected"]["s3_bytes"],
        11_000_006
    );

    // Back over the default limit, deleting a file stores no new digest, so
    // the deletion stays a cleanup at every check.
    approve(
        &task,
        env,
        "10000000",
        "The user reset the limit to the default",
    );
    let tip = rev(&fixture.remote, branch).unwrap();
    let before = remote_snapshot(&storage);
    std::fs::remove_file(data.join("b.bin")).unwrap();
    let plan = json(&workspace_env(&task, ["plan"], env));
    let usage = &plan["cloud_usage"];
    assert_eq!(usage["status"], "approval_required");
    assert_eq!(usage["cleanup_only"], true);
    assert_eq!(usage["publish_allowed"], true);
    assert_eq!(usage["projected"]["s3_bytes"], 11_000_006);
    let cleaned = json(&workspace_env(
        &task,
        ["publish", "-m", "Remove b.bin"],
        env,
    ));
    assert_eq!(cleaned["status"], "pushed");
    assert_eq!(cleaned["cloud_usage"]["cleanup_only"], true);
    assert_eq!(cleaned["cloud_usage"]["projected"]["s3_bytes"], 11_000_006);
    assert_ne!(rev(&fixture.remote, branch).as_deref(), Some(tip.as_str()));
    // Only the new directory manifest was uploaded.
    let after = remote_snapshot(&storage);
    assert_eq!(after.len(), before.len() + 1);
    assert!(
        after
            .iter()
            .filter(|(path, _)| !before.iter().any(|(old, _)| old == path))
            .all(|(path, _)| path.to_string_lossy().ends_with(".dir"))
    );
    let settled = json(&workspace_env(&task, ["plan"], env));
    assert_eq!(settled["status"], "no_changes");
    assert_eq!(settled["cloud_usage"]["published"]["s3_bytes"], 11_000_006);
}

#[cfg(all(unix, feature = "test-storage"))]
#[test]
fn symlinks_in_a_published_s3_directory_are_measured_by_their_targets() {
    use std::os::unix::fs::symlink;

    if !dvc_available() {
        return;
    }
    let fixture = managed_fixture(true);
    let (task_id, task) = create_task(&fixture, "usage-links", "20260918-164500");
    document_task(&task);
    let env = &LIMIT_10MB[..];
    let data = task.join("my dir");
    std::fs::create_dir(&data).unwrap();
    std::fs::write(data.join("ckpt-100.bin"), noise(3_000, 5)).unwrap();
    workspace_env(
        &task,
        [
            "storage",
            "set",
            &format!("{task_id}/my dir"),
            "--to",
            "s3",
            "--reason",
            "Keep checkpoints in storage.",
        ],
        env,
    );
    let first = json(&workspace_env(
        &task,
        ["publish", "-m", "Publish checkpoints"],
        env,
    ));
    assert_eq!(first["status"], "pushed");
    assert_eq!(first["cloud_usage"]["projected"]["s3_bytes"], 3_000);

    // Before the engine hashes it, a new link is charged the size of the
    // content it points to. Once recorded, the content-addressed remote
    // stores nothing new for content it already holds.
    symlink("ckpt-100.bin", data.join("latest.bin")).unwrap();
    let plan = workspace_env(&task, ["plan"], env);
    assert!(stderr(&plan).is_empty(), "{}", stderr(&plan));
    let usage = &json(&plan)["cloud_usage"];
    assert_eq!(usage["status"], "within_limit");
    assert_eq!(usage["published"]["s3_bytes"], 3_000);
    assert_eq!(usage["projected"]["s3_bytes"], 6_000);
    let linked = json(&workspace_env(
        &task,
        ["publish", "-m", "Link the latest checkpoint"],
        env,
    ));
    assert_eq!(linked["status"], "pushed");
    assert_eq!(linked["cloud_usage"]["projected"]["s3_bytes"], 3_000);

    // A link retargeted to a new checkpoint adds that checkpoint once.
    std::fs::write(data.join("ckpt-200.bin"), noise(5_000, 6)).unwrap();
    std::fs::remove_file(data.join("latest.bin")).unwrap();
    symlink("ckpt-200.bin", data.join("latest.bin")).unwrap();
    let plan = json(&workspace_env(&task, ["plan"], env));
    assert_eq!(plan["cloud_usage"]["projected"]["s3_bytes"], 13_000);
    let retargeted = json(&workspace_env(
        &task,
        ["publish", "-m", "Link the new checkpoint"],
        env,
    ));
    assert_eq!(retargeted["status"], "pushed");
    assert_eq!(retargeted["cloud_usage"]["projected"]["s3_bytes"], 8_000);
    let settled = json(&workspace_env(&task, ["plan"], env));
    assert_eq!(settled["status"], "no_changes");
    assert_eq!(settled["cloud_usage"]["published"]["s3_bytes"], 8_000);
    assert_eq!(settled["cloud_usage"]["projected"]["s3_bytes"], 8_000);
    // The remote holds exactly the two checkpoints plus manifests.
    let stored = remote_snapshot(&fixture.root.join("storage-remote"))
        .iter()
        .filter(|(path, _)| !path.to_string_lossy().ends_with(".dir"))
        .map(|(_, content)| content.len() as u64)
        .sum::<u64>();
    assert_eq!(stored, 8_000);
}

#[cfg(feature = "test-storage")]
#[test]
fn the_real_threshold_refuses_a_sparse_upload_before_tracking() {
    if !dvc_available() {
        return;
    }
    let fixture = managed_fixture(true);
    let (task_id, task) = create_task(&fixture, "sparse-gate", "20260918-170000");
    document_task(&task);
    let branch = "refs/heads/codex/sparse-gate";
    workspace(&task, ["publish", "-m", "Publish scaffold"]);
    let tip = rev(&fixture.remote, branch);
    let local_branch = rev(&fixture.shared, branch);
    let storage_remote = fixture.root.join("storage-remote");
    let storage_before = remote_snapshot(&storage_remote);
    let sparse = task.join("sparse.bin");
    std::fs::File::create(&sparse)
        .unwrap()
        .set_len(REAL_THRESHOLD + 1)
        .unwrap();

    let refused = workspace_unchecked(&task, ["publish", "-m", "Publish sparse data"]);
    assert_refused(
        &refused,
        &[
            &format!("workspace-mgr: cloud usage for task {task_id} needs the user's approval"),
            "S3 1 GiB (1073741825 bytes), total 1 GiB (",
            "limit 1 GiB (1073741824 bytes).",
        ],
    );
    assert!(!task.join("sparse.bin.dvc").exists());
    assert!(!task.join(".gitignore").exists());
    assert_eq!(cached_files(&fixture.shared), 0);
    assert_eq!(rev(&fixture.remote, branch), tip);
    assert_eq!(rev(&fixture.shared, branch), local_branch);
    assert_eq!(remote_snapshot(&storage_remote), storage_before);
    let usage = json(&workspace(&task, ["task", "status"]))["cloud_usage"].clone();
    assert_eq!(usage["threshold_bytes"], REAL_THRESHOLD);
    assert_eq!(usage["limit_bytes"], REAL_THRESHOLD);
    assert_eq!(
        usage["pending"]["projected"]["s3_bytes"],
        REAL_THRESHOLD + 1
    );

    std::fs::remove_file(&sparse).unwrap();
    let plan = json(&workspace(&task, ["plan"]));
    assert_eq!(plan["status"], "no_changes");
    assert_eq!(plan["cloud_usage"]["status"], "within_limit");
    assert!(cloud_usage_state(&fixture.shared).is_empty());
}

#[cfg(feature = "test-storage")]
#[test]
fn infrastructure_tasks_wait_for_approval_and_publish_the_trailer() {
    let fixture = managed_fixture(false);
    let env = &LIMIT_300KB[..];
    let created = json(&workspace_env(
        &fixture.shared,
        [
            "task",
            "create",
            "shared-assets",
            "--kind",
            "infrastructure",
            "--title",
            "Shared assets",
            "--purpose",
            "Publish repository-wide reference assets.",
            "--scope",
            "assets",
            "--scope-note",
            "The user requested shared reference assets.",
        ],
        env,
    ));
    let worktree = PathBuf::from(created["path"].as_str().unwrap());
    std::fs::create_dir(worktree.join("assets")).unwrap();
    std::fs::write(worktree.join("assets/model.bin"), noise(400_000, 3)).unwrap();

    let plan = json(&workspace_env(&worktree, ["plan"], env));
    assert_eq!(plan["cloud_usage"]["status"], "approval_required");
    assert_git_contributor(
        &plan["cloud_usage"]["contributors"][0],
        "assets/model.bin",
        "pending",
    );
    assert_refused(
        &workspace_env_unchecked(&worktree, ["publish", "-m", "Publish assets"], env),
        &["cloud usage for task infra-shared-assets needs the user's approval"],
    );
    assert_eq!(
        rev(&fixture.remote, "refs/heads/codex/infra-shared-assets"),
        None
    );
    let waiting = workspace_env(&worktree, ["storage", "status", "assets/model.bin"], env);
    assert!(stderr(&waiting).starts_with(&format!(
        "workspace-mgr: task infra-shared-assets {REMINDER}"
    )));

    // Infrastructure worktrees share the repository's private task state.
    let recorded = approve(&worktree, env, "1MB", "The user approved 1 MB of assets");
    assert_eq!(recorded["schema_version"], 3);
    assert_eq!(cloud_usage_state(&fixture.shared).len(), 1);
    let published = json(&workspace_env(
        &worktree,
        ["publish", "-m", "Publish assets"],
        env,
    ));
    assert_eq!(published["status"], "pushed");
    assert_eq!(published["cloud_usage"]["status"], "within_limit");
    // The infrastructure manifest is private, so the approval travels only
    // in the trailer and never raises the repository's requirement.
    assert!(published.get("repository_requirement").is_none());
    assert_eq!(published["changed_paths"], json!(["assets/model.bin"]));
    let tip = published["remote_oid"].as_str().unwrap();
    assert_eq!(
        show(&fixture.remote, &format!("{tip}:{CONFIG}")),
        std::fs::read_to_string(fixture.shared.join(CONFIG)).unwrap()
    );
    let message = commit_message(&fixture.remote, published["remote_oid"].as_str().unwrap());
    assert_eq!(
        last_line(&message),
        Some(
            trailer(&json!({
                "limit_bytes": 1_000_000,
                "note": "The user approved 1 MB of assets",
            }))
            .as_str()
        )
    );
    assert!(message.contains("Workspace-Scope: assets\n"));
    assert!(!message.contains("Workspace-Requirement"));
}
