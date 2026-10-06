from __future__ import annotations

import importlib.util
from dataclasses import replace
import io
import json
from pathlib import Path
import sys
import unittest


def load_asset(name):
    path = Path(__file__).parents[2] / "assets" / f"{name}.py"
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


registry_module = load_asset("dvc_archive_registry")
verifier = load_asset("dvc_version_verify")


class ProviderError(Exception):
    def __init__(self, code):
        super().__init__(code)
        self.response = {"Error": {"Code": code}}


class FakeS3:
    def __init__(self):
        self.registry_objects = {}
        self.versions = {}
        self.calls = []
        self.lose_put_response = False

    def call_s3(self, operation, **request):
        self.calls.append((operation, request.copy()))
        key = request["Key"]
        if operation == "get_object":
            if key not in self.registry_objects:
                raise ProviderError("NoSuchKey")
            value = self.registry_objects[key]
            if isinstance(value, Exception):
                raise value
            return {"Body": io.BytesIO(value)}
        if operation == "put_object":
            if request.get("IfNoneMatch") != "*":
                raise AssertionError("registry writes must be conditional")
            if key in self.registry_objects:
                raise ProviderError("PreconditionFailed")
            self.registry_objects[key] = request["Body"]
            if self.lose_put_response:
                self.lose_put_response = False
                raise ConnectionError("response lost after accepted write")
            return {"VersionId": "registry-version"}
        if operation == "head_object":
            identity = (key, request["VersionId"])
            if identity not in self.versions:
                raise ProviderError("NoSuchVersion")
            value = self.versions[identity]
            if isinstance(value, Exception):
                raise value
            return value
        raise AssertionError(f"unexpected operation {operation}")


def receipt(source="task", destination="2026/07/task", old="old", new="new",
            source_etag="original-etag", destination_etag="copied-etag", size=7):
    return {
        "schema_version": 1, "status": "copied", "source": source,
        "destination": destination, "bucket": "bucket", "remote_prefix": "remote",
        "remote": "workspace-mgr", "versions": [{
            "source_object": f"{source}/data", "destination_object": f"{destination}/data",
            "source_version_id": old, "destination_version_id": new,
            "source_etag": source_etag, "destination_etag": destination_etag,
            "size": size, "delete_marker": False,
            "source_last_modified": "2026-07-01T12:00:00Z",
            "destination_last_modified": "2026-10-06T12:00:00Z",
        }],
    }


def entry():
    return verifier.Entry("task/data", "remote/task/data", "old", 7,
                          "original-etag", "9e107d9d372bb6826bd81d3542a419d6")


def head(version="new", etag="copied-etag", size=7):
    return {"VersionId": version, "ContentLength": size, "ETag": f'"{etag}"'}


