use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::process::{ByteOutput, CommandOutput, run_bytes, run_unchecked, run_with, stream};

#[derive(Debug, Clone)]
pub struct GitRepo {
    pub root: PathBuf,
}

impl GitRepo {
    pub fn discover(path: &Path) -> Result<Self> {
        let candidate = path.canonicalize().map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let output = run_unchecked(
            "git",
            [
                "-C",
                &candidate.to_string_lossy(),
                "rev-parse",
                "--show-toplevel",
            ],
            &candidate,
        )?;
        if !output.success() {
            return Err(Error::message(format!(
                "not inside a Git repository: {}",
                candidate.display()
            )));
        }
        Ok(Self {
            root: PathBuf::from(output.stdout.trim()),
        })
    }

    pub fn discover_for_manifest(path: &Path) -> Result<Self> {
        let absolute = crate::local_state::absolute_path(path)?;
        let parent = absolute
            .parent()
            .ok_or_else(|| Error::message("manifest path has no parent"))?;
        let infrastructure = absolute.file_name().and_then(|name| name.to_str())
            == Some(crate::manifest::INFRASTRUCTURE_TASK_MANIFEST_FILE)
            || (absolute.file_name().and_then(|name| name.to_str()) == Some("task.toml")
                && parent.file_name().and_then(|name| name.to_str()) == Some("workspace-mgr"));
        let repo = match Self::discover(parent) {
            Ok(repo) => repo,
            Err(error) if infrastructure => {
                let git_directory =
                    if absolute.file_name().and_then(|name| name.to_str()) == Some("task.toml") {
                        parent.parent()
                    } else {
                        parent
                            .parent()
                            .filter(|directory| {
                                directory.file_name().and_then(|name| name.to_str())
                                    == Some("infrastructure-tasks")
                            })
                            .and_then(Path::parent)
                            .filter(|directory| {
                                directory.file_name().and_then(|name| name.to_str())
                                    == Some("workspace-mgr")
                            })
                            .and_then(Path::parent)
                    }
                    .ok_or_else(|| {
                        Error::message(format!(
                            "infrastructure manifest is outside private task state: {error}"
                        ))
                    })?;
                let output = run_bytes(
                    "git",
                    [
                        "--git-dir",
                        &git_directory.to_string_lossy(),
                        "worktree",
                        "list",
                        "--porcelain",
                        "-z",
                    ],
                    git_directory,
                    &BTreeMap::new(),
                    None,
                    true,
                )?;
                let listing = String::from_utf8(output.stdout)
                    .map_err(|_| Error::message("primary checkout path is not UTF-8"))?;
                let root = listing
                    .split('\0')
                    .find_map(|field| field.strip_prefix("worktree "))
                    .ok_or_else(|| {
                        Error::message("infrastructure manifest requires a primary checkout")
                    })?;
                match Self::discover(Path::new(root)) {
                    Ok(repo) => repo,
                    Err(_) => {
                        let configured = run_unchecked(
                            "git",
                            [
                                "--git-dir",
                                &git_directory.to_string_lossy(),
                                "config",
                                "--path",
                                "--get",
                                "core.worktree",
                            ],
                            git_directory,
                        )?;
                        if !configured.success() || configured.stdout.trim().is_empty() {
                            return Err(Error::message(
                                "workspace-mgr cannot locate the primary checkout of a separate Git directory; configure Git core.worktree with the absolute primary checkout path",
                            ));
                        }
                        let configured = PathBuf::from(configured.stdout.trim());
                        let root = if configured.is_absolute() {
                            configured
                        } else {
                            git_directory.join(configured)
                        };
                        Self::discover(&root)?
                    }
                }
            }
            Err(error) => return Err(error),
        };
        let repo = if infrastructure {
            repo.infrastructure_checkout()?
        } else {
            repo
        };
        repo.resolve_manifest_path(&absolute)?;
        Ok(repo)
    }

