"""Durable exact-version archive mappings shared by all Git revisions.

The module imports only the standard library until the command-line adapter is
run. Receipt object names are relative to the configured remote prefix; lookup
callers pass full bucket-relative S3 keys.
"""

from __future__ import annotations

import hashlib
import inspect
import json
from pathlib import PurePosixPath
import sys
import threading

REGISTRY_SCHEMA = 1
MAX_ARCHIVE_HOPS = 32


def missing_object(error):
    code = (getattr(error, "response", None) or {}).get("Error", {}).get("Code")
    if code is not None:
        return code in ("NoSuchKey", "NoSuchVersion", "NotFound", "404")
    return isinstance(error, FileNotFoundError)


def relative_path(value, field):
    if not isinstance(value, str) or not value:
        raise RuntimeError(f"invalid archive registry {field}")
    path = PurePosixPath(value)
    if path.is_absolute() or ".." in path.parts or path.as_posix() != value:
        raise RuntimeError(f"invalid archive registry {field}: {value!r}")
    return value


def full_key(remote_prefix, object_name):
    return f"{remote_prefix}/{object_name}" if remote_prefix else object_name


def relative_object(value, field):
    # S3 object names are flat keys: a trailing slash, doubled separator, or
    # dot component is literal data, not a filesystem directory traversal.
    if not isinstance(value, str) or not value or value.startswith("/"):
        raise RuntimeError(f"invalid archive registry {field}")
    return value


def registry_key(remote_prefix, source):
    source = relative_path(source, "source")
    digest = hashlib.sha256(source.encode("utf-8")).hexdigest()
    return full_key(remote_prefix, f".workspace-mgr/archive/{digest}.json")


def validate_receipt(receipt, bucket, remote_prefix):
    if not isinstance(receipt, dict) or receipt.get("schema_version") != REGISTRY_SCHEMA:
        raise RuntimeError("unsupported archive registry receipt schema")
    source = relative_path(receipt.get("source"), "source")
    destination = relative_path(receipt.get("destination"), "destination")
    if source == destination:
        raise RuntimeError("archive registry source and destination are identical")
    if receipt.get("bucket") != bucket or receipt.get("remote_prefix") != remote_prefix:
        raise RuntimeError("archive registry receipt selects another storage location")
    if receipt.get("remote") != "workspace-mgr":
        raise RuntimeError("archive registry receipt selects another remote")
    versions = receipt.get("versions")
    if not isinstance(versions, list):
        raise RuntimeError("archive registry receipt has no complete version mapping")
    identities = {}
    for row in versions:
        if not isinstance(row, dict):
            raise RuntimeError("invalid archive registry version mapping")
        old = relative_object(row.get("source_object"), "source object")
        new = relative_object(row.get("destination_object"), "destination object")
        if not old.startswith(source + "/"):
            raise RuntimeError("archive registry object escapes its source task")
        expected = destination + old[len(source):]
        if new != expected:
            raise RuntimeError("archive registry object does not match its destination")
        for field in ("source_version_id", "destination_version_id"):
            if not isinstance(row.get(field), str) or not row[field]:
                raise RuntimeError(f"archive registry mapping has no exact {field}")
        if row["destination_version_id"] == "null":
            raise RuntimeError("archive registry mapping has no exact destination_version_id")
        if not isinstance(row.get("delete_marker"), bool):
            raise RuntimeError("archive registry mapping has no marker type")
        if not row["delete_marker"]:
            size = row.get("size")
            if not isinstance(size, int) or isinstance(size, bool) or size < 0:
                raise RuntimeError("archive registry data mapping has no valid size")
            etag = row.get("destination_etag")
            if not isinstance(etag, str) or not etag:
                raise RuntimeError("archive registry data mapping has no destination etag")
        identity = (old, row["source_version_id"])
        if identity in identities and identities[identity] != row:
            raise RuntimeError("archive registry contains conflicting version mappings")
        identities[identity] = row
    return receipt


