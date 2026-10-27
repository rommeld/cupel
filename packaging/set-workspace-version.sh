#!/bin/sh
# Rewrite the inherited [workspace.package] version in Cargo.toml. Shared by
# the release build (which pins the version to the tag before compiling) and
# the changelog job (which records it on main). Usage:
#
#   packaging/set-workspace-version.sh 0.33.10
#
# Takes the bare version (no leading `v`). POSIX sh + awk only, like the rest
# of packaging/.
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <version>" >&2
    exit 1
fi

version=$1
awk -v v="$version" '
    /^\[workspace\.package\]/ { inpkg = 1 }
    /^\[/ && !/^\[workspace\.package\]/ { inpkg = 0 }
    inpkg && /^version = / && !done { print "version = \"" v "\""; done = 1; next }
    { print }
    END { if (!done) exit 1 }
' Cargo.toml > Cargo.toml.tmp
mv Cargo.toml.tmp Cargo.toml
