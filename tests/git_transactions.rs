mod common;

use common::*;
use fs2::FileExt;

fn managed_fixture() -> GitFixture {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["init"]);
    fixture.commit_seed("Add workspace policy");
    fixture.clone_shared();
    fixture
}

#[test]
fn publishes_only_the_task_scope_without_switching_main() {
    let fixture = managed_fixture();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "git-flow",
            "--title",
            "Git flow",
            "--purpose",
            "Test scoped publication.",
            "--timestamp",
            "20260829-170100",
        ],
    );
    std::fs::write(fixture.shared.join("unrelated.txt"), "another task\n").unwrap();

    let published = workspace(
        &fixture.shared.join("20260829-170100-git-flow"),
        ["publish", "-m", "Publish scoped task"],
    );
    let payload = json(&published);
    assert_eq!(payload["status"], "pushed");
    assert_eq!(payload["head"], "main");
    let commit = payload["commit_oid"].as_str().unwrap();
    let readme = git(
        &fixture.shared,
        [
            "show",
            &format!("{commit}:20260829-170100-git-flow/README.md"),
        ],
    );
    assert!(String::from_utf8_lossy(&readme.stdout).contains("Git flow"));
    let unrelated = git_unchecked(
        &fixture.shared,
        ["cat-file", "-e", &format!("{commit}:unrelated.txt")],
    );
    assert!(!unrelated.status.success());
    assert_eq!(
        String::from_utf8_lossy(&git(&fixture.shared, ["branch", "--show-current"]).stdout).trim(),
        "main"
    );

    let plan = workspace(&fixture.shared.join("20260829-170100-git-flow"), ["plan"]);
    assert_eq!(json(&plan)["status"], "no_changes");
}

#[test]
fn published_git_placement_stays_stable_when_a_file_grows() {
    let fixture = managed_fixture();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "sticky-git",
            "--title",
            "Sticky Git",
            "--purpose",
            "Verify published placement remains stable.",
            "--timestamp",
            "20260829-170150",
        ],
    );
    let task_id = "20260829-170150-sticky-git";
    let task = fixture.shared.join(task_id);
    document_task(&task);
    let retained = task.join("retained.bin");
    std::fs::write(&retained, vec![1_u8; 512]).unwrap();
    workspace(&task, ["publish", "-m", "Publish small Git file"]);

    std::fs::write(&retained, vec![2_u8; 10_485_761]).unwrap();
    let status = workspace(
        &task,
        ["storage", "status", &format!("{task_id}/retained.bin")],
    );
    assert_eq!(json(&status)["placements"][0]["target"], "git");
    assert_eq!(json(&status)["placements"][0]["basis"], "published-history");
    workspace(
        &task,
        [
            "storage",
            "set",
            &format!("{task_id}/retained.bin"),
            "--to",
            "git",
            "--reason",
            "Keep the published Git placement explicit for this check.",
        ],
    );
    let reset = workspace(
        &task,
        ["storage", "reset", &format!("{task_id}/retained.bin")],
    );
    assert_eq!(json(&reset)["placements"][0]["target"], "git");
    assert!(!task.join("retained.bin.dvc").exists());
    let plan = workspace(&task, ["plan"]);
    assert_eq!(json(&plan)["status"], "dry_run");
    assert!(
        json(&plan)["storage"]["placement"]["decisions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[cfg(unix)]
#[test]
fn managed_storage_paths_may_not_escape_through_symlinks() {
    let fixture = managed_fixture();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "symlink-boundary",
            "--title",
            "Symlink boundary",
            "--purpose",
            "Reject repository metadata writes through symlinks.",
            "--timestamp",
            "20260829-170152",
        ],
    );
    let task_id = "20260829-170152-symlink-boundary";
    let task = fixture.shared.join(task_id);
    let outside = fixture.root.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("data.txt"), "outside\n").unwrap();
    std::os::unix::fs::symlink(&outside, task.join("linked")).unwrap();

    let rejected = workspace_unchecked(
        &task,
        [
            "storage",
            "set",
            &format!("{task_id}/linked/data.txt"),
            "--to",
            "git",
            "--reason",
            "This path must not escape.",
        ],
    );
    assert_eq!(rejected.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("symlink"));
    assert!(!outside.join("data.txt.workspace-mgr-storage.toml").exists());
}

#[test]
fn storage_status_uses_the_task_history_not_unrelated_branches() {
    let fixture = managed_fixture();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "history-context",
            "--title",
            "History context",
            "--purpose",
            "Keep placement history task-specific.",
            "--timestamp",
            "20260829-170155",
        ],
    );
    let task_id = "20260829-170155-history-context";
    let task = fixture.shared.join(task_id);
    std::fs::write(task.join("data.txt"), "new task content\n").unwrap();

    let unrelated = fixture.root.join("unrelated-worktree");
    git(
        &fixture.shared,
        [
            "worktree",
            "add",
            "-b",
            "unrelated-history",
            unrelated.to_str().unwrap(),
            "main",
        ],
    );
    configure_git(&unrelated);
    let unrelated_task = unrelated.join(task_id);
    std::fs::create_dir(&unrelated_task).unwrap();
    std::fs::write(unrelated_task.join("data.txt"), "unrelated branch\n").unwrap();
    git(&unrelated, ["add", "-A"]);
    git(&unrelated, ["commit", "-m", "Add unrelated path history"]);

    let status = workspace(&task, ["storage", "status", &format!("{task_id}/data.txt")]);
    assert_eq!(
        json(&status)["placements"][0]["basis"],
        "automatic-size-fallback"
    );
}

#[test]
fn explicit_git_directory_applies_recursively_and_status_lists_git_content() {
    let fixture = managed_fixture();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "git-directory",
            "--title",
            "Git directory",
            "--purpose",
            "Verify recursive explicit placement.",
            "--timestamp",
            "20260829-170155",
        ],
    );
    let task_id = "20260829-170155-git-directory";
    let task = fixture.shared.join(task_id);
    let directory = task.join("reviewable");
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(directory.join("large.bin"), vec![3_u8; 10_485_761]).unwrap();
    workspace(
        &task,
        [
            "storage",
            "set",
            &format!("{task_id}/reviewable"),
            "--to",
            "git",
            "--reason",
            "The complete directory must remain reviewable in Git.",
        ],
    );

    let status = workspace(&task, ["storage", "status"]);
    let placements = json(&status)["placements"].as_array().unwrap().clone();
    assert!(placements.iter().any(|entry| {
        entry["path"] == format!("{task_id}/reviewable") && entry["target"] == "git"
    }));
    assert!(placements.iter().any(|entry| {
        entry["path"] == format!("{task_id}/README.md") && entry["target"] == "git"
    }));
    assert!(
        !placements
            .iter()
            .any(|entry| { entry["path"] == format!("{task_id}/reviewable/large.bin") })
    );
    let reset = workspace(
        &task,
        ["storage", "reset", &format!("{task_id}/reviewable")],
    );
    assert!(json(&reset)["placements"].as_array().unwrap().is_empty());
    let directory_status = workspace_unchecked(
        &task,
        ["storage", "status", &format!("{task_id}/reviewable")],
    );
    assert_eq!(directory_status.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&directory_status.stderr)
            .contains("is not a single storage boundary")
    );
    let plan = workspace_unchecked(&task, ["plan"]);
    assert_eq!(plan.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&plan.stderr).contains("automatic policy selected S3"));
}

