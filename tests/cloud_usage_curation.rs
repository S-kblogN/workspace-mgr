//! Where the cloud-usage gate meets the curation guards: the documentation
//! refusal, the escaping-link and machine-local-ignore refusals, and the
//! `task-record-unchanged` and `bulk-publication` warnings. Neither side may
//! leave a task with no publication it is allowed to make.

mod common;

use std::path::{Path, PathBuf};
use std::process::Output;

use common::*;
use serde_json::Value;

const CONFIG: &str = ".workspace-mgr.toml";
const MANIFEST: &str = ".workspace-mgr-task.toml";
const UNDOCUMENTED: &str = "publishes content but documents nothing";

fn managed_fixture(storage: bool) -> GitFixture {
    let fixture = GitFixture::new();
    if storage {
        workspace(
            &fixture.seed,
            [
                "init",
                "--s3-url",
                fixture.root.join("storage-remote").to_str().unwrap(),
            ],
        );
    } else {
        workspace(&fixture.seed, ["init"]);
    }
    fixture.commit_seed("Initialize workspace");
    fixture.clone_shared();
    fixture
}

fn create_task(fixture: &GitFixture, slug: &str, timestamp: &str) -> (String, PathBuf) {
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            slug,
            "--title",
            "Cloud usage and curation",
            "--purpose",
            "Verify that the cloud-usage gate and the curation guards compose.",
            "--timestamp",
            timestamp,
        ],
    );
    let task_id = format!("{timestamp}-{slug}");
    let task = fixture.shared.join(&task_id);
    (task_id, task)
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_refused(output: &Output, needle: &str) {
    let message = stderr(output);
    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout:\n{}\nstderr:\n{message}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(output.stdout.is_empty(), "a refusal prints no report");
    assert!(
        message.contains(needle),
        "missing {needle:?} in:\n{message}"
    );
}

fn rev(repo: &Path, reference: &str) -> Option<String> {
    let output = git_unchecked(repo, ["rev-parse", "-q", "--verify", reference]);
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn show(repo: &Path, object: &str) -> String {
    String::from_utf8_lossy(&git(repo, ["show", object]).stdout).into_owned()
}

fn exclude_file(repo: &Path) -> PathBuf {
    let raw =
        String::from_utf8_lossy(&git(repo, ["rev-parse", "--git-path", "info/exclude"]).stdout)
            .trim()
            .to_owned();
    let path = PathBuf::from(raw);
    let path = if path.is_absolute() {
        path
    } else {
        repo.join(path)
    };
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    path
}

/// An approval-only publication changes the task manifest and, through it,
/// the repository's declared minimum release. Both are workspace-mgr's own
/// control files, so the curation guards see nothing to judge: the task still
/// documents nothing, yet the publication passes, warns about nothing, and the
/// reconciled configuration is reported where the publication changes it and
/// nowhere else, even when a machine-local rule names it.
#[test]
fn an_approval_only_publication_is_housekeeping_for_the_curation_guards() {
    let fixture = managed_fixture(false);
    let (task_id, task) = create_task(&fixture, "approval-only", "20260920-100000");
    let branch = "refs/heads/codex/approval-only";
    let manifest = format!("{task_id}/{MANIFEST}");
    workspace(&task, ["publish", "-m", "Publish scaffold"]);
    assert!(
        fixture.shared.join(".gitignore").is_file(),
        "init generated the product-owned root ignore file"
    );
    // The configuration is tracked, so this rule hides nothing; it must not be
    // mistaken for content that only a machine-local rule keeps out of review.
    std::fs::write(exclude_file(&fixture.shared), "/.workspace-mgr.toml\n").unwrap();
    let shared_config = std::fs::read_to_string(fixture.shared.join(CONFIG)).unwrap();

    let recorded = json(&workspace(
        &task,
        [
            "task",
            "approve-cloud-usage",
            "--limit",
            "2GiB",
            "--note",
            "The user approved 2 GiB for the evaluation outputs",
        ],
    ));
    assert_eq!(recorded["status"], "recorded");
    let plan = json(&workspace(&task, ["plan"]));
    assert_eq!(plan["status"], "dry_run");
    assert_eq!(plan["changed_paths"], serde_json::json!([CONFIG, manifest]));
    assert_eq!(plan["repository_requirement"]["change"], "raise");
    assert!(plan.get("warnings").is_none(), "{plan}");
    assert!(plan.get("ignored_paths").is_none(), "{plan}");
    assert_eq!(plan["cloud_usage"]["status"], "within_limit");

    let published = json(&workspace(&task, ["publish", "-m", "Publish the approval"]));
    assert_eq!(published["status"], "pushed");
    assert_eq!(
        published["changed_paths"],
        serde_json::json!([CONFIG, manifest])
    );
    assert!(published.get("warnings").is_none(), "{published}");
    assert!(published.get("ignored_paths").is_none(), "{published}");
    let tip = published["remote_oid"].as_str().unwrap().to_owned();
    assert!(
        show(&fixture.remote, &format!("{tip}:{CONFIG}"))
            .starts_with("minimum_cli_version = \"0.4.0\"\n"),
        "the published tree carries the raised declaration"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.shared.join(CONFIG)).unwrap(),
        shared_config,
        "the shared checkout keeps its configuration"
    );

    // Withdrawing the approval is housekeeping in the other direction.
    workspace(
        &task,
        [
            "task",
            "approve-cloud-usage",
            "--limit",
            "1GiB",
            "--note",
            "The user returned to the default limit",
        ],
    );
    let withdrawn = json(&workspace(
        &task,
        ["publish", "-m", "Withdraw the approval"],
    ));
    assert_eq!(withdrawn["status"], "pushed");
    assert_eq!(withdrawn["repository_requirement"]["change"], "withdraw");
    assert_eq!(
        withdrawn["changed_paths"],
        serde_json::json!([CONFIG, manifest])
    );
    assert!(withdrawn.get("warnings").is_none(), "{withdrawn}");

    // The task still documents nothing, so the guard is live: new content is
    // refused exactly as before.
    std::fs::write(task.join("result.csv"), "a,b\n1,2\n").unwrap();
    assert_refused(&workspace_unchecked(&task, ["plan"]), UNDOCUMENTED);
    assert_eq!(
        rev(&fixture.remote, branch),
        withdrawn["remote_oid"].as_str().map(ToOwned::to_owned)
    );
}

