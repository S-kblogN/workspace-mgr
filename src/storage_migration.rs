//! Repository adoption is one local, recoverable transaction. Legacy metadata
//! is read only here; normal storage operations write the native format.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::config::{CONFIG_NAME, Config};
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::lock::RepositoryLock;
use crate::path::{reject_symlink_traversal, repo_path, to_slash};
use crate::policy::NATIVE_STORAGE_MINIMUM_CLI_VERSION;
use crate::scaffold::{ManageOptions, ManageReport};

const JOURNAL_NAME: &str = "storage-migration.json";

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct MigrationReport {
    pub converted: Vec<Conversion>,
    pub removed: Vec<String>,
    pub retained: Vec<Retention>,
    pub excluded_nested_repositories: Vec<String>,
    pub recovered_interrupted_operation: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote_objects: Vec<crate::storage_import::RemoteObject>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub remote_transfer_bytes: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Conversion {
    pub source: String,
    pub destination: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Retention {
    pub source: String,
    pub destination: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct Report {
    pub status: String,
    pub repo: String,
    pub migration: MigrationReport,
    pub actions: Vec<crate::scaffold::ManageAction>,
}

#[derive(Default)]
struct Plan {
    writes: BTreeMap<String, Vec<u8>>,
    expected_before: BTreeMap<String, Option<Vec<u8>>>,
    removes: BTreeSet<String>,
    moves: Vec<Retention>,
    report: MigrationReport,
    inferred_url: Option<String>,
    inferred_endpoint: Option<String>,
    selected_remote: Option<String>,
    legacy_version_aware: bool,
    legacy_cas_proven: bool,
    legacy_directory: bool,
    credentials: crate::native_s3::CredentialsConfig,
    import: Option<crate::storage_import::Plan>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    schema_version: u32,
    root: PathBuf,
    changes: Vec<FileChange>,
    moves: Vec<DirectoryMove>,
    #[serde(default)]
    legacy_directory: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileChange {
    path: String,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectoryMove {
    source: String,
    destination: String,
}

pub(crate) fn manage(options: &ManageOptions) -> Result<Report> {
    match manage_inner(options) {
        Ok(report) => Ok(report),
        Err(error) => {
            let recorded = GitRepo::discover(&options.repo)
                .ok()
                .and_then(|repo| crate::storage_import::journal_path(&repo).ok())
                .is_some_and(|path| path.exists());
            if recorded {
                Err(Error::message(format!(
                    "{error}; the storage import is retained for safe retry. Rerun workspace-mgr manage, or use workspace-mgr manage --cancel-migration to forget this import job while preserving source objects, uploaded versions and ownership receipts"
                )))
            } else {
                Err(error)
            }
        }
    }
}

pub(crate) fn cancel(options: &ManageOptions) -> Result<Report> {
    let repo = GitRepo::discover(&options.repo)?;
    require_primary_checkout(&repo)?;
    let _lock = if options.dry_run {
        None
    } else {
        Some(RepositoryLock::acquire(&repo)?)
    };
    let local = crate::local_state::directory_unmigrated(&repo)?;
    let journal_path = local.join(JOURNAL_NAME);
    let recovered = !options.dry_run && journal_path.exists();
    if recovered {
        recover(&repo, &journal_path)?;
    }
    let rows = crate::storage_import::cancel(&repo, options.dry_run)?;
    let present = rows.is_some();
    let mut migration = MigrationReport {
        recovered_interrupted_operation: recovered,
        remote_objects: rows.unwrap_or_default(),
        ..MigrationReport::default()
    };
    if present {
        migration
            .removed
            .push(".workspace-mgr/local/storage-import.json".to_owned());
        for path in [
            ".workspace-mgr/local/storage-import-uploads/",
            ".workspace-mgr/local/cache/",
        ] {
            migration.retained.push(Retention {
                source: path.to_owned(),
                destination: path.to_owned(),
            });
        }
    }
    Ok(Report {
        status: if options.dry_run {
            "dry_run"
        } else if present {
            "migration_cancelled"
        } else {
            "no_changes"
        }
        .to_owned(),
        repo: repo.root.to_string_lossy().into_owned(),
        migration,
        actions: if present {
            vec![crate::scaffold::ManageAction {
            action: "cancel".to_owned(), path: ".workspace-mgr/local/storage-import.json".to_owned(),
            detail: "Forget only the local import job; retain legacy sources, uploaded S3 versions, verified cache and upload ownership receipts. Uploaded copies are never automatically deleted.".to_owned(),
        }]
        } else {
            Vec::new()
        },
    })
}

fn manage_inner(options: &ManageOptions) -> Result<Report> {
    let repo = GitRepo::discover(&options.repo)?;
    let journal_path = crate::local_state::directory_unmigrated(&repo)?.join(JOURNAL_NAME);
    reject_symlink_path(&journal_path)?;
    // The standard lock may adopt old private state. Reject unsupported inputs
    // before that adoption, then repeat the complete preflight under the lock.
    if !options.dry_run && !journal_path.exists() {
        prepare_plan(&repo, options)?;
    }
    let _lock = if options.dry_run {
        None
    } else {
        Some(RepositoryLock::acquire(&repo)?)
    };
    let recovered = if journal_path.exists() {
        if options.dry_run {
            return Err(Error::message(
                "an interrupted manage transaction requires recovery; run workspace-mgr manage without --dry-run to roll it back before replanning",
            ));
        }
        recover(&repo, &journal_path)?;
        true
    } else {
        false
    };
    let completed_import = crate::storage_import::finish_committed(&repo, options.dry_run)?;
    let (mut plan, scaffold) = prepare_plan(&repo, options)?;
    if plan.import.is_none()
        && crate::storage_import::journal_path(&repo)?.exists()
        && !completed_import
    {
        return Err(Error::message(
            "an interrupted storage import has no matching legacy sources; retain its private journal and reconcile the repository edits before manage",
        ));
    }
    plan.report.recovered_interrupted_operation = recovered || completed_import;
    let changed = !plan.writes.is_empty()
        || !plan.removes.is_empty()
        || !plan.moves.is_empty()
        || plan.legacy_directory;
    if !options.dry_run && changed {
        if let Some(import) = &mut plan.import {
            crate::storage_import::execute(&repo, import, &plan.expected_before)?;
            let destinations = plan
                .report
                .converted
                .iter()
                .map(|row| row.destination.clone())
                .collect::<Vec<_>>();
            crate::storage_import::bind_manifests(import, &mut plan.writes, &destinations)?;
            crate::storage_import::prepare_commit(
                &repo,
                &plan.writes,
                &plan.removes,
                &plan
                    .moves
                    .iter()
                    .map(|row| (row.source.clone(), row.destination.clone()))
                    .collect::<Vec<_>>(),
            )?;
            plan.report.remote_objects = import.objects.clone();
        }
        execute(&repo, &journal_path, &plan)?;
        if plan.import.is_some() {
            crate::storage_import::finish(&repo)?;
        }
    }
    Ok(Report {
        status: if options.dry_run {
            "dry_run"
        } else if changed {
            "managed"
        } else {
            "no_changes"
        }
        .to_owned(),
        repo: repo.root.display().to_string(),
        migration: plan.report,
        actions: scaffold.actions,
    })
}

fn prepare_plan(repo: &GitRepo, options: &ManageOptions) -> Result<(Plan, ManageReport)> {
    let mut plan = preflight(repo)?;
    let mut effective = options.clone();
    if !repo.root.join(CONFIG_NAME).exists() && effective.s3_url.is_none() {
        effective.s3_url = plan.inferred_url.clone();
        effective.s3_endpoint_url = plan.inferred_endpoint.clone();
    }
    // Both independent preflights finish before the first repository write.
    // Scaffold rendering is exposed in the report for this transaction only.
    effective.dry_run = true;
    let mut scaffold = crate::scaffold::manage_unlocked(&effective)?;
    merge_scaffold(repo, &mut plan, &scaffold)?;
    if let Some(import) = &mut plan.import {
        crate::storage_import::preflight(repo, import, &plan.expected_before)?;
        plan.report.remote_objects = import.objects.clone();
        plan.report.remote_transfer_bytes = import
            .objects
            .iter()
            .try_fold(0u64, |sum, row| sum.checked_add(row.size))
            .ok_or_else(|| Error::message("storage import byte inventory overflows"))?;
    }
    for path in plan.writes.keys() {
        if !scaffold.actions.iter().any(|action| action.path == *path)
            && !plan
                .report
                .converted
                .iter()
                .any(|item| item.destination == *path)
            && !plan
                .report
                .retained
                .iter()
                .any(|item| item.destination == *path)
        {
            scaffold.actions.push(crate::scaffold::ManageAction {
                action: "update".to_owned(),
                path: path.clone(),
                detail: "native storage migration and compatibility requirement".to_owned(),
            });
        }
    }
    Ok((plan, scaffold))
}

fn merge_scaffold(repo: &GitRepo, plan: &mut Plan, scaffold: &ManageReport) -> Result<()> {
    for (path, bytes) in &scaffold.expected_before {
        record_expected(plan, path, bytes.clone())?;
    }
    for (path, bytes) in &scaffold.writes {
        plan.writes.insert(path.clone(), bytes.clone());
    }
    // Payload ignore rules still protect the same local bytes. Only exact
    // obsolete control rules have become meaningless after this migration.
    let adopting_legacy = plan.legacy_directory
        || !plan.report.converted.is_empty()
        || plan.removes.contains(".dvcignore");
    for path in [".gitignore", ".workspace-mgr/repository.gitignore"]
        .into_iter()
        .filter(|_| adopting_legacy)
    {
        let absolute = repo.root.join(path);
        let bytes = match plan.writes.get(path) {
            Some(bytes) => Some(bytes.clone()),
            None => fs::read(&absolute).ok(),
        };
        if let Some(bytes) = bytes {
            let raw = String::from_utf8(bytes)
                .map_err(|_| Error::message(format!("ignore file {path:?} is not UTF-8")))?;
            let obsolete = [
                ".dvc",
                ".dvc/",
                "/.dvc",
                "/.dvc/",
                ".dvcignore",
                "/.dvcignore",
                "*.dvc",
            ];
            let retained = raw
                .split_inclusive('\n')
                .filter(|line| !obsolete.contains(&line.trim()))
                .collect::<String>();
            if retained != raw {
                plan.writes.insert(path.to_owned(), retained.into_bytes());
            }
        }
    }
    if !plan.report.converted.is_empty() || plan.legacy_directory {
        let path = repo.root.join(CONFIG_NAME);
        let raw = match plan.writes.get(CONFIG_NAME) {
            Some(bytes) => bytes.clone(),
            None => fs::read(&path).at(&path)?,
        };
        let raw = String::from_utf8(raw)
            .map_err(|_| Error::message("repository configuration is not UTF-8"))?;
        let mut config = Config::parse(&raw, &path)?;
        if config
            .minimum_cli_version
            .as_deref()
            .and_then(|v| semver::Version::parse(v).ok())
            .is_none_or(|v| v < NATIVE_STORAGE_MINIMUM_CLI_VERSION)
        {
            config.minimum_cli_version = Some(NATIVE_STORAGE_MINIMUM_CLI_VERSION.to_string());
            plan.writes
                .insert(CONFIG_NAME.to_owned(), config.render()?.into_bytes());
        }
    }
    plan.writes
        .retain(|path, bytes| fs::read(repo.root.join(path)).ok().as_ref() != Some(bytes));
    Ok(())
}

fn record_expected(plan: &mut Plan, path: &str, bytes: Option<Vec<u8>>) -> Result<()> {
    if let Some(previous) = plan.expected_before.get(path) {
        if *previous != bytes {
            return Err(Error::message(format!(
                "repository control {path:?} changed during manage preflight; retry after its writer finishes"
            )));
        }
    } else {
        plan.expected_before.insert(path.to_owned(), bytes);
    }
    Ok(())
}

fn read_config_snapshot(repo: &GitRepo, plan: &mut Plan) -> Result<Option<Config>> {
    reject_symlink_traversal(&repo.root, CONFIG_NAME, "repository configuration")?;
    let path = repo.root.join(CONFIG_NAME);
    match fs::read_to_string(&path) {
        Ok(raw) => {
            record_expected(plan, CONFIG_NAME, Some(raw.as_bytes().to_vec()))?;
            Ok(Some(Config::parse(&raw, &path)?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            record_expected(plan, CONFIG_NAME, None)?;
            Ok(None)
        }
        Err(source) => Err(Error::Io { path, source }),
    }
}

fn preflight(repo: &GitRepo) -> Result<Plan> {
    let mut plan = Plan::default();
    legacy_controls(repo, &mut plan)?;
    if plan.legacy_directory
        || !plan.removes.is_empty()
        || !plan.moves.is_empty()
        || !plan.writes.is_empty()
    {
        require_primary_checkout(repo)?;
    }
    let mut pointers = Vec::new();
    let mut native_pointers = Vec::new();
    let indexed = repo.run_bytes(["ls-files", "--stage", "-z"], None)?;
    let gitlinks = indexed
        .stdout
        .split(|byte| *byte == b'\0')
        .filter(|row| row.starts_with(b"160000 "))
        .map(|row| {
            let path = row
                .splitn(2, |byte| *byte == b'\t')
                .nth(1)
                .ok_or_else(|| Error::message("invalid nested Git index boundary"))?;
            std::str::from_utf8(path)
                .map(str::to_owned)
                .map_err(|_| Error::message("nested Git boundary is not UTF-8"))
        })
        .collect::<Result<BTreeSet<_>>>()?;
    let mut walker = WalkDir::new(&repo.root).follow_links(false).into_iter();
    while let Some(entry) = walker.next() {
        let entry = entry
            .map_err(|error| Error::message(format!("inspect repository migration: {error}")))?;
        if entry.path() == repo.root {
            continue;
        }
        let relative = to_slash(
            entry
                .path()
                .strip_prefix(&repo.root)
                .expect("repository entry"),
        );
        if entry.file_type().is_dir() {
            if relative == ".git" || relative == ".dvc" || relative == ".workspace-mgr/local" {
                walker.skip_current_dir();
            } else if gitlinks.contains(&relative)
                || fs::symlink_metadata(entry.path().join(".git")).is_ok()
            {
                plan.report.excluded_nested_repositories.push(relative);
                walker.skip_current_dir();
            }
            continue;
        }
        if entry.file_name() == "dvc.yaml"
            || entry.file_name() == "dvc.lock"
            || entry.file_name() == "Dvcfile"
        {
            return Err(Error::message(format!(
                "cannot migrate DVC pipeline control {relative:?}; native storage manages immutable payloads, so migrate the pipeline separately before manage"
            )));
        }
        if entry.path().extension().and_then(|name| name.to_str()) == Some("dvc") {
            require_regular(repo, &relative)?;
            pointers.push(relative);
        } else if relative.ends_with(crate::storage_format::SUFFIX) {
            require_regular(repo, &relative)?;
            native_pointers.push(relative);
        }
    }
    pointers.sort();
    if !pointers.is_empty() && plan.legacy_cas_proven {
        let location = match plan
            .expected_before
            .get(CONFIG_NAME)
            .and_then(Option::as_ref)
        {
            Some(raw) => Config::parse(
                std::str::from_utf8(raw)
                    .map_err(|_| Error::message("configuration is not UTF-8"))?,
                &repo.root.join(CONFIG_NAME),
            )?
            .s3
            .map(|s3| (s3.url, s3.endpoint_url)),
            None => plan
                .inferred_url
                .clone()
                .map(|url| (url, plan.inferred_endpoint.clone())),
        }
        .ok_or_else(|| Error::message("legacy storage pointers require a selected storage URL"))?;
        if location.0.starts_with("s3://") {
            plan.import = Some(crate::storage_import::Plan {
                client: crate::storage_import::client(
                    &location.0,
                    location.1.as_deref(),
                    &plan.credentials,
                )?,
                objects: Vec::new(),
                metadata_sources: Vec::new(),
            });
        }
    }
    let mut boundaries = BTreeSet::new();
    for pointer in native_pointers {
        let raw = fs::read_to_string(repo.root.join(&pointer)).at(repo.root.join(&pointer))?;
        record_expected(&mut plan, &pointer, Some(raw.as_bytes().to_vec()))?;
        let manifest = crate::storage_format::Manifest::parse(&raw, &pointer)?;
        let parent = Path::new(&pointer).parent().unwrap_or(Path::new(""));
        let boundary = repo_path(
            &to_slash(&parent.join(&manifest.path)),
            "native storage output",
        )?;
        if pointer != crate::storage_metadata::pointer_path(&boundary) {
            return Err(Error::message(format!(
                "native manifest {pointer:?} does not name its adjacent payload boundary"
            )));
        }
        if boundaries.iter().any(|other: &String| {
            boundary == *other
                || boundary.starts_with(&format!("{other}/"))
                || other.starts_with(&format!("{boundary}/"))
        }) {
            return Err(Error::message(format!(
                "overlapping existing native storage boundary {boundary:?}"
            )));
        }
        boundaries.insert(boundary);
    }
    for source in pointers {
        let origin = repo.root.join(&source);
        let raw = fs::read_to_string(&origin).at(&origin)?;
        record_expected(&mut plan, &source, Some(raw.as_bytes().to_vec()))?;
        let raw = crate::legacy_dvc::normalize_remote_binding(
            &raw,
            &source,
            plan.selected_remote.as_deref(),
        )?;
        let manifest = if let Some(import) = &mut plan.import {
            let (manifest, objects, metadata_sources) =
                crate::storage_import::import_manifest(&import.client, &repo.root, &raw, &source)?;
            import.objects.extend(objects);
            import.metadata_sources.extend(metadata_sources);
            manifest
        } else {
            crate::legacy_dvc::import_manifest_with_root(&raw, &source, &repo.root)?
        };
        manifest.validate(&source)?;
        let parent = Path::new(&source).parent().unwrap_or(Path::new(""));
        let boundary = repo_path(
            &to_slash(&parent.join(&manifest.path)),
            "legacy storage output",
        )?;
        if matches!(
            boundary.as_str(),
            CONFIG_NAME
                | "AGENTS.md"
                | ".gitignore"
                | ".gitattributes"
                | ".dvcignore"
                | ".dvc"
                | ".workspace-mgr"
        ) || boundary.starts_with(".workspace-mgr/")
            || boundary.starts_with(".dvc/")
        {
            return Err(Error::message(format!(
                "legacy pointer {source:?} targets repository controls rather than a payload boundary"
            )));
        }
        reject_symlink_traversal(&repo.root, &boundary, "legacy storage output")?;
        if source != format!("{boundary}.dvc") {
            return Err(Error::message(format!(
                "legacy pointer {source:?} does not match its single storage output {boundary:?}; move the pointer to the canonical output sidecar location before migration"
            )));
        }
        if boundaries.iter().any(|other: &String| {
            boundary == *other
                || boundary.starts_with(&format!("{other}/"))
                || other.starts_with(&format!("{boundary}/"))
        }) {
            return Err(Error::message(format!(
                "overlapping legacy storage boundary {boundary:?}"
            )));
        }
        boundaries.insert(boundary.clone());
        let destination = format!("{boundary}.wm-storage.json");
        reject_symlink_traversal(&repo.root, &destination, "native storage manifest")?;
        if fs::symlink_metadata(repo.root.join(&destination)).is_ok() {
            return Err(Error::message(format!(
                "native storage manifest collision at {destination:?}; migration will not overwrite an existing file"
            )));
        }
        record_expected(&mut plan, &destination, None)?;
        let bytes = manifest.serialize()?.into_bytes();
        // The parser verifies exactly the bytes we will install before deleting
        // the source, including exact object-version references.
        crate::storage_format::Manifest::parse(
            std::str::from_utf8(&bytes).expect("serialized JSON"),
            &destination,
        )?;
        plan.writes.insert(destination.clone(), bytes);
        plan.removes.insert(source.clone());
        plan.report.converted.push(Conversion {
            source,
            destination,
        });
    }
    if !plan.report.converted.is_empty() {
        reject_pending_transactions(repo)?;
    }
    if !plan.report.converted.is_empty() {
        verify_s3_bindings(repo, &plan)?;
    }
    plan.report.removed = plan.removes.iter().cloned().collect();
    if plan.legacy_directory {
        plan.report.removed.push(".dvc/".to_owned());
    }
    plan.report.retained.extend(plan.moves.clone());
    Ok(plan)
}

fn legacy_controls(repo: &GitRepo, plan: &mut Plan) -> Result<()> {
    let root = repo.root.join(".dvc");
    if fs::symlink_metadata(&root).is_ok() {
        reject_symlink_traversal(&repo.root, ".dvc", "legacy storage controls")?;
        if !root.is_dir() {
            return Err(Error::message(
                "legacy .dvc control path must be a directory",
            ));
        }
        plan.legacy_directory = true;
        for entry in fs::read_dir(&root).at(&root)? {
            let entry = entry.at(&root)?;
            let name = entry.file_name().to_string_lossy().to_string();
            match name.as_str() {
                "config" | "config.local" | ".gitignore" => {
                    require_regular(repo, &format!(".dvc/{name}"))?;
                }
                "cache" | "tmp" => {
                    let source = format!(".dvc/{name}");
                    reject_symlink_traversal(&repo.root, &source, "legacy local storage")?;
                    if !entry.path().is_dir() {
                        return Err(Error::message(format!(
                            "legacy local storage {source:?} is not a directory"
                        )));
                    }
                    // Rename whole trees: no payload deletion, copying, upload,
                    // credentials access or traversal into unknown contents.
                    let destination = if name == "cache" {
                        ".workspace-mgr/local/cache"
                    } else {
                        ".workspace-mgr/local/retained-storage-state"
                    }
                    .to_owned();
                    reject_symlink_traversal(&repo.root, &destination, "retained local storage")?;
                    plan.moves.push(Retention {
                        source,
                        destination,
                    });
                }
                _ => {
                    return Err(Error::message(format!(
                        "unknown legacy control .dvc/{name}; manage refuses to delete or reinterpret it"
                    )));
                }
            }
        }
    }
    let mut settings = BTreeMap::new();
    let mut selected = None;
    let mut sections = BTreeMap::<String, BTreeMap<String, String>>::new();
    let mut credential_sources = Vec::new();
    for path in [".dvc/config", ".dvc/config.local"] {
        if repo.root.join(path).exists() {
            let raw = fs::read_to_string(repo.root.join(path)).at(repo.root.join(path))?;
            record_expected(plan, path, Some(raw.as_bytes().to_vec()))?;
            parse_config(&raw, path, &mut sections)?;
            if raw
                .lines()
                .filter_map(|line| line.split_once('='))
                .any(|(key, _)| {
                    matches!(
                        key.trim(),
                        "access_key_id"
                            | "secret_access_key"
                            | "session_token"
                            | "profile"
                            | "region"
                            | "credential_process"
                    )
                })
            {
                credential_sources.push(path.to_owned());
            }
            plan.removes.insert(path.to_owned());
        }
    }
    if let Some(core) = sections.remove("core") {
        for (key, value) in core {
            if key != "remote" {
                return Err(Error::message(format!(
                    "unsupported legacy core setting {key:?}"
                )));
            }
            selected = Some(value);
        }
    }
    plan.selected_remote = selected.clone();
    let mut selected_remote_present = false;
    if !sections.is_empty() {
        let selected = selected.ok_or_else(|| {
            Error::message(
                "legacy configuration has remote sections without one selected core.remote",
            )
        })?;
        let section = format!("remote \"{selected}\"");
        settings = sections
            .remove(&section)
            .ok_or_else(|| Error::message("selected legacy storage remote does not exist"))?;
        selected_remote_present = true;
        if !sections.is_empty() {
            return Err(Error::message(
                "legacy configuration contains other remotes or custom sections; select and remove unsupported configuration explicitly before manage",
            ));
        }
    }
    let supported = [
        "url",
        "endpointurl",
        "version_aware",
        "access_key_id",
        "secret_access_key",
        "session_token",
        "profile",
        "region",
        "credential_process",
    ];
    for key in settings.keys() {
        if !supported.contains(&key.as_str()) {
            return Err(Error::message(format!(
                "unsupported legacy remote setting {key:?}; manage will not silently drop it"
            )));
        }
    }
    if let Some(value) = settings.get("version_aware")
        && value != "true"
        && value != "false"
    {
        return Err(Error::message("legacy version_aware must be true or false"));
    }
    plan.legacy_version_aware = settings
        .get("version_aware")
        .is_some_and(|value| value == "true");
    plan.inferred_url = settings.get("url").cloned();
    plan.inferred_endpoint = settings.get("endpointurl").cloned();
    let native_config = read_config_snapshot(repo, plan)?;
    let native_s3 = native_config
        .as_ref()
        .and_then(|config| config.s3.as_ref())
        .is_some_and(|s3| s3.url.starts_with("s3://"));
    if let Some(config) = native_config {
        if let Some(url) = &plan.inferred_url {
            let s3 = config.s3.as_ref().ok_or_else(|| {
                Error::message(
                    "legacy S3 location conflicts with storage disabled in .workspace-mgr.toml",
                )
            })?;
            if *url != s3.url || plan.inferred_endpoint != s3.endpoint_url {
                return Err(Error::message(
                    "legacy S3 location differs from authoritative .workspace-mgr.toml; reconcile it before migration",
                ));
            }
        }
        if !plan.report.converted.is_empty() && config.s3.is_none() {
            return Err(Error::message(
                "legacy pointers require an S3 location in .workspace-mgr.toml",
            ));
        }
    } else if !plan.report.converted.is_empty() && plan.inferred_url.is_none() {
        return Err(Error::message(
            "legacy pointers have no selected storage URL; configure the legacy remote before manage",
        ));
    }
    // An unbound pointer in a native repository does not prove a CAS layout.
    // Only a selected, actual legacy remote with DVC's non-version-aware
    // setting/default establishes that repository-level storage convention.
    plan.legacy_cas_proven = selected_remote_present
        && !plan.legacy_version_aware
        && (native_s3
            || plan
                .inferred_url
                .as_ref()
                .is_some_and(|url| url.starts_with("s3://")));
    let credentials = crate::native_s3::CredentialsConfig::from_legacy_settings(&settings)?;
    plan.credentials = credentials.clone();
    let rendered = credentials.render()?;
    if !rendered.trim().is_empty() {
        let path = crate::native_s3::CREDENTIALS_NAME;
        reject_symlink_traversal(&repo.root, path, "private storage credentials")?;
        if fs::symlink_metadata(repo.root.join(path)).is_ok() {
            let actual = fs::read_to_string(repo.root.join(path)).at(repo.root.join(path))?;
            record_expected(plan, path, Some(actual.as_bytes().to_vec()))?;
            if actual != rendered {
                return Err(Error::message(
                    "private native credential file already exists with different settings; reconcile it before migration",
                ));
            }
        } else {
            record_expected(plan, path, None)?;
            plan.writes
                .insert(path.to_owned(), rendered.clone().into_bytes());
        }
        for source in credential_sources {
            plan.report.retained.push(Retention {
                source,
                destination: path.to_owned(),
            });
        }
    }
    if rendered.trim().is_empty() && repo.root.join(crate::native_s3::CREDENTIALS_NAME).exists() {
        let path = crate::native_s3::CREDENTIALS_NAME;
        require_regular(repo, path)?;
        let raw = fs::read_to_string(repo.root.join(path)).at(repo.root.join(path))?;
        record_expected(plan, path, Some(raw.as_bytes().to_vec()))?;
        plan.credentials = toml::from_str(&raw).map_err(|_| {
            Error::message(
                "invalid private native credential configuration; use supported flat TOML fields",
            )
        })?;
        plan.credentials.render()?;
    }
    // Native commands run before migration may already own the cache root.
    // The legacy layout stays readable below `legacy/`, as for CAS imports.
    let native_cache = fs::symlink_metadata(repo.root.join(".workspace-mgr/local/cache"))
        .is_ok_and(|metadata| metadata.is_dir());
    for item in &mut plan.moves {
        if item.source == ".dvc/cache" && (plan.legacy_cas_proven || native_cache) {
            item.destination = ".workspace-mgr/local/cache/legacy".to_owned();
        }
        reject_symlink_traversal(&repo.root, &item.destination, "retained local storage")?;
        if fs::symlink_metadata(repo.root.join(&item.destination)).is_ok() {
            return Err(Error::message(format!(
                "retained local storage destination already exists: {:?}; relocate or reconcile that cache before migration",
                item.destination
            )));
        }
    }
    if repo.root.join(".dvc/.gitignore").exists() {
        let raw = fs::read_to_string(repo.root.join(".dvc/.gitignore"))
            .at(repo.root.join(".dvc/.gitignore"))?;
        record_expected(plan, ".dvc/.gitignore", Some(raw.as_bytes().to_vec()))?;
        let permitted = ["/config.local", "/tmp", "/cache"];
        if raw.lines().any(|line| {
            !line.trim().is_empty()
                && !line.trim().starts_with('#')
                && !permitted.contains(&line.trim())
        }) {
            return Err(Error::message(
                "legacy .dvc/.gitignore contains custom rules; preserve them explicitly before manage",
            ));
        }
        plan.removes.insert(".dvc/.gitignore".to_owned());
    }
    if repo.root.join(".dvcignore").exists() {
        require_regular(repo, ".dvcignore")?;
        let raw =
            fs::read_to_string(repo.root.join(".dvcignore")).at(repo.root.join(".dvcignore"))?;
        record_expected(plan, ".dvcignore", Some(raw.as_bytes().to_vec()))?;
        if raw
            .lines()
            .any(|line| !line.trim().is_empty() && !line.trim().starts_with('#'))
        {
            return Err(Error::message(
                "legacy .dvcignore contains custom payload selection rules; resolve those rules before manage",
            ));
        }
        plan.removes.insert(".dvcignore".to_owned());
    }
    if repo.root.join(".gitattributes").exists() {
        require_regular(repo, ".gitattributes")?;
        let raw = fs::read_to_string(repo.root.join(".gitattributes"))
            .at(repo.root.join(".gitattributes"))?;
        record_expected(plan, ".gitattributes", Some(raw.as_bytes().to_vec()))?;
        let mut changed = false;
        let mut retained = String::new();
        for line in raw.split_inclusive('\n') {
            if line.trim() == "*.dvc whitespace=-blank-at-eol" {
                changed = true;
            } else if line
                .split_whitespace()
                .next()
                .is_some_and(|rule| rule.contains(".dvc"))
            {
                return Err(Error::message(
                    "custom DVC .gitattributes rules require explicit migration before manage",
                ));
            } else {
                retained.push_str(line);
            }
        }
        if changed {
            if retained.trim().is_empty() {
                plan.removes.insert(".gitattributes".to_owned());
            } else {
                plan.writes
                    .insert(".gitattributes".to_owned(), retained.into_bytes());
            }
        }
    }
    Ok(())
}

fn verify_s3_bindings(repo: &GitRepo, plan: &Plan) -> Result<()> {
    let url = match plan
        .expected_before
        .get(CONFIG_NAME)
        .and_then(Option::as_ref)
    {
        Some(bytes) => {
            let raw = std::str::from_utf8(bytes)
                .map_err(|_| Error::message("repository configuration is not UTF-8"))?;
            Config::parse(raw, &repo.root.join(CONFIG_NAME))?
                .s3
                .map(|s3| s3.url)
        }
        None => plan.inferred_url.clone(),
    };
    let url = url.ok_or_else(|| {
        Error::message("legacy storage pointers require a selected storage URL before manage")
    })?;
    if url.starts_with("s3://") {
        if plan.import.is_some() {
            return Ok(());
        }
        if !plan.legacy_version_aware {
            return Err(Error::message(
                "legacy S3 metadata has neither proven content-addressed DVC remote configuration nor version_aware = true path-based bindings; restore its original .dvc/config and remote convention before manage instead of guessing missing versions",
            ));
        }
        for conversion in &plan.report.converted {
            let raw = std::str::from_utf8(
                plan.writes
                    .get(&conversion.destination)
                    .expect("planned conversion"),
            )
            .expect("serialized native manifest");
            let manifest = crate::storage_format::Manifest::parse(raw, &conversion.destination)?;
            let exact = match manifest.kind {
                crate::storage_format::Kind::File => manifest
                    .version
                    .as_ref()
                    .is_some_and(|version| !version.id.trim().is_empty() && version.id != "null"),
                crate::storage_format::Kind::Directory => manifest
                    .entries
                    .as_ref()
                    .expect("validated directory")
                    .iter()
                    .all(|entry| {
                        entry.version.as_ref().is_some_and(|version| {
                            !version.id.trim().is_empty() && version.id != "null"
                        })
                    }),
            };
            if !exact {
                return Err(Error::message(format!(
                    "legacy pointer {:?} lacks exact S3 object versions; resolve its path-based bindings with the previous CLI before manage",
                    conversion.source
                )));
            }
        }
    }
    Ok(())
}

fn reject_pending_transactions(repo: &GitRepo) -> Result<()> {
    let local = crate::local_state::directory_unmigrated(repo)?;
    require_primary_checkout(repo)?;
    let mut directories = vec![local.clone()];
    directories.extend(crate::local_state::legacy_directories(repo)?);
    for state_directory in directories {
        reject_symlink_path(&state_directory)?;
        let mut terminal_sources = BTreeSet::new();
        for name in [
            "archive-attempts",
            "archive",
            "uploads",
            "archive-reservations",
        ] {
            let directory = state_directory.join(name);
            if !directory.exists() {
                continue;
            }
            reject_symlink_path(&directory)?;
            for entry in fs::read_dir(&directory).at(&directory)? {
                let path = entry.at(&directory)?.path();
                if path.extension().and_then(|value| value.to_str()) != Some("json") {
                    continue;
                }
                reject_symlink_path(&path)?;
                let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).at(&path)?)
                    .map_err(|_| {
                        Error::message(format!(
                            "cannot verify private {name} journal before storage migration"
                        ))
                    })?;
                let inactive = match name {
                    "archive-attempts" => {
                        let terminal =
                            matches!(value["status"].as_str(), Some("published" | "cancelled"));
                        if terminal && let Some(source) = value["source"].as_str() {
                            terminal_sources.insert(source.to_owned());
                        }
                        terminal
                    }
                    "archive" => matches!(value["status"].as_str(), Some("copied" | "cancelled")),
                    "uploads" => value["phase"] == "complete",
                    "archive-reservations" => {
                        value["acquired"] == false
                            || value["receipt"]["source"]
                                .as_str()
                                .is_some_and(|source| terminal_sources.contains(source))
                    }
                    _ => false,
                };
                if !inactive {
                    return Err(Error::message(format!(
                        "legacy storage migration refuses an active private {name} transaction; finish publication or cancel the owning archive/upload with the previous CLI before manage"
                    )));
                }
            }
        }
    }
    if crate::s3_purge::has_pending_read_only(repo)? {
        return Err(Error::message(
            "legacy storage migration refuses pending S3 deletion transactions; finish workspace-mgr refresh or publish with the previous CLI before manage",
        ));
    }
    let legacy_uploads = repo.root.join(".dvc/tmp/native-uploads");
    if legacy_uploads.exists() {
        reject_symlink_path(&legacy_uploads)?;
        for entry in fs::read_dir(&legacy_uploads).at(&legacy_uploads)? {
            let path = entry.at(&legacy_uploads)?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            reject_symlink_path(&path)?;
            let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).at(&path)?)
                .map_err(|_| {
                    Error::message("cannot verify legacy storage upload journal before migration")
                })?;
            if value["phase"] != "complete" {
                return Err(Error::message(
                    "legacy storage migration refuses an unfinished upload; finish its publication with the previous CLI before manage",
                ));
            }
        }
    }
    Ok(())
}

fn require_primary_checkout(repo: &GitRepo) -> Result<()> {
    let local = crate::local_state::directory_unmigrated(repo)?;
    if local.parent().and_then(Path::parent) != Some(repo.root.as_path()) {
        return Err(Error::message(
            "legacy storage migration must run in the primary shared checkout, where private storage state and credentials reside",
        ));
    }
    Ok(())
}

fn parse_config(
    raw: &str,
    origin: &str,
    sections: &mut BTreeMap<String, BTreeMap<String, String>>,
) -> Result<()> {
    let mut section = None;
    let mut seen = BTreeSet::new();
    for line in raw.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            let name = name.trim().trim_matches('\'').to_owned();
            if name != "core" && !(name.starts_with("remote \"") && name.ends_with('"')) {
                return Err(Error::message(format!(
                    "unsupported configuration section in {origin}"
                )));
            }
            section = Some(name);
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| Error::message(format!("invalid configuration setting in {origin}")))?;
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        let value = if value.starts_with('"') || value.ends_with('"') {
            if value.len() < 2 || !value.starts_with('"') || !value.ends_with('"') {
                return Err(Error::message(format!(
                    "ambiguous quoted configuration value in {origin}"
                )));
            }
            &value[1..value.len() - 1]
        } else {
            value
        };
        let section = section.as_ref().ok_or_else(|| {
            Error::message(format!("configuration setting outside section in {origin}"))
        })?;
        if value.is_empty() || !seen.insert((section.clone(), key.clone())) {
            return Err(Error::message(format!(
                "ambiguous configuration setting in {origin}"
            )));
        }
        sections
            .entry(section.clone())
            .or_default()
            .insert(key, value.to_owned());
    }
    Ok(())
}

fn require_regular(repo: &GitRepo, path: &str) -> Result<()> {
    reject_symlink_traversal(&repo.root, path, "migration control file")?;
    let absolute = repo.root.join(path);
    let meta = fs::symlink_metadata(&absolute).at(&absolute)?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(Error::message(format!(
            "migration control must be a regular file: {path:?}"
        )));
    }
    Ok(())
}

