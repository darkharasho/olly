# olly — design

Date: 2026-10-27
Status: draft for review

## Why

On this Bazzite desktop (KDE Plasma on Wayland, 30 GB RAM, 22.9 GB zstd zram swap, NVIDIA), runaway memory regularly locks the machine until a hard restart. Evidence from the journal:

- Oct 18: the kernel OOM killer finally killed `game.exe` at 21.5 GB anon RSS. systemd-oomd, which is running, never acted. Its limits are 80% memory pressure sustained for 20s on `user@1000.service`, which is too lax and too slow.
- The large zram swap is why "freeze" happens before "kill". A runaway process gets compressed into zram and the system thrashes for minutes before the kernel gives up.
- Oct 26: an `electron` process was killed three times at about 6 GB by a *cgroup* memory limit. That is a self-imposed limit working as intended, not system-wide pressure. olly should log events like this but leave them alone.

olly is a small, friendly watchdog that steps in during the thrashing window, seconds after trouble starts rather than minutes, and tells the user what it did.

## Goals

- Detect runaway memory early, using kernel PSI plus per-app memory growth (RAM **plus swap**).
- Act within seconds using deterministic local rules. The ladder is throttle, then freeze, then kill. The emergency path never touches the network or an LLM.
- Never touch anything on the never-kill allowlist.
- Notify the desktop whenever olly acts.
- Keep an incident log.
- Use Claude on demand, only for post-incident diagnosis and slow-leak judgment. Claude suggests and never acts. Zero cost while idle.
- Expose a local IPC socket for otto and for olly's own CLI.
- Keep a tiny, steady footprint, and make sure olly itself survives extreme pressure.
- Have a cheerful, friendly-little-robot personality (see "Voice").

## Scope

v1 targets the **KDE Plasma desktop session only**. Bazzite's Game Mode (the gamescope session) is out of scope for v1: it has a different cgroup layout, notifications may not show over the game, and games are children of gamescope. Behaviour there is undefined until a later version.

## Non-goals and anti-goals

- No shared code with otto, and no dependency on otto.
- No LLM call in the emergency path.
- No dashboards and no general metrics collection.
- **Anti-goal: killing a big but stable app.** A game sitting steady at 20 GB is fine. Targets are chosen by growth and pressure, not by size alone.
- **Anti-goal: olly itself getting swapped out or OOM-killed** during the exact moment it's needed.
- **Anti-goal: cuteness that hides urgency.** In an emergency the message must say plainly what happened and what the user can do.
- **Anti-goal: olly fighting systemd-oomd or the kernel.** It runs alongside them and acts earlier.

## Decisions made

| Topic | Decision |
|---|---|
| Language | Rust, single daemon binary plus a small `olly` CLI (can be the same binary with subcommands) |
| Service type | **Root system service** (`/etc/systemd/system/olly.service`), so it can set `OOMScoreAdjust=-1000`, `mlockall`, see every process, and freeze or kill any cgroup |
| systemd-oomd | **Run alongside it**, leaving it untouched as the backstop. olly's thresholds are well below oomd's |
| Emergency behaviour | **Freeze and notify, then auto-kill** after a grace period (default 10s) if pressure is still high or rising. Kill immediately at the emergency level |
| Claude | **Suggest only.** It never kills, never edits the allowlist, and never changes config |
| IPC | Unix socket with newline-delimited JSON. Access limited to uid 1000 (the desktop user, configurable) via `SO_PEERCRED` |
| Desktop | KDE Plasma (kwin_wayland) |
| Personality | Lighthearted, cheery, friendly helpful little robot |

## How it works

### Detection

- **Event-driven wake-up, no busy polling.** Register a PSI trigger on `/proc/pressure/memory` (for example `some 150000 1000000`, meaning 150ms of stall per 1s window) and `poll()` for `POLLPRI`. While idle, olly does almost nothing.
- **Active mode.** Once triggered, olly samples roughly every 1s until pressure has been calm for a while. Each sample includes:
  - system: `/proc/pressure/memory` (`some`/`full` avg10), `/proc/meminfo` (`MemAvailable`), and actual zram RAM use (`/sys/block/zram0/mm_stat`). "Free swap" is misleading with zram, because zram uses RAM.
  - per app: `memory.current` + `memory.swap.current` for each leaf cgroup under `user.slice/user-1000.slice/user@1000.service/app.slice` and `session.slice`, plus per-process RSS + `VmSwap` from `/proc/<pid>/status`.
  - growth rate per app or process over a short sliding window (for example 10s).
