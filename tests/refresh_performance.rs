mod common;

use common::*;

fn managed_fixture() -> GitFixture {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["manage"]);
    fixture.commit_seed("Add workspace policy");
    fixture
}

#[test]
fn refresh_batches_large_literal_path_sets_and_keeps_overlays() {
    let fixture = managed_fixture();
    let mut paths = (0..1_200)
        .map(|index| format!("{}-{index:04}.txt", "long-name-".repeat(10)))
        .collect::<Vec<_>>();
    paths.extend(
        [
            "line\nbreak",
            "tab\tfile",
            "literal[?]*",
            "quote\"file",
            "back\\slash",
        ]
        .map(str::to_owned),
    );
    for path in &paths {
        std::fs::write(fixture.seed.join(path), "before\n").unwrap();
    }
    fixture.commit_seed("Add many literal paths");
    fixture.clone_shared();
    for path in &paths {
        std::fs::write(fixture.seed.join(path), "after\n").unwrap();
    }
    fixture.commit_seed("Change many literal paths");
    std::fs::write(fixture.shared.join(&paths[0]), "local overlay\n").unwrap();
    let trace = fixture.root.join("refresh-trace.jsonl");
    let refreshed = workspace_env(
        &fixture.shared,
        ["refresh"],
        &[("GIT_TRACE2_EVENT", trace.to_str().unwrap())],
    );
    assert_eq!(json(&refreshed)["status"], "updated");
    assert_eq!(
        std::fs::read_to_string(fixture.shared.join(&paths[0])).unwrap(),
        "local overlay\n"
    );
    for path in &paths[1..] {
        assert_eq!(
            std::fs::read_to_string(fixture.shared.join(path)).unwrap(),
            "after\n",
            "{path:?}"
        );
    }
    let commands = std::fs::read_to_string(trace)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event["event"] == "start")
        .map(|event| {
            event["argv"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|arg| arg.as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let count = |name: &str| {
        commands
            .iter()
            .filter(|argv| argv.iter().any(|arg| arg == name))
            .count()
    };
    assert!(
        (2..=3).contains(&count("hash-object")),
        "hashes must use bounded batches: {commands:?}"
    );
    assert_eq!(
        count("checkout-index"),
        1,
        "materialization must use NUL stdin"
    );
    assert!(
        count("ls-tree") < 20,
        "tree lookups must not spawn once per file"
    );
    assert!(
        git(&fixture.shared, ["diff", "--cached", "--name-only"])
            .stdout
            .is_empty()
    );
}

#[cfg(unix)]
#[test]
fn refresh_preserves_filtered_bytes_executable_and_symlink_overlays() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let fixture = managed_fixture();
    git(
        &fixture.seed,
        ["config", "filter.upper.clean", "tr A-Z a-z"],
    );
    git(
        &fixture.seed,
        ["config", "filter.upper.smudge", "tr a-z A-Z"],
    );
    std::fs::write(fixture.seed.join(".gitattributes"), "*.flt filter=upper\n").unwrap();
    for path in [
        "clean.flt",
        "overlay.flt",
        "mode.txt",
        "link.txt",
        "plain.txt",
        "executable.sh",
    ] {
        std::fs::write(fixture.seed.join(path), "BEFORE\n").unwrap();
    }
    std::fs::set_permissions(
        fixture.seed.join("executable.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::fs::create_dir(fixture.seed.join("directory")).unwrap();
    std::fs::write(fixture.seed.join("directory/file.txt"), "BEFORE\n").unwrap();
    fixture.commit_seed("Add filter and overlay fixtures");
    fixture.clone_shared();
    git(
        &fixture.shared,
        ["config", "filter.upper.clean", "tr A-Z a-z"],
    );
    git(
        &fixture.shared,
        ["config", "filter.upper.smudge", "tr a-z A-Z"],
    );
    git(&fixture.shared, ["checkout-index", "--force", "--all"]);
    std::fs::write(fixture.shared.join("overlay.flt"), "LOCAL OVERLAY\n").unwrap();
    std::fs::set_permissions(
        fixture.shared.join("mode.txt"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::fs::remove_file(fixture.shared.join("link.txt")).unwrap();
    symlink("plain.txt", fixture.shared.join("link.txt")).unwrap();
    let outside = fixture.root.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("file.txt"), "BEFORE\n").unwrap();
    std::fs::remove_dir_all(fixture.shared.join("directory")).unwrap();
    symlink(&outside, fixture.shared.join("directory")).unwrap();
    for path in [
        "clean.flt",
        "overlay.flt",
        "mode.txt",
        "link.txt",
        "plain.txt",
        "executable.sh",
        "directory/file.txt",
    ] {
        std::fs::write(fixture.seed.join(path), "AFTER\n").unwrap();
    }
    std::fs::set_permissions(
        fixture.seed.join("executable.sh"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    fixture.commit_seed("Update filtered and overlaid files");
    let refreshed = workspace(&fixture.shared, ["refresh"]);
    assert_eq!(json(&refreshed)["status"], "updated");
    assert_eq!(
        std::fs::read_to_string(fixture.shared.join("clean.flt")).unwrap(),
        "AFTER\n"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.shared.join("overlay.flt")).unwrap(),
        "LOCAL OVERLAY\n"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.shared.join("mode.txt")).unwrap(),
        "BEFORE\n"
    );
    assert_ne!(
        std::fs::metadata(fixture.shared.join("mode.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o100,
        0
    );
    assert_eq!(
        std::fs::read_link(fixture.shared.join("link.txt")).unwrap(),
        std::path::Path::new("plain.txt")
    );
    assert_eq!(
        std::fs::read_to_string(outside.join("file.txt")).unwrap(),
        "BEFORE\n"
    );
    assert_eq!(
        std::fs::metadata(fixture.shared.join("executable.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o100,
        0
    );
    assert_eq!(
        std::fs::read_to_string(fixture.shared.join("executable.sh")).unwrap(),
        "AFTER\n"
    );
    assert!(
        git(&fixture.shared, ["diff", "--cached", "--name-only"])
            .stdout
            .is_empty()
    );
}

#[cfg(unix)]
#[test]
fn refresh_obeys_the_repositories_disabled_filemode_check() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = managed_fixture();
    std::fs::write(fixture.seed.join("file.txt"), "before\n").unwrap();
    fixture.commit_seed("Add ordinary file");
    fixture.clone_shared();
    git(&fixture.shared, ["config", "core.filemode", "false"]);
    std::fs::set_permissions(
        fixture.shared.join("file.txt"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::fs::write(fixture.seed.join("file.txt"), "after\n").unwrap();
    fixture.commit_seed("Update ordinary file");
    workspace(&fixture.shared, ["refresh"]);
    assert_eq!(
        std::fs::read_to_string(fixture.shared.join("file.txt")).unwrap(),
        "after\n"
    );
}
