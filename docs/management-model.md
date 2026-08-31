# How this workspace works

## What this repository is for

This repository is a durable workspace for conversations between a user and
coding agents. `workspace-mgr` exists to make that kind of workspace practical:
it lets the user treat a coding agent as a general-purpose collaborator, not
only as a tool for editing an existing software project.

The user can start a chat and ask for any kind of work the agent can perform:
research a question, compare products, study a paper, write a report, analyze
data, prepare media, build software, organize records, or combine several of
those activities. The chat is the user-facing interface. The repository gives
that conversation a place to retain inputs, working materials, outputs, and
reproducibility evidence when the result should outlive the chat.

The user should normally describe the desired outcome rather than plan task
directories, branches, commits, or storage mechanics. The agent translates the
request into repository operations and reports the resulting task, artifacts,
and review state. The user may still make repository-level choices when they
matter, such as explicitly asking for an artifact to be stored in Git or S3,
requesting a shared repository change, or deciding when a pull request should
be merged.

`workspace-mgr` is the management interface behind this workspace. It gives the
agent a consistent way to create a task, bound its scope, place retained
content, publish a reviewable result, and coexist with other chats. It is not
the subject of the user's work; it is the mechanism that keeps the workspace
safe and understandable while the agent carries out that work.

The product applies the same management strategy to every initialized
repository. Repositories provide different Git and optional S3 locations, but
they do not select different task layouts, storage thresholds, review models,
or agent responsibilities. This makes behavior portable: a user and agent can
move between managed repositories without relearning their operating contract.

A chat that remains purely conversational or read-only does not need to create
repository state. As soon as a chat needs to create, change, download,
generate, or retain files, it becomes a writable conversation and owns exactly
one task in this workspace.

## From a chat to a task

For repository-writing work, the central relationship is:

```text
one writable conversation (chat) = one task = one target branch = one draft pull request
```

These are four views of one reviewable intention:

- The **conversation** contains the user's goal, decisions, and authorization.
- The **task** is the durable workspace for that conversation.
- The **target branch** is the task's publication lane.
- The **draft pull request** is the task's review and merge record.

An ordinary deliverable task directory holds the conversation's retained
inputs, working files, tools, evidence, and deliverables. Its README explains
the task's current purpose and important outputs; it is not a transcript or
chronological log. Its manifest records the task identity, declared scope, and
target branch. An infrastructure task instead has an isolated worktree and a
private manifest because its content belongs at shared repository paths rather
than inside a timestamped deliverable directory.

The task directory is the default ownership boundary. If the user asks the
agent to change a shared path elsewhere in the repository, the agent records
that additional scope and the reason it was authorized. This makes the user's
request auditable without turning a narrow task into permission to modify
unrelated repository state.

`task create` establishes the local task, README, manifest, scope, and branch
identity. It does not publish a remote branch or create a pull request.
`task status` resolves the current task and reports its scoped state without
publishing. `task rename` changes the current human-readable slug when the
conversation's topic evolves. For a deliverable it moves the complete task
directory, including Git and S3 placement metadata, while preserving the
immutable task ID and target branch. The next normal publication removes the
old remote tree and publishes the new path. For infrastructure it updates the
current slug in private task metadata while its identity-owned worktree remains
stable. `task discard` is the explicit opposite endpoint: after the user
decides that an unmerged task should not be retained and the agent closes or
verifies absence of its pull request, it removes that task's branch and local
workspace instead of publishing or merging it.

The same chat continues using the same task for its lifetime. Continue using
the same branch and pull request when refining that same intention. Start a new
task when work should be reviewed or merged independently. Unrelated chats must
not share a task, branch, or pull request.

The current slug is a mutable topic label; the task ID and target branch are
stable publication identities. This distinction preserves the one-PR contract:
renaming an open pull request's head branch can close that request on hosting
providers. After a task rename, the agent reuses the existing pull request and
updates its title and living description instead of replacing it.

A repository-infrastructure change follows the same relationship. It is still
one task with one branch and one pull request, but `task create --kind
infrastructure` gives it an isolated worktree, private task metadata, no
timestamped repository task directory, and an explicitly declared scope of
shared policy, root entrypoints, CI, or other repository-wide mechanisms.
Infrastructure is a kind of task, not a bypass around task ownership.

The draft-pull-request relationship is the review model for every managed
repository. It is part of the product strategy, not a per-repository option.

## Where task content lives

Task scope answers **which conversation owns a path**. Storage placement answers
**where the retained bytes live**. These are independent properties: putting an
artifact in S3 does not remove it from the task, and putting it in Git does not
broaden the task's scope.

Every retained path has one storage placement:

- **Git is the collaboration and control plane.** It is appropriate when
  content should be present in ordinary clones and its value comes from direct
  review, diff, merge, or joint evolution with repository source.
- **S3 is the artifact and data plane.** It is appropriate when content is
  consumed as an exact object, changes atomically, or should be hydrated on
  demand.

The agent understands why content exists, so it makes the semantic choice when
that intent is clear and records the reason with `storage set`. The CLI does not
guess from filename extensions. A user may explicitly choose either location
at any size, and that instruction takes priority.

Size is a fallback for new content whose semantics have not been selected. A
new boundary below 1 MiB strongly defaults to Git. From 1 through 10 MiB, Git
remains the fallback but the agent is asked to review whether collaboration or
artifact history is actually intended. Above 10 MiB, S3 is the fallback. A
standalone S3 boundary below 1 MiB is normally inefficient because its metadata
and remote operations may outweigh the payload; an explicit selection still
succeeds with a warning. A meaningful directory may instead be selected as one
boundary, whose reported size is the aggregate size of its materialized regular
files. Automatic evaluation treats unclassified files independently; selecting
a directory boundary is an intentional semantic operation and does not promise
that the storage backend packs it into one remote object.

