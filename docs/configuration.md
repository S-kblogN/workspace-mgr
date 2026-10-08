# Configuration reference

`.workspace-mgr.toml` describes the external facts that differ between
repositories. It does not select a workspace-management strategy.
`workspace-mgr` applies one product-owned strategy to every initialized
repository, so policy changes ship as tested CLI releases rather than per-repo
configuration switches.

The complete public schema is:

```toml
minimum_cli_version = "0.4.0"

[git]
remote = "origin"
branch = "main"

[s3]
url = "s3://example-bucket/workspace"
endpoint_url = "https://s3.example.invalid"
```

`[git]`, `git.remote`, and `git.branch` are required. `minimum_cli_version`
and `[s3]` are optional. Unknown fields and incomplete sections are rejected so
a repository cannot silently invent or retain policy knobs—or fall back to
unstated repository facts—that the product does not support.

## Git facts

`git.remote` names the Git remote used for publication and refresh.
`git.branch` names the shared and base branch. Repository URLs are discovered
through the named remote; they are never compiled into the binary or copied
into this file. The remote must be a safe Git remote name rather than a URL or
command-line option.

## S3 facts

Adding `[s3]` enables S3 placement. `url` must use `s3://` in release builds.
`endpoint_url` is optional and supports S3-compatible services.

S3 has one fixed product contract: bucket object versioning is required and
exact object version IDs are verified before Git publication. There is no
configuration switch that weakens this guarantee. URL userinfo, queries, and
fragments are rejected so tracked locations cannot carry credentials or signed
URLs. Authentication belongs in ignored local configuration or
platform-standard identity and environment mechanisms.

The native engine reads AWS access/secret keys and optional session tokens from
the environment, the selected shared credentials/profile files, or ignored
local remote settings. It also accepts a profile `credential_process`, invoking
its argument vector directly without a shell. Role, SSO, and web identity
profiles must supply resolved temporary environment credentials or a credential
process; unsupported profiles fail before any S3 request.
Set the bucket's region through `AWS_REGION`, `AWS_DEFAULT_REGION`, or the
selected profile. B2 regions can also be inferred from their service endpoint.
The native transport does not replay signed requests across provider redirects.

`workspace-mgr manage --s3-url <url> [--s3-endpoint-url <url>]` writes
these public facts. The native engine reads `.workspace-mgr.toml` directly;
there is no generated second remote configuration. Once storage boundaries
exist, `manage` refuses changing the URL or endpoint. Place retained boundaries
in Git before changing the repository's S3 location.

Machine-local authentication can be saved in the ignored
`.workspace-mgr/local/credentials.toml`:

```toml
profile = "research"
region = "us-east-1"
```

The supported optional fields are `access_key_id`, `secret_access_key`,
`session_token`, `profile`, `region`, and `credential_process`. Access and secret
keys must be supplied together, and session tokens require those keys. Location
and endpoint fields are deliberately excluded: tracked repository facts own
routing. The CLI never prints credential values in management reports.

## Storage manifests and repository migration

Each managed file or directory has one adjacent `.wm-storage.json` sidecar.
Native schema 1 records `path`, `kind`, `checksum` (algorithm and digest), physical
`size`, and optional exact `version` (id and etag). Directory boundaries also
record complete relative `entries`, each with its own checksum, size and version.
Unknown fields, unsupported schema versions, unsafe paths and inconsistent
content identities are rejected. See [Native repository storage](storage.md) for
examples and the engine contract.

`workspace-mgr manage --repo <path> --dry-run` previews legacy storage migration
and scaffold reconciliation together. Remove `--dry-run` to apply the reviewed
transaction. The command inventories all current pointers, including nested
archive directories, while keeping nested Git repositories opaque. It preserves
payload bytes and imported checksum algorithms, and leaves Git history and the
index unchanged. Path-based, version-aware imports preserve their existing exact
object bindings. Ordinary DVC S3 imports retain the source CAS keys and create
verified native object versions at repository-relative paths.
Run legacy adoption in the primary shared checkout. A linked worktree can
reconcile native scaffolding once the repository has been adopted.

