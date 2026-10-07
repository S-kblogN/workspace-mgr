# workspace-mgr

`workspace-mgr` is the repository interface for coding agents. It turns repository
policy into executable instructions, creates scoped task scaffolding, chooses
whether retained content lives in Git or S3, and publishes both as one verified
transaction.

The public model has two storage locations:

- **Git** stores content directly in the repository history.
- **S3** stores content as versioned objects while Git records the small metadata
  needed to reproduce that exact state.

Content can also be explicitly kept **local only** with `untrack`: its bytes
stay in the working tree, while a small placement record and managed
`.gitignore` rule prevent future publication. New clones do not receive those
bytes.

Users and agents choose between those concepts; the lower-level engines remain
implementation details.

Start with the [workspace model](docs/management-model.md) to understand how a
user can treat a coding agent as a general-purpose collaborator, how a writable
chat becomes one task/branch/PR, and how scope, placement, publication, and the
shared checkout fit together. Continue with the
[user guide](docs/guide.md) for the lifecycle. The
[command reference](docs/commands.md) documents every public command, option,
side effect, and example.

## Core workflow

```sh
workspace-mgr setup
workspace-mgr init \
  --s3-url s3://example-bucket/workspace \
  --s3-endpoint-url https://s3.example.invalid
workspace-mgr doctor
workspace-mgr instructions
workspace-mgr task create example-task \
  --title "Example task" \
  --purpose "Produce one reviewable deliverable"
workspace-mgr plan
workspace-mgr publish -m "Create the task review"
workspace-mgr task create shared-policy --kind infrastructure \
  --title "Shared policy" --purpose "Update repository policy" \
  --scope AGENTS.md --scope-note "The user requested this shared change"
```

Infrastructure tasks work in the same shared checkout, which stays on the
configured main branch. Creation returns a private manifest path; pass it as
`--manifest <path>` to task-scoped commands such as `plan` and `publish`.
Both task kinds publish to their own unmounted branch through a private index.

