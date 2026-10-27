//! Update orchestration for the daemon: worker threads for the check, the staging and the
//! hand-off to the supervisor, plus reading the supervisor's result. The main loop only spawns
//! these workers and collects their results with `try_recv`; the refusal rules are the
//! daemon's, since they need the engine.

use crate::engine::Event;
use crate::supervise::{self, LastUpdate, Layout};
use crate::update::{self, Fetch, Manifest, Staged, UpdateError, UpdateInfo, UpdateState};
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
/// An install with no result after this long (monotonic ms) is given up on.
pub const INSTALL_TIMEOUT_MS: u64 = 600_000;
const WORKER_LOST: &str = "update worker stopped unexpectedly";

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

pub type FetchFactory = Box<dyn Fn() -> Box<dyn Fetch + Send>>;

/// A finished worker.
pub enum Done {
    Checked(Result<Option<Manifest>, String>),
    Staged(Result<Staged, String>),
    Launched(Result<(), String>),
    /// An interrupted swap was finished or undone; true when a result was written.
    Recovered(bool),
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
    /// Why the latest update stopped before the hand-off.
    pub error: Option<UpdateError>,
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
    /// The same moment, monotonic ms, for the install timeout.
    installing_mono: Option<u64>,
    tx: mpsc::Sender<Done>,
    rx: mpsc::Receiver<Done>,
}

/// Sends the worker's result; a worker that panics sends `lost` instead, so the daemon's
/// state never stays stuck and parked askers get an answer.
struct Reply {
    tx: mpsc::Sender<Done>,
    fd: Option<RawFd>,
    lost: Option<Done>,
}

impl Drop for Reply {
    fn drop(&mut self) {
        if let Some(d) = self.lost.take() {
            let _ = self.tx.send(d);
        }
        wake(self.fd);
    }
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
            error: None,
            checking: false,
            launching: false,
            waiters: Vec::new(),
            next_check: FIRST_CHECK_MS,
            emitted_at: None,
            installing_since: None,
            installing_mono: None,
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

    fn spawn(
        &self,
        name: &str,
        lost: Done,
        job: impl FnOnce() -> Done + Send + 'static,
    ) -> Result<(), String> {
        let mut reply = Reply {
            tx: self.tx.clone(),
            fd: self.wake,
            lost: Some(lost),
        };
        thread::Builder::new()
            .name(name.into())
            .stack_size(WORKER_STACK)
            .spawn(move || {
                let done = job();
                reply.lost = None;
                let _ = reply.tx.send(done);
            })
            .map(|_| ())
            .map_err(|e| format!("cannot start the {name} thread: {e}"))
    }

    /// Starts a check worker. The caller makes sure none runs.
    pub fn spawn_check(&mut self) -> Result<(), String> {
        let fetch = (self.fetch)();
        let key = self.pubkey.clone();
        let lost = Done::Checked(Err(WORKER_LOST.into()));
        self.spawn("olly-update-check", lost, move || {
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
        let lost = Done::Staged(Err(WORKER_LOST.into()));
        self.spawn("olly-update-stage", lost, move || {
            Done::Staged(update::stage(fetch.as_ref(), &key, CURRENT, &work))
        })?;
        self.state = UpdateState::Downloading;
        Ok(())
    }

    /// Copies this binary next to the staged tree and starts the supervisor in its own unit.
    pub fn spawn_launch(&mut self, staged: &Staged, wall_ms: u64, now: u64) -> Result<(), String> {
        let work = self.work_dir();
        let root = staged.dir.clone();
        let to = staged.manifest.version.clone();
        let live = self.live;
        let lost = Done::Launched(Err(WORKER_LOST.into()));
        self.spawn("olly-update-launch", lost, move || {
            Done::Launched(launch(&work, &root, &to, live))
        })?;
        self.launching = true;
        self.state = UpdateState::Installing;
        self.installing_since = Some(wall_ms);
        self.installing_mono = Some(now);
        Ok(())
    }

    /// Finishes or undoes a swap whose supervisor died. Plain file work, run on the main
    /// thread at start only; true when it wrote a result.
    pub fn recover(&mut self, wall_ms: u64) -> bool {
        supervise::recover(&self.layout, CURRENT, wall_ms).is_some()
    }

    /// The same, on a worker, for an install that timed out.
    pub fn spawn_recover(&mut self, wall_ms: u64) -> Result<(), String> {
        let layout = self.layout.clone();
        self.spawn("olly-update-recover", Done::Recovered(false), move || {
            Done::Recovered(supervise::recover(&layout, CURRENT, wall_ms).is_some())
        })
    }

    /// The supervisor never reported back (killed, or the machine was busy): give up on the
    /// install after `INSTALL_TIMEOUT_MS`. True when it just gave up.
    pub fn expire_install(&mut self, now: u64) -> bool {
        let stale = self.state == UpdateState::Installing
            && self
                .installing_mono
                .is_some_and(|t| now.saturating_sub(t) > INSTALL_TIMEOUT_MS);
        if stale {
            self.state = UpdateState::Idle;
            self.installing_since = None;
            self.installing_mono = None;
        }
        stale
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
            self.installing_mono = None;
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

/// The supervisor runs in its own unit, as protected from the OOM killer as olly is.
fn supervisor_command(sup: &Path, root: &Path, to: &str) -> Command {
    let unit = format!("--unit=olly-update-{to}");
    let mut cmd = Command::new("systemd-run");
    cmd.args([unit.as_str(), "--collect", "--quiet"])
        .args(["-p", "OOMScoreAdjust=-1000"])
        .arg(sup)
        .arg("update-supervise")
        .arg(root)
        .args([CURRENT, to]);
    cmd
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
    let mut cmd = supervisor_command(&sup, root, to);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supervisor_unit_is_oom_protected() {
        let cmd = supervisor_command(Path::new("/w/sup"), Path::new("/w/0.2.0/root"), "0.2.0");
        assert_eq!(cmd.get_program(), "systemd-run");
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "--unit=olly-update-0.2.0",
                "--collect",
                "--quiet",
                "-p",
                "OOMScoreAdjust=-1000",
                "/w/sup",
                "update-supervise",
                "/w/0.2.0/root",
                CURRENT,
                "0.2.0",
            ]
        );
    }
}
