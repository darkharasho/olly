//! Update orchestration for the daemon: worker threads for the check, the staging and the
//! hand-off to the supervisor, plus reading the supervisor's result. The main loop only spawns
//! these workers and collects their results with `try_recv`; the refusal rules are the
//! daemon's, since they need the engine.

use crate::engine::Event;
use crate::supervise::{LastUpdate, Layout};
use crate::update::{self, Fetch, Manifest, Staged, UpdateInfo, UpdateState};
use std::fs;
use std::os::fd::RawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;

/// The first check runs this long after start (monotonic ms).
pub const FIRST_CHECK_MS: u64 = 300_000;
/// Then one check every this many ms, whether the last one ran or was skipped.
pub const CHECK_EVERY_MS: u64 = 21_600_000;
/// A result younger than this is announced with `Event::Updated`.
const FRESH_RESULT_MS: u64 = 10 * 60 * 1000;
const WORKER_STACK: usize = 256 * 1024;

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

pub type FetchFactory = Box<dyn Fn() -> Box<dyn Fetch + Send>>;

/// A finished worker.
pub enum Done {
    Checked(Result<Option<Manifest>, String>),
    Staged(Result<Staged, String>),
    Launched(Result<(), String>),
}

pub struct Updater {
    pub layout: Layout,
    fetch: FetchFactory,
    pubkey: String,
    /// False under `OLLY_RUN_DIR` (tests, manual runs): the launcher only logs.
    live: bool,
    /// Periodic checks are on (`update_check` in the config).
    pub enabled: bool,
    wake: Option<RawFd>,
    pub update: Option<UpdateInfo>,
    pub state: UpdateState,
    pub last_update: Option<LastUpdate>,
    pub widget_version: Option<String>,
    /// A check worker runs.
    pub checking: bool,
    /// A launch worker runs.
    pub launching: bool,
    /// `update_check` askers waiting for the running check.
    pub waiters: Vec<mpsc::Sender<String>>,
    /// Monotonic ms of the next periodic check.
    pub next_check: u64,
    /// Wall ms of the result that was last announced.
    emitted_at: Option<u64>,
    /// Wall ms when the hand-off to the supervisor started.
    installing_since: Option<u64>,
    tx: mpsc::Sender<Done>,
    rx: mpsc::Receiver<Done>,
}

fn wake(fd: Option<RawFd>) {
    if let Some(fd) = fd {
        let one = 1u64;
        unsafe { libc::write(fd, &one as *const u64 as *const libc::c_void, 8) };
    }
}

impl Updater {
    pub fn new(
        layout: Layout,
        fetch: FetchFactory,
        pubkey: String,
        live: bool,
        enabled: bool,
        wake: Option<RawFd>,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        Updater {
            layout,
            fetch,
            pubkey,
            live,
            enabled,
            wake,
            update: None,
            state: UpdateState::Idle,
            last_update: None,
            widget_version: None,
            checking: false,
            launching: false,
            waiters: Vec::new(),
            next_check: FIRST_CHECK_MS,
            emitted_at: None,
            installing_since: None,
            tx,
            rx,
        }
    }

    pub fn work_dir(&self) -> PathBuf {
        self.layout.state.join("update")
    }

    pub fn try_recv(&self) -> Option<Done> {
        self.rx.try_recv().ok()
    }

    fn spawn(&self, name: &str, job: impl FnOnce() -> Done + Send + 'static) -> Result<(), String> {
        let tx = self.tx.clone();
        let fd = self.wake;
        thread::Builder::new()
            .name(name.into())
            .stack_size(WORKER_STACK)
            .spawn(move || {
                let _ = tx.send(job());
                wake(fd);
            })
            .map(|_| ())
            .map_err(|e| format!("cannot start the {name} thread: {e}"))
    }

    /// Starts a check worker. The caller makes sure none runs.
    pub fn spawn_check(&mut self) -> Result<(), String> {
        let fetch = (self.fetch)();
        let key = self.pubkey.clone();
        self.spawn("olly-update-check", move || {
            Done::Checked(update::check(fetch.as_ref(), &key, CURRENT))
        })?;
        self.checking = true;
        if self.state == UpdateState::Idle {
            self.state = UpdateState::Checking;
        }
        Ok(())
    }

