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

## Core workflow

```sh
workspace-mgr init --profile shared-checkout \
  --s3-url s3://example-bucket/workspace \
  --s3-endpoint-url https://s3.example.invalid
workspace-mgr doctor
workspace-mgr instructions
workspace-mgr task create example-task \
  --title "Example task" \
  --purpose "Produce one reviewable deliverable"
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

The default `auto` policy sends new retained files above 10 MiB to S3 and smaller
files to Git. Existing published placement stays stable. Size is a default, not
a prohibition: `storage set --to git|s3` records an explicit choice in either
direction, and `storage reset` returns a path to automatic policy.

Directories may be placed in S3 as one logical boundary. `move` preserves a
path's placement. `storage hydrate` materializes S3 content without publishing.

## Agent instructions

`workspace-mgr init` installs a deliberately small `AGENTS.md` that tells the
agent to run `workspace-mgr instructions --repo .`. The generated document
combines product-owned policy modules, repository configuration, and an optional
repository-specific module. This keeps the bootstrap stable while allowing the
effective policy to evolve with the CLI.

## Installation from source

```sh
cargo build --release
./target/release/workspace-mgr --help
```

The alpha build expects its private execution engines to be provisioned by the
installer. The exact compatibility contract is in
[docs/platform-support.md](docs/platform-support.md).

Configuration is documented in
[docs/configuration.md](docs/configuration.md), transaction guarantees in
[docs/architecture.md](docs/architecture.md), and releases in
[docs/releasing.md](docs/releasing.md).

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo package --allow-dirty
```

Integration tests use fresh temporary repositories and local storage. GitHub
Actions also runs the full public lifecycle against a versioned local S3 service
and a network Git server. Neither test path reads developer cloud credentials.

## License

MIT
