use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use fs2::FileExt;

use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::manifest::{INFRASTRUCTURE_TASK_MANIFEST_FILE, TaskKind, parse_task_identity};
use crate::path::reject_symlink_traversal;

pub const LOCAL_STATE_PATH: &str = ".workspace-mgr/local";
/// `[l]` matches only `l`, keeping these exclusions specific to `local`.
/// A nonliteral prefix prevents Git from treating the excluded directory as
/// an explicitly requested ignored path. Cover both the entry and its files.
pub const LOCAL_STATE_EXCLUDE_PATHSPECS: [&str; 2] = [
    ":(top,exclude,glob).workspace-mgr/[l]ocal",
    ":(top,exclude,glob).workspace-mgr/[l]ocal/**",
];
pub const BUSY_MESSAGE: &str = "another workspace-mgr repository operation is running";
const MANIFEST_ALIASES: &str = "legacy-manifest-paths.json";

/// Use Git's primary checkout, rather than the current worktree or branch.
/// In particular, a separate Git directory need not be below that checkout.
pub fn directory_unmigrated(repo: &GitRepo) -> Result<PathBuf> {
    let output = repo.run_bytes(["worktree", "list", "--porcelain", "-z"], None)?;
    let listing = String::from_utf8(output.stdout)
        .map_err(|_| Error::message("primary checkout path is not UTF-8"))?;
    let first = listing.split("\0\0").next().unwrap_or_default();
    if first.split('\0').any(|field| field == "bare") {
        return Err(Error::message(
            "workspace-mgr local state requires a primary checkout",
        ));
    }
    let mut root = first
        .split('\0')
        .find_map(|field| field.strip_prefix("worktree "))
        .map(PathBuf::from)
        .ok_or_else(|| Error::message("workspace-mgr local state requires a primary checkout"))?;
    let common = repo.common_dir()?.canonicalize().at(&repo.root)?;
    if root.canonicalize().ok().as_ref() == Some(&common) {
        // With --separate-git-dir, Git lists the metadata directory as its
        // primary worktree and records no reverse pointer unless configured.
        let configured = repo.run_unchecked([
            "--git-dir",
            &common.to_string_lossy(),
            "config",
            "--local",
            "--path",
            "--get",
            "core.worktree",
        ])?;
        if !configured.success() || configured.stdout.trim().is_empty() {
            return Err(Error::message(
                "workspace-mgr cannot locate the primary checkout of a separate Git directory; configure Git core.worktree with the absolute primary checkout path",
            ));
        }
        let configured = PathBuf::from(configured.stdout.trim());
        root = if configured.is_absolute() {
            configured
        } else {
            common.join(configured)
        };
    }
    let primary = GitRepo::discover(&root).map_err(|error| {
        Error::message(format!(
            "workspace-mgr primary checkout is unavailable: {error}"
        ))
    })?;
    if primary.common_dir()?.canonicalize().at(&root)? != common
        || primary.git_dir()?.canonicalize().at(&root)? != common
    {
        return Err(Error::message(
            "workspace-mgr primary checkout does not own this repository's common Git directory",
        ));
    }
    reject_symlink_traversal(&primary.root, LOCAL_STATE_PATH, "workspace-mgr local state")?;
    let directory = primary.root.join(LOCAL_STATE_PATH);
    if directory.exists() && !directory.is_dir() {
        return Err(Error::message(
            "workspace-mgr local state must be a directory",
        ));
    }
    // Even forced tracked files cannot serve as private product state.
    if !primary
        .run(["ls-files", "--", LOCAL_STATE_PATH])?
        .stdout
        .is_empty()
    {
        return Err(Error::message(
            "workspace-mgr local state must not contain tracked files",
        ));
    }
    Ok(directory)
}

pub fn directory(repo: &GitRepo) -> Result<PathBuf> {
    let directory = directory_unmigrated(repo)?;
    if !legacy_directories(repo)?.is_empty() {
        // RepositoryLock migrates while holding both the new and legacy locks.
        let _lock = crate::lock::RepositoryLock::acquire(repo)?;
    }
    Ok(directory)
}

