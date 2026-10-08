//! Nested repositories are opaque local payloads, never part of the outer
//! repository's publication. Archive checks that boundary and leaves their
//! contents, pointers, runtime paths and external registrations untouched.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use walkdir::WalkDir;

use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;

/// Require every nested repository to be untracked and excluded by a shared
/// repository ignore rule. User/global rules alone cannot establish this
/// boundary for another checkout.
pub(crate) fn validate(repo: &GitRepo, source: &str) -> Result<()> {
    validate_roots(repo, &roots(repo, source)?)
}

fn validate_roots(repo: &GitRepo, roots: &[String]) -> Result<()> {
    if roots.is_empty() {
        return Ok(());
    }
    let literals = roots
        .iter()
        .map(|root| format!(":(literal){root}"))
        .collect::<Vec<_>>();
    let mut tracked = BTreeSet::new();
    for batch in crate::git::pathspec_batches(&literals) {
        let mut args = vec!["ls-files", "-z", "--"];
        args.extend(batch.iter().map(String::as_str));
        let output = repo.run_bytes(args, None)?;
        tracked.extend(
            output
                .stdout
                .split(|byte| *byte == 0)
                .filter(|path| !path.is_empty())
                .map(<[u8]>::to_vec),
        );
    }
    let rules = shared_ignore_rules(repo, roots, None)?;
    for root in roots {
        if contains_tracked_path(&tracked, root.as_bytes()) {
            return Err(Error::message(format!(
                "nested Git repository {root:?} is tracked by the outer repository (including gitlinks); remove it from the outer Git index and ignore the entire directory in a shared .gitignore"
            )));
        }
        let (source, pattern) = &rules[root];
        validate_shared_ignore_rule(root, source, pattern)?;
    }
    Ok(())
}

/// Check both the current boundary and the rules that the move will leave at
/// its destination. A task-local relative ignore travels with the task; a
/// root ignore tied to only its old absolute repository path does not.
pub(crate) fn validate_move(repo: &GitRepo, source: &str, destination: &str) -> Result<()> {
    let source = crate::path::repo_path(source, "archive source")?;
    let destination = crate::path::repo_path(destination, "archive destination")?;
    let roots = roots(repo, &source)?;
    validate_roots(repo, &roots)?;
    if roots.is_empty() {
        return Ok(());
    }
    let shadow = tempfile::tempdir().map_err(|source| Error::Io {
        path: std::env::temp_dir(),
        source,
    })?;
    let git_dir = repo.git_dir()?;
    let git_dir = git_dir.canonicalize().at(&git_dir)?;
    let mut future_roots = Vec::new();
    let mut copied_rules = BTreeSet::new();
    for root in &roots {
        let nested = Path::new(root)
            .strip_prefix(&source)
            .map_err(|_| Error::message("nested Git root is outside archive source"))?;
        let future_root = Path::new(&destination).join(nested);
        let target = shadow.path().join(&future_root);
        fs::create_dir_all(&target).at(&target)?;
        // Copy precisely the rules Git can encounter on its way to this
        // directory, replacing the moved task's prefix where appropriate.
        let mut parent = future_root.parent();
        while let Some(directory) = parent {
            let current = if let Ok(within_task) = directory.strip_prefix(&destination) {
                Path::new(&source).join(within_task)
            } else {
                directory.to_path_buf()
            };
            let ignore = repo.root.join(current).join(".gitignore");
            if copied_rules.insert(directory.to_path_buf())
                && metadata(&ignore)?.is_some_and(|value| value.is_file())
            {
                let copied = shadow.path().join(directory).join(".gitignore");
                if let Some(parent) = copied.parent() {
                    fs::create_dir_all(parent).at(parent)?;
                }
                fs::copy(&ignore, &copied).at(&copied)?;
            }
            parent = directory.parent();
        }
        future_roots.push(crate::path::to_slash(&future_root));
    }
    let shadow_repo = GitRepo {
        root: shadow.path().to_path_buf(),
    };
    let rules = shared_ignore_rules(&shadow_repo, &future_roots, Some(&git_dir))?;
    for (root, future_root) in roots.iter().zip(&future_roots) {
        let (source, pattern) = &rules[future_root];
        validate_shared_ignore_rule(future_root, source, pattern).map_err(|error| {
            Error::message(format!(
                "archive would leave nested Git repository {root:?} unignored at {future_root:?}: {error}; use a task-local relative .gitignore rule or a shared rule that also covers the destination"
            ))
        })?;
    }
    Ok(())
}

