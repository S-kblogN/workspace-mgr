# Command reference

This reference describes the public CLI. Run `workspace-mgr --help` or
`workspace-mgr <command> --help` for the same syntax at the installed version.
The [user guide](guide.md) explains how the commands form one workflow.

## Conventions

- Repository paths are relative to the Git root, even when a command is run
  from a task directory.
- `--repo <path>` selects the starting repository or task path and defaults to
  the current directory.
- Task-scoped commands discover `.workspace-mgr-task.toml` from the starting
  path. `--manifest <path>` selects one explicitly.
- `--include <path>` adds a one-invocation scope and requires a one-line
  `--scope-note <reason>`. Repeat `--include` for multiple paths.
- `--dry-run` previews local mutation for commands that support it.
- Human output is concise YAML, except Markdown from `instructions` and TOML
  from `config show`. Use global `--format json` or set
  `WORKSPACE_MGR_FORMAT=json` for stable structured output.
- Errors exit with status 2 and start with `workspace-mgr:`.

## `workspace-mgr setup`

Provision and verify the private managed-storage runtime.

```text
workspace-mgr setup [--runtime-dir <path>] [--dry-run]
```

The default location follows `WORKSPACE_MGR_RUNTIME_DIR`, then
`XDG_DATA_HOME`, then `${HOME}/.local/share`. Setup creates an isolated Python
environment and installs the exact compatible storage runtime. It requires Git
and Python for provisioning, but users and agents do not invoke the private
engine directly. `--dry-run` performs no installation or package download.

## `workspace-mgr init`

Initialize a repository or reconcile its managed scaffolding.

```text
workspace-mgr init [--repo <path>]
  [--profile standard|shared-checkout]
  [--s3-url <url> [--s3-endpoint-url <url>]]
  [--adopt] [--dry-run]
```

`--profile` selects defaults only when creating a new configuration. `--s3-url`
must use `s3://` and is a tracked, non-secret storage location; userinfo,
queries, fragments, and other credential-bearing URL forms are rejected.
`--adopt` preserves an existing unmanaged `AGENTS.md` as a repository
instruction module before replacing it with the bootstrap. Re-running `init`
validates public configuration and deterministically repairs internal storage
scaffolding. This command never contacts or writes a remote.

```sh
workspace-mgr init --profile standard
workspace-mgr init --profile shared-checkout --adopt \
  --s3-url s3://example-bucket/workspace
workspace-mgr init --dry-run
```

## `workspace-mgr instructions`

Render the shared workspace model and effective agent policy.

```text
workspace-mgr instructions [all|model|core|task|publish|artifacts|storage|shared-checkout|infrastructure]
  [--repo <path>]
```

With no topic, `all` is used. It renders the canonical workspace model first,
then the effective operational rules. `model` returns only that
shared conceptual document. The output includes a CLI version, schema version,
topic, and policy hash. `core` and `model` are always available. Other topics
require their corresponding `[agent].modules` entry; requesting a disabled
topic is an error. Repository-specific additions are appended only to `all`.

```sh
workspace-mgr instructions
workspace-mgr instructions model
workspace-mgr instructions storage
workspace-mgr --format json instructions publish
```

## `workspace-mgr doctor`

Diagnose the CLI version constraint, repository configuration, Git state, and
required private execution engines.

```text
workspace-mgr doctor [--repo <path>]
```

The command is read-only. When S3 is configured it reads the bucket-versioning
setting and rejects a bucket that is not enabled. It exits with status 2 if any
reported check is not healthy.

## `workspace-mgr config show`

Parse, validate, and print `.workspace-mgr.toml`.

```text
workspace-mgr config show [--repo <path>]
```

Human output is TOML. JSON output exposes the public configuration model and
does not expose private engine configuration or credentials.

## `workspace-mgr task create`

Create one task directory, README, manifest, and local target-branch ref.

```text
workspace-mgr task create <slug> --title <title> --purpose <purpose>
  [--branch <branch>] [--repo <path>] [--dry-run]
```

The slug is lowercase kebab case. The generated directory follows
`tasks.directory_pattern`; the default branch uses
`publication.branch_prefix`. The command fetches the configured base branch to
anchor the local target ref. It refuses existing directories or local/remote
branch names and does not publish the new branch.

```sh
workspace-mgr task create training-report \
  --title "Training report" \
  --purpose "Produce the final training report"
workspace-mgr task create urgent-fix --title "Urgent fix" \
  --purpose "Repair the release input" --branch review/urgent-fix --dry-run
```

## `workspace-mgr task status`

Show the resolved task identity, manifest, branch, remote, base branch, scopes,
and current working changes inside those scopes.

```text
workspace-mgr task status [--repo <path>] [--manifest <path>]
```

This is a local read-only view. Use `plan` for the complete prospective
publication state.

## `workspace-mgr storage status`

Explain effective Git/S3 placement.

```text
workspace-mgr storage status [<path> ...]
  [--manifest <path>]
  [--include <path> --scope-note <reason>]
  [--repo <path>]
```

With paths, status reports those paths, including a descendant inherited from a
directory boundary. The path need not currently be materialized if published
metadata can determine its placement. With no paths, status lists ordinary Git
files and one row per explicit or published S3 boundary in all resolved scopes.
The report includes `target`, `selected_by`, and an explicit `reason` when one
exists. It never writes a remote.

