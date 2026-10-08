#[test]
fn user_documentation_routes_global_and_operation_specific_information() {
    let readme = include_str!("../README.md");
    let model = include_str!("../docs/management-model.md");
    let guide = include_str!("../docs/guide.md");
    let commands = include_str!("../docs/commands.md");
    let contributing = include_str!("../CONTRIBUTING.md");
    let architecture = include_str!("../docs/architecture.md");
    let audit = include_str!("../docs/control-plane-audit.md");
    assert!(readme.contains("docs/management-model.md"));
    assert!(readme.contains("docs/guide.md"));
    assert!(readme.contains("docs/commands.md"));
    assert!(readme.contains("docs/control-plane-audit.md"));
    assert!(guide.contains("management-model.md"));

    let normalized = model.split_whitespace().collect::<Vec<_>>().join(" ");
    for concept in [
        "general-purpose collaborator",
        "user-facing interface",
        "durable workspace",
        "one writable conversation (chat) = one task = one target branch = one draft pull request",
        "Task scope",
        "Storage placement",
        "Reading and ownership are separate",
        "Reading a path does not transfer ownership or authorize mutation",
        "default write boundary is its own task directory",
        "explicit user authorization for the exact path and action",
        "manifest scopes are its write boundary",
        "Untracked does not mean unowned",
        "current slug is a mutable topic label",
        "task ID and review branch remain stable",
        "instructions repository",
    ] {
        assert!(
            normalized.contains(concept),
            "global model omits {concept:?}"
        );
    }
    assert!(
        model.len() < 8_000,
        "global model became an operation manual"
    );
    for detail in [
        "small-s3-boundary",
        "task-record-unchanged",
        "bulk-publication",
        "checkpoint_tree",
        "delete markers",
        "many broken links",
        "manually audit and repair",
    ] {
        assert!(
            !model.contains(detail),
            "global model leaks operation detail {detail:?}"
        );
    }
    for command in [
        "setup",
        "manage",
        "instructions",
        "doctor",
        "config show",
        "task create",
        "task list",
        "task path",
        "task show",
        "task rename",
        "task upgrade",
        "archive",
        "task status",
        "task discard",
        "task approve-cloud-usage",
        "storage status",
        "storage set",
        "storage reset",
        "storage hydrate",
        "move",
        "remove",
        "untrack",
        "plan",
        "publish",
        "refresh",
    ] {
        assert!(
            commands.contains(&format!("## `workspace-mgr {command}`")),
            "missing command reference {command}"
        );
    }
    for (name, doc) in [("guide", guide), ("commands", commands)] {
        assert!(
            doc.contains("instructions repository"),
            "{name} loses repository-owned policy access"
        );
        assert!(
            doc.contains("policy hash"),
            "{name} omits effective-policy change detection"
        );
    }
    for (name, doc) in [
        ("contributing", contributing),
        ("architecture", architecture),
        ("audit", audit),
    ] {
        let doc = doc
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        assert!(
            doc.contains("information locality"),
            "{name} lacks the durable locality requirement"
        );
        assert!(
            doc.contains("command help"),
            "{name} omits operation-local guidance"
        );
        assert!(
            doc.contains("execution"),
            "{name} omits outcome-specific execution guidance"
        );
    }
}

