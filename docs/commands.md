# Command reference

This reference describes the public CLI. Run `workspace-mgr --help` or
`workspace-mgr <command> --help` for the same syntax at the installed version.
The [user guide](guide.md) explains how the commands form one workflow.

## Conventions

- Repository paths are relative to the Git root, even when a command is run
  from a task directory. `/` is their only separator: on the supported Linux
  and macOS targets a backslash is an ordinary file-name character that is
  never rewritten, so typed paths, Git paths, and storage metadata compare
  exactly. The established storage path convention excludes backslashes, so an S3
  boundary path may not contain one. Automatic placement, explicit
  `storage set --to s3`, and a `storage reset` whose automatic policy selects
  S3 refuse such a path before any metadata is written, and `move` refuses it
  as a destination for content already in S3; rename the path, or keep it in
  Git with `storage set --to git`. Storage metadata that an earlier release
  left at such a path remains excluded from normal operations, so `plan`, `publish`,
  `storage hydrate`, `storage set`, and `untrack` refuse it with status 2 and
  a `workspace-mgr move` recovery hint.
- `--repo <path>` selects the starting repository or task path and defaults to
  the current directory.
- Task-scoped commands discover `.workspace-mgr-task.toml` from the starting
  path for deliverable tasks. Infrastructure tasks always require their private
  manifest selected explicitly with `--manifest <path>`.
- `--include <path>` records a user-authorized one-invocation scope and requires
  a one-line `--scope-note <reason>`. It does not create authorization. Repeat
  `--include` for multiple paths.
- `--dry-run` previews local mutation for commands that support it. Task discard
  also saves a private revision-bound confirmation plan.
- Human output is concise YAML, except Markdown from `instructions`, TOML
  from `config show`, the compact table from `task list`, and bare paths from
  `task path` or `task list --paths`. Use global `--format json` or set
  `WORKSPACE_MGR_FORMAT=json` for stable structured output.
- Errors exit with status 2 and start with `workspace-mgr:`.
- The offline discovery commands `task list`, `task path`, and `task show`,
  help output, and argument errors skip the update check. Other invocations,
  including `--version`, perform a best-effort cached update check. A newer
  applicable release produces exactly one `workspace-mgr: update available`
  line on stderr; stdout, structured output, and command exit status are
  unchanged. The CLI never updates itself. Agents report the versions and ask
  the user before updating, then run `workspace-mgr setup`; scaffold changes are
  reconciled with `workspace-mgr manage` in an infrastructure task.
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

Verify Git and the built-in native storage engine.

```text
workspace-mgr setup [--runtime-dir <path>] [--dry-run]
```

No separate runtime or package download is needed. `--runtime-dir` remains an
accepted compatibility option and has no filesystem effect, including when it
names an existing user directory. Setup leaves former runtimes unchanged.

## `workspace-mgr manage`

Adopt a repository, update its scaffolding and migrate supported legacy storage.

```text
workspace-mgr manage [--repo <path>]
  [--s3-url <url> [--s3-endpoint-url <url>]]
  [--cancel-migration]
  [--dry-run]
```

`--s3-url` must use `s3://` and is a tracked, non-secret storage location; userinfo,
queries, fragments, and other credential-bearing URL forms are rejected.
Re-running `manage` validates public configuration and deterministically repairs
or upgrades product-owned `AGENTS.md` and the generated root `.gitignore`.
`.workspace-mgr.toml` directly configures the native storage engine. Management
does not generate a second remote configuration or engine-specific ignore file.
An unowned existing `AGENTS.md` blocks first adoption before files change.

The root `.gitignore` is the one reserved path a repository is likely to have
arranged for itself long before workspace-mgr existed, so the product owns it
only once it wrote it, which the generated first line records. A root
`.gitignore` that the product did not generate has its repository rules preserved in
`.workspace-mgr/repository.gitignore` as part of the same transaction. An
incompatible existing module blocks adoption. Below the generated header the
file is product-owned, so a hand edit there is drift that `manage` repairs.
Managed local-retention blocks stay in the root file separately. Migration drops
only the recognized obsolete DVC control rules and normalizes trailing newlines.

The command scans the whole checkout, including archived tasks, for legacy
`.dvc` manifests. It converts supported metadata to `.wm-storage.json`, carries
reusable caches and private credentials into `.workspace-mgr/local/`, and removes
recognized obsolete DVC controls. Nested Git repositories are excluded. Both
migration and scaffold changes are preflighted before the recoverable transaction
writes files. Path-based exact-version imports need no object transfer. Ordinary
DVC 2 and DVC 3 S3 content-addressed imports verify source identities and copy
opaque payloads to native object paths, including remote-only directory listings.
Each destination binds a verified exact version in a bucket with versioning
enabled and conditional writes supported; original CAS objects remain available
to old Git snapshots. A durable import journal resumes partial transfers and
reconciles lost upload responses.
`--dry-run` reports conversions, removals, scaffold actions and remote transfer
bytes without writes; it may read S3 metadata and directory listings. Legacy
adoption requires the primary shared checkout;
native scaffold reconciliation can also run in a linked worktree. See
[native storage](storage.md) for import restrictions.
The former `init` spelling remains a hidden compatibility alias.

Publish all converted manifests, their legacy sidecar deletions, obsolete
control deletions and updated configuration together, with all paths included in
the infrastructure task's scopes. Publication refuses to remove routing
controls still needed by legacy pointers in its proposed Git tree.

