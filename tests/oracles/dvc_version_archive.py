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
import re
import subprocess
import sys
import tempfile
import time
try:
    import tomllib
except ModuleNotFoundError:
    tomllib = None
from urllib.parse import urlencode, urlsplit
import uuid


SCHEMA_VERSION = 1
LIST_PAGE_SIZE = 1000
MAX_LIST_PAGES = 100_000
COPY_LIMIT = 5 * 2**30
COPY_PART_SIZE = 512 * 2**20
TRANSACTION_METADATA = "workspace-mgr-archive-copy"
B2_VERSION_INTERVAL = 1.05
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
COPY_CONTROL_FIELDS = (
    "destination_version_id", "destination_etag", "destination_last_modified",
    "started", "multipart_upload_id", "cancel_started", "cancel_deleted", "cancel_owned_versions",
)


def normalized_planned_receipt(receipt):
    receipt = json.loads(json.dumps(receipt))
    receipt["status"] = "planned"
    receipt.pop("transaction_id", None)
    for row in receipt.get("versions", []):
        for field in COPY_CONTROL_FIELDS:
            row.pop(field, None)
    return receipt


def canonical_json(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8")


def verify_reservation(context, payload, journal=None):
    """Verify the non-expiring Git claim before any copy-side mutation."""
    proof = payload.get("reservation")
    if proof is None and "reservation" not in payload:
        # Direct in-process asset mocks have no repository. The production
        # orchestrator supplies a claim before invoking its copy transport.
        return
    if not isinstance(proof, dict) or proof.get("mode") != "git-copy-reservation":
        raise RuntimeError("archive copy requires a valid Git copy reservation")
    receipt = proof.get("receipt")
    validate_receipt(receipt, context)
    if normalized_planned_receipt(receipt) != receipt:
        raise RuntimeError("archive copy reservation does not contain a normalized planned receipt")
    planned = payload.get("planned")
    if planned is not None and normalized_planned_receipt(planned) != receipt:
        raise RuntimeError("archive copy reservation selects another planned receipt")
    if journal is not None and source_signature(journal["versions"]) != source_signature(receipt["versions"]):
        raise RuntimeError("archive copy reservation selects another original source snapshot")
    root = proof.get("repo_path")
    if (not isinstance(root, str) or not Path(root).is_absolute() or not Path(root).is_dir()
            or str(Path(root).resolve()) != root or payload.get("repo_path", root) != root):
        raise RuntimeError("archive copy reservation selects another repository")
    owner = hashlib.sha256(root.encode("utf-8")).hexdigest()
    nonce = proof.get("attempt_nonce")
    if not isinstance(nonce, str) or not nonce:
        raise RuntimeError("archive copy reservation has no attempt nonce")
    descriptor = {"attempt_nonce": nonce, "owner_hash": owner, "receipt": receipt}
    body = canonical_json(descriptor)
    identity = canonical_json([context["bucket"], context["remote_prefix"], context["source"]])
    reference = "refs/tags/workspace-mgr/archive-copy/" + hashlib.sha256(identity).hexdigest()
    oid, remote = proof.get("oid"), proof.get("remote")
    if (proof.get("owner_hash") != owner or proof.get("ref") != reference
            or proof.get("descriptor_sha256") != hashlib.sha256(body).hexdigest()
            or proof.get("receipt_sha256") != hashlib.sha256(canonical_json(receipt)).hexdigest()
            or not isinstance(oid, str) or not re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", oid)
            or not isinstance(remote, str) or not remote or remote.startswith("-") or any(ord(c) < 32 for c in remote)):
        raise RuntimeError("archive copy reservation has an invalid immutable identity")
    state_path = proof.get("state_path")
    if not isinstance(state_path, str) or not Path(state_path).is_absolute():
        raise RuntimeError("archive copy reservation has no absolute private journal")
    state = load_journal(state_path)
    if (not isinstance(state, dict) or state.get("schema_version") != 1 or state.get("acquired") is not True
            or state.get("attempt_nonce") != nonce or state.get("owner_hash") != owner or state.get("receipt") != receipt):
        raise RuntimeError("archive copy reservation is not owned by its acquired private journal")
    try:
        configuration = Path(root) / ".workspace-mgr.toml"
        if tomllib is not None:
            with configuration.open("rb") as stream:
                configured_remote = tomllib.load(stream)["git"]["remote"]
        else:
            # DVC already depends on tomlkit; older supported Python
            # interpreters need it only when verifying the configured remote.
            import tomlkit
            configured_remote = tomlkit.parse(configuration.read_text(encoding="utf-8"))["git"]["remote"]
    except (OSError, KeyError, ValueError, TypeError, ImportError) as error:
        raise RuntimeError("archive copy reservation cannot verify its repository configuration") from error
    if configured_remote != remote:
        raise RuntimeError("archive copy reservation selects another configured Git remote")

    def git(*args):
        result = subprocess.run(["git", *args], cwd=root, capture_output=True, check=False)
        if result.returncode:
            raise RuntimeError("archive copy reservation could not verify its Git binding")
        return result.stdout

    if git("rev-parse", "--show-toplevel").decode().strip() != root:
        raise RuntimeError("archive copy reservation is not the repository root")
    fetch = git("remote", "get-url", "--all", remote).splitlines()
    push = git("remote", "get-url", "--push", "--all", remote).splitlines()
    if len(fetch) != 1 or push != fetch:
        raise RuntimeError("archive copy reservation requires one identical Git fetch and push destination")
    if git("ls-remote", "--refs", "--", remote, reference) != f"{oid}\t{reference}\n".encode():
        raise RuntimeError("archive copy reservation Git claim was removed or replaced")
    if git("cat-file", "blob", oid) != body:
        raise RuntimeError("archive copy reservation Git blob differs from its descriptor")


def b2_version_spacing(raw_fs):
    client = getattr(raw_fs, "_s3", None)
    endpoint = (getattr(raw_fs, "endpoint_url", None)
                or getattr(raw_fs, "client_kwargs", {}).get("endpoint_url", "")
                or getattr(getattr(client, "meta", None), "endpoint_url", ""))
    host = urlsplit(endpoint or "").hostname or ""
    return host == "backblazeb2.com" or host.endswith(".backblazeb2.com")


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


def destination_uploads(raw_fs, context):
    prefix = key_for(context, context["destination"] + "/")
    request = {"Bucket": context["bucket"], "Prefix": prefix, "MaxUploads": LIST_PAGE_SIZE}
    uploads, seen, markers = [], set(), set()
    for _ in range(MAX_LIST_PAGES):
        response = raw_fs.call_s3("list_multipart_uploads", **request)
        for item in response.get("Uploads", []):
            key, upload = item.get("Key"), item.get("UploadId")
            if (not isinstance(key, str) or not key.startswith(prefix)
                    or not isinstance(upload, str) or not upload or (key, upload) in seen):
                raise RuntimeError("archive multipart inventory contains an escaped or repeated upload")
            seen.add((key, upload))
            uploads.append({"key": key, "upload_id": upload})
        if not response.get("IsTruncated"):
            return uploads
        marker = (response.get("NextKeyMarker"), response.get("NextUploadIdMarker"))
        if not marker[0] or not marker[1] or marker in markers:
            raise RuntimeError("archive multipart inventory has missing or repeated pagination markers")
        markers.add(marker)
        request["KeyMarker"], request["UploadIdMarker"] = marker
    raise RuntimeError("archive multipart inventory exceeded the archive pagination limit")


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


def abort_upload(raw_fs, context, row, authorize=None):
    upload_id = row.get("multipart_upload_id")
    if upload_id:
        if authorize is not None:
            authorize()
        try:
            raw_fs.call_s3("abort_multipart_upload", Bucket=context["bucket"],
                           Key=key_for(context, row["destination_object"]), UploadId=upload_id)
        except Exception as error:
            provider, seen = error, set()
            code = None
            while provider is not None and id(provider) not in seen:
                seen.add(id(provider))
                code = getattr(provider, "response", {}).get("Error", {}).get("Code")
                if code:
                    break
                provider = getattr(provider, "__cause__", None) or getattr(provider, "__context__", None)
            if code != "NoSuchUpload" and not (code is None and isinstance(error, FileNotFoundError)):
                raise
        row.pop("multipart_upload_id", None)


def copy_payload(raw_fs, context, journal, row, persist, authorize):
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
        authorize()
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
    authorize()
    response = raw_fs.call_s3("create_multipart_upload", Bucket=bucket, Key=destination_key, **headers)
    upload_id = response.get("UploadId")
    if not upload_id:
        raise RuntimeError("archive multipart copy returned no upload ID")
    row["multipart_upload_id"] = upload_id
    persist()
    part_size = max(COPY_PART_SIZE, (row["size"] + 9999) // 10000)
    if part_size > COPY_LIMIT:
        abort_upload(raw_fs, context, row, authorize)
        persist()
        raise RuntimeError("archive object exceeds the S3 multipart-copy limit")
    try:
        parts = []
        for number, start in enumerate(range(0, row["size"], part_size), 1):
            last = min(start + part_size, row["size"]) - 1
            authorize()
            response = raw_fs.call_s3(
                "upload_part_copy", Bucket=bucket, Key=destination_key,
                UploadId=upload_id, PartNumber=number, CopySource=source,
                CopySourceIfMatch=info["ETag"], CopySourceRange=f"bytes={start}-{last}",
            )
            part_etag = response.get("CopyPartResult", {}).get("ETag")
            if not part_etag:
                raise RuntimeError("archive multipart copy returned no part ETag")
            parts.append({"PartNumber": number, "ETag": part_etag})
        authorize()
        response = raw_fs.call_s3(
            "complete_multipart_upload", Bucket=bucket, Key=destination_key,
            UploadId=upload_id, MultipartUpload={"Parts": parts},
        )
        record_destination(row, response.get("VersionId"), response.get("ETag"))
    except Exception:
        # A lost Complete response may already have committed the destination.
        # The next invocation recovers it by its per-copy metadata token.
        abort_upload(raw_fs, context, row, authorize)
        persist()
        raise


def public_receipt(journal, status=None):
    result = {key: value for key, value in journal.items() if key != "versions"}
    result["versions"] = [{key: value for key, value in row.items()
                           if key not in ("started", "multipart_upload_id", "cancel_started",
                                          "cancel_deleted", "cancel_owned_versions")}
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


def cancel_inventory(raw_fs, context, journal, *, verify_originals=True):
    """Identify only exact copies owned by this durable attempt.

    An S3 delete marker cannot carry a transaction token. A recorded marker ID
    is safe to remove, but a lost marker response is deliberately ambiguous.
    Other versions and uploads never become owned merely by sharing a prefix.
    """
    if not isinstance(journal.get("transaction_id"), str) or not journal["transaction_id"]:
        raise RuntimeError("archive journal contains no transaction identity")
    actual = destination_inventory(raw_fs, context)
    present = {(item["Key"], item["VersionId"]): item for item in actual}
    owned, tracked = {}, set()
    for row in journal["versions"]:
        records = row.setdefault("cancel_owned_versions", [])
        if not isinstance(records, list):
            raise RuntimeError("archive cancellation contains an invalid private version inventory")
        version = row.get("destination_version_id")
        if version and not any(record.get("version_id") == version for record in records if isinstance(record, dict)):
            records.append({"version_id": version, "etag": row.get("destination_etag"),
                            "delete_marker": row["delete_marker"],
                            "started": row.get("cancel_started") is True,
                            "deleted": row.get("cancel_deleted") is True})
        for record in records:
            if (not isinstance(record, dict) or not isinstance(record.get("version_id"), str)
                    or not record["version_id"] or record["version_id"] == "null"
                    or type(record.get("delete_marker")) is not bool
                    or type(record.get("started")) is not bool or type(record.get("deleted")) is not bool
                    or record["delete_marker"] != row["delete_marker"]
                    or (record["deleted"] and not record["started"])
                    or (record["delete_marker"] and (record["version_id"] != version or record.get("etag") is not None))
                    or (not record["delete_marker"] and (not isinstance(record.get("etag"), str) or not record["etag"]))):
                raise RuntimeError("archive cancellation contains an invalid private copied version")
            identity = (key_for(context, row["destination_object"]), record["version_id"])
            if identity in tracked:
                raise RuntimeError("archive cancellation repeated a private copied version")
            tracked.add(identity)
            item = present.get(identity)
            if item is None:
                if not record["started"]:
                    raise RuntimeError("archive cancellation lost an unretired copied version")
                continue
            if item["delete_marker"] != row["delete_marker"]:
                raise RuntimeError("archive cancellation found a mismatched copied delete marker")
            if not row["delete_marker"]:
                info = head_payload(raw_fs, context, row["destination_object"], record["version_id"], row["size"], record["etag"])
                if info.get("Metadata", {}).get(token_metadata_key(journal)) != copy_token(journal, row):
                    raise RuntimeError("archive cancellation cannot verify copied version ownership")
                if item.get("Size") != row["size"] or etag(item.get("ETag")) != record["etag"]:
                    raise RuntimeError("archive cancellation found mismatched copied payload metadata")
            owned[identity] = (row, record)
    for row in journal["versions"]:
        matching = [item for item in actual if item["Key"] == key_for(context, row["destination_object"])
                    and (item["Key"], item["VersionId"]) not in owned]
        if row["delete_marker"]:
            if row.get("started") and not row.get("destination_version_id") and any(item["delete_marker"] for item in matching):
                raise RuntimeError("archive cancellation cannot prove ownership of an unrecorded delete marker")
            continue
        for item in matching:
            if item["delete_marker"]:
                continue
            info = raw_fs.call_s3("head_object", Bucket=context["bucket"], Key=item["Key"], VersionId=item["VersionId"])
            if info.get("Metadata", {}).get(token_metadata_key(journal)) == copy_token(journal, row):
                if (item["VersionId"] == "null" or info.get("DeleteMarker")
                        or info.get("VersionId") != item["VersionId"] or info.get("ContentLength") != row["size"]
                        or item.get("Size") != row["size"] or not etag(item.get("ETag"))
                        or etag(info.get("ETag")) != etag(item.get("ETag"))):
                    raise RuntimeError("archive cancellation recovered a mismatched copied version")
                # SDK retries can create multiple generations with this one
                # row's token. Keep all exact IDs privately without changing
                # the receipt's published source/destination binding.
                record = {"version_id": item["VersionId"], "etag": etag(item["ETag"]),
                          "delete_marker": False, "started": False, "deleted": False}
                row["cancel_owned_versions"].append(record)
                identity = (item["Key"], item["VersionId"])
                tracked.add(identity)
                owned[identity] = (row, record)
    unrelated = [item for item in actual if (item["Key"], item["VersionId"]) not in owned]
    if verify_originals and (journal.get("status") != "cancelled" or owned
            or any(row.get("multipart_upload_id") for row in journal["versions"])):
        verify_cancel_source(raw_fs, context, journal)
    return owned, unrelated


def version_report(items):
    return [{"key": item["Key"], "version_id": item["VersionId"], "delete_marker": item["delete_marker"]}
            for item in items]


def verify_cancel_source(raw_fs, context, receipt):
    """Prove original complete history before withdrawing a registry mapping."""
    validate_receipt(receipt, context)
    current = {(row["source_object"], row["source_version_id"]): row
               for row in source_inventory(raw_fs, context)}
    immutable = ("source_last_modified", "delete_marker", "size", "source_etag")
    for row in receipt["versions"]:
        original = current.get((row["source_object"], row["source_version_id"]))
        if original is None or any(original[name] != row[name] for name in immutable):
            raise RuntimeError("archive cancellation requires every unchanged original source version and delete marker")
        if not row["delete_marker"]:
            head_payload(raw_fs, context, row["source_object"], row["source_version_id"], row["size"], row["source_etag"])


def cancel_copy(raw_fs, context, journal, state_path, *, preview):
    terminal = journal.get("status") == "cancelled"
    owned, unrelated = cancel_inventory(raw_fs, context, journal, verify_originals=not terminal)
    uploads = [{"object": row["destination_object"], "upload_id": row["multipart_upload_id"]}
               for row in journal["versions"] if row.get("multipart_upload_id")]
    upload_inventory = destination_uploads(raw_fs, context)
    owned_uploads = {(key_for(context, upload["object"]), upload["upload_id"]) for upload in uploads}
    unrelated_uploads = [item for item in upload_inventory if (item["key"], item["upload_id"]) not in owned_uploads]
    if terminal:
        if owned or any((item["key"], item["upload_id"]) in owned_uploads for item in upload_inventory):
            raise RuntimeError("completed archive cancellation unexpectedly contains owned versions or uploads")
        # A later transaction may now own the vacated namespace. It cannot
        # prevent the already completed attempt's local undo, and its bytes
        # are reported without being adopted into this transaction.
        return {"status": "cancelled",
                "already_cancelled": True, "deleted_versions": [], "delete_versions": [],
                "retained_versions": version_report(unrelated), "uploads": [], "retained_uploads": unrelated_uploads}
    deletions = [{"Key": key, "VersionId": version, "delete_marker": row["delete_marker"]}
                 for (key, version), (row, _) in owned.items()]
    if preview:
        return {"status": "would_cancel", "delete_versions": version_report(deletions),
                "retained_versions": version_report(unrelated), "uploads": uploads,
                "retained_uploads": unrelated_uploads}
    journal["status"] = "canceling"
    save_journal(state_path, journal)
    for row in journal["versions"]:
        abort_upload(raw_fs, context, row)
        save_journal(state_path, journal)
    for (key, version), (row, record) in owned.items():
        # Save before mutation: response loss can then be recovered by absence
        # of this exact ID, without interpreting another writer's version.
        record["started"] = True
        if row.get("destination_version_id") == version:
            row["cancel_started"] = True
        save_journal(state_path, journal)
        raw_fs.call_s3("delete_object", Bucket=context["bucket"], Key=key, VersionId=version)
        record["deleted"] = True
        if row.get("destination_version_id") == version:
            row["cancel_deleted"] = True
        save_journal(state_path, journal)
    for row in journal["versions"]:
        for record in row.get("cancel_owned_versions", []):
            if record["started"]:
                record["deleted"] = True
        if row.get("cancel_started"):
            row["cancel_deleted"] = True
    remaining_owned, remaining = cancel_inventory(raw_fs, context, journal, verify_originals=False)
    remaining_uploads = destination_uploads(raw_fs, context)
    if remaining_owned:
        save_journal(state_path, journal)
        raise RuntimeError("archive cancellation still contains an owned copied version")
    if any((item["key"], item["upload_id"]) in owned_uploads for item in remaining_uploads):
        raise RuntimeError("archive cancellation still contains an owned multipart upload")
    # Keep the receipt bindings as private audit evidence. A fresh copy starts
    # a new transaction only after this cleanup is complete.
    journal["status"] = "cancelled" if not remaining and not remaining_uploads else "canceling"
    save_journal(state_path, journal)
    return {"status": "cancelled" if not remaining and not remaining_uploads else "cancelled_with_unrelated_history",
            "deleted_versions": version_report(deletions),
            "retained_versions": version_report(remaining), "uploads": uploads,
            "retained_uploads": remaining_uploads}


def archive(raw_fs, context, operation, payload):
    if operation not in ("plan", "copy", "verify", "verify-source", "cancel", "cancel-preview"):
        raise RuntimeError(f"unknown archive storage operation: {operation!r}")
    if operation == "plan":
        rows = source_inventory(raw_fs, context)
        if destination_inventory(raw_fs, context) or destination_uploads(raw_fs, context):
            raise RuntimeError("archive destination already contains object history or multipart uploads")
        return {**context, "status": "planned", "versions": rows,
                "source_cleanup": "after_verified_git_publication"}
    state_path = payload.get("state_path")
    journal = load_journal(state_path) if state_path else None
    if operation in ("cancel", "cancel-preview"):
        if journal is None:
            return {"status": "no_remote_copy", "retained_versions": [], "uploads": []}
        validate_receipt(journal, context)
        return cancel_copy(raw_fs, context, journal, state_path, preview=operation == "cancel-preview")
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
    verify_reservation(context, payload, None if journal and journal.get("status") == "cancelled" else journal)
    if journal is not None:
        validate_receipt(journal, context)
        if journal.get("status") == "canceling":
            raise RuntimeError("archive cancellation is incomplete; finish cancel before starting another copy")
        if journal.get("status") == "cancelled":
            if destination_inventory(raw_fs, context) or destination_uploads(raw_fs, context):
                raise RuntimeError("archive destination contains unrelated history after cancellation")
            journal = None
    if journal is not None:
        if journal.get("status") == "copied":
            # Source cleanup belongs to publication, which can run after this
            # receipt has been completed. A retry verifies the immutable
            # destination versions without requiring the retired source.
            planned = payload.get("planned")
            if planned is not None:
                validate_receipt(planned, context)
                if source_signature(planned["versions"]) != source_signature(journal["versions"]):
                    raise RuntimeError("retained archive copy differs from this attempt's complete source history")
            verify_history(raw_fs, context, journal)
            verify_reservation(context, payload, journal)
            return public_receipt(journal)
    current = source_inventory(raw_fs, context)
    if journal is None:
        planned = payload.get("planned")
        if planned is not None:
            validate_receipt(planned, context)
            if source_signature(planned["versions"]) != source_signature(current):
                raise RuntimeError("archive source history changed after planning")
        if destination_inventory(raw_fs, context) or destination_uploads(raw_fs, context):
            raise RuntimeError("archive destination already contains object history or multipart uploads")
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
    authorize = lambda: verify_reservation(context, payload, journal)
    spaced = b2_version_spacing(raw_fs)
    written_keys = {row["destination_object"] for row in journal["versions"]
                    if row.get("destination_version_id")}
    for row in journal["versions"]:
        if row.get("destination_version_id"):
            continue
        # B2 documents that writes/markers for one key within a second may be
        # processed out of order. Space sequential generations, including a
        # recovered prior version, to keep the snapshot's exact current state.
        if spaced and row["destination_object"] in written_keys:
            time.sleep(B2_VERSION_INTERVAL)
        abort_upload(raw_fs, context, row, authorize)
        row["started"] = True
        persist()
        if row["delete_marker"]:
            authorize()
            response = raw_fs.call_s3("delete_object", Bucket=context["bucket"],
                                     Key=key_for(context, row["destination_object"]))
            if response.get("DeleteMarker") is not True:
                raise RuntimeError("archive delete-marker copy did not return a delete marker")
            record_destination(row, response.get("VersionId"))
        else:
            copy_payload(raw_fs, context, journal, row, persist, authorize)
        written_keys.add(row["destination_object"])
        persist()
    if source_signature(source_inventory(raw_fs, context)) != source_signature(journal["versions"]):
        raise RuntimeError("archive source history changed before completion")
    verify_history(raw_fs, context, journal)
    authorize()
    journal["status"] = "copied"
    persist()
    return public_receipt(journal)


def main(argv=None):
    from dvc.repo import Repo as DvcRepo

    argv = sys.argv[1:] if argv is None else argv
    repo_path, operation, payload = Path(argv[0]), argv[1], json.loads(argv[2])
    if operation == "copy" and "reservation" not in payload:
        raise RuntimeError("archive copy requires a Git reservation before opening remote storage")
    payload["repo_path"] = str(repo_path.resolve())
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
