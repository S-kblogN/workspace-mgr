#[test]
fn user_documentation_covers_the_complete_public_model() {
    let readme = include_str!("../README.md");
    let model = include_str!("../docs/management-model.md");
    let normalized_model = model.split_whitespace().collect::<Vec<_>>().join(" ");
    let guide = include_str!("../docs/guide.md");
    let commands = include_str!("../docs/commands.md");
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
    assert!(normalized_model.contains("the user asks for outcomes"));
    assert!(normalized_model.contains("None of these operations publishes a task"));
    for command in [
        "setup",
        "init",
        "instructions",
        "doctor",
        "config show",
        "task create",
        "task status",
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
    assert!(guide.contains("remote branch or a pull request"));
    assert!(guide.contains("does not call a GitHub or other hosting API"));
    for responsibility in [
        "create exactly one",
        "never create a duplicate",
        "living description",
        "head revision",
        "must not merge",
        "enable auto-merge",
    ] {
        assert!(
            guide.contains(responsibility) || normalized_model.contains(responsibility),
            "documentation is missing pull-request responsibility {responsibility:?}"
        );
    }
    assert!(guide.contains("S3 first, then Git"));
    assert!(guide.contains("Nested placement boundaries"));
    assert!(e2e_readme.contains("COVERAGE.md"));
    for boundary in [
        "Transaction concurrency",
        "Version-aware S3",
        "Publish failure ordering",
        "Shared-checkout refresh",
        "Refresh ancestry",
        "Pull-request ownership",
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
