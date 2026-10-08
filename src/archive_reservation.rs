//! Reserve an archive source before creating any remote copies.
//!
//! A non-expiring Git compare-and-create tag binds the frozen source snapshot
//! and the owning checkout. A competing clone fails before its first S3 write.
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::archive_git_control::{ControlOids, object_ids};
use crate::config::Config;
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::hex::encode_lower;
use crate::path::{reject_symlink_traversal, repo_path};

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::message(format!("archive reservation has no {key}")))
}

pub(crate) fn normalized_receipt(receipt: &Value) -> Result<Value> {
    let mut planned = receipt.clone();
    if !matches!(planned["schema_version"].as_u64(), Some(1 | 2)) {
        return Err(Error::message(
            "archive reservation receipt has an unsupported schema",
        ));
    }
    planned["schema_version"] = 1.into();
    let object = planned
        .as_object_mut()
        .ok_or_else(|| Error::message("invalid archive reservation receipt"))?;
    object.remove("transaction_id");
    object.insert("status".to_owned(), "planned".into());
    let rows = object
        .get_mut("versions")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| Error::message("archive reservation has no complete source inventory"))?;
    for row in rows {
        let row = row
            .as_object_mut()
            .ok_or_else(|| Error::message("invalid archive reservation source version"))?;
        for field in [
            "destination_version_id",
            "destination_etag",
            "destination_last_modified",
            "started",
            "multipart_upload_id",
            "cancel_started",
            "cancel_deleted",
            "cancel_owned_versions",
        ] {
            row.remove(field);
        }
    }
    repo_path(text(&planned, "source")?, "archive reservation source")?;
    repo_path(
        text(&planned, "destination")?,
        "archive reservation destination",
    )?;
    text(&planned, "bucket")?;
    if !planned.get("remote_prefix").is_some_and(Value::is_string) {
        return Err(Error::message("archive reservation has no storage prefix"));
    }
    Ok(planned)
}

fn identity(receipt: &Value) -> Result<String> {
    let location = serde_json::to_vec(&[
        text(receipt, "bucket")?,
        receipt["remote_prefix"]
            .as_str()
            .ok_or_else(|| Error::message("archive reservation has no storage prefix"))?,
        text(receipt, "source")?,
    ])
    .map_err(|error| Error::message(error.to_string()))?;
    Ok(encode_lower(Sha256::digest(location)))
}

fn state_path(repo: &GitRepo, receipt: &Value) -> Result<PathBuf> {
    let root = repo.local_state_dir()?;
    let relative = format!("archive-reservations/{}.json", identity(receipt)?);
    reject_symlink_traversal(&root, &relative, "private archive reservation")?;
    Ok(root.join(relative))
}

fn owner(repo: &GitRepo) -> Result<(String, String)> {
    let root = repo.root.canonicalize().at(&repo.root)?;
    let root = root
        .to_str()
        .ok_or_else(|| Error::message("archive reservation checkout path is not UTF-8"))?
        .to_owned();
    Ok((encode_lower(Sha256::digest(root.as_bytes())), root))
}

fn read_state(path: &Path) -> Result<Option<Value>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(Error::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| Error::message(format!("invalid private archive reservation: {error}")))?;
    if value["schema_version"] != 1
        || !value["acquired"].is_boolean()
        || value["attempt_nonce"].as_str().is_none_or(str::is_empty)
    {
        return Err(Error::message(
            "unsupported private archive reservation schema",
        ));
    }
    Ok(Some(value))
}

fn nonce(path: &Path) -> Result<String> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::message("archive reservation has no parent"))?;
    fs::create_dir_all(parent).at(parent)?;
    let temporary = tempfile::Builder::new()
        .prefix("attempt-")
        .rand_bytes(32)
        .tempfile_in(parent)
        .at(parent)?;
    let name = temporary
        .path()
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| Error::message("archive reservation nonce is not UTF-8"))?
        .to_owned();
    Ok(name)
}

