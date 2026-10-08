use super::*;
use std::cell::RefCell;
use std::path::PathBuf;

#[derive(Clone)]
struct Version {
    value: Value,
    body: Vec<u8>,
}
#[derive(Default)]
struct State {
    versions: Vec<Version>,
    uploads: BTreeMap<String, Value>,
    calls: Vec<(String, Value)>,
    counts: BTreeMap<String, usize>,
    fail: BTreeMap<String, usize>,
    lose: BTreeMap<String, usize>,
    reject: BTreeMap<String, String>,
    page: usize,
    conditional_error: Option<String>,
    rejected_header: Option<String>,
    versioning: Option<String>,
    remove_claim_after: Option<(String, String, String)>,
    require_fenced_journal: Option<PathBuf>,
}
struct Memory {
    state: RefCell<State>,
    b2: bool,
}
fn provider(code: &str) -> S3Error {
    S3Error {
        status: None,
        code: code.to_owned(),
        message: "isolated fixture".to_owned(),
        header: None,
        details: Value::Null,
    }
}
impl Memory {
    fn new() -> Self {
        Self {
            state: RefCell::new(State {
                page: 1000,
                ..State::default()
            }),
            b2: false,
        }
    }
    fn add(
        &self,
        object: &str,
        version: &str,
        body: &[u8],
        marker: bool,
        size: Option<u64>,
        properties: Value,
    ) {
        let mut state = self.state.borrow_mut();
        for row in &mut state.versions {
            if row.value["Key"] == object {
                row.value["IsLatest"] = false.into();
            }
        }
        let mut value = json!({"Key":object,"VersionId":version,"IsLatest":true,"delete_marker":marker,
            "LastModified":format!("2026-07-02T00:00:{:02}+00:00",state.versions.len()+1),"Size":size.unwrap_or(body.len() as u64),
            "ETag":format!("\"{}\"",encode_lower(Md5::digest(body))),"Metadata":{},"TagSet":[]});
        if let Some(properties) = properties.as_object() {
            for (name, property) in properties {
                value[name] = property.clone();
            }
        }
        state.versions.push(Version {
            value,
            body: body.to_vec(),
        });
    }
    fn source(&self, object: &str, version: &str, body: &[u8], marker: bool) {
        self.add(
            &format!("storage/task/{object}"),
            version,
            body,
            marker,
            None,
            Value::Null,
        );
    }
    fn versions(&self, prefix: &str) -> Vec<Version> {
        self.state
            .borrow()
            .versions
            .iter()
            .filter(|row| row.value["Key"].as_str().unwrap().starts_with(prefix))
            .cloned()
            .collect()
    }
    fn count(&self, method: &str) -> usize {
        *self.state.borrow().counts.get(method).unwrap_or(&0)
    }
    fn response(
        &self,
        method: &str,
        args: &Value,
        body: Option<&[u8]>,
    ) -> std::result::Result<S3Response, S3Error> {
        let value = match method {
            "get_bucket_versioning" => {
                json!({"Status":self.state.borrow().versioning.as_deref().unwrap_or("Enabled")})
            }
            "list_object_versions" => {
                let mut versions = self.versions(args["Prefix"].as_str().unwrap());
                versions.sort_by(|left, right| {
                    left.value["Key"]
                        .as_str()
                        .cmp(&right.value["Key"].as_str())
                        .then_with(|| {
                            right.value["LastModified"]
                                .as_str()
                                .cmp(&left.value["LastModified"].as_str())
                        })
                });
                let start = if args["KeyMarker"].is_string() {
                    versions
                        .iter()
                        .position(|row| {
                            row.value["Key"] == args["KeyMarker"]
                                && row.value["VersionId"] == args["VersionIdMarker"]
                        })
                        .unwrap()
                        + 1
                } else {
                    0
                };
                let end = (start + self.state.borrow().page).min(versions.len());
                let mut result =
                    json!({"Versions":[],"DeleteMarkers":[],"IsTruncated":end<versions.len()});
                for row in &versions[start..end] {
                    let section = if row.value["delete_marker"] == true {
                        "DeleteMarkers"
                    } else {
                        "Versions"
                    };
                    result[section]
                        .as_array_mut()
                        .unwrap()
                        .push(row.value.clone());
                }
                if end < versions.len() {
                    result["NextKeyMarker"] = versions[end - 1].value["Key"].clone();
                    result["NextVersionIdMarker"] = versions[end - 1].value["VersionId"].clone();
                }
                result
            }
            "list_multipart_uploads" => {
                let state = self.state.borrow();
                let uploads = state
                    .uploads
                    .iter()
                    .filter(|(_, row)| {
                        row["Key"]
                            .as_str()
                            .unwrap()
                            .starts_with(args["Prefix"].as_str().unwrap())
                    })
                    .map(|(id, row)| json!({"Key":row["Key"],"UploadId":id}))
                    .collect::<Vec<_>>();
                let start = if args["KeyMarker"].is_string() {
                    uploads
                        .iter()
                        .position(|row| {
                            row["Key"] == args["KeyMarker"]
                                && row["UploadId"] == args["UploadIdMarker"]
                        })
                        .unwrap()
                        + 1
                } else {
                    0
                };
                let end = (start + state.page).min(uploads.len());
                let mut result =
                    json!({"Uploads":uploads[start..end],"IsTruncated":end<uploads.len()});
                if end < uploads.len() {
                    result["NextKeyMarker"] = uploads[end - 1]["Key"].clone();
                    result["NextUploadIdMarker"] = uploads[end - 1]["UploadId"].clone();
                }
                result
            }
            "head_object" | "get_object" | "get_object_tagging" => {
                let state = self.state.borrow();
                let row = state
                    .versions
                    .iter()
                    .find(|row| {
                        row.value["Key"] == args["Key"]
                            && row.value["VersionId"] == args["VersionId"]
                            && row.value["delete_marker"] != true
                    })
                    .ok_or_else(|| provider("NoSuchVersion"))?;
                if method == "get_object" {
                    return Ok(S3Response {
                        value: row.value.clone(),
                        body: row.body.clone(),
                    });
                }
                if method == "get_object_tagging" {
                    json!({"TagSet":row.value["TagSet"]})
                } else {
                    let mut value = row.value.clone();
                    value["ContentLength"] = value["Size"].clone();
                    value
                }
            }
            "copy_object" => {
                let source = {
                    let state = self.state.borrow();
                    state
                        .versions
                        .iter()
                        .find(|row| {
                            row.value["Key"] == args["CopySource"]["Key"]
                                && row.value["VersionId"] == args["CopySource"]["VersionId"]
                        })
                        .cloned()
                        .unwrap()
                };
                assert_eq!(source.value["ETag"], args["CopySourceIfMatch"]);
                let version = format!("copied-{}", self.state.borrow().versions.len());
                let mut properties = args.clone();
                properties["TagSet"] = source.value["TagSet"].clone();
                self.add(
                    args["Key"].as_str().unwrap(),
                    &version,
                    &source.body,
                    false,
                    source.value["Size"].as_u64(),
                    properties,
                );
                let row = self.state.borrow().versions.last().unwrap().value.clone();
                json!({"VersionId":version,"CopyObjectResult":{"ETag":row["ETag"],"LastModified":row["LastModified"]}})
            }
            "delete_object" => {
                if let Some(version) = args["VersionId"].as_str() {
                    assert!(!args["Key"].as_str().unwrap().starts_with("storage/task/"));
                    let mut state = self.state.borrow_mut();
                    state.versions.retain(|row| {
                        row.value["Key"] != args["Key"] || row.value["VersionId"] != version
                    });
                    if let Some(row) = state
                        .versions
                        .iter_mut()
                        .rev()
                        .find(|row| row.value["Key"] == args["Key"])
                    {
                        row.value["IsLatest"] = true.into();
                    }
                    json!({"VersionId":version})
                } else {
                    let version = format!("marker-{}", self.state.borrow().versions.len());
                    self.add(
                        args["Key"].as_str().unwrap(),
                        &version,
                        b"",
                        true,
                        None,
                        Value::Null,
                    );
                    json!({"VersionId":version,"DeleteMarker":true})
                }
            }
            "put_object" => {
                if args["IfNoneMatch"] == "*" {
                    let state = self.state.borrow();
                    if let Some(code) = &state.conditional_error {
                        let mut error = provider(code);
                        error.header = state.rejected_header.clone();
                        return Err(error);
                    }
                    if state.versions.iter().any(|row| {
                        row.value["Key"] == args["Key"]
                            && row.value["IsLatest"] == true
                            && row.value["delete_marker"] != true
                    }) {
                        return Err(provider("PreconditionFailed"));
                    }
                }
                let body = body.unwrap();
                assert_eq!(
                    args["ContentMD5"],
                    base64::engine::general_purpose::STANDARD.encode(Md5::digest(body))
                );
                let version = format!("registry-{}", self.state.borrow().versions.len());
                self.add(
                    args["Key"].as_str().unwrap(),
                    &version,
                    body,
                    false,
                    None,
                    args.clone(),
                );
                json!({"VersionId":version})
            }
            "create_multipart_upload" => {
                let upload = format!("upload-{}", self.count(method));
                self.state
                    .borrow_mut()
                    .uploads
                    .insert(upload.clone(), args.clone());
                json!({"UploadId":upload})
            }
            "upload_part_copy" => {
                let mut state = self.state.borrow_mut();
                let row = state
                    .uploads
                    .get_mut(args["UploadId"].as_str().unwrap())
                    .unwrap();
                row["CopySource"] = args["CopySource"].clone();
                json!({"CopyPartResult":{"ETag":format!("\"part-{}\"",args["PartNumber"])}})
            }
            "complete_multipart_upload" => {
                let upload = self
                    .state
                    .borrow_mut()
                    .uploads
                    .remove(args["UploadId"].as_str().unwrap())
                    .ok_or_else(|| provider("NoSuchUpload"))?;
                let source = {
                    let state = self.state.borrow();
                    state
                        .versions
                        .iter()
                        .find(|row| {
                            row.value["Key"] == upload["CopySource"]["Key"]
                                && row.value["VersionId"] == upload["CopySource"]["VersionId"]
                        })
                        .cloned()
                        .unwrap()
                };
                let mut properties = upload.clone();
                let tags = url::form_urlencoded::parse(
                    upload["Tagging"].as_str().unwrap_or("").as_bytes(),
                )
                .map(|(name, value)| json!({"Key":name,"Value":value}))
                .collect::<Vec<_>>();
                properties["TagSet"] = tags.into();
                properties["ETag"] = "\"multipart-etag\"".into();
                let version = format!("copied-{}", self.state.borrow().versions.len());
                self.add(
                    args["Key"].as_str().unwrap(),
                    &version,
                    &source.body,
                    false,
                    source.value["Size"].as_u64(),
                    properties,
                );
                json!({"VersionId":version,"ETag":"\"multipart-etag\""})
            }
            "abort_multipart_upload" => {
                if self
                    .state
                    .borrow_mut()
                    .uploads
                    .remove(args["UploadId"].as_str().unwrap())
                    .is_none()
                {
                    return Err(provider("NoSuchUpload"));
                }
                json!({})
            }
            _ => panic!("unsupported isolated fixture operation {method}"),
        };
        Ok(S3Response {
            value,
            body: Vec::new(),
        })
    }
}
impl Storage for Memory {
    fn call(
        &self,
        method: &str,
        args: &Value,
        body: Option<&[u8]>,
    ) -> std::result::Result<S3Response, S3Error> {
        assert_eq!(args["Bucket"], "fixture");
        if matches!(
            method,
            "copy_object"
                | "create_multipart_upload"
                | "upload_part_copy"
                | "complete_multipart_upload"
                | "abort_multipart_upload"
                | "delete_object"
                | "put_object"
                | "put_object_tagging"
        ) && let Some(path) = &self.state.borrow().require_fenced_journal
        {
            let private: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
            assert_eq!(
                private["schema_version"], 2,
                "private fence must precede {method}"
            );
        }
        {
            let mut state = self.state.borrow_mut();
            state.calls.push((method.to_owned(), args.clone()));
            let count = state.counts.entry(method.to_owned()).or_default();
            *count += 1;
        }
        if self.state.borrow().fail.get(method) == Some(&self.count(method)) {
            return Err(provider("TransportError"));
        }
        if let Some(code) = self.state.borrow().reject.get(method) {
            let mut error = provider(code);
            error.status = Some(404);
            return Err(error);
        }
        let result = self.response(method, args, body)?;
        if let Some((after, remote, reference)) = self
            .state
            .borrow()
            .remove_claim_after
            .as_ref()
            .filter(|(after, _, _)| after == method)
        {
            let _ = after;
            process::run(
                "git",
                ["--git-dir", remote, "update-ref", "-d", reference],
                Path::new("/"),
            )
            .unwrap();
        }
        if self.state.borrow().lose.get(method) == Some(&self.count(method)) {
            return Err(provider("TransportError"));
        }
        Ok(result)
    }
    fn bucket(&self) -> &str {
        "fixture"
    }
    fn prefix(&self) -> &str {
        "storage"
    }
    fn b2(&self) -> bool {
        self.b2
    }
}

