mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Output;

use common::{
    GitFixture, binary_command, git, json, workspace, workspace_env, workspace_unchecked,
};
use serde_json::{Value, json as value};

const MANIFEST: &str = ".workspace-mgr-task.toml";
const INFRA_MANIFEST: &str = ".workspace-mgr-infrastructure.toml";
const FIRST: &str = "20260712-120000-analysis";
const SECOND: &str = "20260812-120000-inputs";

fn write_task(repo: &Path, path: &str, id: &str, slug: &str) -> PathBuf {
    let directory = repo.join(path);
    std::fs::create_dir_all(&directory).unwrap();
    let original_slug = id.splitn(3, '-').nth(2).unwrap();
    std::fs::write(
        directory.join(MANIFEST),
        format!(
            "schema_version = 2\nkind = \"deliverable\"\nid = \"{id}\"\nslug = \"{slug}\"\npath = \"{path}\"\nbranch = \"codex/{original_slug}\"\ntitle = \"Catalog {slug} report\"\npurpose = \"Resolve current task locations without mutation\"\nadditional_scopes = []\n"
        ),
    )
    .unwrap();
    std::fs::write(directory.join("README.md"), "# Task\n").unwrap();
    directory
}

fn write_infrastructure(path: &Path, slug: &str) -> PathBuf {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        format!(
            "schema_version = 2\nkind = \"infrastructure\"\nid = \"infra-{slug}\"\nslug = \"{slug}\"\nbranch = \"codex/infra-{slug}\"\ntitle = \"Catalog infrastructure {slug}\"\npurpose = \"Keep the repository discoverable\"\n\n[[additional_scopes]]\npath = \"README.md\"\nreason = \"The user requested this scope\"\n"
        ),
    )
    .unwrap();
    path.to_path_buf()
}

fn private_infrastructure(repo: &Path, slug: &str) -> PathBuf {
    write_infrastructure(
        &repo.join(format!(
            ".workspace-mgr/local/infrastructure-tasks/infra-{slug}/{INFRA_MANIFEST}"
        )),
        slug,
    )
}

fn tasks(output: &Value) -> &[Value] {
    output["tasks"].as_array().unwrap()
}

fn task_by_path<'a>(output: &'a Value, path: &str) -> &'a Value {
    tasks(output)
        .iter()
        .find(|task| task["path"] == path)
        .unwrap_or_else(|| panic!("missing {path:?} in {output}"))
}

fn error_text(output: &Output) -> String {
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .map(|entry| entry.unwrap())
        .map(|entry| {
            let path = entry.path().strip_prefix(root).unwrap().to_path_buf();
            let content = if entry.file_type().is_file() {
                std::fs::read(entry.path()).unwrap()
            } else if entry.file_type().is_symlink() {
                std::fs::read_link(entry.path())
                    .unwrap()
                    .to_string_lossy()
                    .as_bytes()
                    .to_vec()
            } else {
                Vec::new()
            };
            (path, content)
        })
        .collect()
}

#[test]
fn empty_repository_has_a_machine_readable_empty_catalog_without_creating_state() {
    let fixture = GitFixture::new();
    let before = snapshot(&fixture.seed);
    let catalog = json(&workspace(&fixture.seed, ["task", "list"]));
    assert_eq!(
        catalog["repo"].as_str(),
        fixture.seed.canonicalize().unwrap().to_str()
    );
    assert!(tasks(&catalog).is_empty());
    assert_eq!(catalog["warnings"], value!([]));
    assert_eq!(
        json(&workspace(&fixture.seed, ["task", "list", "--paths"])),
        value!([])
    );
    assert_eq!(before, snapshot(&fixture.seed));
    assert!(!fixture.seed.join(".workspace-mgr/local").exists());
}

