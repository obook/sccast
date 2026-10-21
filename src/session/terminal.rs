//! terminal.rs
//! Terminal helpers: a spinner while a step runs, the choice of the device, the Ctrl+C /
//! SIGTERM flag, and stderr sent to the log while the menu draws.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::fs::File;
use std::io::{IsTerminal, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::bail;

use crate::device::{discovery, CastDevice};

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(8);
pub const SEARCHING: &str = "Searching for Chromecast devices, please wait...";
const STDERR: i32 = 2;

/// While it lives, what libraries print on stderr (a GStreamer plugin's "Xlib: extension
/// missing", a panic) goes to a file instead of over the menu, which only redraws the cells
/// it changed. Dropping it gives stderr back.
pub struct StderrToFile {
    saved: OwnedFd,
}

impl StderrToFile {
    pub fn new(file: &File) -> std::io::Result<Self> {
        let saved = std::io::stderr().as_fd().try_clone_to_owned()?;
        // SAFETY: both descriptors are open; dup2 only replaces descriptor 2.
        if unsafe { libc::dup2(file.as_raw_fd(), STDERR) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { saved })
    }
}

impl Drop for StderrToFile {
    fn drop(&mut self) {
        // SAFETY: `saved` is the original stderr, still open.
        unsafe { libc::dup2(self.saved.as_raw_fd(), STDERR) };
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sends_stderr_to_a_file() {
        let log = tempfile::NamedTempFile::new().unwrap();
        {
            let _redirect = StderrToFile::new(log.as_file()).unwrap();
            // Written to descriptor 2 itself, as a C library does.
            std::io::stderr().write_all(b"Xlib: extension missing\n").unwrap();
        }
        assert_eq!(std::fs::read_to_string(log.path()).unwrap(), "Xlib: extension missing\n");
    }
}
