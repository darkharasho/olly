//! The main loop: wires sampling, the engine, the actuator, incidents and IPC.

use crate::actuator::{self, Actuator, DryRunActuator, SysActuator};
use crate::allowlist::Allowlist;
use crate::claude::{ClaudeRunner, Job};
use crate::config::Config;
use crate::engine::{Action, Command, Engine, Event, FrozenView, Level, StepOutput};
use crate::history::{History, LeakWatch};
use crate::incident::{IncidentRecorder, IncidentStore, TopApp};
use crate::ipc::{self, err_line, ok_line, FrozenInfo, Request, StatusData, Subscribers, ToDaemon};
use crate::procfs::parse_status;
use crate::psi::{self, PsiTrigger};
use crate::sample::{AppSample, Sampler, SystemSample};
use crate::voice;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PSI_SPEC: &str = "some 150000 1000000";
const MAX_INCIDENTS: usize = 100;
/// The same error is logged at most once per this many ms.
const LOG_REPEAT_MS: u64 = 60_000;
/// The kernel fires a trigger at most once per 1 s window; more than this is a broken fd.
const PSI_STORM: u32 = 20;
/// At most this many Claude jobs wait; the oldest is dropped first.
const CLAUDE_QUEUE: usize = 4;
/// A leaking cgroup is handed to Claude at most once per day.
const LEAK_COOLDOWN_MS: u64 = 24 * 60 * 60 * 1000;

struct Paths {
    run_dir: PathBuf,
    state_dir: PathBuf,
    sock: PathBuf,
    active: PathBuf,
    incidents: PathBuf,
    allowlist: PathBuf,
    /// OLLY_RUN_DIR is set (tests, manual runs).
    overridden: bool,
}

impl Paths {
    fn from_env() -> Self {
        let (run_dir, state_dir, overridden) = match std::env::var_os("OLLY_RUN_DIR") {
            Some(d) => (PathBuf::from(&d), PathBuf::from(d), true),
            None => (
                PathBuf::from("/run/olly"),
                PathBuf::from("/var/lib/olly"),
                false,
            ),
        };
        Paths {
            sock: run_dir.join("olly.sock"),
            active: run_dir.join("active.json"),
            incidents: state_dir.join("incidents.jsonl"),
            allowlist: state_dir.join("allowlist.toml"),
            run_dir,
            state_dir,
            overridden,
        }
    }
}

fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn level_name(l: Level) -> String {
    format!("{l:?}").to_lowercase()
}

fn self_rss() -> u64 {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| parse_status(&s))
        .map_or(0, |s| s.rss)
}

/// Logs a given repeated error at most once per `LOG_REPEAT_MS`.
#[derive(Default)]
struct RateLog {
    last: HashMap<&'static str, u64>,
}

impl RateLog {
    fn log(&mut self, key: &'static str, now: u64, msg: &str) {
        if self
            .last
            .get(key)
            .is_none_or(|&t| now.saturating_sub(t) >= LOG_REPEAT_MS)
        {
            self.last.insert(key, now);
            eprintln!("olly: {msg}");
        }
    }
}

enum Act {
    Dry(DryRunActuator),
    Sys(SysActuator),
}

impl Act {
    fn apply(&mut self, a: &Action) -> Result<(), String> {
        match self {
            Act::Dry(d) => {
                let r = d.apply(a);
                // The daemon logs actions itself; don't let the record grow forever.
                d.log.clear();
                r
            }
            Act::Sys(s) => s.apply(a),
        }
    }

    /// Undoes freezes and throttles the engine has dropped. Dry run: nothing to undo.
    fn reconcile(&mut self, engine: &Engine, rlog: &mut RateLog, now: u64) {
        let Act::Sys(s) = self else { return };
        let frozen: Vec<u64> = engine.frozen().iter().map(|t| t.id).collect();
        let throttled: Vec<u64> = engine.throttled().iter().map(|t| t.id).collect();
        for line in s.reconcile(&frozen, &throttled) {
            if line.starts_with("could not") {
                rlog.log("reconcile", now, &format!("reconcile: {line}"));
            } else {
                eprintln!("olly: reconcile: {line}");
            }
        }
    }

    /// Safety net at IncidentEnd: the actuator's state is the truth about what is applied.
    fn restore_throttles(&mut self) {
        if let Act::Sys(s) = self {
            for line in s.restore_throttles() {
                eprintln!("olly: {line}");
            }
        }
    }

    fn undo_all(&mut self) {
        if let Act::Sys(s) = self {
            actuator::undo_all(s);
        }
    }
}

fn action_parts(a: &Action) -> (&'static str, &str) {
    match a {
        Action::Throttle { target, .. } => ("throttle", &target.name),
        Action::Unthrottle { target } => ("unthrottle", &target.name),
        Action::Freeze { target } => ("freeze", &target.name),
        Action::Resume { target } => ("resume", &target.name),
        Action::Kill { target, .. } => ("kill", &target.name),
    }
}

