use std::collections::BTreeSet;
use std::fs;
use std::io::Write;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::path::{allowed, reject_symlink_traversal, repo_path, resolved_under};
use crate::policy::TASK_MANIFEST_NAME;
use crate::s3_purge::ObjectVersion;
use crate::storage_metadata;

pub const RECEIPT_NAME: &str = ".workspace-mgr-archive.json";

/// Rust-owned task metadata is retained beside the transport mapping. Keep
/// reconstruction and comparison in publish/cancel on the same field list.
pub(crate) const RECEIPT_METADATA_FIELDS: [&str; 6] = [
    "task_id",
    "previous_receipt",
    "completion_reviews",
    "historical_records",
    "closed_pull_request",
    "migration_kind",
];

pub(crate) fn is_task_rename(receipt: &Value) -> bool {
    receipt["migration_kind"] == "task-rename"
}

/// A rename changes the readable slug, never the immutable task timestamp.
/// Keep this exception narrower than ordinary arbitrary-prefix relocation.
fn validate_rename_identity(receipt: &Value, source: &str, destination: &str) -> Result<()> {
    use crate::manifest::{TaskKind, parse_task_identity};
    let identity = parse_task_identity(TaskKind::Deliverable, text(receipt, "task_id")?)?;
    let old = parse_task_identity(TaskKind::Deliverable, source)?;
    let new = parse_task_identity(TaskKind::Deliverable, destination)?;
    if identity.timestamp != old.timestamp || identity.timestamp != new.timestamp {
        return Err(Error::message(
            "task rename migration changes the immutable task timestamp",
        ));
    }
    Ok(())
}

pub fn plan(repo: &GitRepo, config: &Config, source: &str, destination: &str) -> Result<Value> {
    if !config.s3_enabled() {
        return Ok(
            json!({"schema_version":1,"source":source,"destination":destination,
            "status":"planned","versions":[]}),
        );
    }
    if !config.requires_object_versioning() {
        return Err(Error::message(
            "archive requires version-aware S3 storage; migrate the repository before archiving stored tasks",
        ));
    }
    storage_metadata::ensure_ready(repo, config)?;
    storage_metadata::verify_object_versioning(repo, config)?;
    storage_metadata::version_archive_adapter(
        repo,
        "plan",
        &json!({"source":source,"destination":destination}),
    )
}

pub fn receipts(repo: &GitRepo, scopes: &[String]) -> Result<Vec<(String, Value)>> {
    let mut result = Vec::new();
    for path in repo.visible_paths(scopes)? {
        if !path.ends_with(&format!("/{RECEIPT_NAME}")) {
            continue;
        }
        reject_symlink_traversal(&repo.root, &path, "archive receipt")?;
        let absolute = resolved_under(&repo.root, &path);
        let raw = fs::read_to_string(&absolute).at(&absolute)?;
        let receipt: Value = serde_json::from_str(&raw)
            .map_err(|error| Error::message(format!("invalid archive receipt {path}: {error}")))?;
        validate(&path, &receipt)?;
        result.push((path, receipt));
    }
    Ok(result)
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::message(format!("archive receipt is missing {key}")))
}

pub(crate) fn validate(path: &str, receipt: &Value) -> Result<()> {
    let source = repo_path(text(receipt, "source")?, "archive source")?;
    let destination = repo_path(text(receipt, "destination")?, "archive destination")?;
    let rename = is_task_rename(receipt);
    if receipt.get("migration_kind").is_some() && !rename {
        return Err(Error::message("unknown task migration kind"));
    }
    if rename {
        validate_rename_identity(receipt, &source, &destination)?;
    }
    if receipt["schema_version"] != 1
        || !matches!(receipt["status"].as_str(), Some("planned" | "copied"))
        || path != format!("{destination}/{RECEIPT_NAME}")
        || source == destination
        || source.starts_with(&format!("{destination}/"))
        || destination.starts_with(&format!("{source}/"))
        || !rename && source.rsplit('/').next() != destination.rsplit('/').next()
        || !receipt["versions"].is_array()
    {
        return Err(Error::message(format!(
            "invalid archive migration identity or location: {path}"
        )));
    }
    text(receipt, "task_id")?;
    for version in receipt["versions"].as_array().expect("validated versions") {
        let object = text(version, "source_object")?;
        if !object.starts_with(&format!("{source}/"))
            || text(version, "destination_object")?
                != format!("{destination}{}", &object[source.len()..])
            || !version["delete_marker"].is_boolean()
        {
            return Err(Error::message(format!(
                "archive version escapes its task prefixes: {path}"
            )));
        }
        text(version, "source_version_id")?;
        if receipt["status"] == "copied" {
            text(version, "destination_version_id")?;
        }
    }
    Ok(())
}

