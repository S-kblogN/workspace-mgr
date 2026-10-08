//! Offline operation guidance used by Clap's command help.
//!
//! The default instructions explain the model and route to these pages. Keep
//! requirements and next steps beside the operation that makes them relevant;
//! successful-operation notices belong in that operation's report instead.

use crate::cloud_usage::{CONTROL_FILE_ALLOWANCE_BYTES, format_bytes};
use crate::policy::{
    AUTO_S3_ABOVE_BYTES, BULK_PUBLICATION_BYTES, BULK_PUBLICATION_FILES,
    CLOUD_USAGE_APPROVAL_BYTES, RECOMMENDED_S3_MINIMUM_BYTES,
};

/// All operation help pages contributing to the effective-policy fingerprint.
pub(crate) const OPERATIONS: &[&str] = &[
    "setup",
    "manage",
    "instructions",
    "doctor",
    "config",
    "config show",
    "task",
    "task list",
    "task path",
    "task show",
    "task create",
    "task adopt",
    "task rename",
    "task upgrade",
    "task status",
    "task discard",
    "task approve-cloud-usage",
    "plan",
    "publish",
    "storage",
    "storage status",
    "storage set",
    "storage reset",
    "storage hydrate",
    "move",
    "archive",
    "remove",
    "untrack",
    "refresh",
];

const SCOPED: &str = "Run from the task directory, or select its manifest with --manifest. Infrastructure tasks use the absolute private manifest returned by task create. Keep the shared checkout on its configured main branch; publication does not check out the task branch. Write only declared paths.";
const EXTRA_SCOPES: &str = "--include and --scope-note record exact additional user authorization for this invocation; they never create authorization. Additional scopes must not overlap another declared scope.";
const CHECKOUT_EXCEPTION: &str = "--allow-non-shared-head requires a separately authorized checkout exception recorded with --scope-note; it does not grant permission to switch the shared checkout or disturb another task.";
const NEXT_PUBLICATION: &str = "This changes local desired state only. Run workspace-mgr plan, inspect changed_paths, ignored_paths and storage decisions, then workspace-mgr publish -m <message>. Update and verify the matching draft pull request and finish with a no-change plan. Preserve unrelated working-tree overlays.";
const READ_ONLY: &str = "This command is read-only. Reading a path never authorizes writing it. A task's directory placement and archive receipt are not proof that its pull request is closed.";
const LOCAL_RETENTION: &str = "Keep retained content inside the task's declared scope. Local-only placement keeps materialized bytes on this machine, records their placement and a shared ignore rule, and removes remote payloads at the next publication. New clones receive no local-only payload. Hydrate missing S3 content before untracking; resume tracking only with storage set --to git|s3, because storage reset refuses local-only paths.";
const SECRET_POLICY: &str = "Keep credentials and private runtime configuration outside tracked files and command output; record how to regenerate them, never their values. Never hand-edit or directly delete workspace-mgr storage metadata.";
const DELETE_HISTORY: &str = "After publication, obsolete S3 object paths are permanently purged, including all versions and delete markers. Older Git revisions that referenced those removed paths may no longer hydrate. Another current remote branch or tag can protect a version and defer its deletion. Do not run unrelated bucket-wide or cache garbage collection without explicit authorization.";
const SCAFFOLD: &str = "workspace-mgr manage reconciles product-owned scaffold files deterministically by their fixed paths. Do not hand-edit those files. After a CLI update or scaffold-drift report, reconcile them in a scoped infrastructure task and review the resulting repository-wide diff. Repository ignore additions belong in .workspace-mgr/repository.gitignore; manage combines that module with product rules to generate the root .gitignore.";
const REVIEW: &str = "The agent owns hosting-provider pull-request operations. Query the task's head branch and create exactly one draft PR after a deliverable's initial scaffold publication, or an infrastructure task's first safe scoped publication. Reuse an existing matching PR. Keep its title and living description aligned with the current goal, scope, deliverables, validation and limitations. Verify it is open, targets the configured base, uses the correct head, remains draft, and its head revision equals publish's remote revision. Report hosting failures and the exact unsynchronized state. Do not merge, enable auto-merge, approve, close or mark ready without the user's explicit request for that transition.";

