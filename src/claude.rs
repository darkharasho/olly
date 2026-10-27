//! Suggest-only diagnosis through the Claude CLI, run as the desktop user.
//!
//! Claude never acts: the prompt forbids it and the tool allowlist is read-only
//! (`Read`, `Grep`, `Glob`, `journalctl`, `ps`). Nothing here runs in the emergency
//! path; the daemon hands jobs over and polls for answers without waiting.

use crate::config::ClaudeConfig;
use crate::incident::Incident;
use crate::voice::fmt_bytes;
use std::io::Read;
use std::os::fd::RawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

const DAY_MS: u64 = 86_400_000;
/// More output than this and the child is killed; a runaway can't balloon the daemon.
const MAX_OUTPUT: usize = 256 * 1024;
const ALLOWED_TOOLS: &str = "Read,Grep,Glob,Bash(journalctl:*),Bash(ps:*)";
const POLL: Duration = Duration::from_millis(50);

pub struct ClaudeRunner {
    cfg: ClaudeConfig,
    #[allow(dead_code)]
    uid: u32,
    as_root: bool,
    user: String,
    home: String,
    calls_today: (u64 /*day*/, u32),
    /// An eventfd to poke when a job finishes, so the daemon loop wakes up.
    wake: Option<RawFd>,
}

#[derive(Clone, Debug)]
pub enum Job {
    Diagnose(Incident),
    Leak { cgroup: String, growth: u64 },
}

/// Name and home directory for `uid`, via `getpwuid_r`.
fn lookup_user(uid: u32) -> Result<(String, String), String> {
    let mut buf = vec![0u8; 4096];
    loop {
        let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
        let mut out: *mut libc::passwd = std::ptr::null_mut();
        let rc = unsafe { libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr() as *mut libc::c_char, buf.len(), &mut out) };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || out.is_null() {
            return Err(format!("no passwd entry for uid {uid}"));
        }
        let cstr = |p: *const libc::c_char| unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned();
        return Ok((cstr(pw.pw_name), cstr(pw.pw_dir)));
    }
}

impl ClaudeRunner {
    /// Looks up the desktop user. `as_root` says whether the daemon can `runuser`.
    pub fn new(cfg: ClaudeConfig, uid: u32, as_root: bool) -> Result<Self, String> {
        let (user, home) = lookup_user(uid)?;
        Ok(Self::with_user(cfg, uid, as_root, &user, &home))
    }

    pub fn with_user(cfg: ClaudeConfig, uid: u32, as_root: bool, user: &str, home: &str) -> Self {
        ClaudeRunner {
            cfg,
            uid,
            as_root,
            user: user.to_string(),
            home: home.to_string(),
            calls_today: (0, 0),
            wake: None,
        }
    }

    pub fn set_wake(&mut self, fd: RawFd) {
        self.wake = Some(fd);
    }

    pub fn recovered_avail(&self) -> f32 {
        self.cfg.recovered_avail
    }

    /// Enabled and under `max_per_day`. A `true` answer uses up one call for today.
    pub fn allowed(&mut self, now_wall_ms: u64) -> bool {
        let day = now_wall_ms / DAY_MS;
        if self.calls_today.0 != day {
            self.calls_today = (day, 0);
        }
        if !self.cfg.enabled || self.calls_today.1 >= self.cfg.max_per_day {
            return false;
        }
        self.calls_today.1 += 1;
        true
    }

    pub fn build_prompt(job: &Job) -> String {
        let facts = match job {
            Job::Diagnose(inc) => format!(
                "A memory incident just ended on this desktop. Here is olly's record of it as JSON:\n\n{}\n",
                serde_json::to_string_pretty(inc).unwrap_or_else(|_| inc.summary.clone())
            ),
            Job::Leak { cgroup, growth } => format!(
                "olly noticed a slow memory leak. The cgroup {cgroup} has grown by {} over the last couple of hours and has not recovered.\n",
                fmt_bytes(*growth)
            ),
        };
        format!(
            "You are helping olly, a small memory-guardian daemon on a Linux desktop.\n\n{facts}\n\
             Work out the likely cause (look at the journal and the process list if it helps), whether it is likely to recur, and what the user could do about it.\n\
             You may only suggest; do not run commands that change anything, and do not edit files or config.\n\n\
             Reply in olly's voice: plain, calm and kind, with a lowercase \"olly\". Start with a short notification line of at most 3 sentences. \
             Then leave a blank line and put the details and your suggestions after it."
        )
    }

    fn claude_path(&self) -> String {
        self.cfg.claude_path.clone().unwrap_or_else(|| format!("{}/.local/bin/claude", self.home))
    }

