# olly releases and updates Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Pushing a `v*` tag publishes a signed release, and olly finds it, shows it in the tray, and installs it on one click with automatic rollback.

**Architecture:** A release workflow builds, bundles, signs (minisign) and publishes. In the daemon, a worker thread checks and stages updates using `curl` and `tar` subprocesses; the current binary is then launched under `systemd-run` as a supervisor that swaps files, health-checks the new daemon over the socket, and rolls back on failure. `olly-notify` does the user-side follow-up (widget, user unit) after a confirmed success.

**Tech Stack:** Rust 2021 (new crates: `minisign-verify` 0.2, `sha2` 0.10, `semver` 1, all in the offline cargo cache), bash + jq for release scripts, GitHub Actions, QML/JS for the widget, node:test for JS tests.

**Spec:** `docs/superpowers/specs/2026-10-27-olly-updates-design.md`

## Global Constraints

- Tests: `cargo test -j 2 -- --test-threads=2`; node: `node --test tests/tray/*.test.mjs`. Never more parallelism.
- Builds are offline: `cargo build --offline` must work. Only the three crates above may be added.
- Never run sudo, `scripts/install.sh`, `scripts/uninstall.sh`, `systemctl`, `systemd-run`, or the daemon as root. Never restart plasmashell. The widget is only ever installed with `kpackagetool6 --packageroot <tmp>` (via `scripts/check-plasmoid.sh`).
- Never push, tag, or publish a release. Never print or read `~/.config/olly-release/olly-update.key`.
- No network or update work on the daemon's main loop or emergency path. The main loop only spawns workers and reads their results with `try_recv`.
- Updates are refused unless level is `calm`, no incident is open, nothing is frozen and nothing is throttled. Checks are skipped unless level is `calm`, no incident is open, and olly isn't paused.
- Check timing: first check 5 min (300 000 ms) after start, then every 6 h (21 600 000 ms).
- Health check: up to 30 s for `status` with the new `version`.
- Release URL: `https://github.com/darkharasho/olly/releases/latest/download/manifest.json` (and `.minisig`). Tarball: `https://github.com/darkharasho/olly/releases/download/v<version>/olly-<version>-x86_64.tar.gz`. Release page: `https://github.com/darkharasho/olly/releases/tag/v<version>`.
- Public key (commit as `src/update_key.pub`, exact copy of `~/.config/olly-release/olly-update.pub`):
  ```
  untrusted comment: minisign public key FEE7FE12633D6160
  RWRgYT1jEv7n/tvpXS6kgjSepfvW4OfX+TGfvXMJEZJNhMht6u9fYMLB
  ```
- Install paths: `/usr/local/bin/olly`, `/usr/local/bin/olly-notify`, `/etc/systemd/system/olly.service`, `/usr/local/share/olly/plasmoid/`, `/usr/local/share/olly/olly-notify.service`. Update work dir: `/var/lib/olly/update/`. Result: `/var/lib/olly/update-result.json`.
- Every new `StatusData` field is `#[serde(default)]`. Existing requests and fields are unchanged.
- Copy is in olly's voice, CLI lines start with 🤖 (`\u{1F916}`). Exact strings fixed by the spec: "olly 0.2.0 is ready!", "updating…", "updated to 0.2.0! ✨", "that update didn't work, so I went back to 0.1.0.", "I'll update once things are calm".
- Antenna glow colour: `#4fc3f7`.
- `update_check = true` config key, default true.

## Rulings made while planning (spec gaps)

