use olly::engine::{Event, Target};
use olly::incident::Incident;
use olly::ipc::{Request, StatusData};
use olly::voice::fmt_bytes;
use serde::Deserialize;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

const DEFAULT_SOCKET: &str = "/run/olly/olly.sock";

const USAGE: &str = "\u{1F916} here's what I can do:
  olly status                 how memory looks right now
  olly incidents [-n N]       recent incidents, newest first
  olly incident <id>          one incident in full
  olly pause <minutes>        stop slowing and pausing apps for a while
  olly resume                 end a pause early
  olly allow <name>           never close this process name
  olly allowlist              show the never-close list
  olly kill <id>              close a paused app (id from `olly status`)
  olly unfreeze <id>          let a paused app carry on
  olly watch                  follow what olly does, live
  olly daemon [--config PATH] [--root PATH]";

#[derive(Debug, PartialEq)]
enum LocalCmd {
    Daemon { config: PathBuf, root: PathBuf },
    Help,
}

#[derive(Debug, PartialEq)]
enum Cli {
    Req(Request),
    Local(LocalCmd),
}

fn need_id(sub: &str, args: &[String]) -> Result<u64, String> {
    match args {
        [id] => id.parse().map_err(|_| {
            format!(
                "\u{1F916} `olly {sub}` needs the number from `olly status`, like `olly {sub} 2`."
            )
        }),
        _ => Err(format!(
            "\u{1F916} `olly {sub}` needs one id, like `olly {sub} 2`."
        )),
    }
}

fn parse_cli(args: &[String]) -> Result<Cli, String> {
    let Some((sub, rest)) = args.split_first() else {
        return Ok(Cli::Local(LocalCmd::Help));
    };
    let no_args = |req: Request| {
        if rest.is_empty() {
            Ok(Cli::Req(req))
        } else {
            Err(format!(
                "\u{1F916} `olly {sub}` doesn't take any arguments."
            ))
        }
    };
    match sub.as_str() {
        "status" => no_args(Request::Status),
        "resume" => no_args(Request::Resume),
        "allowlist" => no_args(Request::AllowlistList),
        "watch" => no_args(Request::Subscribe),
        "help" | "--help" | "-h" => Ok(Cli::Local(LocalCmd::Help)),
        "incidents" => match rest {
            [] => Ok(Cli::Req(Request::Incidents {
                limit: 10,
                offset: 0,
            })),
            [flag, n] if flag == "-n" => match n.parse::<usize>() {
                Ok(limit) if limit > 0 => Ok(Cli::Req(Request::Incidents { limit, offset: 0 })),
                _ => Err(
                    "\u{1F916} `-n` needs a number of at least 1, like `olly incidents -n 5`."
                        .to_string(),
                ),
            },
            _ => Err("\u{1F916} try `olly incidents` or `olly incidents -n 5`.".to_string()),
        },
        "incident" => Ok(Cli::Req(Request::Incident {
            id: need_id(sub, rest)?,
        })),
        "kill" => Ok(Cli::Req(Request::FrozenKill {
            id: need_id(sub, rest)?,
        })),
        "unfreeze" => Ok(Cli::Req(Request::FrozenResume {
            id: need_id(sub, rest)?,
        })),
        "pause" => {
            let usage = "\u{1F916} `olly pause` needs a number of minutes, like `olly pause 15`.";
            let [m] = rest else {
                return Err(usage.to_string());
            };
            let minutes: u64 = m.parse().map_err(|_| usage.to_string())?;
            if minutes == 0 {
                return Err(
                    "\u{1F916} a pause needs at least 1 minute, like `olly pause 15`.".to_string(),
                );
            }
            let secs = minutes.checked_mul(60).ok_or_else(|| usage.to_string())?;
            Ok(Cli::Req(Request::Pause { secs }))
        }
        "allow" => match rest {
            [name] if !name.trim().is_empty() => {
                Ok(Cli::Req(Request::AllowlistAdd { name: name.clone() }))
            }
            _ => Err(
                "\u{1F916} `olly allow` needs one process name, like `olly allow obs`.".to_string(),
            ),
        },
        "daemon" => {
            let mut config = PathBuf::from("/etc/olly/config.toml");
            let mut root = PathBuf::from("/");
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                let slot = match a.as_str() {
                    "--config" => &mut config,
                    "--root" => &mut root,
                    _ => return Err(format!("\u{1F916} the daemon doesn't know {a:?}.")),
                };
                match it.next() {
                    Some(v) => *slot = PathBuf::from(v),
                    None => return Err(format!("\u{1F916} {a} needs a value.")),
                }
            }
            Ok(Cli::Local(LocalCmd::Daemon { config, root }))
        }
        other => Err(format!("\u{1F916} I don't know `{other}`.\n{USAGE}")),
    }
}

