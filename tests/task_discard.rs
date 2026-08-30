mod common;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use common::{GitFixture, git, git_unchecked, json, workspace, workspace_unchecked};

fn create_task(fixture: &GitFixture, slug: &str, timestamp: &str) -> (String, std::path::PathBuf) {
    let task_id = format!("{timestamp}-{slug}");
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            slug,
            "--title",
            "Discard test",
            "--purpose",
            "Verify safe task discard behavior.",
            "--timestamp",
            timestamp,
        ],
    );
    let task = fixture.shared.join(&task_id);
    (task_id, task)
}

#[test]
fn discard_requires_a_current_dry_run_and_exact_task_confirmation() {
    let fixture = GitFixture::new();
    fixture.clone_shared();
    workspace(&fixture.shared, ["init"]);
    let (task_id, task) = create_task(&fixture, "confirmation", "20260830-120000");
    let manifest = task.join(".workspace-mgr-task.toml");
    std::fs::write(task.join("notes.txt"), "discard me\n").unwrap();

    let missing_plan = workspace_unchecked(
        &fixture.shared,
        [
            "task",
            "discard",
            "--manifest",
            manifest.to_str().unwrap(),
            "--confirm",
            &task_id,
        ],
    );
    assert_eq!(missing_plan.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&missing_plan.stderr).contains("dry-run"));

    let preview = workspace(&task, ["task", "discard", "--dry-run"]);
    let preview = json(&preview);
    assert_eq!(preview["status"], "dry_run");
    assert_eq!(preview["operation"], "task-discard");
    assert_eq!(preview["review"]["managed_by"], "agent");
    assert_eq!(preview["review"]["provider_state_verified_by_cli"], false);
    assert!(
        preview["confirmation_plan"]
            .as_str()
            .unwrap()
            .contains("discard-plan.json")
    );
    assert!(task.exists());

    let wrong = workspace_unchecked(
        &fixture.shared,
        [
            "task",
            "discard",
            "--manifest",
            manifest.to_str().unwrap(),
            "--confirm",
            "another-task",
        ],
    );
    assert_eq!(wrong.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("exactly match"));
    assert!(task.exists());

    let branch = "codex/confirmation";
    let original = git(&fixture.shared, ["rev-parse", branch]);
    let original = String::from_utf8(original.stdout)
        .unwrap()
        .trim()
        .to_owned();
    let tree = git(&fixture.shared, ["show", "-s", "--format=%T", &original]);
    let tree = String::from_utf8(tree.stdout).unwrap().trim().to_owned();
    let changed = git(
        &fixture.shared,
        [
            "commit-tree",
            &tree,
            "-p",
            &original,
            "-m",
            "Changed after preview",
        ],
    );
    let changed = String::from_utf8(changed.stdout).unwrap().trim().to_owned();
    git(
        &fixture.shared,
        [
            "update-ref",
            &format!("refs/heads/{branch}"),
            &changed,
            &original,
        ],
    );
    let stale = workspace_unchecked(
        &fixture.shared,
        [
            "task",
            "discard",
            "--manifest",
            manifest.to_str().unwrap(),
            "--confirm",
            &task_id,
        ],
    );
    assert_eq!(stale.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&stale.stderr).contains("state changed"));
    assert!(task.exists());

    workspace(&task, ["task", "discard", "--dry-run"]);
    let discarded = workspace(
        &fixture.shared,
        [
            "task",
            "discard",
            "--manifest",
            manifest.to_str().unwrap(),
            "--confirm",
            &task_id,
        ],
    );
    let discarded = json(&discarded);
    assert_eq!(discarded["status"], "discarded");
    assert!(discarded.get("cleanup_warnings").is_none());
    assert!(!task.exists());
    assert!(
        !git_unchecked(&fixture.shared, ["rev-parse", "--verify", branch])
            .status
            .success()
    );
}

