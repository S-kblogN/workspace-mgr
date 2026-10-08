# User guide

`workspace-mgr` is the single repository-management interface for people and
coding agents. It creates the repository and task scaffolding, explains the
effective repository policy, decides where retained content lives, and
publishes a task without switching a shared checkout.

Begin with the [workspace model](management-model.md). It explains why this
repository exists as a durable workspace for general-purpose conversations with
coding agents, how one writable chat maps to one task/branch/PR, and how scope,
Git/S3 placement, publication, and concurrent chats fit together. The same
source document appears first in the default output of `workspace-mgr
instructions`, so users and agents reason from one model rather than parallel
summaries.

The sections below apply that model as an operational lifecycle. The public
storage vocabulary deliberately contains only **Git** and **S3**; users and
agents do not configure or invoke private execution engines directly.

## Repository lifecycle

### 1. Install the CLI and verify Git

Native archives include `install.sh`, which installs the executable and runs
`workspace-mgr setup`. After `cargo install`, run setup explicitly:

```sh
workspace-mgr setup
```

Setup checks that Git is available and reports the built-in Rust storage engine.
It creates no separate runtime or user data directory. `workspace-mgr setup
--dry-run` performs the same check without changes. The legacy `--runtime-dir`
flag remains accepted for older installers; setup does not inspect or modify
that directory.

Invocations other than `task list`, `task path`, `task show`, help output, and
argument errors also check the local update cache; `--version` still checks.
A successful registry check remains fresh for six hours; a failed check is
silent and is retried after one hour. The network request has a short timeout,
never changes the command's exit status, and never writes to stdout. If a newer
applicable release is known, each such invocation writes one line to stderr
asking the agent to notify the user. Stable installations consider only stable
releases; prerelease
installations follow the newest non-yanked release, including prereleases.

The CLI never updates itself. The agent reports the current and available
versions and asks for approval. After an approved update, run:

```sh
workspace-mgr setup
```

If the release changes managed repository scaffolding, create an infrastructure
task covering the affected product-owned paths and run `workspace-mgr manage`
in the shared checkout. Review and publish that generated diff using the private
manifest path returned by `task create`.

A repository can also require a minimum release. `.workspace-mgr.toml` may
declare `minimum_cli_version`, which `workspace-mgr` raises itself when a
publication introduces task state that older releases cannot read, and never
lowers once it is merged. From 0.4.0 on, a release older than the declaration
refuses every repository command, including `instructions`, with a message
that names both versions; `workspace-mgr doctor` still runs and reports the
comparison as its `cli-version` check. Releases up to 0.3.0 do not know the
key: they reject `.workspace-mgr.toml` with an unknown-field error for
`minimum_cli_version`, and their `doctor` reports only that configuration
error. In both cases the agent tells the user the installed version and the
required one and asks before updating, exactly as for an update notice; nobody
removes or edits the key to make an older release work.

### 2. Manage a repository

Run `manage` from the Git repository to adopt it or update its scaffolding and storage metadata:

```sh
workspace-mgr manage \
  --s3-url s3://example-bucket/workspace \
  --s3-endpoint-url https://s3.example.invalid
```

Management creates:

- `.workspace-mgr.toml`, the public Git and optional S3 facts;
- a thin `AGENTS.md` bootstrap;
- the root `.gitignore`, generated from the product's fixed rules for
  regenerated output and private product state, and from this repository's own
  rules in `.workspace-mgr/repository.gitignore`.

An existing repository-owned root `.gitignore` is preserved verbatim in
`.workspace-mgr/repository.gitignore` before the generated root file replaces
it. A conflicting existing module or an unowned `AGENTS.md` blocks adoption
before files change. After `.workspace-mgr.toml` establishes the repository as
managed, `AGENTS.md` and the generated root `.gitignore` are product-owned.
Every `manage` deterministically reconciles them with the installed CLI and
current Git/S3 facts. It also converts supported legacy DVC storage metadata
through the same recoverable transaction; see [native storage](storage.md).
Use `--dry-run` to inspect all planned changes first. In a managed repository,
an agent performs this reconciliation
inside an infrastructure task so the generated repository-wide diff is
reviewed like any other shared change.

### Upgrading private product state

Private product state now lives in the primary checkout's `.workspace-mgr/local/`.
The generated root `.gitignore` includes `/.workspace-mgr/local/`, leaving
`.workspace-mgr/repository.gitignore` and
`.workspace-mgr/instructions/repository.md` available as tracked
repository-owned modules. All linked worktrees use the same private directory
in the primary checkout, including the repository lock, so operations in
different worktrees still exclude one another.
The primary checkout is the repository's original checkout, usually the shared
main checkout, and remains the state location even when its branch changes.
If that checkout is unavailable or the repository is bare, the CLI refuses to
create separate state elsewhere.

If the repository was created with `git init --separate-git-dir`, Git does not
record a reverse path from the common directory to its primary checkout. Set
Git's `core.worktree` to the absolute path of the original checkout before
running `workspace-mgr`:

```sh
git config core.worktree /absolute/path/to/primary-checkout
```

This is Git's checkout-location setting, not a workspace-mgr state-directory
option. Without it, the CLI refuses the repository so different linked
worktrees cannot create independent state and locks.

The CLI automatically migrates existing state from
`<git-common-dir>/workspace-mgr` when it accesses private product state.
Infrastructure manifests, private indexes, pending S3 cleanup, archive retry
journals, discard confirmation and backup state, and caches retain their
contents. An old infrastructure manifest path passed to `--manifest` continues
to select the migrated manifest. This directory is not all disposable cache:
preserve private manifests and pending transaction records until their task or
operation completes.

Upgrade the CLI used by every linked worktree and stop older processes before
running the new version. Migration refuses a lock held by an older process or
a conflicting destination path; resolve the reported conflict before retrying.
Older binaries keep using the previous directory and cannot participate in the
new repository lock. Running old and new CLIs in parallel is unsupported.
Run `workspace-mgr manage` in an infrastructure task to
regenerate the root `.gitignore`, then publish that scaffold change so other
clones receive the new rule.

