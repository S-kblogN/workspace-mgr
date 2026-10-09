use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::process::{ByteOutput, CommandOutput, run_bytes, run_unchecked, run_with, stream};

#[derive(Debug, Clone)]
pub struct GitRepo {
    pub root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TreeEntry {
    pub mode: String,
    pub kind: String,
    pub oid: String,
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

    /// Resolves literal repository paths without putting filenames into the
    /// line-delimited `cat-file` protocol. Each invocation is a fresh snapshot.
    pub fn blob_ids(
        &self,
        revision: &str,
        paths: &[String],
    ) -> Result<BTreeMap<String, Option<String>>> {
        self.file_ids(revision, paths, false, true, true)
    }

    /// Reads exact literal entries, including trees and symlinks, in bounded
    /// batches. NUL framing keeps Git's filename quoting out of comparisons.
    pub(crate) fn tree_entries(
        &self,
        revision: &str,
        paths: &[String],
    ) -> Result<BTreeMap<String, TreeEntry>> {
        let requested = paths.iter().map(String::as_str).collect::<BTreeSet<_>>();
        let literal = paths
            .iter()
            .map(|path| format!(":(literal){path}"))
            .collect::<Vec<_>>();
        let mut found = BTreeMap::new();
        for batch in pathspec_batches(&literal) {
            let mut args = vec!["ls-tree", "-r", "-t", "-z", revision, "--"]
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            args.extend(batch.iter().cloned());
            let output = self.run_bytes(args, None)?;
            for record in output
                .stdout
                .split(|byte| *byte == 0)
                .filter(|r| !r.is_empty())
            {
                let Some(separator) = record.iter().position(|byte| *byte == b'\t') else {
                    return Err(Error::message("unexpected Git tree entry"));
                };
                let Ok(path) = std::str::from_utf8(&record[separator + 1..]) else {
                    continue;
                };
                if !requested.contains(path) {
                    continue;
                }
                let attributes = std::str::from_utf8(&record[..separator])
                    .map_err(|_| Error::message("Git tree attributes are not UTF-8"))?;
                let fields = attributes.split(' ').collect::<Vec<_>>();
                let [mode, kind, oid] = fields.as_slice() else {
                    return Err(Error::message("unexpected Git tree attributes"));
                };
                found.insert(
                    path.to_owned(),
                    TreeEntry {
                        mode: (*mode).to_owned(),
                        kind: (*kind).to_owned(),
                        oid: (*oid).to_owned(),
                    },
                );
            }
        }
        Ok(found)
    }

    /// Hashes files using their own paths for attributes and clean filters.
    /// Literal argv avoids the line-based stdin-paths filename protocol.
    pub(crate) fn filtered_worktree_hashes(
        &self,
        paths: &[String],
    ) -> Result<BTreeMap<String, String>> {
        let mut found = BTreeMap::new();
        for batch in pathspec_batches(paths) {
            let mut args = vec![
                "hash-object".to_owned(),
                "--filters".to_owned(),
                "--".to_owned(),
            ];
            args.extend(batch.iter().cloned());
            let output = self.run_bytes(args, None)?;
            let hashes = std::str::from_utf8(&output.stdout)
                .map_err(|_| Error::message("Git working-tree hashes are not UTF-8"))?
                .lines()
                .collect::<Vec<_>>();
            if hashes.len() != batch.len() {
                return Err(Error::message(
                    "Git working-tree hash batch omitted requested paths",
                ));
            }
            for (path, hash) in batch.iter().zip(hashes) {
                if !matches!(hash.len(), 40 | 64)
                    || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    return Err(Error::message(
                        "Git working-tree hash batch returned an invalid object ID",
                    ));
                }
                found.insert(path.clone(), hash.to_owned());
            }
        }
        Ok(found)
    }

    pub fn existing_paths(&self, revision: &str, paths: &[String]) -> Result<BTreeSet<String>> {
        Ok(self
            .file_ids(revision, paths, false, true, false)?
            .into_iter()
            .filter_map(|(path, oid)| oid.map(|_| path))
            .collect())
    }

