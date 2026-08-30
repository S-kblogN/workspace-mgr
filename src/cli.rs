use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::config::StorageTarget;
use crate::manifest::TaskKind;
use crate::output::Format;

#[derive(Debug, Parser)]
#[command(
    name = "workspace-mgr",
    version,
    about = "Policy-driven repository workspace manager for coding agents",
    subcommand_required = true,
    arg_required_else_help = true
)]
pub struct Cli {
    #[arg(
        long,
        global = true,
        value_enum,
        default_value = "human",
        env = "WORKSPACE_MGR_FORMAT"
    )]
    pub format: Format,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Provision the private execution runtime used by managed storage.
    Setup(SetupArgs),

    /// Initialize or reconcile repository facts and managed scaffolding.
    Init(InitArgs),

    /// Print the shared workspace model and effective repository instructions.
    Instructions(InstructionsArgs),

    /// Diagnose dependencies, configuration, and repository state.
    Doctor(RepoArgs),

    /// Inspect repository configuration.
    Config(ConfigArgs),

    /// Create or inspect task scaffolding.
    Task(TaskArgs),

    /// Preview a scoped repository transaction without publishing.
    Plan(PlanArgs),

    /// Publish a scoped repository transaction.
    Publish(PublishCommandArgs),

    /// Inspect or change whether content is stored in Git or S3.
    Storage(StorageArgs),

    /// Move a path while preserving its storage placement.
    Move(MoveArgs),

    /// Safely update a shared checkout and hydrate incoming stored data.
    Refresh(RefreshArgs),
}

#[derive(Debug, Args)]
pub struct SetupArgs {
    /// Override the private runtime directory.
    #[arg(long)]
    pub runtime_dir: Option<PathBuf>,

    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct RepoArgs {
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
}

#[derive(Debug, Args)]
pub struct InitArgs {
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,

    /// Configure an S3 bucket at this non-secret URL.
    #[arg(long)]
    pub s3_url: Option<String>,

    /// Optional S3-compatible API endpoint for managed storage.
    #[arg(long, requires = "s3_url")]
    pub s3_endpoint_url: Option<String>,

    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct InstructionsArgs {
    /// Optional topic: model, core, task, publish, artifacts, storage, shared-checkout, or infrastructure.
    pub topic: Option<String>,

    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
}

#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print and validate the effective repository configuration.
    Show(RepoArgs),
}

#[derive(Debug, Args)]
pub struct TaskArgs {
    #[command(subcommand)]
    pub command: TaskCommand,
}

#[derive(Debug, Subcommand)]
pub enum TaskCommand {
    /// Create a deliverable workspace or isolated infrastructure worktree.
    Create(TaskCreateArgs),
    /// Inspect the resolved task scope and working changes.
    Status(TaskStatusArgs),
}

#[derive(Debug, Args)]
pub struct TaskCreateArgs {
    pub slug: String,

    #[arg(long)]
    pub title: String,

    #[arg(long)]
    pub purpose: String,

    #[arg(long, value_enum, default_value = "deliverable")]
    pub kind: TaskKind,

    /// Declare an infrastructure path. Repeat for multiple paths.
    #[arg(long = "scope")]
    pub scopes: Vec<String>,

    /// Explain why the declared infrastructure paths are authorized.
    #[arg(long, requires = "scopes")]
    pub scope_note: Option<String>,

    #[arg(long, hide = true)]
    pub timestamp: Option<String>,

