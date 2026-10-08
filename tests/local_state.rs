mod common;

use common::*;
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

const LOCAL_STATE: &str = ".workspace-mgr/local";
const LEGACY_STATE: &str = ".git/workspace-mgr";

fn managed_fixture() -> GitFixture {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["manage"]);
    fixture.commit_seed("Add workspace policy");
    fixture.clone_shared();
    fixture
}

fn write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn hold_lock(path: &Path) -> File {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    file.try_lock_exclusive().unwrap();
    file
}

fn create_infrastructure_task(fixture: &GitFixture) -> PathBuf {
    let created = workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "state-location",
            "--kind",
            "infrastructure",
            "--title",
            "Local state location",
            "--purpose",
            "Keep shared infrastructure task state through an upgrade.",
            "--scope",
            "shared-policy.md",
            "--scope-note",
            "The user requested this repository-wide policy change.",
        ],
    );
    PathBuf::from(json(&created)["manifest"].as_str().unwrap())
}

fn separate_git_fixture() -> (GitFixture, PathBuf) {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["manage"]);
    fixture.commit_seed("Add workspace policy");
    let git_directory = fixture.root.join("git-metadata");
    command(
        &fixture.root,
        "git",
        [
            "clone",
            "--separate-git-dir",
            git_directory.to_str().unwrap(),
            fixture.remote.to_str().unwrap(),
            fixture.shared.to_str().unwrap(),
        ],
    );
    configure_git(&fixture.shared);
    (fixture, git_directory)
}

#[test]
fn local_state_is_ignored_while_shared_configuration_remains_trackable() {
    let fixture = GitFixture::new();
    fixture.clone_shared();
    workspace(&fixture.shared, ["manage"]);

    let lock = fixture.shared.join(LOCAL_STATE).join("repository.lock");
    assert!(lock.is_file());
    assert!(!fixture.shared.join(LEGACY_STATE).exists());
    let ignored = git_unchecked(
        &fixture.shared,
        ["check-ignore", ".workspace-mgr/local/repository.lock"],
    );
    assert!(ignored.status.success());
    for path in [
        ".workspace-mgr/repository.gitignore",
        ".workspace-mgr/instructions/repository.md",
    ] {
        write(&fixture.shared.join(path), b"shared configuration\n");
        assert_eq!(
            git_unchecked(&fixture.shared, ["check-ignore", path])
                .status
                .code(),
            Some(1),
            "shared configuration must stay trackable: {path}"
        );
    }
    git(&fixture.shared, ["add", "-A"]);
    let staged =
        String::from_utf8(git(&fixture.shared, ["diff", "--cached", "--name-only"]).stdout)
            .unwrap();
    assert!(staged.contains(".workspace-mgr/repository.gitignore\n"));
    assert!(staged.contains(".workspace-mgr/instructions/repository.md\n"));
    assert!(!staged.contains(".workspace-mgr/local/"));
}

