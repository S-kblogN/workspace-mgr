//! Adapter regressions through the native HTTP client, using loopback only.
use super::*;
use crate::native_s3::tests::{
    Reply, RoutedFixture, WireRequest, configure_repo, empty_fixture, routed_fixture,
    single_reply_fixture,
};
use md5::Md5;
use sha2::{Digest, Sha256};

fn repo() -> (tempfile::TempDir, GitRepo) {
    let directory = tempfile::tempdir().unwrap();
    let repo = GitRepo {
        root: directory.path().to_path_buf(),
    };
    (directory, repo)
}
fn entry(object: &str) -> Entry {
    Entry {
        metadata: StorageEntry {
            pointer: format!("{object}.wm-storage.json"),
            object: object.to_owned(),
            md5: Some("900150983cd24fb0d6963f7d28e17f72".to_owned()),
            size: Some(3),
            version_id: Some("v1".to_owned()),
            etag: Some("abc".to_owned()),
            verification: None,
            hash_name: "md5".to_owned(),
        },
        key: format!("root/{object}"),
        version: "v1".to_owned(),
        etag: Some("abc".to_owned()),
    }
}
fn head(version: &str, tag: &str, size: u64) -> Reply {
    Reply {
        status: 200,
        headers: vec![
            ("x-amz-version-id", version.to_owned()),
            ("ETag", format!("\"{tag}\"")),
            ("Content-Length", size.to_string()),
        ],
        body: Vec::new(),
    }
}
fn missing(code: &str, status: u16) -> Reply {
    Reply {
        status,
        headers: vec![("x-amz-error-code", code.to_owned())],
        body: format!("<Error><Code>{code}</Code><Message>fixture</Message></Error>").into_bytes(),
    }
}

fn read_targets(requests: &[WireRequest], method: &str) -> BTreeSet<String> {
    requests
        .iter()
        .filter(|request| request.method == method)
        .map(|request| request.target.clone())
        .collect()
}

fn copied_receipt() -> Value {
    json!({"schema_version":1,"status":"copied","remote":"workspace-mgr","bucket":"fixture-bucket","remote_prefix":"root","source":"task","destination":"archive/task","transaction_id":"fixture-transaction","versions":[{"source_object":"task/a","destination_object":"archive/task/a","source_version_id":"v1","source_last_modified":"2026-10-07T00:00:00+00:00","source_is_latest":true,"source_list_order":0,"delete_marker":false,"size":3,"source_etag":"abc","destination_version_id":"dst","destination_etag":"copied","destination_last_modified":"2026-10-07T00:00:01+00:00"}]})
}
fn registry_object() -> String {
    format!(
        "root/.workspace-mgr/archive/{}.json",
        crate::hex::encode_lower(Sha256::digest(b"task"))
    )
}
fn registry_listing() -> Reply {
    Reply::xml(&format!(
        "<ListVersionsResult><Version><Key>{}</Key><VersionId>registry-version</VersionId><Size>1</Size><ETag>&quot;r&quot;</ETag><IsLatest>true</IsLatest><LastModified>2026-10-07T00:00:00Z</LastModified></Version><IsTruncated>false</IsTruncated></ListVersionsResult>",
        registry_object()
    ))
}
fn registry_body(receipt: &Value) -> Reply {
    Reply {
        status: 200,
        headers: vec![("x-amz-version-id", "registry-version".into())],
        body: serde_json::to_vec(receipt).unwrap(),
    }
}

#[test]
fn only_missing_exact_head_resolves_verified_historical_mapping() {
    let (_directory, repo) = repo();
    let receipt = copied_receipt();
    let (client, worker) = routed_fixture(move |request| {
        if request.method == "HEAD" {
            if request.target.contains("/archive/task/a?") {
                head("dst", "copied", 3)
            } else {
                missing("NoSuchVersion", 404)
            }
        } else if request.target.contains("versions=") {
            registry_listing()
        } else {
            registry_body(&receipt)
        }
    });
    verify_head(&client, &repo, &entry("task/a")).unwrap();
    let requests = worker.finish_requests();
    assert_eq!(read_targets(&requests, "HEAD").len(), 2);
    assert!(
        requests
            .iter()
            .any(|request| request.target == "/fixture-bucket/root/archive/task/a?versionId=dst")
    );
}

#[test]
fn publishing_exact_versions_never_substitutes_a_historical_archive_location() {
    let (_directory, repo) = repo();
    let (client, worker) = single_reply_fixture(missing("NoSuchVersion", 404));
    assert!(verify_storage_entries(&client, &repo, &[entry("task/a").metadata]).is_err());
    let requests = worker.finish_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "HEAD");
    assert!(requests[0].target.contains("/root/task/a?versionId=v1"));
}

#[test]
fn historical_mapping_mismatched_size_or_etag_never_heads_destination() {
    let (_directory, repo) = repo();
    for field in ["size", "source_etag"] {
        let mut receipt = copied_receipt();
        receipt["versions"][0][field] = if field == "size" {
            4.into()
        } else {
            "different".into()
        };
        let (client, worker) = routed_fixture(move |request| {
            if request.method == "HEAD" {
                missing("NoSuchVersion", 404)
            } else if request.target.contains("versions=") {
                registry_listing()
            } else {
                registry_body(&receipt)
            }
        });
        assert!(
            verify_head(&client, &repo, &entry("task/a"))
                .unwrap_err()
                .to_string()
                .contains("mismatched")
        );
        let requests = worker.finish_requests();
        assert_eq!(read_targets(&requests, "HEAD").len(), 1);
    }
}

#[test]
fn dense_listing_stops_at_two_pages_and_falls_back_to_exact_heads() {
    let (_directory, repo) = repo();
    let (client, worker) = routed_fixture(|request| {
        if request.method == "HEAD" {
            head("v1", "abc", 3)
        } else {
            Reply::xml(
                "<ListVersionsResult><IsTruncated>true</IsTruncated><NextKeyMarker>root/task/nonadvancing</NextKeyMarker><NextVersionIdMarker>same</NextVersionIdMarker></ListVersionsResult>",
            )
        }
    });
    let entries = (0..8)
        .map(|index| entry(&format!("task/a{index}")))
        .collect::<Vec<_>>();
    verify_entries(&client, &repo, &entries).unwrap();
    let requests = worker.finish_requests();
    assert_eq!(read_targets(&requests, "GET").len(), 2);
    assert_eq!(read_targets(&requests, "HEAD").len(), 8);
}

#[test]
fn denied_dense_listing_uses_readable_exact_heads() {
    let (_directory, repo) = repo();
    let (client, worker) = routed_fixture(|request| {
        if request.method == "HEAD" {
            head("v1", "abc", 3)
        } else {
            missing("AccessDenied", 403)
        }
    });
    let entries = (0..8)
        .map(|index| entry(&format!("task/a{index}")))
        .collect::<Vec<_>>();
    verify_entries(&client, &repo, &entries).unwrap();
    let requests = worker.finish_requests();
    assert_eq!(read_targets(&requests, "GET").len(), 1);
    assert_eq!(read_targets(&requests, "HEAD").len(), 8);
}

#[test]
fn network_reads_reach_every_exact_version_and_overlap() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let (_directory, repo) = repo();
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let worker_active = active.clone();
    let worker_maximum = maximum.clone();
    let (client, worker) = routed_fixture(move |request| {
        assert_eq!(request.method, "HEAD");
        let count = worker_active.fetch_add(1, Ordering::SeqCst) + 1;
        worker_maximum.fetch_max(count, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(35));
        worker_active.fetch_sub(1, Ordering::SeqCst);
        head("v1", "abc", 3)
    });
    // Sparse prefixes require every exact HEAD. The pool bound is checked
    // inside bounded_map; an abandoned HTTP attempt may outlive its retry.
    let entries = (0..40)
        .map(|index| entry(&format!("task{index}/a")))
        .collect::<Vec<_>>();
    verify_entries(&client, &repo, &entries).unwrap();
    let requests = worker.finish_requests();
    assert_eq!(read_targets(&requests, "HEAD").len(), 40);
    assert!(maximum.load(Ordering::SeqCst) > 1);
}

#[test]
fn sparse_versions_use_exact_heads_and_metadata_mismatch_fails() {
    let (_dir, repo) = repo();
    let (client, worker) = single_reply_fixture(head("v1", "abc", 3));
    verify_entries(&client, &repo, &[entry("task/a")]).unwrap();
    let requests = worker.finish_requests();
    assert_eq!(read_targets(&requests, "HEAD").len(), 1);
    assert_eq!(requests[0].method, "HEAD");
    assert!(requests[0].target.ends_with("versionId=v1"));
    for response in [
        head("wrong", "abc", 3),
        head("v1", "wrong", 3),
        head("v1", "abc", 4),
    ] {
        let (client, worker) = single_reply_fixture(response);
        assert!(
            verify_head(&client, &repo, &entry("task/a"))
                .unwrap_err()
                .to_string()
                .contains("mismatched")
        );
        worker.finish_requests();
    }
}

#[test]
fn permission_and_named_bucket_errors_never_trigger_registry_lookup() {
    let (_dir, repo) = repo();
    for (status, code) in [
        (403, "AccessDenied"),
        (404, "NoSuchBucket"),
        (403, "NoSuchBucket"),
    ] {
        let (client, worker) = single_reply_fixture(missing(code, status));
        assert!(verify_head(&client, &repo, &entry("task/a")).is_err());
        let requests = worker.finish_requests();
        assert_eq!(read_targets(&requests, "HEAD").len(), 1);
        assert_eq!(requests[0].method, "HEAD");
    }
}

#[test]
fn cache_content_requires_exact_physical_size_and_md5() {
    let (dir, _repo) = repo();
    let path = dir.path().join("bytes");
    let entry = entry("task/a");
    fs::write(&path, b"abc").unwrap();
    assert!(content_matches(&entry, &path).unwrap());
    fs::write(&path, b"abd").unwrap();
    assert!(!content_matches(&entry, &path).unwrap());
    fs::write(&path, b"abcx").unwrap();
    assert!(!content_matches(&entry, &path).unwrap());
}

#[test]
fn pending_aliases_validate_exact_storage_size_and_identity() {
    let (client, worker) = empty_fixture();
    let receipt = json!({"schema_version":1,"status":"planned","remote":"workspace-mgr","bucket":"fixture-bucket","remote_prefix":"root","source":"task","destination":"archive/task","versions":[{"source_object":"task/a","destination_object":"archive/task/a","source_version_id":"v1","delete_marker":false,"size":3,"source_etag":"abc"}]});
    let mut target = entry("archive/task/a");
    pending_aliases(
        &client,
        std::slice::from_mut(&mut target),
        std::slice::from_ref(&receipt),
    )
    .unwrap();
    assert_eq!(target.key, "root/task/a");
    assert_eq!(target.version, "v1");
    let mut invalid = receipt.clone();
    invalid["bucket"] = "another".into();
    assert!(pending_aliases(&client, &mut [entry("archive/task/a")], &[invalid]).is_err());
    let mut wrong = entry("archive/task/a");
    wrong.metadata.size = Some(4);
    assert!(pending_aliases(&client, &mut [wrong], &[receipt]).is_err());
    assert!(worker.finish_requests().is_empty());
}

