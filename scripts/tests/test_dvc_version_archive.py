"""Complete-history archive regressions with a private in-process S3 transport."""

from __future__ import annotations

from collections import Counter
from datetime import datetime, timedelta, timezone
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import sys
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock
from urllib.parse import parse_qsl


SCRIPT = Path(__file__).resolve().parents[2] / "assets" / "dvc_version_archive.py"
SPEC = importlib.util.spec_from_file_location("dvc_version_archive", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
adapter = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(adapter)

CONTEXT = {
    "schema_version": 1, "remote": "workspace-mgr", "bucket": "private-test-bucket",
    "remote_prefix": "storage", "source": "20260702-old-task", "destination": "2026/07/20260702-old-task",
}


class ProviderError(Exception):
    def __init__(self, code):
        super().__init__(code)
        self.response = {"Error": {"Code": code}}


class S3History:
    def __init__(self, page_size=1000):
        self.page_size = page_size
        self.versions = []
        self.calls = []
        self.counts = Counter()
        self.uploads = {}
        self.fail_before = {}
        self.lose_after = {}
        self.response_override = {}
        self.hook = None
        self.clock = datetime(2026, 7, 2, tzinfo=timezone.utc)

    def add(self, key, version, body=b"content", *, marker=False, size=None,
            metadata=None, tags=None, modified=None, **headers):
        self.clock += timedelta(seconds=1)
        for entry in self.versions:
            if entry["Key"] == key:
                entry["IsLatest"] = False
        result = {
            "Key": key, "VersionId": version, "IsLatest": True,
            "LastModified": modified or self.clock, "delete_marker": marker,
            "Size": len(body) if size is None else size,
            "ETag": '"' + hashlib.md5(body).hexdigest() + '"',
            "body": body, "Metadata": metadata or {}, "tags": tags or [],
            **headers,
        }
        self.versions.append(result)
        return result

    def source(self, name, version, **kwargs):
        return self.add(adapter.key_for(CONTEXT, CONTEXT["source"] + "/" + name), version, **kwargs)

    def matching(self, prefix):
        return [entry for entry in self.versions if entry["Key"].startswith(prefix)]

    def exact(self, key, version):
        entry = next((item for item in self.versions if item["Key"] == key and item["VersionId"] == version), None)
        if entry is None or entry["delete_marker"]:
            raise FileNotFoundError(f"{key}@{version}")
        return entry

    def listed(self, entry):
        return {key: entry[key] for key in ("Key", "VersionId", "IsLatest", "LastModified", "Size", "ETag")}

    def source_request(self, request):
        source = request["CopySource"]
        if source["Bucket"] != CONTEXT["bucket"] or not source.get("VersionId"):
            raise AssertionError("source copies must use an exact version in the configured bucket")
        item = self.exact(source["Key"], source["VersionId"])
        if request["CopySourceIfMatch"] != item["ETag"]:
            raise AssertionError("source copy must use its recorded ETag precondition")
        return item

    def call_s3(self, method, **request):
        self.calls.append((method, request.copy()))
        self.counts[method] += 1
        if self.hook is not None:
            self.hook(method, request, self.counts[method])
        if self.fail_before.get(method) == self.counts[method]:
            raise ConnectionError("fixture fails before mutation")
        response = self._request(method, request)
        if self.lose_after.get(method) == self.counts[method]:
            raise ConnectionError("fixture loses response after mutation")
        response.update(self.response_override.get(method, {}))
        return response

    def _request(self, method, request):
        if request["Bucket"] != CONTEXT["bucket"]:
            raise AssertionError("request escaped the fixture bucket")
        if method == "list_object_versions":
            entries = sorted(self.matching(request["Prefix"]), key=lambda item: (
                item["Key"], -item["LastModified"].timestamp(), -self.versions.index(item),
            ))
            start = 0
            if request.get("KeyMarker"):
                identity = (request["KeyMarker"], request.get("VersionIdMarker"))
                start = next(index + 1 for index, item in enumerate(entries)
                             if (item["Key"], item["VersionId"]) == identity)
            limit = min(request["MaxKeys"], self.page_size)
            page = entries[start:start + limit]
            result = {"Versions": [], "DeleteMarkers": [], "IsTruncated": start + len(page) < len(entries)}
            for item in page:
                result["DeleteMarkers" if item["delete_marker"] else "Versions"].append(self.listed(item))
            if result["IsTruncated"]:
                result["NextKeyMarker"], result["NextVersionIdMarker"] = page[-1]["Key"], page[-1]["VersionId"]
            return result
        if method == "list_multipart_uploads":
            entries = sorted([{"Key": item["request"]["Key"], "UploadId": upload}
                              for upload, item in self.uploads.items()
                              if item.get("request", {}).get("Key", "").startswith(request["Prefix"])],
                             key=lambda item: (item["Key"], item["UploadId"]))
            start = 0
            if request.get("KeyMarker"):
                identity = (request["KeyMarker"], request.get("UploadIdMarker"))
                start = next(index + 1 for index, item in enumerate(entries)
                             if (item["Key"], item["UploadId"]) == identity)
            page = entries[start:start + min(request["MaxUploads"], self.page_size)]
            result = {"Uploads": page, "IsTruncated": start + len(page) < len(entries)}
            if result["IsTruncated"]:
                result["NextKeyMarker"], result["NextUploadIdMarker"] = page[-1]["Key"], page[-1]["UploadId"]
            return result
        if method == "head_object":
            if not request.get("VersionId"):
                raise AssertionError("HEAD must specify an exact version")
            item = self.exact(request["Key"], request["VersionId"])
            return {"VersionId": item["VersionId"], "ContentLength": item["Size"],
                    "ETag": item["ETag"], "Metadata": item["Metadata"],
                    **{field: item[field] for field in adapter.COPY_HEADERS if field in item}}
        if method == "copy_object":
            source = self.source_request(request)
            if request["MetadataDirective"] != "REPLACE" or request["TaggingDirective"] != "COPY":
                raise AssertionError("copy must preserve metadata and tags")
            copied = self.add(request["Key"], f"copied-{len(self.versions)}", body=source["body"],
                              size=source["Size"], metadata=request["Metadata"], tags=source["tags"],
                              **{field: request[field] for field in adapter.COPY_HEADERS if field in request})
            return {"VersionId": copied["VersionId"], "CopyObjectResult": {
                "ETag": copied["ETag"], "LastModified": copied["LastModified"],
            }}
        if method == "delete_object":
            if "VersionId" in request:
                if not request["Key"].startswith(adapter.key_for(CONTEXT, CONTEXT["destination"] + "/")):
                    raise AssertionError("cancellation must never permanently delete a source version")
                self.versions = [item for item in self.versions
                                 if (item["Key"], item["VersionId"]) != (request["Key"], request["VersionId"])]
                remaining = [item for item in self.versions if item["Key"] == request["Key"]]
                if remaining:
                    max(remaining, key=lambda item: item["LastModified"])["IsLatest"] = True
                return {"VersionId": request["VersionId"]}
            deleted = self.add(request["Key"], f"marker-{len(self.versions)}", marker=True)
            return {"VersionId": deleted["VersionId"], "DeleteMarker": True}
        if method == "get_object_tagging":
            return {"TagSet": self.exact(request["Key"], request["VersionId"])["tags"]}
        if method == "create_multipart_upload":
            upload_id = f"upload-{self.counts[method]}"
            self.uploads[upload_id] = {"request": request, "parts": [], "source": None}
            return {"UploadId": upload_id}
        if method == "upload_part_copy":
            source = self.source_request(request)
            upload = self.uploads[request["UploadId"]]
            upload["source"] = source
            first, last = map(int, request["CopySourceRange"].removeprefix("bytes=").split("-"))
            if first != sum(upload["parts"]) or last >= source["Size"]:
                raise AssertionError("multipart ranges must cover the exact source without gaps")
            upload["parts"].append(last - first + 1)
            return {"CopyPartResult": {"ETag": f'"part-{request["PartNumber"]}"'}}
        if method == "complete_multipart_upload":
            upload = self.uploads.pop(request["UploadId"])
            source, started = upload["source"], upload["request"]
            if sum(upload["parts"]) != source["Size"]:
                raise AssertionError("multipart copy has wrong size")
            copied = self.add(request["Key"], f"copied-{len(self.versions)}", body=source["body"],
                              size=source["Size"], metadata=started["Metadata"],
                              tags=[{"Key": key, "Value": value} for key, value in parse_qsl(started.get("Tagging", ""))],
                              **{field: started[field] for field in adapter.COPY_HEADERS if field in started})
            copied["ETag"] = '"different-multipart-etag"'
            return {"VersionId": copied["VersionId"], "ETag": copied["ETag"]}
        if method == "abort_multipart_upload":
            if request["UploadId"] not in self.uploads:
                raise ProviderError("NoSuchUpload")
            self.uploads.pop(request["UploadId"])
            return {}
        raise AssertionError(f"unexpected request {method!r}")


class ArchiveHistoryTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.journal = str(Path(self.temporary.name) / "private" / "archive.json")
        self.remote = S3History()
        self.payload = {"source": CONTEXT["source"], "destination": CONTEXT["destination"], "state_path": self.journal}

    def run_archive(self, operation="copy", **payload):
        return adapter.archive(self.remote, CONTEXT, operation, {**self.payload, **payload})

    def source_snapshot(self):
        return json.dumps(self.remote.matching(adapter.key_for(CONTEXT, CONTEXT["source"] + "/")),
                          default=str, sort_keys=True)

    def assert_no_source_writes(self):
        writes = {"copy_object", "delete_object", "create_multipart_upload", "upload_part_copy", "complete_multipart_upload", "abort_multipart_upload"}
        destination = adapter.key_for(CONTEXT, CONTEXT["destination"] + "/")
        for method, request in self.remote.calls:
            if method in writes:
                self.assertTrue(request["Key"].startswith(destination), method)

    def test_cancel_preview_and_retry_remove_only_copied_history_and_start_a_fresh_attempt(self):
        self.remote.page_size = 1
        self.remote.source("file", "null", body=b"old")
        self.remote.source("file", "deleted", marker=True)
        self.remote.source("file", "new", body=b"new")
        self.remote.source("retired", "retired", marker=True)
        self.run_archive()
        snapshot = json.dumps(self.remote.versions, sort_keys=True, default=str)
        original = self.source_snapshot()
        transaction = adapter.load_journal(self.journal)["transaction_id"]
        preview = self.run_archive("cancel-preview")
        self.assertEqual(len(preview["delete_versions"]), 4)
        self.assertEqual(preview["retained_versions"], [])
        self.assertEqual(snapshot, json.dumps(self.remote.versions, sort_keys=True, default=str))
        result = self.run_archive("cancel")
        self.assertEqual(result["status"], "cancelled")
        self.assertEqual(len(result["deleted_versions"]), 4)
        self.assertEqual(original, self.source_snapshot())
        self.assertFalse(adapter.destination_inventory(self.remote, CONTEXT))
        self.run_archive("cancel")
        self.assertEqual(self.remote.counts["delete_object"], 6)  # Two copied markers plus four exact deletions.
        receipt = self.run_archive()
        self.assertNotEqual(receipt["transaction_id"], transaction)
        self.assertEqual(len(receipt["versions"]), 4)
        self.assertEqual(original, self.source_snapshot())
        self.assert_no_source_writes()

    def test_b2_spaces_same_key_versions_and_delete_markers_without_delaying_other_keys(self):
        self.remote.endpoint_url = "https://s3.us-west-004.backblazeb2.com"
        self.remote.source("file", "old", body=b"old")
        self.remote.source("file", "deleted", marker=True)
        self.remote.source("file", "new", body=b"new")
        self.remote.source("other", "independent", body=b"other")
        with mock.patch.object(adapter.time, "sleep") as sleep:
            result = self.run_archive()
        self.assertEqual(sleep.call_args_list, [mock.call(adapter.B2_VERSION_INTERVAL)] * 2)
        self.assertEqual(len(result["versions"]), 4)
        self.assertEqual(result["status"], "copied")

    def test_cancel_partial_copy_removes_owned_versions_and_reports_unrelated_history(self):
        self.remote.source("a", "source-a", body=b"a")
        self.remote.source("b", "source-b", body=b"b")
        self.remote.fail_before["copy_object"] = 2
        with self.assertRaises(ConnectionError):
            self.run_archive()
        self.remote.add(adapter.key_for(CONTEXT, CONTEXT["destination"] + "/other"), "foreign", marker=True)
        source = self.source_snapshot()
        result = self.run_archive("cancel")
        self.assertEqual(result["status"], "cancelled_with_unrelated_history")
        self.assertEqual(len(result["deleted_versions"]), 1)
        self.assertEqual(result["retained_versions"], [{"key": adapter.key_for(CONTEXT, CONTEXT["destination"] + "/other"),
                                                       "version_id": "foreign", "delete_marker": True}])
        self.assertEqual(source, self.source_snapshot())

    def test_cancel_aborts_only_its_durable_multipart_upload(self):
        self.remote.source("large", "large-source", size=adapter.COPY_LIMIT + 1)
        plan = self.run_archive("plan")
        journal = {**plan, "status": "copying", "transaction_id": "test-owned"}
        row = journal["versions"][0]
        row["started"] = True
        row["multipart_upload_id"] = "owned"
        self.remote.uploads = {
            "owned": {"request": {"Key": adapter.key_for(CONTEXT, row["destination_object"])}},
            "other-task": {"request": {"Key": "storage/another-task/large"}},
        }
        adapter.save_journal(self.journal, journal)
        snapshot = self.source_snapshot()
        self.run_archive("cancel-preview")
        self.assertEqual(set(self.remote.uploads), {"owned", "other-task"})
        self.run_archive("cancel")
        self.assertEqual(set(self.remote.uploads), {"other-task"})
        self.run_archive("cancel")
        self.assertEqual(self.remote.counts["abort_multipart_upload"], 1)
        self.assertEqual(snapshot, self.source_snapshot())

    def test_cancel_recovers_a_lost_abort_response_translated_by_s3fs(self):
        self.remote.source("large", "large-source", size=adapter.COPY_LIMIT + 1)
        journal = {**self.run_archive("plan"), "status": "copying", "transaction_id": "owned"}
        journal["versions"][0].update(started=True, multipart_upload_id="owned")
        adapter.save_journal(self.journal, journal)
        self.remote.uploads["owned"] = {"request": {"Key": adapter.key_for(CONTEXT, CONTEXT["destination"] + "/large")}}
        self.remote.lose_after["abort_multipart_upload"] = 1
        with self.assertRaises(ConnectionError):
            self.run_archive("cancel")
        original = self.remote.call_s3

        def translated(method, **request):
            try:
                return original(method, **request)
            except ProviderError as error:
                raise FileNotFoundError("translated by s3fs") from error

        with mock.patch.object(self.remote, "call_s3", side_effect=translated):
            self.run_archive("cancel")
        self.assertFalse(self.remote.uploads)
        self.assertNotIn("multipart_upload_id", adapter.load_journal(self.journal)["versions"][0])

    def test_cancelled_copy_does_not_reuse_obsolete_source_inventory(self):
        self.remote.source("file", "v1")
        self.run_archive()
        self.run_archive("cancel")
        self.remote.source("file", "v2")
        planned = {**CONTEXT, "status": "planned", "versions": adapter.source_inventory(self.remote, CONTEXT)}
        receipt = self.run_archive(planned=planned)
        self.assertEqual([row["source_version_id"] for row in receipt["versions"]], ["v1", "v2"])

    def test_cancel_recovers_a_lost_payload_copy_response_by_exact_transaction_token(self):
        self.remote.source("data", "source")
        self.remote.lose_after["copy_object"] = 1
        with self.assertRaises(ConnectionError):
            self.run_archive()
        result = self.run_archive("cancel")
        self.assertEqual(result["status"], "cancelled")
        self.assertEqual(len(result["deleted_versions"]), 1)
        self.assertFalse(adapter.destination_inventory(self.remote, CONTEXT))
        self.assert_no_source_writes()

    def test_cancel_removes_all_sdk_retry_versions_without_changing_receipt_bindings(self):
        self.remote.source("data", "source")
        receipt = self.run_archive()
        row = receipt["versions"][0]
        key = adapter.key_for(CONTEXT, row["destination_object"])
        copied = self.remote.exact(key, row["destination_version_id"])
        for version in ("sdk-retry-1", "sdk-retry-2"):
            self.remote.add(key, version, body=copied["body"], metadata=copied["Metadata"])
        self.remote.add(key, "another-attempt", body=copied["body"], metadata={"foreign": "token"})
        original = self.source_snapshot()
        preview = self.run_archive("cancel-preview")
        self.assertEqual({item["version_id"] for item in preview["delete_versions"]},
                         {row["destination_version_id"], "sdk-retry-1", "sdk-retry-2"})
        self.assertNotIn("cancel_owned_versions", adapter.load_journal(self.journal)["versions"][0])
        result = self.run_archive("cancel")
        self.assertEqual(result["status"], "cancelled_with_unrelated_history")
        self.assertEqual([item["version_id"] for item in result["retained_versions"]], ["another-attempt"])
        journal = adapter.load_journal(self.journal)
        self.assertEqual(journal["versions"][0]["destination_version_id"], row["destination_version_id"])
        self.assertEqual(adapter.public_receipt(journal, "copied"), receipt)
        self.assertEqual(len(journal["versions"][0]["cancel_owned_versions"]), 3)
        self.assertEqual(original, self.source_snapshot())
        self.assertEqual(self.remote.counts["delete_object"], 3)

    def test_cancel_recovers_multiple_unrecorded_payloads_after_lost_copy_responses(self):
        self.remote.source("data", "source")
        self.remote.lose_after["copy_object"] = 1
        with self.assertRaises(ConnectionError):
            self.run_archive()
        copied = adapter.destination_inventory(self.remote, CONTEXT)[0]
        payload = self.remote.exact(copied["Key"], copied["VersionId"])
        self.remote.add(copied["Key"], "sdk-response-loss-retry", body=payload["body"], metadata=payload["Metadata"])
        with self.assertRaisesRegex(RuntimeError, "unrelated or ambiguous"):
            self.run_archive()
        public_before = adapter.public_receipt(adapter.load_journal(self.journal))
        self.assertEqual(len(self.run_archive("cancel-preview")["delete_versions"]), 2)
        result = self.run_archive("cancel")
        self.assertEqual(result["status"], "cancelled")
        self.assertEqual(len(result["deleted_versions"]), 2)
        journal = adapter.load_journal(self.journal)
        self.assertIsNone(journal["versions"][0]["destination_version_id"])
        self.assertEqual(adapter.public_receipt(journal, "copying"), public_before)
        self.assertEqual(len(journal["versions"][0]["cancel_owned_versions"]), 2)
        self.assertFalse(adapter.destination_inventory(self.remote, CONTEXT))

    def test_cancel_duplicate_delete_response_loss_recovers_each_exact_version(self):
        self.remote.source("data", "source")
        receipt = self.run_archive()
        row = receipt["versions"][0]
        key = adapter.key_for(CONTEXT, row["destination_object"])
        copied = self.remote.exact(key, row["destination_version_id"])
        for version in ("sdk-retry-1", "sdk-retry-2"):
            self.remote.add(key, version, body=copied["body"], metadata=copied["Metadata"])
        self.remote.lose_after["delete_object"] = 2
        with self.assertRaises(ConnectionError):
            self.run_archive("cancel")
        journal = adapter.load_journal(self.journal)
        self.assertEqual(journal["status"], "canceling")
        self.assertEqual(len(journal["versions"][0]["cancel_owned_versions"]), 3)
        self.assertEqual(self.run_archive("cancel")["status"], "cancelled")
        self.assertEqual(self.remote.counts["delete_object"], 3)
        self.assertFalse(adapter.destination_inventory(self.remote, CONTEXT))
        journal = adapter.load_journal(self.journal)
        self.assertTrue(all(record["started"] and record["deleted"]
                            for record in journal["versions"][0]["cancel_owned_versions"]))
        self.assertEqual(adapter.public_receipt(journal, "copied"), receipt)
        self.run_archive("cancel")
        self.assertEqual(self.remote.counts["delete_object"], 3)

    def test_cancel_refuses_mismatched_token_copy_before_deleting_recorded_versions(self):
        self.remote.source("data", "source")
        receipt = self.run_archive()
        row = receipt["versions"][0]
        key = adapter.key_for(CONTEXT, row["destination_object"])
        copied = self.remote.exact(key, row["destination_version_id"])
        self.remote.add(key, "malformed-retry", body=b"wrong size", metadata=copied["Metadata"])
        with self.assertRaisesRegex(RuntimeError, "mismatched copied version"):
            self.run_archive("cancel")
        self.assertEqual(self.remote.counts["delete_object"], 0)

    def test_plan_and_fresh_copy_refuse_existing_multipart_uploads_before_mutation(self):
        self.remote.source("data", "source")
        self.remote.uploads["foreign-upload"] = {"request": {
            "Key": adapter.key_for(CONTEXT, CONTEXT["destination"] + "/data")}}
        for operation in ("plan", "copy"):
            with self.subTest(operation=operation):
                with self.assertRaisesRegex(RuntimeError, "multipart uploads"):
                    self.run_archive(operation)
        self.assertFalse(Path(self.journal).exists())
        self.assertEqual(self.remote.counts["copy_object"], 0)
        self.assertEqual(self.remote.counts["abort_multipart_upload"], 0)
        self.assertEqual(set(self.remote.uploads), {"foreign-upload"})

    def test_cancel_refuses_unrecorded_delete_marker_and_mutates_nothing(self):
        self.remote.source("gone", "source-marker", marker=True)
        self.remote.lose_after["delete_object"] = 1
        with self.assertRaises(ConnectionError):
            self.run_archive()
        snapshot = json.dumps(self.remote.versions, sort_keys=True, default=str)
        for operation in ("cancel-preview", "cancel"):
            with self.assertRaisesRegex(RuntimeError, "unrecorded delete marker"):
                self.run_archive(operation)
        self.assertEqual(snapshot, json.dumps(self.remote.versions, sort_keys=True, default=str))
        self.assertEqual(self.remote.counts["delete_object"], 1)

    def test_cancel_refuses_missing_original_source_history_before_deletion(self):
        self.remote.source("data", "source")
        self.run_archive()
        self.remote.source("data", "new-source")
        self.remote.versions = [item for item in self.remote.versions if item["VersionId"] != "source"]
        with self.assertRaisesRegex(RuntimeError, "every unchanged original"):
            self.run_archive("cancel")
        self.assertEqual(self.remote.counts["delete_object"], 0)

    def test_cancel_preserves_additional_source_generations_and_independent_keys(self):
        self.remote.source("data", "source")
        self.remote.source("retired", "original-marker", marker=True)
        self.run_archive()
        self.remote.source("data", "new-source", body=b"new")
        self.remote.source("new-key", "independent", body=b"independent")
        original = self.source_snapshot()
        self.assertEqual(self.run_archive("cancel")["status"], "cancelled")
        self.assertEqual(original, self.source_snapshot())
        self.assert_no_source_writes()

    def test_cancel_source_proof_rejects_missing_markers_and_changed_immutable_metadata(self):
        self.remote.source("data", "source")
        self.remote.source("retired", "source-marker", marker=True)
        receipt = self.run_archive()
        before = self.remote.counts["delete_object"]
        marker = next(item for item in self.remote.versions if item["VersionId"] == "source-marker")
        self.remote.versions.remove(marker)
        with self.assertRaisesRegex(RuntimeError, "every unchanged original"):
            adapter.verify_cancel_source(self.remote, CONTEXT, receipt)
        self.remote.versions.append(marker)
        source = next(item for item in self.remote.versions if item["VersionId"] == "source")
        source["ETag"] = '"independent-corruption"'
        with self.assertRaisesRegex(RuntimeError, "every unchanged original"):
            self.run_archive("cancel")
        self.assertEqual(self.remote.counts["delete_object"], before)

    def test_cancel_upload_scan_failure_prevents_every_destructive_request(self):
        self.remote.source("data", "source")
        self.run_archive()
        self.remote.response_override["list_multipart_uploads"] = {"IsTruncated": True}
        with self.assertRaisesRegex(RuntimeError, "pagination markers"):
            self.run_archive("cancel")
        self.assertEqual(self.remote.counts["delete_object"], 0)
        self.assertEqual(self.remote.counts["abort_multipart_upload"], 0)
        self.assertEqual(adapter.load_journal(self.journal)["status"], "copied")

    def test_cancel_refuses_a_forged_payload_binding_before_deletion(self):
        self.remote.source("data", "source")
        receipt = self.run_archive()
        copied = self.remote.exact(adapter.key_for(CONTEXT, receipt["versions"][0]["destination_object"]),
                                   receipt["versions"][0]["destination_version_id"])
        copied["Metadata"] = {"other-attempt": "token"}
        with self.assertRaisesRegex(RuntimeError, "ownership"):
            self.run_archive("cancel")
        self.assertEqual(self.remote.counts["delete_object"], 0)

    def test_cancel_does_not_delete_a_concurrent_version_of_the_same_destination_key(self):
        self.remote.source("data", "source")
        receipt = self.run_archive()
        key = adapter.key_for(CONTEXT, receipt["versions"][0]["destination_object"])
        self.remote.add(key, "other-attempt", body=b"other")
        result = self.run_archive("cancel")
        self.assertEqual(result["status"], "cancelled_with_unrelated_history")
        self.assertEqual(self.remote.exact(key, "other-attempt")["body"], b"other")
        self.assertEqual([item["version_id"] for item in result["retained_versions"]], ["other-attempt"])

    def test_cancel_delete_response_loss_remains_resumable_and_never_recopies(self):
        self.remote.source("a", "source-a")
        self.remote.source("b", "source-b")
        self.run_archive()
        self.remote.lose_after["delete_object"] = 1
        with self.assertRaises(ConnectionError):
            self.run_archive("cancel")
        self.assertEqual(adapter.load_journal(self.journal)["status"], "canceling")
        with self.assertRaisesRegex(RuntimeError, "finish cancel"):
            self.run_archive()
        result = self.run_archive("cancel")
        self.assertEqual(result["status"], "cancelled")
        self.assertFalse(adapter.destination_inventory(self.remote, CONTEXT))
        self.assertEqual(self.remote.counts["copy_object"], 2)
        self.assertEqual(self.remote.counts["delete_object"], 2)

    def test_cancel_reports_a_lost_unjournaled_multipart_create_instead_of_claiming_clean(self):
        self.remote.source("large", "source-large", size=adapter.COPY_LIMIT + 1)
        self.remote.lose_after["create_multipart_upload"] = 1
        with self.assertRaises(ConnectionError):
            self.run_archive()
        result = self.run_archive("cancel")
        self.assertEqual(result["status"], "cancelled_with_unrelated_history")
        self.assertEqual(result["retained_uploads"], [{"key": adapter.key_for(CONTEXT, CONTEXT["destination"] + "/large"),
                                                      "upload_id": "upload-1"}])
        self.assertEqual(self.remote.counts["abort_multipart_upload"], 0)
        self.assertEqual(adapter.load_journal(self.journal)["status"], "canceling")
        with self.assertRaisesRegex(RuntimeError, "finish cancel"):
            self.run_archive()

    def test_cancel_completely_scans_multipart_uploads_and_preserves_foreign_attempts(self):
        self.remote.source("data", "source")
        self.run_archive()
        self.remote.page_size = 1
        for index in range(3):
            self.remote.uploads[str(index)] = {"request": {"Key": adapter.key_for(CONTEXT, CONTEXT["destination"] + "/data")}}
        before = len(self.remote.calls)
        preview = self.run_archive("cancel-preview")
        self.assertEqual(len(preview["retained_uploads"]), 3)
        requests = [request for method, request in self.remote.calls[before:] if method == "list_multipart_uploads"]
        self.assertEqual(len(requests), 3)
        self.assertTrue(all(request.get("UploadIdMarker") for request in requests[1:]))
        result = self.run_archive("cancel")
        self.assertEqual(result["status"], "cancelled_with_unrelated_history")
        self.assertEqual(set(self.remote.uploads), {"0", "1", "2"})

    def test_completed_cancel_is_idempotent_after_the_original_task_publishes_new_work(self):
        self.remote.source("data", "old-source")
        self.run_archive()
        self.run_archive("cancel")
        self.remote.source("data", "new-source")
        self.assertEqual(self.run_archive("cancel")["status"], "cancelled")
        self.assertEqual(self.remote.counts["delete_object"], 1)

    def test_completed_cancel_retries_after_source_retirement_without_reads_or_mutation(self):
        self.remote.source("data", "original")
        self.run_archive()
        self.run_archive("cancel")
        journal = Path(self.journal).read_bytes()
        source = adapter.key_for(CONTEXT, CONTEXT["source"] + "/")
        self.remote.versions = [item for item in self.remote.versions if not item["Key"].startswith(source)]
        before = len(self.remote.calls)
        for operation in ("cancel-preview", "cancel"):
            result = self.run_archive(operation)
            self.assertEqual(result["status"], "cancelled")
            self.assertTrue(result["already_cancelled"])
        self.assertEqual(Path(self.journal).read_bytes(), journal)
        calls = self.remote.calls[before:]
        self.assertTrue(all(method in ("list_object_versions", "list_multipart_uploads") for method, _ in calls))
        self.assertTrue(all(not request.get("Prefix", "").startswith(source) for _, request in calls))

    def test_completed_cancel_preserves_foreign_history_after_source_retirement(self):
        self.remote.source("data", "original")
        self.run_archive()
        self.run_archive("cancel")
        source = adapter.key_for(CONTEXT, CONTEXT["source"] + "/")
        self.remote.versions = [item for item in self.remote.versions if not item["Key"].startswith(source)]
        self.remote.add(adapter.key_for(CONTEXT, CONTEXT["destination"] + "/data"), "foreign", body=b"foreign")
        self.remote.uploads["next-owner-upload"] = {"request": {"Key": adapter.key_for(CONTEXT, CONTEXT["destination"] + "/next")}}
        snapshot = json.dumps(self.remote.versions, sort_keys=True, default=str)
        result = self.run_archive("cancel")
        self.assertEqual(result["status"], "cancelled")
        self.assertTrue(result["already_cancelled"])
        self.assertEqual([item["version_id"] for item in result["retained_versions"]], ["foreign"])
        self.assertEqual(result["retained_uploads"], [{"key": adapter.key_for(CONTEXT, CONTEXT["destination"] + "/next"),
                                                     "upload_id": "next-owner-upload"}])
        self.assertEqual(set(self.remote.uploads), {"next-owner-upload"})
        self.assertEqual(snapshot, json.dumps(self.remote.versions, sort_keys=True, default=str))

    def test_completed_cancel_refuses_reappearing_owned_versions_without_deleting_anything(self):
        self.remote.source("data", "original")
        receipt = self.run_archive()
        copied = next(item.copy() for item in self.remote.versions if item["VersionId"] == receipt["versions"][0]["destination_version_id"])
        self.run_archive("cancel")
        self.remote.versions.append(copied)
        deletes = self.remote.counts["delete_object"]
        with self.assertRaisesRegex(RuntimeError, "unexpectedly contains owned"):
            self.run_archive("cancel")
        self.assertEqual(self.remote.counts["delete_object"], deletes)

    def test_completed_cancel_detects_late_sdk_retry_without_requiring_source_history(self):
        self.remote.source("data", "original")
        receipt = self.run_archive()
        row = receipt["versions"][0]
        key = adapter.key_for(CONTEXT, row["destination_object"])
        copied = self.remote.exact(key, row["destination_version_id"]).copy()
        self.run_archive("cancel")
        self.remote.versions = []
        self.remote.add(key, "late-sdk-retry", body=copied["body"], metadata=copied["Metadata"])
        journal = Path(self.journal).read_bytes()
        before = len(self.remote.calls)
        with self.assertRaisesRegex(RuntimeError, "unexpectedly contains owned"):
            self.run_archive("cancel")
        self.assertEqual(self.remote.counts["delete_object"], 1)
        self.assertEqual(Path(self.journal).read_bytes(), journal)
        source = adapter.key_for(CONTEXT, CONTEXT["source"] + "/")
        self.assertTrue(all(not request.get("Prefix", request.get("Key", "")).startswith(source)
                            for _, request in self.remote.calls[before:]))

    def test_complete_history_includes_retired_keys_delete_markers_and_null_version(self):
        first = self.remote.source("data/file.txt", "null", body=b"initial", metadata={"original": "retained"}, tags=[{"Key": "purpose", "Value": "test"}], ContentType="text/plain")
        self.remote.source("data/file.txt", "deleted-once", marker=True)
        self.remote.source("data/file.txt", "latest", body=b"last")
        self.remote.source("retired.txt", "retired-payload", body=b"retired")
        self.remote.source("retired.txt", "retired-marker", marker=True)
        self.remote.add("storage/20260702-old-task-neighbor/file", "unrelated")
        original = self.source_snapshot()
        receipt = self.run_archive()
        self.assertEqual(receipt["status"], "copied")
        self.assertEqual(len(receipt["versions"]), 5)
        self.assertEqual(receipt["source_cleanup"], "after_verified_git_publication")
        self.assertEqual(original, self.source_snapshot())
        self.assertEqual([row["source_version_id"] for row in receipt["versions"][:3]], ["null", "deleted-once", "latest"])
        self.assertEqual(receipt["versions"][0]["source_last_modified"], first["LastModified"].isoformat())
        copied = self.remote.exact(adapter.key_for(CONTEXT, receipt["versions"][0]["destination_object"]), receipt["versions"][0]["destination_version_id"])
        self.assertEqual(copied["Metadata"]["original"], "retained")
        self.assertEqual(copied["tags"], first["tags"])
        self.assertEqual(copied["ContentType"], "text/plain")
        self.assertNotEqual(receipt["versions"][0]["source_version_id"], receipt["versions"][0]["destination_version_id"])
        self.assert_no_source_writes()

    def test_plan_is_read_only_and_preserves_both_pagination_markers(self):
        self.remote.page_size = 1
        for index in range(4):
            self.remote.source("data", str(index))
        receipt = self.run_archive("plan")
        self.assertEqual(receipt["status"], "planned")
        self.assertFalse(Path(self.journal).exists())
        requests = [request for method, request in self.remote.calls if method == "list_object_versions"]
        self.assertEqual(len(requests), 5)
        self.assertTrue(all(request.get("VersionIdMarker") for request in requests[1:4]))
        self.assertTrue(all(method in ("list_object_versions", "list_multipart_uploads") for method, _ in self.remote.calls))

    def test_destination_delete_marker_counts_as_collision_before_any_writes(self):
        self.remote.source("data", "v1")
        self.remote.add(adapter.key_for(CONTEXT, CONTEXT["destination"] + "/old"), "collision", marker=True)
        with self.assertRaisesRegex(RuntimeError, "already contains"):
            self.run_archive()
        self.assertEqual(self.remote.counts["copy_object"], 0)
        self.assertFalse(Path(self.journal).exists())

    def test_planned_source_snapshot_cannot_silently_change(self):
        self.remote.source("data", "v1")
        planned = self.run_archive("plan")
        self.remote.source("data", "v2")
        with self.assertRaisesRegex(RuntimeError, "changed after planning"):
            self.run_archive(planned=planned)
        self.assertEqual(self.remote.counts["copy_object"], 0)

    def test_copy_is_idempotent_and_verify_reads_all_exact_versions(self):
        self.remote.source("data", "v1")
        self.remote.source("data", "v2")
        receipt = self.run_archive()
        copies = self.remote.counts["copy_object"]
        repeated = self.run_archive()
        self.assertEqual(repeated, receipt)
        self.assertEqual(self.remote.counts["copy_object"], copies)
        self.assertEqual(self.run_archive("verify")["status"], "verified")
        self.assertTrue(all(request.get("VersionId") for method, request in self.remote.calls if method == "head_object"))

    def test_completed_copy_can_resume_after_publication_retires_source(self):
        self.remote.source("data", "v1")
        receipt = self.run_archive()
        prefix = adapter.key_for(CONTEXT, CONTEXT["source"] + "/")
        self.remote.versions = [row for row in self.remote.versions if not row["Key"].startswith(prefix)]
        self.assertEqual(self.run_archive(), receipt)
        self.assertEqual(self.remote.counts["copy_object"], 1)

    def test_complete_prefix_scans_do_not_repeat_for_every_payload(self):
        for index in range(25):
            self.remote.source(f"file-{index}", f"version-{index}")
        receipt = self.run_archive()
        self.assertEqual(len(receipt["versions"]), 25)
        self.assertLessEqual(self.remote.counts["list_object_versions"], 5)

    def test_response_loss_recovers_payload_by_transaction_token_without_duplicate(self):
        self.remote.source("data", "v1")
        self.remote.lose_after["copy_object"] = 1
        with self.assertRaises(ConnectionError):
            self.run_archive()
        journal = adapter.load_journal(self.journal)
        self.assertTrue(journal["versions"][0]["started"])
        receipt = self.run_archive()
        self.assertEqual(receipt["status"], "copied")
        self.assertEqual(self.remote.counts["copy_object"], 1)
        self.assert_no_source_writes()

    def test_response_loss_recovers_one_delete_marker_without_duplicate(self):
        self.remote.source("retired", "v1")
        self.remote.source("retired", "gone", marker=True)
        self.remote.lose_after["delete_object"] = 1
        with self.assertRaises(ConnectionError):
            self.run_archive()
        receipt = self.run_archive()
        self.assertTrue(receipt["versions"][-1]["delete_marker"])
        self.assertEqual(self.remote.counts["delete_object"], 1)
        self.assert_no_source_writes()

    def test_failed_request_before_mutation_can_retry(self):
        self.remote.source("data", "v1")
        self.remote.fail_before["copy_object"] = 1
        original = self.source_snapshot()
        with self.assertRaises(ConnectionError):
            self.run_archive()
        receipt = self.run_archive()
        self.assertEqual(receipt["status"], "copied")
        self.assertEqual(original, self.source_snapshot())
        self.assert_no_source_writes()

    def test_response_loss_does_not_adopt_an_unrelated_destination_payload(self):
        self.remote.source("data", "v1")
        self.remote.fail_before["copy_object"] = 1
        with self.assertRaises(ConnectionError):
            self.run_archive()
        self.remote.add(adapter.key_for(CONTEXT, CONTEXT["destination"] + "/data"), "intruder")
        with self.assertRaisesRegex(RuntimeError, "unrelated or ambiguous"):
            self.run_archive()
        self.assertEqual(self.remote.counts["copy_object"], 1)

    def test_ambiguous_multiple_delete_markers_fail_closed(self):
        self.remote.source("gone", "source-marker", marker=True)
        self.remote.lose_after["delete_object"] = 1
        with self.assertRaises(ConnectionError):
            self.run_archive()
        self.remote.add(adapter.key_for(CONTEXT, CONTEXT["destination"] + "/gone"), "other-marker", marker=True)
        with self.assertRaisesRegex(RuntimeError, "unrelated or ambiguous"):
            self.run_archive()

    def test_source_change_before_completion_leaves_source_history_intact(self):
        self.remote.source("data", "v1")
        def mutate(method, request, count):
            if method == "copy_object" and count == 1:
                self.remote.source("new-data", "during-copy")
        self.remote.hook = mutate
        with self.assertRaisesRegex(RuntimeError, "changed before completion"):
            self.run_archive()
        self.assertEqual(len(self.remote.matching(adapter.key_for(CONTEXT, CONTEXT["source"] + "/"))), 2)
        self.assert_no_source_writes()

    def test_destination_loss_is_detected_and_never_recreates_without_history(self):
        self.remote.source("data", "v1")
        receipt = self.run_archive()
        removed = receipt["versions"][0]["destination_version_id"]
        self.remote.versions = [row for row in self.remote.versions if row["VersionId"] != removed]
        with self.assertRaisesRegex(RuntimeError, "lost a previously copied"):
            self.run_archive()
        self.assertEqual(self.remote.counts["copy_object"], 1)

    def test_large_object_multipart_uses_exact_versions_preserves_properties_and_records_new_etag(self):
        source = self.remote.source("large", "large-v1", size=adapter.COPY_LIMIT + 7,
                                    metadata={"owner": "test"}, tags=[{"Key": "a key", "Value": "a+b /"}], ContentType="binary/test")
        receipt = self.run_archive()
        row = receipt["versions"][0]
        self.assertEqual(row["size"], source["Size"])
        self.assertEqual(row["source_etag"], adapter.etag(source["ETag"]))
        self.assertEqual(row["destination_etag"], "different-multipart-etag")
        self.assertEqual(self.remote.counts["copy_object"], 0)
        self.assertGreater(self.remote.counts["upload_part_copy"], 1)
        self.assertFalse(self.remote.uploads)
        copied = self.remote.exact(adapter.key_for(CONTEXT, row["destination_object"]), row["destination_version_id"])
        self.assertEqual(copied["tags"], source["tags"])
        self.assertEqual(copied["Metadata"]["owner"], "test")
        self.assertEqual(copied["ContentType"], "binary/test")
        self.assert_no_source_writes()

    def test_multipart_failure_aborts_destination_upload_and_can_resume(self):
        self.remote.source("large", "large-v1", size=adapter.COPY_LIMIT + 1)
        self.remote.fail_before["upload_part_copy"] = 2
        original = self.source_snapshot()
        with self.assertRaises(ConnectionError):
            self.run_archive()
        self.assertFalse(self.remote.uploads)
        self.assertEqual(self.remote.counts["abort_multipart_upload"], 1)
        receipt = self.run_archive()
        self.assertEqual(receipt["status"], "copied")
        self.assertEqual(original, self.source_snapshot())
        self.assert_no_source_writes()

    def test_lost_multipart_completion_response_resumes_without_duplicate(self):
        self.remote.source("large", "large-v1", size=adapter.COPY_LIMIT + 1)
        self.remote.lose_after["complete_multipart_upload"] = 1
        with self.assertRaises(ConnectionError):
            self.run_archive()
        receipt = self.run_archive()
        self.assertEqual(receipt["status"], "copied")
        self.assertEqual(self.remote.counts["complete_multipart_upload"], 1)
        self.assertFalse(self.remote.uploads)

    def test_missing_destination_version_id_is_not_silently_accepted(self):
        self.remote.source("data", "v1")
        self.remote.response_override["copy_object"] = {"VersionId": "null"}
        with self.assertRaisesRegex(RuntimeError, "no exact destination"):
            self.run_archive()
        self.remote.response_override.clear()
        self.assertEqual(self.run_archive()["status"], "copied")
        self.assertEqual(self.remote.counts["copy_object"], 1)

    def test_empty_task_history_copies_nothing(self):
        receipt = self.run_archive()
        self.assertEqual(receipt["versions"], [])
        self.assertEqual(receipt["status"], "copied")
        self.assert_no_source_writes()

    def test_listing_stops_on_nonadvancing_pagination(self):
        page = {"IsTruncated": True, "NextKeyMarker": "same", "NextVersionIdMarker": "same"}
        with mock.patch.object(self.remote, "call_s3", return_value=page) as request:
            with self.assertRaisesRegex(RuntimeError, "repeated pagination"):
                self.run_archive("plan")
        self.assertEqual(request.call_count, 2)
        self.assertEqual(self.remote.counts["copy_object"], 0)

    def test_listing_missing_markers_cannot_prove_full_inventory(self):
        self.remote.source("data", "v1")
        self.remote.response_override["list_object_versions"] = {"IsTruncated": True}
        with self.assertRaisesRegex(RuntimeError, "pagination markers"):
            self.run_archive("plan")

    def test_receipt_cannot_escape_task_source_or_destination(self):
        self.remote.source("data", "v1")
        planned = self.run_archive("plan")
        planned["versions"][0]["destination_object"] = "unrelated/data"
        with self.assertRaisesRegex(RuntimeError, "escaped its destination"):
            self.run_archive(planned=planned)
        self.assertEqual(self.remote.counts["copy_object"], 0)

    def test_receipt_from_another_remote_cannot_be_used(self):
        self.remote.source("data", "v1")
        planned = self.run_archive("plan")
        planned["remote_prefix"] = "other-storage"
        with self.assertRaisesRegex(RuntimeError, "does not match"):
            self.run_archive(planned=planned)

    def test_verify_can_use_published_receipt_without_private_journal(self):
        self.remote.source("data", "v1")
        receipt = self.run_archive()
        Path(self.journal).unlink()
        self.assertEqual(self.run_archive("verify", receipt=receipt)["status"], "verified")
        self.assertEqual(adapter.archive(self.remote, CONTEXT, "verify", receipt)["status"], "verified")

    def test_verify_cannot_claim_two_source_versions_share_one_destination_version(self):
        self.remote.source("data", "v1")
        self.remote.source("data", "v2")
        receipt = self.run_archive()
        Path(self.journal).unlink()
        receipt["versions"][1]["destination_version_id"] = receipt["versions"][0]["destination_version_id"]
        with self.assertRaisesRegex(RuntimeError, "repeated destination"):
            self.run_archive("verify", receipt=receipt)

    def test_verify_source_accepts_an_unchanged_complete_copied_receipt(self):
        self.remote.source("data", "v1")
        self.remote.source("data", "v2")
        receipt = self.run_archive()
        verified = adapter.archive(self.remote, CONTEXT, "verify-source", receipt)
        self.assertEqual(verified["status"], "source-verified")
        self.assertEqual(self.remote.counts["copy_object"], 2)
        self.assert_no_source_writes()

    def test_verify_source_rejects_a_copied_receipt_omitting_an_old_version(self):
        self.remote.source("data", "v1")
        self.remote.source("data", "v2")
        receipt = self.run_archive()
        receipt["versions"].pop(0)
        with self.assertRaisesRegex(RuntimeError, "complete live source history"):
            adapter.archive(self.remote, CONTEXT, "verify-source", receipt)
        self.assertEqual(self.remote.counts["copy_object"], 2)
        self.assert_no_source_writes()

    def test_current_version_is_preserved_when_marker_and_payload_share_a_timestamp(self):
        modified = datetime(2026, 7, 2, tzinfo=timezone.utc)
        self.remote.source("data", "one", modified=modified)
        self.remote.source("data", "marker", marker=True, modified=modified)
        self.remote.source("data", "latest", modified=modified)
        receipt = self.run_archive()
        self.assertEqual(receipt["versions"][-1]["source_version_id"], "latest")
        self.assertTrue(receipt["versions"][-1]["source_is_latest"])

    def test_symlink_journal_is_rejected_before_remote_mutation(self):
        real = Path(self.temporary.name) / "real.json"
        real.write_text("{}")
        link = Path(self.temporary.name) / "link.json"
        link.symlink_to(real)
        with self.assertRaisesRegex(RuntimeError, "regular-file path"):
            self.run_archive(state_path=str(link))
        self.assertEqual(self.remote.counts["copy_object"], 0)


class CopyReservationTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = (Path(self.directory.name) / "checkout").resolve()
        self.root.mkdir()
        self.git_remote = self.root.parent / "remote.git"
        self.git("init", "-q", "-b", "main")
        self.git("init", "-q", "--bare", str(self.git_remote))
        self.git("remote", "add", "origin", str(self.git_remote))
        (self.root / ".workspace-mgr.toml").write_text("[git]\nremote='origin'\nbranch='main'\n")
        self.transport = S3History()
        self.transport.source("data", "original")
        self.payload = {"source": CONTEXT["source"], "destination": CONTEXT["destination"],
                        "state_path": str(self.root.parent / "copy-journal.json"), "repo_path": str(self.root)}

    def git(self, *args, input=None):
        return subprocess.run(["git", *args], cwd=self.root, input=input,
                              capture_output=True, check=True).stdout

    def reserve(self):
        planned = adapter.archive(self.transport, CONTEXT, "plan", self.payload)
        receipt = adapter.normalized_planned_receipt(planned)
        owner = hashlib.sha256(str(self.root).encode()).hexdigest()
        nonce = str(adapter.uuid.uuid4())
        body = adapter.canonical_json({"attempt_nonce": nonce, "owner_hash": owner, "receipt": receipt})
        identity = adapter.canonical_json([CONTEXT["bucket"], CONTEXT["remote_prefix"], CONTEXT["source"]])
        reference = "refs/tags/workspace-mgr/archive-copy/" + hashlib.sha256(identity).hexdigest()
        oid = self.git("hash-object", "-w", "--stdin", input=body).decode().strip()
        self.git("push", "--force-with-lease=" + reference + ":", "origin", oid + ":" + reference)
        state = self.root.parent / "reservation.json"
        adapter.save_journal(state, {"schema_version": 1, "attempt_nonce": nonce, "owner_hash": owner,
                                     "receipt": receipt, "acquired": True})
        proof = {"mode": "git-copy-reservation", "remote": "origin", "ref": reference, "oid": oid,
                 "owner_hash": owner, "repo_path": str(self.root), "state_path": str(state),
                 "attempt_nonce": nonce,
                 "receipt_sha256": hashlib.sha256(adapter.canonical_json(receipt)).hexdigest(),
                 "descriptor_sha256": hashlib.sha256(body).hexdigest(), "receipt": receipt}
        self.payload.update(planned=planned, reservation=proof)
        return proof

    def remove_claim(self):
        self.git("--git-dir", str(self.git_remote), "update-ref", "-d", self.payload["reservation"]["ref"])

    def run_copy(self):
        return adapter.archive(self.transport, CONTEXT, "copy", self.payload)

    def test_acquired_private_reservation_and_exact_remote_blob_allow_copy(self):
        self.reserve()
        self.assertEqual(self.run_copy()["status"], "copied")
        self.assertEqual(self.transport.counts["copy_object"], 1)

    def test_valid_reservation_uses_existing_tomlkit_dependency_on_older_python(self):
        import tomlkit
        self.reserve()
        with mock.patch.object(adapter, "tomllib", None), mock.patch.object(tomlkit, "parse", wraps=tomlkit.parse) as parse:
            self.assertEqual(self.run_copy()["status"], "copied")
        self.assertGreater(parse.call_count, 0)
        self.assertTrue(all(call.args[0] == (self.root / ".workspace-mgr.toml").read_text()
                            for call in parse.call_args_list))
        self.assertEqual(self.transport.counts["copy_object"], 1)

    def test_removed_claim_prevents_any_storage_mutation(self):
        self.reserve()
        self.remove_claim()
        with self.assertRaisesRegex(RuntimeError, "removed or replaced"):
            self.run_copy()
        self.assertEqual(self.transport.counts["copy_object"], 0)
        self.assertEqual(self.transport.counts["delete_object"], 0)
        self.assertFalse(Path(self.payload["state_path"]).exists())

    def test_explicit_missing_or_forged_proofs_are_rejected_before_writes(self):
        proof = self.reserve()
        for forged in (None, {}, {**proof, "owner_hash": "foreign"},
                       {**proof, "descriptor_sha256": "foreign"},
                       {**proof, "receipt_sha256": "foreign"},
                       {**proof, "ref": "refs/tags/foreign"},
                       {**proof, "attempt_nonce": None},
                       {**proof, "attempt_nonce": "another-attempt"},
                       {**proof, "state_path": None},
                       {**proof, "repo_path": str(self.root.parent)}):
            with self.subTest(forged=forged), self.assertRaises(RuntimeError):
                adapter.archive(self.transport, CONTEXT, "copy", {**self.payload, "reservation": forged})
        self.assertEqual(self.transport.counts["copy_object"], 0)
        self.assertEqual(self.transport.counts["create_multipart_upload"], 0)

    def test_unacquired_private_state_cannot_authorize_storage_writes(self):
        proof = self.reserve()
        state = adapter.load_journal(proof["state_path"])
        state["acquired"] = False
        adapter.save_journal(proof["state_path"], state)
        with self.assertRaisesRegex(RuntimeError, "acquired private journal"):
            self.run_copy()
        self.assertEqual(self.transport.counts["copy_object"], 0)

    def test_self_consistent_forged_descriptor_cannot_replace_the_git_blob(self):
        proof = copy.deepcopy(self.reserve())
        proof["receipt"]["source_cleanup"] = "forged"
        body = adapter.canonical_json({"attempt_nonce": proof["attempt_nonce"], "owner_hash": proof["owner_hash"],
                                       "receipt": proof["receipt"]})
        proof["descriptor_sha256"] = hashlib.sha256(body).hexdigest()
        proof["receipt_sha256"] = hashlib.sha256(adapter.canonical_json(proof["receipt"])).hexdigest()
        state = adapter.load_journal(proof["state_path"])
        state["receipt"] = proof["receipt"]
        adapter.save_journal(proof["state_path"], state)
        payload = {**self.payload, "planned": proof["receipt"], "reservation": proof}
        with self.assertRaisesRegex(RuntimeError, "blob differs"):
            adapter.archive(self.transport, CONTEXT, "copy", payload)
        self.assertEqual(self.transport.counts["copy_object"], 0)

    def test_claim_loss_after_payload_prevents_the_next_delete_marker(self):
        self.transport.source("data", "source-marker", marker=True)
        self.reserve()
        def remove_after_copy(method, request, count):
            if method == "copy_object" and count == 1:
                self.remove_claim()
        self.transport.hook = remove_after_copy
        with self.assertRaisesRegex(RuntimeError, "removed or replaced"):
            self.run_copy()
        self.assertEqual(self.transport.counts["copy_object"], 1)
        self.assertEqual(self.transport.counts["delete_object"], 0)
        self.assertEqual(adapter.load_journal(self.payload["state_path"])["status"], "copying")

    def test_claim_loss_prevents_copy_completion_even_after_the_last_write(self):
        self.reserve()
        def remove_after_copy(method, request, count):
            if method == "copy_object":
                self.remove_claim()
        self.transport.hook = remove_after_copy
        with self.assertRaisesRegex(RuntimeError, "removed or replaced"):
            self.run_copy()
        self.assertEqual(self.transport.counts["copy_object"], 1)
        self.assertEqual(adapter.load_journal(self.payload["state_path"])["status"], "copying")

    def test_multipart_claim_loss_blocks_further_parts_completion_and_abort(self):
        self.transport.versions.clear()
        self.transport.source("large", "original-large", size=adapter.COPY_LIMIT + 1)
        self.reserve()
        def remove_after_part(method, request, count):
            if method == "upload_part_copy" and count == 1:
                self.remove_claim()
        self.transport.hook = remove_after_part
        with self.assertRaisesRegex(RuntimeError, "removed or replaced"):
            self.run_copy()
        self.assertEqual(self.transport.counts["create_multipart_upload"], 1)
        self.assertEqual(self.transport.counts["upload_part_copy"], 1)
        self.assertEqual(self.transport.counts["complete_multipart_upload"], 0)
        self.assertEqual(self.transport.counts["abort_multipart_upload"], 0)
        self.assertEqual(len(self.transport.uploads), 1)


class MainBindingTests(unittest.TestCase):
    def test_copy_without_reservation_fails_before_opening_remote(self):
        repo = mock.Mock()
        modules = {"dvc": SimpleNamespace(), "dvc.repo": SimpleNamespace(Repo=repo)}
        payload = {"source": CONTEXT["source"], "destination": CONTEXT["destination"]}
        with mock.patch.dict(sys.modules, modules), self.assertRaisesRegex(RuntimeError, "Git reservation"):
            adapter.main(["/private/repo", "copy", json.dumps(payload)])
        repo.assert_not_called()

    def test_main_uses_configured_private_remote_and_emits_json(self):
        remote = S3History()
        class Raw:
            def split_path(self, path):
                return CONTEXT["bucket"], CONTEXT["remote_prefix"], None
            def is_bucket_versioned(self, bucket):
                return True
            def call_s3(self, method, **request):
                return remote.call_s3(method, **request)
        class Repo:
            def __init__(self, path):
                self.cloud = SimpleNamespace(get_remote=lambda: SimpleNamespace(
                    name=CONTEXT["remote"], path="configured-remote", fs=SimpleNamespace(version_aware=True, fs=Raw()),
                ))
            def __enter__(self):
                return self
            def __exit__(self, *args):
                return False
        modules = {"dvc": SimpleNamespace(), "dvc.repo": SimpleNamespace(Repo=Repo)}
        payload = {"source": CONTEXT["source"], "destination": CONTEXT["destination"]}
        with mock.patch.dict(sys.modules, modules), mock.patch("builtins.print") as printed:
            result = adapter.main(["/private/repo", "plan", json.dumps(payload)])
        self.assertEqual(result["remote_prefix"], "storage")
        self.assertEqual(json.loads(printed.call_args.args[0]), result)

    def test_main_rejects_overlapping_paths_before_opening_remote(self):
        modules = {"dvc": SimpleNamespace(), "dvc.repo": SimpleNamespace(Repo=mock.Mock())}
        for destination in (CONTEXT["source"], CONTEXT["source"] + "/nested", "../escaping"):
            with self.subTest(destination=destination), mock.patch.dict(sys.modules, modules):
                with self.assertRaisesRegex(RuntimeError, "overlap|invalid archive"):
                    adapter.main(["/private/repo", "plan", json.dumps({"source": CONTEXT["source"], "destination": destination})])


if __name__ == "__main__":
    unittest.main()
