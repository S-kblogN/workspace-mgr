use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::config::Config;
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::native_engine::Operation;
use crate::path::{allowed, reject_symlink_traversal, relative_to, repo_path, resolved_under};
use crate::process::CommandOutput;

pub const INTERNAL_REMOTE: &str = "workspace-mgr";

pub fn pointer_path(object: &str) -> String {
    format!("{object}{}", crate::storage_format::SUFFIX)
}

pub fn is_pointer(path: &str) -> bool {
    boundary_path(path).is_some()
}

pub fn boundary_path(pointer: &str) -> Option<&str> {
    pointer
        .strip_suffix(crate::storage_format::SUFFIX)
        .or_else(|| pointer.strip_suffix(".dvc"))
}

pub fn require_runtime(_repo: &GitRepo) -> Result<String> {
    Ok(format!("native Rust {}", env!("CARGO_PKG_VERSION")))
}

pub fn require_version_adapter(repo: &GitRepo) -> Result<String> {
    require_runtime(repo)
}

pub fn internal_location(repo: &GitRepo) -> Result<Option<(String, Option<String>)>> {
    if repo.root.join(".workspace-mgr.toml").is_file() {
        return Ok(Config::load_compatible(repo)?
            .s3
            .map(|remote| (remote.url, remote.endpoint_url)));
    }
    crate::legacy_dvc::remote_location(&repo.root)
}

pub fn repository_pointers(repo: &GitRepo) -> Result<Vec<String>> {
    let mut found = BTreeSet::new();
    for path in repo.visible_paths(&[])? {
        let absolute = resolved_under(&repo.root, &path);
        if is_pointer(&path) {
            let metadata = fs::symlink_metadata(&absolute).at(&absolute)?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(Error::message(format!(
                    "managed-storage metadata must be a regular file: {}",
                    absolute.display()
                )));
            }
            found.insert(path);
        }
    }
    Ok(found.into_iter().collect())
}

pub fn validate_internal_config(repo: &GitRepo, _config: &Config) -> Result<()> {
    Config::load_compatible(repo).map(|_| ())
}

pub fn ensure_ready(repo: &GitRepo, config: &Config) -> Result<()> {
    if !config.s3_enabled() {
        return Err(Error::message(
            "managed storage is not enabled in .workspace-mgr.toml",
        ));
    }
    require_runtime(repo)?;
    validate_internal_config(repo, config)?;
    if config.requires_object_versioning() {
        require_version_adapter(repo)?;
    }
    Ok(())
}

pub fn verify_object_versioning(repo: &GitRepo, config: &Config) -> Result<serde_json::Value> {
    ensure_ready(repo, config)?;
    if !config.requires_object_versioning() {
        return Ok(serde_json::json!({"mode": "not-required"}));
    }
    crate::native_versions::check_versioning(repo)
}

#[derive(Debug, Clone, Serialize)]
pub struct StorageReport {
    pub mode: String,
    pub files: Vec<String>,
    pub outputs: BTreeMap<String, Vec<String>>,
    pub dirty_files: Vec<String>,
    pub would_commit: Vec<String>,
    pub would_push: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub committed: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pushed: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<serde_json::Value>,
}