### Preserving repository ignore rules

The generated first line identifies a product-owned root `.gitignore`.
When adopting a repository with its own root ignore file, `manage` automatically
preserves its rules in the repository module and regenerates the root file:

```sh
workspace-mgr manage --dry-run
workspace-mgr manage
```

The generated file contains the product's fixed rules followed by that module,
so repository rules and negations retain their order and take precedence.
Resolve a conflicting existing module explicitly before retrying. Do this
inside an infrastructure task like any other repository-wide
change, and publish the result: an ignore rule reaches other clones only once
the regenerated root file is on the shared branch.

Repository-specific additions belong in
`.workspace-mgr/instructions/repository.md`, which `manage` preserves, and this
repository's own ignore rules belong in `.workspace-mgr/repository.gitignore`,
which `manage` imports verbatim into the generated root `.gitignore` under its
own comment header. Both modules are repository-owned and limited to 64 KiB of
UTF-8; the product validates nothing else about the ignore patterns, except
that the ignore module may not contain the `# workspace-mgr local begin` and
`# workspace-mgr local end` markers, which belong to `untrack`. Hand edits
to the generated root file, below its generated header, are drift, which
`doctor` reports and `manage` repairs; a well-formed `# workspace-mgr local
begin` block in the root file survives regeneration unchanged, while a marker
left without its partner is dropped and named in the reported action. Shared
files such as `.gitattributes` retain repository-owned content; migration removes
only recognized obsolete storage rules. `--dry-run` reports every planned action
without writing files.

Every initialized repository uses the same shared-checkout, task, storage, and
review strategy. There are no policy profiles. The configuration records only
the external facts that genuinely differ between repositories.

### 3. Load the policy for an agent session

The generated `AGENTS.md` asks every agent to run:

```sh
workspace-mgr instructions --repo .
```

If the command is unavailable, the same scaffold tells the agent to stop
repository work, inform the user, and ask permission to install the latest
stable release from crates.io. After approval it runs `cargo install --locked
workspace-mgr`, then `workspace-mgr setup`, and retries `workspace-mgr
instructions --repo .`. An unapproved or failed installation remains a
blocker; the agent does not substitute lower-level repository or storage
commands.

Default `instructions` and explicit `instructions all` provide the mental
model, operation directory, genuinely session-wide constraints and current
repository facts. They do not print every operation's detailed policy. Read
`workspace-mgr <command> --help` before that operation for its prerequisites,
applicable repository-management rules and next steps. Execution reports supply
facts and reminders that become relevant only after the operation runs.

`instructions model` returns only the short conceptual document. Existing
`core`, `task`, `publish`, `artifacts`, `storage`, `shared-checkout` and
`infrastructure` topics remain detailed on-demand compatibility views; this
changes where policy appears, not the policy itself. The user's
`.workspace-mgr/instructions/repository.md` is indexed by default; read it before
task work, either directly or with `instructions repository`. Its bytes still
contribute to the effective policy hash even when its body is not printed.

### 4. Create one task

```sh
workspace-mgr task create model-report \
  --title "Model report" \
  --purpose "Produce the reviewable model report"
```

This fetches the configured base branch, then creates a timestamped top-level
directory, a concise README, a task manifest, and a local target-branch ref. It
does not itself create a remote branch or call a hosting provider. Immediately
after the command succeeds, the agent runs `plan`, publishes the initial
scaffold, and creates and verifies the matching draft pull request before
substantial task work. That checkpoint is automatic and does not require
another user prompt.

The task directory is where the work happens, not only where finished results
are filed. Scripts written to do the work, the materials they read, and the
task's own notes are created inside it. Do not build a scratch workspace in a
temporary directory outside the repository: the agent loses it at the end of the
session, and so does anyone reading the task later.

The product does not prescribe how the task organizes its record, only that it
is written in Markdown. Choose the Markdown files that hold the decisions the
conversation reached, the process it followed, the tools it wrote, and the
results that are hard to reproduce; keep them inside the task directory and
list them in the README's directory map. Publication refuses a deliverable task
that adds or changes content inside its own directory while documenting
nothing, so the record is written with the work rather than after it.

Run task commands from inside the task directory so the manifest is discovered
automatically. Use `--manifest <path>` when working elsewhere. The task
directory is the default write boundary. The agent may read anywhere in the
repository for context, but reading does not authorize mutation. Before the
agent creates, edits, moves, or deletes anything outside its task directory,
the user's request must explicitly authorize the exact path and action. This
includes shared root paths and another chat's task directory. If the request
does not, the agent asks and waits before writing.

A manifest may declare durable additional scopes, while `--include <path>
--scope-note <reason>` records a user-authorized additional scope for one
invocation. Neither mechanism creates authorization by itself.

For shared repository mechanisms, use `--kind infrastructure` with explicit
`--scope` paths and a `--scope-note`. Work in the same shared checkout on the
configured main branch. Creation reports its root as `path` and an absolute
private `manifest` path; pass that manifest as `--manifest <path>` to subsequent
task-scoped commands. There is no manifest autodiscovery for infrastructure
tasks and no timestamped task directory or separate worktree. Because
there is no deliverable task directory, the exact user-authorized manifest
scopes are the infrastructure task's write boundary. Reading outside them is
allowed for context; writing outside them is not.
Creation requires the shared HEAD to match the fetched base revision; run
`refresh` first when the shared checkout is behind.

```sh
task_manifest=/absolute/path/reported/by/task-create
workspace-mgr plan --manifest "$task_manifest"
workspace-mgr publish --manifest "$task_manifest" -m "Publish shared policy"
```

If the conversation's topic changes, keep the same task and rename its current
slug:

