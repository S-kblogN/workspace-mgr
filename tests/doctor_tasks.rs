mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use common::{
    GitFixture, git, json, workspace, workspace_env, workspace_env_unchecked, workspace_unchecked,
};
use serde_json::{Value, json as value};

const MANIFEST: &str = ".workspace-mgr-task.toml";
const FIRST: &str = "20260712-120000-analysis";
const SECOND: &str = "20260812-120000-inputs";

fn managed_fixture() -> GitFixture {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["manage"]);
    fixture
}

fn write_task(repo: &Path, path: &str, id: &str, slug: &str) -> PathBuf {
    let directory = repo.join(path);
    std::fs::create_dir_all(&directory).unwrap();
    let original_slug = id.splitn(3, '-').nth(2).unwrap();
    std::fs::write(
        directory.join(MANIFEST),
        format!(
            "schema_version = 2\nkind = \"deliverable\"\nid = \"{id}\"\nslug = \"{slug}\"\npath = \"{path}\"\nbranch = \"codex/{original_slug}\"\ntitle = \"Doctor {slug}\"\npurpose = \"Inspect current task ownership\"\nadditional_scopes = []\n"
        ),
    )
    .unwrap();
    std::fs::write(directory.join("README.md"), "# Task\n").unwrap();
    directory
}

fn tasks(report: &Value) -> &[Value] {
    report["tasks"].as_array().unwrap()
}

fn task_check<'a>(report: &'a Value, name: &str) -> &'a Value {
    let check_name = format!("task-metadata:{name}");
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == check_name)
        .unwrap_or_else(|| panic!("missing {check_name} in {report}"))
}

