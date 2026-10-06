import json
import sys
from pathlib import Path, PurePosixPath

ARCHIVE_SUFFIX = "/.workspace-mgr-archive.json"
MAX_LIST_PAGES = 100_000


def normalized_object(parts):
    path = PurePosixPath(*parts)
    if path.is_absolute() or not path.parts or ".." in path.parts:
        raise RuntimeError(f"invalid managed-storage object path: {path.as_posix()!r}")
    return path.as_posix()


def objects_at(repo_path, revision, pointers):
    from dvc.repo import Repo as DvcRepo
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
                    # `out.isdir()` inspects the current worktree, so it becomes
                    # false when DVC opens a historical Git revision whose
                    # stored output is intentionally absent from Git. The
                    # pointer hash is the revision-stable source of truth.
                    if out.hash_info and out.hash_info.isdir:
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


def list_versions(raw_fs, bucket, prefix):
    request = {"Bucket": bucket, "Prefix": prefix, "MaxKeys": 1000}
    versions = []
    markers_seen, identities = set(), set()
    for _ in range(MAX_LIST_PAGES):
        response = raw_fs.call_s3("list_object_versions", **request)
        for section in ("Versions", "DeleteMarkers"):
            for item in response.get(section, []):
                key, version = item.get("Key"), item.get("VersionId")
                if (not isinstance(key, str) or not key.startswith(prefix)
                        or not isinstance(version, str) or not version):
                    raise RuntimeError("S3 purge history contains an invalid object version")
                identity = (key, version)
                if identity in identities:
                    raise RuntimeError("S3 purge history repeated an object version")
                identities.add(identity)
                versions.append(item)
        if not response.get("IsTruncated"):
            return versions
        marker = (response.get("NextKeyMarker"), response.get("NextVersionIdMarker"))
        if not all(isinstance(value, str) and value for value in marker) or marker in markers_seen:
            raise RuntimeError("S3 purge version pagination cannot advance")
        markers_seen.add(marker)
        request["KeyMarker"], request["VersionIdMarker"] = marker
    raise RuntimeError("S3 purge history exceeded the pagination limit")


def delete_candidates(raw_fs, remote, bucket, payload):
    groups = {}
    archives = {}
    for candidate in payload:
        pointer, object_name, version = (candidate.get(field) for field in ("pointer", "object", "version_id"))
        if (not isinstance(pointer, str) or not isinstance(object_name, str)
                or not isinstance(version, str) or not version):
            raise RuntimeError("managed-storage purge contains an invalid candidate")
        if pointer.endswith(ARCHIVE_SUFFIX):
            source = pointer[:-len(ARCHIVE_SUFFIX)]
            if not source or not object_name.startswith(source + "/"):
                raise RuntimeError("archive purge object escapes its task prefix")
            group = archives.setdefault(source, {})
            group.setdefault(object_name, []).append(candidate)
        else:
            normalized = normalized_object(PurePosixPath(object_name).parts)
            groups.setdefault(normalized, []).append(candidate)

    def remote_key(object_name, *, literal=False):
        # Archive keys are literal, including folder markers and doubled '/'s.
        if literal:
            object_bucket, prefix, remote_version = raw_fs.split_path(remote.path)
            if remote_version:
                raise RuntimeError("archive purge remote contains a version selector")
            key = prefix.rstrip("/") + "/" + object_name if prefix else object_name
        else:
            path = remote.fs.join(remote.path, *PurePosixPath(object_name).parts)
            object_bucket, key, _ = raw_fs.split_path(path)
        if object_bucket != bucket or not key:
            raise RuntimeError(f"managed-storage purge escaped its configured bucket: {object_name!r}")
        return key

    deleted, already_absent, retained_unmapped = [], [], []
    # Archive candidates win over generic retirement of the same path: generic
    # candidates must not turn a mapped-version cleanup into an all-version
    # purge that could erase writes made after the archive source snapshot.
    for source, objects in archives.items():
        prefix = remote_key(source + "/", literal=True)
        initial = list_versions(raw_fs, bucket, prefix)
        wanted = {(remote_key(name, literal=True), item["version_id"])
                  for name, candidates in objects.items() for item in candidates}
        present = {(item["Key"], item["VersionId"]): item for item in initial}
        for name, candidates in objects.items():
            key = remote_key(name, literal=True)
            requested = {item["version_id"] for item in candidates}
            existing = sorted(version for version in requested if (key, version) in present)
            for version in existing:
                raw_fs.call_s3("delete_object", Bucket=bucket, Key=key, VersionId=version)
            if existing:
                deleted.append({**candidates[0], "deleted_version_ids": existing})
            else:
                already_absent.append(candidates[0])
        for name in list(groups):
            if name.startswith(source + "/"):
                groups.pop(name)
        # A complete rescan proves all mapped versions are absent, and reports
        # later writes (including brand-new keys) without deleting their bytes.
        remaining = list_versions(raw_fs, bucket, prefix)
        if any((item["Key"], item["VersionId"]) in wanted for item in remaining):
            raise RuntimeError("mapped archive object versions still exist after permanent deletion")
        for item in remaining:
            retained_unmapped.append({
                "pointer": source + ARCHIVE_SUFFIX,
                "object": source + "/" + item["Key"][len(prefix):],
                "version_id": item["VersionId"],
            })

    for object_name, candidates in groups.items():
        key = remote_key(object_name)
        versions = [item for item in list_versions(raw_fs, bucket, key) if item["Key"] == key]
        if not versions:
            already_absent.append(candidates[0])
            continue
        for version in versions:
            raw_fs.call_s3("delete_object", Bucket=bucket, Key=key, VersionId=version["VersionId"])
        if any(item["Key"] == key for item in list_versions(raw_fs, bucket, key)):
            raise RuntimeError(f"managed-storage object versions still exist after permanent deletion: {object_name!r}")
        deleted.append({**candidates[0], "deleted_version_ids": sorted(item["VersionId"] for item in versions)})
    retained_unmapped.sort(key=lambda item: (item["pointer"], item["object"], item["version_id"]))
    return {"mode": "permanent-version-deletion", "remote": remote.name,
            "deleted": deleted, "already_absent": already_absent,
            "retained_unmapped": retained_unmapped}


def main(argv=None):
    from dvc.repo import Repo as DvcRepo

    argv = sys.argv[1:] if argv is None else argv
    repo_path, operation, payload = Path(argv[0]), argv[1], json.loads(argv[2])
    if operation == "list":
        result = []
        for request in payload:
            result.extend(objects_at(repo_path, request.get("revision"), request["pointers"]))
    elif operation == "delete":
        with DvcRepo(str(repo_path)) as repo:
            remote = repo.cloud.get_remote()
            if not remote.fs.version_aware:
                raise RuntimeError(f"configured remote {remote.name!r} is not version-aware")
            raw_fs = remote.fs.fs
            bucket, _, _ = raw_fs.split_path(remote.path)
            if not bucket:
                raise RuntimeError("configured S3 remote does not name a bucket")
            if not raw_fs.is_bucket_versioned(bucket):
                raise RuntimeError(f"S3 bucket {bucket!r} does not have object versioning enabled")
            result = delete_candidates(raw_fs, remote, bucket, payload)
    else:
        raise RuntimeError(f"unknown managed-storage purge operation: {operation!r}")
    print(json.dumps(result, sort_keys=True))
    return result


if __name__ == "__main__":
    main()
