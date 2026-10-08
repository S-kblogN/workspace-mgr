//! Local task discovery independent of archive layout and hosting state.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use walkdir::WalkDir;

use crate::cli::{TaskListArgs, TaskPathArgs, TaskShowArgs};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::git::GitRepo;
use crate::manifest::{ResolvedTask, TaskKind, parse_task_identity};
use crate::output::{Format, print_human, print_json};
use crate::path::{reject_symlink_traversal, relative_to, repo_path};
use crate::policy::TASK_MANIFEST_NAME;

const METADATA_LIMIT: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Placement {
    TopLevel,
    Nested,
    Repository,
}

impl Placement {
    fn label(self) -> &'static str {
        match self {
            Self::TopLevel => "top-level",
            Self::Nested => "nested",
            Self::Repository => "repository",
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct CatalogTask {
    id: Option<String>,
    name: String,
    slug: Option<String>,
    kind: TaskKind,
    metadata: &'static str,
    placement: Placement,
    path: Option<String>,
    manifest: Option<PathBuf>,
    branch: Option<String>,
    title: Option<String>,
    purpose: Option<String>,
    scopes: Vec<String>,
    archive_status: Option<String>,
    diagnostic: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CatalogWarning {
    path: PathBuf,
    message: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct Catalog {
    repo: PathBuf,
    tasks: Vec<CatalogTask>,
    warnings: Vec<CatalogWarning>,
}

/// The identity and current ownership boundaries used by a read-only doctor
/// inspection. Invalid rows remain visible during a repository-wide audit.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct DoctorTask {
    pub(crate) id: Option<String>,
    pub(crate) name: String,
    pub(crate) kind: TaskKind,
    pub(crate) path: Option<String>,
    pub(crate) manifest: Option<PathBuf>,
    pub(crate) scopes: Vec<String>,
    pub(crate) diagnostic: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct DoctorWarning {
    pub(crate) path: PathBuf,
    pub(crate) message: String,
}

#[derive(Debug)]
pub(crate) struct DoctorSelection {
    pub(crate) tasks: Vec<DoctorTask>,
    pub(crate) warnings: Vec<DoctorWarning>,
}

pub(crate) fn select_for_doctor(start: &Path, selector: Option<&str>) -> Result<DoctorSelection> {
    // Doctor diagnoses a newer required CLI rather than refusing the audit.
    let catalog = discover_with_cli_requirement(start, false)?;
    let selected = match selector {
        Some(selector) => vec![catalog.resolve(selector)?],
        None => catalog.tasks.iter().collect(),
    };
    let warnings = catalog
        .warnings
        .iter()
        .filter(|warning| {
            selector.is_none()
                || selected.iter().any(|task| {
                    task.manifest.as_ref() == Some(&warning.path)
                        || task
                            .scopes
                            .iter()
                            .any(|scope| warning.path.starts_with(catalog.repo.join(scope)))
                })
        })
        .map(|warning| DoctorWarning {
            path: warning.path.clone(),
            message: warning.message.clone(),
        })
        .collect();
    let tasks = selected
        .into_iter()
        .map(|task| DoctorTask {
            id: task.id.clone(),
            name: task.name.clone(),
            kind: task.kind,
            path: task.path.clone(),
            manifest: task.manifest.clone(),
            scopes: task.scopes.clone(),
            diagnostic: task.diagnostic.clone(),
        })
        .collect();
    Ok(DoctorSelection { tasks, warnings })
}

pub(crate) fn list(args: &TaskListArgs, format: Format) -> Result<()> {
    let mut catalog = discover(&args.repo)?;
    let query = args.query.as_ref().map(|value| value.to_lowercase());
    catalog.tasks.retain(|task| {
        args.kind.is_none_or(|kind| task.kind == kind)
            && args
                .placement
                .is_none_or(|placement| task.placement == placement)
            && query.as_ref().is_none_or(|query| {
                [
                    task.id.as_deref(),
                    Some(task.name.as_str()),
                    task.slug.as_deref(),
                    task.title.as_deref(),
                    task.path.as_deref(),
                ]
                .into_iter()
                .flatten()
                .any(|value| value.to_lowercase().contains(query))
            })
    });
    if args.paths {
        let mut paths = Vec::new();
        for task in &catalog.tasks {
            if task.kind == TaskKind::Deliverable {
                require_valid(task)?;
                let path = task.path.as_ref().ok_or_else(|| {
                    Error::message(format!(
                        "cannot verify the current path of task {}",
                        task.name
                    ))
                })?;
                paths.push(path);
            }
        }
        print_warnings(&catalog.warnings);
        if format == Format::Json {
            return print_json(&paths);
        }
        for path in paths {
            println!("{path}");
        }
        return Ok(());
    }
    if format == Format::Json {
        return print_json(&catalog);
    }
    if catalog.tasks.is_empty() {
        println!("No tasks found.");
    } else {
        println!(
            "{:<14} {:<8} {:<10} {:<44} PATH / TITLE",
            "KIND", "METADATA", "PLACEMENT", "NAME"
        );
        for task in &catalog.tasks {
            let kind = match task.kind {
                TaskKind::Deliverable => "deliverable",
                TaskKind::Infrastructure => "infrastructure",
            };
            println!(
                "{kind:<14} {:<8} {:<10} {:<44} {}{}",
                task.metadata,
                task.placement.label(),
                cell(&task.name),
                task.path.as_deref().unwrap_or("(no task directory)"),
                task.title
                    .as_ref()
                    .map(|title| format!("  {}", cell(title)))
                    .unwrap_or_default()
            );
        }
    }
    print_warnings(&catalog.warnings);
    Ok(())
}

pub(crate) fn path(args: &TaskPathArgs, format: Format) -> Result<()> {
    let catalog = discover(&args.repo)?;
    let task = catalog.resolve(&args.selector)?;
    let relative = task.path.as_deref().ok_or_else(|| {
        Error::message(format!(
            "infrastructure task {} has no task directory; use task show {} to locate its manifest",
            task.name, task.name
        ))
    })?;
    let path = if args.relative {
        PathBuf::from(relative)
    } else {
        catalog.repo.join(relative)
    };
    if format == Format::Json {
        return print_json(&serde_json::json!({"repo":catalog.repo,"id":task.id,"path":path}));
    }
    println!("{}", path.display());
    Ok(())
}

pub(crate) fn show(args: &TaskShowArgs, format: Format) -> Result<()> {
    let catalog = discover(&args.repo)?;
    let task = catalog.resolve(&args.selector)?;
    let report = serde_json::json!({"repo":catalog.repo,"task":task});
    match format {
        Format::Json => print_json(&report),
        Format::Human => print_human(&report),
    }
}

impl Catalog {
    fn resolve(&self, selector: &str) -> Result<&CatalogTask> {
        let selector = selector.trim();
        if selector.is_empty() || selector.chars().any(char::is_control) {
            return Err(Error::message(
                "task selector must be a nonempty single-line ID, name, slug, or path",
            ));
        }
        let candidate = Path::new(selector);
        let relative = if candidate.is_absolute() {
            candidate
                .canonicalize()
                .ok()
                .and_then(|path| relative_to(&path, &self.repo, "task selector").ok())
        } else {
            repo_path(selector, "task selector").ok()
        };
        let matches = self
            .tasks
            .iter()
            .filter(|task| {
                task.id.as_deref() == Some(selector)
                    || task.name == selector
                    || task.slug.as_deref() == Some(selector)
                    || task
                        .path
                        .as_ref()
                        .is_some_and(|path| relative.as_ref() == Some(path))
            })
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => Err(Error::message(format!(
                "no current task matches {selector:?}; use task list to search names and titles"
            ))),
            [task] => {
                require_valid(task)?;
                Ok(task)
            }
            _ => {
                let candidates = matches
                    .iter()
                    .map(|task| {
                        format!(
                            "  {}: {} (id={}, manifest={})",
                            task.name,
                            task.path
                                .as_deref()
                                .unwrap_or("infrastructure; no task directory"),
                            task.id.as_deref().unwrap_or("invalid"),
                            task.manifest
                                .as_ref()
                                .map(|path| path.display().to_string())
                                .unwrap_or_else(|| "none (legacy candidate)".into())
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Err(Error::message(format!(
                    "ambiguous task selector {selector:?}; choose an exact current directory path or unique ID:\n{candidates}"
                )))
            }
        }
    }

    fn warning(&mut self, path: &Path, message: impl ToString) {
        self.warnings.push(CatalogWarning {
            path: path.to_path_buf(),
            message: message.to_string(),
        });
    }

    fn add(
        &mut self,
        repo: &GitRepo,
        config: &Config,
        directory: Option<&Path>,
        manifest: &Path,
        state: &Path,
    ) {
        let relative =
            directory.and_then(|path| relative_to(path, &repo.root, "task directory").ok());
        let name = directory
            .or_else(|| manifest.parent())
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .unwrap_or("(invalid name)")
            .to_owned();
        let kind = if directory.is_some() {
            TaskKind::Deliverable
        } else {
            TaskKind::Infrastructure
        };
        let placement = match &relative {
            Some(path) if path.contains('/') => Placement::Nested,
            Some(_) => Placement::TopLevel,
            None => Placement::Repository,
        };
        let loaded = metadata_file(manifest)
            .and_then(|()| {
                // WalkDir never follows links, but a manifest itself can be a link.
                let (root, relative) = if let Some(directory) = directory {
                    (directory, TASK_MANIFEST_NAME.to_owned())
                } else {
                    (state, relative_to(manifest, state, "private manifest")?)
                };
                reject_symlink_traversal(root, &relative, "task catalog manifest")?;
                ResolvedTask::load_read_only(repo, config, manifest, state)
            })
            .and_then(|task| {
                if task.kind != kind {
                    return Err(Error::message(
                        "task manifest kind does not match its current location",
                    ));
                }
                Ok(task)
            });
        let mut task = CatalogTask {
            id: None,
            name,
            slug: None,
            kind,
            metadata: "invalid",
            placement,
            path: relative,
            manifest: Some(manifest.to_path_buf()),
            branch: None,
            title: None,
            purpose: None,
            scopes: Vec::new(),
            archive_status: None,
            diagnostic: None,
        };
        match loaded {
            Ok(resolved) => {
                task.scopes = resolved.scopes();
                if kind == TaskKind::Infrastructure {
                    task.name = resolved.task_id.clone();
                }
                task.id = Some(resolved.task_id);
                task.slug = Some(resolved.slug);
                task.branch = Some(resolved.branch);
                task.title = Some(resolved.title);
                task.purpose = Some(resolved.purpose);
                task.metadata = "managed";
                if let Some(directory) = directory {
                    task.archive_status = self.receipt_status(directory);
                }
            }
            Err(error) => {
                task.diagnostic = Some(error.to_string());
                self.warning(manifest, error);
            }
        }
        self.tasks.push(task);
    }

    fn receipt_status(&mut self, directory: &Path) -> Option<String> {
        let path = directory.join(crate::archive_migration::RECEIPT_NAME);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => {
                self.warning(&path, error);
                return None;
            }
            Ok(_) => {}
        }
        let result = metadata_file(&path).and_then(|()| {
            let raw = fs::read_to_string(&path).map_err(|source| Error::Io {
                path: path.clone(),
                source,
            })?;
            #[derive(serde::Deserialize)]
            struct ReceiptStatus {
                status: String,
            }
            serde_json::from_str::<ReceiptStatus>(&raw)
                .map(|receipt| receipt.status)
                .map_err(|error| {
                    Error::message(format!("cannot inspect local archive receipt: {error}"))
                })
        });
        match result {
            Ok(status) => Some(status),
            Err(error) => {
                self.warning(&path, error);
                None
            }
        }
    }
}

pub(crate) fn discover(start: &Path) -> Result<Catalog> {
    discover_with_cli_requirement(start, true)
}

fn discover_with_cli_requirement(start: &Path, enforce_cli_requirement: bool) -> Result<Catalog> {
    let repo = GitRepo::discover(start)?;
    let config = if Config::path(&repo).exists() {
        if enforce_cli_requirement {
            Config::load_compatible(&repo)?
        } else {
            Config::load_compatible_ignoring_cli_requirement(&repo)?
        }
    } else {
        Config::default()
    };
    let mut catalog = Catalog {
        repo: repo.root.clone(),
        tasks: Vec::new(),
        warnings: Vec::new(),
    };
    let mut walker = WalkDir::new(&repo.root).follow_links(false).into_iter();
    while let Some(entry) = walker.next() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                let path = error.path().unwrap_or(&repo.root).to_path_buf();
                catalog.warning(&path, error);
                continue;
            }
        };
        if !entry.file_type().is_dir() || entry.path() == repo.root {
            continue;
        }
        let Some(name) = entry.file_name().to_str() else {
            catalog.warning(
                entry.path(),
                "non-UTF-8 directory name cannot be listed as a task",
            );
            walker.skip_current_dir();
            continue;
        };
        if matches!(name, ".git" | ".dvc" | ".workspace-mgr") {
            walker.skip_current_dir();
            continue;
        }
        let manifest = entry.path().join(TASK_MANIFEST_NAME);
        let identity = parse_task_identity(TaskKind::Deliverable, name);
        // Only absence can create a legacy candidate. Permission and I/O
        // failures retain an invalid managed row instead of inventing identity.
        if !matches!(metadata_present(&manifest), Ok(false)) {
            catalog.add(&repo, &config, Some(entry.path()), &manifest, &repo.root);
            walker.skip_current_dir();
        } else if fs::symlink_metadata(entry.path().join(".git")).is_ok_and(|metadata| {
            metadata.is_dir() || metadata.file_type().is_symlink() || metadata.len() > 0
        }) {
            // Explicit task metadata identifies a task root above. A name
            // alone cannot claim an unrelated nested checkout as a legacy task.
            // Empty cache markers do not create a Git repository boundary.
            walker.skip_current_dir();
        } else if let Ok(identity) = identity {
            let relative = relative_to(entry.path(), &repo.root, "legacy task directory")?;
            let placement = if relative.contains('/') {
                Placement::Nested
            } else {
                Placement::TopLevel
            };
            catalog.tasks.push(CatalogTask {
                id: Some(name.to_owned()),
                name: name.to_owned(),
                slug: Some(identity.original_slug),
                kind: TaskKind::Deliverable,
                metadata: "legacy",
                placement,
                scopes: vec![relative.clone()],
                path: Some(relative),
                manifest: None,
                branch: None,
                title: None,
                purpose: None,
                archive_status: None,
                diagnostic: None,
            });
            walker.skip_current_dir();
        }
    }
    let mut private_roots = BTreeSet::new();
    match crate::local_state::directory_unmigrated(&repo) {
        Ok(root) => {
            private_roots.insert(root);
        }
        Err(error) => catalog.warning(&repo.root.join(crate::local_state::LOCAL_STATE_PATH), error),
    }
    let common = repo
        .common_dir()?
        .canonicalize()
        .map_err(|source| Error::Io {
            path: repo.root.clone(),
            source,
        })?;
    private_roots.insert(common.join("workspace-mgr"));
    let worktrees = common.join("worktrees");
    if worktrees.is_dir() {
        for entry in fs::read_dir(&worktrees).map_err(|source| Error::Io {
            path: worktrees.clone(),
            source,
        })? {
            match entry {
                Ok(entry) if entry.file_type().is_ok_and(|kind| kind.is_dir()) => {
                    private_roots.insert(entry.path().join("workspace-mgr"));
                }
                Ok(_) => {}
                Err(error) => catalog.warning(&worktrees, error),
            }
        }
    }
    for root in private_roots {
        scan_private(&mut catalog, &repo, &config, &root);
    }
    catalog.tasks.sort_by(|left, right| {
        left.id
            .as_deref()
            .unwrap_or(&left.name)
            .cmp(right.id.as_deref().unwrap_or(&right.name))
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.manifest.cmp(&right.manifest))
    });
    catalog.warnings.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.message.cmp(&right.message))
    });
    Ok(catalog)
}

