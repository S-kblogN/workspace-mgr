# Repository management model

`workspace-mgr` treats repository work as a set of reviewable intentions, not
as a collection of ad hoc file and Git commands. Its job is to preserve the
relationship between why a change exists, which paths belong to it, where its
content is stored, and how it becomes visible to reviewers.

This model is shared by people and coding agents. A person can use it to reason
about repository state; an agent must use it as the context for the concrete
rules that follow it in `workspace-mgr instructions`.

## One writable conversation is one reviewable task

For work that changes a repository, the central relationship is:

```text
one writable conversation (chat) = one task = one target branch = one draft pull request
```

These are four views of the same reviewable intention:

- The **conversation** contains the request, decisions, and authorization.
- The **task** is its durable repository representation: purpose, owned paths,
  working files, and publication state.
- The **target branch** is the task's publication lane. It can advance without
  switching the shared checkout away from its shared branch.
- The **draft pull request** is the task's review and merge record. Its title and
  description should continue to describe the same intention as the task.

This is the default review model. A repository may explicitly disable the draft
pull-request requirement; that changes only the hosting review surface. The
conversation, task, and target branch remain one-to-one, and the effective
repository instructions state whether a pull request is required.

A managed task has a dedicated directory, a concise README, and a manifest.
The README explains what the task is producing; the manifest defines the paths
the task owns and the branch to which it publishes. Paths outside that declared
scope require an explicit reason, so a narrow request cannot silently become a
repository-wide change.

`task create` establishes the local task, scope, and target-branch identity;
it does not publish a remote branch or create a pull request. `task status`
resolves that identity and reports its current scoped state without publishing.

Continue using the same task, branch, and pull request while pursuing the same
intention. Start a new set when work should be reviewed or merged independently.
Do not share one active task between unrelated conversations, and do not place
unrelated changes into the same pull request. A read-only conversation creates
none of these objects because it has no repository change to publish.

A repository-infrastructure change follows the same one-task/one-branch/one-PR
relationship. Its declared scope contains shared policy, entrypoints, CI, or
other repository-wide mechanisms, and its pull request remains isolated from
ordinary deliverable content. Infrastructure is a kind of task, not a bypass
around task ownership.

`workspace-mgr` publishes the target branch but deliberately does not create,
edit, approve, or merge a pull request through a hosting provider. The user or
agent maintains the corresponding draft pull request through the repository's
hosting workflow, and merging remains an explicit reviewer or maintainer action.

## Scope and placement answer different questions

Every retained path has two independent properties:

1. **Task scope** answers which reviewable intention owns the path.
2. **Storage placement** answers where the retained bytes live: Git or S3.

Putting content in S3 does not remove it from the task, and putting content in
Git does not broaden the task's scope. The task manifest controls ownership;
placement controls storage.

Choose **Git** when content should be present directly in repository history,
ordinary clones, and diffs. Choose **S3** when content is bulky, binary, or best
retrieved as versioned data. Both are first-class choices. File size supplies
an automatic default for new content, not an absolute rule: an explicit choice
may put a large reviewable artifact in Git or a small data artifact in S3.

Published placement is stable. A file does not silently move between Git and S3
merely because its size changes. An explicit placement remains in force until
it is reset. A directory placed in S3 is one recursive boundary whose
descendants inherit that placement; overlapping placement boundaries are
rejected so that every path has one owner and one answer.

Placement operations express local intent:

- `storage status` explains the effective placement and why it was selected.
- `storage set` records an explicit Git or S3 choice.
- `storage reset` removes that explicit choice and returns to repository policy.
- `move` changes a path while preserving its placement.
- `storage hydrate` reads exact S3 content into the working tree.

None of those operations publishes a branch or uploads a new task state.
Placement and publication are separate on purpose.

## Publication is the remote visibility boundary

Files in the working tree and placement choices are proposed local state.
`plan` explains the exact task scope, placement decisions, and Git changes that
would be published. It may inspect remote state, but it does not upload task
content, create a revision, or advance a remote branch.

`publish` is the only repository command that writes a task state to remotes.
For S3-placed content, it uploads and verifies the exact object versions first.
It then constructs a Git commit from only the declared task scopes, advances
the task's target branch, and verifies the remote revision. The Git revision is
the publication point for the combined state: a published branch must never
refer to missing S3 content.

Publication makes the branch ready for review; it does not merge it. The draft
pull request is the human-visible review surface for that branch. A final
no-change plan, matching local and remote branch revisions, and current pull
request metadata together establish that the task is fully synchronized.

If Git publication fails after S3 upload, an unreferenced S3 object version may
remain, but no remote Git revision should point to missing content. Retrying the
same publication is safe. Automatic deletion of shared remote history is not
part of normal repository management.

## A shared checkout is a coordination surface

In the shared-checkout profile, the checkout stays on its shared branch while
different tasks may have independent working-tree overlays. A task publishes
to its target branch without checking that branch out and without staging
unrelated paths in the shared Git index.

This means an unrelated untracked or modified path can be valid state owned by
another active task. Broad stash, clean, reset, or deletion operations would
erase that ownership boundary and must not be used as routine synchronization.

After a task is merged, `refresh` safely advances the shared branch, preserves
unrelated overlays, and materializes incoming Git and S3 content. Refresh is an
inbound synchronization operation; it does not publish a task.

## The lifecycle as one story

The complete repository lifecycle is:

1. `setup` provisions the CLI's private host runtime.
2. `init` establishes tracked repository policy and the thin agent bootstrap.
3. `config show` reports effective policy, while `doctor` validates the CLI,
   repository, Git, and storage environment; neither changes repository state.
4. `instructions` loads this model followed by the repository's effective rules.
5. `task create` begins one writable, reviewable intention.
6. Work is kept inside its declared scope, with retained content placed in Git
   or S3.
7. `plan` explains the proposed remote state without publishing it.
8. `publish` verifies stored content and advances only the task branch.
9. The matching draft pull request carries review; a reviewer or maintainer
   decides when to merge it.
10. `refresh` safely brings a merged result into the shared checkout.

The resulting boundary is deliberate: users and agents think in terms of
repository policy, tasks, scope, Git/S3 placement, publication, review, and
refresh. `workspace-mgr` owns the lower-level repository and storage mechanics
needed to preserve those meanings.
