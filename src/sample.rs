//! Periodic sampling: system pressure plus per-app memory with lineage grouping.

use crate::procfs::{
    list_pids, parse_meminfo, parse_psi, parse_status, parse_zram_used, read_proc, read_u64_file,
    MemInfo, Proc, Psi,
};
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

const SMALL_PROC: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SystemSample {
    pub t_ms: u64,
    pub psi: Psi,
    /// Instantaneous `full` stall, in percent, since the previous sample: the `full total=`
    /// delta over the elapsed time. None on the first sample or after a counter reset.
    pub full_now: Option<f32>,
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
    Processes {
        pids: Vec<(i32, u64)>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppSample {
    pub key: AppKey,
    pub name: String,
    pub exe: Option<PathBuf>,
    pub kind: TargetKind,
    /// RAM + swap.
    pub bytes: u64,
    /// RAM only: memory.current for a cgroup, summed RSS for a process group.
    pub ram: u64,
    /// comm of every member, for allowlist matching.
    pub names: Vec<String>,
    /// exe basename of every member (comm is cut at 15 chars), for allowlist matching.
    pub exe_names: Vec<String>,
    /// A game: split from the Steam client and launched by `reaper`, run by a
    /// wine/proton loader, or running under gamescope with no Steam client in between
    /// (else launched by `reaper`). Game mode picks other apps first.
    pub is_game: bool,
}

/// Exe basenames of the wine/proton loaders a Windows game runs under.
const GAME_LOADERS: [&str; 4] = ["wine64-preloader", "wine-preloader", "wineserver", "proton"];

fn is_steam(p: &Proc) -> bool {
    p.status.name == "steam" || exe_name(p).as_deref() == Some("steam")
}

fn is_gamescope(name: &str) -> bool {
    name.starts_with("gamescope")
}

/// pid -> (ppid, comm, exe basename) for every desktop-user process.
type Lineage = HashMap<i32, (i32, String, Option<String>)>;

/// What the ancestors of a process (not the process itself) say about it being a game.
#[derive(Debug, Default, PartialEq)]
struct Ancestry {
    /// Some ancestor is `reaper`: Steam launches every game via `reaper SteamLaunch`.
    reaper: bool,
    /// Some ancestor is gamescope.
    gamescope: bool,
    /// Some ancestor is gamescope, with no Steam client between it and the process (nor
    /// the process being the client). In
    /// the gamescope session, gamescope launches Steam, so everything Steam starts is
    /// under it; that alone doesn't make a game.
    gamescope_direct: bool,
}

fn ancestry(lineage: &Lineage, pid: i32) -> Ancestry {
    let is =
        |l: &(i32, String, Option<String>), want: &str| l.1 == want || l.2.as_deref() == Some(want);
    let mut a = Ancestry::default();
    // The Steam client itself counts as Steam in between.
    let mut steam_seen = lineage.get(&pid).is_some_and(|l| is(l, "steam"));
    let mut cur = lineage.get(&pid).map(|l| l.0);
    for _ in 0..lineage.len() {
        let Some(l) = cur.filter(|&c| c != pid).and_then(|c| lineage.get(&c)) else {
            break;
        };
        a.reaper |= is(l, "reaper");
        steam_seen |= is(l, "steam");
        if is_gamescope(&l.1) || l.2.as_deref().is_some_and(is_gamescope) {
            a.gamescope = true;
            a.gamescope_direct |= !steam_seen;
        }
        cur = Some(l.0);
    }
    a
}

pub struct Sampler {
    root: PathBuf,
    uid: u32,
    /// (t_ms, full total) from the previous `system` call, for `full_now`.
    prev_full: Cell<Option<(u64, u64)>>,
}

fn rel(root: &Path, p: &str) -> PathBuf {
    root.join(p.trim_start_matches('/'))
}

fn mem_of(p: &Proc) -> u64 {
    p.status.rss + p.status.swap
}

/// Basename of `exe`, without the " (deleted)" the kernel adds for a replaced binary.
fn exe_name(p: &Proc) -> Option<String> {
    let name = p.exe.as_ref()?.file_name()?.to_str()?;
    Some(name.strip_suffix(" (deleted)").unwrap_or(name).to_string())
}

/// Stall percentage between two `full total=` readings (microseconds).
fn stall_pct(prev: Option<(u64, u64)>, t_ms: u64, total: u64) -> Option<f32> {
    let (t0, total0) = prev?;
    if t_ms <= t0 || total < total0 {
        return None;
    }
    let pct = (total - total0) as f64 / ((t_ms - t0) as f64 * 1000.0) * 100.0;
    Some(pct.min(100.0) as f32)
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
        Self {
            root,
            uid,
            prev_full: Cell::new(None),
        }
    }

    pub fn system(&self, t_ms: u64) -> Option<SystemSample> {
        let psi = parse_psi(&fs::read_to_string(self.root.join("proc/pressure/memory")).ok()?)?;
        let mem = parse_meminfo(&fs::read_to_string(self.root.join("proc/meminfo")).ok()?)?;
        let zram_used = fs::read_to_string(self.root.join("sys/block/zram0/mm_stat"))
            .map(|s| parse_zram_used(&s))
            .unwrap_or(0);
        let full_now = stall_pct(self.prev_full.get(), t_ms, psi.full_total);
        self.prev_full.set(Some((t_ms, psi.full_total)));
        Some(SystemSample {
            t_ms,
            psi,
            full_now,
            mem,
            zram_used,
        })
    }

    fn user_service(&self) -> String {
        format!("/user.slice/user-{0}.slice/user@{0}.service", self.uid)
    }

    /// All desktop-user processes, grouped by lineage; sorted by key.
    pub fn apps(&self) -> Vec<AppSample> {
        let prefix = format!("{}/", self.user_service());
        let mut by_cgroup: HashMap<String, Vec<Proc>> = HashMap::new();
        let mut lineage = HashMap::new();
        for pid in list_pids(&self.root) {
            if let Some(p) = read_proc(&self.root, pid) {
                if p.uid == self.uid {
                    lineage.insert(p.pid, (p.status.ppid, p.status.name.clone(), exe_name(&p)));
                }
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
            // The whole cgroup is a target only if every process in it is the user's.
            let single = groups.len() == 1 && !self.has_foreign(cgroup, &index);
            let steam = procs.iter().any(is_steam);
            for (root_pid, mut members) in groups {
                members.sort_by_key(|p| p.pid);
                let root = index[&root_pid];
                let (key, kind, bytes, ram) = if single {
                    let dir = rel(&self.root, &format!("sys/fs/cgroup{cgroup}"));
                    let ram = read_u64_file(&dir.join("memory.current"));
                    let bytes = ram + read_u64_file(&dir.join("memory.swap.current"));
                    (
                        AppKey {
                            cgroup: cgroup.clone(),
                            root_pid: None,
                        },
                        TargetKind::Cgroup,
                        bytes,
                        ram,
                    )
                } else {
                    (
                        AppKey {
                            cgroup: cgroup.clone(),
                            root_pid: Some(root_pid),
                        },
                        TargetKind::Processes {
                            pids: members.iter().map(|p| (p.pid, p.start_time)).collect(),
                        },
                        members.iter().map(|p| mem_of(p)).sum(),
                        members.iter().map(|p| p.status.rss).sum(),
                    )
                };
                let mut exe_names: Vec<String> = Vec::new();
                for n in members.iter().filter_map(|p| exe_name(p)) {
                    if !exe_names.contains(&n) {
                        exe_names.push(n);
                    }
                }
                // Split from the Steam client by lineage: its root's parent is in the
                // same cgroup, so it is a child the client launched, not the client.
                let split_from_steam = steam
                    && !single
                    && !members.iter().any(|p| is_steam(p))
                    && index.contains_key(&root.status.ppid);
                // Steam's own helpers (steamwebhelper under srt-bwrap/pv-adverb) split
                // off too; only what reaper launched is a game. Under gamescope with
                // Steam in between, the reaper rule alone decides.
                let up = ancestry(&lineage, root_pid);
                let is_game = (split_from_steam && up.reaper)
                    || exe_names.iter().any(|n| GAME_LOADERS.contains(&n.as_str()))
                    || up.gamescope_direct
                    || (up.gamescope && up.reaper);
                out.push(AppSample {
                    key,
                    name: root.status.name.clone(),
                    exe: root.exe.clone(),
                    kind,
                    bytes,
                    ram,
                    names: members.iter().map(|p| p.status.name.clone()).collect(),
                    exe_names,
                    is_game,
                });
            }
        }
        out.sort_by(|a, b| (&a.key.cgroup, a.key.root_pid).cmp(&(&b.key.cgroup, b.key.root_pid)));
        out
    }

    /// True if `cgroup.procs` lists a live pid owned by someone other than the desktop
    /// user (root helpers, other users). `ours` holds the user's pids in this cgroup.
    fn has_foreign(&self, cgroup: &str, ours: &HashMap<i32, &Proc>) -> bool {
        let dir = rel(&self.root, &format!("sys/fs/cgroup{cgroup}"));
        let Ok(procs) = fs::read_to_string(dir.join("cgroup.procs")) else {
            return false;
        };
        procs
            .split_whitespace()
            .filter_map(|t| t.parse::<i32>().ok())
            .filter(|pid| !ours.contains_key(pid))
            .any(|pid| {
                fs::read_to_string(rel(&self.root, &format!("proc/{pid}/status")))
                    .ok()
                    .and_then(|t| parse_status(&t))
                    .is_some_and(|st| st.uid != self.uid)
            })
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
        Sampler::new(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/steam"),
            1000,
        )
    }

    #[test]
    fn firefox_is_one_cgroup_target() {
        let apps = sampler().apps();
        let ff: Vec<_> = apps
            .iter()
            .filter(|a| a.key.cgroup.contains("firefox"))
            .collect();
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
        let st: Vec<_> = apps
            .iter()
            .filter(|a| a.key.cgroup.contains("steam"))
            .collect();
        assert_eq!(st.len(), 3);
        let client = st.iter().find(|a| a.key.root_pid == Some(100)).unwrap();
        let game = st.iter().find(|a| a.key.root_pid == Some(102)).unwrap();
        assert_eq!(
            game.kind,
            TargetKind::Processes {
                pids: vec![(102, 1102), (103, 1103)]
            }
        );
        assert_eq!(game.bytes, 9 * GIB);
        assert_eq!(game.name, "game.exe");
        assert_eq!(
            client.kind,
            TargetKind::Processes {
                pids: vec![(100, 1100), (101, 1101), (104, 1104), (105, 1105)]
            }
        );
        assert_eq!(client.bytes, (400 + 2 + 4 + 2) << 20);
        assert_eq!(
            client.names,
            vec!["steam", "reaper", "srt-bwrap", "pv-adverb"]
        );
        let helper = st.iter().find(|a| a.key.root_pid == Some(106)).unwrap();
        assert_eq!(helper.name, "steamwebhelper");
        assert_eq!(helper.bytes, GIB);
    }

    #[test]
    fn other_users_and_system_ignored() {
        let apps = sampler().apps();
        assert_eq!(apps.len(), 4);
        assert!(apps.iter().all(|a| !a.key.cgroup.contains("system.slice")));
        assert!(sampler_for(1001).apps().is_empty());
    }

    fn sampler_for(uid: u32) -> Sampler {
        Sampler::new(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/steam"),
            uid,
        )
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

    /// A private copy of the steam fixture (symlinks kept), safe to modify.
    fn fixture_copy() -> tempfile::TempDir {
        fn copy(from: &Path, to: &Path) {
            fs::create_dir_all(to).unwrap();
            for e in fs::read_dir(from).unwrap() {
                let e = e.unwrap();
                let (src, dst) = (e.path(), to.join(e.file_name()));
                let ft = e.file_type().unwrap();
                if ft.is_symlink() {
                    std::os::unix::fs::symlink(fs::read_link(&src).unwrap(), &dst).unwrap();
                } else if ft.is_dir() {
                    copy(&src, &dst);
                } else {
                    fs::copy(&src, &dst).unwrap();
                }
            }
        }
        let d = tempfile::tempdir().unwrap();
        copy(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/steam"),
            d.path(),
        );
        d
    }

    #[test]
    fn full_now_from_total_delta() {
        let d = fixture_copy();
        let s = Sampler::new(d.path().to_path_buf(), 1000);
        let psi = |total: u64| {
            let p = d.path().join("proc/pressure/memory");
            fs::write(
                &p,
                format!("some avg10=60.00 avg60=0 avg300=0 total={total}\nfull avg10=55.00 avg60=0 avg300=0 total={total}\n"),
            )
            .unwrap();
        };
        psi(10_000_000);
        assert_eq!(s.system(1_000).unwrap().full_now, None);
        // 500 ms of full stall in a 2 s interval: 25%, whatever avg10 says.
        psi(10_500_000);
        let now = s.system(3_000).unwrap();
        assert_eq!(now.psi.full_avg10, 55.0);
        assert_eq!(now.full_now, Some(25.0));
        psi(10_500_000);
        assert_eq!(s.system(4_000).unwrap().full_now, Some(0.0));
        // A counter that went backwards gives no reading rather than a wrong one.
        psi(1);
        assert_eq!(s.system(5_000).unwrap().full_now, None);
    }

    #[test]
    fn ram_excludes_swap() {
        let apps = sampler().apps();
        let ff = apps.iter().find(|a| a.name == "firefox").unwrap();
        assert_eq!(ff.ram, 2_000_000_000);
        assert_eq!(ff.bytes, 2_000_000_100);
        let game = apps.iter().find(|a| a.name == "game.exe").unwrap();
        assert_eq!(game.ram, 9 * GIB);
    }

    #[test]
    fn exe_basenames_for_allowlist() {
        let apps = sampler().apps();
        let game = apps.iter().find(|a| a.name == "game.exe").unwrap();
        assert_eq!(game.exe_names, vec!["wine64-preloader"]);
        let client = apps.iter().find(|a| a.name == "steam").unwrap();
        assert_eq!(
            client.exe_names,
            vec!["steam", "reaper", "srt-bwrap", "pv-adverb"]
        );
    }

    #[test]
    fn steam_game_is_a_game_client_and_firefox_are_not() {
        let apps = sampler().apps();
        let game = |n: &str| apps.iter().find(|a| a.name == n).unwrap().is_game;
        assert!(game("game.exe"));
        assert!(!game("steam"));
        assert!(!game("firefox"));
        // Split from the client like a game, but not launched by reaper.
        assert!(!game("steamwebhelper"));
    }

    #[test]
    fn gamescope_session_steam_helpers_are_not_games() {
        // Bazzite's gamescope session: gamescope launches Steam, Steam launches the rest.
        let lineage: Lineage = [
            (10, (1, "gamescope-wl", Some("gamescope"))),
            (11, (10, "steam", Some("steam"))),
            (12, (11, "srt-bwrap", Some("srt-bwrap"))),
            (13, (12, "steamwebhelper", Some("steamwebhelper"))),
            (14, (11, "reaper", Some("reaper"))),
            (15, (14, "game.exe", Some("wine64-preloader"))),
            (16, (10, "firefox", Some("firefox"))),
            (17, (14, "gamescope", Some("gamescope"))),
            (18, (17, "nested", None)),
        ]
        .into_iter()
        .map(|(pid, (ppid, comm, exe))| (pid, (ppid, comm.to_string(), exe.map(String::from))))
        .collect();
        let game = |pid| {
            let a = ancestry(&lineage, pid);
            a.gamescope_direct || (a.gamescope && a.reaper)
        };
        assert!(!game(11), "the Steam client");
        assert!(
            !game(13),
            "steamwebhelper: Steam is between gamescope and it"
        );
        assert!(game(15), "launched by reaper");
        assert!(game(16), "under gamescope with no Steam in between");
        assert!(game(18), "a nested gamescope a game started");
    }

    #[test]
    fn app_under_gamescope_is_a_game() {
        let d = fixture_copy();
        // Re-parent firefox under a gamescope process outside the user service.
        let gs = d.path().join("proc/400");
        fs::create_dir_all(&gs).unwrap();
        fs::write(
            gs.join("status"),
            "Name:\tgamescope-wl\nPPid:\t1\nUid:\t1000\t1000\t1000\t1000\nVmRSS:\t1024 kB\nVmSwap:\t0 kB\n",
        )
        .unwrap();
        fs::write(
            gs.join("stat"),
            "400 (gamescope-wl) S 1 1 1 0 -1 4194304 0 0 0 0 0 0 0 0 20 0 1 0 1400 1 1 0\n",
        )
        .unwrap();
        fs::write(
            gs.join("cgroup"),
            "0::/user.slice/user-1000.slice/session-2.scope\n",
        )
        .unwrap();
        let st = d.path().join("proc/200/status");
        let text = fs::read_to_string(&st)
            .unwrap()
            .replace("PPid:\t1\n", "PPid:\t400\n");
        fs::write(&st, text).unwrap();
        let apps = Sampler::new(d.path().to_path_buf(), 1000).apps();
        let ff = apps.iter().find(|a| a.name == "firefox").unwrap();
        assert!(ff.is_game);
        assert!(!apps.iter().find(|a| a.name == "steam").unwrap().is_game);
    }

    #[test]
    fn cgroup_with_root_process_becomes_process_target() {
        let d = fixture_copy();
        let ff = "user.slice/user-1000.slice/user@1000.service/app.slice/app-org.mozilla.firefox-1.scope";
        let p = d.path().join("proc/204");
        fs::create_dir_all(&p).unwrap();
        fs::write(
            p.join("status"),
            "Name:\thelper\nPPid:\t1\nUid:\t0\t0\t0\t0\nVmRSS:\t1024 kB\nVmSwap:\t0 kB\n",
        )
        .unwrap();
        fs::write(
            p.join("stat"),
            "204 (helper) S 1 1 1 0 -1 4194304 0 0 0 0 0 0 0 0 20 0 1 0 1204 1 1 0\n",
        )
        .unwrap();
        fs::write(p.join("cgroup"), format!("0::/{ff}\n")).unwrap();
        fs::write(
            d.path().join("sys/fs/cgroup").join(ff).join("cgroup.procs"),
            "200\n201\n202\n204\n",
        )
        .unwrap();
        let apps = Sampler::new(d.path().to_path_buf(), 1000).apps();
        let ff: Vec<_> = apps
            .iter()
            .filter(|a| a.key.cgroup.contains("firefox"))
            .collect();
        assert_eq!(ff.len(), 1);
        // Never freeze or kill the cgroup: only the desktop user's own processes.
        assert_eq!(
            ff[0].kind,
            TargetKind::Processes {
                pids: vec![(200, 1200), (201, 1201), (202, 1202)]
            }
        );
        assert_eq!(ff[0].key.root_pid, Some(200));
        assert_eq!(ff[0].bytes, 2 * GIB);
        assert_eq!(ff[0].ram, 2 * GIB);
    }

    #[test]
    fn cgroup_totals_lists_populated_cgroups() {
        let t = sampler().cgroup_totals();
        let base = "/user.slice/user-1000.slice/user@1000.service/app.slice";
        assert_eq!(
            t,
            vec![
                (
                    format!("{base}/app-org.mozilla.firefox-1.scope"),
                    2_000_000_100
                ),
                (
                    format!("{base}/app-steam@autostart.service"),
                    10_000_000_500
                ),
            ]
        );
    }
}