#[test]
fn publication_excludes_private_state_before_the_legacy_ignore_rules_are_upgraded() {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["manage"]);
    let root_ignore = fixture.seed.join(".gitignore");
    let current_ignore = fs::read_to_string(&root_ignore).unwrap();
    let legacy_ignore = current_ignore.replace("/.workspace-mgr/local/\n", "");
    assert_ne!(legacy_ignore, current_ignore);
    fs::write(&root_ignore, &legacy_ignore).unwrap();
    let policy = ".workspace-mgr/shared-policy.md";
    write(&fixture.seed.join(policy), b"original shared policy\n");
    // The seed's current private state is unignored in this simulated old
    // scaffold, so commit only the repository-owned files explicitly.
    git(
        &fixture.seed,
        [
            "add",
            "--",
            ".workspace-mgr.toml",
            "AGENTS.md",
            ".gitignore",
            policy,
        ],
    );
    git(
        &fixture.seed,
        ["commit", "-m", "Seed the previous ignore scaffold"],
    );
    git(&fixture.seed, ["push", "origin", "main"]);
    fixture.clone_shared();

    let created = json(&workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "legacy-ignore",
            "--kind",
            "infrastructure",
            "--title",
            "Upgrade shared workspace policy",
            "--purpose",
            "Publish shared policy without publishing private state.",
            "--scope",
            ".workspace-mgr",
            "--scope-note",
            "The user requested shared workspace policy updates.",
        ],
    ));
    let manifest = PathBuf::from(created["manifest"].as_str().unwrap());
    let private = fixture
        .shared
        .join(LOCAL_STATE)
        .join("state/retained-private.bin");
    write(&private, b"private state must never publish\n");
    write(&fixture.shared.join(policy), b"updated shared policy\n");
    assert_eq!(
        git_unchecked(
            &fixture.shared,
            [
                "check-ignore",
                ".workspace-mgr/local/state/retained-private.bin"
            ],
        )
        .status
        .code(),
        Some(1),
        "the fixture must exercise protection before the new ignore rule exists"
    );

    let plan = json(&workspace(
        &fixture.shared,
        ["plan", "--manifest", manifest.to_str().unwrap()],
    ));
    assert_eq!(plan["changed_paths"], serde_json::json!([policy]));
    let published = json(&workspace(
        &fixture.shared,
        [
            "publish",
            "--manifest",
            manifest.to_str().unwrap(),
            "-m",
            "Publish shared workspace policy before ignore reconciliation",
        ],
    ));
    assert_eq!(published["status"], "pushed");
    assert_eq!(published["changed_paths"], serde_json::json!([policy]));
    let commit = published["commit_oid"].as_str().unwrap();
    assert!(
        git(
            &fixture.remote,
            ["ls-tree", "-r", "--name-only", commit, "--", LOCAL_STATE],
        )
        .stdout
        .is_empty(),
        "the pushed tree must not retain any private state"
    );
    assert_eq!(
        git(&fixture.remote, ["show", &format!("{commit}:{policy}")]).stdout,
        b"updated shared policy\n"
    );
    assert_eq!(
        fs::read(&private).unwrap(),
        b"private state must never publish\n"
    );
    assert!(manifest.is_file());
    assert_eq!(
        fs::read_to_string(fixture.shared.join(".gitignore")).unwrap(),
        legacy_ignore,
        "publication must protect private state without requiring init first"
    );
}

#[test]
fn ignored_private_state_does_not_break_task_or_repository_scoped_publication() {
    for infrastructure in [false, true] {
        let fixture = GitFixture::new();
        workspace(&fixture.seed, ["manage"]);
        let policy = ".workspace-mgr/shared-policy.md";
        let obsolete_policy = ".workspace-mgr/obsolete-policy.md";
        // The E2E failure requires tracked shared configuration underneath
        // .workspace-mgr; a fixture containing only its ignored local state
        // does not exercise the same Git pathspec behavior.
        write(&fixture.seed.join(policy), b"original shared policy\n");
        write(
            &fixture.seed.join(obsolete_policy),
            b"obsolete shared policy\n",
        );
        fixture.commit_seed("Add shared workspace policy");
        fixture.clone_shared();
        let mut create = vec![
            "task",
            "create",
            "ignored-state",
            "--title",
            "Ignored private state",
            "--purpose",
            "Stage scoped changes while keeping ignored private state intact.",
        ];
        if infrastructure {
            create.extend([
                "--kind",
                "infrastructure",
                "--scope",
                ".workspace-mgr",
                "--scope-note",
                "The user requested these shared workspace policy updates.",
            ]);
        } else {
            create.extend(["--timestamp", "20261006-160000"]);
        }
        let created = json(&workspace(&fixture.shared, create));
        let manifest = PathBuf::from(created["manifest"].as_str().unwrap());
        let private = fixture
            .shared
            .join(LOCAL_STATE)
            .join("state/retained-private.bin");
        write(&private, b"ignored private state\0must remain local\n");
        assert!(
            git_unchecked(
                &fixture.shared,
                [
                    "check-ignore",
                    ".workspace-mgr/local/state/retained-private.bin"
                ],
            )
            .status
            .success()
        );
        let mut plan_args = vec!["plan", "--manifest", manifest.to_str().unwrap()];
        let mut publish_args = vec![
            "publish",
            "--manifest",
            manifest.to_str().unwrap(),
            "-m",
            "Publish scoped changes with ignored private state",
        ];
        if infrastructure {
            write(&fixture.shared.join(policy), b"updated shared policy\n");
            fs::remove_file(fixture.shared.join(obsolete_policy)).unwrap();
        } else {
            git(&fixture.shared, ["switch", "-c", "alternate-checkout"]);
            for args in [&mut plan_args, &mut publish_args] {
                args.extend([
                    "--allow-non-shared-head",
                    "--scope-note",
                    "The user authorized this alternate checkout workflow.",
                ]);
            }
        }
        let plan = json(&workspace(&fixture.shared, plan_args));
        if infrastructure {
            assert_eq!(
                plan["changed_paths"],
                serde_json::json!([obsolete_policy, policy])
            );
        } else {
            assert!(
                plan["changed_paths"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|path| {
                        path.as_str()
                            .unwrap()
                            .starts_with("20261006-160000-ignored-state/")
                    })
            );
        }
        let published = json(&workspace(&fixture.shared, publish_args));
        assert_eq!(published["status"], "pushed");
        assert_eq!(published["changed_paths"], plan["changed_paths"]);
        let commit = published["commit_oid"].as_str().unwrap();
        assert!(
            git(
                &fixture.remote,
                ["ls-tree", "-r", "--name-only", commit, "--", LOCAL_STATE],
            )
            .stdout
            .is_empty()
        );
        if infrastructure {
            assert_eq!(
                git(&fixture.remote, ["show", &format!("{commit}:{policy}")]).stdout,
                b"updated shared policy\n"
            );
            assert!(
                !git_unchecked(
                    &fixture.remote,
                    ["cat-file", "-e", &format!("{commit}:{obsolete_policy}")],
                )
                .status
                .success(),
                "staging must also retain the deletion of a tracked scope file"
            );
        }
        assert_eq!(
            fs::read(&private).unwrap(),
            b"ignored private state\0must remain local\n"
        );
        assert!(manifest.is_file());
    }
}

