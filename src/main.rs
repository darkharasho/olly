use std::path::PathBuf;

const USAGE: &str = "usage: olly daemon [--config PATH] [--root PATH]";

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("daemon") => {}
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
    let mut config = PathBuf::from("/etc/olly/config.toml");
    let mut root = PathBuf::from("/");
    while let Some(a) = args.next() {
        let slot = match a.as_str() {
            "--config" => &mut config,
            "--root" => &mut root,
            _ => {
                eprintln!("olly: unknown argument {a:?}\n{USAGE}");
                std::process::exit(2);
            }
        };
        match args.next() {
            Some(v) => *slot = PathBuf::from(v),
            None => {
                eprintln!("olly: {a} needs a value\n{USAGE}");
                std::process::exit(2);
            }
        }
    }
    let (cfg, cfg_err) = olly::config::load(&config);
    olly::daemon::run(cfg, cfg_err, root)
}
