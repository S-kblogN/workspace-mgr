# User guide

`workspace-mgr` is the single repository-management interface for people and
coding agents. It creates the repository and task scaffolding, explains the
effective repository policy, decides where retained content lives, and
publishes a task without switching a shared checkout.

The public model deliberately has only two storage locations: **Git** and
**S3**. The implementation may use other programs internally, but users and
agents should not configure or invoke those programs directly.

## The mental model

Five concepts describe the complete workflow.

| Concept | Meaning | Main operations |
| --- | --- | --- |
| Repository policy | Tracked, non-secret defaults in `.workspace-mgr.toml` | `init`, `config show`, `doctor` |
| Agent instructions | The effective operating rules generated from policy modules | `instructions` |
| Task | One reviewable unit with a directory, manifest, README, and target branch | `task create`, `task status` |
| Placement | Whether retained content is stored in Git or S3 | `storage status`, `set`, `reset`, `hydrate`, `move` |
| Publication | One scoped, verified Git-and-S3 transaction | `plan`, `publish`, `refresh` |

The task manifest defines the paths and branch involved in one transaction.
Placement answers where content is stored. Publication is the only operation
that makes the task visible on a remote.

## Repository lifecycle

### 1. Initialize or adopt a repository

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

### 2. Load the policy for an agent session

The generated `AGENTS.md` asks every agent to run:

```sh
workspace-mgr instructions --repo .
```

`instructions` is intentionally different from `help`: help explains command
syntax, while instructions renders the repository's actual operating policy.
The output combines built-in policy modules, `.workspace-mgr.toml`, and the
optional repository-specific module. Run `workspace-mgr doctor` before work if
the installation or repository state may be inconsistent.

The `[agent].modules` list controls which policy sections appear. The operating
core is always present; task scope, publication, artifact hygiene, storage,
shared-checkout, and infrastructure rules are independent modules. Requesting
a disabled topic is an error instead of silently returning incomplete policy.

### 3. Create one task

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

### 4. Choose where retained content lives

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
automatic policy. Resetting a directory removes its atomic directory choice;
its files may then be evaluated independently during planning and publication.

### 5. Inspect and materialize placement

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

### 6. Plan, then publish

```sh
workspace-mgr plan
workspace-mgr publish -m "Publish the model report"
```

`plan` resolves the task branch and scopes, fetches relevant Git refs, evaluates
placement, validates the private prospective Git tree, and reports the exact
changes. It may update ignored local transaction state, but it creates no
commit, uploads no S3 content, and publishes no branch.

`publish` repeats the validation. It first reconciles and uploads every in-scope
S3 boundary, verifies the exact remote content, builds a Git commit from only
the declared scopes, pushes an explicit target-branch ref, and verifies the
remote object ID. It does not switch the checkout or stage files in the shared
Git index.

Creating and maintaining a pull request remains a repository-hosting action.
`workspace-mgr` is provider-neutral: it publishes the branch transaction but
does not call a GitHub or other hosting API. If repository policy asks for one
draft pull request per task, the user or agent creates and updates it separately.
Merging is likewise an explicit reviewer or maintainer action.

### 7. Refresh after merge

In a shared checkout, use:

```sh
workspace-mgr refresh
```

`refresh` fetches the configured shared branch, permits only a fast-forward,
prefetches incoming S3 content, updates the local branch and index without
overwriting unrelated working-tree overlays, then materializes and verifies
incoming S3 content. If the update fails after the local ref changes, it
attempts to restore the previous ref, index, metadata, and outputs.

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
| `init` | Creates or repairs scaffolding | None | None |
| `instructions`, `doctor`, `config show` | Read-only checks/output | None | None |
| `task create` | Creates task files and a local branch ref | Fetches the Git base branch | None |
| `task status`, `storage status` | Read-only report | None | None |
| `storage set`, `storage reset`, `move` | Changes local content/placement metadata | None | None |
| `storage hydrate` | Materializes S3 content | S3 | None |
| `plan` | Creates ignored/private preview state | Fetches Git refs | None |
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
