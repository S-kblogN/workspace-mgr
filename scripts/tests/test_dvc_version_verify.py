from __future__ import annotations

import errno
import importlib.util
from pathlib import Path
import sys
import threading
from types import SimpleNamespace
import unittest
from unittest import mock


SCRIPT = Path(__file__).parents[2] / "tests" / "oracles" / "dvc_version_verify.py"
SPEC = importlib.util.spec_from_file_location("dvc_version_verify", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
verifier = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = verifier
SPEC.loader.exec_module(verifier)


def entry(name: str, version: str = "recorded-version", size: int = 7):
    return verifier.Entry(
        object_name=name,
        key=f"remote/{name}",
        version_id=version,
        size=size,
        etag="expected-etag",
        md5="9e107d9d372bb6826bd81d3542a419d6",
        hash_name="md5",
    )


def listed(item, **changes):
    result = {
        "Key": item.key,
        "VersionId": item.version_id,
        "Size": item.size,
        "ETag": f'"{item.etag}"',
        "IsLatest": False,
    }
    result.update(changes)
    return result


def headed(item, **changes):
    result = {
        "VersionId": item.version_id,
        "ContentLength": item.size,
        "ETag": f'"{item.etag}"',
    }
    result.update(changes)
    return result


class ProviderError(Exception):
    def __init__(self, code: str):
        super().__init__(code)
        self.response = {"Error": {"Code": code}}


class FakeS3:
    def __init__(self, entries, pages=(), list_error=None, head_hook=None):
        self.pages = iter(pages)
        self.list_error = list_error
        self.head_hook = head_hook
        self.head_results = {
            (item.key, item.version_id): headed(item) for item in entries
        }
        self.calls = []
        self.lock = threading.Lock()

    def call_s3(self, operation, **request):
        with self.lock:
            self.calls.append((operation, request.copy()))
        if operation == "list_object_versions":
            if self.list_error:
                raise self.list_error
            try:
                return next(self.pages)
            except StopIteration as exc:
                raise AssertionError("unexpected extra version-list request") from exc
        if operation != "head_object":
            raise AssertionError(f"verification attempted {operation!r}")
        if self.head_hook:
            self.head_hook(request)
        result = self.head_results[(request["Key"], request["VersionId"])]
        if isinstance(result, Exception):
            raise result
        return result

    def requests(self, operation):
        return [request for method, request in self.calls if method == operation]


class VersionVerificationTests(unittest.TestCase):
    def dense_entries(self, parent="task/data"):
        return [entry(f"{parent}/item-{index}") for index in range(8)]

    def verify(self, remote, entries):
        verifier.verify_entries(remote, "synthetic-bucket", entries)
        self.assertTrue(
            all(call[1]["Bucket"] == "synthetic-bucket" for call in remote.calls)
        )

    def test_one_page_verifies_multiple_exact_historical_versions(self):
        entries = self.dense_entries()
        remote = FakeS3(entries, [{"Versions": [listed(item) for item in entries]}])
        self.verify(remote, entries)
        self.assertEqual(len(remote.requests("list_object_versions")), 1)
        self.assertEqual(remote.requests("head_object"), [])

    def test_stop_when_all_versions_found_even_if_more_history_exists(self):
        entries = self.dense_entries()
        remote = FakeS3(
            entries,
            [{
                "Versions": [listed(item) for item in entries],
                "IsTruncated": True,
                "NextKeyMarker": "remote/task/data/next",
                "NextVersionIdMarker": "older-version",
            }],
        )
        self.verify(remote, entries)
        self.assertEqual(len(remote.calls), 1)

    def test_second_page_preserves_both_pagination_markers(self):
        entries = self.dense_entries()
        remote = FakeS3(
            entries,
            [
                {
                    "Versions": [listed(item) for item in entries[:4]],
                    "IsTruncated": True,
                    "NextKeyMarker": entries[3].key,
                    "NextVersionIdMarker": "last-returned-version",
                },
                {"Versions": [listed(item) for item in entries[4:]]},
            ],
        )
        self.verify(remote, entries)
        requests = remote.requests("list_object_versions")
        self.assertEqual(len(requests), 2)
        self.assertEqual(requests[1]["KeyMarker"], entries[3].key)
        self.assertEqual(requests[1]["VersionIdMarker"], "last-returned-version")
        self.assertEqual(remote.requests("head_object"), [])

    def test_history_scan_is_capped_and_only_unresolved_versions_use_head(self):
        entries = self.dense_entries()
        pages = [
            {
                "Versions": [listed(item) for item in entries[:2]],
                "IsTruncated": True,
                "NextKeyMarker": "page-one-key",
                "NextVersionIdMarker": "page-one-version",
            },
            {
                "Versions": [listed(item) for item in entries[2:4]],
                "IsTruncated": True,
                "NextKeyMarker": "page-two-key",
                "NextVersionIdMarker": "page-two-version",
            },
        ]
        remote = FakeS3(entries, pages)
        self.verify(remote, entries)
        self.assertEqual(len(remote.requests("list_object_versions")), 2)
        self.assertEqual(
            {(request["Key"], request["VersionId"]) for request in remote.requests("head_object")},
            {(item.key, item.version_id) for item in entries[4:]},
        )

    def test_missing_pagination_markers_fall_back_without_rescanning(self):
        entries = self.dense_entries()
        remote = FakeS3(entries, [{"Versions": [], "IsTruncated": True}])
        self.verify(remote, entries)
        self.assertEqual(len(remote.requests("list_object_versions")), 1)
        self.assertEqual(len(remote.requests("head_object")), len(entries))

    def test_repeated_pagination_markers_do_not_loop(self):
        entries = self.dense_entries()
        page = {
            "Versions": [],
            "IsTruncated": True,
            "NextKeyMarker": "stuck-key",
            "NextVersionIdMarker": "stuck-version",
        }
        remote = FakeS3(entries, [page, page.copy()])
        self.verify(remote, entries)
        self.assertEqual(len(remote.requests("list_object_versions")), 2)
        self.assertEqual(len(remote.requests("head_object")), len(entries))

    def test_deleted_latest_version_does_not_hide_recorded_old_payload(self):
        entries = self.dense_entries()
        remote = FakeS3(
            entries,
            [{
                "Versions": [listed(item) for item in entries],
                "DeleteMarkers": [
                    {"Key": item.key, "VersionId": "current-delete", "IsLatest": True}
                    for item in entries
                ],
            }],
        )
        self.verify(remote, entries)
        self.assertEqual(remote.requests("head_object"), [])

    def test_delete_marker_cannot_satisfy_a_payload_version(self):
        entries = self.dense_entries()
        remote = FakeS3(
            entries,
            [{
                "Versions": [listed(item) for item in entries[1:]],
                "DeleteMarkers": [listed(entries[0])],
            }],
        )
        remote.head_results[(entries[0].key, entries[0].version_id)] = FileNotFoundError("deleted")
        with self.assertRaisesRegex(RuntimeError, "missing|mismatched"):
            self.verify(remote, entries)
        self.assertEqual(
            [(request["Key"], request["VersionId"]) for request in remote.requests("head_object")],
            [(entries[0].key, entries[0].version_id)],
        )

    def test_two_recorded_versions_at_same_key_are_both_required(self):
        entries = [entry(f"task/data/item-{index}", version) for index in range(4) for version in ("old", "new")]
        remote = FakeS3(entries, [{"Versions": [listed(item) for item in entries if item.version_id == "new"]}])
        self.verify(remote, entries)
        self.assertEqual(
            {(request["Key"], request["VersionId"]) for request in remote.requests("head_object")},
            {(item.key, "old") for item in entries if item.version_id == "old"},
        )

    def test_version_id_on_another_key_cannot_satisfy_a_requested_object(self):
        entries = self.dense_entries()
        remote = FakeS3(
            entries,
            [{"Versions": [listed(item, Key=item.key + "-unrelated") for item in entries]}],
        )
        self.verify(remote, entries)
        self.assertEqual(len(remote.requests("head_object")), len(entries))

    def test_prefixes_end_at_the_actual_parent_directory(self):
        entries = self.dense_entries("task/data") + self.dense_entries("task/database")
        remote = FakeS3(entries, list_error=PermissionError(errno.EACCES, "denied"))
        self.verify(remote, entries)
        self.assertEqual(
            {request["Prefix"] for request in remote.requests("list_object_versions")},
            {"remote/task/data/", "remote/task/database/"},
        )

    def test_unavailable_listing_falls_back_to_exact_heads(self):
        entries = self.dense_entries()
        for error in (PermissionError(errno.EACCES, "denied"), ProviderError("AccessDenied"), ProviderError("NotImplemented")):
            with self.subTest(error=repr(error)):
                remote = FakeS3(entries, list_error=error)
                self.verify(remote, entries)
                self.assertEqual(len(remote.requests("list_object_versions")), 1)
                self.assertEqual(
                    {(request["Key"], request["VersionId"]) for request in remote.requests("head_object")},
                    {(item.key, item.version_id) for item in entries},
                )

    def test_other_provider_failures_are_not_silently_retried_as_heads(self):
        entries = self.dense_entries()
        for error in (TimeoutError("read timed out"), ProviderError("InternalError")):
            with self.subTest(error=repr(error)):
                remote = FakeS3(entries, list_error=error)
                with self.assertRaises(type(error)) as raised:
                    self.verify(remote, entries)
                self.assertIs(raised.exception, error)
                self.assertEqual(remote.requests("head_object"), [])

    def test_sparse_objects_skip_listing_and_use_exact_version_heads(self):
        entries = [entry(f"task-{index}/data") for index in range(7)]
        remote = FakeS3(entries)
        self.verify(remote, entries)
        self.assertEqual(remote.requests("list_object_versions"), [])
        self.assertEqual(
            {(request["Key"], request["VersionId"]) for request in remote.requests("head_object")},
            {(item.key, item.version_id) for item in entries},
        )

    def test_head_requests_are_parallel_and_have_a_fixed_bound(self):
        entries = [entry(f"task-{index}/data") for index in range(40)]
        condition = threading.Condition()
        release = threading.Event()
        active = 0
        peak = 0

        def block_request(_):
            nonlocal active, peak
            with condition:
                active += 1
                peak = max(peak, active)
                condition.notify_all()
            try:
                if not release.wait(3):
                    raise AssertionError("parallel verification failed to make progress")
            finally:
                with condition:
                    active -= 1

        def release_requests():
            with condition:
                condition.wait_for(lambda: active >= 16, timeout=1)
                # Keep requests blocked briefly after the pool is full so an
                # unbounded implementation cannot pass by completing early.
                condition.wait_for(lambda: active > 16, timeout=0.05)
            release.set()

        supervisor = threading.Thread(target=release_requests)
        supervisor.start()
        try:
            self.verify(FakeS3(entries, head_hook=block_request), entries)
        finally:
            release.set()
            supervisor.join()
        self.assertGreaterEqual(peak, 2)
        self.assertLessEqual(peak, 16)

    def test_completed_request_refills_pool_while_first_request_is_blocked(self):
        first_started = threading.Event()
        release_first = threading.Event()
        replacement_started = threading.Event()
        results = []
        errors = []

        def request(index):
            if index == 0:
                first_started.set()
                if not release_first.wait(5):
                    raise AssertionError("first request was never released")
            elif index == 1:
                if not first_started.wait(2):
                    raise AssertionError("first request never started")
            else:
                if release_first.is_set():
                    raise AssertionError("replacement waited for the first request")
                replacement_started.set()
            return index

        def consume():
            try:
                results.extend(verifier.bounded_map(request, range(3)))
            except BaseException as error:
                errors.append(error)

        with mock.patch.object(verifier, "VERIFY_WORKERS", 2):
            consumer = threading.Thread(target=consume)
            consumer.start()
            try:
                self.assertTrue(first_started.wait(2), "first request never started")
                self.assertTrue(
                    replacement_started.wait(2),
                    "a completed request did not free its slot until the first request finished",
                )
            finally:
                release_first.set()
                consumer.join(timeout=5)
            self.assertFalse(consumer.is_alive(), "request consumer did not finish")
        self.assertEqual(errors, [])
        self.assertEqual(sorted(results), [0, 1, 2])

    def test_missing_objects_report_in_sorted_order_despite_input_order(self):
        entries = [entry("z-task/data"), entry("a-task/data")]
        remote = FakeS3(entries)
        for item in entries:
            remote.head_results[(item.key, item.version_id)] = FileNotFoundError("missing")
        with self.assertRaises(RuntimeError) as raised:
            self.verify(remote, entries)
        message = str(raised.exception)
        self.assertLess(message.index("a-task/data"), message.index("z-task/data"))

    def test_head_authorization_error_propagates(self):
        item = entry("task/data")
        remote = FakeS3([item])
        error = PermissionError(errno.EACCES, "exact version read denied")
        remote.head_results[(item.key, item.version_id)] = error
        with self.assertRaises(PermissionError) as raised:
            self.verify(remote, [item])
        self.assertIs(raised.exception, error)

    def test_listed_metadata_mismatch_cannot_be_replaced_by_a_matching_head(self):
        entries = self.dense_entries()
        records = [listed(item) for item in entries]
        records[0]["Size"] += 1
        remote = FakeS3(entries, [{"Versions": records}])
        with self.assertRaisesRegex(RuntimeError, "mismatched.*size"):
            self.verify(remote, entries)

    def test_size_zero_and_quoted_etag_are_validated_without_truthiness_loss(self):
        item = entry("task/empty", size=0)
        verifier.validate_info(item, headed(item))
        verifier.validate_info(item, listed(item))
        with self.assertRaisesRegex(RuntimeError, "mismatched.*size"):
            verifier.validate_info(item, headed(item, ContentLength=1))

    def test_each_required_metadata_field_is_checked(self):
        item = entry("task/data")
        for field, changed, expected in (
            ("VersionId", "another-version", "version"),
            ("ContentLength", 8, "size"),
            ("ETag", '"another-etag"', "etag"),
        ):
            with self.subTest(field=field):
                with self.assertRaisesRegex(RuntimeError, f"mismatched.*{expected}"):
                    verifier.validate_info(item, headed(item, **{field: changed}))
        info = headed(item)
        del info["VersionId"]
        with self.assertRaisesRegex(RuntimeError, "mismatched.*version"):
            verifier.validate_info(item, info)
        info = headed(item)
        del info["ETag"]
        with self.assertRaisesRegex(RuntimeError, "mismatched.*etag"):
            verifier.validate_info(item, info)


class MetadataValidationTests(unittest.TestCase):
    def setUp(self):
        def split_path(path):
            bucket, _, key = path.partition("/")
            return bucket, key, None

        raw_fs = SimpleNamespace(split_path=split_path)
        self.remote = SimpleNamespace(
            name="workspace-mgr",
            path="synthetic-bucket/remote",
            fs=SimpleNamespace(fs=raw_fs, join=lambda *parts: "/".join(parts)),
        )
        self.output = SimpleNamespace(
            is_in_repo=True,
            can_push=True,
            use_cache=True,
            index_key=("repo", ("task", "data")),
            remote=None,
            hash_name="md5",
            hash_info=SimpleNamespace(isdir=False, value="9e107d9d372bb6826bd81d3542a419d6"),
            meta=SimpleNamespace(version_id="recorded-version", remote=None, size=7, etag="expected-etag"),
            cache=SimpleNamespace(
                fs=SimpleNamespace(protocol="local"),
                oid_to_path=lambda digest: f"/synthetic-cache/{digest}",
            ),
        )
        self.repo = SimpleNamespace(
            root_dir="/synthetic-repo",
            stage=SimpleNamespace(
                collect=lambda _: [SimpleNamespace(outs=[self.output])],
            ),
        )

    def collect(self):
        return verifier.collect_entries(self.repo, self.remote, ["task/data.dvc"])

    def test_missing_or_null_version_id_is_rejected_before_remote_discovery(self):
        for value in (None, "", "null"):
            with self.subTest(version=value):
                self.output.meta.version_id = value
                with self.assertRaisesRegex(RuntimeError, "no exact version ID"):
                    self.collect()

    def test_missing_or_malformed_content_hash_is_rejected(self):
        for value in (None, "", "not-a-content-hash"):
            with self.subTest(digest=value):
                self.output.hash_info.value = value
                with self.assertRaisesRegex(RuntimeError, "no supported content hash"):
                    self.collect()

    def test_incomplete_directory_manifest_cannot_trigger_remote_directory_walk(self):
        self.output.hash_info.isdir = True
        self.output.files = None
        with self.assertRaisesRegex(RuntimeError, "directory metadata is incomplete"):
            self.collect()

    def test_directory_paths_cannot_escape_the_recorded_boundary(self):
        self.output.hash_info.isdir = True
        for path in ("../outside", "/absolute", "", "."):
            with self.subTest(path=path):
                self.output.files = [{"relpath": path}]
                with self.assertRaisesRegex(RuntimeError, "invalid path"):
                    self.collect()

    def test_unexpected_remote_is_rejected(self):
        self.output.meta.remote = "another-remote"
        with self.assertRaisesRegex(RuntimeError, "unexpected remote"):
            self.collect()

    def test_nonlocal_cache_is_rejected(self):
        self.output.cache.fs.protocol = "s3"
        with self.assertRaisesRegex(RuntimeError, "local cache"):
            self.collect()

    def test_file_manifest_retains_exact_key_and_version(self):
        bucket, entries, trees = self.collect()
        self.assertEqual(bucket, "synthetic-bucket")
        self.assertEqual(trees, [])
        self.assertEqual(len(entries), 1)
        self.assertEqual(entries[0].object_name, "task/data")
        self.assertEqual(entries[0].key, "remote/task/data")
        self.assertEqual(entries[0].version_id, "recorded-version")


if __name__ == "__main__":
    unittest.main()