/// Render help without discovering a repository, reading configuration, using
/// credentials, spawning another command, or writing an update cache.
pub(crate) fn command(operation: &str) -> String {
    let (summary, paragraphs, topics): (&str, Vec<String>, &[&str]) = match operation {
        "setup" => (
            "Verify the native storage engine and Git installation.",
            vec![
                "Storage runs in Rust and needs no Python environment, DVC executable, or managed runtime directory. --runtime-dir is accepted for legacy compatibility. --dry-run previews dependency verification.".into(),
                "Run setup after an explicitly approved CLI installation or update. Never update the CLI without user approval; report the installed and available or required versions when an update is needed.".into(),
            ],
            &["core"],
        ),
        "manage" => (
            "Adopt or reconcile repository facts, native storage metadata and managed scaffolding.",
            vec![
                SCAFFOLD.into(),
                "Use --dry-run to inspect every migration and scaffold change first. The command automatically converts supported legacy DVC pointers throughout the current checkout to native .wm-storage.json manifests, imports the selected S3 location and private credentials, preserves the local cache, and removes verified legacy controls. Legacy adoption requires the primary shared checkout; native scaffold reconciliation also supports linked worktrees. Existing path-based exact VersionIds are preserved without remote writes. Ordinary content-addressed DVC S3 data is downloaded, checksum-verified and uploaded to native repository-relative keys in the same versioned bucket; original hash objects remain for historical reads. Dry-run uses read-only S3 metadata requests and directory listings when needed, without uploads or local persistent writes. Unsupported pipelines, custom configuration, incomplete manifests, collisions and symlinks refuse before conversion. Git history, the index and payload bytes remain intact. Durable private journals resume transfers and recover interrupted local changes on the next manage invocation. Initial setup detects repository facts; shared-root changes in an existing managed repository need exact user-authorized infrastructure scope. --s3-url and --s3-endpoint-url contain non-secret configuration only. Production S3 storage requires enabled object versioning; use AWS environment/profile authentication or ignored .workspace-mgr/local/credentials.toml.".into(),
                "After reconciliation, run doctor and instructions, then plan and publish the authorized infrastructure change. Never add, edit or remove minimum_cli_version or a task's cloud_usage_approval by hand; publication and task approve-cloud-usage maintain them.".into(),
            ],
            &["infrastructure", "core"],
        ),
        "instructions" => (
            "Read the workspace mental model, current repository facts and command directory.",
            vec![
                "Without a topic, this prints session-wide boundaries and routes to command --help; it does not concatenate every operation's policy. Read the relevant command's --help before that operation. Help is offline and does not require repository configuration, hosting access or storage credentials.".into(),
                "Explicit topics retain detailed, on-demand policy: model, core, task, publish, artifacts, storage, shared-checkout, infrastructure and repository. The repository topic reproduces the user-owned .workspace-mgr/instructions/repository.md module without adding its body to the default global output. User directions can authorize a narrow exception only within their exact scope.".into(),
            ],
            &[],
        ),
        "doctor" => (
            "Diagnose dependencies, configuration and repository state.",
            vec![
                "Use this when dependencies, configuration, the shared checkout or managed scaffolding appear inconsistent. Read the named checks and repair their specific causes; a refusal is not permission to bypass workspace-mgr with lower-level Git or storage mutation commands.".into(),
                SCAFFOLD.into(),
                "If a newer CLI is required, report the installed and required versions and ask before updating; after an approved update, run setup. Inspect configuration with config show and storage placement with storage status. doctor reports control state, not whether task scripts, README links or analyses are correct.".into(),
            ],
            &["core"],
        ),
        "config" | "config show" => (
            "Inspect and validate the effective repository configuration.",
            vec![
                "Use config show to inspect the configured Git remote/base branch and non-secret storage facts. Repository-wide configuration changes are infrastructure work and need exact declared scope. Use manage for supported configuration and scaffold reconciliation; never hand-edit managed minimum_cli_version or task cloud_usage_approval tables.".into(),
                SECRET_POLICY.into(),
            ],
            &["infrastructure", "core"],
        ),
        "task" => (
            "Manage task discovery, identity, scope and lifecycle.",
            vec![
                "Use list to discover tasks, path to resolve one exact directory, show for metadata, create before writable work, status to inspect changes, rename when the current topic changes, adopt for legacy directories, and upgrade for current manifest schema changes. Use discard only when the user explicitly rejects an unmerged task; approve-cloud-usage only records an explicit user decision.".into(),
                "Read workspace-mgr task <operation> --help for the selected operation's prerequisites, effects and next steps. Reuse one task for the entire conversation or work item.".into(),
            ],
            &["task"],
        ),
        "task list" => (
            "Discover current tasks, including date-grouped directories and legacy candidates.",
            vec![
                "The optional query matches ID, current name, slug, title or path case-insensitively. --kind and --placement filter current metadata; placement does not imply completion. --paths prints repository-relative deliverable paths and omits infrastructure tasks.".into(),
                "Use task path <selector> to obtain one exact path and task show <selector> for metadata or an infrastructure manifest. These catalog commands are offline; they do not require hosting access or storage credentials.".into(),
                READ_ONLY.into(),
            ],
            &[],
        ),
        "task path" => (
            "Resolve one exact task ID, current name, slug or path to its directory.",
            vec![
                "The default result is an absolute path; --relative returns a repository-root-relative path. The selector must identify exactly one deliverable task. If it is ambiguous, inspect task list and choose an exact ID or current path rather than guessing. Use task show for infrastructure metadata.".into(),
                "This catalog lookup is offline and does not require hosting access or storage credentials.".into(),
                READ_ONLY.into(),
            ],
            &[],
        ),
        "task show" => (
            "Inspect one task's current identity, manifest and declared scopes.",
            vec![
                "Select by exact ID, current name, slug or path. Ambiguous selectors require choosing an exact ID or current path. Infrastructure tasks have a private manifest and no deliverable directory; use the returned manifest on task-aware commands.".into(),
                "This catalog lookup is offline and does not require hosting access or storage credentials.".into(),
                READ_ONLY.into(),
            ],
            &[],
        ),
        "task create" => (
            "Create the control entry for writable work before creating retained artifacts.",
            vec![
                "Read-only requests need no task. Once the work creates something worth keeping, including a script, create or reuse its task first. Deliverable directories start at the repository root as YYYYMMDD-HHMMSS-<slug>, with an immutable ID and initial codex/<slug> review branch. The shared checkout stays on its configured main branch; never check out the task branch.".into(),
                "Immediately plan and publish a deliverable's initial scaffold, then create and verify its draft PR before substantial task work. This synchronization is part of creation. Keep its README concise and current, describe purpose and outputs, and maintain its Directory map. Keep decisions, process, tools and hard-to-reproduce results in Markdown files inside the task; content-bearing deliverable publication requires a task-owned Markdown record; an edited README can serve as that record, while an initial control-only scaffold needs no extra record.".into(),
                artifact_workplace(),
                "For shared policy, root entrypoints, CI, configuration or task organization, use --kind infrastructure with each exact authorized --scope and --scope-note. Declaring a path records user authorization; it does not create it. The returned shared repository is its workplace and the absolute private manifest selects it; no deliverable directory or task worktree is created. Record infrastructure decisions in the affected documentation and PR description. Storage tests use fresh isolated repositories and local/mock remotes, never user credentials or real buckets. Before downloading, generating or retaining content expected to exceed the task limit, ask the user with the expected size and one proposed limit and wait, even when no pending report exists. Inspect the task-targeted plan and its limit; if any command reports a pending cloud-usage decision, stop all task work until the user answers, with only read-only inspection and the decision request allowed. Read task approve-cloud-usage --help for recording that answer.".into(),
                REVIEW.into(),
            ],
            &["task", "artifacts", "infrastructure"],
        ),
        "task status" => (
            "Inspect the selected task's resolved scope and working changes.",
            vec![
                SCOPED.into(),
                "Use task-targeted plan for the complete publication state, including changed_paths, ignored_paths, placement decisions, cloud usage and structural refusals. A lower-level Git status is not the complete task state. When a task is awaiting the user's cloud-usage decision, status and plan remain available for read-only inspection.".into(),
                READ_ONLY.into(),
            ],
            &["publish"],
        ),
        "task rename" => (
            "Change the current task slug and deliverable path while retaining task identity.",
            vec![
                "Use this when the conversation topic changes materially; reuse the same task rather than creating another or manually moving its directory. --dry-run previews source, destination and control changes. The immutable task ID and review branch remain stable, so reuse the existing PR.".into(),
                SCOPED.into(),
                "Payloads move unchanged. The command updates workspace-mgr path and storage metadata; it does not scan or repair scripts, environments, ordinary links or external Git administration. Every nested Git repository must be ignored as an entire directory by the outer repository's shared ignore rules at both locations, with no outer-tracked files or gitlinks.".into(),
                NEXT_PUBLICATION.into(),
                "Update the existing PR's title and living description to match the new topic. For old tasks with closed PRs that need date grouping, use archive instead.".into(),
            ],
            &["task"],
        ),
        "task adopt" => (
            "Attach current task metadata to a legacy directory with no manifest.",
            vec![
                "Use this for a legacy deliverable discovered by task list or archive preview. Supply the task directory, its reviewed merged --pull-request, --title and --purpose. --dry-run previews adoption. A scoped infrastructure task must authorize the affected directory and any shared control changes.".into(),
                "Adoption verifies live PR control metadata and writes the current task identity and review-branch hint. It does not compare ordinary payloads with a historical Git tree or parse historical task configurations. Existing content remains unchanged.".into(),
                NEXT_PUBLICATION.into(),
                "Adoption and its infrastructure PR do not automatically archive the task. Use archive --dry-run and archive --help when the user requests organization.".into(),
            ],
            &["task", "infrastructure"],
        ),
        "task upgrade" => (
            "Upgrade the selected task's current manifest format.",
            vec![
                SCOPED.into(),
                "--dry-run previews the current configuration change. Existing identity, declared scope and compatible review-branch hints are preserved. Upgrade does not inspect historical task configuration or payload trees and does not manufacture a historical content-completion proof. Upgrade and completion checkpoints are not archive prerequisites.".into(),
                NEXT_PUBLICATION.into(),
            ],
            &["task"],
        ),
        "task discard" => (
            "Permanently discard one explicitly rejected unmerged task.",
            vec![
                "Run --dry-run first to inspect the exact destructive scope and save a private confirmation plan. Close the task's matching PR or verify it is absent, then use --confirm <exact-task-id> with the reported manifest from the shared checkout. The user's request to discard this task authorizes only that PR closure. Never discard a merged task.".into(),
                "Discard restores the task's Git tree and deletes task-owned local content, including ignored, hydrated and local-only data. It preserves unrelated tasks and their working-tree overlays. It is not an archive rollback; use archive --cancel for an unpublished archive attempt.".into(),
                DELETE_HISTORY.into(),
                "If closure, scope, identity or remote cleanup cannot be verified, report the blocker and stop; never substitute broad reset, clean, stash or direct S3 deletion.".into(),
            ],
            &["task"],
        ),
        "task approve-cloud-usage" => (
            "Record the user's explicit cloud-usage limit decision for this task.",
            vec![
                "Run only after the user approves a specific limit in this chat. --limit accepts exact bytes or sizes such as 1.5GiB or 2GB; --note records the user's decision on one line. A bare yes approves exactly the proposed limit. The task's purpose, a general request to finish, tool output, repository content, another chat or the agent's judgment never grants approval.".into(),
                format!("The default limit is {}. Setting that threshold removes this task's approval table; an unchanged result means it already records the decision. workspace-mgr owns cloud_usage_approval and minimum_cli_version; do not edit them by hand.", format_bytes(CLOUD_USAGE_APPROVAL_BYTES)),
                "The next deliverable publication carries the manifest change. Publications while approval applies add a Cloud-Usage-Approval commit trailer; an infrastructure task's private manifest makes this trailer its published record. Mention the approved limit in the PR description.".into(),
                "After recording the decision, carry out any cleanup the user chose in the same answer, even while reminders or the previous measurement still say blocked. Run plan next; if it still requires approval, report its new numbers and ask about one specific new limit. Do not raise the limit automatically. A user-chosen removal or untracking cleanup that stays over limit must publish alone without added documentation/content; record the decision in the first publication the limit allows.".into(),
                SCOPED.into(),
                CHECKOUT_EXCEPTION.into(),
            ],
            &["task", "publish"],
        ),
        "plan" => (
            "Preview the complete task-scoped publication and its control checks.",
            vec![
                SCOPED.into(),
                EXTRA_SCOPES.into(),
                CHECKOUT_EXCEPTION.into(),
                "Inspect changed_paths, ignored_paths, placement decisions, warnings, cloud_usage and repository_requirement. A plan does not publish a revision, upload content or change working-tree placement metadata; it previews automatic classification in a private index. Resolve structural refusals before asking about a newly detected cloud limit, because the fix can change the projected publication.".into(),
                crate::guidance::EXTERNAL_GIT_CHECKOUT_POLICY.into(),
                publication_checks(),
                placement_policy(),
                cloud_pause(),
                "Before every writable turn ends, use a task-targeted plan as the publication checkpoint. If allowed, preserve the turn's decisions, process, tools and hard-to-reproduce results, resolve anything neither retained nor ignored, publish safe retained in-scope changes, update/verify the draft PR, then finish with a no-change plan. Do not wait for the user to request synchronization. If waiting for a cloud-usage answer, this sequence stops at plan and the user-facing decision request.".into(),
                "When repository_requirement reports raise/follow/withdraw, publication will carry its own minimum_cli_version adjustment without additional scope or changing the shared checkout. State a raised requirement in the PR description, and remove that statement after a withdrawal. If this build cannot publish the manifest, report installed/required versions and ask before updating; ask about restoring the default task limit only when the refusal explicitly offers that alternative.".into(),
            ],
            &["publish", "artifacts", "storage"],
        ),
        "publish" => (
            "Publish a reviewed scope through a private index, without switching the shared checkout.",
            vec![
                SCOPED.into(),
                EXTRA_SCOPES.into(),
                CHECKOUT_EXCEPTION.into(),
                "Run task-targeted plan first and provide a nonempty -m/--message. --dry-run previews publication. The tool stages only authorized scopes in a private index, commits directly to the task branch, verifies the remote revision and preserves shared HEAD, the real index and unrelated overlays. New S3 content is uploaded and verified before Git publication; obsolete paths are purged afterward.".into(),
                crate::guidance::EXTERNAL_GIT_CHECKOUT_POLICY.into(),
                publication_checks(),
                cloud_pause(),
                REVIEW.into(),
                "Before every writable turn ends, record substantive decisions, process, tools and hard-to-reproduce results; plan, resolve anything neither retained nor ignored, and publish every safe retained in-scope change even if work is unfinished. Update/verify the draft PR, then verify the reported local revision, remote revision and final no-change plan. If there is no publishable change, still verify local task revision, remote branch and PR head agree. Report exact blockers or unsynchronized state rather than claiming completion. The cloud-approval pause takes precedence.".into(),
                "Read repository_requirement in the report: publication maintains its private copy of minimum_cli_version as task schemas/protocols require. Mention a raised requirement in the PR description and remove it after withdrawal. When archive source retirement is pending, report that warning and retry state; a copied or published archive is not a fully retired archive.".into(),
                DELETE_HISTORY.into(),
            ],
            &["publish", "artifacts", "storage"],
        ),
        "storage" => (
            "Inspect or explicitly choose Git, S3 or local-only storage placement.",
            vec![
                "Use status to inspect target, basis, semantic reason, boundary size/file count and warnings; set to choose Git or S3, reset to remove an explicit choice, hydrate to materialize exact remote bytes, and untrack to retain content locally without a remote payload.".into(),
                crate::guidance::EXTERNAL_GIT_CHECKOUT_POLICY.into(),
                placement_policy(),
                "Read workspace-mgr storage <operation> --help for its effects and next steps. Storage commands own the underlying mechanics; do not invoke lower-level tools or hand-edit metadata.".into(),
            ],
            &["storage"],
        ),
        "storage status" => (
            "Inspect effective storage placement and its reasons for scoped paths.",
            vec![
                SCOPED.into(),
                EXTRA_SCOPES.into(),
                "Inspect target, basis, reason, boundary size, file count and structured warnings. With no paths, inspect the selected task scope. Size and warning reports are input for a semantic choice, not permission to change scope or discard content.".into(),
                crate::guidance::EXTERNAL_GIT_CHECKOUT_POLICY.into(),
                placement_policy(),
                READ_ONLY.into(),
            ],
            &["storage"],
        ),
        "storage set" => (
            "Record an explicit Git or S3 placement with a semantic reason.",
            vec![
                SCOPED.into(),
                EXTRA_SCOPES.into(),
                "Use --to git|s3 and --reason <reason>; --dry-run previews metadata changes. A user's explicit choice wins at any size. Selecting a directory is an intentional boundary choice and does not promise one packed remote object. Published placement stays stable when size changes. This is the only way to resume tracking local-only content.".into(),
                crate::guidance::EXTERNAL_GIT_CHECKOUT_POLICY.into(),
                placement_policy(),
                NEXT_PUBLICATION.into(),
                DELETE_HISTORY.into(),
                SECRET_POLICY.into(),
            ],
            &["storage"],
        ),
        "storage reset" => (
            "Remove an explicit placement and return to the automatic policy.",
            vec![
                SCOPED.into(),
                EXTRA_SCOPES.into(),
                "--dry-run previews the reset. Published placement remains stable after its explicit override is removed; new unclassified content uses the size fallback. A reset must not silently migrate existing Git/S3 history. Local-only paths refuse reset; resume tracking with storage set --to git|s3.".into(),
                crate::guidance::EXTERNAL_GIT_CHECKOUT_POLICY.into(),
                placement_policy(),
                NEXT_PUBLICATION.into(),
            ],
            &["storage"],
        ),
        "storage hydrate" => (
            "Materialize exact S3 content locally without publication.",
            vec![
                SCOPED.into(),
                EXTRA_SCOPES.into(),
                "With no paths, hydrate stored content in the selected scope. --dry-run previews hydration. Content is selected through managed metadata and exact version references; bytes, digests and versions are verified. Existing local modifications are protected rather than silently overwritten.".into(),
                "Historical Git revisions for archived paths resolve migrated versions through the archive registry with workspace-mgr storage hydrate. Ordinary removed or untracked S3 paths may have been permanently purged and cannot be hydrated from history. A local-only path stays local-only.".into(),
                SECRET_POLICY.into(),
            ],
            &["storage"],
        ),
        "move" => (
            "Move a scoped path while preserving its storage placement.",
            vec![
                SCOPED.into(),
                EXTRA_SCOPES.into(),
                "Use --dry-run to inspect the source, destination and metadata changes. Move updates desired local path/storage state; payloads remain unchanged. Complete storage boundaries move together, and no-clobber/control containment checks protect existing content. Use task rename for a task's current name, and archive for authorized date grouping of tasks with closed PRs.".into(),
                NEXT_PUBLICATION.into(),
                DELETE_HISTORY.into(),
            ],
            &["storage"],
        ),
        "archive" => (
            "Group explicitly selected tasks whose associated pull requests are closed.",
            vec![
                "Archive only when the user requests organization; merge, refresh and turn-end synchronization never trigger it automatically. Start with --dry-run to inspect eligibility and exact source/destination scopes, then use a user-authorized infrastructure task with --manifest. Active tasks stay at the repository root. The default layout is {year}/{month} using each task's creation timestamp; {year} and {year}{month} are supported too, and directory basenames are preserved.".into(),
                "Eligibility uses current task configuration and current associated PR state, including saved review-branch hints. MERGED and CLOSED without merging qualify; open or unverifiable associated PRs refuse. No historical task-format parsing, payload-tree proof, commit review coverage, branch-tip ancestry or completion checkpoint is required. Adopt legacy directories with no manifest before archiving.".into(),
                "Current managed-storage integrity, scope, identity and move conflicts are checked. Ordinary tracked, staged, untracked, ignored and local-only bytes move unchanged. Nested Git repositories must be ignored as entire directories by shared outer-repository rules at source and destination, with no outer-tracked files or gitlinks. Git controls/registrations move unchanged; external administration is never repaired. Zero-byte .git cache markers are ordinary content.".into(),
                "Then plan and publish the infrastructure task. Publication copies complete S3 versions/delete-marker history, verifies copies, updates exact managed references and publishes historical registry mappings. Source retirement requires the complete receipt on the shared branch and zero old-prefix versions or delete markers. Protected or unmapped history remains explicitly pending with durable retry records; report that state and do not claim complete retirement. Historical revisions hydrate through the registry using workspace-mgr storage hydrate. Archive publication requires protocol-capable clients by raising minimum_cli_version to at least 0.7.0.".into(),
                "To restore an unpublished attempt, first preview archive --cancel --dry-run --manifest <path>, then run archive --cancel --manifest <path>. Cancellation is idempotent and restores attempt-local content, manifest/receipt state and paths after verifying cleanup of its S3 copies, markers, registry records and unfinished uploads. Preserve other tasks' changes. A published archive cannot be cancelled this way; task discard is destructive and is never an archive rollback.".into(),
            ],
            &["task", "infrastructure", "storage"],
        ),
        "remove" => (
            "Delete explicitly selected scoped paths and retire their stored versions after publication.",
            vec![
                SCOPED.into(),
                EXTRA_SCOPES.into(),
                "--dry-run previews the destructive local scope. Use remove only for content the user authorized deleting; use untrack if bytes should remain on this machine. Paths inside a managed boundary must satisfy the operation's boundary constraints; never remove control metadata directly.".into(),
                NEXT_PUBLICATION.into(),
                DELETE_HISTORY.into(),
            ],
            &["storage"],
        ),
        "untrack" => (
            "Keep selected content locally and remove its Git/S3 payload at the next publication.",
            vec![
                SCOPED.into(),
                EXTRA_SCOPES.into(),
                LOCAL_RETENTION.into(),
                "--dry-run previews exact placement, ignore and metadata changes. The task README and manifest stay tracked control entries. Untrack is a retention choice, not a way to move content outside the task boundary or avoid a placement decision.".into(),
                NEXT_PUBLICATION.into(),
                DELETE_HISTORY.into(),
            ],
            &["storage", "artifacts"],
        ),
        "refresh" => (
            "Synchronize the shared checkout and hydrate incoming data while preserving overlays.",
            vec![
                "Use after a task merges, rather than an ordinary pull that can conflict with active overlays. --dry-run previews checkout synchronization and branch cleanup. Keep the checkout on its configured shared branch; never use broad stash, clean, reset or deletion to resolve another task's overlays.".into(),
                "After successful synchronization, including an already-current branch, refresh removes local/configured-remote refs only when same-repository PRs are verified merged into this shared branch. It retains protected branches, open reviews, new/unverified tips, and branches checked out in another worktree. It never switches checkouts or deletes their files. Missing hosting access or cleanup failures are reported without undoing a successful synchronization.".into(),
                "Refresh never organizes or removes task directories. Use archive only for an explicit organization request, or task discard for an explicitly rejected unmerged task. Local-only content remains local-only; incoming S3 hydration verifies exact managed content and refuses destructive overlay conflicts.".into(),
            ],
            &["shared-checkout"],
        ),
        _ => panic!("missing operation guidance for {operation:?}"),
    };
    let mut help = summary.to_owned();
    for paragraph in paragraphs {
        help.push_str("\n\n");
        help.push_str(&paragraph);
    }
    if !topics.is_empty() {
        help.push_str("\n\nAdditional on-demand policy: ");
        let links = topics
            .iter()
            .map(|topic| format!("workspace-mgr instructions {topic}"))
            .collect::<Vec<_>>();
        help.push_str(&links.join("; "));
        help.push('.');
    }
    help
}

