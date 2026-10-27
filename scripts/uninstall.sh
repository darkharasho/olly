#!/usr/bin/env bash
# Removes olly. Keeps /etc/olly/config.toml and /var/lib/olly unless --purge.
set -euo pipefail

purge=0
case "${1:-}" in
    "") ;;
    --purge) purge=1 ;;
    *)
        echo "🤖 usage: uninstall.sh [--purge]" >&2
        exit 2
        ;;
esac

if [ "$(id -u)" -eq 0 ]; then
    echo "🤖 please run uninstall.sh as your normal user, not root. It uses sudo where needed." >&2
    exit 1
fi

echo "🤖 stopping olly (sudo will ask for your password)..."
systemctl --user disable --now olly-notify 2>/dev/null || true
if [ -e /etc/systemd/system/olly.service ]; then
    if ! sudo systemctl disable --now olly; then
        echo "🤖 couldn't stop olly, so I left it installed. Check 'systemctl status olly' and run uninstall.sh again." >&2
        exit 1
    fi
fi

if command -v kpackagetool6 >/dev/null 2>&1; then
    kpackagetool6 --type Plasma/Applet --remove org.olly.tray 2>/dev/null || true
    echo "🤖 tray face tidied away (if it was there)."
fi

rm -f "$HOME/.config/systemd/user/olly-notify.service"
systemctl --user daemon-reload || true
sudo rm -f /etc/systemd/system/olly.service /usr/local/bin/olly /usr/local/bin/olly-notify
sudo systemctl daemon-reload

if [ "$purge" -eq 1 ]; then
    sudo rm -rf /etc/olly /var/lib/olly
    echo "🤖 olly is gone, along with its config and incident history."
else
    echo "🤖 olly is gone. I kept /etc/olly/config.toml and /var/lib/olly; add --purge to remove them too."
fi