#[test]
fn catalog_discovers_current_task_roots_at_arbitrary_depth_and_stops_at_payload() {
    let fixture = GitFixture::new();
    write_task(&fixture.seed, FIRST, FIRST, "analysis");
    let nested = format!("finished/research/2026/08/{SECOND}");
    write_task(&fixture.seed, &nested, SECOND, "inputs");
    let embedded = format!("{FIRST}/payload/20260912-120000-looking-like-a-task");
    std::fs::create_dir_all(fixture.seed.join(embedded)).unwrap();
    std::fs::create_dir_all(fixture.seed.join("ordinary-data")).unwrap();
    std::fs::write(fixture.seed.join("ordinary-data/input.txt"), "input\n").unwrap();

    let catalog = json(&workspace(&fixture.seed, ["task", "list"]));
    assert_eq!(tasks(&catalog).len(), 2);
    let first = task_by_path(&catalog, FIRST);
    assert_eq!(first["placement"], "top-level");
    assert_eq!(first["metadata"], "managed");
    assert_eq!(first["kind"], "deliverable");
    assert_eq!(first["id"], FIRST);
    assert_eq!(first["name"], FIRST);
    assert_eq!(first["slug"], "analysis");
    assert_eq!(first["branch"], "codex/analysis");
    assert_eq!(first["title"], "Catalog analysis report");
    assert_eq!(
        first["purpose"],
        "Resolve current task locations without mutation"
    );
    assert_eq!(first["scopes"], value!([FIRST]));
    assert!(first["archive_status"].is_null());
    assert!(first["diagnostic"].is_null());
    assert_eq!(
        first["manifest"].as_str(),
        fixture
            .seed
            .join(FIRST)
            .join(MANIFEST)
            .canonicalize()
            .unwrap()
            .to_str()
    );
    assert_eq!(task_by_path(&catalog, &nested)["placement"], "nested");
}

#[test]
fn renamed_task_resolves_by_stable_id_and_each_exact_current_selector() {
    let fixture = GitFixture::new();
    let current_name = "20260712-120000-final-report";
    let current_path = format!("2026/07/{current_name}");
    let directory = write_task(&fixture.seed, &current_path, FIRST, "final-report");
    let absolute = directory.canonicalize().unwrap();
    let catalog = json(&workspace(&fixture.seed, ["task", "list"]));
    let task = task_by_path(&catalog, &current_path);
    assert_eq!(task["id"], FIRST);
    assert_eq!(task["name"], current_name);
    assert_eq!(task["slug"], "final-report");
    for selector in [
        FIRST,
        current_name,
        "final-report",
        &current_path,
        absolute.to_str().unwrap(),
    ] {
        let resolved = json(&workspace(
            &fixture.seed,
            ["task", "path", selector, "--relative"],
        ));
        assert_eq!(resolved["id"], FIRST);
        assert_eq!(resolved["path"], current_path);
        assert_eq!(
            resolved["repo"].as_str(),
            fixture.seed.canonicalize().unwrap().to_str()
        );
    }
    let default = json(&workspace(&fixture.seed, ["task", "path", FIRST]));
    assert_eq!(default["path"].as_str(), absolute.to_str());
}

#[test]
fn path_stdout_is_one_shell_usable_line_even_when_archive_parents_have_spaces() {
    let fixture = GitFixture::new();
    let relative = format!("Completed work/2026/07/{FIRST}");
    let directory = write_task(&fixture.seed, &relative, FIRST, "analysis");
    std::fs::create_dir(directory.join("tools")).unwrap();
    let absolute = directory.canonicalize().unwrap();
    let output = workspace_env(
        &directory.join("tools"),
        ["task", "path", "analysis"],
        &[("WORKSPACE_MGR_FORMAT", "human")],
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{}\n", absolute.display())
    );
    assert!(output.stderr.is_empty());
    let relative_output = workspace_env(
        &directory.join("tools"),
        ["task", "path", FIRST, "--relative"],
        &[("WORKSPACE_MGR_FORMAT", "human")],
    );
    assert_eq!(
        String::from_utf8(relative_output.stdout).unwrap(),
        format!("{relative}\n")
    );
}

#[test]
fn explicit_repository_can_be_selected_from_outside_its_checkout() {
    let fixture = GitFixture::new();
    write_task(&fixture.seed, FIRST, FIRST, "analysis");
    let repository = fixture.seed.to_str().unwrap();
    let listed = json(&workspace(
        &fixture.root,
        ["task", "list", "--repo", repository],
    ));
    assert_eq!(tasks(&listed).len(), 1);
    let shown = json(&workspace(
        &fixture.root,
        ["task", "show", FIRST, "--repo", repository],
    ));
    assert_eq!(shown["task"], tasks(&listed)[0]);
    assert_eq!(shown["repo"], listed["repo"]);
}

