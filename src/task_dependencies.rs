//! Preflight the task dependency graph against a complete proposed move batch.
//!
//! This is a conservative static check, not a general language interpreter.
//! Only explicit file-relative anchors can prove that a dependency survives.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};

use walkdir::WalkDir;

use crate::error::{Error, IoContext, Result};

const CHUNK_BYTES: u64 = 65_536;
const MAX_REFERENCES: usize = 200;
const MAX_ANCHORS: usize = 256;

/// Repository-relative locations for every current task, including stationary
/// tasks. A stationary task has identical source and destination paths.
#[derive(Debug, Clone)]
pub(crate) struct TaskLocation {
    pub source: PathBuf,
    pub destination: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct DependencyReference {
    pub file: PathBuf,
    pub line: usize,
    pub target: PathBuf,
    pub destination: PathBuf,
    pub literal: String,
    pub reason: String,
}

#[derive(Debug, Default)]
pub(crate) struct DependencyReport {
    pub references: Vec<DependencyReference>,
    pub truncated: bool,
}

pub(crate) fn validate(
    root: &Path,
    tasks: &[TaskLocation],
    git_controls: &[PathBuf],
    historical_records: &[PathBuf],
) -> Result<()> {
    let report = scan(root, tasks, git_controls, historical_records)?;
    if report.references.is_empty() {
        return Ok(());
    }
    let mut message = String::from(
        "archive would invalidate or cannot verify cross-task path dependencies; repair these before moving:\n",
    );
    for task in tasks.iter().filter(|task| task.source != task.destination) {
        message.push_str(&format!(
            "  proposed move: {} -> {}\n",
            task.source.display(),
            task.destination.display()
        ));
    }
    for reference in report.references {
        message.push_str(&format!(
            "  {}:{}: {:?} -> {:?}: {:?}; {}\n",
            reference.file.display(),
            reference.line,
            reference.target,
            reference.destination,
            reference.literal,
            reference.reason
        ));
    }
    if report.truncated {
        message.push_str("  Additional matching locations omitted after 200 references; repair these and repeat preview.\n");
    }
    message.push_str("Inspect the entire archive batch: tasks outside the batch can depend on moved tasks too. Resolve dependencies from a declared repository root and the destination task paths, or replace them with reviewed task-local inputs. Publish tracked repairs and repeat archive --dry-run. Dynamic path construction and caller-dependent working directories require an explicit dependency repair; no files have been rewritten.");
    Err(Error::message(message))
}

pub(crate) fn scan(
    root: &Path,
    tasks: &[TaskLocation],
    git_controls: &[PathBuf],
    historical_records: &[PathBuf],
) -> Result<DependencyReport> {
    let root = root.canonicalize().at(root)?;
    for task in tasks {
        validate_task_path(&task.source)?;
        validate_task_path(&task.destination)?;
    }
    if tasks.iter().all(|task| task.source == task.destination) {
        return Ok(DependencyReport::default());
    }
    let controls = git_controls
        .iter()
        .flat_map(|path| std::iter::once(path.clone()).chain(path.canonicalize().ok()))
        .collect::<BTreeSet<_>>();
    let historical = historical_records
        .iter()
        .map(|path| {
            if path.is_absolute() {
                path.clone()
            } else {
                root.join(path)
            }
        })
        .collect::<BTreeSet<_>>();
    let mut references = BTreeSet::new();
    let mut truncated = false;
    let mut walker = WalkDir::new(&root).follow_links(false).into_iter();
    while let Some(entry) = walker.next() {
        let entry = entry.map_err(|error| {
            Error::message(format!("cannot inspect cross-task dependencies: {error}"))
        })?;
        let relative = entry.path().strip_prefix(&root).expect("walked repository");
        if skip_directory(relative, tasks, root.join("Cargo.toml").is_file()) {
            if entry.file_type().is_dir() {
                walker.skip_current_dir();
            }
            continue;
        }
        if entry.file_type().is_symlink() {
            if let Some(reference) = symlink_dependency(&root, relative, tasks)? {
                record(&mut references, &mut truncated, reference);
            }
            continue;
        }
        if !entry.file_type().is_file()
            || control_file(entry.path())
            || controls.contains(entry.path())
            || historical.contains(entry.path())
            || entry
                .path()
                .canonicalize()
                .is_ok_and(|path| controls.contains(&path))
        {
            continue;
        }
        let owner = tasks
            .iter()
            .filter(|task| relative.starts_with(&task.source))
            .max_by_key(|task| task.source.components().count());
        let relocated_file = owner.map_or_else(
            || entry.path().to_path_buf(),
            |task| {
                root.join(&task.destination)
                    .join(relative.strip_prefix(&task.source).unwrap())
            },
        );
        let mut reader = BufReader::new(File::open(entry.path()).at(entry.path())?);
        let sample = reader.fill_buf().at(entry.path())?;
        if sample.contains(&0)
            || std::str::from_utf8(sample).is_err_and(|error| error.error_len().is_some())
        {
            if crate::historical_records::is_source_file(entry.path()) {
                return Err(Error::message(format!(
                    "cannot verify path dependencies in non-UTF-8 source file {}; convert this source to UTF-8 or explicitly repair its encoding before archive",
                    entry.path().display()
                )));
            }
            continue;
        }
        let shell_file = entry
            .path()
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| matches!(ext, "sh" | "bash" | "zsh"))
            || sample.starts_with(b"#!/bin/sh")
            || sample.starts_with(b"#!/bin/bash")
            || sample.starts_with(b"#!/usr/bin/env bash");
        let mut anchors = BTreeMap::new();
        let mut line_number = 1;
        let mut chunk = Vec::new();
        let mut tail = Vec::new();
        let mut shell_anchor = false;
        let mut opaque_shell = false;
        let mut trusted_symbols = true;
        let overlap = tasks
            .iter()
            .map(|task| root.join(&task.source).as_os_str().len() + 256)
            .max()
            .unwrap_or(256);
        loop {
            chunk.clear();
            let count = reader
                .by_ref()
                .take(CHUNK_BYTES)
                .read_until(b'\n', &mut chunk)
                .at(entry.path())?;
            if count == 0 {
                break;
            }
            let mut bytes = std::mem::take(&mut tail);
            bytes.extend_from_slice(&chunk);
            let text = String::from_utf8_lossy(&bytes);
            let compact: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
            if [
                "__file__=",
                "Path=",
                "pathlib=",
                "os=",
                "os.path=",
                "os.path.dirname=",
            ]
            .iter()
            .any(|marker| {
                compact.match_indices(marker).any(|(position, _)| {
                    position == 0 || !path_name_byte(compact.as_bytes()[position - 1])
                })
            }) || ["globals(", "locals(", "exec(", "eval("]
                .iter()
                .any(|marker| compact.contains(marker))
            {
                trusted_symbols = false;
                anchors.clear();
            }
            if trusted_symbols {
                update_anchors(&text, &root, entry.path(), &relocated_file, &mut anchors);
            }
            if anchors.len() >= MAX_ANCHORS {
                return Err(Error::message(format!(
                    "cannot verify additional path aliases in {}; simplify or explicitly repair these dynamic dependencies before archive",
                    entry.path().display()
                )));
            }
            if shell_file {
                update_shell_anchor(&text, &mut shell_anchor, &mut opaque_shell);
            }
            if let Some((target, literal, reason)) = dynamic_dependency(
                &text,
                entry.path(),
                &relocated_file,
                owner,
                &anchors,
                tasks,
                trusted_symbols,
            ) {
                record(
                    &mut references,
                    &mut truncated,
                    DependencyReference {
                        file: relative.to_path_buf(),
                        line: line_number,
                        target: target.source.clone(),
                        destination: target.destination.clone(),
                        literal,
                        reason,
                    },
                );
            }
            for invoked in invoked_records(&text, &historical, &root) {
                record(&mut references, &mut truncated, DependencyReference {
                    file: relative.to_path_buf(),
                    line: line_number,
                    target: invoked.clone(),
                    destination: invoked,
                    literal: text.trim().chars().take(200).collect(),
                    reason: "acknowledged historical record can be selected by an execution, source, or import command; it cannot be exempted as nonoperational".to_owned(),
                });
            }
            let complete_end =
                chunk.last() == Some(&b'\n') || reader.fill_buf().at(entry.path())?.is_empty();
            for task in tasks {
                if task.source == task.destination
                    && owner.is_none_or(|owner| owner.source == owner.destination)
                {
                    continue;
                }
                for occurrence in occurrences(&text, task, complete_end) {
                    let reason = dependency_reason(
                        &root,
                        entry.path(),
                        &relocated_file,
                        task,
                        &occurrence,
                        &ReferenceAnchors {
                            aliases: &anchors,
                            shell: shell_anchor,
                            file_symbols: trusted_symbols,
                        },
                    );
                    let Some(reason) = reason else {
                        continue;
                    };
                    let reference = DependencyReference {
                        file: relative.to_path_buf(),
                        line: line_number,
                        target: task.source.clone(),
                        destination: task.destination.clone(),
                        literal: occurrence.value.chars().take(200).collect(),
                        reason,
                    };
                    record(&mut references, &mut truncated, reference);
                }
            }
            if chunk.last() == Some(&b'\n') {
                line_number += 1;
            } else {
                // Include the preceding byte so truncating a long token cannot
                // invent a component boundary at the start of the next chunk.
                tail.extend_from_slice(&bytes[bytes.len().saturating_sub(overlap + 1)..]);
            }
        }
    }
    Ok(DependencyReport {
        references: references.into_iter().collect(),
        truncated,
    })
}

