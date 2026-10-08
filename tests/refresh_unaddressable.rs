// Every case here turns on a repository path containing a backslash, which is
// an ordinary file-name character only on Unix, and on an isolated filesystem
// storage remote.
#![cfg(all(unix, feature = "test-storage"))]

mod common;

use std::path::{Path, PathBuf};

use common::*;
use md5::Digest;

const TIMESTAMP: &str = "20260920-090000";
const TASK_ID: &str = "20260920-090000-refresh-unaddressable";
const FIRST_PAYLOAD: &[u8] = b"the first boundary, hydrated before the fixture branches\n";
const SECOND_PAYLOAD: &[u8] = b"the second boundary, held only by the storage remote\n";
const UNADDRESSABLE_PAYLOAD: &[u8] = b"the payload nothing can address\n";

struct SharedBranch {
    fixture: GitFixture,
    /// The revision the crafting checkout published to the shared branch.
    incoming_oid: String,
}

impl SharedBranch {
    fn task(&self) -> PathBuf {
        self.fixture.shared.join(TASK_ID)
    }
}

fn unaddressable_boundary() -> String {
    format!("{TASK_ID}/top\\level.bin")
}

/// Reproduce a migrated file boundary without invoking an external engine.
/// Its metadata and raw MD5 cache bytes live only in this fixture's checkout
/// and filesystem remote, including when the historical path is unaddressable.
fn fixture_storage_boundary(repo: &Path, remote: &Path, name: &str, payload: &[u8]) {
    let task = repo.join(TASK_ID);
    std::fs::write(task.join(name), payload).unwrap();
    let digest = md5::Md5::digest(payload)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    std::fs::write(
        task.join(format!("{name}.wm-storage.json")),
        storage_file_manifest(name, &digest, payload.len() as u64, None),
    )
    .unwrap();
    for root in [repo.join(".workspace-mgr/local/cache"), remote.to_owned()] {
        let cache = root
            .join("objects/md5")
            .join(&digest[..2])
            .join(&digest[2..]);
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(cache, payload).unwrap();
    }
    let ignore = task.join(".gitignore");
    let mut rules = std::fs::read_to_string(&ignore).unwrap_or_default();
    let rule = format!("/{}", name.replace('\\', "\\\\"));
    if !rules.lines().any(|line| line == rule) {
        if !rules.is_empty() && !rules.ends_with('\n') {
            rules.push('\n');
        }
        rules.push_str(&rule);
        rules.push('\n');
        std::fs::write(ignore, rules).unwrap();
    }
    // The historical payload is local/cache content, never ordinary Git data.
    git(repo, ["check-ignore", &format!("{TASK_ID}/{name}")]);
}

