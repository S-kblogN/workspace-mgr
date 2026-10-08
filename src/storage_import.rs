//! Resumable import of legacy content-addressed S3 objects into native keys.
//! Source objects are read and verified, never changed or deleted.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::native_engine::StorageEntry;
use crate::native_s3::S3Client;
use crate::path::{reject_symlink_traversal, repo_path, to_slash};
use crate::storage_format::{Checksum, Kind, Manifest, Version};

const JOURNAL_NAME: &str = "storage-import.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteObject {
    pub source_key: String,
    pub destination_key: String,
    pub object: String,
    pub algorithm: String,
    pub digest: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_version_id: Option<String>,
    pub source_etag: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

impl RemoteObject {
    fn entry(&self) -> StorageEntry {
        StorageEntry {
            pointer: format!("{}{}", self.object, crate::storage_format::SUFFIX),
            object: self.object.clone(),
            md5: Some(self.digest.clone()),
            size: Some(self.size),
            version_id: None,
            etag: None,
            hash_name: self.algorithm.clone(),
        }
    }

    fn source(&self) -> crate::native_engine::CasSource {
        crate::native_engine::CasSource {
            key: self.source_key.clone(),
            version_id: self.source_version_id.clone(),
            etag: self.source_etag.clone(),
            size: self.size,
            checksum: Checksum {
                algorithm: self.algorithm.clone(),
                digest: self.digest.clone(),
            },
        }
    }

    fn unbound(&self) -> Self {
        let mut value = self.clone();
        value.version_id = None;
        value.etag = None;
        value
    }
}

