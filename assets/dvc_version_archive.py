"""Copy a task's complete S3 version history without modifying its source.

The destination receives new S3 version IDs. A durable journal records every
source/destination pair and the original timestamps, including delete markers.
Only the command entry point imports the pinned private storage engine.
"""

from __future__ import annotations

from collections import defaultdict
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import sys
import tempfile
from urllib.parse import urlencode
import uuid


SCHEMA_VERSION = 1
LIST_PAGE_SIZE = 1000
MAX_LIST_PAGES = 100_000
COPY_LIMIT = 5 * 2**30
COPY_PART_SIZE = 512 * 2**20
TRANSACTION_METADATA = "workspace-mgr-archive-copy"
SOURCE_FIELDS = (
    "source_object", "destination_object", "source_version_id",
    "source_last_modified", "source_is_latest", "source_list_order",
    "delete_marker", "size", "source_etag",
)
COPY_HEADERS = (
    "CacheControl", "ContentDisposition", "ContentEncoding", "ContentLanguage",
    "ContentType", "Expires", "WebsiteRedirectLocation", "StorageClass",
    "ServerSideEncryption", "SSEKMSKeyId", "BucketKeyEnabled", "ObjectLockMode",
    "ObjectLockRetainUntilDate", "ObjectLockLegalHoldStatus",
)


def object_path(value):
    if not isinstance(value, str) or not value:
        raise RuntimeError("archive task paths must be non-empty strings")
    path = PurePosixPath(value)
    if path.is_absolute() or not path.parts or ".." in path.parts:
        raise RuntimeError(f"invalid archive task path: {value!r}")
    return path.as_posix()


def etag(value):
    return value.strip('"') if isinstance(value, str) else None


def timestamp(value):
    if isinstance(value, str):
        try:
            value = datetime.fromisoformat(value.replace("Z", "+00:00"))
        except ValueError as error:
            raise RuntimeError("S3 history contains an invalid timestamp") from error
    if not isinstance(value, datetime) or value.tzinfo is None:
        raise RuntimeError("S3 history contains no timezone-aware timestamp")
    return value.astimezone(timezone.utc).isoformat()


def list_versions(raw_fs, bucket, prefix):
    """Read the complete prefix, failing rather than accepting a partial scan."""
    request = {"Bucket": bucket, "Prefix": prefix, "MaxKeys": LIST_PAGE_SIZE}
    result = []
    seen_versions, seen_markers = set(), set()
    for _ in range(MAX_LIST_PAGES):
        response = raw_fs.call_s3("list_object_versions", **request)
        for section in ("Versions", "DeleteMarkers"):
            for item in response.get(section, []):
                key, version = item.get("Key"), item.get("VersionId")
                if not isinstance(key, str) or not key.startswith(prefix):
                    raise RuntimeError("S3 history listing escaped its requested prefix")
                if not isinstance(version, str) or not version:
                    raise RuntimeError("S3 history contains no exact version ID")
                identity = (key, version)
                if identity in seen_versions:
                    raise RuntimeError("S3 history listing repeated an object version")
                seen_versions.add(identity)
                result.append({**item, "delete_marker": section == "DeleteMarkers"})
        if not response.get("IsTruncated"):
            return result
        marker = (response.get("NextKeyMarker"), response.get("NextVersionIdMarker"))
        if not marker[0] or not marker[1] or marker in seen_markers:
            raise RuntimeError("S3 history listing has missing or repeated pagination markers")
        seen_markers.add(marker)
        request["KeyMarker"], request["VersionIdMarker"] = marker
    raise RuntimeError("S3 history listing exceeded the archive pagination limit")


def key_for(context, object_name):
    prefix = context["remote_prefix"]
    return f"{prefix}/{object_name}" if prefix else object_name