    /// `runuser -u <user> -- nice -n 19 ionice -c3 <claude> -p <prompt> ...`; without
    /// root there is no `runuser` and the rest runs directly.
    pub fn command(&self, prompt: &str) -> Command {
        let tail = [
            "-n".to_string(),
            "19".into(),
            "ionice".into(),
            "-c3".into(),
            self.claude_path(),
            "-p".into(),
            prompt.into(),
            "--output-format".into(),
            "json".into(),
            "--allowedTools".into(),
            ALLOWED_TOOLS.into(),
        ];
        let mut c = if self.as_root {
            let mut c = Command::new("runuser");
            c.args(["-u", &self.user, "--", "nice"]);
            c
        } else {
            Command::new("nice")
        };
        c.args(tail).env("HOME", &self.home).current_dir("/");
        c
    }

    /// Runs the job on its own thread and sends `(job, result)` when it ends. Never blocks.
    pub fn spawn(&self, job: Job, done: mpsc::Sender<(Job, Result<String, String>)>) {
        let cmd = self.command(&Self::build_prompt(&job));
        let timeout = Duration::from_secs(self.cfg.timeout_secs);
        let wake = self.wake;
        let (fallback_job, fallback_done) = (job.clone(), done.clone());
        let started = thread::Builder::new().name("olly-claude".into()).spawn(move || {
            let result = run_command(cmd, timeout);
            let _ = done.send((job, result));
            if let Some(fd) = wake {
                let one = 1u64;
                unsafe { libc::write(fd, &one as *const u64 as *const libc::c_void, 8) };
            }
        });
        if let Err(e) = started {
            let _ = fallback_done.send((fallback_job, Err(format!("cannot start the claude thread: {e}"))));
        }
    }

    #[cfg(test)]
    fn run_blocking(&self, prompt: &str) -> Result<String, String> {
        run_command(self.command(prompt), Duration::from_secs(self.cfg.timeout_secs))
    }
}

/// The daemon blocks SIGTERM/SIGINT and children inherit that. Give the child a clean
/// mask and its own process group, so a timeout can kill everything it started.
fn isolate(cmd: &mut Command) {
    unsafe {
        cmd.pre_exec(|| {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            if libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut()) != 0 || libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn kill_group(child: &mut Child) {
    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    let _ = child.kill();
    let _ = child.wait();
}

fn run_command(mut cmd: Command, timeout: Duration) -> Result<String, String> {
    isolate(&mut cmd);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| format!("cannot start claude: {e}"))?;
    let mut out = child.stdout.take().expect("stdout is piped");
    let overflow = Arc::new(AtomicBool::new(false));
    let flag = overflow.clone();
    let reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match out.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.len() > MAX_OUTPUT {
                        flag.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
        }
        buf
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if overflow.load(Ordering::SeqCst) {
            kill_group(&mut child);
            let _ = reader.join();
            return Err("claude printed too much output".to_string());
        }
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {}
            Err(e) => {
                kill_group(&mut child);
                let _ = reader.join();
                return Err(format!("cannot wait for claude: {e}"));
            }
        }
        if Instant::now() >= deadline {
            kill_group(&mut child);
            let _ = reader.join();
            return Err(format!("claude timed out after {}s", timeout.as_secs()));
        }
        thread::sleep(POLL);
    };
    // Anything the child left behind in its group goes too, which also closes the pipe.
    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    let buf = reader.join().unwrap_or_default();
    if overflow.load(Ordering::SeqCst) {
        return Err("claude printed too much output".to_string());
    }
    if !status.success() {
        return Err(format!("claude exited with {status}"));
    }
    parse_result(&buf)
}

