//! Canonical, isolated Git commits carry archive control JSON on Git hosts.
//!
//! A bare blob remains readable for older claims. New claims use a parentless
//! commit with exactly one regular control file, without the task tree/index.
use std::collections::BTreeMap;
use std::path::Path;

use crate::error::{Error, Result};
use crate::process::run_bytes;

pub(crate) const FILE_NAME: &str = "workspace-mgr-control.json";
const IDENTITY: &str = "workspace-mgr <archive-control@workspace-mgr.invalid> 0 +0000";
const MESSAGE: &str = "workspace-mgr archive control envelope\n";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ControlOids {
    pub legacy_blob: String,
    pub commit: String,
}

fn valid_oid(oid: &str) -> bool {
    [40, 64].contains(&oid.len()) && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn hash(repo_path: &Path, kind: &str, bytes: &[u8], write: bool) -> Result<String> {
    let mut args = vec![
        "--no-replace-objects",
        "hash-object",
        "--no-filters",
        "-t",
        kind,
    ];
    if write {
        args.push("-w");
    }
    args.push("--stdin");
    let output = run_bytes("git", args, repo_path, &BTreeMap::new(), Some(bytes), true)?;
    let oid = String::from_utf8(output.stdout)
        .map_err(|_| Error::message("archive Git control hash is not UTF-8"))?
        .trim()
        .to_owned();
    if !valid_oid(&oid) {
        return Err(Error::message(
            "invalid archive Git control object identity",
        ));
    }
    Ok(oid)
}

fn raw_oid(oid: &str) -> Result<Vec<u8>> {
    if !valid_oid(oid) {
        return Err(Error::message(
            "invalid archive Git control object identity",
        ));
    }
    oid.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let text = std::str::from_utf8(pair)
                .map_err(|_| Error::message("invalid archive Git control object identity"))?;
            u8::from_str_radix(text, 16)
                .map_err(|_| Error::message("invalid archive Git control object identity"))
        })
        .collect()
}

/// Pure identity calculation when `write` is false; no index, filters or refs.
pub(crate) fn object_ids(repo_path: &Path, body: &str, write: bool) -> Result<ControlOids> {
    let legacy_blob = hash(repo_path, "blob", body.as_bytes(), write)?;
    let mut tree = format!("100644 {FILE_NAME}\0").into_bytes();
    tree.extend(raw_oid(&legacy_blob)?);
    let tree_oid = hash(repo_path, "tree", &tree, write)?;
    let commit = format!("tree {tree_oid}\nauthor {IDENTITY}\ncommitter {IDENTITY}\n\n{MESSAGE}");
    Ok(ControlOids {
        legacy_blob,
        commit: hash(repo_path, "commit", commit.as_bytes(), write)?,
    })
}

fn object(repo_path: &Path, kind: &str, oid: &str) -> Result<Vec<u8>> {
    Ok(run_bytes(
        "git",
        ["--no-replace-objects", "cat-file", kind, oid],
        repo_path,
        &BTreeMap::new(),
        None,
        true,
    )?
    .stdout)
}

