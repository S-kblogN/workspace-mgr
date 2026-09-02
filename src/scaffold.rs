use std::fs;
use std::path::{Path, PathBuf};

use chrono::Local;
use serde::Serialize;

use crate::config::{CONFIG_NAME, Config, S3Config};
use crate::dvc;
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::instructions::BOOTSTRAP;
use crate::lock::RepositoryLock;
use crate::manifest::{
    AdditionalScope, INFRASTRUCTURE_MANIFEST_NAME, TASK_SCHEMA_VERSION, TaskKind, TaskManifest,
    build_task_branch, build_task_id, one_line, validate_additional_scopes,
};
use crate::path::{reject_symlink_traversal, repo_path};
use crate::policy::{
    REVIEW_DELIVERABLE_CREATION_TIMING, REVIEW_INFRASTRUCTURE_CREATION_TIMING,
    REVIEW_INITIAL_STATE, REVIEW_MANAGED_BY, REVIEW_MERGE_AUTHORITY, REVIEW_PULL_REQUEST,
    REVIEW_SYNC_CADENCE, TASK_MANIFEST_NAME,
};
use crate::s3_purge;

const STORAGE_GITIGNORE: &str = "/config.local\n/tmp\n/cache\n";
const STORAGE_IGNORE: &str =
    "# Managed by workspace-mgr. Storage paths are selected through workspace-mgr.\n";

