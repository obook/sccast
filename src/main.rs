mod capture;
mod cast;
mod cli;
mod discovery;
mod menu;
mod portal;
mod server;

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::bail;
use clap::Parser;
use cli::{Cli, Command};
use tracing_subscriber::EnvFilter;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(8);
/// Never set: outside the menu, stopping always closes the receiver app.
static NO_KEEP: AtomicBool = AtomicBool::new(false);
/// Set while the menu runs: logs go to log_path().
static MENU_LOG: AtomicBool = AtomicBool::new(false);

/// Menu log, on the per-user tmpfs (private, cleared at logout).
fn log_path() -> std::path::PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir, Into::into).join("sccast.log")
}
/// A normal launch takes 2 to 6 s, the fallback to Google's receiver included.
const RETRY_DELAY: Duration = Duration::from_secs(2);
const SEARCHING: &str = "Searching for Chromecast devices, please wait...";

fn main() -> anyhow::Result<()> {
    // RUST_LOG prints to stderr. The menu owns the terminal: it logs to a file instead, always,
    // so a problem seen in the menu can be read afterwards.
    let verbose = std::env::var_os("RUST_LOG").is_some();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("sccast=info")))
        .with_ansi(false)
        .with_writer(move || -> Box<dyn Write> {
            if MENU_LOG.load(Ordering::Relaxed) {
                let file = std::fs::OpenOptions::new().create(true).append(true).open(log_path());
                file.map_or_else(|_| Box::new(std::io::sink()) as Box<dyn Write>, |f| Box::new(f))
            } else if verbose {
                Box::new(std::io::stderr())
            } else {
                Box::new(std::io::sink())
            }
        })
        .init();

    // `sccast` alone opens the cast menu.
    let cli = if std::env::args_os().len() == 1 { Cli::parse_from(["sccast", "cast"]) } else { Cli::parse() };
    match cli.command {
        Command::List { timeout } => {
            let devices = spin(SEARCHING, || discovery::discover(Duration::from_secs(timeout)))?;
            if devices.is_empty() {
                bail!("no Chromecast found (check that mDNS/UDP 5353 is not blocked)");
            }
            for d in devices {
                println!("{}\t{}\t{}:{}\t{}", d.name, d.model, d.ip, d.port, d.id);
            }
            Ok(())
        }
        Command::Cast { device, url, file, audio_delay, receiver, latency, stream } => {
            if let Some(file) = &file {
                if !file.is_file() {
                    bail!("{} is not a file", file.display());
                }
            }
            let settings = menu::Settings { stream, audio_delay, latency, receiver };
            // In a terminal without --device, the menu picks the device and runs the casts.
            let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
            if device.is_none() && url.is_none() && file.is_none() && interactive {
                return run_menu(settings);
            }
            // Find the device before the portal dialog, so a typo fails fast.
            let device = pick_device(device.as_deref())?;
            let stop = stop_flag()?;
            let out = Out::Term;
            let say = |r: cast::Report| out.report(r);
            let port = settings.stream.port;

            if let Some(url) = url {
                return launch(&device, &settings.receiver, false, stop, &out)?
                    .play(&url, "video/mp4", false, stop, &NO_KEEP, &say);
            }
            if let Some(file) = file {
                let ip = server::lan_ip(device.ip.into())?;
                let _server = server::serve_file(&file, port, stop)?;
                let receiver = launch(&device, &settings.receiver, false, stop, &out)?;
                return receiver.play(&format!("http://{ip}:{port}/media"), "video/mp4", false, stop, &NO_KEEP, &say);
            }
            let screen = portal::ScreenCast::open()?;
            cast_screen(&device, &settings, &screen, stop, None, &out)
        }
        Command::Stop { device, receiver: app_id } => {
            let device = pick_device(device.as_deref())?;
            if !cast::stop(&device, &app_id, None)? {
                println!("nothing to stop on {}", device.name);
            }
            Ok(())
        }
        Command::Serve { test_pattern, stream } => {
            let screen = if test_pattern { None } else { Some(portal::ScreenCast::open()?) };
            let stop = stop_flag()?;
            let hls = capture::Capture::start(screen.as_ref(), &stream, Duration::ZERO)?;
            let (path, _, _server) = serve(&hls, &stream, stop)?;
            println!("http://localhost:{}/{path}", stream.port);
            while !stop.load(Ordering::SeqCst) {
                hls.poll(Duration::from_millis(200))?;
            }
            tracing::info!("stopping");
            Ok(())
        }
    }
}

/// Serve the capture over HTTP until `stop`; return the stream path, its content type and the server thread.
fn serve(
    capture: &capture::Capture,
    stream: &cli::Stream,
    stop: &'static AtomicBool,
) -> anyhow::Result<(&'static str, &'static str, JoinHandle<()>)> {
    Ok(match capture.tcp_port {
        Some(source) => (capture::MKV, "video/x-matroska", server::serve_mkv(source, stream.port, stop)?),
        None => (capture::PLAYLIST, "application/x-mpegURL", server::serve_hls(capture.dir(), stream.port, stop)?),
    })
}

