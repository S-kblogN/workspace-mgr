# Architecture and transaction guarantees

## State boundaries

`workspace-mgr` separates fixed product policy, tracked repository facts,
scoped task state, and private runtime state. `.workspace-mgr.toml` contains
only non-secret Git and optional S3 locations, plus the `minimum_cli_version`
compatibility declaration that `workspace-mgr` maintains itself. Task manifests
contain identity, purpose, current slug, scope, and branch state and, from
schema 3, the user's cloud-usage approval and, from schema 4, durable archive
review provenance. The task ID and review branch are
immutable; the slug and deliverable path may change together. Deliverable
manifests are tracked inside their task directories; infrastructure manifests
live below the primary checkout's `.workspace-mgr/local/` and are selected
explicitly with `--manifest`. Both kinds work in the shared checkout on the
configured base branch and publish to unmounted task refs. Private indexes and
locks also live in `.workspace-mgr/local/`. All linked worktrees resolve that
same directory in the primary checkout rather than each keeping an independent
copy, so repository locks still exclude operations across worktrees.
All mutating repository, placement, publication, hydration, and refresh
operations share `.workspace-mgr/local/repository.lock`; task locks under
`state/<hash>/` and storage-boundary locks under `storage-locks/` add narrower
diagnostics.

An infrastructure manifest is stored at
`<primary-checkout>/.workspace-mgr/local/infrastructure-tasks/<id>/.workspace-mgr-infrastructure.toml`.
Creation returns the shared repository root as `path` and the absolute manifest
path as `manifest`. It creates no worktree or checkout transition. Existing
legacy infrastructure worktrees are not migrated automatically.
Creation requires the shared HEAD to equal the fetched base revision; a behind
checkout must refresh first.

The generated root `.gitignore` includes `/.workspace-mgr/local/`; it does not
ignore all of `.workspace-mgr/`, because repository-owned configuration and
instruction modules remain tracked. The private directory also holds archive
retry journals, pending S3 cleanup, discard confirmation and backup state,
and caches. This state is not all disposable: deleting it can lose an
infrastructure task or the records needed to finish a pending transaction.

The primary checkout is the original checkout identified by Git's worktree
metadata, independent of which branch it currently has checked out. It is
usually the shared main checkout. A linked worktree never anchors private
state in its own root. For repositories created with `git init --separate-git-dir`,
Git's common directory does not identify the original
checkout; set Git's `core.worktree` to that checkout's absolute path:

```sh
git config core.worktree /absolute/path/to/primary-checkout
```

This setting records a Git checkout location, not a configurable product-state
directory. Without it, a separate Git directory is refused rather than resolved
from whichever worktree invoked the command. A bare repository or an unavailable
primary checkout is also refused rather than assigned a second private state
directory.

On access, the CLI automatically migrates existing private state from
`<git-common-dir>/workspace-mgr` into the primary checkout's
`.workspace-mgr/local/`, preserving its contents. Old infrastructure manifest
paths supplied with `--manifest` continue to select the migrated manifest.
Migration refuses a lock held by an older process or a conflicting destination
path instead of combining divergent state. It does not leave a symlink at the
old location. Upgrade the CLI used by every linked worktree and stop older
processes before migration. Older binaries still use the previous directory
and cannot share the new repository lock; running old and new CLIs in parallel
is unsupported.

The user's cloud-usage approval is task state. `task approve-cloud-usage`
writes it into the task manifest's `[cloud_usage_approval]` table, which makes
the manifest at least schema 3, so a deliverable publishes the approval with
the task and reviewers see it in the pull request. An infrastructure manifest
keeps it private. Every publication commit also carries a `Cloud-Usage-Approval`
trailer while the manifest records an approval; the trailer is written for
review only and is never read back. The usage gate, the reminder, and
`task status` read the approval from the manifest alone. Each task's private
state directory, keyed by its immutable task ID and branch, holds
`cloud-usage.json` only while a measurement waits for the user's decision: the
file records that pending decision and is removed when none remains. It
survives rename and is deleted by confirmed discard. A disposable
`cloud-usage-cache.json` beside it holds results derived from immutable Git
objects and is recomputed when missing or unreadable.