#[test]
fn immutable_archive_reads_overlap_with_a_bounded_worker_count_and_keep_request_order() {
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    struct ConcurrentReads {
        memory: Mutex<Memory>,
        active: AtomicUsize,
        peak: AtomicUsize,
    }
    impl Storage for ConcurrentReads {
        fn call(
            &self,
            operation: &str,
            args: &Value,
            body: Option<&[u8]>,
        ) -> std::result::Result<S3Response, S3Error> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(3));
            let result = self.memory.lock().unwrap().call(operation, args, body);
            self.active.fetch_sub(1, Ordering::SeqCst);
            result
        }
        fn bucket(&self) -> &str {
            "fixture"
        }
        fn prefix(&self) -> &str {
            "storage"
        }
        fn b2(&self) -> bool {
            false
        }
    }
    let memory = Memory::new();
    let requests = (0..33)
        .map(|index| {
            let object = format!("storage/archive/object-{index}");
            let version = format!("version-{index}");
            memory.add(
                &object,
                &version,
                format!("{{\"receipt\":{index}}}").as_bytes(),
                false,
                None,
                Value::Null,
            );
            json!({"Bucket":"fixture","Key":object,"VersionId":version})
        })
        .collect::<Vec<_>>();
    let store = ConcurrentReads {
        memory: Mutex::new(memory),
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    };
    let heads = parallel_heads(&store, &requests).unwrap();
    for (request, head) in requests.iter().zip(heads) {
        assert_eq!(head["VersionId"], request["VersionId"]);
    }
    assert!((2..=READ_WORKERS).contains(&store.peak.load(Ordering::SeqCst)));
    store.peak.store(0, Ordering::SeqCst);
    let receipts = parallel_registries(&store, &requests, READ_WORKERS).unwrap();
    for (index, receipt) in receipts.into_iter().enumerate() {
        assert_eq!(receipt["receipt"], index);
    }
    assert!((2..=READ_WORKERS).contains(&store.peak.load(Ordering::SeqCst)));
    store.peak.store(0, Ordering::SeqCst);
    parallel_registries(&store, &requests, 2).unwrap();
    assert_eq!(store.peak.load(Ordering::SeqCst), 2);
    store
        .memory
        .lock()
        .unwrap()
        .state
        .borrow_mut()
        .fail
        .insert("head_object".to_owned(), 38);
    assert!(parallel_heads(&store, &requests).is_err());
    assert_eq!(store.active.load(Ordering::SeqCst), 0);
    assert_eq!(store.memory.lock().unwrap().count("head_object"), 66);
}