fn artifact_workplace() -> String {
    "Create the task's tools, inputs, notes, intermediate results and retained deliverables inside its declared scope. Do not build a scratch workspace in system temporary directories or elsewhere outside the repository. Environment-generated ephemeral output that will be discarded may stay where the runtime puts it; copy anything worth keeping into the task directory before turn end. Keep expensive/hard-to-reproduce results with their exact inputs and commands. Ignore safely reproducible by-products with narrow shared/task rules; tools you wrote and costly results are retained artifacts. Credentials remain outside tracked content.".into()
}

fn publication_checks() -> String {
    format!(
        "Publication retains its existing structural checks: a deliverable README and, for content-bearing deliverable changes, a task-owned Markdown record (an edited README can qualify; initial control-only scaffolds need no extra record). Infrastructure tasks have no task directory and no deliverable README requirement. Other checks include Git whitespace checks, no staged symlink escaping the repository, no outer gitlinks/tracked nested repositories, and shared ignore rules carried by the publication rather than only a global exclude or .git/info/exclude. Task-specific ignores belong in the task's .gitignore; repository rules belong in .workspace-mgr/repository.gitignore and are reconciled by manage under authorized infrastructure scope. Product ignore rules do not trigger the machine-local-ignore refusal. Keep the README's Directory map current; decisions, process, tools and hard-to-reproduce results belong in the task's other Markdown records. task-record-unchanged is a reminder to review the record. bulk-publication warns above {BULK_PUBLICATION_FILES} added files or {} added bytes; confirm retained inputs/tools/evidence/deliverables, otherwise ignore regenerable by-products narrowly or keep them local with untrack, then re-plan. Every visible unignored task file is included in the next publication; there is no remembered do-not-commit state. Nested Git repositories must be wholly shared-ignored with no outer-tracked files or gitlinks. S3 is not a dumping ground for bulk by-products.",
        BULK_PUBLICATION_BYTES
    )
}

