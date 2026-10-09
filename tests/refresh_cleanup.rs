mod common;

use common::*;

#[test]
fn refresh_reports_git_advance_and_durable_queue_when_cleanup_preflight_fails() {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["manage"]);
    fixture.commit_seed("Manage Git repository");
    fixture.clone_shared();

    // Retained cleanup can outlive removal of the repository's S3 settings.
    // Retirement must fail, while a completed Git refresh still has a report.
    let queued = serde_json::json!({
        "pointer": "retired/file.bin.wm-storage.json",
        "object": "retired/file.bin",
        "version_id": "exact-retired-version"
    });
    let state_path = fixture.shared.join(".workspace-mgr/local/s3-purge.json");
    std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
    std::fs::write(
        &state_path,
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 2,
            "pending": [queued],
            "pending_prefixes": {}
        }))
        .unwrap(),
    )
    .unwrap();

    std::fs::write(fixture.seed.join("README.md"), "incoming Git content\n").unwrap();
    fixture.commit_seed("Advance Git independently of pending storage cleanup");
    let incoming = String::from_utf8(git(&fixture.seed, ["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    for expected_status in ["updated", "no_changes"] {
        let report = json(&workspace(&fixture.shared, ["refresh"]));
        assert_eq!(report["status"], expected_status);
        assert_eq!(report["new_oid"], incoming);
        assert_eq!(report["storage"]["purge"]["status"], "cleanup_pending");
        assert_eq!(
            report["storage"]["purge"]["pending"],
            serde_json::json!([queued])
        );
        assert!(
            report["storage"]["purge"]["errors"][0]
                .as_str()
                .unwrap()
                .contains("managed storage is not enabled")
        );
        assert!(
            report["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning["code"] == "s3-cleanup-failed")
        );
        assert_eq!(
            std::fs::read_to_string(fixture.shared.join("README.md")).unwrap(),
            "incoming Git content\n"
        );
        assert_eq!(
            String::from_utf8(git(&fixture.shared, ["rev-parse", "HEAD"]).stdout)
                .unwrap()
                .trim(),
            incoming
        );
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(persisted["pending"], serde_json::json!([queued]));
        assert_eq!(persisted["schema_version"], 2);
        assert!(
            git(&fixture.shared, ["diff", "--cached", "--name-only"])
                .stdout
                .is_empty()
        );
    }
}
