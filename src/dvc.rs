use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::config::Config;
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::hex::encode_lower;
use crate::path::{allowed, reject_symlink_traversal, relative_to, repo_path, resolved_under};
use crate::process::{CommandOutput, run as run_process, run_unchecked as run_process_unchecked};

const VERSION_VERIFY_SCRIPT: &str = include_str!("../assets/dvc_version_verify.py");
const VERSION_PURGE_SCRIPT: &str = include_str!("../assets/dvc_version_purge.py");
const INTERNAL_CONFIG_HEADER: &str =
    "# Managed by workspace-mgr. Edit .workspace-mgr.toml and rerun workspace-mgr init.\n";
pub const REQUIRED_DVC_VERSION: &str = "3.67.1";
pub const INTERNAL_REMOTE: &str = "workspace-mgr";
#[cfg(feature = "test-storage")]
pub const STORAGE_PYTHON_ENV: &str = "WORKSPACE_MGR_STORAGE_PYTHON";

pub fn storage_python() -> String {
    crate::runtime::storage_python()
}

pub fn dvc_program() -> String {
    crate::runtime::dvc_program()
}

pub fn require_runtime(repo: &GitRepo) -> Result<String> {
    let output = inspect_engine(&repo.root, ["--version"])?;
    if !output.success() {
        return Err(Error::message(
            "managed-storage runtime is unavailable; install workspace-mgr with its required storage runtime",
        ));
    }
    let actual = output.stdout.trim();
    if actual != REQUIRED_DVC_VERSION {
        return Err(Error::message(format!(
            "managed-storage runtime version {actual:?} is incompatible; workspace-mgr requires exactly {REQUIRED_DVC_VERSION}"
        )));
    }
    Ok(actual.to_owned())
}

pub fn require_version_adapter(repo: &GitRepo) -> Result<String> {
    let python = storage_python();
    let output = run_process_unchecked(
        &python,
        ["-c", "import dvc; print(dvc.__version__)"],
        &repo.root,
    )
    .map_err(private_engine_error)?;
    if !output.success() {
        return Err(Error::message(
            "managed-storage version adapter is unavailable; run `workspace-mgr setup`",
        ));
    }
    let actual = output.stdout.trim();
    if actual != REQUIRED_DVC_VERSION {
        return Err(Error::message(format!(
            "managed-storage version adapter {actual:?} is incompatible; workspace-mgr requires exactly {REQUIRED_DVC_VERSION}"
        )));
    }
    Ok(format!("internal adapter {actual}"))
}

pub fn render_internal_config(config: &Config) -> Result<Option<String>> {
    let Some(s3) = &config.s3 else {
        return Ok(None);
    };
    let url = &s3.url;
    let mut rendered = format!(
        "{INTERNAL_CONFIG_HEADER}[core]\n    remote = {INTERNAL_REMOTE}\n['remote \"{INTERNAL_REMOTE}\"']\n    url = {url}\n"
    );
    if let Some(endpoint) = &s3.endpoint_url {
        rendered.push_str(&format!("    endpointurl = {endpoint}\n"));
    }
    if config.requires_object_versioning() {
        rendered.push_str("    version_aware = true\n");
    }
    Ok(Some(rendered))
}

pub fn write_internal_config(repo: &GitRepo, config: &Config) -> Result<bool> {
    let Some(rendered) = render_internal_config(config)? else {
        return Ok(false);
    };
    let path = internal_config_path(repo)?;
    if fs::read_to_string(&path).ok().as_deref() == Some(&rendered) {
        return Ok(false);
    }
    let parent = path
        .parent()
        .ok_or_else(|| Error::message("managed-storage config path has no parent"))?;
    fs::create_dir_all(parent).at(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).at(parent)?;
    temporary.write_all(rendered.as_bytes()).at(&path)?;
    temporary.flush().at(&path)?;
    temporary.persist(&path).map_err(|error| Error::Io {
        path,
        source: error.error,
    })?;
    Ok(true)
}

pub fn internal_config_exists(repo: &GitRepo) -> Result<bool> {
    let path = internal_config_path(repo)?;
    Ok(path.is_file())
}