#[test]
fn pending_aliases_allow_only_formally_validated_task_rename_leaf_changes() {
    let (client, worker) = empty_fixture();
    let source = "20260702-123456-original";
    let destination = "20260702-123456-renamed";
    let receipt = json!({"schema_version":1,"status":"planned","remote":"workspace-mgr",
        "bucket":"fixture-bucket","remote_prefix":"root","migration_kind":"task-rename",
        "task_id":source,"source":source,"destination":destination,"versions":[{
        "source_object":format!("{source}/a"),"destination_object":format!("{destination}/a"),
        "source_version_id":"v1","delete_marker":false,"size":3,"source_etag":"abc"}]});
    let mut target = entry(&format!("{destination}/a"));
    pending_aliases(
        &client,
        std::slice::from_mut(&mut target),
        std::slice::from_ref(&receipt),
    )
    .unwrap();
    assert_eq!(target.key, format!("root/{source}/a"));
    for (field, value) in [
        ("migration_kind", "unknown"),
        ("task_id", "20260703-123456-other"),
        ("destination", "20260703-123456-renamed"),
    ] {
        let mut invalid = receipt.clone();
        invalid[field] = value.into();
        assert!(
            pending_aliases(
                &client,
                &mut [entry(&format!("{destination}/a"))],
                &[invalid]
            )
            .is_err()
        );
    }
    let mut unmarked = receipt;
    unmarked.as_object_mut().unwrap().remove("migration_kind");
    assert!(
        pending_aliases(
            &client,
            &mut [entry(&format!("{destination}/a"))],
            &[unmarked]
        )
        .is_err()
    );
    assert!(worker.finish_requests().is_empty());
}

#[test]
fn historical_directory_manifest_flattens_without_reading_payload() {
    let (dir, repo) = repo();
    repo.run(["init"]).unwrap();
    repo.run(["config", "user.name", "Fixture"]).unwrap();
    repo.run(["config", "user.email", "fixture@example.invalid"])
        .unwrap();
    fs::create_dir_all(dir.path().join("task")).unwrap();
    // DVC's directory hash is MD5 of its sorted logical [{md5,relpath}] tree.
    let tree = b"[{\"md5\": \"900150983cd24fb0d6963f7d28e17f72\", \"relpath\": \"a\"}]";
    let digest = format!("{}.dir", crate::hex::encode_lower(Md5::digest(tree)));
    let pointer = format!(
        "outs:\n- md5: {digest}\n  size: 3\n  path: data\n  files:\n  - md5: 900150983cd24fb0d6963f7d28e17f72\n    size: 3\n    relpath: a\n    cloud:\n      workspace-mgr:\n        version_id: v1\n        etag: abc\n"
    );
    fs::write(dir.path().join("task/data.dvc"), pointer).unwrap();
    repo.run(["add", "task/data.dvc"]).unwrap();
    repo.run(["commit", "-m", "fixture"]).unwrap();
    let payload = json!([{"revision":"HEAD","pointers":["task/data.dvc"]}]);
    let listed = purge(&repo, "list", &payload).unwrap();
    assert_eq!(
        listed,
        json!([{"pointer":"task/data.dvc","object":"task/data/a","version_id":"v1"}])
    );
    assert!(!dir.path().join("task/data").exists());
}

#[test]
fn historical_files_only_directory_reads_without_its_omitted_aggregate() {
    let (dir, repo) = repo();
    repo.run(["init"]).unwrap();
    repo.run(["config", "user.name", "Fixture"]).unwrap();
    repo.run(["config", "user.email", "fixture@example.invalid"])
        .unwrap();
    fs::create_dir_all(dir.path().join("task")).unwrap();
    // DVC 3 writes a cloud-versioned directory without `md5`, `size` or `nfiles`.
    let pointer = "outs:\n- hash: md5\n  path: data\n  files:\n  - relpath: a\n    md5: 900150983cd24fb0d6963f7d28e17f72\n    size: 3\n    cloud:\n      workspace-mgr:\n        etag: abc\n        version_id: v1\n";
    fs::write(dir.path().join("task/data.dvc"), pointer).unwrap();
    repo.run(["add", "task/data.dvc"]).unwrap();
    repo.run(["commit", "-m", "fixture"]).unwrap();
    let payload = json!([{"revision":"HEAD","pointers":["task/data.dvc"]}]);
    assert_eq!(
        purge(&repo, "list", &payload).unwrap(),
        json!([{"pointer":"task/data.dvc","object":"task/data/a","version_id":"v1"}])
    );
    let pointers = vec!["task/data.dvc".to_owned()];
    for revision in [Some("HEAD"), None] {
        let entries = crate::native_engine::metadata_entries(&repo, revision, &pointers).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].object, "task/data/a");
        assert_eq!(entries[0].version_id.as_deref(), Some("v1"));
    }
    crate::native_engine::install_directory_manifests(&repo, &pointers).unwrap();
    // The rebuilt aggregate stays in memory; the pointer is never rewritten.
    assert_eq!(
        fs::read_to_string(dir.path().join("task/data.dvc")).unwrap(),
        pointer
    );
    // A stated aggregate that disagrees with the list is still refused.
    fs::write(
        dir.path().join("task/data.dvc"),
        pointer.replace(
            "  path: data\n",
            "  path: data\n  md5: 00000000000000000000000000000000.dir\n",
        ),
    )
    .unwrap();
    assert!(
        crate::native_engine::metadata_entries(&repo, None, &pointers)
            .unwrap_err()
            .to_string()
            .contains("legacy directory manifest hash mismatch")
    );
}

#[test]
fn shared_cas_history_is_retained_outside_path_version_retirement() {
    let (dir, repo) = repo();
    repo.run(["init"]).unwrap();
    repo.run(["config", "user.name", "Fixture"]).unwrap();
    repo.run(["config", "user.email", "fixture@example.invalid"])
        .unwrap();
    fs::create_dir_all(dir.path().join(".dvc")).unwrap();
    fs::create_dir_all(dir.path().join("task")).unwrap();
    let remote = "[core]\nremote = source\n['remote \"source\"']\nurl = s3://fixture-bucket/root\n";
    fs::write(dir.path().join(".dvc/config"), remote).unwrap();
    fs::write(
        dir.path().join("task/data.dvc"),
        "outs:\n- md5: 900150983cd24fb0d6963f7d28e17f72.dir\n  hash: md5\n  size: 3\n  nfiles: 1\n  path: data\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("task/a.wm-storage.json"),
        crate::storage_format::Manifest {
            schema_version: 1,
            path: "a".into(),
            kind: crate::storage_format::Kind::File,
            checksum: crate::storage_format::Checksum {
                algorithm: "md5".into(),
                digest: "900150983cd24fb0d6963f7d28e17f72".into(),
            },
            size: 3,
            version: Some(crate::storage_format::Version {
                id: "native-exact".into(),
                etag: None,
                verification: None,
            }),
            entries: None,
        }
        .serialize()
        .unwrap(),
    )
    .unwrap();
    repo.run(["add", ".dvc/config", "task"]).unwrap();
    repo.run(["commit", "-m", "CAS and native history"])
        .unwrap();
    let request =
        json!([{"revision":"HEAD","pointers":["task/data.dvc","task/a.wm-storage.json"]}]);
    assert_eq!(
        purge(&repo, "list", &request).unwrap(),
        json!([{"pointer":"task/a.wm-storage.json","object":"task/a","version_id":"native-exact"}])
    );
    assert!(!dir.path().join(".dvc/cache").exists());

    fs::write(
        dir.path().join(".dvc/config"),
        format!("{remote}version_aware = true\n"),
    )
    .unwrap();
    fs::write(
        dir.path().join("task/data.dvc"),
        "outs:\n- md5: 900150983cd24fb0d6963f7d28e17f72\n  hash: md5\n  size: 3\n  path: data\n",
    )
    .unwrap();
    repo.run(["add", ".dvc/config", "task/data.dvc"]).unwrap();
    repo.run(["commit", "-m", "Invalid unbound path history"])
        .unwrap();
    let rejected = purge(&repo, "list", &request).unwrap_err().to_string();
    assert!(rejected.contains("no exact version ID"), "{rejected}");
}

fn version_row(key: &str, id: &str) -> String {
    format!(
        "<Version><Key>{key}</Key><VersionId>{id}</VersionId><IsLatest>false</IsLatest><LastModified>2026-10-07T00:00:00Z</LastModified><ETag>&quot;abc&quot;</ETag><Size>3</Size></Version>"
    )
}
fn marker_row(key: &str, id: &str) -> String {
    format!(
        "<DeleteMarker><Key>{key}</Key><VersionId>{id}</VersionId><IsLatest>true</IsLatest><LastModified>2026-10-07T00:00:01Z</LastModified></DeleteMarker>"
    )
}
fn history(rows: &str) -> Reply {
    Reply::xml(&format!(
        "<ListVersionsResult>{rows}<IsTruncated>false</IsTruncated></ListVersionsResult>"
    ))
}
fn deleted() -> Reply {
    Reply {
        status: 204,
        headers: Vec::new(),
        body: Vec::new(),
    }
}

fn batch_delete_items(request: &WireRequest) -> Vec<(String, String)> {
    #[derive(serde::Deserialize)]
    struct Request {
        #[serde(rename = "Object")]
        objects: Vec<Object>,
    }
    #[derive(serde::Deserialize)]
    struct Object {
        #[serde(rename = "Key")]
        key: String,
        #[serde(rename = "VersionId")]
        version: String,
    }
    assert_eq!(request.method, "POST");
    let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
    assert_eq!(url.path().trim_end_matches('/'), "/fixture-bucket");
    assert!(url.query_pairs().any(|(name, _)| name == "delete"));
    let parsed: Request = quick_xml::de::from_reader(request.body.as_slice()).unwrap();
    assert!((1..=1_000).contains(&parsed.objects.len()));
    parsed
        .objects
        .into_iter()
        .map(|object| (object.key, object.version))
        .collect()
}

fn batch_deleted(items: &[(String, String)]) -> Reply {
    let mut body = String::from("<DeleteResult>");
    for (key, version) in items {
        body.push_str(&format!(
            "<Deleted><Key>{key}</Key><VersionId>{version}</VersionId></Deleted>"
        ));
    }
    body.push_str("</DeleteResult>");
    Reply::xml(&body)
}

#[test]
fn generic_purge_deletes_complete_exact_object_history_including_markers() {
    let (_directory, repo) = repo();
    let retired = std::sync::Mutex::new(BTreeSet::new());
    let (client, worker) = routed_fixture(move |request| {
        let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
        let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
        if request.method == "POST" {
            let items = batch_delete_items(request);
            assert_eq!(items.len(), 2);
            for (object, version) in &items {
                assert_eq!(object, "root/task/a");
                assert!(matches!(version.as_str(), "v1" | "d1"));
                assert!(retired.lock().unwrap().insert(version.clone()));
            }
            return batch_deleted(&items);
        }
        assert_eq!(request.method, "GET");
        assert!(query.contains_key("versions"));
        if query["prefix"] == registry_object() {
            return history("");
        }
        assert_eq!(query["prefix"], "root/task/a");
        if retired.lock().unwrap().len() == 2 {
            return history(&version_row("root/task/ab", "sibling"));
        }
        if let Some(marker) = query.get("key-marker") {
            assert_eq!(marker, "root/task/a");
            assert_eq!(query["version-id-marker"], "v1");
            return history(&marker_row("root/task/a", "d1"));
        }
        Reply::xml(&format!(
            "<ListVersionsResult>{}<IsTruncated>true</IsTruncated><NextKeyMarker>root/task/a</NextKeyMarker><NextVersionIdMarker>v1</NextVersionIdMarker></ListVersionsResult>",
            version_row("root/task/a", "v1")
        ))
    });
    let result = delete_candidates(
        &client,
        &repo,
        &json!([{"pointer":"task/a.dvc","object":"task/a","version_id":"v1"}]),
    )
    .unwrap();
    assert_eq!(
        result["deleted"][0]["deleted_version_ids"],
        json!(["d1", "v1"])
    );
    let requests = worker.finish_requests();
    let deleted = requests
        .iter()
        .filter(|request| request.method == "POST")
        .collect::<Vec<_>>();
    assert_eq!(deleted.len(), 1);
    assert_eq!(
        batch_delete_items(deleted[0])
            .into_iter()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            ("root/task/a".into(), "v1".into()),
            ("root/task/a".into(), "d1".into())
        ])
    );
}

