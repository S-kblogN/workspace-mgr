#[test]
fn user_documentation_covers_the_complete_public_model() {
    let readme = include_str!("../README.md");
    let model = include_str!("../docs/management-model.md");
    let normalized_model = model.split_whitespace().collect::<Vec<_>>().join(" ");
    let guide = include_str!("../docs/guide.md");
    let commands = include_str!("../docs/commands.md");
    let configuration = include_str!("../docs/configuration.md");
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
        "storage status",
        "storage set",
        "storage reset",
        "storage hydrate",
        "move",
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
    assert!(guide.contains("Nested placement boundaries"));
    assert!(guide.contains("small-s3-boundary"));
    assert!(guide.contains("semantic-placement-review"));
    assert!(guide.contains("retained-not-purged"));
    assert!(commands.contains("force-with-lease"));
    assert!(normalized_model.contains("explicit opposite endpoint"));
    assert!(normalized_model.contains("current slug is a mutable topic label"));
    assert!(commands.contains("head branch can close"));
    assert!(commands.contains("payload_bytes"));
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
    assert!(e2e_readme.contains("COVERAGE.md"));
    for boundary in [
        "Transaction concurrency",
        "Version-aware S3",
        "Publish failure ordering",
        "Shared-checkout refresh",
        "Refresh ancestry",
        "Pull-request ownership",
        "Task slug rename",
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
