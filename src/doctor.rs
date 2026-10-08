use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use semver::Version;
use serde::Serialize;

use crate::config::{
    CONFIG_NAME, Config, cli_version_satisfies, declared_minimum_cli_version,
    installed_cli_version, minimum_cli_version_at,
};
use crate::error::Result;
use crate::git::GitRepo;
use crate::path::reject_symlink_traversal;
use crate::process::command_exists;
use crate::scaffold;
use crate::storage_metadata;
use crate::task_catalog::{DoctorSelection, DoctorTask};

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub status: String,
    pub repo: String,
    pub cli_version: String,
    pub checks: Vec<DoctorCheck>,
    pub(crate) tasks: Vec<DoctorTask>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) storage: Option<crate::doctor_storage::AuditReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorCheck {
    pub name: String,
    pub status: String,
    pub detail: String,
}

impl DoctorReport {
    pub fn healthy(&self) -> bool {
        self.checks.iter().all(|check| check.status == "ok")
    }
}

pub fn inspect(path: &Path, selector: Option<&str>) -> Result<DoctorReport> {
    let repo = GitRepo::discover(path)?;
    let mut checks = Vec::new();
    let mut storage = None;
    let mut repository_runtime = command_check("git", true);
    repository_runtime.name = "repository-runtime".to_owned();
    checks.push(repository_runtime);

    let config_path = repo.root.join(CONFIG_NAME);
    // A repository that requires a newer CLI is reported below instead of
    // stopping the diagnosis.
    let config = match Config::load_compatible_ignoring_cli_requirement(&repo) {
        Ok(config) => {
            checks.push(DoctorCheck {
                name: "repository-config".to_owned(),
                status: "ok".to_owned(),
                detail: config_path.display().to_string(),
            });
            Some(config)
        }
        Err(error) => {
            checks.push(DoctorCheck {
                name: "repository-config".to_owned(),
                status: "error".to_owned(),
                detail: error.to_string(),
            });
            None
        }
    };
    if let Some(check) = cli_version_check(&repo, config.as_ref(), &installed_cli_version()) {
        checks.push(check);
    }

    let selection = if config.is_some() {
        match crate::task_catalog::select_for_doctor(&repo.root, selector) {
            Ok(selection) => selection,
            Err(error) if selector.is_some() => return Err(error),
            Err(error) => {
                checks.push(DoctorCheck {
                    name: "task-discovery".into(),
                    status: "error".into(),
                    detail: error.to_string(),
                });
                DoctorSelection {
                    tasks: Vec::new(),
                    warnings: Vec::new(),
                }
            }
        }
    } else {
        DoctorSelection {
            tasks: Vec::new(),
            warnings: Vec::new(),
        }
    };
    for task in &selection.tasks {
        checks.push(DoctorCheck {
            name: format!("task-metadata:{}", task.name),
            status: if task.diagnostic.is_some() {
                "error"
            } else {
                "ok"
            }
            .into(),
            detail: task.diagnostic.clone().unwrap_or_else(|| {
                task.path.clone().unwrap_or_else(|| {
                    task.manifest
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "infrastructure task".into())
                })
            }),
        });
    }
    for warning in &selection.warnings {
        checks.push(DoctorCheck {
            name: "task-discovery".into(),
            status: "error".into(),
            detail: format!("{}: {}", warning.path.display(), warning.message),
        });
    }

    if let Some(config) = &config {
        checks.push(match scaffold::validate_owned_files(&repo, config) {
            Ok(()) => DoctorCheck {
                name: "repository-scaffold".to_owned(),
                status: "ok".to_owned(),
                detail: "product-owned files match the installed CLI".to_owned(),
            },
            Err(error) => DoctorCheck {
                name: "repository-scaffold".to_owned(),
                status: "error".to_owned(),
                detail: error.to_string(),
            },
        });
        let head = repo
            .current_branch()?
            .unwrap_or_else(|| "detached".to_owned());
        let expected = &config.git.branch;
        let branch_ok = head == *expected;
        checks.push(DoctorCheck {
            name: "checkout-branch".to_owned(),
            status: if branch_ok { "ok" } else { "error" }.to_owned(),
            detail: format!("current {head}, configured shared branch {expected}"),
        });
        let identity = repo.run_unchecked(["var", "GIT_AUTHOR_IDENT"])?;
        checks.push(DoctorCheck {
            name: "publication-identity".to_owned(),
            status: if identity.success() { "ok" } else { "error" }.to_owned(),
            detail: if identity.success() {
                "publication author name and email are configured".to_owned()
            } else {
                "publication author name or email is not configured".to_owned()
            },
        });
        let remote_name_ok = repo.validate_remote_name(&config.git.remote).is_ok();
        let remote = if remote_name_ok {
            repo.run_unchecked(["remote", "get-url", "--", &config.git.remote])?
        } else {
            crate::process::CommandOutput {
                code: 1,
                stdout: String::new(),
                stderr: "invalid configured remote name".to_owned(),
            }
        };
        checks.push(DoctorCheck {
            name: "publication-remote".to_owned(),
            status: if remote.success() { "ok" } else { "error" }.to_owned(),
            detail: if remote.success() {
                format!("configured remote {:?} exists", config.git.remote)
            } else {
                format!("configured remote {:?} does not exist", config.git.remote)
            },
        });

        if !config.s3_enabled() {
            let scopes = selection
                .tasks
                .iter()
                .flat_map(|task| task.scopes.iter().cloned())
                .collect::<Vec<_>>();
            match crate::doctor_storage::discover_pointers(&repo, &scopes, selector.is_none()) {
                Ok(pointers) if pointers.is_empty() => {}
                Ok(pointers) => checks.push(DoctorCheck {
                    name: "managed-storage-integrity".into(),
                    status: "error".into(),
                    detail: format!(
                        "managed-storage metadata exists without configured S3: {}",
                        pointers.join(", ")
                    ),
                }),
                Err(error) => checks.push(DoctorCheck {
                    name: "managed-storage-integrity".into(),
                    status: "error".into(),
                    detail: format!("storage metadata inspection could not complete: {error}"),
                }),
            }
        }
        if config.s3_enabled() {
            checks.push(match storage_metadata::require_runtime(&repo) {
                Ok(version) => DoctorCheck {
                    name: "managed-storage-runtime".to_owned(),
                    status: "ok".to_owned(),
                    detail: format!("internal engine {version}"),
                },
                Err(error) => DoctorCheck {
                    name: "managed-storage-runtime".to_owned(),
                    status: "error".to_owned(),
                    detail: error.to_string(),
                },
            });
            checks.push(match crate::native_s3::load_credentials_config(&repo) {
                Ok(_) => DoctorCheck {
                    name: "managed-storage-config".to_owned(),
                    status: "ok".to_owned(),
                    detail: "repository S3 configuration and optional local credentials are valid"
                        .to_owned(),
                },
                Err(error) => DoctorCheck {
                    name: "managed-storage-config".to_owned(),
                    status: "error".to_owned(),
                    detail: error.to_string(),
                },
            });
            if config.requires_object_versioning() {
                checks.push(
                    match storage_metadata::verify_object_versioning(&repo, config) {
                        Ok(detail) => DoctorCheck {
                            name: "managed-storage-object-versioning".to_owned(),
                            status: "ok".to_owned(),
                            detail: detail.to_string(),
                        },
                        Err(error) => DoctorCheck {
                            name: "managed-storage-object-versioning".to_owned(),
                            status: "error".to_owned(),
                            detail: error.to_string(),
                        },
                    },
                );
            }
            let local = crate::native_s3::credentials_path(&repo)?;
            if local.exists() {
                let relative = crate::native_s3::CREDENTIALS_NAME;
                let primary =
                    GitRepo::discover(local.parent().expect("credentials have a parent"))?;
                let ignored = primary.run_unchecked(["check-ignore", "--quiet", "--", relative])?;
                checks.push(DoctorCheck {
                    name: "managed-storage-local-secrets".to_owned(),
                    status: if ignored.code == 0 { "ok" } else { "error" }.to_owned(),
                    detail: if ignored.code == 0 {
                        format!("{relative} is ignored")
                    } else {
                        format!("{relative} exists but is not ignored")
                    },
                });
            }
            let scopes = selection
                .tasks
                .iter()
                .flat_map(|task| task.scopes.iter().cloned())
                .collect::<Vec<_>>();
            // The feature-gated filesystem fixture uses CAS keys, not the
            // logical versioned S3 layout. Production configuration only
            // accepts s3:// and always takes the full integrity audit below.
            if config
                .s3
                .as_ref()
                .is_some_and(|remote| remote.url.starts_with("s3://"))
            {
                let audit = retired_scopes(&repo, &selection.tasks, selector.is_none()).and_then(
                    |retired| {
                        crate::doctor_storage::inspect(&repo, &scopes, selector.is_none(), &retired)
                    },
                );
                match audit {
                    Ok(report) => {
                        checks.push(DoctorCheck {
                            name: "managed-storage-integrity".into(),
                            status: if report.issues.is_empty() {
                                "ok"
                            } else {
                                "error"
                            }
                            .into(),
                            detail: format!(
                                "{} expected objects, {} remote versions, {} differences",
                                report.expected_objects,
                                report.remote_versions,
                                report.issues.len(),
                            ),
                        });
                        for issue in &report.issues {
                            checks.push(DoctorCheck {
                                name: format!("storage:{}", issue.code),
                                status: "error".into(),
                                detail: format!("{}: {}", issue.path, issue.detail),
                            });
                        }
                        storage = Some(report);
                    }
                    Err(error) => checks.push(DoctorCheck {
                        name: "managed-storage-integrity".into(),
                        status: "error".into(),
                        detail: format!("storage audit could not complete: {error}"),
                    }),
                }
            }
        }
    }

    let status = if checks.iter().all(|check| check.status == "ok") {
        "ok"
    } else {
        "error"
    };
    Ok(DoctorReport {
        status: status.to_owned(),
        repo: repo.root.display().to_string(),
        cli_version: env!("CARGO_PKG_VERSION").to_owned(),
        checks,
        tasks: selection.tasks,
        storage,
    })
}

