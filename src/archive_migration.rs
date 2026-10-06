use std::collections::BTreeSet;
use std::fs;
use std::io::Write;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::dvc;
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::path::{allowed, reject_symlink_traversal, repo_path, resolved_under};
use crate::policy::TASK_MANIFEST_NAME;
use crate::s3_purge::ObjectVersion;

pub const RECEIPT_NAME: &str = ".workspace-mgr-archive.json";

pub fn plan(repo: &GitRepo, config: &Config, source: &str, destination: &str) -> Result<Value> {
    if !config.s3_enabled() {
        return Ok(
            json!({"schema_version":1,"source":source,"destination":destination,
            "status":"planned","versions":[]}),
        );
    }
    if !config.requires_object_versioning() {
        return Err(Error::message(
            "archive requires version-aware S3 storage; migrate the repository before archiving stored tasks",
        ));
    }
    dvc::ensure_ready(repo, config)?;
    dvc::verify_object_versioning(repo, config)?;
    dvc::version_archive_adapter(
        repo,
        "plan",
        &json!({"source":source,"destination":destination}),
    )
}

pub fn receipts(repo: &GitRepo, scopes: &[String]) -> Result<Vec<(String, Value)>> {
    let mut result = Vec::new();
    for path in repo.visible_paths(scopes)? {
        if !path.ends_with(&format!("/{RECEIPT_NAME}")) {
            continue;
        }
        reject_symlink_traversal(&repo.root, &path, "archive receipt")?;
        let absolute = resolved_under(&repo.root, &path);
        let raw = fs::read_to_string(&absolute).at(&absolute)?;
        let receipt: Value = serde_json::from_str(&raw)
            .map_err(|error| Error::message(format!("invalid archive receipt {path}: {error}")))?;
        validate(&path, &receipt)?;
        result.push((path, receipt));
    }
    Ok(result)
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::message(format!("archive receipt is missing {key}")))
}

fn validate(path: &str, receipt: &Value) -> Result<()> {
    let source = repo_path(text(receipt, "source")?, "archive source")?;
    let destination = repo_path(text(receipt, "destination")?, "archive destination")?;
    if receipt["schema_version"] != 1
        || !matches!(receipt["status"].as_str(), Some("planned" | "copied"))
        || path != format!("{destination}/{RECEIPT_NAME}")
        || source == destination
        || source.starts_with(&format!("{destination}/"))
        || destination.starts_with(&format!("{source}/"))
        || source.rsplit('/').next() != destination.rsplit('/').next()
        || !receipt["versions"].is_array()
    {
        return Err(Error::message(format!(
            "invalid archive migration identity or location: {path}"
        )));
    }
    text(receipt, "task_id")?;
    for version in receipt["versions"].as_array().expect("validated versions") {
        let object = text(version, "source_object")?;
        if !object.starts_with(&format!("{source}/"))
            || text(version, "destination_object")?
                != format!("{destination}{}", &object[source.len()..])
            || !version["delete_marker"].is_boolean()
        {
            return Err(Error::message(format!(
                "archive version escapes its task prefixes: {path}"
            )));
        }
        text(version, "source_version_id")?;
        if receipt["status"] == "copied" {
            text(version, "destination_version_id")?;
        }
    }
    Ok(())
}

pub fn pointer_set(repo: &GitRepo, scopes: &[String]) -> Result<BTreeSet<String>> {
    let mut result = BTreeSet::new();
    for (_, receipt) in receipts(repo, scopes)? {
        result.extend(dvc::discover(
            repo,
            &[text(&receipt, "destination")?.to_owned()],
        )?);
    }
    Ok(result)
}

