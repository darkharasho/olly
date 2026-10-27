import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";

const path = new URL("../../plasmoid/package/contents/ui/logic.js", import.meta.url);
const src = fs.readFileSync(path, "utf8").replace(/^\.pragma library.*$/m, "");
const L = new Function(
  src +
    "; return {mood, moodInfo, barColor, gb, relTime, countdown, incidentDot, incidentSummary, incidentSub, needsCards, stripBot, pausedNow, command, parseStatus, parseIncidents, esc, needsText, statRows, swapWarning, diagnoseButton, diagnosisDone, gameLine, leakCards, pruneDismissed, dismissLeak, actionLine, hasUpdate, updateCard, updateSeenAt, shouldCheck, checkLine};",
)();

const C = {
  calm: "#3ddc84", watch: "#4fc3f7", worried: "#ffb74d", onit: "#ff5252",
  needs: "#ffa726", nap: "#9575cd", offline: "#6b7378",
};
const NOW = 1761550000000;
const base = (o) => Object.assign({ level: "calm", paused_until_wall_ms: null, frozen: [], frozen_info: [],
  incident_active: false, incident_acted: false, last_action_wall_ms: null }, o);

test("mood precedence", () => {
  const fi = [{ id: 1, name: "a", bytes: 1, rate: 0, kill_at_wall_ms: NOW + 1000 }];
  assert.equal(L.mood(base({ frozen_info: fi, paused_until_wall_ms: NOW + 5000, level: "warning", last_action_wall_ms: NOW }), NOW), "needs");
  assert.equal(L.mood(base({ last_action_wall_ms: NOW - 1000, paused_until_wall_ms: NOW + 5000, level: "warning" }), NOW), "onit");
  assert.equal(L.mood(base({ last_action_wall_ms: NOW - 900000, incident_active: true, incident_acted: true }), NOW), "onit");
  assert.equal(L.mood(base({ last_action_wall_ms: NOW - 900000, incident_active: true, incident_acted: false }), NOW), "calm");
  assert.equal(L.mood(base({ paused_until_wall_ms: NOW + 5000, level: "warning" }), NOW), "nap");
  assert.equal(L.mood(base({ paused_until_wall_ms: NOW - 1, level: "warning" }), NOW), "worried");
  for (const l of ["warning", "critical", "emergency"]) assert.equal(L.mood(base({ level: l }), NOW), "worried");
  assert.equal(L.mood(base({ level: "watch" }), NOW), "watch");
  assert.equal(L.mood(base(), NOW), "calm");
});

test("mood offline", () => {
  assert.equal(L.mood(null, NOW), "offline");
  assert.equal(L.mood({ offline: true }, NOW), "offline");
});

test("mood_old_daemon_shape", () => {
  const old = { level: "calm", dry_run: true, paused_until_wall_ms: null, mem_available: 15227060000,
    mem_total: 31953848000, throttled: [], frozen: [], self_rss: 3200000, message: "..." };
  assert.equal(L.mood(Object.assign({}, old, { level: "warning" }), NOW), "worried");
  assert.equal(L.mood(Object.assign({}, old, { frozen: [{ id: 2, name: "game.exe", bytes: 1 }] }), NOW), "needs");
});

test("mood_needs_until_all_answered", () => {
  const e = (id) => ({ id, name: "x", bytes: 1, rate: 0, kill_at_wall_ms: NOW + 1000 });
  assert.equal(L.mood(base({ frozen_info: [e(1), e(2)], last_action_wall_ms: NOW - 1000 }), NOW), "needs");
  assert.equal(L.mood(base({ frozen_info: [e(1)], last_action_wall_ms: NOW - 1000 }), NOW), "needs");
  assert.equal(L.mood(base({ frozen_info: [], last_action_wall_ms: NOW - 30000 }), NOW), "onit");
});

test("onit linger", () => {
  assert.equal(L.mood(base({ last_action_wall_ms: NOW - 59999 }), NOW), "onit");
  assert.equal(L.mood(base({ last_action_wall_ms: NOW - 60001, level: "watch" }), NOW), "watch");
  assert.equal(L.mood(base({ last_action_wall_ms: NOW - 60001 }), NOW), "calm");
});

