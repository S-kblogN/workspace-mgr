# Changelog

All notable changes to this project will be documented in this file. The format
is based on Keep a Changelog, and this project follows Semantic Versioning.

## [Unreleased]

### Added

- `init` owns the root `.gitignore` and generates it from the product's fixed
  rules for output that is regenerated rather than retained, this repository's
  own rules imported verbatim from the new optional
  `.workspace-mgr/repository.gitignore` module, and any well-formed
  `# workspace-mgr local begin` block the root file already holds, which
  regeneration preserves byte for byte. A hand edit below the generated header
  is drift that `doctor` reports and `init` repairs; a block marker left
  without its partner is dropped and named in the reported action. The ignore
  module carries ignore patterns only, and one containing those markers is
  refused. Git has no include directive, so the file is generated rather than
  merged.
- Before the first successful initialization, an existing root `.gitignore` is a
  scaffold collision like `AGENTS.md`. Nothing is migrated silently and nothing
  is discarded: move the repository's rules into
  `.workspace-mgr/repository.gitignore`, remove the root file, and run
  `workspace-mgr init` again.
- `plan` and `publish` refuse a path inside the resolved scopes that only a
  machine-local ignore rule hides — the user's global excludes,
  `.git/info/exclude`, or an ignore file whose matching bytes the publication
  does not carry — because such a rule keeps the file out of every other clone
  and out of review. The refusal names the path, the rule, and the file the rule
  came from, lists the first five and counts the rest, and is decided before
  placement or upload. Carrying is decided on content, so a rule appended to a
  tracked ignore file and never published is machine-local too. Git resolves the
  deepest matching ignore file first, so a carried repository or task rule that
  also matches is the reported source, and the product's own fixed rules are
  carried by every installation, so an ordinary `.DS_Store` never triggers it. A
  directory whose whole content is ignored, which Git reports as one collapsed
  entry, is expanded so a file-level rule is resolved rather than missed.
- `plan` and `publish` report the `bulk-publication` warning when one
  publication adds more than 200 new files, or more than 256 MiB (268435456
  bytes) of new content, inside a deliverable task directory, counting content
  automatic placement routes to S3 once rather than twice, and not counting a
  file that only moved. It is a check rather than a refusal.
- Publication refuses a deliverable task that adds or changes content inside its
  own task directory while that directory documents nothing. A task documents
  itself with Markdown files of its own choosing inside its directory; a README
  still carrying only the creation scaffold's directory map does not count. A
  storage pointer counts as the content it addresses, so a result routed to S3
  is judged like one kept in Git. A publication that only retires content is
  allowed; one that removes the task's last record while publishing content is
  refused by name. `plan` applies the same refusal, before it changes placement
  or uploads anything.
- Publication refuses a staged symbolic link whose target is outside the
  repository. The target is classified from the staged link alone, without
  reading the filesystem, so a link inside a boundary placed in S3 or kept local
  with `untrack` is outside what the check can see.
- `plan` and `publish` report structured `warnings` when there is something to
  report. `task-record-unchanged` reports a publication that changes content
  inside the task directory while none of the task's own documentation changed
  with it, and says when to ignore it.
- `plan` and `publish` report `ignored_paths` beside the existing
  `ignored_entries` count when any path inside the resolved scopes is ignored,
  so ignored content inside a task can be reviewed as reproducible output rather
  than lost work. The count stays exact; the list carries up to fifty entries.

### Upgrading

- The root `.gitignore` is now product-owned, so a repository initialized by an
  earlier release performs one migration before its next `init`. Unlike the
  other owned paths, this one is claimed by the generated header rather than by
  its name, so `init` and `doctor` refuse a root `.gitignore` the product did
  not write instead of replacing it. Move the repository's rules into
  `.workspace-mgr/repository.gitignore` (`git mv .gitignore
  .workspace-mgr/repository.gitignore`) and run `workspace-mgr init`, which
  regenerates the root file from the product's fixed rules followed by that
  module; every existing rule keeps working, negations included. Publish the
  result like any other repository-wide change.

### Changed

- The fixed policy, the workspace model, and the user guide state the curation
  half of the workplace rule: every file under a task is either selected for
  publication or ignored by a rule this repository tracks, the by-products of
  the work are not published by default, task-specific ignore rules belong in
  `<task>/.gitignore` while repository-wide rules belong in
  `.workspace-mgr/repository.gitignore` — a shared root path that costs the same
  authorization and publication as any change outside the task directory — and
  S3 is not a way to keep Git small. The turn-end
  reconciliation now reads the plan's `changed_paths`, `ignored_paths`, and
  placement decisions and resolves anything that is neither retained content nor
  ignored.
- The fixed policy, the workspace model, and the user guide state that the task
  directory is the workplace: tools, intermediate materials, and the task's
  record of decisions, process, and hard-to-reproduce results are created inside
  it rather than in a temporary directory, a `mktemp` directory, or the home
  directory. They also answer the two cases that push work outside a repository,
  large scratch content and credentials, and the instruction policy version
  moves from 9 to 10.
