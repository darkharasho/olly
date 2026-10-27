//! The main loop: wires sampling, the engine, the actuator, incidents and IPC.

use crate::actuator::{self, Actuator, DryRunActuator, SysActuator};
use crate::allowlist::Allowlist;
use crate::config::Config;
use crate::engine::{Action, Command, Engine, Event, Level, StepOutput};
use crate::history::{History, LeakWatch};
use crate::incident::{IncidentRecorder, IncidentStore};
use crate::ipc::{self, err_line, ok_line, Request, StatusData, Subscribers, ToDaemon};
use crate::procfs::parse_status;
use crate::psi::{self, PsiTrigger};
use crate::sample::{Sampler, SystemSample};
use crate::voice;
use std::collections::HashMap;
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
            None => (PathBuf::from("/run/olly"), PathBuf::from("/var/lib/olly"), false),
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
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn level_name(l: Level) -> String {
    format!("{l:?}").to_lowercase()
}

fn self_rss() -> u64 {
    fs::read_to_string("/proc/self/status").ok().and_then(|s| parse_status(&s)).map_or(0, |s| s.rss)
}

/// Logs a given repeated error at most once per `LOG_REPEAT_MS`.
#[derive(Default)]
struct RateLog {
    last: HashMap<&'static str, u64>,
}

impl RateLog {
    fn log(&mut self, key: &'static str, now: u64, msg: &str) {
        if self.last.get(key).is_none_or(|&t| now.saturating_sub(t) >= LOG_REPEAT_MS) {
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
    rlog: RateLog,
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
            self.rlog.log("system", now, "cannot read /proc/pressure/memory or /proc/meminfo; skipping this sample");
            return;
        };
        let apps = self.sampler.apps();
        self.hist.record(now, &apps);
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
    /// Task 10 hands these to Claude; for now they are only logged.
    fn on_suspects(&mut self, suspects: Vec<(String, u64)>) {
        for (cgroup, growth) in suspects {
            eprintln!("olly: event=leak_suspect target={cgroup} growth={}", voice::fmt_bytes(growth));
        }
    }

    /// Applies the actions, then logs, records and broadcasts the events. A failed
    /// action's `Acted` event becomes a `Notice` saying olly couldn't act.
    fn process(&mut self, out: StepOutput) {
        let results: Vec<Result<(), String>> = out.actions.iter().map(|a| self.act.apply(a)).collect();
        // The engine pushes one Acted event per action, in the same order.
        let mut results = results.into_iter();
        let wall = wall_ms();
        let level = level_name(self.engine.level());
        for ev in out.events {
            let ev = match ev {
                Event::Acted { action, message, dry_run } => {
                    let (verb, name) = action_parts(&action);
                    match results.next().unwrap_or(Ok(())) {
                        Ok(()) => {
                            eprintln!("olly: level={level} action={verb} target={name} dry_run={dry_run}");
                            Event::Acted { action, message, dry_run }
                        }
                        Err(e) => {
                            eprintln!("olly: level={level} action={verb} target={name} failed: {e}");
                            Event::Notice { message: voice::act_failed(&action), frozen_id: None }
                        }
                    }
                }
                other => other,
            };
            match &ev {
                Event::Level { level } => eprintln!("olly: level={} event=level_change", level_name(*level)),
                Event::IncidentStart { id, .. } => {
                    self.incident_open = true;
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
                if let Err(e) = self.store.append(&inc).and_then(|_| self.store.rotate_if_needed()) {
                    eprintln!("olly: cannot save incident {}: {e}", inc.id);
                }
            }
            self.subs.broadcast(&ipc::event_line(&ev, wall));
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
            Request::Incidents { limit, offset } => ok_line(&self.store.list(limit.min(MAX_INCIDENTS), offset)),
            Request::Incident { id } => match self.store.get(id) {
                Some(i) => ok_line(&i),
                None => err_line(&format!("no incident with id {id}")),
            },
            // The connection thread handles subscribe itself; it never reaches here.
            Request::Subscribe => err_line("subscribe is not a request"),
            Request::Pause { secs } if secs > self.cfg.max_pause_secs => {
                err_line(&format!("pause can be at most {} seconds", self.cfg.max_pause_secs))
            }
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
            Request::AllowlistAdd { name } => {
                done(self.allow.add(&name).map_err(|e| format!("could not save the allowlist: {e}")))
            }
            Request::AllowlistList => ok_line(&self.allow.list()),
            Request::FrozenKill { id } => done(self.command(Command::KillFrozen(id))),
            Request::FrozenResume { id } => done(self.command(Command::ResumeFrozen(id))),
        }
    }

    fn status(&self) -> StatusData {
        let now = self.now();
        let remaining = self.engine.paused_until().filter(|&u| u > now).map(|u| u - now);
        let (avail, total) = self.last_sys.map_or((0, 0), |s| (s.mem.available, s.mem.total));
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
        eprintln!("olly: eventfd failed, IPC disabled: {}", std::io::Error::last_os_error());
        return None;
    }
    Some(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn drain(fd: RawFd, len: usize) {
    let mut buf = [0u8; 128];
    unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, len.min(buf.len())) };
}

/// Binds the socket (0600, owned by `uid` when running as root). Never fatal: olly
/// keeps protecting the system without IPC.
fn bind_socket(p: &Paths, uid: u32) -> Option<UnixListener> {
    if let Err(e) = fs::create_dir_all(&p.run_dir) {
        eprintln!("olly: cannot create {}, IPC disabled: {e}", p.run_dir.display());
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
        eprintln!("olly: not root; {} stays owned by the current user", p.sock.display());
    } else if let Err(e) = std::os::unix::fs::chown(&p.sock, Some(uid), None) {
        eprintln!("olly: cannot chown {} to uid {uid}: {e}", p.sock.display());
    }
    Some(listener)
}

pub fn run(cfg: Config, cfg_err: Option<String>, root: PathBuf) -> ! {
    let paths = Paths::from_env();
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
    let mut engine = Engine::new(cfg.clone());
    engine.set_next_incident_id(store.next_id());
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
        rlog: RateLog::default(),
        start: Instant::now(),
        cfg,
    };
    if cfg_err.is_some() {
        let ev = Event::Notice { message: voice::config_fallback(), frozen_id: None };
        let line = ipc::event_line(&ev, wall_ms());
        d.subs.broadcast(&line);
        d.pending.push(line);
    }
    eprintln!(
        "olly: started dry_run={} root={} ipc={}",
        d.cfg.dry_run,
        root.display(),
        if served { paths.sock.display().to_string() } else { "off".into() }
    );

    let active_ms = d.cfg.timing.active_sample_ms.max(1);
    let slow_ms = d.cfg.timing.slow_sample_secs.max(1) * 1000;
    let (mut next_tick, mut next_slow) = (0u64, 0u64);
    let mut psi_storm = (u64::MAX, 0u32);
    loop {
        let now = d.now();
        if now >= next_tick {
            d.tick(now);
            next_tick = now + if d.active() { active_ms } else { slow_ms };
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
        let timeout = next_tick.min(next_slow).saturating_sub(d.now()).min(i32::MAX as u64) as i32;
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
        if ready[0] {
            drain(fds[0].0, std::mem::size_of::<libc::signalfd_siginfo>());
            eprintln!("olly: stopping; undoing every freeze and throttle");
            d.act.undo_all();
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
            } else {
                next_tick = 0;
            }
        }
    }
}

