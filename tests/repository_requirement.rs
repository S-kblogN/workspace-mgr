mod common;

use std::path::{Path, PathBuf};
use std::process::Output;

use common::*;
use serde_json::{Value, json};

const CONFIG: &str = ".workspace-mgr.toml";
const MANIFEST: &str = ".workspace-mgr-task.toml";

fn managed_fixture() -> GitFixture {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["init"]);
    fixture.commit_seed("Initialize workspace");
    fixture.clone_shared();
    fixture
}

fn create_task(fixture: &GitFixture, slug: &str, timestamp: &str) -> (String, PathBuf) {
    workspace(
        &fixture.shared,
        [
            "task",
            "create",
            slug,
            "--title",
            "Repository requirement",
            "--purpose",
            "Verify the repository's minimum workspace-mgr version.",
            "--timestamp",
            timestamp,
        ],
    );
    let task_id = format!("{timestamp}-{slug}");
    let task = fixture.shared.join(&task_id);
    (task_id, task)
}

fn installed() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

/// The canonical rendering of a configuration that declares `version`.
fn declaring(version: &str, config: &str) -> String {
    format!("minimum_cli_version = \"{version}\"\n\n{config}")
}

fn refusal(required: &str, installed: &str, location: &str) -> String {
    format!(
        "workspace-mgr: this repository requires workspace-mgr {required} or newer (`minimum_cli_version` in {location}); this is workspace-mgr {installed}. Tell the user both versions and ask before updating with `cargo install --locked workspace-mgr`, then run `workspace-mgr setup`.\n"
    )
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_refused_with(output: &Output, expected: &str) {
    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        stderr(output)
    );
    assert!(output.stdout.is_empty(), "a refusal prints no report");
    assert_eq!(stderr(output), expected);
}

fn rev(repo: &Path, reference: &str) -> Option<String> {
    let output = git_unchecked(repo, ["rev-parse", "-q", "--verify", reference]);
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn show(repo: &Path, object: &str) -> String {
    String::from_utf8_lossy(&git(repo, ["show", object]).stdout).into_owned()
}

fn commit_message(repo: &Path, oid: &str) -> String {
    String::from_utf8_lossy(&git(repo, ["show", "-s", "--format=%B", oid]).stdout).into_owned()
}

fn porcelain(repo: &Path) -> String {
    String::from_utf8_lossy(&git(repo, ["status", "--porcelain", "--untracked-files=all"]).stdout)
        .into_owned()
}

fn doctor_check(report: &Value, name: &str) -> Value {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == name)
        .cloned()
        .unwrap_or_else(|| panic!("doctor has no {name} check: {report}"))
}

fn failing_checks(report: &Value) -> Vec<String> {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|check| check["status"] != "ok")
        .map(|check| check["name"].as_str().unwrap().to_owned())
        .collect()
}

fn approve(task: &Path, env: &[(&str, &str)], limit: &str, note: &str) -> Value {
    json(&workspace_env(
        task,
        [
            "task",
            "approve-cloud-usage",
            "--limit",
            limit,
            "--note",
            note,
        ],
        env,
    ))
}

#[test]
fn a_repository_requiring_a_newer_cli_refuses_every_command_but_doctor() {
    let fixture = managed_fixture();
    let (task_id, task) = create_task(&fixture, "requirement", "20260918-180000");
    std::fs::write(task.join("notes.md"), "notes\n").unwrap();
    let config_path = fixture.shared.join(CONFIG);
    let original = read(&config_path);
    let manifest = task.join(MANIFEST);
    let manifest_before = read(&manifest);
    let notes = format!("{task_id}/notes.md");
    let later = fixture.shared.join("20260918-180100-later");
    let local_branch = rev(&fixture.shared, "refs/heads/codex/requirement");
    assert!(local_branch.is_some());
    std::fs::write(&config_path, declaring("99.0.0", &original)).unwrap();
    let version = installed();
    let installed = version.to_string();
    let local = refusal("99.0.0", &installed, CONFIG);

    let shared = fixture.shared.as_path();
    let commands: Vec<(&Path, Vec<&str>)> = vec![
        (shared, vec!["instructions"]),
        (shared, vec!["instructions", "core"]),
        (shared, vec!["config", "show"]),
        (shared, vec!["init", "--dry-run"]),
        (shared, vec!["init"]),
        (shared, vec!["refresh", "--dry-run"]),
        (shared, vec!["refresh"]),
        (
            shared,
            vec![
                "task",
                "create",
                "later",
                "--title",
                "Later",
                "--purpose",
                "Start later.",
                "--timestamp",
                "20260918-180100",
            ],
        ),
        (&task, vec!["task", "status"]),
        (&task, vec!["plan"]),
        (&task, vec!["publish", "-m", "Publish notes", "--dry-run"]),
        (&task, vec!["publish", "-m", "Publish notes"]),
        (&task, vec!["storage", "status"]),
        (&task, vec!["untrack", notes.as_str(), "--dry-run"]),
        (&task, vec!["remove", notes.as_str()]),
        (
            &task,
            vec![
                "task",
                "approve-cloud-usage",
                "--limit",
                "2GiB",
                "--note",
                "The user approved 2 GiB",
            ],
        ),
        (&task, vec!["task", "rename", "renamed", "--dry-run"]),
        (&task, vec!["task", "discard", "--dry-run"]),
    ];
    for (cwd, args) in &commands {
        let output = workspace_unchecked(cwd, args);
        assert_eq!(stderr(&output), local, "{args:?}");
        assert_refused_with(&output, &local);
    }
    // Nothing changed locally or remotely.
    assert_eq!(read(&manifest), manifest_before);
    assert_eq!(read(&task.join("notes.md")), "notes\n");
    assert!(!later.exists());
    assert_eq!(read(&config_path), declaring("99.0.0", &original));
    assert_eq!(rev(&fixture.remote, "refs/heads/codex/requirement"), None);
    assert_eq!(
        rev(&fixture.shared, "refs/heads/codex/requirement"),
        local_branch
    );
    assert_eq!(rev(&fixture.shared, "refs/heads/codex/later"), None);

    // doctor still runs and names both versions.
    let doctor = workspace_unchecked(shared, ["doctor"]);
    assert_eq!(doctor.status.code(), Some(2));
    assert_eq!(
        stderr(&doctor),
        "workspace-mgr: doctor found one or more errors\n"
    );
    let report = json(&doctor);
    assert_eq!(report["status"], "error");
    assert_eq!(failing_checks(&report), ["cli-version"]);
    assert_eq!(doctor_check(&report, "repository-config")["status"], "ok");
    assert_eq!(
        doctor_check(&report, "cli-version"),
        json!({
            "name": "cli-version",
            "status": "error",
            "detail": format!("installed {installed}, repository requires 99.0.0"),
        })
    );
    let names = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|check| check["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    let config_check = names
        .iter()
        .position(|name| *name == "repository-config")
        .unwrap();
    assert_eq!(names[config_check + 1], "cli-version");

    // A configuration written by a newer release may carry fields this one
    // does not know; the requirement is still what every command reports.
    std::fs::write(
        &config_path,
        format!(
            "{}\n[future]\nsetting = true\n",
            declaring("99.0.0", &original)
        ),
    )
    .unwrap();
    for (cwd, args) in [(shared, vec!["instructions"]), (&task, vec!["plan"])] {
        assert_refused_with(&workspace_unchecked(cwd, &args), &local);
    }
    let report = json(&workspace_unchecked(shared, ["doctor"]));
    assert_eq!(
        failing_checks(&report),
        ["repository-config", "cli-version"]
    );
    let config_error = doctor_check(&report, "repository-config")["detail"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        config_error.contains("unknown field `future`"),
        "{config_error}"
    );
    assert_eq!(
        doctor_check(&report, "cli-version")["detail"],
        format!("installed {installed}, repository requires 99.0.0")
    );

    // Semantic-version precedence decides: a pre-release declaration sorts
    // below its release, so a newer one is refused before strict parsing.
    let next_patch = format!(
        "{}.{}.{}-rc.1",
        version.major,
        version.minor,
        version.patch + 1
    );
    let next_minor = format!("{}.{}.0", version.major, version.minor + 1);
    for newer in [&next_patch, &next_minor] {
        std::fs::write(&config_path, declaring(newer, &original)).unwrap();
        assert_refused_with(
            &workspace_unchecked(&task, ["task", "status"]),
            &refusal(newer, &installed, CONFIG),
        );
    }
    for older in [installed.clone(), "0.1.0".to_owned()] {
        std::fs::write(&config_path, declaring(&older, &original)).unwrap();
        assert_eq!(json(&workspace(&task, ["plan"]))["status"], "dry_run");
        let report = json(&workspace(shared, ["doctor"]));
        assert_eq!(report["status"], "ok");
        assert_eq!(
            doctor_check(&report, "cli-version")["detail"],
            format!("installed {installed}, repository requires {older}")
        );
    }

    // Only plain release versions are declarations.
    for (invalid, error) in [
        (
            "0.4".to_owned(),
            "minimum_cli_version must be a plain semantic version",
        ),
        (
            "0.1.0+build.7".to_owned(),
            "minimum_cli_version must not carry build metadata",
        ),
        (
            format!("{}.{}.{}-rc.1", version.major, version.minor, version.patch),
            "minimum_cli_version must be a release version such as \"0.4.0\", not a pre-release",
        ),
    ] {
        std::fs::write(&config_path, declaring(&invalid, &original)).unwrap();
        let output = workspace_unchecked(&task, ["task", "status"]);
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains(error), "{}", stderr(&output));
    }

    std::fs::write(&config_path, &original).unwrap();
    assert_eq!(json(&workspace(&task, ["plan"]))["status"], "dry_run");
    assert_eq!(
        doctor_check(&json(&workspace(shared, ["doctor"])), "cli-version"),
        json!({
            "name": "cli-version",
            "status": "ok",
            "detail": format!("installed {installed}, repository declares no minimum version"),
        })
    );
}

