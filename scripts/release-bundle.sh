#!/usr/bin/env bash
# Writes the release tarball and manifest.json.
# Usage: release-bundle.sh <version> <bin-dir> <out-dir>
set -euo pipefail

if [ "$#" -ne 3 ]; then
    echo "🤖 usage: release-bundle.sh <version> <bin-dir> <out-dir>" >&2
    exit 1
fi
version="$1"
bin_dir="$(cd "$2" && pwd)"
mkdir -p "$3"
out_dir="$(cd "$3" && pwd)"

cd "$(dirname "${BASH_SOURCE[0]}")/.."

tarball="olly-$version-x86_64.tar.gz"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

install -m755 "$bin_dir/olly" "$stage/olly"
install -m755 "$bin_dir/olly-notify" "$stage/olly-notify"
mkdir -p "$stage/dist" "$stage/plasmoid"
install -m644 dist/olly.service dist/olly-notify.service "$stage/dist/"
cp -r plasmoid/package "$stage/plasmoid/package"
printf '%s\n' "$version" >"$stage/VERSION"

tar -C "$stage" --owner=0 --group=0 --numeric-owner -czf "$out_dir/$tarball" \
    olly olly-notify dist plasmoid VERSION

sha=$(sha256sum "$out_dir/$tarball" | cut -d' ' -f1)
ref=$(git rev-parse -q --verify "v$version^{commit}" || echo HEAD)
prev=$(git describe --tags --abbrev=0 --match 'v*' "$ref^" 2>/dev/null || true)
notes=$(scripts/release-notes.sh "$prev" "v$version" |
    jq -R . | jq -s .)

jq -n --arg version "$version" --arg tarball "$tarball" --arg sha "$sha" \
    --argjson notes "$notes" --arg published "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    '{version: $version, tarball: $tarball, sha256: $sha, notes: $notes, published: $published}' \
    >"$out_dir/manifest.json"

echo "🤖 bundled $tarball and manifest.json in $out_dir"