After a failed CAS transfer, re-run `manage` to resume its verified source
inventory and owned uploads. If a source or planned control file needs repair,
`manage --cancel-migration` abandons the pending import plan so the next `manage`
can build a fresh one. Preview cancellation with `--dry-run`. Cancellation keeps
legacy controls, payloads, cache, upload receipts and every remote version; it
performs no remote deletion. It cannot be combined with S3 routing options.

The generated root `.gitignore` is the product's fixed rules, followed by this
repository's own rules imported from
`.workspace-mgr/repository.gitignore`, followed by any
`# workspace-mgr local begin` blocks the root file already holds. The fixed
rules are written in these groups:

- private workspace-mgr state: `/.workspace-mgr/local/`;
- operating-system metadata: `.DS_Store`, `._*`, `.AppleDouble`,
  `.LSOverride`, `__MACOSX/`, `Thumbs.db`, `ehthumbs.db`, `[Dd]esktop.ini`,
  `.directory`, `.fuse_hidden*`, `.Trash-*`, `.nfs*`;
- editor swap, backup, and per-user state: `[._]*.sw[a-p]`, `*~`, `\#*\#`,
  `.\#*`, `*.iws`, `.idea/**/workspace.xml`, `.idea/**/shelf`;
- Python bytecode, environments, and tool caches: `__pycache__/`,
  `*.py[codz]`, `*$py.class`, `*.egg-info/`, `.eggs/`, `.venv/`, `venv/`,
  `__pypackages__/`, `.pdm-build/`, `.ipynb_checkpoints/`, `.pytest_cache/`,
  `.mypy_cache/`, `.dmypy.json`, `.ruff_cache/`, `.pytype/`, `.pyre/`,
  `.tox/`, `.nox/`, `.hypothesis/`, `.coverage`, `.coverage.*`, `htmlcov/`,
  `cython_debug/`, `__marimo__/`, `.ropeproject`;
- JavaScript dependencies, caches, and framework output: `node_modules/`,
  `.npm/`, `.pnpm-store/`, `npm-debug.log*`, `yarn-debug.log*`,
  `yarn-error.log*`, `.eslintcache`, `.stylelintcache`, `*.tsbuildinfo`,
  `.parcel-cache/`, `.next/`, `.nuxt/`, `.svelte-kit/`, `.vite/`,
  `.node_repl_history`;
- R, Julia, and Rust session and tool by-products: `.Rhistory`,
  `.Rapp.history`, `.RDataTmp`, `.Rproj.user/`, `*.jl.cov`, `*.jl.*.cov`,
  `*.jl.mem`, `*.jl.*.mem`, `**/*.rs.bk`, `rustc-ice-*.txt`;
- credentials and private runtime configuration: `.env`, `.env.*`,
  `!.env.example`, `.Renviron`, `.httr-oauth`, `.pypirc`,
  `.streamlit/secrets.toml`.

The set draws on GitHub's common ignore templates but is curated rather than
their union. The private product-state rule keeps locks, private manifests,
indexes, and pending transaction records out of Git while leaving
`.workspace-mgr/repository.gitignore` and
`.workspace-mgr/instructions/repository.md` trackable. Apart from that rule and
the credentials group, a fixed rule covers output a tool regenerates under a
name that cannot plausibly be retained content. Names
that are as often retained data as build output — `target/`, `build/`,
`dist/`, `lib/`, `out/`, `docs/`, `*.log`, `coverage`, `.RData`, knitr's
`*_cache/`, and Julia's `Manifest.toml` — are left to a task's own
`.gitignore` or to `.workspace-mgr/repository.gitignore`.
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
root file through its `repository-scaffold` check. `manage` refuses to change the S3
location while retained S3 boundaries exist. It preserves higher existing
`minimum_cli_version` declarations and raises the requirement to at least
0.8.1 when adopting native storage. It does not push Git refs. Ordinary CAS
adoption reads S3 sources and creates verified native object versions;
`--dry-run` reads only the remote metadata and listings needed for its inventory.
The generated `AGENTS.md` includes an approval-gated command that
installs the latest stable release from crates.io, followed by `setup` and an
instructions retry, so a new machine can bootstrap without inventing a
lower-level workflow.

```sh
workspace-mgr manage
workspace-mgr manage \
  --s3-url s3://example-bucket/workspace
workspace-mgr manage --dry-run
```

## `workspace-mgr instructions`

Render the shared workspace model and effective agent policy.

```text
workspace-mgr instructions [all|model|core|task|publish|artifacts|storage|shared-checkout|infrastructure|repository]
  [--repo <path>]
```

With no topic, `all` is used. Both default and explicit `all` return the short
mental model, operation directory, session-wide constraints and current
repository control facts. They do not concatenate operation-specific sections.
`model` returns only the short conceptual document. Other existing topics remain
available as detailed on-demand compatibility views with the same applicable
repository policies. The relevant command's `--help` is the primary operation
entrypoint; execution output contains outcome-specific guidance.

When `.workspace-mgr/instructions/repository.md` exists, default output indexes
it and requires reading it before task work. `instructions repository`
reproduces the user-owned module; it is not silently dropped. Its current bytes
still affect `all`'s policy hash. Every response includes CLI version, product
policy version, topic and hash. Product-owned default wording remains compact
regardless of that module's length.

```sh
workspace-mgr instructions
workspace-mgr instructions model
workspace-mgr instructions storage
workspace-mgr --format json instructions publish
```

