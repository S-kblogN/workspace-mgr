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
    workspace(
        &fixture.seed,
        [
            "init",
            "--profile",
            "shared-checkout",
            "--storage-url",
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
        json(&published)["storage"]["dirty_files"][0],
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
    workspace(
        &fixture.seed,
        [
            "init",
            "--profile",
            "shared-checkout",
            "--storage-url",
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
fn object_version_adapter_and_engine_config_are_internal() {
    use std::os::unix::fs::PermissionsExt;

    if which::which("dvc").is_err() {
        eprintln!("skipping: dvc is unavailable");
        return;
    }
    let fixture = GitFixture::new();
    let dvc_remote = fixture.root.join("dvc-remote");
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
            "--storage-url",
            dvc_remote.to_str().unwrap(),
            "--require-object-versioning",
        ])
        .current_dir(&fixture.seed)
        .env("WORKSPACE_MGR_FORMAT", "json")
        .env("WORKSPACE_MGR_STORAGE_PYTHON", &fake_python)
        .output()
        .unwrap();
    assert!(output.status.success());
    let public = std::fs::read_to_string(fixture.seed.join(".workspace-mgr.toml")).unwrap();
    assert!(public.contains("require_object_versioning = true"));
    assert!(!public.contains("[dvc]"));
    assert!(!public.contains("require_version_aware"));
    assert!(!public.contains("python"));
    let internal = std::fs::read_to_string(fixture.seed.join(".dvc/config")).unwrap();
    assert!(internal.contains("remote = workspace-mgr"));
    assert!(internal.contains("version_aware = true"));
}
