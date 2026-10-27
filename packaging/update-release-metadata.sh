#!/bin/sh
# Record a release on main: prepend its CHANGELOG.md section and set the
# workspace version, then push straight to main.
#
#   packaging/update-release-metadata.sh v0.33.10
#
# Run from a clean checkout of main with full history and tags. The push relies
# on the GitHub Actions app being a bypass actor on main's ruleset (see
# packaging/README.md); it never forces, so a concurrent push to main fails the
# job instead of clobbering it. Idempotent: a release already recorded on main
# is a no-op.
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <tag>" >&2
    exit 1
fi

TAG=$1
PACKAGING=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
current_version=$(awk -F '"' '/^version = / { print $2; exit }' Cargo.toml)
update=$(python3 "$PACKAGING/version-order.py" "$TAG" "$current_version")
if [ "$update" != true ]; then
    echo "::notice::Skipping main update for older release $TAG (main is $current_version)"
    exit 0
fi

sh "$PACKAGING/update-changelog.sh" "$TAG"
sh "$PACKAGING/set-workspace-version.sh" "${TAG#v}"

# Refresh workspace versions without upgrading locked dependencies.
cargo update --workspace

git add CHANGELOG.md Cargo.toml Cargo.lock
if git diff --cached --quiet; then
    echo "release $TAG is already recorded on main - nothing to do"
    exit 0
fi

git -c user.name=cupel-release-bot -c user.email=actions@users.noreply.github.com \
    commit -m "changelog: $TAG"
git push origin HEAD:main