/// Local time like `2026-10-27 14:05`.
fn fmt_time(wall_ms: u64, with_date: bool) -> String {
    let secs = (wall_ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return secs.to_string();
    }
    let fmt = if with_date {
        c"%Y-%m-%d %H:%M"
    } else {
        c"%H:%M:%S"
    };
    let mut buf = [0u8; 64];
    let n = unsafe {
        libc::strftime(
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            fmt.as_ptr(),
            &tm,
        )
    };
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

fn target_lines(label: &str, targets: &[Target]) -> String {
    if targets.is_empty() {
        return String::new();
    }
    let mut s = format!("\n{label}:");
    for t in targets {
        s.push_str(&format!(
            "\n  [{}] {} ({})",
            t.id,
            t.name,
            fmt_bytes(t.bytes)
        ));
    }
    s
}

fn render_status(st: &StatusData) -> String {
    format!(
        "{}{}{}",
        st.message,
        target_lines("Frozen", &st.frozen),
        target_lines("Slowed", &st.throttled)
    )
}

fn render_incidents(list: &[Incident]) -> String {
    if list.is_empty() {
        return "\u{1F916} no incidents yet, which is a good sign.".to_string();
    }
    list.iter()
        .map(|i| {
            format!(
                "{:>4}  {}  {}",
                i.id,
                fmt_time(i.start_ms_wall, true),
                i.summary
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Deserialize)]
struct EventLine {
    event: Event,
    wall_ms: u64,
}

/// One line for a streamed event; `None` for events that carry no message.
fn render_event(line: &str) -> Option<String> {
    let EventLine { event, wall_ms } = serde_json::from_str(line).ok()?;
    let m = match event {
        Event::Acted { message, .. } | Event::Notice { message, .. } => message,
        _ => return None,
    };
    Some(format!(
        "{}  {}: {}",
        fmt_time(wall_ms, false),
        m.title,
        m.body
    ))
}

/// Turns a successful response into the text to print.
fn render(req: &Request, data: serde_json::Value) -> Result<String, String> {
    let bad = |e: serde_json::Error| format!("\u{1F916} olly's answer wasn't what I expected: {e}");
    Ok(match req {
        Request::Status | Request::Pause { .. } | Request::Resume => {
            render_status(&serde_json::from_value(data).map_err(bad)?)
        }
        Request::Incidents { .. } => {
            render_incidents(&serde_json::from_value::<Vec<Incident>>(data).map_err(bad)?)
        }
        Request::Incident { .. } => serde_json::to_string_pretty(&data).map_err(bad)?,
        Request::AllowlistList => serde_json::from_value::<Vec<String>>(data)
            .map_err(bad)?
            .join("\n"),
        Request::AllowlistAdd { name } => format!("\u{1F916} okay, olly will never close {name}."),
        Request::FrozenKill { id } => format!("\u{1F916} okay, closing [{id}]."),
        Request::FrozenResume { id } => format!("\u{1F916} okay, letting [{id}] carry on."),
        Request::Subscribe => String::new(),
    })
}

fn unreachable(socket: &str) -> ! {
    eprintln!("\u{1F916} olly isn't running (couldn't reach {socket}).");
    std::process::exit(1)
}

fn fail(msg: &str) -> ! {
    eprintln!("\u{1F916} {msg}");
    std::process::exit(1)
}

fn client(req: Request) -> ! {
    let socket = std::env::var("OLLY_SOCKET").unwrap_or_else(|_| DEFAULT_SOCKET.to_string());
    let Ok(mut stream) = UnixStream::connect(&socket) else {
        unreachable(&socket)
    };
    let line = serde_json::to_string(&req).expect("request serializes");
    if stream.write_all(format!("{line}\n").as_bytes()).is_err() {
        unreachable(&socket);
    }
    let watching = req == Request::Subscribe;
    let _ = stream.set_read_timeout((!watching).then_some(Duration::from_secs(10)));
    let mut lines = BufReader::new(stream).lines();
    let mut next = || lines.next().and_then(Result::ok);
    let first = next().unwrap_or_else(|| fail("olly didn't answer."));
    let v: serde_json::Value =
        serde_json::from_str(&first).unwrap_or_else(|_| fail("olly's answer wasn't readable."));
    if v["ok"] != true {
        fail(v["error"].as_str().unwrap_or("olly said no."));
    }
    if watching {
        eprintln!("\u{1F916} watching; press Ctrl-C to stop.");
        while let Some(l) = next() {
            if let Some(text) = render_event(&l) {
                println!("{text}");
            }
        }
        fail("olly stopped sending events.");
    }
    match render(&req, v["data"].clone()) {
        Ok(text) => {
            println!("{text}");
            std::process::exit(0)
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1)
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_cli(&args) {
        Ok(Cli::Req(req)) => client(req),
        Ok(Cli::Local(LocalCmd::Help)) => println!("{USAGE}"),
        Ok(Cli::Local(LocalCmd::Daemon { config, root })) => {
            let (cfg, cfg_err) = olly::config::load(&config);
            olly::daemon::run(cfg, cfg_err, root)
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Cli, String> {
        let args: Vec<String> = s.split_whitespace().map(String::from).collect();
        parse_cli(&args)
    }

    fn req(s: &str) -> Request {
        match p(s) {
            Ok(Cli::Req(r)) => r,
            other => panic!("{s}: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_subcommands() {
        assert_eq!(req("status"), Request::Status);
        assert_eq!(req("pause 15"), Request::Pause { secs: 900 });
        assert_eq!(req("resume"), Request::Resume);
        assert_eq!(
            req("allow obs"),
            Request::AllowlistAdd { name: "obs".into() }
        );
        assert_eq!(req("allowlist"), Request::AllowlistList);
        assert_eq!(req("kill 2"), Request::FrozenKill { id: 2 });
        assert_eq!(req("unfreeze 2"), Request::FrozenResume { id: 2 });
        assert_eq!(req("incident 7"), Request::Incident { id: 7 });
        assert_eq!(
            req("incidents"),
            Request::Incidents {
                limit: 10,
                offset: 0
            }
        );
        assert_eq!(
            req("incidents -n 3"),
            Request::Incidents {
                limit: 3,
                offset: 0
            }
        );
        assert_eq!(req("watch"), Request::Subscribe);
        assert_eq!(p("help"), Ok(Cli::Local(LocalCmd::Help)));
        assert_eq!(p(""), Ok(Cli::Local(LocalCmd::Help)));
        assert_eq!(
            p("daemon --config /tmp/c.toml"),
            Ok(Cli::Local(LocalCmd::Daemon {
                config: "/tmp/c.toml".into(),
                root: "/".into()
            }))
        );
    }

    #[test]
    fn cli_errors_are_friendly() {
        let e = p("pause 0").unwrap_err();
        assert!(e.contains("at least 1 minute"), "{e}");
        for bad in [
            "pause",
            "pause abc",
            "pause -5",
            "kill",
            "kill x",
            "kill 1 2",
            "allow",
            "incidents -n 0",
            "incidents -n",
            "status now",
            "frobnicate",
            "daemon --nope",
        ] {
            let e = p(bad).unwrap_err();
            assert!(e.starts_with('\u{1F916}'), "{bad}: {e}");
        }
        // No way to kill an arbitrary pid or remove from the allowlist from here.
        assert!(p("allowlist remove obs").is_err());
        assert!(p("pause 99999999999999999999").is_err());
        assert!(p("pause 307445734561825861").is_err());
    }

    #[test]
    fn renders_status_and_incidents() {
        let st: StatusData = serde_json::from_str(
            r#"{"level":"warning","dry_run":false,"paused_until_wall_ms":null,"mem_available":1,"mem_total":2,
            "throttled":[],"frozen":[{"id":2,"key":{"cgroup":"/a","root_pid":null},"name":"game.exe","kind":"Cgroup","bytes":21000000000}],
            "self_rss":1,"message":"Memory is a bit tight."}"#,
        )
        .unwrap();
        let text = render_status(&st);
        assert!(
            text.starts_with("Memory is a bit tight.\nFrozen:\n  [2] game.exe ("),
            "{text}"
        );
        assert!(!text.contains("Slowed"));
        assert!(render_incidents(&[]).contains("no incidents"));
        let line = r#"{"event":{"Notice":{"message":{"title":"T","body":"B"},"frozen_id":null}},"wall_ms":0}"#;
        assert!(render_event(line).unwrap().ends_with("T: B"));
        assert_eq!(
            render_event(r#"{"event":{"IncidentStart":{"id":1,"t_ms":0}},"wall_ms":0}"#),
            None
        );
    }
}
