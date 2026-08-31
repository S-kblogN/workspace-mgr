mod common;

use std::path::{Path, PathBuf};

use common::{GitFixture, git, git_unchecked, json, workspace, workspace_unchecked};

fn managed_fixture() -> GitFixture {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["init"]);
    fixture.commit_seed("Initialize managed workspace");
    fixture.clone_shared();
    fixture
}

fn create_task(fixture: &GitFixture, slug: &str, timestamp: &str) -> (String, PathBuf, String) {
    let task_id = format!("{timestamp}-{slug}");
    let branch = format!("codex/{slug}");
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            slug,
            "--title",
            "Rename test",
            "--purpose",
            "Verify complete task slug migration.",
            "--timestamp",
            timestamp,
        ],
    );
    (task_id.clone(), fixture.shared.join(task_id), branch)
}

fn remote_oid(repo: &Path, branch: &str) -> String {
    String::from_utf8(
        git(
            repo,
            [
                "ls-remote",
                "--heads",
                "origin",
                &format!("refs/heads/{branch}"),
            ],
        )
        .stdout,
    )
    .unwrap()
    .split_whitespace()
    .next()
    .unwrap()
    .to_owned()
}

fn tree_has(repo: &Path, oid: &str, path: &str) -> bool {
    git_unchecked(repo, ["cat-file", "-e", &format!("{oid}:{path}")])
        .status
        .success()
}

#[test]
fn unpublished_deliverable_rename_moves_the_workspace_and_preserves_identity() {
    let fixture = managed_fixture();
    let (task_id, old_task, branch) = create_task(&fixture, "initial-topic", "20260830-120000");
    let new_task_id = "20260830-120000-better-topic";
    let new_task = fixture.shared.join(new_task_id);
    std::fs::create_dir(old_task.join("nested")).unwrap();
    std::fs::write(old_task.join("nested/evidence.txt"), "retained\n").unwrap();
    let branch_before = String::from_utf8(git(&fixture.shared, ["rev-parse", &branch]).stdout)
        .unwrap()
        .trim()
        .to_owned();

    let preview = workspace(&old_task, ["task", "rename", "better-topic", "--dry-run"]);
    let preview = json(&preview);
    assert_eq!(preview["status"], "dry_run");
    assert_eq!(preview["task_id"], task_id);
    assert_eq!(preview["old_slug"], "initial-topic");
    assert_eq!(preview["new_slug"], "better-topic");
    assert_eq!(preview["branch"], branch);
    assert_eq!(preview["review"]["head_branch_unchanged"], true);
    assert!(old_task.is_dir());
    assert!(!new_task.exists());

    let renamed = workspace(&old_task, ["task", "rename", "better-topic"]);
    let renamed = json(&renamed);
    assert_eq!(renamed["status"], "renamed");
    assert_eq!(renamed["remote_writes"], false);
    assert!(!old_task.exists());
    assert_eq!(
        std::fs::read_to_string(new_task.join("nested/evidence.txt")).unwrap(),
        "retained\n"
    );
    let manifest = std::fs::read_to_string(new_task.join(".workspace-mgr-task.toml")).unwrap();
    assert!(manifest.contains("schema_version = 2"));
    assert!(manifest.contains(&format!("id = \"{task_id}\"")));
    assert!(manifest.contains("slug = \"better-topic\""));
    assert!(manifest.contains(&format!("path = \"{new_task_id}\"")));
    assert!(manifest.contains(&format!("branch = \"{branch}\"")));
    assert_eq!(
        String::from_utf8(git(&fixture.shared, ["rev-parse", &branch]).stdout)
            .unwrap()
            .trim(),
        branch_before
    );
    let status = json(&workspace(&new_task, ["task", "status"]));
    assert_eq!(status["task_id"], task_id);
    assert_eq!(status["slug"], "better-topic");
    assert_eq!(status["scopes"][0], new_task_id);
}