#[cfg(unix)]
#[test]
fn plan_prunes_ignored_directories_before_inspecting_their_contents() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = managed_fixture();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "ignored-directories",
            "--title",
            "Ignored directories",
            "--purpose",
            "Verify ignored directory pruning.",
            "--timestamp",
            "20260829-170157",
        ],
    );
    let task_id = "20260829-170157-ignored-directories";
    let task = fixture.shared.join(task_id);
    std::fs::write(task.join(".gitignore"), ".venv/\n.chat-sync-state/\n").unwrap();
    let ignored = [task.join(".venv"), task.join(".chat-sync-state")];
    for directory in &ignored {
        std::fs::create_dir(directory).unwrap();
        std::fs::write(directory.join("sentinel"), "must not be inspected\n").unwrap();
        let mut permissions = std::fs::metadata(directory).unwrap().permissions();
        permissions.set_mode(0o000);
        std::fs::set_permissions(directory, permissions).unwrap();
    }

    let plan = workspace_unchecked(&task, ["plan"]);
    let status = workspace_unchecked(&task, ["storage", "status"]);

    for directory in &ignored {
        let mut permissions = std::fs::metadata(directory).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(directory, permissions).unwrap();
    }

    assert!(
        plan.status.success(),
        "plan failed:\n{}",
        String::from_utf8_lossy(&plan.stderr)
    );
    assert!(
        status.status.success(),
        "storage status failed:\n{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(json(&plan)["ignored_entries"].as_u64().unwrap() <= 2);
    assert!(
        json(&status)["paths"]
            .as_array()
            .unwrap()
            .iter()
            .all(|path| {
                let path = path.as_str().unwrap();
                !path.contains("/.venv/") && !path.contains("/.chat-sync-state/")
            })
    );
}

#[test]
fn additional_scope_requires_and_records_a_reason() {
    let fixture = managed_fixture();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "extra-scope",
            "--title",
            "Extra scope",
            "--purpose",
            "Test explicit authorization.",
            "--timestamp",
            "20260829-170200",
        ],
    );
    std::fs::write(fixture.shared.join("authorized.txt"), "allowed\n").unwrap();
    let task = fixture.shared.join("20260829-170200-extra-scope");
    document_task(&task);
    let rejected = workspace_unchecked(
        &task,
        ["publish", "-m", "Publish", "--include", "authorized.txt"],
    );
    assert_eq!(rejected.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("--scope-note"));

    let published = workspace(
        &task,
        [
            "publish",
            "-m",
            "Publish authorized scope",
            "--include",
            "authorized.txt",
            "--scope-note",
            "The user explicitly requested this root file.",
        ],
    );
    let commit = json(&published)["commit_oid"].as_str().unwrap().to_owned();
    let message = git(&fixture.shared, ["show", "-s", "--format=%B", &commit]);
    assert!(
        String::from_utf8_lossy(&message.stdout)
            .contains("Workspace-Task: 20260829-170200-extra-scope")
    );
    assert!(String::from_utf8_lossy(&message.stdout).contains("Scope-Authorization"));
}

#[test]
fn a_remote_branch_cannot_be_shared_by_distinct_tasks() {
    let fixture = managed_fixture();
    let competitor = fixture.root.join("competitor");
    command(
        &fixture.root,
        "git",
        [
            "clone",
            fixture.remote.to_str().unwrap(),
            competitor.to_str().unwrap(),
        ],
    );
    configure_git(&competitor);

    for (repo, timestamp, title) in [
        (&fixture.shared, "20260829-170210", "First branch owner"),
        (&competitor, "20260829-170211", "Competing branch owner"),
    ] {
        workspace(
            repo,
            [
                "task",
                "create",
                "shared-slug",
                "--title",
                title,
                "--purpose",
                "Verify that one branch cannot combine distinct tasks.",
                "--timestamp",
                timestamp,
            ],
        );
    }

    let first = fixture.shared.join("20260829-170210-shared-slug");
    document_task(&first);
    std::fs::write(first.join("first.txt"), "first task\n").unwrap();
    let published = workspace(&first, ["publish", "-m", "Publish first branch owner"]);
    let remote_oid = json(&published)["remote_oid"].as_str().unwrap().to_owned();

    let second = competitor.join("20260829-170211-shared-slug");
    std::fs::write(second.join("second.txt"), "second task\n").unwrap();
    let rejected = workspace_unchecked(&second, ["plan"]);
    assert_eq!(rejected.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("another task"));
    let remote = git(
        &fixture.shared,
        ["ls-remote", "origin", "refs/heads/codex/shared-slug"],
    );
    assert_eq!(
        String::from_utf8_lossy(&remote.stdout)
            .split_whitespace()
            .next(),
        Some(remote_oid.as_str())
    );
}