fn record(
    references: &mut BTreeSet<DependencyReference>,
    truncated: &mut bool,
    reference: DependencyReference,
) {
    if references.len() < MAX_REFERENCES {
        references.insert(reference);
    } else if !references.contains(&reference) {
        *truncated = true;
    }
}

fn validate_task_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(Error::message(format!(
            "task dependency location must be a normalized repository-relative path: {}",
            path.display()
        )));
    }
    Ok(())
}

fn symlink_dependency(
    root: &Path,
    file: &Path,
    tasks: &[TaskLocation],
) -> Result<Option<DependencyReference>> {
    let absolute = root.join(file);
    let target = std::fs::read_link(&absolute).at(&absolute)?;
    let before = normalize(&absolute.parent().unwrap().join(&target));
    let owner = tasks
        .iter()
        .filter(|task| file.starts_with(&task.source))
        .max_by_key(|task| task.source.components().count());
    let relocated = owner.map_or_else(
        || absolute.clone(),
        |task| {
            root.join(&task.destination)
                .join(file.strip_prefix(&task.source).unwrap())
        },
    );
    let after = normalize(&relocated.parent().unwrap().join(&target));
    let canonical = absolute.canonicalize().unwrap_or_else(|_| before.clone());
    let matched = tasks
        .iter()
        .filter(|task| case_alias_prefix(&canonical, &root.join(&task.source)))
        .max_by_key(|task| task.source.components().count())
        .or_else(|| {
            tasks
                .iter()
                .filter(|task| case_alias_prefix(&before, &root.join(&task.source)))
                .max_by_key(|task| task.source.components().count())
        });
    let affected = if let Some(task) = matched {
        if task.source == task.destination
            && owner.is_none_or(|owner| owner.source == owner.destination)
        {
            return Ok(None);
        }
        let original = root.join(&task.source);
        let original_target = if case_alias_prefix(&canonical, &original) {
            &canonical
        } else {
            &before
        };
        let suffix = original_target
            .components()
            .skip(original.components().count())
            .collect::<PathBuf>();
        if after == root.join(&task.destination).join(suffix) {
            return Ok(None);
        }
        task
    } else if before != after {
        let Some(owner) = owner else {
            return Ok(None);
        };
        owner
    } else {
        return Ok(None);
    };
    Ok(Some(DependencyReference {
        file: file.to_path_buf(),
        line: 1,
        target: affected.source.clone(),
        destination: affected.destination.clone(),
        literal: target.to_string_lossy().chars().take(200).collect(),
        reason: "symlink target would no longer resolve to the same task content after this batch"
            .to_owned(),
    }))
}

fn skip_directory(path: &Path, tasks: &[TaskLocation], cargo_repository: bool) -> bool {
    let components = path.components().collect::<Vec<_>>();
    if components
        .first()
        .is_some_and(|part| part.as_os_str() == ".dvc")
    {
        return true;
    }
    if cargo_repository
        && components
            .first()
            .is_some_and(|part| part.as_os_str() == "target")
        && components.iter().skip(1).any(|part| {
            matches!(
                part.as_os_str().to_str(),
                Some("deps" | ".fingerprint" | "incremental" | "build" | "examples")
            )
        })
        && !tasks
            .iter()
            .any(|task| task.source.starts_with(path) || path.starts_with(&task.source))
    {
        return true;
    }
    if components.len() >= 2
        && components[0].as_os_str() == ".workspace-mgr"
        && components[1].as_os_str() == "local"
    {
        return true;
    }
    if let Some(git) = components
        .iter()
        .position(|component| component.as_os_str() == ".git")
    {
        return components.len() > git + 1 && components[git + 1].as_os_str() != "hooks";
    }
    false
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

#[derive(Debug)]
struct Occurrence {
    value: String,
    prefix: String,
}

fn occurrences(text: &str, task: &TaskLocation, complete_end: bool) -> Vec<Occurrence> {
    let Some(basename) = task.source.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    let bytes = text.as_bytes();
    let mut result = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if matches!(bytes[index], b'\'' | b'"' | b'`') {
            let quote = bytes[index];
            let start = index + 1;
            index = start;
            while index < bytes.len() && bytes[index] != quote && bytes[index] != b'\n' {
                if bytes[index] == b'\\' && index + 1 < bytes.len() {
                    index += 1;
                }
                index += 1;
            }
            let value = &text[start..index];
            if contains_component(value, basename, index < bytes.len() || complete_end) {
                result.push(Occurrence {
                    value: value.to_owned(),
                    prefix: bounded_prefix(&text[..start - 1]),
                });
            }
            index += 1;
        } else if path_byte(bytes[index]) {
            let start = index;
            while index < bytes.len() && path_byte(bytes[index]) {
                index += 1;
            }
            if index == bytes.len() && !complete_end {
                break;
            }
            let value = &text[start..index];
            if contains_component(value, basename, index < bytes.len() || complete_end) {
                result.push(Occurrence {
                    value: value.to_owned(),
                    prefix: bounded_prefix(&text[..start]),
                });
            }
        } else {
            index += 1;
        }
    }
    result
}