    /// Select the unique usable configured base checkout for infrastructure
    /// operations, even when the private manifest lives in the primary checkout.
    fn infrastructure_checkout(&self) -> Result<Self> {
        let common = self
            .common_dir()?
            .canonicalize()
            .map_err(|source| Error::Io {
                path: self.root.clone(),
                source,
            })?;
        let output = self.run_bytes(["worktree", "list", "--porcelain", "-z"], None)?;
        let listing = String::from_utf8(output.stdout)
            .map_err(|_| Error::message("shared checkout paths are not UTF-8"))?;
        let mut shared = Vec::new();
        let mut unavailable = Vec::new();
        for block in listing.split("\0\0") {
            let fields = block.split('\0').collect::<Vec<_>>();
            let Some(checkout) = fields
                .iter()
                .find_map(|field| field.strip_prefix("worktree "))
            else {
                continue;
            };
            let Some(branch) = fields
                .iter()
                .find_map(|field| field.strip_prefix("branch refs/heads/"))
            else {
                continue;
            };
            let checkout = if Path::new(checkout).canonicalize().ok().as_ref() == Some(&common) {
                crate::local_state::directory_unmigrated(self)?
                    .parent()
                    .and_then(Path::parent)
                    .ok_or_else(|| Error::message("local state has no primary checkout"))?
                    .to_path_buf()
            } else {
                PathBuf::from(checkout)
            };
            let repo = match Self::discover(&checkout) {
                Ok(repo) => repo,
                Err(error) => {
                    unavailable.push(error.to_string());
                    continue;
                }
            };
            let config = match crate::config::Config::load_compatible(&repo) {
                Ok(config) => config,
                Err(error) => {
                    unavailable.push(error.to_string());
                    continue;
                }
            };
            let actual_common = match repo.common_dir().and_then(|directory| {
                directory.canonicalize().map_err(|source| Error::Io {
                    path: directory,
                    source,
                })
            }) {
                Ok(directory) => directory,
                Err(error) => {
                    unavailable.push(error.to_string());
                    continue;
                }
            };
            if branch == config.git.branch && actual_common == common {
                shared.push(repo);
            }
        }
        if shared.len() != 1 {
            let details = if shared.is_empty() && !unavailable.is_empty() {
                format!("; unavailable checkouts: {}", unavailable.join("; "))
            } else {
                String::new()
            };
            return Err(Error::message(format!(
                "infrastructure manifest requires exactly one valid shared checkout on the configured base branch{details}"
            )));
        }
        Ok(shared.remove(0))
    }

    pub fn local_state_dir(&self) -> Result<PathBuf> {
        crate::local_state::directory(self)
    }