#[test]
fn refresh_preserves_working_tree_overlays() {
    let fixture = managed_fixture();
    std::fs::write(fixture.seed.join("refresh-update.txt"), "old update\n").unwrap();
    std::fs::write(fixture.seed.join("refresh-delete.txt"), "delete me\n").unwrap();
    std::fs::write(fixture.seed.join("refresh-file-to-dir"), "old file\n").unwrap();
    std::fs::create_dir(fixture.seed.join("refresh-dir-to-file")).unwrap();
    std::fs::write(
        fixture.seed.join("refresh-dir-to-file/old.txt"),
        "old child\n",
    )
    .unwrap();
    std::fs::create_dir(fixture.seed.join("refresh-overlay-dir")).unwrap();
    std::fs::write(
        fixture.seed.join("refresh-overlay-dir/old.txt"),
        "old overlay child\n",
    )
    .unwrap();
    fixture.commit_seed("Add refresh fixtures");
    git(&fixture.shared, ["pull", "--ff-only", "origin", "main"]);
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "refresh-flow",
            "--title",
            "Refresh flow",
            "--purpose",
            "Test index-only refresh.",
            "--timestamp",
            "20260829-170400",
        ],
    );
    let task = fixture.shared.join("20260829-170400-refresh-flow");
    let published = workspace(&task, ["publish", "-m", "Publish refresh task"]);
    let mut new_main = json(&published)["commit_oid"].as_str().unwrap().to_owned();
    git(
        &fixture.remote,
        ["update-ref", "refs/heads/main", &new_main],
    );
    git(&fixture.seed, ["fetch", "origin", "main"]);
    git(&fixture.seed, ["merge", "--ff-only", "FETCH_HEAD"]);
    std::fs::write(fixture.seed.join("refresh-update.txt"), "new update\n").unwrap();
    std::fs::remove_file(fixture.seed.join("refresh-delete.txt")).unwrap();
    std::fs::write(fixture.seed.join("refresh-added.txt"), "new file\n").unwrap();
    std::fs::remove_file(fixture.seed.join("refresh-file-to-dir")).unwrap();
    std::fs::create_dir(fixture.seed.join("refresh-file-to-dir")).unwrap();
    std::fs::write(
        fixture.seed.join("refresh-file-to-dir/new.txt"),
        "new child\n",
    )
    .unwrap();
    std::fs::remove_dir_all(fixture.seed.join("refresh-dir-to-file")).unwrap();
    std::fs::write(fixture.seed.join("refresh-dir-to-file"), "new file\n").unwrap();
    std::fs::remove_dir_all(fixture.seed.join("refresh-overlay-dir")).unwrap();
    std::fs::write(
        fixture.seed.join("refresh-overlay-dir"),
        "remote replacement\n",
    )
    .unwrap();
    fixture.commit_seed("Change ordinary Git files for refresh");
    new_main = String::from_utf8_lossy(&git(&fixture.seed, ["rev-parse", "main"]).stdout)
        .trim()
        .to_owned();
    std::fs::write(fixture.shared.join("README.md"), "active tracked overlay\n").unwrap();
    std::fs::write(
        fixture.shared.join("unrelated.txt"),
        "active untracked overlay\n",
    )
    .unwrap();
    std::fs::write(
        fixture.shared.join("refresh-overlay-dir/local.txt"),
        "active directory overlay\n",
    )
    .unwrap();

    let dry = workspace(&fixture.shared, ["refresh", "--dry-run"]);
    assert_eq!(json(&dry)["status"], "dry_run");
    let refreshed = workspace(&fixture.shared, ["refresh"]);
    assert_eq!(json(&refreshed)["status"], "updated");
    assert_eq!(
        std::fs::read_to_string(fixture.shared.join("README.md")).unwrap(),
        "active tracked overlay\n"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.shared.join("unrelated.txt")).unwrap(),
        "active untracked overlay\n"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.shared.join("refresh-update.txt")).unwrap(),
        "new update\n"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.shared.join("refresh-added.txt")).unwrap(),
        "new file\n"
    );
    assert!(!fixture.shared.join("refresh-delete.txt").exists());
    assert_eq!(
        std::fs::read_to_string(fixture.shared.join("refresh-file-to-dir/new.txt")).unwrap(),
        "new child\n"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.shared.join("refresh-dir-to-file")).unwrap(),
        "new file\n"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.shared.join("refresh-overlay-dir/local.txt")).unwrap(),
        "active directory overlay\n"
    );
    assert!(fixture.shared.join("refresh-overlay-dir").is_dir());
    assert!(
        json(&refreshed)["materialized_git_paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path == "refresh-added.txt")
    );
    assert_eq!(
        String::from_utf8_lossy(&git(&fixture.shared, ["rev-parse", "main"]).stdout).trim(),
        new_main
    );
}

#[cfg(unix)]
#[test]
fn refresh_rejects_new_storage_metadata_below_a_symlink() {
    let fixture = managed_fixture();
    let old_oid = String::from_utf8_lossy(&git(&fixture.shared, ["rev-parse", "main"]).stdout)
        .trim()
        .to_owned();
    let outside = fixture.root.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, fixture.shared.join("linked")).unwrap();

    std::fs::create_dir(fixture.seed.join("linked")).unwrap();
    std::fs::write(
        fixture.seed.join("linked/payload.dvc"),
        "outs:\n- path: payload\n  md5: d41d8cd98f00b204e9800998ecf8427e\n  size: 0\n",
    )
    .unwrap();
    fixture.commit_seed("Add incoming storage metadata");

    let rejected = workspace_unchecked(&fixture.shared, ["refresh"]);
    assert_eq!(rejected.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("symlink"));
    assert_eq!(
        String::from_utf8_lossy(&git(&fixture.shared, ["rev-parse", "main"]).stdout).trim(),
        old_oid
    );
    assert!(std::fs::read_dir(outside).unwrap().next().is_none());
}

#[test]
fn rejects_unmanaged_large_files_and_nested_gitlinks() {
    let fixture = managed_fixture();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "unsafe-artifacts",
            "--title",
            "Unsafe artifacts",
            "--purpose",
            "Exercise publication refusals.",
            "--timestamp",
            "20260829-170700",
        ],
    );
    let task = fixture.shared.join("20260829-170700-unsafe-artifacts");
    document_task(&task);
    std::fs::write(task.join("large.bin"), vec![0_u8; 10_485_761]).unwrap();
    let large = workspace_unchecked(&task, ["plan"]);
    assert_eq!(large.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&large.stderr).contains("[s3] is not configured"));

    let explicitly_git = workspace(
        &task,
        [
            "storage",
            "set",
            "20260829-170700-unsafe-artifacts/large.bin",
            "--to",
            "git",
            "--reason",
            "The user requires this artifact in Git.",
        ],
    );
    assert_eq!(json(&explicitly_git)["remote_writes"], false);
    let allowed = workspace(&task, ["plan"]);
    assert_eq!(json(&allowed)["status"], "dry_run");

    std::fs::remove_file(task.join("large.bin")).unwrap();
    std::fs::remove_file(task.join("large.bin.workspace-mgr-storage.toml")).unwrap();
    let nested = task.join("nested");
    command(&task, "git", ["init", nested.to_str().unwrap()]);
    configure_git(&nested);
    std::fs::write(nested.join("README.md"), "nested repository\n").unwrap();
    git(&nested, ["add", "README.md"]);
    git(&nested, ["commit", "-m", "Nested commit"]);
    let gitlink = workspace_unchecked(&task, ["plan"]);
    assert_eq!(gitlink.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&gitlink.stderr).contains("gitlink"));
}

#[test]
fn refresh_refuses_staged_changes_and_non_fast_forwards() {
    let fixture = managed_fixture();
    std::fs::write(fixture.shared.join("README.md"), "staged overlay\n").unwrap();
    git(&fixture.shared, ["add", "README.md"]);
    let staged = workspace_unchecked(&fixture.shared, ["refresh"]);
    assert_eq!(staged.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&staged.stderr).contains("staged changes"));
    git(&fixture.shared, ["restore", "--staged", "README.md"]);

    let tree =
        String::from_utf8_lossy(&git(&fixture.seed, ["show", "-s", "--format=%T", "main"]).stdout)
            .trim()
            .to_owned();
    let divergent = String::from_utf8_lossy(
        &git(
            &fixture.seed,
            ["commit-tree", &tree, "-m", "Divergent root"],
        )
        .stdout,
    )
    .trim()
    .to_owned();
    git(
        &fixture.seed,
        [
            "push",
            "--force",
            "origin",
            &format!("{divergent}:refs/heads/main"),
        ],
    );
    let non_fast_forward = workspace_unchecked(&fixture.shared, ["refresh"]);
    assert_eq!(non_fast_forward.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&non_fast_forward.stderr).contains("cannot fast-forward"));
}

