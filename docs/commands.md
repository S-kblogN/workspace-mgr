# Command reference

This reference describes the public CLI. Run `workspace-mgr --help` or
`workspace-mgr <command> --help` for the same syntax at the installed version.
The [user guide](guide.md) explains how the commands form one workflow.

## Conventions

- Repository paths are relative to the Git root, even when a command is run
  from a task directory. `/` is their only separator: on the supported Linux
  and macOS targets a backslash is an ordinary file-name character that is
  never rewritten, so typed paths, Git paths, and storage metadata compare
  exactly. The storage engine reads a backslash as a separator, so an S3
  boundary path may not contain one. Automatic placement, explicit
  `storage set --to s3`, and a `storage reset` whose automatic policy selects
  S3 refuse such a path before any metadata is written, and `move` refuses it
  as a destination for content already in S3; rename the path, or keep it in
  Git with `storage set --to git`. Storage metadata that an earlier release
  left at such a path cannot be addressed at all, so `plan`, `publish`,
  `storage hydrate`, `storage set`, and `untrack` refuse it with status 2 and
  a `workspace-mgr move` recovery hint.
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
- A repository whose `.workspace-mgr.toml` declares a `minimum_cli_version`
  that the installed CLI does not meet is refused, with status 2, by every
  command that reads the repository configuration, including `instructions`;
  only `doctor` still runs and reports it. The error names the required
  version, where it was declared, and the installed version, and tells the
  agent to report both versions to the user and ask before updating with
  `cargo install --locked workspace-mgr`, followed by `workspace-mgr setup`.
  Commands that fetch apply the same check to what they fetched before they
  change anything: `task create` and `task discard` to the base branch,
  `task rename`, `plan`, and `publish` to the base branch and the task branch,
  and `refresh` to the incoming revision; their errors name
  `.workspace-mgr.toml on <remote>/<branch>`. A pre-release meets a
  declaration of its own release. The
  [configuration reference](configuration.md) describes the declaration.
- While a task is waiting for the user's cloud-usage decision, `storage
  status`, `storage set`, `storage reset`, `storage hydrate`, `move`, `remove`,
  `untrack`, `task rename`, and `task discard` print one `workspace-mgr: task
  <id> is waiting for the user's cloud-usage decision` line on stderr as soon as
  the task is resolved. It is a reminder that task work stops until the user
  answers, not an error; stdout, structured output, and exit status are
  unchanged. It repeats the projection last measured by `plan` or `publish`, so
  it does not interrupt carrying out an answer the user already gave; `plan`
  re-measures afterward. The line disappears once the approval recorded in the
  task manifest covers the pending projection or a later `plan` or `publish`
  measures the task within its limit.

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
location while retained S3 boundaries exist. It keeps an existing
`minimum_cli_version` exactly and never adds one. It never contacts or writes a
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

Whenever `.workspace-mgr.toml` is readable, the `cli-version` check follows
`repository-config`. It compares the installed CLI with the higher of the
checkout's `minimum_cli_version` and the declaration committed at the base
branch's remote-tracking ref, `refs/remotes/<remote>/<branch>`, as last
fetched; doctor itself never fetches. Its detail is `installed <version>,
repository requires <version>` for the checkout's declaration, `installed
<version>, <remote>/<branch> requires <version>` when the fetched base branch
requires more, or `installed <version>, repository declares no minimum
version`. The check is `ok` when the installed CLI meets that requirement and
`error` when it does not, for example in a checkout that still has to be
refreshed after the base branch was raised. Unlike other commands, doctor does
not refuse such a repository: it reads the declaration even when the rest of
the file uses fields this CLI does not know, which `repository-config` then
reports as an error.

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
publish nothing. Before creating a branch, directory, or worktree, they refuse
a base branch whose `minimum_cli_version` the installed CLI does not meet. `--dry-run` reads the remote base branch without moving any ref and
fetches its commit only when it is not available locally.

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
the new current slug and path; it keeps every other field, including a
cloud-usage approval, and uses schema 2, or schema 3 when it records an
approval. Infrastructure tasks keep
their identity-owned private worktree path and update only the private current
slug metadata.

The task ID and target branch are immutable. Keeping the branch stable lets the
agent reuse the one existing draft pull request; renaming an open pull request's
head branch can close it on hosting providers. The report tells the agent to
update that pull request's title and description after publication.

Rename fetches the shared and task refs to reject merged tasks, changed remote
identity, published destination collisions, local destination collisions, and
staged source/destination changes. Before it moves or rewrites anything, also
with `--dry-run`, it refuses when the installed CLI does not meet the
`minimum_cli_version` of the fetched shared branch or task branch. It writes no
Git or S3 remote. On a published
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
branch, scopes, current working changes inside those scopes, and the task's
recorded cloud-usage state.

```text
workspace-mgr task status [--repo <path>] [--manifest <path>]
```

This is a local read-only view. Use `plan` for the complete prospective
publication state. Its `cloud_usage` object reports the effective
`threshold_bytes`, the task's `limit_bytes`, the `approval` recorded in the
task manifest (`limit_bytes` and `note`), and the `pending` decision left in
this clone's private state by the last `plan` or `publish` that measured the
task above its limit; absent values are `null`.

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

Both modes fetch the shared branch first and refuse, before writing a plan,
deleting a ref, or purging anything, when the installed CLI does not meet its
`minimum_cli_version`.

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

## `workspace-mgr task approve-cloud-usage`

Record the user's explicit approval of a cloud-usage limit for this task.

```text
workspace-mgr task approve-cloud-usage --limit <size> --note <decision>
  [--allow-non-shared-head --scope-note <reason>]
  [--repo <path>] [--manifest <path>] [--dry-run]
