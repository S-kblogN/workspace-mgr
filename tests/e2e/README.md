# End-to-end test

`run.py` treats the compiled CLI as an external program and exercises the
complete repository lifecycle against two network service boundaries:

- a versioned MinIO bucket accessed through the S3 API;
- a bare repository served over `git://` by `git daemon`, with network fetch,
  push, ref inspection, and an intentional server-side rejection.

The scenario covers initialization and adoption, configuration, instruction
rendering, diagnostics, task creation and status, scoped planning and
publication, version-aware managed-storage track/update/hydrate/move/untrack
operations, internal-configuration drift and repair, failure ordering,
shared-checkout refresh, a fresh-clone hydrate, and the explicit Git-only
exception. Each command and assertion is written to
`evidence.jsonl` for CI inspection.

The GitHub Actions workflow owns the MinIO container and installs the S3-enabled
DVC 3.67.1 private runtime. The test owns only newly created repositories,
buckets, caches, and refs in its isolated runner directory; it never reads a
developer's storage configuration or cloud credentials.
