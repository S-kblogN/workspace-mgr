use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::config::{CONFIG_NAME, Config};
use crate::dvc::{self, DataStatus, PointerDocument, PointerEntry};
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::manifest::{CloudUsageApproval, ResolvedTask};
use crate::path::resolved_under;
use crate::policy::{CLOUD_USAGE_APPROVAL_BYTES, TASK_MANIFEST_NAME};
use crate::storage::PLACEMENT_SUFFIX;
use crate::transaction::task_state_dir;

/// Test builds may lower the approval threshold; they can never raise it.
pub const THRESHOLD_OVERRIDE_ENV: &str = "WORKSPACE_MGR_TEST_CLOUD_USAGE_THRESHOLD_BYTES";
pub const APPROVAL_TRAILER: &str = "Cloud-Usage-Approval";
const STATE_SCHEMA: u32 = 1;
const STATE_NAME: &str = "cloud-usage.json";
const CACHE_SCHEMA: u32 = 1;
const CACHE_NAME: &str = "cloud-usage-cache.json";
const LFS_POINTER_MAX_BYTES: u64 = 1024;
/// New workspace-mgr control-file content that a cleanup-only publication may
/// carry. Placement records, manifests, managed ignore rules, and small
/// metadata rewrites stay far below it; metadata that only drops entries is
/// free.
pub(crate) const CONTROL_FILE_ALLOWANCE_BYTES: u64 = 1_048_576;
/// Suggested limits are whole multiples of 256 MiB.
const SUGGESTION_STEP_BYTES: u64 = 268_435_456;
const CONTRIBUTOR_LIMIT: usize = 10;
// Single-threaded packing with fixed settings keeps packed sizes reproducible.
const PACK_SETTINGS: [&str; 10] = [
    "-c",
    "pack.threads=1",
    "-c",
    "pack.window=10",
    "-c",
    "pack.depth=50",
    "-c",
    "core.compression=-1",
    "-c",
    "pack.compression=-1",
];

pub fn effective_threshold() -> u64 {
    threshold_from_override(std::env::var(THRESHOLD_OVERRIDE_ENV).ok().as_deref())
}

fn threshold_from_override(value: Option<&str>) -> u64 {
    #[cfg(feature = "test-storage")]
    if let Some(lowered) = value.and_then(|value| value.trim().parse::<u64>().ok()) {
        return lowered.min(CLOUD_USAGE_APPROVAL_BYTES);
    }
    #[cfg(not(feature = "test-storage"))]
    let _ = value;
    CLOUD_USAGE_APPROVAL_BYTES
}

/// An approval can raise the ceiling above the threshold but never lower it.
pub(crate) fn effective_limit(threshold_bytes: u64, approval: Option<&CloudUsageApproval>) -> u64 {
    approval.map_or(threshold_bytes, |approval| {
        approval.limit_bytes.max(threshold_bytes)
    })
}

/// Parses a byte count or a decimal/binary size such as `2.5 GB` or `3GiB`.
pub fn parse_size(raw: &str) -> std::result::Result<u64, String> {
    let value = raw.trim();
    let split = value
        .find(|character: char| !character.is_ascii_digit() && character != '.')
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(split);
    let unit = unit.trim_start();
    let multiplier: u128 = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "tb" => 1_000_000_000_000,
        "kib" => 1 << 10,
        "mib" => 1 << 20,
        "gib" => 1 << 30,
        "tib" => 1 << 40,
        _ => {
            return Err(format!(
                "invalid size {raw:?}; use a byte count or a number with B, KB, MB, GB, TB, KiB, MiB, GiB, or TiB"
            ));
        }
    };
    let (whole, fraction) = match number.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (number, None),
    };
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    if !digits(whole) || !fraction.is_none_or(digits) {
        return Err(format!(
            "invalid size {raw:?}; use a byte count or a number with B, KB, MB, GB, TB, KiB, MiB, GiB, or TiB"
        ));
    }
    if fraction.is_some() && unit.is_empty() {
        return Err(format!(
            "invalid size {raw:?}; a fractional size needs a unit"
        ));
    }
    let too_large = || format!("size {raw:?} is too large");
    let fraction = fraction.unwrap_or_default().trim_end_matches('0');
    let scale = u32::try_from(fraction.len())
        .ok()
        .and_then(|digits| 10_u128.checked_pow(digits))
        .ok_or_else(too_large)?;
    let whole = whole.parse::<u128>().map_err(|_| too_large())?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u128>().map_err(|_| too_large())?
    };
    let scaled = whole
        .checked_mul(scale)
        .and_then(|value| value.checked_add(fraction))
        .and_then(|value| value.checked_mul(multiplier))
        .ok_or_else(too_large)?;
    if scaled % scale != 0 {
        return Err(format!("size {raw:?} is not a whole number of bytes"));
    }
    u64::try_from(scaled / scale).map_err(|_| too_large())
}

/// Renders binary units with the exact byte count, never rounding up.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [(u64, &str); 4] = [
        (1 << 40, "TiB"),
        (1 << 30, "GiB"),
        (1 << 20, "MiB"),
        (1 << 10, "KiB"),
    ];
    for (scale, unit) in UNITS {
        if bytes < scale {
            continue;
        }
        let hundredths = u128::from(bytes) * 100 / u128::from(scale);
        let (whole, fraction) = (hundredths / 100, hundredths % 100);
        let number = if fraction == 0 {
            whole.to_string()
        } else if fraction % 10 == 0 {
            format!("{whole}.{}", fraction / 10)
        } else {
            format!("{whole}.{fraction:02}")
        };
        return format!("{number} {unit} ({bytes} bytes)");
    }
    if bytes == 1 {
        "1 byte".to_owned()
    } else {
        format!("{bytes} bytes")
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub git_bytes: u64,
    pub git_uncompressed_bytes: u64,
    pub git_lfs_bytes: u64,
    pub s3_bytes: u64,
    pub total_bytes: u64,
}

