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
    assert!(
        !fixture
            .shared
            .join(".git/workspace-mgr/repository.lock")
            .exists()
    );

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
    let model = text.find("# How this workspace works").unwrap();
    let rules = text.find("# Effective repository instructions").unwrap();
    assert!(
        model < rules,
        "management model must precede operational rules"
    );
    assert!(text.contains("one writable conversation (chat) = one task"));
    assert!(text.contains("Effective repository instructions"));
    assert!(text.contains("shared checkout"));
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
    assert_eq!(
        json(&report)["storage_runtime"],
        workspace_mgr::dvc::REQUIRED_DVC_VERSION
    );
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
        std::fs::read_to_string(runtime.join("bin/generated-launcher")).unwrap(),
        format!("#!{}/bin/python\n", runtime.display())
    );

    let repeated = std::process::Command::new(binary())
        .args(["setup", "--runtime-dir", runtime.to_str().unwrap()])
        .current_dir(&fixture.root)
        .env("WORKSPACE_MGR_FORMAT", "json")
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
        1,
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
        .output()
        .unwrap();
    assert_eq!(blocked.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("setup operation is running"));
    assert!(!runtime.exists());
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

#[test]
fn agent_modules_control_the_generated_policy() {
    let fixture = GitFixture::new();
    fixture.clone_shared();
    workspace(&fixture.shared, ["init"]);

    let path = fixture.shared.join(".workspace-mgr.toml");
    let raw = std::fs::read_to_string(&path).unwrap();
    let mut config: toml::Value = toml::from_str(&raw).unwrap();
    config["agent"]["modules"] = toml::Value::Array(vec![toml::Value::String("scope".to_owned())]);
    std::fs::write(&path, toml::to_string_pretty(&config).unwrap()).unwrap();

    let all = workspace(&fixture.shared, ["--format", "human", "instructions"]);
    let text = String::from_utf8(all.stdout).unwrap();
    assert!(text.contains("Operating model"));
    assert!(text.contains("Task lifecycle"));
    assert!(!text.contains("\n## Publication\n"));
    assert!(!text.contains("\n## Artifact hygiene\n"));
    assert!(!text.contains("\n## Storage placement\n"));
    assert!(!text.contains("\n## Shared checkout\n"));

    let disabled = workspace_unchecked(&fixture.shared, ["instructions", "publish"]);
    assert_eq!(disabled.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&disabled.stderr).contains("disabled"));
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
fn tracked_configuration_rejects_credential_urls_and_incompatible_cli_versions() {
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
    let config = std::fs::read_to_string(&config_path)
        .unwrap()
        .replace(">=0.1.0-alpha.1,<0.2.0", ">=9.0.0");
    std::fs::write(&config_path, config).unwrap();
    let instructions = workspace_unchecked(&fixture.shared, ["instructions"]);
    assert_eq!(instructions.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&instructions.stderr).contains("does not satisfy"));
    let doctor = workspace_unchecked(&fixture.shared, ["doctor"]);
    assert_eq!(doctor.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&doctor.stdout).contains("cli-version"));
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
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("not owned"));
    assert_eq!(
        std::fs::read_to_string(&dvc_config).unwrap(),
        "[core]\n    remote = preexisting\n"
    );

    std::fs::remove_dir_all(&dvc_dir).unwrap();
    workspace(
        &fixture.shared,
        ["init", "--s3-url", remote.to_str().unwrap()],
    );
    assert!(
        std::fs::read_to_string(fixture.shared.join(".gitattributes"))
            .unwrap()
            .contains("*.dvc whitespace=-blank-at-eol")
    );
    let config_path = fixture.shared.join(".workspace-mgr.toml");
    let mut config: toml::Value =
        toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    config["storage"].as_table_mut().unwrap().remove("s3");
    std::fs::write(&config_path, toml::to_string_pretty(&config).unwrap()).unwrap();
    let disabled = workspace(&fixture.shared, ["init"]);
    assert_eq!(json(&disabled)["status"], "initialized");
    assert!(!dvc_config.exists());
}
