use clap::Parser;
use workspace_mgr::cli::Cli;

#[test]
fn user_documentation_covers_the_complete_public_model() {
    let readme = include_str!("../README.md");
    let guide = include_str!("../docs/guide.md");
    let commands = include_str!("../docs/commands.md");

    assert!(readme.contains("docs/guide.md"));
    assert!(readme.contains("docs/commands.md"));
    for concept in [
        "Repository policy",
        "Agent instructions",
        "Task",
        "Placement",
        "Publication",
    ] {
        assert!(guide.contains(concept), "guide is missing {concept}");
    }
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
    assert!(guide.contains("S3 first, then Git"));
    assert!(guide.contains("Nested placement boundaries"));
    assert!(!guide.to_ascii_lowercase().contains("dvc"));
    assert!(!commands.to_ascii_lowercase().contains("dvc"));
}

#[test]
fn documented_command_shapes_are_accepted_by_clap() {
    let examples: &[&[&str]] = &[
        &["setup", "--dry-run"],
        &[
            "setup",
            "--runtime-dir",
            "/tmp/workspace-mgr-runtime",
            "--dry-run",
        ],
        &["init", "--profile", "standard"],
        &[
            "init",
            "--profile",
            "shared-checkout",
            "--s3-url",
            "s3://example-bucket/workspace",
            "--s3-endpoint-url",
            "https://s3.example.invalid",
            "--adopt",
            "--dry-run",
        ],
        &["instructions"],
        &["instructions", "storage", "--repo", "/tmp/repository"],
        &["--format", "json", "instructions", "publish"],
        &["doctor", "--repo", "/tmp/repository"],
        &["config", "show", "--repo", "/tmp/repository"],
        &[
            "task",
            "create",
            "training-report",
            "--title",
            "Training report",
            "--purpose",
            "Produce the final training report",
            "--branch",
            "review/training-report",
            "--dry-run",
        ],
        &["task", "status", "--manifest", "task/manifest.toml"],
        &["storage", "status"],
        &[
            "storage",
            "status",
            "task/results/model.bin",
            "--manifest",
            "task/manifest.toml",
        ],
        &[
            "storage",
            "set",
            "task/report.pdf",
            "task/summary.txt",
            "--to",
            "git",
            "--reason",
            "Review these files directly",
        ],
        &[
            "storage",
            "set",
            "task/data",
            "--to",
            "s3",
            "--reason",
            "Retain the dataset",
            "--dry-run",
        ],
        &["storage", "reset", "task/data"],
        &["storage", "hydrate", "task/data/example.csv"],
        &["move", "task/old.bin", "task/new.bin", "--dry-run"],
        &[
            "plan",
            "--include",
            "docs/shared.md",
            "--scope-note",
            "The user requested this shared documentation update",
        ],
        &[
            "plan",
            "--allow-non-shared-head",
            "--scope-note",
            "The user selected an alternate checkout",
        ],
        &["publish", "-m", "Publish the training report"],
        &[
            "publish",
            "--message",
            "Publish shared documentation",
            "--include",
            "docs/shared.md",
            "--scope-note",
            "The user requested this shared documentation update",
            "--dry-run",
        ],
        &[
            "refresh",
            "--remote",
            "origin",
            "--branch",
            "main",
            "--dry-run",
        ],
    ];

    for args in examples {
        let argv = std::iter::once("workspace-mgr").chain(args.iter().copied());
        Cli::try_parse_from(argv).unwrap_or_else(|error| {
            panic!("documented command failed to parse: {args:?}\n{error}")
        });
    }
}

#[test]
fn read_only_commands_do_not_expose_meaningless_mutation_flags() {
    assert!(Cli::try_parse_from(["workspace-mgr", "plan", "--dry-run"]).is_err());
    assert!(Cli::try_parse_from(["workspace-mgr", "plan", "-m", "unused"]).is_err());
    assert!(Cli::try_parse_from(["workspace-mgr", "storage", "status", "--dry-run"]).is_err());
}
