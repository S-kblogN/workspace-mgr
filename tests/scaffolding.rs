mod common;

use common::*;

#[test]
fn init_instructions_doctor_and_task_create_form_one_workflow() {
    let fixture = GitFixture::new();
    fixture.clone_shared();

    let dry = workspace(
        &fixture.shared,
        [
            "init",
            "--dry-run",
            "--repo",
            fixture.shared.to_str().unwrap(),
        ],
    );
    assert_eq!(json(&dry)["status"], "dry_run");
    assert!(!fixture.shared.join(".workspace-mgr.toml").exists());
    assert!(
        !fixture
            .shared
            .join(".git/workspace-mgr/repository.lock")
            .exists()
    );

    let initialized = workspace(
        &fixture.shared,
        ["init", "--repo", fixture.shared.to_str().unwrap()],
    );
    assert_eq!(json(&initialized)["status"], "initialized");
    assert!(fixture.shared.join(".workspace-mgr.toml").is_file());
    assert!(fixture.shared.join("AGENTS.md").is_file());

    let repeated = workspace(
        &fixture.shared,
        ["init", "--repo", fixture.shared.to_str().unwrap()],
    );
    assert_eq!(json(&repeated)["status"], "no_changes");

    let instructions = workspace(&fixture.shared, ["--format", "human", "instructions"]);
    let text = String::from_utf8(instructions.stdout).unwrap();
    let model = text.find("# How this workspace works").unwrap();
    let rules = text.find("# Effective repository instructions").unwrap();
    assert!(
        model < rules,
        "management model must precede operational rules"
    );
    assert!(text.contains("one writable conversation (chat) = one task"));
    assert!(text.contains("Effective repository instructions"));
    assert!(text.contains("shared checkout"));
    assert!(text.contains("Repository-wide reading is allowed"));
    assert!(text.contains("default write boundary is its own task directory"));
    assert!(text.contains("another chat's task directory"));
    assert!(text.contains("explicitly authorize the exact path and action"));
    assert!(text.contains("they do not create authorization"));
    assert!(text.contains("write boundary is instead the exact user-authorized paths"));
    assert!(text.contains("policy="));

    let model_only = workspace(
        &fixture.shared,
        ["--format", "human", "instructions", "model"],
    );
    let model_only = String::from_utf8(model_only.stdout).unwrap();
    assert!(model_only.contains("# How this workspace works"));
    assert!(!model_only.contains("# Effective repository instructions"));

    let doctor = workspace(&fixture.shared, ["--format", "json", "doctor"]);
    assert_eq!(json(&doctor)["status"], "ok");

    let created = workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "sample-task",
            "--title",
            "Sample task",
            "--purpose",
            "Exercise deterministic scaffolding.",
            "--timestamp",
            "20260829-170000",
        ],
    );
    let payload = json(&created);
    assert_eq!(payload["status"], "created");
    assert_eq!(payload["task_id"], "20260829-170000-sample-task");
    let task = fixture.shared.join("20260829-170000-sample-task");
    assert!(task.join("README.md").is_file());
    assert!(task.join(".workspace-mgr-task.toml").is_file());
    let branch = git(
        &fixture.shared,
        ["rev-parse", "--verify", "codex/sample-task"],
    );
    assert!(!branch.stdout.is_empty());
}