    /// Starts a staging worker. The caller checks the refusal rules first.
    pub fn spawn_stage(&mut self) -> Result<(), String> {
        let fetch = (self.fetch)();
        let key = self.pubkey.clone();
        let work = self.work_dir();
        self.spawn("olly-update-stage", move || {
            Done::Staged(update::stage(fetch.as_ref(), &key, CURRENT, &work))
        })?;
        self.state = UpdateState::Downloading;
        Ok(())
    }

    /// Copies this binary next to the staged tree and starts the supervisor in its own unit.
    pub fn spawn_launch(&mut self, staged: &Staged, wall_ms: u64) -> Result<(), String> {
        let work = self.work_dir();
        let root = staged.dir.clone();
        let to = staged.manifest.version.clone();
        let live = self.live;
        self.spawn("olly-update-launch", move || {
            Done::Launched(launch(&work, &root, &to, live))
        })?;
        self.launching = true;
        self.state = UpdateState::Installing;
        self.installing_since = Some(wall_ms);
        Ok(())
    }

    pub fn read_widget_version(&mut self) {
        self.widget_version = widget_version(&self.layout.share.join("plasmoid/metadata.json"));
    }

    /// Reads `update-result.json` into `last_update`. Returns the event to announce when the
    /// result is fresh and hasn't been announced yet.
    pub fn load_result(&mut self, wall_ms: u64) -> Option<Event> {
        let path = self.layout.result_path();
        let r: LastUpdate = match fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("olly: cannot read {}: {e}", path.display());
                    return None;
                }
            },
            Err(_) => return None,
        };
        if r.ok {
            let installed = |v: &str| !update::is_newer(v, CURRENT) || !update::is_newer(v, &r.to);
            if self.update.as_ref().is_some_and(|u| installed(&u.version)) {
                self.update = None;
            }
        }
        // The supervisor is done once a result newer than the hand-off shows up.
        if self.state == UpdateState::Installing
            && self.installing_since.is_some_and(|t| r.at_wall_ms >= t)
        {
            self.state = UpdateState::Idle;
            self.installing_since = None;
        }
        let fresh = wall_ms.saturating_sub(r.at_wall_ms) < FRESH_RESULT_MS;
        let ev = (fresh && self.emitted_at != Some(r.at_wall_ms)).then(|| Event::Updated {
            from: r.from.clone(),
            to: r.to.clone(),
            ok: r.ok,
            message: r.message.clone(),
            widget_changed: r.widget_changed,
        });
        if ev.is_some() {
            self.emitted_at = Some(r.at_wall_ms);
        }
        self.last_update = Some(r);
        ev
    }
}

fn widget_version(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v["KPlugin"]["Version"].as_str().map(String::from)
}

/// Runs on the launch worker. `live` false: copy only, log the systemd-run line.
fn launch(work: &Path, root: &Path, to: &str, live: bool) -> Result<(), String> {
    let sup = work.join("olly-supervisor");
    let _ = fs::remove_file(&sup);
    fs::copy("/proc/self/exe", &sup)
        .map_err(|e| format!("cannot copy olly to {}: {e}", sup.display()))?;
    fs::set_permissions(&sup, fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("cannot chmod {}: {e}", sup.display()))?;
    let unit = format!("--unit=olly-update-{to}");
    let mut cmd = Command::new("systemd-run");
    cmd.args([unit.as_str(), "--collect", "--quiet"])
        .arg(&sup)
        .arg("update-supervise")
        .arg(root)
        .args([CURRENT, to]);
    if !live {
        eprintln!("olly: OLLY_RUN_DIR is set; not running {cmd:?}");
        return Ok(());
    }
    let status = cmd
        .stdin(Stdio::null())
        .status()
        .map_err(|e| format!("cannot run systemd-run: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("systemd-run failed ({status})"))
    }
}
