use crate::config::Growth;
use crate::sample::{AppKey, AppSample};
use std::collections::{HashMap, VecDeque};

pub struct History {
    short_ms: u64,
    recent_ms: u64,
    min_bytes: u64,
    min_ratio: f64,
    points: HashMap<AppKey, VecDeque<(u64, u64)>>,
}

impl History {
    pub fn new(g: &Growth) -> Self {
        History {
            short_ms: g.short_window_secs * 1000,
            recent_ms: g.recent_window_secs * 1000,
            min_bytes: g.recent_min_bytes,
            min_ratio: g.recent_min_ratio as f64,
            points: HashMap::new(),
        }
    }

    pub fn record(&mut self, t_ms: u64, apps: &[AppSample]) {
        let cutoff = t_ms.saturating_sub(self.recent_ms);
        for app in apps {
            self.points
                .entry(app.key.clone())
                .or_default()
                .push_back((t_ms, app.bytes));
        }
        self.points.retain(|_, ring| {
            while ring.front().is_some_and(|&(t, _)| t < cutoff) {
                ring.pop_front();
            }
            !ring.is_empty()
        });
    }

    /// (oldest, newest) points within `window_ms` of `now_ms`.
    fn span(&self, key: &AppKey, now_ms: u64, window_ms: u64) -> Option<((u64, u64), (u64, u64))> {
        let start = now_ms.saturating_sub(window_ms);
        let mut it = self
            .points
            .get(key)?
            .iter()
            .filter(|&&(t, _)| t >= start && t <= now_ms);
        let first = *it.next()?;
        let last = it.last().copied()?;
        Some((first, last))
    }

    /// Bytes/sec over the short window; 0 if fewer than 2 points.
    pub fn rate(&self, key: &AppKey, now_ms: u64) -> f64 {
        match self.span(key, now_ms, self.short_ms) {
            Some(((t0, b0), (t1, b1))) if t1 > t0 => {
                (b1 as f64 - b0 as f64) / ((t1 - t0) as f64 / 1000.0)
            }
            _ => 0.0,
        }
    }

    pub fn recently_growing(&self, key: &AppKey, now_ms: u64) -> bool {
        match self.span(key, now_ms, self.recent_ms) {
            Some(((t0, b0), (t1, b1))) if t1 > t0 => {
                let growth = b1.saturating_sub(b0);
                growth > 0
                    && (growth >= self.min_bytes || growth as f64 >= self.min_ratio * b0 as f64)
            }
            _ => false,
        }
    }
}

pub struct LeakWatch {
    window_ms: u64,
    min_bytes: u64,
    points: HashMap<String, VecDeque<(u64, u64)>>,
}

impl LeakWatch {
    pub fn new(g: &Growth) -> Self {
        LeakWatch {
            window_ms: g.leak_window_secs * 1000,
            min_bytes: g.leak_min_bytes,
            points: HashMap::new(),
        }
    }

    pub fn record(&mut self, t_ms: u64, totals: &[(String, u64)]) {
        let cutoff = t_ms.saturating_sub(self.window_ms);
        self.points
            .retain(|k, _| totals.iter().any(|(n, _)| n == k));
        for (name, bytes) in totals {
            let ring = self.points.entry(name.clone()).or_default();
            ring.push_back((t_ms, *bytes));
            // Keep one point at-or-before the cutoff so the window stays fully covered.
            while ring.get(1).is_some_and(|&(t, _)| t <= cutoff) {
                ring.pop_front();
            }
        }
    }

    pub fn suspects(&self, now_ms: u64) -> Vec<(String, u64)> {
        let quarter_start = now_ms.saturating_sub(self.window_ms / 4);
        let mut out = Vec::new();
        for (name, ring) in &self.points {
            let (Some(&(t0, b0)), Some(&(_, b1))) = (ring.front(), ring.back()) else {
                continue;
            };
            if now_ms.saturating_sub(t0) < self.window_ms {
                continue;
            }
            let growth = b1.saturating_sub(b0);
            if growth < self.min_bytes {
                continue;
            }
            let bq = ring
                .iter()
                .take_while(|&&(t, _)| t <= quarter_start)
                .last()
                .map_or(b0, |&(_, b)| b);
            let recent = b1.saturating_sub(bq);
            if recent as f64 >= 0.10 * growth as f64 {
                out.push((name.clone(), growth));
            }
        }
        out.sort();
        out
    }
}

/// Decides the popup's "swap is filling up" card from swap-used samples.
/// On at >= 50% used with >= 1 GiB of growth over the last 15 minutes; off
/// below 40% used, or once that growth is gone. No swap means never on.
#[derive(Default)]
pub struct SwapWatch {
    points: VecDeque<(u64, u64)>,
    on: bool,
}

impl SwapWatch {
    const WINDOW_MS: u64 = 15 * 60 * 1000;
    const GROWTH: u64 = 1 << 30;

