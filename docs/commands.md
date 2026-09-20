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
- `--include <path>` records a user-authorized one-invocation scope and requires
  a one-line `--scope-note <reason>`. It does not create authorization. Repeat
  `--include` for multiple paths.
- `--dry-run` previews local mutation for commands that support it. Task discard
  also saves a private revision-bound confirmation plan.
- Human output is concise YAML, except Markdown from `instructions` and TOML
  from `config show`. Use global `--format json` or set
  `WORKSPACE_MGR_FORMAT=json` for stable structured output.
- Errors exit with status 2 and start with `workspace-mgr:`.
- Every invocation performs a best-effort cached update check. A newer
  applicable release produces exactly one `workspace-mgr: update available`
  line on stderr; stdout, structured output, and command exit status are
  unchanged. The CLI never updates itself. Agents report the versions and ask
  the user before updating, then run `workspace-mgr setup`; scaffold changes are
  reconciled with `workspace-mgr init` in an infrastructure task.

## `workspace-mgr setup`

Provision and verify the private managed-storage runtime.

```text
workspace-mgr setup [--runtime-dir <path>] [--dry-run]
```

The default location follows `WORKSPACE_MGR_RUNTIME_DIR`, then
`XDG_DATA_HOME`, then `${HOME}/.local/share`. Setup creates an isolated Python
environment and installs the exact compatible storage runtime. It requires Git
and Python for provisioning, but users and agents do not invoke the private
engine directly. `--dry-run` performs no installation or package download. An
existing target is replaced only when it carries workspace-mgr's private
ownership marker; an arbitrary file, directory, or symlink is refused without
modification.

## `workspace-mgr init`

Initialize a repository or reconcile its managed scaffolding.

```text
workspace-mgr init [--repo <path>]
  [--s3-url <url> [--s3-endpoint-url <url>]]
  [--dry-run]
```

`--s3-url` must use `s3://` and is a tracked, non-secret storage location; userinfo,
queries, fragments, and other credential-bearing URL forms are rejected.
Re-running `init` validates public configuration and deterministically repairs
or upgrades product-owned scaffolding. Ownership is established by the
initialized repository and reserved path, not inferred from file content, so
old, edited, or damaged `AGENTS.md` and internal storage configuration are
replaced with their current deterministic forms, as are the private engine's
ignore files. Before the first successful initialization, an existing
`AGENTS.md`, root `.gitignore`, or private internal-storage scaffold is instead
an atomic collision that the caller must move or remove explicitly; for the
root `.gitignore` the message says where its rules belong.

The root `.gitignore` is the one reserved path a repository is likely to have
arranged for itself long before workspace-mgr existed, so the product owns it
only once it wrote it, which the generated first line records. A root
`.gitignore` that the product did not generate is never reconciled over: `init`
and `doctor` both refuse it and say to move this repository's own rules into
`.workspace-mgr/repository.gitignore`, remove the root file, and run `init`
again. Every repository initialized by an earlier release performs that
migration once; see the upgrade note in the guide. Below the generated header
the file is product-owned like the others, so a hand edit there is drift that
`init` repairs.

The generated root `.gitignore` is the product's fixed rules for output that is
regenerated rather than retained — `.DS_Store`, `__pycache__/`, `*.pyc`,
`*.pyo`, `.ipynb_checkpoints/`, `.pytest_cache/`, `.mypy_cache/`,
`.ruff_cache/`, `.venv/`, `venv/`, and `node_modules/` — followed by this
repository's own rules imported verbatim from
`.workspace-mgr/repository.gitignore`, followed by any
`# workspace-mgr local begin` blocks the root file already holds.
The module is optional, repository-owned, and limited to 64 KiB of UTF-8; an
absent or empty module produces no import section. It carries ignore patterns
only: those two block markers belong to `untrack`, and a module containing one
is refused, because regeneration harvests them back out of the file it writes
and the file would never settle. Regeneration preserves a well-formed block
byte for byte. `untrack` writes its block into the ignore file of the path's
own directory, which today is always inside a task, so a block in the root file
is a state this format supports rather than one a command produces; a marker
without its partner has no readable extent, so regeneration drops it and the
reported action names the marker it dropped. `doctor` reports a hand-edited
root file through its `repository-scaffold` check. `init` refuses to change the S3
location while retained S3 boundaries exist. It never contacts or writes a
remote. The generated `AGENTS.md` includes an approval-gated command that
installs the latest stable release from crates.io, followed by `setup` and an
instructions retry, so a new machine can bootstrap without inventing a
lower-level workflow.

