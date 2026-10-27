//! Local IPC: newline-delimited JSON over a unix socket. See docs/ipc.md.

use crate::engine::{Event, Level, Target};
use crate::incident::TopApp;
use crate::supervise::LastUpdate;
use crate::update::{UpdateError, UpdateInfo, UpdateState};
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub const MAX_LINE: usize = 64 * 1024;
pub const MAX_CONNS: usize = 8;
const SUB_QUEUE: usize = 64;
const SUB_FULL_LIMIT: u8 = 3;
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(not(test))]
const PROBE_INTERVAL: Duration = Duration::from_secs(20);
#[cfg(test)]
const PROBE_INTERVAL: Duration = Duration::from_millis(200);
#[cfg(not(test))]
const IDLE_READ_TIMEOUT: Duration = Duration::from_secs(300);
#[cfg(test)]
const IDLE_READ_TIMEOUT: Duration = Duration::from_secs(2);
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
/// `update_check` waits for the fetch.
const UPDATE_CHECK_TIMEOUT: Duration = Duration::from_secs(75);
const CONN_STACK: usize = 256 * 1024;

fn d10() -> usize {
    10
}

#[derive(Deserialize, Serialize, Debug, PartialEq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    Incidents {
        #[serde(default = "d10")]
        limit: usize,
        #[serde(default)]
        offset: usize,
    },
    Incident {
        id: u64,
    },
    Subscribe,
    Pause {
        secs: u64,
    },
    Resume,
    AllowlistAdd {
        name: String,
    },
    AllowlistList,
    FrozenKill {
        id: u64,
    },
    FrozenResume {
        id: u64,
    },
    /// Look for a new release now; answered when the check finishes.
    UpdateCheck,
    /// Download and install the known new release.
    Update,
    /// From the supervisor: re-read `update-result.json`.
    UpdateFinished,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct StatusData {
    pub level: Level,
    pub dry_run: bool,
    pub paused_until_wall_ms: Option<u64>,
    pub mem_available: u64,
    pub mem_total: u64,
    pub throttled: Vec<Target>,
    pub frozen: Vec<Target>,
    pub self_rss: u64,
    pub message: String,
    #[serde(default)]
    pub swap_total: u64,
    #[serde(default)]
    pub swap_used: u64,
    #[serde(default)]
    pub some_avg10: f32,
    #[serde(default)]
    pub top: Option<TopApp>,
    #[serde(default)]
    pub incident_active: bool,
    #[serde(default)]
    pub incident_acted: bool,
    #[serde(default)]
    pub last_action_wall_ms: Option<u64>,
    #[serde(default)]
    pub last_incident_id: Option<u64>,
    #[serde(default)]
    pub frozen_info: Vec<FrozenInfo>,
    /// The running daemon's version.
    #[serde(default)]
    pub version: String,
    /// A newer release, when olly knows of one.
    #[serde(default)]
    pub update: Option<UpdateInfo>,
    #[serde(default)]
    pub update_state: UpdateState,
    #[serde(default)]
    pub last_update: Option<LastUpdate>,
    /// `KPlugin.Version` of the widget copy olly ships.
    #[serde(default)]
    pub widget_version: Option<String>,
    /// Why the latest update stopped before the hand-off, cleared when a new one starts.
    #[serde(default)]
    pub update_error: Option<UpdateError>,
    /// Swap is over half full and has grown by 1 GiB or more in the last 15 minutes.
    #[serde(default)]
    pub swap_warning: bool,
    /// The running game's display name (the largest if several), while game mode is on.
    #[serde(default)]
    pub game: Option<String>,
}

/// What the tray shows for one frozen app.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct FrozenInfo {
    pub id: u64,
    pub name: String,
    pub bytes: u64,
    pub rate: f64,
    pub kill_at_wall_ms: u64,
}

pub enum ToDaemon {
    /// The daemon answers with one response line (no trailing newline).
    Req(Request, mpsc::Sender<String>),
    /// The daemon pushes stream lines (no trailing newline) with `Subscribers`.
    Subscribe(mpsc::SyncSender<String>),
}

// Structs rather than json!{} so field order is stable ("ok" first).
pub fn ok_line<T: Serialize>(data: &T) -> String {
    #[derive(Serialize)]
    struct Ok<'a, T> {
        ok: bool,
        data: &'a T,
    }
    serde_json::to_string(&Ok { ok: true, data }).unwrap_or_else(|e| err_line(&e.to_string()))
}

