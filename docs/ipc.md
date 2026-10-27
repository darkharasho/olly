# olly IPC

Local-only, newline-delimited JSON over a unix socket: `/run/olly/olly.sock`.

- One JSON object per line, UTF-8, `\n` terminated. A request line may be at most 64 KiB; longer lines get an error and the connection is closed.
- The peer uid is checked with `SO_PEERCRED`. Only root and the desktop user are allowed; anyone else gets `{"ok":false,"error":"not allowed"}` and is disconnected.
- At most 8 concurrent connections. A 9th gets `{"ok":false,"error":"busy: too many connections"}` and is closed.
- Garbage input gets `{"ok":false,"error":"bad request: ..."}`; the connection stays open.
- IPC deliberately cannot kill an arbitrary pid, remove allowlist entries, or change thresholds.

## Responses

Every request gets exactly one response line (except `subscribe`, which then streams):

```
{"ok":true,"data":<value>}
{"ok":false,"error":"<message>"}
```

## Requests

| Request | `data` in the response |
|---|---|
| `{"cmd":"status"}` | `StatusData` (below) |
| `{"cmd":"incidents","limit":10,"offset":0}` | array of `Incident`, newest first. `limit` defaults to 10, `offset` to 0 |
| `{"cmd":"incident","id":7}` | one `Incident`, or an error if unknown |
| `{"cmd":"subscribe"}` | `"subscribed"`, then a stream of event lines |
| `{"cmd":"pause","secs":600}` | pause status. `secs` must be at least 1 and at most the daemon's `max_pause_secs` (default 3600). While paused, throttle and freeze are suspended; emergency kills still happen. A pause does not cancel the kill timers of apps that are already frozen: they are still closed at the end of their grace period or after the no-answer timeout unless you send `frozen_resume` |
| `{"cmd":"resume"}` | ends a pause early |
| `{"cmd":"allowlist_add","name":"obs"}` | adds a never-kill process name. Must be non-empty and contain no `*`. It matches a process's name (cut at 15 characters by the kernel) or its executable's file name, so long names work |
| `{"cmd":"allowlist_list"}` | array of names |
| `{"cmd":"frozen_kill","id":2}` | kills a currently frozen target (`id` is `Target.id`) |
| `{"cmd":"frozen_resume","id":2}` | resumes a currently frozen target |
| `{"cmd":"diagnose","id":3}` | asks Claude why incident `id` happened, or asks again if it already has a diagnosis; `data` is a plain sentence. It is queued behind the usual rules (waits for calm, queue of 4, daily limit), and the answer is saved on the incident and sent as a `notice`. Replies: `I'll ask Claude once things are calm.`, or `Already on it.` when a job for that incident is queued or running. Errors: `Claude is turned off in my config.`, `I've asked Claude {n} times today already. I'll be able to again tomorrow.` ("once" for 1, "twice" for 2). If the limit runs out while a by-hand request is still queued, the request is dropped and the same sentence comes as a `notice`, `no incident with id {id}` |
| `{"cmd":"update_check"}` | looks for a new olly release now and answers when the check is done (the connection waits up to 75 s for this request; others time out after 5 s). `data` is an `UpdateInfo` (below), or `null` when olly is up to date. A check that fails (no network, bad signature) is an error. Asking while a check runs joins that check. It runs even with `update_check = false` or while paused, but not while the level isn't `calm` or an incident is open: then the error is `I'll look once things are calm` |
| `{"cmd":"update"}` | starts downloading and installing the known new release; `data` is `"started"`. Errors: `there's no update to install right now` (run `update_check` first, or wait for the periodic check), `I'm already updating` (`update_state` isn't `idle`), `I'll update once things are calm` (level isn't `calm`, an incident is open, or something is frozen or throttled). The rules are checked again when the download finishes; if they fail then, olly drops back to `idle` and keeps offering the update. If the supervisor never reports back within 10 minutes, olly gives up on the install and returns to `idle`. Follow progress with `status` (`update_state`, then `last_update`, or `update_error` if it stopped before the hand-off) or the `Updated` event |
| `{"cmd":"update_finished"}` | sent by the update supervisor when it is done; olly re-reads its result (`last_update`) and the shipped widget version. `data` is `null`. Harmless from any client |

### StatusData

