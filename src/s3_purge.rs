use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::dvc;
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::hex::encode_lower;

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
}

impl PurgeReport {
    pub fn warning(&self) -> Option<(&'static str, String)> {
        if self.status == "blocked_unmapped" {
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
    // keys and markers that current DVC pointers cannot enumerate.
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
    objects_at(repo, config, None, &dvc::discover(repo, scopes)?)
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
        if let Some(previous) = state.pending_prefixes.get(&source) {
            if previous != receipt {
                return Err(Error::message(format!(
                    "pending archive prefix {source:?} has a different immutable receipt"
                )));
            }
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

pub fn has_pending(repo: &GitRepo) -> Result<bool> {
    let state = read_state(repo)?;
    Ok(!state.pending.is_empty() || !state.pending_prefixes.is_empty())
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
    dvc::ensure_ready(repo, config)?;
    dvc::verify_object_versioning(repo, config)?;
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
    let submitted_prefixes = prefixes
        .iter()
        .map(receipt_prefix)
        .collect::<Result<BTreeSet<_>>>()?;
    let protected_objects = referenced_objects(repo, config, remote, &state.pending)?;
    let protected_set = protected_objects.iter().cloned().collect::<BTreeSet<_>>();
    let deleted = state
        .pending
        .iter()
        .filter(|candidate| !protected_set.contains(*candidate))
        .cloned()
        .collect::<Vec<_>>();
    let mut retained_unmapped = Vec::new();
    let mut retained_mapped: Vec<ObjectVersion> = Vec::new();
    let mut cleaned_prefixes = Vec::new();
    if !deleted.is_empty() || !prefixes.is_empty() {
        let payload = serde_json::json!({"candidates":deleted,"prefixes":prefixes});
        let response = dvc::version_purge_adapter(repo, "delete", &payload)?;
        if let Some(retained) = response.get("retained_unmapped") {
            retained_unmapped = serde_json::from_value(retained.clone()).map_err(|error| {
                Error::message(format!("invalid unmapped archive version report: {error}"))
            })?;
        }
        if let Some(retained) = response.get("retained_mapped") {
            retained_mapped = serde_json::from_value(retained.clone()).map_err(|error| {
                Error::message(format!("invalid retained archive version report: {error}"))
            })?;
        }
        if let Some(cleaned) = response.get("cleaned_prefixes") {
            cleaned_prefixes = serde_json::from_value(cleaned.clone()).map_err(|error| {
                Error::message(format!("invalid cleaned archive prefix report: {error}"))
            })?;
        }
    }
    let (next, report) = finish_purge_with_prefixes(
        &state.pending,
        protected_objects,
        deleted,
        retained_unmapped,
        retained_mapped,
        PrefixCompletion {
            pending: state.pending_prefixes,
            submitted: submitted_prefixes,
            cleaned: cleaned_prefixes,
        },
    )?;
    write_state(repo, &next)?;
    Ok(report)
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
        if let Some(previous) = state.pending_prefixes.get(&source) {
            if previous != receipt {
                return Err(Error::message(
                    "pending source retirement differs from its published archive receipt",
                ));
            }
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
    if retained_mapped.iter().any(|item| !deleted.contains(item)) {
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
    dvc::ensure_ready(repo, config)?;
    let payload = serde_json::json!([{
        "revision": revision,
        "pointers": pointers,
    }]);
    let value = dvc::version_purge_adapter(repo, "list", &payload)?;
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
        .filter(|path| path.ends_with(".dvc"))
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
    for path in paths_at(repo, revision, scopes)? {
        if !path.ends_with(&format!("/{}", crate::archive_migration::RECEIPT_NAME)) {
            continue;
        }
        let raw = repo.run(["show", &format!("{revision}:{path}")])?;
        let receipt: serde_json::Value = serde_json::from_str(&raw.stdout).map_err(|error| {
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
    let candidate_pointers = candidates
        .iter()
        .map(|candidate| candidate.pointer.clone())
        .collect::<BTreeSet<_>>();
    let mut requests = Vec::new();
    let mut archive_protected = BTreeSet::new();
    let mut live_trees = BTreeSet::new();
    for revision in revisions {
        // Coordination tags can name blobs; commit and tree references can
        // contain live pointers. Peel nested annotated tags recursively.
        let kind = repo.run(["cat-file", "-t", &format!("{revision}^{{}}")])?;
        if kind.stdout.trim() == "blob" {
            continue;
        }
        let tree = repo.run(["rev-parse", "--verify", &format!("{revision}^{{tree}}")])?;
        let revision = tree.stdout.trim().to_owned();
        if !live_trees.insert(revision.clone()) {
            continue;
        }
        // A pending child pointer can disappear while a newly published
        // parent or renamed pointer still references the same object. Generic
        // retirement deletes its entire history, so collect references by
        // actual object identity through every live pointer, not its old name.
        let mut pointers = pointers_at(repo, &revision, &[])?;
        for pointer in &candidate_pointers {
            if let Some(source) =
                pointer.strip_suffix(&format!("/{}", crate::archive_migration::RECEIPT_NAME))
            {
                // An archive retires the entire prefix, including historical
                // files no current pointer names. Keep that complete snapshot
                // while any live branch or tag still contains the source task.
                // Pre-adoption tags can contain the task directory without a
                // manifest. Its tree is still a live reference to that logical
                // path; a same-named blob or coordination tag is not.
                let source_tree =
                    repo.run_unchecked(["cat-file", "-t", &format!("{revision}:{source}")])?;
                if source_tree.success()
                    && source_tree.stdout.trim() == "tree"
                    && !published_archive_sources.contains(source)
                {
                    archive_protected.insert(format!("{source}/"));
                }
                continue;
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
        let value = dvc::version_purge_adapter(repo, "list", &serde_json::Value::Array(requests))?;
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
            if published_archive_versions.contains(&identity) {
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
    let path = state_path(repo)?;
    if !path.is_file() {
        return Ok(PurgeState {
            schema_version: STATE_SCHEMA,
            pending: Vec::new(),
            pending_prefixes: BTreeMap::new(),
        });
    }
    let raw = fs::read_to_string(&path).at(&path)?;
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
        // no current DVC pointer names, and keep the expanded state on disk.
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
        let files = [dvc::PointerFileVersion {
            relpath: relpath.to_owned(),
            md5: Some("900150983cd24fb0d6963f7d28e17f72".to_owned()),
            size: Some(3),
            version_id: Some(version.to_owned()),
            etag: Some("abc".to_owned()),
        }];
        let digest = crate::native_engine::directory_digest(&files).unwrap();
        let path = repo.root.join(pointer);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
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
            "task/data/child.dvc",
            "child",
            "a.bin",
            "old-version",
        );
        repo.run(["add", "."]).unwrap();
        repo.run(["commit", "-m", "Publish the original child pointer"])
            .unwrap();
        repo.run(["rm", "task/data/child.dvc"]).unwrap();
        repo.run(["commit", "-m", "Retire the child pointer before cleanup"])
            .unwrap();
        repo.run(["remote", "add", "origin", remote.to_str().unwrap()])
            .unwrap();
        repo.run(["push", "origin", "main"]).unwrap();

        // Only a new parent pointer names the same physical object, now at a
        // different version. The original candidate pointer is absent.
        write_reference_directory(&repo, "task/data.dvc", "data", "child/a.bin", "new-version");
        repo.run(["add", "task/data.dvc"]).unwrap();
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
            pointer: "task/data/child.dvc".to_owned(),
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
        repo.run(["rm", "task/data.dvc"]).unwrap();
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
        use crate::native_s3::tests::{Reply, configure_repo, fixture};

        let (_directory, repo, _reference) = parent_reference_fixture(false);
        let old = retired_child_version();
        let (client, worker) = fixture(vec![Reply::xml(
            "<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>",
        )]);
        configure_repo(&client, &repo);
        let (_, endpoint_url) = dvc::internal_location(&repo).unwrap().unwrap();
        let config = Config {
            s3: Some(crate::config::S3Config {
                url: "s3://fixture-bucket/root".to_owned(),
                endpoint_url,
            }),
            ..Config::default()
        };
        dvc::write_internal_config(&repo, &config).unwrap();
        queue(&repo, std::slice::from_ref(&old)).unwrap();
        let report = purge_pending(&repo, &config, "origin").unwrap();
        assert_eq!(report.status, "cleanup_pending");
        assert_eq!(report.protected, [old.clone()]);
        assert_eq!(report.pending, [old]);
        assert!(report.deleted.is_empty());
        let requests = worker.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert!(requests[0].target.contains("versioning="));
        assert!(requests.iter().all(|request| request.method != "DELETE"));
    }
}