fn legacy_directories(repo: &GitRepo) -> Result<Vec<PathBuf>> {
    let common = repo.common_dir()?.canonicalize().at(&repo.root)?;
    let mut directories = Vec::new();
    let root = common.join("workspace-mgr");
    if fs::symlink_metadata(&root).is_ok() {
        directories.push(root);
    }
    let worktrees = common.join("worktrees");
    if worktrees.is_dir() {
        for entry in fs::read_dir(&worktrees).at(&worktrees)? {
            let entry = entry.at(&worktrees)?;
            let directory = entry.path().join("workspace-mgr");
            if fs::symlink_metadata(&directory).is_ok() {
                directories.push(directory);
            }
        }
    }
    directories.sort();
    Ok(directories)
}

pub fn open_lock(path: &Path, create: bool) -> Result<File> {
    if fs::symlink_metadata(path)
        .is_ok_and(|metadata| !metadata.is_file() || metadata.file_type().is_symlink())
    {
        return Err(Error::message(format!(
            "repository lock must be a regular file: {}",
            path.display()
        )));
    }
    let file = OpenOptions::new()
        .create(create)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .at(path)?;
    file.try_lock_exclusive()
        .map_err(|_| Error::message(BUSY_MESSAGE))?;
    Ok(file)
}

/// Preflight the complete migration before moving anything. Interrupted moves
/// can be resumed, and identical duplicates can be removed without overwrites.
pub fn migrate(repo: &GitRepo, destination: &Path) -> Result<Vec<File>> {
    let directories = legacy_directories(repo)?;
    let mut guards = Vec::new();
    let mut moves = Vec::new();
    let common = repo.common_dir()?.canonicalize().at(&repo.root)?;
    let mut aliases = load_aliases(destination)?;
    let previous_aliases = aliases.clone();
    for directory in &directories {
        let metadata = fs::symlink_metadata(directory).at(directory)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(Error::message(format!(
                "legacy workspace-mgr state must be a directory: {}",
                directory.display()
            )));
        }
        guards.push(open_lock(&directory.join("repository.lock"), true)?);
        for (key, value) in load_aliases(directory)? {
            if aliases.get(&key).is_some_and(|existing| existing != &value) {
                return Err(conflict(&directory.join(MANIFEST_ALIASES)));
            }
            aliases.insert(key, value);
        }
        collect_moves(directory, destination, true, &mut moves)?;
    }
    // Two old worktrees must not silently overwrite one another either.
    let mut destinations = std::collections::BTreeMap::<PathBuf, PathBuf>::new();
    for (source, target) in &moves {
        check_target(source, target)?;
        if directories
            .iter()
            .any(|directory| source == &directory.join("task.toml"))
        {
            let key =
                crate::path::to_slash(source.strip_prefix(&common).map_err(|_| conflict(source))?);
            let value = crate::path::to_slash(
                target
                    .strip_prefix(destination)
                    .map_err(|_| conflict(target))?,
            );
            if aliases.get(&key).is_some_and(|old| old != &value) {
                return Err(conflict(source));
            }
            aliases.insert(key, value);
        }
        if let Some(previous) = destinations.insert(target.clone(), source.clone()) {
            if !same_file(&previous, source)? {
                return Err(conflict(target));
            }
        }
    }
    for target in destinations.keys() {
        let mut parent = target.parent();
        while let Some(path) = parent {
            if destinations.contains_key(path) {
                return Err(conflict(path));
            }
            if path == destination {
                break;
            }
            parent = path.parent();
        }
    }
    if aliases != previous_aliases {
        let path = destination.join(MANIFEST_ALIASES);
        let mut temporary = tempfile::NamedTempFile::new_in(destination).at(destination)?;
        serde_json::to_writer(&mut temporary, &aliases).map_err(|error| {
            Error::message(format!("failed to encode legacy manifest paths: {error}"))
        })?;
        temporary.as_file().sync_all().at(&path)?;
        temporary.persist(&path).map_err(|error| Error::Io {
            path,
            source: error.error,
        })?;
    }
    for (source, target) in moves {
        move_file(&source, &target)?;
    }
    for directory in directories {
        remove_empty_directories(&directory)?;
        if directory.join(MANIFEST_ALIASES).is_file() {
            fs::remove_file(directory.join(MANIFEST_ALIASES)).at(&directory)?;
        }
        fs::remove_file(directory.join("repository.lock")).at(&directory)?;
        fs::remove_dir(&directory).at(&directory)?;
    }
    Ok(guards)
}

