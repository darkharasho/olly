//! Incident recording and the append-only JSONL incident log.

use crate::engine::{Action, Event, Target};
use crate::history::History;
use crate::sample::{AppKey, AppSample, SystemSample};
use crate::voice::fmt_bytes;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TopApp {
    pub name: String,
    pub key: AppKey,
    pub bytes: u64,
    pub rate: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Incident {
    pub id: u64,
    pub start_ms_wall: u64,
    pub end_ms_wall: u64,
    pub peak_full: f32,
    pub min_avail: u64,
    pub top_at_peak: Vec<TopApp>,
    pub actions: Vec<(u64, Action, bool)>,
    pub summary: String,
    pub diagnosis: Option<String>,
}

#[derive(Default)]
pub struct IncidentRecorder {
    current: Option<Incident>,
}

const TOP_N: usize = 10;
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const KEEP_INCIDENTS: usize = 200;

impl IncidentRecorder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Call once per sample, after the engine step. Ignored outside an incident.
    pub fn on_sample(&mut self, sys: &SystemSample, apps: &[AppSample], hist: &History, _wall_ms: u64) {
        let Some(inc) = self.current.as_mut() else { return };
        inc.min_avail = inc.min_avail.min(sys.mem.available);
        // peak_full starts below zero so the first sample always records the top apps.
        if sys.psi.full_avg10 > inc.peak_full {
            inc.peak_full = sys.psi.full_avg10;
            let mut top: Vec<&AppSample> = apps.iter().collect();
            top.sort_by(|a, b| b.bytes.cmp(&a.bytes));
            inc.top_at_peak = top
                .into_iter()
                .take(TOP_N)
                .map(|a| TopApp {
                    name: a.name.clone(),
                    key: a.key.clone(),
                    bytes: a.bytes,
                    rate: hist.rate(&a.key, sys.t_ms),
                })
                .collect();
        }
    }

    /// Returns the completed incident on `IncidentEnd`.
    pub fn on_event(&mut self, ev: &Event, wall_ms: u64) -> Option<Incident> {
        match ev {
            Event::IncidentStart { id, .. } => {
                self.current = Some(Incident {
                    id: *id,
                    start_ms_wall: wall_ms,
                    end_ms_wall: wall_ms,
                    peak_full: -1.0,
                    min_avail: u64::MAX,
                    top_at_peak: Vec::new(),
                    actions: Vec::new(),
                    summary: String::new(),
                    diagnosis: None,
                });
                None
            }
            Event::Acted { action, dry_run, .. } => {
                if let Some(inc) = self.current.as_mut() {
                    inc.actions.push((wall_ms, action.clone(), *dry_run));
                }
                None
            }
            Event::IncidentEnd { id, .. } => {
                let mut inc = self.current.take().filter(|i| i.id == *id)?;
                inc.end_ms_wall = wall_ms;
                inc.peak_full = inc.peak_full.max(0.0);
                if inc.min_avail == u64::MAX {
                    inc.min_avail = 0;
                }
                inc.summary = summarize(&inc);
                Some(inc)
            }
            _ => None,
        }
    }
}