#[test]
fn repository_mutations_share_one_cross_command_lock() {
    let fixture = managed_fixture();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "operation-lock",
            "--title",
            "Operation lock",
            "--purpose",
            "Prevent cross-command repository races.",
            "--timestamp",
            "20260829-170850",
        ],
    );
    let task = fixture.shared.join("20260829-170850-operation-lock");
    let common_dir =
        String::from_utf8_lossy(&git(&fixture.shared, ["rev-parse", "--git-common-dir"]).stdout)
            .trim()
            .to_owned();
    let common_dir = {
        let path = std::path::PathBuf::from(common_dir);
        if path.is_absolute() {
            path
        } else {
            fixture.shared.join(path)
        }
    };
    let lock_path = common_dir.join("workspace-mgr/repository.lock");
    std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .unwrap();
    lock.try_lock_exclusive().unwrap();

    let publish = workspace_unchecked(&task, ["publish", "-m", "Must wait for lock"]);
    assert_eq!(publish.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&publish.stderr).contains("repository operation"));
    let set = workspace_unchecked(
        &task,
        [
            "storage",
            "set",
            "20260829-170850-operation-lock/README.md",
            "--to",
            "git",
            "--reason",
            "Must also honor the lock.",
        ],
    );
    assert_eq!(set.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&set.stderr).contains("repository operation"));
}

#[test]
fn publication_requires_a_message_and_task_readme_before_mutation() {
    let fixture = managed_fixture();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "required-metadata",
            "--title",
            "Required metadata",
            "--purpose",
            "Exercise early publication guards.",
            "--timestamp",
            "20260829-170900",
        ],
    );
    let task = fixture.shared.join("20260829-170900-required-metadata");
    let missing_message = workspace_unchecked(&task, ["publish"]);
    assert_eq!(missing_message.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&missing_message.stderr).contains("--message"));
    assert!(
        git_unchecked(
            &fixture.shared,
            [
                "ls-remote",
                "--exit-code",
                "origin",
                "refs/heads/codex/required-metadata"
            ]
        )
        .status
        .code()
        .is_some_and(|code| code != 0)
    );

    std::fs::remove_file(task.join("README.md")).unwrap();
    let missing_readme = workspace_unchecked(&task, ["plan"]);
    assert_eq!(missing_readme.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&missing_readme.stderr).contains("README is required"));
}

#[test]
fn publication_requires_the_task_to_document_its_own_content() {
    let fixture = managed_fixture();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "undocumented",
            "--title",
            "Undocumented",
            "--purpose",
            "Verify that published content carries a record.",
            "--timestamp",
            "20260829-170910",
        ],
    );
    let task_id = "20260829-170910-undocumented";
    let task = fixture.shared.join(task_id);
    let scaffold = workspace(&task, ["publish", "-m", "Publish the initial scaffold"]);
    let scaffold = json(&scaffold);
    assert_eq!(scaffold["status"], "pushed");
    assert!(scaffold["warnings"].is_null());
    let scaffold_oid = scaffold["remote_oid"].as_str().unwrap().to_owned();

    std::fs::create_dir(task.join("tools")).unwrap();
    std::fs::write(task.join("tools/run.py"), "print('analysis')\n").unwrap();
    let refused = workspace_unchecked(&task, ["plan"]);
    assert_eq!(refused.status.code(), Some(2));
    let message = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(
        message.contains("publishes content but documents nothing"),
        "{message}"
    );
    assert!(message.contains(task_id), "{message}");

    // The pristine-README test reads the scaffold's own fixed directory map, so
    // editing the mutable task title cannot retire the guard for this task.
    let manifest_path = task.join(".workspace-mgr-task.toml");
    let manifest = std::fs::read_to_string(&manifest_path).unwrap();
    std::fs::write(
        &manifest_path,
        manifest.replace("title = \"Undocumented\"", "title = \"Undocumented v2\""),
    )
    .unwrap();
    let retitled = workspace_unchecked(&task, ["plan"]);
    assert_eq!(retitled.status.code(), Some(2));
    let retitled = String::from_utf8_lossy(&retitled.stderr).into_owned();
    assert!(
        retitled.contains("publishes content but documents nothing"),
        "{retitled}"
    );
    std::fs::write(&manifest_path, &manifest).unwrap();

    let refused_publish = workspace_unchecked(&task, ["publish", "-m", "Publish the tool"]);
    assert_eq!(refused_publish.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(
            &git(&fixture.shared, ["rev-parse", "origin/codex/undocumented"]).stdout
        )
        .trim(),
        scaffold_oid,
        "a refused publication must leave the remote branch where the scaffold left it"
    );
    assert!(
        git_unchecked(
            &fixture.shared,
            [
                "cat-file",
                "-e",
                &format!("origin/codex/undocumented:{task_id}/tools/run.py")
            ]
        )
        .status
        .code()
        .is_some_and(|code| code != 0)
    );

    std::fs::create_dir(task.join("notes")).unwrap();
    std::fs::write(
        task.join("notes/process.md"),
        "# Process\n\nRan the analysis with `tools/run.py`.\n",
    )
    .unwrap();
    let published = workspace(&task, ["publish", "-m", "Publish the tool and its record"]);
    let published = json(&published);
    assert_eq!(published["status"], "pushed");
    assert!(published["warnings"].is_null());
    let commit = published["commit_oid"].as_str().unwrap();
    let recorded = git(
        &fixture.shared,
        ["show", &format!("{commit}:{task_id}/notes/process.md")],
    );
    assert!(String::from_utf8_lossy(&recorded.stdout).contains("Ran the analysis"));
}

#[test]
fn a_task_whose_readme_was_edited_documents_itself() {
    let fixture = managed_fixture();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "legacy-readme",
            "--title",
            "Legacy readme",
            "--purpose",
            "Verify that an earlier release's task still publishes.",
            "--timestamp",
            "20260829-170915",
        ],
    );
    let task_id = "20260829-170915-legacy-readme";
    let task = fixture.shared.join(task_id);
    std::fs::write(
        task.join("README.md"),
        "# Legacy readme\n\nVerify that an earlier release's task still publishes.\n\n## Directory map\n\n- `README.md` describes this task and its retained outputs.\n- `.workspace-mgr-task.toml` declares the task scope and target branch.\n",
    )
    .unwrap();
    std::fs::write(task.join("result.txt"), "carried over\n").unwrap();

    let published = workspace(&task, ["publish", "-m", "Publish a pre-upgrade task"]);
    assert_eq!(json(&published)["status"], "pushed");
}