#[test]
fn plan_and_publish_refuse_when_only_the_shared_branch_requires_a_newer_cli() {
    let fixture = managed_fixture();
    let (_, published) = create_task(&fixture, "published", "20260918-181000");
    let (_, fresh) = create_task(&fixture, "fresh", "20260918-181500");
    workspace(&published, ["publish", "-m", "Publish scaffold"]);
    let published_branch = "refs/heads/codex/published";
    let fresh_branch = "refs/heads/codex/fresh";
    let tip = rev(&fixture.remote, published_branch).unwrap();
    let local_tip = rev(&fixture.shared, published_branch);
    let fresh_local = rev(&fixture.shared, fresh_branch);
    std::fs::write(published.join("notes.md"), "notes\n").unwrap();
    std::fs::write(fresh.join("notes.md"), "notes\n").unwrap();
    let local_config = read(&fixture.shared.join(CONFIG));

    // A newer release raised the shared branch after this checkout was
    // refreshed.
    let seed_config = fixture.seed.join(CONFIG);
    std::fs::write(&seed_config, declaring("99.0.0", &read(&seed_config))).unwrap();
    fixture.commit_seed("Require a newer workspace-mgr");
    let remote = refusal(
        "99.0.0",
        &installed().to_string(),
        ".workspace-mgr.toml on origin/main",
    );

    // Task creation checks the fetched base before it creates a branch,
    // directory, or worktree, and so does its rehearsal, which fetches the
    // missing base commit without moving any ref.
    let tracking = rev(&fixture.shared, "refs/remotes/origin/main");
    let create = |slug: &str, infrastructure: bool, dry_run: bool| {
        let mut args = vec![
            "task",
            "create",
            slug,
            "--title",
            "Later",
            "--purpose",
            "Start later.",
        ];
        if infrastructure {
            args.extend([
                "--kind",
                "infrastructure",
                "--scope",
                "tools",
                "--scope-note",
                "The user requested tools.",
            ]);
        } else {
            args.extend(["--timestamp", "20260918-182000"]);
        }
        if dry_run {
            args.push("--dry-run");
        }
        workspace_unchecked(&fixture.shared, args)
    };
    assert_refused_with(&create("later", false, true), &remote);
    assert_eq!(rev(&fixture.shared, "refs/remotes/origin/main"), tracking);
    for (slug, infrastructure, dry_run) in [
        ("later", false, false),
        ("tools", true, true),
        ("tools", true, false),
    ] {
        assert_refused_with(&create(slug, infrastructure, dry_run), &remote);
    }
    assert_eq!(rev(&fixture.shared, "refs/heads/codex/later"), None);
    assert_eq!(rev(&fixture.shared, "refs/heads/codex/infra-tools"), None);
    assert!(!fixture.shared.join("20260918-182000-later").exists());
    assert!(
        !fixture
            .shared
            .join(".git/workspace-mgr/checkouts/infra-tools")
            .exists()
    );

    for (task, args) in [
        (&published, vec!["plan"]),
        (
            &published,
            vec!["publish", "-m", "Publish notes", "--dry-run"],
        ),
        (&published, vec!["publish", "-m", "Publish notes"]),
        (&fresh, vec!["plan"]),
        (&fresh, vec!["publish", "-m", "Publish notes"]),
    ] {
        assert_refused_with(&workspace_unchecked(task, &args), &remote);
    }
    assert_eq!(
        rev(&fixture.remote, published_branch).as_deref(),
        Some(tip.as_str())
    );
    assert_eq!(rev(&fixture.shared, published_branch), local_tip);
    assert_eq!(rev(&fixture.remote, fresh_branch), None);
    assert_eq!(rev(&fixture.shared, fresh_branch), fresh_local);

    // Discard checks it before it writes its plan, deletes a branch, or
    // purges anything.
    let manifest = published.join(MANIFEST);
    assert_refused_with(
        &workspace_unchecked(&published, ["task", "discard", "--dry-run"]),
        &remote,
    );
    assert_refused_with(
        &workspace_unchecked(
            &fixture.shared,
            [
                "task",
                "discard",
                "--manifest",
                manifest.to_str().unwrap(),
                "--confirm",
                "20260918-181000-published",
            ],
        ),
        &remote,
    );
    assert!(
        walkdir::WalkDir::new(fixture.shared.join(".git/workspace-mgr"))
            .into_iter()
            .map(Result::unwrap)
            .all(|entry| entry.file_name() != "discard-plan.json")
    );
    assert!(published.join("notes.md").is_file());
    assert_eq!(
        rev(&fixture.remote, published_branch).as_deref(),
        Some(tip.as_str())
    );
    assert_eq!(rev(&fixture.shared, published_branch), local_tip);

    // Rename fetches the shared branch as well and checks it before it moves
    // or rewrites anything, also in its rehearsal.
    let manifest_before = read(&manifest);
    for args in [
        &["task", "rename", "renamed", "--dry-run"][..],
        &["task", "rename", "renamed"][..],
    ] {
        assert_refused_with(&workspace_unchecked(&published, args), &remote);
    }
    assert_eq!(read(&manifest), manifest_before);
    assert!(!fixture.shared.join("20260918-181000-renamed").exists());

    // The checkout itself still declares nothing, so local commands work,
    // but doctor reports the declaration it last fetched from the shared
    // branch.
    assert_eq!(read(&fixture.shared.join(CONFIG)), local_config);
    let status = json(&workspace(&published, ["task", "status"]));
    assert_eq!(status["task_id"], "20260918-181000-published");
    workspace(&fixture.shared, ["instructions"]);
    let doctor = workspace_unchecked(&fixture.shared, ["doctor"]);
    assert_eq!(doctor.status.code(), Some(2));
    let report = json(&doctor);
    assert_eq!(failing_checks(&report), ["cli-version"]);
    assert_eq!(
        doctor_check(&report, "cli-version")["detail"],
        format!("installed {}, origin/main requires 99.0.0", installed())
    );
}