fn reject_symlink_path(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(Error::message(
            "private migration journal must not be a symlink",
        ));
    }
    Ok(())
}

fn execute(repo: &GitRepo, journal_path: &Path, plan: &Plan) -> Result<()> {
    for (path, expected) in &plan.expected_before {
        verify_expected_file(repo, path, expected)?;
    }
    for item in &plan.moves {
        reject_symlink_traversal(&repo.root, &item.source, "legacy cache move")?;
        reject_symlink_traversal(&repo.root, &item.destination, "native cache move")?;
        if !repo.root.join(&item.source).is_dir()
            || fs::symlink_metadata(repo.root.join(&item.destination)).is_ok()
        {
            return Err(Error::message(
                "local cache paths changed after manage preflight; retry after their writer finishes",
            ));
        }
    }
    let mut changes = Vec::new();
    for path in plan
        .writes
        .keys()
        .chain(plan.removes.iter())
        .collect::<BTreeSet<_>>()
    {
        reject_symlink_traversal(&repo.root, path, "manage transaction path")?;
        let before = plan
            .expected_before
            .get(path)
            .ok_or_else(|| {
                Error::message(format!(
                    "manage plan omitted its input snapshot for {path:?}"
                ))
            })?
            .clone();
        changes.push(FileChange {
            path: path.clone(),
            before,
            after: plan.writes.get(path).cloned(),
        });
    }
    let journal = Journal {
        schema_version: 1,
        root: repo.root.clone(),
        changes,
        moves: plan
            .moves
            .iter()
            .map(|item| DirectoryMove {
                source: item.source.clone(),
                destination: item.destination.clone(),
            })
            .collect(),
        legacy_directory: plan.legacy_directory,
    };
    let bytes = serde_json::to_vec(&journal)
        .map_err(|error| Error::message(format!("serialize manage journal: {error}")))?;
    write_recovery_journal(repo, journal_path, &bytes)?;
    let result = (|| {
        for (path, bytes) in &plan.writes {
            atomic_write(
                &repo.root.join(path),
                bytes,
                path.starts_with(".workspace-mgr/local/"),
            )?;
        }
        for item in &plan.moves {
            let destination = repo.root.join(&item.destination);
            fs::create_dir_all(destination.parent().expect("local cache parent"))
                .at(&destination)?;
            fs::rename(repo.root.join(&item.source), &destination).at(&destination)?;
            sync_parent(&destination)?;
            sync_parent(&repo.root.join(&item.source))?;
        }
        // Verify every target before removing even the first source pointer.
        for (path, bytes) in &plan.writes {
            if fs::read(repo.root.join(path)).at(repo.root.join(path))? != *bytes {
                return Err(Error::message(format!(
                    "manage verification failed at {path:?}"
                )));
            }
        }
        for path in &plan.removes {
            let expected = plan
                .expected_before
                .get(path)
                .expect("checked input snapshot");
            verify_expected_file(repo, path, expected)?;
            fs::remove_file(repo.root.join(path)).at(repo.root.join(path))?;
            sync_parent(&repo.root.join(path))?;
        }
        if plan.legacy_directory {
            fs::remove_dir(repo.root.join(".dvc")).at(repo.root.join(".dvc"))?;
            sync_parent(&repo.root.join(".dvc"))?;
        }
        fs::remove_file(journal_path).at(journal_path)?;
        sync_parent(journal_path)?;
        Ok(())
    })();
    if let Err(error) = result {
        recover(repo, journal_path).map_err(|recovery| Error::message(format!("manage failed: {error}; recovery also failed: {recovery}; keep the private journal and rerun manage after resolving the blocker")))?;
        return Err(error);
    }
    Ok(())
}