## `workspace-mgr doctor`

Diagnose one task or all tasks, the repository configuration, product-owned
scaffold, Git state, and local/S3 storage integrity.

```text
workspace-mgr doctor [<task>] [--repo <path>]
```

`<task>` resolves an exact immutable ID, current name, slug or current path,
including nested archive directories, with the same ambiguity rules as
`task path`. Without it, doctor checks all locally discoverable tasks and the
entire configured S3 prefix, including orphan keys outside current task paths.
Repository configuration and dependency checks still run for either scope.

The command is read-only. When S3 is configured, it checks bucket versioning
and compares the local storage manifests with the complete remote inventory,
including historical object versions and delete markers. Current objects must
exist at the exact repository-relative key and have the manifest's latest
version, size, ETag and checksum. A wrong directory, extra object, retired key
with surviving history, stale version binding, or invalid metadata is an error.
Older versions at a valid current key remain permitted by the existing history
policy. Known archive coordination records are checked as control metadata,
separately from payload paths; arbitrary control-looking keys are not exempt.

The audit checks materialized local bytes and directory membership against the
manifest too. Unmaterialized outputs use the manifest as the logical local tree
and are counted separately; doctor does not hydrate them. Exact remote checksum
verification downloads each current object into temporary scratch space, without
installing a cache or changing repository files. Large tasks can take time and
incur S3 read/transfer costs.

A selected task also checks its former prefixes recorded in locally available
Git history and archive receipts. Doctor never fetches refs or follows archive
aliases to make an object at the wrong path pass. A repository-wide audit finds
orphan paths even when their old task identity is no longer available locally.
The JSON report includes the selected `tasks` and structured `storage.issues`
with exact paths and diagnostic codes. Doctor exits with status 2 if any check
is unhealthy, or if the audit cannot finish; it never deletes or repairs data.

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

## `workspace-mgr task list`

List tasks in the current local repository, including directories already
grouped under time folders.

```text
workspace-mgr task list [<query>]
  [--kind deliverable|infrastructure]
  [--placement top-level|nested|repository]
  [--paths] [--repo <path>]
```

The default human output is a compact table of kind, metadata, placement,
current name, path, and title. The optional query matches a
case-insensitive substring of the immutable task ID, current basename, current
slug, title, or repository-relative path. Filters combine with the query.
Placement describes the current location: `top-level` deliverables are directly
under the repository root, `nested` deliverables are below another directory,
and `repository` infrastructure tasks have private metadata and no task
directory. Nested placement alone says nothing about completion or review.

Discovery walks the current filesystem, including ignored and untracked task
directories. It stops at each task root and does not follow symbolic links.
A root with a valid current task manifest remains a known task even if it has
its own Git controls. Other nested Git checkouts are excluded before legacy
candidate detection. Timestamped directories that have neither a task manifest
nor a nested Git checkout appear as legacy candidates; their presence does not
establish ownership or make them eligible for adoption or archive. The `metadata` field
is `managed`, `legacy`, or `invalid`. Malformed current metadata appears as
invalid with a diagnostic, so an unreadable manifest does
not silently remove the task from the list. Private infrastructure metadata is
read in place, including its previous private-state location before migration.
Discovery does not migrate or repair it.

`--paths` prints only repository-relative deliverable paths, one per line,
without a table or headings. Infrastructure entries are omitted. If the
selected deliverable metadata is invalid, the command refuses instead of
emitting an incomplete path list. With global `--format json`, normal listing
returns `{repo, tasks, warnings}` and `--paths` returns a string array.
With `--paths`, discovery warnings go to stderr in both formats, leaving stdout
as the path list or JSON array.

These discovery commands are read-only and offline: they do not fetch, query
GitHub or S3, run the update check, write a cache, or migrate private state.
Local archive receipt status is reported as `archive_status`; it is not a live
check that the task's PR merged or that S3 publication or cleanup finished.

```sh
workspace-mgr task list
workspace-mgr task list model --kind deliverable
workspace-mgr task list --placement nested
workspace-mgr task list --kind deliverable --paths
workspace-mgr --format json task list --repo /path/to/repository
```

## `workspace-mgr task path`

Resolve one task to its current deliverable directory.

```text
workspace-mgr task path <selector> [--relative] [--repo <path>]
```

The selector must exactly match the immutable task ID, current basename,
current slug, repository-relative directory path, or absolute directory path.
Matching does not use fuzzy search, an old renamed slug, or a latest-task
fallback. If more than one task matches, the command exits with status 2 and
lists candidates; choose an ID or current path that identifies one task.
Malformed current metadata also refuses resolution.

Human output is one bare absolute path. `--relative` returns the current path
relative to the repository root, even when the command starts in a task
directory. Global `--format json` returns `{repo, id, path}` and applies the
same `--relative` choice to `path`. Infrastructure tasks have no deliverable
directory, so `task path` refuses them; use `task show` to obtain their manifest.
When capturing the path in a shell command, pass `--format human` explicitly
so a `WORKSPACE_MGR_FORMAT=json` environment setting cannot change the output.

```sh
cd "$(workspace-mgr --format human task path example-task)"
workspace-mgr task path 20261007-120000-example-task --relative
workspace-mgr task path 2026/10/20261007-120000-example-task
```

## `workspace-mgr task show`

Inspect one task's current local identity and metadata.

```text
workspace-mgr task show <selector> [--repo <path>]
```

