//! Applies engine actions to the system.

use crate::engine::{Action, Target};
use crate::procfs::parse_start_time;
use crate::sample::TargetKind;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub trait Actuator {
    fn apply(&mut self, a: &Action) -> Result<(), String>;
}

/// Records actions without touching the system.
#[derive(Default)]
pub struct DryRunActuator {
    pub log: Vec<Action>,
}

impl Actuator for DryRunActuator {
    fn apply(&mut self, a: &Action) -> Result<(), String> {
        self.log.push(a.clone());
        Ok(())
    }
}

#[derive(Default, Serialize, Deserialize, PartialEq, Debug)]
pub struct ActiveState {
    pub frozen: Vec<Target>,
    /// (target, previous memory.high)
    pub throttled: Vec<(Target, String)>,
}

pub struct SysActuator {
    cgroup_root: PathBuf,
    proc_root: PathBuf,
    active_path: PathBuf,
    pub active: ActiveState,
}

fn cg_file(root: &Path, t: &Target, file: &str) -> PathBuf {
    // key.cgroup starts with '/', which would make join() replace the root.
    root.join(t.key.cgroup.trim_start_matches('/')).join(file)
}

fn cg_exists(root: &Path, t: &Target) -> bool {
    cg_file(root, t, "").is_dir()
}

fn write_state(path: &Path, state: &ActiveState) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut tmp = path.to_path_buf().into_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let json = serde_json::to_string(state).map_err(std::io::Error::other)?;
    fs::write(&tmp, json)?;
    fs::rename(&tmp, path)
}

fn write_cg(root: &Path, t: &Target, file: &str, val: &str) -> Result<(), String> {
    let p = cg_file(root, t, file);
    fs::write(&p, val).map_err(|e| format!("write {}: {e}", p.display()))
}

/// Signals each pid whose start time still matches; returns how many were signalled.
fn signal_pids(proc_root: &Path, pids: &[(i32, u64)], sig: i32) -> usize {
    let mut n = 0;
    for &(pid, start) in pids {
        if pid <= 0 {
            continue;
        }
        let stat = proc_root.join("proc").join(pid.to_string()).join("stat");
        let Ok(txt) = fs::read_to_string(stat) else { continue };
        if parse_start_time(&txt) != Some(start) {
            continue;
        }
        // SAFETY: plain kill(2) on a pid whose identity was just verified.
        if unsafe { libc::kill(pid, sig) } == 0 {
            n += 1;
        }
    }
    n
}

fn signal_target(proc_root: &Path, pids: &[(i32, u64)], sig: i32) -> Result<(), String> {
    if signal_pids(proc_root, pids, sig) > 0 {
        Ok(())
    } else {
        Err("target gone".to_string())
    }
}

impl SysActuator {
    pub fn new(cgroup_root: PathBuf, proc_root: PathBuf, active_path: PathBuf) -> Self {
        Self { cgroup_root, proc_root, active_path, active: ActiveState::default() }
    }

    fn persist(&self) {
        if let Err(e) = self.try_persist() {
            eprintln!("olly: cannot persist {}: {e}", self.active_path.display());
        }
    }

    fn try_persist(&self) -> std::io::Result<()> {
        write_state(&self.active_path, &self.active)
    }

    fn mark_frozen(&mut self, t: &Target) {
        self.active.frozen.retain(|f| f.key != t.key);
        self.active.frozen.push(t.clone());
    }

    fn forget_frozen(&mut self, t: &Target) {
        self.active.frozen.retain(|f| f.key != t.key);
    }

    fn forget_throttled(&mut self, t: &Target) {
        self.active.throttled.retain(|(f, _)| f.key != t.key);
    }

    fn throttle(&mut self, t: &Target, high: u64) -> Result<(), String> {
        if !matches!(t.kind, TargetKind::Cgroup) {
            return Err("cannot throttle processes".to_string());
        }
        let existing = self.active.throttled.iter().position(|(f, _)| f.key == t.key);
        let prev = match existing {
            Some(i) => self.active.throttled[i].1.clone(),
            None => {
                let p = cg_file(&self.cgroup_root, t, "memory.high");
                fs::read_to_string(&p)
                    .map_err(|e| format!("read {}: {e}", p.display()))?
                    .trim()
                    .to_string()
            }
        };
        // Record before acting so a crash mid-way still leaves a recovery entry.
        match existing {
            Some(i) => self.active.throttled[i].0 = t.clone(),
            None => self.active.throttled.push((t.clone(), prev)),
        }
        self.persist();
        let r = write_cg(&self.cgroup_root, t, "memory.high", &high.to_string());
        if r.is_err() && existing.is_none() {
            self.forget_throttled(t);
            self.persist();
        }
        r
    }