pub(crate) struct Plan {
    pub client: S3Client,
    pub objects: Vec<RemoteObject>,
    pub metadata_sources: Vec<SourceIdentity>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceIdentity {
    key: String,
    version_id: Option<String>,
    etag: String,
    size: u64,
    digest: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    schema_version: u32,
    root: PathBuf,
    bucket: String,
    prefix: String,
    objects: Vec<RemoteObject>,
    metadata_sources: Vec<SourceIdentity>,
    // Private snapshots also protect credentials and source configuration.
    expected_before: BTreeMap<String, Option<Vec<u8>>>,
    #[serde(default)]
    committed_after: BTreeMap<String, Option<Vec<u8>>>,
    #[serde(default)]
    moves: Vec<(String, String)>,
}

pub(crate) fn journal_path(repo: &GitRepo) -> Result<PathBuf> {
    let local = crate::local_state::directory_unmigrated(repo)?;
    reject_symlink_traversal(&local, JOURNAL_NAME, "private storage import journal")?;
    Ok(local.join(JOURNAL_NAME))
}

pub(crate) fn client(
    location: &str,
    endpoint: Option<&str>,
    credentials: &crate::native_s3::CredentialsConfig,
) -> Result<S3Client> {
    let client = S3Client::from_location(location, endpoint, credentials)?;
    require_versioning(&client)?;
    Ok(client)
}

fn require_versioning(client: &S3Client) -> Result<()> {
    let info = client
        .call_s3(
            "get_bucket_versioning",
            &json!({"Bucket":client.bucket}),
            None,
        )?
        .value;
    if info["Status"] != "Enabled" {
        return Err(Error::message(format!(
            "DVC S3 import requires enabled object versioning in bucket {:?}; enable it before manage, so new native objects receive exact versions",
            client.bucket
        )));
    }
    Ok(())
}

/// Read only: resolve DVC 2 and DVC 3 layouts and a complete physical inventory.
pub(crate) fn import_manifest(
    client: &S3Client,
    root: &Path,
    raw: &str,
    origin: &str,
) -> Result<(Manifest, Vec<RemoteObject>, Vec<SourceIdentity>)> {
    let saved = read_journal(&GitRepo {
        root: root.to_owned(),
    })?;
    if saved
        .as_ref()
        .is_some_and(|journal| journal.bucket != client.bucket || journal.prefix != client.prefix)
    {
        return Err(Error::message(
            "interrupted storage import belongs to a different S3 location; cancel it before changing routes",
        ));
    }
    let parsed = crate::legacy_dvc::parse_document(raw, origin)?;
    if parsed.outs.len() != 1 {
        return Err(Error::message(
            "legacy storage metadata must define exactly one output",
        ));
    }
    let out = &parsed.outs[0];
    let algorithm = crate::legacy_dvc::hash_algorithm(raw, origin)?;
    let digest = out
        .md5
        .as_deref()
        .ok_or_else(|| Error::message("legacy output has no MD5 checksum"))?;
    Checksum {
        algorithm: algorithm.clone(),
        digest: digest.trim_end_matches(".dir").to_owned(),
    }
    .validate()?;
    let parent = Path::new(origin).parent().unwrap_or(Path::new(""));
    let object = repo_path(&to_slash(&parent.join(&out.path)), "legacy output object")?;
    let document: serde_yaml::Value =
        serde_yaml::from_str(raw).map_err(|_| Error::message("invalid legacy pointer"))?;
    let modern = document["outs"][0].get("hash").is_some();
    let mut sizes = BTreeMap::new();
    let mut objects = Vec::new();
    let mut directory_raw = None;
    let mut metadata_sources = Vec::new();
    if digest.ends_with(".dir") {
        let files = match &out.files {
            Some(files) => files.clone(),
            None => {
                let old = saved.as_ref().and_then(|journal| {
                    journal
                        .metadata_sources
                        .iter()
                        .find(|row| row.digest == digest)
                });
                let located = match old {
                    Some(old) => locate_recorded(client, old)?,
                    None => locate(client, digest, modern, out.version_id.as_deref())?,
                };
                let mut args =
                    json!({"Bucket":client.bucket,"Key":located.key,"IfMatch":located.etag});
                if let Some(version) = &located.version_id {
                    args["VersionId"] = version.clone().into();
                }
                let fetched = client.call_s3("get_object", &args, None)?;
                verify_read(&located, &fetched.value, fetched.body.len() as u64)?;
                let files = crate::legacy_dvc::parse_directory_manifest(&fetched.body, digest)?;
                metadata_sources.push(SourceIdentity {
                    key: located.key,
                    version_id: located.version_id,
                    etag: located.etag,
                    size: located.size,
                    digest: digest.to_owned(),
                });
                directory_raw = Some(fetched.body);
                files
            }
        };
        for file in files {
            let digest = file
                .md5
                .as_deref()
                .ok_or_else(|| Error::message("legacy directory entry has no checksum"))?;
            let path = repo_path(
                &format!("{object}/{}", file.relpath),
                "legacy directory object",
            )?;
            let source = locate_payload(
                client,
                saved.as_ref(),
                &path,
                &algorithm,
                digest,
                modern,
                file.version_id.as_deref(),
            )?;
            if file.size.is_some_and(|size| size != source.size) {
                return Err(Error::message(
                    "legacy directory entry size differs from its exact remote object",
                ));
            }
            sizes.insert(file.relpath.clone(), source.size);
            objects.push(remote_object(client, path, &algorithm, digest, source));
        }
    } else {
        let source = locate_payload(
            client,
            saved.as_ref(),
            &object,
            &algorithm,
            digest,
            modern,
            out.version_id.as_deref(),
        )?;
        if out.size.is_some_and(|size| size != source.size) {
            return Err(Error::message(
                "legacy file size differs from its exact remote object",
            ));
        }
        sizes.insert(out.path.clone(), source.size);
        objects.push(remote_object(client, object, &algorithm, digest, source));
    }
    let mut manifest = crate::legacy_dvc::import_manifest_with_remote_inventory(
        raw,
        origin,
        root,
        directory_raw.as_deref(),
        &sizes,
    )?;
    // Legacy versions address hash keys. New bindings are installed only after
    // the verified import, never copied unchanged onto different physical keys.
    manifest.version = None;
    if let Some(entries) = &mut manifest.entries {
        for entry in entries {
            entry.version = None;
        }
    }
    manifest.validate(origin)?;
    Ok((manifest, objects, metadata_sources))
}

fn locate_payload(
    client: &S3Client,
    saved: Option<&Journal>,
    object: &str,
    algorithm: &str,
    digest: &str,
    modern: bool,
    version: Option<&str>,
) -> Result<Located> {
    if let Some(row) =
        saved.and_then(|journal| journal.objects.iter().find(|row| row.object == object))
    {
        if row.algorithm != algorithm
            || row.digest != digest
            || row.destination_key != client.key_for(object)
        {
            return Err(Error::message(
                "legacy source metadata changed during an interrupted storage import; cancel the recorded migration before replanning",
            ));
        }
        return locate_recorded(
            client,
            &SourceIdentity {
                key: row.source_key.clone(),
                version_id: row.source_version_id.clone(),
                etag: row.source_etag.clone(),
                size: row.size,
                digest: row.digest.clone(),
            },
        );
    }
    locate(client, digest, modern, version)
}

fn locate_recorded(client: &S3Client, source: &SourceIdentity) -> Result<Located> {
    let mut args = json!({"Bucket":client.bucket,"Key":source.key,"IfMatch":source.etag});
    if let Some(version) = &source.version_id {
        args["VersionId"] = version.clone().into();
    }
    let info = client.call_s3("head_object", &args, None).map_err(|error| Error::message(format!("recorded legacy S3 source is unavailable; retained migration can be cancelled before replanning: {error}")))?.value;
    if info["DeleteMarker"] == true
        || info["ETag"].as_str() != Some(source.etag.as_str())
        || info["ContentLength"].as_u64() != Some(source.size)
        || source
            .version_id
            .as_ref()
            .is_some_and(|id| info["VersionId"].as_str() != Some(id.as_str()))
    {
        return Err(Error::message(
            "recorded legacy source changed; retain or cancel the interrupted migration before replanning",
        ));
    }
    Ok(Located {
        key: source.key.clone(),
        version_id: source.version_id.clone(),
        etag: source.etag.clone(),
        size: source.size,
    })
}

struct Located {
    key: String,
    version_id: Option<String>,
    etag: String,
    size: u64,
}

fn locate(client: &S3Client, digest: &str, modern: bool, version: Option<&str>) -> Result<Located> {
    if digest.len() < 2 {
        return Err(Error::message("invalid legacy digest"));
    }
    let (first, rest) = digest.split_at(2);
    let flat = format!("{first}/{rest}");
    let current = format!("files/md5/{first}/{rest}");
    let layouts = if modern {
        [current, flat]
    } else {
        [flat, current]
    };
    for object in layouts {
        let key = client.key_for(&object);
        let mut args = json!({"Bucket":client.bucket,"Key":key});
        if let Some(version) = version {
            args["VersionId"] = version.into();
        }
        let info = match client.call_s3("head_object", &args, None) {
            Ok(response) => response.value,
            Err(error) if error.is_missing() => continue,
            Err(error) => return Err(error.into()),
        };
        if info["DeleteMarker"] == true {
            continue;
        }
        let etag = info["ETag"]
            .as_str()
            .filter(|etag| !etag.is_empty())
            .ok_or_else(|| Error::message("legacy S3 object has no ETag"))?
            .to_owned();
        let size = info["ContentLength"]
            .as_u64()
            .ok_or_else(|| Error::message("legacy S3 object has no physical size"))?;
        let found_version = info["VersionId"]
            .as_str()
            .filter(|id| !id.is_empty() && *id != "null")
            .map(str::to_owned);
        if version
            .filter(|id| *id != "null")
            .is_some_and(|id| found_version.as_deref() != Some(id))
        {
            return Err(Error::message(
                "legacy S3 HEAD did not preserve its requested exact version",
            ));
        }
        return Ok(Located {
            key,
            version_id: found_version,
            etag,
            size,
        });
    }
    Err(Error::message(format!(
        "legacy content-addressed object {digest:?} is missing from both supported DVC remote layouts"
    )))
}

fn verify_read(source: &Located, value: &Value, length: u64) -> Result<()> {
    if value["DeleteMarker"] == true
        || value["ETag"].as_str() != Some(source.etag.as_str())
        || value["ContentLength"].as_u64() != Some(source.size)
        || length != source.size
        || source
            .version_id
            .as_ref()
            .is_some_and(|version| value["VersionId"].as_str() != Some(version.as_str()))
    {
        return Err(Error::message(
            "legacy exact directory-manifest GET differs from its preflight HEAD",
        ));
    }
    Ok(())
}

fn remote_object(
    client: &S3Client,
    object: String,
    algorithm: &str,
    digest: &str,
    source: Located,
) -> RemoteObject {
    RemoteObject {
        destination_key: client.key_for(&object),
        object,
        algorithm: algorithm.to_owned(),
        digest: digest.to_owned(),
        source_key: source.key,
        source_version_id: source.version_id,
        source_etag: source.etag,
        size: source.size,
        version_id: None,
        etag: None,
    }
}

fn read_journal(repo: &GitRepo) -> Result<Option<Journal>> {
    let path = journal_path(repo)?;
    let raw = match fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(Error::Io { path, source }),
    };
    let journal: Journal = serde_json::from_slice(&raw).map_err(|_| {
        Error::message("invalid private storage-import journal; retain it and resolve manually")
    })?;
    if journal.schema_version != 1 || journal.root != repo.root {
        return Err(Error::message(
            "storage-import journal belongs to another repository or schema",
        ));
    }
    let mut objects = BTreeSet::new();
    for row in &journal.objects {
        if repo_path(&row.object, "import destination")? != row.object
            || row.source_key.is_empty()
            || row.destination_key.is_empty()
            || row.source_etag.is_empty()
            || row
                .version_id
                .as_ref()
                .is_some_and(|id| id.is_empty() || id == "null")
        {
            return Err(Error::message(
                "invalid object identity in private storage-import journal",
            ));
        }
        if !objects.insert(row.object.clone())
            || row.destination_key != journal_key(&journal.prefix, &row.object)
            || !legacy_key(&journal.prefix, &row.source_key, &row.digest)
            || row
                .source_version_id
                .as_ref()
                .is_some_and(|id| id.is_empty() || id == "null")
        {
            return Err(Error::message(
                "private import object escaped its recorded layout or has duplicate identities",
            ));
        }
        Checksum {
            algorithm: row.algorithm.clone(),
            digest: row.digest.clone(),
        }
        .validate()?;
    }
    for path in journal
        .expected_before
        .keys()
        .chain(journal.committed_after.keys())
    {
        if repo_path(path, "import snapshot")? != *path || !control_path(path) {
            return Err(Error::message(
                "private storage-import snapshot names an unsupported repository control",
            ));
        }
    }
    for row in &journal.metadata_sources {
        if row.key.is_empty()
            || row.etag.is_empty()
            || !row.digest.ends_with(".dir")
            || !legacy_key(&journal.prefix, &row.key, &row.digest)
        {
            return Err(Error::message(
                "invalid directory source in private import journal",
            ));
        }
        Checksum {
            algorithm: "md5".to_owned(),
            digest: row.digest.trim_end_matches(".dir").to_owned(),
        }
        .validate()?;
    }
    for (source, destination) in &journal.moves {
        if !matches!(
            (source.as_str(), destination.as_str()),
            (".dvc/cache", ".workspace-mgr/local/cache/legacy")
                | (".dvc/tmp", ".workspace-mgr/local/retained-storage-state")
        ) {
            return Err(Error::message(
                "invalid retained cache move in storage-import journal",
            ));
        }
    }
    Ok(Some(journal))
}

