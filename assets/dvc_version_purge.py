import json
import sys
from pathlib import Path, PurePosixPath

from dvc.repo import Repo as DvcRepo


repo_path = Path(sys.argv[1])
operation = sys.argv[2]
payload = json.loads(sys.argv[3])


def normalized_object(parts):
    path = PurePosixPath(*parts)
    if path.is_absolute() or not path.parts or ".." in path.parts:
        raise RuntimeError(f"invalid managed-storage object path: {path.as_posix()!r}")
    return path.as_posix()


def objects_at(revision, pointers):
    found = []
    require_versions = revision is not None
    with DvcRepo(str(repo_path), rev=revision) as dvc_repo:
        for pointer in pointers:
            stages = list(dvc_repo.stage.collect(pointer))
            if not stages:
                raise RuntimeError(
                    f"managed-storage metadata did not define an output: {pointer}"
                )
            for stage in stages:
                for out in stage.outs:
                    if not out.is_in_repo or not out.can_push:
                        raise RuntimeError(
                            f"managed-storage output in {pointer!r} must be pushable and inside the repository"
                        )
                    _, base_parts = out.index_key
                    if out.isdir():
                        if out.files is None:
                            raise RuntimeError(
                                f"managed-storage directory metadata is incomplete: {pointer}"
                            )
                        for entry in out.files:
                            relpath = entry.get("relpath")
                            if not isinstance(relpath, str):
                                raise RuntimeError(
                                    f"managed-storage directory entry has no path: {pointer}"
                                )
                            rel = PurePosixPath(relpath)
                            if rel.is_absolute() or ".." in rel.parts:
                                raise RuntimeError(
                                    f"invalid path in managed-storage directory metadata: {relpath!r}"
                                )
                            version_id = entry.get("version_id")
                            if not version_id or version_id == "null":
                                if require_versions:
                                    raise RuntimeError(
                                        f"managed-storage object has no exact version ID: {pointer}:{relpath}"
                                    )
                                continue
                            found.append(
                                {
                                    "pointer": pointer,
                                    "object": normalized_object((*base_parts, *rel.parts)),
                                    "version_id": version_id,
                                }
                            )
                    else:
                        meta = out.meta
                        version_id = meta.version_id if meta else None
                        if not version_id or version_id == "null":
                            if require_versions:
                                raise RuntimeError(
                                    f"managed-storage object has no exact version ID: {pointer}"
                                )
                            continue
                        found.append(
                            {
                                "pointer": pointer,
                                "object": normalized_object(base_parts),
                                "version_id": version_id,
                            }
                        )
    return found


if operation == "list":
    result = []
    for request in payload:
        result.extend(objects_at(request.get("revision"), request["pointers"]))
    print(json.dumps(result, sort_keys=True))
    raise SystemExit(0)

if operation != "delete":
    raise RuntimeError(f"unknown managed-storage purge operation: {operation!r}")

deleted = []
already_absent = []
with DvcRepo(str(repo_path)) as dvc_repo:
    remote = dvc_repo.cloud.get_remote()
    if not remote.fs.version_aware:
        raise RuntimeError(f"configured remote {remote.name!r} is not version-aware")
    raw_fs = remote.fs.fs
    bucket, _, _ = raw_fs.split_path(remote.path)
    if not bucket:
        raise RuntimeError("configured S3 remote does not name a bucket")
    if not raw_fs.is_bucket_versioned(bucket):
        raise RuntimeError(f"S3 bucket {bucket!r} does not have object versioning enabled")

    candidates_by_object = {candidate["object"]: candidate for candidate in payload}
    for candidate in candidates_by_object.values():
        object_name = normalized_object(PurePosixPath(candidate["object"]).parts)
        remote_path = remote.fs.join(remote.path, *PurePosixPath(object_name).parts)
        object_bucket, object_key, _ = raw_fs.split_path(remote_path)
        if object_bucket != bucket or not object_key:
            raise RuntimeError(f"managed-storage purge escaped its configured bucket: {object_name!r}")
        request = {"Bucket": bucket, "Prefix": object_key}
        versions = []
        while True:
            response = raw_fs.call_s3("list_object_versions", **request)
            for section in ("Versions", "DeleteMarkers"):
                versions.extend(
                    item
                    for item in response.get(section, [])
                    if item.get("Key") == object_key
                )
            if not response.get("IsTruncated"):
                break
            request["KeyMarker"] = response.get("NextKeyMarker", "")
            request["VersionIdMarker"] = response.get("NextVersionIdMarker", "")
        if not versions:
            already_absent.append(candidate)
            continue
        for version in versions:
            raw_fs.call_s3(
                "delete_object",
                Bucket=bucket,
                Key=object_key,
                VersionId=version["VersionId"],
            )
        remaining = raw_fs.call_s3(
            "list_object_versions", Bucket=bucket, Prefix=object_key
        )
        if any(
            item.get("Key") == object_key
            for section in ("Versions", "DeleteMarkers")
            for item in remaining.get(section, [])
        ):
            raise RuntimeError(
                f"managed-storage object versions still exist after permanent deletion: {object_name!r}"
            )
        deleted.append(
            {
                **candidate,
                "deleted_version_ids": sorted(
                    version["VersionId"] for version in versions
                ),
            }
        )

print(
    json.dumps(
        {
            "mode": "permanent-version-deletion",
            "remote": remote.name,
            "deleted": deleted,
            "already_absent": already_absent,
        },
        sort_keys=True,
    )
)