    pub fn resolve_manifest_path(&self, path: &Path) -> Result<PathBuf> {
        let absolute = crate::local_state::absolute_path(path)?;
        let private_manifest = absolute.file_name().and_then(|name| name.to_str())
            == Some(crate::manifest::INFRASTRUCTURE_TASK_MANIFEST_FILE)
            || (absolute.file_name().and_then(|name| name.to_str()) == Some("task.toml")
                && absolute
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|name| name.to_str())
                    == Some("workspace-mgr"));
        if !private_manifest {
            return absolute.canonicalize().map_err(|source| Error::Io {
                path: absolute,
                source,
            });
        }
        let common = self
            .common_dir()?
            .canonicalize()
            .map_err(|source| Error::Io {
                path: self.root.clone(),
                source,
            })?;
        let old_root = common.join("workspace-mgr");
        let legacy_task = absolute.file_name().and_then(|name| name.to_str()) == Some("task.toml")
            && absolute
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                == Some("workspace-mgr")
            && absolute.starts_with(&common);
        let resolved = if legacy_task {
            let local = self.local_state_dir()?;
            crate::local_state::legacy_manifest_target(&local, &common, &absolute)?
        } else if let Ok(relative) = absolute.strip_prefix(&old_root) {
            self.local_state_dir()?.join(relative)
        } else {
            absolute
        };
        resolved.canonicalize().map_err(|source| Error::Io {
            path: resolved,
            source,
        })
    }

    pub fn run<I, S>(&self, args: I) -> Result<CommandOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut full = vec!["-C".to_owned(), self.root.to_string_lossy().into_owned()];
        full.extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        run_with("git", full, &self.root, &BTreeMap::new(), None, true)
    }

    pub fn run_unchecked<I, S>(&self, args: I) -> Result<CommandOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut full = vec!["-C".to_owned(), self.root.to_string_lossy().into_owned()];
        full.extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        run_with("git", full, &self.root, &BTreeMap::new(), None, false)
    }

    pub fn run_bytes<I, S>(&self, args: I, input: Option<&[u8]>) -> Result<ByteOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut full = vec!["-C".to_owned(), self.root.to_string_lossy().into_owned()];
        full.extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        run_bytes("git", full, &self.root, &BTreeMap::new(), input, true)
    }

    pub fn stream<I, S, F>(&self, args: I, input: Option<&[u8]>, on_stdout: F) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
        F: FnMut(&[u8]) -> Result<()>,
    {
        let mut full = vec!["-C".to_owned(), self.root.to_string_lossy().into_owned()];
        full.extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        stream(
            "git",
            full,
            &self.root,
            &BTreeMap::new(),
            input,
            true,
            on_stdout,
        )?;
        Ok(())
    }

    pub fn run_with_index<I, S>(
        &self,
        index: &Path,
        args: I,
        input: Option<&str>,
        check: bool,
    ) -> Result<CommandOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut full = vec!["-C".to_owned(), self.root.to_string_lossy().into_owned()];
        full.extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        let env = BTreeMap::from([(
            "GIT_INDEX_FILE".to_owned(),
            index.to_string_lossy().into_owned(),
        )]);
        run_with("git", full, &self.root, &env, input, check)
    }

    pub fn visible_paths(&self, scopes: &[String]) -> Result<Vec<String>> {
        let mut args = vec![
            "ls-files".to_owned(),
            "--cached".to_owned(),
            "--others".to_owned(),
            "--exclude-standard".to_owned(),
            "-z".to_owned(),
            "--".to_owned(),
        ];
        if scopes.is_empty() {
            args.push(".".to_owned());
        } else {
            args.extend(scopes.iter().cloned());
        }
        let mut paths = Vec::new();
        for path in self
            .run(args)?
            .stdout
            .split('\0')
            .filter(|path| !path.is_empty())
        {
            // Private state stays outside content discovery even before an
            // upgraded checkout regenerates the root ignore rules.
            if Path::new(path).starts_with(crate::local_state::LOCAL_STATE_PATH) {
                continue;
            }
            match fs::symlink_metadata(self.root.join(path)) {
                Ok(_) => paths.push(path.to_owned()),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) => {}
                Err(source) => {
                    return Err(Error::Io {
                        path: self.root.join(path),
                        source,
                    });
                }
            }
        }
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    pub fn common_dir(&self) -> Result<PathBuf> {
        let raw = self.run(["rev-parse", "--git-common-dir"])?.stdout;
        let path = PathBuf::from(raw.trim());
        if path.is_absolute() {
            Ok(path)
        } else {
            Ok(self.root.join(path))
        }
    }

    pub fn git_dir(&self) -> Result<PathBuf> {
        let raw = self.run(["rev-parse", "--git-dir"])?.stdout;
        let path = PathBuf::from(raw.trim());
        if path.is_absolute() {
            Ok(path)
        } else {
            Ok(self.root.join(path))
        }
    }

    pub fn branch_worktrees(&self, branch: &str) -> Result<Vec<PathBuf>> {
        let target = format!("branch refs/heads/{branch}");
        let mut worktree = None;
        let mut matches = Vec::new();
        for line in self
            .run(["worktree", "list", "--porcelain"])?
            .stdout
            .lines()
        {
            if let Some(path) = line.strip_prefix("worktree ") {
                worktree = Some(PathBuf::from(path));
            } else if line == target {
                if let Some(path) = worktree.take() {
                    matches.push(path);
                }
            }
        }
        Ok(matches)
    }

    pub fn current_branch(&self) -> Result<Option<String>> {
        let output = self.run_unchecked(["symbolic-ref", "--quiet", "--short", "HEAD"])?;
        match output.code {
            0 => Ok(Some(output.stdout.trim().to_owned())),
            1 => Ok(None),
            _ => Err(Error::message(command_detail(
                &output.stderr,
                "failed to read current branch",
            ))),
        }
    }

    pub fn optional_oid(&self, reference: &str) -> Result<Option<String>> {
        let output = self.run_unchecked(["rev-parse", "--verify", "--quiet", reference])?;
        match output.code {
            0 => Ok(Some(output.stdout.trim().to_owned())),
            1 => Ok(None),
            _ => Err(Error::message(command_detail(
                &output.stderr,
                &format!("failed to resolve {reference}"),
            ))),
        }
    }

    pub fn fetch_branch(&self, remote: &str, branch: &str) -> Result<String> {
        let remote_ref = format!("refs/remotes/{remote}/{branch}");
        let refspec = format!("+refs/heads/{branch}:{remote_ref}");
        self.run([
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            remote,
            &refspec,
        ])?;
        self.optional_oid(&remote_ref)?
            .ok_or_else(|| Error::message(format!("fetch did not create {remote_ref}")))
    }

    /// Makes the commit `oid` that `remote`'s `branch` pointed to available
    /// locally without updating any ref, fetching the branch only when the
    /// object is missing.
    pub fn fetch_branch_objects(&self, remote: &str, branch: &str, oid: &str) -> Result<()> {
        let commit = format!("{oid}^{{commit}}");
        if self.run_unchecked(["cat-file", "-e", &commit])?.success() {
            return Ok(());
        }
        self.run([
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--refmap=",
            remote,
            &format!("refs/heads/{branch}"),
        ])?;
        if self.run_unchecked(["cat-file", "-e", &commit])?.success() {
            return Ok(());
        }
        Err(Error::message(format!(
            "{remote}/{branch} changed while it was being inspected; retry"
        )))
    }

    pub fn remote_branch_oid(&self, remote: &str, branch: &str) -> Result<Option<String>> {
        let reference = format!("refs/heads/{branch}");
        let output = self.run(["ls-remote", "--heads", remote, &reference])?;
        let lines: Vec<&str> = output
            .stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        if lines.is_empty() {
            return Ok(None);
        }
        if lines.len() != 1 {
            return Err(Error::message(format!(
                "remote returned multiple matches for {reference}"
            )));
        }
        let mut parts = lines[0].split_whitespace();
        let oid = parts.next().unwrap_or_default();
        let actual = parts.next().unwrap_or_default();
        if actual != reference {
            return Err(Error::message(format!("unexpected remote ref {actual:?}")));
        }
        Ok(Some(oid.to_owned()))
    }

    pub fn ensure_branch_not_checked_out(&self, branch: &str) -> Result<()> {
        if !self.branch_worktrees(branch)?.is_empty() {
            return Err(Error::message(format!(
                "target branch {branch:?} is checked out in a worktree"
            )));
        }
        Ok(())
    }

    pub fn validate_branch(&self, branch: &str) -> Result<()> {
        self.run(["check-ref-format", &format!("refs/heads/{branch}")])?;
        Ok(())
    }

    pub fn validate_remote_name(&self, remote: &str) -> Result<()> {
        if remote.starts_with('-') || remote.chars().any(char::is_whitespace) {
            return Err(Error::message(format!(
                "unsafe Git remote name {remote:?}; configure a named remote"
            )));
        }
        let probe = format!("refs/remotes/{remote}/workspace-mgr-probe");
        let checked = self.run_unchecked(["check-ref-format", &probe])?;
        if !checked.success() {
            return Err(Error::message(format!(
                "invalid Git remote name {remote:?}; configure a named remote"
            )));
        }
        Ok(())
    }
}

fn command_detail(stderr: &str, fallback: &str) -> String {
    let detail = stderr.trim();
    if detail.is_empty() {
        fallback.to_owned()
    } else {
        detail.to_owned()
    }
}