struct Fixture {
    _temp: tempfile::TempDir,
    repo: GitRepo,
    remote: String,
    store: Memory,
    payload: Value,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        fs::create_dir(&root).unwrap();
        let repo = GitRepo { root };
        repo.run(["init", "-q", "-b", "main"]).unwrap();
        let remote = temp.path().join("remote.git").to_str().unwrap().to_owned();
        repo.run(["init", "-q", "--bare", &remote]).unwrap();
        repo.run(["remote", "add", "origin", &remote]).unwrap();
        fs::write(
            repo.root.join(".workspace-mgr.toml"),
            "[git]\nremote='origin'\nbranch='main'\n",
        )
        .unwrap();
        let state = crate::archive_cancel::copy_journal(&repo, "task", "2026/07/task").unwrap();
        let payload = json!({"source":"task","destination":"2026/07/task","state_path":state,"repo_path":repo.root.canonicalize().unwrap()});
        Self {
            _temp: temp,
            repo,
            remote,
            store: Memory::new(),
            payload,
        }
    }
    fn run(&self, operation: &str) -> Result<Value> {
        execute_with(&self.store, &self.repo, operation, &self.payload)
    }
    fn reserve(&mut self) {
        let planned = self.run("plan").unwrap();
        self.payload["reservation"] =
            crate::archive_reservation::reserve(&self.repo, &planned).unwrap();
        self.payload["planned"] = planned;
    }
    fn journal(&self) -> Value {
        load_journal(Path::new(self.payload["state_path"].as_str().unwrap()))
            .unwrap()
            .unwrap()
    }
    fn claim_registry(&self, receipt: &Value) -> Value {
        crate::archive_registry::coordinate(&self.repo, receipt, true).unwrap()
    }
    fn registry(&self, operation: &str, receipt: &Value, proof: &Value) -> Result<Value> {
        registry_with(
            &self.store,
            &self.repo,
            operation,
            &json!({"receipt":receipt,"coordination":proof}),
        )
    }
}

#[test]
fn complete_history_preserves_null_versions_retired_keys_markers_and_flat_suffixes() {
    let mut fixture = Fixture::new();
    fixture.store.state.borrow_mut().page = 1;
    fixture
        .store
        .source("data//literal/./file", "null", b"first", false);
    fixture
        .store
        .source("data//literal/./file", "gone", b"", true);
    fixture
        .store
        .source("data//literal/./file", "latest", b"last", false);
    fixture.store.source("retired", "retired", b"", true);
    fixture.reserve();
    let receipt = fixture.run("copy").unwrap();
    assert_eq!(rows(&receipt).unwrap().len(), 4);
    assert_eq!(receipt["status"], "copied");
    assert_eq!(receipt["schema_version"], 1);
    assert_eq!(fixture.journal()["schema_version"], 2);
    assert_eq!(public_receipt(&fixture.journal(), None).unwrap(), receipt);
    assert_eq!(
        crate::archive_reservation::normalized_receipt(&fixture.journal()).unwrap()["schema_version"],
        1
    );
    assert_eq!(fixture.store.versions("storage/task/").len(), 4);
    assert_eq!(fixture.store.versions("storage/2026/07/task/").len(), 4);
    fixture.run("verify").unwrap();
    let preview = fixture.run("cancel-preview").unwrap();
    assert_eq!(preview["delete_versions"].as_array().unwrap().len(), 4);
    fixture.run("cancel").unwrap();
    assert!(fixture.store.versions("storage/2026/07/task/").is_empty());
    assert_eq!(fixture.store.versions("storage/task/").len(), 4);
}

#[test]
fn legacy_copy_retry_and_cancel_fence_0_6_without_changing_the_public_receipt() {
    let mut fixture = Fixture::new();
    fixture
        .store
        .source("data", "payload-version", b"payload", false);
    fixture.store.source("data", "marker-version", b"", true);
    fixture.reserve();
    let receipt = fixture.run("copy").unwrap();
    let mut legacy = fixture.journal();
    legacy["schema_version"] = 1.into();
    let state = Path::new(fixture.payload["state_path"].as_str().unwrap());
    fs::write(state, canonical(&legacy).unwrap()).unwrap();
    let before = fs::read(state).unwrap();
    fixture.run("cancel-preview").unwrap();
    assert_eq!(fs::read(state).unwrap(), before);
    let copies = fixture.store.count("copy_object");
    assert_eq!(fixture.run("copy").unwrap(), receipt);
    assert_eq!(fixture.store.count("copy_object"), copies);
    let private = fixture.journal();
    assert_eq!(private["schema_version"], 2);
    // Released 0.6.0 assets/dvc_version_archive.py validates the complete
    // context, including schema_version == 1, before resuming copy.
    let mut old_context = receipt_context(&private);
    old_context["schema_version"] = private["schema_version"].clone();
    assert_ne!(
        old_context,
        context(&fixture.store, &fixture.payload).unwrap()
    );
    let proof = fixture.claim_registry(&receipt);
    fixture.registry("publish", &receipt, &proof).unwrap();
    fixture.registry("cancel", &receipt, &proof).unwrap();
    fixture.run("cancel").unwrap();
    assert!(fixture.store.versions("storage/2026/07/task/").is_empty());
    assert_eq!(fixture.store.versions("storage/task/").len(), 2);
    assert_eq!(fixture.journal()["schema_version"], 2);
}

#[test]
fn registry_withdrawal_fences_a_legacy_journal_before_deleting_exact_versions() {
    let mut fixture = Fixture::new();
    fixture.store.source("data", "source", b"payload", false);
    fixture.reserve();
    let receipt = fixture.run("copy").unwrap();
    let proof = fixture.claim_registry(&receipt);
    fixture.registry("publish", &receipt, &proof).unwrap();
    let mut legacy = fixture.journal();
    legacy["schema_version"] = 1.into();
    let state = Path::new(fixture.payload["state_path"].as_str().unwrap());
    fs::write(state, canonical(&legacy).unwrap()).unwrap();
    let preview_before = fs::read(state).unwrap();
    fixture
        .registry("cancel-preview", &receipt, &proof)
        .unwrap();
    assert_eq!(fs::read(state).unwrap(), preview_before);
    fixture.store.state.borrow_mut().require_fenced_journal = Some(state.to_owned());
    fixture.registry("cancel", &receipt, &proof).unwrap();
    assert_eq!(fixture.journal()["schema_version"], 2);
    assert!(
        registry_read_with(&fixture.store, "task")
            .unwrap()
            .is_none()
    );
    assert_eq!(fixture.store.versions("storage/task/").len(), 1);
}

#[test]
fn partial_legacy_copy_cancel_is_fenced_before_payload_and_marker_deletion() {
    let mut fixture = Fixture::new();
    fixture.store.source("data", "payload", b"payload", false);
    fixture.store.source("data", "marker", b"", true);
    fixture.reserve();
    fixture
        .store
        .state
        .borrow_mut()
        .fail
        .insert("delete_object".into(), 1);
    assert!(fixture.run("copy").is_err());
    let mut journal = fixture.journal();
    assert_eq!(journal["status"], "copying");
    journal["schema_version"] = 1.into();
    let state = Path::new(fixture.payload["state_path"].as_str().unwrap());
    fs::write(state, canonical(&journal).unwrap()).unwrap();
    fixture.store.state.borrow_mut().require_fenced_journal = Some(state.to_owned());
    fixture.run("cancel").unwrap();
    assert_eq!(fixture.journal()["schema_version"], 2);
    assert_eq!(fixture.journal()["status"], "cancelled");
    assert!(fixture.store.versions("storage/2026/07/task/").is_empty());
    assert_eq!(fixture.store.versions("storage/task/").len(), 2);
}

#[test]
fn future_private_copy_journal_fails_before_remote_copy_or_delete() {
    let mut fixture = Fixture::new();
    fixture.store.source("data", "source", b"payload", false);
    fixture.reserve();
    fixture.run("copy").unwrap();
    let mut journal = fixture.journal();
    journal["schema_version"] = 3.into();
    let state = Path::new(fixture.payload["state_path"].as_str().unwrap());
    fs::write(state, canonical(&journal).unwrap()).unwrap();
    let before = fs::read(state).unwrap();
    let copies = fixture.store.count("copy_object");
    let deletes = fixture.store.count("delete_object");
    for operation in ["copy", "cancel", "cancel-preview"] {
        assert!(
            fixture
                .run(operation)
                .unwrap_err()
                .to_string()
                .contains("unsupported schema")
        );
    }
    assert_eq!(fixture.store.count("copy_object"), copies);
    assert_eq!(fixture.store.count("delete_object"), deletes);
    assert_eq!(fs::read(state).unwrap(), before);
}