fn bounded_prefix(text: &str) -> String {
    let mut start = text.len().saturating_sub(1024);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
}

fn path_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || byte >= 0x80
        || matches!(byte, b'/' | b'.' | b'_' | b'-' | b'$' | b'{' | b'}' | b':')
}

fn contains_component(value: &str, component: &str, complete_end: bool) -> bool {
    let folded = value.to_ascii_lowercase();
    let component = component.to_ascii_lowercase();
    folded.match_indices(&component).any(|(index, _)| {
        let before = value.as_bytes().get(index.wrapping_sub(1));
        let after = value.as_bytes().get(index + component.len());
        before.is_none_or(|byte| !path_name_byte(*byte))
            && (after.is_some_and(|byte| !path_name_byte(*byte))
                || (after.is_none() && complete_end))
    })
}

type Anchor = (PathBuf, PathBuf);

struct ReferenceAnchors<'a> {
    aliases: &'a BTreeMap<String, Anchor>,
    shell: bool,
    file_symbols: bool,
}

fn update_anchors(
    text: &str,
    root: &Path,
    before: &Path,
    after: &Path,
    anchors: &mut BTreeMap<String, Anchor>,
) {
    let calls: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
    if ["globals(", "locals(", "exec(", "eval("]
        .iter()
        .any(|marker| calls.contains(marker))
    {
        anchors.clear();
        return;
    }
    if text.starts_with(char::is_whitespace) || text.contains(';') {
        if text.contains('=') {
            for (name, anchor) in anchors.iter_mut() {
                if has_component_in_text(text, name) {
                    *anchor = (PathBuf::new(), PathBuf::new());
                }
            }
        }
        return;
    }
    let Some((name, rhs)) = text.split_once('=') else {
        return;
    };
    let name = name.trim();
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        for (existing, anchor) in anchors.iter_mut() {
            if has_component_in_text(name, existing) {
                *anchor = (PathBuf::new(), PathBuf::new());
            }
        }
        return;
    }
    let compact: String = rhs.chars().filter(|ch| !ch.is_whitespace()).collect();
    // Alias assignment must be an entire supported expression. A matching
    // subexpression after `or` or `else` does not establish the actual value.
    let file = (compact.starts_with("Path(__file__)") || compact.starts_with("os.path.dirname("))
        .then(|| file_anchor(rhs, before, after))
        .flatten();
    let fixed = ['\'', '"']
        .into_iter()
        .any(|quote| compact == format!("Path({quote}{}{quote})", root.display()))
        .then(|| (root.to_path_buf(), root.to_path_buf()));
    if let Some(anchor) = file.or(fixed) {
        if anchors.len() < MAX_ANCHORS || anchors.contains_key(name) {
            anchors.insert(name.to_owned(), anchor);
        }
    } else if anchors.contains_key(name)
        || compact.contains("Path(")
        || compact.contains("Path.cwd()")
        || compact.contains("os.getcwd()")
        || compact.contains("__file__")
    {
        if anchors.len() < MAX_ANCHORS || anchors.contains_key(name) {
            anchors.insert(name.to_owned(), (PathBuf::new(), PathBuf::new()));
        }
    } else {
        anchors.remove(name);
    }
}

fn update_shell_anchor(text: &str, anchor: &mut bool, opaque: &mut bool) {
    let trimmed = text.trim();
    if trimmed.starts_with('#') || trimmed.is_empty() {
        return;
    }
    if [
        "if ",
        "for ",
        "while ",
        "case ",
        "function ",
        "eval ",
        "source ",
        "source(",
        "dofile(",
        ". ",
    ]
    .iter()
    .any(|prefix| trimmed.starts_with(prefix))
        || trimmed.contains("()")
        || trimmed.ends_with('{')
        || trimmed.starts_with("pushd ")
        || trimmed.starts_with("popd")
    {
        *opaque = true;
        *anchor = false;
    }
    if trimmed.starts_with("cd ") || trimmed.starts_with("cd\t") {
        let compact: String = trimmed.chars().filter(|ch| !ch.is_whitespace()).collect();
        *anchor = !*opaque
            && matches!(
                compact.as_str(),
                "cd\"$(dirname\"$0\")\""
                    | "cd--\"$(dirname\"$0\")\""
                    | "cd--\"$(dirname--\"$0\")\""
                    | "cd\"$(dirname\"${BASH_SOURCE[0]}\")\""
            );
    }
}

fn file_anchor(prefix: &str, before: &Path, after: &Path) -> Option<Anchor> {
    let compact: String = prefix.chars().filter(|ch| !ch.is_whitespace()).collect();
    let depth = if let Some(start) = compact.rfind("Path(__file__)") {
        let mut suffix = &compact[start + "Path(__file__)".len()..];
        let mut depth = 0usize;
        loop {
            if let Some(rest) = suffix
                .strip_prefix(".resolve()")
                .or_else(|| suffix.strip_prefix(".absolute()"))
            {
                suffix = rest;
            } else if let Some(rest) = suffix.strip_prefix(".parents[") {
                let (index, rest) = rest.split_once(']')?;
                depth = depth.checked_add(index.parse::<usize>().ok()?.checked_add(1)?)?;
                suffix = rest;
            } else if let Some(rest) = suffix.strip_prefix(".parent") {
                depth = depth.checked_add(1)?;
                suffix = rest;
            } else {
                break;
            }
        }
        if !matches!(suffix, "" | "/" | "," | ".joinpath(") {
            return None;
        }
        depth
    } else if compact.contains("__file__") && compact.contains("os.path.dirname(") {
        let position = compact.find("__file__")?;
        let mut prefix = &compact[..position];
        let mut depth = 0;
        while let Some(rest) = prefix.strip_suffix("os.path.dirname(") {
            depth += 1;
            prefix = rest;
        }
        let suffix = &compact[position + "__file__".len()..];
        let close = ")".repeat(depth);
        if depth == 0
            || !(prefix.is_empty() || prefix.ends_with('=') || prefix.ends_with("os.path.join("))
            || !(suffix == close || suffix == format!("{close},"))
        {
            return None;
        }
        depth
    } else {
        return None;
    };
    if depth == 0 || depth > 32 {
        return None;
    }
    let mut before = before.to_path_buf();
    let mut after = after.to_path_buf();
    for _ in 0..depth {
        if !before.pop() || !after.pop() {
            return None;
        }
    }
    Some((before, after))
}

