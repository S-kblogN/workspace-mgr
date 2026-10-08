mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::ops::Deref;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use common::*;

struct LegacyFixture {
    git: GitFixture,
    s3: ExactVersionFixture,
}

impl Deref for LegacyFixture {
    type Target = GitFixture;

    fn deref(&self) -> &Self::Target {
        &self.git
    }
}

/// Read-only, immutable versions with deliberately absent SHA256 headers. The
/// fixture makes legacy migration establish its proof through an actual exact
/// GET rather than trusting a made-up checksum or a developer's S3 account.
struct ExactVersionFixture {
    endpoint: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl ExactVersionFixture {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let serving_requests = requests.clone();
        let serving_stop = stop.clone();
        let worker = thread::spawn(move || {
            while !serving_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(10)))
                            .unwrap();
                        let mut raw = Vec::new();
                        let mut byte = [0];
                        while !raw.ends_with(b"\r\n\r\n") {
                            if stream.read(&mut byte).unwrap_or(0) == 0 {
                                break;
                            }
                            raw.push(byte[0]);
                            assert!(raw.len() < 64 * 1024);
                        }
                        let raw = String::from_utf8(raw).unwrap();
                        let line = raw.lines().next().unwrap_or_default();
                        serving_requests.lock().unwrap().push(line.to_owned());
                        let mut fields = line.split_whitespace();
                        let method = fields.next().unwrap_or_default();
                        let target = fields.next().unwrap_or_default();
                        let url = url::Url::parse(&format!("http://local{target}")).unwrap();
                        let wanted = match url.path() {
                            "/offline.invalid/repository/data.bin"
                            | "/offline.invalid/repository/2025/archive/task/cold.bin" => {
                                Some(("exact-version", "exact-etag"))
                            }
                            "/offline.invalid/repository/absent-directory/nested/sample.bin" => {
                                Some(("exact-directory-file-version", "exact-directory-file-etag"))
                            }
                            _ => None,
                        };
                        let version = url
                            .query_pairs()
                            .find(|(key, _)| key == "versionId")
                            .map(|(_, value)| value.into_owned());
                        if method == "GET" && url.query_pairs().any(|(key, _)| key == "versioning")
                        {
                            let body = b"<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>";
                            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).unwrap();
                            stream.write_all(body).unwrap();
                        } else if let Some((id, etag)) = wanted
                            && version.as_deref() == Some(id)
                            && matches!(method, "HEAD" | "GET")
                        {
                            let header = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nETag: \"{etag}\"\r\nx-amz-version-id: {id}\r\nConnection: close\r\n\r\n"
                            );
                            stream.write_all(header.as_bytes()).unwrap();
                            if method == "GET" {
                                stream.write_all(b"abc").unwrap();
                            }
                        } else {
                            let body = b"<Error><Code>NoSuchVersion</Code></Error>";
                            stream.write_all(format!("HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).unwrap();
                            if method != "HEAD" {
                                stream.write_all(body).unwrap();
                            }
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("legacy S3 fixture failed: {error}"),
                }
            }
        });
        Self {
            endpoint,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn payload_gets(&self) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.starts_with("GET ") && line.contains("versionId="))
            .count()
    }
}

impl Drop for ExactVersionFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
        if thread::panicking() {
            eprintln!("legacy S3 requests: {:?}", self.requests.lock().unwrap());
        }
    }
}

