use std::collections::BTreeSet;
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

const STATE_SCHEMA: u32 = 1;
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
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PurgeReport {
    pub status: String,
    pub queued: Vec<ObjectVersion>,
    pub deleted: Vec<ObjectVersion>,
    pub protected: Vec<ObjectVersion>,
    pub pending: Vec<ObjectVersion>,
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

pub fn preview(repo: &GitRepo) -> Result<PurgeReport> {
    let state = read_state(repo)?;
    Ok(PurgeReport {
        status: if state.pending.is_empty() {
            "no_changes"
        } else {
            "pending"
        }
        .to_owned(),
        pending: state.pending,
        ..PurgeReport::default()
    })
}

pub fn has_pending(repo: &GitRepo) -> Result<bool> {
    Ok(!read_state(repo)?.pending.is_empty())
}

pub fn purge_pending(repo: &GitRepo, config: &Config, remote: &str) -> Result<PurgeReport> {
    let state = read_state(repo)?;
    if state.pending.is_empty() {
        return Ok(PurgeReport {
            status: "no_changes".to_owned(),
            ..PurgeReport::default()
        });
    }
    dvc::ensure_ready(repo, config)?;
    dvc::verify_object_versioning(repo, config)?;
    let protected_objects = referenced_objects(repo, config, remote, &state.pending)?;
    let protected_set = protected_objects.iter().cloned().collect::<BTreeSet<_>>();
    let deleted = state
        .pending
        .iter()
        .filter(|candidate| !protected_set.contains(*candidate))
        .cloned()
        .collect::<Vec<_>>();
    if !deleted.is_empty() {
        let payload = serde_json::to_value(&deleted).map_err(|error| {
            Error::message(format!("failed to encode S3 purge candidates: {error}"))
        })?;
        dvc::version_purge_adapter(repo, "delete", &payload)?;
    }
    let next = PurgeState {
        schema_version: STATE_SCHEMA,
        pending: protected_objects.clone(),
    };
    write_state(repo, &next)?;
    Ok(PurgeReport {
        status: if !deleted.is_empty() {
            "deleted"
        } else {
            "protected"
        }
        .to_owned(),
        queued: Vec::new(),
        deleted,
        protected: protected_objects.clone(),
        pending: protected_objects,
    })
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

fn pointers_at(repo: &GitRepo, revision: &str, scopes: &[String]) -> Result<Vec<String>> {
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
        args.extend(scopes.iter().cloned());
    }
    let mut pointers = repo
        .run(args)?
        .stdout
        .split('\0')
        .filter(|path| path.ends_with(".dvc"))
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    pointers.sort();
    pointers.dedup();
    Ok(pointers)
}

fn referenced_objects(
    repo: &GitRepo,
    _config: &Config,
    remote: &str,
    candidates: &[ObjectVersion],
) -> Result<Vec<ObjectVersion>> {
    let revisions = fetch_current_remote_tips(repo, remote)?;
    let candidate_pointers = candidates
        .iter()
        .map(|candidate| candidate.pointer.clone())
        .collect::<BTreeSet<_>>();
    let mut requests = Vec::new();
    for revision in revisions {
        let mut pointers = Vec::new();
        for pointer in &candidate_pointers {
            if repo
                .run_unchecked(["cat-file", "-e", &format!("{revision}:{pointer}")])?
                .success()
            {
                pointers.push(pointer.clone());
            }
        }
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
    let referenced = referenced
        .into_iter()
        .map(|object| object.object)
        .collect::<BTreeSet<_>>();
    Ok(candidates
        .iter()
        .filter(|candidate| referenced.contains(&candidate.object))
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
    Ok(repo.common_dir()?.join("workspace-mgr").join(STATE_NAME))
}

fn read_state(repo: &GitRepo) -> Result<PurgeState> {
    let path = state_path(repo)?;
    if !path.is_file() {
        return Ok(PurgeState {
            schema_version: STATE_SCHEMA,
            pending: Vec::new(),
        });
    }
    let raw = fs::read_to_string(&path).at(&path)?;
    let mut state: PurgeState = serde_json::from_str(&raw)
        .map_err(|error| Error::message(format!("invalid private S3 purge state: {error}")))?;
    if state.schema_version != STATE_SCHEMA {
        return Err(Error::message(
            "private S3 purge state has an unsupported schema",
        ));
    }
    state.pending.sort();
    state.pending.dedup();
    Ok(state)
}

fn write_state(repo: &GitRepo, state: &PurgeState) -> Result<()> {
    let path = state_path(repo)?;
    if state.pending.is_empty() {
        if path.is_file() {
            fs::remove_file(&path).at(&path)?;
        }
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| Error::message("private S3 purge state has no parent"))?;
    fs::create_dir_all(parent).at(parent)?;
    let encoded = serde_json::to_vec_pretty(state)
        .map_err(|error| Error::message(format!("failed to encode S3 purge state: {error}")))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).at(parent)?;
    temporary.write_all(&encoded).at(&path)?;
    temporary.write_all(b"\n").at(&path)?;
    temporary.flush().at(&path)?;
    temporary.persist(&path).map_err(|error| Error::Io {
        path,
        source: error.error,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
