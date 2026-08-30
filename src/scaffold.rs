use std::fs;
use std::path::{Path, PathBuf};

use chrono::Local;
use serde::Serialize;

use crate::config::{CONFIG_NAME, Config, Profile, SCHEMA_VERSION};
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::instructions::BOOTSTRAP;
use crate::manifest::{TaskManifest, one_line};
use crate::process::{run, run_unchecked};

#[derive(Debug, Clone)]
pub struct InitOptions {
    pub repo: PathBuf,
    pub profile: Profile,
    pub dvc: bool,
    pub dvc_remote: Option<String>,
    pub dvc_remote_url: Option<String>,
    pub version_aware: bool,
    pub adopt: bool,
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

pub fn init(options: &InitOptions) -> Result<InitReport> {
    let repo = GitRepo::discover(&options.repo)?;
    let config_path = repo.root.join(CONFIG_NAME);
    let agents_path = repo.root.join("AGENTS.md");
    if let Some(url) = &options.dvc_remote_url {
        validate_public_remote_url(url)?;
    }
    let mut actions = Vec::new();
    let existing_config = config_path.is_file();
    let mut config = if existing_config {
        Config::load(&repo)?
    } else {
        let mut config = Config::defaults(options.profile, options.dvc);
        detect_git_defaults(&repo, &mut config)?;
        config
    };
    if options.dvc {
        config.dvc.enabled = true;
        if !existing_config || options.version_aware {
            config.dvc.require_version_aware = options.version_aware;
        }
        if options.dvc_remote.is_some() {
            config.dvc.remote.clone_from(&options.dvc_remote);
        } else if config.dvc.remote.is_none() {
            config.dvc.remote = detect_dvc_remote(&repo)?;
        }
        if !config.agent.modules.iter().any(|module| module == "dvc") {
            config.agent.modules.push("dvc".to_owned());
        }
    }
    config.validate()?;

    let rendered = config.render()?;
    let config_changed = fs::read_to_string(&config_path).ok().as_deref() != Some(&rendered);
    if config_changed {
        actions.push(InitAction {
            action: if existing_config { "update" } else { "create" }.to_owned(),
            path: CONFIG_NAME.to_owned(),
            detail: "repository policy configuration".to_owned(),
        });
    }

    let mut adopted_module = None;
    let mut install_bootstrap = false;
    if agents_path.is_file() {
        let existing = fs::read_to_string(&agents_path).at(&agents_path)?;
        if existing != BOOTSTRAP {
            if !options.adopt {
                return Err(Error::message(
                    "AGENTS.md already contains unmanaged instructions; rerun with --adopt to preserve them as a repository instruction module",
                ));
            }
            let module = repo.root.join(".workspace-mgr/instructions/repository.md");
            if module.exists() {
                if !module.is_file() {
                    return Err(Error::message(format!(
                        "repository instruction module is not a regular file: {}",
                        module.display()
                    )));
                }
                let preserved = fs::read_to_string(&module).at(&module)?;
                if preserved != existing {
                    return Err(Error::message(
                        "AGENTS.md and the existing repository instruction module differ; merge them explicitly before rerunning init --adopt",
                    ));
                }
            }
            actions.push(InitAction {
                action: if module.exists() {
                    "preserve"
                } else {
                    "create"
                }
                .to_owned(),
                path: ".workspace-mgr/instructions/repository.md".to_owned(),
                detail: "preserve the previous AGENTS.md as a tracked repository module".to_owned(),
            });
            actions.push(InitAction {
                action: "replace".to_owned(),
                path: "AGENTS.md".to_owned(),
                detail: "install the workspace-mgr bootstrap".to_owned(),
            });
            adopted_module = Some((module, existing));
            install_bootstrap = true;
        }
    } else {
        actions.push(InitAction {
            action: "create".to_owned(),
            path: "AGENTS.md".to_owned(),
            detail: "workspace-mgr bootstrap".to_owned(),
        });
        install_bootstrap = true;
    }

    if options.dvc {
        if !repo.root.join(".dvc").exists() {
            actions.push(InitAction {
                action: "run".to_owned(),
                path: ".dvc/".to_owned(),
                detail: "dvc init".to_owned(),
            });
            if !options.dry_run {
                run("dvc", ["init"], &repo.root)?;
            }
        }
        match (&options.dvc_remote, &options.dvc_remote_url) {
            (Some(name), Some(url)) => {
                actions.push(InitAction {
                    action: "configure".to_owned(),
                    path: ".dvc/config".to_owned(),
                    detail: format!("set default DVC remote {name:?}"),
                });
                if !options.dry_run {
                    run("dvc", ["remote", "add", "-d", "-f", name, url], &repo.root)?;
                }
            }
            (Some(name), None) => {
                actions.push(InitAction {
                    action: "configure".to_owned(),
                    path: ".dvc/config".to_owned(),
                    detail: format!("select existing DVC remote {name:?} as default"),
                });
                if !options.dry_run {
                    run("dvc", ["remote", "default", name], &repo.root)?;
                }
            }
            (None, Some(_)) => {
                return Err(Error::message("--dvc-remote-url requires --dvc-remote"));
            }
            _ => {}
        }
        if options.version_aware {
            let remote = config.dvc.remote.as_deref().ok_or_else(|| {
                Error::message(
                    "--version-aware requires an existing default DVC remote or --dvc-remote",
                )
            })?;
            actions.push(InitAction {
                action: "configure".to_owned(),
                path: ".dvc/config".to_owned(),
                detail: format!("require exact version-aware verification for {remote:?}"),
            });
            if !options.dry_run {
                run(
                    "dvc",
                    ["remote", "modify", remote, "version_aware", "true"],
                    &repo.root,
                )?;
            }
        }
        if !options.dry_run {
            ensure_line(&repo.root.join(".dvc/.gitignore"), "/config.local")?;
        }
    }

    if !options.dry_run {
        if config_changed {
            atomic_write(&config_path, &rendered)?;
        }
        if let Some((module, existing)) = adopted_module {
            if !module.exists() {
                atomic_write(&module, &existing)?;
            }
        }
        if install_bootstrap {
            atomic_write(&agents_path, BOOTSTRAP)?;
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

fn validate_public_remote_url(url: &str) -> Result<()> {
    if url.trim().is_empty() || url.contains(['\n', '\r']) {
        return Err(Error::message(
            "DVC remote URL must be a non-empty single-line value",
        ));
    }
    if let Some((_, authority_and_path)) = url.split_once("://") {
        let authority = authority_and_path.split('/').next().unwrap_or_default();
        if authority.contains('@') {
            return Err(Error::message(
                "DVC remote URL must not contain embedded credentials; use local DVC configuration or environment credentials",
            ));
        }
    }
    Ok(())
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
        config.git.base_branch.clone_from(&branch);
        config.git.shared_checkout_branch = branch;
    }
    Ok(())
}

fn detect_dvc_remote(repo: &GitRepo) -> Result<Option<String>> {
    if !repo.root.join(".dvc").exists() {
        return Ok(None);
    }
    let output = run_unchecked("dvc", ["config", "core.remote"], &repo.root)?;
    if !output.success() || output.stdout.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(output.stdout.trim().to_owned()))
}

#[derive(Debug, Clone)]
pub struct TaskCreateOptions {
    pub repo: PathBuf,
    pub slug: String,
    pub title: String,
    pub purpose: String,
    pub branch: Option<String>,
    pub timestamp: Option<String>,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskCreateReport {
    pub status: String,
    pub task_id: String,
    pub path: String,
    pub branch: String,
    pub base_oid: String,
    pub files: Vec<String>,
}

pub fn create_task(options: &TaskCreateOptions) -> Result<TaskCreateReport> {
    validate_slug(&options.slug)?;
    let title = one_line(&options.title, "task title")?;
    let purpose = one_line(&options.purpose, "task purpose")?;
    let repo = GitRepo::discover(&options.repo)?;
    let config = Config::load(&repo)?;
    if !config.tasks.enabled {
        return Err(Error::message(
            "task scaffolding is disabled by repository config",
        ));
    }
    let timestamp = match &options.timestamp {
        Some(value) => validate_timestamp(value)?,
        None => Local::now().format("%Y%m%d-%H%M%S").to_string(),
    };
    let pattern = config
        .tasks
        .directory_pattern
        .replace("%Y%m%d-%H%M%S", &timestamp)
        .replace("{slug}", &options.slug);
    if pattern.contains('%') {
        return Err(Error::message(
            "tasks.directory_pattern contains unsupported chrono directives; use %Y%m%d-%H%M%S and {slug}",
        ));
    }
    let task_id = crate::path::repo_path(&pattern, "generated task directory")?;
    if task_id.contains('/') {
        return Err(Error::message(
            "generated task directory must be directly below the repository root",
        ));
    }
    let task_dir = repo.root.join(&task_id);
    if task_dir.exists() {
        return Err(Error::message(format!(
            "task directory already exists: {}",
            task_dir.display()
        )));
    }
    let branch = options
        .branch
        .clone()
        .unwrap_or_else(|| format!("{}{}", config.git.branch_prefix, options.slug));
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
    let base_oid = repo.fetch_branch(&config.git.remote, &config.git.base_branch)?;
    let manifest = TaskManifest {
        schema_version: SCHEMA_VERSION,
        id: task_id.clone(),
        path: task_id.clone(),
        branch: branch.clone(),
        additional_scopes: Vec::new(),
    };
    let readme = format!(
        "# {}\n\n{}\n\n## Directory map\n\n- `README.md` describes this task and its retained outputs.\n- `{}` declares the task scope and target branch.\n",
        title, purpose, config.tasks.manifest_name
    );
    let files = vec![
        format!("{task_id}/README.md"),
        format!("{task_id}/{}", config.tasks.manifest_name),
    ];
    if !options.dry_run {
        repo.run([
            "update-ref",
            "-m",
            &format!("workspace-mgr task create {task_id}"),
            &format!("refs/heads/{branch}"),
            &base_oid,
            &"0".repeat(40),
        ])?;
        if let Err(error) =
            write_task_files(&task_dir, &config.tasks.manifest_name, &readme, &manifest)
        {
            let _ = repo.run_unchecked([
                "update-ref",
                "-d",
                &format!("refs/heads/{branch}"),
                &base_oid,
            ]);
            return Err(error);
        }
    }
    Ok(TaskCreateReport {
        status: if options.dry_run {
            "dry_run"
        } else {
            "created"
        }
        .to_owned(),
        task_id,
        path: task_dir.display().to_string(),
        branch,
        base_oid,
        files,
    })
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
    if result.is_err() {
        let _ = fs::remove_dir_all(task_dir);
    }
    result
}

fn validate_slug(slug: &str) -> Result<()> {
    let valid = !slug.is_empty()
        && !slug.starts_with('-')
        && !slug.ends_with('-')
        && !slug.contains("--")
        && slug
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if !valid {
        return Err(Error::message(
            "task slug must be lowercase kebab case using ASCII letters and digits",
        ));
    }
    Ok(())
}

fn validate_timestamp(timestamp: &str) -> Result<String> {
    let valid = timestamp.len() == 15
        && timestamp.as_bytes()[8] == b'-'
        && timestamp
            .bytes()
            .enumerate()
            .all(|(index, byte)| index == 8 || byte.is_ascii_digit());
    if !valid {
        return Err(Error::message("timestamp must use YYYYMMDD-HHMMSS"));
    }
    Ok(timestamp.to_owned())
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
    let parent = path
        .parent()
        .ok_or_else(|| Error::message(format!("path has no parent: {}", path.display())))?;
    fs::create_dir_all(parent).at(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).at(parent)?;
    use std::io::Write;
    temporary.write_all(content.as_bytes()).at(path)?;
    temporary.flush().at(path)?;
    temporary.persist(path).map_err(|error| Error::Io {
        path: path.to_path_buf(),
        source: error.error,
    })?;
    Ok(())
}
