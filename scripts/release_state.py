#!/usr/bin/env python3
"""Inspect and validate the declarative workspace-mgr release state."""

from __future__ import annotations

import argparse
import dataclasses
import hashlib
import io
import json
import os
from pathlib import Path
import re
import sys
import tarfile
import tomllib
from typing import Any
from urllib.error import HTTPError
from urllib.parse import quote, urljoin
from urllib.request import Request, urlopen


CRATES_IO = "https://crates.io"
GITHUB_API = "https://api.github.com"
SHA1_RE = re.compile(r"^[0-9a-f]{40}$")


class ReleaseStateError(RuntimeError):
    """A release declaration or remote state is invalid."""


@dataclasses.dataclass(frozen=True)
class Package:
    name: str
    version: str
    prerelease: bool

    @property
    def tag(self) -> str:
        return f"v{self.version}"


@dataclasses.dataclass(frozen=True)
class RegistryState:
    crate_exists: bool
    version_exists: bool
    source_sha: str | None


@dataclasses.dataclass(frozen=True)
class GitHubState:
    tag_sha: str | None
    release_exists: bool
    release_assets: frozenset[str]


@dataclasses.dataclass(frozen=True)
class ReleaseState:
    source_sha: str
    tag_exists: bool
    release_exists: bool
    release_complete: bool
    release_needed: bool


def load_package(repo_root: Path) -> Package:
    with (repo_root / "Cargo.toml").open("rb") as handle:
        manifest = tomllib.load(handle)
    package = manifest.get("package", {})
    name = package.get("name")
    version = package.get("version")
    if not isinstance(name, str) or not name:
        raise ReleaseStateError("Cargo.toml does not declare package.name")
    if not isinstance(version, str) or not version:
        raise ReleaseStateError("Cargo.toml does not declare package.version")
    prerelease = "-" in version.split("+", 1)[0]
    return Package(name=name, version=version, prerelease=prerelease)


def validate_changelog(repo_root: Path, version: str) -> None:
    changelog = (repo_root / "CHANGELOG.md").read_text(encoding="utf-8")
    heading = re.compile(
        rf"^## \[{re.escape(version)}\] - \d{{4}}-\d{{2}}-\d{{2}}$", re.MULTILINE
    )
    if not heading.search(changelog):
        raise ReleaseStateError(
            f"CHANGELOG.md has no dated release heading for {version}"
        )


def release_notes(repo_root: Path, version: str) -> str:
    changelog = (repo_root / "CHANGELOG.md").read_text(encoding="utf-8")
    heading = re.compile(
        rf"^## \[{re.escape(version)}\] - \d{{4}}-\d{{2}}-\d{{2}}\s*$",
        re.MULTILINE,
    )
    match = heading.search(changelog)
    if match is None:
        raise ReleaseStateError(
            f"CHANGELOG.md has no dated release heading for {version}"
        )
    next_heading = re.search(r"^## ", changelog[match.end() :], re.MULTILINE)
    end = match.end() + next_heading.start() if next_heading else len(changelog)
    notes = changelog[match.end() : end].strip()
    if not notes:
        raise ReleaseStateError(f"CHANGELOG.md release {version} has no notes")
    return notes + "\n"


def request_json(
    url: str, *, token: str | None = None, missing_ok: bool = False
) -> dict[str, Any] | None:
    headers = {
        "Accept": "application/vnd.github+json, application/json",
        "User-Agent": "workspace-mgr-release-workflow",
    }
    if token:
        headers["Authorization"] = f"Bearer {token}"
        headers["X-GitHub-Api-Version"] = "2022-11-28"
    request = Request(url, headers=headers)
    try:
        with urlopen(request, timeout=30) as response:
            value = json.load(response)
    except HTTPError as error:
        if error.code == 404 and missing_ok:
            return None
        raise ReleaseStateError(f"GET {url} failed with HTTP {error.code}") from error
    if not isinstance(value, dict):
        raise ReleaseStateError(f"GET {url} returned a non-object response")
    return value


def request_bytes(url: str) -> bytes:
    request = Request(url, headers={"User-Agent": "workspace-mgr-release-workflow"})
    try:
        with urlopen(request, timeout=30) as response:
            return response.read()
    except HTTPError as error:
        raise ReleaseStateError(f"GET {url} failed with HTTP {error.code}") from error


