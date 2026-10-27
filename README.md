# olly

olly is a tiny background daemon for a Bazzite desktop (KDE Plasma). It watches the kernel's memory-pressure signals (PSI) and how fast each app's memory is growing, and when something runs away it steps in within seconds, before the system freezes. It slows the app down, pauses it, or as a last resort closes it, and it tells you every time.

The emergency path is plain local rules: no network, no LLM, no spawning processes. Claude is only asked on demand, for slow leaks and for a second opinion after an incident, and it only ever suggests. When nothing is wrong, olly sleeps and uses about 3 MB of memory.

## What it does

- **Watches** PSI memory pressure and available memory, sampling once a minute when things are calm and once a second when they aren't. If the kernel's PSI trigger isn't available, it samples every 2 seconds even when calm.
- **Acts in steps** as things get worse: throttle a growing app, freeze it and ask you what to do, and in an emergency close it. After closing an app, olly waits until it is really gone and memory has had 10 seconds to settle, and it only closes another if memory is still in trouble right now (not just in the 10-second average). When nothing is clearly growing it closes the biggest app as a last resort, at most twice per incident and never a desktop session service. `olly pause` suspends throttling and freezing for a while, but emergency kills still happen, and apps that are already frozen keep their timers.
- **Notifies you** with a desktop notification whenever it acts.
- **Keeps an incident log** in `/var/lib/olly/incidents.jsonl`, with what happened, which app was the biggest, and what olly did.
- **Asks Claude for a hunch** about slow leaks and after incidents (at most 3 a day by default). This runs sandboxed as your desktop user: only the read-only Read, Grep and Glob tools (no shell), no MCP, and your user settings are not loaded. For an incident, olly reads the kernel log itself (up to 200 lines from the incident window) and passes it along as untrusted data. Claude never acts, never edits the never-kill list, and never changes config.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/darkharasho/olly/main/scripts/get-olly.sh | bash
```

Run it as your normal user, not root. It downloads the latest prebuilt release, checks the tarball against the release's checksum, and installs it, so you need no Rust toolchain or checkout. After that, olly updates itself (see [Releases and updates](#releases-and-updates)). This first download trusts GitHub over HTTPS; every later update is also checked against olly's signing key.

To install from a source checkout instead:

```sh
scripts/install.sh
```

It builds with `cargo build --release`, then uses sudo to install `olly` and `olly-notify` into `/usr/local/bin`, the system unit into `/etc/systemd/system`, and the config into `/etc/olly/config.toml` (an existing config is left alone). The notification agent installs as a user unit under `~/.config/systemd/user`. On Bazzite, `/usr/local` is writable, so this works on the immutable base.

To remove it: `scripts/uninstall.sh` from a checkout, or `curl -fsSL https://raw.githubusercontent.com/darkharasho/olly/main/scripts/uninstall.sh | bash`. That stops and disables both units and deletes the binaries and units, but keeps `/etc/olly/config.toml` and `/var/lib/olly` (your incident history). Add `--purge` to remove those too.

## Dry-run first

The shipped config has `dry_run = true`. In dry-run olly watches and notifies as usual, and records what it *would* have done, but it never freezes, throttles, or kills anything. Live with it for a while, check `olly incidents`, and when you're happy:

```sh
sudo sed -i 's/^dry_run = true/dry_run = false/' /etc/olly/config.toml && sudo systemctl restart olly
```

`/etc/olly/config.toml` lists every setting with its default and a comment.

## The `olly` command

```
olly status [--json]        how memory looks right now
olly incidents [-n N] [--json]  recent incidents, newest first
olly incident <id>          one incident in full
olly pause <minutes>        stop slowing and pausing apps for a while
olly resume                 end a pause early
olly allow <name>           never close this process name
olly allowlist              show the never-close list
olly kill <id>              close a paused app (id from `olly status`)
olly unfreeze <id>          let a paused app carry on
olly watch                  follow what olly does, live
```

`olly daemon [--config PATH] [--root PATH]` is what the systemd unit runs. A pause is capped at `max_pause_secs` (one hour by default).

## Tray face

`scripts/install.sh` also installs a small Plasma system-tray widget (`org.olly.tray`) for your user, if `kpackagetool6` is available. It shows olly's mood as a face:

- **All good:** green, happy eyes.
- **Watching:** blue, pressure rising.
- **Worried:** amber, warning or worse.
- **On it!:** red, olly acted in the last minute.
- **Needs you:** orange, an app is frozen and waiting on you.
- **Napping:** purple, paused.
- **Offline:** grey, daemon not reachable.

Click it for a popup with the mood and stats, a Close it / Let it run card while something is frozen, the last 3 incidents (click one to expand it; "All incidents…" shows 20), and Pause 30 min / Wake olly up.

If you don't see it, log out and back in, then check System Tray Settings → Entries → olly → Always shown. The installer never restarts plasmashell.

## The never-kill list

olly never freezes, throttles, or kills an app that contains one of these process names (a trailing `*` matches any ending). A name matches the process name or its executable's file name, so names longer than the kernel's 15-character process name work too:

`olly`, `olly-notify`, `systemd`, `systemd-*`, `dbus-broker`, `dbus-broker-launch`, `dbus-daemon`, `kwin_wayland`, `kwin_wayland_wrapper`, `plasmashell`, `ksmserver`, `kded6`, `Xwayland`, `xdg-desktop-portal*`, `pipewire`, `pipewire-pulse`, `wireplumber`, `sddm`, `NetworkManager`, `sshd`, `polkitd`, `gamescope`

Add your own with `olly allow <name>`; those are saved in `/var/lib/olly/allowlist.toml`. Nothing can remove an entry over IPC. To change the base list, edit `allowlist` in the config.

## Releases and updates

To cut a release: bump `version` in `Cargo.toml`; if anything under `plasmoid/` changed, also bump `KPlugin.Version` in `plasmoid/package/metadata.json` (`scripts/release-check.sh` fails the release if you forget); commit; then `git tag vX.Y.Z && git push --tags`. The `v*` tag runs `.github/workflows/release.yml`: it checks the tag matches `Cargo.toml`, runs the tests, builds static musl binaries, bundles them, signs `manifest.json` with minisign (secret `OLLY_MINISIGN_KEY`), and publishes a GitHub release.

olly checks for a new release 5 minutes after it starts and then every 6 hours, only when things are calm. When there's one, the tray antenna glows blue and the popup shows an update card with an Update button. You can also run `olly update --check`, `olly update`, or `olly update --no-wait` (start it and return). An update only installs while olly is calm with nothing frozen or throttled; olly verifies the signature and checksum, swaps the binaries, and checks that the new daemon answers. If it doesn't, olly rolls back by itself. `olly-notify` then upgrades the widget and its user unit; after a changed widget you may need to log out and back in.

To turn off the automatic checks, set `update_check = false` in `/etc/olly/config.toml`. Manual `olly update` still works.

Updates are verified against the public key in `src/update_key.pub` (key ID `FEE7FE12633D6160`). The secret key lives at `~/.config/olly-release/olly-update.key`; back it up somewhere safe and never commit it. To rotate keys, publish one release signed with the old key that carries the new public key; after that, sign with the new one. If the key is lost, installed copies can't verify new releases, so reinstall once with `scripts/install.sh` from a build with the new key. If it leaks, rotate at once.

## How it relates to systemd-oomd

olly leaves systemd-oomd alone; the two can run side by side. oomd reacts per cgroup after pressure has already stayed high for a while. olly looks at per-app growth as well as pressure, tries gentler options first (throttle, freeze, ask you), and explains itself afterwards. If olly ever fails to act in time, oomd and the kernel OOM killer are still there behind it.

## IPC

olly listens on `/run/olly/olly.sock` (newline-delimited JSON, root and the desktop user only) so other tools, such as otto, can ask for status, read incidents, send simple commands, and follow an event stream. See [docs/ipc.md](docs/ipc.md).

## Try it with memhog

`memhog` is a small example program that grows its memory at a steady rate, so you can watch olly react. Always run it under a hard cap:

```sh
cargo build --release --examples
systemd-run --user --scope -p MemoryMax=14G target/release/examples/memhog 1024 --max 12288
```

`memhog <mb_per_sec> [--max MB]` stops growing at `--max` (default 2048 MB), holds, and exits cleanly on Ctrl-C. In dry-run, olly will say what it would have done.

## Manual checks to run as root

These need a real machine and root, so they aren't part of `cargo test`. Always keep memhog under a `MemoryMax` cap.

1. **Daemon in dry-run (as root, test thresholds).** Copy `dist/config.toml` to `/tmp/olly-test.toml`, keep `dry_run = true`, and raise the thresholds so a capped hog can trigger them: `watch_avail = 45`, `warning_avail = 40`, `critical_avail = 35`, `emergency_avail = 25` (suited to about 16 GB free on a 30 GB machine; adjust for yours). Run `sudo OLLY_RUN_DIR=/tmp/olly target/debug/olly daemon --config /tmp/olly-test.toml`. In a second terminal run `systemd-run --user --scope -p MemoryMax=14G target/release/examples/memhog 512`. Expect level changes and "would have" actions naming memhog on stderr, and a clean exit on Ctrl-C.
2. **Notification agent.** With that daemon running, also run `cargo run --bin olly-notify`. Try `olly status`, `olly pause 1`, `olly status` (shows the pause), `olly resume`. Then run memhog again: a KDE notification with Kill/Resume should appear, and clicking Kill should leave `olly status` showing nothing frozen.
3. **End to end.** Run `scripts/install.sh`, switch `/etc/olly/config.toml` to `dry_run = false` with the same raised thresholds, and restart olly. Run `systemd-run --user --scope -p MemoryMax=14G target/release/examples/memhog 1024`. Expect throttle and/or freeze notifications, then a kill before the 14G cap, the incident in `olly incidents`, and a Claude hunch once memory recovers. Afterwards restore the shipped config (`dry_run = true`, default thresholds) and restart olly.
4. **Tray face.** Run `scripts/install.sh` and look for the olly face in the system tray. With the dry-run daemon and raised thresholds from check 1, run memhog and watch the face move through its moods. Then, on a dry-run freeze, press Close it and Let it run in the popup and confirm `olly status` agrees.

## Uses

Rust, Linux PSI and cgroups v2, systemd, and the Claude CLI for on-demand diagnosis. The daemon depends only on serde, serde_json, toml and libc; `olly-notify` adds notify-rust.
