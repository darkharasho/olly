//! Desktop notification agent: subscribes to olly's event stream and shows
//! freedesktop notifications. Runs as the desktop user; never needed by the daemon.

use notify_rust::{Notification, Urgency};
use olly::engine::{Action, Event};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const RETRY: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq)]
enum Urg {
    Normal,
    Critical,
}

/// What to show for one event.
#[derive(Debug, PartialEq)]
struct Note {
    summary: String,
    body: String,
    urgency: Urg,
    /// Offer Kill and Resume for this frozen target.
    frozen_id: Option<u64>,
    /// Offer "Never touch this" for this app, by the name the allowlist matches.
    /// `None` for dry runs and for events that are not about one app.
    app: Option<String>,
}

/// The name `olly allow` takes for a target: its process name.
fn allow_name(action: &Action) -> Option<&str> {
    match action {
        Action::Throttle { target, .. }
        | Action::Freeze { target }
        | Action::Kill { target, .. } => Some(&target.name),
        Action::Unthrottle { .. } | Action::Resume { .. } => None,
    }
}

/// Maps an event to a notification. `None` for events that are not shown.
/// `frozen_id` doubles as the replace key: one notification per frozen target.
fn map_event(ev: &Event) -> Option<Note> {
    match ev {
        Event::Acted {
            action,
            message,
            dry_run,
        } => {
            // Dry-run freezes are tracked by the engine too, so they keep Kill/Resume.
            let frozen_id = match action {
                Action::Freeze { target } => Some(target.id),
                _ => None,
            };
            let urgent = match action {
                Action::Freeze { .. } => true,
                Action::Kill { .. } => !*dry_run,
                _ => false,
            };
            Some(Note {
                summary: message.title.clone(),
                body: message.body.clone(),
                urgency: if urgent { Urg::Critical } else { Urg::Normal },
                frozen_id,
                app: allow_name(action).filter(|_| !*dry_run).map(str::to_string),
            })
        }
        Event::Notice { message, frozen_id } => Some(Note {
            summary: message.title.clone(),
            body: message.body.clone(),
            urgency: if frozen_id.is_some() {
                Urg::Critical
            } else {
                Urg::Normal
            },
            frozen_id: *frozen_id,
            app: None,
        }),
        // An update is announced by the widget and the CLI; the desktop stays quiet.
        Event::Updated { .. } => None,
        _ => None,
    }
}

/// Where the shared (system) copies and the per-user copies live.
struct UserPaths {
    share: PathBuf,
    plasmoid_installed: PathBuf,
    user_unit: PathBuf,
}

impl UserPaths {
    fn from_home(home: &Path) -> Self {
        Self {
            share: PathBuf::from("/usr/local/share/olly"),
            plasmoid_installed: home.join(".local/share/plasma/plasmoids/org.olly.tray"),
            user_unit: home.join(".config/systemd/user/olly-notify.service"),
        }
    }
}

/// Runs an external program; true when it exited successfully.
trait Run {
    fn run(&mut self, prog: &str, args: &[&str]) -> bool;
}

struct RealRun;

impl Run for RealRun {
    fn run(&mut self, prog: &str, args: &[&str]) -> bool {
        std::process::Command::new(prog)
            .args(args)
            .status()
            .is_ok_and(|s| s.success())
    }
}

#[derive(Debug, Default, PartialEq)]
struct SyncOutcome {
    widget_upgraded: bool,
    unit_updated: bool,
}

/// `KPlugin.Version` from a plasmoid directory's `metadata.json`.
fn widget_version(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("metadata.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(v.get("KPlugin")?.get("Version")?.as_str()?.to_string())
}

/// Brings the user's widget and user unit up to date with the shared copies.
/// Each failure is logged once and skipped; nothing here retries. A changed unit only
/// applies once olly-notify restarts, so the widget waits for that restart: its
/// notification needs a process that stays up to answer it.
fn sync_user_side(p: &UserPaths, run: &mut dyn Run) -> SyncOutcome {
    let mut out = SyncOutcome::default();

    let shared_unit = p.share.join("olly-notify.service");
    if let Ok(new) = std::fs::read(&shared_unit) {
        if std::fs::read(&p.user_unit).ok().as_deref() != Some(new.as_slice()) {
            let copied = p
                .user_unit
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(&p.user_unit, &new));
            match copied {
                Ok(()) => {
                    out.unit_updated = true;
                    if !run.run("systemctl", &["--user", "daemon-reload"]) {
                        eprintln!("olly-notify: systemctl --user daemon-reload failed");
                    }
                }
                Err(e) => eprintln!("olly-notify: cannot update the user unit: {e}"),
            }
        }
    }
    if out.unit_updated {
        return out;
    }

    let shared_widget = p.share.join("plasmoid");
    if p.plasmoid_installed.exists() {
        match (
            widget_version(&shared_widget),
            widget_version(&p.plasmoid_installed),
        ) {
            (Some(new), Some(old)) if new != old => {
                let dir = shared_widget.to_string_lossy().into_owned();
                if run.run(
                    "kpackagetool6",
                    &["--type", "Plasma/Applet", "--upgrade", &dir],
                ) {
                    out.widget_upgraded = true;
                } else {
                    eprintln!("olly-notify: kpackagetool6 could not upgrade the widget");
                }
            }
            (Some(_), Some(_)) => {}
            _ => eprintln!("olly-notify: cannot compare widget versions; leaving the widget"),
        }
    }
    out
}

