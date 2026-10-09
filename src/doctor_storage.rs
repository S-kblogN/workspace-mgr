//! Read-only comparison of the checkout's logical storage tree with S3.
//!
//! Unlike hydration, this audit never follows archive aliases, installs cache
//! objects, repairs pointers, or deletes remotely retained objects. An exact
//! version at another logical path is a layout error even when still readable.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufReader, IsTerminal, Read};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
#[cfg(test)]
use std::path::Path;

use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::archive_migration;
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::native_engine::{self, StorageEntry};
use crate::native_s3::S3Client;
use crate::path::{reject_symlink_traversal, to_slash};
use crate::storage_metadata;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuditReport {
    pub status: String,
    pub expected_objects: usize,
    pub remote_versions: usize,
    pub retained_archive_versions: usize,
    pub remote_checksum_objects: usize,
    pub verified_version_objects: usize,
    pub streamed_objects: usize,
    pub streamed_bytes: u64,
    pub materialized_boundaries: usize,
    pub unmaterialized_boundaries: usize,
    pub issues: Vec<AuditIssue>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuditIssue {
    pub code: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub detail: String,
}

impl AuditReport {
    fn new() -> Self {
        Self {
            status: "ok".into(),
            expected_objects: 0,
            remote_versions: 0,
            retained_archive_versions: 0,
            remote_checksum_objects: 0,
            verified_version_objects: 0,
            streamed_objects: 0,
            streamed_bytes: 0,
            materialized_boundaries: 0,
            unmaterialized_boundaries: 0,
            issues: Vec::new(),
        }
    }

    fn issue(&mut self, code: &str, path: &str, detail: impl Into<String>) {
        self.issues.push(AuditIssue {
            code: code.into(),
            path: path.into(),
            pointer: None,
            version: None,
            detail: detail.into(),
        });
    }

    fn entry_issue(&mut self, code: &str, entry: &StorageEntry, detail: impl Into<String>) {
        self.issues.push(AuditIssue {
            code: code.into(),
            path: entry.object.clone(),
            pointer: Some(entry.pointer.clone()),
            version: entry.version_id.clone(),
            detail: detail.into(),
        });
    }
}

pub(crate) fn inspect(
    repo: &GitRepo,
    scopes: &[String],
    all: bool,
    retired_scopes: &[String],
) -> Result<AuditReport> {
    let client = S3Client::from_repo(repo)?;
    inspect_with(repo, scopes, all, retired_scopes, &client)
}

fn contains(scopes: &[String], path: &str) -> bool {
    scopes
        .iter()
        .any(|scope| scope.is_empty() || path == scope || path.starts_with(&format!("{scope}/")))
}

fn selected(path: &str, scopes: &[String], retired: &[String], all: bool) -> bool {
    all || contains(scopes, path) || contains(retired, path)
}

fn inspect_with(
    repo: &GitRepo,
    scopes: &[String],
    all: bool,
    retired_scopes: &[String],
    client: &S3Client,
) -> Result<AuditReport> {
    let mut report = AuditReport::new();
    let pointers = discover_pointers(repo, scopes, all)?;
    let mut expected = BTreeMap::new();
    let mut metadata_snapshot = BTreeMap::new();
    let mut local_snapshot = BTreeMap::new();
    let mut local_entries = Vec::new();
    let progress = AuditProgress::new();
    progress.stage("local metadata and layout");
    for pointer in &pointers {
        let control = reject_symlink_traversal(&repo.root, pointer, "doctor storage metadata")
            .and_then(|()| {
                let path = repo.root.join(pointer);
                let metadata = fs::symlink_metadata(&path).at(&path)?;
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    return Err(Error::message("storage metadata must be a regular file"));
                }
                fs::read(&path).at(&path)
            });
        let raw = match control {
            Ok(raw) => raw,
            Err(error) => {
                report.issue("invalid-metadata", pointer, error.to_string());
                continue;
            }
        };
        metadata_snapshot.insert(pointer.clone(), raw.clone());
        let entries = std::str::from_utf8(&raw)
            .map_err(|error| Error::message(error.to_string()))
            .and_then(|raw| {
                let (document, algorithm) = if pointer.ends_with(crate::storage_format::SUFFIX) {
                    let manifest = crate::storage_format::Manifest::parse(raw, pointer)?;
                    (
                        storage_metadata::logical_document(&manifest),
                        manifest.checksum.algorithm,
                    )
                } else {
                    let normalized =
                        storage_metadata::normalize_pointer_in_repo(repo, None, raw, pointer)?;
                    (
                        storage_metadata::parse_pointer_document(&normalized, pointer)?,
                        storage_metadata::hash_algorithm(&normalized, pointer)?,
                    )
                };
                storage_metadata::metadata_output_from_document(repo, pointer, &document)?;
                native_engine::entries_from_document(pointer, &document, &algorithm)
            });
        let entries = match entries {
            Ok(entries) => entries,
            Err(error) => {
                report.issue("invalid-metadata", pointer, error.to_string());
                continue;
            }
        };
        if let Some(boundary) = storage_metadata::boundary_path(pointer) {
            match local_state(repo, boundary) {
                Ok(state) => {
                    local_snapshot.insert(boundary.to_owned(), state);
                }
                Err(error) => report.issue("local-state-unreadable", boundary, error.to_string()),
            }
        }
        inspect_local_boundary(repo, pointer, &entries, &mut report);
        for entry in entries {
            local_entries.push(entry.clone());
            if let Err(error) = crate::native_versions::validate_digest(&entry) {
                report.entry_issue("invalid-metadata", &entry, error.to_string());
            }
            if entry.size.is_none() {
                report.entry_issue("invalid-metadata", &entry, "metadata has no physical size");
            }
            if entry
                .etag
                .as_deref()
                .is_none_or(|etag| tag(etag).is_empty())
            {
                report.entry_issue(
                    "missing-etag-binding",
                    &entry,
                    "metadata has no S3 ETag binding",
                );
            }
            if entry
                .version_id
                .as_deref()
                .is_none_or(|id| id.is_empty() || id == "null")
            {
                report.entry_issue(
                    "missing-version-binding",
                    &entry,
                    "metadata has no exact non-null S3 version ID",
                );
            }
            if expected
                .insert(entry.object.clone(), entry.clone())
                .is_some()
            {
                report.entry_issue(
                    "invalid-metadata",
                    &entry,
                    "multiple storage pointers declare this logical object",
                );
            }
        }
    }
    report.expected_objects = expected.len();
    progress.stage("local checksums");
    let cpu_workers = std::thread::available_parallelism().map_or(1, usize::from);
    let local_results = crate::native_versions::bounded_map_with_workers(
        &local_entries,
        cpu_workers,
        false,
        |entry| {
            let mut partial = AuditReport::new();
            let hashes = inspect_local_entry(repo, entry, &mut partial);
            progress.completed("local checksums", local_entries.len());
            Ok((entry.object.clone(), hashes, partial))
        },
    )?;
    let mut local_hashes = BTreeMap::new();
    for (object, hashes, partial) in local_results {
        if let Some(hashes) = hashes {
            local_hashes.insert(object, hashes);
        }
        report.issues.extend(partial.issues);
    }
    progress.stage("S3 version inventory");

    // Registry objects are exceptions only when a local receipt identifies
    // their exact canonical key. An arbitrary control-looking key is extra.
    let mut outer_receipts = BTreeMap::new();
    let registries = expected_registries(
        repo,
        scopes,
        retired_scopes,
        all,
        &mut report,
        &mut outer_receipts,
        &mut metadata_snapshot,
    )?;
    let root_prefix = client.key_for("");
    let inventory = load_inventory(client, scopes, retired_scopes, all, &registries)?;
    let verified_registries = inspect_registries(repo, client, &registries, &mut report)?;
    let retained = inspect_retained_archive_history(
        client,
        &outer_receipts,
        &verified_registries,
        &inventory,
        &expected,
        &mut report,
    )?;
    let mut objects = BTreeMap::<String, Vec<&Value>>::new();
    for row in &inventory {
        let key = row["Key"]
            .as_str()
            .ok_or_else(|| Error::message("S3 inventory has no key"))?;
        let path = key
            .strip_prefix(&root_prefix)
            .ok_or_else(|| Error::message("S3 inventory escaped the configured logical root"))?;
        if path.starts_with(".workspace-mgr/") {
            if registries.contains_key(path) {
                report.remote_versions += 1;
            } else if all {
                report.remote_versions += 1;
                report.issue(
                    "unexpected-control-object",
                    path,
                    "remote control object has no matching local archive receipt",
                );
                report.issues.last_mut().unwrap().version =
                    row["VersionId"].as_str().map(str::to_owned);
            }
            continue;
        }
        if !selected(path, scopes, retired_scopes, all) {
            continue;
        }
        report.remote_versions += 1;
        if !expected.contains_key(path)
            && !retained.contains(&(
                path.to_owned(),
                row["VersionId"].as_str().unwrap_or("").to_owned(),
            ))
        {
            report.issue(
                "unexpected-object",
                path,
                if row["delete_marker"] == true {
                    "obsolete or misplaced remote delete marker exists outside the local storage tree"
                } else {
                    "obsolete or misplaced remote object version exists outside the local storage tree"
                },
            );
            report.issues.last_mut().unwrap().version =
                row["VersionId"].as_str().map(str::to_owned);
        }
        objects.entry(path.to_owned()).or_default().push(row);
    }

    progress.stage("exact S3 versions and checksums");
    let entries = expected.values().collect::<Vec<_>>();
    let remote_results = crate::native_versions::bounded_map(&entries, |entry| {
        let rows = objects.get(&entry.object).map(Vec::as_slice).unwrap_or(&[]);
        let mut partial = AuditReport::new();
        inspect_remote_entry(
            repo,
            client,
            entry,
            rows,
            local_hashes.get(&entry.object),
            &mut partial,
        );
        progress.completed("exact S3 versions and checksums", entries.len());
        Ok(partial)
    })?;
    for partial in remote_results {
        report.remote_checksum_objects += partial.remote_checksum_objects;
        report.verified_version_objects += partial.verified_version_objects;
        report.streamed_objects += partial.streamed_objects;
        report.streamed_bytes += partial.streamed_bytes;
        report.issues.extend(partial.issues);
    }
    // Version listing is not an atomic snapshot. Never report success when
    // the compared logical inventory visibly changed during this audit.
    progress.stage("final remote and local snapshots");
    let after = load_inventory(client, scopes, retired_scopes, all, &registries)?;
    if signature(
        &inventory,
        &root_prefix,
        scopes,
        retired_scopes,
        all,
        &registries,
    )? != signature(
        &after,
        &root_prefix,
        scopes,
        retired_scopes,
        all,
        &registries,
    )? {
        report.issue(
            "remote-inventory-changed",
            "",
            "S3 object inventory changed during diagnosis; rerun doctor against a stable task",
        );
    }
    for (pointer, before) in metadata_snapshot {
        if fs::read(repo.root.join(&pointer))
            .map(|after| after != before)
            .unwrap_or(true)
        {
            report.issue("local-metadata-changed", &pointer, "local storage metadata changed or disappeared during diagnosis; rerun doctor against a stable task");
        }
    }
    for (boundary, before) in local_snapshot {
        if local_state(repo, &boundary)
            .map(|after| after != before)
            .unwrap_or(true)
        {
            report.issue("local-state-changed", &boundary, "local payload paths, types, sizes, or timestamps changed during diagnosis; rerun doctor against a stable task");
        }
    }
    report.issues.sort_by(|left, right| {
        (&left.path, &left.code, &left.version).cmp(&(&right.path, &right.code, &right.version))
    });
    report.status = if report.issues.is_empty() {
        "ok"
    } else {
        "error"
    }
    .into();
    Ok(report)
}