fn write_recovery_journal(repo: &GitRepo, path: &Path, bytes: &[u8]) -> Result<()> {
    // The first durable snapshot can contain legacy credentials before the
    // public scaffold ignore file exists. Protect it independently so a crash
    // at this boundary cannot expose private state to a later `git add`.
    crate::storage_import::protect_private_state(repo)?;
    atomic_write(path, bytes, true)
}

pub(crate) fn verify_expected_file(
    repo: &GitRepo,
    relative: &str,
    expected: &Option<Vec<u8>>,
) -> Result<()> {
    reject_symlink_traversal(&repo.root, relative, "manage input snapshot")?;
    let path = repo.root.join(relative);
    let actual = match fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => return Err(Error::Io { path, source }),
    };
    if actual != *expected {
        return Err(Error::message(format!(
            "manage input {relative:?} changed after preflight; preserve the edit and retry after its writer finishes"
        )));
    }
    Ok(())
}

fn recover(repo: &GitRepo, journal_path: &Path) -> Result<()> {
    let raw = fs::read(journal_path).at(journal_path)?;
    let journal: Journal = serde_json::from_slice(&raw).map_err(|_| {
        Error::message("private manage recovery journal is invalid; retain it and resolve manually")
    })?;
    if journal.schema_version != 1 || journal.root != repo.root {
        return Err(Error::message(
            "private manage recovery journal belongs to another repository or version",
        ));
    }
    // Protect any user edit made after interruption, before restoring anything.
    for change in &journal.changes {
        if repo_path(&change.path, "recovery path")? != change.path {
            return Err(Error::message("invalid path in manage recovery journal"));
        }
        let control = matches!(
            change.path.as_str(),
            CONFIG_NAME
                | "AGENTS.md"
                | ".gitignore"
                | ".gitattributes"
                | ".dvcignore"
                | ".dvc/config"
                | ".dvc/config.local"
                | ".dvc/.gitignore"
                | ".workspace-mgr/repository.gitignore"
                | crate::native_s3::CREDENTIALS_NAME
        );
        if !control
            && !change.path.ends_with(".dvc")
            && !change.path.ends_with(crate::storage_format::SUFFIX)
        {
            return Err(Error::message(
                "manage recovery journal names a path outside its storage and scaffold controls",
            ));
        }
        reject_symlink_traversal(&repo.root, &change.path, "manage recovery path")?;
        let path = repo.root.join(&change.path);
        let current = match fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => return Err(Error::Io { path, source }),
        };
        if current != change.before && current != change.after {
            return Err(Error::message(format!(
                "manage recovery found a subsequent edit at {:?}; preserve or reconcile it before retrying",
                change.path
            )));
        }
    }
    for item in &journal.moves {
        repo_path(&item.source, "recovery source")?;
        repo_path(&item.destination, "recovery destination")?;
        if !matches!(
            (item.source.as_str(), item.destination.as_str()),
            (".dvc/cache", ".workspace-mgr/local/cache")
                | (".dvc/cache", ".workspace-mgr/local/cache/legacy")
                | (".dvc/tmp", ".workspace-mgr/local/retained-storage-state")
        ) {
            return Err(Error::message(
                "manage recovery journal names an unsupported directory move",
            ));
        }
        reject_symlink_traversal(&repo.root, &item.source, "manage recovery source")?;
        reject_symlink_traversal(&repo.root, &item.destination, "manage recovery destination")?;
        let source = repo.root.join(&item.source);
        let destination = repo.root.join(&item.destination);
        for path in [&source, &destination] {
            if path.exists() && !path.is_dir() {
                return Err(Error::message(
                    "manage recovery cache path is not a directory",
                ));
            }
        }
        if destination.exists() {
            if source.exists() {
                return Err(Error::message(
                    "both retained-cache recovery paths exist; resolve the collision before retrying",
                ));
            }
        } else if !source.exists() {
            return Err(Error::message(
                "both retained-cache recovery paths are missing; keep the journal and recover the cache before retrying",
            ));
        }
    }
    if journal.legacy_directory {
        reject_symlink_traversal(&repo.root, ".dvc", "manage recovery directory")?;
        fs::create_dir_all(repo.root.join(".dvc")).at(repo.root.join(".dvc"))?;
    }
    for item in journal.moves.iter().rev() {
        let source = repo.root.join(&item.source);
        let destination = repo.root.join(&item.destination);
        if destination.exists() {
            fs::create_dir_all(source.parent().expect("legacy parent")).at(&source)?;
            fs::rename(&destination, &source).at(&source)?;
            sync_parent(&source)?;
            sync_parent(&destination)?;
        }
    }
    for change in journal.changes.iter().rev() {
        let path = repo.root.join(&change.path);
        if let Some(bytes) = &change.before {
            atomic_write(
                &path,
                bytes,
                change.path == ".dvc/config.local"
                    || change.path.starts_with(".workspace-mgr/local/"),
            )?;
        } else if path.exists() {
            fs::remove_file(&path).at(&path)?;
            sync_parent(&path)?;
        }
    }
    fs::remove_file(journal_path).at(journal_path)?;
    sync_parent(journal_path)
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8], private: bool) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::message("manage output has no parent"))?;
    fs::create_dir_all(parent).at(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).at(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if private {
            0o600
        } else {
            fs::metadata(path)
                .map(|meta| meta.permissions().mode())
                .unwrap_or(0o644)
        };
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(mode))
            .at(path)?;
    }
    #[cfg(not(unix))]
    let _ = private;
    temporary.write_all(bytes).at(path)?;
    temporary.as_file().sync_all().at(path)?;
    temporary.persist(path).map_err(|error| Error::Io {
        path: path.to_path_buf(),
        source: error.error,
    })?;
    sync_parent(path)
}