#[test]
fn infrastructure_task_uses_private_state_and_an_isolated_worktree() {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["init"]);
    fixture.commit_seed("Add workspace policy");
    fixture.clone_shared();

    let created = workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "shared-policy",
            "--kind",
            "infrastructure",
            "--title",
            "Shared policy",
            "--purpose",
            "Update one repository-wide policy file.",
            "--scope",
            "shared-policy.md",
            "--scope-note",
            "The user requested this repository-wide policy change.",
        ],
    );
    let created = json(&created);
    assert_eq!(created["kind"], "infrastructure");
    assert_eq!(created["task_id"], "infra-shared-policy");
    assert_eq!(created["branch"], "codex/infra-shared-policy");
    let worktree = std::path::PathBuf::from(created["path"].as_str().unwrap());
    let manifest = std::path::PathBuf::from(created["manifest"].as_str().unwrap());
    assert!(worktree.is_dir());
    assert!(manifest.is_file());
    assert!(!fixture.shared.join("infra-shared-policy").exists());
    assert_eq!(
        String::from_utf8_lossy(&git(&worktree, ["branch", "--show-current"]).stdout).trim(),
        "codex/infra-shared-policy"
    );

    let status = workspace(&worktree, ["task", "status"]);
    assert_eq!(json(&status)["kind"], "infrastructure");
    assert_eq!(
        json(&status)["scopes"],
        serde_json::json!(["shared-policy.md"])
    );
    let explicit = workspace(
        &fixture.shared,
        ["task", "status", "--manifest", manifest.to_str().unwrap()],
    );
    assert_eq!(json(&explicit)["branch"], "codex/infra-shared-policy");
    let doctor = workspace(&worktree, ["doctor"]);
    assert_eq!(json(&doctor)["status"], "ok");
    std::fs::write(worktree.join("shared-policy.md"), "shared policy\n").unwrap();
    let published = workspace(&worktree, ["publish", "-m", "Publish shared policy"]);
    let published = json(&published);
    assert_eq!(published["status"], "pushed");
    assert_eq!(published["head"], "codex/infra-shared-policy");
    assert_eq!(published["review"]["pull_request"], "required");
    assert_eq!(published["review"]["managed_by"], "agent");
    assert_eq!(published["review"]["merge_authority"], "user");
    assert!(git(&worktree, ["status", "--short"]).stdout.is_empty());
    let commit = published["commit_oid"].as_str().unwrap();
    assert!(
        git_unchecked(
            &fixture.shared,
            ["cat-file", "-e", &format!("{commit}:shared-policy.md")],
        )
        .status
        .success()
    );

    std::fs::remove_file(worktree.join("shared-policy.md")).unwrap();
    let removed = workspace(&worktree, ["publish", "-m", "Remove shared policy"]);
    let removed = json(&removed);
    assert_eq!(removed["status"], "pushed");
    assert_eq!(
        removed["changed_paths"],
        serde_json::json!(["shared-policy.md"])
    );
    let removed_commit = removed["commit_oid"].as_str().unwrap();
    assert!(
        !git_unchecked(
            &fixture.shared,
            [
                "cat-file",
                "-e",
                &format!("{removed_commit}:shared-policy.md")
            ],
        )
        .status
        .success()
    );
    let clean = workspace(&worktree, ["plan"]);
    assert_eq!(json(&clean)["status"], "no_changes");
}

#[test]
fn setup_dry_run_reports_private_runtime_without_installing_it() {
    let fixture = GitFixture::new();
    let runtime = fixture.root.join("private-runtime");
    let report = workspace(
        &fixture.root,
        [
            "setup",
            "--runtime-dir",
            runtime.to_str().unwrap(),
            "--dry-run",
        ],
    );
    assert_eq!(json(&report)["status"], "dry_run");
    assert_eq!(json(&report)["storage_runtime"], "3.67.1");
    assert!(!runtime.exists());
}