test("moodInfo", () => {
  const names = { calm: "All good!", watch: "Watching", worried: "Worried", onit: "On it!",
    needs: "Needs you!", nap: "Napping", offline: "Offline" };
  for (const k of Object.keys(names)) assert.deepEqual(L.moodInfo(k), { name: names[k], color: C[k] });
});

test("barColor thresholds", () => {
  const g = C.calm, a = C.worried, r = C.onit;
  assert.equal(L.barColor("ram", 0.21), g);
  assert.equal(L.barColor("ram", 0.2), a);
  assert.equal(L.barColor("ram", 0.1), a);
  assert.equal(L.barColor("ram", 0.09), r);
  assert.equal(L.barColor("swap", 0.49), g);
  assert.equal(L.barColor("swap", 0.5), a);
  assert.equal(L.barColor("swap", 0.8), a);
  assert.equal(L.barColor("swap", 0.81), r);
  assert.equal(L.barColor("psi", 9.9), g);
  assert.equal(L.barColor("psi", 10), a);
  assert.equal(L.barColor("psi", 30), a);
  assert.equal(L.barColor("psi", 30.1), r);
});

test("gb matches fmt_bytes", () => {
  assert.equal(L.gb(21_000_000_000), "21 GB");
  assert.equal(L.gb(1_200_000_000), "1.2 GB");
  assert.equal(L.gb(512_000_000), "512 MB");
  assert.equal(L.gb(0), "0 MB");
  assert.equal(L.gb(10_000_000_000), "10 GB");
});

test("relTime", () => {
  // Fixed local dates so the test is time-zone independent. Wed Oct 15 2025 12:00 local.
  const now = new Date(2025, 9, 15, 12, 0, 0).getTime();
  const ago = (ms) => L.relTime(now - ms, now);
  const S = 1000, M = 60 * S, H = 60 * M, D = 24 * H;
  assert.equal(ago(0), "just now");
  assert.equal(ago(59 * S), "just now");
  assert.equal(ago(60 * S), "1m ago");
  assert.equal(ago(59 * M), "59m ago");
  assert.equal(ago(60 * M), "1h ago");
  assert.equal(ago(23 * H), "23h ago");
  assert.equal(ago(24 * H), "yesterday");
  assert.equal(ago(47 * H), "yesterday");
  const d2 = new Date(2025, 9, 13, 9, 0).getTime(); // Mon, 51h before
  assert.equal(L.relTime(d2, now), "Mon");
  const d6 = new Date(2025, 9, 9, 13, 0).getTime(); // Thu, under 7 days
  assert.equal(L.relTime(d6, now), "Thu");
  const old = new Date(2025, 9, 3, 12, 0).getTime();
  assert.equal(L.relTime(old, now), "Oct 3");
  const old2 = new Date(2025, 0, 21, 12, 0).getTime();
  assert.equal(L.relTime(old2, now), "Jan 21");
});

test("relTime boundaries at 48 h and 7 d", () => {
  const now = new Date(2025, 9, 15, 12, 0, 0).getTime();
  const H = 3600000, D = 24 * H;
  assert.equal(L.relTime(now - (48 * H - 1), now), "yesterday");
  assert.equal(L.relTime(now - 48 * H, now), "Mon");
  assert.equal(L.relTime(now - (7 * D - 1), now), "Wed");
  assert.equal(L.relTime(now - 7 * D, now), "Oct 8");
});

test("countdown", () => {
  assert.equal(L.countdown(NOW + 252000, NOW), "4:12");
  assert.equal(L.countdown(NOW + 5000, NOW), "0:05");
  assert.equal(L.countdown(NOW - 5000, NOW), "0:00");
});