#[test]
fn migration_preserves_private_state_and_accepts_the_legacy_manifest_path() {
    let fixture = managed_fixture();
    let manifest = create_infrastructure_task(&fixture);
    let raw_manifest = fs::read(&manifest).unwrap();
    let relative_manifest = manifest
        .strip_prefix(fixture.shared.join(LOCAL_STATE).canonicalize().unwrap())
        .unwrap()
        .to_path_buf();
    let local = fixture.shared.join(LOCAL_STATE);
    let legacy = fixture.shared.join(LEGACY_STATE);
    fs::rename(&local, &legacy).unwrap();
    let legacy_manifest = legacy.join(&relative_manifest);

    let retained = [
        ("state/legacy/private.index", &b"private index\0bytes"[..]),
        (
            "state/legacy/cloud-usage.json",
            &b"{\"pending\":true}\n"[..],
        ),
        ("s3-purge.json", &b"{\"retained\":\"purge queue\"}\n"[..]),
        ("archive/legacy.json", &b"{\"status\":\"copied\"}\n"[..]),
        (
            "discard-quarantine/legacy/payload.bin",
            &b"retained payload\0"[..],
        ),
        (
            "discard-quarantine/legacy/local/task.toml",
            &b"retained user file called task.toml\0"[..],
        ),
        (
            "discard-quarantine/legacy/repository.lock",
            &b"retained user file called repository.lock\0"[..],
        ),
    ];
    for (relative, bytes) in retained {
        write(&legacy.join(relative), bytes);
    }

    let status = workspace(
        &fixture.root,
        [
            "task",
            "status",
            "--manifest",
            legacy_manifest.to_str().unwrap(),
        ],
    );
    assert_eq!(json(&status)["task_id"], "infra-state-location");
    assert_eq!(
        fs::read(local.join(&relative_manifest)).unwrap(),
        raw_manifest
    );
    for (relative, bytes) in retained {
        assert_eq!(fs::read(local.join(relative)).unwrap(), bytes, "{relative}");
        assert!(!legacy.join(relative).exists(), "{relative} did not move");
    }
    assert!(!legacy_manifest.exists());
    // Previously printed explicit selections also work after migration.
    let repeated = workspace(
        &fixture.root,
        [
            "task",
            "status",
            "--manifest",
            legacy_manifest.to_str().unwrap(),
        ],
    );
    assert_eq!(json(&repeated)["task_id"], "infra-state-location");
    let current = workspace(
        &fixture.shared,
        ["task", "status", "--manifest", manifest.to_str().unwrap()],
    );
    assert_eq!(json(&current)["task_id"], "infra-state-location");
}