#[test]
fn information_routing_preserves_unapproved_repository_policies() {
    let guide = include_str!("../docs/guide.md");
    let commands = include_str!("../docs/commands.md");
    let configuration = include_str!("../docs/configuration.md");
    let architecture = include_str!("../docs/architecture.md");
    let readme = include_str!("../README.md");
    // Moving guidance is not authorization to remove these policies.
    for preserved in [
        "small-s3-boundary",
        "semantic-placement-review",
        "task-record-unchanged",
        "bulk-publication",
        "200 new files",
        "256 MiB (268435456 bytes)",
        ".workspace-mgr/repository.gitignore",
        ".git/info/exclude",
        "ignored_paths",
    ] {
        assert!(
            guide.contains(preserved),
            "guide loses retained policy {preserved:?}"
        );
        assert!(
            commands.contains(preserved),
            "commands lose retained policy {preserved:?}"
        );
    }
    assert!(architecture.contains("whitespace errors"));
    assert!(commands.contains("# workspace-mgr local begin"));
    assert!(commands.contains("force-with-lease"));
    for (name, doc) in [
        ("readme", readme),
        ("guide", guide),
        ("commands", commands),
        ("configuration", configuration),
        ("architecture", architecture),
    ] {
        assert!(
            doc.contains(".workspace-mgr/local/"),
            "{name} loses private control state"
        );
        assert!(
            doc.contains("--manifest"),
            "{name} loses explicit task selection"
        );
    }
    for requirement in [
        "repository_requirement",
        "Workspace-Requirement: minimum_cli_version=",
        "Cloud-Usage-Approval: limit_bytes=<n>; note=<note>",
        "`unchanged`",
        "`raise`",
        "`follow`",
        "`withdraw`",
    ] {
        assert!(
            commands.contains(requirement),
            "commands omit protocol {requirement:?}"
        );
    }
    for (name, doc) in [("readme", readme), ("guide", guide)] {
        let doc = doc.split_whitespace().collect::<Vec<_>>().join(" ");
        for fact in [
            "From 0.4.0 on",
            "Releases up to 0.3.0",
            "unknown-field error",
        ] {
            assert!(
                doc.contains(fact),
                "{name} loses old-client behavior {fact:?}"
            );
        }
    }
    assert!(configuration.contains("[cloud_usage_approval]"));
    assert!(configuration.contains("deliberately not configurable"));
    let configuration = configuration
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(configuration.contains("never below the fetched base branch's declaration"));
    assert!(configuration.contains("even only comments or formatting"));
    for warning in [
        "unaddressable-storage-metadata",
        "branch-cleanup-unavailable",
        "branch-cleanup-failed",
    ] {
        assert!(guide.contains(warning));
        assert!(commands.contains(warning));
    }
    assert!(commands.contains("storage.unaddressable"));
    for field in [
        "branch_cleanup",
        "`planned`",
        "`deleted`",
        "`skipped`",
        "`errors`",
        "`warnings`",
        "`unavailable`",
        "`not_applicable`",
        "`branches_cleaned`",
    ] {
        assert!(
            commands.contains(field),
            "cleanup control field {field:?} disappeared"
        );
    }
}

#[test]
fn approved_runtime_and_historical_content_proof_removals_are_documented() {
    let commands = include_str!("../docs/commands.md");
    let guide = include_str!("../docs/guide.md");
    let configuration = include_str!("../docs/configuration.md");
    let architecture = include_str!("../docs/architecture.md");
    let archive = commands
        .split("## `workspace-mgr archive`")
        .nth(1)
        .unwrap()
        .split("\n## ")
        .next()
        .unwrap();
    let archive = archive.split_whitespace().collect::<Vec<_>>().join(" ");
    for fact in [
        "An OPEN PR means pending",
        "MERGED, CLOSED without merging, or a successful query finding no corresponding PR means done",
        "hosting-query failures are reported as errors",
        "The PR need not target today's configured base branch",
        "Archive does not inspect historical configuration, directory-tree history",
        "It does not scan runtime paths or cross-task dependencies",
        "Ordinary tracked, staged, untracked, ignored, and local-only contents move with the directory",
        "Local `.git/info/exclude` or a global ignore file is insufficient",
        "new attempts do not rewrite nested Git controls",
        "`--pull-request` is optional",
        "Without it, adoption writes current task metadata without querying old PRs or creating a review record",
    ] {
        assert!(archive.contains(fact), "archive reference omits {fact:?}");
    }
    for (name, doc) in [("guide", guide), ("commands", commands)] {
        assert!(
            !doc.contains("--historical-record"),
            "{name} retains removed option"
        );
        assert!(
            doc.contains("manual-content-audit-after-relocation"),
            "{name} loses success-only relocation notice"
        );
    }
    assert!(configuration.contains("no longer synthesizes historical content proof"));
    assert!(!architecture.contains("A separate bounded streaming scan checks ordinary text"));
    assert!(
        !architecture.contains(
            "Location-bound Python environments and stale Git registrations fail preflight"
        )
    );
    assert!(architecture.contains("Old attempt journals retain their saved"));
}

#[test]
fn e2e_coverage_remains_explicit_about_control_boundaries() {
    let readme = include_str!("e2e/README.md");
    let coverage = include_str!("e2e/COVERAGE.md");
    assert!(readme.contains("COVERAGE.md"));
    for boundary in [
        "Transaction concurrency",
        "Version-aware S3",
        "Publish failure ordering",
        "Shared-checkout refresh",
        "Merged branch cleanup",
        "Refresh ancestry",
        "Pull-request ownership",
        "Task slug rename",
        "Cloud usage approval",
    ] {
        assert!(coverage.contains(boundary), "E2E coverage omits {boundary}");
    }
}
