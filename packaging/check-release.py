"""Check a native release archive and install it offline into a clean home.

Usage: python3 packaging/check-release.py <platform.tar.gz> <sha256sums.txt>
Requires Python 3.12+ for safe tar extraction; no Rust or external Python packages.
"""

import hashlib
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tarfile
import tempfile


def verify_archive(archive, checksums, destination):
    listed = [line.split() for line in checksums.read_text().splitlines()]
    expected = [fields[0] for fields in listed if len(fields) == 2 and fields[1] == archive.name]
    actual = hashlib.sha256(archive.read_bytes()).hexdigest()
    if expected != [actual]:
        raise ValueError(f"missing, duplicate, or incorrect checksum for {archive.name}")
    with tarfile.open(archive) as package:
        package.extractall(destination, filter="data")
    for name in ("cupel", "cupel-acp"):
        binary = destination / name
        if not binary.is_file() or binary.is_symlink() or not os.access(binary, os.X_OK):
            raise ValueError(f"archive has no executable {name}")


def install(archive, checksums, root, *, custom_directory=False):
    """Replace only curl, serving the actual archive/checksums without network."""
    tools = root / "tools"
    tools.mkdir(parents=True)
    curl = tools / "curl"
    curl.write_text(f'''#!/bin/sh
set -eu
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o) output=$2; shift ;;
        https://*) url=$1 ;;
    esac
    shift
done
case "$url" in
    */{shlex.quote(archive.name)}) cp {shlex.quote(str(archive))} "$output" ;;
    */sha256sums.txt) cp {shlex.quote(str(checksums))} "$output" ;;
    *) echo "unexpected download: $url" >&2; exit 1 ;;
esac
''')
    curl.chmod(0o755)
    home = root / "home"
    home.mkdir(exist_ok=True)
    env = {"PATH": f"{tools}{os.pathsep}{os.defpath}", "HOME": str(home), "SHELL": "/bin/sh"}
    cupel_home = home / ".cupel"
    install_dir = cupel_home / "bin"
    if custom_directory:
        cupel_home = root / "custom-home"
        install_dir = root / "custom-bin"
        env.update(CUPEL_HOME=str(cupel_home), CUPEL_INSTALL_DIR=str(install_dir))
    result = subprocess.run(
        ["sh", str(Path(__file__).resolve().parents[1] / "install.sh")],
        env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=30,
    )
    return result, install_dir, cupel_home


def main():
    archive, checksums = (Path(arg).resolve() for arg in sys.argv[1:])
    with tempfile.TemporaryDirectory(prefix="cupel-release-") as directory:
        root = Path(directory)
        extracted = root / "archive"
        verify_archive(archive, checksums, extracted)
        for custom_directory in (False, True):
            result, install_dir, cupel_home = install(
                archive, checksums, root / str(custom_directory), custom_directory=custom_directory,
            )
            if result.returncode:
                raise RuntimeError(result.stdout)
            assert (install_dir / ".cupel-installer").is_file()
            assert (cupel_home / "prompts").is_dir()
            for name in ("cupel", "cupel-acp"):
                binary = install_dir / name
                assert binary.read_bytes() == (extracted / name).read_bytes(), name
                help_result = subprocess.run(
                    [str(binary), "--help"], text=True, stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT, timeout=15,
                    env={"PATH": os.defpath, "HOME": str(cupel_home.parent), "CUPEL_HOME": str(cupel_home)},
                )
                assert help_result.returncode == 0, help_result.stdout
                assert f"usage: {name}" in help_result.stdout, help_result.stdout
            subprocess.run(
                [sys.executable, str(extracted / "examples/stdio_smoke.py"), str(install_dir / "cupel-acp")],
                check=True, timeout=45,
            )
        print(f"PASS: {archive.name}: checksum, both executables, help, default/custom installs, ACP exchange")


if __name__ == "__main__":
    main()