Scaffold ownership is structural. In a repository established by
`.workspace-mgr.toml`, `AGENTS.md`, and the root `.gitignore` have fixed roles:
the TOML file is the user-editable source of Git/S3 facts; the other two are
whole-file generated paths reconciled by `manage`. Native storage reads that
TOML directly and creates no duplicate remote configuration. Ownership is
structural for all but one: the root `.gitignore` exists in most repositories
before the product does, so it is claimed by the generated header the product
writes rather than by its path. During adoption, `manage` preserves a file
without that header in `.workspace-mgr/repository.gitignore` and creates the
generated root file in the same transaction. An incompatible existing module
refuses adoption. Git has no include directive, so the root ignore file is generated
rather than merged: it carries the product's fixed rules, imports
`.workspace-mgr/repository.gitignore` verbatim, and preserves any well-formed
managed local-only block the file already holds.
`.workspace-mgr/instructions/repository.md` and
`.workspace-mgr/repository.gitignore` remain repository-owned content.
Shared aggregate files such as `.gitattributes` keep unrelated repository
content while the product enforces only its required rules. Before first
initialization, existing reserved scaffold paths are reported as collisions
rather than classified from their contents.

The task layout, branch prefix, shared-checkout behavior, semantic storage
model, 1 MiB S3 recommendation, 10 MiB automatic threshold, 1 GiB per-task
cloud-usage threshold, instruction set, pull-request ownership, and merge
authority are compiled product policy. They
are intentionally absent from repository configuration so different
repositories cannot drift into different management strategies.

## Information routing

Default and explicit `instructions all` load a short mental model, operation
index and session-wide constraints, plus current repository facts. Detailed
policy remains in `guidance::topic` for explicit compatibility views. Command
help is rendered by `command_guidance::command` and is available without
repository configuration or network access. Conditions and outcomes that only
become known at execution are reported by that operation; relocation success
reminders never appear unconditionally in global instructions.

The user-owned repository instruction module is indexed globally and exposed
through `instructions repository`. Default rendering validates its path, size
and encoding and includes its current bytes in the policy hash without printing
the body. This preserves effective-policy change detection while avoiding a
second global operation manual.

New policy and guidance follow the durable information locality requirement in
[CONTRIBUTING.md](../CONTRIBUTING.md). The routing and retained-policy audit is
[control-plane-audit.md](control-plane-audit.md).

## Repository compatibility

Tracked task state evolves with the CLI. Releases parse task manifests
strictly, so a manifest schema added by a newer release cannot be read by an
older one. Instead of keeping every future schema readable by every old
release, a repository declares the oldest compatible release in
`minimum_cli_version` and older releases fail closed.

- Every configuration load first reads `minimum_cli_version` leniently from the
  raw TOML and refuses a newer requirement before strict parsing, so a
  configuration that also carries fields from a newer release still produces a
  message naming both versions. `doctor` loads the configuration without that
  refusal and reports a `cli-version` check, which also reads the declaration
  at the last fetched base-branch ref without fetching. Commands that fetch
  check the declaration in what they fetched before they change anything,
  because the remote may be ahead of the local checkout: `task create` and
  `task discard` check the base tip, `task rename`, `plan`, and `publish` check
  the base tip and the task-branch tip, and `refresh` checks the incoming
  revision before it changes the ref, index, or files. A `task create --dry-run` fetches a missing
  base commit without moving any ref. `manage` preserves higher existing
  declarations and raises the requirement to at least 0.8.1 when adopting
  native storage.
- Declarations are plain release versions. An installed release meets one by
  semantic-version precedence, and a pre-release also meets a declaration of
  its own release, so a release candidate can operate on the repositories it
  raises.
