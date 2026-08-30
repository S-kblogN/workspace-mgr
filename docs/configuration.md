# Configuration reference

Repository policy lives in `.workspace-mgr.toml` at the Git root. Unknown fields
and unsupported schema versions are rejected.

```toml
schema_version = 1
required_cli = ">=0.1.0-alpha.1,<0.2.0"
profile = "shared-checkout"

[publication]
remote = "origin"
base_branch = "main"
shared_checkout_branch = "main"
branch_prefix = "codex/"

[tasks]
enabled = true
directory_pattern = "%Y%m%d-%H%M%S-{slug}"
manifest_name = ".workspace-mgr-task.toml"
require_readme = true

[review]
pull_request = "required"
initial_state = "draft"
managed_by = "agent"
merge_authority = "user"

[storage]
default = "auto"
auto_s3_above_bytes = 10485760

[storage.s3]
url = "s3://example-bucket/workspace"
endpoint_url = "https://s3.example.invalid"

[agent]
modules = [
  "scope",
  "shared-checkout",
  "publication",
  "artifact-hygiene",
  "storage",
]
```

## Publication

`publication` identifies where task branches are published and which branch a
shared checkout must keep mounted. Repository URLs are discovered through the
named Git remote; they are never compiled into the binary. `remote` must be a
safe Git remote name, not a URL or command-line option.

## Storage

`storage.default` is `auto`, `git`, or `s3`. In `auto`, an unplaced new file
larger than `auto_s3_above_bytes` is placed in S3 and a smaller file is placed in
Git. Published placement is sticky. An explicit per-path choice made with
`storage set` overrides the default until `storage reset`.

`[storage.s3]` enables S3 and `url` must use `s3://`. It has one fixed product contract:
bucket object versioning is required and exact version IDs are verified before
Git publication. There is no configuration switch that weakens this guarantee.
`endpoint_url` supports S3-compatible services.

URL userinfo, queries, and fragments are rejected so tracked locations cannot
carry credentials or signed URLs. Authentication belongs in ignored local
configuration or platform-standard identity and environment mechanisms.

`workspace-mgr init --s3-url <url> [--s3-endpoint-url <url>]` writes this public
configuration and deterministically generates the private engine configuration.
Every S3 operation rejects drift in that derived file. Users and agents should
edit only `.workspace-mgr.toml` and rerun `workspace-mgr init`.

`required_cli` is enforced by every repository operation. `doctor` remains able
to load an incompatible configuration so it can report the required and
installed versions without mutating the repository.

## Tasks

Task manifests contain task-specific state only:

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
worktree Git state rather than committed to the repository.

`require_readme` is checked for deliverable tasks before publication.
Infrastructure task metadata is private Git worktree state and has no README or
timestamped repository directory.

## Review

`review.pull_request` is `required`, `optional`, or `disabled`.
`initial_state` is `draft` or `ready`; `managed_by` is `agent` or `user`; and
`merge_authority` is `user` or `agent`. The defaults require the agent to create
and maintain exactly one draft pull request while reserving merge-state actions
for the user. These values generate an explicit agent responsibility contract
and a structured publication handoff. They never make `workspace-mgr` call a
GitHub or other hosting-provider API.

## Agent instruction modules

`[agent].modules` controls the generated policy returned by
`workspace-mgr instructions`. The canonical workspace model and operating core
are always included in the default `all` document and are not module-controlled;
`instructions model` returns the model by itself. Supported optional modules are:

| Module | Instruction topic | Policy added |
| --- | --- | --- |
| `scope` | `task` | Task creation, manifest scope, and README rules |
| `publication` | `publish` | Planning, branch publication, and review handoff |
| `artifact-hygiene` | `artifacts` | Nested repositories, generated output, credentials, and retained artifacts |
| `storage` | `storage` | Git/S3 placement and hydration rules |
| `shared-checkout` | `shared-checkout` | Overlay preservation and post-merge refresh |
| `infrastructure` | `infrastructure` | Isolation and test rules for shared repository mechanisms |

The default module set includes scope, publication, artifact hygiene, and
storage. `shared-checkout` is added by `init --profile shared-checkout`.
Infrastructure policy is opt-in. Unknown and duplicate module names are
rejected. Requesting a topic whose module is disabled is also rejected, so an
agent cannot mistake an empty document for effective policy.
