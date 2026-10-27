use crate::sample::AppSample;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
struct State {
    added: Vec<String>,
}

pub struct Allowlist {
    base: Vec<String>,
    added: Vec<String>,
    path: PathBuf,
}

/// The names added over IPC. A missing file is an empty list.
fn read_added(path: &Path) -> Result<Vec<String>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    toml::from_str::<State>(&text)
        .map(|s| s.added)
        .map_err(|e| format!("invalid allowlist {}: {e}", path.display()))
}

impl Allowlist {
    pub fn load(base: Vec<String>, state_path: PathBuf) -> Self {
        let added = read_added(&state_path).unwrap_or_else(|e| {
            let one_line: Vec<&str> = e.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
            eprintln!(
                "olly: ERROR: {}; ignoring the names added with `olly allow`",
                one_line.join(" ")
            );
            Vec::new()
        });
        Allowlist {
            base,
            added,
            path: state_path,
        }
    }

    /// Exact match, or glob with a trailing '*' only. Case-sensitive.
    pub fn matches(&self, name: &str) -> bool {
        self.base
            .iter()
            .chain(&self.added)
            .any(|pat| match pat.strip_suffix('*') {
                Some(prefix) => name.starts_with(prefix),
                None => pat == name,
            })
    }

    /// True if ANY member's comm or exe basename matches, i.e. the app is protected.
    /// comm is cut at 15 chars, so long names only match through the exe.
    pub fn allows_app(&self, app: &AppSample) -> bool {
        app.names
            .iter()
            .chain(&app.exe_names)
            .any(|n| self.matches(n))
    }

    pub fn add(&mut self, name: &str) -> io::Result<()> {
        if self.base.iter().chain(&self.added).any(|n| n == name) {
            return Ok(());
        }
        let mut added = self.added.clone();
        added.push(name.to_string());
        let text = toml::to_string(&State {
            added: added.clone(),
        })
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
            key: AppKey {
                cgroup: "/c".into(),
                root_pid: None,
            },
            name: "session".into(),
            exe: None,
            kind: TargetKind::Cgroup,
            bytes: 1,
            ram: 1,
            names: vec!["bash".into(), "kwin_wayland".into()],
            exe_names: vec![],
            is_game: false,
        };
        assert!(a.allows_app(&app));
        let mut other = app.clone();
        other.names = vec!["bash".into()];
        assert!(!a.allows_app(&other));
    }

    fn app_with(comm: &str, exe: &str) -> AppSample {
        AppSample {
            key: AppKey {
                cgroup: "/c".into(),
                root_pid: None,
            },
            name: comm.into(),
            exe: Some(PathBuf::from(exe)),
            kind: TargetKind::Cgroup,
            bytes: 1,
            ram: 1,
            names: vec![comm.into()],
            exe_names: vec![PathBuf::from(exe)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()],
            is_game: false,
        }
    }

    #[test]
    fn long_name_matches_exe_basename() {
        let d = tempfile::tempdir().unwrap();
        let mut a = Allowlist::load(vec![], d.path().join("s.toml"));
        // comm is cut at 15 chars; `olly allow` takes the full name.
        let app = app_with("my-long-game-na", "/opt/game/my-long-game-name");
        assert!(!a.allows_app(&app));
        a.add("my-long-game-name").unwrap();
        assert!(a.allows_app(&app));
        // Globs apply to the exe basename too.
        let g = Allowlist::load(vec!["my-long-*".into()], d.path().join("g.toml"));
        assert!(g.allows_app(&app_with("x", "/opt/my-long-thing")));
        assert!(!g.allows_app(&app_with("x", "/opt/other")));
    }

    #[test]
    fn kwin_wrapper_protected() {
        let d = tempfile::tempdir().unwrap();
        let a = Allowlist::load(
            crate::config::Config::default().allowlist,
            d.path().join("s.toml"),
        );
        assert!(a.allows_app(&app_with(
            "kwin_wayland_wr",
            "/usr/bin/kwin_wayland_wrapper"
        )));
        assert!(a.allows_app(&app_with("dbus-broker-lau", "/usr/bin/dbus-broker-launch")));
        assert!(!a.allows_app(&app_with("firefox", "/usr/lib/firefox/firefox")));
    }

    #[test]
    fn unreadable_or_invalid_state_is_reported() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.toml");
        assert_eq!(read_added(&p), Ok(vec![]));
        std::fs::write(&p, "not [valid").unwrap();
        let e = read_added(&p).unwrap_err();
        assert!(
            e.contains("invalid") && e.contains(p.to_str().unwrap()),
            "{e}"
        );
        let e = read_added(d.path()).unwrap_err();
        assert!(e.contains("cannot read"), "{e}");
        std::fs::write(&p, "added = [\"obs\"]\n").unwrap();
        assert_eq!(read_added(&p), Ok(vec!["obs".to_string()]));
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