Private product state lives in the primary checkout's `.workspace-mgr/local/`,
which the generated root `.gitignore` ignores. All linked worktrees use that
same directory for locks, private indexes, infrastructure manifests, and
pending transaction state. Repository-owned modules such as
`.workspace-mgr/repository.gitignore` and
`.workspace-mgr/instructions/repository.md` remain trackable. Existing state
under `<git-common-dir>/workspace-mgr` migrates automatically; see the
[upgrade guide](docs/guide.md#upgrading-private-product-state).

Inside a task:

```sh
workspace-mgr task rename more-accurate-topic
workspace-mgr storage status
workspace-mgr storage set path/to/data --to s3 --reason "Retained dataset"
workspace-mgr storage set path/to/report.pdf --to git --reason "Review in Git"
workspace-mgr remove path/to/obsolete-data
workspace-mgr untrack path/to/local-data
workspace-mgr plan
workspace-mgr publish -m "Publish the deliverable"
```

A task directory is where the work happens, not only where finished results are
filed: the tools the agent writes, the materials they use, and the task's own
record of decisions, process, and hard-to-reproduce results all live inside it,
listed in its README directory map.

Active deliverable task directories stay at the repository's top level. After
the task is done and its pull request is confirmed merged, the user may request
that its directory be grouped under a time folder, such as `2026/`, `202607/`,
or `2026/07/`. Organizing completed tasks is an explicitly requested
infrastructure task, never an automatic action after merge or at turn end. If
the user requests organization without choosing a structure, use
`YYYY/MM/<task-dir>` based on each task directory's timestamp, unless the user
specifies another date basis. Preserve each task's basename, retained contents,
immutable task ID, and target branch.

Use `archive --dry-run` to inspect eligible tasks and the required source
and destination scopes, then apply `archive` in that infrastructure task.
The command moves local directories; normal publication copies and verifies
their complete retained S3 history, rewrites storage metadata, and records
durable exact-version mappings before obsolete source objects are purged.
Archive verifies current task identity against opaque Git directory trees,
commit ancestry, and hosting review records; it never reads historical task
configuration blobs or infers old paths or branches from their format.
`task upgrade --manifest <task-config> --dry-run` previews a one-time backfill
of verified review evidence into a schema 4 completion checkpoint. Apply and
publish that metadata through a scoped infrastructure review, merge and refresh
it, then archive. The checkpoint requires 0.7.0 and is revalidated against live
reviews and subsequent changes; it is not a permanent completed flag. A current
manifest without a checkpoint remains eligible for the same bootstrap checks
at its known path. Unverifiable ownership or path continuity is refused, and
manifestless legacy tasks use explicit `task adopt` before archive.
For a direct import, the reviewed adoption commit becomes the history boundary;
later changes still need review. Empty `.git` cache markers are preserved.
Before movement, archive reports literal old paths in scripts and README files
and refuses external Git administration, stale registrations and location-bound
Python environments; the command reference explains repair steps.
See the [task upgrade command](docs/commands.md#workspace-mgr-task-upgrade).
Preview or undo an
unpublished local attempt with `archive --cancel --manifest <owner> --dry-run`;
cancel preserves ignored and hydrated local content while removing this
attempt's S3 copies and registry records. Archive completion requires the old
S3 prefix to contain no data versions or delete markers; protected or unmapped
history remains explicitly pending. A Git control tag binds each canonical
receipt, allowing B2-compatible publication without permanently retaining
duplicate source history. See the [archive command](docs/commands.md#workspace-mgr-archive)
for review, conflict, and cancellation guarantees.
Every archive publication requires 0.7.0 independently of task schema; private
purge/copy journals use schema 2 so 0.6.0 refuses protected retry state.
Historical Git snapshots remain readable through `workspace-mgr storage
hydrate`, including after their original S3 versions have moved.

After merge, `workspace-mgr refresh` brings the configured shared branch into the
shared checkout and automatically cleans local and configured-remote branch
refs that still match a verified merged GitHub pull request. Protected branches
and branches with new commits are kept. A branch checked out in any legacy or
custom worktree is also kept.
`refresh --dry-run` reports the proposed cleanup. GitHub CLI access is optional:
when merge evidence is unavailable, refresh preserves the refs and reports a
warning. This cleanup leaves task directories, worktrees and payloads in place;
organizing their directories still requires the user's request.

What leaves the task directory is curated. Every file under a task is either
selected for publication or ignored by a rule this repository tracks, so the
by-products of a run are not published by accident. Rules for one task belong
in that task's own `.gitignore`; this repository's own rules belong in
`.workspace-mgr/repository.gitignore`, from which `init` generates the root
`.gitignore` together with the product's fixed rules. A path that only a
machine-local rule hides is refused.

Immediately after creating a deliverable task, the agent publishes its initial
scaffold and creates the one matching draft pull request. Before every later
turn ends, it automatically records the turn's decisions, process, tools, and
hard-to-reproduce results in the task's own files, publishes all safe retained
in-scope changes, updates the draft pull request, and verifies that the local
task, remote branch, and pull-request head agree. This synchronization does not
require a separate user request.

When a conversation's topic changes, `task rename <new-slug>` moves the complete
deliverable directory and updates task metadata while preserving the immutable
task ID, target branch, and existing pull request. The next ordinary `publish`
removes the previously published path and publishes the new one. `storage set`,
`storage reset`, `move`, `remove`, and `untrack` change local desired state only.
`storage hydrate` reads from S3. `plan` is read-only. `publish` is the only
command that publishes repository content, and it verifies S3 before publishing
a Git revision. It then permanently deletes every S3 version at object paths
removed by delete, move, rename, untrack, or S3-to-Git placement; current remote branches
and tags defer deletion until the last live reference disappears. If the user
instead decides to retain none of the task, the
agent closes its unmerged pull request and uses `task discard --dry-run` followed
by `task discard --confirm <task-id>` to remove its branch and local workspace.

Each task's cloud usage across Git history and retained S3 versions is limited
to 1 GiB (1073741824 bytes). `plan` reports the published and projected usage,
and `publish` refuses to grow a task past its limit; while a task is over its
limit, only publications that remove content, apart from at most 1 MiB
(1048576 bytes) of new workspace-mgr control-file content per publication,
where metadata that only drops entries is free, remain allowed. The agent then
stops the task and asks the user, who either approves a higher limit, recorded
in the task manifest with `task approve-cloud-usage` and published with the
task for review, or chooses the cleanup to publish.

## Placement policy

Git is the collaboration/control plane for clone-ready, directly reviewable
repository state. S3 is the artifact/data plane for exact objects that change
atomically or hydrate on demand. The agent records that semantic choice with
`storage set --to git|s3 --reason <reason>` when intent is clear; the CLI does
not infer intent from filename extensions.

Size is only the fallback for unclassified new files. Below 1 MiB, Git is the
strong default. From 1 through 10 MiB, Git remains the fallback but the plan
asks the agent to review the semantic choice. Above 10 MiB, S3 is the fallback.
An explicit S3 boundary below 1 MiB is allowed but receives an efficiency
warning. Existing published placement stays stable when size changes, and
`storage reset` returns a path to published history or the fallback.

Directories may be placed in S3 as one logical boundary whose aggregate payload
size is reported. Git and S3 placement both count toward the task's cloud-usage
limit; placement never changes who approves growth. `move` preserves a path's
placement, and `remove` explicitly deletes a file or boundary without confusing
an unhydrated S3 output for an intentional deletion. `storage hydrate`
materializes S3 content without publishing.

`untrack` keeps a materialized file or complete storage boundary locally and
adds an exact ignore rule. After publication, its payload is absent from the
task's Git tree and obsolete S3 versions are queued for permanent cleanup.
`refresh` preserves the local copy after the change is merged. Use
`storage set <path> --to git|s3 --reason <reason>` to track it again; `storage
reset` does not undo a local-only choice. Git commit history remains available.

## Agent instructions

`workspace-mgr init` installs a deliberately small `AGENTS.md` that tells the
agent to run `workspace-mgr instructions --repo .`. The generated document
begins with the same [workspace model](docs/management-model.md) read by users,
then renders the complete product-owned policy using the repository's Git and
S3 facts and appends an optional repository-specific content module. Every
initialized repository gets the same management strategy; policy evolves with
the CLI rather than through per-repository switches. Re-running `init` after a
CLI update deterministically replaces product-owned scaffold files with the
current versions; their ownership comes from the initialized repository and
reserved path, never from matching old file content.

The scaffold also contains a recovery path for a machine without the CLI. The
agent asks the user before installing the latest stable release from crates.io
with `cargo install --locked workspace-mgr`, runs `workspace-mgr setup`, and
then retries the instructions command. It never falls back to raw repository or
storage mutation commands.

## Installation

Install the latest stable release from crates.io, then verify Git and its
built-in storage engine:

```sh
cargo install --locked workspace-mgr
workspace-mgr setup
workspace-mgr --help
```

Building the crates.io package requires Rust 1.85 or newer. To install without
a Rust toolchain, download a prebuilt native archive for Linux x86-64/arm64 or
Apple Silicon macOS from the
[latest GitHub release](https://github.com/S-kblogN/workspace-mgr/releases/latest),
extract it, and run:

```sh
./install.sh
```

The native installer checks Git and copies the CLI to
`${HOME}/.local/bin` by default. Set `WORKSPACE_MGR_PREFIX` to choose another
executable prefix.

`setup` checks Git. Storage, exact-version S3 reads, archive, and cancellation
run inside the Rust executable; Python, DVC, and a separate storage runtime are
not required. Existing DVC-compatible pointers, cache objects, and archive
journals remain readable. See [docs/platform-support.md](docs/platform-support.md).

Every CLI invocation consults a local update cache. At most once every six
hours, it asks crates.io for newer non-yanked versions; a failed request is
silently retried after one hour. When an applicable version is available, the
CLI writes one agent-directed notice to stderr without changing command output
or exit status. It never updates itself. The agent reports the versions and asks
the user before updating, then runs `workspace-mgr setup`; managed repository
scaffolding is reconciled with `workspace-mgr init` in an infrastructure task.

A repository can also declare the oldest compatible release as
`minimum_cli_version` in `.workspace-mgr.toml`. `workspace-mgr` maintains that
declaration itself: a publication raises it when it introduces task state that
older releases cannot read, such as a task manifest that records a cloud-usage
approval, and nothing lowers it once it is merged. From 0.4.0 on, a release
older than the declaration refuses the repository with a message naming both
versions, and the agent asks the user before updating. Releases up to 0.3.0 do
not know the key and reject `.workspace-mgr.toml` with an unknown-field error
for `minimum_cli_version`; update the CLI instead of removing or editing the
key.

Configuration is documented in
[docs/configuration.md](docs/configuration.md), transaction guarantees in
[docs/architecture.md](docs/architecture.md), platform requirements in
[docs/platform-support.md](docs/platform-support.md), and releases in
[docs/releasing.md](docs/releasing.md).

## Development

```sh
cargo fmt --check
cargo deny check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo package --allow-dirty
```

Integration tests use fresh temporary repositories and local storage. GitHub
Actions also runs the full public lifecycle against a versioned local S3 service
and a network Git server. Neither test path reads developer cloud credentials.

## License

MIT
