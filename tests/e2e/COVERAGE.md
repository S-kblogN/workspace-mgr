# End-to-end coverage contract

This matrix is the audit boundary for `workspace-mgr`. “System E2E” means the
compiled CLI is treated as an opaque program, Git traffic crosses a real
`git://` server, and stored content crosses the S3 API to a versioned MinIO
bucket. It is not a count of test cases: each row names the state transition or
failure invariant that must remain covered.

Small input-validation permutations and deterministic fault injection stay in
isolated integration or unit tests. Moving those cases into the system scenario
would make the network lifecycle slower without proving another boundary.

| Public behavior or invariant | System E2E coverage | Isolated coverage retained for variants |
| --- | --- | --- |
| Private runtime setup | Dry-run, real isolated installation of the pinned storage runtime, explicit ownership marker, executable verification, and idempotent reuse | Unmanaged-target refusal, failed replacement rollback, and concurrent setup lock |
| Repository initialization | Dry-run, full scaffold, unmanaged `AGENTS.md` refusal with no partial files, idempotency, and managed-config drift detection and repair | Partial engine failure rollback, pre-existing unmanaged internal config, removal of unused S3 configuration, and unsafe URLs |
| Effective policy | Every instruction topic, model-before-rules ordering, constrained repository-content module composition, human and JSON output, stable policy hashes, and fixed agent/user pull-request responsibilities | Documentation vocabulary and fixed-policy contract |
| Configuration and diagnostics | Public configuration contains exactly Git and S3 facts, exposes no strategy switches, hides the private engine, and cannot relocate existing S3 boundaries; doctor passes with versioned S3 and fails when versioning or managed config is unhealthy | Strict schema, URL, policy-key rejection, and release-build filesystem-remote validation |
| Deliverable task scaffold | Dry-run, creation, duplicate refusal without clobbering, manifest discovery from the task and explicit path, branch initialization, README, and shared checkout staying on `main` | Required identity fields, exact ID/branch mapping, cross-clone branch ownership, invalid slug/timestamp, and missing-README publication guard |
| Exceptional checkout | Unexpected checkout is refused; the explicit `--allow-non-shared-head` plus one-line authorization can plan | Target branch mounted in another worktree and malformed authorization text |
| Infrastructure task scaffold | Dry-run, private manifest, isolated worktree, no task directory, exact declared scopes, undeclared-change refusal, shared private cache/config links, task-aware storage status, Git plus S3 publication, and a clean worktree | Invalid/missing infrastructure scopes and explicit-manifest lookup from another checkout |
| Scope authorization | Default task-only publication, unrelated overlay exclusion, one-shot root scope requiring a reason, authorization trailer, and infrastructure out-of-scope refusal | Overlapping-scope refusal, symlink traversal, nested Git checkout/gitlink, multiple include normalization, and path parser permutations |
| Git-only publication | `plan`, `publish --dry-run`, first publish, repeat no-op, network ref verification, Unicode/space paths, ordinary Git move, and review handoff | Explicit Git directory inheritance, sticky published Git placement, identity/message validation, and private-index details |
| Transaction concurrency | A separate process holding the common repository lock blocks another transaction and release resumes normally | Cross-command lock variants and per-storage-boundary lock naming |
| Explicit and automatic placement | File and directory S3 boundaries, large-file automatic S3, explicit large-file Git override, status explanations, reset stickiness, Git-to-S3, and S3-to-Git | Multi-path local conversion rollback, threshold boundaries, recursive explicit Git, and missing S3 configuration |
| Boundary integrity | Descendant inheritance plus nested set/reset refusal without remote changes | Symlink escapes and additional parent/child boundary permutations |
| Version-aware S3 | Enabled-bucket precondition, exact version IDs in metadata, exact object verification, immutable updates, directory additions/deletions, and retained old versions | Metadata shape/ownership validation and path-bound version removal during move |
| Hydration | Targeted and scope-wide hydration from an empty cache, exact file/directory versions, fresh-clone hydration, dry-run, local-modification refusal, and missing exact-version failure | Invalid targets and local-filesystem remote variants |
| Publish failure ordering | Bad S3 credentials cannot move either local or remote Git refs; Git server rejection occurs only after verified S3 upload, leaves a retryable local commit, and succeeds on retry; missing local stored output is not deletion | Multi-path local metadata rollback and adapter error rendering |
| Credential hygiene | Virtual credentials come only from private environment/config, are not staged, and deliberately failed S3 operations do not echo them | Credential-bearing tracked URL rejection |
| Shared-checkout refresh | Dry-run, staged-index refusal, provider failure before ref advance, ordinary add/modify/delete, tracked and untracked overlays, S3 prefetch, induced post-ref checkout failure with full rollback, success, and idempotency | Incoming storage symlink traversal, file/directory type transitions, unresolved-index variants, and temporary-worktree cleanup fault injection |
| Refresh ancestry | A real network force-push to a divergent `main` is rejected without moving local `main`, the index, or overlays | Deterministic ancestry edge cases |
| Pull-request ownership | The CLI returns a provider-neutral handoff requiring one draft PR, agent-managed metadata, and user-only merge authority | Actual GitHub PR creation/update is intentionally agent-owned and outside the CLI/system E2E boundary |
| Platform and packaging | The same source is built and tested on Linux ARM64 and Apple Silicon; the Linux system job exercises network Git and versioned S3 | Native archive/installer tests and Rust 1.85 compatibility |

## Deliberate external boundaries

- Tests never contact the real B2/S3 account, read developer credentials, or
  delete real remote objects. Provider faults are exercised only against the
  disposable versioned MinIO bucket.
- The CLI does not call GitHub or another hosting API. The E2E checks the review
  handoff contract; the responsible agent owns PR title, description, state,
  head verification, and the rule that it must not merge.
- The service-backed lifecycle runs once on Linux. Apple Silicon and Linux ARM64
  run the complete isolated Rust/integration suite and native packaging checks;
  duplicating the Docker-backed MinIO lifecycle on every architecture would not
  cross a new product boundary.

When a public command, state transition, or remote failure point is added, this
matrix and either `run.py` or the named isolated coverage must change in the same
pull request.