fn inspect_registries(
    repo: &GitRepo,
    client: &S3Client,
    registries: &BTreeMap<String, Value>,
    report: &mut AuditReport,
) -> Result<BTreeSet<String>> {
    let registry_entries = registries.iter().collect::<Vec<_>>();
    // Registry histories have their own workers. Split the same network
    // budget across sources and their immutable versions rather than nesting
    // two independent pools of sixteen.
    let registry_workers = registry_entries.len().clamp(1, 16);
    let registry_results = crate::native_versions::bounded_map_with_workers(
        &registry_entries,
        registry_workers,
        false,
        |(path, receipt)| {
            let mut partial = AuditReport::new();
            let mut verified = false;
            let source = receipt["source"]
                .as_str()
                .expect("validated receipt source");
            match crate::native_archive::registry_read_with_parallelism(
                client,
                repo,
                source,
                16 / registry_workers,
            ) {
                Ok(Some(remote)) if remote == **receipt => verified = true,
                Ok(_) => partial.issue(
                    "archive-registry-mismatch",
                    path,
                    "remote archive registry differs from the local copied receipt or is missing",
                ),
                Err(error) => partial.issue("archive-registry-mismatch", path, error.to_string()),
            }
            Ok(((*path).clone(), verified, partial))
        },
    )?;
    let mut verified = BTreeSet::new();
    for (path, valid, partial) in registry_results {
        report.issues.extend(partial.issues);
        if valid {
            verified.insert(path);
        }
    }
    Ok(verified)
}

/// Only the current outer receipt names versions physically retained at the
/// present destination. Previous receipts keep registry/alias checks but their
/// intermediate versions may already have moved again. No local payload is
/// required for this separately verified historical inventory.
fn inspect_retained_archive_history(
    client: &S3Client,
    outer: &BTreeMap<String, Value>,
    verified_registries: &BTreeSet<String>,
    inventory: &[Value],
    expected: &BTreeMap<String, StorageEntry>,
    report: &mut AuditReport,
) -> Result<BTreeSet<(String, String)>> {
    let present = inventory
        .iter()
        .filter_map(|row| {
            Some((
                (
                    row["Key"].as_str()?.to_owned(),
                    row["VersionId"].as_str()?.to_owned(),
                ),
                row,
            ))
        })
        .collect::<BTreeMap<_, _>>();
    let mut entries = BTreeMap::new();
    for (registry, receipt) in outer {
        if !verified_registries.contains(registry) {
            continue;
        }
        let pointer = format!(
            "{}/{}",
            receipt["destination"].as_str().unwrap_or(""),
            archive_migration::RECEIPT_NAME
        );
        for row in receipt["versions"].as_array().into_iter().flatten() {
            let Some(object) = row["destination_object"].as_str() else {
                continue;
            };
            let Some(version) = row["destination_version_id"].as_str() else {
                continue;
            };
            let identity = (object.to_owned(), version.to_owned());
            if entries
                .insert(identity, (pointer.clone(), row.clone()))
                .is_some_and(|previous| previous.1 != *row)
            {
                report.issue(
                    "invalid-archive-receipt",
                    &pointer,
                    "current receipts disagree about a retained destination version",
                );
            }
        }
    }
    let entries = entries.into_iter().collect::<Vec<_>>();
    let results = crate::native_versions::bounded_map(
        &entries,
        |((object, version), (pointer, row))| {
            let mut partial = AuditReport::new();
            let issue = |partial: &mut AuditReport, code: &str, detail: String| {
                partial.issue(code, object, detail);
                let issue = partial.issues.last_mut().unwrap();
                issue.pointer = Some(pointer.clone());
                issue.version = Some(version.clone());
            };
            let Some(recorded) = present.get(&(client.key_for(object), version.clone())) else {
                issue(&mut partial, "retained-archive-version-missing", "copied historical version or delete marker is absent at its current receipt destination".into());
                return Ok((None, false, partial));
            };
            let marker = row["delete_marker"] == true;
            if recorded["delete_marker"] != marker
                || !marker
                    && (recorded["Size"] != row["size"]
                        || recorded["ETag"].as_str().map(tag)
                            != row["destination_etag"].as_str().map(tag))
            {
                issue(&mut partial, "retained-archive-version-mismatch", "retained history inventory differs from its exact copied size, ETag, or marker binding".into());
                return Ok((None, false, partial));
            }
            let current = expected
                .get(object)
                .is_some_and(|entry| entry.version_id.as_deref() == Some(version));
            if !marker && !current {
                let mut args = json!({"Bucket":client.bucket,"Key":client.key_for(object),"VersionId":version});
                if let Some(etag) = row["destination_etag"].as_str() {
                    args["IfMatch"] = format!("\"{}\"", tag(etag)).into();
                }
                match client.call_s3("head_object", &args, None) {
                    Ok(head)
                        if head.value["VersionId"] == *version
                            && head.value["DeleteMarker"] != true
                            && head.value["ContentLength"] == row["size"]
                            && head.value["ETag"].as_str().map(tag)
                                == row["destination_etag"].as_str().map(tag) => {}
                    Ok(_) => {
                        issue(&mut partial, "retained-archive-version-mismatch", "exact retained-history HEAD differs from its copied version, size, or ETag".into());
                        return Ok((None, false, partial));
                    }
                    Err(error) => {
                        issue(
                            &mut partial,
                            "retained-archive-read-failed",
                            error.to_string(),
                        );
                        return Ok((None, false, partial));
                    }
                }
            }
            Ok((Some((object.clone(), version.clone())), !current, partial))
        },
    )?;
    let mut retained = BTreeSet::new();
    for (identity, historical, partial) in results {
        report.issues.extend(partial.issues);
        if let Some(identity) = identity {
            retained.insert(identity);
            report.retained_archive_versions += usize::from(historical);
        }
    }
    Ok(retained)
}

fn local_state(repo: &GitRepo, boundary: &str) -> Result<BTreeMap<String, String>> {
    reject_symlink_traversal(&repo.root, boundary, "doctor local state")?;
    let root = repo.root.join(boundary);
    let metadata = match fs::symlink_metadata(&root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(source) => return Err(Error::Io { path: root, source }),
    };
    let mut state = BTreeMap::new();
    let stamp = |metadata: &fs::Metadata| {
        let basic = format!(
            "{}:{}:{}:{:?}",
            metadata.is_file(),
            metadata.is_dir(),
            metadata.len(),
            metadata.modified().ok()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            format!(
                "{basic}:{}:{}:{}:{}",
                metadata.dev(),
                metadata.ino(),
                metadata.ctime(),
                metadata.ctime_nsec()
            )
        }
        #[cfg(not(unix))]
        {
            basic
        }
    };
    state.insert(boundary.to_owned(), stamp(&metadata));
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        for row in WalkDir::new(&root)
            .follow_links(false)
            .follow_root_links(false)
        {
            let row = row.map_err(|error| Error::message(error.to_string()))?;
            let relative = row
                .path()
                .strip_prefix(&repo.root)
                .map_err(|_| Error::message("doctor local state escaped repository"))?;
            let metadata = fs::symlink_metadata(row.path()).at(row.path())?;
            state.insert(to_slash(relative), stamp(&metadata));
        }
    }
    Ok(state)
}

pub(crate) fn discover_pointers(
    repo: &GitRepo,
    scopes: &[String],
    all: bool,
) -> Result<Vec<String>> {
    Ok(discover_control_files(repo, scopes, all)?
        .into_iter()
        .filter(|path| storage_metadata::is_pointer(path))
        .collect())
}

fn discover_control_files(repo: &GitRepo, scopes: &[String], all: bool) -> Result<Vec<String>> {
    let mut pointers = repo
        .visible_paths(if all { &[] } else { scopes })?
        .into_iter()
        .filter(|path| {
            storage_metadata::is_pointer(path)
                || path.ends_with(&format!("/{}", archive_migration::RECEIPT_NAME))
        })
        .collect::<BTreeSet<_>>();
    let roots = if scopes.is_empty() && all {
        vec![String::new()]
    } else {
        scopes.to_vec()
    };
    for scope in &roots {
        // A storage scope can itself be a file/directory boundary; its
        // adjacent control sidecar lies outside a walk of that boundary.
        for candidate in [
            storage_metadata::pointer_path(scope),
            format!("{scope}.dvc"),
        ] {
            if fs::symlink_metadata(repo.root.join(&candidate)).is_ok() {
                pointers.insert(candidate);
            }
        }
        let root = repo.root.join(scope);
        if fs::symlink_metadata(&root).is_err() {
            continue;
        }
        if root.is_dir()
            && [
                storage_metadata::pointer_path(scope),
                format!("{scope}.dvc"),
            ]
            .iter()
            .any(|pointer| fs::symlink_metadata(repo.root.join(pointer)).is_ok())
        {
            continue;
        }
        reject_symlink_traversal(&repo.root, scope, "doctor metadata scope")?;
        for row in WalkDir::new(&root)
            .follow_links(false)
            .follow_root_links(false)
            .into_iter()
            .filter_entry(|entry| {
                let path = entry.path();
                if path == root {
                    return true;
                }
                if entry.file_type().is_dir() {
                    if entry.file_name() == ".git"
                        || path
                            .strip_prefix(&repo.root)
                            .is_ok_and(|relative| relative.starts_with(".workspace-mgr/local"))
                        || fs::symlink_metadata(path.join(".git")).is_ok_and(|metadata| {
                            metadata.is_dir()
                                || metadata.file_type().is_symlink()
                                || metadata.len() > 0
                        })
                    {
                        return false;
                    }
                    // Declared storage boundaries contain opaque payloads. A
                    // sidecar-shaped payload inside them is not another pointer.
                    if path.to_str().is_some_and(|path| {
                        fs::symlink_metadata(format!("{path}{}", crate::storage_format::SUFFIX))
                            .is_ok()
                            || fs::symlink_metadata(format!("{path}.dvc")).is_ok()
                    }) {
                        return false;
                    }
                }
                true
            })
        {
            let row = row.map_err(|error| {
                Error::message(format!("inspect doctor storage metadata: {error}"))
            })?;
            if row.file_type().is_dir() {
                continue;
            }
            let relative = row
                .path()
                .strip_prefix(&repo.root)
                .map_err(|_| Error::message("doctor metadata escaped repository"))?;
            let path = to_slash(relative);
            if storage_metadata::is_pointer(&path)
                || path.ends_with(&format!("/{}", archive_migration::RECEIPT_NAME))
            {
                pointers.insert(path);
            }
        }
    }
    // Git-visible files can also be payloads, including force-tracked legacy
    // sidecar-shaped bytes. Resolve outer directory boundaries first before
    // interpreting any candidate inside them as control metadata.
    let mut candidates = pointers.into_iter().collect::<Vec<_>>();
    candidates.sort_by_key(|path| (path.matches('/').count(), path.clone()));
    let mut opaque = Vec::new();
    let mut result = Vec::new();
    for candidate in candidates {
        if contains(&opaque, &candidate) {
            continue;
        }
        if let Some(boundary) = storage_metadata::boundary_path(&candidate) {
            let directory =
                reject_symlink_traversal(&repo.root, &candidate, "doctor storage metadata")
                    .and_then(|()| {
                        let path = repo.root.join(&candidate);
                        let metadata = fs::symlink_metadata(&path).at(&path)?;
                        if !metadata.is_file() || metadata.file_type().is_symlink() {
                            return Err(Error::message("storage metadata must be a regular file"));
                        }
                        fs::read_to_string(&path).at(&path)
                    })
                    .ok()
                    .and_then(|raw| storage_metadata::parse_pointer_document(&raw, &candidate).ok())
                    .is_some_and(|document| {
                        document.outs.iter().any(|output| {
                            output.files.is_some()
                                || output
                                    .md5
                                    .as_deref()
                                    .is_some_and(|digest| digest.ends_with(".dir"))
                        })
                    });
            if directory || repo.root.join(boundary).is_dir() {
                opaque.push(boundary.to_owned());
            }
        }
        result.push(candidate);
    }
    result.sort();
    Ok(result)
}

