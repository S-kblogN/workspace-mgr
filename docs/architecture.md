# Architecture and transaction guarantees

## Boundaries

`workspace-mgr` separates four kinds of state:

1. Package defaults are compiled into the released CLI.
2. Repository policy lives in tracked `.workspace-mgr.toml`.
3. A task manifest records only task-specific scope and branch state.
4. Runtime indexes and locks live below the Git common directory and are never
   staged.

Repository configuration may name a publication remote and a non-secret storage
URL, but the binary contains no repository URL, bucket name, or credential.
Storage credentials remain in environment or platform identity mechanisms.
`.workspace-mgr.toml` is authoritative; engine-specific configuration is a
deterministic derived file owned and drift-checked by `workspace-mgr`.

## Scoped publication

For a task publication, the CLI:

1. Resolves and validates the declared task and additional scopes.
2. Acquires task and managed-storage-boundary locks.
3. Fetches the configured base and target branches.
4. Reconciles and verifies in-scope managed-storage metadata.
5. Builds a private Git index from the target branch, or the base branch when
   the target does not yet exist.
6. Stages only declared scopes and rejects gitlinks, unexpected large files,
   whitespace errors, and unexplained storage-metadata deletion.
7. Creates a commit with `git commit-tree`.
8. Updates the unmounted local target ref with compare-and-swap semantics.
9. Pushes an explicit commit-to-ref refspec and verifies the remote object ID.

The shared checkout's branch and working files are not switched by publication.
Pull-request creation and metadata updates remain a repository-hosting concern;
the generated instructions tell agents when a draft review is required, while
the core transaction engine stays independent of a specific hosting provider.

## Managed-storage ordering

The repository commit is the publication point for a combined metadata and
storage transaction. When storage is enabled, dirty metadata is reconciled, all
live in-scope objects are pushed, and the remote is verified before a repository
commit can be published. A later publication error may leave an unreferenced
remote object version, but the CLI must not publish a commit that references
missing stored data.

The current private storage adapter is DVC 3.67.1. Standard remotes use its cloud
status; object-versioned remotes require exact version metadata and existence
checks. The Rust process invokes a small embedded adapter through a Python
interpreter importing that same exact release. Pinning both surfaces protects
the internal API boundary while preserving DVC's remote resolution and version
semantics without embedding a provider-specific bucket implementation in the
core CLI.
When a boundary moves, path-bound cloud metadata from the old location is
cleared before the push so DVC uploads the new object path and records its new
version ID. Existing remote versions are retained.

## Shared-checkout refresh

Refresh requires the configured shared branch and a clean shared Git index. It
fetches the remote branch, verifies a fast-forward, prefetches incoming stored
revisions, compare-and-swap updates the local branch ref, resets only the index,
then materializes changed storage metadata and outputs. Existing working-tree
overlays are preserved. A failure after the ref update triggers rollback of the
ref, index, storage metadata, and outputs created by that refresh.