```sh
workspace-mgr task rename updated-model-comparison --dry-run
workspace-mgr task rename updated-model-comparison
```

For a deliverable this moves the complete timestamped task directory and
rewrites its manifest. The immutable task ID and original target branch remain
stable so the same draft pull request continues to represent the chat. The
command writes no remote; the next ordinary `plan` shows both the published old
path and current path, and `publish` removes the old Git tree after preserving
Git/S3 placement history. The agent then updates the existing pull request's
title and description. An infrastructure rename uses `--manifest` to update its
private current slug while its manifest path remains stable.

### Find an existing task

Use task discovery instead of assuming that every task is directly below the
repository root. Renaming changes the current slug, and archiving can move the
same task into a year or month folder while its immutable ID remains stable.

```sh
workspace-mgr task list
workspace-mgr task list model --kind deliverable
workspace-mgr task list --placement nested --paths
workspace-mgr task show model-comparison
cd "$(workspace-mgr --format human task path model-comparison)"
```

Listing searches current local task metadata throughout the repository,
including ignored and untracked deliverables and private infrastructure tasks.
The query is a case-insensitive substring search over identity, name, title,
and path. `--paths` emits only repository-relative deliverable paths, one per
line; use `task path <selector> --relative` for one such path. `task path`
defaults to an absolute directory and requires an exact current slug,
basename, immutable ID, or path. If a slug matches multiple tasks, it reports
the candidates and requires a more precise selector.

`task show` provides current metadata and the absolute manifest path. An
infrastructure task has no deliverable directory; select its manifest from
this output when using task-scoped commands. A timestamped directory without
a manifest is a legacy candidate, and invalid current metadata is reported
with a diagnostic. Neither a candidate nor a nested location proves that a
task is complete. Local `archive_status` describes its current receipt,
without checking GitHub merge or S3 publication status. Discovery is offline
and read-only, including no update-check cache or private-state migration.
It stops at task roots and skips symbolic links. A current task manifest can
identify a task with its own Git controls; other nested Git checkouts are
excluded, even when their names look like timestamped legacy tasks.

### 5. Choose where retained content lives

First decide whether content should be retained at all. Ignore safely
reproducible caches and intermediate build output that are neither inputs,
deliverables, nor evidence. Tools the agent wrote and results that were
expensive or impossible to reproduce are never in that category, even when they
look like intermediate output. Retain them. If they are too large for ordinary
Git, place them with `workspace-mgr storage` or keep the bytes locally with
`workspace-mgr untrack`; do not move them outside the repository to avoid the
decision, and do not route bulk by-products to S3 to keep Git small.

External Git repositories cloned for reference, dependencies or inspection
stay local and ignored. Never publish the checkout or a copy of its repository
contents to Git or S3, including through a containing storage boundary. Cover
the whole clone directory with a shared repository or task-local `.gitignore`
rule, such as `/references/upstream-tool/`; ignoring only `.git` is insufficient.
Do not delete `.git`, flatten or copy the clone, or package it into an archive
to bypass this rule. Record the source URL, exact commit and commands to clone
and check out that commit in the task's notes. Keep task-authored patches, tools
and results separately outside the ignored clone.

Every nested Git repository must be ignored as an entire directory and have
no outer-tracked files or gitlinks. `plan` and `publish` check the whole-directory
ignore boundary before storage placement. A new task-local ignore file can be
published with the task; a global ignore or `.git/info/exclude` alone does not
satisfy the rule.

Every remaining file under the task is in one of two states: selected, meaning
published in Git or placement-recorded for S3 or local-only retention, or
ignored by a rule this repository tracks. Publication stages the whole declared
scope, so there is no third state in which a file stays in the task directory
and is remembered as not-to-be-committed. Write the ignore rule in the layer
that matches its audience:

| Rule | Where it belongs | Who sees it |
| --- | --- | --- |
| Specific to one task | `<task>/.gitignore`, the narrowest rule that covers it | Every clone, and the task's own review |
| This repository's own, for every task | `.workspace-mgr/repository.gitignore`, imported into the generated root `.gitignore` by `manage` | Every clone, once that change is published |
| Only this machine | Nowhere the product accepts: a rule in your global excludes or `.git/info/exclude` makes `plan` and `publish` refuse | Only you |

The first layer is the cheap one, and it is the one a deliverable task can
reach: `<task>/.gitignore` is inside the task's own write boundary and is
staged by the same publication. The second is a pair of shared root paths, so
changing it needs the user's explicit authorization like any path outside the
task directory, belongs in an infrastructure task, and takes effect elsewhere
only once the regenerated root file is published on the shared branch.

A path that only a machine-local rule hides is refused rather than published,
because the rule keeps the file out of every other clone and out of review.
Move the rule into one of the first two layers, or let the file be published
when the task retains it.

For retained content, choose the history model before considering size. Git is
the collaboration/control plane for clone-ready content whose value comes from
review, diff, merge, or joint evolution with source. S3 is the artifact/data
plane for exact objects that change atomically or hydrate on demand. The agent
has the task context needed for this choice; `workspace-mgr` deliberately does
not infer semantics from extensions.

Use an explicit choice when intent matters more than size:

```sh
workspace-mgr storage set task/path/report.pdf \
  --to git --reason "Review the rendered report in Git"

workspace-mgr storage set task/path/dataset \
  --to s3 --reason "Retain the dataset as one versioned directory"
```

An explicit user choice wins at any size. A directory is one logical placement
boundary and all descendants inherit it. Its reported payload size is the sum
of its materialized regular files; this does not imply that the backend packs
the directory into one remote object. Nested placement boundaries are rejected:
set or reset the existing boundary instead. `storage set`, `storage reset`, and
`move` change only local desired state; none writes to a remote.

When no semantic choice has been recorded, the fixed size fallback is:

| New boundary size | Fallback |
| --- | --- |
| Below 1 MiB | Git, with no routine plan warning |
| 1 through 10 MiB | Git, with `semantic-placement-review` so the agent checks intent |
| Above 10 MiB | S3 |

