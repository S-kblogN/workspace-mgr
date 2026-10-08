//! Versioned native storage sidecars. The wire format has no external-engine fields.
use std::collections::BTreeSet;

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::path::repo_path;

pub const SUFFIX: &str = ".wm-storage.json";
pub const SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Checksum {
    pub algorithm: String,
    pub digest: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Directory,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Version {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<Verification>,
}

/// A raw-byte checksum reliably established for this exact remote version.
///
/// Storage scope is part of the evidence: a repository configuration change
/// must not reuse evidence from a different endpoint, bucket or object key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Verification {
    pub endpoint: String,
    pub bucket: String,
    pub key: String,
    pub version_id: String,
    pub checksum: Checksum,
    pub size: u64,
    pub method: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub path: String,
    pub checksum: Checksum,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<Version>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub path: String,
    pub kind: Kind,
    pub checksum: Checksum,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<Version>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entries: Option<Vec<Entry>>,
}

impl Checksum {
    pub fn validate(&self) -> Result<()> {
        let length = match self.algorithm.as_str() {
            "md5" | "md5-dos2unix" => 32,
            "sha256" => 64,
            _ => return Err(Error::message("invalid native storage checksum")),
        };
        if self.digest.len() != length
            || !self
                .digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::message("invalid native storage checksum"));
        }
        Ok(())
    }

    fn validate_logical(&self) -> Result<()> {
        self.validate()?;
        if !matches!(self.algorithm.as_str(), "md5" | "md5-dos2unix") {
            return Err(Error::message(
                "native storage boundary checksum must use md5 or md5-dos2unix",
            ));
        }
        Ok(())
    }
}

impl Verification {
    pub fn validate(&self) -> Result<()> {
        self.checksum.validate()?;
        if self.checksum.algorithm != "sha256" {
            return Err(Error::message(
                "native storage verification requires a raw sha256 checksum",
            ));
        }
        let endpoint = url::Url::parse(&self.endpoint).map_err(|_| {
            Error::message("native storage verification requires an HTTP(S) endpoint")
        })?;
        if self.endpoint.trim() != self.endpoint
            || !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || self.endpoint.chars().any(char::is_control)
            || self.endpoint.chars().any(char::is_whitespace)
            || self.bucket.trim().is_empty()
            || self.bucket.chars().any(char::is_whitespace)
            || self.bucket.chars().any(char::is_control)
            || self.key.is_empty()
            || self.version_id.trim().is_empty()
            || self.version_id.trim() == "null"
        {
            return Err(Error::message(
                "native storage verification requires a nonempty exact storage scope",
            ));
        }
        if !matches!(
            self.method.as_str(),
            "verified-upload" | "provider-checksum" | "verified-read" | "verified-copy"
        ) {
            return Err(Error::message("invalid native storage verification method"));
        }
        Ok(())
    }
}

impl Manifest {
    pub fn parse(raw: &str, origin: &str) -> Result<Self> {
        let manifest: Self = serde_json::from_str(raw).map_err(|error| {
            Error::message(format!("invalid native storage metadata {origin}: {error}"))
        })?;
        manifest.validate(origin)?;
        Ok(manifest)
    }

    pub fn serialize(&self) -> Result<String> {
        self.validate("native storage manifest")?;
        let mut value = self.clone();
        if let Some(entries) = &mut value.entries {
            entries.sort_by(|a, b| a.path.cmp(&b.path));
        }
        serde_json::to_string_pretty(&value)
            .map(|raw| format!("{raw}\n"))
            .map_err(|error| Error::message(error.to_string()))
    }

