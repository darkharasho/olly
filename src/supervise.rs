//! The update supervisor: swaps in a staged release, checks the new daemon answers, and puts
//! the old files back if it doesn't. It runs from a copy of the old binary in its own unit, so
//! rollback never depends on the new binary working.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// How long the new daemon has to answer `status` with the new version.
const HEALTH_MS: u64 = 30_000;
/// How long the old daemon has to answer `status` with the old version after a rollback.
const ROLLBACK_HEALTH_MS: u64 = 15_000;
const POLL_MS: u64 = 500;
const SOCKET: &str = "/run/olly/olly.sock";
const SOCKET_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_REPLY: u64 = 1024 * 1024;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct LastUpdate {
    pub from: String,
    pub to: String,
    pub ok: bool,
    pub at_wall_ms: u64,
    pub message: String,
    pub widget_changed: bool,
}

/// Where an install lives.
#[derive(Clone)]
pub struct Layout {
    pub bin: PathBuf,
    pub unit_dir: PathBuf,
    pub share: PathBuf,
    pub state: PathBuf,
}

impl Layout {
    pub fn system() -> Self {
        Layout {
            bin: "/usr/local/bin".into(),
            unit_dir: "/etc/systemd/system".into(),
            share: "/usr/local/share/olly".into(),
            state: "/var/lib/olly".into(),
        }
    }

    pub fn result_path(&self) -> PathBuf {
        self.state.join("update-result.json")
    }

    /// Written before the first backup and removed after the result: while it exists, a swap
    /// is running or was interrupted.
    pub fn journal_path(&self) -> PathBuf {
        self.state.join("update/swap.json")
    }
}

/// What a swap was doing, so an interrupted one can be finished or undone.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Journal {
    from: String,
    to: String,
}

/// What the supervisor needs from the system, so tests can fake it.
pub trait Host {
    fn daemon_reload(&mut self) -> Result<(), String>;
    /// Clears systemd's failed state and restart counter for olly, so a rollback restart
    /// isn't refused after the new daemon crash-looped.
    fn reset_failed(&mut self) -> Result<(), String>;
    fn restart_olly(&mut self) -> Result<(), String>;
    fn status_version(&mut self) -> Option<String>;
    fn notify_finished(&mut self);
    fn sleep_ms(&mut self, ms: u64);
    fn wall_ms(&mut self) -> u64;
}

/// The real system: systemctl and olly's socket.
pub struct SystemHost;

fn systemctl(args: &[&str]) -> Result<(), String> {
    match Command::new("systemctl").args(args).status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("systemctl {} failed: {s}", args.join(" "))),
        Err(e) => Err(format!("couldn't run systemctl: {e}")),
    }
}

/// One request line to the daemon and its first reply line, with short timeouts.
fn ask(line: &str) -> Option<serde_json::Value> {
    let mut s = UnixStream::connect(SOCKET).ok()?;
    s.set_read_timeout(Some(SOCKET_TIMEOUT)).ok()?;
    s.set_write_timeout(Some(SOCKET_TIMEOUT)).ok()?;
    s.write_all(format!("{line}\n").as_bytes()).ok()?;
    let mut reply = String::new();
    BufReader::new(s.take(MAX_REPLY))
        .read_line(&mut reply)
        .ok()?;
    serde_json::from_str(&reply).ok()
}

impl Host for SystemHost {
    fn daemon_reload(&mut self) -> Result<(), String> {
        systemctl(&["daemon-reload"])
    }

    fn reset_failed(&mut self) -> Result<(), String> {
        systemctl(&["reset-failed", "olly"])
    }

    fn restart_olly(&mut self) -> Result<(), String> {
        systemctl(&["restart", "olly"])
    }