fn journal_key(prefix: &str, object: &str) -> String {
    if prefix.is_empty() {
        object.to_owned()
    } else {
        format!("{prefix}/{object}")
    }
}

fn legacy_key(prefix: &str, key: &str, digest: &str) -> bool {
    digest.split_at_checked(2).is_some_and(|(first, rest)| {
        key == journal_key(prefix, &format!("{first}/{rest}"))
            || key == journal_key(prefix, &format!("files/md5/{first}/{rest}"))
    })
}

fn control_path(path: &str) -> bool {
    matches!(
        path,
        crate::config::CONFIG_NAME
            | "AGENTS.md"
            | ".gitignore"
            | ".gitattributes"
            | ".dvcignore"
            | ".dvc/config"
            | ".dvc/config.local"
            | ".dvc/.gitignore"
            | ".workspace-mgr/repository.gitignore"
            | crate::native_s3::CREDENTIALS_NAME
    ) || path.ends_with(".dvc")
        || path.ends_with(crate::storage_format::SUFFIX)
}

fn save_journal(repo: &GitRepo, journal: &Journal) -> Result<()> {
    let path = journal_path(repo)?;
    let raw = serde_json::to_vec(journal).map_err(|error| Error::message(error.to_string()))?;
    crate::storage_migration::atomic_write(&path, &raw, true)
}

