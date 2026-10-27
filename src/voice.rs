use crate::engine::{Action, KillReason, Level};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub title: String,
    pub body: String,
}

const DRY: &str = "[dry run] ";

fn msg(dry: bool, title: String, body: String) -> Message {
    let title = if dry { format!("{DRY}{title}") } else { title };
    Message { title, body }
}

/// Decimal units: 1 GB = 1e9, 1 MB = 1e6. GB >= 10 shows no decimal, else one.
pub fn fmt_bytes(b: u64) -> String {
    let gb = b as f64 / 1e9;
    if gb >= 9.95 {
        format!("{gb:.0} GB")
    } else if gb >= 0.95 {
        format!("{gb:.1} GB")
    } else {
        format!("{:.0} MB", b as f64 / 1e6)
    }
}

pub fn throttled(app: &str, rate_bytes_per_min: u64, dry: bool) -> Message {
    let rate = fmt_bytes(rate_bytes_per_min);
    let body = if dry {
        format!("It's gobbling memory fast (+{rate}/min). I would have slowed it down a bit \u{2014} dry run, so I left it alone.")
    } else {
        format!("It's gobbling memory fast (+{rate}/min), so I've slowed it down a bit. Everything else should stay smooth.")
    };
    msg(
        dry,
        format!("olly is keeping an eye on {app} \u{1F916}"),
        body,
    )
}

pub fn frozen(app: &str, grace_secs: u64, dry: bool) -> Message {
    let body = if dry {
        "Memory is almost full. I would have frozen it so your system doesn't lock up \u{2014} dry run, so it's still running.".to_string()
    } else {
        format!("Memory is almost full. I froze it so your system doesn't lock up. Kill it or resume? I'll close it in {grace_secs}s if things keep getting worse.")
    };
    msg(dry, format!("olly paused {app}"), body)
}

pub fn still_frozen(app: &str) -> Message {
    Message {
        title: format!("{app} is still paused"),
        body: "Kill it or resume? If I don't hear back I'll close it in a few minutes \u{2014} resuming would likely restart the leak.".to_string(),
    }
}

pub fn killed(app: &str, bytes: u64, reason: KillReason, dry: bool) -> Message {
    let b = fmt_bytes(bytes);
    let title = match reason {
        KillReason::User => format!("Closed {app}"),
        _ => format!("olly closed {app}"),
    };
    let body = if dry {
        format!("I would have closed it (it's using {b}) \u{2014} dry run, so it's still running.")
    } else {
        match reason {
            KillReason::Emergency | KillReason::GraceExpired => format!("It was using {b} and climbing, and your system was about to freeze. Your desktop is safe."),
            KillReason::NoAnswer => format!("Nobody answered, and resuming would likely restart the leak, so I closed it. It was using {b}."),
            KillReason::LastResort => format!("Your system was about to freeze and nothing was clearly running away, so I closed the biggest app ({b}) as a last resort. Sorry if that one was innocent \u{2014} I've noted it."),
            KillReason::User => format!("Done \u{2014} freed up {b}."),
        }
    };
    msg(dry, title, body)
}

/// Emergency, after the last-resort cap: no more size-based kills this incident.
pub fn last_resort_cap() -> Message {
    Message {
        title: "olly has done what it safely can".to_string(),
        body: "Memory is still very tight, but nothing is clearly running away and I've already closed two apps, so I won't guess again. Save your work and close something you don't need if you can.".to_string(),
    }
}

pub fn resumed(app: &str) -> Message {
    Message {
        title: format!("{app} is running again"),
        body: "I'll keep a close eye on it.".to_string(),
    }
}

pub fn unthrottled(app: &str) -> Message {
    Message {
        title: format!("olly eased off {app}"),
        body: format!("Memory's calm again, so {app} is back to full speed."),
    }
}

pub fn all_clear() -> Message {
    Message {
        title: "All clear! \u{2728}".to_string(),
        body: "Memory's back to comfy. I've saved notes on what happened: olly incidents"
            .to_string(),
    }
}

pub fn hunch(summary: &str) -> Message {
    Message {
        title: "olly has a hunch \u{1F50D}".to_string(),
        body: clip(summary, 200),
    }
}

/// At most `max` chars, cut at a word boundary with an ellipsis when it had to be shortened.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max - 1).collect();
    let cut = head
        .rfind(char::is_whitespace)
        .filter(|&i| i > 0)
        .map_or(head.as_str(), |i| &head[..i]);
    format!("{}\u{2026}", cut.trim_end())
}

