mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::*;

const TASK: &str = "20260914-120000-local-storage";

fn fixture() -> (GitFixture, PathBuf) {
    fixture_with_storage(false)
}

fn fixture_with_storage(s3: bool) -> (GitFixture, PathBuf) {
    let fixture = GitFixture::new();
    if s3 {
        workspace(
            &fixture.seed,
            [
                "init",
                "--s3-url",
                fixture.root.join("storage-remote").to_str().unwrap(),
            ],
        );
    } else {
        workspace(&fixture.seed, ["init"]);
    }
    fixture.commit_seed("Initialize workspace");
    fixture.clone_shared();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "local-storage",
            "--title",
            "Local storage",
            "--purpose",
            "Exercise local-only storage boundaries.",
            "--timestamp",
            "20260914-120000",
        ],
    );
    let task = fixture.shared.join(TASK);
    (fixture, task)
}

fn path(name: &str) -> String {
    format!("{TASK}/{name}")
}

fn ignored(repo: &Path, path: &str) -> bool {
    git_unchecked(repo, ["check-ignore", "--no-index", "--quiet", "--", path])
        .status
        .success()
}

#[test]
fn untrack_preserves_payload_and_index_and_generates_exact_owned_ignore() {
    let (fixture, task) = fixture();
    let name = "[draft] !data*.bin";
    let payload = path(name);
    fs::write(task.join(name), b"local payload\0bytes").unwrap();
    fs::write(task.join("draft !dataA.bin"), b"neighbor").unwrap();
    fs::write(
        task.join(".gitignore"),
        b"# User rules without final newline",
    )
    .unwrap();
    git(&fixture.shared, ["add", "--", &payload]);
    let index = fs::read(fixture.shared.join(".git/index")).unwrap();
    let preview = json(&workspace(&task, ["untrack", &payload, "--dry-run"]));
    assert_eq!(preview["placements"][0]["target"], "local");
    assert!(
        !task
            .join(format!("{name}.workspace-mgr-storage.toml"))
            .exists()
    );
    assert_eq!(
        fs::read(task.join(".gitignore")).unwrap(),
        b"# User rules without final newline"
    );
    assert_eq!(fs::read(fixture.shared.join(".git/index")).unwrap(), index);

    workspace(&task, ["untrack", &payload]);
    let rules = fs::read(task.join(".gitignore")).unwrap();
    assert!(ignored(&fixture.shared, &payload));
    assert!(!ignored(&fixture.shared, &path("draft !dataA.bin")));
    assert_eq!(fs::read(task.join(name)).unwrap(), b"local payload\0bytes");
    assert_eq!(fs::read(fixture.shared.join(".git/index")).unwrap(), index);
    workspace(&task, ["untrack", &payload]);
    assert_eq!(fs::read(task.join(".gitignore")).unwrap(), rules);
    assert_eq!(
        json(&workspace(&task, ["storage", "status", &payload]))["placements"][0]["target"],
        "local"
    );

    workspace(
        &task,
        [
            "storage",
            "set",
            &payload,
            "--to",
            "git",
            "--reason",
            "Share again",
            "--dry-run",
        ],
    );
    assert_eq!(fs::read(task.join(".gitignore")).unwrap(), rules);
    workspace(
        &task,
        [
            "storage",
            "set",
            &payload,
            "--to",
            "git",
            "--reason",
            "Share again",
        ],
    );
    assert!(!ignored(&fixture.shared, &payload));
    assert_eq!(
        fs::read(task.join(".gitignore")).unwrap(),
        b"# User rules without final newline"
    );
    assert_eq!(
        json(&workspace(&task, ["storage", "status", &payload]))["placements"][0]["target"],
        "git"
    );
}

#[test]
fn retrack_refuses_remaining_user_ignore_and_reset_requires_explicit_target() {
    let (_fixture, task) = fixture();
    fs::write(task.join("private.bin"), b"keep me").unwrap();
    fs::write(task.join(".gitignore"), b"*.bin\n").unwrap();
    let payload = path("private.bin");
    workspace(&task, ["untrack", &payload]);
    let rules = fs::read(task.join(".gitignore")).unwrap();
    let metadata = fs::read(task.join("private.bin.workspace-mgr-storage.toml")).unwrap();
    for extra in [vec![], vec!["--dry-run"]] {
        let mut args = vec![
            "storage",
            "set",
            &payload,
            "--to",
            "git",
            "--reason",
            "Share again",
        ];
        args.extend(extra);
        let output = workspace_unchecked(&task, args);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("user Git ignore rules"));
        assert_eq!(fs::read(task.join(".gitignore")).unwrap(), rules);
        assert_eq!(
            fs::read(task.join("private.bin.workspace-mgr-storage.toml")).unwrap(),
            metadata
        );
    }
    let reset = workspace_unchecked(&task, ["storage", "reset", &payload]);
    assert!(!reset.status.success());
    assert!(String::from_utf8_lossy(&reset.stderr).contains("resume tracking explicitly"));
    workspace(&task, ["remove", &payload]);
    assert!(!task.join("private.bin").exists());
    assert!(!task.join("private.bin.workspace-mgr-storage.toml").exists());
    assert_eq!(fs::read(task.join(".gitignore")).unwrap(), b"*.bin\n");
}