#[test]
fn human_list_is_compact_and_show_retains_structured_task_details() {
    let fixture = GitFixture::new();
    write_task(&fixture.seed, FIRST, FIRST, "analysis");
    private_infrastructure(&fixture.seed, "cleanup");
    let listed = workspace_env(
        &fixture.seed,
        ["task", "list"],
        &[("WORKSPACE_MGR_FORMAT", "human")],
    );
    let table = String::from_utf8(listed.stdout).unwrap();
    assert!(table.contains(FIRST), "{table}");
    assert!(table.contains("infra-cleanup"), "{table}");
    assert!(table.lines().count() <= 5, "{table}");
    let shown = workspace_env(
        &fixture.seed,
        ["task", "show", FIRST],
        &[("WORKSPACE_MGR_FORMAT", "human")],
    );
    let report: serde_yaml::Value = serde_yaml::from_slice(&shown.stdout).unwrap();
    assert_eq!(report["task"]["id"].as_str(), Some(FIRST));
    assert_eq!(report["task"]["branch"].as_str(), Some("codex/analysis"));
    assert_eq!(report["task"]["path"].as_str(), Some(FIRST));
}

#[test]
fn list_filters_are_case_insensitive_and_combine_with_kind_and_placement() {
    let fixture = GitFixture::new();
    write_task(&fixture.seed, FIRST, FIRST, "analysis");
    let nested = format!("finished/{SECOND}");
    write_task(&fixture.seed, &nested, SECOND, "inputs");
    private_infrastructure(&fixture.seed, "cleanup");

    let all = json(&workspace(&fixture.seed, ["task", "list"]));
    assert_eq!(tasks(&all).len(), 3);
    for query in ["ANALYSIS", "CATALOG ANALYSIS", "20260712", FIRST] {
        let output = json(&workspace(&fixture.seed, ["task", "list", query]));
        assert_eq!(tasks(&output).len(), 1, "query {query}");
        assert_eq!(tasks(&output)[0]["path"], FIRST);
    }
    let nested_only = json(&workspace(
        &fixture.seed,
        [
            "task",
            "list",
            "FINISHED",
            "--kind",
            "deliverable",
            "--placement",
            "nested",
        ],
    ));
    assert_eq!(tasks(&nested_only).len(), 1);
    assert_eq!(tasks(&nested_only)[0]["path"], nested);
    let none = json(&workspace(
        &fixture.seed,
        ["task", "list", "inputs", "--placement", "top-level"],
    ));
    assert!(tasks(&none).is_empty());
    let infrastructure = json(&workspace(
        &fixture.seed,
        [
            "task",
            "list",
            "--kind",
            "infrastructure",
            "--placement",
            "repository",
        ],
    ));
    assert_eq!(tasks(&infrastructure).len(), 1);
    assert_eq!(tasks(&infrastructure)[0]["id"], "infra-cleanup");
}

#[test]
fn list_paths_prints_deliverable_locations_and_skips_infrastructure() {
    let fixture = GitFixture::new();
    write_task(&fixture.seed, FIRST, FIRST, "analysis");
    let nested = format!("2026/08/{SECOND}");
    write_task(&fixture.seed, &nested, SECOND, "inputs");
    private_infrastructure(&fixture.seed, "cleanup");
    let mut actual: Vec<String> = json(&workspace(&fixture.seed, ["task", "list", "--paths"]))
        .as_array()
        .unwrap()
        .iter()
        .map(|path| path.as_str().unwrap().to_owned())
        .collect();
    actual.sort();
    let mut expected = vec![FIRST.to_owned(), nested.clone()];
    expected.sort();
    assert_eq!(actual, expected);
    let human = workspace_env(
        &fixture.seed,
        ["task", "list", "--paths", "--placement", "nested"],
        &[("WORKSPACE_MGR_FORMAT", "human")],
    );
    assert_eq!(
        String::from_utf8(human.stdout).unwrap(),
        format!("{nested}\n")
    );
    assert_eq!(
        json(&workspace(
            &fixture.seed,
            ["task", "list", "--paths", "--kind", "infrastructure"]
        )),
        value!([])
    );
}