pub fn internal_location(repo: &GitRepo) -> Result<Option<(String, Option<String>)>> {
    let path = internal_config_path(repo)?;
    if !path.is_file() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path).at(&path)?;
    let mut url = None;
    let mut endpoint = None;
    for line in raw.lines().map(str::trim) {
        if let Some(value) = line.strip_prefix("url = ") {
            if value.is_empty() || url.replace(value.to_owned()).is_some() {
                return Err(Error::message(
                    "managed-storage configuration has an ambiguous storage URL; restore it with `workspace-mgr init` after removing all storage boundaries",
                ));
            }
        } else if let Some(value) = line.strip_prefix("endpointurl = ") {
            if value.is_empty() || endpoint.replace(value.to_owned()).is_some() {
                return Err(Error::message(
                    "managed-storage configuration has an ambiguous endpoint URL; restore it with `workspace-mgr init` after removing all storage boundaries",
                ));
            }
        }
    }
    let url = url.ok_or_else(|| {
        Error::message(
            "managed-storage configuration has no storage URL; restore it with `workspace-mgr init` after removing all storage boundaries",
        )
    })?;
    Ok(Some((url, endpoint)))
}

pub fn remove_internal_config(repo: &GitRepo) -> Result<bool> {
    if !internal_config_exists(repo)? {
        return Ok(false);
    }
    let path = internal_config_path(repo)?;
    fs::remove_file(&path).at(&path)?;
    Ok(true)
}

