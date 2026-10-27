# Five additions: notification buttons, swap warning, game mode, leak heads-up, ask Claude why

The user picked these from a list and asked for defaults without questions. Every
behavior below is the controller's call, made to fit olly's goals: act locally and
deterministically in an emergency, stay tiny, tell the user plainly, Claude only on demand.

## Global constraints

- No new crates. Builds are offline.
- Tests run with low parallelism. Use `cargo test -j 2 -- --test-threads=2` and `node --test tests/tray/*.test.mjs`.
- Do not run `cargo fmt` repo-wide. Only format the files you touched, if you format at all.
- Never run sudo, install.sh, systemctl, the daemon as root, or a real kpackagetool6 install. Never restart plasmashell. A temporary packageroot via `scripts/check-plasmoid.sh` is fine.
- Voice follows `src/voice.rs`. It is plain and friendly, and speaks as "I" ("I paused it", "I'll look"). "olly" is lower-case in titles. Status-style lines start with the 🤖 prefix.
- New status fields are additive and optional, so an older widget or CLI keeps working. Document them in `docs/ipc.md`.
- The widget logic goes in `logic.js`, with node tests in `tests/tray/logic.test.mjs`. The QML only binds to it.
- Each feature is one commit. Commit messages use `feat(<area>): ...` and end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## 1. Notification buttons

- **Freeze notification.** It keeps Kill and Resume. It gains **Never touch this**, which adds the app's name to the allowlist (`allowlist_add`), then resumes it (`frozen_resume`).
- **Throttle and kill notifications.** They gain **Never touch this** (allowlist add).
  - The daemon lifts throttles from allowlisted apps.
  - The daemon must lift an existing throttle on, or thaw an existing freeze of, an app as soon as it becomes allowlisted. If it doesn't do that today, add it.
- **Dry runs.** Dry-run notifications get no buttons, as now.
- **The name.** The name sent is the one the allowlist matches: the app's process name, as in `olly allow <name>`. Carry it in the notification's state, not by re-querying.
- **Confirmation.** When the click succeeds, show a short notification: "Got it, I'll leave {app} alone from now on."

## 2. Swap warning

- **The calculation.** The daemon decides it and status shows it.
  - It is on when swap used is at least 50% of swap total and grew by at least 1 GiB over the last 15 minutes.
  - It goes off when usage drops below 40%, or when there has been no growth of 1 GiB over the last 15 minutes.
  - Use the samples olly already takes, including slow calm ones. With no swap (total 0) it's never on.
- **Status field.** Add `swap_warning: bool`.
- **Widget.** Show a soft card under the stat bars: "Swap is filling up: {used} of {total} and climbing. Things may get sluggish. Closing an app you don't need would help."
- **No notification and no action.** It is information only.

## 3. Game mode

- **What counts as a game.**
  - Any app sampled as a game: the Steam game that sample.rs already splits from the Steam client by lineage.
  - Any app whose exe names include wine/proton loaders: `wine64-preloader`, `wine-preloader`, `wineserver`, `proton`.
  - Any app running under `gamescope`.
  - Reuse what sample.rs knows. Add an `is_game` flag on the app sample.
- **Behavior while a game is running.** Config `game_mode = true` is the default. Add it to `dist/config.toml` with a comment.
  - Non-game apps are always chosen first, at every level. That covers throttle, freeze and the last-resort kill ordering.
  - A game is never throttled, because throttling a game makes it stutter.
  - A game can still be frozen or killed, but only when no non-game candidate can act at that level and the game is itself the problem. Being the problem means it is growing, or it is the last-resort kill at emergency. This keeps olly's job of preventing a lockup.
  - Thresholds don't change.
- **Status field.** Add `game: string|null`, the running game's display name, or the largest one if there are several.
- **Widget.** Show a quiet line "🎮 {game} is running. I'll pick other apps first." The face is unchanged.

## 4. Slow-leak heads-up

- **Detection.** Leak detection (`LeakWatch`) stays as is.
- **Status field.** The daemon keeps the current suspects in status as `leaks: [{name, growth_bytes, window_secs}]`, largest first, at most 3.
  - An app stays listed while `LeakWatch` still flags it.
  - An app is dropped if it is no longer sampled.
- **Notification.**
  - A plain notification goes out once per app per 24 h, using the existing `leak_seen` cooldown: "{app} has grown {growth} in the last {hours} hours. Restarting it when convenient would free that up."
  - It goes out whether Claude is enabled or not.
  - The Claude hunch still follows when Claude is enabled, as now.
- **Widget.**
  - A soft card per leak with the same text and a **Dismiss** button.
  - Dismissals live in widget memory, keyed by name, and last until the app stops being a suspect.
  - Nothing is ever acted on.

## 5. Ask Claude why

- **IPC.** Add `Request::Diagnose{id}`.
  - **If the incident has no diagnosis:** the daemon enqueues `Job::Diagnose` for it. The existing rules still apply: daily limit, waits for calm, queue cap.
  - **If the incident already has a diagnosis:** it is re-asked.
  - **Replies:**
    - Accepted: "I'll ask Claude once things are calm."
    - Claude off: "Claude is turned off in my config."
    - Daily limit hit: "I've asked Claude {n} times today already. I'll be able to again tomorrow."
    - Unknown id: an error.
    - Already queued or running for that id: "Already on it."
- **Status.** Add `diagnosing: number|null`, the incident id being diagnosed or queued, if any.
- **CLI.** Add `olly diagnose <id>`.
- **Widget.**
  - An expanded IncidentRow gets **Ask Claude why**.
  - The button shows only when the incident has no diagnosis, and reads "Claude is looking…" when that id is `diagnosing`.
  - The diagnosis text shows as it does today, once it arrives.
- **Notification.** When a diagnosis requested this way finishes, send a notice with the voice's hunch style. The automatic post-incident diagnosis keeps whatever it does today.
