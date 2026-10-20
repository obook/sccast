mod capture;
mod cast;
mod cli;
mod discovery;
mod portal;
mod server;

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::bail;
use clap::Parser;
use cli::{Cli, Command};
use tracing_subscriber::EnvFilter;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(8);
const SEARCHING: &str = "Searching for Chromecast devices, please wait...";

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    match Cli::parse().command {
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
        Command::Cast { device, url, file, audio_delay, receiver: app_id, latency, stream } => {
            if let Some(file) = &file {
                if !file.is_file() {
                    bail!("{} is not a file", file.display());
                }
            }
            // Find the device before the portal dialog, so a typo fails fast.
            let device = pick_device(device.as_deref())?;
            tracing::info!(name = %device.name, ip = %device.ip, "casting");
            let stop = stop_flag()?;
            let ip = server::lan_ip(device.ip.into())?;
            let port = stream.port;

            if let Some(url) = url {
                return launch(&device, &app_id)?.play(&url, "video/mp4", false, stop);
            }
            if let Some(file) = file {
                server::serve_file(&file, port)?;
                let receiver = launch(&device, &app_id)?;
                return receiver.play(&format!("http://{ip}:{port}/media"), "video/mp4", false, stop);
            }

            let screen = portal::ScreenCast::open()?;
            let hls = capture::Capture::start(Some(screen), &stream, Duration::from_millis(audio_delay))?;
            let (path, content_type) = serve(&hls, &stream)?;
            // The capture fills segments while the TV starts its receiver (2-3 s): both waits overlap.
            let receiver = launch(&device, &app_id)?;
            let prepared = spin("Preparing the stream...", || -> anyhow::Result<()> {
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
            if let Some(latency) = latency {
                query.push(format!("latency={latency}"));
            }
            if cfg!(debug_assertions) {
                query.push("debug".to_string());
            }
            let mut url = format!("http://{ip}:{port}/{path}");
            if !query.is_empty() {
                url = format!("{url}?{}", query.join("&"));
            }
            let played = receiver.play(&url, content_type, true, stop);
            stop.store(true, Ordering::SeqCst);
            // A capture failure explains why the cast stopped: report it first.
            capture.join().expect("capture watcher panicked")?;
            played
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
            let hls = capture::Capture::start(screen, &stream, Duration::ZERO)?;
            let (path, _) = serve(&hls, &stream)?;
            println!("http://localhost:{}/{path}", stream.port);
            while !stop.load(Ordering::SeqCst) {
                hls.poll(Duration::from_millis(200))?;
            }
            tracing::info!("stopping");
            Ok(())
        }
    }
}

/// Serve the capture over HTTP; return the stream path and its content type.
fn serve(capture: &capture::Capture, stream: &cli::Stream) -> anyhow::Result<(&'static str, &'static str)> {
    match capture.tcp_port {
        Some(source) => {
            server::serve_mkv(source, stream.port)?;
            Ok((capture::MKV, "video/x-matroska"))
        }
        None => {
            server::serve_hls(capture.dir(), stream.port)?;
            Ok((capture::PLAYLIST, "application/x-mpegURL"))
        }
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
fn launch(device: &discovery::CastDevice, app_id: &str) -> anyhow::Result<cast::Receiver> {
    let receiver = spin(&format!("Starting the receiver on {}...", device.name), || cast::launch(device, app_id))?;
    if receiver.app_id() != app_id {
        eprintln!("The sccast receiver did not start on {}: using Google's receiver (about 5 s of delay).", device.name);
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