#[test]
fn complete_directory_boundary_is_supported_and_nested_operations_are_refused() {
    let (fixture, task) = fixture();
    fs::create_dir_all(task.join("bundle/nested")).unwrap();
    fs::write(task.join("bundle/nested/file.bin"), b"one").unwrap();
    let boundary = path("bundle");
    let refusal = workspace_unchecked(&task, ["untrack", &boundary]);
    assert!(!refusal.status.success());
    assert!(String::from_utf8_lossy(&refusal.stderr).contains("not a complete storage boundary"));
    workspace(
        &task,
        [
            "storage",
            "set",
            &boundary,
            "--to",
            "git",
            "--reason",
            "Directory boundary",
        ],
    );
    let nested = workspace_unchecked(&task, ["untrack", &path("bundle/nested/file.bin")]);
    assert!(!nested.status.success());
    assert!(String::from_utf8_lossy(&nested.stderr).contains("existing placement boundary"));
    workspace(&task, ["untrack", &boundary]);
    assert!(ignored(&fixture.shared, &path("bundle/nested/file.bin")));
    assert_eq!(
        fs::read(task.join("bundle/nested/file.bin")).unwrap(),
        b"one"
    );
    fs::write(task.join("bundle/.gitignore"), b"*.bin\n").unwrap();
    fs::write(task.join("bundle/generated.dvc"), b"ordinary local file\n").unwrap();
    workspace(&task, ["untrack", &boundary]);
    let restore = workspace_unchecked(
        &task,
        [
            "storage",
            "set",
            &boundary,
            "--to",
            "git",
            "--reason",
            "Share directory",
        ],
    );
    assert!(!restore.status.success());
    assert!(String::from_utf8_lossy(&restore.stderr).contains("user Git ignore rules"));
    let moved = workspace_unchecked(&task, ["move", &boundary, &path("elsewhere")]);
    assert!(!moved.status.success());
    assert!(String::from_utf8_lossy(&moved.stderr).contains("local-only"));
}

#[test]
fn untrack_rejects_control_metadata_hidden_metadata_and_batch_failure_without_mutation() {
    let (_fixture, task) = fixture();
    fs::write(task.join("good.bin"), b"keep").unwrap();
    for target in [
        path("README.md"),
        path(".workspace-mgr-task.toml"),
        TASK.to_owned(),
    ] {
        assert!(
            !workspace_unchecked(&task, ["untrack", &target])
                .status
                .success()
        );
    }
    let failed = workspace_unchecked(&task, ["untrack", &path("good.bin"), &path("missing.bin")]);
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("hydrate"));
    assert!(!task.join("good.bin.workspace-mgr-storage.toml").exists());
    assert!(!task.join(".gitignore").exists());
    fs::write(task.join(".gitignore"), b"*.toml\n").unwrap();
    let hidden = workspace_unchecked(&task, ["untrack", &path("good.bin"), "--dry-run"]);
    assert!(!hidden.status.success());
    assert!(String::from_utf8_lossy(&hidden.stderr).contains("local-only metadata"));
    assert!(!task.join("good.bin.workspace-mgr-storage.toml").exists());
}

#[test]
fn absent_payload_still_has_local_status_and_idempotent_untrack() {
    let (_fixture, task) = fixture();
    fs::write(task.join("private.bin"), b"local").unwrap();
    workspace(&task, ["untrack", &path("private.bin")]);
    fs::remove_file(task.join("private.bin")).unwrap();
    let status = json(&workspace(
        &task,
        ["storage", "status", &path("private.bin")],
    ));
    assert_eq!(status["placements"][0]["target"], "local");
    assert!(status["placements"][0].get("payload_bytes").is_none());
    workspace(&task, ["untrack", &path("private.bin")]);
}