Selection follows the exact matching and ambiguity rules of `task path`, and
also supports infrastructure task identity. Human output is concise YAML;
global `--format json` returns `{repo, task}`. The task includes its immutable
ID, current name, slug, title, purpose, branch, scopes, placement, and absolute
manifest path. Deliverable directory paths are repository-relative;
infrastructure tasks have no deliverable directory. Legacy candidates remain
explicitly marked as candidates, and malformed current metadata refuses.

The output describes the current filesystem and local receipt. It grants no
write scope and does not establish completion or live review status. Use
`task status` for resolved task-scoped status, `plan` for publication
assessment, and `archive --dry-run` for current archive eligibility.

```sh
workspace-mgr task show example-task
workspace-mgr --format json task show 20261007-120000-example-task
```

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
`--scope-note`; it creates the unmounted branch `codex/infra-<slug>` and a
private manifest below the primary checkout's `.workspace-mgr/local/`, with no
repository task directory or separate worktree. Both kinds work in the shared
checkout on the configured main branch. Infrastructure creation reports `path` as the repository root and
`manifest` as an absolute path; pass that path using `--manifest` to subsequent
task-scoped commands. The shared HEAD must equal the fetched base revision;
run `refresh` before creation if it is behind. Infrastructure planning and
publication also require shared HEAD to match the fetched base, so an upstream
change cannot be overwritten from stale local files; refresh before retrying.
Every infrastructure scope is
explicit. Both kinds fetch the configured base branch, reject an existing
directory or local/remote branch, and publish nothing. Before creating a branch,
directory, or manifest, they refuse a base branch whose `minimum_cli_version`
the installed CLI does not meet. `--dry-run` reads the remote base branch without
moving any ref and
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
task_manifest=/absolute/path/reported/by/task-create
workspace-mgr plan --manifest "$task_manifest"
workspace-mgr publish --manifest "$task_manifest" -m "Publish shared policy"
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
cloud-usage approval and archive completion checkpoint. It writes the lowest
schema representing those fields: schema 2, schema 3 with an approval, or
schema 4 with a checkpoint. Infrastructure tasks keep
their identity-owned private manifest path and update only the private current
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

A successful deliverable directory move reports a `notices` entry with code
`manual-content-audit-after-relocation`, reminding the user to manually inspect
and repair affected links or path references. The tool neither checks nor
repairs those payload dependencies. Dry-run, no-change and infrastructure
metadata-only rename do not emit this success notice.

## `workspace-mgr task upgrade`

Upgrade supported current task configuration without moving content or writing
a remote.

```text
workspace-mgr task upgrade
  [--repo <path>] [--manifest <path>] [--dry-run]
```

Without `--manifest`, discover the current deliverable task from `--repo` or the
working directory. Select another current task explicitly with `--manifest`.
Current manifest loading is strict: unknown fields or unsupported schemas are
not guessed. Upgrade preserves identity, scopes, cloud-usage approval and
compatible saved review metadata, including existing schema 4 fields.

The operation is local and idempotent. It fetches the configured shared branch
and validates current published manifest identity, staged manifest state and
client compatibility. It does not query PRs, inspect historical configuration,
compare ordinary task directory trees or require ordinary staged, modified or
untracked payloads to be clean.
`--dry-run` previews the same current-metadata change without rewriting it.
Reports retain `previous_schema_version`, `schema_version`,
`completion_recorded` and `remote_writes: false`; `completion_recorded` indicates
compatible saved metadata, not verified task-content history or live eligibility.
No new completion checkpoint is synthesized. A changed current manifest must
still be published within its authorized scope.

Archive is independent of upgrade: it checks supported current metadata and
current associated PR closure, not old directory history or saved proof.

## `workspace-mgr archive`

Organize completed deliverable task directories through a user-requested
repository-infrastructure task.

```text
workspace-mgr archive [<task-path> ...]
  [--layout <template>] [--repo <path>] [--manifest <path>] [--dry-run]
workspace-mgr archive [<source-or-destination> ...]
  --cancel --manifest <owning-infrastructure-manifest> [--dry-run]
```

With no paths, inspect top-level deliverable directories and skip pending tasks.
An explicitly named task with an OPEN PR is refused. Archive reads the current
task identity, branch and compatible saved branch hints, then queries their
current PR state in the configured repository through `gh`. An OPEN PR means
pending; MERGED, CLOSED without merging, or a successful query finding no
corresponding PR means done. The PR need not target today's configured base
branch. An open PR found through a saved hint still blocks archive.
Authentication, network and other hosting-query failures are reported as errors;
they are never treated as a successful no-PR result. Current configuration
remains strictly validated.

Archive does not inspect historical configuration, directory-tree history,
commit-to-PR associations, or historical checkpoint proofs. Saved review
metadata supplies branch lookup hints only; its historical tree and ancestry
checks are not replayed. A compatible pre-0.7 adoption record can provide a
current branch hint; malformed or unrelated records add no extra refusal.
Archive does not require finding an earlier branch or PR when the current
queries succeed without a match. It does not require earlier commits to have
reviewed PRs, full Git history, or retained branch tips to equal or descend from
reviewed heads. Changes to
ordinary task contents do not determine eligibility. Pending task directories
stay at the top level,
and merge or turn-end synchronization never runs archive automatically.