/// A repository whose shared branch carries one incoming storage boundary the
/// engine can address and one it cannot, plus an ordinary incoming Git file.
///
/// No current command produces the unaddressable boundary: placement refuses
/// it. Migration preserves an existing boundary with this name, so the fixture
/// reproduces its native metadata/cache and Git in a crafting checkout,
/// and leaves the shared checkout one fast-forward behind it.
fn shared_branch_carrying_unaddressable_metadata() -> SharedBranch {
    let fixture = GitFixture::new();
    let storage_remote = fixture.root.join("storage-remote");
    workspace(
        &fixture.seed,
        ["manage", "--s3-url", storage_remote.to_str().unwrap()],
    );
    fixture.commit_seed("Initialize isolated storage");
    fixture.clone_shared();
    let created = workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "refresh-unaddressable",
            "--title",
            "Refresh unaddressable",
            "--purpose",
            "Refresh a branch carrying metadata the engine cannot address.",
            "--timestamp",
            TIMESTAMP,
        ],
    );
    let task_branch = json(&created)["branch"].as_str().unwrap().to_owned();
    let task = fixture.shared.join(TASK_ID);
    document_task(&task);
    std::fs::write(task.join("first.bin"), FIRST_PAYLOAD).unwrap();
    workspace(
        &task,
        [
            "storage",
            "set",
            &format!("{TASK_ID}/first.bin"),
            "--to",
            "s3",
            "--reason",
            "Exercise a boundary the engine can address.",
        ],
    );
    let published = workspace(&task, ["publish", "-m", "Publish the first boundary"]);
    let oid = json(&published)["commit_oid"].as_str().unwrap().to_owned();
    git(&fixture.remote, ["update-ref", "refs/heads/main", &oid]);
    workspace(&fixture.shared, ["refresh"]);

    let publisher = fixture.root.join("publisher");
    command(
        &fixture.root,
        "git",
        [
            "clone",
            fixture.remote.to_str().unwrap(),
            publisher.to_str().unwrap(),
        ],
    );
    configure_git(&publisher);
    let publisher_task = publisher.join(TASK_ID);
    fixture_storage_boundary(&publisher, &storage_remote, "second.bin", SECOND_PAYLOAD);
    fixture_storage_boundary(
        &publisher,
        &storage_remote,
        "top\\level.bin",
        UNADDRESSABLE_PAYLOAD,
    );
    std::fs::write(
        publisher_task.join("notes.md"),
        "Ordinary Git content arriving in the same revision.\n",
    )
    .unwrap();
    git(&publisher, ["add", "-A"]);
    git(
        &publisher,
        [
            "commit",
            "-m",
            // The trailers a publication of this task would have written, so
            // the branch this fixture leaves behind is the one the product
            // recognizes as that task's.
            &format!(
                "Publish metadata the engine cannot address\n\nWorkspace-Task: {TASK_ID}\nWorkspace-Scope: {TASK_ID}\n"
            ),
        ],
    );
    // The unaddressable boundary reached the shared branch the way any content
    // does, through the task branch, so both refs carry it and a later
    // publication of that task still builds on it.
    git(&publisher, ["push", "origin", "HEAD:refs/heads/main"]);
    git(
        &publisher,
        ["push", "origin", &format!("HEAD:refs/heads/{task_branch}")],
    );
    let incoming_oid = revision(&publisher, "HEAD");

    SharedBranch {
        fixture,
        incoming_oid,
    }
}

fn revision(repo: &Path, name: &str) -> String {
    String::from_utf8(git(repo, ["rev-parse", name]).stdout)
        .unwrap()
        .trim()
        .to_owned()
}

#[test]
fn refresh_skips_one_unaddressable_boundary_and_hydrates_everything_else() {
    let branch = shared_branch_carrying_unaddressable_metadata();
    let shared = &branch.fixture.shared;
    let task = branch.task();
    let boundary = unaddressable_boundary();

    let refreshed = workspace(shared, ["refresh"]);
    let report = json(&refreshed);

    // The branch advances: one boundary nothing can address must not freeze
    // inbound synchronization for the whole checkout.
    assert_eq!(report["status"], "updated");
    assert_eq!(revision(shared, "HEAD"), branch.incoming_oid);

    // Everything else in the same revision arrives, including the boundary
    // whose payload only the storage remote held.
    assert_eq!(
        std::fs::read(task.join("second.bin")).unwrap(),
        SECOND_PAYLOAD
    );
    assert!(task.join("second.bin.wm-storage.json").is_file());
    assert!(task.join("notes.md").is_file());
    assert_eq!(
        std::fs::read(task.join("first.bin")).unwrap(),
        FIRST_PAYLOAD
    );

    // The unaddressable boundary's metadata advances like any other Git file,
    // and its payload is deliberately left behind.
    assert!(task.join("top\\level.bin.wm-storage.json").is_file());
    assert!(!task.join("top\\level.bin").exists());
    assert_eq!(
        report["storage"]["unaddressable"],
        serde_json::json!([boundary])
    );
    assert_eq!(
        report["warnings"][0]["code"],
        "unaddressable-storage-metadata"
    );
    let message = report["warnings"][0]["message"].as_str().unwrap();
    assert!(message.contains(&boundary), "{message}");
    assert!(message.contains("workspace-mgr move"), "{message}");
    assert!(message.contains(&format!("(`{TASK_ID}`)")), "{message}");
    assert!(report["warnings"][1].is_null());

    // Nothing is half-applied, so refreshing again is an ordinary no-op that
    // reports nothing: the condition belongs to the revision that carried it.
    let again = json(&workspace(shared, ["refresh"]));
    assert_eq!(again["status"], "no_changes");
    assert!(again["warnings"].is_null());
    assert!(again["storage"]["unaddressable"].is_null());
}