    fn file_ids(
        &self,
        revision: &str,
        paths: &[String],
        regular_only: bool,
        verify_existence: bool,
        blobs_only: bool,
    ) -> Result<BTreeMap<String, Option<String>>> {
        let mut found = paths
            .iter()
            .map(|path| (path.clone(), None))
            .collect::<BTreeMap<_, _>>();
        let literal = paths
            .iter()
            .map(|path| format!(":(literal){path}"))
            .collect::<Vec<_>>();
        for batch in pathspec_batches(&literal) {
            let mut args = vec!["ls-tree", "-r", "-t", "-z", revision, "--"]
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            args.extend(batch.iter().cloned());
            let output = self.run_bytes(args, None)?;
            for record in output
                .stdout
                .split(|byte| *byte == 0)
                .filter(|r| !r.is_empty())
            {
                let Some(separator) = record.iter().position(|byte| *byte == b'\t') else {
                    return Err(Error::message("unexpected Git tree entry"));
                };
                let attributes = std::str::from_utf8(&record[..separator])
                    .map_err(|_| Error::message("Git tree attributes are not UTF-8"))?;
                let Ok(path) = std::str::from_utf8(&record[separator + 1..]) else {
                    // A descendant that was not requested can use arbitrary
                    // Git filename bytes without breaking a UTF-8 selection.
                    continue;
                };
                let fields = attributes.split(' ').collect::<Vec<_>>();
                let [mode, kind, oid] = fields.as_slice() else {
                    return Err(Error::message("unexpected Git tree attributes"));
                };
                if (!blobs_only || *kind == "blob")
                    && (!regular_only || matches!(*mode, "100644" | "100755"))
                    && let Some(value) = found.get_mut(path)
                {
                    *value = Some((*oid).to_owned());
                }
            }
        }
        if verify_existence {
            let objects =
                self.object_types(&found.values().filter_map(Clone::clone).collect::<Vec<_>>())?;
            for oid in found.values_mut() {
                if oid.as_ref().is_some_and(|oid| {
                    let kind = objects.get(oid).and_then(Option::as_deref);
                    kind.is_none() || (blobs_only && kind != Some("blob"))
                }) {
                    *oid = None;
                }
            }
        }
        Ok(found)
    }