class RegistryTests(unittest.TestCase):
    def setUp(self):
        self.fs = FakeS3()
        self.registry = registry_module.ArchiveRegistry(self.fs, "bucket", "remote")

    def publish(self, value=None):
        return self.registry.publish(receipt() if value is None else value)

    def test_publish_is_read_verified_and_idempotent(self):
        first = self.publish()
        self.assertEqual(first["status"], "published")
        self.assertTrue(first["registry_key"].startswith("remote/.workspace-mgr/archive/"))
        self.assertEqual(self.publish()["status"], "unchanged")
        self.assertEqual(sum(op == "put_object" for op, _ in self.fs.calls), 1)
        self.assertEqual(self.registry.read("task"), receipt())

    def test_lost_write_response_recovers_only_matching_published_receipt(self):
        self.fs.lose_put_response = True
        self.assertEqual(self.publish()["status"], "unchanged")
        self.assertEqual(self.publish()["status"], "unchanged")
        self.assertEqual(sum(op == "put_object" for op, _ in self.fs.calls), 1)

    def test_conflicting_receipt_is_never_overwritten(self):
        self.publish()
        other = receipt(new="different-version")
        with self.assertRaisesRegex(RuntimeError, "conflicting archive registry"):
            self.publish(other)
        self.assertEqual(self.registry.read("task"), receipt())
        self.assertEqual(sum(op == "put_object" for op, _ in self.fs.calls), 1)

    def test_lookup_is_read_only_and_finds_progressive_parent(self):
        value = receipt()
        value["versions"][0]["source_object"] = "task/nested/data"
        value["versions"][0]["destination_object"] = "2026/07/task/nested/data"
        self.publish(value)
        self.fs.calls.clear()
        mapping = self.registry.lookup("remote/task/nested/data", "old")
        self.assertEqual(mapping["destination_key"], "remote/2026/07/task/nested/data")
        self.assertEqual(mapping["destination_version_id"], "new")
        self.assertTrue(all(op == "get_object" for op, _ in self.fs.calls))
        self.assertEqual(len(self.fs.calls), 2)

    def test_lookup_does_not_guess_an_unrecorded_version(self):
        self.publish()
        self.assertIsNone(self.registry.lookup("remote/task/data", "unknown"))

    def test_repeated_lookups_cache_receipts_and_missing_parents_per_invocation(self):
        value = receipt()
        row = value["versions"][0]
        row["source_object"] = "task/nested/one"
        row["destination_object"] = "2026/07/task/nested/one"
        value["versions"].append({
            **row, "source_object": "task/nested/two",
            "destination_object": "2026/07/task/nested/two",
        })
        self.publish(value)
        self.fs.calls.clear()
        for name in ("one", "two", "one", "missing"):
            self.registry.lookup(f"remote/task/nested/{name}", "old")
        self.assertEqual([op for op, _ in self.fs.calls], ["get_object", "get_object"])

    def test_lookup_cache_never_hides_fresh_publish_reads(self):
        self.assertIsNone(self.registry.lookup("remote/task/data", "old"))
        self.publish()
        self.assertIsNotNone(self.registry.lookup("remote/task/data", "old"))
        different = receipt(new="other-version")
        key = registry_module.registry_key("remote", "task")
        self.fs.registry_objects[key] = json.dumps(different).encode()
        self.assertEqual(self.registry.read("task"), different)
        with self.assertRaisesRegex(RuntimeError, "conflicting archive registry"):
            self.publish()

    def test_read_errors_and_corrupt_registry_do_not_become_missing(self):
        key = registry_module.registry_key("remote", "task")
        for failure in (PermissionError("denied"), ConnectionError("offline")):
            self.fs.registry_objects[key] = failure
            with self.assertRaises(type(failure)):
                self.registry.read("task")
        self.fs.registry_objects[key] = b"not JSON"
        with self.assertRaisesRegex(RuntimeError, "invalid archive registry"):
            self.registry.read("task")

    def test_validation_rejects_escaping_and_conflicting_mappings(self):
        for field, value in (("destination_object", "elsewhere/data"),
                             ("source_object", "task/../outside"),
                             ("destination_version_id", "null")):
            bad = receipt()
            bad["versions"][0][field] = value
            with self.assertRaises(RuntimeError):
                self.publish(bad)
        bad = receipt()
        bad["versions"].append({**bad["versions"][0], "destination_version_id": "other"})
        with self.assertRaisesRegex(RuntimeError, "conflicting version mappings"):
            self.publish(bad)
        self.assertFalse(any(op == "put_object" for op, _ in self.fs.calls))

    def test_delete_marker_identity_is_retained_but_not_used_as_data(self):
        value = receipt()
        value["versions"][0].update(delete_marker=True, size=None, destination_etag=None)
        self.publish(value)
        self.assertEqual(self.registry.read("task"), value)
        with self.assertRaisesRegex(RuntimeError, "delete marker"):
            self.registry.lookup("remote/task/data", "old")

    def test_legacy_null_source_version_can_be_preserved_in_complete_history(self):
        self.publish(receipt(old="null"))
        self.assertEqual(
            self.registry.lookup("remote/task/data", "null")["destination_version_id"],
            "new",
        )

    def test_object_names_with_trailing_whitespace_are_preserved(self):
        value = receipt()
        value["versions"][0]["source_object"] += " "
        value["versions"][0]["destination_object"] += " "
        self.publish(value)
        self.assertTrue(self.registry.lookup("remote/task/data ", "old")["destination_key"].endswith(" "))

    def test_flat_folder_marker_payload_keys_are_preserved_exactly(self):
        value = receipt(size=0)
        value["versions"][0]["source_object"] = "task/double//folder/"
        value["versions"][0]["destination_object"] = "2026/07/task/double//folder/"
        self.publish(value)
        mapping = self.registry.lookup("remote/task/double//folder/", "old")
        self.assertEqual(mapping["destination_key"], "remote/2026/07/task/double//folder/")

    def planned(self):
        value = receipt()
        value["status"] = "planned"
        value["versions"][0].pop("destination_version_id")
        value["versions"][0].pop("destination_etag")
        return value

    def test_pending_local_archive_alias_preserves_pinned_content_and_cache(self):
        moved = replace(entry(), key="remote/2026/07/task/data", cache_path="same-cache")
        resolved = verifier.pending_archive_entries([moved], [self.planned()], "bucket", "remote")[0]
        self.assertEqual(resolved.key, "remote/task/data")
        self.assertEqual(resolved.version_id, moved.version_id)
        self.assertEqual(resolved.md5, moved.md5)
        self.assertEqual(resolved.size, moved.size)
        self.assertEqual(resolved.cache_path, moved.cache_path)

    def test_pending_alias_never_applies_to_copied_or_unmatched_versions(self):
        moved = replace(entry(), key="remote/2026/07/task/data")
        self.assertEqual(
            verifier.pending_archive_entries([moved], [receipt()], "bucket", "remote"),
            [moved],
        )
        other = replace(moved, version_id="another-version")
        self.assertEqual(
            verifier.pending_archive_entries([other], [self.planned()], "bucket", "remote"),
            [other],
        )

    def test_pending_alias_rejects_forged_location_or_integrity(self):
        moved = replace(entry(), key="remote/2026/07/task/data")
        for field, value in (("bucket", "another"), ("remote_prefix", "elsewhere"),
                             ("source", "../outside"), ("destination", "2026/07/other")):
            bad = self.planned()
            bad[field] = value
            with self.assertRaises(RuntimeError):
                verifier.pending_archive_entries([moved], [bad], "bucket", "remote")
        bad = self.planned()
        bad["versions"][0]["source_etag"] = "wrong-etag"
        with self.assertRaisesRegex(RuntimeError, "mismatched etag"):
            verifier.pending_archive_entries([moved], [bad], "bucket", "remote")
        bad = self.planned()
        bad["versions"][0]["size"] += 1
        with self.assertRaisesRegex(RuntimeError, "mismatched size"):
            verifier.pending_archive_entries([moved], [bad], "bucket", "remote")

    def test_missing_source_reads_verified_destination(self):
        self.publish()
        self.fs.versions[("remote/2026/07/task/data", "new")] = head()
        verifier.verify_entries(self.fs, "bucket", [entry()], registry=self.registry)
        resolved = verifier.archive_destination(entry(), self.registry, set())
        self.assertEqual(resolved.md5, entry().md5)
        self.assertEqual(resolved.size, entry().size)
        self.assertEqual(resolved.etag, "copied-etag")

    def test_present_source_is_tried_first_without_registry_reads(self):
        self.fs.versions[(entry().key, "old")] = head("old", "original-etag")
        verifier.verify_entries(self.fs, "bucket", [entry()], registry=self.registry)
        self.assertEqual([op for op, _ in self.fs.calls], ["head_object"])

    def test_chained_archives_resolve_exact_versions(self):
        self.publish()
        self.publish(receipt("2026/07/task", "2026/task", "new", "last",
                             "copied-etag", "last-etag"))
        self.fs.versions[("remote/2026/task/data", "last")] = head("last", "last-etag")
        verifier.verify_entries(self.fs, "bucket", [entry()], registry=self.registry)

    def test_cycle_is_bounded_without_guessing_a_destination(self):
        self.publish()
        self.publish(receipt("2026/07/task", "task", "new", "old",
                             "copied-etag", "original-etag"))
        with self.assertRaisesRegex(RuntimeError, "cyclic"):
            verifier.verify_entries(self.fs, "bucket", [entry()], registry=self.registry)
        self.assertLess(len(self.fs.calls), 20)

    def test_permission_and_network_failures_never_trigger_fallback(self):
        self.publish()
        for failure in (PermissionError("denied"), ConnectionError("offline")):
            self.fs.calls.clear()
            self.fs.versions[(entry().key, "old")] = failure
            with self.assertRaises(type(failure)):
                verifier.verify_entries(self.fs, "bucket", [entry()], registry=self.registry)
            self.assertEqual([op for op, _ in self.fs.calls], ["head_object"])

    def test_explicit_s3_error_code_outweighs_generic_file_not_found(self):
        for module in (registry_module, verifier):
            for code in ("AccessDenied", "NoSuchBucket", "MethodNotAllowed"):
                error = FileNotFoundError("provider translated the error")
                error.response = {"Error": {"Code": code}}
                self.assertFalse(module.missing_object(error))

    def test_source_integrity_mismatch_never_triggers_fallback(self):
        self.publish()
        self.fs.calls.clear()
        self.fs.versions[(entry().key, "old")] = head("old", "wrong-etag")
        with self.assertRaisesRegex(RuntimeError, "mismatched etag"):
            verifier.verify_entries(self.fs, "bucket", [entry()], registry=self.registry)
        self.assertEqual([op for op, _ in self.fs.calls], ["head_object"])

    def test_mapping_and_destination_integrity_are_checked(self):
        self.publish(receipt(size=8))
        with self.assertRaisesRegex(RuntimeError, "mismatched size"):
            verifier.verify_entries(self.fs, "bucket", [entry()], registry=self.registry)
        self.fs.registry_objects.clear()
        self.publish()
        self.fs.versions[("remote/2026/07/task/data", "new")] = head(size=8)
        with self.assertRaisesRegex(RuntimeError, "mismatched size"):
            verifier.verify_entries(self.fs, "bucket", [entry()], registry=self.registry)


if __name__ == "__main__":
    unittest.main()