- Product policy maps each task manifest schema to the oldest release that
  reads it: top-level manifests with schemas 1 and 2 need no declaration, and
  schema 3 needs 0.4.0 and schema 4 completion evidence needs 0.7.0.
  A nested archive task manifest needs at least 0.5.0 regardless
  of its schema; publication uses the higher schema or path requirement.
  Writers use the lowest schema that represents a manifest, so only a task that
  records a cloud-usage approval without a checkpoint produces schema 3, and a
  task retaining a checkpoint stays at schema 4 even after approval reset.
- Publication reconciles the declaration. Each private publication index (the
  preview, the pre-upload validation, and the final index) is scanned for task
  manifests at the top level and in nested archived task directories, excluding
  ordinary nested copies. When `.workspace-mgr.toml` is outside
  the publication's scopes and the task branch's copy is exactly what
  `workspace-mgr` writes there, meaning the blob at the task's fork point with
  the fetched base branch or that configuration rendered canonically with the
  branch's own declaration, the configuration belongs to `workspace-mgr`. If a
  manifest needs more than the fork point declares, the index receives the
  fork point's configuration rendered canonically with the higher of the
  requirement and the fetched base tip's declaration. Otherwise a branch that
  never changed the file keeps the fork point's blob exactly, and a branch
  that raised it earlier withdraws the raise down to the higher of the fork
  point's and the fetched base tip's declaration: to the fork point's blob when
  the base tip declares no more, and otherwise to the fork point's
  configuration rendered with the base tip's declaration. So a branch that
  never needs a newer release never touches the file, branches raised for the
  same requirements produce the same content, a branch raised earlier follows
  a base branch raised further, and a branch whose manifests no longer need
  its raise withdraws it without ever lowering a declaration the base branch
  carries, which a rebase merge would otherwise replay onto the base branch;
  hosting merges stay clean as long as every raised branch is published after the base
  branch's last raise. When the configuration is inside the scopes, or the
  branch's copy differs from the fork point in anything else, even comments or
  formatting only, the staged file is the user's: its declaration is only ever
  raised, to the highest of the requirement and the fetched base tip's
  declaration. The reconciled path passes the scope check as a
  workspace-mgr-managed change and counts as a control file for cleanup-only
  measurement. When the published declaration differs from the task-branch
  tip's, the report's `repository_requirement` names the `raise`, `follow`, or
  `withdraw` and the commit carries a `Workspace-Requirement` trailer. A merged
  declaration is never lowered. A build refuses to publish a raise that it does not meet
  itself, before placement or upload, and a tree without `.workspace-mgr.toml`
  cannot be raised, so such a publication is refused. The refusal offers
  removing the approval only when it clears the task's own schema requirement;
  archived-path requirements and another task's requirements, such as one
  merged on the base branch, need an update. Infrastructure manifests are
  private and never trigger a raise, but an infrastructure publication raises
  the declaration when base content it publishes needs one. The reconciled
  configuration exists only in the publication tree until merged refresh,
  unless the task explicitly changes that shared path within its scopes.
- Releases up to 0.3.0 do not know the key. Once a raised configuration is
  merged, they reject it as an unknown field, which also fails closed.
- Another task's published manifest is read only for the identity fields that
  placement history needs, so its newer optional fields never break a
  repository-wide scan.

## Update observation boundary

Update discovery is advisory and user-scoped, not repository configuration.
Arguments are parsed before update discovery. Help, argument errors and offline
`task list`, `task path` and `task show` skip the update cache and network check.
Other invocations, including explicit `--version`, read a small cache in the
user's cache directory. A successful crates.io result is reused for six hours
and a failed attempt for one hour. Refresh uses a nonblocking process lock, a 750 ms
request deadline, a 1 MiB response limit, and an atomic cache replacement. Lock
contention, unavailable cache storage, malformed responses, and all network
failures are ignored.

The cache records only timestamps and the newest non-yanked stable and overall
versions. Stable installations compare against the stable entry; prerelease
installations compare against the overall entry using semantic-version
precedence. A newer candidate emits one stderr line per invocation. The update
checker never writes stdout, changes the requested command's status, downloads
an executable, mutates a repository, or changes product policy.

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

