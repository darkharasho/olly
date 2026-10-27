//! memhog: a gentle way to watch olly work. It allocates and touches memory
//! at a steady rate, up to a cap, then holds until you stop it.
//!
//!     memhog <mb_per_sec> [--max MB]
//!
//! Run it under a hard cap, e.g.
//!     systemd-run --user --scope -p MemoryMax=14G memhog 1024 --max 12288

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const DEFAULT_MAX_MB: usize = 2048;
const MIB: usize = 1024 * 1024;

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn usage() -> ! {
    eprintln!(
        "\u{1F916} memhog <mb_per_sec> [--max MB]\n   \
         allocates memory at that rate, up to --max (default {DEFAULT_MAX_MB} MB), then holds."
    );
    std::process::exit(2)
}

fn parse(args: &[String]) -> Option<(usize, usize)> {
    let mut rate = None;
    let mut max = DEFAULT_MAX_MB;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--max" {
            max = it.next()?.parse().ok()?;
        } else if rate.is_none() {
            rate = Some(a.parse::<usize>().ok().filter(|&r| r > 0)?);
        } else {
            return None;
        }
    }
    Some((rate?, max))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((rate, max)) = parse(&args) else {
        usage()
    };
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
    println!("\u{1F916} memhog: growing {rate} MB/s up to {max} MB. Ctrl-C to stop.");
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut total = 0usize;
    while !STOP.load(Ordering::Relaxed) {
        let tick = Instant::now();
        let step = rate.min(max - total);
        if step > 0 {
            let mut v = vec![0u8; step * MIB];
            // Touch every page so it is really resident.
            for i in (0..v.len()).step_by(4096) {
                v[i] = 1;
            }
            chunks.push(v);
            total += step;
        }
        if total >= max {
            println!("\u{1F916} memhog: holding at {total} MB. Ctrl-C to let go.");
        } else {
            println!("\u{1F916} memhog: {total} MB so far.");
        }
        while !STOP.load(Ordering::Relaxed) && tick.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    println!("\u{1F916} memhog: letting go of {total} MB. Bye!");
}
