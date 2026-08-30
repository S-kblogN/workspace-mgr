# End-to-end test

[`COVERAGE.md`](COVERAGE.md) is the auditable coverage contract: it separates
cross-service invariants that must run here from parser permutations and fault
injection that are intentionally kept in isolated integration tests.

`run.py` treats the compiled CLI as an external program and exercises the public
lifecycle against two local network services:

- a versioned MinIO bucket through the S3 API;
- a bare repository through `git daemon`, including an intentional server-side
  rejection.

The scenario covers private-runtime setup, initialization,
instructions, diagnostics, task creation, storage status, explicit Git and S3
placement, aggregate boundary sizing, tiny-S3 warnings, the semantic review
band, automatic placement, reset, hydrate, move, scoped plan and publish,
configuration drift and repair, disabled-versioning refusal before upload,
isolated infrastructure task scaffolding and scoped publication,
fixed policy with a minimal Git/S3-only public configuration,
Git-to-S3 and S3-to-Git transitions, failure ordering, successful and rolled-back
ordinary Git/S3 shared-checkout refresh, fresh-clone hydration, missing exact S3
versions, cross-process locks, alternate checkouts, and network non-fast-forwards.
It checks that placement commands never write either remote, that S3 object
versions exist before the corresponding Git ref, and that remote failure cannot
produce a Git commit pointing to missing content. Every command and assertion is
recorded in `evidence.jsonl`.

GitHub Actions owns the MinIO container and installs the exact private storage
runtime. The test owns only newly created repositories, buckets, caches, and
refs under its runner directory. It never reads developer configuration or
credentials.