`--layout` uses `{year}` and `{month}` from each task directory's creation
timestamp. The default is `{year}/{month}`; `{year}` and `{year}{month}` also
work. The rendered relative path must include the year and must not target
hidden repository-control directories. The task directory's basename, retained
contents, immutable ID, and target branch are preserved.

Run `--dry-run` from the shared checkout or an infrastructure task to inspect
`tasks`, `skipped`, and `required_scopes`. It reads current PR state and
versioned S3 history but changes no repository content or remote. Applying
archive requires an infrastructure task with both source and destination paths
declared. The command moves complete local directories, updates manifest paths,
and writes `.workspace-mgr-archive.json` migration receipts. It writes neither
Git nor S3 remotes. It refuses destination collisions, invalid current task
metadata, inconsistent managed-storage pointers or hashes, and unavailable
referenced S3 generations. It validates supported pointer hashes and complete
directory metadata, verifies hydrated payload hashes, and checks exact S3
versions for unhydrated payloads. Ordinary tracked, staged, untracked,
ignored, and local-only contents move with the directory; unpublished content
alone does not prevent archive. The shared Git index is left unchanged.

Archive preserves ordinary local contents byte for byte, including scripts,
README commands, historical logs, ignored caches, symlinks, and Python
environments.
It does not scan runtime paths or cross-task dependencies, run reproduction
commands, rebuild environments, or rewrite their references. A script that
depends on the former directory may need separate maintenance after moving;
that does not prevent archive.

Every nested Git repository must be ignored by the outer repository's shared
ignore rules and contain no outer-tracked files or gitlinks. Use a repository
or task-local `.gitignore` rule covering the whole nested directory, for example
`vendor/tool/`, and ensure the rule still covers its archived destination.
Local `.git/info/exclude` or a global ignore file is insufficient because
another clone must enforce the same boundary. Archive refuses a nested
repository that violates this rule before moving. Ignored nested repositories
move unchanged: Git pointer files, registrations and external administrative
files are neither parsed for relocation nor repaired. This may leave their
old references unusable. Zero-byte `.git` cache markers are ordinary content
and do not establish a nested repository.

The rule need not already be tracked: a new task-local `.gitignore` can travel
with the task and be carried by its publication. `plan` and `publish` enforce
the same nested-repository boundary before evaluating storage placement in
the selected directory scopes; they also verify that the publication carries
its ignore rules.

The normal `plan` and `publish` flow handles the migration. Publication copies
every retained data version and delete marker under the source task prefix,
including superseded versions and retired paths absent from current storage
pointers. It verifies destination versions, rewrites standalone and directory
managed-storage cloud metadata automatically, and publishes the archive
registry before publishing Git. Content hashes and file sizes stay fixed;
copied versions and recreated markers receive new native IDs and timestamps,
which the receipt maps to their originals. Copying history is charged to the
infrastructure task's cloud-usage projection, so its full retained history must
fit that task's approved limit before migration starts.

Before the copied receipt merges into the configured shared branch, current
remote branches or tags containing the source directory defer cleanup,
including legacy trees without manifests. Once that receipt is merged, old
branches and tags can read exact mapped versions through the registry; they
remain intact and no longer require duplicate history at the original path.
New referenced generations without mappings remain protected. Retirement is
complete only after a full version-history scan finds neither data versions nor delete markers under
the original prefix. `storage.purge.status: cleanup_pending` reports protected
history or a prefix waiting for its copied receipt to reach the shared branch.
Even an archive whose original version inventory is empty keeps a durable
`pending_prefixes` intent until a complete version-and-marker scan confirms
that the old prefix is empty.
`blocked_unmapped` reports concurrent versions without a verified
mapping. Both preserve retry records and report a warning. Git's `pushed` or
`updated` status does not mean storage retirement is complete. Concurrent
unmapped or foreign data is preserved and blocks completion; it is never
silently erased or forgotten from the retry queue. A failed publication
preserves source history and retry journals. Historical Git checkouts use
`workspace-mgr storage hydrate` to resolve the durable registry and verify their
original content hashes after source cleanup. Reading old pointers directly with the underlying storage engine cannot
resolve the changed keys and version IDs.

Archive publication requires `minimum_cli_version = "0.7.0"` regardless of
the task manifest schema, including archives with an empty S3 inventory.
The CLI reconciles this declaration in its private publication index before
upload. Private S3 purge queues and archive copy journals use schema 2; 0.6.0
rejects them before deletion rather than ignoring newer protection fields.
The new CLI can read legacy schema 1 private state, but a destructive retry
durably upgrades it first. Preview leaves old bytes unchanged. Public copied
receipts and the immutable registry remain schema 1, so exact historical
version mappings keep their data format. Cancellation also upgrades restored
cleanup state; do not remove the repository version declaration or downgrade
private journals to resume with an older CLI.

Before copying, an immutable source reservation under
`refs/tags/workspace-mgr/archive-copy/` chooses one attempt by Git
compare-and-create, so competing publishers cannot both copy into the same
destination. The canonical registry is then bound to the complete copied
receipt by a separate control tag under
`refs/tags/workspace-mgr/archive-registry/` on the configured Git remote.
The reservation binds a normalized planned receipt and its private journal's
attempt nonce; the canonical binding records the complete copied receipt.
Exact remote object ID checks guard mutations. Conflicting receipts or registry
history refuse publication and cleanup.
Neither ownership claim expires or permits takeover. The remote must permit
creating and conditionally deleting both sets of control tags.
For Backblaze B2, the registry writer disables automatic SDK checksum headers
and sends Content-MD5. It first attempts conditional Put; a provider's explicit
not-implemented/not-supported response permits an unconditional fallback only
while the verified Git binding owns this exact receipt. Source history is
retired under the same completion checks as other providers. Same-key copy
generations are spaced by at least one second to preserve B2's current version
ordering. Unknown endpoints keep conditional publication and fail safely if
their provider rejects it.