pub fn pointer_set(repo: &GitRepo, scopes: &[String]) -> Result<BTreeSet<String>> {
    let mut result = BTreeSet::new();
    for (_, receipt) in receipts(repo, scopes)? {
        // Active renamed tasks keep normal reconciliation: unchanged copied
        // versions are reused, while later edits upload only changed bytes.
        if is_task_rename(&receipt) {
            continue;
        }
        result.extend(storage_metadata::discover(
            repo,
            &[text(&receipt, "destination")?.to_owned()],
        )?);
    }
    Ok(result)
}

pub(crate) fn pending_rename_source(
    repo: &GitRepo,
    task: &crate::manifest::ResolvedTask,
    base: &str,
) -> Result<Option<String>> {
    let Some(destination) = task.task_path.as_deref() else {
        return Ok(None);
    };
    let path = format!("{destination}/{RECEIPT_NAME}");
    let absolute = resolved_under(&repo.root, &path);
    let published = repo.run_unchecked(["show", &format!("{base}:{path}")])?;
    let published_receipt = published
        .success()
        .then(|| serde_json::from_str::<Value>(&published.stdout).ok())
        .flatten();
    if let Some(original) = published_receipt
        .as_ref()
        .filter(|receipt| is_task_rename(receipt) && receipt["status"] == "copied")
    {
        validate(&path, original)?;
        reject_symlink_traversal(&repo.root, &path, "published task rename receipt")?;
        let current = fs::read(&absolute)
            .ok()
            .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok());
        if current.as_ref() != Some(original) {
            return Err(Error::message(
                "published task rename receipt was removed or changed; restore its original copied history binding before publication",
            ));
        }
        if original["task_id"] != task.task_id {
            return Err(Error::message(
                "published task rename receipt belongs to another immutable task identity",
            ));
        }
        return Ok(None);
    }
    if !absolute.exists() {
        return Ok(None);
    }
    reject_symlink_traversal(&repo.root, &path, "task rename receipt")?;
    let receipt: Value = serde_json::from_slice(&fs::read(&absolute).at(&absolute)?)
        .map_err(|error| Error::message(format!("invalid task rename receipt: {error}")))?;
    if !is_task_rename(&receipt) {
        return Ok(None);
    }
    validate(&path, &receipt)?;
    if receipt["task_id"] != task.task_id {
        return Err(Error::message(
            "task rename receipt belongs to another immutable task identity",
        ));
    }
    if published_receipt.as_ref() == Some(&receipt) {
        return Ok(None);
    }
    crate::archive_cancel::validate_migration(repo, &receipt)?;
    let source = text(&receipt, "source")?;
    if resolved_under(&repo.root, source).exists() {
        return Err(Error::message(
            "the original S3 task rename source reappeared locally; preserve the conflicting path before publication",
        ));
    }
    Ok(Some(source.to_owned()))
}

