mod common;

use common::{GitFixture, json, workspace, workspace_unchecked};

#[test]
fn move_reports_manual_content_review_only_after_success_and_preserves_bytes() {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["init"]);
    fixture.commit_seed("Initialize workspace");
    fixture.clone_shared();
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "move-notice",
            "--title",
            "Move notice",
            "--purpose",
            "Exercise operation-local guidance",
            "--timestamp",
            "20261007-120000",
        ],
    );
    let task = fixture.shared.join("20261007-120000-move-notice");
    let source = "20261007-120000-move-notice/old.txt";
    let destination = "20261007-120000-move-notice/new.txt";
    let bytes = b"old.txt\r\n/private/old/location\n";
    std::fs::write(task.join("old.txt"), bytes).unwrap();
    let preview = json(&workspace(
        &task,
        ["move", source, destination, "--dry-run"],
    ));
    assert!(preview.get("notices").is_none());
    assert!(task.join("old.txt").exists());
    assert!(!task.join("new.txt").exists());
    let moved = json(&workspace(&task, ["move", source, destination]));
    assert_eq!(
        moved["notices"][0]["code"],
        "manual-content-audit-after-relocation"
    );
    let message = moved["notices"][0]["message"].as_str().unwrap();
    assert!(message.contains("Move succeeded"));
    assert!(message.contains("manually audit and repair"));
    assert_eq!(std::fs::read(task.join("new.txt")).unwrap(), bytes);
    assert!(!task.join("old.txt").exists());
    let failed = workspace_unchecked(&task, ["move", destination, destination]);
    assert!(!failed.status.success());
    assert!(!String::from_utf8_lossy(&failed.stdout).contains("Move succeeded"));
    let status = json(&workspace(&task, ["storage", "status", destination]));
    assert!(status.get("notices").is_none());
}
