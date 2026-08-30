use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::config::Profile;
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

    /// Preview a scoped repository transaction without publishing.
    Plan(PublishArgs),

    /// Publish a scoped repository transaction.
    Publish(PublishCommandArgs),

    /// Move one or more paths into managed storage and publish them.
    Track(TrackArgs),

    /// Move one managed-storage boundary and publish the result.
    Move(MoveArgs),

    /// Stop managing stored outputs without deleting their local content.
    Untrack(UntrackArgs),

    /// Fetch, materialize, and verify stored outputs in the current task scope.
    Hydrate(HydrateArgs),

    /// Safely update a shared checkout and hydrate incoming stored data.
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

    /// Enable managed storage at this non-secret URL.
    #[arg(long)]
    pub storage_url: Option<String>,

    /// Optional S3-compatible API endpoint for managed storage.
    #[arg(long, requires = "storage_url")]
    pub storage_endpoint_url: Option<String>,

    /// Require bucket object versioning and exact version verification.
    #[arg(long, requires = "storage_url")]
    pub require_object_versioning: bool,

    #[arg(long)]
    pub adopt: bool,

    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct InstructionsArgs {
    /// Optional topic: core, task, publish, storage, or infrastructure.
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