#[test]
fn a_legacy_singular_infrastructure_manifest_is_migrated_and_remains_selectable() {
    let fixture = managed_fixture();
    let manifest = create_infrastructure_task(&fixture);
    let raw = fs::read(&manifest).unwrap();
    let legacy_manifest = fixture.shared.join(LEGACY_STATE).join("task.toml");
    write(&legacy_manifest, &raw);
    fs::remove_dir_all(manifest.parent().unwrap()).unwrap();

    for _ in 0..2 {
        let status = workspace(
            &fixture.root,
            [
                "task",
                "status",
                "--manifest",
                legacy_manifest.to_str().unwrap(),
            ],
        );
        assert_eq!(json(&status)["task_id"], "infra-state-location");
    }
    assert_eq!(fs::read(&manifest).unwrap(), raw);
    assert!(!legacy_manifest.exists());
}

#[test]
fn migration_merges_distinct_state_without_overwriting_existing_files() {
    let fixture = managed_fixture();
    let local = fixture.shared.join(LOCAL_STATE);
    let legacy = fixture.shared.join(LEGACY_STATE);
    write(&local.join("state/new/private.index"), b"new index");
    write(&legacy.join("state/old/private.index"), b"old index");
    write(&local.join("archive/same.json"), b"same retained journal");
    write(&legacy.join("archive/same.json"), b"same retained journal");

    workspace(&fixture.shared, ["manage"]);
    assert_eq!(
        fs::read(local.join("state/new/private.index")).unwrap(),
        b"new index"
    );
    assert_eq!(
        fs::read(local.join("state/old/private.index")).unwrap(),
        b"old index"
    );
    assert_eq!(
        fs::read(local.join("archive/same.json")).unwrap(),
        b"same retained journal"
    );
    assert!(!legacy.join("state/old/private.index").exists());
    assert!(!legacy.join("archive/same.json").exists());
}

#[test]
fn conflicting_state_refuses_migration_and_preserves_both_versions() {
    let fixture = managed_fixture();
    let local = fixture.shared.join(LOCAL_STATE);
    let legacy = fixture.shared.join(LEGACY_STATE);
    write(
        &local.join("state/task/cloud-usage.json"),
        b"new measurement",
    );
    write(
        &legacy.join("state/task/cloud-usage.json"),
        b"old measurement",
    );
    write(&legacy.join("archive/preserved.json"), b"legacy journal");

    let rejected = workspace_unchecked(&fixture.shared, ["manage"]);
    assert_eq!(rejected.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("conflicting workspace-mgr local state")
    );
    assert_eq!(
        fs::read(local.join("state/task/cloud-usage.json")).unwrap(),
        b"new measurement"
    );
    assert_eq!(
        fs::read(legacy.join("state/task/cloud-usage.json")).unwrap(),
        b"old measurement"
    );
    assert_eq!(
        fs::read(legacy.join("archive/preserved.json")).unwrap(),
        b"legacy journal",
        "a conflict must be detected before any legacy data is moved"
    );
}

#[test]
fn an_active_legacy_operation_blocks_migration_until_its_lock_is_released() {
    let fixture = managed_fixture();
    let legacy = fixture.shared.join(LEGACY_STATE);
    write(&legacy.join("state/task/private.index"), b"in-use index");
    let lock = hold_lock(&legacy.join("repository.lock"));

    let rejected = workspace_unchecked(&fixture.shared, ["manage"]);
    assert_eq!(rejected.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("repository operation"));
    assert_eq!(
        fs::read(legacy.join("state/task/private.index")).unwrap(),
        b"in-use index"
    );
    assert!(
        !fixture
            .shared
            .join(LOCAL_STATE)
            .join("state/task/private.index")
            .exists()
    );

    drop(lock);
    workspace(&fixture.shared, ["manage"]);
    assert_eq!(
        fs::read(
            fixture
                .shared
                .join(LOCAL_STATE)
                .join("state/task/private.index")
        )
        .unwrap(),
        b"in-use index"
    );
}

