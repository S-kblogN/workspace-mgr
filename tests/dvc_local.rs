mod common;

use common::*;

#[test]
fn track_publish_and_hydrate_use_an_isolated_local_remote() {
    if which::which("dvc").is_err() {
        eprintln!("skipping: dvc is unavailable");
        return;
    }
    let fixture = GitFixture::new();
    let dvc_remote = fixture.root.join("dvc-remote");
    command(&fixture.seed, "dvc", ["init"]);
    command(
        &fixture.seed,
        "dvc",
        [
            "remote",
            "add",
            "-d",
            "storage",
            dvc_remote.to_str().unwrap(),
        ],
    );
    workspace(
        &fixture.seed,
        ["init", "--profile", "shared-checkout", "--dvc", "--adopt"],
    );
    let config = std::fs::read_to_string(fixture.seed.join(".workspace-mgr.toml")).unwrap();
    assert!(config.contains("remote = \"storage\""));
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

    let tracked = workspace(
        &task,
        [
            "track",
            "-m",
            "Track DVC data",
            &format!("{task_name}/data.bin"),
        ],
    );
    assert_eq!(json(&tracked)["status"], "pushed");
    assert!(task.join("data.bin.dvc").is_file());

    std::fs::write(&data, b"version two\n").unwrap();
    let published = workspace(&task, ["publish", "-m", "Update DVC data"]);
    assert_eq!(json(&published)["status"], "pushed");
    assert_eq!(
        json(&published)["dvc"]["dirty_files"][0],
        format!("{task_name}/data.bin.dvc")
    );

    std::fs::remove_file(&data).unwrap();
    let hydrated = workspace(&task, ["hydrate"]);
    assert_eq!(json(&hydrated)["status"], "hydrated");
    assert_eq!(std::fs::read(&data).unwrap(), b"version two\n");

    let moved = task.join("moved.bin");
    let move_report = workspace(
        &task,
        [
            "move",
            "-m",
            "Move DVC boundary",
            &format!("{task_name}/data.bin"),
            &format!("{task_name}/moved.bin"),
        ],
    );
    assert_eq!(json(&move_report)["status"], "pushed");
    assert!(moved.is_file());
    assert!(task.join("moved.bin.dvc").is_file());

    let untrack_report = workspace(
        &task,
        [
            "untrack",
            "-m",
            "Untrack DVC boundary",
            &format!("{task_name}/moved.bin.dvc"),
        ],
    );
    assert_eq!(json(&untrack_report)["status"], "pushed");
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
    command(&fixture.seed, "dvc", ["init"]);
    command(
        &fixture.seed,
        "dvc",
        [
            "remote",
            "add",
            "-d",
            "storage",
            dvc_remote.to_str().unwrap(),
        ],
    );
    workspace(
        &fixture.seed,
        ["init", "--profile", "shared-checkout", "--dvc", "--adopt"],
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
            "track",
            "-m",
            "Track data",
            &format!("{task_name}/data.bin"),
        ],
    );
    std::fs::remove_file(&data).unwrap();
    let rejected = workspace_unchecked(&task, ["publish", "-m", "Delete output"]);
    assert_eq!(rejected.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("missing locally"));
}

#[cfg(unix)]
#[test]
fn version_aware_verifier_is_an_explicit_adapter() {
    use std::os::unix::fs::PermissionsExt;

    if which::which("dvc").is_err() {
        eprintln!("skipping: dvc is unavailable");
        return;
    }
    let fixture = GitFixture::new();
    let dvc_remote = fixture.root.join("dvc-remote");
    command(&fixture.seed, "dvc", ["init"]);
    command(
        &fixture.seed,
        "dvc",
        [
            "remote",
            "add",
            "-d",
            "storage",
            dvc_remote.to_str().unwrap(),
        ],
    );
    workspace(
        &fixture.seed,
        ["init", "--profile", "shared-checkout", "--dvc", "--adopt"],
    );
    let fake_python = fixture.root.join("fake-python");
    std::fs::write(
        &fake_python,
        "#!/bin/sh\nprintf '%s\\n' '{\"mode\":\"version-aware\",\"remote\":\"fake\",\"checked_objects\":[]}'\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&fake_python).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&fake_python, permissions).unwrap();
    let config_path = fixture.seed.join(".workspace-mgr.toml");
    let config = std::fs::read_to_string(&config_path).unwrap();
    let config = config
        .replace(
            "require_version_aware = false",
            "require_version_aware = true",
        )
        .replace(
            "python = \"python3\"",
            &format!("python = {:?}", fake_python.to_str().unwrap()),
        );
    std::fs::write(&config_path, config).unwrap();
    fixture.commit_seed("Configure exact DVC verifier adapter");
    fixture.clone_shared();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "version-aware",
            "--title",
            "Version aware",
            "--purpose",
            "Exercise the exact-verification adapter.",
            "--timestamp",
            "20260829-170600",
        ],
    );
    let task_name = "20260829-170600-version-aware";
    let task = fixture.shared.join(task_name);
    std::fs::write(task.join("data.bin"), b"content\n").unwrap();
    let output = workspace(
        &task,
        [
            "track",
            "-m",
            "Track version-aware fixture",
            &format!("{task_name}/data.bin"),
        ],
    );
    assert_eq!(
        json(&output)["dvc"]["verification"]["mode"],
        "version-aware"
    );
}