    fn unthrottle(&mut self, t: &Target) -> Result<(), String> {
        let Some(i) = self.active.throttled.iter().position(|(f, _)| f.key == t.key) else {
            return Ok(());
        };
        let (stored, prev) = self.active.throttled[i].clone();
        // Write first: the entry is the only record that the cap is still in place.
        let r = write_cg(&self.cgroup_root, &stored, "memory.high", &prev);
        if r.is_ok() || !cg_exists(&self.cgroup_root, &stored) {
            self.forget_throttled(t);
            self.persist();
        }
        r
    }

    fn freeze(&mut self, t: &Target) -> Result<(), String> {
        let had = self.active.frozen.iter().any(|f| f.key == t.key);
        self.mark_frozen(t);
        self.persist();
        let r = match &t.kind {
            TargetKind::Cgroup => write_cg(&self.cgroup_root, t, "cgroup.freeze", "1"),
            TargetKind::Processes { pids } => signal_target(&self.proc_root, pids, libc::SIGSTOP),
        };
        if r.is_err() && !had {
            self.forget_frozen(t);
            self.persist();
        }
        r
    }

    /// Cgroup targets stay tracked after a failed write unless the cgroup is gone;
    /// process targets are gone whenever no pid matched.
    fn settled(&self, t: &Target, r: &Result<(), String>) -> bool {
        match t.kind {
            TargetKind::Cgroup => r.is_ok() || !cg_exists(&self.cgroup_root, t),
            TargetKind::Processes { .. } => true,
        }
    }

    fn resume(&mut self, t: &Target) -> Result<(), String> {
        let r = match &t.kind {
            TargetKind::Cgroup => write_cg(&self.cgroup_root, t, "cgroup.freeze", "0"),
            TargetKind::Processes { pids } => signal_target(&self.proc_root, pids, libc::SIGCONT),
        };
        if self.settled(t, &r) {
            self.forget_frozen(t);
            self.persist();
        }
        r
    }

    fn kill(&mut self, t: &Target) -> Result<(), String> {
        let r = match &t.kind {
            TargetKind::Cgroup => write_cg(&self.cgroup_root, t, "cgroup.kill", "1"),
            TargetKind::Processes { pids } => signal_target(&self.proc_root, pids, libc::SIGKILL),
        };
        if self.settled(t, &r) {
            self.forget_frozen(t);
            self.forget_throttled(t);
            self.persist();
        }
        r
    }

    /// Writes every stored previous `memory.high` back and clears the throttles.
    pub fn restore_throttles(&mut self) -> Vec<String> {
        let mut log = Vec::new();
        let mut kept = Vec::new();
        for (t, prev) in std::mem::take(&mut self.active.throttled) {
            match write_cg(&self.cgroup_root, &t, "memory.high", &prev) {
                Ok(()) => log.push(format!("restored memory.high={prev} on {}", t.key.cgroup)),
                Err(e) => {
                    log.push(format!("could not restore {}: {e}", t.key.cgroup));
                    if cg_exists(&self.cgroup_root, &t) {
                        kept.push((t, prev));
                    }
                }
            }
        }
        self.active.throttled = kept;
        self.persist();
        log
    }
}

impl Actuator for SysActuator {
    fn apply(&mut self, a: &Action) -> Result<(), String> {
        match a {
            Action::Throttle { target, high } => self.throttle(target, *high),
            Action::Unthrottle { target } => self.unthrottle(target),
            Action::Freeze { target } => self.freeze(target),
            Action::Resume { target } => self.resume(target),
            Action::Kill { target, .. } => self.kill(target),
        }
    }
}

