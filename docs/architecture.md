# Architecture and transaction guarantees

## Boundaries

`workspace-mgr` separates four kinds of state:

1. Package defaults are compiled into the released CLI.
2. Repository policy lives in tracked `.workspace-mgr.toml`.
3. A task manifest records only task-specific scope and branch state.
4. Runtime indexes and locks live below the Git common directory and are never
   staged.

Repository configuration may name a Git remote or DVC remote, but the binary
contains no repository URL, bucket name, or credential. DVC credentials remain
in DVC local configuration or the environment.

## Scoped Git publication

For a task publication, the CLI:

1. Resolves and validates the declared task and additional scopes.
2. Acquires task and DVC-boundary locks.
3. Fetches the configured base and target branches.
4. Reconciles and verifies in-scope DVC stages.
5. Builds a private Git index from the target branch, or the base branch when
   the target does not yet exist.
6. Stages only declared scopes and rejects gitlinks, unexpected large files,
   whitespace errors, and unexplained DVC pointer deletion.
7. Creates a commit with `git commit-tree`.
8. Updates the unmounted local target ref with compare-and-swap semantics.
9. Pushes an explicit commit-to-ref refspec and verifies the remote object ID.

The shared checkout's branch and working files are not switched by publication.
Pull-request creation and metadata updates remain a repository-hosting concern;
the generated instructions tell agents when a draft review is required, while
the core transaction engine stays independent of a specific hosting provider.

## DVC ordering

The Git commit is the publication point for a Git+DVC transaction. When DVC is
enabled, dirty metadata is committed, all live in-scope stages are pushed, and
the remote is verified before a Git commit can be published. A later Git error
may leave an unreferenced remote DVC version, but the CLI must not publish a Git
commit that references missing DVC data.

Standard remotes are verified with DVC's cloud status. Version-aware remotes
require exact version metadata and existence checks. The Rust process invokes a
small embedded adapter through a Python interpreter that can import the same
DVC installation; this preserves DVC's remote resolution and version semantics
without embedding a provider-specific bucket implementation in the core CLI.
When a boundary moves, path-bound cloud metadata from the old location is
cleared before the push so DVC uploads the new object path and records its new
version ID. Existing remote versions are retained.

## Shared-checkout refresh

Refresh requires the configured shared branch and a clean shared Git index. It
fetches the remote branch, verifies a fast-forward, prefetches incoming DVC
revisions, compare-and-swap updates the local branch ref, resets only the index,
then materializes changed DVC metadata and outputs. Existing working-tree
overlays are preserved. A failure after the ref update triggers rollback of the
ref, index, DVC metadata, and outputs created by that refresh.