#[cfg(unix)]
#[test]
fn setup_installs_and_reuses_a_verified_private_runtime() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = GitFixture::new();
    let runtime = fixture.root.join("private-runtime");
    let bootstrap = fixture.root.join("bootstrap-python");
    std::fs::write(
        &bootstrap,
        "#!/bin/sh\nset -eu\nif [ \"${1:-}\" = \"-m\" ] && [ \"${2:-}\" = \"venv\" ]; then\n  mkdir -p \"$3/bin\"\n  cp \"$0\" \"$3/bin/python\"\n  cp \"$0\" \"$3/bin/dvc\"\n  printf '#!%s/bin/python\\n' \"$3\" > \"$3/bin/generated-launcher\"\n  exit 0\nfi\nif [ \"${1:-}\" = \"--version\" ]; then\n  printf '%s\\n' '3.67.1'\n  exit 0\nfi\nif [ \"${1:-}\" = \"-m\" ] && [ \"${2:-}\" = \"pip\" ]; then\n  exit 0\nfi\nif [ \"${1:-}\" = \"-c\" ]; then\n  printf '%s\\n' '3.67.1'\n  exit 0\nfi\nexit 23\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&bootstrap).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&bootstrap, permissions).unwrap();

    let install = std::process::Command::new(binary())
        .args(["setup", "--runtime-dir", runtime.to_str().unwrap()])
        .current_dir(&fixture.root)
        .env("WORKSPACE_MGR_FORMAT", "json")
        .env("WORKSPACE_MGR_UPDATE_CHECK_DISABLE", "1")
        .env("WORKSPACE_MGR_BOOTSTRAP_PYTHON", &bootstrap)
        .output()
        .unwrap();
    assert!(
        install.status.success(),
        "setup failed: stdout={} stderr={}",
        String::from_utf8_lossy(&install.stdout),
        String::from_utf8_lossy(&install.stderr)
    );
    assert_eq!(json(&install)["status"], "installed");
    assert!(runtime.join("bin/dvc").is_file());
    assert!(runtime.join("bin/python").is_file());
    assert_eq!(
        std::fs::read_to_string(runtime.join(".workspace-mgr-runtime")).unwrap(),
        "workspace-mgr private runtime v1\n"
    );
    assert_eq!(
        std::fs::read_to_string(runtime.join("bin/generated-launcher")).unwrap(),
        format!("#!{}/bin/python\n", runtime.display())
    );

    let repeated = std::process::Command::new(binary())
        .args(["setup", "--runtime-dir", runtime.to_str().unwrap()])
        .current_dir(&fixture.root)
        .env("WORKSPACE_MGR_FORMAT", "json")
        .env("WORKSPACE_MGR_UPDATE_CHECK_DISABLE", "1")
        .env("WORKSPACE_MGR_BOOTSTRAP_PYTHON", &bootstrap)
        .output()
        .unwrap();
    assert!(
        repeated.status.success(),
        "repeat setup failed: stdout={} stderr={}",
        String::from_utf8_lossy(&repeated.stdout),
        String::from_utf8_lossy(&repeated.stderr)
    );
    assert_eq!(json(&repeated)["status"], "no_changes");
}

#[cfg(unix)]
#[test]
fn failed_runtime_install_restores_the_previous_directory() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = GitFixture::new();
    let runtime = fixture.root.join("private-runtime");
    std::fs::create_dir(&runtime).unwrap();
    std::fs::write(runtime.join("sentinel"), "previous runtime\n").unwrap();
    std::fs::write(
        runtime.join(".workspace-mgr-runtime"),
        "workspace-mgr private runtime v1\n",
    )
    .unwrap();
    let bootstrap = fixture.root.join("failing-bootstrap-python");
    std::fs::write(
        &bootstrap,
        "#!/bin/sh\nset -eu\nif [ \"${1:-}\" = \"-m\" ] && [ \"${2:-}\" = \"venv\" ]; then\n  mkdir -p \"$3/bin\"\n  cp \"$0\" \"$3/bin/python\"\n  exit 0\nfi\nif [ \"${1:-}\" = \"-m\" ] && [ \"${2:-}\" = \"pip\" ]; then\n  exit 23\nfi\nexit 23\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&bootstrap).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&bootstrap, permissions).unwrap();

    let failed = std::process::Command::new(binary())
        .args(["setup", "--runtime-dir", runtime.to_str().unwrap()])
        .current_dir(&fixture.root)
        .env("WORKSPACE_MGR_UPDATE_CHECK_DISABLE", "1")
        .env("WORKSPACE_MGR_BOOTSTRAP_PYTHON", &bootstrap)
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(2));
    assert_eq!(
        std::fs::read_to_string(runtime.join("sentinel")).unwrap(),
        "previous runtime\n"
    );
    assert_eq!(
        std::fs::read_dir(runtime).unwrap().count(),
        2,
        "partial replacement files must be removed"
    );
    assert!(std::fs::read_dir(&fixture.root).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".workspace-mgr-runtime-backup-")
    }));
}

