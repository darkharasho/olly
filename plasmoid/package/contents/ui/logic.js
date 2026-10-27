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

function incidentSub(inc) {
    if (inc && inc.diagnosis) {
        var line = String(inc.diagnosis).split("\n")[0];
        var s = "Claude: " + line;
        return s.length > 70 ? s.slice(0, 69) + "…" : s;
    }
    return "peak stall " + Math.round((inc && inc.peak_full) || 0) + "%";
}

function needsCards(status, now) {
    var info = (status && status.frozen_info) || [];
    var i, out = [];
    if (info.length) {
        for (i = 0; i < info.length; i++) {
            var f = info[i];
            out.push({
                id: f.id,
                name: f.name,
                size: gb(f.bytes),
                rateText: f.rate > 0 ? "it was growing " + gb(f.rate * 60) + "/min" : "",
                countdown: countdown(f.kill_at_wall_ms, now)
            });
        }
        return out;
    }
    var frozen = (status && status.frozen) || [];
    for (i = 0; i < frozen.length; i++)
        out.push({ id: frozen[i].id, name: frozen[i].name, size: gb(frozen[i].bytes), rateText: "", countdown: "" });
    return out;
}

function stripBot(text) {
    return String(text).trim().replace(/^🤖\s*/, "");
}