#[test]
fn published_deliverable_rename_removes_the_old_tree_on_next_publish() {
    let fixture = managed_fixture();
    let (task_id, old_task, branch) = create_task(&fixture, "draft-report", "20260830-120100");
    std::fs::write(old_task.join("report.md"), "first topic\n").unwrap();
    let first = json(&workspace(
        &old_task,
        ["publish", "-m", "Publish the first topic"],
    ));
    let first_oid = first["remote_oid"].as_str().unwrap();
    assert!(tree_has(
        &fixture.shared,
        first_oid,
        "20260830-120100-draft-report/report.md"
    ));

    workspace(&old_task, ["task", "rename", "final-analysis"]);
    let new_task = fixture.shared.join("20260830-120100-final-analysis");
    std::fs::write(new_task.join("report.md"), "final topic\n").unwrap();
    let plan = json(&workspace(&new_task, ["plan"]));
    assert_eq!(plan["status"], "dry_run");
    assert_eq!(plan["branch"], branch);
    assert!(
        plan["scopes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| { path == "20260830-120100-draft-report" })
    );
    assert!(
        plan["changed_paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| { path == "20260830-120100-final-analysis/report.md" })
    );
    assert!(
        plan["changed_paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| { path == "20260830-120100-draft-report/report.md" })
    );

    let second = json(&workspace(
        &new_task,
        ["publish", "-m", "Rename the task for its final topic"],
    ));
    let second_oid = second["remote_oid"].as_str().unwrap();
    assert_eq!(remote_oid(&fixture.shared, &branch), second_oid);
    assert!(!tree_has(
        &fixture.shared,
        second_oid,
        "20260830-120100-draft-report"
    ));
    assert!(tree_has(
        &fixture.shared,
        second_oid,
        "20260830-120100-final-analysis/report.md"
    ));
    let message =
        String::from_utf8(git(&fixture.shared, ["show", "-s", "--format=%B", second_oid]).stdout)
            .unwrap();
    assert!(message.contains(&format!("Workspace-Task: {task_id}")));
    assert_eq!(
        json(&workspace(&new_task, ["plan"]))["status"],
        "no_changes"
    );
}

#[test]
fn repeated_rename_before_publish_migrates_from_the_published_path_to_the_latest_path() {
    let fixture = managed_fixture();
    let (task_id, original_task, branch) = create_task(&fixture, "first-topic", "20260830-120150");
    std::fs::write(original_task.join("result.txt"), "stable result\n").unwrap();
    workspace(
        &original_task,
        ["publish", "-m", "Publish the original topic"],
    );

    workspace(&original_task, ["task", "rename", "middle-topic"]);
    let middle_task = fixture.shared.join("20260830-120150-middle-topic");
    workspace(&middle_task, ["task", "rename", "final-topic"]);
    let final_task = fixture.shared.join("20260830-120150-final-topic");
    assert!(!original_task.exists());
    assert!(!middle_task.exists());
    assert_eq!(
        std::fs::read_to_string(final_task.join("result.txt")).unwrap(),
        "stable result\n"
    );

    let plan = json(&workspace(&final_task, ["plan"]));
    assert_eq!(plan["status"], "dry_run");
    let scopes = plan["scopes"].as_array().unwrap();
    assert!(scopes.iter().any(|path| path == &task_id));
    assert!(
        scopes
            .iter()
            .any(|path| path == "20260830-120150-final-topic")
    );
    assert!(
        !scopes
            .iter()
            .any(|path| path == "20260830-120150-middle-topic")
    );

    let published = json(&workspace(
        &final_task,
        ["publish", "-m", "Publish the final topic"],
    ));
    let oid = published["remote_oid"].as_str().unwrap();
    assert_eq!(remote_oid(&fixture.shared, &branch), oid);
    assert!(!tree_has(&fixture.shared, oid, &task_id));
    assert!(tree_has(
        &fixture.shared,
        oid,
        "20260830-120150-final-topic/result.txt"
    ));
}

#[test]
fn rename_preserves_published_git_placement_across_the_path_change() {
    let fixture = managed_fixture();
    let (_, old_task, _) = create_task(&fixture, "small-file", "20260830-120200");
    std::fs::write(old_task.join("growing.bin"), vec![1_u8; 512]).unwrap();
    workspace(&old_task, ["publish", "-m", "Publish small Git content"]);

    workspace(&old_task, ["task", "rename", "large-file"]);
    let new_task = fixture.shared.join("20260830-120200-large-file");
    std::fs::write(new_task.join("growing.bin"), vec![2_u8; 10_485_761]).unwrap();
    let status = json(&workspace(
        &new_task,
        [
            "storage",
            "status",
            "20260830-120200-large-file/growing.bin",
        ],
    ));
    assert_eq!(status["placements"][0]["target"], "git");
    assert_eq!(status["placements"][0]["basis"], "published-history");
    let plan = json(&workspace(&new_task, ["plan"]));
    assert_eq!(plan["status"], "dry_run");
    assert!(
        plan["storage"]["placement"]["placed_in_s3"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn rename_clears_path_bound_s3_versions_for_republication_at_the_new_path() {
    let fixture = managed_fixture();
    let (_, old_task, _) = create_task(&fixture, "versioned-path", "20260830-120250");
    std::fs::write(
        old_task.join("artifact.bin.dvc"),
        "outs:\n- md5: abc\n  size: 3\n  path: artifact.bin\n  cloud:\n    storage:\n      version_id: old-path-version\n",
    )
    .unwrap();

    workspace(&old_task, ["task", "rename", "new-versioned-path"]);

    let pointer = fixture
        .shared
        .join("20260830-120250-new-versioned-path/artifact.bin.dvc");
    let contents = std::fs::read_to_string(pointer).unwrap();
    assert!(contents.contains("md5: abc"));
    assert!(contents.contains("path: artifact.bin"));
    assert!(!contents.contains("cloud:"));
    assert!(!contents.contains("version_id:"));
}

#[test]
fn rename_rejects_invalid_colliding_staged_and_merged_deliverables() {
    let fixture = managed_fixture();
    let (_, task, branch) = create_task(&fixture, "guarded", "20260830-120300");
    let invalid = workspace_unchecked(&task, ["task", "rename", "Not-Valid"]);
    assert_eq!(invalid.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("lowercase kebab"));

    let same = workspace_unchecked(&task, ["task", "rename", "guarded"]);
    assert_eq!(same.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&same.stderr).contains("already uses"));

    let collision = fixture.shared.join("20260830-120300-collision");
    std::fs::create_dir(&collision).unwrap();
    let colliding = workspace_unchecked(&task, ["task", "rename", "collision"]);
    assert_eq!(colliding.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&colliding.stderr).contains("already exists"));
    std::fs::remove_dir(&collision).unwrap();

    std::fs::write(task.join("staged.txt"), "staged\n").unwrap();
    git(
        &fixture.shared,
        ["add", "20260830-120300-guarded/staged.txt"],
    );
    let staged = workspace_unchecked(&task, ["task", "rename", "staged-rename"]);
    assert_eq!(staged.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&staged.stderr).contains("staged changes"));
    git(
        &fixture.shared,
        ["reset", "--", "20260830-120300-guarded/staged.txt"],
    );

    workspace(&task, ["publish", "-m", "Publish before merge"]);
    git(&fixture.seed, ["fetch", "origin", &branch]);
    git(&fixture.seed, ["merge", "--ff-only", "FETCH_HEAD"]);
    git(&fixture.seed, ["push", "origin", "main"]);
    let merged = workspace_unchecked(&task, ["task", "rename", "after-merge"]);
    assert_eq!(merged.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&merged.stderr).contains("merged tasks cannot"));
    assert!(task.is_dir());
}

#[test]
fn infrastructure_rename_updates_only_mutable_metadata() {
    let fixture = managed_fixture();
    let created = json(&workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "old-policy",
            "--kind",
            "infrastructure",
            "--title",
            "Policy rename",
            "--purpose",
            "Rename an infrastructure topic.",
            "--scope",
            "AGENTS.md",
            "--scope-note",
            "The test owns the managed instruction scaffold.",
        ],
    ));
    let worktree = PathBuf::from(created["path"].as_str().unwrap());
    let manifest = PathBuf::from(created["manifest"].as_str().unwrap());
    let branch = created["branch"].as_str().unwrap().to_owned();
    let task_id = created["task_id"].as_str().unwrap().to_owned();

    let renamed = json(&workspace(&worktree, ["task", "rename", "current-policy"]));
    assert_eq!(renamed["status"], "renamed");
    assert_eq!(renamed["task_id"], task_id);
    assert_eq!(renamed["branch"], branch);
    assert_eq!(renamed["old_path"], serde_json::Value::Null);
    assert_eq!(renamed["new_path"], serde_json::Value::Null);
    assert!(worktree.is_dir());
    let raw = std::fs::read_to_string(&manifest).unwrap();
    assert!(raw.contains("schema_version = 2"));
    assert!(raw.contains("slug = \"current-policy\""));
    assert!(raw.contains(&format!("id = \"{task_id}\"")));
    assert!(raw.contains(&format!("branch = \"{branch}\"")));
    let status = json(&workspace(&worktree, ["task", "status"]));
    assert_eq!(status["slug"], "current-policy");
}

