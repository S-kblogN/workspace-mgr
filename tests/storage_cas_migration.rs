#![cfg(all(unix, feature = "test-storage"))]

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use common::*;
use md5::{Digest, Md5};
use serde_json::Value;

const BUCKET: &str = "migration-test";
const PREFIX: &str = "repository";
const AUTH: &[(&str, &str)] = &[
    ("AWS_ACCESS_KEY_ID", "isolated-access"),
    ("AWS_SECRET_ACCESS_KEY", "isolated-secret"),
    ("AWS_REGION", "us-east-1"),
];

#[derive(Clone, Debug)]
struct Version {
    id: String,
    etag: String,
    bytes: Vec<u8>,
    owner: Option<String>,
}

#[derive(Clone, Debug)]
struct Request {
    method: String,
    key: String,
    query: BTreeMap<String, String>,
    headers: BTreeMap<String, String>,
}

#[derive(Default)]
struct State {
    objects: BTreeMap<String, Vec<Version>>,
    requests: Vec<Request>,
    trace: Vec<String>,
    next_version: usize,
    reject_put: Option<String>,
    lose_put_response: bool,
}

/// A real HTTP boundary with versioned, immutable objects. The tests use no
/// developer credentials, S3 account, local payload, or legacy cache.
struct S3Fixture {
    endpoint: String,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl S3Fixture {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let serving_state = state.clone();
        let serving_stop = stop.clone();
        let worker = thread::spawn(move || {
            while !serving_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // Darwin can inherit the listener's nonblocking mode.
                        // Reading immediately after accept must wait for bytes.
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(10)))
                            .unwrap();
                        stream
                            .set_write_timeout(Some(Duration::from_secs(10)))
                            .unwrap();
                        if let Some((request, body)) = read_request(&mut stream) {
                            let identity =
                                format!("{} {} {:?}", request.method, request.key, request.query);
                            let response =
                                respond(&mut serving_state.lock().unwrap(), request, body);
                            if let Some(response) = response {
                                let header_end = response
                                    .windows(4)
                                    .position(|part| part == b"\r\n\r\n")
                                    .unwrap()
                                    + 4;
                                let header = String::from_utf8_lossy(&response[..header_end]);
                                let length = header
                                    .lines()
                                    .find(|line| {
                                        line.to_ascii_lowercase().starts_with("content-length:")
                                    })
                                    .unwrap();
                                let outcome = stream.write_all(&response);
                                serving_state.lock().unwrap().trace.push(format!(
                                    "{identity} => {} {length}, actualbody={}, write={outcome:?}",
                                    header.lines().next().unwrap(),
                                    response.len() - header_end
                                ));
                            } else {
                                serving_state
                                    .lock()
                                    .unwrap()
                                    .trace
                                    .push(format!("{identity} => injected lost response"));
                            }
                        } else {
                            serving_state
                                .lock()
                                .unwrap()
                                .trace
                                .push("connection ended before a complete request".to_owned());
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("isolated S3 accept failed: {error}"),
                }
            }
        });
        Self {
            endpoint,
            state,
            stop,
            worker: Some(worker),
        }
    }

    fn seed(&self, key: &str, bytes: &[u8]) {
        let mut state = self.state.lock().unwrap();
        assert!(!state.objects.contains_key(key));
        let id = format!("source-{}", state.objects.len() + 1);
        state.objects.insert(
            key.to_owned(),
            vec![Version {
                id,
                etag: digest(bytes),
                bytes: bytes.to_vec(),
                owner: None,
            }],
        );
    }

    fn requests(&self) -> Vec<Request> {
        self.state.lock().unwrap().requests.clone()
    }

    fn append_source(&self, key: &str, bytes: &[u8]) {
        let mut state = self.state.lock().unwrap();
        let versions = state.objects.get_mut(key).unwrap();
        versions.push(Version {
            id: format!("source-update-{}", versions.len() + 1),
            etag: digest(bytes),
            bytes: bytes.to_vec(),
            owner: None,
        });
    }

    fn versions(&self, key: &str) -> Vec<Version> {
        self.state
            .lock()
            .unwrap()
            .objects
            .get(key)
            .cloned()
            .unwrap_or_default()
    }

    fn native_version_count(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .objects
            .values()
            .flatten()
            .filter(|version| version.owner.is_some())
            .count()
    }
}

impl Drop for S3Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let worker = self.worker.take().unwrap().join();
        if std::thread::panicking() {
            eprintln!(
                "isolated S3 trace:\n{}",
                self.state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .trace
                    .join("\n")
            );
        } else {
            worker.unwrap();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Option<(Request, Vec<u8>)> {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(position) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break position + 4;
        }
        assert!(bytes.len() < 64 * 1024, "unexpectedly large HTTP headers");
    };
    let headers = std::str::from_utf8(&bytes[..header_end]).unwrap();
    let mut lines = headers.split("\r\n");
    let mut first = lines.next().unwrap().split_whitespace();
    let method = first.next().unwrap().to_owned();
    let target = first.next().unwrap();
    let url = url::Url::parse(&format!("http://fixture{target}")).unwrap();
    let key = url
        .path()
        .strip_prefix(&format!("/{BUCKET}"))
        .unwrap_or_else(|| panic!("unexpected S3 bucket path {}", url.path()))
        .trim_start_matches('/')
        .to_owned();
    let query = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let headers: BTreeMap<_, _> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    assert!(
        !headers.contains_key("transfer-encoding"),
        "fixture expects replayable sized request bodies"
    );
    let length = headers
        .get("content-length")
        .map_or(0, |value| value.parse::<usize>().unwrap());
    while bytes.len() - header_end < length {
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    Some((
        Request {
            method,
            key,
            query,
            headers,
        },
        bytes[header_end..header_end + length].to_vec(),
    ))
}

