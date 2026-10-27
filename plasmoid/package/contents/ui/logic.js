.pragma library

// Pure logic for the olly tray widget. Callers pass `now` (ms since epoch).

var COLORS = {
    calm: "#3ddc84",
    watch: "#4fc3f7",
    worried: "#ffb74d",
    onit: "#ff5252",
    needs: "#ffa726",
    nap: "#9575cd",
    offline: "#6b7378"
};

var NAMES = {
    calm: "All good!",
    watch: "Watching",
    worried: "Worried",
    onit: "On it!",
    needs: "Needs you!",
    nap: "Napping",
    offline: "Offline"
};

var WEEKDAYS = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
var MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

function pausedNow(status, now) {
    return !!status && status.paused_until_wall_ms != null && status.paused_until_wall_ms > now;
}

function mood(status, now) {
    if (status == null || status.offline)
        return "offline";
    if ((status.frozen_info || []).length || (status.frozen || []).length)
        return "needs";
    if (status.last_action_wall_ms != null
            && ((status.incident_active && status.incident_acted)
                || now - status.last_action_wall_ms < 60000))
        return "onit";
    if (pausedNow(status, now))
        return "nap";
    var level = status.level;
    if (level === "warning" || level === "critical" || level === "emergency")
        return "worried";
    if (level === "watch")
        return "watch";
    return "calm";
}

function moodInfo(m) {
    return { name: NAMES[m], color: COLORS[m] };
}

function barColor(kind, v) {
    var level; // 0 green, 1 amber, 2 red
    if (kind === "ram")
        level = v > 0.20 ? 0 : (v >= 0.10 ? 1 : 2);
    else if (kind === "swap")
        level = v < 0.50 ? 0 : (v <= 0.80 ? 1 : 2);
    else
        level = v < 10 ? 0 : (v <= 30 ? 1 : 2);
    return [COLORS.calm, COLORS.worried, COLORS.onit][level];
}

function gb(bytes) {
    if (bytes >= 9.95e9)
        return (bytes / 1e9).toFixed(0) + " GB";
    if (bytes >= 0.95e9)
        return (bytes / 1e9).toFixed(1) + " GB";
    return Math.round(bytes / 1e6) + " MB";
}

function relTime(ms, now) {
    var d = now - ms;
    if (d < 60000)
        return "just now";
    if (d < 3600000)
        return Math.floor(d / 60000) + "m ago";
    if (d < 86400000)
        return Math.floor(d / 3600000) + "h ago";
    if (d < 172800000)
        return "yesterday";
    var date = new Date(ms);
    if (d < 7 * 86400000)
        return WEEKDAYS[date.getDay()];
    return MONTHS[date.getMonth()] + " " + date.getDate();
}

function countdown(killAtMs, now) {
    var s = Math.max(0, Math.floor((killAtMs - now) / 1000));
    var r = s % 60;
    return Math.floor(s / 60) + ":" + (r < 10 ? "0" : "") + r;
}

function incidentDot(inc) {
    var actions = (inc && inc.actions) || [];
    var hasFreeze = false;
    for (var i = 0; i < actions.length; i++) {
        var obj = actions[i][1];
        var name = obj ? Object.keys(obj)[0] : "";
        if (name === "Kill")
            return COLORS.onit;
        if (name === "Freeze")
            hasFreeze = true;
    }
    return hasFreeze ? COLORS.needs : COLORS.worried;
}

function incidentSummary(inc) {
    return (inc && inc.summary) ? inc.summary : "Something happened";
}