```sh
workspace-mgr init
workspace-mgr init \
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
shared conceptual document. The output includes a CLI version, product policy
version, topic, and policy hash. Every topic is always available.
Repository-specific additions are appended only to `all`.

```sh
workspace-mgr instructions
workspace-mgr instructions model
workspace-mgr instructions storage
workspace-mgr --format json instructions publish
```

## `workspace-mgr doctor`

Diagnose the repository configuration, product-owned scaffold, Git state, and
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

Create one deliverable workspace or repository-infrastructure workspace.

```text
workspace-mgr task create <slug> --title <title> --purpose <purpose>
  [--kind deliverable|infrastructure]
  [--scope <path>... --scope-note <reason>]
  [--repo <path>] [--dry-run]
```

The slug is lowercase kebab case. The default `deliverable` kind creates a
timestamped top-level directory, README, tracked manifest, and the unmounted
target branch `codex/<slug>`. The scaffolded README's directory map tells the
task to keep its tools, process, decisions, and hard-to-reproduce results in
that directory and to list them there; which files carry them is the agent's
choice. The `infrastructure` kind requires at least one `--scope` plus a
`--scope-note`; it creates `codex/infra-<slug>` and an isolated worktree below
private Git common state, with no repository task directory. Its manifest is
private worktree state and every scope is explicit. Both kinds fetch the
configured base branch, reject an existing directory or local/remote branch, and
publish nothing.

The report contains a structured `review` handoff. Deliverable creation reports
`creation_timing: immediate-after-scaffold-publication`; the agent must
immediately plan and publish the initial scaffold, then create and verify the
one draft pull request. Infrastructure creation reports
`creation_timing: after-first-scoped-publication`. Both report
`synchronization_cadence: before-every-turn-end`, requiring the agent to
reconcile the task, remote branch, and pull request automatically before each
writable-task turn ends.

```sh
workspace-mgr task create training-report \
  --title "Training report" \
  --purpose "Produce the final training report"
workspace-mgr task create urgent-fix --title "Urgent fix" \
  --purpose "Repair the release input" --dry-run
workspace-mgr task create shared-policy --kind infrastructure \
  --title "Shared policy" --purpose "Update repository-wide policy" \
  --scope AGENTS.md --scope .github/workflows/ci.yml \
  --scope-note "The user requested this infrastructure change"
```

## `workspace-mgr task rename`

Change the current human-readable slug without replacing the task, target
branch, or pull request.

```text
workspace-mgr task rename <new-slug>
  [--repo <path>] [--manifest <path>] [--dry-run]
```

The new slug uses the same lowercase ASCII kebab-case validation as creation.
For a deliverable, the command preserves the timestamp and moves the entire
task directory from `<timestamp>-<old-slug>` to
`<timestamp>-<new-slug>`. Its README, retained content, S3 pointers, placement
sidecars, and manifest move together. The manifest is atomically rewritten with
schema 2 and records the new current slug and path. Infrastructure tasks keep
their identity-owned private worktree path and update only the private current
slug metadata.

The task ID and target branch are immutable. Keeping the branch stable lets the
agent reuse the one existing draft pull request; renaming an open pull request's
head branch can close it on hosting providers. The report tells the agent to
update that pull request's title and description after publication.

Rename fetches the shared and task refs to reject merged tasks, changed remote
identity, published destination collisions, local destination collisions, and
staged source/destination changes. It writes no Git or S3 remote. On a published
deliverable, the next normal `plan` includes the published old path as an
identity-derived cleanup scope, preserves published Git/S3 placement at the new
path, and `publish` deletes the old tree while advancing the same branch.
Because version-aware S3 IDs are bound to object paths, rename clears those old
bindings from moved pointers; publish creates and verifies new object versions
at the new path, publishes Git, then permanently deletes every version at the
old path unless another current remote branch or tag still references it.

```sh
workspace-mgr task rename current-research-question --dry-run
workspace-mgr task rename current-research-question
workspace-mgr plan
workspace-mgr publish -m "Rename the task for its current topic"
```

## `workspace-mgr task status`

Show the immutable task identity, current slug, manifest, branch, remote, base
branch, scopes, and current working changes inside those scopes.

```text
workspace-mgr task status [--repo <path>] [--manifest <path>]
```

This is a local read-only view. Use `plan` for the complete prospective
publication state.

## `workspace-mgr task discard`

Permanently abandon one unmerged task after its pull request is closed or
verified absent by the agent.

```text
workspace-mgr task discard (--dry-run | --confirm <task-id>)
  [--repo <path>] [--manifest <path>]