#[test]
fn refresh_dry_run_reports_the_condition_instead_of_a_false_green() {
    let branch = shared_branch_carrying_unaddressable_metadata();
    let shared = &branch.fixture.shared;
    let task = branch.task();
    let before = revision(shared, "HEAD");
    let status_before = git(shared, ["status", "--porcelain"]).stdout;

    let report = json(&workspace(shared, ["refresh", "--dry-run"]));

    assert_eq!(report["status"], "dry_run");
    assert_eq!(
        report["storage"]["unaddressable"],
        serde_json::json!([unaddressable_boundary()])
    );
    assert_eq!(
        report["warnings"][0]["code"],
        "unaddressable-storage-metadata"
    );

    // The detection runs before anything changes, so the preview that reports
    // it is still a preview.
    assert_eq!(revision(shared, "HEAD"), before);
    assert_eq!(git(shared, ["status", "--porcelain"]).stdout, status_before);
    assert!(!task.join("second.bin.wm-storage.json").exists());
    assert!(!task.join("second.bin").exists());
    assert!(!task.join("notes.md").exists());
    assert!(!task.join("top\\level.bin.wm-storage.json").exists());
}

#[test]
fn the_recovery_the_warning_names_works_as_written_for_every_other_checkout() {
    let branch = shared_branch_carrying_unaddressable_metadata();
    let shared = &branch.fixture.shared;
    let boundary = unaddressable_boundary();
    let destination = format!("{TASK_ID}/top-level.bin");
    let refreshed = json(&workspace(shared, ["refresh"]));
    let warning = refreshed["warnings"][0]["message"].as_str().unwrap();

    // The other checkout takes the unaddressable metadata without ever seeing
    // a payload for it, exactly as the shared checkout did.
    let consumer = branch.fixture.root.join("consumer");
    command(
        &branch.fixture.root,
        "git",
        [
            "clone",
            branch.fixture.remote.to_str().unwrap(),
            consumer.to_str().unwrap(),
        ],
    );
    configure_git(&consumer);
    assert!(
        consumer
            .join(format!("{boundary}.wm-storage.json"))
            .is_file()
    );
    assert!(!consumer.join(&boundary).exists());

    // The warning's recovery, step by step: an infrastructure task scoped to
    // the directory that holds the boundary, which the warning names.
    assert!(warning.contains("infrastructure task"), "{warning}");
    assert!(warning.contains(&format!("(`{TASK_ID}`)")), "{warning}");
    let created = json(&workspace(
        &consumer,
        [
            "task",
            "create",
            "recover-boundary",
            "--kind",
            "infrastructure",
            "--title",
            "Recover an unaddressable boundary",
            "--purpose",
            "Rename a storage boundary the engine cannot address.",
            "--scope",
            TASK_ID,
            "--scope-note",
            "The user authorized renaming this boundary.",
        ],
    ));
    let worktree = PathBuf::from(created["path"].as_str().unwrap());
    let manifest = created["manifest"].as_str().unwrap();
    // The task starts from the fetched base, so it holds the metadata but, like
    // every checkout, no payload for the boundary.
    assert!(
        worktree
            .join(format!("{boundary}.wm-storage.json"))
            .is_file()
    );
    assert!(!worktree.join(&boundary).exists());

    let moved = json(&workspace(
        &worktree,
        ["move", &boundary, &destination, "--manifest", manifest],
    ));
    assert_eq!(moved["status"], "updated");
    assert!(
        !worktree
            .join(format!("{boundary}.wm-storage.json"))
            .exists()
    );
    assert!(
        worktree
            .join(format!("{destination}.wm-storage.json"))
            .is_file()
    );
    // The move fetched the payload through the old metadata and materialized
    // it at the destination, so publication has the bytes to upload under
    // the new path and no separate hydration is needed.
    assert_eq!(
        std::fs::read(worktree.join(&destination)).unwrap(),
        UNADDRESSABLE_PAYLOAD
    );
    assert!(!worktree.join(&boundary).exists());

    // Publication requires every boundary in its scope to be present, and a
    // scope-wide hydrate would refuse before the rename, so the warning says
    // to name the others.
    let early = workspace_unchecked(
        &worktree,
        [
            "publish",
            "--manifest",
            manifest,
            "-m",
            "Publish before hydrating",
        ],
    );
    assert_eq!(early.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&early.stderr).contains("hydrate them before publishing"),
        "{}",
        String::from_utf8_lossy(&early.stderr)
    );
    let hydrated = json(&workspace(
        &worktree,
        [
            "storage",
            "hydrate",
            "--manifest",
            manifest,
            &format!("{TASK_ID}/first.bin"),
            &format!("{TASK_ID}/second.bin"),
        ],
    ));
    assert_eq!(hydrated["status"], "hydrated");

    let published = json(&workspace(
        &worktree,
        [
            "publish",
            "--manifest",
            manifest,
            "-m",
            "Recover the unaddressable boundary",
        ],
    ));
    assert_eq!(published["status"], "pushed");
    let oid = published["commit_oid"].as_str().unwrap().to_owned();
    git(
        &branch.fixture.remote,
        ["update-ref", "refs/heads/main", &oid],
    );

    // The claim the warning makes: a later refresh in any checkout hydrates
    // the renamed boundary that refresh had to leave behind.
    for checkout in [&consumer, shared] {
        let refreshed = json(&workspace(checkout, ["refresh"]));
        assert_eq!(refreshed["status"], "updated");
        assert!(refreshed["warnings"].is_null());
        assert!(refreshed["storage"]["unaddressable"].is_null());
        assert_eq!(
            std::fs::read(checkout.join(&destination)).unwrap(),
            UNADDRESSABLE_PAYLOAD
        );
        assert!(
            !checkout
                .join(format!("{boundary}.wm-storage.json"))
                .exists()
        );
        assert!(!checkout.join(&boundary).exists());
    }
}