    fn status_version(&mut self) -> Option<String> {
        let v = ask(r#"{"cmd":"status"}"#)?;
        if v["ok"] != true {
            return None;
        }
        v["data"]["version"].as_str().map(String::from)
    }

    fn notify_finished(&mut self) {
        let _ = ask(r#"{"cmd":"update_finished"}"#);
    }

    fn sleep_ms(&mut self, ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }

    fn wall_ms(&mut self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    File(u32),
    Dir,
}

/// One installed path and its staged source, plus how far the swap got.
struct Item {
    src: PathBuf,
    dst: PathBuf,
    kind: Kind,
    /// `Some(true)`: the old copy is at `.prev`; `Some(false)`: there was no old copy.
    backed_up: Option<bool>,
    installed: bool,
}

fn sibling(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

fn exists(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

fn remove_any(p: &Path) -> io::Result<()> {
    let r = match fs::symlink_metadata(p) {
        Ok(md) if md.is_dir() => fs::remove_dir_all(p),
        Ok(_) => fs::remove_file(p),
        Err(e) => Err(e),
    };
    match r {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

/// Copies a tree of plain files and directories (dirs 0755, files 0644); anything else fails.
fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir(dst)?;
    fs::set_permissions(dst, fs::Permissions::from_mode(0o755))?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ft = fs::symlink_metadata(&from)?.file_type();
        if ft.is_dir() {
            copy_tree(&from, &to)?;
        } else if ft.is_file() {
            fs::copy(&from, &to)?;
            fs::set_permissions(&to, fs::Permissions::from_mode(0o644))?;
        } else {
            return Err(io::Error::other(format!(
                "{} isn't a plain file",
                from.display()
            )));
        }
    }
    Ok(())
}

fn err(what: &str, p: &Path, e: io::Error) -> String {
    format!("couldn't {what} {}: {e}", p.display())
}

impl Item {
    fn prev(&self) -> PathBuf {
        sibling(&self.dst, ".prev")
    }

    fn new_path(&self) -> PathBuf {
        sibling(&self.dst, ".new")
    }

    /// Files are copied to `.prev` (the old one stays in place until the swap); the plasmoid
    /// directory is renamed. A leftover `.prev` is replaced: `supervise` has already settled
    /// any interrupted swap that made one.
    fn back_up(&mut self) -> Result<(), String> {
        let prev = self.prev();
        // A crashed earlier run may have left the only good copy at `.prev`: put it back first.
        if !exists(&self.dst) && exists(&prev) {
            fs::rename(&prev, &self.dst).map_err(|e| err("recover", &prev, e))?;
            eprintln!(
                "\u{1F916} recovered {} from an earlier update",
                self.dst.display()
            );
        }
        remove_any(&prev).map_err(|e| err("clear", &prev, e))?;
        if !exists(&self.dst) {
            self.backed_up = Some(false);
            return Ok(());
        }
        match self.kind {
            Kind::File(_) => {
                // Via a temp name, so a `.prev` is always complete.
                let tmp = sibling(&prev, ".tmp");
                let copied = fs::copy(&self.dst, &tmp)
                    .and_then(|_| fs::File::open(&tmp)?.sync_all())
                    .and_then(|_| fs::rename(&tmp, &prev));
                if let Err(e) = copied {
                    let _ = fs::remove_file(&tmp);
                    return Err(err("back up", &self.dst, e));
                }
            }
            Kind::Dir => fs::rename(&self.dst, &prev).map_err(|e| err("back up", &self.dst, e))?,
        }
        self.backed_up = Some(true);
        Ok(())
    }

    fn install(&mut self) -> Result<(), String> {
        let new = self.new_path();
        if let Some(parent) = self.dst.parent() {
            fs::create_dir_all(parent).map_err(|e| err("make", parent, e))?;
        }
        remove_any(&new).map_err(|e| err("clear", &new, e))?;
        let staged = match self.kind {
            Kind::File(mode) => fs::copy(&self.src, &new)
                .and_then(|_| fs::set_permissions(&new, fs::Permissions::from_mode(mode)))
                .and_then(|_| fs::File::open(&new)?.sync_all()),
            Kind::Dir => copy_tree(&self.src, &new),
        };
        if let Err(e) = staged {
            let _ = remove_any(&new);
            return Err(err("copy", &self.src, e));
        }
        if let Err(e) = fs::rename(&new, &self.dst) {
            let _ = remove_any(&new);
            return Err(err("install", &self.dst, e));
        }
        self.installed = true;
        Ok(())
    }

    /// Puts back what was there before; returns false if that didn't fully work.
    fn restore(&self) -> bool {
        let _ = remove_any(&self.new_path());
        match self.backed_up {
            None => true,
            Some(true) => {
                let prev = self.prev();
                if self.kind == Kind::Dir {
                    if let Err(e) = remove_any(&self.dst) {
                        eprintln!("\u{1F916} {}", err("remove", &self.dst, e));
                    }
                }
                match fs::rename(&prev, &self.dst) {
                    Ok(()) => true,
                    Err(e) => {
                        eprintln!("\u{1F916} {}", err("restore", &self.dst, e));
                        false
                    }
                }
            }
            Some(false) if self.installed => match remove_any(&self.dst) {
                Ok(()) => true,
                Err(e) => {
                    eprintln!("\u{1F916} {}", err("remove", &self.dst, e));
                    false
                }
            },
            Some(false) => true,
        }
    }

    fn drop_backup(&self) {
        if self.backed_up == Some(true) {
            let prev = self.prev();
            if let Err(e) = remove_any(&prev) {
                eprintln!("\u{1F916} {}", err("remove", &prev, e));
            }
        }
    }
}

const UNIT: usize = 2;
const WIDGET: usize = 3;

fn widget_version(dir: &Path) -> Option<String> {
    let text = fs::read_to_string(dir.join("metadata.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v["KPlugin"]["Version"].as_str().map(String::from)
}

/// Writes the journal atomically and keeps it locked (flock) until the returned file is
/// dropped, so a daemon starting meanwhile can tell the swap is still running.
fn write_journal(layout: &Layout, from: &str, to: &str) -> Result<fs::File, String> {
    use std::os::fd::AsRawFd;
    let path = layout.journal_path();
    let tmp = sibling(&path, ".tmp");
    let j = Journal {
        from: from.to_string(),
        to: to.to_string(),
    };
    let write = || -> io::Result<fs::File> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string(&j).map_err(io::Error::other)?;
        let mut f = fs::File::create(&tmp)?;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        f.write_all(json.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, &path)?;
        Ok(f)
    };
    write().map_err(|e| {
        let _ = fs::remove_file(&tmp);
        err("write", &path, e)
    })
}

fn remove_journal(layout: &Layout) {
    let path = layout.journal_path();
    if let Err(e) = remove_any(&path) {
        eprintln!("\u{1F916} {}", err("remove", &path, e));
    }
}

/// The installed paths and where a release stages each one.
fn items(layout: &Layout, staged_root: &Path) -> [Item; 5] {
    let item = |src: PathBuf, dst: PathBuf, kind| Item {
        src,
        dst,
        kind,
        backed_up: None,
        installed: false,
    };
    [
        item(
            staged_root.join("olly"),
            layout.bin.join("olly"),
            Kind::File(0o755),
        ),
        item(
            staged_root.join("olly-notify"),
            layout.bin.join("olly-notify"),
            Kind::File(0o755),
        ),
        item(
            staged_root.join("dist/olly.service"),
            layout.unit_dir.join("olly.service"),
            Kind::File(0o644),
        ),
        item(
            staged_root.join("plasmoid/package"),
            layout.share.join("plasmoid"),
            Kind::Dir,
        ),
        item(
            staged_root.join("dist/olly-notify.service"),
            layout.share.join("olly-notify.service"),
            Kind::File(0o644),
        ),
    ]
}

/// Puts back every `.prev` an install left. Returns the paths that couldn't be restored.
pub fn restore_backups(layout: &Layout) -> Vec<String> {
    let mut items = items(layout, Path::new(""));
    items
        .iter_mut()
        .rev()
        .filter_map(|it| {
            let _ = remove_any(&sibling(&it.prev(), ".tmp"));
            if !exists(&it.prev()) {
                let _ = remove_any(&it.new_path());
                return None;
            }
            it.backed_up = Some(true);
            (!it.restore()).then(|| it.dst.display().to_string())
        })
        .collect()
}

fn rollback_message(from: &str, problems: &[String]) -> String {
    if problems.is_empty() {
        format!("that update didn't work, so I went back to {from}.")
    } else {
        format!(
            "that update didn't work, so I went back to {from}. ({})",
            problems.join("; ")
        )
    }
}

fn not_restored_problem(paths: &[String]) -> String {
    format!("some files couldn't be restored: {}", paths.join(", "))
}

/// What `open_journal` found at the journal path.
enum Found {
    Nothing,
    /// A live supervisor holds the lock.
    Busy,
    Unreadable(String),
    /// The journal, locked until the file is dropped.
    Swap(Journal, fs::File),
}

/// Opens and locks the journal without waiting, and reads it through the locked file.
fn open_journal(layout: &Layout) -> Found {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    let path = layout.journal_path();
    let mut f = match fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Found::Nothing,
        Err(e) => return Found::Unreadable(err("read", &path, e)),
    };
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Found::Busy;
    }
    // A supervisor that renamed a fresh journal in between holds that one's lock.
    let same = match (f.metadata(), fs::metadata(&path)) {
        (Ok(a), Ok(b)) => a.ino() == b.ino() && a.dev() == b.dev(),
        _ => false,
    };
    if !same {
        return Found::Busy;
    }
    let mut text = String::new();
    match f
        .read_to_string(&mut text)
        .and_then(|_| serde_json::from_str(&text).map_err(io::Error::other))
    {
        Ok(j) => Found::Swap(j, f),
        Err(e) => Found::Unreadable(err("read", &path, e)),
    }
}

/// Undoes (`from == current`) or keeps (`to == current`) the swap a journal describes. `None`
/// when it is for another version, so nothing is known about its files. `reload` runs after
/// an old unit file went back.
fn settle(
    layout: &Layout,
    j: &Journal,
    current: &str,
    wall_ms: u64,
    reload: &mut dyn FnMut(),
) -> Option<LastUpdate> {
    if j.from == current {
        // The old binary runs, so the swap never finished: undo it.
        eprintln!(
            "\u{1F916} an update to {} was interrupted; putting {} back",
            j.to, j.from
        );
        let items = items(layout, Path::new(""));
        let unit_back = exists(&items[UNIT].prev());
        let not_restored = restore_backups(layout);
        if unit_back {
            // Otherwise systemd keeps the new unit loaded until something reloads.
            reload();
        }
        let problems: Vec<String> = if not_restored.is_empty() {
            Vec::new()
        } else {
            vec![not_restored_problem(&not_restored)]
        };
        Some(LastUpdate {
            message: rollback_message(&j.from, &problems),
            from: j.from.clone(),
            to: j.to.clone(),
            ok: false,
            at_wall_ms: wall_ms,
            widget_changed: false,
        })
    } else if j.to == current {
        // The new binary runs, so the supervisor died after the restart: keep it. The
        // `.prev` copies stay until the daemon has run a while (`drop_backups`), in case
        // the new binary turns out broken.
        eprintln!(
            "\u{1F916} the update to {} finished without its supervisor; keeping it",
            j.to
        );
        let items = items(layout, Path::new(""));
        let widget = &items[WIDGET].dst;
        let old_widget = widget_version(&sibling(widget, ".prev"));
        let widget_changed = old_widget.is_none() || widget_version(widget) != old_widget;
        for it in &items {
            let _ = remove_any(&it.new_path());
            let _ = remove_any(&sibling(&it.prev(), ".tmp"));
        }
        Some(LastUpdate {
            message: format!("updated to {}! \u{2728}", j.to),
            from: j.from.clone(),
            to: j.to.clone(),
            ok: true,
            at_wall_ms: wall_ms,
            widget_changed,
        })
    } else {
        None
    }
}

/// Finishes or undoes a swap whose supervisor died, from the daemon (on start, and when an
/// install times out). `current` is the running daemon's version; `reload` runs
/// `systemctl daemon-reload`. Returns the result it wrote, if any. Leaves a swap alone while
/// its supervisor still holds the journal lock.
pub fn recover(
    layout: &Layout,
    current: &str,
    wall_ms: u64,
    reload: &mut dyn FnMut(),
) -> Option<LastUpdate> {
    let (j, lock) = match open_journal(layout) {
        Found::Nothing => return None,
        Found::Busy => {
            eprintln!("olly: an update is still swapping files; leaving it to finish");
            return None;
        }
        Found::Unreadable(e) => {
            eprintln!("olly: {e}; leaving the update files as they are");
            return None;
        }
        Found::Swap(j, f) => (j, f),
    };
    let Some(result) = settle(layout, &j, current, wall_ms, reload) else {
        eprintln!(
            "olly: found an interrupted update from {} to {}, but I'm {current}; leaving its files as they are",
            j.from, j.to
        );
        return None;
    };
    write_result(layout, &result);
    remove_journal(layout);
    drop(lock);
    Some(result)
}

/// Removes the `.prev` copies a recovered update kept. Does nothing while a journal exists.
pub fn drop_backups(layout: &Layout) {
    if exists(&layout.journal_path()) {
        return;
    }
    for mut it in items(layout, Path::new("")) {
        if exists(&it.prev()) {
            it.backed_up = Some(true);
            it.drop_backup();
        }
    }
}

fn write_result(layout: &Layout, r: &LastUpdate) {
    let path = layout.result_path();
    let tmp = sibling(&path, ".tmp");
    let write = || -> io::Result<()> {
        fs::create_dir_all(&layout.state)?;
        let json = serde_json::to_string(r).map_err(io::Error::other)?;
        let mut f = fs::File::create(&tmp)?;
        f.write_all(json.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, &path)
    };
    if let Err(e) = write() {
        let _ = fs::remove_file(&tmp);
        eprintln!("\u{1F916} {}", err("write", &path, e));
    }
}

/// Polls until the daemon reports `version`, for up to `budget_ms` (by sleeps and by the clock).
fn healthy(host: &mut dyn Host, version: &str, budget_ms: u64) -> bool {
    let start = host.wall_ms();
    let mut waited = 0;
    loop {
        if host.status_version().as_deref() == Some(version) {
            return true;
        }
        let elapsed = host.wall_ms().saturating_sub(start);
        if waited >= budget_ms || elapsed >= budget_ms {
            return false;
        }
        host.sleep_ms(POLL_MS);
        waited += POLL_MS;
    }
}

/// Swaps in the release staged at `staged_root`, checks it, and rolls back if it doesn't come
/// up. Always writes the result file and tells the daemon. Never panics.
pub fn supervise(
    layout: &Layout,
    staged_root: &Path,
    from: &str,
    to: &str,
    host: &mut dyn Host,
) -> LastUpdate {
    // A journal left by an interrupted swap is settled first, so the backups made below are
    // of what actually runs now.
    match open_journal(layout) {
        Found::Nothing => {}
        Found::Busy => {
            return finish(
                layout,
                host,
                None,
                LastUpdate {
                    from: from.to_string(),
                    to: to.to_string(),
                    ok: false,
                    at_wall_ms: 0,
                    message: format!(
                        "another update is still swapping files, so I stayed on {from}."
                    ),
                    widget_changed: false,
                },
            );
        }
        Found::Unreadable(e) => {
            eprintln!("\u{1F916} {e}; starting over");
            remove_journal(layout);
        }
        Found::Swap(j, lock) => {
            let wall = host.wall_ms();
            let mut reload = || {
                if let Err(e) = host.daemon_reload() {
                    eprintln!("\u{1F916} {e}");
                }
            };
            if settle(layout, &j, from, wall, &mut reload).is_none() {
                eprintln!(
                    "\u{1F916} found an interrupted update from {} to {}; starting over",
                    j.from, j.to
                );
            }
            remove_journal(layout);
            drop(lock);
        }
    }

    let mut items = items(layout, staged_root);

    let unit_changed = match (fs::read(&items[UNIT].src), fs::read(&items[UNIT].dst)) {
        (Ok(new), Ok(old)) => new != old,
        _ => true,
    };
    let old_widget = widget_version(&items[WIDGET].dst);
    let widget_changed = old_widget.is_none() || widget_version(&items[WIDGET].src) != old_widget;

    let mut journal = None;
    // Set once olly was (or may have been) restarted onto the new files.
    let mut restarted = false;
    let mut swap = |items: &mut [Item], host: &mut dyn Host| -> Result<(), String> {
        journal = Some(write_journal(layout, from, to)?);
        for it in items.iter_mut() {
            it.back_up()?;
        }
        for it in items.iter_mut() {
            it.install()?;
        }
        if unit_changed {
            host.daemon_reload()?;
        }
        restarted = true;
        host.restart_olly()?;
        if healthy(host, to, HEALTH_MS) {
            Ok(())
        } else {
            Err(format!("olly didn't report {to} within {HEALTH_MS} ms"))
        }
    };

    // `None` on success; otherwise what went wrong with the rollback, if anything.
    let swapped = swap(&mut items, host);
    let rollback: Option<Vec<String>> = match swapped {
        Ok(()) => {
            for it in &items {
                it.drop_backup();
            }
            None
        }
        Err(e) => {
            eprintln!("\u{1F916} update to {to} failed, rolling back: {e}");
            let mut problems = Vec::new();
            let not_restored: Vec<String> = items
                .iter()
                .rev()
                .filter(|it| !it.restore())
                .map(|it| it.dst.display().to_string())
                .collect();
            if !not_restored.is_empty() {
                eprintln!(
                    "\u{1F916} rollback incomplete: couldn't restore {}",
                    not_restored.join(", ")
                );
                problems.push(not_restored_problem(&not_restored));
            }
            if items[UNIT].installed && unit_changed {
                if let Err(e) = host.daemon_reload() {
                    eprintln!("\u{1F916} {e}");
                }
            }
            // The running daemon never left the old binary, so there is nothing to restart.
            if restarted {
                let _ = host.reset_failed();
                if let Err(e) = host.restart_olly() {
                    eprintln!("\u{1F916} {e}; trying once more");
                    if let Err(e) = host.restart_olly() {
                        eprintln!("\u{1F916} {e}");
                    }
                }
                // Poll even if restart failed: systemd may still bring it up.
                if !healthy(host, from, ROLLBACK_HEALTH_MS) {
                    eprintln!(
                        "\u{1F916} rollback incomplete: olly didn't report {from} within {ROLLBACK_HEALTH_MS} ms"
                    );
                    problems.push(
                        "but olly didn't come back up; run `sudo systemctl status olly`"
                            .to_string(),
                    );
                }
            }
            Some(problems)
        }
    };
    let ok = rollback.is_none();
    let message = match &rollback {
        None => format!("updated to {to}! \u{2728}"),
        Some(p) if restarted => rollback_message(from, p),
        Some(p) => stayed_message(to, from, p),
    };
    let result = LastUpdate {
        from: from.to_string(),
        to: to.to_string(),
        ok,
        at_wall_ms: 0,
        message,
        widget_changed: ok && widget_changed,
    };
    finish(layout, host, journal, result)
}

fn stayed_message(to: &str, from: &str, problems: &[String]) -> String {
    let base = format!("I couldn't install {to}, so I stayed on {from}.");
    if problems.is_empty() {
        base
    } else {
        format!("{base} ({})", problems.join("; "))
    }
}

/// Stamps and writes the result, drops the journal, and tells the daemon.
fn finish(
    layout: &Layout,
    host: &mut dyn Host,
    journal: Option<fs::File>,
    mut result: LastUpdate,
) -> LastUpdate {
    result.at_wall_ms = host.wall_ms();
    write_result(layout, &result);
    // Removed before the lock is released, so recovery never sees a finished swap.
    if let Some(lock) = journal {
        remove_journal(layout);
        drop(lock);
    }
    host.notify_finished();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct FakeHost {
        calls: Vec<String>,
        versions: VecDeque<Option<String>>,
        last_version: Option<String>,
        restart_fails: VecDeque<bool>,
        clock: u64,
        polls: u32,
    }

    impl FakeHost {
        fn new(versions: &[Option<&str>]) -> Self {
            FakeHost {
                calls: Vec::new(),
                versions: versions.iter().map(|v| v.map(String::from)).collect(),
                last_version: None,
                restart_fails: VecDeque::new(),
                clock: 1_000_000,
                polls: 0,
            }
        }
        fn count(&self, name: &str) -> usize {
            self.calls.iter().filter(|c| *c == name).count()
        }
    }

    impl Host for FakeHost {
        fn daemon_reload(&mut self) -> Result<(), String> {
            self.calls.push("daemon_reload".into());
            Ok(())
        }
        fn reset_failed(&mut self) -> Result<(), String> {
            self.calls.push("reset_failed".into());
            Err("reset-failed fails here; it must not matter".into())
        }
        fn restart_olly(&mut self) -> Result<(), String> {
            self.calls.push("restart_olly".into());
            if self.restart_fails.pop_front().unwrap_or(false) {
                Err("restart failed".into())
            } else {
                Ok(())
            }
        }
        fn status_version(&mut self) -> Option<String> {
            self.polls += 1;
            if let Some(v) = self.versions.pop_front() {
                self.last_version = v;
            }
            self.last_version.clone()
        }
        fn notify_finished(&mut self) {
            self.calls.push("notify_finished".into());
        }
        fn sleep_ms(&mut self, ms: u64) {
            self.clock += ms;
        }
        fn wall_ms(&mut self) -> u64 {
            self.clock
        }
    }

    struct Env {
        _tmp: tempfile::TempDir,
        layout: Layout,
        staged: PathBuf,
    }

    fn meta(version: &str) -> String {
        format!(r#"{{"KPlugin":{{"Id":"org.olly.tray","Version":"{version}"}}}}"#)
    }

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    fn read(p: &Path) -> String {
        fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
    }

    /// Old install with `old_widget` metadata; staged release with `new_unit` and `new_widget`.
    fn env_with(new_unit: &str, old_widget: Option<&str>, new_widget: &str) -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let layout = Layout {
            bin: t.join("bin"),
            unit_dir: t.join("unit"),
            share: t.join("share"),
            state: t.join("state"),
        };
        write(&layout.bin.join("olly"), "old-olly");
        write(&layout.bin.join("olly-notify"), "old-notify");
        write(&layout.unit_dir.join("olly.service"), "old-unit");
        write(&layout.share.join("olly-notify.service"), "old-notify-unit");
        if let Some(v) = old_widget {
            write(&layout.share.join("plasmoid/metadata.json"), &meta(v));
            write(
                &layout.share.join("plasmoid/contents/ui/main.qml"),
                "old-qml",
            );
        }
        let staged = t.join("work/0.2.0/root");
        write(&staged.join("olly"), "new-olly");
        write(&staged.join("olly-notify"), "new-notify");
        write(&staged.join("VERSION"), "0.2.0\n");
        write(&staged.join("dist/olly.service"), new_unit);
        write(&staged.join("dist/olly-notify.service"), "new-notify-unit");
        write(
            &staged.join("plasmoid/package/metadata.json"),
            &meta(new_widget),
        );
        write(
            &staged.join("plasmoid/package/contents/ui/main.qml"),
            "new-qml",
        );
        Env {
            _tmp: tmp,
            layout,
            staged,
        }
    }

    fn env() -> Env {
        env_with("new-unit", Some("0.1.0"), "0.1.0")
    }

    fn mode(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let Ok(rd) = fs::read_dir(dir) else {
            return out;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".prev") || name.ends_with(".new") {
                out.push(e.path().display().to_string());
            }
        }
        out
    }

    fn all_leftovers(l: &Layout) -> Vec<String> {
        [&l.bin, &l.unit_dir, &l.share, &l.state]
            .iter()
            .flat_map(|d| leftovers(d))
            .collect()
    }

    fn assert_old(l: &Layout) {
        assert_eq!(read(&l.bin.join("olly")), "old-olly");
        assert_eq!(read(&l.bin.join("olly-notify")), "old-notify");
        assert_eq!(read(&l.unit_dir.join("olly.service")), "old-unit");
        assert_eq!(
            read(&l.share.join("olly-notify.service")),
            "old-notify-unit"
        );
        assert_eq!(
            read(&l.share.join("plasmoid/contents/ui/main.qml")),
            "old-qml"
        );
        assert_eq!(all_leftovers(l), Vec::<String>::new());
    }

    fn saved(l: &Layout) -> LastUpdate {
        serde_json::from_str(&read(&l.result_path())).unwrap()
    }

    #[test]
    fn system_layout_paths() {
        let l = Layout::system();
        assert_eq!(l.bin, PathBuf::from("/usr/local/bin"));
        assert_eq!(l.unit_dir, PathBuf::from("/etc/systemd/system"));
        assert_eq!(l.share, PathBuf::from("/usr/local/share/olly"));
        assert_eq!(l.state, PathBuf::from("/var/lib/olly"));
        assert_eq!(
            l.result_path(),
            PathBuf::from("/var/lib/olly/update-result.json")
        );
    }

    #[test]
    fn success_swaps_and_cleans_up() {
        let e = env();
        let mut h = FakeHost::new(&[None, Some("0.1.0"), Some("0.2.0")]);
        let r = supervise(&e.layout, &e.staged, "0.1.0", "0.2.0", &mut h);
        assert!(r.ok, "{r:?}");
        assert_eq!(r.from, "0.1.0");
        assert_eq!(r.to, "0.2.0");
        assert_eq!(r.message, "updated to 0.2.0! \u{2728}");
        assert_eq!(h.polls, 3);
        let l = &e.layout;
        assert_eq!(read(&l.bin.join("olly")), "new-olly");
        assert_eq!(read(&l.bin.join("olly-notify")), "new-notify");
        assert_eq!(read(&l.unit_dir.join("olly.service")), "new-unit");
        assert_eq!(
            read(&l.share.join("olly-notify.service")),
            "new-notify-unit"
        );
        assert_eq!(
            read(&l.share.join("plasmoid/contents/ui/main.qml")),
            "new-qml"
        );
        assert_eq!(mode(&l.bin.join("olly")), 0o755);
        assert_eq!(mode(&l.bin.join("olly-notify")), 0o755);
        assert_eq!(mode(&l.unit_dir.join("olly.service")), 0o644);
        assert_eq!(mode(&l.share.join("olly-notify.service")), 0o644);
        assert_eq!(all_leftovers(l), Vec::<String>::new());
        assert_eq!(saved(l), r);
        assert_eq!(h.count("notify_finished"), 1);
        assert_eq!(h.count("restart_olly"), 1);
        assert_eq!(h.calls.last().map(String::as_str), Some("notify_finished"));
        // The staged tree is left for the daemon to clean.
        assert!(e.staged.join("olly").exists());
    }

    #[test]
    fn health_timeout_rolls_back_everything() {
        let e = env();
        let mut h = FakeHost::new(&[Some("0.1.0")]);
        let start = h.clock;
        let r = supervise(&e.layout, &e.staged, "0.1.0", "0.2.0", &mut h);
        assert!(!r.ok);
        assert_eq!(
            r.message,
            "that update didn't work, so I went back to 0.1.0."
        );
        assert!(!r.widget_changed);
        assert_old(&e.layout);
        assert_eq!(
            read(&e.layout.share.join("plasmoid/metadata.json")),
            meta("0.1.0")
        );
        assert_eq!(h.count("restart_olly"), 2);
        // The failed state is cleared before the rollback's restart, never before the first.
        let calls: Vec<&str> = h
            .calls
            .iter()
            .map(String::as_str)
            .filter(|c| *c == "reset_failed" || *c == "restart_olly")
            .collect();
        assert_eq!(calls, ["restart_olly", "reset_failed", "restart_olly"]);
        assert!(h.clock - start >= 30_000, "{}", h.clock - start);
        assert!(h.clock - start <= 31_000, "{}", h.clock - start);
        // The unit changed, so systemd reloads on the way in and on the way back.
        assert_eq!(h.count("daemon_reload"), 2);
        assert_eq!(saved(&e.layout), r);
        assert_eq!(h.count("notify_finished"), 1);
    }

    #[test]
    fn restart_failure_rolls_back() {
        let e = env();
        let mut h = FakeHost::new(&[Some("0.1.0")]);
        h.restart_fails.push_back(true);
        let r = supervise(&e.layout, &e.staged, "0.1.0", "0.2.0", &mut h);
        assert!(!r.ok);
        assert_eq!(
            r.message,
            "that update didn't work, so I went back to 0.1.0."
        );
        assert_old(&e.layout);
        assert_eq!(h.count("restart_olly"), 2);
        // Only the rollback check polled, and the old version answered at once.
        assert_eq!(h.polls, 1);
        assert_eq!(saved(&e.layout), r);
        assert_eq!(h.count("notify_finished"), 1);
    }

    #[test]
    fn missing_staged_file_rolls_back() {
        let e = env();
        fs::remove_file(e.staged.join("dist/olly-notify.service")).unwrap();
        let mut h = FakeHost::new(&[Some("0.1.0")]);
        let r = supervise(&e.layout, &e.staged, "0.1.0", "0.2.0", &mut h);
        assert!(!r.ok);
        assert_eq!(r.message, STAYED);
        assert_old(&e.layout);
        assert_eq!(saved(&e.layout), r);
        // The new binary was copied in but never run, so olly isn't restarted.
        assert_eq!(h.count("restart_olly"), 0);
    }

    const PLAIN_ROLLBACK: &str = "that update didn't work, so I went back to 0.1.0.";
    const STAYED: &str = "I couldn't install 0.2.0, so I stayed on 0.1.0.";

    #[test]
    fn rollback_with_old_version_answering_is_plain() {
        let e = env();
        // New daemon never answers; the old one does straight after rollback.
        let mut h = FakeHost::new(&[None]);
        h.versions = std::iter::repeat_n(None, 61)
            .chain([Some("0.1.0".to_string())])
            .collect();
        let r = supervise(&e.layout, &e.staged, "0.1.0", "0.2.0", &mut h);
        assert!(!r.ok);
        assert_eq!(r.message, PLAIN_ROLLBACK);
        assert_old(&e.layout);
    }

    #[test]
    fn rollback_restart_failing_twice_says_so() {
        let e = env();
        let mut h = FakeHost::new(&[None]);
        h.restart_fails.extend([true, true, true]);
        let start = h.clock;
        let r = supervise(&e.layout, &e.staged, "0.1.0", "0.2.0", &mut h);
        assert!(!r.ok);
        assert_eq!(
            r.message,
            format!(
                "{PLAIN_ROLLBACK} (but olly didn't come back up; run `sudo systemctl status olly`)"
            )
        );
        assert_eq!(h.count("restart_olly"), 3);
        assert!(h.clock - start >= 15_000, "{}", h.clock - start);
        assert!(h.clock - start <= 16_000, "{}", h.clock - start);
        assert_old(&e.layout);
        assert_eq!(saved(&e.layout), r);
        assert_eq!(h.count("notify_finished"), 1);
    }

    #[test]
    fn rollback_restart_retry_succeeds() {
        let e = env();
        let mut h = FakeHost::new(&[Some("0.1.0")]);
        h.restart_fails.extend([true, true, false]);
        let r = supervise(&e.layout, &e.staged, "0.1.0", "0.2.0", &mut h);
        assert_eq!(r.message, PLAIN_ROLLBACK);
        assert_eq!(h.count("restart_olly"), 3);
    }

    #[test]
    fn stale_plasmoid_backup_is_recovered() {
        // A crashed run left the old widget only at `plasmoid.prev`.
        let e = env();
        let share = &e.layout.share;
        fs::rename(share.join("plasmoid"), share.join("plasmoid.prev")).unwrap();
        let mut h = FakeHost::new(&[Some("0.1.0")]);
        let r = supervise(&e.layout, &e.staged, "0.1.0", "0.2.0", &mut h);
        assert!(!r.ok);
        assert_eq!(r.message, PLAIN_ROLLBACK);
        assert_old(&e.layout);
        assert_eq!(read(&share.join("plasmoid/metadata.json")), meta("0.1.0"));
    }

    #[test]
    fn backup_failure_partway_restores_and_installs_nothing() {
        let e = env();
        let l = &e.layout;
        // The unit path is a non-empty directory, so copying it to `.prev` fails after
        // both binaries were backed up.
        let unit = l.unit_dir.join("olly.service");
        fs::remove_file(&unit).unwrap();
        write(&unit.join("inner"), "old-unit-dir");
        let mut h = FakeHost::new(&[Some("0.1.0")]);
        let r = supervise(l, &e.staged, "0.1.0", "0.2.0", &mut h);
        assert!(!r.ok);
        assert_eq!(read(&l.bin.join("olly")), "old-olly");
        assert_eq!(read(&l.bin.join("olly-notify")), "old-notify");
        assert_eq!(read(&unit.join("inner")), "old-unit-dir");
        assert_eq!(
            read(&l.share.join("olly-notify.service")),
            "old-notify-unit"
        );
        assert_eq!(
            read(&l.share.join("plasmoid/contents/ui/main.qml")),
            "old-qml"
        );
        assert_eq!(all_leftovers(l), Vec::<String>::new());
        // Nothing was swapped and olly never left the old binary: no reload, no restart.
        assert_eq!(r.message, STAYED);
        assert_eq!(h.count("daemon_reload"), 0);
        assert_eq!(h.count("restart_olly"), 0);
        assert_eq!(h.count("notify_finished"), 1);
    }

    #[test]
    fn unit_change_triggers_daemon_reload() {
        let e = env_with("new-unit", Some("0.1.0"), "0.1.0");
        let mut h = FakeHost::new(&[Some("0.2.0")]);
        assert!(supervise(&e.layout, &e.staged, "0.1.0", "0.2.0", &mut h).ok);
        assert_eq!(h.count("daemon_reload"), 1);
        let reload = h.calls.iter().position(|c| c == "daemon_reload").unwrap();
        let restart = h.calls.iter().position(|c| c == "restart_olly").unwrap();
        assert!(reload < restart);

        let e = env_with("old-unit", Some("0.1.0"), "0.1.0");
        let mut h = FakeHost::new(&[Some("0.2.0")]);
        assert!(supervise(&e.layout, &e.staged, "0.1.0", "0.2.0", &mut h).ok);
        assert_eq!(h.count("daemon_reload"), 0);
    }

    #[test]
    fn widget_changed_follows_metadata_version() {
        let e = env_with("new-unit", Some("0.1.0"), "0.1.0");
        let mut h = FakeHost::new(&[Some("0.2.0")]);
        assert!(!supervise(&e.layout, &e.staged, "0.1.0", "0.2.0", &mut h).widget_changed);

        let e = env_with("new-unit", Some("0.1.0"), "0.1.1");
        let mut h = FakeHost::new(&[Some("0.2.0")]);
        let r = supervise(&e.layout, &e.staged, "0.1.0", "0.2.0", &mut h);
        assert!(r.widget_changed);
        assert!(saved(&e.layout).widget_changed);

        let e = env_with("new-unit", None, "0.1.0");
        let mut h = FakeHost::new(&[Some("0.2.0")]);
        assert!(supervise(&e.layout, &e.staged, "0.1.0", "0.2.0", &mut h).widget_changed);
    }

    #[test]
    fn missing_old_share_is_fine() {
        // Installed before the widget copy existed: nothing to back up.
        let e = env_with("new-unit", None, "0.1.0");
        fs::remove_file(e.layout.share.join("olly-notify.service")).unwrap();
        let mut h = FakeHost::new(&[Some("0.2.0")]);
        let r = supervise(&e.layout, &e.staged, "0.1.0", "0.2.0", &mut h);
        assert!(r.ok, "{r:?}");
        let l = &e.layout;
        assert_eq!(
            read(&l.share.join("plasmoid/contents/ui/main.qml")),
            "new-qml"
        );
        assert_eq!(
            read(&l.share.join("olly-notify.service")),
            "new-notify-unit"
        );
        assert_eq!(all_leftovers(l), Vec::<String>::new());

        // On rollback, what was installed is removed again.
        let e = env_with("new-unit", None, "0.1.0");
        fs::remove_dir_all(&e.layout.share).unwrap();
        let mut h = FakeHost::new(&[Some("0.1.0")]);
        let r = supervise(&e.layout, &e.staged, "0.1.0", "0.2.0", &mut h);
        assert!(!r.ok);
        let l = &e.layout;
        assert!(!l.share.join("plasmoid").exists());
        assert!(!l.share.join("olly-notify.service").exists());
        assert_eq!(read(&l.bin.join("olly")), "old-olly");
        assert_eq!(all_leftovers(l), Vec::<String>::new());
    }

    #[test]
    fn stale_backups_do_not_block() {
        let e = env();
        write(&e.layout.share.join("plasmoid.prev/junk"), "junk");
        write(&e.layout.share.join("plasmoid.new/junk"), "junk");
        let mut h = FakeHost::new(&[Some("0.2.0")]);
        let r = supervise(&e.layout, &e.staged, "0.1.0", "0.2.0", &mut h);
        assert!(r.ok, "{r:?}");
        assert!(!e.layout.share.join("plasmoid/junk").exists());
        assert_eq!(all_leftovers(&e.layout), Vec::<String>::new());
    }

    #[test]
    fn result_written_even_without_state_dir() {
        let e = env();
        let mut h = FakeHost::new(&[Some("0.2.0")]);
        let r = supervise(&e.layout, &e.staged, "0.1.0", "0.2.0", &mut h);
        assert_eq!(saved(&e.layout), r);
        assert_eq!(r.at_wall_ms, h.clock);
        assert!(!e.layout.state.join("update-result.json.tmp").exists());
    }

    fn journal(l: &Layout, from: &str, to: &str) {
        write(
            &l.journal_path(),
            &format!(r#"{{"from":"{from}","to":"{to}"}}"#),
        );
    }

    /// The state a supervisor killed after installing leaves: new files in place, old ones
    /// at `.prev`, and the journal.
    fn interrupted() -> Env {
        let e = env_with("new-unit", Some("0.1.0"), "0.1.1");
        let l = &e.layout;
        for (dst, new) in [
            (l.bin.join("olly"), "new-olly"),
            (l.bin.join("olly-notify"), "new-notify"),
            (l.unit_dir.join("olly.service"), "new-unit"),
            (l.share.join("olly-notify.service"), "new-notify-unit"),
        ] {
            fs::copy(&dst, sibling(&dst, ".prev")).unwrap();
            write(&dst, new);
        }
        let w = l.share.join("plasmoid");
        fs::rename(&w, sibling(&w, ".prev")).unwrap();
        write(&w.join("metadata.json"), &meta("0.1.1"));
        write(&w.join("contents/ui/main.qml"), "new-qml");
        journal(l, "0.1.0", "0.2.0");
        e
    }

    #[test]
    fn swap_writes_and_removes_the_journal() {
        let e = env();
        let mut h = FakeHost::new(&[Some("0.2.0")]);
        assert!(supervise(&e.layout, &e.staged, "0.1.0", "0.2.0", &mut h).ok);
        assert!(!e.layout.journal_path().exists());
        assert!(!sibling(&e.layout.journal_path(), ".tmp").exists());

        let e = env();
        let mut h = FakeHost::new(&[Some("0.1.0")]);
        assert!(!supervise(&e.layout, &e.staged, "0.1.0", "0.2.0", &mut h).ok);
        assert!(!e.layout.journal_path().exists());
    }

    #[test]
    fn retry_with_journal_keeps_the_original_backup() {
        let e = interrupted();
        let l = &e.layout;
        // The staged release is broken this time too: the rollback must reach the
        // original files, not the half-installed ones.
        let mut h = FakeHost::new(&[Some("0.1.0")]);
        let r = supervise(l, &e.staged, "0.1.0", "0.2.0", &mut h);
        assert!(!r.ok);
        assert_eq!(r.message, PLAIN_ROLLBACK);
        assert_old(l);
        assert_eq!(read(&l.share.join("plasmoid/metadata.json")), meta("0.1.0"));
        assert!(!l.journal_path().exists());

        // And a retry that works drops the old backups.
        let e = interrupted();
        let mut h = FakeHost::new(&[Some("0.2.0")]);
        assert!(supervise(&e.layout, &e.staged, "0.1.0", "0.2.0", &mut h).ok);
        assert_eq!(read(&e.layout.bin.join("olly")), "new-olly");
        assert_eq!(all_leftovers(&e.layout), Vec::<String>::new());
    }

    #[test]
    fn without_journal_a_stale_backup_is_replaced() {
        let e = env();
        let l = &e.layout;
        write(&l.bin.join("olly.prev"), "stale");
        let mut h = FakeHost::new(&[Some("0.1.0")]);
        assert!(!supervise(l, &e.staged, "0.1.0", "0.2.0", &mut h).ok);
        assert_eq!(read(&l.bin.join("olly")), "old-olly");
    }

    #[test]
    fn recover_on_the_old_version_restores_files() {
        let e = interrupted();
        let l = &e.layout;
        let r = recover(l, "0.1.0", 42, &mut || {}).expect("a result");
        assert!(!r.ok);
        assert_eq!((r.from.as_str(), r.to.as_str()), ("0.1.0", "0.2.0"));
        assert_eq!(r.message, PLAIN_ROLLBACK);
        assert_eq!(r.at_wall_ms, 42);
        assert!(!r.widget_changed);
        assert_old(l);
        assert_eq!(read(&l.share.join("plasmoid/metadata.json")), meta("0.1.0"));
        assert_eq!(saved(l), r);
        assert!(!l.journal_path().exists());
    }

    #[test]
    fn recover_on_the_new_version_keeps_it() {
        let e = interrupted();
        let l = &e.layout;
        let r = recover(l, "0.2.0", 42, &mut || {}).expect("a result");
        assert!(r.ok);
        assert_eq!(r.message, "updated to 0.2.0! \u{2728}");
        assert!(r.widget_changed);
        assert_eq!(read(&l.bin.join("olly")), "new-olly");
        assert_eq!(
            read(&l.share.join("plasmoid/contents/ui/main.qml")),
            "new-qml"
        );
        // The old copies stay until olly has run a while.
        assert_eq!(read(&l.bin.join("olly.prev")), "old-olly");
        assert_eq!(saved(l), r);
        assert!(!l.journal_path().exists());
        drop_backups(l);
        assert_eq!(all_leftovers(l), Vec::<String>::new());
        assert_eq!(read(&l.bin.join("olly")), "new-olly");
    }

    #[test]
    fn recover_without_journal_does_nothing() {
        let e = env();
        let l = &e.layout;
        write(&l.bin.join("olly.prev"), "something");
        assert_eq!(recover(l, "0.1.0", 42, &mut || {}), None);
        assert_eq!(read(&l.bin.join("olly.prev")), "something");
        assert_eq!(read(&l.bin.join("olly")), "old-olly");
        assert!(!l.result_path().exists());
    }

    #[test]
    fn recover_for_another_version_leaves_everything() {
        let e = interrupted();
        let l = &e.layout;
        assert_eq!(recover(l, "0.3.0", 42, &mut || {}), None);
        assert!(l.journal_path().exists());
        assert_eq!(read(&l.bin.join("olly")), "new-olly");
        assert_eq!(read(&l.bin.join("olly.prev")), "old-olly");
        assert!(!l.result_path().exists());
    }

    #[test]
    fn recover_leaves_a_running_swap_alone() {
        let e = interrupted();
        let l = &e.layout;
        // A live supervisor holds the journal lock.
        let _lock = write_journal(l, "0.1.0", "0.2.0").unwrap();
        assert_eq!(recover(l, "0.2.0", 42, &mut || {}), None);
        assert!(l.journal_path().exists());
        assert_eq!(read(&l.bin.join("olly.prev")), "old-olly");
        assert!(!l.result_path().exists());
    }

    #[test]
    fn recover_reloads_systemd_after_putting_the_old_unit_back() {
        let e = interrupted();
        let mut reloads = 0;
        recover(&e.layout, "0.1.0", 42, &mut || reloads += 1).expect("a result");
        assert_eq!(read(&e.layout.unit_dir.join("olly.service")), "old-unit");
        assert_eq!(reloads, 1);

        // Interrupted before the unit was backed up: nothing to reload for.
        let e = interrupted();
        let unit = e.layout.unit_dir.join("olly.service");
        fs::rename(sibling(&unit, ".prev"), &unit).unwrap();
        let mut reloads = 0;
        recover(&e.layout, "0.1.0", 42, &mut || reloads += 1).expect("a result");
        assert_eq!(reloads, 0);
    }

    #[test]
    fn recover_reads_the_journal_it_locked() {
        let e = interrupted();
        let l = &e.layout;
        // Garbage at the path is reported, not acted on, and the files stay put.
        write(&l.journal_path(), "not json");
        assert_eq!(recover(l, "0.1.0", 42, &mut || {}), None);
        assert_eq!(read(&l.bin.join("olly")), "new-olly");
        assert_eq!(read(&l.bin.join("olly.prev")), "old-olly");
    }

    #[test]
    fn drop_backups_waits_for_the_journal() {
        let e = interrupted();
        drop_backups(&e.layout);
        assert_eq!(read(&e.layout.bin.join("olly.prev")), "old-olly");
    }

    #[test]
    fn a_finished_swap_journal_does_not_leak_into_the_next_update() {
        // 0.1.0 -> 0.2.0 finished but its supervisor died, and 0.2.0's start skipped the
        // still-locked journal. Now 0.2.0 updates to 0.3.0, which fails.
        let e = interrupted();
        let l = &e.layout;
        let mut h = FakeHost::new(&[Some("0.2.0")]);
        let r = supervise(l, &e.staged, "0.2.0", "0.3.0", &mut h);
        assert!(!r.ok);
        assert_eq!(
            r.message,
            "that update didn't work, so I went back to 0.2.0."
        );
        // Back on the 0.2.0 files, not 0.1.0's backups.
        assert_eq!(read(&l.bin.join("olly")), "new-olly");
        assert_eq!(read(&l.unit_dir.join("olly.service")), "new-unit");
        assert_eq!(
            read(&l.share.join("plasmoid/contents/ui/main.qml")),
            "new-qml"
        );
        assert_eq!(all_leftovers(l), Vec::<String>::new());
        assert!(!l.journal_path().exists());
    }

    #[test]
    fn a_journal_for_another_version_is_dropped_not_trusted() {
        // Left by a manual reinstall over a dead swap: its `.prev` files are of no use.
        let e = env();
        let l = &e.layout;
        write(&l.bin.join("olly.prev"), "ancient");
        journal(l, "0.0.1", "0.0.2");
        let mut h = FakeHost::new(&[Some("0.1.0")]);
        let r = supervise(l, &e.staged, "0.1.0", "0.2.0", &mut h);
        assert!(!r.ok);
        assert_eq!(r.message, PLAIN_ROLLBACK);
        assert_old(l);
        assert!(!l.journal_path().exists());
    }

    #[test]
    fn a_live_swap_blocks_another_without_touching_anything() {
        let e = interrupted();
        let l = &e.layout;
        let _lock = write_journal(l, "0.1.0", "0.2.0").unwrap();
        let mut h = FakeHost::new(&[Some("0.1.0")]);
        let r = supervise(l, &e.staged, "0.1.0", "0.2.0", &mut h);
        assert!(!r.ok);
        assert_eq!(
            r.message,
            "another update is still swapping files, so I stayed on 0.1.0."
        );
        assert_eq!(read(&l.bin.join("olly")), "new-olly");
        assert_eq!(read(&l.bin.join("olly.prev")), "old-olly");
        assert!(l.journal_path().exists());
        assert_eq!(h.count("restart_olly"), 0);
        assert_eq!(h.count("notify_finished"), 1);
        assert_eq!(saved(l), r);
    }

    #[test]
    fn a_failed_journal_write_changes_nothing() {
        let e = env();
        let l = &e.layout;
        // The journal's directory is a file, so the journal can't be written.
        write(&l.state.join("update"), "in the way");
        let mut h = FakeHost::new(&[Some("0.1.0")]);
        let r = supervise(l, &e.staged, "0.1.0", "0.2.0", &mut h);
        assert!(!r.ok);
        assert_eq!(r.message, STAYED);
        assert_old(l);
        assert_eq!(h.count("restart_olly"), 0);
        assert_eq!(h.count("reset_failed"), 0);
    }
}