pub fn err_line(msg: &str) -> String {
    #[derive(Serialize)]
    struct Err<'a> {
        ok: bool,
        error: &'a str,
    }
    serde_json::to_string(&Err {
        ok: false,
        error: msg,
    })
    .expect("string serializes")
}

/// `{"event":<Event>,"wall_ms":<u64>}`
pub fn event_line(ev: &Event, wall_ms: u64) -> String {
    #[derive(Serialize)]
    struct Ev<'a> {
        event: &'a Event,
        wall_ms: u64,
    }
    serde_json::to_string(&Ev { event: ev, wall_ms }).unwrap_or_else(|e| err_line(&e.to_string()))
}

/// Parse and validate one request line. `Err` is the error message to send back.
/// The upper bound on `Pause` is the daemon's job; it owns the config.
pub fn handle_line(line: &str) -> Result<Request, String> {
    let req: Request = serde_json::from_str(line).map_err(|e| format!("bad request: {e}"))?;
    match &req {
        Request::Pause { secs: 0 } => {
            Err("pause needs a duration of at least 1 second".to_string())
        }
        Request::AllowlistAdd { name } if name.trim().is_empty() => {
            Err("allowlist name must not be empty".to_string())
        }
        Request::AllowlistAdd { name } if name.contains('*') => {
            Err("allowlist name must not contain '*'".to_string())
        }
        _ => Ok(req),
    }
}

/// Daemon-side subscriber set: never blocks, drops dead or persistently slow subscribers.
#[derive(Default)]
pub struct Subscribers {
    /// (sender, consecutive Full count)
    subs: Vec<(mpsc::SyncSender<String>, u8)>,
}

impl Subscribers {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, tx: mpsc::SyncSender<String>) {
        self.subs.push((tx, 0));
    }

    /// `try_send` to everyone. Dropped on `Disconnected`, or on `Full` 3 times in a row.
    pub fn broadcast(&mut self, line: &str) {
        self.subs
            .retain_mut(|(tx, full)| match tx.try_send(line.to_string()) {
                Ok(()) => {
                    *full = 0;
                    true
                }
                Err(mpsc::TrySendError::Full(_)) => {
                    *full += 1;
                    *full < SUB_FULL_LIMIT
                }
                Err(mpsc::TrySendError::Disconnected(_)) => false,
            });
    }

    pub fn len(&self) -> usize {
        self.subs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.subs.is_empty()
    }
}

struct Shared {
    allowed: [u32; 2],
    tx: mpsc::Sender<ToDaemon>,
    wake: RawFd,
    conns: AtomicUsize,
}

impl Shared {
    fn send(&self, msg: ToDaemon) -> bool {
        if self.tx.send(msg).is_err() {
            return false;
        }
        let one = 1u64;
        // EAGAIN means the counter is already saturated, i.e. the daemon will wake anyway.
        unsafe { libc::write(self.wake, &one as *const u64 as *const libc::c_void, 8) };
        true
    }
}

struct ConnSlot(Arc<Shared>);

impl Drop for ConnSlot {
    fn drop(&mut self) {
        self.0.conns.fetch_sub(1, Ordering::SeqCst);
    }
}

fn peer_uid(s: &UnixStream) -> Option<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    (rc == 0).then_some(cred.uid)
}

fn write_line(mut s: &UnixStream, line: &str) -> io::Result<()> {
    let mut buf = Vec::with_capacity(line.len() + 1);
    buf.extend_from_slice(line.as_bytes());
    buf.push(b'\n');
    s.write_all(&buf)
}

/// Start the accept loop on its own thread. The thread ends when the listener is
/// shut down (`libc::shutdown` on its fd) or closed.
pub fn serve(
    listener: UnixListener,
    allowed_uids: [u32; 2],
    tx: mpsc::Sender<ToDaemon>,
    wake: RawFd,
) -> JoinHandle<()> {
    let shared = Arc::new(Shared {
        allowed: allowed_uids,
        tx,
        wake,
        conns: AtomicUsize::new(0),
    });
    thread::Builder::new()
        .name("olly-ipc".into())
        .stack_size(64 * 1024)
        .spawn(move || accept_loop(listener, shared))
        .expect("spawn ipc accept thread")
}

