"""Exact-version S3 reads for the pinned private storage engine.

Metadata collection and cache updates stay on the owning thread. Network reads
use a bounded pool, with no persistent verification cache across invocations.
"""

from __future__ import annotations

from collections import defaultdict
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait
from contextlib import ExitStack
from dataclasses import dataclass, replace
import errno
import json
from pathlib import Path, PurePosixPath
import re
import sys
import tempfile

VERIFY_WORKERS = 16
LIST_MIN_ENTRIES = 8
LIST_PAGE_LIMIT = 2
MAX_ARCHIVE_HOPS = 32


class VersionMismatch(RuntimeError):
    pass


class MissingObjectVersion(RuntimeError):
    pass


@dataclass(frozen=True)
class Entry:
    object_name: str
    key: str
    version_id: str
    size: int | None
    etag: str | None
    md5: str | None
    hash_name: str = "md5"
    cache_path: str | None = None
    cache: object = None


def bounded_map(function, items):
    """Bound both running requests and queued futures on older Python too."""
    iterator = iter(items)
    with ThreadPoolExecutor(max_workers=VERIFY_WORKERS) as pool:
        pending = set()
        try:
            for _ in range(VERIFY_WORKERS):
                item = next(iterator, None)
                if item is None:
                    break
                pending.add(pool.submit(function, item))
            while pending:
                completed, pending = wait(pending, return_when=FIRST_COMPLETED)
                for future in completed:
                    yield future.result()
                    item = next(iterator, None)
                    if item is not None:
                        pending.add(pool.submit(function, item))
        finally:
            for future in pending:
                future.cancel()


def normalized_etag(value):
    return value.strip('"') if isinstance(value, str) else None


def validate_info(entry, info):
    version = info.get("VersionId") or info.get("version_id")
    size = info.get("ContentLength", info.get("Size", info.get("size")))
    etag = normalized_etag(info.get("ETag") or info.get("etag"))
    mismatches = []
    if version != entry.version_id or info.get("DeleteMarker"):
        mismatches.append("version ID")
    if entry.size is not None and size != entry.size:
        mismatches.append("size")
    if entry.etag and etag != normalized_etag(entry.etag):
        mismatches.append("etag")
    if mismatches:
        raise VersionMismatch(
            f"version-aware object {entry.object_name!r} has mismatched "
            + ", ".join(mismatches)
        )


def listing_unavailable(error):
    code = getattr(error, "response", {}).get("Error", {}).get("Code")
    return (
        isinstance(error, (PermissionError, NotImplementedError))
        or getattr(error, "errno", None) in (errno.EACCES, errno.EPERM, errno.ENOSYS)
        or code in ("AccessDenied", "NotImplemented", "MethodNotAllowed", "501", "405")
    )


def missing_object(error):
    code = (getattr(error, "response", None) or {}).get("Error", {}).get("Code")
    if code is not None:
        return code in ("NoSuchKey", "NoSuchVersion", "NotFound", "404")
    return isinstance(error, FileNotFoundError)


def archive_destination(entry, registry, seen):
    """Resolve only a proven missing exact version; retain its content hash."""
    identity = (entry.key, entry.version_id)
    if identity in seen or len(seen) >= MAX_ARCHIVE_HOPS:
        raise RuntimeError("historical archive mapping is cyclic or exceeds its hop limit")
    seen.add(identity)
    mapping = registry.lookup(entry.key, entry.version_id)
    if mapping is None:
        return None
    if entry.size is not None and mapping["size"] != entry.size:
        raise VersionMismatch("historical archive mapping has mismatched size")
    if entry.etag and mapping.get("source_etag") and (
        normalized_etag(entry.etag) != normalized_etag(mapping["source_etag"])
    ):
        raise VersionMismatch("historical archive mapping has mismatched source etag")
    return replace(
        entry, key=mapping["destination_key"],
        version_id=mapping["destination_version_id"],
        etag=normalized_etag(mapping["destination_etag"]),
    )


