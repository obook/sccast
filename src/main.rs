//! main.rs
//! sccast: cast the screen and sound of a Linux Wayland desktop to a Chromecast.
//! This file sets up the log, reads the command line and runs the command:
//!   capture/ (PC side), device/ (TV side), hls/ and mirror/ (the two transports),
//!   menu/ (terminal interface), session/ (how a cast runs).
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

mod capture;
mod cli;
mod device;
mod hls;
mod menu;
mod mirror;
mod screenshare;
mod session;

use std::io::{IsTerminal, Write};
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::bail;
use clap::Parser;
use cli::{Cli, Command};
use session::{pick_device, spin, stop_flag, Out, NO_KEEP};
use tracing_subscriber::EnvFilter;

/// Set while the menu runs: logs go to this file, opened once.
static MENU_LOG: std::sync::OnceLock<std::fs::File> = std::sync::OnceLock::new();

/// Menu log, on the per-user tmpfs (private, cleared at logout).
fn log_path() -> std::path::PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir, Into::into).join("sccast.log")
}

fn main() -> anyhow::Result<()> {
    // RUST_LOG prints to stderr. The menu owns the terminal: it logs to a file instead, always,
    // so a problem seen in the menu can be read afterwards.
    let verbose = std::env::var_os("RUST_LOG").is_some();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("sccast=info")))
        .with_ansi(false)
        .with_writer(move || -> Box<dyn Write> {
            if let Some(file) = MENU_LOG.get() {
                Box::new(file)
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
            let devices = spin(session::SEARCHING, || device::discovery::discover(Duration::from_secs(timeout)))?;
            if devices.is_empty() {
                bail!("{}", device::discovery::NONE_FOUND);
            }
            for d in devices {
                println!("{}\t{}\t{}:{}\t{}", d.name, d.model, d.ip, d.port, d.id);
            }
            Ok(())
        }
        Command::Cast { device, url, file, audio_delay, receiver, latency, host, pin, touchback, stream } => {
            if let Some(file) = &file {
                if !file.is_file() {
                    bail!("{} is not a file", file.display());
                }
            }
            let settings = menu::Settings { stream, audio_delay, latency, receiver, pin, touchback };
            // A panel given by address or by its code, without discovery.
            if settings.stream.format == cli::Format::Screenshare && device.is_none() {
                let host = match (host, settings.pin.as_deref()) {
                    (Some(host), _) => host,
                    (None, Some(code)) => screenshare::pin::decode(code).map_err(anyhow::Error::msg)?.into(),
                    (None, None) => bail!("--format screenshare needs --pin CODE, --host IP:PORT or --device NAME"),
                };
                // Before the screen dialog: a wrong code fails in seconds, without a click.
                let host = spin(&format!("Looking for the panel at {}...", host.ip()), || screenshare::find(host))?;
                let screen = capture::ScreenCast::open(settings.touchback)?;
                return screenshare::cast(host, &host.to_string(), &settings, &screen, stop_flag()?, &Out::Term);
            }
            // In a terminal without --device, the menu picks the device and runs the casts.
            let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
            if device.is_none() && url.is_none() && file.is_none() && interactive {
                return session::run_menu(settings);
            }
            // Find the device before the portal dialog, so a typo fails fast.
            let device = pick_device(device.as_deref())?;
            let stop = stop_flag()?;
            let out = Out::Term;
            let say = |r: device::Report| out.report(r);
            let port = settings.stream.port;

            if let Some(url) = url {
                return session::launch(&device, &settings.receiver, false, stop, &out)?
                    .play(&url, "video/mp4", false, stop, &NO_KEEP, &say);
            }
            if let Some(file) = file {
                let ip = hls::lan_ip(device.ip.into())?;
                let _server = hls::serve_file(&file, port, stop)?;
                let receiver = session::launch(&device, &settings.receiver, false, stop, &out)?;
                return receiver.play(&format!("http://{ip}:{port}/media"), "video/mp4", false, stop, &NO_KEEP, &say);
            }
            let screen = capture::ScreenCast::open(settings.touchback)?;
            session::cast_screen(&device, &settings, &screen, stop, None, &out)
        }
        Command::Pin { code } => {
            let address = screenshare::pin::decode(&code).map_err(anyhow::Error::msg)?;
            let index = screenshare::pin::PORTS.iter().position(|p| *p == address.port()).unwrap_or_default();
            println!("{}\tport {} (index {index} of 0-3)", address.ip(), address.port());
            println!("protocol trace of the casts: {}", screenshare::trace::path().display());
            Ok(())
        }
        Command::Stop { device, receiver: app_id } => {
            let device = pick_device(device.as_deref())?;
            if !device::stop(&device, &app_id, None)? {
                println!("nothing to stop on {}", device.name);
            }
            Ok(())
        }
        Command::Serve { test_pattern, mut stream } => {
            // Mirror streams to a TV only, over RTP: serve plays the same screen in HLS.
            if matches!(stream.format, cli::Format::Mirror | cli::Format::Screenshare) {
                stream.format = cli::Format::Hls;
            }
            let screen = if test_pattern { None } else { Some(capture::ScreenCast::open(false)?) };
            let stop = stop_flag()?;
            let hls = capture::Capture::start(screen.as_ref(), &stream, 0)?;
            let (path, _, _server) = session::serve(&hls, &stream, stop)?;
            println!("http://localhost:{}/{path}", stream.port);
            while !stop.load(Ordering::SeqCst) {
                hls.poll(Duration::from_millis(200))?;
            }
            tracing::info!("stopping");
            Ok(())
        }
    }
}
