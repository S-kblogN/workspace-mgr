# Configuration reference

`.workspace-mgr.toml` describes the external facts that differ between
repositories. It does not select a workspace-management strategy.
`workspace-mgr` applies one product-owned strategy to every initialized
repository, so policy changes ship as tested CLI releases rather than per-repo
configuration switches.

The complete public schema is:

```toml
[git]
remote = "origin"
branch = "main"

[s3]
url = "s3://example-bucket/workspace"
endpoint_url = "https://s3.example.invalid"
```

`[git]`, `git.remote`, and `git.branch` are required. `[s3]` is optional.
Unknown fields and incomplete sections are rejected so a repository cannot
silently invent or retain policy knobs—or fall back to unstated repository
facts—that the product does not support.

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
Once S3 boundaries exist, `init` will not relocate them to another URL or
endpoint; place all retained boundaries in Git before changing the repository's
S3 location.

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
- the agent creates and maintains the pull request title and living
  description;
- the user or maintainer controls merge, ready, approval, close, and auto-merge
  transitions; after the user explicitly abandons an unmerged task, the agent
  closes that task's pull request before confirmed `task discard` cleanup;
- all instruction topics are always available, and Git/S3 are the only public
  storage concepts.

Users may still explicitly select Git or S3 for a path, authorize an additional
scope, or request a narrow exceptional action. Those are task-level decisions,
not alternate repository strategies.

## Task manifests

Task manifests contain task-specific state rather than repository policy:

```toml
schema_version = 1
kind = "deliverable"
id = "20260829-170000-example"
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

Every field shown above except `additional_scopes` is required. A deliverable
ID and path must be exactly `YYYYMMDD-HHMMSS-<slug>` and its branch must be
`codex/<slug>`. An infrastructure ID must be `infra-<slug>` and its branch must
be `codex/infra-<slug>`. Slugs are lowercase ASCII kebab case. Declared scopes
must be distinct and non-overlapping. These constraints are validated whenever
a manifest is loaded, so hand-editing task state cannot select another
repository-management strategy.