#[test]
fn manifestless_legacy_tasks_remain_discoverable_at_root_and_inside_archives() {
    let fixture = GitFixture::new();
    let nested = format!("prior-work/2026/08/{SECOND}");
    for path in [FIRST, &nested] {
        std::fs::create_dir_all(fixture.seed.join(path)).unwrap();
        std::fs::write(
            fixture.seed.join(path).join("notes.md"),
            "legacy evidence\n",
        )
        .unwrap();
    }
    let catalog = json(&workspace(&fixture.seed, ["task", "list"]));
    assert_eq!(tasks(&catalog).len(), 2);
    for path in [FIRST, &nested] {
        let task = task_by_path(&catalog, path);
        assert_eq!(task["metadata"], "legacy");
        assert!(task["manifest"].is_null());
        assert!(task["branch"].is_null());
        assert!(task["title"].is_null());
        assert!(task["purpose"].is_null());
        assert!(task["archive_status"].is_null());
    }
    let resolved = json(&workspace(
        &fixture.seed,
        ["task", "path", "inputs", "--relative"],
    ));
    assert_eq!(resolved["path"], nested);
}

#[test]
fn malformed_manifests_are_reported_without_hiding_other_tasks_or_becoming_legacy() {
    let fixture = GitFixture::new();
    let bad = write_task(&fixture.seed, FIRST, FIRST, "analysis");
    std::fs::write(bad.join(MANIFEST), "not [valid TOML\n").unwrap();
    write_task(&fixture.seed, SECOND, SECOND, "inputs");
    let catalog = json(&workspace(&fixture.seed, ["task", "list"]));
    assert_eq!(tasks(&catalog).len(), 2);
    let invalid = task_by_path(&catalog, FIRST);
    assert_eq!(invalid["metadata"], "invalid");
    assert!(invalid["id"].is_null());
    assert!(invalid["slug"].is_null());
    assert!(!invalid["diagnostic"].as_str().unwrap().is_empty());
    let warnings = catalog["warnings"].as_array().unwrap();
    assert!(warnings.iter().any(|warning| {
        !warning["path"].as_str().unwrap().is_empty()
            && !warning["message"].as_str().unwrap().is_empty()
    }));
    let show_error = error_text(&workspace_unchecked(&fixture.seed, ["task", "show", FIRST]));
    assert!(show_error.contains(FIRST), "{show_error}");
    let path_error = error_text(&workspace_unchecked(&fixture.seed, ["task", "path", FIRST]));
    assert!(path_error.contains(FIRST), "{path_error}");
    let paths_error = error_text(&workspace_unchecked(
        &fixture.seed,
        ["task", "list", "--paths"],
    ));
    assert!(paths_error.contains(FIRST), "{paths_error}");
    assert_eq!(
        json(&workspace(
            &fixture.seed,
            ["task", "list", "inputs", "--paths"]
        )),
        value!([SECOND])
    );
}

#[test]
fn path_mismatches_in_current_metadata_are_invalid_instead_of_redirecting_lookup() {
    let fixture = GitFixture::new();
    let first = write_task(&fixture.seed, FIRST, FIRST, "analysis");
    let original = std::fs::read_to_string(first.join(MANIFEST)).unwrap();
    std::fs::write(
        first.join(MANIFEST),
        original.replace(
            &format!("path = \"{FIRST}\""),
            &format!("path = \"other/{FIRST}\""),
        ),
    )
    .unwrap();
    let catalog = json(&workspace(&fixture.seed, ["task", "list"]));
    assert_eq!(task_by_path(&catalog, FIRST)["metadata"], "invalid");
    let error = error_text(&workspace_unchecked(&fixture.seed, ["task", "path", FIRST]));
    assert!(error.contains(FIRST), "{error}");
}

#[test]
fn paths_output_refuses_invalid_deliverables_with_unrepresentable_relative_locations() {
    let fixture = GitFixture::new();
    let declared = format!("bad-group/{FIRST}");
    let directory = write_task(&fixture.seed, &declared, FIRST, "analysis");
    let unrepresentable_parent = fixture.seed.join(" bad-group");
    std::fs::rename(directory.parent().unwrap(), &unrepresentable_parent).unwrap();

    let catalog = json(&workspace(&fixture.seed, ["task", "list"]));
    assert_eq!(tasks(&catalog).len(), 1);
    let task = &tasks(&catalog)[0];
    assert_eq!(task["metadata"], "invalid");
    assert!(task["path"].is_null());
    assert!(!task["diagnostic"].as_str().unwrap().is_empty());
    for format in ["json", "human"] {
        let output = common::workspace_env_unchecked(
            &fixture.seed,
            ["task", "list", "--paths"],
            &[("WORKSPACE_MGR_FORMAT", format)],
        );
        let error = error_text(&output);
        assert!(error.contains(FIRST), "{error}");
    }
}