#[test]
fn generic_purge_acknowledges_every_initially_absent_candidate() {
    for has_payload in [false, true] {
        let (_directory, repo) = repo();
        let removed = std::sync::atomic::AtomicBool::new(false);
        let (client, worker) = routed_fixture(move |request| {
            if request.method == "DELETE" {
                assert!(request.target.ends_with("/root/task/a?versionId=v1"));
                removed.store(true, Ordering::SeqCst);
                return deleted();
            }
            assert_eq!(request.method, "GET");
            if request.target.contains("prefix=root%2Ftask%2Fa")
                && has_payload
                && !removed.load(Ordering::SeqCst)
            {
                history(&version_row("root/task/a", "v1"))
            } else {
                history("")
            }
        });
        let candidates = json!([
            {"pointer":"task/a.dvc","object":"task/a","version_id":"v1"},
            {"pointer":"task/a.dvc","object":"task/a","version_id":"absent-2"},
            {"pointer":"task/a.wm-storage.json","object":"task/a","version_id":"absent-3"}
        ]);
        let result = delete_candidates(&client, &repo, &candidates).unwrap();
        assert_eq!(
            result["already_absent"],
            if has_payload {
                json!([candidates[1], candidates[2]])
            } else {
                candidates
            }
        );
        worker.finish_requests();
    }
}

#[test]
fn generic_purge_preserves_single_deletes_for_history_with_a_null_generation() {
    use std::sync::{Arc, Mutex};

    let (_directory, repo) = repo();
    let retired = Arc::new(Mutex::new(BTreeSet::new()));
    let observed = retired.clone();
    let (client, worker) = routed_fixture(move |request| {
        let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
        let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
        if request.method == "DELETE" {
            assert_eq!(url.path(), "/fixture-bucket/root/task/a");
            let version = query["versionId"].clone();
            assert!(matches!(version.as_str(), "v1" | "null"));
            assert!(observed.lock().unwrap().insert(version));
            return deleted();
        }
        assert_eq!(request.method, "GET");
        assert!(query.contains_key("versions"));
        if query["prefix"] == registry_object() {
            return history("");
        }
        assert_eq!(query["prefix"], "root/task/a");
        let retired = observed.lock().unwrap();
        let mut rows = version_row("root/task/ab", "neighbor");
        for version in ["v1", "null"] {
            if !retired.contains(version) {
                rows.push_str(&version_row("root/task/a", version));
            }
        }
        history(&rows)
    });
    let result = delete_candidates(
        &client,
        &repo,
        &json!([{"pointer":"task/a.wm-storage.json","object":"task/a","version_id":"v1"}]),
    )
    .unwrap();
    assert_eq!(
        result["deleted"][0]["deleted_version_ids"],
        json!(["null", "v1"])
    );
    assert_eq!(
        *retired.lock().unwrap(),
        BTreeSet::from(["null".into(), "v1".into()])
    );
    let requests = worker.finish_requests();
    assert_eq!(requests.iter().filter(|r| r.method == "DELETE").count(), 2);
    assert!(requests.iter().all(|r| r.method != "POST"));
    assert_eq!(requests.last().unwrap().method, "GET");
}

#[test]
fn generic_batch_purge_partial_failure_keeps_candidates_and_retries_remaining_version() {
    use std::sync::{Arc, Mutex};

    let (_directory, repo) = repo();
    let retired = Arc::new(Mutex::new(BTreeSet::new()));
    let observed = retired.clone();
    let (client, worker) = routed_fixture(move |request| {
        let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
        let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
        if request.method == "POST" {
            assert_eq!(
                batch_delete_items(request)
                    .into_iter()
                    .collect::<BTreeSet<_>>(),
                BTreeSet::from([
                    ("root/task/a".into(), "v1".into()),
                    ("root/task/a".into(), "v2".into())
                ])
            );
            assert!(observed.lock().unwrap().insert("v1".to_owned()));
            return Reply::xml(
                "<DeleteResult><Deleted><Key>root/task/a</Key><VersionId>v1</VersionId></Deleted><Error><Key>root/task/a</Key><VersionId>v2</VersionId><Code>AccessDenied</Code><Message>fixture partial failure</Message></Error></DeleteResult>",
            );
        }
        if request.method == "DELETE" {
            assert_eq!(url.path(), "/fixture-bucket/root/task/a");
            assert_eq!(query["versionId"], "v2");
            assert!(observed.lock().unwrap().insert("v2".to_owned()));
            return deleted();
        }
        assert_eq!(request.method, "GET");
        assert!(query.contains_key("versions"));
        if query["prefix"] == registry_object() {
            return history("");
        }
        assert_eq!(query["prefix"], "root/task/a");
        let retired = observed.lock().unwrap();
        let mut rows = version_row("root/task/ab", "neighbor");
        for version in ["v1", "v2"] {
            if !retired.contains(version) {
                rows.push_str(&version_row("root/task/a", version));
            }
        }
        history(&rows)
    });
    let payload = json!([{"pointer":"task/a.wm-storage.json","object":"task/a","version_id":"v1"}]);
    let unchanged = payload.clone();
    let error = delete_candidates(&client, &repo, &payload).unwrap_err();
    assert!(error.to_string().contains("AccessDenied"), "{error}");
    assert_eq!(payload, unchanged);
    assert_eq!(*retired.lock().unwrap(), BTreeSet::from(["v1".into()]));
    let result = delete_candidates(&client, &repo, &payload).unwrap();
    assert_eq!(result["deleted"][0]["deleted_version_ids"], json!(["v2"]));
    assert_eq!(
        *retired.lock().unwrap(),
        BTreeSet::from(["v1".into(), "v2".into()])
    );
    let requests = worker.finish_requests();
    assert_eq!(requests.iter().filter(|r| r.method == "POST").count(), 1);
    assert_eq!(requests.iter().filter(|r| r.method == "DELETE").count(), 1);
    assert_eq!(requests.last().unwrap().method, "GET");
}

#[test]
fn generic_batch_purge_caps_large_exact_history_at_one_thousand_items_per_request() {
    use std::sync::{Arc, Mutex};

    let (_directory, repo) = repo();
    const COUNT: usize = 2_005;
    let retired = Arc::new(Mutex::new(BTreeSet::new()));
    let observed = retired.clone();
    let (client, worker) = routed_fixture(move |request| {
        let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
        let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
        if request.method == "POST" {
            let items = batch_delete_items(request);
            let mut retired = observed.lock().unwrap();
            for (key, version) in &items {
                assert_eq!(key, "root/task/a");
                assert!(version.strip_prefix('v').unwrap().parse::<usize>().unwrap() < COUNT);
                assert!(retired.insert(version.clone()));
            }
            return batch_deleted(&items);
        }
        assert_eq!(request.method, "GET");
        assert!(query.contains_key("versions"));
        if query["prefix"] == registry_object() {
            return history("");
        }
        assert_eq!(query["prefix"], "root/task/a");
        if observed.lock().unwrap().len() == COUNT {
            return history(&version_row("root/task/ab", "neighbor"));
        }
        let start = query
            .get("version-id-marker")
            .map_or(0, |version| version[1..].parse::<usize>().unwrap() + 1);
        let end = (start + 1_000).min(COUNT);
        let mut rows = (start..end)
            .map(|index| version_row("root/task/a", &format!("v{index:05}")))
            .collect::<String>();
        if end < COUNT {
            Reply::xml(&format!(
                "<ListVersionsResult>{rows}<IsTruncated>true</IsTruncated><NextKeyMarker>root/task/a</NextKeyMarker><NextVersionIdMarker>v{:05}</NextVersionIdMarker></ListVersionsResult>",
                end - 1
            ))
        } else {
            rows.push_str(&version_row("root/task/ab", "neighbor"));
            history(&rows)
        }
    });
    let result = delete_candidates(
        &client,
        &repo,
        &json!([{"pointer":"task/a.wm-storage.json","object":"task/a","version_id":"v00000"}]),
    )
    .unwrap();
    assert_eq!(retired.lock().unwrap().len(), COUNT);
    let ids = result["deleted"][0]["deleted_version_ids"]
        .as_array()
        .unwrap();
    assert_eq!(ids.len(), COUNT);
    assert_eq!(ids.first().unwrap(), "v00000");
    assert_eq!(ids.last().unwrap(), "v02004");
    let requests = worker.finish_requests();
    let sizes = requests
        .iter()
        .filter(|r| r.method == "POST")
        .map(|request| batch_delete_items(request).len())
        .collect::<Vec<_>>();
    assert_eq!(sizes, [1_000, 1_000, 5]);
    assert!(requests.iter().all(|r| r.method != "DELETE"));
    assert_eq!(requests.last().unwrap().method, "GET");
}

#[derive(Default)]
struct ConcurrentPurgeState {
    deleted: BTreeSet<(String, String)>,
    active: BTreeSet<String>,
    maximum: usize,
    post_lists: usize,
    published: bool,
    failed: bool,
}
type ConcurrentPurgeFixture = (
    S3Client,
    RoutedFixture,
    std::sync::Arc<(std::sync::Mutex<ConcurrentPurgeState>, std::sync::Condvar)>,
);

fn concurrent_purge_fixture(
    objects: Vec<String>,
    marker: bool,
    fail_first_delete: bool,
    publish_after_first_wave: bool,
) -> ConcurrentPurgeFixture {
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    let state = Arc::new((Mutex::new(ConcurrentPurgeState::default()), Condvar::new()));
    let observed = state.clone();
    let registry = format!(
        "root/.workspace-mgr/archive/{}.json",
        crate::hex::encode_lower(Sha256::digest(b"task/deep/shared"))
    );
    let mut receipt = copied_receipt();
    receipt["source"] = "task/deep/shared".into();
    receipt["destination"] = "archive/task/deep/shared".into();
    receipt["versions"][0]["source_object"] = "task/deep/shared/item04".into();
    receipt["versions"][0]["destination_object"] = "archive/task/deep/shared/item04".into();
    let (client, worker) = routed_fixture(move |request| {
        let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
        let query = url.query_pairs().collect::<BTreeMap<_, _>>();
        let (lock, changed) = &*observed;
        if request.method == "DELETE" || request.method == "POST" {
            let items = if request.method == "POST" {
                batch_delete_items(request)
            } else {
                vec![(
                    url.path().strip_prefix("/fixture-bucket/").unwrap().into(),
                    query.get("versionId").unwrap().to_string(),
                )]
            };
            let mut state = lock.lock().unwrap();
            for (object, version) in &items {
                assert!(objects.iter().any(|expected| expected == object));
                assert!(version == "v1" || (marker && version == "d1"));
                if fail_first_delete && object == &objects[0] && !state.failed {
                    state.failed = true;
                    return missing("InternalError", 500);
                }
                assert!(state.deleted.insert((object.clone(), version.clone())));
            }
            return if request.method == "POST" {
                batch_deleted(&items)
            } else {
                deleted()
            };
        }
        assert_eq!(request.method, "GET");
        if !query.contains_key("versions") {
            assert_eq!(url.path(), format!("/fixture-bucket/{registry}"));
            return registry_body(&receipt);
        }
        let prefix = query.get("prefix").unwrap().as_ref();
        if prefix.starts_with("root/.workspace-mgr/archive/") {
            if prefix == registry && lock.lock().unwrap().published {
                return history(&version_row(&registry, "registry-version"));
            }
            return history("");
        }
        assert!(objects.iter().any(|expected| expected == prefix));
        let mut state = lock.lock().unwrap();
        let remaining = !state.deleted.contains(&(prefix.into(), "v1".into()));
        if remaining {
            state.active.insert(prefix.into());
            state.maximum = state.maximum.max(state.active.len());
            changed.notify_all();
            // Make the first wave overlap deterministically, with a finite wait
            // so a regression to a sequential implementation cannot hang CI.
            let (updated, _) = changed
                .wait_timeout_while(state, Duration::from_secs(5), |state| {
                    state.maximum < PURGE_WORKERS
                })
                .unwrap();
            state = updated;
        } else if state.active.remove(prefix) {
            state.post_lists += 1;
            if publish_after_first_wave {
                if state.post_lists == PURGE_WORKERS {
                    state.published = true;
                }
                changed.notify_all();
                let (updated, _) = changed
                    .wait_timeout_while(state, Duration::from_secs(5), |state| !state.published)
                    .unwrap();
                state = updated;
            }
        }
        let mut rows = String::new();
        if remaining {
            rows.push_str(&version_row(prefix, "v1"));
        }
        if marker && !state.deleted.contains(&(prefix.into(), "d1".into())) {
            rows.push_str(&marker_row(prefix, "d1"));
        }
        // ListObjectVersions uses prefix matching; a neighboring logical key
        // must never become a deletion candidate for this object.
        rows.push_str(&version_row(&format!("{prefix}-neighbor"), "neighbor"));
        history(&rows)
    });
    (client, worker, state)
}

