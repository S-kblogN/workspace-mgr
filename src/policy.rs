pub const TASK_DIRECTORY_PATTERN: &str = "%Y%m%d-%H%M%S-{slug}";
pub const TASK_MANIFEST_NAME: &str = ".workspace-mgr-task.toml";
pub const TASK_BRANCH_PREFIX: &str = "codex/";
pub const ROOT_IGNORE_NAME: &str = ".gitignore";
pub const REPOSITORY_IGNORE_MODULE: &str = ".workspace-mgr/repository.gitignore";
pub const REPOSITORY_MODULE_MAX_BYTES: usize = 65_536;
pub const RECOMMENDED_S3_MINIMUM_BYTES: u64 = 1_048_576;
pub const AUTO_S3_ABOVE_BYTES: u64 = 10_485_760;
pub const BULK_PUBLICATION_FILES: u64 = 200;
pub const BULK_PUBLICATION_BYTES: u64 = 268_435_456;
/// The byte threshold in the unit every rendered rule and document states it
/// in, derived from the threshold itself so the two cannot disagree.
pub const BULK_PUBLICATION_MIB: u64 = BULK_PUBLICATION_BYTES / 1_048_576;
pub const INSTRUCTION_POLICY_VERSION: u32 = 10;

pub const REVIEW_PULL_REQUEST: &str = "required";
pub const REVIEW_INITIAL_STATE: &str = "draft";
pub const REVIEW_MANAGED_BY: &str = "agent";
pub const REVIEW_MERGE_AUTHORITY: &str = "user";
pub const REVIEW_DELIVERABLE_CREATION_TIMING: &str = "immediate-after-scaffold-publication";
pub const REVIEW_INFRASTRUCTURE_CREATION_TIMING: &str = "after-first-scoped-publication";
pub const REVIEW_SYNC_CADENCE: &str = "before-every-turn-end";
