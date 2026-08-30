# Configuration reference

The repository config is `.workspace-mgr.toml` in the Git root. All schemas use
an integer `schema_version`; unknown schema versions are rejected.

```toml
schema_version = 1
required_cli = ">=0.1.0-alpha.1,<0.2.0"
profile = "shared-checkout"

[git]
remote = "origin"
base_branch = "main"
shared_checkout_branch = "main"
branch_prefix = "codex/"

[tasks]
enabled = true
directory_pattern = "%Y%m%d-%H%M%S-{slug}"
manifest_name = ".workspace-mgr-task.toml"
require_readme = true
draft_pull_request = true

[large_files]
threshold_bytes = 10485760
primary = "dvc"
fallback = "git-lfs"

[dvc]
enabled = true
remote = "storage"
require_version_aware = false
python = "python3"

[agent]
modules = [
  "scope",
  "shared-checkout",
  "publication",
  "artifact-hygiene",
  "dvc",
]
```

The repository config expresses policy. The actual DVC URL remains in
`.dvc/config`, and credentials remain in `.dvc/config.local` or environment
variables.

`init --dvc-remote <name> --dvc-remote-url <url>` writes a non-secret remote
location to tracked DVC configuration. It rejects embedded URL credentials;
authentication belongs in local DVC configuration or the environment.

New task manifests contain only task-specific state:

```toml
schema_version = 1
id = "20260829-170000-example"
path = "20260829-170000-example"
branch = "codex/example"

[[additional_scopes]]
path = "docs/shared.md"
reason = "The user explicitly requested this shared documentation change"
```

Legacy `.chat-sync.json` version 1 manifests remain readable during the
documented migration window. Their `remote`, `base_branch`, and `shared_head`
fields override repository defaults for compatibility.

`require_readme` is enforced before publication. `draft_pull_request` controls
the generated review instructions; pull-request API calls are intentionally
outside the provider-neutral transaction engine.