fn concurrent_purge_payload(count: usize) -> Value {
    (0..count)
        .rev()
        .map(|index| {
            let object = format!("task/deep/shared/item{index:02}");
            json!({"pointer":format!("{object}.wm-storage.json"),"object":object,"version_id":"v1"})
        })
        .collect::<Vec<_>>()
        .into()
}

fn concurrent_purge_objects(count: usize) -> Vec<String> {
    (0..count)
        .map(|index| format!("root/task/deep/shared/item{index:02}"))
        .collect()
}

#[test]
fn generic_purge_pipelines_distinct_objects_with_four_workers_and_fresh_shared_ancestors() {
    let (_directory, repo) = repo();
    let count = 12;
    let mut payload = concurrent_purge_payload(count);
    // Duplicate candidates must still share a single object pipeline.
    let duplicate = payload[0].clone();
    payload.as_array_mut().unwrap().push(duplicate);
    // Every object checks all three ancestor registries before and after
    // deletion, including delete markers in its exact pipeline.
    let (client, worker, state) =
        concurrent_purge_fixture(concurrent_purge_objects(count), true, false, false);
    let result = delete_candidates(&client, &repo, &payload).unwrap();
    let requests = worker.finish_requests();
    let state = state.0.lock().unwrap();
    assert_eq!(state.maximum, PURGE_WORKERS);
    assert!(state.active.is_empty());
    assert_eq!(state.deleted.len(), count * 2);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "POST")
            .count(),
        count
    );
    assert_eq!(state.post_lists, count);
    assert_eq!(result["deleted"].as_array().unwrap().len(), count);
    for (index, value) in result["deleted"].as_array().unwrap().iter().enumerate() {
        assert_eq!(value["object"], format!("task/deep/shared/item{index:02}"));
        assert_eq!(value["deleted_version_ids"], json!(["d1", "v1"]));
    }
    for source in ["task", "task/deep", "task/deep/shared"] {
        let registry = format!(
            "root/.workspace-mgr/archive/{}.json",
            crate::hex::encode_lower(Sha256::digest(source.as_bytes()))
        );
        let reads = requests
            .iter()
            .filter(|request| {
                let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
                url.query_pairs()
                    .any(|(name, value)| name == "prefix" && value == registry)
            })
            .count();
        assert!(reads >= count * 2, "fresh registry checks for {source}");
    }
}

#[test]
fn generic_purge_queued_object_observes_new_registry_and_refuses_uncoordinated_delete() {
    let (_directory, repo) = repo();
    let payload = concurrent_purge_payload(5);
    // The queued fifth object reads the registry published by the completed
    // first wave and rejects the missing coordination proof before DELETE.
    let (client, worker, state) =
        concurrent_purge_fixture(concurrent_purge_objects(5), false, false, true);
    let error = delete_candidates(&client, &repo, &payload).unwrap_err();
    assert!(error.to_string().contains("atomic Git registry binding"));
    let requests = worker.finish_requests();
    let state = state.0.lock().unwrap();
    assert!(state.published);
    assert_eq!(state.maximum, PURGE_WORKERS);
    assert_eq!(state.deleted.len(), PURGE_WORKERS);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "DELETE")
            .count(),
        PURGE_WORKERS
    );
    assert!(
        !state
            .deleted
            .contains(&("root/task/deep/shared/item04".into(), "v1".into()))
    );
    assert!(
        requests
            .iter()
            .filter(|request| request.method == "DELETE")
            .all(|request| {
                !request.target.contains("item04") && !request.target.contains("neighbor")
            })
    );
}

#[test]
fn generic_purge_joins_inflight_objects_after_failure_and_can_retry_the_same_candidates() {
    let (_directory, repo) = repo();
    let payload = concurrent_purge_payload(PURGE_WORKERS);
    let unchanged = payload.clone();
    // Join every in-flight object after a failed DELETE, then retry only the
    // still-present version while preserving the original candidates.
    let (client, worker, state) =
        concurrent_purge_fixture(concurrent_purge_objects(4), false, true, false);
    assert!(delete_candidates(&client, &repo, &payload).is_err());
    assert_eq!(payload, unchanged);
    assert_eq!(state.0.lock().unwrap().deleted.len(), 3);
    let result = delete_candidates(&client, &repo, &payload).unwrap();
    let requests = worker.finish_requests();
    let state = state.0.lock().unwrap();
    assert_eq!(state.maximum, PURGE_WORKERS);
    assert!(state.active.is_empty());
    assert_eq!(state.deleted.len(), PURGE_WORKERS);
    assert_eq!(result["deleted"].as_array().unwrap().len(), 1);
    assert_eq!(result["deleted"][0]["object"], "task/deep/shared/item00");
    assert_eq!(result["already_absent"].as_array().unwrap().len(), 3);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "DELETE")
            .count(),
        5
    );
}

fn oversized_purge_candidates() -> Value {
    // Real candidate fields carry the size: every distinct, exact version ID
    // is 480 ASCII bytes, rather than padding an ignored metadata field.
    let suffix = "v".repeat(475);
    let candidates = (0..6_869)
        .map(|index| {
            json!({"pointer":"task/a.dvc","object":"task/a","version_id":format!("{index:04}-{suffix}")})
        })
        .collect::<Vec<_>>();
    let payload = Value::Array(candidates);
    assert!(serde_json::to_vec(&payload).unwrap().len() > 3 * 1024 * 1024);
    payload
}

#[test]
fn large_purge_adapter_deletes_retry_history_without_argv_or_neighbor_deletion() {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    let (directory, repo) = repo();
    repo.run(["init", "-q"]).unwrap();
    let remote = directory.path().join("empty-remote.git");
    repo.run(["init", "-q", "--bare", remote.to_str().unwrap()])
        .unwrap();
    repo.run(["remote", "add", "origin", remote.to_str().unwrap()])
        .unwrap();
    let payload = oversized_purge_candidates();
    let candidates = payload.as_array().unwrap();
    assert_eq!(candidates.len(), 6_869);
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate["version_id"].as_str().unwrap())
            .collect::<BTreeSet<_>>()
            .len(),
        candidates.len()
    );
    // All other requested versions have already disappeared during a prior
    // attempt. Two payload versions and one delete marker remain to retire.
    let present =
        [0, 3_000, 6_868].map(|index| candidates[index]["version_id"].as_str().unwrap().to_owned());
    let expected_deleted = present.iter().cloned().collect::<BTreeSet<_>>();
    let retired = Arc::new(Mutex::new(BTreeSet::new()));
    let inventory_reads = Arc::new(AtomicUsize::new(0));
    let handler_retired = retired.clone();
    let handler_reads = inventory_reads.clone();
    let handler_present = present.clone();
    let (client, worker) = routed_fixture(move |request| {
        let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
        let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
        if request.method == "POST" {
            let items = batch_delete_items(request);
            assert_eq!(items.len(), 3);
            for (object, version) in &items {
                assert_eq!(object, "root/task/a");
                assert!(
                    handler_present.contains(version),
                    "unrequested deletion: {version}"
                );
                assert!(handler_retired.lock().unwrap().insert(version.clone()));
            }
            return batch_deleted(&items);
        }
        assert_eq!(request.method, "GET");
        if query.contains_key("versioning") {
            return versioning();
        }
        assert!(query.contains_key("versions"));
        let prefix = query.get("prefix").unwrap();
        if *prefix == registry_object() {
            return history("");
        }
        assert_eq!(prefix, "root/task/a");
        handler_reads.fetch_add(1, Ordering::SeqCst);
        let retired = handler_retired.lock().unwrap();
        let mut rows = String::new();
        for version in &handler_present[..2] {
            if !retired.contains(version) {
                rows.push_str(&version_row("root/task/a", version));
            }
        }
        if !retired.contains(&handler_present[2]) {
            rows.push_str(&marker_row("root/task/a", &handler_present[2]));
        }
        // S3 prefix listings also return this neighboring key. It remains in
        // both inventories while the exact target's final history is empty.
        rows.push_str(&version_row("root/task/ab", "neighbor-version"));
        history(&rows)
    });
    configure_repo(&client, &repo);
    let result = crate::storage_metadata::version_purge_adapter(&repo, "delete", &payload).unwrap();
    assert_eq!(result["mode"], "permanent-version-deletion");
    assert_eq!(result["remote"], "workspace-mgr");
    assert_eq!(result["deleted"].as_array().unwrap().len(), 1);
    assert_eq!(result["deleted"][0]["object"], "task/a");
    assert_eq!(result["deleted"][0]["pointer"], "task/a.dvc");
    assert_eq!(
        result["deleted"][0]["deleted_version_ids"],
        json!(expected_deleted.iter().cloned().collect::<Vec<_>>())
    );
    assert_eq!(*retired.lock().unwrap(), expected_deleted);
    assert!(inventory_reads.load(Ordering::SeqCst) >= 2);
    assert_eq!(result["already_absent"].as_array().unwrap().len(), 6_866);
    assert_eq!(result["retained_unmapped"], json!([]));
    let requests = worker.finish_requests();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "POST")
            .count(),
        1
    );
    assert!(
        requests
            .iter()
            .filter(|request| request.method == "POST")
            .all(|request| {
                batch_delete_items(request).iter().all(|(object, version)| {
                    object == "root/task/a" && version != "neighbor-version"
                })
            })
    );
    assert_eq!(requests.last().unwrap().method, "GET");
    assert!(
        requests
            .last()
            .unwrap()
            .target
            .contains("prefix=root%2Ftask%2Fa")
    );
}

#[test]
fn large_purge_adapter_validates_last_candidate_before_any_deletion() {
    let (_directory, repo) = repo();
    let mut payload = oversized_purge_candidates();
    payload.as_array_mut().unwrap().last_mut().unwrap()["version_id"] = json!("");
    assert_eq!(payload.as_array().unwrap().len(), 6_869);
    assert!(serde_json::to_vec(&payload).unwrap().len() > 3 * 1024 * 1024);
    let (client, worker) = routed_fixture(|request| {
        assert_eq!(request.method, "GET");
        if request.target.contains("versioning=") {
            versioning()
        } else {
            assert!(request.target.contains("versions="));
            history("")
        }
    });
    configure_repo(&client, &repo);
    let error = crate::storage_metadata::version_purge_adapter(&repo, "delete", &payload)
        .unwrap_err()
        .to_string();
    assert!(error.contains("missing or invalid version_id"), "{error}");
    let requests = worker.finish_requests();
    assert!(requests.len() >= 4);
    assert!(requests.iter().all(|request| request.method == "GET"));
    for request in requests {
        let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
        let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
        if query.contains_key("versions") {
            assert_eq!(query.get("prefix").unwrap(), &registry_object());
        } else {
            assert!(query.contains_key("versioning"));
        }
    }
}

