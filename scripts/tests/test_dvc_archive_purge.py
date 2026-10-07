"""Archive retirement deletes only receipt-mapped S3 version IDs."""

from __future__ import annotations

from collections import Counter
from datetime import datetime, timedelta, timezone
import copy
import importlib.util
import io
import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[2] / "tests" / "oracles" / "dvc_version_purge.py"
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

    def add(self, name, version, marker=False, *, copied=False):
        task = "2026/07/" + SOURCE if copied else SOURCE
        key = f"storage/{task}/{name}"
        for row in self.versions:
            if row["Key"] == key:
                row["IsLatest"] = False
        self.versions.append({"Key": key, "VersionId": version, "marker": marker,
                              "Size": 7, "ETag": '"etag"', "IsLatest": True,
                              "LastModified": datetime(2026, 7, 12, tzinfo=timezone.utc) + timedelta(seconds=len(self.versions))})

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
        if method == "head_object":
            self.calls.append((method, request.copy()))
            self.counts[method] += 1
            row = next((row for row in self.versions
                        if (row["Key"], row["VersionId"]) == (request["Key"], request.get("VersionId"))), None)
            if row is None or row["marker"]:
                raise FileNotFoundError("exact fixture payload is missing")
            return {"VersionId": row["VersionId"], "ContentLength": row["Size"],
                    "ETag": row["ETag"], "Metadata": {}}
        if method == "list_object_versions":
            # Let the base mock exercise complete two-marker pagination, then
            # supply the immutable S3 metadata needed to verify copied history.
            response = super().call_s3(method, **request)
            for section in ("Versions", "DeleteMarkers"):
                for item in response[section]:
                    original = next(row for row in self.versions
                                    if (row["Key"], row["VersionId"]) == (item["Key"], item["VersionId"]))
                    for field in ("Size", "ETag", "IsLatest", "LastModified"):
                        if field in original:
                            item[field] = original[field]
            return response
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
            if source != SOURCE or not self.mapped:
                return None
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

    def test_late_generic_registry_on_any_provider_cannot_retire_unpublished_originals(self):
        self.transport.add("data", "mapped")
        self.mapped = [candidate("data", "mapped")]
        with self.assertRaisesRegex(RuntimeError, "published receipt"):
            purger.delete_candidates(
                self.transport, self.remote, BUCKET,
                [candidate("data", "mapped", pointer=SOURCE + "/data.dvc")],
                self.registry, repo_path="/isolated-fixture")
        self.assertEqual(self.transport.counts["delete_object"], 0)
        self.assertEqual([row["VersionId"] for row in self.transport.versions], ["mapped"])

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
        helper = self.registry_module.archive_helper()
        sys.modules["dvc_version_archive"] = helper
        self.registry = self.registry_module.ArchiveRegistry(
            self.transport, BUCKET, "storage", coordination_check=self.check_binding)
        self.key = self.registry_module.registry_key("storage", SOURCE)
        self.transport.add("data", "mapped")
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.claim = None
        self.binding_checks = 0
        self.receipt = None
        self.proof = None

    def check_binding(self, receipt, coordination):
        self.binding_checks += 1
        if self.claim != coordination["receipt_sha256"]:
            raise RuntimeError("Git CAS binding was removed or replaced")

    def complete_copy(self):
        original = [row.copy() for row in self.transport.versions
                    if row["Key"].startswith(f"storage/{SOURCE}/")]
        rows = []
        for index, source in enumerate(original):
            name = source["Key"][len(f"storage/{SOURCE}/"):]
            copied = "copy-" + source["VersionId"]
            self.transport.add(name, copied, marker=source["marker"], copied=True)
            rows.append({
                "source_object": SOURCE + "/" + name,
                "destination_object": "2026/07/" + SOURCE + "/" + name,
                "source_version_id": source["VersionId"], "destination_version_id": copied,
                "source_last_modified": source["LastModified"].isoformat(),
                "source_is_latest": source["IsLatest"], "source_list_order": index,
                "delete_marker": source["marker"],
                "size": None if source["marker"] else source["Size"],
                "source_etag": None if source["marker"] else "etag",
                "destination_etag": None if source["marker"] else "etag",
                "destination_last_modified": self.transport.versions[-1]["LastModified"].isoformat(),
            })
        self.receipt = {"schema_version": 1, "status": "copied", "source": SOURCE,
                        "destination": "2026/07/" + SOURCE, "bucket": BUCKET,
                        "remote_prefix": "storage", "remote": "workspace-mgr",
                        "transaction_id": "isolated-purge-transaction", "versions": rows,
                        "source_cleanup": "after_verified_git_publication"}
        body = self.registry_module.encoded_receipt(self.receipt)
        digest = self.registry_module.hashlib.sha256(body).hexdigest()
        self.claim = digest
        journal = Path(self.directory.name) / "private-copy.json"
        journal.write_text(json.dumps(self.receipt))
        self.proof = {"receipt": self.receipt, "coordination": {
            "mode": "git-cas", "receipt_sha256": digest,
            "transaction_id": self.receipt["transaction_id"], "state_path": str(journal),
            "remote": "origin", "ref": self.registry_module.coordination_ref(self.receipt), "oid": "a" * 40,
        }}

    def append_registry(self, version, *, destination=None, marker=False):
        if self.receipt is None:
            self.complete_copy()
        value = copy.deepcopy(self.receipt)
        if destination is not None:
            value["versions"][0]["destination_version_id"] = destination
        self.transport.versions.append({"Key": self.key, "VersionId": version, "marker": marker,
                                        "body": json.dumps(value).encode()})

    def delete(self, candidates=None, *, authorized=True):
        candidates = candidates if candidates is not None else [candidate("data", "mapped")]
        coordination = {SOURCE: self.proof} if authorized and self.proof is not None else {}
        return purger.delete_candidates(self.transport, self.remote, BUCKET, candidates,
                                       self.registry, coordination=coordination)

    def source_versions(self):
        return [row for row in self.transport.versions if row["Key"].startswith(f"storage/{SOURCE}/")]

    def test_verified_b2_binding_retires_full_payload_and_delete_marker_history(self):
        self.transport.versions.clear()
        self.transport.add("data", "null")
        self.transport.add("data", "old-marker", marker=True)
        self.transport.add("data", "mapped")
        self.transport.add("retired", "retired-data")
        self.transport.add("retired", "retired-marker", marker=True)
        self.append_registry("registry-first")
        self.append_registry("registry-identical")
        copies = [row.copy() for row in self.transport.versions
                  if row["Key"].startswith(f"storage/2026/07/{SOURCE}/")]
        candidates = [candidate(row["source_object"][len(SOURCE) + 1:], row["source_version_id"])
                      for row in self.receipt["versions"]]
        result = self.delete(candidates)
        self.assertFalse(self.source_versions())
        self.assertEqual(result["retained_mapped"], [])
        self.assertEqual(result["retained_unmapped"], [])
        self.assertEqual(self.transport.counts["delete_object"], 5)
        self.assertEqual(copies, [row for row in self.transport.versions
                                 if row["Key"].startswith(f"storage/2026/07/{SOURCE}/")])
        self.assertEqual(self.binding_checks, 6)
        gets = [request for op, request in self.transport.calls if op == "get_object"]
        self.assertEqual({request["VersionId"] for request in gets}, {"registry-first", "registry-identical"})
        deletes = [request for op, request in self.transport.calls if op == "delete_object"]
        self.assertTrue(all(request["Key"].startswith(f"storage/{SOURCE}/") and request["VersionId"] for request in deletes))

    def test_unavailable_binding_blocks_b2_source_deletion(self):
        self.append_registry("registry-first")
        with self.assertRaisesRegex(RuntimeError, "atomic Git registry binding"):
            self.delete(authorized=False)
        self.claim = "another-writer"
        with self.assertRaisesRegex(RuntimeError, "removed or replaced"):
            self.delete()
        self.assertEqual(self.transport.counts["delete_object"], 0)
        self.assertEqual([row["VersionId"] for row in self.source_versions()], ["mapped"])

    def test_missing_or_corrupted_destination_history_blocks_every_original_delete(self):
        self.append_registry("registry-first")
        copied = next(row for row in self.transport.versions if row["VersionId"] == "copy-mapped")
        self.transport.versions.remove(copied)
        with self.assertRaisesRegex(RuntimeError, "lost a previously copied"):
            self.delete()
        self.transport.versions.append(copied)
        copied["ETag"] = '"corrupt-copy"'
        with self.assertRaisesRegex(RuntimeError, "missing or mismatched"):
            self.delete()
        self.assertEqual(self.transport.counts["delete_object"], 0)

    def test_mixed_generic_candidates_cannot_delete_later_generations(self):
        self.append_registry("registry-first")
        self.transport.add("data", "generic-later")
        generic_same = candidate("data", "mapped", pointer=SOURCE + "/data.dvc")
        generic_later = candidate("data", "generic-later", pointer=SOURCE + "/data.dvc")
        result = self.delete([candidate("data", "mapped"), generic_same, generic_later])
        self.assertEqual(result["retained_unmapped"], [candidate("data", "generic-later")])
        self.assertEqual(result["retained_mapped"], [])
        self.assertEqual(self.transport.counts["delete_object"], 1)
        self.assertEqual([row["VersionId"] for row in self.source_versions()], ["generic-later"])

    def test_generic_only_retirement_discovers_registry_and_requires_exact_coordinated_mapping(self):
        self.append_registry("registry-first")
        self.transport.add("data", "later-marker", marker=True)
        later = candidate("data", "later-marker", pointer=SOURCE + "/retired.dvc")
        with self.assertRaisesRegex(RuntimeError, "no coordinated exact mapping"):
            self.delete([later])
        self.assertEqual(self.transport.counts["delete_object"], 0)
        mapped = candidate("data", "mapped", pointer=SOURCE + "/retired.dvc")
        result = self.delete([mapped, later])
        self.assertEqual(result["retained_unmapped"], [candidate("data", "later-marker")])
        self.assertEqual([row["VersionId"] for row in self.source_versions()], ["later-marker"])

    def test_generic_only_mapped_version_cannot_bypass_unavailable_binding(self):
        self.append_registry("registry-first")
        mapped = candidate("data", "mapped", pointer=SOURCE + "/retired.dvc")
        with self.assertRaisesRegex(RuntimeError, "atomic Git registry binding"):
            self.delete([mapped], authorized=False)
        self.assertEqual(self.transport.counts["delete_object"], 0)

    def test_generic_nested_unmapped_keys_are_reported_without_any_cleanup(self):
        self.append_registry("registry-first")
        self.transport.add("nested/retired", "independent")
        generic = candidate("nested/retired", "independent", pointer=SOURCE + "/nested/retired.dvc")
        result = self.delete([generic])
        self.assertEqual(result["retained_unmapped"], [candidate("nested/retired", "independent")])
        prefixes = {request["Prefix"] for method, request in self.transport.calls if method == "list_object_versions"}
        self.assertEqual(prefixes, {
            self.registry_module.registry_key("storage", SOURCE + "/nested"), self.key,
            f"storage/{SOURCE}/nested/retired",
        })
        self.assertEqual(self.transport.counts["delete_object"], 0)

    def test_unplanned_payloads_and_markers_after_copy_remain_visible_and_not_completed(self):
        self.append_registry("registry-first")
        self.transport.add("new", "late-payload")
        self.transport.add("new", "late-marker", marker=True)
        result = self.delete()
        self.assertEqual(result["retained_unmapped"], [candidate("new", "late-marker"), candidate("new", "late-payload")])
        self.assertEqual(self.transport.counts["delete_object"], 1)
        self.assertEqual({row["VersionId"] for row in self.source_versions()}, {"late-payload", "late-marker"})

    def test_externally_removed_unmapped_pending_versions_allow_complete_retry(self):
        self.append_registry("registry-first")
        self.transport.add("new", "late-payload")
        self.transport.add("new", "late-marker", marker=True)
        first = self.delete()
        self.assertEqual(first["retained_unmapped"], [candidate("new", "late-marker"), candidate("new", "late-payload")])
        pending = [candidate("data", "mapped"), *first["retained_unmapped"]]
        with self.assertRaisesRegex(RuntimeError, "candidate is not mapped"):
            self.delete(pending)
        self.assertEqual(self.transport.counts["delete_object"], 1)
        # An independent owner resolves its added generations. A queued ID
        # that is now absent need not be invented into the immutable receipt.
        source_prefix = f"storage/{SOURCE}/"
        self.transport.versions = [row for row in self.transport.versions if not row["Key"].startswith(source_prefix)]
        retried = self.delete(pending)
        self.assertEqual(retried["deleted"], [])
        self.assertEqual(retried["retained_unmapped"], [])
        self.assertEqual(retried["retained_mapped"], [])
        self.assertTrue(retried["already_absent"])
        self.assertFalse(self.source_versions())
        self.assertEqual(self.transport.counts["delete_object"], 1)

    def test_claim_or_copy_loss_between_deletions_blocks_remaining_originals(self):
        self.transport.add("data", "second")
        self.append_registry("registry-first")
        def remove_claim(method, request, count):
            if method == "delete_object" and count == 1:
                self.claim = None
        self.transport.hook = remove_claim
        with self.assertRaisesRegex(RuntimeError, "removed or replaced"):
            self.delete([candidate("data", "mapped"), candidate("data", "second")])
        self.assertEqual(self.transport.counts["delete_object"], 1)
        self.assertEqual([row["VersionId"] for row in self.source_versions()], ["second"])

    def test_copy_loss_between_deletions_blocks_remaining_originals(self):
        self.transport.add("data", "second")
        self.append_registry("registry-first")
        def remove_copy(method, request, count):
            if method == "delete_object" and count == 1:
                self.transport.versions = [row for row in self.transport.versions if row["VersionId"] != "copy-second"]
        self.transport.hook = remove_copy
        with self.assertRaisesRegex(RuntimeError, "lost a previously copied"):
            self.delete([candidate("data", "mapped"), candidate("data", "second")])
        self.assertEqual(self.transport.counts["delete_object"], 1)
        self.assertEqual([row["VersionId"] for row in self.source_versions()], ["second"])

    def test_conflicting_append_after_verification_cannot_erase_originals_or_markers(self):
        self.transport.add("data", "source-marker", marker=True)
        self.append_registry("registry-first")
        def append_conflict(method, request, count):
            if method == "list_object_versions" and request["Prefix"] == f"storage/{SOURCE}/":
                self.transport.hook = None
                self.append_registry("registry-conflict", destination="other-copy")
        self.transport.hook = append_conflict
        with self.assertRaisesRegex(RuntimeError, "conflicting archive registry history"):
            self.delete([candidate("data", "mapped"), candidate("data", "source-marker")])
        self.assertEqual(self.transport.counts["delete_object"], 0)
        self.assertEqual({row["VersionId"] for row in self.source_versions()}, {"mapped", "source-marker"})

    def test_paginated_conflicting_registry_blocks_archive_and_generic_cleanup(self):
        self.append_registry("registry-first", destination="conflicting")
        self.append_registry("registry-newest")
        for candidates in ([candidate("data", "mapped")],
                           [candidate("data", "mapped", pointer=SOURCE + "/retired.dvc")]):
            with self.assertRaisesRegex(RuntimeError, "conflicting archive registry history"):
                self.delete(candidates)
        self.assertEqual(self.transport.counts["delete_object"], 0)

    def test_registry_delete_marker_blocks_source_cleanup_without_hiding_history(self):
        self.append_registry("registry-first")
        self.append_registry("registry-hidden", marker=True)
        with self.assertRaisesRegex(RuntimeError, "delete marker"):
            self.delete()
        self.assertEqual(self.transport.counts["delete_object"], 0)

    def test_b2_generic_retirement_without_any_archive_mapping_remains_supported(self):
        generic = candidate("data", "mapped", pointer=SOURCE + "/data.dvc")
        result = self.delete([generic])
        self.assertEqual(result["retained_mapped"], [])
        self.assertEqual(self.transport.counts["delete_object"], 1)
        self.assertEqual(self.transport.versions, [])

    def test_empty_snapshot_prefix_finds_late_payload_and_marker_and_finishes_only_when_empty(self):
        self.transport.versions = []
        self.append_registry("empty-registry")
        self.assertEqual(self.receipt["versions"], [])
        self.transport.add("late", "independent-payload")
        self.transport.add("late", "independent-marker", marker=True)
        result = purger.delete_candidates(
            self.transport, self.remote, BUCKET, [], self.registry,
            coordination={SOURCE: self.proof}, prefixes=[self.receipt])
        self.assertEqual(result["cleaned_prefixes"], [])
        self.assertEqual(result["retained_unmapped"], [
            candidate("late", "independent-marker"), candidate("late", "independent-payload")])
        self.assertEqual(self.transport.counts["delete_object"], 0)
        self.transport.versions = [row for row in self.transport.versions
                                   if not row["Key"].startswith(f"storage/{SOURCE}/")]
        retry = purger.delete_candidates(
            self.transport, self.remote, BUCKET, [], self.registry,
            coordination={SOURCE: self.proof}, prefixes=[self.receipt])
        self.assertEqual(retry["cleaned_prefixes"], [SOURCE])
        self.assertEqual(retry["retained_unmapped"], [])
        self.assertEqual(self.transport.counts["delete_object"], 0)

    def test_empty_prefix_receipt_cannot_replace_canonical_inventory(self):
        self.append_registry("registry")
        forged = copy.deepcopy(self.receipt)
        forged["versions"] = []
        with self.assertRaisesRegex(RuntimeError, "differs from its canonical"):
            purger.delete_candidates(self.transport, self.remote, BUCKET, [], self.registry,
                                    coordination={SOURCE: self.proof}, prefixes=[forged])
        self.assertEqual(self.transport.counts["delete_object"], 0)


if __name__ == "__main__":
    unittest.main()
