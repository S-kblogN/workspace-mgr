# User guide

`workspace-mgr` is the single repository-management interface for people and
coding agents. It creates the repository and task scaffolding, explains the
effective repository policy, decides where retained content lives, and
publishes a task without switching a shared checkout.

Begin with the [repository management model](management-model.md). It explains
the one-conversation/one-task/one-branch/one-PR relationship, separates task
scope from Git/S3 placement, and defines publication and shared-checkout
semantics before introducing commands. The same source document appears first
in the default output of `workspace-mgr instructions`, so users and agents
reason from one model rather than parallel summaries.

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
and actions without changing the host.

### 2. Initialize or adopt a repository

Run `init` once from the Git repository:

```sh
workspace-mgr init --profile shared-checkout \
  --s3-url s3://example-bucket/workspace \
  --s3-endpoint-url https://s3.example.invalid
```

Initialization creates:

- `.workspace-mgr.toml`, the public repository policy;
- a thin `AGENTS.md` bootstrap;
- internal storage scaffolding when S3 is configured.

If `AGENTS.md` already contains repository rules, `init` refuses to overwrite
it. Review the file, then use `init --adopt` to preserve it as
`.workspace-mgr/instructions/repository.md` and install the thin bootstrap.
`--dry-run` reports every planned action without writing files.

Use the `standard` profile for an ordinary checkout. Use `shared-checkout` when
multiple tasks may leave independent working-tree overlays in one checkout
that remains on a shared branch such as `main`.

### 3. Load the policy for an agent session

The generated `AGENTS.md` asks every agent to run:

```sh
workspace-mgr instructions --repo .
```

`instructions` is intentionally different from `help`: help explains command
syntax, while instructions first establishes the shared management model and
then renders the repository's actual operating policy. The output combines the
canonical model document, built-in policy modules, `.workspace-mgr.toml`, and
the optional repository-specific module. Run `workspace-mgr doctor` before work
if the installation or repository state may be inconsistent.

The default `all` document contains the model followed by all enabled rules.
`instructions model` returns only the conceptual model. The `[agent].modules`
list controls which operational sections appear; the operating core is always
included in `all`, while task scope, publication, artifact hygiene, storage,
shared-checkout, and infrastructure rules are independent modules. Requesting a
disabled operational topic is an error instead of silently returning incomplete
policy.

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

### 5. Choose where retained content lives

Most files need no manual choice. With the default automatic policy, a new file
above the configured threshold goes to S3 and a smaller file goes to Git.
Previously published placement is sticky: changing a file's size does not
silently move it between storage locations.

Use an explicit choice when intent matters more than size:

```sh
workspace-mgr storage set task/path/report.pdf \
  --to git --reason "Review the rendered report in Git"

workspace-mgr storage set task/path/dataset \
  --to s3 --reason "Retain the dataset as one versioned directory"
```

An explicit choice is valid in either direction at any size. A directory is one
placement boundary and all descendants inherit it. Nested placement boundaries
are rejected: set or reset the existing boundary instead. `storage set`,
`storage reset`, and `move` change only local desired state; none writes to a
remote.

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
`selected_by` explains the result:

| `selected_by` | Meaning |
| --- | --- |
| `explicit` | The path itself has an explicit choice |
| `explicit-ancestor` | An explicit directory boundary contains the path |
| `published-history` | Existing published history fixes this path in Git or S3 |
| `published-ancestor` | A published S3 directory boundary contains the path |
| `automatic` | Repository defaults and, for a new file, its current size decide |

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
does not call a GitHub or other hosting API. If repository policy asks for one
draft pull request per task, the user or agent creates and updates it separately.
Merging is likewise an explicit reviewer or maintainer action.

### 8. Refresh after merge

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
| The content should appear directly in diffs and repository history | The content is binary, bulky, or naturally retrieved as data |
| Ordinary clone and checkout behavior is desirable | Exact object-version recovery matters |
| The file is intentionally reviewable despite being large | A directory should be retained as one logical boundary |

The automatic threshold is a useful default for new content. Explicit
`--to git` and `--to s3` choices always take precedence until reset.

For an `s3://` location, bucket object versioning is mandatory. Publication
records and verifies exact object version IDs before publishing the Git
revision. Credentials never belong in `.workspace-mgr.toml`; use ignored local
configuration or platform-standard identity mechanisms.

## Side effects by command

| Command | Local effect | Remote reads | Remote writes |
| --- | --- | --- | --- |
| `setup` | Installs an isolated private runtime | Python package index | None |
| `init` | Creates or repairs scaffolding | None | None |
| `instructions`, `config show` | Read-only checks/output | None | None |
| `doctor` | Read-only checks/output | S3 bucket settings when configured | None |
| `task create` | Creates task files and a local branch ref | Fetches the Git base branch | None |
| `task status`, `storage status` | Read-only report | None | None |
| `storage set`, `storage reset`, `move` | Changes local content/placement metadata | None | None |
| `storage hydrate` | Materializes S3 content | S3 | None |
| `plan` | Creates ignored/private preview state | Git refs and S3 bucket settings when configured | None |
| `publish` | Updates private state and a local target ref | Git and S3 verification | S3 first, then Git |
| `refresh` | Fast-forwards and materializes incoming content | Git and, when needed, S3 | None |

`--dry-run` suppresses the normal local mutation of commands that support it.
It never grants broader scope or bypasses safety checks.

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
