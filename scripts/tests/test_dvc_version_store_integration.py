"""Real DVC/s3fs regression tests with an in-process S3 transport.

Run with the pinned workspace-mgr storage Python. These tests never contact AWS;
only the raw S3 request boundary is replaced. DVC metadata parsing, cache writes,
directory manifests, and checkout use the installed storage engine.
"""
from __future__ import annotations

import argparse
import asyncio
import builtins
from collections import Counter
from contextlib import ExitStack, redirect_stdout
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock


HAS_STORAGE_RUNTIME = all(
    importlib.util.find_spec(name) is not None for name in ("dvc", "s3fs", "yaml")
)
SCRIPT = Path(__file__).resolve().parents[2] / "assets" / "dvc_version_verify.py"


class ResponseBody:
    def __init__(self, data):
        self.data = data
        self.offset = 0
        self.closed = False

    async def read(self, size=-1):
        if size < 0:
            size = len(self.data)
        chunk = self.data[self.offset : self.offset + size]
        self.offset += len(chunk)
        return chunk

    def close(self):
        self.closed = True


class S3Transport:
    """Versioned objects and bounded real-async request concurrency."""

    def __init__(self, latency=0.003):
        self.latency = latency
        self.versions = []
        self.archive_registries = {}
        self.overrides = {}
        self.deny_listing = False
        self.allow_legacy_probe = False
        self.lock = threading.Lock()
        self.reset_counts()

    def reset_counts(self):
        self.calls = []
        self.active = Counter()
        self.max_active = Counter()

    def add(self, key, version_id, body, *, delete_marker=False, latest=True):
        if latest:
            for entry in self.versions:
                if entry["Key"] == key:
                    entry["IsLatest"] = False
        entry = {
            "Key": key,
            "VersionId": version_id,
            "IsLatest": latest,
            "ETag": '"' + hashlib.md5(body).hexdigest() + '"',
            "Size": len(body),
            "body": body,
            "delete_marker": delete_marker,
        }
        self.versions.append(entry)
        return entry

    async def request(self, _raw_fs, method, *args, **kwargs):
        params = {}
        for arg in args:
            params.update(arg)
        params.update(kwargs)
        with self.lock:
            self.calls.append((method, dict(params)))
            self.active[method] += 1
            self.max_active[method] = max(self.max_active[method], self.active[method])
        try:
            await asyncio.sleep(self.latency)
            if method == "get_bucket_versioning":
                return {"Status": "Enabled"}
            if method == "head_bucket":
                return {}
            if method == "list_object_versions":
                return self._list_versions(params)
            if method == "list_objects_v2":
                if not self.allow_legacy_probe:
                    raise AssertionError("hydration must not discover current S3 objects")
                current = [entry for entry in self.versions if entry["IsLatest"] and not entry["delete_marker"] and entry["Key"].startswith(params.get("Prefix", ""))]
                return {"KeyCount": len(current), "Contents": [{"Key": entry["Key"], "Size": entry["Size"]} for entry in current[:params.get("MaxKeys", 1000)]]}
            if method not in ("head_object", "get_object"):
                raise AssertionError(f"unexpected S3 operation: {method}")
            if method == "get_object" and params["Key"].startswith("storage/.workspace-mgr/archive/"):
                if params["Key"] not in self.archive_registries:
                    raise FileNotFoundError(params["Key"])
                return {"Body": ResponseBody(self.archive_registries[params["Key"]])}
            # A successful current-object fallback would conceal a serious bug.
            if not params.get("VersionId"):
                if self.allow_legacy_probe and method == "head_object" and params["Key"] == "storage":
                    raise FileNotFoundError(params["Key"])
                raise AssertionError(f"{method} did not specify an exact version")
            entry = next(
                (
                    entry
                    for entry in self.versions
                    if entry["Key"] == params["Key"]
                    and entry["VersionId"] == params["VersionId"]
                ),
                None,
            )
            if entry is None or entry["delete_marker"]:
                raise FileNotFoundError(f"{params['Key']}@{params['VersionId']}")
            if params.get("IfMatch", entry["ETag"]).strip('"') != entry["ETag"].strip('"'):
                raise RuntimeError("IfMatch precondition failed")
            result = {
                "ContentLength": entry["Size"],
                "ETag": entry["ETag"],
                "VersionId": entry["VersionId"],
            }
            if method == "get_object":
                result["Body"] = ResponseBody(entry["body"])
                result.update(self.overrides.get((entry["Key"], entry["VersionId"]), {}))
            return result
        finally:
            with self.lock:
                self.active[method] -= 1

    def _list_versions(self, params):
        if self.deny_listing:
            raise PermissionError("fixture denies ListBucketVersions")
        entries = sorted(
            (entry for entry in self.versions if entry["Key"].startswith(params.get("Prefix", ""))),
            key=lambda entry: (entry["Key"], entry["VersionId"]),
        )
        start = 0
        if "KeyMarker" in params:
            for index, entry in enumerate(entries):
                if (entry["Key"], entry["VersionId"]) == (
                    params["KeyMarker"],
                    params.get("VersionIdMarker"),
                ):
                    start = index + 1
                    break
            else:
                raise AssertionError("invalid listing continuation marker")
        page = entries[start : start + params.get("MaxKeys", 1000)]
        result = {"Versions": [], "DeleteMarkers": [], "IsTruncated": start + len(page) < len(entries)}
        for entry in page:
            result["DeleteMarkers" if entry["delete_marker"] else "Versions"].append(
                {key: value for key, value in entry.items() if key not in ("body", "delete_marker")}
            )
        if result["IsTruncated"]:
            result["NextKeyMarker"] = page[-1]["Key"]
            result["NextVersionIdMarker"] = page[-1]["VersionId"]
        return result

    def count(self, method):
        return sum(name == method for name, _ in self.calls)

    def payload_gets(self):
        return [params for method, params in self.calls
                if method == "get_object" and params.get("VersionId")]