struct Daemon {
    cfg: Config,
    start: Instant,
    sampler: Sampler,
    hist: History,
    leak: LeakWatch,
    allow: Allowlist,
    engine: Engine,
    act: Act,
    recorder: IncidentRecorder,
    store: IncidentStore,
    subs: Subscribers,
    /// Event lines every new subscriber gets first (e.g. the config-fallback notice).
    pending: Vec<String>,
    last_sys: Option<SystemSample>,
    incident_open: bool,
    /// The biggest app in the latest sample.
    last_top: Option<TopApp>,
    /// Wall time of the latest action olly took.
    last_action_wall_ms: Option<u64>,
    /// Whether olly has acted during the open incident.
    incident_acted: bool,
    /// The newest incident id on record, if any.
    last_incident_id: Option<u64>,
    rlog: RateLog,
    /// None when Claude is disabled or the desktop user can't be found.
    claude: Option<ClaudeRunner>,
    claude_queue: VecDeque<Job>,
    claude_running: bool,
    claude_tx: mpsc::Sender<(Job, Result<String, String>)>,
    claude_rx: mpsc::Receiver<(Job, Result<String, String>)>,
    /// cgroup -> monotonic ms when it was last handed to Claude.
    leak_seen: HashMap<String, u64>,
}

/// Adds a job, dropping the oldest when the queue is full.
fn enqueue(queue: &mut VecDeque<Job>, job: Job) {
    if queue.len() >= CLAUDE_QUEUE {
        queue.pop_front();
    }
    queue.push_back(job);
}

/// A queued Claude job may start once memory has recovered and no incident is open.
fn may_start(incident_open: bool, avail_pct: f64, recovered_avail: f64) -> bool {
    !incident_open && avail_pct >= recovered_avail
}

fn prune_cooldown(seen: &mut HashMap<String, u64>, now: u64) {
    seen.retain(|_, &mut t| now.saturating_sub(t) < LEAK_COOLDOWN_MS);
}

fn in_cooldown(seen: &HashMap<String, u64>, cgroup: &str, now: u64) -> bool {
    seen.get(cgroup)
        .is_some_and(|&t| now.saturating_sub(t) < LEAK_COOLDOWN_MS)
}

/// Delay until the next sample.
/// Without a PSI trigger nothing wakes olly early, so even idle it samples about every 2s.
fn tick_ms(active: bool, psi_ok: bool, active_ms: u64, slow_ms: u64) -> u64 {
    if active {
        active_ms
    } else if psi_ok {
        slow_ms
    } else {
        (2 * active_ms).min(slow_ms)
    }
}

/// What the tray shows for each frozen app; the kill deadline is in wall time.
fn frozen_info(
    views: &[FrozenView],
    now_mono: u64,
    now_wall: u64,
    giveup_secs: u64,
) -> Vec<FrozenInfo> {
    views
        .iter()
        .map(|v| FrozenInfo {
            id: v.target.id,
            name: v.target.name.clone(),
            bytes: v.target.bytes,
            rate: v.rate,
            kill_at_wall_ms: now_wall.saturating_sub(now_mono.saturating_sub(v.at_ms))
                + giveup_secs * 1000,
        })
        .collect()
}

/// The biggest app in a sample, with its growth rate.
fn top_app(apps: &[AppSample], hist: &History, now: u64) -> Option<TopApp> {
    apps.iter().max_by_key(|a| a.bytes).map(|a| TopApp {
        name: a.name.clone(),
        key: a.key.clone(),
        bytes: a.bytes,
        rate: hist.rate(&a.key, now),
    })
}

/// An intervention (throttle, freeze, kill), not the undoing of one.
fn is_intervention(a: &Action) -> bool {
    matches!(
        a,
        Action::Throttle { .. } | Action::Freeze { .. } | Action::Kill { .. }
    )
}

/// Whether the open incident has had an intervention, after `ev`.
fn acted_after(acted: bool, ev: &Event, incident_open: bool) -> bool {
    match ev {
        Event::Acted { action, .. } => acted || (incident_open && is_intervention(action)),
        Event::IncidentStart { .. } | Event::IncidentEnd { .. } => false,
        _ => acted,
    }
}