#[cfg(feature = "test-storage")]
mod test_storage {
    use super::*;

    pub const APPROVAL_REFUSAL: &str = "needs the user's approval";
    pub const REMINDER: &str = "is waiting for the user's cloud-usage decision";

    pub fn tree_contains(repo: &Path, oid: &str, path: &str) -> bool {
        git_unchecked(repo, ["cat-file", "-e", &format!("{oid}:{path}")])
            .status
            .success()
    }

    pub fn cloud_usage_state(repo: &Path) -> Vec<PathBuf> {
        let root = repo.join(".git/workspace-mgr/state");
        if !root.exists() {
            return Vec::new();
        }
        walkdir::WalkDir::new(root)
            .into_iter()
            .map(Result::unwrap)
            .filter(|entry| entry.file_name() == "cloud-usage.json")
            .map(|entry| entry.into_path())
            .collect()
    }

    pub fn warning_codes(report: &Value) -> Vec<String> {
        report["warnings"]
            .as_array()
            .map(|warnings| {
                warnings
                    .iter()
                    .map(|warning| warning["code"].as_str().unwrap().to_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Deterministic bytes that Git cannot compress.
    pub fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 32) as u8
            })
            .collect()
    }

    // A lowered threshold needs a test build; approvals are not involved.
    pub const OVER_300KB: [(&str, &str); 1] = [(CLOUD_USAGE_THRESHOLD_ENV, "300000")];
    pub const ROOMY_10MB: [(&str, &str); 1] = [(CLOUD_USAGE_THRESHOLD_ENV, "10000000")];

    pub fn dvc_available() -> bool {
        if which::which("dvc").is_err() {
            eprintln!("skipping: dvc is unavailable");
            return false;
        }
        true
    }

    pub fn remote_snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        if !root.exists() {
            return Vec::new();
        }
        let mut files = walkdir::WalkDir::new(root)
            .into_iter()
            .map(Result::unwrap)
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                (
                    entry.path().strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(entry.path()).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        files.sort();
        files
    }

    /// Asserts that a publication was refused by a structural guard before the
    /// cloud-usage gate could measure it: the guard's own message, no usage
    /// refusal, no pending decision, nothing placed, and no remote change.
    pub fn assert_refused_before_the_gate(
        fixture: &GitFixture,
        task: &Path,
        branch: &str,
        needle: &str,
    ) {
        let tip = rev(&fixture.remote, branch);
        for args in [
            &["plan"][..],
            &["publish", "-m", "Publish over the limit", "--dry-run"][..],
            &["publish", "-m", "Publish over the limit"][..],
        ] {
            let refused = workspace_env_unchecked(task, args, &OVER_300KB);
            assert_refused(&refused, needle);
            let message = stderr(&refused);
            assert!(!message.contains(APPROVAL_REFUSAL), "{args:?}: {message}");
            assert!(!message.contains(REMINDER), "{args:?}: {message}");
            assert!(
                cloud_usage_state(&fixture.shared).is_empty(),
                "{args:?} recorded a cloud-usage decision"
            );
            assert_eq!(rev(&fixture.remote, branch), tip, "{args:?}");
        }
        let status = json(&workspace_env(task, ["task", "status"], &OVER_300KB));
        assert_eq!(status["cloud_usage"]["pending"], Value::Null);
    }
}

#[cfg(feature = "test-storage")]
use test_storage::*;

/// The curation guards are decided before the cloud-usage gate. Resolving one
/// of them is ordinary work that may change the publication, so the user is
/// asked about cloud usage only once the publication those guards accept is
/// known, and a refused guard never leaves a pending decision behind.
#[cfg(all(unix, feature = "test-storage"))]
#[test]
fn structural_refusals_come_before_the_cloud_usage_gate() {
    let fixture = managed_fixture(false);
    let (task_id, task) = create_task(&fixture, "gate-after-curation", "20260920-101000");
    let branch = "refs/heads/codex/gate-after-curation";
    workspace_env(&task, ["publish", "-m", "Publish scaffold"], &OVER_300KB);
    // Content that alone takes the task past its 300 KB limit.
    std::fs::write(task.join("weights.bin"), noise(400_000, 3)).unwrap();

    // The task documents nothing.
    assert_refused_before_the_gate(&fixture, &task, branch, UNDOCUMENTED);
    document_task(&task);

    // A staged link that leaves the repository.
    std::os::unix::fs::symlink("/private/tmp/outside", task.join("scratch-link")).unwrap();
    assert_refused_before_the_gate(&fixture, &task, branch, "which is outside the repository");
    std::fs::remove_file(task.join("scratch-link")).unwrap();

    // By-products hidden only by this machine's rules.
    std::fs::write(task.join("run-1.log"), "per-run log\n").unwrap();
    std::fs::write(exclude_file(&fixture.shared), "*.log\n").unwrap();
    assert_refused_before_the_gate(
        &fixture,
        &task,
        branch,
        "only an ignore rule this publication does not carry hides",
    );

    // Once the task carries the rule, the only question left is cloud usage,
    // and it is about exactly the publication the guards accept.
    std::fs::write(exclude_file(&fixture.shared), "").unwrap();
    std::fs::write(task.join(".gitignore"), "*.log\n").unwrap();
    let plan = json(&workspace_env(&task, ["plan"], &OVER_300KB));
    assert_eq!(plan["cloud_usage"]["status"], "approval_required");
    assert_eq!(
        plan["ignored_paths"],
        serde_json::json!([format!("{task_id}/run-1.log")])
    );
    let changed = plan["changed_paths"].as_array().unwrap();
    assert!(changed.contains(&Value::from(format!("{task_id}/weights.bin"))));
    assert!(!changed.contains(&Value::from(format!("{task_id}/run-1.log"))));
    assert_eq!(cloud_usage_state(&fixture.shared).len(), 1);
    let refused = workspace_env_unchecked(&task, ["publish", "-m", "Publish weights"], &OVER_300KB);
    assert_refused(&refused, APPROVAL_REFUSAL);

    // Content on its way to S3 is refused by the guard before placement or
    // upload, too.
    if !dvc_available() {
        return;
    }
    let fixture = managed_fixture(true);
    let (_, task) = create_task(&fixture, "gate-before-upload", "20260920-102000");
    let branch = "refs/heads/codex/gate-before-upload";
    let storage_remote = fixture.root.join("storage-remote");
    workspace_env(&task, ["publish", "-m", "Publish scaffold"], &OVER_300KB);
    let storage_before = remote_snapshot(&storage_remote);
    std::fs::write(task.join("checkpoint.bin"), noise(10_485_761, 5)).unwrap();
    assert_refused_before_the_gate(&fixture, &task, branch, UNDOCUMENTED);
    assert!(!task.join("checkpoint.bin.dvc").exists());
    assert!(!task.join(".gitignore").exists());
    assert_eq!(
        remote_snapshot(&fixture.shared.join(".dvc/cache")),
        Vec::new()
    );
    assert_eq!(remote_snapshot(&storage_remote), storage_before);
}