#[test]
fn plan_and_publish_refuse_a_task_branch_that_requires_a_newer_cli() {
    let fixture = managed_fixture();
    let (task_id, task) = create_task(&fixture, "tip", "20260918-181800");
    workspace(&task, ["publish", "-m", "Publish scaffold"]);
    let branch = "refs/heads/codex/tip";

    // A newer release published the task branch from another clone.
    git(&fixture.seed, ["fetch", "-q", "origin", "codex/tip"]);
    git(&fixture.seed, ["switch", "-q", "-c", "tip", "FETCH_HEAD"]);
    let seed_config = fixture.seed.join(CONFIG);
    std::fs::write(&seed_config, declaring("99.0.0", &read(&seed_config))).unwrap();
    git(&fixture.seed, ["add", "-A"]);
    git(
        &fixture.seed,
        [
            "commit",
            "-q",
            "-m",
            &format!("Publish from a newer release\n\nWorkspace-Task: {task_id}"),
        ],
    );
    git(&fixture.seed, ["push", "-q", "origin", "HEAD:codex/tip"]);
    git(&fixture.seed, ["switch", "-q", "main"]);
    let tip = rev(&fixture.remote, branch).unwrap();

    std::fs::write(task.join("notes.md"), "notes\n").unwrap();
    let on_tip = refusal(
        "99.0.0",
        &installed().to_string(),
        ".workspace-mgr.toml on origin/codex/tip",
    );
    for args in [
        &["plan"][..],
        &["publish", "-m", "Publish notes", "--dry-run"][..],
        &["publish", "-m", "Publish notes"][..],
        &["task", "rename", "renamed", "--dry-run"][..],
        &["task", "rename", "renamed"][..],
    ] {
        assert_refused_with(&workspace_unchecked(&task, args), &on_tip);
    }
    assert_eq!(rev(&fixture.remote, branch).as_deref(), Some(tip.as_str()));
    assert!(task.join(MANIFEST).is_file());
    assert!(!fixture.shared.join("20260918-181800-renamed").exists());
}

#[test]
fn refresh_refuses_an_incoming_requirement_before_changing_the_checkout() {
    let fixture = managed_fixture();
    let config_path = fixture.shared.join(CONFIG);
    let original = read(&config_path);
    let head = rev(&fixture.shared, "HEAD").unwrap();
    std::fs::write(fixture.seed.join("incoming.md"), "incoming\n").unwrap();
    std::fs::write(fixture.seed.join(CONFIG), declaring("99.0.0", &original)).unwrap();
    fixture.commit_seed("Require a newer workspace-mgr");
    let remote = refusal(
        "99.0.0",
        &installed().to_string(),
        ".workspace-mgr.toml on origin/main",
    );
    for args in [&["refresh", "--dry-run"][..], &["refresh"][..]] {
        assert_refused_with(&workspace_unchecked(&fixture.shared, args), &remote);
        assert_eq!(rev(&fixture.shared, "HEAD").as_deref(), Some(head.as_str()));
        assert_eq!(
            rev(&fixture.shared, "refs/heads/main").as_deref(),
            Some(head.as_str())
        );
        assert!(!fixture.shared.join("incoming.md").exists());
        assert_eq!(read(&config_path), original);
        assert_eq!(porcelain(&fixture.shared), "");
    }

    // An incoming declaration this release meets is followed like any other
    // change, and init keeps it exactly.
    let met = declaring(&installed().to_string(), &original);
    std::fs::write(fixture.seed.join(CONFIG), &met).unwrap();
    fixture.commit_seed("Require this workspace-mgr");
    let seed_head = rev(&fixture.seed, "HEAD").unwrap();
    let refreshed = json(&workspace(&fixture.shared, ["refresh"]));
    assert_eq!(refreshed["status"], "updated");
    assert_eq!(refreshed["new_oid"], seed_head.as_str());
    assert_eq!(read(&config_path), met);
    assert!(fixture.shared.join("incoming.md").is_file());
    workspace(&fixture.shared, ["init"]);
    assert_eq!(read(&config_path), met);
    assert_eq!(porcelain(&fixture.shared), "");
}

#[test]
fn init_preserves_the_declaration_and_publication_raises_it_from_there() {
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["init"]);
    let seed_config = fixture.seed.join(CONFIG);
    let plain = read(&seed_config);
    let declared = declaring("0.2.0", &plain);
    std::fs::write(&seed_config, &declared).unwrap();
    for args in [&["init", "--dry-run"][..], &["init"][..]] {
        workspace(&fixture.seed, args);
        assert_eq!(read(&seed_config), declared, "{args:?}");
    }
    fixture.commit_seed("Initialize workspace");
    fixture.clone_shared();
    let config_path = fixture.shared.join(CONFIG);
    workspace(&fixture.shared, ["init"]);
    assert_eq!(read(&config_path), declared);
    assert_eq!(porcelain(&fixture.shared), "");
    assert_eq!(
        doctor_check(
            &json(&workspace(&fixture.shared, ["doctor"])),
            "cli-version"
        ),
        json!({
            "name": "cli-version",
            "status": "ok",
            "detail": format!("installed {}, repository requires 0.2.0", installed()),
        })
    );

    if cfg!(not(feature = "test-storage")) {
        // Only test builds can stand in for a release that reads schema 3.
        return;
    }
    // The first schema 3 manifest raises the declaration in the published
    // tree, reporting the value it replaced.
    let release = [(CLI_VERSION_ENV, "0.4.0")];
    let (task_id, task) = create_task(&fixture, "raise", "20260918-182000");
    approve(
        &task,
        &[],
        "2GiB",
        "The user approved 2 GiB for the evaluation outputs",
    );
    let plan = json(&workspace_env(&task, ["plan"], &release));
    let requirement = json!({
        "path": CONFIG,
        "change": "raise",
        "minimum_cli_version": "0.4.0",
        "previous_minimum_cli_version": "0.2.0",
        "task_manifest_schema": 3,
    });
    assert_eq!(plan["repository_requirement"], requirement);
    assert_eq!(
        plan["changed_paths"],
        json!([
            CONFIG,
            format!("{task_id}/{MANIFEST}"),
            format!("{task_id}/README.md"),
        ])
    );
    let published = json(&workspace_env(
        &task,
        ["publish", "-m", "Publish raise"],
        &release,
    ));
    assert_eq!(published["repository_requirement"], requirement);
    let tip = published["remote_oid"].as_str().unwrap();
    assert_eq!(
        show(&fixture.remote, &format!("{tip}:{CONFIG}")),
        declaring("0.4.0", &plain)
    );
    assert!(
        commit_message(&fixture.remote, tip).contains(
            "\nWorkspace-Requirement: minimum_cli_version=0.4.0 (task manifest schema 3)\n"
        )
    );
    assert_eq!(read(&config_path), declared);
    assert_eq!(
        porcelain(&fixture.shared),
        format!("?? {task_id}/{MANIFEST}\n?? {task_id}/README.md\n")
    );
}