/// Where cast progress goes: the terminal, with spinners, or the menu.
#[derive(Clone)]
enum Out {
    Term,
    Menu(Sender<menu::Status>),
}

impl Out {
    fn say(&self, msg: &str) {
        match self {
            Out::Term => eprintln!("{msg}"),
            Out::Menu(_) => self.send(menu::Status::Info(msg.to_string())),
        }
    }

    /// Menu only: the terminal shows messages, not stats.
    fn send(&self, status: menu::Status) {
        if let Out::Menu(tx) = self {
            let _ = tx.send(status);
        }
    }

    fn report(&self, r: cast::Report) {
        match r {
            cast::Report::Say(m) => self.say(m),
            cast::Report::Player(state) => self.send(menu::Status::Player(format!("{state:?}"))),
            cast::Report::Reconnecting => self.send(menu::Status::Reconnecting),
            cast::Report::Loaded(..) => {}
        }
    }

    /// Run `f`, showing `msg` while it runs.
    fn step<T>(&self, msg: &str, f: impl FnOnce() -> T) -> T {
        match self {
            Out::Term => spin(msg, f),
            Out::Menu(_) => {
                self.say(msg);
                f()
            }
        }
    }
}

/// Capture `screen`, serve it and play it on `device` until `stop` or the end of the cast.
/// Frees the HTTP port before returning, so the next cast can bind it.
/// With `keep` (menu), reuse a receiver left open and leave it open when `keep` is set at the stop.
fn cast_screen(
    device: &discovery::CastDevice,
    s: &menu::Settings,
    screen: &portal::ScreenCast,
    stop: &'static AtomicBool,
    keep: Option<&'static AtomicBool>,
    out: &Out,
) -> anyhow::Result<()> {
    let reuse = keep.is_some();
    let keep = keep.unwrap_or(&NO_KEEP);
    tracing::info!(name = %device.name, ip = %device.ip, "casting");
    let stream = &s.stream;
    let ip = server::lan_ip(device.ip.into())?;
    let hls = capture::Capture::start(Some(screen), stream, Duration::from_millis(s.audio_delay))?;
    let (path, content_type, server) = serve(&hls, stream, stop)?;
    if stream.format == cli::Format::Hls {
        out.send(menu::Status::Segments(hls.dir().to_path_buf()));
    }
    if let Some(served) = hls.served() {
        out.send(menu::Status::Served(Box::new(served)));
    }
    out.send(menu::Status::Dropped(Arc::clone(&hls.dropped)));
    if let Out::Menu(_) = out {
        watch_round_trip(device, stop, out.clone());
    }
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
        // The query is read by sccast's receiver page; the server ignores it.
        // kbps lets the page tell a network too slow for the stream.
        let mut query = vec![format!("kbps={}", stream.bitrate)];
        if let Some(latency) = s.latency {
            query.push(format!("latency={latency}"));
        }
        if cfg!(debug_assertions) {
            query.push("debug".to_string());
        }
        let url = format!("http://{ip}:{}/{path}?{}", stream.port, query.join("&"));
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

/// Every 5 s until `stop`, time a TCP connection to the TV's Cast port: a congested network
/// shows there (the school's Chromecast answered in 170 to 500 ms, the test TV in 2 ms).
fn watch_round_trip(device: &discovery::CastDevice, stop: &'static AtomicBool, out: Out) {
    let address = std::net::SocketAddr::from((device.ip, device.port));
    std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            let started = Instant::now();
            let answered = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2)).is_ok();
            out.send(menu::Status::RoundTrip(answered.then(|| started.elapsed())));
            for _ in 0..50 {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    });
}

/// Run the menu: each Enter (re)starts a cast in a thread, the menu stays on screen.
fn run_menu(mut settings: menu::Settings) -> anyhow::Result<()> {
    // Ctrl+C is a key in the menu; the flag still catches SIGTERM.
    let quit = stop_flag()?;
    // The screen first, before the menu takes the terminal.
    let screen = portal::ScreenCast::open()?;
    // A fresh log for each run.
    let _ = std::fs::remove_file(log_path());
    MENU_LOG.store(true, Ordering::Relaxed);
    let mut terminal = ratatui::init();
    let result = drive_menu(&mut terminal, &mut settings, screen, quit);
    // Not restore(): its eprintln! panics when the terminal was closed.
    let _ = ratatui::try_restore();
    result
}

