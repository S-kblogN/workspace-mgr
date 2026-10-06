#!/usr/bin/env python3
"""Real CLI archive/publication/history E2E against a CI-owned MinIO service.

Requires a --features test-storage binary, the pinned DVC[s3] installation, and
the same WORKSPACE_MGR_BIN / WORKSPACE_MGR_E2E_ROOT / MINIO_* variables as the
ordinary E2E harness. This scenario uses its own bucket and temporary home; it
never reads user credentials or connects to an AWS endpoint.
"""

from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import shutil
import sys
from urllib.parse import urlsplit


HARNESS_PATH = Path(__file__).resolve().parents[1] / "tests" / "e2e" / "run.py"
SPEC = importlib.util.spec_from_file_location("workspace_mgr_e2e", HARNESS_PATH)
assert SPEC is not None and SPEC.loader is not None
e2e = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(e2e)

TASK = "20260712-121000-history-task"
BRANCH = "codex/history-task"
DESTINATION = f"2026/07/{TASK}"
RECEIPT = ".workspace-mgr-archive.json"


class ArchiveHarness(e2e.Harness):
    def __init__(self):
        super().__init__()
        if urlsplit(self.endpoint).hostname not in ("127.0.0.1", "localhost", "::1", "minio"):
            raise e2e.E2EFailure("archive E2E requires a local CI-owned MinIO endpoint")
        self.bucket = os.environ.get("MINIO_ARCHIVE_BUCKET", self.bucket + "-archive")
        self.env["WORKSPACE_MGR_STORAGE_PYTHON"] = sys.executable
        dvc_program = shutil.which("dvc") or str(Path(sys.executable).parent / "dvc")
        self.env["WORKSPACE_MGR_STORAGE_DVC"] = dvc_program
        self.unmapped_source_versions = []

    def namespace_versions(self, task):
        prefix = f"dvc/{task}/"
        entries = []
        for page in self.s3.get_paginator("list_object_versions").paginate(Bucket=self.bucket, Prefix=prefix):
            for section in ("Versions", "DeleteMarkers"):
                for item in page.get(section, []):
                    entries.append({
                        "key": item["Key"], "version_id": item["VersionId"],
                        "delete_marker": section == "DeleteMarkers",
                        "is_latest": item["IsLatest"],
                        "last_modified": item["LastModified"].isoformat(),
                        "size": item.get("Size"), "etag": item.get("ETag", "").strip('"') or None,
                    })
        entries.sort(key=lambda item: (item["key"], item["version_id"]))
        self.record("archive-s3-state", {"task": task, "versions": entries})
        return entries

    def exact_body(self, key, version):
        response = self.s3.get_object(Bucket=self.bucket, Key=key, VersionId=version)
        try:
            return response["Body"].read()
        finally:
            response["Body"].close()

    def fake_gh(self, merged_oid, head_oid):
        database = {BRANCH: [{
            "number": 7, "url": "https://example.invalid/owner/archive-fixture/pull/7",
            "state": "MERGED", "mergedAt": "2026-07-12T20:00:00Z",
            "mergeCommit": {"oid": merged_oid}, "headRefName": BRANCH,
            "headRefOid": head_oid,
            "baseRefName": "main", "isCrossRepository": False,
        }]}
        executable = self.root / "fake-gh"
        executable.write_text(
            "#!/usr/bin/env python3\nimport json, sys\n"
            f"database = json.loads({json.dumps(json.dumps(database))})\n"
            "head = sys.argv[sys.argv.index('--head') + 1]\n"
            "print(json.dumps(database.get(head, [])))\n",
            encoding="utf-8",
        )
        executable.chmod(0o755)
        self.env["WORKSPACE_MGR_TEST_GH"] = str(executable)

    def initialize(self):
        self.section("isolated versioned S3 and Git fixture")
        actual = self.run([sys.executable, "-c", "import dvc; print(dvc.__version__)"]).stdout.strip()
        self.check(actual == "3.67.1", "the archive fixture uses the pinned DVC version", version=actual)
        self.setup_s3()
        self.setup_repository()
        self.wm(self.shared, "init", "--s3-url", f"s3://{self.bucket}/dvc", "--s3-endpoint-url", self.endpoint)
        self.git(self.shared, "add", "-A")
        self.git(self.shared, "commit", "-m", "Initialize archive fixture")
        self.git(self.shared, "push", "origin", "main")

    def publish_original(self):
        self.section("published standalone and directory version histories")
        created = self.wm(
            self.shared, "task", "create", "history-task", "--timestamp", "20260712-121000",
            "--title", "Retain historical S3 payloads", "--purpose", "Exercise exact-version archival.",
        )
        task = Path(created["path"])
        self.document_task(task)
        (task / "single.txt").write_bytes(b"standalone version one\n")
        (task / "bundle" / "nested").mkdir(parents=True)
        (task / "bundle" / "alpha.txt").write_bytes(b"directory alpha version one\n")
        (task / "bundle" / "nested" / "beta.bin").write_bytes(b"unchanged nested beta\n")
        for output in ("single.txt", "bundle"):
            self.wm(task, "storage", "set", f"{TASK}/{output}", "--to", "s3", "--reason", "Exercise archive version history.")
        first = self.wm(task, "publish", "-m", "Publish the first exact payload versions")
        first_oid = first["remote_oid"]
        (task / "single.txt").write_bytes(b"standalone version two\n")
        (task / "bundle" / "alpha.txt").write_bytes(b"directory alpha version two\n")
        second = self.wm(task, "publish", "-m", "Publish changed standalone and directory versions")
        merged = self.merge_branch_to_main(BRANCH)
        self.wm(self.shared, "refresh")
        # The archived namespace also owns versions absent from the current
        # DVC pointer inventory. These must move with all payload generations.
        retired_key = f"dvc/{TASK}/retired.bin"
        for body in (b"retired generation one\n", b"retired generation two\n"):
            response = self.s3.put_object(Bucket=self.bucket, Key=retired_key, Body=body,
                                          Metadata={"purpose": "original historical metadata"}, ContentType="binary/archive-test")
            self.s3.put_object_tagging(Bucket=self.bucket, Key=retired_key, VersionId=response["VersionId"],
                                       Tagging={"TagSet": [{"Key": "archive purpose", "Value": "retain + history"}]})
        self.s3.delete_object(Bucket=self.bucket, Key=retired_key)
        # A published pointer remains readable through its exact payload even
        # when S3's current state is a delete marker above it.
        self.s3.delete_object(Bucket=self.bucket, Key=f"dvc/{TASK}/single.txt")
        self.git(self.shared, "push", "origin", "--delete", BRANCH)
        self.fake_gh(merged, second["remote_oid"])
        original = self.namespace_versions(TASK)
        self.check(sum(not row["delete_marker"] for row in original) == 7
                   and sum(row["delete_marker"] for row in original) >= 2,
                   "fixture retains five published payloads, two orphan payloads, and engine/generated delete markers", versions=original)
        return first_oid, original

    def organize_without_materialization(self, original):
        self.section("archive in a fresh infrastructure worktree without stored outputs")
        organizer = self.root / "organizing-shared"
        self.run(["git", "clone", self.remote_url, organizer])
        self.configure_git(organizer)
        self.check(not (organizer / TASK / "single.txt").exists() and not (organizer / TASK / "bundle").exists(),
                   "a fresh clone has only DVC pointers")
        preview = self.wm(organizer, "archive", TASK, "--dry-run")
        self.check(preview["tasks"][0]["destination"] == DESTINATION, "archive defaults to year/month folders")
        created = self.wm(
            organizer, "task", "create", "archive-history", "--kind", "infrastructure",
            "--title", "Archive a completed task", "--purpose", "Retain complete object history at its archived path.",
            "--scope", TASK, "--scope", DESTINATION, "--scope-note", "The user requested this archive source and destination.",
        )
        worktree = Path(created["path"])
        archived = self.wm(worktree, "archive", TASK)
        self.check(archived["status"] == "archived" and archived["remote_writes"] is False,
                   "archive is a local operation in the infrastructure task")
        self.check(not (worktree / TASK).exists() and (worktree / DESTINATION / "single.txt.dvc").is_file(),
                   "archive moves Git metadata to the selected date folder")
        self.check(not (worktree / DESTINATION / "single.txt").exists() and not (worktree / DESTINATION / "bundle").exists(),
                   "archive does not materialize absent S3 outputs")
        self.check(self.namespace_versions(TASK) == original and self.namespace_versions(DESTINATION) == [],
                   "archive and its preview leave S3 untouched")
        planned = json.loads((worktree / DESTINATION / RECEIPT).read_text())
        self.check(planned["status"] == "planned" and len(planned["versions"]) == len(original),
                   "the durable local plan includes the complete source history")
        return organizer, worktree, created["branch"]

    def check_copied_history(self, receipt, original):
        mapped = {(row["source_object"], row["source_version_id"]): row for row in receipt["versions"]}
        copied = self.namespace_versions(DESTINATION)
        self.check(len(mapped) == len(original) == len(copied), "every source payload and marker has one copied destination version")
        actual = {(row["key"], row["version_id"]): row for row in copied}
        for old in original:
            object_name = old["key"].removeprefix("dvc/")
            row = mapped[(object_name, old["version_id"])]
            destination_key = "dvc/" + row["destination_object"]
            new = actual[(destination_key, row["destination_version_id"])]
            self.check(row["source_last_modified"] == old["last_modified"], "receipt preserves the original timestamp", object=object_name)
            self.check(row["delete_marker"] == old["delete_marker"] == new["delete_marker"]
                       and old["is_latest"] == new["is_latest"], "copied marker type and current state are exact", object=object_name)
            self.check(row["destination_version_id"] != row["source_version_id"], "copied versions receive new exact IDs", object=object_name)
            if not old["delete_marker"]:
                self.check(self.exact_body(old["key"], old["version_id"]) == self.exact_body(destination_key, row["destination_version_id"]),
                           "copied payload bytes equal the exact original version", object=object_name)
                self.check(row["destination_etag"] == new["etag"] and row["size"] == new["size"],
                           "receipt records verified destination ETag and size", object=object_name)
                if object_name.endswith("/retired.bin"):
                    info = self.s3.head_object(Bucket=self.bucket, Key=destination_key, VersionId=row["destination_version_id"])
                    tags = self.s3.get_object_tagging(Bucket=self.bucket, Key=destination_key, VersionId=row["destination_version_id"])
                    self.check(info["Metadata"]["purpose"] == "original historical metadata"
                               and info["ContentType"] == "binary/archive-test"
                               and tags["TagSet"] == [{"Key": "archive purpose", "Value": "retain + history"}],
                               "historical user metadata and tags survive the copy")
        return copied

    def publish_and_retry(self, worktree, branch, original):
        self.section("copy-before-Git publication, rejection, and idempotent retry")
        reject_flag = self.install_rejecting_hook()
        reject_flag.write_text("reject archive publication\n")
        refused = self.wm(worktree, "publish", "-m", "Archive full S3 histories", expected=2)
        self.check("reject" in refused["stderr"].lower(), "the deliberate Git publication failure is visible")
        self.check(self.remote_ref(branch) is None, "failed Git publication creates no remote infrastructure branch")
        receipt = json.loads((worktree / DESTINATION / RECEIPT).read_text())
        self.check(receipt["status"] == "copied", "failed publication retains a completed copy receipt")
        copied = self.check_copied_history(receipt, original)
        self.check(self.namespace_versions(TASK) == original, "a failed Git publication leaves every source version untouched")
        self.check(not (worktree / DESTINATION / "single.txt").exists() and not (worktree / DESTINATION / "bundle").exists(),
                   "archive publication can copy without downloading task outputs")
        import yaml
        single = yaml.safe_load((worktree / DESTINATION / "single.txt.dvc").read_text())["outs"][0]
        version = single["cloud"]["workspace-mgr"]["version_id"]
        self.check(any(row["destination_object"] == DESTINATION + "/single.txt" and row["destination_version_id"] == version
                       for row in receipt["versions"]), "the standalone DVC pointer automatically binds the copied exact version")
        bundle = yaml.safe_load((worktree / DESTINATION / "bundle.dvc").read_text())["outs"][0]
        self.check(all(any(row["destination_object"] == DESTINATION + "/bundle/" + item["relpath"]
                              and row["destination_version_id"] == item["cloud"]["workspace-mgr"]["version_id"]
                          for row in receipt["versions"]) for item in bundle["files"]),
                   "every directory manifest entry automatically binds its copied version")
        # Model interruption after copying but before every local metadata
        # rewrite became durable. A copied receipt must repair an old binding
        # on retry, rather than treating its status as permission to skip it.
        base = self.remote_ref("main")
        old_pointer = self.remote_file(base, f"{TASK}/single.txt.dvc")
        (worktree / DESTINATION / "single.txt.dvc").write_text(old_pointer)
        # A concurrent writer can append history after this complete copy.
        # It must survive cleanup, and cannot become a latest-version fallback
        # for an older Git pointer whose exact original version was retired.
        for name, body in (("single.txt", b"unmapped standalone written after copying\n"),
                           ("late-unmapped.txt", b"unmapped brand-new source key\n")):
            response = self.s3.put_object(Bucket=self.bucket, Key=f"dvc/{TASK}/{name}", Body=body)
            self.unmapped_source_versions.append((f"{TASK}/{name}", response["VersionId"]))
        source_after_late_writes = self.namespace_versions(TASK)
        reject_flag.unlink()
        published = self.wm(worktree, "publish", "-m", "Retry archive Git publication")
        self.check(published["status"] == "pushed", "archive Git publication retries successfully")
        self.check(self.namespace_versions(DESTINATION) == copied, "retry creates no duplicate destination history")
        self.check(self.namespace_versions(TASK) == source_after_late_writes,
                   "all original and later source history remains protected while main uses the old task path")
        repaired = yaml.safe_load((worktree / DESTINATION / "single.txt.dvc").read_text())["outs"][0]
        self.check(repaired["cloud"]["workspace-mgr"]["version_id"] == version,
                   "retry repairs old path-bound metadata even when the private copy journal is complete")
        repeated = self.wm(worktree, "publish", "-m", "Verify completed archive is idempotent")
        self.check(repeated["status"] == "no_changes" and self.namespace_versions(DESTINATION) == copied,
                   "completed publication verifies history without recopying")
        return copied

    def merge_and_read_history(self, organizer, branch, first_oid, copied):
        self.section("canonical source retirement and historical Git hydration")
        merged = self.merge_branch_to_main(branch)
        refreshed = self.wm(organizer, "refresh")
        self.check(refreshed["status"] == "updated" and refreshed["new_oid"] == merged,
                   "refresh receives the merged archived tree")
        remaining = self.namespace_versions(TASK)
        self.check({(row["key"].removeprefix("dvc/"), row["version_id"]) for row in remaining}
                   == set(self.unmapped_source_versions),
                   "merged archival retires every mapped original version and preserves later source writes")
        retained = refreshed["storage"]["purge"]["retained_unmapped"]
        self.check({(row["object"], row["version_id"]) for row in retained} == set(self.unmapped_source_versions),
                   "refresh reports the exact unmapped source versions it retained")
        self.check(self.namespace_versions(DESTINATION) == copied, "source cleanup retains every destination historical version")
        self.check((organizer / DESTINATION / "single.txt").read_bytes() == b"standalone version two\n"
                   and (organizer / DESTINATION / "bundle" / "alpha.txt").read_bytes() == b"directory alpha version two\n",
                   "refresh hydrates exact published payloads beneath their copied current delete marker")
        history = self.root / "historical-consumer"
        self.run(["git", "clone", self.remote_url, history])
        self.configure_git(history)
        self.git(history, "checkout", "--detach", first_oid)
        self.check(not (history / TASK / "single.txt").exists() and not (history / TASK / "bundle").exists(),
                   "the historical clone starts with old pointers and an empty cache")
        hydrated = self.wm(history / TASK, "storage", "hydrate")
        self.check(hydrated["status"] == "hydrated", "old Git revisions hydrate through the canonical archive registry")
        self.check((history / TASK / "single.txt").read_bytes() == b"standalone version one\n"
                   and (history / TASK / "bundle" / "alpha.txt").read_bytes() == b"directory alpha version one\n"
                   and (history / TASK / "bundle" / "nested" / "beta.bin").read_bytes() == b"unchanged nested beta\n",
                   "historical hydration restores the original exact standalone and directory bytes")
        self.check(self.namespace_versions(TASK) == remaining and self.namespace_versions(DESTINATION) == copied,
                   "historical reads neither recreate source objects nor change destination history")

    def execute(self):
        self.initialize()
        first_oid, original = self.publish_original()
        organizer, worktree, branch = self.organize_without_materialization(original)
        copied = self.publish_and_retry(worktree, branch, original)
        self.merge_and_read_history(organizer, branch, first_oid, copied)
        summary = {"status": "passed", "assertions": self.assertions, "evidence": str(self.evidence_path),
                   "git_remote": self.remote_url, "s3_endpoint": self.endpoint, "bucket": self.bucket}
        self.record("summary", summary)
        print(json.dumps(summary, indent=2, sort_keys=True), flush=True)


def main():
    harness = None
    try:
        harness = ArchiveHarness()
        harness.execute()
        return 0
    except Exception as error:
        if harness is not None:
            harness.record("summary", {"status": "failed", "error": repr(error)})
        print(f"workspace-mgr archive E2E failed: {error}", file=sys.stderr, flush=True)
        return 1
    finally:
        if harness is not None:
            harness.close()


if __name__ == "__main__":
    raise SystemExit(main())