/// Storage metadata reduced to what usage accounting needs. It does not depend
/// on the metadata file's location, so it can be cached per Git blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PointerDocument {
    pub outs: Vec<PointerOutput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PointerOutput {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub md5: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<PointerFileVersion>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PointerFileVersion {
    pub relpath: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub md5: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

/// One stored object named by a metadata file: a file boundary, one file of a
/// directory boundary, or a directory recorded only by its aggregate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PointerEntry {
    pub key: String,
    pub md5: Option<String>,
    pub size: Option<u64>,
    pub version_id: Option<String>,
    pub etag: Option<String>,
    pub aggregate: bool,
}

pub(crate) fn parse_pointer_document(raw: &str, origin: &str) -> Result<PointerDocument> {
    if origin.ends_with(crate::storage_format::SUFFIX)
        || (!origin.ends_with(".dvc") && raw.trim_start().starts_with('{'))
    {
        let manifest = crate::storage_format::Manifest::parse(raw, origin)?;
        Ok(logical_document(&manifest))
    } else {
        crate::legacy_dvc::parse_document(raw, origin)
    }
}

pub(crate) fn logical_document(manifest: &crate::storage_format::Manifest) -> PointerDocument {
    PointerDocument {
        outs: vec![PointerOutput {
            path: manifest.path.clone(),
            md5: Some(if manifest.kind == crate::storage_format::Kind::Directory {
                format!("{}.dir", manifest.checksum.digest)
            } else {
                manifest.checksum.digest.clone()
            }),
            size: Some(manifest.size),
            version_id: manifest.version.as_ref().map(|version| version.id.clone()),
            etag: manifest
                .version
                .as_ref()
                .and_then(|version| version.etag.clone()),
            files: manifest.entries.as_ref().map(|entries| {
                entries
                    .iter()
                    .map(|entry| PointerFileVersion {
                        relpath: entry.path.clone(),
                        md5: Some(entry.checksum.digest.clone()),
                        size: Some(entry.size),
                        version_id: entry.version.as_ref().map(|version| version.id.clone()),
                        etag: entry
                            .version
                            .as_ref()
                            .and_then(|version| version.etag.clone()),
                    })
                    .collect()
            }),
        }],
    }
}

pub(crate) fn hash_algorithm(raw: &str, origin: &str) -> Result<String> {
    if origin.ends_with(crate::storage_format::SUFFIX)
        || (!origin.ends_with(".dvc") && raw.trim_start().starts_with('{'))
    {
        Ok(crate::storage_format::Manifest::parse(raw, origin)?
            .checksum
            .algorithm)
    } else {
        crate::legacy_dvc::hash_algorithm(raw, origin)
    }
}

pub(crate) fn read_pointer_document(repo: &GitRepo, pointer: &str) -> Result<PointerDocument> {
    reject_symlink_traversal(&repo.root, pointer, "managed-storage metadata")?;
    let pointer_path = resolved_under(&repo.root, pointer);
    let raw = fs::read_to_string(&pointer_path).at(&pointer_path)?;
    parse_pointer_document(&raw, pointer)
}

impl PointerDocument {
    /// Lists stored objects keyed by repository-relative path.
    ///
    /// Keys only label usage accounting and are never used for file access.
    /// Names the storage engine accepted, including ones with surrounding
    /// whitespace or control characters, are kept as written, so a published
    /// metadata blob can never make a measurement fail. A path that would be
    /// empty or escape its parent is charged to the enclosing boundary.
    pub(crate) fn entries(&self, pointer: &str) -> Vec<PointerEntry> {
        let parent = Path::new(pointer).parent().unwrap_or_else(|| Path::new(""));
        let metadata_boundary = boundary_path(pointer).unwrap_or(pointer);
        let mut entries = Vec::new();
        for output in &self.outs {
            let boundary = accounting_path(&output.path)
                .map(|path| {
                    if parent.as_os_str().is_empty() {
                        path
                    } else {
                        format!("{}/{path}", crate::path::to_slash(parent))
                    }
                })
                .unwrap_or_else(|| metadata_boundary.to_owned());
            match &output.files {
                Some(files) => {
                    for file in files {
                        entries.push(PointerEntry {
                            key: object_key(&boundary, &file.relpath),
                            md5: file.md5.clone(),
                            size: file.size,
                            version_id: file.version_id.clone(),
                            etag: file.etag.clone(),
                            aggregate: false,
                        });
                    }
                }
                None => entries.push(PointerEntry {
                    key: boundary,
                    md5: output.md5.clone(),
                    size: output.size,
                    version_id: output.version_id.clone(),
                    etag: output.etag.clone(),
                    aggregate: output
                        .md5
                        .as_deref()
                        .is_some_and(|md5| md5.ends_with(".dir")),
                }),
            }
        }
        entries
    }
}

/// Accounting key of a file that a directory boundary lists by `relpath`.
pub(crate) fn object_key(boundary: &str, relpath: &str) -> String {
    match accounting_path(relpath) {
        Some(relative) => format!("{boundary}/{relative}"),
        None => boundary.to_owned(),
    }
}

/// Drops empty and `.` components without judging the names themselves. It
/// separates on `/` alone, exactly like [`crate::path::repo_path`], so a
/// backslash stays an ordinary name character and keys match worktree paths
/// and object paths. Unlike `repo_path` it never fails: an empty, absolute,
/// or escaping path yields `None`, so no published metadata can make a
/// measurement fail.
fn accounting_path(raw: &str) -> Option<String> {
    if raw.starts_with('/') {
        return None;
    }
    let mut parts = Vec::new();
    for part in raw.split('/') {
        match part {
            "" | "." => {}
            ".." => return None,
            name => parts.push(name),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// File-level differences between metadata and the worktree, as reported by
/// the storage engine. Paths are repository-relative with `/` separators and
/// are kept exactly as reported, because a backslash is an ordinary name
/// character on Linux and macOS; directory rows keep a trailing `/`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub(crate) struct DataStatus {
    #[serde(default)]
    pub not_in_cache: Vec<String>,
    #[serde(default)]
    pub uncommitted: DataChanges,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub(crate) struct DataChanges {
    #[serde(default)]
    pub added: Vec<String>,
    #[serde(default)]
    pub modified: Vec<String>,
    #[serde(default)]
    pub deleted: Vec<String>,
    #[serde(default)]
    pub renamed: Vec<DataRename>,
    #[serde(default)]
    pub unknown: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct DataRename {
    pub old: String,
    pub new: String,
}

/// Runs one granular data-status pass over the given output paths.
///
/// Targets must be output paths: metadata file paths are accepted by the
/// engine but silently match nothing.
pub(crate) fn data_status(repo: &GitRepo, outputs: &[String]) -> Result<DataStatus> {
    if outputs.is_empty() {
        return Ok(DataStatus::default());
    }
    let output = inspect_engine(
        &repo.root,
        &Operation::Changes {
            outputs: outputs.to_vec(),
        },
    )?;
    if !output.success() {
        return Err(Error::message(format!(
            "managed-storage data status failed: {}",
            private_detail(&output)
        )));
    }
    parse_data_status(&output.stdout)
}

pub(crate) fn parse_data_status(raw: &str) -> Result<DataStatus> {
    serde_json::from_str(raw.trim().if_empty("{}")).map_err(|error| {
        Error::message(format!(
            "managed-storage data status returned invalid data: {error}"
        ))
    })
}

/// One file of a directory version, as the version's manifest lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListedFile {
    pub relpath: String,
    pub md5: String,
    pub size: u64,
}

/// Locally available native and historical objects used for usage accounting.
pub(crate) fn local_object_stores(repo: &GitRepo, config: &Config) -> Vec<PathBuf> {
    let mut stores = Vec::new();
    if let Ok(cache) = crate::native_engine::cache_root(repo) {
        stores.push(cache);
    }
    stores.push(crate::legacy_dvc::cache_dir(&repo.root.join(".dvc")));
    if let Some(s3) = config.s3.as_ref().filter(|s3| !s3.url.contains("://")) {
        for store in [
            repo.root.join(&s3.url),
            repo.root.join(".dvc").join(&s3.url),
        ] {
            if !stores.contains(&store) {
                stores.push(store);
            }
        }
    }
    stores
}

pub(crate) fn directory_listing(stores: &[PathBuf], digest: &str) -> Option<Vec<ListedFile>> {
    crate::legacy_dvc::directory_listing(stores, digest)
}

pub fn discover(repo: &GitRepo, scopes: &[String]) -> Result<Vec<String>> {
    let mut found = BTreeSet::new();
    let mut discovery_scopes = scopes.to_vec();
    for scope in scopes {
        if !is_pointer(scope) {
            discovery_scopes.push(pointer_path(scope));
            discovery_scopes.push(format!("{scope}.dvc"));
        }
    }
    for path in repo.visible_paths(&discovery_scopes)? {
        let absolute = resolved_under(&repo.root, &path);
        if !is_pointer(&path) {
            continue;
        }
        if crate::storage::is_local(repo, &path)? {
            continue;
        }
        let boundary = boundary_path(&path).expect("metadata suffix checked above");
        if crate::storage::is_local(repo, boundary)? {
            continue;
        }
        let metadata = fs::symlink_metadata(&absolute).at(&absolute)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(Error::message(format!(
                "managed-storage metadata must be a regular file: {}",
                absolute.display()
            )));
        }
        found.insert(path);
    }
    Ok(found.into_iter().collect())
}

pub fn output_paths(repo: &GitRepo, pointers: &[String]) -> Result<BTreeMap<String, Vec<String>>> {
    let mut result = BTreeMap::new();
    for pointer in pointers {
        reject_symlink_traversal(&repo.root, pointer, "managed-storage metadata")?;
        let pointer_path = resolved_under(&repo.root, pointer);
        let raw = fs::read_to_string(&pointer_path).at(&pointer_path)?;
        result.insert(pointer.clone(), vec![metadata_output(repo, pointer, &raw)?]);
    }
    Ok(result)
}

/// The one output the metadata `raw` of `pointer` defines, which must be the
/// boundary the metadata file is named after. It reads no file and runs no
/// engine command, so it also validates metadata taken from a Git revision.
pub fn metadata_output(repo: &GitRepo, pointer: &str, raw: &str) -> Result<String> {
    let parsed = parse_pointer_document(raw, pointer)?;
    let [output] = parsed.outs.as_slice() else {
        return Err(Error::message(format!(
            "managed-storage metadata must define exactly one output: {pointer}"
        )));
    };
    let parent = Path::new(pointer).parent().unwrap_or_else(|| Path::new(""));
    let raw = if parent.as_os_str().is_empty() {
        output.path.clone()
    } else {
        format!("{}/{}", crate::path::to_slash(parent), output.path)
    };
    let output = repo_path(&raw, "managed-storage output")?;
    let expected = boundary_path(pointer)
        .ok_or_else(|| Error::message(format!("invalid metadata path: {pointer}")))?;
    if output != expected {
        return Err(Error::message(format!(
            "managed-storage output {output:?} must match metadata boundary {expected:?}"
        )));
    }
    reject_symlink_traversal(&repo.root, &output, "managed-storage output")?;
    Ok(output)
}

/// Preserve the established managed-storage path convention across the native
/// migration. Older engines interpreted backslashes inconsistently. Refresh
/// reports legacy boundaries; move reads their literal metadata and recovers
/// them under an addressable name.
pub fn is_addressable(path: &str) -> bool {
    !path.contains('\\')
}

pub fn require_addressable(path: &str, field: &str, remedy: &str) -> Result<()> {
    if !is_addressable(path) {
        return Err(Error::message(format!(
            "{field} {path:?} contains a backslash, which is unsupported in managed-storage boundary names; {remedy}"
        )));
    }
    Ok(())
}

pub fn require_addressable_metadata(pointers: &[String]) -> Result<()> {
    for pointer in pointers {
        require_addressable(
            pointer,
            "managed-storage metadata",
            "rename its output to a path without backslashes with `workspace-mgr move`",
        )?;
    }
    Ok(())
}

pub fn status(repo: &GitRepo, pointer: &str) -> Result<serde_json::Value> {
    let output = inspect_engine(
        &repo.root,
        &Operation::Status {
            pointers: vec![pointer.to_owned()],
            cloud: false,
            quiet: false,
        },
    )?;
    if !output.success() {
        return Err(Error::message(format!(
            "managed-storage status failed for {pointer}: {}",
            private_detail(&output)
        )));
    }
    serde_json::from_str(output.stdout.trim().if_empty("{}")).map_err(|error| {
        Error::message(format!(
            "managed-storage status returned invalid data: {error}"
        ))
    })
}

/// Checks every output against its metadata and, unless `dry_run`, commits
/// changed outputs to the local cache. Nothing is uploaded until
/// [`push_outputs`], so a caller can re-check the committed metadata first.
pub fn reconcile(
    repo: &GitRepo,
    config: &Config,
    pointers: &[String],
    dry_run: bool,
) -> Result<StorageReport> {
    if config.s3_enabled() || !pointers.is_empty() {
        ensure_ready(repo, config)?;
    }
    if !pointers.is_empty() && config.requires_object_versioning() {
        verify_object_versioning(repo, config)?;
    }
    let outputs = output_paths(repo, pointers)?;
    let mut dirty = Vec::new();
    for pointer in pointers {
        let value = status(repo, pointer)?;
        let is_dirty = value
            .as_object()
            .map(|object| !object.is_empty())
            .unwrap_or(true);
        if is_dirty {
            dirty.push(pointer.clone());
        }
    }
    let missing: Vec<String> = dirty
        .iter()
        .flat_map(|pointer| outputs.get(pointer).into_iter().flatten())
        .filter(|path| !resolved_under(&repo.root, path).exists())
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(Error::message(format!(
            "managed-storage outputs are missing locally and will not be interpreted as deletions: {}; hydrate them before publishing",
            missing.join(", ")
        )));
    }
    let mut report = StorageReport {
        mode: if dry_run { "plan" } else { "publish" }.to_owned(),
        files: pointers.to_vec(),
        outputs,
        dirty_files: dirty.clone(),
        would_commit: dirty.clone(),
        would_push: pointers.to_vec(),
        committed: Vec::new(),
        pushed: Vec::new(),
        verification: None,
    };
    if dry_run {
        return Ok(report);
    }
    for pointer in &dirty {
        execute_engine(
            &repo.root,
            &Operation::Record {
                pointers: vec![pointer.to_owned()],
            },
        )?;
    }
    report.committed = dirty;
    Ok(report)
}

/// Uploads every output that [`reconcile`] prepared and verifies the result.
pub fn push_outputs(repo: &GitRepo, config: &Config, report: &mut StorageReport) -> Result<()> {
    if !report.files.is_empty() {
        execute_engine(
            &repo.root,
            &Operation::Upload {
                pointers: report.files.clone(),
            },
        )?;
        report.pushed = report.files.clone();
        report.verification = Some(verify(repo, config, &report.files)?);
    }
    Ok(())
}

pub fn verify(repo: &GitRepo, config: &Config, pointers: &[String]) -> Result<serde_json::Value> {
    if pointers.is_empty() {
        return Ok(serde_json::json!({"mode": "no-files"}));
    }
    let cas = crate::native_engine::historical_cas_pointers(repo, pointers)?;
    if !cas.is_empty() {
        ensure_ready(repo, config)?;
        let rest = pointers
            .iter()
            .filter(|pointer| !cas.contains(pointer))
            .cloned()
            .collect::<Vec<_>>();
        let legacy = crate::native_engine::fetch_historical_cas(repo, &cas)?
            .ok_or_else(|| Error::message("historical CAS control changed during verification"))?;
        verify_local(repo, &cas)?;
        if rest.is_empty() {
            return Ok(legacy);
        }
        let other = verify(repo, config, &rest)?;
        return Ok(serde_json::json!({"mode":"mixed-storage","sources":[legacy,other]}));
    }
    verify_local(repo, pointers)?;
    ensure_ready(repo, config)?;
    if config.requires_object_versioning() {
        return version_read_adapter(repo, pointers, "--verify");
    }

    let cloud = inspect_engine(
        &repo.root,
        &Operation::Status {
            pointers: pointers.to_vec(),
            cloud: true,
            quiet: true,
        },
    )?;
    if !cloud.success() {
        return Err(Error::message(format!(
            "stored content is missing from the configured remote for: {}",
            pointers.join(", ")
        )));
    }
    Ok(serde_json::json!({"mode": "remote-status"}))
}

/// Verify materialized bytes after a fetch that already checked exact remote
/// versions. This does not reuse remote evidence across separate commands.
pub fn verify_local(repo: &GitRepo, pointers: &[String]) -> Result<()> {
    if pointers.is_empty() {
        return Ok(());
    }
    let local = inspect_engine(
        &repo.root,
        &Operation::Status {
            pointers: pointers.to_vec(),
            cloud: false,
            quiet: true,
        },
    )?;
    if !local.success() {
        return Err(Error::message(format!(
            "managed-storage metadata does not match local data for: {}",
            pointers.join(", ")
        )));
    }

    Ok(())
}

fn version_read_adapter(
    repo: &GitRepo,
    pointers: &[String],
    operation: &str,
) -> Result<serde_json::Value> {
    let receipts = crate::archive_migration::receipts(repo, &[])?
        .into_iter()
        .map(|(_, receipt)| receipt)
        .filter(|receipt| receipt["status"] == "planned")
        .collect::<Vec<_>>();
    crate::native_versions::read(repo, pointers, operation, &receipts)
}

/// Populate the local cache. Versioned S3 reads validate GET metadata and local
/// content hashes, and batch-check remote versions for valid cache hits.
pub fn fetch(
    repo: &GitRepo,
    config: &Config,
    pointers: &[String],
) -> Result<Option<serde_json::Value>> {
    let cas = crate::native_engine::historical_cas_pointers(repo, pointers)?;
    if !cas.is_empty() {
        let rest = pointers
            .iter()
            .filter(|pointer| !cas.contains(pointer))
            .cloned()
            .collect::<Vec<_>>();
        let legacy = crate::native_engine::fetch_historical_cas(repo, &cas)?
            .ok_or_else(|| Error::message("historical CAS control changed during fetch"))?;
        if rest.is_empty() {
            return Ok(Some(legacy));
        }
        let other =
            fetch(repo, config, &rest)?.unwrap_or(serde_json::json!({"mode":"remote-status"}));
        return Ok(Some(
            serde_json::json!({"mode":"mixed-storage","sources":[legacy,other]}),
        ));
    }
    if config.requires_object_versioning() {
        return version_read_adapter(repo, pointers, "--fetch").map(Some);
    }
    execute_engine(
        &repo.root,
        &Operation::Fetch {
            pointers: pointers.to_vec(),
        },
    )?;
    Ok(None)
}

pub fn version_purge_adapter(
    repo: &GitRepo,
    operation: &str,
    payload: &serde_json::Value,
) -> Result<serde_json::Value> {
    let mut coordination = Vec::new();
    if operation == "delete" {
        let mut sources = BTreeSet::new();
        let candidates = payload.get("candidates").unwrap_or(payload);
        for candidate in candidates.as_array().into_iter().flatten() {
            if let Some(source) = candidate["pointer"]
                .as_str()
                .and_then(|pointer| pointer.strip_suffix("/.workspace-mgr-archive.json"))
            {
                sources.insert(source.to_owned());
            } else if let Some(object) = candidate["object"].as_str() {
                let parts = object.split('/').collect::<Vec<_>>();
                for length in 1..parts.len() {
                    sources.insert(parts[..length].join("/"));
                }
            }
        }
        for receipt in payload["prefixes"].as_array().into_iter().flatten() {
            if let Some(source) = receipt["source"].as_str() {
                sources.insert(source.to_owned());
            }
        }
        for source in sources {
            let inspected =
                archive_registry_adapter(repo, "inspect", &serde_json::json!({"source":source}))?;
            let Some(receipt) = inspected.get("receipt").filter(|value| value.is_object()) else {
                continue;
            };
            coordination.push(serde_json::json!({"receipt":receipt,"coordination":crate::archive_registry::coordinate_published(repo, receipt)?}));
        }
    }
    let mut request = if payload.is_object() || coordination.is_empty() {
        payload.clone()
    } else {
        serde_json::json!({"candidates":payload})
    };
    if !coordination.is_empty() {
        request["coordination"] = coordination.into();
    }
    // Purge journals can exceed execve's argument-size limit; keep the full
    // request in-process instead of handing serialized JSON to a subprocess.
    crate::native_versions::purge(repo, operation, &request)
}

pub(crate) fn version_archive_adapter(
    repo: &GitRepo,
    operation: &str,
    payload: &serde_json::Value,
) -> Result<serde_json::Value> {
    crate::native_archive::execute(repo, operation, payload)
}

pub(crate) fn archive_registry_adapter(
    repo: &GitRepo,
    operation: &str,
    payload: &serde_json::Value,
) -> Result<serde_json::Value> {
    let coordinated = if operation == "publish" {
        serde_json::json!({"receipt":payload,"coordination":crate::archive_registry::coordinate(repo, payload, true)?})
    } else {
        payload.clone()
    };
    crate::native_archive::registry(repo, operation, &coordinated)
}

pub(crate) fn verify_archived(repo: &GitRepo, pointers: &[String]) -> Result<serde_json::Value> {
    if pointers.is_empty() {
        return Ok(serde_json::json!({"mode":"no-files"}));
    }
    version_read_adapter(repo, pointers, "--verify")
}

pub fn hydrate(
    repo: &GitRepo,
    config: &Config,
    scopes: &[String],
    targets: &[String],
    dry_run: bool,
) -> Result<HydrateReport> {
    ensure_ready(repo, config)?;
    let discovered = discover(repo, scopes)?;
    let pointers = if targets.is_empty() {
        discovered
    } else {
        let mut targets = targets
            .iter()
            .map(|path| repo_path(path, "hydrate target"))
            .collect::<Result<Vec<_>>>()?;
        targets.sort();
        targets.dedup();
        for target in &targets {
            if !is_pointer(target) {
                return Err(Error::message(format!(
                    "hydrate target is not a managed-storage metadata file: {target}"
                )));
            }
            if !allowed(target, scopes)
                && !boundary_path(target).is_some_and(|object| allowed(object, scopes))
            {
                return Err(Error::message(format!(
                    "hydrate target escapes the declared scope: {target}"
                )));
            }
            if !resolved_under(&repo.root, target).is_file() {
                return Err(Error::message(format!(
                    "hydrate target does not exist: {target}"
                )));
            }
        }
        targets
    };
    require_addressable_metadata(&pointers)?;
    if config.requires_object_versioning()
        && !crate::native_engine::is_historical_cas_read(repo, &pointers)?
    {
        verify_object_versioning(repo, config)?;
    }
    let outputs = output_paths(repo, &pointers)?;
    let mut report = HydrateReport {
        status: if dry_run { "dry_run" } else { "pending" }.to_owned(),
        scopes: scopes.to_vec(),
        metadata_files: pointers.clone(),
        outputs,
        verification: None,
    };
    if pointers.is_empty() {
        report.status = "no_changes".to_owned();
        return Ok(report);
    }
    if dry_run {
        return Ok(report);
    }
    let remote_verification = fetch(repo, config, &pointers)?;
    // Cleared caches need fetching before comparison with materialized data.
    // Fetching first restores the comparison object without touching
    // the worktree, allowing the conflict check to distinguish identical
    // content from a genuine local modification.
    validate_worktree(repo, config, &pointers)?;
    execute_engine(
        &repo.root,
        &Operation::Materialize {
            pointers: pointers.clone(),
        },
    )?;
    report.verification = Some(if let Some(verification) = remote_verification {
        verify_local(repo, &pointers)?;
        verification
    } else {
        verify(repo, config, &pointers)?
    });
    report.status = "hydrated".to_owned();
    Ok(report)
}

#[derive(Debug, Clone, Serialize)]
pub struct HydrateReport {
    pub status: String,
    pub scopes: Vec<String>,
    pub metadata_files: Vec<String>,
    pub outputs: BTreeMap<String, Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<serde_json::Value>,
}

pub fn validate_worktree(repo: &GitRepo, config: &Config, pointers: &[String]) -> Result<()> {
    ensure_ready(repo, config)?;
    let mut conflicts = Vec::new();
    for pointer in pointers {
        if !resolved_under(&repo.root, pointer).is_file() {
            continue;
        }
        let dirty = status(repo, pointer)?
            .as_object()
            .map(|object| !object.is_empty())
            .unwrap_or(true);
        if !dirty {
            continue;
        }
        let outputs = output_paths(repo, std::slice::from_ref(pointer))?;
        let has_output = outputs
            .get(pointer)
            .into_iter()
            .flatten()
            .any(|output| resolved_under(&repo.root, output).exists());
        if has_output && !pointer_matches_worktree(repo, pointer)? {
            conflicts.push(pointer.clone());
        }
    }
    if !conflicts.is_empty() {
        return Err(Error::message(format!(
            "managed-storage metadata would overwrite locally changed outputs: {}; publish or preserve those changes first",
            conflicts.join(", ")
        )));
    }
    Ok(())
}

fn pointer_matches_worktree(repo: &GitRepo, pointer: &str) -> Result<bool> {
    reject_symlink_traversal(&repo.root, pointer, "managed-storage metadata")?;
    let pointer_path = resolved_under(&repo.root, pointer);
    let raw = fs::read_to_string(&pointer_path).at(&pointer_path)?;
    payload_matches_metadata(repo, pointer, &raw)
}

/// Whether the payload at `pointer`'s boundary matches the metadata `raw` byte
/// for byte, decided without the storage engine, which cannot address every
/// path. A directory whose metadata does not list its files cannot be compared
/// this way, so it never matches: callers treat a mismatch as a conflict, never
/// as permission to overwrite.
pub fn payload_matches_metadata(repo: &GitRepo, pointer: &str, raw: &str) -> Result<bool> {
    let parsed = parse_pointer_document(raw, pointer)?;
    let algorithm = hash_algorithm(raw, pointer)?;
    if parsed.outs.len() != 1 {
        return Err(Error::message(format!(
            "managed-storage metadata must define exactly one output: {pointer}"
        )));
    }
    let output = &parsed.outs[0];
    let boundary = boundary_path(pointer)
        .ok_or_else(|| Error::message(format!("invalid metadata path: {pointer}")))?;
    let boundary_path = resolved_under(&repo.root, boundary);
    if boundary_path.is_symlink() {
        return Ok(false);
    }
    if algorithm == "md5-dos2unix" {
        let entries = match &output.files {
            Some(files) => files
                .iter()
                .map(|file| crate::native_engine::StorageEntry {
                    pointer: pointer.into(),
                    object: format!("{boundary}/{}", file.relpath),
                    md5: file.md5.clone(),
                    size: file.size,
                    version_id: file.version_id.clone(),
                    etag: file.etag.clone(),
                    hash_name: algorithm.clone(),
                })
                .collect::<Vec<_>>(),
            None => vec![crate::native_engine::StorageEntry {
                pointer: pointer.into(),
                object: boundary.into(),
                md5: output.md5.clone(),
                size: output.size,
                version_id: output.version_id.clone(),
                etag: output.etag.clone(),
                hash_name: algorithm.clone(),
            }],
        };
        for entry in entries {
            if !crate::native_engine::exact_raw_bytes_match(
                repo,
                &entry,
                &repo.root.join(&entry.object),
            )? {
                return Ok(false);
            }
        }
    }
    match &output.files {
        None => {
            if !boundary_path.is_file() {
                return Ok(false);
            }
            let Some(expected_md5) = &output.md5 else {
                return Err(Error::message(format!(
                    "managed-storage metadata lacks a file digest: {pointer}"
                )));
            };
            if output.size != Some(fs::metadata(&boundary_path).at(&boundary_path)?.len()) {
                return Ok(false);
            }
            Ok(crate::native_engine::file_digest(&boundary_path, &algorithm)? == *expected_md5)
        }
        Some(files) => {
            if !boundary_path.is_dir() {
                return Ok(false);
            }
            let mut expected = BTreeMap::new();
            for file in files {
                let relative = repo_path(&file.relpath, "managed-storage directory entry")?;
                if expected
                    .insert(
                        relative.clone(),
                        (
                            file.md5
                                .as_ref()
                                .ok_or_else(|| Error::message("directory entry has no checksum"))?,
                            file.size.ok_or_else(|| {
                                Error::message("directory entry has no physical size")
                            })?,
                        ),
                    )
                    .is_some()
                {
                    return Err(Error::message(format!(
                        "managed-storage metadata repeats directory entry {relative:?}: {pointer}"
                    )));
                }
            }
            let mut actual = BTreeSet::new();
            for entry in WalkDir::new(&boundary_path).follow_links(false) {
                let entry = entry.map_err(|error| {
                    Error::message(format!(
                        "failed to inspect managed-storage output {}: {error}",
                        boundary_path.display()
                    ))
                })?;
                if entry.path() == boundary_path {
                    continue;
                }
                if entry.file_type().is_symlink() {
                    return Ok(false);
                }
                if !entry.file_type().is_file() {
                    continue;
                }
                let relative = relative_to(
                    entry.path(),
                    &boundary_path,
                    "managed-storage directory entry",
                )?;
                let Some((expected_md5, expected_size)) = expected.get(&relative) else {
                    return Ok(false);
                };
                if fs::metadata(entry.path()).at(entry.path())?.len() != *expected_size
                    || crate::native_engine::file_digest(entry.path(), &algorithm)?
                        != **expected_md5
                {
                    return Ok(false);
                }
                actual.insert(relative);
            }
            Ok(actual.len() == expected.len())
        }
    }
}

pub fn management(
    repo: &GitRepo,
    config: &Config,
    operation: &str,
    paths: &[String],
    dry_run: bool,
) -> Result<serde_json::Value> {
    ensure_ready(repo, config)?;
    match (operation, paths) {
        ("track", paths) => {
            for path in paths {
                require_addressable(path, "S3 storage path", "rename it before placing it in S3")?;
            }
        }
        ("move", [_, destination]) => require_addressable(
            destination,
            "managed-storage move destination",
            "choose a path without backslashes",
        )?,
        // Removal would leave the engine's ignore rule behind and hide the
        // payload from Git.
        ("untrack", pointers) => require_addressable_metadata(pointers)?,
        _ => {}
    }
    if !dry_run {
        let native = match (operation, paths) {
            ("track", paths) => Operation::Track {
                paths: paths.to_vec(),
            },
            ("move", [source, destination]) => Operation::Move {
                source: source.clone(),
                destination: destination.clone(),
            },
            ("untrack", pointers) => Operation::Untrack {
                pointers: pointers.to_vec(),
            },
            _ => {
                return Err(Error::message(format!(
                    "invalid managed-storage operation {operation}"
                )));
            }
        };
        execute_engine(&repo.root, &native)?;
        if operation == "move" {
            reset_moved_cloud_metadata(repo, &paths[1])?;
        }
    }
    Ok(serde_json::json!({
        "operation": operation,
        "paths": paths,
        "mode": if dry_run { "plan" } else { "apply" },
    }))
}

fn reset_moved_cloud_metadata(repo: &GitRepo, output: &str) -> Result<()> {
    reset_moved_pointer_cloud_metadata(repo, &pointer_path(output)).map(|_| ())
}

pub(crate) fn reset_moved_pointer_cloud_metadata(repo: &GitRepo, pointer: &str) -> Result<bool> {
    if !pointer.ends_with(crate::storage_format::SUFFIX) {
        return Err(Error::message(
            "legacy DVC metadata must be migrated before mutation; run `workspace-mgr manage`",
        ));
    }
    reject_symlink_traversal(&repo.root, pointer, "moved storage metadata")?;
    let path = resolved_under(&repo.root, pointer);
    let raw = fs::read_to_string(&path).at(&path)?;
    let mut manifest = crate::storage_format::Manifest::parse(&raw, pointer)?;
    if !manifest.clear_versions() {
        return Ok(false);
    }
    let rendered = manifest.serialize()?;
    let parent = path
        .parent()
        .ok_or_else(|| Error::message("storage metadata has no parent"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).at(parent)?;
    temporary.write_all(rendered.as_bytes()).at(&path)?;
    temporary.flush().at(&path)?;
    temporary.persist(&path).map_err(|error| Error::Io {
        path,
        source: error.error,
    })?;
    Ok(true)
}

pub fn prepare_revision(
    repo: &GitRepo,
    config: &Config,
    oid: &str,
    pointers: &[String],
) -> Result<PreparedRevision> {
    if pointers.is_empty() {
        return Ok(PreparedRevision {
            prepared_files: Vec::new(),
            outputs: BTreeMap::new(),
            mode: "no_changes".to_owned(),
        });
    }
    let container = tempfile::tempdir().map_err(|source| Error::Io {
        path: std::env::temp_dir(),
        source,
    })?;
    let checkout = container.path().join("checkout");
    repo.run([
        "worktree",
        "add",
        "--quiet",
        "--detach",
        &checkout.to_string_lossy(),
        oid,
    ])?;
    let result = (|| {
        let checkout_repo = GitRepo {
            root: checkout.clone(),
        };
        link_private_worktree_state(repo, &checkout_repo)?;
        let revision_config = checkout_repo.root.join(".workspace-mgr.toml");
        let legacy_revision = !revision_config.exists()
            && !revision_config.is_symlink()
            && pointers.iter().all(|pointer| pointer.ends_with(".dvc"));
        let content_addressed =
            crate::native_engine::is_historical_cas_read(&checkout_repo, pointers)?;
        if legacy_revision {
            require_runtime(&checkout_repo)?;
            if config.requires_object_versioning() && !content_addressed {
                crate::native_versions::check_versioning(&checkout_repo)?;
            } else if crate::legacy_dvc::remote_location(&checkout_repo.root)?.is_none() {
                return Err(Error::message(
                    "historical storage metadata has no configured legacy remote",
                ));
            }
        } else {
            ensure_ready(&checkout_repo, config)?;
            if config.requires_object_versioning() && !content_addressed {
                verify_object_versioning(&checkout_repo, config)?;
            }
        }
        let outputs = output_paths(&checkout_repo, pointers)?;
        fetch(&checkout_repo, config, pointers).map_err(|source| prefetch_error(oid, source))?;
        Ok(PreparedRevision {
            prepared_files: pointers.to_vec(),
            outputs,
            mode: "fetched_to_shared_cache".to_owned(),
        })
    })();
    let cleanup = cleanup_preparation_worktree(repo, &checkout, container);
    match (result, cleanup) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(source), Ok(())) => Err(source),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
        (Err(source), Err(cleanup_error)) => Err(Error::message(format!(
            "{source}; temporary worktree cleanup also failed: {cleanup_error}"
        ))),
    }
}

pub fn link_private_worktree_state(source: &GitRepo, checkout: &GitRepo) -> Result<()> {
    let shared_cache = crate::native_engine::cache_root(source)?;
    fs::create_dir_all(&shared_cache).at(&shared_cache)?;
    let checkout_cache = crate::native_engine::cache_root(checkout)?;
    if shared_cache != checkout_cache {
        fs::create_dir_all(checkout_cache.parent().expect("cache parent")).at(&checkout_cache)?;
        if !checkout_cache.exists() {
            symlink_dir(&shared_cache, &checkout_cache)?;
        }
    }
    let shared_local = crate::native_s3::credentials_path(source)?;
    let checkout_local = crate::native_s3::credentials_path(checkout)?;
    if shared_local != checkout_local && shared_local.is_file() && !checkout_local.exists() {
        fs::create_dir_all(checkout_local.parent().expect("credentials parent"))
            .at(&checkout_local)?;
        fs::copy(&shared_local, &checkout_local).at(&checkout_local)?;
    }
    Ok(())
}

fn prefetch_error(oid: &str, source: Error) -> Error {
    let detail = match source {
        Error::Command { detail, .. } => detail
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("storage engine command failed")
            .trim()
            .to_owned(),
        other => other.to_string(),
    };
    Error::message(format!(
        "managed-storage prefetch for repository revision {oid} failed; check object-read credentials and provider download or read-transaction caps. Provider detail: {detail}"
    ))
}

fn cleanup_preparation_worktree(
    repo: &GitRepo,
    checkout: &Path,
    container: tempfile::TempDir,
) -> Result<()> {
    let removal = repo.run_unchecked([
        "worktree",
        "remove",
        "--force",
        "--force",
        &checkout.to_string_lossy(),
    ]);
    let removal_detail = match removal {
        Ok(output) if output.success() => None,
        Ok(output) => Some(output.stderr.trim().to_owned()),
        Err(error) => Some(error.to_string()),
    };
    let close_error = container.close().err().map(|error| error.to_string());
    let prune = repo.run_unchecked(["worktree", "prune"]);
    let prune_error = match prune {
        Ok(output) if output.success() => None,
        Ok(output) => Some(output.stderr.trim().to_owned()),
        Err(error) => Some(error.to_string()),
    };
    let listing = repo.run_unchecked(["worktree", "list", "--porcelain"]);
    let marker = format!("worktree {}", checkout.display());
    let (registered, listing_error) = match listing {
        Ok(output) if output.success() => (output.stdout.lines().any(|line| line == marker), None),
        Ok(output) => (false, Some(output.stderr.trim().to_owned())),
        Err(error) => (false, Some(error.to_string())),
    };
    let checkout_exists = match fs::symlink_metadata(checkout) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(Error::message(format!(
                "could not verify temporary worktree cleanup at {}: {error}",
                checkout.display()
            )));
        }
    };
    if !registered && !checkout_exists && close_error.is_none() && prune_error.is_none() {
        return Ok(());
    }

    let mut details = Vec::new();
    if let Some(detail) = removal_detail.filter(|_| registered || checkout_exists) {
        details.push(format!("Git removal: {detail}"));
    }
    if let Some(detail) = close_error {
        details.push(format!("filesystem removal: {detail}"));
    }
    if let Some(detail) = prune_error {
        details.push(format!("Git prune: {detail}"));
    }
    if let Some(detail) = listing_error {
        details.push(format!("Git verification: {detail}"));
    }
    if registered {
        details.push("worktree remains registered".to_owned());
    }
    if checkout_exists {
        details.push("worktree directory remains on disk".to_owned());
    }
    Err(Error::message(format!(
        "failed to clean temporary worktree {}: {}",
        checkout.display(),
        details.join("; ")
    )))
}

