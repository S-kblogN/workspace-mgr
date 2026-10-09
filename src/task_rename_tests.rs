use super::*;
use serde_json::json;

const SOURCE: &str = "20261008-120000-original";
const DESTINATION: &str = "20261008-120000-renamed";
const LAST: &str = "20261008-120000-final";

fn fixture() -> (tempfile::TempDir, GitRepo, ResolvedTask, Value, String) {
    let temporary = tempfile::tempdir().unwrap();
    let repo = GitRepo {
        root: temporary.path().canonicalize().unwrap(),
    };
    repo.run(["init", "-q", "-b", "main"]).unwrap();
    repo.run(["config", "user.name", "Rename fixture"]).unwrap();
    repo.run(["config", "user.email", "rename@example.invalid"])
        .unwrap();
    repo.run(["remote", "add", "origin", repo.root.to_str().unwrap()])
        .unwrap();
    fs::write(Config::path(&repo), Config::default().render().unwrap()).unwrap();
    fs::write(repo.root.join(".gitignore"), "/.workspace-mgr/local/\n").unwrap();
    let source = repo.root.join(SOURCE);
    fs::create_dir(&source).unwrap();
    fs::write(source.join(TASK_MANIFEST_NAME), format!(
        "schema_version = 2\nkind = \"deliverable\"\nid = \"{SOURCE}\"\nslug = \"original\"\npath = \"{SOURCE}\"\nbranch = \"codex/original\"\ntitle = \"Rename fixture\"\npurpose = \"Preserve exact history\"\nadditional_scopes = []\n"
    )).unwrap();
    fs::write(source.join("payload.bin"), b"abc").unwrap();
    let pointer = format!("{SOURCE}/payload.bin{}", crate::storage_format::SUFFIX);
    let raw = format!(
        "{}\n",
        serde_json::to_string_pretty(&json!({
            "schema_version":1,"path":"payload.bin","kind":"file",
            "checksum":{"algorithm":"md5","digest":"900150983cd24fb0d6963f7d28e17f72"},
            "size":3,"version":{"id":"source-current","etag":"900150983cd24fb0d6963f7d28e17f72"}
        }))
        .unwrap()
    );
    fs::write(repo.root.join(&pointer), &raw).unwrap();
    repo.run(["add", "."]).unwrap();
    repo.run(["commit", "-q", "-m", "Initial fixture"]).unwrap();
    repo.run(["branch", "codex/original"]).unwrap();
    let task =
        ResolvedTask::load(&repo, &Config::default(), &source.join(TASK_MANIFEST_NAME)).unwrap();
    let receipt = json!({"schema_version":1,"source":SOURCE,"destination":DESTINATION,
    "task_id":SOURCE,"migration_kind":"task-rename","status":"planned",
    "remote":"workspace-mgr","bucket":"fixture-bucket","remote_prefix":"root",
    "source_cleanup":"after_verified_git_publication",
    "versions":[
        {"source_object":format!("{SOURCE}/payload.bin"),"destination_object":format!("{DESTINATION}/payload.bin"),
         "source_version_id":"source-older","source_etag":"older-etag","size":5,"delete_marker":false},
        {"source_object":format!("{SOURCE}/payload.bin"),"destination_object":format!("{DESTINATION}/payload.bin"),
         "source_version_id":"source-current","source_etag":"900150983cd24fb0d6963f7d28e17f72","size":3,"delete_marker":false},
        {"source_object":format!("{SOURCE}/deleted.bin"),"destination_object":format!("{DESTINATION}/deleted.bin"),
         "source_version_id":"source-marker","delete_marker":true}
    ]});
    (temporary, repo, task, receipt, raw)
}

fn move_task(
    repo: &GitRepo,
    task: &ResolvedTask,
    destination: &str,
    migration: &StorageRename,
) -> ResolvedTask {
    let source = task.task_path.as_deref().unwrap();
    let relocation =
        RelocationPlan::opaque(&repo.root.join(source), &repo.root.join(destination)).unwrap();
    let mut manifest = task.manifest();
    manifest.slug = destination.splitn(3, '-').nth(2).unwrap().to_owned();
    manifest.path = Some(destination.to_owned());
    apply_rename(
        repo,
        &Config::default(),
        task,
        Some(destination),
        &manifest.render().unwrap(),
        Some(&relocation),
        Some(migration),
    )
    .unwrap();
    ResolvedTask::load(
        repo,
        &Config::default(),
        &repo.root.join(destination).join(TASK_MANIFEST_NAME),
    )
    .unwrap()
}

