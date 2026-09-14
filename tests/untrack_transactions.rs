mod common;

#[cfg(feature = "test-storage")]
use std::path::Path;
use std::path::PathBuf;

use common::*;

fn managed_fixture(storage: bool) -> GitFixture {
    let fixture = GitFixture::new();
    if storage {
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
    fixture
}

fn create_task(fixture: &GitFixture, slug: &str) -> (String, PathBuf) {
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            slug,
            "--title",
            "Local retention",
            "--purpose",
            "Verify publication excludes retained local content.",
            "--timestamp",
            "20260914-120000",
        ],
    );
    let task_id = format!("20260914-120000-{slug}");
    let task = fixture.shared.join(&task_id);
    (task_id, task)
}

fn tree_contains(fixture: &GitFixture, oid: &str, path: &str) -> bool {
    git_unchecked(
        &fixture.remote,
        ["cat-file", "-e", &format!("{oid}:{path}")],
    )
    .status
    .success()
}

#[cfg(feature = "test-storage")]
fn remote_snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files = walkdir::WalkDir::new(root)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            (
                entry.path().strip_prefix(root).unwrap().to_path_buf(),
                std::fs::read(entry.path()).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    files.sort();
    files
}

#[test]
fn untrack_removes_published_git_content_without_touching_payload_or_shared_index() {
    let fixture = managed_fixture(false);
    let (task_id, task) = create_task(&fixture, "local-git");
    let name = "[draft] *.bin";
    let path = format!("{task_id}/{name}");
    let retained = task.join(name);
    let original = b"published Git bytes\n";
    std::fs::write(&retained, original).unwrap();
    std::fs::write(task.join("draft other.bin"), "independent file\n").unwrap();
    std::fs::write(task.join(".gitignore"), "# User rules\n/user-cache/\n").unwrap();
    let first = json(&workspace(&task, ["publish", "-m", "Publish Git content"]));
    let first_oid = first["commit_oid"].as_str().unwrap();
    assert!(tree_contains(&fixture, first_oid, &path));

    // A shared index may still contain this path. Publication must remove it
    // from its private index without rewriting the shared index or payload.
    let local_bytes = b"locally edited bytes to retain\n";
    std::fs::write(&retained, local_bytes).unwrap();
    git(&fixture.shared, ["add", "--", &path]);
    let index_path = fixture.shared.join(".git/index");
    let index_before = std::fs::read(&index_path).unwrap();
    let ignore_before = std::fs::read(task.join(".gitignore")).unwrap();
    let sidecar = task.join(format!("{name}.workspace-mgr-storage.toml"));

    let preview = json(&workspace(&task, ["untrack", &path, "--dry-run"]));
    assert_eq!(preview["status"], "dry_run");
    assert_eq!(preview["remote_writes"], false);
    assert!(!sidecar.exists());
    assert_eq!(
        std::fs::read(task.join(".gitignore")).unwrap(),
        ignore_before
    );
    assert_eq!(std::fs::read(&retained).unwrap(), local_bytes);
    assert_eq!(std::fs::read(&index_path).unwrap(), index_before);

    let changed = json(&workspace(&task, ["untrack", &path]));
    assert_eq!(changed["placements"][0]["target"], "local");
    assert_eq!(changed["remote_writes"], false);
    assert_eq!(std::fs::read(&retained).unwrap(), local_bytes);
    assert_eq!(std::fs::read(&index_path).unwrap(), index_before);
    let ignored = git(&fixture.shared, ["check-ignore", "--no-index", "--", &path]);
    assert_eq!(String::from_utf8_lossy(&ignored.stdout).trim(), path);
    assert!(
        !git_unchecked(
            &fixture.shared,
            [
                "check-ignore",
                "--no-index",
                "--",
                &format!("{task_id}/draft other.bin"),
            ],
        )
        .status
        .success()
    );
    let ignore_after = std::fs::read(task.join(".gitignore")).unwrap();
    let sidecar_after = std::fs::read(&sidecar).unwrap();
    workspace(&task, ["untrack", &path]);
    assert_eq!(
        std::fs::read(task.join(".gitignore")).unwrap(),
        ignore_after
    );
    assert_eq!(std::fs::read(&sidecar).unwrap(), sidecar_after);

    let plan = json(&workspace(&task, ["plan"]));
    assert!(
        plan["storage"]["local_only"]
            .as_array()
            .unwrap()
            .contains(&path.clone().into())
    );
    assert!(
        plan["changed_paths"]
            .as_array()
            .unwrap()
            .contains(&path.clone().into())
    );
    let published = json(&workspace(
        &task,
        ["publish", "-m", "Retain content locally"],
    ));
    let oid = published["commit_oid"].as_str().unwrap();
    assert!(!tree_contains(&fixture, oid, &path));
    assert!(tree_contains(
        &fixture,
        oid,
        &format!("{path}.workspace-mgr-storage.toml")
    ));
    assert!(tree_contains(
        &fixture,
        oid,
        &format!("{task_id}/.gitignore")
    ));
    assert!(tree_contains(&fixture, first_oid, &path));
    assert_eq!(std::fs::read(&retained).unwrap(), local_bytes);
    assert_eq!(std::fs::read(&index_path).unwrap(), index_before);

    std::fs::write(&retained, vec![7_u8; 10_485_761]).unwrap();
    assert_eq!(json(&workspace(&task, ["plan"]))["status"], "no_changes");
    assert_eq!(
        json(&workspace(
            &task,
            ["publish", "-m", "Repeat local-only publication"]
        ))["status"],
        "no_changes"
    );
    assert_eq!(std::fs::read(&index_path).unwrap(), index_before);

    workspace(
        &task,
        [
            "storage",
            "set",
            &path,
            "--to",
            "git",
            "--reason",
            "Share the retained file again.",
        ],
    );
    assert_eq!(
        std::fs::read(task.join(".gitignore")).unwrap(),
        ignore_before
    );
    let restored = json(&workspace(&task, ["publish", "-m", "Restore Git tracking"]));
    assert!(tree_contains(
        &fixture,
        restored["commit_oid"].as_str().unwrap(),
        &path
    ));
    assert_eq!(std::fs::read(&index_path).unwrap(), index_before);
}

