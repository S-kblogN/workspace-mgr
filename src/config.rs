use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::path::reject_symlink_traversal;

pub const CONFIG_NAME: &str = ".workspace-mgr.toml";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub git: GitConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3: Option<S3Config>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitConfig {
    pub remote: String,
    pub branch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S3Config {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum StorageTarget {
    Git,
    S3,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            remote: "origin".to_owned(),
            branch: "main".to_owned(),
        }
    }
}

impl Config {
    pub fn path(repo: &GitRepo) -> PathBuf {
        repo.root.join(CONFIG_NAME)
    }

    pub fn load(repo: &GitRepo) -> Result<Self> {
        reject_symlink_traversal(&repo.root, CONFIG_NAME, "repository configuration")?;
        Self::load_path(&Self::path(repo))
    }

    pub fn load_compatible(repo: &GitRepo) -> Result<Self> {
        let config = Self::load(repo)?;
        repo.validate_remote_name(&config.git.remote)?;
        repo.validate_branch(&config.git.branch)?;
        repo.validate_branch(&format!(
            "{}workspace-mgr-probe",
            crate::policy::TASK_BRANCH_PREFIX
        ))?;
        Ok(config)
    }

    pub fn load_path(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path).at(path)?;
        let config: Self = toml::from_str(&raw).map_err(|source| Error::Toml {
            path: path.to_path_buf(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    pub fn render(&self) -> Result<String> {
        toml::to_string_pretty(self)
            .map_err(|error| Error::message(format!("failed to render config: {error}")))
    }

    pub fn validate(&self) -> Result<()> {
        for (field, value) in [
            ("git.remote", &self.git.remote),
            ("git.branch", &self.git.branch),
        ] {
            if value.trim().is_empty() || value.contains('\n') {
                return Err(Error::message(format!(
                    "{field} must be a non-empty single-line string"
                )));
            }
        }
        validate_remote_name(&self.git.remote)?;
        if let Some(s3) = &self.s3 {
            validate_s3_url("s3.url", &s3.url)?;
            if let Some(endpoint) = &s3.endpoint_url {
                validate_endpoint_url("s3.endpoint_url", endpoint)?;
            }
        }
        Ok(())
    }

    pub fn s3_enabled(&self) -> bool {
        self.s3.is_some()
    }

    pub fn requires_object_versioning(&self) -> bool {
        self.s3
            .as_ref()
            .is_some_and(|s3| s3.url.starts_with("s3://"))
    }
}

fn validate_remote_name(value: &str) -> Result<()> {
    if value.starts_with('-')
        || value.chars().any(char::is_whitespace)
        || value.chars().any(char::is_control)
    {
        return Err(Error::message(
            "git.remote must be a safe Git remote name, not an option or URL",
        ));
    }
    Ok(())
}

fn validate_s3_url(field: &str, value: &str) -> Result<()> {
    validate_public_location(field, value)?;
    if value.starts_with("s3://") {
        let authority = value
            .trim_start_matches("s3://")
            .split('/')
            .next()
            .unwrap_or_default();
        if authority.is_empty() {
            return Err(Error::message(format!(
                "{field} must name a non-empty S3 bucket"
            )));
        }
        return Ok(());
    }
    #[cfg(feature = "test-storage")]
    {
        if !value.contains("://") {
            return Ok(());
        }
    }
    Err(Error::message(format!(
        "{field} must use s3://; filesystem storage is available only in test builds"
    )))
}

fn validate_endpoint_url(field: &str, value: &str) -> Result<()> {
    validate_public_location(field, value)?;
    if !value.starts_with("https://") && !value.starts_with("http://") {
        return Err(Error::message(format!(
            "{field} must use https:// or http://"
        )));
    }
    let authority = value
        .split_once("://")
        .map(|(_, remainder)| remainder.split('/').next().unwrap_or_default())
        .unwrap_or_default();
    if authority.is_empty() {
        return Err(Error::message(format!(
            "{field} must name a non-empty endpoint host"
        )));
    }
    Ok(())
}

fn validate_public_location(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty()
        || value != value.trim()
        || value.chars().any(char::is_control)
        || value.chars().any(char::is_whitespace)
    {
        return Err(Error::message(format!(
            "{field} must be a non-empty value without whitespace"
        )));
    }
    if value.contains(['?', '#']) {
        return Err(Error::message(format!(
            "{field} must not contain a query or fragment because tracked locations cannot contain credentials"
        )));
    }
    if let Some((_, authority_and_path)) = value.split_once("://") {
        let authority = authority_and_path.split('/').next().unwrap_or_default();
        if authority.contains('@') {
            return Err(Error::message(format!(
                "{field} must not contain embedded credentials; use environment credentials or ignored local configuration"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_option_like_remotes_and_credential_bearing_locations() {
        let mut config = Config::default();
        config.git.remote = "--upload-pack=printf injected".to_owned();
        assert!(config.validate().is_err());

        config.git.remote = "origin".to_owned();
        config.s3 = Some(S3Config {
            url: "s3://bucket/prefix?X-Amz-Signature=secret".to_owned(),
            endpoint_url: None,
        });
        assert!(config.validate().is_err());

        config.s3 = Some(S3Config {
            url: "s3://bucket/prefix".to_owned(),
            endpoint_url: Some("https://user:secret@example.invalid".to_owned()),
        });
        assert!(config.validate().is_err());
    }

    #[cfg(not(feature = "test-storage"))]
    #[test]
    fn production_build_rejects_filesystem_storage() {
        let mut config = Config::default();
        config.s3 = Some(S3Config {
            url: "/tmp/test-storage".to_owned(),
            endpoint_url: None,
        });
        assert!(config.validate().is_err());
    }
}
