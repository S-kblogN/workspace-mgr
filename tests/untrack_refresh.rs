mod common;

use std::path::{Path, PathBuf};

use common::*;

fn managed_fixture() -> GitFixture {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["init"]);
    fixture.commit_seed("Initialize workspace");
    fixture.clone_shared();
    fixture
}

fn create_task(fixture: &GitFixture) -> (String, PathBuf) {
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "untrack-refresh",
            "--title",
            "Untrack refresh",
            "--purpose",
            "Keep local bytes after publication and refresh.",
            "--timestamp",
            "20260914-120000",
        ],
    );
    let id = "20260914-120000-untrack-refresh".to_owned();
    let path = fixture.shared.join(&id);
    document_task(&path);
    (id, path)
}

fn publish_to_main(fixture: &GitFixture, task: &Path) -> String {
    let published = workspace(task, ["publish", "-m", "Publish refresh fixture"]);
    let oid = json(&published)["commit_oid"].as_str().unwrap().to_owned();
    git(&fixture.remote, ["update-ref", "refs/heads/main", &oid]);
    oid
}

fn local_placement(repo: &Path, path: &str) {
    std::fs::write(
        repo.join(format!("{path}.workspace-mgr-storage.toml")),
        "schema_version = 1\ntarget = \"local\"\nreason = \"Keep this local copy\"\n",
    )
    .unwrap();
}

#[test]
fn published_git_untrack_survives_refresh_and_republication() {
    let fixture = managed_fixture();
    let (id, task) = create_task(&fixture);
    let path = format!("{id}/data.txt");
    let payload = b"retain this exact local payload\n";
    std::fs::write(task.join("data.txt"), payload).unwrap();
    publish_to_main(&fixture, &task);
    workspace(&fixture.shared, ["refresh"]);

    workspace(&task, ["untrack", &path]);
    let oid = publish_to_main(&fixture, &task);
    assert!(
        !git_unchecked(
            &fixture.remote,
            ["cat-file", "-e", &format!("{oid}:{path}")]
        )
        .status
        .success()
    );
    let dry_run = workspace(&fixture.shared, ["refresh", "--dry-run"]);
    assert_eq!(json(&dry_run)["status"], "dry_run");
    assert_eq!(std::fs::read(task.join("data.txt")).unwrap(), payload);
    workspace(&fixture.shared, ["refresh"]);
    assert_eq!(std::fs::read(task.join("data.txt")).unwrap(), payload);
    assert!(
        git(&fixture.shared, ["ls-files", "--", &path])
            .stdout
            .is_empty()
    );
    assert!(
        git(&fixture.shared, ["check-ignore", "--", &path])
            .status
            .success()
    );
    assert_eq!(json(&workspace(&task, ["plan"]))["status"], "no_changes");
    assert_eq!(
        json(&workspace(&task, ["publish", "-m", "Retry publication"]))["status"],
        "no_changes"
    );
    assert_eq!(std::fs::read(task.join("data.txt")).unwrap(), payload);
}

#[test]
fn incoming_local_choice_preserves_an_unchanged_git_file() {
    let fixture = managed_fixture();
    std::fs::write(fixture.seed.join("payload.bin"), b"original bytes").unwrap();
    fixture.commit_seed("Add payload");
    workspace(&fixture.shared, ["refresh"]);

    local_placement(&fixture.seed, "payload.bin");
    // The root ignore file is product-owned, so a repository rule is added
    // through its own module and `init` regenerates the root file from it.
    std::fs::create_dir_all(fixture.seed.join(".workspace-mgr")).unwrap();
    std::fs::write(
        fixture.seed.join(".workspace-mgr/repository.gitignore"),
        "/payload.bin\n",
    )
    .unwrap();
    workspace(&fixture.seed, ["init"]);
    git(&fixture.seed, ["rm", "--cached", "--", "payload.bin"]);
    fixture.commit_seed("Keep payload local");
    workspace(&fixture.shared, ["refresh"]);

    assert_eq!(
        std::fs::read(fixture.shared.join("payload.bin")).unwrap(),
        b"original bytes"
    );
    assert!(
        git(&fixture.shared, ["ls-files", "--", "payload.bin"])
            .stdout
            .is_empty()
    );
}

