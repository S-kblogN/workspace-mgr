mod common;

use std::fs;
use std::path::Path;

use common::*;

fn legacy_repository() -> GitFixture {
    let fixture = GitFixture::new();
    fixture.clone_shared();
    fs::create_dir_all(fixture.shared.join(".dvc/cache/files/md5/90")).unwrap();
    fs::write(fixture.shared.join(".dvc/config"), "[core]\nremote = workspace-mgr\n['remote \"workspace-mgr\"']\nurl = s3://offline.invalid/repository\nversion_aware = true\n").unwrap();
    fs::write(
        fixture.shared.join(".dvc/.gitignore"),
        "/config.local\n/cache\n/tmp\n",
    )
    .unwrap();
    fs::write(fixture.shared.join(".dvcignore"), "# legacy default\n").unwrap();
    fs::write(
        fixture.shared.join(".gitignore"),
        "/data.bin\nuser-private/\n",
    )
    .unwrap();
    fs::write(
        fixture.shared.join(".gitattributes"),
        "*.dvc whitespace=-blank-at-eol\n*.txt text\n",
    )
    .unwrap();
    fs::write(fixture.shared.join("data.bin"), b"abc").unwrap();
    fs::write(
        fixture
            .shared
            .join(".dvc/cache/files/md5/90/0150983cd24fb0d6963f7d28e17f72"),
        b"abc",
    )
    .unwrap();
    pointer(&fixture.shared, "data.bin.dvc", "data.bin");
    fixture
}

fn pointer(root: &Path, path: &str, payload: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, format!("outs:\n- path: {payload}\n  hash: md5\n  md5: 900150983cd24fb0d6963f7d28e17f72\n  size: 3\n  cloud:\n    workspace-mgr:\n      version_id: exact-version\n      etag: exact-etag\n")).unwrap();
}