```json
{
  "level": "calm",
  "dry_run": true,
  "paused_until_wall_ms": null,
  "mem_available": 15227060000,
  "mem_total": 31953848000,
  "throttled": [],
  "frozen": [],
  "self_rss": 3200000,
  "message": "...",
  "swap_total": 8589934592,
  "swap_used": 104857600,
  "swap_warning": false,
  "game": null,
  "leaks": [],
  "diagnosing": null,
  "some_avg10": 1.5,
  "top": {"name":"firefox","key":{"cgroup":"/user.slice/.../app-firefox.scope","root_pid":null},"bytes":4000000000,"rate":0.0},
  "incident_active": false,
  "incident_acted": false,
  "last_action_wall_ms": null,
  "last_incident_id": 7,
  "frozen_info": [],
  "version": "0.1.0",
  "update": {"version":"0.2.0","notes":["..."],"url":"https://github.com/darkharasho/olly/releases/tag/v0.2.0"},
  "update_state": "idle",
  "last_update": null,
  "widget_version": "1.0",
  "update_error": null
}
```

`level` is one of `calm`, `watch`, `warning`, `critical`, `emergency`. Memory values are bytes; wall times are milliseconds since the Unix epoch. `message` is a ready-to-show one-line status in olly's voice.

The fields from `swap_total` on are newer; a reply from an older daemon may lack them, and they default to zero, `null`, `false` or empty.

- `swap_total`: bytes of swap in total (0 when there is none).
- `swap_used`: bytes of swap in use.
- `swap_warning`: `true` while swap is filling up: at least 50% used and up by 1 GiB or more over the last 15 minutes. It clears when usage drops below 40% or that growth is gone, and is never `true` with no swap. Information only; olly takes no action.
- `game`: the running game's display name (its process name without a trailing `.exe`), the largest one if several are running, or `null` when none is or `game_mode` is off. A game is one Steam launched (split from the Steam client), one run by a wine/proton loader, or one running under gamescope. While one runs, olly picks other apps first and never throttles the game. Absent from older daemons; treat absent as `null`. The widget hides its Ask Claude why button when it is absent, since an older daemon can't take `diagnose`.
- `diagnosing`: the incident id Claude is looking at or has queued, or `null`. One asked for with `diagnose` wins over an automatic one. Absent from older daemons; treat absent as `null`. The widget hides its Ask Claude why button when it is absent, since an older daemon can't take `diagnose`.
- `leaks`: apps that have been slowly growing, `[{"name":"firefox","growth_bytes":3000000000,"window_secs":7200}]`, largest first, at most 3. An app stays listed while olly still flags it and is dropped once it is no longer sampled. olly also sends one plain `notice` per app per 24 hours. Information only; olly takes no action. Absent from older daemons; treat absent as `[]`.
- `some_avg10`: memory "some" pressure over the last 10 s, in percent.
- `top`: the biggest app by memory in the latest sample, an `Incident` `top_at_peak` entry (`name`, `key`, `bytes`, and `rate` in bytes/s), or `null` before the first sample.
- `incident_active`: whether an incident is open.
- `incident_acted`: whether olly has made an intervention (throttle, freeze or kill) since the open incident began (false when none is open).
- `last_action_wall_ms`: wall time (ms) of olly's latest intervention (a throttle, freeze or kill, including dry-run ones and a kill you commanded), or `null` if there has been none since it started. Undoing one (unthrottle, resume) does not count.
- `last_incident_id`: the newest incident id on record, or `null` if there are none.
- `version`: the running daemon's version (`""` from a daemon older than updates).
- `update`: a newer release olly knows of, or `null`. An `UpdateInfo`: `version`, `notes` (one line each) and `url` (the release page). olly looks 5 minutes after start, then every 6 hours, only while the level is `calm`, no incident is open and olly isn't paused (`update_check = false` in the config turns the periodic check off; `update_check` requests still work).
- `update_state`: `idle`, `checking` (a check runs), `downloading` (the release is being downloaded and verified) or `installing` (handed to the supervisor, which restarts olly). Defaults to `idle`.
- `last_update`: the latest result the supervisor wrote, or `null`:

```json
{"from":"0.1.0","to":"0.2.0","ok":true,"at_wall_ms":1761550300000,"message":"updated to 0.2.0! ✨","widget_changed":false}
```

  `ok` false means the new version didn't come up and the old one was put back; `message` is ready to show. `widget_changed` means the shipped widget's version changed, so the tray widget needs a fresh login (or `olly-notify` upgrades it).
