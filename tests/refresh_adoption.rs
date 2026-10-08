#![cfg(all(unix, feature = "test-storage"))]

mod common;

use std::os::unix::fs::PermissionsExt;

use common::*;

#[test]
fn refresh_adopts_first_s3_configuration_and_restores_controls_after_hydration_failure() {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["manage"]);
    fixture.commit_seed("Manage a Git-only repository");
    fixture.clone_shared();

    let original_config = std::fs::read(fixture.shared.join(".workspace-mgr.toml")).unwrap();
    assert!(!String::from_utf8_lossy(&original_config).contains("[s3]"));
    let original_head = git(&fixture.shared, ["rev-parse", "HEAD"]).stdout;
    let original_index = git(&fixture.shared, ["ls-files", "--stage", "-z"]).stdout;
    let storage_remote = fixture.root.join("native-storage");

    let created = json(&workspace(
        &fixture.seed,
        [
            "task",
            "create",
            "enable-storage",
            "--kind",
            "infrastructure",
            "--title",
            "Enable native storage",
            "--purpose",
            "Introduce native storage to an existing Git-only repository",
            "--scope",
            ".workspace-mgr.toml",
            "--scope",
            "assets",
            "--scope-note",
            "The user requested storage configuration and its first retained asset",
        ],
    ));
    let manifest = created["manifest"].as_str().unwrap();
    workspace(
        &fixture.seed,
        ["manage", "--s3-url", storage_remote.to_str().unwrap()],
    );
    let payload = b"the exact first asset from native storage\0\r\n";
    std::fs::create_dir(fixture.seed.join("assets")).unwrap();
    std::fs::write(fixture.seed.join("assets/model.bin"), payload).unwrap();
    workspace(
        &fixture.seed,
        [
            "storage",
            "set",
            "assets/model.bin",
            "--manifest",
            manifest,
            "--to",
            "s3",
            "--reason",
            "Keep this opaque asset in native storage",
        ],
    );
    let published = json(&workspace(
        &fixture.seed,
        [
            "publish",
            "--manifest",
            manifest,
            "-m",
            "Enable native storage",
        ],
    ));
    let incoming = published["commit_oid"].as_str().unwrap();
    git(&fixture.remote, ["update-ref", "refs/heads/main", incoming]);

    let preview = json(&workspace(&fixture.shared, ["refresh", "--dry-run"]));
    assert_eq!(preview["status"], "dry_run");
    assert_eq!(
        git(&fixture.shared, ["rev-parse", "HEAD"]).stdout,
        original_head
    );
    assert_eq!(
        std::fs::read(fixture.shared.join(".workspace-mgr.toml")).unwrap(),
        original_config
    );
    assert!(!fixture.shared.join("assets/model.bin").exists());

    // Fail after refresh advances the revision, during payload hydration. The
    // previous public config and shared index must survive for a normal retry.
    let hook = fixture.root.join("fail-hydration");
    std::fs::write(
        &hook,
        "#!/bin/sh\nif [ \"$1\" = checkout ]; then\n  echo 'forced adoption hydration failure' >&2\n  exit 9\nfi\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let refused = workspace_env_unchecked(
        &fixture.shared,
        ["refresh"],
        &[("WORKSPACE_MGR_TEST_STORAGE_HOOK", hook.to_str().unwrap())],
    );
    assert_eq!(refused.status.code(), Some(2));
    let refusal = String::from_utf8_lossy(&refused.stderr);
    assert!(
        refusal.contains("forced adoption hydration failure"),
        "{refusal}"
    );
    assert!(refusal.contains("rolled back"), "{refusal}");
    assert_eq!(
        git(&fixture.shared, ["rev-parse", "HEAD"]).stdout,
        original_head
    );
    assert_eq!(
        git(&fixture.shared, ["ls-files", "--stage", "-z"]).stdout,
        original_index
    );
    assert_eq!(
        std::fs::read(fixture.shared.join(".workspace-mgr.toml")).unwrap(),
        original_config
    );
    assert!(
        !fixture
            .shared
            .join("assets/model.bin.wm-storage.json")
            .exists()
    );
    assert!(!fixture.shared.join("assets/model.bin").exists());

    let refreshed = json(&workspace(&fixture.shared, ["refresh"]));
    assert_eq!(refreshed["status"], "updated");
    assert_eq!(
        git(&fixture.shared, ["rev-parse", "HEAD"]).stdout,
        format!("{incoming}\n").as_bytes()
    );
    assert_eq!(
        std::fs::read(fixture.shared.join("assets/model.bin")).unwrap(),
        payload
    );
    assert_eq!(
        std::fs::read(fixture.shared.join("assets/model.bin.wm-storage.json")).unwrap(),
        std::fs::read(fixture.seed.join("assets/model.bin.wm-storage.json")).unwrap()
    );
    let adopted: toml::Value = toml::from_str(
        &std::fs::read_to_string(fixture.shared.join(".workspace-mgr.toml")).unwrap(),
    )
    .unwrap();
    assert_eq!(adopted["minimum_cli_version"].as_str(), Some("0.8.1"));
    assert_eq!(adopted["s3"]["url"].as_str(), storage_remote.to_str());
    assert!(
        git(&fixture.shared, ["diff", "--cached", "--name-only"])
            .stdout
            .is_empty()
    );
    assert!(
        git(&fixture.shared, ["status", "--porcelain"])
            .stdout
            .is_empty()
    );
}