#[test]
fn publication_warns_when_content_changes_without_the_task_record() {
    let fixture = managed_fixture();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "stale-record",
            "--title",
            "Stale record",
            "--purpose",
            "Verify the task-record warning and its suppression rules.",
            "--timestamp",
            "20260829-170920",
        ],
    );
    let task_id = "20260829-170920-stale-record";
    let task = fixture.shared.join(task_id);
    let record = task.join("decisions.md");
    std::fs::write(&record, "# Decisions\n\nStarted from the scaffold.\n").unwrap();
    workspace(&task, ["publish", "-m", "Publish the initial record"]);

    std::fs::write(task.join("result.csv"), "value\n1\n").unwrap();
    let stale = workspace(&task, ["plan"]);
    let warnings = json(&stale)["warnings"].as_array().unwrap().clone();
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0]["code"], "task-record-unchanged");
    let message = warnings[0]["message"].as_str().unwrap();
    assert!(message.contains(task_id), "{message}");
    assert!(
        message.contains("ignore this warning otherwise"),
        "{message}"
    );

    std::fs::write(
        &record,
        "# Decisions\n\nStarted from the scaffold.\nKept the result as CSV for review.\n",
    )
    .unwrap();
    let recorded = workspace(&task, ["plan"]);
    assert!(json(&recorded)["warnings"].is_null());
    workspace(
        &task,
        ["publish", "-m", "Publish the result and its record"],
    );

    let readme = std::fs::read_to_string(task.join("README.md")).unwrap();
    std::fs::write(
        task.join("README.md"),
        format!("{readme}\nThe result is `result.csv`.\n"),
    )
    .unwrap();
    let housekeeping = workspace(&task, ["plan"]);
    assert_eq!(json(&housekeeping)["status"], "dry_run");
    assert!(json(&housekeeping)["warnings"].is_null());

    let unchanged = workspace(&task, ["publish", "-m", "Publish the README update"]);
    assert_eq!(json(&unchanged)["status"], "pushed");
    let clean = workspace(&task, ["plan"]);
    assert_eq!(json(&clean)["status"], "no_changes");
    assert!(json(&clean)["warnings"].is_null());
}

#[cfg(unix)]
#[test]
fn publication_refuses_symbolic_links_that_leave_the_repository() {
    let fixture = managed_fixture();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "escaping-symlink",
            "--title",
            "Escaping symlink",
            "--purpose",
            "Refuse work kept outside the repository.",
            "--timestamp",
            "20260829-170930",
        ],
    );
    let task_id = "20260829-170930-escaping-symlink";
    let task = fixture.shared.join(task_id);
    document_task(&task);
    let outside = fixture.root.join("outside-work");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("data.txt"), "kept outside the repository\n").unwrap();

    let link = task.join("scratch");
    for target in [
        outside.clone(),
        fixture.root.join("outside-work-that-never-existed"),
        std::path::PathBuf::from("../../outside-work"),
    ] {
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let refused = workspace_unchecked(&task, ["plan"]);
        assert_eq!(
            refused.status.code(),
            Some(2),
            "{} was not refused",
            target.display()
        );
        let message = String::from_utf8_lossy(&refused.stderr).into_owned();
        assert!(message.contains("outside the repository"), "{message}");
        assert!(message.contains("scratch"), "{message}");
        std::fs::remove_file(&link).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn publication_allows_a_symbolic_link_inside_the_task() {
    let fixture = managed_fixture();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "internal-symlink",
            "--title",
            "Internal symlink",
            "--purpose",
            "Keep ordinary in-tree links publishable.",
            "--timestamp",
            "20260829-170935",
        ],
    );
    let task_id = "20260829-170935-internal-symlink";
    let task = fixture.shared.join(task_id);
    document_task(&task);
    std::fs::create_dir(task.join("results")).unwrap();
    std::fs::write(task.join("results/run-2.csv"), "value\n2\n").unwrap();
    std::os::unix::fs::symlink("results/run-2.csv", task.join("latest.csv")).unwrap();

    let published = workspace(&task, ["publish", "-m", "Publish an in-tree link"]);
    let published = json(&published);
    assert_eq!(published["status"], "pushed");
    let commit = published["commit_oid"].as_str().unwrap();
    let staged = git(
        &fixture.shared,
        ["ls-tree", commit, &format!("{task_id}/latest.csv")],
    );
    assert!(String::from_utf8_lossy(&staged.stdout).contains("120000"));
}

#[test]
fn plan_reports_the_ignored_paths_inside_the_task() {
    let fixture = managed_fixture();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "ignored-paths",
            "--title",
            "Ignored paths",
            "--purpose",
            "Expose ignored task content for review.",
            "--timestamp",
            "20260829-170940",
        ],
    );
    let task_id = "20260829-170940-ignored-paths";
    let task = fixture.shared.join(task_id);
    document_task(&task);
    std::fs::write(task.join(".gitignore"), ".venv/\n").unwrap();
    std::fs::create_dir(task.join(".venv")).unwrap();
    std::fs::write(task.join(".venv/pyvenv.cfg"), "home = /usr\n").unwrap();
    std::fs::write(task.join("result.txt"), "reviewed output\n").unwrap();

    let plan = workspace(&task, ["plan"]);
    let plan = json(&plan);
    let ignored = plan["ignored_paths"].as_array().unwrap();
    // Containment rather than equality: a contributor's own global Git ignore
    // rules may legitimately add an entry, and that is not this test's subject.
    assert!(
        ignored.contains(&serde_json::json!(format!("{task_id}/.venv/"))),
        "{ignored:?}"
    );
    assert_eq!(
        plan["ignored_entries"].as_u64().unwrap(),
        ignored.len() as u64
    );
    assert!(
        !plan["changed_paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path.as_str().unwrap().contains("/.venv/"))
    );

    // The list is a sample the agent reads at every turn end; the count stays
    // exact. A pattern rule yields one entry per file, so it must stay bounded.
    std::fs::write(task.join(".gitignore"), ".venv/\n*.log\n").unwrap();
    std::fs::create_dir(task.join("runs")).unwrap();
    std::fs::write(task.join("runs/index.txt"), "run index\n").unwrap();
    for run in 0..80 {
        std::fs::write(task.join(format!("runs/run-{run:03}.log")), "log\n").unwrap();
    }
    let capped = workspace(&task, ["plan"]);
    let capped = json(&capped);
    let listed = capped["ignored_paths"].as_array().unwrap().len();
    assert_eq!(listed, 50);
    assert!(
        capped["ignored_entries"].as_u64().unwrap() >= 81,
        "{}",
        capped["ignored_entries"]
    );
}

