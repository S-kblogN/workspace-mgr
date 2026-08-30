use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::config::Config;
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::path::{allowed, reject_symlink_traversal, relative_to, repo_path, resolved_under};
use crate::process::{CommandOutput, run as run_process, run_unchecked as run_process_unchecked};

const VERSION_VERIFY_SCRIPT: &str = include_str!("../assets/dvc_version_verify.py");
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
    validate_internal_config_ownership(repo)?;
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

pub fn validate_internal_config_ownership(repo: &GitRepo) -> Result<()> {
    let path = internal_config_path(repo)?;
    if !path.exists() {
        return Ok(());
    }
    let raw = fs::read_to_string(&path).at(&path)?;
    if raw.trim().is_empty() || raw.starts_with(INTERNAL_CONFIG_HEADER) {
        return Ok(());
    }
    Err(Error::message(
        "existing .dvc/config is not managed by workspace-mgr and will not be overwritten; resolve it explicitly before rerunning init",
    ))
}

pub fn managed_internal_config_exists(repo: &GitRepo) -> Result<bool> {
    let path = internal_config_path(repo)?;
    if !path.is_file() {
        return Ok(false);
    }
    Ok(fs::read_to_string(&path)
        .at(&path)?
        .starts_with(INTERNAL_CONFIG_HEADER))
}

pub fn managed_internal_location(repo: &GitRepo) -> Result<Option<(String, Option<String>)>> {
    let path = internal_config_path(repo)?;
    if !path.is_file() {
        return Ok(None);
    }
    validate_internal_config_ownership(repo)?;
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
    if !managed_internal_config_exists(repo)? {
        return Ok(false);
    }
    let path = internal_config_path(repo)?;
    fs::remove_file(&path).at(&path)?;
    Ok(true)
}

pub fn repository_pointers(repo: &GitRepo) -> Result<Vec<String>> {
    let mut found = BTreeSet::new();
    for entry in WalkDir::new(&repo.root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !matches!(entry.file_name().to_str(), Some(".git" | ".dvc")))
    {
        let entry =
            entry.map_err(|error| Error::message(format!("failed to walk repository: {error}")))?;
        if entry.path().extension().and_then(|value| value.to_str()) == Some("dvc") {
            if !entry.file_type().is_file() || entry.path().is_symlink() {
                return Err(Error::message(format!(
                    "managed-storage metadata must be a regular file: {}",
                    entry.path().display()
                )));
            }
            found.insert(relative_to(
                entry.path(),
                &repo.root,
                "managed-storage metadata",
            )?);
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

pub fn discover(repo: &GitRepo, scopes: &[String]) -> Result<Vec<String>> {
    let mut found = BTreeSet::new();
    for scope in scopes {
        let root = resolved_under(&repo.root, scope);
        if root.is_symlink() {
            if root.extension().and_then(|value| value.to_str()) == Some("dvc") {
                return Err(Error::message(format!(
                    "managed-storage metadata may not be a symlink: {scope}"
                )));
            }
            continue;
        }
        if root.is_file() {
            if root.extension().and_then(|value| value.to_str()) == Some("dvc") {
                found.insert(scope.clone());
            }
            continue;
        }
        if !root.exists() || root.is_symlink() {
            continue;
        }
        for entry in WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| entry.file_name() != ".git")
        {
            let entry = entry.map_err(|error| {
                Error::message(format!("failed to walk {}: {error}", root.display()))
            })?;
            if entry.path().extension().and_then(|value| value.to_str()) != Some("dvc") {
                continue;
            }
            if !entry.file_type().is_file() || entry.path().is_symlink() {
                return Err(Error::message(format!(
                    "managed-storage metadata must be a regular file: {}",
                    entry.path().display()
                )));
            }
            found.insert(relative_to(
                entry.path(),
                &repo.root,
                "managed-storage metadata",
            )?);
        }
    }
    Ok(found.into_iter().collect())
}

pub fn output_paths(repo: &GitRepo, pointers: &[String]) -> Result<BTreeMap<String, Vec<String>>> {
    let mut result = BTreeMap::new();
    for pointer in pointers {
        reject_symlink_traversal(&repo.root, pointer, "managed-storage metadata")?;
        let pointer_path = resolved_under(&repo.root, pointer);
        let raw = fs::read_to_string(&pointer_path).at(&pointer_path)?;
        let parsed: Pointer = serde_yaml::from_str(&raw).map_err(|source| Error::Yaml {
            path: pointer_path.clone(),
            source,
        })?;
        if parsed.outs.len() != 1 {
            return Err(Error::message(format!(
                "managed-storage metadata must define exactly one output: {pointer}"
            )));
        }
        let parent = Path::new(pointer).parent().unwrap_or_else(|| Path::new(""));
        let mut outputs = BTreeSet::new();
        for output in parsed.outs {
            let raw = if parent.as_os_str().is_empty() {
                output.path
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
            outputs.insert(output);
        }
        result.insert(pointer.clone(), outputs.into_iter().collect());
    }
    Ok(result)
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
    if !pointers.is_empty() {
        let mut args = vec!["push".to_owned(), "--".to_owned()];
        args.extend(pointers.iter().cloned());
        execute_engine(&repo.root, args)?;
        report.verification = Some(verify(repo, config, pointers)?);
    }
    report.committed = dirty;
    report.pushed = pointers.to_vec();
    Ok(report)
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
    let parsed: Pointer = serde_yaml::from_str(&raw).map_err(|source| Error::Yaml {
        path: pointer_path,
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
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn management(
    repo: &GitRepo,
    config: &Config,
    operation: &str,
    paths: &[String],
    dry_run: bool,
) -> Result<serde_json::Value> {
    ensure_ready(repo, config)?;
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
    let pointer = resolved_under(&repo.root, &format!("{output}.dvc"));
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
        return Ok(());
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
    Ok(())
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
}