Content has one placement: Git, S3, or explicit local-only state.

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
- `untrack` records `target = "local"` in the existing placement sidecar and
  maintains a literal anchored rule in the parent `.gitignore`. It removes S3
  pointers while preserving the payload. The sidecar is the durable intent;
  ignore rules alone cannot remove already tracked Git files.
- An S3 boundary must stay addressable by the storage engine, which reads a
  backslash as a separator although the repository keeps it as an ordinary
  name character. Automatic and explicit S3 placement refuse such a path
  before any metadata is written, and every command that would address
  metadata an earlier release left at one refuses with a rename hint, so the
  engine never holds metadata it cannot be commanded to address again.

Local boundaries are excluded from automatic placement and explicitly removed
from every private publication index. Only their sidecars and ignore rules are
published. S3 cleanup reuses the reference-protected purge transaction.
`refresh` reads incoming and pending local placement to keep payload bytes
through merged Git deletions, without requiring retired S3 data to be fetched.
An explicit `storage set --to git|s3` resumes tracking and removes only the
managed ignore rule; reset does not implicitly resume tracking.

These commands do not publish. Automatic policy is evaluated during `plan` and
`publish`; existing published content is not silently moved because its size
changed. An automatic candidate below 1 MiB uses Git without routine warning;
1–10 MiB uses Git with semantic-review feedback; above 10 MiB uses S3. Explicit
S3 below 1 MiB remains valid but reports an efficiency warning based on the
aggregate materialized boundary size.

## Scoped publication

`archive` is a separate transition for completed deliverable tasks. It checks
merged GitHub pull-request evidence against the fetched shared branch, retains
task identity, and prepares a date-grouped directory with an immutable source
inventory in `.workspace-mgr-archive.json`. It requires an explicitly requested
infrastructure task and both source and destination scopes. The default layout
is `{year}/{month}` from the original task timestamp; active tasks stay at the
top level. No merge or synchronization hook invokes it automatically.

Publication measures the full inventory before copying any S3 version. The
transport copies all payload versions and recreates delete markers, including
objects absent from current storage manifests, with a durable private retry journal.
It preserves literal S3 keys and records original timestamps plus the new exact
VersionIds and ETags. Storage file and directory entries are rewritten without
changing their content hashes. The complete receipt remains in Git even when
large; it is storage control metadata and may not be untracked or placed in S3.
An identical canonical receipt under the remote's
`.workspace-mgr/archive/<source-prefix-sha256>.json` makes old Git revisions
addressable independently of their checked-out files. Historical hydration
follows bounded mappings only after an exact original version is missing,
preserving hash and size checks. Planned local pointers can read their original
exact versions before publication.

Archive completion uses supported current task identity and current associated
PR state. Saved review branch metadata is a lookup hint, never replayed content
proof. Historical configuration, directory-tree transitions, commit-to-PR
coverage, full-history availability and branch-tip ancestry do not determine
eligibility. `task upgrade` locally preserves compatible current metadata rather
than creating a new historical checkpoint. `task adopt` verifies the supplied
live PR control association without inspecting ordinary payload trees or dirty
content.

Archive and task rename move ordinary directory contents unchanged. They do not
parse Git worktree pointers or administration, inspect environment launchers,
follow ordinary links, scan script paths, or repair runtime dependencies. Nested
Git remains governed by shared-ignore and no-outer-tracking control rules.
Zero-byte `.git` cache markers remain ordinary payloads. New relocation plans
contain no runtime rewrite references. Old attempt journals retain their saved
reference snapshots for backward-compatible cancellation.

