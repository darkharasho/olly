//! Desktop notification agent: subscribes to olly's event stream and shows
//! freedesktop notifications. Runs as the desktop user; never needed by the daemon.

use notify_rust::{Notification, Urgency};
use olly::engine::{Action, Event};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
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
        }),
        _ => None,
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

/// Sends a command over a fresh connection and logs the answer.
fn send_command(cmd: &str, id: u64) {
    let line = format!("{{\"cmd\":\"{cmd}\",\"id\":{id}}}\n");
    let result = UnixStream::connect(socket_path()).and_then(|mut s| {
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        s.write_all(line.as_bytes())?;
        let mut resp = String::new();
        BufReader::new(s).read_line(&mut resp)?;
        Ok(resp)
    });
    match result {
        Ok(r) if r.contains("\"ok\":true") => eprintln!("olly-notify: {cmd} {id}: done"),
        Ok(r) => eprintln!("olly-notify: {cmd} {id}: {}", r.trim()),
        Err(e) => eprintln!("olly-notify: {cmd} {id}: {e}"),
    }
}

/// Notification ids and click generations per frozen target.
#[derive(Default)]
struct Shown {
    ids: HashMap<u64, u32>,
    gens: HashMap<u64, Arc<AtomicU64>>,
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
    if let Some(fid) = note.frozen_id {
        n.action("kill", "Kill").action("resume", "Resume");
        let mut s = shown.lock().unwrap();
        if let Some(&id) = s.ids.get(&fid) {
            n.id(id);
        }
        let g = s.gens.entry(fid).or_default().clone();
        generation = Some((g.fetch_add(1, Ordering::SeqCst) + 1, g));
    }
    let handle = match n.show() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("olly-notify: cannot show a notification: {e}");
            return;
        }
    };
    let (Some(fid), Some((mine, g))) = (note.frozen_id, generation) else {
        return;
    };
    shown.lock().unwrap().ids.insert(fid, handle.id());
    // Waiting for a click blocks, so it gets its own thread. A newer notification for the
    // same target replaces this one; its clicks win and this thread's are ignored.
    let spawned = std::thread::Builder::new()
        .name("olly-notify-action".into())
        .spawn(move || {
            handle.wait_for_action(|action| {
                if g.load(Ordering::SeqCst) != mine {
                    return;
                }
                match action {
                    "kill" => send_command("frozen_kill", fid),
                    "resume" => send_command("frozen_resume", fid),
                    _ => {}
                }
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
}
