//! Opaque task moves. Legacy reference snapshots are accepted only to restore
//! the original bytes of an existing archive cancellation journal.

use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use crate::error::{Error, IoContext, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct RelocationNotice {
    pub code: &'static str,
    pub message: &'static str,
}

impl RelocationNotice {
    pub(crate) fn renamed_directory() -> Self {
        Self {
            code: "manual-content-audit-after-relocation",
            message: "Rename succeeded. Moving the task directory may create many broken links or path references; manually audit and repair affected task content.",
        }
    }

    pub(crate) fn archived_directory() -> Self {
        Self {
            code: "manual-content-audit-after-relocation",
            message: "Archive directory move succeeded. Moving directories may create many broken links or path references; manually audit and repair affected task content.",
        }
    }

    pub(crate) fn moved_path() -> Self {
        Self {
            code: "manual-content-audit-after-relocation",
            message: "Move succeeded. Changing paths may create many broken links or path references; manually audit and repair affected task content.",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RelocationPlan {
    source: PathBuf,
    destination: PathBuf,
    references: Vec<ReferenceSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReferenceSnapshot {
    before_path: PathBuf,
    after_path: PathBuf,
    before: Vec<u8>,
    after: Vec<u8>,
    #[serde(default)]
    unix_mode: Option<u32>,
}

impl RelocationPlan {
    /// Archive moves payloads without interpreting or rewriting their contents.
    /// In particular, ignored nested repositories retain their Git control
    /// files and any external registrations exactly as they were.
    pub(crate) fn opaque(source: &Path, destination: &Path) -> Result<Self> {
        let source = source.canonicalize().at(source)?;
        if !destination.is_absolute() {
            return Err(Error::message("relocation destination must be absolute"));
        }
        Ok(Self {
            source,
            destination: normalize(destination),
            references: Vec::new(),
        })
    }

    pub(crate) fn retarget(&self, destination: &Path) -> Result<Self> {
        if !self.references.is_empty() || !destination.is_absolute() {
            return Err(Error::message(
                "only an opaque pending relocation can be retargeted",
            ));
        }
        let mut next = self.clone();
        next.destination = normalize(destination);
        next.validate_scope()?;
        Ok(next)
    }

    /// New moves are opaque. Existing reference snapshots are cancellation
    /// evidence and must never authorize a fresh payload rewrite.
    pub(crate) fn apply(&self) -> Result<()> {
        self.validate_scope()?;
        if !self.references.is_empty() {
            return Err(Error::message(
                "legacy relocation rewrites are no longer supported; cancel the old attempt to restore its original bytes",
            ));
        }
        Ok(())
    }

    /// Restore before reversing the directory move. Also succeeds after an
    /// already completed reversal, so interrupted cancel is resumable.
    pub(crate) fn restore(&self) -> Result<()> {
        self.validate_scope()?;
        let paths = self
            .references
            .iter()
            .map(|reference| reference.existing_path())
            .collect::<Result<Vec<_>>>()?;
        for (reference, path) in self.references.iter().zip(&paths) {
            reference.validate(path)?;
        }
        for (reference, path) in self.references.iter().zip(paths).rev() {
            reference.write(&path, &reference.before)?;
        }
        Ok(())
    }

    pub(crate) fn validate_applied(&self) -> Result<()> {
        self.validate_scope()?;
        for reference in &self.references {
            reference.validate(&reference.existing_path()?)?;
        }
        Ok(())
    }

    fn validate_scope(&self) -> Result<()> {
        for reference in &self.references {
            let relative = reference.before_path.strip_prefix(&self.source).ok();
            if normalize(&reference.before_path) != reference.before_path
                || normalize(&reference.after_path) != reference.after_path
                || relative
                    .is_none_or(|relative| self.destination.join(relative) != reference.after_path)
            {
                return Err(Error::message(format!(
                    "saved relocation plan would modify Git control files outside the task scope: {} -> {}; repair this old attempt's external Git layout separately before retrying",
                    reference.before_path.display(),
                    reference.after_path.display()
                )));
            }
            for (path, root) in [
                (&reference.before_path, &self.source),
                (&reference.after_path, &self.destination),
            ] {
                if path.exists() {
                    require_within_task(path, root, "saved Git control file")?;
                }
            }
        }
        Ok(())
    }
}

impl ReferenceSnapshot {
    fn existing_path(&self) -> Result<PathBuf> {
        if self.after_path.exists() {
            Ok(self.after_path.clone())
        } else if self.before_path.exists() {
            Ok(self.before_path.clone())
        } else {
            Err(Error::message(format!(
                "relocation reference disappeared: {}",
                self.after_path.display()
            )))
        }
    }

    fn validate(&self, path: &Path) -> Result<()> {
        ordinary_file(path)?;
        let contents = fs::read(path).at(path)?;
        if contents != self.before && contents != self.after {
            return Err(Error::message(format!(
                "relocation reference changed independently; preserve it before proceeding: {}",
                path.display()
            )));
        }
        #[cfg(unix)]
        if let Some(mode) = self.unix_mode {
            use std::os::unix::fs::PermissionsExt;
            if fs::metadata(path).at(path)?.permissions().mode() != mode {
                return Err(Error::message(format!(
                    "relocation reference permissions changed independently: {}",
                    path.display()
                )));
            }
        }
        Ok(())
    }

    fn write(&self, path: &Path, contents: &[u8]) -> Result<()> {
        if fs::read(path).at(path)? == contents {
            return Ok(());
        }
        let parent = path.parent().expect("reference parent");
        let mut file = tempfile::NamedTempFile::new_in(parent).at(parent)?;
        file.write_all(contents).at(path)?;
        #[cfg(unix)]
        if let Some(mode) = self.unix_mode {
            use std::os::unix::fs::PermissionsExt;
            file.as_file()
                .set_permissions(fs::Permissions::from_mode(mode))
                .at(path)?;
        }
        file.as_file().sync_all().at(path)?;
        file.persist(path).map_err(|error| Error::Io {
            path: path.to_path_buf(),
            source: error.error,
        })?;
        Ok(())
    }
}

fn require_within_task(path: &Path, root: &Path, description: &str) -> Result<()> {
    let canonical_path = path.canonicalize().at(path)?;
    let canonical_root = root.canonicalize().at(root)?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(Error::message(format!(
            "nested {description} lies outside the task scope: {} (resolves to {}); move both the checkout and its Git administrative directory inside the task, or create an independent clone inside it before moving",
            path.display(),
            canonical_path.display()
        )));
    }
    Ok(())
}

fn ordinary_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).at(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(Error::message(format!(
            "Git relocation control must be an ordinary file: {}",
            path.display()
        )));
    }
    Ok(())
}

fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            component => result.push(component.as_os_str()),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_journal_restores_original_bytes_but_cannot_authorize_new_rewrites() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("task");
        let destination = root.join("moved/task");
        fs::create_dir_all(destination.join("nested")).unwrap();
        let before = b"original user bytes\n";
        let after = b"old client rewritten bytes\n";
        fs::write(destination.join("nested/.git"), after).unwrap();
        let plan = RelocationPlan {
            source: source.clone(),
            destination: destination.clone(),
            references: vec![ReferenceSnapshot {
                before_path: source.join("nested/.git"),
                after_path: destination.join("nested/.git"),
                before: before.to_vec(),
                after: after.to_vec(),
                unix_mode: None,
            }],
        };
        let plan: RelocationPlan =
            serde_json::from_str(&serde_json::to_string(&plan).unwrap()).unwrap();
        assert!(
            plan.apply()
                .unwrap_err()
                .to_string()
                .contains("no longer supported")
        );
        assert_eq!(fs::read(destination.join("nested/.git")).unwrap(), after);
        plan.restore().unwrap();
        plan.restore().unwrap();
        assert_eq!(fs::read(destination.join("nested/.git")).unwrap(), before);
    }

    #[test]
    fn saved_plan_external_controls_are_refused_by_apply_restore_and_validation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("task");
        fs::create_dir_all(&source).unwrap();
        let external = root.join("external-gitdir");
        let contents = b"independent external bytes\n";
        fs::write(&external, contents).unwrap();
        let plan = RelocationPlan {
            source,
            destination: root.join("2026/07/task"),
            references: vec![ReferenceSnapshot {
                before_path: external.clone(),
                after_path: external.clone(),
                before: contents.to_vec(),
                after: b"rewritten external bytes\n".to_vec(),
                unix_mode: None,
            }],
        };
        let plan: RelocationPlan =
            serde_json::from_str(&serde_json::to_string(&plan).unwrap()).unwrap();
        for result in [plan.apply(), plan.restore(), plan.validate_applied()] {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("saved relocation plan"), "{error}");
            assert!(error.contains("outside the task scope"), "{error}");
            assert!(error.contains(external.to_str().unwrap()), "{error}");
        }
        assert_eq!(fs::read(external).unwrap(), contents);
        assert!(plan.source.is_dir());
        assert!(!plan.destination.exists());
    }

    #[cfg(unix)]
    #[test]
    fn saved_plan_canonical_scope_rejects_symlinked_control_parent() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("task");
        fs::create_dir_all(&source).unwrap();
        let external = root.join("external-admin");
        fs::create_dir_all(&external).unwrap();
        let contents = b"independent external bytes\n";
        fs::write(external.join("config"), contents).unwrap();
        symlink(&external, source.join("admin")).unwrap();
        let destination = root.join("2026/07/task");
        let plan = RelocationPlan {
            source: source.clone(),
            destination: destination.clone(),
            references: vec![ReferenceSnapshot {
                before_path: source.join("admin/config"),
                after_path: destination.join("admin/config"),
                before: contents.to_vec(),
                after: b"rewritten external bytes\n".to_vec(),
                unix_mode: None,
            }],
        };
        for result in [plan.apply(), plan.restore(), plan.validate_applied()] {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("outside the task scope"), "{error}");
            assert!(error.contains(external.to_str().unwrap()), "{error}");
        }
        assert_eq!(fs::read(external.join("config")).unwrap(), contents);
        assert!(!destination.exists());
    }
}
