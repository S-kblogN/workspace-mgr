use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};

pub fn repo_path(raw: &str, field: &str) -> Result<String> {
    let value = raw.trim().replace('\\', "/");
    if value.is_empty() {
        return Err(Error::message(format!("{field} must not be empty")));
    }
    let path = Path::new(&value);
    if path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::RootDir))
    {
        return Err(Error::message(format!(
            "{field} must be a repository-relative path: {value:?}"
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
    if normalized.is_empty() || normalized == ".git" || normalized.starts_with(".git/") {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_repository_relative_paths() {
        assert_eq!(repo_path("task/data", "path").unwrap(), "task/data");
        assert!(repo_path("../outside", "path").is_err());
        assert!(repo_path("/absolute", "path").is_err());
        assert!(repo_path(".git/index", "path").is_err());
    }
}
