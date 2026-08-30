# Architecture and transaction guarantees

## State boundaries

`workspace-mgr` separates fixed product policy, tracked repository facts,
scoped task state, and private runtime state. `.workspace-mgr.toml` contains
only non-secret Git and optional S3 locations. Task manifests contain identity, purpose,
scope, and branch state. Deliverable manifests are tracked inside their task
directories; infrastructure manifests and isolated worktrees live below the
Git common directory. Private indexes and locks also live there. All
mutating repository, placement, publication, hydration, and refresh operations
share a repository lock; task and storage-boundary locks add narrower
diagnostics.

Scaffold ownership is structural. In a repository established by
`.workspace-mgr.toml`, `AGENTS.md`, `.dvc/config`, `.dvc/.gitignore`, and
`.dvcignore` have fixed roles: the TOML file is the user-editable source of
Git/S3 facts, while the other four are whole-file generated paths owned by the
product and reconciled by `init`, regardless of their prior content.
`.workspace-mgr/instructions/repository.md` remains repository-owned content.
Shared aggregate files such as `.gitattributes` keep unrelated repository
content while the product enforces only its required rules. Before first
initialization, existing reserved scaffold paths are reported as collisions
rather than classified from their contents.

The task layout, branch prefix, shared-checkout behavior, semantic storage
model, 1 MiB S3 recommendation, 10 MiB automatic threshold, instruction set,
pull-request ownership, and merge authority are compiled product policy. They
are intentionally absent from repository configuration so different
repositories cannot drift into different management strategies.

An explicit storage choice is recorded beside its path as workspace-mgr-owned
metadata. This makes the choice reviewable and keeps independently active task
scopes from contending on one central placement file. Users must not edit these
sidecars or the generated S3 metadata directly.

A directory choice is one recursive placement boundary. Descendants inherit
the explicit or published directory placement, and nested boundaries are
rejected so one path never has competing owners. Status enumeration reports a
directory boundary once while still listing ordinary Git files elsewhere in the
scope.

## Placement lifecycle

Content has one public placement: Git or S3.

- Git represents collaboration/control-plane history; S3 represents
  artifact/data-plane object history. An explicit choice and reason carry the
  semantic decision, while size is only the fallback for unclassified new
  files.
- `storage status` explains the effective target, boundary, basis, payload
  metrics, semantic reason, and warnings.
- `storage set` records an explicit local choice.
- `storage reset` removes that choice and reapplies automatic policy.
- `move` preserves placement while changing a path.
- `storage hydrate` reads exact S3 content into the working tree.

These commands do not publish. Automatic policy is evaluated during `plan` and
`publish`; existing published content is not silently moved because its size
changed. An automatic candidate below 1 MiB uses Git without routine warning;
1–10 MiB uses Git with semantic-review feedback; above 10 MiB uses S3. Explicit
S3 below 1 MiB remains valid but reports an efficiency warning based on the
aggregate materialized boundary size.

## Scoped publication

For a task publication, the CLI:

1. resolves the task and explicitly authorized scopes;
2. fetches the configured base and target branches;
3. verifies that an existing target branch belongs to the same task identity;
4. evaluates placement and acquires task and storage-boundary locks;
5. reconciles S3 metadata, uploads all live in-scope objects, and verifies them;
6. builds a private Git index from the target branch, or the base branch when no
   target exists;
7. stages only declared scopes and rejects gitlinks, invalid placement, and
   whitespace errors;
8. creates a commit with its task identity, updates the local target ref with compare-and-swap
   semantics, pushes an explicit refspec, and verifies the remote object ID.

Deliverable target refs remain unmounted, so publication never changes the
shared checkout. An infrastructure target ref is mounted only in its dedicated
worktree; after the ref update, the CLI synchronizes that worktree's index to
the published tree without touching its files or any shared checkout.

The Git commit is the publication point for the combined transaction. A later
Git error may leave an unreferenced S3 object version, but a published Git
revision must never reference missing S3 content. Publication never switches or
rewrites the shared checkout's working files.

## Private storage adapter

The S3 adapter currently uses DVC 3.67.1 internally. S3 remotes require exact
object-version metadata and existence checks through an embedded verifier using
the same exact DVC release. This is a maintainer compatibility boundary, not a
public command or repository concept. A filesystem remote is compiled only by
the `test-storage` feature for isolated tests and uses remote-presence
verification. Release builds reject it in the public S3 schema.

Moving an S3 boundary clears path-bound cloud metadata before upload so the new
object path receives and records its own version ID. Existing remote versions
are retained.

## Shared-checkout refresh

`refresh` requires the configured shared branch and a clean shared Git index. It
verifies a fast-forward, prefetches incoming S3 revisions, compare-and-swap
updates the local branch ref, resets the index, materializes ordinary Git paths
whose prior working state was clean or absent, and hydrates stored content.
Existing working-tree overlays are preserved. A failure after the ref update
rolls back the ref, index, ordinary files, metadata, and outputs created by the
refresh.
