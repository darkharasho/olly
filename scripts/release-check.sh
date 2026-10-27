#!/usr/bin/env bash
# Refuses a release whose tag disagrees with Cargo.toml, or whose widget
# changed without its own version being bumped.
# Usage: release-check.sh <tag>
set -euo pipefail

if [ "$#" -ne 1 ]; then
    echo "🤖 usage: release-check.sh <tag>" >&2
    exit 1
fi
tag="$1"

if ! [[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "🤖 tag $tag isn't a plain vX.Y.Z release tag, so I won't release it." >&2
    exit 1
fi

cd "$(dirname "${BASH_SOURCE[0]}")/.."

cargo_version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
if [ "v$cargo_version" != "$tag" ]; then
    echo "🤖 tag $tag doesn't match Cargo.toml version $cargo_version. Bump Cargo.toml or retag." >&2
    exit 1
fi

ref=$(git rev-parse -q --verify "$tag^{commit}" || echo HEAD)
prev=$(git describe --tags --abbrev=0 --match 'v*' "$ref^" 2>/dev/null || true)
if [ -z "$prev" ]; then
    echo "🤖 first release, skipping the widget version check."
    exit 0
fi

meta=plasmoid/package/metadata.json
if ! git diff --quiet "$prev" "$ref" -- plasmoid/; then
    old=$(git show "$prev:$meta" | jq -r '.KPlugin.Version')
    new=$(git show "$ref:$meta" | jq -r '.KPlugin.Version')
    if [ "$old" = "$new" ]; then
        echo "🤖 plasmoid/ changed since $prev but its KPlugin.Version is still $new. Bump it in $meta." >&2
        exit 1
    fi
fi
echo "🤖 release check passed for $tag."
