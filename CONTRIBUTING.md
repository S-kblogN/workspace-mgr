# Contributing

Contributions are welcome through focused pull requests.

Before submitting a change, run:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

Tests that exercise DVC must use a new temporary repository and a local
filesystem remote. They must not read user credentials, contact a cloud remote,
or depend on deleting remote objects after the test.
