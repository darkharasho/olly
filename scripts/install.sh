#!/usr/bin/env bash
# Builds olly and installs it. Run as your normal user; sudo is used only
# for the steps that need it.
set -euo pipefail

if [ "$(id -u)" -eq 0 ]; then
    echo "🤖 please run install.sh as your normal user, not root. It uses sudo where needed." >&2
    exit 1
fi

cd "$(dirname "${BASH_SOURCE[0]}")/.."

echo "🤖 building olly..."
cargo build --release

echo "🤖 installing (sudo will ask for your password)..."
sudo install -Dm755 target/release/olly /usr/local/bin/olly
sudo install -Dm755 target/release/olly-notify /usr/local/bin/olly-notify
sudo install -Dm644 dist/olly.service /etc/systemd/system/olly.service
if [ -e /etc/olly/config.toml ]; then
    echo "🤖 keeping your existing /etc/olly/config.toml"
else
    sudo install -Dm644 dist/config.toml /etc/olly/config.toml
fi
install -Dm644 dist/olly-notify.service "$HOME/.config/systemd/user/olly-notify.service"
# Copies the self-updater restores or upgrades from (the widget and notify unit).
sudo install -d /usr/local/share/olly
sudo rm -rf /usr/local/share/olly/plasmoid && sudo cp -r plasmoid/package /usr/local/share/olly/plasmoid
sudo install -Dm644 dist/olly-notify.service /usr/local/share/olly/olly-notify.service

# The tray face installs per-user (no sudo), before olly-notify restarts: it would
# otherwise see the old face and upgrade it at the same time. --upgrade fails when it isn't
# installed yet, so fall back to --install. plasmashell is never restarted.
if command -v kpackagetool6 >/dev/null 2>&1; then
    if kpackagetool6 --type Plasma/Applet --upgrade plasmoid/package 2>/dev/null || kpackagetool6 --type Plasma/Applet --install plasmoid/package; then
        echo "🤖 tray face installed! If you don't see it, log out and back in, then check System Tray Settings → Entries → olly → Always shown. If an upgraded face looks old or blank, restart Plasma: systemctl --user restart plasma-plasmashell"
    else
        echo "🤖 couldn't install the tray face, but olly itself is fine." >&2
    fi
else
    echo "🤖 no kpackagetool6 found, so I skipped the tray face."
fi

# enable, then restart: a re-run after a rebuild picks up the new binaries.
sudo systemctl daemon-reload
sudo systemctl enable olly
sudo systemctl restart olly
systemctl --user daemon-reload
systemctl --user enable olly-notify
systemctl --user restart olly-notify

if grep -q '^dry_run = true' /etc/olly/config.toml; then
    echo "🤖 olly is up and watching, in dry-run mode: it will tell you what it would do, but won't act yet."
    echo "   Check in with: olly status"
    echo "   When you're happy, let it act with:"
    echo "   sudo sed -i 's/^dry_run = true/dry_run = false/' /etc/olly/config.toml && sudo systemctl restart olly"
else
    echo "🤖 olly is up and watching. Check in with: olly status"
fi