- **Widget version is the widget's own, not stamped per release.** The spec says the workflow stamps the olly version into `metadata.json`. That would change the widget on every release and force a logout note every time, against "say so only when the widget actually changed". Instead `metadata.json` `KPlugin.Version` is bumped by hand when `plasmoid/` changes, and `scripts/release-check.sh` fails the release if `plasmoid/` changed since the previous tag but the version didn't. The daemon reports the shipped copy's version as a new status field `widget_version`; the widget shows the logout hint when its own running version differs.
- **Event shape.** Events are serde's externally tagged form, so the spec's `updated` event is `{"event":{"Updated":{...}},"wall_ms":…}`, a new `Event::Updated` variant. The daemon emits it when it loads a result less than 10 minutes old (on start or on `update_finished`); `olly-notify` also catches up on its own start, so nothing is lost after that window.
- **Supervisor notification.** A new request `{"cmd":"update_finished"}` makes the daemon re-read `update-result.json`. It is harmless from any client: it only re-reads a root-owned file.
- **Slow replies.** `update_check` waits for the fetch. The daemon keeps the reply sender and answers when the worker finishes; the IPC connection waits up to 60 s for this request only (others stay at 5 s).
- **Widget click.** The widget runs `olly update --no-wait`, which returns as soon as the daemon says "started". Plain `olly update` follows progress until the result arrives.
- **Download tools.** `curl` and `tar` subprocesses (spec's curl call extended to tar, so no tar crate); sha256 via the `sha2` crate, so the hash check doesn't depend on another subprocess.

## Review Focus

- **The state changes during a download.** Memory gets tight or an app gets frozen while the tarball downloads: the install must not start. The daemon re-checks the refusal rules when staging finishes, and drops back to idle with the update still offered. Test in Task 4.
- **A manifest that lies.** A correctly signed manifest whose `tarball` name has a path in it, or whose version is not newer, or a tarball whose `VERSION` differs: refused, nothing changed. Tests in Task 2.
- **The new daemon never answers.** The new binary exits at once or reports the old version: the supervisor restores every file (binaries, unit, plasmoid copy, notify unit copy) and the result says rollback. Test in Task 3.
- **olly-notify sees the same result twice** (it reconnects, or restarts after its own update): it must not loop restarting itself. It exits only when the event says ok and the event's `to` differs from its own compiled version. Test in Task 6.
- **Clicking Update twice / clicking while a check is running.** A second `update` while one is in progress is refused with a friendly message, not a second supervisor. Test in Task 4.

---

### Task 1: Release pipeline

**Files:**
- Create: `scripts/release-check.sh`, `scripts/release-notes.sh`, `scripts/release-bundle.sh`, `.github/workflows/release.yml`, `src/update_key.pub`
- Test: `tests/release_scripts.rs`

**Interfaces:**
- Produces: `scripts/release-check.sh <tag>` exits 0 or 1 with a 🤖 message on stderr. `scripts/release-notes.sh <prev-tag-or-empty> <tag>` prints one note per line. `scripts/release-bundle.sh <version> <bin-dir> <out-dir>` writes `olly-<version>-x86_64.tar.gz` and `manifest.json` into `<out-dir>`. Manifest: `{"version":"0.2.0","tarball":"olly-0.2.0-x86_64.tar.gz","sha256":"<hex>","notes":["…"],"published":"<RFC 3339 UTC>"}`. Tarball root (no leading directory): `olly`, `olly-notify`, `dist/olly.service`, `dist/olly-notify.service`, `plasmoid/package/…`, `VERSION` (the version plus newline).

- [ ] **Step 1: Write the failing tests** in `tests/release_scripts.rs`. Each test makes a temp git repo (`git init`, set `user.name`/`user.email` locally), copies the needed script and writes a minimal `Cargo.toml` and `plasmoid/package/metadata.json`:
  - `check_fails_on_tag_version_mismatch`: Cargo version `0.2.0`, tag `v0.3.0` → exit non-zero, stderr contains `0.2.0`.
  - `check_passes_on_match`: `0.2.0` / `v0.2.0` → exit 0.
  - `check_fails_when_widget_changed_without_bump`: tag `v0.1.0` exists, a later commit changes a file under `plasmoid/` but keeps metadata `Version` → `release-check.sh v0.2.0` fails; bumping `Version` makes it pass.
  - `notes_keep_feat_and_fix_only`: commits `feat(tray): mood face`, `fix: socket timeout`, `docs: readme`, `chore: bump`, a merge commit → output exactly `New: mood face\nFixed: socket timeout\n` (newest first is fine; assert the set and that `docs`/`chore`/merge lines are absent).
  - `notes_since_previous_tag_only`: a `feat:` before `v0.1.0` is absent from `release-notes.sh v0.1.0 v0.2.0`.
  - `bundle_writes_manifest_and_tarball`: fake `olly`/`olly-notify` files in a bin dir → manifest parses, `sha256` equals `sha256sum` of the tarball, `tar -tzf` lists `VERSION`, `olly`, `dist/olly.service`, `plasmoid/package/metadata.json`.
- [ ] **Step 2: Run** `cargo test -j 2 --test release_scripts -- --test-threads=2`. Expected: FAIL (scripts missing).
- [ ] **Step 3: Write the three scripts** (`set -euo pipefail`, run from repo root). `release-check.sh` reads Cargo's version with `sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1`. Previous tag: `git describe --tags --abbrev=0 --match 'v*' <tag>^ 2>/dev/null` (empty for the first release; then all commits). Notes: `git log --no-merges --format=%s <range>`, keep `^(feat|fix)(\([^)]*\))?!?: `, map `feat` → `New: `, `fix` → `Fixed: `. Bundle builds the manifest with `jq -n` and notes from `release-notes.sh`.
- [ ] **Step 4: Run** the tests again. Expected: PASS.
- [ ] **Step 5: Add `src/update_key.pub`** (exact text in Global Constraints) and `.github/workflows/release.yml`: on `push: tags: ['v*']`, `permissions: contents: write`, ubuntu-latest, `fetch-depth: 0`. Steps: `scripts/release-check.sh "$GITHUB_REF_NAME"`; `cargo test --locked`; node tray tests; build `--release --target x86_64-unknown-linux-musl` (`rustup target add`, `apt-get install -y musl-tools`); `scripts/release-bundle.sh`; `sudo apt-get install -y minisign`; sign with the secret written to a `mktemp` file from `${{ secrets.OLLY_MINISIGN_KEY }}` via env (never echoed), `minisign -S -s "$key" -m out/manifest.json -t "olly $VERSION"`, then delete the key file; `gh release create "$GITHUB_REF_NAME" out/* --title "olly $VERSION" --notes-file <notes>` with `GH_TOKEN: ${{ github.token }}`. If the musl build fails to link locally-unknown deps, the builder may switch to a glibc build on `ubuntu-22.04`; record that.
- [ ] **Step 6: Verify** `bash -n` on each script and `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/release.yml'))"` (or `ruby -ryaml`). Expected: no errors.
- [ ] **Step 7: Commit** `feat(release): tag-triggered signed release workflow`.

### Task 2: Update core (manifest, signature, staging)

**Files:**
- Create: `src/update.rs` (add `pub mod update;` to `src/lib.rs`), `tests/fixtures/update/` (test key pair, signed fixtures)
- Modify: `Cargo.toml`, `Cargo.lock` (add `minisign-verify = "0.2"`, `sha2 = "0.10"`, `semver = "1"`, `cargo update --offline -p …` style)

**Interfaces:**
- Produces (all `pub` in `olly::update`):
  - `struct Manifest { version: String, tarball: String, sha256: String, notes: Vec<String>, published: String }` (Serialize, Deserialize, Clone, Debug, PartialEq)
  - `struct UpdateInfo { version: String, notes: Vec<String>, url: String }` (Serialize, Deserialize, Clone, Debug, PartialEq) and `fn info(m: &Manifest) -> UpdateInfo`
  - `const RELEASE_KEY: &str = include_str!("update_key.pub");`
  - `fn verify_manifest(manifest: &[u8], minisig: &str, pubkey_file: &str) -> Result<Manifest, String>` — verifies the signature against the key's second line, parses JSON, checks `tarball == format!("olly-{version}-x86_64.tar.gz")` and that `version` is valid semver.
  - `fn is_newer(candidate: &str, current: &str) -> bool` (semver; false on unparsable)
  - `fn may_check(level: Level, incident_open: bool, paused: bool) -> bool`
  - `fn may_update(level: Level, incident_open: bool, frozen: usize, throttled: usize) -> Result<(), String>` — `Err` text is exactly `"I'll update once things are calm"`.
  - `trait Fetch { fn get(&self, url: &str, max_bytes: u64, timeout_secs: u64) -> Result<Vec<u8>, String>; fn get_to_file(&self, url: &str, dest: &Path, max_bytes: u64, timeout_secs: u64) -> Result<(), String>; }` and `struct Curl;` implementing it with `curl -fsSL --proto =https --proto-redir =https --max-time <t> --max-filesize <n>` (manifest/sig: 1 MiB, 30 s; tarball: 64 MiB, 300 s). `Curl` reads stdout via `Command::output`.
  - `fn check(fetch: &dyn Fetch, pubkey: &str, current: &str) -> Result<Option<Manifest>, String>` — fetches manifest + `.minisig`, verifies, returns `Some` only if newer.
  - `struct Staged { dir: PathBuf, manifest: Manifest }` and `fn stage(fetch: &dyn Fetch, pubkey: &str, current: &str, work: &Path) -> Result<Staged, String>` — re-fetches and re-verifies the manifest, requires newer, downloads the tarball to `work/<version>/bundle.tar.gz`, checks sha256 (lowercase hex), unpacks with `tar -xzf … --no-same-owner -C work/<version>/root`, checks `root/VERSION` trimmed equals `version`, checks `root/olly` and `root/olly-notify` exist. Any failure removes `work/<version>` and returns `Err`.
- Consumes: `olly::engine::Level`.

- [ ] **Step 1: Make test fixtures** with `/home/linuxbrew/.linuxbrew/bin/minisign`: `minisign -G -W -p tests/fixtures/update/test.pub -s tests/fixtures/update/test.key` and a second pair `other.pub`/`other.key` (test-only keys, fine to commit; add a `README` line saying so). Sign `good-manifest.json` (version `9.9.9`, tarball `olly-9.9.9-x86_64.tar.gz`) with `test.key`, and also with `other.key` into `good-manifest.json.other.minisig`.
- [ ] **Step 2: Write the failing tests** in `src/update.rs`:
  - `verifies_good_signature` → `Ok`, version `9.9.9`.
  - `rejects_tampered_manifest` (one byte changed) → `Err`.
  - `rejects_wrong_key` (other.minisig with test.pub) → `Err`.
  - `rejects_tarball_name_with_path` (sign a manifest whose tarball is `../evil.tar.gz` at test time is not possible without the key in tests — so commit a second fixture `bad-name-manifest.json` signed with `test.key`) → `Err`.
  - `is_newer`: `("0.2.0","0.1.0")` true, `("0.1.0","0.1.0")` false, `("0.1.0","0.2.0")` false, `("0.10.0","0.9.0")` true, `("junk","0.1.0")` false.
  - `may_check`: true only for `(Calm,false,false)`.
  - `may_update`: `Ok` for `(Calm,false,0,0)`; `Err("I'll update once things are calm")` for `Watch`, for an open incident, for `frozen=1`, for `throttled=1`.
  - `stage_*` with a `FakeFetch` (map url → bytes; tarball built in the test with `tar -czf`, manifest signed by running `minisign -S -s tests/fixtures/update/test.key` in the test if the binary exists at that path, else the test returns early with an `eprintln!` skip — CI has no minisign, so CI covers verification through the static fixtures above): good bundle → `Ok`, `root/olly` exists; sha mismatch → `Err` and `work/<version>` gone; `VERSION` mismatch → `Err`; not newer → `Err`.
- [ ] **Step 3: Run** `cargo test -j 2 --lib update -- --test-threads=2`. Expected: FAIL.
- [ ] **Step 4: Implement** the interface above.
- [ ] **Step 5: Run** the tests again, then `cargo build --offline`. Expected: PASS.
- [ ] **Step 6: Commit** `feat(update): signed manifest checks and staging`.

### Task 3: Update supervisor (swap, health check, rollback)

**Files:**
- Create: `src/supervise.rs` (add `pub mod supervise;`)
- Modify: `src/main.rs` (hidden subcommand `olly update-supervise <staged-root> <from> <to>`, not in USAGE)

**Interfaces:**
- Produces:
  - `struct LastUpdate { from: String, to: String, ok: bool, at_wall_ms: u64, message: String, widget_changed: bool }` (Serialize, Deserialize, Clone, Debug, PartialEq) — the content of `update-result.json` and of `StatusData.last_update`.
  - `struct Layout { bin: PathBuf, unit_dir: PathBuf, share: PathBuf, state: PathBuf }` with `Layout::system()` = `/usr/local/bin`, `/etc/systemd/system`, `/usr/local/share/olly`, `/var/lib/olly`; `fn result_path(&self) -> PathBuf` = `state/update-result.json`.
  - `trait Host { fn daemon_reload(&mut self) -> Result<(), String>; fn restart_olly(&mut self) -> Result<(), String>; fn status_version(&mut self) -> Option<String>; fn notify_finished(&mut self); fn sleep_ms(&mut self, ms: u64); fn wall_ms(&mut self) -> u64; }` and `struct SystemHost` (`systemctl daemon-reload`, `systemctl restart olly`, a status request over `/run/olly/olly.sock` reading `data.version`, sending `{"cmd":"update_finished"}`).
  - `fn supervise(layout: &Layout, staged_root: &Path, from: &str, to: &str, host: &mut dyn Host) -> LastUpdate` — never panics.
- Behaviour (in order): back up `olly`, `olly-notify` → `<name>.prev` (copy), `olly.service` → `.prev`, `share/plasmoid` → `share/plasmoid.prev` (rename), `share/olly-notify.service` → `.prev`; install new files by copying to a `.new` sibling then `rename` (mode 0755 binaries, 0644 units); `widget_changed` = staged `plasmoid/package/metadata.json` `KPlugin.Version` differs from the old copy's (true if the old copy is missing); `daemon_reload` if `olly.service` content changed; `restart_olly`; poll `status_version` every 500 ms for up to 30 000 ms until it equals `to`. Success: remove the `.prev` backups, message `"updated to <to>! ✨"`. Failure (any step after the first backup, or health timeout): restore every backup that was made, `daemon_reload` if the unit was swapped, `restart_olly`, message `"that update didn't work, so I went back to <from>."`. Always: write the result JSON (atomic write via temp + rename) and `notify_finished`.

- [ ] **Step 1: Write the failing tests** in `src/supervise.rs` with a temp `Layout` populated with old files (content `"old-olly"`, etc.) and a `FakeHost` that records calls and returns a scripted `status_version` sequence:
  - `success_swaps_and_cleans_up`: version `Some("0.2.0")` on the 3rd poll → `ok`, `bin/olly` holds the new content, no `.prev` left, result file parses to the same `LastUpdate`, `notify_finished` called once.
  - `health_timeout_rolls_back_everything`: always `Some("0.1.0")` → `!ok`, every file and the plasmoid dir hold the old content, `restart_olly` called twice, message is the rollback copy, elapsed fake time ≥ 30 000 ms.
  - `restart_failure_rolls_back`: `restart_olly` fails the first time.
  - `unit_change_triggers_daemon_reload`: changed `olly.service` → `daemon_reload` called; unchanged → not called.
  - `widget_changed_follows_metadata_version`.
- [ ] **Step 2: Run** `cargo test -j 2 --lib supervise -- --test-threads=2`. Expected: FAIL.
- [ ] **Step 3: Implement** `supervise`, `SystemHost` and the hidden subcommand (it calls `supervise(&Layout::system(), …, &mut SystemHost)` and exits 0 on ok, 1 otherwise). Add a `parse_cli` test that `update-supervise /x 0.1.0 0.2.0` parses and that USAGE doesn't mention it.
- [ ] **Step 4: Run** the tests again. Expected: PASS.
- [ ] **Step 5: Commit** `feat(update): supervisor with health check and rollback`.

### Task 4: Daemon, IPC and status

**Files:**
- Modify: `src/ipc.rs`, `src/engine.rs` (Event variant only), `src/daemon.rs`, `src/config.rs`, `dist/config.toml`, `docs/ipc.md`, `src/incident.rs`/`src/main.rs`/`src/bin/olly-notify.rs` only where a new `Event` variant breaks an exhaustive match

**Interfaces:**
- Consumes: Task 2 `check`, `stage`, `may_check`, `may_update`, `info`, `UpdateInfo`, `Curl`, `RELEASE_KEY`; Task 3 `LastUpdate`, `Layout`.
- Produces:
  - `Request::UpdateCheck` (`{"cmd":"update_check"}`), `Request::Update` (`{"cmd":"update"}`), `Request::UpdateFinished` (`{"cmd":"update_finished"}`).
  - `StatusData` new fields: `version: String` (`env!("CARGO_PKG_VERSION")`), `update: Option<UpdateInfo>`, `update_state: UpdateState`, `last_update: Option<LastUpdate>`, `widget_version: Option<String>` (from `share/plasmoid/metadata.json`, read at start and on `update_finished`).
  - `enum UpdateState { Idle, Checking, Downloading, Installing }` in `olly::update`, serde `rename_all = "snake_case"`, `Default` = `Idle`.
  - `Event::Updated { from: String, to: String, ok: bool, message: String, widget_changed: bool }`.
  - `Config.update_check: bool` (default true).
- Behaviour:
  - Timer: `next_update_check` monotonic ms = start + 300 000, then + 21 600 000 after each attempt (skipped or not); included in the poll timeout. When due, `update_check` is true, `may_check` passes and no worker runs → spawn a check worker (`thread::Builder` with a 256 KiB stack, like Claude's runner) sending its result over an mpsc channel; `pump_update()` (called next to `pump_claude()`) reads it with `try_recv`.
  - Check result: `Ok(Some(m))` → `update = Some(info(&m))`; `Ok(None)` → `None`; `Err` → keep the previous value, log once via `RateLog` (a bad signature logged as `warning:`). Answer every waiting `update_check` reply sender with `ok_line(&update)`.
  - `update_check` request: if a check runs, park the reply sender; otherwise start one now (on demand ignores `may_check` and the config flag) and park it. `ipc.rs` waits up to 60 s for this request's reply.
  - `update` request: `Err("there's no update to install right now")` if none known; `Err("I'm already updating")` if state isn't `Idle`; `may_update` error passed through; else spawn a stage worker (`work = state_dir/update`), state `Downloading`, reply `ok_line(&"started")`. When staging finishes: re-check `may_update`; if it now fails or staging failed → state `Idle`, log, keep `update` offered. Else copy `/proc/self/exe` to `work/olly-supervisor` (0700), state `Installing`, run `systemd-run --unit=olly-update-<to> --collect --quiet <work>/olly-supervisor update-supervise <root> <from> <to>`; a spawn failure → `Idle` and log. Under `OLLY_RUN_DIR` (tests) the launcher is a no-op that logs, so tests never call systemd-run.
  - Results: on start and on `UpdateFinished`, read `state_dir/update-result.json` into `last_update`. If it is less than 10 min old and its `at_wall_ms` differs from the last one emitted, broadcast `Event::Updated` and push it to `pending`. A successful result clears `update` if its version is `<=` the running version.
- [ ] **Step 1: Write the failing tests**: in `ipc.rs` `parse_all_requests` gains the three requests; `old_status_without_new_fields_parses` still passes (version defaults to `""`, update_state to `Idle`); in `daemon.rs` tests (using the existing test daemon builder at ~line 1065): `update_refused_when_frozen`, `update_refused_when_nothing_known`, `second_update_refused_while_downloading`, `stage_result_rechecks_rules` (state turns non-calm between start and result → back to `Idle`, `update` still `Some`), `check_timer_skips_when_not_calm`, `fresh_result_emits_updated_once`, `old_result_loaded_but_not_emitted`, `status_reports_version_and_update`. Make the fetcher injectable (`Box<dyn Fetch + Send>` factory on `Daemon`) so tests use a fake. Config: `defaults_match_spec` asserts `update_check` true; `shipped_config_matches_defaults` stays green after adding `update_check = true` with a comment to `dist/config.toml`.
- [ ] **Step 2: Run** `cargo test -j 2 -- --test-threads=2`. Expected: the new tests FAIL.
- [ ] **Step 3: Implement** the behaviour above.
- [ ] **Step 4: Run** the full suite. Expected: PASS.
- [ ] **Step 5: Document** the three requests, five fields, `UpdateState` values, `LastUpdate` shape and the `Updated` event in `docs/ipc.md`.
- [ ] **Step 6: Commit** `feat(update): daemon checks, stages and hands off updates`.

### Task 5: CLI

**Files:**
- Modify: `src/main.rs`, `tests/cli_json.rs` if it has a fake-daemon harness worth reusing

**Interfaces:**
- Consumes: Task 4 requests and status fields.
- Produces: `olly update --check`, `olly update`, `olly update --no-wait`; USAGE lines `olly update [--check]             install a new version, or just look for one`.
- Behaviour:
  - `--check`: sends `update_check` with a 70 s read timeout. `null` → `🤖 you're on the newest olly (<version>).` Otherwise `🤖 olly <v> is ready!` then each note as `  • <note>`, then `Run \`olly update\` to install it.` Exit 0 either way.
  - `update`: sends `update`; an error reply prints `🤖 <error>` and exits 1. With `--no-wait` it prints `🤖 updating…` and exits 0. Otherwise it prints `🤖 updating…`, then polls `status` every 1 s (reconnecting; connection failures during the restart are expected and silent) for up to 5 min until `last_update.at_wall_ms` is newer than when it started; prints `🤖 <message>` and exits 0 if `ok`, else 1. Timeout → `🤖 I lost track of the update; check \`olly status\`.`, exit 1.
  - `render_status` adds one line when `update` is set: `olly <v> is ready! Run \`olly update\` to install it.`
  - `render_event` shows `Updated` as `<message>`.
- [ ] **Step 1: Write the failing tests**: `parse_cli` cases for `update`, `update --check`, `update --no-wait`, `update --bogus` (friendly error); `render_status` with and without `update`; rendering the `--check` text for `null` and for an `UpdateInfo` with two notes; `render_event` for `Updated`.
- [ ] **Step 2: Run** `cargo test -j 2 --bin olly -- --test-threads=2`. Expected: FAIL.
- [ ] **Step 3: Implement.** Keep the update flow in its own function (`fn update_cmd(mode) -> !`) rather than growing `client`.
- [ ] **Step 4: Run** the full suite. Expected: PASS.
- [ ] **Step 5: Commit** `feat(cli): olly update and update --check`.

### Task 6: olly-notify user side

**Files:**
- Modify: `src/bin/olly-notify.rs`

**Interfaces:**
- Consumes: `Event::Updated`, `/usr/local/share/olly/plasmoid/metadata.json`, `/usr/local/share/olly/olly-notify.service`.
- Produces:
  - `struct UserPaths { share: PathBuf, plasmoid_installed: PathBuf, user_unit: PathBuf }`, `UserPaths::from_home(home)` = `/usr/local/share/olly`, `<home>/.local/share/plasma/plasmoids/org.olly.tray`, `<home>/.config/systemd/user/olly-notify.service`.
  - `trait Run { fn run(&mut self, prog: &str, args: &[&str]) -> bool; }` (real impl uses `Command::status`).
  - `fn sync_user_side(p: &UserPaths, run: &mut dyn Run) -> SyncOutcome` with `struct SyncOutcome { widget_upgraded: bool, unit_updated: bool }`:
    - widget: only if the installed widget exists and its `KPlugin.Version` differs from the shared copy's → `kpackagetool6 --type Plasma/Applet --upgrade <share>/plasmoid`. A widget that was never installed is left alone.
    - unit: if the shared unit exists and differs from the user's → copy it, then `systemctl --user daemon-reload`.
  - `fn should_restart(ev: &Event, own_version: &str) -> bool`: true only for `Updated { ok: true, to, .. }` with `to != own_version`.
- Behaviour: call `sync_user_side` once at start (before the subscribe loop) and on every `Updated { ok: true }`; then if `should_restart`, log and `std::process::exit(0)` (systemd restarts it on the new binary). Failures are logged once and never stop notifications. `Updated` shows no desktop notification.
- [ ] **Step 1: Write the failing tests** with temp dirs and a recording fake `Run`: widget upgraded when versions differ; not when equal; not when not installed; unit copied and daemon-reload run when different; nothing when same; `should_restart` true for ok+different, false for ok+same, false for `ok: false`; `map_event` returns `None` for `Updated`.
- [ ] **Step 2: Run** `cargo test -j 2 --bin olly-notify -- --test-threads=2`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the full suite. Expected: PASS.
- [ ] **Step 5: Commit** `feat(notify): update the widget and user unit after an update`.

### Task 7: Tray face and update card

**Files:**
- Create: `plasmoid/package/contents/ui/UpdateCard.qml`
- Modify: `plasmoid/package/contents/ui/logic.js`, `Face.qml`, `Popup.qml`, `main.qml`, `plasmoid/package/metadata.json` (`Version` → `0.2.0`, since the widget changes), `tests/tray/logic.test.mjs`

**Interfaces:**
- Consumes: status fields `version`, `update`, `update_state`, `last_update`, `widget_version`; `Plasmoid.metaData.version` (the running widget's version).
- Produces in `logic.js`:
  - `hasUpdate(status)` → true when `status && status.update`.
  - `updateCard(status, dismissedAt, now, ownVersion)` → `null` or `{kind, title, notes, more, url, hint, button}`:
    - `update_state` `downloading`/`installing` → `{kind:"updating", title:"updating…"}`.
    - `last_update` with `at_wall_ms > dismissedAt` and within the last hour → `ok`: `{kind:"done", title:"updated to <to>! ✨", hint}` where `hint` is `"log out and back in to see my new look"` when `status.widget_version` is set and differs from `ownVersion`, else `""`; not ok: `{kind:"rolledback", title:"that update didn't work, so I went back to <from>."}`.
    - `update` set → `{kind:"ready", title:"olly <v> is ready!", notes: first 5, more: count beyond 5, url, button:"Update"}`.
    - Otherwise `null`. Precedence: updating → done/rolledback → ready.
  - `command("update")` → `OLLY + " update --no-wait"`.
- Behaviour:
  - `Face.qml`: `property bool updateReady`; when true, a `#4fc3f7` circle covers the antenna tip (viewBox cx 12, cy 2.6, r 1.2, scaled like the "?"), in every mood. `main.qml` passes `Logic.hasUpdate(status)` to both faces.
  - `Popup.qml`: `UpdateCard` sits after the needs-you cards and before the stats. "and N more" is a link that opens `url` (`Qt.openUrlExternally`). The Update button runs `app.run("update")`; its error (e.g. "I'll update once things are calm") shows inline on the card, using the existing `actionError` with target `"update"`.
  - `main.qml`: `property double updateDismissedAt: 0`; when the popup closes, set it to `status.last_update.at_wall_ms` if present.
- [ ] **Step 1: Write the failing node tests** for `hasUpdate`, `updateCard` (each kind, precedence, notes capped at 5 with `more`, hint present only when versions differ, dismissed and older-than-1h results hidden) and `command("update")`.
- [ ] **Step 2: Run** `node --test tests/tray/*.test.mjs`. Expected: FAIL.
- [ ] **Step 3: Implement** `logic.js`, then the QML.
- [ ] **Step 4: Run** the node tests (PASS), then `scripts/check-plasmoid.sh` (PASS; it uses a temp packageroot only).
- [ ] **Step 5: Commit** `feat(tray): update card and antenna light`.

### Task 8: Install, uninstall, docs

**Files:**
- Modify: `scripts/install.sh`, `scripts/uninstall.sh`, `tests/dist_files.rs`, `README.md`, `.github/workflows/ci.yml` (none needed unless a script test needs `jq`; ubuntu has it)

**Interfaces:**
- Consumes: install paths from Global Constraints.
- Behaviour:
  - `install.sh` adds: `sudo install -d /usr/local/share/olly`, `sudo rm -rf /usr/local/share/olly/plasmoid && sudo cp -r plasmoid/package /usr/local/share/olly/plasmoid`, `sudo install -Dm644 dist/olly-notify.service /usr/local/share/olly/olly-notify.service`.
  - `uninstall.sh` removes `/usr/local/share/olly`, `/var/lib/olly/update` and `/var/lib/olly/update-result.json`.
  - README: a "Releases and updates" section: how to cut a release (bump `Cargo.toml`, bump the widget version if `plasmoid/` changed, commit, `git tag vX.Y.Z && git push --tags`), how updates appear and install, `update_check = false`, key location and backup advice (`~/.config/olly-release/olly-update.key`), key rotation (one release signed with the old key carrying the new public key), and key loss/leak (reinstall once with `install.sh` / rotate at once).
- [ ] **Step 1: Write the failing tests** in `tests/dist_files.rs`: `install_ships_update_copies` (install.sh has the three lines above), `uninstall_removes_update_files`, `widget_version_is_semver` (metadata `KPlugin.Version` parses as `x.y.z`), `release_key_matches_known_id` (`src/update_key.pub` first line ends with `FEE7FE12633D6160`).
- [ ] **Step 2: Run** `cargo test -j 2 --test dist_files -- --test-threads=2`. Expected: FAIL.
- [ ] **Step 3: Implement** the script and README changes. Do not run either script.
- [ ] **Step 4: Run** the full Rust suite and node tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(update): install update copies and document releases`.

---

After Task 8: bump `Cargo.toml` to `0.2.0` only when the user asks to release; the first tag push is theirs.