/// True only for a successful update to a version other than ours.
fn should_restart(ev: &Event, own_version: &str) -> bool {
    matches!(ev, Event::Updated { ok: true, to, .. } if to != own_version)
}

/// Runs the sync with the real home and runner.
fn sync_now() -> SyncOutcome {
    let Some(home) = std::env::var_os("HOME") else {
        eprintln!("olly-notify: HOME is not set; skipping the user-side update");
        return SyncOutcome::default();
    };
    let o = sync_user_side(&UserPaths::from_home(Path::new(&home)), &mut RealRun);
    if o.widget_upgraded || o.unit_updated {
        eprintln!(
            "olly-notify: updated the user side (widget: {}, unit: {})",
            o.widget_upgraded, o.unit_updated
        );
    }
    o
}

/// What restarting Plasma runs. Windows stay open: they belong to KWin, not plasmashell.
const RESTART_PLASMA: [&str; 3] = ["--user", "restart", "plasma-plasmashell"];

/// Plasma keeps running the widget it loaded, or drops it, until plasmashell restarts,
/// so say so and offer to do it.
fn show_widget_upgraded() {
    let mut n = Notification::new();
    n.appname("olly")
        .icon("dialog-information")
        .summary("olly's tray face was updated")
        .body("Plasma shows the new one after it restarts. Your windows stay open.")
        .action("restart_plasma", "Restart Plasma");
    let handle = match n.show() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("olly-notify: cannot show a notification: {e}");
            return;
        }
    };
    let spawned = std::thread::Builder::new()
        .name("olly-notify-plasma".into())
        .spawn(move || {
            handle.wait_for_action(|action| {
                if action == "restart_plasma" && !RealRun.run("systemctl", &RESTART_PLASMA) {
                    eprintln!("olly-notify: couldn't restart Plasma");
                }
            });
        });
    if let Err(e) = spawned {
        eprintln!("olly-notify: cannot start the action thread: {e}");
    }
}

/// Syncs, and offers to restart Plasma when the widget changed. A changed unit exits so
/// systemd restarts olly-notify under it (`Restart=always`).
fn sync_and_tell() {
    let o = sync_now();
    if o.unit_updated {
        eprintln!("olly-notify: my unit changed; restarting under it");
        std::process::exit(0);
    }
    if o.widget_upgraded {
        show_widget_upgraded();
    }
}

#[derive(Deserialize)]
struct EventLine {
    event: Event,
}

fn socket_path() -> PathBuf {
    std::env::var_os("OLLY_SOCKET")
        .map_or_else(|| PathBuf::from("/run/olly/olly.sock"), PathBuf::from)
}

/// A command for the daemon socket.
#[derive(Debug, PartialEq)]
enum Cmd {
    Kill(u64),
    Resume(u64),
    Allow(String),
}

impl Cmd {
    fn label(&self) -> String {
        match self {
            Cmd::Kill(id) => format!("frozen_kill {id}"),
            Cmd::Resume(id) => format!("frozen_resume {id}"),
            Cmd::Allow(name) => format!("allowlist_add {name}"),
        }
    }

    /// The request line, as `Request` in ipc.rs reads it.
    fn wire(&self) -> String {
        let v = match self {
            Cmd::Kill(id) => serde_json::json!({"cmd": "frozen_kill", "id": id}),
            Cmd::Resume(id) => serde_json::json!({"cmd": "frozen_resume", "id": id}),
            Cmd::Allow(name) => serde_json::json!({"cmd": "allowlist_add", "name": name}),
        };
        format!("{v}\n")
    }
}

