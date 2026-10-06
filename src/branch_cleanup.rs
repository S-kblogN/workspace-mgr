use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::git::GitRepo;
use crate::process;

#[derive(Debug, Clone, Serialize)]
pub struct BranchCleanupReport {
    pub status: &'static str,
    pub planned: Vec<BranchCleanupTarget>,
    pub deleted: Vec<BranchCleanupTarget>,
    pub skipped: Vec<SkippedBranch>,
    pub errors: Vec<CleanupFailure>,
    pub warnings: Vec<String>,
    pub remote_writes: bool,
}

impl Default for BranchCleanupReport {
    fn default() -> Self {
        Self {
            status: "not_run",
            planned: Vec::new(),
            deleted: Vec::new(),
            skipped: Vec::new(),
            errors: Vec::new(),
            warnings: Vec::new(),
            remote_writes: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BranchCleanupTarget {
    pub branch: String,
    pub head_oid: String,
    pub pull_request: u64,
    pub local: bool,
    pub remote: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkippedBranch {
    pub branch: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CleanupFailure {
    pub branch: String,
    pub action: &'static str,
    pub error: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequest {
    number: u64,
    state: String,
    merged_at: Option<String>,
    merge_commit: Option<Commit>,
    head_ref_name: String,
    head_ref_oid: String,
    base_ref_name: String,
    is_cross_repository: bool,
}

#[derive(Deserialize)]
struct Commit {
    oid: String,
}

struct HostingRepository {
    host: String,
    path: String,
}

/// Cleanup is a best-effort follow-up to a successful refresh. Verification
/// and permission failures are reported without undoing the synchronized base.
pub fn execute(
    repo: &GitRepo,
    config: &Config,
    base_oid: &str,
    dry_run: bool,
) -> BranchCleanupReport {
    let mut report = BranchCleanupReport {
        status: if dry_run { "dry_run" } else { "complete" },
        ..BranchCleanupReport::default()
    };
    match plan(repo, config, base_oid, &mut report) {
        Ok(true) => {}
        Ok(false) => {
            report.status = "not_applicable";
            return report;
        }
        Err(error) => {
            report.status = "unavailable";
            report.warnings.push(format!(
                "merged-branch cleanup is unavailable; Git refresh remains successful: {error}"
            ));
            // Never apply a partial plan after a global verification failure.
            report.planned.clear();
            return report;
        }
    }
    if !dry_run {
        apply(repo, config, &mut report);
    }
    report
}

fn plan(
    repo: &GitRepo,
    config: &Config,
    base_oid: &str,
    report: &mut BranchCleanupReport,
) -> Result<bool> {
    repo.validate_remote_name(&config.git.remote)?;
    let Some(host) = hosting_repository(repo, &config.git.remote)? else {
        return Ok(false);
    };
    validate_push_destination(repo, &config.git.remote)?;
    let (locals, symbolic) = local_heads(repo)?;
    let remotes = remote_heads(repo, &config.git.remote)?;
    let default = remote_default(repo, &config.git.remote)?;
    let current = repo.current_branch()?;
    let branches = locals
        .keys()
        .chain(remotes.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for branch in branches {
        let reason = if branch == config.git.branch {
            Some("configured base branch")
        } else if branch == default {
            Some("remote default branch")
        } else if current.as_deref() == Some(&branch) {
            Some("current branch")
        } else if symbolic.contains(&branch) {
            Some("symbolic local branch")
        } else {
            None
        };
        if let Some(reason) = reason {
            skip(report, &branch, reason);
            continue;
        }
        let proof = match merged_proof(repo, config, &host, &branch, base_oid) {
            Ok(Ok(proof)) => proof,
            Ok(Err(reason)) => {
                skip(report, &branch, &reason);
                continue;
            }
            Err(error @ Error::MissingCommand(_)) => return Err(error),
            Err(error) => {
                failure(report, &branch, "verification", error);
                continue;
            }
        };
        let local = locals.get(&branch);
        let remote = remotes.get(&branch);
        if local.is_some_and(|oid| oid != &proof.head_ref_oid) {
            skip(
                report,
                &branch,
                "local branch has new or unverified commits after the merged PR",
            );
            continue;
        }
        if remote.is_some_and(|oid| oid != &proof.head_ref_oid) {
            skip(
                report,
                &branch,
                "remote branch has new or unverified commits after the merged PR",
            );
            continue;
        }
        if remote.is_some() {
            match protected(repo, &host, &branch) {
                Ok(true) => {
                    skip(report, &branch, "protected remote branch");
                    continue;
                }
                Ok(false) => {}
                Err(error @ Error::MissingCommand(_)) => return Err(error),
                Err(error) => {
                    failure(report, &branch, "protection", error);
                    continue;
                }
            }
        }
        match repo.branch_worktrees(&branch) {
            Ok(paths) if !paths.is_empty() => {
                skip(
                    report,
                    &branch,
                    "branch is checked out in a worktree; its files and HEAD were preserved",
                );
                continue;
            }
            Err(error) => {
                failure(report, &branch, "verification", error);
                continue;
            }
            _ => {}
        }
        report.planned.push(BranchCleanupTarget {
            branch,
            head_oid: proof.head_ref_oid,
            pull_request: proof.number,
            local: local.is_some(),
            remote: remote.is_some(),
        });
    }
    Ok(true)
}

fn merged_proof(
    repo: &GitRepo,
    config: &Config,
    host: &HostingRepository,
    branch: &str,
    base: &str,
) -> Result<std::result::Result<PullRequest, String>> {
    let output = process::run(
        &hosting_command(),
        [
            "pr",
            "list",
            "--repo",
            &format!("{}/{}", host.host, host.path),
            "--head",
            branch,
            "--state",
            "all",
            "--limit",
            "100",
            "--json",
            "number,state,mergedAt,mergeCommit,headRefName,headRefOid,baseRefName,isCrossRepository",
        ],
        &repo.root,
    )?;
    let requests: Vec<PullRequest> = serde_json::from_str(&output.stdout).map_err(|error| {
        Error::message(format!("invalid merged-PR verification response: {error}"))
    })?;
    if requests.len() >= 100 {
        return Ok(Err(
            "PR query reached its limit; branch history is ambiguous".into(),
        ));
    }
    let requests = requests
        .into_iter()
        .filter(|pr| pr.head_ref_name == branch && !pr.is_cross_repository)
        .collect::<Vec<_>>();
    if requests.iter().any(|pr| pr.state == "OPEN") {
        return Ok(Err("branch has an open pull request".into()));
    }
    // Reused branch names retain multiple reviews. Do not guess which review
    // authorizes deleting the present ref, even if an older one was merged.
    if requests.len() != 1 {
        return Ok(Err(if requests.is_empty() {
            "no verified same-repository merged PR"
        } else {
            "branch has ambiguous or reused pull-request history"
        }
        .into()));
    }
    let pr = requests.into_iter().next().expect("one pull request");
    if pr.state != "MERGED" || pr.base_ref_name != config.git.branch {
        return Ok(Err("pull request is not merged".into()));
    }
    let timestamp = pr
        .merged_at
        .as_deref()
        .ok_or_else(|| Error::message("merged PR has no merge timestamp"))?;
    let merge = pr
        .merge_commit
        .as_ref()
        .ok_or_else(|| Error::message("merged PR has no merge commit"))?;
    if chrono::DateTime::parse_from_rfc3339(timestamp).is_err()
        || !oid(&merge.oid)
        || !oid(&pr.head_ref_oid)
    {
        return Err(Error::message(
            "merged PR has invalid exact commit evidence",
        ));
    }
    let ancestor = repo.run_unchecked(["merge-base", "--is-ancestor", &merge.oid, base])?;
    match ancestor.code {
        0 => Ok(Ok(pr)),
        1 => Ok(Err(
            "PR merge is not reachable from the synchronized base".into()
        )),
        _ => Err(Error::message(
            "cannot verify the PR merge commit on the synchronized base",
        )),
    }
}

fn protected(repo: &GitRepo, host: &HostingRepository, branch: &str) -> Result<bool> {
    let endpoint = format!("repos/{}/branches/{}", host.path, encoded_component(branch));
    let response = process::run(
        &hosting_command(),
        ["api", &endpoint, "--hostname", &host.host],
        &repo.root,
    )?;
    let value: serde_json::Value = serde_json::from_str(&response.stdout)
        .map_err(|error| Error::message(format!("invalid branch protection response: {error}")))?;
    value["protected"].as_bool().ok_or_else(|| {
        Error::message(
            "branch protection response did not positively identify its protection state",
        )
    })
}

fn apply(repo: &GitRepo, config: &Config, report: &mut BranchCleanupReport) {
    for target in report.planned.clone() {
        let check = (|| -> Result<bool> {
            verify_live_refs(repo, config, &target, target.remote)?;
            Ok(repo.branch_worktrees(&target.branch)?.is_empty())
        })();
        match check {
            Ok(true) => {}
            Ok(false) => {
                skip(
                    report,
                    &target.branch,
                    "mounted worktrees changed during cleanup",
                );
                continue;
            }
            Err(error) => {
                failure(report, &target.branch, "verification", error);
                continue;
            }
        }
        let mut deleted = target.clone();
        deleted.local = false;
        deleted.remote = false;
        if target.remote {
            let reference = format!("refs/heads/{}", target.branch);
            let lease = format!("--force-with-lease={reference}:{}", target.head_oid);
            let deletion = format!(":{reference}");
            match repo.run(["push", &lease, &config.git.remote, &deletion]) {
                Ok(_) => {
                    deleted.remote = true;
                    report.remote_writes = true;
                }
                Err(error) => {
                    failure(report, &target.branch, "remote", error);
                    continue;
                }
            }
        }
        if target.local {
            let result = (|| -> Result<()> {
                verify_live_refs(repo, config, &target, false)?;
                repo.ensure_branch_not_checked_out(&target.branch)?;
                repo.run([
                    "update-ref",
                    "--no-deref",
                    "-d",
                    &format!("refs/heads/{}", target.branch),
                    &target.head_oid,
                ])?;
                Ok(())
            })();
            match result {
                Ok(()) => deleted.local = true,
                Err(error) => failure(report, &target.branch, "local", error),
            }
        }
        if deleted.local || deleted.remote {
            if let Err(error) = remove_tracking_ref(repo, config, &target) {
                failure(report, &target.branch, "tracking", error);
            }
            report.deleted.push(deleted);
        }
    }
}

fn verify_live_refs(
    repo: &GitRepo,
    config: &Config,
    target: &BranchCleanupTarget,
    remote_present: bool,
) -> Result<()> {
    let local = repo.optional_oid(&format!("refs/heads/{}", target.branch))?;
    let remote = repo.remote_branch_oid(&config.git.remote, &target.branch)?;
    if local.as_deref() != target.local.then_some(target.head_oid.as_str())
        || remote.as_deref() != remote_present.then_some(target.head_oid.as_str())
    {
        return Err(Error::message(
            "branch has new or changed commits during cleanup; its remaining refs were preserved",
        ));
    }
    Ok(())
}

fn remove_tracking_ref(
    repo: &GitRepo,
    config: &Config,
    target: &BranchCleanupTarget,
) -> Result<()> {
    let reference = format!("refs/remotes/{}/{}", config.git.remote, target.branch);
    if repo.optional_oid(&reference)?.as_deref() != Some(&target.head_oid) {
        return Ok(());
    }
    // HEAD and other symbolic aliases are repository configuration, not stale
    // cached branch tips. Never dereference one while removing a cache ref.
    if repo
        .run_unchecked(["symbolic-ref", "--quiet", &reference])?
        .code
        != 1
    {
        return Ok(());
    }
    repo.run([
        "update-ref",
        "--no-deref",
        "-d",
        &reference,
        &target.head_oid,
    ])?;
    Ok(())
}

fn local_heads(repo: &GitRepo) -> Result<(BTreeMap<String, String>, BTreeSet<String>)> {
    let output = repo.run([
        "for-each-ref",
        "--format=%(refname)%00%(objectname)%00%(symref)",
        "refs/heads/",
    ])?;
    let mut result = BTreeMap::new();
    let mut symbolic = BTreeSet::new();
    for line in output.stdout.lines() {
        let fields = line.split('\0').collect::<Vec<_>>();
        if fields.len() != 3 || !oid(fields[1]) {
            return Err(Error::message("invalid local branch listing"));
        }
        let name = fields[0]
            .strip_prefix("refs/heads/")
            .ok_or_else(|| Error::message("local branch listing escaped heads"))?;
        repo.validate_branch(name)?;
        if !fields[2].is_empty() {
            symbolic.insert(name.to_owned());
        }
        result.insert(name.to_owned(), fields[1].to_owned());
    }
    Ok((result, symbolic))
}

fn remote_heads(repo: &GitRepo, remote: &str) -> Result<BTreeMap<String, String>> {
    let output = repo.run(["ls-remote", "--heads", remote])?;
    let mut result = BTreeMap::new();
    for line in output.stdout.lines() {
        let (id, reference) = line
            .split_once('\t')
            .ok_or_else(|| Error::message("invalid remote branch listing"))?;
        let name = reference
            .strip_prefix("refs/heads/")
            .ok_or_else(|| Error::message("remote branch listing escaped heads"))?;
        if !oid(id) {
            return Err(Error::message("remote branch has no exact commit ID"));
        }
        repo.validate_branch(name)?;
        if result.insert(name.to_owned(), id.to_owned()).is_some() {
            return Err(Error::message("remote branch listing repeated a ref"));
        }
    }
    Ok(result)
}

fn remote_default(repo: &GitRepo, remote: &str) -> Result<String> {
    let output = repo.run(["ls-remote", "--symref", remote, "HEAD"])?;
    output
        .stdout
        .lines()
        .find_map(|line| {
            line.strip_prefix("ref: refs/heads/")
                .and_then(|rest| rest.strip_suffix("\tHEAD"))
                .map(ToOwned::to_owned)
        })
        .ok_or_else(|| Error::message("cannot identify the remote default branch safely"))
}

fn validate_push_destination(repo: &GitRepo, remote: &str) -> Result<()> {
    let fetch = repo.run(["remote", "get-url", remote])?.stdout;
    let pushes = repo
        .run(["remote", "get-url", "--push", "--all", remote])?
        .stdout;
    let urls = pushes
        .lines()
        .filter(|url| !url.is_empty())
        .collect::<Vec<_>>();
    if urls.len() != 1 || urls[0] != fetch.trim_end() {
        return Err(Error::message(
            "automatic branch cleanup requires exactly one push URL identical to the verified fetch remote; differing or multiple push destinations were preserved",
        ));
    }
    Ok(())
}

fn hosting_command() -> String {
    #[cfg(feature = "test-storage")]
    if let Ok(path) = std::env::var("WORKSPACE_MGR_TEST_GH") {
        return path;
    }
    "gh".into()
}

fn hosting_repository(repo: &GitRepo, remote: &str) -> Result<Option<HostingRepository>> {
    #[cfg(feature = "test-storage")]
    if std::env::var_os("WORKSPACE_MGR_TEST_GH").is_some() {
        return Ok(Some(HostingRepository {
            host: "example.invalid".into(),
            path: "owner/archive-fixture".into(),
        }));
    }
    let url = repo.run(["remote", "get-url", remote])?.stdout;
    let raw = url.trim();
    let (host, path) = if let Some((_, value)) = raw.split_once("://") {
        let (authority, path) = value
            .split_once('/')
            .ok_or_else(|| Error::message("invalid Git repository URL"))?;
        (authority.rsplit('@').next().unwrap_or(authority), path)
    } else if let Some((authority, path)) = raw.split_once(':') {
        (authority.rsplit('@').next().unwrap_or(authority), path)
    } else {
        return Ok(None);
    };
    let path = path
        .trim_end_matches('/')
        .strip_suffix(".git")
        .unwrap_or(path.trim_end_matches('/'));
    let pieces = path.split('/').collect::<Vec<_>>();
    if pieces.len() != 2
        || pieces.iter().any(|piece| {
            piece.is_empty()
                || !piece
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        })
        || host.is_empty()
        || !host
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-.".contains(&c))
    {
        return Ok(None);
    }
    if matches!(host, "gitlab.com" | "bitbucket.org" | "codeberg.org") {
        return Ok(None);
    }
    Ok(Some(HostingRepository {
        host: host.into(),
        path: path.into(),
    }))
}

fn encoded_component(value: &str) -> String {
    let mut result = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            result.push(byte as char);
        } else {
            result.push_str(&format!("%{byte:02X}"));
        }
    }
    result
}

fn oid(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn skip(report: &mut BranchCleanupReport, branch: &str, reason: &str) {
    report.skipped.push(SkippedBranch {
        branch: branch.into(),
        reason: reason.into(),
    });
}

fn failure(report: &mut BranchCleanupReport, branch: &str, action: &'static str, error: Error) {
    report.errors.push(CleanupFailure {
        branch: branch.into(),
        action,
        error: error.to_string(),
    });
}
