//! session/
//! How one cast runs, from the capture to the TV, and where its progress goes (the terminal
//! or the menu). menu_loop.rs runs the casts started from the menu; terminal.rs holds the
//! terminal helpers (spinner, device choice, Ctrl+C).
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

mod menu_loop;
mod terminal;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::capture::{self, Capture, ScreenCast};
use crate::device::{self, CastDevice};
use crate::{cli, hls, menu, mirror};
pub use menu_loop::run_menu;
pub use terminal::{pick_device, spin, stop_flag, SEARCHING};

/// Never set: outside the menu, stopping always closes the receiver app.
pub static NO_KEEP: AtomicBool = AtomicBool::new(false);
/// Pause between two launch tries in the menu.
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// Where cast progress goes: the terminal, with spinners, or the menu.
#[derive(Clone)]
pub enum Out {
    Term,
    Menu(Sender<menu::Status>),
}

impl Out {
    pub fn say(&self, msg: &str) {
        match self {
            Out::Term => eprintln!("{msg}"),
            Out::Menu(_) => self.send(menu::Status::Info(msg.to_string())),
        }
    }

    /// Menu only: the terminal shows messages, not stats.
    pub fn send(&self, status: menu::Status) {
        if let Out::Menu(tx) = self {
            let _ = tx.send(status);
        }
    }

    pub fn report(&self, r: device::Report) {
        match r {
            device::Report::Say(m) => self.say(m),
            device::Report::Player(state) => self.send(menu::Status::Player(format!("{state:?}"))),
            device::Report::Reconnecting => self.send(menu::Status::Reconnecting),
            device::Report::Loaded(..) => {}
        }
    }

    /// Run `f`, showing `msg` while it runs.
    pub fn step<T>(&self, msg: &str, f: impl FnOnce() -> T) -> T {
        match self {
            Out::Term => spin(msg, f),
            Out::Menu(_) => {
                self.say(msg);
                f()
            }
        }
    }
}

/// Serve the capture over HTTP until `stop`; return the stream path, its content type and the server thread.
pub fn serve(
    capture: &Capture,
    stream: &cli::Stream,
    stop: &'static AtomicBool,
) -> anyhow::Result<(&'static str, &'static str, JoinHandle<()>)> {
    Ok(match capture.tcp_port {
        Some(source) => (capture::MKV, "video/x-matroska", hls::serve_mkv(source, stream.port, stop)?),
        None => (capture::PLAYLIST, "application/x-mpegURL", hls::serve_hls(capture.dir(), stream.port, stop)?),
    })
}