/// The notification line: the first paragraph, on one line.
fn first_paragraph(text: &str) -> String {
    let first = text.trim().split("\n\n").next().unwrap_or("");
    first.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Daemon {
    fn now(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    /// Fast cadence: throttle/freeze/kill decisions need short sample intervals.
    fn active(&self) -> bool {
        self.engine.level() >= Level::Watch
            || self.incident_open
            || !self.engine.throttled().is_empty()
            || !self.engine.frozen().is_empty()
    }

    fn tick(&mut self, now: u64) {
        let Some(sys) = self.sampler.system(now) else {
            self.rlog.log(
                "system",
                now,
                "cannot read /proc/pressure/memory or /proc/meminfo; skipping this sample",
            );
            return;
        };
        let apps = self.sampler.apps();
        self.hist.record(now, &apps);
        self.last_top = top_app(&apps, &self.hist, now);
        let out = self.engine.step(&sys, &apps, &self.hist, &self.allow);
        self.process(out);
        self.recorder.on_sample(&sys, &apps, &self.hist, wall_ms());
        self.last_sys = Some(sys);
    }

    fn slow(&mut self, now: u64) {
        let totals = self.sampler.cgroup_totals();
        self.leak.record(now, &totals);
        let suspects = self.leak.suspects(now);
        if !suspects.is_empty() {
            self.on_suspects(suspects);
        }
    }

    /// Slow-leak suspects: (cgroup, growth in bytes over the leak window).
    /// Each cgroup is handed to Claude at most once per 24h.
    fn on_suspects(&mut self, suspects: Vec<(String, u64)>) {
        let now = self.now();
        prune_cooldown(&mut self.leak_seen, now);
        for (cgroup, growth) in suspects {
            eprintln!(
                "olly: event=leak_suspect target={cgroup} growth={}",
                voice::fmt_bytes(growth)
            );
            if self.claude.is_none() || in_cooldown(&self.leak_seen, &cgroup, now) {
                continue;
            }
            self.leak_seen.insert(cgroup.clone(), now);
            enqueue(&mut self.claude_queue, Job::Leak { cgroup, growth });
        }
    }

    /// Never blocks: collects finished Claude answers, then starts the next queued job
    /// if memory has recovered, no incident is open, and today's budget allows.
    fn pump_claude(&mut self) {
        while let Ok((job, result)) = self.claude_rx.try_recv() {
            self.claude_running = false;
            self.on_claude_result(job, result);
        }
        if self.claude_running || self.claude_queue.is_empty() {
            return;
        }
        let Some(sys) = self.last_sys else { return };
        let Some(runner) = self.claude.as_mut() else {
            self.claude_queue.clear();
            return;
        };
        let avail_pct = if sys.mem.total == 0 {
            0.0
        } else {
            sys.mem.available as f64 / sys.mem.total as f64 * 100.0
        };
        if !may_start(
            self.incident_open,
            avail_pct,
            runner.recovered_avail() as f64,
        ) || !runner.allowed(wall_ms())
        {
            return;
        }
        if let Some(job) = self.claude_queue.pop_front() {
            self.claude_running = true;
            runner.spawn(job, self.claude_tx.clone());
        }
    }

    fn on_claude_result(&mut self, job: Job, result: Result<String, String>) {
        let what = match &job {
            Job::Diagnose(inc) => format!("incident {}", inc.id),
            Job::Leak { cgroup, .. } => format!("leak in {cgroup}"),
        };
        let text = match result {
            Ok(t) => t,
            Err(e) => {
                eprintln!("olly: claude could not look at {what}: {e}");
                return;
            }
        };
        eprintln!("olly: claude looked at {what}");
        if let Job::Diagnose(inc) = &job {
            if let Err(e) = self.store.append_diagnosis(inc.id, &text) {
                eprintln!(
                    "olly: cannot save the diagnosis for incident {}: {e}",
                    inc.id
                );
            }
        }
        let ev = Event::Notice {
            message: voice::hunch(&first_paragraph(&text)),
            frozen_id: None,
        };
        self.subs.broadcast(&ipc::event_line(&ev, wall_ms()));
    }

    /// Applies the actions, then logs, records and broadcasts the events. A failed
    /// action's `Acted` event becomes a `Notice` saying olly couldn't act.
    fn process(&mut self, out: StepOutput) {
        let results: Vec<Result<(), String>> =
            out.actions.iter().map(|a| self.act.apply(a)).collect();
        let now = self.now();
        for (a, r) in out.actions.iter().zip(&results) {
            if let (Action::Kill { target, .. }, Err(_)) = (a, r) {
                self.engine.kill_failed(target.id, now);
            }
        }
        // Anything the engine let go of (a failed kill, a regrouped key) is undone.
        self.act.reconcile(&self.engine, &mut self.rlog, now);
        // The engine pushes one Acted event per action, in the same order.
        let mut results = results.into_iter();
        let wall = wall_ms();
        let level = level_name(self.engine.level());
        for ev in out.events {
            let ev = match ev {
                Event::Acted {
                    action,
                    message,
                    dry_run,
                } => {
                    let (verb, name) = action_parts(&action);
                    match results.next().unwrap_or(Ok(())) {
                        Ok(()) => {
                            eprintln!(
                                "olly: level={level} action={verb} target={name} dry_run={dry_run}"
                            );
                            Event::Acted {
                                action,
                                message,
                                dry_run,
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "olly: level={level} action={verb} target={name} failed: {e}"
                            );
                            Event::Notice {
                                message: voice::act_failed(&action),
                                frozen_id: None,
                            }
                        }
                    }
                }
                other => other,
            };
            self.incident_acted =
                acted_after(self.incident_acted, &ev, self.engine.incident().is_some());
            if matches!(&ev, Event::Acted { action, .. } if is_intervention(action)) {
                self.last_action_wall_ms = Some(wall);
            }
            match &ev {
                Event::Level { level } => {
                    eprintln!("olly: level={} event=level_change", level_name(*level))
                }
                Event::IncidentStart { id, .. } => {
                    self.incident_open = true;
                    if let (true, Some(r)) = (self.claude_running, &self.claude) {
                        r.cancel();
                    }
                    eprintln!("olly: level={level} event=incident_start id={id}");
                }
                Event::IncidentEnd { id, .. } => {
                    self.incident_open = false;
                    self.act.restore_throttles();
                    eprintln!("olly: level={level} event=incident_end id={id}");
                }
                _ => {}
            }
            if let Some(inc) = self.recorder.on_event(&ev, wall) {
                self.last_incident_id = Some(inc.id);
                if let Err(e) = self
                    .store
                    .append(&inc)
                    .and_then(|_| self.store.rotate_if_needed())
                {
                    eprintln!("olly: cannot save incident {}: {e}", inc.id);
                }
                if self.claude.is_some() {
                    enqueue(&mut self.claude_queue, Job::Diagnose(inc));
                }
            }
            self.subs.broadcast(&ipc::event_line(&ev, wall));
        }
    }

    /// On shutdown: an incident still open is saved, marked as cut short.
    fn flush_open_incident(&mut self) {
        if let Some(inc) = self.recorder.finish_now(wall_ms()) {
            eprintln!("olly: saving open incident {} before exit", inc.id);
            if let Err(e) = self.store.append(&inc) {
                eprintln!("olly: cannot save incident {}: {e}", inc.id);
            }
        }
    }

    fn command(&mut self, cmd: Command) -> Result<(), String> {
        let out = self.engine.command(cmd, self.now())?;
        self.process(out);
        Ok(())
    }

    fn handle(&mut self, msg: ToDaemon) {
        match msg {
            ToDaemon::Req(req, reply) => {
                let line = self.respond(req);
                let _ = reply.send(line);
            }
            ToDaemon::Subscribe(tx) => {
                for line in &self.pending {
                    let _ = tx.try_send(line.clone());
                }
                self.subs.add(tx);
            }
        }
    }

    fn respond(&mut self, req: Request) -> String {
        let done = |r: Result<(), String>| match r {
            Ok(()) => ok_line(&()),
            Err(e) => err_line(&e),
        };
        match req {
            Request::Status => ok_line(&self.status()),
            Request::Incidents { limit, offset } => {
                ok_line(&self.store.list(limit.min(MAX_INCIDENTS), offset))
            }
            Request::Incident { id } => match self.store.get(id) {
                Some(i) => ok_line(&i),
                None => err_line(&format!("no incident with id {id}")),
            },
            // The connection thread handles subscribe itself; it never reaches here.
            Request::Subscribe => err_line("subscribe is not a request"),
            Request::Pause { secs } if secs > self.cfg.max_pause_secs => err_line(&format!(
                "pause can be at most {} minutes",
                self.cfg.max_pause_secs / 60
            )),
            Request::Pause { secs } => {
                let until_ms = self.now().saturating_add(secs.saturating_mul(1000));
                match self.command(Command::Pause { until_ms }) {
                    Ok(()) => ok_line(&self.status()),
                    Err(e) => err_line(&e),
                }
            }
            Request::Resume => match self.command(Command::Unpause) {
                Ok(()) => ok_line(&self.status()),
                Err(e) => err_line(&e),
            },
            Request::AllowlistAdd { name } => done(
                self.allow
                    .add(&name)
                    .map_err(|e| format!("could not save the allowlist: {e}")),
            ),
            Request::AllowlistList => ok_line(&self.allow.list()),
            Request::FrozenKill { id } => done(self.command(Command::KillFrozen(id))),
            Request::FrozenResume { id } => done(self.command(Command::ResumeFrozen(id))),
        }
    }

    fn status(&self) -> StatusData {
        let now = self.now();
        let remaining = self
            .engine
            .paused_until()
            .filter(|&u| u > now)
            .map(|u| u - now);
        let (avail, total) = self
            .last_sys
            .map_or((0, 0), |s| (s.mem.available, s.mem.total));
        let (swap_total, swap_free) = self
            .last_sys
            .map_or((0, 0), |s| (s.mem.swap_total, s.mem.swap_free));
        let level = self.engine.level();
        StatusData {
            level,
            dry_run: self.cfg.dry_run,
            paused_until_wall_ms: remaining.map(|r| wall_ms() + r),
            mem_available: avail,
            mem_total: total,
            throttled: self.engine.throttled().to_vec(),
            frozen: self.engine.frozen(),
            self_rss: self_rss(),
            message: voice::status_line(level, avail, remaining),
            swap_total,
            swap_used: swap_total.saturating_sub(swap_free),
            some_avg10: self.last_sys.map_or(0.0, |s| s.psi.some_avg10),
            top: self.last_top.clone(),
            incident_active: self.engine.incident().is_some(),
            incident_acted: self.incident_acted,
            last_action_wall_ms: self.last_action_wall_ms,
            last_incident_id: self.last_incident_id,
            frozen_info: frozen_info(
                &self.engine.frozen_view(),
                now,
                wall_ms(),
                self.cfg.timing.freeze_giveup_secs,
            ),
        }
    }
}

/// mlockall needs root: unprivileged, MCL_FUTURE would make later allocations fail
/// once RLIMIT_MEMLOCK is reached, so it is skipped.
fn lock_memory() {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("olly: not root; skipping mlockall");
        return;
    }
    if unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) } != 0 {
        eprintln!("olly: mlockall failed: {}", std::io::Error::last_os_error());
    }
}

