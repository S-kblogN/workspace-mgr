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

`workspace-mgr init --s3-url <url> [--s3-endpoint-url <url>]` writes these
public facts and deterministically generates the private storage-engine
configuration. Every S3 operation rejects drift in that derived file. Users and
agents should edit only `.workspace-mgr.toml` and rerun `workspace-mgr init`.
The generated file remains product-owned even if its content is old, edited, or
damaged; content comparison detects drift but does not determine ownership. If
retained S3 boundaries exist and that generated file can no longer identify its
location, `init` uses the committed public facts as the relocation-safety anchor
before repairing it.
Once S3 boundaries exist, `init` will not relocate them to another URL or
endpoint; place all retained boundaries in Git before changing the repository's
S3 location.

## Minimum workspace-mgr version

`minimum_cli_version` names the oldest `workspace-mgr` release that can read
the repository's tracked task state. It is a compatibility fact that
`workspace-mgr` maintains, not a policy switch: it is only a lower bound and
never selects behavior. When it is absent, the repository has no requirement.
The value is a plain release version such as `"0.4.0"`; pre-release versions
and build metadata are rejected. An installed release meets the declaration
when its version is at least the declared one by semantic-version precedence.
A pre-release also meets a declaration of its own release, so 0.4.0-rc.1 meets
`"0.4.0"`, while it does not meet `"0.4.1"`.

Publication maintains the declaration. Task manifest schema 3, which records a
cloud-usage approval, needs `workspace-mgr` 0.4.0; schemas 1 and 2 need no
declaration. Unless the task is authorized to change `.workspace-mgr.toml`
itself, and as long as the task branch's copy of the file is exactly what
`workspace-mgr` wrote there, each publication reconciles the file in its own
private Git index, never in the shared checkout, against the configuration at
the point where the task branch left the base branch:

- When the branch never changed the file and no task manifest in the published
  tree needs more than that starting point declares, the publication carries
  the starting point's configuration exactly. A task that never needs a newer
  release therefore never changes the file.
- When a task manifest needs a newer release, the publication carries the
  starting point's configuration in its canonical form with the higher of that
  requirement and the fetched base branch's declaration. Task branches raised
  for the same schema write the same content, and a branch raised earlier
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
higher value. `init` keeps an existing declaration exactly. `init`, and
publication whenever it rewrites the declaration, write this file in its
canonical form, so comments in it are not preserved then.

A build never publishes a declaration that it does not meet itself: when a
task manifest needs a newer release than the installed one, `plan` and
`publish`, including `publish --dry-run`, refuse before anything is placed or
uploaded. When that manifest is the task's own, the refusal also offers
recording the default limit, which removes the approval; when it is another
task's manifest in the published tree, such as one merged on the base branch,
the refusal names it and only an update helps.

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
- deliverable tasks use timestamped top-level directories, a README, and
  `.workspace-mgr-task.toml`;
- shared repository changes use an infrastructure task in an isolated
  worktree;
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
requires at least one `additional_scopes` entry. It is stored in private
worktree Git state rather than committed to the repository. The manifest schema
version describes serialized task state; it is not a strategy selector.

Every field shown above except `additional_scopes` is required; the schema 3
`cloud_usage_approval` table described below is optional. The ID is the
immutable creation identity: a deliverable uses
`YYYYMMDD-HHMMSS-<original-slug>` and an infrastructure task uses
`infra-<original-slug>`. The branch is derived from that immutable identity and
does not change. `slug` is the current lowercase ASCII kebab-case topic label.
A deliverable path uses the ID's original timestamp plus the current slug, so
`task rename` can move it without replacing the task or review branch. An
infrastructure task has no `path`; its private worktree remains keyed by the
stable ID. Declared scopes must be distinct and non-overlapping. Schema 1
manifests are still readable with their original slug; `task rename` and
`task approve-cloud-usage` rewrite them as schema 2, or as schema 3 when they
record an approval. These constraints are validated whenever a manifest is
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
without an approval and schema 3 with one, so a task without an approval never
requires a newer release. Running `task approve-cloud-usage` with a limit equal
to the threshold removes the table and returns the manifest to schema 2;
publishing that change also withdraws the task branch's `minimum_cli_version`
raise unless another task manifest still needs it, but never below the base
branch's declaration. Reading schema 3 needs
`workspace-mgr` 0.4.0 or newer, which is why publishing a deliverable manifest
that records an approval raises `minimum_cli_version`. An infrastructure
manifest stays private, so its approval is published only as the
`Cloud-Usage-Approval` commit trailer and never raises the declaration.
