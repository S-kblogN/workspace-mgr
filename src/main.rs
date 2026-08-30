use std::path::{Path, PathBuf};

use clap::Parser;

use workspace_mgr::cli::{
    Cli, Command, ConfigCommand, PublishArgs, PublishCommandArgs, RequiredPublishArgs, TaskCommand,
};
use workspace_mgr::config::Config;
use workspace_mgr::dvc;
use workspace_mgr::error::{Error, Result};
use workspace_mgr::git::GitRepo;
use workspace_mgr::instructions;
use workspace_mgr::manifest::{AdditionalScope, ResolvedTask, one_line};
use workspace_mgr::output::{Format, print_human, print_json};
use workspace_mgr::path::repo_path;
use workspace_mgr::refresh::{RefreshOptions, execute as refresh};
use workspace_mgr::scaffold::{InitOptions, TaskCreateOptions, create_task, init};
use workspace_mgr::transaction::{Operation, TransactionOptions, execute as transact, task_status};

fn main() {
    let cli = Cli::parse();
    if let Err(error) = run(cli) {
        eprintln!("workspace-mgr: {error}");
        std::process::exit(2);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Init(args) => {
            let report = init(&InitOptions {
                repo: args.repo,
                profile: args.profile,
                storage_url: args.storage_url,
                storage_endpoint_url: args.storage_endpoint_url,
                require_object_versioning: args.require_object_versioning,
                adopt: args.adopt,
                dry_run: args.dry_run,
            })?;
            emit(&report, cli.format)
        }
        Command::Instructions(args) => {
            let repo = GitRepo::discover(&args.repo)?;
            let config = Config::load(&repo)?;
            let document = instructions::render(&repo, &config, args.topic.as_deref())?;
            if cli.format == Format::Json {
                print_json(&document)
            } else {
                print!("{}", document.markdown);
                Ok(())
            }
        }
        Command::Doctor(args) => {
            let report = workspace_mgr::doctor::inspect(&args.repo)?;
            if cli.format == Format::Json {
                print_json(&report)?;
            } else {
                println!("workspace-mgr doctor: {}", report.status);
                for check in &report.checks {
                    println!("{:<5} {:<28} {}", check.status, check.name, check.detail);
                }
            }
            if !report.healthy() {
                return Err(Error::message("doctor found one or more errors"));
            }
            Ok(())
        }
        Command::Config(args) => match args.command {
            ConfigCommand::Show(args) => {
                let repo = GitRepo::discover(&args.repo)?;
                let config = Config::load(&repo)?;
                if cli.format == Format::Json {
                    print_json(&config)
                } else {
                    print!("{}", config.render()?);
                    Ok(())
                }
            }
        },
        Command::Task(args) => match args.command {
            TaskCommand::Create(args) => emit(
                &create_task(&TaskCreateOptions {
                    repo: args.repo,
                    slug: args.slug,
                    title: args.title,
                    purpose: args.purpose,
                    branch: args.branch,
                    timestamp: args.timestamp,
                    dry_run: args.dry_run,
                })?,
                cli.format,
            ),
            TaskCommand::Status(args) => emit(
                &task_status(&args.repo, args.manifest.as_deref())?,
                cli.format,
            ),
        },
        Command::Plan(args) => run_transaction(args, Operation::Plan, Vec::new(), cli.format),
        Command::Publish(args) => run_publish(args, cli.format),
        Command::Track(args) => {
            run_required_transaction(args.publish, Operation::Track, args.paths, cli.format)
        }
        Command::Move(args) => run_required_transaction(
            args.publish,
            Operation::Move,
            vec![args.old_path, args.new_path],
            cli.format,
        ),
        Command::Untrack(args) => {
            run_required_transaction(args.publish, Operation::Untrack, args.targets, cli.format)
        }
        Command::Hydrate(args) => {
            let repo = GitRepo::discover(&args.repo)?;
            let config = Config::load(&repo)?;
            let manifest_path = match args.manifest {
                Some(path) => path,
                None => ResolvedTask::discover(&repo, &config, &args.repo)?,
            };
            let task = ResolvedTask::load(&repo, &config, &manifest_path)?;
            let (scopes, _) = hydrate_scopes(&task, &args.include, args.scope_note.as_deref())?;
            let report = dvc::hydrate(&repo, &config, &scopes, &args.targets, args.dry_run)?;
            emit(&report, cli.format)
        }
        Command::Refresh(args) => emit(
            &refresh(&RefreshOptions {
                repo: args.repo,
                remote: args.remote,
                branch: args.branch,
                dry_run: args.dry_run,
                git_only: args.git_only,
                scope_note: args.scope_note,
            })?,
            cli.format,
        ),
    }
}

fn run_publish(args: PublishCommandArgs, format: Format) -> Result<()> {
    run_transaction(
        PublishArgs {
            manifest: args.manifest,
            message: Some(args.message),
            include: args.include,
            scope_note: args.scope_note,
            allow_non_shared_head: args.allow_non_shared_head,
            dry_run: args.dry_run,
            git_only: args.git_only,
            repo: args.repo,
        },
        Operation::Publish,
        Vec::new(),
        format,
    )
}

fn run_transaction(
    args: PublishArgs,
    operation: Operation,
    management_paths: Vec<String>,
    format: Format,
) -> Result<()> {
    emit(
        &transact(&TransactionOptions {
            start: args.repo,
            manifest: args.manifest,
            message: args.message,
            include: args.include,
            scope_note: args.scope_note,
            allow_non_shared_head: args.allow_non_shared_head,
            git_only: args.git_only,
            dry_run: args.dry_run,
            operation,
            management_paths,
        })?,
        format,
    )
}

fn run_required_transaction(
    args: RequiredPublishArgs,
    operation: Operation,
    management_paths: Vec<String>,
    format: Format,
) -> Result<()> {
    run_transaction(
        PublishArgs {
            manifest: args.manifest,
            message: Some(args.message),
            include: args.include,
            scope_note: args.scope_note,
            allow_non_shared_head: args.allow_non_shared_head,
            dry_run: args.dry_run,
            git_only: false,
            repo: args.repo,
        },
        operation,
        management_paths,
        format,
    )
}

fn hydrate_scopes(
    task: &ResolvedTask,
    includes: &[String],
    scope_note: Option<&str>,
) -> Result<(Vec<String>, Vec<AdditionalScope>)> {
    let mut additional = task.additional_scopes.clone();
    if !includes.is_empty() && scope_note.is_none() {
        return Err(Error::message(
            "--include requires --scope-note describing its authorization",
        ));
    }
    if let Some(reason) = scope_note {
        let reason = one_line(reason, "scope note")?;
        for path in includes {
            additional.push(AdditionalScope {
                path: repo_path(path, "included scope")?,
                reason: reason.clone(),
            });
        }
    }
    let mut scopes = vec![task.task_path.clone()];
    scopes.extend(additional.iter().map(|scope| scope.path.clone()));
    scopes.sort();
    scopes.dedup();
    Ok((scopes, additional))
}

fn emit<T: serde::Serialize>(value: &T, format: Format) -> Result<()> {
    match format {
        Format::Human => print_human(value),
        Format::Json => print_json(value),
    }
}

#[allow(dead_code)]
fn absolute(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}