@unittest.skipUnless(HAS_STORAGE_RUNTIME, "requires the pinned DVC + s3fs storage runtime")
class DvcVersionStoreIntegrationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        global s3fs, yaml, DvcRepo, sync
        import s3fs
        import yaml
        from dvc.repo import Repo as DvcRepo
        from fsspec.asyn import sync

        spec = importlib.util.spec_from_file_location("dvc_version_store_integration_asset", SCRIPT)
        cls.store = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = cls.store
        spec.loader.exec_module(cls.store)

    def setUp(self):
        self.stack = ExitStack()
        self.addCleanup(self.stack.close)
        self.root = Path(self.stack.enter_context(tempfile.TemporaryDirectory(prefix="dvc-version-store-")))
        self.stack.enter_context(
            mock.patch.dict(
                os.environ,
                {
                    "AWS_CONFIG_FILE": str(self.root / "empty-aws-config"),
                    "AWS_SHARED_CREDENTIALS_FILE": str(self.root / "empty-aws-credentials"),
                    "AWS_EC2_METADATA_DISABLED": "true",
                    "DVC_GLOBAL_CONFIG_DIR": str(self.root / "global-config"),
                    "DVC_SYSTEM_CONFIG_DIR": str(self.root / "system-config"),
                    "DVC_NO_ANALYTICS": "1",
                },
            )
        )
        self.transport = S3Transport()
        self.pending_receipts = []
        transport = self.transport

        async def session(_raw_fs, *args, **kwargs):
            return transport

        async def get_s3(_raw_fs, bucket):
            return transport

        async def call_async(raw_fs, method, *args, **kwargs):
            return await transport.request(raw_fs, method, *args, **kwargs)

        def call_sync(raw_fs, method, *args, **kwargs):
            return sync(raw_fs.loop, call_async, raw_fs, method, *args, **kwargs)

        s3fs.S3FileSystem.clear_instance_cache()
        self.addCleanup(s3fs.S3FileSystem.clear_instance_cache)
        for name, replacement in (
            ("set_session", session),
            ("connect", lambda *args, **kwargs: transport),
            ("get_s3", get_s3),
            ("_call_s3", call_async),
            ("call_s3", call_sync),
        ):
            self.stack.enter_context(mock.patch.object(s3fs.S3FileSystem, name, replacement))

    def make_repo(self, count=1, *, directory=True, warm=False, duplicate_contents=False):
        self.repo_path = self.root / "repo"
        self.repo_path.mkdir()
        self.payload = self.repo_path / ("data" if directory else "data.txt")
        self.expected = {}
        with redirect_stdout(io.StringIO()), DvcRepo.init(str(self.repo_path), no_scm=True) as repo:
            if directory:
                self.payload.mkdir()
                for index in range(count):
                    relative = f"{index:06}.txt"
                    body = b"same bytes\n" if duplicate_contents else f"payload {index:06}\n".encode()
                    (self.payload / relative).write_bytes(body)
                    self.expected[relative] = body
            else:
                body = b"single pinned payload\n"
                self.payload.write_bytes(body)
                self.expected[""] = body
            repo.add(str(self.payload))
        self.pointer = self.repo_path / f"{self.payload.name}.dvc"
        data = yaml.safe_load(self.pointer.read_text())
        out = data["outs"][0]
        self.object_entries = []
        self.cache_files = []
        files = []
        for index, (relative, body) in enumerate(self.expected.items()):
            md5 = hashlib.md5(body).hexdigest()
            version_id = f"published-{index:06}"
            key = "storage/" + self.payload.name + ("/" + relative if relative else "")
            self.object_entries.append(self.transport.add(key, version_id, body))
            cloud = {"workspace-mgr": {"version_id": version_id, "etag": md5}}
            if directory:
                files.append({"relpath": relative, "size": len(body), "md5": md5, "cloud": cloud})
            else:
                out["cloud"] = cloud
            self.cache_files.append(self.repo_path / ".dvc/cache/files/md5" / md5[:2] / md5[2:])
        if directory:
            out["files"] = files
        self.pointer.write_text(yaml.safe_dump(data))
        if not warm:
            shutil.rmtree(self.repo_path / ".dvc/cache")
        if directory:
            shutil.rmtree(self.payload)
        else:
            self.payload.unlink()
        (self.repo_path / ".dvc/config").write_text(
            '[core]\n    no_scm = true\n    remote = workspace-mgr\n'
            '[\'remote "workspace-mgr"\']\n    url = s3://test-bucket/storage\n'
            '    version_aware = true\n    allow_anonymous_login = true\n'
        )
        self.transport.reset_counts()

    def run_store(self, fetch=True):
        output = io.StringIO()
        argv = [str(self.repo_path), json.dumps([str(self.pointer.relative_to(self.repo_path))]),
                "--fetch" if fetch else "--verify", json.dumps(self.pending_receipts)]
        with redirect_stdout(output):
            result = self.store.main(argv)
        self.assertIsInstance(result, dict)
        return output.getvalue()

    def checkout_and_assert(self):
        with DvcRepo(str(self.repo_path)) as repo:
            repo.checkout(targets=[str(self.pointer)])
        for relative, expected in self.expected.items():
            actual = (self.payload / relative) if relative else self.payload
            self.assertEqual(actual.read_bytes(), expected)

    def archive_recorded_versions(self):
        """Keep the old Git pointer, but expose its exact versions only at new keys."""
        source = self.payload.name
        destination = f"2026/07/{source}"
        rows = []
        for original in tuple(self.object_entries):
            new_key = "storage/" + destination + original["Key"][len("storage/" + source):]
            copied = self.transport.add(new_key, "archived-" + original["VersionId"], original["body"])
            # Copying an encrypted/multipart object may change its native ETag.
            copied["ETag"] = '"copied-etag-' + original["VersionId"] + '"'
            rows.append({
                "source_object": original["Key"][len("storage/"):],
                "destination_object": new_key[len("storage/"):],
                "source_version_id": original["VersionId"],
                "destination_version_id": copied["VersionId"],
                "source_etag": original["ETag"].strip('"'),
                "destination_etag": copied["ETag"].strip('"'),
                "size": original["Size"], "delete_marker": False,
            })
            self.transport.versions.remove(original)
        value = {
            "schema_version": 1, "status": "copied", "source": source,
            "destination": destination, "remote": "workspace-mgr",
            "bucket": "test-bucket", "remote_prefix": "storage", "versions": rows,
        }
        digest = hashlib.sha256(source.encode()).hexdigest()
        self.transport.archive_registries[f"storage/.workspace-mgr/archive/{digest}.json"] = json.dumps(value).encode()
        return rows

    def test_old_git_cold_directory_hydrates_archived_exact_versions(self):
        self.make_repo(2)
        original_pointer = self.pointer.read_bytes()
        rows = self.archive_recorded_versions()
        self.run_store()
        self.checkout_and_assert()
        self.assertEqual(self.pointer.read_bytes(), original_pointer)
        retrieved = {(params["Key"], params["VersionId"]) for params in self.transport.payload_gets()}
        for row in rows:
            self.assertIn(("storage/" + row["destination_object"], row["destination_version_id"]), retrieved)

    def test_pending_local_archive_hydrates_original_versions_before_publication(self):
        self.make_repo(2)
        source = self.repo_path / "task"
        source.mkdir()
        shutil.move(self.pointer, source / self.pointer.name)
        rows = []
        for original in self.object_entries:
            original["Key"] = "storage/task/" + original["Key"][len("storage/"):]
            object_name = original["Key"][len("storage/"):]
            rows.append({
                "source_object": object_name,
                "destination_object": "2026/07/" + object_name,
                "source_version_id": original["VersionId"],
                "source_etag": original["ETag"].strip('"'),
                "size": original["Size"], "delete_marker": False,
            })
        destination = self.repo_path / "2026/07/task"
        destination.parent.mkdir(parents=True)
        shutil.move(source, destination)
        self.pointer = destination / self.pointer.name
        self.payload = destination / "data"
        self.pending_receipts = [{
            "schema_version": 1, "status": "planned", "source": "task",
            "destination": "2026/07/task", "remote": "workspace-mgr",
            "bucket": "test-bucket", "remote_prefix": "storage", "versions": rows,
        }]
        before_pointer = self.pointer.read_bytes()
        self.run_store()
        self.checkout_and_assert()
        self.assertEqual(self.pointer.read_bytes(), before_pointer)
        self.assertEqual(self.transport.archive_registries, {})
        self.assertEqual(
            {(params["Key"], params["VersionId"]) for params in self.transport.payload_gets()},
            {("storage/" + row["source_object"], row["source_version_id"]) for row in rows},
        )

    def test_old_git_warm_directory_verifies_archive_despite_existing_cache(self):
        self.make_repo(2, warm=True)
        self.archive_recorded_versions()
        self.run_store()
        self.assertEqual(self.transport.payload_gets(), [])
        self.assertEqual(self.transport.count("head_object"), 4)
        self.checkout_and_assert()

    def test_archived_destination_corrupt_bytes_never_enter_historical_cache(self):
        self.make_repo(1)
        rows = self.archive_recorded_versions()
        row = rows[0]
        self.transport.overrides[("storage/" + row["destination_object"], row["destination_version_id"])] = {
            "Body": ResponseBody(b"X" * row["size"]),
        }
        with self.assertRaisesRegex(RuntimeError, "downloaded content hash mismatch"):
            self.run_store()
        self.assertFalse(self.cache_files[0].exists())
        self.assertFalse(self.payload.exists())

    def test_archived_destination_wrong_size_never_enters_historical_cache(self):
        self.make_repo(1)
        rows = self.archive_recorded_versions()
        row = rows[0]
        self.transport.overrides[("storage/" + row["destination_object"], row["destination_version_id"])] = {
            "ContentLength": row["size"] + 1,
        }
        with self.assertRaisesRegex(RuntimeError, "mismatched size"):
            self.run_store()
        self.assertFalse(self.cache_files[0].exists())
        self.assertFalse(self.payload.exists())

    def test_cold_single_file_uses_one_exact_get_and_checks_out(self):
        self.make_repo(directory=False)
        self.run_store()
        self.assertEqual(self.transport.count("get_object"), 1)
        self.assertEqual(self.transport.count("head_object"), 0)
        self.assertEqual(self.transport.count("list_object_versions"), 0)
        self.checkout_and_assert()

    def test_cold_directory_gets_each_object_once_in_parallel_and_checks_out(self):
        self.make_repo(16)
        self.run_store()
        self.assertEqual(self.transport.count("get_object"), 16)
        self.assertEqual(self.transport.count("head_object"), 0)
        self.assertEqual(self.transport.count("list_object_versions"), 0)
        self.assertGreater(self.transport.max_active["get_object"], 1)
        self.assertLessEqual(self.transport.max_active["get_object"], self.store.VERIFY_WORKERS)
        self.assertTrue(all("Range" not in params for method, params in self.transport.calls if method == "get_object"))
        self.checkout_and_assert()

    def test_duplicate_contents_still_read_each_exact_remote_version(self):
        self.make_repo(2, duplicate_contents=True)
        self.run_store()
        self.assertEqual(self.transport.count("get_object"), 2)
        self.assertEqual(len(set(self.cache_files)), 1)
        self.checkout_and_assert()

    def test_corrupt_local_cache_is_replaced_only_with_valid_download(self):
        self.make_repo(directory=False, warm=True)
        self.cache_files[0].chmod(0o644)
        self.cache_files[0].write_bytes(b"broken cache")
        self.run_store()
        self.assertEqual(self.transport.count("get_object"), 1)
        self.assertEqual(self.transport.count("head_object"), 0)
        self.checkout_and_assert()

    def record_custom_cache_staging(self):
        cache_root = self.root / "separate-cache-volume" / "configured-cache"
        config = self.repo_path / ".dvc/config"
        config.write_text(config.read_text() + f"[cache]\n    dir = {cache_root}\n")
        staged = []

        def record_open(path, mode="r", *args, **kwargs):
            if mode == "wb":
                destination = Path(path)
                self.assertTrue(destination.is_relative_to(cache_root))
                self.assertTrue(destination.parent.is_dir())
                staged.append(destination)
            return builtins.open(path, mode, *args, **kwargs)

        self.stack.enter_context(mock.patch.object(self.store, "open", record_open, create=True))
        return cache_root, staged

    def assert_staging_cleaned(self, cache_root, staged):
        self.assertTrue(staged, "expected to observe streamed download destinations")
        self.assertEqual(len({path.parent for path in staged}), 1)
        self.assertTrue(all(not path.exists() and not path.parent.exists() for path in staged))
        self.assertEqual(list(cache_root.rglob("workspace-mgr-fetch-*")), [])

    def test_successful_downloads_stage_inside_configured_cache_and_clean_up(self):
        self.make_repo(16)
        cache_root, staged = self.record_custom_cache_staging()
        self.run_store()
        self.assertEqual(len(staged), 16)
        self.assert_staging_cleaned(cache_root, staged)
        self.checkout_and_assert()

    def test_failed_downloads_clean_up_staging_inside_configured_cache(self):
        self.make_repo(16)
        cache_root, staged = self.record_custom_cache_staging()
        entry = self.object_entries[0]
        self.transport.overrides[(entry["Key"], entry["VersionId"])] = {
            "Body": ResponseBody(b"X" * entry["Size"]),
        }
        with self.assertRaises(RuntimeError):
            self.run_store()
        self.assert_staging_cleaned(cache_root, staged)
        self.assertFalse(self.payload.exists())

    def test_local_staging_failure_does_not_trigger_archive_lookup(self):
        self.make_repo(1)

        def missing_destination(path, mode="r", *args, **kwargs):
            if mode == "wb":
                raise FileNotFoundError("local staging directory disappeared")
            return builtins.open(path, mode, *args, **kwargs)

        self.stack.enter_context(mock.patch.object(self.store, "open", missing_destination, create=True))
        with self.assertRaisesRegex(FileNotFoundError, "local staging directory"):
            self.run_store()
        self.assertEqual(self.transport.count("get_object"), 1)
        self.assertTrue(all(params.get("VersionId") for method, params in self.transport.calls
                            if method == "get_object"))
        self.assertFalse(self.cache_files[0].exists())

    def test_warm_directory_verifies_historical_versions_with_one_listing(self):
        self.make_repo(16, warm=True)
        for entry in tuple(self.object_entries):
            self.transport.add(entry["Key"], "new-latest-delete-marker", b"", delete_marker=True)
        self.run_store()
        self.assertEqual(self.transport.count("get_object"), 0)
        self.assertEqual(self.transport.count("head_object"), 0)
        self.assertEqual(self.transport.count("list_object_versions"), 1)
        self.checkout_and_assert()

    def test_history_listing_is_bounded_then_falls_back_to_parallel_exact_heads(self):
        self.make_repo(16, warm=True)
        # This prefix has a deep history before the requested objects in key order.
        for index in range(2100):
            self.transport.add("storage/data/!history", f"history-{index:06}", b"old", latest=False)
        self.run_store()
        self.assertEqual(self.transport.count("list_object_versions"), self.store.LIST_PAGE_LIMIT)
        self.assertEqual(self.transport.count("head_object"), 16)
        self.assertEqual(self.transport.count("get_object"), 0)
        self.assertGreater(self.transport.max_active["head_object"], 1)
        self.assertLessEqual(self.transport.max_active["head_object"], self.store.VERIFY_WORKERS)
        self.checkout_and_assert()

    def test_denied_listing_falls_back_to_exact_heads(self):
        self.make_repo(16, warm=True)
        self.transport.deny_listing = True
        self.run_store()
        self.assertGreaterEqual(self.transport.count("list_object_versions"), 1)
        self.assertEqual(self.transport.count("head_object"), 16)
        self.assertEqual(self.transport.count("get_object"), 0)

    def test_warm_cache_still_rejects_a_missing_pinned_version(self):
        self.make_repo(16, warm=True)
        self.transport.versions.remove(self.object_entries[0])
        with self.assertRaises(RuntimeError):
            self.run_store()
        self.assertEqual(self.transport.payload_gets(), [])
        self.assertFalse(self.payload.exists())

    def test_requested_delete_marker_is_not_accepted_as_object_data(self):
        self.make_repo(16, warm=True)
        self.object_entries[0]["delete_marker"] = True
        with self.assertRaises(RuntimeError):
            self.run_store()
        self.assertEqual(self.transport.payload_gets(), [])
        self.assertFalse(self.payload.exists())

    def test_cold_missing_version_preserves_existing_payload(self):
        self.make_repo(directory=False)
        self.transport.versions.clear()
        self.payload.write_bytes(b"local work must survive")
        before_pointer = self.pointer.read_bytes()
        with self.assertRaises(RuntimeError):
            self.run_store()
        self.assertEqual(self.payload.read_bytes(), b"local work must survive")
        self.assertEqual(self.pointer.read_bytes(), before_pointer)
        self.assertFalse(self.cache_files[0].exists())

    def test_corrupt_response_never_enters_cache_or_overwrites_payload(self):
        self.make_repo(directory=False)
        entry = self.object_entries[0]
        bad_body = b"X" * entry["Size"]
        self.transport.overrides[(entry["Key"], entry["VersionId"])] = {"Body": ResponseBody(bad_body)}
        self.payload.write_bytes(b"local work must survive")
        with self.assertRaises(RuntimeError):
            self.run_store()
        self.assertEqual(self.payload.read_bytes(), b"local work must survive")
        self.assertFalse(self.cache_files[0].exists())

    def test_wrong_response_version_is_rejected_before_cache_write(self):
        self.make_repo(directory=False)
        entry = self.object_entries[0]
        self.transport.overrides[(entry["Key"], entry["VersionId"])] = {"VersionId": "wrong-version"}
        with self.assertRaises(RuntimeError):
            self.run_store()
        self.assertFalse(self.cache_files[0].exists())

    def test_truncated_response_is_rejected_before_cache_write(self):
        self.make_repo(directory=False)
        entry = self.object_entries[0]
        self.transport.overrides[(entry["Key"], entry["VersionId"])] = {"Body": ResponseBody(entry["body"][:-1])}
        with self.assertRaises(RuntimeError):
            self.run_store()
        self.assertFalse(self.cache_files[0].exists())

    def test_incomplete_directory_metadata_fails_without_remote_discovery(self):
        self.make_repo(16)
        data = yaml.safe_load(self.pointer.read_text())
        del data["outs"][0]["files"]
        self.pointer.write_text(yaml.safe_dump(data))
        with self.assertRaises(RuntimeError):
            self.run_store()
        self.assertEqual(self.transport.count("get_object"), 0)
        self.assertEqual(self.transport.count("list_object_versions"), 0)
        self.assertFalse(self.payload.exists())

    def test_verify_only_never_downloads_payload(self):
        self.make_repo(16)
        self.run_store(fetch=False)
        self.assertEqual(self.transport.count("get_object"), 0)
        self.assertEqual(self.transport.count("list_object_versions"), 1)
        self.assertFalse(any(path.exists() for path in self.cache_files))