    /// Checks every unique object with the same existence/type semantics as
    /// individual `cat-file -e`/`-t` calls, including broken tree references.
    pub fn object_types(&self, oids: &[String]) -> Result<BTreeMap<String, Option<String>>> {
        let unique = oids.iter().collect::<BTreeSet<_>>();
        if unique.is_empty() {
            return Ok(BTreeMap::new());
        }
        for oid in &unique {
            if !matches!(oid.len(), 40 | 64) || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(Error::message(format!("invalid Git object ID {oid:?}")));
            }
        }
        let input = unique
            .iter()
            .map(|oid| format!("{oid}\n"))
            .collect::<String>();
        let output = self.run_bytes(
            ["cat-file", "--batch-check=%(objectname) %(objecttype)"],
            Some(input.as_bytes()),
        )?;
        let text = std::str::from_utf8(&output.stdout)
            .map_err(|_| Error::message("Git object types are not UTF-8"))?;
        let records = text.lines().collect::<Vec<_>>();
        if records.len() != unique.len() {
            return Err(Error::message(
                "Git object type batch omitted requested objects",
            ));
        }
        unique
            .into_iter()
            .zip(records)
            .map(|(expected, record)| {
                let (oid, kind) = record
                    .split_once(' ')
                    .ok_or_else(|| Error::message("unexpected Git object type entry"))?;
                if !expected.eq_ignore_ascii_case(oid) {
                    return Err(Error::message(
                        "Git object type batch returned an unexpected object",
                    ));
                }
                let kind = match kind {
                    "blob" | "tree" | "commit" | "tag" => Some(kind.to_owned()),
                    "missing" => None,
                    _ => return Err(Error::message("unexpected Git object type")),
                };
                Ok((expected.clone(), kind))
            })
            .collect()
    }

    /// Reads all requested blobs with one child process, retaining binary bytes.
    /// Only full object IDs enter the batch protocol, so arbitrary filenames do
    /// not affect framing. Missing objects and non-blobs fail the whole read.
    pub fn visit_blobs(
        &self,
        oids: &[String],
        mut visit: impl FnMut(&str, &[u8]) -> Result<()>,
    ) -> Result<()> {
        let mut seen = BTreeSet::new();
        let oids = oids
            .iter()
            .filter(|oid| seen.insert(*oid))
            .collect::<Vec<_>>();
        if oids.is_empty() {
            return Ok(());
        }
        for oid in &oids {
            if !matches!(oid.len(), 40 | 64) || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(Error::message(format!(
                    "invalid Git blob object ID {oid:?}"
                )));
            }
        }
        let input = oids
            .iter()
            .map(|oid| format!("{oid}\n"))
            .collect::<String>();
        let mut decoder = BlobBatchDecoder::new(&oids);
        self.stream(["cat-file", "--batch"], Some(input.as_bytes()), |chunk| {
            decoder.feed(chunk, &mut visit)
        })?;
        decoder.finish()
    }

    pub fn read_blobs(&self, oids: &[String]) -> Result<BTreeMap<String, Vec<u8>>> {
        let mut contents = BTreeMap::new();
        self.visit_blobs(oids, |oid, content| {
            contents.insert(oid.to_owned(), content.to_vec());
            Ok(())
        })?;
        Ok(contents)
    }

    /// Equivalent to individual `git show <revision>:<path>` blob reads, with
    /// bounded path arguments and one batched content read for unique objects.
    pub fn show_files(&self, revision: &str, paths: &[String]) -> Result<BTreeMap<String, String>> {
        self.show_file_blobs(revision, paths, false)
    }

    pub fn show_regular_files(
        &self,
        revision: &str,
        paths: &[String],
    ) -> Result<BTreeMap<String, String>> {
        self.show_file_blobs(revision, paths, true)
    }

    fn show_file_blobs(
        &self,
        revision: &str,
        paths: &[String],
        regular_only: bool,
    ) -> Result<BTreeMap<String, String>> {
        // The content batch below verifies object existence and type itself.
        let ids = self.file_ids(revision, paths, regular_only, false, true)?;
        let oids = ids.values().filter_map(Clone::clone).collect::<Vec<_>>();
        for (path, oid) in &ids {
            if oid.is_none() {
                return Err(Error::message(format!(
                    "Git blob {revision}:{path} is missing"
                )));
            }
        }
        let contents = self.read_blobs(&oids)?;
        ids.into_iter()
            .map(|(path, oid)| {
                let oid = oid.expect("validated blob identity");
                let content = contents.get(&oid).ok_or_else(|| {
                    Error::message(format!("Git blob {oid} was absent from batch output"))
                })?;
                Ok((path, String::from_utf8_lossy(content).into_owned()))
            })
            .collect()
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
            } else if line == target
                && let Some(path) = worktree.take()
            {
                matches.push(path);
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

/// Bounds the argument payload on all supported platforms. Input protocols are
/// used for content and index edits; `ls-tree` accepts pathspec arguments only.
const PATHSPEC_ARGUMENT_BYTES: usize = 96 * 1024;

pub(crate) fn pathspec_batches(paths: &[String]) -> Vec<&[String]> {
    let mut batches = Vec::new();
    let mut start = 0;
    let mut bytes = 0;
    for (position, path) in paths.iter().enumerate() {
        let cost = path.len() + 1;
        if position > start && bytes + cost > PATHSPEC_ARGUMENT_BYTES {
            batches.push(&paths[start..position]);
            start = position;
            bytes = 0;
        }
        bytes += cost;
    }
    if start < paths.len() {
        batches.push(&paths[start..]);
    }
    batches
}

struct BlobBatchDecoder {
    expected: Vec<String>,
    position: usize,
    buffer: Vec<u8>,
    pending: Option<(String, usize)>,
}

impl BlobBatchDecoder {
    fn new(oids: &[&String]) -> Self {
        Self {
            expected: oids.iter().map(|oid| (*oid).clone()).collect(),
            position: 0,
            buffer: Vec::new(),
            pending: None,
        }
    }

    fn feed(
        &mut self,
        chunk: &[u8],
        visit: &mut impl FnMut(&str, &[u8]) -> Result<()>,
    ) -> Result<()> {
        self.buffer.extend_from_slice(chunk);
        loop {
            if self.pending.is_none() {
                let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') else {
                    if self.buffer.len() > 256 {
                        return Err(Error::message("Git blob batch header is too long"));
                    }
                    return Ok(());
                };
                let header = std::str::from_utf8(&self.buffer[..end])
                    .map_err(|_| Error::message("Git blob batch header is not UTF-8"))?;
                let fields = header.split(' ').collect::<Vec<_>>();
                let (oid, size) = match fields.as_slice() {
                    [oid, "blob", size] => {
                        let size = size.parse::<usize>().map_err(|_| {
                            Error::message(format!("unexpected Git blob header {header:?}"))
                        })?;
                        ((*oid).to_owned(), size)
                    }
                    [oid, "missing"] => {
                        return Err(Error::message(format!(
                            "Git object {oid} is missing locally"
                        )));
                    }
                    _ => {
                        return Err(Error::message(format!(
                            "unexpected Git blob header {header:?}"
                        )));
                    }
                };
                if !self
                    .expected
                    .get(self.position)
                    .is_some_and(|expected| expected.eq_ignore_ascii_case(&oid))
                {
                    return Err(Error::message(
                        "Git blob batch returned an unexpected object",
                    ));
                }
                self.buffer.drain(..=end);
                self.pending = Some((oid, size));
            }
            let (oid, size) = self.pending.as_ref().expect("decoded header");
            let end = size
                .checked_add(1)
                .ok_or_else(|| Error::message("Git blob is too large to inspect"))?;
            if self.buffer.len() < end {
                return Ok(());
            }
            if self.buffer[*size] != b'\n' {
                return Err(Error::message(
                    "Git blob batch has an invalid content terminator",
                ));
            }
            visit(oid, &self.buffer[..*size])?;
            self.buffer.drain(..end);
            self.pending = None;
            self.position += 1;
        }
    }

    fn finish(self) -> Result<()> {
        if self.pending.is_some() || !self.buffer.is_empty() {
            return Err(Error::message("Git blob stream ended mid-record"));
        }
        if self.position != self.expected.len() {
            return Err(Error::message("Git blob stream omitted requested objects"));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, GitRepo) {
        let directory = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: directory.path().to_path_buf(),
        };
        repo.run(["init", "-q", "-b", "main"]).unwrap();
        repo.run(["config", "user.name", "Batch fixture"]).unwrap();
        repo.run(["config", "user.email", "fixture@example.invalid"])
            .unwrap();
        (directory, repo)
    }

    fn commit(repo: &GitRepo) {
        repo.run(["add", "-A"]).unwrap();
        repo.run(["commit", "-qm", "fixture"]).unwrap();
    }

    #[test]
    fn batch_reads_keep_literal_unusual_paths_and_binary_blob_bytes() {
        let (_directory, repo) = fixture();
        let names = [
            "line\nbreak",
            "tab\tfile",
            "literal[?]*",
            "colon:file",
            "back\\slash",
            "empty",
        ];
        for name in names {
            fs::write(
                repo.root.join(name),
                if name == "empty" {
                    &b""[..]
                } else {
                    name.as_bytes()
                },
            )
            .unwrap();
        }
        fs::create_dir(repo.root.join("directory")).unwrap();
        fs::write(repo.root.join("directory/binary"), [0, 255, b'\n', 1]).unwrap();
        commit(&repo);
        let mut paths = names
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>();
        paths.push("directory/binary".to_owned());
        paths.push("literal[?]*".to_owned());
        let ids = repo.blob_ids("HEAD", &paths).unwrap();
        assert_eq!(ids.len(), 7);
        let contents = repo
            .read_blobs(&ids.values().filter_map(Clone::clone).collect::<Vec<_>>())
            .unwrap();
        assert_eq!(
            contents[ids["directory/binary"].as_ref().unwrap()],
            [0, 255, b'\n', 1]
        );
        let text = repo.show_files("HEAD", &paths).unwrap();
        for name in names {
            assert_eq!(text[name], if name == "empty" { "" } else { name });
        }
        assert!(
            repo.show_files("HEAD", &["absent".to_owned()])
                .unwrap_err()
                .to_string()
                .contains("missing")
        );
        assert!(repo.show_files("HEAD", &["directory".to_owned()]).is_err());
        assert!(
            repo.show_files("does-not-exist", &["empty".to_owned()])
                .is_err()
        );
    }

    #[test]
    fn batched_path_reads_are_fresh_after_a_new_commit() {
        let (_directory, repo) = fixture();
        fs::write(repo.root.join("file"), "before").unwrap();
        commit(&repo);
        let old = repo.optional_oid("HEAD").unwrap().unwrap();
        let paths = ["file".to_owned()];
        assert_eq!(repo.show_files("HEAD", &paths).unwrap()["file"], "before");
        fs::write(repo.root.join("file"), "after").unwrap();
        commit(&repo);
        assert_eq!(repo.show_files("HEAD", &paths).unwrap()["file"], "after");
        assert_eq!(repo.show_files(&old, &paths).unwrap()["file"], "before");
    }

    #[cfg(unix)]
    #[test]
    fn regular_file_batch_rejects_a_symlink_blob() {
        let (_directory, repo) = fixture();
        fs::write(repo.root.join("regular"), "content").unwrap();
        std::os::unix::fs::symlink("regular", repo.root.join("link")).unwrap();
        commit(&repo);
        let paths = ["regular".to_owned(), "link".to_owned()];
        assert_eq!(repo.show_files("HEAD", &paths).unwrap()["link"], "regular");
        assert!(repo.show_regular_files("HEAD", &paths).is_err());
    }

    #[test]
    fn missing_non_blob_and_invalid_batch_objects_are_rejected() {
        let (_directory, repo) = fixture();
        fs::write(repo.root.join("file"), "content").unwrap();
        commit(&repo);
        let commit = repo.optional_oid("HEAD").unwrap().unwrap();
        assert!(
            repo.read_blobs(&[commit])
                .unwrap_err()
                .to_string()
                .contains("unexpected Git blob header")
        );
        assert!(
            repo.read_blobs(&["0".repeat(40)])
                .unwrap_err()
                .to_string()
                .contains("missing locally")
        );
        assert!(
            repo.read_blobs(&["HEAD\nHEAD".to_owned()])
                .unwrap_err()
                .to_string()
                .contains("invalid Git blob object ID")
        );
    }

    #[test]
    fn path_batches_preserve_missing_object_checks_from_cat_file() {
        let (_directory, repo) = fixture();
        fs::create_dir(repo.root.join("directory")).unwrap();
        fs::write(
            repo.root.join("directory/file"),
            "unique broken fixture blob",
        )
        .unwrap();
        commit(&repo);
        let paths = ["directory/file".to_owned(), "directory".to_owned()];
        let id = repo.blob_ids("HEAD", &paths).unwrap()["directory/file"]
            .clone()
            .unwrap();
        let objects = repo.git_dir().unwrap().join("objects");
        fs::remove_file(objects.join(&id[..2]).join(&id[2..])).unwrap();
        assert_eq!(
            repo.blob_ids("HEAD", &paths).unwrap()["directory/file"],
            None
        );
        assert_eq!(
            repo.existing_paths("HEAD", &paths).unwrap(),
            BTreeSet::from(["directory".to_owned()])
        );
        assert!(
            repo.show_files("HEAD", &["directory/file".to_owned()])
                .unwrap_err()
                .to_string()
                .contains("missing locally")
        );
    }

    #[test]
    fn blob_batch_decoder_rejects_malformed_truncated_or_extra_records() {
        let oid = "a".repeat(40);
        for raw in [
            format!("{oid} tree 0\n\n"),
            format!("{oid} missing\n"),
            format!("{} blob 0\n\n", "b".repeat(40)),
            format!("{oid} blob nope\n"),
            format!("{oid} blob 2\nab!"),
            format!("{oid} blob 2\na"),
            format!("{oid} blob 0\n\n{oid} blob 0\n\n"),
            String::new(),
        ] {
            let mut decoder = BlobBatchDecoder::new(&[&oid]);
            let result = decoder
                .feed(raw.as_bytes(), &mut |_, _| Ok(()))
                .and_then(|()| decoder.finish());
            assert!(result.is_err(), "accepted malformed reply {raw:?}");
        }
        let mut decoder = BlobBatchDecoder::new(&[&oid]);
        let raw = format!("{oid} blob 4\n");
        let mut seen = Vec::new();
        for chunk in [raw.as_bytes(), &[0, b'\n', 255], &[1, b'\n']] {
            decoder
                .feed(chunk, &mut |_, content| {
                    seen.push(content.to_vec());
                    Ok(())
                })
                .unwrap();
        }
        decoder.finish().unwrap();
        assert_eq!(seen, [vec![0, b'\n', 255, 1]]);
    }

    #[test]
    fn batch_read_stream_handles_many_objects_and_large_binary_content() {
        let (_directory, repo) = fixture();
        let large = (0..2 * 1024 * 1024)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let mut paths = Vec::new();
        for index in 0..1_000 {
            let content = format!("small blob {index}");
            let path = format!("small-{index}");
            fs::write(repo.root.join(&path), content).unwrap();
            paths.push(path);
        }
        fs::write(repo.root.join("large"), &large).unwrap();
        paths.push("large".to_owned());
        commit(&repo);
        let ids = repo.blob_ids("HEAD", &paths).unwrap();
        let mut objects = ids.values().filter_map(Clone::clone).collect::<Vec<_>>();
        let large_id = ids["large"].clone().unwrap();
        objects.push(large_id.clone());
        let mut visited = 0;
        repo.visit_blobs(&objects, |oid, content| {
            visited += 1;
            if oid == large_id {
                assert_eq!(content, large);
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(visited, 1_001);
    }
}
