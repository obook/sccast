//! media.rs
//! The video and sound connections of a panel cast: one TCP connection each, frames written
//! as the capture's appsinks hand them out. See docs/screenshare.md.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context;
use gstreamer as gst;
use gstreamer::prelude::*;

use super::wire::{media_frame, split_config};
use crate::capture::Capture;

/// Connect to `port` on the panel. `timeout` also bounds each write: a panel that stops
/// reading must not hang the sender thread.
pub fn connect(host: SocketAddr, port: u16, timeout: Duration) -> anyhow::Result<TcpStream> {
    let address = SocketAddr::new(host.ip(), port);
    let socket = TcpStream::connect_timeout(&address, timeout).with_context(|| format!("cannot reach {address}"))?;
    socket.set_nodelay(true)?;
    socket.set_write_timeout(Some(timeout))?;
    Ok(socket)
}

/// Write each frame of appsink `name` to `socket` until `ended`. Video (the "sink" appsink):
/// SPS and PPS apart, type 0, `index` when the panel wants one. Audio: the type field carries
/// a clock in ms, as the Android sender does.
/// `sent` counts the bytes written, for the menu's bitrate.
pub fn send_frames(
    capture: &Capture,
    name: &'static str,
    mut socket: TcpStream,
    mut index: Option<i32>,
    sent: &Arc<AtomicU64>,
    ended: &'static AtomicBool,
) -> anyhow::Result<JoinHandle<()>> {
    let sink = capture.element(name).with_context(|| format!("no {name} in the pipeline"))?;
    let video = name == "sink";
    let sent = Arc::clone(sent);
    let started = Instant::now();
    Ok(std::thread::spawn(move || {
        while !ended.load(Ordering::SeqCst) {
            let Some(sample) = sink.emit_by_name::<Option<gst::Sample>>("try-pull-sample", &[&100_000_000u64]) else {
                // At the end of stream the pull returns at once: stop instead of spinning.
                if sink.property::<bool>("eos") {
                    break;
                }
                continue;
            };
            let Some(map) = sample.buffer().and_then(|b| b.map_readable().ok()) else { continue };
            let parts = if video { split_config(&map) } else { vec![map.as_slice()] };
            for part in parts {
                let kind = if video { 0 } else { started.elapsed().as_millis() as i32 };
                let frame = media_frame(index, kind, part);
                if let Err(e) = socket.write_all(&frame) {
                    tracing::warn!(name, "media connection: {e}");
                    ended.store(true, Ordering::SeqCst);
                    return;
                }
                sent.fetch_add(frame.len() as u64, Ordering::Relaxed);
                index = index.map(|i| i.wrapping_add(1));
            }
        }
    }))
}