/// A task over its limit whose user chose cleanup must be able to publish that
/// cleanup even when the task documents nothing: `remove` and `untrack` only
/// retire content, and adding a record to the cleanup would make it growth the
/// limit refuses. The record warning says so instead of inviting the refusal.
#[cfg(feature = "test-storage")]
#[test]
fn an_undocumented_task_over_its_limit_publishes_the_cleanup_the_user_chose() {
    if !dvc_available() {
        return;
    }
    let fixture = managed_fixture(true);
    let (task_id, task) = create_task(&fixture, "undocumented-cleanup", "20260920-103000");
    let branch = "refs/heads/codex/undocumented-cleanup";
    let stored = format!("{task_id}/stored.bin");
    let table = format!("{task_id}/table.bin");
    document_task(&task);
    std::fs::write(task.join("stored.bin"), noise(400_000, 7)).unwrap();
    std::fs::write(task.join("table.bin"), noise(200_000, 8)).unwrap();
    workspace_env(
        &task,
        [
            "storage",
            "set",
            stored.as_str(),
            "--to",
            "s3",
            "--reason",
            "Keep the stored result in storage.",
        ],
        &ROOMY_10MB,
    );
    let published = json(&workspace_env(
        &task,
        ["publish", "-m", "Publish results"],
        &ROOMY_10MB,
    ));
    assert_eq!(published["status"], "pushed");
    // Retiring the record alone publishes no content, so it is allowed, and
    // leaves a task that documents nothing, as one published before the guard
    // existed would be.
    std::fs::remove_file(task.join("record.md")).unwrap();
    let retired = json(&workspace_env(
        &task,
        ["publish", "-m", "Retire the record"],
        &ROOMY_10MB,
    ));
    assert_eq!(retired["status"], "pushed");
    std::fs::write(task.join("extra.txt"), "extra\n").unwrap();
    assert_refused(
        &workspace_env_unchecked(&task, ["plan"], &ROOMY_10MB),
        UNDOCUMENTED,
    );
    std::fs::remove_file(task.join("extra.txt")).unwrap();

    // Under a lower limit the task is over it and waiting for the user.
    let waiting = json(&workspace_env(&task, ["plan"], &OVER_300KB));
    assert_eq!(waiting["status"], "no_changes");
    assert_eq!(waiting["cloud_usage"]["status"], "approval_required");
    let storage_remote = fixture.root.join("storage-remote");
    let storage_before = remote_snapshot(&storage_remote);

    // The user chooses cleanup: keep the stored result on this machine only.
    let untracked = workspace_env(&task, ["untrack", stored.as_str()], &OVER_300KB);
    assert!(stderr(&untracked).contains(REMINDER));
    let plan = json(&workspace_env(&task, ["plan"], &OVER_300KB));
    let usage = &plan["cloud_usage"];
    assert_eq!(usage["status"], "approval_required");
    assert_eq!(usage["cleanup_only"], true);
    assert_eq!(usage["publish_allowed"], true);
    assert_eq!(warning_codes(&plan), ["task-record-unchanged"]);
    let message = plan["warnings"][0]["message"].as_str().unwrap();
    assert!(
        message.ends_with(
            "while the task is over its cloud-usage limit this publication is allowed only because it adds no content, so publish it as it is and record the decision in the first publication the limit allows"
        ),
        "{message}"
    );
    let kept = json(&workspace_env(
        &task,
        ["publish", "-m", "Keep the stored result local"],
        &OVER_300KB,
    ));
    assert_eq!(kept["status"], "pushed");
    let tip = kept["remote_oid"].as_str().unwrap().to_owned();
    assert!(!tree_contains(
        &fixture.remote,
        &tip,
        &format!("{stored}.dvc")
    ));
    assert!(tree_contains(
        &fixture.remote,
        &tip,
        &format!("{stored}.workspace-mgr-storage.toml")
    ));
    assert_eq!(remote_snapshot(&storage_remote), storage_before);
    assert_eq!(
        std::fs::read(task.join("stored.bin")).unwrap(),
        noise(400_000, 7)
    );

    // ...and remove the table from Git.
    workspace_env(&task, ["remove", table.as_str()], &OVER_300KB);
    let removed = json(&workspace_env(
        &task,
        ["publish", "-m", "Remove the table"],
        &OVER_300KB,
    ));
    assert_eq!(removed["status"], "pushed");
    assert_eq!(removed["cloud_usage"]["cleanup_only"], true);
    let tip = removed["remote_oid"].as_str().unwrap().to_owned();
    assert!(!tree_contains(&fixture.remote, &tip, &table));

    // Published Git history cannot shrink, so the task stays over its limit,
    // and a record added now would be growth: it waits for the user's answer
    // like any other content.
    let after = json(&workspace_env(&task, ["plan"], &OVER_300KB));
    assert_eq!(after["status"], "no_changes");
    assert_eq!(after["cloud_usage"]["status"], "approval_required");
    document_task(&task);
    assert_refused(
        &workspace_env_unchecked(&task, ["publish", "-m", "Record the cleanup"], &OVER_300KB),
        APPROVAL_REFUSAL,
    );
    assert_eq!(rev(&fixture.remote, branch).as_deref(), Some(tip.as_str()));
}

