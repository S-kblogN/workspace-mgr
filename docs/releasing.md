# Release process

Phase 1 produces release candidates but does not publish a public package.

1. Run the local quality gate documented in the README.
2. Push a review branch and let CI produce the four native candidate artifacts.
3. After the workflow exists on the default branch, the standalone **Package
   release candidate** workflow can reproduce them from a reviewed commit.
4. Download each immutable workflow artifact, verify its SHA-256 checksum, and
   exercise the binary on its named architecture.

The packaging workflow builds native archives for Linux x86-64 and arm64 and
macOS arm64 and Intel. Each archive contains the executable, MIT License, and
README. Artifacts expire after 14 days and are not a GitHub Release.

Phase 4 adds the irreversible publication steps: a reviewed version/tag,
crates.io publication, durable GitHub Release assets, and release notes. A
crates.io token or GitHub release permission must never be exposed to pull
request workflows.