fn legacy_repository() -> LegacyFixture {
    let fixture = GitFixture::new();
    let s3 = ExactVersionFixture::new();
    fixture.clone_shared();
    fs::create_dir_all(fixture.shared.join(".dvc/cache/files/md5/90")).unwrap();
    fs::write(fixture.shared.join(".dvc/config"), format!("[core]\nremote = workspace-mgr\n['remote \"workspace-mgr\"']\nurl = s3://offline.invalid/repository\nendpointurl = {}\nversion_aware = true\n", s3.endpoint)).unwrap();
    fs::write(fixture.shared.join(".dvc/config.local"), "['remote \"workspace-mgr\"']\naccess_key_id = isolated-access\nsecret_access_key = isolated-secret\nregion = us-east-1\n").unwrap();
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
    LegacyFixture { git: fixture, s3 }
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
    assert_eq!(manifest["schema_version"], 2);
    assert_eq!(
        manifest["checksum"]["digest"],
        "900150983cd24fb0d6963f7d28e17f72"
    );
    assert_eq!(manifest["version"]["id"], "exact-version");
    assert_eq!(manifest["version"]["etag"], "exact-etag");
    assert_eq!(
        manifest["version"]["verification"]["version_id"],
        "exact-version"
    );
    assert_eq!(
        manifest["version"]["verification"]["endpoint"],
        fixture.s3.endpoint
    );
    assert_eq!(
        manifest["version"]["verification"]["bucket"],
        "offline.invalid"
    );
    assert_eq!(
        manifest["version"]["verification"]["key"],
        "repository/data.bin"
    );
    assert_eq!(
        manifest["version"]["verification"]["checksum"]["algorithm"],
        "sha256"
    );
    assert_eq!(
        manifest["version"]["verification"]["checksum"]["digest"],
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(fixture.s3.payload_gets(), 2);
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
            .contains("minimum_cli_version = \"0.8.7\"")
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
    assert_eq!(
        fixture.s3.payload_gets(),
        2,
        "schema 2 reconciliation reread payloads"
    );
}

#[test]
fn dry_run_and_failed_preflight_leave_all_files_intact() {
    let fixture = legacy_repository();
    let before = fs::read(fixture.shared.join("data.bin.dvc")).unwrap();
    let private_exclude = fixture.shared.join(".git/info/exclude");
    let exclude_before = fs::read(&private_exclude).unwrap();
    let report = json(&workspace(&fixture.shared, ["manage", "--dry-run"]));
    assert_eq!(fixture.s3.payload_gets(), 0, "dry run downloaded a payload");
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
fn native_schema1_upgrade_is_previewed_without_payload_reads_and_verified_once() {
    let fixture = legacy_repository();
    workspace(&fixture.shared, ["manage"]);
    let path = fixture.shared.join("data.bin.wm-storage.json");
    let mut legacy: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    legacy["schema_version"] = 1.into();
    legacy["version"]
        .as_object_mut()
        .unwrap()
        .remove("verification");
    let before = serde_json::to_vec_pretty(&legacy).unwrap();
    fs::write(&path, &before).unwrap();
    fs::remove_file(fixture.shared.join("data.bin")).unwrap();
    let reads_before = fixture.s3.payload_gets();

    let preview = json(&workspace(&fixture.shared, ["manage", "--dry-run"]));
    assert_eq!(preview["status"], "dry_run");
    assert_eq!(
        preview["migration"]["upgraded"],
        serde_json::json!(["data.bin.wm-storage.json"])
    );
    assert_eq!(fixture.s3.payload_gets(), reads_before);
    assert_eq!(fs::read(&path).unwrap(), before);

    let upgraded = json(&workspace(&fixture.shared, ["manage"]));
    assert_eq!(
        upgraded["migration"]["upgraded"],
        serde_json::json!(["data.bin.wm-storage.json"])
    );
    let native: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(native["schema_version"], 2);
    assert_eq!(native["checksum"], legacy["checksum"]);
    assert_eq!(native["version"]["id"], legacy["version"]["id"]);
    assert_eq!(
        native["version"]["verification"]["version_id"],
        native["version"]["id"]
    );
    assert_eq!(native["version"]["verification"]["method"], "verified-read");
    assert_eq!(fixture.s3.payload_gets(), reads_before + 1);
    assert!(!fixture.shared.join("data.bin").exists());
    assert_eq!(
        json(&workspace(&fixture.shared, ["manage"]))["status"],
        "no_changes"
    );
    assert_eq!(fixture.s3.payload_gets(), reads_before + 1);
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
    for path in [".dvc/config", ".dvc/config.local", "data.bin.dvc"] {
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
        // Published history needs the missing versions; only a pointer that no
        // revision recorded may become a pending native placement.
        git(&fixture.shared, ["add", "--", "data.bin.dvc"]);
        git(&fixture.shared, ["commit", "-q", "-m", "published pointer"]);
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
fn directory_manifest_migration_verifies_remote_bytes_without_materializing_payloads() {
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
    assert_eq!(native["schema_version"], 2);
    assert_eq!(
        native["entries"][0]["version"]["verification"]["method"],
        "verified-read"
    );
    assert_eq!(fixture.s3.payload_gets(), 2);
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
fn manage_keeps_an_existing_native_cache_and_retains_the_legacy_cache_below_it() {
    let fixture = legacy_repository();
    // Native commands such as refresh populate the cache before migration.
    let native = fixture
        .shared
        .join(".workspace-mgr/local/cache/objects/md5/aa/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    fs::create_dir_all(native.parent().unwrap()).unwrap();
    fs::write(&native, b"native").unwrap();
    assert_eq!(
        json(&workspace(&fixture.shared, ["manage", "--dry-run"]))["status"],
        "dry_run"
    );
    assert!(fixture.shared.join(".dvc/cache").is_dir());
    let report = json(&workspace(&fixture.shared, ["manage"]));
    assert_eq!(report["status"], "managed");
    assert_eq!(fs::read(&native).unwrap(), b"native");
    assert_eq!(
        fs::read(
            fixture.shared.join(
                ".workspace-mgr/local/cache/legacy/files/md5/90/0150983cd24fb0d6963f7d28e17f72"
            )
        )
        .unwrap(),
        b"abc"
    );
    assert!(!fixture.shared.join(".dvc").exists());
    assert_eq!(fs::read(fixture.shared.join("data.bin")).unwrap(), b"abc");
}

#[test]
fn unpublished_unbound_pointer_with_matching_payload_converts_pending_upload() {
    use md5::{Digest, Md5};
    let fixture = legacy_repository();
    // A legacy S3 placement recorded locally but never published: an aggregate
    // digest whose listing lives only in the local cache, and no version.
    let rows = "[{\"md5\": \"900150983cd24fb0d6963f7d28e17f72\", \"relpath\": \"a.bin\"}]";
    let digest = Md5::digest(rows.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let listing = fixture.shared.join(format!(
        ".dvc/cache/files/md5/{}/{}.dir",
        &digest[..2],
        &digest[2..]
    ));
    fs::create_dir_all(listing.parent().unwrap()).unwrap();
    fs::write(&listing, rows).unwrap();
    fs::create_dir_all(fixture.shared.join("task/raw")).unwrap();
    fs::write(fixture.shared.join("task/raw/a.bin"), b"abc").unwrap();
    fs::write(fixture.shared.join("task/.gitignore"), "/raw\n").unwrap();
    let raw =
        format!("outs:\n- md5: {digest}.dir\n  size: 3\n  nfiles: 1\n  hash: md5\n  path: raw\n");
    fs::write(fixture.shared.join("task/raw.dvc"), &raw).unwrap();

    // A payload that differs from the record still refuses, leaving files intact.
    fs::write(fixture.shared.join("task/raw/extra.bin"), b"x").unwrap();
    let refused = workspace_unchecked(&fixture.shared, ["manage"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("lacks exact S3 object versions"));
    assert_eq!(
        fs::read_to_string(fixture.shared.join("task/raw.dvc")).unwrap(),
        raw
    );
    fs::remove_file(fixture.shared.join("task/raw/extra.bin")).unwrap();

    let preview = json(&workspace(&fixture.shared, ["manage", "--dry-run"]));
    assert_eq!(preview["migration"]["pending_upload"][0], "task/raw.dvc");
    let report = json(&workspace(&fixture.shared, ["manage"]));
    assert_eq!(report["migration"]["pending_upload"][0], "task/raw.dvc");
    let native: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.shared.join("task/raw.wm-storage.json")).unwrap())
            .unwrap();
    assert_eq!(native["kind"], "directory");
    assert_eq!(native["entries"][0]["path"], "a.bin");
    assert!(
        native["entries"][0]
            .get("version")
            .is_none_or(|v| v.is_null())
    );
    assert!(!fixture.shared.join("task/raw.dvc").exists());
    assert_eq!(
        fs::read(fixture.shared.join("task/raw/a.bin")).unwrap(),
        b"abc"
    );
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
