"""Offline regression tests for release ordering and changelog updates."""

import argparse
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

PACKAGING = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("version_order", PACKAGING / "version-order.py")
version_order = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(version_order)


class VersionOrderTests(unittest.TestCase):
    def test_semver_precedence_including_numeric_prereleases(self):
        versions = [
            "1.0.0-alpha", "1.0.0-alpha.1", "1.0.0-alpha.beta",
            "1.0.0-beta", "1.0.0-beta.2", "1.0.0-beta.11",
            "1.0.0-rc.1", "1.0.0", "1.0.1", "1.1.0", "1.10.0", "2.0.0",
        ]
        for older, newer in zip(versions, versions[1:]):
            with self.subTest(older=older, newer=newer):
                self.assertLess(version_order.version_key(older), version_order.version_key(newer))

    def test_tag_prefix_does_not_change_precedence(self):
        self.assertEqual(version_order.version_key("v0.33.6"), version_order.version_key("0.33.6"))

    def test_invalid_versions_fail_closed(self):
        for value in ("", "v1.2", "v01.2.3", "1.2.3-", "1.2.3-beta..1", "1.2.3-beta.01"):
            with self.subTest(value=value), self.assertRaises(argparse.ArgumentTypeError):
                version_order.version_key(value)

    def test_cli_allows_equal_or_newer_versions_only(self):
        for candidate, current, expected in (
            ("v0.33.5", "0.33.6", "false"),
            ("v0.33.6", "0.33.6", "true"),
            ("v0.34.0", "0.33.6", "true"),
            ("v1.0.0-beta.2", "1.0.0-beta.11", "false"),
            ("v1.0.0-beta.11", "1.0.0", "false"),
        ):
            with self.subTest(candidate=candidate, current=current):
                result = subprocess.run(
                    [sys.executable, str(PACKAGING / "version-order.py"), candidate, current],
                    capture_output=True, text=True, check=True, timeout=10,
                )
                self.assertEqual(result.stdout.strip(), expected)


class ReleaseUpdateTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cupel-release-update-test-")
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        self.work = root / "checkout"
        self.work.mkdir()
        self.remote = root / "remote.git"
        tools = root / "tools"
        tools.mkdir()
        cargo = tools / "cargo"
        cargo.write_text("""#!/bin/sh
set -eu
[ "$*" = 'update --workspace' ]
awk -F '"' '/^version = / { print $2; exit }' Cargo.toml > Cargo.lock
""")
        cargo.chmod(0o755)
        self.env = {
            **os.environ,
            "PATH": f"{tools}{os.pathsep}{os.defpath}",
            "PYTHONDONTWRITEBYTECODE": "1",
        }
        self.git("init", "--bare", "--initial-branch=main", str(self.remote))
        self.git("init", "--initial-branch=main")
        self.git("config", "user.name", "Test")
        self.git("config", "user.email", "test@example.com")
        self.git("config", "commit.gpgsign", "false")
        (self.work / "Cargo.toml").write_text('[workspace.package]\nversion = "0.33.6"\n')
        (self.work / "Cargo.lock").write_text("0.33.6\n")
        (self.work / "CHANGELOG.md").write_text("# Changelog\n\n## [v0.33.6] - 2026-10-24\n\n- Previous release\n")
        self.git("add", ".")
        self.git("commit", "-m", "previous release")
        self.git("tag", "v0.33.6")
        (self.work / "README.md").write_text("Example change\n")
        self.git("add", ".")
        self.git("commit", "-m", "example change")
        self.git("tag", "v0.33.8")
        self.git("remote", "add", "origin", str(self.remote))
        self.git("push", "origin", "main", "--tags")
        self.main = self.git("rev-parse", "main")

    def git(self, *args):
        return subprocess.run(
            ["git", *args], cwd=self.work, env=self.env,
            capture_output=True, text=True, check=True, timeout=10,
        ).stdout.strip()

    def update(self, tag="v0.33.8"):
        return subprocess.run(
            ["sh", str(PACKAGING / "update-release-metadata.sh"), tag],
            cwd=self.work, env=self.env, capture_output=True, text=True, timeout=30,
        )

    def test_records_release_and_pushes_main(self):
        result = self.update()
        self.assertEqual(result.returncode, 0, result.stderr)
        head = self.git("rev-parse", "HEAD")
        self.assertNotEqual(head, self.main)
        self.assertEqual(self.git("ls-remote", "origin", "refs/heads/main").split()[0], head)
        self.assertEqual(self.git("log", "-1", "--format=%s"), "changelog: v0.33.8")
        self.assertEqual(
            self.git("diff", "--name-only", f"{self.main}..HEAD").splitlines(),
            ["CHANGELOG.md", "Cargo.lock", "Cargo.toml"],
        )
        self.assertIn("## [v0.33.8]", self.git("show", "HEAD:CHANGELOG.md"))
        self.assertIn("- example change", self.git("show", "HEAD:CHANGELOG.md"))
        self.assertIn('version = "0.33.8"', self.git("show", "HEAD:Cargo.toml"))
        self.assertEqual(self.git("show", "HEAD:Cargo.lock"), "0.33.8")

    def test_already_recorded_release_is_a_noop(self):
        self.assertEqual(self.update().returncode, 0)
        recorded = self.git("rev-parse", "HEAD")
        result = self.update()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.git("rev-parse", "HEAD"), recorded)
        self.assertEqual(self.git("ls-remote", "origin", "refs/heads/main").split()[0], recorded)

    def test_older_release_is_skipped(self):
        result = self.update("v0.33.5")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.git("rev-parse", "HEAD"), self.main)
        self.assertEqual(self.git("ls-remote", "origin", "refs/heads/main").split()[0], self.main)

    def test_concurrent_main_advance_is_not_clobbered(self):
        # Someone else pushes to main after the job's checkout: the plain push
        # is rejected instead of overwriting their commit.
        (self.work / "OTHER.md").write_text("concurrent\n")
        self.git("add", "OTHER.md")
        self.git("commit", "-m", "concurrent change")
        self.git("push", "origin", "HEAD:main")
        concurrent = self.git("rev-parse", "HEAD")
        self.git("reset", "--hard", self.main)
        self.assertNotEqual(self.update().returncode, 0)
        self.assertEqual(self.git("ls-remote", "origin", "refs/heads/main").split()[0], concurrent)


if __name__ == "__main__":
    unittest.main()