/// A loopback-only bucket with no payload downloads: each listed object is an
/// intentional extra. This exercises CLI selection and historical prefixes
/// through the real S3 transport without any external bucket.
struct InventoryBucket {
    endpoint: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl InventoryBucket {
    fn new(paths: &[String]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let objects = paths.to_vec();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("accept inventory request: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let Ok(reader_stream) = stream.try_clone() else {
                    continue;
                };
                let mut reader = BufReader::new(reader_stream);
                let mut first_line = String::new();
                if !reader.read_line(&mut first_line).is_ok_and(|size| size > 0) {
                    // A client may preconnect or close a pooled connection.
                    // Neither is an HTTP request nor a server failure.
                    continue;
                }
                recorded.lock().unwrap().push(first_line.trim().to_owned());
                let mut complete_headers = false;
                loop {
                    let mut header = String::new();
                    if !reader.read_line(&mut header).is_ok_and(|size| size > 0) {
                        break;
                    }
                    if header == "\r\n" {
                        complete_headers = true;
                        break;
                    }
                }
                if !complete_headers {
                    continue;
                }
                let parts = first_line.split_whitespace().collect::<Vec<_>>();
                let target = url::Url::parse(&format!("http://localhost{}", parts[1])).unwrap();
                let query = target
                    .query_pairs()
                    .collect::<std::collections::BTreeMap<_, _>>();
                let (status, body) = if parts[0] == "GET" && query.contains_key("versioning") {
                    (
                        "200 OK",
                        "<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>"
                            .to_owned(),
                    )
                } else if parts[0] == "GET" && query.contains_key("versions") {
                    let prefix = query
                        .get("prefix")
                        .map(|value| value.as_ref())
                        .unwrap_or("");
                    let rows = objects
                        .iter()
                        .enumerate()
                        .filter(|(_, key)| key.starts_with(prefix))
                        .map(|(index, key)| {
                            format!(
                                "<Version><Key>{key}</Key><VersionId>extra-{index}</VersionId><IsLatest>true</IsLatest><ETag>\"abc\"</ETag><Size>3</Size></Version>"
                            )
                        })
                        .collect::<String>();
                    (
                        "200 OK",
                        format!(
                            "<ListVersionsResult><IsTruncated>false</IsTruncated>{rows}</ListVersionsResult>"
                        ),
                    )
                } else {
                    (
                        "500 Internal Server Error",
                        "<Error><Code>UnexpectedRequest</Code></Error>".to_owned(),
                    )
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
            }
        });
        Self {
            endpoint,
            requests,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for InventoryBucket {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

const MOCK_CREDENTIALS: &[(&str, &str)] = &[
    ("AWS_ACCESS_KEY_ID", "doctor-loopback-key"),
    ("AWS_SECRET_ACCESS_KEY", "doctor-loopback-secret"),
];

#[test]
fn doctor_selects_renamed_nested_archive_by_every_exact_current_selector() {
    let fixture = managed_fixture();
    let current_name = "20260712-120000-final-report";
    let current_path = format!("Completed work/research/2026/07/{current_name}");
    let directory = write_task(&fixture.seed, &current_path, FIRST, "final-report");
    write_task(&fixture.seed, SECOND, SECOND, "inputs");
    let absolute = directory.canonicalize().unwrap();
    for selector in [
        FIRST,
        current_name,
        "final-report",
        &current_path,
        absolute.to_str().unwrap(),
    ] {
        let report = json(&workspace(&fixture.seed, ["doctor", selector]));
        assert_eq!(report["status"], "ok", "selector {selector}: {report}");
        assert_eq!(tasks(&report).len(), 1, "selector {selector}: {report}");
        let task = &tasks(&report)[0];
        assert_eq!(task["id"], FIRST);
        assert_eq!(task["name"], current_name);
        assert_eq!(task["kind"], "deliverable");
        assert_eq!(task["path"], current_path);
        assert_eq!(task["scopes"], value!([current_path]));
        assert_eq!(
            task["manifest"],
            value!(directory.join(MANIFEST).canonicalize().unwrap())
        );
        assert!(task["diagnostic"].is_null());
        assert_eq!(task_check(&report, current_name)["status"], "ok");
    }
}

#[test]
fn doctor_without_selector_includes_active_archived_legacy_and_invalid_tasks() {
    let fixture = managed_fixture();
    write_task(&fixture.seed, FIRST, FIRST, "analysis");
    let nested = format!("archives/2026/08/{SECOND}");
    std::fs::create_dir_all(fixture.seed.join(&nested)).unwrap();
    std::fs::write(fixture.seed.join(&nested).join("report.bin"), b"legacy").unwrap();
    let bad_name = "20260912-120000-broken";
    let bad = fixture.seed.join(bad_name);
    std::fs::create_dir(&bad).unwrap();
    std::fs::write(bad.join(MANIFEST), "not [valid TOML\n").unwrap();

    let output = workspace_unchecked(&fixture.seed, ["doctor"]);
    assert_eq!(output.status.code(), Some(2));
    let report = json(&output);
    assert_eq!(report["status"], "error");
    assert_eq!(tasks(&report).len(), 3);
    let legacy = tasks(&report)
        .iter()
        .find(|task| task["id"] == SECOND)
        .unwrap();
    assert_eq!(legacy["path"], nested);
    assert_eq!(legacy["scopes"], value!([nested]));
    assert!(legacy["manifest"].is_null());
    assert_eq!(task_check(&report, FIRST)["status"], "ok");
    assert_eq!(task_check(&report, SECOND)["status"], "ok");
    assert_eq!(task_check(&report, bad_name)["status"], "error");
    let invalid = tasks(&report)
        .iter()
        .find(|task| task["name"] == bad_name)
        .unwrap();
    assert!(!invalid["diagnostic"].as_str().unwrap().is_empty());
}

#[test]
fn doctor_selected_task_is_isolated_from_other_invalid_task_metadata() {
    let fixture = managed_fixture();
    write_task(&fixture.seed, FIRST, FIRST, "analysis");
    let other = write_task(&fixture.seed, SECOND, SECOND, "inputs");
    std::fs::write(other.join(MANIFEST), "not [valid TOML\n").unwrap();
    let report = json(&workspace(&fixture.seed, ["doctor", FIRST]));
    assert_eq!(report["status"], "ok");
    assert_eq!(tasks(&report).len(), 1);
    assert_eq!(tasks(&report)[0]["id"], FIRST);
    assert!(report["checks"].as_array().unwrap().iter().all(|check| {
        !check["detail"].as_str().unwrap().contains(SECOND)
            && !check["name"].as_str().unwrap().contains(SECOND)
    }));

    let invalid = workspace_unchecked(&fixture.seed, ["doctor", SECOND]);
    assert_eq!(invalid.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&invalid.stderr).contains("invalid current metadata"),
        "{invalid:?}"
    );
}

#[test]
fn doctor_refuses_unknown_and_ambiguous_selectors_with_current_path_candidates() {
    let fixture = managed_fixture();
    let first = format!("archives/one/{FIRST}");
    let second = format!("archives/two/{FIRST}");
    write_task(&fixture.seed, &first, FIRST, "analysis");
    write_task(&fixture.seed, &second, FIRST, "analysis");

    let unknown = workspace_unchecked(&fixture.seed, ["doctor", "missing-task"]);
    assert_eq!(unknown.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&unknown.stderr).contains("no current task matches"),
        "{unknown:?}"
    );
    for selector in [FIRST, "analysis"] {
        let ambiguous = workspace_unchecked(&fixture.seed, ["doctor", selector]);
        assert_eq!(ambiguous.status.code(), Some(2));
        let text = String::from_utf8_lossy(&ambiguous.stderr);
        assert!(text.contains("ambiguous task selector"), "{text}");
        assert!(text.contains(&first), "{text}");
        assert!(text.contains(&second), "{text}");
    }
    let exact = json(&workspace(&fixture.seed, ["doctor", &second]));
    assert_eq!(tasks(&exact).len(), 1);
    assert_eq!(tasks(&exact)[0]["path"], second);
}

#[test]
fn doctor_can_select_another_repository_and_private_infrastructure_task_scopes() {
    let fixture = managed_fixture();
    let manifest = fixture.seed.join(
        ".workspace-mgr/local/infrastructure-tasks/infra-audit/.workspace-mgr-infrastructure.toml",
    );
    std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
    std::fs::write(
        &manifest,
        "schema_version = 2\nkind = \"infrastructure\"\nid = \"infra-audit\"\nslug = \"audit\"\nbranch = \"codex/infra-audit\"\ntitle = \"Audit\"\npurpose = \"Inspect an explicit repository scope\"\n\n[[additional_scopes]]\npath = \"README.md\"\nreason = \"The user requested this scope\"\n",
    )
    .unwrap();
    let report = json(&workspace(
        &fixture.root,
        [
            "doctor",
            "infra-audit",
            "--repo",
            fixture.seed.to_str().unwrap(),
        ],
    ));
    assert_eq!(tasks(&report).len(), 1);
    assert_eq!(tasks(&report)[0]["id"], "infra-audit");
    assert_eq!(tasks(&report)[0]["kind"], "infrastructure");
    assert!(tasks(&report)[0]["path"].is_null());
    assert_eq!(
        tasks(&report)[0]["manifest"],
        value!(manifest.canonicalize().unwrap())
    );
    assert_eq!(tasks(&report)[0]["scopes"], value!(["README.md"]));
}

#[test]
fn selected_doctor_finds_git_historical_prefix_after_rename_and_keeps_other_task_isolated() {
    let fixture = GitFixture::new();
    let old_path = format!("past/2026/07/{FIRST}");
    let current_name = "20260712-120000-final-report";
    let current_path = format!("Completed work/2026/07/{current_name}");
    let stale = format!("{old_path}/retained.bin");
    let unrelated = format!("{SECOND}/unrelated.bin");
    let bucket = InventoryBucket::new(&[
        format!("workspace/{stale}"),
        format!("workspace/{unrelated}"),
    ]);
    workspace_env(
        &fixture.seed,
        [
            "manage",
            "--s3-url",
            "s3://fixture/workspace",
            "--s3-endpoint-url",
            &bucket.endpoint,
        ],
        MOCK_CREDENTIALS,
    );
    let original = write_task(&fixture.seed, &old_path, FIRST, "analysis");
    git(&fixture.seed, ["add", "-A"]);
    git(
        &fixture.seed,
        ["commit", "-m", "Record original archived task location"],
    );
    std::fs::create_dir_all(fixture.seed.join(&current_path).parent().unwrap()).unwrap();
    std::fs::rename(&original, fixture.seed.join(&current_path)).unwrap();
    write_task(&fixture.seed, &current_path, FIRST, "final-report");
    write_task(&fixture.seed, SECOND, SECOND, "inputs");
    git(&fixture.seed, ["add", "-A"]);
    git(
        &fixture.seed,
        ["commit", "-m", "Rename task and change archive parents"],
    );

    let selected = workspace_env_unchecked(&fixture.seed, ["doctor", FIRST], MOCK_CREDENTIALS);
    assert_eq!(selected.status.code(), Some(2));
    let report = json(&selected);
    assert_eq!(tasks(&report).len(), 1);
    assert_eq!(tasks(&report)[0]["path"], current_path);
    assert_eq!(report["storage"]["remote_versions"], 1);
    let issues = report["storage"]["issues"].as_array().unwrap();
    assert_eq!(issues.len(), 1, "{report}");
    assert_eq!(issues[0]["code"], "unexpected-object");
    assert_eq!(issues[0]["path"], stale);

    let all = json(&workspace_env_unchecked(
        &fixture.seed,
        ["doctor"],
        MOCK_CREDENTIALS,
    ));
    assert_eq!(all["storage"]["remote_versions"], 2);
    assert!(
        all["storage"]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| { issue["path"] == unrelated && issue["code"] == "unexpected-object" })
    );
    assert!(
        bucket
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.starts_with("GET "))
    );
}

