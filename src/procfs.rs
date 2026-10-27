//! Parsers and readers for /proc, /sys and cgroup v2 files.
//! Pure parsers take `&str`; readers take a `root` so tests can use fixtures.

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Psi {
    pub some_avg10: f32,
    pub full_avg10: f32,
    /// `full total=`: cumulative microseconds all tasks were stalled on memory.
    pub full_total: u64,
}

fn avg10(line: &str) -> Option<f32> {
    line.split_whitespace()
        .find_map(|t| t.strip_prefix("avg10="))?
        .parse()
        .ok()
}

fn total(line: &str) -> Option<u64> {
    line.split_whitespace()
        .find_map(|t| t.strip_prefix("total="))?
        .parse()
        .ok()
}

pub fn parse_psi(s: &str) -> Option<Psi> {
    let mut some = None;
    let mut full = None;
    let mut full_total = None;
    for line in s.lines() {
        if line.starts_with("some ") {
            some = avg10(line);
        } else if line.starts_with("full ") {
            full = avg10(line);
            full_total = total(line);
        }
    }
    Some(Psi {
        some_avg10: some?,
        full_avg10: full?,
        full_total: full_total?,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemInfo {
    pub total: u64,
    pub available: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

fn kb_field(s: &str, key: &str) -> Option<u64> {
    let rest = s
        .lines()
        .find_map(|l| l.strip_prefix(key))?
        .strip_prefix(':')?;
    let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
    kb.checked_mul(1024)
}

pub fn parse_meminfo(s: &str) -> Option<MemInfo> {
    Some(MemInfo {
        total: kb_field(s, "MemTotal")?,
        available: kb_field(s, "MemAvailable")?,
        // No swap configured: the lines are absent.
        swap_total: kb_field(s, "SwapTotal").unwrap_or(0),
        swap_free: kb_field(s, "SwapFree").unwrap_or(0),
    })
}

/// Third field of zram `mm_stat` (mem_used_total) in bytes; 0 on error.
pub fn parse_zram_used(mm_stat: &str) -> u64 {
    mm_stat
        .split_whitespace()
        .nth(2)
        .and_then(|t| t.parse().ok())
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcStatus {
    pub name: String,
    pub ppid: i32,
    pub uid: u32,
    pub rss: u64,
    pub swap: u64,
}

pub fn parse_status(s: &str) -> Option<ProcStatus> {
    let name = s
        .lines()
        .find_map(|l| l.strip_prefix("Name:"))?
        .trim()
        .to_string();
    let ppid = s
        .lines()
        .find_map(|l| l.strip_prefix("PPid:"))?
        .trim()
        .parse()
        .ok()?;
    let uid = s
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(ProcStatus {
        name,
        ppid,
        uid,
        rss: kb_field(s, "VmRSS").unwrap_or(0),
        swap: kb_field(s, "VmSwap").unwrap_or(0),
    })
}

/// Field 22 (starttime) of /proc/<pid>/stat; parsed after the last ')'.
pub fn parse_start_time(stat: &str) -> Option<u64> {
    let after = &stat[stat.rfind(')')? + 1..];
    // After ')' the fields start at field 3 (state), so field 22 is index 19.
    after.split_whitespace().nth(19)?.parse().ok()
}

/// "0::/user.slice/..." -> "/user.slice/..."
pub fn parse_cgroup_path(proc_cgroup: &str) -> Option<String> {
    proc_cgroup
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|p| p.trim().to_string())
}

/// Reads a single-integer file; "max", unparsable or missing -> 0.
pub fn read_u64_file(path: &Path) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    pub pid: i32,
    pub status: ProcStatus,
    pub start_time: u64,
    pub exe: Option<PathBuf>,
    pub uid: u32,
    pub cgroup: String,
}

fn under(root: &Path, p: &str) -> PathBuf {
    root.join(p.trim_start_matches('/'))
}

pub fn read_proc(root: &Path, pid: i32) -> Option<Proc> {
    let dir = under(root, &format!("proc/{pid}"));
    let status = parse_status(&fs::read_to_string(dir.join("status")).ok()?)?;
    let start_time = parse_start_time(&fs::read_to_string(dir.join("stat")).ok()?)?;
    let cgroup = parse_cgroup_path(&fs::read_to_string(dir.join("cgroup")).ok()?)?;
    let exe = fs::read_link(dir.join("exe")).ok();
    Some(Proc {
        pid,
        uid: status.uid,
        status,
        start_time,
        exe,
        cgroup,
    })
}

pub fn list_pids(root: &Path) -> Vec<i32> {
    let Ok(rd) = fs::read_dir(under(root, "proc")) else {
        return Vec::new();
    };
    rd.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/basic")
    }

    #[test]
    fn psi_parses_some_and_full() {
        let p = parse_psi("some avg10=1.50 avg60=0.00 avg300=0.01 total=1\nfull avg10=0.75 avg60=0 avg300=0 total=34906813\n").unwrap();
        assert_eq!(p.some_avg10, 1.5);
        assert_eq!(p.full_avg10, 0.75);
        assert_eq!(p.full_total, 34906813);
    }

    #[test]
    fn meminfo_in_bytes() {
        let m = parse_meminfo(
            "MemTotal:       31876543 kB\nMemFree: 1 kB\nMemAvailable:   20000000 kB\n",
        )
        .unwrap();
        assert_eq!(m.total, 31876543 * 1024);
        assert_eq!(m.available, 20000000 * 1024);
    }

    #[test]
    fn meminfo_reads_swap() {
        let m = parse_meminfo(
            "MemTotal: 100 kB\nMemAvailable: 50 kB\nSwapTotal:      8388604 kB\nSwapFree:       8000000 kB\n",
        )
        .unwrap();
        assert_eq!(m.swap_total, 8388604 * 1024);
        assert_eq!(m.swap_free, 8000000 * 1024);
        let m = parse_meminfo("MemTotal: 100 kB\nMemAvailable: 50 kB\n").unwrap();
        assert_eq!((m.swap_total, m.swap_free), (0, 0));
    }

    #[test]
    fn zram_used_third_field() {
        assert_eq!(
            parse_zram_used(
                "3930337280 953574207 976715776 0 5300969472 26220 451314 36771 155291"
            ),
            976715776
        );
    }

    #[test]
    fn status_rss_plus_swap() {
        let s = parse_status("Name:\tfoo\nPPid:\t42\nUid:\t1000\t1000\t1000\t1000\nVmRSS:\t     100 kB\nVmSwap:\t      50 kB\n").unwrap();
        assert_eq!(s.name, "foo");
        assert_eq!(s.ppid, 42);
        assert_eq!(s.uid, 1000);
        assert_eq!(s.rss, 102400);
        assert_eq!(s.swap, 51200);
    }

    #[test]
    fn status_kernel_thread_has_zero_rss() {
        let s = parse_status("Name:\tkthreadd\nPPid:\t0\nUid:\t0\t0\t0\t0\n").unwrap();
        assert_eq!((s.rss, s.swap), (0, 0));
    }

    #[test]
    fn start_time_survives_paren_in_comm() {
        let stat = fs::read_to_string(fixture().join("proc/1235/stat")).unwrap();
        assert_eq!(parse_start_time(&stat), Some(777));
        let stat = fs::read_to_string(fixture().join("proc/1234/stat")).unwrap();
        assert_eq!(parse_start_time(&stat), Some(4200487));
    }

    #[test]
    fn cgroup_v2_line() {
        let p =
            "/user.slice/user-1000.slice/user@1000.service/app.slice/app-steam@autostart.service";
        assert_eq!(parse_cgroup_path(&format!("0::{p}\n")).as_deref(), Some(p));
    }

    #[test]
    fn missing_files_are_tolerated() {
        assert_eq!(read_proc(&fixture(), 999999), None);
        assert_eq!(read_u64_file(Path::new("/nonexistent/olly/x")), 0);
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("max");
        fs::write(&f, "max\n").unwrap();
        assert_eq!(read_u64_file(&f), 0);
        assert_eq!(parse_zram_used(""), 0);
    }

    #[test]
    fn reads_fixture_proc_and_lists_pids() {
        let p = read_proc(&fixture(), 1235).unwrap();
        assert_eq!(p.status.name, "a) (b");
        assert_eq!(p.uid, 1000);
        assert_eq!(p.start_time, 777);
        assert!(p.cgroup.starts_with("/user.slice/"));
        assert_eq!(p.exe, None);
        let p = read_proc(&fixture(), 1234).unwrap();
        assert_eq!(p.exe, Some(PathBuf::from("/usr/bin/cp")));
        let mut pids = list_pids(&fixture());
        pids.sort();
        assert_eq!(pids, vec![1234, 1235]);
    }

    #[test]
    fn real_fixtures_parse() {
        let f = fixture();
        assert!(parse_psi(&fs::read_to_string(f.join("proc/pressure/memory")).unwrap()).is_some());
        assert!(parse_meminfo(&fs::read_to_string(f.join("proc/meminfo")).unwrap()).is_some());
        assert!(
            parse_zram_used(&fs::read_to_string(f.join("sys/block/zram0/mm_stat")).unwrap()) > 0
        );
    }
}