#[derive(Debug, Clone)]
pub struct InitOptions {
    pub repo: PathBuf,
    pub s3_url: Option<String>,
    pub s3_endpoint_url: Option<String>,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct InitReport {
    pub status: String,
    pub repo: String,
    pub actions: Vec<InitAction>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InitAction {
    pub action: String,
    pub path: String,
    pub detail: String,
}

pub fn validate_owned_files(repo: &GitRepo, config: &Config) -> Result<()> {
    let mut drifted = Vec::new();
    check_owned_file(repo, "AGENTS.md", BOOTSTRAP.as_bytes(), &mut drifted)?;
    if config.s3_enabled() {
        let internal = dvc::render_internal_config(config)?.ok_or_else(|| {
            Error::message("managed S3 configuration disappeared during scaffold validation")
        })?;
        check_owned_file(repo, ".dvc/config", internal.as_bytes(), &mut drifted)?;
        check_owned_file(
            repo,
            ".dvc/.gitignore",
            STORAGE_GITIGNORE.as_bytes(),
            &mut drifted,
        )?;
        check_owned_file(repo, ".dvcignore", STORAGE_IGNORE.as_bytes(), &mut drifted)?;
        let attributes_path = repo.root.join(".gitattributes");
        reject_symlink_traversal(
            repo.root.as_path(),
            ".gitattributes",
            "managed scaffold path",
        )?;
        let attributes = match fs::read_to_string(&attributes_path) {
            Ok(attributes) => attributes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(source) => {
                return Err(Error::Io {
                    path: attributes_path,
                    source,
                });
            }
        };
        if !attributes
            .lines()
            .any(|line| line.trim() == "*.dvc whitespace=-blank-at-eol")
        {
            drifted.push(".gitattributes");
        }
    } else {
        if repo.root.join(".dvc/config").exists() {
            drifted.push(".dvc/config");
        }
        if repo.root.join(".dvc").is_dir() {
            check_owned_file(
                repo,
                ".dvc/.gitignore",
                STORAGE_GITIGNORE.as_bytes(),
                &mut drifted,
            )?;
            check_owned_file(repo, ".dvcignore", STORAGE_IGNORE.as_bytes(), &mut drifted)?;
        } else if repo.root.join(".dvcignore").exists() {
            drifted.push(".dvcignore");
        }
    }
    if drifted.is_empty() {
        Ok(())
    } else {
        Err(Error::message(format!(
            "product-owned scaffold is out of date: {}; run `workspace-mgr init` to reconcile it",
            drifted.join(", ")
        )))
    }
}

fn check_owned_file<'a>(
    repo: &GitRepo,
    relative: &'a str,
    expected: &[u8],
    drifted: &mut Vec<&'a str>,
) -> Result<()> {
    reject_symlink_traversal(&repo.root, relative, "managed scaffold path")?;
    let path = repo.root.join(relative);
    match fs::read(&path) {
        Ok(actual) if actual == expected => {}
        Ok(_) => drifted.push(relative),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => drifted.push(relative),
        Err(source) => return Err(Error::Io { path, source }),
    }
    Ok(())
}

pub fn init(options: &InitOptions) -> Result<InitReport> {
    let repo = GitRepo::discover(&options.repo)?;
    let _repository_lock = if options.dry_run {
        None
    } else {
        Some(RepositoryLock::acquire(&repo)?)
    };
    for path in [
        CONFIG_NAME,
        "AGENTS.md",
        ".dvc",
        ".dvc/config",
        ".dvc/.gitignore",
        ".dvcignore",
        ".gitattributes",
    ] {
        reject_symlink_traversal(&repo.root, path, "managed scaffold path")?;
    }
    let config_path = repo.root.join(CONFIG_NAME);
    let agents_path = repo.root.join("AGENTS.md");
    let mut actions = Vec::new();
    let existing_config = config_path.is_file();
    if !existing_config {
        reject_first_init_collisions(&repo)?;
    }
    let mut config = if existing_config {
        Config::load_compatible(&repo)?
    } else {
        let mut config = Config::default();
        detect_git_defaults(&repo, &mut config)?;
        config
    };
    let previous_s3 = config.s3.clone();
    if let Some(url) = &options.s3_url {
        config.s3 = Some(S3Config {
            url: url.clone(),
            endpoint_url: options.s3_endpoint_url.clone(),
        });
    }
    if previous_s3 != config.s3 && s3_purge::has_pending(&repo)? {
        return Err(Error::message(
            "cannot change the managed S3 location while permanent deletions remain pending; run `workspace-mgr refresh` or `workspace-mgr publish` first",
        ));
    }
    config.validate()?;
    repo.validate_remote_name(&config.git.remote)?;

    let rendered = config.render()?;
    let config_changed = fs::read_to_string(&config_path).ok().as_deref() != Some(&rendered);
    if config_changed {
        actions.push(InitAction {
            action: if existing_config { "update" } else { "create" }.to_owned(),
            path: CONFIG_NAME.to_owned(),
            detail: "repository Git and S3 facts".to_owned(),
        });
    }

    let bootstrap_changed = fs::read_to_string(&agents_path).ok().as_deref() != Some(BOOTSTRAP);
    if bootstrap_changed {
        actions.push(InitAction {
            action: if agents_path.is_file() {
                "update"
            } else {
                "create"
            }
            .to_owned(),
            path: "AGENTS.md".to_owned(),
            detail: "current workspace-mgr bootstrap".to_owned(),
        });
    }

    let mut initialize_storage_engine = false;
    let mut attributes_changed = false;
    let mut storage_gitignore_changed = false;
    let mut storage_ignore_changed = false;
    let mut remove_storage_ignore = false;
    let mut remove_storage_config = false;
    if config.s3_enabled() {
        let pointers = dvc::repository_pointers(&repo)?;
        if !pointers.is_empty() {
            let configured = established_s3_location(&repo)?;
            let requested = config.s3.as_ref().ok_or_else(|| {
                Error::message("managed S3 configuration disappeared during initialization")
            })?;
            if configured != (requested.url.clone(), requested.endpoint_url.clone()) {
                return Err(Error::message(format!(
                    "cannot change the managed S3 location while storage boundaries remain: {}; place them in Git first",
                    pointers.join(", ")
                )));
            }
        }
        dvc::require_runtime(&repo)?;
        if config.requires_object_versioning() {
            dvc::require_version_adapter(&repo)?;
        }
        if !repo.root.join(".dvc").exists() {
            initialize_storage_engine = true;
            actions.push(InitAction {
                action: "run".to_owned(),
                path: ".dvc/".to_owned(),
                detail: "initialize the internal managed-storage engine".to_owned(),
            });
        }
        if let Some(rendered) = dvc::render_internal_config(&config)? {
            let internal_path = repo.root.join(".dvc/config");
            if fs::read_to_string(&internal_path).ok().as_deref() != Some(&rendered) {
                let versioning = if config.requires_object_versioning() {
                    " with exact object-version verification"
                } else {
                    ""
                };
                actions.push(InitAction {
                    action: "configure".to_owned(),
                    path: ".dvc/config".to_owned(),
                    detail: format!("generate internal managed-storage configuration{versioning}"),
                });
            }
        }
        let attributes_path = repo.root.join(".gitattributes");
        let attributes = fs::read_to_string(&attributes_path).unwrap_or_default();
        attributes_changed = !attributes
            .lines()
            .any(|line| line.trim() == "*.dvc whitespace=-blank-at-eol");
        if attributes_changed {
            actions.push(InitAction {
                action: "configure".to_owned(),
                path: ".gitattributes".to_owned(),
                detail: "allow generated version-aware storage metadata".to_owned(),
            });
        }
    } else if dvc::internal_config_exists(&repo)? {
        let pointers = dvc::repository_pointers(&repo)?;
        if !pointers.is_empty() {
            return Err(Error::message(format!(
                "cannot disable managed S3 while storage boundaries remain: {}; move or reset them to Git first",
                pointers.join(", ")
            )));
        }
        actions.push(InitAction {
            action: "remove".to_owned(),
            path: ".dvc/config".to_owned(),
            detail: "remove disabled workspace-mgr storage configuration".to_owned(),
        });
        remove_storage_config = true;
    }
    let storage_directory_retained = config.s3_enabled() || repo.root.join(".dvc").is_dir();
    if storage_directory_retained {
        let storage_gitignore_path = repo.root.join(".dvc/.gitignore");
        storage_gitignore_changed =
            fs::read_to_string(&storage_gitignore_path).ok().as_deref() != Some(STORAGE_GITIGNORE);
        if storage_gitignore_changed {
            actions.push(InitAction {
                action: "configure".to_owned(),
                path: ".dvc/.gitignore".to_owned(),
                detail: "current private storage ignore rules".to_owned(),
            });
        }
        let storage_ignore_path = repo.root.join(".dvcignore");
        storage_ignore_changed =
            fs::read_to_string(&storage_ignore_path).ok().as_deref() != Some(STORAGE_IGNORE);
        if storage_ignore_changed {
            actions.push(InitAction {
                action: "configure".to_owned(),
                path: ".dvcignore".to_owned(),
                detail: "current private storage path-selection policy".to_owned(),
            });
        }
    } else if repo.root.join(".dvcignore").is_file() {
        actions.push(InitAction {
            action: "remove".to_owned(),
            path: ".dvcignore".to_owned(),
            detail: "remove unused private storage path-selection policy".to_owned(),
        });
        remove_storage_ignore = true;
    }

    if !options.dry_run {
        let snapshot = ScaffoldSnapshot::capture(&repo)?;
        let applied: Result<()> = (|| {
            if config_changed {
                atomic_write(&config_path, &rendered)?;
            }
            if bootstrap_changed {
                atomic_write(&agents_path, BOOTSTRAP)?;
            }
            if config.s3_enabled() {
                if initialize_storage_engine {
                    dvc::execute_engine(&repo.root, ["init"])?;
                }
                dvc::write_internal_config(&repo, &config)?;
                if attributes_changed {
                    let attributes_path = repo.root.join(".gitattributes");
                    ensure_line(
                        &attributes_path,
                        "# Version-aware storage metadata may contain a generated folded key line with trailing space.",
                    )?;
                    ensure_line(&attributes_path, "*.dvc whitespace=-blank-at-eol")?;
                }
            } else if remove_storage_config {
                dvc::remove_internal_config(&repo)?;
            }
            if storage_gitignore_changed {
                atomic_write(&repo.root.join(".dvc/.gitignore"), STORAGE_GITIGNORE)?;
            }
            if storage_ignore_changed {
                atomic_write(&repo.root.join(".dvcignore"), STORAGE_IGNORE)?;
            } else if remove_storage_ignore {
                fs::remove_file(repo.root.join(".dvcignore")).at(repo.root.join(".dvcignore"))?;
            }
            Ok(())
        })();
        if let Err(error) = applied {
            return match snapshot.restore(&repo) {
                Ok(()) => Err(Error::message(format!(
                    "repository initialization failed and was rolled back: {error}"
                ))),
                Err(rollback) => Err(Error::message(format!(
                    "repository initialization failed: {error}; rollback also failed: {rollback}"
                ))),
            };
        }
    }
    Ok(InitReport {
        status: if actions.is_empty() {
            "no_changes"
        } else if options.dry_run {
            "dry_run"
        } else {
            "initialized"
        }
        .to_owned(),
        repo: repo.root.display().to_string(),
        actions,
    })
}

fn detect_git_defaults(repo: &GitRepo, config: &mut Config) -> Result<()> {
    let remotes = repo.run(["remote"])?.stdout;
    let remote_names: Vec<&str> = remotes
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if remote_names.contains(&"origin") {
        config.git.remote = "origin".to_owned();
    } else if let Some(remote) = remote_names.first() {
        config.git.remote = (*remote).to_owned();
    }
    let remote_head = format!("refs/remotes/{}/HEAD", config.git.remote);
    let symbolic = repo.run_unchecked(["symbolic-ref", "--quiet", "--short", &remote_head])?;
    let branch = if symbolic.success() {
        symbolic
            .stdout
            .trim()
            .split_once('/')
            .map(|(_, branch)| branch.to_owned())
    } else {
        repo.current_branch()?
    };
    if let Some(branch) = branch {
        config.git.branch = branch;
    }
    Ok(())
}

fn reject_first_init_collisions(repo: &GitRepo) -> Result<()> {
    let mut collisions = Vec::new();
    for relative in [CONFIG_NAME, "AGENTS.md", ".dvc", ".dvcignore"] {
        let path = repo.root.join(relative);
        match fs::symlink_metadata(&path) {
            Ok(_) => collisions.push(relative),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(Error::Io { path, source }),
        }
    }
    if collisions.is_empty() {
        return Ok(());
    }
    Err(Error::message(format!(
        "cannot initialize because reserved workspace-mgr scaffold paths already exist: {}; move or remove those paths explicitly before first initialization",
        collisions.join(", ")
    )))
}

fn established_s3_location(repo: &GitRepo) -> Result<(String, Option<String>)> {
    let internal = dvc::internal_location(repo);
    if let Ok(Some(location)) = internal {
        return Ok(location);
    }
    if let Some(location) = committed_s3_location(repo)? {
        return Ok(location);
    }
    match internal {
        Err(error) => Err(Error::message(format!(
            "cannot verify the established S3 location while storage boundaries remain: {error}"
        ))),
        Ok(_) => Err(Error::message(
            "cannot assign an S3 location while storage boundaries already exist without a verifiable prior location",
        )),
    }
}

fn committed_s3_location(repo: &GitRepo) -> Result<Option<(String, Option<String>)>> {
    let Some(head) = repo.optional_oid("HEAD")? else {
        return Ok(None);
    };
    let object = format!("{head}:{CONFIG_NAME}");
    if !repo.run_unchecked(["cat-file", "-e", &object])?.success() {
        return Ok(None);
    }
    let raw = repo.run(["show", &object])?.stdout;
    let source = PathBuf::from(format!("HEAD:{CONFIG_NAME}"));
    let config = Config::parse(&raw, &source)?;
    Ok(config.s3.map(|s3| (s3.url, s3.endpoint_url)))
}

#[derive(Debug, Clone)]
pub struct TaskCreateOptions {
    pub repo: PathBuf,
    pub slug: String,
    pub title: String,
    pub purpose: String,
    pub kind: TaskKind,
    pub scopes: Vec<String>,
    pub scope_note: Option<String>,
    pub timestamp: Option<String>,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskCreateReport {
    pub status: String,
    pub kind: TaskKind,
    pub task_id: String,
    pub path: String,
    pub manifest: String,
    pub branch: String,
    pub base_oid: String,
    pub files: Vec<String>,
    pub review: TaskCreateReviewHandoff,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskCreateReviewHandoff {
    pub pull_request: &'static str,
    pub initial_state: &'static str,
    pub managed_by: &'static str,
    pub merge_authority: &'static str,
    pub creation_timing: &'static str,
    pub synchronization_cadence: &'static str,
}

pub fn create_task(options: &TaskCreateOptions) -> Result<TaskCreateReport> {
    let title = one_line(&options.title, "task title")?;
    let purpose = one_line(&options.purpose, "task purpose")?;
    let repo = GitRepo::discover(&options.repo)?;
    let _repository_lock = RepositoryLock::acquire(&repo)?;
    let config = Config::load_compatible(&repo)?;
    repo.validate_remote_name(&config.git.remote)?;
    let mut additional_scopes = create_scopes(options)?;
    let (task_id, task_dir) = match options.kind {
        TaskKind::Deliverable => {
            let timestamp = match &options.timestamp {
                Some(value) => value.clone(),
                None => Local::now().format("%Y%m%d-%H%M%S").to_string(),
            };
            let task_id = build_task_id(options.kind, &options.slug, Some(&timestamp))?;
            let task_dir = repo.root.join(&task_id);
            if task_dir.exists() {
                return Err(Error::message(format!(
                    "task directory already exists: {}",
                    task_dir.display()
                )));
            }
            (task_id, task_dir)
        }
        TaskKind::Infrastructure => {
            let task_id = build_task_id(options.kind, &options.slug, options.timestamp.as_deref())?;
            if additional_scopes.is_empty() {
                return Err(Error::message(
                    "infrastructure task creation requires --scope and --scope-note",
                ));
            }
            let checkout = repo
                .common_dir()?
                .join("workspace-mgr/checkouts")
                .join(&task_id);
            if checkout.exists() {
                return Err(Error::message(format!(
                    "infrastructure worktree already exists: {}",
                    checkout.display()
                )));
            }
            (task_id, checkout)
        }
    };
    additional_scopes = validate_additional_scopes(
        (options.kind == TaskKind::Deliverable).then_some(task_id.as_str()),
        additional_scopes,
    )?;
    let branch = build_task_branch(options.kind, &options.slug)?;
    repo.validate_branch(&branch)?;
    if repo
        .optional_oid(&format!("refs/heads/{branch}"))?
        .is_some()
        || repo
            .remote_branch_oid(&config.git.remote, &branch)?
            .is_some()
    {
        return Err(Error::message(format!(
            "task branch already exists: {branch}"
        )));
    }
    let base_oid = if options.dry_run {
        repo.remote_branch_oid(&config.git.remote, &config.git.branch)?
            .ok_or_else(|| {
                Error::message(format!(
                    "remote base branch does not exist: {}/{}",
                    config.git.remote, config.git.branch
                ))
            })?
    } else {
        repo.fetch_branch(&config.git.remote, &config.git.branch)?
    };
    let manifest = TaskManifest {
        schema_version: TASK_SCHEMA_VERSION,
        kind: options.kind,
        id: task_id.clone(),
        slug: options.slug.clone(),
        path: (options.kind == TaskKind::Deliverable).then(|| task_id.clone()),
        branch: branch.clone(),
        title: title.clone(),
        purpose: purpose.clone(),
        additional_scopes,
    };
    let readme = format!(
        "# {}\n\n{}\n\n## Directory map\n\n- `README.md` describes this task and its retained outputs.\n- `{}` declares the task scope and target branch.\n",
        title, purpose, TASK_MANIFEST_NAME
    );
    let mut manifest_path = match options.kind {
        TaskKind::Deliverable => task_dir.join(TASK_MANIFEST_NAME),
        TaskKind::Infrastructure => task_dir.join("<private-git-state>/task.toml"),
    };
    let files = match options.kind {
        TaskKind::Deliverable => vec![
            format!("{task_id}/README.md"),
            format!("{task_id}/{TASK_MANIFEST_NAME}"),
        ],
        TaskKind::Infrastructure => Vec::new(),
    };
    if !options.dry_run {
        repo.run([
            "update-ref",
            "-m",
            &format!("workspace-mgr task create {task_id}"),
            &format!("refs/heads/{branch}"),
            &base_oid,
            &"0".repeat(40),
        ])?;
        let created = match options.kind {
            TaskKind::Deliverable => {
                write_task_files(&task_dir, TASK_MANIFEST_NAME, &readme, &manifest)
            }
            TaskKind::Infrastructure => {
                create_infrastructure_worktree(&repo, &task_dir, &branch, &manifest)
            }
        };
        if let Err(error) = created {
            let rollback = repo.run_unchecked([
                "update-ref",
                "-d",
                &format!("refs/heads/{branch}"),
                &base_oid,
            ])?;
            return if rollback.success() {
                Err(error)
            } else {
                Err(Error::message(format!(
                    "task scaffolding failed: {error}; local branch rollback also failed: {}",
                    rollback.stderr.trim()
                )))
            };
        }
        if options.kind == TaskKind::Infrastructure {
            manifest_path = GitRepo::discover(&task_dir)?
                .git_dir()?
                .join(INFRASTRUCTURE_MANIFEST_NAME);
        }
    }
    Ok(TaskCreateReport {
        status: if options.dry_run {
            "dry_run"
        } else {
            "created"
        }
        .to_owned(),
        kind: options.kind,
        task_id,
        path: task_dir.display().to_string(),
        manifest: manifest_path.display().to_string(),
        branch,
        base_oid,
        files,
        review: TaskCreateReviewHandoff {
            pull_request: REVIEW_PULL_REQUEST,
            initial_state: REVIEW_INITIAL_STATE,
            managed_by: REVIEW_MANAGED_BY,
            merge_authority: REVIEW_MERGE_AUTHORITY,
            creation_timing: match options.kind {
                TaskKind::Deliverable => REVIEW_DELIVERABLE_CREATION_TIMING,
                TaskKind::Infrastructure => REVIEW_INFRASTRUCTURE_CREATION_TIMING,
            },
            synchronization_cadence: REVIEW_SYNC_CADENCE,
        },
    })
}

fn create_scopes(options: &TaskCreateOptions) -> Result<Vec<AdditionalScope>> {
    if options.scopes.is_empty() {
        if options.scope_note.is_some() {
            return Err(Error::message("--scope-note requires at least one --scope"));
        }
        return Ok(Vec::new());
    }
    let reason = one_line(
        options
            .scope_note
            .as_deref()
            .ok_or_else(|| Error::message("--scope requires --scope-note"))?,
        "scope note",
    )?;
    let mut paths = options
        .scopes
        .iter()
        .map(|path| repo_path(path, "task scope"))
        .collect::<Result<Vec<_>>>()?;
    paths.sort();
    paths.dedup();
    Ok(paths
        .into_iter()
        .map(|path| AdditionalScope {
            path,
            reason: reason.clone(),
        })
        .collect())
}

fn create_infrastructure_worktree(
    repo: &GitRepo,
    checkout: &Path,
    branch: &str,
    manifest: &TaskManifest,
) -> Result<()> {
    let parent = checkout
        .parent()
        .ok_or_else(|| Error::message("infrastructure worktree has no parent"))?;
    fs::create_dir_all(parent).at(parent)?;
    let added = repo.run_unchecked([
        "worktree",
        "add",
        "--quiet",
        &checkout.to_string_lossy(),
        branch,
    ])?;
    if !added.success() {
        return Err(Error::message(format!(
            "failed to create infrastructure worktree: {}",
            added.stderr.trim()
        )));
    }
    let result = (|| {
        let worktree = GitRepo::discover(checkout)?;
        dvc::link_private_worktree_state(repo, &worktree)?;
        let path = worktree.git_dir()?.join(INFRASTRUCTURE_MANIFEST_NAME);
        atomic_write(&path, &manifest.render()?)
    })();
    if let Err(error) = result {
        let cleanup =
            repo.run_unchecked(["worktree", "remove", "--force", &checkout.to_string_lossy()])?;
        return if cleanup.success() {
            Err(error)
        } else {
            Err(Error::message(format!(
                "infrastructure task creation failed: {error}; worktree cleanup also failed: {}",
                cleanup.stderr.trim()
            )))
        };
    }
    Ok(())
}

fn write_task_files(
    task_dir: &Path,
    manifest_name: &str,
    readme: &str,
    manifest: &TaskManifest,
) -> Result<()> {
    fs::create_dir(task_dir).at(task_dir)?;
    let result = (|| {
        atomic_write(&task_dir.join("README.md"), readme)?;
        atomic_write(&task_dir.join(manifest_name), &manifest.render()?)?;
        Ok(())
    })();
    if let Err(error) = result {
        return match fs::remove_dir_all(task_dir) {
            Ok(()) => Err(error),
            Err(rollback) => Err(Error::message(format!(
                "task file creation failed: {error}; directory rollback also failed: {rollback}"
            ))),
        };
    }
    Ok(())
}

fn ensure_line(path: &Path, line: &str) -> Result<()> {
    let mut content = fs::read_to_string(path).unwrap_or_default();
    if content.lines().any(|existing| existing.trim() == line) {
        return Ok(());
    }
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(line);
    content.push('\n');
    atomic_write(path, &content)
}

fn atomic_write(path: &Path, content: &str) -> Result<()> {
    atomic_write_bytes(path, content.as_bytes())
}

fn atomic_write_bytes(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::message(format!("path has no parent: {}", path.display())))?;
    fs::create_dir_all(parent).at(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).at(parent)?;
    use std::io::Write;
    temporary.write_all(content).at(path)?;
    temporary.flush().at(path)?;
    temporary.persist(path).map_err(|error| Error::Io {
        path: path.to_path_buf(),
        source: error.error,
    })?;
    Ok(())
}

struct ScaffoldSnapshot {
    files: Vec<(PathBuf, Option<Vec<u8>>)>,
    dvc_directory_existed: bool,
}

impl ScaffoldSnapshot {
    fn capture(repo: &GitRepo) -> Result<Self> {
        let relative_paths = [
            CONFIG_NAME,
            "AGENTS.md",
            ".dvc/config",
            ".dvc/.gitignore",
            ".dvcignore",
            ".gitattributes",
        ];
        let mut files = Vec::new();
        for relative in relative_paths {
            let path = repo.root.join(relative);
            let contents = match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                    Some(fs::read(&path).at(&path)?)
                }
                Ok(_) => {
                    return Err(Error::message(format!(
                        "managed scaffold path is not a regular file: {}",
                        path.display()
                    )));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(source) => return Err(Error::Io { path, source }),
            };
            files.push((path, contents));
        }
        Ok(Self {
            files,
            dvc_directory_existed: repo.root.join(".dvc").is_dir(),
        })
    }

