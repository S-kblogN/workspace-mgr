# Changelog

All notable changes to this project will be documented in this file. The format
is based on Keep a Changelog, and this project follows Semantic Versioning.

## [Unreleased]

### Added

- Initial standalone Rust CLI project.
- Repository initialization, effective instructions, diagnostics, and task
  scaffolding.
- Scoped Git and DVC transaction commands with strict repository and task
  schemas.
- Shared-checkout refresh and rollback support.
- Isolated local-DVC and Git transaction integration tests, including refusal
  and rollback guards.
- Linux and macOS CI plus native packaging workflows.