The S3 importer supports path-based, version-aware legacy remotes with complete
exact object bindings, and ordinary DVC 2 or DVC 3 content-addressed remotes.
Selected legacy remote names can be arbitrary. A remote-only directory listing
is sufficient when its entries and aggregate identity can be verified. Native
destinations require bucket versioning and conditional writes; imported CAS
objects are read under an exact version or ETag condition and copied without
changing their raw bytes.
`--dry-run` reports the verified remote inventory and planned transfer bytes
without local or remote writes. Pipelines, custom controls, incomplete or
ambiguous identities, active storage transactions and destination collisions
refuse before conversion. See [the import contract](storage.md#migrating-legacy-dvc-repositories).

Verified legacy sidecars and managed controls are removed after replacement
verification. Cache trees move into `.workspace-mgr/local/cache`; other legacy
local state remains in `.workspace-mgr/local/retained-storage-state`. Supported
local authentication moves into `credentials.toml`. Unrelated attribute and
payload-ignore rules remain. A private durable journal allows an interrupted
local operation to roll back on the next `manage`; recovery protects later user
edits. CAS transfers keep a separate `storage-import.json` journal until local
conversion succeeds. Re-running `manage` verifies the source inventory and
reuses owned, verified uploaded versions after a partial transfer or lost response.
`manage --cancel-migration` abandons a failed import plan without deleting old
controls, remote versions, cache or upload receipts, so source or metadata repairs
can be followed by a fresh `manage`. Combine it with `--dry-run` to preview;
S3 routing options cannot accompany cancellation.

Publish the converted manifests, their legacy sidecar deletions, obsolete
controls and updated configuration together in one infrastructure task.
Publication refuses to delete legacy routing controls while its proposed Git
tree still needs them. Retained source CAS hashes remain available to old Git
history; native path cleanup does not automatically garbage-collect them.

## Minimum workspace-mgr version

`minimum_cli_version` names the oldest `workspace-mgr` release that can read
the repository's tracked task and storage state. It is a compatibility fact that
`workspace-mgr` maintains, not a policy switch: it is only a lower bound and
never selects behavior. When it is absent, the repository has no requirement.
The value is a plain release version such as `"0.4.0"`; pre-release versions
and build metadata are rejected. An installed release meets the declaration
when its version is at least the declared one by semantic-version precedence.
A pre-release also meets a declaration of its own release, so 0.4.0-rc.1 meets
`"0.4.0"`, while it does not meet `"0.4.1"`.

Management raises the declaration to at least 0.8.0 when adopting native storage. Publication maintains the declaration for subsequent task and storage changes. Task manifest schema 3, which records a
cloud-usage approval, needs `workspace-mgr` 0.4.0. Schema 4, which retains
archive completion evidence, needs 0.7.0. Top-level manifests with
schemas 1 and 2 need no declaration. A nested archive task manifest needs
0.5.0 regardless of whether its schema is 1, 2, or 3. Unless the task is
authorized to change `.workspace-mgr.toml` itself, and as long as the task
branch's copy of the file is exactly what `workspace-mgr` wrote there, each
publication reconciles the file in its own private Git index, never in the
shared checkout, against the configuration at the point where the task branch
left the base branch:

- When the branch never changed the file and no task manifest in the published
  tree needs more than that starting point declares, the publication carries
  the starting point's configuration exactly. A task that never needs a newer
  release therefore never changes the file.
- When a task manifest needs a newer release, the publication carries the
  starting point's configuration in its canonical form with the higher of that
  requirement and the fetched base branch's declaration. Task branches raised
  for the same requirements write the same content, and a branch raised earlier
  follows a base branch that a later release raised further on its next
  publication, so both merge cleanly.
- When no task manifest needs the branch's earlier raise any more, for example
  after the user reset an approval, the publication withdraws the raise, but
  never below the fetched base branch's declaration: it returns to the
  starting point's configuration exactly when the base branch declares no more
  than that, and otherwise carries the starting point's configuration with the
  base branch's declaration. A withdrawal therefore never lowers a declaration
  the base branch already carries, even when a hosting provider replays the
  branch's commits onto the base branch in a rebase merge.

When the task is authorized to change `.workspace-mgr.toml`, or an earlier
publication of the branch changed anything else in the file, even only comments
or formatting, the file's content belongs to the user: publication keeps the
staged file and only raises its declaration when it is lower than what the
task manifests need or than the fetched base branch's declaration, to the
highest of those, and never lowers it. Such a branch therefore also follows a
base branch that a later release raised further.

Once a raise is merged into the base branch, it stays even after the manifests
that needed it are gone; nothing lowers a merged declaration. A branch that was
raised before the base branch was raised further conflicts with it until the
branch is published again; resolving such a conflict by hand must keep the
higher value. `manage` preserves an existing declaration or raises it to at
least 0.8.0 when adopting native storage; it never lowers one. `manage`, and
publication whenever it rewrites the declaration, write this file in its
canonical form, so comments in it are not preserved then.

A build never publishes a declaration that it does not meet itself: when a
task manifest needs a newer release than the installed one, `plan` and
`publish`, including `publish --dry-run`, refuse before anything is placed or
uploaded. The refusal offers recording the default limit only when removing
the task's own approval clears its schema requirement. An archived-path
requirement, or another task's requirement in the published tree, needs an
update; removing an approval does not make an archived path readable by an
older release.

A release older than the declaration refuses the repository before it reads
anything else from this file, so a declaration written next to fields that only
newer releases know still yields a clear message. Every repository command
other than `doctor`, including `instructions`, stops with an error that names
the installed and required versions. Commands that fetch also check what they
fetched before they change anything: `task create` and `task discard` check the
base branch, `task rename`, `plan`, and `publish` check the base branch and the
task branch, and `refresh` checks the incoming revision. `doctor` still runs
and reports the comparison as its `cli-version` check, which also considers
the declaration last fetched from the base branch. Releases up to 0.3.0 do not
know the key at all and reject the file for its unknown field. Update the CLI
instead of editing, removing, or lowering the declaration; users and agents
never write this key by hand.

## Fixed workspace policy

The following behavior is deliberately not configurable:

- one writable conversation maps to one task, one `codex/` branch, and one
  draft pull request;
- active deliverable tasks use timestamped top-level directories, a README,
  and `.workspace-mgr-task.toml`;
- completed deliverable task directories whose pull requests are confirmed
  merged may be grouped under time folders only in a user-requested
  infrastructure task; merge and turn-end synchronization never organize them
  automatically;
- shared repository changes use an infrastructure task in the same shared
  checkout, with a private manifest selected explicitly;
- the shared checkout remains on `git.branch` and preserves unrelated overlays;
- Git is the collaboration/control plane and S3 is the artifact/data plane;
  agents record clear semantic choices, while unclassified new files use the
  fixed below-1 MiB Git preference, 1–10 MiB review band, and above-10 MiB S3
  fallback; published or explicitly selected placement stays sticky;
- each task's cloud usage across Git and S3 is limited to 1 GiB (1073741824
  bytes) until the user approves a higher limit for that task, and publication
  refuses growth past the limit; the threshold has no repository setting;
