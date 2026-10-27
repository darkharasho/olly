#!/usr/bin/env bash
# Dev-only static checks for the tray widget. Never touches your real Plasma
# setup: the package is installed into a temp packageroot and removed after.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

echo "🤖 checking metadata.json..."
python3 -m json.tool plasmoid/package/metadata.json >/dev/null

echo "🤖 installing into a temp packageroot..."
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
kpackagetool6 --type Plasma/Applet --install plasmoid/package --packageroot "$tmp"
if ! kpackagetool6 --type Plasma/Applet --list --packageroot "$tmp" | grep -q 'org\.olly\.tray'; then
    echo "🤖 org.olly.tray is missing from the package list" >&2
    exit 1
fi

if command -v podman >/dev/null; then
    echo "🤖 running qmllint in a throwaway Fedora container..."
    # The container has no KDE modules, so every org.kde.* import is unresolved
    # and qmllint warns about it and about everything that uses those types.
    # Those warnings are expected: only error lines and syntax problems fail.
    # qmllint reports a syntax error as "Warning: file:line:col: ... [syntax]",
    # and prints source lines as context, so match the prefix and the tag only.
    out=$(podman run --rm -v "$PWD/plasmoid/package/contents/ui:/ui:ro,Z" registry.fedoraproject.org/fedora:latest \
        sh -c 'dnf -y -q install qt6-qtdeclarative-devel >/dev/null 2>&1 && echo "qmllint ran" && /usr/lib64/qt6/bin/qmllint /ui/*.qml' 2>&1) || true
    if grep -E '^Error|\[syntax\]' <<<"$out"; then
        echo "🤖 qmllint found errors" >&2
        exit 1
    fi
    if ! grep -q '^qmllint ran$' <<<"$out"; then
        echo "$out" >&2
        echo "🤖 qmllint didn't run (image pull or dnf failed?)" >&2
        exit 1
    fi
else
    echo "🤖 podman not found, skipping qmllint"
fi

echo "🤖 plasmoid checks passed"
