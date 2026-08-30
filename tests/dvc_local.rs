mod common;

use common::*;

#[test]
fn automatic_policy_plans_without_mutation_and_publishes_to_s3() {
    if which::which("dvc").is_err() {
        eprintln!("skipping: dvc is unavailable");
        return;
    }
    let fixture = GitFixture::new();
    let storage_remote = fixture.root.join("storage-remote");
    workspace(
        &fixture.seed,
        [
            "init",
            "--profile",
            "shared-checkout",
            "--s3-url",
            storage_remote.to_str().unwrap(),
        ],
    );
    let config_path = fixture.seed.join(".workspace-mgr.toml");
    let config = std::fs::read_to_string(&config_path).unwrap().replace(
        "auto_s3_above_bytes = 10485760",
        "auto_s3_above_bytes = 1024",
    );
    std::fs::write(config_path, config).unwrap();
    fixture.commit_seed("Initialize automatic storage policy");
    fixture.clone_shared();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "automatic-placement",
            "--title",
            "Automatic placement",
            "--purpose",
            "Test automatic S3 routing.",
            "--timestamp",
            "20260829-170450",
        ],
    );
    let task_id = "20260829-170450-automatic-placement";
    let task = fixture.shared.join(task_id);
    std::fs::write(task.join("large.bin"), vec![7_u8; 2048]).unwrap();

    let plan = workspace(&task, ["plan"]);
    assert_eq!(json(&plan)["status"], "dry_run");
    assert_eq!(
        json(&plan)["storage"]["placement"]["would_place_in_s3"][0],
        format!("{task_id}/large.bin")
    );
    assert!(!task.join("large.bin.dvc").exists());
    assert!(!storage_remote.exists());

    let published = workspace(&task, ["publish", "-m", "Publish automatic placement"]);
    assert_eq!(json(&published)["status"], "pushed");
    assert!(task.join("large.bin.dvc").is_file());
    assert!(storage_remote.exists());
}

#[test]
fn placement_publish_and_hydrate_use_an_isolated_local_remote() {
    if which::which("dvc").is_err() {
        eprintln!("skipping: dvc is unavailable");
        return;
    }
    let fixture = GitFixture::new();
    let dvc_remote = fixture.root.join("dvc-remote");
    workspace(
        &fixture.seed,
        [
            "init",
            "--profile",
            "shared-checkout",
            "--s3-url",
            dvc_remote.to_str().unwrap(),
        ],
    );
    let config = std::fs::read_to_string(fixture.seed.join(".workspace-mgr.toml")).unwrap();
    assert!(config.contains("[storage]"));
    assert!(config.contains(&format!("url = {:?}", dvc_remote.to_str().unwrap())));
    let internal = std::fs::read_to_string(fixture.seed.join(".dvc/config")).unwrap();
    assert!(internal.contains("remote = workspace-mgr"));
    fixture.commit_seed("Initialize managed DVC repository");
    fixture.clone_shared();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "dvc-flow",
            "--title",
            "DVC flow",
            "--purpose",
            "Test local DVC publication.",
            "--timestamp",
            "20260829-170500",
        ],
    );
    let task_name = "20260829-170500-dvc-flow";
    let task = fixture.shared.join(task_name);
    let data = task.join("data.bin");
    std::fs::write(&data, b"version one\n").unwrap();
    let bundle = task.join("bundle");
    std::fs::create_dir(&bundle).unwrap();
    std::fs::write(bundle.join("alpha.txt"), b"alpha\n").unwrap();

    let placed = workspace(
        &task,
        [
            "storage",
            "set",
            &format!("{task_name}/data.bin"),
            &format!("{task_name}/bundle"),
            "--to",
            "s3",
            "--reason",
            "Exercise explicit S3 placement.",
        ],
    );
    assert_eq!(json(&placed)["status"], "updated");
    assert_eq!(json(&placed)["remote_writes"], false);
    let tracked = workspace(&task, ["publish", "-m", "Publish S3 data"]);
    assert_eq!(json(&tracked)["status"], "pushed");
    assert!(task.join("data.bin.dvc").is_file());

    let inherited = workspace(
        &task,
        [
            "storage",
            "status",
            &format!("{task_name}/bundle/alpha.txt"),
        ],
    );
    assert_eq!(json(&inherited)["placements"][0]["target"], "s3");
    assert_eq!(
        json(&inherited)["placements"][0]["selected_by"],
        "explicit-ancestor"
    );
    let overlap = workspace_unchecked(
        &task,
        [
            "storage",
            "set",
            &format!("{task_name}/bundle/alpha.txt"),
            "--to",
            "git",
            "--reason",
            "This nested choice must be rejected.",
        ],
    );
    assert_eq!(overlap.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&overlap.stderr).contains("existing placement boundary"));
    let nested_reset = workspace_unchecked(
        &task,
        ["storage", "reset", &format!("{task_name}/bundle/alpha.txt")],
    );
    assert_eq!(nested_reset.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&nested_reset.stderr).contains("existing placement boundary"));

    std::fs::write(&data, b"version two\n").unwrap();
    let published = workspace(&task, ["publish", "-m", "Update DVC data"]);
    assert_eq!(json(&published)["status"], "pushed");
    assert_eq!(
        json(&published)["storage"]["s3"]["dirty_files"][0],
        format!("{task_name}/data.bin.dvc")
    );

    std::fs::remove_file(&data).unwrap();
    let hydrated = workspace(&task, ["storage", "hydrate"]);
    assert_eq!(json(&hydrated)["status"], "hydrated");
    assert_eq!(std::fs::read(&data).unwrap(), b"version two\n");

    let moved = task.join("moved.bin");
    let move_report = workspace(
        &task,
        [
            "move",
            &format!("{task_name}/data.bin"),
            &format!("{task_name}/moved.bin"),
        ],
    );
    assert_eq!(json(&move_report)["status"], "updated");
    assert_eq!(json(&move_report)["remote_writes"], false);
    let moved_publish = workspace(&task, ["publish", "-m", "Publish moved S3 data"]);
    assert_eq!(json(&moved_publish)["status"], "pushed");
    assert!(moved.is_file());
    assert!(task.join("moved.bin.dvc").is_file());

    let git_placement = workspace(
        &task,
        [
            "storage",
            "set",
            &format!("{task_name}/moved.bin"),
            "--to",
            "git",
            "--reason",
            "Exercise explicit Git placement.",
        ],
    );
    assert_eq!(json(&git_placement)["status"], "updated");
    let git_publish = workspace(&task, ["publish", "-m", "Publish Git placement"]);
    assert_eq!(json(&git_publish)["status"], "pushed");
    assert!(moved.is_file());
    assert!(!task.join("moved.bin.dvc").exists());
}

