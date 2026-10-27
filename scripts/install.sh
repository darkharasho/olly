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

sudo systemctl daemon-reload
sudo systemctl enable --now olly
systemctl --user daemon-reload
systemctl --user enable --now olly-notify

echo "🤖 olly is up and watching, in dry-run mode: it will tell you what it would do, but won't act yet."
echo "   Check in with: olly status"
echo "   When you're happy, let it act with:"
echo "   sudo sed -i 's/^dry_run = true/dry_run = false/' /etc/olly/config.toml && sudo systemctl restart olly"
