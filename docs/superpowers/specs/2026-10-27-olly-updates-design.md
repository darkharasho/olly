# olly releases and updates — design

Date: 2026-10-27. Status: approved. This builds on `2026-10-27-olly-design.md`, `2026-10-27-olly-tray-design.md` and `docs/ipc.md`.

## Intent

Today, getting a new olly means pulling the repo and re-running `scripts/install.sh`. olly should keep itself current the way a friendly app does:

1. You publish a release by pushing a tag.
2. olly's face tells you a new version is ready, with what changed.
3. You click Update.
4. A few seconds later you're on the new version. There's no password prompt and no terminal.

Success is when releasing takes one `git push` of a tag, and updating takes one click you don't have to think about.

## Goals

- **One-step releases.** Pushing a version tag builds, signs and publishes a GitHub release with no other manual steps. The release notes are generated from commit messages.
- **Update checks.** olly checks for a newer release now and then. When it finds one, the tray popup shows the version and what changed, with an Update button.
- **One-click installs.** Clicking Update installs the release with no password prompt. That includes the daemon, `olly-notify` and the tray widget.
- **Signed releases only.** olly only installs a release signed with your key.
- **Rollback.** A bad update rolls back to the previous version by itself, and the popup says so.
- **CLI access.** `olly update` and `olly update --check` do the same from a terminal or from otto.

## Non-goals

- Fully automatic updates without a click.
- Pre-releases, release channels, or choosing a version to downgrade to.
- Updating `/etc/olly/config.toml`. Your config is never touched.
- Platforms other than x86_64 Bazzite / Fedora Atomic with KDE Plasma 6.
- Delta updates. A release is small, so the whole bundle is downloaded.

## Anti-goals (a failure even if it technically works)

- **Weakening olly's safety.** Network access, downloads and installs never run in the emergency path or the main loop, and never while memory pressure is high. Idle checking costs almost nothing, and the daemon's steady memory footprint doesn't grow by a TLS stack.
- **Letting something else in as root.** Anything that can talk to olly's socket can ask for an update, so an update must never install anything except a newer, correctly signed official release.
- **Getting stranded.** A failed or half-finished update must never leave you with a stopped or broken olly. Rollback doesn't depend on the new version working.
- **Nagging.** Finding an update sends no desktop notification. A failed check shows no error popups, and logs at most one quiet line.
- **Interrupting protection.** It never updates during an incident or while an app is frozen and waiting on you.

## The release pipeline

Trigger: pushing a tag matching `v*` (e.g. `v0.2.0`). This uses a new workflow `.github/workflows/release.yml` on GitHub-hosted runners.

1. **Checks.** It checks that the tag (without `v`) equals `version` in `Cargo.toml`. If they differ, the workflow fails and nothing is published. Then it runs the same tests as CI: `cargo test --locked` and the node tray tests.
2. **Build.** It builds the release binaries `olly` and `olly-notify` for x86_64 Linux. They must run on current Bazzite (Fedora 44 era glibc or newer). Whether to build against an old glibc or statically with musl is the builder's call.
3. **Bundle.** It makes `olly-<version>-x86_64.tar.gz` containing:
   - `olly`
   - `olly-notify`
   - `dist/olly.service`
   - `dist/olly-notify.service`
   - `plasmoid/package/`
   - a `VERSION` file
4. **Manifest.** It writes `manifest.json`: `{version, tarball, sha256, notes: [..], published}`. `notes` is the list of `feat:` and `fix:` commit subjects since the previous tag (all commits for the first release), with the `feat(scope): ` / `fix: ` prefix turned into a friendly form. The exact wording is the builder's call. Merge commits and other types (`docs:`, `chore:`, `test:`) are left out.
5. **Sign.** It signs `manifest.json` with minisign, producing `manifest.json.minisig`. The secret key is the repo secret `OLLY_MINISIGN_KEY`. The matching public key is compiled into olly. The manifest carries the tarball's sha256, so signing the manifest covers the tarball.
6. **Publish.** It publishes a GitHub release for the tag, with the generated notes as its body and the tarball, `manifest.json` and `manifest.json.minisig` as assets.