fn response(status: u16, body: &[u8], headers: &[(&str, String)], head: bool) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        412 => "Precondition Failed",
        503 => "Service Unavailable",
        _ => "Bad Request",
    };
    let mut output = format!("HTTP/1.1 {status} {reason}\r\nConnection: close\r\n");
    if !headers
        .iter()
        .any(|(key, _)| key.eq_ignore_ascii_case("content-length"))
    {
        output.push_str(&format!(
            "Content-Length: {}\r\n",
            if head { 0 } else { body.len() }
        ));
    }
    for (key, value) in headers {
        output.push_str(&format!("{key}: {value}\r\n"));
    }
    output.push_str("\r\n");
    let mut output = output.into_bytes();
    if !head {
        output.extend_from_slice(body);
    }
    output
}

fn respond(state: &mut State, request: Request, body: Vec<u8>) -> Option<Vec<u8>> {
    state.requests.push(request.clone());
    let head = request.method == "HEAD";
    if request.key.is_empty() && request.method == "GET" && request.query.contains_key("versioning")
    {
        return Some(response(200, b"<VersioningConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Status>Enabled</Status></VersioningConfiguration>", &[], false));
    }
    if request.key.is_empty() && request.method == "GET" && request.query.contains_key("versions") {
        let prefix = request.query.get("prefix").cloned().unwrap_or_default();
        let mut xml = format!(
            "<ListVersionsResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Name>{BUCKET}</Name><Prefix>{prefix}</Prefix><IsTruncated>false</IsTruncated>"
        );
        for (key, versions) in &state.objects {
            if !key.starts_with(&prefix) {
                continue;
            }
            for (index, version) in versions.iter().enumerate().rev() {
                xml.push_str(&format!("<Version><Key>{key}</Key><VersionId>{}</VersionId><IsLatest>{}</IsLatest><LastModified>2026-10-07T12:00:00.000Z</LastModified><ETag>\"{}\"</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass></Version>", version.id, index + 1 == versions.len(), version.etag, version.bytes.len()));
            }
        }
        xml.push_str("</ListVersionsResult>");
        return Some(response(200, xml.as_bytes(), &[], false));
    }
    if request.method == "PUT" && !request.key.is_empty() {
        match state
            .objects
            .get(&request.key)
            .and_then(|versions| versions.last())
        {
            Some(latest) => {
                if request
                    .headers
                    .get("if-match")
                    .is_none_or(|etag| etag.trim_matches('"') != latest.etag)
                {
                    return Some(response(
                        412,
                        b"<Error><Code>PreconditionFailed</Code></Error>",
                        &[],
                        false,
                    ));
                }
            }
            None => assert_eq!(
                request.headers.get("if-none-match").map(String::as_str),
                Some("*"),
                "CAS import must condition creation on an absent target object"
            ),
        }
        if state.reject_put.as_ref() == Some(&request.key) {
            return Some(response(503, b"<Error><Code>ServiceUnavailable</Code><Message>Injected transfer interruption</Message></Error>", &[], false));
        }
        let owner = request
            .headers
            .get("x-amz-meta-workspace-mgr-upload")
            .cloned()
            .expect("every migrated upload must have a durable ownership token");
        assert!(!owner.is_empty());
        state.next_version += 1;
        let version = Version {
            id: format!("native-{}", state.next_version),
            etag: digest(&body),
            bytes: body,
            owner: Some(owner),
        };
        state
            .objects
            .entry(request.key)
            .or_default()
            .push(version.clone());
        if state.lose_put_response {
            state.lose_put_response = false;
            return None;
        }
        return Some(response(
            200,
            b"",
            &[
                ("x-amz-version-id", version.id),
                ("ETag", format!("\"{}\"", version.etag)),
            ],
            false,
        ));
    }
    if matches!(request.method.as_str(), "HEAD" | "GET") && !request.key.is_empty() {
        let version = state.objects.get(&request.key).and_then(|versions| {
            match request.query.get("versionId") {
                Some(id) => versions.iter().find(|version| &version.id == id),
                None => versions.last(),
            }
        });
        if let Some(version) = version {
            if request
                .headers
                .get("if-match")
                .is_some_and(|etag| etag.trim_matches('"') != version.etag)
            {
                return Some(response(
                    412,
                    b"<Error><Code>PreconditionFailed</Code></Error>",
                    &[],
                    head,
                ));
            }
            let mut headers = vec![
                ("content-length", version.bytes.len().to_string()),
                ("x-amz-version-id", version.id.clone()),
                ("ETag", format!("\"{}\"", version.etag)),
            ];
            if let Some(owner) = &version.owner {
                headers.push(("x-amz-meta-workspace-mgr-upload", owner.clone()));
            }
            return Some(response(200, &version.bytes, &headers, head));
        }
    }
    Some(response(
        404,
        b"<Error><Code>NoSuchKey</Code><Message>Isolated object does not exist</Message></Error>",
        &[],
        head,
    ))
}

