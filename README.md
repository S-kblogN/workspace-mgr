# workspace-mgr

`workspace-mgr` is a policy-driven workspace manager for coding agents. It
combines deterministic task scaffolding, repository-specific agent
instructions, scoped Git publication, and optional DVC-backed large-file
transactions in one standalone Rust CLI.

The project is currently pre-release. Its configuration and task schemas are
strict: unknown fields and unsupported schema versions are rejected.

## Core model

- A tracked repository config declares policy without embedding repository URLs
  or credentials in the binary.
- A small `AGENTS.md` can direct an agent to `workspace-mgr instructions`.
- `workspace-mgr task create` creates a task directory, README, manifest, and
  unmounted target branch from the configured base branch.
- `plan` and `publish` build a private Git index for the declared task scope,
  without switching the shared checkout.
- Optional DVC operations upload and verify data before publishing a Git commit
  that references it.
- `refresh` safely advances a shared checkout while preserving working-tree
  overlays.

## Installation from source

```sh
cargo build --release
./target/release/workspace-mgr --help
```

The Git transaction commands require `git`; DVC-backed workflows additionally
require `dvc`, and Git LFS fallback checks require `git-lfs`. Exact verification
for a version-aware DVC remote also requires a Python interpreter that can
import the installed `dvc` package.

## Quick start

```sh
workspace-mgr init --profile shared-checkout --dvc
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

Integration tests create isolated temporary Git repositories and local
filesystem DVC remotes. They do not read user DVC configuration or cloud
credentials.

## License

MIT
