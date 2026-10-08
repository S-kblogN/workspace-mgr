//! Exact-version reads and permanent retirement of managed logical S3 keys.
//! Network workers never mutate pointer documents or share verification caches.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use serde_json::{Value, json};

use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::native_archive;
use crate::native_engine::{self, StorageEntry};
use crate::native_s3::S3Client;
use crate::storage_metadata;

const ARCHIVE_SUFFIX: &str = "/.workspace-mgr-archive.json";
const MAX_ARCHIVE_HOPS: usize = 32;
const WORKERS: usize = 16;
const PURGE_WORKERS: usize = 4;

fn string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value[field]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::message(format!("missing or invalid {field}")))
}
fn etag(value: &str) -> &str {
    value.trim_matches('"')
}
fn key(client: &S3Client, object: &str) -> String {
    if client.prefix.is_empty() {
        object.to_owned()
    } else {
        format!("{}/{object}", client.prefix.trim_end_matches('/'))
    }
}
fn identity(value: &Value) -> Result<(String, String)> {
    Ok((
        string(value, "Key")?.to_owned(),
        string(value, "VersionId")?.to_owned(),
    ))
}
pub(crate) fn bounded_map<T: Sync, R: Send>(
    items: &[T],
    function: impl Fn(&T) -> Result<R> + Sync,
) -> Result<Vec<R>> {
    bounded_map_with_workers(items, WORKERS, false, function)
}
/// Executes independent work concurrently, returning values in input order.
/// All started workers are joined before reporting any failure.
pub(crate) fn bounded_map_with_workers<T: Sync, R: Send>(
    items: &[T],
    limit: usize,
    stop_after_error: bool,
    function: impl Fn(&T) -> Result<R> + Sync,
) -> Result<Vec<R>> {
    if limit <= 1 || items.len() <= 1 {
        return items.iter().map(function).collect();
    }
    let next = AtomicUsize::new(0);
    let stopped = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for _ in 0..limit.min(items.len()) {
            let function = &function;
            let next = &next;
            let stopped = &stopped;
            workers.push(scope.spawn(move || {
                let mut output = Vec::new();
                while !stopped.load(Ordering::Acquire) {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(index) else {
                        break;
                    };
                    match function(item) {
                        Ok(value) => output.push((index, value)),
                        Err(error) => {
                            if stop_after_error {
                                stopped.store(true, Ordering::Release);
                            }
                            return Err(error);
                        }
                    }
                }
                Ok(output)
            }));
        }
        let mut output = Vec::new();
        let mut first_error = None;
        for worker in workers {
            match worker.join() {
                Ok(Ok(values)) => output.extend(values),
                Ok(Err(error)) => {
                    first_error.get_or_insert(error);
                }
                Err(_) => {
                    first_error
                        .get_or_insert_with(|| Error::message("version read worker panicked"));
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => {
                output.sort_unstable_by_key(|(index, _)| *index);
                Ok(output.into_iter().map(|(_, value)| value).collect())
            }
        }
    })
}

pub(crate) fn check_versioning(repo: &GitRepo) -> Result<Value> {
    let client = S3Client::from_repo(repo)?;
    check_client_versioning(&client)
}
fn check_client_versioning(client: &S3Client) -> Result<Value> {
    let info = client
        .call_s3(
            "get_bucket_versioning",
            &json!({"Bucket":client.bucket}),
            None,
        )?
        .value;
    if info["Status"] != "Enabled" {
        return Err(Error::message(format!(
            "S3 bucket {:?} does not have object versioning enabled",
            client.bucket
        )));
    }
    Ok(
        json!({"mode":"bucket-versioning","remote":"workspace-mgr","bucket":client.bucket,"status":"enabled"}),
    )
}

#[derive(Clone)]
struct Entry {
    metadata: StorageEntry,
    key: String,
    version: String,
    etag: Option<String>,
}
fn validate_info(entry: &Entry, info: &Value) -> Result<()> {
    let size = info
        .get("ContentLength")
        .or_else(|| info.get("Size"))
        .and_then(Value::as_u64);
    let remote_etag = info["ETag"].as_str().map(etag);
    let mut mismatch = Vec::new();
    if info["VersionId"] != entry.version || info["DeleteMarker"] == true {
        mismatch.push("version ID");
    }
    if entry.metadata.size.is_some() && size != entry.metadata.size {
        mismatch.push("size");
    }
    if entry
        .etag
        .as_deref()
        .is_some_and(|expected| Some(etag(expected)) != remote_etag)
    {
        mismatch.push("etag");
    }
    if mismatch.is_empty() {
        Ok(())
    } else {
        Err(Error::message(format!(
            "version-aware object {:?} has mismatched {}",
            entry.metadata.object,
            mismatch.join(", ")
        )))
    }
}
pub(crate) fn validate_digest(entry: &StorageEntry) -> Result<()> {
    let valid = entry.md5.as_deref().is_some_and(|v| {
        v.len() == 32
            && v.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    });
    if !valid || !matches!(entry.hash_name.as_str(), "md5" | "md5-dos2unix") {
        return Err(Error::message(format!(
            "managed-storage object has no supported content hash: {}",
            entry.object
        )));
    }
    Ok(())
}
fn content_matches(entry: &Entry, path: &Path) -> Result<bool> {
    content_matches_with_inventory(entry, path, &mut native_engine::HashInventory::new())
}