#[test]
fn setup_refuses_to_replace_an_unmanaged_directory() {
    let fixture = GitFixture::new();
    let runtime = fixture.root.join("ordinary-data");
    std::fs::create_dir(&runtime).unwrap();
    std::fs::write(runtime.join("sentinel"), "must survive\n").unwrap();

    let rejected = workspace_unchecked(
        &fixture.root,
        [
            "setup",
            "--runtime-dir",
            runtime.to_str().unwrap(),
            "--dry-run",
        ],
    );
    assert_eq!(rejected.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("not owned"));
    assert_eq!(
        std::fs::read_to_string(runtime.join("sentinel")).unwrap(),
        "must survive\n"
    );
}

#[test]
fn concurrent_runtime_install_is_rejected_before_provisioning() {
    use fs2::FileExt;

    let fixture = GitFixture::new();
    let runtime = fixture.root.join("private-runtime");
    let lock_path = fixture.root.join(".workspace-mgr-setup.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .unwrap();
    lock.try_lock_exclusive().unwrap();

    let blocked = std::process::Command::new(binary())
        .args(["setup", "--runtime-dir", runtime.to_str().unwrap()])
        .current_dir(&fixture.root)
        .env("WORKSPACE_MGR_UPDATE_CHECK_DISABLE", "1")
        .output()
        .unwrap();
    assert_eq!(blocked.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("setup operation is running"));
    assert!(!runtime.exists());
}

#[test]
fn first_init_treats_reserved_paths_as_collisions_without_inspecting_content() {
    let fixture = GitFixture::new();
    fixture.clone_shared();
    workspace(&fixture.shared, ["init"]);
    let agents_path = fixture.shared.join("AGENTS.md");
    let canonical = std::fs::read_to_string(&agents_path).unwrap();
    assert!(canonical.contains("install the latest stable release"));
    assert!(canonical.contains("    cargo install --locked workspace-mgr\n"));
    assert!(!canonical.contains("cargo install --locked workspace-mgr --version"));
    assert!(canonical.contains("workspace-mgr setup"));
    assert!(canonical.contains("retry `workspace-mgr instructions --repo .`"));
    std::fs::remove_file(fixture.shared.join(".workspace-mgr.toml")).unwrap();

    let rejected = workspace_unchecked(&fixture.shared, ["init"]);
    assert_eq!(rejected.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(stderr.contains("reserved workspace-mgr scaffold paths"));
    assert!(stderr.contains("AGENTS.md"));
    assert_eq!(std::fs::read_to_string(&agents_path).unwrap(), canonical);
    assert!(!fixture.shared.join(".workspace-mgr.toml").exists());
}

#[test]
fn init_reconciles_the_owned_agents_bootstrap_regardless_of_content() {
    let fixture = GitFixture::new();
    fixture.clone_shared();
    workspace(&fixture.shared, ["init"]);
    let agents_path = fixture.shared.join("AGENTS.md");
    let canonical = std::fs::read_to_string(&agents_path).unwrap();

    std::fs::write(&agents_path, "# Legacy or locally edited bootstrap\n").unwrap();
    let unhealthy = workspace_unchecked(&fixture.shared, ["doctor"]);
    assert_eq!(unhealthy.status.code(), Some(2));
    let report = json(&unhealthy);
    assert!(report["checks"].as_array().unwrap().iter().any(|check| {
        check["name"] == "repository-scaffold"
            && check["status"] == "error"
            && check["detail"].as_str().unwrap().contains("AGENTS.md")
    }));
    let updated = workspace(&fixture.shared, ["init"]);
    assert_eq!(json(&updated)["status"], "initialized");
    assert_eq!(json(&updated)["actions"][0]["action"], "update");
    assert_eq!(json(&updated)["actions"][0]["path"], "AGENTS.md");
    assert_eq!(std::fs::read_to_string(&agents_path).unwrap(), canonical);

    std::fs::remove_file(&agents_path).unwrap();
    let recreated = workspace(&fixture.shared, ["init"]);
    assert_eq!(json(&recreated)["status"], "initialized");
    assert_eq!(json(&recreated)["actions"][0]["action"], "create");
    assert_eq!(std::fs::read_to_string(&agents_path).unwrap(), canonical);
}

#[test]
fn repository_configuration_cannot_change_the_workspace_policy() {
    let fixture = GitFixture::new();
    fixture.clone_shared();
    workspace(&fixture.shared, ["init"]);

    let path = fixture.shared.join(".workspace-mgr.toml");
    let raw = std::fs::read_to_string(&path).unwrap();
    for forbidden in [
        "schema_version",
        "required_cli",
        "profile",
        "[publication]",
        "[tasks]",
        "[review]",
        "[storage]",
        "[agent]",
        "branch_prefix",
        "auto_s3_above_bytes",
    ] {
        assert!(
            !raw.contains(forbidden),
            "unexpected policy key {forbidden}"
        );
    }

    let all = workspace(&fixture.shared, ["--format", "human", "instructions"]);
    let text = String::from_utf8(all.stdout).unwrap();
    assert!(text.contains("Operating model"));
    assert!(text.contains("Task lifecycle"));
    assert!(text.contains("\n## Publication\n"));
    assert!(text.contains("\n## Pull request responsibility\n"));
    assert!(text.contains("\n## Artifact hygiene\n"));
    assert!(text.contains("\n## Storage placement\n"));
    assert!(text.contains("\n## Shared checkout\n"));
    assert!(text.contains("\n## Repository infrastructure\n"));

    for topic in [
        "task",
        "publish",
        "artifacts",
        "storage",
        "shared-checkout",
        "infrastructure",
    ] {
        let rendered = workspace(&fixture.shared, ["instructions", topic]);
        assert!(
            !rendered.stdout.is_empty(),
            "topic {topic} was not rendered"
        );
    }

    let task_rules = workspace(&fixture.shared, ["instructions", "task"]);
    let task_rules = String::from_utf8(task_rules.stdout).unwrap();
    assert!(task_rules.contains("default write boundary is its own task directory"));
    assert!(task_rules.contains("explicitly authorize the exact path and action"));
    assert!(task_rules.contains("they do not create authorization"));

    let infrastructure_rules = workspace(&fixture.shared, ["instructions", "infrastructure"]);
    let infrastructure_rules = String::from_utf8(infrastructure_rules.stdout).unwrap();
    assert!(infrastructure_rules.contains("write only the declared paths"));
    assert!(infrastructure_rules.contains("separate explicit user approval"));

    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "fixed-branch",
            "--title",
            "Fixed branch",
            "--purpose",
            "Verify the product-owned branch strategy.",
            "--timestamp",
            "20260829-170010",
        ],
    );
    let manifest = fixture
        .shared
        .join("20260829-170010-fixed-branch/.workspace-mgr-task.toml");
    let original = std::fs::read_to_string(&manifest).unwrap();
    let task = fixture.shared.join("20260829-170010-fixed-branch");
    let cases = [
        (
            original.replace("codex/fixed-branch", "codex/other-branch"),
            "task branch must be",
        ),
        (original.replace("kind = \"deliverable\"\n", ""), "kind"),
        (
            original.replace("slug = \"fixed-branch\"\n", ""),
            "lowercase kebab",
        ),
        (original.replace("title = \"Fixed branch\"\n", ""), "title"),
        (
            original.replace(
                "purpose = \"Verify the product-owned branch strategy.\"\n",
                "",
            ),
            "purpose",
        ),
        (
            original.replace(
                "additional_scopes = []\n",
                "[[additional_scopes]]\npath = \"20260829-170010-fixed-branch/nested\"\nreason = \"This is already inside the task.\"\n",
            ),
            "must not overlap",
        ),
    ];
    for (altered, expected) in cases {
        std::fs::write(&manifest, altered).unwrap();
        let rejected = workspace_unchecked(&task, ["task", "status"]);
        assert_eq!(rejected.status.code(), Some(2));
        assert!(
            String::from_utf8_lossy(&rejected.stderr).contains(expected),
            "missing {expected:?} in {}",
            String::from_utf8_lossy(&rejected.stderr)
        );
    }
    std::fs::write(&manifest, original).unwrap();
}