/// Undoes whatever a previous (crashed) run left in `active_path`. Errors are logged in the
/// returned lines, never raised. Entries that failed and still exist are written back to the
/// file; it is deleted only when nothing is left.
pub fn recover(cgroup_root: &Path, proc_root: &Path, active_path: &Path) -> Vec<String> {
    let Ok(txt) = fs::read_to_string(active_path) else { return Vec::new() };
    let Ok(state) = serde_json::from_str::<ActiveState>(&txt) else { return Vec::new() };
    let mut log = Vec::new();
    let mut left = ActiveState::default();
    for t in state.frozen {
        let (r, gone) = match &t.kind {
            TargetKind::Cgroup => (
                write_cg(cgroup_root, &t, "cgroup.freeze", "0"),
                !cg_exists(cgroup_root, &t),
            ),
            TargetKind::Processes { pids } => (signal_target(proc_root, pids, libc::SIGCONT), true),
        };
        match r {
            Ok(()) => log.push(format!("resumed {}", t.name)),
            Err(e) => {
                log.push(format!("could not resume {}: {e}", t.name));
                if !gone {
                    left.frozen.push(t);
                }
            }
        }
    }
    for (t, prev) in state.throttled {
        match write_cg(cgroup_root, &t, "memory.high", &prev) {
            Ok(()) => log.push(format!("restored memory.high={prev} on {}", t.key.cgroup)),
            Err(e) => {
                log.push(format!("could not restore {}: {e}", t.key.cgroup));
                if cg_exists(cgroup_root, &t) {
                    left.throttled.push((t, prev));
                }
            }
        }
    }
    if left == ActiveState::default() {
        let _ = fs::remove_file(active_path);
    } else if let Err(e) = write_state(active_path, &left) {
        log.push(format!("could not rewrite {}: {e}", active_path.display()));
    }
    log
}