fn content_matches_with_inventory(
    entry: &Entry,
    path: &Path,
    hashes: &mut native_engine::HashInventory,
) -> Result<bool> {
    let input = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(Error::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    let metadata = input.metadata().at(path)?;
    if !metadata.is_file()
        || entry
            .metadata
            .size
            .is_some_and(|size| size != metadata.len())
    {
        return Ok(false);
    }
    match &entry.metadata.verification {
        Some(proof) => {
            let actual = hashes.hashes(path)?;
            Ok(
                entry.metadata.md5.as_deref() == Some(actual.digest(&entry.metadata.hash_name)?)
                    && actual.sha256 == proof.checksum.digest,
            )
        }
        None => {
            let digest = hashes.digest(path, &entry.metadata.hash_name)?;
            Ok(entry.metadata.md5.as_deref() == Some(digest.as_str()))
        }
    }
}
fn pending_aliases(client: &S3Client, entries: &mut [Entry], receipts: &[Value]) -> Result<()> {
    let mut aliases = BTreeMap::new();
    for receipt in receipts {
        if receipt["status"] == "copied" {
            continue;
        }
        if receipt["status"] != "planned"
            || receipt["schema_version"] != 1
            || receipt["bucket"] != client.bucket
            || receipt["remote_prefix"] != client.prefix
            || receipt["remote"] != "workspace-mgr"
        {
            return Err(Error::message(
                "invalid pending archive receipt state or storage location",
            ));
        }
        let source = string(receipt, "source")?;
        let destination = string(receipt, "destination")?;
        validate_task_prefix(source)?;
        validate_task_prefix(destination)?;
        if source == destination
            || source.starts_with(&format!("{destination}/"))
            || destination.starts_with(&format!("{source}/"))
            || source.rsplit('/').next() != destination.rsplit('/').next()
        {
            return Err(Error::message("invalid pending archive task prefixes"));
        }
        for row in receipt["versions"]
            .as_array()
            .ok_or_else(|| Error::message("pending archive receipt has no version snapshot"))?
        {
            let old = string(row, "source_object")?;
            let new = string(row, "destination_object")?;
            let suffix = old.strip_prefix(&format!("{source}/")).ok_or_else(|| {
                Error::message("pending archive version escapes its task prefixes")
            })?;
            if new != format!("{destination}/{suffix}") || !row["delete_marker"].is_boolean() {
                return Err(Error::message("invalid pending archive version"));
            }
            let version = string(row, "source_version_id")?;
            if row["delete_marker"] == true {
                continue;
            }
            let size = row["size"]
                .as_u64()
                .ok_or_else(|| Error::message("pending archive version has no valid size"))?;
            let tag = etag(string(row, "source_etag")?).to_owned();
            let target = (key(client, new), version.to_owned());
            let alias = (key(client, old), size, tag);
            if aliases
                .insert(target.clone(), alias.clone())
                .is_some_and(|previous| previous != alias)
            {
                return Err(Error::message(
                    "conflicting pending archive version aliases",
                ));
            }
        }
    }
    for entry in entries {
        if let Some((source, size, tag)) = aliases.get(&(entry.key.clone(), entry.version.clone()))
        {
            if entry
                .metadata
                .size
                .is_some_and(|expected| expected != *size)
                || entry
                    .etag
                    .as_deref()
                    .is_some_and(|expected| etag(expected) != tag)
            {
                return Err(Error::message(
                    "pending archive alias has mismatched size or etag",
                ));
            }
            entry.key = source.clone();
            entry.etag = Some(tag.clone());
        }
    }
    Ok(())
}
fn validate_task_prefix(prefix: &str) -> Result<()> {
    if prefix.is_empty()
        || prefix.starts_with('/')
        || prefix
            .split('/')
            .any(|p| p.is_empty() || matches!(p, "." | ".."))
    {
        return Err(Error::message("invalid archive task prefix"));
    }
    Ok(())
}
fn mapped_entry(
    client: &S3Client,
    repo: &GitRepo,
    entry: &Entry,
    seen: &mut BTreeSet<(String, String)>,
) -> Result<Entry> {
    if seen.len() >= MAX_ARCHIVE_HOPS || !seen.insert((entry.key.clone(), entry.version.clone())) {
        return Err(Error::message(
            "historical archive mapping is cyclic or exceeds its hop limit",
        ));
    }
    let row = native_archive::registry_lookup(client, repo, &entry.key, &entry.version)?
        .ok_or_else(|| Error::message(format!("missing version: {}", entry.metadata.object)))?;
    if entry
        .metadata
        .size
        .is_some_and(|s| row["size"].as_u64() != Some(s))
        || entry.etag.as_deref().is_some_and(|s| {
            row["source_etag"]
                .as_str()
                .is_some_and(|v| etag(s) != etag(v))
        })
    {
        return Err(Error::message(
            "historical archive mapping has mismatched size or source etag",
        ));
    }
    let mut next = entry.clone();
    next.key = string(&row, "destination_key")?.to_owned();
    next.version = string(&row, "destination_version_id")?.to_owned();
    next.etag = Some(etag(string(&row, "destination_etag")?).to_owned());
    Ok(next)
}
fn verify_head(client: &S3Client, repo: &GitRepo, entry: &Entry) -> Result<()> {
    let mut current = entry.clone();
    let mut seen = BTreeSet::new();
    loop {
        match client.call_s3(
            "head_object",
            &json!({"Bucket":client.bucket,"Key":current.key,"VersionId":current.version}),
            None,
        ) {
            Ok(response) => return validate_info(&current, &response.value),
            Err(error) if error.is_missing() => {
                current = mapped_entry(client, repo, &current, &mut seen)?
            }
            Err(error) => return Err(error.into()),
        }
    }
}
fn verify_entries(client: &S3Client, repo: &GitRepo, entries: &[Entry]) -> Result<()> {
    verify_entries_with_placement(client, repo, entries, true)
}

fn verify_entries_with_placement(
    client: &S3Client,
    repo: &GitRepo,
    entries: &[Entry],
    allow_archive: bool,
) -> Result<()> {
    let mut groups: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
    for entry in entries {
        let prefix = entry
            .key
            .rsplit_once('/')
            .map(|(p, _)| format!("{p}/"))
            .unwrap_or_default();
        groups.entry(prefix).or_default().push(entry.clone());
    }
    let groups = groups.into_iter().collect::<Vec<_>>();
    let remaining = bounded_map(&groups, |(prefix, members)| {
        if prefix.is_empty() || members.len() < 8 {
            return Ok(members.clone());
        }
        let mut wanted: BTreeMap<(String, String), Vec<Entry>> = BTreeMap::new();
        for entry in members {
            wanted
                .entry((entry.key.clone(), entry.version.clone()))
                .or_default()
                .push(entry.clone());
        }
        let mut request = json!({"Bucket":client.bucket,"Prefix":prefix,"MaxKeys":1000});
        let mut seen = BTreeSet::new();
        for _ in 0..2 {
            let response = match client.call_s3("list_object_versions", &request, None) {
                Ok(response) => response.value,
                Err(error)
                    if matches!(
                        error.code.as_str(),
                        "AccessDenied" | "NotImplemented" | "MethodNotAllowed" | "501" | "405"
                    ) || matches!(error.status, Some(403 | 405 | 501)) =>
                {
                    break;
                }
                Err(error) => return Err(error.into()),
            };
            for item in response["Versions"].as_array().into_iter().flatten() {
                if let Some(matches) = wanted.remove(&identity(item)?) {
                    for entry in matches {
                        validate_info(&entry, item)?;
                    }
                }
            }
            if wanted.is_empty() || response["IsTruncated"] != true {
                break;
            }
            let marker = (
                response["NextKeyMarker"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                response["NextVersionIdMarker"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            );
            if marker.0.is_empty() || !seen.insert(marker.clone()) {
                break;
            }
            request["KeyMarker"] = marker.0.into();
            if marker.1.is_empty() {
                request.as_object_mut().unwrap().remove("VersionIdMarker");
            } else {
                request["VersionIdMarker"] = marker.1.into();
            }
        }
        Ok(wanted.into_values().flatten().collect::<Vec<_>>())
    })?
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    bounded_map(&remaining, |entry| {
        if allow_archive {
            verify_head(client, repo, entry)
        } else {
            let info = client.call_s3(
                "head_object",
                &json!({"Bucket":client.bucket,"Key":entry.key,"VersionId":entry.version}),
                None,
            )?;
            validate_info(entry, &info.value)
        }
    })?;
    Ok(())
}

pub(crate) fn verify_storage_entries(
    client: &S3Client,
    repo: &GitRepo,
    metadata: &[StorageEntry],
) -> Result<()> {
    let entries = metadata
        .iter()
        .map(|metadata| {
            validate_digest(metadata)?;
            native_engine::validate_verification_scope(client, metadata)?;
            let version = metadata
                .version_id
                .clone()
                .filter(|version| !version.is_empty() && version != "null")
                .ok_or_else(|| {
                    Error::message(format!(
                        "managed-storage object has no exact version ID: {}",
                        metadata.object
                    ))
                })?;
            Ok(Entry {
                key: key(client, &metadata.object),
                version,
                etag: metadata.etag.clone(),
                metadata: metadata.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    verify_entries_with_placement(client, repo, &entries, false)
}

pub(crate) fn read(
    repo: &GitRepo,
    pointers: &[String],
    operation: &str,
    receipts: &[Value],
) -> Result<Value> {
    if !matches!(operation, "--verify" | "--fetch") {
        return Err(Error::message("unknown native version read operation"));
    }
    let client = S3Client::from_repo(repo)?;
    check_client_versioning(&client)?;
    let mut entries = Vec::new();
    for metadata in native_engine::metadata_entries(repo, None, pointers)? {
        validate_digest(&metadata)?;
        let version = metadata
            .version_id
            .clone()
            .filter(|v| !v.is_empty() && v != "null")
            .ok_or_else(|| {
                Error::message(format!(
                    "managed-storage object has no exact version ID: {}",
                    metadata.object
                ))
            })?;
        entries.push(Entry {
            key: key(&client, &metadata.object),
            version,
            etag: metadata.etag.clone(),
            metadata,
        });
    }
    pending_aliases(&client, &mut entries, receipts)?;
    for entry in &entries {
        if let Some(proof) = &entry.metadata.verification
            && (proof.endpoint != client.endpoint_identity()
                || proof.bucket != client.bucket
                || proof.key != entry.key
                || proof.version_id != entry.version
                || entry.metadata.size != Some(proof.size))
        {
            return Err(Error::message(
                "storage proof differs from its resolved physical binding",
            ));
        }
    }
    if operation == "--fetch" {
        let cache = native_engine::CachePaths::new(repo)?;
        let mut cached = Vec::new();
        let mut missing = Vec::new();
        let cpus = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1);
        let local = bounded_map_with_workers(&entries, cpus, false, |entry| {
            let path = cache.entry(&entry.metadata)?;
            Ok((entry.clone(), content_matches(entry, &path)?))
        })?;
        for (entry, available) in local {
            if available {
                cached.push(entry);
            } else {
                missing.push(entry);
            }
        }
        verify_entries(&client, repo, &cached)?;
        let cache_root = cache.root();
        fs::create_dir_all(cache_root).at(cache_root)?;
        let scratch = tempfile::tempdir_in(cache_root).at(cache_root)?;
        let requests = missing.iter().enumerate().collect::<Vec<_>>();
        let downloaded = bounded_map(&requests, |(index, entry)| {
            let destination = scratch.path().join(index.to_string());
            let mut current = (*entry).clone();
            let mut hashes = native_engine::HashInventory::new();
            let mut seen = BTreeSet::new();
            loop {
                let mut args =
                    json!({"Bucket":client.bucket,"Key":current.key,"VersionId":current.version});
                if let Some(tag) = &current.etag {
                    args["IfMatch"] = format!("\"{}\"", etag(tag)).into();
                }
                let mut attempt = 0;
                let response = loop {
                    match client.get_to_file(&args, &destination) {
                        Ok(response) => break Ok(response),
                        Err(error) if error.is_retryable() && attempt < 2 => {
                            attempt += 1;
                        }
                        Err(error) => break Err(error),
                    }
                };
                match response {
                    Ok(response) => {
                        validate_info(&current, &response.value)?;
                        if !content_matches_with_inventory(entry, &destination, &mut hashes)? {
                            return Err(Error::message(format!(
                                "downloaded content hash mismatch: {}",
                                entry.metadata.object
                            )));
                        }
                        return Ok((entry.metadata.clone(), destination, hashes));
                    }
                    Err(error) if error.is_missing() => {
                        current = mapped_entry(&client, repo, &current, &mut seen)?
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        })?;
        bounded_map_with_workers(&downloaded, cpus, false, |(metadata, path, verified)| {
            cache.install_entry_with_inventory(metadata, path, &mut verified.clone())
        })?;
        native_engine::install_directory_manifests(repo, pointers)?;
    } else {
        verify_entries(&client, repo, &entries)?;
    }
    let checked = entries
        .iter()
        .map(|entry| entry.metadata.object.clone())
        .collect::<BTreeSet<_>>();
    Ok(json!({"mode":"version-aware","remote":"workspace-mgr","checked_objects":checked}))
}

fn path_version_pointers(
    repo: &GitRepo,
    revision: Option<&str>,
    pointers: Vec<String>,
) -> Result<Vec<String>> {
    let Some(revision) = revision else {
        return Ok(pointers);
    };
    if !crate::legacy_dvc::is_content_addressed_revision(repo, revision)? {
        return Ok(pointers);
    }
    let mut retained = Vec::new();
    let legacy = pointers
        .iter()
        .filter(|pointer| pointer.ends_with(".dvc"))
        .cloned()
        .collect::<Vec<_>>();
    let documents = storage_metadata::read_pointer_documents(repo, Some(revision), &legacy)?;
    let raw_files = repo.show_regular_files(revision, &legacy)?;
    for pointer in pointers {
        if !pointer.ends_with(".dvc") {
            retained.push(pointer);
            continue;
        }
        let raw = &raw_files[&pointer];
        let (document, _) = &documents[&pointer];
        let [output] = document.outs.as_slice() else {
            return Err(Error::message(format!(
                "historical storage metadata must define one output: {pointer}"
            )));
        };
        let identity = output.md5.as_deref().unwrap_or_default();
        let digest = identity.strip_suffix(".dir").unwrap_or(identity);
        if digest.len() != 32
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(Error::message(format!(
                "historical storage metadata has no supported checksum: {pointer}"
            )));
        }
        let yaml: serde_yaml::Value = serde_yaml::from_str(raw).map_err(|_| {
            Error::message(format!("invalid historical storage metadata: {pointer}"))
        })?;
        let row = &yaml["outs"][0];
        let bound = row.get("cloud").is_some()
            || row["files"]
                .as_sequence()
                .is_some_and(|files| files.iter().any(|entry| entry.get("cloud").is_some()));
        if bound {
            retained.push(pointer);
        }
        // Unbound CAS history names shared hash objects outside the logical
        // task prefix. Keep those sources; only exact path versions belong in
        // archive copying and permanent prefix retirement. No .dir cache is
        // needed to establish that distinction.
    }
    Ok(retained)
}

pub(crate) fn purge(repo: &GitRepo, operation: &str, payload: &Value) -> Result<Value> {
    if operation == "list" {
        let mut result = Vec::new();
        for request in payload
            .as_array()
            .ok_or_else(|| Error::message("invalid storage history request"))?
        {
            let revision = request["revision"].as_str();
            let pointers: Vec<String> = serde_json::from_value(request["pointers"].clone())
                .map_err(|e| Error::message(e.to_string()))?;
            let pointers = path_version_pointers(repo, revision, pointers)?;
            for entry in native_engine::metadata_entries(repo, revision, &pointers)? {
                match entry
                    .version_id
                    .as_deref()
                    .filter(|v| !v.is_empty() && *v != "null")
                {
                    Some(version) => result.push(
                        json!({"pointer":entry.pointer,"object":entry.object,"version_id":version}),
                    ),
                    None if revision.is_some() => {
                        return Err(Error::message(format!(
                            "managed-storage object has no exact version ID: {}",
                            entry.pointer
                        )));
                    }
                    None => {}
                }
            }
        }
        return Ok(Value::Array(result));
    }
    if operation != "delete" {
        return Err(Error::message("unknown native purge operation"));
    }
    let client = S3Client::from_repo(repo)?;
    check_client_versioning(&client)?;
    delete_candidates(&client, repo, payload)
}

fn delete_candidates(client: &S3Client, repo: &GitRepo, payload: &Value) -> Result<Value> {
    let candidates = payload
        .get("candidates")
        .unwrap_or(payload)
        .as_array()
        .ok_or_else(|| Error::message("invalid purge candidates"))?;
    let mut coordination = BTreeMap::new();
    for proof in payload["coordination"].as_array().into_iter().flatten() {
        let source = string(&proof["receipt"], "source")?;
        if coordination
            .insert(source.to_owned(), proof.clone())
            .is_some()
        {
            return Err(Error::message("duplicate archive cleanup coordination"));
        }
    }
    let mut prefixes = BTreeMap::new();
    let mut archives: BTreeMap<String, BTreeMap<String, Vec<Value>>> = BTreeMap::new();
    let mut generic: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for receipt in payload["prefixes"].as_array().into_iter().flatten() {
        let source = string(receipt, "source")?;
        if receipt["status"] != "copied"
            || !receipt["versions"].is_array()
            || prefixes
                .insert(source.to_owned(), receipt.clone())
                .is_some()
        {
            return Err(Error::message("invalid archive retirement prefix"));
        }
        archives.insert(source.to_owned(), BTreeMap::new());
    }
    for candidate in candidates {
        let pointer = string(candidate, "pointer")?;
        let object = string(candidate, "object")?;
        string(candidate, "version_id")?;
        if let Some(source) = pointer.strip_suffix(ARCHIVE_SUFFIX) {
            validate_task_prefix(source)?;
            if !object.starts_with(&format!("{source}/")) {
                return Err(Error::message(
                    "archive purge object escapes its task prefix",
                ));
            }
            archives
                .entry(source.to_owned())
                .or_default()
                .entry(object.to_owned())
                .or_default()
                .push(candidate.clone());
        } else {
            validate_task_prefix(object)?;
            generic
                .entry(object.to_owned())
                .or_default()
                .push(candidate.clone());
        }
    }
    let verify_registry = |source: &str,
                           objects: &BTreeMap<String, Vec<Value>>,
                           present: Option<&BTreeMap<(String, String), Value>>|
     -> Result<Value> {
        let receipt = native_archive::registry_read(client, repo, source)?.ok_or_else(|| {
            Error::message("archive cleanup requires a published canonical registry")
        })?;
        if prefixes
            .get(source)
            .is_some_and(|expected| *expected != receipt)
        {
            return Err(Error::message(
                "archive retirement prefix differs from its canonical registry",
            ));
        }
        let proof = coordination.get(source).ok_or_else(|| {
            Error::message(
                "archive cleanup requires an atomic Git registry binding and published receipt",
            )
        })?;
        if proof["receipt"] != receipt {
            return Err(Error::message(
                "archive cleanup registry differs from its coordinated receipt",
            ));
        }
        if proof["coordination"]["publication_oid"]
            .as_str()
            .is_none_or(str::is_empty)
        {
            return Err(Error::message(
                "archive cleanup requires a receipt published on the shared branch",
            ));
        }
        native_archive::verify_coordination(client, repo, &receipt, &proof["coordination"], true)?;
        native_archive::verify_history(client, &receipt)?;
        let mapped = receipt["versions"]
            .as_array()
            .ok_or_else(|| Error::message("invalid canonical registry"))?
            .iter()
            .map(|row| {
                Ok((
                    string(row, "source_object")?.to_owned(),
                    string(row, "source_version_id")?.to_owned(),
                ))
            })
            .collect::<Result<BTreeSet<_>>>()?;
        for (object, candidates) in objects {
            for candidate in candidates {
                let version = string(candidate, "version_id")?;
                if present
                    .is_none_or(|p| p.contains_key(&(key(client, object), version.to_owned())))
                    && !mapped.contains(&(object.clone(), version.to_owned()))
                {
                    return Err(Error::message(
                        "archive cleanup candidate is not mapped by its published registry",
                    ));
                }
            }
        }
        Ok(receipt)
    };
    let mut deleted = Vec::new();
    let mut absent = Vec::new();
    let mut retained = Vec::new();
    let mut cleaned = BTreeSet::new();
    for (source, objects) in &archives {
        let prefix = key(client, &format!("{source}/"));
        let present = client
            .list_versions(&prefix)?
            .into_iter()
            .map(|v| Ok((identity(&v)?, v)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        verify_registry(source, objects, Some(&present))?;
        generic.retain(|object, _| !object.starts_with(&format!("{source}/")));
        let mut wanted = BTreeSet::new();
        for (object, candidates) in objects {
            let remote = key(client, object);
            let requested = candidates
                .iter()
                .map(|v| string(v, "version_id").map(str::to_owned))
                .collect::<Result<BTreeSet<_>>>()?;
            let mut existing = Vec::new();
            for version in requested {
                wanted.insert((remote.clone(), version.clone()));
                if present.contains_key(&(remote.clone(), version.clone())) {
                    verify_registry(source, objects, Some(&present))?;
                    client.call_s3(
                        "delete_object",
                        &json!({"Bucket":client.bucket,"Key":remote,"VersionId":version}),
                        None,
                    )?;
                    existing.push(version);
                }
            }
            if existing.is_empty() {
                absent.push(candidates[0].clone());
            } else {
                let mut result = candidates[0].clone();
                result["deleted_version_ids"] = existing.into();
                deleted.push(result);
            }
        }
        let remaining = client.list_versions(&prefix)?;
        if remaining
            .iter()
            .any(|item| identity(item).is_ok_and(|id| wanted.contains(&id)))
        {
            return Err(Error::message(
                "mapped archive object versions still exist after permanent deletion",
            ));
        }
        if remaining.is_empty() {
            cleaned.insert(source.clone());
        }
        for item in remaining {
            retained.push(json!({"pointer":format!("{source}{ARCHIVE_SUFFIX}"),"object":format!("{source}/{}",string(&item,"Key")?.strip_prefix(&prefix).ok_or_else(||Error::message("purge history escaped prefix"))?),"version_id":string(&item,"VersionId")?}));
        }
    }
    // Archive prefixes finish first. Each worker owns one distinct logical key
    // and performs fresh ancestor registry checks, exact deletes and verification.
    // No worker updates pointer documents, coordination bindings or purge state.
    let generic = generic.into_iter().enumerate().collect::<Vec<_>>();
    let mut completed = bounded_map_with_workers(
        &generic,
        PURGE_WORKERS,
        true,
        |(index, (object, candidates))| {
            let mut deleted = Vec::new();
            let mut absent = Vec::new();
            let mut retained = Vec::new();
            let parts = object.split('/').collect::<Vec<_>>();
            let mut archive = None;
            for length in (1..parts.len()).rev() {
                let source = parts[..length].join("/");
                if let Some(receipt) = native_archive::registry_read(client, repo, &source)? {
                    archive = Some((source, receipt));
                    break;
                }
            }
            let remote = key(client, object);
            let versions = client
                .list_versions(&remote)?
                .into_iter()
                .filter(|v| v["Key"] == remote)
                .collect::<Vec<_>>();
            if let Some((source, receipt)) = archive {
                let mapped = receipt["versions"]
                    .as_array()
                    .ok_or_else(|| Error::message("invalid canonical registry"))?
                    .iter()
                    .map(|row| {
                        Ok((
                            string(row, "source_object")?.to_owned(),
                            string(row, "source_version_id")?.to_owned(),
                        ))
                    })
                    .collect::<Result<BTreeSet<_>>>()?;
                let exact = candidates
                    .iter()
                    .filter(|item| {
                        item["version_id"]
                            .as_str()
                            .is_some_and(|v| mapped.contains(&(object.clone(), v.to_owned())))
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                let objects = BTreeMap::from([(object.clone(), exact.clone())]);
                if !exact.is_empty() {
                    verify_registry(&source, &objects, None)?;
                }
                let mut present = BTreeSet::new();
                for item in versions {
                    let version = string(&item, "VersionId")?;
                    present.insert(version.to_owned());
                    if mapped.contains(&(object.clone(), version.to_owned())) {
                        if exact.is_empty() {
                            return Err(Error::message(
                                "generic archive retirement has no coordinated exact mapping",
                            ));
                        }
                        verify_registry(&source, &objects, None)?;
                        client.call_s3(
                            "delete_object",
                            &json!({"Bucket":client.bucket,"Key":remote,"VersionId":version}),
                            None,
                        )?;
                    } else {
                        retained.push(json!({"pointer":format!("{source}{ARCHIVE_SUFFIX}"),"object":object,"version_id":version}));
                    }
                }
                for candidate in candidates {
                    let version = string(candidate, "version_id")?;
                    if !present.contains(version) {
                        absent.push(candidate.clone());
                    } else if mapped.contains(&(object.clone(), version.to_owned())) {
                        deleted.push(candidate.clone());
                    }
                }
            } else {
                if versions.is_empty() {
                    absent.push(candidates[0].clone());
                    return Ok((*index, deleted, absent, retained));
                }
                let mut ids = versions
                    .iter()
                    .map(|item| string(item, "VersionId").map(str::to_owned))
                    .collect::<Result<Vec<_>>>()?;
                if ids.len() == 1 || ids.iter().any(|version| version == "null") {
                    // A bucket can retain its pre-versioning null generation.
                    // Keep the existing explicit single-delete behavior for
                    // that inventory; the batch helper accepts immutable IDs.
                    for version in &ids {
                        client.call_s3(
                            "delete_object",
                            &json!({"Bucket":client.bucket,"Key":remote,"VersionId":version}),
                            None,
                        )?;
                    }
                } else {
                    // This branch has no canonical archive mapping or per-
                    // version registry guard. Retire this one logical object's
                    // exact versions together; the client validates every
                    // reported result and bounds requests to S3's 1,000 items.
                    let exact = ids
                        .iter()
                        .map(|version| (remote.clone(), version.clone()))
                        .collect::<Vec<_>>();
                    client.delete_versions(&exact)?;
                }
                if client
                    .list_versions(&remote)?
                    .iter()
                    .any(|v| v["Key"] == remote)
                {
                    return Err(Error::message(format!(
                        "managed-storage object versions still exist after permanent deletion: {object}"
                    )));
                }
                ids.sort();
                let mut result = candidates[0].clone();
                result["deleted_version_ids"] = ids.into();
                deleted.push(result);
            }
            Ok((*index, deleted, absent, retained))
        },
    )?;
    completed.sort_by_key(|(index, _, _, _)| *index);
    for (_, object_deleted, object_absent, object_retained) in completed {
        deleted.extend(object_deleted);
        absent.extend(object_absent);
        retained.extend(object_retained);
    }
    retained.sort_by_key(|v| {
        (
            v["pointer"].as_str().unwrap_or_default().to_owned(),
            v["object"].as_str().unwrap_or_default().to_owned(),
            v["version_id"].as_str().unwrap_or_default().to_owned(),
        )
    });
    Ok(
        json!({"mode":"permanent-version-deletion","remote":"workspace-mgr","deleted":deleted,"already_absent":absent,"retained_unmapped":retained,"retained_mapped":[],"cleaned_prefixes":cleaned}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_workers_preserve_input_order_after_out_of_order_completion() {
        use std::sync::{Condvar, Mutex};
        use std::time::Duration;

        let finished = (Mutex::new(0_usize), Condvar::new());
        let input = (0..32).collect::<Vec<_>>();
        let first_wave = (Mutex::new(0_usize), Condvar::new());
        let output = bounded_map_with_workers(&input, 4, false, |value| {
            if *value < 4 {
                let (lock, changed) = &first_wave;
                let mut arrived = lock.lock().unwrap();
                *arrived += 1;
                changed.notify_all();
                let (arrived, _) = changed
                    .wait_timeout_while(arrived, Duration::from_secs(5), |arrived| *arrived < 4)
                    .unwrap();
                assert_eq!(*arrived, 4, "the first four callbacks must overlap");
            }
            let (lock, changed) = &finished;
            if *value == 0 {
                let (completed, _) = changed
                    .wait_timeout_while(lock.lock().unwrap(), Duration::from_secs(5), |done| {
                        *done < input.len() - 1
                    })
                    .unwrap();
                assert_eq!(
                    *completed,
                    input.len() - 1,
                    "all other inputs finish before input zero"
                );
            } else {
                *lock.lock().unwrap() += 1;
                changed.notify_all();
            }
            Ok(value * value)
        })
        .unwrap();
        assert_eq!(
            output,
            input.iter().map(|value| value * value).collect::<Vec<_>>()
        );
    }

    #[test]
    fn key_preserves_literal_s3_segments_and_folder_markers() {
        // The low-level client receives the literal name; only task prefixes
        // are normalized, never archive history object names.
        assert!(validate_task_prefix("old/task").is_ok());
        assert!(validate_task_prefix("old/../task").is_err());
        assert!(validate_task_prefix("old//task").is_err());
    }
    #[test]
    fn bounded_workers_visit_every_item_once() {
        use std::sync::{Condvar, Mutex};
        use std::time::Duration;

        let input = (0..100).collect::<Vec<_>>();
        let active = AtomicUsize::new(0);
        let maximum = AtomicUsize::new(0);
        let first_callbacks = (Mutex::new((0_usize, false)), Condvar::new());
        let mut output = bounded_map(&input, |value| {
            // Count client work itself: an abandoned HTTP response handler
            // cannot inflate the number of bounded-map callbacks in flight.
            let current = active.fetch_add(1, Ordering::SeqCst) + 1;
            maximum.fetch_max(current, Ordering::SeqCst);
            let (lock, changed) = &first_callbacks;
            let mut gate = lock.lock().unwrap();
            gate.0 += 1;
            if gate.0 >= 2 {
                gate.1 = true;
                changed.notify_all();
            }
            if !gate.1 {
                let (mut gate, _) = changed
                    .wait_timeout_while(gate, Duration::from_secs(5), |gate| !gate.1)
                    .unwrap();
                // A sequential regression waits only once, then releases
                // every remaining callback before the overlap assertion fails.
                gate.1 = true;
                changed.notify_all();
            }
            active.fetch_sub(1, Ordering::SeqCst);
            Ok(value * 2)
        })
        .unwrap();
        output.sort();
        assert_eq!(output, (0..100).map(|v| v * 2).collect::<Vec<_>>());
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert!(maximum.load(Ordering::SeqCst) <= WORKERS);
        assert!(maximum.load(Ordering::SeqCst) > 1);
    }
}

#[cfg(test)]
#[path = "native_versions_tests.rs"]
mod transport_tests;
