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
/// A normal launch takes 2 to 6 s, the fallback to Google's receiver included.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(20);
const SEARCHING: &str = "Searching for Chromecast devices, please wait...";

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
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
                return launch(&device, &settings.receiver, &out)?.play(&url, "video/mp4", false, stop, &say);
            }
            if let Some(file) = file {
                let ip = server::lan_ip(device.ip.into())?;
                let _server = server::serve_file(&file, port, stop)?;
                let receiver = launch(&device, &settings.receiver, &out)?;
                return receiver.play(&format!("http://{ip}:{port}/media"), "video/mp4", false, stop, &say);
            }
            let screen = portal::ScreenCast::open()?;
            cast_screen(&device, &settings, &screen, stop, &out)
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
fn cast_screen(
    device: &discovery::CastDevice,
    s: &menu::Settings,
    screen: &portal::ScreenCast,
    stop: &'static AtomicBool,
    out: &Out,
) -> anyhow::Result<()> {
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
    let played = (|| {
        // The capture fills segments while the TV starts its receiver (2-3 s): both waits overlap.
        let launched: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        watch_launch(launched, stop, hls.halter(), out.clone(), device.name.clone());
        let receiver = launch(device, &s.receiver, out);
        launched.store(true, Ordering::SeqCst);
        let receiver = receiver?;
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
            receiver.close();
            return prepared;
        }
        let capture = hls.watch(stop)?;
        // The query is read by sccast's receiver page; the server ignores it.
        let mut query = Vec::new();
        if let Some(latency) = s.latency {
            query.push(format!("latency={latency}"));
        }
        if cfg!(debug_assertions) {
            query.push("debug".to_string());
        }
        let mut url = format!("http://{ip}:{}/{path}", stream.port);
        if !query.is_empty() {
            url = format!("{url}?{}", query.join("&"));
        }
        let played = receiver.play(&url, content_type, true, stop, &|r| out.report(r));
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

/// The Sharp TV sometimes never answers a launch (black screen, then its home screen), and
/// rust_cast has no timeout. If `launched` is still false after LAUNCH_TIMEOUT, stop the capture
/// and the server, and report. ponytail: the blocked launch call stays behind, on its socket.
fn watch_launch(
    launched: &'static AtomicBool,
    stop: &'static AtomicBool,
    halt: impl FnOnce() + Send + 'static,
    out: Out,
    name: String,
) {
    std::thread::spawn(move || {
        let started = Instant::now();
        while !launched.load(Ordering::SeqCst) && started.elapsed() < LAUNCH_TIMEOUT {
            std::thread::sleep(Duration::from_millis(100));
        }
        if launched.load(Ordering::SeqCst) {
            return;
        }
        stop.store(true, Ordering::SeqCst);
        halt();
        let message = format!(
            "{name} did not start the receiver within {} s. Try again; if it fails again, restart the TV",
            LAUNCH_TIMEOUT.as_secs()
        );
        match out {
            Out::Term => {
                eprintln!("Error: {message}");
                std::process::exit(1);
            }
            Out::Menu(_) => out.send(menu::Status::Error(message)),
        }
    });
}

/// Run the menu: each Enter (re)starts a cast in a thread, the menu stays on screen.
fn run_menu(mut settings: menu::Settings) -> anyhow::Result<()> {
    // Ctrl+C is a key in the menu; the flag still catches SIGTERM.
    let quit = stop_flag()?;
    // The screen first, before the menu takes the terminal.
    let screen = portal::ScreenCast::open()?;
    let mut terminal = ratatui::init();
    let result = drive_menu(&mut terminal, &mut settings, screen, quit);
    ratatui::restore();
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
    let mut running: Option<(&'static AtomicBool, JoinHandle<()>)> = None;
    loop {
        let action = menu.next(terminal, s, quit)?;
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
        if let Some((stop, cast)) = running.take() {
            menu.set_status(menu::Status::Info("Stopping...".into()));
            menu.draw_now(terminal, s)?;
            stop.store(true, Ordering::SeqCst);
            // The TV can stop answering: stop waiting after 5 s.
            let started = Instant::now();
            while !cast.is_finished() && started.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(50));
            }
            menu.set_status(menu::Status::Ended);
        }
        let device = match action {
            menu::Action::Quit => return Ok(()),
            menu::Action::Stop | menu::Action::PickScreen => continue,
            menu::Action::Cast(device) => device,
        };
        // ponytail: one leaked flag per cast (a few bytes), play() wants a 'static flag.
        let stop: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let out = Out::Menu(menu.status_sender());
        let settings = s.clone();
        let screen = Arc::clone(&screen.lock().unwrap());
        menu.set_casting(Some(device.id.clone()));
        let cast = std::thread::spawn(move || match cast_screen(&device, &settings, &screen, stop, &out) {
            Ok(()) => out.send(menu::Status::Ended),
            // The sharing may have been stopped from the desktop.
            Err(e) if format!("{e:#}").contains("capture") => {
                out.send(menu::Status::Error(format!("{e:#} (p: choose the screen again)")))
            }
            Err(e) => out.send(menu::Status::Error(format!("{e:#}"))),
        });
        running = Some((stop, cast));
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
            eprintln!("The device did not answer: exiting.");
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

/// Resolve `--device`, or let the user pick one from the devices found.
/// Start the receiver on `device`, telling the user when sccast's own fell back to Google's.
fn launch(device: &discovery::CastDevice, app_id: &str, out: &Out) -> anyhow::Result<cast::Receiver> {
    let receiver = out.step(&format!("Starting the receiver on {}...", device.name), || cast::launch(device, app_id))?;
    if receiver.app_id() != app_id {
        out.say(&format!(
            "The sccast receiver did not start on {}: using Google's receiver (about 5 s of delay).",
            device.name
        ));
    }
    Ok(receiver)
}

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