#[cfg(feature = "test-storage")]
#[test]
fn a_raise_applies_on_top_of_an_authorized_configuration_change() {
    let fixture = managed_fixture();
    let release = [(CLI_VERSION_ENV, "0.4.0")];
    let config_path = fixture.shared.join(CONFIG);
    let plain = read(&config_path);
    let (task_id, task) = create_task(&fixture, "config-change", "20260918-183000");
    approve(
        &task,
        &[],
        "2GiB",
        "The user approved 2 GiB for the configuration task",
    );
    // The user authorized this task to change the shared configuration.
    let edited = declaring("0.2.0", &plain);
    std::fs::write(&config_path, &edited).unwrap();
    let scope_note = "The user asked this task to declare workspace-mgr 0.2.0";
    let scoped = |args: &[&str]| {
        let mut args = args.to_vec();
        args.extend(["--include", CONFIG, "--scope-note", scope_note]);
        json(&workspace_env(&task, args, &release))
    };
    let requirement = json!({
        "path": CONFIG,
        "change": "raise",
        "minimum_cli_version": "0.4.0",
        "previous_minimum_cli_version": "0.2.0",
        "task_manifest_schema": 3,
    });
    let plan = scoped(&["plan"]);
    assert_eq!(plan["repository_requirement"], requirement);
    let published = scoped(&["publish", "-m", "Declare the configuration"]);
    assert_eq!(published["repository_requirement"], requirement);
    assert_eq!(
        published["changed_paths"],
        json!([
            CONFIG,
            format!("{task_id}/{MANIFEST}"),
            format!("{task_id}/README.md"),
        ])
    );
    let tip = published["remote_oid"].as_str().unwrap().to_owned();
    assert_eq!(
        show(&fixture.remote, &format!("{tip}:{CONFIG}")),
        declaring("0.4.0", &plain)
    );
    assert_eq!(
        commit_message(&fixture.remote, &tip),
        format!(
            "Declare the configuration\n\nWorkspace-Task: {task_id}\nWorkspace-Scope: {task_id}, {CONFIG}\nScope-Authorization: {CONFIG} -- {scope_note}\nWorkspace-Requirement: minimum_cli_version=0.4.0 (task manifest schema 3)\nCloud-Usage-Approval: limit_bytes=2147483648; note=The user approved 2 GiB for the configuration task\n\n"
        )
    );
    // The checkout keeps the user's edit.
    assert_eq!(read(&config_path), edited);

    // Publishing the same scope again restores the branch's declaration
    // instead of lowering it, and reports no new raise.
    std::fs::write(task.join("notes.md"), "notes\n").unwrap();
    let again = scoped(&["publish", "-m", "Publish notes"]);
    assert_eq!(again["status"], "pushed");
    assert!(again.get("repository_requirement").is_none(), "{again}");
    assert_eq!(
        again["changed_paths"],
        json!([format!("{task_id}/notes.md")])
    );
    let again_tip = again["remote_oid"].as_str().unwrap();
    assert_eq!(
        show(&fixture.remote, &format!("{again_tip}:{CONFIG}")),
        declaring("0.4.0", &plain)
    );
    assert!(!commit_message(&fixture.remote, again_tip).contains("Workspace-Requirement"));
}

#[cfg(feature = "test-storage")]
#[test]
fn init_with_storage_keeps_the_declaration_first() {
    if which::which("dvc").is_err() {
        eprintln!("skipping: dvc is unavailable");
        return;
    }
    let fixture = GitFixture::new();
    workspace(&fixture.seed, ["init"]);
    let config_path = fixture.seed.join(CONFIG);
    let declared = declaring("0.2.0", &read(&config_path));
    std::fs::write(&config_path, &declared).unwrap();
    let storage = fixture.root.join("storage-remote");
    workspace(
        &fixture.seed,
        ["init", "--s3-url", storage.to_str().unwrap()],
    );
    assert_eq!(
        read(&config_path),
        format!("{declared}\n[s3]\nurl = \"{}\"\n", storage.display())
    );
}