Automatic evaluation treats each unclassified new file as its own candidate.
Directory aggregation is meaningful only after the agent or user explicitly
selects that directory as a semantic boundary.

An S3 boundary path may not contain a backslash, because the storage engine
reads it as a directory separator. Automatic placement and `storage set --to s3`
refuse such a path before writing any metadata: rename it, or place it in Git
explicitly with `storage set --to git`. Everywhere else a backslash is an
ordinary file-name character that workspace-mgr never rewrites. Storage
metadata that an earlier release left at such a path is refused the same way,
with a `workspace-mgr move` hint, by every command that would have to address
it. `refresh` cannot refuse what a shared branch already carries, so it skips
exactly that boundary instead: it advances the branch, hydrates every other
incoming boundary, leaves that one payload unhydrated, and reports it with the
rename that recovers it. Until the rename, a scope-wide `storage hydrate` over a
scope containing that boundary refuses, so name the other boundaries there to
hydrate them.

A standalone S3 boundary below 1 MiB is usually less efficient than Git because
its metadata and remote operations may outweigh the payload. Explicit S3 still
succeeds but reports `small-s3-boundary`; prefer Git or a larger meaningful
directory boundary when possible. Previously published placement is sticky, so
changing size never silently moves content.

`storage reset <path>` removes an explicit choice and returns the path to
automatic policy. If that directory has not been published, removing its atomic
choice allows its files to be evaluated independently during planning and
publication. A published S3 directory remains one sticky S3 boundary until an
explicit `storage set --to git` moves it.

To retain existing bytes only on this machine, use `untrack`:

```sh
workspace-mgr untrack task/path/data.bin --dry-run
workspace-mgr untrack task/path/data.bin
workspace-mgr plan
workspace-mgr publish -m "Keep data.bin local only"
```

This records `target = "local"` and an exact managed `.gitignore` rule while
preserving the payload. Publication removes the payload from Git and queues
obsolete S3 versions for permanent cleanup. References from other remote
branches or tags defer cleanup until they disappear. Git history is retained.
After merge, `refresh` keeps the local copy, and future publication does not
upload it again. A new clone receives the placement record and ignore rule,
but no payload. Hydrate absent S3 content before the initial untrack operation.

Operate on a standalone file or complete directory boundary; untracking a
child of an existing storage boundary is refused. To resume tracking, use
`storage set <path> --to git|s3 --reason <reason>`. This removes only the tool's
ignore rule; conflicting user rules are reported and preserved. Local-only
paths refuse `storage reset` so a reset cannot accidentally upload private
local content.

For an ordinary directory without a placement boundary, first select it with
`storage set <directory> --to git --reason <reason>`, then untrack that boundary.

### 6. Inspect and materialize placement

```sh
workspace-mgr storage status
workspace-mgr storage status task/path/dataset/example.csv
workspace-mgr storage hydrate task/path/dataset
```

With no paths, `storage status` lists ordinary Git content plus explicit,
local-only, or published S3 boundaries in the resolved task scopes. A directory boundary is
shown once rather than once per descendant. For a selected path,
`basis` explains the result:

| `basis` | Meaning |
| --- | --- |
| `explicit` | The path itself has an explicit choice |
| `explicit-ancestor` | An explicit directory boundary contains the path |
| `published-history` | Existing published history fixes this path in Git or S3 |
| `published-ancestor` | A published S3 directory boundary contains the path |
| `automatic-size-fallback` | A new unclassified file uses the fixed size fallback |

Each row also reports its effective `boundary`, available `payload_bytes` and
`payload_files`, an explicit semantic `reason` when present, and structured
`warnings`. `plan` includes automatic decisions in the 1–10 MiB review band or
above 10 MiB, plus existing boundaries with actionable warnings.

`storage hydrate` reads exact S3 content into the working tree. With no paths it
hydrates every S3 boundary in scope. It refuses to overwrite locally modified
content.

### 7. Plan, then publish

```sh
workspace-mgr plan
workspace-mgr publish -m "Publish the model report"
```

`plan` resolves the task branch and scopes, fetches relevant Git refs, evaluates
placement, validates a private preview tree with would-be S3 payloads excluded,
and reports Git changes plus pending placement. Exact generated S3 metadata is
established during `publish`, because plan does not rewrite it. Plan may update
ignored local transaction state, but it creates no commit, uploads no S3
content, and publishes no branch.

`publish` repeats the validation. It first reconciles and uploads every in-scope
S3 boundary, verifies the exact remote content, builds a Git commit from only
the declared scopes, pushes an explicit target-branch ref, and verifies the
remote object ID. It does not switch the checkout or stage files in the shared
Git index.

Creating and maintaining the task's one draft pull request remains a
repository-hosting action.
`publish` writes the branch transaction; it does not create or update a pull
request. `archive` reads the task's corresponding PR state through `gh`;
`refresh` reads merge evidence for branch cleanup.
Immediately after publishing a new deliverable task's scaffold, the agent finds
the request by head branch, reuses
it or creates exactly one draft pull request, and never creates a duplicate. An
infrastructure task does this after its first safe scoped publication. The agent
keeps the title and living description aligned with the goal, scope,
deliverables, validation, and known limitations, then verifies the base, head,
draft/open state, and head revision after every material publication.

Before every writable-task turn ends, the agent automatically records the
turn's decisions, process, tools, and hard-to-reproduce results in the task's
own files when the turn produced any, runs a task-targeted plan, publishes all
safe retained in-scope changes even if the work remains in progress, updates
and verifies the draft pull request, and finishes with a no-change plan. If
there is nothing to publish, it still verifies that the local task revision,
remote branch, and pull-request head agree. A
publication or provider blocker is reported with the exact unsynchronized state;
the user never has to ask for routine turn-end synchronization. A task waiting
for the user's cloud-usage decision is such a blocker: the reconciliation stops
at the plan, without recording the turn, resolving unignored by-products, or
publishing, and the agent reports the usage, its question, and the changes that
remain unpublished, as described in the next step.
Hosting failures are reported immediately. The agent must not merge, enable
auto-merge, approve, close, or mark the pull request ready unless the user
explicitly requests that exact transition. An explicit request to discard one
unmerged task authorizes closing only that task's pull request before cleanup.