// The first non-blank line of a diagnosis, trimmed, without a leading
// markdown heading "#"s or "- " bullet. "" when every line is blank.
function firstLine(text) {
    var lines = String(text).split(/\r?\n/);
    for (var i = 0; i < lines.length; i++) {
        var line = lines[i].trim().replace(/^#+\s*/, "").replace(/^-\s+/, "").trim();
        if (line !== "")
            return line;
    }
    return "";
}

function incidentSub(inc) {
    var line = (inc && inc.diagnosis) ? firstLine(inc.diagnosis) : "";
    if (line !== "") {
        var s = "Claude: " + line;
        return s.length > 70 ? s.slice(0, 69) + "…" : s;
    }
    return "peak stall " + Math.round((inc && inc.peak_full) || 0) + "%";
}

function needsCards(status, now) {
    var info = (status && status.frozen_info) || [];
    var dryRun = !!(status && status.dry_run);
    var i, out = [];
    if (info.length) {
        for (i = 0; i < info.length; i++) {
            var f = info[i];
            out.push({
                id: f.id,
                name: f.name,
                size: gb(f.bytes),
                rateText: f.rate > 0 ? "it was growing " + gb(f.rate * 60) + "/min" : "",
                countdown: countdown(f.kill_at_wall_ms, now),
                dryRun: dryRun
            });
        }
        return out;
    }
    var frozen = (status && status.frozen) || [];
    for (i = 0; i < frozen.length; i++)
        out.push({ id: frozen[i].id, name: frozen[i].name, size: gb(frozen[i].bytes), rateText: "", countdown: "",
                  dryRun: dryRun });
    return out;
}

function stripBot(text) {
    return String(text).trim().replace(/^🤖\s*/, "");
}

var OLLY = "/usr/local/bin/olly";

// Shell-safe action command: only the fixed path, fixed words and an integer id.
// Returns "" for anything else.
function command(verb, id) {
    if (verb === "pause")
        return OLLY + " pause 30";
    if (verb === "resume")
        return OLLY + " resume";
    if (verb === "update")
        return OLLY + " update --no-wait";
    if ((verb === "kill" || verb === "unfreeze")
            && Number.isInteger(id) && id >= 0 && id <= Number.MAX_SAFE_INTEGER)
        return OLLY + " " + verb + " " + id;
    return "";
}

// `olly status --json` result to a StatusData object, or null (offline).
function parseStatus(exitCode, stdout) {
    if (exitCode !== 0)
        return null;
    try {
        var s = JSON.parse(stdout);
        return (s && typeof s === "object" && !Array.isArray(s) && !s.offline) ? s : null;
    } catch (e) {
        return null;
    }
}

// `olly incidents --json` result to an array, or null to keep the old list.
function parseIncidents(exitCode, stdout) {
    if (exitCode !== 0)
        return null;
    try {
        var list = JSON.parse(stdout);
        return Array.isArray(list) ? list : null;
    } catch (e) {
        return null;
    }
}

function esc(text) {
    return String(text).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

// The two lines of a needs-you card. `head` is StyledText with the name escaped.
// In a dry run the app was never really frozen, so say what olly would have done.
function needsText(card) {
    var rate = card.rateText ? " (" + card.rateText + ")" : "";
    if (card.dryRun)
        return {
            head: "I would have frozen <b>" + esc(card.name) + "</b> at " + card.size + rate
                + ". Dry run, so it's still running.",
            sub: card.countdown
                ? "In a real run I'd close it in " + card.countdown + " if you didn't pick."
                : "In a real run I'd close it if you didn't pick."
        };
    return {
        head: "<b>" + esc(card.name) + "</b> is frozen at " + card.size
            + (card.rateText ? " (" + card.rateText + ")" : "") + ".",
        sub: card.countdown
            ? "I'll close it in " + card.countdown + " if you don't pick, sooner if memory gets really tight."
            : "I'll close it if you don't pick, sooner if memory gets really tight."
    };
}

function frac(part, total) {
    return total > 0 ? Math.max(0, Math.min(1, part / total)) : 0;
}

// The stat bars: {label, frac (0..1), color, value}. RAM always; Swap and
// Pressure only when the daemon reports them (an older one doesn't), so the
// popup never shows made-up zeros.
function statRows(status) {
    var s = status || {};
    var ram = frac(s.mem_available || 0, s.mem_total || 0);
    var rows = [{ label: "RAM free", frac: ram, color: barColor("ram", ram), value: gb(s.mem_available || 0) }];
    if (s.swap_total != null) {
        var swap = frac(s.swap_used || 0, s.swap_total);
        rows.push({ label: "Swap used", frac: swap, color: barColor("swap", swap), value: gb(s.swap_used || 0) });
    }
    if (s.some_avg10 != null) {
        var p = s.some_avg10;
        rows.push({ label: "Pressure", frac: frac(p, 100), color: barColor("pressure", p),
                    value: (p < 10 ? p.toFixed(1) : Math.round(p)) + "%" });
    }
    return rows;
}

// One incident action `[wall_ms, {Name: {target, ...}}, dry_run]` as "HH:MM  Name target",
// with " (would have)" when it was a dry run.
function actionLine(action) {
    var d = new Date(action[0]);
    var obj = action[1] || {};
    var name = Object.keys(obj)[0] || "";
    var target = (obj[name] && obj[name].target && obj[name].target.name) || "";
    var hh = d.getHours(), mm = d.getMinutes();
    return (hh < 10 ? "0" : "") + hh + ":" + (mm < 10 ? "0" : "") + mm + "  " + name + " " + target
        + (action[2] ? " (would have)" : "");
}

function hasUpdate(status) {
    return !!(status && status.update);
}

// The update card, or null. Precedence: updating, then a fresh (undismissed,
// within the last hour) result, then an offered update. `updating` omits the
// other fields. `ownVersion` is the running widget's version.
function updateCard(status, dismissedAt, now, ownVersion) {
    if (!status)
        return null;
    if (status.update_state === "downloading" || status.update_state === "installing")
        return { kind: "updating", title: "updating…" };
    var last = status.last_update;
    if (last && last.at_wall_ms > dismissedAt && now - last.at_wall_ms <= 3600000) {
        if (last.ok) {
            var changed = status.widget_version != null && status.widget_version !== ownVersion;
            return {
                kind: "done",
                title: "updated to " + last.to + "! ✨",
                hint: changed ? "log out and back in to see my new look" : ""
            };
        }
        return { kind: "rolledback", title: "that update didn't work, so I went back to " + last.from + "." };
    }
    var u = status.update;
    if (u) {
        var notes = Array.isArray(u.notes) ? u.notes : [];
        return {
            kind: "ready",
            title: "olly " + u.version + " is ready!",
            notes: notes.slice(0, 5),
            more: Math.max(0, notes.length - 5),
            url: u.url,
            hint: "",
            button: "Update"
        };
    }
    return null;
}