fn collect_moves(
    source: &Path,
    target: &Path,
    is_root: bool,
    moves: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<()> {
    for entry in fs::read_dir(source).at(source)? {
        let entry = entry.at(source)?;
        if is_root
            && (entry.file_name() == "repository.lock" || entry.file_name() == MANIFEST_ALIASES)
        {
            continue;
        }
        let from = entry.path();
        let mut to = target.join(entry.file_name());
        if entry.file_name() == "task.toml" && is_root {
            let raw = fs::read_to_string(&from).at(&from)?;
            let manifest: toml::Value = toml::from_str(&raw).map_err(|error| {
                Error::message(format!("invalid legacy infrastructure manifest: {error}"))
            })?;
            let id = manifest
                .get("id")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| Error::message("legacy infrastructure manifest has no task id"))?;
            parse_task_identity(TaskKind::Infrastructure, id)?;
            to = target
                .join("infrastructure-tasks")
                .join(id)
                .join(INFRASTRUCTURE_TASK_MANIFEST_FILE);
        }
        let metadata = fs::symlink_metadata(&from).at(&from)?;
        if metadata.is_dir() {
            if let Ok(existing) = fs::symlink_metadata(&to) {
                if !existing.is_dir() || existing.file_type().is_symlink() {
                    return Err(conflict(&to));
                }
            }
            collect_moves(&from, &to, false, moves)?;
        } else if metadata.is_file() || metadata.file_type().is_symlink() {
            moves.push((from, to));
        } else {
            return Err(Error::message(format!(
                "unsupported legacy state file: {}",
                from.display()
            )));
        }
    }
    Ok(())
}