/// The source snapshot is revalidated by the transport before it copies any
/// version. Journals live outside the checkout and survive a failed publish.
pub fn prepare(
    repo: &GitRepo,
    config: &Config,
    scopes: &[String],
    base: &str,
) -> Result<Vec<Value>> {
    let mut completed = Vec::new();
    let mut proof_client = None;
    for (path, mut receipt) in receipts(repo, scopes)? {
        let source = text(&receipt, "source")?.to_owned();
        let destination = text(&receipt, "destination")?.to_owned();
        let existing = repo.run_unchecked(["show", &format!("{base}:{path}")])?;
        let published = existing.success()
            && serde_json::from_str::<Value>(&existing.stdout)
                .ok()
                .as_ref()
                == Some(&receipt);
        if published && is_task_rename(&receipt) {
            completed.push(receipt);
            continue;
        }
        let pointers = storage_metadata::discover(repo, std::slice::from_ref(&destination))?;
        for pointer in &pointers {
            let absolute = resolved_under(&repo.root, pointer);
            let raw = fs::read_to_string(&absolute).at(&absolute)?;
            let output = resolved_under(
                &repo.root,
                storage_metadata::boundary_path(pointer).unwrap(),
            );
            if !is_task_rename(&receipt)
                && output.exists()
                && !storage_metadata::payload_matches_metadata(repo, pointer, &raw)?
            {
                return Err(Error::message(format!(
                    "archived output changed after planning: {pointer}"
                )));
            }
        }
        if !published {
            if !allowed(&source, scopes) || !allowed(&destination, scopes) {
                return Err(Error::message(
                    "archive migration requires both source and destination in the infrastructure task's declared scopes",
                ));
            }
            let manifest_path = format!("{destination}/{TASK_MANIFEST_NAME}");
            reject_symlink_traversal(&repo.root, &manifest_path, "current archived task manifest")?;
            let absolute = resolved_under(&repo.root, &manifest_path);
            let current = fs::read_to_string(&absolute).at(&absolute)?;
            let manifest: crate::manifest::TaskManifest =
                toml::from_str(&current).map_err(|error| {
                    Error::message(format!("invalid current archived task manifest: {error}"))
                })?;
            if manifest.kind != crate::manifest::TaskKind::Deliverable
                || manifest.id != text(&receipt, "task_id")?
                || manifest.path.as_deref() != Some(destination.as_str())
            {
                return Err(Error::message(
                    "current archived task identity or directory differs from the migration receipt",
                ));
            }
            crate::archive_cancel::validate_migration(repo, &receipt)?;
        }
        if receipt["status"] == "planned" {
            if config.s3_enabled() {
                let digest = crate::hex::encode_lower(
                    Sha256::digest(format!("{source}\0{destination}").as_bytes()).as_slice(),
                );
                let journal_dir = repo.local_state_dir()?.join("archive");
                fs::create_dir_all(&journal_dir).at(&journal_dir)?;
                let journal = journal_dir.join(format!("{digest}.json"));
                let reservation = crate::archive_reservation::reserve(repo, &receipt)?;
                let copied = storage_metadata::version_archive_adapter(
                    repo,
                    "copy",
                    &json!({
                        "source":source,"destination":destination,"planned":receipt,
                        "state_path":journal.to_string_lossy(),"reservation":reservation
                    }),
                )?;
                // Keep task identity and earlier receipts, which the transport
                // intentionally does not interpret.
                let mut next = copied;
                for key in RECEIPT_METADATA_FIELDS {
                    if let Some(value) = receipt.get(key) {
                        next.as_object_mut()
                            .ok_or_else(|| Error::message("archive copy did not return an object"))?
                            .insert(key.to_owned(), value.clone());
                    }
                }
                receipt = next;
                storage_metadata::version_archive_adapter(repo, "verify", &receipt)?;
                storage_metadata::archive_registry_adapter(repo, "publish", &receipt)?;
            } else {
                receipt["status"] = "copied".into();
            }
            write_receipt(repo, &path, &receipt)?;
        }
        if config.s3_enabled() && receipt["status"] == "copied" {
            if !published && !trusted_copy_journal(repo, &receipt)? {
                storage_metadata::version_archive_adapter(repo, "verify-source", &receipt)?;
            }
            storage_metadata::version_archive_adapter(repo, "verify", &receipt)?;
            storage_metadata::archive_registry_adapter(repo, "publish", &receipt)?;
            for pointer in &pointers {
                rewrite_pointer_with_client(repo, pointer, &receipt, &mut proof_client)?;
            }
            if !is_task_rename(&receipt) {
                storage_metadata::verify_archived(repo, &pointers)?;
            }
        }
        completed.push(receipt);
    }
    Ok(completed)
}

pub(crate) fn trusted_copy_journal(repo: &GitRepo, receipt: &Value) -> Result<bool> {
    let source = text(receipt, "source")?;
    let destination = text(receipt, "destination")?;
    let digest = crate::hex::encode_lower(
        Sha256::digest(format!("{source}\0{destination}").as_bytes()).as_slice(),
    );
    let path = repo
        .local_state_dir()?
        .join("archive")
        .join(format!("{digest}.json"));
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => return Err(Error::Io { path, source }),
    };
    let mut journal: Value = serde_json::from_str(&raw)
        .map_err(|error| Error::message(format!("invalid private archive journal: {error}")))?;
    if !matches!(journal["schema_version"].as_u64(), Some(1 | 2)) {
        return Err(Error::message(
            "private archive copy journal has an unsupported schema",
        ));
    }
    journal["schema_version"] = 1.into();
    if [
        "schema_version",
        "remote",
        "bucket",
        "remote_prefix",
        "source",
        "destination",
        "transaction_id",
    ]
    .iter()
    .any(|key| journal[key] != receipt[key])
        || !matches!(
            journal["status"].as_str(),
            Some("copied" | "canceling" | "cancelled")
        )
    {
        return Ok(false);
    }
    let Some(versions) = journal["versions"].as_array_mut() else {
        return Ok(false);
    };
    for version in versions {
        if let Some(object) = version.as_object_mut() {
            object.remove("started");
            object.remove("multipart_upload_id");
            object.remove("cancel_started");
            object.remove("cancel_deleted");
            object.remove("cancel_owned_versions");
        }
    }
    Ok(journal["versions"] == receipt["versions"])
}

pub fn purge_candidates(receipts: &[Value]) -> Result<Vec<ObjectVersion>> {
    let mut result = BTreeSet::new();
    for receipt in receipts {
        for version in receipt["versions"]
            .as_array()
            .ok_or_else(|| Error::message("archive versions missing"))?
        {
            result.insert(ObjectVersion {
                pointer: format!("{}/{RECEIPT_NAME}", text(receipt, "source")?),
                object: text(version, "source_object")?.to_owned(),
                version_id: text(version, "source_version_id")?.to_owned(),
            });
        }
    }
    Ok(result.into_iter().collect())
}