#[test]
fn an_authorized_extra_scope_publishes_without_a_task_record() {
    let fixture = managed_fixture();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "extra-only",
            "--title",
            "Extra only",
            "--purpose",
            "Publish only a user-authorized path outside the task.",
            "--timestamp",
            "20260829-170945",
        ],
    );
    let task_id = "20260829-170945-extra-only";
    let task = fixture.shared.join(task_id);
    workspace(&task, ["publish", "-m", "Publish the initial scaffold"]);

    // The task directory still holds nothing but the creation scaffold. The
    // documentation guard judges the task's own content, so an authorized
    // change to a shared root path is not held to it.
    std::fs::write(fixture.shared.join("authorized.txt"), "authorized\n").unwrap();
    let published = workspace(
        &task,
        [
            "publish",
            "-m",
            "Publish the authorized root change",
            "--include",
            "authorized.txt",
            "--scope-note",
            "The user explicitly requested this root file.",
        ],
    );
    let published = json(&published);
    assert_eq!(published["status"], "pushed");
    assert_eq!(
        published["changed_paths"],
        serde_json::json!(["authorized.txt"])
    );
    assert!(published["warnings"].is_null());
}

#[test]
fn a_task_may_retire_its_content_but_not_its_last_record() {
    let fixture = managed_fixture();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "retirement",
            "--title",
            "Retirement",
            "--purpose",
            "Retract a published result and its record.",
            "--timestamp",
            "20260829-170950",
        ],
    );
    let task_id = "20260829-170950-retirement";
    let task = fixture.shared.join(task_id);
    std::fs::write(task.join("notes.md"), "# Notes\n\nThe result is here.\n").unwrap();
    std::fs::write(task.join("result.csv"), "value\n1\n").unwrap();
    workspace(&task, ["publish", "-m", "Publish the result and its notes"]);

    // Removing the record while still publishing content names the real
    // problem rather than claiming the task published an undocumented result.
    std::fs::remove_file(task.join("notes.md")).unwrap();
    std::fs::write(task.join("result.csv"), "value\n2\n").unwrap();
    let refused = workspace_unchecked(&task, ["plan"]);
    assert_eq!(refused.status.code(), Some(2));
    let message = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(
        message.contains("removes the last of its own documentation"),
        "{message}"
    );
    assert!(
        message.contains("a published record is durable"),
        "{message}"
    );

    // A publication that only retires content cannot be an undocumented
    // addition, so the cleanup itself is publishable.
    std::fs::remove_file(task.join("result.csv")).unwrap();
    let retired = workspace(&task, ["publish", "-m", "Retract the result and its notes"]);
    let retired = json(&retired);
    assert_eq!(retired["status"], "pushed");
    assert_eq!(
        retired["changed_paths"],
        serde_json::json!([
            format!("{task_id}/notes.md"),
            format!("{task_id}/result.csv")
        ])
    );
    let commit = retired["commit_oid"].as_str().unwrap();
    assert!(
        git_unchecked(
            &fixture.shared,
            ["cat-file", "-e", &format!("{commit}:{task_id}/result.csv")],
        )
        .status
        .code()
        .is_some_and(|code| code != 0)
    );
}

#[test]
fn infrastructure_publication_is_not_held_to_the_task_record() {
    let fixture = managed_fixture();
    let created = workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "shared-tool",
            "--kind",
            "infrastructure",
            "--title",
            "Shared tool",
            "--purpose",
            "Change one repository-wide tool.",
            "--scope",
            "shared-tool.py",
            "--scope-note",
            "The user requested this repository-wide change.",
        ],
    );
    let worktree = std::path::PathBuf::from(json(&created)["path"].as_str().unwrap());

    // An infrastructure task has no repository task directory, so neither the
    // refusal nor the warning applies to it.
    std::fs::write(worktree.join("shared-tool.py"), "print('shared')\n").unwrap();
    let published = workspace(&worktree, ["publish", "-m", "Publish the shared tool"]);
    let published = json(&published);
    assert_eq!(published["status"], "pushed");
    assert_eq!(
        published["changed_paths"],
        serde_json::json!(["shared-tool.py"])
    );
    assert!(published["warnings"].is_null());
}

#[cfg(unix)]
#[test]
fn an_escaping_symlink_refusal_names_the_workplace_each_task_kind_has() {
    let fixture = managed_fixture();
    let created = workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "shared-area",
            "--kind",
            "infrastructure",
            "--title",
            "Shared area",
            "--purpose",
            "Change one repository-wide directory.",
            "--scope",
            "shared-area",
            "--scope-note",
            "The user requested this repository-wide change.",
        ],
    );
    let worktree = std::path::PathBuf::from(json(&created)["path"].as_str().unwrap());
    std::fs::create_dir(worktree.join("shared-area")).unwrap();
    std::fs::write(worktree.join("shared-area/config.yaml"), "v1\n").unwrap();
    std::os::unix::fs::symlink(
        fixture.root.join("outside-work"),
        worktree.join("shared-area/scratch"),
    )
    .unwrap();

    let refused = workspace_unchecked(&worktree, ["plan"]);
    assert_eq!(refused.status.code(), Some(2));
    let message = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(message.contains("outside the repository"), "{message}");
    assert!(
        message.contains("keep the work inside a declared scope"),
        "{message}"
    );
    assert!(!message.contains("task directory"), "{message}");
}