test("incident_dot_and_sub_line_edge_cases", () => {
  const act = (name) => [NOW, { [name]: { reason: "x" } }, false];
  assert.equal(L.incidentDot({ actions: [act("Throttle"), act("Kill")] }), C.onit);
  assert.equal(L.incidentDot({ actions: [act("Throttle"), act("Freeze")] }), C.needs);
  assert.equal(L.incidentDot({ actions: [act("Throttle")] }), C.worried);
  assert.equal(L.incidentDot({ actions: [] }), C.worried);
  assert.equal(L.incidentSub({ actions: [], peak_full: 0, diagnosis: null }), "peak stall 0%");
  assert.equal(L.incidentSub({ peak_full: 29.6 }), "peak stall 30%");
  assert.equal(L.incidentSub({ peak_full: 3, diagnosis: "Short cause.\nMore." }), "Claude: Short cause.");
  const long = "x".repeat(200) + "\nsecond line";
  const sub = L.incidentSub({ peak_full: 3, diagnosis: long });
  assert.equal(sub.length, 70);
  assert.ok(sub.startsWith("Claude: x"));
  assert.ok(sub.endsWith("…"));
  assert.ok(!sub.includes("\n"));
  assert.equal(L.incidentSummary({ summary: "Closed it." }), "Closed it.");
  assert.equal(L.incidentSummary({ summary: "" }), "Something happened");
  assert.equal(L.incidentSummary({}), "Something happened");
});

test("incidentSub uses the first non-blank diagnosis line", () => {
  assert.equal(L.incidentSub({ peak_full: 3, diagnosis: "\n  \nReal cause.\nMore." }), "Claude: Real cause.");
  assert.equal(L.incidentSub({ peak_full: 3, diagnosis: "First line.\r\nSecond." }), "Claude: First line.");
  assert.equal(L.incidentSub({ peak_full: 3, diagnosis: "## Heading here\nbody" }), "Claude: Heading here");
  assert.equal(L.incidentSub({ peak_full: 3, diagnosis: "- a bullet\nbody" }), "Claude: a bullet");
  assert.equal(L.incidentSub({ peak_full: 29.6, diagnosis: "\n \r\n\t\n" }), "peak stall 30%");
  assert.equal(L.incidentSub({ peak_full: 3, diagnosis: "  padded  " }), "Claude: padded");
});

test("needs_cards_follow_frozen_info", () => {
  const fi = [
    { id: 5, name: "game.exe", bytes: 21_000_000_000, rate: 20_000_000, kill_at_wall_ms: NOW + 252000 },
    { id: 3, name: "b", bytes: 512_000_000, rate: 0, kill_at_wall_ms: NOW + 1000 },
  ];
  const cards = L.needsCards(base({ frozen_info: fi, frozen: [{ id: 5 }, { id: 3 }] }), NOW);
  assert.deepEqual(cards.map((c) => c.id), [5, 3]);
  assert.deepEqual(cards[0], { id: 5, name: "game.exe", size: "21 GB", rateText: "it was growing 1.2 GB/min", countdown: "4:12", dryRun: false });
  assert.equal(cards[1].rateText, "");
  assert.deepEqual(L.needsCards(base({ frozen_info: [] }), NOW), []);
  const old = L.needsCards({ frozen: [{ id: 2, name: "g", bytes: 21_000_000_000 }] }, NOW);
  assert.deepEqual(old, [{ id: 2, name: "g", size: "21 GB", rateText: "", countdown: "", dryRun: false }]);
});

test("needs_cards_carry_dry_run", () => {
  const fi = [{ id: 5, name: "chrome", bytes: 7.4e9, rate: 35e6, kill_at_wall_ms: NOW + 252000 }];
  assert.equal(L.needsCards(base({ frozen_info: fi, dry_run: true }), NOW)[0].dryRun, true);
  assert.equal(L.needsCards(base({ frozen_info: fi, dry_run: false }), NOW)[0].dryRun, false);
  assert.equal(L.needsCards({ frozen: [{ id: 2, name: "g", bytes: 1 }], dry_run: true }, NOW)[0].dryRun, true);
});