    /// Feed one sample; returns whether the warning is on now.
    pub fn record(&mut self, t_ms: u64, used: u64, total: u64) -> bool {
        let cutoff = t_ms.saturating_sub(Self::WINDOW_MS);
        while self.points.front().is_some_and(|&(t, _)| t < cutoff) {
            self.points.pop_front();
        }
        let oldest = self.points.front().map_or(used, |&(_, u)| u);
        self.points.push_back((t_ms, used));
        let grew = used.saturating_sub(oldest) >= Self::GROWTH;
        self.on = if total == 0 {
            false
        } else if self.on {
            used.saturating_mul(100) >= total.saturating_mul(40) && grew
        } else {
            used.saturating_mul(2) >= total && grew
        };
        self.on
    }

    pub fn is_on(&self) -> bool {
        self.on
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::TargetKind;

    const GIB: u64 = 1 << 30;

    fn key() -> AppKey {
        AppKey {
            cgroup: "/app.slice/x".into(),
            root_pid: Some(42),
        }
    }

    fn app(bytes: u64) -> AppSample {
        AppSample {
            key: key(),
            name: "x".into(),
            exe: None,
            kind: TargetKind::Cgroup,
            bytes,
            ram: bytes,
            names: vec!["x".into()],
            exe_names: vec![],
        }
    }

    #[test]
    fn stable_app_not_recently_growing() {
        let mut h = History::new(&Growth::default());
        for s in 0..=300u64 {
            h.record(s * 1000, &[app(20 * GIB)]);
        }
        assert!(!h.recently_growing(&key(), 300_000));
        assert_eq!(h.rate(&key(), 300_000), 0.0);
    }

    #[test]
    fn fast_growth_is_recent_and_rate() {
        let mut h = History::new(&Growth::default());
        for s in 0..=10u64 {
            h.record(s * 1000, &[app(10 * GIB + s * GIB / 10)]);
        }
        assert!(h.recently_growing(&key(), 10_000));
        let r = h.rate(&key(), 10_000);
        assert!((r - 107_374_182.0).abs() / 107_374_182.0 < 0.01, "rate {r}");
    }

    #[test]
    fn ratio_growth_counts() {
        let mut h = History::new(&Growth::default());
        for s in 0..=300u64 {
            h.record(s * 1000, &[app(GIB + s * (GIB / 4) / 300)]);
        }
        assert!(h.recently_growing(&key(), 300_000));
    }

    #[test]
    fn leak_requires_full_window_and_no_plateau() {
        let g = Growth::default();
        let name = "/app.slice/leaky".to_string();
        let total = 5 * GIB / 2;

        let mut w = LeakWatch::new(&g);
        for m in 0..=120u64 {
            w.record(m * 60_000, &[(name.clone(), 10 * GIB + m * total / 120)]);
        }
        let s = w.suspects(7_200_000);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].0, name);
        assert_eq!(s[0].1, total);

        let mut w = LeakWatch::new(&g);
        for m in 0..=120u64 {
            let grown = m.min(60) * total / 60;
            w.record(m * 60_000, &[(name.clone(), 10 * GIB + grown)]);
        }
        assert!(w.suspects(7_200_000).is_empty());

        let mut w = LeakWatch::new(&g);
        for m in 0..=60u64 {
            w.record(m * 60_000, &[(name.clone(), 10 * GIB + m * total / 60)]);
        }
        assert!(w.suspects(3_600_000).is_empty());
    }

    #[test]
    fn swap_warning_needs_half_full_and_a_gib_of_growth() {
        let mut w = SwapWatch::default();
        let min = 60_000;
        assert!(!w.record(0, 4 * GIB, 16 * GIB)); // half used but no growth yet
        assert!(!w.record(min, 4 * GIB + GIB / 2, 16 * GIB));
        assert!(!w.record(2 * min, 7 * GIB, 16 * GIB)); // grew 3 GiB, under 50%
        assert!(w.record(3 * min, 8 * GIB, 16 * GIB));
        assert!(w.is_on());
    }

    #[test]
    fn swap_warning_has_hysteresis_and_clears_without_growth() {
        let mut w = SwapWatch::default();
        let min = 60_000;
        w.record(0, 6 * GIB, 16 * GIB);
        assert!(w.record(min, 8 * GIB, 16 * GIB));
        // Dips under 50% but stays over 40%, still growing: stays on.
        assert!(w.record(2 * min, 7 * GIB, 16 * GIB));
        // Below 40%: off.
        assert!(!w.record(3 * min, 6 * GIB, 16 * GIB));
        // Back over 50% with growth from the 6 GiB point: on again.
        assert!(w.record(4 * min, 8 * GIB, 16 * GIB));
        // Plateau: after 15 minutes with no further growth it clears.
        for i in 5..=20 {
            w.record(i * min, 8 * GIB, 16 * GIB);
        }
        assert!(!w.is_on());
    }

    #[test]
    fn swap_warning_never_on_without_swap() {
        let mut w = SwapWatch::default();
        assert!(!w.record(0, 0, 0));
        assert!(!w.record(60_000, 5 * GIB, 0));
    }
}