#[test]
fn publication_refuses_content_hidden_only_by_a_machine_local_ignore_rule() {
    let fixture = managed_fixture();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "machine-local-ignore",
            "--title",
            "Machine local ignore",
            "--purpose",
            "Refuse content only an untracked rule hides.",
            "--timestamp",
            "20260829-171000",
        ],
    );
    let task_id = "20260829-171000-machine-local-ignore";
    let task = fixture.shared.join(task_id);
    document_task(&task);
    std::fs::write(task.join("search_log1.txt"), "per-run log\n").unwrap();
    std::fs::write(
        fixture.shared.join(".git/info/exclude"),
        format!("{task_id}/search_log1.txt\n"),
    )
    .unwrap();

    for operation in [
        vec!["plan"],
        vec!["publish", "-m", "Publish the machine-local fixture"],
    ] {
        let refused = workspace_unchecked(&task, operation.clone());
        assert_eq!(
            refused.status.code(),
            Some(2),
            "{operation:?} was not refused"
        );
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(
            stderr.contains("only an ignore rule this publication does not carry hides"),
            "{stderr}"
        );
        assert!(
            stderr.contains(&format!("\"{task_id}/search_log1.txt\"")),
            "the path must be named: {stderr}"
        );
        assert!(
            stderr.contains(&format!("by rule \"{task_id}/search_log1.txt\"")),
            "the rule must be named: {stderr}"
        );
        assert!(
            stderr.contains("in \".git/info/exclude\""),
            "the source must be named: {stderr}"
        );
        assert!(
            stderr.contains(&format!("`{task_id}/.gitignore`"))
                && stderr.contains("`.workspace-mgr/repository.gitignore`"),
            "both fixes must be offered: {stderr}"
        );
    }
    // Nothing was published while the refusal stood.
    assert!(
        git_unchecked(
            &fixture.shared,
            [
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/remotes/origin/codex/machine-local-ignore"
            ]
        )
        .stdout
        .is_empty()
    );

    // The documented fix: carry the rule in the task's own ignore file.
    std::fs::write(fixture.shared.join(".git/info/exclude"), "").unwrap();
    std::fs::write(task.join(".gitignore"), "search_log1.txt\n").unwrap();

    let plan = workspace(&task, ["plan"]);
    let plan = json(&plan);
    assert!(
        plan["ignored_paths"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(format!("{task_id}/search_log1.txt"))),
        "{plan}"
    );
    let published = workspace(
        &task,
        ["publish", "-m", "Publish with a carried ignore rule"],
    );
    assert_eq!(json(&published)["status"], "pushed");
    let commit = json(&published)["commit_oid"].as_str().unwrap().to_owned();
    let tracked = git(
        &fixture.shared,
        ["ls-tree", "-r", "--name-only", &commit, "--", task_id],
    );
    let tracked = String::from_utf8_lossy(&tracked.stdout);
    assert!(
        tracked.contains(&format!("{task_id}/.gitignore")),
        "{tracked}"
    );
    assert!(!tracked.contains("search_log1.txt"), "{tracked}");
}

#[test]
fn a_tracked_ignore_rule_outranks_a_machine_local_one() {
    let fixture = managed_fixture();
    let excludes = fixture.root.join("global-excludes");
    std::fs::write(&excludes, ".DS_Store\n*.machine-local\n").unwrap();
    git(
        &fixture.shared,
        ["config", "core.excludesFile", excludes.to_str().unwrap()],
    );
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "global-excludes",
            "--title",
            "Global excludes",
            "--purpose",
            "Check that a tracked rule outranks a global one.",
            "--timestamp",
            "20260829-171100",
        ],
    );
    let task_id = "20260829-171100-global-excludes";
    let task = fixture.shared.join(task_id);
    document_task(&task);
    // The product's own root rules are tracked, and Git resolves an in-tree
    // ignore file before the global excludes, so the everyday case is silent.
    std::fs::write(task.join(".DS_Store"), "finder junk\n").unwrap();

    let plan = workspace(&task, ["plan"]);
    assert!(
        json(&plan)["ignored_paths"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(format!("{task_id}/.DS_Store"))),
        "{}",
        json(&plan)
    );

    // A rule that only the global excludes file carries is refused, and the
    // message names that file.
    std::fs::write(task.join("bulk.machine-local"), "hidden only by me\n").unwrap();
    let refused = workspace_unchecked(&task, ["plan"]);
    assert_eq!(refused.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("\"*.machine-local\""), "{stderr}");
    assert!(stderr.contains(excludes.to_str().unwrap()), "{stderr}");
    assert!(
        !stderr.contains(".DS_Store"),
        "the tracked rule must not fire: {stderr}"
    );
}

#[test]
fn a_directory_whose_whole_content_is_ignored_is_still_resolved_to_its_rule() {
    // `git status --ignored` collapses such a directory into one entry, and a
    // file-level rule does not match the directory, so the entry arrives with
    // no rule at all. That is the common shape of the problem this refusal
    // exists for: a tree of per-run by-products hidden by one personal rule.
    let fixture = managed_fixture();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "collapsed-ignore",
            "--title",
            "Collapsed ignore",
            "--purpose",
            "Resolve a wholly ignored directory to its rule.",
            "--timestamp",
            "20260829-171300",
        ],
    );
    let task_id = "20260829-171300-collapsed-ignore";
    let task = fixture.shared.join(task_id);
    document_task(&task);
    let results = task.join("results");
    std::fs::create_dir(&results).unwrap();
    for index in 1..=3 {
        std::fs::write(results.join(format!("run-{index}.log")), "per-run log\n").unwrap();
    }
    std::fs::write(fixture.shared.join(".git/info/exclude"), "*.log\n").unwrap();
    // Nothing else lives beside them, so Git reports exactly one entry.
    let listed = git(
        &fixture.shared,
        ["status", "--ignored", "--short", "--", task_id],
    );
    let listed = String::from_utf8_lossy(&listed.stdout).into_owned();
    assert!(
        listed.contains(&format!("!! {task_id}/results/")),
        "the fixture must exercise the collapsed listing: {listed}"
    );

    for operation in [
        vec!["plan"],
        vec!["publish", "-m", "Publish the collapsed fixture"],
    ] {
        let refused = workspace_unchecked(&task, operation.clone());
        assert_eq!(
            refused.status.code(),
            Some(2),
            "{operation:?} was not refused"
        );
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(
            stderr.contains(&format!("\"{task_id}/results/run-1.log\"")),
            "the hidden file must be named: {stderr}"
        );
        assert!(stderr.contains("by rule \"*.log\""), "{stderr}");
        assert!(stderr.contains("in \".git/info/exclude\""), "{stderr}");
    }

    // The outcome must not depend on an unrelated sibling existing: with one
    // non-ignored file beside them Git lists the three individually, and the
    // same refusal stands.
    std::fs::write(results.join("summary.md"), "kept\n").unwrap();
    assert_eq!(workspace_unchecked(&task, ["plan"]).status.code(), Some(2));

    // Carried by the task's own rule, the same content plans cleanly.
    std::fs::write(fixture.shared.join(".git/info/exclude"), "").unwrap();
    std::fs::write(task.join(".gitignore"), "results/*.log\n").unwrap();
    assert_eq!(json(&workspace(&task, ["plan"]))["status"], "dry_run");
}

#[test]
fn the_products_own_rules_never_refuse_the_first_publication_of_a_repository() {
    // Between `init` and the publication of the generated root file there is a
    // window the product cannot close itself, because no task publication can
    // carry a root path. The product's own fixed rules are carried by every
    // installation regardless, so they must never be read as machine-local.
    let fixture = GitFixture::new();
    fixture.clone_shared();
    workspace(&fixture.shared, ["init"]);
    assert!(
        git_unchecked(&fixture.shared, ["ls-files", "--", ".gitignore"])
            .stdout
            .is_empty(),
        "the fixture must exercise the uncommitted scaffold"
    );
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "bootstrap",
            "--title",
            "Bootstrap",
            "--purpose",
            "Publish the initial scaffold before the root ignore file is carried.",
            "--timestamp",
            "20260829-171400",
        ],
    );
    let task_id = "20260829-171400-bootstrap";
    let task = fixture.shared.join(task_id);
    // Nobody creates these on purpose; macOS and Python leave them behind.
    std::fs::write(task.join(".DS_Store"), "finder junk\n").unwrap();
    std::fs::create_dir(task.join("__pycache__")).unwrap();
    std::fs::write(task.join("__pycache__/tool.pyc"), "bytecode\n").unwrap();

    let plan = json(&workspace(&task, ["plan"]));

    assert_eq!(plan["status"], "dry_run");
    assert_eq!(plan["ignored_entries"], 2);
}