- the agent creates and maintains the pull request title and living
  description, creates the draft pull request immediately after a deliverable
  task's initial scaffold publication, and automatically reconciles the local
  task, remote branch, and pull request before every writable-task turn ends;
- the user or maintainer controls merge, ready, approval, close, and auto-merge
  transitions; after the user explicitly abandons an unmerged task, the agent
  closes that task's pull request before confirmed `task discard` cleanup;
- all instruction topics are always available, and Git/S3 are the only public
  storage concepts.

Users may still explicitly select Git or S3 for a path, authorize an additional
scope, approve a higher cloud-usage limit for one task, or request a narrow
exceptional action. Those are task-level decisions, not alternate repository
strategies.

Completed-task folder structure is flexible, for example `YYYY/<task-dir>`,
`YYYYMM/<task-dir>`, or `YYYY/MM/<task-dir>`. If the user requests organization
without choosing a structure, use `YYYY/MM/<task-dir>` based on each directory's
timestamp, unless the user specifies another date basis. Preserve each task's
basename, retained contents, immutable task ID, and target branch. This is a
choice for the requested organization task, not a repository configuration
option.

## Task manifests

Task manifests contain task-specific state rather than repository policy:

```toml
schema_version = 2
kind = "deliverable"
id = "20260829-170000-example"
slug = "example"
path = "20260829-170000-example"
branch = "codex/example"
title = "Example"
purpose = "Produce one reviewable example"

[[additional_scopes]]
path = "docs/shared.md"
reason = "The user explicitly requested this shared documentation change"
```