/// The buttons a note offers, as (action key, label).
fn buttons(frozen_id: Option<u64>, app: Option<&str>) -> Vec<(&'static str, &'static str)> {
    let mut b = Vec::new();
    if frozen_id.is_some() {
        b.push(("kill", "Kill"));
        b.push(("resume", "Resume"));
    }
    if app.is_some() {
        b.push(("allow", "Never touch this"));
    }
    b
}

/// What a click does. "Never touch this" allowlists the app, then thaws it if frozen.
fn plan_click(action: &str, frozen_id: Option<u64>, app: Option<&str>) -> Vec<Cmd> {
    match (action, frozen_id, app) {
        ("kill", Some(id), _) => vec![Cmd::Kill(id)],
        ("resume", Some(id), _) => vec![Cmd::Resume(id)],
        ("allow", fid, Some(app)) => {
            let mut v = vec![Cmd::Allow(app.to_string())];
            v.extend(fid.map(Cmd::Resume));
            v
        }
        _ => Vec::new(),
    }
}

fn allowed_message(app: &str) -> String {
    format!("Got it, I'll leave {app} alone from now on.")
}

/// Runs a click's commands. Once the allowlist add succeeds, says so.
fn handle_click(action: &str, frozen_id: Option<u64>, app: Option<&str>) {
    for cmd in plan_click(action, frozen_id, app) {
        let ok = send_command(&cmd);
        if ok && matches!(cmd, Cmd::Allow(_)) {
            if let Some(app) = app {
                let shown = Notification::new()
                    .appname("olly")
                    .icon("dialog-information")
                    .summary(&allowed_message(app))
                    .show();
                if let Err(e) = shown {
                    eprintln!("olly-notify: cannot show a notification: {e}");
                }
            }
        }
    }
}

/// Sends a command over a fresh connection and logs the answer. True when the daemon said ok.
fn send_command(cmd: &Cmd) -> bool {
    let label = cmd.label();
    let line = cmd.wire();
    let result = UnixStream::connect(socket_path()).and_then(|mut s| {
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        s.write_all(line.as_bytes())?;
        let mut resp = String::new();
        BufReader::new(s).read_line(&mut resp)?;
        Ok(resp)
    });
    match result {
        Ok(r) if r.contains("\"ok\":true") => {
            eprintln!("olly-notify: {label}: done");
            true
        }
        Ok(r) => {
            eprintln!("olly-notify: {label}: {}", r.trim());
            false
        }
        Err(e) => {
            eprintln!("olly-notify: {label}: {e}");
            false
        }
    }
}

/// Notification ids and click generations per frozen target.
#[derive(Default)]
struct Shown {
    ids: HashMap<u64, u32>,
    gens: HashMap<u64, Arc<AtomicU64>>,
    /// The app name behind each frozen target, kept for its later reminders.
    names: HashMap<u64, String>,
}

fn show(note: &Note, shown: &Mutex<Shown>) {
    let mut n = Notification::new();
    n.appname("olly")
        .icon("dialog-information")
        .summary(&note.summary)
        .body(&note.body);
    n.urgency(match note.urgency {
        Urg::Normal => Urgency::Normal,
        Urg::Critical => Urgency::Critical,
    });
    let mut generation = None;
    let mut app = note.app.clone();
    if let Some(fid) = note.frozen_id {
        let mut s = shown.lock().unwrap();
        match &app {
            Some(a) => {
                s.names.insert(fid, a.clone());
            }
            // A reminder carries no name; use the one from the freeze notification.
            None => app = s.names.get(&fid).cloned(),
        }
        if let Some(&id) = s.ids.get(&fid) {
            n.id(id);
        }
        let g = s.gens.entry(fid).or_default().clone();
        generation = Some((g.fetch_add(1, Ordering::SeqCst) + 1, g));
    }
    let offered = buttons(note.frozen_id, app.as_deref());
    for (key, label) in &offered {
        n.action(key, label);
    }
    let handle = match n.show() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("olly-notify: cannot show a notification: {e}");
            return;
        }
    };
    if let (Some(fid), Some(_)) = (note.frozen_id, &generation) {
        shown.lock().unwrap().ids.insert(fid, handle.id());
    }
    if offered.is_empty() {
        return;
    }
    let frozen_id = note.frozen_id;
    // Waiting for a click blocks, so it gets its own thread. A newer notification for the
    // same target replaces this one; its clicks win and this thread's are ignored.
    let spawned = std::thread::Builder::new()
        .name("olly-notify-action".into())
        .spawn(move || {
            handle.wait_for_action(|action| {
                if generation
                    .as_ref()
                    .is_some_and(|(mine, g)| g.load(Ordering::SeqCst) != *mine)
                {
                    return;
                }
                handle_click(action, frozen_id, app.as_deref());
            });
        });
    if let Err(e) = spawned {
        eprintln!("olly-notify: cannot start the action thread: {e}");
    }
}

