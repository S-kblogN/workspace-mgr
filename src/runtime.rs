use std::path::PathBuf;

use serde::Serialize;

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct SetupOptions {
    /// Accepted for older installers; native storage does not create a runtime.
    pub runtime_dir: Option<PathBuf>,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SetupReport {
    pub status: String,
    pub runtime_dir: String,
    pub storage_runtime: String,
    pub actions: Vec<String>,
}

pub fn setup(options: &SetupOptions) -> Result<SetupReport> {
    if which::which("git").is_err() {
        return Err(Error::message(
            "Git is unavailable; install Git before using workspace-mgr",
        ));
    }
    // Preserve the old report field and CLI flag without inspecting, replacing,
    // or deleting any former private runtime or user directory.
    let _ = &options.runtime_dir;
    Ok(SetupReport {
        status: if options.dry_run {
            "dry_run"
        } else {
            "no_changes"
        }
        .to_owned(),
        runtime_dir: String::new(),
        storage_runtime: format!("native Rust {}", env!("CARGO_PKG_VERSION")),
        actions: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_does_not_modify_a_legacy_runtime_directory() {
        let directory = tempfile::tempdir().unwrap();
        let sentinel = directory.path().join("user-content");
        std::fs::write(&sentinel, b"preserve").unwrap();
        for dry_run in [true, false] {
            let result = setup(&SetupOptions {
                runtime_dir: Some(directory.path().to_owned()),
                dry_run,
            })
            .unwrap();
            assert!(result.runtime_dir.is_empty());
            assert!(result.storage_runtime.starts_with("native Rust "));
            assert!(result.actions.is_empty());
            assert_eq!(std::fs::read(&sentinel).unwrap(), b"preserve");
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        }
    }
}