#[cfg(unix)]
#[test]
fn unreadable_task_metadata_is_invalid_instead_of_becoming_a_legacy_candidate() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = GitFixture::new();
    let directory = write_task(&fixture.seed, FIRST, FIRST, "analysis");
    let permissions = std::fs::metadata(&directory).unwrap().permissions();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o000)).unwrap();
    let inaccessible = std::fs::symlink_metadata(directory.join(MANIFEST));
    match inaccessible {
        Ok(_) => {
            // A privileged test process can still read mode-000 directories.
            std::fs::set_permissions(&directory, permissions).unwrap();
            return;
        }
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {}
        Err(error) => {
            std::fs::set_permissions(&directory, permissions).unwrap();
            panic!("unexpected metadata access error: {error}");
        }
    }
    let listed = workspace_unchecked(&fixture.seed, ["task", "list"]);
    let resolved = workspace_unchecked(&fixture.seed, ["task", "path", FIRST]);
    let shown = workspace_unchecked(&fixture.seed, ["task", "show", FIRST]);
    let paths = workspace_unchecked(&fixture.seed, ["task", "list", "--paths"]);
    std::fs::set_permissions(&directory, permissions).unwrap();

    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let catalog = json(&listed);
    let task = task_by_path(&catalog, FIRST);
    assert_eq!(task["metadata"], "invalid");
    assert!(task["id"].is_null());
    assert!(!task["diagnostic"].as_str().unwrap().is_empty());
    assert!(!catalog["warnings"].as_array().unwrap().is_empty());
    for output in [&resolved, &shown, &paths] {
        let error = error_text(output);
        assert!(error.contains(FIRST), "{error}");
    }
}

#[test]
fn duplicate_ids_and_slugs_require_an_exact_location_selector() {
    let fixture = GitFixture::new();
    let first = format!("copy-one/{FIRST}");
    let second = format!("copy-two/{FIRST}");
    write_task(&fixture.seed, &first, FIRST, "analysis");
    write_task(&fixture.seed, &second, FIRST, "analysis");
    let catalog = json(&workspace(&fixture.seed, ["task", "list"]));
    assert_eq!(tasks(&catalog).len(), 2);
    for selector in [FIRST, "analysis"] {
        let error = error_text(&workspace_unchecked(
            &fixture.seed,
            ["task", "path", selector],
        ));
        assert!(error.contains(&first), "{error}");
        assert!(error.contains(&second), "{error}");
        let show_error = error_text(&workspace_unchecked(
            &fixture.seed,
            ["task", "show", selector],
        ));
        assert!(show_error.contains(&first), "{show_error}");
        assert!(show_error.contains(&second), "{show_error}");
    }
    let resolved = json(&workspace(
        &fixture.seed,
        ["task", "path", &second, "--relative"],
    ));
    assert_eq!(resolved["path"], second);

    let unique_id = "20260912-120000-analysis";
    write_task(&fixture.seed, unique_id, unique_id, "analysis");
    let unique = json(&workspace(
        &fixture.seed,
        ["task", "path", unique_id, "--relative"],
    ));
    assert_eq!(unique["id"], unique_id);
    assert_eq!(unique["path"], unique_id);
    let slug_error = error_text(&workspace_unchecked(
        &fixture.seed,
        ["task", "path", "analysis"],
    ));
    assert!(slug_error.contains(&first), "{slug_error}");
    assert!(slug_error.contains(&second), "{slug_error}");
    assert!(slug_error.contains(unique_id), "{slug_error}");
}

#[test]
fn resolver_uses_exact_selectors_and_refuses_missing_infrastructure_or_file_paths() {
    let fixture = GitFixture::new();
    write_task(&fixture.seed, FIRST, FIRST, "analysis");
    private_infrastructure(&fixture.seed, "cleanup");
    for selector in ["anal", "ANALYSIS", "absent", "README.md", "infra-cleanup"] {
        let error = error_text(&workspace_unchecked(
            &fixture.seed,
            ["task", "path", selector],
        ));
        assert!(!error.is_empty());
    }
    let infrastructure = json(&workspace(&fixture.seed, ["task", "show", "infra-cleanup"]));
    assert_eq!(infrastructure["task"]["kind"], "infrastructure");
    assert!(infrastructure["task"]["path"].is_null());
}