Key setup is done (2026-10-27):
- A minisign key pair without a password (my call: CI then needs only the one secret), key ID `FEE7FE12633D6160`.
- The secret key is in the repo secret `OLLY_MINISIGN_KEY`. The only other copy is `~/.config/olly-release/olly-update.key` (mode 600) on your machine; back that file up somewhere safe.
- The public key is `~/.config/olly-release/olly-update.pub`. The builder commits it as `src/update_key.pub`, included at build time:
  `RWRgYT1jEv7n/tvpXS6kgjSepfvW4OfX+TGfvXMJEZJNhMht6u9fYMLB`
- The secret key never enters the repo. CI installs minisign on the runner to sign.

**Key rotation (my call).** No special mechanism is needed. To rotate, publish one release signed with the old key whose binary carries the new public key. Every later release is signed with the new key.

**Key loss or leak.** If the secret key is lost, installed copies can't verify updates any more and must be reinstalled once with `install.sh`. If it leaks, rotate at once. The README documents both.

**Scope of this work (my call).** It stops at "ready to release". The first real tag push (`v0.2.0`, after bumping `Cargo.toml`) publishes publicly, so it happens only when you say so. An end-to-end check of update and rollback needs two published releases, so it is a manual check after that.

## How olly finds an update

- **Who checks.** The daemon checks, in a background worker thread, never on the main loop.
- **When.**
  - First check: 5 minutes after start.
  - Then: every 6 hours.
  - On demand: through `olly update --check`.
  - Skipped while memory level is above `calm`, during an incident, or while paused. A skipped check is retried at the next interval.
- **How.**
  - It fetches `https://github.com/darkharasho/olly/releases/latest/download/manifest.json` and its `.minisig`. The repo is public, so no token or GitHub API is needed.
  - It verifies the signature with the built-in public key.
  - It compares versions with semver, so only strictly newer counts.
  - "Latest" on GitHub excludes pre-releases, which is what we want.
- **Downloading (my call).** The daemon runs the system `curl` as a short-lived subprocess with tight timeouts, rather than linking an HTTP/TLS stack. That keeps the daemon's resident memory small. Signature checking uses a small pure-Rust minisign verifier. `minisign-verify` is already in the local cargo cache; the build is offline, so new crates must be available offline.
- **Failure.** Any failure (no network, bad signature, unparsable manifest) means "no update known". It's logged once at a low level and retried at the next interval. A bad signature is logged as a warning, since it suggests tampering.
- **Config.** `update_check = true` in config. With `false`, olly never checks on its own. `olly update --check` still works on demand.

## What you see

- **Antenna light.** When an update is known, the tray face's antenna tip glows (my call: the "watch" blue `#4fc3f7`) in every mood. It's a light, not a new mood, so it never hides what olly is feeling.
- **Popup card.** The popup shows an update card above the stats (below the needs-you card if one is showing):
  - "olly 0.2.0 is ready!"
  - the first 5 notes, plus "and N more" with a link to the release page if there are more
  - an **Update** button
  - Copy is in olly's voice.
- **While updating.** The card shows "updating…". During the restart the face may briefly show Offline, which is expected.
- **After updating.**
  - On success, the card says "updated to 0.2.0! ✨" until the popup is next closed.
  - On a rollback, it says "that update didn't work, so I went back to 0.1.0." The failure is also kept in `olly status`.
- **Blocked.** If you click Update during an incident or with a frozen app, the card says "I'll update once things are calm" and nothing happens. You can click again later. A queued update is not needed (my call: keep it simple).
- **CLI.**
  - `olly update --check` prints whether a newer version exists, with its notes.
  - `olly update` installs it. It prints olly's usual 🤖 voice lines and exits non-zero on failure or refusal.
  - `olly status` mentions a known update in one line.

## How an update installs

The daemon is root and already allowed to manage olly's files, so it installs the update itself and no prompt is needed. An update is refused unless memory level is `calm`, no incident is open, nothing is frozen and nothing is throttled.

1. **Stage.**
   - The daemon (in its worker thread) downloads the tarball to `/var/lib/olly/update/`.
   - It re-verifies the manifest signature.
   - It checks the tarball's sha256 against the signed manifest.
   - It unpacks the tarball and checks that `VERSION` matches.
   - Any mismatch aborts, and nothing is changed.