    pub fn validate(&self, origin: &str) -> Result<()> {
        if !matches!(self.schema_version, 1 | SCHEMA_VERSION) {
            return Err(Error::message(format!(
                "unsupported native storage schema {}: {origin}",
                self.schema_version
            )));
        }
        if repo_path(&self.path, "native storage output")? != self.path || self.path.contains('/') {
            return Err(Error::message(format!(
                "native storage output must name its adjacent boundary: {origin}"
            )));
        }
        if let Some(boundary) = origin.strip_suffix(SUFFIX) {
            let expected = std::path::Path::new(boundary)
                .file_name()
                .and_then(|name| name.to_str());
            if expected != Some(self.path.as_str()) {
                return Err(Error::message(format!(
                    "native storage path does not match its sidecar boundary: {origin}"
                )));
            }
        }
        self.checksum.validate_logical()?;
        validate_version(self.version.as_ref(), self.schema_version, self.size)?;
        match self.kind {
            Kind::File if self.entries.is_some() => {
                return Err(Error::message(
                    "file storage metadata cannot contain directory entries",
                ));
            }
            Kind::File => {}
            Kind::Directory => {
                if self.version.is_some() {
                    return Err(Error::message(
                        "directory storage versions belong to individual entries",
                    ));
                }
                let entries = self.entries.as_ref().ok_or_else(|| {
                    Error::message("directory storage metadata requires a complete entry list")
                })?;
                let mut seen = BTreeSet::new();
                let mut size = 0u64;
                for entry in entries {
                    if repo_path(&entry.path, "native storage entry")? != entry.path
                        || !seen.insert(entry.path.clone())
                    {
                        return Err(Error::message(
                            "invalid or duplicate native storage entry path",
                        ));
                    }
                    entry.checksum.validate_logical()?;
                    if entry.checksum.algorithm != self.checksum.algorithm {
                        return Err(Error::message(
                            "native storage entries must use the boundary checksum algorithm",
                        ));
                    }
                    validate_version(entry.version.as_ref(), self.schema_version, entry.size)?;
                    size = size
                        .checked_add(entry.size)
                        .ok_or_else(|| Error::message("native storage entry sizes overflow"))?;
                }
                for path in &seen {
                    let mut parent = std::path::Path::new(path).parent();
                    while let Some(value) = parent {
                        if value.to_str().is_some_and(|parent| seen.contains(parent)) {
                            return Err(Error::message(
                                "native storage entries contain overlapping file paths",
                            ));
                        }
                        parent = value.parent();
                    }
                }
                if size != self.size || directory_digest(entries)? != self.checksum.digest {
                    return Err(Error::message(format!(
                        "native storage directory checksum or size mismatch: {origin}"
                    )));
                }
            }
        }
        Ok(())
    }

    pub fn clear_versions(&mut self) -> bool {
        let mut changed = self.version.take().is_some();
        for entry in self.entries.iter_mut().flatten() {
            changed |= entry.version.take().is_some();
        }
        changed
    }
}