#[test]
fn an_unpublished_rule_in_a_tracked_ignore_file_is_not_carried() {
    // `git check-ignore` resolves against the work tree, so a rule written into
    // a tracked ignore file and never staged decides locally while reaching no
    // other clone. Checking only that the path is tracked would make the
    // refusal's own remedy silence it without publishing anything.
    let fixture = managed_fixture();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "unpublished-rule",
            "--title",
            "Unpublished rule",
            "--purpose",
            "Refuse a rule the publication does not carry.",
            "--timestamp",
            "20260829-171500",
        ],
    );
    let task_id = "20260829-171500-unpublished-rule";
    let task = fixture.shared.join(task_id);
    document_task(&task);
    std::fs::write(task.join("scratch.junk"), "per-run junk\n").unwrap();
    let root_ignore = fixture.shared.join(".gitignore");
    let generated = std::fs::read_to_string(&root_ignore).unwrap();
    std::fs::write(&root_ignore, format!("{generated}*.junk\n")).unwrap();

    let refused = workspace_unchecked(&task, ["plan"]);

    assert_eq!(refused.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(
        stderr.contains(&format!("\"{task_id}/scratch.junk\"")),
        "{stderr}"
    );
    assert!(stderr.contains("by rule \"*.junk\""), "{stderr}");
    assert!(stderr.contains("in \".gitignore\""), "{stderr}");

    // The task's own rule is staged by this very publication, so it is carried.
    std::fs::write(&root_ignore, generated).unwrap();
    std::fs::write(task.join(".gitignore"), "*.junk\n").unwrap();
    let plan = json(&workspace(&task, ["plan"]));
    assert_eq!(plan["status"], "dry_run");
    assert!(
        plan["ignored_paths"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(format!("{task_id}/scratch.junk"))),
        "{plan}"
    );
}

#[test]
fn a_bulk_publication_warns_above_the_file_threshold_and_is_silent_below() {
    let fixture = managed_fixture();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "bulk-publication",
            "--title",
            "Bulk publication",
            "--purpose",
            "Check the bulk publication threshold.",
            "--timestamp",
            "20260829-171200",
        ],
    );
    let task_id = "20260829-171200-bulk-publication";
    let task = fixture.shared.join(task_id);
    document_task(&task);
    let results = task.join("results");
    std::fs::create_dir(&results).unwrap();
    // 199 results plus the task's record are exactly 200 new content files.
    // The README and the manifest are housekeeping and are not counted.
    for index in 0..199 {
        std::fs::write(results.join(format!("run-{index:03}.json")), "{}\n").unwrap();
    }

    let quiet = json(&workspace(&task, ["plan"]));
    assert_eq!(quiet["changed_paths"].as_array().unwrap().len(), 202);
    assert!(
        !warning_codes(&quiet).contains(&"bulk-publication"),
        "the threshold itself is silent: {quiet}"
    );

    std::fs::write(results.join("run-199.json"), "{}\n").unwrap();
    let loud = json(&workspace(&task, ["plan"]));
    assert!(warning_codes(&loud).contains(&"bulk-publication"), "{loud}");
    let message = loud["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|warning| warning["code"] == "bulk-publication")
        .unwrap()["message"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(message.contains("201 new files"), "{message}");
    assert!(message.contains("rather than a refusal"), "{message}");

    // It is a check, not a refusal.
    assert_eq!(
        json(&workspace(
            &task,
            ["publish", "-m", "Publish the bulk results"]
        ))["status"],
        "pushed"
    );
    // A second publication that adds nothing new is silent again.
    std::fs::write(task.join("record.md"), "# Record\n\nOne edit.\n").unwrap();
    let repeat = json(&workspace(&task, ["plan"]));
    assert!(
        !warning_codes(&repeat).contains(&"bulk-publication"),
        "an edit to published content is not new bulk: {repeat}"
    );

    // A rename moves every published file at once. Nothing arrives, so nothing
    // is bulk; counting the moved files would invite the agent to ignore or
    // untrack content it already deliberately published.
    assert_eq!(
        json(&workspace(
            &task,
            ["task", "rename", "bulk-publication-renamed"]
        ))["status"],
        "renamed"
    );
    let renamed = fixture
        .shared
        .join("20260829-171200-bulk-publication-renamed");
    let after_rename = json(&workspace(&renamed, ["plan"]));
    assert!(
        !warning_codes(&after_rename).contains(&"bulk-publication"),
        "a rename adds no content: {after_rename}"
    );
}

#[test]
fn a_bulk_publication_warns_above_the_byte_threshold() {
    let fixture = managed_fixture();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "bulk-bytes",
            "--title",
            "Bulk bytes",
            "--purpose",
            "Check the bulk publication byte threshold.",
            "--timestamp",
            "20260829-171300",
        ],
    );
    let task_id = "20260829-171300-bulk-bytes";
    let task = fixture.shared.join(task_id);
    document_task(&task);
    // One sparse file above 256 MiB: the byte threshold must fire on its own,
    // with the file count far below its own threshold.
    let payload = task.join("intermediate.bin");
    std::fs::File::create(&payload)
        .unwrap()
        .set_len(268_435_457)
        .unwrap();
    workspace(
        &task,
        [
            "storage",
            "set",
            &format!("{task_id}/intermediate.bin"),
            "--to",
            "git",
            "--reason",
            "Keep the fixture in Git so size alone is under test",
        ],
    );

    let plan = json(&workspace(&task, ["plan"]));

    let message = plan["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|warning| warning["code"] == "bulk-publication")
        .unwrap_or_else(|| panic!("{plan}"))["message"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(message.contains("3 new files"), "{message}");
    // The threshold is stated in the unit the rest of the policy uses, with
    // the exact byte count beside it.
    assert!(
        message.contains("256 MiB (268435456 bytes) threshold"),
        "{message}"
    );
    let reported: u64 = message
        .split(" bytes of new content")
        .next()
        .unwrap()
        .rsplit(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(reported > 268_435_456, "{message}");
}

fn warning_codes(report: &serde_json::Value) -> Vec<&str> {
    report["warnings"]
        .as_array()
        .map(|warnings| {
            warnings
                .iter()
                .filter_map(|warning| warning["code"].as_str())
                .collect()
        })
        .unwrap_or_default()
}