/// Return repository-relative roots without descending into their payloads.
/// Callers that scan storage metadata can skip these opaque directories.
/// This discovers controls from filesystem metadata only: no nested Git
/// command, pointer parsing, config read, or external-path access is performed.
pub(crate) fn roots(repo: &GitRepo, source: &str) -> Result<Vec<String>> {
    let source = crate::path::repo_path(source, "archive source")?;
    crate::path::reject_symlink_traversal(&repo.root, &source, "archive source")?;
    let source_path = repo.root.join(&source);
    let mut roots = indexed_gitlinks(repo, &source)?;
    let opaque = opaque_ignored_directories(repo, &source)?;
    let indexed_roots = roots
        .iter()
        .map(|root| repo.root.join(root))
        .collect::<BTreeSet<_>>();
    let mut entries = WalkDir::new(&source_path).follow_links(false).into_iter();
    while let Some(entry) = entries.next() {
        let entry = entry
            .map_err(|error| Error::message(format!("inspect nested Git boundaries: {error}")))?;
        if !entry.file_type().is_dir() {
            continue;
        }
        let relative_bytes = entry
            .path()
            .strip_prefix(&repo.root)
            .expect("walker is inside the repository")
            .as_os_str()
            .as_encoded_bytes();
        let ignored = opaque.contains(relative_bytes);
        let marker = match repository_marker(entry.path()) {
            Ok(marker) => marker,
            // The outer ignore boundary was established without reading this
            // directory's children. An inaccessible ordinary ignored tree
            // remains opaque rather than becoming an archive content gate.
            Err(Error::Io { source, .. })
                if ignored && source.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                entries.skip_current_dir();
                continue;
            }
            Err(error) => return Err(error),
        };
        // Ordinary directory names are payload, not workspace-mgr paths.
        // Only a real Git boundary needs conversion for the ignore query.
        if indexed_roots.contains(entry.path()) || marker {
            let relative = crate::path::relative_to(entry.path(), &repo.root, "nested Git root")?;
            roots.insert(relative);
            entries.skip_current_dir();
        } else if ignored {
            // Outer Git ignore declares this complete, untracked local tree.
            // Do not inspect or infer repositories hidden inside it, and do
            // not impose destination requirements on ordinary cache content.
            entries.skip_current_dir();
        }
    }
    // A gitlink can be listed below a containing repository, but only the
    // outer opaque boundary needs to be returned or checked.
    let mut result = Vec::<String>::new();
    for root in roots {
        if !result
            .iter()
            .any(|parent| root.starts_with(&format!("{parent}/")))
        {
            result.push(root);
        }
    }
    Ok(result)
}