fn load_inventory(
    client: &S3Client,
    scopes: &[String],
    retired: &[String],
    all: bool,
    registries: &BTreeMap<String, Value>,
) -> Result<Vec<Value>> {
    if all {
        return client.list_versions(&client.key_for(""));
    }
    let mut prefixes = scopes
        .iter()
        .chain(retired)
        .cloned()
        .collect::<BTreeSet<_>>();
    prefixes.extend(registries.keys().cloned());
    let roots = prefixes
        .iter()
        .filter(|scope| {
            !prefixes
                .iter()
                .any(|parent| parent != *scope && contains(std::slice::from_ref(parent), scope))
        })
        .cloned()
        .collect::<Vec<_>>();
    let remote_root = client.key_for("");
    let mut result = BTreeMap::new();
    let inventories = crate::native_versions::bounded_map(&roots, |prefix| {
        client.list_versions(&client.key_for(prefix))
    })?;
    for inventory in inventories {
        for row in inventory {
            let key = row["Key"]
                .as_str()
                .ok_or_else(|| Error::message("S3 inventory has no key"))?;
            let path = key
                .strip_prefix(&remote_root)
                .ok_or_else(|| Error::message("S3 inventory escaped logical root"))?;
            // Prefix matching on S3 is bytewise, so a query for `task` can
            // also return `task-neighbor`. Only exact logical scopes count.
            if !selected(path, scopes, retired, false) && !registries.contains_key(path) {
                continue;
            }
            let version = row["VersionId"]
                .as_str()
                .ok_or_else(|| Error::message("S3 inventory has no exact version"))?;
            let identity = (key.to_owned(), version.to_owned());
            if result
                .insert(identity, row.clone())
                .is_some_and(|previous| previous != row)
            {
                return Err(Error::message(
                    "overlapping S3 inventories disagree about an exact version",
                ));
            }
        }
    }
    Ok(result.into_values().collect())
}

fn inspect_local_boundary(
    repo: &GitRepo,
    pointer: &str,
    entries: &[StorageEntry],
    report: &mut AuditReport,
) {
    let boundary = storage_metadata::boundary_path(pointer).expect("discovered pointer");
    let path = repo.root.join(boundary);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            report.unmaterialized_boundaries += 1;
            return;
        }
        Err(error) => {
            report.issue("local-layout-mismatch", boundary, error.to_string());
            return;
        }
    };
    report.materialized_boundaries += 1;
    if metadata.file_type().is_symlink()
        || reject_symlink_traversal(&repo.root, boundary, "doctor local payload").is_err()
    {
        report.issue(
            "local-layout-mismatch",
            boundary,
            "local payload traverses a symbolic link",
        );
        return;
    }
    let raw = match fs::read_to_string(repo.root.join(pointer)) {
        Ok(raw) => raw,
        Err(error) => {
            report.issue("invalid-metadata", pointer, error.to_string());
            return;
        }
    };
    let directory = match storage_metadata::parse_pointer_document(&raw, pointer) {
        Ok(document) => document
            .outs
            .first()
            .is_some_and(|output| output.files.is_some()),
        Err(error) => {
            report.issue("invalid-metadata", pointer, error.to_string());
            return;
        }
    };
    if directory != metadata.is_dir() || (!directory && !metadata.is_file()) {
        report.issue(
            "local-layout-mismatch",
            boundary,
            "local payload type differs from its storage metadata",
        );
        return;
    }
    if directory {
        let mut actual = BTreeSet::new();
        for row in WalkDir::new(&path).follow_links(false) {
            match row {
                Ok(row) if row.file_type().is_file() => {
                    if let Ok(relative) = row.path().strip_prefix(&repo.root) {
                        actual.insert(to_slash(relative));
                    }
                }
                Ok(row) if row.file_type().is_symlink() => report.issue(
                    "local-layout-mismatch",
                    &to_slash(row.path().strip_prefix(&repo.root).unwrap_or(row.path())),
                    "storage directory contains a symbolic link",
                ),
                Ok(row) if !row.file_type().is_dir() => report.issue(
                    "local-layout-mismatch",
                    boundary,
                    "storage directory contains a non-regular payload",
                ),
                Ok(_) => {}
                Err(error) => report.issue("local-layout-mismatch", boundary, error.to_string()),
            }
        }
        let wanted = entries
            .iter()
            .map(|entry| entry.object.clone())
            .collect::<BTreeSet<_>>();
        for missing in wanted.difference(&actual) {
            report.issue(
                "local-layout-mismatch",
                missing,
                "materialized directory is missing a file declared in its metadata",
            );
        }
        for extra in actual.difference(&wanted) {
            report.issue(
                "local-layout-mismatch",
                extra,
                "materialized directory contains a file absent from its metadata",
            );
        }
    }
}

fn inspect_local_entry(
    repo: &GitRepo,
    entry: &StorageEntry,
    report: &mut AuditReport,
) -> Option<native_engine::FileHashes> {
    let local = repo.root.join(&entry.object);
    if !local.is_file()
        || reject_symlink_traversal(&repo.root, &entry.object, "doctor local payload").is_err()
    {
        return None;
    }
    let checked = fs::metadata(&local).at(&local).and_then(|metadata| {
        let hashes = native_engine::HashInventory::new().hashes(&local)?;
        if entry.size != Some(metadata.len())
            || entry.md5.as_deref() != Some(hashes.digest(&entry.hash_name)?)
            || entry.verification.as_ref().is_some_and(|proof| {
                proof.size != metadata.len() || proof.checksum.digest != hashes.sha256
            })
        {
            report.entry_issue(
                "local-content-mismatch",
                entry,
                "local payload checksum or physical size differs from its metadata",
            );
        }
        Ok(hashes)
    });
    match checked {
        Ok(hashes) => Some(hashes),
        Err(error) => {
            report.entry_issue("local-content-mismatch", entry, error.to_string());
            None
        }
    }
}

fn tag(raw: &str) -> &str {
    raw.trim_matches('"')
}

fn inspect_remote_entry(
    repo: &GitRepo,
    client: &S3Client,
    entry: &StorageEntry,
    rows: &[&Value],
    local_hashes: Option<&native_engine::FileHashes>,
    report: &mut AuditReport,
) {
    let Some(version) = entry
        .version_id
        .as_deref()
        .filter(|version| !version.is_empty() && *version != "null")
    else {
        return;
    };
    let original_issues = report.issues.len();
    let latest = rows
        .iter()
        .filter(|row| row["IsLatest"] == true)
        .collect::<Vec<_>>();
    if latest.len() != 1 || latest[0]["VersionId"] != version || latest[0]["delete_marker"] == true
    {
        report.entry_issue("remote-latest-mismatch", entry, "remote latest object version differs from the exact version declared by local metadata");
    }
    let recorded = rows
        .iter()
        .find(|row| row["VersionId"] == version && row["delete_marker"] != true);
    let Some(recorded) = recorded else {
        report.entry_issue(
            "remote-version-missing",
            entry,
            "declared exact version is absent at the corresponding local logical path",
        );
        return;
    };
    if recorded["Size"].as_u64() != entry.size {
        report.entry_issue(
            "remote-size-mismatch",
            entry,
            "S3 version inventory physical size differs from local metadata",
        );
    }
    if entry
        .etag
        .as_deref()
        .is_some_and(|expected| recorded["ETag"].as_str().map(tag) != Some(tag(expected)))
    {
        report.entry_issue(
            "remote-etag-mismatch",
            entry,
            "S3 version inventory ETag differs from local metadata",
        );
    }
    let mut args =
        json!({"Bucket":client.bucket,"Key":client.key_for(&entry.object),"VersionId":version});
    if let Some(etag) = &entry.etag {
        args["IfMatch"] = format!("\"{}\"", tag(etag)).into();
    }
    let mut head_args = args.clone();
    head_args["ChecksumMode"] = "ENABLED".into();
    let local = repo.root.join(&entry.object);
    // Missing local hashes do not establish that the payload is absent: a
    // failed local read must still receive the streamed byte comparison.
    let local_absent = fs::symlink_metadata(&local)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
    if let Some(proof) = &entry.verification {
        // Schema 2 records a previously established raw SHA256 binding to
        // this exact immutable storage version. Reading object metadata is
        // sufficient to re-use that proof; a payload read cannot be the
        // recovery path for an invalid binding or a failed metadata check.
        if proof.endpoint != client.endpoint_identity()
            || proof.bucket != client.bucket
            || proof.key != client.key_for(&entry.object)
            || proof.version_id != version
            || Some(proof.size) != entry.size
        {
            report.entry_issue(
                "remote-verification-binding-mismatch",
                entry,
                "verified content binding differs from the configured endpoint, bucket, key, exact version or physical size",
            );
            return;
        }
        let head = match client.head_with_checksums(&args) {
            Ok(head) => head,
            Err(error) => {
                report.entry_issue(
                    if error.status == Some(412) {
                        "remote-etag-mismatch"
                    } else {
                        "remote-read-failed"
                    },
                    entry,
                    error.to_string(),
                );
                return;
            }
        };
        validate_remote_response(entry, recorded, version, &head.value, report);
        if head.value["ChecksumType"] == "FULL_OBJECT"
            && let Some(raw) = head.value.get("ChecksumSHA256")
        {
            match raw
                .as_str()
                .and_then(|raw| STANDARD.decode(raw).ok())
                .filter(|bytes| bytes.len() == 32)
                .map(crate::hex::encode_lower)
            {
                Some(digest) if digest == proof.checksum.digest => {}
                Some(_) => report.entry_issue(
                    "remote-content-mismatch",
                    entry,
                    "S3 full-object SHA256 differs from the verified exact-version checksum",
                ),
                None => report.entry_issue(
                    "remote-checksum-invalid",
                    entry,
                    "S3 returned a malformed full-object SHA256 checksum",
                ),
            }
        }
        if let Some(hashes) = local_hashes {
            if hashes.sha256 != proof.checksum.digest {
                report.entry_issue(
                    "local-remote-bytes-mismatch",
                    entry,
                    "local raw-byte SHA256 differs from the verified exact-version checksum",
                );
            }
        } else if !local_absent {
            report.entry_issue(
                "local-content-unverified",
                entry,
                "materialized local payload could not be hashed against its verified exact-version checksum",
            );
        }
        if original_issues == report.issues.len() {
            report.verified_version_objects += 1;
        }
        return;
    }
    if let Ok(head) = client.call_s3("head_object", &head_args, None) {
        let prior_issues = report.issues.len();
        validate_remote_response(entry, recorded, version, &head.value, report);
        if prior_issues == report.issues.len()
            && (local_hashes.is_some() || local_absent)
            && checksum_proof(entry, &head.value, local_hashes, report)
        {
            report.remote_checksum_objects += 1;
            return;
        }
    }
    // Unsupported, absent, composite or weak checksums cannot replace the
    // manifest checksum check. Read the exact version once, hashing and
    // comparing raw local bytes while they arrive; no scratch write/fsync.
    let mut local_reader = if fs::symlink_metadata(&local)
        .is_ok_and(|m| m.is_file() && !m.file_type().is_symlink())
        && reject_symlink_traversal(&repo.root, &entry.object, "doctor local payload").is_ok()
    {
        match fs::File::open(&local).at(&local) {
            Ok(file) => Some(BufReader::new(file)),
            Err(error) => {
                report.entry_issue("local-remote-bytes-mismatch", entry, error.to_string());
                None
            }
        }
    } else {
        None
    };
    let mut different = false;
    let mut comparison = [0u8; 64 * 1024];
    let mut hashes = native_engine::FileHashStream::new();
    let response = match client.get_stream(&args, |chunk| {
        hashes.update(chunk);
        report.streamed_bytes += chunk.len() as u64;
        if let Some(reader) = &mut local_reader
            && (reader.read_exact(&mut comparison[..chunk.len()]).is_err()
                || comparison[..chunk.len()] != *chunk)
        {
            different = true;
        }
        Ok(())
    }) {
        Ok(response) => response.value,
        Err(error) => {
            report.entry_issue(
                if error.status == Some(412) {
                    "remote-etag-mismatch"
                } else {
                    "remote-read-failed"
                },
                entry,
                error.to_string(),
            );
            return;
        }
    };
    report.streamed_objects += 1;
    validate_remote_response(entry, recorded, version, &response, report);
    let remote_hashes = hashes.finish();
    match remote_hashes.digest(&entry.hash_name) {
        Ok(digest) if entry.md5.as_deref() == Some(digest) => {}
        Ok(_) => report.entry_issue(
            "remote-content-mismatch",
            entry,
            "exact remote payload checksum differs from local metadata",
        ),
        Err(error) => report.entry_issue("remote-content-mismatch", entry, error.to_string()),
    }
    if let Some(reader) = &mut local_reader {
        let mut extra = [0u8; 1];
        if !matches!(reader.read(&mut extra), Ok(0)) {
            different = true;
        }
        if fs::metadata(&local)
            .map(|m| Some(m.len()) != entry.size)
            .unwrap_or(true)
        {
            different = true;
        }
    }
    if different {
        report.entry_issue(
            "local-remote-bytes-mismatch",
            entry,
            "materialized local payload differs byte for byte from its exact remote version",
        );
    }
}