#[test]
fn exact_selected_path_does_not_claim_another_current_task_with_the_same_id() {
    let fixture = GitFixture::new();
    let nested = format!("archives/2026/{FIRST}");
    let bucket = InventoryBucket::new(&[format!("workspace/{FIRST}/other-task.bin")]);
    workspace_env(
        &fixture.seed,
        [
            "manage",
            "--s3-url",
            "s3://fixture/workspace",
            "--s3-endpoint-url",
            &bucket.endpoint,
        ],
        MOCK_CREDENTIALS,
    );
    write_task(&fixture.seed, FIRST, FIRST, "analysis");
    write_task(&fixture.seed, &nested, FIRST, "analysis");
    git(&fixture.seed, ["add", "-A"]);
    git(
        &fixture.seed,
        ["commit", "-m", "Record separately selectable current paths"],
    );

    let report = json(&workspace_env(
        &fixture.seed,
        ["doctor", &nested],
        MOCK_CREDENTIALS,
    ));
    assert_eq!(report["status"], "ok");
    assert_eq!(tasks(&report).len(), 1);
    assert_eq!(tasks(&report)[0]["path"], nested);
    assert_eq!(report["storage"]["remote_versions"], 0);
    assert_eq!(report["storage"]["issues"], value!([]));
}

#[test]
fn doctor_refuses_orphan_storage_metadata_without_s3_and_preserves_task_isolation() {
    let fixture = managed_fixture();
    let task = write_task(&fixture.seed, FIRST, FIRST, "analysis");
    write_task(&fixture.seed, SECOND, SECOND, "inputs");
    let pointer = format!("{FIRST}/data.bin.wm-storage.json");
    let manifest = value!({
        "schema_version": 1,
        "path": "data.bin",
        "kind": "file",
        "checksum": {"algorithm": "md5", "digest": "900150983cd24fb0d6963f7d28e17f72"},
        "size": 3,
        "version": {"id": "orphan-version", "etag": "900150983cd24fb0d6963f7d28e17f72"}
    });
    std::fs::write(
        task.join("data.bin.wm-storage.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    for args in [vec!["doctor", FIRST], vec!["doctor"]] {
        let output = workspace_unchecked(&fixture.seed, args);
        assert_eq!(output.status.code(), Some(2));
        let report = json(&output);
        assert_eq!(report["status"], "error");
        let integrity = report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == "managed-storage-integrity")
            .unwrap();
        assert_eq!(integrity["status"], "error");
        assert!(
            integrity["detail"]
                .as_str()
                .unwrap()
                .contains("without configured S3"),
            "{integrity}"
        );
        assert!(integrity["detail"].as_str().unwrap().contains(&pointer));
    }
    let other = json(&workspace(&fixture.seed, ["doctor", SECOND]));
    assert_eq!(other["status"], "ok");
    assert_eq!(tasks(&other).len(), 1);
    assert_eq!(tasks(&other)[0]["id"], SECOND);
}
