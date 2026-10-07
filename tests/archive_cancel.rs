#![cfg(all(feature = "test-storage", unix))]
mod common;

use common::*;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const SOURCE: &str = "20260712-120000-completed";
const DEST: &str = "2026/07/20260712-120000-completed";
const MANIFEST: &str = ".workspace-mgr-task.toml";
const RECEIPT: &str = ".workspace-mgr-archive.json";

fn fixture() -> (GitFixture, PathBuf, PathBuf) {
    fixture_with_metadata(false)
}

fn fixture_with_metadata(metadata: bool) -> (GitFixture, PathBuf, PathBuf) {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["init"]);
    fixture.commit_seed("Initialize");
    git(&fixture.seed, ["switch", "-c", "codex/completed"]);
    let task = fixture.seed.join(SOURCE);
    std::fs::create_dir(&task).unwrap();
    std::fs::write(task.join(MANIFEST), format!("schema_version = 2\nkind = \"deliverable\"\nid = \"{SOURCE}\"\nslug = \"completed\"\npath = \"{SOURCE}\"\nbranch = \"codex/completed\"\ntitle = \"Completed task\"\npurpose = \"Retain results\"\nadditional_scopes = []\n")).unwrap();
    std::fs::write(task.join("README.md"), "# Completed\n").unwrap();
    std::fs::write(task.join("record.md"), "Retained the results.\n").unwrap();
    std::fs::write(task.join("result.md"), "original result\n").unwrap();
    std::fs::write(task.join(".gitignore"), "cache/\nnested/\n").unwrap();
    if metadata {
        std::fs::create_dir(task.join("metadata")).unwrap();
        std::fs::write(task.join("metadata/data.bin.dvc"), "# original pointer formatting\nouts:\n- md5: 00000000000000000000000000000000\n  size: 4\n  hash: md5\n  path: data.bin\n  cloud:\n    workspace-mgr:\n      version_id: original-v1\n      etag: original-etag\n    other:\n      version_id: preserved-other\n").unwrap();
        std::fs::write(task.join(RECEIPT), format!("{{\n \"schema_version\": 1, \"status\": \"copied\", \"task_id\": \"{SOURCE}\",\n \"source\": \"legacy/{SOURCE}\", \"destination\": \"{SOURCE}\", \"versions\": []\n}}\n")).unwrap();
    }
    git(&fixture.seed, ["add", SOURCE]);
    git(&fixture.seed, ["commit", "-m", "Complete task"]);
    let head = String::from_utf8(git(&fixture.seed, ["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    git(&fixture.seed, ["push", "origin", "codex/completed"]);
    git(&fixture.seed, ["switch", "main"]);
    git(&fixture.seed, ["merge", "--squash", "codex/completed"]);
    git(&fixture.seed, ["commit", "-m", "Merge completed task"]);
    let merge = String::from_utf8(git(&fixture.seed, ["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    git(&fixture.seed, ["push", "origin", "main"]);
    fixture.clone_shared();
    let rows = json!([{"number":1,"url":"https://example.invalid/owner/repo/pull/1","state":"MERGED","mergedAt":"2026-07-12T20:00:00Z","mergeCommit":{"oid":merge},"headRefName":"codex/completed","headRefOid":head,"baseRefName":"main","isCrossRepository":false}]);
    let gh = fixture.root.join("fake-gh");
    std::fs::write(
        &gh,
        format!(
            "#!/usr/bin/env python3\nimport json\nprint({:?})\n",
            rows.to_string()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let created = json(&workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "archive-test",
            "--kind",
            "infrastructure",
            "--title",
            "Archive tasks",
            "--purpose",
            "Test cancel",
            "--scope",
            SOURCE,
            "--scope",
            DEST,
            "--scope",
            "README.md",
            "--scope-note",
            "User requested archive and scoped infrastructure edit",
        ],
    ));
    let manifest = PathBuf::from(created["manifest"].as_str().unwrap());
    (fixture, gh, manifest)
}

fn invoke(repo: &Path, gh: &Path, manifest: &Path, cancel: bool, dry: bool) -> Value {
    let mut args = vec!["archive", SOURCE, "--manifest", manifest.to_str().unwrap()];
    if cancel {
        args.push("--cancel");
    }
    if dry {
        args.push("--dry-run");
    }
    json(&workspace_env(
        repo,
        args,
        &[("WORKSPACE_MGR_TEST_GH", gh.to_str().unwrap())],
    ))
}

#[test]
fn cancel_preserves_ignored_content_metadata_permissions_index_and_other_tasks() {
    let (f, gh, manifest) = fixture();
    let original = std::fs::read(f.shared.join(SOURCE).join(MANIFEST)).unwrap();
    std::fs::set_permissions(
        f.shared.join(SOURCE).join(MANIFEST),
        std::fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    std::fs::create_dir(f.shared.join(SOURCE).join("cache")).unwrap();
    std::fs::write(
        f.shared.join(SOURCE).join("cache/hydrated.bin"),
        b"\0all local bytes\xff",
    )
    .unwrap();
    std::fs::write(f.shared.join("README.md"), "other task pending edit\n").unwrap();
    git(&f.shared, ["add", "README.md"]);
    let index = git(&f.shared, ["ls-files", "--stage"]).stdout;
    invoke(&f.shared, &gh, &manifest, false, false);
    let preview = invoke(&f.shared, &gh, &manifest, true, true);
    assert_eq!(preview["status"], "dry_run");
    assert!(f.shared.join(DEST).exists());
    assert!(!f.shared.join(SOURCE).exists());
    // Content created after moving is preserved too: cancellation never cleans.
    std::fs::write(f.shared.join(DEST).join("cache/new-after-move"), "keep me").unwrap();
    assert_eq!(
        invoke(&f.shared, &gh, &manifest, true, false)["status"],
        "cancelled"
    );
    assert_eq!(
        std::fs::read(f.shared.join(SOURCE).join(MANIFEST)).unwrap(),
        original
    );
    assert_eq!(
        std::fs::metadata(f.shared.join(SOURCE).join(MANIFEST))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o640
    );
    assert_eq!(
        std::fs::read(f.shared.join(SOURCE).join("cache/hydrated.bin")).unwrap(),
        b"\0all local bytes\xff"
    );
    assert_eq!(
        std::fs::read_to_string(f.shared.join(SOURCE).join("cache/new-after-move")).unwrap(),
        "keep me"
    );
    assert!(!f.shared.join(SOURCE).join(RECEIPT).exists());
    assert!(
        !f.shared.join("2026").exists(),
        "cancel removes only its newly created empty parent directories"
    );
    assert_eq!(git(&f.shared, ["ls-files", "--stage"]).stdout, index);
    assert_eq!(
        std::fs::read_to_string(f.shared.join("README.md")).unwrap(),
        "other task pending edit\n"
    );
    assert_eq!(
        invoke(&f.shared, &gh, &manifest, true, false)["status"],
        "no_changes"
    );
    invoke(&f.shared, &gh, &manifest, false, false);
    invoke(&f.shared, &gh, &manifest, true, false);
}

#[test]
fn cancel_restores_opaque_nested_absolute_git_worktree_and_exact_control_bytes() {
    let (f, gh, manifest) = fixture();
    let primary = f.shared.join(SOURCE).join("cache/nested-origin");
    command(
        &f.root,
        "git",
        ["init", "-b", "main", primary.to_str().unwrap()],
    );
    configure_git(&primary);
    std::fs::write(primary.join("content"), "nested repository").unwrap();
    git(&primary, ["add", "."]);
    git(&primary, ["commit", "-m", "Nested"]);
    let nested = f.shared.join(SOURCE).join("nested");
    git(
        &primary,
        ["worktree", "add", "--detach", nested.to_str().unwrap()],
    );
    let pointer = std::fs::read(nested.join(".git")).unwrap();
    let admin = String::from_utf8(git(&nested, ["rev-parse", "--absolute-git-dir"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    let backlink = std::fs::read(Path::new(&admin).join("gitdir")).unwrap();
    invoke(&f.shared, &gh, &manifest, false, false);
    assert_eq!(
        std::fs::read(f.shared.join(DEST).join("nested/.git")).unwrap(),
        pointer
    );
    assert_eq!(
        std::fs::read(
            f.shared
                .join(DEST)
                .join("cache/nested-origin/.git/worktrees/nested/gitdir")
        )
        .unwrap(),
        backlink
    );
    assert!(
        !git_unchecked(
            &f.shared.join(DEST).join("nested"),
            ["status", "--porcelain"]
        )
        .status
        .success()
    );
    invoke(&f.shared, &gh, &manifest, true, false);
    git(&nested, ["status", "--porcelain"]);
    assert_eq!(std::fs::read(nested.join(".git")).unwrap(), pointer);
    assert_eq!(
        std::fs::read(Path::new(&admin).join("gitdir")).unwrap(),
        backlink
    );
}

#[test]
fn cancel_refuses_collisions_and_independently_edited_metadata() {
    let (f, gh, manifest) = fixture();
    invoke(&f.shared, &gh, &manifest, false, false);
    let path = f.shared.join(DEST).join(MANIFEST);
    let current = std::fs::read_to_string(&path).unwrap();
    for edited in [
        current.replace("Completed task", "Changed task title"),
        format!("{current}# Preserve this independent comment\n"),
    ] {
        std::fs::write(&path, &edited).unwrap();
        let result = workspace_env_unchecked(
            &f.shared,
            [
                "archive",
                SOURCE,
                "--cancel",
                "--manifest",
                manifest.to_str().unwrap(),
            ],
            &[("WORKSPACE_MGR_TEST_GH", gh.to_str().unwrap())],
        );
        assert_eq!(result.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&result.stderr).contains("edited after moving"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), edited);
    }
    std::fs::write(&path, current).unwrap();
    std::fs::create_dir(f.shared.join(SOURCE)).unwrap();
    let result = workspace_env_unchecked(
        &f.shared,
        [
            "archive",
            SOURCE,
            "--cancel",
            "--manifest",
            manifest.to_str().unwrap(),
        ],
        &[("WORKSPACE_MGR_TEST_GH", gh.to_str().unwrap())],
    );
    assert_eq!(result.status.code(), Some(2));
    assert!(f.shared.join(DEST).exists());
}

#[test]
fn cancel_after_rejected_push_restores_tree_and_only_its_retirement_queue() {
    let (f, gh, manifest) = fixture();
    invoke(&f.shared, &gh, &manifest, false, false);
    std::fs::write(f.shared.join("README.md"), "other approved work\n").unwrap();
    let hook = f.remote.join("hooks/pre-receive");
    std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let publish = workspace_env_unchecked(
        &f.shared,
        [
            "publish",
            "--manifest",
            manifest.to_str().unwrap(),
            "-m",
            "Archive completed task",
        ],
        &[("WORKSPACE_MGR_TEST_GH", gh.to_str().unwrap())],
    );
    assert_eq!(
        publish.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&publish.stderr)
    );
    assert!(
        String::from_utf8_lossy(&publish.stderr).contains("failed to push"),
        "{}",
        String::from_utf8_lossy(&publish.stderr)
    );
    let local_before = git(
        &f.shared,
        ["rev-parse", "refs/heads/codex/infra-archive-test"],
    )
    .stdout;
    let state = f.shared.join(".workspace-mgr/local/s3-purge.json");
    std::fs::write(&state, json!({"schema_version":1,"pending":[
        {"pointer":format!("{SOURCE}/{RECEIPT}"),"object":format!("{SOURCE}/remote.bin"),"version_id":"old-version"},
        {"pointer":"other-task/data.dvc","object":"other-task/data","version_id":"other-version"}
    ]}).to_string()).unwrap();
    invoke(&f.shared, &gh, &manifest, true, false);
    assert!(f.shared.join(SOURCE).is_dir());
    let queue: Value = serde_json::from_slice(&std::fs::read(state).unwrap()).unwrap();
    assert_eq!(queue["pending"].as_array().unwrap().len(), 1);
    assert_eq!(queue["pending"][0]["object"], "other-task/data");
    let reference = "refs/heads/codex/infra-archive-test";
    assert_ne!(
        git(&f.shared, ["rev-parse", reference]).stdout,
        local_before
    );
    assert_eq!(
        String::from_utf8(git(&f.shared, ["show", &format!("{reference}:README.md")]).stdout)
            .unwrap(),
        "other approved work\n"
    );
    assert!(
        git_unchecked(
            &f.shared,
            [
                "cat-file",
                "-e",
                &format!("{reference}:{SOURCE}/{MANIFEST}")
            ]
        )
        .status
        .success()
    );
    assert!(
        !git_unchecked(
            &f.shared,
            ["cat-file", "-e", &format!("{reference}:{DEST}/{RECEIPT}")]
        )
        .status
        .success()
    );
    assert_eq!(
        std::fs::read_to_string(f.shared.join("README.md")).unwrap(),
        "other approved work\n"
    );
    // Model interruption after the generated undo ref became durable but
    // before the final cancellation state was saved.
    let journal = attempt_journal(&f);
    let mut attempt: Value = serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
    attempt["status"] = "canceling".into();
    std::fs::write(journal, serde_json::to_vec(&attempt).unwrap()).unwrap();
    assert_eq!(
        invoke(&f.shared, &gh, &manifest, true, false)["status"],
        "cancelled"
    );
    assert_eq!(
        invoke(&f.shared, &gh, &manifest, true, false)["status"],
        "no_changes"
    );
}

#[test]
fn cancel_restores_exact_previous_receipt_and_rebound_dvc_pointer_bytes() {
    let (f, gh, manifest) = fixture_with_metadata(true);
    let source = f.shared.join(SOURCE);
    let pointer = "metadata/data.bin.dvc";
    let before_pointer = std::fs::read(source.join(pointer)).unwrap();
    let before_receipt = std::fs::read(source.join(RECEIPT)).unwrap();
    std::fs::set_permissions(source.join(pointer), std::fs::Permissions::from_mode(0o640)).unwrap();
    invoke(&f.shared, &gh, &manifest, false, false);
    let moved = f.shared.join(DEST).join(pointer);
    let raw = std::fs::read_to_string(&moved).unwrap();
    let rendered = raw
        .replace("original-v1", "copied-v1")
        .replace("original-etag", "copied-etag");
    // Model the durable authorization written by record_pointer_rewrite
    // before the transport replaces this pointer; no bucket is contacted.
    let journal = attempt_journal(&f);
    let mut attempt: Value = serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
    let metadata = attempt["metadata"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|metadata| metadata["path"] == pointer)
        .unwrap();
    metadata["generated"] = json!([rendered.as_bytes()]);
    std::fs::write(&journal, serde_json::to_vec(&attempt).unwrap()).unwrap();
    std::fs::write(&moved, rendered).unwrap();
    invoke(&f.shared, &gh, &manifest, true, false);
    assert_eq!(std::fs::read(source.join(pointer)).unwrap(), before_pointer);
    assert_eq!(std::fs::read(source.join(RECEIPT)).unwrap(), before_receipt);
    assert_eq!(
        std::fs::metadata(source.join(pointer))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o640
    );
}

#[test]
fn cancel_refuses_a_successfully_pushed_archive() {
    let (f, gh, manifest) = fixture();
    invoke(&f.shared, &gh, &manifest, false, false);
    workspace_env(
        &f.shared,
        [
            "publish",
            "--manifest",
            manifest.to_str().unwrap(),
            "-m",
            "Archive completed task",
        ],
        &[("WORKSPACE_MGR_TEST_GH", gh.to_str().unwrap())],
    );
    let cancel = workspace_env_unchecked(
        &f.shared,
        [
            "archive",
            SOURCE,
            "--cancel",
            "--manifest",
            manifest.to_str().unwrap(),
        ],
        &[("WORKSPACE_MGR_TEST_GH", gh.to_str().unwrap())],
    );
    assert_eq!(cancel.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&cancel.stderr).contains("was pushed"));
    assert!(f.shared.join(DEST).exists());
}

fn attempt_journal(fixture: &GitFixture) -> PathBuf {
    std::fs::read_dir(fixture.shared.join(".workspace-mgr/local/archive-attempts"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .unwrap()
}

#[test]
fn interrupted_terminal_cancel_preserves_a_later_owners_merged_archive_and_claims() {
    let (f, gh, manifest) = fixture();
    let original_manifest = std::fs::read(f.shared.join(SOURCE).join(MANIFEST)).unwrap();
    std::fs::write(
        f.shared.join("README.md"),
        "another task's retained local edit\n",
    )
    .unwrap();
    git(&f.shared, ["add", "README.md"]);
    let index_before = git(&f.shared, ["ls-files", "--stage"]).stdout;
    invoke(&f.shared, &gh, &manifest, false, false);
    invoke(&f.shared, &gh, &manifest, true, false);
    let journal = attempt_journal(&f);
    let mut attempt: Value = serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
    // The process lost its final state write after durable cleanup, local undo
    // and claim release. A different checkout can now publish a new attempt.
    attempt["status"] = "canceling".into();
    attempt["remote_cleanup_complete"] = true.into();
    std::fs::write(&journal, serde_json::to_vec(&attempt).unwrap()).unwrap();

    std::fs::create_dir_all(f.seed.join("2026/07")).unwrap();
    git(&f.seed, ["mv", SOURCE, DEST]);
    let later_receipt = json!({
        "schema_version":1,"task_id":SOURCE,"source":SOURCE,"destination":DEST,
        "status":"copied","bucket":"isolated-fixture","remote_prefix":"dvc",
        "transaction_id":"later-owner","versions":[]
    });
    std::fs::write(
        f.seed.join(DEST).join(RECEIPT),
        serde_json::to_vec_pretty(&later_receipt).unwrap(),
    )
    .unwrap();
    let later_manifest = std::fs::read_to_string(f.seed.join(DEST).join(MANIFEST))
        .unwrap()
        .replace(
            &format!("path = \"{SOURCE}\""),
            &format!("path = \"{DEST}\""),
        );
    std::fs::write(f.seed.join(DEST).join(MANIFEST), later_manifest).unwrap();
    f.commit_seed("Merge a later owner's archive after old claims were released");
    let receipt_blob =
        String::from_utf8(git(&f.seed, ["rev-parse", &format!("HEAD:{DEST}/{RECEIPT}")]).stdout)
            .unwrap();
    for namespace in ["archive-copy", "archive-registry"] {
        git(
            &f.remote,
            [
                "update-ref",
                &format!("refs/tags/workspace-mgr/{namespace}/later-owner"),
                receipt_blob.trim(),
            ],
        );
    }
    let refs_before = git(
        &f.remote,
        ["for-each-ref", "--format=%(refname) %(objectname)"],
    )
    .stdout;
    let later_tree_before = git(&f.remote, ["rev-parse", "refs/heads/main^{tree}"]).stdout;

    assert_eq!(
        invoke(&f.shared, &gh, &manifest, true, true)["status"],
        "dry_run"
    );
    assert_eq!(
        invoke(&f.shared, &gh, &manifest, true, false)["status"],
        "cancelled"
    );
    assert_eq!(
        invoke(&f.shared, &gh, &manifest, true, false)["status"],
        "no_changes"
    );
    assert_eq!(
        git(
            &f.remote,
            ["for-each-ref", "--format=%(refname) %(objectname)"]
        )
        .stdout,
        refs_before
    );
    assert_eq!(
        git(&f.remote, ["rev-parse", "refs/heads/main^{tree}"]).stdout,
        later_tree_before
    );
    assert_eq!(
        std::fs::read(f.shared.join(SOURCE).join(MANIFEST)).unwrap(),
        original_manifest
    );
    assert!(!f.shared.join(DEST).exists());
    assert_eq!(git(&f.shared, ["ls-files", "--stage"]).stdout, index_before);
    assert_eq!(
        std::fs::read_to_string(f.shared.join("README.md")).unwrap(),
        "another task's retained local edit\n"
    );
    let finished: Value = serde_json::from_slice(&std::fs::read(journal).unwrap()).unwrap();
    assert_eq!(finished["status"], "cancelled");
}

#[test]
fn resumed_cancel_still_refuses_independently_edited_metadata() {
    let (f, gh, manifest) = fixture();
    invoke(&f.shared, &gh, &manifest, false, false);
    let journal = attempt_journal(&f);
    let mut attempt: Value = serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
    attempt["status"] = "canceling".into();
    std::fs::write(&journal, serde_json::to_vec(&attempt).unwrap()).unwrap();
    let metadata = f.shared.join(DEST).join(MANIFEST);
    let current = std::fs::read_to_string(&metadata).unwrap();
    let independently_edited = current.replace("Completed task", "Preserve this independent edit");
    std::fs::write(&metadata, &independently_edited).unwrap();
    for dry_run in [true, false] {
        let mut args = vec![
            "archive",
            SOURCE,
            "--cancel",
            "--manifest",
            manifest.to_str().unwrap(),
        ];
        if dry_run {
            args.push("--dry-run");
        }
        let result = workspace_env_unchecked(
            &f.shared,
            args,
            &[("WORKSPACE_MGR_TEST_GH", gh.to_str().unwrap())],
        );
        assert_eq!(result.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&result.stderr).contains("edited after moving"));
        assert_eq!(
            std::fs::read_to_string(&metadata).unwrap(),
            independently_edited
        );
        assert!(!f.shared.join(SOURCE).exists());
        assert!(f.shared.join(DEST).exists());
    }
}

#[test]
fn resumed_cancel_checks_metadata_after_directory_has_already_returned() {
    let (f, gh, manifest) = fixture();
    let original_manifest = std::fs::read(f.shared.join(SOURCE).join(MANIFEST)).unwrap();
    invoke(&f.shared, &gh, &manifest, false, false);
    let journal = attempt_journal(&f);
    let mut attempt: Value = serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
    attempt["status"] = "canceling".into();
    std::fs::write(&journal, serde_json::to_vec(&attempt).unwrap()).unwrap();
    std::fs::rename(f.shared.join(DEST), f.shared.join(SOURCE)).unwrap();
    std::fs::write(f.shared.join(SOURCE).join(MANIFEST), &original_manifest).unwrap();
    std::fs::remove_file(f.shared.join(SOURCE).join(RECEIPT)).unwrap();
    let metadata = f.shared.join(SOURCE).join(MANIFEST);
    let edited = format!(
        "{}# Preserve this post-interruption edit\n",
        String::from_utf8(original_manifest.clone()).unwrap()
    );
    std::fs::write(&metadata, &edited).unwrap();
    let args = [
        "archive",
        SOURCE,
        "--cancel",
        "--manifest",
        manifest.to_str().unwrap(),
    ];
    let environment = [("WORKSPACE_MGR_TEST_GH", gh.to_str().unwrap())];
    let refused = workspace_env_unchecked(&f.shared, args, &environment);
    assert_eq!(refused.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("edited after moving"));
    assert_eq!(std::fs::read_to_string(&metadata).unwrap(), edited);
    assert!(!f.shared.join(DEST).exists());
    std::fs::write(&metadata, original_manifest).unwrap();
    assert_eq!(
        json(&workspace_env(&f.shared, args, &environment))["status"],
        "cancelled"
    );
}

#[test]
fn completed_cancel_preserves_empty_parents_recreated_by_other_tasks() {
    let (f, gh, manifest) = fixture();
    invoke(&f.shared, &gh, &manifest, false, false);
    invoke(&f.shared, &gh, &manifest, true, false);
    assert!(!f.shared.join("2026").exists());
    std::fs::create_dir_all(f.shared.join("2026/07")).unwrap();
    assert_eq!(
        invoke(&f.shared, &gh, &manifest, true, false)["status"],
        "no_changes"
    );
    assert!(f.shared.join("2026/07").is_dir());
}

#[test]
fn interrupted_parent_cleanup_keeps_canceling_state_and_retries_safely() {
    let (f, gh, manifest) = fixture();
    invoke(&f.shared, &gh, &manifest, false, false);
    let parent = f.shared.join("2026/07");
    let permissions = std::fs::metadata(&parent).unwrap().permissions();
    // Deleting the empty month folder requires write permission on its year
    // folder; restoring the task itself uses its original repository parent.
    let year = f.shared.join("2026");
    let year_permissions = std::fs::metadata(&year).unwrap().permissions();
    std::fs::set_permissions(&year, std::fs::Permissions::from_mode(0o555)).unwrap();
    let refused = workspace_env_unchecked(
        &f.shared,
        [
            "archive",
            SOURCE,
            "--cancel",
            "--manifest",
            manifest.to_str().unwrap(),
        ],
        &[("WORKSPACE_MGR_TEST_GH", gh.to_str().unwrap())],
    );
    if year.exists() {
        std::fs::set_permissions(&year, year_permissions).unwrap();
    }
    // An administrator may bypass Unix directory write permissions, so this
    // fixture's interruption assertion applies when the kernel denies removal.
    if refused.status.success() {
        assert_eq!(json(&refused)["status"], "cancelled");
        return;
    }
    let attempt: Value =
        serde_json::from_slice(&std::fs::read(attempt_journal(&f)).unwrap()).unwrap();
    assert_eq!(attempt["status"], "canceling");
    assert!(f.shared.join(SOURCE).join(MANIFEST).is_file());
    assert!(!f.shared.join(DEST).exists());
    assert!(parent.is_dir());
    assert_eq!(
        std::fs::metadata(&parent).unwrap().permissions().mode(),
        permissions.mode()
    );
    assert_eq!(
        invoke(&f.shared, &gh, &manifest, true, false)["status"],
        "cancelled"
    );
    assert!(!year.exists());
}

#[test]
fn cancel_rejects_a_dvc_metadata_ancestor_symlink_before_external_writes() {
    use std::os::unix::fs::symlink;
    let (f, gh, manifest) = fixture();
    invoke(&f.shared, &gh, &manifest, false, false);
    let journal = attempt_journal(&f);
    let mut attempt: Value = serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
    // Model an existing, snapshotted DVC pointer in a interrupted attempt. No
    // storage adapter or bucket is used; the regression concerns local writes.
    let original = b"outs:\n- path: payload.bin\n  md5: 00000000000000000000000000000000\n";
    attempt["metadata"].as_array_mut().unwrap().push(json!({
        "path":"metadata/payload.bin.dvc", "before":original.as_slice(), "unix_mode":0o100644
    }));
    std::fs::write(&journal, serde_json::to_vec(&attempt).unwrap()).unwrap();
    let external = f.root.join("another-task-metadata");
    std::fs::create_dir(&external).unwrap();
    std::fs::write(external.join("payload.bin.dvc"), original).unwrap();
    symlink(&external, f.shared.join(DEST).join("metadata")).unwrap();
    for dry_run in [true, false] {
        let mut args = vec![
            "archive",
            SOURCE,
            "--cancel",
            "--manifest",
            manifest.to_str().unwrap(),
        ];
        if dry_run {
            args.push("--dry-run");
        }
        let result = workspace_env_unchecked(
            &f.shared,
            args,
            &[("WORKSPACE_MGR_TEST_GH", gh.to_str().unwrap())],
        );
        assert_eq!(result.status.code(), Some(2));
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("symlink"),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            std::fs::read(external.join("payload.bin.dvc")).unwrap(),
            original
        );
        assert!(!f.shared.join(SOURCE).exists());
        assert!(f.shared.join(DEST).exists());
    }
}

#[test]
fn batch_cancel_preserves_other_scoped_work_and_durable_generated_commit_allowlists() {
    const SECOND: &str = "20260712-120100-second";
    const SECOND_DEST: &str = "2026/07/20260712-120100-second";
    let (f, gh, manifest) = fixture();
    let original_rows = std::process::Command::new(&gh).output().unwrap();
    let mut rows: Value = serde_json::from_slice(&original_rows.stdout).unwrap();
    git(&f.seed, ["switch", "-c", "codex/second"]);
    let task = f.seed.join(SECOND);
    std::fs::create_dir(&task).unwrap();
    std::fs::write(task.join(MANIFEST), format!("schema_version = 2\nkind = \"deliverable\"\nid = \"{SECOND}\"\nslug = \"second\"\npath = \"{SECOND}\"\nbranch = \"codex/second\"\ntitle = \"Second completed task\"\npurpose = \"Retain results\"\nadditional_scopes = []\n")).unwrap();
    std::fs::write(task.join("README.md"), "# Second\n").unwrap();
    std::fs::write(task.join("result.md"), "second original result\n").unwrap();
    git(&f.seed, ["add", SECOND]);
    git(&f.seed, ["commit", "-m", "Complete second task"]);
    let head = String::from_utf8(git(&f.seed, ["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    git(&f.seed, ["push", "origin", "codex/second"]);
    git(&f.seed, ["switch", "main"]);
    git(&f.seed, ["merge", "--squash", "codex/second"]);
    git(&f.seed, ["commit", "-m", "Merge second completed task"]);
    let merge = String::from_utf8(git(&f.seed, ["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    git(&f.seed, ["push", "origin", "main"]);
    git(&f.shared, ["fetch", "origin"]);
    git(&f.shared, ["reset", "--hard", "origin/main"]);
    git(
        &f.shared,
        ["update-ref", "refs/heads/codex/infra-archive-test", &merge],
    );
    rows.as_array_mut().unwrap().push(json!({
        "number":2,"url":"https://example.invalid/owner/repo/pull/2","state":"MERGED",
        "mergedAt":"2026-07-12T20:01:00Z","mergeCommit":{"oid":merge},
        "headRefName":"codex/second","headRefOid":head,"baseRefName":"main","isCrossRepository":false
    }));
    std::fs::write(
        &gh,
        format!("#!/usr/bin/env python3\nprint({:?})\n", rows.to_string()),
    )
    .unwrap();
    let original_manifest = std::fs::read_to_string(&manifest).unwrap();
    std::fs::write(&manifest, format!("{original_manifest}\n[[additional_scopes]]\npath = \"{SECOND}\"\nreason = \"User requested the second completed task\"\n\n[[additional_scopes]]\npath = \"{SECOND_DEST}\"\nreason = \"User requested its archive destination\"\n")).unwrap();
    let environment = [("WORKSPACE_MGR_TEST_GH", gh.to_str().unwrap())];
    workspace_env(
        &f.shared,
        [
            "archive",
            SOURCE,
            SECOND,
            "--manifest",
            manifest.to_str().unwrap(),
        ],
        &environment,
    );
    std::fs::write(f.shared.join("README.md"), "other approved work\n").unwrap();
    let hook = f.remote.join("hooks/pre-receive");
    std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let publish = workspace_env_unchecked(
        &f.shared,
        [
            "publish",
            "--manifest",
            manifest.to_str().unwrap(),
            "-m",
            "Archive two completed tasks",
        ],
        &environment,
    );
    assert_eq!(publish.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&publish.stderr).contains("failed to push"));
    let index_before = git(&f.shared, ["ls-files", "--stage"]).stdout;
    let cancelled = json(&workspace_env(
        &f.shared,
        [
            "archive",
            SOURCE,
            SECOND,
            "--cancel",
            "--manifest",
            manifest.to_str().unwrap(),
        ],
        &environment,
    ));
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(cancelled["tasks"].as_array().unwrap().len(), 2);
    for (source, destination) in [(SOURCE, DEST), (SECOND, SECOND_DEST)] {
        assert!(f.shared.join(source).join(MANIFEST).is_file());
        assert!(!f.shared.join(destination).exists());
        let reference = "refs/heads/codex/infra-archive-test";
        assert!(
            git_unchecked(
                &f.shared,
                [
                    "cat-file",
                    "-e",
                    &format!("{reference}:{source}/{MANIFEST}")
                ]
            )
            .status
            .success()
        );
        assert!(
            !git_unchecked(
                &f.shared,
                [
                    "cat-file",
                    "-e",
                    &format!("{reference}:{destination}/{RECEIPT}")
                ]
            )
            .status
            .success()
        );
    }
    assert_eq!(
        String::from_utf8(
            git(
                &f.shared,
                ["show", "refs/heads/codex/infra-archive-test:README.md"]
            )
            .stdout
        )
        .unwrap(),
        "other approved work\n"
    );
    assert_eq!(
        std::fs::read_to_string(f.shared.join("README.md")).unwrap(),
        "other approved work\n"
    );
    assert_eq!(git(&f.shared, ["ls-files", "--stage"]).stdout, index_before);
    assert_eq!(
        json(&workspace_env(
            &f.shared,
            [
                "archive",
                SOURCE,
                SECOND,
                "--cancel",
                "--manifest",
                manifest.to_str().unwrap()
            ],
            &environment
        ))["status"],
        "no_changes"
    );
}

#[test]
fn cancel_preserves_independent_receipt_notes_and_identity_edits() {
    let (f, gh, manifest) = fixture();
    invoke(&f.shared, &gh, &manifest, false, false);
    let path = f.shared.join(DEST).join(RECEIPT);
    let expected: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for (key, edited_value) in [
        ("note", json!("Preserve this independent note")),
        ("task_id", json!("different-task")),
    ] {
        let mut edited = expected.clone();
        edited[key] = edited_value;
        let bytes = serde_json::to_vec_pretty(&edited).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        for dry_run in [true, false] {
            let mut args = vec![
                "archive",
                SOURCE,
                "--cancel",
                "--manifest",
                manifest.to_str().unwrap(),
            ];
            if dry_run {
                args.push("--dry-run");
            }
            let result = workspace_env_unchecked(
                &f.shared,
                args,
                &[("WORKSPACE_MGR_TEST_GH", gh.to_str().unwrap())],
            );
            assert_eq!(result.status.code(), Some(2));
            assert!(
                String::from_utf8_lossy(&result.stderr).contains("receipt was edited after moving")
            );
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert!(!f.shared.join(SOURCE).exists());
        }
    }
}