Before a local archive move, `.workspace-mgr/local/archive-attempts/` saves
original tool-mutated metadata, modes, directory relocation facts, owner refs,
and retirement records. Older journals may also hold saved Git rewrite
snapshots, which remain readable for lossless cancellation. It records generated publication commits before
their ref update and verified pushes before cleanup. `archive --cancel` restores
only an unpublished attempt, preserving whole local directories by rename.
Cancel phases and generated undo commits are durable, so interruption can be
resumed without losing other infrastructure work. Before local restoration,
cancel verifies the original source history, withdraws only the attempt's exact
registry versions, deletes owned copies and markers by exact version ID, and
aborts owned unfinished uploads. A full scan must prove remote cleanup and
record it durably before local restoration. Cancel restores local directories,
metadata, refs and retirement state and removes empty generated parents, then
releases both exact ownership claims before marking the attempt cancelled.
Foreign history blocks initial remote cleanup and stays intact. A retry after
durable remote cleanup finishes local undo without touching later foreign writes
or another owner's claims; completed retries do not recheck original source
generations that a newer archive may have retired.

The configured Git remote first reserves source copying with a control tag under
`refs/tags/workspace-mgr/archive-copy/`. Its immutable descriptor binds the
normalized planned receipt and the private copy journal's attempt nonce before
any destination history is written. It coordinates canonical publication with
a separate tag under `refs/tags/workspace-mgr/archive-registry/`. Its immutable blob
binds the complete copied receipt and transaction. Compare-and-create chooses
the owner; exact remote object ID checks fence mutations. Bindings have no
expiration or takeover. Cancellation releases only its exact copy reservation
and canonical binding after remote cleanup and local restoration. Canonical mappings for completed
archives remain available to historical readers.

Conditional registry Put remains the default. For B2's official endpoints, a
separate writer suppresses SDK flexible checksum headers and writes Content-MD5.
An explicit provider not-implemented/not-supported response permits an
unconditional retry only with the verified Git binding for that exact receipt.
Fresh reads enumerate and compare every registry version, rejecting conflicts
or delete markers. This retains concurrency protection without retaining a
second payload history at the original task path.

Source deletion requires the exact copied receipt on the configured shared
branch. Before that merge, a live branch or tag containing the original task
tree protects the whole source prefix, including trees from before manifest
adoption. After merge, old mapped generations hydrate through the registry, so
historical tags remain while their source bytes can retire. Actual newer
referenced generations without mappings remain protected. Cleanup deletes
only verified mapped versions and completes only when a full scan finds the
original prefix empty of both payload versions and delete markers. Protected
history reports `cleanup_pending`; unmapped concurrent additions report
`blocked_unmapped` and remain in the durable retry queue. Neither is terminal
completion, even when Git publication or synchronization succeeded. A typed
copied-receipt prefix intent persists even when its original inventory is
empty; only a published-receipt scan confirming no versions or markers clears
that intent. Historical revisions use workspace-mgr hydration to interpret the
canonical archive mappings after
their original versions have been retired.

Private purge queues and copy journals use schema 2, fencing the released
0.6.0 reader's schema-1 deletion/resume path. New clients read old schema 1
journals for preview, then durably persist schema 2 before any copy,
registry mutation, source deletion or remote cancellation. Restoring an old
purge snapshot through cancel also writes schema 2. Public receipt/registry
contexts normalize to schema 1 so immutable receipt bindings and old data
formats remain stable. Any archive receipt in the publication index requires
workspace-mgr 0.7.0 independently of task schema or S3 inventory size, and the
managed repository declaration rises before uploads.

The executable, local storage engine, S3 transport, archive registry, and
history copy/cancel adapters are Rust. Native storage uses versioned JSON
manifests and typed Rust operations. Legacy DVC metadata is read only by the
migration and historical compatibility layer; no Python assets are executed. S3 requests use
native Signature Version 4 and explicit checksum/conditional headers. The
production path does not install or invoke Python or DVC.