fn save(path: &Path, state: &Value) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::message("archive reservation has no parent"))?;
    fs::create_dir_all(parent).at(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent).at(parent)?;
    let bytes = serde_json::to_vec(state).map_err(|error| Error::message(error.to_string()))?;
    file.write_all(&bytes).at(path)?;
    file.as_file().sync_all().at(path)?;
    file.persist(path).map_err(|error| Error::Io {
        path: path.to_owned(),
        source: error.error,
    })?;
    fs::File::open(parent).at(parent)?.sync_all().at(parent)
}

fn validate_remote(repo: &GitRepo, remote: &str) -> Result<()> {
    repo.validate_remote_name(remote)?;
    let fetch = repo.run(["remote", "get-url", "--all", remote])?;
    let push = repo.run(["remote", "get-url", "--push", "--all", remote])?;
    if fetch.stdout.lines().count() != 1 || fetch.stdout != push.stdout {
        return Err(Error::message(
            "archive source reservation requires one identical Git fetch and push destination",
        ));
    }
    Ok(())
}

fn remote_oid(repo: &GitRepo, remote: &str, reference: &str) -> Result<Option<String>> {
    let output = repo.run(["ls-remote", "--refs", remote, reference])?;
    let mut oid = None;
    for line in output.stdout.lines() {
        let Some((value, name)) = line.split_once('\t') else {
            return Err(Error::message(
                "invalid archive source reservation ref response",
            ));
        };
        if name != reference
            || ![40, 64].contains(&value.len())
            || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
            || oid.replace(value.to_owned()).is_some()
        {
            return Err(Error::message(
                "ambiguous archive source reservation ref response",
            ));
        }
    }
    Ok(oid)
}

fn descriptor(state: &Value) -> Value {
    json!({"attempt_nonce":state["attempt_nonce"],"owner_hash":state["owner_hash"],"receipt":state["receipt"]})
}

fn control(repo: &GitRepo, state: &Value, write: bool) -> Result<(ControlOids, String)> {
    let body = serde_json::to_string(&descriptor(state))
        .map_err(|error| Error::message(error.to_string()))?;
    Ok((object_ids(&repo.root, &body, write)?, body))
}

pub fn reserve(repo: &GitRepo, receipt: &Value) -> Result<Value> {
    let planned = normalized_receipt(receipt)?;
    let config = Config::load(repo)?;
    validate_remote(repo, &config.git.remote)?;
    let key = identity(&planned)?;
    let reference = format!("refs/tags/workspace-mgr/archive-copy/{key}");
    let path = state_path(repo, &planned)?;
    let (owner_hash, repo_path) = owner(repo)?;
    let existing_state = read_state(&path)?;
    let mut state = match &existing_state {
        Some(state) => state.clone(),
        None => json!({"schema_version":1,"owner_hash":owner_hash,"receipt":planned,
                       "attempt_nonce":nonce(&path)?,"acquired":false}),
    };
    if state["owner_hash"] != owner_hash || state["receipt"] != planned {
        return Err(Error::message(
            "another pending archive reservation owns this source in the checkout; finish its cancellation first",
        ));
    }
    let canonical = format!("refs/tags/workspace-mgr/archive-registry/{key}");
    // An existing canonical mapping is never adopted by a new copy attempt.
    // The existing reservation owner may retry its already completed copy.
    let held = remote_oid(repo, &config.git.remote, &reference)?;
    if remote_oid(repo, &config.git.remote, &canonical)?.is_some()
        && (existing_state.is_none() || held.is_none())
    {
        return Err(Error::message(
            "archive source already has a canonical registry binding; refusing a new copy attempt",
        ));
    }
    let (ids, body) = control(repo, &state, true)?;
    let oid = if let Some(held) = held {
        if held != ids.commit && held != ids.legacy_blob {
            return Err(Error::message(
                "another checkout owns this archive source reservation; no S3 copy was authorized",
            ));
        }
        held
    } else {
        let oid = ids.commit;
        // Save provenance before CAS; after response loss, only this checkout
        // can recover the matching immutable remote descriptor.
        save(&path, &state)?;
        let lease = format!("--force-with-lease={reference}:");
        let refspec = format!("{oid}:{reference}");
        let pushed =
            repo.run_unchecked(["push", "--porcelain", &lease, &config.git.remote, &refspec])?;
        if remote_oid(repo, &config.git.remote, &reference)?.as_deref() != Some(&oid) {
            return Err(Error::message(format!(
                "archive source compare-and-create failed; no S3 copy was authorized: {}",
                pushed.stderr.trim()
            )));
        }
        oid
    };
    state["acquired"] = true.into();
    save(&path, &state)?;
    let receipt_body =
        serde_json::to_vec(&planned).map_err(|error| Error::message(error.to_string()))?;
    Ok(
        json!({"mode":"git-copy-reservation","remote":config.git.remote,"ref":reference,"oid":oid,
        "owner_hash":owner_hash,"attempt_nonce":state["attempt_nonce"],"state_path":path.to_string_lossy(),"repo_path":repo_path,
        "descriptor_sha256":encode_lower(Sha256::digest(body.as_bytes())),
        "receipt_sha256":encode_lower(Sha256::digest(receipt_body)),"receipt":planned}),
    )
}