- **Slow loop** every 30–60s, cheap: per-app memory totals only. It detects steady growth over hours (slow leaks) and feeds the Claude slow-leak check.

### Pressure levels (defaults are starting points to tune during dry-run)

| Level | Rough trigger | Action |
|---|---|---|
| calm | PSI `some` avg10 < 5% | nothing |
| watch | `some` avg10 ≥ 10% or `MemAvailable` < 15% | sample actively, no action |
| warning | `full` avg10 ≥ 10% or `MemAvailable` < 10%, *and* a target is growing | **throttle** the target (`memory.high` set just above its current usage) and notify |
| critical | `full` avg10 ≥ 25% or `MemAvailable` < 5% | **freeze** a *recently growing* target and notify with Kill / Resume (see "Frozen targets"). If nothing grew recently, take no action and keep watching |
| emergency | `full` avg10 ≥ 50% or `MemAvailable` < 2% | **kill** the top target immediately and notify. If nothing grew recently, kill the largest killable app anyway |

A throttle (`memory.high`) is lifted when the incident ends (calm for about 60s), restoring the previous value. If the app runs away again, it can be throttled again in a new incident. Active throttles appear in `status` and in the incident record. *(My call.)*

After a kill, re-check. If pressure doesn't fall, move to the next target.

### Choosing a target

- **Recently growing** means the app's RAM+swap grew meaningfully within the last few minutes (for example ≥ 500 MB or ≥ 20% over 5 min; tune during dry-run). Throttle and freeze only ever target recently growing apps, so a big but stable app is never touched below the emergency level.
- **At the emergency level only**, if nothing is recently growing, olly kills the largest killable app (RAM+swap) rather than letting the machine lock up, since a frozen machine loses everything. The notification and incident record say plainly that this was a last-resort size-based kill.
- Score each candidate by growth rate (dominant factor) and current RAM+swap (secondary). Candidates must not be on the allowlist and must be in the user's session. olly never acts on root/system processes unless they are explicitly configured as targetable.
- **Cgroup vs process.** Act on the whole app cgroup when it contains only the offender's process tree and nothing allowlisted. Otherwise act on the process and its children (`SIGSTOP`/`SIGCONT`/`SIGKILL`). This matters because Steam games run inside Steam's own scope: a runaway game must be stopped without freezing or killing the Steam client. Throttling (`memory.high`) is only possible at the cgroup level. When only per-process action is available, skip straight to freeze.
- Freezing stops growth but **does not free memory**, especially with zram. That's why freeze has a grace period that ends in a kill.
### Frozen targets

A freeze always ends in Kill or Resume. There's no endless freeze. olly never auto-resumes, because resuming a leaking app usually restarts the leak.

1. On freeze: notify with [Kill] [Resume]. A 10s grace period starts.
2. At the end of the grace period, if pressure is **not clearly falling** (`full` avg10 is not lower than when the freeze happened), olly kills the target.
3. If pressure is falling, the target stays frozen. After about 1 min with no answer, olly sends a follow-up notification ("Cyberpunk2077.exe is still paused — kill it or resume?").
4. If there's still no answer about 5 min after the freeze, olly kills the target and says why ("nobody answered, and resuming would likely restart the leak").
5. At any point, the user (notification, CLI) or otto (IPC) can choose Kill or Resume. Rising pressure to the emergency level kills the target immediately.

Timings are config values.

### Never-kill allowlist

Default allowlist (matched on process name/exe and/or cgroup unit):

- olly itself
- systemd, systemd-* daemons, dbus-broker/dbus-daemon
- kwin_wayland, kwin_wayland_wrapper, plasmashell, ksmserver, kded6, Xwayland, xdg-desktop-portal*
- pipewire, pipewire-pulse, wireplumber
- sddm, NetworkManager, sshd, polkitd
- gamescope *(my call: harmless to include. Game Mode itself is out of scope for v1, see Scope)*