`--cancel --dry-run` previews a journaled local attempt. Apply restores its
source directory, original manifest, receipt, storage pointer bytes and permissions.
All local payloads, including nested Git controls,
ignored and hydrated content and files added after moving, travel with the
directory. Cancellation changes neither the shared Git index nor another
task's files. A failed publication's generated local tree and retirement queue
are reversed only for the selected archive paths; other scoped work is retained.
Independent metadata/ref edits and destination collisions refuse cancellation
before movement. Repeating cancel is safe, including after interruption.
Existing attempt journals remain readable, including saved relocation
metadata from older attempts; new attempts do not rewrite nested Git controls.

Cancellation verifies that all original source versions and markers remain
intact, withdraws only this attempt's exact registry versions, deletes its
copied data versions and delete markers, and aborts its recorded multipart
uploads. It records their verified absence durably, restores local directories,
metadata, refs and retirement state, removes its empty generated parents, then
releases its exact canonical binding and copy reservation before marking the
attempt cancelled. Preview performs none of these writes. Foreign destination
data, changed original source versions, or a conflicting registry blocks this
initial remote cleanup and preserves a resumable attempt.
Once remote cleanup is durably verified, an interrupted retry can finish local
restoration while preserving any later foreign writes or a newer owner's
claims. Completed cancellation does not require the source generations to
remain after a newer archive retires them. Retries use exact version IDs without
creating new delete markers. A subsequent archive starts a fresh copy transaction
rather than reusing cancelled copies.
After verified Git push, use a reviewed revert. Receipts from an older CLI that
did not save the local attempt journal cannot provide a verified lossless cancel.

Legacy directories without a manifest appear in `skipped` with an adoption
instruction. First adopt explicitly through an infrastructure task scoped to
that directory, supplying its title and purpose. `--pull-request` is optional.
Without it, adoption writes current task metadata without querying old PRs or
creating a review record. If supplied, the known merged PR is verified in the
configured same repository and base branch, and its branch hint is saved.
Adoption preserves ordinary contents and does not compare them to historical
trees or parse old task configuration. Its infrastructure PR need not merge
before archive. Existing managed tasks do not need adoption or an invented
review association merely because their current branch has no matching PR.

```sh
workspace-mgr task adopt <legacy-task-path> \
  --title "Retained task" --purpose "Retain task materials" \
  --manifest "$task_manifest" --dry-run
# Repeat without --dry-run; publish the adopted control metadata through its normal review.
# Add --pull-request <number> only to record a known merged PR.
```

Cancel only an archive attempt that has already moved locally and remains
unpublished. Adoption alone does not create such an attempt:

```sh
workspace-mgr archive <source-or-destination> --cancel --manifest "$task_manifest" --dry-run
workspace-mgr archive <source-or-destination> --cancel --manifest "$task_manifest"
```

```sh
workspace-mgr archive --dry-run
workspace-mgr archive --layout '{year}{month}' --dry-run
# Apply in the shared checkout after declaring the reported scopes.
task_manifest=/absolute/path/reported/by/task-create
workspace-mgr archive --manifest "$task_manifest"
workspace-mgr plan --manifest "$task_manifest"
workspace-mgr publish --manifest "$task_manifest" -m "Organize completed tasks"
```

After a successful directory move, `notices` includes
`manual-content-audit-after-relocation`. It requests a manual audit of the moved
payload's links and path references; archive itself never inspects or repairs
them. Preview, cancellation and invocations with no move do not emit the
success-only reminder. This notice is not a runtime precondition or refusal.

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

- every working change in the task's declared scopes;
- the deliverable task directory to delete, when present;
- each shared scope to restore from the local shared branch;
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
contained in the shared branch, a target branch checked out anywhere, or an
invocation whose current directory would be deleted. It deletes an existing
remote task branch with `force-with-lease`, verifies absence, deletes local and
remote-tracking refs, then removes the local workspace and private task state.
Deliverable scopes are first moved into private quarantine; additional scopes
and their shared-index entries are restored from the local shared branch. A
remote failure restores quarantined paths and their prior index state.
Infrastructure confirmation restores its declared scopes, removes its private
manifest and task state, and never deletes the shared repository directory.

`task discard` does not verify pull-request state itself; its report makes that
agent responsibility explicit. Before branch deletion,
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
carry, so it applies the checkout rules of `plan` and `publish`: both kinds run
from the shared checkout on the base branch while their task branch is not
checked out anywhere. Infrastructure approval requires its explicit private
`--manifest` path. In an explicitly authorized alternate workflow, where
the deliverable is published from another checkout head with
`--allow-non-shared-head --scope-note <reason>`, the approval takes the same
override, with the same one-line scope note, and the task branch must still not
be the checkout's head. This override is deliverable-only; infrastructure tasks
always use the configured shared branch. Any other unauthorized checkout is
refused before anything is written, also with `--dry-run`. Every task's default
limit is the fixed 1 GiB (1073741824 bytes) threshold. `--limit` is a byte count
or a number with a decimal unit (`B`, `KB`, `MB`, `GB`, `TB`) or binary unit
(`KiB`, `MiB`, `GiB`, `TiB`), case-insensitive, with or without a space. A
fraction needs a unit and must come to a whole number of bytes; bare `K`, `M`,
`G`, and `T` suffixes are rejected. The limit must be at least the threshold,
because an approval can only raise the limit, and at most 9223372036854775807
bytes, the largest integer the manifest can hold. `--note` is required, must be
one line, and records the user's decision.

