# Changelog

All notable changes to this project will be documented in this file. The format
is based on Keep a Changelog, and this project follows Semantic Versioning.

## [Unreleased]

### Added

- Initial standalone Rust CLI project.
- Repository initialization, effective instructions, diagnostics, and task
  scaffolding.
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
- Bidirectional Git/S3 placement transitions, ordinary Git refresh
  materialization, repository-wide operation locking, and tracked-input
  hardening.
- First-class infrastructure tasks with explicit shared scopes, private task
  metadata, isolated worktrees, private storage-state handoff, and scoped
  publication without timestamped repository task directories.
- A provider-neutral review policy and publication handoff that assigns
  pull-request creation, living metadata, verification, and merge authority
  while keeping hosting-provider API calls outside the CLI.

### Fixed

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
