//! terminal.rs
//! Helpers for the command line without the menu: a spinner while a step runs, the choice
//! of the device, and the Ctrl+C / SIGTERM flag.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::bail;

use crate::device::{discovery, CastDevice};

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(8);
pub const SEARCHING: &str = "Searching for Chromecast devices, please wait...";

/// Flag set by Ctrl+C or SIGTERM. The handler can be installed only once per process.
pub fn stop_flag() -> anyhow::Result<&'static AtomicBool> {
    static STOP: AtomicBool = AtomicBool::new(false);
    ctrlc::set_handler(|| {
        STOP.store(true, Ordering::SeqCst);
        // Closing the TV app hung once the TV was back on its home screen: do not wait forever.
        // ponytail: exit skips destructors (the tmpfs segment dir stays), add a timeout per call if that matters.
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(5));
            // Not eprintln!: it panics when the terminal was closed (SIGHUP), and the exit would not come.
            let _ = writeln!(std::io::stderr(), "The device did not answer: exiting.");
            std::process::exit(130);
        });
    })?;
    Ok(&STOP)
}

/// Run `f` while an animated `message` turns on stderr. Plain run when stderr is not a terminal.
pub fn spin<T>(message: &str, f: impl FnOnce() -> T) -> T {
    if !std::io::stderr().is_terminal() {
        return f();
    }
    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            // Rotating star: a cross turning by 45 degrees each frame.
            for frame in ["✚", "✖"].iter().cycle() {
                if done.load(Ordering::Relaxed) {
                    break;
                }
                eprint!("\r{frame} {message}");
                std::thread::sleep(Duration::from_millis(150));
            }
            // Erase the line so the next output starts clean.
            eprint!("\r\x1b[2K");
        });
        let result = f();
        done.store(true, Ordering::Relaxed);
        result
    })
}

/// Resolve `--device`, or let the user pick one from the devices found.
pub fn pick_device(query: Option<&str>) -> anyhow::Result<CastDevice> {
    if let Some(query) = query {
        return spin(&format!("Looking for {query}..."), || discovery::find(query, DISCOVERY_TIMEOUT));
    }
    let mut devices = spin(SEARCHING, || discovery::discover(DISCOVERY_TIMEOUT))?;
    if devices.is_empty() {
        bail!("{}", discovery::NONE_FOUND);
    }
    for (i, d) in devices.iter().enumerate() {
        println!("{}) {} ({})", i + 1, d.name, d.model);
    }
    print!("Device number: ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    match answer.trim().parse::<usize>() {
        Ok(n) if (1..=devices.len()).contains(&n) => Ok(devices.swap_remove(n - 1)),
        _ => bail!("no device chosen"),
    }
}
