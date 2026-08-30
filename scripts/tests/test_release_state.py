from __future__ import annotations

import hashlib
import io
import importlib.util
import json
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).parents[1] / "release_state.py"
SPEC = importlib.util.spec_from_file_location("release_state", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
release_state = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = release_state
SPEC.loader.exec_module(release_state)


SHA_A = "a" * 40
SHA_B = "b" * 40


class ReleaseStateTests(unittest.TestCase):
    def setUp(self) -> None:
        self.crate = release_state.Package("workspace-mgr", "1.2.3-alpha.1", True)
        self.assets = release_state.expected_assets(self.crate)

    def test_unpublished_version_uses_current_revision(self) -> None:
        state = release_state.derive_release_state(
            self.crate,
            release_state.RegistryState(False, False, None),
            release_state.GitHubState(None, False, frozenset()),
            SHA_A,
        )
        self.assertEqual(state.source_sha, SHA_A)
        self.assertTrue(state.release_needed)
        self.assertFalse(state.tag_exists)

    def test_complete_release_is_a_noop(self) -> None:
        state = release_state.derive_release_state(
            self.crate,
            release_state.RegistryState(True, True, SHA_A),
            release_state.GitHubState(SHA_A, True, self.assets),
            SHA_B,
        )
        self.assertEqual(state.source_sha, SHA_A)
        self.assertTrue(state.release_complete)
        self.assertFalse(state.release_needed)

    def test_published_version_without_tag_is_recoverable(self) -> None:
        state = release_state.derive_release_state(
            self.crate,
            release_state.RegistryState(True, True, SHA_A),
            release_state.GitHubState(None, False, frozenset()),
            SHA_B,
        )
        self.assertEqual(state.source_sha, SHA_A)
        self.assertTrue(state.release_needed)

    def test_incomplete_assets_are_recoverable(self) -> None:
        state = release_state.derive_release_state(
            self.crate,
            release_state.RegistryState(True, True, SHA_A),
            release_state.GitHubState(SHA_A, True, frozenset()),
            SHA_B,
        )
        self.assertTrue(state.release_needed)
        self.assertFalse(state.release_complete)

    def test_tag_before_registry_version_is_rejected(self) -> None:
        with self.assertRaisesRegex(
            release_state.ReleaseStateError, "exists on GitHub before"
        ):
            release_state.derive_release_state(
                self.crate,
                release_state.RegistryState(True, False, None),
                release_state.GitHubState(SHA_A, False, frozenset()),
                SHA_A,
            )

    def test_mismatched_published_source_and_tag_are_rejected(self) -> None:
        with self.assertRaisesRegex(
            release_state.ReleaseStateError, "but crates.io records"
        ):
            release_state.derive_release_state(
                self.crate,
                release_state.RegistryState(True, True, SHA_A),
                release_state.GitHubState(SHA_B, True, self.assets),
                SHA_B,
            )

    def test_published_archive_records_exact_source_revision(self) -> None:
        archive_buffer = io.BytesIO()
        vcs_info = json.dumps({"git": {"sha1": SHA_A}}).encode()
        with tarfile.open(fileobj=archive_buffer, mode="w:gz") as archive:
            member = tarfile.TarInfo(
                "workspace-mgr-1.2.3-alpha.1/.cargo_vcs_info.json"
            )
            member.size = len(vcs_info)
            archive.addfile(member, io.BytesIO(vcs_info))
        package = archive_buffer.getvalue()
        version_data = {
            "version": {
                "checksum": hashlib.sha256(package).hexdigest(),
                "dl_path": "/download",
            }
        }
        with mock.patch.object(release_state, "request_bytes", return_value=package):
            self.assertEqual(
                release_state.package_source_sha(self.crate, version_data), SHA_A
            )

    def test_published_archive_checksum_mismatch_is_rejected(self) -> None:
        version_data = {
            "version": {"checksum": "0" * 64, "dl_path": "/download"}
        }
        with mock.patch.object(release_state, "request_bytes", return_value=b"bad"):
            with self.assertRaisesRegex(
                release_state.ReleaseStateError, "checksum mismatch"
            ):
                release_state.package_source_sha(self.crate, version_data)

    def test_changelog_and_notes_use_the_declared_version(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "Cargo.toml").write_text(
                '[package]\nname = "workspace-mgr"\nversion = "1.2.3-alpha.1"\n',
                encoding="utf-8",
            )
            (root / "CHANGELOG.md").write_text(
                "# Changelog\n\n## [Unreleased]\n\n"
                "## [1.2.3-alpha.1] - 2026-08-30\n\n"
                "### Added\n\n- Release automation.\n\n"
                "## [1.2.2] - 2026-08-01\n\n- Earlier.\n",
                encoding="utf-8",
            )
            package = release_state.load_package(root)
            release_state.validate_changelog(root, package.version)
            self.assertTrue(package.prerelease)
            self.assertEqual(
                release_state.release_notes(root, package.version),
                "### Added\n\n- Release automation.\n",
            )


if __name__ == "__main__":
    unittest.main()
