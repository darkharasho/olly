use olly::engine::{Event, Target};
use olly::incident::Incident;
use olly::ipc::{Request, StatusData};
use olly::supervise::LastUpdate;
use olly::update::UpdateInfo;
use olly::voice::fmt_bytes;
use serde::Deserialize;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

const DEFAULT_SOCKET: &str = "/run/olly/olly.sock";

const USAGE: &str = "\u{1F916} here's what I can do:
  olly status [--json]             how memory looks right now
  olly incidents [-n N] [--json]   recent incidents, newest first
  olly incident <id>               one incident in full
  olly pause <minutes>             stop slowing and pausing apps for a while
  olly resume                      end a pause early
  olly allow <name>                never close this process name
  olly allowlist                   show the never-close list
  olly kill <id>                   close a paused app (id from `olly status`)
  olly unfreeze <id>               let a paused app carry on
  olly update [--check]            install a new version, or just look for one
  olly watch                       follow what olly does, live
  olly daemon [--config PATH] [--root PATH]";

#[derive(Debug, PartialEq)]
enum LocalCmd {
    Daemon {
        config: PathBuf,
        root: PathBuf,
    },
    /// Hidden: run by the daemon in its own transient unit to swap in a staged release.
    Supervise {
        root: PathBuf,
        from: String,
        to: String,
    },
    /// `olly update`, talking to the daemon with several requests.
    Update(UpdateMode),
    Help,
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum UpdateMode {
    /// Just look for a new version.
    Check,
    /// Start an update and follow it to the end.
    Wait,
    /// Start an update and return at once.
    NoWait,
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum Output {
    Text,
    Json,
}

#[derive(Debug, PartialEq)]
enum Cli {
    Req(Request, Output),
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
    // `--json` is only accepted by `status` and `incidents`.
    let (json, rest) = match sub.as_str() {
        "status" | "incidents" => (
            rest.iter().any(|a| a == "--json"),
            &rest
                .iter()
                .filter(|a| *a != "--json")
                .cloned()
                .collect::<Vec<_>>()[..],
        ),
        _ => (false, rest),
    };
    let out = if json { Output::Json } else { Output::Text };
    let no_args = |req: Request| {
        if rest.is_empty() {
            Ok(Cli::Req(req, out))
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
            [] => Ok(Cli::Req(
                Request::Incidents {
                    limit: 10,
                    offset: 0,
                },
                out,
            )),
            [flag, n] if flag == "-n" => match n.parse::<usize>() {
                Ok(limit) if limit > 0 => {
                    Ok(Cli::Req(Request::Incidents { limit, offset: 0 }, out))
                }
                _ => Err(
                    "\u{1F916} `-n` needs a number of at least 1, like `olly incidents -n 5`."
                        .to_string(),
                ),
            },
            _ => Err("\u{1F916} try `olly incidents` or `olly incidents -n 5`.".to_string()),
        },
        "incident" => Ok(Cli::Req(
            Request::Incident {
                id: need_id(sub, rest)?,
            },
            Output::Text,
        )),
        "kill" => Ok(Cli::Req(
            Request::FrozenKill {
                id: need_id(sub, rest)?,
            },
            Output::Text,
        )),
        "unfreeze" => Ok(Cli::Req(
            Request::FrozenResume {
                id: need_id(sub, rest)?,
            },
            Output::Text,
        )),
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
            Ok(Cli::Req(Request::Pause { secs }, Output::Text))
        }
        "allow" => match rest {
            [name] if !name.trim().is_empty() => Ok(Cli::Req(
                Request::AllowlistAdd { name: name.clone() },
                Output::Text,
            )),
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
        "update" => match rest {
            [] => Ok(Cli::Local(LocalCmd::Update(UpdateMode::Wait))),
            [f] if f == "--check" => Ok(Cli::Local(LocalCmd::Update(UpdateMode::Check))),
            [f] if f == "--no-wait" => Ok(Cli::Local(LocalCmd::Update(UpdateMode::NoWait))),
            _ => Err("\u{1F916} try `olly update`, `olly update --check` or `olly update --no-wait`.".to_string()),
        },
        "update-supervise" => match rest {
            [root, from, to] => Ok(Cli::Local(LocalCmd::Supervise {
                root: PathBuf::from(root),
                from: from.clone(),
                to: to.clone(),
            })),
            _ => Err(
                "\u{1F916} `olly update-supervise` needs a staged folder, the old version and the new one."
                    .to_string(),
            ),
        },
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
    let update = st.update.as_ref().map_or(String::new(), |u| {
        format!(
            "\nolly {} is ready! Run `olly update` to install it.",
            u.version
        )
    });
    format!(
        "{}{}{}{}",
        st.message,
        update,
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
        Event::Updated { message, .. } => {
            return Some(format!("{}  {message}", fmt_time(wall_ms, false)));
        }
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
        Request::Pause { .. } => {
            let st: StatusData = serde_json::from_value(data).map_err(bad)?;
            let mut text = render_status(&st);
            if !st.frozen.is_empty() {
                text.push_str("\nApps olly has already frozen keep their timers: if you don't answer, olly still closes them when the time is up.");
            }
            text
        }
        Request::Status | Request::Resume => {
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
        Request::UpdateCheck => render_check(
            serde_json::from_value::<Option<UpdateInfo>>(data)
                .map_err(bad)?
                .as_ref(),
            env!("CARGO_PKG_VERSION"),
        ),
        Request::Update => "\u{1F916} updating…".to_string(),
        // Only the supervisor sends this.
        Request::UpdateFinished => String::new(),
    })
}

/// What `olly update --check` prints.
fn render_check(info: Option<&UpdateInfo>, current: &str) -> String {
    let Some(u) = info else {
        return format!("\u{1F916} you're on the newest olly ({current}).");
    };
    let mut s = format!("\u{1F916} olly {} is ready!", u.version);
    for n in &u.notes {
        s.push_str(&format!("\n  • {n}"));
    }
    s.push_str("\nRun `olly update` to install it.");
    s
}

/// Is `last` the result of the update we started? `baseline` is the
/// `at_wall_ms` seen just before asking (0 if there was none).
fn is_new_result(last: Option<&LastUpdate>, baseline: u64) -> bool {
    last.is_some_and(|l| l.at_wall_ms > baseline)
}

fn unreachable(socket: &str) -> ! {
    eprintln!("\u{1F916} olly isn't running (couldn't reach {socket}).");
    std::process::exit(1)
}

fn fail(msg: &str) -> ! {
    eprintln!("\u{1F916} {msg}");
    std::process::exit(1)
}

static STDOUT_CLAIMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Only one thread may print the final `--json` line; the loser waits for the winner to exit.
fn claim_stdout() {
    if STDOUT_CLAIMED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        loop {
            std::thread::park();
        }
    }
}

/// What `--json` prints for every failure the tray should treat as "offline".
fn offline() -> ! {
    claim_stdout();
    println!("{{\"offline\":true}}");
    std::process::exit(1)
}

fn client(req: Request, out: Output) -> ! {
    let json = out == Output::Json;
    if json {
        // Overall deadline: connect has no timeout and read timeouts restart on every read.
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(1000));
            offline()
        });
    }
    let socket = std::env::var("OLLY_SOCKET").unwrap_or_else(|_| DEFAULT_SOCKET.to_string());
    let Ok(mut stream) = UnixStream::connect(&socket) else {
        if json {
            offline()
        }
        unreachable(&socket)
    };
    let line = serde_json::to_string(&req).expect("request serializes");
    if stream.write_all(format!("{line}\n").as_bytes()).is_err() {
        if json {
            offline()
        }
        unreachable(&socket);
    }
    let watching = req == Request::Subscribe;
    let timeout = if json {
        Duration::from_millis(1000)
    } else {
        Duration::from_secs(10)
    };
    let _ = stream.set_read_timeout((!watching).then_some(timeout));
    let mut lines = BufReader::new(stream).lines();
    let mut next = || lines.next().and_then(Result::ok);
    if json {
        let Some(v) = next().and_then(|l| serde_json::from_str::<serde_json::Value>(&l).ok())
        else {
            offline()
        };
        let Some(obj) = v.as_object() else { offline() };
        if obj.get("ok") == Some(&serde_json::Value::Bool(false)) {
            let msg = obj
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("olly said no.");
            claim_stdout();
            println!("{}", serde_json::json!({ "error": msg }));
            std::process::exit(1);
        }
        let (true, Some(data)) = (
            obj.get("ok") == Some(&serde_json::Value::Bool(true)),
            obj.get("data"),
        ) else {
            offline()
        };
        claim_stdout();
        println!("{data}");
        std::process::exit(0);
    }
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