#[test]
fn archive_purge_rejects_missing_coordination_and_unpublished_private_proof() {
    let (_directory, repo) = repo();
    for proof in [
        None,
        Some(
            json!({"state_path":"/private/journal","mode":"git-cas","transaction_id":"fixture-transaction"}),
        ),
    ] {
        let receipt = copied_receipt();
        let mut payload = json!({"candidates":[{"pointer":"task/.workspace-mgr-archive.json","object":"task/a","version_id":"v1"}]});
        if let Some(proof) = proof {
            payload["coordination"] = json!([{"receipt":receipt,"coordination":proof}]);
        }
        let (client, worker) = routed_fixture(move |request| {
            assert_eq!(request.method, "GET");
            let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
            let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
            if query.contains_key("versions") {
                if query["prefix"] == "root/task/" {
                    history(&version_row("root/task/a", "v1"))
                } else {
                    assert_eq!(query["prefix"], registry_object());
                    registry_listing()
                }
            } else {
                assert_eq!(
                    request.target,
                    format!(
                        "/fixture-bucket/{}?versionId=registry-version",
                        registry_object()
                    )
                );
                registry_body(&receipt)
            }
        });
        let error = delete_candidates(&client, &repo, &payload)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("coordination") || error.contains("published"),
            "{error}"
        );
        let requests = worker.finish_requests();
        assert!(!requests.iter().any(|request| request.method == "DELETE"));
    }
}

fn file_pointer(repo: &GitRepo) {
    fs::create_dir_all(repo.root.join("task")).unwrap();
    fs::write(repo.root.join("task/data.dvc"),"outs:\n- md5: 900150983cd24fb0d6963f7d28e17f72\n  hash: md5\n  size: 3\n  path: data\n  cloud:\n    workspace-mgr:\n      version_id: v1\n      etag: abc\n").unwrap();
}
fn versioning() -> Reply {
    Reply::xml("<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>")
}
fn downloaded(bytes: &[u8], version: &str, tag: &str) -> Reply {
    Reply {
        status: 200,
        headers: vec![
            ("x-amz-version-id", version.to_owned()),
            ("ETag", format!("\"{tag}\"")),
        ],
        body: bytes.to_vec(),
    }
}

fn data_fetch_fixture(replies: Vec<(&str, &str, Reply)>) -> (S3Client, RoutedFixture) {
    let replies = replies
        .into_iter()
        .map(|(method, version, reply)| {
            (
                (
                    method.to_owned(),
                    format!("/fixture-bucket/root/task/data?versionId={version}"),
                ),
                reply,
            )
        })
        .collect::<BTreeMap<_, _>>();
    routed_fixture(move |request| {
        if request.method == "GET" && request.target == "/fixture-bucket?versioning=" {
            return versioning();
        }
        replies
            .get(&(request.method.clone(), request.target.clone()))
            .unwrap_or_else(|| {
                panic!(
                    "unexpected data read: {} {}",
                    request.method, request.target
                )
            })
            .clone()
    })
}

// These fetch operations run serially; adjacent retries preserve the same
// logical read. Keep separate reads across GET/HEAD or version transitions.
fn serial_read_trace(worker: RoutedFixture) -> Vec<WireRequest> {
    let mut requests = worker.finish_requests();
    assert!(
        requests
            .iter()
            .all(|request| matches!(request.method.as_str(), "GET" | "HEAD"))
    );
    requests.dedup_by(|next, previous| {
        next.method == previous.method && next.target == previous.target
    });
    requests
}

#[test]
fn fetch_streams_exact_version_with_ifmatch_and_installs_only_verified_bytes() {
    let (_directory, repo) = repo();
    let (client, worker) = data_fetch_fixture(vec![("GET", "v1", downloaded(b"abc", "v1", "abc"))]);
    configure_repo(&client, &repo);
    file_pointer(&repo);
    let result = read(&repo, &["task/data.dvc".into()], "--fetch", &[]).unwrap();
    assert_eq!(result["checked_objects"], json!(["task/data"]));
    let path = native_engine::cache_path(&repo, "900150983cd24fb0d6963f7d28e17f72").unwrap();
    assert_eq!(fs::read(path).unwrap(), b"abc");
    let requests = serial_read_trace(worker);
    assert_eq!(
        requests[1].target,
        "/fixture-bucket/root/task/data?versionId=v1"
    );
    assert_eq!(requests[1].headers["if-match"], "\"abc\"");
}

#[test]
fn named_remote_git_history_hydrates_original_file_and_directory_versions() {
    let (_directory, repo) = repo();
    let (client, worker) = routed_fixture(|request| {
        if request.target.contains("versioning=") {
            assert_eq!(request.method, "GET");
            return versioning();
        }
        match (request.method.as_str(), request.target.as_str()) {
            ("GET", "/fixture-bucket/root/task/file.bin?versionId=old-file-version") => {
                assert_eq!(request.headers["if-match"], "\"old-file-etag\"");
                downloaded(b"abc", "old-file-version", "old-file-etag")
            }
            ("HEAD", "/fixture-bucket/root/task/file.bin?versionId=old-file-version") => {
                head("old-file-version", "old-file-etag", 3)
            }
            ("GET", "/fixture-bucket/root/task/tree/nested/b.bin?versionId=old-entry-version") => {
                assert_eq!(request.headers["if-match"], "\"old-entry-etag\"");
                downloaded(b"xyz", "old-entry-version", "old-entry-etag")
            }
            ("HEAD", "/fixture-bucket/root/task/tree/nested/b.bin?versionId=old-entry-version") => {
                head("old-entry-version", "old-entry-etag", 3)
            }
            _ => panic!(
                "unexpected historical request: {} {}",
                request.method, request.target
            ),
        }
    });
    configure_repo(&client, &repo);
    repo.run(["config", "user.name", "Fixture"]).unwrap();
    repo.run(["config", "user.email", "fixture@example.invalid"])
        .unwrap();
    let config = crate::config::Config::load_compatible(&repo).unwrap();
    let location = config.s3.as_ref().unwrap();
    let public_config = fs::read(repo.root.join(".workspace-mgr.toml")).unwrap();
    fs::create_dir(repo.root.join(".dvc")).unwrap();
    fs::write(
        repo.root.join(".dvc/config"),
        format!(
            "[core]\nremote = research-data\n['remote \"research-data\"']\nurl = {}\nendpointurl = {}\nversion_aware = true\n",
            location.url,
            location.endpoint_url.as_deref().unwrap(),
        ),
    )
    .unwrap();
    fs::create_dir(repo.root.join("task")).unwrap();
    fs::write(repo.root.join("task/.gitignore"), "/file.bin\n/tree/\n").unwrap();
    let file_pointer = "outs:\n- path: file.bin\n  remote: research-data\n  hash: md5\n  md5: 900150983cd24fb0d6963f7d28e17f72\n  size: 3\n  cloud:\n    research-data:\n      version_id: old-file-version\n      etag: old-file-etag\n";
    let directory_rows =
        b"[{\"md5\": \"d16fb36f0911f878998c136191af705e\", \"relpath\": \"nested/b.bin\"}]";
    let directory_digest = crate::hex::encode_lower(Md5::digest(directory_rows));
    let directory_pointer = format!(
        "outs:\n- path: tree\n  remote: research-data\n  hash: md5\n  md5: {directory_digest}.dir\n  size: 3\n  files:\n  - relpath: nested/b.bin\n    md5: d16fb36f0911f878998c136191af705e\n    size: 3\n    cloud:\n      research-data:\n        version_id: old-entry-version\n        etag: old-entry-etag\n"
    );
    fs::write(repo.root.join("task/file.bin.dvc"), file_pointer).unwrap();
    fs::write(repo.root.join("task/tree.dvc"), &directory_pointer).unwrap();
    repo.run(["add", ".workspace-mgr.toml", ".dvc/config", "task"])
        .unwrap();
    repo.run(["commit", "-q", "-m", "named exact legacy storage"])
        .unwrap();
    let old_oid = repo
        .run(["rev-parse", "HEAD"])
        .unwrap()
        .stdout
        .trim()
        .to_owned();
    repo.run(["rm", ".dvc/config", "task/file.bin.dvc", "task/tree.dvc"])
        .unwrap();
    repo.run(["commit", "-q", "-m", "retire legacy controls"])
        .unwrap();
    let current_oid = repo.run(["rev-parse", "HEAD"]).unwrap().stdout;
    let pointers = vec!["task/file.bin.dvc".to_owned(), "task/tree.dvc".to_owned()];

    // The selected remote must come from this Git revision, even though the
    // primary checkout has already removed its legacy configuration.
    let historical = native_engine::metadata_entries(&repo, Some(&old_oid), &pointers).unwrap();
    assert_eq!(historical.len(), 2);
    assert!(
        historical
            .iter()
            .any(|entry| entry.object == "task/file.bin"
                && entry.version_id.as_deref() == Some("old-file-version"))
    );
    assert!(
        historical
            .iter()
            .any(|entry| entry.object == "task/tree/nested/b.bin"
                && entry.version_id.as_deref() == Some("old-entry-version"))
    );
    let prepared =
        crate::storage_metadata::prepare_revision(&repo, &config, &old_oid, &pointers).unwrap();
    assert_eq!(prepared.prepared_files, pointers);

    let detached = tempfile::tempdir().unwrap();
    let checkout = detached.path().join("historical");
    repo.run([
        "worktree",
        "add",
        "--quiet",
        "--detach",
        checkout.to_str().unwrap(),
        &old_oid,
    ])
    .unwrap();
    let historical_repo = GitRepo {
        root: checkout.clone(),
    };
    crate::storage_metadata::link_private_worktree_state(&repo, &historical_repo).unwrap();
    let report = crate::storage_metadata::hydrate(
        &historical_repo,
        &config,
        &["task".to_owned()],
        &pointers,
        false,
    )
    .unwrap();
    assert_eq!(report.status, "hydrated");
    assert_eq!(fs::read(checkout.join("task/file.bin")).unwrap(), b"abc");
    assert_eq!(
        fs::read(checkout.join("task/tree/nested/b.bin")).unwrap(),
        b"xyz"
    );
    assert_eq!(
        fs::read_to_string(checkout.join("task/file.bin.dvc")).unwrap(),
        file_pointer
    );
    assert_eq!(
        fs::read_to_string(checkout.join("task/tree.dvc")).unwrap(),
        directory_pointer
    );
    repo.run([
        "worktree",
        "remove",
        "--force",
        "--force",
        checkout.to_str().unwrap(),
    ])
    .unwrap();
    assert_eq!(repo.run(["rev-parse", "HEAD"]).unwrap().stdout, current_oid);
    assert_eq!(
        fs::read(repo.root.join(".workspace-mgr.toml")).unwrap(),
        public_config
    );
    assert!(!repo.root.join("task/file.bin").exists());
    assert!(!repo.root.join("task/tree").exists());
    let requests = worker.finish_requests();
    for (object, version) in [
        ("task/file.bin", "old-file-version"),
        ("task/tree/nested/b.bin", "old-entry-version"),
    ] {
        let target = format!("/fixture-bucket/root/{object}?versionId={version}");
        assert!(
            requests
                .iter()
                .filter(|request| request.method == "GET" && request.target == target)
                .count()
                >= 1
        );
        assert!(
            requests
                .iter()
                .filter(|request| request.method == "HEAD" && request.target == target)
                .count()
                >= 1
        );
    }
    assert!(requests.len() >= 8);
}

