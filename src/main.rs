use clap::Parser;

mod cli;
mod config;
mod discard;
mod doctor;
mod dvc;
mod error;
mod git;
mod hex;
mod instructions;
mod lock;
mod manifest;
mod output;
mod path;
mod policy;
mod process;
mod refresh;
mod runtime;
mod scaffold;
mod storage;
mod transaction;
mod update;

use crate::cli::{
    Cli, Command, ConfigCommand, PlanArgs, PublishCommandArgs, ScopedArgs, StorageCommand,
    TaskCommand,
};
use crate::config::Config;
use crate::discard::{TaskDiscardOptions, discard};
use crate::error::{Error, Result};
use crate::git::GitRepo;
use crate::lock::RepositoryLock;
use crate::manifest::{AdditionalScope, ResolvedTask, one_line, validate_additional_scopes};
use crate::output::{Format, print_human, print_json};
use crate::path::repo_path;
use crate::refresh::{RefreshOptions, execute as refresh};
use crate::runtime::{SetupOptions, setup};
use crate::scaffold::{InitOptions, TaskCreateOptions, create_task, init};
use crate::transaction::{Operation, TransactionOptions, execute as transact, task_status};

fn main() {
    update::check_and_warn();
    let cli = Cli::parse();
    if let Err(error) = run(cli) {
        eprintln!("workspace-mgr: {error}");
        std::process::exit(2);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Setup(args) => emit(
            &setup(&SetupOptions {
                runtime_dir: args.runtime_dir,
                dry_run: args.dry_run,
            })?,
            cli.format,
        ),
        Command::Init(args) => {
            let report = init(&InitOptions {
                repo: args.repo,
                s3_url: args.s3_url,
                s3_endpoint_url: args.s3_endpoint_url,
                dry_run: args.dry_run,
            })?;
            emit(&report, cli.format)
        }
        Command::Instructions(args) => {
            let repo = GitRepo::discover(&args.repo)?;
            let config = Config::load_compatible(&repo)?;
            let document = instructions::render(&repo, &config, args.topic.as_deref())?;
            if cli.format == Format::Json {
                print_json(&document)
            } else {
                print!("{}", document.markdown);
                Ok(())
            }
        }
        Command::Doctor(args) => {
            let report = doctor::inspect(&args.repo)?;
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
                let config = Config::load_compatible(&repo)?;
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
                    kind: args.kind,
                    scopes: args.scopes,
                    scope_note: args.scope_note,
                    timestamp: args.timestamp,
                    dry_run: args.dry_run,
                })?,
                cli.format,
            ),
            TaskCommand::Status(args) => emit(
                &task_status(&args.repo, args.manifest.as_deref())?,
                cli.format,
            ),
            TaskCommand::Discard(args) => emit(
                &discard(&TaskDiscardOptions {
                    start: args.repo,
                    manifest: args.manifest,
                    dry_run: args.dry_run,
                    confirm: args.confirm,
                })?,
                cli.format,
            ),
        },
        Command::Plan(args) => run_plan(args, cli.format),
        Command::Publish(args) => run_publish(args, cli.format),
        Command::Storage(args) => match args.command {
            StorageCommand::Status(args) => {
                let (repo, config, scopes, _lock) = scoped_context(&args.scoped, false)?;
                emit(
                    &storage::status(&repo, &config, &scopes, &args.paths)?,
                    cli.format,
                )
            }
            StorageCommand::Set(args) => {
                let (repo, config, scopes, _lock) = scoped_context(&args.scoped, true)?;
                emit(
                    &storage::set(
                        &repo,
                        &config,
                        &scopes,
                        &args.paths,
                        args.to,
                        &args.reason,
                        args.dry_run,
                    )?,
                    cli.format,
                )
            }
            StorageCommand::Reset(args) => {
                let (repo, config, scopes, _lock) = scoped_context(&args.scoped, true)?;
                emit(
                    &storage::reset(&repo, &config, &scopes, &args.paths, args.dry_run)?,
                    cli.format,
                )
            }
            StorageCommand::Hydrate(args) => {
                let (repo, config, scopes, _lock) = scoped_context(&args.scoped, true)?;
                emit(
                    &storage::hydrate(&repo, &config, &scopes, &args.paths, args.dry_run)?,
                    cli.format,
                )
            }
        },
        Command::Move(args) => {
            let (repo, config, scopes, _lock) = scoped_context(&args.scoped, true)?;
            emit(
                &storage::move_path(
                    &repo,
                    &config,
                    &scopes,
                    &args.old_path,
                    &args.new_path,
                    args.dry_run,
                )?,
                cli.format,
            )
        }
        Command::Refresh(args) => emit(
            &refresh(&RefreshOptions {
                repo: args.repo,
                dry_run: args.dry_run,
            })?,
            cli.format,
        ),
    }
}

fn run_plan(args: PlanArgs, format: Format) -> Result<()> {
    emit(
        &transact(&TransactionOptions {
            start: args.repo,
            manifest: args.manifest,
            message: None,
            include: args.include,
            scope_note: args.scope_note,
            allow_non_shared_head: args.allow_non_shared_head,
            dry_run: false,
            operation: Operation::Plan,
        })?,
        format,
    )
}

fn run_publish(args: PublishCommandArgs, format: Format) -> Result<()> {
    emit(
        &transact(&TransactionOptions {
            start: args.repo,
            manifest: args.manifest,
            message: Some(args.message),
            include: args.include,
            scope_note: args.scope_note,
            allow_non_shared_head: args.allow_non_shared_head,
            dry_run: args.dry_run,
            operation: Operation::Publish,
        })?,
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
    let additional = validate_additional_scopes(task.task_path.as_deref(), additional)?;
    let scopes = task
        .task_path
        .iter()
        .cloned()
        .chain(additional.iter().map(|scope| scope.path.clone()))
        .collect();
    Ok((scopes, additional))
}

fn scoped_context(
    args: &ScopedArgs,
    exclusive: bool,
) -> Result<(GitRepo, Config, Vec<String>, Option<RepositoryLock>)> {
    let repo = match &args.manifest {
        Some(path) => GitRepo::discover_for_manifest(path)?,
        None => GitRepo::discover(&args.repo)?,
    };
    let lock = if exclusive {
        Some(RepositoryLock::acquire(&repo)?)
    } else {
        None
    };
    let config = Config::load_compatible(&repo)?;
    let manifest_path = match &args.manifest {
        Some(path) => path.clone(),
        None => ResolvedTask::discover(&repo, &args.repo)?,
    };
    let task = ResolvedTask::load(&repo, &config, &manifest_path)?;
    let (scopes, _) = hydrate_scopes(&task, &args.include, args.scope_note.as_deref())?;
    Ok((repo, config, scopes, lock))
}

fn emit<T: serde::Serialize>(value: &T, format: Format) -> Result<()> {
    match format {
        Format::Human => print_human(value),
        Format::Json => print_json(value),
    }
}
