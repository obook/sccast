//! media.rs
//! The video and sound connections of a panel cast: one TCP connection each, frames written
//! as the capture's appsinks hand them out. TCP never loses a frame but queues it: on a
//! crowded Wi-Fi the queue would grow into seconds of delay. So the kernel buffer is kept
//! small, a frame is dropped while the previous one is still unsent, video then waits for the
//! next key frame, and the bitrate goes down (back up when calm). See docs/screenshare.md.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::io::{self, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context;
use gstreamer as gst;
use gstreamer::prelude::*;

use super::wire::{media_frame, split_config};
use crate::capture::{request_key_frame, Capture};
use crate::mirror::rate::{Rate, LOSS_LIMIT};

/// Kernel send buffer: about 0.2 s at 2500 kbit/s, so a slow network shows up here as unsent
/// bytes instead of delay hidden in the kernel.
const SEND_BUFFER: usize = 64 * 1024;
/// Unsent bytes stuck this long: the link is gone even if TCP has not said so.
const STALL: Duration = Duration::from_secs(10);

/// What the media threads sent and dropped, and why the first one to stop stopped.
#[derive(Clone, Default)]
pub struct Link {
    pub sent: Arc<AtomicU64>,
    pub dropped: Arc<AtomicU64>,
    lost: Arc<Mutex<Option<String>>>,
}

impl Link {
    fn lose(&self, why: String) {
        tracing::warn!(%why, "media connection lost");
        self.lost.lock().unwrap().get_or_insert(why);
    }

    pub fn lost(&self) -> Option<String> {
        self.lost.lock().unwrap().clone()
    }
}

/// A connection error in words, to tell from the log who cut the link.
pub fn cause(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::UnexpectedEof | io::ErrorKind::WriteZero => "closed by the panel (EOF)".into(),
        io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe => "reset by the panel or the network (RST)".into(),
        io::ErrorKind::ConnectionAborted => "aborted by the PC".into(),
        _ => e.to_string(),
    }
}

/// Connect to `port` on the panel, with a small send buffer, non-blocking.
pub fn connect(host: SocketAddr, port: u16, timeout: Duration) -> anyhow::Result<TcpStream> {
    let address = SocketAddr::new(host.ip(), port);
    let socket = TcpStream::connect_timeout(&address, timeout).with_context(|| format!("cannot reach {address}"))?;
    socket.set_nodelay(true)?;
    socket2::SockRef::from(&socket).set_send_buffer_size(SEND_BUFFER)?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}

#[derive(Debug, PartialEq)]
pub enum Admit {
    Send,
    Drop,
    /// Drop, and ask the encoder for a key frame: the decoder needs one after a gap.
    DropAndAskKeyFrame,
}

/// Drop frames while the previous one is unsent, then until the next key frame.
#[derive(Default)]
pub struct Gate {
    dropping: bool,
    asked: bool,
}

impl Gate {
    pub fn admit(&mut self, backlog: bool, key_frame: bool) -> Admit {
        if backlog {
            (self.dropping, self.asked) = (true, false);
            return Admit::Drop;
        }
        if !self.dropping || key_frame {
            self.dropping = false;
            return Admit::Send;
        }
        // A key frame asked while the backlog lasted would have been dropped too: ask now.
        if self.asked {
            Admit::Drop
        } else {
            self.asked = true;
            Admit::DropAndAskKeyFrame
        }
    }
}

/// Write what the socket takes now.
fn flush(socket: &mut TcpStream, pending: &mut Vec<u8>) -> io::Result<()> {
    while !pending.is_empty() {
        match socket.write(pending) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => drop(pending.drain(..n)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Write each frame of appsink `name` to `socket` until `ended`. Video (the "sink" appsink):
/// SPS and PPS apart, type 0, `index` when the panel wants one, bitrate adapted from
/// `bitrate` down. Audio: the type field carries a clock in ms, as the Android sender does.
pub fn send_frames(
    capture: &Capture,
    name: &'static str,
    mut socket: TcpStream,
    mut index: Option<i32>,
    bitrate: u32,
    link: &Link,
    ended: &'static AtomicBool,
) -> anyhow::Result<JoinHandle<()>> {
    let sink = capture.element(name).with_context(|| format!("no {name} in the pipeline"))?;
    let video = name == "sink";
    let encoder = if video { Some(capture.element("venc").context("no encoder in the pipeline")?) } else { None };
    let link = link.clone();
    let started = Instant::now();
    Ok(std::thread::spawn(move || {
        let (mut pending, mut gate, mut rate) = (Vec::new(), Gate::default(), Rate::new(bitrate));
        let mut stalled_since: Option<Instant> = None;
        while !ended.load(Ordering::SeqCst) {
            if let Err(e) = flush(&mut socket, &mut pending) {
                link.lose(format!("{name}: {}", cause(&e)));
                break;
            }
            stalled_since = if pending.is_empty() { None } else { stalled_since.or(Some(Instant::now())) };
            if stalled_since.is_some_and(|at| at.elapsed() > STALL) {
                link.lose(format!("{name}: nothing sent for {} s", STALL.as_secs()));
                break;
            }
            let Some(sample) = sink.emit_by_name::<Option<gst::Sample>>("try-pull-sample", &[&50_000_000u64]) else {
                // At the end of stream the pull returns at once: stop instead of spinning.
                if sink.property::<bool>("eos") {
                    break;
                }
                continue;
            };
            let Some(buffer) = sample.buffer() else { continue };
            let key_frame = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
            let admit = gate.admit(!pending.is_empty(), key_frame);
            // Video bitrate: one cut per dropping spell, back up after 10 s without one.
            let fraction_lost = if admit == Admit::DropAndAskKeyFrame { LOSS_LIMIT + 1 } else { 0 };
            if let Some(encoder) = &encoder {
                if admit == Admit::DropAndAskKeyFrame {
                    request_key_frame(encoder);
                }
                if let Some(kbps) = rate.adapt(fraction_lost, Instant::now()) {
                    tracing::info!(kbps, "video bitrate");
                    encoder.set_property("bitrate", kbps);
                }
            }
            if admit != Admit::Send {
                link.dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let Ok(map) = buffer.map_readable() else { continue };
            let parts = if video { split_config(&map) } else { vec![map.as_slice()] };
            for part in parts {
                let kind = if video { 0 } else { started.elapsed().as_millis() as i32 };
                let frame = media_frame(index, kind, part);
                link.sent.fetch_add(frame.len() as u64, Ordering::Relaxed);
                pending.extend_from_slice(&frame);
                index = index.map(|i| i.wrapping_add(1));
            }
        }
        ended.store(true, Ordering::SeqCst);
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_until_a_key_frame_after_a_backlog() {
        let mut gate = Gate::default();
        assert_eq!(gate.admit(false, false), Admit::Send);
        // Backlog: drop, even key frames.
        assert_eq!(gate.admit(true, false), Admit::Drop);
        assert_eq!(gate.admit(true, true), Admit::Drop);
        // Backlog gone: ask for a key frame once, drop until it comes.
        assert_eq!(gate.admit(false, false), Admit::DropAndAskKeyFrame);
        assert_eq!(gate.admit(false, false), Admit::Drop);
        assert_eq!(gate.admit(false, true), Admit::Send);
        assert_eq!(gate.admit(false, false), Admit::Send);
        // A new backlog asks again.
        gate.admit(true, false);
        assert_eq!(gate.admit(false, false), Admit::DropAndAskKeyFrame);
    }
}