- `widget_version`: `KPlugin.Version` of the widget copy olly ships (`/usr/local/share/olly/plasmoid/metadata.json`), or `null` if it can't be read.
- `update_error`: why the latest update stopped before olly handed it to the supervisor, or `null`. `{"at_wall_ms":1761550300000,"message":"I couldn't download the update"}`; `message` is short and ready to show (`I couldn't download the update` for a failed download or check of the release, `I'll update once things are calm` when the rules failed after the download, `I couldn't start the update` when the supervisor couldn't be started). The details are in olly's journal. Cleared when a new update starts; a failure after the hand-off shows up in `last_update` instead. Defaults to `null`.
- `frozen_info`: one entry per frozen app, in the same order as `frozen`. `bytes` is its memory, `rate` its growth in bytes/s when it was frozen, and `kill_at_wall_ms` is the wall time (ms) olly closes it if you do not answer:

```json
{"id":2,"name":"game.exe","bytes":21000000000,"rate":53687091.2,"kill_at_wall_ms":1761550300000}
```

`throttled` and `frozen` hold `Target` objects:

```json
{"id":2,"key":{"cgroup":"/user.slice/.../app-steam.scope","root_pid":102},"name":"game.exe","kind":{"Processes":{"pids":[[102,1102]]}},"bytes":21000000000}
```

`kind` is `"Cgroup"` or `{"Processes":{"pids":[[pid,start_time],...]}}`. Target ids start from the daemon's start time in milliseconds since the epoch, so they are large numbers, and an id from before a restart never matches a new target.

### Incident

```json
{
  "id": 7, "start_ms_wall": 1761550000000, "end_ms_wall": 1761550014000,
  "peak_full": 30.0, "min_avail": 2000000000,
  "top_at_peak": [{"name":"game.exe","key":{"cgroup":"...","root_pid":102},"bytes":21000000000,"rate":20000000.0}],
  "actions": [[1761550009000, {"Kill":{"target":{...},"reason":"emergency"}}, false]],
  "summary": "Closed game.exe (21 GB, growing 1.2 GB/min). Recovered in 14s.",
  "diagnosis": null
}
```

`actions` entries are `[wall_ms, action, dry_run]`. `rate` is bytes per second. `diagnosis` is filled in later if Claude was asked to look.

## Event stream

After `subscribe`, the first line is the ack `{"ok":true,"data":"subscribed"}`. Then one line per event:

```
{"event":<Event>,"wall_ms":1761550009000}
```

`Event` is serde's externally tagged form:

```
{"Level":{"level":"warning"}}
{"IncidentStart":{"id":7,"t_ms":123456}}
{"IncidentEnd":{"id":7,"t_ms":137456}}
{"Acted":{"action":{...},"message":{"title":"...","body":"..."},"dry_run":false}}
{"Notice":{"message":{"title":"...","body":"..."},"frozen_id":2}}
{"Updated":{"from":"0.1.0","to":"0.2.0","ok":true,"message":"updated to 0.2.0! ✨","widget_changed":false}}
```

`Updated` has the same fields as `last_update` (without `at_wall_ms`). The daemon sends it when it loads a result less than 10 minutes old (on start, so the freshly updated daemon announces itself, and on `update_finished`), once per result. New subscribers get it first, so a client that reconnects after the restart still sees it; it may therefore arrive more than once per process lifetime of a client, so act on it idempotently.

`Acted` and `Notice` carry `message.title` and `message.body`. They are already written in olly's voice, so otto can relay them verbatim as a notification. `frozen_id`, when set, means the user can answer with `frozen_kill` or `frozen_resume` for that id. `t_ms` fields are the daemon's monotonic clock; use `wall_ms` for display.

A subscriber that stops reading is dropped (its queue holds 64 lines; three full-queue misses in a row disconnect it). Reconnect and `subscribe` again.

## Examples

```sh
echo '{"cmd":"status"}' | socat - UNIX-CONNECT:/run/olly/olly.sock

# Stay connected and print events
echo '{"cmd":"subscribe"}' | socat - UNIX-CONNECT:/run/olly/olly.sock
```

```js
const net = require("net");
const sock = net.connect("/run/olly/olly.sock");
let buf = "";
sock.on("data", (d) => {
  buf += d;
  let i;
  while ((i = buf.indexOf("\n")) >= 0) {
    const msg = JSON.parse(buf.slice(0, i));
    buf = buf.slice(i + 1);
    const inner = msg.event && (msg.event.Acted || msg.event.Notice);
    if (inner) console.log(inner.message.title, "-", inner.message.body);
  }
});
sock.write(JSON.stringify({ cmd: "subscribe" }) + "\n");
```