/// Release only this checkout's exact reservation after cancellation finished.
pub fn release(repo: &GitRepo, receipt: &Value) -> Result<()> {
    let planned = normalized_receipt(receipt)?;
    let path = state_path(repo, &planned)?;
    let Some(state) = read_state(&path)? else {
        return Ok(());
    };
    let (owner_hash, _) = owner(repo)?;
    if state["owner_hash"] != owner_hash || state["receipt"] != planned {
        return Err(Error::message(
            "archive cancel refuses to release another attempt's source reservation",
        ));
    }
    let config = Config::load(repo)?;
    validate_remote(repo, &config.git.remote)?;
    let reference = format!(
        "refs/tags/workspace-mgr/archive-copy/{}",
        identity(&planned)?
    );
    let (ids, _) = control(repo, &state, false)?;
    if let Some(held) = remote_oid(repo, &config.git.remote, &reference)? {
        if held != ids.commit && held != ids.legacy_blob {
            // Cancellation has already proved this attempt's copies absent
            // and restored its local directory. A failed contender, or an
            // already released owner, must never remove the new winner's ref.
        } else {
            let lease = format!("--force-with-lease={reference}:{held}");
            let refspec = format!(":{reference}");
            repo.run_unchecked(["push", "--porcelain", &lease, &config.git.remote, &refspec])?;
            if remote_oid(repo, &config.git.remote, &reference)?.is_some() {
                return Err(Error::message(
                    "archive source reservation remains or changed after cancel; retry release",
                ));
            }
        }
    }
    fs::remove_file(&path).at(&path)?;
    let parent = path.parent().expect("reservation path has parent");
    fs::File::open(parent).at(parent)?.sync_all().at(parent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkout(root: PathBuf, remote: &Path) -> GitRepo {
        fs::create_dir(&root).unwrap();
        let repo = GitRepo { root };
        repo.run(["init", "-q", "-b", "main"]).unwrap();
        repo.run(["remote", "add", "origin", remote.to_str().unwrap()])
            .unwrap();
        fs::write(
            repo.root.join(".workspace-mgr.toml"),
            "[git]\nremote='origin'\nbranch='main'\n",
        )
        .unwrap();
        repo
    }

    fn fixture() -> (tempfile::TempDir, GitRepo, GitRepo, Value) {
        let temp = tempfile::tempdir().unwrap();
        let remote = temp.path().join("remote.git");
        crate::process::run(
            "git",
            ["init", "--bare", "-q", remote.to_str().unwrap()],
            temp.path(),
        )
        .unwrap();
        let first = checkout(temp.path().join("first"), &remote);
        let second = checkout(temp.path().join("second"), &remote);
        let receipt = json!({"schema_version":1,"status":"planned","remote":"workspace-mgr","bucket":"fixture",
            "remote_prefix":"storage","source":"task","destination":"2026/07/task","task_id":"fixture-task",
            "versions":[{"source_object":"task/data","destination_object":"2026/07/task/data","source_version_id":"old",
                "destination_version_id":null,"destination_etag":null,"delete_marker":false,"size":7,"source_etag":"etag"}]});
        (temp, first, second, receipt)
    }

    #[test]
    fn concurrent_clones_only_one_can_copy_even_different_destinations() {
        let (_temp, first, second, receipt) = fixture();
        let mut other = receipt.clone();
        other["destination"] = "2026/08/task".into();
        other["versions"][0]["destination_object"] = "2026/08/task/data".into();
        let barrier = std::sync::Barrier::new(2);
        let outcomes = std::thread::scope(|scope| {
            let a = scope.spawn(|| {
                barrier.wait();
                reserve(&first, &receipt)
            });
            let b = scope.spawn(|| {
                barrier.wait();
                reserve(&second, &other)
            });
            [a.join().unwrap(), b.join().unwrap()]
        });
        assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(outcomes.iter().filter(|result| result.is_err()).count(), 1);
        // The losing clone can undo its local attempt without deleting the
        // winning source reservation; no copy was ever authorized for it.
        let (winner, winner_receipt, loser, loser_receipt) = if outcomes[0].is_ok() {
            (&first, &receipt, &second, &other)
        } else {
            (&second, &other, &first, &receipt)
        };
        let proof = reserve(winner, winner_receipt).unwrap();
        release(loser, loser_receipt).unwrap();
        assert_eq!(
            remote_oid(winner, "origin", proof["ref"].as_str().unwrap())
                .unwrap()
                .as_deref(),
            proof["oid"].as_str()
        );
    }

    #[test]
    fn retries_are_owned_and_cancel_release_is_idempotent() {
        let (_temp, first, second, receipt) = fixture();
        let proof = reserve(&first, &receipt).unwrap();
        assert_eq!(reserve(&first, &receipt).unwrap(), proof);
        assert!(reserve(&second, &receipt).is_err());
        assert!(!state_path(&second, &receipt).unwrap().exists());
        let mut copied = receipt.clone();
        copied["status"] = "copied".into();
        copied["transaction_id"] = "copy-attempt".into();
        copied["versions"][0]["destination_version_id"] = "new".into();
        copied["versions"][0]["destination_etag"] = "new-etag".into();
        release(&first, &copied).unwrap();
        release(&first, &copied).unwrap();
        assert!(
            remote_oid(&first, "origin", proof["ref"].as_str().unwrap())
                .unwrap()
                .is_none()
        );
        let retried = reserve(&first, &receipt).unwrap();
        assert_ne!(retried["attempt_nonce"], proof["attempt_nonce"]);
        assert_ne!(retried["oid"], proof["oid"]);
        release(&first, &receipt).unwrap();
        reserve(&second, &receipt).unwrap();
    }

    #[test]
    fn snapshot_change_and_existing_canonical_mapping_are_not_adopted() {
        let (_temp, first, second, receipt) = fixture();
        reserve(&first, &receipt).unwrap();
        let mut changed = receipt.clone();
        changed["versions"][0]["source_version_id"] = "new-source-work".into();
        assert!(
            reserve(&first, &changed)
                .unwrap_err()
                .to_string()
                .contains("pending archive reservation")
        );
        let canonical = format!(
            "refs/tags/workspace-mgr/archive-registry/{}",
            identity(&receipt).unwrap()
        );
        let state = read_state(&state_path(&first, &receipt).unwrap())
            .unwrap()
            .unwrap();
        let (ids, _) = control(&first, &state, true).unwrap();
        let oid = ids.commit;
        first
            .run(["push", "origin", &format!("{oid}:{canonical}")])
            .unwrap();
        assert!(
            reserve(&second, &receipt)
                .unwrap_err()
                .to_string()
                .contains("canonical registry binding")
        );
        // Only the existing copy owner may resume under its still-held ref.
        reserve(&first, &receipt).unwrap();
    }

    #[test]
    fn lost_acquisition_result_is_recovered_and_foreign_release_never_deletes() {
        let (_temp, first, second, receipt) = fixture();
        let proof = reserve(&first, &receipt).unwrap();
        let path = state_path(&first, &receipt).unwrap();
        let mut state = read_state(&path).unwrap().unwrap();
        state["acquired"] = false.into();
        save(&path, &state).unwrap();
        assert_eq!(reserve(&first, &receipt).unwrap(), proof);
        assert_eq!(read_state(&path).unwrap().unwrap()["acquired"], true);
        let foreign = json!({"owner_hash":owner(&second).unwrap().0,"receipt":normalized_receipt(&receipt).unwrap(),
                             "schema_version":1,"acquired":true,"attempt_nonce":"foreign-attempt"});
        let (ids, _) = control(&second, &foreign, true).unwrap();
        let foreign_oid = ids.commit;
        let reference = proof["ref"].as_str().unwrap();
        second
            .run([
                "push",
                "--force",
                "origin",
                &format!("{foreign_oid}:{reference}"),
            ])
            .unwrap();
        release(&first, &receipt).unwrap();
        release(&first, &receipt).unwrap();
        assert_eq!(
            remote_oid(&first, "origin", reference).unwrap().as_deref(),
            Some(foreign_oid.as_str())
        );
        assert!(!path.exists());
    }
    #[cfg(unix)]
    #[test]
    fn large_source_inventory_uses_commit_only_host_and_legacy_claim_retries_release() {
        let (temp, repo, _, mut receipt) = fixture();
        receipt["versions"] = Value::Array(
            (0..800)
                .map(|i| {
                    json!({
                        "source_object":format!("task/file-{i:04}.bin"),
                        "destination_object":format!("2026/07/task/file-{i:04}.bin"),
                        "source_version_id":format!("source-version-{i:04}"),
                        "source_etag":format!("source-etag-{i:04}"),"delete_marker":false,"size":7
                    })
                })
                .collect(),
        );
        assert!(serde_json::to_vec(&receipt).unwrap().len() >= 135_895);
        let remote = temp.path().join("remote.git");
        crate::archive_git_control::tests::install_commit_only_hook(&remote);
        let proof = reserve(&repo, &receipt).unwrap();
        let oid = proof["oid"].as_str().unwrap();
        assert_eq!(
            repo.run(["--git-dir", remote.to_str().unwrap(), "cat-file", "-t", oid])
                .unwrap()
                .stdout
                .trim(),
            "commit"
        );
        let state = read_state(&state_path(&repo, &receipt).unwrap())
            .unwrap()
            .unwrap();
        let body = crate::archive_git_control::read_body(&repo.root, oid).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            descriptor(&state)
        );
        let ids = object_ids(&repo.root, &body, false).unwrap();
        let rejected = repo
            .run_unchecked([
                "push",
                "origin",
                &format!(
                    "{}:refs/tags/workspace-mgr/archive-copy/legacy-probe",
                    ids.legacy_blob
                ),
            ])
            .unwrap();
        assert!(!rejected.success());
        assert!(
            rejected
                .stderr
                .contains("archive control claim requires a commit")
        );
        assert_eq!(reserve(&repo, &receipt).unwrap(), proof);
        release(&repo, &receipt).unwrap();
        assert!(
            remote_oid(&repo, "origin", proof["ref"].as_str().unwrap())
                .unwrap()
                .is_none()
        );

        // Seed a legacy owner before this hosting restriction was introduced.
        fs::remove_file(remote.join("hooks/pre-receive")).unwrap();
        let new = reserve(&repo, &receipt).unwrap();
        let state = read_state(&state_path(&repo, &receipt).unwrap())
            .unwrap()
            .unwrap();
        let (ids, _) = control(&repo, &state, true).unwrap();
        let reference = new["ref"].as_str().unwrap();
        repo.run([
            "push",
            &format!(
                "--force-with-lease={reference}:{}",
                new["oid"].as_str().unwrap()
            ),
            "origin",
            &format!("{}:{reference}", ids.legacy_blob),
        ])
        .unwrap();
        crate::archive_git_control::tests::install_commit_only_hook(&remote);
        let legacy = reserve(&repo, &receipt).unwrap();
        assert_eq!(legacy["oid"], ids.legacy_blob);
        assert_eq!(
            remote_oid(&repo, "origin", reference).unwrap().as_deref(),
            Some(ids.legacy_blob.as_str())
        );
        assert_eq!(legacy["attempt_nonce"], new["attempt_nonce"]);
        release(&repo, &receipt).unwrap();
        assert!(remote_oid(&repo, "origin", reference).unwrap().is_none());
    }
}
