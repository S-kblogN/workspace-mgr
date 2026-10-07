from __future__ import annotations

import importlib.util
from dataclasses import replace
import io
import json
from pathlib import Path
import sys
import subprocess
import tempfile
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch


def load_asset(name):
    path = Path(__file__).parents[2] / "tests" / "oracles" / f"{name}.py"
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


class FakeB2S3:
    """Versioned registry; every operation stays in memory."""

    endpoint_url = "https://s3.fixture.backblazeb2.com"
    storage_args = ()

    def __init__(self, backend=None, config_kwargs=None, **options):
        self.backend = backend if backend is not None else {
            "history": [], "calls": [], "lock": threading.Lock(), "page_size": 1000,
        }
        self.config_kwargs = config_kwargs or {}
        self.storage_options = {"backend": self.backend, "config_kwargs": self.config_kwargs}
        self.client_kwargs = {}

    @property
    def calls(self):
        return self.backend["calls"]

    def append(self, key, body=None, marker=False):
        with self.backend["lock"]:
            version = "registry-" + str(len(self.backend["history"]) + 1)
            self.backend["history"].append({"Key": key, "VersionId": version,
                                             "body": body, "marker": marker})
            return version

    def call_s3(self, operation, **request):
        self.calls.append((operation, request.copy()))
        if operation == "put_object":
            if "IfNoneMatch" in request:
                if not self.backend.get("conditional_supported"):
                    error = ProviderError("NotImplemented")
                    error.response["Error"]["Header"] = self.backend.get("rejected_header", "If-None-Match")
                    raise error
                if any(item["Key"] == request["Key"] for item in self.backend["history"]):
                    raise ProviderError("PreconditionFailed")
            if self.config_kwargs.get("request_checksum_calculation") != "when_required":
                raise AssertionError("B2 writes must suppress SDK optional checksums")
            barrier = self.backend.get("before_put")
            if barrier is not None:
                barrier.wait(timeout=5)
            version = self.append(request["Key"], request["Body"])
            barrier = self.backend.get("after_put")
            if barrier is not None:
                barrier.wait(timeout=5)
            if self.backend.pop("lose_put_response", False):
                raise ConnectionError("response lost after append")
            return {"VersionId": version}
        if operation == "delete_object":
            if not request.get("VersionId"):
                raise AssertionError("registry cancellation may only delete an exact version")
            with self.backend["lock"]:
                self.backend["history"][:] = [item for item in self.backend["history"]
                                             if (item["Key"], item["VersionId"]) != (request["Key"], request["VersionId"])]
            if self.backend.pop("lose_delete_response", False):
                raise ConnectionError("response lost after exact deletion")
            return {}
        if operation == "get_object":
            if not request.get("VersionId"):
                raise AssertionError("B2 registry must read exact versions")
            item = next((item for item in self.backend["history"]
                         if (item["Key"], item["VersionId"]) == (request["Key"], request["VersionId"])), None)
            if item is None or item["marker"]:
                raise ProviderError("NoSuchVersion")
            return {"Body": io.BytesIO(item["body"]), "VersionId": item["VersionId"]}
        if operation == "list_object_versions":
            if self.backend.get("deny_listing"):
                raise PermissionError("denied version history")
            rows = sorted((item for item in self.backend["history"]
                           if item["Key"].startswith(request["Prefix"])),
                          key=lambda item: (item["Key"], item["VersionId"]))
            start = 0
            if request.get("KeyMarker"):
                identities = [(item["Key"], item["VersionId"]) for item in rows]
                start = identities.index((request["KeyMarker"], request["VersionIdMarker"])) + 1
            size = min(request["MaxKeys"], self.backend["page_size"])
            page = rows[start:start + size]
            result = {"Versions": [], "DeleteMarkers": [], "IsTruncated": start + size < len(rows)}
            for item in page:
                result["DeleteMarkers" if item["marker"] else "Versions"].append(
                    {"Key": item["Key"], "VersionId": item["VersionId"]})
            if result["IsTruncated"]:
                result.update(NextKeyMarker=page[-1]["Key"], NextVersionIdMarker=page[-1]["VersionId"])
            return result
        raise AssertionError(f"unexpected B2 operation {operation}")


