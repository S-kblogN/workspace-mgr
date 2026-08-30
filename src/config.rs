use std::fs;
use std::path::{Path, PathBuf};

use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;

pub const CONFIG_NAME: &str = ".workspace-mgr.toml";
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    Standard,
    SharedCheckout,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    pub required_cli: String,
    pub profile: Profile,
    pub publication: PublicationConfig,
    #[serde(default)]
    pub tasks: TaskConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub agent: AgentConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationConfig {
    pub remote: String,
    pub base_branch: String,
    pub shared_checkout_branch: String,
    pub branch_prefix: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TaskConfig {
    pub enabled: bool,
    pub directory_pattern: String,
    pub manifest_name: String,
    pub require_readme: bool,
    pub draft_pull_request: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    pub default: StorageDefault,
    pub auto_s3_above_bytes: u64,
    pub s3: Option<S3Config>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S3Config {
    pub url: String,
    pub endpoint_url: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum StorageDefault {
    #[default]
    Auto,
    Git,
    S3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum StorageTarget {
    Git,
    S3,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    pub modules: Vec<String>,
}

impl Default for TaskConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            directory_pattern: "%Y%m%d-%H%M%S-{slug}".to_owned(),
            manifest_name: ".workspace-mgr-task.toml".to_owned(),
            require_readme: true,
            draft_pull_request: true,
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            modules: vec![
                "scope".to_owned(),
                "publication".to_owned(),
                "artifact-hygiene".to_owned(),
                "storage".to_owned(),
            ],
        }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            default: StorageDefault::Auto,
            auto_s3_above_bytes: 10_485_760,
            s3: None,
        }
    }
}

impl StorageConfig {
    pub fn s3_enabled(&self) -> bool {
        self.s3.is_some()
    }

    pub fn requires_object_versioning(&self) -> bool {
        self.s3
            .as_ref()
            .is_some_and(|s3| s3.url.starts_with("s3://"))
    }
}

impl Config {
    pub fn defaults(profile: Profile) -> Self {
        let shared = profile == Profile::SharedCheckout;
        let mut modules = AgentConfig::default().modules;
        if shared {
            modules.push("shared-checkout".to_owned());
        }
        Self {
            schema_version: SCHEMA_VERSION,
            required_cli: ">=0.1.0-alpha.1,<0.2.0".to_owned(),
            profile,
            publication: PublicationConfig {
                remote: "origin".to_owned(),
                base_branch: "main".to_owned(),
                shared_checkout_branch: "main".to_owned(),
                branch_prefix: "codex/".to_owned(),
            },
            tasks: TaskConfig::default(),
            storage: StorageConfig::default(),
            agent: AgentConfig { modules },
        }
    }

    pub fn path(repo: &GitRepo) -> PathBuf {
        repo.root.join(CONFIG_NAME)
    }

    pub fn load(repo: &GitRepo) -> Result<Self> {
        Self::load_path(&Self::path(repo))
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
        if self.schema_version != SCHEMA_VERSION {
            return Err(Error::message(format!(
                "unsupported config schema {}, expected {}",
                self.schema_version, SCHEMA_VERSION
            )));
        }
        VersionReq::parse(&self.required_cli).map_err(|error| {
            Error::message(format!("invalid required_cli requirement: {error}"))
        })?;
        for (field, value) in [
            ("publication.remote", &self.publication.remote),
            ("publication.base_branch", &self.publication.base_branch),
            (
                "publication.shared_checkout_branch",
                &self.publication.shared_checkout_branch,
            ),
            ("publication.branch_prefix", &self.publication.branch_prefix),
            ("tasks.directory_pattern", &self.tasks.directory_pattern),
            ("tasks.manifest_name", &self.tasks.manifest_name),
        ] {
            if value.trim().is_empty() || value.contains('\n') {
                return Err(Error::message(format!(
                    "{field} must be a non-empty single-line string"
                )));
            }
        }
        if !self.tasks.directory_pattern.contains("{slug}") {
            return Err(Error::message(
                "tasks.directory_pattern must contain {slug}",
            ));
        }
        if self.tasks.manifest_name.contains('/') || self.tasks.manifest_name == ".git" {
            return Err(Error::message(
                "tasks.manifest_name must be a file name, not a path",
            ));
        }
        if self.storage.auto_s3_above_bytes == 0 {
            return Err(Error::message(
                "storage.auto_s3_above_bytes must be positive",
            ));
        }
        if self.storage.default == StorageDefault::S3 && self.storage.s3.is_none() {
            return Err(Error::message(
                "storage.default = \"s3\" requires [storage.s3]",
            ));
        }
        if let Some(s3) = &self.storage.s3 {
            validate_public_url("storage.s3.url", &s3.url)?;
            if let Some(endpoint) = &s3.endpoint_url {
                validate_public_url("storage.s3.endpoint_url", endpoint)?;
            }
        }
        let supported = [
            "scope",
            "shared-checkout",
            "publication",
            "artifact-hygiene",
            "storage",
            "infrastructure",
        ];
        let mut seen = std::collections::BTreeSet::new();
        for module in &self.agent.modules {
            if !supported.contains(&module.as_str()) {
                return Err(Error::message(format!(
                    "unsupported agent module {module:?}"
                )));
            }
            if !seen.insert(module) {
                return Err(Error::message(format!("duplicate agent module {module:?}")));
            }
        }
        Ok(())
    }

    pub fn version_matches(&self) -> Result<bool> {
        let requirement = VersionReq::parse(&self.required_cli).map_err(|error| {
            Error::message(format!("invalid required_cli requirement: {error}"))
        })?;
        let version = Version::parse(env!("CARGO_PKG_VERSION")).map_err(|error| {
            Error::message(format!("invalid compiled package version: {error}"))
        })?;
        Ok(requirement.matches(&version))
    }
}

fn validate_public_url(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() || value.contains(['\n', '\r']) {
        return Err(Error::message(format!(
            "{field} must be a non-empty single-line value"
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