def pending_archive_entries(entries, receipts, bucket, remote_prefix):
    """Read moved, unpublished outputs from their original pinned objects."""
    from dvc_archive_registry import full_key, relative_object, relative_path

    if not isinstance(receipts, list):
        raise RuntimeError("pending archive context must be a list of receipts")
    aliases = {}
    remote_prefix = remote_prefix.rstrip("/")
    for receipt in receipts:
        if not isinstance(receipt, dict):
            raise RuntimeError("invalid pending archive receipt")
        if receipt.get("status") == "copied":
            continue
        if receipt.get("status") != "planned" or receipt.get("schema_version") != 1:
            raise RuntimeError("invalid pending archive receipt state")
        if (receipt.get("bucket"), receipt.get("remote_prefix"), receipt.get("remote")) != (
            bucket, remote_prefix, "workspace-mgr",
        ):
            raise RuntimeError("pending archive receipt selects another storage location")
        source = relative_path(receipt.get("source"), "source")
        destination = relative_path(receipt.get("destination"), "destination")
        if (source == destination or source.startswith(destination + "/")
                or destination.startswith(source + "/")
                or source.rsplit("/", 1)[-1] != destination.rsplit("/", 1)[-1]):
            raise RuntimeError("invalid pending archive task prefixes")
        versions = receipt.get("versions")
        if not isinstance(versions, list):
            raise RuntimeError("pending archive receipt has no version snapshot")
        for row in versions:
            if not isinstance(row, dict) or not isinstance(row.get("delete_marker"), bool):
                raise RuntimeError("invalid pending archive version")
            old = relative_object(row.get("source_object"), "source object")
            new = relative_object(row.get("destination_object"), "destination object")
            if not old.startswith(source + "/") or new != destination + old[len(source):]:
                raise RuntimeError("pending archive version escapes its task prefixes")
            version = row.get("source_version_id")
            if not isinstance(version, str) or not version:
                raise RuntimeError("pending archive version has no exact source version")
            if row["delete_marker"]:
                continue
            size, etag = row.get("size"), row.get("source_etag")
            if not isinstance(size, int) or isinstance(size, bool) or size < 0:
                raise RuntimeError("pending archive version has no valid size")
            if not isinstance(etag, str) or not etag:
                raise RuntimeError("pending archive version has no source etag")
            identity = (full_key(remote_prefix, new), version)
            alias = (full_key(remote_prefix, old), size, normalized_etag(etag))
            if identity in aliases and aliases[identity] != alias:
                raise RuntimeError("conflicting pending archive version aliases")
            aliases[identity] = alias
    result = []
    for entry in entries:
        alias = aliases.get((entry.key, entry.version_id))
        if alias is None:
            result.append(entry)
            continue
        key, size, etag = alias
        if entry.size is not None and entry.size != size:
            raise VersionMismatch("pending archive alias has mismatched size")
        if entry.etag and normalized_etag(entry.etag) != etag:
            raise VersionMismatch("pending archive alias has mismatched etag")
        result.append(replace(entry, key=key, etag=etag))
    return result


