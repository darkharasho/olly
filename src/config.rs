use serde::{Deserialize, Serialize};
use std::path::Path;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub dry_run: bool,
    pub desktop_uid: u32,
    pub thresholds: Thresholds,
    pub timing: Timing,
    pub growth: Growth,
    pub allowlist: Vec<String>,
    pub claude: ClaudeConfig,
    pub max_pause_secs: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Thresholds {
    pub watch_some: f32,
    pub calm_some: f32,
    pub warning_full: f32,
    pub critical_full: f32,
    pub emergency_full: f32,
    pub watch_avail: f32,
    pub warning_avail: f32,
    pub critical_avail: f32,
    pub emergency_avail: f32,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Timing {
    pub active_sample_ms: u64,
    pub slow_sample_secs: u64,
    pub calm_end_secs: u64,
    pub freeze_grace_secs: u64,
    pub freeze_remind_secs: u64,
    pub freeze_giveup_secs: u64,
    pub action_cooldown_secs: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Growth {
    pub short_window_secs: u64,
    pub recent_window_secs: u64,
    pub recent_min_bytes: u64,
    pub recent_min_ratio: f32,
    pub leak_window_secs: u64,
    pub leak_min_bytes: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct ClaudeConfig {
    pub enabled: bool,
    pub max_per_day: u32,
    pub recovered_avail: f32,
    pub timeout_secs: u64,
    pub claude_path: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            dry_run: false,
            desktop_uid: 1000,
            thresholds: Thresholds::default(),
            timing: Timing::default(),
            growth: Growth::default(),
            allowlist: [
                "olly",
                "olly-notify",
                "systemd",
                "systemd-*",
                "dbus-broker",
                "dbus-broker-launch",
                "dbus-daemon",
                "kwin_wayland",
                "kwin_wayland_wrapper",
                "plasmashell",
                "ksmserver",
                "kded6",
                "Xwayland",
                "xdg-desktop-portal*",
                "pipewire",
                "pipewire-pulse",
                "wireplumber",
                "sddm",
                "NetworkManager",
                "sshd",
                "polkitd",
                "gamescope",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            claude: ClaudeConfig::default(),
            max_pause_secs: 3600,
        }
    }
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            watch_some: 10.0,
            calm_some: 5.0,
            warning_full: 10.0,
            critical_full: 25.0,
            emergency_full: 50.0,
            watch_avail: 15.0,
            warning_avail: 10.0,
            critical_avail: 5.0,
            emergency_avail: 2.0,
        }
    }
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            active_sample_ms: 1000,
            slow_sample_secs: 60,
            calm_end_secs: 60,
            freeze_grace_secs: 10,
            freeze_remind_secs: 60,
            freeze_giveup_secs: 300,
            action_cooldown_secs: 5,
        }
    }
}

impl Default for Growth {
    fn default() -> Self {
        Growth {
            short_window_secs: 10,
            recent_window_secs: 300,
            recent_min_bytes: 500 * MIB,
            recent_min_ratio: 0.20,
            leak_window_secs: 7200,
            leak_min_bytes: 2 * GIB,
        }
    }
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        ClaudeConfig {
            enabled: true,
            max_per_day: 3,
            recovered_avail: 25.0,
            timeout_secs: 180,
            claude_path: None,
        }
    }
}

/// Load config from `path`. A missing file yields defaults with no error;
/// an unreadable or invalid file yields defaults plus an error message.
pub fn load(path: &Path) -> (Config, Option<String>) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Config::default(), None),
        Err(e) => {
            return (
                Config::default(),
                Some(format!("cannot read {}: {}", path.display(), e)),
            )
        }
    };
    match toml::from_str::<Config>(&text) {
        Ok(c) => (c, None),
        Err(e) => (
            Config::default(),
            Some(format!("invalid config {}: {}", path.display(), e)),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn defaults_match_spec() {
        let c = Config::default();
        assert_eq!(c.thresholds.critical_full, 25.0);
        assert_eq!(c.timing.freeze_grace_secs, 10);
        assert_eq!(c.timing.freeze_giveup_secs, 300);
        assert_eq!(c.max_pause_secs, 3600);
        assert!(c.allowlist.iter().any(|s| s == "kwin_wayland"));
        assert!(!c.allowlist.iter().any(|s| s == "steam"));
    }

    #[test]
    fn partial_file_keeps_other_defaults() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("c.toml");
        std::fs::write(&p, "dry_run = true\n[timing]\nfreeze_grace_secs = 20").unwrap();
        let (c, err) = load(&p);
        assert!(err.is_none());
        assert!(c.dry_run);
        assert_eq!(c.timing.freeze_grace_secs, 20);
        assert_eq!(c.timing.freeze_remind_secs, 60);
    }

    #[test]
    fn missing_file_is_defaults_without_error() {
        assert_eq!(
            load(Path::new("/nonexistent/olly.toml")),
            (Config::default(), None)
        );
    }

    #[test]
    fn invalid_config_falls_back_to_defaults() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("c.toml");
        std::fs::write(&p, "dry_run = [[").unwrap();
        let (c, err) = load(&p);
        assert_eq!(c, Config::default());
        assert!(err.unwrap().contains(p.to_str().unwrap()));
    }
}
