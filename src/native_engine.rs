//! Native storage hashing, metadata, cache and local materialization.
//! Network archive/version policy lives in the native transport adapters.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::legacy_dvc::{TreeEntry, directory_digest, tree_bytes, tree_manifest_bytes};
use crate::storage_format::{Checksum, Entry, Kind, Manifest, Version};
use md5::{Digest, Md5};
use serde_json::{Value, json};
use walkdir::WalkDir;

use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::path::{reject_symlink_traversal, repo_path, resolved_under, to_slash};
use crate::process::CommandOutput;
use crate::storage_metadata;

#[derive(Debug, Clone)]
pub(crate) struct StorageEntry {
    pub pointer: String,
    pub object: String,
    pub md5: Option<String>,
    pub size: Option<u64>,
    pub version_id: Option<String>,
    pub etag: Option<String>,
    pub hash_name: String,
}

#[derive(Debug, Clone)]
pub(crate) struct CasSource {
    pub key: String,
    pub version_id: Option<String>,
    pub etag: String,
    pub size: u64,
    pub checksum: Checksum,
}

pub(crate) fn fetch_cas_to_cache(
    repo: &GitRepo,
    client: &crate::native_s3::S3Client,
    source: &CasSource,
) -> Result<PathBuf> {
    source.checksum.validate()?;
    if source
        .version_id
        .as_deref()
        .is_some_and(|version| version.trim().is_empty() || version == "null")
    {
        return Err(Error::message(
            "legacy CAS source has an invalid pinned exact version ID",
        ));
    }
    if source.etag.trim_matches('"').is_empty() {
        return Err(Error::message("legacy CAS source has no pinned ETag"));
    }
    let mut request = json!({"Bucket":client.bucket,"Key":source.key,"IfMatch":source.etag});
    if let Some(version) = &source.version_id {
        request["VersionId"] = version.clone().into();
    }
    let matches = |value: &Value| {
        value["DeleteMarker"] != true
            && value["ContentLength"].as_u64() == Some(source.size)
            && value["ETag"]
                .as_str()
                .is_some_and(|etag| etag.trim_matches('"') == source.etag.trim_matches('"'))
            && source
                .version_id
                .as_deref()
                .is_none_or(|version| value["VersionId"].as_str() == Some(version))
    };
    if !matches(&client.call_s3("head_object", &request, None)?.value) {
        return Err(Error::message(
            "legacy CAS source changed after its import inventory was recorded",
        ));
    }
    let cache =
        cache_path_with_algorithm(repo, &source.checksum.digest, &source.checksum.algorithm)?;
    let root = cache_root(repo)?;
    fs::create_dir_all(&root).at(&root)?;
    let temporary = tempfile::NamedTempFile::new_in(&root).at(&root)?;
    let fetched = client.get_to_file(&request, temporary.path())?;
    if !matches(&fetched.value)
        || fs::metadata(temporary.path()).at(temporary.path())?.len() != source.size
        || file_digest(temporary.path(), &source.checksum.algorithm)? != source.checksum.digest
    {
        return Err(Error::message(
            "legacy CAS GET differs from its pinned ETag, exact version, physical size or checksum",
        ));
    }
    fs::create_dir_all(cache.parent().expect("cache parent")).at(&cache)?;
    temporary.persist(&cache).map_err(|error| Error::Io {
        path: cache.clone(),
        source: error.error,
    })?;
    Ok(cache)
}

pub(crate) fn upload_verified(
    repo: &GitRepo,
    client: &crate::native_s3::S3Client,
    entry: &StorageEntry,
    source: &Path,
) -> Result<Version> {
    let raw_sha256 = file_sha256(source)?;
    let condition = import_destination_condition(repo, client, entry)?;
    let (id, etag) = upload_version_in(
        client,
        repo,
        entry,
        source,
        5 * (1 << 30),
        64 * (1 << 20),
        UploadPolicy {
            namespace: "storage-import-uploads",
            condition: condition.as_deref(),
            raw_sha256: Some(&raw_sha256),
        },
    )?;
    let mut bound = entry.clone();
    bound.version_id = Some(id.clone());
    install_cache_for_entry(repo, &bound, source)?;
    if file_sha256(&cache_path_for_entry(repo, &bound)?)? != raw_sha256 {
        return Err(Error::message(
            "storage import source changed while installing its exact-version cache",
        ));
    }
    Ok(Version {
        id,
        etag: Some(etag),
    })
}

pub(crate) fn is_historical_cas_read(repo: &GitRepo, pointers: &[String]) -> Result<bool> {
    Ok(!pointers.is_empty() && historical_cas_pointers(repo, pointers)?.len() == pointers.len())
}

pub(crate) fn historical_cas_pointers(repo: &GitRepo, pointers: &[String]) -> Result<Vec<String>> {
    if !pointers.iter().any(|pointer| pointer.ends_with(".dvc")) {
        return Ok(Vec::new());
    }
    if !crate::legacy_dvc::is_content_addressed_checkout(repo)? {
        return Ok(Vec::new());
    }
    let mut selected = Vec::new();
    for pointer in pointers {
        if !pointer.ends_with(".dvc") {
            continue;
        }
        reject_symlink_traversal(&repo.root, pointer, "legacy CAS metadata")?;
        let raw = fs::read_to_string(repo.root.join(pointer)).at(repo.root.join(pointer))?;
        let yaml: serde_yaml::Value =
            serde_yaml::from_str(&raw).map_err(|error| Error::message(error.to_string()))?;
        let has_bindings = yaml["outs"].as_sequence().into_iter().flatten().any(|out| {
            out.get("cloud").is_some()
                || out
                    .get("files")
                    .and_then(serde_yaml::Value::as_sequence)
                    .is_some_and(|files| files.iter().any(|file| file.get("cloud").is_some()))
        });
        if !has_bindings {
            selected.push(pointer.clone());
        }
    }
    Ok(selected)
}

