use std::fs;
use std::path::Path;

use semver::Version;
use serde::Serialize;

use crate::config::{
    CONFIG_NAME, Config, cli_version_satisfies, declared_minimum_cli_version,
    installed_cli_version, minimum_cli_version_at,
};
use crate::dvc;
use crate::error::Result;
use crate::git::GitRepo;
use crate::manifest::{ResolvedTask, TaskKind};
use crate::path::reject_symlink_traversal;
use crate::process::command_exists;
use crate::scaffold;

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub status: String,
    pub repo: String,
    pub cli_version: String,
    pub checks: Vec<DoctorCheck>,
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

pub fn inspect(path: &Path) -> Result<DoctorReport> {
    let repo = GitRepo::discover(path)?;
    let mut checks = Vec::new();
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
        let infrastructure_task = if head == *expected {
            None
        } else {
            ResolvedTask::discover(&repo, &repo.root)
                .and_then(|path| ResolvedTask::load(&repo, config, &path))
                .ok()
                .filter(|task| task.kind == TaskKind::Infrastructure && task.branch == head)
        };
        let branch_ok = head == *expected || infrastructure_task.is_some();
        checks.push(DoctorCheck {
            name: "checkout-branch".to_owned(),
            status: if branch_ok { "ok" } else { "error" }.to_owned(),
            detail: match infrastructure_task {
                Some(task) => format!(
                    "current {head}, valid isolated infrastructure task {}",
                    task.task_id
                ),
                None => format!("current {head}, configured shared branch {expected}"),
            },
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

        if config.s3_enabled() {
            checks.push(match dvc::require_runtime(&repo) {
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
            checks.push(match dvc::validate_internal_config(&repo, config) {
                Ok(()) => DoctorCheck {
                    name: "managed-storage-config".to_owned(),
                    status: "ok".to_owned(),
                    detail: "internal configuration matches .workspace-mgr.toml".to_owned(),
                },
                Err(error) => DoctorCheck {
                    name: "managed-storage-config".to_owned(),
                    status: "error".to_owned(),
                    detail: error.to_string(),
                },
            });
            if config.requires_object_versioning() {
                checks.push(match dvc::require_version_adapter(&repo) {
                    Ok(adapter) => DoctorCheck {
                        name: "managed-storage-version-adapter".to_owned(),
                        status: "ok".to_owned(),
                        detail: adapter,
                    },
                    Err(error) => DoctorCheck {
                        name: "managed-storage-version-adapter".to_owned(),
                        status: "error".to_owned(),
                        detail: error.to_string(),
                    },
                });
                checks.push(match dvc::verify_object_versioning(&repo, config) {
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
                });
            }
            let local = repo.root.join(".dvc/config.local");
            if local.exists() {
                let relative = ".dvc/config.local";
                let ignored = repo.run_unchecked(["check-ignore", "--quiet", "--", relative])?;
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
    })
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