#[test]
fn published_deliverable_discard_deletes_branches_and_restores_additional_scopes() {
    let fixture = GitFixture::new();
    std::fs::write(fixture.seed.join("shared.txt"), "base value\n").unwrap();
    fixture.commit_seed("Add shared file");
    fixture.clone_shared();
    workspace(&fixture.shared, ["init"]);
    let task_id = "20260830-120100-published-discard";
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "published-discard",
            "--title",
            "Published discard",
            "--purpose",
            "Discard a published task and restore shared state.",
            "--timestamp",
            "20260830-120100",
            "--scope",
            "shared.txt",
            "--scope-note",
            "The discard test owns this shared file.",
        ],
    );
    let task = fixture.shared.join(task_id);
    let manifest = task.join(".workspace-mgr-task.toml");
    std::fs::write(task.join("result.txt"), "published task\n").unwrap();
    std::fs::write(fixture.shared.join("shared.txt"), "task value\n").unwrap();
    let published = workspace(&task, ["publish", "-m", "Publish discard fixture"]);
    let published = json(&published);
    let branch = "codex/published-discard";
    assert_eq!(published["status"], "pushed");
    assert!(
        git_unchecked(
            &fixture.shared,
            [
                "ls-remote",
                "--exit-code",
                "origin",
                &format!("refs/heads/{branch}")
            ]
        )
        .status
        .success()
    );
    git(&fixture.shared, ["add", "--", task_id, "shared.txt"]);

    let preview = workspace(&task, ["task", "discard", "--dry-run"]);
    let preview = json(&preview);
    assert_eq!(preview["remote_branch_oid"], published["remote_oid"]);
    assert_eq!(preview["local_actions"][0]["action"], "delete");
    assert_eq!(preview["local_actions"][1]["action"], "restore");
    assert!(
        preview["review"]["required_before_confirm"]
            .as_str()
            .unwrap()
            .contains("close")
    );

    let discarded = workspace(
        &fixture.shared,
        [
            "task",
            "discard",
            "--manifest",
            manifest.to_str().unwrap(),
            "--confirm",
            task_id,
        ],
    );
    let discarded = json(&discarded);
    assert_eq!(discarded["status"], "discarded");
    assert!(discarded.get("cleanup_warnings").is_none());
    assert!(!task.exists());
    assert_eq!(
        std::fs::read_to_string(fixture.shared.join("shared.txt")).unwrap(),
        "base value\n"
    );
    assert!(
        !git_unchecked(&fixture.shared, ["rev-parse", "--verify", branch])
            .status
            .success()
    );
    assert!(
        !git_unchecked(
            &fixture.shared,
            [
                "ls-remote",
                "--exit-code",
                "origin",
                &format!("refs/heads/{branch}")
            ]
        )
        .status
        .success()
    );
    assert!(
        git(&fixture.shared, ["status", "--short", "--", "shared.txt"])
            .stdout
            .is_empty()
    );
    assert!(
        git(
            &fixture.shared,
            [
                "diff",
                "--cached",
                "--name-only",
                "--",
                task_id,
                "shared.txt"
            ]
        )
        .stdout
        .is_empty()
    );
}

#[test]
fn discard_reports_but_does_not_purge_versioned_s3_boundaries() {
    let fixture = GitFixture::new();
    fixture.clone_shared();
    workspace(&fixture.shared, ["init"]);
    let (task_id, task) = create_task(&fixture, "s3-retention", "20260830-120200");
    let manifest = task.join(".workspace-mgr-task.toml");
    std::fs::write(
        task.join("artifact.bin.dvc"),
        "outs:\n- md5: abc\n  size: 3\n  path: artifact.bin\n  cloud:\n    storage:\n      version_id: exact-version-1\n",
    )
    .unwrap();
    let preview = workspace(&task, ["task", "discard", "--dry-run"]);
    let retained = &json(&preview)["retained_s3"][0];
    assert_eq!(retained["boundary"], format!("{task_id}/artifact.bin"));
    assert_eq!(retained["version_ids"][0], "exact-version-1");
    assert_eq!(retained["disposition"], "retained-not-purged");

    workspace(
        &fixture.shared,
        [
            "task",
            "discard",
            "--manifest",
            manifest.to_str().unwrap(),
            "--confirm",
            &task_id,
        ],
    );
    assert!(!task.exists());
}

#[test]
fn merged_task_is_refused_and_preserved() {
    let fixture = GitFixture::new();
    fixture.clone_shared();
    workspace(&fixture.shared, ["init"]);
    let (task_id, task) = create_task(&fixture, "already-merged", "20260830-120300");
    std::fs::write(task.join("result.txt"), "merged result\n").unwrap();
    workspace(&task, ["publish", "-m", "Publish merged fixture"]);
    let branch = "codex/already-merged";
    git(
        &fixture.shared,
        ["push", "origin", &format!("{branch}:main")],
    );

    let rejected = workspace_unchecked(&task, ["task", "discard", "--dry-run"]);
    assert_eq!(rejected.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("merged tasks cannot be discarded"));
    assert!(task.exists());
    assert!(
        git_unchecked(&fixture.shared, ["rev-parse", "--verify", branch])
            .status
            .success()
    );
    assert!(!task_id.is_empty());
}

#[test]
fn remote_branch_change_after_preview_requires_a_new_discard_plan() {
    let fixture = GitFixture::new();
    fixture.clone_shared();
    workspace(&fixture.shared, ["init"]);
    let (task_id, task) = create_task(&fixture, "remote-race", "20260830-120350");
    let manifest = task.join(".workspace-mgr-task.toml");
    std::fs::write(task.join("result.txt"), "remote race\n").unwrap();
    let published = workspace(&task, ["publish", "-m", "Publish remote race fixture"]);
    let published = json(&published);
    workspace(&task, ["task", "discard", "--dry-run"]);

    let old = published["remote_oid"].as_str().unwrap();
    let tree = git(&fixture.shared, ["show", "-s", "--format=%T", old]);
    let tree = String::from_utf8(tree.stdout).unwrap().trim().to_owned();
    let message = format!("Concurrent task update\n\nWorkspace-Task: {task_id}\n");
    let changed = git(
        &fixture.shared,
        ["commit-tree", &tree, "-p", old, "-m", &message],
    );
    let changed = String::from_utf8(changed.stdout).unwrap().trim().to_owned();
    git(
        &fixture.shared,
        [
            "push",
            "origin",
            &format!("{changed}:refs/heads/codex/remote-race"),
        ],
    );

    let stale = workspace_unchecked(
        &fixture.shared,
        [
            "task",
            "discard",
            "--manifest",
            manifest.to_str().unwrap(),
            "--confirm",
            &task_id,
        ],
    );
    assert_eq!(stale.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&stale.stderr).contains("state changed"));
    assert!(task.exists());
    let remote = git(
        &fixture.shared,
        ["ls-remote", "origin", "refs/heads/codex/remote-race"],
    );
    assert!(
        String::from_utf8(remote.stdout)
            .unwrap()
            .starts_with(&changed)
    );
}

