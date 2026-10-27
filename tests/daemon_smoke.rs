//! Runs the real daemon against the fixture root in dry-run mode.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Kills the daemon if the test fails before it exits on its own.
struct Daemon(Option<Child>);

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Daemon {
    fn pid(&self) -> i32 {
        self.0.as_ref().unwrap().id() as i32
    }

    fn wait_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if let Some(s) = self.0.as_mut().unwrap().try_wait().unwrap() {
                self.0 = None;
                return Some(s);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }
}

fn connect(sock: &Path, d: &mut Daemon, log: &Path) -> UnixStream {
    let start = Instant::now();
    loop {
        if let Ok(s) = UnixStream::connect(sock) {
            s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            return s;
        }
        if let Some(st) = d.0.as_mut().unwrap().try_wait().unwrap() {
            d.0 = None;
            panic!(
                "daemon exited early ({st}): {}",
                std::fs::read_to_string(log).unwrap_or_default()
            );
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "socket never appeared"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn request(s: &UnixStream, line: &str) -> serde_json::Value {
    (&*s).write_all(format!("{line}\n").as_bytes()).unwrap();
    let mut resp = String::new();
    BufReader::new(s).read_line(&mut resp).unwrap();
    serde_json::from_str(&resp).unwrap_or_else(|e| panic!("bad response {resp:?}: {e}"))
}

/// Recursive copy that keeps symlinks (the fixture's `exe` links) as symlinks.
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let (src, dst) = (e.path(), to.join(e.file_name()));
        let ft = e.file_type().unwrap();
        if ft.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&src).unwrap(), &dst).unwrap();
        } else if ft.is_dir() {
            copy_tree(&src, &dst);
        } else {
            std::fs::copy(&src, &dst).unwrap();
        }
    }
}

/// Runs the daemon on a private copy of the fixture, so it can never write a checked-in file.
fn spawn(tmp: &Path, config: &str) -> (Daemon, PathBuf, PathBuf) {
    let root = tmp.join("root");
    copy_tree(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/steam"),
        &root,
    );
    spawn_on(tmp, &root, config)
}

fn spawn_on(tmp: &Path, root: &Path, config: &str) -> (Daemon, PathBuf, PathBuf) {
    let cfg = tmp.join("config.toml");
    std::fs::write(&cfg, config).unwrap();
    let log = tmp.join("stderr.log");
    let child = Command::new(env!("CARGO_BIN_EXE_olly"))
        .args(["daemon", "--root"])
        .arg(root)
        .arg("--config")
        .arg(&cfg)
        .env("OLLY_RUN_DIR", tmp)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    (Daemon(Some(child)), tmp.join("olly.sock"), log)
}

fn stop(mut d: Daemon, log: &Path) -> String {
    assert_eq!(unsafe { libc::kill(d.pid(), libc::SIGTERM) }, 0);
    let status = d
        .wait_exit(Duration::from_secs(10))
        .expect("daemon did not exit after SIGTERM");
    let stderr = std::fs::read_to_string(log).unwrap();
    assert!(status.success(), "exit {status}: {stderr}");
    stderr
}

