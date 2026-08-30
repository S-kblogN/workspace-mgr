use std::path::Path;

use serde::Serialize;

use crate::config::{CONFIG_NAME, Config};
use crate::error::{Error, Result};
use crate::git::GitRepo;
use crate::process::{command_exists, run_unchecked};

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
    checks.push(command_check("git", true));

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
        checks.push(DoctorCheck {
            name: "cli-version".to_owned(),
            status: if config.version_matches()? {
                "ok"
            } else {
                "error"
            }
            .to_owned(),
            detail: format!(
                "installed {} required {}",
                env!("CARGO_PKG_VERSION"),
                config.required_cli
            ),
        });
        let head = repo
            .current_branch()?
            .unwrap_or_else(|| "detached".to_owned());
        let expected = &config.git.shared_checkout_branch;
        let branch_ok =
            config.profile != crate::config::Profile::SharedCheckout || head == *expected;
        checks.push(DoctorCheck {
            name: "checkout-branch".to_owned(),
            status: if branch_ok { "ok" } else { "error" }.to_owned(),
            detail: format!("current {head}, configured shared branch {expected}"),
        });
        let identity = repo.run_unchecked(["var", "GIT_AUTHOR_IDENT"])?;
        checks.push(DoctorCheck {
            name: "git-author-identity".to_owned(),
            status: if identity.success() { "ok" } else { "error" }.to_owned(),
            detail: if identity.success() {
                "Git author name and email are configured".to_owned()
            } else {
                "Git author name or email is not configured".to_owned()
            },
        });
        let remote = repo.run_unchecked(["remote", "get-url", &config.git.remote])?;
        checks.push(DoctorCheck {
            name: "git-remote".to_owned(),
            status: if remote.success() { "ok" } else { "error" }.to_owned(),
            detail: if remote.success() {
                format!("configured remote {:?} exists", config.git.remote)
            } else {
                format!("configured remote {:?} does not exist", config.git.remote)
            },
        });

        if config.large_files.fallback == "git-lfs" {
            checks.push(command_check("git-lfs", true));
        }
        if config.dvc.enabled {
            let dvc = command_check("dvc", true);
            let dvc_ok = dvc.status == "ok";
            checks.push(dvc);
            let mut actual_remote = None;
            if dvc_ok {
                let output = run_unchecked("dvc", ["config", "core.remote"], &repo.root)?;
                let actual = output.stdout.trim();
                let expected = config.dvc.remote.as_deref();
                let remote_ok = output.success()
                    && !actual.is_empty()
                    && expected.is_none_or(|value| value == actual);
                checks.push(DoctorCheck {
                    name: "dvc-default-remote".to_owned(),
                    status: if remote_ok { "ok" } else { "error" }.to_owned(),
                    detail: match (expected, actual.is_empty()) {
                        (Some(expected), false) => {
                            format!("configured {expected}, DVC default {actual}")
                        }
                        (Some(expected), true) => {
                            format!("configured {expected}, but DVC has no default remote")
                        }
                        (None, false) => format!("DVC default {actual}"),
                        (None, true) => "DVC has no default remote".to_owned(),
                    },
                });
                if !actual.is_empty() {
                    actual_remote = Some(actual.to_owned());
                }
            }
            if config.dvc.require_version_aware {
                if let Some(remote) = actual_remote {
                    let key = format!("remote.{remote}.version_aware");
                    let output = run_unchecked("dvc", ["config", &key], &repo.root)?;
                    let enabled =
                        output.success() && output.stdout.trim().eq_ignore_ascii_case("true");
                    checks.push(DoctorCheck {
                        name: "dvc-version-aware".to_owned(),
                        status: if enabled { "ok" } else { "error" }.to_owned(),
                        detail: format!(
                            "remote {remote} version_aware={}",
                            if enabled { "true" } else { "false or unset" }
                        ),
                    });
                }
                let python = command_check(&config.dvc.python, true);
                let python_ok = python.status == "ok";
                checks.push(python);
                if python_ok {
                    let output = run_unchecked(
                        &config.dvc.python,
                        ["-c", "import dvc; print(dvc.__version__)"],
                        &repo.root,
                    )?;
                    checks.push(DoctorCheck {
                        name: "dvc-python-adapter".to_owned(),
                        status: if output.success() { "ok" } else { "error" }.to_owned(),
                        detail: if output.success() {
                            format!("dvc {}", output.stdout.trim())
                        } else {
                            output.stderr.trim().to_owned()
                        },
                    });
                }
            }
            let local = repo.root.join(".dvc/config.local");
            if local.exists() {
                let relative = ".dvc/config.local";
                let ignored = repo.run_unchecked(["check-ignore", "--quiet", "--", relative])?;
                checks.push(DoctorCheck {
                    name: "dvc-local-config-ignore".to_owned(),
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

pub fn require_healthy(path: &Path) -> Result<DoctorReport> {
    let report = inspect(path)?;
    if report.healthy() {
        Ok(report)
    } else {
        Err(Error::message("workspace-mgr doctor found errors"))
    }
}
