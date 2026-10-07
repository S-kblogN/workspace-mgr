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

## Information locality

Keep information at its point of use. Default `instructions` and explicit
`instructions all` contain the mental model, operation discovery and genuinely
session-wide constraints, rather than concatenating every operation's policy.
Put prerequisites, applicable rules and procedures in the relevant command help.
Put facts, conditional decisions and outcome-specific reminders in that
operation's execution report. A relocation-success reminder belongs only to a
successful relocation report, never to unconditional global instructions.

Detailed compatibility topics may remain available on demand. Moving a policy
out of global output is not authorization to weaken or remove it. Every change
must preserve the underlying policy and make its relevant command entrypoint
reachable. Repository-owned instructions stay user text: index the module in
global output, expose it through `instructions repository`, and retain hash
sensitivity to its current bytes.

Verify compact default/all output, operation-help coverage, retained policies,
repository-module reachability and outcome-only notices with isolated tests.
The [routing audit](docs/control-plane-audit.md) records the current map and the
explicitly authorized content-boundary changes.
