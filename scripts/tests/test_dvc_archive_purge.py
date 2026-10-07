"""Archive retirement deletes only receipt-mapped S3 version IDs."""

from __future__ import annotations

from collections import Counter
import importlib.util
import io
import json
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[2] / "assets" / "dvc_version_purge.py"
SPEC = importlib.util.spec_from_file_location("dvc_version_purge", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
purger = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(purger)

BUCKET = "private-fixture"
SOURCE = "20260712-old-task"
POINTER = SOURCE + "/.workspace-mgr-archive.json"


def candidate(name, version, pointer=POINTER):
    return {"pointer": pointer, "object": SOURCE + "/" + name, "version_id": version}


class VersionTransport:
    def __init__(self, page_size=1000):
        self.versions = []
        self.page_size = page_size
        self.calls = []
        self.counts = Counter()
        self.fail_delete_number = None
        self.keep_deleted_versions = False
        self.hook = None

    def add(self, name, version, marker=False):
        self.versions.append({"Key": f"storage/{SOURCE}/{name}", "VersionId": version, "marker": marker})

    def split_path(self, path):
        path = path.removeprefix("s3://")
        bucket, _, key = path.partition("/")
        key, _, version = key.partition("?versionId=")
        return bucket, key, version or None

    def call_s3(self, method, **request):
        self.calls.append((method, request.copy()))
        self.counts[method] += 1
        if request["Bucket"] != BUCKET:
            raise AssertionError("purge escaped its configured bucket")
        if self.hook:
            self.hook(method, request, self.counts[method])
        if method == "delete_object":
            if not request.get("VersionId"):
                raise AssertionError("purge must explicitly name every deleted version")
            if self.counts[method] == self.fail_delete_number:
                raise ConnectionError("fixture fails a deletion")
            if not self.keep_deleted_versions:
                self.versions = [row for row in self.versions
                                 if (row["Key"], row["VersionId"]) != (request["Key"], request["VersionId"])]
            return {}
        if method != "list_object_versions":
            raise AssertionError(f"unexpected operation {method}")
        entries = sorted((row for row in self.versions if row["Key"].startswith(request["Prefix"])),
                         key=lambda row: (row["Key"], row["VersionId"]))
        start = 0
        if request.get("KeyMarker"):
            start = next(index + 1 for index, row in enumerate(entries)
                         if (row["Key"], row["VersionId"]) == (request["KeyMarker"], request["VersionIdMarker"]))
        page = entries[start:start + min(self.page_size, request["MaxKeys"])]
        result = {"Versions": [], "DeleteMarkers": [], "IsTruncated": start + len(page) < len(entries)}
        for row in page:
            result["DeleteMarkers" if row["marker"] else "Versions"].append(
                {"Key": row["Key"], "VersionId": row["VersionId"]})
        if result["IsTruncated"]:
            result["NextKeyMarker"], result["NextVersionIdMarker"] = page[-1]["Key"], page[-1]["VersionId"]
        return result


class B2VersionTransport(VersionTransport):
    endpoint_url = "https://s3.fixture.backblazeb2.com"
    client_kwargs = {}

    def call_s3(self, method, **request):
        if method == "get_object":
            self.calls.append((method, request.copy()))
            self.counts[method] += 1
            version = request.get("VersionId")
            if not version:
                raise AssertionError("B2 registry must read exact historical versions")
            row = next(row for row in self.versions
                       if (row["Key"], row["VersionId"]) == (request["Key"], version))
            return {"Body": io.BytesIO(row["body"]), "VersionId": version}
        return super().call_s3(method, **request)


class ArchivePurgeTests(unittest.TestCase):
    def setUp(self):
        self.transport = VersionTransport()
        self.remote = SimpleNamespace(name="workspace-mgr", path=f"{BUCKET}/storage",
                                      fs=SimpleNamespace(join=lambda *parts: "/".join(parts)))
        self.registry_calls = []
        self.mapped = []
        self.registry_failure = None

        def read(source):
            self.registry_calls.append(source)
            if self.registry_failure is not None:
                raise self.registry_failure
            return {"versions": [{"source_object": row["object"], "source_version_id": row["version_id"]}
                                 for row in self.mapped]}

        self.registry = SimpleNamespace(read=read)

    def delete(self, candidates):
        self.mapped = [row for row in candidates if row["pointer"].endswith("/.workspace-mgr-archive.json")]
        return purger.delete_candidates(self.transport, self.remote, BUCKET, candidates, self.registry)

    def test_archive_deletes_every_mapped_payload_and_marker_but_retains_later_versions(self):
        self.transport.add("data", "old-one")
        self.transport.add("data", "old-marker", marker=True)
        self.transport.add("data", "new-after-copy")
        result = self.delete([candidate("data", "old-one"), candidate("data", "old-marker")])
        self.assertEqual(result["deleted"][0]["deleted_version_ids"], ["old-marker", "old-one"])
        self.assertEqual(result["retained_unmapped"], [candidate("data", "new-after-copy")])
        self.assertEqual([row["VersionId"] for row in self.transport.versions], ["new-after-copy"])

    def test_archive_reports_new_unplanned_keys_and_delete_markers_without_erasing_them(self):
        self.transport.add("data", "mapped")
        self.transport.add("unplanned", "new-payload")
        self.transport.add("unplanned", "new-marker", marker=True)
        result = self.delete([candidate("data", "mapped")])
        self.assertEqual(result["retained_unmapped"], [candidate("unplanned", "new-marker"), candidate("unplanned", "new-payload")])
        self.assertEqual(self.transport.counts["delete_object"], 1)

    def test_archive_mapping_wins_over_generic_all_version_retirement(self):
        self.transport.add("data", "mapped")
        self.transport.add("data", "unmapped")
        result = self.delete([candidate("data", "mapped"), candidate("data", "unmapped", pointer=SOURCE + "/data.dvc")])
        self.assertEqual(result["retained_unmapped"], [candidate("data", "unmapped")])
        self.assertEqual(self.transport.counts["delete_object"], 1)

    def test_generic_cleanup_under_an_archived_prefix_cannot_delete_an_unmapped_new_key(self):
        self.transport.add("data", "mapped")
        self.transport.add("other", "unmapped")
        result = self.delete([candidate("data", "mapped"), candidate("other", "unmapped", pointer=SOURCE + "/other.dvc")])
        self.assertEqual(result["retained_unmapped"], [candidate("other", "unmapped")])
        self.assertEqual(self.transport.counts["delete_object"], 1)

    def test_generic_rename_and_removal_still_delete_all_versions_and_markers(self):
        self.transport.add("data", "first")
        self.transport.add("data", "second")
        self.transport.add("data", "marker", marker=True)
        result = self.delete([candidate("data", "second", pointer=SOURCE + "/data.dvc")])
        self.assertEqual(result["deleted"][0]["deleted_version_ids"], ["first", "marker", "second"])
        self.assertEqual(result["retained_unmapped"], [])
        self.assertEqual(self.transport.versions, [])

    def test_archive_cleanup_and_postverification_read_every_pagination_page(self):
        self.transport.page_size = 1
        self.transport.add("data", "a-mapped")
        self.transport.add("data", "b-mapped", marker=True)
        self.transport.add("data", "c-later")
        self.transport.add("new", "d-later")
        result = self.delete([candidate("data", "a-mapped"), candidate("data", "b-mapped")])
        self.assertEqual(result["retained_unmapped"], [candidate("data", "c-later"), candidate("new", "d-later")])
        self.assertEqual(self.transport.counts["list_object_versions"], 6)
        continuation = [request for method, request in self.transport.calls
                        if method == "list_object_versions" and request.get("KeyMarker")]
        self.assertTrue(all(request.get("VersionIdMarker") for request in continuation))

    def test_postverification_cannot_miss_a_mapped_version_hidden_on_a_later_page(self):
        self.transport.page_size = 1
        self.transport.keep_deleted_versions = True
        self.transport.add("data", "a-unmapped")
        self.transport.add("data", "z-mapped")
        with self.assertRaisesRegex(RuntimeError, "mapped archive object versions still exist"):
            self.delete([candidate("data", "z-mapped")])
        self.assertEqual(self.transport.counts["delete_object"], 1)

    def test_new_write_during_cleanup_is_retained_and_reported(self):
        self.transport.add("data", "mapped")
        def write_after_delete(method, request, count):
            if method == "list_object_versions" and count == 2:
                self.transport.add("data", "created-during-cleanup")
        self.transport.hook = write_after_delete
        result = self.delete([candidate("data", "mapped")])
        self.assertEqual(result["retained_unmapped"], [candidate("data", "created-during-cleanup")])

    def test_archive_retry_after_partial_deletion_is_safe_and_idempotent(self):
        self.transport.add("data", "a-mapped")
        self.transport.add("data", "b-mapped")
        self.transport.add("data", "unmapped")
        candidates = [candidate("data", "a-mapped"), candidate("data", "b-mapped")]
        self.transport.fail_delete_number = 2
        with self.assertRaises(ConnectionError):
            self.delete(candidates)
        retried = self.delete(candidates)
        self.assertEqual(retried["deleted"][0]["deleted_version_ids"], ["b-mapped"])
        self.assertEqual(retried["retained_unmapped"], [candidate("data", "unmapped")])
        repeated = self.delete(candidates)
        self.assertEqual(repeated["deleted"], [])
        self.assertEqual(repeated["retained_unmapped"], [candidate("data", "unmapped")])

    def test_archive_preserves_literal_folder_marker_and_doubled_separator_keys(self):
        self.transport.add("folder/", "folder-version")
        self.transport.add("folder//file", "file-version")
        self.transport.add("folder/file", "unmapped-distinct-key")
        result = self.delete([candidate("folder/", "folder-version"), candidate("folder//file", "file-version")])
        deletes = [request["Key"] for method, request in self.transport.calls if method == "delete_object"]
        self.assertEqual(deletes, [f"storage/{SOURCE}/folder/", f"storage/{SOURCE}/folder//file"])
        self.assertEqual(result["retained_unmapped"], [candidate("folder/file", "unmapped-distinct-key")])

    def test_archive_can_retire_a_legacy_null_version_without_touching_newer_history(self):
        self.transport.add("data", "null")
        self.transport.add("data", "versioned")
        result = self.delete([candidate("data", "null")])
        self.assertEqual(result["deleted"][0]["deleted_version_ids"], ["null"])
        self.assertEqual(result["retained_unmapped"], [candidate("data", "versioned")])

    def test_literal_archive_key_cannot_be_reinterpreted_as_an_s3_version_selector(self):
        self.transport.add("file?versionId=literal-name", "null")
        self.transport.add("file", "null")
        result = self.delete([candidate("file?versionId=literal-name", "null")])
        self.assertEqual(result["retained_unmapped"], [candidate("file", "null")])
        deletes = [request for method, request in self.transport.calls if method == "delete_object"]
        self.assertEqual(deletes[0]["Key"], f"storage/{SOURCE}/file?versionId=literal-name")

    def test_generic_exact_key_cleanup_does_not_delete_prefix_neighbors(self):
        self.transport.add("data", "mapped")
        self.transport.add("data-more", "neighbor")
        self.delete([candidate("data", "mapped", pointer=SOURCE + "/data.dvc")])
        self.assertEqual([row["VersionId"] for row in self.transport.versions], ["neighbor"])

    def test_archive_candidate_cannot_escape_its_source_task(self):
        escaped = {"pointer": POINTER, "object": "other-task/data", "version_id": "v1"}
        with self.assertRaisesRegex(RuntimeError, "escapes its task prefix"):
            self.delete([escaped])
        self.assertEqual(self.transport.counts["delete_object"], 0)


    def test_registry_conflict_blocks_all_source_cleanup(self):
        self.transport.add("data", "mapped")
        self.registry_failure = RuntimeError("conflicting archive registry history")
        with self.assertRaisesRegex(RuntimeError, "conflicting archive registry history"):
            self.delete([candidate("data", "mapped")])
        self.assertEqual(self.transport.counts["delete_object"], 0)
        self.assertEqual([row["VersionId"] for row in self.transport.versions], ["mapped"])

    def test_registry_is_rechecked_before_each_exact_source_deletion(self):
        self.transport.add("data", "first")
        self.transport.add("data", "second")

        def conflicting_writer(method, request, count):
            if method == "delete_object" and count == 1:
                self.registry_failure = RuntimeError("conflicting archive registry history")

        self.transport.hook = conflicting_writer
        with self.assertRaisesRegex(RuntimeError, "conflicting archive registry history"):
            self.delete([candidate("data", "first"), candidate("data", "second")])
        self.assertEqual(self.transport.counts["delete_object"], 1)
        self.assertEqual([row["VersionId"] for row in self.transport.versions], ["second"])
        self.assertEqual(len(self.registry_calls), 3)

    def test_unmapped_or_unpublished_registry_never_authorizes_archive_deletion(self):
        self.transport.add("data", "mapped")
        for result, message in ((None, "requires a published"), ({"versions": []}, "not mapped")):
            registry = SimpleNamespace(read=lambda source: result)
            with self.assertRaisesRegex(RuntimeError, message):
                purger.delete_candidates(self.transport, self.remote, BUCKET,
                                         [candidate("data", "mapped")], registry)
        self.assertEqual(self.transport.counts["delete_object"], 0)

    def test_nonadvancing_pagination_cannot_trigger_deletion_after_partial_inventory(self):
        response = {"IsTruncated": True, "NextKeyMarker": "fixed", "NextVersionIdMarker": "fixed"}
        with mock.patch.object(self.transport, "call_s3", return_value=response) as request:
            with self.assertRaisesRegex(RuntimeError, "cannot advance"):
                self.delete([candidate("data", "mapped")])
        self.assertEqual(request.call_count, 2)
        self.assertEqual(self.transport.counts["delete_object"], 0)


class B2ArchivePurgeTests(unittest.TestCase):
    def setUp(self):
        self.transport = B2VersionTransport(page_size=1)
        self.remote = SimpleNamespace(name="workspace-mgr", path=f"{BUCKET}/storage",
                                      fs=SimpleNamespace(join=lambda *parts: "/".join(parts)))
        spec = importlib.util.spec_from_file_location(
            "purge_registry_fixture", SCRIPT.with_name("dvc_archive_registry.py"))
        self.registry_module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.registry_module)
        self.key = self.registry_module.registry_key("storage", SOURCE)
        self.transport.add("data", "mapped")

    def append_registry(self, version, *, destination="copy", marker=False):
        value = {"schema_version": 1, "source": SOURCE, "destination": "2026/07/" + SOURCE,
                 "bucket": BUCKET, "remote_prefix": "storage", "remote": "workspace-mgr",
                 "versions": [{"source_object": SOURCE + "/data",
                               "destination_object": "2026/07/" + SOURCE + "/data",
                               "source_version_id": "mapped", "destination_version_id": destination,
                               "delete_marker": False, "size": 7, "destination_etag": "etag"}]}
        self.transport.versions.append({"Key": self.key, "VersionId": version, "marker": marker,
                                        "body": json.dumps(value).encode()})

    def delete(self):
        return purger.delete_candidates(self.transport, self.remote, BUCKET, [candidate("data", "mapped")])

    def test_complete_b2_registry_history_retains_exact_originals_for_historical_reads(self):
        self.append_registry("registry-first")
        self.append_registry("registry-identical")
        result = self.delete()
        self.assertEqual(self.transport.counts["delete_object"], 0)
        self.assertEqual(len(self.transport.versions), 3)
        self.assertEqual(result["retained_mapped"], [{**candidate("data", "mapped"),
                                                    "reason": "append_only_registry_no_atomic_cleanup"}])
        self.assertEqual(result["deleted"], [])
        gets = [request for op, request in self.transport.calls if op == "get_object"]
        self.assertEqual({request["VersionId"] for request in gets}, {"registry-first", "registry-identical"})

    def test_mixed_generic_and_archive_candidates_are_reported_protected_exactly(self):
        self.append_registry("registry-first")
        self.transport.add("data", "generic-later")
        generic_same_version = candidate("data", "mapped", pointer=SOURCE + "/data.dvc")
        generic_later = candidate("data", "generic-later", pointer=SOURCE + "/data.dvc")
        generic_absent = candidate("data", "already-gone", pointer=SOURCE + "/data.dvc")
        result = purger.delete_candidates(self.transport, self.remote, BUCKET, [
            candidate("data", "mapped"), generic_same_version, generic_later, generic_absent,
        ])
        protected = [{key: row[key] for key in ("pointer", "object", "version_id")}
                     for row in result["retained_mapped"]]
        self.assertCountEqual(protected, [candidate("data", "mapped"), generic_same_version, generic_later])
        self.assertEqual(result["already_absent"], [generic_absent])
        self.assertEqual(self.transport.counts["delete_object"], 0)
        self.assertEqual({row["VersionId"] for row in self.transport.versions
                          if row["Key"].startswith(f"storage/{SOURCE}/")}, {"mapped", "generic-later"})

    def test_generic_only_retirement_discovers_b2_registry_without_archive_candidate(self):
        self.append_registry("registry-first")
        self.transport.add("data", "later-marker", marker=True)
        generic = candidate("data", "later-marker", pointer=SOURCE + "/retired.dvc")
        result = purger.delete_candidates(self.transport, self.remote, BUCKET, [generic])
        self.assertEqual(result["retained_mapped"], [{**generic,
                                                    "reason": "append_only_registry_no_atomic_cleanup"}])
        self.assertEqual(self.transport.counts["delete_object"], 0)
        self.assertEqual({row["VersionId"] for row in self.transport.versions
                          if row["Key"].startswith(f"storage/{SOURCE}/")}, {"mapped", "later-marker"})

    def test_generic_only_nested_retirement_checks_only_ancestor_registry_keys(self):
        self.append_registry("registry-first")
        self.transport.add("nested/retired", "generic")
        generic = candidate("nested/retired", "generic", pointer=SOURCE + "/nested/retired.dvc")
        result = purger.delete_candidates(self.transport, self.remote, BUCKET, [generic])
        self.assertEqual(result["retained_mapped"][0]["version_id"], "generic")
        prefixes = {request["Prefix"] for method, request in self.transport.calls if method == "list_object_versions"}
        self.assertEqual(prefixes, {
            self.registry_module.registry_key("storage", SOURCE + "/nested"), self.key,
            f"storage/{SOURCE}/nested/retired",
        })
        self.assertEqual(self.transport.counts["delete_object"], 0)

    def test_conflicting_registry_blocks_generic_only_b2_retirement(self):
        self.append_registry("registry-first", destination="different")
        self.append_registry("registry-second")
        generic = candidate("data", "mapped", pointer=SOURCE + "/retired.dvc")
        with self.assertRaisesRegex(RuntimeError, "conflicting archive registry history"):
            purger.delete_candidates(self.transport, self.remote, BUCKET, [generic])
        self.assertEqual(self.transport.counts["delete_object"], 0)
        self.assertEqual(len(self.transport.versions), 3)

    def test_b2_generic_retirement_without_any_archive_mapping_remains_supported(self):
        generic = candidate("data", "mapped", pointer=SOURCE + "/data.dvc")
        result = purger.delete_candidates(self.transport, self.remote, BUCKET, [generic])
        self.assertEqual(result["retained_mapped"], [])
        self.assertEqual(self.transport.counts["delete_object"], 1)
        self.assertEqual(self.transport.versions, [])

    def test_conflicting_append_after_verification_cannot_erase_originals_or_markers(self):
        self.append_registry("registry-first")
        value = json.loads(self.transport.versions[-1]["body"])
        value["versions"].append({**value["versions"][0], "source_version_id": "source-marker",
                                  "destination_version_id": "copy-marker", "delete_marker": True,
                                  "size": None, "destination_etag": None})
        self.transport.versions[-1]["body"] = json.dumps(value).encode()
        self.transport.add("data", "source-marker", marker=True)
        self.transport.add("other", "unmapped")

        def append_after_registry_check(method, request, count):
            if method == "list_object_versions" and request["Prefix"] == f"storage/{SOURCE}/":
                self.transport.hook = None
                self.append_registry("registry-concurrent-conflict", destination="different")

        self.transport.hook = append_after_registry_check
        result = purger.delete_candidates(self.transport, self.remote, BUCKET, [
            candidate("data", "mapped"), candidate("data", "source-marker"),
            candidate("other", "unmapped", pointer=SOURCE + "/other.dvc"),
        ])
        self.assertEqual(self.transport.counts["delete_object"], 0)
        self.assertEqual({row["version_id"] for row in result["retained_mapped"]}, {"mapped", "source-marker", "unmapped"})
        self.assertEqual(result["retained_unmapped"], [candidate("other", "unmapped")])
        self.assertEqual({row["VersionId"] for row in self.transport.versions
                          if row["Key"].startswith(f"storage/{SOURCE}/")},
                         {"mapped", "source-marker", "unmapped"})
        registry = self.registry_module.ArchiveRegistry(self.transport, BUCKET, "storage")
        with self.assertRaisesRegex(RuntimeError, "conflicting archive registry history"):
            registry.read(SOURCE)

    def test_paginated_conflicting_b2_registry_history_blocks_source_cleanup(self):
        self.append_registry("registry-first", destination="conflicting")
        self.append_registry("registry-newest")
        with self.assertRaisesRegex(RuntimeError, "conflicting archive registry history"):
            self.delete()
        self.assertEqual(self.transport.counts["delete_object"], 0)
        self.assertEqual(len(self.transport.versions), 3)

    def test_b2_registry_delete_marker_blocks_source_cleanup_without_hiding_history(self):
        self.append_registry("registry-first")
        self.append_registry("registry-hidden", marker=True)
        with self.assertRaisesRegex(RuntimeError, "delete marker"):
            self.delete()
        self.assertEqual(self.transport.counts["delete_object"], 0)
        self.assertEqual(len(self.transport.versions), 3)


if __name__ == "__main__":
    unittest.main()
