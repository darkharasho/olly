//! Periodic sampling: system pressure plus per-app memory with lineage grouping.

use crate::procfs::{
    list_pids, parse_meminfo, parse_psi, parse_zram_used, read_proc, read_u64_file, MemInfo, Proc,
    Psi,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

const SMALL_PROC: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SystemSample {
    pub t_ms: u64,
    pub psi: Psi,
    pub mem: MemInfo,
    pub zram_used: u64,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct AppKey {
    pub cgroup: String,
    /// None => the whole cgroup.
    pub root_pid: Option<i32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum TargetKind {
    Cgroup,
    /// (pid, start_time)
    Processes { pids: Vec<(i32, u64)> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppSample {
    pub key: AppKey,
    pub name: String,
    pub exe: Option<PathBuf>,
    pub kind: TargetKind,
    /// RAM + swap.
    pub bytes: u64,
    /// comm of every member, for allowlist matching.
    pub names: Vec<String>,
}

pub struct Sampler {
    root: PathBuf,
    uid: u32,
}

fn rel(root: &Path, p: &str) -> PathBuf {
    root.join(p.trim_start_matches('/'))
}

fn mem_of(p: &Proc) -> u64 {
    p.status.rss + p.status.swap
}

/// Walk up in-cgroup parents while the child belongs to the parent's group.
fn group_root(procs: &HashMap<i32, &Proc>, pid: i32) -> i32 {
    let mut cur = pid;
    for _ in 0..procs.len() {
        let p = procs[&cur];
        let Some(parent) = procs.get(&p.status.ppid).filter(|q| q.pid != cur) else {
            return cur;
        };
        let same_exe = p.exe.is_some() && p.exe == parent.exe;
        if same_exe || mem_of(p) < SMALL_PROC {
            cur = parent.pid;
        } else {
            return cur;
        }
    }
    cur
}

impl Sampler {
    pub fn new(root: PathBuf, uid: u32) -> Self {
        Self { root, uid }
    }

    pub fn system(&self, t_ms: u64) -> Option<SystemSample> {
        let psi = parse_psi(&fs::read_to_string(self.root.join("proc/pressure/memory")).ok()?)?;
        let mem = parse_meminfo(&fs::read_to_string(self.root.join("proc/meminfo")).ok()?)?;
        let zram_used = fs::read_to_string(self.root.join("sys/block/zram0/mm_stat"))
            .map(|s| parse_zram_used(&s))
            .unwrap_or(0);
        Some(SystemSample { t_ms, psi, mem, zram_used })
    }

    fn user_service(&self) -> String {
        format!("/user.slice/user-{0}.slice/user@{0}.service", self.uid)
    }

    /// All desktop-user processes, grouped by lineage; sorted by key.
    pub fn apps(&self) -> Vec<AppSample> {
        let prefix = format!("{}/", self.user_service());
        let mut by_cgroup: HashMap<String, Vec<Proc>> = HashMap::new();
        for pid in list_pids(&self.root) {
            if let Some(p) = read_proc(&self.root, pid) {
                if p.uid == self.uid && p.cgroup.starts_with(&prefix) {
                    by_cgroup.entry(p.cgroup.clone()).or_default().push(p);
                }
            }
        }

        let mut out = Vec::new();
        for (cgroup, procs) in &by_cgroup {
            let index: HashMap<i32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
            let mut groups: HashMap<i32, Vec<&Proc>> = HashMap::new();
            for p in procs {
                groups.entry(group_root(&index, p.pid)).or_default().push(p);
            }
            let single = groups.len() == 1;
            for (root_pid, mut members) in groups {
                members.sort_by_key(|p| p.pid);
                let root = index[&root_pid];
                let (key, kind, bytes) = if single {
                    let dir = rel(&self.root, &format!("sys/fs/cgroup{cgroup}"));
                    let bytes = read_u64_file(&dir.join("memory.current"))
                        + read_u64_file(&dir.join("memory.swap.current"));
                    (AppKey { cgroup: cgroup.clone(), root_pid: None }, TargetKind::Cgroup, bytes)
                } else {
                    (
                        AppKey { cgroup: cgroup.clone(), root_pid: Some(root_pid) },
                        TargetKind::Processes {
                            pids: members.iter().map(|p| (p.pid, p.start_time)).collect(),
                        },
                        members.iter().map(|p| mem_of(p)).sum(),
                    )
                };
                out.push(AppSample {
                    key,
                    name: root.status.name.clone(),
                    exe: root.exe.clone(),
                    kind,
                    bytes,
                    names: members.iter().map(|p| p.status.name.clone()).collect(),
                });
            }
        }
        out.sort_by(|a, b| {
            (&a.key.cgroup, a.key.root_pid).cmp(&(&b.key.cgroup, b.key.root_pid))
        });
        out
    }

    /// memory.current + memory.swap.current for every populated cgroup under
    /// app.slice and session.slice. Keyed by cgroup path.
    pub fn cgroup_totals(&self) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        for slice in ["app.slice", "session.slice"] {
            let path = format!("{}/{slice}", self.user_service());
            self.walk(&path, &mut out);
        }
        out.sort();
        out
    }

    fn walk(&self, cgroup: &str, out: &mut Vec<(String, u64)>) {
        let dir = rel(&self.root, &format!("sys/fs/cgroup{cgroup}"));
        let populated = fs::read_to_string(dir.join("cgroup.procs"))
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        if populated {
            let bytes = read_u64_file(&dir.join("memory.current"))
                + read_u64_file(&dir.join("memory.swap.current"));
            out.push((cgroup.to_string(), bytes));
        }
        let Ok(rd) = fs::read_dir(&dir) else { return };
        for e in rd.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                if let Some(n) = e.file_name().to_str() {
                    self.walk(&format!("{cgroup}/{n}"), out);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn sampler() -> Sampler {
        Sampler::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/steam"), 1000)
    }

    #[test]
    fn firefox_is_one_cgroup_target() {
        let apps = sampler().apps();
        let ff: Vec<_> = apps.iter().filter(|a| a.key.cgroup.contains("firefox")).collect();
        assert_eq!(ff.len(), 1);
        assert_eq!(ff[0].kind, TargetKind::Cgroup);
        assert_eq!(ff[0].key.root_pid, None);
        assert_eq!(ff[0].bytes, 2_000_000_100);
        assert_eq!(ff[0].name, "firefox");
        assert_eq!(ff[0].names.len(), 3);
    }

    #[test]
    fn steam_game_split_from_client() {
        let apps = sampler().apps();
        let st: Vec<_> = apps.iter().filter(|a| a.key.cgroup.contains("steam")).collect();
        assert_eq!(st.len(), 2);
        let client = st.iter().find(|a| a.key.root_pid == Some(100)).unwrap();
        let game = st.iter().find(|a| a.key.root_pid == Some(102)).unwrap();
        assert_eq!(
            game.kind,
            TargetKind::Processes { pids: vec![(102, 1102), (103, 1103)] }
        );
        assert_eq!(game.bytes, 9 * GIB);
        assert_eq!(game.name, "game.exe");
        assert_eq!(
            client.kind,
            TargetKind::Processes { pids: vec![(100, 1100), (101, 1101)] }
        );
        assert_eq!(client.bytes, (400 + 2) << 20);
        assert_eq!(client.names, vec!["steam", "reaper"]);
    }

    #[test]
    fn other_users_and_system_ignored() {
        let apps = sampler().apps();
        assert_eq!(apps.len(), 3);
        assert!(apps.iter().all(|a| !a.key.cgroup.contains("system.slice")));
        assert!(sampler_for(1001).apps().is_empty());
    }

    fn sampler_for(uid: u32) -> Sampler {
        Sampler::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/steam"), uid)
    }

    #[test]
    fn system_sample_reads_fixtures() {
        let s = sampler().system(42).unwrap();
        assert_eq!(s.t_ms, 42);
        assert_eq!(s.psi.some_avg10, 0.0);
        assert_eq!(s.mem.total, 31953848 * 1024);
        assert_eq!(s.mem.available, 15227060 * 1024);
        assert_eq!(s.zram_used, 976678912);
        let empty = Sampler::new(PathBuf::from("/nonexistent/olly"), 1000);
        assert!(empty.system(0).is_none());
    }

    #[test]
    fn cgroup_totals_lists_populated_cgroups() {
        let t = sampler().cgroup_totals();
        let base = "/user.slice/user-1000.slice/user@1000.service/app.slice";
        assert_eq!(
            t,
            vec![
                (format!("{base}/app-org.mozilla.firefox-1.scope"), 2_000_000_100),
                (format!("{base}/app-steam@autostart.service"), 10_000_000_500),
            ]
        );
    }
}