A plan or publication of a deliverable task can report structured `warnings`
alongside its changed paths:

| Code | Meaning | When to ignore it |
| --- | --- | --- |
| `task-record-unchanged` | The publication changes content inside the task directory, but none of the task's documentation changed with it | The turn produced no decision, tool, process step, or hard-to-reproduce result worth recording |
| `bulk-publication` | The publication adds more than 200 new files, or more than 256 MiB (268435456 bytes) of new content, inside the task directory | The content is genuinely retained inputs, tools, evidence, or deliverables |

The `warnings` list appears only when a plan or publication has something to
report, so an ordinary clean report has no `warnings` key at all.

`refresh` reports warnings in the same shape:

| Code | Meaning | When to ignore it |
| --- | --- | --- |
| `unaddressable-storage-metadata` | The incoming revision carries an S3 boundary whose path contains a backslash, so the storage engine cannot hydrate or verify it; the branch advances and every other boundary hydrates, but that payload does not | Never: the named boundary stays unhydrated in every checkout until it is renamed, in an infrastructure task scoped to the directory that holds it |
| `branch-cleanup-unavailable` | GitHub merge evidence is unavailable, so branch refs are preserved after synchronization | Fix GitHub CLI access if automatic branch cleanup is wanted |
| `branch-cleanup-failed` | Verification or deletion failed for a branch; the synchronized shared branch remains current | Review the named branch and failure before retrying |

`refresh --dry-run` reports unaddressable storage metadata before anything
changes, and `storage.unaddressable` names the same boundaries. Its
`branch_cleanup` report separately names
cleanup warnings, errors, and retained branches, as described in step 10.

Two conditions are refusals rather than warnings, and both fail at `plan`,
before it changes placement or uploads anything. A deliverable publication that
would add or change content inside its own task directory while that directory
documents nothing is refused until the task records something of its own. A
publication that only retires content is not held to it, and one that removes
the task's last record while publishing content is refused by name. Removing
files from a directory boundary in S3 retires content: its rewritten metadata
names nothing the published metadata did not, and adds no text beside those
entries, such as a description or a comment. Untracking published content
retires it as well, because the placement record `untrack` writes then takes the
payload out of Git and S3. A result kept local before it was ever published
retires nothing, so its placement record, the only durable trace of it, counts
as content once the task is within its limit; while the task waits for the
user's cloud-usage decision it does not, because a record added to that cleanup
would be growth the limit refuses. Content placed in S3 is judged by its
pointer, so routing a result out of Git does not exempt it, and a change inside
a boundary that the storage engine has not committed yet is read from the
engine, so `plan` refuses it rather than `publish` after the upload; `publish`
judges the committed metadata once more before the upload, for a change that
lands after the plan. A staged symbolic link whose target is outside the
repository is refused, because the link points at content no other checkout has;
copy what the task must keep into the declared scope instead. The link check
reads the staged tree only, so a link inside a boundary already placed in S3 or
kept local with `untrack` is not classified: that content never reaches the
index.

A third refusal covers the ignore layer. A path inside the task's scopes that
only a machine-local rule hides — the user's global excludes, `.git/info/exclude`,
or an ignore file whose matching bytes this publication does not carry — is
refused with the path, the rule, and the file the rule came from. Carrying is
decided on content rather than on the file being tracked, so a rule appended to
the tracked root `.gitignore` and never published is machine-local too. Git
resolves the deepest matching ignore file first, so a carried repository or task
rule that also matches is the reported source, and the product's own fixed
rules are always carried, so an ordinary `.DS_Store` never triggers it even
before the generated root file has been published. A directory whose entire
content is ignored, which Git reports as one collapsed entry, is expanded to the
files inside it so that a file-level rule such as `*.log` is resolved rather than
missed. The refusal names at most the first five paths and counts the rest.

All three refusals are decided before the cloud-usage measurement of the next
step. Resolving one can change what the publication holds, so the agent
resolves it first, and a plan measures cloud usage, and the user is asked, only
for a publication that passes them.

A plan or publication also reports `ignored_paths` beside `ignored_entries`
when any path inside the resolved scopes is ignored. The count is exact and the
list carries up to its first fifty entries, so a pattern rule such as `*.log`
cannot fill the report. Read the list as a question: each entry should be
reproducible output, not a tool or result that should have been retained.

Repository-wide policy, root entrypoints, CI, and shared storage mechanisms use
`task create --kind infrastructure`. The command returns a private manifest
instead of creating a timestamped task directory. Work stays in the shared
checkout on the configured main branch and remains limited to the declared
scopes. `plan --manifest <path>` and `publish --manifest <path>` build the task's
publication with a private index; they do not switch the checkout or stage
another chat's files in its shared index.

### 8. Ask before a task exceeds its cloud-usage limit

Each task has a cloud-usage limit of 1 GiB (1073741824 bytes) unless the user
approves a higher limit for that task. Cloud usage is what the task keeps on the
remotes: the Git history its branch adds beyond the base branch, including Git
LFS objects, and every retained S3 object version of its paths, plus the
uploads its next publication would add. `plan` reports the published and
projected totals, the limit, and the largest contributors under `cloud_usage`.

