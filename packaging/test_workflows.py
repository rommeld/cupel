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