/// The source snapshot is revalidated by the transport before it copies any
/// version. Journals live outside the checkout and survive a failed publish.
pub fn prepare(
    repo: &GitRepo,
    config: &Config,
    scopes: &[String],
    base: &str,
) -> Result<Vec<Value>> {
    let mut completed = Vec::new();
    for (path, mut receipt) in receipts(repo, scopes)? {
        let source = text(&receipt, "source")?.to_owned();
        let destination = text(&receipt, "destination")?.to_owned();
        let existing = repo.run_unchecked(["show", &format!("{base}:{path}")])?;
        let published = existing.success()
            && serde_json::from_str::<Value>(&existing.stdout)
                .ok()
                .as_ref()
                == Some(&receipt);
        let pointers = dvc::discover(repo, std::slice::from_ref(&destination))?;
        for pointer in &pointers {
            let absolute = resolved_under(&repo.root, pointer);
            let raw = fs::read_to_string(&absolute).at(&absolute)?;
            let output = resolved_under(&repo.root, pointer.strip_suffix(".dvc").unwrap());
            if output.exists() && !dvc::payload_matches_metadata(repo, pointer, &raw)? {
                return Err(Error::message(format!(
                    "archived output changed after planning: {pointer}"
                )));
            }
        }
        if !published {
            if !allowed(&source, scopes) || !allowed(&destination, scopes) {
                return Err(Error::message(
                    "archive migration requires both source and destination in the infrastructure task's declared scopes",
                ));
            }
            let manifest_path = format!("{source}/{TASK_MANIFEST_NAME}");
            let original = repo.run(["show", &format!("{base}:{manifest_path}")])?;
            let manifest: crate::manifest::TaskManifest = toml::from_str(&original.stdout)
                .map_err(|error| {
                    Error::message(format!("invalid archive source manifest: {error}"))
                })?;
            if manifest.id != text(&receipt, "task_id")? {
                return Err(Error::message(
                    "archive source identity changed since the migration was prepared",
                ));
            }
        }
        if receipt["status"] == "planned" {
            if config.s3_enabled() {
                let digest = crate::hex::encode_lower(
                    Sha256::digest(format!("{source}\0{destination}").as_bytes()).as_slice(),
                );
                let journal_dir = repo.local_state_dir()?.join("archive");
                fs::create_dir_all(&journal_dir).at(&journal_dir)?;
                let journal = journal_dir.join(format!("{digest}.json"));
                let copied = dvc::version_archive_adapter(
                    repo,
                    "copy",
                    &json!({
                        "source":source,"destination":destination,"planned":receipt,
                        "state_path":journal.to_string_lossy()
                    }),
                )?;
                // Keep task identity and earlier receipts, which the transport
                // intentionally does not interpret.
                let mut next = copied;
                for key in ["task_id", "previous_receipt"] {
                    if let Some(value) = receipt.get(key) {
                        next.as_object_mut()
                            .ok_or_else(|| Error::message("archive copy did not return an object"))?
                            .insert(key.to_owned(), value.clone());
                    }
                }
                receipt = next;
                dvc::version_archive_adapter(repo, "verify", &receipt)?;
                dvc::archive_registry_adapter(repo, "publish", &receipt)?;
            } else {
                receipt["status"] = "copied".into();
            }
            write_receipt(repo, &path, &receipt)?;
        }
        if config.s3_enabled() && receipt["status"] == "copied" {
            if !published && !trusted_copy_journal(repo, &receipt)? {
                dvc::version_archive_adapter(repo, "verify-source", &receipt)?;
            }
            dvc::version_archive_adapter(repo, "verify", &receipt)?;
            dvc::archive_registry_adapter(repo, "publish", &receipt)?;
            for pointer in &pointers {
                rewrite_pointer(repo, pointer, &receipt)?;
            }
            dvc::verify_archived(repo, &pointers)?;
        }
        completed.push(receipt);
    }
    Ok(completed)
}

fn trusted_copy_journal(repo: &GitRepo, receipt: &Value) -> Result<bool> {
    let source = text(receipt, "source")?;
    let destination = text(receipt, "destination")?;
    let digest = crate::hex::encode_lower(
        Sha256::digest(format!("{source}\0{destination}").as_bytes()).as_slice(),
    );
    let path = repo
        .local_state_dir()?
        .join("archive")
        .join(format!("{digest}.json"));
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => return Err(Error::Io { path, source }),
    };
    let mut journal: Value = serde_json::from_str(&raw)
        .map_err(|error| Error::message(format!("invalid private archive journal: {error}")))?;
    if [
        "schema_version",
        "remote",
        "bucket",
        "remote_prefix",
        "source",
        "destination",
        "transaction_id",
    ]
    .iter()
    .any(|key| journal[key] != receipt[key])
        || journal["status"] != "copied"
    {
        return Ok(false);
    }
    let Some(versions) = journal["versions"].as_array_mut() else {
        return Ok(false);
    };
    for version in versions {
        if let Some(object) = version.as_object_mut() {
            object.remove("started");
            object.remove("multipart_upload_id");
        }
    }
    Ok(journal["versions"] == receipt["versions"])
}