fn accept_loop(listener: UnixListener, shared: Arc<Shared>) {
    loop {
        let stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(e) => match e.raw_os_error() {
                // Listener shut down or closed.
                Some(libc::EINVAL) | Some(libc::EBADF) => return,
                // EMFILE and friends: back off instead of spinning.
                Some(libc::EMFILE) | Some(libc::ENFILE) | Some(libc::ENOMEM)
                | Some(libc::ENOBUFS) => {
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
                _ => continue,
            },
        };
        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
        match peer_uid(&stream) {
            Some(uid) if shared.allowed.contains(&uid) => {}
            _ => {
                let _ = write_line(&stream, &err_line("not allowed"));
                continue;
            }
        }
        if shared.conns.fetch_add(1, Ordering::SeqCst) >= MAX_CONNS {
            shared.conns.fetch_sub(1, Ordering::SeqCst);
            let _ = write_line(&stream, &err_line("busy: too many connections"));
            continue;
        }
        let slot = ConnSlot(shared.clone());
        let spawned = thread::Builder::new()
            .name("olly-ipc-conn".into())
            .stack_size(CONN_STACK)
            .spawn(move || {
                let slot = slot;
                connection(stream, &slot.0);
            });
        // On failure the closure (and its slot) is dropped, releasing the count.
        let _ = spawned;
    }
}

enum ReadLine {
    Line(String),
    TooLong,
    Eof,
}

fn read_request_line<R: BufRead>(r: &mut R) -> io::Result<ReadLine> {
    let mut buf = Vec::new();
    let n = r
        .by_ref()
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(ReadLine::Eof);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
    } else if buf.len() > MAX_LINE {
        return Ok(ReadLine::TooLong);
    }
    Ok(ReadLine::Line(String::from_utf8_lossy(&buf).into_owned()))
}

/// Discard what the client already sent so closing doesn't reset the connection
/// before it has read our error. Bounded in bytes and time.
fn drain(stream: &UnixStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let mut sink = [0u8; 4096];
    let mut left = 1024 * 1024;
    let mut r = stream;
    while left > 0 {
        match r.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(n) => left -= n.min(left),
        }
    }
}

fn connection(stream: UnixStream, shared: &Shared) {
    // An idle client must not hold a slot forever; a timeout ends the read with an error.
    let _ = stream.set_read_timeout(Some(IDLE_READ_TIMEOUT));
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    loop {
        let line = match read_request_line(&mut reader) {
            Ok(ReadLine::Line(l)) => l,
            Ok(ReadLine::TooLong) => {
                let _ = write_line(&stream, &err_line("request too long"));
                drain(&stream);
                return;
            }
            Ok(ReadLine::Eof) | Err(_) => return,
        };
        let req = match handle_line(&line) {
            Ok(r) => r,
            Err(msg) => {
                if write_line(&stream, &err_line(&msg)).is_err() {
                    return;
                }
                continue;
            }
        };
        if req == Request::Subscribe {
            stream_events(&stream, shared);
            return;
        }
        let wait = if req == Request::UpdateCheck {
            UPDATE_CHECK_TIMEOUT
        } else {
            REPLY_TIMEOUT
        };
        let (rtx, rrx) = mpsc::channel();
        if !shared.send(ToDaemon::Req(req, rtx)) {
            let _ = write_line(&stream, &err_line("daemon unavailable"));
            return;
        }
        let resp = match rrx.recv_timeout(wait) {
            Ok(r) => r,
            Err(_) => err_line("daemon did not answer"),
        };
        if write_line(&stream, &resp).is_err() {
            return;
        }
    }
}