fn scan_private(catalog: &mut Catalog, repo: &GitRepo, config: &Config, root: &Path) {
    match metadata_present(root) {
        Ok(false) => return,
        Ok(true) => {}
        Err(error) => {
            catalog.warning(root, error);
            return;
        }
    }
    let Some(parent) = root.parent() else {
        return;
    };
    if let Err(error) = reject_symlink_traversal(
        parent,
        root.file_name().unwrap().to_str().unwrap_or(""),
        "private task catalog",
    ) {
        catalog.warning(root, error);
        return;
    }
    let singleton = root.join("task.toml");
    if !matches!(metadata_present(&singleton), Ok(false)) {
        catalog.add(repo, config, None, &singleton, root);
    }
    let directory = root.join("infrastructure-tasks");
    match metadata_present(&directory) {
        Ok(false) => return,
        Ok(true) => {}
        Err(error) => {
            catalog.warning(&directory, error);
            return;
        }
    }
    if let Err(error) =
        reject_symlink_traversal(root, "infrastructure-tasks", "private task catalog")
    {
        catalog.warning(&directory, error);
        return;
    }
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) => {
            catalog.warning(&directory, error);
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                catalog.warning(&directory, error);
                continue;
            }
        };
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let manifest = entry
            .path()
            .join(crate::manifest::INFRASTRUCTURE_TASK_MANIFEST_FILE);
        if !matches!(metadata_present(&manifest), Ok(false)) {
            catalog.add(repo, config, None, &manifest, root);
        }
    }
}

fn metadata_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > METADATA_LIMIT {
        return Err(Error::message(format!(
            "task query metadata must be a regular file of at most 16 MiB: {}",
            path.display()
        )));
    }
    Ok(())
}

fn metadata_present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(Error::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn require_valid(task: &CatalogTask) -> Result<()> {
    if let Some(diagnostic) = &task.diagnostic {
        return Err(Error::message(format!(
            "cannot resolve task {} with invalid current metadata: {diagnostic}",
            task.name
        )));
    }
    Ok(())
}

fn print_warnings(warnings: &[CatalogWarning]) {
    for warning in warnings {
        eprintln!(
            "workspace-mgr: {}: {}",
            warning.path.display(),
            warning.message
        );
    }
}

fn cell(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}
