# End-to-end test

`run.py` treats the compiled CLI as an external program and exercises the public
lifecycle against two local network services:

- a versioned MinIO bucket through the S3 API;
- a bare repository through `git daemon`, including an intentional server-side
  rejection.

The scenario covers initialization and adoption, instructions, diagnostics,
task creation, storage status, explicit Git and S3 placement, automatic
placement, reset, hydrate, move, scoped plan and publish, configuration drift and
repair, failure ordering, shared-checkout refresh, and fresh-clone hydration.
It checks that placement commands never write either remote, that S3 object
versions exist before the corresponding Git ref, and that remote failure cannot
produce a Git commit pointing to missing content. Every command and assertion is
recorded in `evidence.jsonl`.

GitHub Actions owns the MinIO container and installs the exact private storage
runtime. The test owns only newly created repositories, buckets, caches, and
refs under its runner directory. It never reads developer configuration or
credentials.