2. **Hand off.** The swap is done by the *current, known-good* binary, never the new one.
   - The daemon copies its own binary into the update directory.
   - It launches it as a separate transient systemd unit (`systemd-run`) in an update-supervisor mode, so it survives olly's own restart.
   - The exact mechanism is the builder's call. The requirement is that rollback must work even if the new binary crashes on start.
3. **Swap.** The supervisor:
   - keeps the current files as backups (`/usr/local/bin/olly.prev` and the rest)
   - installs the new `olly` and `olly-notify` with atomic renames
   - installs the new `olly.service` (system) if it changed, and runs `daemon-reload`
   - copies the new plasmoid package to `/usr/local/share/olly/plasmoid/`
   - restarts `olly.service`
4. **Health check.** The supervisor waits up to 30 s for the new daemon to answer `status` on the socket, with the new version in it.
   - **Success:** it deletes the backups, writes the result to `/var/lib/olly/update-result.json` and tells the running daemon (e.g. with an IPC request only the supervisor uses, or by having it re-read the file), then exits.
   - **Failure:** it restores the backups, writes the rollback result, restarts olly and exits.
5. **Report.** On start, and when the supervisor tells it, the daemon loads `update-result.json`. Then `status` reports the last update's outcome, and the daemon emits the `updated` event once per result. (My call: this fixes the order, since the success result only exists after the new daemon has started.)

### The user side

The daemon is root, but the user parts live in your home: the widget, and the `olly-notify` user unit. `olly-notify` already runs as you and is subscribed to the event stream.

- **Timing (my call).** The user side only changes after a *confirmed* success, never during the 30 s health check, so a rollback never leaves a newer widget against an older daemon.
- **What it does.** On an `updated` event with `ok: true`, and also every time it starts (to catch up if you weren't logged in during the update):
  - It compares the installed widget's version with `/usr/local/share/olly/plasmoid`. The release workflow stamps the version into the plasmoid's `metadata.json`.
  - It runs `kpackagetool6 --type Plasma/Applet --upgrade /usr/local/share/olly/plasmoid` if they differ.
  - It updates `~/.config/systemd/user/olly-notify.service` if that changed.
  - It then exits so `systemd --user` restarts it on the new binary.
- **Rollbacks.** On a rollback the plasmoid copy in `/usr/local/share/olly/` is restored too, so it always matches the installed daemon.
- Plasma may need a log out and back in to load changed widget QML. The success card says so only when the widget actually changed (my call).
- Root never writes into your home directory.

### Install script

`scripts/install.sh` also installs the plasmoid copy to `/usr/local/share/olly/plasmoid/`, so the first update works the same way. It is still how you install from a source checkout.

## IPC and status

- **New requests:**
  - `{"cmd":"update_check"}` checks now. Its data is the update info or null.
  - `{"cmd":"update"}` starts the install. Its data is "started", or an error if refused or nothing is available.
- **New `StatusData` fields**, all `#[serde(default)]`, documented in `docs/ipc.md`:
  - `version`: olly's running version.
  - `update`: `{version, notes, url}` or null.
  - `update_state`: `idle` | `checking` | `downloading` | `installing`.
  - `last_update`: `{from, to, ok, at_wall_ms, message}` or null.
- **New event:** `{"event":"updated", ...}` on the stream after a restart that completed an update or a rollback. `olly-notify` uses it.
- **Unchanged.** Existing requests and fields stay as they are, so otto keeps working, and an older widget against a newer daemon still works.

## Left to the builder

- **Workflow details.** Caching, action versions, and how the notes are worded.
- **Binary build.** musl or an old-glibc build.
- **Supervisor mechanism.** How the update-supervisor process is launched and kept alive through the restart, as long as rollback never depends on the new binary.
- **File layout** under `/var/lib/olly/update/`.
- **Tests:**
  - Rust tests for manifest parsing, signature verification (good, bad and wrong-key signatures, using a test key), semver comparison, the refusal rules, and the rollback decision.
  - A test that the release workflow's tag/version check fails on a mismatch, as a script test.
  - Node tests for the new popup copy helpers.
- **Mood and copy wording**, in olly's voice.
