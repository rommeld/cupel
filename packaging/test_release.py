"""Offline regression tests for release validation and installer compatibility."""

import hashlib
import importlib.util
import io
import os
from pathlib import Path
import platform
import subprocess
import tarfile
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("check_release", Path(__file__).with_name("check-release.py"))
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="cupel-packaging-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        if platform.system() == "Darwin":
            asset = "cupel-macos-universal"
        else:
            arch = "aarch64" if platform.machine() in ("arm64", "aarch64") else "x86_64"
            asset = f"cupel-linux-{arch}"
        self.archive = self.root / f"{asset}.tar.gz"
        self.sums = self.root / "sha256sums.txt"

    def package(self, *, acp=True, executable=True):
        with tarfile.open(self.archive, "w:gz") as archive:
            for name in ("cupel", "cupel-acp") if acp else ("cupel",):
                contents = f"#!/bin/sh\necho 'usage: {name}'\n".encode()
                member = tarfile.TarInfo(name)
                member.size = len(contents)
                member.mode = 0o755 if executable else 0o644
                archive.addfile(member, io.BytesIO(contents))
        checksum = hashlib.sha256(self.archive.read_bytes()).hexdigest()
        self.sums.write_text(f"{checksum}  {self.archive.name}\n")

    def test_both_executables_are_required(self):
        self.package(acp=False)
        with self.assertRaisesRegex(ValueError, "cupel-acp"):
            release.verify_archive(self.archive, self.sums, self.root / "extract")

    def test_executable_permissions_are_required(self):
        self.package(executable=False)
        with self.assertRaisesRegex(ValueError, "executable cupel"):
            release.verify_archive(self.archive, self.sums, self.root / "extract")

    def test_valid_archive(self):
        self.package()
        destination = self.root / "extract"
        release.verify_archive(self.archive, self.sums, destination)
        self.assertTrue(os.access(destination / "cupel-acp", os.X_OK))

    def test_missing_duplicate_and_incorrect_checksums_are_rejected(self):
        self.package()
        valid = self.sums.read_text()
        for contents in ("", valid + valid, f"{'0' * 64}  {self.archive.name}\n"):
            with self.subTest(checksums=contents):
                self.sums.write_text(contents)
                with self.assertRaisesRegex(ValueError, "checksum"):
                    release.verify_archive(self.archive, self.sums, self.root / "extract")

    def test_default_and_custom_installation(self):
        self.package()
        for custom in (False, True):
            with self.subTest(custom_directory=custom):
                result, install_dir, home = release.install(
                    self.archive, self.sums, self.root / str(custom), custom_directory=custom,
                )
                self.assertEqual(result.returncode, 0, result.stdout)
                for name in ("cupel", "cupel-acp"):
                    self.assertTrue(os.access(install_dir / name, os.X_OK))
                self.assertTrue((install_dir / ".cupel-installer").is_file())
                self.assertTrue((home / "prompts").is_dir())

    def test_installer_rejects_a_bad_checksum_before_installation(self):
        self.package()
        self.sums.write_text(f"{'0' * 64}  {self.archive.name}\n")
        result, install_dir, _ = release.install(self.archive, self.sums, self.root / "install")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum mismatch", result.stdout)
        self.assertFalse(install_dir.exists())

    def test_failed_install_leaves_existing_binaries_untouched(self):
        self.package()
        self.sums.write_text(f"{'0' * 64}  {self.archive.name}\n")
        install_dir = self.root / "install/home/.cupel/bin"
        install_dir.mkdir(parents=True)
        for name in ("cupel", "cupel-acp", ".cupel-installer"):
            (install_dir / name).write_text("existing")
        result, _, _ = release.install(self.archive, self.sums, self.root / "install")
        self.assertNotEqual(result.returncode, 0)
        for name in ("cupel", "cupel-acp", ".cupel-installer"):
            self.assertEqual((install_dir / name).read_text(), "existing")

    def test_install_keeps_existing_configuration(self):
        self.package()
        home = self.root / "install/home/.cupel"
        (home / "prompts").mkdir(parents=True)
        (home / "settings.json").write_text('{"keep":"settings"}')
        (home / "prompts/custom.md").write_text("keep prompt")
        result, _, _ = release.install(self.archive, self.sums, self.root / "install")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertEqual((home / "settings.json").read_text(), '{"keep":"settings"}')
        self.assertEqual((home / "prompts/custom.md").read_text(), "keep prompt")

    def test_older_cupel_only_releases_still_install(self):
        self.package(acp=False)
        result, install_dir, _ = release.install(self.archive, self.sums, self.root / "install")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertTrue(os.access(install_dir / "cupel", os.X_OK))
        self.assertTrue((install_dir / ".cupel-installer").is_file())
        self.assertIn("predates cupel-acp packaging", result.stdout)

    def test_formula_regeneration_keeps_both_binaries(self):
        packaging = Path(__file__).resolve().parent
        formula = self.root / "cupel.rb"
        formula.write_bytes((packaging / "homebrew/cupel.rb").read_bytes())
        self.sums.write_text("".join(
            f"{digit * 64}  cupel-{platform}.tar.gz\n"
            for digit, platform in zip("123", ("macos-universal", "linux-x86_64", "linux-aarch64"))
        ))
        result = subprocess.run(
            ["sh", str(packaging / "update-formula.sh"), "0.99.0", str(self.sums), str(formula)],
            text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stdout)
        contents = formula.read_text()
        self.assertIn('version "0.99.0"', contents)
        self.assertIn('bin.install "cupel", "cupel-acp"', contents)
        self.assertIn('cupel-acp --help', contents)
        for digit in "123":
            self.assertIn(f'sha256 "{digit * 64}"', contents)


if __name__ == "__main__":
    unittest.main()
