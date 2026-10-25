#!/bin/sh
# Resume a draft after an interrupted upload; leave published releases untouched.
# Run in the directory containing the three platform archives and sha256sums.txt.
# GH_REPO and GH_TOKEN select and authenticate the repository.
set -eu

TAG=$1
for asset in cupel-macos-universal.tar.gz cupel-linux-x86_64.tar.gz cupel-linux-aarch64.tar.gz sha256sums.txt; do
    test -s "$asset" || { echo "error: missing release asset: $asset" >&2; exit 1; }
done

if draft=$(gh release view "$TAG" --json isDraft --jq '.isDraft'); then
    if [ "$draft" = false ]; then
        echo "release $TAG is already published - leaving its assets unchanged"
        exit 0
    fi
else
    gh release create "$TAG" --draft --title "cupel $TAG" --generate-notes --verify-tag
fi

# Only drafts reach this point. Upload every asset before making the release public.
gh release upload "$TAG" cupel-*.tar.gz sha256sums.txt --clobber
gh release edit "$TAG" --draft=false
