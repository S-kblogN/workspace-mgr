//! Ordinary text references need an explicit repair before their directory moves.
//! Git control files and workspace-mgr metadata have their own verified rewrites.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::error::{Error, IoContext, Result};

const MAX_REPORTED_REFERENCES: usize = 200;

pub fn validate(
    source: &Path,
    destination: &Path,
    repository_path: &str,
    git_controls: &[PathBuf],
) -> Result<()> {
    let canonical = source.canonicalize().at(source)?;
    let mut replacements = BTreeSet::from([
        (
            source.to_string_lossy().into_owned(),
            destination.to_string_lossy().into_owned(),
        ),
        (
            canonical.to_string_lossy().into_owned(),
            destination.to_string_lossy().into_owned(),
        ),
    ]);
    replacements.insert((
        repository_path.to_owned(),
        "the task's new repository-relative path".to_owned(),
    ));
    let git_controls = git_controls
        .iter()
        .flat_map(|path| std::iter::once(path.clone()).chain(path.canonicalize().ok()))
        .collect::<BTreeSet<_>>();
    let mut references = BTreeSet::new();
    let mut truncated = false;
    let mut walker = WalkDir::new(source).follow_links(false).into_iter();
    while let Some(entry) = walker.next() {
        let entry = entry.map_err(|error| {
            Error::message(format!(
                "cannot inspect relocation text references: {error}"
            ))
        })?;
        let relative = entry
            .path()
            .strip_prefix(source)
            .expect("walked source path");
        let components = relative.components().collect::<Vec<_>>();
        if let Some(git) = components
            .iter()
            .position(|component| component.as_os_str() == ".git")
        {
            // Hooks are ordinary scripts. Object stores, refs and other Git
            // administration are handled by relocation and remain opaque here.
            if components.len() > git + 1 && components[git + 1].as_os_str() != "hooks" {
                if entry.file_type().is_dir() {
                    walker.skip_current_dir();
                }
                continue;
            }
        }
        if !entry.file_type().is_file()
            || control_file(entry.path())
            || git_controls.contains(entry.path())
            || (!git_controls.is_empty()
                && entry
                    .path()
                    .canonicalize()
                    .is_ok_and(|path| git_controls.contains(&path)))
        {
            continue;
        }
        let mut reader = BufReader::new(File::open(entry.path()).at(entry.path())?);
        let sample = reader.fill_buf().at(entry.path())?;
        // Treat binary payloads as opaque. No extension or ignore rule exempts scripts,
        // documentation, or other ordinary text, including ignored local content.
        if sample.contains(&0)
            || std::str::from_utf8(sample).is_err_and(|error| error.error_len().is_some())
        {
            continue;
        }
        let mut line = 1;
        let mut chunk = Vec::new();
        let mut tail = Vec::new();
        let mut text_boundary = true;
        let overlap = replacements
            .iter()
            .map(|(old, _)| old.len() + 1)
            .max()
            .unwrap_or(1);
        loop {
            chunk.clear();
            // Bound a single unbroken line while retaining enough overlap to find
            // a reference crossing a read boundary. Large text files are scanned too.
            let count = reader
                .by_ref()
                .take(65_536)
                .read_until(b'\n', &mut chunk)
                .at(entry.path())?;
            let mut text = std::mem::take(&mut tail);
            text.extend_from_slice(&chunk);
            for (old, new) in &replacements {
                if contains_path(&text, old.as_bytes(), text_boundary, count == 0) {
                    let reference = (entry.path().to_path_buf(), line, old.clone(), new.clone());
                    if references.len() < MAX_REPORTED_REFERENCES {
                        references.insert(reference);
                    } else if !references.contains(&reference) {
                        truncated = true;
                    }
                }
            }
            if count == 0 {
                break;
            }
            if chunk.last() == Some(&b'\n') {
                line += 1;
                text_boundary = true;
            } else {
                text_boundary &= text.len() <= overlap;
                tail.extend_from_slice(&text[text.len().saturating_sub(overlap)..]);
            }
        }
    }
    if references.is_empty() {
        return Ok(());
    }
    let mut message = String::from(
        "relocation would invalidate ordinary file path references; repair these before moving:\n",
    );
    for (path, line, old, new) in references {
        message.push_str(&format!(
            "  {}:{line}: {old:?} -> {new:?}\n",
            path.display()
        ));
    }
    if truncated {
        message.push_str("  Additional matching locations omitted after 200 references; repair these and repeat preview for the remaining locations.\n");
    }
    message.push_str("Derive task-local inputs and commands from the script's own directory, or use paths relative to the task directory in README instructions. Review and publish tracked repairs, refresh, then rerun archive --dry-run. Local ignored scripts need the same repair. No file has been rewritten.");
    Err(Error::message(message))
}

fn control_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    matches!(
        name,
        ".git"
            | ".workspace-mgr-task.toml"
            | ".workspace-mgr-legacy.json"
            | ".workspace-mgr-archive.json"
    ) || name.ends_with(".dvc")
}

fn contains_path(text: &[u8], path: &[u8], text_boundary: bool, eof: bool) -> bool {
    if path.is_empty() || text.len() < path.len() {
        return false;
    }
    text.windows(path.len())
        .enumerate()
        .any(|(index, candidate)| {
            candidate == path
                && (if index == 0 {
                    text_boundary
                } else {
                    !path_component(text[index - 1])
                })
                && (if index + path.len() == text.len() {
                    eof
                } else {
                    !path_component(text[index + path.len()])
                })
        })
}

