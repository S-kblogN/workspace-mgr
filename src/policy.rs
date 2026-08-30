pub const TASK_DIRECTORY_PATTERN: &str = "%Y%m%d-%H%M%S-{slug}";
pub const TASK_MANIFEST_NAME: &str = ".workspace-mgr-task.toml";
pub const TASK_BRANCH_PREFIX: &str = "codex/";
pub const RECOMMENDED_S3_MINIMUM_BYTES: u64 = 1_048_576;
pub const AUTO_S3_ABOVE_BYTES: u64 = 10_485_760;
pub const INSTRUCTION_POLICY_VERSION: u32 = 3;

pub const REVIEW_PULL_REQUEST: &str = "required";
pub const REVIEW_INITIAL_STATE: &str = "draft";
pub const REVIEW_MANAGED_BY: &str = "agent";
pub const REVIEW_MERGE_AUTHORITY: &str = "user";