#[test]
fn untrack_complete_git_directory_excludes_existing_and_future_descendants() {
    let fixture = managed_fixture(false);
    let (task_id, task) = create_task(&fixture, "local-directory");
    let path = format!("{task_id}/private data");
    let directory = task.join("private data");
    std::fs::create_dir_all(directory.join("nested")).unwrap();
    std::fs::write(directory.join("nested/record.txt"), "retained record\n").unwrap();
    std::fs::write(task.join("private data.txt"), "shared sibling\n").unwrap();
    workspace(
        &task,
        [
            "storage",
            "set",
            &path,
            "--to",
            "git",
            "--reason",
            "Treat the directory as one complete storage boundary.",
        ],
    );
    workspace(&task, ["publish", "-m", "Publish directory"]);
    workspace(&task, ["untrack", &path]);
    let first = json(&workspace(&task, ["publish", "-m", "Keep directory local"]));
    let oid = first["commit_oid"].as_str().unwrap();
    assert!(!tree_contains(&fixture, oid, &path));
    assert!(tree_contains(
        &fixture,
        oid,
        &format!("{task_id}/private data.txt")
    ));
    assert_eq!(
        std::fs::read(directory.join("nested/record.txt")).unwrap(),
        b"retained record\n"
    );
    std::fs::write(directory.join("future.bin"), vec![8_u8; 10_485_761]).unwrap();
    std::fs::write(
        directory.join("future.dvc"),
        "local payload, not storage metadata\n",
    )
    .unwrap();
    let status = json(&workspace(
        &task,
        ["storage", "status", &format!("{path}/future.bin")],
    ));
    assert_eq!(status["placements"][0]["target"], "local");
    assert_eq!(status["placements"][0]["boundary"], path);
    assert_eq!(json(&workspace(&task, ["plan"]))["status"], "no_changes");
    assert!(!directory.join("future.bin.dvc").exists());
}