#[test]
fn refresh_refuses_to_leave_a_payload_under_metadata_that_no_longer_describes_it() {
    let branch = shared_branch_carrying_unaddressable_metadata();
    // The crafting checkout still holds the payload it published.
    let publisher = branch.fixture.root.join("publisher");
    let payload = publisher.join(TASK_ID).join("top\\level.bin");
    let metadata = publisher
        .join(TASK_ID)
        .join("top\\level.bin.wm-storage.json");
    let metadata_before = std::fs::read(&metadata).unwrap();

    // Someone replaces that boundary's content upstream, represented by
    // historical pointer/cache bytes that current placement would refuse.
    let updater = branch.fixture.root.join("updater");
    command(
        &branch.fixture.root,
        "git",
        [
            "clone",
            branch.fixture.remote.to_str().unwrap(),
            updater.to_str().unwrap(),
        ],
    );
    configure_git(&updater);
    fixture_storage_boundary(
        &updater,
        &branch.fixture.root.join("storage-remote"),
        "top\\level.bin",
        b"newer content for the payload nothing can address\n",
    );
    git(&updater, ["add", "-A"]);
    git(
        &updater,
        ["commit", "-m", "Replace the unaddressable payload"],
    );
    git(&updater, ["push", "origin", "HEAD:refs/heads/main"]);

    // Refresh can neither replace nor verify those bytes, so advancing the
    // metadata over them would describe content the checkout does not hold.
    for args in [&["refresh", "--dry-run"][..], &["refresh"][..]] {
        let refused = workspace_unchecked(&publisher, args);
        assert_eq!(refused.status.code(), Some(2), "{args:?}");
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(
            stderr.contains("does not describe the payload this checkout holds"),
            "{stderr}"
        );
        assert!(stderr.contains(&unaddressable_boundary()), "{stderr}");
    }

    // Nothing changed: the refusal comes before the branch, the metadata, or
    // the payload moves.
    assert_eq!(revision(&publisher, "HEAD"), branch.incoming_oid);
    assert_eq!(std::fs::read(&metadata).unwrap(), metadata_before);
    assert_eq!(std::fs::read(&payload).unwrap(), UNADDRESSABLE_PAYLOAD);
}