pub fn repository_pointers(repo: &GitRepo) -> Result<Vec<String>> {
    let mut found = BTreeSet::new();
    for path in repo.visible_paths(&[])? {
        let absolute = resolved_under(&repo.root, &path);
        if absolute.extension().and_then(|value| value.to_str()) == Some("dvc") {
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

pub fn validate_internal_config(repo: &GitRepo, config: &Config) -> Result<()> {
    let Some(expected) = render_internal_config(config)? else {
        return Ok(());
    };
    let path = internal_config_path(repo)?;
    let actual = fs::read_to_string(&path).at(&path)?;
    if actual != expected {
        return Err(Error::message(
            "managed-storage configuration drifted from .workspace-mgr.toml; run `workspace-mgr init` to regenerate internal scaffolding",
        ));
    }
    Ok(())
}

fn internal_config_path(repo: &GitRepo) -> Result<std::path::PathBuf> {
    reject_symlink_traversal(&repo.root, ".dvc/config", "managed-storage configuration")?;
    Ok(repo.root.join(".dvc/config"))
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
    let python = storage_python();
    let output = run_process_unchecked(
        &python,
        [
            "-c",
            VERSION_VERIFY_SCRIPT,
            &repo.root.to_string_lossy(),
            "[]",
            "--check-versioning-only",
        ],
        &repo.root,
    )
    .map_err(private_engine_error)?;
    if !output.success() {
        return Err(Error::message(format!(
            "failed to verify S3 bucket object versioning: {}",
            private_detail(&output)
        )));
    }
    serde_json::from_str(output.stdout.trim()).map_err(|error| {
        Error::message(format!(
            "bucket-versioning verifier returned invalid JSON: {error}"
        ))
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct DvcReport {
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

#[derive(Debug, Clone, Deserialize)]
struct Pointer {
    outs: Vec<PointerOut>,
}

#[derive(Debug, Clone, Deserialize)]
struct PointerOut {
    path: String,
    #[serde(default)]
    md5: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    files: Option<Vec<PointerFile>>,
}

#[derive(Debug, Clone, Deserialize)]
struct PointerFile {
    relpath: String,
    md5: String,
    size: u64,
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

#[derive(Debug, Deserialize)]
struct RawPointer {
    #[serde(default)]
    outs: Vec<RawPointerOutput>,
}

#[derive(Debug, Deserialize)]
struct RawPointerOutput {
    path: String,
    #[serde(default)]
    md5: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    cloud: Option<serde_yaml::Value>,
    #[serde(default)]
    files: Option<Vec<RawPointerFile>>,
}

#[derive(Debug, Deserialize)]
struct RawPointerFile {
    relpath: String,
    #[serde(default)]
    md5: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    cloud: Option<serde_yaml::Value>,
}

pub(crate) fn parse_pointer_document(raw: &str, origin: &str) -> Result<PointerDocument> {
    let parsed: RawPointer = serde_yaml::from_str(raw).map_err(|error| {
        Error::message(format!(
            "invalid managed-storage metadata {origin}: {error}"
        ))
    })?;
    Ok(PointerDocument {
        outs: parsed
            .outs
            .into_iter()
            .map(|output| {
                let (version_id, etag) = internal_cloud_version(output.cloud.as_ref());
                PointerOutput {
                    path: output.path,
                    md5: output.md5,
                    size: output.size,
                    version_id,
                    etag,
                    files: output.files.map(|files| {
                        files
                            .into_iter()
                            .map(|file| {
                                let (version_id, etag) =
                                    internal_cloud_version(file.cloud.as_ref());
                                PointerFileVersion {
                                    relpath: file.relpath,
                                    md5: file.md5,
                                    size: file.size,
                                    version_id,
                                    etag,
                                }
                            })
                            .collect()
                    }),
                }
            })
            .collect(),
    })
}

pub(crate) fn read_pointer_document(repo: &GitRepo, pointer: &str) -> Result<PointerDocument> {
    reject_symlink_traversal(&repo.root, pointer, "managed-storage metadata")?;
    let pointer_path = resolved_under(&repo.root, pointer);
    let raw = fs::read_to_string(&pointer_path).at(&pointer_path)?;
    parse_pointer_document(&raw, pointer)
}

fn internal_cloud_version(cloud: Option<&serde_yaml::Value>) -> (Option<String>, Option<String>) {
    let Some(remote) = cloud.and_then(|cloud| cloud.get(INTERNAL_REMOTE)) else {
        return (None, None);
    };
    let field = |name: &str| match remote.get(name) {
        Some(serde_yaml::Value::String(value)) if !value.is_empty() => Some(value.clone()),
        Some(serde_yaml::Value::Number(value)) => Some(value.to_string()),
        _ => None,
    };
    (field("version_id"), field("etag"))
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
        let metadata_boundary = pointer.strip_suffix(".dvc").unwrap_or(pointer);
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
    let args = ["data", "status", "--granular", "--json", "--"]
        .into_iter()
        .map(ToOwned::to_owned)
        .chain(outputs.iter().cloned())
        .collect::<Vec<_>>();
    let output = inspect_engine(&repo.root, args)?;
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

#[derive(Debug, Deserialize)]
struct ManifestEntry {
    md5: String,
    relpath: String,
}

/// Local object stores that can hold directory manifests and file objects:
/// the repository's storage cache and, for a filesystem remote, the remote
/// itself. Neither is contacted over a network.
pub(crate) fn local_object_stores(repo: &GitRepo, config: &Config) -> Vec<PathBuf> {
    let engine_dir = repo.root.join(".dvc");
    let mut stores = vec![cache_dir(&engine_dir)];
    if let Some(s3) = config.s3.as_ref().filter(|s3| !s3.url.contains("://")) {
        // The engine resolves a relative remote path against its config file.
        stores.push(engine_dir.join(&s3.url));
    }
    stores
}

/// The engine's cache directory: `cache.dir` from the private or generated
/// engine config, relative to the config directory, or the default.
fn cache_dir(engine_dir: &Path) -> PathBuf {
    ["config.local", "config"]
        .iter()
        .filter_map(|name| fs::read_to_string(engine_dir.join(name)).ok())
        .find_map(|raw| configured_cache_dir(&raw))
        .map_or_else(|| engine_dir.join("cache"), |dir| engine_dir.join(dir))
}

fn configured_cache_dir(raw: &str) -> Option<String> {
    let mut in_cache = false;
    for line in raw.lines().map(str::trim) {
        if line.starts_with('[') {
            in_cache = line == "[cache]";
            continue;
        }
        if !in_cache {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let value = value.trim();
            if key.trim() == "dir" && !value.is_empty() {
                return Some(value.to_owned());
            }
        }
    }
    None
}

/// Resolves the files of a directory version recorded as `<md5>.dir` from
/// its manifest, sizing each file by its stored object. Returns `None` when
/// the manifest, or the object of any file it lists, is not available in a
/// local object store, so the caller can fall back to the directory's
/// aggregate size.
pub(crate) fn directory_listing(stores: &[PathBuf], digest: &str) -> Option<Vec<ListedFile>> {
    let manifest = stored_object(stores, digest.strip_suffix(".dir")?, ".dir")?;
    let raw = fs::read(manifest).ok()?;
    let entries: Vec<ManifestEntry> = serde_json::from_slice(&raw).ok()?;
    entries
        .into_iter()
        .map(|entry| {
            let object = stored_object(stores, &entry.md5, "")?;
            let size = fs::metadata(object).ok()?.len();
            Some(ListedFile {
                relpath: entry.relpath,
                md5: entry.md5,
                size,
            })
        })
        .collect()
}

/// Finds an object in the current or the legacy cache layout. Only a plain
/// MD5 digest can name an object, so metadata cannot point outside a store.
fn stored_object(stores: &[PathBuf], md5: &str, suffix: &str) -> Option<PathBuf> {
    if md5.len() != 32 || !md5.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let (prefix, rest) = md5.split_at(2);
    let name = format!("{rest}{suffix}");
    stores
        .iter()
        .flat_map(|store| {
            [
                store.join("files/md5").join(prefix).join(&name),
                store.join(prefix).join(&name),
            ]
        })
        .find(|path| path.is_file())
}

pub fn discover(repo: &GitRepo, scopes: &[String]) -> Result<Vec<String>> {
    let mut found = BTreeSet::new();
    for path in repo.visible_paths(scopes)? {
        if crate::storage::is_local(repo, &path)? {
            continue;
        }
        let absolute = resolved_under(&repo.root, &path);
        if absolute.extension().and_then(|value| value.to_str()) != Some("dvc") {
            continue;
        }
        let boundary = path
            .strip_suffix(".dvc")
            .expect("the metadata extension was checked above");
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
    let parsed: Pointer = serde_yaml::from_str(raw).map_err(|source| Error::Yaml {
        path: resolved_under(&repo.root, pointer),
        source,
    })?;
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
    let expected = pointer
        .strip_suffix(".dvc")
        .ok_or_else(|| Error::message(format!("invalid metadata path: {pointer}")))?;
    if output != expected {
        return Err(Error::message(format!(
            "managed-storage output {output:?} must match metadata boundary {expected:?}"
        )));
    }
    reject_symlink_traversal(&repo.root, &output, "managed-storage output")?;
    Ok(output)
}

/// The storage engine resolves a `\` in a command target inconsistently.
/// Measured with the pinned engine on Unix, `add`, `fetch`, `checkout`, and
/// `move` address such a path literally, while `status` rewrites the `\` to
/// `/`, reports the rewritten path missing, and fails, so nothing can verify
/// such a boundary through the engine. Which subcommands rewrite is
/// undocumented and free to change between engine versions, so the commands
/// that place content refuse such a path outright rather than rest on the ones
/// that happen to work today. `refresh` does not choose what a shared branch
/// carries, so it skips that one boundary and reports it instead of refusing
/// the whole update; `move`, which fetches the payload through the old
/// metadata and then renames it, is the recovery.
pub fn is_addressable(path: &str) -> bool {
    !path.contains('\\')
}

pub fn require_addressable(path: &str, field: &str, remedy: &str) -> Result<()> {
    if !is_addressable(path) {
        return Err(Error::message(format!(
            "{field} {path:?} contains a backslash, which the storage engine reads as a path separator; {remedy}"
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
    let output = inspect_engine(&repo.root, ["status", "--json", "--", pointer])?;
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
) -> Result<DvcReport> {
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
    let mut report = DvcReport {
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
        execute_engine(&repo.root, ["commit", "--force", "--", pointer])?;
    }
    report.committed = dirty;
    Ok(report)
}

/// Uploads every output that [`reconcile`] prepared and verifies the result.
pub fn push_outputs(repo: &GitRepo, config: &Config, report: &mut DvcReport) -> Result<()> {
    if !report.files.is_empty() {
        let mut args = vec!["push".to_owned(), "--".to_owned()];
        args.extend(report.files.iter().cloned());
        execute_engine(&repo.root, args)?;
        report.verification = Some(verify(repo, config, &report.files)?);
    }
    report.pushed = report.files.clone();
    Ok(())
}

pub fn verify(repo: &GitRepo, config: &Config, pointers: &[String]) -> Result<serde_json::Value> {
    if pointers.is_empty() {
        return Ok(serde_json::json!({"mode": "no-files"}));
    }
    let local_args = std::iter::once("status".to_owned())
        .chain(["--quiet".to_owned(), "--".to_owned()])
        .chain(pointers.iter().cloned())
        .collect::<Vec<_>>();
    let local = inspect_engine(&repo.root, local_args)?;
    if !local.success() {
        return Err(Error::message(format!(
            "managed-storage metadata does not match local data for: {}",
            pointers.join(", ")
        )));
    }

    ensure_ready(repo, config)?;
    let exact = config.requires_object_versioning();
    if exact {
        let python = storage_python();
        let serialized = serde_json::to_string(pointers).map_err(|error| {
            Error::message(format!("failed to encode storage metadata files: {error}"))
        })?;
        let output = run_process_unchecked(
            &python,
            [
                "-c",
                VERSION_VERIFY_SCRIPT,
                &repo.root.to_string_lossy(),
                &serialized,
            ],
            &repo.root,
        )
        .map_err(private_engine_error)?;
        if !output.success() {
            return Err(Error::message(format!(
                "failed to verify versioned storage content: {}",
                private_detail(&output)
            )));
        }
        return serde_json::from_str(output.stdout.trim()).map_err(|error| {
            Error::message(format!(
                "version-aware verifier returned invalid JSON: {error}"
            ))
        });
    }

    let cloud_args = std::iter::once("status".to_owned())
        .chain(["--cloud".to_owned(), "--quiet".to_owned(), "--".to_owned()])
        .chain(pointers.iter().cloned())
        .collect::<Vec<_>>();
    let cloud = inspect_engine(&repo.root, cloud_args)?;
    if !cloud.success() {
        return Err(Error::message(format!(
            "stored content is missing from the configured remote for: {}",
            pointers.join(", ")
        )));
    }
    Ok(serde_json::json!({"mode": "remote-status"}))
}

pub fn version_purge_adapter(
    repo: &GitRepo,
    operation: &str,
    payload: &serde_json::Value,
) -> Result<serde_json::Value> {
    let python = storage_python();
    let serialized = serde_json::to_string(payload).map_err(|error| {
        Error::message(format!(
            "failed to encode managed-storage purge request: {error}"
        ))
    })?;
    let output = run_process_unchecked(
        &python,
        [
            "-c",
            VERSION_PURGE_SCRIPT,
            &repo.root.to_string_lossy(),
            operation,
            &serialized,
        ],
        &repo.root,
    )
    .map_err(private_engine_error)?;
    if !output.success() {
        return Err(Error::message(format!(
            "managed-storage permanent deletion failed: {}",
            private_detail(&output)
        )));
    }
    serde_json::from_str(output.stdout.trim()).map_err(|error| {
        Error::message(format!(
            "managed-storage purge adapter returned invalid JSON: {error}"
        ))
    })
}

pub fn hydrate(
    repo: &GitRepo,
    config: &Config,
    scopes: &[String],
    targets: &[String],
    dry_run: bool,
) -> Result<HydrateReport> {
    ensure_ready(repo, config)?;
    if config.requires_object_versioning() {
        verify_object_versioning(repo, config)?;
    }
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
            if !target.ends_with(".dvc") {
                return Err(Error::message(format!(
                    "hydrate target is not a managed-storage metadata file: {target}"
                )));
            }
            if !allowed(target, scopes) {
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
    let fetch = ["fetch".to_owned(), "--".to_owned()]
        .into_iter()
        .chain(pointers.iter().cloned())
        .collect::<Vec<_>>();
    execute_engine(&repo.root, fetch)?;
    // DVC reports an exact local output as "not in cache" when the cache was
    // cleared. Fetching first restores the comparison object without touching
    // the worktree, allowing the conflict check to distinguish identical
    // content from a genuine local modification.
    validate_worktree(repo, config, &pointers)?;
    let checkout = ["checkout".to_owned(), "--".to_owned()]
        .into_iter()
        .chain(pointers.iter().cloned())
        .collect::<Vec<_>>();
    execute_engine(&repo.root, checkout)?;
    report.verification = Some(verify(repo, config, &pointers)?);
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
    let parsed: Pointer = serde_yaml::from_str(raw).map_err(|source| Error::Yaml {
        path: resolved_under(&repo.root, pointer),
        source,
    })?;
    if parsed.outs.len() != 1 {
        return Err(Error::message(format!(
            "managed-storage metadata must define exactly one output: {pointer}"
        )));
    }
    let output = &parsed.outs[0];
    let boundary = pointer
        .strip_suffix(".dvc")
        .ok_or_else(|| Error::message(format!("invalid metadata path: {pointer}")))?;
    let boundary_path = resolved_under(&repo.root, boundary);
    if boundary_path.is_symlink() {
        return Ok(false);
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
            Ok(md5_file(&boundary_path)? == *expected_md5)
        }
        Some(files) => {
            if !boundary_path.is_dir() {
                return Ok(false);
            }
            let mut expected = BTreeMap::new();
            for file in files {
                let relative = repo_path(&file.relpath, "managed-storage directory entry")?;
                if expected
                    .insert(relative.clone(), (&file.md5, file.size))
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
                    || md5_file(entry.path())? != **expected_md5
                {
                    return Ok(false);
                }
                actual.insert(relative);
            }
            Ok(actual.len() == expected.len())
        }
    }
}

fn md5_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).at(path)?;
    let mut hasher = Md5::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).at(path)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(encode_lower(hasher.finalize()))
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
        let mut args = match operation {
            "track" => vec!["add".to_owned(), "--".to_owned()],
            "move" => vec!["move".to_owned(), "--".to_owned()],
            "untrack" => vec!["remove".to_owned(), "--".to_owned()],
            other => {
                return Err(Error::message(format!(
                    "unknown managed-storage operation {other}"
                )));
            }
        };
        args.extend(paths.iter().cloned());
        execute_engine(&repo.root, args)?;
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
    reset_moved_pointer_cloud_metadata(repo, &format!("{output}.dvc")).map(|_| ())
}

pub(crate) fn reset_moved_pointer_cloud_metadata(repo: &GitRepo, pointer: &str) -> Result<bool> {
    let pointer = repo_path(pointer, "moved managed-storage metadata")?;
    if !pointer.ends_with(".dvc") {
        return Err(Error::message(format!(
            "moved managed-storage metadata must end in .dvc: {pointer}"
        )));
    }
    reject_symlink_traversal(&repo.root, &pointer, "moved managed-storage metadata")?;
    let pointer = resolved_under(&repo.root, &pointer);
    let raw = fs::read_to_string(&pointer).at(&pointer)?;
    let mut document: serde_yaml::Value =
        serde_yaml::from_str(&raw).map_err(|source| Error::Yaml {
            path: pointer.clone(),
            source,
        })?;
    let outs = document
        .as_mapping_mut()
        .and_then(|mapping| mapping.get_mut(serde_yaml::Value::String("outs".to_owned())))
        .and_then(serde_yaml::Value::as_sequence_mut)
        .ok_or_else(|| {
            Error::message(format!(
                "moved storage metadata did not define outputs: {}",
                pointer.display()
            ))
        })?;
    let mut removed = false;
    for out in outs {
        removed |= remove_cloud_metadata(out);
    }
    if !removed {
        return Ok(false);
    }
    let rendered = serde_yaml::to_string(&document).map_err(|error| {
        Error::message(format!(
            "failed to render moved storage metadata {}: {error}",
            pointer.display()
        ))
    })?;
    let parent = pointer
        .parent()
        .ok_or_else(|| Error::message("moved storage metadata has no parent"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).at(parent)?;
    use std::io::Write;
    temporary.write_all(rendered.as_bytes()).at(&pointer)?;
    temporary.flush().at(&pointer)?;
    temporary.persist(&pointer).map_err(|error| Error::Io {
        path: pointer,
        source: error.error,
    })?;
    Ok(true)
}

fn remove_cloud_metadata(value: &mut serde_yaml::Value) -> bool {
    match value {
        serde_yaml::Value::Mapping(mapping) => {
            let removed = mapping
                .remove(serde_yaml::Value::String("cloud".to_owned()))
                .is_some();
            mapping.values_mut().fold(removed, |changed, value| {
                remove_cloud_metadata(value) || changed
            })
        }
        serde_yaml::Value::Sequence(sequence) => {
            let mut removed = false;
            for value in sequence {
                removed |= remove_cloud_metadata(value);
            }
            removed
        }
        _ => false,
    }
}

pub fn prepare_revision(
    repo: &GitRepo,
    config: &Config,
    oid: &str,
    pointers: &[String],
) -> Result<PreparedRevision> {
    ensure_ready(repo, config)?;
    if config.requires_object_versioning() {
        verify_object_versioning(repo, config)?;
    }
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
        let outputs = output_paths(&checkout_repo, pointers)?;
        let args = ["fetch".to_owned(), "--".to_owned()]
            .into_iter()
            .chain(pointers.iter().cloned())
            .collect::<Vec<_>>();
        execute_engine(&checkout, args).map_err(|source| prefetch_error(oid, source))?;
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
    if !checkout.root.join(".dvc").is_dir() {
        return Ok(());
    }
    let shared_cache = source.root.join(".dvc/cache");
    fs::create_dir_all(&shared_cache).at(&shared_cache)?;
    let checkout_cache = checkout.root.join(".dvc/cache");
    if !checkout_cache.exists() {
        symlink_dir(&shared_cache, &checkout_cache)?;
    }
    let shared_local = source.root.join(".dvc/config.local");
    let checkout_local = checkout.root.join(".dvc/config.local");
    if shared_local.is_file() && !checkout_local.exists() {
        symlink_file(&shared_local, &checkout_local)?;
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

#[cfg(unix)]
fn symlink_file(source: &Path, target: &Path) -> Result<()> {
    std::os::unix::fs::symlink(source, target).at(target)
}

#[cfg(windows)]
fn symlink_file(source: &Path, target: &Path) -> Result<()> {
    std::os::windows::fs::symlink_file(source, target).at(target)
}

trait EmptyFallback {
    fn if_empty<'a>(&'a self, fallback: &'a str) -> &'a str;
}

impl EmptyFallback for str {
    fn if_empty<'a>(&'a self, fallback: &'a str) -> &'a str {
        if self.is_empty() { fallback } else { self }
    }
}

pub fn execute_engine<I, S>(cwd: &Path, args: I) -> Result<CommandOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    run_process(&dvc_program(), args, cwd).map_err(private_engine_error)
}

fn inspect_engine<I, S>(cwd: &Path, args: I) -> Result<CommandOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    run_process_unchecked(&dvc_program(), args, cwd).map_err(private_engine_error)
}

fn private_engine_error(error: Error) -> Error {
    match error {
        Error::Command { code, detail, .. } => Error::Command {
            command: "managed-storage".to_owned(),
            code,
            detail: sanitize_private_detail(&detail),
        },
        Error::Terminated { status, detail, .. } => Error::Terminated {
            command: "managed-storage".to_owned(),
            status,
            detail: sanitize_private_detail(&detail),
        },
        Error::MissingCommand(_) => {
            Error::message("managed-storage runtime is unavailable; run `workspace-mgr setup`")
        }
        other => other,
    }
}

fn private_detail(output: &CommandOutput) -> String {
    let detail = if output.stderr.trim().is_empty() {
        &output.stdout
    } else {
        &output.stderr
    };
    sanitize_private_detail(detail)
}

fn sanitize_private_detail(detail: &str) -> String {
    let candidates = detail
        .lines()
        .map(str::trim)
        .filter(|line| {
            !(line.is_empty()
                || line.starts_with("Traceback")
                || line.starts_with("File \"")
                || line.starts_with("See ")
                || line.starts_with("http://")
                || line.starts_with("https://")
                || line.starts_with('<') && line.ends_with('>'))
        })
        .collect::<Vec<_>>();
    let line = candidates
        .iter()
        .rev()
        .find(|line| line.starts_with("ERROR:") || line.contains("Error:"))
        .or_else(|| candidates.last())
        .copied()
        .unwrap_or("internal engine reported a failure");
    let mut sanitized = line
        .replace("DVC", "internal engine")
        .replace("dvc", "internal engine");
    if let Some(runtime) = crate::runtime::managed_runtime_dir() {
        let runtime = runtime.to_string_lossy();
        if !runtime.is_empty() {
            sanitized = sanitized.replace(runtime.as_ref(), "<private-runtime>");
        }
    }
    sanitized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefetch_error_is_actionable_without_exposing_the_engine_command() {
        let error = prefetch_error(
            "deadbeef",
            Error::Command {
                command: "dvc".to_owned(),
                code: 255,
                detail: "ERROR: HeadObject returned 403\nSee https://dvc.org/support".to_owned(),
            },
        )
        .to_string();

        assert!(error.contains("download or read-transaction caps"));
        assert!(error.contains("HeadObject returned 403"));
        assert!(!error.contains("dvc"));
    }

    #[test]
    fn private_engine_errors_hide_runtime_details_and_tracebacks() {
        let runtime = crate::runtime::managed_runtime_dir().unwrap();
        let error = private_engine_error(Error::Command {
            command: runtime.join("bin/dvc").display().to_string(),
            code: 23,
            detail: format!(
                "Traceback: internal Python frame\nDVC failed in {}",
                runtime.join("lib/dvc/cache").display()
            ),
        });
        let detail = error.to_string();
        assert!(detail.contains("managed-storage failed"));
        assert!(detail.contains("exit code 23"));
        assert!(detail.contains("internal engine failed"));
        assert!(!detail.contains("Traceback"));
        assert!(!detail.contains(&runtime.display().to_string()));
        assert!(!detail.contains("DVC"));
        assert!(!detail.contains("dvc"));

        // An engine that a signal ended is named and sanitized the same way.
        let error = private_engine_error(Error::Terminated {
            command: runtime.join("bin/dvc").display().to_string(),
            status: "signal: 9 (SIGKILL)".to_owned(),
            detail: format!("DVC stopped in {}", runtime.display()),
        });
        assert_eq!(
            error.to_string(),
            "managed-storage did not exit normally (signal: 9 (SIGKILL)): internal engine stopped in <private-runtime>"
        );
    }

    #[test]
    fn private_diagnostics_skip_internal_documentation_links() {
        let detail = sanitize_private_detail(
            "ERROR: DVC could not retrieve stored content\nSee troubleshooting details\n<https://error.dvc.org/missing-files>\n",
        );
        assert_eq!(
            detail,
            "ERROR: internal engine could not retrieve stored content"
        );
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
    fn moved_pointer_drops_path_bound_cloud_versions() {
        let temp = tempfile::tempdir().unwrap();
        let task = temp.path().join("task");
        fs::create_dir(&task).unwrap();
        let pointer = task.join("moved.dvc");
        fs::write(
            &pointer,
            "outs:\n- md5: directory.dir\n  path: moved\n  cloud:\n    storage:\n      version_id: old-directory\n  files:\n  - relpath: alpha.txt\n    md5: alpha\n    cloud:\n      storage:\n        version_id: old-alpha\n",
        )
        .unwrap();
        let repo = GitRepo {
            root: temp.path().to_path_buf(),
        };

        reset_moved_cloud_metadata(&repo, "task/moved").unwrap();

        let content = fs::read_to_string(pointer).unwrap();
        assert!(!content.contains("cloud:"));
        assert!(!content.contains("version_id:"));
        assert!(content.contains("md5: directory.dir"));
        assert!(content.contains("relpath: alpha.txt"));
        assert!(content.contains("path: moved"));
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
        assert_eq!(stored_object(&stores, "0c/../../x", ""), None);
    }

    #[test]
    fn object_stores_follow_the_engine_configuration() {
        let temp = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: temp.path().to_path_buf(),
        };
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
            vec![engine.join("cache")]
        );
        config.s3.as_mut().unwrap().url = "/srv/storage".to_owned();
        assert_eq!(
            local_object_stores(&repo, &config),
            vec![engine.join("cache"), PathBuf::from("/srv/storage")]
        );
        config.s3.as_mut().unwrap().url = "../storage".to_owned();
        fs::write(
            engine.join("config.local"),
            "[core]\n    dir = ignored\n[cache]\n    type = copy\n    dir = /shared/cache\n",
        )
        .unwrap();
        assert_eq!(
            local_object_stores(&repo, &config),
            vec![PathBuf::from("/shared/cache"), engine.join("../storage")]
        );
    }
}