/// A boundary moved in a checkout that never held its payload is fetched
/// through the old metadata and materialized at the destination. The pending
/// cloud-usage reminder is printed once, and the moved boundary is charged
/// once: the content-addressed remote already stores its digest, so the move
/// uploads nothing and stays a cleanup while the task is over its limit.
#[cfg(feature = "test-storage")]
#[test]
fn a_boundary_moved_without_its_payload_is_charged_once() {
    if !dvc_available() {
        return;
    }
    let fixture = managed_fixture(true);
    let (task_id, task) = create_task(&fixture, "move-unmaterialized", "20260920-104000");
    let branch = "codex/move-unmaterialized";
    let stored = format!("{task_id}/stored.bin");
    let moved = format!("{task_id}/moved.bin");
    let payload = noise(400_000, 9);
    document_task(&task);
    std::fs::write(task.join("stored.bin"), &payload).unwrap();
    workspace_env(
        &task,
        [
            "storage",
            "set",
            stored.as_str(),
            "--to",
            "s3",
            "--reason",
            "Keep the stored result in storage.",
        ],
        &ROOMY_10MB,
    );
    let published = json(&workspace_env(
        &task,
        ["publish", "-m", "Publish the stored result"],
        &ROOMY_10MB,
    ));
    assert_eq!(published["status"], "pushed");
    let published_s3 = published["cloud_usage"]["projected"]["s3_bytes"].clone();
    assert_eq!(published_s3, 400_000);

    // Another chat continues the task in a clone that has only the metadata.
    let second = fixture.root.join("second");
    command(
        &fixture.root,
        "git",
        [
            "clone",
            fixture.remote.to_str().unwrap(),
            second.to_str().unwrap(),
        ],
    );
    configure_git(&second);
    git(&second, ["fetch", "-q", "origin", branch]);
    git(
        &second,
        [
            "restore",
            "--source=FETCH_HEAD",
            "--worktree",
            "--",
            &task_id,
        ],
    );
    let other = second.join(&task_id);
    assert!(other.join("stored.bin.dvc").is_file());
    assert!(!other.join("stored.bin").exists());

    // Planning needs every boundary in scope present, so that clone hydrates
    // and, under a lower limit, records the pending decision. Then the payload
    // and the local cache go again, leaving only the metadata, as in any
    // checkout that never held the payload.
    workspace_env(&other, ["storage", "hydrate", stored.as_str()], &ROOMY_10MB);
    let waiting = json(&workspace_env(&other, ["plan"], &OVER_300KB));
    assert_eq!(waiting["status"], "no_changes");
    assert_eq!(waiting["cloud_usage"]["status"], "approval_required");
    assert_eq!(cloud_usage_state(&second).len(), 1);
    std::fs::remove_file(other.join("stored.bin")).unwrap();
    std::fs::remove_dir_all(second.join(".dvc/cache")).unwrap();

    let output = workspace_env(
        &other,
        ["move", stored.as_str(), moved.as_str()],
        &OVER_300KB,
    );
    assert_eq!(
        stderr(&output).matches(REMINDER).count(),
        1,
        "{}",
        stderr(&output)
    );
    assert_eq!(json(&output)["status"], "updated");
    assert_eq!(std::fs::read(other.join("moved.bin")).unwrap(), payload);
    assert!(!other.join("stored.bin.dvc").exists());

    let plan = json(&workspace_env(&other, ["plan"], &OVER_300KB));
    let usage = &plan["cloud_usage"];
    assert_eq!(usage["status"], "approval_required");
    assert_eq!(usage["published"]["s3_bytes"], published_s3);
    assert_eq!(usage["projected"]["s3_bytes"], published_s3);
    assert_eq!(usage["cleanup_only"], true);
    assert_eq!(usage["publish_allowed"], true);
    let s3 = usage["contributors"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|contributor| contributor["store"] == "s3")
        .collect::<Vec<_>>();
    assert_eq!(s3.len(), 1, "{usage}");
    assert_eq!(s3[0]["bytes"], 400_000);
    assert_eq!(s3[0]["state"], "published");

    let renamed = json(&workspace_env(
        &other,
        ["publish", "-m", "Rename the stored result"],
        &OVER_300KB,
    ));
    assert_eq!(renamed["status"], "pushed");
    let tip = renamed["remote_oid"].as_str().unwrap().to_owned();
    assert!(tree_contains(
        &fixture.remote,
        &tip,
        &format!("{moved}.dvc")
    ));
    assert!(!tree_contains(
        &fixture.remote,
        &tip,
        &format!("{stored}.dvc")
    ));
    let settled = json(&workspace_env(&other, ["plan"], &OVER_300KB));
    assert_eq!(settled["status"], "no_changes");
    assert_eq!(
        settled["cloud_usage"]["published"]["s3_bytes"],
        published_s3
    );
    assert_eq!(
        settled["cloud_usage"]["projected"]["s3_bytes"],
        published_s3
    );
}