#[test]
fn copy_keeps_metadata_tags_and_all_supported_properties() {
    let mut fixture = Fixture::new();
    let mut properties =
        json!({"Metadata":{"original":"kept"},"TagSet":[{"Key":"a b","Value":"x&y"}]});
    for field in COPY_HEADERS {
        properties[field] = format!("fixture-{field}").into();
    }
    fixture.store.add(
        "storage/task/data",
        "source",
        b"payload",
        false,
        None,
        properties.clone(),
    );
    fixture.reserve();
    let receipt = fixture.run("copy").unwrap();
    let copied = fixture
        .store
        .versions("storage/2026/07/task/")
        .pop()
        .unwrap();
    for field in COPY_HEADERS {
        assert_eq!(copied.value[field], properties[field]);
    }
    assert_eq!(copied.value["Metadata"]["original"], "kept");
    assert_eq!(copied.value["TagSet"], properties["TagSet"]);
    assert_eq!(
        receipt["versions"][0]["destination_etag"],
        etag(&copied.value["ETag"]).unwrap()
    );
}

#[test]
fn reservation_requires_nonce_private_ownership_and_fresh_remote_claim() {
    let mut fixture = Fixture::new();
    fixture.store.source("data", "source", b"payload", false);
    fixture.reserve();
    fixture.payload["reservation"]["attempt_nonce"] = "another-attempt".into();
    assert!(fixture.run("copy").is_err());
    assert_eq!(fixture.store.count("copy_object"), 0);
    fixture.payload["reservation"] =
        crate::archive_reservation::reserve(&fixture.repo, &fixture.payload["planned"]).unwrap();
    fixture
        .repo
        .run([
            "--git-dir",
            &fixture.remote,
            "update-ref",
            "-d",
            fixture.payload["reservation"]["ref"].as_str().unwrap(),
        ])
        .unwrap();
    assert!(
        fixture
            .run("copy")
            .unwrap_err()
            .to_string()
            .contains("removed or replaced")
    );
    assert_eq!(fixture.store.count("copy_object"), 0);
}

#[test]
fn claim_loss_after_last_copy_blocks_completion() {
    let mut fixture = Fixture::new();
    fixture.store.source("data", "source", b"payload", false);
    fixture.reserve();
    fixture.store.state.borrow_mut().remove_claim_after = Some((
        "copy_object".to_owned(),
        fixture.remote.clone(),
        fixture.payload["reservation"]["ref"]
            .as_str()
            .unwrap()
            .to_owned(),
    ));
    assert!(
        fixture
            .run("copy")
            .unwrap_err()
            .to_string()
            .contains("removed or replaced")
    );
    assert_eq!(fixture.journal()["status"], "copying");
    assert_eq!(fixture.store.count("copy_object"), 1);
}

#[test]
fn source_change_after_plan_and_foreign_multipart_before_copy_fail_before_writes() {
    let mut fixture = Fixture::new();
    fixture.store.source("data", "source", b"payload", false);
    fixture.reserve();
    fixture.store.source("data", "next", b"next", false);
    assert!(
        fixture
            .run("copy")
            .unwrap_err()
            .to_string()
            .contains("changed after planning")
    );
    assert_eq!(fixture.store.count("copy_object"), 0);
    let mut fixture = Fixture::new();
    fixture.store.source("data", "source", b"payload", false);
    fixture.reserve();
    fixture.store.state.borrow_mut().uploads.insert(
        "foreign".to_owned(),
        json!({"Key":"storage/2026/07/task/data"}),
    );
    assert!(
        fixture
            .run("copy")
            .unwrap_err()
            .to_string()
            .contains("multipart uploads")
    );
    assert!(fixture.run("plan").is_err());
    assert_eq!(fixture.store.count("copy_object"), 0);
}

#[test]
fn lost_copy_response_is_recovered_without_another_copy() {
    let mut fixture = Fixture::new();
    fixture.store.source("data", "source", b"payload", false);
    fixture.reserve();
    fixture
        .store
        .state
        .borrow_mut()
        .lose
        .insert("copy_object".to_owned(), 1);
    assert!(fixture.run("copy").is_err());
    assert_eq!(fixture.run("copy").unwrap()["status"], "copied");
    assert_eq!(fixture.store.count("copy_object"), 1);
}

#[test]
fn cancel_inventory_reads_each_exact_version_once_and_keeps_same_key_ownership_separate() {
    let store = Memory::new();
    let context = json!({"source":"task","destination":"2026/07/task"});
    let mut journal = json!({"transaction_id":"attempt","versions":[]});
    for (source_version, destination, body) in [
        ("old", "2026/07/task/data", b"a".as_slice()),
        ("middle", "2026/07/task/data", b"bb".as_slice()),
        ("latest", "2026/07/task/data", b"ccc".as_slice()),
        ("different-key", "2026/07/task/other", b"dddd".as_slice()),
    ] {
        let row = json!({"source_object":"task/data","source_version_id":source_version,
            "destination_object":destination,"destination_version_id":null,"destination_etag":null,
            "delete_marker":false,"size":body.len(),"started":true});
        let ownership = token(&journal, &row).unwrap();
        // The latest data and other key deliberately share a VersionId.
        let copied_version = if source_version == "different-key" {
            "latest"
        } else {
            source_version
        };
        store.add(
            &format!("storage/{destination}"),
            copied_version,
            body,
            false,
            None,
            json!({"Metadata":{metadata_key(&journal).unwrap():ownership}}),
        );
        journal["versions"].as_array_mut().unwrap().push(row);
    }
    store.add(
        "storage/2026/07/task/data",
        "foreign",
        b"new unrelated payload",
        false,
        None,
        Value::Null,
    );
    store.add(
        "storage/2026/07/task/data",
        "foreign-marker",
        b"",
        true,
        None,
        Value::Null,
    );
    let (owned, unrelated) = cancel_inventory(&store, &context, &mut journal, false).unwrap();
    assert_eq!(owned.len(), 4);
    assert_eq!(unrelated.len(), 2);
    assert_eq!(store.count("head_object"), 5);
    for version in &owned {
        let row = &journal["versions"][version.row];
        let record = &row["cancel_owned_versions"][version.record];
        assert_eq!(record["version_id"], version.version);
        assert_eq!(
            key(&store, text(row, "destination_object").unwrap()),
            version.key
        );
    }
    let before = store.count("head_object");
    cancel_inventory(&store, &context, &mut journal, false).unwrap();
    assert_eq!(store.count("head_object"), before + 5);
    store.add(
        "storage/2026/07/task/data",
        "null",
        b"mutable legacy generation",
        false,
        None,
        Value::Null,
    );
    let before = store.count("head_object");
    let (_, unrelated) = cancel_inventory(&store, &context, &mut journal, false).unwrap();
    assert_eq!(unrelated.len(), 3);
    assert_eq!(
        store.count("head_object"),
        before + 8,
        "null versions are rechecked for each of the three source rows"
    );
    let mut mismatched = journal.clone();
    mismatched["versions"][1]["size"] = 99.into();
    assert!(
        cancel_inventory(&store, &context, &mut mismatched, false)
            .err()
            .unwrap()
            .to_string()
            .contains("payload version is missing or mismatched")
    );
    store.state.borrow_mut().versions[0].value["Metadata"] = json!({});
    assert!(
        cancel_inventory(&store, &context, &mut journal, false)
            .err()
            .unwrap()
            .to_string()
            .contains("cannot verify copied version ownership")
    );
    assert_eq!(store.count("delete_object"), 0);
}

