"""Offline regression tests for release builds, ordering, and retry behavior."""

import argparse
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest

PACKAGING = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("version_order", PACKAGING / "version-order.py")
version_order = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(version_order)


@unittest.skipUnless(sys.platform == "darwin", "requires Apple's clang and lipo")
class MacOSUniversalBuildTests(unittest.TestCase):
    def test_workflow_creates_and_verifies_both_universal_binaries(self):
        workflow = (PACKAGING.parent / ".github/workflows/release.yml").read_text()
        start = workflow.index("          for binary in cupel cupel-acp; do\n")
        end = workflow.index("          done\n", start) + len("          done\n")
        loop = textwrap.dedent(workflow[start:end])

        with tempfile.TemporaryDirectory(prefix="cupel-lipo-test-") as directory:
            root = Path(directory)
            for target, arch in (
                ("aarch64-apple-darwin", "arm64"), ("x86_64-apple-darwin", "x86_64"),
            ):
                output = root / "target" / target / "release"
                output.mkdir(parents=True)
                for binary in ("cupel", "cupel-acp"):
                    subprocess.run(
                        ["xcrun", "clang", "-arch", arch, "-x", "c", "-c", "-o", str(output / binary), "-"],
                        input="int main(void) { return 0; }\n", text=True,
                        capture_output=True, check=True, timeout=30,
                    )

            result = subprocess.run(
                ["bash", "-euo", "pipefail", "-c", loop], cwd=root,
                capture_output=True, text=True, timeout=30,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            for binary in ("cupel", "cupel-acp"):
                archs = subprocess.run(
                    ["lipo", binary, "-archs"], cwd=root,
                    capture_output=True, text=True, check=True, timeout=10,
                )
                self.assertEqual(set(archs.stdout.split()), {"arm64", "x86_64"})


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
        self.calls = root / "calls"
        self.pr = root / "pr"
        self.fail_dispatch = root / "fail-dispatch"
        self.fail_auto_merge = root / "fail-auto-merge"
        tools = root / "tools"
        tools.mkdir()
        gh = tools / "gh"
        gh.write_text("""#!/bin/sh
set -eu
printf '%s\\n' "$*" >> "$CALLS"
case "$1 $2" in
    'pr list') if [ -f "$PR" ]; then echo 42; fi ;;
    'pr create')
        [ ! -f "$PR" ]
        echo 42 > "$PR"
        echo https://github.com/example/cupel/pull/42 ;;
    'pr merge') [ -f "$PR" ] && [ ! -f "$FAIL_AUTO_MERGE" ] ;;
    'workflow run') [ ! -f "$FAIL_DISPATCH" ] ;;
    *) exit 1 ;;
esac
""")
        gh.chmod(0o755)
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
            "CALLS": str(self.calls), "PR": str(self.pr),
            "FAIL_DISPATCH": str(self.fail_dispatch),
            "FAIL_AUTO_MERGE": str(self.fail_auto_merge),
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
        hook = self.remote / "hooks/update"
        hook.write_text("""#!/bin/sh
if [ "$1" = refs/heads/main ]; then
    echo 'GH013: Changes must be made through a pull request' >&2
    exit 1
fi
""")
        hook.chmod(0o755)

    def git(self, *args):
        return subprocess.run(
            ["git", *args], cwd=self.work, env=self.env,
            capture_output=True, text=True, check=True, timeout=10,
        ).stdout.strip()

    def update(self, tag="v0.33.8"):
        return subprocess.run(
            ["sh", str(PACKAGING / "open-release-update.sh"), tag],
            cwd=self.work, env=self.env, capture_output=True, text=True, timeout=30,
        )

    def test_opens_pr_dispatches_ci_and_requests_auto_merge_without_pushing_main(self):
        result = self.update()
        self.assertEqual(result.returncode, 0, result.stderr)
        branch = "release/changelog-v0.33.8"
        self.assertEqual(self.git("ls-remote", "origin", "refs/heads/main").split()[0], self.main)
        self.assertEqual(
            self.git("ls-remote", "origin", f"refs/heads/{branch}").split()[0],
            self.git("rev-parse", branch),
        )
        self.assertEqual(
            self.git("diff", "--name-only", f"main...{branch}").splitlines(),
            ["CHANGELOG.md", "Cargo.lock", "Cargo.toml"],
        )
        self.assertIn("## [v0.33.8]", self.git("show", f"{branch}:CHANGELOG.md"))
        self.assertIn("- example change", self.git("show", f"{branch}:CHANGELOG.md"))
        self.assertIn('version = "0.33.8"', self.git("show", f"{branch}:Cargo.toml"))
        self.assertEqual(self.git("show", f"{branch}:Cargo.lock"), "0.33.8")
        calls = self.calls.read_text().splitlines()
        self.assertEqual(len(calls), 4)
        self.assertTrue(calls[0].startswith("pr list --state open --base main --head " + branch))
        self.assertTrue(calls[1].startswith("pr create --base main --head " + branch))
        self.assertEqual(calls[2], "workflow run ci.yml --ref " + branch)
        self.assertEqual(calls[3], (
            f"pr merge {branch} --auto --squash --match-head-commit {self.git('rev-parse', branch)}"
        ))

    def test_retry_reuses_pr_after_failed_ci_dispatch(self):
        self.fail_dispatch.touch()
        self.assertNotEqual(self.update().returncode, 0)
        self.assertNotIn("pr merge ", self.calls.read_text())
        self.git("switch", "main")
        self.fail_dispatch.unlink()
        result = self.update()
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls.read_text().splitlines()
        self.assertEqual(sum(call.startswith("pr create ") for call in calls), 1)
        self.assertEqual(sum(call.startswith("workflow run ") for call in calls), 2)
        self.assertEqual(sum(call.startswith("pr merge ") for call in calls), 1)
        self.assertEqual(self.git("ls-remote", "origin", "refs/heads/main").split()[0], self.main)

    def test_retry_reenables_auto_merge_on_existing_pr(self):
        result = self.update()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.git("switch", "main")
        result = self.update()
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls.read_text().splitlines()
        self.assertEqual(sum(call.startswith("pr create ") for call in calls), 1)
        self.assertEqual(sum(call.startswith("workflow run ") for call in calls), 2)
        self.assertEqual(sum(call.startswith("pr merge ") for call in calls), 2)
        self.assertEqual(calls[-1], (
            "pr merge release/changelog-v0.33.8 --auto --squash --match-head-commit "
            + self.git("rev-parse", "HEAD")
        ))
        self.assertEqual(self.git("ls-remote", "origin", "refs/heads/main").split()[0], self.main)

    def test_failed_auto_merge_is_reported_and_retry_reuses_pr(self):
        self.fail_auto_merge.touch()
        self.assertNotEqual(self.update().returncode, 0)
        self.assertTrue(self.calls.read_text().splitlines()[-1].startswith("pr merge "))
        self.git("switch", "main")
        self.fail_auto_merge.unlink()
        result = self.update()
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls.read_text().splitlines()
        self.assertEqual(sum(call.startswith("pr create ") for call in calls), 1)
        self.assertEqual(sum(call.startswith("workflow run ") for call in calls), 2)
        self.assertEqual(sum(call.startswith("pr merge ") for call in calls), 2)
        self.assertEqual(self.git("ls-remote", "origin", "refs/heads/main").split()[0], self.main)

    def test_retry_refuses_to_overwrite_a_concurrent_branch_change(self):
        result = self.update()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.git("switch", "main")
        branch = "release/changelog-v0.33.8"
        self.git("--git-dir", str(self.remote), "update-ref", f"refs/heads/{branch}", self.main)
        calls = self.calls.read_text()
        self.assertNotEqual(self.update().returncode, 0)
        self.assertEqual(self.git("ls-remote", "origin", f"refs/heads/{branch}").split()[0], self.main)
        self.assertEqual(self.calls.read_text(), calls)

    def test_already_recorded_release_does_not_open_pr_or_dispatch_ci(self):
        result = self.update()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.git("switch", "main")
        self.git("merge", "--ff-only", "release/changelog-v0.33.8")
        self.calls.unlink()
        result = self.update()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.calls.exists())
        self.assertEqual(self.git("branch", "--show-current"), "main")

    def test_older_release_is_skipped(self):
        result = self.update("v0.33.5")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.calls.exists())
        self.assertEqual(self.git("rev-parse", "HEAD"), self.main)


class PublishReleaseTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="cupel-publish-test-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.state = self.root / "state"
        self.state.write_text("missing")
        self.calls = self.root / "calls"
        self.fail_upload = self.root / "fail-upload"
        tools = self.root / "tools"
        tools.mkdir()
        gh = tools / "gh"
        gh.write_text("""#!/bin/sh
set -eu
printf '%s\\n' "$2" >> "$CALLS"
state=$(cat "$STATE")
case "$2" in
    view)
        case "$state" in
            missing) exit 1 ;;
            draft) echo true ;;
            published) echo false ;;
        esac ;;
    create)
        [ "$state" = missing ]
        echo draft > "$STATE" ;;
    upload)
        [ "$state" = draft ]
        if [ -f "$FAIL_UPLOAD" ]; then
            rm "$FAIL_UPLOAD"
            exit 1
        fi ;;
    edit)
        [ "$state" = draft ]
        echo published > "$STATE" ;;
    *) exit 1 ;;
esac
""")
        gh.chmod(0o755)
        self.env = {
            "PATH": f"{tools}{os.pathsep}{os.defpath}",
            "STATE": str(self.state), "CALLS": str(self.calls),
            "FAIL_UPLOAD": str(self.fail_upload),
        }
        for name in (
            "cupel-macos-universal.tar.gz", "cupel-linux-x86_64.tar.gz",
            "cupel-linux-aarch64.tar.gz", "sha256sums.txt",
        ):
            (self.root / name).write_text("fixture")

    def publish(self):
        return subprocess.run(
            ["sh", str(PACKAGING / "publish-release.sh"), "v0.34.0"],
            cwd=self.root, env=self.env, capture_output=True, text=True, timeout=10,
        )

    def test_new_release_publishes_after_upload(self):
        result = self.publish()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls.read_text().splitlines(), ["view", "create", "upload", "edit"])
        self.assertEqual(self.state.read_text().strip(), "published")

    def test_failed_upload_stays_draft_and_retry_resumes_it(self):
        self.fail_upload.touch()
        self.assertNotEqual(self.publish().returncode, 0)
        self.assertEqual(self.state.read_text().strip(), "draft")
        result = self.publish()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls.read_text().splitlines(), [
            "view", "create", "upload", "view", "upload", "edit",
        ])
        self.assertEqual(self.state.read_text().strip(), "published")

    def test_published_release_is_untouched_on_retry(self):
        self.state.write_text("published")
        result = self.publish()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls.read_text().splitlines(), ["view"])

    def test_missing_platform_asset_prevents_publication(self):
        (self.root / "cupel-linux-aarch64.tar.gz").unlink()
        self.assertNotEqual(self.publish().returncode, 0)
        self.assertFalse(self.calls.exists())


if __name__ == "__main__":
    unittest.main()