fn digest(bytes: &[u8]) -> String {
    Md5::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn cas_key(digest: &str, modern: bool) -> String {
    let layout = if modern { "files/md5/" } else { "" };
    format!("{PREFIX}/{layout}{}/{}", &digest[..2], &digest[2..])
}

fn legacy_repository(remote: &S3Fixture) -> GitFixture {
    let fixture = GitFixture::new();
    fixture.clone_shared();
    fs::create_dir(fixture.shared.join(".dvc")).unwrap();
    fs::write(fixture.shared.join(".dvc/config"), format!("[core]\nremote = research-data\n['remote \"research-data\"']\nurl = s3://{BUCKET}/{PREFIX}\nendpointurl = {}\nversion_aware = false\n", remote.endpoint)).unwrap();
    fs::write(
        fixture.shared.join(".dvc/.gitignore"),
        "/config.local\n/cache\n/tmp\n",
    )
    .unwrap();
    fs::write(fixture.shared.join(".dvcignore"), "# legacy default\n").unwrap();
    fs::write(
        fixture.shared.join(".gitignore"),
        "/data\nuser-private/\n/.workspace-mgr/local/\n",
    )
    .unwrap();
    fs::write(
        fixture.shared.join(".gitattributes"),
        "*.dvc whitespace=-blank-at-eol\n*.txt text\n",
    )
    .unwrap();
    fixture
}

fn file_pointer(root: &Path, path: &str, checksum: &str, size: usize, modern: bool) {
    let sidecar = root.join(format!("{path}.dvc"));
    fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
    let hash = if modern { "  hash: md5\n" } else { "" };
    fs::write(
        sidecar,
        format!(
            "outs:\n- path: {}\n{hash}  md5: {checksum}\n  size: {size}\n",
            Path::new(path).file_name().unwrap().to_str().unwrap()
        ),
    )
    .unwrap();
}

fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != ".git")
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            (
                entry
                    .path()
                    .strip_prefix(root)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned(),
                fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

fn run(root: &Path, args: &[&str]) -> Value {
    json(&workspace_env(root, args, AUTH))
}

fn native(root: &Path, path: &str) -> Value {
    serde_json::from_slice(&fs::read(root.join(format!("{path}.wm-storage.json"))).unwrap())
        .unwrap()
}

fn assert_binding(remote: &S3Fixture, object: &str, checksum: &str, bytes: &[u8], binding: &Value) {
    let key = format!("{PREFIX}/{object}");
    let versions = remote.versions(&key);
    assert_eq!(
        versions.len(),
        1,
        "migration must reuse owned versions for {key}"
    );
    assert_latest_binding(remote, object, checksum, bytes, binding);
}

fn assert_latest_binding(
    remote: &S3Fixture,
    object: &str,
    checksum: &str,
    bytes: &[u8],
    binding: &Value,
) {
    let key = format!("{PREFIX}/{object}");
    let versions = remote.versions(&key);
    let version = versions.last().expect("a verified destination version");
    assert_eq!(version.bytes, bytes);
    assert_eq!(binding["checksum"]["digest"], checksum);
    assert_eq!(binding["size"], bytes.len());
    assert_eq!(binding["version"]["id"], version.id);
    assert_eq!(binding["version"]["etag"], version.etag);
    let requests = remote.requests();
    assert!(
        requests.iter().any(|request| request.method == "GET"
            && request.key == key
            && request.query.get("versionId") == Some(&version.id)
            && request
                .headers
                .get("if-match")
                .is_some_and(|etag| etag.trim_matches('"') == version.etag)),
        "exact uploaded version must be read and verified before binding"
    );
}

#[test]
fn remote_only_directory_preview_is_read_only_and_migration_binds_verified_versions() {
    let remote = S3Fixture::new();
    let fixture = legacy_repository(&remote);
    let first = b"first opaque payload\0\r\n";
    let second = b"second opaque payload\xff";
    let first_digest = digest(first);
    let second_digest = digest(second);
    let listing = format!(
        "[{{\"md5\": \"{first_digest}\", \"relpath\": \"a.bin\"}}, {{\"md5\": \"{second_digest}\", \"relpath\": \"nested/b.bin\"}}]"
    );
    let directory_digest = format!("{}.dir", digest(listing.as_bytes()));
    remote.seed(&cas_key(&first_digest, true), first);
    remote.seed(&cas_key(&second_digest, true), second);
    remote.seed(&cas_key(&directory_digest, true), listing.as_bytes());
    fs::write(fixture.shared.join("data.dvc"), format!("outs:\n- path: data\n  hash: md5\n  md5: {directory_digest}\n  size: {}\n  nfiles: 2\n", first.len() + second.len())).unwrap();
    git(&fixture.shared, ["add", "-A"]);
    git(
        &fixture.shared,
        ["commit", "-m", "Legacy remote-only CAS directory"],
    );
    let before = snapshot(&fixture.shared);
    let head = git(&fixture.shared, ["rev-parse", "HEAD"]).stdout;
    let index = git(&fixture.shared, ["ls-files", "--stage", "-z"]).stdout;
    let preview = run(&fixture.shared, &["manage", "--dry-run"]);
    assert_eq!(preview["status"], "dry_run");
    let objects = preview["migration"]["remote_objects"].as_array().unwrap();
    assert_eq!(objects.len(), 2);
    assert_eq!(
        preview["migration"]["remote_transfer_bytes"],
        first.len() + second.len()
    );
    assert!(
        objects
            .iter()
            .all(|object| object.get("version_id").is_none_or(Value::is_null))
    );
    assert_eq!(snapshot(&fixture.shared), before);
    assert_eq!(git(&fixture.shared, ["rev-parse", "HEAD"]).stdout, head);
    assert_eq!(
        git(&fixture.shared, ["ls-files", "--stage", "-z"]).stdout,
        index
    );
    assert!(!fixture.shared.join(".workspace-mgr").exists());
    assert_eq!(remote.native_version_count(), 0);
    assert!(
        remote
            .requests()
            .iter()
            .all(|request| matches!(request.method.as_str(), "HEAD" | "GET"))
    );

    let applied = run(&fixture.shared, &["manage"]);
    assert_eq!(applied["status"], "managed");
    let manifest = native(&fixture.shared, "data");
    assert_eq!(manifest["kind"], "directory");
    let entries = manifest["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["path"], "a.bin");
    assert_eq!(entries[1]["path"], "nested/b.bin");
    assert_binding(&remote, "data/a.bin", &first_digest, first, &entries[0]);
    assert_binding(
        &remote,
        "data/nested/b.bin",
        &second_digest,
        second,
        &entries[1],
    );
    assert!(
        !fixture.shared.join("data").exists(),
        "adoption does not materialize absent payloads"
    );
    assert!(!fixture.shared.join("data.dvc").exists());
    assert!(!fixture.shared.join(".dvc").exists());
    assert!(!fixture.shared.join(".dvcignore").exists());
    assert!(
        !fixture
            .shared
            .join(".workspace-mgr/local/storage-import.json")
            .exists()
    );
    assert_eq!(
        fs::read_to_string(fixture.shared.join(".gitattributes")).unwrap(),
        "*.txt text\n"
    );
    assert_eq!(git(&fixture.shared, ["rev-parse", "HEAD"]).stdout, head);
    assert_eq!(
        git(&fixture.shared, ["ls-files", "--stage", "-z"]).stdout,
        index
    );
    assert_eq!(
        remote.versions(&cas_key(&directory_digest, true))[0].bytes,
        listing.as_bytes()
    );
    assert_eq!(
        remote.versions(&cas_key(&first_digest, true))[0].bytes,
        first
    );
    assert_eq!(
        remote.versions(&cas_key(&second_digest, true))[0].bytes,
        second
    );
    assert!(
        remote
            .requests()
            .iter()
            .all(|request| request.method != "DELETE")
    );
    assert_eq!(run(&fixture.shared, &["manage"])["status"], "no_changes");
    assert_eq!(remote.native_version_count(), 2);
}

#[test]
fn dvc2_normalized_identity_preserves_raw_crlf_payload_bytes() {
    let remote = S3Fixture::new();
    let fixture = legacy_repository(&remote);
    let bytes = b"a\r\nb\r\n";
    let checksum = digest(b"a\nb\n");
    remote.seed(&cas_key(&checksum, false), bytes);
    file_pointer(&fixture.shared, "data", &checksum, bytes.len(), false);
    let report = run(&fixture.shared, &["manage"]);
    assert_eq!(
        report["migration"]["remote_objects"][0]["algorithm"],
        "md5-dos2unix"
    );
    let manifest = native(&fixture.shared, "data");
    assert_eq!(manifest["checksum"]["algorithm"], "md5-dos2unix");
    assert_binding(&remote, "data", &checksum, bytes, &manifest);
    assert_ne!(
        manifest["version"]["etag"], checksum,
        "ETag is the raw stored object's identity"
    );
}

#[test]
fn corrupt_cas_payload_refuses_before_upload_or_legacy_control_removal() {
    let remote = S3Fixture::new();
    let fixture = legacy_repository(&remote);
    let checksum = digest(b"good");
    remote.seed(&cas_key(&checksum, true), b"evil");
    file_pointer(&fixture.shared, "data", &checksum, 4, true);
    let original = fs::read(fixture.shared.join("data.dvc")).unwrap();
    let head = git(&fixture.shared, ["rev-parse", "HEAD"]).stdout;
    let index = git(&fixture.shared, ["ls-files", "--stage", "-z"]).stdout;
    let refused = workspace_env_unchecked(&fixture.shared, ["manage"], AUTH);
    assert!(!refused.status.success());
    let error = String::from_utf8_lossy(&refused.stderr);
    assert!(
        error.contains("checksum"),
        "unexpected corruption refusal: {error}"
    );
    assert_eq!(fs::read(fixture.shared.join("data.dvc")).unwrap(), original);
    assert!(fixture.shared.join(".dvc/config").exists());
    assert!(fixture.shared.join(".dvcignore").exists());
    assert!(!fixture.shared.join("data.wm-storage.json").exists());
    assert_eq!(remote.native_version_count(), 0);
    assert!(
        remote
            .requests()
            .iter()
            .all(|request| matches!(request.method.as_str(), "HEAD" | "GET"))
    );
    assert_eq!(git(&fixture.shared, ["rev-parse", "HEAD"]).stdout, head);
    assert_eq!(
        git(&fixture.shared, ["ls-files", "--stage", "-z"]).stdout,
        index
    );
}

#[test]
fn foreign_target_object_refuses_before_any_upload_or_control_conversion() {
    let remote = S3Fixture::new();
    let fixture = legacy_repository(&remote);
    let bytes = b"verified CAS source";
    let checksum = digest(bytes);
    remote.seed(&cas_key(&checksum, true), bytes);
    remote.seed(
        &format!("{PREFIX}/data"),
        b"another object's retained history",
    );
    file_pointer(&fixture.shared, "data", &checksum, bytes.len(), true);
    let refused = workspace_env_unchecked(&fixture.shared, ["manage"], AUTH);
    assert!(!refused.status.success());
    let error = String::from_utf8_lossy(&refused.stderr);
    assert!(
        error.contains("ownership") || error.contains("unowned"),
        "unexpected foreign target refusal: {error}"
    );
    assert!(fixture.shared.join("data.dvc").exists());
    assert!(fixture.shared.join(".dvc/config").exists());
    assert!(!fixture.shared.join("data.wm-storage.json").exists());
    assert_eq!(
        remote.versions(&format!("{PREFIX}/data"))[0].bytes,
        b"another object's retained history"
    );
    assert_eq!(remote.native_version_count(), 0);
    assert!(
        remote
            .requests()
            .iter()
            .all(|request| matches!(request.method.as_str(), "HEAD" | "GET"))
    );
}

#[test]
fn interrupted_transfer_resumes_without_duplicate_owned_versions() {
    let remote = S3Fixture::new();
    let fixture = legacy_repository(&remote);
    for (path, bytes) in [
        ("data/a.bin", b"first".as_slice()),
        ("data/b.bin", b"second".as_slice()),
    ] {
        let checksum = digest(bytes);
        remote.seed(&cas_key(&checksum, true), bytes);
        file_pointer(&fixture.shared, path, &checksum, bytes.len(), true);
    }
    remote.state.lock().unwrap().reject_put = Some(format!("{PREFIX}/data/b.bin"));
    let head = git(&fixture.shared, ["rev-parse", "HEAD"]).stdout;
    let index = git(&fixture.shared, ["ls-files", "--stage", "-z"]).stdout;
    let refused = workspace_env_unchecked(&fixture.shared, ["manage"], AUTH);
    assert!(!refused.status.success());
    assert_eq!(remote.versions(&format!("{PREFIX}/data/a.bin")).len(), 1);
    assert_eq!(remote.versions(&format!("{PREFIX}/data/b.bin")).len(), 0);
    assert!(fixture.shared.join("data/a.bin.dvc").exists());
    assert!(fixture.shared.join("data/b.bin.dvc").exists());
    assert!(fixture.shared.join(".dvc/config").exists());
    assert!(!fixture.shared.join("data/a.bin.wm-storage.json").exists());
    assert!(
        fixture
            .shared
            .join(".workspace-mgr/local/storage-import.json")
            .exists()
    );
    assert_eq!(git(&fixture.shared, ["rev-parse", "HEAD"]).stdout, head);
    assert_eq!(
        git(&fixture.shared, ["ls-files", "--stage", "-z"]).stdout,
        index
    );
    remote.state.lock().unwrap().reject_put = None;
    let applied = run(&fixture.shared, &["manage"]);
    assert_eq!(applied["status"], "managed");
    assert_binding(
        &remote,
        "data/a.bin",
        &digest(b"first"),
        b"first",
        &native(&fixture.shared, "data/a.bin"),
    );
    assert_binding(
        &remote,
        "data/b.bin",
        &digest(b"second"),
        b"second",
        &native(&fixture.shared, "data/b.bin"),
    );
    assert!(
        !fixture
            .shared
            .join(".workspace-mgr/local/storage-import.json")
            .exists()
    );
    assert!(!fixture.shared.join(".dvc").exists());
}

#[test]
fn old_local_cache_is_retained_alongside_new_transfer_cache_and_upload_receipts() {
    let remote = S3Fixture::new();
    let fixture = legacy_repository(&remote);
    let bytes = b"legacy cache coexists with remotely verified transfer bytes";
    let checksum = digest(bytes);
    let relative_cache = format!("files/md5/{}/{}", &checksum[..2], &checksum[2..]);
    let cache = fixture.shared.join(".dvc/cache").join(&relative_cache);
    fs::create_dir_all(cache.parent().unwrap()).unwrap();
    fs::write(&cache, bytes).unwrap();
    remote.seed(&cas_key(&checksum, true), bytes);
    file_pointer(&fixture.shared, "data", &checksum, bytes.len(), true);
    run(&fixture.shared, &["manage"]);
    assert_binding(
        &remote,
        "data",
        &checksum,
        bytes,
        &native(&fixture.shared, "data"),
    );
    assert_eq!(
        fs::read(
            fixture
                .shared
                .join(".workspace-mgr/local/cache/legacy")
                .join(relative_cache)
        )
        .unwrap(),
        bytes
    );
    assert!(
        fixture
            .shared
            .join(".workspace-mgr/local/storage-import-uploads")
            .is_dir()
    );
    assert!(!fixture.shared.join(".dvc").exists());
    assert!(
        !fixture
            .shared
            .join(".workspace-mgr/local/storage-import.json")
            .exists()
    );
}

#[test]
fn interrupted_import_keeps_pinned_sources_and_refuses_a_missing_recorded_version() {
    for remove_recorded in [false, true] {
        let remote = S3Fixture::new();
        let fixture = legacy_repository(&remote);
        for (path, bytes) in [
            ("data/a.bin", b"first".as_slice()),
            ("data/b.bin", b"second".as_slice()),
        ] {
            let checksum = digest(bytes);
            remote.seed(&cas_key(&checksum, true), bytes);
            file_pointer(&fixture.shared, path, &checksum, bytes.len(), true);
        }
        remote.state.lock().unwrap().reject_put = Some(format!("{PREFIX}/data/b.bin"));
        let failed = workspace_env_unchecked(&fixture.shared, ["manage"], AUTH);
        assert!(!failed.status.success());
        assert_eq!(remote.native_version_count(), 1);
        let journal = fixture
            .shared
            .join(".workspace-mgr/local/storage-import.json");
        assert!(journal.exists());
        let second_key = cas_key(&digest(b"second"), true);
        let original_id = remote.versions(&second_key)[0].id.clone();
        remote.append_source(&second_key, b"broken");
        if remove_recorded {
            remote
                .state
                .lock()
                .unwrap()
                .objects
                .get_mut(&second_key)
                .unwrap()
                .retain(|version| version.id != original_id);
        }
        fs::remove_dir_all(fixture.shared.join(".workspace-mgr/local/cache")).unwrap();
        remote.state.lock().unwrap().reject_put = None;
        let request_start = remote.requests().len();
        let resumed = workspace_env_unchecked(&fixture.shared, ["manage"], AUTH);
        if remove_recorded {
            assert!(
                !resumed.status.success(),
                "a missing recorded source must not be replaced by its latest version"
            );
            assert!(journal.exists());
            assert!(fixture.shared.join("data/a.bin.dvc").exists());
            assert!(fixture.shared.join("data/b.bin.dvc").exists());
            assert!(!fixture.shared.join("data/a.bin.wm-storage.json").exists());
            assert_eq!(remote.native_version_count(), 1);
            run(&fixture.shared, &["manage", "--cancel-migration"]);
            assert!(!journal.exists());
            assert!(fixture.shared.join("data/b.bin.dvc").exists());
        } else {
            assert!(
                resumed.status.success(),
                "pinned original source should still hydrate: {}",
                String::from_utf8_lossy(&resumed.stderr)
            );
            assert_binding(
                &remote,
                "data/a.bin",
                &digest(b"first"),
                b"first",
                &native(&fixture.shared, "data/a.bin"),
            );
            assert_binding(
                &remote,
                "data/b.bin",
                &digest(b"second"),
                b"second",
                &native(&fixture.shared, "data/b.bin"),
            );
            let requests = remote.requests();
            assert!(
                requests[request_start..]
                    .iter()
                    .any(|request| request.method == "GET"
                        && request.key == second_key
                        && request.query.get("versionId") == Some(&original_id)),
                "retry must read the recorded source version instead of the changed latest object"
            );
        }
        assert!(
            remote
                .requests()
                .iter()
                .all(|request| request.method != "DELETE")
        );
    }
}

#[test]
fn cancelling_partial_import_preserves_controls_and_versions_for_a_fresh_plan() {
    let remote = S3Fixture::new();
    let fixture = legacy_repository(&remote);
    for (path, bytes) in [
        ("data/a.bin", b"first".as_slice()),
        ("data/b.bin", b"second".as_slice()),
    ] {
        let checksum = digest(bytes);
        remote.seed(&cas_key(&checksum, true), bytes);
        file_pointer(&fixture.shared, path, &checksum, bytes.len(), true);
    }
    remote.state.lock().unwrap().reject_put = Some(format!("{PREFIX}/data/b.bin"));
    let failed = workspace_env_unchecked(&fixture.shared, ["manage"], AUTH);
    assert!(!failed.status.success());
    let journal = fixture
        .shared
        .join(".workspace-mgr/local/storage-import.json");
    assert!(journal.exists());
    let before = snapshot(&fixture.shared);
    let head = git(&fixture.shared, ["rev-parse", "HEAD"]).stdout;
    let index = git(&fixture.shared, ["ls-files", "--stage", "-z"]).stdout;
    let remote_requests = remote.requests().len();
    assert_eq!(
        run(
            &fixture.shared,
            &["manage", "--cancel-migration", "--dry-run"]
        )["status"],
        "dry_run"
    );
    assert_eq!(snapshot(&fixture.shared), before);
    assert_eq!(
        remote.requests().len(),
        remote_requests,
        "cancellation needs no S3 calls"
    );
    assert_eq!(
        run(&fixture.shared, &["manage", "--cancel-migration"])["status"],
        "migration_cancelled"
    );
    assert!(!journal.exists());
    for path in [
        "data/a.bin.dvc",
        "data/b.bin.dvc",
        ".dvc/config",
        ".dvc/.gitignore",
        ".dvcignore",
    ] {
        assert_eq!(fs::read(fixture.shared.join(path)).unwrap(), before[path]);
    }
    for (path, bytes) in &before {
        if path.starts_with(".workspace-mgr/local/")
            && path != ".workspace-mgr/local/storage-import.json"
        {
            assert_eq!(
                &fs::read(fixture.shared.join(path)).unwrap(),
                bytes,
                "cancellation must retain private receipts and cache {path}"
            );
        }
    }
    assert_eq!(remote.native_version_count(), 1);
    assert_eq!(git(&fixture.shared, ["rev-parse", "HEAD"]).stdout, head);
    assert_eq!(
        git(&fixture.shared, ["ls-files", "--stage", "-z"]).stdout,
        index
    );
    assert!(
        remote
            .requests()
            .iter()
            .all(|request| request.method != "DELETE")
    );
    remote.state.lock().unwrap().reject_put = None;
    let corrected = b"corrected first opaque payload";
    let corrected_digest = digest(corrected);
    remote.seed(&cas_key(&corrected_digest, true), corrected);
    file_pointer(
        &fixture.shared,
        "data/a.bin",
        &corrected_digest,
        corrected.len(),
        true,
    );
    run(&fixture.shared, &["manage"]);
    assert_latest_binding(
        &remote,
        "data/a.bin",
        &corrected_digest,
        corrected,
        &native(&fixture.shared, "data/a.bin"),
    );
    assert_binding(
        &remote,
        "data/b.bin",
        &digest(b"second"),
        b"second",
        &native(&fixture.shared, "data/b.bin"),
    );
    let first_versions = remote.versions(&format!("{PREFIX}/data/a.bin"));
    assert_eq!(first_versions.len(), 2);
    assert_eq!(
        first_versions[0].bytes, b"first",
        "cancel/replan must retain the previous owned version"
    );
    assert_eq!(remote.native_version_count(), 3);
}

#[test]
fn cancelled_normalized_import_replans_for_different_raw_bytes_with_the_same_identity() {
    let remote = S3Fixture::new();
    let fixture = legacy_repository(&remote);
    let original = b"a\r\nb\n";
    let replacement = b"a\nb\r\n";
    let normalized_digest = digest(b"a\nb\n");
    assert_eq!(original.len(), 5);
    assert_eq!(replacement.len(), 5);
    assert_ne!(digest(original), digest(replacement));
    let source_key = cas_key(&normalized_digest, false);
    remote.seed(&source_key, original);
    file_pointer(&fixture.shared, "data/a.txt", &normalized_digest, 5, false);
    let later = b"later opaque bytes";
    let later_digest = digest(later);
    remote.seed(&cas_key(&later_digest, false), later);
    file_pointer(
        &fixture.shared,
        "data/z.bin",
        &later_digest,
        later.len(),
        false,
    );
    let first_target = format!("{PREFIX}/data/a.txt");
    remote.state.lock().unwrap().reject_put = Some(format!("{PREFIX}/data/z.bin"));
    let failed = workspace_env_unchecked(&fixture.shared, ["manage"], AUTH);
    assert!(!failed.status.success());
    let first_versions = remote.versions(&first_target);
    assert_eq!(first_versions.len(), 1);
    assert_eq!(first_versions[0].bytes, original);
    let original_version = first_versions[0].clone();
    let receipt_dir = fixture
        .shared
        .join(".workspace-mgr/local/storage-import-uploads");
    let original_receipts: BTreeMap<_, _> = snapshot(&receipt_dir)
        .into_iter()
        .filter(|(_, raw)| {
            serde_json::from_slice::<Value>(raw)
                .is_ok_and(|receipt| receipt["context"]["key"] == first_target)
        })
        .collect();
    assert_eq!(
        original_receipts.len(),
        1,
        "first upload must retain an ownership receipt"
    );
    assert!(fixture.shared.join("data/a.txt.dvc").exists());
    run(&fixture.shared, &["manage", "--cancel-migration"]);
    for (path, raw) in &original_receipts {
        assert_eq!(&fs::read(receipt_dir.join(path)).unwrap(), raw);
    }

    // This changes only the raw S3 bytes and source version: legacy normalized
    // digest, physical size, output path and sidecar contents stay identical.
    remote.append_source(&source_key, replacement);
    remote.state.lock().unwrap().reject_put = None;
    run(&fixture.shared, &["manage"]);
    let manifest = native(&fixture.shared, "data/a.txt");
    assert_eq!(manifest["checksum"]["algorithm"], "md5-dos2unix");
    assert_latest_binding(
        &remote,
        "data/a.txt",
        &normalized_digest,
        replacement,
        &manifest,
    );
    let versions = remote.versions(&first_target);
    assert_eq!(
        versions.len(),
        2,
        "a different raw payload needs a new exact version even when its normalized identity matches"
    );
    assert_eq!(versions[0].id, original_version.id);
    assert_eq!(versions[0].bytes, original);
    assert_ne!(versions[1].id, original_version.id);
    assert_ne!(versions[1].etag, original_version.etag);
    for (path, raw) in &original_receipts {
        assert_eq!(
            &fs::read(receipt_dir.join(path)).unwrap(),
            raw,
            "replanning must preserve the original raw version's receipt"
        );
    }
    assert_eq!(remote.versions(&source_key)[0].bytes, original);
    assert_eq!(remote.versions(&source_key)[1].bytes, replacement);
    assert_eq!(remote.native_version_count(), 3);

    let task = run(
        &fixture.shared,
        &[
            "task",
            "create",
            "raw-variant-hydration",
            "--title",
            "Hydrate the exact raw variant",
            "--purpose",
            "Verify normalized identity does not replace exact raw byte identity",
            "--timestamp",
            "20251001-140000",
            "--scope",
            "data",
            "--scope-note",
            "The user requested hydration of the newly bound native version",
        ],
    );
    git(
        &fixture.shared,
        [
            "add",
            "-f",
            "data/a.txt.wm-storage.json",
            "data/z.bin.wm-storage.json",
        ],
    );
    fs::remove_dir_all(fixture.shared.join(".workspace-mgr/local/cache")).unwrap();
    let request_start = remote.requests().len();
    run(
        &fixture.shared,
        &[
            "storage",
            "hydrate",
            "data/a.txt",
            "--manifest",
            task["manifest"].as_str().unwrap(),
        ],
    );
    assert_eq!(
        fs::read(fixture.shared.join("data/a.txt")).unwrap(),
        replacement
    );
    let requests = remote.requests();
    assert!(
        requests[request_start..]
            .iter()
            .any(|request| request.method == "GET"
                && request.key == first_target
                && request.query.get("versionId") == Some(&versions[1].id))
    );
    assert!(
        requests[request_start..]
            .iter()
            .all(|request| matches!(request.method.as_str(), "HEAD" | "GET"))
    );
    assert!(requests.iter().all(|request| request.method != "DELETE"));
    assert_eq!(remote.native_version_count(), 3);
}

#[test]
fn lost_upload_response_recovers_the_owned_exact_version_before_binding() {
    let remote = S3Fixture::new();
    let fixture = legacy_repository(&remote);
    let bytes = b"lost response does not mean a lost object";
    let checksum = digest(bytes);
    remote.seed(&cas_key(&checksum, true), bytes);
    file_pointer(&fixture.shared, "data", &checksum, bytes.len(), true);
    remote.state.lock().unwrap().lose_put_response = true;
    // Either the first invocation reconciles the lost response immediately or
    // it leaves an import journal for the next invocation. Both must reuse it.
    let first = workspace_env_unchecked(&fixture.shared, ["manage"], AUTH);
    if !first.status.success() {
        assert!(fixture.shared.join("data.dvc").exists());
        assert!(
            fixture
                .shared
                .join(".workspace-mgr/local/storage-import.json")
                .exists()
        );
        run(&fixture.shared, &["manage"]);
    }
    assert_binding(
        &remote,
        "data",
        &checksum,
        bytes,
        &native(&fixture.shared, "data"),
    );
    assert!(
        remote
            .requests()
            .iter()
            .any(|request| request.method == "GET" && request.query.contains_key("versions")),
        "lost response must be reconciled through owned version inventory"
    );
    assert_eq!(remote.native_version_count(), 1);
    assert!(!fixture.shared.join("data.dvc").exists());
    assert!(
        !fixture
            .shared
            .join(".workspace-mgr/local/storage-import.json")
            .exists()
    );
}

#[test]
fn old_git_revision_hydrates_retained_cas_objects_after_native_adoption() {
    let remote = S3Fixture::new();
    let fixture = legacy_repository(&remote);
    fs::write(fixture.shared.join(".workspace-mgr.toml"), format!("minimum_cli_version = \"0.8.0\"\n[git]\nremote = \"origin\"\nbranch = \"main\"\n[s3]\nurl = \"s3://{BUCKET}/{PREFIX}\"\nendpoint_url = \"{}\"\n", remote.endpoint)).unwrap();
    let task = run(
        &fixture.shared,
        &[
            "task",
            "create",
            "cas-history",
            "--title",
            "Read retained CAS history",
            "--purpose",
            "Verify old Git snapshots retain exact content after adoption",
            "--timestamp",
            "20251001-120000",
            "--scope",
            "data",
            "--scope-note",
            "The user requested hydration of this retained historical payload",
        ],
    );
    let manifest = Path::new(task["manifest"].as_str().unwrap())
        .strip_prefix(fixture.shared.canonicalize().unwrap())
        .unwrap()
        .to_path_buf();
    let bytes = b"historical opaque payload\0\xff\r\n";
    let checksum = digest(bytes);
    let source = cas_key(&checksum, true);
    remote.seed(&source, bytes);
    file_pointer(&fixture.shared, "data", &checksum, bytes.len(), true);
    git(&fixture.shared, ["add", "-A"]);
    git(
        &fixture.shared,
        ["commit", "-m", "Legacy CAS snapshot before native adoption"],
    );
    let old_oid = String::from_utf8(git(&fixture.shared, ["rev-parse", "HEAD"]).stdout).unwrap();
    run(&fixture.shared, &["manage"]);
    assert_binding(
        &remote,
        "data",
        &checksum,
        bytes,
        &native(&fixture.shared, "data"),
    );
    git(&fixture.shared, ["add", "-A"]);
    git(
        &fixture.shared,
        ["commit", "-m", "Adopt native exact-version storage"],
    );
    let historical = fixture.root.join("historical");
    git(
        &fixture.shared,
        [
            "worktree",
            "add",
            "--detach",
            historical.to_str().unwrap(),
            old_oid.trim(),
        ],
    );
    // Adoption has already downloaded this content. Clear only the disposable
    // shared cache so historical hydration must prove the retained remote key.
    let cache = fixture.shared.join(".workspace-mgr/local/cache");
    if cache.exists() {
        fs::remove_dir_all(cache).unwrap();
    }
    let requests_before = remote.requests().len();
    run(
        &historical,
        &[
            "storage",
            "hydrate",
            "data",
            "--manifest",
            historical.join(manifest).to_str().unwrap(),
        ],
    );
    assert_eq!(fs::read(historical.join("data")).unwrap(), bytes);
    assert!(historical.join("data.dvc").exists());
    assert!(!historical.join("data.wm-storage.json").exists());
    let history_reads = remote.requests();
    assert!(
        history_reads[requests_before..]
            .iter()
            .any(|request| request.method == "GET" && request.key == source),
        "historical hydration must read the retained CAS source"
    );
    assert!(
        history_reads[requests_before..]
            .iter()
            .all(|request| matches!(request.method.as_str(), "HEAD" | "GET"))
    );
    assert_eq!(remote.native_version_count(), 1);
    assert_eq!(remote.versions(&source).len(), 1);
}

#[test]
fn mixed_historical_cas_and_native_exact_bindings_hydrate_from_their_own_keys() {
    let remote = S3Fixture::new();
    let fixture = legacy_repository(&remote);
    fs::write(fixture.shared.join(".workspace-mgr.toml"), format!("minimum_cli_version = \"0.8.0\"\n[git]\nremote = \"origin\"\nbranch = \"main\"\n[s3]\nurl = \"s3://{BUCKET}/{PREFIX}\"\nendpoint_url = \"{}\"\n", remote.endpoint)).unwrap();
    let task = run(
        &fixture.shared,
        &[
            "task",
            "create",
            "mixed-history",
            "--title",
            "Hydrate mixed storage history",
            "--purpose",
            "Verify legacy CAS and native exact versions coexist in one snapshot",
            "--timestamp",
            "20251001-130000",
            "--scope",
            "data",
            "--scope-note",
            "The user requested hydration of both retained historical objects",
        ],
    );
    let legacy_bytes = b"legacy CAS bytes\0\r\n";
    let legacy_digest = digest(legacy_bytes);
    let legacy_key = cas_key(&legacy_digest, true);
    remote.seed(&legacy_key, legacy_bytes);
    file_pointer(
        &fixture.shared,
        "data/legacy.bin",
        &legacy_digest,
        legacy_bytes.len(),
        true,
    );
    let native_bytes = b"independent native exact version\xff";
    let native_digest = digest(native_bytes);
    let native_key = format!("{PREFIX}/data/native.bin");
    remote.seed(&native_key, native_bytes);
    let native_version = remote.versions(&native_key)[0].clone();
    fs::write(
        fixture.shared.join("data/native.bin.wm-storage.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema_version": 1,
            "path": "native.bin",
            "kind": "file",
            "checksum": {"algorithm": "md5", "digest": native_digest},
            "size": native_bytes.len(),
            "version": {"id": native_version.id, "etag": native_version.etag}
        }))
        .unwrap(),
    )
    .unwrap();
    // The original DVC repository ignored the payload directory. Its sidecars
    // are tracked controls, as they would be in an existing historical commit.
    git(
        &fixture.shared,
        [
            "add",
            "-f",
            "data/legacy.bin.dvc",
            "data/native.bin.wm-storage.json",
        ],
    );
    git(&fixture.shared, ["add", "-A"]);
    git(
        &fixture.shared,
        ["commit", "-m", "Historical mixed storage metadata"],
    );
    run(
        &fixture.shared,
        &[
            "storage",
            "hydrate",
            "data/legacy.bin",
            "data/native.bin",
            "--manifest",
            task["manifest"].as_str().unwrap(),
        ],
    );
    assert_eq!(
        fs::read(fixture.shared.join("data/legacy.bin")).unwrap(),
        legacy_bytes
    );
    assert_eq!(
        fs::read(fixture.shared.join("data/native.bin")).unwrap(),
        native_bytes
    );
    let requests = remote.requests();
    assert!(
        requests
            .iter()
            .any(|request| request.method == "GET" && request.key == legacy_key),
        "unbound legacy metadata must use its content-addressed source key"
    );
    assert!(
        requests.iter().any(|request| request.method == "GET"
            && request.key == native_key
            && request.query.get("versionId") == Some(&native_version.id)),
        "native metadata must use the bound logical key and exact version"
    );
    assert!(
        requests
            .iter()
            .all(|request| matches!(request.method.as_str(), "HEAD" | "GET"))
    );
    assert!(fixture.shared.join("data/legacy.bin.dvc").exists());
    assert!(
        fixture
            .shared
            .join("data/native.bin.wm-storage.json")
            .exists()
    );
}
