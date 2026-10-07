//! Repair the small, verifiable set of Git control files that bind a checkout
//! to an absolute location. Snapshots also make a reverse move byte preserving.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::error::{Error, IoContext, Result};
use crate::process::run_unchecked;

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

/// Inspect all local content, including ignored checkouts and runtimes, before
/// moving it. Unknown executable/runtime references are refused rather than
/// rewritten by a broad text replacement.
pub(crate) fn prepare(source: &Path, destination: &Path) -> Result<RelocationPlan> {
    let source = source.canonicalize().at(source)?;
    if !destination.is_absolute() {
        return Err(Error::message("relocation destination must be absolute"));
    }
    let destination = normalize(destination);
    let mut builder = Builder {
        source,
        destination,
        references: BTreeMap::new(),
    };
    let mut entries = WalkDir::new(&builder.source)
        .follow_links(false)
        .into_iter();
    while let Some(entry) = entries.next() {
        let entry =
            entry.map_err(|error| Error::message(format!("inspect relocation: {error}")))?;
        let path = entry.path();
        if entry.file_name() == ".git" && entry.file_type().is_dir() {
            // Avoid traversing potentially enormous object stores.
            builder.inspect_git_directory(path)?;
            entries.skip_current_dir();
        } else if entry.file_type().is_symlink() {
            let target = fs::read_link(path).at(path)?;
            let resolved = resolve(path.parent().expect("entry parent"), &target);
            if (target.is_absolute() && resolved.starts_with(&builder.source))
                || (!target.is_absolute() && !resolved.starts_with(&builder.source))
            {
                return Err(Error::message(format!(
                    "relocation would invalidate a location-dependent symlink at {}; replace it with a relative link within the task or rebuild it before moving",
                    path.display()
                )));
            }
        } else if entry.file_type().is_file() && entry.file_name() == "pyvenv.cfg" {
            builder.inspect_runtime(path.parent().expect("venv root"))?;
        } else if entry.file_name() == ".git" && entry.file_type().is_file() {
            builder.inspect_pointer(path)?;
        }
    }
    Ok(RelocationPlan {
        source: builder.source,
        destination: builder.destination,
        references: builder.references.into_values().collect(),
    })
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

    #[cfg(test)]
    pub(crate) fn reference_paths(&self) -> Vec<PathBuf> {
        self.references
            .iter()
            .map(|reference| reference.before_path.clone())
            .collect()
    }

    /// Call immediately after the directory move. A partial application can
    /// always be reversed using the saved plan.
    pub(crate) fn apply(&self) -> Result<()> {
        self.validate_scope()?;
        for reference in &self.references {
            reference.validate(&reference.after_path)?;
        }
        for reference in &self.references {
            reference.write(&reference.after_path, &reference.after)?;
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

struct Builder {
    source: PathBuf,
    destination: PathBuf,
    references: BTreeMap<PathBuf, ReferenceSnapshot>,
}

impl Builder {
    fn relocated(&self, path: &Path) -> PathBuf {
        match path.strip_prefix(&self.source) {
            Ok(relative) => self.destination.join(relative),
            Err(_) => path.to_path_buf(),
        }
    }

    fn snapshot(&mut self, path: &Path, after: Vec<u8>) -> Result<()> {
        require_within_task(path, &self.source, "Git control file")?;
        ordinary_file(path)?;
        let before = fs::read(path).at(path)?;
        if before == after {
            return Ok(());
        }
        #[cfg(unix)]
        let unix_mode = {
            use std::os::unix::fs::PermissionsExt;
            Some(fs::metadata(path).at(path)?.permissions().mode())
        };
        #[cfg(not(unix))]
        let unix_mode = None;
        let snapshot = ReferenceSnapshot {
            before_path: path.to_path_buf(),
            after_path: self.relocated(path),
            before,
            after,
            unix_mode,
        };
        if let Some(previous) = self.references.get(path) {
            if previous.after != snapshot.after {
                return Err(Error::message("conflicting Git relocation references"));
            }
        } else {
            self.references.insert(path.to_path_buf(), snapshot);
        }
        Ok(())
    }

    fn inspect_pointer(&mut self, pointer: &Path) -> Result<()> {
        ordinary_file(pointer)?;
        let contents = fs::read(pointer).at(pointer)?;
        if contents.is_empty() {
            // uv and other caches use empty .git files as repository-discovery
            // boundaries. They have no Git administrative paths to relocate.
            return Ok(());
        }
        let raw = std::str::from_utf8(&contents).map_err(|_| {
            Error::message(format!("invalid nested Git pointer: {}", pointer.display()))
        })?;
        let value = raw.trim_end().strip_prefix("gitdir: ").ok_or_else(|| {
            Error::message(format!("invalid nested Git pointer: {}", pointer.display()))
        })?;
        let parent = pointer.parent().expect("git pointer parent");
        let directory = resolve(parent, Path::new(value));
        if !directory.join("HEAD").is_file() {
            return Err(Error::message(format!(
                "cannot verify nested Git administrative directory: {}",
                directory.display()
            )));
        }
        require_within_task(&directory, &self.source, "Git administrative directory")?;
        let relocated = self.relocated(&directory);
        let new_parent = self.relocated(parent);
        if resolve(&new_parent, Path::new(value)) != relocated {
            self.snapshot(
                pointer,
                format!("gitdir: {}\n", relocated.display()).into_bytes(),
            )?;
        }
        let backlink = directory.join("gitdir");
        if backlink.exists() {
            self.inspect_backlink(&backlink, Some(pointer))?;
        }
        self.inspect_common_directory(&directory)?;
        self.inspect_core_worktree(&directory.join("config"))?;
        self.inspect_core_worktree(&directory.join("config.worktree"))?;
        Ok(())
    }

    fn inspect_git_directory(&mut self, directory: &Path) -> Result<()> {
        require_within_task(directory, &self.source, "Git administrative directory")?;
        self.inspect_core_worktree(&directory.join("config"))?;
        self.inspect_core_worktree(&directory.join("config.worktree"))?;
        let worktrees = directory.join("worktrees");
        if worktrees.is_dir() {
            for entry in fs::read_dir(&worktrees).at(&worktrees)? {
                let path = entry.at(&worktrees)?.path();
                if path.is_dir() {
                    self.inspect_backlink(&path.join("gitdir"), None)?;
                    self.inspect_common_directory(&path)?;
                    self.inspect_core_worktree(&path.join("config.worktree"))?;
                }
            }
        }
        Ok(())
    }

    fn inspect_backlink(&mut self, backlink: &Path, expected: Option<&Path>) -> Result<()> {
        require_within_task(backlink, &self.source, "Git worktree registration")?;
        ordinary_file(backlink)?;
        let raw = fs::read_to_string(backlink).at(backlink)?;
        let pointer = resolve(
            backlink.parent().expect("backlink parent"),
            Path::new(raw.trim_end()),
        );
        if expected.is_some_and(|expected| pointer != expected) {
            return Err(Error::message(format!(
                "nested Git backlink does not identify its checkout: {}",
                backlink.display()
            )));
        }
        if !pointer.exists() {
            return Err(Error::message(format!(
                "stale nested Git worktree registration at {} points to missing {}; use git worktree repair for an existing checkout, or git worktree prune if it was removed, before moving",
                backlink.display(),
                pointer.display()
            )));
        }
        require_within_task(&pointer, &self.source, "Git linked checkout")?;
        ordinary_file(&pointer)?;
        let forward = fs::read_to_string(&pointer).at(&pointer)?;
        let forward = forward.trim_end().strip_prefix("gitdir: ").ok_or_else(|| {
            Error::message(format!(
                "invalid Git worktree pointer: {}",
                pointer.display()
            ))
        })?;
        let directory = backlink.parent().expect("backlink parent");
        if resolve(
            pointer.parent().expect("pointer parent"),
            Path::new(forward),
        ) != directory
        {
            return Err(Error::message(format!(
                "nested Git pointer/backlink disagree: {}",
                pointer.display()
            )));
        }
        let relocated_pointer = self.relocated(&pointer);
        let relocated_directory = self.relocated(directory);
        if resolve(&relocated_directory, Path::new(raw.trim_end())) != relocated_pointer {
            self.snapshot(
                backlink,
                format!("{}\n", relocated_pointer.display()).into_bytes(),
            )?;
        }
        if resolve(
            relocated_pointer.parent().expect("pointer parent"),
            Path::new(forward),
        ) != relocated_directory
        {
            self.snapshot(
                &pointer,
                format!("gitdir: {}\n", relocated_directory.display()).into_bytes(),
            )?;
        }
        Ok(())
    }

    fn inspect_common_directory(&mut self, directory: &Path) -> Result<()> {
        let common = directory.join("commondir");
        if !common.exists() {
            return Ok(());
        }
        ordinary_file(&common)?;
        let raw = fs::read_to_string(&common).at(&common)?;
        let target = resolve(directory, Path::new(raw.trim_end()));
        require_within_task(&target, &self.source, "Git common administrative directory")?;
        if resolve(&self.relocated(directory), Path::new(raw.trim_end())) != self.relocated(&target)
        {
            self.snapshot(
                &common,
                format!("{}\n", self.relocated(&target).display()).into_bytes(),
            )?;
        }
        Ok(())
    }

    fn inspect_core_worktree(&mut self, config: &Path) -> Result<()> {
        if !config.exists() {
            return Ok(());
        }
        ordinary_file(config)?;
        let parent = config.parent().expect("Git config parent");
        let output = run_unchecked(
            "git",
            [
                "config",
                "--file",
                &config.to_string_lossy(),
                "--get",
                "core.worktree",
            ],
            parent,
        )?;
        if output.code == 1 {
            return Ok(());
        }
        if !output.success() {
            return Err(Error::message(format!(
                "cannot inspect nested Git config: {}",
                config.display()
            )));
        }
        let original = Path::new(output.stdout.trim_end());
        let target = resolve(parent, original);
        require_within_task(&target, &self.source, "Git core.worktree checkout")?;
        if resolve(&self.relocated(parent), original) == self.relocated(&target) {
            return Ok(());
        }
        let mut temporary = tempfile::NamedTempFile::new().at(config)?;
        temporary
            .write_all(&fs::read(config).at(config)?)
            .at(config)?;
        let output = run_unchecked(
            "git",
            [
                "config",
                "--file",
                &temporary.path().to_string_lossy(),
                "core.worktree",
                &self.relocated(&target).to_string_lossy(),
            ],
            parent,
        )?;
        if !output.success() {
            return Err(Error::message(
                "cannot prepare nested Git worktree relocation",
            ));
        }
        self.snapshot(config, fs::read(temporary.path()).at(temporary.path())?)
    }

    fn inspect_runtime(&self, root: &Path) -> Result<()> {
        let prefix = self.source.to_string_lossy();
        for folder in ["bin", "Scripts"] {
            let path = root.join(folder);
            if !path.is_dir() {
                continue;
            }
            for entry in fs::read_dir(&path).at(&path)? {
                let path = entry.at(&path)?.path();
                let metadata = fs::symlink_metadata(&path).at(&path)?;
                if !metadata.is_file() || metadata.len() > 1_048_576 {
                    continue;
                }
                let raw = fs::read(&path).at(&path)?;
                let Ok(text) = std::str::from_utf8(&raw) else {
                    continue;
                };
                let first = text.lines().next().unwrap_or_default();
                if (first.starts_with("#!") && text.contains(prefix.as_ref()))
                    || (text.contains(prefix.as_ref())
                        && (text.contains("VIRTUAL_ENV") || text.contains("virtual_env")))
                {
                    return Err(Error::message(format!(
                        "relocation would invalidate the Python virtual environment at {} because {} embeds its original absolute location; preserve its environment specification and rebuild the runtime outside the task before moving",
                        root.display(),
                        path.display()
                    )));
                }
            }
        }
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

fn resolve(parent: &Path, path: &Path) -> PathBuf {
    normalize(&if path.is_absolute() {
        path.to_path_buf()
    } else {
        parent.join(path)
    })
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
    use std::process::Command;

    fn git(repo: &Path, args: &[&str]) -> String {
        let result = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout)
            .unwrap()
            .trim_end()
            .to_owned()
    }

    fn repository(path: &Path) {
        fs::create_dir_all(path).unwrap();
        git(path, &["init", "-b", "main"]);
        git(path, &["config", "user.email", "test@example.invalid"]);
        git(path, &["config", "user.name", "Relocation Test"]);
        fs::write(path.join("README.md"), "retained\n").unwrap();
        git(path, &["add", "README.md"]);
        git(path, &["commit", "-m", "Initial"]);
    }

    fn move_forward(plan: &RelocationPlan) {
        fs::create_dir_all(plan.destination.parent().unwrap()).unwrap();
        fs::rename(&plan.source, &plan.destination).unwrap();
        plan.apply().unwrap();
        plan.apply().unwrap();
    }

    fn move_back(plan: &RelocationPlan) {
        plan.restore().unwrap();
        fs::rename(&plan.destination, &plan.source).unwrap();
        plan.restore().unwrap();
    }

    #[test]
    fn linked_checkout_with_external_admin_is_rejected_without_external_changes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let repository_path = root.join("origin");
        repository(&repository_path);
        let source = root.join("task");
        let checkout = source.join("ignored/worktree");
        fs::create_dir_all(checkout.parent().unwrap()).unwrap();
        git(
            &repository_path,
            &["worktree", "add", "--detach", checkout.to_str().unwrap()],
        );
        let git_directory = PathBuf::from(git(&checkout, &["rev-parse", "--absolute-git-dir"]));
        let backlink = git_directory.join("gitdir");
        let original = fs::read(&backlink).unwrap();
        let pointer = fs::read(checkout.join(".git")).unwrap();
        let destination = root.join("2026/07/task");
        let error = prepare(&source, &destination).unwrap_err().to_string();
        assert!(error.contains("outside the task scope"), "{error}");
        assert!(error.contains(git_directory.to_str().unwrap()), "{error}");
        assert_eq!(fs::read(backlink).unwrap(), original);
        assert_eq!(fs::read(checkout.join(".git")).unwrap(), pointer);
        assert!(!destination.exists());
        assert_eq!(
            git(&checkout, &["rev-parse", "--show-toplevel"]),
            checkout.to_str().unwrap()
        );
    }

    #[test]
    fn nested_primary_repository_with_external_linked_checkout_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("task");
        let repository_path = source.join("nested-repository");
        repository(&repository_path);
        let external = root.join("external-worktree");
        git(
            &repository_path,
            &["worktree", "add", "--detach", external.to_str().unwrap()],
        );
        let pointer = external.join(".git");
        let original = fs::read(&pointer).unwrap();
        let destination = root.join("2026/07/task");
        let error = prepare(&source, &destination).unwrap_err().to_string();
        assert!(error.contains("outside the task scope"), "{error}");
        assert!(error.contains(pointer.to_str().unwrap()), "{error}");
        assert_eq!(fs::read(pointer).unwrap(), original);
        assert!(!destination.exists());
        git(&external, &["status", "--porcelain"]);
    }

    #[test]
    fn both_primary_and_linked_repository_move_with_absolute_gitdir_paths() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("task");
        let main = source.join("nested-main");
        repository(&main);
        let linked = source.join("nested-linked");
        git(
            &main,
            &["worktree", "add", "--detach", linked.to_str().unwrap()],
        );
        let original_pointer = fs::read(linked.join(".git")).unwrap();
        fs::write(linked.join("ignored-cache.bin"), [0, 0xff, 42]).unwrap();
        let plan = prepare(&source, &root.join("2026/07/task")).unwrap();
        let plan: RelocationPlan =
            serde_json::from_str(&serde_json::to_string(&plan).unwrap()).unwrap();
        move_forward(&plan);
        let moved_main = plan.destination.join("nested-main");
        let moved_linked = plan.destination.join("nested-linked");
        assert_eq!(
            git(&moved_linked, &["rev-parse", "--git-common-dir"]),
            moved_main.join(".git").to_str().unwrap()
        );
        assert_eq!(
            fs::read(moved_linked.join("ignored-cache.bin")).unwrap(),
            [0, 0xff, 42]
        );
        move_back(&plan);
        assert_eq!(fs::read(linked.join(".git")).unwrap(), original_pointer);
    }

    #[test]
    fn relative_external_git_pointer_is_rejected_before_move() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let main = root.join("main");
        repository(&main);
        let source = root.join("task");
        let linked = source.join("linked");
        fs::create_dir_all(&source).unwrap();
        git(
            &main,
            &["worktree", "add", "--detach", linked.to_str().unwrap()],
        );
        let admin = PathBuf::from(git(&linked, &["rev-parse", "--absolute-git-dir"]));
        let relative = format!(
            "../../main/.git/worktrees/{}",
            admin.file_name().unwrap().to_str().unwrap()
        );
        let original = format!("gitdir: {relative}\n");
        fs::write(linked.join(".git"), &original).unwrap();
        let destination = root.join("2026/07/task");
        let error = prepare(&source, &destination).unwrap_err().to_string();
        assert!(error.contains("outside the task scope"), "{error}");
        assert!(error.contains(admin.to_str().unwrap()), "{error}");
        assert_eq!(fs::read_to_string(linked.join(".git")).unwrap(), original);
        assert!(!destination.exists());
    }

    #[test]
    fn separate_external_git_directory_is_rejected_before_changing_config() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("task");
        let checkout = source.join("separate-git-checkout");
        repository(&checkout);
        let external = root.join("separate-admin");
        git(
            &checkout,
            &["init", "--separate-git-dir", external.to_str().unwrap()],
        );
        git(
            &checkout,
            &["config", "core.worktree", checkout.to_str().unwrap()],
        );
        let config = external.join("config");
        let original = fs::read(&config).unwrap();
        let destination = root.join("2026/07/task");
        let error = prepare(&source, &destination).unwrap_err().to_string();
        assert!(error.contains("outside the task scope"), "{error}");
        assert!(error.contains(external.to_str().unwrap()), "{error}");
        assert_eq!(fs::read(config).unwrap(), original);
        assert!(!destination.exists());
        git(&checkout, &["status", "--porcelain"]);
    }

    #[test]
    fn internal_separate_git_directory_core_worktree_is_repaired_and_restored() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("task");
        let checkout = source.join("separate-git-checkout");
        repository(&checkout);
        let admin = source.join("separate-admin");
        git(
            &checkout,
            &["init", "--separate-git-dir", admin.to_str().unwrap()],
        );
        git(
            &checkout,
            &["config", "core.worktree", checkout.to_str().unwrap()],
        );
        let original = fs::read(admin.join("config")).unwrap();
        let plan = prepare(&source, &root.join("2026/07/task")).unwrap();
        move_forward(&plan);
        let moved = plan.destination.join("separate-git-checkout");
        assert_eq!(
            git(&moved, &["rev-parse", "--show-toplevel"]),
            moved.to_str().unwrap()
        );
        move_back(&plan);
        assert_eq!(fs::read(admin.join("config")).unwrap(), original);
    }

    #[test]
    fn independent_internal_git_changes_block_restore_without_overwriting_them() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("task");
        let main = source.join("main");
        repository(&main);
        let linked = source.join("linked");
        git(
            &main,
            &["worktree", "add", "--detach", linked.to_str().unwrap()],
        );
        let admin = PathBuf::from(git(&linked, &["rev-parse", "--absolute-git-dir"]));
        let plan = prepare(&source, &root.join("2026/07/task")).unwrap();
        move_forward(&plan);
        let backlink = plan
            .destination
            .join(admin.strip_prefix(&source).unwrap())
            .join("gitdir");
        fs::write(&backlink, "independent edit\n").unwrap();
        assert!(
            plan.restore()
                .unwrap_err()
                .to_string()
                .contains("changed independently")
        );
        assert_eq!(fs::read_to_string(backlink).unwrap(), "independent edit\n");
        assert!(plan.destination.is_dir());
    }

    #[test]
    fn empty_git_cache_markers_move_and_restore_without_becoming_git_pointers() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("task");
        let cache = source.join(".cache/uv/archive-v0/cache-entry");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join(".git"), []).unwrap();
        fs::write(cache.join("payload.bin"), [0xff, 0, 42]).unwrap();
        let plan = prepare(&source, &root.join("2026/07/task")).unwrap();
        assert!(plan.references.is_empty());
        move_forward(&plan);
        let relocated = plan.destination.join(".cache/uv/archive-v0/cache-entry");
        assert_eq!(fs::metadata(relocated.join(".git")).unwrap().len(), 0);
        assert_eq!(
            fs::read(relocated.join("payload.bin")).unwrap(),
            [0xff, 0, 42]
        );
        move_back(&plan);
        assert_eq!(fs::metadata(cache.join(".git")).unwrap().len(), 0);
        assert_eq!(fs::read(cache.join("payload.bin")).unwrap(), [0xff, 0, 42]);
    }

    #[test]
    fn nonempty_malformed_git_control_files_are_still_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("task");
        fs::create_dir_all(&source).unwrap();
        let destination = root.join("2026/07/task");
        for contents in [
            b"not a Git pointer\n".as_slice(),
            b" \n",
            b"gitdir: \n",
            &[0xff],
        ] {
            fs::write(source.join(".git"), contents).unwrap();
            let error = prepare(&source, &destination).unwrap_err().to_string();
            assert!(error.contains("invalid nested Git pointer"), "{error}");
            assert_eq!(fs::read(source.join(".git")).unwrap(), contents);
            assert!(!destination.exists());
        }
    }

    #[test]
    fn stale_internal_worktree_registration_is_rejected_with_repair_instructions() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("task");
        let main = source.join("main");
        repository(&main);
        let linked = source.join("linked");
        git(
            &main,
            &["worktree", "add", "--detach", linked.to_str().unwrap()],
        );
        let admin = PathBuf::from(git(&linked, &["rev-parse", "--absolute-git-dir"]));
        let original = fs::read(admin.join("gitdir")).unwrap();
        fs::remove_dir_all(&linked).unwrap();
        let destination = root.join("2026/07/task");
        let error = prepare(&source, &destination).unwrap_err().to_string();
        assert!(
            error.contains("stale nested Git worktree registration"),
            "{error}"
        );
        assert!(error.contains("git worktree repair"), "{error}");
        assert!(error.contains("git worktree prune"), "{error}");
        assert_eq!(fs::read(admin.join("gitdir")).unwrap(), original);
        assert!(!destination.exists());
    }

    #[test]
    fn external_common_git_directory_is_rejected_before_move() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("task");
        let main = source.join("main");
        repository(&main);
        let linked = source.join("linked");
        git(
            &main,
            &["worktree", "add", "--detach", linked.to_str().unwrap()],
        );
        let admin = PathBuf::from(git(&linked, &["rev-parse", "--absolute-git-dir"]));
        let external = root.join("external");
        repository(&external);
        let contents = format!("{}\n", external.join(".git").display());
        fs::write(admin.join("commondir"), &contents).unwrap();
        let destination = root.join("2026/07/task");
        let error = prepare(&source, &destination).unwrap_err().to_string();
        assert!(
            error.contains("Git common administrative directory"),
            "{error}"
        );
        assert!(error.contains("outside the task scope"), "{error}");
        assert_eq!(
            fs::read_to_string(admin.join("commondir")).unwrap(),
            contents
        );
        assert!(!destination.exists());
    }

    #[test]
    fn external_core_worktree_is_rejected_before_move() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("task");
        let main = source.join("main");
        repository(&main);
        let external = root.join("external-checkout");
        fs::create_dir_all(&external).unwrap();
        git(
            &main,
            &["config", "core.worktree", external.to_str().unwrap()],
        );
        let original = fs::read(main.join(".git/config")).unwrap();
        let destination = root.join("2026/07/task");
        let error = prepare(&source, &destination).unwrap_err().to_string();
        assert!(error.contains("Git core.worktree checkout"), "{error}");
        assert!(error.contains("outside the task scope"), "{error}");
        assert_eq!(fs::read(main.join(".git/config")).unwrap(), original);
        assert!(!destination.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_external_admin_cannot_bypass_task_scope() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("task");
        let checkout = source.join("checkout");
        repository(&checkout);
        let internal_admin = source.join("admin");
        git(
            &checkout,
            &[
                "init",
                "--separate-git-dir",
                internal_admin.to_str().unwrap(),
            ],
        );
        let external_admin = root.join("external-admin");
        fs::rename(&internal_admin, &external_admin).unwrap();
        symlink(&external_admin, &internal_admin).unwrap();
        let pointer = fs::read(checkout.join(".git")).unwrap();
        let config = fs::read(external_admin.join("config")).unwrap();
        let destination = root.join("2026/07/task");
        let error = prepare(&source, &destination).unwrap_err().to_string();
        assert!(error.contains("outside the task scope"), "{error}");
        assert!(error.contains(external_admin.to_str().unwrap()), "{error}");
        assert_eq!(fs::read(checkout.join(".git")).unwrap(), pointer);
        assert_eq!(fs::read(external_admin.join("config")).unwrap(), config);
        assert!(!destination.exists());
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

    #[test]
    fn non_relocatable_python_runtime_is_rejected_before_any_move() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("task");
        let venv = source.join(".venv");
        fs::create_dir_all(venv.join("bin")).unwrap();
        fs::write(venv.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
        let launcher = format!("#!{}/bin/python\nprint('keep me')\n", venv.display());
        fs::write(venv.join("bin/tool"), &launcher).unwrap();
        let destination = root.join("2026/07/task");
        let error = prepare(&source, &destination).unwrap_err().to_string();
        assert!(error.contains("Python virtual environment"), "{error}");
        assert!(error.contains("rebuild the runtime outside the task"));
        assert_eq!(fs::read_to_string(venv.join("bin/tool")).unwrap(), launcher);
        assert!(!destination.exists());
    }

    #[cfg(unix)]
    #[test]
    fn location_dependent_runtime_symlinks_are_rejected_without_mutation() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("task");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("runtime"), "keep\n").unwrap();
        symlink(source.join("runtime"), source.join("run")).unwrap();
        let destination = root.join("2026/07/task");
        assert!(
            prepare(&source, &destination)
                .unwrap_err()
                .to_string()
                .contains("location-dependent symlink")
        );
        assert_eq!(
            fs::read_link(source.join("run")).unwrap(),
            source.join("runtime")
        );
        assert!(!destination.exists());
    }
}