#[derive(Debug, Clone, Serialize)]
pub struct PreparedRevision {
    pub prepared_files: Vec<String>,
    pub outputs: BTreeMap<String, Vec<String>>,
    pub mode: String,
}

#[cfg(unix)]
fn symlink_dir(source: &Path, target: &Path) -> Result<()> {
    std::os::unix::fs::symlink(source, target).at(target)
}

#[cfg(windows)]
fn symlink_dir(source: &Path, target: &Path) -> Result<()> {
    std::os::windows::fs::symlink_dir(source, target).at(target)
}

trait EmptyFallback {
    fn if_empty<'a>(&'a self, fallback: &'a str) -> &'a str;
}

impl EmptyFallback for str {
    fn if_empty<'a>(&'a self, fallback: &'a str) -> &'a str {
        if self.is_empty() { fallback } else { self }
    }
}

pub fn execute_engine(cwd: &Path, operation: &Operation) -> Result<CommandOutput> {
    let output = inspect_engine(cwd, operation)?;
    if !output.success() {
        return Err(Error::Command {
            command: "managed-storage".to_owned(),
            code: output.code,
            detail: private_detail(&output),
        });
    }
    Ok(output)
}

fn inspect_engine(cwd: &Path, operation: &Operation) -> Result<CommandOutput> {
    crate::native_engine::execute(cwd, operation)
}

