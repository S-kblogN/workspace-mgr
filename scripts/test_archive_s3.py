#!/usr/bin/env python3
"""Real CLI archive/rename/publication/history E2E against a CI-owned MinIO service.

Requires a --features test-storage binary, boto3 for isolated S3 verification, and
the same WORKSPACE_MGR_BIN / WORKSPACE_MGR_E2E_ROOT / MINIO_* variables as the
ordinary E2E harness. This scenario uses its own bucket and temporary home; it
never reads user credentials or connects to an AWS endpoint.
"""

from __future__ import annotations

import importlib.util
import hashlib
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
RENAME_TASK = "20260713-121000-rename-history"
RENAME_BRANCH = "codex/rename-history"
RENAMED_TASK = "20260713-121000-renamed-history"
RENAMED_ARCHIVE = f"2026/07/{RENAMED_TASK}"


class ArchiveHarness(e2e.Harness):
    def __init__(self):
        super().__init__()
        if urlsplit(self.endpoint).hostname not in ("127.0.0.1", "localhost", "::1", "minio"):
            raise e2e.E2EFailure("archive E2E requires a local CI-owned MinIO endpoint")
        self.bucket = os.environ.get("MINIO_ARCHIVE_BUCKET", self.bucket + "-archive")
        # The obsolete Python override deliberately selects a missing program.
        # The test-only storage fault injector remains unset for these real flows.
        self.env["WORKSPACE_MGR_STORAGE_PYTHON"] = str(self.root / "python-must-not-run")
        self.env.pop("WORKSPACE_MGR_TEST_STORAGE_HOOK", None)
        self.unmapped_source_versions = []

    def namespace_versions(self, task):
        prefix = f"objects/{task}/"
        entries = self.versions_under(prefix)
        self.record("archive-s3-state", {"task": task, "versions": entries})
        return entries

    def versions_under(self, prefix):
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
        return entries

    def registry_versions(self):
        key = f"objects/.workspace-mgr/archive/{hashlib.sha256(TASK.encode()).hexdigest()}.json"
        return [row for row in self.versions_under(key) if row["key"] == key]

    def registry_binding(self, receipt, kind="archive-registry"):
        identity = json.dumps([receipt["bucket"], receipt["remote_prefix"], receipt["source"]],
                              ensure_ascii=False, separators=(",", ":")).encode()
        reference = "refs/tags/workspace-mgr/" + kind + "/" + hashlib.sha256(identity).hexdigest()
        listing = self.run(["git", "ls-remote", "--refs", self.remote_url, reference]).stdout.strip()
        rows = [line.split("\t") for line in listing.splitlines() if line]
        self.check(len(rows) <= 1 and all(len(row) == 2 and row[1] == reference for row in rows),
                   "archive ownership has one exact coordination ref", reference=reference)
        return rows[0][0] if rows else None

    def install_archive_rejecting_hook(self):
        reject_flag = self.install_rejecting_hook()
        hook = self.remote / "hooks" / "pre-receive"
        hook.write_text(
            "#!/bin/sh\n"
            "while read -r old new ref; do\n"
            "  if test -f \"$GIT_DIR/workspace-mgr-e2e-reject\"; then\n"
            "    case \"$ref\" in refs/heads/*)\n"
            "      echo 'workspace-mgr E2E intentional branch rejection' >&2; exit 1;; esac\n"
            "  fi\n"
            "done\nexit 0\n",
            encoding="utf-8",
        )
        hook.chmod(0o755)
        return reject_flag

    def exact_body(self, key, version):
        response = self.s3.get_object(Bucket=self.bucket, Key=key, VersionId=version)
        try:
            return response["Body"].read()
        finally:
            response["Body"].close()

    def fake_gh(self, merged_oid, head_oid, branch=BRANCH, number=7):
        database = {branch: [{
            "number": number, "url": f"https://example.invalid/owner/archive-fixture/pull/{number}",
            "state": "MERGED", "mergedAt": "2026-07-12T20:00:00Z",
            "mergeCommit": {"oid": merged_oid}, "headRefName": branch,
            "headRefOid": head_oid,
            "baseRefName": "main", "isCrossRepository": False,
        }]}
        executable = self.root / "fake-gh"
        executable.write_text(
            "#!/usr/bin/env python3\nimport json, sys\n"
            f"database = json.loads({json.dumps(json.dumps(database))})\n"
            "if sys.argv[1] == 'api':\n"
            "    if '/commits/' in sys.argv[-1] and sys.argv[-1].split('?')[0].endswith('/pulls'):\n"
            "        row = next(iter(database.values()))[0]\n"
            "        print(json.dumps([dict(number=row['number'], html_url=row['url'], state='closed', merged_at=row['mergedAt'], merge_commit_sha=row['mergeCommit']['oid'], head=dict(ref=row['headRefName'], sha=row['headRefOid'], repo=dict(full_name='owner/archive-fixture')), base=dict(ref='main', sha=row['mergeCommit']['oid'], repo=dict(full_name='owner/archive-fixture')))]))\n"
            "    else:\n"
            "        print(json.dumps({'protected': False}))\n"
            "    sys.exit(0)\n"
            "if sys.argv[1:3] == ['pr', 'view']:\n"
            "    print(json.dumps(next(iter(database.values()))[0]))\n"
            "    sys.exit(0)\n"
            "head = sys.argv[sys.argv.index('--head') + 1]\n"
            "print(json.dumps(database.get(head, [])))\n",
            encoding="utf-8",
        )
        executable.chmod(0o755)
        self.env["WORKSPACE_MGR_TEST_GH"] = str(executable)
        self.run(["git", "--git-dir", self.remote, "update-ref", f"refs/pull/{number}/head", head_oid])

    def initialize(self):
        self.section("isolated versioned S3 and Git fixture")
        self.setup_s3()
        self.setup_repository()
        self.wm(self.shared, "manage", "--s3-url", f"s3://{self.bucket}/objects", "--s3-endpoint-url", self.endpoint)
        self.git(self.shared, "add", "-A")
        self.git(self.shared, "commit", "-m", "Initialize archive fixture")
        self.git(self.shared, "push", "origin", "main")
        self.check(not Path(self.env["WORKSPACE_MGR_STORAGE_PYTHON"]).exists()
                   and "WORKSPACE_MGR_TEST_STORAGE_HOOK" not in self.env,
                   "native storage initialization ignores a missing legacy Python runtime override")

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
        self.git(self.seed, "fetch", "origin", BRANCH)
        self.git(self.seed, "tag", "archive-historical-snapshot", first_oid)
        self.git(self.seed, "push", "origin", "refs/tags/archive-historical-snapshot")
        (task / "single.txt").write_bytes(b"standalone version two\n")
        (task / "bundle" / "alpha.txt").write_bytes(b"directory alpha version two\n")
        second = self.wm(task, "publish", "-m", "Publish changed standalone and directory versions")
        merged = self.merge_branch_to_main(BRANCH)
        self.wm(self.shared, "refresh")
        # The archived namespace also owns versions absent from the current
        # native manifest inventory. These must move with all payload generations.
        retired_key = f"objects/{TASK}/retired.bin"
        for body in (b"retired generation one\n", b"retired generation two\n"):
            response = self.s3.put_object(Bucket=self.bucket, Key=retired_key, Body=body,
                                          Metadata={"purpose": "original historical metadata"}, ContentType="binary/archive-test")
            self.s3.put_object_tagging(Bucket=self.bucket, Key=retired_key, VersionId=response["VersionId"],
                                       Tagging={"TagSet": [{"Key": "archive purpose", "Value": "retain + history"}]})
        self.s3.delete_object(Bucket=self.bucket, Key=retired_key)
        # A published pointer remains readable through its exact payload even
        # when S3's current state is a delete marker above it.
        self.s3.delete_object(Bucket=self.bucket, Key=f"objects/{TASK}/single.txt")
        self.fake_gh(merged, second["remote_oid"])
        original = self.namespace_versions(TASK)
        self.check(sum(not row["delete_marker"] for row in original) == 7
                   and sum(row["delete_marker"] for row in original) >= 2,
                   "fixture retains five published payloads, two orphan payloads, and engine/generated delete markers", versions=original)
        return first_oid, original

    def organize_without_materialization(self, original):
        self.section("archive in a fresh main checkout without stored outputs")
        organizer = self.root / "organizing-shared"
        self.run(["git", "clone", self.remote_url, organizer])
        self.configure_git(organizer)
        self.check(not (organizer / TASK / "single.txt").exists() and not (organizer / TASK / "bundle").exists(),
                   "a fresh clone has only native manifests")
        self.original_metadata = {
            name: (organizer / TASK / name).read_bytes()
            for name in (".workspace-mgr-task.toml", "single.txt.wm-storage.json", "bundle.wm-storage.json")
        }
        ignored = organizer / TASK / "__pycache__" / "retained-local.bin"
        ignored.parent.mkdir()
        ignored.write_bytes(b"ignored local contents must survive cancellation\n")
        preview = self.wm(organizer, "archive", TASK, "--dry-run")
        self.check(preview["tasks"][0]["destination"] == DESTINATION, "archive defaults to year/month folders")
        created = self.wm(
            organizer, "task", "create", "archive-history", "--kind", "infrastructure",
            "--title", "Archive a completed task", "--purpose", "Retain complete object history at its archived path.",
            "--scope", TASK, "--scope", DESTINATION, "--scope-note", "The user requested this archive source and destination.",
        )
        worktree = Path(created["path"])
        manifest = created["manifest"]
        self.check(worktree == organizer.resolve()
                   and self.git(worktree, "branch", "--show-current").stdout.strip() == "main",
                   "infrastructure archive works in the shared main checkout")
        index_before = self.git(worktree, "ls-files", "--stage").stdout
        archived = self.wm(worktree, "archive", TASK, "--manifest", manifest)
        self.check(archived["status"] == "archived" and archived["remote_writes"] is False,
                   "archive is a local operation in the infrastructure task")
        self.check(not (worktree / TASK).exists() and (worktree / DESTINATION / "single.txt.wm-storage.json").is_file(),
                   "archive moves Git metadata to the selected date folder")
        self.check(not (worktree / DESTINATION / "single.txt").exists() and not (worktree / DESTINATION / "bundle").exists(),
                   "archive does not materialize absent S3 outputs")
        self.check(self.namespace_versions(TASK) == original and self.namespace_versions(DESTINATION) == [],
                   "archive and its preview leave S3 untouched")
        planned = json.loads((worktree / DESTINATION / RECEIPT).read_text())
        self.check(planned["status"] == "planned" and len(planned["versions"]) == len(original),
                   "the durable local plan includes the complete source history")
        self.check(self.git(worktree, "ls-files", "--stage").stdout == index_before,
                   "archive leaves the shared Git index untouched")
        return organizer, worktree, created["branch"], manifest

    def cancel_failed_publication(self, worktree, branch, original, manifest):
        self.section("failed archive publication cancels without remote or local residue")
        index_before = self.git(worktree, "ls-files", "--stage").stdout
        reject_flag = self.install_archive_rejecting_hook()
        reject_flag.write_text("reject archive branch but allow registry coordination\n")
        refused = self.wm(worktree, "publish", "--manifest", manifest,
                          "-m", "Exercise lossless archive cancellation", expected=2)
        self.check("reject" in refused["stderr"].lower() and self.remote_ref(branch) is None,
                   "fixture rejects task publication after the registry and copies exist")
        receipt = json.loads((worktree / DESTINATION / RECEIPT).read_text())
        copied = self.check_copied_history(receipt, original)
        registry = self.registry_versions()
        binding = self.registry_binding(receipt)
        reservation = self.registry_binding(receipt, "archive-copy")
        self.check(registry and binding is not None and reservation is not None,
                   "failed task push retains its copy reservation, registry and immutable canonical binding")
        git_control = ["git", "--git-dir", self.remote]
        control_type = self.run([*git_control, "cat-file", "-t", binding]).stdout.strip()
        self.check(control_type == "commit", "new registry bindings use a Git commit")
        ancestry = self.run([*git_control, "rev-list", "--parents", "--max-count=1", binding]).stdout.split()
        self.check(ancestry == [binding], "registry control commits have no parent task history")
        entries = self.run([*git_control, "ls-tree", "-z", binding]).stdout.rstrip("\0").split("\0")
        header, _, name = entries[0].partition("\t")
        self.check(len(entries) == 1 and header.startswith("100644 blob ")
                   and name == "workspace-mgr-control.json",
                   "registry control commits contain only one regular control JSON file")
        canonical = self.run([*git_control, "show", f"{binding}:workspace-mgr-control.json"]).stdout
        self.check(json.loads(canonical) == receipt, "coordination commit binds the exact complete copied receipt")
        self.check(not (worktree / DESTINATION / "single.txt").exists()
                   and not (worktree / DESTINATION / "bundle").exists(),
                   "server-side history copy does not materialize absent outputs")
        self.wm(worktree / DESTINATION, "storage", "hydrate")
        preview = self.wm(worktree, "archive", TASK, "--cancel", "--manifest", manifest, "--dry-run")
        self.check(preview["status"] == "dry_run"
                   and self.namespace_versions(TASK) == original
                   and self.namespace_versions(DESTINATION) == copied
                   and self.registry_versions() == registry
                   and self.registry_binding(receipt) == binding
                   and self.registry_binding(receipt, "archive-copy") == reservation
                   and (worktree / DESTINATION).is_dir() and not (worktree / TASK).exists(),
                   "cancel preview preserves directories, every object version, registry history and binding")
        cancelled = self.wm(worktree, "archive", TASK, "--cancel", "--manifest", manifest)
        self.check(cancelled["status"] == "cancelled"
                   and self.namespace_versions(TASK) == original
                   and self.namespace_versions(DESTINATION) == []
                   and self.registry_versions() == []
                   and self.registry_binding(receipt) is None
                   and self.registry_binding(receipt, "archive-copy") is None,
                   "cancel removes copied payloads, markers, registry versions and both owned Git claims without source writes")
        restored = worktree / TASK
        self.check(restored.is_dir() and not (worktree / DESTINATION).exists()
                   and not (restored / RECEIPT).exists()
                   and all((restored / name).read_bytes() == before for name, before in self.original_metadata.items())
                   and (restored / "__pycache__" / "retained-local.bin").read_bytes()
                       == b"ignored local contents must survive cancellation\n"
                   and (restored / "single.txt").read_bytes() == b"standalone version two\n"
                   and (restored / "bundle" / "alpha.txt").read_bytes() == b"directory alpha version two\n"
                   and self.git(worktree, "ls-files", "--stage").stdout == index_before,
                   "cancel restores original metadata bytes and preserves ignored/hydrated payloads and the shared index")
        repeated = self.wm(worktree, "archive", TASK, "--cancel", "--manifest", manifest)
        self.check(repeated["status"] == "no_changes"
                   and self.namespace_versions(TASK) == original
                   and self.namespace_versions(DESTINATION) == []
                   and self.registry_versions() == [] and self.registry_binding(receipt) is None
                   and self.registry_binding(receipt, "archive-copy") is None,
                   "completed cancellation is idempotent and leaves no remote residue")
        reject_flag.unlink()
        # This fixture deliberately starts the next attempt without materialized
        # outputs, after proving their bytes survived the previous cancellation.
        (restored / "single.txt").unlink()
        shutil.rmtree(restored / "bundle")
        archived = self.wm(worktree, "archive", TASK, "--manifest", manifest)
        self.check(archived["status"] == "archived" and self.namespace_versions(DESTINATION) == [],
                   "a cancelled archive starts a fresh transaction with no retained destination copies")

    def check_copied_history(self, receipt, original):
        mapped = {(row["source_object"], row["source_version_id"]): row for row in receipt["versions"]}
        copied = self.namespace_versions(receipt["destination"])
        self.check(len(mapped) == len(original) == len(copied), "every source payload and marker has one copied destination version")
        actual = {(row["key"], row["version_id"]): row for row in copied}
        for old in original:
            object_name = old["key"].removeprefix("objects/")
            row = mapped[(object_name, old["version_id"])]
            destination_key = "objects/" + row["destination_object"]
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

    def publish_and_retry(self, worktree, branch, original, manifest):
        self.section("copy-before-Git publication, rejection, and idempotent retry")
        reject_flag = self.install_archive_rejecting_hook()
        reject_flag.write_text("reject archive publication\n")
        refused = self.wm(worktree, "publish", "--manifest", manifest, "-m", "Archive full S3 histories", expected=2)
        self.check("reject" in refused["stderr"].lower(), "the deliberate Git publication failure is visible")
        self.check(self.remote_ref(branch) is None, "failed Git publication creates no remote infrastructure branch")
        receipt = json.loads((worktree / DESTINATION / RECEIPT).read_text())
        self.check(receipt["status"] == "copied", "failed publication retains a completed copy receipt")
        self.check(self.registry_binding(receipt, "archive-copy") is not None,
                   "the fresh attempt acquires a source copy reservation before creating destination history")
        copied = self.check_copied_history(receipt, original)
        self.check(self.namespace_versions(TASK) == original, "a failed Git publication leaves every source version untouched")
        self.check(not (worktree / DESTINATION / "single.txt").exists() and not (worktree / DESTINATION / "bundle").exists(),
                   "archive publication can copy without downloading task outputs")
        single = json.loads((worktree / DESTINATION / "single.txt.wm-storage.json").read_text())
        version = single["version"]["id"]
        self.check(any(row["destination_object"] == DESTINATION + "/single.txt" and row["destination_version_id"] == version
                       for row in receipt["versions"]), "the standalone native manifest automatically binds the copied exact version")
        bundle = json.loads((worktree / DESTINATION / "bundle.wm-storage.json").read_text())
        self.check(all(any(row["destination_object"] == DESTINATION + "/bundle/" + item["path"]
                              and row["destination_version_id"] == item["version"]["id"]
                          for row in receipt["versions"]) for item in bundle["entries"]),
                   "every directory manifest entry automatically binds its copied version")
        # Model interruption after copying but before every local metadata
        # rewrite became durable. A copied receipt must repair an old binding
        # on retry, rather than treating its status as permission to skip it.
        base = self.remote_ref("main")
        old_pointer = self.remote_file(base, f"{TASK}/single.txt.wm-storage.json")
        (worktree / DESTINATION / "single.txt.wm-storage.json").write_text(old_pointer)
        # A concurrent writer can append history after this complete copy.
        # It must block completed retirement, and cannot become a latest-version
        # fallback for an older Git pointer whose exact version was retired.
        for name, body in (("single.txt", b"unmapped standalone written after copying\n"),
                           ("late-unmapped.txt", b"unmapped brand-new source key\n")):
            response = self.s3.put_object(Bucket=self.bucket, Key=f"objects/{TASK}/{name}", Body=body)
            self.unmapped_source_versions.append((f"{TASK}/{name}", response["VersionId"]))
        marker = self.s3.delete_object(Bucket=self.bucket, Key=f"objects/{TASK}/late-unmapped.txt")
        self.unmapped_source_versions.append((f"{TASK}/late-unmapped.txt", marker["VersionId"]))
        source_after_late_writes = self.namespace_versions(TASK)
        reject_flag.unlink()
        published = self.wm(worktree, "publish", "--manifest", manifest, "-m", "Retry archive Git publication")
        self.check(published["status"] == "pushed", "archive Git publication retries successfully")
        self.check(published["storage"]["purge"]["status"] == "cleanup_pending"
                   and published["storage"]["purge"].get("pending_prefixes") == [TASK]
                   and any(warning["code"] == "s3-cleanup-pending"
                           for warning in published.get("warnings", [])),
                   "pre-merge publication retains a durable complete-prefix intent and explicit cleanup warning")
        self.check(self.namespace_versions(DESTINATION) == copied, "retry creates no duplicate destination history")
        self.check(self.namespace_versions(TASK) == source_after_late_writes,
                   "all original and later source history remains protected while main uses the old task path")
        repaired = json.loads((worktree / DESTINATION / "single.txt.wm-storage.json").read_text())
        self.check(repaired["version"]["id"] == version,
                   "retry repairs old path-bound metadata even when the private copy journal is complete")
        repeated = self.wm(worktree, "publish", "--manifest", manifest, "-m", "Verify completed archive is idempotent")
        self.check(repeated["status"] == "no_changes" and self.namespace_versions(DESTINATION) == copied,
                   "completed publication verifies history without recopying")
        return copied

    def merge_and_read_history(self, organizer, branch, first_oid, copied):
        self.section("canonical source retirement and historical Git hydration")
        merged = self.merge_branch_to_main(branch)
        cleaner = self.root / "fresh-retirement-clone"
        self.run(["git", "clone", self.remote_url, cleaner])
        self.configure_git(cleaner)
        self.check(not (cleaner / ".workspace-mgr" / "local" / "archive").exists(),
                   "fresh retirement clone has no private copy journal or archive attempt")
        refreshed = self.wm(cleaner, "refresh")
        self.check(refreshed["status"] in {"no_changes", "branches_cleaned"} and refreshed["new_oid"] == merged,
                   "fresh clone already on merged main retires history without a private copy journal or queue")
        self.check(any(row["branch"] == BRANCH and row["remote"]
                       for row in refreshed["branch_cleanup"]["deleted"]),
                   "refresh removes the verified merged source branch before pending S3 retirement")
        self.check(self.remote_ref(BRANCH) is None,
                   "automatic branch cleanup removes the old source's last remote branch protection")
        remaining = self.namespace_versions(TASK)
        self.check({(row["key"].removeprefix("objects/"), row["version_id"]) for row in remaining}
                   == set(self.unmapped_source_versions),
                   "mapped retirement preserves every unreviewed concurrent generation")
        retained = refreshed["storage"]["purge"]["retained_unmapped"]
        self.check(refreshed["storage"]["purge"]["status"] == "blocked_unmapped"
                   and {(row["object"], row["version_id"]) for row in retained} == set(self.unmapped_source_versions)
                   and set(self.unmapped_source_versions).issubset({(row["object"], row["version_id"])
                       for row in refreshed["storage"]["purge"]["pending"]})
                   and any(warning["code"] == "s3-cleanup-blocked-unmapped"
                           for warning in refreshed.get("warnings", [])),
                   "successful Git synchronization explicitly blocks archive completion and durably queues unknown versions")
        # Model the user's explicit reconciliation of these fixture-owned
        # competing writes. The product itself must never erase unknown bytes.
        for object_name, version in self.unmapped_source_versions:
            self.s3.delete_object(Bucket=self.bucket, Key="objects/" + object_name, VersionId=version)
        completed = self.wm(cleaner, "refresh")
        self.check(completed["storage"]["purge"]["status"] == "complete"
                   and completed["storage"]["purge"]["pending"] == []
                   and completed["storage"]["purge"].get("pending_prefixes", []) == []
                   and self.namespace_versions(TASK) == [],
                   "retry completes only after the entire old prefix has no data versions or delete markers")
        self.check(self.run(["git", "ls-remote", "--refs", self.remote_url,
                            "refs/tags/archive-historical-snapshot"]).stdout.strip(),
                   "historical Git tag remains while its mapped source payload prefix is empty")
        self.wm(organizer, "refresh")
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
        self.check(self.namespace_versions(TASK) == [] and self.namespace_versions(DESTINATION) == copied,
                   "historical reads neither recreate source objects nor change destination history")

    def check_retained_copies(self, receipt, bodies):
        actual = {(row["key"], row["version_id"]): row
                  for row in self.namespace_versions(receipt["destination"])}
        for row in receipt["versions"]:
            identity = ("objects/" + row["destination_object"], row["destination_version_id"])
            self.check(identity in actual and actual[identity]["delete_marker"] == row["delete_marker"],
                       "active rename retains each exact copied payload and marker", identity=identity)
            if not row["delete_marker"]:
                self.check(self.exact_body(*identity) == bodies[(row["source_object"], row["source_version_id"])],
                           "retained rename history keeps its original opaque bytes", identity=identity)

    def rename_and_retire_history(self):
        self.section("versioned task rename copies full history without an unchanged payload upload")
        self.wm(self.shared, "refresh")
        created = self.wm(
            self.shared, "task", "create", "rename-history", "--timestamp", "20260713-121000",
            "--title", "Exercise rename history retention", "--purpose", "Preserve copied history during later active work.",
        )
        task = Path(created["path"])
        self.document_task(task)
        (task / "single.txt").write_bytes(b"rename standalone version one\n")
        (task / "bundle").mkdir()
        (task / "bundle" / "alpha.txt").write_bytes(b"rename alpha version one\n")
        (task / "bundle" / "beta.bin").write_bytes(b"rename historical beta\n")
        for output in ("single.txt", "bundle"):
            self.wm(task, "storage", "set", f"{RENAME_TASK}/{output}", "--to", "s3",
                    "--reason", "Exercise versioned rename history.")
        first = self.wm(task, "publish", "-m", "Publish original rename payloads")
        first_oid = first["remote_oid"]
        self.git(self.seed, "fetch", "origin", RENAME_BRANCH)
        self.git(self.seed, "tag", "rename-historical-snapshot", first_oid)
        self.git(self.seed, "push", "origin", "refs/tags/rename-historical-snapshot")
        (task / "single.txt").write_bytes(b"rename standalone version two\n")
        (task / "bundle" / "alpha.txt").write_bytes(b"rename alpha version two\n")
        with (task / "record.md").open("a", encoding="utf-8") as record:
            record.write("\nPublish second exact versions before changing the task topic.\n")
        self.wm(task, "publish", "-m", "Publish second rename payload generations")
        # Keep this task active: its first merge happens only after the rename
        # and subsequent payload edits. A merged deliverable cannot be renamed.
        # Full namespace history includes opaque keys absent from every current
        # pointer and a current marker. Neither may be dropped by a rename.
        orphan_key = f"objects/{RENAME_TASK}/gone.bin"
        for body in (b"rename orphan generation one\n", b"rename orphan generation two\n"):
            self.s3.put_object(Bucket=self.bucket, Key=orphan_key, Body=body)
        self.s3.delete_object(Bucket=self.bucket, Key=orphan_key)
        original = self.namespace_versions(RENAME_TASK)
        bodies = {(row["key"].removeprefix("objects/"), row["version_id"]):
                  self.exact_body(row["key"], row["version_id"])
                  for row in original if not row["delete_marker"]}
        self.check(sum(not row["delete_marker"] for row in original) == 7
                   and any(row["delete_marker"] for row in original),
                   "rename fixture has multiple payload generations and orphan delete-marker history")
        preview = self.wm(task, "task", "rename", "renamed-history", "--dry-run")
        self.check(preview["new_path"] == RENAMED_TASK
                   and self.namespace_versions(RENAME_TASK) == original
                   and self.namespace_versions(RENAMED_TASK) == [],
                   "rename preview leaves every source version and destination untouched")
        renamed = self.wm(task, "task", "rename", "renamed-history")
        task = task.parent / RENAMED_TASK
        self.check(renamed["task_id"] == RENAME_TASK and renamed["branch"] == RENAME_BRANCH
                   and renamed["storage_migration"]["preserved_versions"] == len(original)
                   and renamed["storage_migration"]["delete_markers"]
                       == sum(row["delete_marker"] for row in original)
                   and self.namespace_versions(RENAME_TASK) == original
                   and self.namespace_versions(RENAMED_TASK) == [],
                   "local rename freezes full history, preserves identity and performs no S3 writes")
        published = self.wm(task, "publish", "-m", "Rename through exact server-side history copies")
        receipt = json.loads((task / RECEIPT).read_text())
        self.check(receipt["migration_kind"] == "task-rename" and receipt["status"] == "copied"
                   and receipt["source"] == RENAME_TASK and receipt["destination"] == RENAMED_TASK,
                   "rename publication produces a formally marked exact-version receipt")
        copied = self.check_copied_history(receipt, original)
        self.check(len(copied) == len(original)
                   and self.namespace_versions(RENAME_TASK) == original
                   and published["storage"]["purge"]["status"] == "cleanup_pending",
                   "unchanged rename creates only copied generations and keeps source history pending before merge")
        single = json.loads((task / "single.txt.wm-storage.json").read_text())
        bundle = json.loads((task / "bundle.wm-storage.json").read_text())
        copied_bindings = {(row["destination_object"], row["destination_version_id"])
                           for row in receipt["versions"] if not row["delete_marker"]}
        self.check((f"{RENAMED_TASK}/single.txt", single["version"]["id"]) in copied_bindings
                   and all((f"{RENAMED_TASK}/bundle/{item['path']}", item["version"]["id"])
                           in copied_bindings for item in bundle["entries"]),
                   "unchanged native outputs bind copied versions instead of additional uploaded generations")

        self.section("later active rename edits preserve unique copied history before shared merge")
        (task / "bundle" / "alpha.txt").write_bytes(b"rename active alpha version three\n")
        (task / "bundle" / "beta.bin").unlink()
        with (task / "record.md").open("a", encoding="utf-8") as record:
            record.write("\nEdit alpha and remove current beta while retaining immutable copied history.\n")
        changed = self.wm(task, "publish", "-m", "Edit active renamed payloads before merge")
        self.check(changed["status"] == "pushed"
                   and json.loads((task / RECEIPT).read_text()) == receipt
                   and self.namespace_versions(RENAME_TASK) == original,
                   "subsequent publication keeps the rename receipt and original source obligation")
        bundle = json.loads((task / "bundle.wm-storage.json").read_text())
        self.check([item["path"] for item in bundle["entries"]] == ["alpha.txt"]
                   and (f"{RENAMED_TASK}/bundle/alpha.txt", bundle["entries"][0]["version"]["id"])
                       not in copied_bindings,
                   "changed alpha gets a new exact binding and removed beta leaves the current manifest")
        self.check_retained_copies(receipt, bodies)
        self.check((task / "bundle" / "alpha.txt").read_bytes() == b"rename active alpha version three\n",
                   "normal upload retains the new active opaque bytes")
        self.section("rename merge retires original namespace and historical hydration follows exact copies")
        merged = self.merge_branch_to_main(RENAME_BRANCH)
        self.fake_gh(merged, changed["remote_oid"], RENAME_BRANCH, 8)
        cleaner = self.root / "rename-retirement-clone"
        self.run(["git", "clone", self.remote_url, cleaner])
        self.configure_git(cleaner)
        refused = self.wm(cleaner, "archive", RENAMED_TASK, "--dry-run", expected=2)
        self.check("finish prior source retirement" in refused["stderr"],
                   "rearchive refuses while a prior rename source still retains versions or markers")
        completed = self.wm(cleaner, "refresh")
        self.check(completed["storage"]["purge"]["status"] == "complete"
                   and self.namespace_versions(RENAME_TASK) == [],
                   "merged rename retires every original payload version and delete marker")
        self.check_retained_copies(receipt, bodies)
        # This clone already has the current Git tree. Refresh retires source
        # history but does not hydrate unchanged pointer outputs implicitly.
        current_history = self.namespace_versions(RENAMED_TASK)
        hydrated = self.wm(cleaner / RENAMED_TASK, "storage", "hydrate")
        self.check(hydrated["status"] == "hydrated"
                   and self.namespace_versions(RENAME_TASK) == []
                   and self.namespace_versions(RENAMED_TASK) == current_history,
                   "explicit current hydration reads exact versions without creating any remote history")
        self.check((cleaner / RENAMED_TASK / "single.txt").read_bytes() == b"rename standalone version two\n"
                   and (cleaner / RENAMED_TASK / "bundle" / "alpha.txt").read_bytes()
                       == b"rename active alpha version three\n"
                   and not (cleaner / RENAMED_TASK / "bundle" / "beta.bin").exists()
                   and not (cleaner / RENAMED_TASK / "gone.bin").exists(),
                   "fresh main hydration restores current edits without recreating removed or marker-only historical files")
        history = self.root / "rename-historical-consumer"
        self.run(["git", "clone", self.remote_url, history])
        self.configure_git(history)
        self.git(history, "checkout", "--detach", first_oid)
        self.wm(history / RENAME_TASK, "storage", "hydrate")
        self.check((history / RENAME_TASK / "single.txt").read_bytes() == b"rename standalone version one\n"
                   and (history / RENAME_TASK / "bundle" / "alpha.txt").read_bytes() == b"rename alpha version one\n"
                   and (history / RENAME_TASK / "bundle" / "beta.bin").read_bytes() == b"rename historical beta\n",
                   "historical Git pointers hydrate original exact bytes after source retirement and later removal")

        self.section("rearchive after source retirement moves protected intermediate copies onward")
        preview = self.wm(cleaner, "archive", RENAMED_TASK, "--dry-run")
        self.check(preview["tasks"][0]["destination"] == RENAMED_ARCHIVE,
                   "rearchive becomes eligible only after the earlier source namespace is empty")
        created = self.wm(
            cleaner, "task", "create", "archive-renamed-history", "--kind", "infrastructure",
            "--title", "Archive renamed task", "--purpose", "Preserve a chained exact-version relocation.",
            "--scope", RENAMED_TASK, "--scope", RENAMED_ARCHIVE,
            "--scope-note", "Fixture-owned source retirement completed before this archive.",
        )
        manifest = created["manifest"]
        before_archive = self.namespace_versions(RENAMED_TASK)
        archived = self.wm(cleaner, "archive", RENAMED_TASK, "--manifest", manifest)
        self.check(archived["status"] == "archived", "rearchive applies after earlier source retirement")
        self.wm(cleaner, "publish", "--manifest", manifest, "-m", "Archive retained renamed history")
        next_receipt = json.loads((cleaner / RENAMED_ARCHIVE / RECEIPT).read_text())
        copied_again = self.check_copied_history(next_receipt, before_archive)
        self.check(next_receipt["previous_receipt"] == receipt,
                   "later archive retains the complete prior rename control record")
        self.merge_branch_to_main(created["branch"])
        completed = self.wm(cleaner, "refresh")
        self.check(completed["storage"]["purge"]["status"] == "complete"
                   and self.namespace_versions(RENAME_TASK) == []
                   and self.namespace_versions(RENAMED_TASK) == []
                   and self.namespace_versions(RENAMED_ARCHIVE) == copied_again,
                   "canonical archive source retirement moves copied intermediate history onward without leaking it")
        # Rehydrate with an empty exact-version cache so the A->B->C chain must
        # resolve both canonical receipts rather than reuse the previous read.
        chained_history = self.root / "rename-chained-historical-consumer"
        self.run(["git", "clone", self.remote_url, chained_history])
        self.configure_git(chained_history)
        self.git(chained_history, "checkout", "--detach", first_oid)
        self.wm(chained_history / RENAME_TASK, "storage", "hydrate")
        self.check((chained_history / RENAME_TASK / "single.txt").read_bytes() == b"rename standalone version one\n"
                   and (chained_history / RENAME_TASK / "bundle" / "alpha.txt").read_bytes() == b"rename alpha version one\n"
                   and (chained_history / RENAME_TASK / "bundle" / "beta.bin").read_bytes() == b"rename historical beta\n"
                   and self.namespace_versions(RENAME_TASK) == []
                   and self.namespace_versions(RENAMED_TASK) == [],
                   "historical hydration traverses rename and archive mappings without recreating retired namespaces")

    def execute(self):
        self.initialize()
        first_oid, original = self.publish_original()
        organizer, worktree, branch, manifest = self.organize_without_materialization(original)
        self.cancel_failed_publication(worktree, branch, original, manifest)
        copied = self.publish_and_retry(worktree, branch, original, manifest)
        self.merge_and_read_history(organizer, branch, first_oid, copied)
        self.rename_and_retire_history()
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
