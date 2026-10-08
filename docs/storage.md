# Native repository storage

workspace-mgr needs to preserve repository data across Git publication, local
materialization and S3 history. It does not need a pipeline runner, experiments,
a DVC command interface, or a second remote-configuration authority.

## Responsibilities

Git retains small files, task control metadata, storage placement decisions,
and versioned storage manifests. S3 retains opaque file bytes at
repository-relative object paths with exact object versions. The local cache
is disposable; deleting it cannot delete a remote object or change a manifest.
The engine checks paths, sizes, checksums, object identity and transaction
ownership. It never decides whether task payload contents are correct.

The existing publication, review, cloud-usage, archive and cancellation policies
remain in the repository control layer. Rust storage operations receive typed
requests rather than simulated external command-line arguments.

## One repository management operation

Run `workspace-mgr manage --repo .` to establish a new repository or reconcile an
existing one with this release. `--s3-url` and `--s3-endpoint-url` configure the
public location when needed. `manage --dry-run` inventories changes first.

Management combines storage-format migration and scaffold updates in one
recoverable repository transaction. It reports converted manifests and removed
legacy control files. It leaves changes available for normal Git review; it
does not rewrite Git history. Exact-version imports change metadata only;
ordinary CAS imports upload and verify new native S3 versions.

`.workspace-mgr.toml` is the sole public source of remote facts. Native clients
read it directly. Authentication uses AWS environment/profile mechanisms or the
ignored `.workspace-mgr/local/credentials.toml`. Cached content and retry state
also live below `.workspace-mgr/local/`, shared by linked worktrees.

## Native manifests

A sidecar named `model.bin.wm-storage.json` addresses the adjacent `model.bin`.
Each manifest describes exactly one file or directory boundary and carries a
strict `schema_version`. File manifests record an explicit checksum algorithm,
physical size, and optional exact remote version binding.

New native recordings use MD5 over physical bytes. An imported normalized-text
binding is retained only when its exact-version cache proves the raw bytes are
unchanged; otherwise recording uses a raw checksum and requires a new binding.

```json
{
  "schema_version": 1,
  "path": "model.bin",
  "kind": "file",
  "checksum": {
    "algorithm": "md5",
    "digest": "0cc175b9c0f1b6a831c399e269772661"
  },
  "size": 1,
  "version": {
    "id": "exact-object-version",
    "etag": "remote-etag"
  }
}
```

A directory manifest contains a complete `entries` list. Each entry records its
relative path, checksum, size and optional version. The aggregate identity
covers sorted content descriptions, including sizes; it excludes remote
version bindings, so an archive copy does not change content identity.
Directories cannot contain overlapping file paths. Unknown fields, unknown
versions, ambiguous paths, duplicate entries and inconsistent aggregates fail.

MD5 is retained for existing content identities and cache reuse. Imported
legacy text hashes explicitly declare `md5-dos2unix`; physical byte sizes are
still verified. Changing the implementation language does not change imported
file bytes or checksum semantics.

Repositories using the native format declare `minimum_cli_version = "0.8.0"`
or a higher existing requirement. Older releases must refuse them before
interpreting or publishing unfamiliar control metadata.

## Migrating legacy DVC repositories

`manage` detects legacy `.dvc` sidecars and configuration automatically. The
importer verifies every source before converting anything, preserves file
checksums and version bindings, creates native manifests, carries reusable
cache bytes and supported private credentials into the native local directory,
and removes the converted legacy pointers and obsolete managed DVC controls.
Unrelated `.gitattributes` rules and repository ignore rules are retained.
Legacy adoption runs in the primary shared checkout, including configuration or
cache-only adoption. Native scaffold reconciliation also supports linked worktrees.

Path-based, version-aware S3 metadata imports without transferring objects when
every file already has an exact VersionId. Ordinary DVC S3 remotes instead store
objects by content identity. `manage` supports the DVC 3 `files/md5/<digest>`
layout and the DVC 2 `<digest>` layout, with their split digest directories and
recorded checksum algorithms. It can read a directory's `.dir` listing directly
from S3 when the working payload and cache are absent. It verifies each listed
object's physical size and checksum, then copies its opaque bytes to the native
repository-relative object path. The target bucket must have versioning enabled
and support conditional object writes. New paths use `If-None-Match: *`; a
replanned upload over a verified owned target uses its latest ETag with
`If-Match`. Foreign objects are never overwritten.
Every native binding records a verified exact VersionId and ETag.

`--dry-run` may read remote object metadata and directory listings to produce an
inventory of source and destination keys and transfer bytes. It writes no cache,
repository files, Git state or S3 objects. Actual adoption downloads and uploads
CAS payloads through private local state without materializing missing working
payloads. It retains the original CAS objects so historical Git revisions can
still hydrate them. Source reads use a recorded VersionId or an ETag condition;
an ambiguous source, changed identity, wrong checksum or existing foreign target
object refuses migration instead of guessing or overwriting it.

An incomplete directory manifest, unsupported pipeline, custom setting,
conflicting destination or unsafe path is reported before local conversion.
Legacy sidecars and controls remain until all transfers and exact-version
verification succeed. The private `storage-import.json` journal and owned upload
journals let the next `manage` resume an interrupted transfer, including a lost
upload response, without creating a second owned version. A changed source or
later edit to a planned control file blocks recovery for explicit resolution.
CAS upload receipts also retain a raw SHA-256 identity, verified by reading the
exact uploaded version. Imported normalized-text bindings use a cache isolated
by exact object version, so matching normalized checksums cannot swap raw bytes.
Finish or cancel any pending archive, upload or purge operation before migration.

Publish the complete migration result together: every converted manifest, its
legacy sidecar deletion, obsolete control-file deletions and the updated public
configuration. Include these paths in the same infrastructure task's scopes.
Publication refuses to remove legacy routing controls while its proposed Git
tree still contains legacy pointers that need them.

To abandon a failed CAS import before repairing metadata or remote state, run
`manage --cancel-migration` (`--dry-run` previews it). This clears the pending
import plan while retaining legacy controls, source objects, newly uploaded
versions, reusable cache and upload receipts. It never deletes remote data.
After the repair, ordinary `manage` builds a fresh plan and can reuse verified
owned uploads when they match that plan.

Retained legacy CAS objects belong to the shared historical content store.
They are not native logical object paths and do not enter native archive or
prefix retirement inventories. After migration, `remove` or `untrack` can
permanently retire the selected native paths and their versions, but does not
automatically delete the original CAS sources that older Git snapshots may
still need. Existing path-based exact-version retirement keeps its established
semantics.

Old Git commits retain their original metadata. A confined legacy reader keeps
historical hydration and usage accounting working; ordinary new writes use the
native format. Archive receipts, version mappings and pending journals keep
their established transaction identities. Migrating an active archive attempt
must not invalidate its rollback evidence.
