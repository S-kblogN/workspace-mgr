//! Read-only comparison of the checkout's logical storage tree with S3.
//!
//! Unlike hydration, this audit never follows archive aliases, installs cache
//! objects, repairs pointers, or deletes remotely retained objects. An exact
//! version at another logical path is a layout error even when still readable.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufReader, Read};
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
        let binding = std::str::from_utf8(&raw)
            .map_err(|error| Error::message(error.to_string()))
            .and_then(|raw| storage_metadata::normalize_pointer_in_repo(repo, None, raw, pointer))
            .and_then(|raw| storage_metadata::metadata_output(repo, pointer, &raw));
        if let Err(error) = binding {
            report.issue("invalid-metadata", pointer, error.to_string());
            continue;
        }
        let entries =
            match native_engine::metadata_entries(repo, None, std::slice::from_ref(pointer)) {
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

    // Registry objects are exceptions only when a local receipt identifies
    // their exact canonical key. An arbitrary control-looking key is extra.
    let registries = expected_registries(repo, scopes, retired_scopes, all, &mut report)?;
    let root_prefix = client.key_for("");
    let inventory = load_inventory(client, scopes, retired_scopes, all, &registries)?;
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
        if !expected.contains_key(path) {
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

    // Scratch lives outside the repository and is removed at command exit.
    // Every exact object is hashed even on a fresh unmaterialized checkout.
    let scratch = tempfile::tempdir()
        .map_err(|error| Error::message(format!("create doctor download scratch: {error}")))?;
    let scratch_object = scratch.path().join("payload");
    for entry in expected.values() {
        let rows = objects.get(&entry.object).map(Vec::as_slice).unwrap_or(&[]);
        inspect_remote_entry(repo, client, entry, rows, &scratch_object, &mut report);
        match fs::remove_file(&scratch_object) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => report.issue("scratch-cleanup-failed", &entry.object, error.to_string()),
        }
    }
    for (path, receipt) in &registries {
        let source = receipt["source"]
            .as_str()
            .expect("validated receipt source");
        match crate::native_archive::registry_read(client, repo, source) {
            Ok(Some(remote)) if remote == *receipt => {}
            Ok(_) => report.issue(
                "archive-registry-mismatch",
                path,
                "remote archive registry differs from the local copied receipt or is missing",
            ),
            Err(error) => report.issue("archive-registry-mismatch", path, error.to_string()),
        }
    }

    // Version listing is not an atomic snapshot. Never report success when
    // the compared logical inventory visibly changed during this audit.
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
        format!(
            "{}:{}:{}:{:?}",
            metadata.is_file(),
            metadata.is_dir(),
            metadata.len(),
            metadata.modified().ok()
        )
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
    for prefix in roots {
        for row in client.list_versions(&client.key_for(&prefix))? {
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
    for entry in entries {
        let local = repo.root.join(&entry.object);
        if !local.is_file()
            || reject_symlink_traversal(&repo.root, &entry.object, "doctor local payload").is_err()
        {
            continue;
        }
        match fs::metadata(&local).at(&local).and_then(|metadata| {
            Ok(entry.size == Some(metadata.len())
                && entry.md5.as_deref()
                    == Some(native_engine::file_digest(&local, &entry.hash_name)?.as_str()))
        }) {
            Ok(true) => {}
            Ok(false) => report.entry_issue(
                "local-content-mismatch",
                entry,
                "local payload checksum or physical size differs from its metadata",
            ),
            Err(error) => report.entry_issue("local-content-mismatch", entry, error.to_string()),
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
    scratch: &Path,
    report: &mut AuditReport,
) {
    let Some(version) = entry
        .version_id
        .as_deref()
        .filter(|version| !version.is_empty() && *version != "null")
    else {
        return;
    };
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
    let response = match client.get_to_file(&args, scratch) {
        Ok(response) => response.value,
        Err(error) => {
            report.entry_issue(
                if matches!(error.status, Some(412)) {
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
    if response["VersionId"] != version || response["DeleteMarker"] == true {
        report.entry_issue(
            "remote-version-mismatch",
            entry,
            "S3 GET did not return the requested exact payload version",
        );
    }
    if response["ContentLength"].as_u64() != entry.size {
        report.entry_issue(
            "remote-size-mismatch",
            entry,
            "remote payload physical size differs from local metadata",
        );
    }
    if fs::metadata(scratch)
        .map(|metadata| Some(metadata.len()) != response["ContentLength"].as_u64())
        .unwrap_or(true)
        || response["ContentLength"].as_u64() != recorded["Size"].as_u64()
        || response["ETag"].as_str().map(tag) != recorded["ETag"].as_str().map(tag)
    {
        report.entry_issue(
            "remote-inventory-mismatch",
            entry,
            "exact S3 GET metadata or downloaded byte count differs from its version inventory",
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
    match native_engine::file_digest(scratch, &entry.hash_name) {
        Ok(digest) if entry.md5.as_deref() == Some(digest.as_str()) => {}
        Ok(_) => report.entry_issue(
            "remote-content-mismatch",
            entry,
            "exact remote payload checksum differs from local metadata",
        ),
        Err(error) => report.entry_issue("remote-content-mismatch", entry, error.to_string()),
    }
    let local = repo.root.join(&entry.object);
    if fs::symlink_metadata(&local)
        .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
        && reject_symlink_traversal(&repo.root, &entry.object, "doctor local payload").is_ok()
    {
        match equal_bytes(&local, scratch) {
            Ok(true) => {}
            Ok(false) => report.entry_issue(
                "local-remote-bytes-mismatch",
                entry,
                "materialized local payload differs byte for byte from its exact remote version",
            ),
            Err(error) => {
                report.entry_issue("local-remote-bytes-mismatch", entry, error.to_string())
            }
        }
    }
}

fn equal_bytes(left: &Path, right: &Path) -> Result<bool> {
    let mut remaining = fs::metadata(left).at(left)?.len();
    if remaining != fs::metadata(right).at(right)?.len() {
        return Ok(false);
    }
    let mut left = BufReader::new(fs::File::open(left).at(left)?);
    let mut right = BufReader::new(fs::File::open(right).at(right)?);
    let mut a = [0u8; 64 * 1024];
    let mut b = [0u8; 64 * 1024];
    while remaining > 0 {
        let length = remaining.min(a.len() as u64) as usize;
        left.read_exact(&mut a[..length])
            .map_err(|error| Error::message(error.to_string()))?;
        right
            .read_exact(&mut b[..length])
            .map_err(|error| Error::message(error.to_string()))?;
        if a[..length] != b[..length] {
            return Ok(false);
        }
        remaining -= length as u64;
    }
    Ok(true)
}

fn expected_registries(
    repo: &GitRepo,
    scopes: &[String],
    retired: &[String],
    all: bool,
    report: &mut AuditReport,
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
                serde_json::from_str::<Value>(&raw)
                    .map_err(|error| Error::message(error.to_string()))
            })
            .and_then(|receipt| {
                archive_migration::validate(&path, &receipt)?;
                Ok(receipt)
            });
        match receipt {
            Ok(receipt) => collect_registries(&receipt, &mut result, report, 0),
            Err(error) => report.issue("invalid-archive-receipt", &path, error.to_string()),
        }
    }
    Ok(result)
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
        if let Some(source) = receipt["source"].as_str() {
            let path = format!(
                ".workspace-mgr/archive/{}.json",
                crate::hex::encode_lower(Sha256::digest(source.as_bytes()))
            );
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
    use crate::storage_format::{Checksum, Entry, Kind, Manifest, Version, directory_digest};
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
}
