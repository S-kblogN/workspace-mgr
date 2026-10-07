//! Explicit, byte-bound attestations for inert historical records.
//! These are never inferred from an extension or rewritten during relocation.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::{Error, IoContext, Result};
use crate::path::{reject_symlink_traversal, repo_path};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct HistoricalRecord {
    pub path: String,
    pub sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unix_mode: Option<u32>,
    pub role: &'static str,
}

pub(crate) fn prepare(root: &Path, paths: &[String]) -> Result<Vec<HistoricalRecord>> {
    paths
        .iter()
        .map(|path| repo_path(path, "historical record"))
        .collect::<Result<BTreeSet<_>>>()?
        .into_iter()
        .map(|path| snapshot(root, &path))
        .collect()
}

pub(crate) fn paths(root: &Path, records: &[HistoricalRecord]) -> Vec<PathBuf> {
    records
        .iter()
        .map(|record| root.join(&record.path))
        .collect()
}

pub(crate) fn revalidate(root: &Path, records: &[HistoricalRecord]) -> Result<()> {
    for record in records {
        if snapshot(root, &record.path)? != *record {
            return Err(Error::message(format!(
                "historical record {} changed during archive preflight; inspect its contents and explicitly confirm it again before moving",
                record.path
            )));
        }
    }
    Ok(())
}

fn snapshot(root: &Path, path: &str) -> Result<HistoricalRecord> {
    reject_symlink_traversal(root, path, "historical record")?;
    let absolute = root.join(path);
    let metadata = fs::symlink_metadata(&absolute).at(&absolute)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || protected(Path::new(path)) {
        return Err(Error::message(format!(
            "historical record must be an ordinary inert text file, not a script or repository control file: {path}"
        )));
    }
    #[cfg(unix)]
    let unix_mode = {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode();
        if mode & 0o111 != 0 {
            return Err(Error::message(format!(
                "historical record may not be executable: {path}"
            )));
        }
        Some(mode)
    };
    #[cfg(not(unix))]
    let unix_mode = None;
    let mut file = File::open(&absolute).at(&absolute)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 65_536];
    let mut pending = Vec::new();
    let mut first = true;
    loop {
        let count = file.read(&mut buffer).at(&absolute)?;
        if count == 0 {
            if !pending.is_empty() {
                return Err(not_text(path));
            }
            break;
        }
        let chunk = &buffer[..count];
        if first && chunk.starts_with(b"#!") {
            return Err(Error::message(format!(
                "historical record may not contain a script shebang: {path}"
            )));
        }
        first = false;
        if chunk.contains(&0) {
            return Err(not_text(path));
        }
        hasher.update(chunk);
        pending.extend_from_slice(chunk);
        match std::str::from_utf8(&pending) {
            Ok(_) => pending.clear(),
            Err(error) if error.error_len().is_none() => {
                pending.drain(..error.valid_up_to());
            }
            Err(_) => return Err(not_text(path)),
        }
    }
    Ok(HistoricalRecord {
        path: path.to_owned(),
        sha256: crate::hex::encode_lower(hasher.finalize()),
        unix_mode,
        role: "historical-record",
    })
}

fn not_text(path: &str) -> Error {
    Error::message(format!(
        "historical record must contain ordinary UTF-8 text: {path}"
    ))
}