```

The command records a decision the user already made in the task's chat. It does
not create authorization; agents run it only after the user explicitly approves
that limit. It writes only the manifest copy that the task's publications
carry, so it applies the checkout rules of `plan` and `publish`: a deliverable
approval runs from the shared checkout on the base branch while the task branch
is not checked out anywhere, and an infrastructure approval runs from the
task's managed worktree. In an explicitly authorized alternate workflow, where
the deliverable is published from another checkout head with
`--allow-non-shared-head --scope-note <reason>`, the approval takes the same
override, with the same one-line scope note, and the task branch must still not
be the checkout's head. Any other checkout, such as an infrastructure
worktree's copy of a merged deliverable without that override, is refused
before anything is written, also with `--dry-run`. Every task's default limit
is the fixed 1 GiB (1073741824 bytes) threshold. `--limit` is a byte count
or a number with a decimal unit (`B`, `KB`, `MB`, `GB`, `TB`) or binary unit
(`KiB`, `MiB`, `GiB`, `TiB`), case-insensitive, with or without a space. A
fraction needs a unit and must come to a whole number of bytes; bare `K`, `M`,
`G`, and `T` suffixes are rejected. The limit must be at least the threshold,
because an approval can only raise the limit, and at most 9223372036854775807
bytes, the largest integer the manifest can hold. `--note` is required, must be
one line, and records the user's decision.

The approval is written into the task manifest as a `[cloud_usage_approval]`
table with `limit_bytes` and `note`, which makes it a schema 3 manifest; the
[configuration reference](configuration.md#task-manifests) describes the
format. The command rewrites the manifest atomically, validates the result, and
restores the previous manifest if validation fails. It replaces any earlier
approval, and a limit equal to the threshold removes the table and returns the
manifest to schema 2. `task rename` keeps the approval, and confirmed
`task discard` removes it with the task. The command reads no remote and
reports `remote_writes: false`.

A deliverable manifest lies inside the task directory, so the next publication
carries the change for review. If the published `.workspace-mgr.toml` does not
yet require a release that reads schema 3, that publication also raises its
`minimum_cli_version`, as described under `plan`; a build older than that
release refuses to publish the approval at all. After a reset, the next
publication withdraws a raise that no task manifest in it still needs, but
never below the base branch's declaration. An infrastructure manifest stays
private and never raises it. While the manifest records an approval, every
publication commit carries an audit trailer:

```text
Cloud-Usage-Approval: limit_bytes=<n>; note=<note>
```

The trailer is written for reviewers; `workspace-mgr` never reads it back. For
an infrastructure task it is the only published record of the approval.

The report contains `status`, `operation`, `task_id`, the `manifest` path, the
manifest's resulting `schema_version`, `threshold_bytes`,
`previous_limit_bytes`, `limit_bytes` with its readable `limit`, `note`, the
`pending` decision from the last over-limit `plan` or `publish`, `blocked`,
`remote_writes`, and `next_step`. `status` is `recorded` when the manifest
changed, `dry_run` for a rehearsal, and `unchanged` when the manifest already
records exactly this decision, such as the same approval recorded again or a
reset of a task that has no approval. Nothing is written then, and `next_step`
says that this command changed nothing instead of pointing to a publication;
for a deliverable it adds that `plan` shows whether earlier manifest changes,
such as the same decision recorded before, are still unpublished.
`blocked: true` means the pending projection from that last measurement still
exceeds the new limit, so the approval alone does not unblock the task; if the
user also chose a cleanup, perform it and let `plan` re-measure. The command
works with or without a pending decision, so the user can approve a limit before
large content is produced. Run `plan` afterward to re-measure the task, then
`publish`. `--dry-run` validates and reports without writing the manifest; its
`next_step` says that nothing was recorded and that the command must be rerun
without `--dry-run` once the user has approved the limit.

```sh
workspace-mgr task approve-cloud-usage --limit 1.5GiB \
  --note "The user approved 1.5 GiB for the training checkpoints"