pub(crate) fn preflight(
    repo: &GitRepo,
    plan: &mut Plan,
    expected_before: &BTreeMap<String, Option<Vec<u8>>>,
) -> Result<()> {
    private_state_preflight(repo)?;
    plan.objects.sort_by(|a, b| a.object.cmp(&b.object));
    plan.metadata_sources.sort_by(|a, b| a.key.cmp(&b.key));
    plan.metadata_sources.dedup();
    let mut destinations = BTreeSet::new();
    let sources = plan
        .objects
        .iter()
        .map(|row| &row.source_key)
        .chain(plan.metadata_sources.iter().map(|row| &row.key))
        .collect::<BTreeSet<_>>();
    for row in &plan.objects {
        if !destinations.insert(row.destination_key.clone())
            || sources.contains(&row.destination_key)
        {
            return Err(Error::message(
                "legacy import has duplicate destinations or a native key overlapping a retained source CAS object",
            ));
        }
    }
    let saved = read_journal(repo)?;
    if let Some(journal) = &saved {
        if journal.bucket != plan.client.bucket
            || journal.prefix != plan.client.prefix
            || journal
                .objects
                .iter()
                .map(RemoteObject::unbound)
                .collect::<Vec<_>>()
                != plan
                    .objects
                    .iter()
                    .map(RemoteObject::unbound)
                    .collect::<Vec<_>>()
            || journal.metadata_sources != plan.metadata_sources
            || journal.expected_before != *expected_before
        {
            return Err(Error::message(
                "interrupted storage import differs from current source metadata or remote inventory; retain the journal and reconcile the edits before manage",
            ));
        }
        plan.objects = journal.objects.clone();
    }
    for row in &plan.objects {
        // The uploader verifies ownership, including response-loss recovery.
        // An existing foreign path is never silently overwritten by migration.
        crate::native_engine::verify_import_destination(repo, &plan.client, &row.entry())?;
    }
    Ok(())
}