#[cfg(test)]
pub(crate) fn rewrite_pointer(repo: &GitRepo, pointer: &str, receipt: &Value) -> Result<()> {
    rewrite_pointer_with_client(repo, pointer, receipt, &mut None)
}

fn rewrite_pointer_with_client(
    repo: &GitRepo,
    pointer: &str,
    receipt: &Value,
    proof_client: &mut Option<crate::native_s3::S3Client>,
) -> Result<()> {
    let absolute = resolved_under(&repo.root, pointer);
    let raw = fs::read_to_string(&absolute).at(&absolute)?;
    let manifest = if pointer.ends_with(crate::storage_format::SUFFIX) {
        Some(crate::storage_format::Manifest::parse(&raw, pointer)?)
    } else {
        None
    };
    let entries = match &manifest {
        Some(manifest) => storage_metadata::logical_document(manifest).entries(pointer),
        None => storage_metadata::parse_pointer_document(&raw, pointer)?.entries(pointer),
    };
    if is_task_rename(receipt) && storage_metadata::hash_algorithm(&raw, pointer)? == "md5-dos2unix"
    {
        rebind_normalized_caches(repo, pointer, &entries, receipt)?;
    }
    if proof_client.is_none() && entries.iter().any(|entry| entry.verification.is_some()) {
        *proof_client = Some(crate::native_s3::S3Client::from_repo(repo)?);
    }
    let client = proof_client.as_ref();
    if let Some(mut manifest) = manifest {
        match manifest.kind {
            crate::storage_format::Kind::File => {
                if needs_copy_binding(&entries[0], receipt) {
                    manifest.version = Some(copied_version(client, &entries[0], receipt)?);
                }
            }
            crate::storage_format::Kind::Directory => {
                for (file, entry) in manifest.entries.as_mut().unwrap().iter_mut().zip(&entries) {
                    if needs_copy_binding(entry, receipt) {
                        file.version = Some(copied_version(client, entry, receipt)?);
                    }
                }
            }
        }
        let rendered = manifest.serialize()?;
        crate::archive_cancel::record_pointer_rewrite(repo, receipt, pointer, rendered.as_bytes())?;
        return atomic_write(&absolute, rendered.as_bytes());
    }
    // Legacy archive journals retain their original bytes until cancellation or migration.
    let mut document: serde_yaml::Value =
        serde_yaml::from_str(&raw).map_err(|source| Error::Yaml {
            path: absolute.clone(),
            source,
        })?;
    let outs = document["outs"]
        .as_sequence_mut()
        .ok_or_else(|| Error::message("archive pointer has no outputs"))?;
    let mut entry_index = 0;
    for out in outs {
        if let Some(files) = out
            .get_mut("files")
            .and_then(serde_yaml::Value::as_sequence_mut)
        {
            for file in files {
                replace_cloud(client, file, &entries[entry_index], receipt)?;
                entry_index += 1;
            }
        } else {
            replace_cloud(client, out, &entries[entry_index], receipt)?;
            entry_index += 1;
        }
    }
    let rendered =
        serde_yaml::to_string(&document).map_err(|error| Error::message(error.to_string()))?;
    crate::archive_cancel::record_pointer_rewrite(repo, receipt, pointer, rendered.as_bytes())?;
    atomic_write(&absolute, rendered.as_bytes())
}

/// A verified server copy preserves raw bytes, so its exact-version cache can
/// inherit the original cache's association. Never fill it from the current
/// payload, which may have been edited after the local rename.
fn rebind_normalized_caches(
    repo: &GitRepo,
    pointer: &str,
    entries: &[storage_metadata::PointerEntry],
    receipt: &Value,
) -> Result<()> {
    let cache = crate::native_engine::CachePaths::new(repo)?;
    let mut hashes = crate::native_engine::HashInventory::new();
    for entry in entries.iter().filter(|entry| entry.verification.is_none()) {
        let Some(row) = receipt["versions"].as_array().and_then(|rows| {
            rows.iter().find(|row| {
                row["destination_object"] == entry.key
                    && row["delete_marker"] == false
                    && entry.version_id.as_deref().is_some_and(|id| {
                        row["source_version_id"] == id || row["destination_version_id"] == id
                    })
            })
        }) else {
            continue;
        };
        let source = crate::native_engine::StorageEntry {
            pointer: pointer.to_owned(),
            object: text(row, "source_object")?.to_owned(),
            md5: entry.md5.clone(),
            size: entry.size,
            version_id: Some(text(row, "source_version_id")?.to_owned()),
            etag: row["source_etag"].as_str().map(str::to_owned),
            verification: None,
            hash_name: "md5-dos2unix".to_owned(),
        };
        let mut destination = source.clone();
        destination.object = entry.key.clone();
        destination.version_id = Some(text(row, "destination_version_id")?.to_owned());
        destination.etag = row["destination_etag"].as_str().map(str::to_owned);
        let source_path = cache.entry(&source)?;
        if !source_path.is_file() {
            return Err(Error::message(
                "task rename lost its exact source-version cache for normalized storage; hydrate or migrate the source binding before publishing",
            ));
        }
        cache.install_entry_with_inventory(&destination, &source_path, &mut hashes)?;
    }
    Ok(())
}