fn validate_version(version: Option<&Version>, schema_version: u32, size: u64) -> Result<()> {
    if version.is_some_and(|v| {
        v.id.trim().is_empty()
            || v.id.trim() == "null"
            || v.etag.as_deref().is_some_and(|etag| etag.trim().is_empty())
    }) {
        return Err(Error::message(
            "native storage exact version fields must be nonempty",
        ));
    }
    if let Some(version) = version {
        match (schema_version, &version.verification) {
            (1, Some(_)) => {
                return Err(Error::message(
                    "schema 1 native storage versions cannot contain verification evidence",
                ));
            }
            (SCHEMA_VERSION, None) => {
                return Err(Error::message(
                    "schema 2 native storage versions require verification evidence",
                ));
            }
            (SCHEMA_VERSION, Some(verification)) => {
                verification.validate()?;
                if verification.version_id != version.id {
                    return Err(Error::message(
                        "native storage verification does not match its exact version ID",
                    ));
                }
                if verification.size != size {
                    return Err(Error::message(
                        "native storage verification size does not match its content entry",
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Hash only the immutable directory content description, never remote version bindings.
pub fn directory_digest(entries: &[Entry]) -> Result<String> {
    let mut rows: Vec<_> = entries
        .iter()
        .map(|entry| {
            (
                &entry.path,
                &entry.checksum.algorithm,
                &entry.checksum.digest,
                entry.size,
            )
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(b.0));
    let raw = serde_json::to_vec(&rows).map_err(|error| Error::message(error.to_string()))?;
    Ok(crate::hex::encode_lower(Md5::digest(raw)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_manifest_rejects_unknown_fields_and_future_schemas() {
        let raw = r#"{"schema_version":1,"path":"data","kind":"file","checksum":{"algorithm":"md5","digest":"0cc175b9c0f1b6a831c399e269772661"},"size":1}"#;
        assert!(Manifest::parse(raw, "data.wm-storage.json").is_ok());
        assert!(
            Manifest::parse(
                &raw.replace("\"size\":1", "\"size\":1,\"cloud\":{}"),
                "data.wm-storage.json"
            )
            .is_err()
        );
        assert!(
            Manifest::parse(
                &raw.replace("\"schema_version\":1", "\"schema_version\":3"),
                "data.wm-storage.json"
            )
            .is_err()
        );
    }

    #[test]
    fn directory_identity_excludes_versions_and_includes_physical_size() {
        let mut entries = vec![Entry {
            path: "a".into(),
            checksum: Checksum {
                algorithm: "md5".into(),
                digest: "0cc175b9c0f1b6a831c399e269772661".into(),
            },
            size: 1,
            version: None,
        }];
        let first = directory_digest(&entries).unwrap();
        entries[0].version = Some(Version {
            id: "exact-version".into(),
            etag: None,
            verification: Some(verification()),
        });
        assert_eq!(first, directory_digest(&entries).unwrap());
        entries[0].size = 2;
        assert_ne!(first, directory_digest(&entries).unwrap());
    }

    fn verification() -> Verification {
        Verification {
            endpoint: "https://s3.example.invalid".into(),
            bucket: "bucket".into(),
            key: "prefix/task/data".into(),
            version_id: "exact-version".into(),
            checksum: Checksum {
                algorithm: "sha256".into(),
                digest: "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb".into(),
            },
            size: 1,
            method: "provider-checksum".into(),
        }
    }

    fn file_manifest(schema_version: u32, proof: Option<Verification>) -> Manifest {
        Manifest {
            schema_version,
            path: "data".into(),
            kind: Kind::File,
            checksum: Checksum {
                algorithm: "md5".into(),
                digest: "0cc175b9c0f1b6a831c399e269772661".into(),
            },
            size: 1,
            version: Some(Version {
                id: "exact-version".into(),
                etag: Some("etag".into()),
                verification: proof,
            }),
            entries: None,
        }
    }

    #[test]
    fn schema_two_binds_raw_checksum_to_exact_storage_scope() {
        let manifest = file_manifest(SCHEMA_VERSION, Some(verification()));
        let serialized = manifest.serialize().unwrap();
        assert_eq!(
            Manifest::parse(&serialized, "task/data.wm-storage.json").unwrap(),
            manifest
        );
        let mut changed_scope = manifest.clone();
        changed_scope
            .version
            .as_mut()
            .unwrap()
            .verification
            .as_mut()
            .unwrap()
            .bucket = "other-bucket".into();
        assert_ne!(manifest, changed_scope);
        changed_scope.validate("task/data.wm-storage.json").unwrap();
        let unknown = serialized.replace(
            "\"method\": \"provider-checksum\"",
            "\"method\": \"provider-checksum\", \"trusted\": true",
        );
        assert!(Manifest::parse(&unknown, "task/data.wm-storage.json").is_err());
        let missing_version = serialized.replace("    \"version_id\": \"exact-version\",\n", "");
        // A schema 2 receipt must carry its own exact ID; omitting it cannot
        // silently follow an edited enclosing version binding.
        assert!(Manifest::parse(&missing_version, "task/data.wm-storage.json").is_err());
        let mut changed_version = manifest;
        changed_version.version.as_mut().unwrap().id = "other-version".into();
        assert!(changed_version.serialize().is_err());
    }

    #[test]
    fn schemas_keep_legacy_versions_distinct_from_verified_versions() {
        let legacy = file_manifest(1, None);
        assert!(legacy.serialize().is_ok());
        assert!(file_manifest(1, Some(verification())).serialize().is_err());
        assert!(file_manifest(SCHEMA_VERSION, None).serialize().is_err());
        let mut unbound = file_manifest(SCHEMA_VERSION, None);
        unbound.version = None;
        assert!(unbound.serialize().is_ok());
        for invalid in ["", " ", "null"] {
            let mut bound = file_manifest(SCHEMA_VERSION, Some(verification()));
            bound.version.as_mut().unwrap().id = invalid.into();
            assert!(
                bound.serialize().is_err(),
                "accepted version id {invalid:?}"
            );
        }
        for schema in [0, SCHEMA_VERSION + 1] {
            assert!(file_manifest(schema, None).serialize().is_err());
        }
    }

    #[test]
    fn verification_requires_raw_sha256_size_and_public_exact_scope() {
        let proof = verification();
        let mut invalid_proofs = Vec::new();
        let mut invalid = proof.clone();
        invalid.size += 1;
        invalid_proofs.push(invalid);
        let mut invalid = proof.clone();
        invalid.checksum.algorithm = "md5-dos2unix".into();
        invalid.checksum.digest = "0cc175b9c0f1b6a831c399e269772661".into();
        invalid_proofs.push(invalid);
        let mut invalid = proof.clone();
        invalid.checksum.digest.make_ascii_uppercase();
        invalid_proofs.push(invalid);
        let mut invalid = proof.clone();
        invalid.checksum.digest.pop();
        invalid_proofs.push(invalid);
        for endpoint in [
            "",
            "file:///tmp/storage",
            "https://user:secret@example.invalid",
            "https://example.invalid?secret=value",
            "https://example.invalid#fragment",
            " https://example.invalid",
        ] {
            let mut invalid = proof.clone();
            invalid.endpoint = endpoint.into();
            invalid_proofs.push(invalid);
        }
        for bucket in ["", " ", "bucket with spaces", "bucket\n"] {
            let mut invalid = proof.clone();
            invalid.bucket = bucket.into();
            invalid_proofs.push(invalid);
        }
        let mut invalid = proof.clone();
        invalid.key.clear();
        invalid_proofs.push(invalid);
        for version_id in ["", " ", "null", "other-version"] {
            let mut invalid = proof.clone();
            invalid.version_id = version_id.into();
            invalid_proofs.push(invalid);
        }
        let mut invalid = proof;
        invalid.method = "local-checksum".into();
        invalid_proofs.push(invalid);
        for invalid in invalid_proofs {
            assert!(
                file_manifest(SCHEMA_VERSION, Some(invalid.clone()))
                    .serialize()
                    .is_err(),
                "accepted invalid verification {invalid:?}"
            );
        }
    }

    #[test]
    fn directory_entries_each_require_their_own_verification() {
        let mut entry = Entry {
            path: "a".into(),
            checksum: Checksum {
                algorithm: "md5".into(),
                digest: "0cc175b9c0f1b6a831c399e269772661".into(),
            },
            size: 1,
            version: Some(Version {
                id: "exact-version".into(),
                etag: None,
                verification: Some(verification()),
            }),
        };
        let mut manifest = Manifest {
            schema_version: SCHEMA_VERSION,
            path: "data".into(),
            kind: Kind::Directory,
            checksum: Checksum {
                algorithm: "md5".into(),
                digest: directory_digest(&[entry.clone()]).unwrap(),
            },
            size: 1,
            version: None,
            entries: Some(vec![entry.clone()]),
        };
        manifest.validate("data.wm-storage.json").unwrap();
        entry.version.as_mut().unwrap().verification = None;
        manifest.entries = Some(vec![entry]);
        assert!(manifest.validate("data.wm-storage.json").is_err());
        assert!(manifest.clear_versions());
        manifest.validate("data.wm-storage.json").unwrap();
    }

    #[test]
    fn sha256_proof_does_not_change_legacy_logical_cache_algorithms() {
        let mut manifest = file_manifest(SCHEMA_VERSION, Some(verification()));
        manifest.checksum = verification().checksum;
        assert!(manifest.serialize().is_err());
        for method in [
            "verified-upload",
            "provider-checksum",
            "verified-read",
            "verified-copy",
        ] {
            let mut proof = verification();
            proof.method = method.into();
            proof.validate().unwrap();
        }
    }
}