fn validate_remote_response(
    entry: &StorageEntry,
    recorded: &Value,
    version: &str,
    response: &Value,
    report: &mut AuditReport,
) {
    if response["VersionId"] != version || response["DeleteMarker"] == true {
        report.entry_issue(
            "remote-version-mismatch",
            entry,
            "S3 did not return the requested exact payload version",
        );
    }
    if response["ContentLength"].as_u64() != entry.size {
        report.entry_issue(
            "remote-size-mismatch",
            entry,
            "remote payload physical size differs from local metadata",
        );
    }
    if response["ContentLength"].as_u64() != recorded["Size"].as_u64()
        || response["ETag"].as_str().map(tag) != recorded["ETag"].as_str().map(tag)
    {
        report.entry_issue(
            "remote-inventory-mismatch",
            entry,
            "exact S3 metadata differs from its version inventory",
        );
    }
    if entry
        .etag
        .as_deref()
        .is_some_and(|expected| response["ETag"].as_str().map(tag) != Some(tag(expected)))
    {
        report.entry_issue(
            "remote-etag-mismatch",
            entry,
            "remote payload ETag differs from local metadata",
        );
    }
}

fn checksum_proof(
    entry: &StorageEntry,
    response: &Value,
    local: Option<&native_engine::FileHashes>,
    report: &mut AuditReport,
) -> bool {
    if response["ChecksumType"] != "FULL_OBJECT" || (!cfg!(unix) && local.is_some()) {
        return false;
    }
    let decode = |field: &str, size| {
        response[field]
            .as_str()
            .and_then(|raw| STANDARD.decode(raw).ok())
            .filter(|bytes| bytes.len() == size)
            .map(crate::hex::encode_lower)
    };
    let md5 = decode("ChecksumMD5", 16);
    let sha256 = decode("ChecksumSHA256", 32);
    if entry.hash_name == "md5"
        && let Some(remote) = md5.as_deref()
        && entry.md5.as_deref() != Some(remote)
    {
        report.entry_issue(
            "remote-content-mismatch",
            entry,
            "S3 full-object MD5 differs from the manifest checksum",
        );
    }
    let Some(local) = local else {
        // This replaces only the remote manifest MD5 calculation. There are
        // no materialized bytes whose literal equality also needs checking.
        return entry.hash_name == "md5" && md5.is_some();
    };
    // Normalized manifests alone cannot establish a raw-byte remote digest.
    // Matching MD5 alone also cannot replace the existing local/remote byte
    // comparison: distinct known MD5 collisions must still be detected. Only
    // a matching full-object SHA256 can bridge checked local raw bytes to the
    // remote version; missing or differing SHA256 retains the streamed check.
    let md5_differs = md5.as_deref().is_some_and(|digest| local.md5 != digest);
    let sha256_differs = sha256
        .as_deref()
        .is_some_and(|digest| local.sha256 != digest);
    if md5_differs || sha256_differs {
        report.entry_issue(
            "local-remote-bytes-mismatch",
            entry,
            "local raw-byte checksum differs from the exact S3 version's full-object checksum",
        );
    }
    if sha256.as_deref() != Some(local.sha256.as_str()) || md5_differs {
        return false;
    }
    match local.digest(&entry.hash_name) {
        Ok(digest) if entry.md5.as_deref() == Some(digest) => {}
        _ => report.entry_issue(
            "remote-content-mismatch",
            entry,
            "exact remote raw-byte checksum matches local bytes whose manifest checksum differs",
        ),
    }
    true
}

struct AuditProgress {
    enabled: bool,
    completed: AtomicUsize,
    last: Mutex<Instant>,
}
impl AuditProgress {
    fn new() -> Self {
        Self {
            enabled: std::io::stderr().is_terminal(),
            completed: AtomicUsize::new(0),
            last: Mutex::new(Instant::now()),
        }
    }
    fn stage(&self, name: &str) {
        self.completed.store(0, Ordering::Relaxed);
        if self.enabled {
            eprintln!("workspace-mgr doctor: {name}");
        }
    }
    fn completed(&self, name: &str, total: usize) {
        let completed = self.completed.fetch_add(1, Ordering::Relaxed) + 1;
        if !self.enabled {
            return;
        }
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if completed == total || last.elapsed() >= Duration::from_secs(1) {
            eprintln!("workspace-mgr doctor: {name}: {completed}/{total}");
            *last = Instant::now();
        }
    }
}

fn expected_registries(
    repo: &GitRepo,
    scopes: &[String],
    retired: &[String],
    all: bool,
    report: &mut AuditReport,
    outer: &mut BTreeMap<String, Value>,
    metadata_snapshot: &mut BTreeMap<String, Vec<u8>>,
) -> Result<BTreeMap<String, Value>> {
    let mut result = BTreeMap::new();
    let receipt_scopes = scopes.iter().chain(retired).cloned().collect::<Vec<_>>();
    for path in discover_control_files(repo, &receipt_scopes, all)? {
        if !path.ends_with(&format!("/{}", archive_migration::RECEIPT_NAME)) {
            continue;
        }
        let task = path
            .strip_suffix(&format!("/{}", archive_migration::RECEIPT_NAME))
            .unwrap();
        if !selected(task, scopes, retired, all) {
            continue;
        }
        let receipt = reject_symlink_traversal(&repo.root, &path, "doctor archive receipt")
            .and_then(|()| {
                let absolute = repo.root.join(&path);
                let metadata = fs::symlink_metadata(&absolute).at(&absolute)?;
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    return Err(Error::message("archive receipt must be a regular file"));
                }
                fs::read_to_string(&absolute).at(&absolute)
            })
            .and_then(|raw| {
                let receipt = serde_json::from_str::<Value>(&raw)
                    .map_err(|error| Error::message(error.to_string()))?;
                archive_migration::validate(&path, &receipt)?;
                Ok((raw.into_bytes(), receipt))
            });
        match receipt {
            Ok((raw, receipt)) => {
                metadata_snapshot.insert(path, raw);
                if receipt["status"] == "copied" {
                    outer.insert(registry_path(&receipt), receipt.clone());
                }
                collect_registries(&receipt, &mut result, report, 0);
            }
            Err(error) => report.issue("invalid-archive-receipt", &path, error.to_string()),
        }
    }
    Ok(result)
}

fn registry_path(receipt: &Value) -> String {
    format!(
        ".workspace-mgr/archive/{}.json",
        crate::hex::encode_lower(Sha256::digest(
            receipt["source"].as_str().unwrap_or("").as_bytes()
        ))
    )
}

fn collect_registries(
    receipt: &Value,
    result: &mut BTreeMap<String, Value>,
    report: &mut AuditReport,
    depth: usize,
) {
    if depth >= 32 {
        report.issue(
            "invalid-archive-receipt",
            "",
            "archive receipt history exceeds the supported nesting depth",
        );
        return;
    }
    let receipt_path = format!(
        "{}/{}",
        receipt["destination"].as_str().unwrap_or(""),
        archive_migration::RECEIPT_NAME
    );
    if let Err(error) = archive_migration::validate(&receipt_path, receipt) {
        report.issue("invalid-archive-receipt", &receipt_path, error.to_string());
        return;
    }
    if receipt["status"] == "copied" {
        if receipt["source"].is_string() {
            let path = registry_path(receipt);
            if result
                .insert(path.clone(), receipt.clone())
                .is_some_and(|previous| previous != *receipt)
            {
                report.issue(
                    "invalid-archive-receipt",
                    &path,
                    "local archive receipts disagree about one canonical registry",
                );
            }
        }
    } else {
        report.issue(
            "archive-not-published",
            receipt["destination"].as_str().unwrap_or(""),
            "planned archive metadata has not completed its exact S3 migration",
        );
    }
    if let Some(previous) = receipt
        .get("previous_receipt")
        .filter(|previous| !previous.is_null())
    {
        collect_registries(previous, result, report, depth + 1);
    }
}