fn explicit_root_anchor(prefix: &str, root: &Path) -> Option<Anchor> {
    let compact: String = prefix.chars().filter(|ch| !ch.is_whitespace()).collect();
    for quote in ['\'', '"'] {
        let expression = format!("Path({quote}{}{quote})", root.display());
        if compact.ends_with(&expression)
            || compact.ends_with(&format!("{expression}/"))
            || compact.ends_with(&format!("{expression}.joinpath("))
            || compact.ends_with(&format!("{expression},"))
        {
            return Some((root.to_path_buf(), root.to_path_buf()));
        }
        let expression = format!("os.path.join({quote}{}{quote},", root.display());
        if compact.ends_with(&expression) {
            return Some((root.to_path_buf(), root.to_path_buf()));
        }
    }
    None
}

fn dynamic_dependency<'a>(
    text: &str,
    before: &Path,
    after: &Path,
    owner: Option<&'a TaskLocation>,
    anchors: &BTreeMap<String, Anchor>,
    tasks: &'a [TaskLocation],
    trusted_symbols: bool,
) -> Option<(&'a TaskLocation, String, String)> {
    let affected = owner.or_else(|| tasks.iter().find(|task| task.source != task.destination))?;
    let compact: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
    let mut candidates = Vec::new();
    for (start, _) in compact.match_indices("Path(__file__)") {
        let expression = compact[start..].split(';').next().unwrap_or_default();
        if let Some((prefix, tail)) = expression
            .split_once('/')
            .or_else(|| expression.split_once(".joinpath("))
        {
            if let Some(anchor) = file_anchor(prefix, before, after) {
                candidates.push((anchor, tail));
            }
        }
    }
    if compact.contains("os.path.join(") && compact.contains("__file__") {
        if let Some(start) = compact
            .find("__file__")
            .map(|start| start + "__file__".len())
        {
            if let Some(end) = compact[start..].find(',').map(|end| end + start + 1) {
                if let Some(anchor) = file_anchor(&compact[..end], before, after) {
                    candidates.push((anchor, &compact[end..]));
                }
            }
        }
    }
    for (name, anchor) in anchors {
        for marker in [
            format!("{name}/"),
            format!("({name},"),
            format!("{name}.joinpath("),
        ] {
            for (position, _) in compact.match_indices(&marker) {
                candidates.push((anchor.clone(), &compact[position + marker.len()..]));
            }
        }
    }
    let task_before = owner.and_then(|owner| {
        before
            .ancestors()
            .find(|path| path.ends_with(&owner.source))
    });
    let task_after = owner.and_then(|owner| {
        after
            .ancestors()
            .find(|path| path.ends_with(&owner.destination))
    });
    let uncertain_relative = [
        "Path('../",
        "Path(\"../",
        "Path('..')",
        "Path(\"..\")",
        "os.path.join('..',",
        "os.path.join(\"..\",",
        "Path.cwd()/",
        "Path.cwd())/",
        "Path.cwd().joinpath(",
        "os.path.join(os.getcwd()",
    ]
    .iter()
    .any(|marker| compact.contains(marker));
    let uncertain_anchor = candidates.into_iter().any(|(anchor, tail)| {
        if !trusted_symbols {
            return true;
        }
        if !concrete_local_suffix(tail) {
            return true;
        }
        if task_before.is_some_and(|root| anchor.0.starts_with(root))
            && task_after.is_some_and(|root| anchor.1.starts_with(root))
        {
            return false;
        }
        if let Some(quote) = tail
            .chars()
            .next()
            .filter(|quote| matches!(quote, '\'' | '"'))
        {
            if let Some(end) = tail[1..].find(quote) {
                let literal = &tail[1..end + 1];
                if tasks.iter().any(|task| {
                    task.source
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| has_component_in_text(literal, name))
                }) {
                    // This exact literal target is checked separately. A task
                    // basename in a comment or another expression cannot hide
                    // a different unresolved dynamic dependency on this line.
                    return false;
                }
                return anchor.0 != anchor.1;
            }
        }
        true
    });
    if !uncertain_relative && !uncertain_anchor {
        return None;
    }
    Some((
        affected,
        text.trim().chars().take(200).collect(),
        "outward file-relative dependency has no verified task target; an ancestor anchor or variable can resolve differently after this archive batch".to_owned(),
    ))
}

fn concrete_local_suffix(tail: &str) -> bool {
    let mut rest = tail.split(';').next().unwrap_or_default();
    loop {
        let Some(quote) = rest
            .chars()
            .next()
            .filter(|quote| matches!(quote, '\'' | '"'))
        else {
            return false;
        };
        let Some(end) = rest[1..].find(quote) else {
            return false;
        };
        if rest[1..end + 1].contains('\\') {
            return false;
        }
        let value = Path::new(&rest[1..end + 1]);
        if value.is_absolute()
            || value
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        {
            return false;
        }
        rest = &rest[end + 2..];
        if rest.is_empty()
            || rest.starts_with('#')
            || rest.chars().all(|ch| matches!(ch, ')' | ']'))
        {
            return true;
        }
        if let Some(next) = rest.strip_prefix('/').or_else(|| rest.strip_prefix(',')) {
            rest = next;
        } else {
            return false;
        }
    }
}

fn has_component_in_text(text: &str, component: &str) -> bool {
    text.match_indices(component).any(|(index, _)| {
        let before = text.as_bytes().get(index.wrapping_sub(1));
        let after = text.as_bytes().get(index + component.len());
        before.is_none_or(|byte| !path_name_byte(*byte))
            && after.is_none_or(|byte| !path_name_byte(*byte))
    })
}

fn path_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte >= 0x80 || matches!(byte, b'_' | b'-' | b'.')
}