/// A result kept local before it was ever published has only its placement
/// record as a durable trace, so within the task's limit that record is the
/// result's stand-in and the documentation guard judges it like the content it
/// addresses. Only an `untrack` that takes previously published content out of
/// Git or S3 retires anything; the guard decides after the measurement, which
/// leaves no pending decision behind.
#[test]
fn a_result_kept_local_before_it_was_published_still_needs_a_task_record() {
    let fixture = managed_fixture(false);
    let (task_id, task) = create_task(&fixture, "local-before-publication", "20260920-105000");
    let branch = "refs/heads/codex/local-before-publication";
    let results = format!("{task_id}/results.bin");
    let scaffold = json(&workspace(&task, ["publish", "-m", "Publish scaffold"]));
    assert_eq!(scaffold["status"], "pushed");
    let tip = rev(&fixture.remote, branch);

    std::fs::write(task.join("results.bin"), vec![7_u8; 50_000]).unwrap();
    assert_refused(&workspace_unchecked(&task, ["plan"]), UNDOCUMENTED);
    workspace(&task, ["untrack", results.as_str()]);
    assert_refused(&workspace_unchecked(&task, ["plan"]), UNDOCUMENTED);
    assert_refused(
        &workspace_unchecked(&task, ["publish", "-m", "Keep results local"]),
        UNDOCUMENTED,
    );
    assert_eq!(rev(&fixture.remote, branch), tip);
    let status = json(&workspace(&task, ["task", "status"]));
    assert_eq!(status["cloud_usage"]["pending"], Value::Null);

    document_task(&task);
    let kept = json(&workspace(
        &task,
        ["publish", "-m", "Keep results local and record why"],
    ));
    assert_eq!(kept["status"], "pushed");
    let changed = kept["changed_paths"].as_array().unwrap();
    assert!(
        changed.contains(&Value::from(format!(
            "{results}.workspace-mgr-storage.toml"
        ))),
        "{changed:?}"
    );
    assert!(
        !changed.contains(&Value::from(results.as_str())),
        "{changed:?}"
    );
}

/// Content added or changed inside a boundary already placed in S3 changes
/// nothing Git can see until the storage engine commits the boundary, so the
/// documentation guard asks the engine on the preview. It refuses before the
/// cloud-usage gate, before any placement or metadata change, and before
/// anything is uploaded.
#[cfg(feature = "test-storage")]
#[test]
fn content_changed_inside_a_boundary_in_s3_is_refused_before_the_gate() {
    if !dvc_available() {
        return;
    }
    let fixture = managed_fixture(true);
    let (task_id, task) = create_task(&fixture, "boundary-content", "20260920-110000");
    let branch = "refs/heads/codex/boundary-content";
    let outputs = format!("{task_id}/outputs");
    let stored = format!("{task_id}/stored.bin");
    publish_boundaries_then_retire_the_record(&task, &outputs, &stored);
    let storage_remote = fixture.root.join("storage-remote");
    let storage_before = remote_snapshot(&storage_remote);
    let outputs_pointer = std::fs::read(task.join("outputs.dvc")).unwrap();
    let stored_pointer = std::fs::read(task.join("stored.bin.dvc")).unwrap();

    // A file added to a directory boundary.
    std::fs::write(task.join("outputs/new.bin"), noise(1_000, 14)).unwrap();
    assert_refused_before_the_gate(&fixture, &task, branch, UNDOCUMENTED);
    std::fs::remove_file(task.join("outputs/new.bin")).unwrap();
    // A file of a directory boundary changed in place, next to a removal.
    std::fs::write(task.join("outputs/a.bin"), noise(200_000, 15)).unwrap();
    std::fs::remove_file(task.join("outputs/b.bin")).unwrap();
    assert_refused_before_the_gate(&fixture, &task, branch, UNDOCUMENTED);
    std::fs::write(task.join("outputs/a.bin"), noise(200_000, 11)).unwrap();
    std::fs::write(task.join("outputs/b.bin"), noise(200_000, 12)).unwrap();
    // A file boundary's payload overwritten.
    std::fs::write(task.join("stored.bin"), noise(100_000, 16)).unwrap();
    assert_refused_before_the_gate(&fixture, &task, branch, UNDOCUMENTED);

    assert_eq!(
        std::fs::read(task.join("outputs.dvc")).unwrap(),
        outputs_pointer
    );
    assert_eq!(
        std::fs::read(task.join("stored.bin.dvc")).unwrap(),
        stored_pointer
    );
    assert_eq!(remote_snapshot(&storage_remote), storage_before);

    // With a record, the same change publishes.
    document_task(&task);
    let published = json(&workspace_env(
        &task,
        ["publish", "-m", "Publish the new result and its record"],
        &ROOMY_10MB,
    ));
    assert_eq!(published["status"], "pushed");
    assert!(
        published["changed_paths"]
            .as_array()
            .unwrap()
            .contains(&Value::from(format!("{stored}.dvc")))
    );
}

