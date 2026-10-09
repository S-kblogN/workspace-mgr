#!/usr/bin/env python3
"""System-level workspace-mgr E2E test.

The test talks to MinIO through the S3 API and to a bare Git repository through
git-daemon. It intentionally uses the compiled CLI as an opaque executable.
"""

from __future__ import annotations

import fcntl
import hashlib
import json
import os
import shutil
import socket
import subprocess
import sys
import time
import urllib.request
from urllib.parse import urlsplit
from pathlib import Path
from typing import Any, Iterable

import botocore.session
from botocore.config import Config as BotocoreConfig

VERIFIED_STORAGE_MINIMUM_CLI_VERSION = "0.8.7"
TASK_RENAME_STORAGE_MINIMUM_CLI_VERSION = "0.8.11"


class E2EFailure(RuntimeError):
    pass


class Harness:
    def __init__(self) -> None:
        binary = os.environ.get("WORKSPACE_MGR_BIN")
        root = os.environ.get("WORKSPACE_MGR_E2E_ROOT")
        if not binary or not root:
            raise E2EFailure(
                "WORKSPACE_MGR_BIN and WORKSPACE_MGR_E2E_ROOT are required"
            )
        self.binary = Path(binary).resolve()
        self.root = Path(root).resolve()
        if not self.binary.is_file():
            raise E2EFailure(f"workspace-mgr binary does not exist: {self.binary}")
        if self.root.exists():
            raise E2EFailure(f"E2E root must not already exist: {self.root}")
        self.root.mkdir(parents=True)
        self.home = self.root / "home"
        self.home.mkdir()
        self.evidence_path = self.root / "evidence.jsonl"
        self.sequence = 0
        self.assertions = 0
        self.git_daemon: subprocess.Popen[str] | None = None
        self.git_daemon_log = None
        self.endpoint = os.environ.get("MINIO_ENDPOINT", "http://127.0.0.1:9000")
        if urlsplit(self.endpoint).hostname not in ("127.0.0.1", "localhost", "::1", "minio"):
            raise E2EFailure("E2E tests require a local CI-owned MinIO endpoint")
        self.bucket = os.environ.get("MINIO_BUCKET", "workspace-mgr-e2e")
        self.access_key = os.environ.get("AWS_ACCESS_KEY_ID", "workspace-mgr-e2e")
        self.secret_key = os.environ.get(
            "AWS_SECRET_ACCESS_KEY", "workspace-mgr-e2e-secret"
        )
        self.region = os.environ.get("AWS_DEFAULT_REGION", "us-east-1")
        self.env = os.environ.copy()
        self.env.update(
            {
                "HOME": str(self.home),
                "XDG_CONFIG_HOME": str(self.root / "xdg"),
                "GIT_CONFIG_NOSYSTEM": "1",
                "GIT_TERMINAL_PROMPT": "0",
                "WORKSPACE_MGR_UPDATE_CHECK_DISABLE": "1",
                "AWS_EC2_METADATA_DISABLED": "true",
                "AWS_MAX_ATTEMPTS": "1",
                "AWS_ACCESS_KEY_ID": self.access_key,
                "AWS_SECRET_ACCESS_KEY": self.secret_key,
                "AWS_DEFAULT_REGION": self.region,
            }
        )
        session = botocore.session.get_session()
        self.s3 = session.create_client(
            "s3",
            endpoint_url=self.endpoint,
            region_name=self.region,
            aws_access_key_id=self.access_key,
            aws_secret_access_key=self.secret_key,
            config=BotocoreConfig(
                signature_version="s3v4",
                s3={"addressing_style": "path"},
                retries={"max_attempts": 1, "mode": "standard"},
            ),
        )
        self.remote: Path | None = None
        self.remote_url = ""
        self.seed: Path | None = None
        self.shared: Path | None = None

    def record(self, kind: str, detail: dict[str, Any]) -> None:
        self.sequence += 1
        entry = {
            "sequence": self.sequence,
            "time": time.time(),
            "kind": kind,
            **detail,
        }
        with self.evidence_path.open("a", encoding="utf-8") as stream:
            stream.write(json.dumps(entry, sort_keys=True) + "\n")

    def check(self, condition: bool, message: str, **state: Any) -> None:
        if not condition:
            self.record("assertion", {"status": "failed", "message": message, **state})
            raise E2EFailure(f"assertion failed: {message}; state={state!r}")
        self.assertions += 1
        self.record("assertion", {"status": "passed", "message": message, **state})

    def section(self, name: str) -> None:
        print(f"\n=== {name} ===", flush=True)
        self.record("section", {"name": name})

    def run(
        self,
        command: Iterable[str | Path],
        *,
        cwd: Path | None = None,
        expected: int | Iterable[int] = 0,
        env: dict[str, str] | None = None,
    ) -> subprocess.CompletedProcess[str]:
        argv = [str(part) for part in command]
        expected_codes = {expected} if isinstance(expected, int) else set(expected)
        process_env = self.env.copy()
        if env:
            process_env.update(env)
        print(f"+ ({cwd or self.root}) {' '.join(argv)}", flush=True)
        result = subprocess.run(
            argv,
            cwd=cwd or self.root,
            env=process_env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.record(
            "command",
            {
                "argv": argv,
                "cwd": str(cwd or self.root),
                "exit_code": result.returncode,
                "stdout": result.stdout,
                "stderr": result.stderr,
            },
        )
        if result.stdout.strip():
            print(result.stdout.rstrip(), flush=True)
        if result.stderr.strip():
            print(result.stderr.rstrip(), file=sys.stderr, flush=True)
        if result.returncode not in expected_codes:
            raise E2EFailure(
                f"command exited {result.returncode}, expected {sorted(expected_codes)}: {argv}"
            )
        return result

    def wm(
        self,
        cwd: Path,
        *args: str,
        expected: int = 0,
        env: dict[str, str] | None = None,
    ) -> dict[str, Any]:
        result = self.run(
            [self.binary, "--format", "json", *args],
            cwd=cwd,
            expected=expected,
            env=env,
        )
        if expected != 0:
            return {"stdout": result.stdout, "stderr": result.stderr}
        try:
            payload = json.loads(result.stdout)
        except json.JSONDecodeError as error:
            raise E2EFailure(f"workspace-mgr returned invalid JSON: {error}") from error
        self.record("workspace-mgr-report", {"args": list(args), "payload": payload})
        return payload

    def git(self, repo: Path, *args: str, expected: int | Iterable[int] = 0):
        return self.run(["git", "-C", repo, *args], cwd=repo, expected=expected)

    def configure_git(self, repo: Path) -> None:
        self.git(repo, "config", "user.name", "workspace-mgr E2E")
        self.git(repo, "config", "user.email", "e2e@example.invalid")

    def wait_for_minio(self) -> None:
        deadline = time.monotonic() + 60
        url = f"{self.endpoint}/minio/health/ready"
        while time.monotonic() < deadline:
            try:
                with urllib.request.urlopen(url, timeout=2) as response:
                    if response.status == 200:
                        self.record("service", {"name": "minio", "status": "ready"})
                        return
            except OSError:
                time.sleep(0.5)
        raise E2EFailure(f"MinIO did not become ready at {url}")

    def setup_s3(self) -> None:
        self.wait_for_minio()
        self.s3.create_bucket(Bucket=self.bucket)
        self.s3.put_bucket_versioning(
            Bucket=self.bucket, VersioningConfiguration={"Status": "Enabled"}
        )
        status = self.s3.get_bucket_versioning(Bucket=self.bucket)
        self.check(status.get("Status") == "Enabled", "S3 bucket versioning enabled")
        self.check(self.list_s3_versions() == [], "S3 bucket starts empty")

    def provision_runtime(self) -> None:
        self.section("native storage setup")
        runtime = self.home / ".local" / "share" / "workspace-mgr" / "storage-3.67.1"
        dry = self.wm(self.root, "setup", "--dry-run")
        self.check(dry["status"] == "dry_run", "setup dry-run verifies native storage")
        self.check(not runtime.exists(), "setup dry-run creates no runtime")
        checked = self.wm(self.root, "setup")
        self.check(checked["status"] == "no_changes", "setup requires no installation")
        self.check(checked["storage_runtime"].startswith("native Rust "), "storage is native Rust")
        self.check(checked["runtime_dir"] == "", "storage has no separate runtime directory")
        self.check(not runtime.exists(), "setup never provisions Python or DVC")
        repeated = self.wm(self.root, "setup")
        self.check(repeated["status"] == "no_changes", "setup is idempotent")

    def list_s3_versions(self) -> list[dict[str, Any]]:
        versions: list[dict[str, Any]] = []
        paginator = self.s3.get_paginator("list_object_versions")
        for page in paginator.paginate(Bucket=self.bucket, Prefix="objects/"):
            for item in page.get("Versions", []):
                versions.append(
                    {
                        "key": item["Key"],
                        "version_id": item["VersionId"],
                        "size": item["Size"],
                        "etag": item["ETag"].strip('"'),
                        "is_latest": item["IsLatest"],
                    }
                )
        versions.sort(key=lambda value: (value["key"], value["version_id"]))
        self.record("s3-state", {"versions": versions})
        return versions

    def s3_version_inventory(self) -> list[dict[str, Any]]:
        """Include tombstones so a read-only diagnostic cannot silently delete them."""
        inventory: list[dict[str, Any]] = []
        paginator = self.s3.get_paginator("list_object_versions")
        for page in paginator.paginate(Bucket=self.bucket, Prefix="objects/"):
            for group in ("Versions", "DeleteMarkers"):
                for item in page.get(group, []):
                    inventory.append(
                        {
                            "key": item["Key"],
                            "version_id": item["VersionId"],
                            "delete_marker": group == "DeleteMarkers",
                            "is_latest": item["IsLatest"],
                            "size": item.get("Size"),
                            "etag": item.get("ETag", "").strip('"'),
                        }
                    )
        inventory.sort(key=lambda value: (value["key"], value["version_id"]))
        self.record("s3-version-inventory", {"versions": inventory})
        return inventory

    def create_s3_delete_marker(self, key: str) -> str:
        self.s3.delete_object(Bucket=self.bucket, Key=key)
        markers = [row for row in self.s3_version_inventory()
                   if row["key"] == key and row["delete_marker"] and row["is_latest"]]
        self.check(len(markers) == 1, "fixture deletion creates one exact latest S3 delete marker", key=key, markers=markers)
        return markers[0]["version_id"]

    def s3_bodies(self) -> list[bytes]:
        bodies = []
        for version in self.list_s3_versions():
            response = self.s3.get_object(
                Bucket=self.bucket,
                Key=version["key"],
                VersionId=version["version_id"],
            )
            bodies.append(response["Body"].read())
        return bodies

    def exact_s3_body(self, key: str, version_id: str) -> bytes:
        response = self.s3.get_object(Bucket=self.bucket, Key=key, VersionId=version_id)
        try:
            return response["Body"].read()
        finally:
            response["Body"].close()

    def s3_version_for_body(self, expected: bytes) -> dict[str, Any]:
        matches = []
        for version in self.list_s3_versions():
            response = self.s3.get_object(
                Bucket=self.bucket,
                Key=version["key"],
                VersionId=version["version_id"],
            )
            if response["Body"].read() == expected:
                matches.append(version)
        self.check(
            len(matches) == 1,
            "exactly one S3 object version contains the expected payload",
            matches=matches,
            size=len(expected),
        )
        return matches[0]

    @staticmethod
    def free_port() -> int:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
            listener.bind(("127.0.0.1", 0))
            return int(listener.getsockname()[1])

    def start_git_server(self) -> None:
        git_root = self.root / "git-server"
        git_root.mkdir()
        self.remote = git_root / "remote.git"
        self.run(["git", "init", "--bare", self.remote], cwd=git_root)
        self.run(
            ["git", "--git-dir", self.remote, "symbolic-ref", "HEAD", "refs/heads/main"],
            cwd=git_root,
        )
        (self.remote / "git-daemon-export-ok").write_text("", encoding="utf-8")
        port = self.free_port()
        self.remote_url = f"git://127.0.0.1:{port}/remote.git"
        log_path = self.root / "git-daemon.log"
        self.git_daemon_log = log_path.open("w", encoding="utf-8")
        self.git_daemon = subprocess.Popen(
            [
                "git",
                "daemon",
                "--verbose",
                "--reuseaddr",
                "--export-all",
                "--enable=receive-pack",
                f"--base-path={git_root}",
                "--listen=127.0.0.1",
                f"--port={port}",
                str(git_root),
            ],
            cwd=git_root,
            env=self.env,
            text=True,
            stdout=self.git_daemon_log,
            stderr=subprocess.STDOUT,
        )
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            if self.git_daemon.poll() is not None:
                raise E2EFailure("git daemon exited during startup")
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=1):
                    self.record(
                        "service",
                        {"name": "git-daemon", "status": "ready", "url": self.remote_url},
                    )
                    return
            except OSError:
                time.sleep(0.2)
        raise E2EFailure("git daemon did not become ready")

    def setup_repository(self) -> None:
        self.start_git_server()
        self.seed = self.root / "seed"
        self.run(["git", "init", "-b", "main", self.seed], cwd=self.root)
        self.configure_git(self.seed)
        (self.seed / "README.md").write_text("# Virtual workspace\n", encoding="utf-8")
        self.git(self.seed, "add", "README.md")
        self.git(self.seed, "commit", "-m", "Create virtual workspace")
        self.git(self.seed, "remote", "add", "origin", self.remote_url)
        self.git(self.seed, "push", "-u", "origin", "main")
        self.shared = self.root / "shared"
        self.run(["git", "clone", self.remote_url, self.shared], cwd=self.root)
        self.configure_git(self.shared)
        self.check(
            self.git(self.shared, "remote", "get-url", "origin").stdout.strip().startswith(
                "git://"
            ),
            "workspace clone uses the network Git server",
        )

    def remote_ref(self, branch: str) -> str | None:
        result = self.run(
            ["git", "ls-remote", self.remote_url, f"refs/heads/{branch}"],
            cwd=self.root,
        )
        line = result.stdout.strip()
        network_oid = line.split()[0] if line else None
        assert self.remote is not None
        direct = self.run(
            ["git", "--git-dir", self.remote, "rev-parse", "--verify", f"refs/heads/{branch}"],
            cwd=self.root,
            expected=(0, 128),
        )
        direct_oid = direct.stdout.strip() if direct.returncode == 0 else None
        self.check(network_oid == direct_oid, "network and bare Git refs agree", branch=branch)
        self.record("git-ref", {"branch": branch, "oid": network_oid})
        return network_oid

    def remote_path_exists(self, oid: str, path: str) -> bool:
        assert self.remote is not None
        result = self.run(
            ["git", "--git-dir", self.remote, "cat-file", "-e", f"{oid}:{path}"],
            cwd=self.root,
            expected=(0, 128),
        )
        return result.returncode == 0

    def remote_file(self, oid: str, path: str) -> str:
        assert self.remote is not None
        return self.run(
            ["git", "--git-dir", self.remote, "show", f"{oid}:{path}"], cwd=self.root
        ).stdout

    def install_rejecting_hook(self) -> Path:
        assert self.remote is not None
        hook = self.remote / "hooks" / "pre-receive"
        hook.write_text(
            "#!/bin/sh\n"
            "if test -f \"$GIT_DIR/workspace-mgr-e2e-reject\"; then\n"
            "  echo 'workspace-mgr E2E intentional rejection' >&2\n"
            "  exit 1\n"
            "fi\n"
            "exit 0\n",
            encoding="utf-8",
        )
        hook.chmod(0o755)
        return self.remote / "workspace-mgr-e2e-reject"

    def merge_branch_to_main(self, branch: str) -> str:
        assert self.seed is not None
        self.git(self.seed, "fetch", "origin", branch)
        target = self.git(self.seed, "rev-parse", "FETCH_HEAD").stdout.strip()
        self.git(self.seed, "push", "origin", f"{target}:refs/heads/main")
        self.check(self.remote_ref("main") == target, "server main fast-forwarded", branch=branch)
        return target

    def assert_shared_head(self, expected_oid: str | None = None) -> None:
        assert self.shared is not None
        branch = self.git(self.shared, "branch", "--show-current").stdout.strip()
        self.check(branch == "main", "shared checkout remains on main", branch=branch)
        if expected_oid:
            oid = self.git(self.shared, "rev-parse", "main").stdout.strip()
            self.check(oid == expected_oid, "shared main has expected object ID", oid=oid)

    @staticmethod
    def document_task(task: Path) -> None:
        """Record something in the task's own files, which publication requires
        before a deliverable task may publish substantive content. The record is
        a file of its own rather than an addition to the README, because the
        fixed policy keeps a task README out of the chronological-log role."""
        (task / "record.md").write_text(
            "# Record\n\nThis scenario publishes content, so the task records it here.\n",
            encoding="utf-8",
        )

    def initialize_workspace(self) -> None:
        assert self.shared is not None
        self.section("manage, native configuration, instructions, and doctor")
        common = (
            "manage",
            "--s3-url",
            f"s3://{self.bucket}/objects",
            "--s3-endpoint-url",
            self.endpoint,
        )
        collision = self.root / "first-manage-collision"
        self.run(["git", "clone", self.remote_url, collision], cwd=self.root)
        existing_agents = "# Existing repository policy\n\nPreserve this file.\n"
        (collision / "AGENTS.md").write_text(existing_agents, encoding="utf-8")
        rejected_collision = self.wm(collision, *common, expected=2)
        self.check(
            "reserved workspace-mgr scaffold paths" in rejected_collision["stderr"],
            "manage refuses repository-owned instructions before any conversion",
        )
        self.check(
            (collision / "AGENTS.md").read_text(encoding="utf-8") == existing_agents
            and not (collision / ".workspace-mgr.toml").exists()
            and not (collision / ".dvc").exists(),
            "a refused management operation preserves repository content",
        )

        ignore_repo = self.root / "first-manage-existing-ignore"
        self.run(["git", "clone", self.remote_url, ignore_repo], cwd=self.root)
        existing_ignore = "/build\n*.log\n!keep.log\n"
        (ignore_repo / ".gitignore").write_text(existing_ignore, encoding="utf-8")
        ignore_dry = self.wm(ignore_repo, *common, "--dry-run")
        self.check(
            ignore_dry["status"] == "dry_run"
            and (ignore_repo / ".gitignore").read_text(encoding="utf-8") == existing_ignore
            and not (ignore_repo / ".workspace-mgr" / "repository.gitignore").exists(),
            "manage previews repository ignore adoption without writing",
        )
        self.wm(ignore_repo, *common)
        self.check(
            (ignore_repo / ".workspace-mgr" / "repository.gitignore").read_text(encoding="utf-8") == existing_ignore
            and existing_ignore in (ignore_repo / ".gitignore").read_text(encoding="utf-8"),
            "manage preserves every existing ignore rule and negation automatically",
        )

        unsupported_legacy = self.root / "unsupported-legacy-config"
        self.run(["git", "clone", self.remote_url, unsupported_legacy], cwd=self.root)
        (unsupported_legacy / ".dvc").mkdir()
        old_config = "[future]\n    unsupported = true\n"
        (unsupported_legacy / ".dvc" / "config").write_text(old_config, encoding="utf-8")
        self.wm(unsupported_legacy, *common, expected=2)
        self.check(
            (unsupported_legacy / ".dvc" / "config").read_text(encoding="utf-8") == old_config
            and not (unsupported_legacy / ".workspace-mgr.toml").exists()
            and not (unsupported_legacy / "AGENTS.md").exists(),
            "unsupported import features fail before changing legacy or native scaffolds",
        )

        dry = self.wm(self.shared, *common, "--dry-run")
        self.check(dry["status"] == "dry_run", "manage dry-run reports planned scaffolding")
        self.check(not (self.shared / ".workspace-mgr.toml").exists(), "manage dry-run writes no configuration")
        self.check(not (self.shared / "AGENTS.md").exists(), "manage dry-run writes no bootstrap")
        initialized = self.wm(self.shared, *common)
        self.check(initialized["status"] == "managed", "repository management succeeds")
        config_text = (self.shared / ".workspace-mgr.toml").read_text(encoding="utf-8")
        bootstrap = (self.shared / "AGENTS.md").read_text(encoding="utf-8")
        root_ignore = (self.shared / ".gitignore").read_text(encoding="utf-8")
        self.check(
            root_ignore.startswith("# Generated by workspace-mgr.")
            and "\n.DS_Store\n" in root_ignore
            and "\nnode_modules/\n" in root_ignore
            and "\n/.workspace-mgr/local/\n" in root_ignore,
            "manage generates repository ignore rules with one native private-state directory",
        )
        self.check(
            (self.shared / ".workspace-mgr" / "local" / "repository.lock").is_file()
            and not (self.shared / ".git" / "workspace-mgr").exists(),
            "private repository state lives in the ignored checkout directory",
        )
        self.check(
            all(not (self.shared / path).exists() for path in (".dvc", ".dvcignore", ".gitattributes")),
            "fresh native repositories generate no external-engine scaffolds",
        )
        self.check("# Repository rules imported" not in root_ignore, "an absent module leaves no import section")
        ignore_module = self.shared / ".workspace-mgr" / "repository.gitignore"
        ignore_module.parent.mkdir(parents=True, exist_ok=True)
        ignore_module.write_text("/vendor/\n", encoding="utf-8")
        imported = self.wm(self.shared, "manage")
        root_ignore = (self.shared / ".gitignore").read_text(encoding="utf-8")
        self.check(
            imported["status"] == "managed"
            and "\n# Repository rules imported from .workspace-mgr/repository.gitignore.\n/vendor/\n" in root_ignore,
            "manage imports the repository ignore module verbatim",
        )
        self.check(
            self.git(self.shared, "check-ignore", "--", "vendor/thing.txt", expected=(0, 1)).returncode == 0,
            "imported repository ignore rules take effect",
        )
        template_repo = self.root / "missing-config-owned-bootstrap"
        self.run(["git", "clone", self.remote_url, template_repo], cwd=self.root)
        (template_repo / "AGENTS.md").write_text(bootstrap, encoding="utf-8")
        self.wm(template_repo, *common)
        self.check(
            (template_repo / "AGENTS.md").read_text(encoding="utf-8") == bootstrap
            and (template_repo / ".workspace-mgr.toml").is_file(),
            "manage recognizes its existing bootstrap and recovers missing configuration",
        )
        module = self.shared / ".workspace-mgr" / "instructions" / "repository.md"
        module.parent.mkdir(parents=True)
        module.write_text("# Repository policy\n\nPreserve this repository-specific rule.\n", encoding="utf-8")
        self.check("workspace-mgr instructions" in bootstrap, "thin AGENTS bootstrap installed")
        self.check(self.remote_url not in config_text, "repository Git URL is not embedded in policy")
        self.check("[git]" in config_text and "[s3]" in config_text, "one root config owns Git and S3 facts")
        self.check('minimum_cli_version = "0.8.1"' in config_text, "native metadata gates incompatible older clients")
        self.check(f"s3://{self.bucket}/objects" in config_text and self.endpoint in config_text,
                   "tracked native configuration owns storage URL and endpoint")
        for forbidden in ("schema_version", "required_cli", "profile", "[publication]", "[tasks]", "[review]", "[storage]", "[agent]", "branch_prefix", "auto_s3_above_bytes"):
            self.check(forbidden not in config_text, "public config contains no strategy switch", forbidden=forbidden)
        repeated = self.wm(self.shared, "manage")
        self.check(repeated["status"] == "no_changes", "manage is idempotent")

        (self.shared / "AGENTS.md").write_text("# Legacy or locally edited bootstrap\n", encoding="utf-8")
        (self.shared / ".gitignore").write_text(root_ignore + "# hand edited root ignore rules\n", encoding="utf-8")
        drift_report = json.loads(self.wm(self.shared, "doctor", expected=2)["stdout"])
        self.check(
            any(check["name"] == "repository-scaffold" and check["status"] == "error"
                and "AGENTS.md" in check["detail"] and ".gitignore" in check["detail"]
                for check in drift_report["checks"]),
            "doctor reports drift in every native scaffold",
        )
        repaired = self.wm(self.shared, "manage")
        self.check(repaired["status"] == "managed", "manage repairs scaffold drift")
        self.check((self.shared / "AGENTS.md").read_text(encoding="utf-8") == bootstrap,
                   "management restores the current bootstrap")
        self.check((self.shared / ".gitignore").read_text(encoding="utf-8") == root_ignore,
                   "management restores the generated ignore file including its module")

        (self.shared / ".gitignore").write_text("/secrets.env\n!keep.log\n", encoding="utf-8")
        foreign_doctor = json.loads(self.wm(self.shared, "doctor", expected=2)["stdout"])
        self.check(any(check["name"] == "repository-scaffold" and "workspace-mgr manage" in check["detail"]
                       for check in foreign_doctor["checks"]), "doctor points at automatic ignore adoption")
        self.wm(self.shared, "manage")
        self.check(ignore_module.read_text(encoding="utf-8") == "/vendor/\n\n/secrets.env\n!keep.log\n",
                   "automatic reconciliation preserves existing module rules and foreign root negations")
        ignore_module.write_text("/vendor/\n", encoding="utf-8")
        self.wm(self.shared, "manage")
        root_ignore = (self.shared / ".gitignore").read_text(encoding="utf-8")

        (self.shared / "refresh-update.txt").write_text("old refresh value\n", encoding="utf-8")
        (self.shared / "refresh-delete.txt").write_text("delete during refresh\n", encoding="utf-8")
        self.git(self.shared, "add", "-A")
        staged = self.git(self.shared, "diff", "--cached", "--name-only").stdout.splitlines()
        self.check(".workspace-mgr/local/credentials.toml" not in staged, "local storage credentials stay private")
        self.check(".workspace-mgr/repository.gitignore" in staged
                   and ".workspace-mgr/instructions/repository.md" in staged
                   and not any(path.startswith(".workspace-mgr/local/") for path in staged),
                   "shared configuration is staged while private state stays ignored")
        self.git(self.shared, "commit", "-m", "Manage native workspace")
        self.git(self.shared, "push", "origin", "main")
        self.check(self.remote_ref("main") is not None, "managed main exists on Git server")
        config = self.wm(self.shared, "config", "show")
        self.check(set(config) == {"minimum_cli_version", "git", "s3"}, "config exposes repository facts and compatibility requirement")
        self.check(config["minimum_cli_version"] == "0.8.1", "config reports the native compatibility requirement")
        self.check(config["git"]["remote"] == "origin" and config["git"]["branch"] == "main", "config resolves Git topology")
        self.check(config["s3"]["url"] == f"s3://{self.bucket}/objects", "config resolves native storage location")

        for topic in (
            "all",
            "model",
            "core",
            "task",
            "publish",
            "artifacts",
            "storage",
            "shared-checkout",
            "infrastructure",
            "repository",
        ):
            document = self.wm(self.shared, "instructions", topic)
            self.check(document["topic"] == topic, "instruction topic renders", topic=topic)
            self.check(len(document["policy_hash"]) == 64, "instruction policy hash is complete", topic=topic)
        all_instructions = self.wm(self.shared, "instructions")
        model_heading = all_instructions["markdown"].find("# How this workspace works")
        rules_heading = all_instructions["markdown"].find("# Effective repository instructions")
        self.check(
            0 <= model_heading < rules_heading,
            "management model precedes effective operational rules",
        )
        self.check(
            "one writable conversation (chat) = one task" in all_instructions["markdown"],
            "instructions explain the conversation-task-branch-PR relationship",
        )
        self.check(
            "general-purpose collaborator" in all_instructions["markdown"],
            "instructions explain the workspace purpose before its mechanics",
        )
        self.check(
            "The user controls" in all_instructions["markdown"]
            and "draft PR" in all_instructions["markdown"]
            and "before every writable-task turn" in all_instructions["markdown"],
            "instructions fix agent PR ownership and user merge authority",
        )
        core = self.wm(self.shared, "instructions", "core")["markdown"]
        self.check(
            "workspace-mgr manage" in core
            and "preserves existing repository ignore rules" in core
            and "migrates supported legacy storage metadata in one recoverable transaction" in core
            and "deterministic scaffold reconciliation" not in all_instructions["markdown"],
            "scaffold details are available on demand rather than globally",
        )
        storage_rules = self.wm(self.shared, "instructions", "storage")["markdown"]
        self.check(
            "collaboration and control plane" in storage_rules
            and "artifact and data plane" in storage_rules
            and "small-s3-boundary" in storage_rules
            and "small-s3-boundary" not in all_instructions["markdown"],
            "preserved placement policy is loaded only when relevant",
        )
        repository_rules = self.wm(self.shared, "instructions", "repository")["markdown"]
        self.check(
            "Preserve this repository-specific rule" in repository_rules
            and "Preserve this repository-specific rule" not in all_instructions["markdown"]
            and "instructions repository" in all_instructions["markdown"],
            "global instructions index the accessible repository-owned module",
        )
        self.check(
            "They do not change the fixed task, storage, publication, or review policy"
            in repository_rules,
            "repository-specific content cannot redefine workspace strategy",
        )
        self.check(
            "workspace-mgr archive --help" in all_instructions["markdown"]
            and "many broken links" not in all_instructions["markdown"]
            and "manually audit and repair" not in all_instructions["markdown"],
            "global instructions route operations without unconditional relocation reminders",
        )
        human = self.run([self.binary, "instructions"], cwd=self.shared)
        self.check("Effective repository instructions" in human.stdout, "human instructions render")

        doctor = self.wm(self.shared, "doctor")
        self.check(doctor["status"] == "ok", "doctor accepts full virtual environment")
        self.check(
            all(check["status"] == "ok" for check in doctor["checks"]),
            "every doctor check passes",
            checks=doctor["checks"],
        )
        engine = next(check for check in doctor["checks"] if check["name"] == "managed-storage-runtime")
        self.check("native Rust " in engine["detail"], "doctor verifies the built-in native engine")
        self.check(str(self.home) not in engine["detail"], "doctor omits private authentication paths")

    def create_and_publish_task(self) -> tuple[str, Path, str]:
        assert self.shared is not None
        self.section("task scaffolding, scope planning, and Git publication")
        task_id = "20260829-180000-e2e-flow"
        branch = "codex/e2e-flow"
        dry = self.wm(
            self.shared,
            "task",
            "create",
            "e2e-flow",
            "--title",
            "E2E flow",
            "--purpose",
            "Exercise every managed transaction against virtual services.",
            "--timestamp",
            "20260829-180000",
            "--dry-run",
        )
        self.check(dry["status"] == "dry_run", "task create dry-run succeeds")
        self.check(not (self.shared / task_id).exists(), "task dry-run creates no directory")
        self.check(self.remote_ref(branch) is None, "task dry-run creates no remote branch")

        created = self.wm(
            self.shared,
            "task",
            "create",
            "e2e-flow",
            "--title",
            "E2E flow",
            "--purpose",
            "Exercise every managed transaction against virtual services.",
            "--timestamp",
            "20260829-180000",
        )
        task = self.shared / task_id
        self.check(created["status"] == "created", "task scaffold created")
        self.check(created["branch"] == branch, "task branch follows fixed codex prefix")
        self.check(
            created["review"]["creation_timing"] == "immediate-after-scaffold-publication"
            and created["review"]["synchronization_cadence"] == "before-every-turn-end",
            "deliverable creation reports immediate review and turn-end synchronization",
        )
        self.check(task.joinpath("README.md").is_file(), "task README created")
        self.check(task.joinpath(".workspace-mgr-task.toml").is_file(), "task manifest created")
        self.check(
            "and list them here." in task.joinpath("README.md").read_text(encoding="utf-8"),
            "scaffolded README directory map asks the task to keep its record here",
        )
        readme_before_collision = task.joinpath("README.md").read_bytes()
        collision = self.wm(
            self.shared,
            "task",
            "create",
            "e2e-flow",
            "--title",
            "Replacement title",
            "--purpose",
            "This duplicate must not replace the existing task.",
            "--timestamp",
            "20260829-180000",
            expected=2,
        )
        self.check(collision["stderr"], "duplicate task creation is rejected")
        self.check(
            task.joinpath("README.md").read_bytes() == readme_before_collision,
            "duplicate task creation preserves existing scaffolding",
        )
        self.assert_shared_head()
        base_oid = self.remote_ref("main")
        local_task_oid = self.git(self.shared, "rev-parse", branch).stdout.strip()
        self.check(local_task_oid == base_oid, "unmounted task branch starts at remote main")

        status = self.wm(task, "task", "status")
        self.check(status["task_id"] == task_id, "task status discovers manifest")
        self.check(status["scopes"] == [task_id], "task status reports exact initial scope")
        explicit = self.wm(
            self.shared,
            "task",
            "status",
            "--manifest",
            str(task / ".workspace-mgr-task.toml"),
        )
        self.check(explicit["branch"] == branch, "explicit manifest resolution matches discovery")

        self.git(self.shared, "switch", "-c", "e2e-alternate-checkout")
        wrong_checkout = self.wm(task, "plan", expected=2)
        self.check(
            "--allow-non-shared-head" in wrong_checkout["stderr"],
            "deliverable plan rejects an unexpected shared-checkout branch",
        )
        authorized_checkout = self.wm(
            task,
            "plan",
            "--allow-non-shared-head",
            "--scope-note",
            "The E2E scenario explicitly exercises the exceptional checkout override.",
        )
        self.check(
            authorized_checkout["status"] == "dry_run",
            "explicitly authorized alternate checkout can plan",
        )
        self.git(self.shared, "switch", "main")

        (task / "notes.txt").write_text("task-only content\n", encoding="utf-8")
        git_move_source = task / "move me - α.txt"
        git_move_source.write_text("ordinary Git move payload\n", encoding="utf-8")
        unicode_path = task / "notes with spaces - 结果.txt"
        unicode_path.write_text("Unicode repository path\n", encoding="utf-8")
        (self.shared / "authorized.txt").write_text("authorized root content\n", encoding="utf-8")
        (self.shared / "unrelated.txt").write_text("another active task\n", encoding="utf-8")
        undocumented = self.wm(task, "plan", expected=2)
        self.check(
            "publishes content but documents nothing" in undocumented["stderr"],
            "a content publication is refused while the task documents nothing",
        )
        record = task / "record.md"
        record.write_text(
            "# Record\n\nDecisions, process, tools, and results for this task.\n",
            encoding="utf-8",
        )
        plan = self.wm(task, "plan")
        self.check(plan["status"] == "dry_run", "plan reports task changes")
        self.check(all(path.startswith(task_id + "/") for path in plan["changed_paths"]), "plan stays in task scope")
        self.check(self.remote_ref(branch) is None, "plan does not push target branch")
        self.check(self.list_s3_versions() == [], "plan does not write S3")

        publish_preview = self.wm(
            task,
            "publish",
            "-m",
            "Preview the first publication",
            "--dry-run",
        )
        self.check(publish_preview["status"] == "dry_run", "publish dry-run previews the transaction")
        self.check(self.remote_ref(branch) is None, "publish dry-run creates no remote branch")
        self.check(self.list_s3_versions() == [], "publish dry-run writes no S3 objects")

        rejected = self.wm(
            task,
            "publish",
            "-m",
            "Unauthorized root scope",
            "--include",
            "authorized.txt",
            expected=2,
        )
        self.check("--scope-note" in rejected["stderr"], "additional scope requires an authorization reason")
        self.check(self.remote_ref(branch) is None, "rejected scope does not create branch")

        published = self.wm(
            task,
            "publish",
            "-m",
            "Publish scoped E2E task",
            "--include",
            "authorized.txt",
            "--scope-note",
            "The E2E scenario explicitly authorizes this shared file.",
        )
        commit = published["commit_oid"]
        self.check(published["status"] == "pushed", "task publication succeeds")
        self.check(published["remote_oid"] == commit, "publish verifies remote object ID")
        self.check(self.remote_ref(branch) == commit, "network Git server has published branch")
        self.check(self.remote_path_exists(commit, f"{task_id}/notes.txt"), "task file exists in remote tree")
        self.check(
            self.remote_path_exists(commit, f"{task_id}/notes with spaces - 结果.txt"),
            "Unicode and spaces survive network Git publication",
        )
        self.check(self.remote_path_exists(commit, "authorized.txt"), "authorized extra scope exists in remote tree")
        self.check(self.remote_path_exists(commit, f"{task_id}/record.md"), "the task record exists in remote tree")
        self.check(not self.remote_path_exists(commit, "unrelated.txt"), "unrelated overlay is absent from remote tree")
        message = self.run(
            ["git", "--git-dir", self.remote, "show", "-s", "--format=%B", commit],
            cwd=self.root,
        ).stdout
        self.check("Scope-Authorization: authorized.txt" in message, "commit records scope authorization")
        self.check(
            f"Workspace-Task: {task_id}" in message,
            "commit records the task identity that owns its remote branch",
        )
        self.check(published["review"]["pull_request"] == "required", "deliverable review handoff requires one PR")
        self.check(published["review"]["managed_by"] == "agent", "deliverable review handoff assigns the agent")
        self.check(published["review"]["merge_authority"] == "user", "deliverable review handoff reserves merge for user")
        self.assert_shared_head(base_oid)

        remote_before_move = self.remote_ref(branch)
        versions_before_move = self.list_s3_versions()
        moved_git = self.wm(
            task,
            "move",
            f"{task_id}/move me - α.txt",
            f"{task_id}/renamed/结果.txt",
        )
        self.check(moved_git["placements"][0]["target"] == "git", "ordinary Git move preserves placement")
        self.check(self.remote_ref(branch) == remote_before_move, "ordinary Git move is local-only")
        self.check(self.list_s3_versions() == versions_before_move, "ordinary Git move writes no S3 object")
        moved_git_publish = self.wm(task, "publish", "-m", "Publish ordinary Git move")
        moved_git_oid = moved_git_publish["commit_oid"]
        self.check(
            not self.remote_path_exists(moved_git_oid, f"{task_id}/move me - α.txt")
            and self.remote_path_exists(moved_git_oid, f"{task_id}/renamed/结果.txt"),
            "ordinary Git move is represented exactly in the remote tree",
        )

        self.check(
            [warning["code"] for warning in moved_git_publish.get("warnings", [])]
            == ["task-record-unchanged"],
            "a publication that changes content without the task record warns",
        )
        record.write_text(
            "# Record\n\nDecisions, process, tools, and results for this task.\n"
            "Renamed the Git artifact to its final path.\n",
            encoding="utf-8",
        )
        recorded = self.wm(task, "plan")
        self.check("warnings" not in recorded, "recording the work clears the warning")
        self.wm(task, "publish", "-m", "Publish the updated task record")

        no_changes = self.wm(task, "plan")
        self.check(no_changes["status"] == "no_changes", "post-publish plan is clean")

        # A rule only this checkout carries hides task content from every other
        # clone, so the transaction refuses before placement or upload.
        hidden = task / "search_log1.txt"
        hidden.write_text("per-run search log\n", encoding="utf-8")
        exclude_path = Path(
            self.git(self.shared, "rev-parse", "--git-path", "info/exclude").stdout.strip()
        )
        if not exclude_path.is_absolute():
            exclude_path = self.shared / exclude_path
        exclude_path.parent.mkdir(parents=True, exist_ok=True)
        exclude_path.write_text(f"{task_id}/search_log1.txt\n", encoding="utf-8")
        machine_local = self.wm(task, "plan", expected=2)
        self.check(
            "only an ignore rule this publication does not carry hides"
            in machine_local["stderr"]
            and f'"{task_id}/search_log1.txt"' in machine_local["stderr"]
            and '".git/info/exclude"' in machine_local["stderr"],
            "plan refuses content hidden only by a machine-local ignore rule",
        )

        # `git status --ignored` collapses a directory whose every entry is
        # ignored, and a file-level rule does not match the directory itself, so
        # the rule has to be resolved one level down or the refusal misses the
        # bulk case it exists for.
        collapsed = task / "per-run-logs"
        collapsed.mkdir()
        for index in (1, 2):
            (collapsed / f"run-{index}.log").write_text("per-run log\n", encoding="utf-8")
        exclude_path.write_text("*.log\n", encoding="utf-8")
        listed = self.git(
            self.shared, "status", "--ignored", "--short", "--", task_id
        ).stdout
        self.check(
            f"!! {task_id}/per-run-logs/" in listed,
            "the fixture exercises the collapsed ignored-directory listing",
        )
        collapsed_refusal = self.wm(task, "plan", expected=2)
        self.check(
            f'"{task_id}/per-run-logs/run-1.log"' in collapsed_refusal["stderr"]
            and 'by rule "*.log"' in collapsed_refusal["stderr"],
            "a wholly ignored directory is resolved to the rule that hides it",
        )
        for index in (1, 2):
            (collapsed / f"run-{index}.log").unlink()
        collapsed.rmdir()
        exclude_path.write_text(f"{task_id}/search_log1.txt\n", encoding="utf-8")
        self.wm(task, "plan", expected=2)
        exclude_path.write_text("", encoding="utf-8")
        (task / ".gitignore").write_text("search_log1.txt\n", encoding="utf-8")
        carried = self.wm(task, "plan")
        self.check(
            f"{task_id}/search_log1.txt" in carried.get("ignored_paths", []),
            "the same rule carried in the task's own ignore file publishes cleanly",
        )
        self.wm(task, "publish", "-m", "Publish the task ignore rule")
        repository_lock = self.shared / ".workspace-mgr" / "local" / "repository.lock"
        repository_lock.parent.mkdir(parents=True, exist_ok=True)
        with repository_lock.open("a+", encoding="utf-8") as locked:
            fcntl.flock(locked.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            blocked = self.wm(task, "plan", expected=2)
            self.check(
                "repository operation is running" in blocked["stderr"],
                "a second process cannot enter a repository transaction",
            )
            fcntl.flock(locked.fileno(), fcntl.LOCK_UN)
        self.check(self.wm(task, "plan")["status"] == "no_changes", "transaction resumes after lock release")
        return task_id, task, branch

    def create_and_publish_infrastructure_task(self) -> None:
        assert self.shared is not None
        self.section("infrastructure task on shared main and review handoff")
        branch = "codex/infra-e2e-policy"
        private_config = self.shared / ".workspace-mgr" / "local" / "credentials.toml"
        private_config.write_text("# Virtual private storage state.\n", encoding="utf-8")
        dry = self.wm(
            self.shared,
            "task",
            "create",
            "e2e-policy",
            "--kind",
            "infrastructure",
            "--title",
            "E2E shared policy",
            "--purpose",
            "Exercise scoped repository-wide publication on main.",
            "--scope",
            "e2e-shared-policy.md",
            "--scope",
            "e2e-infra-assets",
            "--scope-note",
            "The E2E scenario authorizes this repository-wide policy file.",
            "--dry-run",
        )
        self.check(dry["status"] == "dry_run", "infrastructure dry-run succeeds")
        self.check(self.remote_ref(branch) is None, "infrastructure dry-run publishes no branch")

        created = self.wm(
            self.shared,
            "task",
            "create",
            "e2e-policy",
            "--kind",
            "infrastructure",
            "--title",
            "E2E shared policy",
            "--purpose",
            "Exercise scoped repository-wide publication on main.",
            "--scope",
            "e2e-shared-policy.md",
            "--scope",
            "e2e-infra-assets",
            "--scope-note",
            "The E2E scenario authorizes this repository-wide policy file.",
        )
        worktree = Path(created["path"])
        manifest = str(created["manifest"])
        main_before = self.git(worktree, "rev-parse", "HEAD").stdout
        index_before = self.git(worktree, "ls-files", "--stage").stdout
        self.check(created["kind"] == "infrastructure", "infrastructure kind is explicit")
        self.check(
            created["review"]["creation_timing"] == "after-first-scoped-publication"
            and created["review"]["synchronization_cadence"] == "before-every-turn-end",
            "infrastructure creation reports first-publication review and turn-end synchronization",
        )
        self.check(worktree == self.shared, "infrastructure uses the shared main checkout")
        self.check(Path(created["manifest"]).is_file(), "infrastructure manifest is private state")
        self.check(
            not (self.shared / "infra-e2e-policy").exists(),
            "infrastructure task creates no repository task directory",
        )
        self.check(
            self.git(worktree, "branch", "--show-current").stdout.strip() == "main",
            "infrastructure creation leaves the shared checkout on main",
        )
        self.check(private_config.read_text(encoding="utf-8") == "# Virtual private storage state.\n",
                   "infrastructure creation preserves private storage configuration")
        status = self.wm(worktree, "task", "status", "--manifest", manifest)
        self.check(
            status["scopes"] == ["e2e-infra-assets", "e2e-shared-policy.md"],
            "infrastructure scopes are exact",
        )

        (worktree / "e2e-shared-policy.md").write_text("isolated policy\n", encoding="utf-8")
        infra_assets = worktree / "e2e-infra-assets"
        infra_assets.mkdir()
        infra_payload = b"infrastructure managed payload\n"
        (infra_assets / "data.bin").write_bytes(infra_payload)
        (worktree / "outside-scope.txt").write_text("must not publish\n", encoding="utf-8")
        scoped = self.wm(worktree, "plan", "--manifest", manifest)
        self.check("outside-scope.txt" not in scoped["changed_paths"],
                   "infrastructure plan excludes another task's overlay")
        infra_placement = self.wm(
            worktree,
            "storage",
            "set",
            "--manifest",
            manifest,
            "e2e-infra-assets/data.bin",
            "--to",
            "s3",
            "--reason",
            "Exercise private managed storage from shared main.",
        )
        self.check(infra_placement["status"] == "updated", "infrastructure content can select S3")
        self.check(infra_placement["remote_writes"] is False, "infrastructure placement is local-only")
        infra_status = self.wm(
            worktree,
            "storage",
            "status",
            "--manifest",
            manifest,
            "e2e-infra-assets/data.bin",
        )
        self.check(
            infra_status["placements"][0]["target"] == "s3"
            and infra_status["placements"][0]["basis"] == "explicit",
            "infrastructure storage status resolves its private task identity",
        )
        plan = self.wm(worktree, "plan", "--manifest", manifest)
        requirement = plan.get("repository_requirement")
        self.check(requirement is not None
                   and requirement["path"] == ".workspace-mgr.toml"
                   and requirement["change"] == "raise"
                   and requirement["minimum_cli_version"] == VERIFIED_STORAGE_MINIMUM_CLI_VERSION,
                   "infrastructure schema 2 publication plans its required compatibility gate")
        self.check(
            all(
                path in {"e2e-shared-policy.md", requirement["path"]} or path.startswith("e2e-infra-assets/")
                for path in plan["changed_paths"]
            ),
            "infrastructure plan contains only declared shared paths and its required compatibility gate",
        )
        self.check(
            "e2e-infra-assets/data.bin.wm-storage.json" in plan["changed_paths"],
            "infrastructure plan includes managed-storage metadata",
        )
        published = self.wm(worktree, "publish", "--manifest", manifest, "-m", "Publish E2E shared policy")
        oid = published["commit_oid"]
        self.check(self.remote_ref(branch) == oid, "infrastructure branch is published")
        self.check(
            self.remote_path_exists(oid, "e2e-shared-policy.md"),
            "infrastructure path exists in the published tree",
        )
        self.check(f'minimum_cli_version = "{VERIFIED_STORAGE_MINIMUM_CLI_VERSION}"'
                   in self.remote_file(oid, ".workspace-mgr.toml"),
                   "infrastructure publication gates clients unable to read schema 2")
        self.check(
            self.remote_path_exists(oid, "e2e-infra-assets/data.bin.wm-storage.json")
            and not self.remote_path_exists(oid, "e2e-infra-assets/data.bin"),
            "infrastructure publication stores metadata in Git and payload in S3",
        )
        self.check(infra_payload in self.s3_bodies(), "infrastructure payload reaches versioned S3")
        self.check(published["review"]["pull_request"] == "required", "review handoff requires one PR")
        self.check(published["review"]["initial_state"] == "draft", "review handoff starts draft")
        self.check(published["review"]["managed_by"] == "agent", "review handoff assigns the agent")
        self.check(published["review"]["merge_authority"] == "user", "review handoff reserves merge for user")
        self.check(
            self.git(worktree, "rev-parse", "HEAD").stdout == main_before
            and self.git(worktree, "ls-files", "--stage").stdout == index_before
            and (worktree / "outside-scope.txt").read_text() == "must not publish\n",
            "infrastructure publication preserves main, its index, and unrelated overlays",
        )
        self.check(self.wm(worktree, "plan", "--manifest", manifest)["status"] == "no_changes", "infrastructure plan ends clean")
        (worktree / "e2e-shared-policy.md").unlink()
        removed = self.wm(worktree, "publish", "--manifest", manifest, "-m", "Remove E2E shared policy")
        removed_oid = removed["commit_oid"]
        self.check(
            removed["changed_paths"] == ["e2e-shared-policy.md"],
            "infrastructure deletion publishes only its missing file scope",
        )
        self.check(
            not self.remote_path_exists(removed_oid, "e2e-shared-policy.md"),
            "published infrastructure deletion removes the remote path",
        )
        self.check(
            self.git(worktree, "rev-parse", "HEAD").stdout == main_before
            and self.git(worktree, "ls-files", "--stage").stdout == index_before,
            "infrastructure scope deletion preserves shared main and index",
        )
        self.check(
            self.wm(worktree, "plan", "--manifest", manifest)["status"] == "no_changes",
            "missing published infrastructure scope remains a clean plan",
        )
        infra_discarded_key = self.s3_version_for_body(infra_payload)["key"]
        discard_preview = self.wm(worktree, "task", "discard", "--manifest", manifest, "--dry-run")
        self.check(discard_preview["status"] == "dry_run", "infrastructure discard previews cleanup")
        self.check(
            discard_preview["review"]["provider_state_verified_by_cli"] is False
            and "close" in discard_preview["review"]["required_before_confirm"],
            "infrastructure discard hands PR closure to the agent",
        )
        self.check(
            all(row["action"] == "restore" for row in discard_preview["local_actions"]),
            "infrastructure discard restores only declared shared scopes",
        )
        self.check(
            any(
                item["object"] == "e2e-infra-assets/data.bin"
                for item in discard_preview["s3_purge"]["queued"]
            ),
            "infrastructure discard reports S3 paths queued for permanent deletion",
        )
        discarded = self.wm(
            self.shared,
            "task",
            "discard",
            "--manifest",
            str(created["manifest"]),
            "--confirm",
            "infra-e2e-policy",
        )
        self.check(discarded["status"] == "discarded", "infrastructure discard succeeds after review handoff")
        self.check(
            discarded.get("cleanup_warnings", []) == [],
            "infrastructure discard completes without cleanup warnings",
        )
        self.check(worktree.is_dir() and not Path(manifest).exists(),
                   "infrastructure discard preserves shared main and removes private task state")
        self.check((worktree / "outside-scope.txt").read_text() == "must not publish\n",
                   "infrastructure discard preserves another task's overlay")
        (worktree / "outside-scope.txt").unlink()
        self.check(self.remote_ref(branch) is None, "infrastructure discard deletes its network branch")
        local_branch = self.git(
            self.shared,
            "rev-parse",
            "--verify",
            branch,
            expected=(0, 128),
        )
        self.check(local_branch.returncode == 128, "infrastructure discard deletes its local branch")
        self.check(
            all(item["key"] != infra_discarded_key for item in self.list_s3_versions())
            and discarded["s3_purge"]["status"] == "cleanup_pending"
            and {(row["object"], row["version_id"])
                 for row in discarded["s3_purge"]["pending"]}
                == self.rename_retention["pending_before_merge"]
            and discarded["s3_purge"].get("pending_prefixes", []) == [self.rename_retention["source"]],
            "infrastructure discard purges its content and preserves only the pending rename history",
        )
        self.assert_shared_head()

    def exercise_native_storage(self, task_id: str, task: Path, branch: str) -> None:
        assert self.shared is not None
        self.section("Git/S3 placement, failure atomicity, hydrate, move, and reset")
        data = task / "data.bin"
        git_to_s3 = task / "notes.txt"
        bundle = task / "bundle"
        bundle.mkdir()
        v1 = b"single-file version one\n"
        bundle_v1_a = b"bundle alpha version one\n"
        bundle_v1_b = b"bundle beta version one\n"
        bundle_bulk = b"x" * 1_048_576
        data.write_bytes(v1)
        (bundle / "alpha.txt").write_bytes(bundle_v1_a)
        (bundle / "beta.txt").write_bytes(bundle_v1_b)
        (bundle / "bulk.bin").write_bytes(bundle_bulk)
        remote_before = self.remote_ref(branch)
        dry = self.wm(
            task,
            "storage",
            "set",
            "--dry-run",
            f"{task_id}/data.bin",
            f"{task_id}/notes.txt",
            f"{task_id}/bundle",
            "--to",
            "s3",
            "--reason",
            "Retained E2E binary data.",
        )
        self.check(dry["status"] == "dry_run", "S3 placement dry-run succeeds")
        self.check(dry["remote_writes"] is False, "placement dry-run reports no remote writes")
        dry_placements = {item["path"]: item for item in dry["placements"]}
        self.check(
            dry_placements[f"{task_id}/data.bin"]["warnings"][0]["code"]
            == "small-s3-boundary",
            "tiny explicit S3 file receives an efficiency warning",
        )
        self.check(
            "warnings" not in dry_placements[f"{task_id}/bundle"]
            and dry_placements[f"{task_id}/bundle"]["payload_bytes"] > 1_048_576,
            "aggregate S3 directory clears the small-boundary warning",
        )
        self.check(not task.joinpath("data.bin.wm-storage.json").exists(), "placement dry-run creates no metadata")
        self.check(not task.joinpath("notes.txt.wm-storage.json").exists(), "Git-to-S3 dry-run creates no metadata")
        self.check(self.remote_ref(branch) == remote_before, "placement dry-run leaves Git remote unchanged")
        self.check(self.list_s3_versions() == [], "placement dry-run leaves S3 empty")

        placed = self.wm(
            task,
            "storage",
            "set",
            f"{task_id}/data.bin",
            f"{task_id}/notes.txt",
            f"{task_id}/bundle",
            "--to",
            "s3",
            "--reason",
            "Retained E2E binary data.",
        )
        self.check(placed["status"] == "updated", "two paths are placed in S3 locally")
        self.check(placed["remote_writes"] is False, "S3 placement performs no remote writes")
        self.check(self.remote_ref(branch) == remote_before, "placement leaves Git remote unchanged")
        self.check(self.list_s3_versions() == [], "placement leaves S3 remote unchanged")
        statuses = self.wm(task, "storage", "status")
        status_paths = {item["path"] for item in statuses["placements"]}
        self.check(
            {f"{task_id}/data.bin", f"{task_id}/notes.txt", f"{task_id}/bundle"}.issubset(status_paths),
            "storage status finds all explicit boundaries alongside Git content",
        )
        self.check(
            all(
                item["target"] == "s3" and item["basis"] == "explicit"
                for item in statuses["placements"]
                if item["path"] in {f"{task_id}/data.bin", f"{task_id}/notes.txt", f"{task_id}/bundle"}
            ),
            "storage status explains explicit S3 placement",
        )
        self.s3.put_bucket_versioning(
            Bucket=self.bucket, VersioningConfiguration={"Status": "Suspended"}
        )
        disabled_doctor = self.wm(self.shared, "doctor", expected=2)
        self.check(
            "does not have object versioning enabled" in disabled_doctor["stdout"],
            "doctor detects disabled S3 bucket versioning",
        )
        disabled_publish = self.wm(
            task,
            "publish",
            "-m",
            "This must fail before an unversioned upload",
            expected=2,
        )
        self.check(
            "object versioning" in disabled_publish["stderr"],
            "publish rejects disabled bucket versioning before upload",
        )
        self.check(self.list_s3_versions() == [], "disabled versioning uploads no S3 object")
        self.s3.put_bucket_versioning(
            Bucket=self.bucket, VersioningConfiguration={"Status": "Enabled"}
        )
        placement_plan = self.wm(task, "plan")
        small_boundaries = {
            decision["boundary"]
            for decision in placement_plan["storage"]["placement"]["decisions"]
            if any(warning["code"] == "small-s3-boundary" for warning in decision.get("warnings", []))
        }
        self.check(
            {f"{task_id}/data.bin", f"{task_id}/notes.txt"}.issubset(small_boundaries)
            and f"{task_id}/bundle" not in small_boundaries,
            "plan surfaces tiny S3 boundaries without warning on an aggregate boundary",
        )
        tracked = self.wm(task, "publish", "-m", "Publish S3 file and directory")
        self.check(tracked["status"] == "pushed", "two S3 boundaries publish atomically")
        verification = tracked["storage"]["s3"]["verification"]
        self.check(verification["mode"] == "version-aware", "exact S3 version verification ran")
        self.check(len(verification["checked_objects"]) >= 3, "each payload object was exactly verified")
        data_pointer = task / "data.bin.wm-storage.json"
        bundle_pointer = task / "bundle.wm-storage.json"
        self.check(data_pointer.is_file() and bundle_pointer.is_file(), "file and directory pointers exist")
        self.check(bool(json.loads(data_pointer.read_text(encoding="utf-8"))["version"]["id"]), "file pointer records S3 version ID")
        self.check(all(entry["version"]["id"] for entry in json.loads(bundle_pointer.read_text(encoding="utf-8"))["entries"]), "directory pointer records S3 version IDs")
        tracked_oid = self.remote_ref(branch)
        assert tracked_oid is not None
        self.check(not self.remote_path_exists(tracked_oid, f"{task_id}/data.bin"), "S3 payload is absent from Git tree")
        self.check(not self.remote_path_exists(tracked_oid, f"{task_id}/notes.txt"), "published Git payload is removed when moved to S3")
        self.check(self.remote_path_exists(tracked_oid, f"{task_id}/notes.txt.wm-storage.json"), "Git-to-S3 transition publishes a pointer")
        self.check(not self.remote_path_exists(tracked_oid, f"{task_id}/bundle"), "S3 directory is absent from Git tree")
        self.check(self.remote_path_exists(tracked_oid, f"{task_id}/data.bin.wm-storage.json"), "file pointer is in Git tree")
        versions_v1 = self.list_s3_versions()
        self.check(len(versions_v1) >= 3, "MinIO contains S3 payload versions")
        self.check(all(item["version_id"] not in ("", "null") for item in versions_v1), "all S3 objects have version IDs")
        self.exercise_doctor_storage(task_id, task, branch)
        beta_key = self.s3_version_for_body(bundle_v1_b)["key"]
        config_before_relocation = (self.shared / ".workspace-mgr.toml").read_bytes()
        relocation = self.wm(self.shared, "manage", "--s3-url", "s3://workspace-mgr-other/objects", expected=2)
        self.check("cannot change the managed S3 location" in relocation["stderr"]
                   and (self.shared / ".workspace-mgr.toml").read_bytes() == config_before_relocation,
                   "committed repository facts prevent storage relocation while boundaries exist")
        changed_config = config_before_relocation.replace(f"s3://{self.bucket}/objects".encode(), b"s3://workspace-mgr-other/objects")
        (self.shared / ".workspace-mgr.toml").write_bytes(changed_config)
        changed_location = self.wm(self.shared, "manage", expected=2)
        self.check("cannot change the managed S3 location" in changed_location["stderr"]
                   and (self.shared / ".workspace-mgr.toml").read_bytes() == changed_config,
                   "editing root config cannot relocate existing boundaries during reconciliation")
        (self.shared / ".workspace-mgr.toml").write_bytes(config_before_relocation)
        restored_config = self.wm(self.shared, "manage")
        self.check(restored_config["status"] == "managed"
                   and [action["path"] for action in restored_config["actions"]] == [".workspace-mgr.toml"]
                   and f'minimum_cli_version = "{VERIFIED_STORAGE_MINIMUM_CLI_VERSION}"' in (self.shared / ".workspace-mgr.toml").read_text(),
                   "restoring native config raises only the schema 2 compatibility requirement")
        self.check(self.wm(self.shared, "manage")["status"] == "no_changes", "restoring native config reconciles without derived files")
        bodies_v1 = self.s3_bodies()
        for payload in (v1, bundle_v1_a, bundle_v1_b, bundle_bulk):
            self.check(payload in bodies_v1, "S3 contains exact version-one payload", payload=payload.decode().strip())
        self.check(self.wm(task, "plan")["status"] == "no_changes", "published S3 state is clean")
        inherited = self.wm(task, "storage", "status", f"{task_id}/bundle/alpha.txt")
        self.check(
            inherited["placements"][0]["target"] == "s3"
            and inherited["placements"][0]["basis"] == "explicit-ancestor",
            "a descendant inherits its directory S3 boundary",
        )
        remote_before_overlap = self.remote_ref(branch)
        versions_before_overlap = self.list_s3_versions()
        overlap = self.wm(
            task,
            "storage",
            "set",
            f"{task_id}/bundle/alpha.txt",
            "--to",
            "git",
            "--reason",
            "This nested override must be rejected.",
            expected=2,
        )
        self.check("existing placement boundary" in overlap["stderr"], "nested placement is rejected")
        nested_reset = self.wm(
            task,
            "storage",
            "reset",
            f"{task_id}/bundle/alpha.txt",
            expected=2,
        )
        self.check("existing placement boundary" in nested_reset["stderr"], "nested reset is rejected")
        self.check(self.remote_ref(branch) == remote_before_overlap, "nested-boundary guards leave Git unchanged")
        self.check(self.list_s3_versions() == versions_before_overlap, "nested-boundary guards leave S3 unchanged")

        v2 = b"single-file version two\n"
        bundle_v2_a = b"bundle alpha version two\n"
        bundle_v2_c = b"bundle gamma version two\n"
        data.write_bytes(v2)
        (bundle / "alpha.txt").write_bytes(bundle_v2_a)
        (bundle / "beta.txt").unlink()
        (bundle / "gamma.txt").write_bytes(bundle_v2_c)
        pointer_before_plan = data_pointer.read_bytes()
        s3_before_plan = self.list_s3_versions()
        remote_before_plan = self.remote_ref(branch)
        planned = self.wm(task, "plan")
        self.check(planned["status"] == "dry_run", "dirty S3 outputs appear in plan")
        self.check(set(planned["storage"]["s3"]["dirty_files"]) == {f"{task_id}/data.bin.wm-storage.json", f"{task_id}/bundle.wm-storage.json"}, "plan finds both dirty S3 boundaries")
        self.check(data_pointer.read_bytes() == pointer_before_plan, "plan does not rewrite storage metadata")
        self.check(self.list_s3_versions() == s3_before_plan, "plan does not upload new S3 versions")
        self.check(self.remote_ref(branch) == remote_before_plan, "plan does not move Git branch")

        bad_credentials = {
            "AWS_ACCESS_KEY_ID": "invalid-e2e-key",
            "AWS_SECRET_ACCESS_KEY": "invalid-e2e-secret",
        }
        failed_s3 = self.wm(
            task,
            "publish",
            "-m",
            "This must fail before Git publication",
            expected=2,
            env=bad_credentials,
        )
        self.check(failed_s3["stderr"], "S3 authentication failure is reported")
        self.check(
            "invalid-e2e-key" not in failed_s3["stderr"]
            and "invalid-e2e-secret" not in failed_s3["stderr"],
            "S3 authentication failure does not echo credentials",
        )
        self.check(self.remote_ref(branch) == remote_before_plan, "S3 failure leaves remote Git ref unchanged")
        self.check(self.git(self.shared, "rev-parse", branch).stdout.strip() == remote_before_plan, "S3 failure leaves local target ref unchanged")
        self.check(self.list_s3_versions() == s3_before_plan, "S3 authentication failure uploads no object")

        published_v2 = self.wm(task, "publish", "-m", "Publish S3 version two")
        self.check(published_v2["status"] == "pushed", "retry after S3 failure succeeds")
        self.check(published_v2["storage"]["s3"]["verification"]["mode"] == "version-aware", "retry verifies exact S3 versions")
        versions_v2 = self.list_s3_versions()
        self.check(len(versions_v2) > len(versions_v1), "S3 retains additional immutable versions")
        self.check(
            all(item["key"] != beta_key for item in versions_v2),
            "publishing a directory deletion permanently removes every S3 version at the deleted child path",
        )
        bodies_v2 = self.s3_bodies()
        for payload in (v2, bundle_v2_a, bundle_v2_c):
            self.check(payload in bodies_v2, "S3 contains exact version-two payload", payload=payload.decode().strip())

        reject_flag = self.install_rejecting_hook()
        reject_flag.write_text("reject\n", encoding="utf-8")
        v3 = b"single-file version three\n"
        bundle_v3_a = b"bundle alpha version three\n"
        data.write_bytes(v3)
        (bundle / "alpha.txt").write_bytes(bundle_v3_a)
        remote_before_reject = self.remote_ref(branch)
        local_before_reject = self.git(self.shared, "rev-parse", branch).stdout.strip()
        versions_before_reject = self.list_s3_versions()
        rejected_git = self.wm(
            task,
            "publish",
            "-m",
            "Upload before intentional Git rejection",
            expected=2,
        )
        self.check("rejection" in rejected_git["stderr"].lower() or "rejected" in rejected_git["stderr"].lower(), "Git server rejection is visible")
        self.check(self.remote_ref(branch) == remote_before_reject, "Git rejection leaves remote branch unchanged")
        local_after_reject = self.git(self.shared, "rev-parse", branch).stdout.strip()
        self.check(local_after_reject != local_before_reject, "failed Git push retains retryable local commit")
        self.check(local_after_reject != remote_before_reject, "local and remote refs expose interrupted publication")
        versions_after_reject = self.list_s3_versions()
        self.check(len(versions_after_reject) > len(versions_before_reject), "S3 data is uploaded before Git publication")
        self.check(v3 in self.s3_bodies() and bundle_v3_a in self.s3_bodies(), "unreferenced retryable S3 versions contain exact payloads")
        reject_flag.unlink()
        retried = self.wm(task, "publish", "-m", "Retry Git publication after rejection")
        self.check(retried["status"] == "pushed", "Git publication retry succeeds")
        self.check(self.remote_ref(branch) == retried["commit_oid"], "retry reconciles local and remote refs")

        cache = self.shared / ".workspace-mgr" / "local" / "cache"
        if cache.exists():
            shutil.rmtree(cache)
        unpublished_edit = b"unpublished local edit that hydrate must preserve\n"
        data.write_bytes(unpublished_edit)
        remote_before_conflict = self.remote_ref(branch)
        versions_before_conflict = self.list_s3_versions()
        conflict = self.wm(
            task,
            "storage",
            "hydrate",
            f"{task_id}/data.bin",
            expected=2,
        )
        self.check("locally changed outputs" in conflict["stderr"], "hydrate rejects a locally modified output")
        self.check(data.read_bytes() == unpublished_edit, "failed hydrate preserves the local modification")
        self.check(self.remote_ref(branch) == remote_before_conflict, "hydrate conflict leaves Git unchanged")
        self.check(self.list_s3_versions() == versions_before_conflict, "hydrate conflict leaves S3 unchanged")
        data.write_bytes(v3)

        remote_before_missing = self.remote_ref(branch)
        data.unlink()
        missing = self.wm(task, "publish", "-m", "Do not interpret missing data as deletion", expected=2)
        self.check("missing locally" in missing["stderr"], "missing S3 output is rejected")
        self.check(self.remote_ref(branch) == remote_before_missing, "missing output leaves Git remote unchanged")
        self.check(not data.exists(), "failed missing-output publication does not synthesize data")

        if cache.exists():
            shutil.rmtree(cache)
        if bundle.exists():
            shutil.rmtree(bundle)
        dry_hydrate = self.wm(task, "storage", "hydrate", "--dry-run", f"{task_id}/data.bin")
        self.check(dry_hydrate["status"] == "dry_run", "hydrate dry-run reports work")
        self.check(not data.exists(), "hydrate dry-run does not materialize output")
        hydrated_file = self.wm(task, "storage", "hydrate", f"{task_id}/data.bin")
        self.check(hydrated_file["status"] == "hydrated", "targeted hydrate succeeds from empty cache")
        self.check(data.read_bytes() == v3, "targeted hydrate restores exact S3 version")
        self.check(not bundle.exists(), "targeted hydrate does not materialize another boundary")
        hydrated_all = self.wm(task, "storage", "hydrate")
        self.check(hydrated_all["status"] == "hydrated", "scope-wide hydrate succeeds")
        self.check((bundle / "alpha.txt").read_bytes() == bundle_v3_a, "directory hydrate restores latest alpha")
        self.check((bundle / "gamma.txt").read_bytes() == bundle_v2_c, "directory hydrate preserves unchanged file")
        self.check(not (bundle / "beta.txt").exists(), "directory hydrate preserves a published deletion")

        old_path = f"{task_id}/data.bin"
        new_path = f"{task_id}/moved.bin"
        move_dry = self.wm(task, "move", "--dry-run", old_path, new_path)
        self.check(move_dry["status"] == "dry_run", "move dry-run succeeds")
        self.check(data.exists() and not task.joinpath("moved.bin").exists(), "move dry-run changes no files")
        versions_before_move = self.list_s3_versions()
        old_data_key = self.s3_version_for_body(v3)["key"]
        remote_before_move = self.remote_ref(branch)
        moved = self.wm(task, "move", old_path, new_path)
        self.check(moved["status"] == "updated", "S3 boundary moves locally")
        self.check(moved["remote_writes"] is False, "move reports no remote writes")
        self.check(self.remote_ref(branch) == remote_before_move, "move leaves Git remote unchanged")
        self.check(self.list_s3_versions() == versions_before_move, "move leaves S3 unchanged")
        moved_output = task / "moved.bin"
        moved_pointer = task / "moved.bin.wm-storage.json"
        self.check(not data.exists() and not data_pointer.exists(), "old S3 boundary is removed")
        self.check(moved_output.read_bytes() == v3 and moved_pointer.is_file(), "moved S3 boundary preserves payload")
        moved_publish = self.wm(task, "publish", "-m", "Publish moved S3 boundary")
        self.check(moved_publish["status"] == "pushed", "moved S3 boundary publishes")
        moved_oid = self.remote_ref(branch)
        assert moved_oid is not None
        self.check(self.remote_path_exists(moved_oid, f"{task_id}/moved.bin.wm-storage.json"), "moved pointer exists in remote Git tree")
        self.check(not self.remote_path_exists(moved_oid, f"{task_id}/data.bin.wm-storage.json"), "old pointer is absent from remote Git tree")
        self.check(
            all(item["key"] != old_data_key for item in self.list_s3_versions()),
            "publishing a move permanently removes every S3 version at the old path",
        )

        reset_dry = self.wm(task, "storage", "reset", "--dry-run", f"{task_id}/moved.bin")
        self.check(reset_dry["status"] == "dry_run", "placement reset dry-run succeeds")
        self.check(reset_dry["placements"][0]["target"] == "s3", "published S3 placement stays stable after reset")
        self.check(moved_pointer.is_file() and moved_output.is_file(), "reset dry-run preserves boundary")
        remote_before_reset = self.remote_ref(branch)
        reset = self.wm(task, "storage", "reset", f"{task_id}/moved.bin")
        self.check(reset["status"] == "updated", "reset returns path to automatic placement")
        self.check(reset["remote_writes"] is False, "reset performs no remote writes")
        self.check(self.remote_ref(branch) == remote_before_reset, "reset leaves Git remote unchanged")
        self.check(moved_pointer.exists(), "reset preserves published S3 metadata locally")
        self.check(moved_output.read_bytes() == v3, "reset preserves output")
        reset_status = self.wm(task, "storage", "status", f"{task_id}/moved.bin")
        self.check(reset_status["placements"][0]["target"] == "s3", "status keeps the published S3 placement")
        moved_key = self.s3_version_for_body(v3)["key"]
        to_git = self.wm(
            task,
            "storage",
            "set",
            f"{task_id}/moved.bin",
            "--to",
            "git",
            "--reason",
            "Exercise the explicit S3-to-Git transition.",
        )
        self.check(to_git["status"] == "updated", "explicit placement moves published S3 content to Git")
        self.check(not moved_pointer.exists(), "explicit Git placement removes S3 metadata locally")
        reset_publish = self.wm(task, "publish", "-m", "Publish explicit Git placement")
        self.check(reset_publish["status"] == "pushed", "Git placement publishes")
        untracked_oid = self.remote_ref(branch)
        assert untracked_oid is not None
        self.check(self.remote_path_exists(untracked_oid, f"{task_id}/moved.bin"), "reset output becomes ordinary Git content")
        self.check(not self.remote_path_exists(untracked_oid, f"{task_id}/moved.bin.wm-storage.json"), "reset S3 metadata is absent from Git")
        self.check(
            all(item["key"] != moved_key for item in self.list_s3_versions()),
            "publishing an S3-to-Git transition permanently removes every S3 version at the old object path",
        )
        self.check(self.remote_path_exists(untracked_oid, f"{task_id}/bundle.wm-storage.json"), "other S3 boundary remains stored")
        self.check(self.wm(task, "plan")["status"] == "no_changes", "storage lifecycle ends cleanly")

    def exercise_doctor_storage(self, task_id: str, task: Path, branch: str) -> None:
        assert self.shared is not None
        self.section("read-only task-scoped doctor with exact S3 layout and metadata")
        pointer = task / "data.bin.wm-storage.json"
        original_pointer = pointer.read_bytes()
        data = task / "data.bin"
        original_data = data.read_bytes()
        injected: list[tuple[str, str]] = []

        def snapshot() -> dict[str, Any]:
            files = {}
            for root in (task, self.shared / ".workspace-mgr" / "local"):
                if root.exists():
                    for path in root.rglob("*"):
                        if path.is_file():
                            files[str(path.relative_to(self.shared))] = hashlib.sha256(path.read_bytes()).hexdigest()
            return {
                "files": files,
                "git_status": self.git(self.shared, "status", "--porcelain=v1", "--untracked-files=all").stdout,
                "task_ref": self.remote_ref(branch),
                "main_ref": self.remote_ref("main"),
                "versions": self.s3_version_inventory(),
            }

        def inspect(*selectors: str, expected: int = 0, cwd: Path | None = None) -> dict[str, Any]:
            before = snapshot()
            output = self.wm(cwd or self.shared, "doctor", *selectors, expected=expected)
            report = output if expected == 0 else json.loads(output["stdout"])
            self.check(snapshot() == before, "doctor preserves payloads, metadata, private state, Git refs, S3 versions, and markers")
            integrity = next(check for check in report["checks"] if check["name"] == "managed-storage-integrity")
            self.check(integrity["status"] == ("ok" if expected == 0 else "error"), "doctor storage check controls its failing exit status")
            return report

        def put(key: str, body: bytes) -> str:
            response = self.s3.put_object(Bucket=self.bucket, Key=key, Body=body)
            version = response["VersionId"]
            injected.append((key, version))
            return version

        def mark_deleted(key: str) -> None:
            injected.append((key, self.create_s3_delete_marker(key)))

        def cleanup() -> None:
            for key, version in reversed(injected):
                self.s3.delete_object(Bucket=self.bucket, Key=key, VersionId=version)
            injected.clear()

        selected = inspect(task_id, "--repo", str(self.shared), cwd=self.root)
        self.check(selected["storage"]["issues"] == [] and selected["storage"]["expected_objects"] == 5,
                   "selected doctor verifies every standalone and directory object")
        self.check(selected["storage"]["verified_version_objects"] == 5
                   and selected["storage"]["streamed_objects"] == 0
                   and selected["storage"]["streamed_bytes"] == 0,
                   "schema 2 doctor verifies exact-version bindings without downloading payloads")
        self.check(inspect()["storage"]["issues"] == [], "doctor without a selector audits all tasks")

        try:
            wrong_path = f"objects/{task_id}/wrong-directory/data.bin"
            retired_path = f"objects/{task_id}/retired.bin"
            marker_only_path = f"objects/{task_id}/marker-only.bin"
            put(wrong_path, original_data)
            put(retired_path, b"retired remote-only payload\n")
            mark_deleted(retired_path)
            marker_payload = put(marker_only_path, b"payload removed beneath a retained marker\n")
            mark_deleted(marker_only_path)
            self.s3.delete_object(Bucket=self.bucket, Key=marker_only_path, VersionId=marker_payload)
            injected.remove((marker_only_path, marker_payload))
            stale = inspect(task_id, expected=2)
            unexpected = {issue["path"] for issue in stale["storage"]["issues"] if issue["code"] == "unexpected-object"}
            self.check({path.removeprefix("objects/") for path in (wrong_path, retired_path, marker_only_path)}.issubset(unexpected),
                       "doctor finds misplaced objects, retained deleted history, and keys with only a delete marker", issues=stale["storage"]["issues"])
        finally:
            cleanup()

        try:
            metadata = json.loads(original_pointer)
            metadata["size"] += 1
            metadata["version"]["verification"]["size"] = metadata["size"]
            pointer.write_text(json.dumps(metadata) + "\n", encoding="utf-8")
            mismatch = inspect(task_id, expected=2)
            self.check(any(issue["code"] == "remote-size-mismatch" for issue in mismatch["storage"]["issues"]),
                       "doctor compares metadata size with the exact remote version")
            metadata = json.loads(original_pointer)
            metadata["version"]["etag"] = "0" * 32
            pointer.write_text(json.dumps(metadata) + "\n", encoding="utf-8")
            mismatch = inspect(task_id, expected=2)
            self.check(any(issue["code"] == "remote-etag-mismatch" for issue in mismatch["storage"]["issues"]),
                       "doctor compares metadata ETag with the exact remote version")
            metadata = json.loads(original_pointer)
            metadata["version"]["verification"]["checksum"]["digest"] = "0" * 64
            pointer.write_text(json.dumps(metadata) + "\n", encoding="utf-8")
            mismatch = inspect(task_id, expected=2)
            self.check(any(issue["code"] == "local-remote-bytes-mismatch" for issue in mismatch["storage"]["issues"])
                       and mismatch["storage"]["streamed_objects"] == 0,
                       "schema 2 doctor rejects a conflicting SHA256 binding without payload downloads")
            metadata = json.loads(original_pointer)
            metadata["schema_version"] = 1
            del metadata["version"]["verification"]
            metadata["checksum"]["digest"] = "0" * 32
            pointer.write_text(json.dumps(metadata) + "\n", encoding="utf-8")
            mismatch = inspect(task_id, expected=2)
            self.check(any(issue["code"] == "remote-content-mismatch" for issue in mismatch["storage"]["issues"]),
                       "schema 1 doctor retains remote-byte verification when version, ETag, and size match metadata")
        finally:
            pointer.write_bytes(original_pointer)

        try:
            data_key = f"objects/{task_id}/data.bin"
            put(data_key, b"unexpected latest content at the correct path\n")
            latest = inspect(task_id, expected=2)
            self.check(any(issue["code"] == "remote-latest-mismatch" for issue in latest["storage"]["issues"]),
                       "doctor detects a newer S3 version although the metadata's exact version still exists")
            mark_deleted(data_key)
            deleted = inspect(task_id, expected=2)
            self.check(any(issue["code"] == "remote-latest-mismatch" for issue in deleted["storage"]["issues"]),
                       "doctor detects a current delete marker hiding a metadata-bound remote object")
        finally:
            cleanup()

        try:
            data.write_bytes(b"locally modified materialized payload\n")
            local = inspect(task_id, expected=2)
            self.check(any(issue["code"] == "local-content-mismatch" for issue in local["storage"]["issues"]),
                       "doctor compares materialized local bytes with metadata")
            self.check(any(issue["code"] == "local-remote-bytes-mismatch" for issue in local["storage"]["issues"]),
                       "doctor also compares materialized raw bytes with their verified exact-version checksum")
            data.write_bytes(original_data)
            extra = task / "bundle" / "unlisted.bin"
            extra.write_bytes(b"unlisted local directory entry\n")
            layout = inspect(task_id, expected=2)
            self.check(any(issue["code"] == "local-layout-mismatch" for issue in layout["storage"]["issues"]),
                       "doctor detects materialized directory entries absent from metadata")
        finally:
            data.write_bytes(original_data)
            (task / "bundle" / "unlisted.bin").unlink(missing_ok=True)

        try:
            data.unlink()
            self.check(inspect(task_id)["storage"]["issues"] == [], "doctor verifies remote bytes when a local payload is not hydrated")
            self.check(not data.exists(), "doctor leaves an unmaterialized local payload absent")
        finally:
            data.write_bytes(original_data)

        isolated_id = "20260829-183000-doctor-isolation"
        created = self.wm(self.shared, "task", "create", "doctor-isolation", "--title", "Doctor isolation",
                          "--purpose", "Prove selected doctor ignores other tasks' remote corruption.", "--timestamp", "20260829-183000")
        isolated_task = Path(created["path"])
        isolated_manifest = Path(created["manifest"])
        try:
            put(f"objects/{isolated_id}/unexpected.bin", b"another task's remote-only content\n")
            self.check(inspect(task_id)["storage"]["issues"] == [], "selected doctor excludes another task's S3 corruption")
            self.check(inspect(isolated_id, expected=2)["storage"]["issues"], "selecting the corrupt task diagnoses its own S3 objects")
            self.check(inspect(expected=2, cwd=task)["storage"]["issues"], "doctor without a selector audits every task even from inside a healthy task")
        finally:
            cleanup()
        self.wm(isolated_task, "task", "discard", "--dry-run")
        self.wm(self.shared, "task", "discard", "--manifest", str(isolated_manifest), "--confirm", isolated_id)
        self.check(inspect()["storage"]["issues"] == [], "exact fixture cleanup restores a healthy all-task diagnosis")

    def rename_published_task(self) -> None:
        assert self.shared is not None
        self.section("published task slug rename with versioned S3 content")
        task_id = "20260829-185000-initial-topic"
        renamed_id = "20260829-185000-current-topic"
        branch = "codex/initial-topic"
        created = self.wm(
            self.shared,
            "task",
            "create",
            "initial-topic",
            "--title",
            "Rename lifecycle",
            "--purpose",
            "Verify a published task can follow the conversation topic.",
            "--timestamp",
            "20260829-185000",
        )
        task = Path(created["path"])
        self.document_task(task)
        payload = b"published payload preserved across task rename\n"
        artifact = task / "artifact.bin"
        artifact.write_bytes(payload)
        self.wm(
            task,
            "storage",
            "set",
            f"{task_id}/artifact.bin",
            "--to",
            "s3",
            "--reason",
            "Exercise S3 identity migration during a task rename.",
        )
        first = self.wm(task, "publish", "-m", "Publish the initial task topic")
        first_oid = first["remote_oid"]
        old_artifact_key = self.s3_version_for_body(payload)["key"]
        self.git(self.shared, "push", "origin", f"{first_oid}:refs/tags/e2e-rename-original-history")
        self.check(
            self.remote_path_exists(first_oid, f"{task_id}/artifact.bin.wm-storage.json"),
            "initial published tree contains the S3 pointer",
        )
        reset = self.wm(
            task,
            "storage",
            "reset",
            f"{task_id}/artifact.bin",
        )
        self.check(
            reset["placements"][0]["target"] == "s3"
            and reset["placements"][0]["basis"] == "published-history",
            "reset removes the explicit choice and inherits published S3 placement",
        )
        versions_before = self.list_s3_versions()
        original = [row for row in self.s3_version_inventory()
                    if row["key"].startswith(f"objects/{task_id}/")]
        remote_before = self.remote_ref(branch)

        preview = self.wm(task, "task", "rename", "current-topic", "--dry-run")
        self.check(
            preview["status"] == "dry_run"
            and preview["task_id"] == task_id
            and preview["branch"] == branch
            and preview["review"]["head_branch_unchanged"] is True,
            "rename preview preserves stable task and review identity",
        )
        self.check(task.is_dir(), "rename preview leaves the old task directory present")
        self.check(
            self.remote_ref(branch) == remote_before,
            "rename preview leaves the network branch unchanged",
        )
        self.check(
            self.list_s3_versions() == versions_before,
            "rename preview leaves versioned S3 unchanged",
        )

        renamed = self.wm(task, "task", "rename", "current-topic")
        renamed_task = self.shared / renamed_id
        self.check(
            renamed["status"] == "renamed"
            and renamed["task_id"] == task_id
            and renamed["branch"] == branch
            and renamed["remote_writes"] is False,
            "rename changes the mutable slug but not task or branch identity",
        )
        self.check(
            not task.exists()
            and renamed_task.is_dir()
            and (renamed_task / "artifact.bin").read_bytes() == payload
            and (renamed_task / "artifact.bin.wm-storage.json").is_file(),
            "rename moves the complete local task including S3 metadata and output",
        )
        self.check(
            self.remote_ref(branch) == remote_before,
            "local rename leaves the network branch unchanged",
        )
        self.check(
            self.list_s3_versions() == versions_before,
            "local rename leaves versioned S3 unchanged",
        )
        status = self.wm(
            renamed_task,
            "storage",
            "status",
            f"{renamed_id}/artifact.bin",
        )
        self.check(
            status["placements"][0]["target"] == "s3"
            and status["placements"][0]["basis"] == "published-history",
            "renamed content retains its published S3 placement",
        )
        plan = self.wm(renamed_task, "plan")
        self.check(
            plan["status"] == "dry_run"
            and set(plan["scopes"]) == {task_id, renamed_id},
            "rename plan owns both the published old path and current path",
        )
        self.check(
            f"{task_id}/artifact.bin.wm-storage.json" in plan["changed_paths"]
            and f"{renamed_id}/artifact.bin.wm-storage.json" in plan["changed_paths"],
            "rename plan records old-pointer deletion and new-pointer addition",
        )
        self.check(
            self.remote_ref(branch) == remote_before
            and self.list_s3_versions() == versions_before,
            "rename plan performs no Git or S3 remote writes",
        )

        published = self.wm(
            renamed_task,
            "publish",
            "-m",
            "Publish the renamed task topic",
        )
        renamed_oid = published["remote_oid"]
        self.check(
            self.remote_ref(branch) == renamed_oid,
            "renamed task advances the same network branch",
        )
        self.check(
            not self.remote_path_exists(renamed_oid, task_id)
            and self.remote_path_exists(
                renamed_oid, f"{renamed_id}/artifact.bin.wm-storage.json"
            ),
            "renamed publication removes the old tree and publishes the new tree",
        )
        receipt = json.loads((renamed_task / ".workspace-mgr-archive.json").read_text())
        original_ids = {(row["key"].removeprefix("objects/"), row["version_id"])
                        for row in original}
        copied_ids = {(row["destination_object"], row["destination_version_id"])
                      for row in receipt["versions"]}
        copied = [row for row in self.s3_version_inventory()
                  if row["key"].startswith(f"objects/{renamed_id}/")]
        self.check(
            receipt["migration_kind"] == "task-rename" and receipt["status"] == "copied"
            and receipt["task_id"] == task_id and receipt["source"] == task_id
            and receipt["destination"] == renamed_id
            and {(row["source_object"], row["source_version_id"])
                 for row in receipt["versions"]} == original_ids
            and {(row["key"].removeprefix("objects/"), row["version_id"])
                 for row in copied} == copied_ids
            and len(copied) == len(original),
            "rename copies every exact source generation with no extra unchanged-payload uploads",
        )
        copied_by_id = {(row["key"].removeprefix("objects/"), row["version_id"]): row
                        for row in copied}
        for row in receipt["versions"]:
            actual = copied_by_id[(row["destination_object"], row["destination_version_id"])]
            self.check(actual["delete_marker"] == row["delete_marker"],
                       "rename preserves the exact copied generation kind")
            if not row["delete_marker"]:
                self.check(self.exact_s3_body("objects/" + row["source_object"], row["source_version_id"])
                           == self.exact_s3_body("objects/" + row["destination_object"], row["destination_version_id"])
                           == payload,
                           "rename server copies retain the original exact opaque payload bytes")
        self.check(
            [row for row in self.s3_version_inventory()
             if row["key"].startswith(f"objects/{task_id}/")] == original
            and published["storage"]["purge"]["status"] == "cleanup_pending"
            and task_id in published["storage"]["purge"]["pending_prefixes"],
            "rename preserves complete source history until its copied receipt reaches shared main",
        )
        pending_doctor = json.loads(self.wm(self.shared, "doctor", renamed_id, expected=2)["stdout"])
        self.check(
            pending_doctor["storage"]["issues"]
            and all(issue["code"] == "unexpected-object" and issue["path"] == f"{task_id}/artifact.bin"
                    for issue in pending_doctor["storage"]["issues"])
            and {issue["version"] for issue in pending_doctor["storage"]["issues"]}
                == {row["version_id"] for row in original},
            "doctor diagnoses every pending old-source generation while the copied current path verifies",
        )
        stale_version = self.s3.put_object(Bucket=self.bucket, Key=old_artifact_key, Body=payload)["VersionId"]
        stale_marker = self.create_s3_delete_marker(old_artifact_key)
        try:
            stale = json.loads(self.wm(self.shared, "doctor", renamed_id, expected=2)["stdout"])
            self.check(any(issue["code"] == "unexpected-object" and issue["path"] == f"{task_id}/artifact.bin"
                           for issue in stale["storage"]["issues"]),
                       "current task selection diagnoses leftover versions and markers at its historical pre-rename path")
            old_rows = [row for row in self.s3_version_inventory() if row["key"] == old_artifact_key]
            self.check({row["version_id"] for row in old_rows}
                       == {row["version_id"] for row in original} | {stale_version, stale_marker},
                       "doctor preserves mapped source history and both unreviewed added generations")
        finally:
            self.s3.delete_object(Bucket=self.bucket, Key=old_artifact_key, VersionId=stale_marker)
            self.s3.delete_object(Bucket=self.bucket, Key=old_artifact_key, VersionId=stale_version)
        self.check(
            self.wm(renamed_task, "task", "status")["slug"] == "current-topic",
            "task status reports the current slug separately from stable identity",
        )
        self.check(
            self.wm(renamed_task, "plan")["status"] == "no_changes",
            "renamed publication ends in a clean task state",
        )

        cache = self.shared / ".workspace-mgr" / "local" / "cache"
        if cache.exists():
            shutil.rmtree(cache)
        (renamed_task / "artifact.bin").unlink()
        hydrated = self.wm(
            renamed_task,
            "storage",
            "hydrate",
            f"{renamed_id}/artifact.bin",
        )
        self.check(
            hydrated["status"] == "hydrated"
            and (renamed_task / "artifact.bin").read_bytes() == payload,
            "renamed S3 boundary hydrates its exact published payload",
        )
        renamed_artifact_key = f"objects/{renamed_id}/artifact.bin"
        removed = self.wm(
            renamed_task,
            "remove",
            f"{renamed_id}/artifact.bin",
        )
        self.check(
            removed["status"] == "updated"
            and removed["remote_writes"] is False
            and not (renamed_task / "artifact.bin").exists()
            and not (renamed_task / "artifact.bin.wm-storage.json").exists(),
            "explicit remove deletes a complete S3 boundary locally without touching remotes",
        )
        removed_publish = self.wm(
            renamed_task,
            "publish",
            "-m",
            "Publish permanent artifact deletion",
        )
        self.check(removed_publish["status"] == "pushed", "explicit S3 deletion publishes")
        self.check(
            [row for row in self.s3_version_inventory()
             if row["key"].startswith(f"objects/{task_id}/")] == original
            and [row for row in self.s3_version_inventory()
                 if row["key"].startswith(f"objects/{renamed_id}/")] == copied
            and json.loads((renamed_task / ".workspace-mgr-archive.json").read_text()) == receipt
            and removed_publish["storage"]["purge"]["status"] == "cleanup_pending",
            "removing current payload preserves the exact copied history and pending source retirement",
        )
        self.check(all(self.exact_s3_body(renamed_artifact_key, row["version_id"]) == payload
                       for row in copied if not row["delete_marker"]),
                   "removed current output remains readable through each retained immutable copy")
        self.rename_retention = {
            "source": task_id, "destination": renamed_id, "branch": branch,
            "task": renamed_task, "first_oid": first_oid, "payload": payload,
            "receipt": receipt, "copied": copied, "copied_ids": copied_ids,
            "pending_before_merge": {(row["object"], row["version_id"])
                                     for row in removed_publish["storage"]["purge"]["pending"]},
        }

    def finish_renamed_history(self) -> None:
        assert self.shared is not None
        self.section("merged rename source retirement and retained historical output")
        history = self.rename_retention
        base = self.remote_ref("main")
        head = self.remote_ref(history["branch"])
        self.git(self.seed, "fetch", "origin")
        self.git(self.seed, "checkout", "--detach", base)
        merged_result = self.git(self.seed, "merge", "--no-ff", head,
                                 "-m", "Merge renamed task and its preserved exact history", expected=(0, 1))
        if merged_result.returncode:
            # Both independent tasks can raise the same compatibility floor.
            # Resolve only that one control line, retaining the rename's higher
            # requirement; payload/configuration conflicts remain hard errors.
            conflicts = self.git(self.seed, "diff", "--name-only", "--diff-filter=U").stdout.splitlines()
            main_config = self.remote_file(base, ".workspace-mgr.toml")
            renamed_config = self.remote_file(head, ".workspace-mgr.toml")
            without_floor = lambda text: "".join(line for line in text.splitlines(keepends=True)
                                                if not line.startswith("minimum_cli_version = "))
            self.check(conflicts == [".workspace-mgr.toml"]
                       and without_floor(main_config) == without_floor(renamed_config)
                       and f'minimum_cli_version = "{TASK_RENAME_STORAGE_MINIMUM_CLI_VERSION}"' in renamed_config,
                       "fixture merge resolves only the rename's higher shared CLI requirement")
            self.git(self.seed, "checkout", "--theirs", "--", ".workspace-mgr.toml")
            self.git(self.seed, "add", ".workspace-mgr.toml")
            self.git(self.seed, "commit", "-m", "Merge renamed task with its required CLI floor")
        merged = self.git(self.seed, "rev-parse", "HEAD").stdout.strip()
        for parent in (base, head):
            self.git(self.seed, "merge-base", "--is-ancestor", parent, merged)
        self.git(self.seed, "push", "origin", "HEAD:refs/heads/main")
        self.check(self.remote_ref("main") == merged,
                   "fixture main merge preserves both primary and renamed task histories")
        refreshed = self.wm(self.shared, "refresh")
        self.check(refreshed["new_oid"] == merged
                   and not [row for row in self.s3_version_inventory()
                            if row["key"].startswith(f"objects/{history['source']}/")],
                   "shared receipt publication retires all exact original source generations")
        self.check([row for row in self.s3_version_inventory()
                    if row["key"].startswith(f"objects/{history['destination']}/")] == history["copied"],
                   "source cleanup preserves precisely the receipt-mapped destination history")
        # Copied destination history is intentional retention. This private
        # queue preserves those protected obligations for a later canonical
        # onward archive; unrelated cleanup must still finish completely.
        self.check(refreshed["storage"]["purge"]["status"] == "cleanup_pending"
                   and {(row["object"], row["version_id"])
                        for row in refreshed["storage"]["purge"]["pending"]} == history["copied_ids"]
                   and not refreshed["storage"]["purge"].get("pending_prefixes", []),
                   "only intentionally preserved copied destination identities remain queued")
        audited = self.wm(self.shared, "doctor", history["destination"])
        self.check(audited["storage"]["issues"] == []
                   and audited["storage"]["retained_archive_versions"] == len(history["copied"]),
                   "doctor accepts every exact historical copy after removal of its current output")
        consumer = self.root / "rename-original-history-consumer"
        self.run(["git", "clone", self.remote_url, consumer], cwd=self.root)
        self.configure_git(consumer)
        self.git(consumer, "checkout", "--detach", history["first_oid"])
        self.wm(consumer / history["source"], "storage", "hydrate", f"{history['source']}/artifact.bin")
        self.check((consumer / history["source"] / "artifact.bin").read_bytes() == history["payload"]
                   and not [row for row in self.s3_version_inventory()
                            if row["key"].startswith(f"objects/{history['source']}/")]
                   and [row for row in self.s3_version_inventory()
                        if row["key"].startswith(f"objects/{history['destination']}/")] == history["copied"],
                   "old Git pointers hydrate original bytes without recreating source or copied generations")

    def refresh_and_cross_clone(self, task_id: str, task: Path, branch: str) -> None:
        assert self.shared is not None
        self.section("shared-checkout refresh and independent clone hydration")
        merged_oid = self.merge_branch_to_main(branch)
        assert self.seed is not None
        refresh_source = self.root / "refresh-source"
        self.git(self.seed, "worktree", "add", "--detach", refresh_source, merged_oid)
        self.configure_git(refresh_source)
        (refresh_source / "refresh-update.txt").write_text("new refresh value\n", encoding="utf-8")
        (refresh_source / "refresh-delete.txt").unlink()
        (refresh_source / "refresh-added.txt").write_text("added by remote\n", encoding="utf-8")
        self.git(refresh_source, "add", "-A")
        self.git(refresh_source, "commit", "-m", "Update ordinary Git files for refresh")
        merged_oid = self.git(refresh_source, "rev-parse", "HEAD").stdout.strip()
        self.git(refresh_source, "push", "origin", "HEAD:refs/heads/main")
        self.git(self.seed, "worktree", "remove", "--force", refresh_source)
        original_main = self.git(self.shared, "rev-parse", "main").stdout.strip()
        self.check(original_main != merged_oid, "shared main remains stale before refresh")
        (self.shared / "README.md").write_text("# Active tracked overlay\n", encoding="utf-8")
        overlay = (self.shared / "README.md").read_bytes()
        unrelated = (self.shared / "unrelated.txt").read_bytes()
        bundle = task / "bundle"
        if bundle.exists():
            shutil.rmtree(bundle)
        cache = self.shared / ".workspace-mgr" / "local" / "cache"
        if cache.exists():
            shutil.rmtree(cache)

        dry = self.wm(self.shared, "refresh", "--dry-run")
        self.check(dry["status"] == "dry_run", "refresh dry-run sees incoming main")
        self.check(self.git(self.shared, "rev-parse", "main").stdout.strip() == original_main, "refresh dry-run does not move local main")
        self.check(not bundle.exists(), "refresh dry-run does not hydrate S3 output")

        self.git(self.shared, "add", "README.md")
        staged_guard = self.wm(self.shared, "refresh", expected=2)
        self.check("staged changes" in staged_guard["stderr"], "refresh refuses a staged shared index")
        self.check(
            self.git(self.shared, "rev-parse", "main").stdout.strip() == original_main,
            "staged-index refusal leaves the local main ref unchanged",
        )
        self.git(self.shared, "restore", "--staged", "--", "README.md")

        bad_credentials = {
            "AWS_ACCESS_KEY_ID": "invalid-refresh-key",
            "AWS_SECRET_ACCESS_KEY": "invalid-refresh-secret",
        }
        failed_prefetch = self.wm(
            self.shared,
            "refresh",
            expected=2,
            env=bad_credentials,
        )
        self.check(failed_prefetch["stderr"], "refresh reports provider authorization failure")
        self.check(
            "invalid-refresh-key" not in failed_prefetch["stderr"]
            and "invalid-refresh-secret" not in failed_prefetch["stderr"],
            "refresh provider failure does not echo credentials",
        )
        self.check(
            self.git(self.shared, "rev-parse", "main").stdout.strip() == original_main,
            "provider failure before refresh leaves the local main ref unchanged",
        )
        self.check(
            (self.shared / "refresh-update.txt").read_text(encoding="utf-8")
            == "old refresh value\n"
            and (self.shared / "refresh-delete.txt").is_file()
            and not (self.shared / "refresh-added.txt").exists(),
            "provider failure before refresh leaves ordinary Git files unchanged",
        )
        self.check(not bundle.exists(), "provider failure before refresh materializes no stored output")
        self.check((self.shared / "README.md").read_bytes() == overlay, "provider failure preserves tracked overlay")
        self.check(
            self.git(self.shared, "diff", "--cached", "--name-only").stdout == "",
            "provider failure preserves a clean shared index",
        )
        checkout_counter = self.root / "refresh-checkout-counter"
        checkout_hook = self.root / "refresh-checkout-hook"
        checkout_hook.write_text(
            "#!/bin/sh\nset -eu\n"
            "if [ \"${1:-}\" = \"checkout\" ] && [ ! -f \"$CHECKOUT_COUNTER\" ]; then\n"
            "  : > \"$CHECKOUT_COUNTER\"\n  exit 23\nfi\nexit 0\n",
            encoding="utf-8",
        )
        checkout_hook.chmod(0o755)
        failed_refresh = self.wm(
            self.shared, "refresh", expected=2,
            env={"CHECKOUT_COUNTER": str(checkout_counter),
                 "WORKSPACE_MGR_TEST_STORAGE_HOOK": str(checkout_hook)},
        )
        self.check("rolled back" in failed_refresh["stderr"], "post-ref refresh failure is rolled back")
        self.check(
            self.git(self.shared, "rev-parse", "main").stdout.strip() == original_main,
            "failed refresh restores the original main ref",
        )
        self.check(
            (self.shared / "refresh-update.txt").read_text(encoding="utf-8")
            == "old refresh value\n",
            "failed refresh restores an ordinary modified Git file",
        )
        self.check(
            (self.shared / "refresh-delete.txt").is_file()
            and not (self.shared / "refresh-added.txt").exists(),
            "failed refresh restores ordinary additions and deletions",
        )
        self.check(not bundle.exists(), "failed refresh removes newly hydrated output")
        self.check((self.shared / "README.md").read_bytes() == overlay, "failed refresh preserves tracked overlay")
        self.check(
            self.git(self.shared, "diff", "--cached", "--name-only").stdout == "",
            "failed refresh restores a clean shared index",
        )
        refreshed = self.wm(self.shared, "refresh")
        self.check(refreshed["status"] == "updated", "refresh fast-forwards shared main")
        self.check(refreshed["new_oid"] == merged_oid, "refresh reports merged object ID")
        self.check(refreshed["storage"]["mode"] == "hydrate", "refresh uses managed-storage hydration")
        self.check(f"{task_id}/bundle.wm-storage.json" in refreshed["storage"]["changed_files"], "refresh identifies incoming storage metadata")
        self.assert_shared_head(merged_oid)
        self.check((self.shared / "README.md").read_bytes() == overlay, "refresh preserves tracked overlay")
        self.check((self.shared / "unrelated.txt").read_bytes() == unrelated, "refresh preserves unrelated untracked overlay")
        self.check((self.shared / "refresh-update.txt").read_text(encoding="utf-8") == "new refresh value\n", "refresh materializes a modified Git file")
        self.check((self.shared / "refresh-added.txt").read_text(encoding="utf-8") == "added by remote\n", "refresh materializes a new Git file")
        self.check(not (self.shared / "refresh-delete.txt").exists(), "refresh removes a clean deleted Git file")
        self.check("refresh-added.txt" in refreshed["materialized_git_paths"], "refresh reports ordinary Git materialization")
        self.check((bundle / "alpha.txt").read_bytes() == b"bundle alpha version three\n", "refresh hydrates exact S3 directory version")
        staged = self.git(self.shared, "diff", "--cached", "--name-only").stdout
        self.check(staged == "", "refresh leaves shared index clean")
        self.check(self.wm(self.shared, "refresh")["status"] == "no_changes", "repeat refresh is idempotent")

        # Merge the separate rename task after this section's primary branch
        # has advanced main, preserving the primary fast-forward assertions.
        self.finish_renamed_history()

        consumer = self.root / "consumer"
        self.run(["git", "clone", self.remote_url, consumer], cwd=self.root)
        self.configure_git(consumer)
        consumer_task = consumer / task_id
        self.check(not (consumer_task / "bundle").exists(), "fresh clone has no S3 payload")
        self.check((consumer_task / "moved.bin").read_bytes() == b"single-file version three\n", "fresh clone receives untracked Git payload")
        doctor = self.wm(consumer, "doctor")
        self.check(doctor["status"] == "ok", "fresh network clone passes doctor")
        hydrated = self.wm(consumer_task, "storage", "hydrate")
        self.check(hydrated["status"] == "hydrated", "fresh clone hydrates from MinIO")
        self.check((consumer_task / "bundle" / "alpha.txt").read_bytes() == b"bundle alpha version three\n", "cross-clone S3 hydration is exact")
        self.check((consumer_task / "notes.txt").read_text(encoding="utf-8") == "task-only content\n", "cross-clone hydration restores Git-to-S3 content")

    def exercise_untrack(self) -> None:
        assert self.shared is not None
        self.section("local retention, remote reference protection, and permanent S3 purge")
        task_id = "20260914-120000-local-retention"
        branch = "codex/local-retention"
        created = self.wm(
            self.shared,
            "task",
            "create",
            "local-retention",
            "--title",
            "Local retention",
            "--purpose",
            "Preserve local files while retiring their Git and S3 copies.",
            "--timestamp",
            "20260914-120000",
        )
        task = Path(created["path"])
        self.document_task(task)
        git_path = f"{task_id}/retained.txt"
        s3_path = f"{task_id}/retained.bin"
        git_payload = task / "retained.txt"
        s3_payload = task / "retained.bin"
        git_bytes = b"Git content retained locally\n"
        s3_v1 = b"S3 content retained locally, version one\n"
        s3_v2 = b"S3 content retained locally, version two\n"
        git_payload.write_bytes(git_bytes)
        s3_payload.write_bytes(s3_v1)
        self.wm(
            task,
            "storage",
            "set",
            s3_path,
            "--to",
            "s3",
            "--reason",
            "Exercise retirement of versioned S3 content.",
        )
        self.wm(task, "publish", "-m", "Publish retained content version one")
        s3_key = self.s3_version_for_body(s3_v1)["key"]
        s3_payload.write_bytes(s3_v2)
        second = self.wm(task, "publish", "-m", "Publish retained content version two")
        latest_version = self.s3_version_for_body(s3_v2)
        self.check(
            latest_version["key"] == s3_key,
            "both retained S3 versions share one object path",
        )
        self.merge_branch_to_main(branch)
        self.wm(self.shared, "refresh")
        tag = "untrack-retention-guard"
        self.git(self.shared, "push", "origin", f"{second['remote_oid']}:refs/tags/{tag}")

        versions_before = self.list_s3_versions()
        index_path = self.shared / ".git" / "index"
        index_before = index_path.read_bytes()
        ignore_before = (task / ".gitignore").read_bytes()
        s3_placement = task / "retained.bin.workspace-mgr-storage.toml"
        placement_before = s3_placement.read_bytes()
        dry = self.wm(task, "untrack", git_path, s3_path, "--dry-run")
        self.check(dry["status"] == "dry_run", "untrack dry-run reports local retention")
        self.check(
            (task / ".gitignore").read_bytes() == ignore_before
            and s3_placement.read_bytes() == placement_before
            and not (task / "retained.txt.workspace-mgr-storage.toml").exists()
            and (task / "retained.bin.wm-storage.json").is_file(),
            "untrack dry-run preserves all local tracking metadata",
        )
        untracked = self.wm(task, "untrack", git_path, s3_path)
        self.check(
            untracked["remote_writes"] is False
            and all(item["target"] == "local" for item in untracked["placements"]),
            "untrack records explicit local placement without remote writes",
        )
        self.check(
            git_payload.read_bytes() == git_bytes and s3_payload.read_bytes() == s3_v2,
            "untrack retains both Git and S3 payload bytes",
        )
        self.check(not (task / "retained.bin.wm-storage.json").exists(), "untrack removes the S3 pointer")
        self.check(
            self.list_s3_versions() == versions_before
            and self.remote_ref(branch) == second["remote_oid"],
            "untrack changes neither S3 versions nor the published Git branch",
        )
        plan = self.wm(task, "plan")
        self.check(
            {git_path, s3_path}.issubset(set(plan["storage"]["local_only"]))
            and git_path in plan["changed_paths"]
            and f"{s3_path}.wm-storage.json" in plan["changed_paths"],
            "plan exposes retained local boundaries and Git deletion records",
        )
        self.check(
            plan["storage"]["purge"]["status"] == "pending_publication"
            and any(
                item["pointer"] == f"{s3_path}.wm-storage.json"
                and f"objects/{item['object']}" == s3_key
                and item["version_id"] == latest_version["version_id"]
                for item in plan["storage"]["purge"]["queued"]
            )
            and self.list_s3_versions() == versions_before,
            "plan previews the exact retired S3 object and version without deleting it",
        )
        published = self.wm(task, "publish", "-m", "Keep Git and S3 content local only")
        published_oid = published["remote_oid"]
        self.check(
            all(
                not self.remote_path_exists(published_oid, path)
                for path in (git_path, s3_path, f"{s3_path}.wm-storage.json")
            )
            and all(
                self.remote_path_exists(published_oid, f"{path}.workspace-mgr-storage.toml")
                for path in (git_path, s3_path)
            ),
            "published tree retains placement intent and removes payloads and S3 pointer",
        )
        self.check(
            published["storage"]["purge"]["status"] == "cleanup_pending"
            and published["storage"]["purge"]["pending"]
            and any(warning["code"] == "s3-cleanup-pending" for warning in published.get("warnings", []))
            and self.list_s3_versions() == versions_before,
            "live main and tag references keep every retired S3 version pending",
        )
        self.check(index_path.read_bytes() == index_before, "untrack and publish preserve the shared index")
        local_s3_bytes = b"edited local-only S3 content after publication\n"
        s3_payload.write_bytes(local_s3_bytes)
        self.check(self.wm(task, "plan")["status"] == "no_changes", "editing retained content creates no publication change")
        repeated = self.wm(task, "publish", "-m", "Retry protected local-retention cleanup")
        self.check(
            repeated["status"] == "no_changes"
            and repeated["storage"]["purge"]["status"] == "cleanup_pending"
            and repeated["storage"]["purge"]["pending"]
            and any(warning["code"] == "s3-cleanup-pending" for warning in repeated.get("warnings", []))
            and self.list_s3_versions() == versions_before,
            "repeat publication neither uploads local content nor loses protected cleanup",
        )

        merged_oid = self.merge_branch_to_main(branch)
        refreshed = self.wm(self.shared, "refresh")
        self.check(
            refreshed["status"] == "updated" and refreshed["new_oid"] == merged_oid,
            "refresh materializes the merged local-retention transition",
        )
        self.check(
            git_payload.read_bytes() == git_bytes
            and s3_payload.read_bytes() == local_s3_bytes,
            "merge and refresh preserve both clean Git bytes and edited S3 bytes locally",
        )
        self.check(
            refreshed["storage"]["purge"]["status"] == "cleanup_pending"
            and refreshed["storage"]["purge"]["pending"]
            and any(warning["code"] == "s3-cleanup-pending" for warning in refreshed.get("warnings", []))
            and self.list_s3_versions() == versions_before,
            "the remote tag independently protects retired S3 versions after main is merged",
        )
        retired_ids = {(row["key"].removeprefix("objects/"), row["version_id"])
                       for row in self.s3_version_inventory() if row["key"] == s3_key}
        # Refresh rechecks every published receipt source, including exact
        # generations already absent after the previous guarded retirement.
        rename_source_ids = {(row["source_object"], row["source_version_id"])
                             for row in self.rename_retention["receipt"]["versions"]}
        self.git(self.shared, "push", "origin", f":refs/tags/{tag}")
        cleaned = self.wm(self.shared, "refresh")
        self.check(
            cleaned["status"] == "no_changes"
            and cleaned["storage"]["purge"]["status"] == "cleanup_pending"
            and retired_ids
            and {(row["object"], row["version_id"])
                 for row in cleaned["storage"]["purge"]["deleted"]} == retired_ids | rename_source_ids
            and {(row["object"], row["version_id"])
                 for row in cleaned["storage"]["purge"]["pending"]}
                == self.rename_retention["copied_ids"]
            and not cleaned["storage"]["purge"].get("pending_prefixes", []),
            "refresh retires every unrelated untrack obligation and retains only canonical rename copies",
        )
        remaining = self.s3.list_object_versions(Bucket=self.bucket, Prefix=s3_key)
        self.check(
            not remaining.get("Versions") and not remaining.get("DeleteMarkers"),
            "cleanup permanently removes every old S3 version and delete marker",
        )
        self.check(
            git_payload.read_bytes() == git_bytes
            and s3_payload.read_bytes() == local_s3_bytes,
            "permanent remote cleanup leaves local payload bytes intact",
        )
        self.check(
            self.wm(task, "plan")["status"] == "no_changes"
            and self.wm(self.shared, "refresh")["status"] == "no_changes",
            "completed local-retention lifecycle is idempotent",
        )
        self.assert_shared_head(merged_oid)

    def exercise_automatic_and_explicit_git(self) -> None:
        assert self.shared is not None
        self.section("automatic S3 placement and explicit large-file Git override")
        task_id = "20260829-190000-placement-policy"
        branch = "codex/placement-policy"
        created = self.wm(
            self.shared,
            "task",
            "create",
            "placement-policy",
            "--title",
            "Placement policy",
            "--purpose",
            "Exercise automatic S3 and explicit Git placement.",
            "--timestamp",
            "20260829-190000",
        )
        self.check(created["status"] == "created", "second task scaffold created")
        task = self.shared / task_id
        self.document_task(task)
        explicit_git = task / "explicit-git.bin"
        automatic_s3 = task / "automatic-s3.bin"
        review_band = task / "review-band.bin"
        small_default = task / "small-default.bin"
        explicit_git.write_bytes(b"g" * 10_485_761)
        automatic_s3.write_bytes(b"s" * 10_485_762)
        review_band.write_bytes(b"r" * 2_097_152)
        small_default.write_bytes(b"d" * 1_048_575)
        remote_before = self.remote_ref(branch)
        versions_before = self.list_s3_versions()
        placed = self.wm(
            task,
            "storage",
            "set",
            f"{task_id}/explicit-git.bin",
            "--to",
            "git",
            "--reason",
            "This E2E artifact must remain directly reviewable in Git.",
        )
        self.check(placed["status"] == "updated", "large file receives explicit Git placement")
        self.check(placed["remote_writes"] is False, "explicit Git placement writes no remote")
        self.check(self.remote_ref(branch) == remote_before, "placement leaves Git branch absent")
        self.check(self.list_s3_versions() == versions_before, "placement leaves S3 unchanged")
        status = self.wm(
            task,
            "storage",
            "status",
            f"{task_id}/explicit-git.bin",
        )
        self.check(status["placements"][0]["target"] == "git", "status reports explicit Git")
        self.s3.put_bucket_versioning(
            Bucket=self.bucket, VersioningConfiguration={"Status": "Suspended"}
        )
        disabled_plan = self.wm(task, "plan", expected=2)
        self.check(
            "does not have object versioning enabled" in disabled_plan["stderr"],
            "plan rejects automatic S3 placement when bucket versioning is disabled",
        )
        self.check(not task.joinpath("automatic-s3.bin.wm-storage.json").exists(), "rejected plan creates no S3 metadata")
        self.check(self.list_s3_versions() == versions_before, "rejected plan performs no S3 upload")
        self.s3.put_bucket_versioning(
            Bucket=self.bucket, VersioningConfiguration={"Status": "Enabled"}
        )
        plan = self.wm(task, "plan")
        self.check(plan["status"] == "dry_run", "automatic S3 placement appears in plan")
        decisions = {
            decision["path"]: decision
            for decision in plan["storage"]["placement"]["decisions"]
        }
        self.check(
            decisions[f"{task_id}/automatic-s3.bin"]["target"] == "s3"
            and decisions[f"{task_id}/automatic-s3.bin"]["basis"]
            == "automatic-size-fallback",
            "plan routes the unplaced large file to S3",
        )
        self.check(
            decisions[f"{task_id}/review-band.bin"]["target"] == "git"
            and decisions[f"{task_id}/review-band.bin"]["warnings"][0]["code"]
            == "semantic-placement-review",
            "plan asks the agent to review semantic placement in the 1-10 MiB band",
        )
        self.check(
            f"{task_id}/small-default.bin" not in decisions,
            "sub-1 MiB content uses the strong Git default without plan noise",
        )
        self.check(not task.joinpath("automatic-s3.bin.wm-storage.json").exists(), "plan does not create S3 metadata")
        self.check(self.list_s3_versions() == versions_before, "plan performs no S3 upload")
        published = self.wm(task, "publish", "-m", "Publish automatic and explicit placement")
        self.check(published["status"] == "pushed", "mixed Git and S3 placement publishes")
        oid = self.remote_ref(branch)
        assert oid is not None
        self.check(self.remote_path_exists(oid, f"{task_id}/explicit-git.bin"), "explicit large file is stored in Git")
        self.check(self.remote_path_exists(oid, f"{task_id}/review-band.bin"), "review-band fallback is stored in Git")
        self.check(self.remote_path_exists(oid, f"{task_id}/small-default.bin"), "sub-1 MiB fallback is stored in Git")
        self.check(not self.remote_path_exists(oid, f"{task_id}/automatic-s3.bin"), "automatic S3 payload is absent from Git")
        self.check(self.remote_path_exists(oid, f"{task_id}/automatic-s3.bin.wm-storage.json"), "automatic S3 metadata is stored in Git")
        self.check(len(self.list_s3_versions()) > len(versions_before), "automatic placement uploads a versioned S3 object")
        self.check(self.wm(task, "plan")["status"] == "no_changes", "mixed placement ends cleanly")

        stored_version = self.s3_version_for_body(automatic_s3.read_bytes())
        remote_before_loss = self.remote_ref(branch)
        self.s3.delete_object(
            Bucket=self.bucket,
            Key=stored_version["key"],
            VersionId=stored_version["version_id"],
        )
        self.record("s3-fault", {"operation": "delete-version", **stored_version})
        automatic_s3.unlink()
        cache = self.shared / ".workspace-mgr" / "local" / "cache"
        if cache.exists():
            shutil.rmtree(cache)
        missing_remote = self.wm(
            task,
            "storage",
            "hydrate",
            f"{task_id}/automatic-s3.bin",
            expected=2,
        )
        self.check(missing_remote["stderr"], "hydrate reports a missing exact S3 version")
        self.check(not automatic_s3.exists(), "failed exact-version hydrate leaves output absent")
        self.check(self.remote_ref(branch) == remote_before_loss, "missing S3 version leaves Git unchanged")

    def exercise_cloud_usage_approval(self) -> tuple[str, Path, str]:
        assert self.shared is not None
        assert self.remote is not None
        self.section("cloud-usage accounting and approval at the real threshold")
        task_id = "20260829-192000-usage-approval"
        branch = "codex/usage-approval"
        threshold = 1_073_741_824
        config_name = ".workspace-mgr.toml"
        manifest_path = f"{task_id}/.workspace-mgr-task.toml"
        created = self.wm(
            self.shared,
            "task",
            "create",
            "usage-approval",
            "--title",
            "Cloud usage approval",
            "--purpose",
            "Measure and gate the cloud usage of one task.",
            "--timestamp",
            "20260829-192000",
        )
        self.check(created["status"] == "created", "cloud-usage task scaffold created")
        task = Path(created["path"])
        manifest = task / ".workspace-mgr-task.toml"
        scaffold = self.wm(task, "publish", "-m", "Publish cloud-usage task scaffold")
        usage = scaffold["cloud_usage"]
        self.check(
            scaffold["status"] == "pushed"
            and usage["status"] == "within_limit"
            and usage["threshold_bytes"] == threshold
            and usage["limit_bytes"] == threshold
            and usage["approval"] is None
            and usage["projected"]["s3_bytes"] == 0,
            "a small publication stays within the fixed 1 GiB threshold",
            cloud_usage=usage,
        )
        self.check(
            "repository_requirement" not in scaffold
            and config_name not in scaffold["changed_paths"]
            and manifest.read_text(encoding="utf-8").startswith("schema_version = 2\n"),
            "a task without an approval keeps manifest schema 2 and leaves the requirement alone",
            report=scaffold,
        )

        # Publishing content requires the task to document itself, and the
        # record must already be published when the later checks compare
        # `changed_paths` exactly.
        self.document_task(task)
        self.exercise_versioned_cloud_usage(task_id, task)

        # A sparse file reports its full logical size without writing 1 GiB.
        remote_before = self.remote_ref(branch)
        versions_before = self.list_s3_versions()
        sparse_path = f"{task_id}/sparse-checkpoint.bin"
        sparse = task / "sparse-checkpoint.bin"
        sparse_pointer = task / "sparse-checkpoint.bin.wm-storage.json"
        with sparse.open("wb") as stream:
            stream.truncate(threshold + 1)
        usage = self.wm(task, "plan")["cloud_usage"]
        self.check(
            usage["status"] == "approval_required"
            and usage["publish_allowed"] is False
            and usage["cleanup_only"] is False
            and usage["limit_bytes"] == threshold
            and usage["published"]["s3_bytes"] == 0
            and usage["projected"]["s3_bytes"] == threshold + 1
            and usage["contributors"][0]
            == {
                "path": sparse_path,
                "store": "s3",
                "bytes": threshold + 1,
                "versions": 1,
                "state": "pending",
            }
            and usage["suggested_limit_bytes"] == 1_610_612_736
            and "exceeds the limit 1 GiB (1073741824 bytes)" in usage["message"],
            "plan reports a pending upload past the threshold as waiting for approval",
            cloud_usage=usage,
        )
        self.check(not sparse_pointer.exists(), "plan does not track the oversized file")
        refused = self.wm(task, "publish", "-m", "Publish sparse checkpoint", expected=2)
        self.check(
            refused["stdout"] == ""
            and f"cloud usage for task {task_id} needs the user's approval" in refused["stderr"]
            and "S3 1 GiB (1073741825 bytes)" in refused["stderr"]
            and "limit 1 GiB (1073741824 bytes)" in refused["stderr"],
            "publish refuses growth past the limit with the measured usage",
        )
        self.check(not sparse_pointer.exists(), "refused publication tracks nothing")
        self.check(self.remote_ref(branch) == remote_before, "refused publication leaves network Git unchanged")
        self.check(self.list_s3_versions() == versions_before, "refused publication uploads no S3 version")
        status = self.wm(task, "task", "status")["cloud_usage"]
        pending = status["pending"]
        self.check(
            status["approval"] is None
            and pending is not None
            and pending["limit_bytes"] == threshold
            and pending["remote_target_oid"] == remote_before
            and pending["projected"]["s3_bytes"] == threshold + 1,
            "task status shows the decision the task is waiting for",
            cloud_usage=status,
        )
        reminder = f"workspace-mgr: task {task_id} is waiting for the user's cloud-usage decision"
        waiting = self.run(
            [self.binary, "--format", "json", "storage", "status", sparse_path], cwd=task
        )
        self.check(reminder in waiting.stderr, "task-scoped commands remind while the decision is pending")

        manifest_before = manifest.read_text(encoding="utf-8")
        lowered = self.wm(
            task,
            "task",
            "approve-cloud-usage",
            "--limit",
            "1023MiB",
            "--note",
            "The E2E user approved 1023 MiB",
            expected=2,
        )
        self.check(
            "approved cloud-usage limit 1023 MiB (1072693248 bytes) is below the threshold 1 GiB (1073741824 bytes)"
            in lowered["stderr"]
            and manifest.read_text(encoding="utf-8") == manifest_before,
            "an approval can only raise the limit",
        )
        note = "The E2E user approved 2 GiB for the sparse checkpoint"
        approval = {"limit_bytes": 2_147_483_648, "note": note}
        approved = self.wm(
            task, "task", "approve-cloud-usage", "--limit", "2GiB", "--note", note
        )
        self.check(
            approved["status"] == "recorded"
            and Path(approved["manifest"]).resolve() == manifest.resolve()
            and approved["schema_version"] == 3
            and approved["previous_limit_bytes"] == threshold
            and approved["limit_bytes"] == 2_147_483_648
            and approved["limit"] == "2 GiB (2147483648 bytes)"
            and approved["pending"] == pending
            and approved["blocked"] is False
            and approved["remote_writes"] is False
            and "recorded_at" not in approved,
            "the user's approval is recorded in the task manifest",
            report=approved,
        )
        approved_manifest = manifest.read_text(encoding="utf-8")
        self.check(
            approved_manifest
            == manifest_before.replace("schema_version = 2", "schema_version = 3")
            + f'\n[cloud_usage_approval]\nlimit_bytes = 2147483648\nnote = "{note}"\n',
            "the manifest moves to schema 3 with the approval table",
            manifest=approved_manifest,
        )
        self.check(
            self.remote_ref(branch) == remote_before and self.list_s3_versions() == versions_before,
            "recording an approval writes neither remote",
        )
        covered = self.run(
            [self.binary, "--format", "json", "storage", "status", sparse_path], cwd=task
        )
        self.check(reminder not in covered.stderr, "the reminder stops once an approval covers the projection")

        # Native repositories already require 0.8.1. A task schema 3
        # approval preserves that higher compatibility floor.
        # Recording the approval does not measure, so the pending decision
        # stays until the next measurement: this plan, within the approved
        # limit, is what clears it.
        self.check(
            self.wm(task, "task", "status")["cloud_usage"]["pending"] == pending,
            "the approval leaves the pending decision to the next measurement",
        )
        approved_plan = self.wm(task, "plan")
        self.check(
            approved_plan["status"] == "dry_run"
            and approved_plan["cloud_usage"]["status"] == "within_limit"
            and approved_plan["cloud_usage"]["approval"] == approval
            and approved_plan["cloud_usage"]["projected"]["s3_bytes"] == threshold + 1
            and approved_plan.get("repository_requirement") is None
            and self.wm(task, "task", "status")["cloud_usage"]["pending"] is None,
            "a plan within the approved limit clears the pending decision",
            report=approved_plan,
        )
        self.check(not sparse_pointer.exists(), "the plan does not track the oversized file")
        rehearsal = self.wm(task, "publish", "-m", "Publish sparse checkpoint", "--dry-run")
        self.check(
            rehearsal["status"] == "dry_run"
            and rehearsal["cloud_usage"]["status"] == "within_limit"
            and rehearsal["cloud_usage"]["limit_bytes"] == 2_147_483_648
            and rehearsal["cloud_usage"]["approval"] == approval
            and rehearsal.get("repository_requirement") is None,
            "publish --dry-run passes the gate under the approved limit",
            report=rehearsal,
        )
        self.check(
            not sparse_pointer.exists()
            and self.remote_ref(branch) == remote_before
            and self.list_s3_versions() == versions_before,
            "the rehearsal neither tracks nor uploads the oversized file",
        )

        # The user then chooses cleanup; the file must never reach S3 because
        # later scenarios download every object version.
        sparse.unlink()
        cleaned = self.wm(task, "plan")
        self.check(
            cleaned["status"] == "dry_run"
            and cleaned["changed_paths"] == [manifest_path]
            and cleaned.get("repository_requirement") is None
            and cleaned["cloud_usage"]["status"] == "within_limit"
            and cleaned["cloud_usage"]["approval"] == approval,
            "after cleanup only the approval remains to publish",
            report=cleaned,
        )
        (task / "usage-notes.md").write_text(
            "The sparse checkpoint was cleaned up instead of published.\n",
            encoding="utf-8",
        )
        shared_config = (self.shared / config_name).read_text(encoding="utf-8")
        main_before = self.remote_ref("main")
        published = self.wm(task, "publish", "-m", "Publish cloud-usage notes")
        commit = published["commit_oid"]
        self.check(
            published["status"] == "pushed"
            and self.remote_ref(branch) == commit
            and published.get("repository_requirement") is None
            and published["changed_paths"]
            == [manifest_path, f"{task_id}/usage-notes.md"],
            "the task publishes the approval after the decision",
            report=published,
        )
        message = self.run(
            ["git", "--git-dir", self.remote, "show", "-s", "--format=%B", commit],
            cwd=self.root,
        ).stdout
        self.check(
            message.rstrip("\n").splitlines()[-1] == f"Cloud-Usage-Approval: limit_bytes=2147483648; note={note}"
            and "Workspace-Requirement:" not in message,
            "the publication carries the approval without lowering the native compatibility requirement",
            commit_message=message,
        )
        published_config = self.remote_file(commit, config_name)
        self.check(
            published_config == shared_config and f'minimum_cli_version = "{TASK_RENAME_STORAGE_MINIMUM_CLI_VERSION}"' in published_config,
            "the published tree retains the higher merged rename compatibility requirement",
            config=published_config,
        )
        self.check(
            self.remote_file(commit, manifest_path) == approved_manifest,
            "the published task manifest is schema 3 with the approval",
        )
        self.check(
            (self.shared / config_name).read_text(encoding="utf-8") == shared_config
            and self.git(self.shared, "status", "--porcelain", "--", config_name).stdout == ""
            and self.remote_ref("main") == main_before
            and f'minimum_cli_version = "{TASK_RENAME_STORAGE_MINIMUM_CLI_VERSION}"' in self.remote_file(main_before, config_name),
            "publication leaves shared main and its higher rename compatibility requirement unchanged",
        )
        self.check(
            self.list_s3_versions() == versions_before,
            "the cleaned-up oversized file never reached S3",
        )
        return task_id, task, branch

    def exercise_versioned_cloud_usage(self, task_id: str, task: Path) -> None:
        """Version-aware S3 usage counts every retained version until retirement."""
        stored_path = f"{task_id}/measurements.bin"
        stored_key = f"objects/{stored_path}"
        stored = task / "measurements.bin"
        stored.write_bytes(b"m" * 1_000)
        self.wm(
            task,
            "storage",
            "set",
            stored_path,
            "--to",
            "s3",
            "--reason",
            "Exercise version-aware cloud-usage accounting.",
        )
        usage = self.wm(task, "plan")["cloud_usage"]
        self.check(
            usage["published"]["s3_bytes"] == 0 and usage["projected"]["s3_bytes"] == 1_000,
            "plan counts storage metadata that has not been uploaded as pending S3 usage",
            cloud_usage=usage,
        )
        self.wm(task, "publish", "-m", "Publish first measurements")
        stored.write_bytes(b"n" * 2_000)
        usage = self.wm(task, "plan")["cloud_usage"]
        self.check(
            usage["published"]["s3_bytes"] == 1_000 and usage["projected"]["s3_bytes"] == 3_000,
            "plan counts a changed stored output as a new pending version",
            cloud_usage=usage,
        )
        self.wm(task, "publish", "-m", "Publish second measurements")
        stored_versions = [item for item in self.list_s3_versions() if item["key"] == stored_key]
        usage = self.wm(task, "plan")["cloud_usage"]
        contributor = next(
            (item for item in usage["contributors"] if item["path"] == stored_path),
            None,
        )
        self.check(
            sorted(item["size"] for item in stored_versions) == [1_000, 2_000]
            and usage["published"]["s3_bytes"] == 3_000
            and usage["projected"]["s3_bytes"] == 3_000
            and contributor
            == {
                "path": stored_path,
                "store": "s3",
                "bytes": 3_000,
                "versions": 2,
                "state": "published",
            },
            "published S3 usage counts every retained version at the live path",
            versions=stored_versions,
            cloud_usage=usage,
        )
        self.wm(task, "remove", stored_path)
        usage = self.wm(task, "plan")["cloud_usage"]
        self.check(
            usage["published"]["s3_bytes"] == 3_000
            and usage["projected"]["s3_bytes"] == 0
            and usage["cleanup_only"] is True,
            "removing stored content projects the retirement of every version",
            cloud_usage=usage,
        )
        self.wm(task, "publish", "-m", "Remove measurements")
        usage = self.wm(task, "plan")["cloud_usage"]
        self.check(
            usage["published"]["s3_bytes"] == 0
            and all(item["key"] != stored_key for item in self.list_s3_versions()),
            "the publication that retires a path purges its versions and releases their usage",
            cloud_usage=usage,
        )

    def discard_published_deliverable(self) -> None:
        assert self.shared is not None
        self.section("explicit abandonment and deliverable task discard")
        task_id = "20260829-193000-discard-flow"
        branch = "codex/discard-flow"
        created = self.wm(
            self.shared,
            "task",
            "create",
            "discard-flow",
            "--title",
            "Disposable E2E task",
            "--purpose",
            "Verify complete unmerged task abandonment.",
            "--timestamp",
            "20260829-193000",
        )
        task = Path(created["path"])
        self.document_task(task)
        manifest = Path(created["manifest"])
        payload = b"discarded task payload retained in versioned S3\n"
        (task / "artifact.bin").write_bytes(payload)
        self.wm(
            task,
            "storage",
            "set",
            f"{task_id}/artifact.bin",
            "--to",
            "s3",
            "--reason",
            "Exercise discard reporting with permanent S3 deletion.",
        )
        published = self.wm(task, "publish", "-m", "Publish disposable E2E task")
        self.check(self.remote_ref(branch) == published["remote_oid"], "disposable task reaches network Git")
        self.check(payload in self.s3_bodies(), "disposable task reaches versioned S3")
        discarded_key = self.s3_version_for_body(payload)["key"]
        shared_head = self.git(self.shared, "rev-parse", "main").stdout.strip()

        preview = self.wm(task, "task", "discard", "--dry-run")
        self.check(preview["status"] == "dry_run", "deliverable discard dry-run succeeds")
        self.check(preview["remote_branch_oid"] == published["remote_oid"], "discard binds the published branch revision")
        self.check(
            preview["review"]["managed_by"] == "agent"
            and "close" in preview["review"]["required_before_confirm"],
            "discard requires the agent to close the unmerged PR",
        )
        self.check(
            preview["local_actions"][0]["path"] == task_id
            and preview["local_actions"][0]["action"] == "delete",
            "discard reports the exact deliverable directory",
        )
        self.check(
            preview["s3_purge"]["status"] == "planned"
            and any(
                item["object"] == f"{task_id}/artifact.bin"
                for item in preview["s3_purge"]["queued"]
            ),
            "discard reports exact S3 paths queued for permanent deletion",
        )
        confirmation_plan = Path(preview["confirmation_plan"])
        self.check(confirmation_plan.is_file(), "discard dry-run saves private confirmation state")

        discarded = self.wm(
            self.shared,
            "task",
            "discard",
            "--manifest",
            str(manifest),
            "--confirm",
            task_id,
        )
        self.check(discarded["status"] == "discarded", "confirmed deliverable discard succeeds")
        self.check(
            discarded.get("cleanup_warnings", []) == [],
            "deliverable discard completes without cleanup warnings",
        )
        self.check(not task.exists(), "deliverable discard removes its local directory")
        self.check(not confirmation_plan.exists(), "deliverable discard removes private task state")
        self.check(self.remote_ref(branch) is None, "deliverable discard deletes its network branch")
        local_branch = self.git(
            self.shared,
            "rev-parse",
            "--verify",
            branch,
            expected=(0, 128),
        )
        self.check(local_branch.returncode == 128, "deliverable discard deletes its local branch")
        self.check(
            all(item["key"] != discarded_key for item in self.list_s3_versions())
            and discarded["s3_purge"]["status"] == "cleanup_pending"
            and {(row["object"], row["version_id"])
                 for row in discarded["s3_purge"]["pending"]} == self.rename_retention["copied_ids"]
            and discarded["s3_purge"].get("pending_prefixes", []) == [],
            "deliverable discard removes every owned S3 version and retains only canonical rename copies",
        )
        self.check(
            self.git(self.shared, "rev-parse", "main").stdout.strip() == shared_head,
            "deliverable discard does not move the shared branch",
        )
        self.check((self.shared / "unrelated.txt").is_file(), "deliverable discard preserves another task overlay")

    def exercise_non_fast_forward_refresh_guard(self) -> None:
        assert self.shared is not None
        self.section("network non-fast-forward refresh guard")
        local_main = self.git(self.shared, "rev-parse", "main").stdout.strip()
        tree = self.git(self.shared, "show", "-s", "--format=%T", local_main).stdout.strip()
        divergent = self.git(
            self.shared,
            "commit-tree",
            tree,
            "-m",
            "Intentional divergent E2E root",
        ).stdout.strip()
        self.git(
            self.shared,
            "push",
            "--force",
            "origin",
            f"{divergent}:refs/heads/main",
        )
        self.check(self.remote_ref("main") == divergent, "network Git server exposes divergent main")
        overlay = (self.shared / "README.md").read_bytes()
        rejected = self.wm(self.shared, "refresh", expected=2)
        self.check("cannot fast-forward" in rejected["stderr"], "refresh rejects a non-fast-forward remote")
        self.check(
            self.git(self.shared, "rev-parse", "main").stdout.strip() == local_main,
            "non-fast-forward refusal preserves local main",
        )
        self.check((self.shared / "README.md").read_bytes() == overlay, "non-fast-forward refusal preserves overlays")
        self.check(
            self.git(self.shared, "diff", "--cached", "--name-only").stdout == "",
            "non-fast-forward refusal preserves a clean shared index",
        )

    def exercise_unaddressable_refresh_recovery(self) -> None:
        """A release before the addressability refusal could publish an S3
        boundary whose path contains a backslash. Reproduce that history on the
        versioned bucket with the engine and Git directly, then prove that
        refresh advances past it and that the recovery its warning names works
        against object versions, which a filesystem remote cannot show: the
        move fetches through the old version ID before the rename drops it,
        publication uploads the payload under the new path, and the old path's
        versions are purged once no reference protects them."""
        assert self.shared is not None and self.seed is not None
        self.section("refresh past storage metadata the engine cannot address")
        synced = self.wm(self.shared, "refresh")
        self.check(
            synced["status"] in {"updated", "no_changes", "s3_purged"},
            "shared checkout synchronizes before the unaddressable scenario",
            status=synced["status"],
        )
        task_id = "20260920-090000-e2e-unaddressable"
        branch = "codex/e2e-unaddressable"
        boundary = f"{task_id}/top\\level.bin"
        destination = f"{task_id}/top-level.bin"
        first = b"addressable boundary published before the crafted revision\n"
        second = b"addressable boundary held only by the versioned bucket\n"
        unaddressable = b"payload of a path the storage engine cannot address\n"
        self.wm(
            self.shared,
            "task",
            "create",
            "e2e-unaddressable",
            "--title",
            "E2E unaddressable boundary",
            "--purpose",
            "Refresh past and recover metadata the storage engine cannot address.",
            "--timestamp",
            "20260920-090000",
        )
        task = self.shared / task_id
        self.document_task(task)
        (task / "first.bin").write_bytes(first)
        self.wm(
            task,
            "storage",
            "set",
            f"{task_id}/first.bin",
            "--to",
            "s3",
            "--reason",
            "Exercise an addressable S3 boundary beside the unaddressable one.",
        )
        published = self.wm(task, "publish", "-m", "Publish the addressable boundary")
        self.check(published["status"] == "pushed", "unaddressable scenario publishes its first boundary")
        self.merge_branch_to_main(branch)
        self.wm(self.shared, "refresh")

        # Construct a published native sidecar literally through the isolated
        # fixture client to exercise an unaddressable historical name.
        publisher = self.root / "unaddressable-publisher"
        self.run(["git", "clone", self.remote_url, publisher], cwd=self.root)
        self.configure_git(publisher)
        publisher_task = publisher / task_id
        import hashlib
        for name, body in [("second.bin", second), ("top\\level.bin", unaddressable)]:
            (publisher_task / name).write_bytes(body)
            uploaded = self.s3.put_object(Bucket=self.bucket, Key=f"objects/{task_id}/{name}", Body=body)
            md5 = hashlib.md5(body).hexdigest()
            (publisher_task / (name + ".wm-storage.json")).write_text(
                json.dumps({
                    "schema_version": 1,
                    "path": name,
                    "kind": "file",
                    "checksum": {"algorithm": "md5", "digest": md5},
                    "size": len(body),
                    "version": {"id": uploaded["VersionId"], "etag": uploaded["ETag"].strip(chr(34))},
                }, indent=2) + "\n",
                encoding="utf-8",
            )
            with (publisher_task / ".gitignore").open("a", encoding="utf-8") as ignore:
                ignore.write("/" + name.replace("\\", "\\\\") + "\n")
        crafted_pointer = (publisher_task / "top\\level.bin.wm-storage.json").read_text(encoding="utf-8")
        self.check(bool(json.loads(crafted_pointer)["version"]["id"]), "crafted unaddressable metadata records its S3 version")
        old_version = self.s3_version_for_body(unaddressable)
        self.git(publisher, "add", "-A")
        self.git(
            publisher,
            "commit",
            "-m",
            "Publish metadata the engine cannot address\n\n"
            f"Workspace-Task: {task_id}\nWorkspace-Scope: {task_id}\n",
        )
        self.git(publisher, "push", "origin", "HEAD:refs/heads/main")
        self.git(publisher, "push", "origin", f"HEAD:refs/heads/{branch}")
        crafted_oid = self.git(publisher, "rev-parse", "HEAD").stdout.strip()
        original_main = self.git(self.shared, "rev-parse", "main").stdout.strip()

        dry = self.wm(self.shared, "refresh", "--dry-run")
        self.check(dry["status"] == "dry_run", "refresh dry-run sees the crafted revision")
        self.check(
            dry["storage"].get("unaddressable") == [boundary],
            "refresh dry-run reports the unaddressable boundary instead of a false green",
            storage=dry["storage"],
        )
        self.check(
            [warning["code"] for warning in dry.get("warnings", [])]
            == ["unaddressable-storage-metadata"],
            "refresh dry-run carries the unaddressable-storage-metadata warning",
        )
        self.check(
            self.git(self.shared, "rev-parse", "main").stdout.strip() == original_main,
            "refresh dry-run leaves main unchanged",
        )

        refreshed = self.wm(self.shared, "refresh")
        self.check(refreshed["status"] == "updated", "refresh advances past unaddressable metadata")
        self.assert_shared_head(crafted_oid)
        self.check(
            (task / "second.bin").read_bytes() == second,
            "refresh hydrates the other incoming boundary from the versioned bucket",
        )
        self.check((task / "top\\level.bin.wm-storage.json").is_file(), "unaddressable metadata advances with the branch")
        self.check(not (task / "top\\level.bin").exists(), "refresh leaves the unaddressable payload unhydrated")
        self.check(
            refreshed["storage"].get("unaddressable") == [boundary],
            "refresh reports the unaddressable boundary",
            storage=refreshed["storage"],
        )
        warning = refreshed["warnings"][0]
        self.check(warning["code"] == "unaddressable-storage-metadata", "refresh warns about the skipped boundary")
        self.check(f"(`{task_id}`)" in warning["message"], "the warning names the directory to scope the recovery to")

        consumer = self.root / "unaddressable-consumer"
        self.run(["git", "clone", self.remote_url, consumer], cwd=self.root)
        self.configure_git(consumer)
        self.check(
            (consumer / f"{boundary}.wm-storage.json").is_file() and not (consumer / boundary).exists(),
            "another checkout holds the unaddressable metadata without its payload",
        )

        created = self.wm(
            consumer,
            "task",
            "create",
            "e2e-recover-boundary",
            "--kind",
            "infrastructure",
            "--title",
            "Recover an unaddressable boundary",
            "--purpose",
            "Rename a storage boundary the engine cannot address.",
            "--scope",
            task_id,
            "--scope-note",
            "The E2E scenario authorizes renaming this boundary.",
        )
        worktree = Path(created["path"])
        manifest = str(created["manifest"])
        self.check(
            (worktree / f"{boundary}.wm-storage.json").is_file() and not (worktree / boundary).exists(),
            "the recovery task starts from the fetched base without the payload",
        )
        moved = self.wm(worktree, "move", boundary, destination, "--manifest", manifest)
        self.check(moved["status"] == "updated", "move renames an un-hydrated unaddressable boundary")
        self.check(
            (worktree / destination).read_bytes() == unaddressable,
            "move fetches the payload through its old version ID and materializes it at the destination",
        )
        self.check(
            not (worktree / f"{boundary}.wm-storage.json").exists() and not (worktree / boundary).exists(),
            "move leaves nothing at the unaddressable path",
        )
        self.check(
            "version" not in json.loads((worktree / f"{destination}.wm-storage.json").read_text(encoding="utf-8")),
            "the renamed metadata carries no version until publication uploads the new path",
        )
        hydrated = self.wm(
            worktree,
            "storage",
            "hydrate",
            "--manifest",
            manifest,
            f"{task_id}/first.bin",
            f"{task_id}/second.bin",
        )
        self.check(hydrated["status"] == "hydrated", "the other boundaries hydrate by name before publication")
        usage = self.wm(worktree, "plan", "--manifest", manifest)["cloud_usage"]
        self.check(
            usage["status"] == "within_limit"
            and usage["published"]["s3_bytes"] == 0
            and usage["projected"]["s3_bytes"] == len(unaddressable)
            and [item for item in usage["contributors"] if item["store"] == "s3"]
            == [
                {
                    "path": destination,
                    "store": "s3",
                    "bytes": len(unaddressable),
                    "versions": 1,
                    "state": "pending",
                }
            ],
            "the moved boundary is charged once, as one pending upload at its new path",
            cloud_usage=usage,
        )
        recovered = self.wm(worktree, "publish", "--manifest", manifest, "-m", "Recover the unaddressable boundary")
        self.check(recovered["status"] == "pushed", "the recovery task publishes")
        self.check(
            bool(json.loads((worktree / f"{destination}.wm-storage.json").read_text(encoding="utf-8"))["version"]["id"]),
            "publication records the renamed boundary's new S3 version",
        )
        self.merge_branch_to_main(created["branch"])
        # The deliverable branch still names the old path; once it is gone no
        # reference protects that path's versions.
        self.git(self.seed, "push", "origin", "--delete", branch)

        for checkout in (consumer, self.shared):
            after = self.wm(checkout, "refresh")
            self.check(after["status"] == "updated", "refresh takes the recovery", checkout=str(checkout))
            expected_pending = self.rename_retention["copied_ids"] if checkout == self.shared else set()
            self.check(
                [warning["code"] for warning in after.get("warnings", [])]
                    == (["s3-cleanup-pending"] if expected_pending else [])
                and {(row["object"], row["version_id"])
                     for row in after["storage"]["purge"]["pending"]} == expected_pending
                and not after["storage"]["purge"].get("pending_prefixes", [])
                and "unaddressable" not in after["storage"],
                "recovery clears every unaddressable obligation and reports only intentional copied-history retention",
                checkout=str(checkout),
            )
            self.check(
                (checkout / destination).read_bytes() == unaddressable,
                "refresh hydrates the renamed boundary from its new S3 version",
                checkout=str(checkout),
            )
            self.check(
                not (checkout / f"{boundary}.wm-storage.json").exists() and not (checkout / boundary).exists(),
                "refresh retires the unaddressable path",
                checkout=str(checkout),
            )
        surviving = self.s3_version_for_body(unaddressable)
        self.check(
            surviving["key"] != old_version["key"] and surviving["key"].endswith(destination),
            "only the renamed path's version survives; the old path's versions are purged",
            old=old_version,
            surviving=surviving,
        )

    def exercise_repository_requirement_guard(self, task_id: str, task: Path, branch: str) -> None:
        assert self.shared is not None
        self.section("network repository minimum-version guard")
        config_name = ".workspace-mgr.toml"
        version = self.run([self.binary, "--version"]).stdout.split()[-1]
        local_main = self.git(self.shared, "rev-parse", "main").stdout.strip()
        self.check(
            self.remote_ref("main") == local_main,
            "the shared checkout starts the minimum-version guard synchronized with network main",
        )
        # Advance main by a fast-forward whose configuration requires a
        # release that does not exist yet.
        config = self.remote_file(local_main, config_name)
        requirement = f'minimum_cli_version = "{TASK_RENAME_STORAGE_MINIMUM_CLI_VERSION}"'
        self.check(requirement in config, "shared main preserves the merged rename compatibility requirement")
        raised = self.root / "raised-workspace-config.toml"
        raised.write_text(config.replace(requirement, 'minimum_cli_version = "99.0.0"'), encoding="utf-8")
        blob = self.git(self.shared, "hash-object", "-w", str(raised)).stdout.strip()
        index = {"GIT_INDEX_FILE": str(self.root / "raised-workspace-index")}
        self.run(["git", "-C", self.shared, "read-tree", local_main], cwd=self.shared, env=index)
        self.run(
            [
                "git",
                "-C",
                self.shared,
                "update-index",
                "--cacheinfo",
                f"100644,{blob},{config_name}",
            ],
            cwd=self.shared,
            env=index,
        )
        tree = self.run(
            ["git", "-C", self.shared, "write-tree"], cwd=self.shared, env=index
        ).stdout.strip()
        required = self.git(
            self.shared,
            "commit-tree",
            tree,
            "-p",
            local_main,
            "-m",
            "Require a future workspace-mgr",
        ).stdout.strip()
        self.git(self.shared, "push", "origin", f"{required}:refs/heads/main")
        self.check(self.remote_ref("main") == required, "network main requires a newer workspace-mgr")

        refusal = (
            "workspace-mgr: this repository requires workspace-mgr 99.0.0 or newer "
            f"(`minimum_cli_version` in {config_name} on origin/main); this is workspace-mgr {version}. "
            "Tell the user both versions and ask before updating with "
            "`cargo install --locked workspace-mgr`, then run `workspace-mgr setup`.\n"
        )
        shared_config = (self.shared / config_name).read_text(encoding="utf-8")
        for args in (("refresh", "--dry-run"), ("refresh",)):
            rejected = self.wm(self.shared, *args, expected=2)
            self.check(
                rejected["stdout"] == "" and rejected["stderr"] == refusal,
                "refresh refuses an incoming requirement it does not meet",
                args=list(args),
                stderr=rejected["stderr"],
            )
            self.check(
                self.git(self.shared, "rev-parse", "main").stdout.strip() == local_main
                and (self.shared / config_name).read_text(encoding="utf-8") == shared_config
                and self.git(self.shared, "diff", "--cached", "--name-only").stdout == "",
                "the refused refresh leaves main, the index, and the configuration unchanged",
            )
        (task / "usage-notes.md").write_text("Checked against a newer main.\n", encoding="utf-8")
        task_tip = self.remote_ref(branch)
        for args in (("plan",), ("publish", "-m", "Publish against a newer main")):
            rejected = self.wm(task, *args, expected=2)
            self.check(
                rejected["stdout"] == "" and rejected["stderr"] == refusal,
                "plan and publish refuse when only the shared branch requires a newer release",
                args=list(args),
                stderr=rejected["stderr"],
            )
        self.check(self.remote_ref(branch) == task_tip, "the refused publication leaves the task branch unchanged")
        status = self.wm(task, "task", "status")
        self.check(
            status["task_id"] == task_id,
            "local commands still work because the checkout requirement is compatible",
        )
        # No later section may inherit a network main this release refuses.
        self.git(self.shared, "push", "--force", "origin", f"{local_main}:refs/heads/main")
        restored = self.wm(self.shared, "refresh", "--dry-run")
        self.check(
            self.remote_ref("main") == local_main and restored["status"] == "no_changes",
            "network main no longer requires a newer workspace-mgr",
            status=restored["status"],
        )

    def close(self) -> None:
        if self.git_daemon is not None:
            self.git_daemon.terminate()
            try:
                self.git_daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.git_daemon.kill()
                self.git_daemon.wait(timeout=5)
        if self.git_daemon_log is not None:
            self.git_daemon_log.close()

    def execute(self) -> None:
        self.section("virtual services")
        self.setup_s3()
        self.provision_runtime()
        self.setup_repository()
        self.initialize_workspace()
        task_id, task, branch = self.create_and_publish_task()
        self.exercise_native_storage(task_id, task, branch)
        self.rename_published_task()
        self.create_and_publish_infrastructure_task()
        self.refresh_and_cross_clone(task_id, task, branch)
        self.exercise_untrack()
        self.exercise_automatic_and_explicit_git()
        usage_task = self.exercise_cloud_usage_approval()
        self.discard_published_deliverable()
        self.exercise_unaddressable_refresh_recovery()
        # Every section above leaves network main as a fast-forward this
        # release can refresh. The minimum-version guard raises main past this
        # release and restores it; the non-fast-forward guard, which leaves
        # main diverged, runs last.
        self.exercise_repository_requirement_guard(*usage_task)
        self.exercise_non_fast_forward_refresh_guard()
        summary = {
            "status": "passed",
            "assertions": self.assertions,
            "evidence": str(self.evidence_path),
            "git_remote": self.remote_url,
            "s3_endpoint": self.endpoint,
            "s3_versions": len(self.list_s3_versions()),
        }
        self.record("summary", summary)
        print("\n" + json.dumps(summary, indent=2, sort_keys=True), flush=True)


def main() -> int:
    harness: Harness | None = None
    try:
        harness = Harness()
        harness.execute()
        return 0
    except Exception as error:  # noqa: BLE001 - top-level evidence boundary
        if harness is not None:
            harness.record("summary", {"status": "failed", "error": repr(error)})
        print(f"workspace-mgr E2E failed: {error}", file=sys.stderr, flush=True)
        return 1
    finally:
        if harness is not None:
            harness.close()


if __name__ == "__main__":
    raise SystemExit(main())
