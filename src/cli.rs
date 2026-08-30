use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::config::Profile;
use crate::output::Format;

#[derive(Debug, Parser)]
#[command(
    name = "workspace-mgr",
    version,
    about = "Policy-driven workspace and Git/DVC transaction manager",
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
    /// Initialize or adopt repository policy and scaffolding.
    Init(InitArgs),

    /// Print the effective repository instructions for an agent.
    Instructions(InstructionsArgs),

    /// Diagnose dependencies, configuration, and repository state.
    Doctor(RepoArgs),

    /// Inspect repository configuration.
    Config(ConfigArgs),

    /// Create or inspect task scaffolding.
    Task(TaskArgs),

    /// Preview a scoped Git+DVC transaction without publishing.
    Plan(PublishArgs),

    /// Publish a scoped Git+DVC transaction.
    Publish(PublishCommandArgs),

    /// Declare one or more new DVC-managed boundaries and publish them.
    Track(TrackArgs),

    /// Move one DVC-managed boundary and publish the result.
    Move(MoveArgs),

    /// Stop tracking standalone DVC stages without deleting their outputs.
    Untrack(UntrackArgs),

    /// Fetch, materialize, and verify DVC outputs in the current task scope.
    Hydrate(HydrateArgs),

    /// Safely fast-forward a shared checkout and hydrate incoming DVC data.
    Refresh(RefreshArgs),
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

    #[arg(long, value_enum, default_value = "standard")]
    pub profile: Profile,

    #[arg(long)]
    pub dvc: bool,

    #[arg(long, requires = "dvc")]
    pub dvc_remote: Option<String>,

    #[arg(long, requires_all = ["dvc", "dvc_remote"])]
    pub dvc_remote_url: Option<String>,

    #[arg(long, requires = "dvc")]
    pub version_aware: bool,

    #[arg(long)]
    pub adopt: bool,

    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct InstructionsArgs {
    /// Optional topic: core, task, publish, dvc, or infrastructure.
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
    /// Create a timestamped task directory, manifest, README, and branch.
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

    #[arg(long)]
    pub branch: Option<String>,

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
pub struct PublishArgs {
    #[arg(long)]
    pub manifest: Option<PathBuf>,

    #[arg(short = 'm', long)]
    pub message: Option<String>,

    #[arg(long = "include")]
    pub include: Vec<String>,

    #[arg(long)]
    pub scope_note: Option<String>,

    #[arg(long)]
    pub allow_non_shared_head: bool,

    #[arg(long)]
    pub dry_run: bool,

    #[arg(long)]
    pub git_only: bool,

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

    #[arg(long)]
    pub git_only: bool,

    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
}

#[derive(Debug, Args)]
pub struct TrackArgs {
    #[command(flatten)]
    pub publish: RequiredPublishArgs,

    #[arg(required = true)]
    pub paths: Vec<String>,
}

#[derive(Debug, Args)]
pub struct MoveArgs {
    #[command(flatten)]
    pub publish: RequiredPublishArgs,

    pub old_path: String,
    pub new_path: String,
}

#[derive(Debug, Args)]
pub struct UntrackArgs {
    #[command(flatten)]
    pub publish: RequiredPublishArgs,

    #[arg(required = true)]
    pub targets: Vec<String>,
}

#[derive(Debug, Clone, Args)]
pub struct RequiredPublishArgs {
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
pub struct HydrateArgs {
    #[arg(long)]
    pub manifest: Option<PathBuf>,

    #[arg(long = "include")]
    pub include: Vec<String>,

    #[arg(long)]
    pub scope_note: Option<String>,

    #[arg(long)]
    pub dry_run: bool,

    #[arg(long, default_value = ".")]
    pub repo: PathBuf,

    pub targets: Vec<String>,
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

    #[arg(long)]
    pub git_only: bool,

    #[arg(long)]
    pub scope_note: Option<String>,
}
