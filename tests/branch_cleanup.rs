#![cfg(all(feature = "test-storage", unix))]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use common::*;
use serde_json::{Value, json as value};

struct CleanupFixture {
    git: GitFixture,
    gh: PathBuf,
    database: PathBuf,
    query_log: PathBuf,
    requests: Value,
}

impl CleanupFixture {
    fn new() -> Self {
        let git = GitFixture::new();
        workspace(&git.seed, ["init"]);
        git.commit_seed("Initialize managed workspace");
        git.clone_shared();
        let gh = git.root.join("fake-gh");
        let database = git.root.join("gh-database.json");
        let query_log = git.root.join("gh-query-log.jsonl");
        let database_literal = serde_json::to_string(database.to_str().unwrap()).unwrap();
        let log_literal = serde_json::to_string(query_log.to_str().unwrap()).unwrap();
        std::fs::write(
            &gh,
            format!(
                "#!/usr/bin/env python3\n\
             import json, pathlib, subprocess, sys\n\
             from urllib.parse import unquote\n\
             path = pathlib.Path({database_literal})\n\
             database = json.loads(path.read_text())\n\
             with open({log_literal}, 'a') as log:\n\
             \x20   log.write(json.dumps(sys.argv[1:]) + '\\n')\n\
             if sys.argv[1] == 'api':\n\
             \x20   branch = unquote(sys.argv[2].split('/branches/', 1)[1])\n\
             \x20   if database.get('mode') == 'protection-unknown':\n\
             \x20       print('{{}}')\n\
             \x20       sys.exit(0)\n\
             \x20   print(json.dumps({{'protected': database['protected'].get(branch, False)}}))\n\
             \x20   sys.exit(0)\n\
             if database.get('mode') == 'query-failure':\n\
             \x20   print('fixture GitHub query unavailable', file=sys.stderr)\n\
             \x20   sys.exit(1)\n\
             if database.get('mode') == 'invalid-json':\n\
             \x20   print('this is not JSON')\n\
             \x20   sys.exit(0)\n\
             head = sys.argv[sys.argv.index('--head') + 1]\n\
             mutation = database.get('mutation')\n\
             if mutation and mutation['branch'] == head:\n\
             \x20   subprocess.run(['git', '-C', mutation['repo'], 'update-ref',\n\
             \x20       'refs/heads/' + head, mutation['new'], mutation['old']], check=True)\n\
             \x20   database.pop('mutation')\n\
             \x20   path.write_text(json.dumps(database))\n\
             requests = database['prs'].get(head, [])\n\
             if '--base' in sys.argv:\n\
             \x20   base = sys.argv[sys.argv.index('--base') + 1]\n\
             \x20   requests = [pr for pr in requests if pr['baseRefName'] == base]\n\
             print(json.dumps(requests))\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let fixture = Self {
            git,
            gh,
            database,
            query_log,
            requests: value!({"prs": {}, "protected": {}}),
        };
        fixture.save();
        fixture
    }

    fn save(&self) {
        std::fs::write(&self.database, serde_json::to_vec(&self.requests).unwrap()).unwrap();
    }

    fn create_branch(&self, branch: &str) -> String {
        git(&self.git.seed, ["switch", "main"]);
        git(&self.git.seed, ["switch", "-c", branch]);
        let path = format!("{}.txt", branch.replace('/', "-"));
        std::fs::write(
            self.git.seed.join(&path),
            format!("{branch} retained work\n"),
        )
        .unwrap();
        git(&self.git.seed, ["add", &path]);
        git(
            &self.git.seed,
            ["commit", "-m", &format!("Publish {branch}")],
        );
        let head = oid(&self.git.seed, "HEAD");
        git(&self.git.seed, ["push", "origin", branch]);
        git(&self.git.seed, ["switch", "main"]);
        let tracking = format!("{branch}:refs/remotes/origin/{branch}");
        git(&self.git.shared, ["fetch", "origin", &tracking]);
        git(
            &self.git.shared,
            ["branch", branch, &format!("origin/{branch}")],
        );
        head
    }

    fn squash_merge(&mut self, branch: &str, head: &str) -> String {
        git(&self.git.seed, ["switch", "main"]);
        git(&self.git.seed, ["merge", "--squash", branch]);
        git(
            &self.git.seed,
            ["commit", "-m", &format!("Squash merged PR for {branch}")],
        );
        let merged = oid(&self.git.seed, "HEAD");
        git(&self.git.seed, ["push", "origin", "main"]);
        self.requests["prs"][branch] = value!([pull_request(branch, head, &merged)]);
        self.save();
        merged
    }

    fn refresh(&self, dry_run: bool) -> Value {
        let args = if dry_run {
            vec!["refresh", "--dry-run"]
        } else {
            vec!["refresh"]
        };
        json(&workspace_env(
            &self.git.shared,
            args,
            &[("WORKSPACE_MGR_TEST_GH", self.gh.to_str().unwrap())],
        ))
    }

    fn synchronize_with_git(&self) {
        git(&self.git.shared, ["fetch", "origin", "main"]);
        git(&self.git.shared, ["reset", "--hard", "origin/main"]);
    }

    fn assert_present(&self, branch: &str, head: &str) {
        assert_eq!(oid(&self.git.shared, &format!("refs/heads/{branch}")), head);
        assert_eq!(oid(&self.git.remote, &format!("refs/heads/{branch}")), head);
    }

    fn assert_deleted(&self, branch: &str) {
        for repo in [&self.git.shared, &self.git.remote] {
            assert!(
                !git_unchecked(
                    repo,
                    ["show-ref", "--verify", &format!("refs/heads/{branch}")]
                )
                .status
                .success(),
                "branch {branch} must be deleted from {}",
                repo.display()
            );
        }
        assert!(
            !git_unchecked(
                &self.git.shared,
                [
                    "show-ref",
                    "--verify",
                    &format!("refs/remotes/origin/{branch}")
                ]
            )
            .status
            .success()
        );
    }
}

fn oid(repo: &Path, reference: &str) -> String {
    String::from_utf8(git(repo, ["rev-parse", "--verify", reference]).stdout)
        .unwrap()
        .trim()
        .to_owned()
}

fn pull_request(branch: &str, head: &str, merged: &str) -> Value {
    value!({
        "number": 17, "url": "https://example.invalid/owner/archive-fixture/pull/17",
        "state": "MERGED", "mergedAt": "2026-07-12T20:00:00Z",
        "mergeCommit": {"oid": merged}, "headRefName": branch, "headRefOid": head,
        "baseRefName": "main", "isCrossRepository": false,
    })
}

fn contains_branch(report: &Value, field: &str, branch: &str) -> bool {
    report["branch_cleanup"][field]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["branch"] == branch)
}

