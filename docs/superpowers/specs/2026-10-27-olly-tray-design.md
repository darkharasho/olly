# olly tray face — design

Date: 2026-10-27. Status: draft for review. Builds on `2026-10-27-olly-design.md` and `docs/ipc.md`.

## Intent

olly is meant to feel like a friendly little robot watching your computer. Today you only see it when it sends a notification. The tray face gives olly a constant, glanceable presence: a small robot face in the KDE system tray that changes mood with what olly is doing, and a popup with status and recent incidents when you click it.

Success is when you glance at the panel and know at once whether your computer is fine, olly is busy, or olly needs you, and when the face makes olly feel like a character rather than a tool.

## Goals

- A tray face in style B ("screen face"): a dark rounded screen with an antenna, and glowing eyes whose colour and shape carry the mood.
- Seven moods driven by olly's real state, updating within about 2 seconds.
- A hover tooltip with olly's current one-line status, in olly's voice.
- A click popup showing mood, memory stats, the biggest app, the last few incidents, and quick actions.
- While an app is frozen, the popup puts the Kill/Resume choice at the top with olly's countdown.
- It looks native on KDE Plasma 6 in both Breeze dark and Breeze light.

## Non-goals

- GNOME or other desktops. KDE Plasma 6 only, as with the rest of olly.
- Settings or threshold editing from the widget.
- Removing allowlist entries or killing arbitrary pids. IPC deliberately can't do either.
- A Claude "diagnose now" button. There's no IPC command for it today.
- Charts or history graphs. olly is not a monitoring dashboard.

## Anti-goals (a failure even if it technically works)

- The widget costs olly safety. It must never hold olly's IPC connection slots for long (8 max) or add work to the daemon's emergency path.
- It becomes noisy. No error popups or log spam when olly is offline: the face just shows Offline.
- It looks like a generic system monitor. Copy is in olly's cheery voice, and the face is a character.
- It needs root to install, or a reboot.

## The face

Each mood has a fixed eye colour (not themed). The screen is always dark (`#14171a`). The frame and antenna use the Plasma theme's text colour at reduced opacity, so they read on both light and dark panels. The approved mockups are in `docs/superpowers/specs/2026-10-27-olly-tray-mockup.html`; this spec uses the style B row. The table below is binding for colours and meaning, and the mockup is binding for the look.

| Mood | Eyes | Colour | When |
|---|---|---|---|
| All good | happy upward arcs (^ ^) | green `#3ddc84` | level `calm` and nothing below applies |
| Watching | tall rounded rects, upright | blue `#4fc3f7` | level `watch` |
| Worried | rounded rects, lids drooping toward the outside | amber `#ffb74d` | level `warning`, `critical` or `emergency` (the On it! rule already took any case where olly acted) |
| On it! | rounded rects, lids slanting down toward the middle (determined) | red `#ff5252` | `incident_active` and olly has acted in it, or `last_action_wall_ms` is within the last 60 s. Any action counts, including throttles and dry-run "would" actions (my call: during a dry-run trial you can see olly would have stepped in; the popup's dry-run badge says it didn't really) |
| Needs you | one short eye, one tall eye, pulsing "?" | orange `#ffa726` | at least one app is frozen |
| Napping | flat lines, small "z" | purple `#9575cd` | olly is paused |
| Offline | three dim dots | grey `#6b7378` | the daemon can't be reached |

Precedence, first match wins: Offline → Needs you → On it! → Napping → Worried → Watching → All good. (My call: a frozen app waiting on you matters more than anything else, and the "On it!" linger means you notice olly acted even if you missed the notification.)

Motion (my call): the "?" in Needs you pulses. In All good, the eyes blink briefly every few seconds. Both are skipped when Plasma's animation speed is set to instant.

## Tooltip

Hovering shows `olly` as the title and `StatusData.message` as the text, e.g. "olly: all good! 41% RAM free, nothing growing. Watching quietly." When offline: "olly isn't running right now."

## Popup

The approved mockup is in `docs/superpowers/specs/2026-10-27-olly-tray-mockup.html`: two popups, calm and mid-freeze. From top to bottom:

