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

### 1. Provision the CLI runtime

Native archives include `install.sh`, which installs the executable and runs
`workspace-mgr setup`. After `cargo install`, run setup explicitly:

```sh
workspace-mgr setup
```

Setup uses an isolated user data directory and verifies the exact private
storage runtime. `workspace-mgr setup --dry-run` reports the intended location
and actions without changing the host. A custom `--runtime-dir` must be absent
or already carry workspace-mgr's private ownership marker; setup never replaces
an arbitrary existing directory.

Every invocation also checks the local update cache. A successful registry
check remains fresh for six hours; a failed check is silent and is retried after
one hour. The network request has a short timeout, never changes the command's
exit status, and never writes to stdout. If a newer applicable release is
known, each invocation writes one line to stderr asking the agent to notify the
user. Stable installations consider only stable releases; prerelease
installations follow the newest non-yanked release, including prereleases.

The CLI never updates itself. The agent reports the current and available
versions and asks for approval. After an approved update, run:

```sh
workspace-mgr setup
```

If the release changes managed repository scaffolding, create an infrastructure
task covering the affected product-owned paths and run `workspace-mgr init`
from its isolated worktree. Review and publish that generated diff normally.

### 2. Initialize a repository

Run `init` once from the Git repository:

```sh
workspace-mgr init \
  --s3-url s3://example-bucket/workspace \
  --s3-endpoint-url https://s3.example.invalid
```

Initialization creates:

- `.workspace-mgr.toml`, the public Git and optional S3 facts;
- a thin `AGENTS.md` bootstrap;
- internal storage scaffolding when S3 is configured.

On first initialization, `AGENTS.md` and the private internal-storage scaffold
paths are reserved. If any already exists, `init` reports the complete
collision before writing anything; it does not inspect content to guess whether
the path is managed. After `.workspace-mgr.toml` establishes the repository as
initialized, `AGENTS.md`, generated internal storage configuration, and the
private engine's ignore files are product-owned. Every `init` deterministically
creates, replaces, or removes them according to the installed CLI and current
Git/S3 facts. This is also the scaffold-upgrade operation after installing a
newer CLI. In an initialized repository, an agent performs this reconciliation
inside an infrastructure task so the generated repository-wide diff is
reviewed like any other shared change.

Repository-specific additions belong in
`.workspace-mgr/instructions/repository.md`, which `init` preserves. Shared
files such as `.gitattributes` retain repository-owned content while `init`
ensures the product-required rules. `--dry-run` reports every planned action
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

`instructions` is intentionally different from `help`: help explains command
syntax, while instructions first establishes the shared workspace model and
then renders the repository's actual operating policy. The output combines the
canonical model document, the complete built-in policy, `.workspace-mgr.toml`
facts, and the optional repository-specific content module. Run
`workspace-mgr doctor` before work
if the installation or repository state may be inconsistent.

The default `all` document contains the model followed by the complete fixed
policy. `instructions model` returns only the conceptual model. Every
operational topic is always available; a repository cannot disable selected
rules and thereby give an agent an incomplete management contract.

### 4. Create one task

```sh
workspace-mgr task create model-report \
  --title "Model report" \
  --purpose "Produce the reviewable model report"
```

This fetches the configured base branch, then creates a timestamped directory,
a concise README, a task manifest, and a local target-branch ref. It does not
create a remote branch or a pull request.

Run task commands from inside the task directory so the manifest is discovered
automatically. Use `--manifest <path>` when working elsewhere. The task
directory is the default scope. A manifest may declare durable additional
scopes, while `--include <path> --scope-note <reason>` authorizes an additional
scope for one invocation.

For shared repository mechanisms, use `--kind infrastructure` with explicit
`--scope` paths and a `--scope-note`. Run subsequent commands from the isolated
worktree reported by `task create`; it discovers the private infrastructure
manifest automatically and creates no timestamped task directory.

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
title and description. An infrastructure rename updates its private current
slug while its identity-owned worktree remains in place.

### 5. Choose where retained content lives

First decide whether content should be retained at all. Ignore safely
reproducible caches and intermediate build output that are neither inputs,
deliverables, nor evidence.

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

### 6. Inspect and materialize placement

```sh
workspace-mgr storage status
workspace-mgr storage status task/path/dataset/example.csv
workspace-mgr storage hydrate task/path/dataset
```