fn stream_events(stream: &UnixStream, shared: &Shared) {
    let (stx, srx) = mpsc::sync_channel(SUB_QUEUE);
    // Ack first so it always precedes the first event.
    if write_line(stream, &ok_line(&"subscribed")).is_err() {
        return;
    }
    if !shared.send(ToDaemon::Subscribe(stx)) {
        let _ = write_line(stream, &err_line("daemon unavailable"));
        return;
    }
    // Ends when the daemon drops the sender or a write fails (client gone or stuck);
    // dropping `srx` then makes the daemon's next try_send see Disconnected.
    loop {
        match srx.recv_timeout(PROBE_INTERVAL) {
            Ok(line) => {
                if write_line(stream, &line).is_err() {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if peer_closed(stream) {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// True if the peer hung up or the socket errored (non-blocking probe).
fn peer_closed(s: &UnixStream) -> bool {
    let mut pfd = libc::pollfd {
        fd: s.as_raw_fd(),
        events: libc::POLLRDHUP,
        revents: 0,
    };
    let rc = unsafe { libc::poll(&mut pfd, 1, 0) };
    rc < 0 || pfd.revents & (libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR) != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::Message;
    use std::io::BufReader;

    #[test]
    fn old_status_without_new_fields_parses() {
        let st: StatusData = serde_json::from_str(
            r#"{"level":"calm","dry_run":true,"paused_until_wall_ms":null,
            "mem_available":15227060000,"mem_total":31953848000,"throttled":[],"frozen":[],
            "self_rss":3200000,"message":"..."}"#,
        )
        .unwrap();
        assert!(st.frozen_info.is_empty());
        assert!(st.top.is_none());
        assert!(!st.incident_active && !st.incident_acted);
        assert_eq!((st.swap_total, st.swap_used), (0, 0));
        assert_eq!(st.last_action_wall_ms, None);
        assert_eq!(st.last_incident_id, None);
        assert_eq!(st.version, "");
        assert_eq!(st.update, None);
        assert_eq!(st.update_state, crate::update::UpdateState::Idle);
        assert_eq!(st.last_update, None);
        assert_eq!(st.widget_version, None);
        assert_eq!(st.update_error, None);
    }

    struct Harness {
        _dir: tempfile::TempDir,
        path: std::path::PathBuf,
        rx: mpsc::Receiver<ToDaemon>,
        wake: RawFd,
        shut: UnixListener,
        handle: Option<JoinHandle<()>>,
    }

    impl Harness {
        fn new(allowed: [u32; 2]) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("olly.sock");
            let listener = UnixListener::bind(&path).unwrap();
            let shut = listener.try_clone().unwrap();
            let wake = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
            assert!(wake >= 0);
            let (tx, rx) = mpsc::channel();
            let handle = serve(listener, allowed, tx, wake);
            Harness {
                _dir: dir,
                path,
                rx,
                wake,
                shut,
                handle: Some(handle),
            }
        }
        fn me() -> Self {
            Self::new([unsafe { libc::getuid() }, 0])
        }
        fn connect(&self) -> UnixStream {
            let s = UnixStream::connect(&self.path).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            s
        }
        fn wake_count(&self) -> u64 {
            let mut v = 0u64;
            let n = unsafe { libc::read(self.wake, &mut v as *mut u64 as *mut _, 8) };
            if n == 8 {
                v
            } else {
                0
            }
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            unsafe { libc::shutdown(self.shut.as_raw_fd(), libc::SHUT_RDWR) };
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
            unsafe { libc::close(self.wake) };
        }
    }

    fn read_line(s: &UnixStream) -> String {
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).unwrap();
        line
    }

    #[test]
    fn parse_all_requests() {
        let cases: Vec<(&str, Request)> = vec![
            (r#"{"cmd":"status"}"#, Request::Status),
            (
                r#"{"cmd":"incidents"}"#,
                Request::Incidents {
                    limit: 10,
                    offset: 0,
                },
            ),
            (
                r#"{"cmd":"incidents","limit":3,"offset":2}"#,
                Request::Incidents {
                    limit: 3,
                    offset: 2,
                },
            ),
            (r#"{"cmd":"incident","id":7}"#, Request::Incident { id: 7 }),
            (r#"{"cmd":"subscribe"}"#, Request::Subscribe),
            (
                r#"{"cmd":"pause","secs":600}"#,
                Request::Pause { secs: 600 },
            ),
            (r#"{"cmd":"resume"}"#, Request::Resume),
            (
                r#"{"cmd":"allowlist_add","name":"obs"}"#,
                Request::AllowlistAdd { name: "obs".into() },
            ),
            (r#"{"cmd":"allowlist_list"}"#, Request::AllowlistList),
            (
                r#"{"cmd":"frozen_kill","id":2}"#,
                Request::FrozenKill { id: 2 },
            ),
            (
                r#"{"cmd":"frozen_resume","id":2}"#,
                Request::FrozenResume { id: 2 },
            ),
            (r#"{"cmd":"update_check"}"#, Request::UpdateCheck),
            (r#"{"cmd":"update"}"#, Request::Update),
            (r#"{"cmd":"update_finished"}"#, Request::UpdateFinished),
        ];
        for (line, want) in cases {
            assert_eq!(handle_line(line), Ok(want), "{line}");
        }
    }

    #[test]
    fn invalid_requests_rejected() {
        for line in [
            "",
            "garbage",
            r#"{"cmd":"nope"}"#,
            r#"{"cmd":"pause"}"#,
            r#"{"cmd":"incident","id":-1}"#,
        ] {
            let e = handle_line(line).unwrap_err();
            assert!(e.starts_with("bad request: "), "{line}: {e}");
        }
        assert!(handle_line(r#"{"cmd":"pause","secs":0}"#).is_err());
        for name in ["", "   ", "\t\n", "fire*", "*", "a*b"] {
            let line = serde_json::to_string(&Request::AllowlistAdd { name: name.into() }).unwrap();
            assert!(handle_line(&line).is_err(), "{name:?}");
        }
        assert!(handle_line(r#"{"cmd":"allowlist_add","name":"fire fox"}"#).is_ok());
        // Longer than the 15-char comm: matches through the exe basename.
        assert_eq!(
            handle_line(r#"{"cmd":"allowlist_add","name":"kwin_wayland_wrapper"}"#),
            Ok(Request::AllowlistAdd {
                name: "kwin_wayland_wrapper".into()
            })
        );
    }

    #[test]
    fn response_lines() {
        assert_eq!(ok_line(&vec![1, 2]), r#"{"ok":true,"data":[1,2]}"#);
        assert_eq!(
            err_line("no \"way\""),
            r#"{"ok":false,"error":"no \"way\""}"#
        );
        let ev = Event::Notice {
            message: Message {
                title: "t".into(),
                body: "b".into(),
            },
            frozen_id: None,
        };
        let v: serde_json::Value = serde_json::from_str(&event_line(&ev, 99)).unwrap();
        assert_eq!(v["wall_ms"], 99);
        assert_eq!(v["event"]["Notice"]["message"]["title"], "t");
    }

    #[test]
    fn request_roundtrip_wakes_daemon() {
        let h = Harness::me();
        let mut c = h.connect();
        c.write_all(b"{\"cmd\":\"status\"}\n").unwrap();
        let ToDaemon::Req(req, reply) = h.rx.recv_timeout(Duration::from_secs(5)).unwrap() else {
            panic!("expected Req")
        };
        assert_eq!(req, Request::Status);
        assert_eq!(h.wake_count(), 1);
        reply.send(ok_line(&"hi")).unwrap();
        assert_eq!(read_line(&c), "{\"ok\":true,\"data\":\"hi\"}\n");
    }

    #[test]
    fn garbage_line_gets_error() {
        let h = Harness::me();
        let mut c = h.connect();
        c.write_all(b"this is not json\n").unwrap();
        let mut r = BufReader::new(c.try_clone().unwrap());
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().starts_with("bad request: "));
        // Connection stays open: a valid request still goes through.
        c.write_all(b"{\"cmd\":\"resume\"}\n").unwrap();
        let ToDaemon::Req(req, reply) = h.rx.recv_timeout(Duration::from_secs(5)).unwrap() else {
            panic!("expected Req")
        };
        assert_eq!(req, Request::Resume);
        reply.send(ok_line(&())).unwrap();
        line.clear();
        r.read_line(&mut line).unwrap();
        assert_eq!(line, "{\"ok\":true,\"data\":null}\n");
    }

    #[test]
    fn oversized_line_closes_connection() {
        let h = Harness::me();
        let mut c = h.connect();
        let mut big = vec![b'a'; MAX_LINE + 4000];
        big.push(b'\n');
        c.write_all(&big).unwrap();
        let mut r = BufReader::new(c.try_clone().unwrap());
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("too long"));
        let mut rest = Vec::new();
        assert_eq!(
            r.read_to_end(&mut rest).unwrap(),
            0,
            "connection should be closed"
        );
        assert!(h.rx.try_recv().is_err());
    }

    #[test]
    fn wrong_uid_rejected() {
        let h = Harness::new([u32::MAX - 1, u32::MAX - 2]);
        let c = h.connect();
        let line = read_line(&c);
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("not allowed"));
        let mut rest = Vec::new();
        assert_eq!((&c).read_to_end(&mut rest).unwrap(), 0);
        assert!(h.rx.try_recv().is_err());
    }

    #[test]
    fn subscribe_acks_then_streams() {
        let h = Harness::me();
        let c = h.connect();
        (&c).write_all(b"{\"cmd\":\"subscribe\"}\n").unwrap();
        let mut r = BufReader::new(c.try_clone().unwrap());
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        assert!(line.starts_with("{\"ok\":true"), "{line}");
        let ToDaemon::Subscribe(sub) = h.rx.recv_timeout(Duration::from_secs(5)).unwrap() else {
            panic!("expected Subscribe")
        };
        assert_eq!(h.wake_count(), 1);
        sub.send("{\"event\":1,\"wall_ms\":2}".into()).unwrap();
        line.clear();
        r.read_line(&mut line).unwrap();
        assert_eq!(line, "{\"event\":1,\"wall_ms\":2}\n");
        // Client goes away: the daemon sees Disconnected and drops it.
        drop(r);
        drop(c);
        let mut subs = Subscribers::new();
        subs.add(sub);
        for _ in 0..200 {
            subs.broadcast("x");
            if subs.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(subs.is_empty());
    }

    #[test]
    fn ninth_connection_refused() {
        let h = Harness::me();
        let held: Vec<UnixStream> = (0..MAX_CONNS).map(|_| h.connect()).collect();
        // Make sure all eight are registered: round trip on each.
        for c in &held {
            (&*c).write_all(b"garbage\n").unwrap();
            assert!(read_line(c).contains("bad request"));
        }
        let extra = h.connect();
        let v: serde_json::Value = serde_json::from_str(&read_line(&extra)).unwrap();
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("busy"));
        drop(extra);
        // Dropping one held connection frees its slot.
        let mut held = held;
        drop(held.pop());
        let mut accepted = false;
        for _ in 0..100 {
            let c = h.connect();
            (&c).write_all(b"garbage\n").unwrap();
            if read_line(&c).contains("bad request") {
                accepted = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(accepted, "slot was not released");
        drop(held);
    }

    #[test]
    fn dead_subscriber_frees_slot() {
        let h = Harness::me();
        let mut subs = Vec::new();
        for _ in 0..MAX_CONNS {
            let c = h.connect();
            (&c).write_all(b"{\"cmd\":\"subscribe\"}\n").unwrap();
            assert!(read_line(&c).starts_with("{\"ok\":true"));
            subs.push(c);
        }
        // Keep the daemon-side senders alive and silent.
        let _senders: Vec<_> = (0..MAX_CONNS)
            .map(|_| h.rx.recv_timeout(Duration::from_secs(5)).unwrap())
            .collect();
        drop(subs);
        std::thread::sleep(PROBE_INTERVAL * 4);
        let c = h.connect();
        (&c).write_all(b"{\"cmd\":\"status\"}\n").unwrap();
        let ToDaemon::Req(_, reply) = h.rx.recv_timeout(Duration::from_secs(5)).unwrap() else {
            panic!("expected Req")
        };
        reply.send(ok_line(&())).unwrap();
        assert_eq!(read_line(&c), "{\"ok\":true,\"data\":null}\n");
    }

    #[test]
    fn slow_subscriber_does_not_block() {
        let (tx, _rx_never_read) = mpsc::sync_channel::<String>(SUB_QUEUE);
        let mut subs = Subscribers::new();
        subs.add(tx);
        for i in 0..200 {
            subs.broadcast(&format!("event {i}"));
        }
        assert!(subs.is_empty(), "stuck subscriber must be dropped");

        let (tx, rx) = mpsc::sync_channel::<String>(SUB_QUEUE);
        let mut subs = Subscribers::new();
        subs.add(tx);
        subs.broadcast("a");
        assert_eq!(subs.len(), 1);
        drop(rx);
        subs.broadcast("b");
        assert!(subs.is_empty(), "disconnected subscriber must be dropped");

        // A subscriber that keeps up survives, even past the queue size.
        let (tx, rx) = mpsc::sync_channel::<String>(SUB_QUEUE);
        let mut subs = Subscribers::new();
        subs.add(tx);
        for i in 0..200 {
            subs.broadcast(&format!("event {i}"));
            rx.recv().unwrap();
        }
        assert_eq!(subs.len(), 1);
    }
}
