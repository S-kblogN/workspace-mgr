#![cfg(all(feature = "test-storage", unix))]

mod common;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use common::*;
use serde_json::{Value, json as value};
use sha2::{Digest, Sha256};

const DONE: &str = "20260712-120000-completed";
const ACTIVE: &str = "20260712-120100-active";
const DESTINATION: &str = "2026/07/20260712-120000-completed";
const RELATED: &str = "20260812-120000-related";
const RELATED_DESTINATION: &str = "2026/08/20260812-120000-related";
const RELATED_SAME_MONTH: &str = "20260712-121000-related";
const RELATED_SAME_MONTH_DESTINATION: &str = "2026/07/20260712-121000-related";
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

fn related_completed_fixture() -> (GitFixture, PathBuf) {
    related_completed_fixture_for(RELATED)
}

fn related_completed_fixture_for(related: &str) -> (GitFixture, PathBuf) {
    let (fixture, first_merge) = managed_fixture(false);
    let first_head = oid(&fixture.remote, "refs/heads/codex/completed");
    git(&fixture.seed, ["switch", "-c", "codex/related"]);
    write_task(&fixture.seed, related, "related", false);
    std::fs::write(fixture.seed.join(related).join("input.tsv"), "value\n17\n").unwrap();
    git(&fixture.seed, ["add", related]);
    git(&fixture.seed, ["commit", "-m", "Complete the related task"]);
    let head = oid(&fixture.seed, "HEAD");
    git(&fixture.seed, ["push", "origin", "codex/related"]);
    git(&fixture.seed, ["switch", "main"]);
    git(&fixture.seed, ["merge", "--squash", "codex/related"]);
    git(&fixture.seed, ["commit", "-m", "Merge related task review"]);
    let merged = oid(&fixture.seed, "HEAD");
    git(&fixture.seed, ["push", "origin", "main"]);
    git(&fixture.shared, ["pull", "--ff-only", "origin", "main"]);
    let gh = write_gh(
        &fixture,
        &BTreeMap::from([
            (
                "codex/completed",
                vec![pr(
                    "MERGED",
                    1,
                    "codex/completed",
                    &first_merge,
                    &first_head,
                )],
            ),
            (
                "codex/related",
                vec![pr("MERGED", 2, "codex/related", &merged, &head)],
            ),
        ]),
    );
    (fixture, gh)
}

fn ignore_fixture_paths(repo: &Path, paths: &[String]) {
    let exclude = repo.join(".git/info/exclude");
    let original = std::fs::read_to_string(&exclude).unwrap();
    let rules = paths
        .iter()
        .map(|path| format!("/{path}\n"))
        .collect::<String>();
    std::fs::write(exclude, format!("{original}\n{rules}")).unwrap();
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
    write_gh_without_historical_queries(fixture, requests, &[])
}