def verify_entries(raw_fs, bucket, entries, registry=None):
    """Read at most two pages per dense parent prefix, then exact HEADs.

    Listings establish metadata presence, not object-read permission. Downloads
    always perform an exact GET, even if another object has identical bytes.
    A truncated or malformed listing never proves an unmatched version absent.
    """
    groups = defaultdict(list)
    for entry in entries:
        prefix = entry.key.rpartition("/")[0]
        groups[prefix + "/" if prefix else ""].append(entry)

    def list_group(group):
        prefix, members = group
        # Never scan the bucket root, or list for a handful of sparse targets.
        if not prefix or len(members) < LIST_MIN_ENTRIES:
            return members, []
        wanted = defaultdict(list)
        for entry in members:
            wanted[(entry.key, entry.version_id)].append(entry)
        request = {"Bucket": bucket, "Prefix": prefix, "MaxKeys": 1000}
        seen_markers = set()
        failures = []
        for _ in range(LIST_PAGE_LIMIT):
            try:
                response = raw_fs.call_s3("list_object_versions", **request)
            except Exception as error:
                if listing_unavailable(error):
                    break
                raise
            for info in response.get("Versions", []):
                matches = wanted.pop((info.get("Key"), info.get("VersionId")), [])
                for entry in matches:
                    try:
                        validate_info(entry, info)
                    except VersionMismatch as error:
                        failures.append(str(error))
            if not wanted or not response.get("IsTruncated"):
                break
            marker = (response.get("NextKeyMarker"), response.get("NextVersionIdMarker"))
            if not marker[0] or marker in seen_markers:
                break
            seen_markers.add(marker)
            request["KeyMarker"] = marker[0]
            if marker[1]:
                request["VersionIdMarker"] = marker[1]
            else:
                request.pop("VersionIdMarker", None)
        return [entry for matches in wanted.values() for entry in matches], failures

    remaining = []
    failures = []
    for unmatched, mismatches in bounded_map(list_group, sorted(groups.items())):
        remaining.extend(unmatched)
        failures.extend(mismatches)

    def head(entry):
        current = entry
        seen = set()
        while True:
            try:
                info = raw_fs.call_s3(
                    "head_object", Bucket=bucket, Key=current.key,
                    VersionId=current.version_id,
                )
            except Exception as error:
                if not missing_object(error):
                    raise
                if registry is None:
                    return f"missing version: {entry.object_name}"
                try:
                    current = archive_destination(current, registry, seen)
                except VersionMismatch as mismatch:
                    return str(mismatch)
                if current is None:
                    return f"missing version: {entry.object_name}"
                continue
            try:
                validate_info(current, info)
            except VersionMismatch as error:
                return str(error)
            return None

    failures.extend(error for error in bounded_map(head, remaining) if error)
    if failures:
        raise RuntimeError(
            "version-aware object versions are missing or mismatched: "
            + "; ".join(sorted(set(failures)))
        )


def collect_entries(dvc_repo, remote, pointers):
    """Parse complete version manifests without loading remote directory trees."""
    entries = []
    trees = []
    raw_fs = remote.fs.fs
    bucket, _, _ = raw_fs.split_path(remote.path)

    def add(parts, version, remote_name, size, etag, md5, out):
        object_name = PurePosixPath(*parts).as_posix()
        if not version or version == "null":
            raise RuntimeError(f"managed-storage object has no exact version ID: {object_name}")
        if remote_name and remote_name != remote.name:
            raise RuntimeError(f"managed-storage metadata selects unexpected remote {remote_name!r}")
        if out.hash_name not in ("md5", "md5-dos2unix") or not re.fullmatch(r"[0-9a-f]{32}", md5 or ""):
            raise RuntimeError(f"managed-storage object has no supported content hash: {object_name}")
        remote_path = remote.fs.join(remote.path, *parts)
        object_bucket, key, path_version = raw_fs.split_path(remote_path)
        if object_bucket != bucket or not key or path_version:
            raise RuntimeError(f"managed-storage object escaped its configured location: {object_name}")
        cache = out.cache
        if cache.fs.protocol != "local":
            raise RuntimeError("managed-storage downloads require a local cache")
        entries.append(Entry(
            object_name, key, version, size, normalized_etag(etag), md5,
            out.hash_name, cache.oid_to_path(md5), cache,
        ))

    for pointer in pointers:
        stages = list(dvc_repo.stage.collect(str(Path(dvc_repo.root_dir) / pointer)))
        if not stages:
            raise RuntimeError(f"managed-storage metadata did not define an output: {pointer}")
        for stage in stages:
            for out in stage.outs:
                if not out.is_in_repo or not out.can_push or not out.use_cache:
                    raise RuntimeError(f"managed-storage output must be cached, pushable and inside the repository: {pointer}")
                _, parts = out.index_key
                if out.hash_info and out.hash_info.isdir:
                    if out.files is None:
                        raise RuntimeError(f"managed-storage directory metadata is incomplete: {pointer}; restore its published file/version manifest")
                    for item in out.files:
                        relpath = item.get("relpath")
                        if not isinstance(relpath, str):
                            raise RuntimeError(f"managed-storage directory entry has no path: {pointer}")
                        rel = PurePosixPath(relpath)
                        if rel.is_absolute() or ".." in rel.parts or not rel.parts:
                            raise RuntimeError(f"invalid path in managed-storage directory metadata: {relpath!r}")
                        add((*parts, *rel.parts), item.get("version_id"),
                            item.get("remote") or out.remote, item.get("size"),
                            item.get("etag") or item.get("md5"), item.get("md5"), out)
                    # Even an empty manifest must agree with its directory hash.
                    from dvc_data.hashfile.tree import Tree
                    tree = Tree.from_list(out.files, hash_name=out.hash_name)
                    tree.digest()
                    if tree.hash_info != out.hash_info:
                        raise RuntimeError(f"managed-storage directory manifest hash mismatch: {pointer}")
                    trees.append((out.cache, tree))
                else:
                    meta = out.meta
                    add(parts, meta.version_id if meta else None,
                        (meta.remote if meta else None) or out.remote,
                        meta.size if meta else None,
                        (getattr(meta, "etag", None) or getattr(meta, "md5", None)) if meta else None,
                        out.hash_info.value if out.hash_info else None, out)
    return bucket, entries, trees