Published placement is stable. Content does not silently move between Git and
S3 merely because its size changes. An explicit placement remains in force
until it is reset. A directory placed in S3 is one recursive boundary whose
descendants inherit that placement; overlapping boundaries are rejected so a
path never has competing placement owners.

Placement operations describe or change local intent:

- `storage status` explains the target, boundary, selection basis, semantic
  reason, payload size/file count, and any structured warning.
- `storage set` records an explicit Git or S3 choice.
- `storage reset` removes that choice and returns the path to fixed policy.
- `move` changes a path while preserving its placement.
- `storage hydrate` retrieves exact S3 content into the local workspace.

None of these operations publishes a task. Placement and publication are
separate so the agent can organize the proposed result before making it visible
remotely.

## From local work to review

Files in the task directory and local placement choices are proposed task
state. `plan` explains what the task would publish: its exact scope, placement
decisions, and Git changes. It may inspect remote state, but it does not upload
task content, create a revision, or advance a remote branch.

`publish` is the remote visibility boundary. It is the only repository command
that writes a task state to remotes. For S3-placed content, it uploads and
verifies the exact object versions first. It then constructs a Git commit from
only the task's declared scopes, advances the target branch, and verifies the
remote revision. The Git revision is the publication point for the combined
state: a published branch must never refer to missing S3 content.

Publishing makes the target branch ready for review; it does not merge it. The
agent maintains the corresponding pull request through the repository's hosting
workflow. `workspace-mgr` does not create, edit, approve, or merge that pull
request through a hosting-provider API. Merging remains an explicit authorized
action.

The agent must query by head branch after the first successful publish, reuse
an existing open pull request or create exactly one, and never create a
duplicate. It owns the title and living description and updates them whenever
the goal, scope, deliverables, validation, or known limitations materially
change. It then verifies the base branch, head branch, review state, and that
the pull-request head revision equals the revision reported by `publish`.
Provider failures are blockers to full synchronization. The agent must not
merge, enable auto-merge, approve, close, or mark the request ready without the
user explicitly authorizing that exact transition. A user request to discard a
specific unmerged task is exact authorization for the agent to close that
task's pull request, verify it is closed, and then run confirmed task cleanup.

A task is fully synchronized when its local and remote branch revisions match,
its pull-request description reflects the same intention, and a final plan
reports no remaining task changes.

If Git publication fails after an S3 upload, an unreferenced S3 object version
may remain, but no remote Git revision should point to missing content.
Retrying the same publication is safe. Automatic deletion of shared remote
history is outside normal workspace management.

Discard is deliberately a two-step destructive operation. Its dry run records
the observed local task ref, remote task ref, and shared-branch revisions in
private confirmation state and reports every local action plus the current S3
version references. Confirmation from the shared checkout is accepted only for
the exact task ID and unchanged revisions. The CLI then deletes the remote task
branch with an exact lease, deletes the local task ref, removes the deliverable
directory or infrastructure worktree, and restores any declared shared paths to
the local shared-branch tree. A remote failure restores quarantined local state.
Merged tasks are refused. Versioned S3 objects are reported as retained orphan
candidates and are never permanently purged by discard.

## How multiple chats share the workspace

The checkout remains on its shared branch while multiple chats may have
independent task directories and working-tree overlays.
A task publishes to its own branch without checking that branch out and without
staging paths owned by other tasks in the shared Git index.

An unrelated untracked or modified path may therefore be valid state owned by
another active conversation. Broad stash, clean, reset, or deletion operations
could erase another task's work and are not routine synchronization tools.

After the user merges a task, `refresh` safely advances the shared branch,
preserves unrelated overlays, and materializes incoming Git and S3 content.
Refresh is inbound synchronization; it does not publish a task.

## The workspace lifecycle

The complete story is:

1. A repository owner uses `setup` and `init` to establish the managed
   workspace, tracked repository facts, and thin agent bootstrap.
2. The user starts a chat and describes the outcome they want.
3. The agent loads `instructions` and decides whether the conversation is
   read-only or needs a writable task.
4. For writable work, `task create` gives the chat one durable workspace,
   scope, branch identity, and eventual pull-request identity.
5. The agent performs the requested work inside that task and keeps its README
   aligned with the current purpose and outputs. If the topic changes, `task
   rename` updates its current slug without replacing its branch or review.
6. Retained artifacts are placed in Git or S3 automatically or by an explicit
   user or agent choice.
7. `plan` explains the proposed reviewable state without publishing it.
8. `publish` verifies stored content and advances only the task's target branch.
9. The matching draft pull request carries review, and the user or maintainer
   decides whether to merge it or explicitly abandon the task.
10. After merge, `refresh` brings the result into the shared workspace without
    disturbing other active chats. After abandonment, the agent closes the
    unmerged pull request and `task discard` removes the task workspace and
    branch without purging retained S3 history.

`config show` reports the repository's Git and S3 facts, and `doctor` diagnoses
the CLI, repository, Git, and storage environment without changing repository
state.

The intended division of responsibility is simple: the user asks for outcomes,
the agent performs the work inside one task, and `workspace-mgr` preserves the
workspace boundaries that make the result durable, reviewable, and safe to
combine with other conversations.
