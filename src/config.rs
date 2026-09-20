use std::fs;
use std::path::{Path, PathBuf};

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::path::reject_symlink_traversal;

pub const CONFIG_NAME: &str = ".workspace-mgr.toml";
pub const MINIMUM_CLI_VERSION_KEY: &str = "minimum_cli_version";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The oldest workspace-mgr release that can operate on this repository.
    /// workspace-mgr maintains it: publication raises it when the published
    /// tree starts using a newer task manifest schema, and nothing lowers it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_cli_version: Option<String>,
    pub git: GitConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3: Option<S3Config>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitConfig {
    pub remote: String,
    pub branch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    #[value(skip)]
    Local,
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
        config.validate_repository(repo)
    }

    /// Loads the configuration without refusing a repository that requires a
    /// newer workspace-mgr, so `doctor` can still report that requirement.
    pub fn load_compatible_ignoring_cli_requirement(repo: &GitRepo) -> Result<Self> {
        reject_symlink_traversal(&repo.root, CONFIG_NAME, "repository configuration")?;
        let path = Self::path(repo);
        let raw = fs::read_to_string(&path).at(&path)?;
        Self::parse_ignoring_cli_requirement(&raw, &path)?.validate_repository(repo)
    }

    fn validate_repository(self, repo: &GitRepo) -> Result<Self> {
        repo.validate_remote_name(&self.git.remote)?;
        repo.validate_branch(&self.git.branch)?;
        repo.validate_branch(&format!(
            "{}workspace-mgr-probe",
            crate::policy::TASK_BRANCH_PREFIX
        ))?;
        Ok(self)
    }

    pub fn load_path(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path).at(path)?;
        Self::parse(&raw, path)
    }

    /// Parses a repository configuration. A repository that declares a
    /// `minimum_cli_version` newer than this CLI is refused before anything
    /// else is interpreted, because newer releases may add fields this one
    /// does not know.
    pub fn parse(raw: &str, path: &Path) -> Result<Self> {
        Self::parse_by(raw, path, &installed_cli_version())
    }

    fn parse_by(raw: &str, path: &Path, installed: &Version) -> Result<Self> {
        require_supported_cli_by(raw, CONFIG_NAME, installed)?;
        Self::parse_ignoring_cli_requirement(raw, path)
    }

    pub fn parse_ignoring_cli_requirement(raw: &str, path: &Path) -> Result<Self> {
        let config: Self = toml::from_str(raw).map_err(|source| Error::Toml {
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
        if let Some(version) = &self.minimum_cli_version {
            parse_minimum_cli_version(version)?;
        }
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

/// Test builds may stand in for another release when comparing against
/// `minimum_cli_version`, so isolated tests can follow a repository that a
/// later release raised. Release builds ignore it.
pub const CLI_VERSION_OVERRIDE_ENV: &str = "WORKSPACE_MGR_TEST_CLI_VERSION";

/// This build's own version.
pub fn installed_cli_version() -> Version {
    cli_version_from_override(std::env::var(CLI_VERSION_OVERRIDE_ENV).ok().as_deref())
}

fn cli_version_from_override(value: Option<&str>) -> Version {
    #[cfg(feature = "test-storage")]
    if let Some(version) = value.and_then(|value| Version::parse(value.trim()).ok()) {
        return version;
    }
    #[cfg(not(feature = "test-storage"))]
    let _ = value;
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the package version is valid semver")
}

fn parse_minimum_cli_version(value: &str) -> Result<Version> {
    let version = Version::parse(value).map_err(|error| {
        Error::message(format!(
            "{MINIMUM_CLI_VERSION_KEY} must be a plain semantic version such as \"0.4.0\": {error}"
        ))
    })?;
    if !version.build.is_empty() {
        return Err(Error::message(format!(
            "{MINIMUM_CLI_VERSION_KEY} must not carry build metadata"
        )));
    }
    if !version.pre.is_empty() {
        return Err(Error::message(format!(
            "{MINIMUM_CLI_VERSION_KEY} must be a release version such as \"0.4.0\", not a pre-release"
        )));
    }
    Ok(version)
}

/// Reads `minimum_cli_version` without interpreting the rest of the
/// configuration, so configurations written by newer releases still yield it.
/// Anything unreadable counts as no declaration here; strict parsing reports
/// it.
pub fn declared_minimum_cli_version(raw: &str) -> Option<Version> {
    let table = toml::from_str::<toml::Table>(raw).ok()?;
    let value = table.get(MINIMUM_CLI_VERSION_KEY)?.as_str()?;
    Version::parse(value).ok()
}

/// Whether a workspace-mgr `installed` meets a `minimum_cli_version` of
/// `required`. Semantic-version precedence decides, except that a pre-release
/// meets a declaration of its own release: 0.4.0-rc.1 meets 0.4.0, so a
/// release candidate can operate on the repositories it raises. A pre-release
/// still sorts below its release everywhere else, so 0.4.0-rc.1 does not meet
/// 0.4.1 and 0.3.0 does not meet 0.4.0-rc.1.
pub fn cli_version_satisfies(installed: &Version, required: &Version) -> bool {
    if installed.cmp_precedence(required).is_ge() {
        return true;
    }
    !installed.pre.is_empty()
        && required.pre.is_empty()
        && (installed.major, installed.minor, installed.patch)
            == (required.major, required.minor, required.patch)
}

/// Refuses a repository whose configuration, read from `location`, requires
/// a newer workspace-mgr than `installed`.
fn require_supported_cli_by(raw: &str, location: &str, installed: &Version) -> Result<()> {
    match declared_minimum_cli_version(raw) {
        Some(required) if !cli_version_satisfies(installed, &required) => {
            Err(unsupported_cli_error(installed, &required, location))
        }
        _ => Ok(()),
    }
}

fn unsupported_cli_error(installed: &Version, required: &Version, location: &str) -> Error {
    Error::message(format!(
        "this repository requires workspace-mgr {required} or newer (`{MINIMUM_CLI_VERSION_KEY}` in {location}); this is workspace-mgr {installed}. Tell the user both versions and ask before updating with `cargo install --locked workspace-mgr`, then run `workspace-mgr setup`."
    ))
}

/// The configuration file committed at `revision`, if it is a readable blob.
pub(crate) fn config_at(repo: &GitRepo, revision: &str) -> Result<Option<String>> {
    let object = format!("{revision}:{CONFIG_NAME}");
    let kind = repo.run_unchecked(["cat-file", "-t", &object])?;
    if !kind.success() || kind.stdout.trim() != "blob" {
        return Ok(None);
    }
    Ok(Some(repo.run(["cat-file", "blob", &object])?.stdout))
}

/// The `minimum_cli_version` committed at `revision`, read leniently.
pub(crate) fn minimum_cli_version_at(repo: &GitRepo, revision: &str) -> Result<Option<Version>> {
    Ok(config_at(repo, revision)?
        .as_deref()
        .and_then(declared_minimum_cli_version))
}

/// Refuses a revision whose committed configuration requires a newer
/// workspace-mgr, and otherwise returns its declaration. `location` names
/// the revision for the user, such as `origin/main`.
pub(crate) fn require_supported_cli_at(
    repo: &GitRepo,
    revision: &str,
    location: &str,
) -> Result<Option<Version>> {
    require_supported_cli_at_by(repo, revision, location, &installed_cli_version())
}

fn require_supported_cli_at_by(
    repo: &GitRepo,
    revision: &str,
    location: &str,
    installed: &Version,
) -> Result<Option<Version>> {
    let declared = minimum_cli_version_at(repo, revision)?;
    if let Some(required) = &declared {
        if !cli_version_satisfies(installed, required) {
            return Err(unsupported_cli_error(
                installed,
                required,
                &format!("{CONFIG_NAME} on {location}"),
            ));
        }
    }
    Ok(declared)
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

    #[test]
    fn tracked_git_facts_are_required() {
        for raw in ["[git]\nbranch = \"main\"\n", "[git]\nremote = \"origin\"\n"] {
            assert!(
                toml::from_str::<Config>(raw).is_err(),
                "incomplete configuration was accepted: {raw:?}"
            );
        }
    }

    const PLAIN: &str = "[git]\nremote = \"origin\"\nbranch = \"main\"\n";

    fn declaring(version: &str) -> String {
        format!("minimum_cli_version = \"{version}\"\n\n{PLAIN}")
    }

    #[test]
    fn minimum_cli_version_renders_before_the_git_table() {
        let mut config = Config::default();
        assert_eq!(config.render().unwrap(), PLAIN);
        config.minimum_cli_version = Some("0.1.0".to_owned());
        let rendered = config.render().unwrap();
        assert_eq!(rendered, declaring("0.1.0"));
        let parsed = Config::parse(&rendered, Path::new(CONFIG_NAME)).unwrap();
        assert_eq!(parsed.minimum_cli_version.as_deref(), Some("0.1.0"));
        assert_eq!(parsed.render().unwrap(), rendered);
    }

    #[test]
    fn minimum_cli_version_must_be_a_plain_release_version() {
        for (invalid, error) in [
            ("0.4", "must be a plain semantic version"),
            ("v0.4.0", "must be a plain semantic version"),
            (" 0.4.0", "must be a plain semantic version"),
            ("latest", "must be a plain semantic version"),
            ("0.4.0+build.7", "must not carry build metadata"),
            ("0.1.0-alpha.1", "not a pre-release"),
        ] {
            let raw = declaring(invalid);
            let message = Config::parse_ignoring_cli_requirement(&raw, Path::new(CONFIG_NAME))
                .unwrap_err()
                .to_string();
            assert!(
                message.starts_with("minimum_cli_version ") && message.contains(error),
                "{invalid:?}: {message}"
            );
        }
        assert!(
            Config::parse_by(
                "minimum_cli_version = 4\n[git]\nremote = \"origin\"\nbranch = \"main\"\n",
                Path::new(CONFIG_NAME),
                &Version::new(0, 4, 0)
            )
            .is_err()
        );
    }

    fn version(value: &str) -> Version {
        Version::parse(value).unwrap()
    }

    #[test]
    fn a_pre_release_meets_its_own_release_and_nothing_newer() {
        for (installed, required, met) in [
            ("0.4.0", "0.4.0", true),
            ("0.4.1", "0.4.0", true),
            ("1.0.0", "0.4.0", true),
            ("0.3.0", "0.4.0", false),
            ("0.3.9", "0.4.0", false),
            // A release candidate operates on the repositories it raises.
            ("0.4.0-rc.1", "0.4.0", true),
            ("0.4.0-alpha", "0.4.0", true),
            ("0.4.1-rc.1", "0.4.0", true),
            // Otherwise a pre-release still sorts below its release.
            ("0.4.0-rc.1", "0.4.1", false),
            ("0.4.0-rc.1", "0.5.0", false),
            ("0.3.0-rc.1", "0.4.0", false),
            // Only newer releases write pre-release declarations, which plain
            // precedence compares.
            ("0.3.0", "0.4.0-rc.1", false),
            ("0.4.0-rc.1", "0.4.0-rc.2", false),
            ("0.4.0-rc.2", "0.4.0-rc.1", true),
            ("0.4.0", "0.4.0-rc.1", true),
        ] {
            assert_eq!(
                cli_version_satisfies(&version(installed), &version(required)),
                met,
                "{installed} meets {required}"
            );
        }
    }

    #[test]
    fn repositories_requiring_a_newer_cli_are_refused_before_strict_parsing() {
        let installed = version("0.4.0");
        let path = Path::new(CONFIG_NAME);
        assert!(Config::parse_by(&declaring("0.4.0"), path, &installed).is_ok());
        assert!(Config::parse_by(&declaring("0.4.0"), path, &version("0.4.0-rc.1")).is_ok());

        // A future release may add fields; the requirement still wins.
        let future = format!("{}[future]\nsetting = true\n", declaring("99.0.0"));
        let error = Config::parse_by(&future, path, &installed)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "this repository requires workspace-mgr 99.0.0 or newer (`minimum_cli_version` in .workspace-mgr.toml); this is workspace-mgr 0.4.0. Tell the user both versions and ask before updating with `cargo install --locked workspace-mgr`, then run `workspace-mgr setup`."
        );
        let error = Config::parse_by(&declaring("0.4.1"), path, &version("0.4.0-rc.1"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("requires workspace-mgr 0.4.1 or newer")
                && error.contains("this is workspace-mgr 0.4.0-rc.1"),
            "{error}"
        );
        assert_eq!(
            declared_minimum_cli_version(&future),
            Some(Version::new(99, 0, 0))
        );
        // Unreadable declarations are left to strict parsing.
        assert_eq!(declared_minimum_cli_version("not toml ["), None);
        assert_eq!(declared_minimum_cli_version(PLAIN), None);
        assert!(
            require_supported_cli_by("minimum_cli_version = \"x\"", CONFIG_NAME, &installed)
                .is_ok()
        );

        // The public entry points compare with this build's own version,
        // whatever a test environment substitutes for it.
        let running = installed_cli_version();
        let newer = Version::new(running.major + 1, 0, 0);
        assert!(!cli_version_satisfies(&running, &newer));
        assert!(cli_version_satisfies(&running, &running));
        let error = Config::parse(&declaring(&newer.to_string()), path)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&format!("this is workspace-mgr {running}.")),
            "{error}"
        );
    }

    #[test]
    fn committed_requirements_are_read_from_revisions() {
        let temp = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: temp.path().to_path_buf(),
        };
        repo.run(["init", "-q", "-b", "main"]).unwrap();
        repo.run(["config", "user.name", "workspace-mgr test"])
            .unwrap();
        repo.run(["config", "user.email", "test@example.invalid"])
            .unwrap();
        fs::write(temp.path().join("README.md"), "base\n").unwrap();
        repo.run(["add", "-A"]).unwrap();
        repo.run(["commit", "-q", "-m", "base"]).unwrap();
        let installed = version("0.4.0");
        assert_eq!(
            require_supported_cli_at_by(&repo, "HEAD", "origin/main", &installed).unwrap(),
            None
        );

        fs::write(temp.path().join(CONFIG_NAME), declaring("99.0.0")).unwrap();
        repo.run(["add", "-A"]).unwrap();
        repo.run(["commit", "-q", "-m", "require"]).unwrap();
        assert_eq!(
            minimum_cli_version_at(&repo, "HEAD").unwrap(),
            Some(Version::new(99, 0, 0))
        );
        let error = require_supported_cli_at_by(&repo, "HEAD", "origin/main", &installed)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "this repository requires workspace-mgr 99.0.0 or newer (`minimum_cli_version` in .workspace-mgr.toml on origin/main); this is workspace-mgr 0.4.0. Tell the user both versions and ask before updating with `cargo install --locked workspace-mgr`, then run `workspace-mgr setup`."
        );
        assert_eq!(
            require_supported_cli_at_by(&repo, "HEAD", "origin/main", &version("99.0.0-rc.1"))
                .unwrap(),
            Some(Version::new(99, 0, 0))
        );
        assert_eq!(
            require_supported_cli_at_by(&repo, "HEAD~1", "origin/main", &installed).unwrap(),
            None
        );
    }

    #[cfg(feature = "test-storage")]
    #[test]
    fn test_builds_may_stand_in_for_another_release() {
        let package = Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        assert_eq!(cli_version_from_override(None), package);
        assert_eq!(
            cli_version_from_override(Some(" 0.5.0 ")),
            Version::new(0, 5, 0)
        );
        assert_eq!(cli_version_from_override(Some("next")), package);
    }

    #[cfg(not(feature = "test-storage"))]
    #[test]
    fn production_build_ignores_cli_version_override() {
        assert_eq!(
            cli_version_from_override(Some("99.0.0")),
            Version::parse(env!("CARGO_PKG_VERSION")).unwrap()
        );
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