fn protected(path: &Path) -> bool {
    // macOS commonly aliases case variants to the same inode. An alternate
    // spelling cannot turn a tool-owned file into an inert history record.
    if path.components().any(|part| {
        part.as_os_str().to_str().is_some_and(|part| {
            matches!(
                part.to_ascii_lowercase().as_str(),
                ".git" | ".workspace-mgr" | ".dvc" | ".github" | ".cargo"
            )
        })
    }) {
        return true;
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if name.starts_with(".workspace-mgr-")
        || name == ".workspace-mgr.toml"
        || name == ".env"
        || name.starts_with(".env.")
        || matches!(
            name.as_str(),
            "makefile"
                | "gnumakefile"
                | "dockerfile"
                | "justfile"
                | "cargo.toml"
                | "pyproject.toml"
                | "package.json"
                | "dvc.yaml"
                | "dvc.lock"
                | ".bashrc"
                | ".zshrc"
                | ".profile"
                | ".gitignore"
                | ".gitattributes"
                | ".gitmodules"
                | ".gitconfig"
        )
    {
        return true;
    }
    is_source_file(path)
}

pub(crate) fn is_source_file(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|value| value.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some(
            "py" | "pyw"
                | "sh"
                | "bash"
                | "zsh"
                | "fish"
                | "ps1"
                | "bat"
                | "cmd"
                | "rs"
                | "js"
                | "mjs"
                | "cjs"
                | "jsx"
                | "ts"
                | "tsx"
                | "r"
                | "jl"
                | "pl"
                | "rb"
                | "lua"
                | "c"
                | "cpp"
                | "h"
                | "java"
                | "go"
                | "ipynb"
                | "dvc"
                | "mk"
                | "nix"
                | "sql"
                | "rmd"
                | "qmd"
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_aliases_cannot_exempt_git_workspace_or_build_controls() {
        for path in [
            "task/.WORKSPACE-MGR-TASK.TOML",
            ".WORKSPACE-MGR.TOML",
            "CARGO.TOML",
            "task/.GIT/config",
            "task/.GIT/hooks/report.log",
            "task/.GITIGNORE",
            "task/.GITATTRIBUTES",
            "task/.GITMODULES",
            ".GITHUB/workflows/ci.yml",
        ] {
            assert!(protected(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn requires_exact_paths_and_binds_bytes_before_any_move() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("logs")).unwrap();
        fs::write(temp.path().join("logs/report.md"), "previous task/path\n").unwrap();
        let records = prepare(
            temp.path(),
            &["logs/./report.md".into(), "logs/report.md".into()],
        )
        .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].path, "logs/report.md");
        revalidate(temp.path(), &records).unwrap();
        fs::write(temp.path().join("logs/report.md"), "changed history\n").unwrap();
        assert!(
            revalidate(temp.path(), &records)
                .unwrap_err()
                .to_string()
                .contains("changed")
        );
        for invalid in ["../outside", "/outside", "logs", "logs/*.md"] {
            assert!(
                prepare(temp.path(), &[invalid.into()]).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn refuses_scripts_controls_and_binary_records_regardless_of_extension() {
        let temp = tempfile::tempdir().unwrap();
        for name in [
            "run.py",
            "history.log",
            ".workspace-mgr-task.toml",
            "Cargo.toml",
        ] {
            fs::write(temp.path().join(name), "#!/bin/sh\necho /old/path\n").unwrap();
            assert!(prepare(temp.path(), &[name.into()]).is_err(), "{name}");
        }
        let mut bytes = vec![b'a'; 65_540];
        bytes.push(0);
        fs::write(temp.path().join("binary.log"), bytes).unwrap();
        assert!(prepare(temp.path(), &["binary.log".into()]).is_err());
        fs::write(temp.path().join("invalid.log"), [0xff]).unwrap();
        assert!(prepare(temp.path(), &["invalid.log".into()]).is_err());
    }

    #[test]
    fn hashes_large_utf8_records_across_read_boundaries() {
        let temp = tempfile::tempdir().unwrap();
        let text = format!("{}中文", "a".repeat(65_535));
        fs::write(temp.path().join("history.log"), &text).unwrap();
        let records = prepare(temp.path(), &["history.log".into()]).unwrap();
        assert_eq!(
            records[0].sha256,
            crate::hex::encode_lower(Sha256::digest(text.as_bytes()))
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_aliases_and_permission_changes() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("history.log");
        fs::write(&path, "history").unwrap();
        symlink(&path, temp.path().join("alias.log")).unwrap();
        assert!(prepare(temp.path(), &["alias.log".into()]).is_err());
        let records = prepare(temp.path(), &["history.log".into()]).unwrap();
        fs::set_permissions(
            &path,
            fs::Permissions::from_mode(records[0].unix_mode.unwrap() ^ 0o040),
        )
        .unwrap();
        assert!(revalidate(temp.path(), &records).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(prepare(temp.path(), &["history.log".into()]).is_err());
    }
}