    #[arg(long, default_value = ".")]
    pub repo: PathBuf,

    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct TaskStatusArgs {
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,

    #[arg(long)]
    pub manifest: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct PlanArgs {
    #[arg(long)]
    pub manifest: Option<PathBuf>,

    #[arg(long = "include")]
    pub include: Vec<String>,

    #[arg(long)]
    pub scope_note: Option<String>,

    #[arg(long)]
    pub allow_non_shared_head: bool,

    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
}

#[derive(Debug, Clone, Args)]
pub struct PublishCommandArgs {
    #[arg(long)]
    pub manifest: Option<PathBuf>,

    #[arg(short = 'm', long)]
    pub message: String,

    #[arg(long = "include")]
    pub include: Vec<String>,

    #[arg(long)]
    pub scope_note: Option<String>,

    #[arg(long)]
    pub allow_non_shared_head: bool,

    #[arg(long)]
    pub dry_run: bool,

    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
}

#[derive(Debug, Args)]
pub struct StorageArgs {
    #[command(subcommand)]
    pub command: StorageCommand,
}

#[derive(Debug, Subcommand)]
pub enum StorageCommand {
    /// Show the effective Git/S3 placement of paths in the task scope.
    Status(StorageStatusArgs),
    /// Explicitly place paths in Git or S3. This changes local desired state only.
    Set(StorageSetArgs),
    /// Remove an explicit choice and reapply the repository's automatic policy.
    Reset(StorageResetArgs),
    /// Materialize S3 content without publishing anything.
    Hydrate(StorageHydrateArgs),
}

#[derive(Debug, Clone, Args)]
pub struct ScopedArgs {
    #[arg(long)]
    pub manifest: Option<PathBuf>,

    #[arg(long = "include")]
    pub include: Vec<String>,

    #[arg(long)]
    pub scope_note: Option<String>,

    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
}

#[derive(Debug, Args)]
pub struct StorageStatusArgs {
    #[command(flatten)]
    pub scoped: ScopedArgs,

    pub paths: Vec<String>,
}

#[derive(Debug, Args)]
pub struct StorageSetArgs {
    #[command(flatten)]
    pub scoped: ScopedArgs,

    #[arg(required = true)]
    pub paths: Vec<String>,

    #[arg(long, value_enum)]
    pub to: StorageTarget,

    #[arg(long)]
    pub reason: String,

    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct StorageResetArgs {
    #[command(flatten)]
    pub scoped: ScopedArgs,

    #[arg(required = true)]
    pub paths: Vec<String>,

    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct StorageHydrateArgs {
    #[command(flatten)]
    pub scoped: ScopedArgs,

    pub paths: Vec<String>,

    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct MoveArgs {
    #[command(flatten)]
    pub scoped: ScopedArgs,

    pub old_path: String,
    pub new_path: String,

    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct RefreshArgs {
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,

    #[arg(long)]
    pub remote: Option<String>,

    #[arg(long)]
    pub branch: Option<String>,

    #[arg(long)]
    pub dry_run: bool,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::Cli;

    #[test]
    fn documented_command_shapes_are_accepted_by_clap() {
        let examples: &[&[&str]] = &[
            &["setup", "--dry-run"],
            &[
                "setup",
                "--runtime-dir",
                "/tmp/workspace-mgr-runtime",
                "--dry-run",
            ],
            &["init"],
            &[
                "init",
                "--s3-url",
                "s3://example-bucket/workspace",
                "--s3-endpoint-url",
                "https://s3.example.invalid",
                "--dry-run",
            ],
            &["instructions"],
            &["instructions", "model"],
            &["instructions", "storage", "--repo", "/tmp/repository"],
            &["--format", "json", "instructions", "publish"],
            &["doctor", "--repo", "/tmp/repository"],
            &["config", "show", "--repo", "/tmp/repository"],
            &[
                "task",
                "create",
                "training-report",
                "--title",
                "Training report",
                "--purpose",
                "Produce the final training report",
                "--dry-run",
            ],
            &[
                "task",
                "create",
                "shared-policy",
                "--kind",
                "infrastructure",
                "--title",
                "Shared policy",
                "--purpose",
                "Update repository policy",
                "--scope",
                "AGENTS.md",
                "--scope",
                ".github/workflows/ci.yml",
                "--scope-note",
                "The user requested this infrastructure change",
            ],
            &["task", "status", "--manifest", "task/manifest.toml"],
            &["storage", "status"],
            &[
                "storage",
                "status",
                "task/results/model.bin",
                "--manifest",
                "task/manifest.toml",
            ],
            &[
                "storage",
                "set",
                "task/report.pdf",
                "task/summary.txt",
                "--to",
                "git",
                "--reason",
                "Review these files directly",
            ],
            &[
                "storage",
                "set",
                "task/data",
                "--to",
                "s3",
                "--reason",
                "Retain the dataset",
                "--dry-run",
            ],
            &["storage", "reset", "task/data"],
            &["storage", "hydrate", "task/data/example.csv"],
            &["move", "task/old.bin", "task/new.bin", "--dry-run"],
            &[
                "plan",
                "--include",
                "docs/shared.md",
                "--scope-note",
                "The user requested this shared documentation update",
            ],
            &[
                "plan",
                "--allow-non-shared-head",
                "--scope-note",
                "The user selected an alternate checkout",
            ],
            &["publish", "-m", "Publish the training report"],
            &[
                "publish",
                "--message",
                "Publish shared documentation",
                "--include",
                "docs/shared.md",
                "--scope-note",
                "The user requested this shared documentation update",
                "--dry-run",
            ],
            &[
                "refresh",
                "--remote",
                "origin",
                "--branch",
                "main",
                "--dry-run",
            ],
        ];

        for args in examples {
            let argv = std::iter::once("workspace-mgr").chain(args.iter().copied());
            Cli::try_parse_from(argv).unwrap_or_else(|error| {
                panic!("documented command failed to parse: {args:?}\n{error}")
            });
        }
    }

    #[test]
    fn read_only_commands_do_not_expose_meaningless_mutation_flags() {
        assert!(Cli::try_parse_from(["workspace-mgr", "plan", "--dry-run"]).is_err());
        assert!(Cli::try_parse_from(["workspace-mgr", "plan", "-m", "unused"]).is_err());
        assert!(Cli::try_parse_from(["workspace-mgr", "storage", "status", "--dry-run"]).is_err());
    }
}