/// One connection's worth of events. Returns when the daemon goes away.
fn run_once(shown: &Mutex<Shown>) -> std::io::Result<()> {
    let mut s = UnixStream::connect(socket_path())?;
    s.write_all(b"{\"cmd\":\"subscribe\"}\n")?;
    let mut lines = BufReader::new(s).lines();
    match lines.next().transpose()? {
        Some(ack) if ack.contains("\"ok\":true") => {}
        Some(ack) => {
            return Err(std::io::Error::other(format!(
                "olly refused the subscription: {ack}"
            )))
        }
        None => return Err(std::io::Error::other("olly closed the connection")),
    }
    for line in lines {
        let line = line?;
        match serde_json::from_str::<EventLine>(&line) {
            Ok(EventLine { event }) => {
                if matches!(event, Event::Updated { ok: true, .. }) {
                    // The new binary syncs when it starts, and stays up to answer the
                    // notification's button.
                    if should_restart(&event, env!("CARGO_PKG_VERSION")) {
                        eprintln!("olly-notify: olly updated; restarting on the new binary");
                        std::process::exit(0);
                    }
                    sync_and_tell();
                }
                if let Some(note) = map_event(&event) {
                    show(&note, shown);
                }
            }
            Err(e) => eprintln!("olly-notify: ignoring a line I couldn't read: {e}"),
        }
    }
    Ok(())
}

