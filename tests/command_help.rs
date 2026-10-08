mod common;

use std::fs;
use std::path::Path;

use common::binary_command;

const PAGES: &[(&[&str], &[&str])] = &[
    (&["setup"], &["native storage", "Python", "user approval"]),
    (
        &["manage"],
        &["product-owned", "infrastructure", "versioning"],
    ),
    (
        &["instructions"],
        &["mental model", "command --help", "on-demand"],
    ),
    (
        &["doctor"],
        &["scaffolding", "specific causes", "task scripts"],
    ),
    (&["config"], &["minimum_cli_version", "non-secret"]),
    (&["config", "show"], &["minimum_cli_version", "non-secret"]),
    (&["task"], &["task <operation> --help", "Reuse one task"]),
    (
        &["task", "list"],
        &[
            "legacy candidates",
            "offline",
            "placement does not imply completion",
        ],
    ),
    (
        &["task", "path"],
        &["ambiguous", "offline", "absolute path"],
    ),
    (
        &["task", "show"],
        &["Ambiguous", "private manifest", "offline"],
    ),
    (
        &["task", "create"],
        &[
            "initial scaffold",
            "Directory map",
            "hard-to-reproduce",
            "exactly one draft PR",
        ],
    ),
    (
        &["task", "adopt"],
        &[
            "legacy directory",
            "--pull-request is optional",
            "without querying old PRs or creating a review record",
            "known merged PR",
            "live control metadata",
            "historical Git tree",
            "Existing content remains unchanged",
        ],
    ),
    (
        &["task", "rename"],
        &[
            "immutable task ID",
            "Payloads move unchanged",
            "does not scan or repair",
        ],
    ),
    (
        &["task", "upgrade"],
        &[
            "current manifest",
            "historical task configuration",
            "not archive prerequisites",
        ],
    ),
    (
        &["task", "status"],
        &["complete publication state", "read-only"],
    ),
    (
        &["task", "discard"],
        &[
            "exact-task-id",
            "ignored, hydrated and local-only",
            "not an archive rollback",
        ],
    ),
    (
        &["task", "approve-cloud-usage"],
        &[
            "this chat",
            "Cloud-Usage-Approval",
            "Do not raise the limit automatically",
        ],
    ),
    (
        &["plan"],
        &[
            "Whitespace",
            "approval_required",
            "machine-local-ignore",
            "semantic-placement-review",
        ],
    ),
    (
        &["publish"],
        &[
            "private index",
            "bulk-publication",
            "final no-change plan",
            "cloud-approval pause",
        ],
    ),
    (
        &["storage"],
        &["storage <operation> --help", "size", "semantic"],
    ),
    (
        &["storage", "status"],
        &[
            "structured warnings",
            "semantic-placement-review",
            "read-only",
        ],
    ),
    (
        &["storage", "set"],
        &[
            "explicit",
            "Published placement stays stable",
            "permanently purged",
        ],
    ),
    (
        &["storage", "reset"],
        &[
            "Local-only paths refuse reset",
            "Published placement remains stable",
        ],
    ),
    (
        &["storage", "hydrate"],
        &[
            "exact version references",
            "Existing local modifications",
            "archive registry",
        ],
    ),
    (
        &["move"],
        &["no-clobber", "payloads remain unchanged", "delete markers"],
    ),
    (
        &["archive"],
        &[
            "OPEN PR means pending",
            "MERGED, CLOSED without merging",
            "successful query finding no corresponding PR means done",
            "hosting-query failures are errors, never a no-PR result",
            "current task configuration",
            "complete S3",
            "--cancel",
            "zero old-prefix",
        ],
    ),
    (
        &["remove"],
        &["destructive local scope", "all versions and delete markers"],
    ),
    (
        &["untrack"],
        &[
            "New clones receive no local-only payload",
            "README and manifest",
            "permanently purged",
        ],
    ),
    (
        &["refresh"],
        &["preserving overlays", "another worktree", "never organizes"],
    ),
];

fn help(cwd: &Path, args: &[&str]) -> String {
    let output = binary_command()
        .args(args)
        .arg("--help")
        .current_dir(cwd)
        .env("WORKSPACE_MGR_FORMAT", "json")
        .output()
        .expect("run offline command help");
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "help must not contact an update service or emit operational diagnostics: {args:?}"
    );
    String::from_utf8(output.stdout).expect("UTF-8 help")
}