def receipt(source="task", destination="2026/07/task", old="old", new="new",
            source_etag="original-etag", destination_etag="copied-etag", size=7):
    return {
        "schema_version": 1, "status": "copied", "source": source,
        "transaction_id": "fixture-transaction", "source_cleanup": "after_verified_git_publication",
        "destination": destination, "bucket": "bucket", "remote_prefix": "remote",
        "remote": "workspace-mgr", "versions": [{
            "source_object": f"{source}/data", "destination_object": f"{destination}/data",
            "source_version_id": old, "destination_version_id": new,
            "source_etag": source_etag, "destination_etag": destination_etag,
            "size": size, "delete_marker": False,
            "source_last_modified": "2026-07-01T12:00:00Z",
            "source_is_latest": True, "source_list_order": 0,
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

    def test_translated_provider_header_is_reported_without_unsafe_retry(self):
        cause = ProviderError("NotImplemented")
        cause.response["Error"]["Header"] = "If-None-Match"
        translated = OSError(78, "A header you provided implies functionality that is not implemented")
        translated.__cause__ = cause
        original = self.fs.call_s3

        def call(operation, **request):
            if operation == "put_object":
                self.fs.calls.append((operation, request))
                raise translated
            return original(operation, **request)

        self.fs.call_s3 = call
        with self.assertRaisesRegex(RuntimeError, "rejected If-None-Match"):
            self.publish()
        self.assertEqual(sum(op == "put_object" for op, _ in self.fs.calls), 1)
        self.assertFalse(self.fs.registry_objects)

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


class B2RegistryTests(unittest.TestCase):
    def setUp(self):
        self.fs = FakeB2S3()
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.claim = None
        self.claim_lock = threading.Lock()
        self.registry = registry_module.ArchiveRegistry(self.fs, "bucket", "remote",
                                                        coordination_check=self.check_binding)
        self.key = registry_module.registry_key("remote", "task")

    def check_binding(self, value, coordination):
        with self.claim_lock:
            if self.claim != coordination["receipt_sha256"]:
                raise RuntimeError("Git CAS binding was removed or replaced")

    def authorization(self, value=None):
        value = value or receipt()
        digest = registry_module.hashlib.sha256(registry_module.encoded_receipt(value)).hexdigest()
        with self.claim_lock:
            if self.claim is None:
                self.claim = digest
            if self.claim != digest:
                raise RuntimeError("conflicting Git CAS binding")
        journal = Path(self.directory.name) / (digest + ".json")
        registry_module.archive_helper().save_journal(journal, value)
        return {"mode": "git-cas", "receipt_sha256": digest,
                "transaction_id": value["transaction_id"], "state_path": str(journal),
                "remote": "origin", "ref": registry_module.coordination_ref(value), "oid": "a" * 40}

    def publish(self, value=None):
        value = value or receipt()
        return self.registry.publish(value, self.authorization(value))

    def test_append_is_idempotent_and_preserves_all_existing_versions(self):
        value = receipt()
        self.assertEqual(self.publish(value)["status"], "published")
        self.assertEqual(self.publish(value)["status"], "unchanged")
        self.fs.append(self.key, json.dumps(value, indent=2).encode())
        self.assertEqual(self.registry.read("task"), value)
        self.assertEqual(len(self.fs.backend["history"]), 2)
        self.assertEqual(sum(op == "put_object" for op, _ in self.fs.calls), 2)
        writes = [args for op, args in self.fs.calls if op == "put_object"]
        self.assertEqual(writes[0]["IfNoneMatch"], "*")
        self.assertNotIn("IfNoneMatch", writes[1])
        self.assertEqual(self.fs.config_kwargs, {})

    def test_lost_append_response_recovers_without_extra_versions(self):
        self.fs.backend["lose_put_response"] = True
        self.assertEqual(self.publish()["status"], "unchanged")
        self.assertEqual(len(self.fs.backend["history"]), 1)

    def test_full_paginated_history_conflicts_cannot_be_hidden_by_latest(self):
        self.fs.backend["page_size"] = 1
        self.fs.append(self.key, registry_module.encoded_receipt(receipt(new="competing")))
        self.fs.append(self.key, registry_module.encoded_receipt(receipt()))
        with self.assertRaisesRegex(RuntimeError, "conflicting archive registry history"):
            self.publish()
        self.assertEqual(len(self.fs.backend["history"]), 2)
        self.assertFalse(any(op == "put_object" for op, _ in self.fs.calls))
        self.assertTrue(any("KeyMarker" in args for op, args in self.fs.calls if op == "list_object_versions"))

    def test_delete_markers_are_not_treated_as_missing_or_removed(self):
        self.fs.append(self.key, registry_module.encoded_receipt(receipt()))
        self.fs.append(self.key, marker=True)
        with self.assertRaisesRegex(RuntimeError, "delete marker"):
            self.publish()
        self.assertEqual(len(self.fs.backend["history"]), 2)
        self.assertFalse(any(op in ("put_object", "delete_object") for op, _ in self.fs.calls))

    def test_listing_permission_failure_never_uses_latest_or_writes(self):
        self.fs.backend["deny_listing"] = True
        with self.assertRaises(PermissionError):
            self.publish()
        self.assertTrue(all(op == "list_object_versions" for op, _ in self.fs.calls))

    def concurrent_publish(self, values):
        start = threading.Barrier(2)
        outcomes = []

        def publish(value):
            try:
                start.wait(timeout=5)
                outcomes.append(self.publish(value))
            except Exception as error:
                outcomes.append(error)

        threads = [threading.Thread(target=publish, args=(value,)) for value in values]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(timeout=10)
            self.assertFalse(thread.is_alive())
        return outcomes

    def test_simultaneous_conflicting_writers_only_cas_winner_can_publish(self):
        outcomes = self.concurrent_publish([receipt(), receipt(new="competing")])
        self.assertEqual(len(outcomes), 2)
        self.assertEqual(sum(isinstance(value, RuntimeError) for value in outcomes), 1, outcomes)
        self.assertEqual(sum(isinstance(value, dict) for value in outcomes), 1, outcomes)
        self.assertEqual(len(self.fs.backend["history"]), 1)
        self.assertIn(self.registry.read("task"), [receipt(), receipt(new="competing")])

    def test_simultaneous_identical_writers_are_safe_with_complete_history(self):
        outcomes = self.concurrent_publish([receipt(), receipt()])
        self.assertTrue(all(isinstance(value, dict) for value in outcomes), outcomes)
        self.assertIn(len(self.fs.backend["history"]), (1, 2))
        self.assertEqual(self.registry.read("task"), receipt())

    def test_later_conflict_invalidates_a_previously_successful_publish(self):
        self.assertEqual(self.publish()["status"], "published")
        self.fs.append(self.key, registry_module.encoded_receipt(receipt(new="later")))
        with self.assertRaisesRegex(RuntimeError, "conflicting archive registry history"):
            self.registry.read("task")
        self.assertEqual(len(self.fs.backend["history"]), 2)

    def test_no_uncoordinated_b2_write_even_for_an_identical_existing_receipt(self):
        with self.assertRaisesRegex(RuntimeError, "Git CAS binding"):
            self.registry.publish(receipt())
        self.assertFalse(self.fs.calls)
        self.publish()
        with self.assertRaisesRegex(RuntimeError, "Git CAS binding"):
            self.registry.publish(receipt())

    def test_supported_b2_condition_is_preserved_without_plain_retry(self):
        self.fs.backend["conditional_supported"] = True
        self.assertEqual(self.publish()["status"], "published")
        writes = [args for op, args in self.fs.calls if op == "put_object"]
        self.assertEqual(len(writes), 1)
        self.assertEqual(writes[0]["IfNoneMatch"], "*")

    def test_provider_identified_different_rejected_header_never_retries_plain(self):
        self.fs.backend["rejected_header"] = "x-amz-sdk-checksum-algorithm"
        with self.assertRaisesRegex(RuntimeError, "checksum"):
            self.publish()
        self.assertFalse(self.fs.backend["history"])
        self.assertEqual(sum(op == "put_object" for op, _ in self.fs.calls), 1)

    def test_cancel_preview_and_exact_version_withdrawal_are_idempotent(self):
        self.publish()
        self.fs.append(self.key, registry_module.encoded_receipt(receipt()))
        auth = self.authorization()
        helper = registry_module.archive_helper()
        with patch.object(helper, "verify_cancel_source") as proof:
            preview = self.registry.cancel(receipt(), auth, preview=True)
            self.assertEqual(len(preview["delete_registry_versions"]), 2)
            self.assertFalse(any(op == "delete_object" for op, _ in self.fs.calls))
            self.fs.backend["lose_delete_response"] = True
            result = self.registry.cancel(receipt(), auth)
            self.assertEqual(len(result["deleted_registry_versions"] + result["already_absent"]), 2)
            self.assertEqual(self.registry.cancel(receipt(), auth)["deleted_registry_versions"], [])
            self.assertEqual(proof.call_count, 3)
        self.assertEqual(self.fs.backend["history"], [])
        self.assertTrue(all(args.get("VersionId") for op, args in self.fs.calls if op == "delete_object"))

    def test_cancel_refuses_missing_source_proof_or_foreign_history(self):
        self.publish()
        auth = self.authorization()
        helper = registry_module.archive_helper()
        with patch.object(helper, "verify_cancel_source", side_effect=RuntimeError("source incomplete")):
            with self.assertRaisesRegex(RuntimeError, "source incomplete"):
                self.registry.cancel(receipt(), auth)
        with patch.object(helper, "verify_cancel_source"):
            self.fs.append(self.key, registry_module.encoded_receipt(receipt(new="foreign")))
            with self.assertRaisesRegex(RuntimeError, "another transaction"):
                self.registry.cancel(receipt(), auth)
        self.assertFalse(any(op == "delete_object" for op, _ in self.fs.calls))

    def test_cancel_requires_owner_and_revalidates_claim_before_each_delete(self):
        self.publish()
        auth = self.authorization()
        auth["transaction_id"] = "foreign"
        with self.assertRaisesRegex(RuntimeError, "private copy transaction"):
            self.registry.cancel(receipt(), auth)
        auth = self.authorization()
        self.claim = None
        with self.assertRaisesRegex(RuntimeError, "removed or replaced"):
            self.registry.cancel(receipt(), auth)
        self.assertFalse(any(op == "delete_object" for op, _ in self.fs.calls))

    def test_partial_copy_may_confirm_absence_but_cannot_withdraw_existing_mapping(self):
        partial = {"source": "task", "versions": [{"destination_version_id": None}]}
        self.assertEqual(self.registry.cancel(partial, None)["status"], "no_registry")
        self.publish()
        with self.assertRaisesRegex(RuntimeError, "owning complete"):
            self.registry.cancel(partial, None)

    def test_unknown_alias_keeps_cas_and_resolved_b2_endpoint_is_recognized(self):
        for endpoint in ("https://b2.example.invalid", "https://backblazeb2.com.example.invalid"):
            fs = SimpleNamespace(endpoint_url=endpoint, client_kwargs={})
            self.assertFalse(registry_module.is_b2(fs))
        fs = SimpleNamespace(endpoint_url=None, client_kwargs={}, _s3=SimpleNamespace(
            meta=SimpleNamespace(endpoint_url="https://s3.fixture.backblazeb2.com")))
        self.assertTrue(registry_module.is_b2(fs))


class GitCoordinationTests(unittest.TestCase):
    """Only isolated local repositories arbitrate; no user remote or bucket."""

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        root = Path(self.directory.name)
        self.repo, self.remote = root / "checkout", root / "remote.git"
        self.repo.mkdir()
        self.run_git("init", "-q")
        self.run_git("config", "user.email", "fixture@example.invalid")
        self.run_git("config", "user.name", "Registry Fixture")
        subprocess.run(["git", "init", "--bare", "-q", str(self.remote)], check=True, capture_output=True)
        self.run_git("remote", "add", "origin", str(self.remote))
        self.value = receipt()
        body = registry_module.encoded_receipt(self.value)
        self.oid = self.run_git("hash-object", "-w", "--stdin", input=body).decode().strip()
        self.reference = registry_module.coordination_ref(self.value)
        self.run_git("push", "--force-with-lease=" + self.reference + ":", "origin", self.oid + ":" + self.reference)
        journal = self.repo / "copy-journal.json"
        journal.write_text(json.dumps(self.value))
        self.auth = {"mode": "git-cas", "receipt_sha256": registry_module.hashlib.sha256(body).hexdigest(),
                     "transaction_id": self.value["transaction_id"], "state_path": str(journal),
                     "remote": "origin", "ref": self.reference, "oid": self.oid}
        self.fs = FakeB2S3()
        self.registry = registry_module.ArchiveRegistry(self.fs, "bucket", "remote", repo_path=self.repo)

    def run_git(self, *args, input=None):
        result = subprocess.run(["git", *args], cwd=self.repo, input=input, capture_output=True, check=True)
        return result.stdout

    def test_fresh_binding_and_private_journal_allow_publish(self):
        self.assertEqual(self.registry.publish(self.value, self.auth)["status"], "published")
        self.assertEqual(self.registry.publish(self.value, self.auth)["status"], "unchanged")
        self.assertEqual(len(self.fs.backend["history"]), 1)

    def test_removed_or_replaced_remote_claim_blocks_even_existing_matching_receipt(self):
        self.registry.publish(self.value, self.auth)
        subprocess.run(["git", "--git-dir", str(self.remote), "update-ref", "-d", self.reference], check=True)
        with self.assertRaisesRegex(RuntimeError, "removed or replaced"):
            self.registry.publish(self.value, self.auth)
        self.assertEqual(len(self.fs.backend["history"]), 1)

    def test_complete_published_git_receipt_proves_purge_but_never_cancellation(self):
        path = "archive/task/.workspace-mgr-archive.json"
        target = self.repo / path
        target.parent.mkdir(parents=True)
        target.write_bytes(registry_module.encoded_receipt(self.value))
        self.run_git("add", path)
        self.run_git("commit", "-qm", "published receipt")
        revision = self.run_git("rev-parse", "HEAD").decode().strip()
        auth = {key: value for key, value in self.auth.items() if key not in ("state_path", "transaction_id")}
        auth.update(publication_oid=revision, receipt_path=path, base_branch="main")
        self.registry.verify_coordination(self.value, auth, allow_published=True)
        self.assertEqual(self.registry.publish(self.value, auth)["status"], "published")
        self.assertEqual(self.registry.publish(self.value, auth)["status"], "unchanged")
        with self.assertRaisesRegex(RuntimeError, "cannot withdraw"):
            self.registry.cancel(self.value, auth)
        target.write_text(json.dumps(receipt(new="foreign")))
        self.run_git("add", path)
        self.run_git("commit", "-qm", "foreign receipt")
        auth["publication_oid"] = self.run_git("rev-parse", "HEAD").decode().strip()
        with self.assertRaisesRegex(RuntimeError, "another receipt"):
            self.registry.verify_coordination(self.value, auth, allow_published=True)

    def test_copy_journal_state_changes_do_not_change_mapping_authority(self):
        journal = dict(self.value)
        journal["versions"] = [dict(self.value["versions"][0], started=True, cancel_started=True, cancel_deleted=True,
                                    cancel_owned_versions=["new", "sdk-retry-copy"])]
        journal["status"] = "canceling"
        Path(self.auth["state_path"]).write_text(json.dumps(journal))
        self.registry.verify_coordination(self.value, self.auth)
        with self.assertRaisesRegex(RuntimeError, "uncancelled copy journal"):
            self.registry.publish(self.value, self.auth)
        self.assertFalse(any(op == "put_object" for op, _ in self.fs.calls))
        journal["status"] = "copied"
        Path(self.auth["state_path"]).write_text(json.dumps(journal))
        with self.assertRaisesRegex(RuntimeError, "uncancelled copy journal"):
            self.registry.publish(self.value, self.auth)
        journal["versions"][0]["destination_version_id"] = "another"
        Path(self.auth["state_path"]).write_text(json.dumps(journal))
        with self.assertRaisesRegex(RuntimeError, "differs from its private"):
            self.registry.verify_coordination(self.value, self.auth)

    def test_orchestrator_review_fields_stay_in_binding_but_not_copy_journal(self):
        value = {**self.value, "task_id": "historical-task", "previous_receipt": "previous-path",
                 "completion_reviews": [{"number": 19, "state": "MERGED", "head_oid": "c" * 40}]}
        body = registry_module.encoded_receipt(value)
        oid = self.run_git("hash-object", "-w", "--stdin", input=body).decode().strip()
        self.run_git("push", "--force-with-lease=" + self.reference + ":" + self.oid,
                     "origin", oid + ":" + self.reference)
        auth = {**self.auth, "oid": oid, "receipt_sha256": registry_module.hashlib.sha256(body).hexdigest()}
        self.assertEqual(self.registry.publish(value, auth)["status"], "published")
        # The old owner cannot use the old mapping after the exact CAS ref
        # switched, even though its copy transaction still matches locally.
        with self.assertRaisesRegex(RuntimeError, "removed or replaced"):
            self.registry.publish(self.value, self.auth)


HAS_STORAGE_RUNTIME = importlib.util.find_spec("s3fs") is not None


@unittest.skipUnless(HAS_STORAGE_RUNTIME, "requires the pinned s3fs storage runtime")
class RegistryRequestTests(unittest.TestCase):
    def test_real_sdk_headers_disable_b2_checksums_and_retain_other_provider_cas(self):
        from aiobotocore.endpoint import AioEndpoint
        import s3fs

        class Captured(Exception):
            pass

        def headers(fs, **condition):
            captured = {}

            async def capture(endpoint, request):
                self.assertEqual(request.method, "PUT")
                captured.update({name.lower(): value for name, value in request.headers.items()})
                raise Captured("request captured before transport")

            # Patch the actual endpoint transport, including clients created
            # lazily for a bucket. A cached client's event hook alone can miss
            # those clients; this boundary prevents any DNS or socket access.
            with patch.object(AioEndpoint, "_send", capture):
                with self.assertRaises(Captured):
                    fs.call_s3("put_object", Bucket="fixture-bucket", Key="registry.json",
                               Body=b"{}", ContentType="application/json",
                               ContentMD5="mZFLkyvTelC5g8XnyQrpOw==", **condition)
            return captured

        raw = s3fs.S3FileSystem(endpoint_url="https://s3.fixture.backblazeb2.com",
                                key="mock-key", secret="mock-secret", version_aware=True,
                                client_kwargs={"region_name": "us-east-1"},
                                config_kwargs={"retries": {"max_attempts": 0}},
                                skip_instance_cache=True)
        baseline = headers(raw, IfNoneMatch="*")
        self.assertIn("x-amz-sdk-checksum-algorithm", baseline)
        writer = registry_module.registry_write_fs(raw)
        safe = headers(writer, IfNoneMatch="*")
        self.assertEqual(safe["if-none-match"], b"*")
        self.assertEqual(safe["content-md5"], b"mZFLkyvTelC5g8XnyQrpOw==")
        self.assertEqual(safe["content-length"], "2")
        self.assertNotIn("x-amz-sdk-checksum-algorithm", safe)
        self.assertNotIn("x-amz-trailer", safe)
        self.assertNotIn("transfer-encoding", safe)
        self.assertNotIn("request_checksum_calculation", raw.config_kwargs)
        ordinary = s3fs.S3FileSystem(endpoint_url="https://s3.fixture.example.invalid",
                                     key="mock-key", secret="mock-secret", version_aware=True,
                                     client_kwargs={"region_name": "us-east-1"},
                                     config_kwargs={"retries": {"max_attempts": 0}},
                                     skip_instance_cache=True)
        self.assertIs(registry_module.registry_write_fs(ordinary), ordinary)
        conditional = headers(ordinary, IfNoneMatch="*")
        self.assertEqual(conditional["if-none-match"], b"*")


if __name__ == "__main__":
    unittest.main()