#[test]
fn publish_refuses_a_missing_dirty_dvc_output() {
    if which::which("dvc").is_err() {
        eprintln!("skipping: dvc is unavailable");
        return;
    }
    let fixture = GitFixture::new();
    let dvc_remote = fixture.root.join("dvc-remote");
    workspace(
        &fixture.seed,
        [
            "init",
            "--profile",
            "shared-checkout",
            "--s3-url",
            dvc_remote.to_str().unwrap(),
        ],
    );
    fixture.commit_seed("Initialize managed DVC repository");
    fixture.clone_shared();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "missing-dvc",
            "--title",
            "Missing DVC",
            "--purpose",
            "Exercise missing-output refusal.",
            "--timestamp",
            "20260829-170800",
        ],
    );
    let task_name = "20260829-170800-missing-dvc";
    let task = fixture.shared.join(task_name);
    let data = task.join("data.bin");
    std::fs::write(&data, b"content\n").unwrap();
    workspace(
        &task,
        [
            "storage",
            "set",
            &format!("{task_name}/data.bin"),
            "--to",
            "s3",
            "--reason",
            "Exercise missing output guard.",
        ],
    );
    workspace(&task, ["publish", "-m", "Publish S3 data"]);
    std::fs::remove_file(&data).unwrap();
    let rejected = workspace_unchecked(&task, ["publish", "-m", "Delete output"]);
    assert_eq!(rejected.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("missing locally"));
}

#[cfg(unix)]
#[test]
fn object_version_adapter_and_engine_config_are_internal() {
    use std::os::unix::fs::PermissionsExt;

    if which::which("dvc").is_err() {
        eprintln!("skipping: dvc is unavailable");
        return;
    }
    let fixture = GitFixture::new();
    let fake_python = fixture.root.join("fake-python");
    std::fs::write(
        &fake_python,
        "#!/bin/sh\ncase \"$2\" in\n  *'print(dvc.__version__)'*) printf '%s\\n' '3.67.1' ;;\n  *) printf '%s\\n' '{\"mode\":\"version-aware\",\"remote\":\"fake\",\"checked_objects\":[]}' ;;\nesac\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&fake_python).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&fake_python, permissions).unwrap();
    let output = std::process::Command::new(binary())
        .args([
            "init",
            "--profile",
            "shared-checkout",
            "--s3-url",
            "s3://example.invalid/workspace",
            "--s3-endpoint-url",
            "https://s3.example.invalid",
        ])
        .current_dir(&fixture.seed)
        .env("WORKSPACE_MGR_FORMAT", "json")
        .env("WORKSPACE_MGR_STORAGE_PYTHON", &fake_python)
        .output()
        .unwrap();
    assert!(output.status.success());
    let public = std::fs::read_to_string(fixture.seed.join(".workspace-mgr.toml")).unwrap();
    assert!(public.contains("[storage.s3]"));
    assert!(public.contains("url = \"s3://example.invalid/workspace\""));
    assert!(!public.contains("[dvc]"));
    assert!(!public.contains("require_version_aware"));
    assert!(!public.contains("python"));
    let internal = std::fs::read_to_string(fixture.seed.join(".dvc/config")).unwrap();
    assert!(internal.contains("remote = workspace-mgr"));
    assert!(internal.contains("version_aware = true"));
}