- The scaffolded task README's directory map asks the task to keep its tools,
  process, decisions, and hard-to-reproduce results in the task directory and to
  list them there. Existing tasks are unaffected: a task whose README was ever
  edited already satisfies the new publication guard, and no upgrade step or
  repository change is required.
- The nested-checkout refusal message now also names copying the needed files
  into the task directory, or into a declared scope for an infrastructure task,
  which has no task directory. The escaping-symbolic-link refusal names the same
  destination.
- `move` accepts a storage boundary whose payload is not materialized, which a
  fresh checkout and an unaddressable boundary both have, so the `refresh`
  recovery under Fixed is possible. It fetches that payload through the source
  metadata before it changes anything and materializes it at the destination,
  because the rename drops the recorded S3 version a version-aware remote needs
  to find the old object; a failed move removes what it materialized and
  restores the metadata.

### Fixed

- A command that receives input on its standard input no longer deadlocks once
  its reply outgrows the operating system's pipe buffer. The input is written
  while the reply is read, rather than in full beforehand, so resolving the
  ignore rules of a task with thousands of ignored files, or validating the
  ignore rules of a large `untrack` boundary, completes instead of hanging.
- Repository paths keep backslashes as ordinary file-name characters instead of
  rewriting them to `/`, so storage metadata, user-typed paths, and Git paths
  compare exactly.
- A file above 10 MiB whose path contains a backslash no longer breaks every
  later `plan` and `publish`. Automatic and explicit S3 placement refuse such a
  path before the storage engine writes metadata it cannot address, and
  metadata left by earlier releases is reported with a `workspace-mgr move`
  recovery hint.
- `refresh` no longer fails and rolls back, freezing inbound synchronization,
  because the shared branch carries one S3 boundary whose path contains a
  backslash, and `refresh --dry-run` no longer reports plain success for such a
  refresh. The storage engine's `status` rewrites that backslash, reports the
  rewritten path missing, and fails, so verifying the boundary rolled back the
  whole refresh, while the preview reported success for a boundary arriving
  new, because it inspects only metadata already present. Refresh now detects
  those boundaries before it changes the branch, the index, the working tree,
  the purge queue, or stored content, advances the branch, hydrates every other
  incoming boundary, and leaves only the unaddressable payload unhydrated. It
  names them in `storage.unaddressable` and in a new
  `unaddressable-storage-metadata` entry in `warnings`, which gives the
  recovery: in an infrastructure task scoped to the directory that holds the
  boundary, `move` it to a path without backslashes, hydrate the directory's
  other boundaries by name, publish, and merge. Refusing the whole refresh was
  rejected deliberately: it would freeze inbound synchronization for every
  checkout over one path, while the recovery needs no refresh at all. Refresh
  cannot replace or verify a payload at such a path either, so it refuses,
  before any change, in a checkout that holds a payload there the incoming
  metadata does not describe byte for byte, or one without metadata beside it;
  a payload that already matches is kept.

## [0.3.0] - 2026-09-14

### Added

- `workspace-mgr remove <path>...` explicitly deletes a file or complete storage
  boundary and schedules obsolete S3 content for permanent cleanup after
  publication.
- `workspace-mgr untrack <path>...` keeps local bytes, adds managed ignore rules,
  and records durable local-only placement. Publication removes payloads from
  Git and queues obsolete S3 versions for reference-protected permanent cleanup.
- Local-only placement is reported by storage status and plan, survives
  publication and post-merge refresh, and can be explicitly restored to Git or
  S3 with `storage set`.

### Changed

- Deletions, moves, task renames, S3-to-Git transitions, untracking, and task
  discard permanently purge all versions and delete markers at retired S3
  object paths. Current
  remote branches and tags protect referenced content; pending cleanup is
  retried by publication, refresh, or discard after those references disappear.
  Git history is retained, but old revisions cannot hydrate purged S3 content.

### Fixed

- Refresh preserves local-only payloads after merged Git deletions and S3
  retirement, including local edits and cases where old remote data or caches
  are unavailable. Failed refresh rolls back metadata without replacing the
  retained bytes.
- Conflicting stale S3 pointers are rejected before publication and can be
  reconciled through `untrack` or explicit re-tracking without damaging owned
  ignore rules.
- Updated rustls to 0.23.45 to address RUSTSEC-2026-0285.

## [0.2.2] - 2026-08-30

### Changed

- Deliverable task creation now hands agents an explicit requirement to publish
  the initial scaffold and create its draft pull request immediately.
- Effective instructions now require automatic publication and pull-request
  reconciliation before every writable-task turn ends, with blockers reported
  as exact unsynchronized state.

## [0.2.1] - 2026-08-30

### Changed

