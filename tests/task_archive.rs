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
const RELATED: &str = "20260812-120000-related";
const RELATED_DESTINATION: &str = "2026/08/20260812-120000-related";
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
    workspace(&fixture.seed, ["manage"]);
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
        "#!/usr/bin/env python3\nimport json, sys\nrequests = json.loads({literal})\nif sys.argv[1] == 'api':\n    commit = sys.argv[-1].split('/commits/')[1].split('/')[0]\n    if {blocked}:\n        open({query_marker}, 'w').write(commit)\n        sys.exit('Historical commit review lookup is forbidden after a completion checkpoint')\n    rows = requests.get('commit:' + commit, [])\n    rows = [dict(number=row['number'], html_url=row['url'], state='closed' if row['state'] != 'OPEN' else 'open', merged_at=row['mergedAt'], merge_commit_sha=(row['mergeCommit'] or {{}}).get('oid'), head={{'ref': row['headRefName'], 'sha': row['headRefOid'], 'repo': {{'full_name': 'owner/archive-fixture'}}}}, base={{'ref': row['baseRefName'], 'sha': row['headRefOid'], 'repo': {{'full_name': 'owner/archive-fixture'}}}}) for row in rows]\nelif sys.argv[2] == 'view':\n    rows = [row for group in requests.values() for row in group if row['number'] == int(sys.argv[3])]\n    print(json.dumps(rows[0] if rows else {{}}))\n    sys.exit(0)\nelse:\n    head = sys.argv[sys.argv.index('--head') + 1]\n    rows = requests.get(head, [])\nif '--base' in sys.argv:\n    base = sys.argv[sys.argv.index('--base') + 1]\n    rows = [row for row in rows if row['baseRefName'] == base]\nif '--state' in sys.argv:\n    state = sys.argv[sys.argv.index('--state') + 1]\n    if state != 'all':\n        rows = [row for row in rows if row['state'].lower() == state]\nif '--limit' in sys.argv:\n    rows = sorted(rows, key=lambda row: row['number'], reverse=True)[:int(sys.argv[sys.argv.index('--limit') + 1])]\nprint(json.dumps(rows))\n"
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
        "archive refuses pending task",
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

#[test]
fn batch_archive_preserves_published_git_and_lfs_bytes_for_managed_and_adopted_tasks() {
    const LEGACY: &str = "20260711-100000-legacy";
    const LEGACY_DEST: &str = "2026/07/20260711-100000-legacy";
    const PAYLOAD_BYTES: u64 = 10_485_777;
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["manage"]);
    for root in [LEGACY, DONE] {
        std::fs::create_dir_all(fixture.seed.join(root)).unwrap();
        if root == DONE {
            write_task(&fixture.seed, DONE, "completed", false);
        } else {
            std::fs::write(fixture.seed.join(root).join("README.md"), "# Legacy task\n").unwrap();
        }
        std::fs::File::create(fixture.seed.join(root).join("large.bin"))
            .unwrap()
            .set_len(PAYLOAD_BYTES)
            .unwrap();
        // This is a previously published LFS pointer. Its materialized bytes
        // will replace it after clone, without needing an external LFS server.
        std::fs::write(
            fixture.seed.join(root).join("large-lfs.bin"),
            format!(
                "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize {PAYLOAD_BYTES}\n",
                sha256_zero_bytes(PAYLOAD_BYTES)
            ),
        )
        .unwrap();
    }
    // A root-prefix rule intentionally stops matching when the legacy task
    // moves. The task-local rule remains valid for the managed task.
    std::fs::write(
        fixture.seed.join(".gitattributes"),
        format!("{LEGACY}/large-lfs.bin filter=archive-fixture -text\n"),
    )
    .unwrap();
    std::fs::write(
        fixture.seed.join(DONE).join(".gitattributes"),
        "large-lfs.bin filter=archive-fixture -text\n",
    )
    .unwrap();
    fixture.commit_seed("Publish retained Git and LFS payloads before adoption");
    let base = oid(&fixture.seed, "HEAD");
    let old_blobs = [LEGACY, DONE].map(|root| {
        ["large.bin", "large-lfs.bin"]
            .map(|file| oid(&fixture.seed, &format!("{base}:{root}/{file}")))
    });
    fixture.clone_shared();
    let pointer = fixture.root.join("fixture-lfs-pointer");
    std::fs::write(
        &pointer,
        git(
            &fixture.shared,
            ["show", &format!("{base}:{DONE}/large-lfs.bin")],
        )
        .stdout,
    )
    .unwrap();
    let clean = format!("cat >/dev/null; cat '{}'", pointer.display());
    git(
        &fixture.shared,
        ["config", "filter.archive-fixture.clean", clean.as_str()],
    );
    for root in [LEGACY, DONE] {
        std::fs::File::create(fixture.shared.join(root).join("large-lfs.bin"))
            .unwrap()
            .set_len(PAYLOAD_BYTES)
            .unwrap();
    }
    let (workplace, manifest) = organizer(&fixture, &[LEGACY, LEGACY_DEST, DONE, DESTINATION]);
    let gh = write_gh(&fixture, &BTreeMap::new());
    json(&archive(
        &workplace,
        &gh,
        &[
            "task",
            "adopt",
            LEGACY,
            "--title",
            "Legacy task",
            "--purpose",
            "Retain already published opaque payloads",
            "--manifest",
            manifest.to_str().unwrap(),
        ],
    ));
    json(&workspace(
        &workplace,
        [
            "storage",
            "set",
            &format!("{LEGACY}/large-lfs.bin"),
            "--to",
            "git",
            "--reason",
            "Keep the existing published Git LFS placement",
            "--manifest",
            manifest.to_str().unwrap(),
        ],
    ));
    json(&archive(
        &workplace,
        &gh,
        &[
            "archive",
            LEGACY,
            DONE,
            "--manifest",
            manifest.to_str().unwrap(),
        ],
    ));
    for _ in 0..2 {
        let plan = json(&workspace(
            &workplace,
            ["plan", "--manifest", manifest.to_str().unwrap()],
        ));
        assert!(
            plan["storage"]["placement"]["placed_in_s3"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let tree = plan["tree_oid"].as_str().unwrap();
        assert_eq!(plan["cloud_usage"]["projected"]["git_lfs_bytes"], 0);
        for (task, destination) in [LEGACY_DEST, DESTINATION].iter().enumerate() {
            for (file, name) in ["large.bin", "large-lfs.bin"].iter().enumerate() {
                assert_eq!(
                    oid(&workplace, &format!("{tree}:{destination}/{name}")),
                    old_blobs[task][file]
                );
                assert_eq!(
                    std::fs::metadata(workplace.join(destination).join(name))
                        .unwrap()
                        .len(),
                    PAYLOAD_BYTES
                );
            }
        }
    }
    let managed_lfs = workplace.join(DESTINATION).join("large-lfs.bin");
    git(&workplace, ["config", "--unset", "core.filemode"]);
    std::fs::set_permissions(&managed_lfs, std::fs::Permissions::from_mode(0o755)).unwrap();
    let plan = json(&workspace(
        &workplace,
        ["plan", "--manifest", manifest.to_str().unwrap()],
    ));
    let tree = plan["tree_oid"].as_str().unwrap();
    let entry = String::from_utf8(
        git(
            &workplace,
            ["ls-tree", tree, &format!("{DESTINATION}/large-lfs.bin")],
        )
        .stdout,
    )
    .unwrap();
    assert!(entry.starts_with("100755 blob "));
    assert_eq!(
        oid(&workplace, &format!("{tree}:{DESTINATION}/large-lfs.bin")),
        old_blobs[1][1]
    );

    // Real byte changes cannot be hidden behind the old pointer. A missing
    // filter refuses before staging the materialized bytes as an ordinary blob.
    use std::io::Write;
    let legacy_lfs = workplace.join(LEGACY_DEST).join("large-lfs.bin");
    std::fs::OpenOptions::new()
        .write(true)
        .open(&legacy_lfs)
        .unwrap()
        .write_all(&[1])
        .unwrap();
    let refused = workspace_unchecked(
        &workplace,
        ["plan", "--manifest", manifest.to_str().unwrap()],
    );
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("changed but has no configured LFS filter")
    );
    std::fs::OpenOptions::new()
        .write(true)
        .open(&legacy_lfs)
        .unwrap()
        .write_all(&[0])
        .unwrap();

    // A configured filter must emit the pointer for the new materialized
    // bytes; stale filter output is rejected instead of silently reusing it.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&managed_lfs)
        .unwrap()
        .write_all(&[1])
        .unwrap();
    let refused = workspace_unchecked(
        &workplace,
        ["plan", "--manifest", manifest.to_str().unwrap()],
    );
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("did not stage a matching LFS pointer")
    );
    use sha2::Digest;
    let checksum = sha2::Sha256::digest(std::fs::read(&managed_lfs).unwrap())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    std::fs::write(&pointer, format!("version https://git-lfs.github.com/spec/v1\noid sha256:{checksum}\nsize {PAYLOAD_BYTES}\n")).unwrap();
    let changed = json(&workspace(
        &workplace,
        ["plan", "--manifest", manifest.to_str().unwrap()],
    ));
    let tree = changed["tree_oid"].as_str().unwrap();
    assert_ne!(
        oid(&workplace, &format!("{tree}:{DESTINATION}/large-lfs.bin")),
        old_blobs[1][1]
    );
    assert_eq!(
        changed["cloud_usage"]["projected"]["git_lfs_bytes"],
        PAYLOAD_BYTES
    );
}