/// Fallback in case the unit file doesn't set OOMScoreAdjust.
fn protect_from_oom() {
    if let Err(e) = fs::write("/proc/self/oom_score_adj", "-1000") {
        eprintln!("olly: cannot set oom_score_adj to -1000: {e}");
    }
}

/// Blocks SIGTERM/SIGINT and returns a signalfd for them. Must run before any thread
/// is spawned so every thread inherits the mask.
fn signal_fd() -> Option<OwnedFd> {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGINT);
        if libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
            eprintln!("olly: cannot block SIGTERM/SIGINT; default signal handling stays");
            return None;
        }
        let fd = libc::signalfd(-1, &set, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC);
        if fd < 0 {
            eprintln!("olly: signalfd failed: {}", std::io::Error::last_os_error());
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
            return None;
        }
        Some(OwnedFd::from_raw_fd(fd))
    }
}

fn event_fd() -> Option<OwnedFd> {
    let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    if fd < 0 {
        eprintln!(
            "olly: eventfd failed, IPC disabled: {}",
            std::io::Error::last_os_error()
        );
        return None;
    }
    Some(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn drain(fd: RawFd, len: usize) {
    let mut buf = [0u8; 128];
    unsafe {
        libc::read(
            fd,
            buf.as_mut_ptr() as *mut libc::c_void,
            len.min(buf.len()),
        )
    };
}

/// Binds the socket (0600, owned by `uid` when running as root). Never fatal: olly
/// keeps protecting the system without IPC.
fn bind_socket(p: &Paths, uid: u32) -> Option<UnixListener> {
    if let Err(e) = fs::create_dir_all(&p.run_dir) {
        eprintln!(
            "olly: cannot create {}, IPC disabled: {e}",
            p.run_dir.display()
        );
        return None;
    }
    if !p.overridden {
        if let Err(e) = fs::set_permissions(&p.run_dir, fs::Permissions::from_mode(0o755)) {
            eprintln!("olly: cannot chmod {}: {e}", p.run_dir.display());
        }
    }
    let _ = fs::remove_file(&p.sock);
    let listener = match UnixListener::bind(&p.sock) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("olly: cannot bind {}, IPC disabled: {e}", p.sock.display());
            return None;
        }
    };
    if let Err(e) = fs::set_permissions(&p.sock, fs::Permissions::from_mode(0o600)) {
        eprintln!("olly: cannot chmod {}: {e}", p.sock.display());
    }
    if unsafe { libc::geteuid() } != 0 {
        eprintln!(
            "olly: not root; {} stays owned by the current user",
            p.sock.display()
        );
    } else if let Err(e) = std::os::unix::fs::chown(&p.sock, Some(uid), None) {
        eprintln!("olly: cannot chown {} to uid {uid}: {e}", p.sock.display());
    }
    Some(listener)
}