Not on the list: Steam client, games, browsers, otto. *(My call: Steam and otto are killable like any app, because a Steam client leak is real. Games are killable because they're usually the culprit.)*

The base allowlist lives in config. Additions via IPC/CLI persist to a separate state file so config is never rewritten.

### Notifications

- Send through the user's session D-Bus (`org.freedesktop.Notifications`), with action buttons (Kill / Resume) for frozen targets. KDE supports actions.
- Hard rule: **notifications never block or delay the emergency path.** A failed or slow notification is logged and skipped.
- Builder's call on mechanism. A root daemon connecting to the user's session bus has auth wrinkles. Acceptable options are (a) the daemon connecting to `/run/user/1000/bus` with the user's credentials, or (b) a tiny user-level companion (`olly notify-agent` as a systemd user service) that subscribes to the IPC event stream, shows notifications, and sends Kill/Resume back. In both cases, actions must still happen if notifications fail.

### Incident log

- JSONL at `/var/lib/olly/incidents.jsonl`, one record per incident. An incident starts at the first escalation above watch and ends when pressure has been calm for about 60s.
- Each record holds: start/end time, peak PSI and `MemAvailable`, top 10 memory users at peak (name, pid, cgroup, RAM, swap, growth rate), every action taken with its reason, the outcome, a short olly-voice summary line, and the Claude diagnosis once it arrives.
- Cgroup-limit OOM kills (like the Oct 26 electron kills) can be noted from the kernel log as informational entries without olly acting. *(Nice-to-have, not v1-critical.)*
- Rotation: keep the last N incidents or N MB, whichever comes first.

### Claude (on demand, suggest-only)

- **Post-incident diagnosis.** Once an incident has ended *and* memory has recovered (for example `MemAvailable` > 25%), olly runs `claude -p` headless **as the desktop user** (it uses `~/.claude` auth and the user's `~/.local/bin/claude`), at low CPU/IO priority with a timeout. It sends the incident record plus a little context (recent journal lines for the target, app identity). It asks for a likely cause, whether this looks recurring, and suggestions (for example "consider allowlisting X" or "this game leaks; cap it with MemoryMax"). Read-only tools only.
- **Slow-leak check.** When the slow loop sees an app growing steadily for a long time (for example +2 GB over 2h with no plateau), it can ask Claude whether this looks like a leak or normal behaviour. The answer becomes a friendly notification with a suggestion and never an action.
- Rate-limited (for example at most a few calls per day, configurable) and can be switched off. If `claude` is missing or fails, olly carries on and logs that.
- Results are appended to the incident record and surfaced as a notification and via IPC.

### IPC

Socket at `/run/olly/olly.sock`. Peer uid is checked with `SO_PEERCRED`. Messages are newline-delimited JSON, as request/response plus a subscription stream.

Allowed over the socket (otto and the `olly` CLI get the same set):

- `status`: level, paused state, current targets, frozen targets, olly's own memory use
- `incidents` (with limit/offset) and `incident <id>`
- `subscribe`: streams events (level changes, actions, incident start/end, Claude results), each with a human-friendly `message` in olly's voice so otto can relay it as-is
- `pause <duration>` and `resume`: duration is required, with a maximum of 1h by default
- `allowlist add <name>` and `allowlist list`
- `frozen kill <id>` and `frozen resume <id>`: only for targets olly itself froze

Deliberately **not** exposed: killing an arbitrary pid, removing from the allowlist (edit the state/config file instead), and changing thresholds.

**While paused,** throttle and freeze are suspended, but **emergency-level kills still happen**, so a forgotten or ill-timed pause can't lock the machine. *(My call.)*

The socket protocol should be documented in `docs/ipc.md` so otto can integrate against it. The otto-side work is a separate task in the otto repo.

### Voice

olly is a cheerful, friendly little robot looking after the computer. That personality shows up in notifications, CLI output, IPC event messages, incident summaries, and the tone requested from Claude.

Rules:

- **Clarity first.** Every message says what happened, to which app, and what the user can do, in plain words. Personality is the seasoning, not the meal.
- **Tone scales with severity.** Calm and recovered moments get full cheer. Warnings stay friendly but focused. Critical and emergency messages stay short and steady, with no jokes while the user may be losing work. The relief afterward can be warm.
- Short. Notification bodies are 1–2 sentences.
- Lowercase "olly". Light emoji use is fine (for example a robot or sparkle) at the start of a title, never in place of information.
- Never blame the user, never be smug about a kill, and own mistakes ("sorry, that one might've been fine — I've noted it").

Examples (illustrative, not final copy):

- throttle: **"olly is keeping an eye on Firefox 🤖"** / "It's gobbling memory fast (+1.2 GB/min), so I've slowed it down a bit. Everything else should stay smooth."
- freeze: **"olly paused Cyberpunk2077.exe"** / "Memory is almost full. I froze it so your system doesn't lock up. Kill it or resume? I'll close it in 10s if things keep getting worse." [Kill] [Resume]
- kill: **"olly closed Cyberpunk2077.exe"** / "It was using 21 GB and climbing, and your system was about to freeze. Your desktop is safe."
- recovery: **"All clear! ✨"** / "Memory's back to comfy. I've saved notes on what happened: `olly incidents`."
- Claude follow-up: **"olly has a hunch 🔍"** / "Firefox has hit the limit 3 times this week, always with a lot of video tabs open. Want to cap it?"
- CLI `olly status`: `🤖 olly is watching. All calm — 14.2 GB free, nothing suspicious.`

## Footprint and self-protection

- Target well under 10 MB RSS and roughly 0% CPU while idle.
- `mlockall(MCL_CURRENT|MCL_FUTURE)`, `OOMScoreAdjust=-1000`, and a small `MemoryMin=` in the unit. Preallocate buffers. Avoid allocating in the hot path where practical. Never spawn processes in the emergency path.
- Keep dependencies few. An async runtime is optional: std threads plus `poll` is enough. Builder's call.
- Log to journald via stderr.

## Configuration and files

- `/etc/olly/config.toml`: thresholds, grace period, allowlist base, desktop uid, Claude settings (enabled, rate limit, recovery threshold), dry-run flag.
- `/var/lib/olly/`: `incidents.jsonl`, `allowlist.toml` (IPC additions).
- `/run/olly/olly.sock`.
- **Dry-run mode** logs and notifies "I would have…" without acting. It is the default for the first install, and the user flips it off once the thresholds look right.

## Install (Bazzite / immutable Fedora Atomic)

- `cargo build --release`. An install script copies the binary to `/usr/local/bin` (writable on Bazzite) and the unit to `/etc/systemd/system/`, writes a default config if none exists, then enables and starts the service.
- An uninstall script reverses this.

## Testing

- The rule engine is a pure function, (samples, state, config) → decisions, unit-tested with synthetic and recorded sample sequences: stable large game (no action), fast leak (throttle → freeze → kill), leak inside Steam's scope (per-process action, Steam untouched), allowlisted offender (skipped, next target), pause plus emergency.
- Parsers for `/proc` and cgroup files get tests with fixture files.
- Integration: a small memory-hog test binary run in a transient scope with a hard `MemoryMax` safety net, verifying throttle, freeze and kill end to end on the real machine.
- A week of dry-run on the real machine before enabling actions.

## Milestones

1. Monitoring daemon in dry-run: PSI trigger, sampling, levels, target scoring, journald logs.
2. Actions (throttle/freeze/kill), allowlist, notifications.
3. Incident log, IPC socket, `olly` CLI, `docs/ipc.md`.
4. Claude diagnosis and slow-leak check.
5. systemd unit, install/uninstall scripts, README.
6. (Separate, in the otto repo) otto integration.

## Left to the builder

- Exact crate choices, threading model, and module layout.
- Notification mechanism (see Notifications).
- Exact scoring formula and window sizes. The defaults above are starting points.
- Incident log rotation limits.
- Exact notification copy, following the Voice rules.