/// A task over its limit whose user chose to remove files from a directory
/// boundary in S3 publishes that cleanup even when it documents nothing: the
/// rewritten metadata names only objects the published metadata already
/// names, so it retires content. `plan` sees the pending rewrite and gives the
/// same record advice as `publish`.
#[cfg(feature = "test-storage")]
#[test]
fn a_file_removed_from_a_boundary_in_s3_is_a_cleanup_an_undocumented_task_publishes() {
    if !dvc_available() {
        return;
    }
    let fixture = managed_fixture(true);
    let (task_id, task) = create_task(&fixture, "boundary-cleanup", "20260920-111000");
    let branch = "refs/heads/codex/boundary-cleanup";
    let outputs = format!("{task_id}/outputs");
    let stored = format!("{task_id}/stored.bin");
    let pointer = format!("{outputs}.dvc");
    publish_boundaries_then_retire_the_record(&task, &outputs, &stored);

    let waiting = json(&workspace_env(&task, ["plan"], &OVER_300KB));
    assert_eq!(waiting["status"], "no_changes");
    assert_eq!(waiting["cloud_usage"]["status"], "approval_required");

    // The user chooses cleanup: drop one file of the directory boundary.
    workspace_env(
        &task,
        ["remove", format!("{outputs}/b.bin").as_str()],
        &OVER_300KB,
    );
    let plan = json(&workspace_env(&task, ["plan"], &OVER_300KB));
    assert_eq!(plan["status"], "dry_run");
    let usage = &plan["cloud_usage"];
    assert_eq!(usage["status"], "approval_required");
    assert_eq!(usage["cleanup_only"], true);
    assert_eq!(usage["publish_allowed"], true);
    assert_eq!(warning_codes(&plan), ["task-record-unchanged"]);
    let advice = plan["warnings"][0]["message"].as_str().unwrap().to_owned();
    assert!(
        advice.ends_with("record the decision in the first publication the limit allows"),
        "{advice}"
    );

    let removed = json(&workspace_env(
        &task,
        ["publish", "-m", "Remove one output"],
        &OVER_300KB,
    ));
    assert_eq!(removed["status"], "pushed", "{removed}");
    assert_eq!(removed["changed_paths"], serde_json::json!([pointer]));
    assert_eq!(removed["cloud_usage"]["cleanup_only"], true);
    assert_eq!(warning_codes(&removed), ["task-record-unchanged"]);
    assert_eq!(removed["warnings"][0]["message"], advice.as_str());
    let tip = removed["remote_oid"].as_str().unwrap().to_owned();
    assert!(
        show(&fixture.remote, &format!("{tip}:{pointer}")).contains("nfiles: 2"),
        "the published metadata lists the two remaining files"
    );

    // A retry of a publication whose metadata the storage engine already
    // rewrote is the same cleanup: `plan` judges the committed metadata too.
    workspace_env(
        &task,
        ["remove", format!("{outputs}/a.bin").as_str()],
        &OVER_300KB,
    );
    command(
        &fixture.shared,
        "dvc",
        ["commit", "-q", "--force", pointer.as_str()],
    );
    let retried = json(&workspace_env(&task, ["plan"], &OVER_300KB));
    assert_eq!(retried["status"], "dry_run");
    assert_eq!(retried["changed_paths"], serde_json::json!([pointer]));
    assert_eq!(retried["cloud_usage"]["cleanup_only"], true);

    // Published Git history cannot shrink, so the task stays over its limit,
    // and a record added now would be growth the limit refuses.
    document_task(&task);
    assert_refused(
        &workspace_env_unchecked(&task, ["publish", "-m", "Record the cleanup"], &OVER_300KB),
        APPROVAL_REFUSAL,
    );
    assert_eq!(rev(&fixture.remote, branch).as_deref(), Some(tip.as_str()));
}

/// A result kept local before it was ever published is a cleanup while its
/// task waits for the user's cloud-usage decision: a record added to that
/// publication would make it growth the limit refuses, so the documentation
/// guard waits for the gate's measurement instead of refusing on the preview,
/// and `plan` reports the usage the agent needs. Once an approval covers the
/// task, such a record is content again, so the record the warning deferred
/// goes into the first publication the limit allows.
#[cfg(feature = "test-storage")]
#[test]
fn a_result_kept_local_before_it_was_published_is_a_cleanup_while_the_task_waits() {
    let fixture = managed_fixture(false);
    let (task_id, task) = create_task(&fixture, "withheld-cleanup", "20260920-112000");
    let branch = "refs/heads/codex/withheld-cleanup";
    let fresh = format!("{task_id}/fresh.bin");
    let second = format!("{task_id}/second.bin");
    // Published Git history alone takes the task past a 300 kB limit, and the
    // task documents nothing once its record is retired.
    document_task(&task);
    std::fs::write(task.join("table.bin"), noise(400_000, 21)).unwrap();
    let published = json(&workspace_env(
        &task,
        ["publish", "-m", "Publish the table"],
        &ROOMY_10MB,
    ));
    assert_eq!(published["status"], "pushed");
    std::fs::remove_file(task.join("record.md")).unwrap();
    let retired = json(&workspace_env(
        &task,
        ["publish", "-m", "Retire the record"],
        &ROOMY_10MB,
    ));
    assert_eq!(retired["status"], "pushed");
    let waiting = json(&workspace_env(&task, ["plan"], &OVER_300KB));
    assert_eq!(waiting["cloud_usage"]["status"], "approval_required");
    assert_eq!(waiting["cloud_usage"]["git_history_exceeds_limit"], true);

    // A new result appears; the user declines a higher limit and chooses to
    // keep it on this machine.
    std::fs::write(task.join("fresh.bin"), noise(200_000, 22)).unwrap();
    workspace_env(&task, ["untrack", fresh.as_str()], &OVER_300KB);
    let plan = json(&workspace_env(&task, ["plan"], &OVER_300KB));
    let usage = &plan["cloud_usage"];
    assert_eq!(usage["status"], "approval_required");
    assert_eq!(usage["cleanup_only"], true);
    assert_eq!(usage["publish_allowed"], true);
    assert_eq!(warning_codes(&plan), ["task-record-unchanged"]);
    let advice = plan["warnings"][0]["message"].as_str().unwrap().to_owned();
    assert!(
        advice.ends_with("record the decision in the first publication the limit allows"),
        "{advice}"
    );
    let kept = json(&workspace_env(
        &task,
        ["publish", "-m", "Keep the new result local"],
        &OVER_300KB,
    ));
    assert_eq!(kept["status"], "pushed");
    assert_eq!(
        kept["changed_paths"],
        serde_json::json!([
            format!("{task_id}/.gitignore"),
            format!("{fresh}.workspace-mgr-storage.toml"),
        ])
    );
    assert_eq!(kept["cloud_usage"]["cleanup_only"], true);
    assert_eq!(kept["warnings"][0]["message"], advice.as_str());
    let tip = kept["remote_oid"].as_str().unwrap().to_owned();
    assert!(!tree_contains(&fixture.remote, &tip, &fresh));

    // The user then approves a limit that covers the task. Another result
    // kept local is content again, refused after the measurement, which the
    // refusal leaves without a pending decision.
    workspace_env(
        &task,
        [
            "task",
            "approve-cloud-usage",
            "--limit",
            "1MiB",
            "--note",
            "The user approved 1 MiB for the table",
        ],
        &OVER_300KB,
    );
    std::fs::write(task.join("second.bin"), noise(1_000, 23)).unwrap();
    workspace_env(&task, ["untrack", second.as_str()], &OVER_300KB);
    for args in [
        &["plan"][..],
        &["publish", "-m", "Keep another result local"][..],
    ] {
        assert_refused(
            &workspace_env_unchecked(&task, args, &OVER_300KB),
            UNDOCUMENTED,
        );
    }
    assert_eq!(rev(&fixture.remote, branch).as_deref(), Some(tip.as_str()));
    let status = json(&workspace_env(&task, ["task", "status"], &OVER_300KB));
    assert_eq!(status["cloud_usage"]["pending"], Value::Null);
    document_task(&task);
    let recorded = json(&workspace_env(
        &task,
        ["publish", "-m", "Record the decisions"],
        &OVER_300KB,
    ));
    assert_eq!(recorded["status"], "pushed");
    assert_eq!(recorded["cloud_usage"]["status"], "within_limit");
    assert!(
        recorded["changed_paths"]
            .as_array()
            .unwrap()
            .contains(&Value::from(format!("{second}.workspace-mgr-storage.toml")))
    );
}

