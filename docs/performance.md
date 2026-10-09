# Performance and verification

Performance changes retain the existing control-plane and storage integrity
checks. Their aim is to share work within one operation, batch Git requests, and
run independent computation or network requests concurrently.

## Shared work

- Git trees and immutable blobs are read with `ls-tree -z` and `cat-file --batch`.
  Blob IDs, modes, framing, missing objects, and requested paths are checked.
  Index removal and staging use NUL-delimited batches, including unusual paths.
- Refresh compares overlay paths with bounded literal tree and filtered hash
  batches, then materializes approved ordinary Git paths with one NUL-delimited
  `checkout-index` request. Executable modes, symlinks, filters and overlay
  conflicts remain checked. Temporary historical worktrees load their exact
  index and check out only storage controls, avoiding unrelated payloads and
  their smudge filters.
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
retried without deleting neighboring objects. Archive retirement revalidates
the complete published receipt, canonical registry and destination history
before each batch of at most 1,000 exact versions and once after mutation.
Pre-versioning `null` source versions use freshly guarded single-version
deletion. A final source inventory runs after the destination checks, so new
source writes remain pending. Cancellation retains its per-version ownership
and journal protocol.

Cleanup saves each confirmed source-prefix group atomically, keeping generic
and receipt aliases of the same physical version together. Generic cleanup
groups have at most 1,000 queued records. An interrupted or failed group remains
retryable, while previous checkpoints survive. Refresh preserves the successful
Git result and reports cleanup errors, durable pending versions and prefixes
in JSON; group progress goes to stderr.

Task rename uses the same server-side history copy as archive at publication.
Unchanged copied payloads require no client payload transfer. Active renamed
tasks retain normal reconciliation for later edits. This marked receipt requires
CLI 0.8.11; ordinary archive receipts continue to require 0.8.10.

Pagination within a single inventory remains ordered. Archive copies retain
their mutation order because lost-response recovery and version ownership depend
on it. Compressed Git usage accounting retains `pack.threads=1`: changing its
packing result would also change quota approval decisions. These are correctness
constraints, not unchecked performance shortcuts.

## Shared exact-version verification

Schema 2 manifests record a verified association between raw-byte SHA256 and an
exact endpoint, bucket, key and non-null VersionId. Local files are checked
against this shared record. Remote verification checks the immutable version's
existence, scope, physical size and ETag, while retaining the layout and
transaction inventories. It does not download the payload, even when the
provider exposes only CRC checksums. Verification records do not expire by time;
changes to bytes or storage identity require a new record.
The record repeats its exact VersionId and must match its enclosing version
binding. Bound schema 2 cache entries use `objects/sha256/`; MD5 remains the
logical checksum and the routing identity of older or unbound cache entries.

New uploads establish the association through signed payload SHA256 and
Content-MD5 checks, bounded ordered multipart completion where needed, and an
exact-version metadata check. Source SHA256 is checked against the bytes being
uploaded; interrupted uploads retain evidence of the verified transfer in their
private ownership journal. A journal from an older release cannot acquire this
status merely because its ownership token or VersionId still exists.

`manage` upgrades schema 1 bindings once, after establishing the raw-byte
association. A reliable provider checksum can avoid reading the remote payload;
bindings lacking suitable evidence use the previous streamed verification path.
Dry runs list pending upgrades without downloading file payloads. Historical
schema 1 manifests continue to use the compatibility path below.

## Legacy S3 checksum verification

For schema 1, doctor requests `HeadObject` for the exact version with
`ChecksumMode=ENABLED`.
It accepts a correctly encoded `FULL_OBJECT` MD5 to verify an unmaterialized
raw-MD5 manifest. When local bytes are present, skipping GET requires a matching
`FULL_OBJECT` SHA256 of those checked raw bytes; matching MD5 alone retains the
literal byte comparison. The local manifest checksum is still checked, including
normalized MD5. A regression test verifies a [public MD5 collision pair](https://www.mscs.dal.ca/~selinger/md5collision/)
and proves its differing local and remote bytes are detected. A SHA256 alone
cannot prove an unmaterialized MD5 manifest. ETags and arbitrary user metadata
are not checksum evidence.

CRC checksums, composite checksums, unavailable or malformed checksums, and
normalization ambiguities fall back to a full exact-version GET. The GET is
streamed through the combined hash calculation and local byte comparison; it no
longer creates and rereads a temporary payload file. Both paths retain size,
ETag, exact version, layout, and before/after inventory checks. Local metadata
and payload generation snapshots are also checked after the audit. Provider
digest equality is a cryptographic checksum proof, with the collision limits of
the underlying algorithm.

New uploads to official AWS S3 HTTPS endpoints also send a provider-validated SHA256
alongside the existing MD5 and request signing, using the same local digest.
Custom S3 endpoints, including B2, use the signed SHA256 and Content-MD5 upload
protocol; schema 2 records the resulting verified version association without
depending on a remotely retrievable SHA256 header. A read-only probe of two
existing B2 objects returned only `FULL_OBJECT` CRC32 through S3. Schema 1 objects
with that response retain streamed verification until they are upgraded. No B2
Native API is used; the backend remains `s3`.

An upload response must identify the exact version before the signed transfer
can establish its proof. If that response is lost, the ownership token locates
a recovery candidate but does not prove its content: an offered full-object
SHA256 or an exact-version read must verify the candidate first. This recovery
does not change the metadata-only verification of already proven schema 2
bindings.

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
20 ms delay per HEAD or payload GET. These measurements used schema 1 manifests
before the schema 2 upgrade. The official release 0.8.5 was compared with
the optimized working-tree debug build after that rebuild, with no concurrent
build or test work. Release 0.8.5's doctor storage source is unchanged from 0.8.4.

| Provider checksum | Release time | Current time | Payload GETs, release → current | Peak concurrency, release → current |
| --- | --- | --- | --- | --- |
| CRC32 only | 4.8413 s | 0.9691 s | 128 → 128 | 1 → 16 |
| FULL_OBJECT SHA256 | 4.8355 s | 0.7415 s | 128 → 0 | 1 → 16 |

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

Refresh regressions cover 1,205 paths, including long names, tabs, newlines,
glob characters, executable files, symlinks and Git filters. The Git trace
requires bounded hash batches and one checkout request rather than a subprocess
sequence per file. A 512-version archive retirement fixture requires one delete
batch and 1,024 destination HEAD checks across its pre/post guards. A 1,001-version
fixture checks that registry changes between batches stop the second mutation.
These are request-count and safety assertions, not measured B2 wall-clock gains.