#[test]
fn daemon_smoke() {
    let tmp = tempfile::tempdir().unwrap();
    let uid = unsafe { libc::getuid() };
    let (mut d, sock, log) = spawn(
        tmp.path(),
        &format!("dry_run = true\ndesktop_uid = {uid}\n"),
    );

    let s = connect(&sock, &mut d, &log);
    let v = request(&s, r#"{"cmd":"status"}"#);
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["data"]["dry_run"], true, "{v}");
    assert_eq!(v["data"]["level"], "calm", "{v}");
    assert_eq!(v["data"]["mem_total"], 31953848u64 * 1024, "{v}");
    assert!(
        v["data"]["message"].as_str().unwrap().contains("olly"),
        "{v}"
    );

    let v = request(&s, r#"{"cmd":"pause","secs":999999}"#);
    assert_eq!(v["ok"], false, "{v}");
    let v = request(&s, r#"{"cmd":"pause","secs":60}"#);
    assert_eq!(v["ok"], true, "{v}");
    assert!(v["data"]["paused_until_wall_ms"].as_u64().is_some(), "{v}");
    let v = request(&s, r#"{"cmd":"resume"}"#);
    assert_eq!(
        v["data"]["paused_until_wall_ms"],
        serde_json::Value::Null,
        "{v}"
    );
    let v = request(&s, r#"{"cmd":"incidents"}"#);
    assert_eq!(v["data"], serde_json::json!([]), "{v}");
    let v = request(&s, r#"{"cmd":"frozen_kill","id":5}"#);
    assert_eq!(v["ok"], false, "{v}");
    drop(s);

    let stderr = stop(d, &log);
    assert!(!sock.exists(), "socket left behind");
    assert!(stderr.contains("PSI trigger unavailable"), "{stderr}");
}

/// An unreadable config runs live on defaults (desktop_uid 1000) and tells subscribers.
#[test]
fn config_error_notice_reaches_subscriber() {
    if unsafe { libc::getuid() } != 1000 {
        eprintln!("skipped: the default desktop_uid (1000) must be the test user");
        return;
    }
    // This test runs LIVE (defaults => dry_run = false) with a real SysActuator. It is only
    // safe because the fixture is calm (about 47% available, PSI 0), so the engine never acts.
    // If it did, it would write cgroup files in the copied root and could signal host pids
    // that happen to match the fixture's pids (100-300). The assertion below fails loudly
    // if any action is taken; keep the fixture calm.
    let tmp = tempfile::tempdir().unwrap();
    let (mut d, sock, log) = spawn(tmp.path(), "dry_run = [[");
    let s = connect(&sock, &mut d, &log);
    (&s).write_all(b"{\"cmd\":\"subscribe\"}\n").unwrap();
    // One reader for both lines: the notice may arrive in the same read as the ack.
    let mut r = BufReader::new(&s);
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    assert!(line.contains("subscribed"), "{line}");
    line.clear();
    r.read_line(&mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    let body = v["event"]["Notice"]["message"]["body"]
        .as_str()
        .unwrap_or_else(|| panic!("{v}"));
    assert!(body.contains("not a dry run"), "{v}");
    drop(r);
    drop(s);
    let stderr = stop(d, &log);
    assert!(stderr.contains("ERROR"), "{stderr}");
    assert!(
        !stderr.contains("action="),
        "the live daemon acted on the fixture: {stderr}"
    );
}

/// SIGTERM during an open incident saves it, marked as cut short.
#[test]
fn sigterm_flushes_open_incident() {
    let tmp = tempfile::tempdir().unwrap();
    let uid = unsafe { libc::getuid() };
    let (mut d, sock, log) = {
        // Pre-create the copy with 8% available (warning level), then spawn on it.
        let root = tmp.path().join("root");
        copy_tree(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/steam"),
            &root,
        );
        let mi = root.join("proc/meminfo");
        let text = std::fs::read_to_string(&mi).unwrap();
        let low: String = text
            .lines()
            .map(|l| {
                if l.starts_with("MemAvailable:") {
                    "MemAvailable:    2556307 kB"
                } else {
                    l
                }
            })
            .map(|l| format!("{l}\n"))
            .collect();
        std::fs::remove_file(&mi).unwrap();
        std::fs::write(&mi, low).unwrap();
        spawn_on(
            tmp.path(),
            &root,
            &format!("dry_run = true\ndesktop_uid = {uid}\n"),
        )
    };
    let s = connect(&sock, &mut d, &log);
    let v = request(&s, r#"{"cmd":"status"}"#);
    assert_eq!(v["data"]["level"], "warning", "{v}");
    drop(s);
    stop(d, &log);
    let text = std::fs::read_to_string(tmp.path().join("incidents.jsonl")).unwrap();
    let inc: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(inc["type"], "incident", "{inc}");
    assert!(
        inc["summary"]
            .as_str()
            .unwrap()
            .contains("olly was stopped"),
        "{inc}"
    );
}