/// A single-task audit must also inspect its former prefixes. Local Git history
/// supplies stable task IDs across slug changes; archive receipts supply paths
/// even before the relocation has been committed. This never fetches a ref.
fn retired_scopes(repo: &GitRepo, tasks: &[DoctorTask], all: bool) -> Result<Vec<String>> {
    if all {
        return Ok(Vec::new());
    }
    let ids = tasks
        .iter()
        .filter_map(|task| task.id.as_deref())
        .collect::<BTreeSet<_>>();
    let timestamps = ids
        .iter()
        .filter_map(|id| {
            crate::manifest::parse_task_identity(crate::manifest::TaskKind::Deliverable, id)
                .ok()
                .map(|_| &id[..15])
        })
        .collect::<BTreeSet<_>>();
    if timestamps.is_empty() {
        return Ok(Vec::new());
    }
    let mut retired = BTreeSet::new();
    for task in tasks {
        if let Some(id) = &task.id
            && crate::manifest::parse_task_identity(crate::manifest::TaskKind::Deliverable, id)
                .is_ok()
        {
            retired.insert(id.clone());
        }
        if let Some(path) = &task.path
            && let Some(name) = path.rsplit('/').next()
        {
            retired.insert(name.to_owned());
        }
    }
    let scopes = tasks
        .iter()
        .flat_map(|task| task.scopes.iter().cloned())
        .collect::<Vec<_>>();
    let mut receipt_paths = repo
        .visible_paths(&scopes)?
        .into_iter()
        .filter(|path| path.ends_with(&format!("/{}", crate::archive_migration::RECEIPT_NAME)))
        .collect::<BTreeSet<_>>();
    for task in tasks {
        if let Some(path) = &task.path {
            receipt_paths.insert(format!("{path}/{}", crate::archive_migration::RECEIPT_NAME));
        }
    }
    for relative in receipt_paths {
        let metadata = match fs::symlink_metadata(repo.root.join(&relative)) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(crate::error::Error::Io {
                    path: repo.root.join(&relative),
                    source,
                });
            }
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(crate::error::Error::message(format!(
                "archive receipt must be a regular file: {relative}"
            )));
        }
        reject_symlink_traversal(&repo.root, &relative, "doctor archive receipt")?;
        let raw = fs::read_to_string(repo.root.join(&relative)).map_err(|source| {
            crate::error::Error::Io {
                path: repo.root.join(&relative),
                source,
            }
        })?;
        let receipt = serde_json::from_str::<serde_json::Value>(&raw).map_err(|error| {
            crate::error::Error::message(format!("invalid archive receipt {relative}: {error}"))
        })?;
        crate::archive_migration::validate(&relative, &receipt)?;
        if receipt["task_id"]
            .as_str()
            .is_some_and(|id| ids.contains(id))
        {
            collect_retired_receipt(&receipt, &mut retired, 0)?;
        }
    }
    let output = repo.run_bytes(
        [
            "log",
            "--all",
            "--format=",
            "--raw",
            "--no-abbrev",
            "--no-renames",
            "-z",
            "--",
            "**/.workspace-mgr-task.toml",
        ],
        None,
    )?;
    let mut records = output.stdout.split(|byte| *byte == 0);
    let mut seen = BTreeSet::new();
    while let Some(header) = records.next() {
        let header = String::from_utf8_lossy(header);
        let fields = header.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 5 || !fields[0].starts_with(':') {
            continue;
        }
        let Some(path) = records.next() else { break };
        let path = std::str::from_utf8(path)
            .map_err(|_| crate::error::Error::message("historical task path is not UTF-8"))?;
        if !Path::new(path)
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .is_some_and(|name| timestamps.iter().any(|stamp| name.starts_with(stamp)))
        {
            continue;
        }
        let blob = fields[3];
        if blob.bytes().all(|byte| byte == b'0') || !seen.insert((blob.to_owned(), path.to_owned()))
        {
            continue;
        }
        let raw = repo.run(["cat-file", "blob", blob])?.stdout;
        if let Ok(document) = toml::from_str::<toml::Value>(&raw)
            && document
                .get("id")
                .and_then(toml::Value::as_str)
                .is_some_and(|id| ids.contains(id))
            && let Some(parent) = Path::new(path).parent().and_then(Path::to_str)
        {
            retired.insert(crate::path::repo_path(parent, "historical task path")?);
        }
    }
    // An exact current path can disambiguate two local rows with the same ID.
    // Historical ID matches must not turn the other row's current scope into
    // an obsolete prefix belonging to this selection.
    let other_scopes = crate::task_catalog::select_for_doctor(&repo.root, None)?
        .tasks
        .into_iter()
        .filter(|other| {
            !tasks
                .iter()
                .any(|task| task.manifest == other.manifest && task.path == other.path)
        })
        .filter(|task| task.kind == crate::manifest::TaskKind::Deliverable)
        .filter_map(|task| task.path)
        .collect::<Vec<_>>();
    retired.retain(|prefix| {
        !other_scopes.iter().any(|scope| {
            prefix == scope
                || prefix.starts_with(&format!("{scope}/"))
                || scope.starts_with(&format!("{prefix}/"))
        })
    });
    Ok(retired.into_iter().collect())
}