When the projected total exceeds the limit, `plan` reports
`cloud_usage.status: approval_required` and `publish` refuses before it places,
commits, or uploads anything. The task is then waiting for the user's decision.
The agent stops all task work, including the routine turn-end publication and
its recording and curation steps, and asks in the chat. It reports the
published and projected Git, S3, and total bytes, the limit, and the largest
contributors, and proposes one specific new limit, normally the reported
`suggested_limit_bytes`, alongside the cleanup alternatives. Task-scoped
commands print a one-line reminder on stderr until a recorded approval covers
the pending projection or a later `plan` or `publish` measures the task within
its limit. The reminder repeats the last measurement, so after the user
answers, the agent carries out exactly that answer, then runs `plan` and acts
on its result.

If the user approves, the agent records exactly that answer, re-measures, and
publishes:

```sh
workspace-mgr task approve-cloud-usage --limit 1.5GiB \
  --note "The user approved 1.5 GiB for the training checkpoints"
workspace-mgr plan
workspace-mgr publish -m "Publish the training checkpoints"
```

Recording an approval documents the user's decision; it never creates one. The
command writes the approved limit and the user's note into the task manifest,
which requires at least schema 3, and the next publication carries that change, so
reviewers see it in the pull request. Each publication commit also names the
approval in a `Cloud-Usage-Approval` trailer; for an infrastructure task, whose
manifest is private, the trailer is the only published record. A user may also
approve a limit before large content is produced, and approving a limit equal
to the threshold removes the approval again.

Older workspace-mgr releases cannot read a schema 3 manifest, so the first
publication that carries one in a deliverable task also raises
`minimum_cli_version` in the published `.workspace-mgr.toml` when that file
does not already require a release that reads it. `plan` then lists that file in
`changed_paths` and reports `repository_requirement`, and the commit gains a
`Workspace-Requirement` trailer. This workspace-mgr-maintained change needs no
additional scope, and the shared checkout's copy changes only when the merged
task is refreshed. The agent notes the raised requirement in the pull-request
description, because after the merge every clone needs a release that meets it.
A build older than that release refuses to publish the approval at all, so the
agent reports both versions and asks the user how to continue. If the user
later resets the approval and no manifest still needs the raise, the next
publication withdraws it and reports `change: withdraw`, and the agent drops
the note from the pull-request description; a withdrawal never goes below
what the base branch already declares, so the branch keeps a raise that other
merged task state needs. A branch raised before the base branch was raised
further follows the base
branch's declaration on its next publication, so both merge cleanly.

If the user declines, the agent performs only the cleanup the user chooses:
`remove` or `untrack` of named content, `storage hydrate` only when that cleanup
needs absent S3 content, or discarding the task. It then runs `plan` and
publishes the reduction; a publication that only removes content, apart from at
most 1 MiB (1048576 bytes) of new workspace-mgr control-file content per
publication, where metadata that only drops entries is free, remains allowed
while the task is over its limit. The agent publishes that reduction on its own:
while the task is over its limit, the documentation refusal does not apply to a
publication that only removes or untracks content, and a task record added to it
would be growth the limit refuses. Its `task-record-unchanged` warning says so,
and the agent records the decision in the task's files in the first publication
the limit allows, and in the pull-request description until then. The reduction
takes effect when that publication permanently deletes the retired S3 versions.
Published Git history cannot shrink: when it alone exceeds the limit, only an
approval or discarding the task resolves the decision, and hosting providers may
still retain pull-request refs.

The threshold is fixed product policy with no repository setting. Moving
content into another task, a shared path, or another storage service is not a
way around it.

### 9. Discard an unmerged task instead of saving it

If the user decides that a task should not be retained, first inspect the exact
destructive scope:

```sh
workspace-mgr task discard --dry-run
```

The dry run fetches and records the local task ref, remote task ref, local
shared ref, and remote shared ref in private confirmation state. It reports
working changes, the deliverable directory to remove when present, shared scopes
to restore from the local shared branch, the required pull-request
transition, and any current versioned S3 references that will remain stored.
It does not delete content or refs.

After the user explicitly confirms abandonment, the agent verifies that the
task is unmerged, closes its matching pull request when one exists, and verifies
that provider transition. Run confirmation from the shared checkout; for a
deliverable task, this keeps the invoking shell outside the deleted directory:

```sh
workspace-mgr task discard \
  --manifest /absolute/path/to/.workspace-mgr-task.toml \
  --confirm <exact-task-id>
```

For an infrastructure task, pass its private manifest path with `--manifest`
to both dry-run and confirmation. Confirmation refuses a missing or stale dry
run, a changed local or remote revision, a mismatched task ID, a merged task, or
a branch that does not belong to the task. It deletes the remote branch with an
exact lease, deletes the local branch, removes the deliverable directory when
present, restores declared shared paths to the local shared branch, and clears
private task state. Local deliverable content is quarantined
until the remote deletion succeeds so a failure can restore it.
Infrastructure discard restores its declared scopes and removes its private
manifest and state; it never removes the shared repository directory.

Discard queues the task's managed S3 object paths before deleting its branch,
then permanently deletes every version at paths no current remote branch or tag
still references. Its report distinguishes deleted paths from protected pending
paths. A later publish, refresh, or discard retries protected paths after their
last current reference disappears.

### 10. Refresh after merge

In a shared checkout, use:

```sh
workspace-mgr refresh
```

`refresh` fetches the configured shared branch, permits only a fast-forward,
prefetches incoming S3 content, updates the local branch and index without
overwriting unrelated working-tree overlays, then materializes safe ordinary
Git changes and verifies incoming S3 content. If the update fails after the
local ref changes, it attempts to restore the previous ref, index, ordinary Git
files, metadata, and outputs.

Before it changes anything, refresh checks the incoming `minimum_cli_version`.
If the merged work requires a newer release than the installed one, refresh
refuses and leaves the checkout untouched; after the user approves and
completes the update, run it again.

After incoming content is materialized and verified, refresh automatically
cleans local and configured-remote branches that still match a verified merged
GitHub pull request in the same repository and against the configured base.
It also checks cleanup when the shared branch was already current.
`refresh --dry-run` reports planned deletions without deleting refs.

