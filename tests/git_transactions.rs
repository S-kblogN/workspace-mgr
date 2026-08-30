mod common;

use common::*;

fn managed_fixture() -> GitFixture {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["init", "--profile", "shared-checkout"]);
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
fn legacy_manifest_and_command_line_remain_compatible() {
    let fixture = managed_fixture();
    let name = "20260829-170300-legacy";
    let task = fixture.shared.join(name);
    std::fs::create_dir(&task).unwrap();
    std::fs::write(task.join("README.md"), "legacy task\n").unwrap();
    std::fs::write(
        task.join(".chat-sync.json"),
        format!(
            "{{\n  \"version\": 1,\n  \"task_path\": \"{name}\",\n  \"branch\": \"codex/legacy\",\n  \"remote\": \"origin\",\n  \"base_branch\": \"main\",\n  \"shared_head\": \"main\",\n  \"additional_paths\": []\n}}\n"
        ),
    )
    .unwrap();
    let output = workspace(
        &task,
        [
            "--manifest",
            task.join(".chat-sync.json").to_str().unwrap(),
            "-m",
            "Publish legacy task",
        ],
    );
    assert_eq!(json(&output)["legacy_manifest"], true);
    assert_eq!(json(&output)["status"], "pushed");
}

#[test]
fn refresh_preserves_working_tree_overlays() {
    let fixture = managed_fixture();
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
    let new_main = json(&published)["commit_oid"].as_str().unwrap().to_owned();
    git(
        &fixture.remote,
        ["update-ref", "refs/heads/main", &new_main],
    );
    std::fs::write(fixture.shared.join("README.md"), "active tracked overlay\n").unwrap();
    std::fs::write(
        fixture.shared.join("unrelated.txt"),
        "active untracked overlay\n",
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
        String::from_utf8_lossy(&git(&fixture.shared, ["rev-parse", "main"]).stdout).trim(),
        new_main
    );
}

#[test]
fn rejects_unmanaged_large_files_and_nested_gitlinks() {
    let fixture = managed_fixture();
    let config_path = fixture.shared.join(".workspace-mgr.toml");
    let config = std::fs::read_to_string(&config_path)
        .unwrap()
        .replace("threshold_bytes = 10485760", "threshold_bytes = 1024");
    std::fs::write(&config_path, config).unwrap();

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
    std::fs::write(task.join("large.bin"), vec![0_u8; 2048]).unwrap();
    let large = workspace_unchecked(&task, ["plan"]);
    assert_eq!(large.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&large.stderr).contains("larger than 1024 bytes"));

    std::fs::remove_file(task.join("large.bin")).unwrap();
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
