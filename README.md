# workspace-mgr

`workspace-mgr` is a policy-driven workspace manager for coding agents. It
combines deterministic task scaffolding, repository-specific agent
instructions, scoped publication, and optional versioned large-file storage in
one Rust CLI.

The project is currently pre-release. Its configuration and task schemas are
strict: unknown fields and unsupported schema versions are rejected.

## Core model

- A tracked repository config declares policy without embedding repository URLs
  or credentials in the binary.
- A small `AGENTS.md` can direct an agent to `workspace-mgr instructions`.
- `workspace-mgr task create` creates a task directory, README, manifest, and
  unmounted target branch from the configured base branch.
- `plan` and `publish` transact only the declared task scope without switching
  the shared checkout.
- Managed-storage operations upload and verify data before publishing a
  repository revision that references it.
- `refresh` safely advances a shared checkout while preserving working-tree
  overlays.

## Installation from source

```sh
cargo build --release
./target/release/workspace-mgr --help
```

The alpha source build expects its lower-level repository and storage engines to
be provisioned by the installer. Their exact compatibility contract is in
[docs/platform-support.md](docs/platform-support.md). They are implementation
dependencies, not supported user or agent interfaces.

## Quick start

```sh
workspace-mgr init --profile shared-checkout \
  --storage-url s3://example-bucket/workspace \
  --require-object-versioning
workspace-mgr doctor
workspace-mgr instructions
workspace-mgr task create example-task \
  --title "Example task" \
  --purpose "Produce one reviewable deliverable"
```

Run `workspace-mgr COMMAND --help` for command-specific options. The
configuration schema is documented in [docs/configuration.md](docs/configuration.md),
the transaction guarantees in [docs/architecture.md](docs/architecture.md), the
release process in [docs/releasing.md](docs/releasing.md), and supported hosts
and dependencies in [docs/platform-support.md](docs/platform-support.md).

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo package --allow-dirty
```

`sh scripts/package-native.sh "$(rustc -vV | sed -n 's/^host: //p')"`
assembles the release archive and checksum for a supported native host.

Integration tests create isolated temporary repositories and local filesystem
storage remotes. They do not read user storage configuration or cloud
credentials.

GitHub Actions also runs a system-level E2E lifecycle against a versioned local
S3-compatible service and a bare repository served over the Git network
protocol. It checks every public command, critical failure ordering, exact
remote refs and trees, S3 object versions, cache-independent hydration, and
shared-checkout behavior. See [tests/e2e/README.md](tests/e2e/README.md).

## License

MIT