fn path_component(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.') || byte >= 0x80
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("20260712-120000-task");
        let destination = temp.path().join("2026/07/20260712-120000-task");
        fs::create_dir_all(&source).unwrap();
        (temp, source, destination)
    }

    #[test]
    fn reports_all_scripts_and_readme_locations_without_editing_them() {
        let (_temp, source, destination) = fixture();
        let script = format!("#!/bin/sh\ncat '{}/data/input.tsv'\n", source.display());
        fs::write(source.join("run.sh"), &script).unwrap();
        fs::write(
            source.join("README.md"),
            "# Run\npython 20260712-120000-task/train.py\n",
        )
        .unwrap();
        let error = validate(&source, &destination, "20260712-120000-task", &[])
            .unwrap_err()
            .to_string();
        assert!(error.contains("run.sh:2"), "{error}");
        assert!(error.contains("README.md:2"), "{error}");
        assert!(error.contains("Derive task-local inputs"));
        assert_eq!(fs::read_to_string(source.join("run.sh")).unwrap(), script);
        assert!(!destination.exists());
    }

    #[test]
    fn permits_repaired_relative_commands_and_opaque_storage_or_git_metadata() {
        let (_temp, source, destination) = fixture();
        fs::write(
            source.join("run.py"),
            "from pathlib import Path\ninput = Path(__file__).parent / 'data' / 'input.tsv'\n",
        )
        .unwrap();
        fs::write(
            source.join("README.md"),
            "From this task directory, run `python run.py`.\n",
        )
        .unwrap();
        fs::write(
            source.join(".workspace-mgr-task.toml"),
            format!("path = {:?}\n", source.display()),
        )
        .unwrap();
        fs::write(
            source.join("data.dvc"),
            format!("path: {}\n", source.display()),
        )
        .unwrap();
        fs::create_dir(source.join(".git")).unwrap();
        fs::write(
            source.join(".git/config"),
            source.to_string_lossy().as_bytes(),
        )
        .unwrap();
        fs::write(
            source.join("data.bin"),
            [source.to_string_lossy().as_bytes(), &[0, 0xff]].concat(),
        )
        .unwrap();
        validate(&source, &destination, "20260712-120000-task", &[]).unwrap();
    }

    #[test]
    fn scans_ignored_text_and_references_crossing_large_read_boundaries() {
        let (_temp, source, destination) = fixture();
        fs::create_dir(source.join(".cache")).unwrap();
        let text = format!("{} '{}/input'\n", " ".repeat(65_530), source.display());
        fs::write(source.join(".cache/local-script"), text).unwrap();
        let error = validate(&source, &destination, "20260712-120000-task", &[])
            .unwrap_err()
            .to_string();
        assert!(error.contains(".cache/local-script:1"), "{error}");
    }

    #[test]
    fn does_not_match_a_different_task_with_a_shared_prefix() {
        assert!(!contains_path(
            b"20260712-120000-task-backup/input",
            b"20260712-120000-task",
            true,
            true
        ));
        assert!(!contains_path(
            b"other20260712-120000-task/input",
            b"20260712-120000-task",
            true,
            true
        ));
        assert!(contains_path(
            b"$REPO/20260712-120000-task/input",
            b"20260712-120000-task",
            true,
            true
        ));
    }

    #[test]
    fn checks_lookahead_at_chunk_boundaries_and_handles_true_eof() {
        let (_temp, source, destination) = fixture();
        let old = "20260712-120000-task";
        let prefix = " ".repeat(65_536 - old.len());
        fs::write(
            source.join("README.md"),
            format!("{prefix}{old}-backup/input\n"),
        )
        .unwrap();
        validate(&source, &destination, old, &[]).unwrap();
        fs::write(source.join("README.md"), format!("{prefix}{old}")).unwrap();
        assert!(
            validate(&source, &destination, old, &[])
                .unwrap_err()
                .to_string()
                .contains("README.md:1")
        );
    }

    #[test]
    fn retained_overlap_does_not_invent_a_left_path_boundary() {
        let (_temp, source, destination) = fixture();
        let old = source.to_string_lossy();
        let prefix = " ".repeat(65_536 - old.len() - 2);
        fs::write(
            source.join("README.md"),
            format!("{prefix}X{old} trailing text\n"),
        )
        .unwrap();
        // The task ID inside this non-path token is also prefixed by a
        // component character so neither spelling is a standalone source path.
        validate(&source, &destination, "no-repository-path", &[]).unwrap();
    }

    #[test]
    fn ordinary_git_hook_scripts_are_checked() {
        let (_temp, source, destination) = fixture();
        fs::create_dir_all(source.join(".git/hooks")).unwrap();
        fs::write(
            source.join(".git/hooks/pre-commit"),
            format!("#!/bin/sh\ncd '{}'\n", source.display()),
        )
        .unwrap();
        let error = validate(&source, &destination, "20260712-120000-task", &[])
            .unwrap_err()
            .to_string();
        assert!(error.contains(".git/hooks/pre-commit:2"), "{error}");
    }

    #[test]
    fn repeated_log_paths_have_bounded_diagnostics() {
        let (_temp, source, destination) = fixture();
        fs::write(
            source.join("run.log"),
            "20260712-120000-task/input\n".repeat(5_000),
        )
        .unwrap();
        let error = validate(&source, &destination, "20260712-120000-task", &[])
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("Additional matching locations omitted after 200"),
            "{error}"
        );
        assert!(error.len() < 100_000);
        assert!(!destination.exists());
    }
}
