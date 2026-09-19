use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};

/// Validates and normalizes a repository-relative path.
///
/// Separators are the platform's own: a backslash is an ordinary file-name
/// character on the supported Unix platforms, so it is never rewritten. This
/// keeps user-typed paths, Git paths and storage-engine metadata comparable.
pub fn repo_path(raw: &str, field: &str) -> Result<String> {
    if raw != raw.trim() || raw.chars().any(char::is_control) {
        return Err(Error::message(format!(
            "{field} must not contain leading/trailing whitespace or control characters"
        )));
    }
    if raw.is_empty() {
        return Err(Error::message(format!("{field} must not be empty")));
    }
    let path = Path::new(raw);
    if path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::RootDir))
    {
        return Err(Error::message(format!(
            "{field} must be a repository-relative path: {raw:?}"
        )));
    }
    let normalized = path
        .components()
        .filter_map(|part| match part {
            Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            Component::CurDir => None,
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    let first = normalized.split('/').next().unwrap_or_default();
    if normalized.is_empty() || first.eq_ignore_ascii_case(".git") {
        return Err(Error::message(format!(
            "{field} may not target the repository root or .git"
        )));
    }
    Ok(normalized)
}

pub fn allowed(path: &str, scopes: &[String]) -> bool {
    scopes
        .iter()
        .any(|scope| path == scope || path.starts_with(&format!("{scope}/")))
}

pub fn to_slash(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

pub fn relative_to(path: &Path, root: &Path, field: &str) -> Result<String> {
    let relative = path.strip_prefix(root).map_err(|_| {
        Error::message(format!(
            "{field} is outside the repository: {}",
            path.display()
        ))
    })?;
    repo_path(&to_slash(relative), field)
}

pub fn resolved_under(root: &Path, relative: &str) -> PathBuf {
    root.join(relative.split('/').collect::<PathBuf>())
}

pub fn reject_symlink_traversal(root: &Path, relative: &str, field: &str) -> Result<()> {
    let mut current = root.to_path_buf();
    let components = Path::new(relative).components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(Error::message(format!(
                    "{field} may not traverse a symlink: {relative:?}"
                )));
            }
            Ok(metadata) if index + 1 < components.len() && !metadata.is_dir() => {
                return Err(Error::message(format!(
                    "{field} has a non-directory ancestor: {relative:?}"
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(Error::message(format!(
                    "failed to inspect {field} {relative:?}: {error}"
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_repository_relative_paths() {
        assert_eq!(repo_path("task/data", "path").unwrap(), "task/data");
        assert!(repo_path("../outside", "path").is_err());
        assert!(repo_path("/absolute", "path").is_err());
        assert!(repo_path(".git/index", "path").is_err());
        assert!(repo_path(".GIT/index", "path").is_err());
        assert!(repo_path(" task/data ", "path").is_err());
        assert!(repo_path("task/data\n", "path").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn keeps_backslashes_as_file_name_characters() {
        assert_eq!(
            repo_path("task/top\\level.bin", "path").unwrap(),
            "task/top\\level.bin"
        );
        assert_eq!(
            repo_path("task/./d\\x//big.bin", "path").unwrap(),
            "task/d\\x/big.bin"
        );
        assert_eq!(repo_path("..\\outside", "path").unwrap(), "..\\outside");
        assert_eq!(repo_path("\\task", "path").unwrap(), "\\task");
        assert_eq!(
            relative_to(
                Path::new("/repo/task/d\\x/big.bin"),
                Path::new("/repo"),
                "path"
            )
            .unwrap(),
            "task/d\\x/big.bin"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_paths_that_escape_through_symlink_ancestors() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), temp.path().join("linked")).unwrap();
        assert!(reject_symlink_traversal(temp.path(), "linked/data", "path").is_err());
        assert!(reject_symlink_traversal(temp.path(), "ordinary/data", "path").is_ok());
    }
}
