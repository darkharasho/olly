use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn run(sock: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_olly"))
        .args(["status", "--json"])
        .env("OLLY_SOCKET", sock)
        .output()
        .unwrap()
}

fn reply_with(listener: UnixListener, reply: &'static str) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(s.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        s.write_all(reply.as_bytes()).unwrap();
    })
}

#[test]
fn json_offline_when_no_socket() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(&dir.path().join("nope.sock"));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "{\"offline\":true}\n");
    assert!(out.stderr.is_empty());
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn json_offline_when_daemon_silent() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("o.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    std::thread::spawn(move || {
        let _conn = listener.accept();
        std::thread::sleep(Duration::from_secs(5));
    });
    let t = Instant::now();
    let out = run(&sock);
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "{\"offline\":true}\n");
    assert!(out.stderr.is_empty());
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn json_offline_when_daemon_trickles() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("o.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        while s.write_all(b"x").is_ok() {
            std::thread::sleep(Duration::from_millis(300));
        }
    });
    let t = Instant::now();
    let out = run(&sock);
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "{\"offline\":true}\n");
    assert!(out.stderr.is_empty());
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn json_offline_on_eof() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("o.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let h = std::thread::spawn(move || {
        let (s, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).unwrap();
    });
    let out = run(&sock);
    h.join().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "{\"offline\":true}\n");
    assert!(out.stderr.is_empty());
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn json_prints_data() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("o.sock");
    let h = reply_with(
        UnixListener::bind(&sock).unwrap(),
        "{\"ok\":true,\"data\":{\"level\":\"calm\",\"x\":1}}\n",
    );
    let out = run(&sock);
    h.join().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v, serde_json::json!({"level":"calm","x":1}));
    assert!(out.stderr.is_empty());
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn json_error_reply() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("o.sock");
    let h = reply_with(
        UnixListener::bind(&sock).unwrap(),
        "{\"ok\":false,\"error\":\"nope\"}\n",
    );
    let out = run(&sock);
    h.join().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "{\"error\":\"nope\"}\n"
    );
    assert!(out.stderr.is_empty());
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn update_supervise_refuses_without_root() {
    // As root this would really swap files, so it only runs unprivileged.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_olly"))
        .arg("update-supervise")
        .arg(dir.path())
        .args(["0.1.0", "0.2.0"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(err.lines().count(), 1, "{err}");
    assert!(err.contains("only runs as root"), "{err}");
}