The pull request's merge commit must be reachable from the fetched base, and
its recorded head must match every remaining local or remote ref of that
branch. This accepts squash merges while preserving resumed branches and new
local commits. Open, active, ambiguous, fork, or other-base pull requests do
not authorize deletion. The configured base, current and default branches and
protected remote branches are kept.

A branch checked out in any legacy or custom worktree is kept. Refresh never
detaches a worktree or removes its directory or files.

The `branch_cleanup` report lists `planned`, `deleted`, and `skipped` branches,
plus `errors` and `warnings`.
Its `status` is `dry_run`, `complete`, `unavailable`, or `not_applicable`.
Cleanup needs a GitHub remote and installed, authenticated `gh`. A non-GitHub
remote returns `not_applicable` without a warning; unavailable GitHub access
keeps all refs and reports the blocker.

Cleanup warnings or deletion errors do not roll back successful shared-branch
synchronization. Refresh deletes remote refs with an exact lease and local refs
only if their heads are unchanged.

When the shared branch was already current but refs were deleted, refresh's
overall `status` is `branches_cleaned`.

Branch cleanup retains task directories and their stored payloads. The normal
pending S3 purge retry may proceed when a removed branch was the last live
reference protecting an already queued path. Directory organization remains a
separate user-requested infrastructure task.

### Upgrade current task configuration

`task upgrade` upgrades supported current task metadata locally and idempotently.
It preserves task identity, scopes, cloud-usage approval and compatible saved
review metadata. It fetches the configured shared branch to validate current task control
identity and compatibility, and guards staged manifest edits. It does not query
PRs, compare ordinary historical directory trees or require task payloads to be
clean. Preview first, then publish
the authorized current manifest change through its scoped task.

```sh
task_config=20260918-120000-example/.workspace-mgr-task.toml
workspace-mgr task upgrade --manifest "$task_config" --dry-run
workspace-mgr task upgrade --manifest "$task_config"
```

Older schema 4 completion checkpoints remain readable. Their current review
branch associations can help find live PRs after a branch change; the tool
neither produces new historical content proofs nor replays existing trees,
ancestry or commit-to-PR coverage. Current manifest validation remains strict.
Upgrade is not an archive prerequisite.

Active deliverable task directories remain at the repository's top level.
After a task's corresponding pull request is closed, the user may
explicitly request that old task directories be organized under time folders.
Handle that request through a repository-infrastructure task with the affected
paths in scope; do not organize them automatically after merge or as part of
turn-end synchronization.

The user may choose a structure such as `YYYY/<task-dir>`,
`YYYYMM/<task-dir>`, or `YYYY/MM/<task-dir>`. If the request does not specify a
structure, use `YYYY/MM/<task-dir>` based on each task directory's timestamp,
unless the user specifies another date basis. Keep active tasks at the top
level and preserve each completed task's basename, retained contents, immutable
task ID, and target branch. Inspect the request first:

```sh
workspace-mgr archive --dry-run
```

With no paths, the command scans top-level deliverable tasks and skips active
or unverified tasks. Naming an active task explicitly refuses the operation.
The preview reports closed-PR evidence, proposed destinations, complete S3
history, and the source and destination paths to declare in an infrastructure
task. It changes no repository content or remote. In the shared checkout, run
`archive --manifest <path>`, then `plan --manifest <path>` and
`publish --manifest <path>` for that infrastructure task.
`--layout '{year}'` and `--layout '{year}{month}'` select the other example
structures; the default `{year}/{month}` uses each task's creation timestamp.

Archive verifies the current task configuration and its associated PRs in the
configured repository, regardless of their target base branch. Merged PRs and
PRs closed without merging qualify; an associated open PR, including a saved
association from a previous task branch, blocks archive. One matching closed PR is required;
when none is found, a pre-0.7 adoption record can provide a current branch
hint. Malformed or unrelated adoption records add no extra gate. There is no
commit-by-commit review, historical configuration parsing, full-history
requirement, or branch-tip comparison for archive eligibility.

Archive checks managed-storage integrity and move conflicts. Ordinary
tracked, staged, untracked, ignored, and local-only files are retained;
unpublished ordinary content does not prevent movement. Scripts, README
commands, cross-task references, logs, symlinks, and Python environments move
unchanged. Their paths may need later maintenance if you want to run them at
the new location. Archive neither inspects those runtime dependencies nor rewrites
them.