fn needs_copy_binding(entry: &storage_metadata::PointerEntry, receipt: &Value) -> bool {
    // Reconciliation can clear an edited output's version, or upload a newer
    // destination version, before a Git push fails. Those exact generated
    // controls are checked against the move journal before reaching here and
    // then verified by normal native storage in the destination namespace.
    !is_task_rename(receipt)
        || receipt["versions"].as_array().is_some_and(|versions| {
            versions.iter().any(|row| {
                row["destination_object"] == entry.key
                    && !row["delete_marker"].as_bool().unwrap_or(false)
                    && entry.version_id.as_deref().is_some_and(|id| {
                        row["source_version_id"] == id || row["destination_version_id"] == id
                    })
            })
        })
}

fn copied_version(
    client: Option<&crate::native_s3::S3Client>,
    entry: &storage_metadata::PointerEntry,
    receipt: &Value,
) -> Result<crate::storage_format::Version> {
    copied_version_for_scope(
        entry,
        receipt,
        client.map(|client| {
            (
                client.endpoint_identity(),
                client.bucket.as_str(),
                client.prefix.as_str(),
            )
        }),
    )
}

/// `prepare` verifies the exact source/copy journal and destination ownership
/// before rewriting a pointer. Derive a content proof only from a matching
/// source proof; a copy receipt alone cannot establish a content checksum.
fn copied_version_for_scope(
    entry: &storage_metadata::PointerEntry,
    receipt: &Value,
    scope: Option<(String, &str, &str)>,
) -> Result<crate::storage_format::Version> {
    let versions = receipt["versions"]
        .as_array()
        .ok_or_else(|| Error::message("archive versions missing"))?;
    let matching = versions
        .iter()
        .find(|version| {
            version["destination_object"] == entry.key
                && (version["source_version_id"].as_str() == entry.version_id.as_deref()
                    || version["destination_version_id"].as_str() == entry.version_id.as_deref())
                && !version["delete_marker"].as_bool().unwrap_or(false)
        })
        .ok_or_else(|| {
            Error::message(format!("archive has no copied version for {}", entry.key))
        })?;
    let id = text(matching, "destination_version_id")?.to_owned();
    let etag = text(matching, "destination_etag")?.to_owned();
    let verification = entry
        .verification
        .as_ref()
        .map(|proof| {
            proof.validate()?;
            let (endpoint, bucket, prefix) = scope.as_ref().ok_or_else(|| {
                Error::message("archive content proof requires its configured storage scope")
            })?;
            let full_key = |object: &str| {
                if prefix.is_empty() {
                    object.to_owned()
                } else {
                    format!("{}/{object}", prefix.trim_end_matches('/'))
                }
            };
            let source_key = full_key(text(matching, "source_object")?);
            let destination_key = full_key(text(matching, "destination_object")?);
            let source_version = text(matching, "source_version_id")?;
            let same_source = proof.key == source_key
                && entry.version_id.as_deref() == Some(source_version)
                && proof.version_id == source_version;
            let same_destination = proof.key == destination_key
                && entry.version_id.as_deref() == Some(id.as_str())
                && proof.version_id == id;
            let expected_etag = if same_source {
                text(matching, "source_etag")?
            } else {
                etag.as_str()
            };
            if receipt["status"] != "copied"
                || proof.endpoint != *endpoint
                || proof.bucket != *bucket
                || receipt["bucket"].as_str() != Some(*bucket)
                || receipt["remote_prefix"].as_str() != Some(*prefix)
                || !same_source && !same_destination
                || matching["size"].as_u64() != Some(proof.size)
                || entry.size != Some(proof.size)
                || entry
                    .etag
                    .as_deref()
                    .is_none_or(|tag| tag.trim_matches('"') != expected_etag.trim_matches('"'))
            {
                return Err(Error::message(format!(
                    "archive content proof does not match its exact copy identity: {}",
                    entry.key
                )));
            }
            if id == "null" || source_version == "null" {
                return Err(Error::message(
                    "archive content proof requires immutable exact object versions",
                ));
            }
            let mut next = proof.clone();
            if same_source {
                next.key = destination_key;
                next.version_id = id.clone();
                next.method = "verified-copy".to_owned();
            }
            Ok(next)
        })
        .transpose()?;
    Ok(crate::storage_format::Version {
        id,
        etag: Some(etag),
        verification,
    })
}