#[cfg(feature = "test-storage")]
#[test]
fn untrack_s3_file_and_complete_directory_preserves_bytes_without_repeat_uploads() {
    if which::which("dvc").is_err() {
        eprintln!("skipping: dvc is unavailable");
        return;
    }
    let fixture = managed_fixture(true);
    let (task_id, task) = create_task(&fixture, "local-s3");
    let path = format!("{task_id}/data.bin");
    let bundle_path = format!("{task_id}/bundle");
    let payload = b"retained S3 payload\n";
    std::fs::write(task.join("data.bin"), payload).unwrap();
    std::fs::create_dir(task.join("bundle")).unwrap();
    std::fs::write(
        task.join("bundle/part.bin"),
        b"retained directory payload\n",
    )
    .unwrap();
    workspace(
        &task,
        [
            "storage",
            "set",
            &path,
            &bundle_path,
            "--to",
            "s3",
            "--reason",
            "Publish S3 fixtures.",
        ],
    );
    workspace(&task, ["publish", "-m", "Publish stored content"]);
    let published_pointer = std::fs::read(task.join("data.bin.dvc")).unwrap();
    let index_path = fixture.shared.join(".git/index");
    let index_before = std::fs::read(&index_path).unwrap();
    let remote = fixture.root.join("storage-remote");
    let remote_before = remote_snapshot(&remote);
    assert!(!remote_before.is_empty());

    let rejected = workspace_unchecked(
        &task,
        ["untrack", &path, &format!("{bundle_path}/part.bin")],
    );
    assert_eq!(rejected.status.code(), Some(2));
    assert!(task.join("data.bin.dvc").is_file());
    assert!(task.join("bundle.dvc").is_file());
    assert_eq!(
        json(&workspace(&task, ["storage", "status", &path]))["placements"][0]["target"],
        "s3"
    );
    assert_eq!(remote_snapshot(&remote), remote_before);

    let dry = json(&workspace(
        &task,
        ["untrack", &path, &bundle_path, "--dry-run"],
    ));
    assert_eq!(dry["status"], "dry_run");
    assert!(task.join("data.bin.dvc").is_file());
    assert!(task.join("bundle.dvc").is_file());
    workspace(&task, ["untrack", &path, &bundle_path]);
    assert!(!task.join("data.bin.dvc").exists());
    assert!(!task.join("bundle.dvc").exists());
    assert_eq!(std::fs::read(task.join("data.bin")).unwrap(), payload);
    assert_eq!(
        std::fs::read(task.join("bundle/part.bin")).unwrap(),
        b"retained directory payload\n"
    );
    assert_eq!(std::fs::read(&index_path).unwrap(), index_before);
    assert_eq!(remote_snapshot(&remote), remote_before);

    let published = json(&workspace(
        &task,
        ["publish", "-m", "Keep stored content local"],
    ));
    let oid = published["commit_oid"].as_str().unwrap();
    for output in [&path, &bundle_path] {
        assert!(!tree_contains(&fixture, oid, output));
        assert!(!tree_contains(&fixture, oid, &format!("{output}.dvc")));
        assert!(tree_contains(
            &fixture,
            oid,
            &format!("{output}.workspace-mgr-storage.toml")
        ));
    }
    std::fs::write(task.join("data.bin"), b"new local-only content\n").unwrap();
    std::fs::write(
        task.join("bundle/new.bin"),
        b"new local-only directory member\n",
    )
    .unwrap();
    let plan = json(&workspace(&task, ["plan"]));
    assert_eq!(plan["status"], "no_changes");
    assert!(
        plan["storage"]["purge"]["queued"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        json(&workspace(
            &task,
            ["publish", "-m", "Repeat without upload"]
        ))["status"],
        "no_changes"
    );
    assert_eq!(remote_snapshot(&remote), remote_before);
    assert_eq!(std::fs::read(&index_path).unwrap(), index_before);

    // A restored old pointer must not publish newer local-only bytes under
    // conflicting metadata. Both previews and publication refuse it first.
    let retained_ignores = std::fs::read(task.join(".gitignore")).unwrap();
    std::fs::write(task.join("data.bin.dvc"), &published_pointer).unwrap();
    for args in [
        vec!["plan"],
        vec!["publish", "-m", "Reject a restored stale S3 pointer"],
    ] {
        let rejected = workspace_unchecked(&task, args);
        assert_eq!(rejected.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("conflicts with local-only"));
        assert_eq!(remote_snapshot(&remote), remote_before);
        assert_eq!(
            std::fs::read(task.join("data.bin")).unwrap(),
            b"new local-only content\n"
        );
        assert_eq!(
            std::fs::read(task.join("data.bin.dvc")).unwrap(),
            published_pointer
        );
        assert_eq!(std::fs::read(&index_path).unwrap(), index_before);
    }
    workspace(&task, ["untrack", &path]);
    assert!(!task.join("data.bin.dvc").exists());
    assert_eq!(
        std::fs::read(task.join(".gitignore")).unwrap(),
        retained_ignores
    );
    assert_eq!(
        std::fs::read(task.join("data.bin")).unwrap(),
        b"new local-only content\n"
    );
    assert_eq!(remote_snapshot(&remote), remote_before);
    assert_eq!(json(&workspace(&task, ["plan"]))["status"], "no_changes");
    assert_eq!(
        json(&workspace(
            &task,
            ["publish", "-m", "Keep the stale pointer removed"]
        ))["status"],
        "no_changes"
    );

    // The alternative recovery API must rebuild both metadata and the DVC
    // ignore rule from the current local payload before publication.
    std::fs::write(task.join("data.bin.dvc"), &published_pointer).unwrap();
    let retracked = json(&workspace(
        &task,
        [
            "storage",
            "set",
            &path,
            "--to",
            "s3",
            "--reason",
            "Share the local payload again.",
        ],
    ));
    assert_eq!(retracked["remote_writes"], false);
    assert_eq!(remote_snapshot(&remote), remote_before);
    assert_ne!(
        std::fs::read(task.join("data.bin.dvc")).unwrap(),
        published_pointer
    );
    git(&fixture.shared, ["check-ignore", "--no-index", "--", &path]);
    let restored = json(&workspace(&task, ["publish", "-m", "Restore S3 tracking"]));
    let restored_oid = restored["commit_oid"].as_str().unwrap();
    assert!(tree_contains(
        &fixture,
        restored_oid,
        &format!("{path}.dvc")
    ));
    assert!(!tree_contains(&fixture, restored_oid, &path));
    assert!(!tree_contains(
        &fixture,
        restored_oid,
        &format!("{bundle_path}.dvc")
    ));
    assert_eq!(
        std::fs::read(task.join("data.bin")).unwrap(),
        b"new local-only content\n"
    );
    assert_ne!(remote_snapshot(&remote), remote_before);
}