/// Concurrent raises on different task branches agree, so the user's merges
/// stay clean, and a raise follows a shared branch that a later release
/// raised further.
#[cfg(feature = "test-storage")]
#[test]
fn concurrent_raises_agree_and_follow_the_shared_branch() {
    let fixture = managed_fixture();
    let plain = read(&fixture.shared.join(CONFIG));
    let (alpha_id, alpha) = create_task(&fixture, "alpha", "20260918-190000");
    let (beta_id, beta) = create_task(&fixture, "beta", "20260918-190500");
    let (gamma_id, gamma) = create_task(&fixture, "gamma", "20260918-191000");
    for task in [&alpha, &beta, &gamma] {
        let scaffold = json(&workspace(task, ["publish", "-m", "Publish scaffold"]));
        assert_eq!(scaffold["status"], "pushed");
        assert!(scaffold.get("repository_requirement").is_none());
        assert!(
            !scaffold["changed_paths"]
                .as_array()
                .unwrap()
                .contains(&json!(CONFIG))
        );
    }

    // Two tasks gain approvals and publish independently; a release
    // candidate publishes the declaration of its release.
    let first = json!({
        "path": CONFIG,
        "change": "raise",
        "minimum_cli_version": "0.4.0",
        "previous_minimum_cli_version": null,
        "task_manifest_schema": 3,
    });
    let mut tips = Vec::new();
    for ((task_id, task), version) in [(&alpha_id, &alpha), (&beta_id, &beta)]
        .into_iter()
        .zip(["0.4.0", "0.4.0-rc.1"])
    {
        approve(task, &[], "2GiB", "The user approved 2 GiB for checkpoints");
        std::fs::write(task.join("notes.md"), format!("{task_id}\n")).unwrap();
        let published = json(&workspace_env(
            task,
            ["publish", "-m", "Publish notes"],
            &[(CLI_VERSION_ENV, version)],
        ));
        assert_eq!(published["repository_requirement"], first, "{task_id}");
        assert_eq!(
            published["changed_paths"],
            json!([
                CONFIG,
                format!("{task_id}/{MANIFEST}"),
                format!("{task_id}/notes.md"),
            ])
        );
        tips.push(published["remote_oid"].as_str().unwrap().to_owned());
    }
    let raised_blob = |tip: &str| rev(&fixture.remote, &format!("{tip}:{CONFIG}")).unwrap();
    assert_eq!(raised_blob(&tips[0]), raised_blob(&tips[1]));
    assert_eq!(
        show(&fixture.remote, &format!("{}:{CONFIG}", tips[0])),
        declaring("0.4.0", &plain)
    );

    // The user merges both pull requests without a conflict.
    git(&fixture.seed, ["fetch", "-q", "origin"]);
    git(
        &fixture.seed,
        ["merge", "--no-edit", "-q", "origin/codex/alpha"],
    );
    git(
        &fixture.seed,
        ["merge", "--no-ff", "--no-edit", "-q", "origin/codex/beta"],
    );
    git(&fixture.seed, ["push", "-q", "origin", "main"]);
    assert_eq!(
        rev(&fixture.remote, &format!("refs/heads/main:{CONFIG}")).unwrap(),
        raised_blob(&tips[0])
    );

    // A release older than the declaration refuses the merged branch before
    // anything changes, and a release that meets it follows.
    let older = [(CLI_VERSION_ENV, "0.3.0")];
    let current = [(CLI_VERSION_ENV, "0.4.0")];
    let on_main = ".workspace-mgr.toml on origin/main";
    let head = rev(&fixture.shared, "HEAD").unwrap();
    assert_refused_with(
        &workspace_env_unchecked(&fixture.shared, ["refresh"], &older),
        &refusal("0.4.0", "0.3.0", on_main),
    );
    assert_refused_with(
        &workspace_env_unchecked(&gamma, ["plan"], &older),
        &refusal("0.4.0", "0.3.0", on_main),
    );
    assert_eq!(rev(&fixture.shared, "HEAD").as_deref(), Some(head.as_str()));
    // A release candidate meets the declaration of its own release.
    let candidate = [(CLI_VERSION_ENV, "0.4.0-rc.1")];
    assert_eq!(
        json(&workspace_env(
            &fixture.shared,
            ["refresh", "--dry-run"],
            &candidate
        ))["status"],
        "dry_run"
    );
    let refreshed = json(&workspace_env(&fixture.shared, ["refresh"], &current));
    assert_eq!(refreshed["status"], "updated");
    assert_eq!(
        read(&fixture.shared.join(CONFIG)),
        declaring("0.4.0", &plain)
    );
    assert_refused_with(
        &workspace_env_unchecked(&fixture.shared, ["instructions"], &older),
        &refusal("0.4.0", "0.3.0", CONFIG),
    );
    let report = json(&workspace_env(&fixture.shared, ["doctor"], &current));
    assert_eq!(report["status"], "ok");
    assert_eq!(
        doctor_check(&report, "cli-version")["detail"],
        "installed 0.4.0, repository requires 0.4.0"
    );

    // A later release raises the shared branch further for its own schema.
    let seed_config = fixture.seed.join(CONFIG);
    std::fs::write(&seed_config, declaring("0.5.0", &plain)).unwrap();
    fixture.commit_seed("Adopt a newer task manifest schema");
    let later = [(CLI_VERSION_ENV, "0.5.0")];
    approve(
        &gamma,
        &current,
        "2GiB",
        "The user approved 2 GiB for the gamma outputs",
    );
    assert_refused_with(
        &workspace_env_unchecked(&gamma, ["plan"], &current),
        &refusal("0.5.0", "0.4.0", on_main),
    );

    // A task branch that predates both raises follows the shared branch's
    // higher declaration, so its configuration matches main exactly.
    let raised = json!({
        "path": CONFIG,
        "change": "follow",
        "minimum_cli_version": "0.5.0",
        "previous_minimum_cli_version": null,
        "task_manifest_schema": 3,
    });
    let plan = json(&workspace_env(&gamma, ["plan"], &later));
    assert_eq!(plan["repository_requirement"], raised);
    let published = json(&workspace_env(
        &gamma,
        ["publish", "-m", "Publish gamma approval"],
        &later,
    ));
    assert_eq!(published["repository_requirement"], raised);
    assert_eq!(
        published["changed_paths"],
        json!([CONFIG, format!("{gamma_id}/{MANIFEST}")])
    );
    let tip = published["remote_oid"].as_str().unwrap();
    assert_eq!(
        raised_blob(tip),
        rev(&fixture.remote, &format!("refs/heads/main:{CONFIG}")).unwrap()
    );
    assert!(
        commit_message(&fixture.remote, tip).contains(
            "\nWorkspace-Requirement: minimum_cli_version=0.5.0 (task manifest schema 3; follows origin/main)\n"
        )
    );
    git(&fixture.seed, ["fetch", "-q", "origin"]);
    git(
        &fixture.seed,
        ["merge", "--no-ff", "--no-edit", "-q", "origin/codex/gamma"],
    );
    assert_eq!(read(&seed_config), declaring("0.5.0", &plain));

    // A branch that already meets its own requirement is never raised again
    // or lowered.
    std::fs::write(alpha.join("notes.md"), "more notes\n").unwrap();
    let again = json(&workspace_env(
        &alpha,
        ["publish", "-m", "Publish more"],
        &later,
    ));
    assert!(again.get("repository_requirement").is_none());
    assert_eq!(
        again["changed_paths"],
        json!([format!("{alpha_id}/notes.md")])
    );
    assert_eq!(
        raised_blob(again["remote_oid"].as_str().unwrap()),
        raised_blob(&tips[0])
    );
}