def content_matches(entry, path):
    from dvc_data.hashfile.hash import hash_file
    from dvc_objects.fs import localfs
    try:
        meta, digest = hash_file(str(path), localfs, entry.hash_name)
    except FileNotFoundError:
        return False
    return digest.value == entry.md5 and (entry.size is None or meta.size == entry.size)


def fetch_entries(raw_fs, bucket, entries, trees, registry=None):
    from fsspec.asyn import sync
    from dvc_objects.fs import localfs
    from dvc_data.hashfile.db import add_update_tree
    from s3fs.core import S3_RETRYABLE_ERRORS

    cached, missing = [], []
    for entry in entries:
        (cached if content_matches(entry, entry.cache_path) else missing).append(entry)
    # Cached bytes alone do not prove that their published remote version exists.
    verify_entries(raw_fs, bucket, cached, registry=registry)

    with ExitStack() as cleanup:
        scratch = {}
        for entry in missing:
            cache_root = entry.cache.path
            if cache_root not in scratch:
                Path(cache_root).mkdir(parents=True, exist_ok=True)
                scratch[cache_root] = cleanup.enter_context(tempfile.TemporaryDirectory(
                    prefix="workspace-mgr-fetch-", dir=cache_root,
                ))

        async def read(entry, destination):
            kwargs = {"Bucket": bucket, "Key": entry.key, "VersionId": entry.version_id}
            if entry.etag:
                kwargs["IfMatch"] = f'"{entry.etag}"'
            # Retry interrupted bodies from the start; never mix different GETs
            # or use ranged ContentLength as the full object length.
            for attempt in range(3):
                body = None
                try:
                    try:
                        response = await raw_fs._call_s3("get_object", **kwargs)
                    except Exception as error:
                        if missing_object(error):
                            raise MissingObjectVersion(entry.object_name) from error
                        raise
                    body = response["Body"]
                    validate_info(entry, response)
                    count = 0
                    with open(destination, "wb") as output:
                        while chunk := await body.read(1024 * 1024):
                            count += len(chunk)
                            output.write(chunk)
                    if count != response.get("ContentLength"):
                        raise RuntimeError(f"incomplete download: {entry.object_name}")
                    return
                except S3_RETRYABLE_ERRORS:
                    if attempt == 2:
                        raise
                finally:
                    if body is not None:
                        body.close()

        def download(item):
            index, entry = item
            destination = str(Path(scratch[entry.cache.path]) / str(index))
            current = entry
            seen = set()
            while True:
                try:
                    sync(raw_fs.loop, read, current, destination)
                except MissingObjectVersion as error:
                    if registry is None:
                        raise RuntimeError(
                            f"version-aware object version is missing: {entry.object_name}"
                        ) from error
                    current = archive_destination(current, registry, seen)
                    if current is None:
                        raise RuntimeError(
                            f"version-aware object version is missing: {entry.object_name}"
                        ) from error
                    continue
                break
            if not content_matches(entry, destination):
                raise RuntimeError(f"downloaded content hash mismatch: {entry.object_name}")
            return entry, destination

        for entry, destination in bounded_map(download, enumerate(missing)):
            # The engine's cache/index objects are not shared with workers.
            # Replace a corrupt cache object only after new bytes are verified.
            if Path(entry.cache_path).exists() and not content_matches(entry, entry.cache_path):
                entry.cache.fs.remove(entry.cache_path)
            entry.cache.add(destination, localfs, entry.md5, hardlink=True)
            Path(destination).unlink()
        for cache, tree in trees:
            add_update_tree(cache, tree)


