use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::config::{Config, StorageDefault, StorageTarget};
use crate::dvc;
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::manifest::one_line;
use crate::path::{allowed, relative_to, repo_path, resolved_under};

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
    pub target: StorageTarget,
    pub selected_by: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
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
    pub would_place_in_s3: Vec<String>,
    pub placed_in_s3: Vec<String>,
}

pub fn status(
    repo: &GitRepo,
    config: &Config,
    scopes: &[String],
    paths: &[String],
) -> Result<StorageOperationReport> {
    let paths = resolve_status_paths(repo, scopes, paths)?;
    let placements = paths
        .iter()
        .map(|path| placement_status(repo, config, path))
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
    if target == StorageTarget::S3 && !config.storage.s3_enabled() {
        return Err(Error::message(
            "cannot place content in S3 because [storage.s3] is not configured",
        ));
    }
    if !dry_run {
        for path in &paths {
            apply_target(repo, config, path, target)?;
            write_placement(repo, path, target, &reason)?;
        }
    }
    let placements = paths
        .iter()
        .map(|path| {
            if dry_run {
                Ok(PlacementStatus {
                    path: path.clone(),
                    target,
                    selected_by: "explicit".to_owned(),
                    reason: Some(reason.clone()),
                })
            } else {
                placement_status(repo, config, path)
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
    let desired = paths
        .iter()
        .map(|path| Ok((path.clone(), automatic_target(repo, config, path)?)))
        .collect::<Result<Vec<_>>>()?;
    if !dry_run {
        for (path, target) in &desired {
            let sidecar = sidecar_path(repo, path);
            if sidecar.is_file() {
                fs::remove_file(&sidecar).at(&sidecar)?;
            }
            apply_target(repo, config, path, *target)?;
        }
    }
    let placements = desired
        .into_iter()
        .map(|(path, target)| PlacementStatus {
            path,
            target,
            selected_by: "automatic".to_owned(),
            reason: None,
        })
        .collect();
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
    let before = placement_status(repo, config, &old_path)?;
    if !dry_run {
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
    }
    Ok(StorageOperationReport {
        status: if dry_run { "dry_run" } else { "updated" }.to_owned(),
        operation: "move".to_owned(),
        paths: vec![old_path, new_path.clone()],
        placements: vec![PlacementStatus {
            path: new_path,
            target: before.target,
            selected_by: before.selected_by,
            reason: before.reason,
        }],
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
            let target = match config.storage.default {
                StorageDefault::Git => StorageTarget::Git,
                StorageDefault::S3 => StorageTarget::S3,
                StorageDefault::Auto => {
                    if size > config.storage.auto_s3_above_bytes {
                        StorageTarget::S3
                    } else {
                        StorageTarget::Git
                    }
                }
            };
            if target == StorageTarget::S3 {
                if !config.storage.s3_enabled() {
                    return Err(Error::message(format!(
                        "automatic policy selected S3 for {path:?}, but [storage.s3] is not configured; configure S3 or run `workspace-mgr storage set {path} --to git --reason <reason>`"
                    )));
                }
                candidates.push(path);
            }
        }
    }
    candidates.sort();
    candidates.dedup();
    if !dry_run && !candidates.is_empty() {
        dvc::management(repo, config, "track", &candidates, false)?;
    }
    Ok(AutomaticPlacementReport {
        mode: if dry_run { "plan" } else { "apply" }.to_owned(),
        would_place_in_s3: candidates.clone(),
        placed_in_s3: if dry_run { Vec::new() } else { candidates },
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
    let target = match config.storage.default {
        StorageDefault::Git => StorageTarget::Git,
        StorageDefault::S3 => StorageTarget::S3,
        StorageDefault::Auto => {
            let metadata = fs::metadata(resolved_under(&repo.root, path)).at(path)?;
            if metadata.is_file() && metadata.len() > config.storage.auto_s3_above_bytes {
                StorageTarget::S3
            } else {
                StorageTarget::Git
            }
        }
    };
    if target == StorageTarget::S3 && !config.storage.s3_enabled() {
        return Err(Error::message(format!(
            "automatic policy selected S3 for {path:?}, but [storage.s3] is not configured"
        )));
    }
    Ok(target)
}

fn placement_status(repo: &GitRepo, config: &Config, path: &str) -> Result<PlacementStatus> {
    if let Some(placement) = read_placement(repo, path)? {
        return Ok(PlacementStatus {
            path: path.to_owned(),
            target: placement.target,
            selected_by: "explicit".to_owned(),
            reason: Some(placement.reason),
        });
    }
    if pointer_path(repo, path).is_file() {
        return Ok(PlacementStatus {
            path: path.to_owned(),
            target: StorageTarget::S3,
            selected_by: "published-history".to_owned(),
            reason: None,
        });
    }
    if let Some(boundary) = inherited_boundary(repo, path)? {
        return Ok(PlacementStatus {
            path: path.to_owned(),
            target: boundary.target,
            selected_by: boundary.selected_by,
            reason: boundary.reason,
        });
    }
    let history = repo.run_unchecked(["log", "--all", "--format=%H", "-n", "1", "--", path])?;
    if history.success() && !history.stdout.trim().is_empty() {
        return Ok(PlacementStatus {
            path: path.to_owned(),
            target: StorageTarget::Git,
            selected_by: "published-history".to_owned(),
            reason: None,
        });
    }
    Ok(PlacementStatus {
        path: path.to_owned(),
        target: automatic_target(repo, config, path)?,
        selected_by: "automatic".to_owned(),
        reason: None,
    })
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
    selected_by: String,
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
                selected_by: "explicit-ancestor".to_owned(),
                reason: Some(placement.reason),
                explicit_target: Some(placement.target),
            }));
        }
        if pointer_path(repo, &candidate).is_file() {
            return Ok(Some(Boundary {
                path: candidate,
                target: StorageTarget::S3,
                selected_by: "published-ancestor".to_owned(),
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