- Effective agent instructions now separate repository-wide read access from
  write ownership: deliverable tasks require explicit user approval for exact
  paths and actions outside their own directory, while infrastructure tasks
  write only their exact user-authorized manifest scopes.
- Additional-scope arguments are documented as an audit record of existing
  user authorization, never as a way for an agent to create authorization.

## [0.2.0] - 2026-08-30

### Added

- `workspace-mgr task rename <new-slug>` changes a task's current topic label,
  moves a deliverable workspace as one unit, preserves its immutable task and
  review-branch identity, and lets the next publication remove the old Git tree
  without losing published Git or S3 placement history.

### Changed

- Task manifest schema 2 records the mutable current slug separately from the
  immutable task ID. Schema 1 manifests remain readable and are upgraded when
  renamed.

## [0.1.1] - 2026-08-30

### Fixed

- Managed `AGENTS.md` now installs the latest stable crates.io release instead
  of pinning the CLI version that generated the scaffold.

## [0.1.0] - 2026-08-30

### Added

- Advisory update discovery on every invocation, with six-hour successful
  caching, one-hour silent failure caching, stable/prerelease channel-aware
  selection, agent-directed stderr notices, and no automatic mutation.
- Managed `AGENTS.md` bootstrap instructions for approval-gated installation of
  the exact scaffold-generating CLI version from crates.io, runtime setup, and
  instructions retry.
- Isolated update-check tests covering cache reuse, failure isolation, registry
  filtering, concurrency, timeouts, and structured-output separation.

## [0.1.0-alpha.1] - 2026-08-30

### Added

- Initial standalone Rust CLI project.
- Repository initialization, effective instructions, diagnostics, and task
  scaffolding.
- Structural scaffold ownership: first initialization reports reserved-path
  collisions without inspecting content, while later `init` runs replace old,
  edited, or damaged product-owned files with the current deterministic forms
  and `doctor` reports any remaining drift.
- Scoped repository and managed-storage transaction commands with strict
  repository and task schemas.
- A public storage schema with deterministic internal configuration, exact
  private-runtime provisioning and enforcement, and mandatory S3 object-version
  verification.
- Shared-checkout refresh and rollback support.
- Isolated local-storage and repository transaction integration tests, including
  refusal and rollback guards.
- A networked end-to-end suite using versioned MinIO and `git daemon`, including
  configuration-drift repair and failure-ordering assertions.
- Linux and macOS CI plus native packaging workflows.
- A declarative release workflow that publishes each new `Cargo.toml` version
  once, creates an immutable matching tag, and reconciles native GitHub Release
  assets after the full CI and end-to-end gates pass.
- Bidirectional Git/S3 placement transitions, ordinary Git refresh
  materialization, repository-wide operation locking, and tracked-input
  hardening.
- A fixed collaboration-versus-artifact storage model with aggregate boundary
  metrics, a 1 MiB recommended S3 minimum, a 1–10 MiB semantic-review band,
  structured placement warnings, and explainable plan/status decisions.
- First-class infrastructure tasks with explicit shared scopes, private task
  metadata, isolated worktrees, private storage-state handoff, and scoped
  publication without timestamped repository task directories.
- A provider-neutral review policy and publication handoff that assigns
  pull-request creation, living metadata, verification, and merge authority
  while keeping hosting-provider API calls outside the CLI.
- Transactional `task discard` for an explicitly abandoned, unmerged task,
  including stale-plan and merged-task refusal, leased remote-branch deletion,
  local quarantine and rollback, additional-scope restoration, infrastructure
  worktree cleanup, and non-destructive S3 orphan reporting.

### Fixed

- Planning and storage-status discovery now ask Git for tracked and non-ignored
  paths instead of recursively entering ignored directories; ignored-entry
  reporting also stays at directory granularity.
- Task manifests now enforce the product-owned task identity, branch mapping,
  required purpose metadata, and non-overlapping scopes on every load.
- Distinct tasks can no longer claim the same remote branch after a concurrent
  cross-clone task creation race.
- Runtime setup refuses unmanaged existing targets, records explicit ownership,
  and rechecks the target only after acquiring the setup lock.
- Shared-checkout refresh rejects incoming storage metadata and outputs that
  would traverse local symlink ancestors before advancing the shared ref.
- Private storage-engine failures now expose managed-storage diagnostics without
  leaking internal executables, runtime paths, or tracebacks.
- Failed refresh prefetches now identify object-read credentials and provider
  download or read-transaction caps as likely causes without exposing the
  internal engine command.
- Temporary detached worktrees are forcibly removed, pruned, and verified even
  when managed-storage prefetch fails.
- Repository operations no longer fall back to an ambient storage executable;
  only the provisioned private runtime is used in production builds.
- Refresh always uses the tracked Git remote and shared branch, and repository
  initialization refuses to relocate existing S3 boundaries.
- Failed automatic large-file placement restores partial metadata, while
  infrastructure storage status resolves the private task manifest correctly.
