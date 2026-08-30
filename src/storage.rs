use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::config::{Config, StorageTarget};
use crate::dvc;
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::manifest::{ResolvedTask, one_line};
use crate::path::{allowed, reject_symlink_traversal, relative_to, repo_path, resolved_under};
use crate::policy::{AUTO_S3_ABOVE_BYTES, RECOMMENDED_S3_MINIMUM_BYTES, TASK_MANIFEST_NAME};

pub const PLACEMENT_SUFFIX: &str = ".workspace-mgr-storage.toml";
const PLACEMENT_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlacementFile {
    schema_version: u32,
    target: StorageTarget,
    reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlacementStatus {
    pub path: String,
    pub boundary: String,
    pub target: StorageTarget,
    pub basis: PlacementBasis,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<PlacementWarning>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlacementWarning {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlacementBasis {
    Explicit,
    ExplicitAncestor,
    PublishedHistory,
    PublishedAncestor,
    AutomaticSizeFallback,
}

#[derive(Debug, Clone, Serialize)]
pub struct StorageOperationReport {
    pub status: String,
    pub operation: String,
    pub paths: Vec<String>,
    pub placements: Vec<PlacementStatus>,
    pub remote_writes: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AutomaticPlacementReport {
    pub mode: String,
    pub recommended_s3_minimum_bytes: u64,
    pub automatic_s3_above_bytes: u64,
    pub decisions: Vec<PlacementStatus>,
    pub placed_in_s3: Vec<String>,
    #[serde(skip)]
    automatic_s3: Vec<String>,
}

impl AutomaticPlacementReport {
    pub fn automatic_s3(&self) -> &[String] {
        &self.automatic_s3
    }
}

pub fn status(
    repo: &GitRepo,
    config: &Config,
    scopes: &[String],
    paths: &[String],
) -> Result<StorageOperationReport> {
    let paths = resolve_status_paths(repo, scopes, paths)?;
    let history_oid = task_history_oid(repo, config, scopes)?;
    let placements = paths
        .iter()
        .map(|path| placement_status(repo, config, path, history_oid.as_deref()))
        .collect::<Result<Vec<_>>>()?;
    Ok(StorageOperationReport {
        status: "ok".to_owned(),
        operation: "status".to_owned(),
        paths,
        placements,
        remote_writes: false,
    })
}

pub fn set(
    repo: &GitRepo,
    config: &Config,
    scopes: &[String],
    paths: &[String],
    target: StorageTarget,
    reason: &str,
    dry_run: bool,
) -> Result<StorageOperationReport> {
    let paths = validate_targets(repo, scopes, paths, true)?;
    validate_boundary_targets(repo, scopes, &paths)?;
    let reason = one_line(reason, "storage placement reason")?;
    if target == StorageTarget::S3 && !config.s3_enabled() {
        return Err(Error::message(
            "cannot place content in S3 because [s3] is not configured",
        ));
    }
    if target == StorageTarget::S3 {
        for path in &paths {
            reject_symlink_traversal(&repo.root, path, "S3 storage path")?;
        }
    }
    if !dry_run {
        let snapshot = MetadataSnapshot::capture(repo, &paths)?;
        let result = (|| {
            for path in &paths {
                apply_target(repo, config, path, target)?;
                write_placement(repo, path, target, &reason)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            return Err(rollback_error(error, snapshot.restore()));
        }
    }
    let placements = paths
        .iter()
        .map(|path| {
            if dry_run {
                placement_report(
                    repo,
                    path,
                    path,
                    target,
                    PlacementBasis::Explicit,
                    Some(reason.clone()),
                )
            } else {
                placement_status(repo, config, path, None)
            }
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(StorageOperationReport {
        status: if dry_run { "dry_run" } else { "updated" }.to_owned(),
        operation: "set".to_owned(),
        paths,
        placements,
        remote_writes: false,
    })
}

pub fn reset(
    repo: &GitRepo,
    config: &Config,
    scopes: &[String],
    paths: &[String],
    dry_run: bool,
) -> Result<StorageOperationReport> {
    let paths = validate_targets(repo, scopes, paths, true)?;
    validate_boundary_targets(repo, scopes, &paths)?;
    let history_oid = task_history_oid(repo, config, scopes)?;
    let desired = paths
        .iter()
        .map(|path| {
            let (target, basis) =
                automatic_target_after_reset(repo, config, path, history_oid.as_deref())?;
            Ok((path.clone(), target, basis))
        })
        .collect::<Result<Vec<_>>>()?;
    if !dry_run {
        let snapshot = MetadataSnapshot::capture(repo, &paths)?;
        let result = (|| {
            for (path, target, _) in &desired {
                let sidecar = sidecar_path(repo, path);
                if sidecar.is_file() {
                    fs::remove_file(&sidecar).at(&sidecar)?;
                }
                apply_target(repo, config, path, *target)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            return Err(rollback_error(error, snapshot.restore()));
        }
    }
    let placements = desired
        .into_iter()
        .filter_map(|(path, target, basis)| {
            let is_unbounded_directory = resolved_under(&repo.root, &path).is_dir()
                && target == StorageTarget::Git
                && basis == PlacementBasis::AutomaticSizeFallback;
            (!is_unbounded_directory)
                .then(|| placement_report(repo, &path, &path, target, basis, None))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(StorageOperationReport {
        status: if dry_run { "dry_run" } else { "updated" }.to_owned(),
        operation: "reset".to_owned(),
        paths,
        placements,
        remote_writes: false,
    })
}

pub fn move_path(
    repo: &GitRepo,
    config: &Config,
    scopes: &[String],
    old_path: &str,
    new_path: &str,
    dry_run: bool,
) -> Result<StorageOperationReport> {
    let old_path = repo_path(old_path, "move source")?;
    let new_path = repo_path(new_path, "move destination")?;
    for path in [&old_path, &new_path] {
        if !allowed(path, scopes) {
            return Err(Error::message(format!(
                "move path escapes the declared scope: {path}"
            )));
        }
        reject_symlink_traversal(&repo.root, path, "move path")?;
    }
    let old = resolved_under(&repo.root, &old_path);
    let new = resolved_under(&repo.root, &new_path);
    if !old.exists() {
        return Err(Error::message(format!(
            "move source does not exist: {old_path}"
        )));
    }
    if new.exists()
        || pointer_path(repo, &new_path).exists()
        || sidecar_path(repo, &new_path).exists()
    {
        return Err(Error::message(format!(
            "move destination already exists: {new_path}"
        )));
    }
    let old_container = inherited_boundary(repo, &old_path)?;
    let new_container = inherited_boundary(repo, &new_path)?;
    if old_container.as_ref().map(|boundary| &boundary.path)
        != new_container.as_ref().map(|boundary| &boundary.path)
    {
        return Err(Error::message(
            "move may not cross an existing directory placement boundary; move the boundary itself or reset it first",
        ));
    }
    let history_oid = task_history_oid(repo, config, scopes)?;
    let before = placement_status(repo, config, &old_path, history_oid.as_deref())?;
    if !dry_run {
        let snapshot = MetadataSnapshot::capture(repo, &[old_path.clone(), new_path.clone()])?;
        let result = (|| {
            if pointer_path(repo, &old_path).is_file() {
                dvc::management(
                    repo,
                    config,
                    "move",
                    &[old_path.clone(), new_path.clone()],
                    false,
                )?;
            } else {
                if let Some(parent) = new.parent() {
                    fs::create_dir_all(parent).at(parent)?;
                }
                fs::rename(&old, &new).at(&old)?;
            }
            let old_sidecar = sidecar_path(repo, &old_path);
            if old_sidecar.is_file() {
                let new_sidecar = sidecar_path(repo, &new_path);
                fs::rename(&old_sidecar, &new_sidecar).at(&old_sidecar)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            return Err(rollback_error(error, rollback_move(&old, &new, snapshot)));
        }
    }
    let mut placement = before;
    placement.path = new_path.clone();
    if placement.boundary == old_path {
        placement.boundary = new_path.clone();
    }
    Ok(StorageOperationReport {
        status: if dry_run { "dry_run" } else { "updated" }.to_owned(),
        operation: "move".to_owned(),
        paths: vec![old_path, new_path.clone()],
        placements: vec![placement],
        remote_writes: false,
    })
}

pub fn hydrate(
    repo: &GitRepo,
    config: &Config,
    scopes: &[String],
    paths: &[String],
    dry_run: bool,
) -> Result<dvc::HydrateReport> {
    let pointers = if paths.is_empty() {
        Vec::new()
    } else {
        let paths = validate_targets(repo, scopes, paths, false)?;
        let discovered = dvc::discover(repo, scopes)?;
        let outputs = dvc::output_paths(repo, &discovered)?;
        let mut selected = BTreeSet::new();
        for path in paths {
            for (pointer, values) in &outputs {
                if values
                    .iter()
                    .any(|output| path == *output || is_descendant(&path, output))
                {
                    selected.insert(pointer.clone());
                }
            }
            if !outputs.values().any(|values| {
                values
                    .iter()
                    .any(|output| path == *output || is_descendant(&path, output))
            }) {
                return Err(Error::message(format!("path is not stored in S3: {path}")));
            }
        }
        selected.into_iter().collect()
    };
    dvc::hydrate(repo, config, scopes, &pointers, dry_run)
}

pub fn apply_automatic(
    repo: &GitRepo,
    config: &Config,
    scopes: &[String],
    base_oid: &str,
    dry_run: bool,
) -> Result<AutomaticPlacementReport> {
    let mut candidates = Vec::new();
    let mut decisions = Vec::new();
    for scope in scopes {
        let root = resolved_under(&repo.root, scope);
        if !root.exists() || root.is_symlink() {
            continue;
        }
        let walker = if root.is_file() {
            WalkDir::new(&root).max_depth(0)
        } else {
            WalkDir::new(&root)
        };
        for entry in walker
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| entry.file_name() != ".git")
        {
            let entry = entry.map_err(|error| Error::message(format!("walk failed: {error}")))?;
            if !entry.file_type().is_file() || entry.path().is_symlink() {
                continue;
            }
            let path = relative_to(entry.path(), &repo.root, "storage candidate")?;
            if path.ends_with(".dvc") || path.ends_with(PLACEMENT_SUFFIX) {
                continue;
            }
            let ignored = repo.run_unchecked(["check-ignore", "--quiet", "--", &path])?;
            if ignored.code == 0 {
                continue;
            }
            if ignored.code != 1 {
                return Err(Error::message(format!(
                    "git check-ignore failed for {path}"
                )));
            }
            if explicit_target(repo, &path)? == Some(StorageTarget::Git) {
                continue;
            }
            let object = format!("{base_oid}:{path}");
            if repo.run_unchecked(["cat-file", "-e", &object])?.success() {
                continue;
            }
            let size = entry
                .metadata()
                .map_err(|error| Error::message(error.to_string()))?
                .len();
            let target = size_fallback_target(size);
            if size >= RECOMMENDED_S3_MINIMUM_BYTES {
                decisions.push(placement_report(
                    repo,
                    &path,
                    &path,
                    target,
                    PlacementBasis::AutomaticSizeFallback,
                    None,
                )?);
            }
            if target == StorageTarget::S3 {
                if !config.s3_enabled() {
                    return Err(Error::message(format!(
                        "automatic policy selected S3 for {path:?}, but [s3] is not configured; configure S3 or run `workspace-mgr storage set {path} --to git --reason <reason>`"
                    )));
                }
                candidates.push(path);
            }
        }
    }
    candidates.sort();
    candidates.dedup();
    if !dry_run && !candidates.is_empty() {
        let snapshot = MetadataSnapshot::capture(repo, &candidates)?;
        if let Err(error) = dvc::management(repo, config, "track", &candidates, false) {
            return Err(rollback_error(error, snapshot.restore()));
        }
    }
    for decision in warning_relevant_boundaries(repo, config, scopes, base_oid)? {
        if !decisions
            .iter()
            .any(|current| current.path == decision.path && current.basis == decision.basis)
        {
            decisions.push(decision);
        }
    }
    decisions.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.basis.cmp(&right.basis))
    });
    Ok(AutomaticPlacementReport {
        mode: if dry_run { "plan" } else { "apply" }.to_owned(),
        recommended_s3_minimum_bytes: RECOMMENDED_S3_MINIMUM_BYTES,
        automatic_s3_above_bytes: AUTO_S3_ABOVE_BYTES,
        decisions,
        placed_in_s3: if dry_run {
            Vec::new()
        } else {
            candidates.clone()
        },
        automatic_s3: candidates,
    })
}

pub fn explicit_target(repo: &GitRepo, path: &str) -> Result<Option<StorageTarget>> {
    if let Some(placement) = read_placement(repo, path)? {
        return Ok(Some(placement.target));
    }
    Ok(inherited_boundary(repo, path)?.and_then(|boundary| boundary.explicit_target))
}

fn apply_target(repo: &GitRepo, config: &Config, path: &str, target: StorageTarget) -> Result<()> {
    let pointer = pointer_path(repo, path);
    match target {
        StorageTarget::Git if pointer.is_file() => {
            dvc::management(
                repo,
                config,
                "untrack",
                &[relative_to(&pointer, &repo.root, "storage metadata")?],
                false,
            )?;
        }
        StorageTarget::S3 if !pointer.is_file() => {
            dvc::management(repo, config, "track", &[path.to_owned()], false)?;
        }
        _ => {}
    }
    Ok(())
}

fn automatic_target(repo: &GitRepo, config: &Config, path: &str) -> Result<StorageTarget> {
    let metadata = fs::metadata(resolved_under(&repo.root, path)).at(path)?;
    let target = if metadata.is_file() {
        size_fallback_target(metadata.len())
    } else {
        StorageTarget::Git
    };
    if target == StorageTarget::S3 && !config.s3_enabled() {
        return Err(Error::message(format!(
            "automatic policy selected S3 for {path:?}, but [s3] is not configured"
        )));
    }
    Ok(target)
}

fn size_fallback_target(bytes: u64) -> StorageTarget {
    if bytes > AUTO_S3_ABOVE_BYTES {
        StorageTarget::S3
    } else {
        StorageTarget::Git
    }
}

fn automatic_target_after_reset(
    repo: &GitRepo,
    config: &Config,
    path: &str,
    history_oid: Option<&str>,
) -> Result<(StorageTarget, PlacementBasis)> {
    if let Some(oid) = history_oid {
        if repo
            .run_unchecked(["cat-file", "-e", &format!("{oid}:{path}.dvc")])?
            .success()
        {
            return Ok((StorageTarget::S3, PlacementBasis::PublishedHistory));
        }
        if repo
            .run_unchecked(["cat-file", "-e", &format!("{oid}:{path}")])?
            .success()
        {
            return Ok((StorageTarget::Git, PlacementBasis::PublishedHistory));
        }
    }
    Ok((
        automatic_target(repo, config, path)?,
        PlacementBasis::AutomaticSizeFallback,
    ))
}

fn placement_status(
    repo: &GitRepo,
    config: &Config,
    path: &str,
    history_oid: Option<&str>,
) -> Result<PlacementStatus> {
    if let Some(placement) = read_placement(repo, path)? {
        return placement_report(
            repo,
            path,
            path,
            placement.target,
            PlacementBasis::Explicit,
            Some(placement.reason),
        );
    }
    if let Some(oid) = history_oid {
        if repo
            .run_unchecked(["cat-file", "-e", &format!("{oid}:{path}.dvc")])?
            .success()
        {
            return placement_report(
                repo,
                path,
                path,
                StorageTarget::S3,
                PlacementBasis::PublishedHistory,
                None,
            );
        }
    }
    if pointer_path(repo, path).is_file() {
        return placement_report(
            repo,
            path,
            path,
            StorageTarget::S3,
            PlacementBasis::AutomaticSizeFallback,
            None,
        );
    }
    if let Some(boundary) = inherited_boundary(repo, path)? {
        return placement_report(
            repo,
            path,
            &boundary.path,
            boundary.target,
            boundary.basis,
            boundary.reason,
        );
    }
    let published_in_git = if let Some(oid) = history_oid {
        repo.run_unchecked(["cat-file", "-e", &format!("{oid}:{path}")])?
            .success()
    } else {
        false
    };
    if published_in_git {
        let published_as_directory = history_oid
            .map(|oid| published_object_is_directory(repo, oid, path))
            .transpose()?
            .unwrap_or(false);
        if resolved_under(&repo.root, path).is_dir() || published_as_directory {
            return Err(unbounded_directory_status_error(path));
        }
        return placement_report(
            repo,
            path,
            path,
            StorageTarget::Git,
            PlacementBasis::PublishedHistory,
            None,
        );
    }
    if resolved_under(&repo.root, path).is_dir() {
        return Err(unbounded_directory_status_error(path));
    }
    placement_report(
        repo,
        path,
        path,
        automatic_target(repo, config, path)?,
        PlacementBasis::AutomaticSizeFallback,
        None,
    )
}

fn published_object_is_directory(repo: &GitRepo, oid: &str, path: &str) -> Result<bool> {
    let object = format!("{oid}:{path}");
    Ok(repo.run(["cat-file", "-t", &object])?.stdout.trim() == "tree")
}

fn unbounded_directory_status_error(path: &str) -> Error {
    Error::message(format!(
        "directory {path:?} is not a single storage boundary; run `workspace-mgr storage status` to inspect its files, or select the directory with `workspace-mgr storage set {path} --to git|s3 --reason <reason>`"
    ))
}

#[derive(Debug, Clone, Copy)]
struct PayloadMetrics {
    bytes: u64,
    files: u64,
}

fn placement_report(
    repo: &GitRepo,
    path: &str,
    boundary: &str,
    target: StorageTarget,
    basis: PlacementBasis,
    reason: Option<String>,
) -> Result<PlacementStatus> {
    let metrics = payload_metrics(repo, boundary)?;
    let warnings = placement_warnings(target, basis, metrics);
    Ok(PlacementStatus {
        path: path.to_owned(),
        boundary: boundary.to_owned(),
        target,
        basis,
        payload_bytes: metrics.map(|value| value.bytes),
        payload_files: metrics.map(|value| value.files),
        reason,
        warnings,
    })
}

fn payload_metrics(repo: &GitRepo, boundary: &str) -> Result<Option<PayloadMetrics>> {
    let root = resolved_under(&repo.root, boundary);
    let metadata = match fs::symlink_metadata(&root) {
        Ok(metadata) => metadata,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(source) => return Err(Error::Io { path: root, source }),
    };
    if metadata.file_type().is_symlink() {
        return Err(Error::message(format!(
            "storage boundary may not be a symlink: {boundary}"
        )));
    }
    if metadata.is_file() {
        return Ok(Some(PayloadMetrics {
            bytes: metadata.len(),
            files: 1,
        }));
    }
    if !metadata.is_dir() {
        return Err(Error::message(format!(
            "storage boundary is not a regular file or directory: {boundary}"
        )));
    }
    let mut metrics = PayloadMetrics { bytes: 0, files: 0 };
    for entry in WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != ".git")
    {
        let entry = entry.map_err(|error| Error::message(format!("walk failed: {error}")))?;
        if !entry.file_type().is_file() || entry.path().is_symlink() {
            continue;
        }
        let size = entry
            .metadata()
            .map_err(|error| Error::message(error.to_string()))?
            .len();
        metrics.bytes = metrics
            .bytes
            .checked_add(size)
            .ok_or_else(|| Error::message(format!("storage boundary is too large: {boundary}")))?;
        metrics.files = metrics.files.checked_add(1).ok_or_else(|| {
            Error::message(format!("storage boundary has too many files: {boundary}"))
        })?;
    }
    Ok(Some(metrics))
}

fn placement_warnings(
    target: StorageTarget,
    basis: PlacementBasis,
    metrics: Option<PayloadMetrics>,
) -> Vec<PlacementWarning> {
    let Some(metrics) = metrics else {
        return Vec::new();
    };
    let mut warnings = Vec::new();
    if target == StorageTarget::S3 && metrics.bytes < RECOMMENDED_S3_MINIMUM_BYTES {
        warnings.push(PlacementWarning {
            code: "small-s3-boundary".to_owned(),
            message: "S3 boundary is smaller than the recommended 1 MiB minimum; Git or a larger semantic boundary is usually more efficient".to_owned(),
        });
    }
    if basis == PlacementBasis::AutomaticSizeFallback
        && target == StorageTarget::Git
        && metrics.bytes >= RECOMMENDED_S3_MINIMUM_BYTES
        && metrics.bytes <= AUTO_S3_ABOVE_BYTES
    {
        warnings.push(PlacementWarning {
            code: "semantic-placement-review".to_owned(),
            message: "new boundary is in the 1-10 MiB review band; Git is the size fallback, but choose Git or S3 explicitly when collaboration or artifact semantics are clear".to_owned(),
        });
    }
    warnings
}

fn warning_relevant_boundaries(
    repo: &GitRepo,
    config: &Config,
    scopes: &[String],
    base_oid: &str,
) -> Result<Vec<PlacementStatus>> {
    known_boundaries(repo, scopes)?
        .into_iter()
        .map(|path| placement_status(repo, config, &path, Some(base_oid)))
        .filter_map(|status| match status {
            Ok(status) if !status.warnings.is_empty() => Some(Ok(status)),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

fn task_history_oid(repo: &GitRepo, config: &Config, scopes: &[String]) -> Result<Option<String>> {
    let task_manifest = scopes.iter().find(|scope| {
        resolved_under(&repo.root, &format!("{scope}/{TASK_MANIFEST_NAME}")).is_file()
    });
    let manifest = match task_manifest {
        Some(task_path) => resolved_under(&repo.root, &format!("{task_path}/{TASK_MANIFEST_NAME}")),
        None => match ResolvedTask::discover(repo, &repo.root) {
            Ok(path) => path,
            Err(_) => return Ok(None),
        },
    };
    let task = ResolvedTask::load(repo, config, &manifest)?;
    for reference in [
        format!("refs/remotes/{}/{}", task.remote, task.branch),
        format!("refs/remotes/{}/{}", task.remote, task.base_branch),
        format!("refs/heads/{}", task.base_branch),
    ] {
        if let Some(oid) = repo.optional_oid(&reference)? {
            return Ok(Some(oid));
        }
    }
    Ok(None)
}

fn validate_targets(
    repo: &GitRepo,
    scopes: &[String],
    paths: &[String],
    require_output: bool,
) -> Result<Vec<String>> {
    let mut result = paths
        .iter()
        .map(|path| repo_path(path, "storage path"))
        .collect::<Result<Vec<_>>>()?;
    result.sort();
    result.dedup();
    for path in &result {
        if !allowed(path, scopes) {
            return Err(Error::message(format!(
                "storage path escapes the declared scope: {path}"
            )));
        }
        if require_output && !resolved_under(&repo.root, path).exists() {
            return Err(Error::message(format!(
                "storage path does not exist: {path}"
            )));
        }
        reject_symlink_traversal(&repo.root, path, "storage path")?;
    }
    Ok(result)
}

fn resolve_status_paths(
    repo: &GitRepo,
    scopes: &[String],
    paths: &[String],
) -> Result<Vec<String>> {
    if !paths.is_empty() {
        return validate_targets(repo, scopes, paths, false);
    }
    let mut found = BTreeSet::new();
    let mut boundaries = BTreeSet::new();
    for pointer in dvc::discover(repo, scopes)? {
        for output in dvc::output_paths(repo, std::slice::from_ref(&pointer))?
            .remove(&pointer)
            .unwrap_or_default()
        {
            boundaries.insert(output);
        }
    }
    for scope in scopes {
        let root = resolved_under(&repo.root, scope);
        if !root.exists() || root.is_symlink() {
            continue;
        }
        for entry in WalkDir::new(&root).follow_links(false) {
            let entry = entry.map_err(|error| Error::message(format!("walk failed: {error}")))?;
            if entry.file_type().is_file()
                && entry.path().to_string_lossy().ends_with(PLACEMENT_SUFFIX)
            {
                let sidecar = relative_to(entry.path(), &repo.root, "placement metadata")?;
                boundaries.insert(sidecar.trim_end_matches(PLACEMENT_SUFFIX).to_owned());
            }
        }
    }
    found.extend(boundaries.iter().cloned());
    for scope in scopes {
        let root = resolved_under(&repo.root, scope);
        if !root.exists() || root.is_symlink() {
            continue;
        }
        let walker = if root.is_file() {
            WalkDir::new(&root).max_depth(0)
        } else {
            WalkDir::new(&root)
        };
        for entry in walker
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| entry.file_name() != ".git")
        {
            let entry = entry.map_err(|error| Error::message(format!("walk failed: {error}")))?;
            if !entry.file_type().is_file() || entry.path().is_symlink() {
                continue;
            }
            let path = relative_to(entry.path(), &repo.root, "storage status path")?;
            if path.ends_with(".dvc") || path.ends_with(PLACEMENT_SUFFIX) {
                continue;
            }
            if boundaries
                .iter()
                .any(|boundary| path == *boundary || is_descendant(&path, boundary))
            {
                continue;
            }
            let ignored = repo.run_unchecked(["check-ignore", "--quiet", "--", &path])?;
            match ignored.code {
                0 => continue,
                1 => {
                    found.insert(path);
                }
                _ => {
                    return Err(Error::message(
                        "git check-ignore failed while listing storage status",
                    ));
                }
            }
        }
    }
    Ok(found.into_iter().collect())
}

#[derive(Debug)]
struct Boundary {
    path: String,
    target: StorageTarget,
    basis: PlacementBasis,
    reason: Option<String>,
    explicit_target: Option<StorageTarget>,
}

fn inherited_boundary(repo: &GitRepo, path: &str) -> Result<Option<Boundary>> {
    let mut parent = Path::new(path)
        .parent()
        .filter(|candidate| !candidate.as_os_str().is_empty())
        .map(crate::path::to_slash);
    while let Some(candidate) = parent {
        if let Some(placement) = read_placement(repo, &candidate)? {
            return Ok(Some(Boundary {
                path: candidate,
                target: placement.target,
                basis: PlacementBasis::ExplicitAncestor,
                reason: Some(placement.reason),
                explicit_target: Some(placement.target),
            }));
        }
        if pointer_path(repo, &candidate).is_file() {
            return Ok(Some(Boundary {
                path: candidate,
                target: StorageTarget::S3,
                basis: PlacementBasis::PublishedAncestor,
                reason: None,
                explicit_target: None,
            }));
        }
        parent = Path::new(&candidate)
            .parent()
            .filter(|candidate| !candidate.as_os_str().is_empty())
            .map(crate::path::to_slash);
    }
    Ok(None)
}

fn validate_boundary_targets(repo: &GitRepo, scopes: &[String], paths: &[String]) -> Result<()> {
    for (index, path) in paths.iter().enumerate() {
        if paths
            .iter()
            .skip(index + 1)
            .any(|other| is_descendant(path, other) || is_descendant(other, path))
        {
            return Err(Error::message(
                "one storage operation may not target nested placement boundaries",
            ));
        }
        if let Some(boundary) = inherited_boundary(repo, path)? {
            return Err(Error::message(format!(
                "storage path {path:?} is inside the existing placement boundary {:?}; set or reset that boundary instead",
                boundary.path
            )));
        }
    }
    let known = known_boundaries(repo, scopes)?;
    for path in paths {
        if let Some(descendant) = known
            .iter()
            .find(|candidate| *candidate != path && is_descendant(candidate, path))
        {
            return Err(Error::message(format!(
                "storage path {path:?} contains the existing placement boundary {descendant:?}; reset the nested boundary first"
            )));
        }
    }
    Ok(())
}

fn known_boundaries(repo: &GitRepo, scopes: &[String]) -> Result<BTreeSet<String>> {
    let mut boundaries = BTreeSet::new();
    for pointer in dvc::discover(repo, scopes)? {
        boundaries.extend(
            dvc::output_paths(repo, std::slice::from_ref(&pointer))?
                .remove(&pointer)
                .unwrap_or_default(),
        );
    }
    for scope in scopes {
        let root = resolved_under(&repo.root, scope);
        if !root.exists() || root.is_symlink() {
            continue;
        }
        for entry in WalkDir::new(&root).follow_links(false) {
            let entry = entry.map_err(|error| Error::message(format!("walk failed: {error}")))?;
            if entry.file_type().is_file()
                && entry.path().to_string_lossy().ends_with(PLACEMENT_SUFFIX)
            {
                let sidecar = relative_to(entry.path(), &repo.root, "placement metadata")?;
                boundaries.insert(sidecar.trim_end_matches(PLACEMENT_SUFFIX).to_owned());
            }
        }
    }
    Ok(boundaries)
}

fn is_descendant(path: &str, ancestor: &str) -> bool {
    path.strip_prefix(ancestor)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

fn pointer_path(repo: &GitRepo, path: &str) -> std::path::PathBuf {
    resolved_under(&repo.root, &format!("{path}.dvc"))
}

fn sidecar_path(repo: &GitRepo, path: &str) -> std::path::PathBuf {
    resolved_under(&repo.root, &format!("{path}{PLACEMENT_SUFFIX}"))
}

fn read_placement(repo: &GitRepo, path: &str) -> Result<Option<PlacementFile>> {
    let sidecar = sidecar_path(repo, path);
    reject_symlink_traversal(
        &repo.root,
        &format!("{path}{PLACEMENT_SUFFIX}"),
        "storage placement metadata",
    )?;
    if !sidecar.is_file() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&sidecar).at(&sidecar)?;
    let placement: PlacementFile = toml::from_str(&raw).map_err(|source| Error::Toml {
        path: sidecar.clone(),
        source,
    })?;
    if placement.schema_version != PLACEMENT_SCHEMA {
        return Err(Error::message(format!(
            "unsupported placement metadata schema in {}",
            sidecar.display()
        )));
    }
    Ok(Some(placement))
}

fn write_placement(repo: &GitRepo, path: &str, target: StorageTarget, reason: &str) -> Result<()> {
    let sidecar = sidecar_path(repo, path);
    let parent = sidecar
        .parent()
        .ok_or_else(|| Error::message("placement metadata path has no parent"))?;
    fs::create_dir_all(parent).at(parent)?;
    let rendered = toml::to_string_pretty(&PlacementFile {
        schema_version: PLACEMENT_SCHEMA,
        target,
        reason: reason.to_owned(),
    })
    .map_err(|error| Error::message(format!("failed to render placement metadata: {error}")))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).at(parent)?;
    temporary.write_all(rendered.as_bytes()).at(&sidecar)?;
    temporary.flush().at(&sidecar)?;
    temporary.persist(&sidecar).map_err(|error| Error::Io {
        path: sidecar,
        source: error.error,
    })?;
    Ok(())
}

#[derive(Debug)]
struct MetadataSnapshot {
    files: Vec<FileSnapshot>,
}

#[derive(Debug)]
struct FileSnapshot {
    path: PathBuf,
    contents: Option<Vec<u8>>,
}

impl MetadataSnapshot {
    fn capture(repo: &GitRepo, paths: &[String]) -> Result<Self> {
        let mut candidates = BTreeSet::new();
        for path in paths {
            candidates.insert(pointer_path(repo, path));
            candidates.insert(sidecar_path(repo, path));
            let output = resolved_under(&repo.root, path);
            let parent = output
                .parent()
                .ok_or_else(|| Error::message("storage output has no parent"))?;
            candidates.insert(parent.join(".gitignore"));
        }
        let mut files = Vec::new();
        for path in candidates {
            let contents = match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                    Some(fs::read(&path).at(&path)?)
                }
                Ok(_) => {
                    return Err(Error::message(format!(
                        "storage metadata path is not a regular file: {}",
                        path.display()
                    )));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(source) => return Err(Error::Io { path, source }),
            };
            files.push(FileSnapshot { path, contents });
        }
        Ok(Self { files })
    }

    fn restore(self) -> Result<()> {
        for file in self.files {
            match file.contents {
                Some(contents) => atomic_write_bytes(&file.path, &contents)?,
                None => match fs::symlink_metadata(&file.path) {
                    Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
                        fs::remove_file(&file.path).at(&file.path)?;
                    }
                    Ok(_) => {
                        return Err(Error::message(format!(
                            "cannot roll back non-file storage metadata path: {}",
                            file.path.display()
                        )));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(source) => {
                        return Err(Error::Io {
                            path: file.path,
                            source,
                        });
                    }
                },
            }
        }
        Ok(())
    }
}

fn rollback_move(old: &Path, new: &Path, snapshot: MetadataSnapshot) -> Result<()> {
    let output_result = match (old.exists(), new.exists()) {
        (false, true) => {
            if let Some(parent) = old.parent() {
                fs::create_dir_all(parent).at(parent)?;
            }
            fs::rename(new, old).at(new)
        }
        (true, false) => Ok(()),
        (false, false) => Err(Error::message(
            "move rollback could not find either source or destination output",
        )),
        (true, true) => Err(Error::message(
            "move rollback found both source and destination outputs",
        )),
    };
    let metadata_result = snapshot.restore();
    match (output_result, metadata_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(output), Err(metadata)) => Err(Error::message(format!(
            "output rollback failed: {output}; metadata rollback failed: {metadata}"
        ))),
    }
}

fn rollback_error(error: Error, rollback: Result<()>) -> Error {
    match rollback {
        Ok(()) => Error::message(format!(
            "storage operation failed and was rolled back: {error}"
        )),
        Err(rollback) => Error::message(format!(
            "storage operation failed: {error}; rollback also failed: {rollback}"
        )),
    }
}

fn atomic_write_bytes(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::message("storage metadata path has no parent"))?;
    fs::create_dir_all(parent).at(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).at(parent)?;
    temporary.write_all(contents).at(path)?;
    temporary.flush().at(path)?;
    temporary.persist(path).map_err(|error| Error::Io {
        path: path.to_path_buf(),
        source: error.error,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(bytes: u64) -> Option<PayloadMetrics> {
        Some(PayloadMetrics { bytes, files: 1 })
    }

    #[test]
    fn size_fallback_and_warning_boundaries_are_exact() {
        assert_eq!(
            size_fallback_target(RECOMMENDED_S3_MINIMUM_BYTES - 1),
            StorageTarget::Git
        );
        assert_eq!(
            size_fallback_target(RECOMMENDED_S3_MINIMUM_BYTES),
            StorageTarget::Git
        );
        assert_eq!(
            size_fallback_target(AUTO_S3_ABOVE_BYTES),
            StorageTarget::Git
        );
        assert_eq!(
            size_fallback_target(AUTO_S3_ABOVE_BYTES + 1),
            StorageTarget::S3
        );

        let small_s3 = placement_warnings(
            StorageTarget::S3,
            PlacementBasis::Explicit,
            metrics(RECOMMENDED_S3_MINIMUM_BYTES - 1),
        );
        assert_eq!(small_s3[0].code, "small-s3-boundary");
        assert!(
            placement_warnings(
                StorageTarget::S3,
                PlacementBasis::Explicit,
                metrics(RECOMMENDED_S3_MINIMUM_BYTES),
            )
            .is_empty()
        );

        for bytes in [RECOMMENDED_S3_MINIMUM_BYTES, AUTO_S3_ABOVE_BYTES] {
            let review = placement_warnings(
                StorageTarget::Git,
                PlacementBasis::AutomaticSizeFallback,
                metrics(bytes),
            );
            assert_eq!(review[0].code, "semantic-placement-review");
        }
        assert!(
            placement_warnings(
                StorageTarget::Git,
                PlacementBasis::AutomaticSizeFallback,
                metrics(RECOMMENDED_S3_MINIMUM_BYTES - 1),
            )
            .is_empty()
        );
        assert!(
            placement_warnings(
                StorageTarget::S3,
                PlacementBasis::AutomaticSizeFallback,
                metrics(AUTO_S3_ABOVE_BYTES + 1),
            )
            .is_empty()
        );
    }
}