#[test]
fn linked_worktrees_use_the_shared_checkout_repository_lock() {
    let fixture = managed_fixture();
    workspace(&fixture.shared, ["manage"]);
    let linked = fixture.root.join("linked");
    git(
        &fixture.shared,
        ["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
    );
    // The state anchor is the primary checkout itself, even when its current
    // branch changes while another worktree still needs the repository lock.
    git(
        &fixture.shared,
        ["switch", "-c", "alternate-primary-branch"],
    );
    let lock = hold_lock(&fixture.shared.join(LOCAL_STATE).join("repository.lock"));

    let rejected = workspace_unchecked(&linked, ["refresh"]);
    assert_eq!(rejected.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("repository operation"));
    assert!(!linked.join(LOCAL_STATE).exists());
    drop(lock);

    // The command can now reach its normal shared-branch check, proving the
    // refusal above came from the same lock rather than the linked branch.
    let unlocked = workspace_unchecked(&linked, ["refresh"]);
    assert_eq!(unlocked.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&unlocked.stderr).contains("refresh requires"));
    assert!(!linked.join(LOCAL_STATE).exists());
}

#[test]
fn repositories_with_a_separate_git_directory_keep_state_in_the_checkout() {
    let (fixture, git_directory) = separate_git_fixture();
    // Git does not record a reverse pointer to a separate primary checkout.
    // An explicit core.worktree makes that checkout discoverable from any
    // linked worktree without guessing another local-state directory.
    git(
        &fixture.shared,
        ["config", "core.worktree", fixture.shared.to_str().unwrap()],
    );
    let legacy_index = git_directory.join("workspace-mgr/state/task/private.index");
    write(&legacy_index, b"separate git private index");

    workspace(&fixture.shared, ["manage"]);
    assert_eq!(
        fs::read(
            fixture
                .shared
                .join(LOCAL_STATE)
                .join("state/task/private.index")
        )
        .unwrap(),
        b"separate git private index"
    );
    assert!(!legacy_index.exists());
    assert!(fixture.shared.join(".git").is_file());
    assert!(
        fixture
            .shared
            .join(LOCAL_STATE)
            .join("repository.lock")
            .is_file()
    );
    assert!(
        git_unchecked(
            &fixture.shared,
            [
                "check-ignore",
                ".workspace-mgr/local/state/task/private.index"
            ],
        )
        .status
        .success()
    );
}

#[test]
fn linked_worktrees_with_a_separate_git_directory_share_the_primary_lock() {
    let (fixture, _git_directory) = separate_git_fixture();
    git(
        &fixture.shared,
        ["config", "core.worktree", fixture.shared.to_str().unwrap()],
    );
    workspace(&fixture.shared, ["manage"]);
    let linked = fixture.root.join("linked");
    git(
        &fixture.shared,
        ["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
    );
    let lock = hold_lock(&fixture.shared.join(LOCAL_STATE).join("repository.lock"));
    let rejected = workspace_unchecked(&linked, ["refresh"]);
    assert_eq!(rejected.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("repository operation"));
    assert!(!linked.join(LOCAL_STATE).exists());
    drop(lock);
    let unlocked = workspace_unchecked(&linked, ["refresh"]);
    assert_eq!(unlocked.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&unlocked.stderr).contains("refresh requires"));
}

#[test]
fn a_separate_git_directory_without_a_primary_worktree_pointer_refuses_migration() {
    let (fixture, git_directory) = separate_git_fixture();
    let legacy_index = git_directory.join("workspace-mgr/state/task/private.index");
    write(&legacy_index, b"unmigrated private index");
    let rejected = workspace_unchecked(&fixture.shared, ["manage"]);
    assert_eq!(rejected.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("core.worktree"));
    assert_eq!(
        fs::read(&legacy_index).unwrap(),
        b"unmigrated private index"
    );
    assert!(!fixture.shared.join(LOCAL_STATE).exists());
    assert!(!git_directory.join(LOCAL_STATE).exists());
}