#[test]
fn exact_fetch_preserves_legacy_binary_and_chunk_boundary_hash_semantics() {
    // Fixed digests from DVC 3.67.1's fobj_md5(..., name="md5-dos2unix").
    for (body, digest) in [
        (b"a\0\r\n".to_vec(), "a200e344e12b35719025ffdb8b428ee8"),
        (
            [vec![b'a'; (1 << 20) - 1], b"\r\nb".to_vec()].concat(),
            "8c5619e93d78797e95a4233d0df49978",
        ),
    ] {
        let (_directory, repo) = repo();
        let (client, worker) = data_fetch_fixture(vec![
            ("GET", "v1", downloaded(&body, "v1", "abc")),
            ("HEAD", "v1", head("v1", "abc", body.len() as u64)),
        ]);
        configure_repo(&client, &repo);
        file_pointer(&repo);
        let raw = format!(
            "outs:\n- md5: {digest}\n  hash: md5-dos2unix\n  size: {}\n  path: data\n  cloud:\n    workspace-mgr:\n      version_id: v1\n      etag: abc\n",
            body.len()
        );
        let pointer = repo.root.join("task/data.dvc");
        fs::write(&pointer, &raw).unwrap();
        let metadata = native_engine::metadata_entries(&repo, None, &["task/data.dvc".into()])
            .unwrap()
            .remove(0);
        for _ in 0..2 {
            read(&repo, &["task/data.dvc".into()], "--fetch", &[]).unwrap();
            let cache = native_engine::cache_path_for_entry(&repo, &metadata).unwrap();
            assert_eq!(fs::read(cache).unwrap(), body);
            assert_eq!(fs::read_to_string(&pointer).unwrap(), raw);
        }
        let requests = serial_read_trace(worker);
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[1].method, "GET");
        assert_eq!(requests[3].method, "HEAD");
        assert_eq!(requests[1].headers["if-match"], "\"abc\"");
    }
}

#[test]
fn fetch_preserves_corrupt_cache_when_new_get_metadata_or_hash_disagrees() {
    for reply in [
        downloaded(b"abc", "different", "abc"),
        downloaded(b"abc", "v1", "wrong"),
        downloaded(b"abd", "v1", "abc"),
    ] {
        let (_directory, repo) = repo();
        let (client, worker) = data_fetch_fixture(vec![("GET", "v1", reply)]);
        configure_repo(&client, &repo);
        file_pointer(&repo);
        let path = native_engine::cache_path(&repo, "900150983cd24fb0d6963f7d28e17f72").unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"old-corrupt").unwrap();
        assert!(read(&repo, &["task/data.dvc".into()], "--fetch", &[]).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"old-corrupt");
        worker.finish_requests();
    }
}

#[test]
fn valid_cache_bytes_still_require_remote_exact_version() {
    let (_directory, repo) = repo();
    let (client, worker) = data_fetch_fixture(vec![("HEAD", "v1", missing("AccessDenied", 403))]);
    configure_repo(&client, &repo);
    file_pointer(&repo);
    native_engine::install_cache(&repo, "900150983cd24fb0d6963f7d28e17f72", b"abc").unwrap();
    assert!(
        read(&repo, &["task/data.dvc".into()], "--fetch", &[])
            .unwrap_err()
            .to_string()
            .contains("AccessDenied")
    );
    let requests = serial_read_trace(worker);
    assert_eq!(requests[1].method, "HEAD");
    assert_eq!(requests.len(), 2);
}

#[test]
fn legacy_cache_layout_needs_exact_head_verification_without_a_get() {
    let (_directory, repo) = repo();
    let (client, worker) = data_fetch_fixture(vec![("HEAD", "v1", head("v1", "abc", 3))]);
    configure_repo(&client, &repo);
    file_pointer(&repo);
    let digest = "900150983cd24fb0d6963f7d28e17f72";
    let legacy = native_engine::cache_root(&repo)
        .unwrap()
        .join("90/0150983cd24fb0d6963f7d28e17f72");
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    fs::write(&legacy, b"abc").unwrap();
    assert!(!native_engine::cache_path(&repo, digest).unwrap().exists());
    read(&repo, &["task/data.dvc".into()], "--fetch", &[]).unwrap();
    assert_eq!(fs::read(&legacy).unwrap(), b"abc");
    let requests = serial_read_trace(worker);
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].method, "HEAD");
    assert_eq!(
        requests[1].target,
        "/fixture-bucket/root/task/data?versionId=v1"
    );
}

#[test]
fn omitted_legacy_hash_preserves_cache_namespace_and_exact_payload_bytes() {
    let (_directory, repo) = repo();
    let body = b"a\r\nb\r\n";
    let digest = "dd8c6a395b5dd36c56d23275028f526c";
    let (client, worker) = data_fetch_fixture(vec![
        ("GET", "v1", downloaded(body, "v1", "abc")),
        ("HEAD", "v1", head("v1", "abc", 6)),
    ]);
    configure_repo(&client, &repo);
    file_pointer(&repo);
    // Real DVC 2 pointers omit hash. Their normalized digest also names a
    // different raw-MD5 cache object; preserve both byte representations.
    let raw = format!(
        "outs:\n- md5: {digest}\n  size: 6\n  path: data\n  cloud:\n    workspace-mgr:\n      version_id: v1\n      etag: abc\n"
    );
    let pointer = repo.root.join("task/data.dvc");
    fs::write(&pointer, &raw).unwrap();
    native_engine::install_cache(&repo, digest, b"a\nb\n").unwrap();
    let canonical = native_engine::cache_path(&repo, digest).unwrap();
    let metadata = native_engine::metadata_entries(&repo, None, &["task/data.dvc".into()])
        .unwrap()
        .remove(0);
    let legacy = native_engine::cache_path_for_entry(&repo, &metadata).unwrap();
    for _ in 0..2 {
        read(&repo, &["task/data.dvc".into()], "--fetch", &[]).unwrap();
        assert_eq!(fs::read(&canonical).unwrap(), b"a\nb\n");
        assert_eq!(fs::read(&legacy).unwrap(), body);
        assert_eq!(fs::read_to_string(&pointer).unwrap(), raw);
    }
    let requests = serial_read_trace(worker);
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[1].method, "GET");
    assert_eq!(requests[3].method, "HEAD");
}

fn normalized_file_pointer(repo: &GitRepo, version: &str, tag: &str) -> String {
    let pointer = "task/data.wm-storage.json";
    fs::create_dir_all(repo.root.join("task")).unwrap();
    let raw = serde_json::to_string_pretty(&json!({
        "schema_version": 1,
        "path": "data",
        "kind": "file",
        "checksum": {
            "algorithm": "md5-dos2unix",
            "digest": "dd8c6a395b5dd36c56d23275028f526c"
        },
        "size": 5,
        "version": { "id": version, "etag": tag }
    }))
    .unwrap();
    fs::write(repo.root.join(pointer), raw).unwrap();
    pointer.into()
}

#[test]
fn exact_versions_with_same_normalized_hash_and_size_keep_distinct_raw_caches() {
    let (_directory, repo) = repo();
    let first = b"a\r\nb\n";
    let second = b"a\nb\r\n";
    let digest = "dd8c6a395b5dd36c56d23275028f526c";
    let (client, worker) = data_fetch_fixture(vec![
        ("GET", "version-a", downloaded(first, "version-a", "raw-a")),
        ("GET", "version-b", downloaded(second, "version-b", "raw-b")),
        ("HEAD", "version-a", head("version-a", "raw-a", 5)),
    ]);
    configure_repo(&client, &repo);
    // A normalized checksum and size cannot prove which raw S3 version a
    // generic cache object contains, even when the normalized hash is valid.
    let generic = native_engine::cache_path_with_algorithm(&repo, digest, "md5-dos2unix").unwrap();
    fs::create_dir_all(generic.parent().unwrap()).unwrap();
    fs::write(&generic, second).unwrap();

    let pointer = normalized_file_pointer(&repo, "version-a", "raw-a");
    read(&repo, std::slice::from_ref(&pointer), "--fetch", &[]).unwrap();
    let first_entry = native_engine::metadata_entries(&repo, None, std::slice::from_ref(&pointer))
        .unwrap()
        .remove(0);
    let first_cache = native_engine::cache_path_for_entry(&repo, &first_entry).unwrap();
    assert_eq!(fs::read(&first_cache).unwrap(), first);

    normalized_file_pointer(&repo, "version-b", "raw-b");
    read(&repo, std::slice::from_ref(&pointer), "--fetch", &[]).unwrap();
    let second_entry = native_engine::metadata_entries(&repo, None, std::slice::from_ref(&pointer))
        .unwrap()
        .remove(0);
    let second_cache = native_engine::cache_path_for_entry(&repo, &second_entry).unwrap();
    assert_ne!(first_cache, second_cache);
    assert_eq!(fs::read(&first_cache).unwrap(), first);
    assert_eq!(fs::read(&second_cache).unwrap(), second);

    normalized_file_pointer(&repo, "version-a", "raw-a");
    read(&repo, std::slice::from_ref(&pointer), "--fetch", &[]).unwrap();
    assert_eq!(fs::read(&first_cache).unwrap(), first);
    assert_eq!(fs::read(&second_cache).unwrap(), second);
    assert_eq!(fs::read(&generic).unwrap(), second);
    let requests = serial_read_trace(worker);
    assert_eq!(requests.len(), 6);
    assert_eq!(requests[1].method, "GET");
    assert_eq!(
        requests[1].target,
        "/fixture-bucket/root/task/data?versionId=version-a"
    );
    assert_eq!(requests[1].headers["if-match"], "\"raw-a\"");
    assert_eq!(requests[3].method, "GET");
    assert_eq!(
        requests[3].target,
        "/fixture-bucket/root/task/data?versionId=version-b"
    );
    assert_eq!(requests[3].headers["if-match"], "\"raw-b\"");
    assert_eq!(requests[5].method, "HEAD");
    assert_eq!(
        requests[5].target,
        "/fixture-bucket/root/task/data?versionId=version-a"
    );
}

#[test]
fn checkout_replaces_existing_normalized_raw_variant_with_incoming_exact_version() {
    let (_directory, repo) = repo();
    let first = b"a\r\nb\n";
    let second = b"a\nb\r\n";
    let (client, worker) = data_fetch_fixture(vec![
        ("GET", "version-a", downloaded(first, "version-a", "raw-a")),
        ("GET", "version-b", downloaded(second, "version-b", "raw-b")),
    ]);
    configure_repo(&client, &repo);
    let pointer = normalized_file_pointer(&repo, "version-a", "raw-a");
    for (version, tag, bytes) in [
        ("version-a", "raw-a", first),
        ("version-b", "raw-b", second),
    ] {
        normalized_file_pointer(&repo, version, tag);
        read(&repo, std::slice::from_ref(&pointer), "--fetch", &[]).unwrap();
        let raw = fs::read(repo.root.join(&pointer)).unwrap();
        let result = native_engine::execute(
            &repo.root,
            &native_engine::Operation::Materialize {
                pointers: vec![pointer.clone()],
            },
        )
        .unwrap();
        assert_eq!(result.code, 0, "{}", result.stderr);
        assert_eq!(fs::read(repo.root.join("task/data")).unwrap(), bytes);
        assert_eq!(fs::read(repo.root.join(&pointer)).unwrap(), raw);
    }
    let requests = serial_read_trace(worker);
    assert_eq!(requests.len(), 4);
    assert_eq!(
        requests[1].target,
        "/fixture-bucket/root/task/data?versionId=version-a"
    );
    assert_eq!(
        requests[3].target,
        "/fixture-bucket/root/task/data?versionId=version-b"
    );
}

fn published_proof(repo: &GitRepo, remote: &Path, receipt: &Value) -> Value {
    repo.run(["init", "-q", "-b", "main"]).unwrap();
    repo.run(["config", "user.name", "Fixture"]).unwrap();
    repo.run(["config", "user.email", "fixture@example.invalid"])
        .unwrap();
    repo.run(["init", "-q", "--bare", remote.to_str().unwrap()])
        .unwrap();
    repo.run(["remote", "add", "origin", remote.to_str().unwrap()])
        .unwrap();
    fs::write(
        repo.root.join(".workspace-mgr.toml"),
        "[git]\nremote='origin'\nbranch='main'\n",
    )
    .unwrap();
    fs::create_dir_all(repo.root.join("archive/task")).unwrap();
    fs::write(
        repo.root.join("archive/task/.workspace-mgr-archive.json"),
        serde_json::to_vec_pretty(receipt).unwrap(),
    )
    .unwrap();
    repo.run(["add", "."]).unwrap();
    repo.run(["commit", "-q", "-m", "published fixture"])
        .unwrap();
    repo.run(["push", "-q", "origin", "main"]).unwrap();
    crate::archive_registry::coordinate_published(repo, receipt).unwrap()
}