#[test]
fn manage_converts_entire_checkout_without_payload_or_history_changes() {
    let fixture = legacy_repository();
    pointer(
        &fixture.shared,
        "2025/archive/task/cold.bin.dvc",
        "cold.bin",
    );
    fs::create_dir_all(fixture.shared.join("nested")).unwrap();
    command(&fixture.shared, "git", ["init", "nested"]);
    pointer(&fixture.shared, "nested/untouched.bin.dvc", "untouched.bin");
    fs::write(fixture.shared.join(".dvc/config.local"), "['remote \"workspace-mgr\"']\naccess_key_id = fake-access\nsecret_access_key = fake-secret\nregion = us-test-1\n").unwrap();
    let private_exclude = fixture.shared.join(".git/info/exclude");
    fs::write(&private_exclude, "# retained user ignore\n/user-private").unwrap();
    let head = git(&fixture.shared, ["rev-parse", "HEAD"]).stdout;
    let index = git(&fixture.shared, ["ls-files", "-s"]).stdout;
    let report = json(&workspace(&fixture.shared, ["manage"]));
    assert_eq!(report["status"], "managed");
    assert_eq!(
        report["migration"]["converted"].as_array().unwrap().len(),
        2
    );
    assert_eq!(
        report["migration"]["excluded_nested_repositories"][0],
        "nested"
    );
    assert_eq!(fs::read(fixture.shared.join("data.bin")).unwrap(), b"abc");
    assert!(fixture.shared.join("nested/untouched.bin.dvc").exists());
    assert!(!fixture.shared.join("data.bin.dvc").exists());
    assert!(!fixture.shared.join(".dvc").exists());
    assert!(!fixture.shared.join(".dvcignore").exists());
    assert_eq!(
        fs::read_to_string(fixture.shared.join(".gitattributes")).unwrap(),
        "*.txt text\n"
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.shared.join("data.bin.wm-storage.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["schema_version"], 1);
    assert_eq!(
        manifest["checksum"]["digest"],
        "900150983cd24fb0d6963f7d28e17f72"
    );
    assert_eq!(manifest["version"]["id"], "exact-version");
    assert_eq!(manifest["version"]["etag"], "exact-etag");
    assert_eq!(
        fs::read(
            fixture
                .shared
                .join(".workspace-mgr/local/cache/files/md5/90/0150983cd24fb0d6963f7d28e17f72")
        )
        .unwrap(),
        b"abc"
    );
    assert!(
        fs::read_to_string(fixture.shared.join(".workspace-mgr/local/credentials.toml"))
            .unwrap()
            .contains("fake-secret")
    );
    assert!(
        !String::from_utf8(workspace(&fixture.shared, ["manage", "--dry-run"]).stdout)
            .unwrap()
            .contains("fake-secret")
    );
    assert!(
        fs::read_to_string(fixture.shared.join(".workspace-mgr.toml"))
            .unwrap()
            .contains("minimum_cli_version = \"0.8.1\"")
    );
    assert_eq!(git(&fixture.shared, ["rev-parse", "HEAD"]).stdout, head);
    assert_eq!(git(&fixture.shared, ["ls-files", "-s"]).stdout, index);
    assert_eq!(
        fs::read(&private_exclude).unwrap(),
        b"# retained user ignore\n/user-private\n/.workspace-mgr/local/\n"
    );
    assert!(
        git(
            &fixture.shared,
            [
                "check-ignore",
                "--no-index",
                ".workspace-mgr/local/credentials.toml"
            ]
        )
        .status
        .success()
    );
    assert_eq!(
        json(&workspace(&fixture.shared, ["manage"]))["status"],
        "no_changes"
    );
}

#[test]
fn dry_run_and_failed_preflight_leave_all_files_intact() {
    let fixture = legacy_repository();
    let before = fs::read(fixture.shared.join("data.bin.dvc")).unwrap();
    let private_exclude = fixture.shared.join(".git/info/exclude");
    let exclude_before = fs::read(&private_exclude).unwrap();
    let report = json(&workspace(&fixture.shared, ["manage", "--dry-run"]));
    assert_eq!(report["status"], "dry_run");
    assert_eq!(
        report["migration"]["converted"][0]["destination"],
        "data.bin.wm-storage.json"
    );
    assert_eq!(
        fs::read(fixture.shared.join("data.bin.dvc")).unwrap(),
        before
    );
    assert!(!fixture.shared.join(".workspace-mgr.toml").exists());
    assert!(!fixture.shared.join(".workspace-mgr").exists());
    assert_eq!(fs::read(&private_exclude).unwrap(), exclude_before);
    fs::write(
        fixture.shared.join("late.dvc"),
        "outs:\n- path: late\n  md5: invalid\n  size: 3\n",
    )
    .unwrap();
    let failed = workspace_unchecked(&fixture.shared, ["manage"]);
    assert!(!failed.status.success());
    assert_eq!(
        fs::read(fixture.shared.join("data.bin.dvc")).unwrap(),
        before
    );
    assert!(!fixture.shared.join("data.bin.wm-storage.json").exists());
    assert!(!fixture.shared.join("AGENTS.md").exists());
}

#[test]
fn custom_controls_pipelines_and_collisions_refuse_before_changes() {
    for (path, content) in [
        ("dvc.yaml", "stages: {}\n"),
        (".dvc/unknown", "custom"),
        (".dvcignore", "exclude-important-data\n"),
        ("data.bin.wm-storage.json", "collision"),
        (
            ".dvc/config.local",
            "['remote \"workspace-mgr\"']\ncredentialpath = /private/credentials\n",
        ),
    ] {
        let fixture = legacy_repository();
        fs::write(fixture.shared.join(path), content).unwrap();
        let output = workspace_unchecked(&fixture.shared, ["manage"]);
        assert!(!output.status.success(), "unsupported {path} was accepted");
        assert!(fixture.shared.join("data.bin.dvc").exists());
        assert!(fixture.shared.join(".dvc/config").exists());
        assert!(!fixture.shared.join(".workspace-mgr.toml").exists());
        assert!(!fixture.shared.join("AGENTS.md").exists());
    }
}

#[test]
fn scaffold_failure_is_preflighted_before_pointer_conversion() {
    let fixture = legacy_repository();
    fs::write(fixture.shared.join("AGENTS.md"), "user policy\n").unwrap();
    let output = workspace_unchecked(&fixture.shared, ["manage"]);
    assert!(!output.status.success());
    assert!(fixture.shared.join("data.bin.dvc").exists());
    assert!(!fixture.shared.join("data.bin.wm-storage.json").exists());
    assert_eq!(
        fs::read_to_string(fixture.shared.join("AGENTS.md")).unwrap(),
        "user policy\n"
    );
}

#[test]
fn named_version_aware_remote_imports_exact_bindings() {
    let fixture = legacy_repository();
    for path in [".dvc/config", "data.bin.dvc"] {
        let original = fs::read_to_string(fixture.shared.join(path)).unwrap();
        fs::write(
            fixture.shared.join(path),
            original.replace("workspace-mgr", "research-data"),
        )
        .unwrap();
    }
    workspace(&fixture.shared, ["manage"]);
    let native: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.shared.join("data.bin.wm-storage.json")).unwrap())
            .unwrap();
    assert_eq!(native["version"]["id"], "exact-version");
    assert_eq!(native["version"]["etag"], "exact-etag");
}