The approval is written into the task manifest as a `[cloud_usage_approval]`
table with `limit_bytes` and `note`, which requires at least schema 3; the
[configuration reference](configuration.md#task-manifests) describes the
format. The command rewrites the manifest atomically, validates the result, and
restores the previous manifest if validation fails. It replaces any earlier
approval, and a limit equal to the threshold removes the table. The manifest
returns to schema 2 only when it has no archive completion checkpoint;
otherwise it remains schema 4. `task rename` keeps the approval, and confirmed
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

`semantic-placement-review` reports an automatic Git size fallback from
1 through 10 MiB: review whether collaboration or artifact semantics warrant an
explicit choice. `small-s3-boundary` reports a materialized S3 boundary below
1 MiB, measured by aggregate regular-file bytes. These are placement advice,
not refusals; an explicit user choice still succeeds at any size.

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
Shared legacy CAS sources retained during migration are outside this native
path retirement and are not automatically garbage-collected; see
[legacy import retention](storage.md#migrating-legacy-dvc-repositories).

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
Retained legacy CAS source hashes are not deleted by this native path cleanup.

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
a cloud-usage approval raises the declaration to at least 0.4.0, schema 4
completion evidence requires 0.7.0, a nested
archived task manifest of any supported schema requires at least 0.5.0, a branch
whose manifests no longer need its earlier raise withdraws it but never below
the fetched base branch's declaration, and a branch whose configuration
carries a user-authorized change keeps it and only raises its declaration,
also to follow the base branch. When a task manifest needs a newer release
than the installed CLI, plan and publish refuse with status 2 because this
build cannot publish that task state. The refusal offers recording the default
limit only when removing the task's own approval clears its schema requirement;
an archived-path requirement needs an update. For another task's manifest in
the publication, such as one merged on the base branch, it names the manifest
and asks only for an update. The reconciliation rewrites `.workspace-mgr.toml`
in the private preview index only and lists it in `changed_paths` although it
is outside the declared
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
- `task_manifest_schema` is the actual schema of the manifest that drives the
  newer requirement, or `null` when no manifest drives the change. The path
  may drive that requirement, so an archived schema 2 manifest reports `2`
  while requiring 0.5.0.

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
produced nothing worth recording. `plan` counts the S3 metadata of outputs the
storage engine reports files added to, changed in, or removed from, which
`publish` rewrites when it commits them, so both give the same advice; metadata
whose objects are only missing from the local cache is committed back unchanged
and counts in neither. When the task is over its cloud-usage limit and the
publication is allowed only because it is cleanup-only, the warning adds that
the publication should go out as it is and the decision be recorded in the first
publication the limit allows, because a record added to it would be content the
limit refuses. `bulk-publication` reports a publication that adds more than
200 new files, or more than 256 MiB (268435456 bytes), of new content inside the
task directory; the thresholds are fixed product policy. New content routed to
S3 by automatic placement is counted from the placement decision and measured on
disk, because its payload never reaches the private index, and its pointer is
not counted a second time once `publish` has written it; an explicitly selected
boundary counts as the one pointer it adds. A file that only moved — every
published file of a renamed task, for instance — is not new content and is not
counted. Both are checks rather than refusals.

Plan refuses, before it changes placement or uploads anything, a deliverable
publication that would add or change content inside its own task directory while
that directory documents nothing, and any staged symbolic link whose target is
outside the repository. A task documents itself with Markdown files of its own
choosing inside its directory; a README still carrying only the creation
scaffold's directory map is not yet a record. A storage pointer or placement
record counts as the content it addresses, so a result routed to S3 is judged
like one kept in Git. The staged metadata of a boundary shows a change to its
outputs only once `publish` commits it, after placement and just before the
upload, so for a task that documents nothing the preview asks the storage engine
whether any boundary in the task gained or changed files. A file the engine
cannot compare because its directory's manifest is in neither the local cache
nor the remote counts only when the directory's aggregate digest changed.
`publish` judges the metadata the engine commits again before it uploads
anything, because a background writer may change a boundary's outputs after the
preview; when that check or the cloud-usage re-check refuses, the metadata the
engine committed is restored, so a retry after the late content is gone is not
judged by it. A publication that only retires content is not refused; one that
removes the task's last record while publishing content is refused by name.
Removing files from a directory boundary in S3 retires content, whether or not
the storage engine has committed the boundary yet: the rewritten metadata names
only files the published metadata names, with the same digest or stored
version, and every line it adds records one of those entries. A description,
`meta`, labels, or a comment added to the metadata is text it publishes, so such
a rewrite counts as content. Untracking published content retires it too: the
placement record `untrack` writes then takes the payload or metadata the task
published out of Git and S3, so it does not count as publishing content, and a
task that documents nothing can still publish the cleanup a user chose while the
task is over its cloud-usage limit. A result kept local before it was ever
published retires nothing, so its placement record, the only durable trace of
it, counts as the content it addresses, except while the task waits for the
user's cloud-usage decision: a record added to that cleanup would be growth the
limit refuses, so the record of such a result is judged once the gate has
measured, and counts as content only when the task is within its limit. The
symbolic-link check reads the staged tree, so a link inside a boundary already
placed in S3 or kept local with `untrack` is not classified.

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

All of these refusals are decided on the preview, before the cloud-usage
measurement, so a refused plan or publication records no pending cloud-usage
decision, and the usage `plan` reports afterwards describes the publication
those guards accept.

`--allow-non-shared-head` is a deliverable-only checkout override and requires a
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
switched to the task branch. Both kinds publish through a private index; an
infrastructure task is selected by its private `--manifest` path. A
`.workspace-mgr.toml` reconciled only for publication stays in the Git tree
without overwriting the shared checkout's file.

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
full new size, whether it grew, kept its size, or shrank. A canonical native
directory manifest that only removes entries, preserving every retained entry's
path, checksum, physical size and exact version binding, is free. Legacy
metadata that only removes existing object identities is charged only for
the lines it does not share with the replaced version. Each added
control file is also charged its path plus 28 bytes for the entry it adds to
the Git trees above it. More new control-file content counts as added content.
A cleanup-only publication remains allowed while the task is over its limit.
The refusal prints no report. Its message names the task, the published and
projected Git, S3, and total usage, and the limit, says that the task is
waiting for the user's decision, and points to `plan` for the largest
contributors. The gate is not the first refusal: placement is evaluated before
usage is measured, so a publication that also holds an S3 boundary the storage
engine cannot address is refused for that boundary, before any usage is
measured or recorded. The same holds for the refusals `plan` describes: a
deliverable that documents nothing, a staged symbolic link that escapes the
repository, and a path only a machine-local ignore rule hides are refused before
usage is measured, because resolving them can change what the publication holds.

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
The trailer retains the requiring manifest's actual schema even when its
archived path, rather than its schema, requires the newer release.

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
deletions, and hydrates incoming S3 boundaries. It reads Git, optional GitHub
merge evidence, and S3. After synchronization succeeds, it can delete verified
merged refs on the configured Git remote and retry already pending,
unreferenced S3 purge paths.

Before it changes anything, refresh reads `minimum_cli_version` from the
incoming revision's `.workspace-mgr.toml` and refuses with status 2 when that
revision requires a newer CLI, leaving the checkout untouched. This check comes
first, before refresh inspects any incoming storage metadata, including the
unaddressable boundaries described below, because a newer release may write
metadata this one cannot read; `refresh --dry-run` applies it the same way.
After the user approves and completes the update, rerun refresh.

After incoming materialization and storage verification succeed, refresh
automatically checks local and configured-remote branches for cleanup, even
when the shared branch was already current. `--dry-run` reports planned
deletions without deleting refs.

Cleanup uses the installed, authenticated GitHub CLI (`gh`) to verify a merged
same-repository pull request against the configured base. Its merge commit must
be reachable from the fetched base, and its recorded head must match every
remaining local and remote ref for that branch. Squash merges qualify; new
local commits, resumed branches, open or ambiguous pull requests, fork pull
requests, and other-base pull requests do not. The configured base, current and
default branches and protected remote branches are kept. Remote deletion uses
an exact lease; local deletion checks the expected head again.

Branches checked out in any legacy or custom worktree are skipped. Refresh
never detaches a worktree or removes its directory or files.

The `branch_cleanup` report contains:

| Field | Meaning |
| --- | --- |
| `status` | `dry_run`, `complete`, `unavailable`, or `not_applicable`; `complete` may still have skips or errors |
| `planned`, `deleted` | Entries name `branch`, `head_oid`, `pull_request`, and whether `local` or `remote` refs are involved |
| `skipped` | Retained branches with a `reason` |
| `errors` | Failed cleanup attempts with `branch`, `action`, and `error` |
| `warnings` | Actionable cleanup warnings |
| `remote_writes` | Whether cleanup confirmed a configured-remote branch deletion |

For a non-GitHub remote, cleanup returns `not_applicable` without a warning.
Unavailable GitHub CLI/authentication returns `unavailable`, keeps all refs,
and reports a warning. Cleanup errors and warnings do not undo successful
shared-branch synchronization. Top-level
`warnings` use `branch-cleanup-unavailable` and `branch-cleanup-failed` codes;
the detailed report names each branch and failure. Retained task
directories and payloads are not deleted by branch cleanup. The normal pending
S3 purge retry can proceed once a deleted branch no longer protects a queued
path; moving completed task directories still requires an explicit user
request and `archive` in an infrastructure task.

When the shared branch was already current but refs were deleted, refresh's
overall `status` is `branches_cleaned`.

GitHub access is optional for synchronization. To diagnose unavailable cleanup,
check the CLI and its authentication, then rerun refresh:

```sh
gh --version
gh auth status
workspace-mgr refresh --dry-run
```

An incoming S3 boundary whose path contains a backslash is the one exception.
Older storage engines interpreted backslashes inconsistently. The native
migration preserves the existing boundary convention and hydrates only
addressable paths, while allowing synchronization of everything else. Refresh
detects those
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
infrastructure task in the shared checkout. Declare as its scope the directory
that holds the boundary and the destination directory if that differs: the
rename rewrites
each directory's `.gitignore` and both metadata files. Select its private
manifest with `--manifest` and `move` the boundary to a path without backslashes,
which fetches its payload and materializes it at the destination; hydrate the
other boundaries in those
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