/// Infrastructure manifests are private, so only a shared tree that already
/// contains a schema 3 manifest without the declaration makes an
/// infrastructure publication raise it; the isolated worktree follows.
#[cfg(feature = "test-storage")]
#[test]
fn an_infrastructure_publication_restores_a_missing_requirement() {
    let fixture = managed_fixture();
    let plain = read(&fixture.shared.join(CONFIG));
    let created = json(&workspace(
        &fixture.shared,
        [
            "task",
            "create",
            "shared-notes",
            "--kind",
            "infrastructure",
            "--title",
            "Shared notes",
            "--purpose",
            "Publish repository-wide notes.",
            "--scope",
            "docs",
            "--scope-note",
            "The user requested shared notes.",
        ],
    ));
    let worktree = PathBuf::from(created["path"].as_str().unwrap());
    approve(
        &worktree,
        &[],
        "2GiB",
        "The user approved 2 GiB of shared notes",
    );
    std::fs::create_dir(worktree.join("docs")).unwrap();
    std::fs::write(worktree.join("docs/notes.md"), "notes\n").unwrap();
    // A private manifest never needs a newer release, so any build publishes.
    let published = json(&workspace(&worktree, ["publish", "-m", "Publish notes"]));
    assert_eq!(published["status"], "pushed");
    assert!(published.get("repository_requirement").is_none());
    assert_eq!(published["changed_paths"], json!(["docs/notes.md"]));
    let tip = published["remote_oid"].as_str().unwrap();
    assert_eq!(show(&fixture.remote, &format!("{tip}:{CONFIG}")), plain);

    // Someone merges a deliverable's schema 3 manifest into main but drops
    // the declaration while resolving a conflict.
    let seed_task = fixture.seed.join("20260918-193000-merged");
    std::fs::create_dir(&seed_task).unwrap();
    std::fs::write(
        seed_task.join(MANIFEST),
        "schema_version = 3\nkind = \"deliverable\"\nid = \"20260918-193000-merged\"\nslug = \"merged\"\npath = \"20260918-193000-merged\"\nbranch = \"codex/merged\"\ntitle = \"Merged\"\npurpose = \"Merged earlier.\"\nadditional_scopes = []\n\n[cloud_usage_approval]\nlimit_bytes = 2147483648\nnote = \"The user approved 2 GiB\"\n",
    )
    .unwrap();
    fixture.commit_seed("Merge a task without its requirement");
    let (_, next) = {
        let created = json(&workspace(
            &fixture.shared,
            [
                "task",
                "create",
                "more-notes",
                "--kind",
                "infrastructure",
                "--title",
                "More notes",
                "--purpose",
                "Publish more repository-wide notes.",
                "--scope",
                "guides",
                "--scope-note",
                "The user requested more shared notes.",
            ],
        ));
        (
            created["task_id"].as_str().unwrap().to_owned(),
            PathBuf::from(created["path"].as_str().unwrap()),
        )
    };
    std::fs::create_dir(next.join("guides")).unwrap();
    std::fs::write(next.join("guides/usage.md"), "usage\n").unwrap();
    let requirement = json!({
        "path": CONFIG,
        "change": "raise",
        "minimum_cli_version": "0.4.0",
        "previous_minimum_cli_version": null,
        "task_manifest_schema": 3,
    });
    let current = [(CLI_VERSION_ENV, "0.4.0")];
    // A build that cannot read the merged manifest cannot declare it either.
    assert_refused_with(
        &workspace_env_unchecked(
            &next,
            ["publish", "-m", "Publish usage guide"],
            &[(CLI_VERSION_ENV, "0.3.0")],
        ),
        // The manifest belongs to another task, so only an update helps.
        "workspace-mgr: this build (workspace-mgr 0.3.0) cannot publish 20260918-193000-merged/.workspace-mgr-task.toml, another task's manifest in this publication, because its schema 3 requires workspace-mgr 0.4.0 or newer; update workspace-mgr. Tell the user both versions and ask before updating with `cargo install --locked workspace-mgr`.\n",
    );
    let published = json(&workspace_env(
        &next,
        ["publish", "-m", "Publish usage guide"],
        &current,
    ));
    assert_eq!(published["repository_requirement"], requirement);
    assert_eq!(
        published["changed_paths"],
        json!([CONFIG, "guides/usage.md"])
    );
    let tip = published["remote_oid"].as_str().unwrap();
    assert_eq!(
        show(&fixture.remote, &format!("{tip}:{CONFIG}")),
        declaring("0.4.0", &plain)
    );
    // The isolated worktree checks out the task branch, so it now carries the
    // raised configuration and stays clean; the shared checkout is untouched.
    assert_eq!(read(&next.join(CONFIG)), declaring("0.4.0", &plain));
    assert_eq!(porcelain(&next), "");
    assert_eq!(read(&fixture.shared.join(CONFIG)), plain);
    std::fs::write(next.join("guides/usage.md"), "more usage\n").unwrap();
    let again = json(&workspace_env(
        &next,
        ["publish", "-m", "Publish more usage"],
        &current,
    ));
    assert_eq!(again["status"], "pushed");
    assert!(again.get("repository_requirement").is_none());
    assert_eq!(again["changed_paths"], json!(["guides/usage.md"]));
}

/// Merges the published task branch into the shared branch the way the user
/// would, failing on any conflict.
#[cfg(feature = "test-storage")]
fn merge_into_main(fixture: &GitFixture, branch: &str) {
    git(&fixture.seed, ["fetch", "-q", "origin"]);
    git(
        &fixture.seed,
        [
            "merge",
            "--no-ff",
            "--no-edit",
            "-q",
            &format!("origin/{branch}"),
        ],
    );
    git(&fixture.seed, ["push", "-q", "origin", "main"]);
}

/// The paths the task branch's pull request changes, relative to where it
/// left the shared branch.
#[cfg(feature = "test-storage")]
fn changed_against_main(fixture: &GitFixture, branch: &str) -> String {
    git(&fixture.seed, ["fetch", "-q", "origin"]);
    String::from_utf8_lossy(
        &git(
            &fixture.seed,
            [
                "diff",
                "--name-only",
                &format!("origin/main...origin/{branch}"),
            ],
        )
        .stdout,
    )
    .into_owned()
}

/// A task branch raised before the shared branch was raised further follows
/// it on its next publication, so the user's merge stays clean, while a task
/// branch that never needed a newer release never touches the configuration.
#[cfg(feature = "test-storage")]
#[test]
fn an_unmerged_raise_follows_a_further_raised_shared_branch_and_merges_cleanly() {
    let fixture = managed_fixture();
    let plain = read(&fixture.shared.join(CONFIG));
    let (delta_id, delta) = create_task(&fixture, "delta", "20260918-194000");
    let (_, plain_task) = create_task(&fixture, "untouched", "20260918-194500");
    workspace(&plain_task, ["publish", "-m", "Publish scaffold"]);
    let current = [(CLI_VERSION_ENV, "0.4.0")];
    approve(
        &delta,
        &current,
        "2GiB",
        "The user approved 2 GiB for delta",
    );
    let raised = json(&workspace_env(
        &delta,
        ["publish", "-m", "Publish the approval"],
        &current,
    ));
    assert_eq!(raised["repository_requirement"]["change"], "raise");
    let branch_config = |tip: &str| show(&fixture.remote, &format!("{tip}:{CONFIG}"));
    assert_eq!(
        branch_config(raised["remote_oid"].as_str().unwrap()),
        declaring("0.4.0", &plain)
    );

    // A later release raises the shared branch further for its own schema
    // while the delta pull request is still open.
    std::fs::write(fixture.seed.join(CONFIG), declaring("0.5.0", &plain)).unwrap();
    std::fs::write(fixture.seed.join("later.md"), "later\n").unwrap();
    fixture.commit_seed("Adopt a newer task manifest schema");

    // Delta's next publication follows main exactly.
    let later = [(CLI_VERSION_ENV, "0.5.0")];
    std::fs::write(delta.join("notes.md"), "notes\n").unwrap();
    let follow = json!({
        "path": CONFIG,
        "change": "follow",
        "minimum_cli_version": "0.5.0",
        "previous_minimum_cli_version": "0.4.0",
        "task_manifest_schema": 3,
    });
    let plan = json(&workspace_env(&delta, ["plan"], &later));
    assert_eq!(plan["repository_requirement"], follow);
    assert_eq!(
        plan["changed_paths"],
        json!([CONFIG, format!("{delta_id}/notes.md")])
    );
    let published = json(&workspace_env(
        &delta,
        ["publish", "-m", "Publish notes"],
        &later,
    ));
    assert_eq!(published["repository_requirement"], follow);
    let tip = published["remote_oid"].as_str().unwrap();
    assert!(
        commit_message(&fixture.remote, tip).contains(
            "\nWorkspace-Requirement: minimum_cli_version=0.5.0 (task manifest schema 3; follows origin/main)\n"
        )
    );
    assert_eq!(
        rev(&fixture.remote, &format!("{tip}:{CONFIG}")),
        rev(&fixture.remote, &format!("refs/heads/main:{CONFIG}"))
    );
    assert_eq!(read(&fixture.shared.join(CONFIG)), plain);
    // Once it follows, nothing changes again.
    std::fs::write(delta.join("notes.md"), "more notes\n").unwrap();
    let again = json(&workspace_env(
        &delta,
        ["publish", "-m", "Publish more notes"],
        &later,
    ));
    assert!(again.get("repository_requirement").is_none());
    assert_eq!(
        again["changed_paths"],
        json!([format!("{delta_id}/notes.md")])
    );

    // The untouched task never needed a newer release: it keeps its fork
    // point's configuration, and its publications never list it.
    std::fs::write(plain_task.join("notes.md"), "notes\n").unwrap();
    let untouched = json(&workspace_env(
        &plain_task,
        ["publish", "-m", "Publish notes"],
        &later,
    ));
    assert!(untouched.get("repository_requirement").is_none());
    assert_eq!(
        untouched["changed_paths"],
        json!(["20260918-194500-untouched/notes.md"])
    );
    assert_eq!(
        branch_config(untouched["remote_oid"].as_str().unwrap()),
        plain
    );
    assert!(!changed_against_main(&fixture, "codex/untouched").contains(CONFIG));

    // The user merges both without a conflict, and main keeps 0.5.0.
    merge_into_main(&fixture, "codex/delta");
    merge_into_main(&fixture, "codex/untouched");
    assert_eq!(read(&fixture.seed.join(CONFIG)), declaring("0.5.0", &plain));
}

