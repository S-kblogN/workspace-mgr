//! Embedded DVC-compatible hashing, metadata, cache and local materialization.
//! Network archive/version policy lives in the native transport adapters.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use serde_yaml::{Mapping, Value as Yaml};
use walkdir::WalkDir;

use crate::dvc;
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::path::{reject_symlink_traversal, repo_path, resolved_under, to_slash};
use crate::process::CommandOutput;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct TreeEntry {
    md5: String,
    relpath: String,
}

#[derive(Debug, Clone)]
struct FileState {
    relpath: String,
    md5: String,
    size: u64,
}

pub(crate) fn execute(cwd: &Path, args: Vec<String>) -> Result<CommandOutput> {
    #[cfg(feature = "test-storage")]
    {
        if let Some(hook) = std::env::var_os("WORKSPACE_MGR_TEST_STORAGE_HOOK") {
            let hook = hook
                .to_str()
                .ok_or_else(|| Error::message("storage test hook is not UTF-8"))?;
            crate::process::run(hook, &args, cwd)?;
        }
        if let Some(program) = std::env::var_os("WORKSPACE_MGR_STORAGE_DVC") {
            let program = program
                .to_str()
                .ok_or_else(|| Error::message("storage test engine is not UTF-8"))?;
            return crate::process::run_unchecked(program, &args, cwd);
        }
    }
    let repo = GitRepo {
        root: cwd.canonicalize().at(cwd)?,
    };
    match execute_inner(&repo, &args) {
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

fn execute_inner(repo: &GitRepo, args: &[String]) -> Result<(i32, String)> {
    let command = args.first().map(String::as_str).unwrap_or("");
    let targets = targets(args);
    match command {
        "--version" => Ok((0, format!("native Rust {}\n", env!("CARGO_PKG_VERSION")))),
        "init" => {
            initialize(repo)?;
            Ok((0, String::new()))
        }
        "add" => {
            for target in targets {
                add(repo, &target)?;
            }
            Ok((0, String::new()))
        }
        "commit" => {
            for pointer in select_pointers(repo, &targets)? {
                commit(repo, &pointer)?;
            }
            Ok((0, String::new()))
        }
        "push" => {
            push(repo, &select_pointers(repo, &targets)?)?;
            Ok((0, String::new()))
        }
        "fetch" => {
            fetch(repo, &select_pointers(repo, &targets)?)?;
            Ok((0, String::new()))
        }
        "checkout" => {
            checkout(repo, &select_pointers(repo, &targets)?)?;
            Ok((0, String::new()))
        }
        "move" if targets.len() == 2 => {
            move_output(repo, &targets[0], &targets[1])?;
            Ok((0, String::new()))
        }
        "remove" => {
            for pointer in select_pointers(repo, &targets)? {
                remove(repo, &pointer)?;
            }
            Ok((0, String::new()))
        }
        "status" => status(
            repo,
            &select_pointers(repo, &targets)?,
            args.iter().any(|a| a == "--cloud"),
            args.iter().any(|a| a == "--quiet" || a == "-q"),
        ),
        "data" if args.get(1).map(String::as_str) == Some("status") => {
            let value = data_status(repo, &targets)?;
            Ok((
                0,
                serde_json::to_string(&value).map_err(|e| Error::message(e.to_string()))?,
            ))
        }
        other => Err(Error::message(format!(
            "unsupported embedded storage command: {other}"
        ))),
    }
}

fn targets(args: &[String]) -> Vec<String> {
    let start = args.iter().position(|a| a == "--").map(|i| i + 1);
    match start {
        Some(start) => args[start..].to_vec(),
        None => args
            .iter()
            .skip(if args.first().is_some_and(|a| a == "data") {
                2
            } else {
                1
            })
            .filter(|a| !a.starts_with('-'))
            .cloned()
            .collect(),
    }
}

fn initialize(repo: &GitRepo) -> Result<()> {
    for path in [".dvc", ".dvc/cache", ".dvc/tmp"] {
        reject_symlink_traversal(&repo.root, path, "storage initialization")?;
        fs::create_dir_all(repo.root.join(path)).at(repo.root.join(path))?;
    }
    for (path, bytes) in [
        (".dvc/config", ""),
        (".dvc/.gitignore", "/config.local\n/tmp\n/cache\n"),
        (
            ".dvcignore",
            "# Add patterns of files dvc should ignore, which could improve\n# the performance. To learn more about .dvcignore, visit\n# https://dvc.org/doc/user-guide/dvcignore\n",
        ),
    ] {
        reject_symlink_traversal(&repo.root, path, "storage initialization")?;
        if !repo.root.join(path).exists() {
            atomic_write(&repo.root.join(path), bytes.as_bytes())?;
        }
    }
    Ok(())
}

pub(crate) fn cache_root(repo: &GitRepo) -> Result<PathBuf> {
    let path = dvc::cache_dir(&repo.root.join(".dvc"));
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
        "md5" => format!("files/md5/{prefix}/{rest}"),
        "md5-dos2unix" => format!("{prefix}/{rest}"),
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
    // Older caches and interrupted native upgrades can contain the same
    // object in the other layout. Every consumer still checks its digest.
    let alternate = cache_path_with_algorithm(
        repo,
        digest,
        if hash_name == "md5" {
            "md5-dos2unix"
        } else {
            "md5"
        },
    )?;
    Ok(if alternate.is_file() {
        alternate
    } else {
        preferred
    })
}

pub(crate) fn file_digest(path: &Path, hash_name: &str) -> Result<String> {
    let mut file = fs::File::open(path).at(path)?;
    stream_digest(&mut file, path, hash_name)
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

fn ascii_json_string(value: &str) -> Result<String> {
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

fn tree_bytes(files: &[TreeEntry]) -> Result<Vec<u8>> {
    let mut files = files.to_vec();
    files.sort_by(|a, b| a.relpath.cmp(&b.relpath));
    let mut seen = BTreeSet::new();
    let mut rows = Vec::new();
    for file in files {
        if repo_path(&file.relpath, "directory manifest path")? != file.relpath {
            return Err(Error::message("directory manifest path is not canonical"));
        }
        digest_parts(&file.md5)?;
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

pub(crate) fn tree_manifest_bytes(files: &[dvc::PointerFileVersion]) -> Result<Vec<u8>> {
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

pub(crate) fn directory_digest(files: &[dvc::PointerFileVersion]) -> Result<String> {
    Ok(format!(
        "{}.dir",
        crate::hex::encode_lower(Md5::digest(tree_manifest_bytes(files)?))
    ))
}

fn read_yaml(repo: &GitRepo, pointer: &str) -> Result<Yaml> {
    let pointer = repo_path(pointer, "storage metadata")?;
    reject_symlink_traversal(&repo.root, &pointer, "storage metadata")?;
    let path = resolved_under(&repo.root, &pointer);
    if !fs::symlink_metadata(&path).at(&path)?.is_file() {
        return Err(Error::message("storage metadata must be a regular file"));
    }
    let bytes = fs::read(&path).at(&path)?;
    serde_yaml::from_slice(&bytes).map_err(|source| Error::Yaml { path, source })
}

fn write_yaml(repo: &GitRepo, pointer: &str, document: &Yaml) -> Result<()> {
    let bytes = serde_yaml::to_string(document).map_err(|e| Error::message(e.to_string()))?;
    atomic_write(&resolved_under(&repo.root, pointer), bytes.as_bytes())
}

fn output_object(pointer: &str, path: &str) -> Result<String> {
    let parent = Path::new(pointer).parent().unwrap_or_else(|| Path::new(""));
    repo_path(&to_slash(&parent.join(path)), "storage output")
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
        let parsed = dvc::parse_pointer_document(&raw, pointer)?;
        let yaml: Yaml = serde_yaml::from_str(&raw).map_err(|e| Error::message(e.to_string()))?;
        if parsed.outs.is_empty() {
            return Err(Error::message(format!(
                "storage metadata has no outputs: {pointer}"
            )));
        }
        for (index, out) in parsed.outs.iter().enumerate() {
            validate_transport_settings(&yaml["outs"][index], pointer)?;
            let hash_name = yaml["outs"][index]["hash"]
                .as_str()
                .unwrap_or("md5-dos2unix")
                .to_owned();
            if !matches!(hash_name.as_str(), "md5" | "md5-dos2unix") {
                return Err(Error::message(format!(
                    "unsupported storage hash algorithm: {hash_name}"
                )));
            }
            if let Some(digest) = &out.md5 {
                digest_parts(digest)?;
            }
            let object = output_object(pointer, &out.path)?;
            if let Some(files) = &out.files {
                if out.md5.as_deref() != Some(directory_digest(files)?.as_str()) {
                    return Err(Error::message(format!(
                        "directory manifest hash mismatch: {pointer}"
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
            } else if out.md5.as_deref().is_some_and(|md5| md5.ends_with(".dir")) {
                return Err(Error::message(format!(
                    "directory metadata is incomplete: {pointer}; restore its published file/version manifest"
                )));
            } else {
                entries.push(StorageEntry {
                    pointer: pointer.clone(),
                    object,
                    md5: out.md5.clone(),
                    size: out.size,
                    version_id: out.version_id.clone(),
                    etag: out.etag.clone(),
                    hash_name,
                });
            }
        }
    }
    Ok(entries)
}

fn validate_transport_settings(output: &Yaml, pointer: &str) -> Result<()> {
    for key in ["cache", "can_push", "push"] {
        if let Some(value) = output.get(key) {
            if value.as_bool() != Some(true) {
                return Err(Error::message(format!(
                    "storage output disables or invalidates {key}: {pointer}"
                )));
            }
        }
    }
    if let Some(remote) = output.get("remote") {
        if remote.as_str() != Some(dvc::INTERNAL_REMOTE) {
            return Err(Error::message(format!(
                "storage output uses a different remote: {pointer}"
            )));
        }
    }
    if let Some(files) = output.get("files").and_then(Yaml::as_sequence) {
        for file in files {
            if let Some(remote) = file.get("remote") {
                if remote.as_str() != Some(dvc::INTERNAL_REMOTE) {
                    return Err(Error::message(format!(
                        "directory file uses a different storage remote: {pointer}"
                    )));
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn install_directory_manifests(repo: &GitRepo, pointers: &[String]) -> Result<()> {
    for pointer in pointers {
        let yaml = read_yaml(repo, pointer)?;
        for (index, out) in dvc::read_pointer_document(repo, pointer)?
            .outs
            .into_iter()
            .enumerate()
        {
            let hash_name = yaml["outs"][index]["hash"]
                .as_str()
                .unwrap_or("md5-dos2unix");
            if let Some(files) = out.files {
                let bytes = tree_manifest_bytes(&files)?;
                let digest = format!("{}.dir", crate::hex::encode_lower(Md5::digest(&bytes)));
                if out.md5.as_deref() != Some(&digest) {
                    return Err(Error::message("directory manifest hash mismatch"));
                }
                if hash_name == "md5" {
                    install_cache(repo, &digest, &bytes)?;
                } else {
                    install_cache_with_algorithm(repo, &digest, &bytes, hash_name)?;
                }
            }
        }
    }
    Ok(())
}

fn select_pointers(repo: &GitRepo, targets: &[String]) -> Result<Vec<String>> {
    if targets.is_empty() {
        return dvc::discover(repo, &[]);
    }
    let mut pointers = BTreeSet::new();
    for target in targets {
        let target = repo_path(target, "storage target")?;
        let pointer = if target.ends_with(".dvc") {
            target.clone()
        } else {
            format!("{target}.dvc")
        };
        if repo.root.join(&pointer).is_file() {
            pointers.insert(pointer);
        } else {
            for pointer in dvc::discover(repo, std::slice::from_ref(&target))? {
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
        // DVC records a file symlink's target bytes. Directory links cannot
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
        });
    }
    files.sort_by(|a, b| a.relpath.cmp(&b.relpath));
    Ok(files)
}

fn recorded_files(
    repo: &GitRepo,
    pointer: &str,
    out: &dvc::PointerOutput,
    algorithm: &str,
) -> Result<Vec<FileState>> {
    if let Some(files) = &out.files {
        if out.md5.as_deref() != Some(directory_digest(files)?.as_str()) {
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
            })
        })
        .collect()
}

fn put(map: &mut Mapping, key: &str, value: Yaml) {
    map.insert(Yaml::String(key.to_owned()), value);
}
fn string(value: impl Into<String>) -> Yaml {
    Yaml::String(value.into())
}

fn clear_internal_cloud(map: &mut Mapping) {
    if let Some(Yaml::Mapping(cloud)) = map.get_mut(string("cloud")) {
        cloud.remove(string(dvc::INTERNAL_REMOTE));
        if cloud.is_empty() {
            map.remove(string("cloud"));
        }
    }
}

fn add(repo: &GitRepo, target: &str) -> Result<()> {
    let object = repo_path(target, "storage output")?;
    dvc::require_addressable(
        &object,
        "storage output",
        "choose a path without backslashes",
    )?;
    let pointer = format!("{object}.dvc");
    reject_symlink_traversal(&repo.root, &pointer, "storage metadata")?;
    let parent = Path::new(&object).parent().unwrap_or_else(|| Path::new(""));
    let filename = Path::new(&object)
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| Error::message("output name is not UTF-8"))?;
    let mut map = Mapping::new();
    put(&mut map, "path", string(filename));
    put(&mut map, "hash", string("md5"));
    let mut document = Yaml::Mapping(Mapping::from_iter([(
        string("outs"),
        Yaml::Sequence(vec![Yaml::Mapping(map)]),
    )]));
    update_document(repo, &pointer, &mut document)?;
    write_yaml(repo, &pointer, &document)?;
    update_ignore(&repo.root.join(parent).join(".gitignore"), filename, true)
}

fn commit(repo: &GitRepo, pointer: &str) -> Result<()> {
    let mut document = read_yaml(repo, pointer)?;
    update_document(repo, pointer, &mut document)?;
    write_yaml(repo, pointer, &document)
}

fn version_aware(repo: &GitRepo) -> bool {
    fs::read_to_string(repo.root.join(".dvc/config"))
        .ok()
        .is_some_and(|raw| {
            raw.lines()
                .any(|line| line.trim() == "version_aware = true")
        })
}

fn update_document(repo: &GitRepo, pointer: &str, document: &mut Yaml) -> Result<()> {
    let outs = document
        .get_mut("outs")
        .and_then(Yaml::as_sequence_mut)
        .ok_or_else(|| Error::message("metadata has no outputs"))?;
    for out in outs {
        let map = out
            .as_mapping_mut()
            .ok_or_else(|| Error::message("invalid metadata output"))?;
        let name = map
            .get(string("path"))
            .and_then(Yaml::as_str)
            .ok_or_else(|| Error::message("metadata output has no path"))?;
        let object = output_object(pointer, name)?;
        let path = repo.root.join(&object);
        if !path.exists() {
            return Err(Error::message(format!(
                "storage output is missing: {object}"
            )));
        }
        let algorithm = map
            .get(string("hash"))
            .and_then(Yaml::as_str)
            .unwrap_or("md5-dos2unix")
            .to_owned();
        let files = current_files_with_algorithm(repo, &object, &algorithm)?;
        let old_digest = map
            .get(string("md5"))
            .and_then(Yaml::as_str)
            .map(ToOwned::to_owned);
        let size = files.iter().map(|f| f.size).sum::<u64>();
        let digest;
        if path.is_dir() {
            let tree = files
                .iter()
                .map(|f| TreeEntry {
                    relpath: f.relpath.clone(),
                    md5: f.md5.clone(),
                })
                .collect::<Vec<_>>();
            let bytes = tree_bytes(&tree)?;
            digest = format!("{}.dir", crate::hex::encode_lower(Md5::digest(&bytes)));
            install_cache_with_algorithm(repo, &digest, &bytes, &algorithm)?;
            let previous = map
                .get(string("files"))
                .and_then(Yaml::as_sequence)
                .cloned()
                .unwrap_or_default();
            if version_aware(repo) || !previous.is_empty() {
                let mut rows = Vec::new();
                for file in &files {
                    let mut row = previous
                        .iter()
                        .find(|row| row["relpath"].as_str() == Some(&file.relpath))
                        .and_then(Yaml::as_mapping)
                        .cloned()
                        .unwrap_or_default();
                    if row.get(string("md5")).and_then(Yaml::as_str) != Some(&file.md5)
                        || row
                            .get(string("size"))
                            .and_then(Yaml::as_u64)
                            .is_some_and(|size| size != file.size)
                    {
                        clear_internal_cloud(&mut row);
                    }
                    put(&mut row, "relpath", string(&file.relpath));
                    put(&mut row, "md5", string(&file.md5));
                    put(&mut row, "size", Yaml::Number(file.size.into()));
                    rows.push(Yaml::Mapping(row));
                }
                put(map, "files", Yaml::Sequence(rows));
            }
            put(map, "nfiles", Yaml::Number((files.len() as u64).into()));
        } else {
            digest = files
                .first()
                .ok_or_else(|| Error::message("storage output is not a file"))?
                .md5
                .clone();
            map.remove(string("files"));
            map.remove(string("nfiles"));
        }
        for file in &files {
            let source = if file.relpath.is_empty() {
                path.clone()
            } else {
                path.join(&file.relpath)
            };
            install_cache_file_with_algorithm(repo, &file.md5, &source, &algorithm)?;
        }
        if old_digest.as_deref() != Some(&digest)
            || map
                .get(string("size"))
                .and_then(Yaml::as_u64)
                .is_some_and(|previous| previous != size)
        {
            clear_internal_cloud(map);
        }
        put(map, "md5", string(digest));
        put(map, "size", Yaml::Number(size.into()));
        if map.contains_key(string("hash")) {
            put(map, "hash", string(algorithm));
        }
    }
    Ok(())
}

fn remote_root(repo: &GitRepo) -> Result<String> {
    dvc::internal_location(repo)?
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
        repo.root.join(".dvc").join(root)
    };
    let relative = match hash_name {
        "md5" => format!("files/md5/{first}/{rest}"),
        "md5-dos2unix" => format!("{first}/{rest}"),
        _ => return Err(Error::message("unsupported storage hash algorithm")),
    };
    reject_symlink_traversal(&root, &relative, "filesystem storage object")?;
    Ok(root.join(relative))
}

fn algorithms_for_pointer(repo: &GitRepo, pointer: &str) -> Result<BTreeMap<String, String>> {
    let yaml = read_yaml(repo, pointer)?;
    let mut hashes = BTreeMap::new();
    for (index, out) in dvc::read_pointer_document(repo, pointer)?
        .outs
        .iter()
        .enumerate()
    {
        let algorithm = yaml["outs"][index]["hash"]
            .as_str()
            .unwrap_or("md5-dos2unix");
        if let Some(digest) = &out.md5 {
            hashes.insert(digest.clone(), algorithm.to_owned());
        }
        for file in recorded_files(repo, pointer, out, algorithm)? {
            hashes.insert(file.md5, algorithm.to_owned());
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

fn cloud_binding(map: &mut Mapping, version: &str, etag: &str) {
    let cloud = map
        .entry(string("cloud"))
        .or_insert_with(|| Yaml::Mapping(Mapping::new()));
    if !cloud.is_mapping() {
        *cloud = Yaml::Mapping(Mapping::new());
    }
    let remote = cloud
        .as_mapping_mut()
        .expect("mapping")
        .entry(string(dvc::INTERNAL_REMOTE))
        .or_insert_with(|| Yaml::Mapping(Mapping::new()));
    if !remote.is_mapping() {
        *remote = Yaml::Mapping(Mapping::new());
    }
    let remote = remote.as_mapping_mut().expect("mapping");
    put(remote, "version_id", string(version));
    put(remote, "etag", string(etag));
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
            let source = existing_cache_with_algorithm(repo, digest, &entry.hash_name)?;
            if file_digest(&source, &entry.hash_name)? != digest {
                return Err(Error::message("storage cache content hash mismatch"));
            }
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
            let (version, etag) = upload_version(
                &client,
                repo,
                &entry,
                &source,
                5 * (1 << 30),
                64 * (1 << 20),
            )?;
            let mut document = read_yaml(repo, pointer)?;
            let mut bound = false;
            for out in document["outs"]
                .as_sequence_mut()
                .ok_or_else(|| Error::message("metadata has no outputs"))?
            {
                let object = output_object(
                    pointer,
                    out["path"]
                        .as_str()
                        .ok_or_else(|| Error::message("output has no path"))?,
                )?;
                if let Some(files) = out.get_mut("files").and_then(Yaml::as_sequence_mut) {
                    for file in files {
                        if format!("{object}/{}", file["relpath"].as_str().unwrap_or(""))
                            == entry.object
                        {
                            if !binding_is_unchanged(file, &entry) {
                                return Err(Error::message(
                                    "storage metadata changed during upload; retry after reconciling the pointer",
                                ));
                            }
                            cloud_binding(
                                file.as_mapping_mut()
                                    .ok_or_else(|| Error::message("invalid directory file"))?,
                                &version,
                                &etag,
                            );
                            bound = true;
                        }
                    }
                } else if object == entry.object {
                    if !binding_is_unchanged(out, &entry) {
                        return Err(Error::message(
                            "storage metadata changed during upload; retry after reconciling the pointer",
                        ));
                    }
                    cloud_binding(
                        out.as_mapping_mut()
                            .ok_or_else(|| Error::message("invalid output"))?,
                        &version,
                        &etag,
                    );
                    bound = true;
                }
            }
            if !bound {
                return Err(Error::message(
                    "storage output changed during upload; its exact version remains recorded in the private upload journal",
                ));
            }
            // Each successful upload's exact ID is durable before the next put.
            write_yaml(repo, pointer, &document)?;
        }
    }
    Ok(())
}

fn binding_is_unchanged(row: &Yaml, entry: &StorageEntry) -> bool {
    let binding = &row["cloud"][dvc::INTERNAL_REMOTE];
    let expected = |field: &str, value: Option<&str>| match value {
        Some(value) => binding[field].as_str() == Some(value),
        None => binding[field].is_null(),
    };
    row["md5"].as_str() == entry.md5.as_deref()
        && row["size"].as_u64() == entry.size
        && expected("version_id", entry.version_id.as_deref())
        && expected("etag", entry.etag.as_deref())
}

const UPLOAD_TOKEN: &str = "workspace-mgr-upload";

fn upload_journal(
    repo: &GitRepo,
    client: &crate::native_s3::S3Client,
    entry: &StorageEntry,
) -> Result<(PathBuf, Value)> {
    use sha2::Sha256;
    let key = client.key_for(&entry.object);
    let digest = entry
        .md5
        .as_deref()
        .ok_or_else(|| Error::message("storage upload has no digest"))?;
    let context = json!({"schema":1,"bucket":client.bucket,"key":key,"md5":digest,"hash_name":entry.hash_name,"size":entry.size});
    let identity = crate::hex::encode_lower(Sha256::digest(
        serde_json::to_vec(&context).map_err(|e| Error::message(e.to_string()))?,
    ));
    let relative = format!(".dvc/tmp/native-uploads/{identity}.json");
    reject_symlink_traversal(&repo.root, &relative, "private storage upload journal")?;
    let path = repo.root.join(relative);
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

fn save_upload(path: &Path, journal: &Value) -> Result<()> {
    atomic_write(
        path,
        &serde_json::to_vec_pretty(journal).map_err(|e| Error::message(e.to_string()))?,
    )
}

fn verify_uploaded_version(
    client: &crate::native_s3::S3Client,
    repo: &GitRepo,
    entry: &StorageEntry,
    token: &str,
    version: &str,
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
    let temporary = tempfile::NamedTempFile::new_in(cache_root(repo)?).at(cache_root(repo)?)?;
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
            verify_uploaded_version(client, repo, entry, token, &version)
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
    let (path, mut journal) = upload_journal(repo, client, entry)?;
    let key = client.key_for(&entry.object);
    let token = journal["token"]
        .as_str()
        .ok_or_else(|| Error::message("private upload journal has no ownership token"))?
        .to_owned();
    if let Some(version) = journal["version_id"].as_str() {
        return verify_uploaded_version(client, repo, entry, &token, version)
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
    let request = json!({"Bucket":client.bucket,"Key":key,"Metadata":{UPLOAD_TOKEN:token}});
    let response = if size <= single_limit {
        journal["phase"] = "uploading".into();
        save_upload(&path, &journal)?;
        let mut request = request;
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
            client.call_s3("complete_multipart_upload", &json!({"Bucket":client.bucket,"Key":key,"UploadId":upload,"MultipartUpload":{"Parts":parts}}),None).map_err(Error::from)
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
    let etag = verify_uploaded_version(client, repo, entry, &token, &version)?;
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
        let document = dvc::read_pointer_document(repo, pointer)?;
        let yaml = read_yaml(repo, pointer)?;
        for (index, out) in document.outs.iter().enumerate() {
            let algorithm = yaml["outs"][index]["hash"]
                .as_str()
                .unwrap_or("md5-dos2unix");
            let digest = out
                .md5
                .as_deref()
                .ok_or_else(|| Error::message("metadata has no content hash"))?;
            if digest.ends_with(".dir") {
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
        let yaml = read_yaml(repo, pointer)?;
        for (index, out) in dvc::read_pointer_document(repo, pointer)?
            .outs
            .into_iter()
            .enumerate()
        {
            let hash_name = yaml["outs"][index]["hash"]
                .as_str()
                .unwrap_or("md5-dos2unix");
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
                let source = existing_cache_with_algorithm(repo, &file.md5, hash_name)?;
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
                    format!(
                        "{}.dir",
                        crate::hex::encode_lower(Md5::digest(tree_bytes(
                            &current
                                .iter()
                                .map(|file| TreeEntry {
                                    md5: file.md5.clone(),
                                    relpath: file.relpath.clone()
                                })
                                .collect::<Vec<_>>()
                        )?))
                    )
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
                let source = existing_cache_with_algorithm(repo, &file.md5, hash_name)?;
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
        let yaml = read_yaml(repo, pointer)?;
        for (index, out) in dvc::read_pointer_document(repo, pointer)?
            .outs
            .into_iter()
            .enumerate()
        {
            let algorithm = yaml["outs"][index]["hash"]
                .as_str()
                .unwrap_or("md5-dos2unix");
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
                    format!(
                        "{}.dir",
                        crate::hex::encode_lower(Md5::digest(tree_bytes(
                            &current
                                .iter()
                                .map(|f| TreeEntry {
                                    relpath: f.relpath.clone(),
                                    md5: f.md5.clone()
                                })
                                .collect::<Vec<_>>()
                        )?))
                    )
                } else {
                    current.first().map(|f| f.md5.clone()).unwrap_or_default()
                };
                if actual != digest {
                    Some("modified")
                } else if !algorithms_for_pointer(repo, pointer).is_ok_and(|hashes| {
                    hashes.iter().all(|(hash, algorithm)| {
                        existing_cache_with_algorithm(repo, hash, algorithm).is_ok_and(|path| {
                            path.is_file()
                                && file_digest(&path, algorithm)
                                    .is_ok_and(|d| d == hash.trim_end_matches(".dir"))
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
        let yaml = read_yaml(repo, &pointer)?;
        for (index, out) in dvc::read_pointer_document(repo, &pointer)?
            .outs
            .into_iter()
            .enumerate()
        {
            let algorithm = yaml["outs"][index]["hash"]
                .as_str()
                .unwrap_or("md5-dos2unix");
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
                .map(|f| (f.relpath.clone(), f.md5.clone()))
                .collect::<BTreeMap<_, _>>();
            let new = current
                .iter()
                .map(|f| (f.relpath.clone(), f.md5.clone()))
                .collect::<BTreeMap<_, _>>();
            if old != new || !repo.root.join(&object).exists() {
                if !repo.root.join(&object).exists() {
                    deleted.insert(label.clone());
                } else if directory {
                    modified.insert(label.clone());
                }
            }
            if let Some(digest) = &out.md5 {
                if !existing_cache_with_algorithm(repo, digest, algorithm)?.is_file() {
                    not_in_cache.insert(label);
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
                    Some(hash) if hash != &file.md5 => {
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
    dvc::require_addressable(
        &destination,
        "storage move destination",
        "choose a path without backslashes",
    )?;
    for path in [
        &source,
        &destination,
        &format!("{source}.dvc"),
        &format!("{destination}.dvc"),
    ] {
        reject_symlink_traversal(&repo.root, path, "storage move")?;
    }
    let old_pointer = format!("{source}.dvc");
    let new_pointer = format!("{destination}.dvc");
    if repo.root.join(&destination).exists() || repo.root.join(&new_pointer).exists() {
        return Err(Error::message("storage move destination already exists"));
    }
    let mut document = read_yaml(repo, &old_pointer)?;
    let outs = document["outs"]
        .as_sequence_mut()
        .ok_or_else(|| Error::message("storage metadata has no outputs"))?;
    if outs.len() != 1 {
        return Err(Error::message(
            "storage move requires one output per pointer",
        ));
    }
    let map = outs[0]
        .as_mapping_mut()
        .ok_or_else(|| Error::message("invalid storage output"))?;
    if version_aware(repo) {
        clear_internal_cloud(map);
        if let Some(files) = map.get_mut(string("files")).and_then(Yaml::as_sequence_mut) {
            for file in files {
                clear_internal_cloud(
                    file.as_mapping_mut()
                        .ok_or_else(|| Error::message("invalid directory file"))?,
                );
            }
        }
    }
    put(
        map,
        "path",
        string(
            Path::new(&destination)
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| Error::message("invalid output name"))?,
        ),
    );
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
    if let Err(error) = write_yaml(repo, &new_pointer, &document) {
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
    let document = dvc::read_pointer_document(repo, pointer)?;
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
            repo.root.join(".dvc/config"),
            format!(
                "[core]\n    remote = workspace-mgr\n['remote \"workspace-mgr\"']\n    url = {}\n",
                path.display()
            ),
        )
        .unwrap();
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
        fs::write(repo.root.join(".dvc/config"), "version_aware = true\n").unwrap();
        fs::create_dir(repo.root.join("data")).unwrap();
        fs::write(repo.root.join("data/a"), "a").unwrap();
        fs::write(repo.root.join("data/z"), "z").unwrap();
        add(&repo, "data").unwrap();
        let mut document = read_yaml(&repo, "data.dvc").unwrap();
        assert_eq!(document["outs"][0]["hash"].as_str(), Some("md5"));
        cloud_binding(
            document["outs"][0]["files"][0].as_mapping_mut().unwrap(),
            "old-a",
            "etag-a",
        );
        cloud_binding(
            document["outs"][0]["files"][1].as_mapping_mut().unwrap(),
            "old-z",
            "etag-z",
        );
        write_yaml(&repo, "data.dvc", &document).unwrap();
        assert_eq!(data_status(&repo, &["data".into()]).unwrap(), json!({}));
        fs::write(repo.root.join("data/z"), "new z").unwrap();
        fs::write(repo.root.join("data/new"), "new").unwrap();
        let changes = data_status(&repo, &["data".into()]).unwrap();
        assert_eq!(
            changes["uncommitted"]["modified"],
            json!(["data/", "data/z"])
        );
        assert_eq!(changes["uncommitted"]["added"], json!(["data/new"]));
        commit(&repo, "data.dvc").unwrap();
        let entries = metadata_entries(&repo, None, &["data.dvc".into()]).unwrap();
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
        let pointers = vec!["data.dvc".into()];
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
        commit(&repo, "data.dvc").unwrap();
        fs::write(repo.root.join("data/a"), "old a").unwrap();
        fs::write(repo.root.join("data/retired"), "retired").unwrap();
        fs::remove_file(repo.root.join("data/new")).unwrap();
        let pointers = vec!["data.dvc".into()];
        checkout(&repo, &pointers).unwrap();
        assert_eq!(
            fs::read_to_string(repo.root.join("data/a")).unwrap(),
            "new a"
        );
        assert!(!repo.root.join("data/retired").exists());
        assert!(repo.root.join("data/new").exists());
        let out = dvc::read_pointer_document(&repo, "data.dvc")
            .unwrap()
            .outs
            .remove(0);
        let files = recorded_files(&repo, "data.dvc", &out, "md5").unwrap();
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
        assert!(error.to_string().contains("size"), "{error}");
        assert!(!repo.root.join("text").exists());
        assert_eq!(fs::read(canonical).unwrap(), b"a\nb\n");
        assert_eq!(fs::read_to_string(repo.root.join("text.dvc")).unwrap(), raw);
    }

    #[test]
    fn legacy_directory_namespaces_survive_commit_push_fetch_and_checkout() {
        let (_temporary, repo) = repository();
        let storage = tempfile::tempdir().unwrap();
        remote(&repo, storage.path());
        fs::create_dir(repo.root.join("data")).unwrap();
        fs::write(repo.root.join("data/a"), b"a\r\nb\r\n").unwrap();
        let file_digest = "dd8c6a395b5dd36c56d23275028f526c";
        let directory_digest = "178e38d9097fc874ace61e427874fc39.dir";
        let raw = format!(
            "outs:\n- path: data\n  md5: {directory_digest}\n  size: 6\n  nfiles: 1\n  files:\n  - relpath: a\n    md5: {file_digest}\n    size: 6\n"
        );
        fs::write(repo.root.join("data.dvc"), &raw).unwrap();
        // A new-format LF object has the same digest as the legacy CRLF
        // object. Their physical bytes must occupy separate namespaces.
        let normalized = repo.root.join("normalized");
        fs::write(&normalized, b"a\nb\n").unwrap();
        install_cache_file(&repo, file_digest, &normalized).unwrap();
        let canonical = cache_path(&repo, file_digest).unwrap();
        let legacy = cache_path_with_algorithm(&repo, file_digest, "md5-dos2unix").unwrap();
        let legacy_directory =
            cache_path_with_algorithm(&repo, directory_digest, "md5-dos2unix").unwrap();
        let pointers = vec!["data.dvc".into()];
        install_directory_manifests(&repo, &pointers).unwrap();
        assert!(legacy_directory.is_file());
        assert!(!cache_path(&repo, directory_digest).unwrap().exists());
        commit(&repo, "data.dvc").unwrap();
        assert!(read_yaml(&repo, "data.dvc").unwrap()["outs"][0]["hash"].is_null());
        assert_eq!(fs::read(&legacy).unwrap(), b"a\r\nb\r\n");
        assert_eq!(fs::read(&canonical).unwrap(), b"a\nb\n");
        assert_eq!(status(&repo, &pointers, false, false).unwrap().1, "{}");
        push(&repo, &pointers).unwrap();
        assert_eq!(
            fs::read(
                remote_cache_path(
                    storage.path().to_str().unwrap(),
                    file_digest,
                    &repo,
                    "md5-dos2unix"
                )
                .unwrap()
            )
            .unwrap(),
            b"a\r\nb\r\n"
        );
        assert_eq!(status(&repo, &pointers, true, false).unwrap().1, "{}");
        fs::remove_file(&legacy).unwrap();
        fs::remove_file(&legacy_directory).unwrap();
        fs::remove_dir_all(repo.root.join("data")).unwrap();
        let before = fs::read(repo.root.join("data.dvc")).unwrap();
        fetch(&repo, &pointers).unwrap();
        checkout(&repo, &pointers).unwrap();
        assert_eq!(fs::read(repo.root.join("data/a")).unwrap(), b"a\r\nb\r\n");
        assert_eq!(fs::read(&canonical).unwrap(), b"a\nb\n");
        assert_eq!(fs::read(&legacy).unwrap(), b"a\r\nb\r\n");
        assert!(legacy_directory.is_file());
        assert_eq!(fs::read(repo.root.join("data.dvc")).unwrap(), before);
    }

    #[test]
    fn moving_an_unhydrated_pointer_preserves_metadata_and_ignore_rules() {
        let (_temporary, repo) = repository();
        fs::write(repo.root.join("data"), "data").unwrap();
        add(&repo, "data").unwrap();
        fs::remove_file(repo.root.join("data")).unwrap();
        fs::write(repo.root.join(".gitignore"), "/data\n/keep-local\n").unwrap();
        move_output(&repo, "data", "nested/moved").unwrap();
        assert!(!repo.root.join("data.dvc").exists());
        assert!(!repo.root.join("nested/moved").exists());
        assert_eq!(
            read_yaml(&repo, "nested/moved.dvc").unwrap()["outs"][0]["path"],
            string("moved")
        );
        assert_eq!(
            fs::read_to_string(repo.root.join(".gitignore")).unwrap(),
            "/keep-local\n"
        );
        assert_eq!(
            fs::read_to_string(repo.root.join("nested/.gitignore")).unwrap(),
            "/moved\n"
        );
        remove(&repo, "nested/moved.dvc").unwrap();
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
        checkout(&repo, &["data.dvc".into()]).unwrap();
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
        assert!(!foreign.path().join("data.dvc").exists());
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
        assert!(!repo.root.join("data.dvc").exists());
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
        let yaml: Yaml = serde_yaml::from_str("files:\n- relpath: a\n  remote: foreign\n").unwrap();
        assert!(validate_transport_settings(&yaml, "data.dvc").is_err());
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
                verify_uploaded_version(&client, &repo, &entry, "owned-token", "owned-version")
                    .is_err()
            );
            worker.join().unwrap();
        }
    }

    #[test]
    fn restoring_legacy_cache_preserves_hash_algorithm_and_exact_binding() {
        let (_temporary, repo) = repository();
        fs::write(repo.root.join("text"), b"a\r\nb\r\n").unwrap();
        let digest = file_digest(&repo.root.join("text"), "md5-dos2unix").unwrap();
        let raw = format!(
            "outs:\n- path: text\n  hash: md5-dos2unix\n  md5: {digest}\n  size: 6\n  cloud:\n    workspace-mgr:\n      version_id: original-version\n      etag: original-etag\n"
        );
        fs::write(repo.root.join("text.dvc"), raw).unwrap();
        commit(&repo, "text.dvc").unwrap();
        let entries = metadata_entries(&repo, None, &["text.dvc".into()]).unwrap();
        assert_eq!(entries[0].hash_name, "md5-dos2unix");
        assert_eq!(entries[0].md5.as_deref(), Some(digest.as_str()));
        assert_eq!(entries[0].version_id.as_deref(), Some("original-version"));
        assert_eq!(
            fs::read(existing_cache(&repo, &digest).unwrap()).unwrap(),
            b"a\r\nb\r\n"
        );
        fs::write(repo.root.join("text"), b"a\nb\n").unwrap();
        commit(&repo, "text.dvc").unwrap();
        assert!(
            metadata_entries(&repo, None, &["text.dvc".into()]).unwrap()[0]
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
        let pointer = repo.root.join("source.dvc");
        let captured = std::sync::Arc::new(std::sync::Mutex::new((String::new(), Vec::new())));
        let expected = captured.clone();
        let (client, worker) = fixture_handler(4, move |request| {
            if request.target.contains("versioning") {
                return Reply::xml(
                    "<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>",
                );
            }
            if request.method == "PUT" {
                let mut raw: Yaml = serde_yaml::from_slice(&fs::read(&pointer).unwrap()).unwrap();
                cloud_binding(
                    raw["outs"][0].as_mapping_mut().unwrap(),
                    "independent-version",
                    "independent-etag",
                );
                let rendered = serde_yaml::to_string(&raw).unwrap().into_bytes();
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
        let error = push_versioned(&repo, &["source.dvc".into()]).unwrap_err();
        assert!(error.to_string().contains("metadata changed during upload"));
        worker.join().unwrap();
        assert_eq!(
            fs::read(repo.root.join("source.dvc")).unwrap(),
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
            &status(&repo, &["source.dvc".into()], false, false)
                .unwrap()
                .1,
        )
        .unwrap();
        assert_eq!(value["source.dvc"][0]["changed outs"]["source"], "modified");
    }
}