impl UsageTotals {
    fn new(git_objects: u64, git_uncompressed: u64, git_lfs: u64, s3: u64) -> Self {
        let git_bytes = git_objects.saturating_add(git_lfs);
        Self {
            git_bytes,
            git_uncompressed_bytes: git_uncompressed,
            git_lfs_bytes: git_lfs,
            s3_bytes: s3,
            total_bytes: git_bytes.saturating_add(s3),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Contributor {
    pub path: String,
    pub store: &'static str,
    pub bytes: u64,
    pub versions: u64,
    pub state: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct CloudUsageReport {
    pub status: String,
    pub publish_allowed: bool,
    pub cleanup_only: bool,
    pub git_history_exceeds_limit: bool,
    pub threshold_bytes: u64,
    pub limit_bytes: u64,
    pub approval: Option<CloudUsageApproval>,
    pub published: UsageTotals,
    pub projected: UsageTotals,
    pub git_measure: String,
    pub headroom_bytes: u64,
    pub suggested_limit_bytes: u64,
    pub contributors: Vec<Contributor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl CloudUsageReport {
    pub fn approval_required(&self) -> bool {
        self.status == "approval_required"
    }
}

/// What plan and publish can observe before they change anything.
#[derive(Debug, Clone, Copy)]
pub(crate) struct UsageInputs<'a> {
    pub state_dir: &'a Path,
    pub remote_base_oid: &'a str,
    pub remote_target_oid: Option<&'a str>,
    /// Tree that the publication would commit.
    pub projected_tree_oid: &'a str,
    /// In-scope worktree metadata files that the publication would push.
    pub pointers: &'a [String],
    /// New files that automatic placement would move to S3.
    pub automatic_s3: &'a [String],
    /// Whether to ask the storage engine for uncommitted output changes.
    pub inspect_outputs: bool,
}

impl UsageInputs<'_> {
    fn base_oid(&self) -> &str {
        self.remote_target_oid.unwrap_or(self.remote_base_oid)
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Measurement {
    git: GitUsage,
    storage: StorageUsage,
    packed: Option<PackedGit>,
}

#[derive(Debug, Clone, Default)]
struct GitUsage {
    published_objects: Vec<GitObject>,
    delta_objects: Vec<GitObject>,
    published_bytes: u64,
    delta_bytes: u64,
    published_lfs_bytes: u64,
    projected_lfs_bytes: u64,
    adds_content: bool,
    /// Uncompressed object bytes per path.
    contributors: BTreeMap<String, Tally>,
    /// Compressed on-disk object bytes per path, comparable with packed totals.
    packed_contributors: BTreeMap<String, Tally>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct StorageUsage {
    published_bytes: u64,
    projected_bytes: u64,
    pending_uploads: bool,
    contributors: BTreeMap<String, Tally>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PackedGit {
    published: u64,
    delta: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Tally {
    bytes: u64,
    versions: u64,
    pending: bool,
}

impl Tally {
    fn add(&mut self, bytes: u64, pending: bool) {
        self.bytes = self.bytes.saturating_add(bytes);
        self.versions += 1;
        self.pending |= pending;
    }
}

/// Measures published and projected Git and S3 bytes for one task.
///
/// Packed Git sizes are measured only when the uncompressed estimate exceeds
/// `limit_bytes`, because packing can be expensive.
pub(crate) fn measure(
    repo: &GitRepo,
    config: &Config,
    inputs: &UsageInputs<'_>,
    limit_bytes: u64,
) -> Result<Measurement> {
    let mut cache = UsageCache::load(inputs.state_dir);
    let mut measurement = Measurement {
        git: measure_git(repo, inputs)?,
        storage: measure_storage(repo, config, inputs, &mut cache)?,
        packed: None,
    };
    measurement.settle(repo, inputs, limit_bytes, &mut cache)?;
    cache.save(inputs.state_dir);
    Ok(measurement)
}

impl Measurement {
    /// Re-measures S3 from the current worktree metadata, keeping Git bytes.
    pub(crate) fn refresh_storage(
        &mut self,
        repo: &GitRepo,
        config: &Config,
        inputs: &UsageInputs<'_>,
        limit_bytes: u64,
    ) -> Result<()> {
        let mut cache = UsageCache::load(inputs.state_dir);
        self.storage = measure_storage(repo, config, inputs, &mut cache)?;
        self.settle(repo, inputs, limit_bytes, &mut cache)?;
        cache.save(inputs.state_dir);
        Ok(())
    }

    /// Re-measures Git bytes for `inputs.projected_tree_oid`, keeping S3.
    pub(crate) fn refresh_git(
        &mut self,
        repo: &GitRepo,
        inputs: &UsageInputs<'_>,
        limit_bytes: u64,
    ) -> Result<()> {
        let mut cache = UsageCache::load(inputs.state_dir);
        self.git = measure_git(repo, inputs)?;
        self.packed = None;
        self.settle(repo, inputs, limit_bytes, &mut cache)?;
        cache.save(inputs.state_dir);
        Ok(())
    }

    pub(crate) fn totals(&self) -> (UsageTotals, UsageTotals) {
        let git = &self.git;
        let (published_git, projected_git) = match self.packed {
            Some(packed) => (
                packed.published,
                packed.published.saturating_add(packed.delta),
            ),
            None => (
                git.published_bytes,
                git.published_bytes.saturating_add(git.delta_bytes),
            ),
        };
        (
            UsageTotals::new(
                published_git,
                git.published_bytes,
                git.published_lfs_bytes,
                self.storage.published_bytes,
            ),
            UsageTotals::new(
                projected_git,
                git.published_bytes.saturating_add(git.delta_bytes),
                git.projected_lfs_bytes,
                self.storage.projected_bytes,
            ),
        )
    }

    #[cfg(test)]
    pub(crate) fn pending_uploads(&self) -> bool {
        self.storage.pending_uploads
    }

    /// A publication that uploads nothing and, apart from deletions, only
    /// publishes a bounded amount of new workspace-mgr control-file content
    /// cannot materially grow the footprint.
    pub(crate) fn cleanup_only(&self) -> bool {
        !self.storage.pending_uploads && !self.git.adds_content
    }

    pub(crate) fn git_measure(&self) -> &'static str {
        if self.packed.is_some() {
            "packed"
        } else {
            "uncompressed"
        }
    }

    /// The largest contributors. Git bytes are compressed on-disk estimates
    /// when the totals are packed, and uncompressed object sizes otherwise.
    pub(crate) fn contributors(&self) -> Vec<Contributor> {
        let git = if self.packed.is_some() {
            &self.git.packed_contributors
        } else {
            &self.git.contributors
        };
        let mut contributors = git
            .iter()
            .map(|(path, tally)| contributor(path, "git", tally))
            .chain(
                self.storage
                    .contributors
                    .iter()
                    .map(|(path, tally)| contributor(path, "s3", tally)),
            )
            .collect::<Vec<_>>();
        contributors.sort_by(|left, right| {
            right
                .bytes
                .cmp(&left.bytes)
                .then_with(|| left.path.cmp(&right.path))
                .then_with(|| left.store.cmp(right.store))
        });
        contributors.truncate(CONTRIBUTOR_LIMIT);
        contributors
    }

    fn settle(
        &mut self,
        repo: &GitRepo,
        inputs: &UsageInputs<'_>,
        limit_bytes: u64,
        cache: &mut UsageCache,
    ) -> Result<()> {
        if self.packed.is_some() {
            return Ok(());
        }
        let (published, projected) = self.totals();
        if published.total_bytes <= limit_bytes && projected.total_bytes <= limit_bytes {
            return Ok(());
        }
        let published_packed = match inputs.remote_target_oid {
            None => 0,
            Some(target) => match cache.packed_published(target, inputs.remote_base_oid) {
                Some(bytes) => bytes,
                None => {
                    let bytes = packed_size(repo, &self.git.published_objects)?;
                    cache.record_packed_published(target, inputs.remote_base_oid, bytes);
                    bytes
                }
            },
        };
        self.packed = Some(PackedGit {
            published: published_packed,
            delta: packed_size(repo, &self.git.delta_objects)?,
        });
        Ok(())
    }
}

fn contributor(path: &str, store: &'static str, tally: &Tally) -> Contributor {
    Contributor {
        path: path.to_owned(),
        store,
        bytes: tally.bytes,
        versions: tally.versions,
        state: if tally.pending {
            "pending"
        } else {
            "published"
        },
    }
}

pub(crate) fn evaluate(
    measurement: &Measurement,
    threshold_bytes: u64,
    approval: Option<&CloudUsageApproval>,
) -> CloudUsageReport {
    let limit_bytes = effective_limit(threshold_bytes, approval);
    let (published, projected) = measurement.totals();
    let within_limit = projected.total_bytes <= limit_bytes;
    let cleanup_only = measurement.cleanup_only();
    let git_history_exceeds_limit = published.git_bytes > limit_bytes;
    CloudUsageReport {
        status: if within_limit {
            "within_limit"
        } else {
            "approval_required"
        }
        .to_owned(),
        publish_allowed: within_limit || cleanup_only,
        cleanup_only,
        git_history_exceeds_limit,
        threshold_bytes,
        limit_bytes,
        approval: approval.cloned(),
        published,
        projected,
        git_measure: measurement.git_measure().to_owned(),
        headroom_bytes: limit_bytes.saturating_sub(projected.total_bytes),
        suggested_limit_bytes: suggested_limit(projected.total_bytes, limit_bytes, !within_limit),
        contributors: measurement.contributors(),
        message: (!within_limit).then(|| {
            approval_message(
                &projected,
                limit_bytes,
                cleanup_only,
                git_history_exceeds_limit,
            )
        }),
    }
}

/// A ceiling with headroom for continued work, in whole multiples of 256 MiB.
fn suggested_limit(projected_bytes: u64, limit_bytes: u64, approval_required: bool) -> u64 {
    let step = u128::from(SUGGESTION_STEP_BYTES);
    let projected = u128::from(projected_bytes);
    let mut suggestion = (projected * 5).div_ceil(4).max(projected + step);
    if approval_required {
        suggestion = suggestion.max(u128::from(limit_bytes) + step);
    }
    u64::try_from(suggestion.div_ceil(step) * step).unwrap_or(u64::MAX)
}

fn approval_message(
    projected: &UsageTotals,
    limit_bytes: u64,
    cleanup_only: bool,
    git_history_exceeds_limit: bool,
) -> String {
    let mut message = format!(
        "Projected cloud usage {} exceeds the limit {}. This task is waiting for the user's decision: record an approval only after the user gives it in this chat, or perform only the cleanup the user chooses and re-measure with `workspace-mgr plan`.",
        format_bytes(projected.total_bytes),
        format_bytes(limit_bytes)
    );
    if cleanup_only {
        message.push_str(&format!(
            " This publication only removes content, apart from at most {} of new workspace-mgr control-file content, where metadata that only drops entries is free, so it remains allowed.",
            format_bytes(CONTROL_FILE_ALLOWANCE_BYTES)
        ));
    }
    if git_history_exceeds_limit {
        message.push_str(
            " Published Git history alone exceeds the limit and cleanup cannot shrink it; only an approval or discarding the task resolves it.",
        );
    }
    message
}

/// The self-contained error for a publication refused by the usage gate.
pub(crate) fn refusal_message(task_id: &str, report: &CloudUsageReport) -> String {
    let published = &report.published;
    let projected = &report.projected;
    let mut message = format!(
        "cloud usage for task {task_id} needs the user's approval: published Git {}, S3 {}, total {}; projected Git {}, S3 {}, total {}; limit {}. The task is waiting for the user's decision. Record an approval only after the user gives it in this chat; otherwise perform only the cleanup the user chooses (`workspace-mgr remove`, `workspace-mgr untrack`, or `workspace-mgr task discard`), since a publication that only removes content, apart from at most {} of new workspace-mgr control-file content per publication, where metadata that only drops entries is free, remains allowed.",
        format_bytes(published.git_bytes),
        format_bytes(published.s3_bytes),
        format_bytes(published.total_bytes),
        format_bytes(projected.git_bytes),
        format_bytes(projected.s3_bytes),
        format_bytes(projected.total_bytes),
        format_bytes(report.limit_bytes),
        format_bytes(CONTROL_FILE_ALLOWANCE_BYTES),
    );
    if report.git_history_exceeds_limit {
        message.push_str(
            " Published Git history alone exceeds the limit, so cleanup cannot resolve it; only an approval or discarding the task can.",
        );
    }
    message.push_str(" Run `workspace-mgr plan` for the largest contributors.");
    message
}

/// The usage decision for one plan or publish run. Every evaluation records
/// or clears the pending decision in the task's private state.
#[derive(Debug)]
pub(crate) struct UsageGate {
    task_id: String,
    threshold_bytes: u64,
    approval: Option<CloudUsageApproval>,
    limit_bytes: u64,
    state: CloudUsageState,
    measurement: Measurement,
    report: CloudUsageReport,
}

impl UsageGate {
    /// Measures and evaluates the publication against the limit that the
    /// task manifest's approval sets.
    pub(crate) fn open(
        repo: &GitRepo,
        config: &Config,
        task: &ResolvedTask,
        inputs: &UsageInputs<'_>,
        threshold_bytes: u64,
    ) -> Result<Self> {
        let state = load_state(inputs.state_dir, &task.task_id, &task.branch)?;
        let approval = task.cloud_usage_approval.clone();
        let limit_bytes = effective_limit(threshold_bytes, approval.as_ref());
        let measurement = measure(repo, config, inputs, limit_bytes)?;
        let report = evaluate(&measurement, threshold_bytes, approval.as_ref());
        let mut gate = Self {
            task_id: task.task_id.clone(),
            threshold_bytes,
            approval,
            limit_bytes,
            state,
            measurement,
            report,
        };
        gate.persist(inputs)?;
        Ok(gate)
    }

    /// Re-evaluates S3 from the committed metadata before anything is uploaded.
    pub(crate) fn recheck_storage(
        &mut self,
        repo: &GitRepo,
        config: &Config,
        inputs: &UsageInputs<'_>,
    ) -> Result<()> {
        self.measurement
            .refresh_storage(repo, config, inputs, self.limit_bytes)?;
        self.reevaluate(inputs)
    }

    /// Re-evaluates Git from the final tree before a commit is created.
    pub(crate) fn recheck_git(&mut self, repo: &GitRepo, inputs: &UsageInputs<'_>) -> Result<()> {
        self.measurement
            .refresh_git(repo, inputs, self.limit_bytes)?;
        self.reevaluate(inputs)
    }

    /// Refuses a publication that would grow the task past its limit.
    pub(crate) fn enforce(&self) -> Result<()> {
        if self.report.publish_allowed {
            return Ok(());
        }
        Err(Error::message(refusal_message(&self.task_id, &self.report)))
    }

    pub(crate) fn approval(&self) -> Option<&CloudUsageApproval> {
        self.approval.as_ref()
    }

    pub(crate) fn report(&self) -> &CloudUsageReport {
        &self.report
    }

    fn reevaluate(&mut self, inputs: &UsageInputs<'_>) -> Result<()> {
        self.report = evaluate(
            &self.measurement,
            self.threshold_bytes,
            self.approval.as_ref(),
        );
        self.persist(inputs)
    }

    fn persist(&mut self, inputs: &UsageInputs<'_>) -> Result<()> {
        self.state.update_pending(&self.report, inputs);
        save_state(inputs.state_dir, &self.state)
    }
}

/// The task manifest's approval and the locally recorded pending decision.
#[derive(Debug, Clone, Serialize)]
pub struct LocalUsageStatus {
    pub threshold_bytes: u64,
    pub limit_bytes: u64,
    pub approval: Option<CloudUsageApproval>,
    pub pending: Option<PendingDecision>,
}

pub(crate) fn local_status(repo: &GitRepo, task: &ResolvedTask) -> Result<LocalUsageStatus> {
    let state_dir = task_state_dir(&repo.common_dir()?, task);
    let state = load_state(&state_dir, &task.task_id, &task.branch)?;
    let threshold_bytes = effective_threshold();
    Ok(LocalUsageStatus {
        threshold_bytes,
        limit_bytes: effective_limit(threshold_bytes, task.cloud_usage_approval.as_ref()),
        approval: task.cloud_usage_approval.clone(),
        pending: state.pending,
    })
}

/// Prints the pending-decision reminder, if any. Never fails the command.
pub(crate) fn remind(repo: &GitRepo, task: &ResolvedTask) {
    let Ok(common_dir) = repo.common_dir() else {
        return;
    };
    let state_dir = task_state_dir(&common_dir, task);
    if let Some(line) = reminder(&state_dir, task) {
        eprintln!("{line}");
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingDecision {
    pub measured_at: String,
    pub limit_bytes: u64,
    pub remote_base_oid: String,
    pub remote_target_oid: Option<String>,
    pub projected_tree_oid: String,
    pub published: UsageTotals,
    pub projected: UsageTotals,
}

impl PendingDecision {
    pub(crate) fn exceeds(&self, limit_bytes: u64) -> bool {
        self.projected.total_bytes > limit_bytes
    }
}

/// Private, local-only state. The user's approval lives in the task
/// manifest; this file only remembers the decision a measurement is waiting
/// for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudUsageState {
    pub schema_version: u32,
    pub task_id: String,
    pub branch: String,
    #[serde(default)]
    pub pending: Option<PendingDecision>,
}

impl CloudUsageState {
    pub(crate) fn new(task_id: &str, branch: &str) -> Self {
        Self {
            schema_version: STATE_SCHEMA,
            task_id: task_id.to_owned(),
            branch: branch.to_owned(),
            pending: None,
        }
    }

    /// Records the decision a plan or publish gate is waiting for, or clears
    /// it once usage is within the limit.
    pub(crate) fn update_pending(&mut self, report: &CloudUsageReport, inputs: &UsageInputs<'_>) {
        self.pending = report.approval_required().then(|| PendingDecision {
            measured_at: now_rfc3339(),
            limit_bytes: report.limit_bytes,
            remote_base_oid: inputs.remote_base_oid.to_owned(),
            remote_target_oid: inputs.remote_target_oid.map(ToOwned::to_owned),
            projected_tree_oid: inputs.projected_tree_oid.to_owned(),
            published: report.published,
            projected: report.projected,
        });
    }
}

pub(crate) fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub(crate) fn state_path(state_dir: &Path) -> PathBuf {
    state_dir.join(STATE_NAME)
}

pub(crate) fn load_state(state_dir: &Path, task_id: &str, branch: &str) -> Result<CloudUsageState> {
    let path = state_path(state_dir);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CloudUsageState::new(task_id, branch));
        }
        Err(source) => return Err(Error::Io { path, source }),
    };
    let state: CloudUsageState = serde_json::from_str(&raw).map_err(|error| {
        Error::message(format!(
            "invalid private cloud-usage state {}: {error}",
            path.display()
        ))
    })?;
    if state.schema_version != STATE_SCHEMA {
        return Err(Error::message(format!(
            "private cloud-usage state {} has an unsupported schema",
            path.display()
        )));
    }
    if state.task_id != task_id || state.branch != branch {
        return Err(Error::message(format!(
            "private cloud-usage state belongs to task {:?} on {:?}, not {task_id:?} on {branch:?}",
            state.task_id, state.branch
        )));
    }
    Ok(state)
}

pub(crate) fn save_state(state_dir: &Path, state: &CloudUsageState) -> Result<()> {
    let path = state_path(state_dir);
    if state.pending.is_none() {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(Error::Io { path, source }),
        }
        return Ok(());
    }
    let encoded = serde_json::to_vec_pretty(state)
        .map_err(|error| Error::message(format!("failed to encode cloud-usage state: {error}")))?;
    write_atomic(state_dir, &path, &encoded)
}

fn write_atomic(parent: &Path, path: &Path, encoded: &[u8]) -> Result<()> {
    fs::create_dir_all(parent).at(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).at(parent)?;
    temporary.write_all(encoded).at(path)?;
    temporary.write_all(b"\n").at(path)?;
    temporary.flush().at(path)?;
    temporary.persist(path).map_err(|error| Error::Io {
        path: path.to_path_buf(),
        source: error.error,
    })?;
    Ok(())
}

/// The one-line reminder task-scoped commands print while a decision is due.
/// It reflects the last measurement, which only `plan` and `publish` refresh,
/// so it must not override an answer the user already gave.
pub(crate) fn pending_reminder(
    task_id: &str,
    state: &CloudUsageState,
    approval: Option<&CloudUsageApproval>,
    threshold_bytes: u64,
) -> Option<String> {
    let pending = state.pending.as_ref()?;
    let limit_bytes = effective_limit(threshold_bytes, approval);
    if !pending.exceeds(limit_bytes) {
        return None;
    }
    Some(format!(
        "workspace-mgr: task {task_id} is waiting for the user's cloud-usage decision, as last measured by `workspace-mgr plan` or `workspace-mgr publish`: projected {} exceeds limit {}. Unless the user already answered, stop task work and ask the user; after carrying out the user's answer, run `workspace-mgr plan` to re-measure.",
        format_bytes(pending.projected.total_bytes),
        format_bytes(limit_bytes)
    ))
}

/// Best-effort variant of [`pending_reminder`] that never fails a command.
pub(crate) fn reminder(state_dir: &Path, task: &ResolvedTask) -> Option<String> {
    let state = load_state(state_dir, &task.task_id, &task.branch).ok()?;
    pending_reminder(
        &task.task_id,
        &state,
        task.cloud_usage_approval.as_ref(),
        effective_threshold(),
    )
}

/// The audit trailer a publication carries while the task manifest records an
/// approval. It is written for reviewers only; nothing reads it back.
pub(crate) fn approval_trailer(approval: &CloudUsageApproval) -> String {
    format!(
        "{APPROVAL_TRAILER}: limit_bytes={}; note={}",
        approval.limit_bytes, approval.note
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitObject {
    oid: String,
    name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ObjectInfo {
    kind: String,
    size: u64,
    /// Compressed size in the local object store.
    disk_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LfsObject {
    oid: String,
    size: u64,
}

fn measure_git(repo: &GitRepo, inputs: &UsageInputs<'_>) -> Result<GitUsage> {
    let base_tree = format!("{}^{{tree}}", inputs.remote_base_oid);
    let published = match inputs.remote_target_oid {
        Some(target) => list_objects(
            repo,
            &[target, "--not", inputs.remote_base_oid, base_tree.as_str()],
        )?,
        None => Vec::new(),
    };
    let target_tree = inputs
        .remote_target_oid
        .map(|target| format!("{target}^{{tree}}"));
    let mut revisions = vec![inputs.projected_tree_oid, "--not"];
    revisions.extend(target_tree.as_deref());
    revisions.extend([inputs.remote_base_oid, base_tree.as_str()]);
    let known = published
        .iter()
        .map(|object| object.oid.clone())
        .collect::<BTreeSet<_>>();
    let delta = list_objects(repo, &revisions)?
        .into_iter()
        .filter(|object| !known.contains(&object.oid))
        .collect::<Vec<_>>();
    let changes = raw_changes(
        repo,
        &[
            "diff-tree",
            "-r",
            "-z",
            "--no-color",
            "--no-renames",
            inputs.base_oid(),
            inputs.projected_tree_oid,
        ],
    )?;
    let info = object_info(
        repo,
        published
            .iter()
            .chain(&delta)
            .map(|object| object.oid.as_str()),
    )?;
    let candidates = info
        .iter()
        .filter(|(_, info)| info.kind == "blob" && info.size <= LFS_POINTER_MAX_BYTES)
        .map(|(oid, _)| oid.clone())
        .collect::<Vec<_>>();
    let lfs = lfs_objects(repo, &candidates)?;
    let added = delta
        .iter()
        .map(|object| object.oid.as_str())
        .collect::<BTreeSet<_>>();
    // Rewritten storage metadata is compared with the version it replaces.
    let rewritten = changes
        .iter()
        .filter(|change| {
            change.status != 'D'
                && change.path.ends_with(".dvc")
                && added.contains(change.new_oid.as_str())
        })
        .filter_map(|change| change.old_blob().zip(change.new_blob()))
        .flat_map(|(old, new)| [old.to_owned(), new.to_owned()])
        .collect::<BTreeSet<_>>();
    let mut contents = BTreeMap::new();
    read_blobs(
        repo,
        &rewritten.into_iter().collect::<Vec<_>>(),
        |oid, content| {
            contents.insert(oid.to_owned(), content.to_vec());
            Ok(())
        },
    )?;
    let adds_content = adds_content(&changes, &added, &info, &contents);
    let mut usage = tally_git(published, delta, &info, &lfs);
    usage.adds_content = adds_content;
    Ok(usage)
}

fn tally_git(
    published: Vec<GitObject>,
    delta: Vec<GitObject>,
    info: &BTreeMap<String, ObjectInfo>,
    lfs: &BTreeMap<String, LfsObject>,
) -> GitUsage {
    let mut usage = GitUsage::default();
    let mut published_lfs = BTreeMap::new();
    let mut projected_lfs = BTreeMap::new();
    for (objects, pending) in [(&published, false), (&delta, true)] {
        for object in objects {
            let Some(object_info) = info.get(&object.oid) else {
                continue;
            };
            if pending {
                usage.delta_bytes = usage.delta_bytes.saturating_add(object_info.size);
            } else {
                usage.published_bytes = usage.published_bytes.saturating_add(object_info.size);
            }
            let large = lfs.get(&object.oid);
            if let Some(large) = large {
                if !pending {
                    published_lfs.insert(large.oid.clone(), large.size);
                }
                projected_lfs.insert(large.oid.clone(), large.size);
            }
            if object_info.kind != "blob" {
                continue;
            }
            if let Some(name) = object.name.as_deref().filter(|name| !name.is_empty()) {
                let large_bytes = large.map_or(0, |large| large.size);
                usage
                    .contributors
                    .entry(name.to_owned())
                    .or_default()
                    .add(object_info.size.saturating_add(large_bytes), pending);
                usage
                    .packed_contributors
                    .entry(name.to_owned())
                    .or_default()
                    .add(object_info.disk_size.saturating_add(large_bytes), pending);
            }
        }
    }
    usage.published_lfs_bytes = saturating_sum(published_lfs.values().copied());
    usage.projected_lfs_bytes = saturating_sum(projected_lfs.values().copied());
    usage.published_objects = published;
    usage.delta_objects = delta;
    usage
}

fn saturating_sum(values: impl IntoIterator<Item = u64>) -> u64 {
    values
        .into_iter()
        .fold(0_u64, |total, value| total.saturating_add(value))
}

fn is_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_absent(oid: &str) -> bool {
    oid.bytes().all(|byte| byte == b'0')
}

fn list_objects(repo: &GitRepo, revisions: &[&str]) -> Result<Vec<GitObject>> {
    let mut args = vec!["rev-list", "--objects"];
    args.extend_from_slice(revisions);
    let output = repo.run_bytes(args, None)?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut objects = Vec::new();
    for line in text.split('\n').filter(|line| !line.is_empty()) {
        let (oid, name) = match line.split_once(' ') {
            Some((oid, name)) => (oid, Some(name.to_owned())),
            None => (line, None),
        };
        if !is_object_id(oid) {
            return Err(Error::message(format!(
                "unexpected Git object listing entry {line:?}"
            )));
        }
        objects.push(GitObject {
            oid: oid.to_owned(),
            name,
        });
    }
    Ok(objects)
}

fn object_info<'a>(
    repo: &GitRepo,
    oids: impl Iterator<Item = &'a str>,
) -> Result<BTreeMap<String, ObjectInfo>> {
    let unique = oids.collect::<BTreeSet<_>>();
    if unique.is_empty() {
        return Ok(BTreeMap::new());
    }
    let input = unique
        .iter()
        .map(|oid| format!("{oid}\n"))
        .collect::<String>();
    let output = repo.run_bytes(
        [
            "cat-file",
            "--batch-check=%(objectname) %(objecttype) %(objectsize) %(objectsize:disk)",
        ],
        Some(input.as_bytes()),
    )?;
    let mut info = BTreeMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        match fields.as_slice() {
            [oid, kind, size, disk_size] => {
                let parse = |value: &str| {
                    value.parse::<u64>().map_err(|_| {
                        Error::message(format!("unexpected Git object size in {line:?}"))
                    })
                };
                info.insert(
                    (*oid).to_owned(),
                    ObjectInfo {
                        kind: (*kind).to_owned(),
                        size: parse(size)?,
                        disk_size: parse(disk_size)?,
                    },
                );
            }
            [oid, "missing"] => {
                return Err(Error::message(format!(
                    "Git object {oid} is missing locally; cloud usage cannot be measured"
                )));
            }
            _ => {
                return Err(Error::message(format!(
                    "unexpected Git object size entry {line:?}"
                )));
            }
        }
    }
    Ok(info)
}

/// Streams blob contents through `visit` without decoding them as text.
pub(crate) fn read_blobs(
    repo: &GitRepo,
    oids: &[String],
    mut visit: impl FnMut(&str, &[u8]) -> Result<()>,
) -> Result<()> {
    if oids.is_empty() {
        return Ok(());
    }
    let input = oids
        .iter()
        .map(|oid| format!("{oid}\n"))
        .collect::<String>();
    let mut buffer = Vec::new();
    repo.stream(["cat-file", "--batch"], Some(input.as_bytes()), |chunk| {
        buffer.extend_from_slice(chunk);
        let mut start = 0;
        while let Some(offset) = buffer[start..].iter().position(|byte| *byte == b'\n') {
            let header_end = start + offset;
            let header = String::from_utf8_lossy(&buffer[start..header_end]).into_owned();
            let fields = header.split(' ').collect::<Vec<_>>();
            let (oid, size) = match fields.as_slice() {
                [oid, _, size] => (
                    *oid,
                    size.parse::<usize>().map_err(|_| {
                        Error::message(format!("unexpected Git object header {header:?}"))
                    })?,
                ),
                [oid, "missing"] => {
                    return Err(Error::message(format!(
                        "Git object {oid} is missing locally; cloud usage cannot be measured"
                    )));
                }
                _ => {
                    return Err(Error::message(format!(
                        "unexpected Git object header {header:?}"
                    )));
                }
            };
            let content_start = header_end + 1;
            let Some(record_end) = content_start
                .checked_add(size)
                .and_then(|end| end.checked_add(1))
            else {
                return Err(Error::message("Git object is too large to inspect"));
            };
            if buffer.len() < record_end {
                break;
            }
            visit(oid, &buffer[content_start..record_end - 1])?;
            start = record_end;
        }
        buffer.drain(..start);
        Ok(())
    })?;
    if !buffer.is_empty() {
        return Err(Error::message("Git object stream ended mid-record"));
    }
    Ok(())
}

fn lfs_objects(repo: &GitRepo, candidates: &[String]) -> Result<BTreeMap<String, LfsObject>> {
    let mut found = BTreeMap::new();
    read_blobs(repo, candidates, |oid, content| {
        if let Some(pointer) = lfs_pointer(content) {
            found.insert(oid.to_owned(), pointer);
        }
        Ok(())
    })?;
    Ok(found)
}

fn lfs_pointer(content: &[u8]) -> Option<LfsObject> {
    let text = std::str::from_utf8(content).ok()?;
    let mut lines = text.lines();
    if !matches!(
        lines.next()?,
        "version https://git-lfs.github.com/spec/v1" | "version https://hawser.github.com/spec/v1"
    ) {
        return None;
    }
    let mut oid = None;
    let mut size = None;
    for line in lines {
        if let Some(value) = line.strip_prefix("oid ") {
            oid = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("size ") {
            size = value.trim().parse::<u64>().ok();
        }
    }
    Some(LfsObject {
        oid: oid?,
        size: size?,
    })
}

fn packed_size(repo: &GitRepo, objects: &[GitObject]) -> Result<u64> {
    if objects.is_empty() {
        return Ok(0);
    }
    let input = objects
        .iter()
        .map(|object| match &object.name {
            Some(name) if !name.is_empty() => format!("{} {name}\n", object.oid),
            _ => format!("{}\n", object.oid),
        })
        .collect::<String>();
    let mut bytes = 0_u64;
    let mut args = PACK_SETTINGS.to_vec();
    args.extend(["pack-objects", "--stdout", "-q"]);
    repo.stream(args, Some(input.as_bytes()), |chunk| {
        bytes = bytes.saturating_add(chunk.len() as u64);
        Ok(())
    })?;
    Ok(bytes)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawChange {
    old_mode: String,
    new_mode: String,
    old_oid: String,
    new_oid: String,
    status: char,
    path: String,
}

impl RawChange {
    fn old_blob(&self) -> Option<&str> {
        regular_blob(&self.old_mode, &self.old_oid)
    }

    fn new_blob(&self) -> Option<&str> {
        regular_blob(&self.new_mode, &self.new_oid)
    }
}

fn regular_blob<'a>(mode: &str, oid: &'a str) -> Option<&'a str> {
    (matches!(mode, "100644" | "100755") && !is_absent(oid)).then_some(oid)
}

fn raw_changes(repo: &GitRepo, args: &[&str]) -> Result<Vec<RawChange>> {
    let output = repo.run_bytes(args, None)?;
    parse_raw_changes(&String::from_utf8_lossy(&output.stdout))
}

fn raw_commits(repo: &GitRepo, args: &[&str]) -> Result<Vec<Vec<RawChange>>> {
    let output = repo.run_bytes(args, None)?;
    parse_raw_commits(&String::from_utf8_lossy(&output.stdout))
}

/// Parses `-z --raw` output, skipping the commit IDs that `log` interleaves.
fn parse_raw_changes(output: &str) -> Result<Vec<RawChange>> {
    Ok(parse_raw_commits(output)?.into_iter().flatten().collect())
}

/// Parses `-z --raw` output into one group per commit ID that `log`
/// interleaves. Output without commit IDs, such as `diff-tree`, forms one
/// group.
fn parse_raw_commits(output: &str) -> Result<Vec<Vec<RawChange>>> {
    let mut tokens = output.split('\0');
    let mut commits: Vec<Vec<RawChange>> = Vec::new();
    while let Some(token) = tokens.next() {
        let token = token.trim_start_matches('\n');
        let Some(meta) = token.strip_prefix(':') else {
            if !token.is_empty() {
                commits.push(Vec::new());
            }
            continue;
        };
        let fields = meta.split(' ').collect::<Vec<_>>();
        let [old_mode, new_mode, old_oid, new_oid, status] = fields.as_slice() else {
            return Err(Error::message(format!(
                "unexpected Git change record {meta:?}"
            )));
        };
        let path = tokens
            .next()
            .ok_or_else(|| Error::message("Git change record is missing its path"))?;
        let change = RawChange {
            old_mode: (*old_mode).to_owned(),
            new_mode: (*new_mode).to_owned(),
            old_oid: (*old_oid).to_owned(),
            new_oid: (*new_oid).to_owned(),
            status: status.chars().next().unwrap_or('M'),
            path: path.to_owned(),
        };
        match commits.last_mut() {
            Some(commit) => commit.push(change),
            None => commits.push(vec![change]),
        }
    }
    Ok(commits)
}

fn is_control_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    path.ends_with(".dvc")
        || path.ends_with(PLACEMENT_SUFFIX)
        || name == ".gitignore"
        || name == TASK_MANIFEST_NAME
        || path == CONFIG_NAME
}

/// Whether a publication adds content: any added or changed path other than a
/// workspace-mgr control file, or more than [`CONTROL_FILE_ALLOWANCE_BYTES`]
/// of new control-file content. Only blobs the remote does not hold yet
/// (`added`) are charged, each at its full size, except storage metadata that
/// only drops entries (see [`control_file_charge`]). Every added path is also
/// charged the entry it adds to the Git trees above it, so new directory
/// names cannot carry content either.
fn adds_content(
    changes: &[RawChange],
    added: &BTreeSet<&str>,
    info: &BTreeMap<String, ObjectInfo>,
    contents: &BTreeMap<String, Vec<u8>>,
) -> bool {
    let mut charged = 0_u64;
    for change in changes.iter().filter(|change| change.status != 'D') {
        if !is_control_file(&change.path) {
            return true;
        }
        if change.status == 'A' {
            charged = charged.saturating_add(tree_entry_bytes(&change.path));
        }
        if !added.contains(change.new_oid.as_str()) {
            continue;
        }
        charged = charged.saturating_add(control_file_charge(change, info, contents));
    }
    charged > CONTROL_FILE_ALLOWANCE_BYTES
}

/// An upper bound on the tree bytes a new path adds: its full path, which
/// covers any new directory names above it, plus the mode, separator, and
/// binary object ID of one tree entry.
fn tree_entry_bytes(path: &str) -> u64 {
    path.len() as u64 + 28
}

/// New bytes that one added or changed control file publishes: the whole new
/// blob, because rewriting a file with new content of any size stores that
/// content again. Storage metadata whose entries are a subset of the entries
/// it replaces, with the same object path and version (or digest when no
/// version is recorded), only drops content; it is charged just the lines it
/// does not share with the replaced version, which is nothing for a listing
/// that lost entries and everything for padding such as comments.
fn control_file_charge(
    change: &RawChange,
    info: &BTreeMap<String, ObjectInfo>,
    contents: &BTreeMap<String, Vec<u8>>,
) -> u64 {
    let full = info.get(&change.new_oid).map_or(0, |info| info.size);
    let blob = |oid: Option<&str>| oid.and_then(|oid| contents.get(oid));
    match (blob(change.old_blob()), blob(change.new_blob())) {
        (Some(old), Some(new)) if drops_entries_only(&change.path, old, new) => {
            unshared_line_bytes(old, new).min(full)
        }
        _ => full,
    }
}

/// Whether every entry of the new metadata names an object that the old
/// metadata already names.
fn drops_entries_only(path: &str, old: &[u8], new: &[u8]) -> bool {
    let parse = |content: &[u8]| {
        dvc::parse_pointer_document(&String::from_utf8_lossy(content), path)
            .ok()
            .map(|document| document.entries(path))
    };
    let (Some(old), Some(new)) = (parse(old), parse(new)) else {
        return false;
    };
    let identity = |entry: &PointerEntry| match &entry.version_id {
        Some(version) => (entry.key.clone(), Some(version.clone()), None),
        None => (entry.key.clone(), None, entry.md5.clone()),
    };
    let known = old.iter().map(identity).collect::<BTreeSet<_>>();
    new.iter().all(|entry| known.contains(&identity(entry)))
}

/// Bytes of the lines in `new` that `old` does not contain, counting repeated
/// lines as often as they occur.
fn unshared_line_bytes(old: &[u8], new: &[u8]) -> u64 {
    let mut available: BTreeMap<&[u8], usize> = BTreeMap::new();
    for line in old.split_inclusive(|byte| *byte == b'\n') {
        *available.entry(line).or_default() += 1;
    }
    let mut unshared = 0_u64;
    for line in new.split_inclusive(|byte| *byte == b'\n') {
        match available.get_mut(line) {
            Some(count) if *count > 0 => *count -= 1,
            _ => unshared = unshared.saturating_add(line.len() as u64),
        }
    }
    unshared
}

type EntryRow = (Vec<PointerEntry>, Vec<PointerEntry>);
/// Object paths and the bytes an upload would add at each.
type SizedPaths = Vec<(String, u64)>;

/// Uploads that the publication would perform, keyed by object path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PendingSources {
    /// Automatic-placement candidates and their worktree sizes.
    automatic: SizedPaths,
    /// Every entry of every in-scope worktree metadata file.
    worktree: Vec<PointerEntry>,
    /// Changed outputs and the worktree bytes that would be uploaded.
    dirty: SizedPaths,
    /// Changed directories whose files cannot be resolved, with the whole
    /// directory's current worktree size.
    dirty_aggregates: SizedPaths,
}

/// Resolves directory versions recorded only by their aggregate to the files
/// their manifests list, once per version. Content-addressed remotes store
/// each file by its digest, so accounting per file charges exactly the new
/// digests; a version that cannot be resolved locally keeps its aggregate.
struct DirectoryListings {
    stores: Vec<PathBuf>,
    resolved: RefCell<BTreeMap<String, Option<Rc<Vec<dvc::ListedFile>>>>>,
}

impl DirectoryListings {
    fn new(stores: Vec<PathBuf>) -> Self {
        Self {
            stores,
            resolved: RefCell::new(BTreeMap::new()),
        }
    }

    fn expand(&self, entries: Vec<PointerEntry>) -> Vec<PointerEntry> {
        let mut expanded = Vec::with_capacity(entries.len());
        for entry in entries {
            let listing = entry
                .md5
                .as_deref()
                .filter(|_| entry.aggregate)
                .and_then(|digest| self.listing(digest));
            match listing {
                Some(files) => expanded.extend(files.iter().map(|file| PointerEntry {
                    key: dvc::object_key(&entry.key, &file.relpath),
                    md5: Some(file.md5.clone()),
                    size: Some(file.size),
                    version_id: None,
                    etag: None,
                    aggregate: false,
                })),
                None => expanded.push(entry),
            }
        }
        expanded
    }

    fn listing(&self, digest: &str) -> Option<Rc<Vec<dvc::ListedFile>>> {
        self.resolved
            .borrow_mut()
            .entry(digest.to_owned())
            .or_insert_with(|| dvc::directory_listing(&self.stores, digest).map(Rc::new))
            .clone()
    }
}

fn measure_storage(
    repo: &GitRepo,
    config: &Config,
    inputs: &UsageInputs<'_>,
    cache: &mut UsageCache,
) -> Result<StorageUsage> {
    let base = inputs.base_oid();
    let history = match inputs.remote_target_oid {
        Some(target) => raw_commits(
            repo,
            &[
                "log",
                "--reverse",
                "--full-history",
                "--no-color",
                "--raw",
                "-z",
                "--no-renames",
                "--no-abbrev",
                "--format=%H",
                target,
                "--not",
                inputs.remote_base_oid,
                "--",
                "*.dvc",
            ],
        )?,
        None => Vec::new(),
    };
    let worktree = inputs
        .pointers
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let projected = raw_changes(
        repo,
        &[
            "diff-tree",
            "-r",
            "-z",
            "--no-color",
            "--no-renames",
            base,
            inputs.projected_tree_oid,
            "--",
            "*.dvc",
        ],
    )?
    .into_iter()
    .filter(|change| change.path.ends_with(".dvc") && !worktree.contains(change.path.as_str()))
    .collect::<Vec<_>>();
    let history = history
        .into_iter()
        .map(|commit| {
            commit
                .into_iter()
                .filter(|change| change.path.ends_with(".dvc"))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let base_blobs = blobs_at(repo, base, inputs.pointers)?;
    let wanted = history
        .iter()
        .flatten()
        .chain(&projected)
        .flat_map(|change| [change.old_blob(), change.new_blob()])
        .flatten()
        .chain(base_blobs.values().flatten().map(String::as_str))
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>();
    cache.load_documents(repo, &wanted)?;
    let documents = &cache.pointers;
    let version_aware = config.requires_object_versioning();
    let listings =
        (!version_aware).then(|| DirectoryListings::new(dvc::local_object_stores(repo, config)));
    let expand = |entries: Vec<PointerEntry>| match &listings {
        Some(listings) => listings.expand(entries),
        None => entries,
    };
    let entries = |oid: Option<&str>, path: &str| -> Vec<PointerEntry> {
        expand(
            oid.and_then(|oid| documents.get(oid))
                .map_or_else(Vec::new, |document| document.entries(path)),
        )
    };
    let rows = |changes: &[RawChange]| -> Vec<EntryRow> {
        changes
            .iter()
            .map(|change| {
                (
                    entries(change.old_blob(), &change.path),
                    entries(change.new_blob(), &change.path),
                )
            })
            .collect()
    };
    let history_commits = history
        .iter()
        .map(|commit| rows(commit))
        .collect::<Vec<_>>();
    let mut projection_rows = rows(&projected);
    let mut sources = PendingSources::default();
    for pointer in inputs.pointers {
        let current = expand(dvc::read_pointer_document(repo, pointer)?.entries(pointer));
        let previous = entries(
            base_blobs.get(pointer).and_then(|oid| oid.as_deref()),
            pointer,
        );
        sources.worktree.extend(current.iter().cloned());
        projection_rows.push((previous, current));
    }
    for path in inputs.automatic_s3 {
        let absolute = resolved_under(&repo.root, path);
        let size = fs::metadata(&absolute).at(&absolute)?.len();
        sources.automatic.push((path.clone(), size));
    }
    if inputs.inspect_outputs && config.s3_enabled() && !inputs.pointers.is_empty() {
        let outputs = inputs
            .pointers
            .iter()
            .filter_map(|pointer| pointer.strip_suffix(".dvc"))
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        let status = dvc::data_status(repo, &outputs)?;
        (sources.dirty, sources.dirty_aggregates) =
            dirty_uploads(&repo.root, &status, &sources.worktree, version_aware);
    }
    Ok(if version_aware {
        versioned_storage(&history_commits, &projection_rows, &sources)
    } else {
        content_storage(&history_commits, &projection_rows, &sources)
    })
}

/// Resolves `<revision>:<path>` for each path to a blob ID, if present.
fn blobs_at(
    repo: &GitRepo,
    revision: &str,
    paths: &[String],
) -> Result<BTreeMap<String, Option<String>>> {
    if paths.is_empty() {
        return Ok(BTreeMap::new());
    }
    let input = paths
        .iter()
        .map(|path| format!("{revision}:{path}\n"))
        .collect::<String>();
    let output = repo.run_bytes(
        ["cat-file", "--batch-check=%(objectname) %(objecttype)"],
        Some(input.as_bytes()),
    )?;
    let text = String::from_utf8_lossy(&output.stdout);
    let lines = text.lines().collect::<Vec<_>>();
    if lines.len() != paths.len() {
        return Err(Error::message(
            "unexpected Git object lookup output while measuring cloud usage",
        ));
    }
    Ok(paths
        .iter()
        .zip(lines)
        .map(|(path, line)| {
            let blob = match line.split_once(' ') {
                Some((oid, "blob")) if is_object_id(oid) => Some(oid.to_owned()),
                _ => None,
            };
            (path.clone(), blob)
        })
        .collect())
}

/// Sizes the uploads implied by uncommitted output changes. Returns changed
/// paths and, separately, changed directories that are still recorded only by
/// their aggregate: any change beneath such a directory, including a deletion,
/// makes the storage engine record a new version of it, sized as a whole.
/// Directories resolved to their files are sized per changed file, since an
/// added or modified file becomes a new object.
///
/// Missing outputs are never read as reductions, and sizing a changed path
/// never fails the measurement.
fn dirty_uploads(
    root: &Path,
    status: &DataStatus,
    worktree: &[PointerEntry],
    version_aware: bool,
) -> (SizedPaths, SizedPaths) {
    let aggregates = worktree
        .iter()
        .filter(|entry| entry.aggregate)
        .collect::<Vec<_>>();
    let recorded = worktree
        .iter()
        .map(|entry| (entry.key.as_str(), entry.size))
        .collect::<BTreeMap<_, _>>();
    let changes = &status.uncommitted;
    // Renamed content is stored again only at a new version-aware path.
    let renamed = changes
        .renamed
        .iter()
        .filter(|_| version_aware)
        .map(|rename| &rename.new);
    let uploaded = changes
        .modified
        .iter()
        .chain(&changes.added)
        .chain(&changes.unknown)
        .chain(renamed)
        .map(|path| (path, true));
    let removed = changes
        .deleted
        .iter()
        .chain(
            changes
                .renamed
                .iter()
                .flat_map(|rename| [&rename.old, &rename.new]),
        )
        .map(|path| (path, false));
    let mut uploads = BTreeMap::new();
    let mut directories = BTreeMap::new();
    for (path, upload) in uploaded.chain(removed) {
        if path.ends_with('/') {
            continue;
        }
        match aggregates.iter().find(|entry| beneath(path, &entry.key)) {
            Some(aggregate) => {
                if directories.contains_key(&aggregate.key) {
                    continue;
                }
                if let Some(bytes) = upload_bytes(root, &aggregate.key, aggregate.size) {
                    directories.insert(aggregate.key.clone(), bytes);
                }
            }
            None if upload && !uploads.contains_key(path) => {
                let previous = recorded.get(path.as_str()).copied().flatten();
                if let Some(bytes) = upload_bytes(root, path, previous) {
                    uploads.insert(path.clone(), bytes);
                }
            }
            None => {}
        }
    }
    (
        uploads.into_iter().collect(),
        directories.into_iter().collect(),
    )
}

/// Sizes a changed path the way the storage engine reads it: a symlinked
/// file counts its target, and a directory counts every file beneath it
/// without entering linked directories, which the engine skips. A missing
/// path or a dangling link uploads nothing. A path that cannot be inspected
/// keeps its recorded size; the storage engine reports the problem when it
/// commits it.
fn upload_bytes(root: &Path, key: &str, recorded: Option<u64>) -> Option<u64> {
    let absolute = resolved_under(root, key);
    let metadata = match fs::metadata(&absolute) {
        Ok(metadata) => metadata,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return None;
        }
        Err(_) => return Some(recorded.unwrap_or(0)),
    };
    if !metadata.is_dir() {
        return Some(metadata.len());
    }
    let linked =
        fs::symlink_metadata(&absolute).is_ok_and(|metadata| metadata.file_type().is_symlink());
    (!linked).then(|| directory_bytes(&absolute))
}

/// Total size of the files beneath a directory, counting symlinked files by
/// their targets. Linked directories and entries that cannot be read, such
/// as dangling links, count as empty.
fn directory_bytes(root: &Path) -> u64 {
    WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != ".git")
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| {
            let kind = entry.file_type();
            if kind.is_file() {
                entry.metadata().ok()
            } else if kind.is_symlink() {
                fs::metadata(entry.path())
                    .ok()
                    .filter(std::fs::Metadata::is_file)
            } else {
                None
            }
        })
        .fold(0_u64, |total, metadata| {
            total.saturating_add(metadata.len())
        })
}

fn beneath(path: &str, boundary: &str) -> bool {
    path == boundary
        || path
            .strip_prefix(boundary)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Version-aware remotes keep every version at a live object path until a
/// publication retires the path and purges all of its versions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct VersionLedger {
    keys: BTreeMap<String, BTreeMap<String, u64>>,
}

impl VersionLedger {
    /// Applies the metadata changes of one commit, or of the whole
    /// projection, together. Like the purge after publication, a path is
    /// retired only when no metadata file still names it, so an object path
    /// that moves between metadata files keeps every stored version.
    fn apply(&mut self, rows: &[EntryRow]) {
        let old = || rows.iter().flat_map(|(old, _)| old);
        let new = || rows.iter().flat_map(|(_, new)| new);
        let live = new()
            .map(|entry| entry.key.as_str())
            .collect::<BTreeSet<_>>();
        // A directory recorded only by its aggregate does not list its files,
        // so none of the paths beneath it can be shown to be retired.
        let aggregates = new()
            .filter(|entry| entry.aggregate)
            .map(|entry| entry.key.as_str())
            .collect::<Vec<_>>();
        for entry in old() {
            let key = entry.key.as_str();
            let covered = aggregates.iter().any(|boundary| beneath(key, boundary));
            if !live.contains(key) && !covered {
                self.keys.remove(key);
            }
        }
        let inherited = old()
            .filter_map(|entry| {
                entry
                    .version_id
                    .as_deref()
                    .map(|version| (entry.key.as_str(), version))
            })
            .collect::<BTreeSet<_>>();
        for entry in new() {
            let Some(version) = entry.version_id.as_deref() else {
                continue;
            };
            if inherited.contains(&(entry.key.as_str(), version)) {
                continue;
            }
            self.keys
                .entry(entry.key.clone())
                .or_default()
                .insert(version.to_owned(), entry.size.unwrap_or(0));
        }
    }

    fn total(&self) -> u64 {
        saturating_sum(
            self.keys
                .values()
                .flat_map(|versions| versions.values().copied()),
        )
    }

    fn contains(&self, key: &str, version: &str) -> bool {
        self.keys
            .get(key)
            .is_some_and(|versions| versions.contains_key(version))
    }
}

fn versioned_storage(
    history: &[Vec<EntryRow>],
    projection: &[EntryRow],
    sources: &PendingSources,
) -> StorageUsage {
    let mut ledger = VersionLedger::default();
    for commit in history {
        ledger.apply(commit);
    }
    let published = ledger.clone();
    ledger.apply(projection);
    let mut pending: BTreeMap<String, u64> = BTreeMap::new();
    for entry in &sources.worktree {
        if entry.version_id.is_none() {
            let bytes = pending.entry(entry.key.clone()).or_default();
            *bytes = bytes.saturating_add(entry.size.unwrap_or(0));
        }
    }
    pending.extend(sources.dirty.iter().cloned());
    // An aggregate-only directory is uploaded whole.
    pending.extend(sources.dirty_aggregates.iter().cloned());
    pending.extend(sources.automatic.iter().cloned());
    let mut contributors: BTreeMap<String, Tally> = BTreeMap::new();
    for (key, versions) in &ledger.keys {
        for (version, size) in versions {
            let unpublished = !published.contains(key, version);
            contributors
                .entry(key.clone())
                .or_default()
                .add(*size, unpublished);
        }
    }
    for (key, size) in &pending {
        contributors
            .entry(key.clone())
            .or_default()
            .add(*size, true);
    }
    StorageUsage {
        published_bytes: published.total(),
        projected_bytes: ledger
            .total()
            .saturating_add(saturating_sum(pending.values().copied())),
        pending_uploads: !pending.is_empty(),
        contributors,
    }
}

/// Non-version-aware remotes are content-addressed and never purged, so each
/// distinct digest is stored once.
///
/// Directories are accounted per file wherever their manifests resolve
/// locally. At the gate, before the storage engine records a directory's new
/// version, each added or modified file beneath it is charged its worktree
/// size; after the engine commits it, and when the published commit is
/// replayed, the new version charges exactly the digests not stored before.
/// For new content these agree, and a deletion charges nothing. A directory
/// version that cannot be resolved is charged whole under its own digest.
fn content_storage(
    history: &[Vec<EntryRow>],
    projection: &[EntryRow],
    sources: &PendingSources,
) -> StorageUsage {
    let mut stored: BTreeMap<String, (String, u64)> = BTreeMap::new();
    for entry in history.iter().flatten().flat_map(|(_, new)| new) {
        if let Some(md5) = &entry.md5 {
            stored
                .entry(md5.clone())
                .or_insert_with(|| (entry.key.clone(), entry.size.unwrap_or(0)));
        }
    }
    let mut seen = stored.keys().cloned().collect::<BTreeSet<_>>();
    let inherited = projection
        .iter()
        .flat_map(|(old, _)| old)
        .filter_map(|entry| entry.md5.as_deref())
        .collect::<BTreeSet<_>>();
    let mut pending: BTreeMap<String, u64> = BTreeMap::new();
    for entry in projection.iter().flat_map(|(_, new)| new) {
        let Some(md5) = entry.md5.as_deref() else {
            continue;
        };
        if inherited.contains(md5) || !seen.insert(md5.to_owned()) {
            continue;
        }
        let bytes = pending.entry(entry.key.clone()).or_default();
        *bytes = bytes.saturating_add(entry.size.unwrap_or(0));
    }
    // Changed outputs replace what their stale metadata would upload.
    pending.extend(sources.dirty.iter().cloned());
    pending.extend(sources.dirty_aggregates.iter().cloned());
    pending.extend(sources.automatic.iter().cloned());
    let mut contributors: BTreeMap<String, Tally> = BTreeMap::new();
    for (key, size) in stored.values() {
        contributors
            .entry(key.clone())
            .or_default()
            .add(*size, false);
    }
    for (key, size) in &pending {
        contributors
            .entry(key.clone())
            .or_default()
            .add(*size, true);
    }
    let published = saturating_sum(stored.values().map(|(_, size)| *size));
    StorageUsage {
        published_bytes: published,
        projected_bytes: published.saturating_add(saturating_sum(pending.values().copied())),
        pending_uploads: !pending.is_empty(),
        contributors,
    }
}

/// Parsed metadata per immutable blob plus the packed size of the published
/// history. The cache only saves work; any read or write problem is ignored.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct UsageCache {
    schema_version: u32,
    #[serde(default)]
    pointers: BTreeMap<String, PointerDocument>,
    #[serde(default)]
    packed: Option<PackedCache>,
    #[serde(skip)]
    used: Option<BTreeSet<String>>,
    #[serde(skip)]
    changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PackedCache {
    remote_target_oid: String,
    remote_base_oid: String,
    bytes: u64,
}

impl UsageCache {
    fn load(state_dir: &Path) -> Self {
        fs::read_to_string(state_dir.join(CACHE_NAME))
            .ok()
            .and_then(|raw| serde_json::from_str::<Self>(&raw).ok())
            .filter(|cache| cache.schema_version == CACHE_SCHEMA)
            .unwrap_or_else(|| Self {
                schema_version: CACHE_SCHEMA,
                ..Self::default()
            })
    }

    fn save(mut self, state_dir: &Path) {
        if let Some(used) = &self.used {
            let before = self.pointers.len();
            self.pointers.retain(|oid, _| used.contains(oid));
            self.changed |= self.pointers.len() != before;
        }
        if !self.changed {
            return;
        }
        if let Ok(encoded) = serde_json::to_vec(&self) {
            let _ = write_atomic(state_dir, &state_dir.join(CACHE_NAME), &encoded);
        }
    }

    fn load_documents(&mut self, repo: &GitRepo, oids: &BTreeSet<String>) -> Result<()> {
        let missing = oids
            .iter()
            .filter(|oid| !self.pointers.contains_key(*oid))
            .cloned()
            .collect::<Vec<_>>();
        let mut parsed = BTreeMap::new();
        read_blobs(repo, &missing, |oid, content| {
            let raw = String::from_utf8_lossy(content);
            parsed.insert(
                oid.to_owned(),
                dvc::parse_pointer_document(&raw, &format!("in Git blob {oid}"))?,
            );
            Ok(())
        })?;
        self.changed |= !parsed.is_empty();
        self.pointers.extend(parsed);
        self.used
            .get_or_insert_with(BTreeSet::new)
            .extend(oids.iter().cloned());
        Ok(())
    }

    fn packed_published(&self, target: &str, base: &str) -> Option<u64> {
        self.packed
            .as_ref()
            .filter(|packed| packed.remote_target_oid == target && packed.remote_base_oid == base)
            .map(|packed| packed.bytes)
    }

    fn record_packed_published(&mut self, target: &str, base: &str, bytes: u64) {
        self.packed = Some(PackedCache {
            remote_target_oid: target.to_owned(),
            remote_base_oid: base.to_owned(),
            bytes,
        });
        self.changed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::S3Config;

    fn entry(key: &str, size: u64, version: Option<&str>) -> PointerEntry {
        PointerEntry {
            key: key.to_owned(),
            md5: Some(format!("md5-{key}-{}", version.unwrap_or("none"))),
            size: Some(size),
            version_id: version.map(ToOwned::to_owned),
            etag: None,
            aggregate: false,
        }
    }

    fn content(key: &str, md5: &str, size: u64) -> PointerEntry {
        PointerEntry {
            key: key.to_owned(),
            md5: Some(md5.to_owned()),
            size: Some(size),
            version_id: None,
            etag: None,
            aggregate: false,
        }
    }

    fn change(status: char, path: &str) -> RawChange {
        RawChange {
            old_mode: "100644".to_owned(),
            new_mode: "100644".to_owned(),
            old_oid: "1".repeat(40),
            new_oid: "2".repeat(40),
            status,
            path: path.to_owned(),
        }
    }

    fn measurement(
        published_git: u64,
        delta_git: u64,
        published_s3: u64,
        projected_s3: u64,
    ) -> Measurement {
        Measurement {
            git: GitUsage {
                published_bytes: published_git,
                delta_bytes: delta_git,
                ..GitUsage::default()
            },
            storage: StorageUsage {
                published_bytes: published_s3,
                projected_bytes: projected_s3,
                ..StorageUsage::default()
            },
            packed: None,
        }
    }

    fn approval(limit_bytes: u64) -> CloudUsageApproval {
        CloudUsageApproval {
            limit_bytes,
            note: "User approved in chat".to_owned(),
        }
    }

    #[test]
    fn sizes_accept_byte_counts_and_decimal_or_binary_units() {
        for (raw, expected) in [
            ("0", 0),
            ("1000000000", 1_000_000_000),
            ("12B", 12),
            ("1GB", 1_000_000_000),
            ("1 GB", 1_000_000_000),
            ("  2 gb  ", 2_000_000_000),
            ("1.5 GB", 1_500_000_000),
            ("2.25Gb", 2_250_000_000),
            ("1.25 KB", 1_250),
            ("1.000 KB", 1_000),
            ("1.0B", 1),
            ("3 MB", 3_000_000),
            ("1 TB", 1_000_000_000_000),
            ("3GiB", 3 << 30),
            ("1.5 kib", 1_536),
            ("0.5 KiB", 512),
            ("2  MiB", 2 << 20),
            ("1.5 TiB", 1_649_267_441_664),
            ("18446744073709551615", u64::MAX),
            (
                "1.000000000000000000000000000000000000000000000000GB",
                1_000_000_000,
            ),
        ] {
            assert_eq!(parse_size(raw), Ok(expected), "{raw:?}");
        }
    }

    #[test]
    fn sizes_reject_ambiguous_fractional_and_oversized_values() {
        for raw in [
            "",
            "GB",
            "1K",
            "1 k",
            "1M",
            "1G",
            "1T",
            "1 kbit",
            "-1GB",
            "+1GB",
            "1e9",
            "1,000",
            "1..5GB",
            "1.GB",
            ".5GB",
            "1.5",
            "1.0",
            "1.5B",
            "1.0001 KB",
            "0.3 KiB",
            "1 GB extra",
            "18446744073709551616",
            "18446744073709551.616 KB",
            "20000000 TB",
            "999999999999999999999999999999999999999999 GB",
        ] {
            assert!(parse_size(raw).is_err(), "{raw:?} was accepted");
        }
        assert!(
            parse_size("1.5B")
                .unwrap_err()
                .contains("whole number of bytes")
        );
        assert!(parse_size("1.5").unwrap_err().contains("needs a unit"));
        assert!(parse_size("20000000 TB").unwrap_err().contains("too large"));
    }

    #[test]
    fn byte_formatting_uses_binary_units_without_rounding_up() {
        assert_eq!(format_bytes(0), "0 bytes");
        assert_eq!(format_bytes(1), "1 byte");
        assert_eq!(format_bytes(1_023), "1023 bytes");
        assert_eq!(format_bytes(1_000), "1000 bytes");
        assert_eq!(format_bytes(1_024), "1 KiB (1024 bytes)");
        assert_eq!(format_bytes(1_048_576), "1 MiB (1048576 bytes)");
        assert_eq!(format_bytes(1_073_741_824), "1 GiB (1073741824 bytes)");
        assert_eq!(format_bytes(1_342_177_280), "1.25 GiB (1342177280 bytes)");
        assert_eq!(format_bytes(1_288_490_189), "1.2 GiB (1288490189 bytes)");
        assert_eq!(format_bytes(1_127_428_916), "1.05 GiB (1127428916 bytes)");
        assert_eq!(format_bytes(1_127_428_915), "1.04 GiB (1127428915 bytes)");
        assert_eq!(format_bytes(2_147_483_647), "1.99 GiB (2147483647 bytes)");
        assert_eq!(format_bytes(1_000_000_000), "953.67 MiB (1000000000 bytes)");
        assert_eq!(format_bytes(1 << 40), "1 TiB (1099511627776 bytes)");
        assert_eq!(
            format_bytes(u64::MAX),
            "16777215.99 TiB (18446744073709551615 bytes)"
        );
    }

    #[cfg(feature = "test-storage")]
    #[test]
    fn test_build_threshold_override_can_only_lower_the_threshold() {
        assert_eq!(threshold_from_override(None), CLOUD_USAGE_APPROVAL_BYTES);
        assert_eq!(threshold_from_override(Some("15000000")), 15_000_000);
        assert_eq!(threshold_from_override(Some(" 0 ")), 0);
        assert_eq!(
            threshold_from_override(Some("5000000000")),
            CLOUD_USAGE_APPROVAL_BYTES
        );
        assert_eq!(
            threshold_from_override(Some("1 GB")),
            CLOUD_USAGE_APPROVAL_BYTES
        );
    }

    #[cfg(not(feature = "test-storage"))]
    #[test]
    fn production_build_ignores_cloud_usage_threshold_override() {
        assert_eq!(
            threshold_from_override(Some("1")),
            CLOUD_USAGE_APPROVAL_BYTES
        );
    }

    #[test]
    fn approvals_raise_but_never_lower_the_limit() {
        assert_eq!(effective_limit(1_000, None), 1_000);
        assert_eq!(effective_limit(1_000, Some(&approval(3_000))), 3_000);
        assert_eq!(effective_limit(1_000, Some(&approval(500))), 1_000);
    }

    fn row(old: &[PointerEntry], new: &[PointerEntry]) -> EntryRow {
        (old.to_vec(), new.to_vec())
    }

    #[test]
    fn version_history_replay_retires_removed_paths_and_keeps_superseded_versions() {
        let first = vec![
            entry("T/model.pt", 100, Some("m1")),
            entry("T/dir/a", 10, Some("a1")),
            entry("T/dir/b", 20, Some("b1")),
        ];
        let second = vec![
            entry("T/model.pt", 150, Some("m2")),
            entry("T/dir/a", 10, Some("a1")),
        ];
        let mut ledger = VersionLedger::default();
        ledger.apply(&[row(&[], &first)]);
        assert_eq!(ledger.total(), 130);
        ledger.apply(&[row(&first, &second)]);
        assert_eq!(ledger.total(), 260);
        assert!(ledger.contains("T/model.pt", "m1"));
        assert!(!ledger.keys.contains_key("T/dir/b"));

        // Removing the path purges every version stored under it.
        ledger.apply(&[row(&second[..1], &[])]);
        assert_eq!(ledger.total(), 10);
        // Re-adding the path counts only the new upload.
        ledger.apply(&[row(&[], &[entry("T/model.pt", 70, Some("m3"))])]);
        assert_eq!(ledger.total(), 80);
    }

    #[test]
    fn version_replay_ignores_versions_inherited_from_the_base() {
        let base = vec![entry("shared/data", 500, Some("s1"))];
        let changed = vec![
            entry("shared/data", 500, Some("s1")),
            entry("shared/extra", 40, Some("e1")),
        ];
        let mut ledger = VersionLedger::default();
        ledger.apply(&[row(&base, &changed)]);
        assert_eq!(ledger.total(), 40);
        ledger.apply(&[row(&base, &[])]);
        assert_eq!(ledger.total(), 40);
    }

    #[test]
    fn aggregate_directory_metadata_does_not_retire_its_files() {
        let files = vec![
            entry("T/dir/a", 10, Some("a1")),
            entry("T/dir/b", 20, Some("b1")),
        ];
        let mut aggregate = entry("T/dir", 35, None);
        aggregate.aggregate = true;
        let mut ledger = VersionLedger::default();
        ledger.apply(&[row(&[], &files)]);
        ledger.apply(&[row(&files, std::slice::from_ref(&aggregate))]);
        assert_eq!(ledger.total(), 30);
    }

    #[test]
    fn object_paths_that_move_between_metadata_files_keep_their_versions() {
        let a1 = entry("T/dir/a.bin", 400, Some("a1"));
        let b1 = entry("T/dir/b.bin", 400, Some("b1"));
        let a2 = entry("T/dir/a.bin", 400, Some("a2"));
        let b2 = entry("T/dir/b.bin", 400, Some("b2"));

        // Consolidation: per-file metadata becomes one directory listing in a
        // commit whose rows list the new directory before the deletions.
        let history = vec![
            vec![
                row(&[], std::slice::from_ref(&a1)),
                row(&[], std::slice::from_ref(&b1)),
            ],
            vec![
                row(&[], &[a2.clone(), b2.clone()]),
                row(std::slice::from_ref(&a1), &[]),
                row(std::slice::from_ref(&b1), &[]),
            ],
        ];
        let usage = versioned_storage(&history, &[], &PendingSources::default());
        assert_eq!(usage.published_bytes, 1_600);
        assert_eq!(usage.contributors["T/dir/a.bin"].versions, 2);

        // Split: the directory listing becomes per-file metadata.
        let history = vec![
            vec![row(&[], &[a1.clone(), b1.clone()])],
            vec![
                row(&[a1.clone(), b1.clone()], &[]),
                row(&[], std::slice::from_ref(&a2)),
                row(&[], std::slice::from_ref(&b2)),
            ],
        ];
        let usage = versioned_storage(&history, &[], &PendingSources::default());
        assert_eq!(usage.published_bytes, 1_600);

        // Move plus reuse: the directory moves to T/dir2 and a new file takes
        // over one of its former object paths.
        let moved = vec![
            entry("T/dir2/a.bin", 400, Some("m1")),
            entry("T/dir2/b.bin", 400, Some("m2")),
        ];
        let reused = entry("T/dir/a.bin", 300, Some("a3"));
        let first = vec![vec![row(&[], &[a1.clone(), b1.clone()])]];
        let restructure = vec![
            row(&[a1.clone(), b1.clone()], &[]),
            row(&[], std::slice::from_ref(&reused)),
            row(&[], &moved),
        ];
        let mut history = first.clone();
        history.push(restructure.clone());
        let usage = versioned_storage(&history, &[], &PendingSources::default());
        assert_eq!(usage.published_bytes, 1_500);
        assert!(!usage.contributors.contains_key("T/dir/b.bin"));

        // The same restructure as a pending projection.
        let usage = versioned_storage(&first, &restructure, &PendingSources::default());
        assert_eq!(usage.published_bytes, 800);
        assert_eq!(usage.projected_bytes, 1_500);
        assert_eq!(
            usage.contributors["T/dir/a.bin"],
            Tally {
                bytes: 700,
                versions: 2,
                pending: true
            }
        );
    }

    #[test]
    fn versioned_projection_counts_pending_uploads_once_per_path() {
        let history = vec![
            vec![(
                Vec::new(),
                vec![
                    entry("T/model.pt", 100, Some("m1")),
                    entry("T/old.bin", 40, Some("o1")),
                ],
            )],
            vec![(
                vec![entry("T/model.pt", 100, Some("m1"))],
                vec![entry("T/model.pt", 150, Some("m2"))],
            )],
        ];
        let current = vec![
            entry("T/model.pt", 150, Some("m2")),
            entry("T/run1/model.bin", 70, None),
            entry("T/run2/model.bin", 70, None),
            entry("T/stale.bin", 5, None),
        ];
        let projection = vec![
            (vec![entry("T/old.bin", 40, Some("o1"))], Vec::new()),
            (vec![entry("T/model.pt", 150, Some("m2"))], current.clone()),
        ];
        let sources = PendingSources {
            automatic: vec![("T/auto.bin".to_owned(), 33)],
            worktree: current,
            dirty: vec![("T/stale.bin".to_owned(), 9)],
            dirty_aggregates: Vec::new(),
        };
        let usage = versioned_storage(&history, &projection, &sources);
        assert_eq!(usage.published_bytes, 290);
        assert_eq!(usage.projected_bytes, 250 + 70 + 70 + 9 + 33);
        assert!(usage.pending_uploads);
        assert_eq!(
            usage.contributors["T/model.pt"],
            Tally {
                bytes: 250,
                versions: 2,
                pending: false
            }
        );
        assert!(usage.contributors["T/run2/model.bin"].pending);
        assert!(!usage.contributors.contains_key("T/old.bin"));

        let quiet = versioned_storage(
            &history,
            &[(vec![entry("T/old.bin", 40, Some("o1"))], Vec::new())],
            &PendingSources::default(),
        );
        assert_eq!(quiet.projected_bytes, 250);
        assert!(!quiet.pending_uploads);
    }

    #[test]
    fn content_addressed_storage_counts_each_digest_once() {
        let history = vec![
            vec![(
                Vec::new(),
                vec![content("T/a", "aaa", 100), content("T/copy", "aaa", 100)],
            )],
            vec![(vec![content("T/a", "aaa", 100)], Vec::new())],
        ];
        let projection = vec![
            (
                vec![content("shared/base", "bbb", 7)],
                vec![
                    content("shared/base", "bbb", 7),
                    content("T/new", "ccc", 30),
                ],
            ),
            (Vec::new(), vec![content("T/again", "aaa", 100)]),
            (Vec::new(), vec![content("T/twin", "ccc", 30)]),
        ];
        let usage = content_storage(&history, &projection, &PendingSources::default());
        assert_eq!(usage.published_bytes, 100);
        assert_eq!(usage.projected_bytes, 130);
        assert!(usage.pending_uploads);

        let clean = content_storage(&history, &projection[1..2], &PendingSources::default());
        assert_eq!(clean.projected_bytes, 100);
        assert!(!clean.pending_uploads);
    }

    fn aggregate(key: &str, md5: &str, size: u64) -> PointerEntry {
        PointerEntry {
            aggregate: true,
            ..content(key, &format!("{md5}.dir"), size)
        }
    }

    #[test]
    fn content_addressed_directories_are_charged_per_file_everywhere() {
        let a1 = content("T/data/a.bin", "a1", 5_000);
        let b1 = content("T/data/b.bin", "b1", 5_000);
        let c1 = content("T/data/c.bin", "c1", 1_000);
        let first = vec![a1.clone(), b1.clone(), c1.clone()];
        let history = vec![vec![row(&[], &first)]];
        let unchanged = vec![row(&first, &first)];
        let gate = |dirty: &[(&str, u64)]| {
            content_storage(
                &history,
                &unchanged,
                &PendingSources {
                    worktree: first.clone(),
                    dirty: dirty
                        .iter()
                        .map(|(key, size)| ((*key).to_owned(), *size))
                        .collect(),
                    ..PendingSources::default()
                },
            )
        };
        let recheck = |files: &[PointerEntry]| {
            content_storage(
                &history,
                &[row(&first, files)],
                &PendingSources {
                    worktree: files.to_vec(),
                    ..PendingSources::default()
                },
            )
        };
        let replay = |files: &[PointerEntry]| {
            let mut published = history.clone();
            published.push(vec![row(&first, files)]);
            content_storage(&published, &[row(files, files)], &PendingSources::default())
        };
        let agree = |dirty: &[(&str, u64)], files: &[PointerEntry], projected: u64| {
            let (gate, recheck, replay) = (gate(dirty), recheck(files), replay(files));
            assert_eq!(gate.published_bytes, 11_000);
            assert_eq!(gate.projected_bytes, projected, "gate");
            assert_eq!(recheck.projected_bytes, projected, "re-check");
            assert_eq!(replay.published_bytes, projected, "replay");
            assert_eq!(replay.projected_bytes, projected, "replay");
            assert!(!replay.pending_uploads);
            (gate, recheck)
        };

        // Adding a file charges exactly the file.
        let notes = content("T/data/notes.txt", "n1", 6);
        let mut added = first.clone();
        added.push(notes);
        let (gate_usage, recheck_usage) = agree(&[("T/data/notes.txt", 6)], &added, 11_006);
        assert!(gate_usage.pending_uploads && recheck_usage.pending_uploads);

        // A same-size rewrite and a shrink store new content.
        let rewritten = vec![content("T/data/a.bin", "a2", 5_000), b1.clone(), c1.clone()];
        agree(&[("T/data/a.bin", 5_000)], &rewritten, 16_000);
        let shrunk = vec![content("T/data/a.bin", "a3", 3_000), b1.clone(), c1.clone()];
        agree(&[("T/data/a.bin", 3_000)], &shrunk, 14_000);
        let usage = replay(&shrunk);
        assert_eq!(usage.contributors["T/data/a.bin"].versions, 2);
        assert_eq!(usage.contributors["T/data/a.bin"].bytes, 8_000);

        // A deletion stores nothing and uploads nothing.
        let deleted = vec![a1.clone(), b1.clone()];
        let (gate_usage, recheck_usage) = agree(&[], &deleted, 11_000);
        assert!(!gate_usage.pending_uploads && !recheck_usage.pending_uploads);

        // Content restored from an earlier digest is not stored again.
        let restored = content("T/data/c.bin", "a1", 5_000);
        assert_eq!(
            recheck(&[a1.clone(), b1.clone(), restored]).projected_bytes,
            11_000
        );

        // A directory version that cannot be resolved is charged whole.
        let unresolved = aggregate("T/data", "d9", 12_000);
        let usage = content_storage(
            &[],
            &[row(&[], std::slice::from_ref(&unresolved))],
            &PendingSources {
                dirty_aggregates: vec![("T/data".to_owned(), 12_500)],
                ..PendingSources::default()
            },
        );
        assert_eq!(usage.projected_bytes, 12_500);
        let usage = content_storage(
            &[vec![row(&[], std::slice::from_ref(&unresolved))]],
            &[row(std::slice::from_ref(&unresolved), &first)],
            &PendingSources::default(),
        );
        assert_eq!(usage.published_bytes, 12_000);
        assert_eq!(usage.projected_bytes, 23_000);
    }

    fn sized(sizes: &[(&str, u64)], added: &[&str]) -> (BTreeMap<String, ObjectInfo>, Vec<String>) {
        let info = sizes
            .iter()
            .map(|(oid, size)| {
                (
                    (*oid).to_owned(),
                    ObjectInfo {
                        kind: "blob".to_owned(),
                        size: *size,
                        disk_size: *size,
                    },
                )
            })
            .collect();
        (info, added.iter().map(|oid| (*oid).to_owned()).collect())
    }

    #[test]
    fn cleanup_only_allows_deletions_and_bounded_control_file_content() {
        let none = BTreeMap::new();
        let nothing = BTreeSet::new();
        let unread = BTreeMap::new();
        assert!(!adds_content(&[], &nothing, &none, &unread));
        let changes = [
            change('D', "T/huge.bin"),
            change('D', "T/notes.md"),
            change('M', "T/data.dvc"),
            change('A', "T/data.workspace-mgr-storage.toml"),
            change('M', "T/.gitignore"),
            change('M', "T/.workspace-mgr-task.toml"),
        ];
        assert!(!adds_content(&changes, &nothing, &none, &unread));
        for path in ["T/README.md", "T/new.bin", "T/gitignore.txt"] {
            assert!(
                adds_content(&[change('A', path)], &nothing, &none, &unread),
                "{path}"
            );
        }
        assert!(adds_content(
            &[change('T', "T/link")],
            &nothing,
            &none,
            &unread
        ));

        // Every new control-file blob is charged in full, whether it grows,
        // keeps its size, or shrinks: rewriting stores the new content.
        let old = "1".repeat(40);
        let new = "2".repeat(40);
        let allowance = CONTROL_FILE_ALLOWANCE_BYTES;
        let charged = |old_size: u64, new_size: u64, status: char| {
            let (info, added) = sized(&[(&old, old_size), (&new, new_size)], &[&new]);
            let added = added.iter().map(String::as_str).collect::<BTreeSet<_>>();
            let mut rewritten = change(status, "T/.gitignore");
            if status == 'A' {
                rewritten.old_mode = "000000".to_owned();
                rewritten.old_oid = "0".repeat(40);
            }
            adds_content(&[rewritten], &added, &info, &unread)
        };
        let entry = tree_entry_bytes("T/.gitignore");
        assert!(!charged(0, allowance - entry, 'A'));
        assert!(charged(0, allowance - entry + 1, 'A'));
        assert!(!charged(10, allowance, 'M'));
        assert!(charged(990_000, 1_980_000, 'M'), "a ratchet step");
        assert!(charged(3_000_000, 3_000_000, 'M'), "a same-size rewrite");
        assert!(charged(5_000_000, 4_000_000, 'M'), "a shrinking rewrite");
        // New content is summed over every control file.
        let (info, _) = sized(&[(&old, 0), (&new, 600_000)], &[]);
        let added = BTreeSet::from([new.as_str()]);
        let mut pointer = change('M', "T/a.dvc");
        pointer.old_oid = old.clone();
        let mut ignore = change('M', "T/b/.gitignore");
        ignore.old_oid = old.clone();
        assert!(adds_content(&[pointer, ignore], &added, &info, &unread));
        // Blobs the remote already holds add nothing but their tree entries.
        assert!(!adds_content(
            &[change('A', "T/.gitignore")],
            &BTreeSet::new(),
            &sized(&[(&new, 3_000_000)], &[]).0,
            &unread
        ));
        // Many new directories, each holding only an empty ignore file the
        // remote already has, still carry their names into Git trees.
        let directories = (0..5_000)
            .map(|index| change('A', &format!("T/{index:0>200}/.gitignore")))
            .collect::<Vec<_>>();
        assert!(adds_content(&directories, &BTreeSet::new(), &none, &unread));
        assert!(!adds_content(
            &directories[..100],
            &BTreeSet::new(),
            &none,
            &unread
        ));

        let mut usage = measurement(10, 5, 0, 0);
        assert!(usage.cleanup_only());
        usage.storage.pending_uploads = true;
        assert!(!usage.cleanup_only());
        usage.storage.pending_uploads = false;
        usage.git.adds_content = true;
        assert!(!usage.cleanup_only());
    }

    fn listing(files: &[(&str, &str, u64)], padding: &str) -> Vec<u8> {
        let mut raw = format!("{padding}outs:\n- hash: md5\n  path: data\n  files:\n");
        for (relpath, version, size) in files {
            raw.push_str(&format!(
                "  - relpath: {relpath}\n    md5: md5-{relpath}-{version}\n    size: {size}\n    cloud:\n      workspace-mgr:\n        etag: md5-{relpath}-{version}\n        version_id: {version}\n"
            ));
        }
        raw.into_bytes()
    }

    /// Charges one rewrite of `T/data.dvc` from `old` to `new`.
    fn metadata_charge(old: &[u8], new: &[u8]) -> u64 {
        let old_oid = "1".repeat(40);
        let new_oid = "2".repeat(40);
        let (info, _) = sized(
            &[(&old_oid, old.len() as u64), (&new_oid, new.len() as u64)],
            &[],
        );
        let contents = BTreeMap::from([
            (old_oid.clone(), old.to_vec()),
            (new_oid.clone(), new.to_vec()),
        ]);
        let mut rewrite = change('M', "T/data.dvc");
        rewrite.old_oid = old_oid;
        rewrite.new_oid = new_oid;
        control_file_charge(&rewrite, &info, &contents)
    }

    #[test]
    fn metadata_that_only_drops_entries_is_free() {
        // A directory listing with thousands of entries shrinks after files
        // are deleted from it: nothing new is published.
        let files = (0..8_000)
            .map(|index| {
                (
                    format!("file-{index:05}.bin"),
                    format!("v{index}"),
                    1_000 + index,
                )
            })
            .collect::<Vec<_>>();
        let entries = |range: std::ops::Range<usize>| {
            files[range]
                .iter()
                .map(|(name, version, size)| (name.as_str(), version.as_str(), *size as u64))
                .collect::<Vec<_>>()
        };
        let before = listing(&entries(0..8_000), "");
        let after = listing(&entries(1_000..8_000), "");
        assert!(after.len() as u64 > CONTROL_FILE_ALLOWANCE_BYTES);
        assert_eq!(metadata_charge(&before, &after), 0);
        let aggregate =
            b"outs:\n- md5: d1.dir\n  size: 12\n  nfiles: 3\n  hash: md5\n  path: data\n";
        assert_eq!(metadata_charge(aggregate, aggregate), 0);

        // Anything else in the new metadata is charged: a new version at a
        // kept path, a new path, a changed digest, or padding.
        let replaced = listing(&[("a.bin", "v2", 5)], "");
        let original = listing(&[("a.bin", "v1", 5), ("b.bin", "v1", 7)], "");
        assert_eq!(metadata_charge(&original, &replaced), replaced.len() as u64);
        let added = listing(&[("a.bin", "v1", 5), ("c.bin", "v1", 7)], "");
        assert_eq!(metadata_charge(&original, &added), added.len() as u64);
        let redirected =
            b"outs:\n- md5: d2.dir\n  size: 12\n  nfiles: 3\n  hash: md5\n  path: data\n";
        assert_eq!(
            metadata_charge(aggregate, redirected),
            redirected.len() as u64
        );
        let comment = format!("# {}\n", "x".repeat(2_000_000));
        let padded = listing(&[("a.bin", "v1", 5)], &comment);
        assert_eq!(metadata_charge(&original, &padded), comment.len() as u64);
        let repadded = listing(
            &[("a.bin", "v1", 5)],
            &format!("# {}\n", "y".repeat(2_000_000)),
        );
        assert_eq!(metadata_charge(&padded, &repadded), comment.len() as u64);
        // Metadata that cannot be parsed is charged in full.
        assert_eq!(metadata_charge(&original, b"outs: ["), 7);
    }

    #[test]
    fn evaluation_treats_the_limit_itself_as_within_limit() {
        let threshold = 1_000;
        let at_limit = evaluate(&measurement(300, 200, 400, 500), threshold, None);
        assert_eq!(at_limit.status, "within_limit");
        assert!(at_limit.publish_allowed);
        assert_eq!(at_limit.projected.total_bytes, 1_000);
        assert_eq!(at_limit.published.total_bytes, 700);
        assert_eq!(at_limit.headroom_bytes, 0);
        assert!(at_limit.message.is_none());
        assert!(at_limit.approval.is_none());
        assert_eq!(at_limit.git_measure, "uncompressed");

        let mut over = measurement(300, 200, 400, 501);
        over.git.adds_content = true;
        let report = evaluate(&over, threshold, None);
        assert!(report.approval_required());
        assert!(!report.publish_allowed);
        assert!(!report.cleanup_only);
        assert!(!report.git_history_exceeds_limit);
        assert_eq!(report.headroom_bytes, 0);
        assert!(report.message.as_deref().unwrap().contains("1001 bytes"));

        let cleanup = evaluate(&measurement(1_200, 10, 0, 0), threshold, None);
        assert!(cleanup.approval_required());
        assert!(cleanup.cleanup_only);
        assert!(cleanup.publish_allowed);
        assert!(cleanup.git_history_exceeds_limit);
        let message = cleanup.message.as_deref().unwrap();
        assert!(message.contains("remains allowed"));
        assert!(message.contains("Published Git history alone exceeds the limit"));

        let approved = approval(2_000);
        let report = evaluate(&over, threshold, Some(&approved));
        assert_eq!(report.status, "within_limit");
        assert_eq!(report.limit_bytes, 2_000);
        assert_eq!(report.threshold_bytes, 1_000);
        assert_eq!(report.headroom_bytes, 999);
        assert_eq!(report.approval, Some(approved));

        let json = serde_json::to_value(evaluate(&measurement(1, 1, 1, 1), 10, None)).unwrap();
        assert!(json["approval"].is_null());
        assert!(json.get("message").is_none());
        assert_eq!(json["projected"]["total_bytes"], 3);
    }

    #[test]
    fn suggested_limits_round_up_to_256_mib_with_headroom() {
        const MIB_256: u64 = 268_435_456;
        const GIB: u64 = 1 << 30;
        assert_eq!(suggested_limit(0, GIB, false), MIB_256);
        assert_eq!(suggested_limit(100, GIB, false), 2 * MIB_256);
        assert_eq!(suggested_limit(400_000_000, GIB, false), 3 * MIB_256);
        assert_eq!(suggested_limit(800_000_000, GIB, false), GIB);
        // 125 % of 1.25 GiB is 1.5625 GiB, which rounds up to 1.75 GiB.
        assert_eq!(suggested_limit(5 * MIB_256, GIB, false), 7 * MIB_256);
        assert_eq!(suggested_limit(GIB + 1, GIB, true), 6 * MIB_256);
        // An approval request always proposes at least 256 MiB more than the
        // current limit.
        assert_eq!(
            suggested_limit(1_100_000_000, 2_000_000_000, true),
            9 * MIB_256
        );
        assert_eq!(suggested_limit(15_000_001, 15_000_000, true), 2 * MIB_256);
        assert_eq!(suggested_limit(20_000_000, 15_000_000, true), 2 * MIB_256);
        assert_eq!(suggested_limit(u64::MAX, u64::MAX, true), u64::MAX);
    }

    #[test]
    fn approval_trailers_record_the_limit_and_note_for_reviewers() {
        let mut noted = approval(3_221_225_472);
        noted.note = "Keep checkpoints; note=literal".to_owned();
        assert_eq!(
            approval_trailer(&noted),
            "Cloud-Usage-Approval: limit_bytes=3221225472; note=Keep checkpoints; note=literal"
        );
    }

    #[test]
    fn private_state_round_trips_and_disappears_when_empty() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let empty = load_state(&state_dir, "task", "codex/task").unwrap();
        assert_eq!(empty, CloudUsageState::new("task", "codex/task"));

        let mut state = empty.clone();
        save_state(&state_dir, &state).unwrap();
        assert!(!state_path(&state_dir).exists());

        let inputs = UsageInputs {
            state_dir: &state_dir,
            remote_base_oid: "base",
            remote_target_oid: None,
            projected_tree_oid: "tree",
            pointers: &[],
            automatic_s3: &[],
            inspect_outputs: false,
        };
        let blocked = evaluate(&measurement(10, 10, 0, 0), 15, None);
        state.update_pending(&blocked, &inputs);
        let pending = state.pending.clone().unwrap();
        assert_eq!(pending.limit_bytes, 15);
        assert_eq!(pending.projected.total_bytes, 20);
        assert!(pending.remote_target_oid.is_none());
        assert!(chrono::DateTime::parse_from_rfc3339(&pending.measured_at).is_ok());
        save_state(&state_dir, &state).unwrap();
        let raw = fs::read_to_string(state_path(&state_dir)).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["pending"]["projected"]["total_bytes"], 20);
        assert!(json.get("approval").is_none());
        assert_eq!(load_state(&state_dir, "task", "codex/task").unwrap(), state);

        assert!(load_state(&state_dir, "other", "codex/task").is_err());
        assert!(load_state(&state_dir, "task", "codex/other").is_err());

        state.update_pending(&evaluate(&measurement(1, 1, 0, 0), 15, None), &inputs);
        assert!(state.pending.is_none());
        save_state(&state_dir, &state).unwrap();
        assert!(!state_path(&state_dir).exists());
        save_state(&state_dir, &state).unwrap();

        fs::write(
            state_path(&state_dir),
            "{\"schema_version\": 2, \"task_id\": \"task\", \"branch\": \"codex/task\"}",
        )
        .unwrap();
        assert!(load_state(&state_dir, "task", "codex/task").is_err());
        fs::write(state_path(&state_dir), "not json").unwrap();
        assert!(load_state(&state_dir, "task", "codex/task").is_err());
        assert!(reminder(&state_dir, &task("task", "codex/task")).is_none());
    }

    #[test]
    fn reminders_name_the_pending_decision_until_an_approval_covers_it() {
        const GIB: u64 = 1 << 30;
        let task_id = "20260918-120000-demo";
        let mut state = CloudUsageState::new(task_id, "codex/demo");
        assert!(pending_reminder(task_id, &state, None, GIB).is_none());
        state.pending = Some(PendingDecision {
            measured_at: "2026-09-18T12:00:00Z".to_owned(),
            limit_bytes: GIB,
            remote_base_oid: "base".to_owned(),
            remote_target_oid: Some("target".to_owned()),
            projected_tree_oid: "tree".to_owned(),
            published: UsageTotals::default(),
            projected: UsageTotals::new(1_342_177_280, 1_342_177_280, 0, 0),
        });
        assert_eq!(
            pending_reminder(task_id, &state, None, GIB).as_deref(),
            Some(
                "workspace-mgr: task 20260918-120000-demo is waiting for the user's cloud-usage decision, as last measured by `workspace-mgr plan` or `workspace-mgr publish`: projected 1.25 GiB (1342177280 bytes) exceeds limit 1 GiB (1073741824 bytes). Unless the user already answered, stop task work and ask the user; after carrying out the user's answer, run `workspace-mgr plan` to re-measure."
            )
        );
        // The manifest's approval covers the pending projection.
        assert!(pending_reminder(task_id, &state, Some(&approval(2 * GIB)), GIB).is_none());
        // An approval below the projection still leaves the decision due.
        assert!(pending_reminder(task_id, &state, Some(&approval(GIB + 1)), GIB).is_some());
    }

    #[test]
    fn refusal_message_is_self_contained() {
        let mut usage = measurement(600, 100, 300, 450);
        usage.git.adds_content = true;
        let report = evaluate(&usage, 1_000, None);
        let message = refusal_message("20260918-120000-demo", &report);
        assert!(message.contains("task 20260918-120000-demo"));
        assert!(message.contains("published Git 600 bytes, S3 300 bytes, total 900 bytes"));
        assert!(
            message.contains("projected Git 700 bytes, S3 450 bytes, total 1.12 KiB (1150 bytes)")
        );
        assert!(message.contains("limit 1000 bytes."));
        assert!(
            message.contains("at most 1 MiB (1048576 bytes) of new workspace-mgr control-file")
        );
        assert!(message.contains("only after the user gives it in this chat"));
        assert!(message.contains("workspace-mgr plan"));
        assert!(!message.contains("history alone"));
        assert!(!message.to_ascii_lowercase().contains("dvc"));
    }

    #[test]
    fn lfs_pointers_are_recognized_by_their_version_line() {
        let pointer = b"version https://git-lfs.github.com/spec/v1\noid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393\nsize 12345\n";
        assert_eq!(
            lfs_pointer(pointer),
            Some(LfsObject {
                oid: "sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393"
                    .to_owned(),
                size: 12_345,
            })
        );
        assert!(lfs_pointer(b"version https://example.invalid/v1\noid x\nsize 1\n").is_none());
        assert!(lfs_pointer(b"version https://git-lfs.github.com/spec/v1\noid x\n").is_none());
        assert!(lfs_pointer(&[0xff, 0xfe]).is_none());
    }

    #[test]
    fn raw_change_records_skip_interleaved_commit_ids() {
        let zero = "0".repeat(40);
        let one = "1".repeat(40);
        let two = "2".repeat(40);
        let output = format!(
            "{one}\0\n:000000 100644 {zero} {two} A\0T/a b.dvc\0:100644 000000 {two} {zero} D\0T/c.dvc\0{two}\0\n:100644 100644 {one} {two} M\0:odd/path.dvc\0"
        );
        let changes = parse_raw_changes(&output).unwrap();
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0].path, "T/a b.dvc");
        assert_eq!(changes[0].old_blob(), None);
        assert_eq!(changes[0].new_blob(), Some(two.as_str()));
        assert_eq!(changes[1].status, 'D');
        assert_eq!(changes[1].new_blob(), None);
        assert_eq!(changes[2].path, ":odd/path.dvc");
        let commits = parse_raw_commits(&output).unwrap();
        assert_eq!(
            commits
                .iter()
                .map(|commit| commit.iter().map(|change| change.path.as_str()).collect())
                .collect::<Vec<Vec<_>>>(),
            vec![vec!["T/a b.dvc", "T/c.dvc"], vec![":odd/path.dvc"]]
        );
        let diff = format!(
            ":100644 100644 {one} {two} M\0T/a.dvc\0:100644 000000 {two} {zero} D\0T/b.dvc\0"
        );
        assert_eq!(parse_raw_commits(&diff).unwrap().len(), 1);
        assert!(parse_raw_commits("").unwrap().is_empty());
        assert!(parse_raw_changes(":100644 100644 x\0path\0").is_err());
        assert!(parse_raw_changes(&format!(":100644 100644 {one} {two} M")).is_err());
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        repo: GitRepo,
        state_dir: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("repo");
            fs::create_dir(&root).unwrap();
            let repo = GitRepo { root };
            repo.run(["init", "-q", "-b", "main"]).unwrap();
            repo.run(["config", "user.name", "workspace-mgr test"])
                .unwrap();
            repo.run(["config", "user.email", "test@example.invalid"])
                .unwrap();
            let state_dir = temp.path().join("state");
            Self {
                _temp: temp,
                repo,
                state_dir,
            }
        }

