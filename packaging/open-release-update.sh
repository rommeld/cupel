#!/bin/sh
# Propose release bookkeeping through a PR instead of pushing to protected main.
# Run from a clean checkout of main with full history and tags.
# GH_REPO and GH_TOKEN select and authenticate the repository.
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
version=${TAG#v}
awk -v v="$version" '
    /^\[workspace\.package\]/ { inpkg = 1 }
    /^\[/ && !/^\[workspace\.package\]/ { inpkg = 0 }
    inpkg && /^version = / && !done { print "version = \"" v "\""; done = 1; next }
    { print }
    END { if (!done) exit 1 }
' Cargo.toml > Cargo.toml.tmp
mv Cargo.toml.tmp Cargo.toml
cargo update --workspace

git add CHANGELOG.md Cargo.toml Cargo.lock
if git diff --cached --quiet; then
    echo "release $TAG is already recorded on main - nothing to do"
    exit 0
fi

# Rebuild this workflow-owned branch from the checked-out main on retries.
branch="release/changelog-$TAG"
git switch -C "$branch"
git -c user.name=cupel-release-bot -c user.email=actions@users.noreply.github.com \
    commit -m "changelog: $TAG"
git push --force-with-lease origin "HEAD:refs/heads/$branch"

pr=$(gh pr list --state open --base main --head "$branch" --json number --jq '.[0].number // empty')
if [ -z "$pr" ]; then
    gh pr create --base main --head "$branch" --title "changelog: $TAG" \
        --body "Update CHANGELOG.md and workspace versions for release $TAG. Merge after the required CI checks pass."
fi

# GITHUB_TOKEN-created PRs do not trigger pull_request workflows. Dispatch CI
# explicitly so the protected branch's required status checks can run.
gh workflow run ci.yml --ref "$branch"
