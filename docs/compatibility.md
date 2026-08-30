# Compatibility contract

## Supported hosts and dependencies

The Phase 1 release candidate is built and tested natively on:

- Linux x86-64 and arm64;
- macOS arm64 and Intel.

Building from source requires Rust 1.85 or newer. Git is always required. The
default large-file policy also checks for Git LFS. DVC workflows require a DVC
3 installation; CI and the initial migration are pinned and tested with DVC
3.67.1. Exact version-aware verification additionally requires the configured
Python interpreter to import that same DVC installation.

Windows is not a Phase 1 release target. The source contains Windows-aware path
and symlink branches, but the end-to-end transaction suite does not yet qualify
them for release.

## Legacy migration window

The following repository-local `sync` entrypoints are retained:

| Legacy form | `workspace-mgr` form |
| --- | --- |
| `sync --manifest … -m …` | `workspace-mgr --manifest … -m …` |
| `sync plan` | `workspace-mgr plan` |
| `sync publish` | `workspace-mgr publish` |
| `sync track` | `workspace-mgr track` |
| `sync move` | `workspace-mgr move` |
| `sync untrack` | `workspace-mgr untrack` |
| `sync hydrate` | `workspace-mgr hydrate` |
| `sync refresh-main` | `workspace-mgr refresh-main` or `refresh` |

Legacy `.chat-sync.json` version 1 manifests remain readable. New tasks use
`.workspace-mgr-task.toml`. Compatibility removal requires the explicit Phase 6
decision in the migration plan.

Operational commands produce concise YAML in human mode and stable JSON with
`--format json` or `WORKSPACE_MGR_FORMAT=json`. `instructions` produces
Markdown in human mode so the minimal `AGENTS.md` bootstrap can invoke it
directly.