fn private_detail(output: &CommandOutput) -> String {
    let detail = if output.stderr.trim().is_empty() {
        &output.stdout
    } else {
        &output.stderr
    };
    if detail.trim().is_empty() {
        "native storage reported a failure".to_owned()
    } else {
        detail.trim().to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_normalized_hash_verifies_crlf_payload_without_losing_change_detection() {
        let temp = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: temp.path().to_owned(),
        };
        for pointer in [
            "outs:\n- path: data.bin\n  size: 7\n  hash: md5-dos2unix\n  md5: b1946ac92492d2347c6235b4d2611184\n",
            "outs:\n- path: data.bin\n  size: 7\n  md5: b1946ac92492d2347c6235b4d2611184\n",
        ] {
            fs::write(temp.path().join("data.bin"), b"hello\r\n").unwrap();
            assert!(payload_matches_metadata(&repo, "data.bin.dvc", pointer).unwrap());
            fs::write(temp.path().join("data.bin"), b"HELLO\r\n").unwrap();
            assert!(!payload_matches_metadata(&repo, "data.bin.dvc", pointer).unwrap());
        }
    }

    #[test]
    fn prefetch_errors_keep_native_provider_context() {
        let error = prefetch_error(
            "deadbeef",
            Error::message("S3 HeadObject returned 403 AccessDenied"),
        )
        .to_string();
        assert!(error.contains("download or read-transaction caps"));
        assert!(error.contains("S3 HeadObject returned 403 AccessDenied"));
    }

    #[test]
    fn temporary_preparation_worktree_is_removed_and_unregistered() {
        let repository = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: repository.path().to_path_buf(),
        };
        repo.run(["init", "-b", "main"]).unwrap();
        repo.run(["config", "user.name", "workspace-mgr test"])
            .unwrap();
        repo.run(["config", "user.email", "test@example.invalid"])
            .unwrap();
        fs::write(repository.path().join("README.md"), "base\n").unwrap();
        repo.run(["add", "README.md"]).unwrap();
        repo.run(["commit", "-m", "Initial commit"]).unwrap();

        let container = tempfile::tempdir().unwrap();
        let checkout = container.path().join("checkout");
        repo.run([
            "worktree",
            "add",
            "--quiet",
            "--detach",
            &checkout.to_string_lossy(),
            "HEAD",
        ])
        .unwrap();
        fs::write(checkout.join("untracked.txt"), "temporary\n").unwrap();
        repo.run(["worktree", "lock", &checkout.to_string_lossy()])
            .unwrap();

        cleanup_preparation_worktree(&repo, &checkout, container).unwrap();

        assert!(!checkout.exists());
        assert!(
            !repo
                .run(["worktree", "list", "--porcelain"])
                .unwrap()
                .stdout
                .contains(&checkout.to_string_lossy().into_owned())
        );
    }

    #[test]
    fn moved_pointer_drops_path_bound_versions() {
        use crate::storage_format::{Checksum, Entry, Kind, Manifest, Version};
        let temp = tempfile::tempdir().unwrap();
        let task = temp.path().join("task");
        fs::create_dir(&task).unwrap();
        let pointer = task.join("moved.wm-storage.json");
        let entries = vec![Entry {
            path: "alpha.txt".into(),
            checksum: Checksum {
                algorithm: "md5".into(),
                digest: "0cc175b9c0f1b6a831c399e269772661".into(),
            },
            size: 1,
            version: Some(Version {
                id: "old-alpha".into(),
                etag: None,
            }),
        }];
        let manifest = Manifest {
            schema_version: 1,
            path: "moved".into(),
            kind: Kind::Directory,
            checksum: Checksum {
                algorithm: "md5".into(),
                digest: crate::storage_format::directory_digest(&entries).unwrap(),
            },
            size: 1,
            version: None,
            entries: Some(entries),
        };
        fs::write(&pointer, manifest.serialize().unwrap()).unwrap();
        let repo = GitRepo {
            root: temp.path().to_path_buf(),
        };
        reset_moved_cloud_metadata(&repo, "task/moved").unwrap();
        let parsed = Manifest::parse(
            &fs::read_to_string(pointer).unwrap(),
            "task/moved.wm-storage.json",
        )
        .unwrap();
        assert_eq!(parsed.checksum, manifest.checksum);
        assert_eq!(parsed.entries.as_ref().unwrap()[0].path, "alpha.txt");
        assert!(parsed.entries.as_ref().unwrap()[0].version.is_none());
    }

    #[test]
    fn metadata_must_own_one_matching_repository_output() {
        let temp = tempfile::tempdir().unwrap();
        let task = temp.path().join("task");
        fs::create_dir(&task).unwrap();
        let pointer = task.join("data.dvc");
        let repo = GitRepo {
            root: temp.path().to_path_buf(),
        };

        fs::write(&pointer, "outs:\n- path: other\n").unwrap();
        assert!(output_paths(&repo, &["task/data.dvc".to_owned()]).is_err());

        fs::write(&pointer, "outs:\n- path: data\n- path: other\n").unwrap();
        assert!(output_paths(&repo, &["task/data.dvc".to_owned()]).is_err());
    }

    #[test]
    fn addressability_turns_only_on_a_backslash_anywhere_in_the_path() {
        for addressable in [
            "task/data.bin",
            "task/data.bin.dvc",
            "task/d-x/big.bin",
            "task/spaced name.bin",
            "task/colon:name.bin",
            "",
        ] {
            assert!(is_addressable(addressable), "{addressable:?}");
            require_addressable(addressable, "field", "remedy").unwrap();
        }
        for unaddressable in [
            "task/top\\level.bin",
            "task/top\\level.bin.dvc",
            "task/d\\x/big.bin",
            "\\leading.bin",
            "trailing.bin\\",
            "task/two\\back\\slashes.bin",
        ] {
            assert!(!is_addressable(unaddressable), "{unaddressable:?}");
            let error = require_addressable(unaddressable, "S3 storage path", "rename it")
                .unwrap_err()
                .to_string();
            assert!(error.contains("contains a backslash"), "{error}");
            assert!(error.contains("rename it"), "{error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn engine_metadata_keeps_backslashes_in_output_paths() {
        let temp = tempfile::tempdir().unwrap();
        let task = temp.path().join("task");
        fs::create_dir_all(task.join("d\\x")).unwrap();
        fs::write(
            task.join("top\\level.bin.dvc"),
            "outs:\n- path: top\\level.bin\n",
        )
        .unwrap();
        fs::write(task.join("d\\x/big.bin.dvc"), "outs:\n- path: big.bin\n").unwrap();
        fs::create_dir_all(task.join("bundle/x")).unwrap();
        fs::write(task.join("bundle/x\\y.txt"), b"alpha\n").unwrap();
        fs::write(task.join("bundle/x/y.txt"), b"alpha\n").unwrap();
        fs::write(
            task.join("bundle.dvc"),
            "outs:\n- path: bundle\n  files:\n  - relpath: x\\y.txt\n    md5: 9f9f90dbe3e5ee1218c86b8839db1995\n    size: 6\n  - relpath: x/y.txt\n    md5: 9f9f90dbe3e5ee1218c86b8839db1995\n    size: 6\n",
        )
        .unwrap();
        let repo = GitRepo {
            root: temp.path().to_path_buf(),
        };

        let pointers = ["task/top\\level.bin.dvc", "task/d\\x/big.bin.dvc"].map(str::to_owned);
        let outputs = output_paths(&repo, &pointers).unwrap();
        assert_eq!(outputs[&pointers[0]], ["task/top\\level.bin"]);
        assert_eq!(outputs[&pointers[1]], ["task/d\\x/big.bin"]);
        assert!(pointer_matches_worktree(&repo, "task/bundle.dvc").unwrap());

        fs::remove_file(task.join("bundle/x\\y.txt")).unwrap();
        assert!(!pointer_matches_worktree(&repo, "task/bundle.dvc").unwrap());
    }

    #[test]
    fn pointer_integrity_distinguishes_exact_and_modified_outputs_without_a_cache() {
        let temp = tempfile::tempdir().unwrap();
        let task = temp.path().join("task");
        fs::create_dir(&task).unwrap();
        fs::write(task.join("data.bin"), b"exact file\n").unwrap();
        fs::create_dir(task.join("bundle")).unwrap();
        fs::write(task.join("bundle/alpha.txt"), b"alpha\n").unwrap();
        fs::write(
            task.join("data.bin.dvc"),
            "outs:\n- md5: 48036ac48f0d02ad143b45123e44d7fd\n  size: 11\n  path: data.bin\n",
        )
        .unwrap();
        fs::write(
            task.join("bundle.dvc"),
            "outs:\n- path: bundle\n  files:\n  - relpath: alpha.txt\n    md5: 9f9f90dbe3e5ee1218c86b8839db1995\n    size: 6\n",
        )
        .unwrap();
        let repo = GitRepo {
            root: temp.path().to_path_buf(),
        };

        assert!(pointer_matches_worktree(&repo, "task/data.bin.dvc").unwrap());
        assert!(pointer_matches_worktree(&repo, "task/bundle.dvc").unwrap());

        fs::write(task.join("data.bin"), b"changed\n").unwrap();
        fs::write(task.join("bundle/extra.txt"), b"extra\n").unwrap();
        assert!(!pointer_matches_worktree(&repo, "task/data.bin.dvc").unwrap());
        assert!(!pointer_matches_worktree(&repo, "task/bundle.dvc").unwrap());
    }
    #[test]
    fn pointer_entries_cover_every_metadata_shape() {
        let file = parse_pointer_document(
            "outs:\n- md5: 48036ac48f0d02ad143b45123e44d7fd\n  size: 11\n  hash: md5\n  path: data.bin\n",
            "task/data.bin.dvc",
        )
        .unwrap();
        assert_eq!(
            file.entries("task/data.bin.dvc"),
            vec![PointerEntry {
                key: "task/data.bin".to_owned(),
                md5: Some("48036ac48f0d02ad143b45123e44d7fd".to_owned()),
                size: Some(11),
                version_id: None,
                etag: None,
                aggregate: false,
            }]
        );

        let directory = parse_pointer_document(
            "outs:\n- md5: eb2dfde6d481867e4c338a60e69ba734.dir\n  size: 12\n  nfiles: 2\n  hash: md5\n  path: bundle\n",
            "bundle.dvc",
        )
        .unwrap();
        let entries = directory.entries("bundle.dvc");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "bundle");
        assert_eq!(entries[0].size, Some(12));
        assert!(entries[0].aggregate);

        let versioned_file = parse_pointer_document(
            "outs:\n- md5: aaa\n  size: 5\n  hash: md5\n  path: model.pt\n  cloud:\n    workspace-mgr:\n      etag: '\"etag-one\"'\n      version_id: v1\n    other:\n      version_id: foreign\n",
            "task/run/model.pt.dvc",
        )
        .unwrap();
        let entries = versioned_file.entries("task/run/model.pt.dvc");
        assert_eq!(entries[0].key, "task/run/model.pt");
        assert_eq!(entries[0].version_id.as_deref(), Some("v1"));
        assert_eq!(entries[0].etag.as_deref(), Some("\"etag-one\""));

        let versioned_directory = parse_pointer_document(
            "outs:\n- hash: md5\n  path: bundle\n  files:\n  - relpath: alpha.txt\n    md5: a1\n    size: 6\n    cloud:\n      workspace-mgr:\n        etag: e1\n        version_id: va\n  - relpath: nested/beta.txt\n    md5: b1\n    size: 5\n    cloud:\n      workspace-mgr:\n        version_id: 12345\n  - relpath: gamma.txt\n    md5: c1\n    size: 7\n",
            "task/bundle.dvc",
        )
        .unwrap();
        let entries = versioned_directory.entries("task/bundle.dvc");
        assert_eq!(
            entries
                .iter()
                .map(|entry| (
                    entry.key.as_str(),
                    entry.size,
                    entry.version_id.as_deref(),
                    entry.aggregate
                ))
                .collect::<Vec<_>>(),
            vec![
                ("task/bundle/alpha.txt", Some(6), Some("va"), false),
                ("task/bundle/nested/beta.txt", Some(5), Some("12345"), false),
                ("task/bundle/gamma.txt", Some(7), None, false),
            ]
        );

        let cached = serde_json::to_string(&versioned_directory).unwrap();
        assert_eq!(
            serde_json::from_str::<PointerDocument>(&cached).unwrap(),
            versioned_directory
        );
        assert!(parse_pointer_document("outs: [", "broken.dvc").is_err());
        assert!(parse_pointer_document("outs:\n- md5: a\n", "pathless.dvc").is_err());
    }

    #[test]
    fn pointer_entries_keep_every_name_the_storage_engine_accepted() {
        // A version-aware push lists files by the names the engine read from
        // disk; a published blob must never make usage accounting fail.
        let odd = parse_pointer_document(
            "outs:\n- hash: md5\n  path: bundle\n  files:\n  - relpath: \"Icon\\r\"\n    md5: i1\n    size: 1\n  - relpath: 'name '\n    md5: n1\n    size: 2\n  - relpath: ' lead'\n    md5: l1\n    size: 3\n  - relpath: ./nested//deep\\file.txt\n    md5: d1\n    size: 4\n  - relpath: ../x\n    md5: x1\n    size: 5\n  - relpath: /abs\n    md5: a1\n    size: 6\n  - relpath: .\n    md5: e1\n    size: 7\n",
            "task/bundle.dvc",
        )
        .unwrap();
        assert_eq!(
            odd.entries("task/bundle.dvc")
                .iter()
                .map(|entry| (entry.key.as_str(), entry.size))
                .collect::<Vec<_>>(),
            vec![
                ("task/bundle/Icon\r", Some(1)),
                ("task/bundle/name ", Some(2)),
                ("task/bundle/ lead", Some(3)),
                // A backslash is part of the name, not a separator.
                ("task/bundle/nested/deep\\file.txt", Some(4)),
                ("task/bundle", Some(5)),
                ("task/bundle", Some(6)),
                ("task/bundle", Some(7)),
            ]
        );

        let trailing = parse_pointer_document(
            "outs:\n- md5: t1\n  size: 8\n  path: 'report '\n",
            "task/report .dvc",
        )
        .unwrap();
        assert_eq!(trailing.entries("task/report .dvc")[0].key, "task/report ");
        let root =
            parse_pointer_document("outs:\n- md5: r1\n  size: 9\n  path: data\n", "data.dvc")
                .unwrap();
        assert_eq!(root.entries("data.dvc")[0].key, "data");
        let escape = parse_pointer_document("outs:\n- path: ../escape\n", "task/escape.dvc")
            .unwrap()
            .entries("task/escape.dvc");
        assert_eq!(escape[0].key, "task/escape");
    }

    #[test]
    fn existing_pointer_readers_still_accept_cloud_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let task = temp.path().join("task");
        fs::create_dir(&task).unwrap();
        fs::write(
            task.join("data.dvc"),
            "outs:\n- md5: aaa\n  size: 1\n  path: data\n  cloud:\n    workspace-mgr:\n      version_id: v1\n",
        )
        .unwrap();
        let repo = GitRepo {
            root: temp.path().to_path_buf(),
        };
        let outputs = output_paths(&repo, &["task/data.dvc".to_owned()]).unwrap();
        assert_eq!(outputs["task/data.dvc"], vec!["task/data".to_owned()]);
        let document = read_pointer_document(&repo, "task/data.dvc").unwrap();
        assert_eq!(document.outs[0].version_id.as_deref(), Some("v1"));
    }

    #[test]
    fn granular_data_status_keeps_the_reported_names() {
        let status = parse_data_status(
            r#"{"not_in_cache": ["T\\g.bin"], "uncommitted": {"modified": ["T/dir/", "T/sub/f.bin", "T/dir/a.txt", "T/dir/a\\b.bin"], "added": ["T/dir/d.txt"], "renamed": [{"old": "T/dir/b.txt", "new": "T/dir/e.txt"}], "deleted": ["T/dir/c.txt"]}, "committed": {"added": ["T/other"]}}"#,
        )
        .unwrap();
        // A backslash is an ordinary name character on Linux and macOS. A
        // boundary at such a path is refused before any status runs, so the
        // top-level row only proves that no metadata can mangle a name;
        // names inside a directory boundary are reported and charged.
        assert_eq!(status.not_in_cache, vec!["T\\g.bin".to_owned()]);
        assert_eq!(
            status.uncommitted.modified,
            vec![
                "T/dir/".to_owned(),
                "T/sub/f.bin".to_owned(),
                "T/dir/a.txt".to_owned(),
                "T/dir/a\\b.bin".to_owned()
            ]
        );
        assert_eq!(status.uncommitted.added, vec!["T/dir/d.txt".to_owned()]);
        assert_eq!(status.uncommitted.deleted, vec!["T/dir/c.txt".to_owned()]);
        assert_eq!(
            status.uncommitted.renamed,
            vec![DataRename {
                old: "T/dir/b.txt".to_owned(),
                new: "T/dir/e.txt".to_owned(),
            }]
        );
        assert_eq!(parse_data_status("").unwrap(), DataStatus::default());
        assert!(parse_data_status("not json").is_err());
    }

    #[test]
    fn directory_listings_resolve_from_local_object_stores() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("cache");
        let remote = temp.path().join("remote");
        let write = |path: PathBuf, content: &[u8]| {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        };
        let a = "0cc175b9c0f1b6a831c399e269772661";
        let b = "92eb5ffee6ae2fec3ad71c777531578f";
        let dir = "00aa73e716a615de21f40969f0ea1dd1";
        write(
            cache.join(format!("files/md5/00/{}.dir", &dir[2..])),
            format!(r#"[{{"md5": "{a}", "relpath": "a.txt"}}, {{"md5": "{b}", "relpath": "sub/b\\c.bin"}}]"#)
                .as_bytes(),
        );
        write(cache.join(format!("files/md5/0c/{}", &a[2..])), b"a");
        // A file object may be found in another store or the legacy layout.
        write(remote.join(format!("92/{}", &b[2..])), b"bbbb");
        let stores = vec![cache.clone(), remote.clone()];
        let listing = directory_listing(&stores, &format!("{dir}.dir")).unwrap();
        assert_eq!(
            listing,
            vec![
                ListedFile {
                    relpath: "a.txt".to_owned(),
                    md5: a.to_owned(),
                    size: 1,
                },
                ListedFile {
                    relpath: "sub/b\\c.bin".to_owned(),
                    md5: b.to_owned(),
                    size: 4,
                },
            ]
        );
        assert_eq!(
            object_key("T/data", &listing[1].relpath),
            "T/data/sub/b\\c.bin"
        );
        // Anything that cannot be resolved completely falls back to the
        // aggregate, and metadata can never name a path outside a store.
        assert_eq!(directory_listing(&stores[..1], &format!("{dir}.dir")), None);
        assert_eq!(directory_listing(&stores, dir), None);
        assert_eq!(
            directory_listing(&stores, "ffffffffffffffffffffffffffffffff.dir"),
            None
        );
        assert_eq!(directory_listing(&stores, "../../../etc/passwd.dir"), None);
        assert_eq!(
            crate::legacy_dvc::stored_object(&stores, "0c/../../x", ""),
            None
        );
    }

    #[test]
    fn object_stores_include_native_and_historical_configuration() {
        let temp = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: temp.path().to_path_buf(),
        };
        repo.run(["init", "-q"]).unwrap();
        let native_cache = crate::native_engine::cache_root(&repo).unwrap();
        let engine = temp.path().join(".dvc");
        fs::create_dir(&engine).unwrap();
        let mut config = Config {
            s3: Some(crate::config::S3Config {
                url: "s3://bucket/prefix".to_owned(),
                endpoint_url: None,
            }),
            ..Config::default()
        };
        assert_eq!(
            local_object_stores(&repo, &config),
            vec![native_cache.clone(), engine.join("cache")]
        );
        config.s3.as_mut().unwrap().url = "/srv/storage".to_owned();
        assert_eq!(
            local_object_stores(&repo, &config),
            vec![
                native_cache.clone(),
                engine.join("cache"),
                PathBuf::from("/srv/storage")
            ]
        );
        config.s3.as_mut().unwrap().url = "../storage".to_owned();
        fs::write(
            engine.join("config.local"),
            "[core]\n    dir = ignored\n[cache]\n    type = copy\n    dir = /shared/cache\n",
        )
        .unwrap();
        assert_eq!(
            local_object_stores(&repo, &config),
            vec![
                native_cache,
                PathBuf::from("/shared/cache"),
                repo.root.join("../storage"),
                engine.join("../storage")
            ]
        );
    }
}