#[cfg(unix)]
#[test]
fn failed_storage_setup_does_not_install_an_unusable_agents_bootstrap() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = GitFixture::new();
    fixture.clone_shared();
    let fake_bin = fixture.root.join("fake-bin");
    std::fs::create_dir(&fake_bin).unwrap();
    let fake_dvc = fake_bin.join("dvc");
    std::fs::write(&fake_dvc, "#!/bin/sh\nexit 23\n").unwrap();
    let mut permissions = std::fs::metadata(&fake_dvc).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&fake_dvc, permissions).unwrap();
    let output = std::process::Command::new(binary())
        .args(["init", "--s3-url", "s3://example.invalid/workspace"])
        .current_dir(&fixture.shared)
        .env("WORKSPACE_MGR_UPDATE_CHECK_DISABLE", "1")
        .env("WORKSPACE_MGR_STORAGE_DVC", &fake_dvc)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(!fixture.shared.join("AGENTS.md").exists());
    assert!(!fixture.shared.join(".workspace-mgr.toml").exists());
}

#[cfg(unix)]
#[test]
fn partially_failing_storage_initialization_rolls_back_all_scaffolding() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = GitFixture::new();
    fixture.clone_shared();
    let fake_dvc = fixture.root.join("partial-dvc");
    std::fs::write(
        &fake_dvc,
        "#!/bin/sh\nset -eu\nif [ \"${1:-}\" = \"--version\" ]; then\n  printf '%s\\n' '3.67.1'\n  exit 0\nfi\nif [ \"${1:-}\" = \"init\" ]; then\n  mkdir -p .dvc\n  printf '%s\\n' 'partial' > .dvc/config\n  printf '%s\\n' 'partial' > .dvcignore\n  exit 23\nfi\nexit 23\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&fake_dvc).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&fake_dvc, permissions).unwrap();
    let storage = fixture.root.join("storage");
    let output = std::process::Command::new(binary())
        .args(["init", "--s3-url", storage.to_str().unwrap()])
        .current_dir(&fixture.shared)
        .env("WORKSPACE_MGR_UPDATE_CHECK_DISABLE", "1")
        .env("WORKSPACE_MGR_STORAGE_DVC", &fake_dvc)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("rolled back"));
    for path in [
        ".workspace-mgr.toml",
        "AGENTS.md",
        ".dvc",
        ".dvcignore",
        ".gitattributes",
    ] {
        assert!(!fixture.shared.join(path).exists(), "{path} remained");
    }
}