def source_inventory(raw_fs, context):
    source_prefix = key_for(context, context["source"] + "/")
    entries = list_versions(raw_fs, context["bucket"], source_prefix)
    rows = []
    for index, item in enumerate(entries):
        suffix = item["Key"][len(source_prefix):]
        deleted = item["delete_marker"]
        size = None if deleted else item.get("Size")
        source_etag = None if deleted else etag(item.get("ETag"))
        if not deleted and (not isinstance(size, int) or size < 0 or not source_etag):
            raise RuntimeError("S3 history contains incomplete payload metadata")
        rows.append({
            "source_object": context["source"] + "/" + suffix,
            "destination_object": context["destination"] + "/" + suffix,
            "source_version_id": item["VersionId"],
            "destination_version_id": None,
            "source_last_modified": timestamp(item.get("LastModified")),
            "source_is_latest": item.get("IsLatest") is True,
            "source_list_order": index,
            "delete_marker": deleted,
            "size": size,
            "source_etag": source_etag,
            "destination_etag": None,
        })
    groups = defaultdict(list)
    for row in rows:
        groups[row["source_object"]].append(row)
    if any(sum(row["source_is_latest"] for row in group) != 1 for group in groups.values()):
        raise RuntimeError("S3 history does not identify exactly one current version per object")
    # Parsed S3 responses separate payloads and markers. Timestamps retain the
    # original chronology; listing order breaks equal timestamp ties within
    # each section, and IsLatest always preserves the exact current state.
    rows.sort(key=lambda row: (
        row["source_object"], row["source_is_latest"],
        row["source_last_modified"], -row["source_list_order"],
    ))
    return rows


def source_signature(rows):
    return [{name: row.get(name) for name in SOURCE_FIELDS} for row in rows]


def receipt_context(receipt):
    return {name: receipt.get(name) for name in (
        "schema_version", "remote", "bucket", "remote_prefix", "source", "destination",
    )}


def validate_receipt(receipt, context):
    if not isinstance(receipt, dict) or receipt_context(receipt) != context:
        raise RuntimeError("archive receipt does not match its source, destination, or remote")
    rows = receipt.get("versions")
    if not isinstance(rows, list):
        raise RuntimeError("archive receipt contains no version inventory")
    identities, destination_identities = set(), set()
    for row in rows:
        if not isinstance(row, dict) or not all(field in row for field in SOURCE_FIELDS):
            raise RuntimeError("archive receipt contains incomplete version records")
        source_object = row["source_object"]
        prefix = context["source"] + "/"
        if not isinstance(source_object, str) or not source_object.startswith(prefix):
            raise RuntimeError("archive receipt escaped its source task")
        if row["destination_object"] != context["destination"] + "/" + source_object[len(prefix):]:
            raise RuntimeError("archive receipt escaped its destination task")
        identity = (source_object, row["source_version_id"])
        if not isinstance(identity[1], str) or not identity[1] or identity in identities:
            raise RuntimeError("archive receipt contains missing or repeated source versions")
        identities.add(identity)
        timestamp(row["source_last_modified"])
        if (not isinstance(row["source_is_latest"], bool)
                or not isinstance(row["delete_marker"], bool)
                or type(row["source_list_order"]) is not int or row["source_list_order"] < 0):
            raise RuntimeError("archive receipt contains invalid version chronology")
        if row["delete_marker"]:
            if row["size"] is not None or row["source_etag"] is not None:
                raise RuntimeError("archive receipt gives payload metadata to a delete marker")
        elif type(row["size"]) is not int or row["size"] < 0 or not isinstance(row["source_etag"], str) or not row["source_etag"]:
            raise RuntimeError("archive receipt contains incomplete payload metadata")
        destination_version = row.get("destination_version_id")
        if destination_version is not None:
            destination_identity = (row["destination_object"], destination_version)
            if (not isinstance(destination_version, str) or not destination_version
                    or destination_version == "null" or destination_identity in destination_identities):
                raise RuntimeError("archive receipt contains missing or repeated destination versions")
            destination_identities.add(destination_identity)


def save_journal(path, journal):
    path = Path(path)
    if not path.is_absolute() or path.is_symlink():
        raise RuntimeError("archive journal must be an absolute regular-file path")
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, delete=False) as stream:
            temporary = Path(stream.name)
            json.dump(journal, stream, sort_keys=True, indent=2)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        temporary = None
        descriptor = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(descriptor)
        finally:
            os.close(descriptor)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def load_journal(path):
    path = Path(path)
    if not path.is_absolute() or path.is_symlink():
        raise RuntimeError("archive journal must be an absolute regular-file path")
    if not path.exists():
        return None
    if not path.is_file():
        raise RuntimeError("archive journal must be a regular file")
    with path.open() as stream:
        return json.load(stream)