#[test]
fn sdk_duplicates_and_delete_response_loss_cancel_all_owned_ids_only() {
    let mut fixture = Fixture::new();
    fixture.store.source("data", "source", b"payload", false);
    fixture.reserve();
    let receipt = fixture.run("copy").unwrap();
    let copied = fixture
        .store
        .versions("storage/2026/07/task/")
        .pop()
        .unwrap();
    fixture.store.add(
        "storage/2026/07/task/data",
        "sdk-retry-1",
        &copied.body,
        false,
        None,
        json!({"Metadata":copied.value["Metadata"]}),
    );
    fixture.store.add(
        "storage/2026/07/task/data",
        "sdk-retry-2",
        &copied.body,
        false,
        None,
        json!({"Metadata":copied.value["Metadata"]}),
    );
    fixture.store.add(
        "storage/2026/07/task/data",
        "foreign",
        b"independent",
        false,
        None,
        Value::Null,
    );
    let snapshot = fs::read(fixture.payload["state_path"].as_str().unwrap()).unwrap();
    assert_eq!(
        fixture.run("cancel-preview").unwrap()["delete_versions"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        snapshot,
        fs::read(fixture.payload["state_path"].as_str().unwrap()).unwrap()
    );
    fixture
        .store
        .state
        .borrow_mut()
        .lose
        .insert("delete_object".to_owned(), 2);
    assert!(fixture.run("cancel").is_err());
    let cancelled = fixture.run("cancel").unwrap();
    assert_eq!(cancelled["status"], "cancelled_with_unrelated_history");
    assert_eq!(fixture.store.count("delete_object"), 3);
    assert_eq!(fixture.store.versions("storage/2026/07/task/").len(), 1);
    assert_eq!(
        fixture.store.versions("storage/2026/07/task/")[0].value["VersionId"],
        "foreign"
    );
    assert_eq!(
        public_receipt(&fixture.journal(), Some("copied")).unwrap(),
        receipt
    );
}

#[test]
fn pending_duplicate_payloads_cancel_without_rewriting_public_mapping() {
    let mut fixture = Fixture::new();
    fixture.store.source("data", "source", b"payload", false);
    fixture.reserve();
    fixture
        .store
        .state
        .borrow_mut()
        .lose
        .insert("copy_object".to_owned(), 1);
    assert!(fixture.run("copy").is_err());
    let copied = fixture
        .store
        .versions("storage/2026/07/task/")
        .pop()
        .unwrap();
    fixture.store.add(
        "storage/2026/07/task/data",
        "sdk-retry",
        &copied.body,
        false,
        None,
        json!({"Metadata":copied.value["Metadata"]}),
    );
    assert!(fixture.run("copy").is_err());
    assert_eq!(
        fixture.run("cancel").unwrap()["deleted_versions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(fixture.journal()["versions"][0]["destination_version_id"].is_null());
    assert!(fixture.store.versions("storage/2026/07/task/").is_empty());
}

#[test]
fn cancellation_requires_unchanged_originals_but_preserves_new_source_work() {
    let mut fixture = Fixture::new();
    fixture.store.source("data", "source", b"payload", false);
    fixture.reserve();
    fixture.run("copy").unwrap();
    fixture.store.source("data", "new-source", b"new", false);
    fixture.run("cancel").unwrap();
    assert_eq!(fixture.store.versions("storage/task/").len(), 2);
    let mut fixture = Fixture::new();
    fixture.store.source("data", "source", b"payload", false);
    fixture.reserve();
    fixture.run("copy").unwrap();
    fixture
        .store
        .state
        .borrow_mut()
        .versions
        .retain(|row| row.value["VersionId"] != "source");
    assert!(
        fixture
            .run("cancel")
            .unwrap_err()
            .to_string()
            .contains("every unchanged original")
    );
    assert_eq!(fixture.store.count("delete_object"), 0);
}

#[test]
fn terminal_cancel_resumes_after_source_retirement_and_preserves_new_owner_history() {
    let mut fixture = Fixture::new();
    fixture.store.source("data", "source", b"payload", false);
    fixture.reserve();
    fixture.run("copy").unwrap();
    fixture.run("cancel").unwrap();
    fixture.store.state.borrow_mut().versions.clear();
    fixture.store.add(
        "storage/2026/07/task/data",
        "foreign",
        b"new owner",
        false,
        None,
        Value::Null,
    );
    fixture.store.state.borrow_mut().uploads.insert(
        "foreign".to_owned(),
        json!({"Key":"storage/2026/07/task/data"}),
    );
    let before = fixture.store.count("head_object");
    let journal = fs::read(fixture.payload["state_path"].as_str().unwrap()).unwrap();
    let result = fixture.run("cancel").unwrap();
    assert_eq!(result["already_cancelled"], true);
    assert_eq!(result["status"], "cancelled");
    assert_eq!(fixture.store.count("head_object"), before + 1);
    assert_eq!(
        journal,
        fs::read(fixture.payload["state_path"].as_str().unwrap()).unwrap()
    );
    assert_eq!(fixture.store.count("delete_object"), 1);
}

#[test]
fn terminal_legacy_cancel_upgrades_only_on_apply_without_source_access() {
    let mut fixture = Fixture::new();
    fixture.store.source("data", "source", b"payload", false);
    fixture.reserve();
    fixture.run("copy").unwrap();
    fixture.run("cancel").unwrap();
    let mut journal = fixture.journal();
    journal["schema_version"] = 1.into();
    let state = Path::new(fixture.payload["state_path"].as_str().unwrap());
    fs::write(state, canonical(&journal).unwrap()).unwrap();
    fixture.store.state.borrow_mut().versions.clear();
    let before = fs::read(state).unwrap();
    let heads = fixture.store.count("head_object");
    let deletes = fixture.store.count("delete_object");
    fixture.run("cancel-preview").unwrap();
    assert_eq!(fs::read(state).unwrap(), before);
    fixture.run("cancel").unwrap();
    assert_eq!(fixture.journal()["schema_version"], 2);
    assert_eq!(fixture.store.count("head_object"), heads);
    assert_eq!(fixture.store.count("delete_object"), deletes);
}

#[test]
fn unrecorded_markers_never_become_owned_during_cancel() {
    let mut fixture = Fixture::new();
    fixture.store.source("data", "source", b"", true);
    fixture.reserve();
    fixture
        .store
        .state
        .borrow_mut()
        .lose
        .insert("delete_object".to_owned(), 1);
    assert!(fixture.run("copy").is_err());
    assert!(
        fixture
            .run("cancel")
            .unwrap_err()
            .to_string()
            .contains("unrecorded delete marker")
    );
    assert_eq!(fixture.store.count("delete_object"), 1);
}

#[test]
fn multipart_preserves_properties_tags_and_recovers_lost_completion() {
    let mut fixture = Fixture::new();
    fixture.store.add("storage/task/large","source",b"large",false,Some(COPY_LIMIT+1),json!({"Metadata":{"original":"kept"},"ContentType":"application/test","TagSet":[{"Key":"a b","Value":"x&y"}]}));
    fixture.reserve();
    fixture
        .store
        .state
        .borrow_mut()
        .lose
        .insert("complete_multipart_upload".to_owned(), 1);
    assert!(fixture.run("copy").is_err());
    let receipt = fixture.run("copy").unwrap();
    assert_eq!(fixture.store.count("complete_multipart_upload"), 1);
    assert_eq!(receipt["versions"][0]["destination_etag"], "multipart-etag");
    let copied = fixture
        .store
        .versions("storage/2026/07/task/")
        .pop()
        .unwrap();
    assert_eq!(copied.value["ContentType"], "application/test");
    assert_eq!(copied.value["Metadata"]["original"], "kept");
    assert_eq!(copied.value["TagSet"], json!([{"Key":"a b","Value":"x&y"}]));
}

#[test]
fn registry_publish_and_cancel_preserve_explicit_historical_record_metadata() {
    let mut fixture = Fixture::new();
    fixture.store.source("data", "source", b"payload", false);
    fixture.store.source("data", "marker", b"", true);
    let mut planned = fixture.run("plan").unwrap();
    planned["task_id"] = "task".into();
    planned["closed_pull_request"] = Value::Null;
    planned["historical_records"] = json!([{
        "path":"task/logs/history.log", "sha256":"reviewed-original-record",
        "unix_mode":33060, "role":"historical-record"
    }]);
    fixture.payload["reservation"] =
        crate::archive_reservation::reserve(&fixture.repo, &planned).unwrap();
    fixture.payload["planned"] = planned.clone();
    let mut receipt = fixture.run("copy").unwrap();
    for key in crate::archive_migration::RECEIPT_METADATA_FIELDS {
        if let Some(value) = planned.get(key) {
            receipt[key] = value.clone();
        }
    }
    let proof = fixture.claim_registry(&receipt);
    assert_eq!(
        fixture.registry("publish", &receipt, &proof).unwrap()["status"],
        "published"
    );
    let stored = registry_read_with(&fixture.store, "task").unwrap().unwrap();
    assert_eq!(stored["historical_records"], receipt["historical_records"]);
    assert_eq!(stored.get("closed_pull_request"), Some(&Value::Null));
    assert_eq!(
        fixture.registry("publish", &receipt, &proof).unwrap()["status"],
        "unchanged"
    );
    fixture.registry("cancel", &receipt, &proof).unwrap();
    fixture.run("cancel").unwrap();
    assert!(fixture.store.versions("storage/2026/07/task/").is_empty());
    assert_eq!(fixture.store.versions("storage/task/").len(), 2);
}

#[test]
fn b2_publish_keeps_condition_first_and_falls_back_only_with_live_git_binding() {
    let mut fixture = Fixture::new();
    fixture.store.b2 = true;
    fixture.store.source("data", "source", b"payload", false);
    fixture.reserve();
    let receipt = fixture.run("copy").unwrap();
    let proof = fixture.claim_registry(&receipt);
    fixture.store.state.borrow_mut().conditional_error = Some("NotImplemented".to_owned());
    fixture.store.state.borrow_mut().rejected_header = Some("If-None-Match".to_owned());
    assert_eq!(
        fixture.registry("publish", &receipt, &proof).unwrap()["status"],
        "published"
    );
    let writes = fixture
        .store
        .state
        .borrow()
        .calls
        .iter()
        .filter(|(method, _)| method == "put_object")
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0].1["IfNoneMatch"], "*");
    assert!(writes[1].1["IfNoneMatch"].is_null());
    assert_eq!(
        fixture.registry("publish", &receipt, &proof).unwrap()["status"],
        "unchanged"
    );
    assert_eq!(fixture.store.count("put_object"), 2);
}

#[test]
fn generic_provider_and_other_b2_header_never_allow_unconditional_publication() {
    for b2 in [false, true] {
        let mut fixture = Fixture::new();
        fixture.store.b2 = b2;
        fixture.store.source("data", "source", b"payload", false);
        fixture.reserve();
        let receipt = fixture.run("copy").unwrap();
        let proof = fixture.claim_registry(&receipt);
        fixture.store.state.borrow_mut().conditional_error = Some("NotImplemented".to_owned());
        fixture.store.state.borrow_mut().rejected_header = Some("x-amz-trailer".to_owned());
        assert!(fixture.registry("publish", &receipt, &proof).is_err());
        assert_eq!(fixture.store.count("put_object"), 1);
    }
}

#[test]
fn registry_lost_put_and_delete_responses_keep_idempotence_and_full_history() {
    let mut fixture = Fixture::new();
    fixture.store.source("data", "source", b"payload", false);
    fixture.reserve();
    let receipt = fixture.run("copy").unwrap();
    let proof = fixture.claim_registry(&receipt);
    fixture
        .store
        .state
        .borrow_mut()
        .lose
        .insert("put_object".to_owned(), 1);
    assert_eq!(
        fixture.registry("publish", &receipt, &proof).unwrap()["status"],
        "unchanged"
    );
    let object = registry_key(&fixture.store, "task").unwrap();
    fixture.store.add(
        &object,
        "same-receipt-second-version",
        &canonical(&receipt).unwrap(),
        false,
        None,
        Value::Null,
    );
    fixture.store.state.borrow_mut().page = 1;
    assert_eq!(
        registry_lookup_with(&fixture.store, "storage/task/data", "source")
            .unwrap()
            .unwrap()["destination_key"],
        "storage/2026/07/task/data"
    );
    fixture
        .store
        .state
        .borrow_mut()
        .lose
        .insert("delete_object".to_owned(), 1);
    let cancelled = fixture.registry("cancel", &receipt, &proof).unwrap();
    assert_eq!(cancelled["already_absent"].as_array().unwrap().len(), 1);
    assert_eq!(fixture.store.versions(&object).len(), 0);
}

#[test]
fn hidden_registry_markers_and_conflicting_older_receipts_fail_closed() {
    let mut fixture = Fixture::new();
    fixture.store.source("data", "source", b"payload", false);
    fixture.reserve();
    let receipt = fixture.run("copy").unwrap();
    let proof = fixture.claim_registry(&receipt);
    fixture.registry("publish", &receipt, &proof).unwrap();
    let object = registry_key(&fixture.store, "task").unwrap();
    let mut conflicting = receipt.clone();
    conflicting["transaction_id"] = "another".into();
    fixture.store.add(
        &object,
        "conflicting",
        &canonical(&conflicting).unwrap(),
        false,
        None,
        Value::Null,
    );
    assert!(
        registry_read_with(&fixture.store, "task")
            .unwrap_err()
            .to_string()
            .contains("conflicting")
    );
    assert!(fixture.registry("cancel", &receipt, &proof).is_err());
    assert_eq!(fixture.store.count("delete_object"), 0);
    fixture
        .store
        .state
        .borrow_mut()
        .versions
        .retain(|row| row.value["VersionId"] != "conflicting");
    fixture
        .store
        .add(&object, "hidden", b"", true, None, Value::Null);
    assert!(
        registry_read_with(&fixture.store, "task")
            .unwrap_err()
            .to_string()
            .contains("delete marker")
    );
}

#[test]
fn registry_claim_loss_refuses_publication_and_withdrawal_before_any_mutation() {
    let mut fixture = Fixture::new();
    fixture.store.source("data", "source", b"payload", false);
    fixture.reserve();
    let receipt = fixture.run("copy").unwrap();
    let proof = fixture.claim_registry(&receipt);
    fixture
        .repo
        .run([
            "--git-dir",
            &fixture.remote,
            "update-ref",
            "-d",
            proof["ref"].as_str().unwrap(),
        ])
        .unwrap();
    assert!(fixture.registry("publish", &receipt, &proof).is_err());
    assert!(fixture.registry("cancel", &receipt, &proof).is_err());
    assert_eq!(fixture.store.count("put_object"), 0);
    assert_eq!(fixture.store.count("delete_object"), 0);
}

#[test]
fn suspended_bucket_cannot_begin_archival() {
    let fixture = Fixture::new();
    require_versioning(&fixture.store).unwrap();
    fixture.store.state.borrow_mut().versioning = Some("Suspended".to_owned());
    assert!(
        require_versioning(&fixture.store)
            .unwrap_err()
            .to_string()
            .contains("enabled versioned bucket")
    );
    assert_eq!(fixture.store.count("copy_object"), 0);
}

#[test]
fn cancellation_recovers_lost_abort_response_and_retains_unjournaled_uploads() {
    let mut fixture = Fixture::new();
    fixture.store.source("data", "source", b"payload", false);
    fixture.reserve();
    let mut journal = fixture.payload["planned"].clone();
    journal["status"] = "copying".into();
    journal["transaction_id"] = "fixture-attempt".into();
    journal["versions"][0]["started"] = true.into();
    journal["versions"][0]["multipart_upload_id"] = "owned".into();
    save_journal(
        Path::new(fixture.payload["state_path"].as_str().unwrap()),
        &journal,
    )
    .unwrap();
    fixture.store.state.borrow_mut().uploads.insert(
        "owned".to_owned(),
        json!({"Key":"storage/2026/07/task/data"}),
    );
    fixture.store.state.borrow_mut().uploads.insert(
        "foreign".to_owned(),
        json!({"Key":"storage/2026/07/task/other"}),
    );
    fixture
        .store
        .state
        .borrow_mut()
        .lose
        .insert("abort_multipart_upload".to_owned(), 1);
    assert!(fixture.run("cancel").is_err());
    let result = fixture.run("cancel").unwrap();
    assert_eq!(result["status"], "cancelled_with_unrelated_history");
    assert_eq!(
        result["retained_uploads"],
        json!([{"key":"storage/2026/07/task/other","upload_id":"foreign"}])
    );
    assert_eq!(fixture.store.state.borrow().uploads.len(), 1);
    assert!(fixture.journal()["versions"][0]["multipart_upload_id"].is_null());
}

#[test]
fn cancelled_attempt_refuses_late_token_owned_sdk_generation_without_source_reads() {
    let mut fixture = Fixture::new();
    fixture.store.source("data", "source", b"payload", false);
    fixture.reserve();
    fixture.run("copy").unwrap();
    let copied = fixture
        .store
        .versions("storage/2026/07/task/")
        .pop()
        .unwrap();
    fixture.run("cancel").unwrap();
    fixture.store.state.borrow_mut().versions.clear();
    fixture.store.add(
        "storage/2026/07/task/data",
        "late-sdk-retry",
        &copied.body,
        false,
        None,
        json!({"Metadata":copied.value["Metadata"]}),
    );
    let before = fixture.store.state.borrow().calls.len();
    let journal = fs::read(fixture.payload["state_path"].as_str().unwrap()).unwrap();
    assert!(
        fixture
            .run("cancel")
            .unwrap_err()
            .to_string()
            .contains("unexpectedly contains owned")
    );
    assert_eq!(fixture.store.count("delete_object"), 1);
    assert_eq!(
        journal,
        fs::read(fixture.payload["state_path"].as_str().unwrap()).unwrap()
    );
    assert!(
        fixture.store.state.borrow().calls[before..]
            .iter()
            .all(|(_, request)| !request["Key"]
                .as_str()
                .unwrap_or("")
                .starts_with("storage/task/")
                && !request["Prefix"]
                    .as_str()
                    .unwrap_or("")
                    .starts_with("storage/task/"))
    );
}

#[test]
fn old_python_copy_tokens_keep_ascii_escapes_and_utf16_surrogate_pairs() {
    let journal = json!({"transaction_id":"d8345e27-699d-4b8f-b2f5-b7d96367c031"});
    for (object, expected) in [
        (
            "task/研究🧬.bin",
            "6a0e254a258a170470c6838f3132bd8051e9d094b078f3c7a9a92f1429400c5d",
        ),
        (
            "task/control\u{7f}/é𝄞.bin",
            "6d2409371db4c9f3a2589a4b2dd11cdc7d9fb5954e17a83381bf643ab1456bca",
        ),
        (
            "task/quote\"\\\n.bin",
            "44e7d31547080454246a8d6bcea14fcec2c9858e86ee1b0c2769e0d608e62624",
        ),
    ] {
        let row = json!({"source_object":object,"source_version_id":"source-version"});
        assert_eq!(token(&journal, &row).unwrap(), expected);
    }
}

#[test]
fn old_unicode_journal_recovers_lost_copy_response_and_cancels_exact_version() {
    let mut fixture = Fixture::new();
    fixture
        .store
        .source("研究🧬.bin", "source-version", b"payload", false);
    fixture.reserve();
    let mut journal = fixture.payload["planned"].clone();
    journal["status"] = "copying".into();
    journal["transaction_id"] = "d8345e27-699d-4b8f-b2f5-b7d96367c031".into();
    journal["versions"][0]["started"] = true.into();
    save_journal(
        Path::new(fixture.payload["state_path"].as_str().unwrap()),
        &journal,
    )
    .unwrap();
    // Literal metadata produced by the old Python adapter, independent of
    // the Rust token implementation, after a lost CopyObject response.
    fixture.store.add(
        "storage/2026/07/task/研究🧬.bin",
        "old-python-copy",
        b"payload",
        false,
        None,
        json!({"Metadata":{"workspace-mgr-archive-copy-d8345e27-699d-4b8f-b2f5-b7d96367c031":"6a0e254a258a170470c6838f3132bd8051e9d094b078f3c7a9a92f1429400c5d"}}),
    );
    let copied = fixture.run("copy").unwrap();
    assert_eq!(copied["status"], "copied");
    assert_eq!(
        copied["versions"][0]["destination_version_id"],
        "old-python-copy"
    );
    assert_eq!(copied["versions"][0]["source_object"], "task/研究🧬.bin");
    assert_eq!(fixture.store.count("copy_object"), 0);
    assert_eq!(
        fixture.run("cancel-preview").unwrap()["delete_versions"][0]["version_id"],
        "old-python-copy"
    );
    let cancelled = fixture.run("cancel").unwrap();
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(fixture.store.count("delete_object"), 1);
    assert!(fixture.store.versions("storage/2026/07/task/").is_empty());
    assert_eq!(fixture.store.versions("storage/task/").len(), 1);
    assert_eq!(
        public_receipt(&fixture.journal(), Some("copied")).unwrap(),
        copied
    );
    assert_eq!(fixture.run("cancel").unwrap()["already_cancelled"], true);
    assert_eq!(fixture.store.count("delete_object"), 1);
}

#[test]
fn old_python_timestamp_and_journal_bindings_remain_byte_compatible_with_cas() {
    let mut fixture = Fixture::new();
    fixture.store.add(
        "storage/task/data",
        "source",
        b"payload",
        false,
        None,
        json!({"LastModified":"2026-07-02T03:04:05.123Z"}),
    );
    fixture.reserve();
    assert_eq!(
        fixture.payload["planned"]["versions"][0]["source_last_modified"],
        "2026-07-02T03:04:05.123000+00:00"
    );
    let receipt = fixture.run("copy").unwrap();
    let proof = fixture.claim_registry(&receipt);
    let original = canonical(&receipt).unwrap();
    fixture.registry("publish", &receipt, &proof).unwrap();
    let stored = registry_read_with(&fixture.store, "task").unwrap().unwrap();
    assert_eq!(canonical(&stored).unwrap(), original);
    assert_eq!(
        stored["versions"][0]["source_last_modified"],
        "2026-07-02T03:04:05.123000+00:00"
    );
}

#[test]
fn exact_published_receipt_can_publish_but_cannot_cancel_another_attempt() {
    let mut fixture = Fixture::new();
    fixture.store.source("data", "source", b"payload", false);
    fixture.reserve();
    let receipt = fixture.run("copy").unwrap();
    let mut proof = fixture.claim_registry(&receipt);
    fixture
        .repo
        .run(["config", "user.name", "Native archive fixture"])
        .unwrap();
    fixture
        .repo
        .run(["config", "user.email", "fixture@example.invalid"])
        .unwrap();
    let path = fixture
        .repo
        .root
        .join("2026/07/task/.workspace-mgr-archive.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, canonical(&receipt).unwrap()).unwrap();
    fs::write(
        fixture.repo.root.join("receipt-copy.json"),
        canonical(&receipt).unwrap(),
    )
    .unwrap();
    fixture
        .repo
        .run([
            "add",
            "2026/07/task/.workspace-mgr-archive.json",
            "receipt-copy.json",
        ])
        .unwrap();
    fixture
        .repo
        .run(["commit", "-q", "-m", "Record exact published receipt"])
        .unwrap();
    object_mut(&mut proof).unwrap().remove("state_path");
    proof["publication_oid"] = fixture
        .repo
        .run(["rev-parse", "HEAD"])
        .unwrap()
        .stdout
        .trim()
        .into();
    proof["receipt_path"] = "2026/07/task/.workspace-mgr-archive.json".into();
    let unmerged = fixture.registry("publish", &receipt, &proof).unwrap_err();
    assert!(unmerged.to_string().contains("configured shared branch"));
    assert_eq!(fixture.store.count("put_object"), 0);
    fixture.repo.run(["push", "origin", "main"]).unwrap();
    assert_eq!(
        fixture.registry("publish", &receipt, &proof).unwrap()["status"],
        "published"
    );
    let mut pasted = proof.clone();
    pasted["receipt_path"] = "receipt-copy.json".into();
    assert!(
        fixture
            .registry("publish", &receipt, &pasted)
            .unwrap_err()
            .to_string()
            .contains("canonical archived receipt path")
    );
    let mut wrong_branch = proof.clone();
    wrong_branch["base_branch"] = "another".into();
    assert!(
        fixture
            .registry("publish", &receipt, &wrong_branch)
            .unwrap_err()
            .to_string()
            .contains("configured shared branch")
    );
    assert_eq!(fixture.store.count("put_object"), 1);
    assert!(
        fixture
            .registry("cancel", &receipt, &proof)
            .unwrap_err()
            .to_string()
            .contains("cannot withdraw")
    );
    assert_eq!(fixture.store.count("delete_object"), 0);
    fs::write(
        fixture.repo.root.join("independent-task.txt"),
        b"other work",
    )
    .unwrap();
    fixture.repo.run(["add", "independent-task.txt"]).unwrap();
    fixture
        .repo
        .run(["commit", "-q", "-m", "Advance shared branch"])
        .unwrap();
    fixture.repo.run(["push", "origin", "main"]).unwrap();
    let advanced = fixture.registry("publish", &receipt, &proof).unwrap_err();
    assert!(advanced.to_string().contains("configured shared branch"));
    assert_eq!(fixture.store.count("put_object"), 1);
}

#[test]
fn missing_bucket_error_does_not_erase_owned_multipart_upload_journal() {
    let store = Memory::new();
    store.state.borrow_mut().reject.insert(
        "abort_multipart_upload".to_owned(),
        "NoSuchBucket".to_owned(),
    );
    let mut row = json!({"destination_object":"2026/07/task/large","multipart_upload_id":"owned"});
    let error = abort_upload(&store, &mut row, || Ok(())).unwrap_err();
    assert!(error.to_string().contains("NoSuchBucket"));
    assert_eq!(row["multipart_upload_id"], "owned");
}

#[test]
fn registry_streaming_reads_over_64mib_and_compares_formatted_history_semantically() {
    large_registry_read(false);
}

#[test]
fn registry_streaming_replays_same_version_after_request_sent_header_interruption() {
    large_registry_read(true);
}

fn large_registry_read(interrupt: bool) {
    use crate::native_s3::tests::{Reply, interrupt_response_once, replayable_read_fixture};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };
    let mut task = Fixture::new();
    task.store.source("data", "source", b"payload", false);
    task.reserve();
    let mut receipt = task.run("copy").unwrap();
    receipt["bucket"] = "fixture-bucket".into();
    receipt["remote_prefix"] = "root".into();
    let compact = canonical(&receipt).unwrap();
    // Large leading whitespace exercises streaming without allocating a huge
    // decoded field. The old generic in-memory GET cap would reject this body.
    let mut formatted = vec![b' '; 64 * 1024 * 1024 + 1];
    formatted.extend(serde_json::to_vec_pretty(&receipt).unwrap());
    let object = format!("root/.workspace-mgr/archive/{}.json", digest(b"task"));
    let listing = format!(
        "<ListVersionsResult><Version><Key>{object}</Key><VersionId>r1</VersionId><Size>{}</Size><ETag>&quot;r1&quot;</ETag><LastModified>2026-07-02T00:00:00Z</LastModified><IsLatest>false</IsLatest></Version><Version><Key>{object}</Key><VersionId>r2</VersionId><Size>{}</Size><ETag>&quot;r2&quot;</ETag><LastModified>2026-07-02T00:00:01Z</LastModified><IsLatest>true</IsLatest></Version><IsTruncated>false</IsTruncated></ListVersionsResult>",
        formatted.len(),
        compact.len()
    );
    let listing_reply = Arc::new(Reply::xml(&listing));
    let first_reply = Arc::new(Reply {
        status: 200,
        headers: vec![("x-amz-version-id", "r1".into())],
        body: formatted,
    });
    let second_reply = Arc::new(Reply {
        status: 200,
        headers: vec![("x-amz-version-id", "r2".into())],
        body: compact.clone(),
    });
    let first_target = format!("/fixture-bucket/{object}?versionId=r1");
    let second_target = format!("/fixture-bucket/{object}?versionId=r2");
    let first_requests = Arc::new(AtomicUsize::new(0));
    let received = first_requests.clone();
    let (request_sent, request_received) = mpsc::channel();
    let (client, worker) = replayable_read_fixture(move |request| {
        assert_eq!(request.method, "GET");
        assert!(request.body.is_empty());
        let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
        let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
        if query.contains_key("versions") {
            assert_eq!(url.path(), "/fixture-bucket");
            assert_eq!(query["prefix"], object);
            assert_eq!(query["max-keys"], "1000");
            assert_eq!(query.len(), 3);
            return listing_reply.clone();
        }
        assert_eq!(url.path(), format!("/fixture-bucket/{object}"));
        assert_eq!(query.len(), 1);
        match query["versionId"].as_str() {
            "r1" => {
                if interrupt && received.fetch_add(1, Ordering::SeqCst) == 0 {
                    request_sent.send(()).unwrap();
                }
                first_reply.clone()
            }
            "r2" => second_reply.clone(),
            version => panic!("unexpected registry version {version}"),
        }
    });
    let (client, injected) = if interrupt {
        interrupt_response_once(client, first_target.clone(), request_received)
    } else {
        (client, Arc::new(AtomicUsize::new(0)))
    };
    let read = registry_read(&client, &task.repo, "task").unwrap().unwrap();
    assert_eq!(canonical(&read).unwrap(), compact);
    let attempts = worker.finish();
    let listing_target = attempts[0].request.target.as_str();
    assert!(listing_target.contains("versions="));
    assert_eq!(attempts.last().unwrap().request.target, listing_target);
    // Immutable versions may interleave, including retries of r1. Both reads
    // remain bounded by the same before/after inventory, and r2 is read once.
    let reads = &attempts[1..attempts.len() - 1];
    assert!(reads.iter().all(|attempt| {
        attempt.request.target == first_target || attempt.request.target == second_target
    }));
    assert_eq!(
        reads
            .iter()
            .filter(|attempt| attempt.request.target == second_target)
            .count(),
        1
    );
    assert!(
        reads
            .iter()
            .any(|attempt| attempt.request.target == first_target)
    );
    let successful = attempts
        .iter()
        .filter(|attempt| attempt.response_sent)
        .collect::<Vec<_>>();
    assert!(successful.len() >= 4);
    let first = attempts
        .iter()
        .filter(|attempt| attempt.request.target == first_target)
        .collect::<Vec<_>>();
    assert!(first.iter().any(|attempt| attempt.response_sent));
    assert_eq!(injected.load(Ordering::SeqCst), usize::from(interrupt));
    if interrupt {
        // A real r1 request was already parsed by the server before its header
        // read failed. The abandoned response must not consume r2's reply.
        assert!(first.len() >= 2);
        assert!(first.iter().any(|attempt| !attempt.response_sent));
        assert!(
            first
                .iter()
                .all(|attempt| attempt.request.headers["authorization"].contains("SignedHeaders="))
        );
    }
}

#[test]
fn registry_get_requires_exact_payload_version_and_complete_body_count() {
    let request = json!({"VersionId":"registry-version"});
    let valid = json!({"VersionId":"registry-version","ContentLength":3});
    validate_registry_get(&valid, &request, 3).unwrap();
    for invalid in [
        json!({"VersionId":"another","ContentLength":3}),
        json!({"VersionId":"registry-version","ContentLength":3,"DeleteMarker":true}),
        json!({"VersionId":"registry-version","ContentLength":4}),
        json!({"VersionId":"registry-version"}),
        json!({"VersionId":"registry-version","ContentLength":3,"Size":4}),
    ] {
        assert!(validate_registry_get(&invalid, &request, 3).is_err());
    }
}
