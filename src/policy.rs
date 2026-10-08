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
pub const CLOUD_USAGE_APPROVAL_BYTES: u64 = 1_073_741_824;
pub const INSTRUCTION_POLICY_VERSION: u32 = 15;
/// The first workspace-mgr release that reads task manifest schema 3, which
/// adds the optional `[cloud_usage_approval]` table.
pub const TASK_SCHEMA_3_MINIMUM_CLI_VERSION: semver::Version = semver::Version::new(0, 4, 0);
/// The first release that reads the optional saved task review record format.
pub const TASK_SCHEMA_4_MINIMUM_CLI_VERSION: semver::Version = semver::Version::new(0, 7, 0);
/// The first release that reads task manifests below date-grouping directories.
pub const ARCHIVED_TASK_PATH_MINIMUM_CLI_VERSION: semver::Version = semver::Version::new(0, 5, 0);
/// The first release that fences archive copies and preserves protected,
/// unmapped source generations through complete-prefix retirement retries.
pub const ARCHIVE_STORAGE_PROTOCOL_MINIMUM_CLI_VERSION: semver::Version =
    semver::Version::new(0, 7, 0);
/// Private copy journals are deliberately incompatible with 0.6.0's
/// uncoordinated resume path. Public, immutable archive receipts remain schema 1.
pub const ARCHIVE_COPY_JOURNAL_SCHEMA_VERSION: u32 = 2;
/// First release that reads native storage manifests.
pub const NATIVE_STORAGE_MINIMUM_CLI_VERSION: semver::Version = semver::Version::new(0, 8, 1);

pub const REVIEW_PULL_REQUEST: &str = "required";
pub const REVIEW_INITIAL_STATE: &str = "draft";
pub const REVIEW_MANAGED_BY: &str = "agent";
pub const REVIEW_MERGE_AUTHORITY: &str = "user";
pub const REVIEW_DELIVERABLE_CREATION_TIMING: &str = "immediate-after-scaffold-publication";
pub const REVIEW_INFRASTRUCTURE_CREATION_TIMING: &str = "after-first-scoped-publication";
pub const REVIEW_SYNC_CADENCE: &str = "before-every-turn-end";

/// The oldest workspace-mgr version able to read a task manifest of this
/// schema, or `None` when every release that knows the schema can read it.
pub fn minimum_cli_version_for_task_schema(schema_version: u32) -> Option<semver::Version> {
    match schema_version {
        3 => Some(TASK_SCHEMA_3_MINIMUM_CLI_VERSION),
        4 => Some(TASK_SCHEMA_4_MINIMUM_CLI_VERSION),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_task_metadata_schemas_raise_the_minimum_cli_version() {
        assert_eq!(minimum_cli_version_for_task_schema(1), None);
        assert_eq!(minimum_cli_version_for_task_schema(2), None);
        assert_eq!(
            minimum_cli_version_for_task_schema(3).map(|version| version.to_string()),
            Some("0.4.0".to_owned())
        );
        assert_eq!(
            minimum_cli_version_for_task_schema(4).map(|version| version.to_string()),
            Some("0.7.0".to_owned())
        );
    }

    /// Publication writes each schema's minimum release into repositories and
    /// refuses to write one the build does not meet, so a release whose
    /// package version is below any mapped minimum could not publish that
    /// schema. The package version is read directly, never through the
    /// test-only release override, and the `production_build_` prefix makes
    /// the production-configuration CI step run it too.
    #[test]
    fn production_build_package_version_meets_every_task_schema_minimum() {
        let package = semver::Version::parse(env!("CARGO_PKG_VERSION"))
            .expect("the package version is valid semver");
        let mut mapped = 0;
        for schema in 0..=u32::from(u8::MAX) {
            let Some(required) = minimum_cli_version_for_task_schema(schema) else {
                continue;
            };
            mapped += 1;
            assert!(
                crate::config::cli_version_satisfies(&package, &required),
                "package version {package} is below the workspace-mgr {required} that task manifest schema {schema} requires; release this schema as {required} or newer"
            );
        }
        assert!(
            mapped > 0,
            "no task manifest schema maps to a minimum release"
        );
        assert!(
            crate::config::cli_version_satisfies(&package, &ARCHIVED_TASK_PATH_MINIMUM_CLI_VERSION),
            "package version {package} is below the workspace-mgr {ARCHIVED_TASK_PATH_MINIMUM_CLI_VERSION} that archived task paths require"
        );
        assert!(
            crate::config::cli_version_satisfies(
                &package,
                &ARCHIVE_STORAGE_PROTOCOL_MINIMUM_CLI_VERSION
            ),
            "package version {package} is below the workspace-mgr {ARCHIVE_STORAGE_PROTOCOL_MINIMUM_CLI_VERSION} that archive storage coordination requires"
        );
        assert!(
            crate::config::cli_version_satisfies(&package, &NATIVE_STORAGE_MINIMUM_CLI_VERSION),
            "package version {package} is below the workspace-mgr {NATIVE_STORAGE_MINIMUM_CLI_VERSION} that native storage metadata requires"
        );
    }
}
