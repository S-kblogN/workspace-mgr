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
use crate::path::{reject_symlink_traversal, repo_path, resolved_under};
use crate::policy::{
    REPOSITORY_IGNORE_MODULE, REPOSITORY_MODULE_MAX_BYTES, REVIEW_DELIVERABLE_CREATION_TIMING,
    REVIEW_INFRASTRUCTURE_CREATION_TIMING, REVIEW_INITIAL_STATE, REVIEW_MANAGED_BY,
    REVIEW_MERGE_AUTHORITY, REVIEW_PULL_REQUEST, REVIEW_SYNC_CADENCE, ROOT_IGNORE_NAME,
    TASK_MANIFEST_NAME,
};
use crate::s3_purge;
use crate::storage::{LOCAL_IGNORE_BEGIN, LOCAL_IGNORE_END};

const STORAGE_GITIGNORE: &str = "/config.local\n/tmp\n/cache\n";
const STORAGE_IGNORE: &str =
    "# Managed by workspace-mgr. Storage paths are selected through workspace-mgr.\n";

/// The fixed ignore rules the product owns, in the groups the generated file
/// renders. The set is deliberately curated rather than the union of the
/// common ignore templates. It holds two kinds of rule. Most cover output a
/// tool regenerates under a name that cannot plausibly be retained content,
/// so they never hide a result someone meant to keep: `target/`, `build/`,
/// `dist/`, `lib/`, `out/`, `docs/`, `*.log`, and `coverage` are absent for
/// exactly that reason, being ordinary names for retained data as often as
/// for build output, and so are `.RData`, knitr's `*_cache/`, and Julia's
/// `Manifest.toml`, which can be the only or the reproducibility-critical copy
/// of a result. The last group covers files that hold credentials or private
/// runtime configuration, which never belong in the repository at all.
///
/// Publication reads the same list: a path that one of these rules hides is
/// hidden by a rule every initialized clone carries, so it never looks like a
/// machine-local rule even before the generated file reaches the base branch.
pub(crate) const PRODUCT_IGNORE_GROUPS: &[(&str, &[&str])] = &[
    (
        "Operating-system metadata.",
        &[
            ".DS_Store",
            "._*",
            ".AppleDouble",
            ".LSOverride",
            "__MACOSX/",
            "Thumbs.db",
            "ehthumbs.db",
            "[Dd]esktop.ini",
            ".directory",
            ".fuse_hidden*",
            ".Trash-*",
            ".nfs*",
        ],
    ),
    (
        "Editor swap, backup, and per-user state.",
        &[
            "[._]*.sw[a-p]",
            "*~",
            "\\#*\\#",
            ".\\#*",
            "*.iws",
            ".idea/**/workspace.xml",
            ".idea/**/shelf",
        ],
    ),
    (
        "Python bytecode, environments, and tool caches.",
        &[
            "__pycache__/",
            "*.py[codz]",
            "*$py.class",
            "*.egg-info/",
            ".eggs/",
            ".venv/",
            "venv/",
            "__pypackages__/",
            ".pdm-build/",
            ".ipynb_checkpoints/",
            ".pytest_cache/",
            ".mypy_cache/",
            ".dmypy.json",
            ".ruff_cache/",
            ".pytype/",
            ".pyre/",
            ".tox/",
            ".nox/",
            ".hypothesis/",
            ".coverage",
            ".coverage.*",
            "htmlcov/",
            "cython_debug/",
            "__marimo__/",
            ".ropeproject",
        ],
    ),
    (
        "JavaScript dependencies, caches, and framework output.",
        &[
            "node_modules/",
            ".npm/",
            ".pnpm-store/",
            "npm-debug.log*",
            "yarn-debug.log*",
            "yarn-error.log*",
            ".eslintcache",
            ".stylelintcache",
            "*.tsbuildinfo",
            ".parcel-cache/",
            ".next/",
            ".nuxt/",
            ".svelte-kit/",
            ".vite/",
            ".node_repl_history",
        ],
    ),
    (
        "R, Julia, and Rust session and tool by-products.",
        &[
            ".Rhistory",
            ".Rapp.history",
            ".RDataTmp",
            ".Rproj.user/",
            "*.jl.cov",
            "*.jl.*.cov",
            "*.jl.mem",
            "*.jl.*.mem",
            "**/*.rs.bk",
            "rustc-ice-*.txt",
        ],
    ),
    (
        "Credentials and private runtime configuration.",
        &[
            ".env",
            ".env.*",
            "!.env.example",
            ".Renviron",
            ".httr-oauth",
            ".pypirc",
            ".streamlit/secrets.toml",
        ],
    ),
];

