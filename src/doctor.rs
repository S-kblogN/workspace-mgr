use std::path::Path;

use serde::Serialize;

use crate::config::{CONFIG_NAME, Config};
use crate::dvc;
use crate::error::Result;
use crate::git::GitRepo;
use crate::process::command_exists;

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
    let config = match Config::load(&repo) {
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

    if let Some(config) = &config {
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