/// Used on SIGTERM: resume everything frozen and restore every throttle.
pub fn undo_all(act: &mut SysActuator) {
    for t in act.active.frozen.clone() {
        if let Err(e) = act.resume(&t) {
            eprintln!("olly: could not resume {}: {e}", t.name);
        }
    }
    for line in act.restore_throttles() {
        eprintln!("olly: {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::AppKey;
    use std::process::{Child, Command};

    struct Guard(Child);
    impl std::ops::Deref for Guard {
        type Target = Child;
        fn deref(&self) -> &Child {
            &self.0
        }
    }
    impl std::ops::DerefMut for Guard {
        fn deref_mut(&mut self) -> &mut Child {
            &mut self.0
        }
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn spawn_sleep() -> Guard {
        Guard(Command::new("sleep").arg("30").spawn().unwrap())
    }

    fn cg_target(cg: &str) -> Target {
        Target {
            id: 1,
            key: AppKey { cgroup: cg.into(), root_pid: None },
            name: "x".into(),
            kind: TargetKind::Cgroup,
            bytes: 1,
        }
    }

    fn setup(cg: &str) -> (tempfile::TempDir, SysActuator, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("cg").join(cg.trim_start_matches('/'));
        fs::create_dir_all(&dir).unwrap();
        let act = SysActuator::new(d.path().join("cg"), "/".into(), d.path().join("state/active.json"));
        (d, act, dir)
    }

    fn start_time(c: &Child) -> u64 {
        parse_start_time(&fs::read_to_string(format!("/proc/{}/stat", c.id())).unwrap()).unwrap()
    }

    fn proc_target(c: &Child, st: u64) -> Target {
        Target {
            id: 2,
            key: AppKey { cgroup: "/p".into(), root_pid: Some(c.id() as i32) },
            name: "sleep".into(),
            kind: TargetKind::Processes { pids: vec![(c.id() as i32, st)] },
            bytes: 1,
        }
    }

    #[test]
    fn dry_run_actuator_records() {
        let target = cg_target("/x");
        let a = Action::Freeze { target };
        let mut d = DryRunActuator::default();
        assert_eq!(d.apply(&a), Ok(()));
        assert_eq!(d.log, vec![a]);
    }

    #[test]
    fn cgroup_freeze_writes_file() {
        let (_d, mut act, dir) = setup("/a.slice/b.service");
        let t = cg_target("/a.slice/b.service");
        act.apply(&Action::Freeze { target: t.clone() }).unwrap();
        assert_eq!(fs::read_to_string(dir.join("cgroup.freeze")).unwrap(), "1");
        act.apply(&Action::Resume { target: t }).unwrap();
        assert_eq!(fs::read_to_string(dir.join("cgroup.freeze")).unwrap(), "0");
    }

    #[test]
    fn throttle_restores_previous_high() {
        let (_d, mut act, dir) = setup("/a");
        fs::write(dir.join("memory.high"), "max\n").unwrap();
        let t = cg_target("/a");
        act.apply(&Action::Throttle { target: t.clone(), high: 1000 }).unwrap();
        assert_eq!(fs::read_to_string(dir.join("memory.high")).unwrap(), "1000");
        // re-throttle keeps the original previous value
        act.apply(&Action::Throttle { target: t.clone(), high: 500 }).unwrap();
        assert_eq!(fs::read_to_string(dir.join("memory.high")).unwrap(), "500");
        assert_eq!(act.active.throttled[0].1, "max");
        act.apply(&Action::Unthrottle { target: t.clone() }).unwrap();
        assert_eq!(fs::read_to_string(dir.join("memory.high")).unwrap(), "max");
        assert!(act.active.throttled.is_empty());
        assert_eq!(act.apply(&Action::Unthrottle { target: t }), Ok(()));
    }

    #[test]
    fn cgroup_kill_writes_and_clears_state() {
        let (_d, mut act, dir) = setup("/a");
        let t = cg_target("/a");
        act.apply(&Action::Freeze { target: t.clone() }).unwrap();
        act.apply(&Action::Kill { target: t, reason: crate::engine::KillReason::User }).unwrap();
        assert_eq!(fs::read_to_string(dir.join("cgroup.kill")).unwrap(), "1");
        assert_eq!(act.active, ActiveState::default());
    }

    #[test]
    fn signal_skips_reused_pid() {
        let (_d, mut act, _dir) = setup("/a");
        let mut c = spawn_sleep();
        let st = start_time(&c);
        let bad = proc_target(&c, st + 1);
        let kill = |t: Target| Action::Kill { target: t, reason: crate::engine::KillReason::User };
        assert_eq!(act.apply(&kill(bad)), Err("target gone".to_string()));
        assert!(c.try_wait().unwrap().is_none());
        act.apply(&kill(proc_target(&c, st))).unwrap();
        let status = c.wait().unwrap();
        assert!(!status.success());
    }

    #[test]
    fn processes_throttle_is_error() {
        let (_d, mut act, _dir) = setup("/a");
        let c = spawn_sleep();
        let t = proc_target(&c, start_time(&c));
        assert_eq!(
            act.apply(&Action::Throttle { target: t, high: 1 }),
            Err("cannot throttle processes".to_string())
        );
    }

    #[test]
    fn stop_and_cont_processes() {
        let (_d, mut act, _dir) = setup("/a");
        let c = spawn_sleep();
        let t = proc_target(&c, start_time(&c));
        let stat = || fs::read_to_string(format!("/proc/{}/stat", c.id())).unwrap();
        let state = |s: &str| s[s.rfind(')').unwrap() + 2..].chars().next().unwrap();
        act.apply(&Action::Freeze { target: t.clone() }).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(state(&stat()), 'T');
        act.apply(&Action::Resume { target: t }).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_ne!(state(&stat()), 'T');
        assert!(act.active.frozen.is_empty());
    }

    #[test]
    fn active_state_roundtrip() {
        let (d, mut act, dir) = setup("/a");
        fs::write(dir.join("memory.high"), "max").unwrap();
        let t = cg_target("/a");
        act.apply(&Action::Freeze { target: t.clone() }).unwrap();
        act.apply(&Action::Throttle { target: t.clone(), high: 9 }).unwrap();
        let path = d.path().join("state/active.json");
        let st: ActiveState = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(st.frozen.len(), 1);
        assert_eq!(st.throttled.len(), 1);
        assert!(!path.with_extension("json.tmp").exists());
        act.apply(&Action::Resume { target: t }).unwrap();
        let st: ActiveState = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(st.frozen.is_empty());
        assert_eq!(st.throttled.len(), 1);
    }

    #[test]
    fn recover_undoes_leftovers() {
        let (d, _act, dir) = setup("/a");
        fs::write(dir.join("cgroup.freeze"), "1").unwrap();
        fs::write(dir.join("memory.high"), "1000").unwrap();
        let t = cg_target("/a");
        let st = ActiveState { frozen: vec![t.clone()], throttled: vec![(t, "max".into())] };
        let path = d.path().join("active.json");
        fs::write(&path, serde_json::to_string(&st).unwrap()).unwrap();
        let lines = recover(&d.path().join("cg"), Path::new("/"), &path);
        assert!(!lines.is_empty());
        assert_eq!(fs::read_to_string(dir.join("cgroup.freeze")).unwrap(), "0");
        assert_eq!(fs::read_to_string(dir.join("memory.high")).unwrap(), "max");
        assert!(!path.exists());
    }

    #[test]
    fn recover_tolerates_missing_or_garbage_file() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("active.json");
        assert!(recover(d.path(), Path::new("/"), &p).is_empty());
        fs::write(&p, "not json").unwrap();
        assert!(recover(d.path(), Path::new("/"), &p).is_empty());
    }

    #[test]
    fn recover_sigconts_processes_and_undo_all() {
        let d = tempfile::tempdir().unwrap();
        let c = spawn_sleep();
        let t = proc_target(&c, start_time(&c));
        unsafe { libc::kill(c.id() as i32, libc::SIGSTOP) };
        let path = d.path().join("active.json");
        let st = ActiveState { frozen: vec![t], throttled: vec![] };
        fs::write(&path, serde_json::to_string(&st).unwrap()).unwrap();
        recover(d.path(), Path::new("/"), &path);
        std::thread::sleep(std::time::Duration::from_millis(100));
        let s = fs::read_to_string(format!("/proc/{}/stat", c.id())).unwrap();
        assert_ne!(s[s.rfind(')').unwrap() + 2..].chars().next().unwrap(), 'T');
    }

    #[test]
    fn undo_all_resumes_and_restores() {
        let (_d, mut act, dir) = setup("/a");
        fs::write(dir.join("memory.high"), "max").unwrap();
        let t = cg_target("/a");
        act.apply(&Action::Freeze { target: t.clone() }).unwrap();
        act.apply(&Action::Throttle { target: t, high: 5 }).unwrap();
        undo_all(&mut act);
        assert_eq!(fs::read_to_string(dir.join("cgroup.freeze")).unwrap(), "0");
        assert_eq!(fs::read_to_string(dir.join("memory.high")).unwrap(), "max");
        assert_eq!(act.active, ActiveState::default());
    }

    #[test]
    fn failed_cgroup_resume_keeps_tracking() {
        let (_d, mut act, dir) = setup("/a");
        let t = cg_target("/a");
        act.apply(&Action::Freeze { target: t.clone() }).unwrap();
        fs::remove_file(dir.join("cgroup.freeze")).unwrap();
        fs::create_dir(dir.join("cgroup.freeze")).unwrap();
        assert!(act.apply(&Action::Resume { target: t.clone() }).is_err());
        assert_eq!(act.active.frozen.len(), 1);
        // kill with a missing cgroup.kill file while the cgroup exists also keeps it
        fs::create_dir(dir.join("cgroup.kill")).unwrap();
        let k = Action::Kill { target: t, reason: crate::engine::KillReason::User };
        assert!(act.apply(&k).is_err());
        assert_eq!(act.active.frozen.len(), 1);
    }

    #[test]
    fn vanished_cgroup_resume_forgets() {
        let (_d, mut act, dir) = setup("/a");
        let t = cg_target("/a");
        act.apply(&Action::Freeze { target: t.clone() }).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert!(act.apply(&Action::Resume { target: t.clone() }).is_err());
        assert!(act.active.frozen.is_empty());
        fs::create_dir_all(&dir).unwrap();
        act.apply(&Action::Freeze { target: t.clone() }).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        let k = Action::Kill { target: t, reason: crate::engine::KillReason::User };
        assert!(act.apply(&k).is_err());
        assert_eq!(act.active, ActiveState::default());
    }

    #[test]
    fn failed_unthrottle_keeps_entry() {
        let (_d, mut act, dir) = setup("/a");
        fs::write(dir.join("memory.high"), "max").unwrap();
        let t = cg_target("/a");
        act.apply(&Action::Throttle { target: t.clone(), high: 7 }).unwrap();
        fs::remove_file(dir.join("memory.high")).unwrap();
        fs::create_dir(dir.join("memory.high")).unwrap();
        assert!(act.apply(&Action::Unthrottle { target: t.clone() }).is_err());
        assert_eq!(act.active.throttled.len(), 1);
        assert!(!act.restore_throttles().is_empty());
        assert_eq!(act.active.throttled.len(), 1);
        fs::remove_dir_all(&dir).unwrap();
        assert!(act.apply(&Action::Unthrottle { target: t }).is_err());
        assert!(act.active.throttled.is_empty());
    }

    #[test]
    fn recover_keeps_failed_entries() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("cg");
        let bad = root.join("bad");
        let ok = root.join("ok");
        fs::create_dir_all(bad.join("cgroup.freeze")).unwrap();
        fs::create_dir_all(&ok).unwrap();
        let (tb, to, tgone) = (cg_target("/bad"), cg_target("/ok"), cg_target("/gone"));
        let st = ActiveState {
            frozen: vec![tb.clone(), to.clone(), tgone.clone()],
            throttled: vec![(to, "max".into()), (tgone, "max".into())],
        };
        let path = d.path().join("active.json");
        fs::write(&path, serde_json::to_string(&st).unwrap()).unwrap();
        recover(&root, Path::new("/"), &path);
        let left: ActiveState = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(left, ActiveState { frozen: vec![tb], throttled: vec![] });
        assert_eq!(fs::read_to_string(ok.join("memory.high")).unwrap(), "max");
    }
}