```sh
workspace-mgr storage status
workspace-mgr storage status 20260829-180000-report/results/model.bin
```

## `workspace-mgr storage set`

Record an explicit Git or S3 placement.

```text
workspace-mgr storage set <path>... --to git|s3 --reason <reason>
  [--manifest <path>]
  [--include <path> --scope-note <reason>]
  [--repo <path>] [--dry-run]
```

Every target must exist and remain in the resolved scopes. The reason must be a
non-empty single line. S3 must be configured before selecting it. Setting a
directory creates one recursive boundary; nested or overlapping existing
boundaries are rejected. The command updates local desired state and reports
`remote_writes: false`.

```sh
workspace-mgr storage set 20260829-180000-report/report.pdf \
  --to git --reason "Review the report directly"
workspace-mgr storage set 20260829-180000-report/data \
  --to s3 --reason "Retain the dataset as one boundary"
```

## `workspace-mgr storage reset`

Remove an explicit choice and return paths to automatic policy. Published
placement remains sticky: resetting a published S3 boundary keeps it in S3;
use `storage set --to git` for an intentional placement change.

```text
workspace-mgr storage reset <path>...
  [--manifest <path>]
  [--include <path> --scope-note <reason>]
  [--repo <path>] [--dry-run]
```

This may locally convert a prior S3 boundary back to ordinary content. Resetting
a directory removes the directory boundary, so its files can be evaluated
individually by the next `plan` or `publish`. No remote is changed.

## `workspace-mgr storage hydrate`

Materialize exact S3 content locally without publication.

```text
workspace-mgr storage hydrate [<path> ...]
  [--manifest <path>]
  [--include <path> --scope-note <reason>]
  [--repo <path>] [--dry-run]
```

With no paths, every S3 boundary in scope is selected. A descendant path selects
its containing S3 directory boundary. Hydration fetches from S3, checks out the
content, and verifies it. It refuses locally modified outputs and never writes
Git or S3 remotes.

## `workspace-mgr move`

Move a path while preserving its effective placement.

```text
workspace-mgr move <old-path> <new-path>
  [--manifest <path>]
  [--include <path> --scope-note <reason>]
  [--repo <path>] [--dry-run]
```

Both paths must remain inside the resolved scopes, the source must exist, and
the destination must not. A move may stay within a containing directory
boundary or move the boundary itself; it may not cross into or out of another
directory boundary. The command changes local desired state only. A later
`publish` writes the new S3 object path when applicable and retains earlier
remote versions.

## `workspace-mgr plan`

Preview the complete task transaction.

```text
workspace-mgr plan [--manifest <path>]
  [--include <path> --scope-note <reason>]
  [--allow-non-shared-head --scope-note <reason>]
  [--repo <path>]
```

Plan fetches the configured base and target Git refs, evaluates automatic
placement, validates S3 metadata and local outputs, constructs a private
preview tree that excludes payloads destined for S3, and reports changed paths,
object IDs, and pending placement. Exact generated S3 metadata is established by
`publish`, because `plan` does not rewrite it. Plan may create ignored local
locks or preview state. It never creates a commit, uploads S3 content, or pushes
a Git branch.

`--allow-non-shared-head` is an exceptional checkout override and requires a
scope note. It still refuses when the target task branch is currently checked
out.

```sh
workspace-mgr plan
workspace-mgr plan --include docs/shared.md \
  --scope-note "The user requested this shared documentation update"
```

## `workspace-mgr publish`

Publish one verified scoped transaction.

```text
workspace-mgr publish -m <message> [--manifest <path>]
  [--include <path> --scope-note <reason>]
  [--allow-non-shared-head --scope-note <reason>]
  [--repo <path>] [--dry-run]
```

The message is required and must be one line. Publication uploads and verifies
all live in-scope S3 boundaries before creating and pushing the Git commit. The
Git tree is based on the existing remote task branch, or the configured base
branch for its first publication, and includes only resolved scopes. The remote
branch object ID is verified after push. The checkout and shared Git index are
not switched to the task branch.

`publish --dry-run` performs the same non-publishing behavior as `plan` while
still requiring a message argument.

```sh
workspace-mgr publish -m "Publish the training report"
```

The command does not create, update, merge, or close a pull request.

## `workspace-mgr refresh`

Safely fast-forward a shared checkout after remote changes are merged.

```text
workspace-mgr refresh [--repo <path>]
  [--remote <remote>] [--branch <branch>] [--dry-run]
```

Defaults come from `[publication]`. The checkout must be on the selected branch,
the shared index must have no staged or unresolved entries, and the remote
revision must be a fast-forward. Refresh preserves unrelated working-tree
overlays, materializes safe ordinary Git additions, modifications, and
deletions, and hydrates incoming S3 boundaries. It reads Git and S3 but writes
no remote.

Use `--remote` or `--branch` only for an explicitly selected alternate shared
checkout target.

## Help and version

```sh
workspace-mgr --help
workspace-mgr storage set --help
workspace-mgr --version
```

Help describes syntax. Repository operating policy comes from
`workspace-mgr instructions`, which is why the generated `AGENTS.md` invokes
`instructions` rather than `help`.