def encoded_receipt(receipt):
    return json.dumps(receipt, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8")


def read_body(raw_fs, body):
    try:
        result = body.read()
        if inspect.isawaitable(result):
            from fsspec.asyn import sync

            async def await_result():
                return await result

            result = sync(raw_fs.loop, await_result)
        return result
    finally:
        body.close()


class ArchiveRegistry:
    def __init__(self, raw_fs, bucket, remote_prefix):
        self.raw_fs = raw_fs
        self.bucket = bucket
        self.remote_prefix = remote_prefix.rstrip("/")
        self._lookup_receipts = {}
        self._lookup_locks = {}
        self._lookup_guard = threading.Lock()

    def _lookup_receipt(self, source):
        # Cache only this read invocation's lookups, including absent parents.
        # A per-prefix lock avoids downloading/parsing a large receipt once
        # per file while unrelated task lookups retain request concurrency.
        with self._lookup_guard:
            lock = self._lookup_locks.setdefault(source, threading.Lock())
        with lock:
            with self._lookup_guard:
                if source in self._lookup_receipts:
                    return self._lookup_receipts[source]
            receipt = self.read(source)
            with self._lookup_guard:
                self._lookup_receipts[source] = receipt
            return receipt

    def read(self, source):
        key = registry_key(self.remote_prefix, source)
        try:
            response = self.raw_fs.call_s3("get_object", Bucket=self.bucket, Key=key)
        except Exception as error:
            if missing_object(error):
                return None
            raise
        try:
            receipt = json.loads(read_body(self.raw_fs, response["Body"]))
        except (ValueError, TypeError, KeyError) as error:
            raise RuntimeError(f"invalid archive registry at {key!r}") from error
        validate_receipt(receipt, self.bucket, self.remote_prefix)
        if receipt["source"] != source:
            raise RuntimeError("archive registry source does not match its canonical key")
        return receipt

    def publish(self, receipt):
        validate_receipt(receipt, self.bucket, self.remote_prefix)
        source = receipt["source"]
        key = registry_key(self.remote_prefix, source)

        def result(status):
            with self._lookup_guard:
                self._lookup_receipts.pop(source, None)
            return {"status": status, "registry_key": key}

        def matches_existing():
            existing = self.read(source)
            if existing is None:
                return False
            if encoded_receipt(existing) != encoded_receipt(receipt):
                raise RuntimeError(f"conflicting archive registry at {key!r}")
            return True

        if matches_existing():
            return result("unchanged")
        try:
            self.raw_fs.call_s3(
                "put_object", Bucket=self.bucket, Key=key,
                Body=encoded_receipt(receipt), ContentType="application/json",
                IfNoneMatch="*",
            )
        except Exception:
            # A response may be lost after S3 accepted the conditional write.
            # Exact rereading also handles a concurrent identical publisher.
            if matches_existing():
                return result("unchanged")
            raise
        if not matches_existing():
            raise RuntimeError("archive registry publication was not readable")
        return result("published")

    def lookup(self, key, version_id):
        prefix = self.remote_prefix + "/" if self.remote_prefix else ""
        if prefix and not key.startswith(prefix):
            raise RuntimeError("historical archive lookup escaped its storage prefix")
        object_name = relative_object(key[len(prefix):], "lookup object")
        parts = object_name.split("/")
        for length in range(len(parts) - 1, 0, -1):
            source = "/".join(parts[:length])
            try:
                relative_path(source, "source")
            except RuntimeError:
                continue
            receipt = self._lookup_receipt(source)
            if receipt is None:
                continue
            for row in receipt["versions"]:
                if (row["source_object"], row["source_version_id"]) != (
                    object_name, version_id,
                ):
                    continue
                if row["delete_marker"]:
                    raise RuntimeError("historical data lookup selected a delete marker")
                return {
                    **row,
                    "destination_key": full_key(self.remote_prefix, row["destination_object"]),
                }
        return None


def main(argv=None):
    from dvc.repo import Repo as DvcRepo

    argv = sys.argv[1:] if argv is None else argv
    repo_path, operation, raw_payload = argv
    payload = json.loads(raw_payload)
    with DvcRepo(repo_path) as repo:
        remote = repo.cloud.get_remote()
        if not remote.fs.version_aware:
            raise RuntimeError("archive registry requires a version-aware remote")
        raw_fs = remote.fs.fs
        bucket, remote_prefix, _ = raw_fs.split_path(remote.path)
        if not bucket or not raw_fs.is_bucket_versioned(bucket):
            raise RuntimeError("archive registry requires an enabled versioned bucket")
        registry = ArchiveRegistry(raw_fs, bucket, remote_prefix)
        if operation == "publish":
            result = registry.publish(payload)
        elif operation == "read":
            mapping = registry.lookup(payload["object"], payload["version_id"])
            result = {"status": "mapped" if mapping else "missing", "mapping": mapping}
        else:
            raise RuntimeError(f"unknown archive registry operation: {operation!r}")
        print(json.dumps(result, sort_keys=True))
        return result


if __name__ == "__main__":
    main()
