//! Git's compare-and-create ref update coordinates providers without S3 CAS.
//! An immutable control commit binds the complete receipt; no lease expires or is stolen.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::archive_git_control::object_ids;
use crate::archive_migration::RECEIPT_NAME;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::git::GitRepo;
use crate::hex::encode_lower;

fn text<'a>(receipt: &'a Value, key: &str) -> Result<&'a str> {
    receipt
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::message(format!("archive registry receipt has no {key}")))
}

pub fn binding_ref(receipt: &Value) -> Result<String> {
    let identity = serde_json::to_vec(&[
        text(receipt, "bucket")?,
        text(receipt, "remote_prefix")?,
        text(receipt, "source")?,
    ])
    .map_err(|error| Error::message(error.to_string()))?;
    Ok(format!(
        "refs/tags/workspace-mgr/archive-registry/{}",
        encode_lower(Sha256::digest(identity))
    ))
}

fn validate_remote(repo: &GitRepo, remote: &str) -> Result<()> {
    repo.validate_remote_name(remote)?;
    let fetch = repo.run(["remote", "get-url", "--all", remote])?;
    let push = repo.run(["remote", "get-url", "--push", "--all", remote])?;
    let fetch = fetch.stdout.lines().collect::<Vec<_>>();
    let push = push.stdout.lines().collect::<Vec<_>>();
    if fetch.len() != 1 || push != fetch {
        return Err(Error::message(
            "archive coordination requires one identical verified Git fetch and push destination",
        ));
    }
    Ok(())
}

fn remote_oid(repo: &GitRepo, remote: &str, reference: &str) -> Result<Option<String>> {
    validate_remote(repo, remote)?;
    let listing = repo.run(["ls-remote", "--refs", remote, reference])?;
    let mut found = None;
    for line in listing.stdout.lines() {
        let Some((oid, name)) = line.split_once('\t') else {
            return Err(Error::message("invalid archive coordination ref response"));
        };
        if name != reference
            || ![40, 64].contains(&oid.len())
            || !oid.bytes().all(|byte| byte.is_ascii_hexdigit())
            || found.replace(oid.to_owned()).is_some()
        {
            return Err(Error::message(
                "ambiguous archive coordination ref response",
            ));
        }
    }
    Ok(found)
}

pub fn has_binding(repo: &GitRepo, receipt: &Value) -> Result<bool> {
    let config = Config::load(repo)?;
    Ok(remote_oid(repo, &config.git.remote, &binding_ref(receipt)?)?.is_some())
}

/// Produces a proof only for the owning private journal or an exact receipt
/// already present on the freshly fetched shared Git branch.
pub fn coordinate(repo: &GitRepo, receipt: &Value, create: bool) -> Result<Value> {
    coordinate_with_authority(repo, receipt, create, false)
}

/// Source retirement needs the reviewed shared-branch receipt. A private
/// copy journal may authorize publication/cancellation, never source deletion.
pub fn coordinate_published(repo: &GitRepo, receipt: &Value) -> Result<Value> {
    coordinate_with_authority(repo, receipt, true, true)
}

fn coordinate_with_authority(
    repo: &GitRepo,
    receipt: &Value,
    create: bool,
    published_only: bool,
) -> Result<Value> {
    let config = Config::load(repo)?;
    repo.validate_remote_name(&config.git.remote)?;
    let reference = binding_ref(receipt)?;
    let body = serde_json::to_string(receipt).map_err(|error| Error::message(error.to_string()))?;
    let ids = object_ids(&repo.root, &body, create)?;
    let oid = &ids.commit;
    let mut proof = json!({
        "mode":"git-cas", "remote":config.git.remote, "ref":reference,
        "oid":oid, "receipt_sha256":encode_lower(Sha256::digest(body.as_bytes())),
        "transaction_id":receipt.get("transaction_id").cloned().unwrap_or(Value::Null),
    });
    let journal = crate::archive_cancel::copy_journal(
        repo,
        text(receipt, "source")?,
        text(receipt, "destination")?,
    )?;
    if !published_only && crate::archive_migration::trusted_copy_journal(repo, receipt)? {
        proof["state_path"] = journal.to_string_lossy().to_string().into();
    } else {
        let base = repo.fetch_branch(&config.git.remote, &config.git.branch)?;
        let path = format!("{}/{RECEIPT_NAME}", text(receipt, "destination")?);
        let actual = repo.run_unchecked(["show", &format!("{base}:{path}")])?;
        let published: Option<Value> = actual
            .success()
            .then(|| serde_json::from_str(&actual.stdout).ok())
            .flatten();
        if published.as_ref() != Some(receipt) {
            return Err(Error::message(
                "archive registry mutation requires this attempt's verified private copy journal or its exact published Git receipt",
            ));
        }
        proof["publication_oid"] = base.into();
        proof["receipt_path"] = path.into();
        proof["base_branch"] = config.git.branch.into();
    }
    if let Some(existing) = remote_oid(repo, &config.git.remote, &reference)? {
        if existing != ids.commit && existing != ids.legacy_blob {
            return Err(Error::message(
                "another archive transaction owns the canonical registry binding; its objects and mapping were preserved",
            ));
        }
        proof["oid"] = existing.into();
        return Ok(proof);
    }
    if !create {
        return Err(Error::message(
            "archive registry has no canonical Git coordination binding",
        ));
    }
    let lease = format!("--force-with-lease={reference}:");
    let refspec = format!("{oid}:{reference}");
    let pushed =
        repo.run_unchecked(["push", "--porcelain", &lease, &config.git.remote, &refspec])?;
    // This read also recovers a lost response after the remote accepted CAS.
    let observed = remote_oid(repo, &config.git.remote, &reference)?;
    if observed
        .as_ref()
        .is_none_or(|held| held != &ids.commit && held != &ids.legacy_blob)
    {
        return Err(Error::message(format!(
            "archive registry compare-and-create failed; no S3 registry write was authorized: {}",
            pushed.stderr.trim()
        )));
    }
    proof["oid"] = observed.expect("verified remote control identity").into();
    Ok(proof)
}

