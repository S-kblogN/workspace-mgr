//! Adapter regressions through the native HTTP client, using loopback only.
use super::*;
use crate::native_s3::tests::{Reply, configure_repo, fixture, fixture_handler};
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
            pointer: format!("{object}.dvc"),
            object: object.to_owned(),
            md5: Some("900150983cd24fb0d6963f7d28e17f72".to_owned()),
            size: Some(3),
            version_id: Some("v1".to_owned()),
            etag: Some("abc".to_owned()),
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
    let (client, worker) = fixture_handler(5, move |request| {
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
    let requests = worker.join().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "HEAD")
            .count(),
        2
    );
    assert!(
        requests
            .iter()
            .any(|request| request.target == "/fixture-bucket/root/archive/task/a?versionId=dst")
    );
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
        let (client, worker) = fixture_handler(4, move |request| {
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
        let requests = worker.join().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.method == "HEAD")
                .count(),
            1
        );
    }
}

#[test]
fn dense_listing_stops_at_two_pages_and_falls_back_to_exact_heads() {
    let (_directory, repo) = repo();
    let (client, worker) = fixture_handler(10, |request| {
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
    let requests = worker.join().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "GET")
            .count(),
        2
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "HEAD")
            .count(),
        8
    );
}

#[test]
fn denied_dense_listing_uses_readable_exact_heads() {
    let (_directory, repo) = repo();
    let (client, worker) = fixture_handler(9, |request| {
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
    let requests = worker.join().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "GET")
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "HEAD")
            .count(),
        8
    );
}

#[test]
fn network_read_workers_are_bounded_to_sixteen() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let (_directory, repo) = repo();
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let worker_active = active.clone();
    let worker_maximum = maximum.clone();
    let (client, worker) = fixture_handler(40, move |request| {
        assert_eq!(request.method, "HEAD");
        let count = worker_active.fetch_add(1, Ordering::SeqCst) + 1;
        worker_maximum.fetch_max(count, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(35));
        worker_active.fetch_sub(1, Ordering::SeqCst);
        head("v1", "abc", 3)
    });
    // Sparse prefixes require HEAD; no listing can conceal an unbounded pool.
    let entries = (0..40)
        .map(|index| entry(&format!("task{index}/a")))
        .collect::<Vec<_>>();
    verify_entries(&client, &repo, &entries).unwrap();
    assert_eq!(worker.join().unwrap().len(), 40);
    assert!(maximum.load(Ordering::SeqCst) <= 16);
    assert!(maximum.load(Ordering::SeqCst) > 1);
}