workspace-mgr task approve-cloud-usage --limit 3GiB \
  --note "The user approved 3 GiB in this chat" --dry-run
```

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
non-empty single line. S3 must be configured before selecting it, and an S3
target path may not contain a backslash, which the storage engine reads as a
separator. Setting a directory creates one recursive boundary; nested or
overlapping existing boundaries are rejected. The command updates local desired
state and reports `remote_writes: false`. Explicit S3 below the recommended
1 MiB aggregate boundary size remains valid but reports `small-s3-boundary`;
select Git or a larger meaningful boundary when practical.

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
Because the reset applies automatic policy immediately, resetting a path whose
name contains a backslash into S3 is refused; the explicit choice is rolled back
and kept.

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
the destination must not. An S3 boundary exists once its metadata does, so its
payload need not be materialized. A move may stay within a containing directory
boundary or move the boundary itself; it may not cross into or out of another
directory boundary. The command changes local desired state only. A later
`publish` writes and verifies the new S3 object path, publishes the Git revision,
then permanently deletes every version of the old path unless another current
remote branch or tag still references it.

The recorded S3 version belongs to the old object path, so the move discards it.
When the source payload is not materialized, `move` therefore first fetches it
through the old metadata, before it changes anything, and then materializes it
at the destination, where the next publication uploads it under the new path. A
fetch failure leaves everything as it was.

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
locks, preview state, or private cloud-usage state. It never creates a commit,
uploads S3 content, or pushes a Git branch.

Plan also measures the task's cloud usage and reports it in `cloud_usage`,
directly after `status` and `operation`:

- `status` is `within_limit` when the projected total is at most the limit and
  `approval_required` otherwise; `publish_allowed` says whether `publish` would
  pass the usage gate, and `cleanup_only` whether this publication only removes
  content, apart from at most 1 MiB (1048576 bytes) of new workspace-mgr
  control-file content per publication, where metadata that only drops entries
  is free.
- `threshold_bytes` is the fixed 1 GiB (1073741824 bytes) threshold,
  `limit_bytes` the task's effective limit, and `approval` the approval
  recorded in the task manifest (`limit_bytes` and `note`) or `null`.
- `published` and `projected` each report `git_bytes`,
  `git_uncompressed_bytes`, `git_lfs_bytes`, `s3_bytes`, and `total_bytes`.
- `git_measure`, `headroom_bytes`, `suggested_limit_bytes`, and
  `git_history_exceeds_limit` qualify those totals.
- `contributors` lists up to ten of the largest paths with `store` (`git` or
  `s3`), `bytes`, `versions`, and `state` (`published` or `pending`). Git
  contributor bytes are compressed estimates when `git_measure` is `packed`,
  so they compare with the packed totals, and uncompressed object sizes
  otherwise; both include referenced Git LFS objects.
- `message` explains the decision the task is waiting for and appears only when
  approval is required.

Published usage is what the task already keeps on the remotes: the Git objects
its published branch adds beyond the base branch, plus every S3 object version
its publications retain at paths that still exist, including superseded
versions. Projected usage adds this publication: new Git objects from the
preview tree, and S3 uploads for new automatic placements and for changed or
not-yet-uploaded content, minus S3 paths this publication removes. Git bytes are
uncompressed object sizes plus the sizes of referenced Git LFS objects. When a
total exceeds the limit, Git objects are measured again as packed data, which
is closer to what a hosting provider stores; `git_measure` then reads `packed`,
and `git_uncompressed_bytes` keeps the uncompressed figure. Published Git
history never shrinks, so `git_history_exceeds_limit: true` means only an
approval or task discard can resolve the decision. `suggested_limit_bytes` is a
ceiling with headroom for continued work: the projection plus the larger of 25%
or 256 MiB (268435456 bytes), rounded up to a multiple of 256 MiB, and at least
256 MiB above the current limit when approval is required.

Plan never refuses because of cloud usage. An `approval_required` plan records
the pending decision in the task's private `cloud-usage.json`, which
task-scoped commands remind about and `task status` reports; a `within_limit`
plan clears it, and the file exists only while a decision is pending. The
user's approval is not private state: it lives in the task manifest. Plan also
keeps a disposable measurement cache in `cloud-usage-cache.json` in the same
private task state.

Placement is evaluated before usage is measured. A plan that finds an S3
boundary the storage engine cannot address therefore refuses with status 2 for
that boundary instead of reporting `cloud_usage`, and leaves an earlier pending
decision as it was until the renamed task is measured again.

Plan refuses, like `publish`, when the installed CLI does not meet the
`minimum_cli_version` of the fetched base branch or task branch. Plan also
previews how `publish` reconciles that declaration with the task manifests in
the published tree, as the
[configuration reference](configuration.md#minimum-workspace-mgr-version)
describes: a task that needs no newer release keeps the configuration of the
point where its branch left the base branch, a schema 3 manifest that records
a cloud-usage approval raises the declaration to at least 0.4.0, a branch
whose manifests no longer need its earlier raise withdraws it but never below
the fetched base branch's declaration, and a branch whose configuration
carries a user-authorized change keeps it and only raises its declaration,
also to follow the base branch. When a task manifest needs a newer release
than the installed CLI, plan and publish refuse with status 2 because this
build cannot publish that schema. The refusal offers recording the default
limit to remove the approval only when that manifest is the task's own; for
another task's manifest in the publication, such as one merged on the base
branch, it names the manifest and asks only for an update. The
reconciliation rewrites `.workspace-mgr.toml` in the private preview index
only and lists it in `changed_paths` although it is outside the declared
scopes. When the published declaration differs from the task branch's, plan
reports `repository_requirement` directly after `changed_paths`:

- `path` is `.workspace-mgr.toml`;
- `change` is `raise` when a task manifest needs a newer release, `follow`
  when the publication takes the fetched base branch's higher declaration,
  either because a task manifest needs a newer release or because the task
  branch must not declare less than the base branch, and `withdraw` when no
  task manifest in the publication needs the task branch's earlier raise any
  more and the base branch declares less;
- `minimum_cli_version` is the published declaration, or `null` when the
  withdrawal removes it;
- `previous_minimum_cli_version` is the declaration in the publication's
  `.workspace-mgr.toml` before the reconciliation, or `null`;
- `task_manifest_schema` is the manifest schema that needs the newer release,
  or `null` when no manifest drives the change.

The field is omitted when the published declaration equals the task branch's.
Plan and publish never change the shared checkout's `.workspace-mgr.toml`; it
receives the declaration when the merged publication is refreshed. A
publication whose tree needs a raise but has no `.workspace-mgr.toml` is
refused.

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
branch for its first publication, and includes only resolved scopes plus the
reconciled `.workspace-mgr.toml` that `plan` describes. The remote branch
object ID is verified after push. The checkout and shared Git index are not
switched to the task branch. When an infrastructure publication changes
`.workspace-mgr.toml`, publish also writes the published file into the task's
isolated worktree, which has the task branch checked out, so that worktree
stays consistent with its branch.

Before it places, commits, or uploads anything, publish evaluates the same
`cloud_usage` report as `plan` and refuses with status 2 when
`publish_allowed` is false: the projected total exceeds the task's limit and
the publication is not cleanup-only. A cleanup-only publication uploads
nothing to S3 and adds or changes no Git content other than workspace-mgr
control files (task manifests, placement records, S3 metadata, `.gitignore`
files, and the root `.workspace-mgr.toml`); every other change is a deletion.
It may carry at most 1 MiB (1048576 bytes) of new workspace-mgr control-file
content per publication, where metadata that only drops entries is free: each
added or changed control file that the remote does not hold yet is charged its
full new size, whether it grew, kept its size, or shrank. The exception is S3
metadata in which every entry names an object path and version (or, where no
version is recorded, content) that the metadata it replaces already names: it
is charged only for the lines it does not share with that version. Each added
control file is also charged its path plus 28 bytes for the entry it adds to
the Git trees above it. More new control-file content counts as added content.
A cleanup-only publication remains allowed while the task is over its limit.
The refusal prints no report. Its message names the task, the published and
projected Git, S3, and total usage, and the limit, says that the task is
waiting for the user's decision, and points to `plan` for the largest
contributors. The gate is not the first refusal: placement is evaluated before
usage is measured, so a publication that also holds an S3 boundary the storage
engine cannot address is refused for that boundary, before any usage is
measured or recorded.

A real publication checks again after local placement and S3 metadata are
committed but before the upload, and again from the final Git tree before it
creates the commit, because content can change while it runs. A late refusal
leaves local placement and S3 metadata applied, like other publication failures
after placement. A refusal from the final check can also leave objects already
uploaded to S3 unreferenced; they count as projected usage until a later
publication succeeds. Every evaluation records or clears the pending decision.
The report's `cloud_usage` reflects the final check.

The commit message ends with trailers in this order: `Workspace-Task`,
`Workspace-Scope`, one `Scope-Authorization` per authorized additional scope,
then `Workspace-Requirement` when the publication changes the task branch's
`minimum_cli_version`, and `Cloud-Usage-Approval` while the task manifest
records an approval:

```text
Workspace-Requirement: minimum_cli_version=<version> (task manifest schema <n>)
Cloud-Usage-Approval: limit_bytes=<n>; note=<note>
```

The requirement trailer above records a raise. A follow reads
`minimum_cli_version=<version> (task manifest schema <n>; follows
<remote>/<branch>)`, or `(follows <remote>/<branch>)` when no manifest drives
it. A withdrawal reads `minimum_cli_version=<version> (withdraws this branch's
raise to <version>; no task manifest in this publication needs it)`, with
`minimum_cli_version removed` in place of the first value when neither the
task's starting point nor the base branch declares anything. Both trailers are written for reviewers only.
The report includes `repository_requirement` as described for `plan`.

`publish --dry-run` performs the same non-publishing behavior as `plan` while
still requiring a message argument, and also rehearses the cloud-usage gate:
unlike `plan`, it refuses with status 2 wherever a real publication would be
refused before placement.
Publication reports the same `warnings` and `ignored_paths` as `plan` and
applies the same refusals before it changes placement or uploads content.

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

Before it changes anything, refresh reads `minimum_cli_version` from the
incoming revision's `.workspace-mgr.toml` and refuses with status 2 when that
revision requires a newer CLI, leaving the checkout untouched. After the user
approves and completes the update, rerun refresh.

An incoming S3 boundary whose path contains a backslash is the one exception.
The storage engine reads the backslash as a path separator in some commands,
including the one that verifies content, so that boundary cannot be verified,
and refresh hydrates only what it verifies. Handing it to the engine would fail
and roll back the whole refresh, and refusing the whole refresh would freeze
inbound synchronization for every checkout over one path. Refresh detects those
boundaries before it changes the branch, the index, the working tree, or stored
content, then advances everything else and leaves only their payload
unhydrated. It lists them in `storage.unaddressable` and reports one
`unaddressable-storage-metadata` entry in `warnings`, which names them and the
recovery. `refresh --dry-run` reports the same condition, so a preview never
reports plain success for a refresh that would leave a boundary behind.

Refresh cannot replace or verify a payload at such a path either, so it refuses,
before anything changes, when this checkout already holds one that the incoming
metadata does not describe byte for byte, or holds one without metadata beside
it. A payload that already matches is kept.

Until such a boundary is renamed, `storage hydrate` refuses it, and a
scope-wide `storage hydrate` refuses its whole scope; name the other boundaries
to hydrate them. Recover each boundary with the user's authorization in an
infrastructure task, which starts from the fetched base branch and so needs no
refresh. Declare as its scope the directory that holds the boundary, and the
directory that will hold the destination if that differs: the rename rewrites
each directory's `.gitignore` and both metadata files. In the task's worktree,
`move` the boundary to a path without backslashes, which fetches its payload
and materializes it at the destination; hydrate the other boundaries in those
directories by naming them, because publication requires every boundary in its
scope to be present; then publish the task and merge it. A later refresh
hydrates the renamed boundary in every checkout.

## Help and version

```sh
workspace-mgr --help
workspace-mgr storage set --help
workspace-mgr --version
```

Help describes syntax. Repository operating policy comes from
`workspace-mgr instructions`, which is why the generated `AGENTS.md` invokes
`instructions` rather than `help`.