test("stripBot", () => {
  assert.equal(L.stripBot("🤖 olly isn't running"), "olly isn't running");
  assert.equal(L.stripBot("  hello "), "hello");
  assert.equal(L.stripBot("olly isn't running"), "olly isn't running");
});

test("pausedNow", () => {
  assert.equal(L.pausedNow(base({ paused_until_wall_ms: NOW + 1 }), NOW), true);
  assert.equal(L.pausedNow(base({ paused_until_wall_ms: NOW }), NOW), false);
  assert.equal(L.pausedNow(base(), NOW), false);
  assert.equal(L.pausedNow(null, NOW), false);
});

test("command is shell-safe", () => {
  const O = "/usr/local/bin/olly";
  assert.equal(L.command("pause"), O + " pause 30");
  assert.equal(L.command("resume"), O + " resume");
  assert.equal(L.command("kill", 1761550000001), O + " kill 1761550000001");
  assert.equal(L.command("unfreeze", 2), O + " unfreeze 2");
  for (const bad of ["2; rm -rf ~", "2", 2.5, -1, NaN, Infinity, null, undefined, 2 ** 60])
    assert.equal(L.command("kill", bad), "", String(bad));
  assert.equal(L.command("diagnose", 7), "/usr/local/bin/olly diagnose 7");
  assert.equal(L.command("diagnose", "7; id"), "");
  assert.equal(L.command("rm", 2), "");
  assert.equal(L.command("kill $(id)", 2), "");
});

test("parseStatus", () => {
  assert.deepEqual(L.parseStatus(0, '{"level":"calm"}\n'), { level: "calm" });
  assert.equal(L.parseStatus(1, '{"offline":true}'), null);
  assert.equal(L.parseStatus(0, '{"offline":true}'), null);
  assert.equal(L.parseStatus(0, "garbage"), null);
  assert.equal(L.parseStatus(0, "[]"), null);
  assert.equal(L.parseStatus(0, "null"), null);
  assert.equal(L.parseStatus(undefined, ""), null);
});

test("parseIncidents", () => {
  assert.deepEqual(L.parseIncidents(0, '[{"id":1}]'), [{ id: 1 }]);
  assert.equal(L.parseIncidents(1, "[]"), null);
  assert.equal(L.parseIncidents(0, '{"offline":true}'), null);
  assert.equal(L.parseIncidents(0, "nope"), null);
});

test("needsText", () => {
  const t = L.needsText({ name: "a<b>&c", size: "7.4 GB", rateText: "it was growing 2.1 GB/min", countdown: "4:12" });
  assert.equal(t.head, "<b>a&lt;b&gt;&amp;c</b> is frozen at 7.4 GB (it was growing 2.1 GB/min).");
  assert.equal(t.sub, "I'll close it in 4:12 if you don't pick, sooner if memory gets really tight.");
  const o = L.needsText({ name: "g", size: "21 GB", rateText: "", countdown: "" });
  assert.equal(o.head, "<b>g</b> is frozen at 21 GB.");
  assert.equal(o.sub, "I'll close it if you don't pick, sooner if memory gets really tight.");
});

test("needsText in dry run says what olly would have done", () => {
  const t = L.needsText({ name: "chr<o>me", size: "7.4 GB", rateText: "it was growing 2.1 GB/min", countdown: "4:12", dryRun: true });
  assert.equal(t.head, "I would have frozen <b>chr&lt;o&gt;me</b> at 7.4 GB (it was growing 2.1 GB/min). Dry run, so it's still running.");
  assert.equal(t.sub, "In a real run I'd close it in 4:12 if you didn't pick.");
  const o = L.needsText({ name: "g", size: "21 GB", rateText: "", countdown: "", dryRun: true });
  assert.equal(o.head, "I would have frozen <b>g</b> at 21 GB. Dry run, so it's still running.");
  assert.equal(o.sub, "In a real run I'd close it if you didn't pick.");
  // Not dry run: the real copy, even when dryRun is explicitly false.
  const r = L.needsText({ name: "g", size: "21 GB", rateText: "", countdown: "4:12", dryRun: false });
  assert.equal(r.head, "<b>g</b> is frozen at 21 GB.");
});

