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
    Level { level: Level },
    IncidentStart { id: u64, t_ms: u64 },
    IncidentEnd { id: u64, t_ms: u64 },
    Acted { action: Action, message: Message, dry_run: bool },
    /// Reminders, all-clear, hunches. `frozen_id` => offer Kill/Resume.
    Notice { message: Message, frozen_id: Option<u64> },
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
    Pause { until_ms: u64 },
    Unpause,
}

const MIB: u64 = 1024 * 1024;

/// Highest level whose full-PSI or available-memory threshold is met.
pub fn classify(s: &SystemSample, t: &Thresholds) -> Level {
    let full = s.psi.full_avg10;
    let avail = if s.mem.total == 0 {
        100.0
    } else {
        (s.mem.available as f64 * 100.0 / s.mem.total as f64) as f32
    };
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

struct FrozenState {
    target: Target,
    at_ms: u64,
    full_at_freeze: f32,
    grace_checked: bool,
    reminded: bool,
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
        }
    }

    pub fn set_next_incident_id(&mut self, id: u64) {
        self.next_incident_id = id;
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
        if level == Level::Calm && sys.psi.some_avg10 < self.cfg.thresholds.calm_some {
            self.calm_since.get_or_insert(now);
        } else {
            self.calm_since = None;
        }
        let close = self.incident.is_some()
            && self.frozen.is_empty()
            && self
                .calm_since
                .is_some_and(|c| now.saturating_sub(c) >= self.cfg.timing.calm_end_secs * 1000);

        // 2. Frozen targets.
        let tm = &self.cfg.timing;
        let (grace, remind, giveup) =
            (tm.freeze_grace_secs * 1000, tm.freeze_remind_secs * 1000, tm.freeze_giveup_secs * 1000);
        let mut frozen = std::mem::take(&mut self.frozen);
        frozen.retain_mut(|f| {
            let Some(a) = find(&f.target.key) else { return false };
            f.target.bytes = a.bytes;
            let elapsed = now.saturating_sub(f.at_ms);
            let mut reason = None;
            if level == Level::Emergency {
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
                !allow.allows_app(a) && !self.is_frozen(&a.key) && !self.dry_killed.contains(&a.key)
            })
            .map(|a| {
                let gentle = !self.resumed.contains(&a.key);
                (a, hist.rate(&a.key, now), hist.recently_growing(&a.key, now), gentle)
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
                    if !growing || !gentle || a.kind != TargetKind::Cgroup || self.is_throttled(&a.key) {
                        continue;
                    }
                    let target = self.target_for(a);
                    let high = (a.bytes * 105 / 100).max(a.bytes + 128 * MIB);
                    let message = voice::throttled(&a.name, (rate * 60.0) as u64, dry);
                    self.throttled.push(target.clone());
                    self.act(&mut out, Action::Throttle { target, high }, message);
                }
            }
            Level::Critical if !paused && cooled => {
                if let Some(&(a, ..)) = cands.iter().find(|c| c.2 && c.3) {
                    let target = self.target_for(a);
                    self.frozen.push(FrozenState {
                        target: target.clone(),
                        at_ms: now,
                        full_at_freeze: sys.psi.full_avg10,
                        grace_checked: false,
                        reminded: false,
                    });
                    self.last_action_ms = Some(now);
                    let message = voice::frozen(&a.name, self.cfg.timing.freeze_grace_secs, dry);
                    self.act(&mut out, Action::Freeze { target }, message);
                }
            }
            Level::Emergency if cooled => {
                let pick = match top_growing {
                    Some(a) => Some((a, KillReason::Emergency)),
                    None => cands.iter().map(|c| c.0).max_by_key(|a| a.bytes).map(|a| (a, KillReason::LastResort)),
                };
                if let Some((a, reason)) = pick {
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
            if let Some(id) = self.incident.take() {
                out.events.push(Event::IncidentEnd { id, t_ms: now });
                out.events.push(Event::Notice { message: voice::all_clear(), frozen_id: None });
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
        Target { id, key: a.key.clone(), name: a.name.clone(), kind: a.kind.clone(), bytes: a.bytes }
    }

    fn take_frozen(&mut self, id: u64) -> Result<Target, String> {
        let i = self
            .frozen
            .iter()
            .position(|f| f.target.id == id)
            .ok_or_else(|| format!("no frozen target with id {id}"))?;
        Ok(self.frozen.remove(i).target)
    }

    /// Kill `target`: forget it everywhere and start the cooldown.
    fn kill(&mut self, out: &mut StepOutput, target: Target, reason: KillReason, now: u64) {
        self.frozen.retain(|f| f.target.key != target.key);
        self.throttled.retain(|t| t.key != target.key);
        self.last_action_ms = Some(now);
        if self.cfg.dry_run {
            self.dry_killed.push(target.key.clone());
        }
        let message = voice::killed(&target.name, target.bytes, reason, self.cfg.dry_run);
        self.act(out, Action::Kill { target, reason }, message);
    }

    fn act(&self, out: &mut StepOutput, action: Action, message: Message) {
        out.events.push(Event::Acted { action: action.clone(), message, dry_run: self.cfg.dry_run });
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
            psi: Psi { some_avg10: some, full_avg10: full },
            mem: MemInfo { total: TOTAL, available: (TOTAL as f64 * avail_pct as f64 / 100.0) as u64 },
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
            key: AppKey { cgroup: key.into(), root_pid: None },
            name: name.into(),
            exe: None,
            kind,
            bytes,
            names: vec![name.into()],
        }
    }

    fn procs() -> TargetKind {
        TargetKind::Processes { pids: vec![(500, 1500)] }
    }

    /// Cgroup app growing 1 GiB per 10s, starting at 10 GiB.
    fn leak(t_ms: u64) -> AppSample {
        app("leaky", "/leaky", TargetKind::Cgroup, 10 * GIB + t_ms * (GIB / 10) / 1000)
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
            let allow = Allowlist::load(cfg.allowlist.clone(), PathBuf::from("/nonexistent/olly/a.toml"));
            Sim { eng: Engine::new(cfg), hist, allow }
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
        assert_eq!(sim.eng.frozen().iter().map(|t| t.id).collect::<Vec<_>>(), vec![id]);

        for s in 13..22 {
            let out = sim.step(critical(s * 1000), &[leak(s * 1000)]);
            assert!(out.actions.is_empty(), "step {s}: {:?}", out.actions);
        }
        let out = sim.step(critical(22_000), &[leak(22_000)]);
        match &out.actions[..] {
            [Action::Kill { target, reason: KillReason::GraceExpired }] => assert_eq!(target.id, id),
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
        assert_eq!(kills(&out), vec![("leaky".to_string(), KillReason::NoAnswer)]);
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
            [Action::Kill { target, reason: KillReason::Emergency }] => {
                assert_eq!(target.id, id);
                assert_eq!(target.bytes, leak(12_000).bytes);
            }
            a => panic!("expected emergency kill, got {a:?}"),
        }
        assert!(sim.eng.frozen().is_empty());
    }

    #[test]
    fn steam_game_frozen_not_steam() {
        let base = Sampler::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/steam"), 1000)
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
        let grow = |t: u64| vec![app("proc", "/proc", procs(), 10 * GIB + t * (GIB / 10) / 1000)];
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
            let mut kwin = app("kwin_wayland", "/kwin", TargetKind::Cgroup, 4 * GIB + t * (GIB / 5) / 1000);
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
        assert_eq!(kills(&out), vec![("big".to_string(), KillReason::LastResort)]);
        assert_eq!(out.actions.len(), 1);
        let Event::Acted { message, dry_run, .. } = out.events.last().unwrap() else {
            panic!("expected acted event last")
        };
        assert!(!dry_run);
        assert_eq!(*message, voice::killed("big", 12 * GIB, KillReason::LastResort, false));
    }

    #[test]
    fn pause_blocks_freeze_not_emergency() {
        let mut sim = Sim::new();
        sim.warm(10, |t| vec![leak(t)]);
        let out = sim.eng.command(Command::Pause { until_ms: 100_000 }, 10_500).unwrap();
        assert_eq!(out, StepOutput::default());
        assert_eq!(sim.eng.paused_until(), Some(100_000));
        for s in 11..=20 {
            let t = s * 1000;
            let s = if s < 15 { warning(t) } else { critical(t) };
            let out = sim.step(s, &[leak(t)]);
            assert!(out.actions.is_empty(), "t {t}: {:?}", out.actions);
        }
        let out = sim.step(emergency(21_000), &[leak(21_000)]);
        assert_eq!(kills(&out), vec![("leaky".to_string(), KillReason::Emergency)]);
        assert_eq!(sim.eng.paused_until(), Some(100_000));

        sim.step(calm(100_000), &[]);
        assert_eq!(sim.eng.paused_until(), None);

        sim.eng.command(Command::Pause { until_ms: 500_000 }, 101_000).unwrap();
        sim.eng.command(Command::Unpause, 102_000).unwrap();
        assert_eq!(sim.eng.paused_until(), None);
    }

    #[test]
    fn cooldown_spaces_kills() {
        let mut sim = Sim::new();
        let other = |t: u64| app("other", "/other", TargetKind::Cgroup, 8 * GIB + t * (GIB / 20) / 1000);
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
        assert_eq!(
            kill_times,
            vec![(11_000, "leaky".to_string()), (16_000, "other".to_string())]
        );
    }

    #[test]
    fn incident_end_unthrottles_and_all_clear() {
        let mut sim = Sim::new();
        sim.warm(10, |t| vec![leak(t)]);
        let out = sim.step(warning(11_000), &[leak(11_000)]);
        assert_eq!(out.events[0], Event::Level { level: Level::Warning });
        assert_eq!(out.events[1], Event::IncidentStart { id: 1, t_ms: 11_000 });
        let tid = match &out.actions[..] {
            [Action::Throttle { target, .. }] => target.id,
            a => panic!("expected throttle, got {a:?}"),
        };

        // Calm starts at 12s; the incident closes once calm has lasted 60s.
        let stable = leak(11_000);
        for s in 12..72 {
            let out = sim.step(calm(s * 1000), std::slice::from_ref(&stable));
            assert!(out.actions.is_empty(), "step {s}: {:?}", out.actions);
            assert!(!out.events.iter().any(|e| matches!(e, Event::IncidentEnd { .. })));
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
        assert_eq!(out.events[1], Event::IncidentEnd { id: 1, t_ms: 72_000 });
        assert_eq!(out.events[2], Event::Notice { message: voice::all_clear(), frozen_id: None });
        assert!(sim.eng.throttled().is_empty());

        // A new incident gets the next id.
        let out = sim.step(warning(80_000), &[stable]);
        assert!(out.events.contains(&Event::IncidentStart { id: 2, t_ms: 80_000 }));
    }

    #[test]
    fn incident_ids_resume_from_set_value() {
        let mut sim = Sim::new();
        sim.eng.set_next_incident_id(42);
        let out = sim.step(critical(1000), &[]);
        assert!(out.events.contains(&Event::IncidentStart { id: 42, t_ms: 1000 }));
    }

    #[test]
    fn command_kill_and_resume_frozen() {
        let mut sim = Sim::new();
        let other = |t: u64| app("other", "/other", TargetKind::Cgroup, 8 * GIB + t * (GIB / 20) / 1000);
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

        let out = sim.eng.command(Command::KillFrozen(ids[0].0), 17_000).unwrap();
        match &out.actions[..] {
            [Action::Kill { target, reason: KillReason::User }] => assert_eq!(target.id, ids[0].0),
            a => panic!("expected user kill, got {a:?}"),
        }
        let out = sim.eng.command(Command::ResumeFrozen(ids[1].0), 17_000).unwrap();
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
        assert!(sim.eng.command(Command::ResumeFrozen(ids[0].0), 17_000).is_err());
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
        assert_eq!(kills(&out), vec![("leaky".to_string(), KillReason::Emergency)]);
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
            ended |= out.events.iter().any(|e| matches!(e, Event::IncidentEnd { .. }));
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
        assert!(matches!(actions[1], Action::Kill { reason: KillReason::GraceExpired, .. }));
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
        assert_eq!(kills(&out), vec![("leaky".to_string(), KillReason::GraceExpired)]);
        assert_eq!(out.actions.len(), 1);
        assert!(notices(&out).is_empty(), "{:?}", out.events);
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
            assert!(!out.events.iter().any(|e| matches!(e, Event::IncidentEnd { .. })));
        }
        assert_eq!(sim.eng.frozen().len(), 1);
        sim.eng.command(Command::ResumeFrozen(id), 150_500).unwrap();
        let out = sim.step(calm(151_000), &[leak(151_000)]);
        assert!(out.events.iter().any(|e| matches!(e, Event::IncidentEnd { id: 1, .. })));
    }
}