#[test]
fn sparse_versions_use_exact_heads_and_metadata_mismatch_fails() {
    let (_dir, repo) = repo();
    let (client, worker) = fixture(vec![head("v1", "abc", 3)]);
    verify_entries(&client, &repo, &[entry("task/a")]).unwrap();
    let requests = worker.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "HEAD");
    assert!(requests[0].target.ends_with("versionId=v1"));
    for response in [
        head("wrong", "abc", 3),
        head("v1", "wrong", 3),
        head("v1", "abc", 4),
    ] {
        let (client, worker) = fixture(vec![response]);
        assert!(
            verify_head(&client, &repo, &entry("task/a"))
                .unwrap_err()
                .to_string()
                .contains("mismatched")
        );
        worker.join().unwrap();
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
        let (client, worker) = fixture(vec![missing(code, status)]);
        assert!(verify_head(&client, &repo, &entry("task/a")).is_err());
        let requests = worker.join().unwrap();
        assert_eq!(requests.len(), 1);
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
    let (client, _worker) = fixture(Vec::new());
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

#[test]
fn generic_purge_deletes_complete_exact_object_history_including_markers() {
    let (_directory, repo) = repo();
    let first = Reply::xml(&format!(
        "<ListVersionsResult>{}<IsTruncated>true</IsTruncated><NextKeyMarker>root/task/a</NextKeyMarker><NextVersionIdMarker>v1</NextVersionIdMarker></ListVersionsResult>",
        version_row("root/task/a", "v1")
    ));
    let (client, worker) = fixture(vec![
        history(""),
        history(""),
        first,
        history(&marker_row("root/task/a", "d1")),
        deleted(),
        deleted(),
        history(&version_row("root/task/ab", "sibling")),
    ]);
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
    let requests = worker.join().unwrap();
    let deleted = requests
        .iter()
        .filter(|request| request.method == "DELETE")
        .collect::<Vec<_>>();
    assert_eq!(deleted.len(), 2);
    assert!(deleted.iter().all(|request| {
        request
            .target
            .starts_with("/fixture-bucket/root/task/a?versionId=")
    }));
    assert!(
        deleted
            .iter()
            .any(|request| request.target.ends_with("versionId=d1"))
    );
    assert!(
        !deleted
            .iter()
            .any(|request| request.target.contains("sibling"))
    );
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
    std::thread::JoinHandle<Vec<crate::native_s3::tests::WireRequest>>,
    std::sync::Arc<(std::sync::Mutex<ConcurrentPurgeState>, std::sync::Condvar)>,
);

fn concurrent_purge_fixture(
    requests: usize,
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
    let (client, worker) = fixture_handler(requests, move |request| {
        let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
        let query = url.query_pairs().collect::<BTreeMap<_, _>>();
        let (lock, changed) = &*observed;
        if request.method == "DELETE" {
            let object = url.path().strip_prefix("/fixture-bucket/").unwrap();
            assert!(objects.iter().any(|expected| expected == object));
            let version = query.get("versionId").unwrap().as_ref();
            assert!(version == "v1" || (marker && version == "d1"));
            let mut state = lock.lock().unwrap();
            if fail_first_delete && object == objects[0] && !state.failed {
                state.failed = true;
                return missing("InternalError", 500);
            }
            assert!(state.deleted.insert((object.into(), version.into())));
            return deleted();
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
            json!({"pointer":format!("{object}.dvc"),"object":object,"version_id":"v1"})
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
    // Three ancestor registries, each read twice, plus initial history,
    // two exact DELETEs (including a marker), and a post-delete history.
    let (client, worker, state) = concurrent_purge_fixture(
        count * 10,
        concurrent_purge_objects(count),
        true,
        false,
        false,
    );
    let result = delete_candidates(&client, &repo, &payload).unwrap();
    let requests = worker.join().unwrap();
    let state = state.0.lock().unwrap();
    assert_eq!(state.maximum, PURGE_WORKERS);
    assert!(state.active.is_empty());
    assert_eq!(state.deleted.len(), count * 2);
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
        assert_eq!(reads, count * 2, "fresh registry checks for {source}");
    }
}

#[test]
fn generic_purge_queued_object_observes_new_registry_and_refuses_uncoordinated_delete() {
    let (_directory, repo) = repo();
    let payload = concurrent_purge_payload(5);
    // Four complete unarchived pipelines (9 requests each). The queued fifth
    // reads the newly published registry twice (3 requests each), lists its
    // object once, then rejects the missing coordination proof before DELETE.
    let (client, worker, state) =
        concurrent_purge_fixture(43, concurrent_purge_objects(5), false, false, true);
    let error = delete_candidates(&client, &repo, &payload).unwrap_err();
    assert!(error.to_string().contains("atomic Git registry binding"));
    let requests = worker.join().unwrap();
    let state = state.0.lock().unwrap();
    assert!(state.published);
    assert_eq!(state.maximum, PURGE_WORKERS);
    assert_eq!(state.deleted.len(), PURGE_WORKERS);
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
    // First attempt: one failed DELETE (8 calls), three successful objects
    // (9 each). Retry: three absent objects (7 each), one success (9).
    let (client, worker, state) =
        concurrent_purge_fixture(65, concurrent_purge_objects(4), false, true, false);
    assert!(delete_candidates(&client, &repo, &payload).is_err());
    assert_eq!(payload, unchanged);
    assert_eq!(state.0.lock().unwrap().deleted.len(), 3);
    let result = delete_candidates(&client, &repo, &payload).unwrap();
    let requests = worker.join().unwrap();
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

    let (_directory, repo) = repo();
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
    // Public adapter registry inspection: versioning + two empty listings (3).
    // Native purge: versioning (1), generic registry lookup (2), history (1),
    // three exact DELETEs (3), then complete post-delete history verification (1).
    let (client, worker) = fixture_handler(11, move |request| {
        let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
        let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
        if request.method == "DELETE" {
            assert_eq!(url.path(), "/fixture-bucket/root/task/a");
            let version = query.get("versionId").unwrap();
            assert!(
                handler_present.contains(version),
                "unrequested deletion: {version}"
            );
            assert!(handler_retired.lock().unwrap().insert(version.clone()));
            return deleted();
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
        let scan = handler_reads.fetch_add(1, Ordering::SeqCst);
        let retired = handler_retired.lock().unwrap();
        assert_eq!(retired.len(), if scan == 0 { 0 } else { 3 });
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
    assert_eq!(inventory_reads.load(Ordering::SeqCst), 2);
    // Generic retirement reports per object, including retries with absent IDs.
    assert_eq!(result["already_absent"], json!([]));
    assert_eq!(result["retained_unmapped"], json!([]));
    let requests = worker.join().unwrap();
    assert_eq!(requests.len(), 11);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "DELETE")
            .count(),
        3
    );
    assert!(
        requests
            .iter()
            .filter(|request| request.method == "DELETE")
            .all(|request| {
                request
                    .target
                    .starts_with("/fixture-bucket/root/task/a?versionId=")
                    && !request.target.contains("neighbor-version")
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
    // Public adapter inspect (3 requests) and native bucket versioning (1)
    // precede validation. No object history or DELETE may be reached.
    let (client, worker) = fixture(vec![versioning(), history(""), history(""), versioning()]);
    configure_repo(&client, &repo);
    let error = crate::storage_metadata::version_purge_adapter(&repo, "delete", &payload)
        .unwrap_err()
        .to_string();
    assert!(error.contains("missing or invalid version_id"), "{error}");
    let requests = worker.join().unwrap();
    assert_eq!(requests.len(), 4);
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
        let (client, worker) = fixture(vec![
            history(&version_row("root/task/a", "v1")),
            registry_listing(),
            registry_body(&receipt),
            registry_listing(),
        ]);
        let error = delete_candidates(&client, &repo, &payload)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("coordination") || error.contains("published"),
            "{error}"
        );
        let requests = worker.join().unwrap();
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

#[test]
fn fetch_streams_exact_version_with_ifmatch_and_installs_only_verified_bytes() {
    let (_directory, repo) = repo();
    let (client, worker) = fixture(vec![versioning(), downloaded(b"abc", "v1", "abc")]);
    configure_repo(&client, &repo);
    file_pointer(&repo);
    let result = read(&repo, &["task/data.dvc".into()], "--fetch", &[]).unwrap();
    assert_eq!(result["checked_objects"], json!(["task/data"]));
    let path = native_engine::cache_path(&repo, "900150983cd24fb0d6963f7d28e17f72").unwrap();
    assert_eq!(fs::read(path).unwrap(), b"abc");
    let requests = worker.join().unwrap();
    assert_eq!(
        requests[1].target,
        "/fixture-bucket/root/task/data?versionId=v1"
    );
    assert_eq!(requests[1].headers["if-match"], "\"abc\"");
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
        let (client, worker) = fixture(vec![
            versioning(),
            downloaded(&body, "v1", "abc"),
            versioning(),
            head("v1", "abc", body.len() as u64),
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
        let requests = worker.join().unwrap();
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
        let (client, worker) = fixture(vec![versioning(), reply]);
        configure_repo(&client, &repo);
        file_pointer(&repo);
        let path = native_engine::cache_path(&repo, "900150983cd24fb0d6963f7d28e17f72").unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"old-corrupt").unwrap();
        assert!(read(&repo, &["task/data.dvc".into()], "--fetch", &[]).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"old-corrupt");
        worker.join().unwrap();
    }
}

#[test]
fn valid_cache_bytes_still_require_remote_exact_version() {
    let (_directory, repo) = repo();
    let (client, worker) = fixture(vec![versioning(), missing("AccessDenied", 403)]);
    configure_repo(&client, &repo);
    file_pointer(&repo);
    native_engine::install_cache(&repo, "900150983cd24fb0d6963f7d28e17f72", b"abc").unwrap();
    assert!(
        read(&repo, &["task/data.dvc".into()], "--fetch", &[])
            .unwrap_err()
            .to_string()
            .contains("AccessDenied")
    );
    let requests = worker.join().unwrap();
    assert_eq!(requests[1].method, "HEAD");
    assert_eq!(requests.len(), 2);
}

#[test]
fn legacy_cache_layout_needs_exact_head_verification_without_a_get() {
    let (_directory, repo) = repo();
    let (client, worker) = fixture(vec![versioning(), head("v1", "abc", 3)]);
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
    let requests = worker.join().unwrap();
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
    let (client, worker) = fixture(vec![
        versioning(),
        downloaded(body, "v1", "abc"),
        versioning(),
        head("v1", "abc", 6),
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
    let requests = worker.join().unwrap();
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
    let (client, worker) = fixture(vec![
        versioning(),
        downloaded(first, "version-a", "raw-a"),
        versioning(),
        downloaded(second, "version-b", "raw-b"),
        versioning(),
        head("version-a", "raw-a", 5),
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
    let requests = worker.join().unwrap();
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
    let (client, worker) = fixture(vec![
        versioning(),
        downloaded(first, "version-a", "raw-a"),
        versioning(),
        downloaded(second, "version-b", "raw-b"),
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
    let requests = worker.join().unwrap();
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

#[test]
fn published_archive_purge_deletes_only_mapped_versions_and_retains_new_writes() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
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
    // Initial list + canonical/history checks before each of two DELETEs +
    // final complete rescan. Every request targets this private loopback.
    let (client, worker) = fixture_handler(19, move |request| {
        if request.method == "DELETE" {
            if request.target.ends_with("versionId=v1") {
                payload_flag.store(true, Ordering::SeqCst);
            } else if request.target.ends_with("versionId=d1") {
                marker_flag.store(true, Ordering::SeqCst);
            } else {
                panic!("attempted unmapped version deletion: {}", request.target);
            }
            return deleted();
        }
        if request.method == "HEAD" {
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
        registry_body(&stored_receipt)
    });
    let payload = json!({"candidates":[{"pointer":"task/.workspace-mgr-archive.json","object":"task/a","version_id":"v1"},{"pointer":"task/.workspace-mgr-archive.json","object":"task/a","version_id":"d1"}],"prefixes":[receipt],"coordination":[{"receipt":receipt,"coordination":proof}]});
    let result = delete_candidates(&client, &repo, &payload).unwrap();
    assert!(deleted_payload.load(Ordering::SeqCst));
    assert!(deleted_marker.load(Ordering::SeqCst));
    assert_eq!(result["retained_unmapped"].as_array().unwrap().len(), 2);
    assert_eq!(result["cleaned_prefixes"], json!([]));
    let requests = worker.join().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "DELETE")
            .count(),
        2
    );
}
