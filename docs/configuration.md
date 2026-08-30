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

[storage]
enabled = true
url = "s3://example-bucket/workspace"
endpoint_url = "https://s3.example.invalid"
require_object_versioning = true

[agent]
modules = [
  "scope",
  "shared-checkout",
  "publication",
  "artifact-hygiene",
  "storage",
]
```

The repository config is the only source of truth for storage policy and its
non-secret location. `workspace-mgr init` deterministically generates the
lower-level compatibility configuration, and every storage operation refuses
to run if that generated file drifts. Do not edit it or invoke the underlying
storage engine directly; rerun `workspace-mgr init` after changing
`.workspace-mgr.toml`.

`workspace-mgr init --storage-url <url>` enables storage. Add
`--storage-endpoint-url <url>` for an S3-compatible service and
`--require-object-versioning` when the bucket's native object versions are part
of the repository's durability contract. Embedded URL credentials are rejected.
Authentication belongs in platform-standard environment or identity mechanisms;
credentials are never written to tracked configuration.

The internal remote name, engine-specific versioning switch, and interpreter
selection are intentionally absent from this schema. They belong to the
`workspace-mgr` release and cannot vary by repository. Maintainers can override
the version-verification interpreter with `WORKSPACE_MGR_STORAGE_PYTHON` for
packaging and isolated tests; it is not a repository setting.

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

`require_readme` is enforced before publication. `draft_pull_request` controls
the generated review instructions; pull-request API calls are intentionally
outside the provider-neutral transaction engine.
