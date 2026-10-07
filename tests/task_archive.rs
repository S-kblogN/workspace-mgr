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
        "#!/usr/bin/env python3\nimport json, sys\nrequests = json.loads({literal})\nif sys.argv[1] == 'api':\n    commit = sys.argv[-1].split('/commits/')[1].split('/')[0]\n    rows = requests.get('commit:' + commit, [])\n    rows = [dict(number=row['number'], html_url=row['url'], state='closed' if row['state'] != 'OPEN' else 'open', merged_at=row['mergedAt'], merge_commit_sha=(row['mergeCommit'] or {{}}).get('oid'), head={{'ref': row['headRefName'], 'sha': row['headRefOid'], 'repo': {{'full_name': 'owner/archive-fixture'}}}}, base={{'ref': row['baseRefName'], 'sha': row['headRefOid'], 'repo': {{'full_name': 'owner/archive-fixture'}}}}) for row in rows]\nelif sys.argv[2] == 'view':\n    rows = [row for group in requests.values() for row in group if row['number'] == int(sys.argv[3])]\n    print(json.dumps(rows[0] if rows else {{}}))\n    sys.exit(0)\nelse:\n    head = sys.argv[sys.argv.index('--head') + 1]\n    rows = requests.get(head, [])\nif '--base' in sys.argv:\n    base = sys.argv[sys.argv.index('--base') + 1]\n    rows = [row for row in rows if row['baseRefName'] == base]\nprint(json.dumps(rows))\n"
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
fn archive_preflight_refuses_a_location_bound_runtime_without_moving_content() {
    let (fixture, merged) = managed_fixture(false);
    let gh = fake_gh(&fixture, &merged, false);
    let (workplace, manifest) = organizer(&fixture, &[DONE, DESTINATION]);
    let source = workplace.join(DONE);
    let venv = source.join(".venv");
    std::fs::create_dir_all(venv.join("bin")).unwrap();
    std::fs::write(venv.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
    let launcher = format!("#!{}/bin/python\nprint('keep')\n", venv.display());
    std::fs::write(venv.join("bin/tool"), &launcher).unwrap();
    let exclude = workplace.join(".git/info/exclude");
    let old_exclude = std::fs::read_to_string(&exclude).unwrap();
    std::fs::write(&exclude, format!("{old_exclude}\n/{DONE}/.venv/\n")).unwrap();
    let manifest_before = std::fs::read(source.join(MANIFEST)).unwrap();
    for dry_run in [true, false] {
        let mut args = vec!["archive", DONE, "--manifest", manifest.to_str().unwrap()];
        if dry_run {
            args.push("--dry-run");
        }
        rejected(
            &workplace,
            &gh,
            &args,
            "relocation would invalidate the Python virtual environment",
        );
        assert!(!workplace.join(DESTINATION).exists());
        assert_eq!(
            std::fs::read(source.join(MANIFEST)).unwrap(),
            manifest_before
        );
        assert_eq!(
            std::fs::read_to_string(venv.join("bin/tool")).unwrap(),
            launcher
        );
        assert!(!source.join(RECEIPT).exists());
    }
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
    let original_head = oid(&fixture.remote, "refs/heads/codex/completed");
    let task = fixture.seed.join(DONE);
    std::fs::write(task.join("model.bin.dvc"), "outs:\n- path: model.bin\n  md5: 9f9f90dbe3e5ee1218c86b8839db1995\n  size: 6\n  cloud:\n    workspace-mgr:\n      version_id: retained-version\n").unwrap();
    std::fs::write(task.join(".gitignore"), "/model.bin\n").unwrap();
    fixture.commit_seed("Retain the task's S3 metadata");
    let metadata_merge = oid(&fixture.seed, "HEAD");
    let gh = write_gh(
        &fixture,
        &BTreeMap::from([(
            "codex/completed",
            vec![
                pr("MERGED", 1, "codex/completed", &merged, &original_head),
                pr(
                    "MERGED",
                    2,
                    "codex/completed",
                    &metadata_merge,
                    &metadata_merge,
                ),
            ],
        )]),
    );
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

fn oid(repo: &Path, reference: &str) -> String {
    String::from_utf8(git(repo, ["rev-parse", reference]).stdout)
        .unwrap()
        .trim()
        .to_owned()
}

#[test]
fn archive_accepts_a_stale_local_branch_that_is_an_ancestor_of_the_reviewed_head() {
    let (fixture, merged) = managed_fixture(false);
    let gh = fake_gh(&fixture, &merged, false);
    let head = oid(&fixture.shared, "origin/codex/completed");
    let older = oid(&fixture.shared, &format!("{head}^"));
    git(
        &fixture.shared,
        ["update-ref", "refs/heads/codex/completed", &older],
    );
    let preview = json(&archive(
        &fixture.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
    ));
    assert_eq!(preview["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(oid(&fixture.shared, "refs/heads/codex/completed"), older);
}

#[test]
fn archive_rejects_a_divergent_local_branch_even_when_the_tree_is_identical() {
    let (fixture, merged) = managed_fixture(false);
    let gh = fake_gh(&fixture, &merged, false);
    let head = oid(&fixture.shared, "origin/codex/completed");
    let tree = oid(&fixture.shared, &format!("{head}^{{tree}}"));
    let parent = oid(&fixture.shared, &format!("{head}^"));
    let diverged = String::from_utf8(
        git(
            &fixture.shared,
            [
                "commit-tree",
                &tree,
                "-p",
                &parent,
                "-m",
                "Unreviewed parallel history",
            ],
        )
        .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    git(
        &fixture.shared,
        ["update-ref", "refs/heads/codex/completed", &diverged],
    );
    rejected(
        &fixture.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
        "refuses active or unverified task",
    );
}

fn migrated_fixture(reviewed: bool) -> (GitFixture, PathBuf) {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["init"]);
    fixture.commit_seed("Initialize managed workspace");
    git(&fixture.seed, ["switch", "-c", "historical/completed"]);
    write_task(&fixture.seed, DONE, "completed", false);
    let manifest = fixture.seed.join(DONE).join(MANIFEST);
    let raw = std::fs::read_to_string(&manifest)
        .unwrap()
        .replace("schema_version = 2", "schema_version = 1")
        .replace("slug = \"completed\"\n", "")
        .replace(
            "branch = \"codex/completed\"",
            "branch = \"historical/completed\"",
        );
    std::fs::write(&manifest, raw).unwrap();
    git(&fixture.seed, ["add", DONE]);
    git(
        &fixture.seed,
        ["commit", "-m", "Finish task under the historical schema"],
    );
    let original_head = oid(&fixture.seed, "HEAD");
    git(&fixture.seed, ["push", "origin", "historical/completed"]);
    git(&fixture.seed, ["switch", "main"]);
    git(&fixture.seed, ["merge", "--squash", "historical/completed"]);
    git(
        &fixture.seed,
        ["commit", "-m", "Merge original reviewed task"],
    );
    let original_merge = oid(&fixture.seed, "HEAD");
    git(&fixture.seed, ["switch", "-c", "codex/infra-schema"]);
    write_task(&fixture.seed, DONE, "completed", true);
    git(&fixture.seed, ["add", DONE]);
    git(
        &fixture.seed,
        ["commit", "-m", "Migrate completed task identity and schema"],
    );
    let migration_head = oid(&fixture.seed, "HEAD");
    git(&fixture.seed, ["push", "origin", "codex/infra-schema"]);
    git(&fixture.seed, ["switch", "main"]);
    git(&fixture.seed, ["merge", "--squash", "codex/infra-schema"]);
    git(
        &fixture.seed,
        ["commit", "-m", "Merge the manifest migration"],
    );
    let migration_merge = oid(&fixture.seed, "HEAD");
    git(&fixture.seed, ["push", "origin", "main"]);
    fixture.clone_shared();
    let key = format!("commit:{migration_merge}");
    let mut requests = BTreeMap::from([(
        "historical/completed",
        vec![pr(
            "MERGED",
            1,
            "historical/completed",
            &original_merge,
            &original_head,
        )],
    )]);
    if reviewed {
        requests.insert(
            &key,
            vec![pr(
                "MERGED",
                2,
                "codex/infra-schema",
                &migration_merge,
                &migration_head,
            )],
        );
    }
    let gh = write_gh(&fixture, &requests);
    (fixture, gh)
}

#[test]
fn archive_follows_a_reviewed_schema_and_branch_migration_to_the_original_task() {
    let (fixture, gh) = migrated_fixture(true);
    let preview = json(&archive(
        &fixture.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
    ));
    assert_eq!(preview["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(preview["tasks"][0]["branch"], "codex/completed");
    assert_eq!(preview["tasks"][0]["pull_request"]["number"], 2);
    let (workplace, manifest) = organizer(&fixture, &[DONE, DESTINATION]);
    archive(
        &workplace,
        &gh,
        &["archive", DONE, "--manifest", manifest.to_str().unwrap()],
    );
    assert!(workplace.join(DESTINATION).join(MANIFEST).is_file());
}

#[test]
fn archive_rejects_an_identity_migration_without_a_verified_review() {
    let (fixture, gh) = migrated_fixture(false);
    rejected(
        &fixture.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
        "refuses active or unverified task",
    );
    assert!(fixture.shared.join(DONE).is_dir());
    assert!(!fixture.shared.join(DESTINATION).exists());
}

#[test]
fn legacy_tasks_are_visible_and_require_explicit_reviewed_adoption() {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["init"]);
    fixture.commit_seed("Initialize managed workspace");
    git(&fixture.seed, ["switch", "-c", "legacy/completed"]);
    std::fs::create_dir_all(fixture.seed.join(DONE)).unwrap();
    std::fs::write(fixture.seed.join(DONE).join("README.md"), "# Legacy task\n").unwrap();
    std::fs::write(
        fixture.seed.join(DONE).join("result.md"),
        "retained result\n",
    )
    .unwrap();
    git(&fixture.seed, ["add", DONE]);
    git(
        &fixture.seed,
        ["commit", "-m", "Complete a pre-manifest task"],
    );
    let head = oid(&fixture.seed, "HEAD");
    git(&fixture.seed, ["push", "origin", "legacy/completed"]);
    git(&fixture.seed, ["switch", "main"]);
    git(&fixture.seed, ["merge", "--squash", "legacy/completed"]);
    git(&fixture.seed, ["commit", "-m", "Merge legacy task review"]);
    let merged = oid(&fixture.seed, "HEAD");
    git(&fixture.seed, ["push", "origin", "main"]);
    fixture.clone_shared();
    let requests = BTreeMap::from([(
        "legacy/completed",
        vec![pr("MERGED", 1, "legacy/completed", &merged, &head)],
    )]);
    let gh = write_gh(&fixture, &requests);
    let preview = json(&archive(&fixture.shared, &gh, &["archive", "--dry-run"]));
    assert_eq!(preview["skipped"][0]["path"], DONE);
    assert!(
        preview["skipped"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("task adopt")
    );
    rejected(
        &fixture.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
        "legacy task has no manifest",
    );
    let before = std::fs::read(fixture.shared.join(DONE).join("result.md")).unwrap();
    let adoption = json(&archive(
        &fixture.shared,
        &gh,
        &[
            "task",
            "adopt",
            DONE,
            "--pull-request",
            "1",
            "--title",
            "Legacy task",
            "--purpose",
            "Retain completed historical work",
            "--dry-run",
        ],
    ));
    assert_eq!(adoption["status"], "dry_run");
    assert!(!fixture.shared.join(DONE).join(MANIFEST).exists());
    let (workplace, manifest) = organizer(&fixture, &[DONE, DESTINATION]);
    let args = [
        "task",
        "adopt",
        DONE,
        "--pull-request",
        "1",
        "--title",
        "Legacy task",
        "--purpose",
        "Retain completed historical work",
        "--manifest",
        manifest.to_str().unwrap(),
    ];
    let adopted = json(&archive(&workplace, &gh, &args));
    assert_eq!(adopted["status"], "adopted");
    assert_eq!(
        json(&archive(&workplace, &gh, &args))["status"],
        "no_changes"
    );
    assert_eq!(
        std::fs::read(workplace.join(DONE).join("result.md")).unwrap(),
        before
    );
    // Leading JSON whitespace keeps the adopted evidence semantically
    // intact while exercising the normal >10 MiB automatic-placement limit.
    let record_path = format!("{DONE}/.workspace-mgr-legacy.json");
    let record = format!(
        "{}{}",
        " ".repeat(10_485_761),
        std::fs::read_to_string(workplace.join(&record_path)).unwrap()
    );
    std::fs::write(workplace.join(&record_path), &record).unwrap();
    let status = json(&workspace(
        &workplace,
        [
            "storage",
            "status",
            &record_path,
            "--manifest",
            manifest.to_str().unwrap(),
        ],
    ));
    assert_eq!(status["placements"][0]["target"], "git");
    rejected(
        &workplace,
        &gh,
        &[
            "storage",
            "set",
            &record_path,
            "--to",
            "s3",
            "--reason",
            "Exercise metadata protection",
            "--manifest",
            manifest.to_str().unwrap(),
            "--dry-run",
        ],
        "legacy adoption records must remain in Git",
    );
    rejected(
        &workplace,
        &gh,
        &[
            "untrack",
            &record_path,
            "--manifest",
            manifest.to_str().unwrap(),
            "--dry-run",
        ],
        "untrack may not hide task or storage control metadata",
    );
    let plan = json(&workspace(
        &workplace,
        ["plan", "--manifest", manifest.to_str().unwrap()],
    ));
    assert_eq!(plan["status"], "dry_run");
    assert_eq!(
        std::fs::read_to_string(workplace.join(&record_path)).unwrap(),
        record
    );
    assert!(!workplace.join(format!("{record_path}.dvc")).exists());
    rejected(
        &workplace,
        &gh,
        &["archive", DONE, "--manifest", manifest.to_str().unwrap()],
        "refuses active or unverified task",
    );

    // Publish adoption through a separate infrastructure review, then verify
    // that both the legacy review and its explicit migration remain usable.
    let publication = json(&workspace(
        &workplace,
        [
            "publish",
            "--manifest",
            manifest.to_str().unwrap(),
            "-m",
            "Publish the explicit legacy adoption for review",
        ],
    ));
    let adoption_merge = publication["commit_oid"].as_str().unwrap().to_owned();
    let published_record = git(
        &workplace,
        ["show", &format!("{adoption_merge}:{record_path}")],
    )
    .stdout;
    assert_eq!(published_record, record.as_bytes());
    // The CLI deliberately leaves the shared index untouched. Align this
    // isolated checkout's adoption files before simulating the review merge.
    git(&workplace, ["add", DONE]);
    git(&workplace, ["merge", "--ff-only", &adoption_merge]);
    git(&workplace, ["push", "origin", "main"]);
    let associated = format!("commit:{adoption_merge}");
    let mut requests = requests;
    requests.insert(
        &associated,
        vec![pr(
            "MERGED",
            2,
            "codex/infra-archive-completed",
            &adoption_merge,
            &adoption_merge,
        )],
    );
    let gh = write_gh(&fixture, &requests);
    let preview = json(&archive(
        &workplace,
        &gh,
        &[
            "archive",
            DONE,
            "--manifest",
            manifest.to_str().unwrap(),
            "--dry-run",
        ],
    ));
    assert_eq!(preview["tasks"].as_array().unwrap().len(), 1);
}

#[test]
fn archive_rejects_unreviewed_task_content_on_the_shared_base() {
    let (fixture, merged) = managed_fixture(false);
    let gh = fake_gh(&fixture, &merged, false);
    std::fs::write(
        fixture.seed.join(DONE).join("result.md"),
        "New work without a review\n",
    )
    .unwrap();
    fixture.commit_seed("Direct change to completed task content");
    git(&fixture.shared, ["pull", "--ff-only", "origin", "main"]);
    rejected(
        &fixture.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
        "refuses active or unverified task",
    );
    assert!(fixture.shared.join(DONE).is_dir());
}

#[test]
fn archive_accepts_later_reviewed_content_with_an_unchanged_manifest() {
    let (fixture, merged) = managed_fixture(false);
    let original_head = oid(&fixture.remote, "refs/heads/codex/completed");
    git(&fixture.seed, ["switch", "-C", "codex/completed", "main"]);
    std::fs::write(
        fixture.seed.join(DONE).join("result.md"),
        "Further reviewed work\n",
    )
    .unwrap();
    git(&fixture.seed, ["add", DONE]);
    git(
        &fixture.seed,
        ["commit", "-m", "Extend task in a second review"],
    );
    let later_head = oid(&fixture.seed, "HEAD");
    git(
        &fixture.seed,
        ["push", "--force-with-lease", "origin", "codex/completed"],
    );
    git(&fixture.seed, ["switch", "main"]);
    git(&fixture.seed, ["merge", "--squash", "codex/completed"]);
    git(&fixture.seed, ["commit", "-m", "Merge later task review"]);
    let later_merge = oid(&fixture.seed, "HEAD");
    git(&fixture.seed, ["push", "origin", "main"]);
    git(&fixture.shared, ["pull", "--ff-only", "origin", "main"]);
    let gh = write_gh(
        &fixture,
        &BTreeMap::from([(
            "codex/completed",
            vec![
                pr("MERGED", 1, "codex/completed", &merged, &original_head),
                pr("MERGED", 2, "codex/completed", &later_merge, &later_head),
            ],
        )]),
    );
    let preview = json(&archive(
        &fixture.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
    ));
    assert_eq!(preview["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(preview["tasks"][0]["pull_request"]["number"], 2);
    assert_eq!(
        preview["tasks"][0]["review_history"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn archive_fetches_a_deleted_branches_missing_immutable_review_head_for_a_stale_local_ref() {
    let (fixture, merged) = managed_fixture(false);
    let head = oid(&fixture.remote, "refs/heads/codex/completed");
    let older = oid(&fixture.seed, &format!("{head}^"));
    git(&fixture.remote, ["update-ref", "refs/pull/1/head", &head]);
    git(
        &fixture.seed,
        ["push", "origin", "--delete", "codex/completed"],
    );
    // Clone only main after the squash branch disappeared. The reviewed head
    // is reachable only through GitHub's PR ref; no task branch is fetched.
    let historical = fixture.root.join("historical-shared");
    git(
        &fixture.root,
        [
            "clone",
            "--no-local",
            "--single-branch",
            "--branch",
            "main",
            fixture.remote.to_str().unwrap(),
            historical.to_str().unwrap(),
        ],
    );
    assert!(
        !git_unchecked(
            &historical,
            ["cat-file", "-e", &format!("{head}^{{commit}}")]
        )
        .status
        .success()
    );
    git(
        &historical,
        ["update-ref", "refs/heads/codex/completed", &older],
    );
    let gh = write_gh(
        &fixture,
        &BTreeMap::from([(
            "codex/completed",
            vec![pr("MERGED", 1, "codex/completed", &merged, &head)],
        )]),
    );
    let preview = json(&archive(&historical, &gh, &["archive", DONE, "--dry-run"]));
    assert_eq!(preview["tasks"].as_array().unwrap().len(), 1);
    assert!(
        git_unchecked(
            &historical,
            ["cat-file", "-e", &format!("{head}^{{commit}}")]
        )
        .status
        .success()
    );
    assert_eq!(oid(&historical, "refs/heads/codex/completed"), older);
    assert!(!historical.join(".git/FETCH_HEAD").exists());
}

#[test]
fn archive_refuses_shallow_history_that_hides_the_original_task_review() {
    let (fixture, original_merge) = managed_fixture(false);
    let manifest = fixture.seed.join(DONE).join(MANIFEST);
    let raw = std::fs::read_to_string(&manifest).unwrap().replace(
        "title = \"Retained task\"",
        "title = \"Migrated historical task\"",
    );
    std::fs::write(&manifest, raw).unwrap();
    fixture.commit_seed("Review a later manifest migration");
    let latest = oid(&fixture.seed, "HEAD");
    git(
        &fixture.seed,
        ["push", "origin", "--delete", "codex/completed"],
    );
    // A shallow clone sees only the later reviewed manifest. Its missing
    // ancestry must not hide the original branch, reviews, or identity gaps.
    let shallow = fixture.root.join("shallow-shared");
    let remote_url = format!("file://{}", fixture.remote.display());
    git(
        &fixture.root,
        [
            "clone",
            "--depth",
            "1",
            "--branch",
            "main",
            &remote_url,
            shallow.to_str().unwrap(),
        ],
    );
    assert_eq!(
        String::from_utf8(git(&shallow, ["rev-parse", "--is-shallow-repository"]).stdout)
            .unwrap()
            .trim(),
        "true"
    );
    assert!(
        !git_unchecked(
            &shallow,
            ["cat-file", "-e", &format!("{original_merge}^{{commit}}")]
        )
        .status
        .success()
    );
    let gh = write_gh(
        &fixture,
        &BTreeMap::from([(
            "codex/completed",
            vec![pr("MERGED", 2, "codex/completed", &latest, &latest)],
        )]),
    );
    let before = std::fs::read(shallow.join(DONE).join(MANIFEST)).unwrap();
    rejected(
        &shallow,
        &gh,
        &["archive", DONE, "--dry-run"],
        "archive requires complete Git history",
    );
    assert_eq!(
        std::fs::read(shallow.join(DONE).join(MANIFEST)).unwrap(),
        before
    );
    assert!(!shallow.join(DESTINATION).exists());
    assert_eq!(oid(&shallow, "HEAD"), latest);
}