/// Withdraw only our exact binding after both registry and copied objects are
/// absent. A new publisher cannot acquire the source until cleanup completes.
pub fn release(repo: &GitRepo, receipt: &Value) -> Result<()> {
    release_binding(repo, receipt, false)
}

/// A completed remote cancellation can resume local undo after a later owner
/// has acquired the source. Its binding must be preserved.
pub fn release_if_owned(repo: &GitRepo, receipt: &Value) -> Result<()> {
    release_binding(repo, receipt, true)
}

fn release_binding(repo: &GitRepo, receipt: &Value, allow_foreign: bool) -> Result<()> {
    let config = Config::load(repo)?;
    let reference = binding_ref(receipt)?;
    let body = serde_json::to_string(receipt).map_err(|error| Error::message(error.to_string()))?;
    let ids = object_ids(&repo.root, &body, false)?;
    let Some(existing) = remote_oid(repo, &config.git.remote, &reference)? else {
        return Ok(());
    };
    if existing != ids.commit && existing != ids.legacy_blob {
        if allow_foreign {
            return Ok(());
        }
        return Err(Error::message(
            "archive cancel refuses to release another transaction's registry binding",
        ));
    }
    let lease = format!("--force-with-lease={reference}:{existing}");
    let refspec = format!(":{reference}");
    repo.run_unchecked(["push", "--porcelain", &lease, &config.git.remote, &refspec])?;
    if remote_oid(repo, &config.git.remote, &reference)?.is_some() {
        return Err(Error::message(
            "archive registry binding changed or remains after cancellation; retry cleanup",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::IoContext;
    use std::fs;

    fn fixture() -> (tempfile::TempDir, GitRepo, Value) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let remote = temp.path().join("remote.git");
        fs::create_dir(&root).unwrap();
        let repo = GitRepo { root };
        repo.run(["init", "-q", "-b", "main"]).unwrap();
        repo.run(["init", "-q", "--bare", remote.to_str().unwrap()])
            .unwrap();
        repo.run(["remote", "add", "origin", remote.to_str().unwrap()])
            .unwrap();
        fs::write(
            repo.root.join(".workspace-mgr.toml"),
            "[git]\nremote='origin'\nbranch='main'\n",
        )
        .unwrap();
        let receipt = json!({"schema_version":1,"remote":"workspace-mgr","bucket":"fixture","remote_prefix":"storage","source":"task","destination":"2026/07/task","transaction_id":"fixture-attempt","status":"copied","versions":[]});
        let journal = crate::archive_cancel::copy_journal(&repo, "task", "2026/07/task").unwrap();
        fs::create_dir_all(journal.parent().unwrap())
            .at(&journal)
            .unwrap();
        fs::write(journal, serde_json::to_vec(&receipt).unwrap()).unwrap();
        (temp, repo, receipt)
    }

    #[test]
    fn concurrent_claims_have_one_winner_and_no_expiring_takeover() {
        let (temp, repo, receipt) = fixture();
        let clone_root = temp.path().join("second");
        fs::create_dir(&clone_root).unwrap();
        let second = GitRepo { root: clone_root };
        second.run(["init", "-q"]).unwrap();
        second
            .run([
                "remote",
                "add",
                "origin",
                temp.path().join("remote.git").to_str().unwrap(),
            ])
            .unwrap();
        fs::copy(
            repo.root.join(".workspace-mgr.toml"),
            second.root.join(".workspace-mgr.toml"),
        )
        .unwrap();
        let mut other = receipt.clone();
        other["transaction_id"] = "second-attempt".into();
        let journal = crate::archive_cancel::copy_journal(&second, "task", "2026/07/task").unwrap();
        fs::create_dir_all(journal.parent().unwrap()).unwrap();
        fs::write(journal, serde_json::to_vec(&other).unwrap()).unwrap();
        let barrier = std::sync::Barrier::new(2);
        let results = std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                barrier.wait();
                coordinate(&repo, &receipt, true)
            });
            let second = scope.spawn(|| {
                barrier.wait();
                coordinate(&second, &other, true)
            });
            [first.join().unwrap(), second.join().unwrap()]
        });
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    }

    #[test]
    fn different_push_destination_cannot_receive_or_authorize_a_binding() {
        let (temp, repo, receipt) = fixture();
        let foreign = temp.path().join("foreign.git");
        repo.run(["init", "--bare", "-q", foreign.to_str().unwrap()])
            .unwrap();
        repo.run([
            "remote",
            "set-url",
            "--push",
            "origin",
            foreign.to_str().unwrap(),
        ])
        .unwrap();
        assert!(
            coordinate(&repo, &receipt, true)
                .unwrap_err()
                .to_string()
                .contains("identical verified")
        );
        assert!(
            repo.run(["--git-dir", foreign.to_str().unwrap(), "for-each-ref"])
                .unwrap()
                .stdout
                .is_empty()
        );
    }

    #[test]
    fn immutable_binding_is_idempotent_and_conflict_never_overwrites_winner() {
        let (_temp, repo, receipt) = fixture();
        let winner = coordinate(&repo, &receipt, true).unwrap();
        assert_eq!(coordinate(&repo, &receipt, true).unwrap(), winner);
        let mut other = receipt.clone();
        other["transaction_id"] = "competing-attempt".into();
        let journal = crate::archive_cancel::copy_journal(&repo, "task", "2026/07/task").unwrap();
        fs::write(journal, serde_json::to_vec(&other).unwrap()).unwrap();
        assert!(
            coordinate(&repo, &other, true)
                .unwrap_err()
                .to_string()
                .contains("another archive transaction")
        );
        assert_eq!(
            remote_oid(&repo, "origin", winner["ref"].as_str().unwrap())
                .unwrap()
                .as_deref(),
            winner["oid"].as_str()
        );
        assert!(release(&repo, &other).is_err());
        release_if_owned(&repo, &other).unwrap();
        assert_eq!(
            remote_oid(&repo, "origin", winner["ref"].as_str().unwrap())
                .unwrap()
                .as_deref(),
            winner["oid"].as_str()
        );
        release(&repo, &receipt).unwrap();
        release(&repo, &receipt).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn large_registry_inventory_uses_commit_only_host_and_preserves_legacy_binding() {
        let (temp, repo, mut receipt) = fixture();
        receipt["versions"] = Value::Array((0..800).map(|i| json!({
            "source_object":format!("task/file-{i:04}.bin"),
            "destination_object":format!("2026/07/task/file-{i:04}.bin"),
            "source_version_id":format!("source-version-{i:04}"),"destination_version_id":format!("copied-version-{i:04}"),
            "source_etag":format!("source-etag-{i:04}"),"destination_etag":format!("copied-etag-{i:04}"),
            "delete_marker":false,"size":7
        })).collect());
        let body = serde_json::to_string(&receipt).unwrap();
        assert!(body.len() >= 135_895);
        let journal = crate::archive_cancel::copy_journal(&repo, "task", "2026/07/task").unwrap();
        fs::write(&journal, &body).unwrap();
        let remote = temp.path().join("remote.git");
        crate::archive_git_control::tests::install_commit_only_hook(&remote);
        let proof = coordinate(&repo, &receipt, true).unwrap();
        let oid = proof["oid"].as_str().unwrap();
        assert_eq!(
            repo.run(["--git-dir", remote.to_str().unwrap(), "cat-file", "-t", oid])
                .unwrap()
                .stdout
                .trim(),
            "commit"
        );
        assert_eq!(
            crate::archive_git_control::read_body(&repo.root, oid).unwrap(),
            body
        );
        let ids = object_ids(&repo.root, &body, false).unwrap();
        let rejected = repo
            .run_unchecked([
                "push",
                "origin",
                &format!(
                    "{}:refs/tags/workspace-mgr/archive-registry/legacy-probe",
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
        assert_eq!(coordinate(&repo, &receipt, false).unwrap(), proof);
        release(&repo, &receipt).unwrap();

        fs::remove_file(remote.join("hooks/pre-receive")).unwrap();
        let ids = object_ids(&repo.root, &body, true).unwrap();
        let reference = binding_ref(&receipt).unwrap();
        repo.run([
            "push",
            &format!("--force-with-lease={reference}:"),
            "origin",
            &format!("{}:{reference}", ids.legacy_blob),
        ])
        .unwrap();
        crate::archive_git_control::tests::install_commit_only_hook(&remote);
        let legacy = coordinate(&repo, &receipt, true).unwrap();
        assert_eq!(legacy["oid"], ids.legacy_blob);
        assert_eq!(coordinate(&repo, &receipt, false).unwrap(), legacy);
        assert_eq!(
            remote_oid(&repo, "origin", &reference).unwrap().as_deref(),
            Some(ids.legacy_blob.as_str())
        );
        release(&repo, &receipt).unwrap();
        assert!(remote_oid(&repo, "origin", &reference).unwrap().is_none());
    }
}
