"""Print whether a release version is at least the current version (SemVer order)."""

import argparse
import re


def version_key(value):
    match = re.fullmatch(
        r"v?(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
        r"(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?",
        value,
    )
    if not match:
        raise argparse.ArgumentTypeError(f"invalid release version: {value!r}")
    major, minor, patch, prerelease = match.groups()
    identifiers = prerelease.split(".") if prerelease else []
    if any(part.isdigit() and len(part) > 1 and part.startswith("0") for part in identifiers):
        raise argparse.ArgumentTypeError(f"numeric prerelease identifiers cannot have leading zeros: {value!r}")
    return (
        int(major), int(minor), int(patch), prerelease is None,
        tuple((0, int(part)) if part.isdigit() else (1, part) for part in identifiers),
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("candidate", type=version_key)
    parser.add_argument("current", type=version_key)
    args = parser.parse_args()
    print("true" if args.candidate >= args.current else "false")


if __name__ == "__main__":
    main()