fn invoked_records(text: &str, historical: &BTreeSet<PathBuf>, root: &Path) -> Vec<PathBuf> {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.starts_with('#') {
        return Vec::new();
    }
    let calls: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
    let interpreter = normalized
        .split(|ch: char| {
            ch.is_whitespace() || matches!(ch, '\'' | '"' | '(' | ')' | '[' | ']' | ',' | ';')
        })
        .any(|token| {
            let token = token.rsplit('/').next().unwrap_or(token);
            matches!(
                token,
                "bash"
                    | "sh"
                    | "zsh"
                    | "ruby"
                    | "Rscript"
                    | "node"
                    | "perl"
                    | "julia"
                    | "lua"
                    | "sys.executable"
            ) || ["python", "pypy"].iter().any(|prefix| {
                token.strip_prefix(prefix).is_some_and(|version| {
                    version
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || byte == b'.')
                })
            })
        });
    let mut words = normalized.split_whitespace();
    let command = words.next().unwrap_or("").trim_matches('"');
    let variable = command
        .strip_prefix('$')
        .unwrap_or("")
        .trim_start_matches('{')
        .trim_end_matches('}');
    let dynamic_command = !variable.is_empty()
        && variable
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && !words.next().is_some_and(|word| word.starts_with('='));
    let import_selection = [
        "spec_from_file_location(",
        "SourceFileLoader(",
        "importlib.import_module(",
    ]
    .iter()
    .any(|marker| calls.contains(marker));
    let inline_program = interpreter
        && normalized
            .split_whitespace()
            .any(|word| matches!(word, "-c" | "-e" | "-E" | "-ne" | "-pe"));
    let inline_source = normalized
        .split(|ch: char| ch.is_whitespace() || matches!(ch, '\'' | '"' | ';'))
        .any(|word| matches!(word, "source" | "." | "load"));
    // Inline-program operands can be historical data. Explicit source/evaluation
    // selectors still require verification of the selected file's role.
    if inline_program
        && ![
            "exec(",
            "execfile(",
            "eval",
            "runpy.",
            "load(",
            "source(",
            "dofile(",
        ]
        .iter()
        .any(|marker| calls.contains(marker))
        && !import_selection
        && !inline_source
    {
        return Vec::new();
    }
    let fixed_exec = fixed_literal_exec(&normalized.replace("exec (", "exec("));
    let operational = [
        "exec(",
        "execfile(",
        "runpy.",
        "source ",
        ". ",
        "python ",
        "python3 ",
        "bash ",
        "sh ",
        "load ",
    ]
    .iter()
    .any(|marker| text.contains(marker))
        || import_selection
        || interpreter
        || dynamic_command
        || calls.contains("exec(")
        || calls.contains("execfile(")
        || calls.contains("source(")
        || calls.contains("dofile(");
    if !operational || fixed_exec {
        return Vec::new();
    }
    let unresolved = ["exec(", "execfile(", "runpy.", "source(", "dofile("]
        .iter()
        .any(|marker| calls.contains(marker))
        || import_selection
        || dynamic_command
        || (interpreter && normalized.contains('$'))
        || ((text.contains("source ")
            || text.trim_start().starts_with(". ")
            || text.contains("python ")
            || text.contains("python3 ")
            || text.contains("bash ")
            || text.contains("sh "))
            && text.contains('$'));
    historical
        .iter()
        .filter(|path| {
            let relative = path.strip_prefix(root).unwrap_or(path).to_string_lossy();
            unresolved
                || text.contains(relative.as_ref())
                || path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| has_component_in_text(text, name))
        })
        .map(|path| path.strip_prefix(root).unwrap_or(path).to_path_buf())
        .collect()
}

fn fixed_literal_exec(text: &str) -> bool {
    let Some(rest) = text.trim().strip_prefix("exec(") else {
        return false;
    };
    let Some(quote) = rest.chars().next().filter(|ch| matches!(ch, '\'' | '"')) else {
        return false;
    };
    let Some(end) = rest[1..].find(quote) else {
        return false;
    };
    let body = &rest[1..end + 1];
    rest[end + 2..].trim() == ")"
        && ![
            "open(", "read_", "import", "exec(", "eval(", "load", "Path(",
        ]
        .iter()
        .any(|marker| body.contains(marker))
}

fn dependency_reason(
    root: &Path,
    file_before: &Path,
    file_after: &Path,
    task: &TaskLocation,
    occurrence: &Occurrence,
    context: &ReferenceAnchors<'_>,
) -> Option<String> {
    let original = root.join(&task.source);
    let destination = root.join(&task.destination);
    let value = Path::new(&occurrence.value);
    if value.is_absolute() {
        return (case_alias_prefix(value, &original) && original != destination)
            .then(|| "absolute dependency still names the target's original directory".to_owned());
    }
    let anchor = context
        .file_symbols
        .then(|| file_anchor(&occurrence.prefix, file_before, file_after))
        .flatten()
        .or_else(|| explicit_root_anchor(&occurrence.prefix, root))
        .or_else(|| {
            context.aliases.iter().find_map(|(name, anchor)| {
                let compact: String = occurrence
                    .prefix
                    .chars()
                    .filter(|ch| !ch.is_whitespace())
                    .collect();
                (compact.ends_with(&format!("{name}/"))
                    || compact.ends_with(&format!("{name},"))
                    || compact.ends_with(&format!("{name}.joinpath(")))
                .then(|| compact.rfind(name))
                .flatten()
                .filter(|position| {
                    *position == 0 || !path_name_byte(compact.as_bytes()[position - 1])
                })
                .map(|_| anchor.clone())
            })
        })
        .or_else(|| {
            context.shell.then(|| {
                (
                    file_before.parent().unwrap().to_path_buf(),
                    file_after.parent().unwrap().to_path_buf(),
                )
            })
        });
    if let Some((before, after)) = anchor {
        let old_target = normalize(&before.join(value));
        let new_target = normalize(&after.join(value));
        if value.starts_with(&task.destination) && new_target.starts_with(&destination) {
            return None;
        }
        if old_target.starts_with(&original) {
            let suffix = old_target.strip_prefix(&original).unwrap();
            if new_target == destination.join(suffix) {
                return None;
            }
            return Some(format!(
                "file-relative dependency would resolve to {}, expected {}",
                new_target.display(),
                destination.join(suffix).display()
            ));
        }
    }
    let root_target = normalize(&root.join(value));
    if root_target.starts_with(&original) && original != destination {
        return Some("repository-relative or undeclared-root dependency still names the target's original directory".to_owned());
    }
    Some("dependency anchor is not statically verified; caller working directories or dynamic path construction can change its target after this batch".to_owned())
}