#[cfg(unix)]
#[test]
fn untrack_rejects_symlink_ignore_files_and_symlink_payloads() {
    let (fixture, task) = fixture();
    fs::write(task.join("private.bin"), b"local").unwrap();
    fs::write(fixture.root.join("external-ignore"), b"unchanged\n").unwrap();
    std::os::unix::fs::symlink(
        fixture.root.join("external-ignore"),
        task.join(".gitignore"),
    )
    .unwrap();
    assert!(
        !workspace_unchecked(&task, ["untrack", &path("private.bin")])
            .status
            .success()
    );
    assert_eq!(
        fs::read(fixture.root.join("external-ignore")).unwrap(),
        b"unchanged\n"
    );
    fs::remove_file(task.join(".gitignore")).unwrap();
    std::os::unix::fs::symlink(task.join("private.bin"), task.join("linked.bin")).unwrap();
    assert!(
        !workspace_unchecked(&task, ["untrack", &path("linked.bin")])
            .status
            .success()
    );
}

#[cfg(feature = "test-storage")]
#[test]
fn s3_untrack_keeps_payload_and_retrack_restores_pointer() {
    if which::which("dvc").is_err() {
        return;
    }
    let (_fixture, task) = fixture_with_storage(true);
    fs::write(task.join("cloud.bin"), b"cloud payload").unwrap();
    let payload = path("cloud.bin");
    workspace(
        &task,
        [
            "storage",
            "set",
            &payload,
            "--to",
            "s3",
            "--reason",
            "Cloud data",
        ],
    );
    fs::remove_file(task.join("cloud.bin")).unwrap();
    let missing = workspace_unchecked(&task, ["untrack", &payload]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("hydrate"));
    fs::write(task.join("cloud.bin"), b"cloud payload").unwrap();
    workspace(&task, ["untrack", &payload]);
    assert!(!task.join("cloud.bin.dvc").exists());
    assert_eq!(fs::read(task.join("cloud.bin")).unwrap(), b"cloud payload");
    workspace(
        &task,
        [
            "storage",
            "set",
            &payload,
            "--to",
            "s3",
            "--reason",
            "Share in cloud again",
        ],
    );
    assert!(task.join("cloud.bin.dvc").is_file());
    assert_eq!(
        json(&workspace(&task, ["storage", "status", &payload]))["placements"][0]["target"],
        "s3"
    );
}

#[cfg(all(feature = "test-storage", unix))]
#[test]
fn failed_s3_untrack_rolls_back_all_metadata_and_keeps_payload() {
    use std::os::unix::fs::PermissionsExt;
    if which::which("dvc").is_err() {
        return;
    }
    let (fixture, task) = fixture_with_storage(true);
    fs::write(task.join("cloud.bin"), b"cloud payload").unwrap();
    let payload = path("cloud.bin");
    workspace(
        &task,
        [
            "storage",
            "set",
            &payload,
            "--to",
            "s3",
            "--reason",
            "Cloud data",
        ],
    );
    let pointer = fs::read(task.join("cloud.bin.dvc")).unwrap();
    let placement = fs::read(task.join("cloud.bin.workspace-mgr-storage.toml")).unwrap();
    let ignore = fs::read(task.join(".gitignore")).unwrap();
    let engine = fixture.root.join("failing-engine");
    fs::write(&engine, "#!/bin/sh\nset -eu\nif [ \"${1:-}\" = \"--version\" ]; then printf '3.67.1\\n'; exit 0; fi\nif [ \"${1:-}\" = \"remove\" ]; then rm -- \"$3\"; printf 'partial mutation\\n' > \"${3%/*}/.gitignore\"; exit 23; fi\nexit 23\n").unwrap();
    fs::set_permissions(&engine, fs::Permissions::from_mode(0o755)).unwrap();
    let output = std::process::Command::new(binary())
        .current_dir(&task)
        .args(["untrack", &payload])
        .env("WORKSPACE_MGR_STORAGE_DVC", &engine)
        .env("WORKSPACE_MGR_FORMAT", "json")
        .env("WORKSPACE_MGR_UPDATE_CHECK_DISABLE", "1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("rolled back"));
    assert_eq!(fs::read(task.join("cloud.bin")).unwrap(), b"cloud payload");
    assert_eq!(fs::read(task.join("cloud.bin.dvc")).unwrap(), pointer);
    assert_eq!(
        fs::read(task.join("cloud.bin.workspace-mgr-storage.toml")).unwrap(),
        placement
    );
    assert_eq!(fs::read(task.join(".gitignore")).unwrap(), ignore);
}
