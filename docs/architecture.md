# Architecture and transaction guarantees

## State boundaries

`workspace-mgr` separates fixed product policy, tracked repository facts,
scoped task state, and private runtime state. `.workspace-mgr.toml` contains
only non-secret Git and optional S3 locations, plus the `minimum_cli_version`
compatibility declaration that `workspace-mgr` maintains itself. Task manifests
contain identity, purpose, current slug, scope, and branch state and, from
schema 3, the user's cloud-usage approval. The task ID and review branch are
immutable; the slug and deliverable path may change together. Deliverable
manifests are tracked inside their task directories; infrastructure manifests
and isolated worktrees live below the Git common directory. Private indexes and
locks also live there. All mutating repository, placement, publication,
hydration, and refresh operations share a repository lock; task and
storage-boundary locks add narrower diagnostics.

The user's cloud-usage approval is task state. `task approve-cloud-usage`
writes it into the task manifest's `[cloud_usage_approval]` table, which makes
the manifest schema 3, so a deliverable publishes the approval with the task
and reviewers see it in the pull request. An infrastructure manifest keeps it
private. Every publication commit also carries a `Cloud-Usage-Approval`
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
`.workspace-mgr.toml`, `AGENTS.md`, the root `.gitignore`, `.dvc/config`,
`.dvc/.gitignore`, and `.dvcignore` have fixed roles: the TOML file is the
user-editable source of Git/S3 facts, while the other five are whole-file
generated paths owned by the product and reconciled by `init`. Ownership is
structural for all but one: the root `.gitignore` exists in most repositories
before the product does, so it is claimed by the generated header the product
writes rather than by its path, and a file without that header is refused with
the migration into `.workspace-mgr/repository.gitignore` rather than
reconciled. Git has no include directive, so the root ignore file is generated
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
  base commit without moving any ref. `init` round-trips an existing
  declaration unchanged.
- Declarations are plain release versions. An installed release meets one by
  semantic-version precedence, and a pre-release also meets a declaration of
  its own release, so a release candidate can operate on the repositories it
  raises.
- Product policy maps each task manifest schema to the oldest release that
  reads it: schemas 1 and 2 need no declaration, and schema 3 needs 0.4.0.
  Writers use the lowest schema that represents a manifest, so only a task that
  records a cloud-usage approval produces schema 3.
- Publication reconciles the declaration. Each private publication index (the
  preview, the pre-upload validation, and the final index) is scanned for task
  manifests one directory below the root. When `.workspace-mgr.toml` is outside
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
  same schema produce the same content, a branch raised earlier follows a base
  branch raised further, and a branch whose manifests no longer need its raise
  withdraws it without ever lowering a declaration the base branch carries,
  which a rebase merge would otherwise replay onto the base branch; hosting
  merges stay clean as long as every raised branch is published after the base
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
  removing the approval only when the task's own manifest needs the newer
  release; another task's manifest, such as one merged on the base branch, is
  named with update advice only. Infrastructure manifests are private and
  never trigger a raise, but an infrastructure publication raises the
  declaration when base content it publishes needs one, and its isolated
  worktree then receives the published configuration.
- Releases up to 0.3.0 do not know the key. Once a raised configuration is
  merged, they reject it as an unknown field, which also fails closed.
- Another task's published manifest is read only for the identity fields that
  placement history needs, so its newer optional fields never break a
  repository-wide scan.

## Update observation boundary

Update discovery is advisory and user-scoped, not repository configuration.
Before parsing any command, the executable reads a small cache in the user's
cache directory. A successful crates.io result is reused for six hours and a
failed attempt for one hour. Refresh uses a nonblocking process lock, a 750 ms
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

`task rename` is a local identity-preserving transition. It moves an ordinary
task directory as one filesystem unit and atomically rewrites manifest schema
2, or rewrites only private metadata for infrastructure. Existing schema 1
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
   holds;
5. measures the task's cloud usage from the preview tree and refuses a
   publication that would exceed the task's limit, unless it only removes
   content apart from at most 1 MiB (1048576 bytes) of new control-file
   content per publication, where metadata that only drops entries is free,
   before any local placement or upload;
6. applies automatic placement, reconciles S3 metadata, re-measures S3 usage
   from the committed metadata, then uploads all live in-scope objects and
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

Deliverable target refs remain unmounted, so publication never changes the
shared checkout. An infrastructure target ref is mounted only in its dedicated
worktree; after the ref update, the CLI synchronizes that worktree's index to
the published tree without touching its files or any shared checkout. The one
exception is a `.workspace-mgr.toml` that the publication reconciled, which is
also written to that worktree so its files keep matching the branch it has
checked out.

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
rewrite stores its new content whatever its size. Storage metadata whose
entries are a subset of the entries it replaces, with the same object paths and
versions (or digests where no version is recorded), is charged only the lines it
does not share with that version, so metadata that only drops entries is free.
Each added control file is also charged its path plus 28 bytes, an upper bound
on the Git tree entries it adds, so new directory names cannot carry content.

Contributors are aggregated per path. Measurement reads Git objects, local
storage metadata, and one local status pass from the storage engine; it does
not list or read S3 objects. Parsed metadata is cached per immutable Git blob.
Directory manifests are read from the local storage cache and, for the
filesystem test remote, from the remote directory itself.

## Private storage adapter

The S3 adapter currently uses DVC 3.67.1 internally. S3 remotes require exact
object-version metadata and existence checks through an embedded verifier using
the same exact DVC release. This is a maintainer compatibility boundary, not a
public command or repository concept. A filesystem remote is compiled only by
the `test-storage` feature for isolated tests and uses remote-presence
verification. Release builds reject it in the public S3 schema.

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

Detection precedes every change, including the purge queue, so `--dry-run`
reports the same condition an applied refresh does. An unaddressable boundary is
excluded from prefetch, checkout, verification, and the unsafe-output scan,
which resolve each pointer as an engine command target: the engine's `status`
rewrites the backslash, reports the rewritten path missing, and fails, so
verifying such a boundary would roll back the whole refresh. The purge adapter
still enumerates it, because it collects pointers through the engine's Python
API, which reads the path literally. Its metadata advances with the branch, and
refresh places no payload for it. A payload this checkout already holds there is
compared with the incoming metadata without the engine: an exact match is kept,
and anything else, including a payload with no metadata beside it, refuses the
refresh before any change, because refresh can neither replace nor verify it.
Rollback therefore restores such a boundary from its metadata alone, because
refresh never placed or removed an output for it.

A `move` whose source payload is not materialized fetches it through the source
metadata before any change and checks it out at the destination, because the
rename drops the recorded S3 version, which a version-aware remote needs to
locate the old object.
