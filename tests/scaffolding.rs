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
            "--profile",
            "shared-checkout",
            "--dry-run",
            "--repo",
            fixture.shared.to_str().unwrap(),
        ],
    );
    assert_eq!(json(&dry)["status"], "dry_run");
    assert!(!fixture.shared.join(".workspace-mgr.toml").exists());

    let initialized = workspace(
        &fixture.shared,
        [
            "init",
            "--profile",
            "shared-checkout",
            "--repo",
            fixture.shared.to_str().unwrap(),
        ],
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
    assert!(text.contains("Effective repository instructions"));
    assert!(text.contains("shared checkout"));
    assert!(text.contains("policy="));

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
fn adopt_preserves_existing_agents_as_a_module() {
    let fixture = GitFixture::new();
    fixture.clone_shared();
    std::fs::write(
        fixture.shared.join("AGENTS.md"),
        "# Existing policy\n\n- Keep this rule.\n",
    )
    .unwrap();

    let rejected = workspace_unchecked(&fixture.shared, ["init"]);
    assert_eq!(rejected.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("--adopt"));

    workspace(&fixture.shared, ["init", "--adopt"]);
    let module = std::fs::read_to_string(
        fixture
            .shared
            .join(".workspace-mgr/instructions/repository.md"),
    )
    .unwrap();
    assert!(module.contains("Keep this rule"));
    let agents = std::fs::read_to_string(fixture.shared.join("AGENTS.md")).unwrap();
    assert!(agents.contains("workspace-mgr instructions"));
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
    let inherited_path = std::env::var_os("PATH").unwrap();
    let mut paths = vec![fake_bin];
    paths.extend(std::env::split_paths(&inherited_path));
    let path = std::env::join_paths(paths).unwrap();
    let output = std::process::Command::new(binary())
        .args(["init", "--s3-url", "s3://example.invalid/workspace"])
        .current_dir(&fixture.shared)
        .env("PATH", path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(!fixture.shared.join("AGENTS.md").exists());
    assert!(!fixture.shared.join(".workspace-mgr.toml").exists());
}