1. **Header:** a large version of the current face, then `olly`, the mood name in the mood colour, and a one-line cheery remark (the daemon's `message`). A small "dry run" badge shows when `dry_run` is true.
2. **Needs-you card** (only while something is frozen): one card per frozen app, e.g. "chrome is frozen at 7.4 GB (it was growing 2.1 GB/min). I'll close it in 4:12 if you don't pick, sooner if memory gets really tight." The countdown is to `kill_at_wall_ms`, the no-answer deadline. The engine can kill earlier (when the 10 s grace check finds pressure no better, or at emergency), which is why the card says "sooner if memory gets really tight" and doesn't promise the full time. It has two buttons, **Close it** (sends `frozen_kill`) and **Let it run** (sends `frozen_resume`). The countdown ticks locally every second from the deadline the daemon reports.
3. **Stats:**
   - RAM free, as a bar and GB.
   - Swap used, as a bar and GB.
   - Pressure (PSI `some avg10`), as a bar and %.
   - The biggest app by bytes (RAM + swap), with its name and size.
   - The bar colour follows severity, with fixed thresholds (my call):
     - RAM free: green above 20 %, amber 10–20 %, red below 10 %.
     - Swap used: green below 50 %, amber 50–80 %, red above 80 %.
     - Pressure: green below 10 %, amber 10–30 %, red above 30 %.
4. **Recent incidents:** the last 3. Each shows:
   - a coloured dot (red for a kill, orange for a freeze, amber for a throttle only),
   - a one-line summary,
   - a sub-line with the peak level and, if present, Claude's diagnosis cut to one line,
   - a relative time.

   Clicking one expands it in place to show the full summary, the list of actions with times, and the full diagnosis.
5. **Footer:**
   - **Pause 30 min** (sends `pause` for 1800 s). It becomes **Wake olly up** (`resume`) while paused.
   - **All incidents…** grows the list to the last 20 inside the popup, scrollable.

Errors from commands (e.g. "already resumed") show as a short inline line under the button, never as a dialog. The text is the CLI's stderr, which is already in olly's voice; the leading 🤖 is stripped. Success is exit code 0. After any button command, the widget polls status right away instead of waiting for the next 2 s tick.

## How it's built (my calls)

- **A Plasma 6 widget** (plasmoid, QML) that lives in the system tray, like the volume and network popups. A StatusNotifierItem icon can show a menu or send an activate signal, but not a native Plasma popup like this one. The package ID is `org.olly.tray`. Its metadata sets `X-Plasma-NotificationArea: true` and `X-Plasma-NotificationAreaCategory: SystemServices`, so it shows up as a system tray entry.
- **Data source: the `olly` CLI, not the socket directly.** QML can't open unix sockets without a compiled plugin, and a compiled plugin is painful to build on an immutable Bazzite host. The widget runs CLI commands through Plasma's executable data source (`Plasma5Support.DataSource`, engine `executable`). It uses the absolute path `/usr/local/bin/olly`, where `install.sh` puts it, so it doesn't depend on Plasma's PATH:
  - `olly status --json` every 2 s.
  - `olly incidents -n 20 --json` when the popup opens and when the status shows a new last-incident id.
  - `olly kill <id>`, `olly unfreeze <id>`, `olly pause 30` and `olly resume` for the buttons.

  Each call is one short connection, so no slots are held. Polling every 2 s is cheap: a status call is one small JSON line.
- **CLI changes:** add a `--json` flag to `status` and `incidents`. It prints the daemon's raw `data` value as one JSON line. If the daemon is unreachable or doesn't answer within 1 s (a socket timeout in the CLI, used only in `--json` mode), it prints `{"offline":true}` and exits 1. The widget treats that, any other failure, or output that doesn't parse, as Offline. If the CLI itself hangs, the widget does nothing special: the next poll is skipped while one is still running.
- **`StatusData` additions.** These are new fields only; existing ones are unchanged, so otto keeps working. Each new field is `#[serde(default)]` so a newer CLI still reads an older daemon's reply, and the widget treats a missing field as null or false. Document them in `docs/ipc.md`.
  - `swap_used`, `swap_total` (bytes).
  - `some_avg10` (the PSI `some avg10` percentage).
  - `top`: `{name, bytes, rate}` of the biggest app by bytes in the latest sample, or null.
  - `incident_active` (bool) and `last_action_wall_ms` (wall ms of olly's most recent action of any kind, dry run included, or null). Used for "On it!".
  - `last_incident_id` (or null). Used to refresh the incident list.
  - `frozen_info`: one entry per frozen target, `{id, name, bytes, rate, kill_at_wall_ms}`. Used for the needs-you card and countdown.
    - `rate` is the growth rate at freeze time; the engine stores it on the frozen entry, since a frozen app doesn't grow.
    - `kill_at_wall_ms` is the no-answer deadline: freeze time plus `freeze_giveup_secs`. The engine exposes the freeze time through a read-only accessor, and the daemon converts it to wall time.
- **Icons:** seven SVGs (one per mood), plus the QML to swap and animate them. The frame uses a theme colour (via the QML layer, or Plasma's `ColorScheme-Text` SVG class), so it adapts to light and dark.
- **Install:** `scripts/install.sh` gains a step that installs or upgrades the widget for the current user with `kpackagetool6` (no root). `uninstall.sh` removes it. It must be run from the user's desktop session, which is how `install.sh` already runs. Plasma may only pick up a new widget after `plasmashell` restarts or you log out and in; the script says so and doesn't restart plasmashell itself. The README says how to make it visible if Plasma hides it: System Tray Settings → Entries → olly → Always shown.
- **olly-notify is unchanged.** Notifications still come from it, and the widget is extra.

## Left to the builder

- QML structure and component split. Use Plasma/Kirigami components so it looks native.
- Exact eye geometry, as long as it matches the approved style B mockup and reads at 16–22 px.
- How the CLI `--json` output is wired internally.
- Relative-time wording ("2h ago", "yesterday", "Mon").
- Tests:
  - Rust tests for the new `StatusData` fields and the CLI `--json` output.
  - The mood-precedence function written as a small pure JS/QML function, with a test if a QML test runner is practical. Otherwise a table-driven check script.