/// Read old blob claims or the exact canonical envelope, never a task commit.
pub(crate) fn read_body(repo_path: &Path, oid: &str) -> Result<String> {
    if !valid_oid(oid) {
        return Err(Error::message(
            "invalid archive Git control object identity",
        ));
    }
    let kind = object(repo_path, "-t", oid)?;
    let body = match kind.as_slice() {
        b"blob\n" => object(repo_path, "blob", oid)?,
        b"commit\n" => {
            let commit = object(repo_path, "commit", oid)?;
            let first = commit
                .split(|byte| *byte == b'\n')
                .next()
                .unwrap_or_default();
            let tree_oid = std::str::from_utf8(first)
                .ok()
                .and_then(|header| header.strip_prefix("tree "))
                .filter(|tree| valid_oid(tree))
                .ok_or_else(|| Error::message("archive Git control has no canonical tree"))?;
            let tree = object(repo_path, "tree", tree_oid)?;
            let prefix = format!("100644 {FILE_NAME}\0");
            let blob = tree
                .strip_prefix(prefix.as_bytes())
                .filter(|bytes| bytes.len() == oid.len() / 2)
                .ok_or_else(|| {
                    Error::message(
                        "archive Git control must contain only its canonical regular file",
                    )
                })?;
            let blob_oid = crate::hex::encode_lower(blob);
            let body = object(repo_path, "blob", &blob_oid)?;
            let text = std::str::from_utf8(&body)
                .map_err(|_| Error::message("archive Git control body is not UTF-8"))?;
            if object_ids(repo_path, text, false)?.commit != oid {
                return Err(Error::message(
                    "archive Git control commit metadata is not canonical",
                ));
            }
            body
        }
        _ => {
            return Err(Error::message(
                "archive Git control must be a blob or canonical envelope commit",
            ));
        }
    };
    String::from_utf8(body).map_err(|_| Error::message("archive Git control body is not UTF-8"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::process::{run, run_with};
    use std::fs;

    #[cfg(unix)]
    pub(crate) fn install_commit_only_hook(remote: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let hook = remote.join("hooks/pre-receive");
        fs::write(&hook, br##"#!/bin/sh
while read old new ref; do
    case "$ref" in
        refs/tags/workspace-mgr/archive-copy/*|refs/tags/workspace-mgr/archive-registry/*)
            case "$new" in 0000000000000000000000000000000000000000|0000000000000000000000000000000000000000000000000000000000000000) continue;; esac
            test "$(git cat-file -t "$new")" = commit || { echo 'archive control claim requires a commit' >&2; exit 1; };;
    esac
done
"##).unwrap();
        fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn repository(format: &str) -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        run(
            "git",
            ["init", "-q", &format!("--object-format={format}")],
            temp.path(),
        )
        .unwrap();
        temp
    }

    #[test]
    fn deterministic_envelopes_preserve_large_raw_bodies_and_both_object_formats() {
        for format in ["sha1", "sha256"] {
            let repo = repository(format);
            let body = format!("{{\"receipt\":\"{}\"}}", "x".repeat(135_895));
            let before = run("git", ["count-objects", "-v"], repo.path())
                .unwrap()
                .stdout;
            let predicted = object_ids(repo.path(), &body, false).unwrap();
            assert_eq!(
                run("git", ["count-objects", "-v"], repo.path())
                    .unwrap()
                    .stdout,
                before
            );
            assert!(run("git", ["cat-file", "-e", &predicted.commit], repo.path()).is_err());
            let written = object_ids(repo.path(), &body, true).unwrap();
            assert_eq!(written, predicted);
            assert_eq!(object_ids(repo.path(), &body, true).unwrap(), written);
            assert_eq!(read_body(repo.path(), &written.commit).unwrap(), body);
            assert_eq!(read_body(repo.path(), &written.legacy_blob).unwrap(), body);
            assert_eq!(
                run("git", ["rev-list", "--count", &written.commit], repo.path())
                    .unwrap()
                    .stdout
                    .trim(),
                "1"
            );
            assert_eq!(
                run("git", ["ls-tree", &written.commit], repo.path())
                    .unwrap()
                    .stdout,
                format!("100644 blob {}\t{FILE_NAME}\n", written.legacy_blob)
            );
        }
    }

    fn write_commit(repo: &Path, raw: &[u8]) -> String {
        hash(repo, "commit", raw, true).unwrap()
    }

    #[test]
    fn noncanonical_metadata_parents_modes_paths_and_extra_files_are_refused() {
        let repo = repository("sha1");
        let ids = object_ids(repo.path(), "{}", true).unwrap();
        let raw = object(repo.path(), "commit", &ids.commit).unwrap();
        let tree_line = std::str::from_utf8(&raw).unwrap().lines().next().unwrap();
        for forged in [
            String::from_utf8(raw.clone())
                .unwrap()
                .replace(MESSAGE, "foreign metadata\n"),
            String::from_utf8(raw.clone()).unwrap().replacen(
                '\n',
                &format!("\nparent {}\n", ids.commit),
                1,
            ),
            String::from_utf8(raw.clone())
                .unwrap()
                .replace(IDENTITY, "foreign <foreign@example.invalid> 1 +0000"),
        ] {
            let oid = write_commit(repo.path(), forged.as_bytes());
            assert!(read_body(repo.path(), &oid).is_err());
        }
        for entries in [
            vec![("100755", FILE_NAME)],
            vec![("120000", FILE_NAME)],
            vec![("100644", "another.json")],
            vec![("100644", FILE_NAME), ("100644", "z-extra.json")],
        ] {
            let mut tree = Vec::new();
            for (mode, name) in entries {
                tree.extend(format!("{mode} {name}\0").as_bytes());
                tree.extend(raw_oid(&ids.legacy_blob).unwrap());
            }
            let tree_oid = hash(repo.path(), "tree", &tree, true).unwrap();
            let forged = String::from_utf8(raw.clone())
                .unwrap()
                .replace(tree_line, &format!("tree {tree_oid}"));
            assert!(read_body(repo.path(), &write_commit(repo.path(), forged.as_bytes())).is_err());
        }
    }

    #[test]
    fn envelope_ignores_task_index_configuration_and_filters() {
        let repo = repository("sha1");
        let body = "{\"opaque\":true}";
        let before = object_ids(repo.path(), body, false).unwrap();
        fs::write(repo.path().join(".gitattributes"), "* filter=hostile\n").unwrap();
        run(
            "git",
            ["config", "filter.hostile.clean", "exit 97"],
            repo.path(),
        )
        .unwrap();
        run(
            "git",
            ["config", "user.name", "Unrelated Task"],
            repo.path(),
        )
        .unwrap();
        run_with(
            "git",
            [
                "update-index",
                "--add",
                "--cacheinfo",
                &format!(
                    "100644,{},task.txt",
                    hash(repo.path(), "blob", b"unrelated", true).unwrap()
                ),
            ],
            repo.path(),
            &BTreeMap::new(),
            None,
            true,
        )
        .unwrap();
        let index = fs::read(repo.path().join(".git/index")).unwrap();
        assert_eq!(object_ids(repo.path(), body, true).unwrap(), before);
        assert_eq!(fs::read(repo.path().join(".git/index")).unwrap(), index);
        assert!(
            run("git", ["for-each-ref"], repo.path())
                .unwrap()
                .stdout
                .is_empty()
        );
    }
}