#[derive(Clone, Copy)]
enum DestinationGuardFault {
    None,
    MissingPayload,
    MissingMarker,
    EmptyInventory,
    ChangedRegistry,
    BindingAfterDelete,
}

fn generic_destination_guard_fixture(
    fault: DestinationGuardFault,
    submit_copies: bool,
) -> (Value, Vec<WireRequest>) {
    let (directory, repo) = repo();
    let mut receipt = copied_receipt();
    receipt["task_id"] = "task".into();
    receipt["versions"][0]["source_is_latest"] = false.into();
    receipt["versions"]
        .as_array_mut()
        .unwrap()
        .push(json!({"source_object":"task/a",
        "destination_object":"archive/task/a","source_version_id":"source-marker",
        "source_last_modified":"2026-10-07T00:00:01+00:00","source_is_latest":true,
        "source_list_order":1,"delete_marker":true,"size":null,"source_etag":null,
        "destination_version_id":"dst-marker","destination_etag":null,
        "destination_last_modified":"2026-10-07T00:00:02+00:00"}));
    published_proof(&repo, &directory.path().join("remote.git"), &receipt);
    // Keep the canonical claim while removing the receipt from every live
    // tree. The same claim also exists before a task branch is merged.
    repo.run(["rm", "archive/task/.workspace-mgr-archive.json"])
        .unwrap();
    repo.run(["commit", "-q", "-m", "remove live receipt"])
        .unwrap();
    repo.run(["push", "-q", "origin", "main"]).unwrap();
    // A second unrelated control catches snapshot ordering by OID instead of ref.
    let mut other = receipt.clone();
    other["source"] = "other".into();
    other["destination"] = "archive/other".into();
    for row in other["versions"].as_array_mut().unwrap() {
        row["source_object"] = "other/a".into();
        row["destination_object"] = "archive/other/a".into();
    }
    let other_ref = crate::archive_registry::binding_ref(&other).unwrap();
    let receipt_ref = crate::archive_registry::binding_ref(&receipt).unwrap();
    let receipt_oid = crate::archive_git_control::object_ids(
        &repo.root,
        &serde_json::to_string(&receipt).unwrap(),
        false,
    )
    .unwrap();
    let oid = (0..100)
        .find_map(|nonce| {
            other["transaction_id"] = format!("other-{nonce}").into();
            let control = crate::archive_git_control::object_ids(
                &repo.root,
                &serde_json::to_string(&other).unwrap(),
                true,
            )
            .unwrap();
            (control.commit.cmp(&receipt_oid.commit) != other_ref.cmp(&receipt_ref))
                .then_some(control)
        })
        .expect("fixture controls have opposite ref and OID sort order");
    repo.run([
        "push",
        "-q",
        "origin",
        &format!("{}:{other_ref}", oid.commit),
    ])
    .unwrap();
    let root = repo.root.clone();
    let reference = crate::archive_registry::binding_ref(&receipt).unwrap();
    let retired = std::sync::atomic::AtomicBool::new(false);
    let stored_receipt = receipt.clone();
    let (client, worker) = routed_fixture(move |request| {
        if request.method == "DELETE" {
            assert_eq!(
                request.target,
                "/fixture-bucket/root/archive/task/a?versionId=later-edit"
            );
            retired.store(true, Ordering::SeqCst);
            if matches!(fault, DestinationGuardFault::BindingAfterDelete) {
                GitRepo { root: root.clone() }
                    .run(["push", "-q", "origin", &format!(":{reference}")])
                    .unwrap();
            }
            return deleted();
        }
        if request.method == "HEAD" {
            return head("dst", "copied", 3);
        }
        let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
        let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
        if query.contains_key("versioning") {
            return versioning();
        }
        if query.contains_key("versions") {
            if query["prefix"] == registry_object() {
                return registry_listing();
            }
            if query["prefix"] != "root/archive/task/a" {
                return history("");
            }
            if matches!(fault, DestinationGuardFault::EmptyInventory) {
                return history("");
            }
            let mut rows = String::new();
            if !matches!(fault, DestinationGuardFault::MissingPayload) {
                rows.push_str(
                    &version_row("root/archive/task/a", "dst")
                        .replace("&quot;abc&quot;", "&quot;copied&quot;"),
                );
            }
            if !matches!(fault, DestinationGuardFault::MissingMarker) {
                rows.push_str(&marker_row("root/archive/task/a", "dst-marker"));
            }
            if !retired.load(Ordering::SeqCst) {
                rows.push_str(&version_row("root/archive/task/a", "later-edit"));
            }
            return history(&rows);
        }
        let mut canonical = stored_receipt.clone();
        if matches!(fault, DestinationGuardFault::ChangedRegistry) {
            canonical["transaction_id"] = "replaced".into();
        }
        registry_body(&canonical)
    });
    configure_repo(&client, &repo);
    let mut candidates = vec![
        json!({"pointer":"archive/task/a.wm-storage.json","object":"archive/task/a","version_id":"later-edit"}),
    ];
    if submit_copies {
        for version in ["dst", "dst-marker"] {
            candidates.push(json!({"pointer":"archive/task/a.dvc","object":"archive/task/a","version_id":version}));
        }
    }
    let result = purge(&repo, "delete", &json!(candidates));
    let requests = worker.finish_requests();
    match fault {
        DestinationGuardFault::None => (result.unwrap(), requests),
        _ => {
            let error = result.unwrap_err().to_string();
            assert!(
                error.contains("retained archive destination") || error.contains("claims changed"),
                "{error}"
            );
            if !matches!(fault, DestinationGuardFault::BindingAfterDelete) {
                assert!(
                    requests
                        .iter()
                        .all(|request| request.method != "DELETE" && request.method != "POST")
                );
            }
            (Value::Null, requests)
        }
    }
}

#[test]
fn generic_retirement_preserves_canonical_destination_history_without_a_live_receipt() {
    for submit_copies in [false, true] {
        let (result, requests) =
            generic_destination_guard_fixture(DestinationGuardFault::None, submit_copies);
        assert_eq!(
            result["deleted"][0]["deleted_version_ids"],
            json!(["later-edit"])
        );
        assert_eq!(
            result["retained_mapped"].as_array().unwrap().len(),
            if submit_copies { 2 } else { 0 }
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.method == "DELETE")
                .count(),
            1
        );
    }
}

#[test]
fn generic_destination_retirement_fails_closed_when_canonical_copies_are_missing_or_changed() {
    for fault in [
        DestinationGuardFault::MissingPayload,
        DestinationGuardFault::MissingMarker,
        DestinationGuardFault::EmptyInventory,
        DestinationGuardFault::ChangedRegistry,
        DestinationGuardFault::BindingAfterDelete,
    ] {
        generic_destination_guard_fixture(fault, false);
    }
}

#[test]
fn published_archive_purge_deletes_only_mapped_versions_and_retains_new_writes() {
    published_archive_purge(false);
}

#[test]
fn published_archive_purge_replays_interrupted_reads_without_repeating_deletes() {
    published_archive_purge(true);
}

fn published_archive_purge(interrupt: bool) {
    use crate::native_s3::tests::{interrupt_response_once, routed_fixture};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    };
    let (directory, repo) = repo();
    let mut receipt = copied_receipt();
    receipt["versions"][0]["source_is_latest"] = false.into();
    receipt["versions"].as_array_mut().unwrap().push(json!({"source_object":"task/a","destination_object":"archive/task/a","source_version_id":"d1","source_last_modified":"2026-10-07T00:00:01+00:00","source_is_latest":true,"source_list_order":1,"delete_marker":true,"size":null,"source_etag":null,"destination_version_id":"dst-delete","destination_etag":null,"destination_last_modified":"2026-10-07T00:00:02+00:00"}));
    let proof = published_proof(&repo, &directory.path().join("remote.git"), &receipt);
    let deleted_payload = Arc::new(AtomicBool::new(false));
    let deleted_marker = Arc::new(AtomicBool::new(false));
    let payload_flag = deleted_payload.clone();
    let marker_flag = deleted_marker.clone();
    let stored_receipt = receipt.clone();
    let registry_target = format!(
        "/fixture-bucket/{}?versionId=registry-version",
        registry_object()
    );
    let interrupted_target = registry_target.clone();
    let first_registry_read = AtomicBool::new(true);
    let (request_sent, request_received) = mpsc::channel();
    // Keep serving until the operation completes: an abandoned GET may replay,
    // while the exact-version batch must still occur exactly once.
    let (client, worker) = routed_fixture(move |request| {
        if request.method == "POST" {
            let items = batch_delete_items(request);
            assert_eq!(items.len(), 2);
            for (object, version) in &items {
                assert_eq!(object, "root/task/a");
                match version.as_str() {
                    "v1" => assert!(!payload_flag.swap(true, Ordering::SeqCst)),
                    "d1" => assert!(!marker_flag.swap(true, Ordering::SeqCst)),
                    _ => panic!("attempted unmapped version deletion: {version}"),
                }
            }
            return batch_deleted(&items);
        }
        if request.method == "HEAD" {
            assert_eq!(
                request.target,
                "/fixture-bucket/root/archive/task/a?versionId=dst"
            );
            return head("dst", "copied", 3);
        }
        if request.target.contains("versions=") {
            if request
                .target
                .contains("prefix=root%2F.workspace-mgr%2Farchive%2F")
            {
                return registry_listing();
            }
            if request.target.contains("prefix=root%2Farchive%2Ftask%2F") {
                return history(&format!(
                    "{}{}",
                    version_row("root/archive/task/a", "dst")
                        .replace("&quot;abc&quot;", "&quot;copied&quot;"),
                    marker_row("root/archive/task/a", "dst-delete")
                ));
            }
            let mut rows = String::new();
            if !payload_flag.load(Ordering::SeqCst) {
                rows.push_str(&version_row("root/task/a", "v1"));
            }
            if !marker_flag.load(Ordering::SeqCst) {
                rows.push_str(&marker_row("root/task/a", "d1"));
            }
            rows.push_str(&version_row("root/task/b", "independent-new-version"));
            rows.push_str(&marker_row("root/task/b", "independent-new-marker"));
            return history(&rows);
        }
        assert_eq!(request.method, "GET");
        assert_eq!(request.target, interrupted_target);
        if interrupt && first_registry_read.swap(false, Ordering::SeqCst) {
            request_sent.send(()).unwrap();
        }
        registry_body(&stored_receipt)
    });
    let (client, injected) = if interrupt {
        interrupt_response_once(client, registry_target.clone(), request_received)
    } else {
        (client, Arc::new(AtomicUsize::new(0)))
    };
    let payload = json!({"candidates":[{"pointer":"task/.workspace-mgr-archive.json","object":"task/a","version_id":"v1"},{"pointer":"task/.workspace-mgr-archive.json","object":"task/a","version_id":"d1"}],"prefixes":[receipt],"coordination":[{"receipt":receipt,"coordination":proof}]});
    let result = delete_candidates(&client, &repo, &payload).unwrap();
    assert!(deleted_payload.load(Ordering::SeqCst));
    assert!(deleted_marker.load(Ordering::SeqCst));
    assert_eq!(result["retained_unmapped"].as_array().unwrap().len(), 2);
    assert_eq!(result["cleaned_prefixes"], json!([]));
    let attempts = worker.finish();
    let deletions = attempts
        .iter()
        .filter(|attempt| attempt.request.method == "POST")
        .collect::<Vec<_>>();
    assert_eq!(deletions.len(), 1);
    assert!(deletions[0].response_sent);
    assert_eq!(
        batch_delete_items(&deletions[0].request),
        [
            ("root/task/a".into(), "d1".into()),
            ("root/task/a".into(), "v1".into())
        ]
    );
    assert_eq!(injected.load(Ordering::SeqCst), usize::from(interrupt));
    let registry_reads = attempts
        .iter()
        .filter(|attempt| attempt.request.target == registry_target)
        .count();
    assert_eq!(registry_reads, 2 + usize::from(interrupt));
    assert_eq!(
        attempts
            .iter()
            .filter(|attempt| attempt.request.method == "HEAD")
            .count(),
        2,
        "one destination HEAD per full verification pass"
    );
}