Nested Git repositories must be covered by the outer repository's shared
`.gitignore` rules at both the source and destination and contain no
outer-tracked files or gitlinks. A local exclude or global ignore does not
satisfy this rule. A new task-local `.gitignore` may be published with the task;
the ignore file need not already be tracked. Ignored nested repositories and
Git controls inside the task move unchanged; external Git administration and
registrations remain untouched. Zero-byte `.git` cache markers remain ordinary
content. The
[command reference](commands.md#workspace-mgr-archive) describes this boundary.

For a manifestless directory, use `task adopt` to establish current task
metadata before archiving. Adoption has current PR/ref control checks but no
ordinary content-tree proof. Its separate adoption PR need not merge before
archive. Archive checks the resulting current manifest and associated closed PR,
without tracing earlier imports or the adoption record's introducing commit.

Archive moves the local directory and manifest together. Publication copies
the complete retained S3 history, including old versions, delete markers, and
retired paths, verifies it, and rewrites storage metadata automatically. Its
receipt records original and destination keys, version IDs, and timestamps;
the S3 registry makes those mappings available to older Git snapshots. Source
history is purged only after the copied receipt merges into the shared branch.
Historical tags remain and hydrate mapped versions through the registry; new
referenced generations without mappings stay protected. Storage retirement is complete only when a full scan finds
no old-prefix data versions or delete markers. `storage.purge` distinguishes
`complete`, `cleanup_pending` and `blocked_unmapped`; the latter two preserve
retry state and report a warning even when Git push or refresh succeeded.
An empty original inventory also keeps a `pending_prefixes` cleanup intent
until its published receipt's full source scan confirms an empty prefix.
Concurrent unmapped history is preserved and blocks completion. Preview and
cancel an unpublished attempt with `archive --cancel --manifest <path> --dry-run`;
apply restores local contents and metadata after verifying removal of its
remote copies, markers, registry records and unfinished uploads.
The copied history counts toward the infrastructure task's
cloud-usage limit. Local-only content remains local and travels with the task.

To hydrate a historical Git checkout after source cleanup, use
`workspace-mgr storage hydrate`; the underlying storage engine reads of old pointers do not consult
the archive registry. Publishing nested archived task state requires at least
0.5.0; the new archive protocol and schema 4 completion evidence require 0.7.0. Removing a
cloud-usage approval does not clear this path requirement; an older CLI needs
an update.

## Git versus S3

Choose placement by how the content should be reviewed and retrieved, not by an
absolute size prohibition.

| Choose Git when | Choose S3 when |
| --- | --- |
| The content's value comes from direct review, diff, merge, or joint evolution with source | The content is consumed as an exact object or changes atomically |
| Ordinary clone and checkout behavior is desirable | Exact object-version recovery matters |
| The file is intentionally reviewable despite being large | A directory should be retained as one logical boundary |

The fixed size bands are only a fallback for new, unclassified files. Explicit
`--to git` and `--to s3` choices always take precedence until reset.

For an `s3://` location, bucket object versioning is mandatory. Publication
records and verifies exact object version IDs before publishing the Git
revision. Credentials never belong in `.workspace-mgr.toml`; use ignored local
configuration or platform-standard identity mechanisms.

## Side effects by command

In addition to the command-specific effects below, invocations other than
`task list`, `task path`, `task show`, help output, and argument errors may read
the crates.io release record when their local update cache is stale. This
best-effort check is bounded, failure-silent, and never performs a remote write.

| Command | Local effect | Remote reads | Remote writes |
| --- | --- | --- | --- |
| `setup` | Verifies Git and the built-in engine | None | None |
| `manage` | Creates or repairs scaffolding | None | None |
| `instructions`, `config show` | Read-only checks/output | None | None |
| `doctor` | Read-only checks/output | S3 bucket settings when configured | None |
| `task list`, `task path`, `task show` | Read-only local discovery; no cache writes or state migration | None, including no update check | None |
| `task create` | Creates task files and a local branch ref | Fetches the Git base branch | None |
| `task rename` | Moves a deliverable directory and rewrites task metadata | Fetches Git refs to reject merged tasks, collisions, and a newer required release | None |
| `task upgrade` | Upgrades current task metadata while preserving compatible saved fields; dry-run changes nothing | Fetches the configured shared branch for current manifest identity and version control | None |
| `archive` | Moves closed-PR task directories and records migration receipts in an infrastructure task; dry-run changes no content | Reads current GitHub PR states, current task/storage metadata, and complete S3 version history | None; publication copies history and records exact-version mappings |
| `task status`, `storage status` | Read-only report | None | None |
| `task discard --dry-run` | Saves private confirmation state | Git refs | None |
| `task approve-cloud-usage` | Rewrites the task manifest with the user's approval | None | None |
| `task discard --confirm` | Removes an unmerged task workspace and local refs | Git and S3 reference verification | Deletes the exact remote task branch, then purges unreferenced S3 paths |
| `storage set`, `storage reset`, `move`, `remove`, `untrack` | Changes local content/placement metadata | None | None |
| `storage hydrate` | Materializes S3 content | S3 | None |
| `plan` | Creates ignored/private preview and cloud-usage state | Git refs and S3 bucket settings when configured | None |
| `publish` | Updates private state and a local target ref | Git and S3 verification | S3 first, then Git, then purge obsolete S3 paths |
| `refresh` | Fast-forwards, materializes incoming content, cleans verified merged local refs, and retries pending purge | Git, GitHub merge evidence through optional `gh`, and, when needed, S3 | Deletes verified merged branch refs and pending unreferenced S3 paths |

Most `--dry-run` forms suppress normal local mutation. `task discard --dry-run`
also saves its private revision-bound confirmation plan; it changes no task
content or remote. Dry-run never grants broader scope or bypasses safety checks.

## Transaction and failure boundary

The Git commit is the publication point for a combined Git-and-S3 transaction.
A Git revision is never intentionally published before all content it references
is present and verified in S3. If a later Git operation fails, an unreferenced
S3 object version may remain, but the remote Git branch must not point to
missing content. Retrying `publish` is safe. Once Git publication succeeds,
object paths removed by a delete, move, rename, untrack, or S3-to-Git transition are
permanently purged, including all older versions at those paths. Current remote
branches and tags defer deletion until the last live reference disappears.

A cloud-usage refusal at the start of `publish` leaves task content and remotes
unchanged; it only records the pending decision in private state. `publish`
checks usage again before its S3 upload and before its Git commit; a refusal at
either point leaves local placement and S3 metadata applied, and one at the
final check may leave an unreferenced uploaded S3 version. No Git revision is
published in either case.

Placement is previewed before usage is measured, so a task that both exceeds
its limit and holds an S3 boundary the storage engine cannot address is refused
for that boundary first, and no cloud-usage decision is recorded yet. Rename
the boundary, then run `plan` to see whether the limit still needs the user's
decision.

All repository paths accepted by task, storage, plan, and publish operations are
repository-relative and must remain inside the resolved scopes. A refusal is a
guard to investigate, not a signal to invoke internal version-control or storage
commands directly.

For exact syntax and every option, see the [command reference](commands.md).

Relocation execution reports contain a success-only notice with code
`manual-content-audit-after-relocation` after an actual directory move. It asks
the user to manually inspect links and path references; the tool does not
validate or repair them. The notice is absent from global instructions,
dry-runs, no-change operations, metadata-only rename and archive cancellation.