#[test]
fn content_addressed_s3_or_missing_versions_refuse_before_migration() {
    for mode in ["content-addressed", "missing-version"] {
        let fixture = legacy_repository();
        let path = if mode == "content-addressed" {
            ".dvc/config"
        } else {
            "data.bin.dvc"
        };
        let original = fs::read_to_string(fixture.shared.join(path)).unwrap();
        let changed = if mode == "content-addressed" {
            original.replace("version_aware = true", "version_aware = false")
        } else {
            original.split("  cloud:").next().unwrap().to_owned()
        };
        fs::write(fixture.shared.join(path), changed).unwrap();
        let output = workspace_unchecked(&fixture.shared, ["manage"]);
        assert!(
            !output.status.success(),
            "unsupported S3 bindings were accepted: {mode}"
        );
        assert!(fixture.shared.join("data.bin.dvc").exists());
        assert!(!fixture.shared.join("data.bin.wm-storage.json").exists());
        assert!(fixture.shared.join(".dvc/config").exists());
    }
}

#[test]
fn unfinished_upload_refuses_pointer_rename() {
    let fixture = legacy_repository();
    fs::create_dir_all(fixture.shared.join(".dvc/tmp/native-uploads")).unwrap();
    fs::write(
        fixture.shared.join(".dvc/tmp/native-uploads/active.json"),
        "{\"phase\":\"uploading\"}",
    )
    .unwrap();
    let output = workspace_unchecked(&fixture.shared, ["manage"]);
    assert!(!output.status.success());
    assert!(fixture.shared.join("data.bin.dvc").exists());
    assert!(!fixture.shared.join("data.bin.wm-storage.json").exists());
}

