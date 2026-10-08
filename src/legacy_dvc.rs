//! Read-only DVC compatibility for explicit import and historical revisions.
use std::fs;
use std::path::{Path, PathBuf};

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::collections::BTreeSet;

use crate::error::{Error, Result};
use crate::path::repo_path;
use crate::storage_format::{Checksum, Entry, Kind, Manifest, Version};
use crate::storage_metadata::{ListedFile, PointerDocument, PointerFileVersion, PointerOutput};

#[derive(Debug, Deserialize)]
struct Document {
    #[serde(default)]
    outs: Vec<Output>,
}

#[derive(Debug, Deserialize)]
struct Output {
    path: String,
    #[serde(default)]
    md5: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    cloud: Option<Value>,
    #[serde(default)]
    files: Option<Vec<File>>,
}

#[derive(Debug, Deserialize)]
struct File {
    relpath: String,
    #[serde(default)]
    md5: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    cloud: Option<Value>,
}

fn version(cloud: Option<&Value>) -> (Option<String>, Option<String>) {
    let field = |name: &str| match cloud
        .and_then(|cloud| cloud.get("workspace-mgr"))
        .and_then(|remote| remote.get(name))
    {
        Some(Value::String(value)) if !value.is_empty() => Some(value.clone()),
        Some(Value::Number(value)) => Some(value.to_string()),
        _ => None,
    };
    (field("version_id"), field("etag"))
}