test("statRows", () => {
  const r = L.statRows({ mem_available: 13.1e9, mem_total: 32e9, swap_used: 1.9e9, swap_total: 16e9, some_avg10: 0.4 });
  assert.deepEqual(r.map((x) => x.label), ["RAM free", "Swap used", "Pressure"]);
  assert.deepEqual(r.map((x) => x.value), ["13 GB", "1.9 GB", "0.4%"]);
  assert.deepEqual(r.map((x) => x.color), [C.calm, C.calm, C.calm]);
  assert.ok(Math.abs(r[0].frac - 13.1 / 32) < 1e-9);
  const hot = L.statRows({ mem_available: 1.9e9, mem_total: 32e9, swap_used: 11.4e9, swap_total: 16e9, some_avg10: 22 });
  assert.deepEqual(hot.map((x) => x.color), [C.onit, C.worried, C.worried]);
  assert.equal(hot[2].value, "22%");
  // No swap at all (swap_total 0) still shows the row.
  const noSwap = L.statRows({ mem_available: 1e9, mem_total: 2e9, swap_used: 0, swap_total: 0, some_avg10: 0 });
  assert.deepEqual(noSwap.map((x) => x.frac), [0.5, 0, 0]);
  assert.deepEqual(noSwap.map((x) => x.value), ["1.0 GB", "0 MB", "0.0%"]);
});

test("statRows hides rows an older daemon doesn't report", () => {
  const old = L.statRows({ mem_available: 1e9, mem_total: 2e9 });
  assert.deepEqual(old.map((x) => x.label), ["RAM free"]);
  const nulls = L.statRows({ mem_available: 1e9, mem_total: 2e9, swap_used: null, swap_total: null, some_avg10: null });
  assert.deepEqual(nulls.map((x) => x.label), ["RAM free"]);
  const swapOnly = L.statRows({ mem_available: 1e9, mem_total: 2e9, swap_used: 1e9, swap_total: 4e9 });
  assert.deepEqual(swapOnly.map((x) => x.label), ["RAM free", "Swap used"]);
  const psiOnly = L.statRows({ mem_available: 1e9, mem_total: 2e9, some_avg10: 5 });
  assert.deepEqual(psiOnly.map((x) => x.label), ["RAM free", "Pressure"]);
  assert.deepEqual(L.statRows(null).map((x) => x.label), ["RAM free"]);
});

test("actionLine", () => {
  const t = new Date(2026, 9, 27, 9, 5).getTime();
  assert.equal(L.actionLine([t, { Kill: { target: { name: "memhog" }, reason: "emergency" } }, false]), "09:05  Kill memhog");
  assert.equal(L.actionLine([t, { Freeze: { target: { name: "chrome" } } }, true]), "09:05  Freeze chrome (would have)");
  assert.equal(L.actionLine([t, { Freeze: { target: { name: "chrome" } } }]), "09:05  Freeze chrome");
});

const upd = { version: "0.2.0", notes: ["a", "b"], url: "https://example.test/r" };
const lastOk = (o) => Object.assign({ from: "0.1.0", to: "0.2.0", ok: true, at_wall_ms: NOW - 1000, message: "" }, o);

test("command update", () => {
  assert.equal(L.command("update"), "/usr/local/bin/olly update --no-wait");
  assert.equal(L.command("check"), "/usr/local/bin/olly update --check");
});

test("shouldCheck: at most every 10 minutes", () => {
  assert.equal(L.shouldCheck(0, NOW), true);
  assert.equal(L.shouldCheck(NOW - 599999, NOW), false);
  assert.equal(L.shouldCheck(NOW - 600000, NOW), true);
});