#[test]
fn a_payload_refresh_cannot_address_is_kept_only_when_it_already_matches() {
    let branch = shared_branch_carrying_unaddressable_metadata();
    let shared = &branch.fixture.shared;
    let task = branch.task();
    let publisher_task = branch.fixture.root.join("publisher").join(TASK_ID);
    let before = revision(shared, "HEAD");

    // An output already at the boundary path, with no metadata beside it, is
    // refused exactly as it is for a boundary the engine can address.
    std::fs::write(task.join("top\\level.bin"), UNADDRESSABLE_PAYLOAD).unwrap();
    let refused = workspace_unchecked(shared, ["refresh"]);
    assert_eq!(refused.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("already exist without matching local metadata"),
        "{stderr}"
    );
    assert_eq!(revision(shared, "HEAD"), before);

    // The checkout whose task produced the boundary holds its metadata and
    // exact payload. There is nothing to reconcile, so refresh advances and
    // keeps those bytes, still warning that no other checkout can have them.
    std::fs::copy(
        publisher_task.join("top\\level.bin.wm-storage.json"),
        task.join("top\\level.bin.wm-storage.json"),
    )
    .unwrap();
    let report = json(&workspace(shared, ["refresh"]));
    assert_eq!(report["status"], "updated");
    assert_eq!(revision(shared, "HEAD"), branch.incoming_oid);
    assert_eq!(
        std::fs::read(task.join("top\\level.bin")).unwrap(),
        UNADDRESSABLE_PAYLOAD
    );
    assert_eq!(
        report["warnings"][0]["code"],
        "unaddressable-storage-metadata"
    );
}

#[test]
fn a_failed_move_restores_a_boundary_that_has_no_payload_to_put_back() {
    let branch = shared_branch_carrying_unaddressable_metadata();
    let task = branch.task();
    let boundary = unaddressable_boundary();
    workspace(&branch.fixture.shared, ["refresh"]);
    let metadata_before = std::fs::read(task.join("top\\level.bin.wm-storage.json")).unwrap();

    // A destination the engine cannot address either, refused after the move
    // has begun and its metadata snapshot has been taken.
    let refused = workspace_unchecked(
        &task,
        ["move", &boundary, &format!("{TASK_ID}/still\\bad.bin")],
    );

    assert_eq!(refused.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("contains a backslash"), "{stderr}");
    // The boundary has no payload on either side, so restoring its metadata
    // restores all of it. A rollback that went looking for an output would
    // report a failure of its own here and leave the operator with two errors.
    assert!(stderr.contains("rolled back"), "{stderr}");
    assert!(!stderr.contains("rollback also failed"), "{stderr}");
    assert_eq!(
        std::fs::read(task.join("top\\level.bin.wm-storage.json")).unwrap(),
        metadata_before
    );
    assert!(!task.join("still\\bad.bin.wm-storage.json").exists());
    assert!(!task.join("still\\bad.bin").exists());
    assert!(!task.join("top\\level.bin").exists());
}