/// An action the actuator could not carry out (the target is usually already gone).
pub fn act_failed(action: &Action) -> Message {
    let (verb, t) = match action {
        Action::Throttle { target, .. } => ("slow down", target),
        Action::Unthrottle { target } => ("ease off", target),
        Action::Freeze { target } => ("pause", target),
        Action::Resume { target } => ("resume", target),
        Action::Kill { target, .. } => ("close", target),
    };
    Message {
        title: format!("olly couldn't {verb} {}", t.name),
        body: format!(
            "olly tried to {verb} {} but couldn't \u{2014} it may have already closed.",
            t.name
        ),
    }
}

/// Startup notice when config.toml was unreadable or invalid.
pub fn config_fallback() -> Message {
    Message {
        title: "olly couldn't read its config \u{1F916}".to_string(),
        body: "I'm running on my default settings, and that means live, not a dry run: I'll really act if memory runs out. Check the config file and restart olly to change that.".to_string(),
    }
}

/// Incident summary tail when olly was stopped before the incident ended.
pub fn stopped_mid_incident(secs: u64) -> String {
    format!("olly was stopped {secs}s in, before memory recovered.")
}

/// `paused_remaining_ms` is the time left on a pause, relative to now.
pub fn status_line(level: Level, avail_bytes: u64, paused_remaining_ms: Option<u64>) -> String {
    let avail = fmt_bytes(avail_bytes);
    let mut s = match level {
        Level::Calm => format!(
            "\u{1F916} olly is watching. All calm \u{2014} {avail} free, nothing suspicious."
        ),
        _ => {
            let name = format!("{level:?}").to_lowercase();
            format!("\u{1F916} olly is on alert ({name}) \u{2014} {avail} free.")
        }
    };
    if let Some(ms) = paused_remaining_ms {
        s.push_str(&format!(" Paused for {} more min.", ms.div_ceil(60_000)));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_messages() -> Vec<Message> {
        let mut v = vec![
            throttled("Firefox", 500_000_000, false),
            throttled("Firefox", 500_000_000, true),
            frozen("Firefox", 60, false),
            frozen("Firefox", 60, true),
            still_frozen("Firefox"),
            resumed("Firefox"),
            unthrottled("Firefox"),
            all_clear(),
            hunch("something odd"),
            config_fallback(),
            last_resort_cap(),
        ];
        for r in [
            KillReason::Emergency,
            KillReason::GraceExpired,
            KillReason::NoAnswer,
            KillReason::LastResort,
            KillReason::User,
        ] {
            v.push(killed("Firefox", 21_000_000_000, r, false));
            v.push(killed("Firefox", 21_000_000_000, r, true));
        }
        v
    }

    #[test]
    fn fmt_bytes_cases() {
        assert_eq!(fmt_bytes(21_000_000_000), "21 GB");
        assert_eq!(fmt_bytes(1_200_000_000), "1.2 GB");
        assert_eq!(fmt_bytes(512_000_000), "512 MB");
        assert_eq!(fmt_bytes(0), "0 MB");
        assert_eq!(fmt_bytes(10_000_000_000), "10 GB");
    }

    #[test]
    fn frozen_copy_exact() {
        let m = frozen("Firefox", 90, false);
        assert_eq!(m.title, "olly paused Firefox");
        assert_eq!(
            m.body,
            "Memory is almost full. I froze it so your system doesn't lock up. Kill it or resume? I'll close it in 90s if things keep getting worse."
        );
    }

    #[test]
    fn killed_last_resort_apologises() {
        let m = killed("Firefox", 4_000_000_000, KillReason::LastResort, false);
        assert!(m.body.contains("last resort"));
        assert!(m.body.contains("4.0 GB") || m.body.contains("4 GB"));
    }

    #[test]
    fn dry_run_prefix() {
        let m = throttled("Firefox", 500_000_000, true);
        assert_eq!(m.title, "[dry run] olly is keeping an eye on Firefox 🤖");
        assert_eq!(
            m.body,
            "It's gobbling memory fast (+500 MB/min). I would have slowed it down a bit — dry run, so I left it alone."
        );
        let f = frozen("Firefox", 60, true);
        assert!(f.title.starts_with("[dry run] "));
        assert_eq!(
            f.body,
            "Memory is almost full. I would have frozen it so your system doesn't lock up — dry run, so it's still running."
        );
        let k = killed("Firefox", 2_000_000_000, KillReason::User, true);
        assert!(k.title.starts_with("[dry run] "));
        assert_eq!(
            k.body,
            "I would have closed it (it's using 2.0 GB) — dry run, so it's still running."
        );
        assert!(!throttled("Firefox", 1, false)
            .title
            .starts_with("[dry run]"));
    }

    #[test]
    fn no_emoji_in_critical_titles() {
        for dry in [false, true] {
            let mut titles = vec![frozen("X", 60, dry).title];
            for r in [
                KillReason::Emergency,
                KillReason::GraceExpired,
                KillReason::NoAnswer,
                KillReason::LastResort,
                KillReason::User,
            ] {
                titles.push(killed("X", 1, r, dry).title);
            }
            for t in titles {
                assert!(t.chars().all(|c| (c as u32) < 0x1F000), "{t}");
            }
        }
    }

    #[test]
    fn olly_always_lowercase() {
        for m in all_messages() {
            assert!(!m.title.contains("Olly") && !m.body.contains("Olly"));
        }
        assert!(!status_line(Level::Calm, 1, None).contains("Olly"));
    }

    #[test]
    fn other_copy_exact() {
        assert_eq!(still_frozen("X").title, "X is still paused");
        assert_eq!(resumed("X").title, "X is running again");
        assert_eq!(resumed("X").body, "I'll keep a close eye on it.");
        assert_eq!(all_clear().title, "All clear! ✨");
        assert_eq!(unthrottled("X").title, "olly eased off X");
        assert_eq!(
            unthrottled("X").body,
            "Memory's calm again, so X is back to full speed."
        );
        let u = killed("X", 3_000_000_000, KillReason::User, false);
        assert_eq!(u.title, "Closed X");
        assert_eq!(u.body, "Done — freed up 3.0 GB.");
        let k = killed("X", 3_000_000_000, KillReason::Emergency, false);
        assert_eq!(k.title, "olly closed X");
        let n = killed("X", 3_000_000_000, KillReason::NoAnswer, false);
        assert!(n.body.starts_with("Nobody answered"));
    }

    #[test]
    fn hunch_truncates() {
        let long = "a".repeat(300);
        assert_eq!(hunch(&long).body.chars().count(), 200);
        let words = "word ".repeat(60);
        let b = hunch(&words).body;
        assert!(
            b.chars().count() <= 200 && b.ends_with("word\u{2026}"),
            "{b}"
        );
        assert_eq!(hunch("short").body, "short");
        assert_eq!(hunch("short").title, "olly has a hunch 🔍");
    }

    #[test]
    fn status_lines() {
        assert_eq!(
            status_line(Level::Calm, 21_000_000_000, None),
            "🤖 olly is watching. All calm — 21 GB free, nothing suspicious."
        );
        assert_eq!(
            status_line(Level::Critical, 1_200_000_000, None),
            "🤖 olly is on alert (critical) — 1.2 GB free."
        );
        assert_eq!(
            status_line(Level::Calm, 21_000_000_000, Some(61_000)),
            "🤖 olly is watching. All calm — 21 GB free, nothing suspicious. Paused for 2 more min."
        );
    }

    #[test]
    fn act_failed_copy() {
        let t = crate::engine::Target {
            id: 1,
            key: crate::sample::AppKey {
                cgroup: "/c".into(),
                root_pid: None,
            },
            name: "Firefox".into(),
            kind: crate::sample::TargetKind::Cgroup,
            bytes: 1,
        };
        let m = act_failed(&Action::Freeze { target: t.clone() });
        assert_eq!(
            m.body,
            "olly tried to pause Firefox but couldn't \u{2014} it may have already closed."
        );
        assert_eq!(m.title, "olly couldn't pause Firefox");
        let k = act_failed(&Action::Kill {
            target: t.clone(),
            reason: KillReason::Emergency,
        });
        assert!(k.body.starts_with("olly tried to close Firefox"));
        assert!(act_failed(&Action::Throttle { target: t, high: 1 })
            .body
            .contains("slow down"));
    }

    #[test]
    fn config_fallback_says_live_defaults() {
        let m = config_fallback();
        assert!(m.title.contains("config"));
        assert!(m.body.contains("default"));
        assert!(m.body.contains("not a dry run"));
    }
}