```

Always run `--dry-run` first. It creates no repository-content or remote change,
but writes a private `discard-plan.json` containing the observed task identity,
local and remote task refs, and local and remote shared refs. Its structured
report includes:

- every working change in the deliverable scopes or infrastructure worktree;
- the task directory or worktree to delete;
- each additional deliverable scope to restore from the local shared branch;
- whether the agent must close a pull request or verify that none exists;
- current local or published managed S3 object paths and recorded exact version
  IDs queued for permanent deletion after the branch is removed.

After explicit user authorization, the agent verifies the task is unmerged,
closes the matching pull request if it exists, and verifies that provider state.
Run confirmation from the shared checkout and pass the manifest printed by the
dry run, because the task workspace itself will be deleted:

```sh
workspace-mgr task discard --dry-run
workspace-mgr task discard \
  --manifest /absolute/path/to/.workspace-mgr-task.toml \
  --confirm 20260830-120000-example
```

Confirmation requires the exact task ID and an unchanged private plan. It
refuses changed refs, a branch with another task identity, a task already
contained in the shared branch, an unmanaged infrastructure worktree, or an
invocation whose current directory would be deleted. It deletes an existing
remote task branch with `force-with-lease`, verifies absence, deletes local and
remote-tracking refs, then removes the local workspace and private task state.
Deliverable scopes are first moved into private quarantine; additional scopes
and their shared-index entries are restored from the local shared branch. A
remote failure restores quarantined paths and their prior index state.
Infrastructure confirmation removes the entire managed worktree.

The CLI is provider-neutral and cannot verify pull-request state itself; the
report makes that agent responsibility explicit. Before branch deletion,
discard queues every versioned S3 object path owned by the task. After the Git
branch is removed, it permanently deletes every version of paths that no
current remote branch or tag still references. Protected paths remain pending
and are retried by a later publish, refresh, or discard.

## `workspace-mgr storage status`

Explain effective Git/S3 placement or explicit local-only state.

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
An explicitly queried directory must itself be a selected or published S3
boundary; otherwise it has no single placement because automatic evaluation
operates on its files independently. Query without paths to inspect those files.
Each row includes `target`, `basis`, effective `boundary`, available
`payload_bytes` and `payload_files`, an explicit semantic `reason` when one
exists, and structured `warnings`. It never writes a remote.

```sh
workspace-mgr storage status
workspace-mgr storage status 20260829-180000-report/results/model.bin
```

## `workspace-mgr storage set`

Record an explicit Git or S3 placement.

For a local-only path, this explicitly resumes tracking and removes only the
ignore rule owned by `untrack`. User-authored ignore rules are preserved; if
they still prevent tracking, resolve the reported conflict first.

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
`remote_writes: false`. Explicit S3 below the recommended 1 MiB aggregate
boundary size remains valid but reports `small-s3-boundary`; select Git or a
larger meaningful boundary when practical.

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
Local-only paths refuse reset: use an explicit `storage set --to git|s3` to
resume tracking.

```text
workspace-mgr storage reset <path>...
  [--manifest <path>]
  [--include <path> --scope-note <reason>]
  [--repo <path>] [--dry-run]