fn opaque_ignored_directories(repo: &GitRepo, source: &str) -> Result<BTreeSet<Vec<u8>>> {
    let literal = format!(":(literal){source}");
    let tracked = repo.run_bytes(["ls-files", "-z", "--", &literal], None)?;
    let tracked = tracked
        .stdout
        .split(|byte| *byte == b'\0')
        .filter(|path| !path.is_empty())
        .map(<[u8]>::to_vec)
        .collect::<BTreeSet<_>>();
    let ignored = repo.run_bytes(
        [
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "-z",
            "--",
            &literal,
        ],
        None,
    )?;
    let mut candidates = BTreeSet::new();
    for path in ignored.stdout.split(|byte| *byte == b'\0') {
        let Some(directory) = path.strip_suffix(b"/") else {
            continue;
        };
        if !contains_tracked_path(&tracked, directory) {
            candidates.insert(directory.to_vec());
        }
    }
    if candidates.is_empty() {
        return Ok(BTreeSet::new());
    }
    let mut request = Vec::new();
    for path in &candidates {
        request.extend_from_slice(path);
        request.push(0);
    }
    let output = crate::process::run_bytes(
        "git",
        [
            "-C",
            &repo.root.to_string_lossy(),
            "check-ignore",
            "-v",
            "-z",
            "--no-index",
            "--stdin",
        ],
        &repo.root,
        &BTreeMap::new(),
        Some(&request),
        false,
    )?;
    if !matches!(output.code, 0 | 1) {
        return Err(Error::message(format!(
            "cannot inspect outer Git ignore boundaries: {}",
            output.stderr.trim()
        )));
    }
    let fields = output
        .stdout
        .split(|byte| *byte == b'\0')
        .collect::<Vec<_>>();
    let mut opaque = BTreeSet::new();
    for fields in fields.as_chunks::<4>().0 {
        if candidates.contains(fields[3]) && !fields[0].is_empty() && !fields[2].starts_with(b"!") {
            opaque.insert(fields[3].to_vec());
        }
    }
    Ok(opaque)
}

fn contains_tracked_path(tracked: &BTreeSet<Vec<u8>>, root: &[u8]) -> bool {
    let mut prefix = root.to_vec();
    prefix.push(b'/');
    tracked.contains(root)
        || tracked
            .range(prefix.clone()..)
            .next()
            .is_some_and(|path| path.starts_with(&prefix))
}

fn shared_ignore_source(raw: &[u8]) -> bool {
    let Ok(raw) = std::str::from_utf8(raw) else {
        return false;
    };
    let source = Path::new(raw);
    !source.is_absolute()
        && source.file_name().is_some_and(|name| name == ".gitignore")
        && !source.components().any(|component| {
            matches!(component, std::path::Component::ParentDir) || component.as_os_str() == ".git"
        })
}

fn indexed_gitlinks(repo: &GitRepo, source: &str) -> Result<BTreeSet<String>> {
    let listing = repo.run_bytes(
        [
            "ls-files",
            "--stage",
            "-z",
            "--",
            &format!(":(literal){source}"),
        ],
        None,
    )?;
    let mut roots = BTreeSet::new();
    for entry in listing.stdout.split(|byte| *byte == b'\0') {
        // Do not decode or constrain unrelated tracked payload names.
        if !entry.starts_with(b"160000 ") {
            continue;
        }
        let Some(separator) = entry.iter().position(|byte| *byte == b'\t') else {
            return Err(Error::message("invalid outer Git index listing"));
        };
        let path = std::str::from_utf8(&entry[separator + 1..])
            .map_err(|_| Error::message("nested Git index paths must be UTF-8"))?;
        roots.insert(path.to_owned());
    }
    Ok(roots)
}

