use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::Serialize;

use crate::dvc::REQUIRED_DVC_VERSION;
use crate::error::{Error, IoContext, Result};
use crate::process::{run, run_unchecked};

pub const RUNTIME_DIR_ENV: &str = "WORKSPACE_MGR_RUNTIME_DIR";
pub const STORAGE_DVC_ENV: &str = "WORKSPACE_MGR_STORAGE_DVC";
pub const BOOTSTRAP_PYTHON_ENV: &str = "WORKSPACE_MGR_BOOTSTRAP_PYTHON";

#[derive(Debug, Clone)]
pub struct SetupOptions {
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

pub fn managed_runtime_dir() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(RUNTIME_DIR_ENV).filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        return Some(
            PathBuf::from(path)
                .join("workspace-mgr")
                .join(format!("storage-{REQUIRED_DVC_VERSION}")),
        );
    }
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|home| {
            home.join(".local")
                .join("share")
                .join("workspace-mgr")
                .join(format!("storage-{REQUIRED_DVC_VERSION}"))
        })
}

pub fn dvc_program() -> String {
    if let Ok(program) = std::env::var(STORAGE_DVC_ENV) {
        if !program.trim().is_empty() {
            return program;
        }
    }
    managed_runtime_dir()
        .map(|root| runtime_program(&root, "dvc"))
        .filter(|path| path.is_file())
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "dvc".to_owned())
}

pub fn storage_python() -> String {
    if let Ok(program) = std::env::var(crate::dvc::STORAGE_PYTHON_ENV) {
        if !program.trim().is_empty() {
            return program;
        }
    }
    managed_runtime_dir()
        .map(|root| runtime_program(&root, "python"))
        .filter(|path| path.is_file())
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "python3".to_owned())
}

pub fn setup(options: &SetupOptions) -> Result<SetupReport> {
    let runtime_dir = match &options.runtime_dir {
        Some(path) => absolute_target(path)?,
        None => managed_runtime_dir().ok_or_else(|| {
            Error::message(format!(
                "cannot determine a private runtime directory; set {RUNTIME_DIR_ENV}"
            ))
        })?,
    };
    let expected = format!("dvc[s3]=={REQUIRED_DVC_VERSION}");
    let actions = vec![
        format!(
            "create an isolated Python environment at {}",
            runtime_dir.display()
        ),
        format!("install private storage runtime {expected}"),
        "verify the isolated executable and Python module versions".to_owned(),
    ];
    if runtime_is_compatible(&runtime_dir)? {
        return Ok(SetupReport {
            status: "no_changes".to_owned(),
            runtime_dir: runtime_dir.display().to_string(),
            storage_runtime: REQUIRED_DVC_VERSION.to_owned(),
            actions: Vec::new(),
        });
    }
    if options.dry_run {
        return Ok(SetupReport {
            status: "dry_run".to_owned(),
            runtime_dir: runtime_dir.display().to_string(),
            storage_runtime: REQUIRED_DVC_VERSION.to_owned(),
            actions,
        });
    }
    if which::which("git").is_err() {
        return Err(Error::message(
            "Git is unavailable; install the platform Git runtime before workspace-mgr setup",
        ));
    }
    let bootstrap_python = std::env::var(BOOTSTRAP_PYTHON_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "python3".to_owned());
    let parent = runtime_dir
        .parent()
        .ok_or_else(|| Error::message("private runtime directory has no parent"))?;
    fs::create_dir_all(parent).at(parent)?;
    let lock_path = parent.join(".workspace-mgr-setup.lock");
    let setup_lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .at(&lock_path)?;
    setup_lock
        .try_lock_exclusive()
        .map_err(|_| Error::message("another workspace-mgr setup operation is running"))?;
    // Another installer may have completed between the optimistic check above
    // and lock acquisition.
    if runtime_is_compatible(&runtime_dir)? {
        return Ok(SetupReport {
            status: "no_changes".to_owned(),
            runtime_dir: runtime_dir.display().to_string(),
            storage_runtime: REQUIRED_DVC_VERSION.to_owned(),
            actions: Vec::new(),
        });
    }
    let backup = parent.join(format!(
        ".workspace-mgr-runtime-backup-{}",
        std::process::id()
    ));
    if path_exists(&backup)? {
        return Err(Error::message(format!(
            "stale private runtime backup blocks setup: {}",
            backup.display()
        )));
    }
    let had_previous = path_exists(&runtime_dir)?;
    if had_previous {
        fs::rename(&runtime_dir, &backup).at(&runtime_dir)?;
    }

    // Virtual environments embed their absolute location in launcher shebangs,
    // so they cannot be assembled elsewhere and renamed into place. Preserve
    // the previous runtime, build at the final path, and restore the backup if
    // provisioning or verification fails.
    let install_result = (|| -> Result<()> {
        run(
            &bootstrap_python,
            ["-m", "venv", &runtime_dir.to_string_lossy()],
            parent,
        )?;
        let runtime_python = runtime_program(&runtime_dir, "python");
        run(
            &runtime_python.to_string_lossy(),
            [
                "-m",
                "pip",
                "install",
                "--isolated",
                "--no-input",
                "--disable-pip-version-check",
                &expected,
            ],
            parent,
        )?;
        if !runtime_is_compatible(&runtime_dir)? {
            return Err(Error::message(
                "new private storage runtime failed its compatibility check",
            ));
        }
        Ok(())
    })();
    if let Err(source) = install_result {
        let cleanup = if path_exists(&runtime_dir).unwrap_or(true) {
            remove_path(&runtime_dir)
        } else {
            Ok(())
        };
        let restore = if had_previous && cleanup.is_ok() {
            fs::rename(&backup, &runtime_dir)
        } else {
            Ok(())
        };
        if let Err(rollback_error) = cleanup.and(restore) {
            return Err(Error::message(format!(
                "private runtime setup failed ({source}); rollback also failed: {rollback_error}"
            )));
        }
        return Err(source);
    }
    if had_previous {
        remove_path(&backup).at(&backup)?;
    }
    Ok(SetupReport {
        status: "installed".to_owned(),
        runtime_dir: runtime_dir.display().to_string(),
        storage_runtime: REQUIRED_DVC_VERSION.to_owned(),
        actions,
    })
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(Error::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn runtime_is_compatible(root: &Path) -> Result<bool> {
    let dvc = runtime_program(root, "dvc");
    let python = runtime_program(root, "python");
    if !dvc.is_file() || !python.is_file() {
        return Ok(false);
    }
    let dvc_version = run_unchecked(&dvc.to_string_lossy(), ["--version"], root)?;
    if !dvc_version.success() || dvc_version.stdout.trim() != REQUIRED_DVC_VERSION {
        return Ok(false);
    }
    let module_version = run_unchecked(
        &python.to_string_lossy(),
        ["-c", "import dvc; print(dvc.__version__)"],
        root,
    )?;
    Ok(module_version.success() && module_version.stdout.trim() == REQUIRED_DVC_VERSION)
}

fn runtime_program(root: &Path, name: &str) -> PathBuf {
    root.join("bin").join(name)
}

fn absolute_target(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let current = std::env::current_dir().map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(current.join(path))
}