#[derive(Clone, Copy)]
enum ArchiveBatchFault {
    None,
    RegistryAfterBatch,
    DestinationAfterBatch,
    IncompleteBatch,
    LostBatchResponse,
    ProtectedSibling,
    NullSource,
    SourceWriteDuringFinalHead,
}

fn archive_batch_fixture(
    count: usize,
    fault: ArchiveBatchFault,
) -> (Result<Value>, Vec<WireRequest>, usize) {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };

    let (directory, repo) = repo();
    let mut receipt = copied_receipt();
    let original = receipt["versions"][0].clone();
    receipt["versions"] = (0..count)
        .map(|index| {
            let mut row = original.clone();
            row["source_object"] = format!("task/a{index:04}").into();
            row["destination_object"] = format!("archive/task/a{index:04}").into();
            row["source_version_id"] =
                if index == 0 && matches!(fault, ArchiveBatchFault::NullSource) {
                    "null".into()
                } else {
                    format!("v{index}").into()
                };
            row["destination_version_id"] = format!("dst{index}").into();
            row["source_list_order"] = index.into();
            row
        })
        .collect::<Vec<_>>()
        .into();
    let proof = published_proof(&repo, &directory.path().join("remote.git"), &receipt);
    let stored = receipt.clone();
    let completed = Arc::new(Mutex::new(BTreeSet::new()));
    let completed_worker = completed.clone();
    let new_source = Arc::new(AtomicBool::new(false));
    let new_source_worker = new_source.clone();
    let (request_sent, request_received) = mpsc::channel();
    let (client, worker) = routed_fixture(move |request| {
        let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
        let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
        if request.method == "POST" {
            let items = batch_delete_items(request);
            let mut completed = completed_worker.lock().unwrap();
            let first = completed.is_empty();
            for (object, version) in &items {
                let index = version.strip_prefix('v').unwrap().parse::<usize>().unwrap();
                assert!(index < count);
                assert_eq!(object, &format!("root/task/a{index:04}"));
                let inserted = completed.insert((object.clone(), version.clone()));
                assert!(inserted || matches!(fault, ArchiveBatchFault::LostBatchResponse));
            }
            if first && matches!(fault, ArchiveBatchFault::LostBatchResponse) {
                request_sent.send(()).unwrap();
            }
            return if matches!(fault, ArchiveBatchFault::IncompleteBatch) {
                batch_deleted(&items[..items.len() - 1])
            } else {
                batch_deleted(&items)
            };
        }
        if request.method == "DELETE" {
            let object = url.path().strip_prefix("/fixture-bucket/").unwrap();
            let version = &query["versionId"];
            if version == "null" {
                assert!(matches!(fault, ArchiveBatchFault::NullSource));
                assert_eq!(object, "root/task/a0000");
            }
            assert!(
                completed_worker
                    .lock()
                    .unwrap()
                    .insert((object.into(), version.clone()))
            );
            return deleted();
        }
        if request.method == "HEAD" {
            let version = &query["versionId"];
            let index = version
                .strip_prefix("dst")
                .unwrap()
                .parse::<usize>()
                .unwrap();
            assert_eq!(
                url.path(),
                format!("/fixture-bucket/root/archive/task/a{index:04}")
            );
            if matches!(fault, ArchiveBatchFault::SourceWriteDuringFinalHead)
                && !completed_worker.lock().unwrap().is_empty()
            {
                new_source_worker.store(true, Ordering::SeqCst);
            }
            return head(version, "copied", 3);
        }
        let completed = completed_worker.lock().unwrap();
        if query.contains_key("versions") {
            let prefix = &query["prefix"];
            if prefix == &registry_object() {
                return registry_listing();
            }
            if prefix == "root/archive/task/" {
                let skip = usize::from(
                    matches!(fault, ArchiveBatchFault::DestinationAfterBatch)
                        && !completed.is_empty(),
                );
                let rows = (skip..count)
                    .map(|index| {
                        version_row(
                            &format!("root/archive/task/a{index:04}"),
                            &format!("dst{index}"),
                        )
                        .replace("&quot;abc&quot;", "&quot;copied&quot;")
                        .replace("<IsLatest>false</IsLatest>", "<IsLatest>true</IsLatest>")
                    })
                    .collect::<String>();
                return history(&rows);
            }
            assert_eq!(prefix, "root/task/");
            let version_for = |index| {
                if index == 0 && matches!(fault, ArchiveBatchFault::NullSource) {
                    "null".to_owned()
                } else {
                    format!("v{index}")
                }
            };
            let mut rows = (0..count)
                .filter(|index| {
                    !completed.contains(&(format!("root/task/a{index:04}"), version_for(*index)))
                })
                .map(|index| version_row(&format!("root/task/a{index:04}"), &version_for(index)))
                .collect::<String>();
            if new_source_worker.load(Ordering::SeqCst) {
                rows.push_str(&version_row("root/task/concurrent", "late-source-version"));
            }
            return history(&rows);
        }
        let mut current = stored.clone();
        if matches!(fault, ArchiveBatchFault::RegistryAfterBatch) && !completed.is_empty() {
            current["transaction_id"] = "foreign-transaction".into();
        }
        registry_body(&current)
    });
    let client = if matches!(fault, ArchiveBatchFault::LostBatchResponse) {
        crate::native_s3::tests::interrupt_response_once(
            client,
            "/fixture-bucket?delete=".into(),
            request_received,
        )
        .0
    } else {
        client
    };
    let mut candidates = receipt["versions"]
        .as_array()
        .unwrap()
        .iter()
        .take(count - usize::from(matches!(fault, ArchiveBatchFault::ProtectedSibling)))
        .map(|row| json!({"pointer":"task/.workspace-mgr-archive.json","object":row["source_object"],"version_id":row["source_version_id"]}))
        .collect::<Vec<_>>();
    candidates.push(json!({"pointer":"task/.workspace-mgr-archive.json","object":"task/a0000","version_id":"already-absent"}));
    let result = delete_candidates_with_catalog(
        &client,
        &repo,
        &json!({"candidates":candidates,"prefixes":[receipt],"coordination":[{"receipt":receipt,"coordination":proof}]}),
        || panic!("archive-only retirement must not fetch a generic destination catalog"),
    );
    let requests = worker.finish_requests();
    let deleted = completed.lock().unwrap().len();
    (result, requests, deleted)
}

#[test]
fn published_archive_batches_large_prefix_with_linear_destination_verification() {
    let count = 512;
    let (result, requests, deleted) = archive_batch_fixture(count, ArchiveBatchFault::None);
    assert_eq!(deleted, count);
    let result = result.unwrap();
    assert_eq!(result["cleaned_prefixes"], json!(["task"]));
    assert_eq!(
        result["already_absent"],
        json!([{"pointer":"task/.workspace-mgr-archive.json","object":"task/a0000","version_id":"already-absent"}])
    );
    assert!(result["retained_unmapped"].as_array().unwrap().is_empty());
    let batches = requests
        .iter()
        .filter(|request| request.method == "POST")
        .collect::<Vec<_>>();
    assert_eq!(batches.len(), 1);
    assert_eq!(batch_delete_items(batches[0]).len(), count);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "HEAD")
            .count(),
        count * 2
    );
}

#[test]
fn published_archive_rechecks_changed_registry_before_next_batch() {
    let (result, requests, deleted) =
        archive_batch_fixture(1001, ArchiveBatchFault::RegistryAfterBatch);
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("canonical registry")
    );
    assert_eq!(deleted, 1000);
    assert_eq!(
        requests
            .iter()
            .filter(|request| matches!(request.method.as_str(), "POST" | "DELETE"))
            .count(),
        1
    );
}

#[test]
fn published_archive_does_not_report_clean_prefix_after_destination_loss() {
    let (result, _, deleted) = archive_batch_fixture(4, ArchiveBatchFault::DestinationAfterBatch);
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("lost a previously copied")
    );
    assert_eq!(deleted, 4);
}

#[test]
fn published_archive_incomplete_batch_acknowledgement_fails_without_replay() {
    let (result, requests, deleted) = archive_batch_fixture(4, ArchiveBatchFault::IncompleteBatch);
    assert!(result.is_err());
    assert_eq!(deleted, 4);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "POST")
            .count(),
        1
    );
}

#[test]
fn published_archive_lost_batch_response_retries_only_identical_exact_versions() {
    let (result, requests, deleted) =
        archive_batch_fixture(4, ArchiveBatchFault::LostBatchResponse);
    assert_eq!(result.unwrap()["cleaned_prefixes"], json!(["task"]));
    assert_eq!(deleted, 4);
    let batches = requests
        .iter()
        .filter(|request| request.method == "POST")
        .collect::<Vec<_>>();
    assert_eq!(batches.len(), 2);
    assert_eq!(batches[0].target, batches[1].target);
    assert_eq!(batches[0].body, batches[1].body);
}

#[test]
fn published_archive_preserves_unrequested_mapped_sibling_as_mapped_retention() {
    let (result, requests, deleted) = archive_batch_fixture(4, ArchiveBatchFault::ProtectedSibling);
    let result = result.unwrap();
    assert_eq!(deleted, 3);
    assert_eq!(result["cleaned_prefixes"], json!([]));
    assert!(result["retained_unmapped"].as_array().unwrap().is_empty());
    assert_eq!(
        result["retained_mapped"],
        json!([{
            "pointer":"task/.workspace-mgr-archive.json",
            "object":"task/a0003",
            "version_id":"v3"
        }])
    );
    let batch = requests
        .iter()
        .find(|request| request.method == "POST")
        .unwrap();
    assert_eq!(batch_delete_items(batch).len(), 3);
}

#[test]
fn published_archive_retires_pre_versioning_null_source_with_explicit_guarded_delete() {
    let (result, requests, deleted) = archive_batch_fixture(4, ArchiveBatchFault::NullSource);
    assert_eq!(result.unwrap()["cleaned_prefixes"], json!(["task"]));
    assert_eq!(deleted, 4);
    let single = requests
        .iter()
        .filter(|request| request.method == "DELETE")
        .collect::<Vec<_>>();
    assert_eq!(single.len(), 1);
    assert_eq!(
        single[0].target,
        "/fixture-bucket/root/task/a0000?versionId=null"
    );
    let batch = requests
        .iter()
        .find(|request| request.method == "POST")
        .unwrap();
    assert_eq!(batch_delete_items(batch).len(), 3);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "HEAD")
            .count(),
        12
    );
}

#[test]
fn published_archive_final_inventory_retains_write_during_final_destination_heads() {
    let (result, _, deleted) =
        archive_batch_fixture(4, ArchiveBatchFault::SourceWriteDuringFinalHead);
    let result = result.unwrap();
    assert_eq!(deleted, 4);
    assert_eq!(result["cleaned_prefixes"], json!([]));
    assert_eq!(
        result["retained_unmapped"],
        json!([{
            "pointer":"task/.workspace-mgr-archive.json",
            "object":"task/concurrent",
            "version_id":"late-source-version"
        }])
    );
}