pub fn run(mut cfg: Config, cfg_err: Option<String>, root: PathBuf) -> ! {
    let paths = Paths::from_env();
    let no_claude = std::env::var_os("OLLY_TEST_NO_CLAUDE").is_some_and(|v| v == "1");
    if let Some(e) = &cfg_err {
        let one_line: Vec<&str> = e.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        eprintln!("olly: ERROR: {}", one_line.join(" "));
        eprintln!("olly: ERROR: running on built-in defaults, LIVE (dry_run = false)");
    }
    lock_memory();
    protect_from_oom();

    let cgroup_root = root.join("sys/fs/cgroup");
    for line in actuator::recover(&cgroup_root, &root, &paths.active) {
        eprintln!("olly: recover: {line}");
    }
    let act = if cfg.dry_run {
        Act::Dry(DryRunActuator::default())
    } else {
        let mut s = SysActuator::new(cgroup_root, root.clone(), paths.active.clone());
        let n = s.adopt_leftovers();
        if n > 0 {
            eprintln!("olly: {n} leftover freeze/throttle entries could not be undone; will retry");
        }
        Act::Sys(s)
    };

    // Signals first: the IPC threads must inherit the blocked mask.
    let signals = signal_fd();
    let wake = event_fd();
    let (tx, rx) = mpsc::channel();
    let mut served = false;
    if let Some(w) = &wake {
        if let Some(l) = bind_socket(&paths, cfg.desktop_uid) {
            ipc::serve(l, [0, cfg.desktop_uid], tx, w.as_raw_fd());
            served = true;
        }
    }
    let mut psi = match PsiTrigger::new(&root.join("proc/pressure/memory"), PSI_SPEC) {
        Ok(t) => Some(t),
        Err(e) => {
            eprintln!("olly: PSI trigger unavailable ({e}); falling back to timeout-only polling");
            None
        }
    };

    if let Err(e) = fs::create_dir_all(&paths.state_dir) {
        eprintln!("olly: cannot create {}: {e}", paths.state_dir.display());
    }
    let store = IncidentStore::new(paths.incidents.clone());
    let next_id = store.next_id();
    let store_last_id = (next_id > 1).then(|| next_id - 1);
    let mut engine = Engine::new(cfg.clone());
    engine.set_next_incident_id(next_id);
    // Ids from before a restart (old notification buttons) must not hit a new target.
    engine.set_next_target_id(wall_ms());
    if no_claude && cfg.claude.enabled {
        eprintln!("olly: claude diagnosis is off: OLLY_TEST_NO_CLAUDE is set");
        cfg.claude.enabled = false;
    }
    let claude = if cfg.claude.enabled {
        let as_root = unsafe { libc::geteuid() } == 0;
        match ClaudeRunner::new(cfg.claude.clone(), cfg.desktop_uid, as_root) {
            Ok(mut r) => {
                let dir = paths.state_dir.join("claude-cwd");
                match fs::create_dir_all(&dir)
                    .and_then(|_| fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)))
                {
                    Ok(()) => r.set_cwd(dir),
                    Err(e) => eprintln!(
                        "olly: cannot prepare {}, claude runs from /: {e}",
                        dir.display()
                    ),
                }
                if let Some(w) = &wake {
                    r.set_wake(w.as_raw_fd());
                }
                Some(r)
            }
            Err(e) => {
                eprintln!("olly: claude diagnosis is off: {e}");
                None
            }
        }
    } else {
        None
    };
    let (claude_tx, claude_rx) = mpsc::channel();
    let mut d = Daemon {
        sampler: Sampler::new(root.clone(), cfg.desktop_uid),
        hist: History::new(&cfg.growth),
        leak: LeakWatch::new(&cfg.growth),
        allow: Allowlist::load(cfg.allowlist.clone(), paths.allowlist.clone()),
        engine,
        act,
        recorder: IncidentRecorder::new(),
        store,
        subs: Subscribers::new(),
        pending: Vec::new(),
        last_sys: None,
        incident_open: false,
        last_top: None,
        last_action_wall_ms: None,
        incident_acted: false,
        last_incident_id: store_last_id,
        rlog: RateLog::default(),
        claude,
        claude_queue: VecDeque::new(),
        claude_running: false,
        claude_tx,
        claude_rx,
        leak_seen: HashMap::new(),
        start: Instant::now(),
        cfg,
    };
    if cfg_err.is_some() {
        let ev = Event::Notice {
            message: voice::config_fallback(),
            frozen_id: None,
        };
        let line = ipc::event_line(&ev, wall_ms());
        d.subs.broadcast(&line);
        d.pending.push(line);
    }
    eprintln!(
        "olly: started dry_run={} root={} ipc={}",
        d.cfg.dry_run,
        root.display(),
        if served {
            paths.sock.display().to_string()
        } else {
            "off".into()
        }
    );

    let active_ms = d.cfg.timing.active_sample_ms.max(1);
    let slow_ms = d.cfg.timing.slow_sample_secs.max(1) * 1000;
    let (mut next_tick, mut next_slow) = (0u64, 0u64);
    let mut psi_storm = (u64::MAX, 0u32);
    loop {
        let now = d.now();
        if now >= next_tick {
            d.tick(now);
            next_tick = now + tick_ms(d.active(), psi.is_some(), active_ms, slow_ms);
        }
        if now >= next_slow {
            d.slow(now);
            next_slow = now + slow_ms;
        }

        let mut fds = Vec::with_capacity(3);
        let fd_of = |o: &Option<OwnedFd>| o.as_ref().map(|f| f.as_raw_fd());
        for (fd, ev) in [
            (fd_of(&signals), libc::POLLIN),
            (fd_of(&wake), libc::POLLIN),
            (psi.as_ref().map(|p| p.fd()), libc::POLLPRI),
        ] {
            fds.push((fd.unwrap_or(-1), ev)); // poll ignores negative fds
        }
        let timeout = next_tick
            .min(next_slow)
            .saturating_sub(d.now())
            .min(i32::MAX as u64) as i32;
        let ready = match psi::wait(&fds, timeout) {
            Ok(r) => r,
            Err(e) => {
                d.rlog.log("poll", d.now(), &format!("poll failed: {e}"));
                std::thread::sleep(Duration::from_millis(100));
                vec![false; fds.len()]
            }
        };

        if ready[1] {
            drain(fds[1].0, 8);
        }
        while let Ok(msg) = rx.try_recv() {
            d.handle(msg);
        }
        d.pump_claude();
        if ready[0] {
            drain(fds[0].0, std::mem::size_of::<libc::signalfd_siginfo>());
            eprintln!("olly: stopping; undoing every freeze and throttle");
            d.act.undo_all();
            d.flush_open_incident();
            if served {
                let _ = fs::remove_file(&paths.sock);
            }
            std::process::exit(0);
        }
        if ready[2] {
            let sec = d.now() / 1000;
            if psi_storm.0 != sec {
                psi_storm = (sec, 0);
            }
            psi_storm.1 += 1;
            if psi_storm.1 > PSI_STORM {
                eprintln!("olly: PSI trigger keeps firing; falling back to timeout-only polling");
                psi = None;
            }
            next_tick = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Target;
    use crate::sample::{AppKey, TargetKind};

    fn leak(n: u64) -> Job {
        Job::Leak {
            cgroup: format!("/c{n}"),
            growth: n,
        }
    }

    #[test]
    fn queue_drops_oldest_when_full() {
        let mut q = VecDeque::new();
        for n in 0..6 {
            enqueue(&mut q, leak(n));
        }
        let kept: Vec<u64> = q
            .iter()
            .map(|j| {
                if let Job::Leak { growth, .. } = j {
                    *growth
                } else {
                    99
                }
            })
            .collect();
        assert_eq!(kept, vec![2, 3, 4, 5]);
    }

    #[test]
    fn may_start_needs_calm_and_recovered_memory() {
        assert!(may_start(false, 30.0, 25.0));
        assert!(may_start(false, 25.0, 25.0));
        assert!(!may_start(false, 24.9, 25.0));
        assert!(!may_start(true, 90.0, 25.0));
    }

    #[test]
    fn leak_cooldown_prunes_and_blocks() {
        let mut seen = HashMap::new();
        seen.insert("/a".to_string(), 1_000);
        seen.insert("/b".to_string(), LEAK_COOLDOWN_MS);
        assert!(in_cooldown(&seen, "/a", LEAK_COOLDOWN_MS));
        assert!(!in_cooldown(&seen, "/a", 1_000 + LEAK_COOLDOWN_MS));
        assert!(!in_cooldown(&seen, "/c", 5));
        prune_cooldown(&mut seen, 1_000 + LEAK_COOLDOWN_MS);
        assert!(!seen.contains_key("/a") && seen.contains_key("/b"));
    }

    #[test]
    fn idle_cadence_without_psi_trigger() {
        assert_eq!(tick_ms(true, true, 1000, 60_000), 1000);
        assert_eq!(tick_ms(true, false, 1000, 60_000), 1000);
        assert_eq!(tick_ms(false, true, 1000, 60_000), 60_000);
        // Nothing wakes olly early without the trigger, so idle polling stays quick.
        assert_eq!(tick_ms(false, false, 1000, 60_000), 2000);
        assert_eq!(tick_ms(false, false, 1000, 1000), 1000);
    }

    fn view(id: u64, at_ms: u64) -> FrozenView {
        FrozenView {
            target: Target {
                id,
                key: AppKey {
                    cgroup: format!("/a{id}"),
                    root_pid: None,
                },
                name: format!("app{id}"),
                kind: TargetKind::Cgroup,
                bytes: 7,
            },
            at_ms,
            rate: 1.5,
        }
    }

    #[test]
    fn frozen_info_computes_wall_deadline() {
        // Frozen 10s ago on the monotonic clock; the wall clock reads 1_000_000.
        let info = frozen_info(&[view(2, 5_000)], 15_000, 1_000_000, 300);
        assert_eq!(
            info,
            vec![FrozenInfo {
                id: 2,
                name: "app2".into(),
                bytes: 7,
                rate: 1.5,
                kill_at_wall_ms: 1_000_000 - 10_000 + 300_000,
            }]
        );
        // A freeze stamped after "now" (clock skew) doesn't underflow.
        let info = frozen_info(&[view(3, 20_000)], 15_000, 1_000_000, 300);
        assert_eq!(info[0].kill_at_wall_ms, 1_000_000 + 300_000);
    }

    #[test]
    fn incident_acted_resets_on_new_incident() {
        let acted = Event::Acted {
            action: Action::Freeze {
                target: view(1, 0).target,
            },
            message: voice::frozen("x", 30, false),
            dry_run: false,
        };
        assert!(acted_after(false, &acted, true));
        // No open incident: an action leaves it false.
        assert!(!acted_after(false, &acted, false));
        let restore = Event::Acted {
            action: Action::Unthrottle {
                target: view(1, 0).target,
            },
            message: voice::unthrottled("x"),
            dry_run: false,
        };
        assert!(!acted_after(false, &restore, true));
        assert!(acted_after(
            true,
            &Event::Level { level: Level::Calm },
            true
        ));
        assert!(!acted_after(
            true,
            &Event::IncidentStart { id: 2, t_ms: 0 },
            true
        ));
        assert!(!acted_after(
            true,
            &Event::IncidentEnd { id: 2, t_ms: 0 },
            false
        ));
    }

    /// A real `Daemon` on fake proc files, driven to a freeze through the engine.
    #[test]
    fn status_fills_tray_fields() {
        const GIB: u64 = 1 << 30;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::create_dir_all(root.join("proc/pressure")).unwrap();
        fs::write(
            root.join("proc/meminfo"),
            "MemTotal: 33554432 kB\nMemAvailable: 16777216 kB\nSwapTotal: 8388604 kB\nSwapFree: 8000000 kB\n",
        )
        .unwrap();
        fs::write(
            root.join("proc/pressure/memory"),
            "some avg10=4.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
        )
        .unwrap();
        let cfg = Config {
            dry_run: true,
            ..Config::default()
        };
        let (claude_tx, claude_rx) = mpsc::channel();
        let mut d = Daemon {
            sampler: Sampler::new(root.clone(), cfg.desktop_uid),
            hist: History::new(&cfg.growth),
            leak: LeakWatch::new(&cfg.growth),
            allow: Allowlist::load(cfg.allowlist.clone(), root.join("allow.toml")),
            engine: Engine::new(cfg.clone()),
            act: Act::Dry(DryRunActuator::default()),
            recorder: IncidentRecorder::new(),
            store: IncidentStore::new(root.join("incidents.jsonl")),
            subs: Subscribers::new(),
            pending: Vec::new(),
            last_sys: None,
            incident_open: false,
            last_top: None,
            last_action_wall_ms: None,
            incident_acted: false,
            last_incident_id: None,
            rlog: RateLog::default(),
            claude: None,
            claude_queue: VecDeque::new(),
            claude_running: false,
            claude_tx,
            claude_rx,
            leak_seen: HashMap::new(),
            start: Instant::now(),
            cfg,
        };
        let leak = |t: u64| AppSample {
            key: AppKey {
                cgroup: "/leaky".into(),
                root_pid: None,
            },
            name: "leaky".into(),
            exe: None,
            kind: TargetKind::Cgroup,
            bytes: 10 * GIB + t * (GIB / 10) / 1000,
            ram: 10 * GIB + t * (GIB / 10) / 1000,
            names: vec!["leaky".into()],
            exe_names: vec![],
        };
        let step = |d: &mut Daemon, t: u64, full: f32| {
            let mut sys = d.sampler.system(t).unwrap();
            sys.psi.full_avg10 = full;
            sys.full_now = Some(full);
            d.hist.record(t, &[leak(t)]);
            let out = d.engine.step(&sys, &[leak(t)], &d.hist, &d.allow);
            d.process(out);
            d.last_top = top_app(&[leak(t)], &d.hist, t);
            d.last_sys = Some(sys);
        };
        for s in 0..=10 {
            step(&mut d, s * 1000, 0.0);
        }
        assert!(!d.status().incident_active);
        step(&mut d, 11_000, 30.0);
        let wall = wall_ms();
        let st = d.status();
        assert_eq!(st.frozen_info.len(), 1);
        let kill_at = st.frozen_info[0].kill_at_wall_ms;
        let want = wall + d.cfg.timing.freeze_giveup_secs * 1000;
        assert!(kill_at.abs_diff(want) < 2000, "{kill_at} vs {want}");
        assert!(st.incident_active && st.incident_acted);
        assert!(st.last_action_wall_ms.is_some());
        let top = st.top.unwrap();
        assert_eq!(top.name, "leaky");
        assert!(top.rate > 0.0);
        assert_eq!(st.swap_total, 8388604 * 1024);
        assert_eq!(st.swap_used, (8388604 - 8000000) * 1024);
        assert_eq!(st.some_avg10, 4.0);
    }

    #[test]
    fn restores_are_not_interventions() {
        let t = view(1, 0).target;
        assert!(is_intervention(&Action::Throttle {
            target: t.clone(),
            high: 1
        }));
        assert!(is_intervention(&Action::Freeze { target: t.clone() }));
        assert!(!is_intervention(&Action::Unthrottle { target: t.clone() }));
        assert!(!is_intervention(&Action::Resume { target: t }));
    }

    #[test]
    fn notification_line_is_first_paragraph() {
        assert_eq!(
            first_paragraph("  It was the browser.\nTabs piled up.\n\nDetails here.\n"),
            "It was the browser. Tabs piled up."
        );
        assert_eq!(first_paragraph(""), "");
    }
}