fn drive_menu(
    terminal: &mut ratatui::DefaultTerminal,
    s: &mut menu::Settings,
    screen: portal::ScreenCast,
    quit: &'static AtomicBool,
) -> anyhow::Result<()> {
    let mut menu = menu::Menu::new();
    // Chosen before the menu, kept for every cast; p replaces it.
    let screen = Arc::new(Mutex::new(Arc::new(screen)));
    // Stop and keep flags of the running cast, and its thread.
    let mut running: Option<(&'static AtomicBool, &'static AtomicBool, JoinHandle<()>)> = None;
    // TV where the receiver may be open, and its app id.
    let mut tv: Option<(discovery::CastDevice, String)> = None;
    // Error of a terminal closed without quitting: returned once the TV is freed.
    let mut lost = None;
    loop {
        let action = match menu.next(terminal, s, quit) {
            Ok(action) => action,
            Err(e) => {
                tracing::warn!("terminal lost: {e:#}");
                lost = Some(e);
                menu::Action::Quit
            }
        };
        if let menu::Action::PickScreen = action {
            // In a thread: the menu keeps answering while the dialog is open. The cast goes on meanwhile.
            let out = Out::Menu(menu.current_sender());
            let slot = Arc::clone(&screen);
            std::thread::spawn(move || {
                out.say("Choose the screen in the dialog...");
                match portal::ScreenCast::open() {
                    Ok(opened) => {
                        *slot.lock().unwrap() = Arc::new(opened);
                        out.say("Screen changed: Enter casts it.");
                    }
                    // Not an Error status: that would end the stats of the running cast.
                    Err(e) => out.say(&format!("Screen not changed: {e:#}")),
                }
            });
            continue;
        }
        // The receiver stays open on the TV between casts to the same TV, and after s:
        // the Sharp TV sometimes never answered a launch right after a close.
        let same_tv = matches!((&action, &tv), (menu::Action::Cast(d), Some((open, _))) if d.id == open.id);
        let keep_open = same_tv || matches!(action, menu::Action::Stop);
        if let Some((stop, keep, cast)) = running.take() {
            menu.set_status(menu::Status::Info("Stopping...".into()));
            // The terminal may be closed: freeing the TV comes first.
            let _ = menu.draw_now(terminal, s);
            keep.store(keep_open, Ordering::SeqCst);
            stop.store(true, Ordering::SeqCst);
            wait_for(&cast);
            menu.set_status(menu::Status::Ended);
        } else if !keep_open {
            // A receiver left open by s, on this TV or another one.
            if let Some((device, app_id)) = tv.take() {
                menu.set_status(menu::Status::Info(format!("Closing the receiver on {}...", device.name)));
                let _ = menu.draw_now(terminal, s);
                wait_for(&std::thread::spawn(move || drop(cast::stop(&device, &app_id, None))));
            }
        }
        if !keep_open {
            tv = None;
        }
        let device = match action {
            menu::Action::Quit => return lost.map_or(Ok(()), Err),
            menu::Action::Stop | menu::Action::PickScreen => continue,
            menu::Action::Cast(device) => device,
        };
        // ponytail: two leaked flags per cast (a few bytes), play() wants 'static flags.
        let stop: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let keep: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let out = Out::Menu(menu.status_sender());
        let settings = s.clone();
        let screen = Arc::clone(&screen.lock().unwrap());
        menu.set_casting(Some(device.id.clone()));
        tv = Some((device.clone(), settings.receiver.clone()));
        let cast = std::thread::spawn(move || match cast_screen(&device, &settings, &screen, stop, Some(keep), &out) {
            Ok(()) => out.send(menu::Status::Ended),
            // The sharing may have been stopped from the desktop.
            Err(e) if format!("{e:#}").contains("capture") => {
                out.send(menu::Status::Error(format!("{e:#} (p: choose the screen again)")))
            }
            Err(e) => out.send(menu::Status::Error(format!("{e:#}"))),
        });
        running = Some((stop, keep, cast));
    }
}

/// Wait for `thread`, at most 5 s: the TV can stop answering.
fn wait_for<T>(thread: &JoinHandle<T>) {
    let started = Instant::now();
    while !thread.is_finished() && started.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Flag set by Ctrl+C or SIGTERM. The handler can be installed only once per process.
fn stop_flag() -> anyhow::Result<&'static AtomicBool> {
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
fn spin<T>(message: &str, f: impl FnOnce() -> T) -> T {
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

/// Start the receiver on `device`, telling the user when sccast's own fell back to Google's.
/// With `reuse` (the menu), try again without limit: the first launch on a Chromecast can fail,
/// the user sees each try and can stop.
fn launch(
    device: &discovery::CastDevice,
    app_id: &str,
    reuse: bool,
    stop: &'static AtomicBool,
    out: &Out,
) -> anyhow::Result<cast::Receiver> {
    let mut tries = 1;
    let receiver = loop {
        let label = match tries {
            1 => format!("Starting the receiver on {}...", device.name),
            n => format!("Starting the receiver on {} (try {n})...", device.name),
        };
        match out.step(&label, || cast::launch(device, app_id, reuse, stop)) {
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

/// Resolve `--device`, or let the user pick one from the devices found.
fn pick_device(query: Option<&str>) -> anyhow::Result<discovery::CastDevice> {
    if let Some(query) = query {
        let found = spin(&format!("Looking for {query}..."), || discovery::find(query, DISCOVERY_TIMEOUT))?;
        if let Some(device) = found {
            return Ok(device);
        }
    }
    let mut devices = spin(SEARCHING, || discovery::discover(DISCOVERY_TIMEOUT))?;
    if devices.is_empty() {
        bail!("no Chromecast found (check that mDNS/UDP 5353 is not blocked)");
    }
    if let Some(q) = query {
        let names: Vec<_> = devices.iter().map(|d| d.name.as_str()).collect();
        bail!("device {q:?} not found; available: {}", names.join(", "));
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