```

This may locally convert a prior S3 boundary back to ordinary content. Resetting
a directory removes the directory boundary, so its files can be evaluated
individually by the next `plan` or `publish`; the reset report therefore has no
single placement row for an unpublished directory boundary. No remote is changed.

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
`publish` writes and verifies the new S3 object path, publishes the Git revision,
then permanently deletes every version of the old path unless another current
remote branch or tag still references it.

## `workspace-mgr remove`

Delete ordinary Git content, a complete S3 boundary, or a descendant inside an
S3 directory boundary without interpreting an unhydrated output as deletion.

```text
workspace-mgr remove <path>...
  [--manifest <path>]
  [--include <path> --scope-note <reason>]
  [--repo <path>] [--dry-run]
```

The command changes local desired state only and refuses to remove an entire
task scope; use confirmed task discard for that. A later `publish` first makes
the deletion authoritative in Git, then permanently deletes every S3 version
at object paths removed by the operation. Current remote branches and tags are
reference guards, so protected objects remain in private pending state until a
later `publish`, `refresh`, or discard can delete them safely.

## `workspace-mgr untrack`

Keep content locally, add a managed ignore rule, and remove its payload from
Git and S3 on the next publication.

```text
workspace-mgr untrack <path>...
  [--manifest <path>]
  [--include <path> --scope-note <reason>]
  [--repo <path>] [--dry-run]