`task rename` is a local identity-preserving transition. It moves an ordinary
task directory as one filesystem unit and atomically rewrites the lowest schema
that preserves its fields, including schema 4 completion evidence, or rewrites
only private metadata for infrastructure. Existing schema 1
manifests remain readable and are upgraded by rename. The immutable task ID
keeps private state and commit ownership stable, while the unchanged target
branch preserves the existing pull request. When a published deliverable path
differs from the current path, publication finds the prior manifest by stable
task ID, treats that old tree as a temporary cleanup scope, maps published
placement history to the new path, and removes the old tree in the same commit.
Version-aware S3 object IDs are path-bound, so local rename removes their old
path bindings from moved pointers. The next publish creates and verifies new
object versions at the new path before publishing Git, then permanently deletes
every version at the old path unless a current remote branch or tag protects it.

For a task publication, the CLI:

1. resolves the task and explicitly authorized scopes;
2. fetches the configured base and target branches and refuses when the base
   tip requires a newer CLI;
3. verifies that an existing target branch belongs to the same task identity;
4. previews placement, refusing an existing or newly selected S3 boundary the
   storage engine cannot address, builds and validates a preview private index,
   resolves the rule source of every ignored path in the scopes in one batched
   pass, and acquires task and storage-boundary locks. The step-8 refusals,
   including a task manifest schema this build cannot declare, are decided on
   that preview, before the cloud-usage measurement and before any placement
   change or upload, because resolving them can change what the publication
   holds. The preview's S3 metadata does not yet show output changes that step
   6 commits, so for a task that documents nothing the documentation refusal
   asks the storage engine whether a boundary in the task gained or changed
   files, and recognizes canonical native metadata that only drops unchanged
   entries as retiring content; the legacy reader retains its line comparison;
5. measures the task's cloud usage from the preview tree and refuses a
   publication that would exceed the task's limit, unless it only removes
   content apart from at most 1 MiB (1048576 bytes) of new control-file
   content per publication, where metadata that only drops entries is free,
   before any local placement or upload. The placement record of a result kept
   local before it was ever published is the one input to the documentation
   refusal decided here rather than in step 4: it counts as content only when
   the task is within its limit, because while the task waits for the user's
   decision a record added to the cleanup would be growth this step refuses;
6. applies automatic placement, reconciles S3 metadata, judges the documentation
   refusal and re-measures S3 usage from the committed metadata, restoring that
   metadata if either refuses, then uploads all live in-scope objects and
   verifies them;
7. builds a private Git index from the target branch, or the base branch when no
   target exists;
8. stages only declared scopes, reconciles `minimum_cli_version` in that
   index with the task manifests it holds, and rejects gitlinks, symbolic links
   that escape the repository, invalid placement, whitespace errors, paths
   outside the scopes other than that reconciliation, a deliverable publication
   that adds or changes content inside its own task directory while that
   directory documents nothing, and scoped content that only an untracked
   ignore source hides;
9. re-measures Git usage from the final tree, then creates a commit with its
   task identity, scope, and scope-authorization trailers plus any
   requirement-change and cloud-usage approval trailers, updates the local
   target ref with compare-and-swap semantics, pushes an explicit refspec, and
   verifies the remote object ID;
10. permanently deletes all versions at obsolete S3 object paths, deferring
    paths still referenced by a current remote branch or tag.

Deliverable and infrastructure target refs remain unmounted. Publication uses
a private Git index and commit-tree to advance the task ref without switching
the shared checkout or resetting its index. A `.workspace-mgr.toml` reconciled
only for publication stays in the published tree; it does not overwrite the
shared checkout's file.

The Git commit is the publication point for the combined transaction. A later
Git error may leave an unreferenced S3 object version, but a published Git
revision must never reference missing S3 content. Publication never switches or
rewrites the shared checkout's working files.

The usage re-checks in steps 6 and 9 guard against content that changes while
publication runs. A refusal there leaves local placement and S3 metadata
applied, like any other failure after placement. A refusal in step 9 may also
leave an unreferenced uploaded S3 version, which counts as projected usage
until a later publication succeeds. `plan` and `publish --dry-run` stop after
the preview and never run the re-checks; `plan` reports the measurement without
refusing, while `publish --dry-run` refuses where step 5 would.

