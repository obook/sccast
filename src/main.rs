mod capture;
mod cast;
mod cli;
mod discovery;
mod portal;
mod server;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::bail;
use clap::Parser;
use cli::{Cli, Command};
use tracing_subscriber::EnvFilter;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(8);
// Fixed so a firewall rule can target it; becomes --port in J8.
const HTTP_PORT: u16 = 8090;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    match Cli::parse().command {
        Command::List { timeout } => {
            let devices = discovery::discover(Duration::from_secs(timeout))?;
            if devices.is_empty() {
                bail!("no Chromecast found (check that mDNS/UDP 5353 is not blocked)");
            }
            for d in devices {
                println!("{}\t{}\t{}:{}\t{}", d.name, d.model, d.ip, d.port, d.id);
            }
            Ok(())
        }
        Command::Cast { device, url, file } => {
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

            if let Some(url) = url {
                return cast::play_url(&device, &url, "video/mp4", false, stop);
            }
            if let Some(file) = file {
                server::serve_file(&file, HTTP_PORT)?;
                return cast::play_url(&device, &format!("http://{ip}:{HTTP_PORT}/media"), "video/mp4", false, stop);
            }

            let hls = capture::Hls::start(Some(portal::ScreenCast::open()?))?;
            server::serve_hls(hls.dir(), HTTP_PORT)?;
            while hls.segments()? < capture::MIN_SEGMENTS {
                if stop.load(Ordering::SeqCst) {
                    return Ok(());
                }
                hls.poll(Duration::from_millis(100))?;
            }
            // ponytail: pipeline errors are not watched while casting; poll the bus
            // from a thread if a capture failure ever leaves the TV frozen.
            let url = format!("http://{ip}:{HTTP_PORT}/{}", capture::PLAYLIST);
            cast::play_url(&device, &url, "application/x-mpegURL", true, stop)
        }
        Command::Stop { device } => {
            let device = pick_device(device.as_deref())?;
            if !cast::stop(&device, None)? {
                println!("nothing to stop on {}", device.name);
            }
            Ok(())
        }
        Command::Serve { test_pattern } => {
            let screen = if test_pattern { None } else { Some(portal::ScreenCast::open()?) };
            let stop = stop_flag()?;
            let hls = capture::Hls::start(screen)?;
            server::serve_hls(hls.dir(), HTTP_PORT)?;
            println!("http://localhost:{HTTP_PORT}/{}", capture::PLAYLIST);
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

/// Resolve `--device`, or fail with the list of devices to choose from.
fn pick_device(query: Option<&str>) -> anyhow::Result<discovery::CastDevice> {
    if let Some(query) = query {
        if let Some(device) = discovery::find(query, DISCOVERY_TIMEOUT)? {
            return Ok(device);
        }
    }
    let names: Vec<_> = discovery::discover(DISCOVERY_TIMEOUT)?
        .into_iter()
        .map(|d| d.name)
        .collect();
    match query {
        Some(q) => bail!("device {q:?} not found; available: {}", names.join(", ")),
        None => bail!("no --device given; available: {}", names.join(", ")),
    }
}