/// Resetting an approval and publishing withdraws the task branch's raise,
/// so its pull request no longer changes the repository's configuration.
#[cfg(feature = "test-storage")]
#[test]
fn a_withdrawn_raise_leaves_the_shared_configuration_untouched() {
    let fixture = managed_fixture();
    let plain = read(&fixture.shared.join(CONFIG));
    let (epsilon_id, epsilon) = create_task(&fixture, "epsilon", "20260918-195000");
    let current = [(CLI_VERSION_ENV, "0.4.0")];
    approve(
        &epsilon,
        &current,
        "2GiB",
        "The user approved 2 GiB for epsilon",
    );
    let raised = json(&workspace_env(
        &epsilon,
        ["publish", "-m", "Publish the approval"],
        &current,
    ));
    assert_eq!(raised["repository_requirement"]["change"], "raise");
    assert!(changed_against_main(&fixture, "codex/epsilon").contains(CONFIG));

    // Main moves on meanwhile.
    std::fs::write(fixture.seed.join("later.md"), "later\n").unwrap();
    fixture.commit_seed("Unrelated change");

    // The user withdraws the approval.
    let reset = approve(
        &epsilon,
        &current,
        "1GiB",
        "The user kept the default limit",
    );
    assert_eq!(reset["status"], "recorded");
    assert_eq!(reset["schema_version"], 2);
    let withdrawn = json!({
        "path": CONFIG,
        "change": "withdraw",
        "minimum_cli_version": null,
        "previous_minimum_cli_version": "0.4.0",
        "task_manifest_schema": null,
    });
    let plan = json(&workspace_env(&epsilon, ["plan"], &current));
    assert_eq!(plan["repository_requirement"], withdrawn);
    assert_eq!(
        plan["changed_paths"],
        json!([CONFIG, format!("{epsilon_id}/{MANIFEST}")])
    );
    let published = json(&workspace_env(
        &epsilon,
        ["publish", "-m", "Withdraw the approval"],
        &current,
    ));
    assert_eq!(published["repository_requirement"], withdrawn);
    let tip = published["remote_oid"].as_str().unwrap();
    assert_eq!(
        commit_message(&fixture.remote, tip),
        format!(
            "Withdraw the approval\n\nWorkspace-Task: {epsilon_id}\nWorkspace-Scope: {epsilon_id}\nWorkspace-Requirement: minimum_cli_version removed (withdraws this branch's raise to 0.4.0; no task manifest in this publication needs it)\n\n"
        )
    );
    assert_eq!(show(&fixture.remote, &format!("{tip}:{CONFIG}")), plain);
    assert!(!changed_against_main(&fixture, "codex/epsilon").contains(CONFIG));

    // Any build continues the task, and merging it does not raise main.
    let older = [(CLI_VERSION_ENV, "0.3.0")];
    assert_eq!(
        json(&workspace_env(&epsilon, ["plan"], &older))["status"],
        "no_changes"
    );
    merge_into_main(&fixture, "codex/epsilon");
    assert_eq!(read(&fixture.seed.join(CONFIG)), plain);
    assert_eq!(
        json(&workspace_env(&fixture.shared, ["refresh"], &older))["status"],
        "updated"
    );
    assert_eq!(read(&fixture.shared.join(CONFIG)), plain);
}

/// Replays the published task branch onto the shared branch the way a
/// hosting provider's "rebase and merge" does, failing on any conflict.
#[cfg(feature = "test-storage")]
fn rebase_merge_into_main(fixture: &GitFixture, branch: &str) {
    git(&fixture.seed, ["fetch", "-q", "origin"]);
    git(
        &fixture.seed,
        [
            "checkout",
            "-q",
            "-B",
            "rebased",
            &format!("origin/{branch}"),
        ],
    );
    git(
        &fixture.seed,
        ["rebase", "-q", "--force-rebase", "origin/main"],
    );
    git(&fixture.seed, ["checkout", "-q", "main"]);
    git(&fixture.seed, ["merge", "-q", "--ff-only", "rebased"]);
    git(&fixture.seed, ["push", "-q", "origin", "main"]);
}