fn copied_receipt(planned: &Value) -> Value {
    let mut copied = planned.clone();
    copied["status"] = "copied".into();
    copied["transaction_id"] = "rename-test-copy".into();
    for (index, row) in copied["versions"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .enumerate()
    {
        row["destination_version_id"] = format!("copied-{index}").into();
        if row["delete_marker"] == false {
            row["destination_etag"] = row["source_etag"].clone();
        }
    }
    copied
}

fn install_copied_receipt(repo: &GitRepo, copied: &Value) {
    let copy_journal = crate::archive_cancel::copy_journal(repo, SOURCE, DESTINATION).unwrap();
    fs::create_dir_all(copy_journal.parent().unwrap()).unwrap();
    let mut private = copied.clone();
    for key in crate::archive_migration::RECEIPT_METADATA_FIELDS {
        private.as_object_mut().unwrap().remove(key);
    }
    fs::write(copy_journal, serde_json::to_vec(&private).unwrap()).unwrap();
    fs::write(
        repo.root
            .join(DESTINATION)
            .join(crate::archive_migration::RECEIPT_NAME),
        format!("{}\n", serde_json::to_string_pretty(copied).unwrap()),
    )
    .unwrap();
}

#[test]
fn planned_s3_rename_preserves_payloads_bindings_and_all_history() {
    let (_temporary, repo, task, receipt, raw) = fixture();
    let migration = StorageRename {
        receipt: receipt.clone(),
        previous: None,
    };
    let renamed = move_task(&repo, &task, DESTINATION, &migration);
    assert_eq!(renamed.task_id, SOURCE);
    assert_eq!(
        fs::read(repo.root.join(DESTINATION).join("payload.bin")).unwrap(),
        b"abc"
    );
    assert_eq!(
        fs::read_to_string(repo.root.join(format!(
            "{DESTINATION}/payload.bin{}",
            crate::storage_format::SUFFIX
        )))
        .unwrap(),
        raw
    );
    assert!(!repo.root.join(SOURCE).exists());
    assert!(crate::archive_cancel::has_trusted_migration(&repo, &receipt).unwrap());
    let stored: Value = serde_json::from_slice(
        &fs::read(
            repo.root
                .join(DESTINATION)
                .join(crate::archive_migration::RECEIPT_NAME),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(stored, receipt);
    assert_eq!(stored["versions"].as_array().unwrap().len(), 3);
    assert!(stored["versions"][2]["delete_marker"].as_bool().unwrap());
}

#[test]
fn s3_rename_planning_freezes_history_without_transferring_payloads() {
    use crate::native_s3::tests::{Reply, configure_repo, routed_fixture};
    let (_temporary, repo, task, _, raw) = fixture();
    let (client, server) = routed_fixture(|request| {
        assert_eq!(request.method, "GET");
        let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
        let query = url
            .query_pairs()
            .collect::<std::collections::BTreeMap<_, _>>();
        if query.contains_key("versioning") {
            return Reply::xml(
                "<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>",
            );
        }
        if query.contains_key("uploads") {
            return Reply::xml(
                "<ListMultipartUploadsResult><IsTruncated>false</IsTruncated></ListMultipartUploadsResult>",
            );
        }
        assert!(query.contains_key("versions"));
        if query.get("prefix").map(|value| value.as_ref())
            != Some(format!("root/{SOURCE}/").as_str())
        {
            return Reply::xml(
                "<ListVersionsResult><IsTruncated>false</IsTruncated></ListVersionsResult>",
            );
        }
        Reply::xml(&format!("<ListVersionsResult><IsTruncated>false</IsTruncated>
            <Version><Key>root/{SOURCE}/payload.bin</Key><VersionId>source-current</VersionId><IsLatest>true</IsLatest><LastModified>2026-10-08T20:00:00Z</LastModified><ETag>900150983cd24fb0d6963f7d28e17f72</ETag><Size>3</Size></Version>
            <Version><Key>root/{SOURCE}/payload.bin</Key><VersionId>source-older</VersionId><IsLatest>false</IsLatest><LastModified>2026-10-07T20:00:00Z</LastModified><ETag>older-etag</ETag><Size>5</Size></Version>
            <DeleteMarker><Key>root/{SOURCE}/deleted.bin</Key><VersionId>source-marker</VersionId><IsLatest>true</IsLatest><LastModified>2026-10-06T20:00:00Z</LastModified></DeleteMarker>
            </ListVersionsResult>"))
    });
    configure_repo(&client, &repo);
    let migration = plan_storage_rename(
        &repo,
        &Config::load(&repo).unwrap(),
        &task,
        SOURCE,
        DESTINATION,
    )
    .unwrap()
    .unwrap();
    assert_eq!(migration.receipt["migration_kind"], "task-rename");
    assert_eq!(migration.receipt["versions"].as_array().unwrap().len(), 3);
    assert_eq!(migration.receipt["task_id"], SOURCE);
    assert!(
        migration.receipt["versions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["delete_marker"] == true)
    );
    assert_eq!(
        fs::read_to_string(repo.root.join(format!(
            "{SOURCE}/payload.bin{}",
            crate::storage_format::SUFFIX
        )))
        .unwrap(),
        raw
    );
    assert!(!repo.root.join(DESTINATION).exists());
    assert!(
        server
            .finish_requests()
            .iter()
            .all(|request| request.method == "GET" && !request.target.contains("versionId="))
    );
}

#[test]
fn planned_repeated_s3_rename_collapses_to_original_source_and_cancels_losslessly() {
    let (_temporary, repo, task, receipt, raw) = fixture();
    let renamed = move_task(
        &repo,
        &task,
        DESTINATION,
        &StorageRename {
            receipt: receipt.clone(),
            previous: None,
        },
    );
    let retarget = plan_storage_rename(&repo, &Config::default(), &renamed, DESTINATION, LAST)
        .unwrap()
        .unwrap();
    assert_eq!(retarget.receipt["source"], SOURCE);
    assert_eq!(retarget.receipt["destination"], LAST);
    let final_task = move_task(&repo, &renamed, LAST, &retarget);
    assert!(crate::archive_cancel::has_trusted_migration(&repo, &retarget.receipt).unwrap());
    fs::write(
        repo.root.join(LAST).join("payload.bin"),
        b"edited opaque payload",
    )
    .unwrap();
    let cancelled =
        crate::archive_cancel::cancel(&repo.root, Some(&final_task.manifest_path), &[], false)
            .unwrap();
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(
        fs::read(repo.root.join(SOURCE).join("payload.bin")).unwrap(),
        b"edited opaque payload"
    );
    assert_eq!(
        fs::read_to_string(repo.root.join(format!(
            "{SOURCE}/payload.bin{}",
            crate::storage_format::SUFFIX
        )))
        .unwrap(),
        raw
    );
    assert!(!repo.root.join(LAST).exists());
    assert!(
        !repo
            .root
            .join(SOURCE)
            .join(crate::archive_migration::RECEIPT_NAME)
            .exists()
    );
}

#[test]
fn started_s3_rename_refuses_retarget_without_changing_controls() {
    let (_temporary, repo, task, receipt, _) = fixture();
    let renamed = move_task(
        &repo,
        &task,
        DESTINATION,
        &StorageRename {
            receipt: receipt.clone(),
            previous: None,
        },
    );
    let journal = crate::archive_cancel::copy_journal(&repo, SOURCE, DESTINATION).unwrap();
    fs::create_dir_all(journal.parent().unwrap()).unwrap();
    fs::write(&journal, b"{\"status\":\"copying\"}").unwrap();
    let error = plan_storage_rename(&repo, &Config::default(), &renamed, DESTINATION, LAST)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("copy has started"), "{error}");
    assert!(repo.root.join(DESTINATION).is_dir());
    assert!(!repo.root.join(LAST).exists());
    assert_eq!(fs::read(&journal).unwrap(), b"{\"status\":\"copying\"}");
    assert!(crate::archive_cancel::has_trusted_migration(&repo, &receipt).unwrap());
    // Completing the copy on retry still rebases the unchanged source
    // metadata; the local move did not discard its old exact generation.
    let copied = copied_receipt(&receipt);
    install_copied_receipt(&repo, &copied);
    let pointer = format!("{DESTINATION}/payload.bin{}", crate::storage_format::SUFFIX);
    crate::archive_migration::rewrite_pointer(&repo, &pointer, &copied).unwrap();
    assert!(crate::archive_cancel::has_trusted_migration(&repo, &copied).unwrap());
    let retried: Value =
        serde_json::from_slice(&fs::read(repo.root.join(pointer)).unwrap()).unwrap();
    assert_eq!(retried["version"]["id"], "copied-1");
}

#[test]
fn published_rename_allows_future_payload_edits_without_replaying_the_copy() {
    let (_temporary, repo, task, receipt, _) = fixture();
    let renamed = move_task(
        &repo,
        &task,
        DESTINATION,
        &StorageRename {
            receipt: receipt.clone(),
            previous: None,
        },
    );
    let copied = copied_receipt(&receipt);
    install_copied_receipt(&repo, &copied);
    let pointer = format!("{DESTINATION}/payload.bin{}", crate::storage_format::SUFFIX);
    crate::archive_migration::rewrite_pointer(&repo, &pointer, &copied).unwrap();
    repo.run(["add", "-A"]).unwrap();
    repo.run(["commit", "-q", "-m", "Published rename fixture"])
        .unwrap();
    let base = repo.optional_oid("HEAD").unwrap().unwrap();
    crate::archive_cancel::record_publication(
        &repo,
        &renamed,
        std::slice::from_ref(&copied),
        &base,
        true,
    )
    .unwrap();
    fs::write(
        repo.root.join(DESTINATION).join("payload.bin"),
        b"later changed bytes",
    )
    .unwrap();
    assert_eq!(
        crate::archive_migration::pending_rename_source(&repo, &renamed, &base).unwrap(),
        None
    );
    assert!(
        crate::archive_migration::pointer_set(&repo, &[DESTINATION.to_owned()])
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        crate::archive_migration::prepare(
            &repo,
            &Config::default(),
            &[DESTINATION.to_owned()],
            &base
        )
        .unwrap(),
        vec![copied]
    );
    let receipt_path = repo
        .root
        .join(DESTINATION)
        .join(crate::archive_migration::RECEIPT_NAME);
    let original = fs::read(&receipt_path).unwrap();
    fs::remove_file(&receipt_path).unwrap();
    let removed = crate::archive_migration::pending_rename_source(&repo, &renamed, &base)
        .unwrap_err()
        .to_string();
    assert!(removed.contains("removed or changed"), "{removed}");
    let mut tampered: Value = serde_json::from_slice(&original).unwrap();
    tampered["versions"][1]["destination_version_id"] = "unrelated-generation".into();
    fs::write(&receipt_path, tampered.to_string()).unwrap();
    let changed = crate::archive_migration::pending_rename_source(&repo, &renamed, &base)
        .unwrap_err()
        .to_string();
    assert!(changed.contains("removed or changed"), "{changed}");
    fs::write(&receipt_path, original).unwrap();
    assert_eq!(
        crate::archive_migration::pending_rename_source(&repo, &renamed, &base).unwrap(),
        None
    );
    let error = plan_storage_rename(&repo, &Config::default(), &renamed, DESTINATION, LAST)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("unsupported"), "{error}");
}

#[test]
fn rename_marker_cannot_authorize_another_timestamp_or_arbitrary_prefix() {
    let (_temporary, _repo, _task, receipt, _) = fixture();
    crate::archive_migration::validate(
        &format!("{DESTINATION}/{}", crate::archive_migration::RECEIPT_NAME),
        &receipt,
    )
    .unwrap();
    for destination in [
        "20261009-120000-other",
        "arbitrary",
        "2026/10/20261008-120000-renamed",
    ] {
        let mut forged = receipt.clone();
        forged["destination"] = destination.into();
        assert!(
            crate::archive_migration::validate(
                &format!("{destination}/{}", crate::archive_migration::RECEIPT_NAME),
                &forged
            )
            .is_err()
        );
    }
}

#[test]
fn legacy_normalized_binding_refuses_rename_before_remote_or_local_mutation() {
    let (_temporary, repo, task, _, raw) = fixture();
    let pointer = format!("{SOURCE}/payload.bin{}", crate::storage_format::SUFFIX);
    let mut normalized: Value = serde_json::from_str(&raw).unwrap();
    normalized["checksum"]["algorithm"] = "md5-dos2unix".into();
    let contents = format!("{}\n", serde_json::to_string_pretty(&normalized).unwrap());
    fs::write(repo.root.join(&pointer), &contents).unwrap();
    let config = Config {
        s3: Some(crate::config::S3Config {
            url: "s3://fixture-bucket/root".to_owned(),
            endpoint_url: Some("https://unused.invalid".to_owned()),
        }),
        ..Config::default()
    };
    let error = plan_storage_rename(&repo, &config, &task, SOURCE, DESTINATION)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("exact source-version cache"), "{error}");
    assert_eq!(
        fs::read_to_string(repo.root.join(&pointer)).unwrap(),
        contents
    );
    assert!(repo.root.join(SOURCE).is_dir());
    assert!(!repo.root.join(DESTINATION).exists());
}

#[test]
fn normalized_rename_rebinds_the_exact_cache_without_upload_and_detects_raw_edits() {
    use crate::native_s3::tests::{Reply, configure_repo, routed_fixture};
    use md5::Digest;
    let (_temporary, repo, task, mut receipt, raw) = fixture();
    let payload = b"a\r\nb\n";
    let tag = crate::hex::encode_lower(md5::Md5::digest(payload));
    let expected_tag = tag.clone();
    let (client, server) = routed_fixture(move |request| {
        let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
        if request.method == "GET" && url.query_pairs().any(|(key, _)| key == "versioning") {
            return Reply::xml(
                "<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>",
            );
        }
        assert_eq!(
            request.method, "HEAD",
            "unchanged normalized bytes transferred: {} {}",
            request.method, request.target
        );
        Reply {
            status: 200,
            headers: vec![
                ("content-length", "5".into()),
                ("etag", expected_tag.clone()),
                ("x-amz-version-id", "copied-1".into()),
            ],
            body: Vec::new(),
        }
    });
    configure_repo(&client, &repo);
    let pointer = format!("{SOURCE}/payload.bin{}", crate::storage_format::SUFFIX);
    fs::write(repo.root.join(SOURCE).join("payload.bin"), payload).unwrap();
    let digest = crate::native_engine::file_digest(
        &repo.root.join(SOURCE).join("payload.bin"),
        "md5-dos2unix",
    )
    .unwrap();
    let mut normalized: Value = serde_json::from_str(&raw).unwrap();
    normalized["checksum"]["algorithm"] = "md5-dos2unix".into();
    normalized["checksum"]["digest"] = digest.clone().into();
    normalized["size"] = 5.into();
    normalized["version"]["etag"] = tag.clone().into();
    fs::write(
        repo.root.join(&pointer),
        format!("{}\n", serde_json::to_string_pretty(&normalized).unwrap()),
    )
    .unwrap();
    receipt["versions"][1]["size"] = 5.into();
    receipt["versions"][1]["source_etag"] = tag.into();
    let source_entry = crate::native_engine::StorageEntry {
        pointer,
        object: format!("{SOURCE}/payload.bin"),
        md5: Some(digest),
        size: Some(5),
        version_id: Some("source-current".into()),
        etag: None,
        verification: None,
        hash_name: "md5-dos2unix".into(),
    };
    crate::native_engine::install_cache_for_entry(
        &repo,
        &source_entry,
        &repo.root.join(SOURCE).join("payload.bin"),
    )
    .unwrap();
    move_task(
        &repo,
        &task,
        DESTINATION,
        &StorageRename {
            receipt: receipt.clone(),
            previous: None,
        },
    );
    let copied = copied_receipt(&receipt);
    install_copied_receipt(&repo, &copied);
    let pointer = format!("{DESTINATION}/payload.bin{}", crate::storage_format::SUFFIX);
    crate::archive_migration::rewrite_pointer(&repo, &pointer, &copied).unwrap();
    let mut destination_entry = source_entry.clone();
    destination_entry.object = format!("{DESTINATION}/payload.bin");
    destination_entry.version_id = Some("copied-1".into());
    assert_eq!(
        fs::read(crate::native_engine::cache_path_for_entry(&repo, &destination_entry).unwrap())
            .unwrap(),
        payload
    );
    let mut report = storage_metadata::reconcile(
        &repo,
        &Config::load(&repo).unwrap(),
        std::slice::from_ref(&pointer),
        false,
    )
    .unwrap();
    assert!(report.committed.is_empty());
    storage_metadata::push_outputs(&repo, &Config::load(&repo).unwrap(), &mut report).unwrap();
    assert!(
        server
            .finish_requests()
            .iter()
            .all(|request| matches!(request.method.as_str(), "GET" | "HEAD"))
    );
    // Same normalized MD5 and size, different raw bytes: the exact cache
    // association must detect the change instead of reusing the copied version.
    fs::write(repo.root.join(DESTINATION).join("payload.bin"), b"a\nb\r\n").unwrap();
    storage_metadata::execute_engine(
        &repo.root,
        &crate::native_engine::Operation::Record {
            pointers: vec![pointer.clone()],
        },
    )
    .unwrap();
    let changed: Value =
        serde_json::from_slice(&fs::read(repo.root.join(pointer)).unwrap()).unwrap();
    assert!(changed.get("version").is_none());
    assert_eq!(changed["checksum"]["algorithm"], "md5");
}

#[test]
fn copied_rename_reuses_unchanged_version_and_only_modified_payload_loses_binding() {
    use crate::native_s3::tests::{Reply, configure_repo, routed_fixture};
    let (_temporary, repo, task, receipt, _) = fixture();
    move_task(
        &repo,
        &task,
        DESTINATION,
        &StorageRename {
            receipt: receipt.clone(),
            previous: None,
        },
    );
    let copied = copied_receipt(&receipt);
    let pointer = format!("{DESTINATION}/payload.bin{}", crate::storage_format::SUFFIX);
    crate::archive_migration::rewrite_pointer(&repo, &pointer, &copied).unwrap();
    install_copied_receipt(&repo, &copied);
    let (client, server) = routed_fixture(|request| {
        let parsed = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
        let query = parsed
            .query_pairs()
            .collect::<std::collections::BTreeMap<_, _>>();
        match (
            request.method.as_str(),
            query.contains_key("versioning"),
            query.contains_key("versions"),
        ) {
            ("GET", true, _) => Reply::xml(
                "<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>",
            ),
            ("GET", _, true) => Reply::xml(&format!(
                "<ListVersionsResult><IsTruncated>false</IsTruncated><Version><Key>root/{DESTINATION}/payload.bin</Key><VersionId>copied-1</VersionId><IsLatest>true</IsLatest><LastModified>2026-10-08T20:00:00Z</LastModified><ETag>900150983cd24fb0d6963f7d28e17f72</ETag><Size>3</Size></Version></ListVersionsResult>"
            )),
            ("HEAD", _, _) => Reply {
                status: 200,
                headers: vec![
                    ("content-length", "3".into()),
                    ("etag", "900150983cd24fb0d6963f7d28e17f72".into()),
                    ("x-amz-version-id", "copied-1".into()),
                ],
                body: Vec::new(),
            },
            _ => panic!(
                "unchanged renamed bytes unexpectedly transferred: {} {}",
                request.method, request.target
            ),
        }
    });
    configure_repo(&client, &repo);
    let config = Config::load(&repo).unwrap();
    let before_record = vec![(pointer.clone(), fs::read(repo.root.join(&pointer)).unwrap())];
    let mut report =
        storage_metadata::reconcile(&repo, &config, std::slice::from_ref(&pointer), false).unwrap();
    crate::archive_cancel::record_rename_reconciliation(
        &repo,
        std::slice::from_ref(&copied),
        &before_record,
    )
    .unwrap();
    let before_upload = vec![(pointer.clone(), fs::read(repo.root.join(&pointer)).unwrap())];
    storage_metadata::push_outputs(&repo, &config, &mut report).unwrap();
    crate::archive_cancel::record_rename_reconciliation(
        &repo,
        std::slice::from_ref(&copied),
        &before_upload,
    )
    .unwrap();
    let requests = server.finish_requests();
    assert!(
        requests
            .iter()
            .all(|request| matches!(request.method.as_str(), "GET" | "HEAD"))
    );
    assert!(
        !requests
            .iter()
            .any(|request| request.target.contains("versionId=") && request.method == "GET")
    );
    fs::write(repo.root.join(DESTINATION).join("payload.bin"), b"changed").unwrap();
    // Recording edited bytes clears only their destination binding; native
    // upload then follows its ordinary exact upload/retry protocol.
    let before = vec![(pointer.clone(), fs::read(repo.root.join(&pointer)).unwrap())];
    storage_metadata::execute_engine(
        &repo.root,
        &crate::native_engine::Operation::Record {
            pointers: vec![pointer.clone()],
        },
    )
    .unwrap();
    crate::archive_cancel::record_rename_reconciliation(
        &repo,
        std::slice::from_ref(&copied),
        &before,
    )
    .unwrap();
    let changed: Value =
        serde_json::from_slice(&fs::read(repo.root.join(&pointer)).unwrap()).unwrap();
    assert!(changed.get("version").is_none());
    assert_eq!(changed["size"], 7);
    crate::archive_migration::rewrite_pointer(&repo, &pointer, &copied).unwrap();
    let retried: Value =
        serde_json::from_slice(&fs::read(repo.root.join(&pointer)).unwrap()).unwrap();
    assert!(retried.get("version").is_none());
}