#[test]
fn receipt_status_is_a_local_observation_and_never_a_completion_inference() {
    let fixture = GitFixture::new();
    let archived = format!("2026/07/{FIRST}");
    let directory = write_task(&fixture.seed, &archived, FIRST, "analysis");
    write_task(&fixture.seed, SECOND, SECOND, "inputs");
    std::fs::write(
        directory.join(".workspace-mgr-archive.json"),
        serde_json::to_vec(&value!({
            "schema_version": 1,
            "task_id": FIRST,
            "source": FIRST,
            "destination": archived,
            "status": "copied",
            "versions": []
        }))
        .unwrap(),
    )
    .unwrap();
    let catalog = json(&workspace(&fixture.seed, ["task", "list"]));
    assert_eq!(
        task_by_path(&catalog, &archived)["archive_status"],
        "copied"
    );
    assert!(task_by_path(&catalog, SECOND)["archive_status"].is_null());
    assert!(
        !tasks(&catalog)
            .iter()
            .any(|task| task["archive_status"] == "merged")
    );
}

#[cfg(unix)]
#[test]
fn discovery_avoids_symlinks_and_nested_repositories_but_allows_cache_git_markers() {
    use std::os::unix::fs::symlink;

    let fixture = GitFixture::new();
    let known_task = write_task(&fixture.seed, FIRST, FIRST, "analysis");
    git(&known_task, ["init"]);
    let outside = fixture.root.join("outside");
    write_task(&outside, SECOND, SECOND, "inputs");
    symlink(&outside, fixture.seed.join("linked-content")).unwrap();
    let checkout = fixture.seed.join("external-checkout");
    std::fs::create_dir(&checkout).unwrap();
    git(&checkout, ["init"]);
    write_task(&checkout, SECOND, SECOND, "inputs");
    let timestamp_checkout = fixture.seed.join("20260912-120000-external-checkout");
    std::fs::create_dir(&timestamp_checkout).unwrap();
    git(&timestamp_checkout, ["init"]);
    std::fs::write(timestamp_checkout.join("input.txt"), "external data\n").unwrap();
    let cache = fixture.seed.join("ordinary-cache");
    std::fs::create_dir(&cache).unwrap();
    std::fs::write(cache.join(".git"), []).unwrap();
    write_task(&cache, SECOND, SECOND, "inputs");
    let catalog = json(&workspace(&fixture.seed, ["task", "list"]));
    assert_eq!(tasks(&catalog).len(), 2);
    assert_eq!(task_by_path(&catalog, FIRST)["metadata"], "managed");
    assert!(
        tasks(&catalog)
            .iter()
            .any(|task| task["path"] == format!("ordinary-cache/{SECOND}"))
    );
    assert!(!tasks(&catalog).iter().any(|task| {
        task["path"]
            .as_str()
            .unwrap()
            .starts_with("external-checkout/")
            || task["path"] == "20260912-120000-external-checkout"
            || task["path"]
                .as_str()
                .unwrap()
                .starts_with("linked-content/")
    }));
}

#[test]
fn ignored_tasks_are_visible_while_product_and_git_metadata_are_excluded() {
    let fixture = GitFixture::new();
    let ignored = format!("ignored/{FIRST}");
    write_task(&fixture.seed, &ignored, FIRST, "analysis");
    std::fs::write(fixture.seed.join(".gitignore"), "/ignored/\n").unwrap();
    for parent in [".git", ".dvc", ".workspace-mgr"] {
        let relative = format!("{parent}/{SECOND}");
        write_task(&fixture.seed, &relative, SECOND, "inputs");
    }
    let catalog = json(&workspace(&fixture.seed, ["task", "list"]));
    assert_eq!(tasks(&catalog).len(), 1);
    assert_eq!(tasks(&catalog)[0]["path"], ignored);
}