test("checkLine", () => {
  const st = { version: "0.2.4" };
  assert.equal(L.checkLine(st, true, NOW, "nope"), "looking for updates…");
  assert.equal(L.checkLine(st, false, NOW, "I'll look once things are calm"), "I'll look once things are calm");
  assert.equal(L.checkLine(st, false, NOW, ""), "you're on the newest olly (0.2.4).");
  assert.equal(L.checkLine(st, false, 0, ""), "");
  assert.equal(L.checkLine(null, false, NOW, ""), "");
  assert.equal(L.checkLine({}, false, NOW, ""), "");
});

test("hasUpdate", () => {
  assert.equal(L.hasUpdate(null), false);
  assert.equal(L.hasUpdate({}), false);
  assert.equal(L.hasUpdate({ update: null }), false);
  assert.equal(L.hasUpdate({ update: upd }), true);
});

test("updateCard null when nothing to show", () => {
  assert.equal(L.updateCard(null, 0, NOW, "0.1.0"), null);
  assert.equal(L.updateCard({ update: null, update_state: "idle", last_update: null }, 0, NOW, "0.1.0"), null);
});

test("updateCard ready: notes capped at 5 with more", () => {
  const c = L.updateCard({ update: upd, update_state: "idle" }, 0, NOW, "0.1.0");
  assert.deepEqual(c, { kind: "ready", title: "olly 0.2.0 is ready!", notes: ["a", "b"], more: 0, url: upd.url, hint: "", error: "", button: "Update" });
  const n7 = ["1", "2", "3", "4", "5", "6", "7"];
  const c7 = L.updateCard({ update: { ...upd, notes: n7 } }, 0, NOW, "0.1.0");
  assert.deepEqual(c7.notes, ["1", "2", "3", "4", "5"]);
  assert.equal(c7.more, 2);
  const c5 = L.updateCard({ update: { ...upd, notes: n7.slice(0, 5) } }, 0, NOW, "0.1.0");
  assert.equal(c5.notes.length, 5);
  assert.equal(c5.more, 0);
  const cn = L.updateCard({ update: { version: "0.2.0", url: "u" } }, 0, NOW, "0.1.0");
  assert.deepEqual(cn.notes, []);
  assert.equal(cn.more, 0);
  assert.deepEqual(L.updateCard({ update: { ...upd, notes: null } }, 0, NOW, "0.1.0").notes, []);
});

test("updateCard updating", () => {
  for (const s of ["downloading", "installing"]) {
    const c = L.updateCard({ update: upd, update_state: s, last_update: lastOk() }, 0, NOW, "0.1.0");
    assert.deepEqual(c, { kind: "updating", title: "updating…" });
  }
  assert.equal(L.updateCard({ update: upd, update_state: "checking" }, 0, NOW, "0.1.0").kind, "ready");
});

test("updateCard done and hint", () => {
  const s = (o) => Object.assign({ update: null, update_state: "idle", last_update: lastOk(), widget_version: "0.2.0" }, o);
  let c = L.updateCard(s(), 0, NOW, "0.2.0");
  assert.deepEqual({ kind: c.kind, title: c.title, hint: c.hint }, { kind: "done", title: "updated to 0.2.0! ✨", hint: "" });
  c = L.updateCard(s(), 0, NOW, "0.1.0");
  assert.equal(c.hint, "log out and back in to see my new look");
  assert.equal(L.updateCard(s({ widget_version: null }), 0, NOW, "0.1.0").hint, "");
  assert.equal(L.updateCard(s({ widget_version: undefined }), 0, NOW, "0.1.0").hint, "");
  // done outranks ready
  assert.equal(L.updateCard(s({ update: upd }), 0, NOW, "0.2.0").kind, "done");
});

test("updateCard rolledback", () => {
  const c = L.updateCard({ update: null, last_update: lastOk({ ok: false }) }, 0, NOW, "0.1.0");
  assert.deepEqual(c, { kind: "rolledback", title: "that update didn't work, so I went back to 0.1.0." });
});