An infrastructure manifest uses `kind = "infrastructure"`, omits `path`, and
requires at least one `additional_scopes` entry. It is stored in the primary
checkout's ignored `.workspace-mgr/local/` rather than committed to the
repository. All linked worktrees use that same private state directory.
Task creation reports an absolute `manifest` path below
`<primary-checkout>/.workspace-mgr/local/infrastructure-tasks/<id>/.workspace-mgr-infrastructure.toml`
for subsequent `--manifest <path>` selection. Infrastructure work
stays in the shared checkout on `git.branch`; no task worktree is created.
Existing state under `<git-common-dir>/workspace-mgr` migrates automatically;
old `--manifest` paths continue to work. See the
[upgrade guide](guide.md#upgrading-private-product-state) before using the new
CLI alongside an older installation.
The manifest schema version describes serialized task state; it is not a
strategy selector.

Every field shown above except `additional_scopes` is required; the schema 3
`cloud_usage_approval` and schema 4 `archive_completion` tables described below
are optional. The ID is the
immutable creation identity: a deliverable uses
`YYYYMMDD-HHMMSS-<original-slug>` and an infrastructure task uses
`infra-<original-slug>`. The branch is derived from that immutable identity and
does not change. `slug` is the current lowercase ASCII kebab-case topic label.
A deliverable directory's basename uses the ID's original timestamp plus the
current slug. Active tasks stay at the top level; after user-requested
organization, a completed task's full `path` may include time-folder parents.
The manifest's `path` must match its actual repository-relative directory, and
the directory basename must still match the timestamp and current slug.
Organization preserves the immutable task ID and target branch. `task rename`
changes an active task's current slug without replacing the task or review
branch. An infrastructure task has no `path`; its private manifest remains
keyed by the stable ID. Declared scopes must be distinct and non-overlapping.
Schema 1 manifests are still readable with their original slug; `task rename`
and `task approve-cloud-usage` rewrite them as schema 2, or as schema 3 when
they record an approval. A retained completion checkpoint uses schema 4.
These constraints are validated whenever a manifest is
loaded, so hand-editing task state cannot select another repository-management
strategy.

Schema 3 is schema 2 plus one optional table that records the user's
cloud-usage approval for the task. `task approve-cloud-usage` writes it; never
add or edit it by hand:

```toml
schema_version = 3
kind = "deliverable"
id = "20260829-170000-example"
slug = "example"
path = "20260829-170000-example"
branch = "codex/example"
title = "Example"
purpose = "Produce one reviewable example"
additional_scopes = []

[cloud_usage_approval]
limit_bytes = 2147483648
note = "The user approved 2 GiB for the training checkpoints"
```

`limit_bytes` is the approved limit in bytes, a non-negative TOML integer of at
most 9223372036854775807, and `note` is the user's decision on one non-empty
line. Both are required and no other field is accepted; Git history records when
the approval was made. Schema 1 and 2 manifests must not contain the table.
`workspace-mgr` writes the lowest schema that represents a manifest: schema 2
without either optional table, schema 3 with only an approval, and schema 4
with a completion checkpoint, whether or not it also has an approval. A
manifest without either table needs no newer release for its schema, but a
nested archive path still needs 0.5.0.
Running `task approve-cloud-usage` with a limit equal to the threshold removes
the approval table and returns the manifest to schema 2 only if no checkpoint
remains;
publishing that change also withdraws the task branch's `minimum_cli_version`
raise unless another task manifest still needs it, but never below the base
branch's declaration. Reading schema 3 needs
`workspace-mgr` 0.4.0 or newer, which is why publishing a deliverable manifest
that records an approval raises `minimum_cli_version`. An infrastructure
manifest stays private, so its approval is published only as the
`Cloud-Usage-Approval` commit trailer and never raises the declaration.

Schema 4 adds an optional `[archive_completion]` table for deliverable tasks.
Existing checkpoints remain readable and are preserved by current metadata
operations; `task upgrade` no longer synthesizes historical content proof.
Archive uses saved current branch associations as hints, not proof replay.
Do not create or edit the evidence by hand. The table contains:

| Fields | Meaning |
| --- | --- |
| `schema_version = 1` | Format of this evidence record, distinct from task schema 4 |
| `task_id`, `repository`, `base_branch` | Binding to the current task and hosting repository/base |
| `checkpoint_commit`, `checkpoint_path`, `checkpoint_tree` | Original Git commit, known directory path and opaque tree ID |
| `branches` | Verified review branches and the current canonical branch |
| `reviews` | Immutable PR number/URL, branch, merge timestamp, merge commit and head commit facts |

The record is compatible saved control metadata, not a trusted completed flag.
Archive queries live PR facts for current associated branches, without verifying
historical directory-tree changes, branch ancestry or commit review coverage.
Neither upgrade nor archive reads historical task configuration. Current
manifest validation remains strict and rejects unknown fields or unsupported
schemas. Rename, approval changes and archive preserve compatible checkpoint
fields; clearing an approval therefore does not remove the schema 4 requirement.
Publishing schema 4 raises `minimum_cli_version` to at least 0.7.0.
