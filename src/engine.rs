//! The rule engine: pure. Samples and time in, actions and events out.

use crate::allowlist::Allowlist;
use crate::config::{Config, Thresholds};
use crate::history::History;
use crate::sample::{AppKey, AppSample, SystemSample, TargetKind};
use crate::voice::{self, Message};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Calm,
    Watch,
    Warning,
    Critical,
    Emergency,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KillReason {
    Emergency,
    GraceExpired,
    NoAnswer,
    LastResort,
    User,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Target {
    pub id: u64,
    pub key: AppKey,
    pub name: String,
    pub kind: TargetKind,
    pub bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Action {
    Throttle { target: Target, high: u64 },
    Unthrottle { target: Target },
    Freeze { target: Target },
    Resume { target: Target },
    Kill { target: Target, reason: KillReason },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Event {
    Level {
        level: Level,
    },
    IncidentStart {
        id: u64,
        t_ms: u64,
    },
    IncidentEnd {
        id: u64,
        t_ms: u64,
    },
    Acted {
        action: Action,
        message: Message,
        dry_run: bool,
    },
    /// Reminders, all-clear, hunches. `frozen_id` => offer Kill/Resume.
    Notice {
        message: Message,
        frozen_id: Option<u64>,
    },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct StepOutput {
    pub actions: Vec<Action>,
    pub events: Vec<Event>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    KillFrozen(u64),
    ResumeFrozen(u64),
    /// Absolute time on the same monotonic clock as `SystemSample.t_ms`.
    Pause {
        until_ms: u64,
    },
    Unpause,
}

const MIB: u64 = 1024 * 1024;
/// At most this many last-resort (size-based) kills per incident.
const LAST_RESORT_CAP: u32 = 2;

fn avail_pct(s: &SystemSample) -> f32 {
    if s.mem.total == 0 {
        100.0
    } else {
        (s.mem.available as f64 * 100.0 / s.mem.total as f64) as f32
    }
}

/// Session services (plasmashell and friends) are never a last-resort pick.
fn in_session_slice(a: &AppSample) -> bool {
    a.key.cgroup.contains("/session.slice/")
}

/// Highest level whose full-PSI or available-memory threshold is met.
pub fn classify(s: &SystemSample, t: &Thresholds) -> Level {
    let full = s.psi.full_avg10;
    let avail = avail_pct(s);
    if full >= t.emergency_full || avail <= t.emergency_avail {
        Level::Emergency
    } else if full >= t.critical_full || avail <= t.critical_avail {
        Level::Critical
    } else if full >= t.warning_full || avail <= t.warning_avail {
        Level::Warning
    } else if s.psi.some_avg10 >= t.watch_some || avail <= t.watch_avail {
        Level::Watch
    } else {
        Level::Calm
    }
}

/// The last kill, until its target is gone and memory has had time to settle.
struct KillWait {
    id: u64,
    key: AppKey,
    killed_at: u64,
    /// When the target was first seen gone from the samples.
    gone_at: Option<u64>,
}

/// A killed target still in the samples after this long counts as stuck
/// (a restarted service, an unreaped zombie), so it can't hold kills forever.
const KILL_GONE_CEILING_MS: u64 = 15_000;

struct FrozenState {
    target: Target,
    at_ms: u64,
    full_at_freeze: f32,
    /// Growth rate (bytes/s) when frozen.
    rate: f64,
    grace_checked: bool,
    reminded: bool,
}

/// A frozen target as the status reply shows it.
#[derive(Clone, Debug)]
pub struct FrozenView {
    pub target: Target,
    pub at_ms: u64,
    /// Growth rate (bytes/s) when it was frozen.
    pub rate: f64,
}

pub struct Engine {
    cfg: Config,
    level: Level,
    incident: Option<u64>,
    calm_since: Option<u64>,
    throttled: Vec<Target>,
    frozen: Vec<FrozenState>,
    last_action_ms: Option<u64>,
    paused_until: Option<u64>,
    next_incident_id: u64,
    next_target_id: u64,
    /// Resumed by the user this incident: no throttle or freeze, only emergency kills.
    resumed: Vec<AppKey>,
    /// Dry run only: already "killed" this incident, so not chosen again.
    dry_killed: Vec<AppKey>,
    /// Blocks the next kill until the last one's target is gone and settled.
    kill_wait: Option<KillWait>,
    /// MemAvailable when the last kill this incident happened; None before the first.
    avail_at_kill: Option<u64>,
    /// MemAvailable in the current sample (kills from commands have no sample).
    last_avail: u64,
    last_resorts: u32,
    cap_noticed: bool,
    /// The actuator couldn't kill these this incident; not picked again.
    stuck: Vec<AppKey>,
}

impl Engine {
    pub fn new(cfg: Config) -> Self {
        Engine {
            cfg,
            level: Level::Calm,
            incident: None,
            calm_since: None,
            throttled: Vec::new(),
            frozen: Vec::new(),
            last_action_ms: None,
            paused_until: None,
            next_incident_id: 1,
            next_target_id: 1,
            resumed: Vec::new(),
            dry_killed: Vec::new(),
            kill_wait: None,
            avail_at_kill: None,
            last_avail: 0,
            last_resorts: 0,
            cap_noticed: false,
            stuck: Vec::new(),
        }
    }

    pub fn set_next_incident_id(&mut self, id: u64) {
        self.next_incident_id = id;
    }

    /// Seed target ids (e.g. from wall time) so ids from before a restart never match.
    pub fn set_next_target_id(&mut self, id: u64) {
        self.next_target_id = id;
    }

    /// The actuator could not kill target `id`, so it won't go away by itself. Stop
    /// waiting for it (the settle window still runs from now) and don't pick it again
    /// this incident.
    pub fn kill_failed(&mut self, id: u64, now_ms: u64) {
        if let Some(w) = self.kill_wait.as_mut().filter(|w| w.id == id) {
            w.gone_at.get_or_insert(now_ms);
            if !self.stuck.contains(&w.key) {
                self.stuck.push(w.key.clone());
            }
        }
    }

    /// No kill until the last killed target is gone and `kill_settle_secs` have passed.
    fn may_kill(&self, now: u64) -> bool {
        let settle = self.cfg.timing.kill_settle_secs * 1000;
        self.kill_wait
            .as_ref()
            .is_none_or(|w| w.gone_at.is_some_and(|g| now.saturating_sub(g) >= settle))
    }

    /// After a kill this incident, a further emergency kill needs the emergency to be
    /// real right now, not in the lagging avg10: available memory at the emergency
    /// line, or the stall since the last sample at emergency while available memory
    /// has not come back (by at least 1% of RAM) since the kill.
    fn still_emergency(&self, sys: &SystemSample) -> bool {
        let Some(avail_then) = self.avail_at_kill else {
            return true;
        };
        let t = &self.cfg.thresholds;
        if avail_pct(sys) <= t.emergency_avail {
            return true;
        }
        let stalled = sys.full_now.is_some_and(|f| f >= t.emergency_full);
        let improved = sys.mem.available >= avail_then.saturating_add(sys.mem.total / 100);
        stalled && !improved
    }

    pub fn step(
        &mut self,
        sys: &SystemSample,
        apps: &[AppSample],
        hist: &History,
        allow: &Allowlist,
    ) -> StepOutput {
        let now = sys.t_ms;
        let mut out = StepOutput::default();
        let find = |key: &AppKey| apps.iter().find(|a| &a.key == key);

        // 1. Level and incident bookkeeping.
        let level = classify(sys, &self.cfg.thresholds);
        if level != self.level {
            self.level = level;
            out.events.push(Event::Level { level });
        }
        if level > Level::Watch && self.incident.is_none() {
            let id = self.next_incident_id;
            self.next_incident_id += 1;
            self.incident = Some(id);
            out.events.push(Event::IncidentStart { id, t_ms: now });
        }
        // Below Warning counts as calm for closing: a busy desktop can sit at Watch for hours.
        if level < Level::Warning && sys.psi.some_avg10 < self.cfg.thresholds.calm_some {
            self.calm_since.get_or_insert(now);
        } else {
            self.calm_since = None;
        }
        let close = self.incident.is_some()
            && self.frozen.is_empty()
            && self
                .calm_since
                .is_some_and(|c| now.saturating_sub(c) >= self.cfg.timing.calm_end_secs * 1000);

        // The last kill's target is gone once it no longer shows up in the samples.
        self.last_avail = sys.mem.available;
        if let Some(w) = self.kill_wait.as_mut() {
            if w.gone_at.is_none() && find(&w.key).is_none() {
                w.gone_at = Some(now);
            } else if w.gone_at.is_none() && now.saturating_sub(w.killed_at) >= KILL_GONE_CEILING_MS
            {
                w.gone_at = Some(now);
                if !self.stuck.contains(&w.key) {
                    self.stuck.push(w.key.clone());
                }
            }
        }
        if self.may_kill(now) {
            self.kill_wait = None;
        }
        let confirmed = self.still_emergency(sys);

        // 2. Frozen targets. At most one kill per step, then the settle window.
        let tm = &self.cfg.timing;
        let (grace, remind, giveup) = (
            tm.freeze_grace_secs * 1000,
            tm.freeze_remind_secs * 1000,
            tm.freeze_giveup_secs * 1000,
        );
        let mut frozen = std::mem::take(&mut self.frozen);
        frozen.retain_mut(|f| {
            let Some(a) = find(&f.target.key) else {
                return false;
            };
            f.target.bytes = a.bytes;
            let elapsed = now.saturating_sub(f.at_ms);
            let mut reason = None;
            if !self.may_kill(now) {
                // Decided once the last kill has settled.
            } else if level == Level::Emergency && confirmed {
                reason = Some(KillReason::Emergency);
            } else {
                if !f.grace_checked && elapsed >= grace {
                    f.grace_checked = true;
                    if sys.psi.full_avg10 >= f.full_at_freeze {
                        reason = Some(KillReason::GraceExpired);
                    }
                }
                if reason.is_none() && elapsed >= giveup {
                    reason = Some(KillReason::NoAnswer);
                }
            }
            if let Some(reason) = reason {
                self.kill(&mut out, f.target.clone(), reason, now);
                return false;
            }
            if !f.reminded && elapsed >= remind {
                f.reminded = true;
                out.events.push(Event::Notice {
                    message: voice::still_frozen(&f.target.name),
                    frozen_id: Some(f.target.id),
                });
            }
            true
        });
        self.frozen = frozen;
        self.throttled.retain(|t| find(&t.key).is_some());

        // 3. Candidates, best first: rate, then bytes.
        // (app, rate, recently growing, eligible for throttle/freeze)
        let mut cands: Vec<(&AppSample, f64, bool, bool)> = apps
            .iter()
            .filter(|a| {
                !allow.allows_app(a)
                    && !self.is_frozen(&a.key)
                    && !self.dry_killed.contains(&a.key)
                    && !self.stuck.contains(&a.key)
                    && self.kill_wait.as_ref().is_none_or(|w| w.key != a.key)
            })
            .map(|a| {
                let gentle = !self.resumed.contains(&a.key);
                (
                    a,
                    hist.rate(&a.key, now),
                    hist.recently_growing(&a.key, now),
                    gentle,
                )
            })
            .collect();
        cands.sort_by(|x, y| y.1.total_cmp(&x.1).then(y.0.bytes.cmp(&x.0.bytes)));
        let top_growing = cands.iter().find(|c| c.2).map(|c| c.0);

        // 4. Cooldown (throttle is exempt).
        let cooled = self
            .last_action_ms
            .is_none_or(|t| now.saturating_sub(t) >= self.cfg.timing.action_cooldown_secs * 1000);
        let paused = self.paused_until.is_some_and(|u| now < u);
        let dry = self.cfg.dry_run;

        // 5. Act by level.
        match level {
            Level::Warning if !paused => {
                for &(a, rate, growing, gentle) in &cands {
                    if !growing
                        || !gentle
                        || a.kind != TargetKind::Cgroup
                        || self.is_throttled(&a.key)
                    {
                        continue;
                    }
                    let target = self.target_for(a);
                    // memory.high caps RAM only; swap (zram) doesn't count against it.
                    let high = (a.ram * 105 / 100).max(a.ram + 128 * MIB);
                    let message = voice::throttled(&a.name, (rate * 60.0) as u64, dry);
                    self.throttled.push(target.clone());
                    self.act(&mut out, Action::Throttle { target, high }, message);
                }
            }
            Level::Critical if !paused && cooled => {
                if let Some(&(a, rate, ..)) = cands.iter().find(|c| c.2 && c.3) {
                    let target = self.target_for(a);
                    self.frozen.push(FrozenState {
                        target: target.clone(),
                        at_ms: now,
                        full_at_freeze: sys.psi.full_avg10,
                        rate,
                        grace_checked: false,
                        reminded: false,
                    });
                    self.last_action_ms = Some(now);
                    let message = voice::frozen(&a.name, self.cfg.timing.freeze_grace_secs, dry);
                    self.act(&mut out, Action::Freeze { target }, message);
                }
            }
            Level::Emergency if cooled && confirmed && self.may_kill(now) => {
                let pick = match top_growing {
                    Some(a) => Some((a, KillReason::Emergency)),
                    None if self.last_resorts >= LAST_RESORT_CAP => {
                        if !self.cap_noticed {
                            self.cap_noticed = true;
                            out.events.push(Event::Notice {
                                message: voice::last_resort_cap(),
                                frozen_id: None,
                            });
                        }
                        None
                    }
                    None => cands
                        .iter()
                        .map(|c| c.0)
                        .filter(|a| !in_session_slice(a))
                        .max_by_key(|a| a.bytes)
                        .map(|a| (a, KillReason::LastResort)),
                };
                if let Some((a, reason)) = pick {
                    if reason == KillReason::LastResort {
                        self.last_resorts += 1;
                    }
                    let target = self.target_for(a);
                    self.kill(&mut out, target, reason, now);
                }
            }
            _ => {}
        }

        // Incident close: unthrottle what is still running, then all clear.
        if close {
            for mut target in std::mem::take(&mut self.throttled) {
                let Some(a) = find(&target.key) else { continue };
                target.bytes = a.bytes;
                let message = voice::unthrottled(&target.name);
                self.act(&mut out, Action::Unthrottle { target }, message);
            }
            self.resumed.clear();
            self.dry_killed.clear();
            self.stuck.clear();
            self.kill_wait = None;
            self.avail_at_kill = None;
            self.last_resorts = 0;
            self.cap_noticed = false;
            if let Some(id) = self.incident.take() {
                out.events.push(Event::IncidentEnd { id, t_ms: now });
                out.events.push(Event::Notice {
                    message: voice::all_clear(),
                    frozen_id: None,
                });
            }
        }

        // 6. Expire the pause.
        if self.paused_until.is_some_and(|u| now >= u) {
            self.paused_until = None;
        }
        out
    }

    pub fn command(&mut self, cmd: Command, now_ms: u64) -> Result<StepOutput, String> {
        let mut out = StepOutput::default();
        match cmd {
            Command::KillFrozen(id) => {
                let target = self.take_frozen(id)?;
                self.kill(&mut out, target, KillReason::User, now_ms);
            }
            Command::ResumeFrozen(id) => {
                let target = self.take_frozen(id)?;
                self.resumed.push(target.key.clone());
                let message = voice::resumed(&target.name);
                self.act(&mut out, Action::Resume { target }, message);
            }
            Command::Pause { until_ms } => self.paused_until = Some(until_ms),
            Command::Unpause => self.paused_until = None,
        }
        Ok(out)
    }

    pub fn level(&self) -> Level {
        self.level
    }

    pub fn throttled(&self) -> &[Target] {
        &self.throttled
    }

    pub fn frozen(&self) -> Vec<Target> {
        self.frozen.iter().map(|f| f.target.clone()).collect()
    }

    pub fn frozen_view(&self) -> Vec<FrozenView> {
        self.frozen
            .iter()
            .map(|f| FrozenView {
                target: f.target.clone(),
                at_ms: f.at_ms,
                rate: f.rate,
            })
            .collect()
    }

    /// The id of the open incident, if any.
    pub fn incident(&self) -> Option<u64> {
        self.incident
    }

    pub fn paused_until(&self) -> Option<u64> {
        self.paused_until
    }

    fn is_frozen(&self, key: &AppKey) -> bool {
        self.frozen.iter().any(|f| &f.target.key == key)
    }

    fn is_throttled(&self, key: &AppKey) -> bool {
        self.throttled.iter().any(|t| &t.key == key)
    }

    /// A target for `a`, keeping the id of an existing throttled target.
    fn target_for(&mut self, a: &AppSample) -> Target {
        let id = match self.throttled.iter().find(|t| t.key == a.key) {
            Some(t) => t.id,
            None => {
                self.next_target_id += 1;
                self.next_target_id - 1
            }
        };
        Target {
            id,
            key: a.key.clone(),
            name: a.name.clone(),
            kind: a.kind.clone(),
            bytes: a.bytes,
        }
    }

    fn take_frozen(&mut self, id: u64) -> Result<Target, String> {
        let i = self
            .frozen
            .iter()
            .position(|f| f.target.id == id)
            .ok_or_else(|| format!("no frozen target with id {id}"))?;
        Ok(self.frozen.remove(i).target)
    }

    /// Kill `target`: forget it everywhere, start the cooldown, and hold further kills
    /// until it is gone and settled.
    fn kill(&mut self, out: &mut StepOutput, target: Target, reason: KillReason, now: u64) {
        self.frozen.retain(|f| f.target.key != target.key);
        self.throttled.retain(|t| t.key != target.key);
        self.last_action_ms = Some(now);
        self.avail_at_kill = Some(self.last_avail);
        // A dry-run target never goes away, so it counts as gone at once.
        self.kill_wait = Some(KillWait {
            id: target.id,
            key: target.key.clone(),
            killed_at: now,
            gone_at: self.cfg.dry_run.then_some(now),
        });
        if self.cfg.dry_run {
            self.dry_killed.push(target.key.clone());
        }
        let message = voice::killed(&target.name, target.bytes, reason, self.cfg.dry_run);
        self.act(out, Action::Kill { target, reason }, message);
    }

    fn act(&self, out: &mut StepOutput, action: Action, message: Message) {
        out.events.push(Event::Acted {
            action: action.clone(),
            message,
            dry_run: self.cfg.dry_run,
        });
        out.actions.push(action);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procfs::{MemInfo, Psi};
    use crate::sample::Sampler;
    use std::path::{Path, PathBuf};

    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    const TOTAL: u64 = 32 * GIB;

    fn sys(t: u64, some: f32, full: f32, avail_pct: f32) -> SystemSample {
        SystemSample {
            t_ms: t,
            psi: Psi {
                some_avg10: some,
                full_avg10: full,
                full_total: 0,
            },
            // Instantaneous stall tracks avg10 unless a test says otherwise.
            full_now: Some(full),
            mem: MemInfo {
                total: TOTAL,
                available: (TOTAL as f64 * avail_pct as f64 / 100.0) as u64,
                swap_total: 0,
                swap_free: 0,
            },
            zram_used: 0,
        }
    }

    fn calm(t: u64) -> SystemSample {
        sys(t, 0.0, 0.0, 50.0)
    }
    fn warning(t: u64) -> SystemSample {
        sys(t, 0.0, 12.0, 50.0)
    }
    fn critical(t: u64) -> SystemSample {
        sys(t, 0.0, 30.0, 50.0)
    }
    fn emergency(t: u64) -> SystemSample {
        sys(t, 0.0, 55.0, 50.0)
    }

    fn app(name: &str, key: &str, kind: TargetKind, bytes: u64) -> AppSample {
        AppSample {
            key: AppKey {
                cgroup: key.into(),
                root_pid: None,
            },
            name: name.into(),
            exe: None,
            kind,
            bytes,
            ram: bytes,
            names: vec![name.into()],
            exe_names: vec![],
        }
    }

    fn procs() -> TargetKind {
        TargetKind::Processes {
            pids: vec![(500, 1500)],
        }
    }

    /// Cgroup app growing 1 GiB per 10s, starting at 10 GiB.
    fn leak(t_ms: u64) -> AppSample {
        app(
            "leaky",
            "/leaky",
            TargetKind::Cgroup,
            10 * GIB + t_ms * (GIB / 10) / 1000,
        )
    }

    struct Sim {
        eng: Engine,
        hist: History,
        allow: Allowlist,
    }

    impl Sim {
        fn new() -> Self {
            let cfg = Config::default();
            let hist = History::new(&cfg.growth);
            let allow = Allowlist::load(
                cfg.allowlist.clone(),
                PathBuf::from("/nonexistent/olly/a.toml"),
            );
            Sim {
                eng: Engine::new(cfg),
                hist,
                allow,
            }
        }

        fn step(&mut self, s: SystemSample, apps: &[AppSample]) -> StepOutput {
            self.hist.record(s.t_ms, apps);
            self.eng.step(&s, apps, &self.hist, &self.allow)
        }

        /// Calm steps at 0..=end_s seconds to build growth history.
        fn warm(&mut self, end_s: u64, apps: impl Fn(u64) -> Vec<AppSample>) {
            for s in 0..=end_s {
                let out = self.step(calm(s * 1000), &apps(s * 1000));
                assert!(out.actions.is_empty());
            }
        }
    }

    fn kills(out: &StepOutput) -> Vec<(String, KillReason)> {
        out.actions
            .iter()
            .filter_map(|a| match a {
                Action::Kill { target, reason } => Some((target.name.clone(), *reason)),
                _ => None,
            })
            .collect()
    }

    fn notices(out: &StepOutput) -> Vec<(Message, Option<u64>)> {
        out.events
            .iter()
            .filter_map(|e| match e {
                Event::Notice { message, frozen_id } => Some((message.clone(), *frozen_id)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn classify_levels() {
        let t = Thresholds::default();
        assert_eq!(classify(&sys(0, 0.0, 0.0, 50.0), &t), Level::Calm);
        assert_eq!(classify(&sys(0, 12.0, 0.0, 50.0), &t), Level::Watch);
        assert_eq!(classify(&sys(0, 0.0, 0.0, 14.0), &t), Level::Watch);
        assert_eq!(classify(&sys(0, 0.0, 12.0, 50.0), &t), Level::Warning);
        assert_eq!(classify(&sys(0, 0.0, 0.0, 4.0), &t), Level::Critical);
        assert_eq!(classify(&sys(0, 0.0, 30.0, 50.0), &t), Level::Critical);
        assert_eq!(classify(&sys(0, 0.0, 55.0, 50.0), &t), Level::Emergency);
        assert_eq!(classify(&sys(0, 0.0, 0.0, 1.0), &t), Level::Emergency);
    }

    #[test]
    fn stable_big_game_untouched_until_emergency() {
        let mut sim = Sim::new();
        let game = || vec![app("game", "/game", TargetKind::Cgroup, 20 * GIB)];
        sim.warm(10, |_| game());
        for s in 11..=70 {
            let out = sim.step(critical(s * 1000), &game());
            assert!(out.actions.is_empty(), "step {s}: {:?}", out.actions);
        }
        assert_eq!(sim.eng.level(), Level::Critical);
    }

    #[test]
    fn fast_leak_ladder() {
        let mut sim = Sim::new();
        sim.warm(10, |t| vec![leak(t)]);

        let out = sim.step(warning(11_000), &[leak(11_000)]);
        let bytes = leak(11_000).bytes;
        assert!(bytes * 105 / 100 > bytes + 128 * MIB);
        match &out.actions[..] {
            [Action::Throttle { target, high }] => {
                assert_eq!(*high, bytes * 105 / 100);
                assert_eq!(target.bytes, bytes);
                assert_eq!(target.kind, TargetKind::Cgroup);
            }
            a => panic!("expected throttle, got {a:?}"),
        }
        assert_eq!(sim.eng.throttled().len(), 1);

        let out = sim.step(critical(12_000), &[leak(12_000)]);
        let id = match &out.actions[..] {
            [Action::Freeze { target }] => target.id,
            a => panic!("expected freeze, got {a:?}"),
        };
        assert_eq!(
            sim.eng.frozen().iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![id]
        );

        for s in 13..22 {
            let out = sim.step(critical(s * 1000), &[leak(s * 1000)]);
            assert!(out.actions.is_empty(), "step {s}: {:?}", out.actions);
        }
        let out = sim.step(critical(22_000), &[leak(22_000)]);
        match &out.actions[..] {
            [Action::Kill {
                target,
                reason: KillReason::GraceExpired,
            }] => assert_eq!(target.id, id),
            a => panic!("expected grace kill, got {a:?}"),
        }
        assert!(sim.eng.frozen().is_empty());
        assert!(sim.eng.throttled().is_empty());
    }

    #[test]
    fn grace_spared_when_falling_then_reminder_then_giveup() {
        let mut sim = Sim::new();
        sim.warm(10, |t| vec![leak(t)]);
        let out = sim.step(sys(11_000, 0.0, 30.0, 50.0), &[leak(11_000)]);
        let id = match &out.actions[..] {
            [Action::Freeze { target }] => target.id,
            a => panic!("expected freeze, got {a:?}"),
        };

        let mut reminders = Vec::new();
        for s in 12..=310 {
            let t = s * 1000;
            let out = sim.step(sys(t, 0.0, 20.0, 50.0), &[leak(t)]);
            for n in notices(&out) {
                reminders.push((t, n));
            }
            assert!(out.actions.is_empty(), "t {t}: {:?}", out.actions);
        }
        assert_eq!(reminders.len(), 1);
        assert_eq!(reminders[0].0, 71_000);
        assert_eq!(reminders[0].1, (voice::still_frozen("leaky"), Some(id)));
        assert_eq!(sim.eng.frozen().len(), 1);

        let out = sim.step(sys(311_000, 0.0, 20.0, 50.0), &[leak(311_000)]);
        assert_eq!(
            kills(&out),
            vec![("leaky".to_string(), KillReason::NoAnswer)]
        );
        assert!(sim.eng.frozen().is_empty());
    }

    #[test]
    fn emergency_kills_frozen_immediately() {
        let mut sim = Sim::new();
        sim.warm(10, |t| vec![leak(t)]);
        let out = sim.step(critical(11_000), &[leak(11_000)]);
        let id = match &out.actions[..] {
            [Action::Freeze { target }] => target.id,
            a => panic!("expected freeze, got {a:?}"),
        };
        let out = sim.step(emergency(12_000), &[leak(12_000)]);
        match &out.actions[..] {
            [Action::Kill {
                target,
                reason: KillReason::Emergency,
            }] => {
                assert_eq!(target.id, id);
                assert_eq!(target.bytes, leak(12_000).bytes);
            }
            a => panic!("expected emergency kill, got {a:?}"),
        }
        assert!(sim.eng.frozen().is_empty());
    }

    #[test]
    fn steam_game_frozen_not_steam() {
        let base = Sampler::new(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/steam"),
            1000,
        )
        .apps();
        let apps_at = |t_ms: u64| {
            let mut v = base.clone();
            for a in &mut v {
                if a.key.root_pid == Some(102) {
                    a.bytes += t_ms * (GIB / 10) / 1000;
                }
            }
            v
        };
        let mut sim = Sim::new();
        sim.warm(10, apps_at);
        let out = sim.step(critical(11_000), &apps_at(11_000));
        match &out.actions[..] {
            [Action::Freeze { target }] => {
                assert_eq!(target.name, "game.exe");
                match &target.kind {
                    TargetKind::Processes { pids } => {
                        let p: Vec<i32> = pids.iter().map(|&(p, _)| p).collect();
                        assert_eq!(p, vec![102, 103]);
                    }
                    k => panic!("expected processes, got {k:?}"),
                }
            }
            a => panic!("expected freeze, got {a:?}"),
        }
    }

    #[test]
    fn process_target_not_throttled_at_warning() {
        let mut sim = Sim::new();
        let grow = |t: u64| {
            vec![app(
                "proc",
                "/proc",
                procs(),
                10 * GIB + t * (GIB / 10) / 1000,
            )]
        };
        sim.warm(10, grow);
        for s in 11..=40 {
            let out = sim.step(warning(s * 1000), &grow(s * 1000));
            assert!(out.actions.is_empty(), "step {s}: {:?}", out.actions);
        }
        assert!(sim.eng.throttled().is_empty());
    }

    #[test]
    fn allowlisted_grower_skipped_next_chosen() {
        let mut sim = Sim::new();
        let apps = |t: u64| {
            let mut kwin = app(
                "kwin_wayland",
                "/kwin",
                TargetKind::Cgroup,
                4 * GIB + t * (GIB / 5) / 1000,
            );
            kwin.names.push("kwin_wayland_wrapper".into());
            vec![kwin, leak(t)]
        };
        sim.warm(10, apps);
        let out = sim.step(critical(11_000), &apps(11_000));
        match &out.actions[..] {
            [Action::Freeze { target }] => assert_eq!(target.name, "leaky"),
            a => panic!("expected freeze of leaky, got {a:?}"),
        }
    }

    #[test]
    fn last_resort_kills_largest_when_none_growing() {
        let mut sim = Sim::new();
        let apps = |_t: u64| {
            vec![
                app("small", "/small", TargetKind::Cgroup, 2 * GIB),
                app("big", "/big", procs(), 12 * GIB),
                app("mid", "/mid", TargetKind::Cgroup, 6 * GIB),
            ]
        };
        sim.warm(10, apps);
        let out = sim.step(emergency(11_000), &apps(11_000));
        assert_eq!(
            kills(&out),
            vec![("big".to_string(), KillReason::LastResort)]
        );
        assert_eq!(out.actions.len(), 1);
        let Event::Acted {
            message, dry_run, ..
        } = out.events.last().unwrap()
        else {
            panic!("expected acted event last")
        };
        assert!(!dry_run);
        assert_eq!(
            *message,
            voice::killed("big", 12 * GIB, KillReason::LastResort, false)
        );
    }

    #[test]
    fn pause_blocks_freeze_not_emergency() {
        let mut sim = Sim::new();
        sim.warm(10, |t| vec![leak(t)]);
        let out = sim
            .eng
            .command(Command::Pause { until_ms: 100_000 }, 10_500)
            .unwrap();
        assert_eq!(out, StepOutput::default());
        assert_eq!(sim.eng.paused_until(), Some(100_000));
        for s in 11..=20 {
            let t = s * 1000;
            let s = if s < 15 { warning(t) } else { critical(t) };
            let out = sim.step(s, &[leak(t)]);
            assert!(out.actions.is_empty(), "t {t}: {:?}", out.actions);
        }
        let out = sim.step(emergency(21_000), &[leak(21_000)]);
        assert_eq!(
            kills(&out),
            vec![("leaky".to_string(), KillReason::Emergency)]
        );
        assert_eq!(sim.eng.paused_until(), Some(100_000));

        sim.step(calm(100_000), &[]);
        assert_eq!(sim.eng.paused_until(), None);

        sim.eng
            .command(Command::Pause { until_ms: 500_000 }, 101_000)
            .unwrap();
        sim.eng.command(Command::Unpause, 102_000).unwrap();
        assert_eq!(sim.eng.paused_until(), None);
    }

    #[test]
    fn kills_spaced_by_settle() {
        let mut sim = Sim::new();
        let other = |t: u64| {
            app(
                "other",
                "/other",
                TargetKind::Cgroup,
                8 * GIB + t * (GIB / 20) / 1000,
            )
        };
        let mut alive = vec![true, true];
        let apps = |t: u64, alive: &[bool]| {
            let mut v = Vec::new();
            if alive[0] {
                v.push(leak(t));
            }
            if alive[1] {
                v.push(other(t));
            }
            v
        };
        sim.warm(10, |t| apps(t, &[true, true]));
        let mut kill_times = Vec::new();
        for s in 11..=30 {
            let t = s * 1000;
            let out = sim.step(emergency(t), &apps(t, &alive));
            for (name, reason) in kills(&out) {
                assert_eq!(reason, KillReason::Emergency);
                kill_times.push((t, name.clone()));
                alive[if name == "leaky" { 0 } else { 1 }] = false;
            }
        }
        // leaky is gone from 12s; the settle window then runs 10s.
        assert_eq!(
            kill_times,
            vec![(11_000, "leaky".to_string()), (22_000, "other".to_string())]
        );
    }

    #[test]
    fn incident_end_unthrottles_and_all_clear() {
        let mut sim = Sim::new();
        sim.warm(10, |t| vec![leak(t)]);
        let out = sim.step(warning(11_000), &[leak(11_000)]);
        assert_eq!(
            out.events[0],
            Event::Level {
                level: Level::Warning
            }
        );
        assert_eq!(
            out.events[1],
            Event::IncidentStart {
                id: 1,
                t_ms: 11_000
            }
        );
        let tid = match &out.actions[..] {
            [Action::Throttle { target, .. }] => target.id,
            a => panic!("expected throttle, got {a:?}"),
        };

        // Calm starts at 12s; the incident closes once calm has lasted 60s.
        let stable = leak(11_000);
        for s in 12..72 {
            let out = sim.step(calm(s * 1000), std::slice::from_ref(&stable));
            assert!(out.actions.is_empty(), "step {s}: {:?}", out.actions);
            assert!(!out
                .events
                .iter()
                .any(|e| matches!(e, Event::IncidentEnd { .. })));
        }
        let out = sim.step(calm(72_000), std::slice::from_ref(&stable));
        match &out.actions[..] {
            [Action::Unthrottle { target }] => assert_eq!(target.id, tid),
            a => panic!("expected unthrottle, got {a:?}"),
        }
        assert_eq!(out.events.len(), 3, "{:?}", out.events);
        assert!(matches!(
            &out.events[0],
            Event::Acted { action: Action::Unthrottle { .. }, message, .. } if *message == voice::unthrottled("leaky")
        ));
        assert_eq!(
            out.events[1],
            Event::IncidentEnd {
                id: 1,
                t_ms: 72_000
            }
        );
        assert_eq!(
            out.events[2],
            Event::Notice {
                message: voice::all_clear(),
                frozen_id: None
            }
        );
        assert!(sim.eng.throttled().is_empty());

        // A new incident gets the next id.
        let out = sim.step(warning(80_000), &[stable]);
        assert!(out.events.contains(&Event::IncidentStart {
            id: 2,
            t_ms: 80_000
        }));
    }

    #[test]
    fn incident_ids_resume_from_set_value() {
        let mut sim = Sim::new();
        sim.eng.set_next_incident_id(42);
        let out = sim.step(critical(1000), &[]);
        assert!(out
            .events
            .contains(&Event::IncidentStart { id: 42, t_ms: 1000 }));
    }

    #[test]
    fn command_kill_and_resume_frozen() {
        let mut sim = Sim::new();
        let other = |t: u64| {
            app(
                "other",
                "/other",
                TargetKind::Cgroup,
                8 * GIB + t * (GIB / 20) / 1000,
            )
        };
        let apps = |t: u64| vec![leak(t), other(t)];
        sim.warm(10, apps);
        let mut ids = Vec::new();
        for s in 11..=16 {
            let out = sim.step(sys(s * 1000, 0.0, 20.0, 4.0), &apps(s * 1000));
            for a in out.actions {
                match a {
                    Action::Freeze { target } => ids.push((target.id, target.name)),
                    a => panic!("unexpected {a:?}"),
                }
            }
        }
        assert_eq!(ids.len(), 2, "{ids:?}");
        assert_eq!(ids[0].1, "leaky");
        assert_eq!(ids[1].1, "other");

        let out = sim
            .eng
            .command(Command::KillFrozen(ids[0].0), 17_000)
            .unwrap();
        match &out.actions[..] {
            [Action::Kill {
                target,
                reason: KillReason::User,
            }] => assert_eq!(target.id, ids[0].0),
            a => panic!("expected user kill, got {a:?}"),
        }
        let out = sim
            .eng
            .command(Command::ResumeFrozen(ids[1].0), 17_000)
            .unwrap();
        match &out.actions[..] {
            [Action::Resume { target }] => assert_eq!(target.id, ids[1].0),
            a => panic!("expected resume, got {a:?}"),
        }
        assert!(matches!(
            &out.events[..],
            [Event::Acted { message, .. }] if *message == voice::resumed("other")
        ));
        assert!(sim.eng.frozen().is_empty());
        assert_eq!(
            sim.eng.command(Command::KillFrozen(99), 17_000),
            Err("no frozen target with id 99".to_string())
        );
        assert!(sim
            .eng
            .command(Command::ResumeFrozen(ids[0].0), 17_000)
            .is_err());
    }

    #[test]
    fn frozen_view_keeps_rate_and_time() {
        let mut sim = Sim::new();
        sim.warm(10, |t| vec![leak(t)]);
        sim.step(critical(11_000), &[leak(11_000)]);
        let views = sim.eng.frozen_view();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].at_ms, 11_000);
        let key = AppKey {
            cgroup: "/leaky".into(),
            root_pid: None,
        };
        let rate = sim.hist.rate(&key, 11_000);
        assert!(rate > 0.0);
        assert_eq!(views[0].rate, rate);
        assert!(sim.eng.incident().is_some());
    }

    #[test]
    fn frozen_app_that_exited_is_dropped() {
        let mut sim = Sim::new();
        sim.warm(10, |t| vec![leak(t)]);
        sim.step(critical(11_000), &[leak(11_000)]);
        assert_eq!(sim.eng.frozen().len(), 1);
        let out = sim.step(critical(12_000), &[]);
        assert!(out.actions.is_empty());
        assert!(sim.eng.frozen().is_empty());
    }

    fn freeze_id(out: &StepOutput) -> u64 {
        match &out.actions[..] {
            [Action::Freeze { target }] => target.id,
            a => panic!("expected freeze, got {a:?}"),
        }
    }

    #[test]
    fn resumed_target_not_refrozen_until_emergency() {
        let mut sim = Sim::new();
        sim.warm(10, |t| vec![leak(t)]);
        let id = freeze_id(&sim.step(critical(11_000), &[leak(11_000)]));
        sim.eng.command(Command::ResumeFrozen(id), 11_500).unwrap();
        for s in 12..=31 {
            let t = s * 1000;
            let out = sim.step(critical(t), &[leak(t)]);
            assert!(out.actions.is_empty(), "t {t}: {:?}", out.actions);
        }
        let out = sim.step(emergency(32_000), &[leak(32_000)]);
        assert_eq!(
            kills(&out),
            vec![("leaky".to_string(), KillReason::Emergency)]
        );
    }

    #[test]
    fn resumed_set_cleared_when_incident_closes() {
        let mut sim = Sim::new();
        sim.warm(10, |t| vec![leak(t)]);
        let id = freeze_id(&sim.step(critical(11_000), &[leak(11_000)]));
        sim.eng.command(Command::ResumeFrozen(id), 11_500).unwrap();
        let mut ended = false;
        for s in 12..=72 {
            let out = sim.step(calm(s * 1000), &[leak(s * 1000)]);
            ended |= out
                .events
                .iter()
                .any(|e| matches!(e, Event::IncidentEnd { .. }));
        }
        assert!(ended);
        let out = sim.step(critical(80_000), &[leak(80_000)]);
        freeze_id(&out);
    }

    #[test]
    fn dry_run_kills_each_app_once_per_incident() {
        let mut sim = Sim::new();
        sim.eng.cfg.dry_run = true;
        sim.warm(10, |t| vec![leak(t)]);
        let mut all = Vec::new();
        for s in 11..=41 {
            let t = s * 1000;
            all.extend(kills(&sim.step(emergency(t), &[leak(t)])));
        }
        assert_eq!(all, vec![("leaky".to_string(), KillReason::Emergency)]);

        // Same at Critical: freeze, grace kill, then never refrozen.
        let mut sim = Sim::new();
        sim.eng.cfg.dry_run = true;
        sim.warm(10, |t| vec![leak(t)]);
        let mut actions = Vec::new();
        for s in 11..=60 {
            let t = s * 1000;
            actions.extend(sim.step(critical(t), &[leak(t)]).actions);
        }
        assert_eq!(actions.len(), 2, "{actions:?}");
        assert!(matches!(actions[0], Action::Freeze { .. }));
        assert!(matches!(
            actions[1],
            Action::Kill {
                reason: KillReason::GraceExpired,
                ..
            }
        ));
    }

    #[test]
    fn exited_throttled_app_pruned() {
        let mut sim = Sim::new();
        sim.warm(10, |t| vec![leak(t)]);
        let out = sim.step(warning(11_000), &[leak(11_000)]);
        assert!(matches!(&out.actions[..], [Action::Throttle { .. }]));
        assert_eq!(sim.eng.throttled().len(), 1);
        let out = sim.step(warning(12_000), &[]);
        assert!(out.actions.is_empty());
        assert!(out.events.is_empty(), "{:?}", out.events);
        assert!(sim.eng.throttled().is_empty());
    }

    #[test]
    fn irregular_gap_after_freeze() {
        let mut sim = Sim::new();
        sim.warm(10, |t| vec![leak(t)]);
        freeze_id(&sim.step(critical(11_000), &[leak(11_000)]));
        let out = sim.step(critical(81_000), &[leak(81_000)]);
        assert_eq!(
            kills(&out),
            vec![("leaky".to_string(), KillReason::GraceExpired)]
        );
        assert_eq!(out.actions.len(), 1);
        assert!(notices(&out).is_empty(), "{:?}", out.events);
    }

    /// `full_avg10` and the instantaneous `full_now` given separately.
    fn sys_now(t: u64, full: f32, full_now: f32, avail_pct: f32) -> SystemSample {
        SystemSample {
            full_now: Some(full_now),
            ..sys(t, 0.0, full, avail_pct)
        }
    }

    fn other(t: u64) -> AppSample {
        app(
            "other",
            "/other",
            TargetKind::Cgroup,
            8 * GIB + t * (GIB / 20) / 1000,
        )
    }

    #[test]
    fn no_second_kill_while_avg10_lags() {
        let mut sim = Sim::new();
        sim.warm(10, |t| vec![leak(t), other(t)]);
        let out = sim.step(emergency(11_000), &[leak(11_000), other(11_000)]);
        assert_eq!(
            kills(&out),
            vec![("leaky".to_string(), KillReason::Emergency)]
        );
        // leaky is gone. avg10 lags at 55 for a long while, but the stall since the last
        // sample is down to 5%: the kill worked, so "other" must not be killed.
        for s in 12..=60 {
            let t = s * 1000;
            let out = sim.step(sys_now(t, 55.0, 5.0, 50.0), &[other(t)]);
            assert!(kills(&out).is_empty(), "t {t}: {:?}", out.actions);
        }
        assert_eq!(sim.eng.level(), Level::Emergency);
        // If the stall really climbs back to emergency, olly does move on to "other".
        let out = sim.step(sys_now(61_000, 55.0, 60.0, 50.0), &[other(61_000)]);
        assert_eq!(
            kills(&out),
            vec![("other".to_string(), KillReason::Emergency)]
        );
    }

    #[test]
    fn no_second_kill_once_memory_came_back() {
        let mut sim = Sim::new();
        sim.warm(10, |t| vec![leak(t), other(t)]);
        let out = sim.step(sys(11_000, 0.0, 30.0, 1.5), &[leak(11_000), other(11_000)]);
        assert_eq!(kills(&out).len(), 1);
        // Memory came back (1.5% -> 30%) but swap-in keeps the stall high: no second kill.
        for s in 12..=40 {
            let t = s * 1000;
            let out = sim.step(sys_now(t, 55.0, 70.0, 30.0), &[other(t)]);
            assert!(kills(&out).is_empty(), "t {t}: {:?}", out.actions);
        }
    }

    #[test]
    fn second_kill_waits_for_target_gone_and_settle() {
        let mut sim = Sim::new();
        sim.warm(10, |t| vec![leak(t), other(t)]);
        let out = sim.step(emergency(11_000), &[leak(11_000), other(11_000)]);
        assert_eq!(
            kills(&out),
            vec![("leaky".to_string(), KillReason::Emergency)]
        );
        // Still at emergency by every measure. leaky takes until 15s to die, so the
        // settle window runs from 15s, and "other" is only killed at 25s.
        let mut kill_times = Vec::new();
        for s in 12..=40 {
            let t = s * 1000;
            let apps = if s < 15 {
                vec![leak(11_000), other(t)]
            } else {
                vec![other(t)]
            };
            for k in kills(&sim.step(emergency(t), &apps)) {
                kill_times.push((t, k));
            }
            if s >= 25 {
                break;
            }
        }
        assert_eq!(
            kill_times,
            vec![(25_000, ("other".to_string(), KillReason::Emergency))]
        );
    }

    #[test]
    fn failed_kill_does_not_block_forever() {
        let mut sim = Sim::new();
        sim.warm(10, |t| vec![leak(t), other(t)]);
        let out = sim.step(emergency(11_000), &[leak(11_000), other(11_000)]);
        let id = match &out.actions[..] {
            [Action::Kill { target, .. }] => target.id,
            a => panic!("expected kill, got {a:?}"),
        };
        // The actuator could not kill it, so it will never go away by itself.
        sim.eng.kill_failed(id, 11_000);
        let mut kill_times = Vec::new();
        for s in 12..=30 {
            let t = s * 1000;
            for k in kills(&sim.step(emergency(t), &[leak(t), other(t)])) {
                kill_times.push((t, k));
            }
        }
        // Settle still applies; the stuck target is not picked again.
        assert_eq!(
            kill_times,
            vec![(21_000, ("other".to_string(), KillReason::Emergency))]
        );
    }

    #[test]
    fn killed_target_that_never_leaves_does_not_block_forever() {
        let mut sim = Sim::new();
        sim.warm(10, |t| vec![leak(t), other(t)]);
        let out = sim.step(emergency(11_000), &[leak(11_000), other(11_000)]);
        assert_eq!(kills(&out).len(), 1);
        // The kill "worked" but the key stays (a restarted service, an unreaped zombie).
        let mut kill_times = Vec::new();
        for s in 12..=60 {
            let t = s * 1000;
            for k in kills(&sim.step(emergency(t), &[leak(t), other(t)])) {
                kill_times.push((t, k));
            }
        }
        // Counted as stuck at 11s + 15s, then the 10s settle.
        assert_eq!(
            kill_times,
            vec![(36_000, ("other".to_string(), KillReason::Emergency))]
        );
    }

    #[test]
    fn two_frozen_targets_not_killed_in_one_go() {
        let mut sim = Sim::new();
        sim.warm(10, |t| vec![leak(t), other(t)]);
        let mut frozen = 0;
        for s in 11..=16 {
            let t = s * 1000;
            let out = sim.step(sys(t, 0.0, 20.0, 4.0), &[leak(t), other(t)]);
            frozen += out.actions.len();
        }
        assert_eq!(frozen, 2);
        let out = sim.step(emergency(17_000), &[leak(17_000), other(17_000)]);
        assert_eq!(kills(&out).len(), 1, "{:?}", out.actions);
    }

    #[test]
    fn last_resort_capped_per_incident() {
        let mut sim = Sim::new();
        let all = [
            ("a", 12 * GIB),
            ("b", 10 * GIB),
            ("c", 8 * GIB),
            ("d", 6 * GIB),
        ];
        let mut alive: Vec<&str> = all.iter().map(|x| x.0).collect();
        let apps = |alive: &[&str]| -> Vec<AppSample> {
            all.iter()
                .filter(|x| alive.contains(&x.0))
                .map(|&(n, b)| app(n, &format!("/{n}"), TargetKind::Cgroup, b))
                .collect()
        };
        sim.warm(10, |_| apps(&alive));
        let mut kill_times = Vec::new();
        let mut capped = Vec::new();
        for s in 11..=200 {
            let t = s * 1000;
            let out = sim.step(emergency(t), &apps(&alive));
            for (name, reason) in kills(&out) {
                alive.retain(|n| *n != name);
                kill_times.push((t, name, reason));
            }
            for (m, _) in notices(&out) {
                capped.push((t, m));
            }
        }
        assert_eq!(
            kill_times,
            vec![
                (11_000, "a".to_string(), KillReason::LastResort),
                (22_000, "b".to_string(), KillReason::LastResort),
            ]
        );
        assert_eq!(capped, vec![(33_000, voice::last_resort_cap())]);
        assert_eq!(alive, vec!["c", "d"]);

        // A clear culprit is still closed after the cap, once it counts as growing.
        let mut after = Vec::new();
        for s in 201..=215 {
            let t = s * 1000;
            let mut v = apps(&alive);
            v.push(leak(t));
            after.extend(kills(&sim.step(emergency(t), &v)));
        }
        assert_eq!(after, vec![("leaky".to_string(), KillReason::Emergency)]);
    }

    #[test]
    fn last_resort_skips_session_slice() {
        let session =
            "/user.slice/user-1000.slice/user@1000.service/session.slice/plasma-foo.service";
        let apps = |with_app: bool| {
            let mut v = vec![app("plasma-foo", session, TargetKind::Cgroup, 14 * GIB)];
            if with_app {
                v.push(app(
                    "big",
                    "/user.slice/user-1000.slice/user@1000.service/app.slice/app-big.scope",
                    TargetKind::Cgroup,
                    6 * GIB,
                ));
            }
            v
        };
        let mut sim = Sim::new();
        sim.warm(10, |_| apps(true));
        let out = sim.step(emergency(11_000), &apps(true));
        assert_eq!(
            kills(&out),
            vec![("big".to_string(), KillReason::LastResort)]
        );

        let mut sim = Sim::new();
        sim.warm(10, |_| apps(false));
        for s in 11..=40 {
            let out = sim.step(emergency(s * 1000), &apps(false));
            assert!(kills(&out).is_empty(), "step {s}: {:?}", out.actions);
        }
    }

    #[test]
    fn throttle_high_uses_ram_not_swap() {
        let mut sim = Sim::new();
        // 4 GiB of the app is in zram swap; memory.high only limits the RAM part.
        let swapped = |t: u64| {
            let mut a = leak(t);
            a.ram = a.bytes - 4 * GIB;
            a
        };
        sim.warm(10, |t| vec![swapped(t)]);
        let a = swapped(11_000);
        let out = sim.step(warning(11_000), std::slice::from_ref(&a));
        match &out.actions[..] {
            [Action::Throttle { high, .. }] => {
                assert_eq!(*high, (a.ram * 105 / 100).max(a.ram + 128 * MIB))
            }
            x => panic!("expected throttle, got {x:?}"),
        }
    }

    #[test]
    fn incident_closes_while_at_watch() {
        let mut sim = Sim::new();
        sim.warm(10, |t| vec![leak(t)]);
        let out = sim.step(warning(11_000), &[leak(11_000)]);
        assert!(matches!(&out.actions[..], [Action::Throttle { .. }]));
        // A busy game keeps available memory at 12%: Watch, never Calm.
        let stable = leak(11_000);
        let mut ended_at = None;
        for s in 12..=200 {
            let t = s * 1000;
            let out = sim.step(sys(t, 0.0, 0.0, 12.0), std::slice::from_ref(&stable));
            assert_eq!(sim.eng.level(), Level::Watch);
            if out
                .events
                .iter()
                .any(|e| matches!(e, Event::IncidentEnd { .. }))
            {
                assert!(matches!(&out.actions[..], [Action::Unthrottle { .. }]));
                ended_at = Some(t);
                break;
            }
        }
        assert_eq!(ended_at, Some(72_000));
        // "some" above calm_some keeps it open (hysteresis).
        let mut sim = Sim::new();
        sim.warm(10, |t| vec![leak(t)]);
        sim.step(warning(11_000), &[leak(11_000)]);
        for s in 12..=200 {
            let out = sim.step(sys(s * 1000, 7.0, 0.0, 50.0), std::slice::from_ref(&stable));
            assert!(!out
                .events
                .iter()
                .any(|e| matches!(e, Event::IncidentEnd { .. })));
        }
    }

    #[test]
    fn target_ids_start_from_seed() {
        let mut sim = Sim::new();
        sim.eng.set_next_target_id(1_761_550_000_000);
        sim.warm(10, |t| vec![leak(t)]);
        assert_eq!(
            freeze_id(&sim.step(critical(11_000), &[leak(11_000)])),
            1_761_550_000_000
        );
    }

    #[test]
    fn incident_stays_open_while_frozen() {
        let mut sim = Sim::new();
        sim.warm(10, |t| vec![leak(t)]);
        let id = freeze_id(&sim.step(critical(11_000), &[leak(11_000)]));
        // Grace at 21s: pressure has fallen, so the target is spared. Calm from 22s.
        let out = sim.step(warning(21_000), &[leak(21_000)]);
        assert!(out.actions.is_empty());
        for s in 22..=150 {
            let out = sim.step(calm(s * 1000), &[leak(s * 1000)]);
            assert!(out.actions.is_empty(), "step {s}: {:?}", out.actions);
            assert!(!out
                .events
                .iter()
                .any(|e| matches!(e, Event::IncidentEnd { .. })));
        }
        assert_eq!(sim.eng.frozen().len(), 1);
        sim.eng.command(Command::ResumeFrozen(id), 150_500).unwrap();
        let out = sim.step(calm(151_000), &[leak(151_000)]);
        assert!(out
            .events
            .iter()
            .any(|e| matches!(e, Event::IncidentEnd { id: 1, .. })));
    }
}