/// Rewritten S3 metadata retires content only when every line it adds records
/// one of the entries it keeps. A description, arbitrary `meta`, or a comment
/// added to published metadata is text the publication carries although the
/// storage engine sees no change, so a task that documents nothing is refused.
#[cfg(feature = "test-storage")]
#[test]
fn text_added_to_published_s3_metadata_is_content() {
    if !dvc_available() {
        return;
    }
    let fixture = managed_fixture(true);
    let (task_id, task) = create_task(&fixture, "metadata-text", "20260920-113000");
    let branch = "refs/heads/codex/metadata-text";
    let outputs = format!("{task_id}/outputs");
    let stored = format!("{task_id}/stored.bin");
    let pointer = format!("{stored}.dvc");
    publish_boundaries_then_retire_the_record(&task, &outputs, &stored);
    let tip = rev(&fixture.remote, branch);
    let published = std::fs::read_to_string(task.join("stored.bin.dvc")).unwrap();
    for added in [
        "desc: Final results of run 3, accuracy 0.93\n",
        "meta:\n  notes: |\n    Run 3 reached accuracy 0.93.\n",
        "# Run 3 reached accuracy 0.93.\n",
    ] {
        std::fs::write(task.join("stored.bin.dvc"), format!("{published}{added}")).unwrap();
        let engine = command(
            &fixture.shared,
            "dvc",
            ["status", "--json", pointer.as_str()],
        );
        assert_eq!(String::from_utf8_lossy(&engine.stdout).trim(), "{}");
        for args in [&["plan"][..], &["publish", "-m", "Annotate the result"][..]] {
            assert_refused(
                &workspace_env_unchecked(&task, args, &ROOMY_10MB),
                UNDOCUMENTED,
            );
        }
        assert_eq!(rev(&fixture.remote, branch), tip, "{added}");
    }
    std::fs::write(task.join("stored.bin.dvc"), &published).unwrap();
    let plan = json(&workspace_env(&task, ["plan"], &ROOMY_10MB));
    assert_eq!(plan["status"], "no_changes");
}

/// A pointer whose objects are only missing from the local cache is committed
/// back to the same metadata, so neither `plan` nor `publish` treats it as a
/// change to task content: neither gives record advice, and the documentation
/// guard does not refuse a task that documents nothing, even when the storage
/// engine cannot compare the files of a directory whose manifest went with the
/// cache.
#[cfg(feature = "test-storage")]
#[test]
fn outputs_missing_only_from_the_local_cache_change_no_task_content() {
    if !dvc_available() {
        return;
    }
    let fixture = managed_fixture(true);
    let (task_id, task) = create_task(&fixture, "cleared-cache", "20260920-114000");
    let outputs = format!("{task_id}/outputs");
    let stored = format!("{task_id}/stored.bin");
    publish_boundaries_then_retire_the_record(&task, &outputs, &stored);
    let settled = json(&workspace_env(&task, ["plan"], &ROOMY_10MB));
    assert_eq!(settled["status"], "no_changes");

    // The user deletes the cache to reclaim space, and the engine's own index
    // of directory listings is empty, as in a fresh environment.
    std::fs::remove_dir_all(fixture.shared.join(".dvc/cache")).unwrap();
    let site_cache = fixture.root.join("empty-engine-site-cache");
    let env = [
        ROOMY_10MB[0],
        ("DVC_SITE_CACHE_DIR", site_cache.to_str().unwrap()),
    ];
    let plan = json(&workspace_env(&task, ["plan"], &env));
    assert_eq!(plan["status"], "dry_run");
    assert_eq!(plan["changed_paths"], serde_json::json!([]));
    let dirty = plan["storage"]["s3"]["dirty_files"].as_array().unwrap();
    assert!(
        dirty.contains(&Value::from(format!("{outputs}.dvc"))),
        "{plan}"
    );
    assert!(
        dirty.contains(&Value::from(format!("{stored}.dvc"))),
        "{plan}"
    );
    assert!(plan.get("warnings").is_none(), "{plan}");

    let restored = json(&workspace_env(
        &task,
        ["publish", "-m", "Restore the cache"],
        &env,
    ));
    assert_eq!(restored["status"], "no_changes");
    assert!(restored.get("warnings").is_none(), "{restored}");
    let after = json(&workspace_env(&task, ["plan"], &env));
    assert_eq!(after["status"], "no_changes");

    // Without the directory's manifest anywhere the engine looks, it cannot
    // compare the directory's files, but their unchanged aggregate still
    // proves them unchanged.
    std::fs::remove_dir_all(fixture.shared.join(".dvc/cache")).unwrap();
    let storage_remote = fixture.root.join("storage-remote");
    let manifests = walkdir::WalkDir::new(&storage_remote)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.path().to_string_lossy().ends_with(".dir"))
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>();
    assert_eq!(manifests.len(), 1, "{manifests:?}");
    std::fs::remove_file(&manifests[0]).unwrap();
    let unlisted = json(&workspace_env(&task, ["plan"], &env));
    assert_eq!(unlisted["status"], "dry_run");
    assert!(unlisted.get("warnings").is_none(), "{unlisted}");
    let republished = json(&workspace_env(
        &task,
        ["publish", "-m", "Restore the manifest"],
        &env,
    ));
    assert_eq!(republished["status"], "no_changes");
    assert!(republished.get("warnings").is_none(), "{republished}");
    assert!(
        manifests[0].is_file(),
        "the publication uploads the manifest again"
    );
}