```

The command preserves local bytes, writes a placement sidecar with `target =
"local"`, adds an anchored literal rule to the parent `.gitignore`, and removes
any S3 pointer for the boundary. The sidecar and ignore rule remain in Git;
the payload does not. It writes no remote and does not modify the shared Git
index. Repeating it is safe and does not duplicate ignore rules. `--dry-run`
previews the change without modifying local files or metadata.

The first conversion requires materialized content. If the S3 output is absent,
run `storage hydrate <path>` first. Local-only placement remains valid on a new
clone where the payload has never existed. A directory is one recursive local
boundary. An ordinary directory must first be selected as a boundary with
`storage set <directory> --to git --reason <reason>`. Nested placement
boundaries, task control files, and entire task
scopes cannot be untracked; operate on a complete existing boundary or first
reorganize its placement. The payload, sidecar, and parent `.gitignore` must
all be inside the authorized scopes.

`plan` reports `storage.local_only`, the Git changes, and retired S3 versions
in `storage.purge.queued`. `publish` first publishes the Git deletion, then
permanently cleans obsolete S3 object versions. Current remote branches and
tags can defer cleanup; for example, `main` protects the previous S3 content
until the deletion is merged. A later `publish` or `refresh` retries pending
cleanup. Git history is not rewritten.

After merge, `refresh` retains existing local-only bytes. Automatic placement,
publication, and hydration do not upload or recreate them. Resume tracking with
`storage set <path> --to git|s3 --reason <reason>`; `storage reset` deliberately
refuses to resume tracking implicitly.

`move` currently refuses local-only boundaries and their descendants; resume
tracking before using managed moves. `remove` can delete a complete local-only
boundary, including its owned ignore rule.

```sh
workspace-mgr untrack 20260829-180000-report/data.bin --dry-run
workspace-mgr untrack 20260829-180000-report/data.bin
workspace-mgr plan
workspace-mgr publish -m "Keep data.bin local only"
```

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
object IDs, and pending placement. Its placement report gives the fixed 1 MiB
recommended S3 minimum and 10 MiB automatic threshold, automatic decisions in
the 1–10 MiB semantic-review band or above the S3 threshold, and existing
boundaries with actionable warnings. Exact generated S3 metadata is established
by `publish`, because `plan` does not rewrite it. Plan may create ignored local
locks or preview state. It never creates a commit, uploads S3 content, or pushes
a Git branch.

Plan reports the ignored paths inside the resolved scopes as `ignored_paths`
beside the `ignored_entries` count, and structured `warnings`. Both fields are
present only when they are not empty: `ignored_entries` is always exact, while
`ignored_paths` carries up to its first fifty entries so a pattern-based ignore
rule cannot fill the report. For a deliverable task, `task-record-unchanged`
reports a publication that changes content inside the task directory while none
of the task's own documentation changed with it; ignore it when the work
produced nothing worth recording. `bulk-publication` reports a publication that
adds more than 200 new files, or more than 256 MiB (268435456 bytes), of new
content inside the task directory; the thresholds are fixed product policy. New
content routed to S3 by automatic placement is counted from the placement
decision and measured on disk, because its payload never reaches the private
index, and its pointer is not counted a second time once `publish` has written
it; an explicitly selected boundary counts as the one pointer it adds. A file
that only moved — every published file of a renamed task, for instance — is not
new content and is not counted. Both are checks rather than refusals.

Plan refuses, before it changes placement or uploads anything, a deliverable
publication that would add or change content inside its own task directory
while that directory documents nothing, and any staged symbolic link whose
target is outside the repository. A task documents itself with Markdown files
of its own choosing inside its directory; a README still carrying only the
creation scaffold's directory map is not yet a record. A storage pointer or
placement record counts as the content it addresses, so a result routed to S3
is judged like one kept in Git. A publication that only retires content is not
refused; one that removes the task's last record while publishing content is
refused by name. The symbolic-link check reads the staged tree, so a link
inside a boundary already placed in S3 or kept local with `untrack` is not
classified.

Plan also refuses, at the same point, a path inside the resolved scopes that
only a machine-local ignore rule hides: the user's global excludes file,
`.git/info/exclude`, or an ignore file whose matching bytes this publication
does not carry. Such a rule keeps the file out of every other clone and out of
review, so the path is in neither of the two states task content may be in. The
message names the path, the rule, and the file the rule came from, lists at most
the first five paths, and counts the rest. It points first at the task's own
`<task>/.gitignore`, which this publication stages and which stays inside a
deliverable task's write boundary, and states that the repository layer is a
shared root path needing explicit authorization and its own publication.

Carrying is decided on content, not on the path being tracked: the work-tree
bytes of the ignore file must be the bytes this publication holds for it, so a
rule added to a tracked root `.gitignore` and never published is machine-local
like any other. The product's own fixed rules are the exception — every
installation regenerates them — so an ordinary `.DS_Store` never triggers the
refusal, including before the generated root file has been published. Git
resolves the deepest matching ignore file before `.git/info/exclude` and the
global excludes, so a carried repository or task rule that also matches is the
reported source and does not trigger the refusal. An ignore file this
publication itself adds already counts as carried.

`git status --ignored` collapses a directory whose every entry is ignored into
a single entry, which a file-level rule such as `*.log` does not itself match.
Those directories, and only those, are re-listed with `--ignored=matching` so
the individual files resolve to their rule; a directory a directory rule already
covers stays collapsed and is never walked. The same rule applies to an
infrastructure task over its declared scopes.

`publish` applies the same refusals.

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
still requiring a message argument. Publication reports the same `warnings` and
`ignored_paths` as `plan` and applies the same refusals before it changes
placement or uploads content.

```sh
workspace-mgr publish -m "Publish the training report"
```

The command does not create, update, merge, or close a pull request. Its output
contains a provider-neutral review handoff: pull-request policy, initial state,
manager, merge authority, remote, base branch, and head branch. The responsible
agent uses those facts with the repository hosting workflow.

## `workspace-mgr refresh`

Safely fast-forward a shared checkout after remote changes are merged.

```text
workspace-mgr refresh [--repo <path>] [--dry-run]
```

The remote and shared branch come from `[git]`. The checkout must be on that branch,
the shared index must have no staged or unresolved entries, and the remote
revision must be a fast-forward. Refresh preserves unrelated working-tree
overlays, materializes safe ordinary Git additions, modifications, and
deletions, and hydrates incoming S3 boundaries. It reads Git and S3 but writes
no remote.

## Help and version

```sh
workspace-mgr --help
workspace-mgr storage set --help
workspace-mgr --version
```

Help describes syntax. Repository operating policy comes from
`workspace-mgr instructions`, which is why the generated `AGENTS.md` invokes
`instructions` rather than `help`.