#[test]
fn pending_local_choices_preserve_payloads_across_upstream_type_changes() {
    let fixture = managed_fixture();
    std::fs::write(fixture.seed.join("cache.bin"), b"local cache bytes").unwrap();
    std::fs::create_dir(fixture.seed.join("bundle")).unwrap();
    std::fs::write(fixture.seed.join("bundle/data.txt"), b"local bundle bytes").unwrap();
    fixture.commit_seed("Add incoming type-change fixtures");
    workspace(&fixture.shared, ["refresh"]);
    local_placement(&fixture.shared, "cache.bin");
    local_placement(&fixture.shared, "bundle");

    std::fs::remove_file(fixture.seed.join("cache.bin")).unwrap();
    std::fs::create_dir(fixture.seed.join("cache.bin")).unwrap();
    std::fs::write(
        fixture.seed.join("cache.bin/remote.txt"),
        b"new remote bytes",
    )
    .unwrap();
    std::fs::remove_dir_all(fixture.seed.join("bundle")).unwrap();
    std::fs::write(fixture.seed.join("bundle"), b"remote replacement file").unwrap();
    fixture.commit_seed("Change payload types upstream");
    workspace(&fixture.shared, ["refresh"]);

    assert_eq!(
        std::fs::read(fixture.shared.join("cache.bin")).unwrap(),
        b"local cache bytes"
    );
    assert_eq!(
        std::fs::read(fixture.shared.join("bundle/data.txt")).unwrap(),
        b"local bundle bytes"
    );
}

#[cfg(feature = "test-storage")]
#[test]
fn s3_untrack_refresh_preserves_dirty_bytes_without_the_old_remote_or_cache() {
    if which::which("dvc").is_err() {
        eprintln!("skipping: dvc is unavailable");
        return;
    }
    let fixture = GitFixture::new();
    let storage_remote = fixture.root.join("storage-remote");
    workspace(
        &fixture.seed,
        ["init", "--s3-url", storage_remote.to_str().unwrap()],
    );
    fixture.commit_seed("Initialize isolated storage");
    fixture.clone_shared();
    let (id, task) = create_task(&fixture);
    let path = format!("{id}/data.txt");
    std::fs::write(task.join("data.txt"), b"old remote bytes").unwrap();
    std::fs::write(task.join(".gitattributes"), "trigger.txt filter=reject\n").unwrap();
    workspace(
        &task,
        [
            "storage",
            "set",
            &path,
            "--to",
            "s3",
            "--reason",
            "Exercise stored data",
        ],
    );
    publish_to_main(&fixture, &task);
    workspace(&fixture.shared, ["refresh"]);

    let payload = b"local edits are deliberately never published\n";
    let consumers = ["consumer", "rollback-consumer"].map(|name| fixture.root.join(name));
    for consumer in &consumers {
        command(
            &fixture.root,
            "git",
            [
                "clone",
                fixture.remote.to_str().unwrap(),
                consumer.to_str().unwrap(),
            ],
        );
        configure_git(consumer);
        std::fs::write(consumer.join(&id).join("data.txt"), payload).unwrap();
    }
    git(&consumers[1], ["config", "filter.reject.smudge", "false"]);
    git(&consumers[1], ["config", "filter.reject.required", "true"]);
    let before_failure = git(&consumers[1], ["rev-parse", "HEAD"]).stdout;
    std::fs::write(task.join("data.txt"), payload).unwrap();
    workspace(&task, ["untrack", &path]);
    std::fs::write(task.join("trigger.txt"), "force rollback in one consumer\n").unwrap();
    publish_to_main(&fixture, &task);
    std::fs::remove_dir_all(&storage_remote).unwrap();
    let cache = fixture.shared.join(".dvc/cache");
    if cache.exists() {
        std::fs::remove_dir_all(&cache).unwrap();
    }
    let refresh = workspace(&fixture.shared, ["refresh"]);
    assert_eq!(json(&refresh)["status"], "updated");
    assert_eq!(std::fs::read(task.join("data.txt")).unwrap(), payload);
    assert!(!task.join("data.txt.dvc").exists());
    assert_eq!(
        json(&workspace(&task, ["storage", "hydrate"]))["status"],
        "no_changes"
    );
    assert_eq!(json(&workspace(&task, ["plan"]))["status"], "no_changes");
    assert_eq!(std::fs::read(task.join("data.txt")).unwrap(), payload);

    // This consumer still has the old pointer and independently modified
    // payload. Neither the remote content nor a comparison cache remains.
    workspace(&consumers[0], ["refresh"]);
    assert_eq!(std::fs::read(consumers[0].join(&path)).unwrap(), payload);
    assert!(!consumers[0].join(format!("{path}.dvc")).exists());

    // A later checkout failure must roll back metadata without trying to
    // restore retired S3 bytes over the locally retained payload.
    let failed = workspace_unchecked(&consumers[1], ["refresh"]);
    assert!(!failed.status.success());
    let error = String::from_utf8_lossy(&failed.stderr);
    assert!(
        error.contains("refresh failed and was rolled back"),
        "{error}"
    );
    assert_eq!(
        git(&consumers[1], ["rev-parse", "HEAD"]).stdout,
        before_failure
    );
    assert_eq!(std::fs::read(consumers[1].join(&path)).unwrap(), payload);
    assert!(consumers[1].join(format!("{path}.dvc")).exists());
    assert!(!consumers[1].join(&id).join("trigger.txt").exists());
}

