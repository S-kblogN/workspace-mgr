import json
import sys
from pathlib import Path, PurePosixPath

from dvc.repo import Repo as DvcRepo


repo_path = Path(sys.argv[1])
dvc_files = json.loads(sys.argv[2])
missing_metadata = []
missing_versions = []
checked = []


with DvcRepo(str(repo_path)) as dvc_repo:
    default_remote = dvc_repo.cloud.get_remote()
    if not default_remote.fs.version_aware:
        raise RuntimeError(
            f"configured remote {default_remote.name!r} is not version-aware"
        )
    remotes = {default_remote.name: default_remote}

    def remote_for(name):
        remote_name = name or default_remote.name
        if remote_name not in remotes:
            remotes[remote_name] = dvc_repo.cloud.get_remote(remote_name)
        remote = remotes[remote_name]
        if not remote.fs.version_aware:
            raise RuntimeError(
                f"DVC metadata selects non-version-aware remote {remote_name!r} "
                "while exact version verification is required"
            )
        return remote

    def check_entry(object_parts, version_id, remote_name, expected_size, expected_etag):
        object_name = PurePosixPath(*object_parts).as_posix()
        if not version_id or version_id == "null":
            missing_metadata.append(object_name)
            return
        remote = remote_for(remote_name)
        remote_path = remote.fs.join(remote.path, *object_parts)
        try:
            info = remote.fs.fs.info(remote_path, version_id=version_id)
        except (FileNotFoundError, KeyError):
            missing_versions.append(object_name)
            return
        except (AttributeError, TypeError):
            versioned_path = remote.fs.version_path(remote_path, version_id)
            if not remote.fs.exists(versioned_path):
                missing_versions.append(object_name)
                return
            checked.append(object_name)
            return

        actual_version = info.get("VersionId") or info.get("version_id")
        actual_size = info.get("size")
        if actual_size is None:
            actual_size = info.get("Size")
        actual_etag = info.get("ETag") or info.get("etag")
        if isinstance(actual_etag, str):
            actual_etag = actual_etag.strip('"')
        normalized_expected_etag = (
            expected_etag.strip('"') if isinstance(expected_etag, str) else None
        )
        mismatches = []
        if actual_version and actual_version != version_id:
            mismatches.append("version ID")
        if expected_size is not None and actual_size != expected_size:
            mismatches.append("size")
        if normalized_expected_etag and actual_etag and actual_etag != normalized_expected_etag:
            mismatches.append("etag")
        if mismatches:
            missing_versions.append(
                f"{object_name} (mismatched {', '.join(mismatches)})"
            )
            return
        checked.append(object_name)

    for dvc_file in dvc_files:
        stages = list(dvc_repo.stage.collect(str(repo_path / dvc_file)))
        if not stages:
            raise RuntimeError(f"DVC metadata did not define an output: {dvc_file}")
        for stage in stages:
            for out in stage.outs:
                if not out.is_in_repo or not out.can_push:
                    continue
                _, base_parts = out.index_key
                if out.isdir():
                    if out.files is None:
                        missing_metadata.append(PurePosixPath(*base_parts).as_posix())
                        continue
                    for entry in out.files:
                        relpath = entry.get("relpath")
                        if not isinstance(relpath, str):
                            missing_metadata.append(PurePosixPath(*base_parts).as_posix())
                            continue
                        rel = PurePosixPath(relpath)
                        if rel.is_absolute() or ".." in rel.parts:
                            raise RuntimeError(
                                f"invalid path in DVC directory metadata: {relpath!r}"
                            )
                        check_entry(
                            (*base_parts, *rel.parts),
                            entry.get("version_id"),
                            entry.get("remote") or out.remote,
                            entry.get("size"),
                            entry.get("etag") or entry.get("md5"),
                        )
                else:
                    meta = out.meta
                    check_entry(
                        base_parts,
                        meta.version_id if meta else None,
                        (meta.remote if meta else None) or out.remote,
                        meta.size if meta else None,
                        (
                            getattr(meta, "etag", None)
                            or getattr(meta, "md5", None)
                        )
                        if meta
                        else None,
                    )

if missing_metadata:
    raise RuntimeError(
        "DVC push did not record a cloud version ID for: "
        + ", ".join(sorted(set(missing_metadata)))
    )
if missing_versions:
    raise RuntimeError(
        "version-aware DVC object versions are missing or mismatched for: "
        + ", ".join(sorted(set(missing_versions)))
    )

print(
    json.dumps(
        {
            "mode": "version-aware",
            "remote": default_remote.name,
            "checked_objects": sorted(set(checked)),
        },
        sort_keys=True,
    )
)