def copy_token(journal, row):
    identity = json.dumps([
        journal["transaction_id"], row["source_object"], row["source_version_id"],
    ], separators=(",", ":"))
    return hashlib.sha256(identity.encode()).hexdigest()


def token_metadata_key(journal):
    return TRANSACTION_METADATA + "-" + journal["transaction_id"]


def head_payload(raw_fs, context, object_name, version, size, expected_etag):
    info = raw_fs.call_s3(
        "head_object", Bucket=context["bucket"], Key=key_for(context, object_name),
        VersionId=version,
    )
    if (info.get("VersionId") != version or info.get("DeleteMarker")
            or info.get("ContentLength") != size or etag(info.get("ETag")) != expected_etag):
        raise RuntimeError(f"archive payload version is missing or mismatched: {object_name!r}")
    return info


def destination_inventory(raw_fs, context):
    return list_versions(raw_fs, context["bucket"], key_for(context, context["destination"] + "/"))


def validate_destination(raw_fs, context, journal, *, recover=False):
    actual = destination_inventory(raw_fs, context)
    wanted = {}
    for row in journal["versions"]:
        version = row.get("destination_version_id")
        if version:
            wanted[(key_for(context, row["destination_object"]), version)] = row
    unknown = [item for item in actual if (item["Key"], item["VersionId"]) not in wanted]
    pending = [row for row in journal["versions"] if row.get("started") and not row.get("destination_version_id")]
    if unknown and recover and len(pending) == 1 and len(unknown) == 1:
        row, item = pending[0], unknown[0]
        if item["Key"] == key_for(context, row["destination_object"]) and item["delete_marker"] == row["delete_marker"]:
            if row["delete_marker"]:
                owned = True
            else:
                info = head_payload(raw_fs, context, row["destination_object"], item["VersionId"], row["size"], etag(item.get("ETag")))
                owned = info.get("Metadata", {}).get(token_metadata_key(journal)) == copy_token(journal, row)
            if owned:
                record_destination(row, item["VersionId"], item.get("ETag"), item.get("LastModified"))
                wanted[(item["Key"], item["VersionId"])] = row
                unknown = []
    if unknown:
        raise RuntimeError("archive destination contains unrelated or ambiguous object history")
    if len(actual) != len(wanted):
        raise RuntimeError("archive destination lost a previously copied object version")
    for item in actual:
        row = wanted[(item["Key"], item["VersionId"])]
        if item["delete_marker"] != row["delete_marker"]:
            raise RuntimeError("archive destination has a mismatched delete marker")
        if not row["delete_marker"]:
            head_payload(raw_fs, context, row["destination_object"], row["destination_version_id"], row["size"], row["destination_etag"])
        row["destination_last_modified"] = timestamp(item.get("LastModified"))
    return actual


def record_destination(row, version, destination_etag=None, modified=None):
    if not isinstance(version, str) or not version or version == "null":
        raise RuntimeError("archive copy returned no exact destination version ID")
    if not row["delete_marker"] and not etag(destination_etag):
        raise RuntimeError("archive copy returned no destination ETag")
    row["destination_version_id"] = version
    row["destination_etag"] = None if row["delete_marker"] else etag(destination_etag)
    if modified is not None:
        row["destination_last_modified"] = timestamp(modified)
    row.pop("started", None)
    row.pop("multipart_upload_id", None)


def abort_upload(raw_fs, context, row):
    upload_id = row.get("multipart_upload_id")
    if upload_id:
        try:
            raw_fs.call_s3("abort_multipart_upload", Bucket=context["bucket"],
                           Key=key_for(context, row["destination_object"]), UploadId=upload_id)
        except Exception as error:
            code = getattr(error, "response", {}).get("Error", {}).get("Code")
            if code != "NoSuchUpload":
                raise
        row.pop("multipart_upload_id", None)