test("updateCard dismissed and stale results hidden", () => {
  const at = NOW - 1000;
  assert.equal(L.updateCard({ last_update: lastOk() }, at, NOW, "0.1.0"), null);
  assert.equal(L.updateCard({ last_update: lastOk() }, at - 1, NOW, "0.1.0").kind, "done");
  assert.equal(L.updateCard({ last_update: lastOk({ at_wall_ms: NOW - 3600000 }) }, 0, NOW, "0.1.0").kind, "done");
  assert.equal(L.updateCard({ last_update: lastOk({ at_wall_ms: NOW - 3600001 }) }, 0, NOW, "0.1.0"), null);
  // hidden result falls through to ready
  assert.equal(L.updateCard({ update: upd, last_update: lastOk() }, at, NOW, "0.1.0").kind, "ready");
});

const uerr = (o) => Object.assign({ at_wall_ms: NOW - 1000, message: "I couldn't download the update" }, o);

test("updateCard ready shows a recent update_error", () => {
  const c = L.updateCard({ update: upd, update_state: "idle", update_error: uerr() }, 0, NOW, "0.1.0");
  assert.equal(c.kind, "ready");
  assert.equal(c.error, "I couldn't download the update");
  assert.equal(c.button, "Update");
  // older than an hour, dismissed, or older than the last result: hidden
  assert.equal(L.updateCard({ update: upd, update_error: uerr({ at_wall_ms: NOW - 3600000 }) }, 0, NOW, "0.1.0").error,
    "I couldn't download the update");
  assert.equal(L.updateCard({ update: upd, update_error: uerr({ at_wall_ms: NOW - 3600001 }) }, 0, NOW, "0.1.0").error, "");
  assert.equal(L.updateCard({ update: upd, update_error: uerr() }, NOW - 1000, NOW, "0.1.0").error, "");
  assert.equal(L.updateCard({ update: upd, update_error: uerr() }, NOW - 1001, NOW, "0.1.0").error,
    "I couldn't download the update");
  const older = lastOk({ ok: false, at_wall_ms: NOW - 500 });
  assert.equal(L.updateCard({ update: upd, update_error: uerr(), last_update: older }, NOW - 400, NOW, "0.1.0").error, "");
  const prior = lastOk({ ok: false, at_wall_ms: NOW - 2000 });
  assert.equal(L.updateCard({ update: upd, update_error: uerr(), last_update: prior }, NOW - 1500, NOW, "0.1.0").error,
    "I couldn't download the update");
  assert.equal(L.updateCard({ update: upd, update_error: null }, 0, NOW, "0.1.0").error, "");
  assert.equal(L.updateCard({ update: upd, update_error: { at_wall_ms: NOW } }, 0, NOW, "0.1.0").error, "");
});

test("updateCard precedence holds with an update_error", () => {
  const e = uerr();
  assert.equal(L.updateCard({ update: upd, update_state: "downloading", update_error: e }, 0, NOW, "0.1.0").kind, "updating");
  assert.equal(L.updateCard({ update: upd, update_error: e, last_update: lastOk({ at_wall_ms: NOW - 2000 }) }, 0, NOW, "0.1.0").kind, "done");
  assert.equal(L.updateCard({ update: upd, update_error: e, last_update: lastOk({ ok: false, at_wall_ms: NOW - 2000 }) }, 0, NOW, "0.1.0").kind,
    "rolledback");
  // No update offered: nothing to show the error on.
  assert.equal(L.updateCard({ update: null, update_error: e }, 0, NOW, "0.1.0"), null);
});

test("updateSeenAt covers results and errors", () => {
  assert.equal(L.updateSeenAt(null), 0);
  assert.equal(L.updateSeenAt({}), 0);
  assert.equal(L.updateSeenAt({ last_update: lastOk() }), NOW - 1000);
  assert.equal(L.updateSeenAt({ update_error: uerr({ at_wall_ms: NOW - 5 }) }), NOW - 5);
  assert.equal(L.updateSeenAt({ last_update: lastOk(), update_error: uerr({ at_wall_ms: NOW - 5000 }) }), NOW - 1000);
});

