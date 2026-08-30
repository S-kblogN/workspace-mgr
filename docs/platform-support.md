# Platform support

Release artifacts are built and tested natively on:

- Linux x86-64 and arm64;
- macOS on Apple Silicon.

Building from source requires Rust 1.85 or newer. The native installer and
`workspace-mgr setup` require platform Git and Python, then provision the S3
storage engine in an isolated user data directory. These remain private
execution engines: users and agents operate repositories through
`workspace-mgr` only.

The storage engine requirement is exact: both the `dvc` executable and the
Python module imported by the version verifier must be DVC 3.67.1. A different
patch release is rejected before a managed-storage transaction starts because
the exact verifier intentionally uses DVC's Python remote APIs. CI installs
`dvc[s3]==3.67.1` and tests this contract.

Intel macOS and Windows are not supported release targets. The source contains
portable path and symlink handling, but the end-to-end transaction suite does
not qualify artifacts for those platforms.

Operational commands produce concise YAML in human mode and stable JSON with
`--format json` or `WORKSPACE_MGR_FORMAT=json`. `instructions` produces Markdown
in human mode so a small `AGENTS.md` bootstrap can invoke it directly.