fn parse_result(buf: &[u8]) -> Result<String, String> {
    let v: serde_json::Value = serde_json::from_slice(buf).map_err(|e| format!("claude output is not JSON: {e}"))?;
    match v.get("result").and_then(|r| r.as_str()) {
        Some(r) if !r.trim().is_empty() => Ok(r.trim().to_string()),
        _ => Err("claude output has no result".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    fn cfg() -> ClaudeConfig {
        ClaudeConfig::default()
    }

    fn incident() -> Incident {
        Incident {
            id: 42,
            start_ms_wall: 1_000,
            end_ms_wall: 15_000,
            peak_full: 30.0,
            min_avail: 2_000_000_000,
            top_at_peak: vec![],
            actions: vec![],
            summary: "Closed game.exe (21 GB). Recovered in 14s.".into(),
            diagnosis: None,
        }
    }

    #[test]
    fn prompt_mentions_suggest_only_and_incident() {
        let p = ClaudeRunner::build_prompt(&Job::Diagnose(incident()));
        assert!(p.contains("only suggest"), "{p}");
        assert!(p.contains("do not run commands that change anything"), "{p}");
        assert!(p.contains("Closed game.exe (21 GB). Recovered in 14s."), "{p}");
        assert!(p.contains("\"id\""), "{p}");
        assert!(p.contains("olly's voice"), "{p}");
        assert!(p.contains("3 sentences"), "{p}");
        let l = ClaudeRunner::build_prompt(&Job::Leak { cgroup: "/user.slice/app-foo.scope".into(), growth: 3_000_000_000 });
        assert!(l.contains("/user.slice/app-foo.scope"), "{l}");
        assert!(l.contains("3.0 GB"), "{l}");
        assert!(l.contains("only suggest"), "{l}");
    }

    #[test]
    fn command_runs_as_user_with_low_priority() {
        let r = ClaudeRunner::with_user(cfg(), 1000, true, "alice", "/home/alice");
        let c = r.command("hello");
        assert_eq!(c.get_program(), "runuser");
        let args: Vec<String> = c.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        let pos = |s: &str| args.iter().position(|a| a == s).unwrap_or_else(|| panic!("{s} in {args:?}"));
        assert_eq!(&args[..4], ["-u", "alice", "--", "nice"]);
        assert!(pos("ionice") > pos("nice"));
        assert_eq!(args[pos("-n") + 1], "19");
        assert_eq!(args[pos("-c3")], "-c3");
        assert!(pos("-p") > pos("ionice"));
        assert_eq!(args[pos("-p") + 1], "hello");
        assert_eq!(args[pos("--output-format") + 1], "json");
        assert!(args[pos("--allowedTools") + 1].contains("Bash(journalctl:*)"));
        assert!(args[pos("-p") - 1].ends_with("/.local/bin/claude"), "{args:?}");
        let home = c.get_envs().find(|(k, _)| *k == "HOME").and_then(|(_, v)| v);
        assert_eq!(home.unwrap(), "/home/alice");

        let mut custom = cfg();
        custom.claude_path = Some("/opt/claude".into());
        let r = ClaudeRunner::with_user(custom, 1000, false, "alice", "/home/alice");
        let c = r.command("x");
        assert_eq!(c.get_program(), "nice");
        let args: Vec<String> = c.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(args.contains(&"/opt/claude".to_string()), "{args:?}");
        assert!(!args.contains(&"runuser".to_string()));
    }

    #[test]
    fn rate_limit_per_day() {
        let mut r = ClaudeRunner::with_user(cfg(), 1000, false, "a", "/h");
        let day = 86_400_000u64;
        let t = 100 * day + 5;
        assert!(r.allowed(t));
        assert!(r.allowed(t + 1));
        assert!(r.allowed(t + 2));
        assert!(!r.allowed(t + 3));
        assert!(r.allowed(t + day));
        let mut off = cfg();
        off.enabled = false;
        assert!(!ClaudeRunner::with_user(off, 1000, false, "a", "/h").allowed(t));
    }

    #[test]
    fn timeout_kills_child() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("sleep.pid");
        let script = dir.path().join("fake-claude");
        std::fs::write(&script, format!("#!/bin/sh\nsleep 60 &\necho $! > {}\nwait\n", pidfile.display())).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut c = cfg();
        c.timeout_secs = 1;
        c.claude_path = Some(script.to_string_lossy().into_owned());
        let r = ClaudeRunner::with_user(c, 1000, false, "a", dir.path().to_str().unwrap());
        let start = Instant::now();
        let err = r.run_blocking("hi").unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(3), "{:?}", start.elapsed());
        let pid: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        let gone = |pid: i32| match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(s) => s.rsplit(')').next().is_some_and(|rest| rest.trim_start().starts_with('Z')),
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        while !gone(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(gone(pid), "sleep {pid} survived");
    }

    fn fake(dir: &std::path::Path, body: &str) -> ClaudeRunner {
        let script = dir.join("fake-claude");
        std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut c = cfg();
        c.claude_path = Some(script.to_string_lossy().into_owned());
        ClaudeRunner::with_user(c, 1000, false, "a", dir.to_str().unwrap())
    }

    #[test]
    fn parses_result_and_rejects_bad_output() {
        let d = tempfile::tempdir().unwrap();
        let ok = fake(d.path(), r#"echo '{"type":"result","is_error":false,"result":"It was the browser."}'"#);
        assert_eq!(ok.run_blocking("x").unwrap(), "It was the browser.");
        let d2 = tempfile::tempdir().unwrap();
        assert!(fake(d2.path(), "echo not json").run_blocking("x").is_err());
        let d3 = tempfile::tempdir().unwrap();
        assert!(fake(d3.path(), r#"echo '{"foo":1}'"#).run_blocking("x").unwrap_err().contains("result"));
        let d4 = tempfile::tempdir().unwrap();
        assert!(fake(d4.path(), "exit 3").run_blocking("x").unwrap_err().contains("exited"));
    }

    #[test]
    fn runaway_output_is_capped() {
        let d = tempfile::tempdir().unwrap();
        let r = fake(d.path(), "yes | head -c 5000000; sleep 30");
        let start = Instant::now();
        let err = r.run_blocking("x").unwrap_err();
        assert!(err.contains("too much output"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