#[test]
fn queries_read_both_private_state_formats_without_network_updates_or_migration() {
    let fixture = GitFixture::new();
    write_task(&fixture.seed, FIRST, FIRST, "analysis");
    let modern = private_infrastructure(&fixture.seed, "modern");
    let legacy = write_infrastructure(&fixture.seed.join(".git/workspace-mgr/task.toml"), "legacy");
    std::fs::write(
        fixture.seed.join(".git/workspace-mgr/pending-retire.json"),
        "protected old state\n",
    )
    .unwrap();
    let before = snapshot(&fixture.seed);
    let cache = fixture
        .root
        .join("must-not-create-update-cache/update.json");
    let mut query = binary_command();
    query
        .args(["--format", "json", "task", "list"])
        .current_dir(&fixture.seed)
        .env_remove("WORKSPACE_MGR_UPDATE_CHECK_DISABLE")
        .env(
            "WORKSPACE_MGR_UPDATE_TEST_URL",
            "http://127.0.0.1:1/not-a-registry",
        )
        .env("WORKSPACE_MGR_UPDATE_TEST_CACHE", &cache);
    let output = query.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let catalog = json(&output);
    assert_eq!(tasks(&catalog).len(), 3);
    for (id, manifest) in [("infra-modern", &modern), ("infra-legacy", &legacy)] {
        let task = tasks(&catalog)
            .iter()
            .find(|task| task["id"] == id)
            .unwrap();
        assert_eq!(task["metadata"], "managed");
        assert_eq!(task["placement"], "repository");
        assert_eq!(task["scopes"], value!(["README.md"]));
        assert_eq!(
            task["manifest"].as_str(),
            manifest.canonicalize().unwrap().to_str()
        );
        assert!(task["path"].is_null());
        workspace(&fixture.seed, ["task", "show", id]);
    }
    workspace(&fixture.seed, ["task", "path", FIRST]);
    for args in [
        vec!["task", "list", "--help"],
        vec!["task", "path", "--help"],
        vec!["task", "list", "--not-a-real-option"],
    ] {
        let is_help = args.last() == Some(&"--help");
        let mut query = binary_command();
        query
            .args(&args)
            .current_dir(&fixture.seed)
            .env_remove("WORKSPACE_MGR_UPDATE_CHECK_DISABLE")
            .env(
                "WORKSPACE_MGR_UPDATE_TEST_URL",
                "http://127.0.0.1:1/not-a-registry",
            )
            .env("WORKSPACE_MGR_UPDATE_TEST_CACHE", &cache);
        let output = query.output().unwrap();
        if is_help {
            assert!(output.status.success());
            assert!(String::from_utf8_lossy(&output.stdout).contains("Usage:"));
        } else {
            assert_eq!(output.status.code(), Some(2));
        }
        assert!(!String::from_utf8_lossy(&output.stderr).contains("update available"));
    }
    assert!(!cache.exists());
    assert!(!cache.parent().unwrap().exists());
    assert_eq!(before, snapshot(&fixture.seed));
    assert!(legacy.is_file());
    assert!(
        !fixture
            .seed
            .join(".workspace-mgr/local/legacy-manifest-paths.json")
            .exists()
    );
}

#[test]
fn linked_worktrees_query_their_own_content_and_the_shared_primary_infrastructure() {
    let fixture = GitFixture::new();
    write_task(&fixture.seed, FIRST, FIRST, "analysis");
    git(&fixture.seed, ["add", FIRST]);
    git(
        &fixture.seed,
        ["commit", "-m", "Task shared by the linked checkout"],
    );
    let modern = private_infrastructure(&fixture.seed, "shared");
    let legacy = write_infrastructure(&fixture.seed.join(".git/workspace-mgr/task.toml"), "legacy");
    let linked = fixture.root.join("linked worktree");
    git(
        &fixture.seed,
        [
            "worktree",
            "add",
            "-b",
            "linked-catalog",
            linked.to_str().unwrap(),
        ],
    );
    write_task(&linked, SECOND, SECOND, "inputs");
    let before = snapshot(&fixture.seed);
    let linked_before = snapshot(&linked);
    let catalog = json(&workspace(&linked, ["task", "list"]));
    assert_eq!(
        catalog["repo"].as_str(),
        linked.canonicalize().unwrap().to_str()
    );
    assert_eq!(tasks(&catalog).len(), 4);
    assert_eq!(task_by_path(&catalog, SECOND)["metadata"], "managed");
    for manifest in [&modern, &legacy] {
        assert!(
            tasks(&catalog)
                .iter()
                .any(|task| task["manifest"].as_str() == manifest.canonicalize().unwrap().to_str())
        );
    }
    let primary = json(&workspace(&fixture.seed, ["task", "list"]));
    assert_eq!(tasks(&primary).len(), 3);
    assert!(!tasks(&primary).iter().any(|task| task["path"] == SECOND));
    assert_eq!(before, snapshot(&fixture.seed));
    assert_eq!(linked_before, snapshot(&linked));
    assert!(!linked.join(".workspace-mgr/local").exists());
}