fn replace_cloud(
    client: Option<&crate::native_s3::S3Client>,
    value: &mut serde_yaml::Value,
    entry: &storage_metadata::PointerEntry,
    receipt: &Value,
) -> Result<()> {
    if !needs_copy_binding(entry, receipt) {
        return Ok(());
    }
    let version = copied_version(client, entry, receipt)?;
    value["cloud"]["workspace-mgr"]["version_id"] = version.id.into();
    value["cloud"]["workspace-mgr"]["etag"] = version.etag.unwrap().into();
    Ok(())
}

fn write_receipt(repo: &GitRepo, path: &str, receipt: &Value) -> Result<()> {
    let mut bytes =
        serde_json::to_vec_pretty(receipt).map_err(|error| Error::message(error.to_string()))?;
    bytes.push(b'\n');
    atomic_write(&resolved_under(&repo.root, path), &bytes)
}

fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::message("archive metadata has no parent"))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).at(parent)?;
    temp.write_all(bytes).at(path)?;
    if let Ok(metadata) = fs::metadata(path) {
        temp.as_file()
            .set_permissions(metadata.permissions())
            .at(path)?;
    }
    temp.as_file().sync_all().at(path)?;
    temp.persist(path).map_err(|error| Error::Io {
        path: path.to_path_buf(),
        source: error.error,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "20260712-120000-completed";
    const DESTINATION: &str = "2026/07/20260712-120000-completed";

    fn receipt(status: &str, objects: &[&str]) -> Value {
        json!({
            "schema_version": 1,
            "source": SOURCE,
            "destination": DESTINATION,
            "task_id": SOURCE,
            "status": status,
            "versions": objects.iter().enumerate().map(|(index, object)| json!({
                "source_object": format!("{SOURCE}/{object}"),
                "destination_object": format!("{DESTINATION}/{object}"),
                "source_version_id": format!("source-{index}"),
                "destination_version_id": format!("destination-{index}"),
                "destination_etag": format!("etag-{index}"),
                "delete_marker": false,
            })).collect::<Vec<_>>(),
        })
    }

    fn repository() -> (tempfile::TempDir, GitRepo) {
        let temp = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: temp.path().canonicalize().unwrap(),
        };
        repo.run(["init", "-b", "main"]).unwrap();
        (temp, repo)
    }

    fn write(repo: &GitRepo, path: &str, raw: &str) {
        let absolute = repo.root.join(path);
        fs::create_dir_all(absolute.parent().unwrap()).unwrap();
        fs::write(absolute, raw).unwrap();
    }

    fn verified_copy_input() -> (storage_metadata::PointerEntry, Value) {
        let proof = crate::storage_format::Verification {
            endpoint: "https://s3.example.invalid".to_owned(),
            bucket: "test-bucket".to_owned(),
            key: format!("prefix/{SOURCE}/artifact.bin"),
            version_id: "source-0".to_owned(),
            checksum: crate::storage_format::Checksum {
                algorithm: "sha256".to_owned(),
                digest: crate::hex::encode_lower(Sha256::digest(b"a")),
            },
            size: 1,
            method: "verified-upload".to_owned(),
        };
        let entry = storage_metadata::PointerEntry {
            key: format!("{DESTINATION}/artifact.bin"),
            md5: Some("0cc175b9c0f1b6a831c399e269772661".to_owned()),
            size: Some(1),
            version_id: Some("source-0".to_owned()),
            etag: Some("source-etag".to_owned()),
            verification: Some(proof),
            aggregate: false,
        };
        let mut copied = receipt("copied", &["artifact.bin"]);
        copied["bucket"] = "test-bucket".into();
        copied["remote_prefix"] = "prefix".into();
        copied["versions"][0]["source_etag"] = "source-etag".into();
        copied["versions"][0]["size"] = 1.into();
        (entry, copied)
    }

    fn copy_in_scope(
        entry: &storage_metadata::PointerEntry,
        receipt: &Value,
    ) -> Result<crate::storage_format::Version> {
        copied_version_for_scope(
            entry,
            receipt,
            Some((
                "https://s3.example.invalid".to_owned(),
                "test-bucket",
                "prefix",
            )),
        )
    }

    #[test]
    fn archive_copy_derives_and_reuses_exact_destination_content_proof() {
        let (mut entry, copied) = verified_copy_input();
        let version = copy_in_scope(&entry, &copied).unwrap();
        assert_eq!(version.id, "destination-0");
        let proof = version.verification.as_ref().unwrap();
        assert_eq!(proof.key, format!("prefix/{DESTINATION}/artifact.bin"));
        assert_eq!(proof.method, "verified-copy");
        assert_eq!(proof.version_id, "destination-0");
        assert_eq!(
            proof.checksum,
            entry.verification.as_ref().unwrap().checksum
        );
        entry.version_id = Some(version.id.clone());
        entry.etag = version.etag.clone();
        entry.verification = version.verification.clone();
        assert_eq!(copy_in_scope(&entry, &copied).unwrap(), version);
    }

    #[test]
    fn archive_copy_cannot_invent_or_rebind_content_proof() {
        let (entry, copied) = verified_copy_input();
        let mut legacy = entry.clone();
        legacy.verification = None;
        assert!(
            copied_version_for_scope(&legacy, &copied, None)
                .unwrap()
                .verification
                .is_none()
        );
        assert!(copied_version_for_scope(&entry, &copied, None).is_err());
        for field in ["endpoint", "bucket", "key"] {
            let mut changed = entry.clone();
            let proof = changed.verification.as_mut().unwrap();
            match field {
                "endpoint" => proof.endpoint = "https://another.example.invalid".to_owned(),
                "bucket" => proof.bucket = "another-bucket".to_owned(),
                "key" => proof.key = format!("prefix/{SOURCE}/another.bin"),
                _ => unreachable!(),
            }
            assert!(
                copy_in_scope(&changed, &copied).is_err(),
                "accepted {field}"
            );
        }
        let mut wrong_version = entry.clone();
        wrong_version.version_id = Some("destination-0".to_owned());
        assert!(copy_in_scope(&wrong_version, &copied).is_err());
        let mut wrong_proof_version = entry.clone();
        wrong_proof_version
            .verification
            .as_mut()
            .unwrap()
            .version_id = "another-version".to_owned();
        assert!(copy_in_scope(&wrong_proof_version, &copied).is_err());
        let mut wrong_etag = entry.clone();
        wrong_etag.etag = Some("another-etag".to_owned());
        assert!(copy_in_scope(&wrong_etag, &copied).is_err());
        for field in ["bucket", "remote_prefix", "status"] {
            let mut changed = copied.clone();
            changed[field] = "untrusted".into();
            assert!(copy_in_scope(&entry, &changed).is_err(), "accepted {field}");
        }
        let mut wrong_size = copied;
        wrong_size["versions"][0]["size"] = 2.into();
        assert!(copy_in_scope(&entry, &wrong_size).is_err());
    }

    #[test]
    fn receipts_refuse_forged_version_prefixes_and_incomplete_copy_identity() {
        let path = format!("{DESTINATION}/{RECEIPT_NAME}");
        let copied = receipt("copied", &["artifact.bin"]);
        validate(&path, &copied).unwrap();
        let mut planned = copied.clone();
        planned["status"] = "planned".into();
        planned["versions"][0]
            .as_object_mut()
            .unwrap()
            .remove("destination_version_id");
        validate(&path, &planned).unwrap();

        for (field, value) in [
            ("source_object", "another-task/artifact.bin"),
            ("destination_object", "another-task/artifact.bin"),
            (
                "destination_object",
                "2026/07/20260712-120000-completed/other.bin",
            ),
            ("source_version_id", ""),
            ("destination_version_id", ""),
        ] {
            let mut forged = copied.clone();
            forged["versions"][0][field] = value.into();
            assert!(
                validate(&path, &forged).is_err(),
                "accepted forged {field}: {value}"
            );
        }
        planned["status"] = "copied".into();
        assert!(validate(&path, &planned).is_err());
        assert!(validate(&format!("other/{RECEIPT_NAME}"), &copied).is_err());
    }

    #[test]
    fn receipt_history_preserves_opaque_s3_object_suffixes() {
        let path = format!("{DESTINATION}/{RECEIPT_NAME}");
        for object in [
            "data/a.bin ",
            "data/a\nb.bin",
            "data//a.bin",
            "legacy-folder/",
        ] {
            let copied = receipt("copied", &[object]);
            validate(&path, &copied).unwrap();
            let candidates = purge_candidates(&[copied]).unwrap();
            assert_eq!(candidates.len(), 1);
            assert_eq!(candidates[0].object, format!("{SOURCE}/{object}"));
        }
    }

    #[test]
    fn copied_receipt_requires_both_scopes_and_current_task_identity() {
        let (_temp, repo) = repository();
        repo.run(["init", "-b", "main"]).unwrap();
        repo.run(["config", "user.name", "workspace-mgr test"])
            .unwrap();
        repo.run(["config", "user.email", "test@example.invalid"])
            .unwrap();
        write(
            &repo,
            &format!("{SOURCE}/{TASK_MANIFEST_NAME}"),
            "opaque historical format with no current manifest fields\n",
        );
        repo.run(["add", "."]).unwrap();
        repo.run(["commit", "-m", "Retain the source task"])
            .unwrap();
        let base = repo
            .run(["rev-parse", "HEAD"])
            .unwrap()
            .stdout
            .trim()
            .to_owned();
        let path = format!("{DESTINATION}/{RECEIPT_NAME}");
        write(
            &repo,
            &format!("{DESTINATION}/{TASK_MANIFEST_NAME}"),
            &format!(
                "schema_version = 2\nkind = \"deliverable\"\nid = \"{SOURCE}\"\nslug = \"completed\"\npath = \"{DESTINATION}\"\nbranch = \"codex/completed\"\ntitle = \"Retained task\"\npurpose = \"Keep the task\"\nadditional_scopes = []\n"
            ),
        );
        let mut copied = receipt("copied", &[]);
        write(&repo, &path, &copied.to_string());
        let error = prepare(&repo, &Config::default(), &[DESTINATION.to_owned()], &base)
            .unwrap_err()
            .to_string();
        assert!(error.contains("both source and destination"), "{error}");
        copied["task_id"] = "another-task".into();
        write(&repo, &path, &copied.to_string());
        let scopes = [SOURCE.to_owned(), DESTINATION.to_owned()];
        let error = prepare(&repo, &Config::default(), &scopes, &base)
            .unwrap_err()
            .to_string();
        assert!(error.contains("current archived task identity"), "{error}");
        copied["task_id"] = SOURCE.into();
        write(&repo, &path, &copied.to_string());
        assert_eq!(
            prepare(&repo, &Config::default(), &scopes, &base).unwrap(),
            vec![copied]
        );
    }

    #[test]
    fn standalone_pointer_rewrite_preserves_metadata_and_can_be_retried() {
        let (_temp, repo) = repository();
        let pointer = format!("{DESTINATION}/artifact.bin.dvc");
        let raw = "meta:\n  owner: research\nouts:\n- md5: 0123456789abcdef0123456789abcdef\n  size: 19\n  hash: md5\n  path: artifact.bin\n  custom: retained\n  cloud:\n    workspace-mgr:\n      version_id: source-0\n      etag: old-etag\n      custom: retained\n    another-remote:\n      version_id: another-version\n";
        write(&repo, &pointer, raw);
        let migration = receipt("copied", &["artifact.bin"]);
        let mut expected: serde_yaml::Value = serde_yaml::from_str(raw).unwrap();
        expected["outs"][0]["cloud"]["workspace-mgr"]["version_id"] = "destination-0".into();
        expected["outs"][0]["cloud"]["workspace-mgr"]["etag"] = "etag-0".into();
        rewrite_pointer(&repo, &pointer, &migration).unwrap();
        let first = fs::read_to_string(repo.root.join(&pointer)).unwrap();
        assert_eq!(
            serde_yaml::from_str::<serde_yaml::Value>(&first).unwrap(),
            expected
        );
        rewrite_pointer(&repo, &pointer, &migration).unwrap();
        assert_eq!(fs::read_to_string(repo.root.join(&pointer)).unwrap(), first);
        assert!(
            !repo
                .root
                .join(format!("{DESTINATION}/artifact.bin"))
                .exists()
        );
    }

    #[test]
    fn directory_rewrite_is_atomic_and_preserves_files_hashes_and_aggregate() {
        let (_temp, repo) = repository();
        let pointer = format!("{DESTINATION}/data.dvc");
        let raw = "outs:\n- md5: 0123456789abcdef0123456789abcdef.dir\n  size: 31\n  nfiles: 2\n  hash: md5\n  path: data\n  cloud:\n    workspace-mgr:\n      version_id: aggregate-kept\n  files:\n  - relpath: a.bin\n    md5: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n    size: 12\n    custom: retained\n    cloud:\n      workspace-mgr:\n        version_id: destination-0\n        etag: etag-0\n  - relpath: nested/b.bin\n    md5: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n    size: 19\n    cloud:\n      workspace-mgr:\n        version_id: source-1\n        etag: old-etag\n";
        write(&repo, &pointer, raw);
        let migration = receipt("copied", &["data/a.bin", "data/nested/b.bin"]);
        let mut incomplete = migration.clone();
        incomplete["versions"].as_array_mut().unwrap().pop();
        let error = rewrite_pointer(&repo, &pointer, &incomplete)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no copied version"), "{error}");
        assert_eq!(fs::read_to_string(repo.root.join(&pointer)).unwrap(), raw);
        rewrite_pointer(&repo, &pointer, &migration).unwrap();
        let mut expected: serde_yaml::Value = serde_yaml::from_str(raw).unwrap();
        expected["outs"][0]["files"][1]["cloud"]["workspace-mgr"]["version_id"] =
            "destination-1".into();
        expected["outs"][0]["files"][1]["cloud"]["workspace-mgr"]["etag"] = "etag-1".into();
        assert_eq!(
            serde_yaml::from_str::<serde_yaml::Value>(
                &fs::read_to_string(repo.root.join(&pointer)).unwrap()
            )
            .unwrap(),
            expected
        );
    }
}
