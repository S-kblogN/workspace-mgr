use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::process::{CommandOutput, run_unchecked, run_with};

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
        let parent = path
            .parent()
            .ok_or_else(|| Error::message("manifest path has no parent"))?;
        if let Ok(repo) = Self::discover(parent) {
            return Ok(repo);
        }
        let worktree_git_dir = parent
            .parent()
            .ok_or_else(|| Error::message("private manifest has no worktree Git directory"))?;
        let pointer = worktree_git_dir.join("gitdir");
        let git_file = fs::read_to_string(&pointer).map_err(|source| crate::error::Error::Io {
            path: pointer.clone(),
            source,
        })?;
        let checkout = PathBuf::from(git_file.trim())
            .parent()
            .ok_or_else(|| Error::message("worktree Git pointer has no checkout parent"))?
            .to_path_buf();
        Self::discover(&checkout)
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
