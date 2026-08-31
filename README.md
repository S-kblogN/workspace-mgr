# workspace-mgr

`workspace-mgr` is the repository interface for coding agents. It turns repository
policy into executable instructions, creates isolated task scaffolding, chooses
whether retained content lives in Git or S3, and publishes both as one verified
transaction.

The public model has two storage locations:

- **Git** stores content directly in the repository history.
- **S3** stores content as versioned objects while Git records the small metadata
  needed to reproduce that exact state.

Users and agents choose between those concepts; the lower-level engines remain
implementation details.

Start with the [workspace model](docs/management-model.md) to understand how a
user can treat a coding agent as a general-purpose collaborator, how a writable
chat becomes one task/branch/PR, and how scope, placement, publication, and the
shared checkout fit together. Continue with the
[user guide](docs/guide.md) for the lifecycle. The
[command reference](docs/commands.md) documents every public command, option,
side effect, and example.

## Core workflow

```sh
workspace-mgr setup
workspace-mgr init \
  --s3-url s3://example-bucket/workspace \
  --s3-endpoint-url https://s3.example.invalid
workspace-mgr doctor
workspace-mgr instructions
workspace-mgr task create example-task \
  --title "Example task" \
  --purpose "Produce one reviewable deliverable"
workspace-mgr task create shared-policy --kind infrastructure \
  --title "Shared policy" --purpose "Update repository policy" \
  --scope AGENTS.md --scope-note "The user requested this shared change"
```

Inside a task:

```sh
workspace-mgr task rename more-accurate-topic
workspace-mgr storage status
workspace-mgr storage set path/to/data --to s3 --reason "Retained dataset"
workspace-mgr storage set path/to/report.pdf --to git --reason "Review in Git"
workspace-mgr plan
workspace-mgr publish -m "Publish the deliverable"
```

When a conversation's topic changes, `task rename <new-slug>` moves the complete
deliverable directory and updates task metadata while preserving the immutable
task ID, target branch, and existing pull request. The next ordinary `publish`
removes the previously published path and publishes the new one. `storage set`,
`storage reset`, and `move` change local desired state only.
`storage hydrate` reads from S3. `plan` is read-only. `publish` is the only
command that publishes repository content, and it verifies S3 before publishing
a Git revision. If the user instead decides to retain none of the task, the
agent closes its unmerged pull request and uses `task discard --dry-run` followed
by `task discard --confirm <task-id>` to remove its branch and local workspace.

## Placement policy

Git is the collaboration/control plane for clone-ready, directly reviewable
repository state. S3 is the artifact/data plane for exact objects that change
atomically or hydrate on demand. The agent records that semantic choice with
`storage set --to git|s3 --reason <reason>` when intent is clear; the CLI does
not infer intent from filename extensions.

Size is only the fallback for unclassified new files. Below 1 MiB, Git is the
strong default. From 1 through 10 MiB, Git remains the fallback but the plan
asks the agent to review the semantic choice. Above 10 MiB, S3 is the fallback.
An explicit S3 boundary below 1 MiB is allowed but receives an efficiency
warning. Existing published placement stays stable when size changes, and
`storage reset` returns a path to published history or the fallback.

Directories may be placed in S3 as one logical boundary whose aggregate payload
size is reported. `move` preserves a path's placement. `storage hydrate`
materializes S3 content without publishing.

## Agent instructions

`workspace-mgr init` installs a deliberately small `AGENTS.md` that tells the
agent to run `workspace-mgr instructions --repo .`. The generated document
begins with the same [workspace model](docs/management-model.md) read by users,
then renders the complete product-owned policy using the repository's Git and
S3 facts and appends an optional repository-specific content module. Every
initialized repository gets the same management strategy; policy evolves with
the CLI rather than through per-repository switches. Re-running `init` after a
CLI update deterministically replaces product-owned scaffold files with the
current versions; their ownership comes from the initialized repository and
reserved path, never from matching old file content.

The scaffold also contains a recovery path for a machine without the CLI. The
agent asks the user before installing the latest stable release from crates.io
with `cargo install --locked workspace-mgr`, runs `workspace-mgr setup`, and
then retries the instructions command. It never falls back to raw repository or
storage mutation commands.

## Installation

Install the latest stable release from crates.io, then provision its private
storage runtime:

```sh
cargo install --locked workspace-mgr
workspace-mgr setup
workspace-mgr --help
```

Building the crates.io package requires Rust 1.85 or newer. To install without
a Rust toolchain, download a prebuilt native archive for Linux x86-64/arm64 or
Apple Silicon macOS from the
[latest GitHub release](https://github.com/S-kblogN/workspace-mgr/releases/latest),
extract it, and run:

```sh
./install.sh
```

The native installer provisions the runtime and copies the CLI to
`${HOME}/.local/bin` by default. Set `WORKSPACE_MGR_PREFIX` to choose another
executable prefix.

`setup` checks Git, creates a private Python environment, installs the pinned
storage engine, and verifies both its executable and Python module. Users and
agents never invoke that engine directly. The exact compatibility contract is in
[docs/platform-support.md](docs/platform-support.md).

Every CLI invocation consults a local update cache. At most once every six
hours, it asks crates.io for newer non-yanked versions; a failed request is
silently retried after one hour. When an applicable version is available, the
CLI writes one agent-directed notice to stderr without changing command output
or exit status. It never updates itself. The agent reports the versions and asks
the user before updating, then runs `workspace-mgr setup`; managed repository
scaffolding is reconciled with `workspace-mgr init` in an infrastructure task.

Configuration is documented in
[docs/configuration.md](docs/configuration.md), transaction guarantees in
[docs/architecture.md](docs/architecture.md), platform requirements in
[docs/platform-support.md](docs/platform-support.md), and releases in
[docs/releasing.md](docs/releasing.md).

## Development

```sh
cargo fmt --check
cargo deny check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo package --allow-dirty
```

Integration tests use fresh temporary repositories and local storage. GitHub
Actions also runs the full public lifecycle against a versioned local S3 service
and a network Git server. Neither test path reads developer cloud credentials.

## License

MIT