fn collect_retired_receipt(
    receipt: &serde_json::Value,
    retired: &mut BTreeSet<String>,
    depth: usize,
) -> Result<()> {
    if depth >= 32 {
        return Err(crate::error::Error::message(
            "archive receipt history exceeds the supported nesting depth",
        ));
    }
    let destination = receipt["destination"]
        .as_str()
        .ok_or_else(|| crate::error::Error::message("archive receipt has no destination"))?;
    crate::archive_migration::validate(
        &format!("{destination}/{}", crate::archive_migration::RECEIPT_NAME),
        receipt,
    )?;
    if let Some(source) = receipt["source"].as_str() {
        retired.insert(source.to_owned());
    }
    if let Some(previous) = receipt
        .get("previous_receipt")
        .filter(|value| !value.is_null())
    {
        if previous["task_id"] != receipt["task_id"] {
            return Err(crate::error::Error::message(
                "archive receipt history changes task identity",
            ));
        }
        collect_retired_receipt(previous, retired, depth + 1)?;
    }
    Ok(())
}

/// Compares this CLI with the repository's `minimum_cli_version`. The
/// declaration is read leniently so a configuration written by a newer
/// release still reports it. The shared branch may already require more than
/// the checkout, so the declaration last fetched from it, at
/// `refs/remotes/<remote>/<branch>`, counts too; doctor itself never fetches.
fn cli_version_check(
    repo: &GitRepo,
    config: Option<&Config>,
    installed: &Version,
) -> Option<DoctorCheck> {
    reject_symlink_traversal(&repo.root, CONFIG_NAME, "repository configuration").ok()?;
    let raw = fs::read_to_string(repo.root.join(CONFIG_NAME)).ok()?;
    let local = declared_minimum_cli_version(&raw);
    let shared = config.and_then(|config| {
        let reference = format!("refs/remotes/{}/{}", config.git.remote, config.git.branch);
        let declared = minimum_cli_version_at(repo, &reference).ok()??;
        Some((
            format!("{}/{}", config.git.remote, config.git.branch),
            declared,
        ))
    });
    let (source, required) = match (local, shared) {
        (Some(local), Some((name, shared))) if shared.cmp_precedence(&local).is_gt() => {
            (name, shared)
        }
        (Some(local), _) => ("repository".to_owned(), local),
        (None, Some((name, shared))) => (name, shared),
        (None, None) => {
            return Some(DoctorCheck {
                name: "cli-version".to_owned(),
                status: "ok".to_owned(),
                detail: format!("installed {installed}, repository declares no minimum version"),
            });
        }
    };
    Some(DoctorCheck {
        name: "cli-version".to_owned(),
        status: if cli_version_satisfies(installed, &required) {
            "ok"
        } else {
            "error"
        }
        .to_owned(),
        detail: format!("installed {installed}, {source} requires {required}"),
    })
}

