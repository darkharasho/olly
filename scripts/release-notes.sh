#!/usr/bin/env bash
# Prints one release note per line from the feat/fix commits in a range.
# Usage: release-notes.sh <prev-tag-or-empty> <tag>
set -euo pipefail

if [ "$#" -ne 2 ]; then
    echo "🤖 usage: release-notes.sh <prev-tag-or-empty> <tag>" >&2
    exit 1
fi
prev="$1"
tag="$2"

cd "$(dirname "${BASH_SOURCE[0]}")/.."

# The tag may not exist yet when previewing notes; fall back to HEAD.
ref=$(git rev-parse -q --verify "$tag^{commit}" || echo HEAD)
if [ -n "$prev" ]; then
    range="$prev..$ref"
else
    range="$ref"
fi

git log --no-merges --format=%s "$range" |
    sed -n -E \
        -e 's/^feat(\([^)]*\))?!?: /New: /p' \
        -e 's/^fix(\([^)]*\))?!?: /Fixed: /p'