/// One olly-voice line: what was done, then how long it took to recover.
fn summarize(inc: &Incident) -> String {
    let secs = inc.end_ms_wall.saturating_sub(inc.start_ms_wall) / 1000;
    let pick = |f: fn(&Action) -> Option<&Target>| {
        inc.actions.iter().rev().find_map(|(_, a, dry)| f(a).map(|t| (t, *dry)))
    };
    let kill = pick(|a| match a {
        Action::Kill { target, .. } => Some(target),
        _ => None,
    });
    let freeze = pick(|a| match a {
        Action::Freeze { target } => Some(target),
        _ => None,
    });
    let throttle = pick(|a| match a {
        Action::Throttle { target, .. } => Some(target),
        _ => None,
    });
    let describe = |t: &Target| {
        let mut s = format!("{} ({}", t.name, fmt_bytes(t.bytes));
        let per_min = inc
            .top_at_peak
            .iter()
            .find(|a| a.key == t.key)
            .map_or(0.0, |a| a.rate * 60.0);
        if per_min >= 1e6 {
            s.push_str(&format!(", growing {}/min", fmt_bytes(per_min as u64)));
        }
        s.push(')');
        s
    };
    let lead = if let Some((t, dry)) = kill {
        let verb = if dry { "Would have closed" } else { "Closed" };
        format!("{verb} {}.", describe(t))
    } else if let Some((t, dry)) = freeze {
        let verb = if dry { "Would have paused" } else { "Paused" };
        format!("{verb} {}.", describe(t))
    } else if let Some((t, dry)) = throttle {
        let verb = if dry { "Would have slowed" } else { "Slowed" };
        format!("{verb} {}.", describe(t))
    } else {
        "Memory ran tight and eased up on its own.".to_string()
    };
    format!("{lead} Recovered in {secs}s.")
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Line {
    Incident(Incident),
    Diagnosis { id: u64, text: String },
}

pub struct IncidentStore {
    path: PathBuf,
}

impl IncidentStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn write_line(&self, line: &Line) -> io::Result<()> {
        let mut buf = serde_json::to_vec(line).map_err(io::Error::other)?;
        buf.push(b'\n');
        let mut f = OpenOptions::new().create(true).append(true).open(&self.path)?;
        f.write_all(&buf)
    }

    pub fn append(&self, inc: &Incident) -> io::Result<()> {
        self.write_line(&Line::Incident(inc.clone()))
    }

    pub fn append_diagnosis(&self, id: u64, text: &str) -> io::Result<()> {
        self.write_line(&Line::Diagnosis { id, text: text.to_string() })
    }

    /// All parseable lines in file order; unreadable or corrupt lines are skipped.
    fn read_lines(&self) -> Vec<Line> {
        let Ok(f) = File::open(&self.path) else { return Vec::new() };
        BufReader::new(f)
            .split(b'\n')
            .map_while(Result::ok)
            .filter_map(|l| serde_json::from_slice(&l).ok())
            .collect()
    }

    /// Incidents in file order with the latest diagnosis for each merged in.
    fn merged(&self) -> Vec<Incident> {
        let mut incs = Vec::new();
        let mut diag: HashMap<u64, String> = HashMap::new();
        for line in self.read_lines() {
            match line {
                Line::Incident(i) => incs.push(i),
                Line::Diagnosis { id, text } => {
                    diag.insert(id, text);
                }
            }
        }
        for i in &mut incs {
            if let Some(t) = diag.get(&i.id) {
                i.diagnosis = Some(t.clone());
            }
        }
        incs
    }

    /// Newest first.
    pub fn list(&self, limit: usize, offset: usize) -> Vec<Incident> {
        self.merged().into_iter().rev().skip(offset).take(limit).collect()
    }

    pub fn get(&self, id: u64) -> Option<Incident> {
        self.merged().into_iter().rev().find(|i| i.id == id)
    }

    /// Over 5 MiB: rewrite keeping the newest 200 incidents and their diagnoses.
    pub fn rotate_if_needed(&self) -> io::Result<()> {
        match fs::metadata(&self.path) {
            Ok(m) if m.len() > MAX_LOG_BYTES => {}
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        }
        let lines = self.read_lines();
        let count = lines.iter().filter(|l| matches!(l, Line::Incident(_))).count();
        let mut skip = count.saturating_sub(KEEP_INCIDENTS);
        let kept_ids: HashSet<u64> = lines
            .iter()
            .filter_map(|l| match l {
                Line::Incident(i) => Some(i.id),
                _ => None,
            })
            .skip(skip)
            .collect();
        let tmp = self.path.with_extension("jsonl.tmp");
        {
            let mut out = io::BufWriter::new(File::create(&tmp)?);
            for line in &lines {
                let keep = match line {
                    Line::Incident(_) if skip > 0 => {
                        skip -= 1;
                        false
                    }
                    Line::Incident(_) => true,
                    Line::Diagnosis { id, .. } => kept_ids.contains(id),
                };
                if keep {
                    serde_json::to_writer(&mut out, line).map_err(io::Error::other)?;
                    out.write_all(b"\n")?;
                }
            }
            out.flush()?;
        }
        fs::rename(&tmp, &self.path)
    }

    /// Highest recorded id + 1, so ids survive restarts.
    pub fn next_id(&self) -> u64 {
        self.read_lines()
            .iter()
            .filter_map(|l| match l {
                Line::Incident(i) => Some(i.id),
                _ => None,
            })
            .max()
            .map_or(1, |m| m + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::engine::{KillReason, Level, Target};
    use crate::procfs::{MemInfo, Psi};
    use crate::sample::TargetKind;
    use crate::voice;
    use std::io::Write;

    const GB: u64 = 1_000_000_000;

    fn sys(t: u64, full: f32, avail: u64) -> SystemSample {
        SystemSample {
            t_ms: t,
            psi: Psi { some_avg10: full, full_avg10: full },
            mem: MemInfo { total: 32 * GB, available: avail },
            zram_used: 0,
        }
    }

    fn app(i: u32, bytes: u64) -> AppSample {
        AppSample {
            key: AppKey { cgroup: format!("/app{i}"), root_pid: None },
            name: format!("app{i}"),
            exe: None,
            kind: TargetKind::Cgroup,
            bytes,
            names: vec![format!("app{i}")],
        }
    }

    fn target(i: u32, bytes: u64) -> Target {
        Target {
            id: i as u64,
            key: AppKey { cgroup: format!("/app{i}"), root_pid: None },
            name: format!("app{i}"),
            kind: TargetKind::Cgroup,
            bytes,
        }
    }

    fn inc(id: u64, pad: usize) -> Incident {
        Incident {
            id,
            start_ms_wall: id * 1000,
            end_ms_wall: id * 1000 + 500,
            peak_full: 12.5,
            min_avail: 3 * GB,
            top_at_peak: vec![],
            actions: vec![],
            summary: "x".repeat(pad),
            diagnosis: None,
        }
    }

    fn store() -> (tempfile::TempDir, IncidentStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = IncidentStore::new(dir.path().join("incidents.jsonl"));
        (dir, s)
    }

    #[test]
    fn recorder_captures_peak_top10() {
        let cfg = Config::default();
        let mut hist = History::new(&cfg.growth);
        let mut rec = IncidentRecorder::new();
        // Samples before an incident are ignored.
        rec.on_sample(&sys(0, 99.0, GB), &[app(0, GB)], &hist, 1_000_000);
        assert_eq!(rec.on_event(&Event::IncidentStart { id: 7, t_ms: 1000 }, 1_001_000), None);

        let big: Vec<AppSample> = (0..12).map(|i| app(i, (i as u64 + 1) * GB)).collect();
        hist.record(1000, &big);
        rec.on_sample(&sys(1000, 20.0, 5 * GB), &big, &hist, 1_001_000);
        // Lower full, lower avail: min_avail updates, peak and top stay.
        let small = vec![app(0, GB)];
        rec.on_sample(&sys(2000, 10.0, 2 * GB), &small, &hist, 1_002_000);
        // Higher full: new peak with different top.
        let grow = vec![app(1, 40 * GB), app(2, 2 * GB)];
        rec.on_sample(&sys(3000, 30.0, 4 * GB), &grow, &hist, 1_003_000);
        rec.on_sample(&sys(4000, 5.0, 6 * GB), &big, &hist, 1_004_000);

        let done = rec
            .on_event(&Event::IncidentEnd { id: 7, t_ms: 9000 }, 1_009_000)
            .expect("incident completes");
        assert_eq!(done.id, 7);
        assert_eq!(done.start_ms_wall, 1_001_000);
        assert_eq!(done.end_ms_wall, 1_009_000);
        assert_eq!(done.peak_full, 30.0);
        assert_eq!(done.min_avail, 2 * GB);
        assert_eq!(done.top_at_peak.len(), 2);
        assert_eq!(done.top_at_peak[0].name, "app1");
        assert_eq!(done.top_at_peak[0].bytes, 40 * GB);

        // Top 10 cap, biggest first.
        let mut rec = IncidentRecorder::new();
        rec.on_event(&Event::IncidentStart { id: 8, t_ms: 0 }, 0);
        rec.on_sample(&sys(1000, 20.0, 5 * GB), &big, &hist, 1000);
        let done = rec.on_event(&Event::IncidentEnd { id: 8, t_ms: 2000 }, 2000).unwrap();
        assert_eq!(done.top_at_peak.len(), 10);
        assert_eq!(done.top_at_peak[0].name, "app11");
        assert_eq!(done.top_at_peak[9].name, "app2");
        assert!(done.summary.contains("Recovered in 2s"), "{}", done.summary);
    }

    #[test]
    fn recorder_collects_actions_and_summary() {
        let cfg = Config::default();
        let mut hist = History::new(&cfg.growth);
        let mut g = app(1, 10 * GB);
        hist.record(50_000, &[g.clone()]);
        g.bytes += 100_000_000; // +100 MB in 10 s => 600 MB/min
        let mut rec = IncidentRecorder::new();
        rec.on_event(&Event::IncidentStart { id: 1, t_ms: 0 }, 100_000);
        hist.record(60_000, &[g.clone()]);
        rec.on_sample(&sys(60_000, 30.0, GB), &[g.clone()], &hist, 160_000);
        let t = target(1, 21 * GB);
        let action = Action::Kill { target: t, reason: KillReason::Emergency };
        let msg = voice::killed("app1", 21 * GB, KillReason::Emergency, false);
        rec.on_event(
            &Event::Acted { action: action.clone(), message: msg, dry_run: false },
            161_000,
        );
        rec.on_event(&Event::Level { level: Level::Calm }, 162_000);
        let done = rec.on_event(&Event::IncidentEnd { id: 1, t_ms: 114_000 }, 114_000).unwrap();
        assert_eq!(done.actions, vec![(161_000, action, false)]);
        assert!(done.summary.starts_with("Closed app1 (21 GB"), "{}", done.summary);
        assert!(done.summary.ends_with("Recovered in 14s."), "{}", done.summary);
        assert!(done.summary.contains("growing 600 MB/min"), "{}", done.summary);
        assert!(!done.summary.contains("olly"), "{}", done.summary);
    }

    #[test]
    fn summary_without_actions() {
        let mut rec = IncidentRecorder::new();
        rec.on_event(&Event::IncidentStart { id: 1, t_ms: 0 }, 0);
        let done = rec.on_event(&Event::IncidentEnd { id: 1, t_ms: 30_000 }, 30_000).unwrap();
        assert!(done.summary.ends_with("Recovered in 30s."), "{}", done.summary);
        assert_eq!(done.min_avail, 0);
        assert_eq!(done.peak_full, 0.0);
    }

    #[test]
    fn store_roundtrip_newest_first() {
        let (_d, s) = store();
        assert!(s.list(10, 0).is_empty());
        for id in 1..=5 {
            s.append(&inc(id, 3)).unwrap();
        }
        let ids: Vec<u64> = s.list(10, 0).iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![5, 4, 3, 2, 1]);
        let ids: Vec<u64> = s.list(2, 1).iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![4, 3]);
        assert_eq!(s.get(3), Some(inc(3, 3)));
        assert_eq!(s.get(99), None);
    }

    #[test]
    fn diagnosis_merged_on_read() {
        let (_d, s) = store();
        s.append(&inc(1, 1)).unwrap();
        s.append(&inc(2, 1)).unwrap();
        s.append_diagnosis(1, "first guess").unwrap();
        s.append_diagnosis(1, "better guess").unwrap();
        assert_eq!(s.get(1).unwrap().diagnosis.as_deref(), Some("better guess"));
        assert_eq!(s.get(2).unwrap().diagnosis, None);
        let all = s.list(10, 0);
        assert_eq!(all[1].diagnosis.as_deref(), Some("better guess"));
        let raw = std::fs::read_to_string(&s.path).unwrap();
        assert!(raw.lines().next().unwrap().contains(r#""type":"incident""#));
        assert!(raw.contains(r#"{"type":"diagnosis","id":1,"text":"first guess"}"#));
    }

    #[test]
    fn corrupt_lines_are_skipped() {
        let (_d, s) = store();
        s.append(&inc(1, 1)).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&s.path)
            .unwrap()
            .write_all(b"{not json\n")
            .unwrap();
        s.append(&inc(2, 1)).unwrap();
        assert_eq!(s.list(10, 0).len(), 2);
        assert_eq!(s.next_id(), 3);
    }

    #[test]
    fn rotation_keeps_newest_200() {
        let (_d, s) = store();
        for id in 1..=300 {
            s.append(&inc(id, 20_000)).unwrap();
        }
        s.append_diagnosis(250, "kept").unwrap();
        s.append_diagnosis(5, "dropped").unwrap();
        assert!(std::fs::metadata(&s.path).unwrap().len() > 5 * 1024 * 1024);
        s.rotate_if_needed().unwrap();
        let all = s.list(1000, 0);
        assert_eq!(all.len(), 200);
        assert_eq!(all[0].id, 300);
        assert_eq!(all[199].id, 101);
        assert_eq!(s.get(250).unwrap().diagnosis.as_deref(), Some("kept"));
        assert_eq!(s.get(5), None);
        assert!(!std::fs::read_to_string(&s.path).unwrap().contains("dropped"));
        assert_eq!(s.next_id(), 301);
        // Under the limit: untouched.
        let before = std::fs::read(&s.path).unwrap();
        s.rotate_if_needed().unwrap();
        assert_eq!(std::fs::read(&s.path).unwrap(), before);
    }

    #[test]
    fn next_id_after_restart() {
        let (_d, s) = store();
        assert_eq!(s.next_id(), 1);
        s.append(&inc(4, 1)).unwrap();
        s.append(&inc(9, 1)).unwrap();
        s.append(&inc(6, 1)).unwrap();
        let reopened = IncidentStore::new(s.path.clone());
        assert_eq!(reopened.next_id(), 10);
    }
}