fn command_check(command: &str, required: bool) -> DoctorCheck {
    let exists = command_exists(command);
    DoctorCheck {
        name: format!("command:{command}"),
        status: if exists || !required { "ok" } else { "error" }.to_owned(),
        detail: if exists {
            which::which(command)
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| "available".to_owned())
        } else {
            "not found on PATH".to_owned()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: &str = "[git]\nremote = \"origin\"\nbranch = \"main\"\n";

    struct Fixture {
        _temp: tempfile::TempDir,
        repo: GitRepo,
    }

    impl Fixture {
        fn new(config: Option<&str>) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let repo = GitRepo {
                root: temp.path().to_path_buf(),
            };
            repo.run(["init", "-q", "-b", "main"]).unwrap();
            repo.run(["config", "user.name", "workspace-mgr test"])
                .unwrap();
            repo.run(["config", "user.email", "test@example.invalid"])
                .unwrap();
            if let Some(config) = config {
                fs::write(temp.path().join(CONFIG_NAME), config).unwrap();
            }
            Self { _temp: temp, repo }
        }

        /// Records `config` as the last fetched state of `origin/main`.
        fn fetched(&self, config: &str) {
            let blob = self
                .repo
                .run_bytes(["hash-object", "-w", "--stdin"], Some(config.as_bytes()))
                .unwrap();
            let blob = String::from_utf8_lossy(&blob.stdout).trim().to_owned();
            let tree = self
                .repo
                .run_bytes(
                    ["mktree"],
                    Some(format!("100644 blob {blob}\t{CONFIG_NAME}\n").as_bytes()),
                )
                .unwrap();
            let tree = String::from_utf8_lossy(&tree.stdout).trim().to_owned();
            let commit = self
                .repo
                .run(["commit-tree", &tree, "-m", "fetched"])
                .unwrap()
                .stdout
                .trim()
                .to_owned();
            self.repo
                .run(["update-ref", "refs/remotes/origin/main", &commit])
                .unwrap();
        }

        fn check(&self, installed: &str) -> Option<DoctorCheck> {
            let config = Config::default();
            cli_version_check(
                &self.repo,
                Some(&config),
                &Version::parse(installed).unwrap(),
            )
        }
    }

    fn declaring(version: &str) -> String {
        format!("minimum_cli_version = \"{version}\"\n\n{PLAIN}")
    }

    fn assert_check(check: Option<DoctorCheck>, status: &str, detail: &str) {
        let check = check.unwrap();
        assert_eq!(check.name, "cli-version");
        assert_eq!(
            (check.status.as_str(), check.detail.as_str()),
            (status, detail)
        );
    }

    #[test]
    fn cli_version_check_reports_the_repository_requirement() {
        assert!(Fixture::new(None).check("0.4.0").is_none());
        assert_check(
            Fixture::new(Some(PLAIN)).check("0.4.0"),
            "ok",
            "installed 0.4.0, repository declares no minimum version",
        );
        let met = Fixture::new(Some(&declaring("0.4.0")));
        assert_check(
            met.check("0.4.0"),
            "ok",
            "installed 0.4.0, repository requires 0.4.0",
        );
        // A release candidate meets its own release.
        assert_check(
            met.check("0.4.0-rc.1"),
            "ok",
            "installed 0.4.0-rc.1, repository requires 0.4.0",
        );
        assert_check(
            met.check("0.3.0"),
            "error",
            "installed 0.3.0, repository requires 0.4.0",
        );

        // Unknown fields from a newer release do not hide the requirement.
        let newer = Fixture::new(Some(
            "minimum_cli_version = \"99.0.0\"\n\n[future]\nsetting = true\n",
        ));
        assert_check(
            newer.check("0.4.0"),
            "error",
            "installed 0.4.0, repository requires 99.0.0",
        );
    }

    #[test]
    fn cli_version_check_includes_the_last_fetched_shared_branch() {
        // The checkout is stale: the shared branch was raised after it was
        // last refreshed.
        let stale = Fixture::new(Some(PLAIN));
        stale.fetched(&declaring("99.0.0"));
        assert_check(
            stale.check("0.4.0"),
            "error",
            "installed 0.4.0, origin/main requires 99.0.0",
        );
        stale.fetched(&declaring("0.4.0"));
        assert_check(
            stale.check("0.4.0"),
            "ok",
            "installed 0.4.0, origin/main requires 0.4.0",
        );

        // The higher of both declarations decides.
        let raised = Fixture::new(Some(&declaring("0.5.0")));
        raised.fetched(&declaring("0.4.0"));
        assert_check(
            raised.check("0.4.0"),
            "error",
            "installed 0.4.0, repository requires 0.5.0",
        );
        raised.fetched(&declaring("0.6.0"));
        assert_check(
            raised.check("0.5.0"),
            "error",
            "installed 0.5.0, origin/main requires 0.6.0",
        );
        raised.fetched(PLAIN);
        assert_check(
            raised.check("0.5.0"),
            "ok",
            "installed 0.5.0, repository requires 0.5.0",
        );

        // Without a readable configuration there is no shared branch to read.
        let unreadable = Fixture::new(Some(PLAIN));
        unreadable.fetched(&declaring("99.0.0"));
        assert_check(
            cli_version_check(&unreadable.repo, None, &Version::new(0, 4, 0)),
            "ok",
            "installed 0.4.0, repository declares no minimum version",
        );
    }
}