#[cfg(feature = "test-storage")]
#[test]
fn incoming_s3_updates_cannot_replace_a_pending_local_choice() {
    if which::which("dvc").is_err() {
        eprintln!("skipping: dvc is unavailable");
        return;
    }
    let fixture = GitFixture::new();
    let storage_remote = fixture.root.join("storage-remote");
    workspace(
        &fixture.seed,
        ["init", "--s3-url", storage_remote.to_str().unwrap()],
    );
    fixture.commit_seed("Initialize isolated storage");
    fixture.clone_shared();
    let (id, task) = create_task(&fixture);
    let path = format!("{id}/data.txt");
    std::fs::write(task.join("data.txt"), b"original remote bytes").unwrap();
    workspace(
        &task,
        [
            "storage",
            "set",
            &path,
            "--to",
            "s3",
            "--reason",
            "Exercise concurrent storage choices",
        ],
    );
    publish_to_main(&fixture, &task);
    workspace(&fixture.shared, ["refresh"]);

    let consumer = fixture.root.join("consumer");
    command(
        &fixture.root,
        "git",
        [
            "clone",
            fixture.remote.to_str().unwrap(),
            consumer.to_str().unwrap(),
        ],
    );
    configure_git(&consumer);
    let payload = b"unpublished local bytes";
    std::fs::write(consumer.join(&path), payload).unwrap();
    workspace(&consumer.join(&id), ["untrack", &path]);
    let old_head = git(&consumer, ["rev-parse", "HEAD"]).stdout;

    std::fs::write(task.join("data.txt"), b"updated upstream bytes").unwrap();
    publish_to_main(&fixture, &task);
    let rejected = workspace_unchecked(&consumer, ["refresh"]);
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("conflicts with a local-only placement")
    );
    assert_eq!(git(&consumer, ["rev-parse", "HEAD"]).stdout, old_head);
    assert_eq!(std::fs::read(consumer.join(&path)).unwrap(), payload);
    assert!(!consumer.join(format!("{path}.dvc")).exists());
}