fn signature(
    rows: &[Value],
    root: &str,
    scopes: &[String],
    retired: &[String],
    all: bool,
    registries: &BTreeMap<String, Value>,
) -> Result<BTreeMap<(String, String), String>> {
    let mut result = BTreeMap::new();
    for row in rows {
        let key = row["Key"]
            .as_str()
            .ok_or_else(|| Error::message("S3 inventory has no key"))?;
        let path = key
            .strip_prefix(root)
            .ok_or_else(|| Error::message("S3 inventory escaped configured root"))?;
        if !selected(path, scopes, retired, all) && !registries.contains_key(path) {
            continue;
        }
        let version = row["VersionId"]
            .as_str()
            .ok_or_else(|| Error::message("S3 inventory has no exact version"))?;
        let value = json!([
            row["IsLatest"],
            row["delete_marker"],
            row["Size"],
            row["ETag"]
        ]);
        result.insert((path.into(), version.into()), value.to_string());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_s3::tests::{Reply, RoutedFixture, configure_repo, replayable_read_fixture};
    use crate::storage_format::{
        Checksum, Entry, Kind, Manifest, Verification, Version, directory_digest,
    };
    use std::io::Cursor;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct Fixture {
        _temp: tempfile::TempDir,
        repo: GitRepo,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let repo = GitRepo {
                root: temp.path().to_path_buf(),
            };
            repo.run(["init", "-q", "-b", "main"]).unwrap();
            Self { _temp: temp, repo }
        }

        fn file(&self, object: &str, body: &[u8], algorithm: &str, materialized: bool) {
            let checksum = checksum(body, algorithm);
            let manifest = Manifest {
                schema_version: 1,
                path: object.rsplit('/').next().unwrap().into(),
                kind: Kind::File,
                checksum,
                size: body.len() as u64,
                version: Some(Version {
                    id: "v1".into(),
                    etag: Some("remote-etag".into()),
                    verification: None,
                }),
                entries: None,
            };
            self.manifest(object, &manifest);
            if materialized {
                fs::write(self.repo.root.join(object), body).unwrap();
            }
        }

        fn manifest(&self, object: &str, manifest: &Manifest) {
            let pointer = self.repo.root.join(storage_metadata::pointer_path(object));
            fs::create_dir_all(pointer.parent().unwrap()).unwrap();
            fs::write(pointer, manifest.serialize().unwrap()).unwrap();
        }

        fn verified_file(
            &self,
            object: &str,
            body: &[u8],
            materialized: bool,
            client: &S3Client,
        ) -> Manifest {
            let manifest = Manifest {
                schema_version: 2,
                path: object.rsplit('/').next().unwrap().into(),
                kind: Kind::File,
                checksum: checksum(body, "md5-dos2unix"),
                size: body.len() as u64,
                version: Some(Version {
                    id: "v1".into(),
                    etag: Some("remote-etag".into()),
                    verification: Some(Verification {
                        endpoint: client.endpoint_identity(),
                        bucket: client.bucket.clone(),
                        key: client.key_for(object),
                        version_id: "v1".into(),
                        checksum: Checksum {
                            algorithm: "sha256".into(),
                            digest: crate::hex::encode_lower(Sha256::digest(body)),
                        },
                        size: body.len() as u64,
                        method: "verified-upload".into(),
                    }),
                }),
                entries: None,
            };
            self.manifest(object, &manifest);
            if materialized {
                fs::write(self.repo.root.join(object), body).unwrap();
            }
            manifest
        }

        fn run(
            &self,
            rows: Vec<Value>,
            bodies: Vec<(String, Vec<u8>)>,
            all: bool,
            aliases: &[String],
        ) -> AuditReport {
            let (client, server) = server(rows, bodies);
            configure_repo(&client, &self.repo);
            let before = snapshot(&self.repo);
            let report = inspect_with(&self.repo, &["task".into()], all, aliases, &client).unwrap();
            assert_eq!(
                snapshot(&self.repo),
                before,
                "doctor changed local files or caches"
            );
            let requests = server.finish_requests();
            assert!(
                requests
                    .iter()
                    .all(|request| matches!(request.method.as_str(), "GET" | "HEAD"))
            );
            report
        }
    }

    fn checksum(body: &[u8], algorithm: &str) -> Checksum {
        Checksum {
            algorithm: algorithm.into(),
            digest: native_engine::stream_digest(
                &mut Cursor::new(body),
                Path::new("fixture"),
                algorithm,
            )
            .unwrap(),
        }
    }

    fn row(path: &str, version: &str, latest: bool, body: &[u8], marker: bool) -> Value {
        json!({"Key":format!("root/{path}"),"VersionId":version,"IsLatest":latest,"delete_marker":marker,"Size":body.len(),"ETag":"\"remote-etag\"","LastModified":"2026-10-08T00:00:00+00:00"})
    }

    fn listing(rows: &[Value], prefix: &str) -> String {
        let mut xml = "<ListVersionsResult><IsTruncated>false</IsTruncated>".to_owned();
        for row in rows
            .iter()
            .filter(|row| row["Key"].as_str().unwrap().starts_with(prefix))
        {
            let element = if row["delete_marker"] == true {
                "DeleteMarker"
            } else {
                "Version"
            };
            xml.push_str(&format!("<{element}><Key>{}</Key><VersionId>{}</VersionId><IsLatest>{}</IsLatest><LastModified>2026-10-08T00:00:00Z</LastModified><Size>{}</Size><ETag>{}</ETag></{element}>",row["Key"].as_str().unwrap(),row["VersionId"].as_str().unwrap(),row["IsLatest"],row["Size"],row["ETag"].as_str().unwrap()));
        }
        xml.push_str("</ListVersionsResult>");
        xml
    }

    fn server(rows: Vec<Value>, bodies: Vec<(String, Vec<u8>)>) -> (S3Client, RoutedFixture) {
        replayable_read_fixture(move |request| {
            let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
            let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
            if query.contains_key("versions") {
                return Arc::new(Reply::xml(&listing(&rows, &query["prefix"])));
            }
            let path = url.path().strip_prefix("/fixture-bucket/root/").unwrap();
            assert_eq!(query.get("versionId").map(String::as_str), Some("v1"));
            let body = bodies
                .iter()
                .find(|(object, _)| object == path)
                .unwrap()
                .1
                .clone();
            Arc::new(Reply {
                status: 200,
                headers: vec![
                    ("x-amz-version-id", "v1".into()),
                    ("etag", "\"remote-etag\"".into()),
                ],
                body,
            })
        })
    }

    fn snapshot(repo: &GitRepo) -> BTreeMap<String, Vec<u8>> {
        WalkDir::new(&repo.root)
            .into_iter()
            .filter_entry(|entry| entry.file_name() != ".git")
            .filter_map(|entry| {
                let entry = entry.unwrap();
                entry.file_type().is_file().then(|| {
                    (
                        to_slash(entry.path().strip_prefix(&repo.root).unwrap()),
                        fs::read(entry.path()).unwrap(),
                    )
                })
            })
            .collect()
    }

    fn has(report: &AuditReport, code: &str, path: &str) -> bool {
        report
            .issues
            .iter()
            .any(|issue| issue.code == code && issue.path == path)
    }

    fn historical_receipt() -> Value {
        json!({
            "schema_version":1,"status":"copied","task_id":"20261008-120000-task",
            "remote":"workspace-mgr","bucket":"fixture-bucket","remote_prefix":"root",
            "source":"old/task","destination":"task","transaction_id":"archive-copy",
            "versions":[
                {"source_object":"old/task/old-name.bin","destination_object":"task/old-name.bin",
                 "source_version_id":"old-source","source_last_modified":"2026-10-08T00:00:00+00:00",
                 "source_is_latest":true,"source_list_order":0,"delete_marker":false,"size":3,
                 "source_etag":"remote-etag","destination_version_id":"history-payload",
                 "destination_etag":"remote-etag","destination_last_modified":"2026-10-08T00:00:00+00:00"},
                {"source_object":"old/task/retired-name.bin","destination_object":"task/retired-name.bin",
                 "source_version_id":"old-marker","source_last_modified":"2026-10-08T00:00:00+00:00",
                 "source_is_latest":true,"source_list_order":1,"delete_marker":true,"size":null,
                 "source_etag":null,"destination_version_id":"history-marker",
                 "destination_etag":null,"destination_last_modified":"2026-10-08T00:00:00+00:00"}
            ]
        })
    }

    #[derive(Clone, Copy)]
    enum HistoryFault {
        None,
        MissingPayload,
        MissingMarker,
        Extra,
        HeadMismatch,
        InventoryMismatch,
        OldSource,
        TamperedReceipt,
        UnboundRenameKind,
        Nested,
    }

    fn retained_history_audit(fault: HistoryFault) -> AuditReport {
        let fixture = Fixture::new();
        fixture.file("task/current-name.bin", b"abc", "md5", true);
        let mut remote_receipt = historical_receipt();
        if matches!(fault, HistoryFault::Nested) {
            let mut previous = historical_receipt();
            previous["source"] = "original/task".into();
            previous["destination"] = "old/task".into();
            for row in previous["versions"].as_array_mut().unwrap() {
                let suffix = row["destination_object"]
                    .as_str()
                    .unwrap()
                    .strip_prefix("task/")
                    .unwrap()
                    .to_owned();
                row["source_object"] = format!("original/task/{suffix}").into();
                row["destination_object"] = format!("old/task/{suffix}").into();
            }
            remote_receipt["previous_receipt"] = previous;
        }
        let mut local_receipt = remote_receipt.clone();
        if matches!(fault, HistoryFault::TamperedReceipt) {
            local_receipt["versions"][0]["destination_version_id"] = "forged-history".into();
        }
        if matches!(fault, HistoryFault::UnboundRenameKind) {
            local_receipt["migration_kind"] = "task-rename".into();
        }
        fs::write(
            fixture.repo.root.join("task/.workspace-mgr-archive.json"),
            serde_json::to_vec(&local_receipt).unwrap(),
        )
        .unwrap();
        let mut registries = BTreeMap::new();
        collect_registries(&remote_receipt, &mut registries, &mut AuditReport::new(), 0);
        let mut rows = vec![row("task/current-name.bin", "v1", true, b"abc", false)];
        if !matches!(fault, HistoryFault::MissingPayload) {
            let mut payload = row("task/old-name.bin", "history-payload", true, b"old", false);
            if matches!(fault, HistoryFault::InventoryMismatch) {
                payload["Size"] = 99.into();
            }
            rows.push(payload);
        }
        if !matches!(fault, HistoryFault::MissingMarker) {
            rows.push(row(
                "task/retired-name.bin",
                "history-marker",
                true,
                b"",
                true,
            ));
        }
        if matches!(fault, HistoryFault::Extra) {
            rows.push(row(
                "task/old-name.bin",
                "unmapped-generation",
                false,
                b"extra",
                false,
            ));
            rows.push(row(
                "task/unmapped-name.bin",
                "unmapped-file",
                true,
                b"extra",
                false,
            ));
        }
        if matches!(fault, HistoryFault::OldSource) {
            rows.push(row(
                "old/task/old-name.bin",
                "old-source",
                true,
                b"old",
                false,
            ));
        }
        for (path, receipt) in &registries {
            rows.push(row(
                path,
                "registry-version",
                true,
                &serde_json::to_vec(receipt).unwrap(),
                false,
            ));
        }
        let (client, server) = replayable_read_fixture(move |request| {
            let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
            let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
            if query.contains_key("versions") {
                return Arc::new(Reply::xml(&listing(&rows, &query["prefix"])));
            }
            let path = url.path().strip_prefix("/fixture-bucket/root/").unwrap();
            if let Some(receipt) = registries.get(path) {
                assert_eq!(query["versionId"], "registry-version");
                return Arc::new(Reply {
                    status: 200,
                    headers: vec![("x-amz-version-id", "registry-version".into())],
                    body: serde_json::to_vec(receipt).unwrap(),
                });
            }
            let (body, version) = if path == "task/current-name.bin" {
                (&b"abc"[..], "v1")
            } else {
                assert_eq!(path, "task/old-name.bin");
                assert_eq!(request.method, "HEAD");
                (
                    &b"old"[..],
                    if matches!(fault, HistoryFault::HeadMismatch) {
                        "wrong-version"
                    } else {
                        "history-payload"
                    },
                )
            };
            Arc::new(Reply {
                status: 200,
                headers: vec![
                    ("x-amz-version-id", version.into()),
                    ("etag", "\"remote-etag\"".into()),
                ],
                body: body.to_vec(),
            })
        });
        configure_repo(&client, &fixture.repo);
        let before = snapshot(&fixture.repo);
        let report = inspect_with(
            &fixture.repo,
            &["task".into()],
            false,
            &["old/task".into()],
            &client,
        )
        .unwrap();
        assert_eq!(
            snapshot(&fixture.repo),
            before,
            "retained history audit changed local bytes or caches"
        );
        assert!(!fixture.repo.root.join("task/old-name.bin").exists());
        assert!(
            server
                .finish_requests()
                .iter()
                .all(|request| matches!(request.method.as_str(), "GET" | "HEAD"))
        );
        report
    }

    #[test]
    fn canonical_copied_history_has_its_own_verified_inventory_without_local_payload() {
        let report = retained_history_audit(HistoryFault::None);
        assert_eq!(report.status, "ok", "{:?}", report.issues);
        assert_eq!(report.expected_objects, 1);
        assert_eq!(report.retained_archive_versions, 2);
        let nested = retained_history_audit(HistoryFault::Nested);
        assert_eq!(nested.status, "ok", "{:?}", nested.issues);
        assert_eq!(
            nested.retained_archive_versions, 2,
            "previous receipt intermediate versions were already relocated"
        );
    }

    #[test]
    fn retained_archive_history_still_requires_exact_payload_and_marker_versions() {
        for fault in [HistoryFault::MissingPayload, HistoryFault::MissingMarker] {
            let report = retained_history_audit(fault);
            assert_eq!(report.status, "error");
            assert!(
                report
                    .issues
                    .iter()
                    .any(|issue| issue.code == "retained-archive-version-missing")
            );
        }
        for fault in [HistoryFault::HeadMismatch, HistoryFault::InventoryMismatch] {
            let report = retained_history_audit(fault);
            assert_eq!(report.status, "error");
            assert!(has(
                &report,
                "retained-archive-version-mismatch",
                "task/old-name.bin"
            ));
        }
    }

    #[test]
    fn copied_receipts_do_not_exempt_unmapped_versions_or_old_source_history() {
        let extra = retained_history_audit(HistoryFault::Extra);
        assert!(has(&extra, "unexpected-object", "task/old-name.bin"));
        assert!(has(&extra, "unexpected-object", "task/unmapped-name.bin"));
        let source = retained_history_audit(HistoryFault::OldSource);
        assert!(has(&source, "unexpected-object", "old/task/old-name.bin"));
    }

    #[test]
    fn local_receipt_tampering_cannot_create_retained_history_exceptions() {
        let forged = retained_history_audit(HistoryFault::TamperedReceipt);
        assert_eq!(forged.retained_archive_versions, 0);
        assert!(
            forged
                .issues
                .iter()
                .any(|issue| issue.code == "archive-registry-mismatch")
        );
        assert!(has(&forged, "unexpected-object", "task/old-name.bin"));
        let kind = retained_history_audit(HistoryFault::UnboundRenameKind);
        assert_eq!(kind.retained_archive_versions, 0);
        assert!(has(
            &kind,
            "invalid-archive-receipt",
            "task/.workspace-mgr-archive.json"
        ));
        assert!(has(&kind, "unexpected-object", "task/old-name.bin"));
    }

    #[test]
    fn exact_live_layout_checks_bytes_and_preserves_same_key_history_read_only() {
        let fixture = Fixture::new();
        fixture.file("task/data", b"abc", "md5", true);
        let report = fixture.run(
            vec![
                row("task/data", "v1", true, b"abc", false),
                row("task/data", "v0", false, b"old", false),
                row("task/data", "marker", false, b"", true),
            ],
            vec![("task/data".into(), b"abc".to_vec())],
            false,
            &[],
        );
        assert_eq!(report.status, "ok", "{:?}", report.issues);
        assert_eq!(
            (
                report.expected_objects,
                report.remote_versions,
                report.materialized_boundaries
            ),
            (1, 3, 1)
        );
    }

    #[test]
    fn legacy_metadata_cannot_bind_a_different_adjacent_output() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.repo.root.join("task")).unwrap();
        fs::write(
            fixture.repo.root.join("task/data.dvc"),
            "outs:\n- path: other\n  hash: md5\n  md5: 900150983cd24fb0d6963f7d28e17f72\n  size: 3\n  cloud:\n    workspace-mgr:\n      version_id: v1\n      etag: remote-etag\n",
        ).unwrap();
        let report = fixture.run(vec![], vec![], false, &[]);
        assert!(has(&report, "invalid-metadata", "task/data.dvc"));
        assert_eq!(report.expected_objects, 0);
        assert_eq!(report.status, "error");
    }

    #[test]
    fn unmaterialized_clone_still_checks_exact_remote_content() {
        let fixture = Fixture::new();
        fixture.file("task/data", b"abc", "md5", false);
        let report = fixture.run(
            vec![row("task/data", "v1", true, b"abc", false)],
            vec![("task/data".into(), b"abc".to_vec())],
            false,
            &[],
        );
        assert_eq!(report.status, "ok");
        assert_eq!(
            (
                report.materialized_boundaries,
                report.unmaterialized_boundaries
            ),
            (0, 1)
        );
        let corrupt = fixture.run(
            vec![row("task/data", "v1", true, b"xyz", false)],
            vec![("task/data".into(), b"xyz".to_vec())],
            false,
            &[],
        );
        assert!(has(&corrupt, "remote-content-mismatch", "task/data"));
    }

    #[test]
    fn selected_scope_finds_retired_path_and_marker_but_ignores_neighbors() {
        let fixture = Fixture::new();
        fixture.file("task/data", b"abc", "md5", true);
        let rows = vec![
            row("task/data", "v1", true, b"abc", false),
            row("old-task/data", "old", true, b"abc", false),
            row("old-task/deleted", "marker", true, b"", true),
            row("task-neighbor/data", "other", true, b"extra", false),
        ];
        let report = fixture.run(
            rows.clone(),
            vec![("task/data".into(), b"abc".to_vec())],
            false,
            &["old-task".into()],
        );
        assert!(has(&report, "unexpected-object", "old-task/data"));
        assert!(has(&report, "unexpected-object", "old-task/deleted"));
        assert!(!has(&report, "unexpected-object", "task-neighbor/data"));
        let all = fixture.run(rows, vec![("task/data".into(), b"abc".to_vec())], true, &[]);
        assert!(has(&all, "unexpected-object", "task-neighbor/data"));
    }

    #[test]
    fn exact_version_elsewhere_and_new_latest_cannot_mask_metadata_drift() {
        let fixture = Fixture::new();
        fixture.file("task/data", b"abc", "md5", true);
        let misplaced = fixture.run(
            vec![row("old-task/data", "v1", true, b"abc", false)],
            vec![],
            false,
            &["old-task".into()],
        );
        assert!(has(&misplaced, "remote-version-missing", "task/data"));
        assert!(has(&misplaced, "unexpected-object", "old-task/data"));
        let replaced = fixture.run(
            vec![
                row("task/data", "v1", false, b"abc", false),
                row("task/data", "v2", true, b"abc", false),
            ],
            vec![("task/data".into(), b"abc".to_vec())],
            false,
            &[],
        );
        assert!(has(&replaced, "remote-latest-mismatch", "task/data"));
    }

    #[test]
    fn directory_metadata_compares_local_file_set_and_remote_listing_metadata() {
        let fixture = Fixture::new();
        let entries = vec![Entry {
            path: "data".into(),
            checksum: checksum(b"abc", "md5"),
            size: 3,
            version: Some(Version {
                id: "v1".into(),
                etag: Some("remote-etag".into()),
                verification: None,
            }),
        }];
        fixture.manifest(
            "task/assets",
            &Manifest {
                schema_version: 1,
                path: "assets".into(),
                kind: Kind::Directory,
                checksum: Checksum {
                    algorithm: "md5".into(),
                    digest: directory_digest(&entries).unwrap(),
                },
                size: 3,
                version: None,
                entries: Some(entries),
            },
        );
        fs::create_dir_all(fixture.repo.root.join("task/assets")).unwrap();
        fs::write(fixture.repo.root.join("task/assets/data"), b"xyz").unwrap();
        fs::write(fixture.repo.root.join("task/assets/extra"), b"extra").unwrap();
        let mut remote = row("task/assets/data", "v1", true, b"abc", false);
        remote["Size"] = 4.into();
        remote["ETag"] = "wrong-inventory-etag".into();
        let report = fixture.run(
            vec![remote],
            vec![("task/assets/data".into(), b"abc".to_vec())],
            false,
            &[],
        );
        assert!(has(&report, "local-layout-mismatch", "task/assets/extra"));
        assert!(has(&report, "local-content-mismatch", "task/assets/data"));
        assert!(has(
            &report,
            "local-remote-bytes-mismatch",
            "task/assets/data"
        ));
        assert!(has(&report, "remote-size-mismatch", "task/assets/data"));
        assert!(has(&report, "remote-etag-mismatch", "task/assets/data"));
        assert!(has(
            &report,
            "remote-inventory-mismatch",
            "task/assets/data"
        ));
    }

    #[test]
    fn normalized_checksums_never_hide_different_raw_bytes() {
        let fixture = Fixture::new();
        fixture.file("task/data", b"a\r\nb\n", "md5-dos2unix", true);
        fs::write(fixture.repo.root.join("task/data"), b"a\nb\r\n").unwrap();
        let report = fixture.run(
            vec![row("task/data", "v1", true, b"a\r\nb\n", false)],
            vec![("task/data".into(), b"a\r\nb\n".to_vec())],
            false,
            &[],
        );
        assert!(!has(&report, "local-content-mismatch", "task/data"));
        assert!(!has(&report, "remote-content-mismatch", "task/data"));
        assert!(has(&report, "local-remote-bytes-mismatch", "task/data"));
    }

    #[test]
    fn ignored_sidecars_are_audited_and_unknown_control_keys_are_not_exempt() {
        let fixture = Fixture::new();
        fixture.file("task/data", b"abc", "md5", true);
        fs::write(fixture.repo.root.join(".gitignore"), "task/\n").unwrap();
        let report = fixture.run(
            vec![
                row("task/data", "v1", true, b"abc", false),
                row(".workspace-mgr/arbitrary", "hidden", true, b"extra", false),
            ],
            vec![("task/data".into(), b"abc".to_vec())],
            true,
            &[],
        );
        assert_eq!(report.expected_objects, 1);
        assert!(has(
            &report,
            "unexpected-control-object",
            ".workspace-mgr/arbitrary"
        ));
    }

    #[test]
    fn opaque_boundary_root_and_git_visible_payload_sidecars_are_not_controls() {
        let fixture = Fixture::new();
        let entries = vec![Entry {
            path: "payload.dvc".into(),
            checksum: checksum(b"opaque bytes", "md5"),
            size: 12,
            version: Some(Version {
                id: "v1".into(),
                etag: Some("remote-etag".into()),
                verification: None,
            }),
        }];
        fixture.manifest(
            "task/assets",
            &Manifest {
                schema_version: 1,
                path: "assets".into(),
                kind: Kind::Directory,
                checksum: Checksum {
                    algorithm: "md5".into(),
                    digest: directory_digest(&entries).unwrap(),
                },
                size: 12,
                version: None,
                entries: Some(entries),
            },
        );
        fs::create_dir_all(fixture.repo.root.join("task/assets")).unwrap();
        fs::write(
            fixture.repo.root.join("task/assets/payload.dvc"),
            b"opaque bytes",
        )
        .unwrap();
        assert_eq!(
            discover_pointers(&fixture.repo, &["task/assets".into()], false).unwrap(),
            ["task/assets.wm-storage.json"]
        );
        assert_eq!(
            discover_pointers(&fixture.repo, &["task".into()], true).unwrap(),
            ["task/assets.wm-storage.json"]
        );
        // Empty cache Git markers do not hide otherwise valid controls.
        fixture.file("task/cache/data", b"abc", "md5", false);
        fs::write(fixture.repo.root.join("task/cache/.git"), b"").unwrap();
        assert!(
            discover_pointers(&fixture.repo, &["task".into()], true)
                .unwrap()
                .contains(&"task/cache/data.wm-storage.json".into())
        );
    }

    #[test]
    fn concurrent_remote_and_local_pointer_changes_prevent_a_clean_report() {
        let fixture = Fixture::new();
        fixture.file("task/data", b"abc", "md5", true);
        let listing_count = Arc::new(AtomicUsize::new(0));
        let seen = listing_count.clone();
        let pointer = fixture.repo.root.join("task/data.wm-storage.json");
        let (client, server) = replayable_read_fixture(move |request| {
            let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
            let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
            if query.contains_key("versions") {
                let count = seen.fetch_add(1, Ordering::SeqCst);
                let rows = if count == 0 {
                    vec![row("task/data", "v1", true, b"abc", false)]
                } else {
                    vec![
                        row("task/data", "v1", false, b"abc", false),
                        row("task/data", "new", true, b"abc", false),
                    ]
                };
                return Arc::new(Reply::xml(&listing(&rows, &query["prefix"])));
            }
            fs::write(&pointer, b"changed during GET").unwrap();
            Arc::new(Reply {
                status: 200,
                headers: vec![
                    ("x-amz-version-id", "v1".into()),
                    ("etag", "\"remote-etag\"".into()),
                ],
                body: b"abc".to_vec(),
            })
        });
        configure_repo(&client, &fixture.repo);
        let report = inspect_with(&fixture.repo, &["task".into()], false, &[], &client).unwrap();
        assert!(has(&report, "remote-inventory-changed", ""));
        assert!(has(
            &report,
            "local-metadata-changed",
            "task/data.wm-storage.json"
        ));
        server.finish_requests();
    }
    fn proof_entry(body: &[u8], algorithm: &str) -> StorageEntry {
        StorageEntry {
            pointer: "task/data.wm-storage.json".into(),
            object: "task/data".into(),
            md5: Some(checksum(body, algorithm).digest),
            size: Some(body.len() as u64),
            version_id: Some("v1".into()),
            etag: Some("remote-etag".into()),
            hash_name: algorithm.into(),
            verification: None,
        }
    }

    #[test]
    fn s3_checksum_proof_requires_full_object_and_a_matching_algorithm() {
        let body = b"a\r\nb\n";
        let raw_md5 = STANDARD.encode(md5::Md5::digest(body));
        let hashes =
            native_engine::stream_hashes(&mut Cursor::new(body), Path::new("fixture")).unwrap();
        let raw = proof_entry(body, "md5");
        let normalized = proof_entry(body, "md5-dos2unix");
        let full_md5 = json!({"ChecksumType":"FULL_OBJECT","ChecksumMD5":raw_md5});
        assert!(checksum_proof(
            &raw,
            &full_md5,
            None,
            &mut AuditReport::new()
        ));
        for entry in [&raw, &normalized] {
            assert!(!checksum_proof(
                entry,
                &full_md5,
                Some(&hashes),
                &mut AuditReport::new()
            ));
        }
        assert!(!checksum_proof(
            &normalized,
            &full_md5,
            None,
            &mut AuditReport::new()
        ));
        let full_sha = json!({"ChecksumType":"FULL_OBJECT","ChecksumSHA256": STANDARD.encode(Sha256::digest(body))});
        for entry in [&raw, &normalized] {
            assert_eq!(
                checksum_proof(entry, &full_sha, Some(&hashes), &mut AuditReport::new()),
                cfg!(unix)
            );
        }
        assert!(!checksum_proof(
            &raw,
            &full_sha,
            None,
            &mut AuditReport::new()
        ));
        for response in [
            json!({"ChecksumType":"COMPOSITE","ChecksumMD5":raw_md5}),
            json!({"ChecksumMD5":raw_md5}),
            json!({"ChecksumType":"FULL_OBJECT","ChecksumCRC32":"1234"}),
            json!({"ChecksumType":"FULL_OBJECT","ChecksumMD5":"malformed"}),
            json!({"ChecksumType":"FULL_OBJECT","ChecksumMD5": STANDARD.encode([0u8;15])}),
        ] {
            assert!(
                !checksum_proof(&raw, &response, Some(&hashes), &mut AuditReport::new()),
                "{response}"
            );
        }
    }

    #[test]
    fn remote_checksum_proof_reports_corruption_and_does_not_hide_normalized_raw_changes() {
        let entry = proof_entry(b"abc", "md5");
        let mut report = AuditReport::new();
        assert!(checksum_proof(
            &entry,
            &json!({"ChecksumType":"FULL_OBJECT","ChecksumMD5": STANDARD.encode(md5::Md5::digest(b"xyz"))}),
            None,
            &mut report
        ));
        assert!(has(&report, "remote-content-mismatch", "task/data"));
        let normalized = proof_entry(b"a\r\nb\n", "md5-dos2unix");
        let changed =
            native_engine::stream_hashes(&mut Cursor::new(b"a\nb\r\n"), Path::new("fixture"))
                .unwrap();
        assert_eq!(
            changed.normalized_md5,
            normalized.md5.as_ref().unwrap().as_str()
        );
        assert!(!checksum_proof(
            &normalized,
            &json!({"ChecksumType":"FULL_OBJECT","ChecksumSHA256": STANDARD.encode(Sha256::digest(b"a\r\nb\n"))}),
            Some(&changed),
            &mut AuditReport::new()
        ));
    }

    #[test]
    fn full_object_md5_collisions_retain_materialized_literal_byte_comparison() {
        // Public 128-byte Wang/Yu collision pair, reproduced by Peter Selinger:
        // https://www.mscs.dal.ca/~selinger/md5collision/
        // Verify the collision here instead of trusting the fixture's label.
        let decode = |hex: &str| {
            let (pairs, remainder) = hex.as_bytes().as_chunks::<2>();
            assert!(remainder.is_empty());
            pairs
                .iter()
                .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
                .collect::<Vec<_>>()
        };
        let local = decode(concat!(
            "d131dd02c5e6eec4693d9a0698aff95c2fcab58712467eab4004583eb8fb7f89",
            "55ad340609f4b30283e488832571415a085125e8f7cdc99fd91dbdf280373c5b",
            "d8823e3156348f5bae6dacd436c919c6dd53e2b487da03fd02396306d248cda0",
            "e99f33420f577ee8ce54b67080a80d1ec69821bcb6a8839396f9652b6ff72a70"
        ));
        let remote = decode(concat!(
            "d131dd02c5e6eec4693d9a0698aff95c2fcab50712467eab4004583eb8fb7f89",
            "55ad340609f4b30283e4888325f1415a085125e8f7cdc99fd91dbd7280373c5b",
            "d8823e3156348f5bae6dacd436c919c6dd53e23487da03fd02396306d248cda0",
            "e99f33420f577ee8ce54b67080280d1ec69821bcb6a8839396f965ab6ff72a70"
        ));
        assert_eq!(local.len(), 128);
        assert_eq!(remote.len(), local.len());
        assert_ne!(local, remote);
        assert_eq!(md5::Md5::digest(&local), md5::Md5::digest(&remote));
        assert_eq!(
            crate::hex::encode_lower(md5::Md5::digest(&local)),
            "79054025255fb1a26e4bc422aef54eb4"
        );
        assert_ne!(Sha256::digest(&local), Sha256::digest(&remote));

        for algorithm in ["md5", "md5-dos2unix"] {
            assert_eq!(checksum(&local, algorithm), checksum(&remote, algorithm));
            for include_sha256 in [false, true] {
                let fixture = Fixture::new();
                fixture.file("task/data", &local, algorithm, true);
                let remote = remote.clone();
                let (client, server) = replayable_read_fixture(move |request| {
                    let url =
                        url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
                    let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
                    if query.contains_key("versions") {
                        return Arc::new(Reply::xml(&listing(
                            &[row("task/data", "v1", true, &remote, false)],
                            &query["prefix"],
                        )));
                    }
                    assert_eq!(query.get("versionId").map(String::as_str), Some("v1"));
                    let mut headers = vec![
                        ("x-amz-version-id", "v1".into()),
                        ("etag", "\"remote-etag\"".into()),
                    ];
                    if request.method == "HEAD" {
                        headers.push(("x-amz-checksum-type", "FULL_OBJECT".into()));
                        headers.push((
                            "x-amz-checksum-md5",
                            STANDARD.encode(md5::Md5::digest(&remote)),
                        ));
                        if include_sha256 {
                            headers.push((
                                "x-amz-checksum-sha256",
                                STANDARD.encode(Sha256::digest(&remote)),
                            ));
                        }
                    }
                    Arc::new(Reply {
                        status: 200,
                        headers,
                        body: remote.clone(),
                    })
                });
                configure_repo(&client, &fixture.repo);
                let before = snapshot(&fixture.repo);
                let report =
                    inspect_with(&fixture.repo, &["task".into()], false, &[], &client).unwrap();
                assert!(has(&report, "local-remote-bytes-mismatch", "task/data"));
                assert!(!has(&report, "local-content-mismatch", "task/data"));
                assert!(!has(&report, "remote-content-mismatch", "task/data"));
                assert_eq!(report.remote_checksum_objects, 0);
                assert_eq!(report.streamed_objects, 1);
                assert_eq!(report.streamed_bytes, 128);
                assert_eq!(snapshot(&fixture.repo), before);
                let requests = server.finish_requests();
                assert_eq!(requests.len(), 4, "two inventories, HEAD and GET");
                assert!(
                    requests
                        .iter()
                        .all(|request| matches!(request.method.as_str(), "GET" | "HEAD"))
                );
            }
        }
    }

    #[test]
    fn missing_local_hashes_never_turn_a_present_payload_into_unmaterialized_proof() {
        let fixture = Fixture::new();
        fixture.file("task/data", b"abc", "md5", true);
        let (client, server) = replayable_read_fixture(move |request| {
            assert!(matches!(request.method.as_str(), "GET" | "HEAD"));
            Arc::new(Reply {
                status: 200,
                headers: vec![
                    ("x-amz-version-id", "v1".into()),
                    ("etag", "\"remote-etag\"".into()),
                    ("x-amz-checksum-type", "FULL_OBJECT".into()),
                    (
                        "x-amz-checksum-md5",
                        STANDARD.encode(md5::Md5::digest(b"abc")),
                    ),
                ],
                body: b"abc".to_vec(),
            })
        });
        let entry = proof_entry(b"abc", "md5");
        let recorded = row("task/data", "v1", true, b"abc", false);
        let mut report = AuditReport::new();
        inspect_remote_entry(
            &fixture.repo,
            &client,
            &entry,
            &[&recorded],
            None,
            &mut report,
        );
        assert_eq!(report.remote_checksum_objects, 0);
        assert_eq!(report.streamed_objects, 1);
        assert!(report.issues.is_empty(), "{:?}", report.issues);
        assert_eq!(server.finish_requests().len(), 2, "HEAD followed by GET");
    }

    #[test]
    fn full_object_s3_md5_proof_checks_an_unmaterialized_object_without_get() {
        let fixture = Fixture::new();
        fixture.file("task/data", b"abc", "md5", false);
        let (client, server) = replayable_read_fixture(move |request| {
            let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
            let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
            if query.contains_key("versions") {
                return Arc::new(Reply::xml(&listing(
                    &[row("task/data", "v1", true, b"abc", false)],
                    &query["prefix"],
                )));
            }
            assert_eq!(
                request.method, "HEAD",
                "checksum proof should not download payloads"
            );
            assert_eq!(query.get("versionId").map(String::as_str), Some("v1"));
            Arc::new(Reply {
                status: 200,
                headers: vec![
                    ("x-amz-version-id", "v1".into()),
                    ("etag", "\"remote-etag\"".into()),
                    ("x-amz-checksum-type", "FULL_OBJECT".into()),
                    (
                        "x-amz-checksum-md5",
                        STANDARD.encode(md5::Md5::digest(b"abc")),
                    ),
                ],
                body: b"abc".to_vec(),
            })
        });
        configure_repo(&client, &fixture.repo);
        let report = inspect_with(&fixture.repo, &["task".into()], false, &[], &client).unwrap();
        assert_eq!(report.status, "ok", "{:?}", report.issues);
        assert_eq!(report.remote_checksum_objects, 1);
        assert_eq!(report.streamed_objects, 0);
        assert_eq!(report.streamed_bytes, 0);
        assert_eq!(
            server.finish_requests().len(),
            3,
            "two inventories plus one HEAD"
        );
    }

    fn verified_server(
        body: &'static [u8],
        checksum_headers: Vec<(&'static str, String)>,
        head_status: u16,
    ) -> (S3Client, RoutedFixture) {
        replayable_read_fixture(move |request| {
            let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
            let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
            if query.contains_key("versions") {
                return Arc::new(Reply::xml(&listing(
                    &[row("task/data", "v1", true, body, false)],
                    &query["prefix"],
                )));
            }
            assert_eq!(
                request.method, "HEAD",
                "schema 2 diagnosis must never GET payload bytes"
            );
            assert_eq!(query.get("versionId").map(String::as_str), Some("v1"));
            let mut headers = vec![
                ("x-amz-version-id", "v1".into()),
                ("etag", "\"remote-etag\"".into()),
            ];
            headers.extend(checksum_headers.clone());
            Arc::new(Reply {
                status: head_status,
                headers,
                body: body.to_vec(),
            })
        })
    }

    #[test]
    fn verified_versions_need_no_payload_read_without_strong_provider_checksums() {
        for materialized in [false, true] {
            for crc in [false, true] {
                let fixture = Fixture::new();
                let headers = if crc {
                    vec![
                        ("x-amz-checksum-type", "FULL_OBJECT".into()),
                        ("x-amz-checksum-crc32", "AAAAAA==".into()),
                    ]
                } else {
                    Vec::new()
                };
                let (client, server) = verified_server(b"a\r\nb\n", headers, 200);
                configure_repo(&client, &fixture.repo);
                fixture.verified_file("task/data", b"a\r\nb\n", materialized, &client);
                let before = snapshot(&fixture.repo);
                let report =
                    inspect_with(&fixture.repo, &["task".into()], false, &[], &client).unwrap();
                assert_eq!(report.status, "ok", "{:?}", report.issues);
                assert_eq!(report.verified_version_objects, 1);
                assert_eq!(report.remote_checksum_objects, 0);
                assert_eq!(report.streamed_objects, 0);
                assert_eq!(report.streamed_bytes, 0);
                assert_eq!(snapshot(&fixture.repo), before);
                assert_eq!(
                    server.finish_requests().len(),
                    3,
                    "two inventories and HEAD"
                );
            }
        }
    }

    #[test]
    fn verified_versions_reject_conflicting_or_malformed_full_sha_without_get() {
        for (sha, expected_code) in [
            (
                STANDARD.encode(Sha256::digest(b"different bytes")),
                "remote-content-mismatch",
            ),
            ("malformed".into(), "remote-checksum-invalid"),
            (STANDARD.encode([0u8; 31]), "remote-checksum-invalid"),
        ] {
            let fixture = Fixture::new();
            let (client, server) = verified_server(
                b"abc",
                vec![
                    ("x-amz-checksum-type", "FULL_OBJECT".into()),
                    ("x-amz-checksum-sha256", sha),
                ],
                200,
            );
            configure_repo(&client, &fixture.repo);
            fixture.verified_file("task/data", b"abc", true, &client);
            let report =
                inspect_with(&fixture.repo, &["task".into()], false, &[], &client).unwrap();
            assert!(has(&report, expected_code, "task/data"));
            assert_eq!(report.verified_version_objects, 0);
            assert_eq!(report.streamed_objects, 0);
            assert_eq!(report.streamed_bytes, 0);
            assert_eq!(server.finish_requests().len(), 3);
        }
    }

    #[test]
    fn verified_raw_sha_rejects_local_changes_hidden_by_normalized_md5_without_get() {
        let fixture = Fixture::new();
        let (client, server) = verified_server(b"a\r\nb\n", Vec::new(), 200);
        configure_repo(&client, &fixture.repo);
        fixture.verified_file("task/data", b"a\r\nb\n", true, &client);
        fs::write(fixture.repo.root.join("task/data"), b"a\nb\r\n").unwrap();
        let report = inspect_with(&fixture.repo, &["task".into()], false, &[], &client).unwrap();
        assert!(has(&report, "local-content-mismatch", "task/data"));
        assert!(has(&report, "local-remote-bytes-mismatch", "task/data"));
        assert_eq!(report.verified_version_objects, 0);
        assert_eq!(report.streamed_objects, 0);
        assert_eq!(server.finish_requests().len(), 3);
    }

    #[test]
    fn verified_storage_scope_cannot_be_reused_at_another_endpoint_bucket_or_key() {
        for changed_field in ["endpoint", "bucket", "key"] {
            let fixture = Fixture::new();
            let (client, server) = verified_server(b"abc", Vec::new(), 200);
            configure_repo(&client, &fixture.repo);
            let mut manifest = fixture.verified_file("task/data", b"abc", true, &client);
            let proof = manifest
                .version
                .as_mut()
                .unwrap()
                .verification
                .as_mut()
                .unwrap();
            match changed_field {
                "endpoint" => proof.endpoint = "https://different.example".into(),
                "bucket" => proof.bucket = "different-bucket".into(),
                "key" => proof.key = "another-root/task/data".into(),
                _ => unreachable!(),
            }
            fixture.manifest("task/data", &manifest);
            let report =
                inspect_with(&fixture.repo, &["task".into()], false, &[], &client).unwrap();
            assert!(has(
                &report,
                "remote-verification-binding-mismatch",
                "task/data"
            ));
            assert_eq!(report.verified_version_objects, 0);
            assert_eq!(report.streamed_objects, 0);
            assert_eq!(
                server.finish_requests().len(),
                2,
                "only the inventory snapshots"
            );
        }
    }

    #[test]
    fn verified_metadata_failure_never_falls_back_to_payload_read() {
        let fixture = Fixture::new();
        let (client, server) = verified_server(b"abc", Vec::new(), 404);
        configure_repo(&client, &fixture.repo);
        fixture.verified_file("task/data", b"abc", false, &client);
        let report = inspect_with(&fixture.repo, &["task".into()], false, &[], &client).unwrap();
        assert!(has(&report, "remote-read-failed", "task/data"));
        assert_eq!(report.verified_version_objects, 0);
        assert_eq!(report.streamed_objects, 0);
        assert_eq!(server.finish_requests().len(), 3);
    }

    #[test]
    fn verified_content_proof_cannot_follow_a_changed_version_id_without_get() {
        let fixture = Fixture::new();
        let (client, server) = verified_server(b"abc", Vec::new(), 200);
        configure_repo(&client, &fixture.repo);
        let mut manifest = fixture.verified_file("task/data", b"abc", false, &client);
        manifest.version.as_mut().unwrap().id = "v2".into();
        let pointer = storage_metadata::pointer_path("task/data");
        // Bypass serialization's validation to model an incorrectly edited
        // control file: proof for v1 must not attest the replacement v2.
        fs::write(
            fixture.repo.root.join(&pointer),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();
        let report = inspect_with(&fixture.repo, &["task".into()], false, &[], &client).unwrap();
        assert!(has(&report, "invalid-metadata", &pointer));
        assert_eq!(report.expected_objects, 0);
        assert_eq!(report.verified_version_objects, 0);
        assert_eq!(report.streamed_objects, 0);
        assert_eq!(server.finish_requests().len(), 2);

        // Also guard callers already holding expanded storage entries, even
        // if they do not go through the strict manifest parser again.
        let (client, server) = verified_server(b"abc", Vec::new(), 200);
        let manifest = fixture.verified_file("task/data", b"abc", false, &client);
        let mut entry = proof_entry(b"abc", "md5");
        entry.verification = manifest.version.unwrap().verification;
        entry.version_id = Some("v2".into());
        let recorded = row("task/data", "v2", true, b"abc", false);
        let mut report = AuditReport::new();
        inspect_remote_entry(
            &fixture.repo,
            &client,
            &entry,
            &[&recorded],
            None,
            &mut report,
        );
        assert!(has(
            &report,
            "remote-verification-binding-mismatch",
            "task/data"
        ));
        assert_eq!(report.streamed_objects, 0);
        assert_eq!(report.verified_version_objects, 0);
        assert!(server.finish_requests().is_empty());
    }

    #[test]
    fn verified_versions_retry_only_explicitly_unsupported_checksum_mode_without_get() {
        for (code, message, status, retry) in [
            (
                "NotImplemented",
                "Unsupported x-amz-checksum-mode header",
                501,
                true,
            ),
            ("InvalidRequest", "ChecksumMode is unsupported", 400, true),
            ("InvalidRequest", "Checksum is invalid", 400, false),
            (
                "NotImplemented",
                "Other functionality is unsupported",
                501,
                false,
            ),
            ("AccessDenied", "Checksum access denied", 403, false),
        ] {
            let fixture = Fixture::new();
            let (client, server) = replayable_read_fixture(move |request| {
                let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
                let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
                if query.contains_key("versions") {
                    return Arc::new(Reply::xml(&listing(
                        &[row("task/data", "v1", true, b"abc", false)],
                        &query["prefix"],
                    )));
                }
                assert_eq!(request.method, "HEAD", "schema 2 must never GET payloads");
                if request.headers.contains_key("x-amz-checksum-mode") {
                    Arc::new(Reply {
                        status,
                        headers: vec![
                            ("x-amz-error-code", code.into()),
                            ("x-amz-error-message", message.into()),
                        ],
                        body: Vec::new(),
                    })
                } else {
                    assert!(retry, "must not discard a different metadata error");
                    Arc::new(Reply {
                        status: 200,
                        headers: vec![
                            ("x-amz-version-id", "v1".into()),
                            ("etag", "\"remote-etag\"".into()),
                        ],
                        body: b"abc".to_vec(),
                    })
                }
            });
            configure_repo(&client, &fixture.repo);
            fixture.verified_file("task/data", b"abc", false, &client);
            let report =
                inspect_with(&fixture.repo, &["task".into()], false, &[], &client).unwrap();
            assert_eq!(report.status == "ok", retry, "{:?}", report.issues);
            assert_eq!(report.verified_version_objects, usize::from(retry));
            assert_eq!(report.streamed_objects, 0);
            assert_eq!(server.finish_requests().len(), if retry { 4 } else { 3 });
        }
    }

    #[cfg(unix)]
    #[test]
    fn local_generation_snapshot_detects_same_size_same_mtime_replacement() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.repo.root.join("task")).unwrap();
        let path = fixture.repo.root.join("task/data");
        fs::write(&path, b"abc").unwrap();
        let before = local_state(&fixture.repo, "task/data").unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"xyz").unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert_ne!(before, local_state(&fixture.repo, "task/data").unwrap());
    }
}