/// Every product rule, in the order the generated file writes them.
pub(crate) fn product_ignore_rules() -> impl Iterator<Item = &'static str> {
    PRODUCT_IGNORE_GROUPS
        .iter()
        .flat_map(|(_, rules)| rules.iter().copied())
}

/// The first line of every root ignore file the product generated, and the
/// record that it owns this one. The other whole-file scaffolds are identified
/// by their reserved path alone, which is safe because nothing but the product
/// writes them. A root `.gitignore` is different: nearly every repository
/// already has one, written by hand long before the product existed, so
/// ownership here is claimed only by having written the file.
const ROOT_IGNORE_HEADER: &str = "# Generated by workspace-mgr. Do not edit this file by hand.\n";

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

/// Git has no include directive, so a repository cannot keep its own root
/// ignore rules and still receive the product's. The product therefore owns the
/// whole root file and generates it from three parts: its own fixed rules, this
/// repository's rules imported from a module it owns, and any managed
/// local-only block the file already holds. Rules that apply to one task stay
/// in that task's own ignore file, where review can see them.
///
/// `untrack` writes its block into the ignore file of the path's own directory,
/// and today every `untrack` runs inside a task, so no command puts a block in
/// the root file. Preserving one is what keeps the format whole rather than a
/// behaviour a user can reach.
fn regenerate_root_gitignore(repo: &GitRepo, existing: &str) -> Result<ComposedRootIgnore> {
    Ok(compose_root_gitignore(
        repository_ignore_module(repo)?.as_deref(),
        existing,
    ))
}

/// A regeneration of the root ignore file, with whatever the previous file
/// needed repaired along the way.
struct ComposedRootIgnore {
    rendered: String,
    repairs: Vec<String>,
}

fn compose_root_gitignore(module: Option<&str>, existing: &str) -> ComposedRootIgnore {
    let mut rendered = format!(
        "{ROOT_IGNORE_HEADER}# This repository's own ignore rules belong in {REPOSITORY_IGNORE_MODULE};\n# `workspace-mgr init` regenerates this file from that module and the fixed rules below.\n# Rules that apply to one task belong in that task's own {ROOT_IGNORE_NAME}.\n"
    );
    for (title, rules) in PRODUCT_IGNORE_GROUPS {
        rendered.push_str(&format!("\n# Product rules: {title}\n"));
        for rule in *rules {
            rendered.push_str(rule);
            rendered.push('\n');
        }
    }
    if let Some(module) = module {
        rendered.push_str(&format!(
            "\n# Repository rules imported from {REPOSITORY_IGNORE_MODULE}.\n"
        ));
        rendered.push_str(module);
    }
    let retained = retained_local_blocks(existing);
    rendered.push_str(&retained.blocks);
    ComposedRootIgnore {
        rendered,
        repairs: retained.repairs,
    }
}

/// This repository's own root ignore rules. It is repository-owned content like
/// `.workspace-mgr/instructions/repository.md`: the product imports it verbatim
/// and validates only its size and encoding, because ignore patterns are the
/// repository's business.
fn repository_ignore_module(repo: &GitRepo) -> Result<Option<String>> {
    reject_symlink_traversal(
        &repo.root,
        REPOSITORY_IGNORE_MODULE,
        "repository ignore module",
    )?;
    let path = resolved_under(&repo.root, REPOSITORY_IGNORE_MODULE);
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            return Err(Error::message(format!(
                "repository ignore module is not valid UTF-8: {}",
                path.display()
            )));
        }
        Err(source) => return Err(Error::Io { path, source }),
    };
    if content.len() > REPOSITORY_MODULE_MAX_BYTES {
        return Err(Error::message(format!(
            "repository ignore module exceeds 64 KiB: {}",
            path.display()
        )));
    }
    if content.trim().is_empty() {
        return Ok(None);
    }
    // The two markers belong to `untrack`, and regeneration harvests them back
    // out of the file it just wrote. A module that carries one would therefore
    // be re-appended on every reconciliation and the file would never settle,
    // so the module is asked for ignore patterns only.
    if let Some(marker) = content
        .lines()
        .find(|line| line.starts_with(LOCAL_IGNORE_BEGIN) || line.starts_with(LOCAL_IGNORE_END))
    {
        return Err(Error::message(format!(
            "repository ignore module may not contain workspace-mgr's own managed block markers, found {marker:?} in {}; those lines belong to `workspace-mgr untrack`, so remove them and keep only this repository's ignore patterns",
            path.display()
        )));
    }
    let mut normalized = content.trim_end_matches('\n').to_owned();
    normalized.push('\n');
    Ok(Some(normalized))
}