/// A task branch that keeps publishing after a rebase merge still starts
/// from its original fork point. Withdrawing its raise must not lower the
/// declaration the shared branch already carries, or replaying the
/// withdrawal would drop the requirement of other merged approvals.
#[cfg(feature = "test-storage")]
#[test]
fn a_withdrawal_after_a_rebase_merge_keeps_the_shared_declaration() {
    let fixture = managed_fixture();
    let plain = read(&fixture.shared.join(CONFIG));
    let current = [(CLI_VERSION_ENV, "0.4.0")];
    let (alpha_id, alpha) = create_task(&fixture, "alpha", "20260918-200000");
    let (beta_id, beta) = create_task(&fixture, "beta", "20260918-200500");
    approve(
        &alpha,
        &current,
        "2GiB",
        "The user approved 2 GiB for alpha",
    );
    let raised = json(&workspace_env(
        &alpha,
        ["publish", "-m", "Publish the approval"],
        &current,
    ));
    assert_eq!(raised["repository_requirement"]["change"], "raise");
    rebase_merge_into_main(&fixture, "codex/alpha");
    assert_eq!(read(&fixture.seed.join(CONFIG)), declaring("0.4.0", &plain));

    // Another approval merges meanwhile, so main keeps needing 0.4.0. Its
    // first publication starts from main, which already declares it.
    approve(&beta, &current, "2GiB", "The user approved 2 GiB for beta");
    let beta_published = json(&workspace_env(
        &beta,
        ["publish", "-m", "Publish the approval"],
        &current,
    ));
    assert_eq!(beta_published["status"], "pushed");
    assert!(beta_published.get("repository_requirement").is_none());
    merge_into_main(&fixture, "codex/beta");

    // The user withdraws alpha's approval while its branch keeps publishing.
    let reset = approve(&alpha, &current, "1GiB", "The user kept the default limit");
    assert_eq!(reset["status"], "recorded");
    let plan = json(&workspace_env(&alpha, ["plan"], &current));
    assert!(plan.get("repository_requirement").is_none(), "{plan}");
    assert_eq!(
        plan["changed_paths"],
        json!([format!("{alpha_id}/{MANIFEST}")])
    );
    let withdrawn = json(&workspace_env(
        &alpha,
        ["publish", "-m", "Withdraw the approval"],
        &current,
    ));
    assert!(
        withdrawn.get("repository_requirement").is_none(),
        "{withdrawn}"
    );
    let tip = withdrawn["remote_oid"].as_str().unwrap();
    assert_eq!(
        show(&fixture.remote, &format!("{tip}:{CONFIG}")),
        declaring("0.4.0", &plain)
    );
    assert!(!commit_message(&fixture.remote, tip).contains("Workspace-Requirement"));

    // Replaying the withdrawal keeps main's declaration for beta's manifest.
    rebase_merge_into_main(&fixture, "codex/alpha");
    assert_eq!(read(&fixture.seed.join(CONFIG)), declaring("0.4.0", &plain));
    assert!(read(&fixture.seed.join(&beta_id).join(MANIFEST)).starts_with("schema_version = 3\n"));
    assert!(read(&fixture.seed.join(&alpha_id).join(MANIFEST)).starts_with("schema_version = 2\n"));
}

/// A user-authorized change of only the configuration's comments or
/// formatting is the user's content, so later publications that leave the
/// configuration outside their scopes keep it.
#[test]
fn an_authorized_comment_in_the_configuration_is_never_reverted() {
    let fixture = managed_fixture();
    let config_path = fixture.shared.join(CONFIG);
    let plain = read(&config_path);
    let (task_id, task) = create_task(&fixture, "ownership", "20260918-201000");
    workspace(&task, ["publish", "-m", "Publish scaffold"]);
    let commented = format!("# Owned by the data team.\n{plain}");
    std::fs::write(&config_path, &commented).unwrap();
    let authorized = json(&workspace(
        &task,
        [
            "publish",
            "-m",
            "Document ownership",
            "--include",
            CONFIG,
            "--scope-note",
            "The user asked to document the configuration's owner",
        ],
    ));
    assert_eq!(authorized["changed_paths"], json!([CONFIG]));
    assert!(authorized.get("repository_requirement").is_none());

    // The shared checkout keeps its own copy, and the next publication
    // leaves the configuration outside its scopes.
    std::fs::write(&config_path, &plain).unwrap();
    std::fs::write(task.join("notes.md"), "notes\n").unwrap();
    let later = json(&workspace(&task, ["publish", "-m", "Publish notes"]));
    assert_eq!(
        later["changed_paths"],
        json!([format!("{task_id}/notes.md")])
    );
    assert!(later.get("repository_requirement").is_none(), "{later}");
    let tip = later["remote_oid"].as_str().unwrap();
    assert_eq!(show(&fixture.remote, &format!("{tip}:{CONFIG}")), commented);
}

/// A task branch whose configuration carries a user-authorized change
/// follows a shared branch that a later release raised further, even when
/// its next publication leaves the configuration outside its scopes, so the
/// user's merge stays clean.
#[cfg(feature = "test-storage")]
#[test]
fn an_authorized_configuration_follows_a_further_raised_shared_branch() {
    if which::which("dvc").is_err() {
        eprintln!("skipping: dvc is unavailable");
        return;
    }
    let fixture = GitFixture::new();
    workspace(
        &fixture.seed,
        [
            "init",
            "--s3-url",
            fixture.root.join("storage-remote").to_str().unwrap(),
        ],
    );
    fixture.commit_seed("Initialize workspace");
    fixture.clone_shared();
    let config_path = fixture.shared.join(CONFIG);
    let original = read(&config_path);
    let current = [(CLI_VERSION_ENV, "0.4.0")];
    let (task_id, task) = create_task(&fixture, "relocation", "20260918-202000");
    approve(
        &task,
        &current,
        "2GiB",
        "The user approved 2 GiB for the relocation",
    );
    // The user authorized this task to relocate the repository's storage.
    workspace(
        &fixture.shared,
        [
            "init",
            "--s3-url",
            fixture.root.join("relocated-storage").to_str().unwrap(),
        ],
    );
    let relocated = read(&config_path);
    assert_ne!(relocated, original);
    let authorized = json(&workspace_env(
        &task,
        [
            "publish",
            "-m",
            "Relocate storage",
            "--include",
            CONFIG,
            "--scope-note",
            "The user asked to relocate storage",
        ],
        &current,
    ));
    assert_eq!(authorized["repository_requirement"]["change"], "raise");
    let tip = authorized["remote_oid"].as_str().unwrap();
    assert_eq!(
        show(&fixture.remote, &format!("{tip}:{CONFIG}")),
        declaring("0.4.0", &relocated)
    );

    // A later release raises the shared branch further for its own schema.
    std::fs::write(fixture.seed.join(CONFIG), declaring("0.5.0", &original)).unwrap();
    fixture.commit_seed("Adopt a newer task manifest schema");

    // The next publication leaves the configuration outside its scopes and
    // follows main's declaration while keeping the relocation.
    git(&fixture.shared, ["checkout", "--", "."]);
    std::fs::write(task.join("notes.md"), "notes\n").unwrap();
    let later = [(CLI_VERSION_ENV, "0.5.0")];
    let follow = json!({
        "path": CONFIG,
        "change": "follow",
        "minimum_cli_version": "0.5.0",
        "previous_minimum_cli_version": "0.4.0",
        "task_manifest_schema": 3,
    });
    let published = json(&workspace_env(
        &task,
        ["publish", "-m", "Publish notes"],
        &later,
    ));
    assert_eq!(published["repository_requirement"], follow);
    assert_eq!(
        published["changed_paths"],
        json!([CONFIG, format!("{task_id}/notes.md")])
    );
    let tip = published["remote_oid"].as_str().unwrap();
    assert_eq!(
        show(&fixture.remote, &format!("{tip}:{CONFIG}")),
        declaring("0.5.0", &relocated)
    );
    assert!(commit_message(&fixture.remote, tip).contains(
        "\nWorkspace-Requirement: minimum_cli_version=0.5.0 (task manifest schema 3; follows origin/main)\n"
    ));

    // The user's merge is clean and keeps both the relocation and 0.5.0.
    merge_into_main(&fixture, "codex/relocation");
    assert_eq!(
        read(&fixture.seed.join(CONFIG)),
        declaring("0.5.0", &relocated)
    );
}
