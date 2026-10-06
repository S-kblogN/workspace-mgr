#![cfg(all(feature = "test-storage", unix))]

mod common;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use common::*;
use serde_json::{Value, json as value};

const DONE: &str = "20260712-120000-completed";
const ACTIVE: &str = "20260712-120100-active";
const DESTINATION: &str = "2026/07/20260712-120000-completed";
const MANIFEST: &str = ".workspace-mgr-task.toml";
const RECEIPT: &str = ".workspace-mgr-archive.json";

fn write_task(repo: &Path, id: &str, slug: &str, approval: bool) {
    let path = repo.join(id);
    std::fs::create_dir_all(&path).unwrap();
    let schema = if approval { 3 } else { 2 };
    let extra = if approval {
        "\n[cloud_usage_approval]\nlimit_bytes = 2147483648\nnote = \"The user approved 2 GiB\"\n"
    } else {
        ""
    };
    std::fs::write(path.join(MANIFEST), format!(
        "schema_version = {schema}\nkind = \"deliverable\"\nid = \"{id}\"\nslug = \"{slug}\"\npath = \"{id}\"\nbranch = \"codex/{slug}\"\ntitle = \"Retained task\"\npurpose = \"Retain the task and its history\"\nadditional_scopes = []\n{extra}"
    )).unwrap();
    std::fs::write(path.join("README.md"), "# Retained task\n").unwrap();
    std::fs::write(path.join("result.md"), "retained result\n").unwrap();
}