With no paths, `storage status` lists ordinary Git content plus explicit or
published S3 boundaries in the resolved task scopes. A directory boundary is
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
`workspace-mgr` is provider-neutral: it publishes the branch transaction but
does not call a GitHub or other hosting API. The agent finds the request by head
branch, reuses it or creates exactly one draft pull request, and never creates a
duplicate. It keeps the title and living description aligned with the goal,
scope, deliverables, validation, and known limitations, then verifies the base,
head, draft/open state, and head revision after every material publication.
Hosting failures are reported immediately. The agent must not merge, enable
auto-merge, approve, close, or mark the pull request ready unless the user
explicitly requests that exact transition. An explicit request to discard one
unmerged task authorizes closing only that task's pull request before cleanup.

Repository-wide policy, root entrypoints, CI, and shared storage mechanisms use
`task create --kind infrastructure`. The command returns an isolated worktree
and stores task metadata privately rather than creating a timestamped task
directory. Work and publication happen from that worktree and remain limited to
the scopes declared at creation.

### 8. Discard an unmerged task instead of saving it

If the user decides that a task should not be retained, first inspect the exact
destructive scope:

```sh
workspace-mgr task discard --dry-run
```

The dry run fetches and records the local task ref, remote task ref, local
shared ref, and remote shared ref in private confirmation state. It reports
working changes, the directory or worktree to remove, additional scopes that
will be restored from the local shared branch, the required pull-request
transition, and any current versioned S3 references that will remain stored.
It does not delete content or refs.

After the user explicitly confirms abandonment, the agent verifies that the
task is unmerged, closes its matching pull request when one exists, and verifies
that provider transition. Confirmation must be run from the shared checkout so
the invoking shell is not left inside the deleted workspace:

```sh
workspace-mgr task discard \
  --manifest /absolute/path/to/.workspace-mgr-task.toml \
  --confirm <exact-task-id>
```

For an infrastructure task, use the private manifest path reported by the dry
run. Confirmation refuses a missing or stale dry run, a changed local or remote
revision, a mismatched task ID, a merged task, an unexpected worktree, or a
branch that does not belong to the task. It deletes the remote branch with an
exact lease, deletes the local branch, removes the deliverable directory or
infrastructure worktree, restores declared shared paths to the local shared
branch, and clears private task state. Local deliverable content is quarantined
until the remote deletion succeeds so a failure can restore it.

Discard never permanently deletes S3 object versions. Its report lists current
managed S3 boundaries and version candidates as `retained-not-purged`; older
unreferenced versions may also remain. Permanent shared-storage garbage
collection is a separate, explicitly authorized problem.

### 9. Refresh after merge

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

In addition to the command-specific effects below, every invocation may read
the crates.io release record when its local update cache is stale. This
best-effort check is bounded, failure-silent, and never performs a remote write.

| Command | Local effect | Remote reads | Remote writes |
| --- | --- | --- | --- |
| `setup` | Installs an isolated private runtime | Python package index | None |
| `init` | Creates or repairs scaffolding | None | None |
| `instructions`, `config show` | Read-only checks/output | None | None |
| `doctor` | Read-only checks/output | S3 bucket settings when configured | None |
| `task create` | Creates task files and a local branch ref | Fetches the Git base branch | None |
| `task rename` | Moves a deliverable directory and rewrites task metadata | Fetches Git refs to reject merged tasks and collisions | None |
| `task status`, `storage status` | Read-only report | None | None |
| `task discard --dry-run` | Saves private confirmation state | Git refs | None |
| `task discard --confirm` | Removes an unmerged task workspace and local refs | Git ref verification | Deletes only the exact remote task branch |
| `storage set`, `storage reset`, `move` | Changes local content/placement metadata | None | None |
| `storage hydrate` | Materializes S3 content | S3 | None |
| `plan` | Creates ignored/private preview state | Git refs and S3 bucket settings when configured | None |
| `publish` | Updates private state and a local target ref | Git and S3 verification | S3 first, then Git |
| `refresh` | Fast-forwards and materializes incoming content | Git and, when needed, S3 | None |

Most `--dry-run` forms suppress normal local mutation. `task discard --dry-run`
also saves its private revision-bound confirmation plan; it changes no task
content or remote. Dry-run never grants broader scope or bypasses safety checks.

## Transaction and failure boundary

The Git commit is the publication point for a combined Git-and-S3 transaction.
A Git revision is never intentionally published before all content it references
is present and verified in S3. If a later Git operation fails, an unreferenced
S3 object version may remain, but the remote Git branch must not point to
missing content. Retrying `publish` is safe; automatic cleanup of shared remote
versions is deliberately outside the product boundary.

All repository paths accepted by task, storage, plan, and publish operations are
repository-relative and must remain inside the resolved scopes. A refusal is a
guard to investigate, not a signal to invoke internal version-control or storage
commands directly.

For exact syntax and every option, see the [command reference](commands.md).