#[test]
fn schema_one_task_is_readable_and_upgraded_by_rename() {
    let fixture = managed_fixture();
    let (task_id, old_task, branch) = create_task(&fixture, "legacy-task", "20260830-120400");
    let manifest = old_task.join(".workspace-mgr-task.toml");
    let raw = std::fs::read_to_string(&manifest).unwrap();
    let legacy = raw
        .replace("schema_version = 2", "schema_version = 1")
        .replace("slug = \"legacy-task\"\n", "");
    std::fs::write(&manifest, legacy).unwrap();
    let before = json(&workspace(&old_task, ["task", "status"]));
    assert_eq!(before["slug"], "legacy-task");

    workspace(&old_task, ["task", "rename", "upgraded-task"]);
    let upgraded = fixture.shared.join("20260830-120400-upgraded-task");
    let raw = std::fs::read_to_string(upgraded.join(".workspace-mgr-task.toml")).unwrap();
    assert!(raw.contains("schema_version = 2"));
    assert!(raw.contains("slug = \"upgraded-task\""));
    assert!(raw.contains(&format!("id = \"{task_id}\"")));
    assert!(raw.contains(&format!("branch = \"{branch}\"")));
}

#[test]
fn rename_invalidates_an_existing_discard_confirmation_plan() {
    let fixture = managed_fixture();
    let (task_id, old_task, _) = create_task(&fixture, "discard-preview", "20260830-120500");
    workspace(&old_task, ["task", "discard", "--dry-run"]);

    workspace(&old_task, ["task", "rename", "renamed-after-preview"]);
    let renamed_task = fixture.shared.join("20260830-120500-renamed-after-preview");
    let renamed_manifest = renamed_task.join(".workspace-mgr-task.toml");
    let confirmation = workspace_unchecked(
        &fixture.shared,
        [
            "task",
            "discard",
            "--manifest",
            renamed_manifest.to_str().unwrap(),
            "--confirm",
            &task_id,
        ],
    );
    assert_eq!(confirmation.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&confirmation.stderr);
    assert!(stderr.contains("state changed"));
    assert!(renamed_task.is_dir());
}

#[cfg(unix)]
#[test]
fn failed_manifest_rewrite_moves_the_deliverable_back_to_its_original_path() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = managed_fixture();
    let (_, original_task, _) = create_task(&fixture, "rollback", "20260830-120600");
    let renamed_task = fixture.shared.join("20260830-120600-should-not-remain");
    let original_permissions = std::fs::metadata(&original_task).unwrap().permissions();
    std::fs::set_permissions(&original_task, std::fs::Permissions::from_mode(0o555)).unwrap();

    let failed = workspace_unchecked(&original_task, ["task", "rename", "should-not-remain"]);

    assert_eq!(failed.status.code(), Some(2));
    assert!(original_task.is_dir());
    assert!(!renamed_task.exists());
    std::fs::set_permissions(&original_task, original_permissions).unwrap();
    let manifest = std::fs::read_to_string(original_task.join(".workspace-mgr-task.toml")).unwrap();
    assert!(manifest.contains("slug = \"rollback\""));
}