fn sync_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        File::open(parent).at(parent)?.sync_all().at(parent)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_metadata_only_journal_keeps_credentials_ignored_before_scaffold_writes() {
        let temp = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: temp.path().canonicalize().unwrap(),
        };
        repo.run(["init", "-b", "main"]).unwrap();
        fs::create_dir(repo.root.join(".dvc")).unwrap();
        fs::write(repo.root.join(".dvc/.gitignore"), "/config.local\n").unwrap();
        let credentials = b"legacy-secret-key\n";
        fs::write(repo.root.join(".dvc/config.local"), credentials).unwrap();
        let journal = Journal {
            schema_version: 1,
            root: repo.root.clone(),
            changes: vec![FileChange {
                path: ".dvc/config.local".to_owned(),
                before: Some(credentials.to_vec()),
                after: None,
            }],
            moves: Vec::new(),
            legacy_directory: true,
        };
        let path = crate::local_state::directory_unmigrated(&repo)
            .unwrap()
            .join(JOURNAL_NAME);
        // Stop exactly where an interrupted process leaves its first durable
        // snapshot, before any root .gitignore or native scaffold is written.
        write_recovery_journal(&repo, &path, &serde_json::to_vec(&journal).unwrap()).unwrap();
        assert!(!repo.root.join(".gitignore").exists());
        let recorded: Journal = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            recorded.changes[0].before.as_deref(),
            Some(credentials.as_slice())
        );
        assert!(
            repo.run_unchecked([
                "check-ignore",
                "--no-index",
                ".workspace-mgr/local/storage-migration.json",
            ])
            .unwrap()
            .success()
        );
        assert!(
            !repo
                .run(["ls-files", "--others", "--exclude-standard"])
                .unwrap()
                .stdout
                .contains(".workspace-mgr/local/")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        recover(&repo, &path).unwrap();
        assert_eq!(
            fs::read(repo.root.join(".dvc/config.local")).unwrap(),
            credentials
        );
        assert!(!path.exists());
        assert!(!repo.root.join(".gitignore").exists());
    }

    #[test]
    fn native_route_without_legacy_layout_proof_does_not_guess_cas() {
        let temp = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: temp.path().canonicalize().unwrap(),
        };
        repo.run(["init", "-b", "main"]).unwrap();
        let config = "[git]\nremote = \"origin\"\nbranch = \"main\"\n[s3]\nurl = \"s3://offline.invalid/repository\"\n";
        fs::write(repo.root.join(CONFIG_NAME), config).unwrap();
        fs::write(repo.root.join("data.dvc"), "outs:\n- path: data\n  hash: md5\n  md5: 900150983cd24fb0d6963f7d28e17f72\n  size: 3\n").unwrap();
        let error = match preflight(&repo) {
            Ok(_) => panic!("unproven CAS layout was accepted"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains("neither proven content-addressed"),
            "{error}"
        );
        assert!(!repo.root.join(".workspace-mgr").exists());
        assert!(repo.root.join("data.dvc").exists());
        assert!(!repo.root.join("data.wm-storage.json").exists());
    }

    #[test]
    fn proven_cas_using_public_route_retains_old_cache_beside_transfer_cache() {
        let temp = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: temp.path().canonicalize().unwrap(),
        };
        repo.run(["init", "-b", "main"]).unwrap();
        let config = "[git]\nremote = \"origin\"\nbranch = \"main\"\n[s3]\nurl = \"s3://offline.invalid/repository\"\n";
        fs::write(repo.root.join(CONFIG_NAME), config).unwrap();
        fs::create_dir_all(repo.root.join(".dvc/cache")).unwrap();
        fs::create_dir_all(repo.root.join(".workspace-mgr/local/cache/objects")).unwrap();
        fs::write(
            repo.root.join(".dvc/config"),
            "[core]\nremote = research-data\n['remote \"research-data\"']\nversion_aware = false\n",
        )
        .unwrap();
        let mut plan = Plan::default();
        legacy_controls(&repo, &mut plan).unwrap();
        assert!(plan.legacy_cas_proven);
        assert_eq!(plan.moves[0].source, ".dvc/cache");
        assert_eq!(
            plan.moves[0].destination,
            ".workspace-mgr/local/cache/legacy"
        );
        assert!(repo.root.join(".dvc/cache").is_dir());
        assert!(!repo.root.join(".workspace-mgr/local/cache/legacy").exists());
    }

    #[test]
    fn version_aware_cache_moves_below_an_existing_native_cache() {
        let temp = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: temp.path().canonicalize().unwrap(),
        };
        repo.run(["init", "-b", "main"]).unwrap();
        let config = "[git]\nremote = \"origin\"\nbranch = \"main\"\n[s3]\nurl = \"s3://offline.invalid/repository\"\n";
        fs::write(repo.root.join(CONFIG_NAME), config).unwrap();
        fs::create_dir_all(repo.root.join(".dvc/cache/files")).unwrap();
        fs::write(
            repo.root.join(".dvc/config"),
            "[core]\nremote = workspace-mgr\n['remote \"workspace-mgr\"']\nurl = s3://offline.invalid/repository\nversion_aware = true\n",
        )
        .unwrap();
        let mut plan = Plan::default();
        legacy_controls(&repo, &mut plan).unwrap();
        assert!(!plan.legacy_cas_proven);
        assert_eq!(plan.moves[0].destination, ".workspace-mgr/local/cache");
        // A native command such as refresh created the cache root first.
        fs::create_dir_all(repo.root.join(".workspace-mgr/local/cache/objects")).unwrap();
        let mut plan = Plan::default();
        legacy_controls(&repo, &mut plan).unwrap();
        assert_eq!(
            plan.moves[0].destination,
            ".workspace-mgr/local/cache/legacy"
        );
    }

    #[test]
    fn edited_source_or_new_destination_refuses_before_journal_and_writes() {
        for scenario in ["edited-source", "destination-collision"] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let repo = GitRepo { root: root.clone() };
            repo.run(["init", "-b", "main"]).unwrap();
            fs::create_dir(root.join(".dvc")).unwrap();
            let config = "[core]\nremote = workspace-mgr\n['remote \"workspace-mgr\"']\nurl = s3://offline.invalid/repository\nversion_aware = true\n";
            fs::write(root.join(".dvc/config"), config).unwrap();
            let source = "outs:\n- path: data.bin\n  hash: md5\n  md5: 900150983cd24fb0d6963f7d28e17f72\n  size: 3\n  cloud:\n    workspace-mgr:\n      version_id: original-version\n";
            fs::write(root.join("data.bin.dvc"), source).unwrap();
            let options = ManageOptions {
                repo: root.clone(),
                s3_url: None,
                s3_endpoint_url: None,
                dry_run: false,
            };
            let (plan, _) = prepare_plan(&repo, &options).unwrap();
            if scenario == "edited-source" {
                fs::write(
                    root.join("data.bin.dvc"),
                    source.replace("original-version", "new-external-version"),
                )
                .unwrap();
            } else {
                fs::write(root.join("data.bin.wm-storage.json"), "external result").unwrap();
            }
            let journal = root.join(".workspace-mgr/local/storage-migration.json");
            let error = execute(&repo, &journal, &plan).unwrap_err().to_string();
            assert!(error.contains("changed after preflight"), "{error}");
            assert!(!journal.exists());
            assert!(!root.join(CONFIG_NAME).exists());
            assert!(!root.join("AGENTS.md").exists());
            assert_eq!(
                fs::read_to_string(root.join(".dvc/config")).unwrap(),
                config
            );
            assert!(root.join("data.bin.dvc").exists());
            if scenario == "edited-source" {
                assert!(
                    fs::read_to_string(root.join("data.bin.dvc"))
                        .unwrap()
                        .contains("new-external-version")
                );
                assert!(!root.join("data.bin.wm-storage.json").exists());
            } else {
                assert_eq!(
                    fs::read_to_string(root.join("data.bin.wm-storage.json")).unwrap(),
                    "external result"
                );
            }
        }
    }
}
