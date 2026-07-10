#!/bin/sh
# Rewrite packaging/homebrew/cupel.rb for a release: set `version` and the
# three per-asset sha256 values from a release's sha256sums.txt.
#
#   packaging/update-formula.sh 0.1.4-beta sha256sums.txt
#
# Used by the `homebrew` job in .github/workflows/release.yml after every
# release, and runnable by hand for a manual bump. POSIX sh + awk only.
#
# The awk pass keys each `sha256` line off the ASSET named in the `url`
# line right above it, so it keeps working if the platform blocks are ever
# reordered.

set -eu

if [ $# -lt 2 ]; then
    echo "usage: $0 <version-without-v> <sha256sums.txt> [formula.rb]" >&2
    exit 1
fi

VERSION="$1"
SUMS="$2"
FORMULA="${3:-$(dirname "$0")/homebrew/cupel.rb}"

sha_for() {
    # sha256sums.txt lines look like: "<hash>  <filename>"
    awk -v file="$1" '$2 == file { print $1 }' "$SUMS"
}

MACOS_SHA="$(sha_for cupel-macos-universal.tar.gz)"
X86_SHA="$(sha_for cupel-linux-x86_64.tar.gz)"
ARM_SHA="$(sha_for cupel-linux-aarch64.tar.gz)"

for pair in "macos:$MACOS_SHA" "linux-x86_64:$X86_SHA" "linux-aarch64:$ARM_SHA"; do
    case "$pair" in
        *:) echo "error: no checksum for ${pair%:} in $SUMS" >&2 && exit 1 ;;
    esac
done

awk -v version="$VERSION" \
    -v macos="$MACOS_SHA" -v x86="$X86_SHA" -v arm="$ARM_SHA" '
    # Keep the header comment honest about which release the values match.
    /REAL values for v/ { sub(/v[0-9][0-9A-Za-z.-]*/, "v" version) }
    /^  version "/      { print "  version \"" version "\""; next }
    /cupel-macos-universal/ { asset = "macos" }
    /cupel-linux-x86_64/    { asset = "x86" }
    /cupel-linux-aarch64/   { asset = "arm" }
    /sha256 "/ {
        if (asset == "macos")    sub(/sha256 "[^"]*"/, "sha256 \"" macos "\"")
        else if (asset == "x86") sub(/sha256 "[^"]*"/, "sha256 \"" x86 "\"")
        else if (asset == "arm") sub(/sha256 "[^"]*"/, "sha256 \"" arm "\"")
        asset = ""
    }
    { print }
' "$FORMULA" > "$FORMULA.tmp"
mv "$FORMULA.tmp" "$FORMULA"

echo "updated $FORMULA to $VERSION"