pub fn purge_candidates(receipts: &[Value]) -> Result<Vec<ObjectVersion>> {
    let mut result = BTreeSet::new();
    for receipt in receipts {
        for version in receipt["versions"]
            .as_array()
            .ok_or_else(|| Error::message("archive versions missing"))?
        {
            result.insert(ObjectVersion {
                pointer: format!("{}/{RECEIPT_NAME}", text(receipt, "source")?),
                object: text(version, "source_object")?.to_owned(),
                version_id: text(version, "source_version_id")?.to_owned(),
            });
        }
    }
    Ok(result.into_iter().collect())
}

fn rewrite_pointer(repo: &GitRepo, pointer: &str, receipt: &Value) -> Result<()> {
    let absolute = resolved_under(&repo.root, pointer);
    let raw = fs::read_to_string(&absolute).at(&absolute)?;
    let entries = dvc::parse_pointer_document(&raw, pointer)?.entries(pointer);
    let mut document: serde_yaml::Value =
        serde_yaml::from_str(&raw).map_err(|source| Error::Yaml {
            path: absolute.clone(),
            source,
        })?;
    let outs = document["outs"]
        .as_sequence_mut()
        .ok_or_else(|| Error::message("archive pointer has no outputs"))?;
    let mut entry_index = 0;
    for out in outs {
        if let Some(files) = out
            .get_mut("files")
            .and_then(serde_yaml::Value::as_sequence_mut)
        {
            for file in files {
                replace_cloud(file, &entries[entry_index], receipt)?;
                entry_index += 1;
            }
        } else {
            replace_cloud(out, &entries[entry_index], receipt)?;
            entry_index += 1;
        }
    }
    let rendered =
        serde_yaml::to_string(&document).map_err(|error| Error::message(error.to_string()))?;
    atomic_write(&absolute, rendered.as_bytes())
}

fn replace_cloud(
    value: &mut serde_yaml::Value,
    entry: &dvc::PointerEntry,
    receipt: &Value,
) -> Result<()> {
    let versions = receipt["versions"]
        .as_array()
        .ok_or_else(|| Error::message("archive versions missing"))?;
    let matching = versions
        .iter()
        .find(|version| {
            version["destination_object"] == entry.key
                && (version["source_version_id"].as_str() == entry.version_id.as_deref()
                    || version["destination_version_id"].as_str() == entry.version_id.as_deref())
                && !version["delete_marker"].as_bool().unwrap_or(false)
        })
        .ok_or_else(|| {
            Error::message(format!("archive has no copied version for {}", entry.key))
        })?;
    value["cloud"]["workspace-mgr"]["version_id"] =
        text(matching, "destination_version_id")?.into();
    value["cloud"]["workspace-mgr"]["etag"] = text(matching, "destination_etag")?.into();
    Ok(())
}

fn write_receipt(repo: &GitRepo, path: &str, receipt: &Value) -> Result<()> {
    let mut bytes =
        serde_json::to_vec_pretty(receipt).map_err(|error| Error::message(error.to_string()))?;
    bytes.push(b'\n');
    atomic_write(&resolved_under(&repo.root, path), &bytes)
}

fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::message("archive metadata has no parent"))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).at(parent)?;
    temp.write_all(bytes).at(path)?;
    temp.as_file().sync_all().at(path)?;
    temp.persist(path).map_err(|error| Error::Io {
        path: path.to_path_buf(),
        source: error.error,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "20260712-120000-completed";
    const DESTINATION: &str = "2026/07/20260712-120000-completed";

    fn receipt(status: &str, objects: &[&str]) -> Value {
        json!({
            "schema_version": 1,
            "source": SOURCE,
            "destination": DESTINATION,
            "task_id": SOURCE,
            "status": status,
            "versions": objects.iter().enumerate().map(|(index, object)| json!({
                "source_object": format!("{SOURCE}/{object}"),
                "destination_object": format!("{DESTINATION}/{object}"),
                "source_version_id": format!("source-{index}"),
                "destination_version_id": format!("destination-{index}"),
                "destination_etag": format!("etag-{index}"),
                "delete_marker": false,
            })).collect::<Vec<_>>(),
        })
    }

    fn repository() -> (tempfile::TempDir, GitRepo) {
        let temp = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: temp.path().canonicalize().unwrap(),
        };
        (temp, repo)
    }

    fn write(repo: &GitRepo, path: &str, raw: &str) {
        let absolute = repo.root.join(path);
        fs::create_dir_all(absolute.parent().unwrap()).unwrap();
        fs::write(absolute, raw).unwrap();
    }

    #[test]
    fn receipts_refuse_forged_version_prefixes_and_incomplete_copy_identity() {
        let path = format!("{DESTINATION}/{RECEIPT_NAME}");
        let copied = receipt("copied", &["artifact.bin"]);
        validate(&path, &copied).unwrap();
        let mut planned = copied.clone();
        planned["status"] = "planned".into();
        planned["versions"][0]
            .as_object_mut()
            .unwrap()
            .remove("destination_version_id");
        validate(&path, &planned).unwrap();

        for (field, value) in [
            ("source_object", "another-task/artifact.bin"),
            ("destination_object", "another-task/artifact.bin"),
            (
                "destination_object",
                "2026/07/20260712-120000-completed/other.bin",
            ),
            ("source_version_id", ""),
            ("destination_version_id", ""),
        ] {
            let mut forged = copied.clone();
            forged["versions"][0][field] = value.into();
            assert!(
                validate(&path, &forged).is_err(),
                "accepted forged {field}: {value}"
            );
        }
        planned["status"] = "copied".into();
        assert!(validate(&path, &planned).is_err());
        assert!(validate(&format!("other/{RECEIPT_NAME}"), &copied).is_err());
    }

    #[test]
    fn receipt_history_preserves_opaque_s3_object_suffixes() {
        let path = format!("{DESTINATION}/{RECEIPT_NAME}");
        for object in [
            "data/a.bin ",
            "data/a\nb.bin",
            "data//a.bin",
            "legacy-folder/",
        ] {
            let copied = receipt("copied", &[object]);
            validate(&path, &copied).unwrap();
            let candidates = purge_candidates(&[copied]).unwrap();
            assert_eq!(candidates.len(), 1);
            assert_eq!(candidates[0].object, format!("{SOURCE}/{object}"));
        }
    }

    #[test]
    fn copied_receipt_requires_both_scopes_and_original_task_identity() {
        let (_temp, repo) = repository();
        repo.run(["init", "-b", "main"]).unwrap();
        repo.run(["config", "user.name", "workspace-mgr test"])
            .unwrap();
        repo.run(["config", "user.email", "test@example.invalid"])
            .unwrap();
        write(
            &repo,
            &format!("{SOURCE}/{TASK_MANIFEST_NAME}"),
            &format!(
                "schema_version = 2\nkind = \"deliverable\"\nid = \"{SOURCE}\"\nslug = \"completed\"\npath = \"{SOURCE}\"\nbranch = \"codex/completed\"\ntitle = \"Retained task\"\npurpose = \"Keep the task\"\nadditional_scopes = []\n"
            ),
        );
        repo.run(["add", "."]).unwrap();
        repo.run(["commit", "-m", "Retain the source task"])
            .unwrap();
        let base = repo
            .run(["rev-parse", "HEAD"])
            .unwrap()
            .stdout
            .trim()
            .to_owned();
        let path = format!("{DESTINATION}/{RECEIPT_NAME}");
        let mut copied = receipt("copied", &[]);
        write(&repo, &path, &copied.to_string());
        let error = prepare(&repo, &Config::default(), &[DESTINATION.to_owned()], &base)
            .unwrap_err()
            .to_string();
        assert!(error.contains("both source and destination"), "{error}");
        copied["task_id"] = "another-task".into();
        write(&repo, &path, &copied.to_string());
        let scopes = [SOURCE.to_owned(), DESTINATION.to_owned()];
        let error = prepare(&repo, &Config::default(), &scopes, &base)
            .unwrap_err()
            .to_string();
        assert!(error.contains("source identity changed"), "{error}");
        copied["task_id"] = SOURCE.into();
        write(&repo, &path, &copied.to_string());
        assert_eq!(
            prepare(&repo, &Config::default(), &scopes, &base).unwrap(),
            vec![copied]
        );
    }

    #[test]
    fn standalone_pointer_rewrite_preserves_metadata_and_can_be_retried() {
        let (_temp, repo) = repository();
        let pointer = format!("{DESTINATION}/artifact.bin.dvc");
        let raw = "meta:\n  owner: research\nouts:\n- md5: 0123456789abcdef0123456789abcdef\n  size: 19\n  hash: md5\n  path: artifact.bin\n  custom: retained\n  cloud:\n    workspace-mgr:\n      version_id: source-0\n      etag: old-etag\n      custom: retained\n    another-remote:\n      version_id: another-version\n";
        write(&repo, &pointer, raw);
        let migration = receipt("copied", &["artifact.bin"]);
        let mut expected: serde_yaml::Value = serde_yaml::from_str(raw).unwrap();
        expected["outs"][0]["cloud"]["workspace-mgr"]["version_id"] = "destination-0".into();
        expected["outs"][0]["cloud"]["workspace-mgr"]["etag"] = "etag-0".into();
        rewrite_pointer(&repo, &pointer, &migration).unwrap();
        let first = fs::read_to_string(repo.root.join(&pointer)).unwrap();
        assert_eq!(
            serde_yaml::from_str::<serde_yaml::Value>(&first).unwrap(),
            expected
        );
        rewrite_pointer(&repo, &pointer, &migration).unwrap();
        assert_eq!(fs::read_to_string(repo.root.join(&pointer)).unwrap(), first);
        assert!(
            !repo
                .root
                .join(format!("{DESTINATION}/artifact.bin"))
                .exists()
        );
    }

    #[test]
    fn directory_rewrite_is_atomic_and_preserves_files_hashes_and_aggregate() {
        let (_temp, repo) = repository();
        let pointer = format!("{DESTINATION}/data.dvc");
        let raw = "outs:\n- md5: 0123456789abcdef0123456789abcdef.dir\n  size: 31\n  nfiles: 2\n  hash: md5\n  path: data\n  cloud:\n    workspace-mgr:\n      version_id: aggregate-kept\n  files:\n  - relpath: a.bin\n    md5: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n    size: 12\n    custom: retained\n    cloud:\n      workspace-mgr:\n        version_id: destination-0\n        etag: etag-0\n  - relpath: nested/b.bin\n    md5: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n    size: 19\n    cloud:\n      workspace-mgr:\n        version_id: source-1\n        etag: old-etag\n";
        write(&repo, &pointer, raw);
        let migration = receipt("copied", &["data/a.bin", "data/nested/b.bin"]);
        let mut incomplete = migration.clone();
        incomplete["versions"].as_array_mut().unwrap().pop();
        let error = rewrite_pointer(&repo, &pointer, &incomplete)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no copied version"), "{error}");
        assert_eq!(fs::read_to_string(repo.root.join(&pointer)).unwrap(), raw);
        rewrite_pointer(&repo, &pointer, &migration).unwrap();
        let mut expected: serde_yaml::Value = serde_yaml::from_str(raw).unwrap();
        expected["outs"][0]["files"][1]["cloud"]["workspace-mgr"]["version_id"] =
            "destination-1".into();
        expected["outs"][0]["files"][1]["cloud"]["workspace-mgr"]["etag"] = "etag-1".into();
        assert_eq!(
            serde_yaml::from_str::<serde_yaml::Value>(
                &fs::read_to_string(repo.root.join(&pointer)).unwrap()
            )
            .unwrap(),
            expected
        );
    }
}
