# Control-plane and information-routing audit

This audit records the approved change boundary and the information locality
requirement for workspace-mgr 0.7.0. It is not authorization to remove every
repository-management rule that mentions task content.

## Approved boundary

Task contents belong to the user. workspace-mgr manages control correctness:
task identity and directory, authorized scopes, branch/PR association, storage
placement and version metadata, publication transactions, resource approval and
Git/S3 byte transport. It does not establish runtime or substantive payload
correctness.

A task README describing its purpose is a reasonable repository control
entrypoint. Existing README scaffolding and repository-management policies
remain in force. This change does not introduce README-link validation or alter
its required format, task records or publication policy.

The user approved two cleanups:

| Approved issue | Result | Code boundary |
| --- | --- | --- |
| Runtime validation or automatic repair during relocation | Move ordinary payloads unchanged. Do not parse or rewrite nested Git controls, environments or ordinary links to make them runnable after moving. | `relocation.rs`, `task_rename.rs`, `archive.rs`; old cancel snapshots remain readable. |
| Historical task-content proof as a lifecycle prerequisite | Do not require reviewed directory-tree transitions, earlier content imports or payload cleanliness to adopt/upgrade/archive. Current task identity, manifest compatibility and live PR/ref control checks remain. | `archive.rs`, `archive_adoption.rs`, `task_upgrade.rs`; compatible old review fields remain preserved. |

Nested Git exclusion remains a control rule: whole repositories must be ignored
by shared outer rules and contain no outer-tracked files or gitlinks. The tool
does not repair their internal or external administration.

A real successful relocation emits `manual-content-audit-after-relocation` in
its `notices` report, asking the user to manually inspect and repair links or
path references. This conditional reminder is not a preflight check. It does
not appear unconditionally in global instructions, command help, dry-runs,
no-change operations, metadata-only rename or archive cancellation.

## Policies retained

The user did not authorize removing the following policies. Information routing
must preserve their semantics and make them available at their operation:

- README/Directory map scaffolding and durable Markdown records.
- Publication documentation requirements and `task-record-unchanged` feedback.
- Whitespace and escaping-symlink publication checks.
- Published/shared ignore-rule provenance and product default ignore patterns.
- Artifact hygiene, task workplace and retention guidance.
- `bulk-publication` review feedback.
- Semantic storage choices, 1 MiB/10 MiB size bands and `small-s3-boundary` advice.
- Cloud-usage approval, pause, accounting and cleanup-only publication rules.
- Task authorization, shared checkout isolation, version declarations and PR
  ownership/synchronization requirements.

Repository content-management rules can serve the control plane. Mentioning a
payload is not by itself proof that a rule is an unauthorized content-quality
check. Future boundary changes require the user's decision rather than silently
reclassifying and deleting these policies.

## Information locality

Global instructions are the onboarding surface, not an operation manual.
Default `instructions` and explicit `instructions all` contain:

1. The short mental model: chat/task/branch/PR, scope, placement and shared checkout.
2. An operation directory naming the relevant command and `--help` entrypoint.
3. Genuinely session-wide constraints: ownership and authorization, isolation,
   managed-operation interface, user authority and publication synchronization.
4. Current repository control facts and an index to user-owned instructions.

Prerequisites, operation policies and procedures belong in command help.
Execution-specific facts, decisions and reminders belong in that operation's
execution output. A user deciding whether to archive should see archive's
current PR/storage/scope requirements there; an unrelated task session should
not repeatedly load full S3 migration and cancellation protocols.

The generated `AGENTS.md` bootstrap is a justified loading exception. Its
`BOOTSTRAP` literal in `src/instructions.rs` remains the narrow instruction to
run `workspace-mgr instructions --repo .`, plus the unavailable-tool recovery
path: request the user's installation approval, install the latest stable
release, run setup and retry. Command help cannot provide that recovery when
the binary is absent. The bootstrap bytes and installation-permission policy
are preserved; it does not become a second global operation manual. The
ordinary default output and operation help still follow information locality.

Detailed `instructions core|task|publish|artifacts|storage|shared-checkout|infrastructure`
remain on-demand compatibility views. `guidance.rs` preserves those detailed
sections. `command_guidance.rs` serves operation-local help without loading a
repository, credentials, external commands or network state.

| Information | Primary operation surface | Additional durable reference |
| --- | --- | --- |
| Mental model, ownership and operation discovery | Default/all instructions | `management-model.md` |
| Task creation, README/records, workplace and initial draft PR | `task create --help` | Detailed `instructions task` and `artifacts`; task-create reference |
| Task identity, name/path and explicit scopes | Task create/status/rename/adopt/upgrade help | Detailed task/infrastructure topics; configuration reference |
| Documentation, whitespace, symlink and ignore publication checks | `plan --help`, `publish --help` | Detailed publish/artifacts topics; publication architecture |
| Artifact hygiene, bulk and record feedback | Create/plan/publish help and affected plan reports | Detailed artifacts/publish topics; guide |
| Semantic placement and size bands | Storage leaf help and placement reports | Detailed storage topic; placement guide |
| Exact storage versions, transport and destructive retirement | Hydrate/move/remove/untrack/publish help and reports | Storage/transaction architecture |
| Cloud approval, pause and accounting | Plan/publish/approve help and blocking diagnostics | Detailed core/task/storage topics; cloud-usage architecture |
| Current closed PR, scoped grouping, complete S3 copy and cancel | Archive help and archive/cancel reports | Archive command reference and architecture |
| Manual link/path audit after an actual move | Successful relocation `notices` only | Output schema in command reference |
| Shared overlays and verified branch-ref cleanup | Refresh help and report | Detailed shared-checkout topic; refresh architecture |
| Product scaffold, client compatibility and update approval | Setup/manage/config/doctor help and relevant diagnostics | Detailed core topic; configuration/reference docs |

The user-owned `.workspace-mgr/instructions/repository.md` is indexed globally,
with an explicit requirement to read it before task work. `instructions
repository` reproduces its text on demand. Validated current module bytes still
contribute to `all`'s policy hash even though its body is no longer repeated in
the global document.
The same fingerprint also includes the instruction-policy version, unchanged
bootstrap, seven configuration-aware compatibility topics and all 29 rendered
command-help pages, preserving sensitivity to hidden policy changes without
adding them to the global output or performing extra I/O.

## Review and verification

Contracts cover compact default/all output, explicit detailed-topic reachability,
retained policy text, command-help routes, user-module access/hash sensitivity
and success-only relocation notices. Runtime/history regressions use isolated
repositories, synthetic review records and local/mock storage. No real user
bucket is part of acceptance testing.

The integration owner records actual test, CI and E2E outcomes after running
those checks. This audit does not claim that unrun checks passed.