pub(crate) fn execute(
    repo: &GitRepo,
    plan: &mut Plan,
    expected_before: &BTreeMap<String, Option<Vec<u8>>>,
) -> Result<()> {
    require_versioning(&plan.client)?;
    for (path, expected) in expected_before {
        crate::storage_migration::verify_expected_file(repo, path, expected)?;
    }
    preflight(repo, plan, expected_before)?;
    let mut journal = Journal {
        schema_version: 1,
        root: repo.root.clone(),
        bucket: plan.client.bucket.clone(),
        prefix: plan.client.prefix.clone(),
        objects: plan.objects.clone(),
        metadata_sources: plan.metadata_sources.clone(),
        expected_before: expected_before.clone(),
        committed_after: BTreeMap::new(),
        moves: Vec::new(),
    };
    // This intent is durable before downloads, upload journals or S3 mutations.
    protect_private_state(repo)?;
    save_journal(repo, &journal)?;
    for index in 0..plan.objects.len() {
        for (path, expected) in expected_before {
            crate::storage_migration::verify_expected_file(repo, path, expected)?;
        }
        let row = &plan.objects[index];
        let source = crate::native_engine::fetch_cas_to_cache(repo, &plan.client, &row.source())?;
        let version =
            crate::native_engine::upload_verified(repo, &plan.client, &row.entry(), &source)?;
        if row.version_id.as_ref().is_some_and(|id| *id != version.id) {
            return Err(Error::message(
                "verified import upload differs from its recorded exact version; retained journal requires reconciliation",
            ));
        }
        plan.objects[index].version_id = Some(version.id);
        plan.objects[index].etag = version.etag;
        journal.objects[index] = plan.objects[index].clone();
        save_journal(repo, &journal)?;
    }
    Ok(())
}

