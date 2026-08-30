# Platform support

Release artifacts are built and tested natively on:

- Linux x86-64 and arm64;
- macOS on Apple Silicon.

Building from source requires Rust 1.85 or newer. Git is always required. The
default large-file policy also checks for Git LFS. DVC workflows require DVC 3;
CI is tested with DVC 3.67.1. Exact version-aware verification additionally
requires the configured Python interpreter to import the same DVC installation.

Intel macOS and Windows are not supported release targets. The source contains
portable path and symlink handling, but the end-to-end transaction suite does
not qualify artifacts for those platforms.

Operational commands produce concise YAML in human mode and stable JSON with
`--format json` or `WORKSPACE_MGR_FORMAT=json`. `instructions` produces Markdown
in human mode so a small `AGENTS.md` bootstrap can invoke it directly.