#[test]
fn directory_manifest_migrates_without_fetching_payloads() {
    use md5::{Digest, Md5};
    let fixture = legacy_repository();
    let rows =
        "[{\"md5\": \"900150983cd24fb0d6963f7d28e17f72\", \"relpath\": \"nested/sample.bin\"}]";
    let directory_digest = Md5::digest(rows.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    fs::write(fixture.shared.join("absent-directory.dvc"), format!("outs:\n- path: absent-directory\n  hash: md5\n  md5: {directory_digest}.dir\n  size: 3\n  nfiles: 1\n  files:\n  - relpath: nested/sample.bin\n    md5: 900150983cd24fb0d6963f7d28e17f72\n    size: 3\n    cloud:\n      workspace-mgr:\n        version_id: exact-directory-file-version\n        etag: exact-directory-file-etag\n")).unwrap();
    workspace(&fixture.shared, ["manage"]);
    let native: serde_json::Value = serde_json::from_slice(
        &fs::read(fixture.shared.join("absent-directory.wm-storage.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(native["kind"], "directory");
    assert_eq!(native["entries"][0]["path"], "nested/sample.bin");
    assert_eq!(
        native["entries"][0]["version"]["id"],
        "exact-directory-file-version"
    );
    assert!(!fixture.shared.join("absent-directory").exists());
}

#[test]
fn files_only_directory_manifest_migrates_without_its_omitted_aggregate() {
    let fixture = legacy_repository();
    // DVC 3 writes a cloud-versioned directory without `md5`, `size` or `nfiles`.
    let raw = "outs:\n- hash: md5\n  path: absent-directory\n  files:\n  - relpath: nested/sample.bin\n    md5: 900150983cd24fb0d6963f7d28e17f72\n    size: 3\n    cloud:\n      workspace-mgr:\n        etag: exact-directory-file-etag\n        version_id: exact-directory-file-version\n";
    fs::write(fixture.shared.join("absent-directory.dvc"), raw).unwrap();
    let preview = json(&workspace(&fixture.shared, ["manage", "--dry-run"]));
    assert_eq!(preview["status"], "dry_run");
    assert_eq!(
        fs::read_to_string(fixture.shared.join("absent-directory.dvc")).unwrap(),
        raw
    );
    workspace(&fixture.shared, ["manage"]);
    let native: serde_json::Value = serde_json::from_slice(
        &fs::read(fixture.shared.join("absent-directory.wm-storage.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(native["kind"], "directory");
    assert_eq!(native["size"], 3);
    assert_eq!(native["entries"][0]["path"], "nested/sample.bin");
    assert_eq!(
        native["entries"][0]["version"]["id"],
        "exact-directory-file-version"
    );
    assert!(!fixture.shared.join("absent-directory.dvc").exists());
    assert!(!fixture.shared.join("absent-directory").exists());
}

#[test]
fn legacy_pointer_cannot_overlap_an_existing_native_boundary() {
    let fixture = legacy_repository();
    pointer(&fixture.shared, "mixed/data.bin.dvc", "data.bin");
    fs::write(fixture.shared.join("mixed.wm-storage.json"), r#"{"schema_version":1,"path":"mixed","kind":"directory","checksum":{"algorithm":"md5","digest":"d751713988987e9331980363e24189ce"},"size":0,"entries":[]}"#).unwrap();
    let output = workspace_unchecked(&fixture.shared, ["manage"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("overlapping")
    );
    assert!(fixture.shared.join("data.bin.dvc").exists());
    assert!(fixture.shared.join("mixed/data.bin.dvc").exists());
    assert!(!fixture.shared.join("data.bin.wm-storage.json").exists());
}

#[test]
fn cache_and_configuration_only_migration_requires_primary_checkout() {
    let fixture = legacy_repository();
    let worktree = fixture.root.join("linked");
    git(
        &fixture.shared,
        ["worktree", "add", "--detach", worktree.to_str().unwrap()],
    );
    fs::create_dir_all(worktree.join(".dvc/cache")).unwrap();
    fs::copy(
        fixture.shared.join(".dvc/config"),
        worktree.join(".dvc/config"),
    )
    .unwrap();
    let output = workspace_unchecked(&worktree, ["manage"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("primary shared checkout")
    );
    assert!(worktree.join(".dvc/config").exists());
    assert!(!worktree.join(".workspace-mgr/local/cache").exists());
    assert!(!worktree.join(".workspace-mgr.toml").exists());
}

#[test]
fn interrupted_journal_recovers_before_fresh_adoption() {
    let fixture = legacy_repository();
    let before = fs::read(fixture.shared.join("data.bin.dvc")).unwrap();
    fs::remove_file(fixture.shared.join("data.bin.dvc")).unwrap();
    let journal = serde_json::json!({
        "schema_version": 1,
        "root": fixture.shared.canonicalize().unwrap(),
        "changes": [{"path": "data.bin.dvc", "before": before, "after": null}],
        "moves": []
    });
    fs::create_dir_all(fixture.shared.join(".workspace-mgr/local")).unwrap();
    fs::write(
        fixture
            .shared
            .join(".workspace-mgr/local/storage-migration.json"),
        serde_json::to_vec(&journal).unwrap(),
    )
    .unwrap();
    assert!(
        !workspace_unchecked(&fixture.shared, ["manage", "--dry-run"])
            .status
            .success()
    );
    let report = json(&workspace(&fixture.shared, ["manage"]));
    assert_eq!(report["migration"]["recovered_interrupted_operation"], true);
    assert!(fixture.shared.join("data.bin.wm-storage.json").exists());
    assert!(
        !fixture
            .shared
            .join(".workspace-mgr/local/storage-migration.json")
            .exists()
    );
}
