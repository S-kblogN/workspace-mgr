# Contributing

Contributions are welcome through focused pull requests.

Before submitting a change, run:

```sh
cargo fmt --check
cargo deny check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

Tests that exercise managed storage must use a new temporary repository and
either the test-only filesystem adapter or the CI-owned versioned MinIO service.
They must not read user credentials, contact a real cloud remote, or depend on
deleting shared remote objects after the test.
