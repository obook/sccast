mod capture;
mod cast;
mod cli;
mod discovery;
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
            if url.is_none() && file.is_none() {
                bail!("screen capture is not implemented yet (J5): use --url or --file");
            }
            if let Some(file) = &file {
                if !file.is_file() {
                    bail!("{} is not a file", file.display());
                }
            }
            let device = pick_device(device.as_deref())?;
            let url = match (url, file) {
                (Some(url), _) => url,
                (None, Some(file)) => {
                    server::serve_file(&file, HTTP_PORT)?;
                    let ip = server::lan_ip(device.ip.into())?;
                    format!("http://{ip}:{HTTP_PORT}/media")
                }
                (None, None) => unreachable!(),
            };
            tracing::info!(name = %device.name, ip = %device.ip, "casting");
            cast::play_url(&device, &url, "video/mp4", stop_flag()?)
        }
        Command::Serve { test_pattern } => {
            if !test_pattern {
                bail!("screen capture is not implemented yet (J5): use --test-pattern");
            }
            let stop = stop_flag()?;
            let hls = capture::Hls::start()?;
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