test("swapWarning", () => {
  assert.equal(L.swapWarning({ swap_warning: true, swap_used: 9.2e9, swap_total: 16e9 }),
    "Swap is filling up: 9.2 GB of 16 GB and climbing. Things may get sluggish. Closing an app you don't need would help.");
  assert.equal(L.swapWarning({ swap_warning: false, swap_used: 9e9, swap_total: 16e9 }), "");
  assert.equal(L.swapWarning({ swap_used: 9e9, swap_total: 16e9 }), "");
  assert.equal(L.swapWarning(null), "");
});

test("gameLine", () => {
  assert.equal(L.gameLine({ game: "Elden Ring" }), "🎮 Elden Ring is running. I'll pick other apps first.");
  assert.equal(L.gameLine({ game: null }), "");
  assert.equal(L.gameLine({ game: "" }), "");
  assert.equal(L.gameLine({}), "");
  assert.equal(L.gameLine(null), "");
});

test("leakCards", () => {
  const st = { leaks: [
    { name: "firefox", growth_bytes: 3e9, window_secs: 7200 },
    { name: "slack", growth_bytes: 600e6, window_secs: 3600 },
  ] };
  const cards = L.leakCards(st, {});
  assert.deepEqual(cards[0], { name: "firefox",
    text: "firefox has grown 3.0 GB in the last 2 hours. Restarting it when convenient would free that up." });
  assert.equal(cards[1].text, "slack has grown 600 MB in the last hour. Restarting it when convenient would free that up.");
  assert.deepEqual(L.leakCards(st, { firefox: true }).map((c) => c.name), ["slack"]);
  assert.deepEqual(L.leakCards({}, {}), []);
  assert.deepEqual(L.leakCards({ leaks: null }, undefined), []);
  assert.deepEqual(L.leakCards(null, {}), []);
});

test("pruneDismissed and dismissLeak", () => {
  const st = { leaks: [{ name: "firefox", growth_bytes: 1, window_secs: 1 }] };
  const d = { firefox: true, slack: true };
  assert.deepEqual(L.pruneDismissed(st, d), { firefox: true });
  const same = { firefox: true };
  assert.equal(L.pruneDismissed(st, same), same);
  assert.deepEqual(L.pruneDismissed({}, d), {});
  assert.equal(L.pruneDismissed(null, d), d);
  const next = L.dismissLeak(same, "slack");
  assert.deepEqual(next, { firefox: true, slack: true });
  assert.deepEqual(same, { firefox: true });
  assert.deepEqual(L.dismissLeak(undefined, "a"), { a: true });
});

test("diagnoseButton", () => {
  assert.equal(L.diagnoseButton(null, { id: 3 }), "ask");
  assert.equal(L.diagnoseButton({ diagnosing: null }, { id: 3 }), "ask");
  assert.equal(L.diagnoseButton({ diagnosing: 4 }, { id: 3 }), "ask");
  assert.equal(L.diagnoseButton({ diagnosing: 3 }, { id: 3 }), "looking");
  assert.equal(L.diagnoseButton({}, { id: 3, diagnosis: "  " }), "ask");
  assert.equal(L.diagnoseButton({ diagnosing: 3 }, { id: 3, diagnosis: "a leak" }), "hidden");
  assert.equal(L.diagnoseButton({}, null), "hidden");
});

test("diagnosisDone", () => {
  assert.equal(L.diagnosisDone({ diagnosing: 3 }, { diagnosing: null }), true);
  assert.equal(L.diagnosisDone({ diagnosing: 3 }, {}), true);
  assert.equal(L.diagnosisDone({ diagnosing: 3 }, { diagnosing: 4 }), true);
  assert.equal(L.diagnosisDone({ diagnosing: 3 }, { diagnosing: 3 }), false);
  assert.equal(L.diagnosisDone({ diagnosing: null }, { diagnosing: null }), false);
  assert.equal(L.diagnosisDone({}, { diagnosing: 3 }), false);
  assert.equal(L.diagnosisDone(null, { diagnosing: 3 }), false);
  assert.equal(L.diagnosisDone({ diagnosing: 3 }, null), false);
});
