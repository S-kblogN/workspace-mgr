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
workspace-mgr storage status
workspace-mgr storage set path/to/data --to s3 --reason "Retained dataset"
workspace-mgr storage set path/to/report.pdf --to git --reason "Review in Git"
workspace-mgr plan
workspace-mgr publish -m "Publish the deliverable"
```

`storage set`, `storage reset`, and `move` change local desired state only.
`storage hydrate` reads from S3. `plan` is read-only. `publish` is the only
command that writes to Git or S3 remotes, and it verifies S3 before publishing a
Git revision.

## Placement policy

The fixed automatic policy sends new retained files above 10 MiB to S3 and smaller
files to Git. Existing published placement stays stable. Size is a default, not
a prohibition: `storage set --to git|s3` records an explicit choice in either
direction, and `storage reset` returns a path to automatic policy.

Directories may be placed in S3 as one logical boundary. `move` preserves a
path's placement. `storage hydrate` materializes S3 content without publishing.

## Agent instructions

`workspace-mgr init` installs a deliberately small `AGENTS.md` that tells the
agent to run `workspace-mgr instructions --repo .`. The generated document
begins with the same [workspace model](docs/management-model.md) read by users,
then renders the complete product-owned policy using the repository's Git and
S3 facts and appends an optional repository-specific content module. Every
initialized repository gets the same management strategy; policy evolves with
the CLI rather than through per-repository switches.

## Installation

Download the native archive for Linux x86-64/arm64 or Apple Silicon macOS,
extract it, and run:

```sh
./install.sh
```

The installer copies the CLI to `${HOME}/.local/bin` by default and provisions
its exact private storage runtime in an isolated user data directory. Set
`WORKSPACE_MGR_PREFIX` to choose another executable prefix.

For a source installation:

```sh
cargo install --locked workspace-mgr --version 0.1.0-alpha.1
workspace-mgr setup
workspace-mgr --help
```

`setup` checks Git, creates a private Python environment, installs the pinned
storage engine, and verifies both its executable and Python module. Users and
agents never invoke that engine directly. The exact compatibility contract is in
[docs/platform-support.md](docs/platform-support.md).

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
