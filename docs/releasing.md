# Release process

1. Update the package version and changelog in a review branch.
2. Run the quality gate documented in the README.
3. Review and merge the release change.
4. Create a signed `v<version>` tag from the reviewed default-branch commit.
5. Run the package workflow for that tag and verify every SHA-256 checksum on
   its named architecture. With the archive and checksum in one directory, run
   `shasum -a 256 -c <archive>.sha256` from that directory.
6. Publish the crate from the tagged source with `cargo publish --locked`.
7. Extract each archive in a clean environment and verify `install.sh` can
   provision the private runtime.
8. Create a GitHub Release for the same tag and attach the three verified native
   archives and checksum files.

The packaging workflow builds native archives for Linux x86-64 and arm64 and
macOS on Apple Silicon. Each archive contains the executable, installer, MIT
License, README, and linked user documentation.

Publishing crates.io packages, tags, or durable GitHub Release assets is an
explicit maintainer action. Credentials and release permissions must never be
available to pull-request workflows.
