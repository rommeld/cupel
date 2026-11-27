#!/bin/sh
# Prepare the changelog and workspace version for a release commit. Does not
# commit, push, or create a tag.
#
# Run this script with sh and a version tag such as v0.33.11.
set -eu

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
    echo "usage: $0 <tag> [previous-tag]" >&2
    exit 1
fi

TAG=$1
PACKAGING=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
printf '%s\n' "$TAG" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$' || {
    echo "error: expected vX.Y.Z or vX.Y.Z-prerelease" >&2
    exit 1
}
if [ -n "${2:-}" ]; then
    git rev-parse --verify "refs/tags/$2^{commit}" >/dev/null
fi

sh "$PACKAGING/set-workspace-version.sh" "${TAG#v}"
cargo update --workspace
sh "$PACKAGING/update-changelog.sh" "$TAG" CHANGELOG.md "${2:-}"
