use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::config::Config;
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::path::{allowed, relative_to, repo_path, resolved_under};
use crate::process::{run, run_unchecked};

const VERSION_VERIFY_SCRIPT: &str = include_str!("../assets/dvc_version_verify.py");

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
}

pub fn discover(repo: &GitRepo, scopes: &[String]) -> Result<Vec<String>> {
    let mut found = BTreeSet::new();
    for scope in scopes {
        let root = resolved_under(&repo.root, scope);
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
            if !entry.file_type().is_file()
                || entry.path().extension().and_then(|value| value.to_str()) != Some("dvc")
            {
                continue;
            }
            found.insert(relative_to(entry.path(), &repo.root, "DVC pointer")?);
        }
    }
    Ok(found.into_iter().collect())
}

pub fn output_paths(repo: &GitRepo, pointers: &[String]) -> Result<BTreeMap<String, Vec<String>>> {
    let mut result = BTreeMap::new();
    for pointer in pointers {
        let pointer_path = resolved_under(&repo.root, pointer);
        let raw = fs::read_to_string(&pointer_path).at(&pointer_path)?;
        let parsed: Pointer = serde_yaml::from_str(&raw).map_err(|source| Error::Yaml {
            path: pointer_path.clone(),
            source,
        })?;
        if parsed.outs.is_empty() {
            return Err(Error::message(format!(
                "DVC metadata did not define an output: {pointer}"
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
            outputs.insert(repo_path(&raw, "DVC output")?);
        }
        result.insert(pointer.clone(), outputs.into_iter().collect());
    }
    Ok(result)
}

pub fn status(repo: &GitRepo, pointer: &str) -> Result<serde_json::Value> {
    let output = run_unchecked("dvc", ["status", "--json", pointer], &repo.root)?;
    if !output.success() {
        return Err(Error::message(format!(
            "dvc status failed for {pointer}: {}",
            if output.stderr.trim().is_empty() {
                output.stdout.trim()
            } else {
                output.stderr.trim()
            }
        )));
    }
    serde_json::from_str(output.stdout.trim().if_empty("{}"))
        .map_err(|error| Error::message(format!("dvc status returned invalid JSON: {error}")))
}

pub fn reconcile(
    repo: &GitRepo,
    config: Option<&Config>,
    pointers: &[String],
    dry_run: bool,
) -> Result<DvcReport> {
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
            "DVC outputs are missing locally and will not be interpreted as deletions: {}; hydrate them before publishing",
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
        run("dvc", ["commit", "--force", pointer], &repo.root)?;
    }
    if !pointers.is_empty() {
        let mut args = vec!["push".to_owned()];
        args.extend(pointers.iter().cloned());
        run("dvc", args, &repo.root)?;
        report.verification = Some(verify(repo, config, pointers)?);
    }
    report.committed = dirty;
    report.pushed = pointers.to_vec();
    Ok(report)
}

pub fn verify(
    repo: &GitRepo,
    config: Option<&Config>,
    pointers: &[String],
) -> Result<serde_json::Value> {
    if pointers.is_empty() {
        return Ok(serde_json::json!({"mode": "no-files"}));
    }
    let local_args = std::iter::once("status".to_owned())
        .chain(std::iter::once("--quiet".to_owned()))
        .chain(pointers.iter().cloned())
        .collect::<Vec<_>>();
    let local = run_unchecked("dvc", local_args, &repo.root)?;
    if !local.success() {
        return Err(Error::message(format!(
            "DVC metadata does not match local data for: {}",
            pointers.join(", ")
        )));
    }

    let exact = config
        .map(|value| value.dvc.require_version_aware)
        .unwrap_or_else(|| detect_version_aware(repo).unwrap_or(false));
    if exact {
        let python = config
            .map(|value| value.dvc.python.as_str())
            .unwrap_or("python3");
        let serialized = serde_json::to_string(pointers)
            .map_err(|error| Error::message(format!("failed to encode DVC files: {error}")))?;
        let output = run_unchecked(
            python,
            [
                "-c",
                VERSION_VERIFY_SCRIPT,
                &repo.root.to_string_lossy(),
                &serialized,
            ],
            &repo.root,
        )?;
        if !output.success() {
            return Err(Error::message(format!(
                "failed to verify version-aware DVC content: {}",
                if output.stderr.trim().is_empty() {
                    output.stdout.trim()
                } else {
                    output.stderr.trim()
                }
            )));
        }
        return serde_json::from_str(output.stdout.trim()).map_err(|error| {
            Error::message(format!(
                "version-aware verifier returned invalid JSON: {error}"
            ))
        });
    }

    let cloud_args = std::iter::once("status".to_owned())
        .chain(["--cloud".to_owned(), "--quiet".to_owned()])
        .chain(pointers.iter().cloned())
        .collect::<Vec<_>>();
    let cloud = run_unchecked("dvc", cloud_args, &repo.root)?;
    if !cloud.success() {
        return Err(Error::message(format!(
            "DVC content is missing from the configured remote for: {}",
            pointers.join(", ")
        )));
    }
    Ok(serde_json::json!({"mode": "dvc-cloud-status"}))
}

fn detect_version_aware(repo: &GitRepo) -> Result<bool> {
    let remote = run_unchecked("dvc", ["config", "core.remote"], &repo.root)?;
    if !remote.success() || remote.stdout.trim().is_empty() {
        return Ok(false);
    }
    let key = format!("remote.{}.version_aware", remote.stdout.trim());
    let value = run_unchecked("dvc", ["config", &key], &repo.root)?;
    Ok(value.success() && value.stdout.trim().eq_ignore_ascii_case("true"))
}

pub fn hydrate(
    repo: &GitRepo,
    config: Option<&Config>,
    scopes: &[String],
    targets: &[String],
    dry_run: bool,
) -> Result<HydrateReport> {
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
                    "hydrate target is not a standalone DVC pointer: {target}"
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
    validate_worktree(repo, &pointers)?;
    let mut report = HydrateReport {
        status: if dry_run { "dry_run" } else { "pending" }.to_owned(),
        scopes: scopes.to_vec(),
        dvc_files: pointers.clone(),
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
    let fetch = std::iter::once("fetch".to_owned())
        .chain(pointers.iter().cloned())
        .collect::<Vec<_>>();
    run("dvc", fetch, &repo.root)?;
    let checkout = std::iter::once("checkout".to_owned())
        .chain(pointers.iter().cloned())
        .collect::<Vec<_>>();
    run("dvc", checkout, &repo.root)?;
    report.verification = Some(verify(repo, config, &pointers)?);
    report.status = "hydrated".to_owned();
    Ok(report)
}

#[derive(Debug, Clone, Serialize)]
pub struct HydrateReport {
    pub status: String,
    pub scopes: Vec<String>,
    pub dvc_files: Vec<String>,
    pub outputs: BTreeMap<String, Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<serde_json::Value>,
}

pub fn validate_worktree(repo: &GitRepo, pointers: &[String]) -> Result<()> {
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
        if outputs
            .get(pointer)
            .into_iter()
            .flatten()
            .any(|output| resolved_under(&repo.root, output).exists())
        {
            conflicts.push(pointer.clone());
        }
    }
    if !conflicts.is_empty() {
        return Err(Error::message(format!(
            "DVC metadata would overwrite locally changed outputs: {}; publish or preserve those changes first",
            conflicts.join(", ")
        )));
    }
    Ok(())
}

pub fn management(
    repo: &GitRepo,
    operation: &str,
    paths: &[String],
    dry_run: bool,
) -> Result<serde_json::Value> {
    if !dry_run {
        let mut args = match operation {
            "track" => vec!["add".to_owned()],
            "move" => vec!["move".to_owned()],
            "untrack" => vec!["remove".to_owned()],
            other => return Err(Error::message(format!("unknown DVC operation {other}"))),
        };
        args.extend(paths.iter().cloned());
        run("dvc", args, &repo.root)?;
    }
    Ok(serde_json::json!({
        "operation": operation,
        "paths": paths,
        "mode": if dry_run { "plan" } else { "apply" },
    }))
}

pub fn prepare_revision(
    repo: &GitRepo,
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
        let shared_cache = repo.root.join(".dvc/cache");
        fs::create_dir_all(&shared_cache).at(&shared_cache)?;
        let checkout_cache = checkout.join(".dvc/cache");
        if !checkout_cache.exists() {
            symlink_dir(&shared_cache, &checkout_cache)?;
        }
        let shared_local = repo.root.join(".dvc/config.local");
        let checkout_local = checkout.join(".dvc/config.local");
        if shared_local.is_file() && !checkout_local.exists() {
            symlink_file(&shared_local, &checkout_local)?;
        }
        let outputs = output_paths(&checkout_repo, pointers)?;
        let args = std::iter::once("fetch".to_owned())
            .chain(pointers.iter().cloned())
            .collect::<Vec<_>>();
        run("dvc", args, &checkout)?;
        Ok(PreparedRevision {
            prepared_files: pointers.to_vec(),
            outputs,
            mode: "fetched_to_shared_cache".to_owned(),
        })
    })();
    let _ = repo.run_unchecked(["worktree", "remove", "--force", &checkout.to_string_lossy()]);
    let _ = repo.run_unchecked(["worktree", "prune"]);
    result
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
