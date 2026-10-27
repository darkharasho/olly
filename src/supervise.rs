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
}

/// What the supervisor needs from the system, so tests can fake it.
pub trait Host {
    fn daemon_reload(&mut self) -> Result<(), String>;
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
    /// directory is renamed.
    fn back_up(&mut self) -> Result<(), String> {
        let prev = self.prev();
        remove_any(&prev).map_err(|e| err("clear", &prev, e))?;
        if !exists(&self.dst) {
            self.backed_up = Some(false);
            return Ok(());
        }
        match self.kind {
            Kind::File(_) => {
                fs::copy(&self.dst, &prev).map_err(|e| err("back up", &self.dst, e))?;
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

fn widget_version(dir: &Path) -> Option<String> {
    let text = fs::read_to_string(dir.join("metadata.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v["KPlugin"]["Version"].as_str().map(String::from)
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

/// Polls until the daemon reports `to`, for up to `HEALTH_MS` (by sleeps and by the clock).
fn healthy(host: &mut dyn Host, to: &str) -> bool {
    let start = host.wall_ms();
    let mut waited = 0;
    loop {
        if host.status_version().as_deref() == Some(to) {
            return true;
        }
        let elapsed = host.wall_ms().saturating_sub(start);
        if waited >= HEALTH_MS || elapsed >= HEALTH_MS {
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
    let item = |src: PathBuf, dst: PathBuf, kind| Item {
        src,
        dst,
        kind,
        backed_up: None,
        installed: false,
    };
    const UNIT: usize = 2;
    const WIDGET: usize = 3;
    let mut items = [
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
    ];

    let unit_changed = match (fs::read(&items[UNIT].src), fs::read(&items[UNIT].dst)) {
        (Ok(new), Ok(old)) => new != old,
        _ => true,
    };
    let old_widget = widget_version(&items[WIDGET].dst);
    let widget_changed = old_widget.is_none() || widget_version(&items[WIDGET].src) != old_widget;

    let swap = |items: &mut [Item], host: &mut dyn Host| -> Result<(), String> {
        for it in items.iter_mut() {
            it.back_up()?;
        }
        for it in items.iter_mut() {
            it.install()?;
        }
        if unit_changed {
            host.daemon_reload()?;
        }
        host.restart_olly()?;
        if healthy(host, to) {
            Ok(())
        } else {
            Err(format!("olly didn't report {to} within {HEALTH_MS} ms"))
        }
    };

    let ok = match swap(&mut items, host) {
        Ok(()) => {
            for it in &items {
                it.drop_backup();
            }
            true
        }
        Err(e) => {
            eprintln!("\u{1F916} update to {to} failed, rolling back: {e}");
            let mut restored = true;
            for it in items.iter().rev() {
                restored &= it.restore();
            }
            if !restored {
                eprintln!("\u{1F916} some files couldn't be put back; see above.");
            }
            if items[UNIT].installed && unit_changed {
                if let Err(e) = host.daemon_reload() {
                    eprintln!("\u{1F916} {e}");
                }
            }
            if let Err(e) = host.restart_olly() {
                eprintln!("\u{1F916} {e}");
            }
            false
        }
    };

    let result = LastUpdate {
        from: from.to_string(),
        to: to.to_string(),
        ok,
        at_wall_ms: host.wall_ms(),
        message: if ok {
            format!("updated to {to}! \u{2728}")
        } else {
            format!("that update didn't work, so I went back to {from}.")
        },
        widget_changed: ok && widget_changed,
    };
    write_result(layout, &result);
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
        let mut h = FakeHost::new(&[Some("0.2.0")]);
        h.restart_fails.push_back(true);
        let r = supervise(&e.layout, &e.staged, "0.1.0", "0.2.0", &mut h);
        assert!(!r.ok);
        assert_eq!(
            r.message,
            "that update didn't work, so I went back to 0.1.0."
        );
        assert_old(&e.layout);
        assert_eq!(h.count("restart_olly"), 2);
        assert_eq!(h.polls, 0);
        assert_eq!(saved(&e.layout), r);
        assert_eq!(h.count("notify_finished"), 1);
    }

    #[test]
    fn missing_staged_file_rolls_back() {
        let e = env();
        fs::remove_file(e.staged.join("dist/olly-notify.service")).unwrap();
        let mut h = FakeHost::new(&[Some("0.2.0")]);
        let r = supervise(&e.layout, &e.staged, "0.1.0", "0.2.0", &mut h);
        assert!(!r.ok);
        assert_old(&e.layout);
        assert_eq!(saved(&e.layout), r);
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
}