#[test]
fn tracked_configuration_rejects_credentials_and_policy_keys() {
    let fixture = GitFixture::new();
    fixture.clone_shared();
    let credentials = workspace_unchecked(
        &fixture.shared,
        [
            "init",
            "--s3-url",
            "s3://bucket/prefix?X-Amz-Signature=tracked-secret",
            "--dry-run",
        ],
    );
    assert_eq!(credentials.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&credentials.stderr).contains("query or fragment"));
    assert!(!fixture.shared.join(".workspace-mgr.toml").exists());

    workspace(&fixture.shared, ["init"]);
    let config_path = fixture.shared.join(".workspace-mgr.toml");
    let original = std::fs::read_to_string(&config_path).unwrap();
    for field in ["remote", "branch"] {
        let prefix = format!("{field} = ");
        let incomplete = original
            .lines()
            .filter(|line| !line.starts_with(&prefix))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(&config_path, incomplete).unwrap();
        let rejected = workspace_unchecked(&fixture.shared, ["instructions"]);
        assert_eq!(rejected.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&rejected.stderr).contains(field));
    }
    let mut config = original;
    config.push_str("\n[review]\nmanaged_by = \"agent\"\n");
    std::fs::write(&config_path, config).unwrap();
    let instructions = workspace_unchecked(&fixture.shared, ["instructions"]);
    assert_eq!(instructions.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&instructions.stderr).contains("unknown field"));
}