/// A newer release may write storage metadata this one cannot read, so refresh
/// checks the incoming `minimum_cli_version` before it inspects any incoming
/// boundary. The requirement wins over every outcome the unaddressable
/// boundary would otherwise produce, including the refusal over a payload this
/// checkout holds there, and it does so at dry run and at apply before anything
/// changes.
#[test]
fn an_incoming_requirement_is_refused_before_unaddressable_metadata_is_inspected() {
    let branch = shared_branch_carrying_unaddressable_metadata();
    let fixture = &branch.fixture;
    let shared = &fixture.shared;
    let task = branch.task();

    // A later release raises the requirement in the same incoming range and
    // rewrites the unaddressable metadata in a form this release cannot read.
    let raiser = fixture.root.join("raiser");
    command(
        &fixture.root,
        "git",
        [
            "clone",
            fixture.remote.to_str().unwrap(),
            raiser.to_str().unwrap(),
        ],
    );
    configure_git(&raiser);
    let config = raiser.join(".workspace-mgr.toml");
    let original = std::fs::read_to_string(&config).unwrap();
    let original = if original.starts_with("minimum_cli_version = ") {
        original.split_once("\n\n").unwrap().1
    } else {
        &original
    };
    std::fs::write(
        &config,
        format!("minimum_cli_version = \"99.0.0\"\n\n{original}"),
    )
    .unwrap();
    std::fs::write(
        raiser.join(TASK_ID).join("top\\level.bin.wm-storage.json"),
        r#"{"schema_version":99,"future_field":"metadata only a newer release reads"}"#,
    )
    .unwrap();
    git(&raiser, ["add", "-A"]);
    git(&raiser, ["commit", "-m", "Require a newer workspace-mgr"]);
    git(&raiser, ["push", "origin", "HEAD:refs/heads/main"]);

    // On its own, this payload would refuse the refresh for a different
    // reason: the incoming metadata does not describe it.
    let local_payload = b"a payload this checkout holds at the unaddressable path\n";
    std::fs::write(task.join("top\\level.bin"), local_payload).unwrap();
    let before = revision(shared, "HEAD");
    let status_before = git(shared, ["status", "--porcelain"]).stdout;
    let installed = String::from_utf8(workspace(shared, ["--version"]).stdout)
        .unwrap()
        .split_whitespace()
        .last()
        .unwrap()
        .to_owned();
    let expected = format!(
        "workspace-mgr: this repository requires workspace-mgr 99.0.0 or newer (`minimum_cli_version` in .workspace-mgr.toml on origin/main); this is workspace-mgr {installed}. Tell the user both versions and ask before updating with `cargo install --locked workspace-mgr`, then run `workspace-mgr setup`.\n"
    );
    for args in [&["refresh", "--dry-run"][..], &["refresh"][..]] {
        let refused = workspace_unchecked(shared, args);
        assert_eq!(refused.status.code(), Some(2), "{args:?}");
        assert!(refused.stdout.is_empty(), "{args:?}");
        assert_eq!(
            String::from_utf8_lossy(&refused.stderr),
            expected,
            "{args:?}"
        );
        assert_eq!(revision(shared, "HEAD"), before, "{args:?}");
        assert_eq!(
            git(shared, ["status", "--porcelain"]).stdout,
            status_before,
            "{args:?}"
        );
        assert!(
            !task.join("second.bin.wm-storage.json").exists(),
            "{args:?}"
        );
        assert!(
            !task.join("top\\level.bin.wm-storage.json").exists(),
            "{args:?}"
        );
        assert_eq!(
            std::fs::read(task.join("top\\level.bin")).unwrap(),
            local_payload
        );
    }
}