def copy_payload(raw_fs, context, journal, row, persist):
    bucket = context["bucket"]
    source_key, destination_key = (key_for(context, row[name]) for name in ("source_object", "destination_object"))
    info = head_payload(raw_fs, context, row["source_object"], row["source_version_id"], row["size"], row["source_etag"])
    metadata = dict(info.get("Metadata", {}))
    metadata_key = token_metadata_key(journal)
    if metadata_key in metadata:
        raise RuntimeError("archive transaction metadata would replace existing source metadata")
    metadata[metadata_key] = copy_token(journal, row)
    headers = {field: info[field] for field in COPY_HEADERS if field in info}
    headers["Metadata"] = metadata
    source = {"Bucket": bucket, "Key": source_key, "VersionId": row["source_version_id"]}
    if row["size"] <= COPY_LIMIT:
        response = raw_fs.call_s3(
            "copy_object", Bucket=bucket, Key=destination_key, CopySource=source,
            CopySourceIfMatch=info["ETag"], MetadataDirective="REPLACE",
            TaggingDirective="COPY", **headers,
        )
        result = response.get("CopyObjectResult", {})
        record_destination(row, response.get("VersionId"), result.get("ETag"), result.get("LastModified"))
        return
    # Multipart copies do not inherit metadata or tags from the source.
    tags = raw_fs.call_s3("get_object_tagging", Bucket=bucket, Key=source_key,
                          VersionId=row["source_version_id"]).get("TagSet", [])
    if tags:
        headers["Tagging"] = urlencode([(item["Key"], item["Value"]) for item in tags])
    response = raw_fs.call_s3("create_multipart_upload", Bucket=bucket, Key=destination_key, **headers)
    upload_id = response.get("UploadId")
    if not upload_id:
        raise RuntimeError("archive multipart copy returned no upload ID")
    row["multipart_upload_id"] = upload_id
    persist()
    part_size = max(COPY_PART_SIZE, (row["size"] + 9999) // 10000)
    if part_size > COPY_LIMIT:
        abort_upload(raw_fs, context, row)
        persist()
        raise RuntimeError("archive object exceeds the S3 multipart-copy limit")
    try:
        parts = []
        for number, start in enumerate(range(0, row["size"], part_size), 1):
            last = min(start + part_size, row["size"]) - 1
            response = raw_fs.call_s3(
                "upload_part_copy", Bucket=bucket, Key=destination_key,
                UploadId=upload_id, PartNumber=number, CopySource=source,
                CopySourceIfMatch=info["ETag"], CopySourceRange=f"bytes={start}-{last}",
            )
            part_etag = response.get("CopyPartResult", {}).get("ETag")
            if not part_etag:
                raise RuntimeError("archive multipart copy returned no part ETag")
            parts.append({"PartNumber": number, "ETag": part_etag})
        response = raw_fs.call_s3(
            "complete_multipart_upload", Bucket=bucket, Key=destination_key,
            UploadId=upload_id, MultipartUpload={"Parts": parts},
        )
        record_destination(row, response.get("VersionId"), response.get("ETag"))
    except Exception:
        # A lost Complete response may already have committed the destination.
        # The next invocation recovers it by its per-copy metadata token.
        abort_upload(raw_fs, context, row)
        persist()
        raise


def public_receipt(journal, status=None):
    result = {key: value for key, value in journal.items() if key != "versions"}
    result["versions"] = [{key: value for key, value in row.items()
                           if key not in ("started", "multipart_upload_id")}
                          for row in journal["versions"]]
    if status is not None:
        result["status"] = status
    result["source_cleanup"] = "after_verified_git_publication"
    return result


def verify_history(raw_fs, context, journal):
    if any(not row.get("destination_version_id") for row in journal["versions"]):
        raise RuntimeError("archive history copy is incomplete")
    actual = validate_destination(raw_fs, context, journal)
    latest = {(item["Key"], item["VersionId"]) for item in actual if item.get("IsLatest")}
    expected = {(key_for(context, row["destination_object"]), row["destination_version_id"])
                for row in journal["versions"] if row["source_is_latest"]}
    if latest != expected:
        raise RuntimeError("archive destination current versions differ from the source snapshot")


def archive(raw_fs, context, operation, payload):
    if operation not in ("plan", "copy", "verify", "verify-source"):
        raise RuntimeError(f"unknown archive storage operation: {operation!r}")
    if operation == "plan":
        rows = source_inventory(raw_fs, context)
        if destination_inventory(raw_fs, context):
            raise RuntimeError("archive destination already contains object history")
        return {**context, "status": "planned", "versions": rows,
                "source_cleanup": "after_verified_git_publication"}
    state_path = payload.get("state_path")
    journal = load_journal(state_path) if state_path else None
    if operation in ("verify", "verify-source"):
        journal = journal or payload.get("receipt") or payload.get("planned")
        if journal is None and isinstance(payload.get("versions"), list):
            journal = payload
        validate_receipt(journal, context)
        if operation == "verify-source":
            if source_signature(source_inventory(raw_fs, context)) != source_signature(journal["versions"]):
                raise RuntimeError("archive receipt does not preserve the complete live source history")
            return public_receipt(journal, "source-verified")
        verify_history(raw_fs, context, journal)
        return public_receipt(journal, "verified")
    if not state_path:
        raise RuntimeError("archive copy requires a durable private journal path")
    if journal is not None:
        validate_receipt(journal, context)
        if journal.get("status") == "copied":
            # Source cleanup belongs to publication, which can run after this
            # receipt has been completed. A retry verifies the immutable
            # destination versions without requiring the retired source.
            verify_history(raw_fs, context, journal)
            return public_receipt(journal)
    current = source_inventory(raw_fs, context)
    if journal is None:
        planned = payload.get("planned")
        if planned is not None:
            validate_receipt(planned, context)
            if source_signature(planned["versions"]) != source_signature(current):
                raise RuntimeError("archive source history changed after planning")
        if destination_inventory(raw_fs, context):
            raise RuntimeError("archive destination already contains object history")
        journal = {**context, "status": "copying", "transaction_id": str(uuid.uuid4()), "versions": current}
        save_journal(state_path, journal)
    validate_receipt(journal, context)
    if not isinstance(journal.get("transaction_id"), str) or not journal["transaction_id"]:
        raise RuntimeError("archive journal contains no transaction identity")
    if source_signature(journal["versions"]) != source_signature(current):
        raise RuntimeError("archive source history changed during copying")
    validate_destination(raw_fs, context, journal, recover=True)
    save_journal(state_path, journal)
    persist = lambda: save_journal(state_path, journal)
    for row in journal["versions"]:
        if row.get("destination_version_id"):
            continue
        abort_upload(raw_fs, context, row)
        row["started"] = True
        persist()
        if row["delete_marker"]:
            response = raw_fs.call_s3("delete_object", Bucket=context["bucket"],
                                     Key=key_for(context, row["destination_object"]))
            if response.get("DeleteMarker") is not True:
                raise RuntimeError("archive delete-marker copy did not return a delete marker")
            record_destination(row, response.get("VersionId"))
        else:
            copy_payload(raw_fs, context, journal, row, persist)
        persist()
    if source_signature(source_inventory(raw_fs, context)) != source_signature(journal["versions"]):
        raise RuntimeError("archive source history changed before completion")
    verify_history(raw_fs, context, journal)
    journal["status"] = "copied"
    persist()
    return public_receipt(journal)


def main(argv=None):
    from dvc.repo import Repo as DvcRepo

    argv = sys.argv[1:] if argv is None else argv
    repo_path, operation, payload = Path(argv[0]), argv[1], json.loads(argv[2])
    source, destination = object_path(payload.get("source")), object_path(payload.get("destination"))
    if source == destination or source.startswith(destination + "/") or destination.startswith(source + "/"):
        raise RuntimeError("archive source and destination task paths must not overlap")
    with DvcRepo(str(repo_path)) as repo:
        remote = repo.cloud.get_remote()
        if not remote.fs.version_aware:
            raise RuntimeError(f"configured remote {remote.name!r} is not version-aware")
        raw_fs = remote.fs.fs
        bucket, remote_prefix, version = raw_fs.split_path(remote.path)
        if not bucket or version:
            raise RuntimeError("archive remote must name an unversioned bucket location")
        if not raw_fs.is_bucket_versioned(bucket):
            raise RuntimeError(f"S3 bucket {bucket!r} does not have object versioning enabled")
        context = {"schema_version": SCHEMA_VERSION, "remote": remote.name, "bucket": bucket,
                   "remote_prefix": remote_prefix.rstrip("/"), "source": source, "destination": destination}
        result = archive(raw_fs, context, operation, payload)
        print(json.dumps(result, sort_keys=True))
        return result


if __name__ == "__main__":
    main()