#[test]
fn every_operation_has_contextual_help_without_a_repository() {
    let temp = tempfile::tempdir().unwrap();
    // A tempting but invalid configuration must never be parsed by --help.
    fs::write(temp.path().join(".workspace-mgr.toml"), "invalid = [").unwrap();
    for (args, expected) in PAGES {
        let output = help(temp.path(), args);
        assert!(output.contains("Usage:"), "{args:?} lacks Clap usage");
        assert!(output.contains("Options:"), "{args:?} lacks flags");
        let lowercase = output
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        for fragment in *expected {
            assert!(
                lowercase.contains(&fragment.to_ascii_lowercase()),
                "{args:?} lacks {fragment:?}:\n{output}"
            );
        }
        if *args == ["task", "adopt"] {
            let usage = output
                .split("Usage:")
                .nth(1)
                .unwrap()
                .split("\n\n")
                .next()
                .unwrap();
            assert!(
                !usage.contains("--pull-request"),
                "adoption usage still requires a PR: {usage}"
            );
        }
        assert!(
            !temp.path().join(".workspace-mgr").exists(),
            "{args:?} wrote managed state"
        );
    }
    assert_eq!(
        fs::read_to_string(temp.path().join(".workspace-mgr.toml")).unwrap(),
        "invalid = ["
    );
}

#[test]
fn unrelated_help_does_not_include_relocation_notices_or_publication_policy() {
    let temp = tempfile::tempdir().unwrap();
    for args in [
        vec!["task", "list"],
        vec!["task", "path"],
        vec!["config", "show"],
        vec!["setup"],
    ] {
        let output = help(temp.path(), &args).to_ascii_lowercase();
        for irrelevant in [
            "broken link",
            "bad link",
            "bulk-publication",
            "task-record-unchanged",
            "directory map",
            "semantic-placement-review",
        ] {
            assert!(
                !output.contains(irrelevant),
                "{args:?} leaked {irrelevant:?}"
            );
        }
    }
    for args in [
        vec!["task", "status"],
        vec!["task", "rename"],
        vec!["task", "upgrade"],
        vec!["task", "approve-cloud-usage"],
    ] {
        let output = help(temp.path(), &args);
        assert!(
            !output.contains("--include"),
            "{args:?} advertises an unsupported scope flag"
        );
        if args[1] != "approve-cloud-usage" {
            assert!(
                !output.contains("--scope-note"),
                "{args:?} advertises an unsupported scope-note flag"
            );
        }
    }
    for args in [vec!["task", "rename"], vec!["archive"]] {
        let output = help(temp.path(), &args).to_ascii_lowercase();
        assert!(!output.contains("rename succeeded"));
        assert!(!output.contains("archive succeeded"));
    }
}

#[cfg(unix)]
#[test]
fn help_never_invokes_git_hosting_storage_or_runtime_tools() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let tools = temp.path().join("tools");
    fs::create_dir(&tools).unwrap();
    let marker = temp.path().join("unexpected-tool-call");
    for name in ["git", "gh", "aws", "dvc", "python", "python3", "curl"] {
        let path = tools.join(name);
        fs::write(
            &path,
            format!("#!/bin/sh\n: > '{}'\nexit 91\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let state = temp.path().join("private-state");
    for (args, _) in PAGES {
        let output = binary_command()
            .args(*args)
            .arg("--help")
            .current_dir(temp.path())
            .env("PATH", &tools)
            .env("XDG_CACHE_HOME", &state)
            .env(
                "AWS_SHARED_CREDENTIALS_FILE",
                temp.path().join("missing-credentials"),
            )
            .env("AWS_CONFIG_FILE", temp.path().join("missing-cloud-config"))
            .env("GH_HOST", "hosting.example.invalid")
            .env("WORKSPACE_MGR_UPDATE_CHECK_DISABLE", "0")
            .output()
            .expect("help without external tools");
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty(), "{args:?} produced diagnostics");
        assert!(!marker.exists(), "{args:?} invoked an external tool");
        assert!(!state.exists(), "{args:?} created a cache");
    }
}
