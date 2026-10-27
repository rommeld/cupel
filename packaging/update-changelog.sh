#!/bin/sh
# Prepend a release section to CHANGELOG.md from git history, before tagging:
#
#   packaging/update-changelog.sh v0.1.8-beta [CHANGELOG.md]
#
# The section lists commit subjects since the previous tag through HEAD.
# An existing tag can also be used to backfill a past release. POSIX sh + awk
# only, same as update-formula.sh.
#
# Two behaviors worth knowing:
# - Idempotent: a section for <tag> already in the file means a re-run,
#   so it exits 0 without touching anything.
# - A hand-maintained "## [Unreleased]" section is replaced by the new
#   tagged section: its commits are part of the generated list anyway, so
#   keeping both would say everything twice.

set -eu

if [ $# -lt 1 ]; then
    echo "usage: $0 <tag> [changelog.md]" >&2
    exit 1
fi

TAG="$1"
CHANGELOG="${2:-CHANGELOG.md}"

if grep -Fq "## [$TAG]" "$CHANGELOG"; then
    echo "$CHANGELOG already has a $TAG section - nothing to do"
    exit 0
fi

# For a planned release use HEAD; for a backfill exclude the tag itself when
# finding its predecessor. A first release logs the whole history.
REF=HEAD
PREVIOUS_REF=HEAD
if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
    REF=$TAG
    PREVIOUS_REF="$TAG^"
fi
PREV="$(git describe --tags --abbrev=0 "$PREVIOUS_REF" 2>/dev/null || true)"
RANGE="${PREV:+$PREV..}$REF"

DATE="$(git log -1 --format=%cd --date=short "$REF")"

# The commit list goes through a temp file, not `awk -v`: -v values get
# backslash-escape processing, which would mangle commit subjects that
# happen to contain backslashes. Metadata-only `changelog: vX.Y.Z` commits
# are filtered out; `|| true` keeps `set -e` happy when grep filters everything.
SECTION="$(mktemp)"
trap 'rm -f "$SECTION"' EXIT
git log --format='- %s' --no-merges "$RANGE" | grep -v '^- changelog: v' > "$SECTION" || true
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
