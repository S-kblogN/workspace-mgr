mod common;

use common::*;
use std::path::PathBuf;

fn infrastructure_fixture() -> (GitFixture, PathBuf) {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["init"]);
    std::fs::write(fixture.seed.join("shared-policy.md"), "original policy\n").unwrap();
    fixture.commit_seed("Add workspace policy");
    fixture.clone_shared();
    let created = workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "discovery",
            "--kind",
            "infrastructure",
            "--title",
            "Private infrastructure discovery",
            "--purpose",
            "Exercise shared main selection.",
            "--scope",
            "shared-policy.md",
            "--scope-note",
            "The user requested this policy edit.",
        ],
    );
    let manifest = PathBuf::from(json(&created)["manifest"].as_str().unwrap());
    (fixture, manifest)
}

#[test]
fn relative_private_manifest_selects_shared_main_from_outside_checkout() {
    let (fixture, manifest) = infrastructure_fixture();
    let root = fixture.root.canonicalize().unwrap();
    let relative = manifest.strip_prefix(&root).unwrap();
    let status = workspace(
        &root,
        ["task", "status", "--manifest", relative.to_str().unwrap()],
    );
    assert_eq!(json(&status)["task_id"], "infra-discovery");
    assert_eq!(json(&status)["base_branch"], "main");
}

#[test]
fn unrelated_missing_and_broken_worktrees_do_not_block_private_task_resolution() {
    let (fixture, manifest) = infrastructure_fixture();
    let stale = fixture.root.join("stale-legacy");
    let broken = fixture.root.join("broken-legacy");
    git(
        &fixture.shared,
        [
            "worktree",
            "add",
            "-b",
            "legacy-stale",
            stale.to_str().unwrap(),
        ],
    );
    git(
        &fixture.shared,
        [
            "worktree",
            "add",
            "-b",
            "legacy-broken",
            broken.to_str().unwrap(),
        ],
    );
    std::fs::remove_dir_all(&stale).unwrap();
    std::fs::write(broken.join(".workspace-mgr.toml"), "invalid = [\n").unwrap();

    let status = workspace(
        &fixture.root,
        ["task", "status", "--manifest", manifest.to_str().unwrap()],
    );
    assert_eq!(json(&status)["task_id"], "infra-discovery");
    assert_eq!(json(&status)["base_branch"], "main");
    let registered =
        String::from_utf8(git(&fixture.shared, ["worktree", "list", "--porcelain"]).stdout)
            .unwrap();
    assert!(registered.contains(stale.to_str().unwrap()));
    assert!(registered.contains(broken.to_str().unwrap()));
    assert_eq!(
        std::fs::read_to_string(broken.join(".workspace-mgr.toml")).unwrap(),
        "invalid = [\n"
    );
}

#[test]
fn broken_shared_configuration_has_a_clear_resolution_error() {
    let (fixture, manifest) = infrastructure_fixture();
    let legacy = fixture.root.join("legacy");
    git(
        &fixture.shared,
        ["worktree", "add", "-b", "legacy", legacy.to_str().unwrap()],
    );
    std::fs::write(fixture.shared.join(".workspace-mgr.toml"), "invalid = [\n").unwrap();

    let status = workspace_unchecked(
        &fixture.root,
        ["task", "status", "--manifest", manifest.to_str().unwrap()],
    );
    assert_eq!(status.status.code(), Some(2));
    let error = String::from_utf8_lossy(&status.stderr);
    assert!(
        error.contains("exactly one valid shared checkout"),
        "{error}"
    );
    assert!(error.contains(".workspace-mgr.toml"), "{error}");
    assert!(error.contains("unavailable checkouts"), "{error}");
}

#[test]
fn two_valid_shared_main_worktrees_are_refused_as_ambiguous() {
    let (fixture, manifest) = infrastructure_fixture();
    let duplicate = fixture.root.join("duplicate-main");
    git(
        &fixture.shared,
        [
            "worktree",
            "add",
            "--force",
            duplicate.to_str().unwrap(),
            "main",
        ],
    );
    let status = workspace_unchecked(
        &fixture.root,
        ["task", "status", "--manifest", manifest.to_str().unwrap()],
    );
    assert_eq!(status.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&status.stderr).contains("exactly one valid shared checkout"));
    assert_eq!(
        String::from_utf8(git(&duplicate, ["branch", "--show-current"]).stdout)
            .unwrap()
            .trim(),
        "main"
    );
}
