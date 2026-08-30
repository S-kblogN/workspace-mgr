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
    let retained = task.join("retained.bin");
    std::fs::write(&retained, vec![1_u8; 512]).unwrap();
    workspace(&task, ["publish", "-m", "Publish small Git file"]);

    std::fs::write(&retained, vec![2_u8; 10_485_761]).unwrap();
    let status = workspace(
        &task,
        ["storage", "status", &format!("{task_id}/retained.bin")],
    );
    assert_eq!(json(&status)["placements"][0]["target"], "git");
    assert_eq!(
        json(&status)["placements"][0]["selected_by"],
        "published-history"
    );
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
        json(&plan)["storage"]["placement"]["would_place_in_s3"]
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
    assert_eq!(json(&status)["placements"][0]["selected_by"], "automatic");
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
    let plan = workspace(&task, ["plan"]);
    assert_eq!(json(&plan)["status"], "dry_run");
    assert!(
        json(&plan)["storage"]["placement"]["would_place_in_s3"]
            .as_array()
            .unwrap()
            .is_empty()
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
    assert!(String::from_utf8_lossy(&message.stdout).contains("Scope-Authorization"));
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