    fn restore(self, repo: &GitRepo) -> Result<()> {
        for (path, contents) in self.files.into_iter().rev() {
            match contents {
                Some(contents) => atomic_write_bytes(&path, &contents)?,
                None => match fs::symlink_metadata(&path) {
                    Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
                        fs::remove_file(&path).at(&path)?;
                        prune_empty_parents(&path, &repo.root)?;
                    }
                    Ok(_) => {
                        return Err(Error::message(format!(
                            "cannot roll back non-file scaffold path: {}",
                            path.display()
                        )));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(source) => return Err(Error::Io { path, source }),
                },
            }
        }
        let dvc_dir = repo.root.join(".dvc");
        if !self.dvc_directory_existed && dvc_dir.exists() {
            fs::remove_dir_all(&dvc_dir).at(&dvc_dir)?;
        }
        Ok(())
    }
}

fn prune_empty_parents(path: &Path, root: &Path) -> Result<()> {
    let mut current = path.parent();
    while let Some(directory) = current {
        if directory == root {
            break;
        }
        match fs::remove_dir(directory) {
            Ok(()) => current = directory.parent(),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::NotFound
                ) =>
            {
                break;
            }
            Err(source) => {
                return Err(Error::Io {
                    path: directory.to_path_buf(),
                    source,
                });
            }
        }
    }
    Ok(())
}
