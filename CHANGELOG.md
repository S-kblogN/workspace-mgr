# Changelog

All notable changes to this project will be documented in this file. The format
is based on Keep a Changelog, and this project follows Semantic Versioning.

## [Unreleased]

### Added

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

### Changed

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

### Fixed

- Repository paths keep backslashes as ordinary file-name characters instead of
  rewriting them to `/`, so storage metadata, user-typed paths, and Git paths
  compare exactly.
- A file above 10 MiB whose path contains a backslash no longer breaks every
  later `plan` and `publish`. Automatic and explicit S3 placement refuse such a
  path before the storage engine writes metadata it cannot address, and
  metadata left by earlier releases is reported with a `workspace-mgr move`
  recovery hint.

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
