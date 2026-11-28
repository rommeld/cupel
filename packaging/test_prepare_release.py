"""Offline tests for preparing manual and automatic releases without pushing."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

PACKAGING = Path(__file__).resolve().parent
MERGED_AT = "2026-11-27T12:00:00Z"


def comment(body, author="OWNER", updated_at=MERGED_AT):
    return {"body": body, "author_association": author, "updated_at": updated_at}


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
        gh = tools / "gh"
        gh.write_text("""#!/bin/sh
set -eu
[ "$*" = "api --paginate --slurp repos/cupel/test/issues/$PR_NUMBER/comments?per_page=100" ]
cat "$PR_COMMENTS"
""")
        gh.chmod(0o755)
        self.event = root / "event.json"
        self.comments = root / "comments.json"
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

    def prepare_merge(self, number, pages):
        self.event.write_text(json.dumps({"pull_request": {"number": number, "merged_at": MERGED_AT}}))
        self.comments.write_text(json.dumps(pages))
        result = subprocess.run(
            [sys.executable, str(PACKAGING / "prepare-merge-release.py")],
            cwd=self.work, env={
                **self.env, "GITHUB_EVENT_PATH": str(self.event), "GITHUB_REPOSITORY": "cupel/test",
                "PR_NUMBER": str(number), "PR_COMMENTS": str(self.comments),
            }, capture_output=True, text=True, timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout.strip()

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

    def test_merge_without_an_eligible_comment_does_not_tag(self):
        pages = [[
            comment("Changelog: added", author="NONE"),
            comment("Changelog: refact", updated_at="2026-11-27T13:00:00Z"),
            comment("Use `Changelog: fixed` to release"),
        ]]
        self.assertEqual(self.prepare_merge(1, pages), "")
        self.assertEqual(self.git("tag", "--list"), "v0.33.10")
        self.assertEqual(self.git("status", "--porcelain"), "")

    def test_merge_rerun_preserves_original_tag(self):
        self.assertEqual(self.prepare_merge(1, [[comment("Changelog: fixed")]]), "v0.33.11")
        tagged = self.git("rev-parse", "v0.33.11")
        self.assertEqual(self.git("rev-parse", "v0.33.11^"), self.head)
        self.assertEqual(self.git("rev-parse", "main"), self.head)
        self.git("switch", "main")
        self.assertEqual(self.prepare_merge(1, [[comment("Changelog: added")]]), "v0.33.11")
        self.assertEqual(self.git("rev-parse", "v0.33.11"), tagged)
        self.assertEqual(self.git("tag", "--list"), "v0.33.10\nv0.33.11")

    def test_successive_merges_use_previous_tag_only_release(self):
        pages = [[comment("Changelog: fixed")], [comment("Changelog: added", author="MEMBER")]]
        self.git("tag", "v9.0.0-rc.1")
        self.assertEqual(self.prepare_merge(1, pages), "v0.34.0")
        self.git("switch", "main")
        (self.work / "README.md").write_text("Another change\n")
        self.git("commit", "-am", "second change")
        self.assertEqual(self.prepare_merge(2, [[comment("Changelog: refact")]]), "v0.34.1")

        changelog = self.git("show", "v0.34.1:CHANGELOG.md")
        new_section, prior_section = changelog.split("## [v0.34.0]", 1)
        self.assertIn("## [v0.34.1]", new_section)
        self.assertIn("- second change", new_section)
        self.assertNotIn("- example change", new_section)
        self.assertIn("- example change", prior_section)
        self.assertEqual(self.git("show", "v0.34.1:Cargo.lock"), "0.34.1")

        self.git("switch", "--detach", self.head)
        self.assertEqual(self.prepare_merge(3, pages), "")


if __name__ == "__main__":
    unittest.main()