fn socket_path() -> String {
    std::env::var("OLLY_SOCKET").unwrap_or_else(|_| DEFAULT_SOCKET.to_string())
}

enum AskErr {
    Unreachable,
    NoAnswer,
}

/// One request, one reply, on a fresh connection.
fn ask(socket: &str, req: &Request, timeout: Duration) -> Result<serde_json::Value, AskErr> {
    let mut stream = UnixStream::connect(socket).map_err(|_| AskErr::Unreachable)?;
    let line = serde_json::to_string(req).map_err(|_| AskErr::NoAnswer)?;
    stream
        .write_all(format!("{line}\n").as_bytes())
        .map_err(|_| AskErr::Unreachable)?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|_| AskErr::NoAnswer)?;
    let first = BufReader::new(stream)
        .lines()
        .next()
        .and_then(Result::ok)
        .ok_or(AskErr::NoAnswer)?;
    serde_json::from_str(&first).map_err(|_| AskErr::NoAnswer)
}

fn status_of(socket: &str) -> Option<StatusData> {
    let v = ask(socket, &Request::Status, Duration::from_secs(10)).ok()?;
    if v["ok"] != true {
        return None;
    }
    serde_json::from_value(v["data"].clone()).ok()
}

fn update_cmd(mode: UpdateMode) -> ! {
    let socket = socket_path();
    if mode == UpdateMode::Check {
        // The daemon waits up to 75 s for the fetch.
        let v = match ask(&socket, &Request::UpdateCheck, Duration::from_secs(80)) {
            Ok(v) => v,
            Err(AskErr::Unreachable) => unreachable(&socket),
            Err(AskErr::NoAnswer) => fail("olly didn't answer."),
        };
        if v["ok"] != true {
            fail(v["error"].as_str().unwrap_or("olly said no."));
        }
        let current = status_of(&socket)
            .map(|st| st.version)
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
        let data = serde_json::from_value::<Option<UpdateInfo>>(v["data"].clone());
        match data {
            Ok(info) => {
                println!("{}", render_check(info.as_ref(), &current));
                std::process::exit(0)
            }
            Err(e) => {
                eprintln!("\u{1F916} olly's answer wasn't what I expected: {e}");
                std::process::exit(1)
            }
        }
    }
    let Some(before) = status_of(&socket) else {
        unreachable(&socket)
    };
    let baseline = before.last_update.as_ref().map_or(0, |l| l.at_wall_ms);
    let v = match ask(&socket, &Request::Update, Duration::from_secs(10)) {
        Ok(v) => v,
        Err(AskErr::Unreachable) => unreachable(&socket),
        Err(AskErr::NoAnswer) => fail("olly didn't answer."),
    };
    if v["ok"] != true {
        fail(v["error"].as_str().unwrap_or("olly said no."));
    }
    println!("\u{1F916} updating…");
    if mode == UpdateMode::NoWait {
        std::process::exit(0);
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(1));
        // Failures are expected while the daemon restarts.
        let Some(st) = status_of(&socket) else {
            continue;
        };
        if is_new_result(st.last_update.as_ref(), baseline) {
            let l = st.last_update.expect("checked above");
            println!("\u{1F916} {}", l.message);
            std::process::exit(if l.ok { 0 } else { 1 });
        }
    }
    fail("I lost track of the update; check `olly status`.")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_cli(&args) {
        Ok(Cli::Req(req, out)) => client(req, out),
        Ok(Cli::Local(LocalCmd::Update(mode))) => update_cmd(mode),
        Ok(Cli::Local(LocalCmd::Help)) => println!("{USAGE}"),
        Ok(Cli::Local(LocalCmd::Daemon { config, root })) => {
            let (cfg, cfg_err) = olly::config::load(&config);
            olly::daemon::run(cfg, cfg_err, root)
        }
        Ok(Cli::Local(LocalCmd::Supervise { root, from, to })) => {
            use olly::supervise::{supervise, Layout, SystemHost};
            if unsafe { libc::geteuid() } != 0 {
                fail("`olly update-supervise` only runs as root; olly starts it for you.");
            }
            let result = supervise(&Layout::system(), &root, &from, &to, &mut SystemHost);
            std::process::exit(if result.ok { 0 } else { 1 })
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
            Ok(Cli::Req(r, Output::Text)) => r,
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
    fn parses_hidden_update_supervise() {
        assert_eq!(
            p("update-supervise /x 0.1.0 0.2.0"),
            Ok(Cli::Local(LocalCmd::Supervise {
                root: "/x".into(),
                from: "0.1.0".into(),
                to: "0.2.0".into()
            }))
        );
        assert!(p("update-supervise /x 0.1.0").is_err());
        assert!(!USAGE.contains("supervise"));
    }

    #[test]
    fn parses_json_flag() {
        assert_eq!(
            p("status --json"),
            Ok(Cli::Req(Request::Status, Output::Json))
        );
        let inc = || Request::Incidents {
            limit: 20,
            offset: 0,
        };
        assert_eq!(
            p("incidents -n 20 --json"),
            Ok(Cli::Req(inc(), Output::Json))
        );
        assert_eq!(
            p("incidents --json -n 20"),
            Ok(Cli::Req(inc(), Output::Json))
        );
        assert!(p("kill 2 --json").is_err());
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

    fn info(notes: &[&str]) -> UpdateInfo {
        UpdateInfo {
            version: "0.2.0".into(),
            notes: notes.iter().map(|s| s.to_string()).collect(),
            url: "u".into(),
        }
    }

    #[test]
    fn parses_update() {
        let u = |m| Ok(Cli::Local(LocalCmd::Update(m)));
        assert_eq!(p("update"), u(UpdateMode::Wait));
        assert_eq!(p("update --check"), u(UpdateMode::Check));
        assert_eq!(p("update --no-wait"), u(UpdateMode::NoWait));
        let e = p("update --bogus").unwrap_err();
        assert!(e.starts_with('\u{1F916}'), "{e}");
        assert!(p("update --check --no-wait").is_err());
        assert!(USAGE.contains(
            "olly update [--check]            install a new version, or just look for one"
        ));
    }

    #[test]
    fn renders_check() {
        assert_eq!(
            render_check(None, "0.1.0"),
            "\u{1F916} you're on the newest olly (0.1.0)."
        );
        assert_eq!(
            render_check(Some(&info(&["a", "b"])), "0.1.0"),
            "\u{1F916} olly 0.2.0 is ready!\n  • a\n  • b\nRun `olly update` to install it."
        );
        let t = render(&Request::UpdateCheck, serde_json::Value::Null).unwrap();
        assert!(t.contains("newest olly"), "{t}");
    }

    #[test]
    fn status_mentions_offered_update() {
        let mut st: StatusData = serde_json::from_str(
            r#"{"level":"calm","dry_run":false,"paused_until_wall_ms":null,"mem_available":1,"mem_total":2,
            "throttled":[],"frozen":[],"self_rss":1,"message":"All calm."}"#,
        )
        .unwrap();
        assert!(!render_status(&st).contains("ready"));
        st.update = Some(info(&[]));
        assert_eq!(
            render_status(&st),
            "All calm.\nolly 0.2.0 is ready! Run `olly update` to install it."
        );
    }

    #[test]
    fn renders_updated_event() {
        let line = r#"{"event":{"Updated":{"from":"0.1.0","to":"0.2.0","ok":true,"message":"updated to 0.2.0! ✨","widget_changed":false}},"wall_ms":0}"#;
        assert!(render_event(line)
            .unwrap()
            .ends_with("updated to 0.2.0! ✨"));
    }

    #[test]
    fn waits_only_for_a_newer_result() {
        let l = |at| LastUpdate {
            from: "a".into(),
            to: "b".into(),
            ok: true,
            at_wall_ms: at,
            message: String::new(),
            widget_changed: false,
        };
        assert!(!is_new_result(None, 0));
        assert!(!is_new_result(Some(&l(5)), 5));
        assert!(is_new_result(Some(&l(6)), 5));
        assert!(is_new_result(Some(&l(1)), 0));
    }

    #[test]
    fn pause_reply_mentions_frozen_timers() {
        let status = |frozen: &str| {
            serde_json::from_str::<serde_json::Value>(&format!(
                r#"{{"level":"critical","dry_run":false,"paused_until_wall_ms":1,"mem_available":1,"mem_total":2,
                "throttled":[],"frozen":[{frozen}],"self_rss":1,"message":"Paused."}}"#
            ))
            .unwrap()
        };
        let one = r#"{"id":2,"key":{"cgroup":"/a","root_pid":null},"name":"game.exe","kind":"Cgroup","bytes":1}"#;
        let text = render(&Request::Pause { secs: 60 }, status(one)).unwrap();
        assert!(text.contains("already frozen"), "{text}");
        let text = render(&Request::Pause { secs: 60 }, status("")).unwrap();
        assert!(!text.contains("already frozen"), "{text}");
        // Only the pause reply carries the note.
        let text = render(&Request::Status, status(one)).unwrap();
        assert!(!text.contains("already frozen"), "{text}");
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
