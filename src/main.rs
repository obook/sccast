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
        Command::Cast { device, url, file, audio_delay, receiver: app_id, stream } => {
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

            let launching = format!("Starting the receiver on {}...", device.name);
            if let Some(url) = url {
                return spin(&launching, || cast::launch(&device, &app_id))?.play(&url, "video/mp4", false, stop);
            }
            if let Some(file) = file {
                server::serve_file(&file, port)?;
                let receiver = spin(&launching, || cast::launch(&device, &app_id))?;
                return receiver.play(&format!("http://{ip}:{port}/media"), "video/mp4", false, stop);
            }

            let screen = portal::ScreenCast::open()?;
            let hls = capture::Hls::start(Some(screen), &stream, Duration::from_millis(audio_delay))?;
            server::serve_hls(hls.dir(), port)?;
            // The capture fills segments while the TV starts its receiver (2-3 s): both waits overlap.
            let receiver = spin(&launching, || cast::launch(&device, &app_id))?;
            let prepared = spin("Preparing the stream...", || -> anyhow::Result<()> {
                while hls.segments()? < capture::MIN_SEGMENTS && !stop.load(Ordering::SeqCst) {
                    hls.poll(Duration::from_millis(100))?;
                }
                Ok(())
            });
            if prepared.is_err() || stop.load(Ordering::SeqCst) {
                receiver.close();
                return prepared;
            }
            let capture = hls.watch(stop)?;
            let url = format!("http://{ip}:{port}/{}", capture::PLAYLIST);
            let played = receiver.play(&url, "application/x-mpegURL", true, stop);
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
            let hls = capture::Hls::start(screen, &stream, Duration::ZERO)?;
            server::serve_hls(hls.dir(), stream.port)?;
            println!("http://localhost:{}/{}", stream.port, capture::PLAYLIST);
            while !stop.load(Ordering::SeqCst) {
                hls.poll(Duration::from_millis(200))?;
            }
            tracing::info!("stopping");
            Ok(())
        }
    }
}

/// Flag set by Ctrl+C or SIGTERM. The handler can be installed only once per process.
fn stop_flag() -> anyhow::Result<&'static AtomicBool> {
    static STOP: AtomicBool = AtomicBool::new(false);
    ctrlc::set_handler(|| STOP.store(true, Ordering::SeqCst))?;
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
            for frame in ["·", "✢", "✳", "✶", "✻", "✽", "✻", "✶", "✳", "✢"].iter().cycle() {
                if done.load(Ordering::Relaxed) {
                    break;
                }
                eprint!("\r{frame} {message}");
                std::thread::sleep(Duration::from_millis(120));
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