fn read_root_gitignore(repo: &GitRepo) -> Result<String> {
    reject_root_ignore_symlink(repo)?;
    let path = repo.root.join(ROOT_IGNORE_NAME);
    match fs::read(&path) {
        // Only the managed blocks are read back, and `untrack` writes those as
        // UTF-8, so a lossy decode cannot corrupt a rule that is preserved.
        Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(source) => Err(Error::Io { path, source }),
    }
}

/// The managed blocks harvested from the current root ignore file, with the
/// unpaired markers that could not be read as one.
struct RetainedLocalBlocks {
    blocks: String,
    repairs: Vec<String>,
}

/// Re-emits every `untrack` block of the current root ignore file exactly as
/// `workspace-mgr untrack` wrote it. Regeneration must not orphan bytes that
/// are still on this machine, and the block has to stay byte-identical so that
/// restoring the path to tracking can still find and remove it.
///
/// A marker without its partner has no readable extent, so it is dropped and
/// reported rather than refused. Refusing would wedge the one file whose whole
/// point is that the product can always restore it: a hand edit or a merge of
/// the tracked root file would leave neither `init` nor `doctor` able to act on
/// it. Dropping it cannot publish local-only bytes either: a path outside every
/// declared scope is never staged, and one inside a scope is removed from the
/// index by its placement record rather than by this rule.
fn retained_local_blocks(existing: &str) -> RetainedLocalBlocks {
    let mut retained = RetainedLocalBlocks {
        blocks: String::new(),
        repairs: Vec::new(),
    };
    let mut open: Option<(String, Vec<&str>)> = None;
    for line in existing.lines() {
        if let Some(key) = line.strip_prefix(LOCAL_IGNORE_BEGIN) {
            if let Some((abandoned, _)) = open.take() {
                retained
                    .repairs
                    .push(format!("{LOCAL_IGNORE_BEGIN}{abandoned}"));
            }
            open = Some((key.to_owned(), Vec::new()));
            continue;
        }
        if let Some(key) = line.strip_prefix(LOCAL_IGNORE_END) {
            match open.take() {
                Some((expected, body)) if expected == key => {
                    retained.blocks.push('\n');
                    retained.blocks.push_str(LOCAL_IGNORE_BEGIN);
                    retained.blocks.push_str(&expected);
                    retained.blocks.push('\n');
                    for entry in body {
                        retained.blocks.push_str(entry);
                        retained.blocks.push('\n');
                    }
                    retained.blocks.push_str(LOCAL_IGNORE_END);
                    retained.blocks.push_str(&expected);
                    retained.blocks.push('\n');
                }
                Some((expected, _)) => {
                    retained
                        .repairs
                        .push(format!("{LOCAL_IGNORE_BEGIN}{expected}"));
                    retained.repairs.push(line.to_owned());
                }
                None => retained.repairs.push(line.to_owned()),
            }
            continue;
        }
        if let Some((_, body)) = open.as_mut() {
            body.push(line);
        }
    }
    if let Some((abandoned, _)) = open {
        retained
            .repairs
            .push(format!("{LOCAL_IGNORE_BEGIN}{abandoned}"));
    }
    retained
}

/// Whether the product may regenerate the root ignore file. It may when it
/// wrote the file that is there, and a file it never wrote holds this
/// repository's own rules, which regeneration would discard.
fn root_ignore_is_product_owned(existing: &str) -> bool {
    existing.is_empty() || existing.starts_with(ROOT_IGNORE_HEADER)
}