pub(crate) fn bind_manifests(
    plan: &Plan,
    writes: &mut BTreeMap<String, Vec<u8>>,
    destinations: &[String],
) -> Result<()> {
    let versions =
        plan.objects
            .iter()
            .map(|row| {
                let id = row.version_id.clone().ok_or_else(|| {
                    Error::message("import object has no completed exact version")
                })?;
                Ok((
                    row.object.clone(),
                    Version {
                        id,
                        etag: row.etag.clone(),
                    },
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
    for path in destinations {
        let raw =
            std::str::from_utf8(writes.get(path).ok_or_else(|| {
                Error::message("import native manifest is absent from transaction")
            })?)
            .map_err(|_| Error::message("import native manifest is not UTF-8"))?;
        let mut manifest = Manifest::parse(raw, path)?;
        let object = path
            .strip_suffix(crate::storage_format::SUFFIX)
            .ok_or_else(|| Error::message("invalid import manifest suffix"))?;
        match manifest.kind {
            Kind::File => {
                manifest.version = Some(
                    versions
                        .get(object)
                        .ok_or_else(|| Error::message("import file version missing"))?
                        .clone(),
                )
            }
            Kind::Directory => {
                for entry in manifest.entries.as_mut().expect("validated directory") {
                    entry.version = Some(
                        versions
                            .get(&format!("{object}/{}", entry.path))
                            .ok_or_else(|| Error::message("import directory file version missing"))?
                            .clone(),
                    );
                }
            }
        }
        let serialized = manifest.serialize()?;
        Manifest::parse(&serialized, path)?;
        writes.insert(path.clone(), serialized.into_bytes());
    }
    Ok(())
}

pub(crate) fn prepare_commit(
    repo: &GitRepo,
    writes: &BTreeMap<String, Vec<u8>>,
    removes: &BTreeSet<String>,
    moves: &[(String, String)],
) -> Result<()> {
    let mut journal = read_journal(repo)?
        .ok_or_else(|| Error::message("storage import intent disappeared before local commit"))?;
    journal.committed_after = journal.expected_before.clone();
    journal.committed_after.extend(
        writes
            .iter()
            .map(|(path, bytes)| (path.clone(), Some(bytes.clone()))),
    );
    journal
        .committed_after
        .extend(removes.iter().map(|path| (path.clone(), None)));
    journal.moves = moves.to_vec();
    save_journal(repo, &journal)
}

/// A crash after the local commit must not make successfully migrated repos
/// permanently retain an unfinished import. Exact postconditions prove finish.
pub(crate) fn finish_committed(repo: &GitRepo, dry_run: bool) -> Result<bool> {
    let Some(journal) = read_journal(repo)? else {
        return Ok(false);
    };
    if journal.committed_after.is_empty() {
        return Ok(false);
    }
    if journal.committed_after.iter().any(|(path, expected)| {
        crate::storage_migration::verify_expected_file(repo, path, expected).is_err()
    }) {
        return Ok(false);
    }
    for (source, destination) in &journal.moves {
        if !matches!(
            (source.as_str(), destination.as_str()),
            (".dvc/cache", ".workspace-mgr/local/cache/legacy")
                | (".dvc/tmp", ".workspace-mgr/local/retained-storage-state")
        ) {
            return Err(Error::message(
                "invalid retained cache move in storage-import journal",
            ));
        }
        reject_symlink_traversal(&repo.root, source, "import retained cache")?;
        reject_symlink_traversal(&repo.root, destination, "import retained cache")?;
        if fs::symlink_metadata(repo.root.join(source)).is_ok()
            || !repo.root.join(destination).is_dir()
        {
            return Ok(false);
        }
    }
    if !dry_run {
        finish(repo)?;
    }
    Ok(true)
}

pub(crate) fn protect_private_state(repo: &GitRepo) -> Result<()> {
    private_state_preflight(repo)?;
    let common = repo.common_dir()?;
    reject_symlink_traversal(&common, "info/exclude", "private Git ignore")?;
    let path = common.join("info/exclude");
    let mut raw = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(source) => return Err(Error::Io { path, source }),
    };
    let rule = b"/.workspace-mgr/local/";
    if !raw.split(|byte| *byte == b'\n').any(|line| line == rule) {
        if !raw.is_empty() && !raw.ends_with(b"\n") {
            raw.push(b'\n');
        }
        raw.extend_from_slice(rule);
        raw.push(b'\n');
        crate::storage_migration::atomic_write(&path, &raw, false)?;
    }
    let ignored = repo.run_unchecked([
        "check-ignore",
        "--no-index",
        ".workspace-mgr/local/storage-import.json",
    ])?;
    if !ignored.success() {
        return Err(Error::message(
            "repository ignore rules expose private storage state; resolve conflicting .gitignore negations before manage",
        ));
    }
    Ok(())
}

fn private_state_preflight(repo: &GitRepo) -> Result<()> {
    let common = repo.common_dir()?;
    reject_symlink_traversal(&common, "info/exclude", "private Git ignore")?;
    let path = common.join("info/exclude");
    if fs::symlink_metadata(&path).is_ok_and(|meta| !meta.is_file()) {
        return Err(Error::message(
            "private Git exclude path must be a regular file",
        ));
    }
    if !repo
        .run(["ls-files", "--", ".workspace-mgr/local/"])?
        .stdout
        .is_empty()
    {
        return Err(Error::message(
            "private workspace state is already tracked by Git; untrack it before importing storage",
        ));
    }
    Ok(())
}

pub(crate) fn cancel(repo: &GitRepo, dry_run: bool) -> Result<Option<Vec<RemoteObject>>> {
    let Some(journal) = read_journal(repo)? else {
        return Ok(None);
    };
    if !dry_run {
        finish(repo)?;
    }
    Ok(Some(journal.objects))
}

pub(crate) fn finish(repo: &GitRepo) -> Result<()> {
    let path = journal_path(repo)?;
    fs::remove_file(&path).at(&path)?;
    fs::File::open(path.parent().expect("journal parent"))
        .at(&path)?
        .sync_all()
        .at(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository() -> (tempfile::TempDir, GitRepo) {
        let temp = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: temp.path().canonicalize().unwrap(),
        };
        repo.run(["init", "-b", "main"]).unwrap();
        (temp, repo)
    }

    fn journal(repo: &GitRepo) -> Journal {
        Journal {
            schema_version: 1,
            root: repo.root.clone(),
            bucket: "isolated".to_owned(),
            prefix: "root".to_owned(),
            objects: vec![RemoteObject {
                source_key: "root/files/md5/90/0150983cd24fb0d6963f7d28e17f72".to_owned(),
                destination_key: "root/data".to_owned(),
                object: "data".to_owned(),
                algorithm: "md5".to_owned(),
                digest: "900150983cd24fb0d6963f7d28e17f72".to_owned(),
                size: 3,
                source_version_id: Some("source-version".to_owned()),
                source_etag: "source-etag".to_owned(),
                version_id: Some("imported-version".to_owned()),
                etag: Some("imported-etag".to_owned()),
            }],
            metadata_sources: Vec::new(),
            expected_before: BTreeMap::new(),
            committed_after: BTreeMap::new(),
            moves: Vec::new(),
        }
    }

    #[test]
    fn committed_import_requires_all_exact_postconditions_and_cancel_retains_receipts() {
        let (_temp, repo) = repository();
        fs::write(repo.root.join("AGENTS.md"), "original scaffold\n").unwrap();
        let mut state = journal(&repo);
        state.committed_after.insert(
            "AGENTS.md".to_owned(),
            Some(b"original scaffold\n".to_vec()),
        );
        save_journal(&repo, &state).unwrap();
        let local = crate::local_state::directory_unmigrated(&repo).unwrap();
        fs::create_dir(local.join("storage-import-uploads")).unwrap();
        fs::write(
            local.join("storage-import-uploads/evidence.json"),
            b"ownership evidence",
        )
        .unwrap();
        assert!(finish_committed(&repo, true).unwrap());
        assert!(journal_path(&repo).unwrap().exists());
        fs::write(repo.root.join("AGENTS.md"), "external scaffold edit\n").unwrap();
        assert!(!finish_committed(&repo, false).unwrap());
        assert!(journal_path(&repo).unwrap().exists());
        cancel(&repo, true).unwrap();
        assert!(journal_path(&repo).unwrap().exists());
        let retained = cancel(&repo, false).unwrap().unwrap();
        assert_eq!(retained[0].version_id.as_deref(), Some("imported-version"));
        assert!(!journal_path(&repo).unwrap().exists());
        assert_eq!(
            fs::read(local.join("storage-import-uploads/evidence.json")).unwrap(),
            b"ownership evidence"
        );
        assert_eq!(
            fs::read(repo.root.join("AGENTS.md")).unwrap(),
            b"external scaffold edit\n"
        );
    }

    #[test]
    fn import_journal_rejects_foreign_layout_and_path_escape_before_cancel() {
        for scenario in [
            "wrong-source",
            "wrong-target",
            "outside-control",
            "path-escape",
        ] {
            let (_temp, repo) = repository();
            let mut state = journal(&repo);
            match scenario {
                "wrong-source" => {
                    state.objects[0].source_key = "other-bucket-prefix/source".to_owned()
                }
                "wrong-target" => state.objects[0].destination_key = "elsewhere/data".to_owned(),
                "outside-control" => {
                    state
                        .expected_before
                        .insert("user-private.txt".to_owned(), None);
                }
                _ => {
                    state
                        .expected_before
                        .insert("../AGENTS.md".to_owned(), None);
                }
            }
            save_journal(&repo, &state).unwrap();
            assert!(cancel(&repo, false).is_err(), "{scenario}");
            assert!(journal_path(&repo).unwrap().exists());
        }
    }

    #[test]
    fn private_git_exclude_preserves_existing_bytes_and_keeps_journals_untracked() {
        let (_temp, repo) = repository();
        let exclude = repo.common_dir().unwrap().join("info/exclude");
        fs::write(&exclude, "# user rule\n/user-secret").unwrap();
        protect_private_state(&repo).unwrap();
        assert_eq!(
            fs::read(&exclude).unwrap(),
            b"# user rule\n/user-secret\n/.workspace-mgr/local/\n"
        );
        let once = fs::read(&exclude).unwrap();
        protect_private_state(&repo).unwrap();
        assert_eq!(fs::read(&exclude).unwrap(), once);
        save_journal(&repo, &journal(&repo)).unwrap();
        assert!(
            repo.run(["status", "--porcelain", "--untracked-files=all"])
                .unwrap()
                .stdout
                .is_empty()
        );
    }
}
