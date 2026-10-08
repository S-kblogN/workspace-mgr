#!/usr/bin/env python3
"""Compare complete CLI doctor audits against an isolated loopback S3 fixture.

The release archive must be accompanied by its official .sha256 asset. No
external bucket is used. Each binary creates its own temporary repository and
product scaffold. Run a production build separately before supplying --current.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tarfile
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, unquote, urlsplit
from xml.sax.saxutils import escape
import zlib


TASK = "20261008-120000-doctor-benchmark"


def run(command, directory, environment):
    result = subprocess.run(command, cwd=directory, env=environment,
                            capture_output=True, text=True, check=False, timeout=180)
    if result.returncode:
        raise RuntimeError(f"{command[0]} failed ({result.returncode}): {result.stderr}")
    return result


def snapshot(repository):
    return {str(path.relative_to(repository)): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in repository.rglob("*") if path.is_file()
            and ".git" not in path.relative_to(repository).parts}


def release_binary(archive: Path, checksum: Path, destination: Path):
    records = checksum.read_text().splitlines()
    hashes = [line.split()[0] for line in records
              if len(line.split()) == 2 and line.split()[1].lstrip("*") == archive.name]
    actual = hashlib.sha256(archive.read_bytes()).hexdigest()
    if hashes != [actual]:
        raise RuntimeError(f"release archive does not match SHA256 manifest: {archive.name}")
    with tarfile.open(archive, "r:gz") as package:
        members = [member for member in package.getmembers()
                   if member.isfile() and Path(member.name).name == "workspace-mgr"]
        if len(members) != 1:
            raise RuntimeError("release archive must contain exactly one workspace-mgr binary")
        source = package.extractfile(members[0])
        if source is None:
            raise RuntimeError("release binary could not be read")
        binary = destination / "release-workspace-mgr"
        binary.write_bytes(source.read())
        binary.chmod(0o700)
    return binary, actual


class Bucket(ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 128

    def __init__(self, bodies, mode, delay):
        super().__init__(("127.0.0.1", 0), Handler)
        self.bodies, self.mode, self.delay = bodies, mode, delay
        self.lock = threading.Lock()
        self.reset()

    def reset(self):
        with self.lock:
            self.counters = {"payload_gets": 0, "heads": 0, "listings": 0,
                             "payload_bytes": 0, "active_requests": 0,
                             "max_concurrent_requests": 0}

    def counter(self, name, amount=1):
        with self.lock:
            self.counters[name] += amount

    def enter(self):
        with self.lock:
            self.counters["active_requests"] += 1
            self.counters["max_concurrent_requests"] = max(
                self.counters["max_concurrent_requests"], self.counters["active_requests"])


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def reply(self, body, headers=None, head=False):
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.end_headers()
        if not head:
            self.wfile.write(body)

    def do_HEAD(self):
        self.handle_object(True)

    def do_GET(self):
        self.handle_object(False)

    def handle_object(self, head):
        bucket = self.server
        bucket.enter()
        try:
            target = urlsplit(self.path)
            query = parse_qs(target.query, keep_blank_values=True)
            if "versioning" in query:
                self.reply(b"<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>")
                return
            if "versions" in query:
                bucket.counter("listings")
                prefix = query.get("prefix", [""])[0]
                rows = []
                for key, body in bucket.bodies.items():
                    if not key.startswith(prefix):
                        continue
                    rows.append(f"<Version><Key>{escape(key)}</Key><VersionId>v1</VersionId>"
                                f"<IsLatest>true</IsLatest><ETag>\"fixture-etag\"</ETag>"
                                f"<Size>{len(body)}</Size><LastModified>2026-10-08T00:00:00Z</LastModified></Version>")
                self.reply(("<ListVersionsResult><IsTruncated>false</IsTruncated>"
                            + "".join(rows) + "</ListVersionsResult>").encode())
                return
            key = unquote(target.path).removeprefix("/fixture-bucket/")
            if key not in bucket.bodies or query.get("versionId") != ["v1"]:
                self.send_error(404, "fixture exact version missing")
                return
            body = bucket.bodies[key]
            headers = {"x-amz-version-id": "v1", "ETag": '"fixture-etag"'}
            if head:
                bucket.counter("heads")
                # Delay HEAD too: no HEAD-concurrency metric depends on an
                # accidental scheduling race with an instantaneous response.
                time.sleep(bucket.delay)
                if self.headers.get("x-amz-checksum-mode") == "ENABLED":
                    headers["x-amz-checksum-type"] = "FULL_OBJECT"
                    if bucket.mode == "sha256":
                        headers["x-amz-checksum-sha256"] = base64.b64encode(hashlib.sha256(body).digest()).decode()
                    else:
                        headers["x-amz-checksum-crc32"] = base64.b64encode(zlib.crc32(body).to_bytes(4, "big")).decode()
            else:
                bucket.counter("payload_gets")
                bucket.counter("payload_bytes", len(body))
                time.sleep(bucket.delay)
            self.reply(body, headers, head)
        finally:
            bucket.counter("active_requests", -1)


def isolated_environment(root: Path):
    environment = {name: value for name, value in os.environ.items()
                   if not name.startswith(("AWS_", "GIT_", "WORKSPACE_MGR_"))
                   and name.upper() not in {"HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"}}
    # Child-only home/config directories: no developer AWS/Git configuration or
    # credential_process can enter this benchmark. All credentials are dummy.
    home = root / "home"
    home.mkdir()
    environment.update({"HOME": str(home), "XDG_CONFIG_HOME": str(home / ".config"),
                        "AWS_CONFIG_FILE": str(root / "empty-aws-config"),
                        "AWS_SHARED_CREDENTIALS_FILE": str(root / "empty-aws-credentials"),
                        "AWS_ACCESS_KEY_ID": "fixture-key", "AWS_SECRET_ACCESS_KEY": "fixture-secret",
                        "AWS_REGION": "us-east-1", "AWS_EC2_METADATA_DISABLED": "true",
                        "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull,
                        "WORKSPACE_MGR_FORMAT": "json"})
    (root / "empty-aws-config").write_text("")
    (root / "empty-aws-credentials").write_text("")
    git_binary = shutil.which("git")
    if git_binary is None:
        raise RuntimeError("git is required")
    wrapper = root / "bin"
    wrapper.mkdir()
    log = root / "git-processes.log"
    script = wrapper / "git"
    script.write_text("#!/bin/sh\nprintf '%s\\n' git >> \"$WM_BENCH_GIT_LOG\"\nexec "
                      + shlex.quote(git_binary) + ' "$@"\n')
    script.chmod(0o700)
    environment["WM_BENCH_GIT_LOG"] = str(log)
    environment["PATH"] = str(wrapper) + os.pathsep + environment.get("PATH", "")
    return environment, log


def benchmark(binary, label, mode, bodies, delay, root):
    directory = root / f"{label}-{mode}"
    directory.mkdir()
    environment, git_log = isolated_environment(directory)
    repository = directory / "repository"
    remote = directory / "origin.git"
    run(["git", "init", "--bare", "-q", str(remote)], directory, environment)
    run(["git", "init", "-q", "-b", "main", str(repository)], directory, environment)
    for key, value in [("user.name", "Doctor benchmark"), ("user.email", "fixture@example.invalid")]:
        run(["git", "config", key, value], repository, environment)
    run(["git", "remote", "add", "origin", str(remote)], repository, environment)
    (repository / "README.md").write_text("# Isolated doctor benchmark\n")
    run(["git", "add", "-A"], repository, environment)
    run(["git", "commit", "-qm", "Fixture seed"], repository, environment)
    run(["git", "push", "-q", "origin", "main"], repository, environment)
    bucket = Bucket(bodies, mode, delay)
    worker = threading.Thread(target=bucket.serve_forever, daemon=True)
    worker.start()
    try:
        endpoint = f"http://127.0.0.1:{bucket.server_address[1]}"
        run([str(binary), "manage", "--repo", str(repository), "--s3-url",
             "s3://fixture-bucket/root", "--s3-endpoint-url", endpoint], directory, environment)
        task = repository / TASK
        task.mkdir()
        (task / ".workspace-mgr-task.toml").write_text(
            f'schema_version = 2\nkind = "deliverable"\nid = "{TASK}"\n'
            f'slug = "doctor-benchmark"\npath = "{TASK}"\nbranch = "codex/doctor-benchmark"\n'
            'title = "Doctor benchmark"\npurpose = "Verify performance with intact checks"\nadditional_scopes = []\n')
        (task / "README.md").write_text("# Doctor benchmark\n")
        payload = task / "data"
        payload.mkdir()
        entries = []
        for key, body in bodies.items():
            name = key.rsplit("/", 1)[1]
            (payload / name).write_bytes(body)
            entries.append({"path": name, "checksum": {"algorithm": "md5", "digest": hashlib.md5(body).hexdigest()},
                            "size": len(body), "version": {"id": "v1", "etag": "fixture-etag"}})
        # Native directory digest is the MD5 of compact JSON tuples, sorted by
        # relative path; derive it through the documented serialization here.
        directory_rows = [[entry["path"], "md5", entry["checksum"]["digest"], entry["size"]]
                          for entry in entries]
        digest = hashlib.md5(json.dumps(directory_rows, separators=(",", ":")).encode()).hexdigest()
        manifest = {"schema_version": 1, "path": "data", "kind": "directory",
                    "checksum": {"algorithm": "md5", "digest": digest},
                    "size": sum(len(body) for body in bodies.values()), "entries": entries}
        (task / "data.wm-storage.json").write_text(json.dumps(manifest))
        (task / ".gitignore").write_text("/data/\n")
        run(["git", "add", "-A"], repository, environment)
        run(["git", "commit", "-qm", "Exact-version payload metadata"], repository, environment)
        bucket.reset()
        git_log.write_text("")
        before = snapshot(repository)
        started = time.perf_counter()
        output = subprocess.run([str(binary), "doctor", TASK, "--repo", str(repository)],
                                cwd=directory, env=environment, capture_output=True, text=True,
                                timeout=180)
        elapsed = time.perf_counter() - started
        if snapshot(repository) != before:
            raise RuntimeError(f"{label}/{mode} doctor modified repository files or private caches")
        report = json.loads(output.stdout)
        storage = report.get("storage", {})
        if output.returncode or report["status"] != "ok" or storage.get("status") != "ok":
            raise RuntimeError(f"{label}/{mode} audit failed: {json.dumps(report)}\n{output.stderr}")
        if storage.get("expected_objects") != len(bodies) or storage.get("remote_versions") != len(bodies) or storage.get("issues"):
            raise RuntimeError(f"{label}/{mode} audit skipped object checks: {storage}")
        counters = {key: value for key, value in bucket.counters.items() if key != "active_requests"}
        expected_gets = 0 if label == "current" and mode == "sha256" else len(bodies)
        if counters["payload_gets"] != expected_gets:
            raise RuntimeError(f"{label}/{mode} payload GET count {counters['payload_gets']} != {expected_gets}")
        return {"binary": label, "mode": mode, "wall_seconds": round(elapsed, 4),
                "git_processes": len(git_log.read_text().splitlines()), **counters,
                "status": report["status"], "storage_status": storage["status"],
                "expected_objects": storage["expected_objects"],
                "issues": storage["issues"],
                "remote_checksum_objects": storage.get("remote_checksum_objects"),
                "streamed_objects": storage.get("streamed_objects"),
                "streamed_bytes": storage.get("streamed_bytes"),
                "scratch_payload_bytes_estimate": counters["payload_bytes"] if label == "release" else 0}
    finally:
        bucket.shutdown()
        bucket.server_close()
        worker.join()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--release-archive", type=Path, required=True)
    parser.add_argument("--release-checksum", type=Path, required=True)
    parser.add_argument("--current", type=Path, default=Path("target/debug/workspace-mgr"))
    parser.add_argument("--objects", type=int, default=128)
    parser.add_argument("--bytes-per-object", type=int, default=65536)
    parser.add_argument("--delay-ms", type=float, default=20)
    parser.add_argument("--output", type=Path)
    arguments = parser.parse_args()
    if not 1 <= arguments.objects <= 1000 or arguments.bytes_per_object < 1 or arguments.delay_ms < 0:
        parser.error("objects must be 1..1000, bytes-per-object positive and delay-ms nonnegative")
    with tempfile.TemporaryDirectory(prefix="workspace-mgr-doctor-benchmark-") as temporary:
        root = Path(temporary)
        baseline, sha256 = release_binary(arguments.release_archive.resolve(),
                                          arguments.release_checksum.resolve(), root)
        current_source = arguments.current.resolve()
        if not current_source.is_file():
            parser.error(f"current binary does not exist: {current_source}")
        # Pin the build for all four runs, even if Cargo updates its target
        # while this benchmark is running in another terminal.
        current = root / "current-workspace-mgr"
        shutil.copyfile(current_source, current)
        current.chmod(0o700)
        current_sha256 = hashlib.sha256(current.read_bytes()).hexdigest()
        bodies = {f"root/{TASK}/data/object-{index:04d}":
                  hashlib.sha256(f"object-{index}".encode()).digest()
                  * (arguments.bytes_per_object // 32)
                  + b"x" * (arguments.bytes_per_object % 32)
                  for index in range(arguments.objects)}
        results = []
        for mode in ["crc32", "sha256"]:
            for label, binary in [("release", baseline), ("current", current)]:
                results.append(benchmark(binary, label, mode, bodies,
                                         arguments.delay_ms / 1000, root))
        document = {"release_archive": arguments.release_archive.name,
                    "release_archive_sha256": sha256,
                    "current_binary": str(current_source),
                    "current_binary_sha256": current_sha256,
                    "objects": arguments.objects, "bytes_per_object": arguments.bytes_per_object,
                    "delay_ms_per_object_request": arguments.delay_ms, "results": results}
        rendered = json.dumps(document, indent=2) + "\n"
        print(rendered, end="")
        if arguments.output:
            arguments.output.write_text(rendered)


if __name__ == "__main__":
    main()