/// Capture `screen`, serve it and play it on `device` until `stop` or the end of the cast.
/// Frees the HTTP port before returning, so the next cast can bind it.
/// With `keep` (menu), reuse a receiver left open and leave it open when `keep` is set at the stop.
pub fn cast_screen(
    device: &CastDevice,
    s: &menu::Settings,
    screen: &ScreenCast,
    stop: &'static AtomicBool,
    keep: Option<&'static AtomicBool>,
    out: &Out,
) -> anyhow::Result<()> {
    tracing::info!(name = %device.name, ip = %device.ip, "casting");
    // A panel takes the screenshare format, whatever the settings say.
    if device.kind == device::discovery::Kind::Screenshare {
        let settings = menu::Settings { stream: cli::Stream { format: cli::Format::Screenshare, ..s.stream.clone() }, ..s.clone() };
        let host = std::net::SocketAddr::from((device.ip, device.port));
        return crate::screenshare::cast(host, None, &settings, screen, stop, out);
    }
    if let Out::Menu(_) = out {
        watch_round_trip(device, stop, out.clone());
    }
    let hls_settings;
    let (s, keep) = if s.stream.format == cli::Format::Mirror {
        if mirror::cast(device, s, screen, stop, out)? {
            return Ok(());
        }
        out.say(&format!("{} has no screen mirroring: casting in HLS (2 to 3 s of delay).", device.name));
        hls_settings = menu::Settings { stream: cli::Stream { format: cli::Format::Hls, ..s.stream.clone() }, ..s.clone() };
        // The menu does not track this receiver (it expected mirror): always close it at the stop.
        (&hls_settings, None)
    } else {
        (s, keep)
    };
    let reuse = keep.is_some();
    let keep = keep.unwrap_or(&NO_KEEP);
    let stream = &s.stream;
    let ip = hls::lan_ip(device.ip.into())?;
    let hls = Capture::start(Some(screen), stream, s.audio_delay)?;
    // No live audio delay in HLS: the jump in the segments' sound made the Sharp buffer.
    let (path, content_type, server) = serve(&hls, stream, stop)?;
    if stream.format == cli::Format::Hls {
        out.send(menu::Status::Segments(hls.dir().to_path_buf()));
    }
    if let Some(served) = hls.served() {
        out.send(menu::Status::Served(Box::new(served)));
    }
    out.send(menu::Status::Dropped(Arc::clone(&hls.dropped)));
    let played = (|| {
        // The capture fills segments while the TV starts its receiver (2-3 s): both waits overlap.
        let receiver = launch(device, &s.receiver, reuse, stop, out)?;
        let prepared = out.step("Preparing the stream...", || -> anyhow::Result<()> {
            while stream.format == cli::Format::Hls
                && hls.segments()? < capture::MIN_SEGMENTS
                && !stop.load(Ordering::SeqCst)
            {
                hls.poll(Duration::from_millis(100))?;
            }
            Ok(())
        });
        if prepared.is_err() || stop.load(Ordering::SeqCst) {
            if !keep.load(Ordering::SeqCst) {
                receiver.close();
            }
            return prepared;
        }
        let capture = hls.watch(stop)?;
        let url = format!("http://{ip}:{}/{path}?{}", stream.port, stream_query(s));
        let played = receiver.play(&url, content_type, true, stop, keep, &|r| out.report(r));
        stop.store(true, Ordering::SeqCst);
        // A capture failure explains why the cast stopped: report it first.
        capture.join().expect("capture watcher panicked")?;
        played
    })();
    stop.store(true, Ordering::SeqCst);
    // Ends the Matroska relay connections, so the server can finish.
    drop(hls);
    let _ = server.join();
    played
}

/// Query of the stream URL, read by sccast's receiver page; the server ignores it.
/// kbps lets the page tell a network too slow for the stream.
fn stream_query(s: &menu::Settings) -> String {
    let mut query = vec![format!("kbps={}", s.stream.bitrate), format!("latency={}", s.latency)];
    if cfg!(debug_assertions) {
        query.push("debug".to_string());
    }
    query.join("&")
}

/// Every 5 s until `stop`, time a TCP connection to the TV's Cast port: a congested network
/// shows there (the school's Chromecast answered in 170 to 500 ms, the test TV in 2 ms).
fn watch_round_trip(device: &CastDevice, stop: &'static AtomicBool, out: Out) {
    let address = std::net::SocketAddr::from((device.ip, device.port));
    let probe = move || {
        let started = Instant::now();
        let answered = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2)).is_ok();
        answered.then(|| started.elapsed())
    };
    std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            let first = probe();
            // Linux resends a lost SYN after 1 s: try again at once, so one lost packet does
            // not show as a slow network (seen with 5 % of losses: 1026 ms).
            let time = if first.is_some_and(|t| t < Duration::from_secs(1)) {
                first
            } else {
                match (first, probe()) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                }
            };
            out.send(menu::Status::RoundTrip(time));
            for _ in 0..50 {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    });
}

/// Start the receiver on `device`, telling the user when sccast's own fell back to Google's.
/// With `reuse` (the menu), try again without limit: the first launch on a Chromecast can fail,
/// the user sees each try and can stop.
pub fn launch(
    device: &CastDevice,
    app_id: &str,
    reuse: bool,
    stop: &'static AtomicBool,
    out: &Out,
) -> anyhow::Result<device::Receiver> {
    let mut tries = 1;
    let receiver = loop {
        let label = match tries {
            1 => format!("Starting the receiver on {}...", device.name),
            n => format!("Starting the receiver on {} (try {n})...", device.name),
        };
        match out.step(&label, || device::launch(device, app_id, reuse, stop)) {
            Ok(receiver) => break receiver,
            Err(e) if reuse && !stop.load(Ordering::SeqCst) => {
                tracing::warn!(tries, "launch failed: {e:#}");
                tries += 1;
                std::thread::sleep(RETRY_DELAY);
            }
            Err(e) => return Err(e),
        }
    };
    if receiver.app_id() != app_id {
        out.say(&format!(
            "The sccast receiver did not start on {}: using Google's receiver (about 5 s of delay).",
            device.name
        ));
    }
    Ok(receiver)
}