pub(crate) fn resolve_cas_source(
    client: &crate::native_s3::S3Client,
    digest: &str,
    algorithm: &str,
) -> Result<CasSource> {
    let mut selected = None;
    for object in crate::legacy_dvc::cas_key_candidates(digest, algorithm)? {
        let key = client.key_for(&object);
        let info = match client.call_s3(
            "head_object",
            &json!({"Bucket":client.bucket,"Key":key}),
            None,
        ) {
            Ok(response) => response.value,
            Err(error) if matches!(error.code.as_str(), "NoSuchKey" | "NotFound" | "404") => {
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let size = info["ContentLength"]
            .as_u64()
            .ok_or_else(|| Error::message("legacy CAS HEAD has no physical size"))?;
        let etag = info["ETag"]
            .as_str()
            .filter(|etag| !etag.trim_matches('"').is_empty())
            .ok_or_else(|| Error::message("legacy CAS HEAD has no ETag"))?
            .to_owned();
        let source = CasSource {
            key,
            version_id: info["VersionId"]
                .as_str()
                .filter(|version| !version.is_empty() && *version != "null")
                .map(str::to_owned),
            etag,
            size,
            checksum: Checksum {
                algorithm: algorithm.into(),
                digest: digest.trim_end_matches(".dir").into(),
            },
        };
        if selected.as_ref().is_some_and(|previous: &CasSource| {
            previous.size != source.size
                || previous.etag.trim_matches('"') != source.etag.trim_matches('"')
        }) {
            return Err(Error::message(
                "legacy CAS object layouts contain conflicting source identities",
            ));
        }
        if selected.is_none() {
            selected = Some(source);
        }
    }
    selected
        .ok_or_else(|| Error::message("legacy CAS object is absent from every supported layout"))
}

pub(crate) fn fetch_historical_cas(repo: &GitRepo, pointers: &[String]) -> Result<Option<Value>> {
    if !is_historical_cas_read(repo, pointers)? {
        return Ok(None);
    }
    let client = crate::native_s3::S3Client::historical_cas_from_repo(repo)?.ok_or_else(|| {
        Error::message("historical metadata has no content-addressed source remote")
    })?;
    let mut checked = BTreeSet::new();
    for pointer in pointers {
        let raw = fs::read_to_string(repo.root.join(pointer)).at(repo.root.join(pointer))?;
        let algorithm = storage_metadata::hash_algorithm(&raw, pointer)?;
        let document = storage_metadata::parse_pointer_document(&raw, pointer)?;
        let [out] = document.outs.as_slice() else {
            return Err(Error::message(
                "historical CAS pointer must define exactly one output",
            ));
        };
        let digest = out
            .md5
            .as_deref()
            .ok_or_else(|| Error::message("historical CAS pointer has no checksum"))?;
        let files = if digest.ends_with(".dir") {
            let source = resolve_cas_source(&client, digest, &algorithm)?;
            let mut directory_source = source.clone();
            directory_source.checksum.algorithm = "md5".into();
            let temporary_cache = fetch_cas_to_cache(repo, &client, &directory_source)?;
            let bytes = fs::read(&temporary_cache).at(&temporary_cache)?;
            let files = crate::legacy_dvc::parse_directory_manifest(&bytes, digest)?;
            if out
                .files
                .as_ref()
                .is_some_and(|inline| directory_digest(inline).ok().as_deref() != Some(digest))
            {
                return Err(Error::message(
                    "historical CAS inline directory inventory differs from its aggregate",
                ));
            }
            atomic_write(
                &cache_path_with_algorithm(repo, digest, &algorithm)?,
                &bytes,
            )?;
            files
                .into_iter()
                .map(|file| (file.relpath, file.md5.unwrap(), file.size))
                .collect::<Vec<_>>()
        } else {
            vec![(String::new(), digest.to_owned(), out.size)]
        };
        let mut total = 0u64;
        for (relative, digest, declared_size) in files {
            let source = resolve_cas_source(&client, &digest, &algorithm)?;
            if declared_size.is_some_and(|size| size != source.size) {
                return Err(Error::message(
                    "historical CAS physical size differs from its metadata",
                ));
            }
            total = total
                .checked_add(source.size)
                .ok_or_else(|| Error::message("historical CAS size overflows"))?;
            fetch_cas_to_cache(repo, &client, &source)?;
            checked.insert(if relative.is_empty() {
                output_object(pointer, &out.path)?
            } else {
                format!("{}/{}", output_object(pointer, &out.path)?, relative)
            });
        }
        if out.size.is_some_and(|size| size != total) {
            return Err(Error::message(
                "historical CAS output physical size differs from its metadata",
            ));
        }
    }
    Ok(Some(
        json!({"mode":"legacy-content-addressed","checked_objects":checked}),
    ))
}

pub(crate) fn verify_import_destination(
    repo: &GitRepo,
    client: &crate::native_s3::S3Client,
    entry: &StorageEntry,
) -> Result<bool> {
    import_destination_condition(repo, client, entry).map(|condition| condition.is_some())
}

fn import_destination_condition(
    repo: &GitRepo,
    client: &crate::native_s3::S3Client,
    entry: &StorageEntry,
) -> Result<Option<String>> {
    let local = crate::local_state::directory_unmigrated(repo)?;
    reject_symlink_traversal(
        &local,
        "storage-import-uploads",
        "private storage import uploads",
    )?;
    let directory = local.join("storage-import-uploads");
    let key = client.key_for(&entry.object);
    let mut receipts = BTreeMap::new();
    if directory.is_dir() {
        for item in fs::read_dir(&directory).at(&directory)? {
            let path = item.at(&directory)?.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            if path.is_symlink() || !path.is_file() {
                return Err(Error::message(
                    "private import upload receipt is not a regular file",
                ));
            }
            let journal: Value =
                serde_json::from_slice(&fs::read(&path).at(&path)?).map_err(|error| {
                    Error::message(format!("invalid private import upload receipt: {error}"))
                })?;
            let context = &journal["context"];
            if context["bucket"] != client.bucket || context["key"] != key {
                continue;
            }
            let token = journal["token"]
                .as_str()
                .filter(|token| {
                    token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
                .ok_or_else(|| {
                    Error::message("private import upload receipt has no valid ownership token")
                })?;
            let checksum = Checksum {
                algorithm: context["hash_name"]
                    .as_str()
                    .ok_or_else(|| {
                        Error::message("private import upload receipt has no checksum algorithm")
                    })?
                    .into(),
                digest: context["md5"]
                    .as_str()
                    .ok_or_else(|| Error::message("private import upload receipt has no checksum"))?
                    .into(),
            };
            checksum.validate()?;
            let owned_entry = StorageEntry {
                pointer: entry.pointer.clone(),
                object: entry.object.clone(),
                md5: Some(checksum.digest),
                size: Some(context["size"].as_u64().ok_or_else(|| {
                    Error::message("private import upload receipt has no physical size")
                })?),
                version_id: None,
                etag: None,
                hash_name: checksum.algorithm,
            };
            let raw_sha256 = context["raw_sha256"]
                .as_str()
                .ok_or_else(|| Error::message("private import upload receipt has no raw SHA256"))?;
            let (expected_path, expected_context) = upload_context(
                repo,
                client,
                &owned_entry,
                "storage-import-uploads",
                Some(raw_sha256),
            )?;
            if path != expected_path
                || context != &expected_context
                || receipts
                    .insert(token.to_owned(), (owned_entry, raw_sha256.to_owned()))
                    .is_some()
            {
                return Err(Error::message(
                    "private import upload receipt identity does not match its destination",
                ));
            }
        }
    }
    let rows = client
        .list_versions(&key)?
        .into_iter()
        .filter(|row| row["Key"] == key)
        .collect::<Vec<_>>();
    if rows.is_empty() {
        return Ok(None);
    }
    let mut used = BTreeSet::new();
    let mut latest = None;
    for row in rows {
        if row["delete_marker"] == true {
            return Err(Error::message(
                "storage import destination has an unowned delete marker",
            ));
        }
        let version = row["VersionId"]
            .as_str()
            .ok_or_else(|| Error::message("storage import destination has no exact version ID"))?;
        let info = client
            .call_s3(
                "head_object",
                &json!({"Bucket":client.bucket,"Key":key,"VersionId":version}),
                None,
            )?
            .value;
        let token = info["Metadata"][UPLOAD_TOKEN].as_str().ok_or_else(|| {
            Error::message("storage import destination already has unowned object history")
        })?;
        let (owned_entry, raw_sha256) = receipts.get(token).ok_or_else(|| {
            Error::message("storage import destination has no matching private ownership receipt")
        })?;
        if !used.insert(token.to_owned()) {
            return Err(Error::message(
                "storage import ownership token matches conflicting destination versions",
            ));
        }
        verify_uploaded_version(client, repo, owned_entry, token, version, Some(raw_sha256))?;
        if row["IsLatest"] == true {
            if latest.is_some() {
                return Err(Error::message(
                    "storage import destination has conflicting latest versions",
                ));
            }
            latest = info["ETag"].as_str().map(str::to_owned);
        }
    }
    latest.map(Some).ok_or_else(|| {
        Error::message("storage import destination has no verified latest ownership binding")
    })
}

#[derive(Debug, Clone)]
struct FileState {
    relpath: String,
    md5: String,
    size: u64,
    version_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) enum Operation {
    #[cfg(test)]
    Initialize,
    Track {
        paths: Vec<String>,
    },
    Record {
        pointers: Vec<String>,
    },
    Upload {
        pointers: Vec<String>,
    },
    Fetch {
        pointers: Vec<String>,
    },
    Materialize {
        pointers: Vec<String>,
    },
    Move {
        source: String,
        destination: String,
    },
    Untrack {
        pointers: Vec<String>,
    },
    Status {
        pointers: Vec<String>,
        cloud: bool,
        quiet: bool,
    },
    Changes {
        outputs: Vec<String>,
    },
}

pub(crate) fn execute(cwd: &Path, operation: &Operation) -> Result<CommandOutput> {
    #[cfg(feature = "test-storage")]
    if let Some(hook) = std::env::var_os("WORKSPACE_MGR_TEST_STORAGE_HOOK") {
        let hook = hook
            .to_str()
            .ok_or_else(|| Error::message("storage test hook is not UTF-8"))?;
        crate::process::run(hook, operation.test_arguments(), cwd)?;
    }
    let repo = GitRepo {
        root: cwd.canonicalize().at(cwd)?,
    };
    match execute_inner(&repo, operation) {
        Ok((code, stdout)) => Ok(CommandOutput {
            code,
            stdout,
            stderr: String::new(),
        }),
        Err(error) => Ok(CommandOutput {
            code: 1,
            stdout: String::new(),
            stderr: error.to_string(),
        }),
    }
}

#[cfg(feature = "test-storage")]
impl Operation {
    fn test_arguments(&self) -> Vec<String> {
        let (name, paths) = match self {
            #[cfg(test)]
            Self::Initialize => ("init", Vec::new()),
            Self::Track { paths } => ("add", paths.clone()),
            Self::Record { pointers } => ("commit", pointers.clone()),
            Self::Upload { pointers } => ("push", pointers.clone()),
            Self::Fetch { pointers } => ("fetch", pointers.clone()),
            Self::Materialize { pointers } => ("checkout", pointers.clone()),
            Self::Move {
                source,
                destination,
            } => ("move", vec![source.clone(), destination.clone()]),
            Self::Untrack { pointers } => ("remove", pointers.clone()),
            Self::Status { pointers, .. } => ("status", pointers.clone()),
            Self::Changes { outputs } => ("data", outputs.clone()),
        };
        std::iter::once(name.to_owned())
            .chain(std::iter::once("--".to_owned()))
            .chain(paths)
            .collect()
    }
}

fn execute_inner(repo: &GitRepo, operation: &Operation) -> Result<(i32, String)> {
    match operation {
        #[cfg(test)]
        Operation::Initialize => initialize(repo)?,
        Operation::Track { paths } => {
            for path in paths {
                add(repo, path)?;
            }
        }
        Operation::Record { pointers } => {
            for pointer in select_pointers(repo, pointers)? {
                commit(repo, &pointer)?;
            }
        }
        Operation::Upload { pointers } => push(repo, &select_pointers(repo, pointers)?)?,
        Operation::Fetch { pointers } => fetch(repo, &select_pointers(repo, pointers)?)?,
        Operation::Materialize { pointers } => checkout(repo, &select_pointers(repo, pointers)?)?,
        Operation::Move {
            source,
            destination,
        } => move_output(repo, source, destination)?,
        Operation::Untrack { pointers } => {
            for pointer in select_pointers(repo, pointers)? {
                remove(repo, &pointer)?;
            }
        }
        Operation::Status {
            pointers,
            cloud,
            quiet,
        } => return status(repo, &select_pointers(repo, pointers)?, *cloud, *quiet),
        Operation::Changes { outputs } => {
            return Ok((
                0,
                serde_json::to_string(&data_status(repo, outputs)?)
                    .map_err(|error| Error::message(error.to_string()))?,
            ));
        }
    }
    Ok((0, String::new()))
}

#[cfg(test)]
fn initialize(repo: &GitRepo) -> Result<()> {
    let local = crate::local_state::directory_unmigrated(repo)?;
    for path in [local.join("cache"), local.join("uploads")] {
        fs::create_dir_all(&path).at(&path)?;
    }
    Ok(())
}

pub(crate) fn cache_root(repo: &GitRepo) -> Result<PathBuf> {
    let local = crate::local_state::directory_unmigrated(repo)?;
    reject_symlink_traversal(&local, "cache", "native storage cache")?;
    let path = local.join("cache");
    if path.exists() && !path.is_dir() {
        return Err(Error::message("storage cache is not a directory"));
    }
    Ok(path)
}

fn digest_parts(digest: &str) -> Result<(&str, &str)> {
    let md5 = digest.strip_suffix(".dir").unwrap_or(digest);
    if md5.len() != 32 || !md5.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(Error::message(format!(
            "invalid storage MD5 digest: {digest:?}"
        )));
    }
    Ok(digest.split_at(2))
}

pub(crate) fn cache_path(repo: &GitRepo, digest: &str) -> Result<PathBuf> {
    cache_path_with_algorithm(repo, digest, "md5")
}

pub(crate) fn cache_path_with_algorithm(
    repo: &GitRepo,
    digest: &str,
    hash_name: &str,
) -> Result<PathBuf> {
    let (prefix, rest) = digest_parts(digest)?;
    let root = cache_root(repo)?;
    let path = match hash_name {
        "md5" => format!("objects/md5/{prefix}/{rest}"),
        "md5-dos2unix" => format!("objects/md5-dos2unix/{prefix}/{rest}"),
        _ => return Err(Error::message("unsupported storage hash algorithm")),
    };
    reject_symlink_traversal(&root, &path, "storage cache object")?;
    Ok(root.join(path))
}

#[cfg(test)]
pub(crate) fn existing_cache(repo: &GitRepo, digest: &str) -> Result<PathBuf> {
    existing_cache_with_algorithm(repo, digest, "md5")
}

pub(crate) fn existing_cache_with_algorithm(
    repo: &GitRepo,
    digest: &str,
    hash_name: &str,
) -> Result<PathBuf> {
    let preferred = if hash_name == "md5" {
        cache_path(repo, digest)?
    } else {
        cache_path_with_algorithm(repo, digest, hash_name)?
    };
    if preferred.is_file() {
        return Ok(preferred);
    }
    Ok(crate::legacy_dvc::existing_cache(&repo.root, digest, hash_name).unwrap_or(preferred))
}

/// Normalized text checksums do not identify raw bytes. Exact bindings must
/// therefore never reuse an unbound hash cache or another object's generation.
pub(crate) fn cache_path_for_entry(repo: &GitRepo, entry: &StorageEntry) -> Result<PathBuf> {
    let digest = entry
        .md5
        .as_deref()
        .ok_or_else(|| Error::message("storage cache entry has no checksum"))?;
    let Some(version) = entry
        .version_id
        .as_deref()
        .filter(|_| entry.hash_name == "md5-dos2unix")
    else {
        return existing_cache_with_algorithm(repo, digest, &entry.hash_name);
    };
    Checksum {
        algorithm: entry.hash_name.clone(),
        digest: digest.into(),
    }
    .validate()?;
    if version.trim().is_empty() || version == "null" {
        return Err(Error::message(
            "storage cache entry has no immutable exact version",
        ));
    }
    let identity = crate::hex::encode_lower(sha2::Sha256::digest(
        serde_json::to_vec(&json!([
            crate::native_s3::S3Client::cache_route(repo)?,
            entry.object,
            version,
            digest
        ]))
        .map_err(|error| Error::message(error.to_string()))?,
    ));
    let relative = format!("versions/md5-dos2unix/{identity}");
    let root = cache_root(repo)?;
    reject_symlink_traversal(&root, &relative, "exact-version storage cache")?;
    Ok(root.join(relative))
}

pub(crate) fn install_cache_for_entry(
    repo: &GitRepo,
    entry: &StorageEntry,
    source: &Path,
) -> Result<()> {
    let digest = entry
        .md5
        .as_deref()
        .ok_or_else(|| Error::message("storage cache entry has no checksum"))?;
    if file_digest(source, &entry.hash_name)? != digest
        || entry.size.is_some_and(|size| {
            fs::metadata(source).map(|metadata| metadata.len()).ok() != Some(size)
        })
    {
        return Err(Error::message("downloaded content size or hash mismatch"));
    }
    let destination = if entry.hash_name == "md5-dos2unix" && entry.version_id.is_some() {
        cache_path_for_entry(repo, entry)?
    } else {
        cache_path_with_algorithm(repo, digest, &entry.hash_name)?
    };
    atomic_copy(source, &destination)
}

pub(crate) fn exact_raw_bytes_match(
    repo: &GitRepo,
    entry: &StorageEntry,
    local: &Path,
) -> Result<bool> {
    if entry.hash_name != "md5-dos2unix" || entry.version_id.is_none() {
        return Ok(true);
    }
    let cache = cache_path_for_entry(repo, entry)?;
    Ok(cache.is_file() && local.is_file() && file_sha256(local)? == file_sha256(&cache)?)
}

pub(crate) fn normalized_exact_cache_missing(repo: &GitRepo, pointer: &str) -> Result<bool> {
    if pointer_algorithm(repo, pointer)? != "md5-dos2unix" {
        return Ok(false);
    }
    for entry in metadata_entries(repo, None, &[pointer.into()])? {
        if entry.version_id.is_some() && !cache_path_for_entry(repo, &entry)?.is_file() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn cache_for_recorded_file(
    repo: &GitRepo,
    object: &str,
    file: &FileState,
    algorithm: &str,
) -> Result<PathBuf> {
    cache_path_for_entry(
        repo,
        &StorageEntry {
            pointer: String::new(),
            object: if file.relpath.is_empty() {
                object.into()
            } else {
                format!("{object}/{}", file.relpath)
            },
            md5: Some(file.md5.clone()),
            size: Some(file.size),
            version_id: file.version_id.clone(),
            etag: None,
            hash_name: algorithm.into(),
        },
    )
}

fn normalized_exact_bytes_match(
    repo: &GitRepo,
    object: &str,
    files: &[FileState],
    algorithm: &str,
) -> Result<bool> {
    if algorithm != "md5-dos2unix" {
        return Ok(true);
    }
    for file in files.iter().filter(|file| file.version_id.is_some()) {
        let cache = cache_for_recorded_file(repo, object, file, algorithm)?;
        let local = if file.relpath.is_empty() {
            repo.root.join(object)
        } else {
            repo.root.join(object).join(&file.relpath)
        };
        if !cache.is_file() || !local.is_file() || file_sha256(&local)? != file_sha256(&cache)? {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn file_digest(path: &Path, hash_name: &str) -> Result<String> {
    let mut file = fs::File::open(path).at(path)?;
    stream_digest(&mut file, path, hash_name)
}

fn file_sha256(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).at(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => hasher.update(&buffer[..size]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(source) => {
                return Err(Error::Io {
                    path: path.to_owned(),
                    source,
                });
            }
        }
    }
    Ok(crate::hex::encode_lower(hasher.finalize()))
}

/// Reproduces DVC's legacy per-1-MiB text heuristic, including CRLF that
/// straddles a chunk boundary. Raw MD5 never normalizes content.
pub(crate) fn stream_digest(input: &mut impl Read, path: &Path, hash_name: &str) -> Result<String> {
    if !matches!(hash_name, "md5" | "md5-dos2unix") {
        return Err(Error::message(format!(
            "unsupported storage hash algorithm: {hash_name}"
        )));
    }
    let mut hasher = Md5::new();
    let legacy = hash_name == "md5-dos2unix";
    let mut buffer = vec![0u8; if legacy { 1024 * 1024 } else { 128 * 1024 }];
    loop {
        let mut size = 0;
        while size < buffer.len() {
            match input.read(&mut buffer[size..]) {
                Ok(0) => break,
                Ok(count) => size += count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(source) => {
                    return Err(Error::Io {
                        path: path.to_owned(),
                        source,
                    });
                }
            }
        }
        if size == 0 {
            break;
        }
        if !legacy {
            hasher.update(&buffer[..size]);
            continue;
        }
        let sample = &buffer[..size.min(512)];
        let nontext = sample
            .iter()
            .filter(|&&byte| !matches!(byte, 32..=126 | b'\n' | b'\r' | b'\t' | b'\x0c' | b'\x08'))
            .count();
        let text = !sample.contains(&0) && nontext * 10 <= sample.len() * 3;
        if !text {
            hasher.update(&buffer[..size]);
            continue;
        }
        let mut normalized = Vec::with_capacity(size);
        for (index, &byte) in buffer[..size].iter().enumerate() {
            if byte != b'\r' || index + 1 == size || buffer[index + 1] != b'\n' {
                normalized.push(byte);
            }
        }
        hasher.update(&normalized);
    }
    Ok(crate::hex::encode_lower(hasher.finalize()))
}

#[cfg(test)]
pub(crate) fn install_cache(repo: &GitRepo, digest: &str, bytes: &[u8]) -> Result<()> {
    install_cache_with_algorithm(repo, digest, bytes, "md5")
}

fn install_cache_with_algorithm(
    repo: &GitRepo,
    digest: &str,
    bytes: &[u8],
    hash_name: &str,
) -> Result<()> {
    let wanted = digest.strip_suffix(".dir").unwrap_or(digest);
    if crate::hex::encode_lower(Md5::digest(bytes)) != wanted {
        return Err(Error::message("downloaded content hash mismatch"));
    }
    atomic_write(&cache_path_with_algorithm(repo, digest, hash_name)?, bytes)
}

#[cfg(test)]
pub(crate) fn install_cache_file(repo: &GitRepo, digest: &str, path: &Path) -> Result<()> {
    install_cache_file_with_algorithm(repo, digest, path, "md5")
}

pub(crate) fn install_cache_file_with_algorithm(
    repo: &GitRepo,
    digest: &str,
    path: &Path,
    hash_name: &str,
) -> Result<()> {
    let wanted = digest.strip_suffix(".dir").unwrap_or(digest);
    if file_digest(path, hash_name)? != wanted {
        return Err(Error::message("downloaded content hash mismatch"));
    }
    atomic_copy(path, &cache_path_with_algorithm(repo, digest, hash_name)?)
}

fn read_manifest(repo: &GitRepo, pointer: &str) -> Result<Manifest> {
    if !pointer.ends_with(crate::storage_format::SUFFIX) {
        return Err(Error::message(
            "legacy DVC metadata must be migrated before mutation; run `workspace-mgr manage`",
        ));
    }
    reject_symlink_traversal(&repo.root, pointer, "storage metadata")?;
    let path = resolved_under(&repo.root, pointer);
    if !fs::symlink_metadata(&path).at(&path)?.is_file() {
        return Err(Error::message("storage metadata must be a regular file"));
    }
    Manifest::parse(&fs::read_to_string(&path).at(&path)?, pointer)
}

fn write_manifest(repo: &GitRepo, pointer: &str, manifest: &Manifest) -> Result<()> {
    if !pointer.ends_with(crate::storage_format::SUFFIX) {
        return Err(Error::message(
            "native storage writes require a .wm-storage.json sidecar",
        ));
    }
    atomic_write(
        &resolved_under(&repo.root, pointer),
        manifest.serialize()?.as_bytes(),
    )
}

fn output_object(pointer: &str, path: &str) -> Result<String> {
    let parent = Path::new(pointer).parent().unwrap_or_else(|| Path::new(""));
    repo_path(&to_slash(&parent.join(path)), "storage output")
}

fn pointer_algorithm(repo: &GitRepo, pointer: &str) -> Result<String> {
    reject_symlink_traversal(&repo.root, pointer, "storage metadata")?;
    let path = repo.root.join(pointer);
    storage_metadata::hash_algorithm(&fs::read_to_string(&path).at(&path)?, pointer)
}

pub(crate) fn metadata_entries(
    repo: &GitRepo,
    revision: Option<&str>,
    pointers: &[String],
) -> Result<Vec<StorageEntry>> {
    let mut entries = Vec::new();
    for pointer in pointers {
        repo_path(pointer, "storage metadata")?;
        let raw = match revision {
            Some(revision) => repo.run(["show", &format!("{revision}:{pointer}")])?.stdout,
            None => {
                reject_symlink_traversal(&repo.root, pointer, "storage metadata")?;
                fs::read_to_string(repo.root.join(pointer)).at(repo.root.join(pointer))?
            }
        };
        let parsed = storage_metadata::parse_pointer_document(&raw, pointer)?;
        let hash_name = storage_metadata::hash_algorithm(&raw, pointer)?;
        if parsed.outs.is_empty() {
            return Err(Error::message(format!(
                "storage metadata has no outputs: {pointer}"
            )));
        }
        for out in parsed.outs {
            if let Some(digest) = &out.md5 {
                digest_parts(digest)?;
            }
            let object = output_object(pointer, &out.path)?;
            if let Some(files) = &out.files {
                if pointer.ends_with(".dvc")
                    && out.md5.as_deref() != Some(directory_digest(files)?.as_str())
                {
                    return Err(Error::message(format!(
                        "legacy directory manifest hash mismatch: {pointer}"
                    )));
                }
                for file in files {
                    if let Some(digest) = &file.md5 {
                        digest_parts(digest)?;
                        if digest.ends_with(".dir") {
                            return Err(Error::message(
                                "directory manifest cannot contain another directory digest",
                            ));
                        }
                    }
                    entries.push(StorageEntry {
                        pointer: pointer.clone(),
                        object: repo_path(
                            &format!("{object}/{}", file.relpath),
                            "stored directory file",
                        )?,
                        md5: file.md5.clone(),
                        size: file.size,
                        version_id: file.version_id.clone(),
                        etag: file.etag.clone(),
                        hash_name: hash_name.clone(),
                    });
                }
            } else if out
                .md5
                .as_deref()
                .is_some_and(|digest| digest.ends_with(".dir"))
            {
                return Err(Error::message(format!(
                    "directory metadata is incomplete: {pointer}; restore its published file/version manifest"
                )));
            } else {
                entries.push(StorageEntry {
                    pointer: pointer.clone(),
                    object,
                    md5: out.md5,
                    size: out.size,
                    version_id: out.version_id,
                    etag: out.etag,
                    hash_name: hash_name.clone(),
                });
            }
        }
    }
    Ok(entries)
}

pub(crate) fn install_directory_manifests(repo: &GitRepo, pointers: &[String]) -> Result<()> {
    for pointer in pointers {
        if pointer.ends_with(crate::storage_format::SUFFIX) {
            read_manifest(repo, pointer)?;
            continue;
        }
        let algorithm = pointer_algorithm(repo, pointer)?;
        for out in storage_metadata::read_pointer_document(repo, pointer)?.outs {
            if let Some(files) = out.files {
                let bytes = tree_manifest_bytes(&files)?;
                let digest = format!("{}.dir", crate::hex::encode_lower(Md5::digest(&bytes)));
                if out.md5.as_deref() != Some(&digest) {
                    return Err(Error::message("legacy directory manifest hash mismatch"));
                }
                install_cache_with_algorithm(repo, &digest, &bytes, &algorithm)?;
            }
        }
    }
    Ok(())
}

fn select_pointers(repo: &GitRepo, targets: &[String]) -> Result<Vec<String>> {
    if targets.is_empty() {
        return storage_metadata::discover(repo, &[]);
    }
    let mut pointers = BTreeSet::new();
    for target in targets {
        let target = repo_path(target, "storage target")?;
        let pointer = if storage_metadata::is_pointer(&target) {
            target.clone()
        } else {
            let native = storage_metadata::pointer_path(&target);
            if repo.root.join(&native).is_file() {
                native
            } else {
                format!("{target}.dvc")
            }
        };
        if repo.root.join(&pointer).is_file() {
            pointers.insert(pointer);
        } else {
            for pointer in storage_metadata::discover(repo, std::slice::from_ref(&target))? {
                pointers.insert(pointer);
            }
        }
    }
    if pointers.is_empty() {
        return Err(Error::message(
            "storage targets did not select any metadata",
        ));
    }
    Ok(pointers.into_iter().collect())
}

#[cfg(test)]
fn current_files(repo: &GitRepo, object: &str) -> Result<Vec<FileState>> {
    current_files_with_algorithm(repo, object, "md5")
}

fn current_files_with_algorithm(
    repo: &GitRepo,
    object: &str,
    hash_name: &str,
) -> Result<Vec<FileState>> {
    reject_symlink_traversal(&repo.root, object, "storage output")?;
    let root = repo.root.join(object);
    let mut files = Vec::new();
    if !root.exists() {
        return Ok(files);
    }
    for item in WalkDir::new(&root).follow_links(false).sort_by_file_name() {
        let item =
            item.map_err(|e| Error::message(format!("failed to inspect storage output: {e}")))?;
        let metadata = fs::metadata(item.path()).at(item.path())?;
        // File symlinks record their target bytes. Directory links cannot
        // be traversed without ambiguous ownership or recursion, so refuse
        // them; reading a file target never mutates the target itself.
        if (item.file_type().is_symlink() && !metadata.is_file())
            || (!metadata.is_dir() && !metadata.is_file())
        {
            return Err(Error::message(
                "storage output contains a symlink or special file",
            ));
        }
        if metadata.is_dir() {
            continue;
        }
        let relpath = if root.is_dir() {
            let relative = item.path().strip_prefix(&root).expect("walk root");
            if relative.to_str().is_none() {
                return Err(Error::message(
                    "stored directory contains a filename that is not UTF-8",
                ));
            }
            to_slash(relative)
        } else {
            String::new()
        };
        if !relpath.is_empty() {
            repo_path(&relpath, "stored directory file")?;
        }
        files.push(FileState {
            relpath,
            md5: file_digest(item.path(), hash_name)?,
            size: metadata.len(),
            version_id: None,
        });
    }
    files.sort_by(|a, b| a.relpath.cmp(&b.relpath));
    Ok(files)
}

fn recorded_files(
    repo: &GitRepo,
    pointer: &str,
    out: &storage_metadata::PointerOutput,
    algorithm: &str,
) -> Result<Vec<FileState>> {
    if let Some(files) = &out.files {
        if pointer.ends_with(".dvc")
            && out.md5.as_deref() != Some(directory_digest(files)?.as_str())
        {
            return Err(Error::message(format!(
                "directory manifest hash mismatch: {pointer}"
            )));
        }
        return files
            .iter()
            .map(|file| {
                Ok(FileState {
                    relpath: repo_path(&file.relpath, "directory manifest entry")?,
                    md5: file
                        .md5
                        .clone()
                        .ok_or_else(|| Error::message("directory file has no digest"))?,
                    size: file.size.unwrap_or(0),
                    version_id: file.version_id.clone(),
                })
            })
            .collect();
    }
    let digest = out
        .md5
        .as_deref()
        .ok_or_else(|| Error::message(format!("metadata has no content digest: {pointer}")))?;
    if !digest.ends_with(".dir") {
        return Ok(vec![FileState {
            relpath: String::new(),
            md5: digest.to_owned(),
            size: out.size.unwrap_or(0),
            version_id: out.version_id.clone(),
        }]);
    }
    let mut path = existing_cache_with_algorithm(repo, digest, algorithm)?;
    if !path.is_file() {
        if let Ok(remote) = remote_root(repo) {
            if !remote.contains("://") {
                path = remote_cache_path(&remote, digest, repo, algorithm)?;
            }
        }
    }
    let bytes = fs::read(&path).at(&path)?;
    if crate::hex::encode_lower(Md5::digest(&bytes)) != digest.trim_end_matches(".dir") {
        return Err(Error::message("cached directory manifest hash mismatch"));
    }
    let files: Vec<TreeEntry> = serde_json::from_slice(&bytes)
        .map_err(|e| Error::message(format!("invalid cached directory manifest: {e}")))?;
    tree_bytes(&files)?;
    files
        .into_iter()
        .map(|file| {
            let path = existing_cache_with_algorithm(repo, &file.md5, algorithm)?;
            Ok(FileState {
                relpath: file.relpath,
                md5: file.md5,
                size: fs::metadata(path).map(|m| m.len()).unwrap_or(0),
                version_id: None,
            })
        })
        .collect()
}

fn add(repo: &GitRepo, target: &str) -> Result<()> {
    let object = repo_path(target, "storage output")?;
    storage_metadata::require_addressable(
        &object,
        "storage output",
        "choose a path without backslashes",
    )?;
    let pointer = storage_metadata::pointer_path(&object);
    reject_symlink_traversal(&repo.root, &pointer, "storage metadata")?;
    if repo.root.join(format!("{object}.dvc")).exists() {
        return Err(Error::message(
            "legacy DVC metadata must be migrated before tracking this boundary; run `workspace-mgr manage`",
        ));
    }
    let parent = Path::new(&object).parent().unwrap_or_else(|| Path::new(""));
    let filename = Path::new(&object)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::message("output name is not UTF-8"))?;
    let mut manifest = if repo.root.join(&pointer).exists() {
        read_manifest(repo, &pointer)?
    } else {
        Manifest {
            schema_version: 1,
            path: filename.to_owned(),
            kind: Kind::File,
            checksum: Checksum {
                algorithm: "md5".into(),
                digest: "00000000000000000000000000000000".into(),
            },
            size: 0,
            version: None,
            entries: None,
        }
    };
    update_manifest(repo, &pointer, &mut manifest)?;
    write_manifest(repo, &pointer, &manifest)?;
    update_ignore(&repo.root.join(parent).join(".gitignore"), filename, true)
}

fn commit(repo: &GitRepo, pointer: &str) -> Result<()> {
    let mut manifest = read_manifest(repo, pointer)?;
    update_manifest(repo, pointer, &mut manifest)?;
    write_manifest(repo, pointer, &manifest)
}

fn update_manifest(repo: &GitRepo, pointer: &str, manifest: &mut Manifest) -> Result<()> {
    let object = output_object(pointer, &manifest.path)?;
    let path = repo.root.join(&object);
    if !path.exists() {
        return Err(Error::message(format!(
            "storage output is missing: {object}"
        )));
    }
    if manifest.checksum.algorithm == "md5-dos2unix" {
        let document = storage_metadata::read_pointer_document(repo, pointer)?;
        let [out] = document.outs.as_slice() else {
            return Err(Error::message(
                "normalized storage metadata must define one output",
            ));
        };
        let recorded = recorded_files(repo, pointer, out, "md5-dos2unix")?;
        let unchanged = recorded.iter().all(|file| file.version_id.is_some())
            && normalized_exact_bytes_match(repo, &object, &recorded, "md5-dos2unix")?;
        if !unchanged {
            manifest.checksum.algorithm = "md5".into();
            manifest.clear_versions();
        }
    }
    let files = current_files_with_algorithm(repo, &object, &manifest.checksum.algorithm)?;
    let size = files.iter().try_fold(0u64, |total, file| {
        total
            .checked_add(file.size)
            .ok_or_else(|| Error::message("storage content sizes overflow"))
    })?;
    let (kind, digest, entries) = if path.is_dir() {
        let previous = manifest.entries.as_deref().unwrap_or_default();
        let entries = files
            .iter()
            .map(|file| Entry {
                path: file.relpath.clone(),
                checksum: Checksum {
                    algorithm: manifest.checksum.algorithm.clone(),
                    digest: file.md5.clone(),
                },
                size: file.size,
                version: previous
                    .iter()
                    .find(|entry| {
                        entry.path == file.relpath
                            && entry.checksum.digest == file.md5
                            && entry.size == file.size
                    })
                    .and_then(|entry| entry.version.clone()),
            })
            .collect::<Vec<_>>();
        (
            Kind::Directory,
            crate::storage_format::directory_digest(&entries)?,
            Some(entries),
        )
    } else {
        (
            Kind::File,
            files
                .first()
                .ok_or_else(|| Error::message("storage output is not a file"))?
                .md5
                .clone(),
            None,
        )
    };
    if manifest.kind != kind || manifest.checksum.digest != digest || manifest.size != size {
        manifest.version = None;
    }
    manifest.kind = kind;
    manifest.checksum.digest = digest;
    manifest.size = size;
    manifest.entries = entries;
    for file in files {
        let source = if file.relpath.is_empty() {
            path.clone()
        } else {
            path.join(&file.relpath)
        };
        install_cache_file_with_algorithm(repo, &file.md5, &source, &manifest.checksum.algorithm)?;
    }
    Ok(())
}

fn actual_directory_digest(pointer: &str, files: &[FileState], algorithm: &str) -> Result<String> {
    if pointer.ends_with(".dvc") {
        return Ok(format!(
            "{}.dir",
            crate::hex::encode_lower(Md5::digest(tree_bytes(
                &files
                    .iter()
                    .map(|file| TreeEntry {
                        relpath: file.relpath.clone(),
                        md5: file.md5.clone()
                    })
                    .collect::<Vec<_>>()
            )?))
        ));
    }
    let entries = files
        .iter()
        .map(|file| Entry {
            path: file.relpath.clone(),
            checksum: Checksum {
                algorithm: algorithm.to_owned(),
                digest: file.md5.clone(),
            },
            size: file.size,
            version: None,
        })
        .collect::<Vec<_>>();
    Ok(format!(
        "{}.dir",
        crate::storage_format::directory_digest(&entries)?
    ))
}

fn remote_root(repo: &GitRepo) -> Result<String> {
    storage_metadata::internal_location(repo)?
        .map(|location| location.0)
        .ok_or_else(|| Error::message("storage remote is not configured"))
}

fn remote_cache_path(
    remote: &str,
    digest: &str,
    repo: &GitRepo,
    hash_name: &str,
) -> Result<PathBuf> {
    let (first, rest) = digest_parts(digest)?;
    let root = Path::new(remote);
    let root = if root.is_absolute() {
        root.to_owned()
    } else {
        repo.root.join(root)
    };
    let relative = match hash_name {
        "md5" => format!("objects/md5/{first}/{rest}"),
        "md5-dos2unix" => format!("objects/md5-dos2unix/{first}/{rest}"),
        _ => return Err(Error::message("unsupported storage hash algorithm")),
    };
    reject_symlink_traversal(&root, &relative, "filesystem storage object")?;
    Ok(root.join(relative))
}

fn algorithms_for_pointer(repo: &GitRepo, pointer: &str) -> Result<BTreeMap<String, String>> {
    let algorithm = pointer_algorithm(repo, pointer)?;
    let mut hashes = BTreeMap::new();
    for out in storage_metadata::read_pointer_document(repo, pointer)?.outs {
        if pointer.ends_with(".dvc") {
            if let Some(digest) = &out.md5 {
                hashes.insert(digest.clone(), algorithm.clone());
            }
        }
        for file in recorded_files(repo, pointer, &out, &algorithm)? {
            hashes.insert(file.md5, algorithm.clone());
        }
    }
    Ok(hashes)
}

fn push(repo: &GitRepo, pointers: &[String]) -> Result<()> {
    let remote = remote_root(repo)?;
    if remote.starts_with("s3://") {
        return push_versioned(repo, pointers);
    }
    if remote.contains("://") {
        return Err(Error::message("unsupported storage remote scheme"));
    }
    for pointer in pointers {
        for (digest, algorithm) in algorithms_for_pointer(repo, pointer)? {
            let source = existing_cache_with_algorithm(repo, &digest, &algorithm)?;
            if file_digest(&source, &algorithm)? != digest.trim_end_matches(".dir") {
                return Err(Error::message("storage cache content hash mismatch"));
            }
            atomic_copy(
                &source,
                &remote_cache_path(&remote, &digest, repo, &algorithm)?,
            )?;
        }
    }
    Ok(())
}

fn push_versioned(repo: &GitRepo, pointers: &[String]) -> Result<()> {
    let client = crate::native_s3::S3Client::from_repo(repo)?;
    let versioning = client.call_s3(
        "get_bucket_versioning",
        &json!({"Bucket":client.bucket}),
        None,
    )?;
    if versioning.value["Status"] != "Enabled" {
        return Err(Error::message(
            "version-aware storage requires enabled S3 bucket versioning",
        ));
    }
    for pointer in pointers {
        let entries = metadata_entries(repo, None, std::slice::from_ref(pointer))?;
        for entry in entries {
            let key = client.key_for(&entry.object);
            let digest = entry
                .md5
                .as_deref()
                .ok_or_else(|| Error::message("stored object has no content hash"))?;
            if let Some(version) = &entry.version_id {
                let info = client.call_s3(
                    "head_object",
                    &json!({"Bucket":client.bucket,"Key":key,"VersionId":version}),
                    None,
                )?;
                if info.value["VersionId"].as_str() != Some(version)
                    || entry
                        .size
                        .is_some_and(|size| info.value["ContentLength"].as_u64() != Some(size))
                    || entry.etag.as_deref().is_some_and(|etag| {
                        info.value["ETag"].as_str().map(|v| v.trim_matches('"'))
                            != Some(etag.trim_matches('"'))
                    })
                {
                    return Err(Error::message("stored exact version differs from metadata"));
                }
                continue;
            }
            let source = existing_cache_with_algorithm(repo, digest, &entry.hash_name)?;
            if file_digest(&source, &entry.hash_name)? != digest {
                return Err(Error::message("storage cache content hash mismatch"));
            }
            let (version, etag) = upload_version(
                &client,
                repo,
                &entry,
                &source,
                5 * (1 << 30),
                64 * (1 << 20),
            )?;
            let mut manifest = read_manifest(repo, pointer)?;
            let object = output_object(pointer, &manifest.path)?;
            let mut bound = false;
            if let Some(entries) = &mut manifest.entries {
                for file in entries {
                    if format!("{object}/{}", file.path) == entry.object {
                        if !binding_is_unchanged(
                            &file.checksum,
                            file.size,
                            file.version.as_ref(),
                            &entry,
                        ) {
                            return Err(Error::message(
                                "storage metadata changed during upload; retry after reconciling the pointer",
                            ));
                        }
                        file.version = Some(Version {
                            id: version.clone(),
                            etag: Some(etag.clone()),
                        });
                        bound = true;
                    }
                }
            } else if object == entry.object {
                if !binding_is_unchanged(
                    &manifest.checksum,
                    manifest.size,
                    manifest.version.as_ref(),
                    &entry,
                ) {
                    return Err(Error::message(
                        "storage metadata changed during upload; retry after reconciling the pointer",
                    ));
                }
                manifest.version = Some(Version {
                    id: version,
                    etag: Some(etag),
                });
                bound = true;
            }
            if !bound {
                return Err(Error::message(
                    "storage output changed during upload; its exact version remains recorded in the private upload journal",
                ));
            }
            write_manifest(repo, pointer, &manifest)?;
        }
    }
    Ok(())
}

fn binding_is_unchanged(
    checksum: &Checksum,
    size: u64,
    version: Option<&Version>,
    entry: &StorageEntry,
) -> bool {
    Some(checksum.digest.as_str()) == entry.md5.as_deref()
        && checksum.algorithm == entry.hash_name
        && Some(size) == entry.size
        && version.map(|version| version.id.as_str()) == entry.version_id.as_deref()
        && version.and_then(|version| version.etag.as_deref()) == entry.etag.as_deref()
}

const UPLOAD_TOKEN: &str = "workspace-mgr-upload";

fn upload_journal(
    repo: &GitRepo,
    client: &crate::native_s3::S3Client,
    entry: &StorageEntry,
) -> Result<(PathBuf, Value)> {
    upload_journal_in(repo, client, entry, "uploads", None)
}

fn upload_journal_in(
    repo: &GitRepo,
    client: &crate::native_s3::S3Client,
    entry: &StorageEntry,
    namespace: &str,
    raw_sha256: Option<&str>,
) -> Result<(PathBuf, Value)> {
    let (path, context) = upload_context(repo, client, entry, namespace, raw_sha256)?;
    if path.is_file() {
        let journal: Value = serde_json::from_slice(&fs::read(&path).at(&path)?)
            .map_err(|e| Error::message(format!("invalid private storage upload journal: {e}")))?;
        if journal["context"] != context || !journal["token"].is_string() {
            return Err(Error::message(
                "private storage upload journal does not match this object",
            ));
        }
        return Ok((path, journal));
    }
    let parent = path.parent().expect("journal parent");
    fs::create_dir_all(parent).at(parent)?;
    let nonce = tempfile::Builder::new()
        .prefix("upload-")
        .rand_bytes(32)
        .tempfile_in(parent)
        .at(parent)?;
    use sha2::Sha256;
    let token = crate::hex::encode_lower(Sha256::digest(
        nonce
            .path()
            .file_name()
            .expect("nonce filename")
            .as_encoded_bytes(),
    ));
    let journal = json!({"context":context,"token":token,"phase":"planned"});
    save_upload(&path, &journal)?;
    Ok((path, journal))
}

fn upload_context(
    repo: &GitRepo,
    client: &crate::native_s3::S3Client,
    entry: &StorageEntry,
    namespace: &str,
    raw_sha256: Option<&str>,
) -> Result<(PathBuf, Value)> {
    use sha2::Sha256;
    let key = client.key_for(&entry.object);
    let digest = entry
        .md5
        .as_deref()
        .ok_or_else(|| Error::message("storage upload has no digest"))?;
    let mut context = json!({"schema":1,"bucket":client.bucket,"key":key,"md5":digest,"hash_name":entry.hash_name,"size":entry.size});
    if namespace == "storage-import-uploads" {
        let raw_sha256 = raw_sha256
            .filter(|digest| {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            })
            .ok_or_else(|| {
                Error::message("storage import upload requires a raw SHA256 identity")
            })?;
        context["schema"] = 2.into();
        context["raw_sha256"] = raw_sha256.into();
    }
    let identity = crate::hex::encode_lower(Sha256::digest(
        serde_json::to_vec(&context).map_err(|e| Error::message(e.to_string()))?,
    ));
    let relative = format!("{namespace}/{identity}.json");
    let local = crate::local_state::directory_unmigrated(repo)?;
    reject_symlink_traversal(&local, &relative, "private storage upload journal")?;
    let path = local.join(relative);
    Ok((path, context))
}

fn save_upload(path: &Path, journal: &Value) -> Result<()> {
    atomic_write(
        path,
        &serde_json::to_vec_pretty(journal).map_err(|e| Error::message(e.to_string()))?,
    )
}

fn verify_uploaded_version(
    client: &crate::native_s3::S3Client,
    _repo: &GitRepo,
    entry: &StorageEntry,
    token: &str,
    version: &str,
    raw_sha256: Option<&str>,
) -> Result<String> {
    let key = client.key_for(&entry.object);
    let info = client
        .call_s3(
            "head_object",
            &json!({"Bucket":client.bucket,"Key":key,"VersionId":version}),
            None,
        )?
        .value;
    if info["VersionId"].as_str() != Some(version)
        || info["Metadata"][UPLOAD_TOKEN].as_str() != Some(token)
        || entry
            .size
            .is_some_and(|size| info["ContentLength"].as_u64() != Some(size))
    {
        return Err(Error::message(
            "uploaded exact version does not match its private ownership journal",
        ));
    }
    let temporary = tempfile::NamedTempFile::new().at(std::env::temp_dir())?;
    let fetched = client.get_to_file(
        &json!({"Bucket":client.bucket,"Key":key,"VersionId":version,"IfMatch":info["ETag"]}),
        temporary.path(),
    )?;
    if fetched.value["VersionId"].as_str() != Some(version)
        || fetched.value["DeleteMarker"] == true
        || fetched.value["ETag"] != info["ETag"]
        || fetched.value["ContentLength"] != info["ContentLength"]
        || entry
            .size
            .is_some_and(|size| fetched.value["ContentLength"].as_u64() != Some(size))
    {
        return Err(Error::message(
            "uploaded exact-version GET response differs from its verified HEAD",
        ));
    }
    if file_digest(temporary.path(), &entry.hash_name)? != entry.md5.as_deref().unwrap_or("") {
        return Err(Error::message(
            "uploaded exact version has a content hash mismatch; its private journal is retained for reconciliation",
        ));
    }
    if let Some(wanted) = raw_sha256 {
        if file_sha256(temporary.path())? != wanted {
            return Err(Error::message(
                "uploaded exact version has a raw content hash mismatch; its private journal is retained for reconciliation",
            ));
        }
    }
    info["ETag"]
        .as_str()
        .map(|etag| etag.trim_matches('"').to_owned())
        .ok_or_else(|| Error::message("uploaded exact version has no ETag"))
}

fn recover_uploaded_version(
    client: &crate::native_s3::S3Client,
    repo: &GitRepo,
    entry: &StorageEntry,
    journal: &Value,
) -> Result<Option<(String, String)>> {
    let key = client.key_for(&entry.object);
    let token = journal["token"]
        .as_str()
        .ok_or_else(|| Error::message("private upload journal has no ownership token"))?;
    let mut found = Vec::new();
    for row in client.list_versions(&key)? {
        if row["Key"] != key || row["delete_marker"] == true {
            continue;
        }
        let version = row["VersionId"]
            .as_str()
            .ok_or_else(|| Error::message("upload recovery listing has no exact ID"))?;
        let info = client
            .call_s3(
                "head_object",
                &json!({"Bucket":client.bucket,"Key":key,"VersionId":version}),
                None,
            )?
            .value;
        if info["Metadata"][UPLOAD_TOKEN].as_str() == Some(token) {
            found.push(version.to_owned());
        }
    }
    if found.len() > 1 {
        return Err(Error::message(
            "private upload ownership token matches multiple exact versions; refusing to duplicate or erase their history",
        ));
    }
    found
        .into_iter()
        .next()
        .map(|version| {
            verify_uploaded_version(
                client,
                repo,
                entry,
                token,
                &version,
                journal["context"]["raw_sha256"].as_str(),
            )
            .map(|etag| (version, etag))
        })
        .transpose()
}

fn upload_version(
    client: &crate::native_s3::S3Client,
    repo: &GitRepo,
    entry: &StorageEntry,
    source: &Path,
    single_limit: u64,
    preferred_part_size: u64,
) -> Result<(String, String)> {
    upload_version_in(
        client,
        repo,
        entry,
        source,
        single_limit,
        preferred_part_size,
        UploadPolicy {
            namespace: "uploads",
            condition: None,
            raw_sha256: None,
        },
    )
}

struct UploadPolicy<'a> {
    namespace: &'a str,
    condition: Option<&'a str>,
    raw_sha256: Option<&'a str>,
}

fn upload_version_in(
    client: &crate::native_s3::S3Client,
    repo: &GitRepo,
    entry: &StorageEntry,
    source: &Path,
    single_limit: u64,
    preferred_part_size: u64,
    policy: UploadPolicy<'_>,
) -> Result<(String, String)> {
    let UploadPolicy {
        namespace,
        condition,
        raw_sha256,
    } = policy;
    let (path, mut journal) = if namespace == "uploads" {
        upload_journal(repo, client, entry)?
    } else {
        upload_journal_in(repo, client, entry, namespace, raw_sha256)?
    };
    let key = client.key_for(&entry.object);
    let token = journal["token"]
        .as_str()
        .ok_or_else(|| Error::message("private upload journal has no ownership token"))?
        .to_owned();
    if let Some(version) = journal["version_id"].as_str() {
        return verify_uploaded_version(client, repo, entry, &token, version, raw_sha256)
            .map(|etag| (version.to_owned(), etag));
    }
    if journal["phase"] != "planned" {
        if let Some((version, etag)) = recover_uploaded_version(client, repo, entry, &journal)? {
            journal["version_id"] = version.clone().into();
            journal["etag"] = etag.clone().into();
            journal["phase"] = "complete".into();
            save_upload(&path, &journal)?;
            return Ok((version, etag));
        }
        if journal["phase"] == "creating" {
            return Err(Error::message(
                "a multipart creation response was lost; reconcile the possibly outstanding upload before retrying this object",
            ));
        }
        if let Some(upload) = journal["upload_id"].as_str() {
            match client.call_s3(
                "abort_multipart_upload",
                &json!({"Bucket":client.bucket,"Key":key,"UploadId":upload}),
                None,
            ) {
                Ok(_) => {}
                Err(error) if error.code == "NoSuchUpload" => {}
                Err(error) => return Err(error.into()),
            }
            journal
                .as_object_mut()
                .expect("journal object")
                .remove("upload_id");
        }
    }
    let size = fs::metadata(source).at(source)?.len();
    if entry.size.is_some_and(|expected| size != expected) {
        return Err(Error::message(
            "cached upload size differs from storage metadata",
        ));
    }
    if file_digest(source, &entry.hash_name)? != entry.md5.as_deref().unwrap_or("") {
        return Err(Error::message(
            "cached upload checksum differs from storage metadata",
        ));
    }
    let request = json!({"Bucket":client.bucket,"Key":key,"Metadata":{UPLOAD_TOKEN:token}});
    let response = if size <= single_limit {
        journal["phase"] = "uploading".into();
        save_upload(&path, &journal)?;
        let mut request = request;
        if namespace == "storage-import-uploads" {
            if let Some(etag) = condition {
                request["IfMatch"] = etag.into();
            } else {
                request["IfNoneMatch"] = "*".into();
            }
        }
        request["ExpectedMD5"] = file_digest(source, "md5")?.into();
        request["ExpectedSize"] = size.into();
        client.put_file(&request, source)
    } else {
        let part_size = preferred_part_size.max(size.div_ceil(10_000));
        if part_size > 5 * (1 << 30) || size > 5 * (1 << 40) {
            return Err(Error::message(
                "stored object exceeds S3 multipart upload limits",
            ));
        }
        journal["phase"] = "creating".into();
        save_upload(&path, &journal)?;
        let created = client.call_s3("create_multipart_upload", &request, None)?;
        let upload = created.value["UploadId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| Error::message("multipart creation returned no upload ID"))?
            .to_owned();
        journal["upload_id"] = upload.clone().into();
        journal["phase"] = "uploading".into();
        save_upload(&path, &journal)?;
        let result = (|| {
            let mut parts = Vec::new();
            let mut offset = 0;
            while offset < size {
                let length = (size - offset).min(part_size);
                let part = parts.len() + 1;
                let response = client.upload_part_file(
                    &json!({"Bucket":client.bucket,"Key":key,"UploadId":upload,"PartNumber":part}),
                    source,
                    offset,
                    length,
                )?;
                let etag = response.value["ETag"]
                    .as_str()
                    .ok_or_else(|| Error::message("multipart part returned no ETag"))?;
                parts.push(json!({"PartNumber":part,"ETag":etag}));
                offset += length;
            }
            if file_digest(source, &entry.hash_name)? != entry.md5.as_deref().unwrap_or("") {
                return Err(Error::message(
                    "cached content changed during multipart upload",
                ));
            }
            journal["phase"] = "completing".into();
            save_upload(&path, &journal)?;
            let mut complete = json!({"Bucket":client.bucket,"Key":key,"UploadId":upload,"MultipartUpload":{"Parts":parts}});
            if namespace == "storage-import-uploads" {
                if let Some(etag) = condition {
                    complete["IfMatch"] = etag.into();
                } else {
                    complete["IfNoneMatch"] = "*".into();
                }
            }
            client
                .call_s3("complete_multipart_upload", &complete, None)
                .map_err(Error::from)
        })();
        match result {
            Ok(response) => Ok(response),
            Err(error) => {
                // A failed completion may already have created its exact
                // version; recovery proves its bytes and ownership first.
                if let Some((version, etag)) =
                    recover_uploaded_version(client, repo, entry, &journal)?
                {
                    journal["version_id"] = version.clone().into();
                    journal["etag"] = etag.clone().into();
                    journal["phase"] = "complete".into();
                    save_upload(&path, &journal)?;
                    return Ok((version, etag));
                }
                match client.call_s3(
                    "abort_multipart_upload",
                    &json!({"Bucket":client.bucket,"Key":key,"UploadId":upload}),
                    None,
                ) {
                    Ok(_) => {}
                    Err(abort) if abort.code == "NoSuchUpload" => {}
                    Err(abort) => {
                        return Err(Error::message(format!(
                            "multipart upload failed ({error}); owned upload could not be aborted ({abort})"
                        )));
                    }
                }
                journal
                    .as_object_mut()
                    .expect("journal object")
                    .remove("upload_id");
                journal["phase"] = "planned".into();
                save_upload(&path, &journal)?;
                return Err(error);
            }
        }
    };
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            if let Some((version, etag)) = recover_uploaded_version(client, repo, entry, &journal)?
            {
                journal["version_id"] = version.clone().into();
                journal["etag"] = etag.clone().into();
                journal["phase"] = "complete".into();
                save_upload(&path, &journal)?;
                return Ok((version, etag));
            }
            return Err(error.into());
        }
    };
    let version = response.value["VersionId"].as_str().filter(|id|!id.is_empty() && *id != "null").ok_or_else(||Error::message("S3 upload returned no exact version ID; the private upload journal will recover it on retry"))?.to_owned();
    // Record the exact generation before its read-back, including a failure.
    journal["version_id"] = version.clone().into();
    save_upload(&path, &journal)?;
    let etag = verify_uploaded_version(client, repo, entry, &token, &version, raw_sha256)?;
    journal["etag"] = etag.clone().into();
    journal["phase"] = "complete".into();
    save_upload(&path, &journal)?;
    Ok((version, etag))
}

fn fetch(repo: &GitRepo, pointers: &[String]) -> Result<()> {
    let remote = remote_root(repo)?;
    if remote.starts_with("s3://") {
        return Err(Error::message(
            "version-aware S3 fetching must use the exact-version adapter",
        ));
    }
    for pointer in pointers {
        let document = storage_metadata::read_pointer_document(repo, pointer)?;
        let algorithm = pointer_algorithm(repo, pointer)?;
        for out in &document.outs {
            let algorithm = algorithm.as_str();
            let digest = out
                .md5
                .as_deref()
                .ok_or_else(|| Error::message("metadata has no content hash"))?;
            if pointer.ends_with(".dvc") && digest.ends_with(".dir") {
                install_cache_file_with_algorithm(
                    repo,
                    digest,
                    &remote_cache_path(&remote, digest, repo, algorithm)?,
                    algorithm,
                )?;
            }
            for file in recorded_files(repo, pointer, out, algorithm)? {
                install_cache_file_with_algorithm(
                    repo,
                    &file.md5,
                    &remote_cache_path(&remote, &file.md5, repo, algorithm)?,
                    algorithm,
                )?;
            }
        }
    }
    Ok(())
}

fn checkout(repo: &GitRepo, pointers: &[String]) -> Result<()> {
    // The Rust caller verifies existing outputs against the previous pointer
    // before authorizing checkout. Stage complete boundaries before replacing
    // any of them, so incoming revisions also retire removed directory files.
    let mut staged = Vec::new();
    for pointer in pointers {
        let algorithm = pointer_algorithm(repo, pointer)?;
        for out in storage_metadata::read_pointer_document(repo, pointer)?.outs {
            let hash_name = algorithm.as_str();
            let object = output_object(pointer, &out.path)?;
            reject_symlink_traversal(&repo.root, &object, "storage checkout")?;
            let root = repo.root.join(&object);
            let current = if root.exists() {
                current_files_with_algorithm(repo, &object, hash_name)?
            } else {
                Vec::new()
            };
            let directory = out.md5.as_deref().is_some_and(|m| m.ends_with(".dir"));
            let files = recorded_files(repo, pointer, &out, hash_name)?;
            let mut cached_size = 0u64;
            for file in &files {
                let source = cache_for_recorded_file(repo, &object, file, hash_name)?;
                let size = fs::metadata(&source).at(&source)?.len();
                cached_size = cached_size
                    .checked_add(size)
                    .ok_or_else(|| Error::message("cached content size overflows"))?;
                if (file.size != 0 && size != file.size)
                    || file_digest(&source, hash_name)? != file.md5
                {
                    return Err(Error::message("cached content size or hash mismatch"));
                }
            }
            if out.size.is_some_and(|size| cached_size != size) {
                return Err(Error::message("cached output size differs from metadata"));
            }
            // A matching boundary stays byte-for-byte as it is, including
            // file links, mode bits and legacy CRLF representation.
            if root.exists() && root.is_dir() == directory {
                let digest = if directory {
                    actual_directory_digest(pointer, &current, hash_name)?
                } else {
                    current
                        .first()
                        .map(|file| file.md5.clone())
                        .unwrap_or_default()
                };
                if out.md5.as_deref() == Some(&digest)
                    && out.size.is_none_or(|size| {
                        current.iter().map(|file| file.size).sum::<u64>() == size
                    })
                    && normalized_exact_bytes_match(repo, &object, &files, hash_name)?
                {
                    continue;
                }
            }
            if staged
                .iter()
                .any(|(previous, _, _): &(PathBuf, tempfile::TempDir, PathBuf)| {
                    root.starts_with(previous) || previous.starts_with(&root)
                })
            {
                return Err(Error::message(
                    "storage checkout contains overlapping outputs",
                ));
            }
            let parent = root
                .parent()
                .ok_or_else(|| Error::message("storage output has no parent"))?;
            fs::create_dir_all(parent).at(parent)?;
            let temporary = tempfile::Builder::new()
                .prefix(".workspace-mgr-checkout-")
                .tempdir_in(parent)
                .at(parent)?;
            let replacement = temporary.path().join("output");
            if directory {
                fs::create_dir(&replacement).at(&replacement)?;
            }
            for file in files {
                let source = cache_for_recorded_file(repo, &object, &file, hash_name)?;
                let existing = if file.relpath.is_empty() {
                    root.clone()
                } else {
                    root.join(&file.relpath)
                };
                let destination = if file.relpath.is_empty() {
                    replacement.clone()
                } else {
                    replacement.join(&file.relpath)
                };
                let relative =
                    to_slash(existing.strip_prefix(&repo.root).expect("validated output"));
                let relative_parent = Path::new(&relative)
                    .parent()
                    .map(to_slash)
                    .unwrap_or_default();
                reject_symlink_traversal(&repo.root, &relative_parent, "storage checkout")?;
                let source_size = fs::metadata(&source).at(&source)?.len();
                if (file.size != 0 && source_size != file.size)
                    || file_digest(&source, hash_name)? != file.md5
                {
                    return Err(Error::message("cached content size or hash mismatch"));
                }
                atomic_copy(&source, &destination)?;
                if fs::metadata(&destination).at(&destination)?.len() != source_size
                    || file_digest(&destination, hash_name)? != file.md5
                {
                    return Err(Error::message("cached content changed during checkout"));
                }
                if let Ok(metadata) = fs::metadata(&existing) {
                    if metadata.is_file() {
                        fs::set_permissions(&destination, metadata.permissions())
                            .at(&destination)?;
                    }
                }
            }
            staged.push((root, temporary, replacement));
        }
    }
    let mut installed: Vec<usize> = Vec::new();
    for index in 0..staged.len() {
        let (root, temporary, replacement) = &staged[index];
        let backup = temporary.path().join("previous");
        let result = (|| {
            if root.exists() {
                fs::rename(root, &backup).at(root)?;
            }
            if let Err(error) = fs::rename(replacement, root).at(root) {
                if backup.exists() {
                    fs::rename(&backup, root).at(root)?;
                }
                return Err(error);
            }
            installed.push(index);
            fs::File::open(root.parent().expect("validated parent"))
                .at(root)?
                .sync_all()
                .at(root)
        })();
        if let Err(error) = result {
            for previous in installed.into_iter().rev() {
                let (root, temporary, _) = &staged[previous];
                if root.is_dir() {
                    fs::remove_dir_all(root).at(root)?;
                } else {
                    fs::remove_file(root).at(root)?;
                }
                let backup = temporary.path().join("previous");
                if backup.exists() {
                    fs::rename(&backup, root).at(root)?;
                }
            }
            return Err(error);
        }
    }
    Ok(())
}

fn status(repo: &GitRepo, pointers: &[String], cloud: bool, quiet: bool) -> Result<(i32, String)> {
    let mut result = serde_json::Map::new();
    for pointer in pointers {
        let mut changed = serde_json::Map::new();
        let algorithm = pointer_algorithm(repo, pointer)?;
        for out in storage_metadata::read_pointer_document(repo, pointer)?.outs {
            let algorithm = algorithm.as_str();
            let object = output_object(pointer, &out.path)?;
            let digest = out
                .md5
                .as_deref()
                .ok_or_else(|| Error::message("metadata has no digest"))?;
            let issue = if cloud {
                let remote = remote_root(repo)?;
                if remote.starts_with("s3://") {
                    return Err(Error::message(
                        "version-aware remote status requires the exact-version adapter",
                    ));
                }
                if algorithms_for_pointer(repo, pointer).is_ok_and(|hashes| {
                    hashes.iter().all(|(hash, algorithm)| {
                        remote_cache_path(&remote, hash, repo, algorithm).is_ok_and(|path| {
                            path.is_file()
                                && file_digest(&path, algorithm)
                                    .is_ok_and(|d| d == hash.trim_end_matches(".dir"))
                        })
                    })
                }) {
                    None
                } else {
                    Some("not in remote")
                }
            } else if !repo.root.join(&object).exists() {
                Some("deleted")
            } else if repo.root.join(&object).is_dir() != digest.ends_with(".dir") {
                Some("modified")
            } else {
                let current = current_files_with_algorithm(repo, &object, algorithm)?;
                let actual = if digest.ends_with(".dir") {
                    actual_directory_digest(pointer, &current, algorithm)?
                } else {
                    current
                        .first()
                        .map(|file| file.md5.clone())
                        .unwrap_or_default()
                };
                if actual != digest {
                    Some("modified")
                } else if !normalized_exact_bytes_match(
                    repo,
                    &object,
                    &recorded_files(repo, pointer, &out, algorithm)?,
                    algorithm,
                )? {
                    Some("modified")
                } else if !recorded_files(repo, pointer, &out, algorithm).is_ok_and(|files| {
                    files.iter().all(|file| {
                        cache_for_recorded_file(repo, &object, file, algorithm).is_ok_and(|path| {
                            path.is_file()
                                && file_digest(&path, algorithm).is_ok_and(|d| d == file.md5)
                        })
                    })
                }) {
                    Some("not in cache")
                } else {
                    None
                }
            };
            if let Some(issue) = issue {
                changed.insert(object, json!(issue));
            }
        }
        if !changed.is_empty() {
            result.insert(pointer.clone(), json!([{"changed outs":changed}]));
        }
    }
    let code = if quiet && !result.is_empty() { 1 } else { 0 };
    Ok((
        code,
        serde_json::to_string(&result).map_err(|e| Error::message(e.to_string()))?,
    ))
}

fn data_status(repo: &GitRepo, targets: &[String]) -> Result<Value> {
    let mut added = BTreeSet::new();
    let mut modified = BTreeSet::new();
    let mut deleted = BTreeSet::new();
    let mut not_in_cache = BTreeSet::new();
    let mut unknown = BTreeSet::new();
    for pointer in select_pointers(repo, targets)? {
        let algorithm = pointer_algorithm(repo, &pointer)?;
        for out in storage_metadata::read_pointer_document(repo, &pointer)?.outs {
            let algorithm = algorithm.as_str();
            let object = output_object(&pointer, &out.path)?;
            let directory = out.md5.as_deref().is_some_and(|m| m.ends_with(".dir"));
            let label = if directory {
                format!("{object}/")
            } else {
                object.clone()
            };
            let files = match recorded_files(repo, &pointer, &out, algorithm) {
                Ok(files) => files,
                Err(Error::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound && directory =>
                {
                    not_in_cache.insert(label.clone());
                    unknown.insert(label);
                    continue;
                }
                Err(error) => return Err(error),
            };
            let current = current_files_with_algorithm(repo, &object, algorithm)?;
            let old = files
                .iter()
                .map(|f| (f.relpath.clone(), (f.md5.clone(), f.size)))
                .collect::<BTreeMap<_, _>>();
            let new = current
                .iter()
                .map(|f| (f.relpath.clone(), (f.md5.clone(), f.size)))
                .collect::<BTreeMap<_, _>>();
            if old != new || !repo.root.join(&object).exists() {
                if !repo.root.join(&object).exists() {
                    deleted.insert(label.clone());
                } else if directory {
                    modified.insert(label.clone());
                }
            }
            if pointer.ends_with(".dvc") {
                if let Some(digest) = &out.md5 {
                    if !existing_cache_with_algorithm(repo, digest, algorithm)?.is_file() {
                        not_in_cache.insert(label);
                    }
                }
            }
            for file in &files {
                let label = if file.relpath.is_empty() {
                    object.clone()
                } else {
                    format!("{object}/{}", file.relpath)
                };
                if !existing_cache_with_algorithm(repo, &file.md5, algorithm)?.is_file() {
                    not_in_cache.insert(label.clone());
                }
                match new.get(&file.relpath) {
                    None => {
                        deleted.insert(label);
                    }
                    Some(state) if state != &(file.md5.clone(), file.size) => {
                        modified.insert(label);
                    }
                    _ => {}
                }
            }
            for file in &current {
                if !old.contains_key(&file.relpath) {
                    added.insert(if file.relpath.is_empty() {
                        object.clone()
                    } else {
                        format!("{object}/{}", file.relpath)
                    });
                }
            }
        }
    }
    let mut changes = serde_json::Map::new();
    for (key, set) in [
        ("added", added),
        ("modified", modified),
        ("deleted", deleted),
    ] {
        if !set.is_empty() {
            changes.insert(key.to_owned(), json!(set));
        }
    }
    let mut value = serde_json::Map::new();
    if !not_in_cache.is_empty() {
        value.insert("not_in_cache".to_owned(), json!(not_in_cache));
    }
    if !changes.is_empty() {
        value.insert("uncommitted".to_owned(), Value::Object(changes));
    }
    if !unknown.is_empty() {
        value.insert("unknown".to_owned(), json!(unknown));
    }
    Ok(Value::Object(value))
}

fn move_output(repo: &GitRepo, source: &str, destination: &str) -> Result<()> {
    let source = repo_path(source, "storage move source")?;
    let destination = repo_path(destination, "storage move destination")?;
    storage_metadata::require_addressable(
        &destination,
        "storage move destination",
        "choose a path without backslashes",
    )?;
    let old_pointer = storage_metadata::pointer_path(&source);
    let new_pointer = storage_metadata::pointer_path(&destination);
    for path in [&source, &destination, &old_pointer, &new_pointer] {
        reject_symlink_traversal(&repo.root, path, "storage move")?;
    }
    if repo.root.join(&destination).exists() || repo.root.join(&new_pointer).exists() {
        return Err(Error::message("storage move destination already exists"));
    }
    let mut document = read_manifest(repo, &old_pointer)?;
    document.clear_versions();
    document.path = Path::new(&destination)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::message("invalid output name"))?
        .to_owned();
    fs::create_dir_all(
        repo.root
            .join(&destination)
            .parent()
            .expect("destination parent"),
    )
    .at(repo.root.join(&destination))?;
    let materialized = repo.root.join(&source).exists();
    if materialized {
        fs::rename(repo.root.join(&source), repo.root.join(&destination))
            .at(repo.root.join(&source))?;
    }
    if let Err(error) = write_manifest(repo, &new_pointer, &document) {
        if materialized {
            let _ = fs::rename(repo.root.join(&destination), repo.root.join(&source));
        }
        return Err(error);
    }
    fs::remove_file(repo.root.join(&old_pointer)).at(repo.root.join(&old_pointer))?;
    update_ignore(
        &repo
            .root
            .join(&source)
            .parent()
            .expect("output parent")
            .join(".gitignore"),
        Path::new(&source)
            .file_name()
            .and_then(|s| s.to_str())
            .expect("UTF-8"),
        false,
    )?;
    update_ignore(
        &repo
            .root
            .join(&destination)
            .parent()
            .expect("output parent")
            .join(".gitignore"),
        Path::new(&destination)
            .file_name()
            .and_then(|s| s.to_str())
            .expect("UTF-8"),
        true,
    )
}

fn remove(repo: &GitRepo, pointer: &str) -> Result<()> {
    let document = storage_metadata::read_pointer_document(repo, pointer)?;
    for out in document.outs {
        let object = output_object(pointer, &out.path)?;
        update_ignore(
            &repo
                .root
                .join(&object)
                .parent()
                .expect("output parent")
                .join(".gitignore"),
            Path::new(&object)
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| Error::message("invalid output name"))?,
            false,
        )?;
    }
    fs::remove_file(repo.root.join(pointer)).at(repo.root.join(pointer))
}

fn ignore_rule(name: &str) -> String {
    let mut escaped = String::from("/");
    for ch in name.chars() {
        if matches!(ch, '\\' | '*' | '?' | '[' | ']' | '!' | '#') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

fn update_ignore(path: &Path, name: &str, add: bool) -> Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(Error::message("storage ignore file may not be a symlink"));
    }
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(Error::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    let rule = ignore_rule(name);
    let mut lines = raw
        .split_inclusive('\n')
        .filter(|line| line.trim_end_matches(['\r', '\n']) != rule || add)
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if add && !raw.lines().any(|line| line == rule) {
        if !raw.is_empty() && !raw.ends_with('\n') {
            lines.push("\n".to_owned());
        }
        lines.push(format!("{rule}\n"));
    }
    let next = lines.concat();
    if next != raw {
        atomic_write(path, next.as_bytes())?;
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::message("storage path has no parent"))?;
    fs::create_dir_all(parent).at(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent).at(parent)?;
    file.write_all(bytes).at(path)?;
    if let Ok(metadata) = fs::metadata(path) {
        file.as_file()
            .set_permissions(metadata.permissions())
            .at(path)?;
    }
    file.as_file().sync_all().at(path)?;
    file.persist(path).map_err(|e| Error::Io {
        path: path.to_owned(),
        source: e.error,
    })?;
    fs::File::open(parent).at(parent)?.sync_all().at(parent)
}

fn atomic_copy(source: &Path, destination: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| Error::message("storage object has no parent"))?;
    fs::create_dir_all(parent).at(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent).at(parent)?;
    let mut input = fs::File::open(source).at(source)?;
    std::io::copy(&mut input, &mut file).at(destination)?;
    let metadata = fs::metadata(destination)
        .or_else(|_| fs::metadata(source))
        .at(destination)?;
    file.as_file()
        .set_permissions(metadata.permissions())
        .at(destination)?;
    file.as_file().sync_all().at(destination)?;
    file.persist(destination).map_err(|e| Error::Io {
        path: destination.to_owned(),
        source: e.error,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository() -> (tempfile::TempDir, GitRepo) {
        let temporary = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: temporary.path().canonicalize().unwrap(),
        };
        repo.run(["init", "-q"]).unwrap();
        initialize(&repo).unwrap();
        (temporary, repo)
    }

    fn remote(repo: &GitRepo, path: &Path) {
        fs::write(
            repo.root.join(".workspace-mgr.toml"),
            format!(
                "[git]\nremote = \"origin\"\nbranch = \"main\"\n[s3]\nurl = {}\n",
                serde_json::to_string(&path.to_string_lossy()).unwrap()
            ),
        )
        .unwrap();
    }

    #[test]
    fn dos2unix_hash_matches_fixed_dvc_oracles_across_short_reads_and_binary_chunks() {
        let (_temporary, repo) = repository();
        let path = repo.root.join("legacy.txt");
        let boundary = [vec![b'a'; 1024 * 1024 - 1], b"\r\n".to_vec()].concat();
        let over_threshold = [vec![0x80; 154], vec![b'a'; 358], b"\r\n".to_vec()].concat();
        let below_threshold = [vec![0x80; 153], vec![b'a'; 359], b"\r\n".to_vec()].concat();
        let later_binary = [vec![b'a'; 1024 * 1024], b"b\0\r\n".to_vec()].concat();
        let later_text = [
            b"a\0\r\n".to_vec(),
            vec![0; 1024 * 1024 - 4],
            b"x\r\n".to_vec(),
        ]
        .concat();
        let within_chunk = [vec![b'a'; 128 * 1024 - 1], b"\r\nb\rc\r\n\r".to_vec()].concat();
        // Literal digests were obtained from pinned DVC3.67.1 fobj_md5,
        // not from another implementation of the Rust normalization loop.
        for (name, bytes, expected) in [
            (
                "NUL binary",
                b"a\0\r\n".to_vec(),
                "a200e344e12b35719025ffdb8b428ee8",
            ),
            (
                "1-MiB CRLF boundary",
                boundary,
                "2647acae3fed7a4538ecf474e9a98d1e",
            ),
            (
                "above 30-percent nontext",
                over_threshold,
                "2cf181203e54df156ffcb988c5897222",
            ),
            (
                "below 30-percent nontext",
                below_threshold,
                "035aaabce8d6cdfb644cf2dbf29cfb8b",
            ),
            (
                "text",
                b"a\r\nb\r\n".to_vec(),
                "dd8c6a395b5dd36c56d23275028f526c",
            ),
            (
                "binary later chunk",
                later_binary,
                "796ce23f04b52ede614636806c2a3b07",
            ),
            (
                "text later chunk",
                later_text,
                "842b31a117d59ef48c1ce05174fbbbc4",
            ),
            (
                "CRLF within logical chunk",
                within_chunk,
                "8cbb84bd2527ebbd2ca5ea12f049afaf",
            ),
        ] {
            struct ShortReads(std::io::Cursor<Vec<u8>>);
            impl Read for ShortReads {
                fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                    let maximum = buffer.len().min(257);
                    self.0.read(&mut buffer[..maximum])
                }
            }
            fs::write(&path, &bytes).unwrap();
            assert_eq!(
                file_digest(&path, "md5-dos2unix").unwrap(),
                expected,
                "{name}"
            );
            assert_eq!(
                stream_digest(
                    &mut ShortReads(std::io::Cursor::new(bytes.clone())),
                    &path,
                    "md5-dos2unix"
                )
                .unwrap(),
                expected,
                "short reads: {name}"
            );
            assert_eq!(
                file_digest(&path, "md5").unwrap(),
                crate::hex::encode_lower(Md5::digest(&bytes)),
                "raw MD5: {name}"
            );
            assert_eq!(fs::read(&path).unwrap(), bytes, "raw bytes: {name}");
        }
    }

    #[test]
    fn native_add_reports_granular_changes_and_preserves_remote_versions() {
        let (_temporary, repo) = repository();
        fs::create_dir(repo.root.join("data")).unwrap();
        fs::write(repo.root.join("data/a"), "a").unwrap();
        fs::write(repo.root.join("data/z"), "z").unwrap();
        add(&repo, "data").unwrap();
        let mut document = read_manifest(&repo, "data.wm-storage.json").unwrap();
        assert_eq!(document.checksum.algorithm, "md5");
        for (entry, id) in document
            .entries
            .as_mut()
            .unwrap()
            .iter_mut()
            .zip(["old-a", "old-z"])
        {
            entry.version = Some(Version {
                id: id.into(),
                etag: Some(format!("etag-{id}")),
            });
        }
        write_manifest(&repo, "data.wm-storage.json", &document).unwrap();
        assert_eq!(data_status(&repo, &["data".into()]).unwrap(), json!({}));
        fs::write(repo.root.join("data/z"), "new z").unwrap();
        fs::write(repo.root.join("data/new"), "new").unwrap();
        let changes = data_status(&repo, &["data".into()]).unwrap();
        assert_eq!(
            changes["uncommitted"]["modified"],
            json!(["data/", "data/z"])
        );
        assert_eq!(changes["uncommitted"]["added"], json!(["data/new"]));
        commit(&repo, "data.wm-storage.json").unwrap();
        let entries = metadata_entries(&repo, None, &["data.wm-storage.json".into()]).unwrap();
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.object == "data/a")
                .unwrap()
                .version_id
                .as_deref(),
            Some("old-a")
        );
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.object == "data/z")
                .unwrap()
                .version_id,
            None
        );
    }

    #[test]
    fn filesystem_fetch_recovers_directory_after_cache_removal() {
        let (_temporary, repo) = repository();
        let destination = tempfile::tempdir().unwrap();
        remote(&repo, destination.path());
        fs::create_dir(repo.root.join("data")).unwrap();
        fs::write(repo.root.join("data/a"), b"old").unwrap();
        add(&repo, "data").unwrap();
        let pointers = vec!["data.wm-storage.json".into()];
        push(&repo, &pointers).unwrap();
        fs::remove_dir_all(cache_root(&repo).unwrap()).unwrap();
        assert!(data_status(&repo, &["data".into()]).unwrap()["uncommitted"].is_null());
        fs::remove_dir_all(repo.root.join("data")).unwrap();
        fetch(&repo, &pointers).unwrap();
        checkout(&repo, &pointers).unwrap();
        assert_eq!(fs::read(repo.root.join("data/a")).unwrap(), b"old");
        assert_eq!(status(&repo, &pointers, false, false).unwrap().1, "{}");
    }

    #[test]
    fn authorized_checkout_replaces_directory_and_never_applies_corrupt_cache() {
        let (_temporary, repo) = repository();
        fs::create_dir(repo.root.join("data")).unwrap();
        fs::write(repo.root.join("data/a"), "old a").unwrap();
        fs::write(repo.root.join("data/retired"), "retired").unwrap();
        add(&repo, "data").unwrap();
        fs::remove_file(repo.root.join("data/retired")).unwrap();
        fs::write(repo.root.join("data/a"), "new a").unwrap();
        fs::write(repo.root.join("data/new"), "new file").unwrap();
        commit(&repo, "data.wm-storage.json").unwrap();
        fs::write(repo.root.join("data/a"), "old a").unwrap();
        fs::write(repo.root.join("data/retired"), "retired").unwrap();
        fs::remove_file(repo.root.join("data/new")).unwrap();
        let pointers = vec!["data.wm-storage.json".into()];
        checkout(&repo, &pointers).unwrap();
        assert_eq!(
            fs::read_to_string(repo.root.join("data/a")).unwrap(),
            "new a"
        );
        assert!(!repo.root.join("data/retired").exists());
        assert!(repo.root.join("data/new").exists());
        let out = storage_metadata::read_pointer_document(&repo, "data.wm-storage.json")
            .unwrap()
            .outs
            .remove(0);
        let files = recorded_files(&repo, "data.wm-storage.json", &out, "md5").unwrap();
        fs::write(existing_cache(&repo, &files[0].md5).unwrap(), "corrupt").unwrap();
        let before = fs::read(repo.root.join("data/a")).unwrap();
        assert!(checkout(&repo, &pointers).is_err());
        assert_eq!(fs::read(repo.root.join("data/a")).unwrap(), before);
    }

    #[test]
    fn legacy_dos2unix_pointer_is_read_without_rewriting_metadata() {
        let (_temporary, repo) = repository();
        fs::write(repo.root.join("text"), b"a\r\nb\r\n").unwrap();
        let digest = "dd8c6a395b5dd36c56d23275028f526c";
        let raw = format!("outs:\n- path: text\n  md5: {digest}\n  size: 6\n");
        fs::write(repo.root.join("text.dvc"), &raw).unwrap();
        install_cache_file_with_algorithm(&repo, digest, &repo.root.join("text"), "md5-dos2unix")
            .unwrap();
        let pointers = vec!["text.dvc".into()];
        assert_eq!(status(&repo, &pointers, false, false).unwrap().1, "{}");
        assert_eq!(data_status(&repo, &["text".into()]).unwrap(), json!({}));
        fs::remove_file(repo.root.join("text")).unwrap();
        checkout(&repo, &pointers).unwrap();
        assert_eq!(fs::read(repo.root.join("text")).unwrap(), b"a\r\nb\r\n");
        assert_eq!(fs::read_to_string(repo.root.join("text.dvc")).unwrap(), raw);
    }

    #[test]
    fn legacy_checkout_refuses_normalized_cache_with_wrong_physical_size() {
        let (_temporary, repo) = repository();
        let digest = "dd8c6a395b5dd36c56d23275028f526c";
        let raw = format!("outs:\n- path: text\n  md5: {digest}\n  size: 6\n");
        fs::write(repo.root.join("text.dvc"), &raw).unwrap();
        let normalized = repo.root.join("normalized");
        fs::write(&normalized, b"a\nb\n").unwrap();
        install_cache_file(&repo, digest, &normalized).unwrap();
        let canonical = cache_path(&repo, digest).unwrap();
        let error = checkout(&repo, &["text.dvc".into()]).unwrap_err();
        assert!(
            error.to_string().contains("size") || error.to_string().contains("No such file"),
            "{error}"
        );
        assert!(!repo.root.join("text").exists());
        assert_eq!(fs::read(canonical).unwrap(), b"a\nb\n");
        assert_eq!(fs::read_to_string(repo.root.join("text.dvc")).unwrap(), raw);
    }

    #[test]
    fn native_record_converts_an_unbound_legacy_directory_to_raw_hashes() {
        let (_temporary, repo) = repository();
        let storage = tempfile::tempdir().unwrap();
        remote(&repo, storage.path());
        fs::create_dir(repo.root.join("data")).unwrap();
        fs::write(repo.root.join("data/a"), b"a\r\nb\r\n").unwrap();
        let digest = "dd8c6a395b5dd36c56d23275028f526c";
        let raw = format!(
            "outs:\n- path: data\n  md5: 178e38d9097fc874ace61e427874fc39.dir\n  size: 6\n  nfiles: 1\n  files:\n  - relpath: a\n    md5: {digest}\n    size: 6\n"
        );
        let manifest = crate::legacy_dvc::import_manifest(&raw, "data.dvc").unwrap();
        write_manifest(&repo, "data.wm-storage.json", &manifest).unwrap();
        let normalized = repo.root.join("normalized");
        fs::write(&normalized, b"a\nb\n").unwrap();
        install_cache_file(&repo, digest, &normalized).unwrap();
        commit(&repo, "data.wm-storage.json").unwrap();
        let canonical = cache_path(&repo, digest).unwrap();
        let raw_digest = file_digest(&repo.root.join("data/a"), "md5").unwrap();
        let raw_cache = cache_path(&repo, &raw_digest).unwrap();
        assert_ne!(canonical, raw_cache);
        assert_eq!(
            read_manifest(&repo, "data.wm-storage.json")
                .unwrap()
                .checksum
                .algorithm,
            "md5"
        );
        assert_eq!(fs::read(&raw_cache).unwrap(), b"a\r\nb\r\n");
        let pointers = vec!["data.wm-storage.json".into()];
        push(&repo, &pointers).unwrap();
        fs::remove_file(&raw_cache).unwrap();
        fs::remove_dir_all(repo.root.join("data")).unwrap();
        let before = fs::read(repo.root.join(&pointers[0])).unwrap();
        fetch(&repo, &pointers).unwrap();
        checkout(&repo, &pointers).unwrap();
        assert_eq!(fs::read(repo.root.join("data/a")).unwrap(), b"a\r\nb\r\n");
        assert_eq!(fs::read(canonical).unwrap(), b"a\nb\n");
        assert_eq!(fs::read(repo.root.join(&pointers[0])).unwrap(), before);
    }

    #[test]
    fn moving_an_unhydrated_pointer_preserves_metadata_and_ignore_rules() {
        let (_temporary, repo) = repository();
        fs::write(repo.root.join("data"), "data").unwrap();
        add(&repo, "data").unwrap();
        fs::remove_file(repo.root.join("data")).unwrap();
        fs::write(repo.root.join(".gitignore"), "/data\n/keep-local\n").unwrap();
        move_output(&repo, "data", "nested/moved").unwrap();
        assert!(!repo.root.join("data.wm-storage.json").exists());
        assert!(!repo.root.join("nested/moved").exists());
        assert_eq!(
            read_manifest(&repo, "nested/moved.wm-storage.json")
                .unwrap()
                .path,
            "moved"
        );
        assert_eq!(
            fs::read_to_string(repo.root.join(".gitignore")).unwrap(),
            "/keep-local\n"
        );
        assert_eq!(
            fs::read_to_string(repo.root.join("nested/.gitignore")).unwrap(),
            "/moved\n"
        );
        remove(&repo, "nested/moved.wm-storage.json").unwrap();
        assert_eq!(
            fs::read_to_string(repo.root.join(".gitignore")).unwrap(),
            "/keep-local\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_symlinks_hash_target_bytes_but_directory_links_are_refused() {
        let (_temporary, repo) = repository();
        fs::create_dir(repo.root.join("data")).unwrap();
        fs::write(repo.root.join("data/original"), "original").unwrap();
        std::os::unix::fs::symlink("original", repo.root.join("data/link")).unwrap();
        add(&repo, "data").unwrap();
        let files = current_files(&repo, "data").unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].md5, files[1].md5);
        checkout(&repo, &["data.wm-storage.json".into()]).unwrap();
        assert!(
            fs::symlink_metadata(repo.root.join("data/link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::create_dir(repo.root.join("outside")).unwrap();
        std::os::unix::fs::symlink("../outside", repo.root.join("data/dir-link")).unwrap();
        assert!(current_files(&repo, "data").is_err());
        let foreign = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(foreign.path(), repo.root.join("foreign")).unwrap();
        assert!(add(&repo, "foreign/data").is_err());
        assert!(!foreign.path().join("data.wm-storage.json").exists());
    }

    // APFS rejects non-UTF-8 filenames at creation; Linux filesystems admit
    // them, so exercise the scanner's refusal where the fixture can exist.
    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_directory_filenames_fail_before_metadata_or_payload_changes() {
        use std::os::unix::ffi::OsStringExt;

        let (_temporary, repo) = repository();
        fs::create_dir(repo.root.join("data")).unwrap();
        let name = std::ffi::OsString::from_vec(b"name-\xff".to_vec());
        let source = repo.root.join("data").join(name);
        fs::write(&source, b"local\0\r\nbytes").unwrap();
        let error = add(&repo, "data").unwrap_err();
        assert!(error.to_string().contains("not UTF-8"), "{error}");
        assert_eq!(fs::read(source).unwrap(), b"local\0\r\nbytes");
        assert!(!repo.root.join("data.wm-storage.json").exists());
        assert!(!repo.root.join(".gitignore").exists());
        assert_eq!(fs::read_dir(repo.root.join("data")).unwrap().count(), 1);
    }

    #[test]
    fn transport_metadata_rejects_disabled_foreign_and_malformed_outputs() {
        let (_temporary, repo) = repository();
        let digest = "0cc175b9c0f1b6a831c399e269772661";
        for extra in [
            "cache: false",
            "push: false",
            "can_push: false",
            "remote: another",
            "hash: sha256",
            "md5: FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF",
        ] {
            let raw = if extra.starts_with("md5:") {
                format!("outs:\n- path: data\n  {extra}\n")
            } else {
                format!("outs:\n- path: data\n  md5: {digest}\n  {extra}\n")
            };
            fs::write(repo.root.join("data.dvc"), raw).unwrap();
            assert!(
                metadata_entries(&repo, None, &["data.dvc".into()]).is_err(),
                "{extra}"
            );
        }
        fs::write(repo.root.join("data.dvc"), "outs: []\n").unwrap();
        assert!(metadata_entries(&repo, None, &["data.dvc".into()]).is_err());
        assert!(
            crate::legacy_dvc::hash_algorithm(
                "outs:\n- path: data\n  files:\n  - relpath: a\n    remote: foreign\n",
                "data.dvc"
            )
            .is_err()
        );
    }

    fn upload_fixture(repo: &GitRepo, bytes: &[u8]) -> (StorageEntry, PathBuf) {
        let source = repo.root.join("source");
        fs::write(&source, bytes).unwrap();
        let digest = file_digest(&source, "md5").unwrap();
        install_cache_file(repo, &digest, &source).unwrap();
        (
            StorageEntry {
                pointer: "source.dvc".into(),
                object: "task/source".into(),
                md5: Some(digest),
                size: Some(bytes.len() as u64),
                version_id: None,
                etag: None,
                hash_name: "md5".into(),
            },
            source,
        )
    }

    fn uploaded_head(token: &str, size: usize) -> crate::native_s3::tests::Reply {
        crate::native_s3::tests::Reply {
            status: 200,
            headers: vec![
                ("x-amz-version-id", "owned-version".into()),
                ("etag", "\"owned-etag\"".into()),
                ("content-length", size.to_string()),
                ("x-amz-meta-workspace-mgr-upload", token.into()),
            ],
            body: Vec::new(),
        }
    }

    fn uploaded_get(bytes: &[u8], version: &str, etag: &str) -> crate::native_s3::tests::Reply {
        crate::native_s3::tests::Reply {
            status: 200,
            headers: vec![("x-amz-version-id", version.into()), ("etag", etag.into())],
            body: bytes.to_vec(),
        }
    }

    #[test]
    fn multipart_upload_streams_complete_parts_and_verifies_its_exact_version() {
        use crate::native_s3::tests::{Reply, fixture_handler};
        let (_temporary, repo) = repository();
        let bytes = b"abcdefghijkl";
        let (entry, source) = upload_fixture(&repo, bytes);
        let token = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let captured = token.clone();
        let (client, worker) = fixture_handler(7, move |request| {
            if request.method == "POST" && request.target.contains("uploads") {
                *captured.lock().unwrap() =
                    request.headers["x-amz-meta-workspace-mgr-upload"].clone();
                return Reply::xml(
                    "<InitiateMultipartUploadResult><UploadId>owned-upload</UploadId></InitiateMultipartUploadResult>",
                );
            }
            if request.method == "PUT" {
                return Reply {
                    status: 200,
                    headers: vec![("etag", "\"part-etag\"".into())],
                    body: Vec::new(),
                };
            }
            if request.method == "POST" {
                return Reply {status:200,headers:vec![("x-amz-version-id","owned-version".into())],body:b"<CompleteMultipartUploadResult><ETag>\"owned-etag\"</ETag></CompleteMultipartUploadResult>".to_vec()};
            }
            if request.method == "HEAD" {
                return uploaded_head(&captured.lock().unwrap(), bytes.len());
            }
            uploaded_get(bytes, "owned-version", "\"owned-etag\"")
        });
        assert_eq!(
            upload_version(&client, &repo, &entry, &source, 4, 5).unwrap(),
            ("owned-version".into(), "owned-etag".into())
        );
        let requests = worker.join().unwrap();
        let parts = requests
            .iter()
            .filter(|request| request.method == "PUT")
            .map(|request| request.body.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            parts,
            vec![b"abcde".to_vec(), b"fghij".to_vec(), b"kl".to_vec()]
        );
        assert!(
            requests
                .iter()
                .filter(|request| request.method == "PUT")
                .all(|request| request.headers.contains_key("content-md5"))
        );
        let completion = String::from_utf8(
            requests
                .iter()
                .find(|request| request.method == "POST" && !request.target.contains("uploads"))
                .unwrap()
                .body
                .clone(),
        )
        .unwrap();
        assert_eq!(completion.matches("<PartNumber>").count(), 3);
        assert!(completion.contains("<PartNumber>3</PartNumber>"));
        assert!(!token.lock().unwrap().is_empty());
    }

    #[test]
    fn lost_put_response_recovers_owned_version_without_another_upload() {
        use crate::native_s3::tests::{Reply, fixture_handler};
        let (_temporary, repo) = repository();
        let bytes = b"recovered bytes";
        let (entry, source) = upload_fixture(&repo, bytes);
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let token = captured.clone();
        let (client, worker) = fixture_handler(5, move |request| {
            if request.method == "PUT" {
                *token.lock().unwrap() = request.headers["x-amz-meta-workspace-mgr-upload"].clone();
                return Reply {status:500,headers:Vec::new(),body:b"<Error><Code>InternalError</Code><Message>response lost after commit</Message></Error>".to_vec()};
            }
            if request.target.contains("versions") {
                return Reply::xml(
                    "<ListVersionsResult><IsTruncated>false</IsTruncated><Version><Key>root/task/source</Key><VersionId>owned-version</VersionId><Size>15</Size><ETag>\"owned-etag\"</ETag></Version></ListVersionsResult>",
                );
            }
            if request.method == "HEAD" {
                return uploaded_head(&token.lock().unwrap(), bytes.len());
            }
            uploaded_get(bytes, "owned-version", "\"owned-etag\"")
        });
        assert_eq!(
            upload_version(&client, &repo, &entry, &source, 100, 5)
                .unwrap()
                .0,
            "owned-version"
        );
        let requests = worker.join().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.method == "PUT")
                .count(),
            1
        );
        let (_, journal) = upload_journal(&repo, &client, &entry).unwrap();
        assert_eq!(journal["phase"], "complete");
        assert_eq!(journal["version_id"], "owned-version");
    }

    #[test]
    fn multipart_failure_aborts_only_the_known_owned_upload() {
        use crate::native_s3::tests::{Reply, fixture_handler};
        let (_temporary, repo) = repository();
        let (entry, source) = upload_fixture(&repo, b"multipart bytes");
        let (client, worker) = fixture_handler(4, move |request| {
            if request.method == "POST" {
                return Reply::xml(
                    "<InitiateMultipartUploadResult><UploadId>owned-upload</UploadId></InitiateMultipartUploadResult>",
                );
            }
            if request.method == "PUT" {
                return Reply {
                    status: 403,
                    headers: Vec::new(),
                    body: b"<Error><Code>AccessDenied</Code></Error>".to_vec(),
                };
            }
            if request.method == "DELETE" {
                assert!(request.target.contains("uploadId=owned-upload"));
                return Reply {
                    status: 204,
                    headers: Vec::new(),
                    body: Vec::new(),
                };
            }
            Reply::xml("<ListVersionsResult><IsTruncated>false</IsTruncated></ListVersionsResult>")
        });
        assert!(upload_version(&client, &repo, &entry, &source, 4, 5).is_err());
        let requests = worker.join().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.method == "DELETE")
                .count(),
            1
        );
        let (_, journal) = upload_journal(&repo, &client, &entry).unwrap();
        assert_eq!(journal["phase"], "planned");
        assert!(journal["upload_id"].is_null());
    }

    #[test]
    fn uploaded_get_cannot_substitute_another_exact_generation_or_etag() {
        use crate::native_s3::tests::fixture;
        for (version, etag) in [
            ("foreign-version", "\"owned-etag\""),
            ("owned-version", "\"foreign-etag\""),
        ] {
            let (_temporary, repo) = repository();
            let bytes = b"same bytes";
            let (entry, _source) = upload_fixture(&repo, bytes);
            let (client, worker) = fixture(vec![
                uploaded_head("owned-token", bytes.len()),
                uploaded_get(bytes, version, etag),
            ]);
            assert!(
                verify_uploaded_version(
                    &client,
                    &repo,
                    &entry,
                    "owned-token",
                    "owned-version",
                    None
                )
                .is_err()
            );
            worker.join().unwrap();
        }
    }

    #[test]
    fn import_upload_receipts_distinguish_raw_variants_of_one_normalized_checksum() {
        use crate::native_s3::tests::fixture;
        let (_temporary, repo) = repository();
        let (mut entry, source) = upload_fixture(&repo, b"a\r\nb\n");
        entry.hash_name = "md5-dos2unix".into();
        entry.md5 = Some(file_digest(&source, &entry.hash_name).unwrap());
        let a = file_sha256(&source).unwrap();
        fs::write(&source, b"a\nb\r\n").unwrap();
        assert_eq!(
            file_digest(&source, &entry.hash_name).unwrap(),
            entry.md5.as_deref().unwrap()
        );
        let b = file_sha256(&source).unwrap();
        let (client, worker) = fixture(vec![]);
        let (a_path, a_receipt) =
            upload_journal_in(&repo, &client, &entry, "storage-import-uploads", Some(&a)).unwrap();
        let (b_path, b_receipt) =
            upload_journal_in(&repo, &client, &entry, "storage-import-uploads", Some(&b)).unwrap();
        assert_ne!(a_path, b_path);
        assert_ne!(a_receipt["token"], b_receipt["token"]);
        assert_eq!(a_receipt["context"]["raw_sha256"], a);
        assert_eq!(b_receipt["context"]["raw_sha256"], b);
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(a_path).unwrap()).unwrap(),
            a_receipt
        );
        assert!(upload_context(&repo, &client, &entry, "storage-import-uploads", None).is_err());
        worker.join().unwrap();
    }

    #[test]
    fn imported_exact_uploaded_get_rejects_a_different_raw_normalized_variant() {
        use crate::native_s3::tests::fixture;
        let (_temporary, repo) = repository();
        let (mut entry, source) = upload_fixture(&repo, b"a\r\nb\n");
        entry.hash_name = "md5-dos2unix".into();
        entry.md5 = Some(file_digest(&source, &entry.hash_name).unwrap());
        let expected = file_sha256(&source).unwrap();
        let bytes = b"a\nb\r\n";
        let (client, worker) = fixture(vec![
            uploaded_head("owned-token", bytes.len()),
            uploaded_get(bytes, "owned-version", "\"owned-etag\""),
        ]);
        assert!(
            verify_uploaded_version(
                &client,
                &repo,
                &entry,
                "owned-token",
                "owned-version",
                Some(&expected)
            )
            .is_err()
        );
        worker.join().unwrap();
    }

    #[test]
    fn restoring_legacy_cache_preserves_hash_algorithm_and_exact_binding() {
        let (_temporary, repo) = repository();
        fs::write(repo.root.join("text"), b"a\r\nb\n").unwrap();
        let digest = file_digest(&repo.root.join("text"), "md5-dos2unix").unwrap();
        let raw = format!(
            "outs:\n- path: text\n  hash: md5-dos2unix\n  md5: {digest}\n  size: 5\n  cloud:\n    workspace-mgr:\n      version_id: original-version\n      etag: original-etag\n"
        );
        let manifest = crate::legacy_dvc::import_manifest(&raw, "text.dvc").unwrap();
        write_manifest(&repo, "text.wm-storage.json", &manifest).unwrap();
        let original = metadata_entries(&repo, None, &["text.wm-storage.json".into()])
            .unwrap()
            .remove(0);
        assert!(
            !storage_metadata::payload_matches_metadata(
                &repo,
                "text.wm-storage.json",
                &manifest.serialize().unwrap()
            )
            .unwrap()
        );
        install_cache_for_entry(&repo, &original, &repo.root.join("text")).unwrap();
        commit(&repo, "text.wm-storage.json").unwrap();
        let entries = metadata_entries(&repo, None, &["text.wm-storage.json".into()]).unwrap();
        assert_eq!(entries[0].hash_name, "md5-dos2unix");
        assert_eq!(entries[0].md5.as_deref(), Some(digest.as_str()));
        assert_eq!(entries[0].version_id.as_deref(), Some("original-version"));
        assert_eq!(
            fs::read(existing_cache_with_algorithm(&repo, &digest, "md5-dos2unix").unwrap())
                .unwrap(),
            b"a\r\nb\n"
        );
        fs::write(repo.root.join("text"), b"a\nb\r\n").unwrap();
        assert_eq!(
            file_digest(&repo.root.join("text"), "md5-dos2unix").unwrap(),
            digest
        );
        assert_eq!(
            status(&repo, &["text.wm-storage.json".into()], false, true)
                .unwrap()
                .0,
            1
        );
        assert!(
            !storage_metadata::payload_matches_metadata(
                &repo,
                "text.wm-storage.json",
                &manifest.serialize().unwrap()
            )
            .unwrap()
        );
        commit(&repo, "text.wm-storage.json").unwrap();
        let changed = read_manifest(&repo, "text.wm-storage.json").unwrap();
        assert_eq!(changed.checksum.algorithm, "md5");
        assert_eq!(
            changed.checksum.digest,
            file_digest(&repo.root.join("text"), "md5").unwrap()
        );
        assert!(
            metadata_entries(&repo, None, &["text.wm-storage.json".into()]).unwrap()[0]
                .version_id
                .is_none()
        );
    }

    #[test]
    fn an_independent_cloud_binding_edit_during_upload_is_preserved() {
        use crate::native_s3::tests::{Reply, configure_repo, fixture_handler};
        let (_temporary, repo) = repository();
        let bytes = b"same bytes";
        fs::write(repo.root.join("source"), bytes).unwrap();
        add(&repo, "source").unwrap();
        let pointer = repo.root.join("source.wm-storage.json");
        let captured = std::sync::Arc::new(std::sync::Mutex::new((String::new(), Vec::new())));
        let expected = captured.clone();
        let (client, worker) = fixture_handler(4, move |request| {
            if request.target.contains("versioning") {
                return Reply::xml(
                    "<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>",
                );
            }
            if request.method == "PUT" {
                let mut raw = Manifest::parse(
                    &fs::read_to_string(&pointer).unwrap(),
                    "source.wm-storage.json",
                )
                .unwrap();
                raw.version = Some(Version {
                    id: "independent-version".into(),
                    etag: Some("independent-etag".into()),
                });
                let rendered = raw.serialize().unwrap().into_bytes();
                fs::write(&pointer, &rendered).unwrap();
                *expected.lock().unwrap() = (
                    request.headers["x-amz-meta-workspace-mgr-upload"].clone(),
                    rendered,
                );
                return Reply {
                    status: 200,
                    headers: vec![
                        ("x-amz-version-id", "owned-version".into()),
                        ("etag", "\"owned-etag\"".into()),
                    ],
                    body: Vec::new(),
                };
            }
            if request.method == "HEAD" {
                return uploaded_head(&expected.lock().unwrap().0, bytes.len());
            }
            uploaded_get(bytes, "owned-version", "\"owned-etag\"")
        });
        configure_repo(&client, &repo);
        let error = push_versioned(&repo, &["source.wm-storage.json".into()]).unwrap_err();
        assert!(error.to_string().contains("metadata changed during upload"));
        worker.join().unwrap();
        assert_eq!(
            fs::read(repo.root.join("source.wm-storage.json")).unwrap(),
            captured.lock().unwrap().1
        );
    }

    #[test]
    fn changing_a_file_boundary_into_a_directory_is_dirty_even_if_one_file_matches() {
        let (_temporary, repo) = repository();
        fs::write(repo.root.join("source"), "same").unwrap();
        add(&repo, "source").unwrap();
        fs::remove_file(repo.root.join("source")).unwrap();
        fs::create_dir(repo.root.join("source")).unwrap();
        fs::write(repo.root.join("source/one"), "same").unwrap();
        let value: Value = serde_json::from_str(
            &status(&repo, &["source.wm-storage.json".into()], false, false)
                .unwrap()
                .1,
        )
        .unwrap();
        assert_eq!(
            value["source.wm-storage.json"][0]["changed outs"]["source"],
            "modified"
        );
    }
}
