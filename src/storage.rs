use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::config::{Config, StorageTarget};
use crate::dvc;
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::manifest::{ResolvedTask, one_line, published_history_path, published_task_paths};
use crate::path::{allowed, reject_symlink_traversal, relative_to, repo_path, resolved_under};
use crate::policy::{AUTO_S3_ABOVE_BYTES, RECOMMENDED_S3_MINIMUM_BYTES, TASK_MANIFEST_NAME};

pub const PLACEMENT_SUFFIX: &str = ".workspace-mgr-storage.toml";
/// The markers around the ignore rule `untrack` writes for a local-only path.
/// Scaffold regeneration of the root ignore file re-emits these blocks
/// verbatim, so the two writers must agree on their exact shape.
pub(crate) const LOCAL_IGNORE_BEGIN: &str = "# workspace-mgr local begin ";
pub(crate) const LOCAL_IGNORE_END: &str = "# workspace-mgr local end ";
const PLACEMENT_SCHEMA: u32 = 1;
const LOCAL_REASON: &str = "Keep payload only in this checkout";

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
    let history = task_history(repo, config, scopes)?;
    let placements = paths
        .iter()
        .map(|path| placement_status(repo, config, path, history.as_ref()))
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
    if target == StorageTarget::Local {
        return Err(Error::message(
            "use `workspace-mgr untrack <path>` to keep content local only",
        ));
    }
    let paths = validate_targets(repo, scopes, paths, true)?;
    validate_boundary_targets(repo, scopes, &paths)?;
    let local = paths
        .iter()
        .filter_map(|path| match is_local(repo, path) {
            Ok(true) => Some(Ok(path.clone())),
            Ok(false) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<Result<Vec<_>>>()?;
    validate_local_metadata_scope(repo, scopes, &local)?;
    validate_retrack_ignores(repo, &local)?;
    let reason = one_line(reason, "storage placement reason")?;
    if target == StorageTarget::S3 && !config.s3_enabled() {
        return Err(Error::message(
            "cannot place content in S3 because [s3] is not configured",
        ));
    }
    if target == StorageTarget::S3 {
        for path in &paths {
            reject_symlink_traversal(&repo.root, path, "S3 storage path")?;
            dvc::require_addressable(path, "S3 storage path", "rename it before placing it in S3")?;
        }
    }
    if !dry_run {
        let snapshot = MetadataSnapshot::capture(repo, &paths)?;
        let result = (|| {
            for path in &paths {
                if local.contains(path) {
                    update_local_ignore(repo, path, false)?;
                    // A restored old pointer must not bypass creation of fresh
                    // storage metadata and DVC ignore rules when re-tracking.
                    apply_target(repo, config, path, StorageTarget::Git)?;
                }
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

/// Keep payload bytes in this checkout while retiring Git and S3 publication.
pub fn untrack(
    repo: &GitRepo,
    config: &Config,
    scopes: &[String],
    paths: &[String],
    dry_run: bool,
) -> Result<StorageOperationReport> {
    let paths = validate_targets(repo, scopes, paths, false)?;
    if paths.is_empty() {
        return Err(Error::message(
            "untrack requires at least one file or complete storage boundary",
        ));
    }
    validate_boundary_targets(repo, scopes, &paths)?;
    validate_local_metadata_scope(repo, scopes, &paths)?;
    let mut already_local = BTreeSet::new();
    for path in &paths {
        reject_control_path(repo, path)?;
        let absolute = resolved_under(&repo.root, path);
        let existing = read_placement(repo, path)?;
        if existing
            .as_ref()
            .is_some_and(|value| value.target == StorageTarget::Local)
        {
            already_local.insert(path.clone());
            continue;
        }
        if !absolute.exists() {
            return Err(Error::message(format!(
                "cannot untrack {path:?} without a local payload; run `workspace-mgr storage hydrate {path}` first if it is stored in S3"
            )));
        }
        if absolute.is_dir() && existing.is_none() && !pointer_path(repo, path).is_file() {
            return Err(Error::message(format!(
                "directory {path:?} is not a complete storage boundary; select an existing boundary or first run `workspace-mgr storage set {path} --to git --reason <reason>`"
            )));
        }
        for file in payload_paths(repo, path)? {
            // Ignore files inside a complete boundary are part of its payload;
            // the boundary's owned rule lives outside it, in its parent.
            if Path::new(&file)
                .file_name()
                .is_some_and(|name| name == ".gitignore")
                && file != *path
            {
                continue;
            }
            reject_control_path(repo, &file)?;
        }
    }
    // Capture and validate metadata even in dry-run, so previews catch unsafe paths.
    let snapshot = MetadataSnapshot::capture(repo, &paths)?;
    let mut replacements = BTreeMap::new();
    for path in &paths {
        let ignore = local_ignore_path(path)?;
        let original = replacements
            .entry(ignore.clone())
            .or_insert(read_ignore(repo, &ignore)?);
        *original = with_local_ignore(original, path, true)?;
    }
    let metadata_paths = paths
        .iter()
        .map(|path| format!("{path}{PLACEMENT_SUFFIX}"))
        .chain(replacements.keys().cloned())
        .collect::<Vec<_>>();
    validate_unignored(repo, &metadata_paths, &replacements, "local-only metadata")?;
    if !dry_run {
        let result = (|| {
            for path in &paths {
                let retained_ignore = if is_local(repo, path)? && pointer_path(repo, path).is_file()
                {
                    let ignore = local_ignore_path(path)?;
                    Some((ignore.clone(), read_ignore(repo, &ignore)?))
                } else {
                    None
                };
                apply_target(repo, config, path, StorageTarget::Local)?;
                if let Some((ignore, contents)) = retained_ignore {
                    // DVC remove may strip its former output pattern from our
                    // managed block. Once local, these rules belong to us.
                    atomic_write_bytes(&resolved_under(&repo.root, &ignore), &contents)?;
                }
                update_local_ignore(repo, path, true)?;
                write_placement(repo, path, StorageTarget::Local, LOCAL_REASON)?;
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
            if already_local.contains(path) {
                return Ok(PlacementStatus {
                    path: path.clone(),
                    boundary: path.clone(),
                    target: StorageTarget::Local,
                    basis: PlacementBasis::Explicit,
                    payload_bytes: None,
                    payload_files: None,
                    reason: Some(LOCAL_REASON.to_owned()),
                    warnings: Vec::new(),
                });
            }
            placement_report(
                repo,
                path,
                path,
                StorageTarget::Local,
                PlacementBasis::Explicit,
                Some(LOCAL_REASON.to_owned()),
            )
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(StorageOperationReport {
        status: if dry_run { "dry_run" } else { "updated" }.to_owned(),
        operation: "untrack".to_owned(),
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
    for path in &paths {
        if is_local(repo, path)? {
            return Err(Error::message(format!(
                "cannot reset local-only content {path:?} automatically; run `workspace-mgr storage set {path} --to git|s3 --reason <reason>` to resume tracking explicitly"
            )));
        }
    }
    let history = task_history(repo, config, scopes)?;
    let desired = paths
        .iter()
        .map(|path| {
            let (target, basis) =
                automatic_target_after_reset(repo, config, path, history.as_ref())?;
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
    // A storage boundary exists once its metadata does, whether or not its
    // payload is currently materialized. A fresh checkout holds the metadata of
    // every S3 boundary but the payload of none, and a boundary the engine
    // cannot address can never be hydrated, so renaming it is the only way out.
    let old_materialized = old.exists();
    if !old_materialized && !pointer_path(repo, &old_path).is_file() {
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
    if is_local(repo, &old_path)?
        || is_local(repo, &new_path)?
        || local_boundaries(repo, scopes)?
            .iter()
            .any(|path| is_descendant(path, &old_path))
    {
        return Err(Error::message(
            "move of or within local-only content is not supported; explicitly restore tracking with `workspace-mgr storage set <boundary> --to git|s3 --reason <reason>` first",
        ));
    }
    if old_container.as_ref().map(|boundary| &boundary.path)
        != new_container.as_ref().map(|boundary| &boundary.path)
    {
        return Err(Error::message(
            "move may not cross an existing directory placement boundary; move the boundary itself or reset it first",
        ));
    }
    let history = task_history(repo, config, scopes)?;
    let before = placement_status(repo, config, &old_path, history.as_ref())?;
    if !dry_run {
        if !old_materialized {
            // The rename drops the version the remote stored the payload under,
            // because that version belongs to the old object path, and a
            // version-aware remote locates an object only by path and version.
            // So fetch the payload while the old metadata still locates it,
            // before anything changes; a failed fetch leaves nothing to undo.
            fetch_unmaterialized_source(repo, config, &old_path)?;
        }
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
            if !old_materialized {
                // Materialized at the destination, the payload is uploaded
                // under its new path by the next publication, exactly as the
                // payload of a boundary that was hydrated before its move.
                materialize_moved_destination(repo, &new_path)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            return Err(rollback_error(
                error,
                rollback_move(&old, &new, old_materialized, snapshot),
            ));
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

pub fn remove_paths(
    repo: &GitRepo,
    config: &Config,
    scopes: &[String],
    paths: &[String],
    dry_run: bool,
) -> Result<StorageOperationReport> {
    let paths = validate_targets(repo, scopes, paths, true)?;
    let local = local_boundaries(repo, scopes)?;
    for path in &paths {
        if local
            .iter()
            .any(|boundary| is_descendant(boundary, path) || is_descendant(path, boundary))
        {
            return Err(Error::message(format!(
                "remove {path:?} intersects a local-only boundary; remove the complete local-only boundary instead"
            )));
        }
    }
    for (index, path) in paths.iter().enumerate() {
        if scopes.iter().any(|scope| scope == path) {
            return Err(Error::message(format!(
                "remove may not delete an entire task scope {path:?}; use `workspace-mgr task discard` for an unmerged task"
            )));
        }
        if paths
            .iter()
            .skip(index + 1)
            .any(|other| is_descendant(path, other) || is_descendant(other, path))
        {
            return Err(Error::message(
                "one remove operation may not target nested paths",
            ));
        }
    }
    let history = task_history(repo, config, scopes)?;
    let placements = paths
        .iter()
        .map(|path| placement_status(repo, config, path, history.as_ref()))
        .collect::<Result<Vec<_>>>()?;
    if !dry_run {
        for path in &paths {
            if local.contains(path) {
                validate_local_metadata_scope(repo, scopes, std::slice::from_ref(path))?;
                update_local_ignore(repo, path, false)?;
            }
            let pointer = pointer_path(repo, path);
            if pointer.is_file() {
                dvc::management(
                    repo,
                    config,
                    "untrack",
                    &[relative_to(&pointer, &repo.root, "storage metadata")?],
                    false,
                )?;
            }
            let sidecar = sidecar_path(repo, path);
            if sidecar.is_file() {
                fs::remove_file(&sidecar).at(&sidecar)?;
            }
            let target = resolved_under(&repo.root, path);
            let metadata = fs::symlink_metadata(&target).at(&target)?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                fs::remove_dir_all(&target).at(&target)?;
            } else {
                fs::remove_file(&target).at(&target)?;
            }
        }
    }
    Ok(StorageOperationReport {
        status: if dry_run { "dry_run" } else { "updated" }.to_owned(),
        operation: "remove".to_owned(),
        paths,
        placements,
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
    let history = history_for_oid(repo, config, scopes, base_oid)?;
    let mut candidates = Vec::new();
    let mut decisions = Vec::new();
    for path in repo.visible_paths(scopes)? {
        if is_local(repo, &path)? {
            continue;
        }
        let absolute = resolved_under(&repo.root, &path);
        let metadata = fs::symlink_metadata(&absolute).at(&absolute)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            continue;
        }
        if path.ends_with(".dvc") || path.ends_with(PLACEMENT_SUFFIX) {
            continue;
        }
        if matches!(
            explicit_target(repo, &path)?,
            Some(StorageTarget::Git | StorageTarget::Local)
        ) {
            continue;
        }
        let object = format!("{base_oid}:{}", history.object_path(&path));
        if repo.run_unchecked(["cat-file", "-e", &object])?.success() {
            continue;
        }
        let size = metadata.len();
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
            dvc::require_addressable(
                &path,
                "automatic S3 placement path",
                &format!(
                    "rename it or run `workspace-mgr storage set {path} --to git --reason <reason>`"
                ),
            )?;
            candidates.push(path);
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
    for decision in warning_relevant_boundaries(repo, config, scopes, &history)? {
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

pub fn is_local(repo: &GitRepo, path: &str) -> Result<bool> {
    Ok(explicit_target(repo, path)? == Some(StorageTarget::Local))
}

pub fn local_boundaries(repo: &GitRepo, scopes: &[String]) -> Result<BTreeSet<String>> {
    let mut result = BTreeSet::new();
    for path in repo.visible_paths(scopes)? {
        if let Some(boundary) = path.strip_suffix(PLACEMENT_SUFFIX) {
            if read_placement(repo, boundary)?
                .is_some_and(|value| value.target == StorageTarget::Local)
            {
                result.insert(boundary.to_owned());
            }
        }
    }
    Ok(result)
}

pub fn local_boundaries_at(repo: &GitRepo, oid: &str) -> Result<BTreeSet<String>> {
    let mut result = BTreeSet::new();
    for entry in repo
        .run(["ls-tree", "-r", "-z", oid])?
        .stdout
        .split('\0')
        .filter(|value| !value.is_empty())
    {
        let Some((header, path)) = entry.split_once('\t') else {
            return Err(Error::message(
                "invalid Git tree while reading local-only boundaries",
            ));
        };
        let Some(boundary) = path.strip_suffix(PLACEMENT_SUFFIX) else {
            continue;
        };
        let fields = header.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 3 || !matches!(fields[0], "100644" | "100755") || fields[1] != "blob" {
            return Err(Error::message(format!(
                "storage placement metadata is not a regular file: {path}"
            )));
        }
        let object = fields[2];
        let raw = repo.run(["cat-file", "blob", object])?.stdout;
        let placement: PlacementFile = toml::from_str(&raw).map_err(|source| Error::Toml {
            path: repo.root.join(path),
            source,
        })?;
        if placement.schema_version != PLACEMENT_SCHEMA {
            return Err(Error::message(format!(
                "unsupported placement metadata schema in {path}"
            )));
        }
        if placement.target == StorageTarget::Local {
            result.insert(repo_path(boundary, "local-only boundary")?);
        }
    }
    Ok(result)
}

fn apply_target(repo: &GitRepo, config: &Config, path: &str, target: StorageTarget) -> Result<()> {
    let pointer = pointer_path(repo, path);
    match target {
        StorageTarget::Git | StorageTarget::Local if pointer.is_file() => {
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
    history: Option<&HistoryContext>,
) -> Result<(StorageTarget, PlacementBasis)> {
    if let Some(history) = history {
        let history_path = history.object_path(path);
        if repo
            .run_unchecked([
                "cat-file",
                "-e",
                &format!("{}:{history_path}.dvc", history.oid),
            ])?
            .success()
        {
            return Ok((StorageTarget::S3, PlacementBasis::PublishedHistory));
        }
        if repo
            .run_unchecked(["cat-file", "-e", &format!("{}:{history_path}", history.oid)])?
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
    history: Option<&HistoryContext>,
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
    if let Some(history) = history {
        let history_path = history.object_path(path);
        if repo
            .run_unchecked([
                "cat-file",
                "-e",
                &format!("{}:{history_path}.dvc", history.oid),
            ])?
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
    let published_in_git = if let Some(history) = history {
        repo.run_unchecked([
            "cat-file",
            "-e",
            &format!("{}:{}", history.oid, history.object_path(path)),
        ])?
        .success()
    } else {
        false
    };
    if published_in_git {
        let published_as_directory = history
            .map(|history| {
                published_object_is_directory(repo, &history.oid, &history.object_path(path))
            })
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
    history: &HistoryContext,
) -> Result<Vec<PlacementStatus>> {
    known_boundaries(repo, scopes)?
        .into_iter()
        .filter_map(|path| match is_local(repo, &path) {
            Ok(true) => None,
            Ok(false) => Some(Ok(path)),
            Err(error) => Some(Err(error)),
        })
        .map(|path| path.and_then(|path| placement_status(repo, config, &path, Some(history))))
        .filter_map(|status| match status {
            Ok(status) if !status.warnings.is_empty() => Some(Ok(status)),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

#[derive(Debug, Clone)]
struct HistoryContext {
    oid: String,
    current_task_path: Option<String>,
    published_task_path: Option<String>,
}

impl HistoryContext {
    fn object_path(&self, current: &str) -> String {
        published_history_path(
            current,
            self.current_task_path.as_deref(),
            self.published_task_path.as_deref(),
        )
    }
}

fn task_history(
    repo: &GitRepo,
    config: &Config,
    scopes: &[String],
) -> Result<Option<HistoryContext>> {
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
            return history_context(repo, &task, &oid).map(Some);
        }
    }
    Ok(None)
}

fn history_for_oid(
    repo: &GitRepo,
    config: &Config,
    scopes: &[String],
    oid: &str,
) -> Result<HistoryContext> {
    let task_manifest = scopes.iter().find(|scope| {
        resolved_under(&repo.root, &format!("{scope}/{TASK_MANIFEST_NAME}")).is_file()
    });
    let task = match task_manifest {
        Some(task_path) => ResolvedTask::load(
            repo,
            config,
            &resolved_under(&repo.root, &format!("{task_path}/{TASK_MANIFEST_NAME}")),
        )?,
        None => match ResolvedTask::discover(repo, &repo.root) {
            Ok(path) => ResolvedTask::load(repo, config, &path)?,
            Err(_) => {
                return Ok(HistoryContext {
                    oid: oid.to_owned(),
                    current_task_path: None,
                    published_task_path: None,
                });
            }
        },
    };
    history_context(repo, &task, oid)
}

fn history_context(repo: &GitRepo, task: &ResolvedTask, oid: &str) -> Result<HistoryContext> {
    let paths = published_task_paths(repo, oid, task)?;
    if paths.len() > 1 {
        return Err(Error::message(format!(
            "published history contains multiple paths for task {:?}",
            task.task_id
        )));
    }
    Ok(HistoryContext {
        oid: oid.to_owned(),
        current_task_path: task.task_path.clone(),
        published_task_path: paths.into_iter().next(),
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
    let visible_paths = repo.visible_paths(scopes)?;
    for path in &visible_paths {
        let absolute = resolved_under(&repo.root, path);
        if path.ends_with(PLACEMENT_SUFFIX)
            && fs::symlink_metadata(&absolute)
                .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
        {
            boundaries.insert(path.trim_end_matches(PLACEMENT_SUFFIX).to_owned());
        }
    }
    found.extend(boundaries.iter().cloned());
    for path in visible_paths {
        let absolute = resolved_under(&repo.root, &path);
        let metadata = fs::symlink_metadata(&absolute).at(&absolute)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            continue;
        }
        if path.ends_with(".dvc") || path.ends_with(PLACEMENT_SUFFIX) {
            continue;
        }
        if boundaries
            .iter()
            .any(|boundary| path == *boundary || is_descendant(&path, boundary))
        {
            continue;
        }
        found.insert(path);
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
    for path in repo.visible_paths(scopes)? {
        let absolute = resolved_under(&repo.root, &path);
        if path.ends_with(PLACEMENT_SUFFIX)
            && fs::symlink_metadata(&absolute)
                .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
        {
            boundaries.insert(path.trim_end_matches(PLACEMENT_SUFFIX).to_owned());
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

fn reject_control_path(repo: &GitRepo, path: &str) -> Result<()> {
    let name = Path::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let task_readme = name == "README.md"
        && Path::new(path)
            .parent()
            .is_some_and(|parent| repo.root.join(parent).join(TASK_MANIFEST_NAME).is_file());
    if name == ".gitignore"
        || task_readme
        || name == TASK_MANIFEST_NAME
        || name == crate::config::CONFIG_NAME
        || name.ends_with(PLACEMENT_SUFFIX)
        || name.ends_with(".dvc")
        || path.split('/').any(|part| part == ".git" || part == ".dvc")
    {
        return Err(Error::message(format!(
            "untrack may not hide task or storage control metadata: {path}"
        )));
    }
    Ok(())
}

fn local_ignore_path(path: &str) -> Result<String> {
    let parent = Path::new(path)
        .parent()
        .ok_or_else(|| Error::message("local-only path has no parent"))?;
    Ok(crate::path::to_slash(&parent.join(".gitignore")))
}

fn validate_local_metadata_scope(
    repo: &GitRepo,
    scopes: &[String],
    paths: &[String],
) -> Result<()> {
    for path in paths {
        if scopes.iter().any(|scope| scope == path) && resolved_under(&repo.root, path).is_dir() {
            return Err(Error::message(format!(
                "untrack may not hide an entire declared scope: {path}"
            )));
        }
        for metadata in [
            format!("{path}{PLACEMENT_SUFFIX}"),
            format!("{path}.dvc"),
            local_ignore_path(path)?,
        ] {
            if !allowed(&metadata, scopes) {
                return Err(Error::message(format!(
                    "local-only metadata {metadata:?} escapes the declared scope; include its parent directory in the task scope first"
                )));
            }
            reject_symlink_traversal(&repo.root, &metadata, "local-only metadata")?;
        }
    }
    Ok(())
}

fn read_ignore(repo: &GitRepo, path: &str) -> Result<Vec<u8>> {
    reject_symlink_traversal(&repo.root, path, "Git ignore file")?;
    let absolute = resolved_under(&repo.root, path);
    match fs::read(&absolute) {
        Ok(contents) => Ok(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(source) => Err(Error::Io {
            path: absolute,
            source,
        }),
    }
}

fn with_local_ignore(contents: &[u8], path: &str, add: bool) -> Result<Vec<u8>> {
    let name = Path::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| Error::message("local-only path must have a UTF-8 file name"))?;
    let key = crate::hex::encode_lower(name.as_bytes());
    let begin = format!("{LOCAL_IGNORE_BEGIN}{key}");
    let end = format!("{LOCAL_IGNORE_END}{key}");
    let mut pattern = String::from("/");
    for character in name.chars() {
        if matches!(character, '\\' | '*' | '?' | '[' | ']' | '!' | '#' | ' ') {
            pattern.push('\\');
        }
        pattern.push(character);
    }
    let block = format!("\n{begin}\n{pattern}\n{end}\n");
    let mut result = contents.to_vec();
    if let Some(start) = result
        .windows(block.len())
        .position(|part| part == block.as_bytes())
    {
        if add {
            return Ok(result);
        }
        result.drain(start..start + block.len());
        return Ok(result);
    }
    if result
        .windows(begin.len())
        .any(|part| part == begin.as_bytes())
        || result.windows(end.len()).any(|part| part == end.as_bytes())
    {
        return Err(Error::message(format!(
            "managed local-only ignore rule for {path:?} was edited; restore its original block before changing tracking"
        )));
    }
    if add {
        result.extend_from_slice(block.as_bytes());
    }
    Ok(result)
}

fn update_local_ignore(repo: &GitRepo, path: &str, add: bool) -> Result<()> {
    let ignore = local_ignore_path(path)?;
    let original = read_ignore(repo, &ignore)?;
    let updated = with_local_ignore(&original, path, add)?;
    if updated != original {
        atomic_write_bytes(&resolved_under(&repo.root, &ignore), &updated)?;
    }
    Ok(())
}

fn payload_paths(repo: &GitRepo, path: &str) -> Result<Vec<String>> {
    let root = resolved_under(&repo.root, path);
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    for entry in WalkDir::new(&root).follow_links(false) {
        let entry = entry.map_err(|error| {
            Error::message(format!("cannot inspect local-only payload: {error}"))
        })?;
        if entry.file_type().is_symlink()
            || !(entry.file_type().is_file() || entry.file_type().is_dir())
        {
            return Err(Error::message(format!(
                "local-only payload must contain regular files and directories: {}",
                entry.path().display()
            )));
        }
        if entry.file_type().is_file() {
            result.push(relative_to(entry.path(), &repo.root, "local-only payload")?);
        }
    }
    Ok(result)
}

fn validate_retrack_ignores(repo: &GitRepo, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut replacements = BTreeMap::new();
    let mut probes = Vec::new();
    for path in paths {
        let ignore = local_ignore_path(path)?;
        let contents = replacements
            .entry(ignore.clone())
            .or_insert(read_ignore(repo, &ignore)?);
        *contents = with_local_ignore(contents, path, false)?;
        probes.push(path.clone());
        probes.extend(payload_paths(repo, path)?);
    }
    validate_unignored(
        repo,
        &probes,
        &replacements,
        "content being restored to tracking",
    )
}

/// Ask Git to evaluate the resulting rules without modifying the real checkout,
/// including global excludes and .git/info/exclude from this repository.
fn validate_unignored(
    repo: &GitRepo,
    probes: &[String],
    replacements: &BTreeMap<String, Vec<u8>>,
    description: &str,
) -> Result<()> {
    let shadow = tempfile::tempdir().map_err(|source| Error::Io {
        path: std::env::temp_dir(),
        source,
    })?;
    let mut ignores = BTreeSet::new();
    for path in probes {
        let mut directory = Path::new(path).parent();
        while let Some(parent) = directory {
            ignores.insert(crate::path::to_slash(&parent.join(".gitignore")));
            directory = parent.parent();
        }
        let absolute = resolved_under(shadow.path(), path);
        if resolved_under(&repo.root, path).is_dir() {
            fs::create_dir_all(&absolute).at(&absolute)?;
        } else {
            if let Some(parent) = absolute.parent() {
                fs::create_dir_all(parent).at(parent)?;
            }
            fs::write(&absolute, []).at(&absolute)?;
        }
    }
    for ignore in ignores {
        let contents = match replacements.get(&ignore) {
            Some(contents) => contents.clone(),
            None => read_ignore(repo, &ignore)?,
        };
        if !contents.is_empty() {
            atomic_write_bytes(&resolved_under(shadow.path(), &ignore), &contents)?;
        }
    }
    let mut input = probes.join("\0");
    input.push('\0');
    let git_dir = repo.git_dir()?;
    let git_dir = git_dir.canonicalize().at(&git_dir)?;
    let mut args = vec![
        "--git-dir".to_owned(),
        git_dir.to_string_lossy().into_owned(),
        "--work-tree".to_owned(),
        shadow.path().to_string_lossy().into_owned(),
    ];
    let excludes = repo.run_unchecked(["config", "--path", "--get", "core.excludesFile"])?;
    if excludes.success() && !excludes.stdout.trim().is_empty() {
        let configured = Path::new(excludes.stdout.trim());
        let absolute = if configured.is_absolute() {
            configured.to_path_buf()
        } else {
            repo.root.join(configured)
        };
        args.extend([
            "-c".to_owned(),
            format!("core.excludesFile={}", absolute.display()),
        ]);
    }
    args.extend(["check-ignore", "--no-index", "-z", "--stdin"].map(str::to_owned));
    let output = crate::process::run_with(
        "git",
        args,
        shadow.path(),
        &BTreeMap::new(),
        Some(&input),
        false,
    )?;
    match output.code {
        0 => Err(Error::message(format!(
            "{description} is still excluded by user Git ignore rules: {}; adjust those rules explicitly before retrying",
            output.stdout.trim_end_matches('\0').replace('\0', ", ")
        ))),
        1 => Ok(()),
        _ => Err(Error::message(format!(
            "cannot validate Git ignore rules: {}",
            output.stderr.trim()
        ))),
    }
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

/// Brings the payload of a boundary whose metadata is present but whose
/// payload is not into the local cache, through that metadata.
fn fetch_unmaterialized_source(repo: &GitRepo, config: &Config, boundary: &str) -> Result<()> {
    dvc::ensure_ready(repo, config)?;
    dvc::execute_engine(&repo.root, ["fetch", "--", &format!("{boundary}.dvc")]).map_err(
        |error| {
            Error::message(format!(
                "move could not fetch the payload of {boundary}, which is not materialized here, so it left the boundary unchanged: {error}"
            ))
        },
    )?;
    Ok(())
}

/// Checks the renamed boundary out of the local cache, where
/// `fetch_unmaterialized_source` placed its payload, and confirms the result
/// matches the renamed metadata.
fn materialize_moved_destination(repo: &GitRepo, boundary: &str) -> Result<()> {
    let pointer = format!("{boundary}.dvc");
    dvc::execute_engine(&repo.root, ["checkout", "--", &pointer])?;
    let clean = dvc::status(repo, &pointer)?
        .as_object()
        .is_some_and(serde_json::Map::is_empty);
    if !clean {
        return Err(Error::message(format!(
            "the payload materialized for moved boundary {boundary} does not match its metadata"
        )));
    }
    Ok(())
}

fn rollback_move(
    old: &Path,
    new: &Path,
    old_materialized: bool,
    snapshot: MetadataSnapshot,
) -> Result<()> {
    let output_result = if old_materialized {
        match (old.exists(), new.exists()) {
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
        }
    } else {
        // The source had no payload, and the destination did not exist before
        // the move, so whatever the move materialized there is removed and the
        // snapshot restores the rest of the boundary: its metadata.
        remove_moved_output(new)
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

fn remove_moved_output(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path).at(path)
        }
        Ok(_) => fs::remove_file(path).at(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Io {
            path: path.to_path_buf(),
            source,
        }),
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

    #[cfg(unix)]
    #[test]
    fn placement_boundaries_may_not_be_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: temp.path().to_path_buf(),
        };
        fs::create_dir(temp.path().join("task")).unwrap();
        fs::write(temp.path().join("task/data.bin"), [1_u8; 12]).unwrap();
        std::os::unix::fs::symlink("data.bin", temp.path().join("task/latest.bin")).unwrap();
        assert_eq!(
            payload_metrics(&repo, "task/data.bin")
                .unwrap()
                .unwrap()
                .bytes,
            12
        );
        let error = payload_metrics(&repo, "task/latest.bin")
            .unwrap_err()
            .to_string();
        assert!(error.contains("may not be a symlink"), "{error}");
        assert!(payload_metrics(&repo, "task/absent.bin").unwrap().is_none());
    }
}