def main(argv=None):
    from dvc.repo import Repo as DvcRepo
    argv = sys.argv[1:] if argv is None else argv
    repo_path = Path(argv[0])
    pointers = json.loads(argv[1])
    operation = argv[2] if len(argv) > 2 else "--verify"
    pending_receipts = json.loads(argv[3]) if len(argv) > 3 else []
    if operation not in ("--verify", "--fetch", "--check-versioning-only"):
        raise RuntimeError(f"unknown version adapter operation: {operation}")
    with DvcRepo(str(repo_path)) as repo:
        remote = repo.cloud.get_remote()
        if not remote.fs.version_aware:
            raise RuntimeError(f"configured remote {remote.name!r} is not version-aware")
        raw_fs = remote.fs.fs
        bucket, remote_prefix, _ = raw_fs.split_path(remote.path)
        if not bucket:
            raise RuntimeError("configured S3 remote does not name a bucket")
        if not raw_fs.is_bucket_versioned(bucket):
            raise RuntimeError(f"S3 bucket {bucket!r} does not have object versioning enabled")
        if operation == "--check-versioning-only":
            result = {"mode": "bucket-versioning", "remote": remote.name,
                      "bucket": bucket, "status": "enabled"}
        else:
            try:
                from dvc_archive_registry import ArchiveRegistry
            except ModuleNotFoundError as error:
                if error.name != "dvc_archive_registry" or "__file__" not in globals():
                    raise
                # Standalone/importlib tests load this asset without putting
                # its directory on sys.path. Embedded Rust adapters preload it.
                import importlib.util

                registry_path = Path(__file__).with_name("dvc_archive_registry.py")
                spec = importlib.util.spec_from_file_location(
                    "dvc_archive_registry", registry_path,
                )
                if spec is None or spec.loader is None:
                    raise RuntimeError("historical archive registry adapter is unavailable")
                module = importlib.util.module_from_spec(spec)
                sys.modules[spec.name] = module
                spec.loader.exec_module(module)
                ArchiveRegistry = module.ArchiveRegistry
            bucket, entries, trees = collect_entries(repo, remote, pointers)
            entries = pending_archive_entries(
                entries, pending_receipts, bucket, remote_prefix,
            )
            registry = ArchiveRegistry(raw_fs, bucket, remote_prefix)
            if operation == "--fetch":
                fetch_entries(raw_fs, bucket, entries, trees, registry=registry)
            else:
                verify_entries(raw_fs, bucket, entries, registry=registry)
            result = {"mode": "version-aware", "remote": remote.name,
                      "checked_objects": sorted({entry.object_name for entry in entries})}
        print(json.dumps(result, sort_keys=True))
        return result


if __name__ == "__main__":
    main()
