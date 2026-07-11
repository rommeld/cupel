#!/bin/sh
# Prepend a release section to Changelog.md from git history:
#
#   packaging/update-changelog.sh v0.1.8-beta [Changelog.md]
#
# The section lists the commit subjects between the previous tag and <tag>,
# dated from the tag's commit. Used by the `changelog` job in
# .github/workflows/release.yml after every release, and runnable by hand
# for a backfill. POSIX sh + awk only, same as update-formula.sh.
#
# Two behaviors worth knowing:
# - Idempotent: a section for <tag> already in the file means a re-run
#   (re-tagged release), so it exits 0 without touching anything.
# - A hand-maintained "## [Unreleased]" section is REPLACED by the new
#   tagged section: its commits are part of the generated list anyway, so
#   keeping both would say everything twice.

set -eu

if [ $# -lt 1 ]; then
    echo "usage: $0 <tag> [changelog.md]" >&2
    exit 1
fi

TAG="$1"
CHANGELOG="${2:-Changelog.md}"

git rev-parse -q --verify "refs/tags/$TAG" >/dev/null || {
    echo "error: tag $TAG not found" >&2
    exit 1
}

if grep -q "^## \[$TAG\]" "$CHANGELOG"; then
    echo "$CHANGELOG already has a $TAG section - nothing to do"
    exit 0
fi

# The previous release is the nearest tag reachable from the commit BEFORE
# this one (`TAG^` so `describe` cannot answer with TAG itself). The very
# first tag has no predecessor: log the whole history instead of a range.
PREV="$(git describe --tags --abbrev=0 "$TAG^" 2>/dev/null || true)"
RANGE="${PREV:+$PREV..}$TAG"

DATE="$(git log -1 --format=%cd --date=short "$TAG")"

# The commit list goes through a temp file, not `awk -v`: -v values get
# backslash-escape processing, which would mangle commit subjects that
# happen to contain backslashes.
SECTION="$(mktemp)"
trap 'rm -f "$SECTION"' EXIT
git log --format='- %s' --no-merges "$RANGE" > "$SECTION"
[ -s "$SECTION" ] || echo "- no changes recorded" > "$SECTION"

# Insert the new section right before the first existing "## [" heading so
# the file stays newest-first. The Unreleased rule comes first and `next`s,
# so its heading never reaches the insert rule below it.
awk -v tag="$TAG" -v date="$DATE" -v secfile="$SECTION" '
    function print_section(   line) {
        print "## [" tag "] - " date
        print ""
        while ((getline line < secfile) > 0) print line
        print ""
        inserted = 1
    }
    /^## \[Unreleased\]/ { skipping = 1; next }
    /^## \[/             { if (!inserted) print_section(); skipping = 0 }
    skipping             { next }
    { print }
    END {
        # A changelog with no release sections yet (or Unreleased as the
        # last section): nothing triggered the insert, append at the end.
        if (!inserted) { print ""; print_section() }
    }
' "$CHANGELOG" > "$CHANGELOG.tmp"
mv "$CHANGELOG.tmp" "$CHANGELOG"

echo "updated $CHANGELOG with $TAG"