## Cloud-usage measurement

Usage is measured per task from the fetched base tip, the fetched task tip when
one exists, and the projected tree. Published usage describes the task tip;
projected usage adds this publication.

- Git: published objects are those reachable from the task tip but not from the
  base tip or its tree. The delta is the projected tree's objects that are in
  neither the task tip's tree nor the base, so content a merged base already
  holds is not charged again. Sizes are uncompressed object sizes; a Git LFS
  pointer blob adds the size of the object it references. When either total
  exceeds the limit, both object sets are packed with fixed single-threaded
  settings and the packed byte counts replace the uncompressed Git figures,
  and Git contributors use each object's compressed size in the local object
  store instead of its uncompressed size. The published pack size is cached per
  task tip and base tip.
- S3 on a version-aware bucket: the task's commits since the base branch are
  replayed in order over its storage metadata. Each recorded object version is
  counted at its object path. A path that a commit removes from every metadata
  file is retired with all its versions, matching the purge after publication;
  a path that moves between metadata files in the same commit stays live. The
  projection applies this publication's metadata changes, including the
  worktree metadata, as one such change, then adds pending uploads: new
  automatic S3 placements, metadata without a recorded version, and outputs the
  storage engine reports as changed, sized from the worktree with symlinked
  files counted by their targets. Recorded versions that the published history
  does not contain, such as uploads left by a publication that failed before
  its Git push, also count.
- The filesystem adapter used by isolated tests is content-addressed and never
  purged, so each distinct file digest counts once. A directory recorded only
  by its aggregate is resolved to its files through its manifest in the local
  storage cache or the filesystem remote, and each file is sized by its stored
  object. At the gate, each added or modified file beneath such a directory is
  charged its worktree size; the check before upload and the replayed history
  charge the digests the new manifest adds. New content, including same-size
  and shrinking rewrites, is therefore charged the same at every check, and a
  deletion adds nothing. A directory version whose manifest or objects are not
  available locally is charged whole.
- Names in storage metadata and in the storage engine's status only label the
  accounting and are kept as the storage engine wrote them, with `/` as the only
  separator: a backslash is an ordinary name character on Linux and macOS. No
  published metadata can make a measurement fail.

A publication is cleanup-only when it uploads nothing to S3 and, apart from
deletions, changes only workspace-mgr control files (task manifests, placement
records, storage metadata, `.gitignore` files, and the root
`.workspace-mgr.toml`) with at most 1 MiB (1048576 bytes) of new control-file
content. Each added or changed control
file that the remote does not hold yet is charged its full new size, because a
rewrite stores its new content whatever its size. A canonical native directory
manifest that only removes entries is free when every retained entry preserves
its path, checksum, physical size and exact version binding. Legacy metadata
whose object identities are a subset of the identities it replaces is charged
only for the lines it does not share with that version.
Each added control file is also charged its path plus 28 bytes, an upper bound
on the Git tree entries it adds, so new directory names cannot carry content.

Contributors are aggregated per path. Measurement reads Git objects, local
storage metadata, and one local status pass from the storage engine; it does
not list or read S3 objects. Parsed metadata is cached per immutable Git blob.
Native directory entries are inline in their manifests. Legacy directory
listings are read from the local storage cache and, for the filesystem test
remote, from the remote directory itself.

## Private storage adapter

The storage adapter is built into the Rust executable. S3 remotes require
exact object-version metadata and existence checks. A filesystem remote is
compiled only by the `test-storage` feature for isolated tests and uses
remote-presence verification. Release builds reject it in the public S3 schema.

Versioned reads use a private adapter instead of the engine's generic fetch
path. It requires a complete file/version manifest, checks existing cache bytes
against the recorded content hash, and fetches missing bytes with exact-version
GETs. GET responses supply the version, full object length, and ETag checks;
downloaded bytes are hashed locally before cache insertion. Temporary downloads
stay on their destination cache filesystem and are removed on success or failure.
Cache writes and
directory-tree construction run on the owning thread. Checkout and local
conflict/content validation remain with the engine. A hydrate or refresh reuses
only the remote verification performed in that same operation; it never persists
a remote-existence receipt for a future command.