#[test]
fn refresh_deletes_exact_squash_merged_local_and_remote_heads() {
    let mut fixture = CleanupFixture::new();
    let branch = "feature/completed";
    let head = fixture.create_branch(branch);
    let merged = fixture.squash_merge(branch, &head);
    assert_ne!(head, merged);
    assert_eq!(
        git_unchecked(
            &fixture.git.seed,
            ["merge-base", "--is-ancestor", &head, &merged]
        )
        .status
        .code(),
        Some(1)
    );
    let report = fixture.refresh(false);
    assert_eq!(report["new_oid"], merged);
    assert_eq!(oid(&fixture.git.shared, "main"), merged);
    assert_eq!(report["branch_cleanup"]["status"], "complete");
    assert_eq!(report["branch_cleanup"]["remote_writes"], true);
    let deleted = report["branch_cleanup"]["deleted"].as_array().unwrap();
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0]["branch"], branch);
    assert_eq!(deleted[0]["head_oid"], head);
    assert_eq!(deleted[0]["local"], true);
    assert_eq!(deleted[0]["remote"], true);
    fixture.assert_deleted(branch);
    assert!(
        fixture.git.shared.join("feature-completed.txt").is_file(),
        "branch cleanup does not remove merged retained content"
    );
}

#[test]
fn refresh_cleans_merged_refs_even_when_main_is_already_current() {
    let mut fixture = CleanupFixture::new();
    let branch = "codex/completed";
    let head = fixture.create_branch(branch);
    let merged = fixture.squash_merge(branch, &head);
    fixture.synchronize_with_git();
    let report = fixture.refresh(false);
    assert_eq!(report["old_oid"], merged);
    assert_eq!(report["new_oid"], merged);
    assert!(contains_branch(&report, "deleted", branch));
    fixture.assert_deleted(branch);
    let again = fixture.refresh(false);
    assert!(
        again["branch_cleanup"]["deleted"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(again["branch_cleanup"]["remote_writes"], false);
}

#[test]
fn refresh_dry_run_previews_cleanup_without_deleting_or_advancing_main() {
    let mut fixture = CleanupFixture::new();
    let branch = "codex/completed";
    let head = fixture.create_branch(branch);
    let old_main = oid(&fixture.git.shared, "main");
    fixture.squash_merge(branch, &head);
    let report = fixture.refresh(true);
    assert_eq!(report["branch_cleanup"]["status"], "dry_run");
    assert!(contains_branch(&report, "planned", branch));
    assert!(
        report["branch_cleanup"]["deleted"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(report["branch_cleanup"]["remote_writes"], false);
    assert_eq!(oid(&fixture.git.shared, "main"), old_main);
    fixture.assert_present(branch, &head);
}

#[test]
fn open_pr_and_resumed_local_or_remote_heads_are_preserved() {
    let mut fixture = CleanupFixture::new();
    let open = "codex/open";
    let open_head = fixture.create_branch(open);
    fixture.requests["prs"][open] = value!([{
        "number": 23, "url": "https://example.invalid/owner/archive-fixture/pull/23",
        "state": "OPEN", "mergedAt": null, "mergeCommit": null,
        "headRefName": open, "headRefOid": open_head, "baseRefName": "main",
        "isCrossRepository": false,
    }]);
    let local = "codex/resumed-local";
    let local_head = fixture.create_branch(local);
    fixture.squash_merge(local, &local_head);
    let remote = "codex/resumed-remote";
    let remote_head = fixture.create_branch(remote);
    fixture.squash_merge(remote, &remote_head);
    // New local commits are unpublished work, even if the original PR merged.
    git(&fixture.git.shared, ["switch", local]);
    std::fs::write(
        fixture.git.shared.join("local-resumed.txt"),
        "unpublished local work\n",
    )
    .unwrap();
    git(&fixture.git.shared, ["add", "local-resumed.txt"]);
    git(&fixture.git.shared, ["commit", "-m", "Resume local task"]);
    let changed_local = oid(&fixture.git.shared, "HEAD");
    git(&fixture.git.shared, ["switch", "main"]);
    // A remote branch reused after merge is equally ineligible for deletion.
    git(&fixture.git.seed, ["switch", remote]);
    std::fs::write(
        fixture.git.seed.join("remote-resumed.txt"),
        "new published task work\n",
    )
    .unwrap();
    git(&fixture.git.seed, ["add", "remote-resumed.txt"]);
    git(&fixture.git.seed, ["commit", "-m", "Resume remote task"]);
    let changed_remote = oid(&fixture.git.seed, "HEAD");
    git(&fixture.git.seed, ["push", "origin", remote]);
    git(&fixture.git.seed, ["switch", "main"]);
    fixture.save();
    let report = fixture.refresh(false);
    for branch in [open, local, remote] {
        assert!(!contains_branch(&report, "deleted", branch));
        assert!(contains_branch(&report, "skipped", branch));
    }
    fixture.assert_present(open, &open_head);
    assert_eq!(
        oid(&fixture.git.shared, &format!("refs/heads/{local}")),
        changed_local
    );
    assert_eq!(
        oid(&fixture.git.remote, &format!("refs/heads/{local}")),
        local_head
    );
    assert_eq!(
        oid(&fixture.git.shared, &format!("refs/heads/{remote}")),
        remote_head
    );
    assert_eq!(
        oid(&fixture.git.remote, &format!("refs/heads/{remote}")),
        changed_remote
    );
}

#[test]
fn fork_wrong_base_and_protected_branches_are_preserved() {
    let mut fixture = CleanupFixture::new();
    let mut heads = Vec::new();
    for branch in [
        "codex/fork",
        "codex/wrong-base",
        "feature/protected",
        "codex/open-other-base",
    ] {
        let head = fixture.create_branch(branch);
        fixture.squash_merge(branch, &head);
        heads.push((branch, head));
    }
    fixture.requests["prs"]["codex/fork"][0]["isCrossRepository"] = value!(true);
    fixture.requests["prs"]["codex/wrong-base"][0]["baseRefName"] = value!("release");
    fixture.requests["protected"]["feature/protected"] = value!(true);
    let mut open_other_base = fixture.requests["prs"]["codex/open-other-base"][0].clone();
    open_other_base["number"] = value!(24);
    open_other_base["state"] = value!("OPEN");
    open_other_base["baseRefName"] = value!("release");
    open_other_base["mergedAt"] = Value::Null;
    open_other_base["mergeCommit"] = Value::Null;
    fixture.requests["prs"]["codex/open-other-base"]
        .as_array_mut()
        .unwrap()
        .push(open_other_base);
    // The configured shared branch is also the currently checked-out branch.
    let main = oid(&fixture.git.seed, "main");
    fixture.requests["prs"]["main"] = value!([pull_request("main", &main, &main)]);
    fixture.save();
    let report = fixture.refresh(false);
    for (branch, head) in heads {
        fixture.assert_present(branch, &head);
        assert!(!contains_branch(&report, "deleted", branch));
        assert!(contains_branch(&report, "skipped", branch));
    }
    fixture.assert_present("main", &main);
    assert!(!contains_branch(&report, "deleted", "main"));
    let queries = std::fs::read_to_string(&fixture.query_log).unwrap();
    assert!(
        queries.contains("feature%2Fprotected"),
        "protection query URL-encodes branch names"
    );
}

#[test]
fn deleted_remote_branch_still_allows_exact_local_cleanup() {
    let mut fixture = CleanupFixture::new();
    let branch = "codex/completed";
    let head = fixture.create_branch(branch);
    fixture.squash_merge(branch, &head);
    git(&fixture.git.seed, ["push", "origin", "--delete", branch]);
    let report = fixture.refresh(false);
    assert!(contains_branch(&report, "deleted", branch));
    let row = report["branch_cleanup"]["deleted"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["branch"] == branch)
        .unwrap();
    assert_eq!(row["local"], true);
    assert_eq!(row["remote"], false);
    assert_eq!(report["branch_cleanup"]["remote_writes"], false);
    fixture.assert_deleted(branch);
}

#[test]
fn remote_only_merged_branch_is_cleaned_without_creating_local_ref() {
    let mut fixture = CleanupFixture::new();
    let branch = "feature/remote-only";
    let head = fixture.create_branch(branch);
    fixture.squash_merge(branch, &head);
    git(&fixture.git.shared, ["branch", "-D", branch]);
    let report = fixture.refresh(false);
    let row = report["branch_cleanup"]["deleted"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["branch"] == branch)
        .unwrap();
    assert_eq!(row["local"], false);
    assert_eq!(row["remote"], true);
    fixture.assert_deleted(branch);
}

#[test]
fn non_github_remote_preserves_existing_refresh_behavior() {
    let mut fixture = CleanupFixture::new();
    let branch = "codex/completed";
    let head = fixture.create_branch(branch);
    let merged = fixture.squash_merge(branch, &head);
    let report = json(&workspace(&fixture.git.shared, ["refresh"]));
    assert_eq!(oid(&fixture.git.shared, "main"), merged);
    assert_eq!(report["branch_cleanup"]["status"], "not_applicable");
    assert!(
        report["branch_cleanup"]["warnings"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    fixture.assert_present(branch, &head);
}

#[test]
fn github_verification_failures_warn_without_blocking_main_sync() {
    for mode in ["invalid-json", "query-failure", "protection-unknown"] {
        let mut fixture = CleanupFixture::new();
        let branch = "codex/completed";
        let head = fixture.create_branch(branch);
        let merged = fixture.squash_merge(branch, &head);
        fixture.requests["mode"] = value!(mode);
        fixture.save();
        let report = fixture.refresh(false);
        assert_eq!(oid(&fixture.git.shared, "main"), merged);
        assert!(
            report["branch_cleanup"]["deleted"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            !report["branch_cleanup"]["errors"]
                .as_array()
                .unwrap()
                .is_empty()
                || !report["branch_cleanup"]["warnings"]
                    .as_array()
                    .unwrap()
                    .is_empty()
        );
        fixture.assert_present(branch, &head);
    }
}

#[test]
fn mismatched_or_multiple_push_urls_preserve_every_remote_branch() {
    for multiple in [false, true] {
        let mut fixture = CleanupFixture::new();
        let branch = "codex/completed";
        let head = fixture.create_branch(branch);
        let merged = fixture.squash_merge(branch, &head);
        let other_remote = fixture.git.root.join("other-remote.git");
        command(
            &fixture.git.root,
            "git",
            ["init", "--bare", other_remote.to_str().unwrap()],
        );
        git(
            &other_remote,
            ["fetch", fixture.git.seed.to_str().unwrap(), &head],
        );
        git(
            &other_remote,
            ["update-ref", &format!("refs/heads/{branch}"), &head],
        );
        if multiple {
            git(
                &fixture.git.shared,
                [
                    "config",
                    "--add",
                    "remote.origin.pushurl",
                    fixture.git.remote.to_str().unwrap(),
                ],
            );
        }
        git(
            &fixture.git.shared,
            [
                "config",
                "--add",
                "remote.origin.pushurl",
                other_remote.to_str().unwrap(),
            ],
        );
        let report = fixture.refresh(false);
        assert_eq!(oid(&fixture.git.shared, "main"), merged);
        assert_eq!(report["branch_cleanup"]["status"], "unavailable");
        assert!(
            report["branch_cleanup"]["deleted"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            !report["branch_cleanup"]["warnings"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        fixture.assert_present(branch, &head);
        assert_eq!(
            oid(&other_remote, &format!("refs/heads/{branch}")),
            head,
            "fetch-side PR evidence must never authorize deleting a different push repository"
        );
    }
}

#[test]
fn remote_deletion_rejection_keeps_local_branch_and_main_sync() {
    let mut fixture = CleanupFixture::new();
    let branch = "codex/completed";
    let head = fixture.create_branch(branch);
    let merged = fixture.squash_merge(branch, &head);
    let hook = fixture.git.remote.join("hooks/pre-receive");
    std::fs::write(&hook, "#!/bin/sh\nwhile read old new ref; do\n  if test \"$new\" = 0000000000000000000000000000000000000000; then\n    echo 'fixture refuses branch deletion' >&2\n    exit 1\n  fi\ndone\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let report = fixture.refresh(false);
    assert_eq!(oid(&fixture.git.shared, "main"), merged);
    fixture.assert_present(branch, &head);
    assert!(
        report["branch_cleanup"]["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["branch"] == branch)
    );
    assert!(!contains_branch(&report, "deleted", branch));
}

#[test]
fn changed_remote_ref_during_cleanup_is_never_deleted() {
    let mut fixture = CleanupFixture::new();
    let branch = "codex/completed";
    let head = fixture.create_branch(branch);
    let merged = fixture.squash_merge(branch, &head);
    git(&fixture.git.seed, ["switch", branch]);
    std::fs::write(
        fixture.git.seed.join("raced-work.txt"),
        "new concurrent work\n",
    )
    .unwrap();
    git(&fixture.git.seed, ["add", "raced-work.txt"]);
    git(&fixture.git.seed, ["commit", "-m", "Race a resumed branch"]);
    let new_head = oid(&fixture.git.seed, "HEAD");
    // Supply the new object without moving the branch until the GH query.
    git(
        &fixture.git.remote,
        ["fetch", fixture.git.seed.to_str().unwrap(), &new_head],
    );
    fixture.requests["mutation"] = value!({
        "repo": fixture.git.remote, "branch": branch, "old": head, "new": new_head,
    });
    fixture.save();
    let report = fixture.refresh(false);
    assert_eq!(oid(&fixture.git.shared, "main"), merged);
    assert_eq!(
        oid(&fixture.git.remote, &format!("refs/heads/{branch}")),
        new_head
    );
    assert_eq!(
        oid(&fixture.git.shared, &format!("refs/heads/{branch}")),
        head
    );
    assert!(!contains_branch(&report, "deleted", branch));
    assert!(
        !report["branch_cleanup"]["errors"]
            .as_array()
            .unwrap()
            .is_empty()
            || contains_branch(&report, "skipped", branch)
    );
}

#[test]
fn mounted_worktrees_preserve_branch_and_all_local_content() {
    for change in ["untracked", "tracked", "staged"] {
        let mut fixture = CleanupFixture::new();
        let branch = "codex/completed";
        let head = fixture.create_branch(branch);
        let merged = fixture.squash_merge(branch, &head);
        let worktree = fixture.git.root.join("completed-worktree");
        git(
            &fixture.git.shared,
            ["worktree", "add", worktree.to_str().unwrap(), branch],
        );
        let changed_file = if change == "untracked" {
            "unpublished.txt"
        } else {
            "codex-completed.txt"
        };
        std::fs::write(
            worktree.join(changed_file),
            "local unpublished retained content\n",
        )
        .unwrap();
        if change == "staged" {
            git(&worktree, ["add", changed_file]);
        }
        std::fs::write(
            worktree.join("ignored.bin"),
            b"ignored local retained bytes\n",
        )
        .unwrap();
        std::fs::write(
            fixture.git.shared.join(".git/info/exclude"),
            "ignored.bin\n",
        )
        .unwrap();
        let index_before = git(&worktree, ["ls-files", "--stage"]).stdout;
        let report = fixture.refresh(false);
        assert_eq!(oid(&fixture.git.shared, "main"), merged);
        fixture.assert_present(branch, &head);
        assert!(contains_branch(&report, "skipped", branch));
        assert_eq!(
            std::fs::read(worktree.join(changed_file)).unwrap(),
            b"local unpublished retained content\n"
        );
        assert_eq!(
            std::fs::read(worktree.join("ignored.bin")).unwrap(),
            b"ignored local retained bytes\n"
        );
        assert_eq!(git(&worktree, ["ls-files", "--stage"]).stdout, index_before);
        assert_eq!(oid(&worktree, "HEAD"), head);
        assert!(worktree.join(".git").is_file());
    }
}

#[test]
fn clean_mounted_worktree_remains_mounted_with_branch_and_ignored_payloads() {
    let mut fixture = CleanupFixture::new();
    let branch = "codex/completed";
    let head = fixture.create_branch(branch);
    fixture.squash_merge(branch, &head);
    let worktree = fixture.git.root.join("completed-worktree");
    git(
        &fixture.git.shared,
        ["worktree", "add", worktree.to_str().unwrap(), branch],
    );
    std::fs::write(
        fixture.git.shared.join(".git/info/exclude"),
        "ignored.bin\n",
    )
    .unwrap();
    std::fs::write(
        worktree.join("ignored.bin"),
        b"retained local-only payload\n",
    )
    .unwrap();
    assert!(git(&worktree, ["status", "--porcelain"]).stdout.is_empty());

    // A mounted branch remains available, including when its only local
    // payload is ignored. Refresh never changes another worktree's HEAD.
    let preview = fixture.refresh(true);
    assert!(contains_branch(&preview, "skipped", branch));
    assert!(!contains_branch(&preview, "planned", branch));
    assert_eq!(
        String::from_utf8(git(&worktree, ["branch", "--show-current"]).stdout)
            .unwrap()
            .trim(),
        branch
    );
    fixture.assert_present(branch, &head);
    assert_eq!(
        std::fs::read(worktree.join("ignored.bin")).unwrap(),
        b"retained local-only payload\n"
    );

    let report = fixture.refresh(false);
    assert!(contains_branch(&report, "skipped", branch));
    assert!(!contains_branch(&report, "deleted", branch));
    fixture.assert_present(branch, &head);
    assert_eq!(
        String::from_utf8(git(&worktree, ["branch", "--show-current"]).stdout)
            .unwrap()
            .trim(),
        branch
    );
    assert_eq!(oid(&worktree, "HEAD"), head);
    assert_eq!(
        std::fs::read(worktree.join("ignored.bin")).unwrap(),
        b"retained local-only payload\n"
    );
    assert!(worktree.join(".git").is_file());
    assert!(worktree.join("codex-completed.txt").is_file());
}