fn case_alias_prefix(path: &Path, prefix: &Path) -> bool {
    let mut parts = path.components();
    prefix.components().all(|part| {
        parts.next().is_some_and(|actual| {
            actual
                .as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(&part.as_os_str().to_string_lossy())
        })
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
    use std::fs;

    #[test]
    fn parent_and_caller_cwd_selectors_without_task_names_require_repair() {
        let fixture = Fixture::new(Some(&format!("2026/07/{A}")), Some(&format!("2026/08/{B}")));
        for script in [
            "Path('..') / TARGET",
            "os.path.join('..', TARGET, 'input.tsv')",
            "Path.cwd() / TARGET",
            "os.path.join(os.getcwd(), TARGET)",
            "(Path.cwd()) / TARGET",
        ] {
            fixture.write(&format!("{A}/run.py"), &format!("input = {script}\n"));
            assert!(!fixture.scan().references.is_empty(), "{script}");
        }
        for prefix in [
            "ROOT = Path.cwd()",
            "ROOT = Path(os.getcwd())",
            "ROOT = os.getcwd()",
        ] {
            fixture.write(
                &format!("{A}/run.py"),
                &format!("{prefix}\ninput = ROOT / TARGET\n"),
            );
            assert!(!fixture.scan().references.is_empty(), "{prefix}");
        }
    }

    #[test]
    fn escaped_task_literals_from_stationary_callers_are_not_assumed_safe() {
        let fixture = Fixture::new(None, Some(&format!("2026/08/{B}")));
        fixture.write(
            &format!("{C}/run.py"),
            &format!(
                "input = Path(__file__).parents[1] / '{}\\x62'\n",
                B.strip_suffix('b').unwrap()
            ),
        );
        assert!(!fixture.scan().references.is_empty());
    }

    #[test]
    fn reassigned_file_symbols_cannot_prove_same_month_sibling_preservation() {
        let fixture = Fixture::new(Some(&format!("2026/07/{A}")), Some(&format!("2026/07/{B}")));
        for binding in [
            format!("__file__ = '{}/fake/run.py'", fixture.root.display()),
            "Path = custom_path".into(),
        ] {
            fixture.write(
                &format!("{A}/run.py"),
                &format!(
                    "{binding}\nROOT = Path(__file__).parents[1]\ninput = ROOT / '{B}/input.tsv'\n"
                ),
            );
            assert!(!fixture.scan().references.is_empty(), "{binding}");
        }
    }

    #[test]
    fn rearchive_checks_split_self_paths_at_an_unchanged_repository_anchor() {
        let mut fixture = Fixture::new(None, None);
        fixture.tasks[0].source = format!("2026/09/{A}").into();
        fixture.tasks[0].destination = format!("2026/07/{A}").into();
        fixture.write(
            &format!("2026/09/{A}/run.py"),
            &format!("input = Path(__file__).parents[3] / '2026' / '09' / '{A}' / 'input.tsv'\n"),
        );
        assert!(!fixture.scan().references.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn stationary_task_symlinks_to_moved_data_are_checked_without_following_links() {
        let fixture = Fixture::new(None, Some(&format!("2026/08/{B}")));
        fixture.write(&format!("{B}/input.tsv"), "payload\n");
        std::os::unix::fs::symlink(
            format!("../{B}/input.tsv"),
            fixture.root.join(C).join("input-link"),
        )
        .unwrap();
        assert_eq!(fixture.scan().references.len(), 1);
        assert_eq!(
            fs::read(fixture.root.join(C).join("input-link")).unwrap(),
            b"payload\n"
        );
        assert!(!fixture.root.join("2026").exists());
    }

    #[test]
    fn incomplete_chunk_tail_cannot_invent_a_component_boundary() {
        let fixture = Fixture::new(None, Some(&format!("2026/08/{B}")));
        let overlap = fixture
            .tasks
            .iter()
            .map(|task| fixture.root.join(&task.source).as_os_str().len() + 256)
            .max()
            .unwrap();
        fixture.write(
            "README.md",
            &format!(
                "{}X{B}/{}\n",
                " ".repeat(CHUNK_BYTES as usize - overlap - 1),
                "a".repeat(overlap - B.len() - 1)
            ),
        );
        assert!(fixture.scan().references.is_empty());
    }

    #[test]
    fn only_entire_anchor_assignments_can_prove_sibling_preservation() {
        let fixture = Fixture::new(Some(&format!("2026/07/{A}")), Some(&format!("2026/07/{B}")));
        for statement in [
            format!(
                "ROOT = Path('{}') or Path(__file__).parents[1]",
                fixture.root.display()
            ),
            format!(
                "ROOT = Path('{}') if True else Path(__file__).parents[1]",
                fixture.root.display()
            ),
            format!(
                "ROOT = os.path.dirname('{}/fake.py') or os.path.dirname(__file__)",
                fixture.root.display()
            ),
            format!(
                "ROOT = Path(__file__).parents[1]\nglobals().update({{'ROOT': Path('{}')}})",
                fixture.root.display()
            ),
        ] {
            fixture.write(
                &format!("{A}/run.py"),
                &format!("{statement}\ninput = ROOT / '{B}/input.tsv'\n"),
            );
            assert!(!fixture.scan().references.is_empty(), "{statement}");
        }
    }

    #[test]
    fn complex_shell_cwd_changes_cannot_prove_a_sibling_edge() {
        let fixture = Fixture::new(Some(&format!("2026/07/{A}")), Some(&format!("2026/07/{B}")));
        for command in [
            "cd \"$(dirname \"$0\")/../..\"",
            "cd \"$(dirname \"$0\")\"\npushd ../..",
            "fn() {\ncd \"$(dirname \"$0\")\"\n}",
        ] {
            fixture.write(
                &format!("{A}/run.sh"),
                &format!("{command}\ncat ../{B}/input.tsv\n"),
            );
            assert!(!fixture.scan().references.is_empty(), "{command}");
        }
    }

    #[test]
    fn case_aliased_targets_from_stationary_tasks_cannot_be_missed() {
        let fixture = Fixture::new(None, Some(&format!("2026/08/{B}")));
        let upper = B.to_ascii_uppercase();
        fixture.write(
            &format!("{C}/run.py"),
            &format!("input = Path(__file__).parents[1] / '{upper}/input.tsv'\n"),
        );
        fixture.write(
            "run.sh",
            &format!("cat '{}/{upper}/input.tsv'\n", fixture.root.display()),
        );
        assert_eq!(fixture.scan().references.len(), 2);
    }

    #[test]
    fn known_source_encodings_are_verified_before_binary_exclusion() {
        let fixture = Fixture::new(None, Some(&format!("2026/08/{B}")));
        fs::write(fixture.root.join(A).join("run.py"), [0xff, b'\n']).unwrap();
        let error = validate(&fixture.root, &fixture.tasks, &[], &[])
            .unwrap_err()
            .to_string();
        assert!(error.contains("non-UTF-8 source file"), "{error}");
    }

    #[test]
    fn interpreter_variants_cannot_execute_an_acknowledged_record() {
        let fixture = Fixture::new(Some(&format!("2026/07/{A}")), None);
        let record = PathBuf::from(format!("{A}/history.log"));
        fixture.write(record.to_str().unwrap(), "historical record\n");
        for command in [
            "python3.13\thistory.log",
            "/usr/bin/python3.13 history.log",
            "ruby history.log",
            "Rscript history.log",
            "Rscript -e \"source('history.log')\"",
            "\"$INTERPRETER\" history.log",
            "exec (Path('history.log').read_text())",
        ] {
            fixture.write(&format!("{A}/run.sh"), &format!("{command}\n"));
            assert!(
                validate(
                    &fixture.root,
                    &fixture.tasks,
                    &[],
                    std::slice::from_ref(&record)
                )
                .is_err(),
                "{command}"
            );
        }
    }

    #[test]
    fn inline_program_operands_and_hook_template_variables_can_be_inert_data() {
        let fixture = Fixture::new(Some(&format!("2026/07/{A}")), None);
        let record = PathBuf::from(format!("{A}/history.log"));
        fixture.write(record.to_str().unwrap(), "historical cwd\n");
        fixture.write(
            &format!("{A}/read.sh"),
            "python -c \"print(open('history.log').read())\"\nperl -ne 'print' \"$INPUT\"\n",
        );
        fixture.write(
            ".git/hooks/template.sample",
            "# $1 hook input\n$json_pkg = \"JSON::XS\";\n${\n",
        );
        validate(&fixture.root, &fixture.tasks, &[], &[record]).unwrap();
    }

    const A: &str = "20260712-120000-a";
    const B: &str = "20260812-120000-b";
    const C: &str = "20260912-120000-c";

    struct Fixture {
        _temporary: tempfile::TempDir,
        root: PathBuf,
        tasks: Vec<TaskLocation>,
    }

    impl Fixture {
        fn new(a: Option<&str>, b: Option<&str>) -> Self {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().canonicalize().unwrap();
            let tasks = [
                TaskLocation {
                    source: A.into(),
                    destination: a.unwrap_or(A).into(),
                },
                TaskLocation {
                    source: B.into(),
                    destination: b.unwrap_or(B).into(),
                },
                TaskLocation {
                    source: C.into(),
                    destination: C.into(),
                },
            ]
            .into();
            for task in [A, B, C] {
                fs::create_dir(root.join(task)).unwrap();
            }
            Self {
                _temporary: temporary,
                root,
                tasks,
            }
        }

        fn write(&self, path: &str, contents: &str) {
            let path = self.root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }

        fn scan(&self) -> DependencyReport {
            scan(&self.root, &self.tasks, &[], &[]).unwrap()
        }

        fn error(&self) -> String {
            validate(&self.root, &self.tasks, &[], &[])
                .unwrap_err()
                .to_string()
        }
    }

    #[test]
    fn different_month_pathlib_edges_are_reported_before_any_move() {
        let fixture = Fixture::new(Some(&format!("2026/07/{A}")), Some(&format!("2026/08/{B}")));
        let script = format!(
            "from pathlib import Path\ninput = Path(__file__).resolve().parent.parent / '{B}' / 'input.tsv'\n"
        );
        fixture.write(&format!("{A}/run.py"), &script);
        let report = fixture.scan();
        assert_eq!(report.references.len(), 1);
        assert_eq!(
            report.references[0].file,
            PathBuf::from(format!("{A}/run.py"))
        );
        assert_eq!(report.references[0].line, 2);
        assert!(
            report.references[0]
                .reason
                .contains("file-relative dependency")
        );
        assert_eq!(
            fs::read_to_string(fixture.root.join(A).join("run.py")).unwrap(),
            script
        );
        assert!(!fixture.root.join("2026").exists());
    }

    #[test]
    fn either_end_moving_breaks_the_sibling_edge() {
        for (a, b) in [
            (Some(format!("2026/07/{A}")), None),
            (None, Some(format!("2026/08/{B}"))),
        ] {
            let fixture = Fixture::new(a.as_deref(), b.as_deref());
            fixture.write(
                &format!("{A}/run.py"),
                &format!("input = Path(__file__).resolve().parents[1] / '{B}' / 'input.tsv'\n"),
            );
            assert_eq!(fixture.scan().references.len(), 1);
        }
    }

    #[test]
    fn same_parent_batch_preserves_provable_pathlib_and_os_dirname_edges() {
        let fixture = Fixture::new(Some(&format!("2026/07/{A}")), Some(&format!("2026/07/{B}")));
        fixture.write(&format!("{A}/run.py"), &format!("ROOT = Path(__file__).resolve().parents[1]\ninput = ROOT / '{B}/input.tsv'\nother = os.path.join(os.path.dirname(os.path.dirname(__file__)), '{B}', 'input.tsv')\n"));
        validate(&fixture.root, &fixture.tasks, &[], &[]).unwrap();
    }

    #[test]
    fn literal_task_names_are_not_required_for_outward_dynamic_detection() {
        let fixture = Fixture::new(Some(&format!("2026/07/{A}")), Some(&format!("2026/08/{B}")));
        for script in [
            "ROOT = Path(__file__).resolve().parents[1]\nTARGET = ''.join(['20260812', '-120000-', 'b'])\ninput = ROOT / TARGET / 'input.tsv'\n",
            "input = Path(__file__).resolve().parent.parent / VARIABLE / 'input.tsv'\n",
            "input = os.path.join(os.path.dirname(os.path.dirname(__file__)), VARIABLE, 'input.tsv')\n",
            "ROOT = Path(__file__).resolve().parents[1]\ninput = Path(ROOT, VARIABLE, 'input.tsv')\n",
            "input = Path(__file__).resolve().parents[1].joinpath(VARIABLE)\n",
        ] {
            fixture.write(&format!("{A}/run.py"), script);
            let error = fixture.error();
            assert!(
                error.contains("outward file-relative dependency"),
                "{script}\n{error}"
            );
        }
    }

    #[test]
    fn task_local_literal_paths_are_allowed_from_exact_file_locations() {
        let fixture = Fixture::new(Some(&format!("2026/07/{A}")), None);
        fixture.write(
            &format!("{A}/run.py"),
            "input = Path(__file__).resolve().parent / 'data' / 'input.tsv'\n",
        );
        fixture.write(
            &format!("{A}/scripts/nested.py"),
            "input = Path(__file__).resolve().parents[1] / 'data'\n",
        );
        validate(&fixture.root, &fixture.tasks, &[], &[]).unwrap();
    }

    #[test]
    fn unresolved_caller_cwd_is_not_assumed_safe_for_a_same_parent_batch() {
        let fixture = Fixture::new(Some(&format!("2026/07/{A}")), Some(&format!("2026/07/{B}")));
        fixture.write(&format!("{A}/run.sh"), &format!("cat ../{B}/input.tsv\n"));
        assert!(
            fixture
                .error()
                .contains("anchor is not statically verified")
        );
        fixture.write(
            &format!("{A}/run.sh"),
            &format!("cd \"$(dirname \"$0\")\"\ncat ../{B}/input.tsv\n"),
        );
        validate(&fixture.root, &fixture.tasks, &[], &[]).unwrap();
    }

    #[test]
    fn stationary_tasks_and_repository_outsiders_are_part_of_the_graph() {
        let fixture = Fixture::new(None, Some(&format!("2026/08/{B}")));
        fixture.write(
            &format!("{C}/run.py"),
            &format!("input = Path(__file__).parents[1] / '{B}'\n"),
        );
        fixture.write(
            "README.md",
            &format!("From the repository root, run python {B}/run.py\n"),
        );
        let report = fixture.scan();
        assert_eq!(report.references.len(), 2, "{report:?}");
        assert!(
            report
                .references
                .iter()
                .any(|reference| reference.file == Path::new("README.md"))
        );
    }

    #[test]
    fn absolute_and_root_relative_old_targets_remain_stale_in_same_parent_batch() {
        let fixture = Fixture::new(Some(&format!("2026/07/{A}")), Some(&format!("2026/07/{B}")));
        fixture.write(
            &format!("{A}/run.py"),
            &format!(
                "input = '{}/{B}/input.tsv'\nother = '{B}/input.tsv'\n",
                fixture.root.display()
            ),
        );
        assert_eq!(fixture.scan().references.len(), 2);
    }

    #[test]
    fn explicit_repository_root_allows_reviewed_future_destination_paths() {
        let fixture = Fixture::new(Some(&format!("2026/07/{A}")), Some(&format!("2026/08/{B}")));
        fixture.write(
            &format!("{A}/run.py"),
            &format!(
                "ROOT = Path('{}')\ninput = ROOT / '2026/08/{B}/input.tsv'\n",
                fixture.root.display()
            ),
        );
        validate(&fixture.root, &fixture.tasks, &[], &[]).unwrap();
    }

    #[test]
    fn partial_path_builder_expressions_are_not_mistaken_for_verified_anchors() {
        let fixture = Fixture::new(Some(&format!("2026/07/{A}")), Some(&format!("2026/07/{B}")));
        fixture.write(
            &format!("{A}/run.py"),
            &format!("input = Path(__file__).parents[1] / VARIABLE / '{B}'\n"),
        );
        assert!(fixture.error().contains("cross-task path dependencies"));
    }

    #[test]
    fn historical_file_roles_are_respected_but_execution_invalidates_the_exemption() {
        let fixture = Fixture::new(Some(&format!("2026/07/{A}")), Some(&format!("2026/08/{B}")));
        let record = PathBuf::from(format!("{A}/history.log"));
        fixture.write(
            record.to_str().unwrap(),
            &format!("old input {B}/input.tsv\n"),
        );
        validate(
            &fixture.root,
            &fixture.tasks,
            &[],
            std::slice::from_ref(&record),
        )
        .unwrap();
        fixture.write(
            &format!("{A}/run.py"),
            "exec(Path('history.log').read_text())\n",
        );
        let error = validate(&fixture.root, &fixture.tasks, &[], &[record])
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("acknowledged historical record can be selected"),
            "{error}"
        );
    }

    #[test]
    fn ignored_hooks_and_scripts_are_checked_without_scanning_root_build_outputs() {
        let fixture = Fixture::new(None, Some(&format!("2026/08/{B}")));
        fixture.write("Cargo.toml", "[package]\nname = 'fixture'\n");
        fixture.write(
            "target/debug/deps/compiled-data",
            &format!("{B}/input.tsv\n"),
        );
        fixture.write(
            ".workspace-mgr/local/transaction.json",
            &format!("{B}/input.tsv\n"),
        );
        fixture.write(".git/objects/opaque", &format!("{B}/input.tsv\n"));
        assert!(fixture.scan().references.is_empty());
        fixture.write(
            &format!("{A}/.cache/run.txt"),
            &format!("cat ../{B}/input.tsv\n"),
        );
        fixture.write(
            &format!("{C}/.git/hooks/pre-commit"),
            &format!("python {B}/run.py\n"),
        );
        assert_eq!(fixture.scan().references.len(), 2);
    }

    #[test]
    fn cargo_exclusions_do_not_hide_build_scripts_inside_a_known_task() {
        let mut fixture = Fixture::new(None, Some(&format!("2026/08/{B}")));
        let task_root = format!("target/debug/{A}");
        fixture.tasks[0] = TaskLocation {
            source: task_root.clone().into(),
            destination: task_root.clone().into(),
        };
        fixture.write("Cargo.toml", "[package]\nname = 'fixture'\n");
        fixture.write(
            "target/debug/deps/compiled-data",
            &format!("{B}/input.tsv\n"),
        );
        let script = format!("{task_root}/build/run.py");
        fixture.write(&script, &format!("input_path = '{B}/input.tsv'\n"));
        let report = fixture.scan();
        assert_eq!(report.references.len(), 1, "{report:?}");
        assert_eq!(report.references[0].file, PathBuf::from(script));
        assert_eq!(report.references[0].target, PathBuf::from(B));
    }

    #[test]
    fn derived_and_multiline_execution_cannot_bypass_a_historical_file_role() {
        let fixture = Fixture::new(Some(&format!("2026/07/{A}")), None);
        let record = PathBuf::from(format!("{A}/runner.log"));
        fixture.write(record.to_str().unwrap(), "print('historical payload')\n");
        for script in [
            "exec(Path(__file__).with_suffix('.log').read_text())\n",
            "script = Path(__file__).parent / 'runner.log'\nexec(script.read_text())\n",
        ] {
            fixture.write(&format!("{A}/runner.py"), script);
            assert!(
                validate(
                    &fixture.root,
                    &fixture.tasks,
                    &[],
                    std::slice::from_ref(&record)
                )
                .unwrap_err()
                .to_string()
                .contains("acknowledged historical record can be selected")
            );
        }
        fixture.write(&format!("{A}/runner.py"), "import json\nprint(json.loads(Path('runner.log').read_text()))\nwith open('runner.log') as record:\n    print(record.read())\n");
        validate(&fixture.root, &fixture.tasks, &[], &[record]).unwrap();
    }

    #[test]
    fn comments_and_other_literals_do_not_hide_an_outward_dynamic_expression() {
        let fixture = Fixture::new(Some(&format!("2026/07/{A}")), Some(&format!("2026/07/{B}")));
        for script in [
            format!("input = Path(__file__).parents[1] / TARGET # historical task {A}\n"),
            format!(
                "a = Path(__file__).parents[1] / TARGET; b = Path(__file__).parents[1] / '{B}'\n"
            ),
            "input = Path('../') / TARGET\n".to_owned(),
            "input = Path('../' + TARGET)\n".to_owned(),
            "VARIABLE = '../' + TARGET\ninput = Path(__file__).parent / VARIABLE\n".to_owned(),
        ] {
            fixture.write(&format!("{A}/run.py"), &script);
            assert!(
                fixture.error().contains("outward file-relative dependency"),
                "{script}"
            );
        }
    }

    #[test]
    fn root_dynamic_selectors_and_build_scripts_cannot_hide_dependencies() {
        let fixture = Fixture::new(None, Some(&format!("2026/08/{B}")));
        fixture.write(
            "run.py",
            "input = Path(__file__).parent / os.environ['TASK_DIR'] / 'input.tsv'\n",
        );
        fixture.write("build/run.py", &format!("input = '{B}/input.tsv'\n"));
        let report = fixture.scan();
        assert_eq!(report.references.len(), 2, "{report:?}");
    }

    #[test]
    fn task_targets_inside_unterminated_large_quoted_spans_are_not_dropped() {
        let fixture = Fixture::new(None, Some(&format!("2026/08/{B}")));
        fixture.write(
            "README.md",
            &format!(
                "\"old input {B}/data {}\"\n",
                " ".repeat(CHUNK_BYTES as usize + 1024)
            ),
        );
        assert!(!fixture.scan().references.is_empty());
    }

    #[test]
    fn unsupported_alias_reassignments_cannot_prove_a_preserved_edge() {
        let fixture = Fixture::new(Some(&format!("2026/07/{A}")), Some(&format!("2026/07/{B}")));
        fixture.write(&format!("{A}/run.py"), &format!("ROOT = Path(__file__).parents[1]\nif True: ROOT = Path('{}')\ninput = ROOT / '{B}/input.tsv'\n", fixture.root.display()));
        assert!(!fixture.scan().references.is_empty());
        fixture.write(
            &format!("{A}/run.py"),
            &format!("ROOT = Path(__file__).parents[1]\ninput = XROOT / '{B}/input.tsv'\n"),
        );
        assert!(!fixture.scan().references.is_empty());
    }

    #[test]
    fn streaming_boundaries_preserve_component_lookahead_and_report_limits() {
        let fixture = Fixture::new(None, Some(&format!("2026/08/{B}")));
        let prefix = " ".repeat(CHUNK_BYTES as usize - B.len());
        fixture.write("README.md", &format!("{prefix}{B}-backup/input.tsv\n"));
        assert!(fixture.scan().references.is_empty());
        fixture.write("README.md", &format!("{prefix}{B}/input.tsv"));
        assert_eq!(fixture.scan().references.len(), 1);
        fixture.write(
            "README.md",
            &format!("{B}/input.tsv\n").repeat(MAX_REFERENCES + 10),
        );
        let report = fixture.scan();
        assert_eq!(report.references.len(), MAX_REFERENCES);
        assert!(report.truncated);
    }
}