For cache hits and publication verification, groups of at least eight entries
under the same non-root parent prefix share at most two ListObjectVersions pages
of up to 1,000 results each. Matching uses both key and exact version ID, including
historical versions under a current delete marker. A page proves only the
versions it contains; unresolved entries fall back to exact HEAD requests,
including after a denied/unsupported listing or a non-advancing pagination
marker. Listing metadata does not prove object-read permission, which GET checks
for downloads. Both listing groups and individual HEAD/GET requests use bounded
concurrency of sixteen, with at most sixteen submitted futures at once. This
limits historical traversal without weakening recorded version/size/ETag or
downloaded-content checks. The immutable version manifest and the local content
hash remain separate from ETags, which are not assumed to be content MD5s.

Moving an S3 boundary clears path-bound cloud metadata before upload so the new
object path receives and records its own version ID. After Git publication,
every version at the old object path is permanently deleted unless protected by
a current remote branch or tag. This deliberately makes older Git revisions
that used the removed path non-hydratable.

## Shared-checkout refresh

`refresh` requires the configured shared branch and a clean shared Git index. It
first refuses an incoming revision whose `minimum_cli_version` this CLI does
not meet, then verifies a fast-forward, detects incoming boundaries the storage
engine cannot address, prefetches incoming S3 revisions, compare-and-swap
updates the local branch ref, resets the index, materializes ordinary Git paths
whose prior working state was clean or absent, and hydrates stored content.
Existing working-tree overlays are preserved. A failure after the ref update
rolls back the ref, index, ordinary files, metadata, and outputs created by the
refresh.

Merged-branch cleanup follows successful materialization and storage
verification, and also runs when synchronization finds the shared branch
already current. Dry-run only reports the deletion plan. The cleanup checks
same-repository GitHub pull requests through `gh`: the merged revision must be
reachable from the fetched base, and the immutable pull-request head must
match every live local and configured-remote head. This proves squash merges
without treating Git ancestry of the original head as proof or discarding new
local commits. Open, resumed, ambiguous, fork, and other-base refs are retained.
Configured base, current, default and protected branches are excluded. Any
branch checked out in a legacy or custom worktree is retained. Cleanup never
detaches a worktree or removes its directory or files.

Remote deletions use an exact head lease, and local deletions compare and swap
the expected ref. The `branch_cleanup` report separates planned and deleted
refs, skipped branches, errors, warnings, and unavailable GitHub evidence.
Cleanup errors do not roll back a successful refresh transaction. It removes
branch refs only; task directories and retained S3 payloads remain. A normal
pending purge retry can delete previously queued S3 paths after a removed ref
releases their last live protection. Completed-directory archive remains an
explicit user-requested infrastructure operation.

The requirement check precedes detection, so refresh never inspects incoming
storage metadata that only a newer release can read, and `--dry-run` refuses
where an applied refresh would. Detection precedes every change, including the
purge queue, so `--dry-run` reports the same condition an applied refresh does.
An unaddressable boundary is excluded from prefetch, checkout, verification,
and the unsafe-output scan. This preserves the existing path convention during
the native migration; older engines disagreed about literal backslashes. The
purge adapter still enumerates its metadata literally. Its metadata advances
with the branch, and refresh places no payload for it. A payload already held
there is compared with incoming metadata: an exact match is kept; different
bytes or a payload without accompanying metadata refuse refresh before any
change. Rollback restores the boundary from its metadata alone because refresh
never placed or removed its output. `move` is the supported recovery to a name
without backslashes.

A `move` whose source payload is not materialized fetches it through the source
metadata before any change and checks it out at the destination, because the
rename drops the recorded S3 version, which a version-aware remote needs to
locate the old object.