/// A background writer may change a boundary's outputs after the preview asked
/// the storage engine and before `publish` commits them. The documentation
/// guard judges the committed metadata again before the upload, so such
/// content is refused with nothing uploaded rather than uploaded and then
/// refused.
#[cfg(all(unix, feature = "test-storage"))]
#[test]
fn content_written_after_the_preview_is_refused_before_the_upload() {
    use std::os::unix::fs::PermissionsExt;

    if !dvc_available() {
        return;
    }
    let fixture = managed_fixture(true);
    let (task_id, task) = create_task(&fixture, "late-content", "20260920-115000");
    let branch = "refs/heads/codex/late-content";
    let outputs = format!("{task_id}/outputs");
    let stored = format!("{task_id}/stored.bin");
    publish_boundaries_then_retire_the_record(&task, &outputs, &stored);
    // The user's chosen cleanup, which a task that documents nothing may
    // publish.
    workspace_env(
        &task,
        ["remove", format!("{outputs}/b.bin").as_str()],
        &ROOMY_10MB,
    );
    let plan = json(&workspace_env(&task, ["plan"], &ROOMY_10MB));
    assert_eq!(plan["status"], "dry_run");
    assert_eq!(warning_codes(&plan), ["task-record-unchanged"]);

    // A writer lands inside the boundary right before the engine commits it.
    let engine = which::which("dvc").unwrap();
    let writer = fixture.root.join("late-writer");
    std::fs::write(
        &writer,
        format!(
            "#!/bin/sh\nif [ \"$1\" = commit ]; then\n  head -c 5000 /dev/urandom > \"$LATE_TARGET\"\nfi\nexec '{}' \"$@\"\n",
            engine.display()
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&writer).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&writer, permissions).unwrap();
    let late = task.join("outputs/late.bin");
    let tip = rev(&fixture.remote, branch);
    let storage_remote = fixture.root.join("storage-remote");
    let storage_before = remote_snapshot(&storage_remote);
    let pointer_before = std::fs::read(task.join("outputs.dvc")).unwrap();
    let refused = workspace_env_unchecked(
        &task,
        ["publish", "-m", "Remove one output"],
        &[
            ROOMY_10MB[0],
            ("WORKSPACE_MGR_STORAGE_DVC", writer.to_str().unwrap()),
            ("LATE_TARGET", late.to_str().unwrap()),
        ],
    );
    assert_refused(&refused, UNDOCUMENTED);
    assert!(late.is_file());
    assert_eq!(remote_snapshot(&storage_remote), storage_before);
    assert_eq!(rev(&fixture.remote, branch), tip);
    // The refusal restores the metadata the engine committed, so it does not
    // keep naming the late file once that is gone.
    assert_eq!(
        std::fs::read(task.join("outputs.dvc")).unwrap(),
        pointer_before
    );

    // Without the late file, the cleanup publishes as planned.
    std::fs::remove_file(&late).unwrap();
    let removed = json(&workspace_env(
        &task,
        ["publish", "-m", "Remove one output"],
        &ROOMY_10MB,
    ));
    assert_eq!(removed["status"], "pushed");
    assert_eq!(
        removed["changed_paths"],
        serde_json::json!([format!("{outputs}.dvc")])
    );
}

/// Publishes a directory boundary `outputs/` (three 200 kB files) and a file
/// boundary `stored.bin` (100 kB) in S3 with a record, then retires the record,
/// leaving a task that documents nothing, as one published before the guard
/// existed would be.
#[cfg(feature = "test-storage")]
fn publish_boundaries_then_retire_the_record(task: &Path, outputs: &str, stored: &str) {
    document_task(task);
    std::fs::create_dir(task.join("outputs")).unwrap();
    std::fs::write(task.join("outputs/a.bin"), noise(200_000, 11)).unwrap();
    std::fs::write(task.join("outputs/b.bin"), noise(200_000, 12)).unwrap();
    std::fs::write(task.join("outputs/c.bin"), noise(200_000, 17)).unwrap();
    std::fs::write(task.join("stored.bin"), noise(100_000, 13)).unwrap();
    for path in [outputs, stored] {
        workspace_env(
            task,
            [
                "storage",
                "set",
                path,
                "--to",
                "s3",
                "--reason",
                "Keep the results in storage.",
            ],
            &ROOMY_10MB,
        );
    }
    let published = json(&workspace_env(
        task,
        ["publish", "-m", "Publish results"],
        &ROOMY_10MB,
    ));
    assert_eq!(published["status"], "pushed");
    std::fs::remove_file(task.join("record.md")).unwrap();
    let retired = json(&workspace_env(
        task,
        ["publish", "-m", "Retire the record"],
        &ROOMY_10MB,
    ));
    assert_eq!(retired["status"], "pushed");
}