#[test]
fn init_owns_internal_storage_config_and_can_disable_an_unused_remote() {
    if which::which("dvc").is_err() {
        eprintln!("skipping: dvc is unavailable");
        return;
    }
    let fixture = GitFixture::new();
    fixture.clone_shared();
    let dvc_dir = fixture.shared.join(".dvc");
    std::fs::create_dir(&dvc_dir).unwrap();
    let dvc_config = dvc_dir.join("config");
    std::fs::write(&dvc_config, "[core]\n    remote = preexisting\n").unwrap();
    let remote = fixture.root.join("storage");
    let rejected = workspace_unchecked(
        &fixture.shared,
        ["init", "--s3-url", remote.to_str().unwrap()],
    );
    assert_eq!(rejected.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("reserved workspace-mgr scaffold paths")
    );
    assert_eq!(
        std::fs::read_to_string(&dvc_config).unwrap(),
        "[core]\n    remote = preexisting\n"
    );

    std::fs::remove_dir_all(&dvc_dir).unwrap();
    workspace(
        &fixture.shared,
        ["init", "--s3-url", remote.to_str().unwrap()],
    );
    let managed_config = std::fs::read_to_string(&dvc_config).unwrap();
    let storage_gitignore = fixture.shared.join(".dvc/.gitignore");
    let managed_storage_gitignore = std::fs::read_to_string(&storage_gitignore).unwrap();
    let storage_ignore = fixture.shared.join(".dvcignore");
    let managed_storage_ignore = std::fs::read_to_string(&storage_ignore).unwrap();
    let config_path = fixture.shared.join(".workspace-mgr.toml");
    let public_config = std::fs::read_to_string(&config_path).unwrap();
    git(&fixture.shared, ["add", "-A"]);
    git(
        &fixture.shared,
        ["commit", "-m", "Initialize managed repository"],
    );
    let pointer = fixture.shared.join("retained.bin.dvc");
    std::fs::write(&pointer, "outs:\n- path: retained.bin\n").unwrap();
    std::fs::write(&dvc_config, "# old or damaged generated configuration\n").unwrap();
    std::fs::write(&storage_gitignore, "/locally-edited\n").unwrap();
    std::fs::write(&storage_ignore, "locally-edited/**\n").unwrap();

    let relocated = public_config.replace(
        remote.to_str().unwrap(),
        fixture.root.join("other-storage").to_str().unwrap(),
    );
    std::fs::write(&config_path, relocated).unwrap();
    let rejected_relocation = workspace_unchecked(&fixture.shared, ["init"]);
    assert_eq!(rejected_relocation.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&rejected_relocation.stderr)
            .contains("cannot change the managed S3 location")
    );

    std::fs::write(&config_path, &public_config).unwrap();
    let repaired = workspace(&fixture.shared, ["init"]);
    assert_eq!(json(&repaired)["status"], "initialized");
    assert_eq!(
        std::fs::read_to_string(&dvc_config).unwrap(),
        managed_config
    );
    assert_eq!(
        std::fs::read_to_string(&storage_gitignore).unwrap(),
        managed_storage_gitignore
    );
    assert_eq!(
        std::fs::read_to_string(&storage_ignore).unwrap(),
        managed_storage_ignore
    );
    std::fs::remove_file(&pointer).unwrap();
    assert!(
        std::fs::read_to_string(fixture.shared.join(".gitattributes"))
            .unwrap()
            .contains("*.dvc whitespace=-blank-at-eol")
    );
    let mut config: toml::Value =
        toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    config.as_table_mut().unwrap().remove("s3");
    std::fs::write(&config_path, toml::to_string_pretty(&config).unwrap()).unwrap();
    let disabled = workspace(&fixture.shared, ["init"]);
    assert_eq!(json(&disabled)["status"], "initialized");
    assert!(!dvc_config.exists());
    assert_eq!(
        std::fs::read_to_string(&storage_gitignore).unwrap(),
        managed_storage_gitignore
    );
    assert_eq!(
        std::fs::read_to_string(&storage_ignore).unwrap(),
        managed_storage_ignore
    );
}
