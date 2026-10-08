# Platform support

Release artifacts are built and tested natively on:

- Linux x86-64 and arm64;
- macOS on Apple Silicon.

Building from source requires Rust 1.99 or newer. Repository development, CI,
and release builds use Rust 1.99.0, pinned in `rust-toolchain.toml`.
The native installer and `workspace-mgr setup` require platform Git.
Storage operations run in the Rust
binary, including AWS Signature Version 4, exact-version reads, history copying,
registry publication, and cancellation. They require no Python or DVC install.

`workspace-mgr manage` converts legacy DVC metadata into versioned native JSON
manifests while retaining exact object versions and reusable cache bytes.
Fresh repositories create no DVC configuration or pointers.
Setup accepts the old `--runtime-dir` flag for installer compatibility but does
not create, inspect, replace, or remove that directory. A former Python runtime
may be removed separately after upgrading; the CLI does not alter it.

Python remains a development tool for isolated S3 fixture clients and release
automation. It is not packaged or invoked by
the production executable.

Intel macOS and Windows are not supported release targets. The source contains
portable path and symlink handling, but the end-to-end transaction suite does
not qualify artifacts for those platforms.

Operational commands produce concise YAML in human mode and stable JSON with
`--format json` or `WORKSPACE_MGR_FORMAT=json`. `instructions` produces Markdown
in human mode so a small `AGENTS.md` bootstrap can invoke it directly.
