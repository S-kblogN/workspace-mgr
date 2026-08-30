#!/usr/bin/env python3
"""System-level workspace-mgr E2E test.

The test talks to MinIO through the S3 API and to a bare Git repository through
git-daemon. It intentionally uses the compiled CLI as an opaque executable.
"""

from __future__ import annotations

import json
import os
import shutil
import socket
import subprocess
import sys
import time
import urllib.request
from pathlib import Path
from typing import Any, Iterable

import botocore.session
from botocore.config import Config as BotocoreConfig


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
                "DVC_NO_ANALYTICS": "true",
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

    def list_s3_versions(self) -> list[dict[str, Any]]:
        versions: list[dict[str, Any]] = []
        paginator = self.s3.get_paginator("list_object_versions")
        for page in paginator.paginate(Bucket=self.bucket, Prefix="dvc/"):
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
        (self.seed / "AGENTS.md").write_text(
            "# Existing repository policy\n\nPreserve this repository-specific rule.\n",
            encoding="utf-8",
        )
        self.git(self.seed, "add", "README.md", "AGENTS.md")
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

    def initialize_workspace(self) -> None:
        assert self.shared is not None
        self.section("init, adoption, configuration, instructions, and doctor")
        original_agents = (self.shared / "AGENTS.md").read_text(encoding="utf-8")
        rejected = self.wm(
            self.shared,
            "init",
            "--s3-url",
            f"s3://{self.bucket}/dvc",
            expected=2,
        )
        self.check("--adopt" in rejected["stderr"], "init refuses unmanaged AGENTS.md")
        self.check(not (self.shared / ".dvc").exists(), "failed init is mutation-free")

        common = (
            "init",
            "--profile",
            "shared-checkout",
            "--s3-url",
            f"s3://{self.bucket}/dvc",
            "--s3-endpoint-url",
            self.endpoint,
            "--adopt",
        )
        dry = self.wm(self.shared, *common, "--dry-run")
        self.check(dry["status"] == "dry_run", "init dry-run reports planned scaffolding")
        self.check(not (self.shared / ".workspace-mgr.toml").exists(), "init dry-run writes nothing")
        self.check(
            (self.shared / "AGENTS.md").read_text(encoding="utf-8") == original_agents,
            "init dry-run preserves AGENTS.md",
        )

        initialized = self.wm(self.shared, *common)
        self.check(initialized["status"] == "initialized", "repository initialized")
        config_text = (self.shared / ".workspace-mgr.toml").read_text(encoding="utf-8")
        dvc_config = (self.shared / ".dvc" / "config").read_text(encoding="utf-8")
        bootstrap = (self.shared / "AGENTS.md").read_text(encoding="utf-8")
        module = (
            self.shared / ".workspace-mgr" / "instructions" / "repository.md"
        ).read_text(encoding="utf-8")
        self.check("workspace-mgr instructions" in bootstrap, "thin AGENTS bootstrap installed")
        self.check(module == original_agents, "existing AGENTS content preserved as a module")
        self.check(self.remote_url not in config_text, "repository Git URL is not embedded in policy")
        self.check("[storage.s3]" in config_text, "S3 placement is configured")
        self.check("version_aware = true" in dvc_config, "internal storage engine is version-aware")
        self.check(f"s3://{self.bucket}/dvc" in dvc_config, "internal storage URL selects test bucket")
        self.check(self.endpoint in dvc_config, "internal storage endpoint selects virtual S3")
        self.check("[dvc]" not in config_text.lower(), "public configuration does not expose a DVC section")
        self.check("require_version_aware" not in config_text, "public configuration hides engine-specific versioning")
        self.check("python" not in config_text.lower(), "public configuration does not expose its adapter")
        repeated = self.wm(self.shared, "init", "--adopt")
        self.check(repeated["status"] == "no_changes", "init is idempotent")

        (self.shared / ".dvc" / "config").write_text(dvc_config + "# drift\n", encoding="utf-8")
        drifted = self.wm(self.shared, "doctor", expected=2)
        self.check("configuration drifted" in drifted["stdout"], "doctor rejects internal storage drift")
        repaired = self.wm(self.shared, "init", "--adopt")
        self.check(repaired["status"] == "initialized", "init repairs internal storage drift")
        self.check(
            (self.shared / ".dvc" / "config").read_text(encoding="utf-8") == dvc_config,
            "repair restores deterministic internal storage config",
        )

        self.git(self.shared, "add", "-A")
        staged = self.git(self.shared, "diff", "--cached", "--name-only").stdout.splitlines()
        self.check(".dvc/config.local" not in staged, "local storage credentials are not staged")
        self.git(self.shared, "commit", "-m", "Initialize managed workspace")
        self.git(self.shared, "push", "origin", "main")
        main_oid = self.remote_ref("main")
        self.check(main_oid is not None, "initialized main exists on Git server")

        config = self.wm(self.shared, "config", "show")
        self.check(config["publication"]["remote"] == "origin", "config resolves publication remote")
        self.check(config["storage"]["s3"]["url"] == f"s3://{self.bucket}/dvc", "config resolves S3 storage")
        self.check(config["storage"]["default"] == "auto", "config defaults to automatic placement")
        self.check("dvc" not in config, "public config JSON hides the internal storage engine")

        for topic in ("all", "core", "task", "publish", "storage", "infrastructure"):
            document = self.wm(self.shared, "instructions", topic)
            self.check(document["topic"] == topic, "instruction topic renders", topic=topic)
            self.check(len(document["policy_hash"]) == 64, "instruction policy hash is complete", topic=topic)
        all_instructions = self.wm(self.shared, "instructions")
        self.check(
            "Preserve this repository-specific rule" in all_instructions["markdown"],
            "adopted repository instructions are composed into output",
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
        self.check(created["branch"] == branch, "task branch follows configured prefix")
        self.check(task.joinpath("README.md").is_file(), "task README created")
        self.check(task.joinpath(".workspace-mgr-task.toml").is_file(), "task manifest created")
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

        (task / "notes.txt").write_text("task-only content\n", encoding="utf-8")
        (self.shared / "authorized.txt").write_text("authorized root content\n", encoding="utf-8")
        (self.shared / "unrelated.txt").write_text("another active task\n", encoding="utf-8")
        plan = self.wm(task, "plan")
        self.check(plan["status"] == "dry_run", "plan reports task changes")
        self.check(all(path.startswith(task_id + "/") for path in plan["changed_paths"]), "plan stays in task scope")
        self.check(self.remote_ref(branch) is None, "plan does not push target branch")
        self.check(self.list_s3_versions() == [], "plan does not write S3")

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
        self.check(self.remote_path_exists(commit, "authorized.txt"), "authorized extra scope exists in remote tree")
        self.check(not self.remote_path_exists(commit, "unrelated.txt"), "unrelated overlay is absent from remote tree")
        message = self.run(
            ["git", "--git-dir", self.remote, "show", "-s", "--format=%B", commit],
            cwd=self.root,
        ).stdout
        self.check("Scope-Authorization: authorized.txt" in message, "commit records scope authorization")
        self.assert_shared_head(base_oid)
        no_changes = self.wm(task, "plan")
        self.check(no_changes["status"] == "no_changes", "post-publish plan is clean")
        return task_id, task, branch

    def exercise_dvc(self, task_id: str, task: Path, branch: str) -> None:
        assert self.shared is not None
        self.section("Git/S3 placement, failure atomicity, hydrate, move, and reset")
        data = task / "data.bin"
        bundle = task / "bundle"
        bundle.mkdir()
        v1 = b"single-file version one\n"
        bundle_v1_a = b"bundle alpha version one\n"
        bundle_v1_b = b"bundle beta version one\n"
        data.write_bytes(v1)
        (bundle / "alpha.txt").write_bytes(bundle_v1_a)
        (bundle / "beta.txt").write_bytes(bundle_v1_b)
        remote_before = self.remote_ref(branch)
        dry = self.wm(
            task,
            "storage",
            "set",
            "--dry-run",
            f"{task_id}/data.bin",
            f"{task_id}/bundle",
            "--to",
            "s3",
            "--reason",
            "Retained E2E binary data.",
        )
        self.check(dry["status"] == "dry_run", "S3 placement dry-run succeeds")
        self.check(dry["remote_writes"] is False, "placement dry-run reports no remote writes")
        self.check(not task.joinpath("data.bin.dvc").exists(), "placement dry-run creates no metadata")
        self.check(self.remote_ref(branch) == remote_before, "placement dry-run leaves Git remote unchanged")
        self.check(self.list_s3_versions() == [], "placement dry-run leaves S3 empty")

        placed = self.wm(
            task,
            "storage",
            "set",
            f"{task_id}/data.bin",
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
        self.check(
            {item["path"] for item in statuses["placements"]}
            == {f"{task_id}/data.bin", f"{task_id}/bundle"},
            "storage status finds both explicit boundaries",
        )
        self.check(
            all(item["target"] == "s3" and item["selected_by"] == "explicit" for item in statuses["placements"]),
            "storage status explains explicit S3 placement",
        )
        tracked = self.wm(task, "publish", "-m", "Publish S3 file and directory")
        self.check(tracked["status"] == "pushed", "two S3 boundaries publish atomically")
        verification = tracked["storage"]["s3"]["verification"]
        self.check(verification["mode"] == "version-aware", "exact S3 version verification ran")
        self.check(len(verification["checked_objects"]) >= 3, "each payload object was exactly verified")
        data_pointer = task / "data.bin.dvc"
        bundle_pointer = task / "bundle.dvc"
        self.check(data_pointer.is_file() and bundle_pointer.is_file(), "file and directory pointers exist")
        self.check("version_id" in data_pointer.read_text(encoding="utf-8"), "file pointer records S3 version ID")
        self.check("version_id" in bundle_pointer.read_text(encoding="utf-8"), "directory pointer records S3 version IDs")
        tracked_oid = self.remote_ref(branch)
        assert tracked_oid is not None
        self.check(not self.remote_path_exists(tracked_oid, f"{task_id}/data.bin"), "DVC payload is absent from Git tree")
        self.check(not self.remote_path_exists(tracked_oid, f"{task_id}/bundle"), "DVC directory is absent from Git tree")
        self.check(self.remote_path_exists(tracked_oid, f"{task_id}/data.bin.dvc"), "file pointer is in Git tree")
        versions_v1 = self.list_s3_versions()
        self.check(len(versions_v1) >= 3, "MinIO contains DVC payload versions")
        self.check(all(item["version_id"] not in ("", "null") for item in versions_v1), "all S3 objects have version IDs")
        bodies_v1 = self.s3_bodies()
        for payload in (v1, bundle_v1_a, bundle_v1_b):
            self.check(payload in bodies_v1, "S3 contains exact version-one payload", payload=payload.decode().strip())
        self.check(self.wm(task, "plan")["status"] == "no_changes", "published S3 state is clean")

        v2 = b"single-file version two\n"
        bundle_v2_a = b"bundle alpha version two\n"
        bundle_v2_c = b"bundle gamma version two\n"
        data.write_bytes(v2)
        (bundle / "alpha.txt").write_bytes(bundle_v2_a)
        (bundle / "gamma.txt").write_bytes(bundle_v2_c)
        pointer_before_plan = data_pointer.read_bytes()
        s3_before_plan = self.list_s3_versions()
        remote_before_plan = self.remote_ref(branch)
        planned = self.wm(task, "plan")
        self.check(planned["status"] == "dry_run", "dirty DVC outputs appear in plan")
        self.check(set(planned["storage"]["s3"]["dirty_files"]) == {f"{task_id}/data.bin.dvc", f"{task_id}/bundle.dvc"}, "plan finds both dirty S3 boundaries")
        self.check(data_pointer.read_bytes() == pointer_before_plan, "plan does not rewrite DVC metadata")
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
        self.check(self.remote_ref(branch) == remote_before_plan, "S3 failure leaves remote Git ref unchanged")
        self.check(self.git(self.shared, "rev-parse", branch).stdout.strip() == remote_before_plan, "S3 failure leaves local target ref unchanged")
        self.check(self.list_s3_versions() == s3_before_plan, "S3 authentication failure uploads no object")

        published_v2 = self.wm(task, "publish", "-m", "Publish DVC version two")
        self.check(published_v2["status"] == "pushed", "retry after S3 failure succeeds")
        self.check(published_v2["storage"]["s3"]["verification"]["mode"] == "version-aware", "retry verifies exact S3 versions")
        versions_v2 = self.list_s3_versions()
        self.check(len(versions_v2) > len(versions_v1), "S3 retains additional immutable versions")
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
        self.check(len(versions_after_reject) > len(versions_before_reject), "DVC data is uploaded before Git publication")
        self.check(v3 in self.s3_bodies() and bundle_v3_a in self.s3_bodies(), "unreferenced retryable S3 versions contain exact payloads")
        reject_flag.unlink()
        retried = self.wm(task, "publish", "-m", "Retry Git publication after rejection")
        self.check(retried["status"] == "pushed", "Git publication retry succeeds")
        self.check(self.remote_ref(branch) == retried["commit_oid"], "retry reconciles local and remote refs")

        remote_before_missing = self.remote_ref(branch)
        data.unlink()
        missing = self.wm(task, "publish", "-m", "Do not interpret missing data as deletion", expected=2)
        self.check("missing locally" in missing["stderr"], "missing DVC output is rejected")
        self.check(self.remote_ref(branch) == remote_before_missing, "missing output leaves Git remote unchanged")
        self.check(not data.exists(), "failed missing-output publication does not synthesize data")

        cache = self.shared / ".dvc" / "cache"
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

        old_path = f"{task_id}/data.bin"
        new_path = f"{task_id}/moved.bin"
        move_dry = self.wm(task, "move", "--dry-run", old_path, new_path)
        self.check(move_dry["status"] == "dry_run", "move dry-run succeeds")
        self.check(data.exists() and not task.joinpath("moved.bin").exists(), "move dry-run changes no files")
        versions_before_move = self.list_s3_versions()
        remote_before_move = self.remote_ref(branch)
        moved = self.wm(task, "move", old_path, new_path)
        self.check(moved["status"] == "updated", "S3 boundary moves locally")
        self.check(moved["remote_writes"] is False, "move reports no remote writes")
        self.check(self.remote_ref(branch) == remote_before_move, "move leaves Git remote unchanged")
        self.check(self.list_s3_versions() == versions_before_move, "move leaves S3 unchanged")
        moved_output = task / "moved.bin"
        moved_pointer = task / "moved.bin.dvc"
        self.check(not data.exists() and not data_pointer.exists(), "old DVC boundary is removed")
        self.check(moved_output.read_bytes() == v3 and moved_pointer.is_file(), "moved DVC boundary preserves payload")
        moved_publish = self.wm(task, "publish", "-m", "Publish moved S3 boundary")
        self.check(moved_publish["status"] == "pushed", "moved S3 boundary publishes")
        moved_oid = self.remote_ref(branch)
        assert moved_oid is not None
        self.check(self.remote_path_exists(moved_oid, f"{task_id}/moved.bin.dvc"), "moved pointer exists in remote Git tree")
        self.check(not self.remote_path_exists(moved_oid, f"{task_id}/data.bin.dvc"), "old pointer is absent from remote Git tree")

        reset_dry = self.wm(task, "storage", "reset", "--dry-run", f"{task_id}/moved.bin")
        self.check(reset_dry["status"] == "dry_run", "placement reset dry-run succeeds")
        self.check(reset_dry["placements"][0]["target"] == "git", "automatic policy selects Git for small file")
        self.check(moved_pointer.is_file() and moved_output.is_file(), "reset dry-run preserves boundary")
        remote_before_reset = self.remote_ref(branch)
        reset = self.wm(task, "storage", "reset", f"{task_id}/moved.bin")
        self.check(reset["status"] == "updated", "reset returns path to automatic placement")
        self.check(reset["remote_writes"] is False, "reset performs no remote writes")
        self.check(self.remote_ref(branch) == remote_before_reset, "reset leaves Git remote unchanged")
        self.check(not moved_pointer.exists(), "reset to Git removes S3 metadata locally")
        self.check(moved_output.read_bytes() == v3, "reset preserves output")
        reset_publish = self.wm(task, "publish", "-m", "Publish automatic Git placement")
        self.check(reset_publish["status"] == "pushed", "Git placement publishes")
        untracked_oid = self.remote_ref(branch)
        assert untracked_oid is not None
        self.check(self.remote_path_exists(untracked_oid, f"{task_id}/moved.bin"), "reset output becomes ordinary Git content")
        self.check(not self.remote_path_exists(untracked_oid, f"{task_id}/moved.bin.dvc"), "reset S3 metadata is absent from Git")
        self.check(self.remote_path_exists(untracked_oid, f"{task_id}/bundle.dvc"), "other S3 boundary remains stored")
        self.check(self.wm(task, "plan")["status"] == "no_changes", "storage lifecycle ends cleanly")

    def refresh_and_cross_clone(self, task_id: str, task: Path, branch: str) -> None:
        assert self.shared is not None
        self.section("shared-checkout refresh and independent clone hydration")
        merged_oid = self.merge_branch_to_main(branch)
        original_main = self.git(self.shared, "rev-parse", "main").stdout.strip()
        self.check(original_main != merged_oid, "shared main remains stale before refresh")
        (self.shared / "README.md").write_text("# Active tracked overlay\n", encoding="utf-8")
        overlay = (self.shared / "README.md").read_bytes()
        unrelated = (self.shared / "unrelated.txt").read_bytes()
        bundle = task / "bundle"
        if bundle.exists():
            shutil.rmtree(bundle)
        cache = self.shared / ".dvc" / "cache"
        if cache.exists():
            shutil.rmtree(cache)

        dry = self.wm(self.shared, "refresh", "--dry-run")
        self.check(dry["status"] == "dry_run", "refresh dry-run sees incoming main")
        self.check(self.git(self.shared, "rev-parse", "main").stdout.strip() == original_main, "refresh dry-run does not move local main")
        self.check(not bundle.exists(), "refresh dry-run does not hydrate DVC output")
        refreshed = self.wm(self.shared, "refresh")
        self.check(refreshed["status"] == "updated", "refresh fast-forwards shared main")
        self.check(refreshed["new_oid"] == merged_oid, "refresh reports merged object ID")
        self.check(refreshed["storage"]["mode"] == "hydrate", "refresh uses managed-storage hydration")
        self.check(f"{task_id}/bundle.dvc" in refreshed["storage"]["changed_files"], "refresh identifies incoming storage metadata")
        self.assert_shared_head(merged_oid)
        self.check((self.shared / "README.md").read_bytes() == overlay, "refresh preserves tracked overlay")
        self.check((self.shared / "unrelated.txt").read_bytes() == unrelated, "refresh preserves unrelated untracked overlay")
        self.check((bundle / "alpha.txt").read_bytes() == b"bundle alpha version three\n", "refresh hydrates exact S3 directory version")
        staged = self.git(self.shared, "diff", "--cached", "--name-only").stdout
        self.check(staged == "", "refresh leaves shared index clean")
        self.check(self.wm(self.shared, "refresh")["status"] == "no_changes", "repeat refresh is idempotent")

        consumer = self.root / "consumer"
        self.run(["git", "clone", self.remote_url, consumer], cwd=self.root)
        self.configure_git(consumer)
        consumer_task = consumer / task_id
        self.check(not (consumer_task / "bundle").exists(), "fresh clone has no DVC payload")
        self.check((consumer_task / "moved.bin").read_bytes() == b"single-file version three\n", "fresh clone receives untracked Git payload")
        doctor = self.wm(consumer, "doctor")
        self.check(doctor["status"] == "ok", "fresh network clone passes doctor")
        hydrated = self.wm(consumer_task, "storage", "hydrate")
        self.check(hydrated["status"] == "hydrated", "fresh clone hydrates from MinIO")
        self.check((consumer_task / "bundle" / "alpha.txt").read_bytes() == b"bundle alpha version three\n", "cross-clone S3 hydration is exact")

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
        explicit_git = task / "explicit-git.bin"
        automatic_s3 = task / "automatic-s3.bin"
        explicit_git.write_bytes(b"g" * 10_485_761)
        automatic_s3.write_bytes(b"s" * 10_485_762)
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
        plan = self.wm(task, "plan")
        self.check(plan["status"] == "dry_run", "automatic S3 placement appears in plan")
        self.check(
            f"{task_id}/automatic-s3.bin"
            in plan["storage"]["placement"]["would_place_in_s3"],
            "plan routes the unplaced large file to S3",
        )
        self.check(not task.joinpath("automatic-s3.bin.dvc").exists(), "plan does not create S3 metadata")
        self.check(self.list_s3_versions() == versions_before, "plan performs no S3 upload")
        published = self.wm(task, "publish", "-m", "Publish automatic and explicit placement")
        self.check(published["status"] == "pushed", "mixed Git and S3 placement publishes")
        oid = self.remote_ref(branch)
        assert oid is not None
        self.check(self.remote_path_exists(oid, f"{task_id}/explicit-git.bin"), "explicit large file is stored in Git")
        self.check(not self.remote_path_exists(oid, f"{task_id}/automatic-s3.bin"), "automatic S3 payload is absent from Git")
        self.check(self.remote_path_exists(oid, f"{task_id}/automatic-s3.bin.dvc"), "automatic S3 metadata is stored in Git")
        self.check(len(self.list_s3_versions()) > len(versions_before), "automatic placement uploads a versioned S3 object")
        self.check(self.wm(task, "plan")["status"] == "no_changes", "mixed placement ends cleanly")

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
        self.setup_repository()
        self.initialize_workspace()
        task_id, task, branch = self.create_and_publish_task()
        self.exercise_dvc(task_id, task, branch)
        self.refresh_and_cross_clone(task_id, task, branch)
        self.exercise_automatic_and_explicit_git()
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
