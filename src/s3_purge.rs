use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::hex::encode_lower;
use crate::storage_metadata;

// 0.6.0 checks this field before invoking its destructive adapter, but ignores
// additional fields. An extra protection field in schema 1 cannot fence it.
const STATE_SCHEMA: u32 = 2;
const STATE_NAME: &str = "s3-purge.json";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ObjectVersion {
    pub pointer: String,
    pub object: String,
    pub version_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PurgeState {
    schema_version: u32,
    pending: Vec<ObjectVersion>,
    #[serde(default)]
    pending_prefixes: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PurgeReport {
    pub status: String,
    pub queued: Vec<ObjectVersion>,
    pub deleted: Vec<ObjectVersion>,
    pub protected: Vec<ObjectVersion>,
    pub pending: Vec<ObjectVersion>,
    pub retained_unmapped: Vec<ObjectVersion>,
    pub retained_mapped: Vec<ObjectVersion>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pending_prefixes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

impl PurgeReport {
    pub fn warning(&self) -> Option<(&'static str, String)> {
        if !self.errors.is_empty() {
            Some((
                "s3-cleanup-failed",
                format!(
                    "Git synchronization succeeded, but S3 cleanup did not finish: {}. Retry uses the last durable checkpoint; {} exact version records and {} archive prefixes remain queued for retry.",
                    self.errors.join("; "),
                    self.pending.len(),
                    self.pending_prefixes.len(),
                ),
            ))
        } else if self.status == "blocked_unmapped" {
            Some((
                "s3-cleanup-blocked-unmapped",
                format!(
                    "S3 cleanup is incomplete: {} exact source versions or delete markers have no verified archive mapping. Their bytes remain intact and retirement records remain queued for retry; resolve this history before treating the archive as complete.",
                    self.retained_unmapped.len()
                ),
            ))
        } else if self.status == "cleanup_pending" {
            Some((
                "s3-cleanup-pending",
                format!(
                    "S3 cleanup is pending: {} exact versions or delete markers and {} complete archive prefixes still require retirement verification. Git publication or synchronization succeeded, but storage retirement requires a later publish or refresh.",
                    self.pending.len(),
                    self.pending_prefixes.len()
                ),
            ))
        } else {
            None
        }
    }
}

pub fn candidates_between(
    repo: &GitRepo,
    config: &Config,
    old_revision: &str,
    new_revision: &str,
    scopes: &[String],
) -> Result<Vec<ObjectVersion>> {
    if !config.requires_object_versioning() {
        return Ok(Vec::new());
    }
    let old = objects_at(
        repo,
        config,
        Some(old_revision),
        &pointers_at(repo, old_revision, scopes)?,
    )?;
    let new = objects_at(
        repo,
        config,
        Some(new_revision),
        &pointers_at(repo, new_revision, scopes)?,
    )?;
    let live_objects = new
        .iter()
        .map(|object| object.object.as_str())
        .collect::<BTreeSet<_>>();
    let mut candidates = old
        .into_iter()
        .filter(|object| !live_objects.contains(object.object.as_str()))
        .collect::<Vec<_>>();
    // A fresh clone has no private copy journal or retirement queue. The
    // merged receipt supplies the complete original history, including retired
    // keys and markers that current storage manifests cannot enumerate.
    candidates.extend(archive_candidates_at(repo, new_revision, scopes)?);
    candidates.sort();
    candidates.dedup();
    Ok(candidates)
}

pub fn candidates_for_revision(
    repo: &GitRepo,
    config: &Config,
    revision: &str,
    scopes: &[String],
) -> Result<Vec<ObjectVersion>> {
    if !config.requires_object_versioning() {
        return Ok(Vec::new());
    }
    objects_at(
        repo,
        config,
        Some(revision),
        &pointers_at(repo, revision, scopes)?,
    )
}

pub fn candidates_for_worktree(
    repo: &GitRepo,
    config: &Config,
    scopes: &[String],
) -> Result<Vec<ObjectVersion>> {
    if !config.requires_object_versioning() {
        return Ok(Vec::new());
    }
    objects_at(
        repo,
        config,
        None,
        &storage_metadata::discover(repo, scopes)?,
    )
}

pub fn queue(repo: &GitRepo, candidates: &[ObjectVersion]) -> Result<()> {
    if candidates.is_empty() {
        return Ok(());
    }
    let mut state = read_state(repo)?;
    state.pending.extend_from_slice(candidates);
    state.pending.sort();
    state.pending.dedup();
    write_state(repo, &state)
}

pub fn archive_prefixes(repo: &GitRepo) -> Result<BTreeMap<String, serde_json::Value>> {
    Ok(read_state(repo)?.pending_prefixes)
}

/// A complete copied receipt is also a prefix-cleanliness intent when its
/// version inventory is empty. Never fabricate an object version as a sentinel.
pub fn queue_archive_prefixes(repo: &GitRepo, receipts: &[serde_json::Value]) -> Result<()> {
    let mut state = read_state(repo)?;
    for receipt in receipts {
        // Local-only archive receipts have no remote object history to retire.
        if receipt["bucket"].as_str().is_none_or(str::is_empty) {
            continue;
        }
        let source = receipt_prefix(receipt)?;
        if let Some(previous) = state.pending_prefixes.get(&source)
            && previous != receipt
        {
            return Err(Error::message(format!(
                "pending archive prefix {source:?} has a different immutable receipt"
            )));
        }
        state.pending_prefixes.insert(source, receipt.clone());
    }
    write_state(repo, &state)
}

fn receipt_prefix(receipt: &serde_json::Value) -> Result<String> {
    let destination = receipt["destination"]
        .as_str()
        .ok_or_else(|| Error::message("archive prefix intent has no destination"))?;
    crate::archive_migration::validate(
        &format!("{destination}/{}", crate::archive_migration::RECEIPT_NAME),
        receipt,
    )?;
    if receipt["status"] != "copied" || receipt["bucket"].as_str().is_none_or(str::is_empty) {
        return Err(Error::message("invalid copied S3 archive prefix intent"));
    }
    Ok(receipt["source"]
        .as_str()
        .expect("validated source")
        .to_owned())
}

/// Cancel only newly queued retirement records belonging to an archive attempt.
/// Older records and records for other tasks keep their original protection.
pub fn cancel_archive(
    repo: &GitRepo,
    source: &str,
    destination: &str,
    previous: &[ObjectVersion],
    previous_prefixes: &BTreeMap<String, serde_json::Value>,
) -> Result<()> {
    let mut state = read_state(repo)?;
    state.pending.retain(|item| {
        previous.contains(item)
            || !(item.object.starts_with(&format!("{source}/"))
                || item.object.starts_with(&format!("{destination}/")))
    });
    for prefix in [source, destination] {
        state.pending_prefixes.remove(prefix);
        if let Some(receipt) = previous_prefixes.get(prefix) {
            state
                .pending_prefixes
                .insert(prefix.to_owned(), receipt.clone());
        }
    }
    write_state(repo, &state)
}

pub fn preview(repo: &GitRepo) -> Result<PurgeReport> {
    let state = read_state(repo)?;
    Ok(PurgeReport {
        status: if state.pending.is_empty() && state.pending_prefixes.is_empty() {
            "no_changes"
        } else {
            "pending"
        }
        .to_owned(),
        pending: state.pending,
        pending_prefixes: state.pending_prefixes.into_keys().collect(),
        ..PurgeReport::default()
    })
}

#[cfg(test)]
pub fn has_pending(repo: &GitRepo) -> Result<bool> {
    let state = read_state(repo)?;
    Ok(!state.pending.is_empty() || !state.pending_prefixes.is_empty())
}

/// Inspects pending retirement state without adopting old private directories.
/// Repository management uses this during its read-only preflight.
pub(crate) fn has_pending_read_only(repo: &GitRepo) -> Result<bool> {
    let mut directories = vec![crate::local_state::directory_unmigrated(repo)?];
    let common = repo.common_dir()?.canonicalize().at(&repo.root)?;
    directories.extend(crate::local_state::legacy_directories(repo)?);
    for directory in directories {
        if let Ok(relative) = directory.strip_prefix(&common) {
            crate::path::reject_symlink_traversal(
                &common,
                &crate::path::to_slash(relative),
                "legacy private state",
            )?;
        }
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(Error::message(
                    "private S3 purge state requires a regular state directory",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(Error::Io {
                    path: directory,
                    source,
                });
            }
        }
        let path = directory.join(STATE_NAME);
        let state = read_state_at(&path)?;
        if !state.pending.is_empty() || !state.pending_prefixes.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn purge_pending(repo: &GitRepo, config: &Config, remote: &str) -> Result<PurgeReport> {
    let mut state = read_state(repo)?;
    if state.pending.is_empty() && state.pending_prefixes.is_empty() {
        return Ok(PurgeReport {
            status: "no_changes".to_owned(),
            ..PurgeReport::default()
        });
    }
    // A legacy queue may be read for preview, but any retry must first fence
    // old clients durably, even when no prefix promotion is necessary.
    write_state(repo, &state)?;
    storage_metadata::ensure_ready(repo, config)?;
    storage_metadata::verify_object_versioning(repo, config)?;
    let base = repo.fetch_branch(remote, &config.git.branch)?;
    let published = archive_receipts_at(repo, &base, &[])?;
    if promote_archive_prefixes(&mut state, &published)? {
        // A generic-only retirement under a canonical archived source also
        // needs the complete prefix scan. Persist this expansion before any
        // adapter call so interruption cannot lose retired keys or markers.
        write_state(repo, &state)?;
    }
    let prefixes = state
        .pending_prefixes
        .values()
        .filter(|receipt| published.contains(receipt))
        .cloned()
        .collect::<Vec<_>>();
    let mut protected_objects = referenced_objects(repo, config, remote, &state.pending)?;
    // A task publication can remove every live pointer at the original path
    // before its copied receipt reaches the shared branch. Those generic
    // aliases are still archive SOURCE retirement, and must wait for the same
    // shared publication proof as the unsubmitted prefix itself.
    protected_objects.extend(deferred_archive_source_candidates(
        repo,
        &state.pending,
        &state.pending_prefixes,
        &published,
    )?);
    protected_objects.sort();
    protected_objects.dedup();
    purge_groups(
        state,
        protected_objects,
        &prefixes,
        |payload| storage_metadata::version_purge_adapter(repo, "delete", payload),
        |next| write_state(repo, next),
    )
}

fn deferred_archive_source_candidates(
    repo: &GitRepo,
    candidates: &[ObjectVersion],
    pending_prefixes: &BTreeMap<String, serde_json::Value>,
    published: &[serde_json::Value],
) -> Result<Vec<ObjectVersion>> {
    let mut deferred = BTreeSet::new();
    let mut client = None;
    for receipt in pending_prefixes
        .values()
        .filter(|receipt| !published.contains(receipt))
    {
        let source = receipt_prefix(receipt)?;
        if client.is_none() {
            client = Some(crate::native_s3::S3Client::from_repo(repo)?);
        }
        let canonical = crate::native_archive::registry_read(
            client.as_ref().expect("initialized storage client"),
            repo,
            &source,
        )?;
        if canonical.as_ref() != Some(receipt) {
            return Err(Error::message(
                "pending archive source differs from its canonical registry before shared publication",
            ));
        }
        // Protection needs only the existing immutable claim. It must not
        // attempt to create/republish a claim or require a shared receipt that
        // this very source retirement is waiting for.
        verify_retained_destination_binding(repo, receipt)?;
        deferred.insert(format!("{source}/"));
    }
    Ok(candidates
        .iter()
        .filter(|candidate| {
            deferred
                .iter()
                .any(|source| candidate.object.starts_with(source))
        })
        .cloned()
        .collect())
}

/// Keep the successful shared-checkout result reviewable when retirement fails.
/// Destructive callers still use `purge_pending` and receive its error normally.
pub(crate) fn purge_after_sync(
    repo: &GitRepo,
    config: &Config,
    remote: &str,
) -> Result<PurgeReport> {
    match purge_pending(repo, config, remote) {
        Ok(report) => Ok(report),
        Err(error) => {
            let mut report = preview(repo)?;
            report.status = "cleanup_pending".to_owned();
            report.errors.push(error.to_string());
            Ok(report)
        }
    }
}

#[derive(Default)]
struct PurgeGroup {
    candidates: Vec<ObjectVersion>,
    prefixes: Vec<serde_json::Value>,
}

/// A group is the entire source namespace of one bound receipt. Generic and
/// archive aliases of the same physical version stay together. Never split a
/// prefix: its final inventory must account for all mapped/protected history.
fn purge_groups(
    mut state: PurgeState,
    protected: Vec<ObjectVersion>,
    prefixes: &[serde_json::Value],
    mut adapter: impl FnMut(&serde_json::Value) -> Result<serde_json::Value>,
    mut checkpoint: impl FnMut(&PurgeState) -> Result<()>,
) -> Result<PurgeReport> {
    let mut groups = BTreeMap::<String, PurgeGroup>::new();
    for receipt in prefixes {
        let source = receipt_prefix(receipt)?;
        groups
            .entry(source)
            .or_default()
            .prefixes
            .push(receipt.clone());
    }
    let mut generic = Vec::new();
    for candidate in &state.pending {
        let source = groups
            .keys()
            .filter(|source| candidate.object.starts_with(&format!("{source}/")))
            .max_by_key(|source| source.len())
            .cloned();
        if let Some(source) = source {
            groups
                .get_mut(&source)
                .expect("known purge group")
                .candidates
                .push(candidate.clone());
        } else {
            generic.push(candidate.clone());
        }
    }
    let mut groups = groups.into_iter().collect::<Vec<_>>();
    // Bound generic batches limit lost progress without inventing receipt
    // ownership or claiming an unscanned prefix is empty.
    for (index, candidates) in generic.chunks(1000).enumerate() {
        groups.push((
            format!("generic batch {}", index + 1),
            PurgeGroup {
                candidates: candidates.to_vec(),
                prefixes: Vec::new(),
            },
        ));
    }
    let protected_set = protected
        .iter()
        .map(|candidate| (candidate.object.clone(), candidate.version_id.clone()))
        .collect::<BTreeSet<_>>();
    let mut report = PurgeReport::default();
    for (index, (label, group)) in groups.iter().enumerate() {
        let group_set = group.candidates.iter().cloned().collect::<BTreeSet<_>>();
        let group_protected = group
            .candidates
            .iter()
            .filter(|candidate| {
                protected_set.contains(&(candidate.object.clone(), candidate.version_id.clone()))
            })
            .cloned()
            .collect::<Vec<_>>();
        let deleted = group
            .candidates
            .iter()
            .filter(|candidate| {
                !protected_set.contains(&(candidate.object.clone(), candidate.version_id.clone()))
            })
            .cloned()
            .collect::<Vec<_>>();
        let submitted = group
            .prefixes
            .iter()
            .map(receipt_prefix)
            .collect::<Result<BTreeSet<_>>>()?;
        let requested = !deleted.is_empty() || !group.prefixes.is_empty();
        let response = if !requested {
            serde_json::json!({
                "retained_unmapped":[],"retained_mapped":[],"cleaned_prefixes":[]
            })
        } else {
            eprintln!(
                "workspace-mgr: S3 cleanup group {}/{}: {label} ({} queued records)",
                index + 1,
                groups.len(),
                group.candidates.len()
            );
            adapter(&serde_json::json!({"candidates":deleted,"prefixes":group.prefixes}))?
        };
        if requested {
            if response["mode"] != "permanent-version-deletion" {
                return Err(Error::message(
                    "storage purge response has no confirmed deletion mode",
                ));
            }
            for field in [
                "deleted",
                "already_absent",
                "retained_unmapped",
                "retained_mapped",
                "cleaned_prefixes",
            ] {
                if !response[field].is_array() {
                    return Err(Error::message(format!(
                        "storage purge response omitted {field} inventory"
                    )));
                }
            }
        }
        let parse = |name: &str| -> Result<Vec<ObjectVersion>> {
            response
                .get(name)
                .map(|value| {
                    serde_json::from_value(value.clone()).map_err(|error| {
                        Error::message(format!("invalid {name} archive version report: {error}"))
                    })
                })
                .transpose()
                .map(Option::unwrap_or_default)
        };
        let cleaned = response
            .get("cleaned_prefixes")
            .map(|value| {
                serde_json::from_value(value.clone()).map_err(|error| {
                    Error::message(format!("invalid cleaned archive prefix report: {error}"))
                })
            })
            .transpose()?
            .unwrap_or_default();
        let retained_unmapped = parse("retained_unmapped")?;
        let retained_mapped = parse("retained_mapped")?;
        if requested {
            validate_purge_acknowledgements(
                &deleted,
                &response,
                &retained_unmapped,
                &retained_mapped,
            )?;
        }
        let (next_group, group_report) = finish_purge_with_prefixes(
            &group.candidates,
            group_protected,
            deleted,
            retained_unmapped,
            retained_mapped,
            PrefixCompletion {
                pending: state
                    .pending_prefixes
                    .iter()
                    .filter(|(source, _)| submitted.contains(*source))
                    .map(|(source, receipt)| (source.clone(), receipt.clone()))
                    .collect(),
                submitted: submitted.clone(),
                cleaned,
            },
        )?;
        // Only after validated success can this group's obligations change.
        // Every unsubmitted version and prefix remains byte-for-byte queued.
        state
            .pending
            .retain(|candidate| !group_set.contains(candidate));
        state.pending.extend(next_group.pending);
        state.pending.sort();
        state.pending.dedup();
        for source in &submitted {
            state.pending_prefixes.remove(source);
        }
        state.pending_prefixes.extend(next_group.pending_prefixes);
        checkpoint(&state)?;
        report.deleted.extend(group_report.deleted);
        report.protected.extend(group_report.protected);
        report
            .retained_unmapped
            .extend(group_report.retained_unmapped);
        report.retained_mapped.extend(group_report.retained_mapped);
    }
    report.deleted.sort();
    report.deleted.dedup();
    report.protected.sort();
    report.protected.dedup();
    report.pending = state.pending;
    report.pending_prefixes = state.pending_prefixes.into_keys().collect();
    report.status = if !report.retained_unmapped.is_empty() {
        "blocked_unmapped"
    } else if report.pending.is_empty() && report.pending_prefixes.is_empty() {
        "complete"
    } else {
        "cleanup_pending"
    }
    .to_owned();
    Ok(report)
}

fn validate_purge_acknowledgements(
    submitted: &[ObjectVersion],
    response: &serde_json::Value,
    retained_unmapped: &[ObjectVersion],
    retained_mapped: &[ObjectVersion],
) -> Result<()> {
    let objects = submitted
        .iter()
        .map(|item| item.object.as_str())
        .collect::<BTreeSet<_>>();
    let mut confirmed = BTreeSet::new();
    for field in ["deleted", "already_absent"] {
        for row in response[field]
            .as_array()
            .expect("validated response array")
        {
            let object = row["object"]
                .as_str()
                .filter(|object| objects.contains(object))
                .ok_or_else(|| Error::message("storage purge confirmed an unknown object"))?;
            let versions = if field == "deleted" && row.get("deleted_version_ids").is_some() {
                row["deleted_version_ids"]
                    .as_array()
                    .filter(|versions| !versions.is_empty())
                    .ok_or_else(|| Error::message("storage purge has invalid deleted version IDs"))?
                    .iter()
                    .map(|version| version.as_str())
                    .collect::<Vec<_>>()
            } else {
                vec![row["version_id"].as_str()]
            };
            for version in versions {
                let version = version
                    .filter(|version| !version.is_empty())
                    .ok_or_else(|| Error::message("storage purge confirmed an invalid version"))?;
                confirmed.insert((object, version));
            }
        }
    }
    let retained = retained_unmapped
        .iter()
        .chain(retained_mapped)
        .map(|item| (item.object.as_str(), item.version_id.as_str()))
        .collect::<BTreeSet<_>>();
    if confirmed.iter().any(|item| retained.contains(item)) {
        return Err(Error::message(
            "storage purge both confirmed deletion and retained the same version",
        ));
    }
    if submitted.iter().any(|item| {
        let physical = (item.object.as_str(), item.version_id.as_str());
        !confirmed.contains(&physical) && !retained.contains(&physical)
    }) {
        return Err(Error::message(
            "storage purge omitted confirmation for a submitted exact version",
        ));
    }
    Ok(())
}

fn promote_archive_prefixes(
    state: &mut PurgeState,
    published: &[serde_json::Value],
) -> Result<bool> {
    let before_pending = state.pending.clone();
    let before_prefixes = state.pending_prefixes.clone();
    for receipt in published {
        if receipt["bucket"].as_str().is_none_or(str::is_empty) {
            continue;
        }
        let source = receipt_prefix(receipt)?;
        if !state
            .pending
            .iter()
            .any(|item| item.object.starts_with(&format!("{source}/")))
        {
            continue;
        }
        if let Some(previous) = state.pending_prefixes.get(&source)
            && previous != receipt
        {
            return Err(Error::message(
                "pending source retirement differs from its published archive receipt",
            ));
        }
        state.pending_prefixes.insert(source, receipt.clone());
        state
            .pending
            .extend(crate::archive_migration::purge_candidates(
                std::slice::from_ref(receipt),
            )?);
    }
    state.pending.sort();
    state.pending.dedup();
    Ok(state.pending != before_pending || state.pending_prefixes != before_prefixes)
}

/// Preserve every version that the adapter observed but did not retire. In
/// particular, an unmapped concurrent write must not disappear from the retry
/// queue merely because the original mapped source versions were deleted.
#[derive(Default)]
struct PrefixCompletion {
    pending: BTreeMap<String, serde_json::Value>,
    submitted: BTreeSet<String>,
    cleaned: Vec<String>,
}

#[cfg(test)]
fn finish_purge(
    candidates: &[ObjectVersion],
    protected: Vec<ObjectVersion>,
    deleted: Vec<ObjectVersion>,
    retained_unmapped: Vec<ObjectVersion>,
    retained_mapped: Vec<ObjectVersion>,
) -> Result<(PurgeState, PurgeReport)> {
    finish_purge_with_prefixes(
        candidates,
        protected,
        deleted,
        retained_unmapped,
        retained_mapped,
        PrefixCompletion::default(),
    )
}

fn finish_purge_with_prefixes(
    candidates: &[ObjectVersion],
    mut protected: Vec<ObjectVersion>,
    mut deleted: Vec<ObjectVersion>,
    mut retained_unmapped: Vec<ObjectVersion>,
    mut retained_mapped: Vec<ObjectVersion>,
    mut prefixes: PrefixCompletion,
) -> Result<(PurgeState, PurgeReport)> {
    if retained_mapped
        .iter()
        .any(|item| !candidates.contains(item))
    {
        return Err(Error::message(
            "storage purge retained an unknown candidate",
        ));
    }
    if retained_unmapped.iter().any(|item| {
        let Some(source) = item
            .pointer
            .strip_suffix(&format!("/{}", crate::archive_migration::RECEIPT_NAME))
        else {
            return true;
        };
        // The embedded adapter can discover a bound ancestor receipt for a
        // generic pointer. Permit that normalization only within a source
        // containing one of this run's candidates, never an adjacent task.
        item.version_id.is_empty()
            || crate::path::repo_path(source, "retained archive source").is_err()
            || !item.object.starts_with(&format!("{source}/"))
            || (!prefixes.submitted.contains(source)
                && !candidates
                    .iter()
                    .any(|candidate| candidate.object.starts_with(&format!("{source}/"))))
    }) {
        return Err(Error::message(
            "storage purge retained an unmapped version outside its archive sources",
        ));
    }
    retained_unmapped.sort();
    retained_unmapped.dedup();
    retained_mapped.sort();
    retained_mapped.dedup();
    // The same physical object version can have generic and archive pointer
    // records. Never claim it was deleted while either adapter report says it
    // is still present.
    deleted.retain(|item| {
        !retained_mapped
            .iter()
            .chain(&retained_unmapped)
            .any(|retained| {
                retained.object == item.object && retained.version_id == item.version_id
            })
    });
    protected.extend(retained_mapped.iter().cloned());
    protected.sort();
    protected.dedup();
    let mut pending = protected.clone();
    pending.extend(retained_unmapped.iter().cloned());
    pending.sort();
    pending.dedup();
    for prefix in &prefixes.cleaned {
        if !prefixes.submitted.contains(prefix)
            || pending
                .iter()
                .any(|item| item.object.starts_with(&format!("{prefix}/")))
        {
            return Err(Error::message(
                "storage purge reported an unverified empty archive prefix",
            ));
        }
        prefixes.pending.remove(prefix);
    }
    let pending_prefixes = prefixes.pending.keys().cloned().collect::<Vec<_>>();
    let status = if !retained_unmapped.is_empty() {
        "blocked_unmapped"
    } else if pending.is_empty() && pending_prefixes.is_empty() {
        "complete"
    } else {
        "cleanup_pending"
    };
    Ok((
        PurgeState {
            schema_version: STATE_SCHEMA,
            pending: pending.clone(),
            pending_prefixes: prefixes.pending,
        },
        PurgeReport {
            status: status.to_owned(),
            queued: Vec::new(),
            deleted,
            protected,
            pending,
            retained_unmapped,
            retained_mapped,
            pending_prefixes,
            errors: Vec::new(),
        },
    ))
}

fn objects_at(
    repo: &GitRepo,
    config: &Config,
    revision: Option<&str>,
    pointers: &[String],
) -> Result<Vec<ObjectVersion>> {
    if pointers.is_empty() {
        return Ok(Vec::new());
    }
    storage_metadata::ensure_ready(repo, config)?;
    let payload = serde_json::json!([{
        "revision": revision,
        "pointers": pointers,
    }]);
    let value = storage_metadata::version_purge_adapter(repo, "list", &payload)?;
    let mut objects: Vec<ObjectVersion> = serde_json::from_value(value).map_err(|error| {
        Error::message(format!(
            "managed-storage purge adapter returned invalid objects: {error}"
        ))
    })?;
    objects.sort();
    objects.dedup();
    Ok(objects)
}

fn paths_at(repo: &GitRepo, revision: &str, scopes: &[String]) -> Result<Vec<String>> {
    let mut args = vec![
        "ls-tree".to_owned(),
        "-r".to_owned(),
        "--name-only".to_owned(),
        "-z".to_owned(),
        revision.to_owned(),
        "--".to_owned(),
    ];
    if scopes.is_empty() {
        args.push(".".to_owned());
    } else {
        args.extend(scopes.iter().map(|scope| format!(":(literal){scope}")));
    }
    let mut paths = repo
        .run(args)?
        .stdout
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn pointers_at(repo: &GitRepo, revision: &str, scopes: &[String]) -> Result<Vec<String>> {
    Ok(paths_at(repo, revision, scopes)?
        .into_iter()
        // Keep literal backslashes: native metadata reads each pointer through
        // Git at this revision. Dropping such a path would leave retired object
        // versions in the bucket after a rename or deletion.
        .filter(|path| storage_metadata::is_pointer(path))
        .collect())
}

pub(crate) fn archive_candidates_at(
    repo: &GitRepo,
    revision: &str,
    scopes: &[String],
) -> Result<Vec<ObjectVersion>> {
    crate::archive_migration::purge_candidates(&archive_receipts_at(repo, revision, scopes)?)
}

pub(crate) fn archive_receipts_at(
    repo: &GitRepo,
    revision: &str,
    scopes: &[String],
) -> Result<Vec<serde_json::Value>> {
    let mut receipts = Vec::new();
    let paths = paths_at(repo, revision, scopes)?
        .into_iter()
        .filter(|path| path.ends_with(&format!("/{}", crate::archive_migration::RECEIPT_NAME)))
        .collect::<Vec<_>>();
    for (path, raw) in repo.show_files(revision, &paths)? {
        let receipt: serde_json::Value = serde_json::from_str(&raw).map_err(|error| {
            Error::message(format!("invalid published archive receipt {path}: {error}"))
        })?;
        crate::archive_migration::validate(&path, &receipt)?;
        if receipt["status"] == "copied" {
            receipts.push(receipt);
        }
    }
    Ok(receipts)
}

fn referenced_objects(
    repo: &GitRepo,
    config: &Config,
    remote: &str,
    candidates: &[ObjectVersion],
) -> Result<Vec<ObjectVersion>> {
    let revisions = fetch_current_remote_tips(repo, remote)?;
    let base = repo.fetch_branch(remote, &config.git.branch)?;
    let published_receipts = archive_receipts_at(repo, &base, &[])?;
    let published_archive_candidates =
        crate::archive_migration::purge_candidates(&published_receipts)?;
    let published_archive_sources = published_receipts
        .iter()
        .filter_map(|receipt| receipt["source"].as_str())
        .collect::<BTreeSet<_>>();
    let published_archive_versions = published_archive_candidates
        .iter()
        .cloned()
        .map(|item| (item.object, item.version_id))
        .collect::<BTreeSet<_>>();
    let candidate_objects = candidates
        .iter()
        .map(|candidate| candidate.object.clone())
        .collect::<BTreeSet<_>>();
    let retained_destinations =
        retained_archive_destinations(repo, &published_receipts, &candidate_objects)?;
    let candidate_sources = candidates
        .iter()
        .filter_map(|candidate| {
            candidate
                .pointer
                .strip_suffix(&format!("/{}", crate::archive_migration::RECEIPT_NAME))
        })
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut requests = Vec::new();
    let mut archive_protected = BTreeSet::new();
    for revision in remote_trees(repo, &revisions)? {
        // A pending child pointer can disappear while a newly published
        // parent or renamed pointer still references the same object. Generic
        // retirement deletes its entire history, so collect references by
        // actual object identity through every live pointer, not its old name.
        let mut pointers = pointers_at(repo, &revision, &[])?;
        for source in archive_source_trees(repo, &revision, &candidate_sources)? {
            // An archive retires the entire prefix, including historical
            // files no current pointer names. Keep that complete snapshot
            // while any live branch or tag still contains the source task.
            // Pre-adoption tags can contain the task directory without a
            // manifest. Its tree is still a live reference to that logical
            // path; a same-named blob or coordination tag is not.
            if !published_archive_sources.contains(source.as_str()) {
                archive_protected.insert(format!("{source}/"));
            }
        }
        pointers.sort();
        pointers.dedup();
        if !pointers.is_empty() {
            requests.push(serde_json::json!({
                "revision": revision,
                "pointers": pointers,
            }));
        }
    }
    let referenced = if requests.is_empty() {
        Vec::new()
    } else {
        let value = storage_metadata::version_purge_adapter(
            repo,
            "list",
            &serde_json::Value::Array(requests),
        )?;
        serde_json::from_value::<Vec<ObjectVersion>>(value).map_err(|error| {
            Error::message(format!(
                "managed-storage purge adapter returned invalid references: {error}"
            ))
        })?
    };
    let referenced_exact = referenced
        .iter()
        .map(|object| (object.object.clone(), object.version_id.clone()))
        .collect::<BTreeSet<_>>();
    let referenced = referenced
        .into_iter()
        .map(|object| object.object)
        .collect::<BTreeSet<_>>();
    Ok(candidates
        .iter()
        .filter(|candidate| {
            // Once the shared branch publishes the complete copied receipt,
            // historical branches/tags read these exact versions through the
            // canonical registry. They no longer require duplicate bytes at
            // the original path. New/unmapped generations remain protected.
            let identity = (candidate.object.clone(), candidate.version_id.clone());
            if retained_destinations.contains(&identity) {
                true
            } else if published_archive_versions.contains(&identity) {
                false
            } else if published_archive_sources
                .iter()
                .any(|source| candidate.object.starts_with(&format!("{source}/")))
            {
                referenced_exact.contains(&identity)
            } else {
                referenced.contains(&candidate.object)
                    || archive_protected
                        .iter()
                        .any(|prefix| candidate.object.starts_with(prefix))
            }
        })
        .cloned()
        .collect())
}

/// A copied destination can remain the only physical copy after its live
/// pointer disappears. Only the current outer receipt owns physical versions:
/// nested earlier destinations may have been relocated again already.
pub(crate) fn retained_archive_destinations(
    repo: &GitRepo,
    published: &[serde_json::Value],
    objects: &BTreeSet<String>,
) -> Result<BTreeSet<(String, String)>> {
    let mut retained = BTreeSet::new();
    let mut client = None;
    for receipt in published {
        let rows = receipt["versions"]
            .as_array()
            .ok_or_else(|| Error::message("published archive receipt has no versions"))?;
        if !rows.iter().any(|row| {
            row["destination_object"]
                .as_str()
                .is_some_and(|object| objects.contains(object))
        }) {
            continue;
        }
        let source = receipt_prefix(receipt)?;
        if client.is_none() {
            client = Some(crate::native_s3::S3Client::from_repo(repo)?);
        }
        let client = client.as_ref().expect("initialized storage client");
        let canonical = crate::native_archive::registry_read(client, repo, &source)?;
        if canonical.as_ref() != Some(receipt) {
            return Err(Error::message(
                "retained archive destination differs from its published canonical registry",
            ));
        }
        // Read the existing binding only. A missing/replaced claim must not
        // authorize deletion by falling back to ordinary object retirement.
        verify_retained_destination_binding(repo, receipt)?;
        for row in rows {
            let object = row["destination_object"].as_str().ok_or_else(|| {
                Error::message("retained archive destination has no object identity")
            })?;
            if !objects.contains(object) {
                continue;
            }
            let version = row["destination_version_id"].as_str().ok_or_else(|| {
                Error::message("retained archive destination has no exact version identity")
            })?;
            retained.insert((object.to_owned(), version.to_owned()));
        }
    }
    Ok(retained)
}

pub(crate) fn verify_retained_destination_binding(
    repo: &GitRepo,
    receipt: &serde_json::Value,
) -> Result<()> {
    let config = Config::load(repo)?;
    let remote = &config.git.remote;
    repo.validate_remote_name(remote)?;
    let fetch = repo.run(["remote", "get-url", "--all", remote])?;
    let push = repo.run(["remote", "get-url", "--push", "--all", remote])?;
    if fetch.stdout.lines().count() != 1 || fetch.stdout != push.stdout {
        return Err(Error::message(
            "retained archive destination requires one identical Git fetch and push destination",
        ));
    }
    let body = serde_json::to_string(receipt)
        .map_err(|error| Error::message(format!("invalid retained archive receipt: {error}")))?;
    let expected = crate::archive_git_control::object_ids(&repo.root, &body, false)?;
    let reference = crate::archive_registry::binding_ref(receipt)?;
    let observed = repo.run(["ls-remote", "--refs", "--", remote, &reference])?;
    if observed.stdout != format!("{}\t{reference}\n", expected.commit)
        && observed.stdout != format!("{}\t{reference}\n", expected.legacy_blob)
    {
        return Err(Error::message(
            "retained archive destination has no matching canonical Git coordination binding",
        ));
    }
    Ok(())
}

/// Resolve every live branch/tag through the same two batched object queries.
/// Coordination tags can point at blobs; nested annotated tags are peeled
/// recursively before the remaining references are required to name trees.
fn remote_trees(repo: &GitRepo, revisions: &[String]) -> Result<BTreeSet<String>> {
    if revisions.is_empty() {
        return Ok(BTreeSet::new());
    }
    let peeled = revisions
        .iter()
        .map(|revision| format!("{revision}^{{}}\n"))
        .collect::<String>();
    let output = repo.run_bytes(
        ["cat-file", "--batch-check=%(objectname) %(objecttype)"],
        Some(peeled.as_bytes()),
    )?;
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| Error::message("Git object types are not UTF-8"))?;
    let records = text.lines().collect::<Vec<_>>();
    if records.len() != revisions.len() {
        return Err(Error::message(
            "Git object type batch omitted remote references",
        ));
    }
    let mut tree_requests = String::new();
    let mut expected = 0;
    for (revision, record) in revisions.iter().zip(records) {
        let Some((_, kind)) = record.split_once(' ') else {
            return Err(Error::message("unexpected Git object type entry"));
        };
        match kind {
            "blob" => {}
            "commit" | "tree" => {
                tree_requests.push_str(&format!("{revision}^{{tree}}\n"));
                expected += 1;
            }
            _ => {
                return Err(Error::message(format!(
                    "remote Git reference {revision} has unexpected type {kind:?}"
                )));
            }
        }
    }
    if expected == 0 {
        return Ok(BTreeSet::new());
    }
    let output = repo.run_bytes(
        ["cat-file", "--batch-check=%(objectname) %(objecttype)"],
        Some(tree_requests.as_bytes()),
    )?;
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| Error::message("Git trees are not UTF-8"))?;
    if text.lines().count() != expected {
        return Err(Error::message("Git tree batch omitted remote references"));
    }
    text.lines()
        .map(|line| match line.split_once(' ') {
            Some((oid, "tree"))
                if matches!(oid.len(), 40 | 64) && oid.bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                Ok(oid.to_owned())
            }
            _ => Err(Error::message(format!(
                "unexpected Git tree entry {line:?}"
            ))),
        })
        .collect()
}

fn archive_source_trees(
    repo: &GitRepo,
    revision: &str,
    sources: &[String],
) -> Result<BTreeSet<String>> {
    let requested = sources.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let literal = sources
        .iter()
        .map(|source| format!(":(literal){source}"))
        .collect::<Vec<_>>();
    let mut candidates = BTreeMap::new();
    for batch in crate::git::pathspec_batches(&literal) {
        let mut args = vec![
            "ls-tree".to_owned(),
            "-d".to_owned(),
            "-z".to_owned(),
            revision.to_owned(),
            "--".to_owned(),
        ];
        args.extend(batch.iter().cloned());
        let output = repo.run_bytes(args, None)?;
        for record in output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
        {
            let record = std::str::from_utf8(record)
                .map_err(|_| Error::message("archive source tree path is not UTF-8"))?;
            let Some((header, path)) = record.split_once('\t') else {
                return Err(Error::message("unexpected archive source tree entry"));
            };
            let fields = header.split(' ').collect::<Vec<_>>();
            if fields.get(1) == Some(&"tree") && requested.contains(path) {
                let oid = fields
                    .get(2)
                    .ok_or_else(|| Error::message("archive source tree entry has no object ID"))?;
                candidates.insert(path.to_owned(), (*oid).to_owned());
            }
        }
    }
    let ids = candidates.values().cloned().collect::<Vec<_>>();
    let types = repo.object_types(&ids)?;
    Ok(candidates
        .into_iter()
        .filter_map(|(path, oid)| (types[&oid].as_deref() == Some("tree")).then_some(path))
        .collect())
}

fn fetch_current_remote_tips(repo: &GitRepo, remote: &str) -> Result<Vec<String>> {
    let mut hasher = Sha256::new();
    hasher.update(remote.as_bytes());
    let namespace = format!(
        "refs/workspace-mgr/s3-protection/{}",
        encode_lower(hasher.finalize())
    );
    let heads = format!("+refs/heads/*:{namespace}/heads/*");
    let tags = format!("+refs/tags/*:{namespace}/tags/*");
    repo.run([
        "fetch",
        "--quiet",
        "--prune",
        "--no-tags",
        "--no-write-fetch-head",
        remote,
        &heads,
        &tags,
    ])?;
    let listing = repo.run([
        "for-each-ref",
        "--format=%(objectname) %(*objectname)",
        &namespace,
    ])?;
    let mut revisions = listing
        .stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let object = fields.next()?;
            Some(fields.next().unwrap_or(object).to_owned())
        })
        .collect::<Vec<_>>();
    revisions.sort();
    revisions.dedup();
    Ok(revisions)
}

fn state_path(repo: &GitRepo) -> Result<PathBuf> {
    Ok(repo.local_state_dir()?.join(STATE_NAME))
}

fn read_state(repo: &GitRepo) -> Result<PurgeState> {
    read_state_at(&state_path(repo)?)
}

fn read_state_at(path: &Path) -> Result<PurgeState> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(Error::message(
                "private S3 purge state must be a regular file",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PurgeState {
                schema_version: STATE_SCHEMA,
                pending: Vec::new(),
                pending_prefixes: BTreeMap::new(),
            });
        }
        Err(source) => {
            return Err(Error::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    }
    let raw = fs::read_to_string(path).at(path)?;
    let mut state: PurgeState = serde_json::from_str(&raw)
        .map_err(|error| Error::message(format!("invalid private S3 purge state: {error}")))?;
    if !matches!(state.schema_version, 1 | STATE_SCHEMA) {
        return Err(Error::message(
            "private S3 purge state has an unsupported schema",
        ));
    }
    state.schema_version = STATE_SCHEMA;
    state.pending.sort();
    state.pending.dedup();
    for (source, receipt) in &state.pending_prefixes {
        if receipt_prefix(receipt)? != *source {
            return Err(Error::message(
                "invalid source identity in private archive prefix intent",
            ));
        }
    }
    Ok(state)
}

fn write_state(repo: &GitRepo, state: &PurgeState) -> Result<()> {
    let path = state_path(repo)?;
    if state.pending.is_empty() && state.pending_prefixes.is_empty() {
        if path.is_file() {
            fs::remove_file(&path).at(&path)?;
        }
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| Error::message("private S3 purge state has no parent"))?;
    fs::create_dir_all(parent).at(parent)?;
    let mut upgraded = state.clone();
    upgraded.schema_version = STATE_SCHEMA;
    let encoded = serde_json::to_vec_pretty(&upgraded)
        .map_err(|error| Error::message(format!("failed to encode S3 purge state: {error}")))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).at(parent)?;
    temporary.write_all(&encoded).at(&path)?;
    temporary.write_all(b"\n").at(&path)?;
    temporary.flush().at(&path)?;
    temporary.as_file().sync_all().at(&path)?;
    temporary.persist(&path).map_err(|error| Error::Io {
        path: path.clone(),
        source: error.error,
    })?;
    fs::File::open(parent).at(parent)?.sync_all().at(parent)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive_version(source: &str, object: &str, version: &str) -> ObjectVersion {
        ObjectVersion {
            pointer: format!("{source}/{}", crate::archive_migration::RECEIPT_NAME),
            object: format!("{source}/{object}"),
            version_id: version.to_owned(),
        }
    }

    fn empty_receipt(source: &str) -> serde_json::Value {
        serde_json::json!({
            "schema_version":1,"task_id":source,"source":source,
            "destination":format!("2026/07/{source}"),"status":"copied",
            "bucket":"isolated-fixture","remote_prefix":"dvc","versions":[]
        })
    }

    fn purge_response(cleaned: &[&str]) -> serde_json::Value {
        serde_json::json!({"mode":"permanent-version-deletion",
            "deleted":[],"already_absent":[],"retained_unmapped":[],
            "retained_mapped":[],"cleaned_prefixes":cleaned})
    }

    #[test]
    fn cleanup_checkpoints_confirmed_prefixes_before_a_later_transport_failure() {
        let first = archive_version("first", "data", "v1");
        let second = archive_version("second", "data", "v2");
        let mut generic_alias = first.clone();
        generic_alias.pointer = "first/data.wm-storage.json".to_owned();
        let receipts = vec![empty_receipt("first"), empty_receipt("second")];
        let state = PurgeState {
            schema_version: STATE_SCHEMA,
            pending: vec![first.clone(), generic_alias, second.clone()],
            pending_prefixes: receipts
                .iter()
                .map(|receipt| {
                    (
                        receipt["source"].as_str().unwrap().to_owned(),
                        receipt.clone(),
                    )
                })
                .collect(),
        };
        let mut checkpoints = Vec::new();
        let mut calls = 0;
        let error = purge_groups(
            state,
            Vec::new(),
            &receipts,
            |payload| {
                calls += 1;
                if calls == 2 {
                    return Err(Error::message("S3 TransportError: timeout: connect"));
                }
                assert_eq!(payload["candidates"].as_array().unwrap().len(), 2);
                assert_eq!(payload["prefixes"].as_array().unwrap().len(), 1);
                let mut response = purge_response(&["first"]);
                response["deleted"] = payload["candidates"].clone();
                Ok(response)
            },
            |next| {
                checkpoints.push(next.clone());
                Ok(())
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("timeout: connect"));
        assert_eq!(calls, 2);
        assert_eq!(checkpoints.len(), 1);
        assert_eq!(checkpoints[0].pending, vec![second]);
        assert!(!checkpoints[0].pending_prefixes.contains_key("first"));
        assert!(checkpoints[0].pending_prefixes.contains_key("second"));
    }

    #[test]
    fn cleanup_does_not_checkpoint_a_false_empty_prefix_or_forget_unsubmitted_groups() {
        let first = archive_version("first", "data", "v1");
        let second = archive_version("second", "data", "v2");
        let receipt = empty_receipt("first");
        let state = PurgeState {
            schema_version: STATE_SCHEMA,
            pending: vec![first.clone(), second],
            pending_prefixes: BTreeMap::from([("first".to_owned(), receipt.clone())]),
        };
        let mut checkpoints = Vec::new();
        let error = purge_groups(
            state,
            vec![first.clone()],
            &[receipt],
            |_| {
                let mut response = purge_response(&["first"]);
                response["retained_mapped"] = serde_json::json!([first]);
                Ok(response)
            },
            |next| {
                checkpoints.push(next.clone());
                Ok(())
            },
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unverified empty archive prefix")
        );
        assert!(checkpoints.is_empty());
    }

    #[test]
    fn cleanup_preserves_protected_aliases_and_new_unmapped_source_versions() {
        let mapped = archive_version("source", "data", "v1");
        let mut alias = mapped.clone();
        alias.pointer = "source/data.wm-storage.json".to_owned();
        let unexpected = archive_version("source", "new", "concurrent");
        let receipt = empty_receipt("source");
        let state = PurgeState {
            schema_version: STATE_SCHEMA,
            pending: vec![mapped.clone(), alias],
            pending_prefixes: BTreeMap::from([("source".to_owned(), receipt.clone())]),
        };
        let mut checkpoints = Vec::new();
        let report = purge_groups(
            state,
            vec![mapped.clone()],
            &[receipt],
            |payload| {
                assert!(payload["candidates"].as_array().unwrap().is_empty());
                let mut response = purge_response(&[]);
                response["retained_mapped"] = serde_json::json!([mapped]);
                response["retained_unmapped"] = serde_json::json!([unexpected]);
                Ok(response)
            },
            |next| {
                checkpoints.push(next.clone());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(report.status, "blocked_unmapped");
        assert!(report.deleted.is_empty());
        assert_eq!(report.retained_unmapped, vec![unexpected.clone()]);
        assert!(checkpoints[0].pending.contains(&unexpected));
        assert!(checkpoints[0].pending_prefixes.contains_key("source"));
    }

    #[test]
    fn cleanup_keeps_last_checkpoint_when_persisting_later_progress_fails() {
        let candidates = (0..1001)
            .map(|index| ObjectVersion {
                pointer: "task/data.wm-storage.json".to_owned(),
                object: format!("task/data/{index}"),
                version_id: format!("v{index}"),
            })
            .collect::<Vec<_>>();
        let state = PurgeState {
            schema_version: STATE_SCHEMA,
            pending: candidates.clone(),
            pending_prefixes: BTreeMap::new(),
        };
        let mut persisted = None;
        let mut checkpoints = 0;
        let error = purge_groups(
            state,
            Vec::new(),
            &[],
            |payload| {
                let mut response = purge_response(&[]);
                response["deleted"] = payload["candidates"].clone();
                Ok(response)
            },
            |next| {
                checkpoints += 1;
                if checkpoints == 2 {
                    return Err(Error::message("checkpoint disk full"));
                }
                persisted = Some(next.clone());
                Ok(())
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("disk full"));
        assert_eq!(persisted.unwrap().pending, candidates[1000..]);
    }

    #[test]
    fn cleanup_rejects_missing_result_inventories_before_checkpointing() {
        let candidate = archive_version("task", "data", "v1");
        let state = PurgeState {
            schema_version: STATE_SCHEMA,
            pending: vec![candidate],
            pending_prefixes: BTreeMap::new(),
        };
        let mut responses = vec![serde_json::json!({}), purge_response(&[])];
        for field in [
            "deleted",
            "already_absent",
            "retained_unmapped",
            "retained_mapped",
            "cleaned_prefixes",
        ] {
            let mut response = purge_response(&[]);
            response.as_object_mut().unwrap().remove(field);
            responses.push(response);
        }
        for response in responses {
            let mut checkpointed = false;
            assert!(
                purge_groups(
                    state.clone(),
                    Vec::new(),
                    &[],
                    |_| Ok(response.clone()),
                    |_| {
                        checkpointed = true;
                        Ok(())
                    }
                )
                .is_err()
            );
            assert!(!checkpointed);
        }
    }

    #[test]
    fn cleanup_confirms_aggregated_deleted_and_absent_exact_versions() {
        let first = archive_version("task", "data", "v1");
        let second = archive_version("task", "data", "v2");
        let mut alias = first.clone();
        alias.pointer = "task/data.wm-storage.json".into();
        let candidates = vec![first.clone(), alias, second.clone()];
        let mut response = purge_response(&[]);
        response["deleted"] = serde_json::json!([{
            "object":first.object,"version_id":"exemplar-is-not-an-ack",
            "deleted_version_ids":["v1"]
        }]);
        assert!(validate_purge_acknowledgements(&candidates, &response, &[], &[]).is_err());
        response["already_absent"] = serde_json::json!([second]);
        validate_purge_acknowledgements(&candidates, &response, &[], &[]).unwrap();
        assert!(validate_purge_acknowledgements(&candidates, &response, &[], &[first]).is_err());
        response["already_absent"][0]["object"] = "neighbor/data".into();
        assert!(validate_purge_acknowledgements(&candidates, &response, &[], &[]).is_err());
    }

    #[test]
    fn archive_source_tree_batch_rejects_missing_referenced_tree_objects() {
        let temp = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: temp.path().to_owned(),
        };
        repo.run(["init", "-q", "-b", "main"]).unwrap();
        repo.run(["config", "user.name", "Purge fixture"]).unwrap();
        repo.run(["config", "user.email", "fixture@example.invalid"])
            .unwrap();
        for (path, content) in [
            ("outer/[literal]/file", "missing tree"),
            ("other/file", "live tree"),
        ] {
            let file = repo.root.join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, content).unwrap();
        }
        repo.run(["add", "-A"]).unwrap();
        repo.run(["commit", "-qm", "source trees"]).unwrap();
        let sources = ["outer/[literal]".to_owned(), "other".to_owned()];
        assert_eq!(
            archive_source_trees(&repo, "HEAD", &sources).unwrap(),
            sources.clone().into_iter().collect()
        );
        let oid = repo
            .run(["rev-parse", "HEAD:outer/[literal]"])
            .unwrap()
            .stdout
            .trim()
            .to_owned();
        fs::remove_file(
            repo.git_dir()
                .unwrap()
                .join("objects")
                .join(&oid[..2])
                .join(&oid[2..]),
        )
        .unwrap();
        assert_eq!(
            archive_source_trees(&repo, "HEAD", &sources).unwrap(),
            BTreeSet::from(["other".to_owned()])
        );
    }

    #[test]
    fn read_only_pending_inspection_does_not_adopt_legacy_private_state() {
        let fixture = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: fixture.path().canonicalize().unwrap(),
        };
        repo.run(["init", "-q", "-b", "main"]).unwrap();
        let legacy = repo.common_dir().unwrap().join("workspace-mgr");
        fs::create_dir_all(&legacy).unwrap();
        let raw = br#"{"schema_version":2,"pending":[{"pointer":"task/data.wm-storage.json","object":"task/data","version_id":"v1"}],"pending_prefixes":{}}"#;
        fs::write(legacy.join(STATE_NAME), raw).unwrap();
        fs::write(legacy.join("keep-state"), b"preserve").unwrap();
        assert!(has_pending_read_only(&repo).unwrap());
        assert_eq!(fs::read(legacy.join(STATE_NAME)).unwrap(), raw);
        assert_eq!(fs::read(legacy.join("keep-state")).unwrap(), b"preserve");
        assert!(
            !repo
                .root
                .join(crate::local_state::LOCAL_STATE_PATH)
                .exists()
        );
        assert!(!legacy.join("repository.lock").exists());
    }

    #[test]
    fn empty_archive_prefix_remains_pending_until_a_published_full_scan_confirms_empty() {
        let source = "empty-task";
        let prefixes = BTreeMap::from([(source.to_owned(), empty_receipt(source))]);
        let (before_merge, pending) = finish_purge_with_prefixes(
            &[],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            PrefixCompletion {
                pending: prefixes.clone(),
                ..PrefixCompletion::default()
            },
        )
        .unwrap();
        assert_eq!(pending.status, "cleanup_pending");
        assert!(pending.pending.is_empty());
        assert_eq!(pending.pending_prefixes, [source]);
        assert_eq!(before_merge.pending_prefixes, prefixes);

        let marker = archive_version(source, "late", "late-marker");
        let (blocked, report) = finish_purge_with_prefixes(
            &[],
            Vec::new(),
            Vec::new(),
            vec![marker.clone()],
            Vec::new(),
            PrefixCompletion {
                pending: prefixes.clone(),
                submitted: BTreeSet::from([source.to_owned()]),
                cleaned: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(report.status, "blocked_unmapped");
        assert_eq!(report.pending, [marker]);
        assert_eq!(blocked.pending_prefixes, prefixes);

        let (complete, report) = finish_purge_with_prefixes(
            &blocked.pending,
            Vec::new(),
            blocked.pending.clone(),
            Vec::new(),
            Vec::new(),
            PrefixCompletion {
                pending: blocked.pending_prefixes,
                submitted: BTreeSet::from([source.to_owned()]),
                cleaned: vec![source.to_owned()],
            },
        )
        .unwrap();
        assert_eq!(report.status, "complete");
        assert!(complete.pending.is_empty());
        assert!(complete.pending_prefixes.is_empty());
    }

    #[test]
    fn archive_prefix_cannot_be_cleared_without_submission_or_while_versions_remain() {
        let source = "empty-task";
        let prefix = || BTreeMap::from([(source.to_owned(), empty_receipt(source))]);
        let error = finish_purge_with_prefixes(
            &[],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            PrefixCompletion {
                pending: prefix(),
                submitted: BTreeSet::new(),
                cleaned: vec![source.to_owned()],
            },
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unverified empty archive prefix")
        );
        let marker = archive_version(source, "late", "late-marker");
        let error = finish_purge_with_prefixes(
            &[],
            Vec::new(),
            Vec::new(),
            vec![marker],
            Vec::new(),
            PrefixCompletion {
                pending: prefix(),
                submitted: BTreeSet::from([source.to_owned()]),
                cleaned: vec![source.to_owned()],
            },
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unverified empty archive prefix")
        );
    }

    #[test]
    fn prefix_intents_survive_disk_and_cancel_restores_only_the_current_attempt() {
        let fixture = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: fixture.path().canonicalize().unwrap(),
        };
        repo.run(["init", "-q", "-b", "main"]).unwrap();
        let prior = archive_prefixes(&repo).unwrap();
        let current = empty_receipt("empty-task");
        let other = empty_receipt("other-task");
        queue_archive_prefixes(&repo, &[current.clone(), other.clone()]).unwrap();
        assert!(has_pending(&repo).unwrap());
        assert_eq!(
            preview(&repo).unwrap().pending_prefixes,
            ["empty-task", "other-task"]
        );
        let original_bytes = fs::read(state_path(&repo).unwrap()).unwrap();
        let mut changed = current;
        changed["note"] = "different receipt".into();
        assert!(queue_archive_prefixes(&repo, &[changed]).is_err());
        assert_eq!(
            fs::read(state_path(&repo).unwrap()).unwrap(),
            original_bytes
        );
        let mut legacy: serde_json::Value = serde_json::from_slice(&original_bytes).unwrap();
        legacy["schema_version"] = 1.into();
        fs::write(
            state_path(&repo).unwrap(),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();
        cancel_archive(&repo, "empty-task", "2026/07/empty-task", &[], &prior).unwrap();
        assert_eq!(
            archive_prefixes(&repo).unwrap(),
            BTreeMap::from([("other-task".to_owned(), other)])
        );
        assert!(has_pending(&repo).unwrap());
        // Restoring a prior snapshot must never reintroduce a schema 1
        // queue that an old client could consume without prefix protection.
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(
                &fs::read(state_path(&repo).unwrap()).unwrap()
            )
            .unwrap()["schema_version"],
            2
        );
    }

    #[test]
    fn old_object_only_purge_journal_defaults_to_no_prefix_intents() {
        let fixture = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: fixture.path().canonicalize().unwrap(),
        };
        repo.run(["init", "-q", "-b", "main"]).unwrap();
        let path = state_path(&repo).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"schema_version":1,"pending":[]}"#).unwrap();
        assert!(archive_prefixes(&repo).unwrap().is_empty());
        assert!(!has_pending(&repo).unwrap());
        // Preview is a read only operation, even for a legacy queue.
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&fs::read(&path).unwrap()).unwrap()["schema_version"],
            1
        );
    }

    #[test]
    fn protected_prefix_queue_fences_the_actual_0_6_reader() {
        // This is the released 0.6.0 PurgeState/read_state contract at
        // 2e6f5d6:src/s3_purge.rs. Serde ignores new fields, so only the
        // incompatible schema check prevents its all-version deletion path.
        #[derive(Deserialize)]
        struct LegacyState {
            schema_version: u32,
            pending: Vec<ObjectVersion>,
        }
        let fixture = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: fixture.path().canonicalize().unwrap(),
        };
        repo.run(["init", "-q", "-b", "main"]).unwrap();
        let unmapped = archive_version("task", "new", "concurrent-marker");
        let state = PurgeState {
            schema_version: STATE_SCHEMA,
            pending: vec![unmapped.clone()],
            pending_prefixes: BTreeMap::from([("task".into(), empty_receipt("task"))]),
        };
        write_state(&repo, &state).unwrap();
        let raw = fs::read(state_path(&repo).unwrap()).unwrap();
        let legacy: LegacyState = serde_json::from_slice(&raw).unwrap();
        assert_eq!(legacy.pending, [unmapped]);
        assert_ne!(
            legacy.schema_version, 1,
            "0.6.0 must reject before its delete adapter"
        );
        assert_eq!(
            read_state(&repo).unwrap().pending_prefixes,
            state.pending_prefixes
        );
    }

    #[test]
    fn legacy_retry_is_fenced_before_even_a_failed_remote_preflight() {
        let fixture = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: fixture.path().canonicalize().unwrap(),
        };
        repo.run(["init", "-q", "-b", "main"]).unwrap();
        let path = state_path(&repo).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let pending = archive_version("task", "new", "unmapped-payload");
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "schema_version":1,"pending":[pending],
                "pending_prefixes":{"task":empty_receipt("task")}
            }))
            .unwrap(),
        )
        .unwrap();
        let before = fs::read(&path).unwrap();
        preview(&repo).unwrap();
        assert_eq!(fs::read(&path).unwrap(), before);
        // This repository has no initialized storage or remote. Its retry
        // cannot reach any S3 endpoint, yet the old-client fence must persist.
        assert!(purge_pending(&repo, &Config::default(), "missing-remote").is_err());
        let after: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(after["schema_version"], STATE_SCHEMA);
        assert_eq!(after["pending"][0]["version_id"], "unmapped-payload");
        assert_eq!(after["pending_prefixes"]["task"], empty_receipt("task"));
    }

    #[test]
    fn unknown_purge_schema_is_rejected_without_rewriting() {
        let fixture = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: fixture.path().canonicalize().unwrap(),
        };
        repo.run(["init", "-q", "-b", "main"]).unwrap();
        let path = state_path(&repo).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let raw = br#"{"schema_version":3,"pending":[],"pending_prefixes":{}}"#;
        fs::write(&path, raw).unwrap();
        assert!(
            preview(&repo)
                .unwrap_err()
                .to_string()
                .contains("unsupported schema")
        );
        assert_eq!(fs::read(&path).unwrap(), raw);
    }

    #[test]
    fn unmapped_payloads_and_markers_remain_durable_until_verified_retirement() {
        let old = archive_version("task", "data", "mapped-old");
        let protected = archive_version("task", "data", "protected-old");
        let late_payload = archive_version("task", "new", "late-payload");
        let late_marker = archive_version("task", "new", "late-marker");
        let candidates = vec![old.clone(), protected.clone()];
        let (state, report) = finish_purge(
            &candidates,
            vec![protected.clone()],
            vec![old.clone()],
            vec![
                late_payload.clone(),
                late_marker.clone(),
                late_payload.clone(),
            ],
            Vec::new(),
        )
        .unwrap();
        assert_eq!(report.status, "blocked_unmapped");
        assert_eq!(report.deleted, [old]);
        assert_eq!(
            report.protected.as_slice(),
            std::slice::from_ref(&protected)
        );
        assert_eq!(report.retained_unmapped.len(), 2);
        assert_eq!(report.pending, state.pending);
        assert!(state.pending.contains(&late_payload));
        assert!(state.pending.contains(&late_marker));
        assert!(state.pending.contains(&protected));
        assert_eq!(report.warning().unwrap().0, "s3-cleanup-blocked-unmapped");

        let fixture = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: fixture.path().canonicalize().unwrap(),
        };
        repo.run(["init", "-b", "main"]).unwrap();
        write_state(&repo, &state).unwrap();
        assert_eq!(read_state(&repo).unwrap().pending, state.pending);
        assert!(has_pending(&repo).unwrap());
        let retry = read_state(&repo).unwrap();
        let (cleared, complete) = finish_purge(
            &retry.pending,
            Vec::new(),
            retry.pending.clone(),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(complete.status, "complete");
        assert!(complete.pending.is_empty());
        assert!(complete.warning().is_none());
        write_state(&repo, &cleared).unwrap();
        assert!(!has_pending(&repo).unwrap());
        assert!(!state_path(&repo).unwrap().exists());
    }

    #[test]
    fn protected_or_retained_mapped_history_never_reports_complete() {
        let referenced = archive_version("task", "data", "referenced");
        let retained = archive_version("task", "data", "retained");
        let candidates = vec![referenced.clone(), retained.clone()];
        let (state, report) = finish_purge(
            &candidates,
            vec![referenced.clone()],
            vec![retained.clone()],
            Vec::new(),
            vec![retained.clone()],
        )
        .unwrap();
        assert_eq!(report.status, "cleanup_pending");
        assert!(report.deleted.is_empty());
        assert_eq!(report.protected.len(), 2);
        assert!(state.pending.contains(&referenced));
        assert!(state.pending.contains(&retained));
        assert_eq!(report.warning().unwrap().0, "s3-cleanup-pending");
    }

    #[test]
    fn adapter_cannot_queue_unmapped_versions_for_another_source_or_empty_id() {
        let scheduled = archive_version("task", "data", "scheduled");
        for retained in [
            archive_version("other-task", "data", "other"),
            ObjectVersion {
                object: "task-neighbor/data".to_owned(),
                ..archive_version("task", "data", "neighbor")
            },
            archive_version("task", "data", ""),
        ] {
            let rejected = finish_purge(
                std::slice::from_ref(&scheduled),
                Vec::new(),
                vec![scheduled.clone()],
                vec![retained],
                Vec::new(),
            )
            .unwrap_err();
            assert!(rejected.to_string().contains("outside its archive sources"));
        }
        assert!(
            finish_purge(
                std::slice::from_ref(&scheduled),
                Vec::new(),
                vec![scheduled.clone()],
                Vec::new(),
                vec![archive_version("task", "data", "unknown")],
            )
            .unwrap_err()
            .to_string()
            .contains("unknown candidate")
        );
    }

    #[test]
    fn a_retained_physical_version_is_not_deleted_through_a_generic_alias() {
        let archive = archive_version("task", "data", "retained");
        let generic = ObjectVersion {
            pointer: "task/data.dvc".to_owned(),
            ..archive.clone()
        };
        let candidates = vec![archive.clone(), generic.clone()];
        let (state, report) = finish_purge(
            &candidates,
            Vec::new(),
            candidates.clone(),
            Vec::new(),
            vec![archive.clone()],
        )
        .unwrap();
        assert_eq!(report.status, "cleanup_pending");
        assert!(report.deleted.is_empty());
        assert_eq!(state.pending, [archive]);
    }

    #[test]
    fn generic_candidates_keep_verified_ancestor_unmapped_history_pending() {
        let generic = ObjectVersion {
            pointer: "2026/07/task/data.dvc".to_owned(),
            object: "2026/07/task/data".to_owned(),
            version_id: "mapped".to_owned(),
        };
        let unmapped = archive_version("2026/07/task", "retired", "late-marker");
        let (state, report) = finish_purge(
            std::slice::from_ref(&generic),
            Vec::new(),
            vec![generic.clone()],
            vec![unmapped.clone()],
            Vec::new(),
        )
        .unwrap();
        assert_eq!(report.status, "blocked_unmapped");
        assert_eq!(report.deleted, [generic]);
        assert_eq!(state.pending, [unmapped]);
    }

    #[test]
    fn published_receipts_queue_complete_history_without_private_journals_or_pointers() {
        let fixture = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: fixture.path().canonicalize().unwrap(),
        };
        repo.run(["init", "-q", "-b", "main"]).unwrap();
        repo.run(["config", "user.name", "Fresh archive consumer"])
            .unwrap();
        repo.run(["config", "user.email", "fixture@example.invalid"])
            .unwrap();
        let source = "20260712-121000-task";
        let destination = format!("2026/07/{source}");
        let path = format!("{destination}/{}", crate::archive_migration::RECEIPT_NAME);
        fs::create_dir_all(repo.root.join(&destination)).unwrap();
        let mut receipt = serde_json::json!({
            "schema_version":1,"task_id":source,"source":source,
            "destination":destination,"status":"copied",
            "bucket":"isolated-fixture","remote_prefix":"dvc",
            "versions":[
                {"source_object":format!("{source}/retired"),"source_version_id":"old-payload",
                 "destination_object":format!("{destination}/retired"),"destination_version_id":"new-payload","delete_marker":false},
                {"source_object":format!("{source}/retired"),"source_version_id":"old-marker",
                 "destination_object":format!("{destination}/retired"),"destination_version_id":"new-marker","delete_marker":true}
            ]
        });
        fs::write(repo.root.join(&path), receipt.to_string()).unwrap();
        let planned_source = "20260712-130000-planned";
        let planned_destination = format!("2026/07/{planned_source}");
        let planned_path = format!(
            "{planned_destination}/{}",
            crate::archive_migration::RECEIPT_NAME
        );
        fs::create_dir_all(repo.root.join(&planned_destination)).unwrap();
        fs::write(
            repo.root.join(planned_path),
            serde_json::json!({"schema_version":1,"task_id":planned_source,
                "source":planned_source,"destination":planned_destination,
                "status":"planned","versions":[]})
            .to_string(),
        )
        .unwrap();
        repo.run(["add", "."]).unwrap();
        repo.run(["commit", "-m", "Merge complete archive receipt"])
            .unwrap();
        let actual = archive_candidates_at(&repo, "HEAD", &[]).unwrap();
        assert_eq!(actual.len(), 2);
        assert!(actual.contains(&archive_version(source, "retired", "old-payload")));
        assert!(actual.contains(&archive_version(source, "retired", "old-marker")));
        assert_eq!(
            archive_candidates_at(&repo, "HEAD", std::slice::from_ref(&destination)).unwrap(),
            actual
        );
        assert!(
            archive_candidates_at(&repo, "HEAD", &[source.to_owned()])
                .unwrap()
                .is_empty()
        );
        assert!(pointers_at(&repo, "HEAD", &[]).unwrap().is_empty());
        assert!(!repo.local_state_dir().unwrap().join("archive").exists());

        // A generic retirement queue can predate the archive receipt. Expand
        // it from the verified published tree, including keys and markers that
        // no current storage manifest names, and keep the expanded state on disk.
        let generic = ObjectVersion {
            pointer: format!("{source}/retired.dvc"),
            object: format!("{source}/retired"),
            version_id: "old-payload".to_owned(),
        };
        let adjacent = ObjectVersion {
            pointer: format!("{source}-neighbor/data.dvc"),
            object: format!("{source}-neighbor/data"),
            version_id: "independent".to_owned(),
        };
        let published = archive_receipts_at(&repo, "HEAD", &[]).unwrap();
        let mut state = PurgeState {
            schema_version: STATE_SCHEMA,
            pending: vec![generic.clone(), adjacent.clone()],
            pending_prefixes: BTreeMap::new(),
        };
        assert!(promote_archive_prefixes(&mut state, &published).unwrap());
        assert!(actual.iter().all(|item| state.pending.contains(item)));
        assert!(state.pending.contains(&generic));
        assert!(state.pending.contains(&adjacent));
        assert_eq!(
            state.pending_prefixes,
            BTreeMap::from([(source.to_owned(), receipt.clone())])
        );
        write_state(&repo, &state).unwrap();
        assert_eq!(read_state(&repo).unwrap().pending, state.pending);
        assert_eq!(archive_prefixes(&repo).unwrap(), state.pending_prefixes);
        assert!(!promote_archive_prefixes(&mut state, &published).unwrap());
        let mut adjacent_only = PurgeState {
            schema_version: STATE_SCHEMA,
            pending: vec![adjacent],
            pending_prefixes: BTreeMap::new(),
        };
        assert!(!promote_archive_prefixes(&mut adjacent_only, &published).unwrap());
        assert!(adjacent_only.pending_prefixes.is_empty());

        receipt["versions"][0]["destination_object"] = "other-task/retired".into();
        fs::write(repo.root.join(&path), receipt.to_string()).unwrap();
        repo.run(["add", "."]).unwrap();
        repo.run(["commit", "-m", "Introduce invalid archive receipt"])
            .unwrap();
        assert!(
            archive_candidates_at(&repo, "HEAD", &[])
                .unwrap_err()
                .to_string()
                .contains("escapes its task prefixes")
        );
    }

    #[test]
    fn a_legacy_remote_tag_protects_source_history_without_a_task_manifest() {
        let fixture = tempfile::tempdir().unwrap();
        let remote = fixture.path().join("remote.git");
        let checkout = fixture.path().join("checkout");
        fs::create_dir(&checkout).unwrap();
        let repo = GitRepo { root: checkout };
        repo.run(["init", "-q", "--bare", remote.to_str().unwrap()])
            .unwrap();
        repo.run(["init", "-q", "-b", "main"]).unwrap();
        repo.run(["config", "user.name", "Legacy archive fixture"])
            .unwrap();
        repo.run(["config", "user.email", "fixture@example.invalid"])
            .unwrap();
        fs::create_dir(repo.root.join("legacy-task")).unwrap();
        fs::write(repo.root.join("legacy-task/README.md"), "legacy tree\n").unwrap();
        fs::write(repo.root.join("same-named-file"), "ordinary blob\n").unwrap();
        repo.run(["add", "."]).unwrap();
        repo.run(["commit", "-m", "Publish a task before manifest adoption"])
            .unwrap();
        repo.run(["tag", "legacy-before-adoption"]).unwrap();
        let control_blob = repo.run(["hash-object", "-w", "same-named-file"]).unwrap();
        repo.run([
            "update-ref",
            "refs/tags/workspace-mgr/archive-registry/fixture",
            control_blob.stdout.trim(),
        ])
        .unwrap();
        fs::create_dir_all(repo.root.join("2026/07")).unwrap();
        repo.run(["mv", "legacy-task", "2026/07/legacy-task"])
            .unwrap();
        repo.run(["commit", "-m", "Archive the legacy task directory"])
            .unwrap();
        repo.run(["remote", "add", "origin", remote.to_str().unwrap()])
            .unwrap();
        repo.run(["push", "origin", "main", "--tags"]).unwrap();
        let archive = archive_version("legacy-task", "retired", "historical");
        let generic = ObjectVersion {
            pointer: "legacy-task/retired.dvc".to_owned(),
            object: "legacy-task/retired".to_owned(),
            version_id: "stale-generic".to_owned(),
        };
        let same_named_file = archive_version("same-named-file", "data", "not-a-tree");
        let adjacent = archive_version("legacy-task-neighbor", "data", "neighbor");
        let candidates = vec![archive.clone(), generic.clone(), same_named_file, adjacent];
        let protected =
            referenced_objects(&repo, &Config::default(), "origin", &candidates).unwrap();
        assert_eq!(protected, [archive.clone(), generic.clone()]);
        assert!(
            repo.run_unchecked([
                "cat-file",
                "-e",
                "legacy-before-adoption:legacy-task/.workspace-mgr-task.toml"
            ])
            .unwrap()
            .code
                != 0
        );
        let receipt = serde_json::json!({
            "schema_version":1,"task_id":"legacy-task","source":"legacy-task",
            "destination":"2026/07/legacy-task","status":"copied",
            "versions":[{"source_object":"legacy-task/retired","source_version_id":"historical",
                "destination_object":"2026/07/legacy-task/retired","destination_version_id":"copied",
                "delete_marker":false}]
        });
        fs::write(
            repo.root
                .join("2026/07/legacy-task/.workspace-mgr-archive.json"),
            receipt.to_string(),
        )
        .unwrap();
        repo.run(["add", "."]).unwrap();
        repo.run([
            "commit",
            "-m",
            "Publish the canonical complete archive receipt",
        ])
        .unwrap();
        repo.run(["push", "origin", "main"]).unwrap();
        let after_merge =
            referenced_objects(&repo, &Config::default(), "origin", &candidates).unwrap();
        assert!(after_merge.is_empty());
        assert!(
            repo.run_unchecked(["show-ref", "--verify", "refs/tags/legacy-before-adoption"])
                .unwrap()
                .success()
        );
    }

    #[test]
    fn object_identity_includes_pointer_path_and_exact_version() {
        let first = ObjectVersion {
            pointer: "task/a.bin.dvc".to_owned(),
            object: "task/a.bin".to_owned(),
            version_id: "one".to_owned(),
        };
        let moved = ObjectVersion {
            pointer: "task/b.bin.dvc".to_owned(),
            object: "task/b.bin".to_owned(),
            version_id: "one".to_owned(),
        };
        assert_ne!(first, moved);
    }

    #[test]
    fn live_archived_source_protects_stale_generic_candidates_across_the_complete_prefix() {
        use std::process::Command;

        let fixture = tempfile::tempdir().unwrap();
        let remote = fixture.path().join("remote.git");
        let checkout = fixture.path().join("checkout");
        fs::create_dir(&checkout).unwrap();
        let git = |directory: &std::path::Path, args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(directory)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(
            fixture.path(),
            &["init", "--bare", remote.to_str().unwrap()],
        );
        git(&checkout, &["init", "-b", "main"]);
        git(
            &checkout,
            &["config", "user.name", "Archive protection fixture"],
        );
        git(
            &checkout,
            &["config", "user.email", "fixture@example.invalid"],
        );
        fs::create_dir(checkout.join("task")).unwrap();
        fs::write(
            checkout
                .join("task")
                .join(crate::policy::TASK_MANIFEST_NAME),
            "historical manifest\n",
        )
        .unwrap();
        git(&checkout, &["add", "."]);
        git(
            &checkout,
            &[
                "commit",
                "-m",
                "Keep source task, with retired pointer absent",
            ],
        );
        git(
            &checkout,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&checkout, &["push", "origin", "main"]);
        let repo = GitRepo::discover(&checkout).unwrap();
        let candidates = vec![
            ObjectVersion {
                pointer: "task/.workspace-mgr-archive.json".to_owned(),
                object: "task/history.bin".to_owned(),
                version_id: "mapped".to_owned(),
            },
            ObjectVersion {
                pointer: "task/retired.bin.dvc".to_owned(),
                object: "task/retired.bin".to_owned(),
                version_id: "stale-generic".to_owned(),
            },
            ObjectVersion {
                pointer: "task-neighbor/retired.bin.dvc".to_owned(),
                object: "task-neighbor/retired.bin".to_owned(),
                version_id: "neighbor".to_owned(),
            },
        ];
        let protected =
            referenced_objects(&repo, &Config::default(), "origin", &candidates).unwrap();
        assert_eq!(protected, candidates[..2]);
        let destructive = candidates
            .iter()
            .filter(|candidate| !protected.contains(candidate))
            .collect::<Vec<_>>();
        assert_eq!(destructive, [&candidates[2]]);
    }

    fn write_reference_directory(
        repo: &GitRepo,
        pointer: &str,
        output: &str,
        relpath: &str,
        version: &str,
    ) {
        let files = [storage_metadata::PointerFileVersion {
            relpath: relpath.to_owned(),
            md5: Some("900150983cd24fb0d6963f7d28e17f72".to_owned()),
            size: Some(3),
            version_id: Some(version.to_owned()),
            etag: Some("abc".to_owned()),
            verification: None,
        }];
        let path = repo.root.join(pointer);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        if pointer.ends_with(crate::storage_format::SUFFIX) {
            use crate::storage_format::{Checksum, Entry, Kind, Manifest, Version};
            let entries = vec![Entry {
                path: relpath.to_owned(),
                checksum: Checksum {
                    algorithm: "md5".to_owned(),
                    digest: files[0].md5.clone().unwrap(),
                },
                size: 3,
                version: Some(Version {
                    id: version.to_owned(),
                    etag: Some("abc".to_owned()),
                    verification: None,
                }),
            }];
            let manifest = Manifest {
                schema_version: 1,
                path: output.to_owned(),
                kind: Kind::Directory,
                checksum: Checksum {
                    algorithm: "md5".to_owned(),
                    digest: crate::storage_format::directory_digest(&entries).unwrap(),
                },
                size: 3,
                version: None,
                entries: Some(entries),
            };
            fs::write(path, manifest.serialize().unwrap()).unwrap();
            return;
        }
        let digest = crate::legacy_dvc::directory_digest(&files).unwrap();
        fs::write(
            path,
            format!(
                "outs:\n- path: {output}\n  hash: md5\n  md5: {digest}\n  size: 3\n  nfiles: 1\n  files:\n  - relpath: {relpath}\n    md5: 900150983cd24fb0d6963f7d28e17f72\n    size: 3\n    cloud:\n      workspace-mgr:\n        version_id: {version}\n        etag: abc\n"
            ),
        )
        .unwrap();
    }

    fn parent_reference_fixture(tag: bool) -> (tempfile::TempDir, GitRepo, String) {
        let directory = tempfile::tempdir().unwrap();
        let remote = directory.path().join("remote.git");
        let checkout = directory.path().join("checkout");
        fs::create_dir(&checkout).unwrap();
        let repo = GitRepo { root: checkout };
        repo.run(["init", "-q", "--bare", remote.to_str().unwrap()])
            .unwrap();
        repo.run(["init", "-q", "-b", "main"]).unwrap();
        repo.run(["config", "user.name", "Parent reference fixture"])
            .unwrap();
        repo.run(["config", "user.email", "fixture@example.invalid"])
            .unwrap();
        fs::write(repo.root.join("README.md"), "isolated reference fixture\n").unwrap();
        write_reference_directory(
            &repo,
            "task/data/child.wm-storage.json",
            "child",
            "a.bin",
            "old-version",
        );
        repo.run(["add", "."]).unwrap();
        repo.run(["commit", "-m", "Publish the original child pointer"])
            .unwrap();
        repo.run(["rm", "task/data/child.wm-storage.json"]).unwrap();
        repo.run(["commit", "-m", "Retire the child pointer before cleanup"])
            .unwrap();
        repo.run(["remote", "add", "origin", remote.to_str().unwrap()])
            .unwrap();
        repo.run(["push", "origin", "main"]).unwrap();

        // Only a new parent pointer names the same physical object, now at a
        // different version. The original candidate pointer is absent.
        write_reference_directory(
            &repo,
            "task/data.wm-storage.json",
            "data",
            "child/a.bin",
            "new-version",
        );
        repo.run(["add", "task/data.wm-storage.json"]).unwrap();
        repo.run(["commit", "-m", "Repack the object under a parent pointer"])
            .unwrap();
        let reference = if tag {
            repo.run(["tag", "-a", "parent-inner", "-m", "inner"])
                .unwrap();
            repo.run(["tag", "-a", "parent-live", "-m", "outer", "parent-inner"])
                .unwrap();
            repo.run(["push", "origin", "refs/tags/parent-live"])
                .unwrap();
            "refs/tags/parent-live"
        } else {
            repo.run(["push", "origin", "HEAD:refs/heads/parent-live"])
                .unwrap();
            "refs/heads/parent-live"
        };
        // Registry coordination tags can point directly to blobs, not trees.
        let blob = repo.run(["hash-object", "-w", "README.md"]).unwrap();
        repo.run([
            "update-ref",
            "refs/tags/workspace-mgr/archive-registry/fixture",
            blob.stdout.trim(),
        ])
        .unwrap();
        repo.run([
            "push",
            "origin",
            "refs/tags/workspace-mgr/archive-registry/fixture",
        ])
        .unwrap();
        (directory, repo, reference.to_owned())
    }

    fn retired_child_version() -> ObjectVersion {
        ObjectVersion {
            pointer: "task/data/child.wm-storage.json".to_owned(),
            object: "task/data/child/a.bin".to_owned(),
            version_id: "old-version".to_owned(),
        }
    }

    fn check_live_parent_reference(tag: bool) {
        let (_directory, repo, reference) = parent_reference_fixture(tag);
        check_parent_object_protection(&repo, &reference);
    }

    fn check_parent_object_protection(repo: &GitRepo, reference: &str) {
        let old = retired_child_version();
        let neighbor = ObjectVersion {
            object: format!("{}-neighbor", old.object),
            ..old.clone()
        };
        let candidates = [old.clone(), neighbor];
        let protected =
            referenced_objects(repo, &Config::default(), "origin", &candidates).unwrap();
        assert_eq!(protected, [old]);

        // Local pointers and ancestor commits are not live remote references.
        // Removing the only live parent releases both objects for retirement.
        repo.run(["push", "origin", &format!(":{reference}")])
            .unwrap();
        assert!(
            referenced_objects(repo, &Config::default(), "origin", &candidates)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn live_parent_pointer_on_remote_branch_protects_old_child_versions() {
        check_live_parent_reference(false);
    }

    #[test]
    fn live_parent_pointer_on_remote_tag_protects_old_child_versions() {
        check_live_parent_reference(true);
    }

    #[test]
    fn differently_named_live_pointer_protects_its_actual_object() {
        let (_directory, repo, reference) = parent_reference_fixture(false);
        repo.run(["rm", "task/data.wm-storage.json"]).unwrap();
        // The metadata filename is not an ancestor of the retired object, and
        // its output path differs from its own name. Protection must follow the
        // parsed object reference rather than guess possible parent filenames.
        write_reference_directory(
            &repo,
            "renamed-manifest.dvc",
            "task/data",
            "child/a.bin",
            "new-version",
        );
        repo.run(["add", "renamed-manifest.dvc"]).unwrap();
        repo.run([
            "commit",
            "-m",
            "Publish an independently named reference manifest",
        ])
        .unwrap();
        repo.run(["push", "origin", &format!("HEAD:{reference}")])
            .unwrap();
        check_parent_object_protection(&repo, &reference);
    }

    #[test]
    fn parent_pointer_discovery_keeps_archive_retirement_exact_to_the_version() {
        let (_directory, repo, _reference) = parent_reference_fixture(false);
        let path = "2026/07/task/.workspace-mgr-archive.json";
        fs::create_dir_all(repo.root.join("2026/07/task")).unwrap();
        let receipt = serde_json::json!({
            "schema_version":1,"task_id":"task","source":"task",
            "destination":"2026/07/task","status":"copied",
            "versions":[{
                "source_object":"task/data/child/a.bin","source_version_id":"old-version",
                "destination_object":"2026/07/task/data/child/a.bin",
                "destination_version_id":"copied-version","delete_marker":false
            }]
        });
        fs::write(repo.root.join(path), receipt.to_string()).unwrap();
        repo.run(["add", path]).unwrap();
        repo.run(["commit", "-m", "Publish the complete archive mapping"])
            .unwrap();
        repo.run(["push", "origin", "main"]).unwrap();
        let mapped = archive_version("task", "data/child/a.bin", "old-version");
        let live = archive_version("task", "data/child/a.bin", "new-version");
        let unreferenced = archive_version("task", "data/child/a.bin", "unreferenced-version");
        let protected = referenced_objects(
            &repo,
            &Config::default(),
            "origin",
            &[mapped, live.clone(), unreferenced],
        )
        .unwrap();
        // Published mapped history can retire, while an unmapped generation
        // requires an exact live reference rather than object-wide protection.
        assert_eq!(protected, [live]);
    }

    #[test]
    fn unrelated_live_pointer_without_an_exact_version_blocks_retirement() {
        let (_directory, repo, _reference) = parent_reference_fixture(false);
        fs::create_dir_all(repo.root.join("unrelated")).unwrap();
        fs::write(
            repo.root.join("unrelated/data.dvc"),
            "outs:\n- path: data\n  hash: md5\n  md5: 900150983cd24fb0d6963f7d28e17f72\n  size: 3\n",
        )
        .unwrap();
        repo.run(["add", "unrelated/data.dvc"]).unwrap();
        repo.run([
            "commit",
            "-m",
            "Publish incomplete unrelated version metadata",
        ])
        .unwrap();
        repo.run(["push", "origin", "HEAD:refs/heads/parent-live"])
            .unwrap();
        let error = referenced_objects(
            &repo,
            &Config::default(),
            "origin",
            &[retired_child_version()],
        )
        .unwrap_err();
        assert!(error.to_string().contains("no exact version ID"));
    }

    #[test]
    fn purge_pending_never_deletes_versions_referenced_by_a_new_parent_pointer() {
        use crate::native_s3::tests::{Reply, configure_repo, routed_fixture};

        let (_directory, repo, _reference) = parent_reference_fixture(false);
        let old = retired_child_version();
        let (client, worker) = routed_fixture(|request| {
            assert_eq!(request.method, "GET");
            assert_eq!(request.target, "/fixture-bucket?versioning=");
            Reply::xml(
                "<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>",
            )
        });
        configure_repo(&client, &repo);
        let endpoint_url = Config::load(&repo).unwrap().s3.unwrap().endpoint_url;
        let config = Config {
            s3: Some(crate::config::S3Config {
                url: "s3://fixture-bucket/root".to_owned(),
                endpoint_url,
            }),
            ..Config::default()
        };
        fs::write(Config::path(&repo), config.render().unwrap()).unwrap();
        queue(&repo, std::slice::from_ref(&old)).unwrap();
        let report = purge_pending(&repo, &config, "origin").unwrap();
        assert_eq!(report.status, "cleanup_pending");
        assert_eq!(report.protected.as_slice(), std::slice::from_ref(&old));
        assert_eq!(report.pending, [old]);
        assert!(report.deleted.is_empty());
        let requests = worker.finish_requests();
        assert!(!requests.is_empty());
        assert!(requests.iter().all(|request| request.method != "DELETE"));
    }

    fn retained_destination_reference_fixture(
        archived_again: bool,
        tampered_registry: bool,
        missing_binding: bool,
    ) -> (
        tempfile::TempDir,
        GitRepo,
        serde_json::Value,
        crate::native_s3::tests::RoutedFixture,
    ) {
        use crate::native_s3::tests::{Reply, configure_repo, routed_fixture};

        const ORIGINAL: &str = "20261008-120000-original";
        const RENAMED: &str = "20261008-120000-renamed";
        let directory = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: directory.path().join("checkout"),
        };
        fs::create_dir(&repo.root).unwrap();
        let remote = directory.path().join("remote.git");
        repo.run(["init", "-q", "-b", "main"]).unwrap();
        repo.run(["config", "user.name", "Retained history fixture"])
            .unwrap();
        repo.run(["config", "user.email", "fixture@example.invalid"])
            .unwrap();
        repo.run(["init", "-q", "--bare", remote.to_str().unwrap()])
            .unwrap();
        repo.run(["remote", "add", "origin", remote.to_str().unwrap()])
            .unwrap();
        let renamed = serde_json::json!({
            "schema_version":1,"task_id":ORIGINAL,"migration_kind":"task-rename",
            "source":ORIGINAL,"destination":RENAMED,"status":"copied",
            "remote":"workspace-mgr","bucket":"fixture-bucket","remote_prefix":"root",
            "transaction_id":"rename-fixture","versions":[{
                "source_object":format!("{ORIGINAL}/data/a.bin"),"source_version_id":"original-version",
                "destination_object":format!("{RENAMED}/data/a.bin"),"destination_version_id":"rename-copy",
                "delete_marker":false,"size":3,"source_etag":"abc","destination_etag":"copied"
            },{
                "source_object":format!("{ORIGINAL}/retired"),"source_version_id":"original-marker",
                "destination_object":format!("{RENAMED}/retired"),"destination_version_id":"rename-marker",
                "delete_marker":true
            }]
        });
        let receipt = if archived_again {
            let destination = format!("2026/10/{RENAMED}");
            serde_json::json!({
                "schema_version":1,"task_id":ORIGINAL,"source":RENAMED,"destination":destination,
                "status":"copied","remote":"workspace-mgr","bucket":"fixture-bucket","remote_prefix":"root",
                "transaction_id":"archive-fixture","previous_receipt":renamed,
                "versions":[{
                    "source_object":format!("{RENAMED}/data/a.bin"),"source_version_id":"rename-copy",
                    "destination_object":format!("{destination}/data/a.bin"),"destination_version_id":"archive-copy",
                    "delete_marker":false,"size":3,"source_etag":"copied","destination_etag":"archived"
                }]
            })
        } else {
            renamed
        };
        let mut canonical = receipt.clone();
        if tampered_registry {
            canonical["transaction_id"] = "unrelated-transaction".into();
        }
        let registry_key = format!(
            "root/.workspace-mgr/archive/{}.json",
            encode_lower(Sha256::digest(
                receipt["source"].as_str().unwrap().as_bytes()
            ))
        );
        let (client, worker) = routed_fixture(move |request| {
            assert_eq!(request.method, "GET");
            let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
            let query = url.query_pairs().collect::<BTreeMap<_, _>>();
            if query.contains_key("versioning") {
                return Reply::xml(
                    "<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>",
                );
            }
            if query.contains_key("versions") {
                assert_eq!(query["prefix"], registry_key);
                return Reply::xml(&format!(
                    "<ListVersionsResult><IsTruncated>false</IsTruncated><Version><Key>{registry_key}</Key><VersionId>registry-version</VersionId><IsLatest>true</IsLatest><LastModified>2026-10-08T20:00:00Z</LastModified><ETag>registry-etag</ETag><Size>1</Size></Version></ListVersionsResult>"
                ));
            }
            assert_eq!(query["versionId"], "registry-version");
            assert_eq!(url.path(), format!("/fixture-bucket/{registry_key}"));
            Reply {
                status: 200,
                headers: vec![("x-amz-version-id", "registry-version".into())],
                body: serde_json::to_vec(&canonical).unwrap(),
            }
        });
        configure_repo(&client, &repo);
        fs::write(repo.root.join(".gitignore"), "/.workspace-mgr/local/\n").unwrap();
        let destination = receipt["destination"].as_str().unwrap();
        let path = format!("{destination}/{}", crate::archive_migration::RECEIPT_NAME);
        fs::create_dir_all(repo.root.join(destination)).unwrap();
        fs::write(repo.root.join(&path), receipt.to_string()).unwrap();
        write_reference_directory(
            &repo,
            &format!("{destination}/data.wm-storage.json"),
            "data",
            "a.bin",
            receipt["versions"][0]["destination_version_id"]
                .as_str()
                .unwrap(),
        );
        repo.run(["add", "."]).unwrap();
        repo.run(["commit", "-q", "-m", "Publish copied task history"])
            .unwrap();
        repo.run(["push", "-q", "origin", "main"]).unwrap();
        if !missing_binding {
            crate::archive_registry::coordinate_published(&repo, &receipt).unwrap();
        }
        repo.run(["rm", &format!("{destination}/data.wm-storage.json")])
            .unwrap();
        repo.run([
            "commit",
            "-q",
            "-m",
            "Remove the final live pointer in a later publication",
        ])
        .unwrap();
        repo.run(["push", "-q", "origin", "main"]).unwrap();
        (directory, repo, receipt, worker)
    }

    #[test]
    fn retained_copied_destination_is_protected_after_later_publication_removes_its_pointer() {
        let (_directory, repo, receipt, worker) =
            retained_destination_reference_fixture(false, false, false);
        let candidate = |row: usize| ObjectVersion {
            pointer: format!(
                "{}.wm-storage.json",
                receipt["versions"][row]["destination_object"]
                    .as_str()
                    .unwrap()
            ),
            object: receipt["versions"][row]["destination_object"]
                .as_str()
                .unwrap()
                .to_owned(),
            version_id: receipt["versions"][row]["destination_version_id"]
                .as_str()
                .unwrap()
                .to_owned(),
        };
        let copied = candidate(0);
        let marker = candidate(1);
        let later = ObjectVersion {
            version_id: "later-unmapped-generation".to_owned(),
            ..copied.clone()
        };
        let adjacent = ObjectVersion {
            object: format!("{}-adjacent", copied.object),
            ..copied.clone()
        };
        let config = Config::load(&repo).unwrap();
        let protected = referenced_objects(
            &repo,
            &config,
            "origin",
            &[copied.clone(), marker.clone(), later, adjacent],
        )
        .unwrap();
        assert_eq!(protected, [copied, marker]);
        assert!(!worker.finish_requests().is_empty());
    }

    #[test]
    fn retained_destination_protection_ignores_relocated_previous_receipt_versions() {
        let (_directory, repo, receipt, worker) =
            retained_destination_reference_fixture(true, false, false);
        let copied = ObjectVersion {
            pointer: "retired.wm-storage.json".to_owned(),
            object: receipt["versions"][0]["destination_object"]
                .as_str()
                .unwrap()
                .to_owned(),
            version_id: "archive-copy".to_owned(),
        };
        let intermediate = ObjectVersion {
            object: receipt["previous_receipt"]["versions"][0]["destination_object"]
                .as_str()
                .unwrap()
                .to_owned(),
            version_id: "rename-copy".to_owned(),
            ..copied.clone()
        };
        let protected = referenced_objects(
            &repo,
            &Config::load(&repo).unwrap(),
            "origin",
            &[copied.clone(), intermediate],
        )
        .unwrap();
        assert_eq!(protected, [copied]);
        assert!(!worker.finish_requests().is_empty());
    }

    #[test]
    fn retained_destination_retirement_fails_closed_on_registry_or_git_binding_changes() {
        for (tampered_registry, missing_binding) in [(true, false), (false, true)] {
            let (_directory, repo, receipt, worker) =
                retained_destination_reference_fixture(false, tampered_registry, missing_binding);
            let candidate = ObjectVersion {
                pointer: "retired.wm-storage.json".to_owned(),
                object: receipt["versions"][0]["destination_object"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
                version_id: "rename-copy".to_owned(),
            };
            let error =
                referenced_objects(&repo, &Config::load(&repo).unwrap(), "origin", &[candidate])
                    .unwrap_err()
                    .to_string();
            assert!(
                error.contains("canonical registry")
                    || error.contains("canonical Git coordination binding"),
                "{error}"
            );
            assert!(
                worker
                    .finish_requests()
                    .iter()
                    .all(|request| request.method == "GET")
            );
        }
    }

    #[test]
    fn canonical_binding_protects_copies_before_merge_and_after_receipt_file_removal() {
        let (_directory, repo, receipt, worker) =
            retained_destination_reference_fixture(false, false, false);
        // The only surviving control is the immutable canonical binding. A
        // protective read must not require a shared-branch receipt or private
        // copy journal, nor recreate either of them.
        let path = format!(
            "{}/{}",
            receipt["destination"].as_str().unwrap(),
            crate::archive_migration::RECEIPT_NAME
        );
        repo.run(["rm", &path]).unwrap();
        repo.run(["commit", "-q", "-m", "Remove the task publication receipt"])
            .unwrap();
        repo.run(["push", "-q", "origin", "main"]).unwrap();
        assert!(archive_receipts_at(&repo, "HEAD", &[]).unwrap().is_empty());
        let object = receipt["versions"][0]["destination_object"]
            .as_str()
            .unwrap()
            .to_owned();
        let retained =
            retained_archive_destinations(&repo, &[receipt], &BTreeSet::from([object.clone()]))
                .unwrap();
        assert_eq!(
            retained,
            BTreeSet::from([(object, "rename-copy".to_owned())])
        );
        assert!(
            worker
                .finish_requests()
                .iter()
                .all(|request| request.method == "GET")
        );
    }

    #[test]
    fn unpublished_copied_receipt_defers_source_cleanup_without_any_live_source_pointer() {
        let (_directory, repo, receipt, worker) =
            retained_destination_reference_fixture(false, false, false);
        let destination = receipt["destination"].as_str().unwrap();
        let path = format!("{destination}/{}", crate::archive_migration::RECEIPT_NAME);
        repo.run(["rm", &path]).unwrap();
        repo.run([
            "commit",
            "-q",
            "-m",
            "shared branch does not yet contain copied receipt",
        ])
        .unwrap();
        repo.run(["push", "-q", "origin", "main"]).unwrap();
        let source = receipt["source"].as_str().unwrap();
        let source_object = receipt["versions"][0]["source_object"].as_str().unwrap();
        let original = ObjectVersion {
            pointer: format!("{source_object}.wm-storage.json"),
            object: source_object.to_owned(),
            version_id: "original-version".to_owned(),
        };
        let mut alias = original.clone();
        alias.pointer = format!("{source}/{}", crate::archive_migration::RECEIPT_NAME);
        let marker = ObjectVersion {
            pointer: format!("{source}/retired.wm-storage.json"),
            object: format!("{source}/retired"),
            version_id: "original-marker".to_owned(),
        };
        let late = ObjectVersion {
            pointer: format!("{source}/late.wm-storage.json"),
            object: format!("{source}/late"),
            version_id: "later-unmapped-source".to_owned(),
        };
        let mut candidates = vec![original, alias, marker, late];
        candidates.sort();
        queue(&repo, &candidates).unwrap();
        queue_archive_prefixes(&repo, std::slice::from_ref(&receipt)).unwrap();
        let report = purge_pending(&repo, &Config::load(&repo).unwrap(), "origin").unwrap();
        assert_eq!(report.status, "cleanup_pending");
        assert_eq!(report.pending, candidates);
        assert_eq!(report.protected, candidates);
        assert!(report.deleted.is_empty());
        assert_eq!(report.pending_prefixes, [source]);
        assert_eq!(read_state(&repo).unwrap().pending, candidates);
        assert_eq!(archive_prefixes(&repo).unwrap().get(source), Some(&receipt));
        // Once the same exact receipt is shared, this extra deferral ends;
        // destructive authorization remains the native source proof's job.
        assert!(
            deferred_archive_source_candidates(
                &repo,
                &candidates,
                &BTreeMap::from([(source.to_owned(), receipt.clone())]),
                &[receipt]
            )
            .unwrap()
            .is_empty()
        );
        let requests = worker.finish_requests();
        assert!(!requests.is_empty());
        assert!(requests.iter().all(|request| request.method == "GET"));
    }

    #[test]
    fn unpublished_source_deferral_fails_closed_on_canonical_registry_or_binding_changes() {
        for (tampered_registry, missing_binding) in [(true, false), (false, true)] {
            let (_directory, repo, receipt, worker) =
                retained_destination_reference_fixture(false, tampered_registry, missing_binding);
            let source = receipt["source"].as_str().unwrap();
            let candidate = ObjectVersion {
                pointer: format!("{source}/retired.dvc"),
                object: format!("{source}/retired"),
                version_id: "original-marker".to_owned(),
            };
            let error = deferred_archive_source_candidates(
                &repo,
                &[candidate],
                &BTreeMap::from([(source.to_owned(), receipt)]),
                &[],
            )
            .unwrap_err()
            .to_string();
            assert!(
                error.contains("canonical registry")
                    || error.contains("canonical Git coordination binding"),
                "{error}"
            );
            assert!(
                worker
                    .finish_requests()
                    .iter()
                    .all(|request| request.method == "GET")
            );
        }
    }
}
