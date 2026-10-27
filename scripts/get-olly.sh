#!/usr/bin/env bash
# Installs the latest olly release, prebuilt. No Rust toolchain or checkout needed:
#   curl -fsSL https://raw.githubusercontent.com/darkharasho/olly/main/scripts/get-olly.sh | bash
# Run it as your normal user; sudo is used only for the steps that need it.
# Re-running it upgrades to the latest release. After this, olly updates itself.
#
# For testing: OLLY_RELEASES and OLLY_RAW override where it downloads from, and
# OLLY_DRY_RUN=1 downloads and checks everything but only prints the install steps.
set -euo pipefail

releases="${OLLY_RELEASES:-https://github.com/darkharasho/olly/releases}"
raw="${OLLY_RAW:-https://raw.githubusercontent.com/darkharasho/olly}"
dry="${OLLY_DRY_RUN:-}"

say() { echo "🤖 $*"; }
die() { echo "🤖 $*" >&2; exit 1; }

# Runs a step, or just prints it in a dry run.
run() {
    if [ -n "$dry" ]; then
        echo "+ $*"
    else
        "$@"
    fi
}

if [ "$(id -u)" -eq 0 ] && [ -z "$dry" ]; then
    die "please run this as your normal user, not root. It uses sudo where needed."
fi
for tool in curl tar sha256sum; do
    command -v "$tool" >/dev/null 2>&1 || die "I need $tool to install, and couldn't find it."
done

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

fetch() { curl -fsSL --retry 2 --connect-timeout 15 --max-time 300 -o "$2" "$1"; }

say "looking for the latest olly..."
fetch "$releases/latest/download/manifest.json" "$work/manifest.json" ||
    die "couldn't reach GitHub to find the latest release. Check your connection and try again."

# The manifest is jq's pretty output: one "key": "value" per line.
field() { sed -n "s/^ *\"$1\": *\"\([^\"]*\)\".*/\1/p" "$work/manifest.json" | head -1; }
version=$(field version)
sha=$(field sha256)
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "the release manifest looks wrong (version '$version'), so I stopped."
[[ "$sha" =~ ^[0-9a-f]{64}$ ]] || die "the release manifest looks wrong (no checksum), so I stopped."
tarball="olly-$version-x86_64.tar.gz"

say "downloading olly $version..."
fetch "$releases/download/v$version/$tarball" "$work/$tarball" ||
    die "couldn't download $tarball. Try again in a bit."
got=$(sha256sum "$work/$tarball" | cut -d' ' -f1)
[ "$got" = "$sha" ] || die "the download didn't match its checksum, so I won't install it."

mkdir "$work/pkg"
tar -xzf "$work/$tarball" -C "$work/pkg" --no-same-owner
pkg="$work/pkg"
for f in olly olly-notify dist/olly.service dist/olly-notify.service plasmoid/package/metadata.json VERSION; do
    [ -f "$pkg/$f" ] || die "the release is missing $f, so I stopped."
done
[ "$(cat "$pkg/VERSION")" = "$version" ] || die "the release's VERSION doesn't match $version, so I stopped."

if [ ! -e /etc/olly/config.toml ]; then
    fetch "$raw/v$version/dist/config.toml" "$work/config.toml" ||
        die "couldn't download the default config. Try again in a bit."
fi

say "installing olly $version (sudo will ask for your password)..."
run sudo install -Dm755 "$pkg/olly" /usr/local/bin/olly
run sudo install -Dm755 "$pkg/olly-notify" /usr/local/bin/olly-notify
run sudo install -Dm644 "$pkg/dist/olly.service" /etc/systemd/system/olly.service
if [ -e /etc/olly/config.toml ]; then
    say "keeping your existing /etc/olly/config.toml"
else
    run sudo install -Dm644 "$work/config.toml" /etc/olly/config.toml
fi
run install -Dm644 "$pkg/dist/olly-notify.service" "$HOME/.config/systemd/user/olly-notify.service"
# Copies the self-updater restores or upgrades from (the widget and notify unit).
run sudo install -d /usr/local/share/olly
run sudo rm -rf /usr/local/share/olly/plasmoid
run sudo cp -r "$pkg/plasmoid/package" /usr/local/share/olly/plasmoid
run sudo install -Dm644 "$pkg/dist/olly-notify.service" /usr/local/share/olly/olly-notify.service

# enable, then restart: a re-run picks up the new binaries.
run sudo systemctl daemon-reload
run sudo systemctl enable olly
run sudo systemctl restart olly
run systemctl --user daemon-reload
run systemctl --user enable olly-notify
run systemctl --user restart olly-notify

# The tray face installs per-user (no sudo). --upgrade fails when it isn't
# installed yet, so fall back to --install. plasmashell is never restarted.
if command -v kpackagetool6 >/dev/null 2>&1; then
    if [ -n "$dry" ]; then
        echo "+ kpackagetool6 --type Plasma/Applet --upgrade|--install $pkg/plasmoid/package"
    elif kpackagetool6 --type Plasma/Applet --upgrade "$pkg/plasmoid/package" 2>/dev/null ||
        kpackagetool6 --type Plasma/Applet --install "$pkg/plasmoid/package"; then
        say "tray face installed! If you don't see it, log out and back in, then check System Tray Settings → Entries → olly → Always shown."
    else
        echo "🤖 couldn't install the tray face, but olly itself is fine." >&2
    fi
else
    say "no kpackagetool6 found, so I skipped the tray face."
fi

if [ -n "$dry" ]; then
    say "dry run: olly $version checked out fine, nothing was installed."
elif grep -q '^dry_run = true' /etc/olly/config.toml; then
    say "olly $version is up and watching, in dry-run mode: it will tell you what it would do, but won't act yet."
    echo "   Check in with: olly status"
    echo "   When you're happy, let it act with:"
    echo "   sudo sed -i 's/^dry_run = true/dry_run = false/' /etc/olly/config.toml && sudo systemctl restart olly"
else
    say "olly $version is up and watching. Check in with: olly status. From now on I'll update myself."
fi