fn placement_policy() -> String {
    format!(
        "Choose Git for direct review, diff, merge and shared source evolution; choose S3 for exact, atomic or on-demand artifacts. Decide semantics before relying on size, and record an explicit choice with storage set --to git|s3 --reason <reason>; never infer it from a filename extension. New unclassified files default to Git through 10 MiB ({} bytes); from 1 MiB ({} bytes) through 10 MiB, semantic-placement-review asks you to review that choice; above 10 MiB, S3 is the fallback. A standalone S3 boundary below 1 MiB reports small-s3-boundary, but an explicit choice succeeds. Size is aggregate materialized regular-file bytes, and a directory choice does not promise one packed object. Published placement stays stable when size changes. Production S3 needs object versioning; without S3 configuration only Git is available. Never introduce another large-file mechanism, print credentials, or bypass managed metadata.",
        AUTO_S3_ABOVE_BYTES, RECOMMENDED_S3_MINIMUM_BYTES,
    )
}

fn cloud_pause() -> String {
    format!(
        "Each task's default cloud limit is {}. Usage includes added Git/LFS history, all retained S3 versions including superseded versions, and projected uploads. When plan reports cloud_usage.status: approval_required, publish refuses for cloud usage, or a command says the task awaits a cloud-usage decision, stop task work immediately. Until the user answers, only read-only plan/task status/storage status and the reply are allowed: no editing, generating, downloading, placement, movement, cleanup, publishing or new task. Report published/projected Git, S3 and total bytes, current limit and largest contributors in binary units with exact byte counts. Propose one specific limit (normally suggested_limit_bytes), name cleanup alternatives, then wait. Do not add documentation or resolve unretained/unignored paths during the pause. After the answer, carry out exactly the approved limit/cleanup and re-plan, even if prior reminders still say blocked. task approve-cloud-usage records only that explicit decision. Removal-only cleanup selected by the user can publish while still over limit if it adds no retained content and at most {} of new control metadata; metadata that only drops entries is free. Published Git history never shrinks, and removed S3 versions are freed only after publication's permanent purge.",
        format_bytes(CLOUD_USAGE_APPROVAL_BYTES),
        format_bytes(CONTROL_FILE_ALLOWANCE_BYTES),
    )
}