fn sha256_zero_bytes(bytes: u64) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    let block = [0_u8; 65_536];
    let mut remaining = bytes;
    while remaining > 0 {
        let count = remaining.min(block.len() as u64) as usize;
        hash.update(&block[..count]);
        remaining -= count as u64;
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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

fn forbid_archive_history_operations(fixture: &GitFixture) {
    let guard = fixture.root.join("historical-config-guard");
    std::fs::create_dir_all(&guard).unwrap();
    let real_git = String::from_utf8(command(&fixture.root, "which", ["git"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    let real_git = serde_json::to_string(&real_git).unwrap();
    let marker = serde_json::to_string(
        fixture
            .root
            .join("archive-history-operation")
            .to_str()
            .unwrap(),
    )
    .unwrap();
    let program = guard.join("git");
    std::fs::write(&program, format!(
        "#!/usr/bin/env python3\nimport os, sys\nargs = sys.argv[1:]\nif 'log' in args or 'merge-base' in args or any('refs/pull/' in arg for arg in args) or ('show' in args and any(':' in arg and arg.endswith('/{MANIFEST}') for arg in args)):\n    open({marker}, 'w').write(' '.join(args))\n    sys.exit('Archive history operations are forbidden')\nos.execv({real_git}, [{real_git}, *args])\n"
    )).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn forbid_task_payload_tree_history(fixture: &GitFixture) {
    let guard = fixture.root.join("historical-config-guard");
    std::fs::create_dir_all(&guard).unwrap();
    let real_git = String::from_utf8(command(&fixture.root, "which", ["git"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    let real_git = serde_json::to_string(&real_git).unwrap();
    let marker = serde_json::to_string(
        fixture
            .root
            .join("payload-tree-history-read")
            .to_str()
            .unwrap(),
    )
    .unwrap();
    let program = guard.join("git");
    std::fs::write(&program, format!(
        "#!/usr/bin/env python3\nimport os, sys\nargs = sys.argv[1:]\nif 'log' in args or '--first-parent' in args or ('ls-tree' in args and any(arg == '{DONE}' or arg.startswith('{DONE}/') for arg in args)):\n    open({marker}, 'w').write(' '.join(args))\n    sys.exit('Task payload tree/history reads are forbidden')\nos.execv({real_git}, [{real_git}, *args])\n"
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
    assert!(preview.get("notices").is_none());
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
        "archive refuses pending task",
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
    assert_eq!(
        output["notices"][0]["code"],
        "manual-content-audit-after-relocation"
    );
    assert!(
        output["notices"][0]["message"]
            .as_str()
            .unwrap()
            .starts_with("Archive directory move succeeded.")
    );
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
    let unchanged = json(&archive(
        &worktree,
        &gh,
        &[
            "archive",
            "--manifest",
            infrastructure_manifest.to_str().unwrap(),
        ],
    ));
    assert_eq!(unchanged["status"], "no_changes");
    assert!(unchanged.get("notices").is_none());
}

#[test]
fn archive_uses_closed_pr_state_instead_of_resumed_remote_branch_content() {
    let (fixture, merged) = managed_fixture(false);
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
        ["commit", "-m", "Resume work after the closed PR"],
    );
    git(&fixture.seed, ["push", "origin", "codex/completed"]);
    let preview = json(&archive(
        &fixture.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
    ));
    assert_eq!(preview["tasks"].as_array().unwrap().len(), 1);
    let (workplace, manifest) = organizer(&fixture, &[DONE, DESTINATION]);
    let report = json(&archive(
        &workplace,
        &gh,
        &["archive", DONE, "--manifest", manifest.to_str().unwrap()],
    ));
    assert_eq!(report["status"], "archived");
    assert_eq!(
        std::fs::read_to_string(workplace.join(DESTINATION).join("result.md")).unwrap(),
        "retained result\n"
    );
    assert_eq!(
        oid(&fixture.remote, "refs/heads/codex/completed"),
        oid(&fixture.seed, "codex/completed")
    );
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
fn archive_preserves_cross_task_scripts_even_when_tasks_move_to_different_months() {
    let (fixture, gh) = related_completed_fixture();
    let (workplace, manifest) =
        organizer(&fixture, &[DONE, DESTINATION, RELATED, RELATED_DESTINATION]);
    let script = format!(
        "from pathlib import Path\nother = Path(__file__).parent.parent / '{RELATED}' / 'input.tsv'\nprint(other.read_text())\n"
    );
    std::fs::write(workplace.join(DONE).join("read_other.py"), &script).unwrap();
    let preview = json(&archive(
        &workplace,
        &gh,
        &[
            "archive",
            DONE,
            RELATED,
            "--manifest",
            manifest.to_str().unwrap(),
            "--dry-run",
        ],
    ));
    assert_eq!(preview["tasks"].as_array().unwrap().len(), 2);
    let report = json(&archive(
        &workplace,
        &gh,
        &[
            "archive",
            DONE,
            RELATED,
            "--manifest",
            manifest.to_str().unwrap(),
        ],
    ));
    assert_eq!(report["status"], "archived");
    assert_eq!(
        std::fs::read_to_string(workplace.join(DESTINATION).join("read_other.py")).unwrap(),
        script
    );
    assert_eq!(
        std::fs::read_to_string(workplace.join(RELATED_DESTINATION).join("input.tsv")).unwrap(),
        "value\n17\n"
    );
}

#[test]
fn archive_moves_an_ignored_external_git_worktree_without_touching_its_administration() {
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
    std::fs::write(
        workplace.join(DONE).join(".gitignore"),
        "ignored-checkout/\n",
    )
    .unwrap();
    let pointer = std::fs::read(checkout.join(".git")).unwrap();
    let admin = PathBuf::from(
        String::from_utf8(git(&checkout, ["rev-parse", "--absolute-git-dir"]).stdout)
            .unwrap()
            .trim(),
    );
    let backlink = std::fs::read(admin.join("gitdir")).unwrap();
    let common = std::fs::read(admin.join("commondir")).unwrap();
    let index = git(&workplace, ["ls-files", "--stage"]).stdout;
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
    archive(
        &workplace,
        &gh,
        &["archive", DONE, "--manifest", manifest.to_str().unwrap()],
    );
    assert_eq!(
        std::fs::read(workplace.join(DESTINATION).join("ignored-checkout/.git")).unwrap(),
        pointer
    );
    assert_eq!(std::fs::read(admin.join("gitdir")).unwrap(), backlink);
    assert_eq!(std::fs::read(admin.join("commondir")).unwrap(), common);
    assert_eq!(git(&workplace, ["ls-files", "--stage"]).stdout, index);
    archive(
        &workplace,
        &gh,
        &[
            "archive",
            DONE,
            "--manifest",
            manifest.to_str().unwrap(),
            "--cancel",
        ],
    );
    assert_eq!(std::fs::read(checkout.join(".git")).unwrap(), pointer);
    assert_eq!(std::fs::read(admin.join("gitdir")).unwrap(), backlink);
    git(&checkout, ["status", "--porcelain"]);
}

#[test]
fn archive_new_protocol_requires_089_before_creating_local_attempt_state() {
    let (fixture, merged) = managed_fixture(false);
    let gh = fake_gh(&fixture, &merged, false);
    for version in ["0.6.0", "0.7.0", "0.8.8"] {
        let output = workspace_env_unchecked(
            &fixture.shared,
            ["archive", DONE, "--dry-run"],
            &[
                ("WORKSPACE_MGR_TEST_GH", gh.to_str().unwrap()),
                (CLI_VERSION_ENV, version),
            ],
        );
        assert_eq!(output.status.code(), Some(2));
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("archive protocol requires workspace-mgr 0.8.9"),
            "{error}"
        );
    }
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
fn archive_and_cancel_preserve_internal_absolute_git_worktree_bytes_without_repairs() {
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
    std::fs::write(
        workplace.join(DONE).join(".gitignore"),
        "nested-main/\nnested-linked/\n",
    )
    .unwrap();
    archive(
        &workplace,
        &gh,
        &["archive", DONE, "--manifest", manifest.to_str().unwrap()],
    );
    let moved = workplace.join(DESTINATION).join("nested-linked");
    assert_eq!(std::fs::read(moved.join(".git")).unwrap(), original_pointer);
    assert!(
        !git_unchecked(&moved, ["status", "--porcelain"])
            .status
            .success(),
        "archive preserves absolute control paths even when this breaks the moved worktree"
    );
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
fn archive_ignores_unpublished_local_branch_history_when_the_pr_is_closed() {
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
    assert_eq!(preview["tasks"].as_array().unwrap().len(), 1);
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
fn archive_refuses_scope_and_destination_collisions_but_accepts_ordinary_overlays() {
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

    // Read-only previews still decide destination collisions before planning migrations.
    std::fs::create_dir_all(fixture.shared.join(DESTINATION)).unwrap();
    rejected(
        &fixture.shared,
        &gh,
        &["archive", "--dry-run"],
        "destination already exists",
    );
    std::fs::remove_dir_all(fixture.shared.join("2026")).unwrap();
    std::fs::write(fixture.shared.join(DONE).join("result.md"), "local edits\n").unwrap();
    let preview = json(&archive(&fixture.shared, &gh, &["archive", "--dry-run"]));
    assert_eq!(preview["tasks"].as_array().unwrap().len(), 1);
    assert!(fixture.shared.join(DONE).is_dir());
    assert!(!fixture.shared.join(DESTINATION).exists());
    assert!(!fixture.shared.join(DONE).join(RECEIPT).exists());
}

#[test]
fn archive_rejects_changed_materialized_s3_payloads_without_touching_metadata() {
    let (fixture, merged) = managed_fixture(false);
    let original_head = oid(&fixture.remote, "refs/heads/codex/completed");
    let task = fixture.seed.join(DONE);
    std::fs::write(
        task.join("model.bin.wm-storage.json"),
        storage_file_manifest(
            "model.bin",
            "9f9f90dbe3e5ee1218c86b8839db1995",
            6,
            Some("retained-version"),
        ),
    )
    .unwrap();
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
    let pointer =
        std::fs::read(fixture.shared.join(DONE).join("model.bin.wm-storage.json")).unwrap();
    rejected(
        &fixture.shared,
        &gh,
        &["archive", "--dry-run"],
        "locally changed materialized S3 output",
    );
    assert_eq!(
        std::fs::read(fixture.shared.join(DONE).join("model.bin.wm-storage.json")).unwrap(),
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
fn archive_accepts_a_divergent_local_branch_when_the_associated_pr_is_closed() {
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
    let preview = json(&archive(
        &fixture.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
    ));
    assert_eq!(preview["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(oid(&fixture.shared, "refs/heads/codex/completed"), diverged);
}

fn migrated_fixture(reviewed: bool) -> (GitFixture, PathBuf) {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["manage"]);
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
        // The current task control association is supplied by its own closed
        // PR; no history walk should rediscover which migration wrote files.
        requests.insert(
            "codex/completed",
            vec![pr(
                "MERGED",
                2,
                "codex/completed",
                &migration_merge,
                &migration_head,
            )],
        );
    }
    let gh = write_gh(&fixture, &requests);
    (fixture, gh)
}

#[test]
fn archive_follows_reviewed_task_updates_without_reading_historical_configuration() {
    let (fixture, gh) = migrated_fixture(true);
    let manifest = fixture.shared.join(DONE).join(MANIFEST);
    archive(
        &fixture.shared,
        &gh,
        &["task", "upgrade", "--manifest", manifest.to_str().unwrap()],
    );
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
fn archive_accepts_a_migrated_branch_without_any_current_pull_request() {
    let (fixture, gh) = migrated_fixture(false);
    let preview = json(&archive(
        &fixture.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
    ));
    assert_eq!(preview["tasks"].as_array().unwrap().len(), 1);
    assert!(preview["tasks"][0]["pull_request"].is_null());
    assert!(fixture.shared.join(DONE).is_dir());
    assert!(!fixture.shared.join(DESTINATION).exists());
    assert!(!fixture.root.join("historical-config-read").exists());
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
            "archive must use current PR state instead of rediscovering old commit reviews"
        );
        assert!(
            !self.git.root.join("historical-config-read").exists(),
            "archive must never read the historical task configuration blob"
        );
    }
}

fn completion_fixture() -> CompletionFixture {
    let (fixture, _) = migrated_fixture(true);
    let original_merge = oid(&fixture.seed, "codex/infra-schema^");
    let original_head = oid(&fixture.seed, "historical/completed");
    let migration_merge = oid(&fixture.seed, "main");
    let migration_head = oid(&fixture.seed, "codex/infra-schema");
    let manifest = fixture.shared.join(DONE).join(MANIFEST);
    // Simulate a record already published by an older client. The current
    // branch hints remain useful even though their old tree proof is opaque.
    let mut current: toml::Value =
        toml::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap();
    current["schema_version"] = 4.into();
    let completion = toml::Value::try_from(value!({
        "schema_version": 1,
        "task_id": DONE,
        "repository": "example.invalid/owner/archive-fixture",
        "base_branch": "main",
        "checkpoint_commit": migration_merge,
        "checkpoint_path": DONE,
        "checkpoint_tree": "0".repeat(40),
        "branches": ["historical/completed", "codex/completed", "codex/infra-schema"],
        "reviews": [
            {"branch":"historical/completed", "number":1, "url":"https://example.invalid/1", "merged_at":"2026-07-12T20:00:00Z", "merge_commit":original_merge, "head_commit":original_head},
            {"branch":"codex/infra-schema", "number":2, "url":"https://example.invalid/2", "merged_at":"2026-07-12T20:00:00Z", "merge_commit":migration_merge, "head_commit":migration_head}
        ],
    })).unwrap();
    current
        .as_table_mut()
        .unwrap()
        .insert("archive_completion".to_owned(), completion);
    std::fs::write(
        fixture.seed.join(DONE).join(MANIFEST),
        toml::to_string_pretty(&current).unwrap(),
    )
    .unwrap();
    fixture.commit_seed("Retain existing task control association metadata");
    git(&fixture.shared, ["pull", "--ff-only", "origin", "main"]);
    let before = std::fs::read(&manifest).unwrap();
    let requests = BTreeMap::from([
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
    ]);
    let borrowed = requests
        .iter()
        .map(|(branch, rows)| (branch.as_str(), rows.clone()))
        .collect();
    let gh = write_gh_without_historical_queries(
        &fixture,
        &borrowed,
        &[original_merge.clone(), migration_merge.clone()],
    );
    archive(
        &fixture.shared,
        &gh,
        &["task", "upgrade", "--manifest", manifest.to_str().unwrap()],
    );
    assert_eq!(
        toml::from_str::<toml::Value>(&std::fs::read_to_string(&manifest).unwrap()).unwrap(),
        toml::from_str::<toml::Value>(std::str::from_utf8(&before).unwrap()).unwrap(),
        "upgrade must preserve already-current association metadata",
    );
    CompletionFixture {
        git: fixture,
        requests,
        historical_commits: vec![original_merge, migration_merge],
    }
}

#[test]
fn task_upgrade_preserves_current_metadata_without_generating_content_proofs() {
    let (fixture, gh) = migrated_fixture(true);
    forbid_task_payload_tree_history(&fixture);
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
    let modern: toml::Value = toml::from_str(std::str::from_utf8(&upgraded).unwrap()).unwrap();
    assert!(modern.get("archive_completion").is_none());
    assert_eq!(
        modern, original,
        "upgrade must retain task identity, scopes, and approval without content proofs"
    );
    archive(
        &fixture.shared,
        &gh,
        &["task", "upgrade", "--manifest", manifest.to_str().unwrap()],
    );
    assert_eq!(std::fs::read(&manifest).unwrap(), upgraded);
    assert_eq!(oid(&fixture.remote, "refs/heads/main"), remote_before);
    assert!(!fixture.root.join("historical-config-read").exists());
    assert!(!fixture.root.join("payload-tree-history-read").exists());
}

#[test]
fn adoption_accepts_dirty_payloads_without_comparing_reviewed_task_trees() {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["manage"]);
    let task = fixture.seed.join(DONE);
    std::fs::create_dir_all(&task).unwrap();
    std::fs::write(task.join("README.md"), "Original description\n").unwrap();
    std::fs::write(task.join("input.bin"), b"original\n").unwrap();
    fixture.commit_seed("Directly import legacy directory");
    let head = oid(&fixture.seed, "HEAD");
    git(&fixture.seed, ["push", "origin", "HEAD:legacy/completed"]);
    fixture.clone_shared();
    let (workplace, infrastructure_manifest) = organizer(&fixture, &[DONE]);
    let task = fixture.shared.join(DONE);
    let description = b"User description with [broken link](/old/missing/input)\n\xff";
    std::fs::write(task.join("README.md"), description).unwrap();
    std::fs::write(task.join("input.bin"), b"staged payload\n").unwrap();
    git(&fixture.shared, ["add", &format!("{DONE}/input.bin")]);
    let payload = b"\0current unstaged bytes\xff\n";
    std::fs::write(task.join("input.bin"), payload).unwrap();
    std::fs::write(task.join("new.log"), "untracked historical /old/path\n").unwrap();
    let index = git(&fixture.shared, ["ls-files", "--stage", "-z"]).stdout;
    let rows = BTreeMap::from([(
        "legacy/completed",
        vec![pr("MERGED", 1, "legacy/completed", &head, &head)],
    )]);
    let gh = write_gh_without_historical_queries(&fixture, &rows, std::slice::from_ref(&head));
    forbid_task_payload_tree_history(&fixture);
    let result = json(&archive(
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
            "Register the current control association",
            "--manifest",
            infrastructure_manifest.to_str().unwrap(),
        ],
    ));
    assert_eq!(result["status"], "adopted");
    assert_eq!(std::fs::read(task.join("README.md")).unwrap(), description);
    assert_eq!(std::fs::read(task.join("input.bin")).unwrap(), payload);
    assert_eq!(
        git(&fixture.shared, ["ls-files", "--stage", "-z"]).stdout,
        index
    );
    assert_eq!(
        std::fs::read_to_string(task.join("new.log")).unwrap(),
        "untracked historical /old/path\n"
    );
    let record: Value =
        serde_json::from_slice(&std::fs::read(task.join(".workspace-mgr-legacy.json")).unwrap())
            .unwrap();
    assert!(record.get("tree").is_none());
    // A previous client's opaque tree field is neither validated nor rewritten
    // by a retry; the current identity and PR association remain unchanged.
    let mut old_record = record;
    old_record["tree"] = value!(["opaque legacy snapshots unrelated to current payload"]);
    let old_bytes = serde_json::to_vec_pretty(&old_record).unwrap();
    std::fs::write(task.join(".workspace-mgr-legacy.json"), &old_bytes).unwrap();
    let retry = json(&archive(
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
            "Register the current control association",
            "--manifest",
            infrastructure_manifest.to_str().unwrap(),
        ],
    ));
    assert_eq!(retry["status"], "no_changes");
    assert_eq!(
        std::fs::read(task.join(".workspace-mgr-legacy.json")).unwrap(),
        old_bytes
    );
    assert!(!fixture.root.join("payload-tree-history-read").exists());
    assert!(!fixture.root.join("blocked-historical-query").exists());
}

#[test]
fn archive_uses_current_completion_metadata_without_reading_any_historical_configuration() {
    let fixture = completion_fixture();
    forbid_archive_history_operations(&fixture.git);
    let gh = fixture.hosting();
    let preview = json(&archive(
        &fixture.git.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
    ));
    assert_eq!(preview["tasks"].as_array().unwrap().len(), 1);
    // The current configuration links the task and schema-migration PRs.
    // The later infrastructure PR is not rediscovered from commit history.
    assert_eq!(preview["tasks"][0]["pull_request"]["number"], 2);
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
    assert!(!fixture.git.root.join("archive-history-operation").exists());
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
        "archive refuses pending task",
    );
    assert!(fixture.git.shared.join(DONE).join(MANIFEST).is_file());
    assert!(!fixture.git.shared.join(DESTINATION).exists());
    fixture.assert_no_historical_lookup();
}

#[test]
fn archive_accepts_current_content_without_rechecking_checkpoint_history() {
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
    let preview = json(&archive(
        &fixture.git.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
    ));
    assert_eq!(preview["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(
        std::fs::read_to_string(fixture.git.shared.join(DONE).join("result.md")).unwrap(),
        "Unreviewed direct main edit\n"
    );
    assert!(!fixture.git.shared.join(DESTINATION).exists());
    fixture.assert_no_historical_lookup();
}

#[test]
fn archive_does_not_verify_saved_checkpoint_trees() {
    let fixture = completion_fixture();
    let manifest = fixture.git.shared.join(DONE).join(MANIFEST);
    let mut current: toml::Value =
        toml::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap();
    current["archive_completion"]["checkpoint_tree"] = toml::Value::String("0".repeat(40));
    std::fs::write(&manifest, toml::to_string_pretty(&current).unwrap()).unwrap();
    let before = std::fs::read(&manifest).unwrap();
    let gh = fixture.hosting();
    let preview = json(&archive(
        &fixture.git.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
    ));
    assert_eq!(preview["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(std::fs::read(&manifest).unwrap(), before);
    assert!(!fixture.git.shared.join(DESTINATION).exists());
    fixture.assert_no_historical_lookup();
}

#[test]
fn archive_accepts_current_completion_metadata_without_requiring_another_review() {
    let (fixture, gh) = migrated_fixture(true);
    let manifest = fixture.shared.join(DONE).join(MANIFEST);
    archive(
        &fixture.shared,
        &gh,
        &["task", "upgrade", "--manifest", manifest.to_str().unwrap()],
    );
    let before = std::fs::read(&manifest).unwrap();
    let preview = json(&archive(
        &fixture.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
    ));
    assert_eq!(preview["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(std::fs::read(&manifest).unwrap(), before);
    assert!(!fixture.shared.join(DESTINATION).exists());
    assert!(!fixture.root.join("historical-config-read").exists());
}

#[test]
fn legacy_tasks_are_visible_and_require_explicit_reviewed_adoption() {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["manage"]);
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
    assert!(
        !workplace
            .join(format!("{record_path}.wm-storage.json"))
            .exists()
    );
    let archive_preview = json(&archive(
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
    assert_eq!(archive_preview["tasks"].as_array().unwrap().len(), 1);

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
    workspace(&fixture.seed, ["manage"]);
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
fn archive_follows_the_closed_adopted_pr_without_audit_of_the_direct_import() {
    let (fixture, workplace, manifest) = directly_imported_adoption_fixture(true);
    let gh = fixture.hosting();
    let preview = json(&archive(&workplace, &gh, &["archive", DONE, "--dry-run"]));
    assert_eq!(preview["tasks"].as_array().unwrap().len(), 1);
    // The current adoption record links the legacy task PR. A separate
    // infrastructure PR that introduced the record is irrelevant to archive.
    assert_eq!(preview["tasks"][0]["pull_request"]["number"], 1);
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
fn archive_does_not_require_an_additional_adoption_review() {
    let (fixture, workplace, _) = directly_imported_adoption_fixture(false);
    let gh = fixture.hosting();
    let preview = json(&archive(&workplace, &gh, &["archive", DONE, "--dry-run"]));
    assert_eq!(preview["tasks"].as_array().unwrap().len(), 1);
    assert!(workplace.join(DONE).join(MANIFEST).is_file());
    assert!(!workplace.join(DESTINATION).exists());
    fixture.assert_no_historical_lookup();
}

#[test]
fn archive_rejects_open_prs_for_all_currently_recorded_task_branches() {
    for branch in ["legacy/completed", "codex/completed"] {
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
            "archive refuses pending task",
        );
        assert!(workplace.join(DONE).is_dir());
        assert!(!workplace.join(DESTINATION).exists());
        fixture.assert_no_historical_lookup();
    }
}

#[test]
fn archive_accepts_current_content_without_a_review_for_every_historical_edit() {
    let (fixture, merged) = managed_fixture(false);
    let gh = fake_gh(&fixture, &merged, false);
    std::fs::write(
        fixture.seed.join(DONE).join("result.md"),
        "New work without a review\n",
    )
    .unwrap();
    fixture.commit_seed("Direct change to completed task content");
    git(&fixture.shared, ["pull", "--ff-only", "origin", "main"]);
    let preview = json(&archive(
        &fixture.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
    ));
    assert_eq!(preview["tasks"].as_array().unwrap().len(), 1);
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
}

#[test]
fn archive_does_not_fetch_missing_historical_pr_heads() {
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
        !git_unchecked(
            &historical,
            ["cat-file", "-e", &format!("{head}^{{commit}}")]
        )
        .status
        .success(),
        "archive must not fetch old PR commits when their current state is enough"
    );
    assert_eq!(oid(&historical, "refs/heads/codex/completed"), older);
    assert!(!historical.join(".git/FETCH_HEAD").exists());
}

#[test]
fn archive_accepts_shallow_checkouts_using_only_current_task_and_pr_state() {
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
    // The current manifest and PR state are sufficient even when the clone
    // does not contain the original task creation history.
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
    let preview = json(&archive(&shallow, &gh, &["archive", DONE, "--dry-run"]));
    assert_eq!(preview["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(
        std::fs::read(shallow.join(DONE).join(MANIFEST)).unwrap(),
        before
    );
    assert!(!shallow.join(DESTINATION).exists());
    assert_eq!(oid(&shallow, "HEAD"), latest);
}

#[test]
fn archive_preserves_arbitrary_local_content_and_user_index_through_cancel() {
    use std::os::unix::fs::symlink;

    let (fixture, merged) = managed_fixture(false);
    let gh = fake_gh(&fixture, &merged, false);
    let (workplace, manifest) = organizer(&fixture, &[DONE, DESTINATION]);
    let source = workplace.join(DONE);
    let original_manifest = std::fs::read(source.join(MANIFEST)).unwrap();
    std::fs::write(source.join("result.md"), "staged local result\n").unwrap();
    git(&workplace, ["add", &format!("{DONE}/result.md")]);
    std::fs::write(source.join("result.md"), "unstaged local result\n").unwrap();
    let script = format!("#!/bin/sh\ncat '{}/result.md'\n", source.display());
    let readme = format!("# Old command\nsh {DONE}/run.sh\n");
    let historical = format!("Previously executed in {}\n", source.display());
    std::fs::write(source.join("run.sh"), &script).unwrap();
    std::fs::set_permissions(
        source.join("run.sh"),
        std::fs::Permissions::from_mode(0o751),
    )
    .unwrap();
    std::fs::write(source.join("README.md"), &readme).unwrap();
    std::fs::create_dir_all(source.join("logs")).unwrap();
    std::fs::write(source.join("logs/history.log"), &historical).unwrap();
    std::fs::write(source.join("new-untracked.bin"), b"\0new local bytes\xff").unwrap();
    std::fs::write(source.join("local.bin"), b"\0retained local-only bytes\xfe").unwrap();
    std::fs::write(
        source.join("local.bin.workspace-mgr-storage.toml"),
        "schema_version = 1\ntarget = \"local\"\nreason = \"Keep payload in this checkout\"\n",
    )
    .unwrap();
    std::fs::write(source.join(".gitignore"), "/local.bin\n/.venv/\n/cache/\n").unwrap();
    let venv = source.join(".venv");
    std::fs::create_dir_all(venv.join("bin")).unwrap();
    std::fs::write(
        venv.join("pyvenv.cfg"),
        format!("home = {}\n", source.display()),
    )
    .unwrap();
    let launcher = format!("#!{}/bin/python\nprint('keep')\n", venv.display());
    std::fs::write(venv.join("bin/tool"), &launcher).unwrap();
    std::fs::create_dir_all(source.join("cache")).unwrap();
    std::fs::write(
        source.join("cache/hydrated.bin"),
        b"ignored hydrated contents",
    )
    .unwrap();
    let link_target = source.join("result.md");
    symlink(&link_target, source.join("absolute-link")).unwrap();
    let other = workplace.join("README.md");
    std::fs::write(&other, "another task pending change\n").unwrap();
    git(&workplace, ["add", "README.md"]);
    let index = git(&workplace, ["ls-files", "--stage", "-z"]).stdout;
    let staged_result = git(&workplace, ["show", &format!(":{DONE}/result.md")]).stdout;
    let originals = [
        "result.md",
        "run.sh",
        "README.md",
        "logs/history.log",
        "new-untracked.bin",
        "local.bin",
        "local.bin.workspace-mgr-storage.toml",
        ".gitignore",
        ".venv/pyvenv.cfg",
        ".venv/bin/tool",
        "cache/hydrated.bin",
    ]
    .into_iter()
    .map(|relative| (relative, std::fs::read(source.join(relative)).unwrap()))
    .collect::<Vec<_>>();
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
    let destination = workplace.join(DESTINATION);
    for (relative, bytes) in &originals {
        assert_eq!(
            std::fs::read(destination.join(relative)).unwrap(),
            *bytes,
            "{relative}"
        );
    }
    assert_eq!(
        std::fs::read_link(destination.join("absolute-link")).unwrap(),
        link_target
    );
    assert_eq!(
        std::fs::metadata(destination.join("run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o751
    );
    assert_eq!(git(&workplace, ["ls-files", "--stage", "-z"]).stdout, index);
    let receipt: Value =
        serde_json::from_slice(&std::fs::read(destination.join(RECEIPT)).unwrap()).unwrap();
    assert!(receipt.get("historical_records").is_none());
    assert!(receipt.get("completion_reviews").is_none());
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
    for (relative, bytes) in &originals {
        assert_eq!(
            std::fs::read(source.join(relative)).unwrap(),
            *bytes,
            "{relative}"
        );
    }
    assert_eq!(
        std::fs::read(source.join(MANIFEST)).unwrap(),
        original_manifest
    );
    assert!(!source.join(RECEIPT).exists());
    assert_eq!(
        std::fs::read_link(source.join("absolute-link")).unwrap(),
        link_target
    );
    assert_eq!(git(&workplace, ["ls-files", "--stage", "-z"]).stdout, index);
    assert_eq!(
        git(&workplace, ["show", &format!(":{DONE}/result.md")]).stdout,
        staged_result
    );
    assert_eq!(
        std::fs::read_to_string(&other).unwrap(),
        "another task pending change\n"
    );
    assert_eq!(
        json(&archive(
            &workplace,
            &gh,
            &[
                "archive",
                DONE,
                "--manifest",
                manifest.to_str().unwrap(),
                "--cancel"
            ]
        ))["status"],
        "no_changes"
    );
}

#[test]
fn archive_accepts_closed_unmerged_pr_and_publishes_a_task_absent_from_shared_history() {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["manage"]);
    fixture.commit_seed("Initialize managed workspace without the task");
    fixture.clone_shared();
    write_task(&fixture.shared, DONE, "completed", false);
    let head = oid(&fixture.shared, "HEAD");
    let gh = write_gh(
        &fixture,
        &BTreeMap::from([(
            "codex/completed",
            vec![pr("CLOSED", 7, "codex/completed", "", &head)],
        )]),
    );
    let (workplace, manifest) = organizer(&fixture, &[DONE, DESTINATION]);
    let original_index = git(&workplace, ["ls-files", "--stage", "-z"]).stdout;
    let report = json(&archive(
        &workplace,
        &gh,
        &["archive", DONE, "--manifest", manifest.to_str().unwrap()],
    ));
    assert_eq!(report["status"], "archived");
    assert_eq!(report["tasks"][0]["pull_request"]["state"], "CLOSED");
    assert!(report["tasks"][0]["pull_request"]["merged_at"].is_null());
    assert!(report["tasks"][0]["pull_request"]["merge_commit"].is_null());
    let receipt: Value =
        serde_json::from_slice(&std::fs::read(workplace.join(DESTINATION).join(RECEIPT)).unwrap())
            .unwrap();
    assert_eq!(receipt["closed_pull_request"]["number"], 7);
    assert_eq!(receipt["closed_pull_request"]["state"], "CLOSED");
    let published = json(&archive(
        &workplace,
        &gh,
        &[
            "publish",
            "--manifest",
            manifest.to_str().unwrap(),
            "-m",
            "Archive current contents after the PR closed",
        ],
    ));
    assert_eq!(published["status"], "pushed");
    let commit = published["commit_oid"].as_str().unwrap();
    assert_eq!(
        git(
            &workplace,
            ["show", &format!("{commit}:{DESTINATION}/result.md")]
        )
        .stdout,
        b"retained result\n"
    );
    assert_eq!(
        git(&workplace, ["ls-files", "--stage", "-z"]).stdout,
        original_index
    );
    assert!(
        !git_unchecked(
            &workplace,
            ["cat-file", "-e", &format!("{head}:{DONE}/{MANIFEST}")]
        )
        .status
        .success()
    );
}

#[test]
fn archive_refuses_a_closed_pr_if_any_current_branch_review_is_open() {
    let (fixture, merged) = managed_fixture(false);
    let head = oid(&fixture.remote, "refs/heads/codex/completed");
    let gh = write_gh(
        &fixture,
        &BTreeMap::from([(
            "codex/completed",
            vec![
                pr("CLOSED", 3, "codex/completed", "", &head),
                pr("OPEN", 4, "codex/completed", "", &head),
            ],
        )]),
    );
    rejected(
        &fixture.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
        "archive refuses pending task",
    );
    assert!(fixture.shared.join(DONE).is_dir());
    assert!(!fixture.shared.join(DESTINATION).exists());
    assert_eq!(oid(&fixture.remote, "refs/heads/main"), merged);
}

#[test]
fn archive_rejects_malformed_current_dvc_metadata_before_moving() {
    let (fixture, merged) = managed_fixture(false);
    let gh = fake_gh(&fixture, &merged, false);
    let (workplace, manifest) = organizer(&fixture, &[DONE, DESTINATION]);
    let pointer = workplace.join(DONE).join("data.bin.wm-storage.json");
    std::fs::write(&pointer, "invalid: [unclosed metadata\n").unwrap();
    let before = std::fs::read(&pointer).unwrap();
    let env = archive_environment(&gh);
    let env = env
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect::<Vec<_>>();
    let output = workspace_env_unchecked(
        &workplace,
        [
            "archive",
            DONE,
            "--manifest",
            manifest.to_str().unwrap(),
            "--dry-run",
        ],
        &env,
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(&pointer).unwrap(), before);
    assert!(workplace.join(DONE).is_dir());
    assert!(!workplace.join(DESTINATION).exists());
}

#[test]
fn archive_requires_nested_git_repositories_to_be_ignored_and_untracked() {
    for mode in [
        "unignored",
        "info-only",
        "git-control-only",
        "tracked-gitlink",
        "old-path-only",
    ] {
        let (fixture, merged) = managed_fixture(false);
        let gh = fake_gh(&fixture, &merged, false);
        let (workplace, manifest) = organizer(&fixture, &[DONE, DESTINATION]);
        let nested = workplace.join(DONE).join("vendor");
        std::fs::create_dir_all(nested.join(".git")).unwrap();
        std::fs::write(nested.join("local-content"), b"opaque payload").unwrap();
        let reason = match mode {
            "info-only" => {
                let exclude = workplace.join(".git/info/exclude");
                let original = std::fs::read_to_string(&exclude).unwrap();
                std::fs::write(exclude, format!("{original}\n/{DONE}/vendor/\n")).unwrap();
                "machine-local ignore rule"
            }
            "git-control-only" => {
                std::fs::write(workplace.join(DONE).join(".gitignore"), "/vendor/.git/\n").unwrap();
                "must be ignored as an entire directory"
            }
            "tracked-gitlink" => {
                std::fs::write(workplace.join(DONE).join(".gitignore"), "/vendor/\n").unwrap();
                let head = oid(&workplace, "HEAD");
                git(
                    &workplace,
                    [
                        "update-index",
                        "--add",
                        "--cacheinfo",
                        &format!("160000,{head},{DONE}/vendor"),
                    ],
                );
                "tracked by the outer repository"
            }
            "old-path-only" => {
                let ignore = workplace.join(".gitignore");
                let original = std::fs::read_to_string(&ignore).unwrap();
                std::fs::write(ignore, format!("{original}\n/{DONE}/vendor/\n")).unwrap();
                "would leave nested Git repository"
            }
            _ => "must be ignored as an entire directory",
        };
        let index = git(&workplace, ["ls-files", "--stage", "-z"]).stdout;
        for dry in [true, false] {
            let mut args = vec!["archive", DONE, "--manifest", manifest.to_str().unwrap()];
            if dry {
                args.push("--dry-run");
            }
            rejected(&workplace, &gh, &args, reason);
            assert!(nested.is_dir());
            assert!(!workplace.join(DESTINATION).exists());
            assert_eq!(git(&workplace, ["ls-files", "--stage", "-z"]).stdout, index);
            assert_eq!(
                std::fs::read(nested.join("local-content")).unwrap(),
                b"opaque payload"
            );
        }
    }
}

#[test]
fn archive_no_longer_accepts_the_historical_record_flag() {
    let (fixture, merged) = managed_fixture(false);
    let gh = fake_gh(&fixture, &merged, false);
    rejected(
        &fixture.shared,
        &gh,
        &[
            "archive",
            DONE,
            "--dry-run",
            "--historical-record",
            "logs/history.log",
        ],
        "unexpected argument '--historical-record'",
    );
}

#[test]
fn archive_ignores_broken_or_obsolete_legacy_records_when_current_pr_is_closed() {
    for legacy in [
        b"\0not-json obsolete configuration\xff\n".as_slice(),
        b"{\"task_id\":\"another-task\",\"branch\":\"codex/unrelated\",\"tree\":\"broken\"}\n".as_slice(),
        b"{\"schema_version\":999,\"old_branch\":\"deleted/historical\",\"former_directory\":\"/missing/old/task\"}\n".as_slice(),
    ] {
        let (fixture, merged) = managed_fixture(false);
        let gh = fake_gh(&fixture, &merged, false);
        let (workplace, manifest) = organizer(&fixture, &[DONE, DESTINATION]);
        let record = ".workspace-mgr-legacy.json";
        std::fs::write(workplace.join(DONE).join(record), legacy).unwrap();
        let history = format!("Historic old cwd: /removed/location/{DONE}\n");
        std::fs::write(workplace.join(DONE).join("former-run.log"), &history).unwrap();
        let preview = json(&archive(&workplace, &gh,
            &["archive", DONE, "--manifest", manifest.to_str().unwrap(), "--dry-run"]));
        assert_eq!(preview["tasks"].as_array().unwrap().len(), 1);
        let report = json(&archive(&workplace, &gh,
            &["archive", DONE, "--manifest", manifest.to_str().unwrap()]));
        assert_eq!(report["status"], "archived");
        assert_eq!(std::fs::read(workplace.join(DESTINATION).join(record)).unwrap(), legacy);
        assert_eq!(std::fs::read_to_string(workplace.join(DESTINATION).join("former-run.log")).unwrap(), history);
    }
}

#[test]
fn archive_does_not_let_same_named_fork_prs_mask_current_repository_pr_state() {
    let (fixture, merged) = managed_fixture(false);
    let head = oid(&fixture.remote, "refs/heads/codex/completed");
    let own_open = pr("OPEN", 1, "codex/completed", "", &head);
    let mut fork_open = pr("OPEN", 2, "codex/completed", "", &head);
    fork_open["isCrossRepository"] = value!(true);
    let own_closed = pr("CLOSED", 3, "codex/completed", "", &head);
    let gh = write_gh(
        &fixture,
        &BTreeMap::from([(
            "codex/completed",
            vec![own_open, fork_open, own_closed.clone()],
        )]),
    );
    rejected(
        &fixture.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
        "archive refuses pending task",
    );
    let mut fork_closed = pr("CLOSED", 4, "codex/completed", "", &head);
    fork_closed["isCrossRepository"] = value!(true);
    let gh = write_gh(
        &fixture,
        &BTreeMap::from([("codex/completed", vec![own_closed, fork_closed])]),
    );
    let report = json(&archive(
        &fixture.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
    ));
    assert_eq!(report["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(report["tasks"][0]["pull_request"]["number"], 3);
    assert_eq!(oid(&fixture.remote, "refs/heads/main"), merged);
}

#[test]
fn archive_accepts_closed_task_prs_targeting_a_former_or_different_base_branch() {
    let (fixture, _) = managed_fixture(false);
    let head = oid(&fixture.remote, "refs/heads/codex/completed");
    let mut closed = pr("CLOSED", 8, "codex/completed", "", &head);
    closed["baseRefName"] = value!("former-default-branch");
    let gh = write_gh(
        &fixture,
        &BTreeMap::from([("codex/completed", vec![closed])]),
    );
    let (workplace, manifest) = organizer(&fixture, &[DONE, DESTINATION]);
    let report = json(&archive(
        &workplace,
        &gh,
        &["archive", DONE, "--manifest", manifest.to_str().unwrap()],
    ));
    assert_eq!(report["status"], "archived");
    assert_eq!(report["tasks"][0]["pull_request"]["number"], 8);
    assert_eq!(report["tasks"][0]["pull_request"]["state"], "CLOSED");
    assert!(workplace.join(DESTINATION).join("result.md").is_file());
}

#[test]
fn archive_rejects_invalid_unhydrated_dvc_hashes_and_incomplete_directory_manifests() {
    for (metadata, reason) in [
        (
            "outs:\n- path: data.bin\n  hash: md5\n  size: 4\n  cloud:\n    workspace-mgr:\n      version_id: original-v1\n",
            "supported content hash",
        ),
        (
            "outs:\n- path: data.bin\n  hash: md5\n  md5: 00000000000000000000000000000000.dir\n  size: 4\n",
            "directory metadata is incomplete",
        ),
    ] {
        let (fixture, merged) = managed_fixture(false);
        let gh = fake_gh(&fixture, &merged, false);
        let (workplace, manifest) = organizer(&fixture, &[DONE, DESTINATION]);
        let pointer = workplace.join(DONE).join("data.bin.dvc");
        std::fs::write(&pointer, metadata).unwrap();
        let index = git(&workplace, ["ls-files", "--stage", "-z"]).stdout;
        assert!(!workplace.join(DONE).join("data.bin").exists());
        for dry in [true, false] {
            let mut args = vec!["archive", DONE, "--manifest", manifest.to_str().unwrap()];
            if dry {
                args.push("--dry-run");
            }
            rejected(&workplace, &gh, &args, reason);
            assert!(workplace.join(DONE).is_dir());
            assert!(!workplace.join(DESTINATION).exists());
            assert_eq!(std::fs::read_to_string(&pointer).unwrap(), metadata);
            assert_eq!(git(&workplace, ["ls-files", "--stage", "-z"]).stdout, index);
        }
    }
}

#[test]
fn archive_treats_a_successful_empty_pr_lookup_as_done_and_cancel_restores_contents() {
    let (fixture, merged) = managed_fixture(false);
    let gh = write_gh_without_historical_queries(&fixture, &BTreeMap::new(), &[merged]);
    let preview = json(&archive(&fixture.shared, &gh, &["archive", "--dry-run"]));
    assert_eq!(preview["tasks"].as_array().unwrap().len(), 1);
    assert!(preview["tasks"][0]["pull_request"].is_null());
    assert!(preview["skipped"].as_array().unwrap().is_empty());

    let (workplace, manifest) = organizer(&fixture, &[DONE, DESTINATION]);
    let source = workplace.join(DONE);
    let original_manifest = std::fs::read(source.join(MANIFEST)).unwrap();
    let payload = std::fs::read(source.join("result.md")).unwrap();
    let index = git(&workplace, ["ls-files", "--stage", "-z"]).stdout;
    let archived = json(&archive(
        &workplace,
        &gh,
        &["archive", DONE, "--manifest", manifest.to_str().unwrap()],
    ));
    assert_eq!(archived["status"], "archived");
    assert!(archived["tasks"][0]["pull_request"].is_null());
    let destination = workplace.join(DESTINATION);
    let receipt: Value =
        serde_json::from_slice(&std::fs::read(destination.join(RECEIPT)).unwrap()).unwrap();
    assert_eq!(receipt.get("closed_pull_request"), Some(&Value::Null));
    assert_eq!(
        std::fs::read(destination.join("result.md")).unwrap(),
        payload
    );
    assert_eq!(git(&workplace, ["ls-files", "--stage", "-z"]).stdout, index);

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
        std::fs::read(source.join(MANIFEST)).unwrap(),
        original_manifest
    );
    assert_eq!(std::fs::read(source.join("result.md")).unwrap(), payload);
    assert!(!destination.exists());
    assert_eq!(git(&workplace, ["ls-files", "--stage", "-z"]).stdout, index);
    assert!(!fixture.root.join("blocked-historical-query").exists());
}

#[test]
fn archive_publishes_a_null_review_without_inventing_a_pull_request() {
    let (fixture, _) = managed_fixture(false);
    let gh = write_gh(&fixture, &BTreeMap::new());
    let (workplace, manifest) = organizer(&fixture, &[DONE, DESTINATION]);
    archive(
        &workplace,
        &gh,
        &["archive", DONE, "--manifest", manifest.to_str().unwrap()],
    );
    let published = json(&archive(
        &workplace,
        &gh,
        &[
            "publish",
            "--manifest",
            manifest.to_str().unwrap(),
            "-m",
            "Archive task with no pending review",
        ],
    ));
    assert_eq!(published["status"], "pushed");
    let commit = published["commit_oid"].as_str().unwrap();
    let receipt: Value = serde_json::from_slice(
        &git(
            &workplace,
            ["show", &format!("{commit}:{DESTINATION}/{RECEIPT}")],
        )
        .stdout,
    )
    .unwrap();
    assert_eq!(receipt.get("closed_pull_request"), Some(&Value::Null));
    assert_eq!(
        git(
            &workplace,
            ["show", &format!("{commit}:{DESTINATION}/result.md")]
        )
        .stdout,
        b"retained result\n"
    );
}

#[test]
fn archive_does_not_treat_hosting_errors_or_invalid_json_as_an_empty_pr_list() {
    let (fixture, _) = managed_fixture(false);
    let (workplace, manifest) = organizer(&fixture, &[DONE, DESTINATION]);
    let source = workplace.join(DONE);
    let original_manifest = std::fs::read(source.join(MANIFEST)).unwrap();
    let payload = std::fs::read(source.join("result.md")).unwrap();
    let head = oid(&workplace, "HEAD");
    let index = git(&workplace, ["ls-files", "--stage", "-z"]).stdout;
    let gh = fixture.root.join("failed-gh");
    for script in [
        "#!/bin/sh\necho 'hosting network unavailable' >&2\nexit 17\n",
        "#!/bin/sh\nprintf '%s\\n' '{invalid-json'\n",
    ] {
        std::fs::write(&gh, script).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        for dry in [true, false] {
            let mut args = vec!["archive", DONE, "--manifest", manifest.to_str().unwrap()];
            if dry {
                args.push("--dry-run");
            }
            let environment = archive_environment(&gh);
            let environment = environment
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect::<Vec<_>>();
            let output = workspace_env_unchecked(&workplace, &args, &environment);
            assert_eq!(output.status.code(), Some(2));
            assert!(!output.stderr.is_empty());
            assert_eq!(
                std::fs::read(source.join(MANIFEST)).unwrap(),
                original_manifest
            );
            assert_eq!(std::fs::read(source.join("result.md")).unwrap(), payload);
            assert!(!source.join(RECEIPT).exists());
            assert!(!workplace.join(DESTINATION).exists());
            assert_eq!(oid(&workplace, "HEAD"), head);
            assert_eq!(git(&workplace, ["ls-files", "--stage", "-z"]).stdout, index);
        }
    }
}

#[test]
fn archive_checks_a_legacy_open_hint_even_when_the_metadata_branch_has_a_closed_pr() {
    for current_closed in [false, true] {
        let (fixture, _) = managed_fixture(false);
        let (workplace, manifest) = organizer(&fixture, &[DONE, DESTINATION]);
        let legacy = value!({"task_id":DONE,"branch":"legacy/completed"});
        let legacy_path = workplace.join(DONE).join(".workspace-mgr-legacy.json");
        std::fs::write(&legacy_path, legacy.to_string()).unwrap();
        let head = oid(&workplace, "HEAD");
        let mut requests = BTreeMap::from([(
            "legacy/completed",
            vec![pr("OPEN", 9, "legacy/completed", "", &head)],
        )]);
        if current_closed {
            requests.insert(
                "codex/completed",
                vec![pr("CLOSED", 8, "codex/completed", "", &head)],
            );
        }
        let gh = write_gh(&fixture, &requests);
        let preview = json(&archive(&workplace, &gh, &["archive", "--dry-run"]));
        assert!(preview["tasks"].as_array().unwrap().is_empty());
        assert_eq!(preview["skipped"][0]["path"], DONE);
        rejected(
            &workplace,
            &gh,
            &["archive", DONE, "--manifest", manifest.to_str().unwrap()],
            "archive refuses pending task",
        );
        assert_eq!(
            std::fs::read_to_string(&legacy_path).unwrap(),
            legacy.to_string()
        );
        assert!(!workplace.join(DESTINATION).exists());
    }
}

#[test]
fn archive_accepts_missing_legacy_and_completion_review_hints_as_done() {
    let (fixture, merged) = managed_fixture(false);
    let (workplace, manifest) = organizer(&fixture, &[DONE, DESTINATION]);
    let legacy = value!({"task_id":DONE,"branch":"legacy/no-longer-present"});
    std::fs::write(
        workplace.join(DONE).join(".workspace-mgr-legacy.json"),
        legacy.to_string(),
    )
    .unwrap();
    let gh = write_gh_without_historical_queries(&fixture, &BTreeMap::new(), &[merged]);
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
    assert!(preview["tasks"][0]["pull_request"].is_null());
    assert!(!fixture.root.join("blocked-historical-query").exists());

    let mut completion = completion_fixture();
    completion.requests.clear();
    let gh = completion.hosting();
    let preview = json(&archive(
        &completion.git.shared,
        &gh,
        &["archive", DONE, "--dry-run"],
    ));
    assert_eq!(preview["tasks"].as_array().unwrap().len(), 1);
    assert!(preview["tasks"][0]["pull_request"].is_null());
    completion.assert_no_historical_lookup();
}

#[test]
fn legacy_adoption_without_a_pull_request_preserves_payload_and_can_be_archived() {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["manage"]);
    fixture.commit_seed("Initialize managed workspace");
    std::fs::create_dir(fixture.seed.join(DONE)).unwrap();
    let payload = b"directly imported result\n";
    std::fs::write(fixture.seed.join(DONE).join("result.md"), payload).unwrap();
    std::fs::write(
        fixture.seed.join(DONE).join("README.md"),
        "# Imported task\n",
    )
    .unwrap();
    fixture.commit_seed("Import an old task without any PR");
    fixture.clone_shared();
    let (workplace, manifest) = organizer(&fixture, &[DONE, DESTINATION]);
    let gh = fixture.root.join("no-adoption-hosting");
    let queried = fixture.root.join("unexpected-adoption-hosting-query");
    std::fs::write(
        &gh,
        format!(
            "#!/usr/bin/env python3\nfrom pathlib import Path\nPath({:?}).write_text('queried')\nraise SystemExit('adoption must not rediscover historical reviews')\n",
            queried.to_str().unwrap()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    for dry in [true, false] {
        let mut args = vec![
            "task",
            "adopt",
            DONE,
            "--title",
            "Retain imported work",
            "--purpose",
            "Preserve the user-selected legacy task",
            "--manifest",
            manifest.to_str().unwrap(),
        ];
        if dry {
            args.push("--dry-run");
        }
        let adopted = json(&archive(&workplace, &gh, &args));
        assert_eq!(adopted["status"], if dry { "dry_run" } else { "adopted" });
        assert_eq!(
            std::fs::read(workplace.join(DONE).join("result.md")).unwrap(),
            payload
        );
        assert!(
            !workplace
                .join(DONE)
                .join(".workspace-mgr-legacy.json")
                .exists()
        );
        assert_eq!(workplace.join(DONE).join(MANIFEST).exists(), !dry);
        assert!(!queried.exists());
    }
    let gh = write_gh(&fixture, &BTreeMap::new());
    let archived = json(&archive(
        &workplace,
        &gh,
        &["archive", DONE, "--manifest", manifest.to_str().unwrap()],
    ));
    assert_eq!(archived["status"], "archived");
    assert!(archived["tasks"][0]["pull_request"].is_null());
    assert_eq!(
        std::fs::read(workplace.join(DESTINATION).join("result.md")).unwrap(),
        payload
    );
    assert!(
        !workplace
            .join(DESTINATION)
            .join(".workspace-mgr-legacy.json")
            .exists()
    );
}