pub(crate) fn parse_document(raw: &str, origin: &str) -> Result<PointerDocument> {
    let document: Document = serde_yaml::from_str(raw).map_err(|error| {
        Error::message(format!("invalid legacy DVC metadata {origin}: {error}"))
    })?;
    Ok(PointerDocument {
        outs: document
            .outs
            .into_iter()
            .map(|out| {
                let (version_id, etag) = version(out.cloud.as_ref());
                PointerOutput {
                    path: out.path,
                    md5: out.md5,
                    size: out.size,
                    version_id,
                    etag,
                    files: out.files.map(|files| {
                        files
                            .into_iter()
                            .map(|file| {
                                let (version_id, etag) = version(file.cloud.as_ref());
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

pub(crate) fn normalize_remote_binding(
    raw: &str,
    origin: &str,
    selected: Option<&str>,
) -> Result<String> {
    let Some(selected) = selected.filter(|name| *name != "workspace-mgr") else {
        return Ok(raw.to_owned());
    };
    let mut document: Value = serde_yaml::from_str(raw)
        .map_err(|_| Error::message(format!("invalid legacy pointer {origin:?}")))?;
    if let Some(outputs) = document.get_mut("outs").and_then(Value::as_sequence_mut) {
        for output in outputs {
            normalize_remote_row(output, selected, origin)?;
            if let Some(files) = output.get_mut("files").and_then(Value::as_sequence_mut) {
                for file in files {
                    normalize_remote_row(file, selected, origin)?;
                }
            }
        }
    }
    serde_yaml::to_string(&document)
        .map_err(|_| Error::message("cannot normalize selected legacy storage remote"))
}

fn normalize_remote_row(row: &mut Value, selected: &str, origin: &str) -> Result<()> {
    if let Some(cloud) = row.get_mut("cloud") {
        let cloud = cloud
            .as_mapping_mut()
            .ok_or_else(|| Error::message(format!("invalid remote binding in {origin:?}")))?;
        let key = Value::String(selected.into());
        if cloud.len() != 1 || !cloud.contains_key(&key) {
            return Err(Error::message(format!(
                "legacy pointer {origin:?} has cloud bindings outside its selected remote"
            )));
        }
        let binding = cloud.remove(&key).expect("selected cloud binding");
        cloud.insert(Value::String("workspace-mgr".into()), binding);
    }
    if let Some(remote) = row.get_mut("remote") {
        if remote.as_str() != Some(selected) {
            return Err(Error::message(format!(
                "legacy pointer {origin:?} selects another remote"
            )));
        }
        *remote = Value::String("workspace-mgr".into());
    }
    Ok(())
}

pub(crate) fn selected_remote(
    repo: &crate::git::GitRepo,
    revision: Option<&str>,
) -> Result<Option<String>> {
    let raw = if let Some(revision) = revision {
        let listing = repo.run(["ls-tree", "-z", revision, "--", ".dvc/config"])?;
        if listing.stdout.is_empty() {
            return Ok(None);
        }
        if !listing.stdout.starts_with("100644 blob ")
            && !listing.stdout.starts_with("100755 blob ")
        {
            return Err(Error::message(
                "historical legacy storage configuration is not a regular file",
            ));
        }
        repo.run(["show", &format!("{revision}:.dvc/config")])?
            .stdout
    } else {
        crate::path::reject_symlink_traversal(
            &repo.root,
            ".dvc/config",
            "legacy storage configuration",
        )?;
        match fs::read_to_string(repo.root.join(".dvc/config")) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Error::message(error.to_string())),
        }
    };
    let mut section = String::new();
    let mut sections = BTreeSet::new();
    let mut core_keys = BTreeSet::new();
    let mut selected = None;
    for line in raw.lines().map(str::trim) {
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if line.starts_with('[') {
            let name = line
                .strip_prefix('[')
                .and_then(|name| name.strip_suffix(']'))
                .ok_or_else(|| Error::message("invalid legacy storage configuration section"))?
                .trim();
            section = if name.len() >= 2
                && ((name.starts_with('\'') && name.ends_with('\''))
                    || (name.starts_with('"') && name.ends_with('"')))
            {
                name[1..name.len() - 1].into()
            } else {
                name.into()
            };
            if !sections.insert(section.clone()) {
                return Err(Error::message(
                    "duplicate legacy storage configuration section",
                ));
            }
        } else if section == "core" {
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| Error::message("invalid legacy core setting"))?;
            let key = key.trim().to_ascii_lowercase();
            if !core_keys.insert(key.clone()) {
                return Err(Error::message("duplicate legacy core setting"));
            }
            if key == "remote" {
                let value = value.trim();
                if value.is_empty() || value.starts_with('"') != value.ends_with('"') {
                    return Err(Error::message("invalid selected legacy remote"));
                }
                selected = Some(value.trim_matches('"').to_owned());
            }
        }
    }
    if selected
        .as_ref()
        .is_some_and(|remote| !sections.contains(&format!("remote \"{remote}\"")))
    {
        return Err(Error::message(
            "selected legacy remote has no matching configuration section",
        ));
    }
    Ok(selected)
}

pub(crate) fn hash_algorithm(raw: &str, origin: &str) -> Result<String> {
    let yaml: Value = serde_yaml::from_str(raw).map_err(|error| {
        Error::message(format!("invalid legacy DVC metadata {origin}: {error}"))
    })?;
    let outputs = yaml["outs"]
        .as_sequence()
        .ok_or_else(|| Error::message("legacy metadata has no outputs"))?;
    if outputs.len() != 1 {
        return Err(Error::message(
            "legacy storage metadata must define exactly one output",
        ));
    }
    let out = &outputs[0];
    for key in ["cache", "can_push", "push"] {
        if out
            .get(key)
            .is_some_and(|value| value.as_bool() != Some(true))
        {
            return Err(Error::message(format!(
                "legacy output disables or invalidates {key}: {origin}"
            )));
        }
    }
    if out
        .get("remote")
        .is_some_and(|value| value.as_str() != Some("workspace-mgr"))
    {
        return Err(Error::message(format!(
            "legacy output names an unsupported remote: {origin}"
        )));
    }
    for file in out
        .get("files")
        .and_then(Value::as_sequence)
        .into_iter()
        .flatten()
    {
        if file
            .get("remote")
            .is_some_and(|value| value.as_str() != Some("workspace-mgr"))
        {
            return Err(Error::message(format!(
                "legacy directory entry names an unsupported remote: {origin}"
            )));
        }
    }
    let algorithm = match out.get("hash") {
        None => "md5-dos2unix",
        Some(Value::String(algorithm)) => algorithm.as_str(),
        Some(_) => {
            return Err(Error::message(format!(
                "legacy storage hash algorithm must be a string: {origin}"
            )));
        }
    };
    if !matches!(algorithm, "md5" | "md5-dos2unix") {
        return Err(Error::message(format!(
            "unsupported legacy storage checksum algorithm: {algorithm}"
        )));
    }
    Ok(algorithm.to_owned())
}

#[cfg(test)]
pub(crate) fn import_manifest(raw: &str, origin: &str) -> Result<Manifest> {
    import_manifest_inner(raw, origin, None)
}

pub(crate) fn import_manifest_with_root(raw: &str, origin: &str, root: &Path) -> Result<Manifest> {
    import_manifest_inner(raw, origin, Some(root))
}

pub(crate) fn parse_directory_manifest(
    bytes: &[u8],
    digest: &str,
) -> Result<Vec<PointerFileVersion>> {
    use md5::Digest;
    if crate::hex::encode_lower(md5::Md5::digest(bytes)) != digest.trim_end_matches(".dir") {
        return Err(Error::message(
            "legacy remote directory manifest checksum mismatch",
        ));
    }
    let rows: Vec<serde_json::Value> = serde_json::from_slice(bytes)
        .map_err(|error| Error::message(format!("invalid legacy directory manifest: {error}")))?;
    let files = rows
        .into_iter()
        .map(|row| {
            let fields = row
                .as_object()
                .ok_or_else(|| Error::message("invalid legacy directory entry"))?;
            if fields
                .keys()
                .any(|key| !matches!(key.as_str(), "md5" | "relpath" | "size"))
            {
                return Err(Error::message(
                    "unsupported legacy directory manifest entry field",
                ));
            }
            if fields
                .get("size")
                .is_some_and(|size| size.as_u64().is_none())
            {
                return Err(Error::message(
                    "invalid legacy directory entry physical size",
                ));
            }
            Ok(PointerFileVersion {
                relpath: row["relpath"]
                    .as_str()
                    .ok_or_else(|| Error::message("legacy directory entry has no path"))?
                    .into(),
                md5: Some(
                    row["md5"]
                        .as_str()
                        .ok_or_else(|| {
                            Error::message("legacy directory entry has no MD5 checksum")
                        })?
                        .into(),
                ),
                size: row["size"].as_u64(),
                version_id: None,
                etag: None,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if directory_digest(&files)? != digest {
        return Err(Error::message(
            "legacy directory manifest does not match its canonical aggregate checksum",
        ));
    }
    Ok(files)
}

pub(crate) fn import_manifest_with_remote_inventory(
    raw: &str,
    origin: &str,
    root: &Path,
    directory_bytes: Option<&[u8]>,
    sizes: &std::collections::BTreeMap<String, u64>,
) -> Result<Manifest> {
    hash_algorithm(raw, origin)?;
    let mut yaml: Value =
        serde_yaml::from_str(raw).map_err(|error| Error::message(error.to_string()))?;
    let out = &mut yaml["outs"][0];
    let digest = out["md5"]
        .as_str()
        .ok_or_else(|| Error::message("legacy output has no MD5 checksum"))?
        .to_owned();
    if digest.ends_with(".dir") {
        if let Some(bytes) = directory_bytes {
            let files = parse_directory_manifest(bytes, &digest)?;
            if out.get("files").is_none() {
                out["files"] = serde_yaml::to_value(files)
                    .map_err(|error| Error::message(error.to_string()))?;
            }
        }
        let files = out["files"]
            .as_sequence_mut()
            .ok_or_else(|| Error::message("legacy directory remote inventory is unavailable"))?;
        let mut total_size = 0u64;
        for file in files {
            let path = file["relpath"]
                .as_str()
                .ok_or_else(|| Error::message("legacy directory entry has no path"))?;
            let size = *sizes
                .get(path)
                .ok_or_else(|| Error::message("legacy directory entry has no remote HEAD size"))?;
            if file
                .get("size")
                .is_some_and(|declared| declared.as_u64() != Some(size))
            {
                return Err(Error::message(
                    "legacy directory entry physical size differs from remote HEAD",
                ));
            }
            file["size"] = size.into();
            total_size = total_size
                .checked_add(size)
                .ok_or_else(|| Error::message("legacy directory physical size overflows"))?;
        }
        if out
            .get("size")
            .is_some_and(|declared| declared.as_u64() != Some(total_size))
        {
            return Err(Error::message(
                "legacy directory aggregate physical size differs from remote HEAD inventory",
            ));
        }
        out["size"] = total_size.into();
    } else {
        let path = out["path"]
            .as_str()
            .ok_or_else(|| Error::message("legacy output has no path"))?;
        let size = *sizes
            .get(path)
            .ok_or_else(|| Error::message("legacy file has no remote HEAD size"))?;
        if out
            .get("size")
            .is_some_and(|declared| declared.as_u64() != Some(size))
        {
            return Err(Error::message(
                "legacy file physical size differs from remote HEAD",
            ));
        }
        out["size"] = size.into();
    }
    let resolved =
        serde_yaml::to_string(&yaml).map_err(|error| Error::message(error.to_string()))?;
    import_manifest_inner(&resolved, origin, Some(root))
}

pub(crate) fn is_content_addressed_revision(
    repo: &crate::git::GitRepo,
    revision: &str,
) -> Result<bool> {
    let listing = repo.run(["ls-tree", "-z", revision, "--", ".dvc/config"])?;
    if !listing.stdout.starts_with("100644 blob ") && !listing.stdout.starts_with("100755 blob ") {
        return Ok(false);
    }
    let raw = repo
        .run(["show", &format!("{revision}:.dvc/config")])?
        .stdout;
    let public_listing = repo.run(["ls-tree", "-z", revision, "--", ".workspace-mgr.toml"])?;
    let public_route = if public_listing.stdout.starts_with("100644 blob ")
        || public_listing.stdout.starts_with("100755 blob ")
    {
        let public_raw = repo
            .run(["show", &format!("{revision}:.workspace-mgr.toml")])?
            .stdout;
        crate::config::Config::parse(&public_raw, &repo.root.join(".workspace-mgr.toml"))
            .ok()
            .is_some_and(|config| config.requires_object_versioning())
    } else {
        false
    };
    Ok(crate::native_s3::is_legacy_cas_configuration_with_public_route(&raw, public_route))
}

pub(crate) fn is_content_addressed_checkout(repo: &crate::git::GitRepo) -> Result<bool> {
    crate::path::reject_symlink_traversal(&repo.root, ".dvc/config", "legacy CAS configuration")?;
    let path = repo.root.join(".dvc/config");
    if !path.is_file() {
        return Ok(false);
    }
    let raw = fs::read_to_string(&path).map_err(|source| Error::Io {
        path: path.clone(),
        source,
    })?;
    let listing = repo.run(["ls-files", "--stage", "--", ".workspace-mgr.toml"])?;
    let public_path = repo.root.join(".workspace-mgr.toml");
    let public_route = (listing.stdout.starts_with("100644 ")
        || listing.stdout.starts_with("100755 "))
        && !public_path.is_symlink()
        && public_path.is_file()
        && crate::config::Config::load_compatible(repo)
            .ok()
            .is_some_and(|config| config.requires_object_versioning());
    Ok(crate::native_s3::is_legacy_cas_configuration_with_public_route(&raw, public_route))
}

pub(crate) fn cas_key_candidates(digest: &str, algorithm: &str) -> Result<Vec<String>> {
    Checksum {
        algorithm: algorithm.into(),
        digest: digest.trim_end_matches(".dir").into(),
    }
    .validate()?;
    let (prefix, rest) = digest.split_at(2);
    Ok(match algorithm {
        "md5" => vec![
            format!("files/md5/{prefix}/{rest}"),
            format!("{prefix}/{rest}"),
        ],
        "md5-dos2unix" => vec![
            format!("{prefix}/{rest}"),
            format!("files/md5-dos2unix/{prefix}/{rest}"),
            format!("files/md5/{prefix}/{rest}"),
        ],
        _ => unreachable!("validated checksum algorithm"),
    })
}

fn import_manifest_inner(raw: &str, origin: &str, root: Option<&Path>) -> Result<Manifest> {
    let algorithm = hash_algorithm(raw, origin)?;
    let yaml: Value =
        serde_yaml::from_str(raw).map_err(|error| Error::message(error.to_string()))?;
    for key in yaml
        .as_mapping()
        .into_iter()
        .flat_map(|mapping| mapping.keys())
    {
        if !matches!(key.as_str(), Some("outs")) {
            return Err(Error::message(format!(
                "unsupported legacy DVC document field: {origin}"
            )));
        }
    }
    for key in yaml["outs"][0]
        .as_mapping()
        .into_iter()
        .flat_map(|mapping| mapping.keys())
    {
        if !matches!(
            key.as_str(),
            Some(
                "path"
                    | "hash"
                    | "md5"
                    | "size"
                    | "nfiles"
                    | "files"
                    | "cloud"
                    | "cache"
                    | "can_push"
                    | "push"
                    | "remote"
            )
        ) {
            return Err(Error::message(format!(
                "unsupported legacy DVC output field: {origin}"
            )));
        }
    }
    let validate_cloud = |value: &Value| -> Result<()> {
        if let Some(cloud) = value.get("cloud") {
            let mapping = cloud
                .as_mapping()
                .ok_or_else(|| Error::message("invalid legacy cloud version mapping"))?;
            if mapping
                .keys()
                .any(|key| key.as_str() != Some("workspace-mgr"))
            {
                return Err(Error::message(
                    "legacy cloud versions name an unsupported remote; migration refuses to discard exact version references",
                ));
            }
            if let Some(remote) = cloud.get("workspace-mgr") {
                let fields = remote
                    .as_mapping()
                    .ok_or_else(|| Error::message("invalid legacy exact version binding"))?;
                if fields
                    .keys()
                    .any(|key| !matches!(key.as_str(), Some("version_id" | "etag")))
                {
                    return Err(Error::message(
                        "unsupported legacy exact version binding field",
                    ));
                }
                if !matches!(remote.get("version_id"), Some(Value::String(id)) if !id.is_empty())
                    && !matches!(remote.get("version_id"), Some(Value::Number(_)))
                {
                    return Err(Error::message(
                        "legacy cloud binding has no valid exact version ID",
                    ));
                }
                if remote
                    .get("etag")
                    .is_some_and(|etag| !matches!(etag, Value::String(value) if !value.is_empty()))
                {
                    return Err(Error::message("legacy cloud binding has an invalid ETag"));
                }
            }
        }
        Ok(())
    };
    validate_cloud(&yaml["outs"][0])?;
    for file in yaml["outs"][0]
        .get("files")
        .and_then(Value::as_sequence)
        .into_iter()
        .flatten()
    {
        for key in file
            .as_mapping()
            .into_iter()
            .flat_map(|mapping| mapping.keys())
        {
            if !matches!(
                key.as_str(),
                Some("relpath" | "md5" | "size" | "cloud" | "remote")
            ) {
                return Err(Error::message("unsupported legacy directory entry field"));
            }
        }
        validate_cloud(file)?;
    }
    let out = parse_document(raw, origin)?
        .outs
        .into_iter()
        .next()
        .ok_or_else(|| Error::message("legacy DVC metadata has no output"))?;
    let digest = out
        .md5
        .as_deref()
        .ok_or_else(|| Error::message("legacy DVC metadata has no MD5 checksum"))?;
    let kind = if digest.ends_with(".dir") {
        Kind::Directory
    } else {
        Kind::File
    };
    let path = repo_path(&out.path, "legacy DVC output")?;
    if path != out.path || path.contains('/') {
        return Err(Error::message(
            "legacy DVC pointer must name its adjacent output",
        ));
    }
    let checksum = Checksum {
        algorithm: algorithm.clone(),
        digest: digest.trim_end_matches(".dir").to_owned(),
    };
    checksum.validate()?;
    let binding = |id: Option<String>, etag| id.map(|id| Version { id, etag });
    let mut manifest = Manifest {
        schema_version: 1,
        path,
        kind,
        checksum,
        size: out
            .size
            .ok_or_else(|| Error::message("legacy DVC metadata has no physical size"))?,
        version: binding(out.version_id, out.etag),
        entries: None,
    };
    if kind == Kind::Directory {
        if manifest.version.is_some() {
            return Err(Error::message(
                "legacy directory aggregate has an exact version binding that native entry metadata cannot represent; migration refuses to discard it",
            ));
        }
        let files = match out.files {
            Some(files) => files,
            None => {
                let root = root.ok_or_else(|| {
                    Error::message(
                        "legacy directory has no file list; its local cache is required for import",
                    )
                })?;
                let cache = existing_cache(root, digest, &algorithm).ok_or_else(|| Error::message("legacy directory cache manifest is unavailable; hydrate it before migration"))?;
                let bytes = fs::read(cache).map_err(|error| Error::message(error.to_string()))?;
                use md5::Digest;
                if crate::hex::encode_lower(md5::Md5::digest(&bytes))
                    != digest.trim_end_matches(".dir")
                {
                    return Err(Error::message(
                        "legacy directory cache manifest checksum mismatch",
                    ));
                }
                let rows: Vec<serde_json::Value> =
                    serde_json::from_slice(&bytes).map_err(|error| {
                        Error::message(format!("invalid legacy directory cache manifest: {error}"))
                    })?;
                rows.into_iter()
                    .map(|row| {
                        Ok(PointerFileVersion {
                            relpath: row["relpath"]
                                .as_str()
                                .ok_or_else(|| {
                                    Error::message("legacy directory entry has no path")
                                })?
                                .into(),
                            md5: row["md5"].as_str().map(str::to_owned),
                            size: row["size"].as_u64(),
                            version_id: None,
                            etag: None,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?
            }
        };
        let entries = files.into_iter().map(|file| {
            let digest = file.md5.ok_or_else(|| Error::message("legacy directory entry has no MD5 checksum"))?;
            let size = file.size.or_else(|| root.and_then(|root| existing_cache(root, &digest, &algorithm)).and_then(|path| fs::metadata(path).ok()).map(|meta| meta.len())).ok_or_else(|| Error::message("legacy directory entry physical size is unavailable; hydrate it before migration"))?;
            Ok(Entry { path: file.relpath, checksum: Checksum { algorithm: algorithm.clone(), digest }, size, version: binding(file.version_id, file.etag) })
        }).collect::<Result<Vec<_>>>()?;
        if yaml["outs"][0]
            .get("nfiles")
            .is_some_and(|count| count.as_u64() != Some(entries.len() as u64))
        {
            return Err(Error::message(
                "legacy directory entry count does not match metadata",
            ));
        }
        let logical = entries
            .iter()
            .map(|entry| PointerFileVersion {
                relpath: entry.path.clone(),
                md5: Some(entry.checksum.digest.clone()),
                size: Some(entry.size),
                version_id: None,
                etag: None,
            })
            .collect::<Vec<_>>();
        if directory_digest(&logical)? != digest {
            return Err(Error::message(
                "legacy directory manifest checksum mismatch",
            ));
        }
        manifest.checksum.digest = crate::storage_format::directory_digest(&entries)?;
        manifest.entries = Some(entries);
    } else if out.files.is_some() {
        return Err(Error::message(
            "legacy file output unexpectedly contains directory entries",
        ));
    }
    manifest.validate(origin)?;
    Ok(manifest)
}

/// Legacy layouts are only read here, including caches relocated by migration.
pub(crate) fn existing_cache(root: &Path, digest: &str, algorithm: &str) -> Option<PathBuf> {
    Checksum {
        algorithm: algorithm.to_owned(),
        digest: digest.trim_end_matches(".dir").to_owned(),
    }
    .validate()
    .ok()?;
    let layouts = cas_key_candidates(digest, algorithm).ok()?;
    let repo = crate::git::GitRepo {
        root: root.to_path_buf(),
    };
    let native = crate::local_state::directory_unmigrated(&repo)
        .map(|local| local.join("cache"))
        .unwrap_or_else(|_| root.join(".workspace-mgr/local/cache"));
    [
        native.clone(),
        native.join("legacy"),
        cache_dir(&root.join(".dvc")),
    ]
    .into_iter()
    .flat_map(|cache| layouts.iter().map(move |layout| cache.join(layout)))
    .find(|path| path.is_file())
}

pub(crate) fn remote_location(root: &Path) -> Result<Option<(String, Option<String>)>> {
    let path = root.join(".dvc/config");
    if !path.is_file() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path).map_err(|error| Error::message(error.to_string()))?;
    let field = |name: &str| {
        raw.lines()
            .map(str::trim)
            .find_map(|line| line.strip_prefix(&format!("{name} = ")).map(str::to_owned))
    };
    Ok(field("url").map(|url| (url, field("endpointurl"))))
}

#[derive(Debug, Deserialize)]
struct ManifestEntry {
    md5: String,
    relpath: String,
}

pub(crate) fn cache_dir(engine_dir: &Path) -> PathBuf {
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
pub(crate) fn stored_object(stores: &[PathBuf], md5: &str, suffix: &str) -> Option<PathBuf> {
    if md5.len() != 32 || !md5.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let (prefix, rest) = md5.split_at(2);
    let name = format!("{rest}{suffix}");
    stores
        .iter()
        .flat_map(|store| {
            [
                store.join("objects/md5").join(prefix).join(&name),
                store.join("objects/md5-dos2unix").join(prefix).join(&name),
                store.join("files/md5").join(prefix).join(&name),
                store.join(prefix).join(&name),
            ]
        })
        .find(|path| path.is_file())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct TreeEntry {
    pub md5: String,
    pub relpath: String,
}

pub(crate) fn ascii_json_string(value: &str) -> Result<String> {
    let rendered = serde_json::to_string(value).map_err(|e| Error::message(e.to_string()))?;
    let mut ascii = String::new();
    for character in rendered.chars() {
        if character.is_ascii() {
            ascii.push(character);
        } else {
            let mut buffer = [0u16; 2];
            for code in character.encode_utf16(&mut buffer) {
                ascii.push_str(&format!("\\u{code:04x}"));
            }
        }
    }
    Ok(ascii)
}

pub(crate) fn tree_bytes(files: &[TreeEntry]) -> Result<Vec<u8>> {
    let mut files = files.to_vec();
    files.sort_by(|a, b| a.relpath.cmp(&b.relpath));
    let mut seen = BTreeSet::new();
    let mut rows = Vec::new();
    for file in files {
        if repo_path(&file.relpath, "directory manifest path")? != file.relpath {
            return Err(Error::message("directory manifest path is not canonical"));
        }
        Checksum {
            algorithm: "md5".into(),
            digest: file.md5.trim_end_matches(".dir").to_owned(),
        }
        .validate()?;
        if file.md5.ends_with(".dir") || !seen.insert(file.relpath.clone()) {
            return Err(Error::message(
                "invalid or duplicate directory manifest entry",
            ));
        }
        rows.push(format!(
            "{{\"md5\": {}, \"relpath\": {}}}",
            ascii_json_string(&file.md5)?,
            ascii_json_string(&file.relpath)?
        ));
    }
    Ok(format!("[{}]", rows.join(", ")).into_bytes())
}

pub(crate) fn tree_manifest_bytes(files: &[PointerFileVersion]) -> Result<Vec<u8>> {
    let files = files
        .iter()
        .map(|file| {
            Ok(TreeEntry {
                relpath: file.relpath.clone(),
                md5: file
                    .md5
                    .clone()
                    .ok_or_else(|| Error::message("directory manifest entry has no MD5 digest"))?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    tree_bytes(&files)
}

pub(crate) fn directory_digest(files: &[PointerFileVersion]) -> Result<String> {
    Ok(format!(
        "{}.dir",
        crate::hex::encode_lower(Md5::digest(tree_manifest_bytes(files)?))
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_remote_normalization_preserves_file_and_directory_exact_bindings() {
        let raw = "outs:\n- path: data\n  hash: md5\n  md5: 0cc175b9c0f1b6a831c399e269772661\n  size: 1\n  remote: research-data\n  cloud:\n    research-data:\n      version_id: file-original\n      etag: file-etag\n  files:\n  - relpath: a\n    md5: 0cc175b9c0f1b6a831c399e269772661\n    size: 1\n    remote: research-data\n    cloud:\n      research-data:\n        version_id: entry-original\n        etag: entry-etag\n";
        let normalized = normalize_remote_binding(raw, "data.dvc", Some("research-data")).unwrap();
        assert_eq!(hash_algorithm(&normalized, "data.dvc").unwrap(), "md5");
        let document = parse_document(&normalized, "data.dvc").unwrap();
        assert_eq!(
            document.outs[0].version_id.as_deref(),
            Some("file-original")
        );
        assert_eq!(
            document.outs[0].files.as_ref().unwrap()[0]
                .version_id
                .as_deref(),
            Some("entry-original")
        );
        assert!(normalize_remote_binding(raw, "data.dvc", Some("another-remote")).is_err());
        assert!(
            normalize_remote_binding(
                &raw.replace("    research-data:", "    unselected:"),
                "data.dvc",
                Some("research-data")
            )
            .is_err()
        );
    }

    #[test]
    fn normalized_text_cache_resolves_algorithm_specific_file_and_directory_objects() {
        let fixture = tempfile::tempdir().unwrap();
        let digest = "0cc175b9c0f1b6a831c399e269772661";
        let directory = fixture.path().join(".dvc/cache/files/md5-dos2unix/0c");
        fs::create_dir_all(&directory).unwrap();
        for suffix in ["", ".dir"] {
            let object = directory.join(format!("{}{suffix}", &digest[2..]));
            fs::write(&object, b"fixture").unwrap();
            assert_eq!(
                existing_cache(fixture.path(), &format!("{digest}{suffix}"), "md5-dos2unix"),
                Some(object)
            );
            assert_eq!(
                existing_cache(fixture.path(), &format!("{digest}{suffix}"), "md5"),
                None
            );
        }
    }

    #[test]
    fn directory_cache_matches_dvc_three_format_including_unicode() {
        let files = vec![
            TreeEntry {
                md5: "fbade9e36a3f36d3d676c1b808451dd7".into(),
                relpath: "z".into(),
            },
            TreeEntry {
                md5: "0cc175b9c0f1b6a831c399e269772661".into(),
                relpath: "a".into(),
            },
        ];
        let bytes = tree_bytes(&files).unwrap();
        assert_eq!(
            crate::hex::encode_lower(Md5::digest(&bytes)),
            "d644a076696b7772d39ef7bc795f874d"
        );
        assert_eq!(
            ascii_json_string("中文/😀").unwrap(),
            "\"\\u4e2d\\u6587/\\ud83d\\ude00\""
        );
        assert!(
            tree_bytes(&[TreeEntry {
                md5: files[0].md5.clone(),
                relpath: "../escaped".into()
            }])
            .is_err()
        );
        assert!(tree_bytes(&[files[0].clone(), files[0].clone()]).is_err());
    }

    #[test]
    fn import_keeps_checksum_size_and_exact_version_and_rejects_unrepresentable_fields() {
        let raw = "outs:\n- path: data\n  hash: md5\n  md5: 0cc175b9c0f1b6a831c399e269772661\n  size: 1\n  cloud:\n    workspace-mgr:\n      version_id: original\n      etag: e1\n";
        let native = import_manifest(raw, "data.dvc").unwrap();
        assert_eq!(native.version.as_ref().unwrap().id, "original");
        assert_eq!(native.size, 1);
        assert_eq!(native.checksum.digest, "0cc175b9c0f1b6a831c399e269772661");
        assert!(import_manifest(&raw.replace("workspace-mgr:", "other:"), "data.dvc").is_err());
        assert!(import_manifest(&format!("{raw}  metric: true\n"), "data.dvc").is_err());
        assert!(
            import_manifest(
                &raw.replace("version_id: original", "version_id: ''"),
                "data.dvc"
            )
            .is_err()
        );
    }

    #[test]
    fn legacy_hash_defaults_only_when_the_field_is_absent() {
        let base = "outs:\n- path: data\n  md5: 900150983cd24fb0d6963f7d28e17f72\n  size: 3\n";
        assert_eq!(hash_algorithm(base, "data.dvc").unwrap(), "md5-dos2unix");
        assert_eq!(
            import_manifest(base, "data.dvc")
                .unwrap()
                .checksum
                .algorithm,
            "md5-dos2unix"
        );
        for algorithm in ["md5", "md5-dos2unix"] {
            let raw = format!("{base}  hash: {algorithm}\n");
            assert_eq!(hash_algorithm(&raw, "data.dvc").unwrap(), algorithm);
        }
        for value in ["null", "123", "true", "{name: md5}", "[md5]"] {
            let raw = format!("{base}  hash: {value}\n");
            let error = import_manifest(&raw, "data.dvc").unwrap_err().to_string();
            assert!(
                error.contains("hash algorithm must be a string"),
                "{value} was not rejected by the checksum parser: {error}"
            );
            assert!(error.contains("data.dvc"));
        }
    }
}
