"""Offline tests for preparing release metadata without pushing or tagging."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest

PACKAGING = Path(__file__).resolve().parent


class PrepareReleaseTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cupel-prepare-release-test-")
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        self.work = root / "checkout"
        self.work.mkdir()
        tools = root / "tools"
        tools.mkdir()
        cargo = tools / "cargo"
        cargo.write_text("""#!/bin/sh
set -eu
[ "$*" = 'update --workspace' ]
[ "${FAIL_CARGO:-0}" = 0 ]
awk -F '"' '/^version = / { print $2; exit }' Cargo.toml > Cargo.lock
""")
        cargo.chmod(0o755)
        self.env = {**os.environ, "PATH": f"{tools}{os.pathsep}{os.defpath}"}
        self.git("init", "--initial-branch=main")
        self.git("config", "user.name", "Test")
        self.git("config", "user.email", "test@example.com")
        self.git("config", "commit.gpgsign", "false")
        (self.work / "Cargo.toml").write_text('[workspace.package]\nversion = "0.33.10"\n')
        (self.work / "Cargo.lock").write_text("0.33.10\n")
        (self.work / "CHANGELOG.md").write_text("# Changelog\n\n## [v0.33.10] - 2026-10-27\n\n- Previous release\n")
        self.git("add", ".")
        self.git("commit", "-m", "previous release")
        self.git("tag", "v0.33.10")
        (self.work / "README.md").write_text("Example change\n")
        self.git("add", ".")
        self.git("commit", "-m", "example change")
        self.head = self.git("rev-parse", "HEAD")

    def git(self, *args):
        return subprocess.run(
            ["git", *args], cwd=self.work, env=self.env,
            capture_output=True, text=True, check=True, timeout=10,
        ).stdout.strip()

    def prepare(self, tag="v0.33.11"):
        return subprocess.run(
            ["sh", str(PACKAGING / "prepare-release.sh"), tag],
            cwd=self.work, env=self.env, capture_output=True, text=True, timeout=30,
        )

    def test_prepares_only_metadata_without_committing_or_tagging(self):
        result = self.prepare()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.git("rev-parse", "HEAD"), self.head)
        self.assertEqual(self.git("tag", "--list"), "v0.33.10")
        self.assertEqual(self.git("diff", "--cached", "--name-only"), "")
        self.assertEqual(
            self.git("diff", "--name-only").splitlines(),
            ["CHANGELOG.md", "Cargo.lock", "Cargo.toml"],
        )
        changelog = (self.work / "CHANGELOG.md").read_text()
        self.assertIn("## [v0.33.11]", changelog)
        self.assertIn("- example change", changelog)
        self.assertNotIn("- previous release", changelog)
        self.assertIn('version = "0.33.11"', (self.work / "Cargo.toml").read_text())
        self.assertEqual((self.work / "Cargo.lock").read_text(), "0.33.11\n")

    def test_preparing_twice_is_idempotent(self):
        self.assertEqual(self.prepare().returncode, 0)
        prepared = self.git("diff")
        result = self.prepare()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.git("diff"), prepared)

    def test_existing_tag_backfill_excludes_later_commits(self):
        self.git("tag", "v0.33.11")
        (self.work / "LATER.md").write_text("later\n")
        self.git("add", ".")
        self.git("commit", "-m", "later change")
        result = self.prepare()
        self.assertEqual(result.returncode, 0, result.stderr)
        changelog = (self.work / "CHANGELOG.md").read_text()
        self.assertIn("- example change", changelog)
        self.assertNotIn("- later change", changelog)

    def test_invalid_tag_does_not_modify_files(self):
        for tag in ("0.33.11", "v0.33", "vnot-a-version"):
            with self.subTest(tag=tag):
                self.assertNotEqual(self.prepare(tag).returncode, 0)
                self.assertEqual(self.git("status", "--porcelain"), "")

    def test_prerelease_tag(self):
        result = self.prepare("v0.33.11-beta.1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('version = "0.33.11-beta.1"', (self.work / "Cargo.toml").read_text())

    def test_first_release_uses_all_history(self):
        self.git("tag", "-d", "v0.33.10")
        result = self.prepare()
        self.assertEqual(result.returncode, 0, result.stderr)
        changelog = (self.work / "CHANGELOG.md").read_text()
        self.assertIn("- previous release", changelog)
        self.assertIn("- example change", changelog)

    def test_cargo_failure_does_not_write_changelog(self):
        original = (self.work / "CHANGELOG.md").read_text()
        self.env["FAIL_CARGO"] = "1"
        self.assertNotEqual(self.prepare().returncode, 0)
        self.assertEqual((self.work / "CHANGELOG.md").read_text(), original)
        self.assertEqual(self.git("rev-parse", "HEAD"), self.head)


if __name__ == "__main__":
    unittest.main()
