//! Versioned native storage sidecars. The wire format has no external-engine fields.
use std::collections::BTreeSet;

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::path::repo_path;

pub const SUFFIX: &str = ".wm-storage.json";
pub const SCHEMA_VERSION: u32 = 1;

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
        if !matches!(self.algorithm.as_str(), "md5" | "md5-dos2unix")
            || self.digest.len() != 32
            || !self
                .digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::message("invalid native storage checksum"));
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
        if self.schema_version != SCHEMA_VERSION {
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
        self.checksum.validate()?;
        validate_version(self.version.as_ref())?;
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
                    entry.checksum.validate()?;
                    if entry.checksum.algorithm != self.checksum.algorithm {
                        return Err(Error::message(
                            "native storage entries must use the boundary checksum algorithm",
                        ));
                    }
                    validate_version(entry.version.as_ref())?;
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

fn validate_version(version: Option<&Version>) -> Result<()> {
    if version.is_some_and(|v| {
        v.id.trim().is_empty()
            || v.id.trim() == "null"
            || v.etag.as_deref().is_some_and(|etag| etag.trim().is_empty())
    }) {
        return Err(Error::message(
            "native storage exact version fields must be nonempty",
        ));
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
                &raw.replace("\"schema_version\":1", "\"schema_version\":2"),
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
            id: "v1".into(),
            etag: None,
        });
        assert_eq!(first, directory_digest(&entries).unwrap());
        entries[0].size = 2;
        assert_ne!(first, directory_digest(&entries).unwrap());
    }
}
