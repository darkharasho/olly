use crate::sample::AppSample;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
struct State {
    added: Vec<String>,
}

pub struct Allowlist {
    base: Vec<String>,
    added: Vec<String>,
    path: PathBuf,
}

impl Allowlist {
    pub fn load(base: Vec<String>, state_path: PathBuf) -> Self {
        let added = std::fs::read_to_string(&state_path)
            .ok()
            .and_then(|t| toml::from_str::<State>(&t).ok())
            .map(|s| s.added)
            .unwrap_or_default();
        Allowlist { base, added, path: state_path }
    }

    /// Exact match, or glob with a trailing '*' only. Case-sensitive.
    pub fn matches(&self, name: &str) -> bool {
        self.base.iter().chain(&self.added).any(|pat| match pat.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => pat == name,
        })
    }

    /// True if ANY member name matches, i.e. the app is protected.
    pub fn allows_app(&self, app: &AppSample) -> bool {
        app.names.iter().any(|n| self.matches(n))
    }

    pub fn add(&mut self, name: &str) -> io::Result<()> {
        if self.base.iter().chain(&self.added).any(|n| n == name) {
            return Ok(());
        }
        let mut added = self.added.clone();
        added.push(name.to_string());
        let text = toml::to_string(&State { added: added.clone() })
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &self.path)?;
        self.added = added;
        Ok(())
    }

    pub fn list(&self) -> Vec<String> {
        self.base.iter().chain(&self.added).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::{AppKey, TargetKind};

    fn al() -> (Allowlist, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        let a = Allowlist::load(
            vec!["systemd".into(), "systemd-*".into(), "kwin_wayland".into()],
            d.path().join("state.toml"),
        );
        (a, d)
    }

    #[test]
    fn allowlist_glob_and_exact() {
        let d = tempfile::tempdir().unwrap();
        let a = Allowlist::load(vec!["systemd-*".into()], d.path().join("s.toml"));
        assert!(a.matches("systemd-journald"));
        assert!(!a.matches("systemd"));
        assert!(!a.matches("steam"));
        assert!(!a.matches("Systemd-journald"));
        let (a, _d) = al();
        assert!(a.matches("systemd"));
        assert!(!a.matches("steam"));
    }

    #[test]
    fn allows_app_if_any_member_protected() {
        let (a, _d) = al();
        let app = AppSample {
            key: AppKey { cgroup: "/c".into(), root_pid: None },
            name: "session".into(),
            exe: None,
            kind: TargetKind::Cgroup,
            bytes: 1,
            names: vec!["bash".into(), "kwin_wayland".into()],
        };
        assert!(a.allows_app(&app));
        let mut other = app.clone();
        other.names = vec!["bash".into()];
        assert!(!a.allows_app(&other));
    }

    #[test]
    fn add_persists() {
        let (mut a, d) = al();
        let p = d.path().join("state.toml");
        a.add("discord").unwrap();
        a.add("discord").unwrap();
        let b = Allowlist::load(vec![], p.clone());
        assert!(b.matches("discord"));
        assert_eq!(b.list(), vec!["discord".to_string()]);
        assert_eq!(a.list().last().unwrap(), "discord");
        std::fs::write(&p, "not [valid").unwrap();
        assert!(!Allowlist::load(vec![], p).matches("discord"));
    }
}