        fn write(&self, path: &str, content: &[u8]) {
            let absolute = resolved_under(&self.repo.root, path);
            fs::create_dir_all(absolute.parent().unwrap()).unwrap();
            fs::write(absolute, content).unwrap();
        }

        fn remove(&self, path: &str) {
            fs::remove_file(resolved_under(&self.repo.root, path)).unwrap();
        }

        fn commit(&self, message: &str) -> String {
            self.repo.run(["add", "-A"]).unwrap();
            self.repo.run(["commit", "-q", "-m", message]).unwrap();
            self.oid("HEAD")
        }

        fn tree(&self) -> String {
            self.repo.run(["add", "-A"]).unwrap();
            self.repo
                .run(["write-tree"])
                .unwrap()
                .stdout
                .trim()
                .to_owned()
        }

        fn oid(&self, revision: &str) -> String {
            self.repo
                .run(["rev-parse", revision])
                .unwrap()
                .stdout
                .trim()
                .to_owned()
        }

        fn size(&self, object: &str) -> u64 {
            self.repo
                .run(["cat-file", "-s", object])
                .unwrap()
                .stdout
                .trim()
                .parse()
                .unwrap()
        }

        fn inputs<'a>(
            &'a self,
            base: &'a str,
            target: Option<&'a str>,
            tree: &'a str,
            pointers: &'a [String],
            automatic: &'a [String],
        ) -> UsageInputs<'a> {
            UsageInputs {
                state_dir: &self.state_dir,
                remote_base_oid: base,
                remote_target_oid: target,
                projected_tree_oid: tree,
                pointers,
                automatic_s3: automatic,
                inspect_outputs: false,
            }
        }
    }

    fn pseudo_random(length: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        (0..length)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                (state >> 56) as u8
            })
            .collect()
    }

    #[test]
    fn git_bytes_cover_task_history_lfs_payloads_and_projected_growth() {
        let fixture = Fixture::new();
        fixture.write("README.md", b"base\n");
        fixture.write("shared.txt", b"shared v1\n");
        let base = fixture.commit("base");
        fixture.write("T/README.md", b"task\n");
        fixture.write("T/data.bin", &pseudo_random(5_000, 1));
        let lfs = b"version https://git-lfs.github.com/spec/v1\noid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393\nsize 123456\n";
        fixture.write("T/large.bin", lfs);
        let first = fixture.commit("task one");
        fixture.write("T/data.bin", &pseudo_random(6_000, 2));
        let target = fixture.commit("task two");
        fixture.write("T/new.txt", b"new content\n");
        let tree = fixture.tree();

        let config = Config::default();
        let usage = measure(
            &fixture.repo,
            &config,
            &fixture.inputs(&base, Some(&target), &tree, &[], &[]),
            u64::MAX,
        )
        .unwrap();
        let (published, projected) = usage.totals();
        let expected_published = [
            first.clone(),
            target.clone(),
            format!("{first}^{{tree}}"),
            format!("{first}:T"),
            format!("{target}^{{tree}}"),
            format!("{target}:T"),
            format!("{first}:T/README.md"),
            format!("{first}:T/data.bin"),
            format!("{first}:T/large.bin"),
            format!("{target}:T/data.bin"),
        ]
        .iter()
        .map(|object| fixture.size(object))
        .sum::<u64>();
        assert_eq!(published.git_uncompressed_bytes, expected_published);
        assert_eq!(published.git_lfs_bytes, 123_456);
        assert_eq!(published.git_bytes, expected_published + 123_456);
        assert_eq!(published.s3_bytes, 0);
        let expected_delta = [
            tree.clone(),
            format!("{tree}:T"),
            format!("{tree}:T/new.txt"),
        ]
        .iter()
        .map(|object| fixture.size(object))
        .sum::<u64>();
        assert_eq!(
            projected.git_uncompressed_bytes,
            expected_published + expected_delta
        );
        assert_eq!(projected.git_lfs_bytes, 123_456);
        assert_eq!(usage.git_measure(), "uncompressed");
        assert!(usage.git.adds_content);
        assert!(!usage.cleanup_only());
        let contributors = usage.contributors();
        assert_eq!(contributors[0].path, "T/large.bin");
        assert_eq!(contributors[0].store, "git");
        assert_eq!(contributors[0].bytes, 123_456 + lfs.len() as u64);
        let data = contributors
            .iter()
            .find(|contributor| contributor.path == "T/data.bin")
            .unwrap();
        assert_eq!(data.bytes, 11_000);
        assert_eq!(data.versions, 2);
        assert_eq!(data.state, "published");
        let new = contributors
            .iter()
            .find(|contributor| contributor.path == "T/new.txt")
            .unwrap();
        assert_eq!(new.state, "pending");

        // A late re-measurement of the final tree picks up extra growth.
        let mut late = usage.clone();
        fixture.write("T/late.txt", &pseudo_random(2_000, 3));
        let final_tree = fixture.tree();
        late.refresh_git(
            &fixture.repo,
            &fixture.inputs(&base, Some(&target), &final_tree, &[], &[]),
            u64::MAX,
        )
        .unwrap();
        assert!(late.totals().1.git_uncompressed_bytes >= projected.git_uncompressed_bytes + 2_000);
        assert_eq!(late.totals().0, published);
        fixture.remove("T/late.txt");

        // Content already in the base branch is never attributed to the task.
        fixture.remove("T/new.txt");
        fixture.write("shared.txt", b"shared v1\n");
        let unchanged = fixture.tree();
        let usage = measure(
            &fixture.repo,
            &config,
            &fixture.inputs(&base, Some(&target), &unchanged, &[], &[]),
            u64::MAX,
        )
        .unwrap();
        assert_eq!(usage.totals().1.git_uncompressed_bytes, expected_published);
        assert!(usage.cleanup_only());

        // A first publication has no published history.
        let usage = measure(
            &fixture.repo,
            &config,
            &fixture.inputs(&base, None, &unchanged, &[], &[]),
            u64::MAX,
        )
        .unwrap();
        let (published, projected) = usage.totals();
        assert_eq!(published.total_bytes, 0);
        assert!(projected.git_uncompressed_bytes > 6_000);
        assert!(projected.git_uncompressed_bytes < 11_000);
        assert_eq!(projected.git_lfs_bytes, 123_456);
        assert!(
            !usage
                .git
                .published_objects
                .iter()
                .any(|object| object.oid == target)
        );
    }

    fn task(task_id: &str, branch: &str) -> ResolvedTask {
        ResolvedTask {
            manifest_path: PathBuf::from("T/.workspace-mgr-task.toml"),
            kind: crate::manifest::TaskKind::Deliverable,
            task_id: task_id.to_owned(),
            slug: "demo".to_owned(),
            task_path: Some("T".to_owned()),
            branch: branch.to_owned(),
            title: "Demo".to_owned(),
            purpose: "Demo".to_owned(),
            remote: "origin".to_owned(),
            base_branch: "main".to_owned(),
            shared_head: "main".to_owned(),
            additional_scopes: Vec::new(),
            cloud_usage_approval: None,
        }
    }

    #[test]
    fn gate_applies_the_manifest_approval_and_records_pending_decisions() {
        let fixture = Fixture::new();
        fixture.write("README.md", b"base\n");
        let base = fixture.commit("base");
        fixture.write("T/data.bin", &pseudo_random(3_000, 11));
        let target = fixture.commit("Publish data\n\nWorkspace-Task: demo");
        let unchanged = fixture.tree();
        fixture.write("T/more.bin", &pseudo_random(3_000, 12));
        let grown = fixture.tree();
        let mut task = task("demo", "codex/demo");
        let approved = approval(5_000);
        task.cloud_usage_approval = Some(approved.clone());
        let config = Config::default();
        let load = || load_state(&fixture.state_dir, "demo", "codex/demo").unwrap();

        let over = fixture.inputs(&base, Some(&target), &grown, &[], &[]);
        let gate = UsageGate::open(&fixture.repo, &config, &task, &over, 100).unwrap();
        assert_eq!(gate.approval(), Some(&approved));
        assert_eq!(gate.report().limit_bytes, 5_000);
        assert_eq!(gate.report().approval, Some(approved));
        assert!(gate.report().approval_required());
        let refusal = gate.enforce().unwrap_err().to_string();
        assert!(refusal.contains("cloud usage for task demo needs the user's approval"));
        let pending = load().pending.unwrap();
        assert_eq!(pending.projected_tree_oid, grown);
        assert_eq!(pending.remote_target_oid.as_deref(), Some(target.as_str()));
        assert!(
            !fs::read_to_string(state_path(&fixture.state_dir))
                .unwrap()
                .contains("approval"),
            "the private state never records approvals"
        );

        // A higher approval in the manifest covers the publication.
        let raised = approval(20_000);
        task.cloud_usage_approval = Some(raised.clone());
        let within = fixture.inputs(&base, Some(&target), &unchanged, &[], &[]);
        let mut gate = UsageGate::open(&fixture.repo, &config, &task, &within, 100).unwrap();
        assert_eq!(gate.approval(), Some(&raised));
        assert_eq!(gate.report().status, "within_limit");
        assert!(gate.enforce().is_ok());
        assert_eq!(load().pending, None);
        assert!(!state_path(&fixture.state_dir).exists());

        // A late re-measurement of a larger final tree records the decision.
        fixture.write("T/late.bin", &pseudo_random(20_000, 13));
        let late_tree = fixture.tree();
        gate.recheck_git(
            &fixture.repo,
            &fixture.inputs(&base, Some(&target), &late_tree, &[], &[]),
        )
        .unwrap();
        assert!(gate.report().approval_required());
        assert!(gate.enforce().is_err());
        assert_eq!(load().pending.unwrap().projected_tree_oid, late_tree);
    }

    #[test]
    fn packed_git_bytes_are_measured_only_above_the_limit_and_cached() {
        let fixture = Fixture::new();
        fixture.write("README.md", b"base\n");
        let base = fixture.commit("base");
        let text = (0..20_000)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        fixture.write("T/text.txt", text.as_bytes());
        let target = fixture.commit("task");
        fixture.write("T/text.txt", format!("{text}tail\n").as_bytes());
        let tree = fixture.tree();
        let config = Config::default();
        let inputs = fixture.inputs(&base, Some(&target), &tree, &[], &[]);

        let uncompressed = measure(&fixture.repo, &config, &inputs, u64::MAX).unwrap();
        assert_eq!(uncompressed.git_measure(), "uncompressed");
        assert!(!fixture.state_dir.join(CACHE_NAME).exists());

        let packed = measure(&fixture.repo, &config, &inputs, 1_000).unwrap();
        assert_eq!(packed.git_measure(), "packed");
        let (published, projected) = packed.totals();
        let (raw_published, raw_projected) = uncompressed.totals();
        assert!(published.git_bytes > 0);
        assert!(published.git_bytes < raw_published.git_bytes);
        assert!(projected.git_bytes > published.git_bytes);
        assert!(projected.git_bytes < raw_projected.git_bytes);
        assert_eq!(
            projected.git_uncompressed_bytes,
            raw_projected.git_uncompressed_bytes
        );
        let cache: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(fixture.state_dir.join(CACHE_NAME)).unwrap())
                .unwrap();
        assert_eq!(cache["packed"]["remote_target_oid"], target.as_str());
        assert_eq!(cache["packed"]["bytes"], published.git_bytes);

        // Contributors are comparable with the gated totals: compressed when
        // the totals are packed, uncompressed otherwise.
        let bytes = |usage: &Measurement| {
            usage
                .contributors()
                .into_iter()
                .find(|contributor| contributor.path == "T/text.txt")
                .unwrap()
                .bytes
        };
        let raw_text = fixture.size(&format!("{target}:T/text.txt"))
            + fixture.size(&format!("{tree}:T/text.txt"));
        assert_eq!(bytes(&uncompressed), raw_text);
        assert!(bytes(&packed) > 0);
        assert!(bytes(&packed) * 4 < raw_text, "{}", bytes(&packed));

        let again = measure(&fixture.repo, &config, &inputs, 1_000).unwrap();
        assert_eq!(again.totals(), packed.totals());
        let report = evaluate(&again, 1_000, None);
        assert_eq!(report.git_measure, "packed");

        fs::write(fixture.state_dir.join(CACHE_NAME), "corrupt").unwrap();
        let recovered = measure(&fixture.repo, &config, &inputs, 1_000).unwrap();
        assert_eq!(recovered.totals(), packed.totals());
    }

    fn versioned_config() -> Config {
        Config {
            s3: Some(S3Config {
                url: "s3://bucket/prefix".to_owned(),
                endpoint_url: None,
            }),
            ..Config::default()
        }
    }

    fn file_pointer(name: &str, size: u64, version: Option<&str>) -> String {
        let cloud = version
            .map(|version| {
                format!("  cloud:\n    workspace-mgr:\n      etag: e-{version}\n      version_id: {version}\n")
            })
            .unwrap_or_default();
        format!(
            "outs:\n- md5: md5-{name}-{size}\n  size: {size}\n  hash: md5\n  path: {name}\n{cloud}"
        )
    }

    fn directory_pointer(name: &str, files: &[(&str, u64, &str)]) -> String {
        let mut raw = format!("outs:\n- hash: md5\n  path: {name}\n  files:\n");
        for (relpath, size, version) in files {
            raw.push_str(&format!(
                "  - relpath: {relpath}\n    md5: md5-{relpath}-{size}\n    size: {size}\n    cloud:\n      workspace-mgr:\n        version_id: {version}\n"
            ));
        }
        raw
    }

    #[test]
    fn versioned_storage_replays_task_history_and_projects_this_publication() {
        let fixture = Fixture::new();
        fixture.write("README.md", b"base\n");
        fixture.write(
            "shared/base.bin.dvc",
            file_pointer("base.bin", 900, Some("s1")).as_bytes(),
        );
        let base = fixture.commit("base");
        fixture.write(
            "T/model.pt.dvc",
            file_pointer("model.pt", 100, Some("m1")).as_bytes(),
        );
        fixture.write(
            "T/dir.dvc",
            directory_pointer("dir", &[("a", 10, "a1"), ("b", 20, "b1")]).as_bytes(),
        );
        fixture.write(
            "T/old.bin.dvc",
            file_pointer("old.bin", 40, Some("o1")).as_bytes(),
        );
        fixture.commit("task one");
        fixture.write(
            "T/model.pt.dvc",
            file_pointer("model.pt", 150, Some("m2")).as_bytes(),
        );
        fixture.write(
            "T/dir.dvc",
            directory_pointer("dir", &[("a", 10, "a1")]).as_bytes(),
        );
        let target = fixture.commit("task two");

        fixture.remove("T/old.bin.dvc");
        let pending = file_pointer("model.bin", 70, None);
        fixture.write("T/run1/model.bin.dvc", pending.as_bytes());
        fixture.write("T/run2/model.bin.dvc", pending.as_bytes());
        fixture.write("T/auto.bin", &[7_u8; 33]);
        let tree = fixture.tree();
        let pointers = [
            "T/dir.dvc".to_owned(),
            "T/model.pt.dvc".to_owned(),
            "T/run1/model.bin.dvc".to_owned(),
            "T/run2/model.bin.dvc".to_owned(),
            "shared/base.bin.dvc".to_owned(),
        ];
        let automatic = ["T/auto.bin".to_owned()];
        let inputs = fixture.inputs(&base, Some(&target), &tree, &pointers, &automatic);
        let config = versioned_config();
        let usage = measure(&fixture.repo, &config, &inputs, u64::MAX).unwrap();
        let (published, projected) = usage.totals();
        assert_eq!(published.s3_bytes, 100 + 150 + 10 + 40);
        assert_eq!(projected.s3_bytes, 100 + 150 + 10 + 70 + 70 + 33);
        assert!(usage.pending_uploads());
        assert!(!usage.cleanup_only());
        let contributors = usage.contributors();
        let model = contributors
            .iter()
            .find(|contributor| contributor.path == "T/model.pt")
            .unwrap();
        assert_eq!(
            (model.store, model.bytes, model.versions, model.state),
            ("s3", 250, 2, "published")
        );
        assert!(
            !contributors
                .iter()
                .any(|contributor| contributor.path == "shared/base.bin")
        );
        assert!(
            !contributors
                .iter()
                .any(|contributor| contributor.path == "T/old.bin")
        );

        let cache: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(fixture.state_dir.join(CACHE_NAME)).unwrap())
                .unwrap();
        assert!(cache["pointers"].as_object().unwrap().len() >= 5);

        // After the pending uploads are published, the same view has nothing
        // left to upload and only the metadata change remains.
        let pushed_model = file_pointer("model.bin", 70, Some("r1"));
        fixture.write("T/run1/model.bin.dvc", pushed_model.as_bytes());
        fixture.write(
            "T/run2/model.bin.dvc",
            file_pointer("model.bin", 70, Some("r2")).as_bytes(),
        );
        fixture.remove("T/auto.bin");
        let tree = fixture.tree();
        let mut usage = measure(
            &fixture.repo,
            &config,
            &fixture.inputs(&base, Some(&target), &tree, &pointers, &[]),
            u64::MAX,
        )
        .unwrap();
        assert_eq!(usage.totals().1.s3_bytes, 100 + 150 + 10 + 70 + 70);
        assert!(!usage.pending_uploads());
        assert!(usage.cleanup_only());

        // A later re-measurement of storage alone sees new metadata.
        fixture.write(
            "T/run2/model.bin.dvc",
            file_pointer("model.bin", 90, None).as_bytes(),
        );
        usage
            .refresh_storage(
                &fixture.repo,
                &config,
                &fixture.inputs(&base, Some(&target), &tree, &pointers, &[]),
                u64::MAX,
            )
            .unwrap();
        assert_eq!(usage.totals().1.s3_bytes, 100 + 150 + 10 + 70 + 90);
        assert!(usage.pending_uploads());
    }

    #[test]
    fn published_directory_listings_with_any_engine_name_are_measured() {
        let fixture = Fixture::new();
        fixture.write("README.md", b"base\n");
        let base = fixture.commit("base");
        fixture.write(
            "T/data.dvc",
            b"outs:\n- md5: d1.dir\n  size: 12\n  nfiles: 3\n  hash: md5\n  path: data\n",
        );
        fixture.commit("task one");
        // The version-aware push lists files by the names read from disk.
        let listing = "outs:\n- hash: md5\n  path: data\n  files:\n  - relpath: \"Icon\\r\"\n    md5: i1\n    size: 1\n    cloud:\n      workspace-mgr:\n        version_id: vi\n  - relpath: 'report '\n    md5: r1\n    size: 5\n    cloud:\n      workspace-mgr:\n        version_id: vr\n  - relpath: ../x\n    md5: x1\n    size: 6\n    cloud:\n      workspace-mgr:\n        version_id: vx\n";
        fixture.write("T/data.dvc", listing.as_bytes());
        let target = fixture.commit("task two");
        let config = versioned_config();

        // The worktree still has the listing, then only the aggregate, then
        // the directory is removed: every measurement succeeds.
        let pointers = ["T/data.dvc".to_owned()];
        let tree = fixture.tree();
        let usage = measure(
            &fixture.repo,
            &config,
            &fixture.inputs(&base, Some(&target), &tree, &pointers, &[]),
            u64::MAX,
        )
        .unwrap();
        assert_eq!(usage.totals().0.s3_bytes, 12);
        assert_eq!(usage.totals().1.s3_bytes, 12);
        let paths = usage
            .contributors()
            .into_iter()
            .map(|contributor| contributor.path)
            .collect::<BTreeSet<_>>();
        assert!(paths.contains("T/data/Icon\r"));
        assert!(paths.contains("T/data/report "));
        assert!(paths.contains("T/data"));

        fixture.remove("T/data.dvc");
        let tree = fixture.tree();
        let usage = measure(
            &fixture.repo,
            &config,
            &fixture.inputs(&base, Some(&target), &tree, &[], &[]),
            u64::MAX,
        )
        .unwrap();
        assert_eq!(usage.totals().0.s3_bytes, 12);
        assert_eq!(usage.totals().1.s3_bytes, 0);
        assert!(usage.cleanup_only());
    }

    #[test]
    fn listings_that_only_drop_entries_stay_cleanup_only() {
        let fixture = Fixture::new();
        fixture.write("README.md", b"base\n");
        let base = fixture.commit("base");
        let files = (0..8_000)
            .map(|index| (format!("f-{index:05}.bin"), format!("v{index}"), 10 + index))
            .collect::<Vec<_>>();
        let entries = |range: std::ops::Range<usize>| {
            files[range]
                .iter()
                .map(|(name, version, size)| (name.as_str(), version.as_str(), *size as u64))
                .collect::<Vec<_>>()
        };
        fixture.write("T/data.dvc", &listing(&entries(0..8_000), ""));
        let target = fixture.commit("publish listing");
        let pointers = ["T/data.dvc".to_owned()];
        let config = versioned_config();
        let measure_tree = |tree: &str| {
            measure(
                &fixture.repo,
                &config,
                &fixture.inputs(&base, Some(&target), tree, &pointers, &[]),
                u64::MAX,
            )
            .unwrap()
        };

        // Files deleted from a published directory on a version-aware remote
        // leave a rewritten listing larger than the allowance: still free.
        let shrunk = listing(&entries(500..8_000), "");
        assert!(shrunk.len() as u64 > CONTROL_FILE_ALLOWANCE_BYTES);
        fixture.write("T/data.dvc", &shrunk);
        let usage = measure_tree(&fixture.tree());
        assert!(!usage.pending_uploads());
        assert!(!usage.git.adds_content);
        assert!(usage.cleanup_only());

        // The same entries padded with new content are not a cleanup.
        let comment = format!("# {}\n", "z".repeat(1_100_000));
        fixture.write("T/data.dvc", &listing(&entries(500..8_000), &comment));
        let usage = measure_tree(&fixture.tree());
        assert!(!usage.pending_uploads());
        assert!(!usage.cleanup_only());
    }

    #[test]
    fn content_addressed_storage_counts_identical_pointers_at_two_paths_once() {
        let fixture = Fixture::new();
        fixture.write("README.md", b"base\n");
        let base = fixture.commit("base");
        let pointer = file_pointer("model.bin", 700, None);
        fixture.write("T/run1/model.bin.dvc", pointer.as_bytes());
        fixture.write("T/run2/model.bin.dvc", pointer.as_bytes());
        let target = fixture.commit("task");
        fixture.write(
            "T/other.bin.dvc",
            file_pointer("other.bin", 5, None).as_bytes(),
        );
        let tree = fixture.tree();
        let pointers = [
            "T/other.bin.dvc".to_owned(),
            "T/run1/model.bin.dvc".to_owned(),
            "T/run2/model.bin.dvc".to_owned(),
        ];
        let config = Config {
            s3: Some(S3Config {
                url: "/tmp/storage".to_owned(),
                endpoint_url: None,
            }),
            ..Config::default()
        };
        let usage = measure(
            &fixture.repo,
            &config,
            &fixture.inputs(&base, Some(&target), &tree, &pointers, &[]),
            u64::MAX,
        )
        .unwrap();
        let (published, projected) = usage.totals();
        assert_eq!(published.s3_bytes, 700);
        assert_eq!(projected.s3_bytes, 705);
        assert!(usage.pending_uploads());
    }

    #[test]
    fn blob_streams_handle_large_and_many_small_objects() {
        let fixture = Fixture::new();
        let large = pseudo_random(300_000, 7);
        fixture.write("large.bin", &large);
        for index in 0..500 {
            fixture.write(
                &format!("small/{index}.txt"),
                format!("{index}\n").as_bytes(),
            );
        }
        fixture.commit("objects");
        let mut oids = vec![fixture.oid("HEAD:large.bin")];
        oids.extend((0..500).map(|index| fixture.oid(&format!("HEAD:small/{index}.txt"))));
        let mut seen = Vec::new();
        read_blobs(&fixture.repo, &oids, |oid, content| {
            seen.push((oid.to_owned(), content.to_vec()));
            Ok(())
        })
        .unwrap();
        assert_eq!(seen.len(), 501);
        assert_eq!(seen[0].1, large);
        assert_eq!(seen[500].1, b"499\n");
        assert!(read_blobs(&fixture.repo, &["0".repeat(40)], |_, _| Ok(())).is_err());

        let paths = [
            "large.bin".to_owned(),
            "absent.dvc".to_owned(),
            "small".to_owned(),
        ];
        let blobs = blobs_at(&fixture.repo, "HEAD", &paths).unwrap();
        assert_eq!(blobs["large.bin"].as_deref(), Some(oids[0].as_str()));
        assert_eq!(blobs["absent.dvc"], None);
        assert_eq!(blobs["small"], None);
    }
    #[test]
    fn dirty_outputs_are_sized_from_the_worktree() {
        let fixture = Fixture::new();
        fixture.write("T/file.bin", &[1_u8; 40]);
        fixture.write("T/dir/a", &[2_u8; 5]);
        fixture.write("T/dir/e", &[3_u8; 7]);
        fixture.write("T/files/new", &[4_u8; 11]);
        let status = DataStatus {
            not_in_cache: vec!["T/file.bin".to_owned()],
            uncommitted: dvc::DataChanges {
                added: vec!["T/files/new".to_owned(), "T/files/vanished".to_owned()],
                modified: vec![
                    "T/dir/".to_owned(),
                    "T/dir/a".to_owned(),
                    "T/file.bin".to_owned(),
                ],
                deleted: vec!["T/dir/c".to_owned(), "T/gone.bin".to_owned()],
                renamed: vec![dvc::DataRename {
                    old: "T/dir/b".to_owned(),
                    new: "T/dir/e".to_owned(),
                }],
                unknown: Vec::new(),
            },
        };
        let mut aggregate = entry("T/dir", 30, None);
        aggregate.aggregate = true;
        let worktree = vec![
            aggregate,
            entry("T/file.bin", 20, Some("f1")),
            entry("T/files/old", 3, Some("o1")),
        ];
        let changed = vec![
            ("T/file.bin".to_owned(), 40),
            ("T/files/new".to_owned(), 11),
        ];
        let directory = vec![("T/dir".to_owned(), 12)];
        assert_eq!(
            dirty_uploads(&fixture.repo.root, &status, &worktree, true),
            (changed.clone(), directory.clone())
        );
        // Renames upload nothing new on a content-addressed remote, but the
        // directory is still sized as a whole.
        assert_eq!(
            dirty_uploads(&fixture.repo.root, &status, &worktree, false),
            (changed, directory.clone())
        );

        // Any change beneath an aggregate records a new version of it, while
        // a deletion elsewhere uploads nothing.
        let deleted = DataStatus {
            not_in_cache: Vec::new(),
            uncommitted: dvc::DataChanges {
                deleted: vec!["T/dir/c".to_owned(), "T/gone.bin".to_owned()],
                ..dvc::DataChanges::default()
            },
        };
        for version_aware in [true, false] {
            assert_eq!(
                dirty_uploads(&fixture.repo.root, &deleted, &worktree, version_aware),
                (Vec::new(), directory.clone())
            );
        }
        let moved = DataStatus {
            not_in_cache: Vec::new(),
            uncommitted: dvc::DataChanges {
                renamed: vec![dvc::DataRename {
                    old: "T/files/old".to_owned(),
                    new: "T/files/new".to_owned(),
                }],
                ..dvc::DataChanges::default()
            },
        };
        assert_eq!(
            dirty_uploads(&fixture.repo.root, &moved, &worktree, true),
            (vec![("T/files/new".to_owned(), 11)], Vec::new())
        );
        assert_eq!(
            dirty_uploads(&fixture.repo.root, &moved, &worktree, false),
            (Vec::new(), Vec::new())
        );
    }

    #[cfg(unix)]
    #[test]
    fn dirty_symlinks_are_sized_by_their_targets_without_failing() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        fixture.write("T/ckpt/ckpt-100.bin", &[1_u8; 100]);
        fixture.write("T/ckpt/ckpt-200.bin", &[2_u8; 200]);
        fixture.write("T/agg/real.bin", &[3_u8; 30]);
        fixture.write("T/agg/sub/deep.bin", &[4_u8; 4]);
        fixture.write("elsewhere/shared.bin", &[5_u8; 50]);
        let root = &fixture.repo.root;
        symlink("ckpt-200.bin", root.join("T/ckpt/latest.bin")).unwrap();
        symlink("missing.bin", root.join("T/ckpt/dangling.bin")).unwrap();
        symlink("../real.bin", root.join("T/agg/sub/latest.bin")).unwrap();
        symlink("../../elsewhere", root.join("T/agg/linked")).unwrap();
        symlink("nowhere", root.join("T/agg/broken")).unwrap();
        let status = DataStatus {
            not_in_cache: Vec::new(),
            uncommitted: dvc::DataChanges {
                added: vec![
                    "T/ckpt/dangling.bin".to_owned(),
                    "T/agg/sub/latest.bin".to_owned(),
                ],
                modified: vec!["T/ckpt/".to_owned(), "T/ckpt/latest.bin".to_owned()],
                ..dvc::DataChanges::default()
            },
        };
        let mut aggregate = entry("T/agg", 34, None);
        aggregate.aggregate = true;
        let worktree = vec![
            entry("T/ckpt/ckpt-100.bin", 100, Some("c1")),
            entry("T/ckpt/latest.bin", 100, Some("l1")),
            aggregate,
        ];
        // Like the storage engine, the linked file counts its target while
        // the linked directory and the dangling link count nothing.
        assert_eq!(
            dirty_uploads(root, &status, &worktree, true),
            (
                vec![("T/ckpt/latest.bin".to_owned(), 200)],
                vec![("T/agg".to_owned(), 30 + 4 + 30)],
            )
        );
        assert_eq!(upload_bytes(root, "T/ckpt/dangling.bin", Some(9)), None);
        assert_eq!(upload_bytes(root, "T/agg/linked", Some(9)), None);
        assert_eq!(upload_bytes(root, "T/missing/file.bin", None), None);
    }

    #[test]
    fn storage_engine_status_feeds_pending_uploads() {
        let runtime = dvc::dvc_program();
        if !Path::new(&runtime).is_file() {
            eprintln!("skipping: managed storage runtime is unavailable");
            return;
        }
        let fixture = Fixture::new();
        fixture.write("README.md", b"base\n");
        dvc::execute_engine(&fixture.repo.root, ["init", "-q"]).unwrap();
        fixture.write("T/file.bin", &[1_u8; 20]);
        fixture.write("T/dir/a", &[2_u8; 5]);
        fixture.write("T/dir/b", &[3_u8; 6]);
        dvc::execute_engine(&fixture.repo.root, ["add", "-q", "T/file.bin", "T/dir"]).unwrap();
        let base = fixture.commit("tracked");
        fixture.write("T/file.bin", &[9_u8; 45]);
        fixture.write("T/dir/c", &[4_u8; 8]);
        fixture.remove("T/dir/b");
        let tree = fixture.tree();
        let pointers = ["T/dir.dvc".to_owned(), "T/file.bin.dvc".to_owned()];
        let mut inputs = fixture.inputs(&base, None, &tree, &pointers, &[]);
        inputs.inspect_outputs = true;

        let content = Config {
            s3: Some(S3Config {
                url: "/tmp/storage".to_owned(),
                endpoint_url: None,
            }),
            ..Config::default()
        };
        let usage = measure(&fixture.repo, &content, &inputs, u64::MAX).unwrap();
        // The recorded directory resolves from the local cache, so only the
        // new file is charged, not the deletion or the unchanged file.
        assert_eq!(usage.totals().1.s3_bytes, 45 + 8);
        assert!(usage.pending_uploads());

        let usage = measure(&fixture.repo, &versioned_config(), &inputs, u64::MAX).unwrap();
        assert_eq!(usage.totals().1.s3_bytes, 45 + 5 + 8);
        assert!(usage.pending_uploads());

        inputs.inspect_outputs = false;
        let usage = measure(&fixture.repo, &content, &inputs, u64::MAX).unwrap();
        assert_eq!(usage.totals().1.s3_bytes, 0);
        assert!(!usage.pending_uploads());

        // Once the engine records the new directory version, its manifest
        // resolves from the cache and the same new digests are charged.
        for pointer in &pointers {
            dvc::execute_engine(
                &fixture.repo.root,
                ["commit", "-q", "--force", "--", pointer],
            )
            .unwrap();
        }
        let tree = fixture.tree();
        let committed = fixture.inputs(&base, None, &tree, &pointers, &[]);
        let usage = measure(&fixture.repo, &content, &committed, u64::MAX).unwrap();
        assert_eq!(usage.totals().1.s3_bytes, 45 + 8);
        assert!(usage.pending_uploads());
        let paths = usage
            .contributors()
            .into_iter()
            .filter(|contributor| contributor.store == "s3")
            .map(|contributor| contributor.path)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            paths,
            BTreeSet::from(["T/dir/c".to_owned(), "T/file.bin".to_owned()])
        );
    }

    /// An S3 boundary path may not contain a backslash, but a file inside a
    /// directory boundary may, and it is charged under that exact name.
    #[test]
    fn dirty_files_with_backslashes_in_their_names_are_charged() {
        let fixture = Fixture::new();
        fixture.write("T/data/a.bin", &[1_u8; 400]);
        fixture.write("T/data/a\\b.bin", &[2_u8; 2_000]);
        assert!(fixture.repo.root.join("T/data/a\\b.bin").is_file());
        let status = dvc::parse_data_status(
            r#"{"uncommitted": {"modified": ["T/data/", "T/data/a\\b.bin"]}}"#,
        )
        .unwrap();
        let document = dvc::parse_pointer_document(
            "outs:\n- hash: md5\n  path: data\n  files:\n  - relpath: a.bin\n    md5: m1\n    size: 400\n    cloud:\n      workspace-mgr:\n        version_id: va\n  - relpath: a\\b.bin\n    md5: m2\n    size: 1\n    cloud:\n      workspace-mgr:\n        version_id: vb\n",
            "T/data.dvc",
        )
        .unwrap();
        let worktree = document.entries("T/data.dvc");
        assert_eq!(worktree[1].key, "T/data/a\\b.bin");
        let (dirty, directories) = dirty_uploads(&fixture.repo.root, &status, &worktree, true);
        assert_eq!(dirty, vec![("T/data/a\\b.bin".to_owned(), 2_000)]);
        assert!(directories.is_empty());

        let history = vec![vec![row(&[], &worktree)]];
        let usage = versioned_storage(
            &history,
            &[row(&worktree, &worktree)],
            &PendingSources {
                worktree: worktree.clone(),
                dirty,
                ..PendingSources::default()
            },
        );
        assert_eq!(usage.published_bytes, 401);
        assert_eq!(usage.projected_bytes, 401 + 2_000);
        assert!(usage.pending_uploads);
        assert!(!usage.contributors.contains_key("T/data/a/b.bin"));
    }

    fn digest(seed: u64) -> String {
        format!("{seed:032x}")
    }

    /// Writes a directory manifest and its file objects into the fixture's
    /// storage cache, the way the storage engine records a directory.
    fn cache_directory(fixture: &Fixture, manifest: u64, files: &[(&str, u64, usize)]) -> String {
        let cache = fixture.repo.root.join(".dvc/cache/files/md5");
        let object =
            |md5: &str, suffix: &str| cache.join(&md5[..2]).join(format!("{}{suffix}", &md5[2..]));
        let mut entries = Vec::new();
        for (relpath, seed, size) in files {
            let md5 = digest(*seed);
            let path = object(&md5, "");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, vec![0_u8; *size]).unwrap();
            entries.push(serde_json::json!({"md5": md5, "relpath": relpath}));
        }
        let md5 = digest(manifest);
        let path = object(&md5, ".dir");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec(&entries).unwrap()).unwrap();
        format!("{md5}.dir")
    }

    #[test]
    fn content_addressed_directories_resolve_from_the_local_cache() {
        let fixture = Fixture::new();
        fixture.write("README.md", b"base\n");
        fixture.write(".gitignore", b".dvc/cache\n");
        let base = fixture.commit("base");
        let aggregate = |md5: &str, size: u64| {
            format!("outs:\n- md5: {md5}\n  size: {size}\n  nfiles: 3\n  hash: md5\n  path: data\n")
        };
        let first = cache_directory(
            &fixture,
            0xd1,
            &[
                ("a.bin", 0xa1, 500),
                ("b.bin", 0xb1, 500),
                ("c.bin", 0xc1, 100),
            ],
        );
        fixture.write("T/data.dvc", aggregate(&first, 1_100).as_bytes());
        let target = fixture.commit("publish data");
        let pointers = ["T/data.dvc".to_owned()];
        let config = Config {
            s3: Some(S3Config {
                url: fixture
                    .repo
                    .root
                    .join("absent-remote")
                    .display()
                    .to_string(),
                endpoint_url: None,
            }),
            ..Config::default()
        };
        let measure_tree = |tree: &str| {
            measure(
                &fixture.repo,
                &config,
                &fixture.inputs(&base, Some(&target), tree, &pointers, &[]),
                u64::MAX,
            )
            .unwrap()
        };

        // A same-size rewrite of one file is charged as new content.
        let rewritten = cache_directory(
            &fixture,
            0xd2,
            &[
                ("a.bin", 0xa2, 500),
                ("b.bin", 0xb1, 500),
                ("c.bin", 0xc1, 100),
            ],
        );
        fixture.write("T/data.dvc", aggregate(&rewritten, 1_100).as_bytes());
        let usage = measure_tree(&fixture.tree());
        assert_eq!(usage.totals().0.s3_bytes, 1_100);
        assert_eq!(usage.totals().1.s3_bytes, 1_600);
        assert!(usage.pending_uploads());
        assert!(
            usage
                .contributors()
                .iter()
                .any(|contributor| contributor.path == "T/data/a.bin"
                    && contributor.store == "s3"
                    && contributor.bytes == 1_000
                    && contributor.versions == 2
                    && contributor.state == "pending")
        );

        // A pure deletion is free and uploads nothing.
        let deleted = cache_directory(
            &fixture,
            0xd3,
            &[("a.bin", 0xa1, 500), ("b.bin", 0xb1, 500)],
        );
        fixture.write("T/data.dvc", aggregate(&deleted, 1_000).as_bytes());
        let usage = measure_tree(&fixture.tree());
        assert_eq!(usage.totals().1.s3_bytes, 1_100);
        assert!(!usage.pending_uploads());

        // A version whose manifest is missing is charged whole.
        fixture.write(
            "T/data.dvc",
            aggregate(&format!("{}.dir", digest(0xd4)), 1_234).as_bytes(),
        );
        let usage = measure_tree(&fixture.tree());
        assert_eq!(usage.totals().1.s3_bytes, 1_100 + 1_234);
        assert!(usage.pending_uploads());
    }
}