fn metadata(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn repository_marker(directory: &Path) -> Result<bool> {
    if let Some(control) = metadata(&directory.join(".git"))? {
        // A zero-byte regular .git file is a common uv/cache marker. A
        // nonempty pointer is an opaque control regardless of whether its
        // external target remains available after a move.
        if control.is_dir()
            || control.file_type().is_symlink()
            || (control.is_file() && control.len() > 0)
        {
            return Ok(true);
        }
    }
    // Recognizable bare repositories have no .git directory. Recognize their
    // local shape without opening HEAD, config, objects, refs, or links.
    Ok(
        metadata(&directory.join("HEAD"))?.is_some_and(|value| value.is_file())
            && metadata(&directory.join("objects"))?.is_some_and(|value| value.is_dir())
            && (metadata(&directory.join("refs"))?.is_some_and(|value| value.is_dir())
                || metadata(&directory.join("packed-refs"))?.is_some_and(|value| value.is_file())),
    )
}

fn shared_ignore_rules(
    repo: &GitRepo,
    roots: &[String],
    git_dir: Option<&Path>,
) -> Result<BTreeMap<String, (String, String)>> {
    let mut input = Vec::new();
    for root in roots {
        input.extend_from_slice(root.as_bytes());
        input.push(0);
    }
    let mut args = vec!["-C".to_owned(), repo.root.to_string_lossy().into_owned()];
    if let Some(git_dir) = git_dir {
        args.extend([
            "--git-dir".to_owned(),
            git_dir.to_string_lossy().into_owned(),
            "--work-tree".to_owned(),
            repo.root.to_string_lossy().into_owned(),
        ]);
    }
    args.extend(
        [
            "check-ignore",
            "-v",
            "-z",
            "--non-matching",
            "--no-index",
            "--stdin",
        ]
        .map(str::to_owned),
    );
    let result = crate::process::run_bytes(
        "git",
        args,
        &repo.root,
        &BTreeMap::new(),
        Some(&input),
        false,
    )?;
    if result.code != 0 && result.code != 1 {
        return Err(Error::message(format!(
            "cannot verify the Git ignore boundaries for nested repositories {roots:?}: {}",
            result.stderr.trim()
        )));
    }
    let raw = std::str::from_utf8(&result.stdout)
        .map_err(|_| Error::message("nested Git ignore rules must be UTF-8"))?;
    let fields = raw.split_terminator('\0').collect::<Vec<_>>();
    if fields.len() != roots.len() * 4 {
        return Err(Error::message(
            "Git returned an incomplete nested ignore inventory",
        ));
    }
    let mut rules = BTreeMap::new();
    for (root, fields) in roots.iter().zip(fields.as_chunks::<4>().0) {
        if fields[3] != root {
            return Err(Error::message(
                "Git returned a mismatched nested ignore path",
            ));
        }
        rules.insert(root.clone(), (fields[0].to_owned(), fields[2].to_owned()));
    }
    Ok(rules)
}

fn validate_shared_ignore_rule(root: &str, source: &str, pattern: &str) -> Result<()> {
    if source.is_empty() || pattern.starts_with('!') {
        return Err(Error::message(format!(
            "nested Git repository {root:?} must be ignored as an entire directory by a shared .gitignore before archiving"
        )));
    }
    if !shared_ignore_source(source.as_bytes()) {
        return Err(local_ignore_error(root, Path::new(source)));
    }
    // Repository and task-local ignore rules are shareable by publication,
    // including a rule introduced by the archive's current task changes.
    // Git ignores symlinked .gitignore files, so no target is opened here.
    crate::path::repo_path(source, "nested Git ignore source")?;
    Ok(())
}

fn local_ignore_error(root: &str, source: &Path) -> Error {
    Error::message(format!(
        "nested Git repository {root:?} is excluded only by a machine-local ignore rule in {}; carry the rule in a repository or task .gitignore before archiving",
        source.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    struct Fixture {
        _temp: tempfile::TempDir,
        repo: GitRepo,
    }

    fn git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .trim_end()
            .to_owned()
    }

    fn initialize(root: &Path) {
        fs::create_dir_all(root).unwrap();
        git(root, &["init", "-b", "main"]);
        git(root, &["config", "user.name", "Nested Git Test"]);
        git(root, &["config", "user.email", "test@example.invalid"]);
        git(root, &["config", "core.excludesFile", ""]);
        fs::write(root.join("README.md"), "fixture\n").unwrap();
        git(root, &["add", "README.md"]);
        git(root, &["commit", "-m", "Initial"]);
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("outer");
            initialize(&root);
            fs::create_dir_all(root.join("task")).unwrap();
            Self {
                _temp: temp,
                repo: GitRepo::discover(&root).unwrap(),
            }
        }

        fn write(&self, path: &str, bytes: impl AsRef<[u8]>) {
            let absolute = self.repo.root.join(path);
            fs::create_dir_all(absolute.parent().unwrap()).unwrap();
            fs::write(absolute, bytes).unwrap();
        }

        fn marker(&self, directory: &str) {
            fs::create_dir_all(self.repo.root.join(directory).join(".git")).unwrap();
        }
    }

    #[test]
    fn batched_nested_boundaries_keep_literal_paths_and_per_root_rule_failures() {
        let fixture = Fixture::new();
        for root in ["task/[literal]", "task/back\\slash", "task/with space"] {
            fixture.marker(root);
        }
        fixture.write(
            "task/.gitignore",
            "/[[]literal]/\n/back\\\\slash/\n/with space/\n",
        );
        validate(&fixture.repo, "task").unwrap();
        validate_move(&fixture.repo, "task", "archive/task").unwrap();
        fixture.write("task/.gitignore", "/[[]literal]/\n/back\\\\slash/\n");
        let error = validate(&fixture.repo, "task").unwrap_err().to_string();
        assert!(error.contains("task/with space"), "{error}");
        assert!(
            error.contains("must be ignored as an entire directory"),
            "{error}"
        );
        fixture.write(
            "task/.gitignore",
            "/[[]literal]/\n/back\\\\slash/\n/with space/\n",
        );
        fixture.write("task/with space/forced", "tracked\n");
        git(
            &fixture.repo.root,
            &["add", "-f", "--", ":(literal)task/with space/forced"],
        );
        let error = validate(&fixture.repo, "task").unwrap_err().to_string();
        assert!(error.contains("task/with space"), "{error}");
        assert!(
            error.contains("is tracked by the outer repository"),
            "{error}"
        );
    }

    #[test]
    fn task_local_unpublished_ignore_keeps_nested_payload_opaque() {
        let fixture = Fixture::new();
        fixture.marker("task/vendor");
        fixture.write("task/.gitignore", "/vendor/\n");
        fixture.write("task/vendor/pyvenv.cfg", "/old/task/location\n");
        fixture.write(
            "task/vendor/data.dvc",
            "invalid unrelated storage metadata\n",
        );
        // No nested payload file, including this invalid pointer, is read.
        fixture.write("task/vendor/deeper/.git", [0xff, 0x00, 0xff]);
        validate(&fixture.repo, "task").unwrap();
        validate_move(&fixture.repo, "task", "archive/2026/10/task").unwrap();
        assert_eq!(roots(&fixture.repo, "task").unwrap(), ["task/vendor"]);
    }

    #[test]
    fn unignored_repository_or_only_ignored_control_directory_is_rejected() {
        let fixture = Fixture::new();
        fixture.marker("task/vendor");
        let error = validate(&fixture.repo, "task").unwrap_err().to_string();
        assert!(error.contains("must be ignored as an entire directory"));
        fixture.write("task/.gitignore", "/vendor/.git/\n");
        assert!(validate(&fixture.repo, "task").is_err());
    }

    #[test]
    fn root_shared_wildcard_rule_covers_source_and_destination() {
        let fixture = Fixture::new();
        fixture.marker("task/vendor");
        fixture.write(".gitignore", "vendor/\n");
        validate_move(&fixture.repo, "task", "archive/2026/10/task").unwrap();
    }

    #[test]
    fn root_rule_tied_to_only_old_location_is_refused_before_move() {
        let fixture = Fixture::new();
        fixture.marker("task/vendor");
        fixture.write(".gitignore", "/task/vendor/\n");
        validate(&fixture.repo, "task").unwrap();
        let error = validate_move(&fixture.repo, "task", "archive/2026/10/task")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("would leave nested Git repository"),
            "{error}"
        );
        assert!(fixture.repo.root.join("task/vendor/.git").is_dir());
        assert!(!fixture.repo.root.join("archive").exists());
    }

    #[test]
    fn existing_destination_negation_is_considered() {
        let fixture = Fixture::new();
        fixture.marker("task/vendor");
        fixture.write(".gitignore", "vendor/\n");
        fixture.write("archive/.gitignore", "!vendor/\n");
        assert!(validate_move(&fixture.repo, "task", "archive/task").is_err());
    }

    #[test]
    fn global_and_info_exclude_rules_are_not_shared_boundaries() {
        let fixture = Fixture::new();
        fixture.marker("task/vendor");
        fixture.write(".git/info/exclude", "/task/vendor/\n");
        let error = validate(&fixture.repo, "task").unwrap_err().to_string();
        assert!(error.contains("machine-local ignore rule"), "{error}");
        fixture.write(".git/info/exclude", "");
        let global = fixture._temp.path().join("global-ignore");
        fs::write(&global, "vendor/\n").unwrap();
        git(
            &fixture.repo.root,
            &["config", "core.excludesFile", global.to_str().unwrap()],
        );
        assert!(validate(&fixture.repo, "task").is_err());
        fixture.write("task/.gitignore", "vendor/\n");
        validate_move(&fixture.repo, "task", "archive/task").unwrap();
    }

    #[test]
    fn tracked_gitlink_is_rejected_even_with_ignore_rule() {
        let fixture = Fixture::new();
        fixture.marker("task/vendor");
        fixture.write("task/.gitignore", "/vendor/\n");
        let oid = git(&fixture.repo.root, &["rev-parse", "HEAD"]);
        git(
            &fixture.repo.root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{oid},task/vendor"),
            ],
        );
        let error = validate(&fixture.repo, "task").unwrap_err().to_string();
        assert!(
            error.contains("is tracked by the outer repository"),
            "{error}"
        );
    }

    #[test]
    fn tracked_regular_files_inside_nested_repository_are_rejected() {
        let fixture = Fixture::new();
        fixture.write("task/vendor/file.txt", "tracked\n");
        git(&fixture.repo.root, &["add", "task/vendor/file.txt"]);
        fixture.marker("task/vendor");
        fixture.write("task/.gitignore", "/vendor/\n");
        assert!(validate(&fixture.repo, "task").is_err());
    }

    #[test]
    fn zero_byte_cache_git_file_does_not_establish_repository() {
        let fixture = Fixture::new();
        fixture.write("task/cache/.git", []);
        fixture.write("task/cache/plain", "ordinary cache\n");
        assert!(roots(&fixture.repo, "task").unwrap().is_empty());
        validate_move(&fixture.repo, "task", "archive/task").unwrap();
    }

    #[test]
    fn ordinary_directory_names_are_not_subject_to_git_boundary_path_policy() {
        let fixture = Fixture::new();
        fixture.write("task/prior\nrun/.git", []);
        fixture.write("task/prior\nrun/results", "ordinary payload\n");
        fixture.write("task/plain\tcache/item", "more ordinary payload\n");
        assert!(roots(&fixture.repo, "task").unwrap().is_empty());
        validate_move(&fixture.repo, "task", "archive/task").unwrap();
        assert_eq!(
            fs::read(fixture.repo.root.join("task/prior\nrun/results")).unwrap(),
            b"ordinary payload\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn shared_ignored_untracked_runtime_tree_is_not_inspected() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new();
        fixture.write("task/.gitignore", ".venv/\n.chat-sync-state/\n");
        fixture.write("task/.venv/private", b"retained environment bytes\n");
        fixture.write("task/.chat-sync-state/private", b"retained runtime state\n");
        let directories = [
            fixture.repo.root.join("task/.venv"),
            fixture.repo.root.join("task/.chat-sync-state"),
        ];
        let permissions = directories
            .iter()
            .map(|path| fs::metadata(path).unwrap().permissions())
            .collect::<Vec<_>>();
        for path in &directories {
            fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
        }
        let discovered = roots(&fixture.repo, "task");
        let validated = validate_move(&fixture.repo, "task", "archive/task");
        for (path, permissions) in directories.iter().zip(permissions) {
            fs::set_permissions(path, permissions).unwrap();
        }
        assert!(discovered.unwrap().is_empty());
        validated.unwrap();
        assert_eq!(
            fs::read(fixture.repo.root.join("task/.venv/private")).unwrap(),
            b"retained environment bytes\n"
        );
    }

    #[test]
    fn ordinary_ignored_cache_does_not_gain_a_destination_ignore_gate() {
        let fixture = Fixture::new();
        fixture.write(".gitignore", "/task/cache/\n");
        fixture.write("task/cache/old-run", "historical ordinary cache\n");
        assert!(roots(&fixture.repo, "task").unwrap().is_empty());
        validate_move(&fixture.repo, "task", "archive/task").unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn ordinary_machine_local_ignored_caches_remain_opaque_without_destination_gates() {
        use std::os::unix::fs::PermissionsExt;

        for mechanism in ["info", "global"] {
            let fixture = Fixture::new();
            fixture.write("task/cache/private", b"ordinary retained cache\n");
            if mechanism == "info" {
                fixture.write(".git/info/exclude", "/task/cache/\n");
            } else {
                let global = fixture._temp.path().join("global-ignore");
                fs::write(&global, "/task/cache/\n").unwrap();
                git(
                    &fixture.repo.root,
                    &["config", "core.excludesFile", global.to_str().unwrap()],
                );
            }
            let cache = fixture.repo.root.join("task/cache");
            let permissions = fs::metadata(&cache).unwrap().permissions();
            fs::set_permissions(&cache, fs::Permissions::from_mode(0o000)).unwrap();
            let discovered = roots(&fixture.repo, "task");
            let validated = validate_move(&fixture.repo, "task", "archive/task");
            fs::set_permissions(&cache, permissions).unwrap();
            assert!(discovered.unwrap().is_empty(), "{mechanism}");
            validated.unwrap();
            assert_eq!(
                fs::read(cache.join("private")).unwrap(),
                b"ordinary retained cache\n"
            );
        }
    }

    #[test]
    fn tracked_nested_files_cannot_hide_inside_an_ignored_parent_tree() {
        let fixture = Fixture::new();
        fixture.write("task/cache/nested/file", "tracked nested content\n");
        git(&fixture.repo.root, &["add", "task/cache/nested/file"]);
        fixture.marker("task/cache/nested");
        fixture.write("task/.gitignore", "/cache/\n");
        assert_eq!(roots(&fixture.repo, "task").unwrap(), ["task/cache/nested"]);
        let error = validate(&fixture.repo, "task").unwrap_err().to_string();
        assert!(
            error.contains("is tracked by the outer repository"),
            "{error}"
        );
    }

    #[test]
    fn ignored_parent_tree_cannot_hide_an_outer_gitlink() {
        let fixture = Fixture::new();
        fixture.marker("task/cache/nested");
        fixture.write("task/.gitignore", "/cache/\n");
        let oid = git(&fixture.repo.root, &["rev-parse", "HEAD"]);
        git(
            &fixture.repo.root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{oid},task/cache/nested"),
            ],
        );
        assert!(validate(&fixture.repo, "task").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unrelated_tracked_non_utf8_filename_does_not_block_archive_boundaries() {
        let fixture = Fixture::new();
        // Construct the tracked entry in Git itself: APFS does not allow
        // non-UTF-8 worktree filenames, while a Git index can still carry one.
        let payload = b"retained bytes\n";
        let object = fixture
            .repo
            .run_bytes(["hash-object", "-w", "--stdin"], Some(payload))
            .unwrap();
        let oid = std::str::from_utf8(&object.stdout).unwrap().trim();
        let mut entry = format!("100644 {oid}\t").into_bytes();
        entry.extend_from_slice(b"task/ordinary-\xff-payload\0");
        fixture
            .repo
            .run_bytes(["update-index", "-z", "--index-info"], Some(&entry))
            .unwrap();
        let before = fixture
            .repo
            .run_bytes(["ls-files", "-s", "-z"], None)
            .unwrap();
        assert!(roots(&fixture.repo, "task").unwrap().is_empty());
        validate_move(&fixture.repo, "task", "archive/task").unwrap();
        assert_eq!(
            fixture
                .repo
                .run_bytes(["ls-files", "-s", "-z"], None)
                .unwrap()
                .stdout,
            before.stdout
        );
        assert_eq!(
            fixture
                .repo
                .run_bytes(["cat-file", "blob", oid], None)
                .unwrap()
                .stdout,
            payload
        );
    }

    #[test]
    fn nonempty_git_pointer_is_not_parsed_or_followed() {
        let fixture = Fixture::new();
        fixture.write("task/vendor/.git", "gitdir: /missing/external/admin\n");
        fixture.write("task/.gitignore", "/vendor/\n");
        validate_move(&fixture.repo, "task", "archive/task").unwrap();
        assert_eq!(roots(&fixture.repo, "task").unwrap(), ["task/vendor"]);
        assert_eq!(
            fs::read(fixture.repo.root.join("task/vendor/.git")).unwrap(),
            b"gitdir: /missing/external/admin\n"
        );
    }

    #[test]
    fn recognizable_bare_repository_requires_ignored_root() {
        let fixture = Fixture::new();
        git(&fixture.repo.root, &["init", "--bare", "task/cache.git"]);
        assert_eq!(roots(&fixture.repo, "task").unwrap(), ["task/cache.git"]);
        assert!(validate(&fixture.repo, "task").is_err());
        fixture.write("task/.gitignore", "/cache.git/\n");
        validate_move(&fixture.repo, "task", "archive/task").unwrap();
    }

    #[test]
    fn ignored_external_worktree_moves_without_rewriting_any_git_bytes() {
        let fixture = Fixture::new();
        let external = fixture._temp.path().join("external");
        initialize(&external);
        let checkout = fixture.repo.root.join("task/vendor");
        git(
            &external,
            &["worktree", "add", "--detach", checkout.to_str().unwrap()],
        );
        let admin = std::path::PathBuf::from(git(&checkout, &["rev-parse", "--absolute-git-dir"]));
        let backlink_path = admin.join("gitdir");
        let backlink = fs::read(&backlink_path).unwrap();
        let pointer = fs::read(checkout.join(".git")).unwrap();
        fixture.write("task/.gitignore", "/vendor/\n");
        validate_move(&fixture.repo, "task", "archive/task").unwrap();
        let source = fixture.repo.root.join("task");
        let destination = fixture.repo.root.join("archive/task");
        let plan = crate::relocation::RelocationPlan::opaque(&source, &destination).unwrap();
        assert_eq!(
            serde_json::to_value(&plan).unwrap()["references"],
            serde_json::json!([]),
            "an opaque move must never journal payload rewrites",
        );
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::rename(&source, &destination).unwrap();
        plan.apply().unwrap();
        plan.apply().unwrap();
        assert_eq!(fs::read(destination.join("vendor/.git")).unwrap(), pointer);
        assert_eq!(fs::read(&backlink_path).unwrap(), backlink);
        plan.restore().unwrap();
        fs::rename(&destination, &source).unwrap();
        plan.restore().unwrap();
        assert_eq!(fs::read(source.join("vendor/.git")).unwrap(), pointer);
        assert_eq!(fs::read(&backlink_path).unwrap(), backlink);
    }
}