fn managed_fixture(active: bool) -> (GitFixture, String) {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["init"]);
    fixture.commit_seed("Initialize managed workspace");
    git(&fixture.seed, ["switch", "-c", "codex/completed"]);
    write_task(&fixture.seed, DONE, "completed", true);
    git(&fixture.seed, ["add", DONE]);
    git(&fixture.seed, ["commit", "-m", "Retain the completed task"]);
    git(&fixture.seed, ["push", "origin", "codex/completed"]);
    git(&fixture.seed, ["switch", "main"]);
    git(&fixture.seed, ["merge", "--squash", "codex/completed"]);
    git(
        &fixture.seed,
        ["commit", "-m", "Squash the completed task PR"],
    );
    let merged = String::from_utf8(git(&fixture.seed, ["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    if active {
        write_task(&fixture.seed, ACTIVE, "active", false);
        git(&fixture.seed, ["add", ACTIVE]);
        git(
            &fixture.seed,
            ["commit", "-m", "Keep an active task at top level"],
        );
    }
    git(&fixture.seed, ["push", "origin", "main"]);
    fixture.clone_shared();
    (fixture, merged)
}

fn pr(state: &str, number: u64, branch: &str, merged: &str, head: &str) -> Value {
    value!({
        "number": number,
        "url": format!("https://example.invalid/owner/archive-fixture/pull/{number}"),
        "state": state,
        "mergedAt": (state == "MERGED").then_some("2026-07-12T20:00:00Z"),
        "mergeCommit": (state == "MERGED").then_some(value!({"oid": merged})),
        "headRefName": branch,
        "headRefOid": head,
        "baseRefName": "main",
        "isCrossRepository": false,
    })
}

fn fake_gh(fixture: &GitFixture, merged: &str, active: bool) -> PathBuf {
    let head =
        String::from_utf8(git(&fixture.remote, ["rev-parse", "refs/heads/codex/completed"]).stdout)
            .unwrap()
            .trim()
            .to_owned();
    let mut requests = BTreeMap::from([(
        "codex/completed",
        vec![pr("MERGED", 1, "codex/completed", merged, &head)],
    )]);
    if active {
        requests.insert(
            "codex/active",
            vec![pr("OPEN", 2, "codex/active", merged, &head)],
        );
    }
    write_gh(fixture, &requests)
}

fn write_gh(fixture: &GitFixture, requests: &BTreeMap<&str, Vec<Value>>) -> PathBuf {
    let path = fixture.root.join("fake-gh");
    let database = serde_json::to_string(&requests).unwrap();
    let literal = serde_json::to_string(&database).unwrap();
    std::fs::write(&path, format!(
        "#!/usr/bin/env python3\nimport json, sys\nrequests = json.loads({literal})\nhead = sys.argv[sys.argv.index('--head') + 1]\nrows = requests.get(head, [])\nif '--base' in sys.argv:\n    base = sys.argv[sys.argv.index('--base') + 1]\n    rows = [row for row in rows if row['baseRefName'] == base]\nprint(json.dumps(rows))\n"
    )).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn archive_keeps_a_branch_with_an_open_pr_to_another_base_active() {
    let (fixture, merged) = managed_fixture(false);
    let head =
        String::from_utf8(git(&fixture.remote, ["rev-parse", "refs/heads/codex/completed"]).stdout)
            .unwrap()
            .trim()
            .to_owned();
    let mut open = pr("OPEN", 2, "codex/completed", &merged, &head);
    open["baseRefName"] = value!("release");
    let requests = BTreeMap::from([(
        "codex/completed",
        vec![pr("MERGED", 1, "codex/completed", &merged, &head), open],
    )]);
    let gh = write_gh(&fixture, &requests);
    let preview = json(&archive(&fixture.shared, &gh, &["archive", "--dry-run"]));
    assert!(preview["tasks"].as_array().unwrap().is_empty());
    assert_eq!(preview["skipped"][0]["path"], DONE);
    let (workplace, manifest) = organizer(&fixture, &[DONE, DESTINATION]);
    rejected(
        &workplace,
        &gh,
        &["archive", DONE, "--manifest", manifest.to_str().unwrap()],
        "refuses active or unverified task",
    );
    assert!(workplace.join(DONE).is_dir());
    assert!(!workplace.join(DESTINATION).exists());
}

fn organizer(fixture: &GitFixture, scopes: &[&str]) -> (PathBuf, PathBuf) {
    let mut args = vec![
        "task",
        "create",
        "archive-completed",
        "--kind",
        "infrastructure",
        "--title",
        "Organize completed tasks",
        "--purpose",
        "Organize the user-selected merged tasks",
        "--scope-note",
        "The user requested these archive sources and destinations",
    ];
    for scope in scopes {
        args.extend(["--scope", scope]);
    }
    let created = json(&workspace(&fixture.shared, args));
    (
        PathBuf::from(created["path"].as_str().unwrap()),
        PathBuf::from(created["manifest"].as_str().unwrap()),
    )
}

fn archive(cwd: &Path, gh: &Path, args: &[&str]) -> std::process::Output {
    workspace_env(
        cwd,
        args,
        &[("WORKSPACE_MGR_TEST_GH", gh.to_str().unwrap())],
    )
}

fn rejected(cwd: &Path, gh: &Path, args: &[&str], reason: &str) {
    let output = workspace_env_unchecked(
        cwd,
        args,
        &[("WORKSPACE_MGR_TEST_GH", gh.to_str().unwrap())],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(reason),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn archive_verifies_squash_merged_pr_skips_active_and_applies_in_infrastructure_task() {
    let (fixture, merged) = managed_fixture(true);
    let gh = fake_gh(&fixture, &merged, true);
    assert_eq!(
        git_unchecked(
            &fixture.shared,
            [
                "merge-base",
                "--is-ancestor",
                "origin/codex/completed",
                "origin/main"
            ]
        )
        .status
        .code(),
        Some(1)
    );
    let preview = json(&archive(&fixture.shared, &gh, &["archive", "--dry-run"]));
    assert_eq!(preview["status"], "dry_run");
    assert_eq!(preview["remote_writes"], false);
    assert_eq!(preview["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(preview["tasks"][0]["source"], DONE);
    assert_eq!(preview["tasks"][0]["destination"], DESTINATION);
    assert_eq!(preview["tasks"][0]["pull_request"]["merge_commit"], merged);
    assert_ne!(preview["tasks"][0]["pull_request"]["head_commit"], merged);
    assert_eq!(preview["skipped"][0]["path"], ACTIVE);
    assert!(fixture.shared.join(DONE).is_dir());
    assert!(!fixture.shared.join(DESTINATION).exists());
    rejected(
        &fixture.shared,
        &gh,
        &["archive"],
        "requires a managed repository-infrastructure task",
    );
    rejected(
        &fixture.shared,
        &gh,
        &["archive", ACTIVE, "--dry-run"],
        "refuses active or unverified task",
    );

    let (worktree, infrastructure_manifest) = organizer(&fixture, &[DONE, DESTINATION]);
    let original: toml::Value =
        toml::from_str(&std::fs::read_to_string(worktree.join(DONE).join(MANIFEST)).unwrap())
            .unwrap();
    let before = git(&fixture.remote, ["rev-parse", "refs/heads/main"]).stdout;
    let index_before = git(&worktree, ["ls-files", "--stage"]).stdout;
    let head_before = git(&worktree, ["rev-parse", "HEAD"]).stdout;
    let output = json(&archive(
        &worktree,
        &gh,
        &[
            "archive",
            "--manifest",
            infrastructure_manifest.to_str().unwrap(),
        ],
    ));
    assert_eq!(output["status"], "archived");
    assert_eq!(output["task_id"], "infra-archive-completed");
    assert_eq!(output["remote_writes"], false);
    assert!(!worktree.join(DONE).exists());
    assert!(worktree.join(DESTINATION).join("result.md").is_file());
    assert!(worktree.join(ACTIVE).is_dir());
    assert!(!fixture.shared.join(DONE).exists());
    assert_eq!(git(&worktree, ["ls-files", "--stage"]).stdout, index_before);
    assert_eq!(git(&worktree, ["rev-parse", "HEAD"]).stdout, head_before);
    assert_eq!(
        String::from_utf8_lossy(&git(&worktree, ["branch", "--show-current"]).stdout).trim(),
        "main"
    );
    assert_eq!(
        git(&fixture.remote, ["rev-parse", "refs/heads/main"]).stdout,
        before
    );
    let mut archived: toml::Value = toml::from_str(
        &std::fs::read_to_string(worktree.join(DESTINATION).join(MANIFEST)).unwrap(),
    )
    .unwrap();
    assert_eq!(archived["path"].as_str(), Some(DESTINATION));
    archived["path"] = original["path"].clone();
    assert_eq!(
        archived, original,
        "archive preserves all task fields except path"
    );
    let receipt: Value = serde_json::from_str(
        &std::fs::read_to_string(worktree.join(DESTINATION).join(RECEIPT)).unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["status"], "planned");
    assert_eq!(receipt["task_id"], DONE);
    let status = json(&workspace(&worktree.join(DESTINATION), ["task", "status"]));
    assert_eq!(status["task_id"], DONE);
    assert_eq!(status["scopes"][0], DESTINATION);
}

#[test]
fn archive_refuses_resumed_remote_branches_without_new_pull_requests() {
    let (fixture, merged) = managed_fixture(false);
    // Record the immutable PR head before the previously merged task resumes.
    let gh = fake_gh(&fixture, &merged, false);
    git(&fixture.seed, ["switch", "codex/completed"]);
    std::fs::write(
        fixture.seed.join(DONE).join("result.md"),
        "resumed task work\n",
    )
    .unwrap();
    git(&fixture.seed, ["add", DONE]);
    git(
        &fixture.seed,
        ["commit", "-m", "Resume work after the merged PR"],
    );
    git(&fixture.seed, ["push", "origin", "codex/completed"]);
    let preview = json(&archive(&fixture.shared, &gh, &["archive", "--dry-run"]));
    assert_eq!(preview["status"], "no_changes");
    assert!(preview["tasks"].as_array().unwrap().is_empty());
    assert_eq!(preview["skipped"][0]["path"], DONE);
    rejected(
        &fixture.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
        "refuses active or unverified task",
    );
    let (worktree, infrastructure_manifest) = organizer(&fixture, &[DONE, DESTINATION]);
    rejected(
        &worktree,
        &gh,
        &[
            "archive",
            DONE,
            "--manifest",
            infrastructure_manifest.to_str().unwrap(),
        ],
        "refuses active or unverified task",
    );
    assert!(worktree.join(DONE).join(MANIFEST).is_file());
    assert!(!worktree.join(DESTINATION).exists());

    // Removing the completed branch is allowed; GitHub's merged PR still
    // supplies the original head and merge evidence.
    git(
        &fixture.seed,
        ["push", "origin", "--delete", "codex/completed"],
    );
    let preview = json(&archive(
        &fixture.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
    ));
    assert_eq!(preview["tasks"].as_array().unwrap().len(), 1);
}

#[test]
fn archive_keeps_an_unpublished_resumed_local_branch_active_without_file_overlays() {
    let (fixture, merged) = managed_fixture(false);
    let gh = fake_gh(&fixture, &merged, false);
    let reference = "refs/heads/codex/completed";
    let original =
        String::from_utf8(git(&fixture.shared, ["rev-parse", "origin/codex/completed"]).stdout)
            .unwrap()
            .trim()
            .to_owned();
    let tree =
        String::from_utf8(git(&fixture.shared, ["show", "-s", "--format=%T", &original]).stdout)
            .unwrap()
            .trim()
            .to_owned();
    let resumed = String::from_utf8(
        git(
            &fixture.shared,
            [
                "commit-tree",
                &tree,
                "-p",
                &original,
                "-m",
                "Resume unpublished local task review",
            ],
        )
        .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    git(&fixture.shared, ["update-ref", reference, &resumed]);
    assert!(
        git(&fixture.shared, ["status", "--porcelain"])
            .stdout
            .is_empty()
    );
    let preview = json(&archive(&fixture.shared, &gh, &["archive", "--dry-run"]));
    assert!(preview["tasks"].as_array().unwrap().is_empty());
    assert_eq!(preview["skipped"][0]["path"], DONE);
    assert_eq!(
        String::from_utf8(git(&fixture.shared, ["rev-parse", reference]).stdout)
            .unwrap()
            .trim(),
        resumed
    );
    assert!(fixture.shared.join(DONE).is_dir());
    assert!(!fixture.shared.join(DESTINATION).exists());
}

#[test]
fn archive_supports_year_and_compact_month_layouts() {
    for (layout, prefix) in [("{year}", "2026"), ("{year}{month}", "202607")] {
        let (fixture, merged) = managed_fixture(false);
        let gh = fake_gh(&fixture, &merged, false);
        let destination = format!("{prefix}/{DONE}");
        let preview = json(&archive(
            &fixture.shared,
            &gh,
            &["archive", "--layout", layout, "--dry-run"],
        ));
        assert_eq!(preview["tasks"][0]["destination"], destination);
        let (worktree, infrastructure_manifest) = organizer(&fixture, &[DONE, &destination]);
        archive(
            &worktree,
            &gh,
            &[
                "archive",
                DONE,
                "--layout",
                layout,
                "--manifest",
                infrastructure_manifest.to_str().unwrap(),
            ],
        );
        assert!(worktree.join(&destination).join(MANIFEST).is_file());
        assert!(!worktree.join(DONE).exists());
    }
}

#[test]
fn archive_refuses_scope_collisions_and_shared_overlays_before_moving() {
    let (fixture, merged) = managed_fixture(false);
    let gh = fake_gh(&fixture, &merged, false);
    let (worktree, infrastructure_manifest) = organizer(&fixture, &[DONE]);
    rejected(
        &worktree,
        &gh,
        &[
            "archive",
            DONE,
            "--manifest",
            infrastructure_manifest.to_str().unwrap(),
        ],
        "escapes the infrastructure task's declared scopes",
    );
    assert!(worktree.join(DONE).is_dir());

    // Read-only previews still decide collisions and overlays before planning migrations.
    std::fs::create_dir_all(fixture.shared.join(DESTINATION)).unwrap();
    rejected(
        &fixture.shared,
        &gh,
        &["archive", "--dry-run"],
        "destination already exists",
    );
    std::fs::remove_dir_all(fixture.shared.join("2026")).unwrap();
    std::fs::write(fixture.shared.join(DONE).join("result.md"), "local edits\n").unwrap();
    rejected(&fixture.shared, &gh, &["archive", "--dry-run"], "overlays");
    assert!(fixture.shared.join(DONE).is_dir());
    assert!(!fixture.shared.join(DESTINATION).exists());
    assert!(!fixture.shared.join(DONE).join(RECEIPT).exists());
}

#[test]
fn archive_rejects_changed_materialized_s3_payloads_without_touching_metadata() {
    let (fixture, merged) = managed_fixture(false);
    let gh = fake_gh(&fixture, &merged, false);
    let task = fixture.seed.join(DONE);
    std::fs::write(task.join("model.bin.dvc"), "outs:\n- path: model.bin\n  md5: 9f9f90dbe3e5ee1218c86b8839db1995\n  size: 6\n  cloud:\n    workspace-mgr:\n      version_id: retained-version\n").unwrap();
    std::fs::write(task.join(".gitignore"), "/model.bin\n").unwrap();
    fixture.commit_seed("Retain the task's S3 metadata");
    git(&fixture.shared, ["pull", "--ff-only", "origin", "main"]);
    std::fs::write(fixture.shared.join(DONE).join("model.bin"), "changed\n").unwrap();
    let pointer = std::fs::read(fixture.shared.join(DONE).join("model.bin.dvc")).unwrap();
    rejected(
        &fixture.shared,
        &gh,
        &["archive", "--dry-run"],
        "locally changed materialized S3 output",
    );
    assert_eq!(
        std::fs::read(fixture.shared.join(DONE).join("model.bin.dvc")).unwrap(),
        pointer
    );
    assert!(fixture.shared.join(DONE).is_dir());
    assert!(!fixture.shared.join(DESTINATION).exists());
}