fn load_aliases(local: &Path) -> Result<std::collections::BTreeMap<String, String>> {
    let path = local.join(MANIFEST_ALIASES);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| Error::message(format!("invalid legacy manifest paths: {error}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(source) => Err(Error::Io { path, source }),
    }
}

pub fn legacy_manifest_target(local: &Path, common: &Path, old_path: &Path) -> Result<PathBuf> {
    let key = crate::path::to_slash(
        old_path
            .strip_prefix(common)
            .map_err(|_| conflict(old_path))?,
    );
    let aliases = load_aliases(local)?;
    let relative = aliases.get(&key).ok_or_else(|| {
        Error::message(format!(
            "legacy infrastructure manifest was not migrated: {}",
            old_path.display()
        ))
    })?;
    let relative = crate::path::repo_path(relative, "legacy manifest destination")?;
    reject_symlink_traversal(local, &relative, "legacy manifest destination")?;
    Ok(local.join(relative))
}

fn conflict(path: &Path) -> Error {
    Error::message(format!(
        "conflicting workspace-mgr local state at {}; both copies were preserved",
        path.display()
    ))
}

fn check_target(source: &Path, target: &Path) -> Result<()> {
    let mut parent = target.parent();
    while let Some(path) = parent {
        match fs::symlink_metadata(path) {
            Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
                return Err(conflict(path));
            }
            Ok(_) => parent = path.parent(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => parent = path.parent(),
            Err(source) => {
                return Err(Error::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }
    }
    match fs::symlink_metadata(target) {
        Ok(_) if !same_file(source, target)? => Err(conflict(target)),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Io {
            path: target.to_path_buf(),
            source,
        }),
    }
}

fn same_file(left: &Path, right: &Path) -> Result<bool> {
    let a = fs::symlink_metadata(left).at(left)?;
    let b = fs::symlink_metadata(right).at(right)?;
    if a.file_type().is_symlink() && b.file_type().is_symlink() {
        return Ok(fs::read_link(left).at(left)? == fs::read_link(right).at(right)?);
    }
    if !a.is_file() || !b.is_file() || a.len() != b.len() {
        return Ok(false);
    }
    // Stream potentially large quarantine files instead of reading them whole.
    use std::io::Read;
    let mut a = File::open(left).at(left)?;
    let mut b = File::open(right).at(right)?;
    let mut a_buffer = [0_u8; 65_536];
    let mut b_buffer = [0_u8; 65_536];
    loop {
        let n = a.read(&mut a_buffer).at(left)?;
        b.read_exact(&mut b_buffer[..n]).at(right)?;
        if a_buffer[..n] != b_buffer[..n] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

fn move_file(source: &Path, target: &Path) -> Result<()> {
    if fs::symlink_metadata(target).is_ok() {
        if !same_file(source, target)? {
            return Err(conflict(target));
        }
    } else {
        let parent = target
            .parent()
            .ok_or_else(|| Error::message("local state path has no parent"))?;
        fs::create_dir_all(parent).at(parent)?;
        if fs::symlink_metadata(source)
            .at(source)?
            .file_type()
            .is_symlink()
        {
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(source).at(source)?, target).at(target)?;
            #[cfg(not(unix))]
            return Err(Error::message(
                "legacy state symlinks are unsupported on this platform",
            ));
        } else if fs::hard_link(source, target).is_err() {
            // Separate Git directories may be on a different filesystem.
            let mut temporary = tempfile::NamedTempFile::new_in(parent).at(parent)?;
            fs::copy(source, temporary.path()).at(source)?;
            temporary.flush().at(target)?;
            temporary.as_file().sync_all().at(target)?;
            temporary
                .persist_noclobber(target)
                .map_err(|error| Error::Io {
                    path: target.to_path_buf(),
                    source: error.error,
                })?;
        }
    }
    fs::remove_file(source).at(source)
}

fn remove_empty_directories(root: &Path) -> Result<()> {
    for entry in fs::read_dir(root).at(root)? {
        let entry = entry.at(root)?;
        if entry.file_type().at(entry.path())?.is_dir() {
            remove_empty_directories(&entry.path())?;
            fs::remove_dir(entry.path()).at(root)?;
        }
    }
    Ok(())
}

/// Canonicalize the existing prefix, including symlinked cwd spellings, while
/// retaining a missing tail for old manifest paths after migration.
pub fn absolute_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().at(path)?.join(path)
    };
    let mut prefix = absolute.clone();
    let mut tail = Vec::new();
    loop {
        match prefix.canonicalize() {
            Ok(mut canonical) => {
                for part in tail.iter().rev() {
                    canonical.push(part);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tail.push(
                    prefix
                        .file_name()
                        .ok_or_else(|| Error::message("manifest path has no existing ancestor"))?
                        .to_os_string(),
                );
                if !prefix.pop() {
                    return Err(Error::message("manifest path has no existing ancestor"));
                }
            }
            Err(source) => {
                return Err(Error::Io {
                    path: prefix,
                    source,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, GitRepo, PathBuf) {
        let temporary = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: temporary.path().canonicalize().unwrap(),
        };
        repo.run(["init", "-q", "-b", "main"]).unwrap();
        let local = directory_unmigrated(&repo).unwrap();
        fs::create_dir_all(&local).unwrap();
        (temporary, repo, local)
    }

    #[test]
    fn planned_file_and_descendant_conflicts_are_found_before_migration() {
        let (_temporary, repo, local) = fixture();
        let legacy = repo.root.join(".git/workspace-mgr");
        let worktree_legacy = repo.root.join(".git/worktrees/legacy/workspace-mgr");
        fs::create_dir_all(legacy.join("state")).unwrap();
        fs::create_dir_all(worktree_legacy.join("state/value")).unwrap();
        fs::write(legacy.join("state/value"), b"a file, not a directory").unwrap();
        fs::write(
            worktree_legacy.join("state/value/child"),
            b"retained worktree state",
        )
        .unwrap();
        assert!(
            migrate(&repo, &local)
                .unwrap_err()
                .to_string()
                .contains("conflicting workspace-mgr local state")
        );
        assert_eq!(
            fs::read(legacy.join("state/value")).unwrap(),
            b"a file, not a directory"
        );
        assert_eq!(
            fs::read(worktree_legacy.join("state/value/child")).unwrap(),
            b"retained worktree state"
        );
        assert!(!local.join("state/value").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_remapped_manifest_cannot_escape_through_a_destination_symlink() {
        let (_temporary, repo, local) = fixture();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(outside.path().join("infra-example")).unwrap();
        std::os::unix::fs::symlink(outside.path(), local.join("infrastructure-tasks")).unwrap();
        let legacy = repo.root.join(".git/workspace-mgr");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("task.toml"), "id = \"infra-example\"\n").unwrap();
        assert!(migrate(&repo, &local).is_err());
        assert!(legacy.join("task.toml").is_file());
        assert!(
            !outside
                .path()
                .join("infra-example")
                .join(INFRASTRUCTURE_TASK_MANIFEST_FILE)
                .exists()
        );
    }
}
