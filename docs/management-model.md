# How this workspace works

## Mental model

This repository is a durable workspace for conversations between a user and
coding agents. The chat is the user-facing interface: the user asks for outcomes
and treats the agent as a general-purpose collaborator. workspace-mgr supplies
the control plane for doing that work in a shared repository.

```text
one writable conversation (chat) = one task = one target branch = one draft pull request
```

A task groups one work item's identity, current name, authorized paths, branch,
review association and storage state. Deliverable tasks have timestamped
repository directories; infrastructure tasks have explicit shared-path scopes
and private manifests. Infrastructure is a kind of task, not an ownership bypass.
The current slug is a mutable topic label; task ID and review branch remain
stable when its directory changes.

Task scope answers which task may mutate a path. Storage placement answers where
its retained bytes are transported: Git, versioned S3, or explicit local-only
state. Neither changes the other. The shared checkout remains on its configured
main branch; tasks publish through private indexes to unmounted task branches.
Several chats can therefore retain independent overlays without switching the
checkout or staging one another's files.

A task README describes its purpose and provides the repository entrypoint.
Task contents belong to the user. workspace-mgr manages repository control,
publication and byte transport; it does not certify payload correctness or
runtime usability. Operation-specific behavior and consequences are explained by that command's
help and relevant execution output.

## Session-wide constraints

Reading and ownership are separate. Reading a path does not transfer ownership
or authorize mutation. Repository-wide reading is allowed for context,
including another chat's task directory. A deliverable task's default write
boundary is its own task directory. Shared or additional paths require explicit
user authorization for the exact path and action; scope declarations record
that authorization and do not manufacture it. Infrastructure manifest scopes
are its write boundary. Untracked does not mean unowned.

Use workspace-mgr for managed lifecycle, placement and publication operations.
Do not bypass a refusal with lower-level Git or object-store mutation commands.
Preserve other tasks' staged, modified and untracked overlays. Do not hand-edit
product-owned control files or private state in `.workspace-mgr/local/`.

The user controls cloud-usage approval, CLI installation and updates, merge and
other PR state transitions. The agent owns the task's draft PR and must reconcile
its authorized local, remote and review state before every writable-task turn
ends. A blocker is reported as exact unsynchronized state. Repository-management
requirements still apply; their operational details are loaded at the relevant
command, rather than repeated throughout every session.

## Find the next operation

Read the relevant command's `--help` before an operation. It gives that
operation's prerequisites, retained repository policies, scope rules, safety
consequences and next steps. Command output reports facts and guidance that
only become relevant once the operation runs.

| Need | Entry point |
| --- | --- |
| Start a writable repository task | `workspace-mgr task create --help` |
| Find an existing task or current directory | `workspace-mgr task list --help`, `task path --help`, `task show --help` |
| Inspect resolved task state | `workspace-mgr task status --help` |
| Rename, adopt or upgrade current task metadata | `workspace-mgr task rename --help`, `task adopt --help`, `task upgrade --help` |
| Choose or inspect placement and retrieve bytes | `workspace-mgr storage --help` and the relevant leaf command |
| Preview and publish one task | `workspace-mgr plan --help`, `publish --help` |
| Respond to a measured resource decision | `workspace-mgr task approve-cloud-usage --help` and the blocking report |
| Move, remove or stop publishing selected paths | `workspace-mgr move --help`, `remove --help`, `untrack --help` |
| Group closed-PR tasks or cancel an unpublished attempt | `workspace-mgr archive --help` |
| Explicitly abandon an unmerged task | `workspace-mgr task discard --help` |
| Synchronize the shared checkout after merge | `workspace-mgr refresh --help` |
| Install dependencies, adopt or update a repository, diagnose control state | `workspace-mgr setup --help`, `manage --help`, `doctor --help`, `config show --help` |

Default `instructions` and explicit `instructions all` return this mental model,
operation directory and session-wide constraints. `instructions model` returns
only this document. Existing detailed topics remain available on demand for
compatibility; they are not appended to the default output.

Repository-owned `.workspace-mgr/instructions/repository.md` remains additional
user guidance. When present, default output points to it; read the file directly
or use `workspace-mgr instructions repository` before task work. This on-demand
view reproduces its text rather than mixing it into every global instruction
response.