/// Where this repository's own root ignore rules belong. Every refusal about
/// the root file says this, because the file exists in almost every repository
/// and its owner needs somewhere to put what it holds.
fn root_ignore_migration_hint() -> String {
    format!(
        "move this repository's own rules into {REPOSITORY_IGNORE_MODULE}, remove the root {ROOT_IGNORE_NAME}, and run `workspace-mgr init` again, which regenerates it from that module and the product's fixed rules"
    )
}

fn reject_foreign_root_ignore() -> Error {
    Error::message(format!(
        "the root {ROOT_IGNORE_NAME} was not generated by workspace-mgr, and regenerating it would discard the rules it holds; {}",
        root_ignore_migration_hint()
    ))
}

/// The root ignore file is the one reserved scaffold path a repository is
/// likely to have arranged for itself, so a symlink refusal about it carries
/// the same migration guidance as a collision.
fn reject_root_ignore_symlink(repo: &GitRepo) -> Result<()> {
    reject_symlink_traversal(&repo.root, ROOT_IGNORE_NAME, "managed scaffold path")
        .map_err(|error| Error::message(format!("{error}; {}", root_ignore_migration_hint())))
}

pub fn validate_owned_files(repo: &GitRepo, config: &Config) -> Result<()> {
    let mut drifted = Vec::new();
    check_owned_file(repo, "AGENTS.md", BOOTSTRAP.as_bytes(), &mut drifted)?;
    let existing_root_ignore = read_root_gitignore(repo)?;
    // Reported before drift, because `init` refuses this file rather than
    // reconciling it and a bare "run init" would send the user in a circle.
    if !root_ignore_is_product_owned(&existing_root_ignore) {
        return Err(reject_foreign_root_ignore());
    }
    let root_ignore = regenerate_root_gitignore(repo, &existing_root_ignore)?.rendered;
    check_owned_file(repo, ROOT_IGNORE_NAME, root_ignore.as_bytes(), &mut drifted)?;
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
    reject_root_ignore_symlink(&repo)?;
    let config_path = repo.root.join(CONFIG_NAME);
    let agents_path = repo.root.join("AGENTS.md");
    let root_ignore_path = repo.root.join(ROOT_IGNORE_NAME);
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

    let existing_root_ignore = read_root_gitignore(&repo)?;
    // A root ignore file the product did not write is this repository's own,
    // and every repository initialized before the product owned this path has
    // one. Reconciling it would discard rules nothing else records, so the
    // takeover is a migration the user performs, not a silent rewrite.
    if !root_ignore_is_product_owned(&existing_root_ignore) {
        return Err(reject_foreign_root_ignore());
    }
    let composed = regenerate_root_gitignore(&repo, &existing_root_ignore)?;
    let root_ignore = composed.rendered;
    let root_ignore_changed =
        fs::read_to_string(&root_ignore_path).ok().as_deref() != Some(&root_ignore);
    if root_ignore_changed {
        let mut detail = format!("product ignore rules and {REPOSITORY_IGNORE_MODULE}");
        if !composed.repairs.is_empty() {
            detail.push_str(&format!(
                "; dropped {} unpaired managed local-only block {}: {}",
                composed.repairs.len(),
                if composed.repairs.len() == 1 {
                    "marker"
                } else {
                    "markers"
                },
                composed.repairs.join(", ")
            ));
        }
        actions.push(InitAction {
            action: if root_ignore_path.is_file() {
                "update"
            } else {
                "create"
            }
            .to_owned(),
            path: ROOT_IGNORE_NAME.to_owned(),
            detail,
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
            if root_ignore_changed {
                atomic_write(&root_ignore_path, &root_ignore)?;
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
    for relative in [
        CONFIG_NAME,
        "AGENTS.md",
        ROOT_IGNORE_NAME,
        ".dvc",
        ".dvcignore",
    ] {
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
    let mut message = format!(
        "cannot initialize because reserved workspace-mgr scaffold paths already exist: {}; move or remove those paths explicitly before first initialization",
        collisions.join(", ")
    );
    if collisions.contains(&ROOT_IGNORE_NAME) {
        message.push_str(&format!(
            "; the root {ROOT_IGNORE_NAME} is generated from the product's fixed rules and {REPOSITORY_IGNORE_MODULE}, so move this repository's own rules into that module, remove the root file, and run `workspace-mgr init` again"
        ));
    }
    Err(Error::message(message))
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

/// The fixed part of the scaffolded README: everything the command does not
/// interpolate. Publication compares a task's README against this block, rather
/// than against a whole rendering, so that editing the mutable task title or
/// purpose cannot turn an untouched README into a record.
pub fn task_readme_directory_map() -> String {
    format!(
        "## Directory map\n\n- `README.md` describes this task and its retained outputs.\n- `{TASK_MANIFEST_NAME}` declares the task scope and target branch.\n- Keep this task's tools, process, decisions, and hard-to-reproduce results in this directory and list them here.\n"
    )
}

/// The exact scaffolded README for a deliverable task.
pub fn task_readme(title: &str, purpose: &str) -> String {
    format!("# {title}\n\n{purpose}\n\n{}", task_readme_directory_map())
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
        let oid = repo
            .remote_branch_oid(&config.git.remote, &config.git.branch)?
            .ok_or_else(|| {
                Error::message(format!(
                    "remote base branch does not exist: {}/{}",
                    config.git.remote, config.git.branch
                ))
            })?;
        repo.fetch_branch_objects(&config.git.remote, &config.git.branch, &oid)?;
        oid
    } else {
        repo.fetch_branch(&config.git.remote, &config.git.branch)?
    };
    // The new task starts from the shared branch, which may already require
    // a newer workspace-mgr than the local checkout declares.
    crate::config::require_supported_cli_at(
        &repo,
        &base_oid,
        &format!("{}/{}", config.git.remote, config.git.branch),
    )?;
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
        cloud_usage_approval: None,
    };
    let readme = task_readme(&title, &purpose);
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
            ROOT_IGNORE_NAME,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn untrack_block(key: &str, pattern: &str) -> String {
        format!("\n{LOCAL_IGNORE_BEGIN}{key}\n{pattern}\n{LOCAL_IGNORE_END}{key}\n")
    }

    #[test]
    fn generates_the_product_rules_without_a_repository_module() {
        let rendered = compose_root_gitignore(None, "").rendered;
        assert!(rendered.starts_with(ROOT_IGNORE_HEADER));
        assert!(rendered.contains(REPOSITORY_IGNORE_MODULE));
        assert!(rendered.contains("\n.DS_Store\n"));
        assert!(rendered.contains("\nnode_modules/\n"));
        // Names that are as plausibly retained data as build output stay out.
        for ambiguous in [
            "\ntarget/\n",
            "\nbuild/\n",
            "\ndist/\n",
            "\nlib/\n",
            "\nout/\n",
            "\ndocs/\n",
            "\n*.log\n",
            "\ncoverage\n",
            "\n.RData\n",
            "\n*_cache/\n",
            "\nManifest*.toml\n",
        ] {
            assert!(!rendered.contains(ambiguous), "{ambiguous:?} is too broad");
        }
        assert!(
            !rendered.contains("# Repository rules imported"),
            "an absent module must not leave an empty import section"
        );
        assert!(rendered.ends_with("\n.streamlit/secrets.toml\n"));
        // The template kept for sharing follows the rule that would hide it.
        let env = rendered.find("\n.env.*\n").unwrap();
        let example = rendered.find("\n!.env.example\n").unwrap();
        assert!(env < example);
        // Publication classifies a rule by the same list the file is built
        // from, so the two cannot drift apart.
        for rule in product_ignore_rules() {
            assert!(rendered.contains(&format!("\n{rule}\n")), "{rule}");
        }
    }

    #[test]
    fn imports_the_repository_module_verbatim_after_the_product_rules() {
        let rendered = compose_root_gitignore(Some("/vendor/\n*.bak\n"), "").rendered;
        let product = rendered.find(".DS_Store").unwrap();
        let imported = rendered.find("/vendor/").unwrap();
        assert!(product < imported, "product rules come first");
        assert!(rendered.ends_with(&format!(
            "\n# Repository rules imported from {REPOSITORY_IGNORE_MODULE}.\n/vendor/\n*.bak\n"
        )));
    }

    #[test]
    fn regeneration_preserves_untrack_blocks_byte_for_byte() {
        let first = untrack_block("6461746162696e", "/data.bin");
        let second = untrack_block("6c6f67", "/log");
        let existing = format!("# a hand written file\n/stale-rule{first}{second}");

        let composed = compose_root_gitignore(Some("/vendor/\n"), &existing);

        assert!(composed.repairs.is_empty());
        assert!(
            !composed.rendered.contains("/stale-rule"),
            "an unmanaged hand edit is drift, not content to merge"
        );
        assert!(composed.rendered.ends_with(&format!("{first}{second}")));
        // Regeneration is idempotent, so drift detection cannot oscillate.
        assert_eq!(
            compose_root_gitignore(Some("/vendor/\n"), &composed.rendered).rendered,
            composed.rendered
        );
    }

    #[test]
    fn a_module_may_not_carry_the_product_s_own_block_markers() {
        // Harvesting a marker out of the file that regeneration just wrote
        // would append the module's block again on every reconciliation, so
        // the file would grow without bound and never reach `no_changes`.
        let temp = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: temp.path().to_path_buf(),
        };
        let module = temp.path().join(REPOSITORY_IGNORE_MODULE);
        fs::create_dir_all(module.parent().unwrap()).unwrap();
        fs::write(
            &module,
            format!("/vendor/\n{}\n", untrack_block("6161", "/aa").trim()),
        )
        .unwrap();

        let error = repository_ignore_module(&repo)
            .expect_err("the managed markers belong to the product, not to the module");

        assert!(
            error.to_string().contains("managed block markers"),
            "{error}"
        );
    }

    #[test]
    fn regeneration_drops_an_unpaired_block_marker_and_reports_it() {
        let key = "6461746162696e";
        let whole = untrack_block(key, "/data.bin");
        for (broken, marker) in [
            (
                format!("{LOCAL_IGNORE_BEGIN}{key}\n/data.bin\n"),
                format!("{LOCAL_IGNORE_BEGIN}{key}"),
            ),
            (
                format!("{LOCAL_IGNORE_END}{key}\n"),
                format!("{LOCAL_IGNORE_END}{key}"),
            ),
            (
                format!("{LOCAL_IGNORE_BEGIN}{key}\n/data.bin\n{LOCAL_IGNORE_END}6c6f67\n"),
                format!("{LOCAL_IGNORE_END}6c6f67"),
            ),
            (
                format!("{LOCAL_IGNORE_BEGIN}{key}\n{LOCAL_IGNORE_BEGIN}6c6f67\n"),
                format!("{LOCAL_IGNORE_BEGIN}6c6f67"),
            ),
        ] {
            let composed = compose_root_gitignore(None, &broken);
            assert!(
                composed.repairs.contains(&marker),
                "{marker} not reported in {:?}",
                composed.repairs
            );
            assert!(
                !composed.rendered.contains(LOCAL_IGNORE_BEGIN),
                "a marker with no readable extent is dropped: {}",
                composed.rendered
            );
            // The repaired file is what the product owns, so a second pass
            // settles rather than reporting the same repair forever.
            let settled = compose_root_gitignore(None, &composed.rendered);
            assert!(settled.repairs.is_empty());
            assert_eq!(settled.rendered, composed.rendered);
        }

        // A well-formed block beside a damaged marker still survives.
        let mixed = format!("{whole}{LOCAL_IGNORE_END}6c6f67\n");
        let composed = compose_root_gitignore(None, &mixed);
        assert!(composed.rendered.ends_with(&whole), "{}", composed.rendered);
        assert_eq!(composed.repairs, vec![format!("{LOCAL_IGNORE_END}6c6f67")]);
    }

    #[test]
    fn only_a_file_the_product_generated_is_reconciled() {
        assert!(root_ignore_is_product_owned(""));
        assert!(root_ignore_is_product_owned(
            &compose_root_gitignore(None, "").rendered
        ));
        // Hand edits below the header stay drift the product repairs.
        assert!(root_ignore_is_product_owned(&format!(
            "{}{}",
            compose_root_gitignore(None, "").rendered,
            "/hand-added\n"
        )));
        // A repository's own file, of the kind every installation predating
        // product ownership has, is never reconciled over.
        assert!(!root_ignore_is_product_owned("/target/\n!keep.secret\n"));
        assert!(!root_ignore_is_product_owned("# my rules\n"));
        assert!(
            reject_foreign_root_ignore()
                .to_string()
                .contains(REPOSITORY_IGNORE_MODULE)
        );
    }
}