#[cfg(unix)]
#[test]
fn rejected_remote_deletion_restores_the_local_task() {
    let fixture = GitFixture::new();
    fixture.clone_shared();
    workspace(&fixture.shared, ["init"]);
    let (task_id, task) = create_task(&fixture, "remote-rollback", "20260830-120400");
    let manifest = task.join(".workspace-mgr-task.toml");
    std::fs::write(task.join("result.txt"), "must survive rollback\n").unwrap();
    workspace(&task, ["publish", "-m", "Publish rollback fixture"]);
    workspace(&task, ["task", "discard", "--dry-run"]);

    let hook = fixture.remote.join("hooks/pre-receive");
    std::fs::write(&hook, "#!/bin/sh\nexit 23\n").unwrap();
    let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&hook, permissions).unwrap();

    let rejected = workspace_unchecked(
        &fixture.shared,
        [
            "task",
            "discard",
            "--manifest",
            manifest.to_str().unwrap(),
            "--confirm",
            &task_id,
        ],
    );
    assert_eq!(rejected.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("local scopes were restored"));
    assert_eq!(
        std::fs::read_to_string(task.join("result.txt")).unwrap(),
        "must survive rollback\n"
    );
    assert!(
        git_unchecked(
            &fixture.shared,
            ["rev-parse", "--verify", "codex/remote-rollback"]
        )
        .status
        .success()
    );
    assert!(
        git_unchecked(
            &fixture.shared,
            [
                "ls-remote",
                "--exit-code",
                "origin",
                "refs/heads/codex/remote-rollback"
            ]
        )
        .status
        .success()
    );
    let quarantine = fixture.shared.join(".git/workspace-mgr/discard-quarantine");
    assert!(
        !quarantine.exists() || std::fs::read_dir(quarantine).unwrap().next().is_none(),
        "successful rollback must not leave a private quarantine"
    );
}

#[test]
fn infrastructure_discard_removes_its_worktree_and_branches() {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["init"]);
    fixture.commit_seed("Add workspace policy");
    fixture.clone_shared();
    let created = workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "discard-policy",
            "--kind",
            "infrastructure",
            "--title",
            "Discard policy",
            "--purpose",
            "Verify infrastructure discard.",
            "--scope",
            "policy.md",
            "--scope-note",
            "The discard test owns this policy file.",
        ],
    );
    let created = json(&created);
    let worktree = std::path::PathBuf::from(created["path"].as_str().unwrap());
    let manifest = std::path::PathBuf::from(created["manifest"].as_str().unwrap());
    std::fs::write(worktree.join("policy.md"), "temporary policy\n").unwrap();
    workspace(&worktree, ["publish", "-m", "Publish disposable policy"]);
    let preview = workspace(&worktree, ["task", "discard", "--dry-run"]);
    assert_eq!(
        json(&preview)["local_actions"][0]["action"],
        "delete-worktree"
    );

    let inside = workspace_unchecked(
        &worktree,
        ["task", "discard", "--confirm", "infra-discard-policy"],
    );
    assert_eq!(inside.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&inside.stderr).contains("shared checkout"));
    assert!(worktree.exists());

    let discarded = workspace(
        &fixture.shared,
        [
            "task",
            "discard",
            "--manifest",
            manifest.to_str().unwrap(),
            "--confirm",
            "infra-discard-policy",
        ],
    );
    let discarded = json(&discarded);
    assert_eq!(discarded["status"], "discarded");
    assert!(discarded.get("cleanup_warnings").is_none());
    assert!(!worktree.exists());
    assert!(
        !git_unchecked(
            &fixture.shared,
            ["rev-parse", "--verify", "codex/infra-discard-policy"]
        )
        .status
        .success()
    );
    assert!(
        !git_unchecked(
            &fixture.shared,
            [
                "ls-remote",
                "--exit-code",
                "origin",
                "refs/heads/codex/infra-discard-policy"
            ]
        )
        .status
        .success()
    );
    assert!(
        !git(&fixture.shared, ["worktree", "list", "--porcelain"])
            .stdout
            .windows(worktree.as_os_str().len())
            .any(|window| window == worktree.as_os_str().as_encoded_bytes())
    );
}