def package_source_sha(crate: Package, version_data: dict[str, Any]) -> str:
    version = version_data.get("version", {})
    checksum = version.get("checksum")
    download_path = version.get("dl_path")
    if not isinstance(checksum, str) or not re.fullmatch(r"[0-9a-f]{64}", checksum):
        raise ReleaseStateError("crates.io returned an invalid package checksum")
    if not isinstance(download_path, str) or not download_path.startswith("/"):
        download_path = (
            f"/api/v1/crates/{quote(crate.name, safe='')}/"
            f"{quote(crate.version, safe='')}/download"
        )
    archive = request_bytes(urljoin(CRATES_IO, download_path))
    actual_checksum = hashlib.sha256(archive).hexdigest()
    if actual_checksum != checksum:
        raise ReleaseStateError(
            f"published crate checksum mismatch: {actual_checksum} != {checksum}"
        )
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as package:
        members = [
            member
            for member in package.getmembers()
            if member.name.endswith("/.cargo_vcs_info.json") and member.isfile()
        ]
        if len(members) != 1:
            raise ReleaseStateError(
                "published crate does not contain exactly one .cargo_vcs_info.json"
            )
        handle = package.extractfile(members[0])
        if handle is None:
            raise ReleaseStateError("could not read published .cargo_vcs_info.json")
        vcs_info = json.load(handle)
    source_sha = vcs_info.get("git", {}).get("sha1")
    if not isinstance(source_sha, str) or not SHA1_RE.fullmatch(source_sha):
        raise ReleaseStateError("published crate has no valid Git source revision")
    return source_sha


def inspect_registry(crate: Package) -> RegistryState:
    encoded_name = quote(crate.name, safe="")
    crate_data = request_json(
        f"{CRATES_IO}/api/v1/crates/{encoded_name}", missing_ok=True
    )
    if crate_data is None:
        return RegistryState(False, False, None)
    version_data = request_json(
        f"{CRATES_IO}/api/v1/crates/{encoded_name}/"
        f"{quote(crate.version, safe='')}",
        missing_ok=True,
    )
    if version_data is None:
        return RegistryState(True, False, None)
    return RegistryState(True, True, package_source_sha(crate, version_data))


def github_tag_sha(
    repository: str, tag: str, token: str, *, api_base: str = GITHUB_API
) -> str | None:
    tag_ref = request_json(
        f"{api_base}/repos/{repository}/git/ref/tags/{quote(tag, safe='')}",
        token=token,
        missing_ok=True,
    )
    if tag_ref is None:
        return None
    obj = tag_ref.get("object", {})
    for _ in range(3):
        object_type = obj.get("type")
        object_sha = obj.get("sha")
        if object_type == "commit":
            if not isinstance(object_sha, str) or not SHA1_RE.fullmatch(object_sha):
                raise ReleaseStateError(f"Git tag {tag} has an invalid commit")
            return object_sha
        if object_type != "tag":
            raise ReleaseStateError(
                f"Git tag {tag} targets unsupported object type {object_type!r}"
            )
        object_url = obj.get("url")
        if not isinstance(object_url, str):
            raise ReleaseStateError(f"Git tag {tag} has no object URL")
        tag_data = request_json(object_url, token=token)
        assert tag_data is not None
        obj = tag_data.get("object", {})
    raise ReleaseStateError(f"Git tag {tag} has excessive tag indirection")


def inspect_github(repository: str, crate: Package, token: str) -> GitHubState:
    tag_sha = github_tag_sha(repository, crate.tag, token)
    release = request_json(
        f"{GITHUB_API}/repos/{repository}/releases/tags/{quote(crate.tag, safe='')}",
        token=token,
        missing_ok=True,
    )
    if release is None:
        return GitHubState(tag_sha, False, frozenset())
    assets = release.get("assets", [])
    names = frozenset(
        asset["name"]
        for asset in assets
        if isinstance(asset, dict) and isinstance(asset.get("name"), str)
    )
    return GitHubState(tag_sha, True, names)