fn main() {
    sync_and_tell();
    let shown = Mutex::new(Shown::default());
    let mut quiet = false;
    loop {
        match run_once(&shown) {
            Ok(()) => {
                eprintln!(
                    "olly-notify: olly went away; retrying every {}s",
                    RETRY.as_secs()
                );
                quiet = false;
            }
            Err(e) => {
                if !quiet {
                    eprintln!(
                        "olly-notify: cannot reach olly ({e}); retrying every {}s",
                        RETRY.as_secs()
                    );
                }
                quiet = true;
            }
        }
        std::thread::sleep(RETRY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use olly::engine::{KillReason, Target};
    use olly::sample::{AppKey, TargetKind};
    use olly::voice::Message;

    fn target(id: u64) -> Target {
        Target {
            id,
            key: AppKey {
                cgroup: "/app".into(),
                root_pid: None,
            },
            name: "game.exe".into(),
            kind: TargetKind::Cgroup,
            bytes: 1,
        }
    }

    fn msg() -> Message {
        Message {
            title: "t".into(),
            body: "b".into(),
        }
    }

    #[test]
    fn never_touch_offered_for_throttle_freeze_and_kill() {
        let acted = |action| Event::Acted {
            action,
            message: msg(),
            dry_run: false,
        };
        let t = Action::Throttle {
            target: target(1),
            high: 5,
        };
        let f = Action::Freeze { target: target(2) };
        let k = Action::Kill {
            target: target(3),
            reason: KillReason::Emergency,
        };
        for a in [t, f, k] {
            let n = map_event(&acted(a)).unwrap();
            assert_eq!(n.app.as_deref(), Some("game.exe"));
        }
        for a in [
            Action::Unthrottle { target: target(1) },
            Action::Resume { target: target(2) },
        ] {
            assert_eq!(map_event(&acted(a)).unwrap().app, None);
        }
    }

    #[test]
    fn dry_runs_and_notices_get_no_never_touch() {
        let ev = Event::Acted {
            action: Action::Throttle {
                target: target(1),
                high: 5,
            },
            message: msg(),
            dry_run: true,
        };
        assert_eq!(map_event(&ev).unwrap().app, None);
        let n = map_event(&Event::Notice {
            message: msg(),
            frozen_id: Some(5),
        })
        .unwrap();
        assert_eq!(n.app, None);
    }

    #[test]
    fn button_sets() {
        assert_eq!(
            buttons(Some(2), Some("x")),
            vec![
                ("kill", "Kill"),
                ("resume", "Resume"),
                ("allow", "Never touch this")
            ]
        );
        assert_eq!(
            buttons(None, Some("x")),
            vec![("allow", "Never touch this")]
        );
        assert_eq!(
            buttons(Some(2), None),
            vec![("kill", "Kill"), ("resume", "Resume")]
        );
        assert!(buttons(None, None).is_empty());
    }

    #[test]
    fn click_plans() {
        assert_eq!(plan_click("kill", Some(2), Some("x")), vec![Cmd::Kill(2)]);
        assert_eq!(
            plan_click("resume", Some(2), Some("x")),
            vec![Cmd::Resume(2)]
        );
        assert_eq!(
            plan_click("allow", Some(2), Some("x")),
            vec![Cmd::Allow("x".into()), Cmd::Resume(2)]
        );
        assert_eq!(
            plan_click("allow", None, Some("x")),
            vec![Cmd::Allow("x".into())]
        );
        assert!(plan_click("allow", Some(2), None).is_empty());
        assert!(plan_click("bogus", Some(2), Some("x")).is_empty());
    }

    #[test]
    fn wire_format_matches_ipc() {
        assert_eq!(Cmd::Kill(7).wire(), "{\"cmd\":\"frozen_kill\",\"id\":7}\n");
        assert_eq!(
            Cmd::Resume(7).wire(),
            "{\"cmd\":\"frozen_resume\",\"id\":7}\n"
        );
        let line = Cmd::Allow("we\"ird".into()).wire();
        assert_eq!(line, "{\"cmd\":\"allowlist_add\",\"name\":\"we\\\"ird\"}\n");
        assert_eq!(
            allowed_message("obs"),
            "Got it, I'll leave obs alone from now on."
        );
    }

    #[test]
    fn freeze_gets_actions_and_critical() {
        let ev = Event::Acted {
            action: Action::Freeze { target: target(2) },
            message: msg(),
            dry_run: false,
        };
        let n = map_event(&ev).unwrap();
        assert_eq!((n.summary.as_str(), n.body.as_str()), ("t", "b"));
        assert_eq!(n.urgency, Urg::Critical);
        assert_eq!(n.frozen_id, Some(2));
    }

    #[test]
    fn dry_run_freeze_keeps_actions() {
        let ev = Event::Acted {
            action: Action::Freeze { target: target(2) },
            message: msg(),
            dry_run: true,
        };
        let n = map_event(&ev).unwrap();
        assert_eq!(n.frozen_id, Some(2));
        assert_eq!(n.urgency, Urg::Critical);
    }

    #[test]
    fn kill_is_critical_without_actions() {
        let ev = Event::Acted {
            action: Action::Kill {
                target: target(3),
                reason: KillReason::Emergency,
            },
            message: msg(),
            dry_run: false,
        };
        let n = map_event(&ev).unwrap();
        assert_eq!((n.urgency, n.frozen_id), (Urg::Critical, None));
    }

    #[test]
    fn notices_and_ignored_events() {
        let n = map_event(&Event::Notice {
            message: msg(),
            frozen_id: Some(5),
        })
        .unwrap();
        assert_eq!((n.urgency, n.frozen_id), (Urg::Critical, Some(5)));
        let n = map_event(&Event::Notice {
            message: msg(),
            frozen_id: None,
        })
        .unwrap();
        assert_eq!((n.urgency, n.frozen_id), (Urg::Normal, None));
        assert_eq!(map_event(&Event::IncidentStart { id: 1, t_ms: 0 }), None);
        assert_eq!(
            map_event(&Event::Level {
                level: olly::engine::Level::Calm
            }),
            None
        );
    }

    #[test]
    fn parses_stream_line() {
        let ev = Event::Notice {
            message: msg(),
            frozen_id: Some(2),
        };
        let line = olly::ipc::event_line(&ev, 5);
        let parsed: EventLine = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed.event, ev);
    }

    // --- user-side sync ---

    #[derive(Default)]
    struct Rec(Vec<String>);
    impl Run for Rec {
        fn run(&mut self, prog: &str, args: &[&str]) -> bool {
            self.0.push(format!("{prog} {}", args.join(" ")));
            true
        }
    }

    fn meta(dir: &std::path::Path, v: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("metadata.json"),
            format!("{{\"KPlugin\":{{\"Version\":\"{v}\"}}}}"),
        )
        .unwrap();
    }

    fn paths(t: &std::path::Path) -> UserPaths {
        let p = UserPaths::from_home(t.join("home").as_path());
        UserPaths {
            share: t.join("share"),
            ..p
        }
    }

    #[test]
    fn user_paths_layout() {
        let p = UserPaths::from_home(std::path::Path::new("/h"));
        assert_eq!(p.share, PathBuf::from("/usr/local/share/olly"));
        assert_eq!(
            p.plasmoid_installed,
            PathBuf::from("/h/.local/share/plasma/plasmoids/org.olly.tray")
        );
        assert_eq!(
            p.user_unit,
            PathBuf::from("/h/.config/systemd/user/olly-notify.service")
        );
    }

    #[test]
    fn widget_upgraded_when_versions_differ() {
        let t = tempfile::tempdir().unwrap();
        let p = paths(t.path());
        meta(&p.share.join("plasmoid"), "2");
        meta(&p.plasmoid_installed, "1");
        let mut r = Rec::default();
        let o = sync_user_side(&p, &mut r);
        assert!(o.widget_upgraded && !o.unit_updated);
        assert_eq!(r.0.len(), 1);
        assert!(r.0[0].starts_with("kpackagetool6 --type Plasma/Applet --upgrade "));
        assert!(r.0[0].ends_with("share/plasmoid"));
    }

    #[test]
    fn widget_untouched_when_equal_or_not_installed() {
        let t = tempfile::tempdir().unwrap();
        let p = paths(t.path());
        meta(&p.share.join("plasmoid"), "2");
        let mut r = Rec::default();
        assert!(!sync_user_side(&p, &mut r).widget_upgraded);
        meta(&p.plasmoid_installed, "2");
        assert!(!sync_user_side(&p, &mut r).widget_upgraded);
        assert!(r.0.is_empty());
    }

    #[test]
    fn widget_failure_is_not_marked_upgraded() {
        struct Fail;
        impl Run for Fail {
            fn run(&mut self, _: &str, _: &[&str]) -> bool {
                false
            }
        }
        let t = tempfile::tempdir().unwrap();
        let p = paths(t.path());
        meta(&p.share.join("plasmoid"), "2");
        meta(&p.plasmoid_installed, "1");
        assert!(!sync_user_side(&p, &mut Fail).widget_upgraded);
    }

    #[test]
    fn unit_copied_and_reloaded_when_different() {
        let t = tempfile::tempdir().unwrap();
        let p = paths(t.path());
        std::fs::create_dir_all(&p.share).unwrap();
        std::fs::write(p.share.join("olly-notify.service"), "new").unwrap();
        let mut r = Rec::default();
        let o = sync_user_side(&p, &mut r);
        assert!(o.unit_updated);
        assert_eq!(std::fs::read_to_string(&p.user_unit).unwrap(), "new");
        assert_eq!(r.0, vec!["systemctl --user daemon-reload".to_string()]);
        let mut r = Rec::default();
        assert!(!sync_user_side(&p, &mut r).unit_updated);
        assert!(r.0.is_empty());
    }

    #[test]
    fn unit_goes_first_and_the_widget_waits() {
        let t = tempfile::tempdir().unwrap();
        let p = paths(t.path());
        meta(&p.share.join("plasmoid"), "2");
        meta(&p.plasmoid_installed, "1");
        std::fs::write(p.share.join("olly-notify.service"), "new").unwrap();
        let mut r = Rec::default();
        let o = sync_user_side(&p, &mut r);
        assert!(o.unit_updated && !o.widget_upgraded);
        assert_eq!(r.0, vec!["systemctl --user daemon-reload".to_string()]);
        // After the restart the unit matches, so the widget goes.
        let mut r = Rec::default();
        let o = sync_user_side(&p, &mut r);
        assert!(!o.unit_updated && o.widget_upgraded);
    }

    #[test]
    fn restart_plasma_targets_the_shell_unit() {
        assert_eq!(RESTART_PLASMA, ["--user", "restart", "plasma-plasmashell"]);
    }

    fn updated(ok: bool, to: &str) -> Event {
        Event::Updated {
            from: "0.1.0".into(),
            to: to.into(),
            ok,
            message: String::new(),
            widget_changed: false,
        }
    }

    #[test]
    fn restart_only_for_ok_and_different() {
        assert!(should_restart(&updated(true, "0.2.0"), "0.1.0"));
        assert!(!should_restart(&updated(true, "0.2.0"), "0.2.0"));
        assert!(!should_restart(&updated(false, "0.2.0"), "0.1.0"));
        assert!(!should_restart(
            &Event::IncidentStart { id: 1, t_ms: 0 },
            "0.1.0"
        ));
    }

    #[test]
    fn updated_shows_no_notification() {
        assert_eq!(map_event(&updated(true, "0.2.0")), None);
    }
}
