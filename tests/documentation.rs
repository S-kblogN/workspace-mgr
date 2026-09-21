#[test]
fn user_documentation_covers_the_complete_public_model() {
    let readme = include_str!("../README.md");
    let model = include_str!("../docs/management-model.md");
    let normalized_model = model.split_whitespace().collect::<Vec<_>>().join(" ");
    let guide = include_str!("../docs/guide.md");
    let commands = include_str!("../docs/commands.md");
    let configuration = include_str!("../docs/configuration.md");
    let changelog = include_str!("../CHANGELOG.md");
    let e2e_readme = include_str!("e2e/README.md");
    let e2e_coverage = include_str!("e2e/COVERAGE.md");

    assert!(readme.contains("docs/management-model.md"));
    assert!(guide.contains("management-model.md"));
    assert!(readme.contains("docs/guide.md"));
    assert!(readme.contains("docs/commands.md"));
    for concept in [
        "general-purpose collaborator",
        "user-facing interface",
        "durable workspace",
        "Task scope",
        "Storage placement",
        "remote visibility boundary",
        "multiple chats",
    ] {
        assert!(
            normalized_model.contains(concept),
            "model is missing {concept}"
        );
    }
    assert!(normalized_model.contains(
        "one writable conversation (chat) = one task = one target branch = one draft pull request"
    ));
    assert!(normalized_model.contains("Infrastructure is a kind of task"));
    for ownership_rule in [
        "Reading and ownership are separate",
        "Reading a path does not transfer ownership or authorize mutation",
        "task directory is the default write boundary",
        "another chat's task directory",
        "explicit user authorization for the exact path and action",
        "does not manufacture approval",
        "manifest scopes are its write boundary",
        "Untracked does not mean unowned",
    ] {
        assert!(
            normalized_model.contains(ownership_rule),
            "model is missing ownership rule {ownership_rule:?}"
        );
    }
    assert!(normalized_model.contains("same management strategy"));
    assert!(normalized_model.contains("the user asks for outcomes"));
    assert!(normalized_model.contains("None of these operations publishes a task"));
    for storage_concept in [
        "collaboration and control plane",
        "artifact and data plane",
        "below 1 MiB",
        "1 through 10 MiB",
        "Above 10 MiB",
        "aggregate size",
    ] {
        assert!(
            normalized_model.contains(storage_concept),
            "model is missing storage concept {storage_concept:?}"
        );
    }
    for command in [
        "setup",
        "init",
        "instructions",
        "doctor",
        "config show",
        "task create",
        "task rename",
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
            "command reference is missing {command}"
        );
    }
    assert!(guide.contains("itself create a remote branch or call a hosting provider"));
    assert!(guide.contains("does not call a GitHub or other hosting API"));
    for responsibility in [
        "immediately follows creation",
        "create exactly one",
        "never create a duplicate",
        "living description",
        "head revision",
        "Before ending every turn",
        "does not need to request",
        "must not merge",
        "enable auto-merge",
    ] {
        assert!(
            guide.contains(responsibility) || normalized_model.contains(responsibility),
            "documentation is missing pull-request responsibility {responsibility:?}"
        );
    }
    assert!(guide.contains("S3 first, then Git"));
    assert!(guide.contains("Before every writable-task turn ends"));
    assert!(guide.contains("then purge obsolete S3 paths"));
    assert!(guide.contains("Nested placement boundaries"));
    assert!(guide.contains("small-s3-boundary"));
    assert!(guide.contains("semantic-placement-review"));
    assert!(guide.contains("task-record-unchanged"));
    assert!(guide.contains("bulk-publication"));
    // The one warning refresh reports, and the report field beside it, are
    // named wherever an agent looks up a code it just received.
    for (name, document) in [("guide", guide), ("commands", commands)] {
        assert!(
            document.contains("unaddressable-storage-metadata"),
            "{name} does not name the warning refresh reports"
        );
    }
    assert!(commands.contains("storage.unaddressable"));
    assert!(guide.contains("ignored_paths"));
    assert!(guide.contains(".workspace-mgr/repository.gitignore"));
    assert!(guide.contains(".git/info/exclude"));
    assert!(guide.contains("permanently deletes every version"));
    assert!(commands.contains("force-with-lease"));
    assert!(normalized_model.contains("explicit opposite endpoint"));
    assert!(normalized_model.contains("current slug is a mutable topic label"));
    assert!(
        normalized_model
            .contains("recording an approval documents the user's decision and never creates it")
    );
    assert!(normalized_model.contains("records that answer in the task manifest"));
    assert!(normalized_model.contains("oldest `workspace-mgr` release"));
    for requirement_fact in [
        "`cli-version`",
        "repository_requirement",
        "Workspace-Requirement: minimum_cli_version=",
        "Cloud-Usage-Approval: limit_bytes=<n>; note=<note>",
    ] {
        assert!(
            commands.contains(requirement_fact),
            "command reference is missing {requirement_fact:?}"
        );
    }
    for fact in [
        "`unchanged`",
        "`raise`",
        "`follow`",
        "`withdraw`",
        "task create` and `task discard",
        "Workspace-Requirement: minimum_cli_version=<version>",
    ] {
        assert!(
            commands.contains(fact),
            "command reference is missing {fact:?}"
        );
    }
    let normalized_commands = commands.split_whitespace().collect::<Vec<_>>().join(" ");
    for fact in [
        "`task rename`, `plan`, and `publish`",
        "this command changed nothing",
        "the approval takes the same override",
        "never below the base branch's declaration",
    ] {
        assert!(
            normalized_commands.contains(fact),
            "command reference is missing {fact:?}"
        );
    }
    // Only releases from 0.4.0 on know the declaration; older ones reject it
    // as an unknown field, which the user-facing overviews must not hide.
    for (name, document) in [("README.md", readme), ("guide.md", guide)] {
        let normalized = document.split_whitespace().collect::<Vec<_>>().join(" ");
        for fact in [
            "From 0.4.0 on",
            "Releases up to 0.3.0",
            "unknown-field error",
        ] {
            assert!(
                normalized.contains(fact),
                "{name} does not qualify the older-release behavior: {fact:?}"
            );
        }
    }
    for removed in ["recorded_at", "adopts", "private task state until"] {
        for (name, document) in [
            ("README.md", readme),
            ("management-model.md", model),
            ("guide.md", guide),
            ("commands.md", commands),
            ("configuration.md", configuration),
        ] {
            assert!(
                !document.contains(removed),
                "{name} still describes the removed private approval record: {removed:?}"
            );
        }
    }
    assert!(commands.contains("head branch can close"));
    assert!(commands.contains("payload_bytes"));
    assert!(commands.contains("ignored_paths"));
    assert!(commands.contains("bulk-publication"));
    assert!(commands.contains(".workspace-mgr/repository.gitignore"));
    assert!(commands.contains("# workspace-mgr local begin"));
    // The thresholds and the product's fixed rules are literals in prose that
    // no other test can see, so a retune of the constants must break a test
    // that names the documents it invalidated.
    for threshold in ["200 new files", "256 MiB (268435456 bytes)"] {
        for (name, document) in [("guide", guide), ("commands", commands)] {
            assert!(
                document.contains(threshold),
                "{name} states a stale bulk-publication threshold, expected {threshold:?}"
            );
        }
    }
    assert!(normalized_model.contains("256 MiB (268435456 bytes)"));
    // Every rule the generated root ignore file carries is documented, read
    // from the source list itself so the two cannot drift apart.
    let scaffold = include_str!("../src/scaffold.rs");
    let groups = scaffold
        .split("pub(crate) const PRODUCT_IGNORE_GROUPS")
        .nth(1)
        .and_then(|rest| rest.split("\n];\n").next())
        .expect("product ignore groups");
    let product_rules = groups
        .lines()
        // Rules sit one level deeper than the group titles.
        .filter_map(|line| line.strip_prefix("            \""))
        .filter_map(|line| line.strip_suffix("\","))
        .map(|line| line.replace("\\\\", "\\"))
        .collect::<Vec<_>>();
    assert!(product_rules.len() > 50, "{product_rules:?}");
    for product_rule in product_rules {
        assert!(
            commands.contains(&format!("`{product_rule}`")),
            "the generated root ignore file's rules are documented in full, missing {product_rule}"
        );
    }
    // The upgrade every existing repository must perform is documented where a
    // maintainer looks for it.
    assert!(guide.contains("Upgrading a repository that predates"));
    assert!(changelog.contains("### Upgrading"));
    for workplace_rule in [
        "Where the work happens",
        "It is where the work happens",
        "rather than in a temporary directory elsewhere on the machine",
        "only that they are Markdown files the README's directory map names",
        "records the turn's decisions, process, tools, and hard-to-reproduce results",
        "curating what leaves it is the other",
        "ignored by a rule this repository tracks",
        "Git has no include directive",
        "S3 keeps Git small, it does not keep the workspace curated",
    ] {
        assert!(
            normalized_model.contains(workplace_rule),
            "model is missing workplace rule {workplace_rule:?}"
        );
    }
    for fact in ["[git]", "remote", "branch", "[s3]", "endpoint_url"] {
        assert!(
            configuration.contains(fact),
            "configuration reference is missing external fact {fact:?}"
        );
    }
    for policy_knob in [
        "[review]",
        "[publication]",
        "[tasks]",
        "[storage]",
        "[agent]",
        "required_cli",
        "branch_prefix",
        "auto_s3_above_bytes",
    ] {
        assert!(
            !configuration.contains(policy_knob),
            "configuration reference exposes policy knob {policy_knob:?}"
        );
    }
    assert!(configuration.contains("deliberately not configurable"));
    assert!(configuration.contains("minimum_cli_version"));
    assert!(configuration.contains("not a policy switch"));
    let normalized_configuration = configuration
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    for fact in [
        "never below the fetched base branch's declaration",
        "even only comments or formatting",
        "`task rename`, `plan`, and `publish` check",
    ] {
        assert!(
            normalized_configuration.contains(fact),
            "configuration reference is missing {fact:?}"
        );
    }
    assert!(configuration.contains("[cloud_usage_approval]"));
    assert!(e2e_readme.contains("COVERAGE.md"));
    for boundary in [
        "Transaction concurrency",
        "Version-aware S3",
        "Publish failure ordering",
        "Shared-checkout refresh",
        "Refresh ancestry",
        "Pull-request ownership",
        "Task slug rename",
        "Cloud usage approval",
    ] {
        assert!(
            e2e_coverage.contains(boundary),
            "E2E coverage contract is missing {boundary}"
        );
    }
    assert!(!model.to_ascii_lowercase().contains("dvc"));
    assert!(!guide.to_ascii_lowercase().contains("dvc"));
    assert!(!commands.to_ascii_lowercase().contains("dvc"));
}
