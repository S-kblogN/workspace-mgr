# Release process

`Cargo.toml` is the single source of truth for the desired release version.
Merging a reviewed version and matching dated `CHANGELOG.md` section into
`main` declares that version ready for release. No separate version file,
manually pushed tag, or maintainer-side `cargo publish` command is used.

Every update to `main` runs the complete CI and end-to-end gates through the
release workflow. After those gates pass, the workflow compares the declared
version with crates.io and GitHub:

- If crates.io does not contain the version, the workflow publishes it with
  `cargo publish --locked`.
- If the exact version is already present, publishing is a no-op.
- The workflow creates the annotated `v<version>` tag only after crates.io has
  accepted the package and verified that the package records the intended Git
  revision.
- It builds and verifies native archives for Linux x86-64, Linux arm64, and
  Apple Silicon macOS, then creates or repairs the matching GitHub Release.
- A prerelease Cargo version creates a GitHub prerelease.

The state checks are resumable. If crates.io accepted a version before a later
step failed, the next run reads `.cargo_vcs_info.json` from the immutable crate,
recovers the exact source revision, and completes the tag or missing release
assets without republishing. A tag that disagrees with the published source, a
release without its tag, or a GitHub tag created before crates.io publication is
an error rather than an instruction to overwrite state.

## Preparing a release

1. In a review branch, update `package.version` in `Cargo.toml` and let Cargo
   update the root package version in `Cargo.lock`.
2. Move the release notes out of `[Unreleased]` into a dated
   `## [<version>] - YYYY-MM-DD` section.
3. Review the package with `cargo publish --dry-run --locked` and merge the
   release change through the normal pull-request process.
4. Monitor the `Release` workflow. A successful run means crates.io, the tag,
   all six archive/checksum assets, and the GitHub Release have converged.

Ordinary changes that do not alter the version still run the main-branch gates;
the release portion reports that the declared version is already complete.
Workflow dispatch exists only for retrying or auditing the same declarative
state and refuses to release a non-`main` revision.

## Authentication

The `release` GitHub Environment owns publication authority. Pull-request
workflows have read-only permissions, do not use that environment, and cannot
read release credentials.

For the first publication of the crate name, crates.io cannot yet associate a
Trusted Publisher with the nonexistent crate. The workflow therefore requires
a short-lived Environment secret named `CRATES_IO_BOOTSTRAP_TOKEN` only when the
crate itself does not exist. Revoke that token and remove the secret immediately
after the first successful release.

After the first publication, configure crates.io Trusted Publishing for:

- repository owner: `S-kblogN`
- repository: `workspace-mgr`
- workflow: `release.yml`
- environment: `release`

All later versions use `rust-lang/crates-io-auth-action` to exchange the GitHub
OIDC identity for a short-lived crates.io token. The workflow does not fall back
to the bootstrap secret after the crate name exists.

## Native packages

Each archive contains the executable, installer, MIT License, README, and linked
user documentation. The build job verifies its checksum on the native runner;
the publication job downloads all three archives, verifies all six files again,
and refuses incomplete output before creating a GitHub Release.

The release workflow creates annotated tags through the GitHub API. Repository
tag rules should reject updates and deletion for `v*` after creation; workflow
identity, the published crate's recorded Git revision, and the protected tag
together replace the former maintainer-side signed-tag ceremony.
