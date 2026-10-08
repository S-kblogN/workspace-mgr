# Performance and verification

Performance changes retain the existing control-plane and storage integrity
checks. Their aim is to share work within one operation, batch Git requests, and
run independent computation or network requests concurrently.

## Shared work

- Git trees and immutable blobs are read with `ls-tree -z` and `cat-file --batch`.
  Blob IDs, modes, framing, missing objects, and requested paths are checked.
  Index removal and staging use NUL-delimited batches, including unusual paths.
- Cache routing is discovered once per engine or verification phase. Every candidate still receives
  its path traversal, file type, size, and checksum checks. Directory manifests
  are parsed once and remote version inventories are shared across their entries.
  Publish revalidates between status, record, upload, and final verification;
  hashes are not shared across those transaction boundaries. Legacy YAML paths
  retain their separate normalization and hash-selection compatibility checks.
- Hashes are reused only within a verification phase. On Unix, reuse requires the
  same device, inode, size, nanosecond modification time, and change time. The open
  descriptor and pathname are checked before and after reading. Other platforms
  do not reuse a hash on the strength of file metadata alone.
- Full verification computes raw MD5, legacy normalized MD5, and SHA256 in one
  pass. Status checks that need only MD5 do not pay for SHA256. The legacy text
  normalization heuristic and its chunk boundaries remain unchanged.
- Upload workers do not modify manifests. Completed exact-version bindings are
  merged once and saved atomically. Failed or interrupted uploads retain their
  durable ownership journals for recovery.

## Concurrency

Local hashing and cache validation use the available CPU parallelism. Nested
directory jobs share that budget rather than creating a pool per file. Exact
remote reads, doctor object checks, and independent scoped inventories use at
most 16 workers. Uploads use four object workers and up to four multipart workers
per object. These limits bound open files, connections, and memory.
Doctor splits its registry worker budget between independent sources and their
version histories, keeping the combined network concurrency at most 16.

Unarchived object retirement batches its exact versions with S3 `DeleteObjects`,
up to 1,000 per request, while retaining the ancestor registry checks and final
version inventory. Every response must cover the requested exact key/version
pairs once; HTTP 200 with an item error fails the operation. A regression fixture
retires 2,005 versions in three requests and proves that partial failure can be
retried without deleting neighboring objects. Archive retirement and cancellation
retain their per-version authorization and journal protocol.

Pagination within a single inventory remains ordered. Archive copies retain
their mutation order because lost-response recovery and version ownership depend
on it. Compressed Git usage accounting retains `pack.threads=1`: changing its
packing result would also change quota approval decisions. These are correctness
constraints, not unchecked performance shortcuts.

## S3 checksums

Doctor requests `HeadObject` for the exact version with `ChecksumMode=ENABLED`.
It accepts a correctly encoded `FULL_OBJECT` MD5 or SHA256 only when that digest
can prove the manifest checksum and, where present, the checked local bytes.
For normalized MD5, a raw provider checksum needs the verified local raw digest
to bridge the two representations. A SHA256 alone cannot prove an unmaterialized
MD5 manifest. ETags and arbitrary user metadata are not checksum evidence.

CRC checksums, composite checksums, unavailable or malformed checksums, and
normalization ambiguities fall back to a full exact-version GET. The GET is
streamed through the combined hash calculation and local byte comparison; it no
longer creates and rereads a temporary payload file. Both paths retain size,
ETag, exact version, layout, and before/after inventory checks. Local metadata
and payload generation snapshots are also checked after the audit. Provider
digest equality is a cryptographic checksum proof, with the collision limits of
the underlying algorithm.

New uploads to official AWS S3 HTTPS endpoints send a provider-validated SHA256
alongside the existing MD5 and request signing, using the same local digest.
Custom S3 endpoints, including B2, retain their existing upload protocol until
support for that header is established. A read-only probe of two existing B2
objects returned only `FULL_OBJECT` CRC32 through S3, so those objects still
require streamed verification. No B2 Native API is used; the backend remains
`s3`.

Protocol references: [AWS HeadObject](https://docs.aws.amazon.com/AmazonS3/latest/API/API_HeadObject.html),
[AWS PutObject](https://docs.aws.amazon.com/AmazonS3/latest/API/API_PutObject.html),
[AWS GetObjectAttributes](https://docs.aws.amazon.com/AmazonS3/latest/API/API_GetObjectAttributes.html),
[AWS DeleteObjects](https://docs.aws.amazon.com/AmazonS3/latest/API/API_DeleteObjects.html),
[B2 S3 DeleteObjects](https://www.backblaze.com/apidocs/s3-delete-objects).

## Measurements

The initial read-only diagnosis used release 0.8.4 in Sunnyvale's
`biocorda-workspace`. Its manifests declared 13,953 entries totaling 3.881 GiB;
13,851 entries had exact remote bindings. A full doctor was still running at
the 30-second diagnostic limit and had read about 4.56 GB locally without
printing a result. This is a bounded observation, not its total runtime.

An isolated macOS Git microbenchmark checked identical file sets and blob bytes:

| Work on 1,000 files | Previous sequence | Batched sequence |
| --- | --- | --- |
| Index removal | 1,001 Git processes, 17.3338 s | 2 processes, 0.0362 s |
| Immutable metadata reads | 1,000 processes, 17.2404 s | 2 processes, 0.0295 s |

These measure the specific subprocess sequences, not complete plan or publish
latency. Integration tests were also running during the measurement.

The reproducible [doctor benchmark](../scripts/benchmark-doctor.py) runs complete
CLI audits against a temporary loopback S3 fixture. It verifies the official
release archive's SHA256 before comparison and uses isolated Git/AWS configuration
with dummy credentials. Each case has 128 materialized objects of 64 KiB and
20 ms delay per HEAD or payload GET. The official release 0.8.5 was compared with
the optimized working-tree debug build after the final rebuild, with no concurrent
build or test work. Release 0.8.5's doctor storage source is unchanged from 0.8.4.

| Provider checksum | Release time | Current time | Payload GETs, release → current | Peak concurrency, release → current |
| --- | --- | --- | --- | --- |
| CRC32 only | 4.8434 s | 0.9963 s | 128 → 128 | 1 → 16 |
| FULL_OBJECT SHA256 | 4.8495 s | 0.7751 s | 128 → 0 | 1 → 16 |

All four audits reported the same 128 expected objects and no integrity issues.
The CRC32 case downloaded the full 8 MiB in both builds; the SHA256 case reduced
payload downloads from 8 MiB to zero. The fixture uses one metadata boundary and
does not demonstrate history batching or real B2 throughput. Scratch payload
bytes are an estimate from the implementation, not filesystem instrumentation.
These runs were about 4.86× faster for CRC32 and 6.26× faster for SHA256.
An earlier run against release 0.8.4 during concurrent integration tests measured
6.5478 → 2.7182 s for CRC32 and 6.2707 → 2.4405 s for SHA256; background load
affects these timings.
Updated full-workspace timings require a stable target workspace and are not
inferred from these benchmarks.