fn write_gh_without_historical_queries(
    fixture: &GitFixture,
    requests: &BTreeMap<&str, Vec<Value>>,
    blocked_commits: &[String],
) -> PathBuf {
    let path = fixture.root.join("fake-gh");
    let database = serde_json::to_string(&requests).unwrap();
    let literal = serde_json::to_string(&database).unwrap();
    let blocked = serde_json::to_string(blocked_commits).unwrap();
    let query_marker = serde_json::to_string(
        fixture
            .root
            .join("blocked-historical-query")
            .to_str()
            .unwrap(),
    )
    .unwrap();
    std::fs::write(&path, format!(
        "#!/usr/bin/env python3\nimport json, sys\nrequests = json.loads({literal})\nif sys.argv[1] == 'api':\n    commit = sys.argv[-1].split('/commits/')[1].split('/')[0]\n    if commit in {blocked}:\n        open({query_marker}, 'w').write(commit)\n        sys.exit('Historical commit review lookup is forbidden after a completion checkpoint')\n    rows = requests.get('commit:' + commit, [])\n    rows = [dict(number=row['number'], html_url=row['url'], state='closed' if row['state'] != 'OPEN' else 'open', merged_at=row['mergedAt'], merge_commit_sha=(row['mergeCommit'] or {{}}).get('oid'), head={{'ref': row['headRefName'], 'sha': row['headRefOid'], 'repo': {{'full_name': 'owner/archive-fixture'}}}}, base={{'ref': row['baseRefName'], 'sha': row['headRefOid'], 'repo': {{'full_name': 'owner/archive-fixture'}}}}) for row in rows]\nelif sys.argv[2] == 'view':\n    rows = [row for group in requests.values() for row in group if row['number'] == int(sys.argv[3])]\n    print(json.dumps(rows[0] if rows else {{}}))\n    sys.exit(0)\nelse:\n    head = sys.argv[sys.argv.index('--head') + 1]\n    rows = requests.get(head, [])\nif '--base' in sys.argv:\n    base = sys.argv[sys.argv.index('--base') + 1]\n    rows = [row for row in rows if row['baseRefName'] == base]\nprint(json.dumps(rows))\n"
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
    let environment = archive_environment(gh);
    let values = environment
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect::<Vec<_>>();
    workspace_env(cwd, args, &values)
}

fn rejected(cwd: &Path, gh: &Path, args: &[&str], reason: &str) {
    let environment = archive_environment(gh);
    let values = environment
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect::<Vec<_>>();
    let output = workspace_env_unchecked(cwd, args, &values);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(reason),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn archive_environment(gh: &Path) -> Vec<(String, String)> {
    let mut environment = vec![(
        "WORKSPACE_MGR_TEST_GH".to_owned(),
        gh.to_str().unwrap().to_owned(),
    )];
    let guard = gh.parent().unwrap().join("historical-config-guard");
    if guard.is_dir() {
        let paths = std::iter::once(guard)
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap()))
            .collect::<Vec<_>>();
        environment.push((
            "PATH".to_owned(),
            std::env::join_paths(paths).unwrap().into_string().unwrap(),
        ));
    }
    environment
}

fn forbid_historical_config_reads(fixture: &GitFixture, old_blob: &str) {
    let guard = fixture.root.join("historical-config-guard");
    std::fs::create_dir(&guard).unwrap();
    let real_git = String::from_utf8(command(&fixture.root, "which", ["git"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    let real_git = serde_json::to_string(&real_git).unwrap();
    let old_blob = serde_json::to_string(old_blob).unwrap();
    let marker = serde_json::to_string(
        fixture
            .root
            .join("historical-config-read")
            .to_str()
            .unwrap(),
    )
    .unwrap();
    let program = guard.join("git");
    std::fs::write(&program, format!(
        "#!/usr/bin/env python3\nimport os, subprocess, sys\nreal_git = {real_git}\nold_blob = {old_blob}\nargs = sys.argv[1:]\ncommands = {{'show', 'cat-file'}}\nif commands.intersection(args) and '-e' not in args and '-t' not in args:\n    for arg in args:\n        if arg == old_blob or (':' in arg and arg.endswith('/{MANIFEST}')):\n            prefix = args[:args.index('show')] if 'show' in args else args[:args.index('cat-file')]\n            resolved = subprocess.run([real_git, *prefix, 'rev-parse', '--verify', arg], capture_output=True, text=True)\n            if arg == old_blob or (resolved.returncode == 0 and resolved.stdout.strip() == old_blob):\n                open({marker}, 'w').write(' '.join(args))\n                sys.exit('Reading any historical task configuration is forbidden')\nos.execv(real_git, [real_git, *args])\n"
    )).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
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
fn archive_and_cancel_preserve_ordinary_uv_cache_git_markers_and_ignored_bytes() {
    let (fixture, merged) = managed_fixture(false);
    let gh = fake_gh(&fixture, &merged, false);
    let (workplace, manifest) = organizer(&fixture, &[DONE, DESTINATION]);
    let cache_relative = "cache/uv/archive-v0/entry";
    let source_cache = workplace.join(DONE).join(cache_relative);
    std::fs::create_dir_all(&source_cache).unwrap();
    std::fs::write(source_cache.join(".git"), []).unwrap();
    let payload = b"\0ignored hydrated cache bytes\xff";
    std::fs::write(source_cache.join("payload.bin"), payload).unwrap();
    let exclude = workplace.join(".git/info/exclude");
    let original_exclude = std::fs::read_to_string(&exclude).unwrap();
    std::fs::write(&exclude, format!("{original_exclude}\n/{DONE}/cache/\n")).unwrap();
    let original_manifest = std::fs::read(workplace.join(DONE).join(MANIFEST)).unwrap();
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
    let result = json(&archive(
        &workplace,
        &gh,
        &["archive", DONE, "--manifest", manifest.to_str().unwrap()],
    ));
    assert_eq!(result["status"], "archived");
    let archived_cache = workplace.join(DESTINATION).join(cache_relative);
    assert!(
        std::fs::read(archived_cache.join(".git"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        std::fs::read(archived_cache.join("payload.bin")).unwrap(),
        payload
    );
    for dry_run in [true, false] {
        let mut args = vec![
            "archive",
            DONE,
            "--manifest",
            manifest.to_str().unwrap(),
            "--cancel",
        ];
        if dry_run {
            args.push("--dry-run");
        }
        let result = json(&archive(&workplace, &gh, &args));
        assert_eq!(
            result["status"],
            if dry_run { "dry_run" } else { "cancelled" }
        );
    }
    assert!(std::fs::read(source_cache.join(".git")).unwrap().is_empty());
    assert_eq!(
        std::fs::read(source_cache.join("payload.bin")).unwrap(),
        payload
    );
    assert_eq!(
        std::fs::read(workplace.join(DONE).join(MANIFEST)).unwrap(),
        original_manifest
    );
    assert!(!workplace.join(DESTINATION).exists());
}

#[test]
fn archive_historical_records_require_explicit_acknowledgment_and_preserve_exact_bytes() {
    let (fixture, merged) = managed_fixture(false);
    let gh = fake_gh(&fixture, &merged, false);
    let (workplace, manifest) = organizer(&fixture, &[DONE, DESTINATION]);
    let history_path = format!("{DONE}/logs/history.log");
    let root_history_path = "import-history.json";
    let history = format!(
        "2026-07-12 completed run\nrecorded cwd: {}\nrecorded input: {DONE}/result.md\nretained output: 中文\n",
        workplace.join(DONE).display(),
    )
    .into_bytes();
    let root_history =
        format!("{{\"past_task\":\"{DONE}\",\"status\":\"completed\"}}\n").into_bytes();
    std::fs::create_dir(workplace.join(DONE).join("logs")).unwrap();
    std::fs::write(workplace.join(&history_path), &history).unwrap();
    std::fs::write(workplace.join(root_history_path), &root_history).unwrap();
    std::fs::set_permissions(
        workplace.join(&history_path),
        std::fs::Permissions::from_mode(0o440),
    )
    .unwrap();
    ignore_fixture_paths(
        &workplace,
        &[format!("{DONE}/logs/"), root_history_path.to_owned()],
    );
    let original_manifest = std::fs::read(workplace.join(DONE).join(MANIFEST)).unwrap();
    for dry_run in [true, false] {
        let mut args = vec!["archive", DONE, "--manifest", manifest.to_str().unwrap()];
        if dry_run {
            args.push("--dry-run");
        }
        rejected(&workplace, &gh, &args, "path references");
        assert!(!workplace.join(DESTINATION).exists());
        assert_eq!(
            std::fs::read(workplace.join(&history_path)).unwrap(),
            history
        );
    }
    let mut final_records = Value::Null;
    for dry_run in [true, false] {
        let mut args = vec![
            "archive",
            DONE,
            "--manifest",
            manifest.to_str().unwrap(),
            "--historical-record",
            history_path.as_str(),
            "--historical-record",
            root_history_path,
        ];
        if dry_run {
            args.push("--dry-run");
        }
        let report = json(&archive(&workplace, &gh, &args));
        assert_eq!(
            report["status"],
            if dry_run { "dry_run" } else { "archived" }
        );
        let records = report["historical_records"].as_array().unwrap();
        assert_eq!(records.len(), 2);
        for (path, bytes) in [
            (history_path.as_str(), &history),
            (root_history_path, &root_history),
        ] {
            let record = records
                .iter()
                .find(|record| record["path"] == path)
                .unwrap();
            assert_eq!(record["role"], "historical-record");
            let digest = Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            assert_eq!(record["sha256"], digest);
            assert!(record["unix_mode"].as_u64().is_some());
        }
        if dry_run {
            assert!(!workplace.join(DESTINATION).exists());
            assert_eq!(
                std::fs::read(workplace.join(&history_path)).unwrap(),
                history
            );
        } else {
            final_records = report["historical_records"].clone();
        }
    }
    let archived_history = workplace.join(DESTINATION).join("logs/history.log");
    assert_eq!(std::fs::read(&archived_history).unwrap(), history);
    assert_eq!(
        std::fs::metadata(&archived_history)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o440
    );
    assert_eq!(
        std::fs::read(workplace.join(root_history_path)).unwrap(),
        root_history
    );
    let receipt: Value =
        serde_json::from_slice(&std::fs::read(workplace.join(DESTINATION).join(RECEIPT)).unwrap())
            .unwrap();
    assert_eq!(receipt["historical_records"], final_records);
    let cancelled = json(&archive(
        &workplace,
        &gh,
        &[
            "archive",
            DONE,
            "--manifest",
            manifest.to_str().unwrap(),
            "--cancel",
        ],
    ));
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(
        std::fs::read(workplace.join(&history_path)).unwrap(),
        history
    );
    assert_eq!(
        std::fs::metadata(workplace.join(&history_path))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o440
    );
    assert_eq!(
        std::fs::read(workplace.join(root_history_path)).unwrap(),
        root_history
    );
    assert_eq!(
        std::fs::read(workplace.join(DONE).join(MANIFEST)).unwrap(),
        original_manifest
    );
    assert!(!workplace.join(DESTINATION).exists());
}

#[test]
fn archive_historical_acknowledgments_cannot_exempt_scripts_controls_or_invalid_paths() {
    for case in [
        "missing",
        "directory",
        "source",
        "executable",
        "shebang",
        "manifest",
        "git-ignore",
        "uppercase-git-control",
        "uppercase-workspace-control",
        "git-hook",
        "symlink",
        "escape",
    ] {
        let (fixture, merged) = managed_fixture(false);
        let gh = fake_gh(&fixture, &merged, false);
        let (workplace, manifest) = organizer(&fixture, &[DONE, DESTINATION]);
        let source = workplace.join(DONE);
        let cache = source.join(".cache");
        std::fs::create_dir(&cache).unwrap();
        ignore_fixture_paths(
            &workplace,
            &[format!("{DONE}/.cache/"), format!("{DONE}/.git/")],
        );
        let mut record = format!("{DONE}/.cache/history.log");
        let contents = format!("historical cwd: {}\n", source.display());
        match case {
            "missing" => (),
            "directory" => std::fs::create_dir(workplace.join(&record)).unwrap(),
            "source" => {
                record = format!("{DONE}/.cache/run.py");
                std::fs::write(
                    workplace.join(&record),
                    format!("data = '{}/result.md'\n", source.display()),
                )
                .unwrap();
            }
            "executable" => {
                std::fs::write(workplace.join(&record), &contents).unwrap();
                std::fs::set_permissions(
                    workplace.join(&record),
                    std::fs::Permissions::from_mode(0o755),
                )
                .unwrap();
            }
            "shebang" => std::fs::write(
                workplace.join(&record),
                format!("#!/bin/sh\ncat '{}/result.md'\n", source.display()),
            )
            .unwrap(),
            "manifest" => record = format!("{DONE}/{MANIFEST}"),
            "git-ignore" => record = ".gitignore".to_owned(),
            "uppercase-git-control" | "uppercase-workspace-control" => {
                use std::os::unix::fs::MetadataExt;
                let original = if case == "uppercase-git-control" {
                    ".gitignore"
                } else {
                    ".workspace-mgr.toml"
                };
                record = original.to_ascii_uppercase();
                let alias = workplace.join(&record);
                if alias.exists() {
                    let original = std::fs::metadata(workplace.join(original)).unwrap();
                    let alias = std::fs::metadata(&alias).unwrap();
                    assert_eq!((alias.dev(), alias.ino()), (original.dev(), original.ino()));
                } else {
                    // Case-sensitive hosts exercise the same protected spelling;
                    // macOS also verifies the real same-inode alias above.
                    std::fs::write(&alias, &contents).unwrap();
                }
            }
            "git-hook" => {
                record = format!("{DONE}/.git/hooks/history.log");
                std::fs::create_dir_all(source.join(".git/hooks")).unwrap();
                std::fs::write(workplace.join(&record), &contents).unwrap();
            }
            "symlink" => {
                let external = fixture.root.join("external-history.log");
                std::fs::write(&external, &contents).unwrap();
                std::os::unix::fs::symlink(&external, workplace.join(&record)).unwrap();
            }
            "escape" => {
                std::fs::write(fixture.root.join("external-history.log"), &contents).unwrap();
                record = "../external-history.log".to_owned();
            }
            _ => unreachable!(),
        }
        let before = std::fs::read(source.join(MANIFEST)).unwrap();
        let index = git(&workplace, ["write-tree"]).stdout;
        for dry_run in [true, false] {
            let mut args = vec![
                "archive",
                DONE,
                "--manifest",
                manifest.to_str().unwrap(),
                "--historical-record",
                &record,
            ];
            if dry_run {
                args.push("--dry-run");
            }
            let env = archive_environment(&gh);
            let env = env
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect::<Vec<_>>();
            let output = workspace_env_unchecked(&workplace, &args, &env);
            assert_eq!(
                output.status.code(),
                Some(2),
                "case {case}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!workplace.join(DESTINATION).exists(), "case {case}");
            assert!(!source.join(RECEIPT).exists(), "case {case}");
            assert_eq!(
                std::fs::read(source.join(MANIFEST)).unwrap(),
                before,
                "case {case}"
            );
            assert_eq!(git(&workplace, ["write-tree"]).stdout, index, "case {case}");
        }
    }
}

#[test]
fn archive_cannot_acknowledge_a_historical_record_executed_through_a_script_variable() {
    let (fixture, merged) = managed_fixture(false);
    let gh = fake_gh(&fixture, &merged, false);
    let (workplace, manifest) = organizer(&fixture, &[DONE, DESTINATION]);
    let source = workplace.join(DONE);
    std::fs::create_dir(source.join(".cache")).unwrap();
    let record_path = format!("{DONE}/.cache/history.log");
    let record = format!(
        "from pathlib import Path\nprint(Path('{}/result.md').read_text())\n",
        source.display()
    );
    let script = "from pathlib import Path\nrecord = Path(__file__).parent / '.cache/history.log'\nexec(record.read_text())\n";
    std::fs::write(workplace.join(&record_path), &record).unwrap();
    std::fs::write(source.join("run.py"), script).unwrap();
    ignore_fixture_paths(
        &workplace,
        &[format!("{DONE}/.cache/"), format!("{DONE}/run.py")],
    );
    let original = std::fs::read(source.join(MANIFEST)).unwrap();
    for dry_run in [true, false] {
        let mut args = vec![
            "archive",
            DONE,
            "--manifest",
            manifest.to_str().unwrap(),
            "--historical-record",
            &record_path,
        ];
        if dry_run {
            args.push("--dry-run");
        }
        rejected(&workplace, &gh, &args, "historical record");
        assert_eq!(std::fs::read(source.join(MANIFEST)).unwrap(), original);
        assert_eq!(
            std::fs::read_to_string(workplace.join(&record_path)).unwrap(),
            record
        );
        assert!(!workplace.join(DESTINATION).exists());
        assert!(!source.join(RECEIPT).exists());
    }
}

#[test]
fn archive_checks_cross_task_dependencies_against_the_entire_move_graph() {
    for (label, script, paths) in [
        (
            "pathlib sibling across months",
            format!(
                "from pathlib import Path\ninput_path = Path(__file__).resolve().parents[1] / '{RELATED}' / 'input.tsv'\n"
            ),
            vec![DONE, RELATED],
        ),
        (
            "root-derived sibling across months",
            format!(
                "from pathlib import Path\nROOT = Path(__file__).resolve().parents[1]\ninput_path = ROOT / '{RELATED}/input.tsv'\n"
            ),
            vec![DONE, RELATED],
        ),
        (
            "relative sibling across months",
            format!("from pathlib import Path\ninput_path = Path('../{RELATED}/input.tsv')\n"),
            vec![DONE, RELATED],
        ),
        (
            "moving owner with stationary dependency",
            format!(
                "from pathlib import Path\ninput_path = Path(__file__).resolve().parents[1] / '{RELATED}' / 'input.tsv'\n"
            ),
            vec![DONE],
        ),
        (
            "stationary owner with moving dependency",
            format!(
                "from pathlib import Path\ninput_path = Path(__file__).resolve().parents[1] / '{RELATED}' / 'input.tsv'\n"
            ),
            vec![RELATED],
        ),
    ] {
        let (fixture, gh) = related_completed_fixture();
        let (workplace, manifest) =
            organizer(&fixture, &[DONE, DESTINATION, RELATED, RELATED_DESTINATION]);
        let script_path = workplace.join(DONE).join("run.py");
        std::fs::write(&script_path, &script).unwrap();
        ignore_fixture_paths(&workplace, &[format!("{DONE}/run.py")]);
        let manifests =
            [DONE, RELATED].map(|path| std::fs::read(workplace.join(path).join(MANIFEST)).unwrap());
        let index = git(&workplace, ["write-tree"]).stdout;
        for dry_run in [true, false] {
            let mut args = vec!["archive"];
            args.extend(paths.iter().copied());
            args.extend(["--manifest", manifest.to_str().unwrap()]);
            if dry_run {
                args.push("--dry-run");
            }
            rejected(&workplace, &gh, &args, "cross-task path dependencies");
            for (path, before) in [DONE, RELATED].into_iter().zip(&manifests) {
                assert_eq!(
                    std::fs::read(workplace.join(path).join(MANIFEST)).unwrap(),
                    *before,
                    "{label}"
                );
                assert!(!workplace.join(path).join(RECEIPT).exists(), "{label}");
            }
            assert!(!workplace.join(DESTINATION).exists(), "{label}");
            assert!(!workplace.join(RELATED_DESTINATION).exists(), "{label}");
            assert_eq!(
                std::fs::read_to_string(&script_path).unwrap(),
                script,
                "{label}"
            );
            assert_eq!(git(&workplace, ["write-tree"]).stdout, index, "{label}");
        }
    }
}

#[test]
fn archive_preserves_stationary_task_symlink_dependencies_when_refusing_a_target_move() {
    let (fixture, gh) = related_completed_fixture();
    let (workplace, manifest) = organizer(&fixture, &[RELATED, RELATED_DESTINATION]);
    let link_relative = format!("{DONE}/input-link");
    let link = workplace.join(&link_relative);
    let target = PathBuf::from(format!("../{RELATED}/input.tsv"));
    std::os::unix::fs::symlink(&target, &link).unwrap();
    ignore_fixture_paths(&workplace, &[link_relative]);
    let original_index = git(&workplace, ["write-tree"]).stdout;
    let original_manifests =
        [DONE, RELATED].map(|path| std::fs::read(workplace.join(path).join(MANIFEST)).unwrap());
    let input = std::fs::read(workplace.join(RELATED).join("input.tsv")).unwrap();
    assert_eq!(std::fs::read(&link).unwrap(), input);
    for dry_run in [true, false] {
        let mut args = vec!["archive", RELATED, "--manifest", manifest.to_str().unwrap()];
        if dry_run {
            args.push("--dry-run");
        }
        rejected(&workplace, &gh, &args, "cross-task path dependencies");
        for (path, original) in [DONE, RELATED].into_iter().zip(&original_manifests) {
            assert!(workplace.join(path).is_dir());
            assert_eq!(
                std::fs::read(workplace.join(path).join(MANIFEST)).unwrap(),
                *original
            );
            assert!(!workplace.join(path).join(RECEIPT).exists());
        }
        assert!(!workplace.join(DESTINATION).exists());
        assert!(!workplace.join(RELATED_DESTINATION).exists());
        assert_eq!(git(&workplace, ["write-tree"]).stdout, original_index);
        assert_eq!(std::fs::read_link(&link).unwrap(), target);
        assert_eq!(std::fs::read(&link).unwrap(), input);
        assert_eq!(
            std::fs::read(workplace.join(RELATED).join("input.tsv")).unwrap(),
            input
        );
    }
}

#[test]
fn archive_batch_preserves_provable_file_relative_sibling_dependencies_in_the_same_month() {
    let (fixture, gh) = related_completed_fixture_for(RELATED_SAME_MONTH);
    let (workplace, manifest) = organizer(
        &fixture,
        &[
            DONE,
            DESTINATION,
            RELATED_SAME_MONTH,
            RELATED_SAME_MONTH_DESTINATION,
        ],
    );
    let script = format!(
        "from pathlib import Path\ninput_path = Path(__file__).resolve().parents[1] / '{RELATED_SAME_MONTH}' / 'input.tsv'\n",
    );
    std::fs::write(workplace.join(DONE).join("run.py"), &script).unwrap();
    ignore_fixture_paths(&workplace, &[format!("{DONE}/run.py")]);
    for dry_run in [true, false] {
        let mut args = vec![
            "archive",
            DONE,
            RELATED_SAME_MONTH,
            "--manifest",
            manifest.to_str().unwrap(),
        ];
        if dry_run {
            args.push("--dry-run");
        }
        let report = json(&archive(&workplace, &gh, &args));
        assert_eq!(
            report["status"],
            if dry_run { "dry_run" } else { "archived" }
        );
        assert_eq!(report["tasks"].as_array().unwrap().len(), 2);
    }
    assert_eq!(
        std::fs::read_to_string(workplace.join(DESTINATION).join("run.py")).unwrap(),
        script,
    );
    let after_anchor = workplace
        .join(DESTINATION)
        .parent()
        .unwrap()
        .join(RELATED_SAME_MONTH)
        .join("input.tsv");
    assert_eq!(
        std::fs::read_to_string(after_anchor).unwrap(),
        "value\n17\n"
    );
}

#[test]
fn archive_does_not_reuse_a_file_relative_anchor_after_an_inline_reassignment() {
    let (fixture, gh) = related_completed_fixture_for(RELATED_SAME_MONTH);
    let (workplace, manifest) = organizer(
        &fixture,
        &[
            DONE,
            DESTINATION,
            RELATED_SAME_MONTH,
            RELATED_SAME_MONTH_DESTINATION,
        ],
    );
    let script = format!(
        "from pathlib import Path\nROOT = Path(__file__).resolve().parents[1]\nif True: ROOT = Path('{}')\ninput_path = ROOT / '{RELATED_SAME_MONTH}' / 'input.tsv'\n",
        workplace.display(),
    );
    std::fs::write(workplace.join(DONE).join("run.py"), &script).unwrap();
    ignore_fixture_paths(&workplace, &[format!("{DONE}/run.py")]);
    for dry_run in [true, false] {
        let mut args = vec![
            "archive",
            DONE,
            RELATED_SAME_MONTH,
            "--manifest",
            manifest.to_str().unwrap(),
        ];
        if dry_run {
            args.push("--dry-run");
        }
        rejected(&workplace, &gh, &args, "cross-task path dependencies");
        assert!(workplace.join(DONE).is_dir());
        assert!(workplace.join(RELATED_SAME_MONTH).is_dir());
        assert!(!workplace.join(DESTINATION).exists());
        assert!(!workplace.join(RELATED_SAME_MONTH_DESTINATION).exists());
        assert!(!workplace.join(DONE).join(RECEIPT).exists());
        assert!(!workplace.join(RELATED_SAME_MONTH).join(RECEIPT).exists());
        assert_eq!(
            std::fs::read_to_string(workplace.join(DONE).join("run.py")).unwrap(),
            script
        );
    }
}

#[test]
fn archive_refuses_uncertain_dynamic_cross_task_paths_without_a_literal_task_basename() {
    let (fixture, gh) = related_completed_fixture();
    let (workplace, manifest) =
        organizer(&fixture, &[DONE, DESTINATION, RELATED, RELATED_DESTINATION]);
    let script = "from pathlib import Path\nROOT = Path(__file__).resolve().parents[1]\nTARGET = ''.join(['20260812', '-120000-', 'related'])\ninput_path = ROOT / TARGET / 'input.tsv'\n";
    assert!(!script.contains(RELATED));
    std::fs::write(workplace.join(DONE).join("run.py"), script).unwrap();
    ignore_fixture_paths(&workplace, &[format!("{DONE}/run.py")]);
    let index = git(&workplace, ["write-tree"]).stdout;
    for dry_run in [true, false] {
        let mut args = vec![
            "archive",
            DONE,
            RELATED,
            "--manifest",
            manifest.to_str().unwrap(),
        ];
        if dry_run {
            args.push("--dry-run");
        }
        rejected(&workplace, &gh, &args, "cross-task path dependencies");
        assert!(workplace.join(DONE).is_dir());
        assert!(workplace.join(RELATED).is_dir());
        assert!(!workplace.join(DESTINATION).exists());
        assert!(!workplace.join(RELATED_DESTINATION).exists());
        assert!(!workplace.join(DONE).join(RECEIPT).exists());
        assert!(!workplace.join(RELATED).join(RECEIPT).exists());
        assert_eq!(git(&workplace, ["write-tree"]).stdout, index);
    }
}

#[test]
fn archive_batch_accepts_repaired_task_local_dependencies_without_rewriting_scripts() {
    let (fixture, gh) = related_completed_fixture();
    let (workplace, manifest) =
        organizer(&fixture, &[DONE, DESTINATION, RELATED, RELATED_DESTINATION]);
    let script =
        "from pathlib import Path\ninput_path = Path(__file__).resolve().parent / 'result.md'\n";
    std::fs::write(workplace.join(DONE).join("run.py"), script).unwrap();
    ignore_fixture_paths(&workplace, &[format!("{DONE}/run.py")]);
    for dry_run in [true, false] {
        let mut args = vec![
            "archive",
            DONE,
            RELATED,
            "--manifest",
            manifest.to_str().unwrap(),
        ];
        if dry_run {
            args.push("--dry-run");
        }
        let report = json(&archive(&workplace, &gh, &args));
        assert_eq!(
            report["status"],
            if dry_run { "dry_run" } else { "archived" }
        );
        assert_eq!(report["tasks"].as_array().unwrap().len(), 2);
    }
    assert_eq!(
        std::fs::read_to_string(workplace.join(DESTINATION).join("run.py")).unwrap(),
        script
    );
    assert_eq!(
        std::fs::read_to_string(workplace.join(RELATED_DESTINATION).join("input.tsv")).unwrap(),
        "value\n17\n"
    );
    assert!(!workplace.join(DONE).exists());
    assert!(!workplace.join(RELATED).exists());
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
fn archive_reports_ordinary_script_and_readme_paths_before_any_move() {
    let (fixture, merged) = managed_fixture(false);
    let gh = fake_gh(&fixture, &merged, false);
    let (workplace, manifest) = organizer(&fixture, &[DONE, DESTINATION]);
    let source = workplace.join(DONE);
    let cache = source.join(".cache");
    std::fs::create_dir(&cache).unwrap();
    let script = format!("#!/bin/sh\ncat '{}/result.md'\n", source.display());
    let readme = format!("# Run\nsh {DONE}/.cache/run.sh\n");
    std::fs::write(cache.join("run.sh"), &script).unwrap();
    std::fs::write(cache.join("README.md"), &readme).unwrap();
    let exclude = workplace.join(".git/info/exclude");
    let old_exclude = std::fs::read_to_string(&exclude).unwrap();
    std::fs::write(&exclude, format!("{old_exclude}\n/{DONE}/.cache/\n")).unwrap();
    let before = std::fs::read(source.join(MANIFEST)).unwrap();
    for dry_run in [true, false] {
        let mut args = vec!["archive", DONE, "--manifest", manifest.to_str().unwrap()];
        if dry_run {
            args.push("--dry-run");
        }
        let env = archive_environment(&gh);
        let env = env
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect::<Vec<_>>();
        let output = workspace_env_unchecked(&workplace, &args, &env);
        assert_eq!(output.status.code(), Some(2));
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("ordinary file path references"), "{error}");
        assert!(error.contains("run.sh:2"), "{error}");
        assert!(error.contains("README.md:2"), "{error}");
        assert!(error.contains("Derive task-local inputs"), "{error}");
        assert!(!workplace.join(DESTINATION).exists());
        assert!(!source.join(RECEIPT).exists());
        assert_eq!(std::fs::read(source.join(MANIFEST)).unwrap(), before);
        assert_eq!(
            std::fs::read_to_string(cache.join("run.sh")).unwrap(),
            script
        );
        assert_eq!(
            std::fs::read_to_string(cache.join("README.md")).unwrap(),
            readme
        );
    }
    std::fs::write(
        cache.join("run.sh"),
        "#!/bin/sh\ncd -- \"$(dirname -- \"$0\")/..\"\ncat result.md\n",
    )
    .unwrap();
    std::fs::write(
        cache.join("README.md"),
        "From the task directory, run `sh .cache/run.sh`.\n",
    )
    .unwrap();
    let report = json(&archive(
        &workplace,
        &gh,
        &["archive", DONE, "--manifest", manifest.to_str().unwrap()],
    ));
    assert_eq!(report["status"], "archived");
    assert!(workplace.join(DESTINATION).join(".cache/run.sh").is_file());
}

#[test]
fn archive_refuses_external_git_administration_before_preview_or_apply_mutations() {
    let (fixture, merged) = managed_fixture(false);
    let gh = fake_gh(&fixture, &merged, false);
    let (workplace, manifest) = organizer(&fixture, &[DONE, DESTINATION]);
    let external = fixture.root.join("external-nested-origin");
    std::fs::create_dir(&external).unwrap();
    git(&external, ["init", "-b", "main"]);
    configure_git(&external);
    std::fs::write(external.join("README.md"), "Nested history\n").unwrap();
    git(&external, ["add", "README.md"]);
    git(&external, ["commit", "-m", "Nested initial history"]);
    let checkout = workplace.join(DONE).join("ignored-checkout");
    git(
        &external,
        ["worktree", "add", "--detach", checkout.to_str().unwrap()],
    );
    let exclude = workplace.join(".git/info/exclude");
    let old_exclude = std::fs::read_to_string(&exclude).unwrap();
    std::fs::write(
        &exclude,
        format!("{old_exclude}\n/{DONE}/ignored-checkout/\n"),
    )
    .unwrap();
    let admin = PathBuf::from(
        String::from_utf8(git(&checkout, ["rev-parse", "--absolute-git-dir"]).stdout)
            .unwrap()
            .trim(),
    );
    let backlink = std::fs::read(admin.join("gitdir")).unwrap();
    let common = std::fs::read(admin.join("commondir")).unwrap();
    let original_manifest = std::fs::read(workplace.join(DONE).join(MANIFEST)).unwrap();
    let original_index = git(&workplace, ["write-tree"]).stdout;
    for dry_run in [true, false] {
        let mut args = vec!["archive", DONE, "--manifest", manifest.to_str().unwrap()];
        if dry_run {
            args.push("--dry-run");
        }
        rejected(&workplace, &gh, &args, "outside the task scope");
        assert_eq!(std::fs::read(admin.join("gitdir")).unwrap(), backlink);
        assert_eq!(std::fs::read(admin.join("commondir")).unwrap(), common);
        assert_eq!(
            std::fs::read(workplace.join(DONE).join(MANIFEST)).unwrap(),
            original_manifest
        );
        assert_eq!(git(&workplace, ["write-tree"]).stdout, original_index);
        assert!(checkout.is_dir());
        assert!(!workplace.join(DESTINATION).exists());
        assert!(!workplace.join(DONE).join(RECEIPT).exists());
    }
}

#[test]
fn archive_new_protocol_requires_070_before_creating_local_attempt_state() {
    let (fixture, merged) = managed_fixture(false);
    let gh = fake_gh(&fixture, &merged, false);
    let output = workspace_env_unchecked(
        &fixture.shared,
        ["archive", DONE, "--dry-run"],
        &[
            ("WORKSPACE_MGR_TEST_GH", gh.to_str().unwrap()),
            (CLI_VERSION_ENV, "0.6.0"),
        ],
    );
    assert_eq!(output.status.code(), Some(2));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("archive protocol requires workspace-mgr 0.7.0"),
        "{error}"
    );
    assert!(fixture.shared.join(DONE).is_dir());
    assert!(!fixture.shared.join(DESTINATION).exists());
    assert!(!fixture.shared.join(DONE).join(RECEIPT).exists());
    assert!(
        !fixture
            .shared
            .join(".workspace-mgr/local/archive-attempts")
            .exists()
    );
}

#[test]
fn archive_and_cancel_keep_internal_absolute_git_worktrees_operational() {
    let (fixture, merged) = managed_fixture(false);
    let gh = fake_gh(&fixture, &merged, false);
    let (workplace, manifest) = organizer(&fixture, &[DONE, DESTINATION]);
    let nested = workplace.join(DONE).join("nested-main");
    let linked = workplace.join(DONE).join("nested-linked");
    std::fs::create_dir(&nested).unwrap();
    git(&nested, ["init", "-b", "main"]);
    configure_git(&nested);
    std::fs::write(nested.join("README.md"), "Nested history\n").unwrap();
    git(&nested, ["add", "README.md"]);
    git(&nested, ["commit", "-m", "Nested initial history"]);
    git(
        &nested,
        ["worktree", "add", "--detach", linked.to_str().unwrap()],
    );
    let original_pointer = std::fs::read(linked.join(".git")).unwrap();
    let exclude = workplace.join(".git/info/exclude");
    let old_exclude = std::fs::read_to_string(&exclude).unwrap();
    std::fs::write(
        &exclude,
        format!("{old_exclude}\n/{DONE}/nested-main/\n/{DONE}/nested-linked/\n"),
    )
    .unwrap();
    archive(
        &workplace,
        &gh,
        &["archive", DONE, "--manifest", manifest.to_str().unwrap()],
    );
    let moved = workplace.join(DESTINATION).join("nested-linked");
    let top = String::from_utf8(git(&moved, ["rev-parse", "--show-toplevel"]).stdout).unwrap();
    assert_eq!(top.trim(), moved.canonicalize().unwrap().to_str().unwrap());
    git(&moved, ["status", "--porcelain"]);
    archive(
        &workplace,
        &gh,
        &[
            "archive",
            DESTINATION,
            "--cancel",
            "--manifest",
            manifest.to_str().unwrap(),
        ],
    );
    assert_eq!(
        std::fs::read(linked.join(".git")).unwrap(),
        original_pointer
    );
    git(&linked, ["status", "--porcelain"]);
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
    // This historical file has no schema, ID, path, branch, or any other
    // current field. It is intentionally neither TOML nor valid UTF-8.
    std::fs::write(&manifest, b"\0opaque pre-tool configuration\xff\n").unwrap();
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
    let old_config_blob = oid(
        &fixture.seed,
        &format!("{original_merge}:{DONE}/{MANIFEST}"),
    );
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
    forbid_historical_config_reads(&fixture, &old_config_blob);
    let key = format!("commit:{migration_merge}");
    let original_key = format!("commit:{original_merge}");
    let original_review = pr(
        "MERGED",
        1,
        "historical/completed",
        &original_merge,
        &original_head,
    );
    let mut requests = BTreeMap::from([
        ("historical/completed", vec![original_review.clone()]),
        (original_key.as_str(), vec![original_review]),
    ]);
    if reviewed {
        let migration_review = pr(
            "MERGED",
            2,
            "codex/infra-schema",
            &migration_merge,
            &migration_head,
        );
        requests.insert(&key, vec![migration_review.clone()]);
        requests.insert("codex/infra-schema", vec![migration_review]);
    }
    let gh = write_gh(&fixture, &requests);
    (fixture, gh)
}

#[test]
fn archive_follows_reviewed_task_updates_without_reading_historical_configuration() {
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
    assert!(!fixture.root.join("historical-config-read").exists());
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

struct CompletionFixture {
    git: GitFixture,
    requests: BTreeMap<String, Vec<Value>>,
    historical_commits: Vec<String>,
}

impl CompletionFixture {
    fn hosting(&self) -> PathBuf {
        let requests = self
            .requests
            .iter()
            .map(|(branch, rows)| (branch.as_str(), rows.clone()))
            .collect();
        write_gh_without_historical_queries(&self.git, &requests, &self.historical_commits)
    }

    fn assert_no_historical_lookup(&self) {
        assert!(
            !self.git.root.join("blocked-historical-query").exists(),
            "archive must use the current completion checkpoint instead of rediscovering old reviews"
        );
        assert!(
            !self.git.root.join("historical-config-read").exists(),
            "archive must never read the historical task configuration blob"
        );
    }
}

fn completion_fixture() -> CompletionFixture {
    let (fixture, gh) = migrated_fixture(true);
    let original_merge = oid(&fixture.seed, "codex/infra-schema^");
    let original_head = oid(&fixture.seed, "historical/completed");
    let migration_merge = oid(&fixture.seed, "main");
    let migration_head = oid(&fixture.seed, "codex/infra-schema");
    let manifest = fixture.shared.join(DONE).join(MANIFEST);
    archive(
        &fixture.shared,
        &gh,
        &["task", "upgrade", "--manifest", manifest.to_str().unwrap()],
    );
    let upgraded: toml::Value =
        toml::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap();
    assert_eq!(upgraded["schema_version"].as_integer(), Some(4));
    let checkpoint = &upgraded["archive_completion"];
    assert_eq!(checkpoint["schema_version"].as_integer(), Some(1));
    assert_eq!(checkpoint["task_id"].as_str(), Some(DONE));
    assert_eq!(
        checkpoint["checkpoint_commit"].as_str(),
        Some(migration_merge.as_str())
    );
    assert_eq!(checkpoint["checkpoint_path"].as_str(), Some(DONE));
    assert_eq!(checkpoint["reviews"].as_array().unwrap().len(), 2);
    for branch in [
        "historical/completed",
        "codex/completed",
        "codex/infra-schema",
    ] {
        assert!(
            checkpoint["branches"]
                .as_array()
                .unwrap()
                .iter()
                .any(|stored| { stored.as_str() == Some(branch) })
        );
    }

    // Completion metadata becomes shared evidence through its own review,
    // just as the historical format migration did. No historical manifest
    // has to acquire fields introduced by a future tool release.
    git(&fixture.seed, ["switch", "-c", "codex/infra-completion"]);
    std::fs::copy(&manifest, fixture.seed.join(DONE).join(MANIFEST)).unwrap();
    git(&fixture.seed, ["add", DONE]);
    git(
        &fixture.seed,
        [
            "commit",
            "-m",
            "Retain verified task completion information",
        ],
    );
    let completion_head = oid(&fixture.seed, "HEAD");
    git(&fixture.seed, ["push", "origin", "codex/infra-completion"]);
    git(&fixture.seed, ["switch", "main"]);
    git(
        &fixture.seed,
        ["merge", "--squash", "codex/infra-completion"],
    );
    git(
        &fixture.seed,
        ["commit", "-m", "Merge the reviewed completion checkpoint"],
    );
    let completion_merge = oid(&fixture.seed, "HEAD");
    git(&fixture.seed, ["push", "origin", "main"]);
    let path = format!("{DONE}/{MANIFEST}");
    git(&fixture.shared, ["restore", "--", &path]);
    git(&fixture.shared, ["pull", "--ff-only", "origin", "main"]);
    CompletionFixture {
        git: fixture,
        requests: BTreeMap::from([
            (
                "historical/completed".to_owned(),
                vec![pr(
                    "MERGED",
                    1,
                    "historical/completed",
                    &original_merge,
                    &original_head,
                )],
            ),
            (
                "codex/infra-schema".to_owned(),
                vec![pr(
                    "MERGED",
                    2,
                    "codex/infra-schema",
                    &migration_merge,
                    &migration_head,
                )],
            ),
            (
                "codex/infra-completion".to_owned(),
                vec![pr(
                    "MERGED",
                    3,
                    "codex/infra-completion",
                    &completion_merge,
                    &completion_head,
                )],
            ),
            (
                format!("commit:{completion_merge}"),
                vec![pr(
                    "MERGED",
                    3,
                    "codex/infra-completion",
                    &completion_merge,
                    &completion_head,
                )],
            ),
        ]),
        historical_commits: vec![original_merge, migration_merge],
    }
}

#[test]
fn task_upgrade_preserves_current_metadata_and_backfills_completion_once() {
    let (fixture, gh) = migrated_fixture(true);
    let manifest = fixture.shared.join(DONE).join(MANIFEST);
    let before = std::fs::read(&manifest).unwrap();
    let remote_before = oid(&fixture.remote, "refs/heads/main");
    let original: toml::Value = toml::from_str(std::str::from_utf8(&before).unwrap()).unwrap();
    archive(
        &fixture.shared,
        &gh,
        &[
            "task",
            "upgrade",
            "--manifest",
            manifest.to_str().unwrap(),
            "--dry-run",
        ],
    );
    assert_eq!(std::fs::read(&manifest).unwrap(), before);
    assert!(
        git(&fixture.shared, ["status", "--porcelain"])
            .stdout
            .is_empty()
    );
    archive(
        &fixture.shared,
        &gh,
        &["task", "upgrade", "--manifest", manifest.to_str().unwrap()],
    );
    let upgraded = std::fs::read(&manifest).unwrap();
    let mut modern: toml::Value = toml::from_str(std::str::from_utf8(&upgraded).unwrap()).unwrap();
    assert_eq!(modern["schema_version"].as_integer(), Some(4));
    assert!(
        modern
            .as_table_mut()
            .unwrap()
            .remove("archive_completion")
            .is_some()
    );
    modern["schema_version"] = original["schema_version"].clone();
    assert_eq!(
        modern, original,
        "upgrade must retain task identity, scopes, and approval"
    );
    archive(
        &fixture.shared,
        &gh,
        &["task", "upgrade", "--manifest", manifest.to_str().unwrap()],
    );
    assert_eq!(std::fs::read(&manifest).unwrap(), upgraded);
    assert_eq!(oid(&fixture.remote, "refs/heads/main"), remote_before);
    assert!(!fixture.root.join("historical-config-read").exists());
}

#[test]
fn archive_uses_current_completion_metadata_without_reading_any_historical_configuration() {
    let fixture = completion_fixture();
    let gh = fixture.hosting();
    let preview = json(&archive(
        &fixture.git.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
    ));
    assert_eq!(preview["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(preview["tasks"][0]["pull_request"]["number"], 3);
    let (workplace, manifest) = organizer(&fixture.git, &[DONE, DESTINATION]);
    let result = json(&archive(
        &workplace,
        &gh,
        &["archive", DONE, "--manifest", manifest.to_str().unwrap()],
    ));
    assert_eq!(result["status"], "archived");
    assert_eq!(
        std::fs::read_to_string(workplace.join(DESTINATION).join("result.md")).unwrap(),
        "retained result\n"
    );
    assert!(!workplace.join(DONE).exists());
    fixture.assert_no_historical_lookup();
}

#[test]
fn completion_checkpoint_keeps_a_historical_branch_with_an_open_pr_active() {
    let mut fixture = completion_fixture();
    let head = oid(&fixture.git.remote, "refs/heads/historical/completed");
    let mut open = pr("OPEN", 4, "historical/completed", "", &head);
    open["baseRefName"] = value!("release");
    fixture
        .requests
        .get_mut("historical/completed")
        .unwrap()
        .push(open);
    let gh = fixture.hosting();
    rejected(
        &fixture.git.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
        "refuses active or unverified task",
    );
    assert!(fixture.git.shared.join(DONE).join(MANIFEST).is_file());
    assert!(!fixture.git.shared.join(DESTINATION).exists());
    fixture.assert_no_historical_lookup();
}

#[test]
fn completion_checkpoint_rejects_unmerged_local_and_remote_commits_even_with_identical_content() {
    for local in [true, false] {
        let fixture = completion_fixture();
        let repo = if local {
            &fixture.git.shared
        } else {
            &fixture.git.remote
        };
        configure_git(repo);
        let branch = "refs/heads/historical/completed";
        let head = oid(&fixture.git.remote, branch);
        let tree = oid(repo, &format!("{head}^{{tree}}"));
        let resumed = String::from_utf8(
            git(
                repo,
                [
                    "commit-tree",
                    &tree,
                    "-p",
                    &head,
                    "-m",
                    "Unmerged work after completion",
                ],
            )
            .stdout,
        )
        .unwrap()
        .trim()
        .to_owned();
        git(repo, ["update-ref", branch, &resumed]);
        let gh = fixture.hosting();
        rejected(
            &fixture.git.shared,
            &gh,
            &["archive", DONE, "--dry-run"],
            "refuses active or unverified task",
        );
        assert_eq!(oid(repo, branch), resumed);
        assert!(fixture.git.shared.join(DONE).join(MANIFEST).is_file());
        assert!(!fixture.git.shared.join(DESTINATION).exists());
        fixture.assert_no_historical_lookup();
    }
}

#[test]
fn completion_checkpoint_rejects_unreviewed_content_added_on_main_after_the_checkpoint() {
    let fixture = completion_fixture();
    std::fs::write(
        fixture.git.seed.join(DONE).join("result.md"),
        "Unreviewed direct main edit\n",
    )
    .unwrap();
    fixture
        .git
        .commit_seed("Change completed task without a review");
    git(&fixture.git.shared, ["pull", "--ff-only", "origin", "main"]);
    let gh = fixture.hosting();
    rejected(
        &fixture.git.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
        "refuses active or unverified task",
    );
    assert_eq!(
        std::fs::read_to_string(fixture.git.shared.join(DONE).join("result.md")).unwrap(),
        "Unreviewed direct main edit\n"
    );
    assert!(!fixture.git.shared.join(DESTINATION).exists());
    fixture.assert_no_historical_lookup();
}

#[test]
fn completion_checkpoint_accepts_later_reviewed_content_without_reopening_old_formats() {
    let mut fixture = completion_fixture();
    git(&fixture.git.seed, ["switch", "-c", "codex/completed"]);
    std::fs::write(
        fixture.git.seed.join(DONE).join("result.md"),
        "Further reviewed content after the checkpoint\n",
    )
    .unwrap();
    git(&fixture.git.seed, ["add", DONE]);
    git(
        &fixture.git.seed,
        ["commit", "-m", "Extend the completed task through review"],
    );
    let later_head = oid(&fixture.git.seed, "HEAD");
    git(&fixture.git.seed, ["push", "origin", "codex/completed"]);
    git(&fixture.git.seed, ["switch", "main"]);
    git(&fixture.git.seed, ["merge", "--squash", "codex/completed"]);
    git(
        &fixture.git.seed,
        ["commit", "-m", "Merge the reviewed task extension"],
    );
    let later_merge = oid(&fixture.git.seed, "HEAD");
    git(&fixture.git.seed, ["push", "origin", "main"]);
    git(&fixture.git.shared, ["pull", "--ff-only", "origin", "main"]);
    let review = pr("MERGED", 4, "codex/completed", &later_merge, &later_head);
    fixture
        .requests
        .insert("codex/completed".to_owned(), vec![review.clone()]);
    fixture
        .requests
        .insert(format!("commit:{later_merge}"), vec![review]);
    let gh = fixture.hosting();
    let preview = json(&archive(
        &fixture.git.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
    ));
    assert_eq!(preview["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(preview["tasks"][0]["pull_request"]["number"], 4);
    assert_eq!(
        preview["tasks"][0]["review_history"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    fixture.assert_no_historical_lookup();
}

#[test]
fn completion_checkpoint_rechecks_immutable_provider_evidence() {
    let mut fixture = completion_fixture();
    fixture.requests.get_mut("historical/completed").unwrap()[0]["mergedAt"] =
        value!("2026-07-13T20:00:00Z");
    let gh = fixture.hosting();
    rejected(
        &fixture.git.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
        "no longer matches its immutable provider evidence",
    );
    assert!(fixture.git.shared.join(DONE).join(MANIFEST).is_file());
    assert!(!fixture.git.shared.join(DESTINATION).exists());
    fixture.assert_no_historical_lookup();
}

#[test]
fn completion_checkpoint_rejects_a_current_record_bound_to_the_wrong_task_tree() {
    let fixture = completion_fixture();
    let manifest = fixture.git.shared.join(DONE).join(MANIFEST);
    let mut current: toml::Value =
        toml::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap();
    current["archive_completion"]["checkpoint_tree"] = toml::Value::String("0".repeat(40));
    std::fs::write(&manifest, toml::to_string_pretty(&current).unwrap()).unwrap();
    let before = std::fs::read(&manifest).unwrap();
    let gh = fixture.hosting();
    rejected(
        &fixture.git.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
        "does not match its published task tree",
    );
    assert_eq!(std::fs::read(&manifest).unwrap(), before);
    assert!(!fixture.git.shared.join(DESTINATION).exists());
    fixture.assert_no_historical_lookup();
}

#[test]
fn archive_rejects_a_local_completion_upgrade_until_it_has_been_reviewed() {
    let (fixture, gh) = migrated_fixture(true);
    let manifest = fixture.shared.join(DONE).join(MANIFEST);
    archive(
        &fixture.shared,
        &gh,
        &["task", "upgrade", "--manifest", manifest.to_str().unwrap()],
    );
    let before = std::fs::read(&manifest).unwrap();
    rejected(
        &fixture.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
        "refuses staged, modified, or untracked overlays",
    );
    assert_eq!(std::fs::read(&manifest).unwrap(), before);
    assert!(!fixture.shared.join(DESTINATION).exists());
    assert!(!fixture.root.join("historical-config-read").exists());
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

fn directly_imported_adoption_fixture(
    adoption_reviewed: bool,
) -> (CompletionFixture, PathBuf, PathBuf) {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["init"]);
    fixture.commit_seed("Initialize managed workspace");
    std::fs::create_dir_all(fixture.seed.join(DONE)).unwrap();
    std::fs::write(
        fixture.seed.join(DONE).join("README.md"),
        "# Imported task\n",
    )
    .unwrap();
    std::fs::write(
        fixture.seed.join(DONE).join("result.md"),
        "directly imported result\n",
    )
    .unwrap();
    fixture.commit_seed("Directly import historical task without a PR");
    let imported = oid(&fixture.seed, "HEAD");

    // This merged review explicitly accepts the existing legacy contents.
    // It does not rewrite the original direct-import commit or task files.
    git(&fixture.seed, ["switch", "-c", "legacy/completed"]);
    std::fs::write(
        fixture.seed.join("legacy-review.md"),
        "Accept the imported task\n",
    )
    .unwrap();
    git(&fixture.seed, ["add", "legacy-review.md"]);
    git(
        &fixture.seed,
        ["commit", "-m", "Review the imported legacy task"],
    );
    let legacy_head = oid(&fixture.seed, "HEAD");
    git(&fixture.seed, ["push", "origin", "legacy/completed"]);
    git(&fixture.seed, ["switch", "main"]);
    git(&fixture.seed, ["merge", "--squash", "legacy/completed"]);
    git(
        &fixture.seed,
        ["commit", "-m", "Merge the legacy acceptance review"],
    );
    let legacy_merge = oid(&fixture.seed, "HEAD");
    git(&fixture.seed, ["push", "origin", "main"]);
    fixture.clone_shared();
    let legacy = pr("MERGED", 1, "legacy/completed", &legacy_merge, &legacy_head);
    let gh = write_gh(
        &fixture,
        &BTreeMap::from([("legacy/completed", vec![legacy.clone()])]),
    );
    let (workplace, manifest) = organizer(&fixture, &[DONE, DESTINATION]);
    archive(
        &workplace,
        &gh,
        &[
            "task",
            "adopt",
            DONE,
            "--pull-request",
            "1",
            "--title",
            "Imported task",
            "--purpose",
            "Retain reviewed legacy content",
            "--manifest",
            manifest.to_str().unwrap(),
        ],
    );
    let publication = json(&workspace(
        &workplace,
        [
            "publish",
            "--manifest",
            manifest.to_str().unwrap(),
            "-m",
            "Publish adoption of directly imported task",
        ],
    ));
    let adoption_merge = publication["commit_oid"].as_str().unwrap().to_owned();
    git(&workplace, ["add", DONE]);
    git(&workplace, ["merge", "--ff-only", &adoption_merge]);
    git(&workplace, ["push", "origin", "main"]);
    let mut requests = BTreeMap::from([("legacy/completed".to_owned(), vec![legacy])]);
    if adoption_reviewed {
        let review = pr(
            "MERGED",
            2,
            "codex/infra-archive-completed",
            &adoption_merge,
            &adoption_merge,
        );
        requests.insert(
            "codex/infra-archive-completed".to_owned(),
            vec![review.clone()],
        );
        requests.insert(format!("commit:{adoption_merge}"), vec![review]);
    }
    (
        CompletionFixture {
            git: fixture,
            requests,
            historical_commits: vec![imported],
        },
        workplace,
        manifest,
    )
}

#[test]
fn reviewed_adoption_starts_completion_after_an_unreviewed_direct_import() {
    let (fixture, workplace, manifest) = directly_imported_adoption_fixture(true);
    let gh = fixture.hosting();
    let preview = json(&archive(&workplace, &gh, &["archive", DONE, "--dry-run"]));
    assert_eq!(preview["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(preview["tasks"][0]["pull_request"]["number"], 2);
    let result = json(&archive(
        &workplace,
        &gh,
        &["archive", DONE, "--manifest", manifest.to_str().unwrap()],
    ));
    assert_eq!(result["status"], "archived");
    assert_eq!(
        std::fs::read_to_string(workplace.join(DESTINATION).join("result.md")).unwrap(),
        "directly imported result\n",
    );
    assert!(!workplace.join(DONE).exists());
    fixture.assert_no_historical_lookup();
}

#[test]
fn adoption_boundary_requires_its_own_merged_review() {
    let (fixture, workplace, _) = directly_imported_adoption_fixture(false);
    let gh = fixture.hosting();
    rejected(
        &workplace,
        &gh,
        &["archive", DONE, "--dry-run"],
        "refuses active or unverified task",
    );
    assert!(workplace.join(DONE).join(MANIFEST).is_file());
    assert!(!workplace.join(DESTINATION).exists());
    fixture.assert_no_historical_lookup();
}

#[test]
fn reviewed_adoption_keeps_open_legacy_current_and_adoption_branches_active() {
    for branch in [
        "legacy/completed",
        "codex/completed",
        "codex/infra-archive-completed",
    ] {
        let (mut fixture, workplace, _) = directly_imported_adoption_fixture(true);
        let head = oid(&workplace, "HEAD");
        fixture
            .requests
            .entry(branch.to_owned())
            .or_default()
            .push(pr("OPEN", 3, branch, "", &head));
        let gh = fixture.hosting();
        rejected(
            &workplace,
            &gh,
            &["archive", DONE, "--dry-run"],
            if branch == "legacy/completed" {
                "legacy adoption refuses an open"
            } else {
                "refuses active or unverified task"
            },
        );
        assert!(workplace.join(DONE).is_dir());
        assert!(!workplace.join(DESTINATION).exists());
        fixture.assert_no_historical_lookup();
    }
}

#[test]
fn reviewed_adoption_does_not_accept_new_unreviewed_content_or_commits() {
    for branch_only in [false, true] {
        let (fixture, workplace, _) = directly_imported_adoption_fixture(true);
        if branch_only {
            git(&workplace, ["switch", "-c", "codex/completed"]);
            git(
                &workplace,
                [
                    "commit",
                    "--allow-empty",
                    "-m",
                    "Unmerged work after adoption",
                ],
            );
            git(&workplace, ["switch", "main"]);
        } else {
            std::fs::write(
                workplace.join(DONE).join("result.md"),
                "Unreviewed post-adoption work\n",
            )
            .unwrap();
            git(&workplace, ["add", DONE]);
            git(&workplace, ["commit", "-m", "Direct edit after adoption"]);
            git(&workplace, ["push", "origin", "main"]);
        }
        let gh = fixture.hosting();
        rejected(
            &workplace,
            &gh,
            &["archive", DONE, "--dry-run"],
            "refuses active or unverified task",
        );
        assert!(workplace.join(DONE).is_dir());
        assert!(!workplace.join(DESTINATION).exists());
        fixture.assert_no_historical_lookup();
    }
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