def benchmark(objects=100, latency_ms=10.0):
    """Compare actual DVC fetch with direct hydration using the same transport.

    This reports synthetic request latency, not AWS throughput. It is opt-in so
    ordinary regression runs have no timing threshold or expensive baseline.
    """
    if not HAS_STORAGE_RUNTIME:
        raise SystemExit("benchmark requires the pinned DVC + s3fs storage runtime")
    DvcVersionStoreIntegrationTests.setUpClass()
    for implementation in ("dvc-fetch", "direct-version-fetch"):
        fixture = DvcVersionStoreIntegrationTests()
        try:
            fixture.setUp()
            fixture.make_repo(objects)
            fixture.transport.latency = latency_ms / 1000.0
            fixture.transport.allow_legacy_probe = implementation == "dvc-fetch"
            for cache in ("cold", "warm"):
                s3fs.S3FileSystem.clear_instance_cache()
                fixture.transport.reset_counts()
                started = time.perf_counter()
                if implementation == "dvc-fetch":
                    with DvcRepo(str(fixture.repo_path)) as repo:
                        repo.fetch(targets=[str(fixture.pointer)])
                else:
                    fixture.run_store()
                elapsed = time.perf_counter() - started
                calls = fixture.transport.calls
                print(json.dumps({
                    "implementation": implementation,
                    "objects": objects,
                    "cache": cache,
                    "latency_ms": latency_ms,
                    "seconds": round(elapsed, 4),
                    "requests": dict(Counter(method for method, _ in calls)),
                    "range_gets": sum(method == "get_object" and "Range" in params for method, params in calls),
                    "max_concurrency": dict(fixture.transport.max_active),
                }), flush=True)
            fixture.checkout_and_assert()
        finally:
            fixture.doCleanups()


if __name__ == "__main__":
    if "--benchmark" in sys.argv:
        parser = argparse.ArgumentParser(description=__doc__)
        parser.add_argument("--benchmark", action="store_true")
        parser.add_argument("--objects", type=int, default=100)
        parser.add_argument("--latency-ms", type=float, default=10.0)
        args = parser.parse_args()
        if args.objects < 1 or args.latency_ms < 0:
            parser.error("objects must be positive and latency must be nonnegative")
        benchmark(args.objects, args.latency_ms)
    else:
        unittest.main()