def expected_assets(crate: Package) -> frozenset[str]:
    targets = (
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "aarch64-apple-darwin",
    )
    names: set[str] = set()
    for target in targets:
        archive = f"{crate.name}-{crate.version}-{target}.tar.gz"
        names.add(archive)
        names.add(f"{archive}.sha256")
    return frozenset(names)


def derive_release_state(
    crate: Package,
    registry: RegistryState,
    github: GitHubState,
    current_sha: str,
) -> ReleaseState:
    if not SHA1_RE.fullmatch(current_sha):
        raise ReleaseStateError(f"invalid current Git revision: {current_sha}")
    if not registry.version_exists:
        if github.tag_sha is not None or github.release_exists:
            raise ReleaseStateError(
                f"{crate.tag} exists on GitHub before {crate.name} {crate.version} "
                "exists on crates.io"
            )
        return ReleaseState(current_sha, False, False, False, True)
    if registry.source_sha is None:
        raise ReleaseStateError("published registry version has no source revision")
    if github.tag_sha is not None and github.tag_sha != registry.source_sha:
        raise ReleaseStateError(
            f"{crate.tag} points to {github.tag_sha}, but crates.io records "
            f"{registry.source_sha}"
        )
    if github.release_exists and github.tag_sha is None:
        raise ReleaseStateError(
            f"GitHub Release {crate.tag} exists without its Git tag"
        )
    complete = github.release_exists and expected_assets(crate).issubset(
        github.release_assets
    )
    tag_exists = github.tag_sha is not None
    return ReleaseState(
        registry.source_sha,
        tag_exists,
        github.release_exists,
        complete,
        not tag_exists or not complete,
    )


def write_github_outputs(path: Path, values: dict[str, str | bool]) -> None:
    with path.open("a", encoding="utf-8") as output:
        for key, value in values.items():
            rendered = str(value).lower() if isinstance(value, bool) else value
            output.write(f"{key}={rendered}\n")


def inspect_command(args: argparse.Namespace) -> None:
    repo_root = args.repo_root.resolve()
    crate = load_package(repo_root)
    validate_changelog(repo_root, crate.version)
    registry = inspect_registry(crate)
    github = inspect_github(args.repository, crate, args.github_token)
    state = derive_release_state(crate, registry, github, args.github_sha)
    values: dict[str, str | bool] = {
        "name": crate.name,
        "version": crate.version,
        "tag": crate.tag,
        "prerelease": crate.prerelease,
        "crate_exists": registry.crate_exists,
        "version_exists": registry.version_exists,
        "source_sha": state.source_sha,
        "tag_exists": state.tag_exists,
        "release_exists": state.release_exists,
        "release_complete": state.release_complete,
        "release_needed": state.release_needed,
    }
    if args.github_output:
        write_github_outputs(args.github_output, values)
    print(json.dumps(values, sort_keys=True))


def notes_command(args: argparse.Namespace) -> None:
    repo_root = args.repo_root.resolve()
    crate = load_package(repo_root)
    notes = release_notes(repo_root, crate.version)
    args.output.write_text(notes, encoding="utf-8")


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description=__doc__)
    commands = root.add_subparsers(dest="command", required=True)
    inspect = commands.add_parser("inspect", help="inspect desired and remote state")
    inspect.add_argument("--repo-root", type=Path, default=Path.cwd())
    inspect.add_argument("--repository", required=True)
    inspect.add_argument("--github-sha", required=True)
    inspect.add_argument(
        "--github-token", default=os.environ.get("GITHUB_TOKEN"), required=False
    )
    inspect.add_argument(
        "--github-output", type=Path, default=os.environ.get("GITHUB_OUTPUT")
    )
    inspect.set_defaults(func=inspect_command)
    notes = commands.add_parser("notes", help="extract the declared release notes")
    notes.add_argument("--repo-root", type=Path, default=Path.cwd())
    notes.add_argument("--output", type=Path, required=True)
    notes.set_defaults(func=notes_command)
    return root


def main() -> int:
    args = parser().parse_args()
    if args.command == "inspect" and not args.github_token:
        raise ReleaseStateError("a GitHub token is required")
    args.func(args)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except ReleaseStateError as error:
        print(f"release state error: {error}", file=sys.stderr)
        sys.exit(2)
