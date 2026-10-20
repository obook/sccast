//! mirror/
//! Mirror format (`--format mirror`): H.264 and Opus over encrypted RTP to Google's
//! mirroring receiver, about 150 ms behind the screen instead of 2 to 3 s with HLS.
//! offer.rs opens the session, send.rs runs its threads, rtp.rs builds the packets,
//! rtcp.rs reads the receiver's reports, rate.rs adapts the bitrate.
//! Protocol from openscreen/cast/streaming/impl. Notes in docs/cast-streaming.md.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

mod offer;
mod rate;
mod rtcp;
mod rtp;
mod send;

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use gstreamer as gst;
use gstreamer::prelude::*;
use rust_cast::channels::connection::ConnectionResponse;
use rust_cast::channels::heartbeat::HeartbeatResponse;
use rust_cast::channels::media::MediaResponse;
use rust_cast::channels::receiver::ReceiverResponse;
use rust_cast::{CastDevice, ChannelMessage};

use crate::capture::Capture;
use crate::capture::ScreenCast;
use crate::device::{self as cast, CastDevice as Device};
use crate::menu;
use crate::session::Out;
use offer::{offer, wait_answer};
use rtp::{Keys, RtpStream};
use send::{feedback, send_frames};

/// Google's mirroring receiver ("Chrome Mirroring").
pub const MIRRORING_RECEIVER: &str = "0F5096E8";
const NAMESPACE: &str = "urn:x-cast:com.google.cast.webrtc";
/// Receiver's playout delay. ponytail: fixed; Chrome uses 400 ms, a lossy Wi-Fi may need more to resend in time.
const TARGET_DELAY_MS: u32 = 100;
const VIDEO_SSRC: u32 = 0x5cca_5701;
const AUDIO_SSRC: u32 = 0x5cca_5702;
// openscreen's RtpPayloadType values.
const H264_PAYLOAD: u8 = 101;
const OPUS_PAYLOAD: u8 = 96;
const AUDIO_BITRATE: u32 = 128_000;
const REPORT_INTERVAL: Duration = Duration::from_millis(100);
/// The receiver repeats its key frame request in each report until one arrives.
const KEY_FRAME_SPACING: Duration = Duration::from_secs(1);

/// Streams of a running session and the socket they share.
struct Session {
    socket: UdpSocket,
    video: Mutex<RtpStream>,
    audio: Option<Mutex<RtpStream>>,
    pipeline: gst::Element,
    /// Wall clock at running time 0, for the Sender Reports.
    wall_start: Duration,
}

impl Session {
    fn stream(&self, ssrc: u32) -> Option<&Mutex<RtpStream>> {
        match ssrc {
            VIDEO_SSRC => Some(&self.video),
            AUDIO_SSRC => self.audio.as_ref(),
            _ => None,
        }
    }

    fn running(&self) -> Duration {
        running_time(&self.pipeline)
    }

    fn send_reports(&self) {
        let running = self.running();
        let wall = self.wall_start + running;
        for stream in std::iter::once(&self.video).chain(&self.audio) {
            let report = stream.lock().unwrap().sender_report(wall, running);
            let _ = self.socket.send(&report);
        }
    }

    /// Payload bytes sent so far, both streams.
    fn octets(&self) -> u64 {
        let video = self.video.lock().unwrap().total_octets;
        let audio = self.audio.as_ref().map_or(0, |a| a.lock().unwrap().total_octets);
        video + audio
    }
}

fn running_time(pipeline: &gst::Element) -> Duration {
    pipeline.current_running_time().map_or(Duration::ZERO, Duration::from)
}

/// Mirror `screen` to `device` until `stop`, or until the TV is taken or lost.
/// The mirroring app is always closed at the end: `keep` of the menu does not apply.
pub fn cast(device: &Device, s: &menu::Settings, screen: &ScreenCast, stop: &'static AtomicBool, out: &Out) -> anyhow::Result<()> {
    let receiver = out.step(&format!("Starting the mirroring receiver on {}...", device.name), || {
        cast::launch(device, MIRRORING_RECEIVER, false, stop)
    })?;
    if receiver.app_id() != MIRRORING_RECEIVER {
        receiver.close();
        bail!("{} has no mirroring receiver: use the HLS format", device.name);
    }
    let session_id = receiver.session_id().to_string();
    // receive() blocks until the next message (up to ~5 s): close the app from a second
    // connection so a stop frees the TV at once.
    let closer = {
        let device = device.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(100));
            }
            if let Err(e) = cast::stop(&device, MIRRORING_RECEIVER, Some(&session_id)) {
                tracing::warn!("cannot close the mirroring receiver: {e:#}");
            }
        })
    };
    let result = stream(receiver.channel(), device, s, screen, stop, out);
    stop.store(true, Ordering::SeqCst);
    let _ = closer.join();
    result
}

fn stream(cast: &CastDevice<'static>, device: &Device, s: &menu::Settings, screen: &ScreenCast, stop: &'static AtomicBool, out: &Out) -> anyhow::Result<()> {
    let video_keys = Keys::random()?;
    let audio_keys = if s.stream.no_audio { None } else { Some(Keys::random()?) };
    cast.receiver.broadcast_message(NAMESPACE, &offer(&s.stream, &video_keys, audio_keys.as_ref()))?;
    let Some(answer) = out.step("Waiting for the TV's answer...", || wait_answer(cast, stop))? else {
        // Stopped by the user meanwhile.
        return Ok(());
    };
    tracing::info!(?answer, "mirroring accepted");
    if !answer.video {
        bail!("{} refused the H.264 video: try a lower resolution", device.name);
    }
    // The capture starts once the TV is ready: frames would pile up before.
    let mut settings = s.stream.clone();
    settings.no_audio = !answer.audio;
    let capture = Capture::start(Some(screen), &settings, Duration::from_millis(s.audio_delay))?;
    let pipeline = capture.pipeline();
    // Before PLAYING the running time reads 0, which would date every frame too early.
    let _ = pipeline.state(gst::ClockTime::from_seconds(2));
    let wall_start = SystemTime::now().duration_since(UNIX_EPOCH)?.saturating_sub(running_time(&pipeline));
    let socket = UdpSocket::bind(("0.0.0.0", 0))?;
    socket.connect((device.ip, answer.port))?;
    socket.set_read_timeout(Some(REPORT_INTERVAL))?;
    let audio = match audio_keys {
        Some(keys) if answer.audio => Some(Mutex::new(RtpStream::new(AUDIO_SSRC, OPUS_PAYLOAD, 48_000, keys))),
        _ => None,
    };
    let session = Arc::new(Session {
        socket,
        video: Mutex::new(RtpStream::new(VIDEO_SSRC, H264_PAYLOAD, 90_000, video_keys)),
        audio,
        pipeline,
        wall_start,
    });
    // The receiver drops packets that arrive before the first Sender Report.
    session.send_reports();

    let encoder = capture.element("venc").context("no video encoder")?;
    let mut threads = vec![send_frames(&session, &capture, "sink", true, stop)?];
    if session.audio.is_some() {
        threads.push(send_frames(&session, &capture, "asink", false, stop)?);
    }
    threads.push(feedback(&session, encoder, s.stream.bitrate, stop));
    let watcher = capture.watch(stop)?;

    // Segment times of an earlier HLS cast would show in the stats.
    crate::hls::forget_fetches();
    out.send(menu::Status::Dropped(Arc::clone(&capture.dropped)));
    // Weak: the menu keeps the counter after the cast, the socket and packets must not stay.
    let counted = Arc::downgrade(&session);
    out.send(menu::Status::Served(Box::new(move || counted.upgrade().map_or(0, |s| s.octets()))));
    out.say(&format!("Mirroring to {}. Ctrl+C stops.", device.name));
    let ended = control(cast, stop, out);
    stop.store(true, Ordering::SeqCst);
    for thread in threads {
        let _ = thread.join();
    }
    // A capture failure explains why the cast stopped: report it first.
    watcher.join().expect("capture watcher panicked")?;
    ended
}

/// Watch the control channel until `stop`, the TV taken by another sender, or the connection lost.
fn control(cast: &CastDevice<'static>, stop: &AtomicBool, out: &Out) -> anyhow::Result<()> {
    while !stop.load(Ordering::SeqCst) {
        let message = match cast.receive() {
            Ok(message) => message,
            // rust_cast may not parse every status of the mirroring app.
            Err(e @ (rust_cast::errors::Error::Parsing(_) | rust_cast::errors::Error::Serialization(_))) => {
                tracing::debug!("unreadable message from the device: {e}");
                continue;
            }
            Err(e) if stop.load(Ordering::SeqCst) => {
                tracing::debug!("connection closed at the stop: {e}");
                return Ok(());
            }
            Err(e) => bail!("lost the TV: {e}"),
        };
        match message {
            ChannelMessage::Heartbeat(HeartbeatResponse::Ping) => cast.heartbeat.pong()?,
            ChannelMessage::Connection(ConnectionResponse::Close) if !stop.load(Ordering::SeqCst) => {
                bail!("the TV closed the mirroring session")
            }
            ChannelMessage::Media(MediaResponse::Status(status)) => {
                for entry in &status.entries {
                    out.report(cast::Report::Player(&entry.player_state));
                }
            }
            // The app answered the OFFER: a status without it means it was closed or replaced.
            ChannelMessage::Receiver(ReceiverResponse::Status(status))
                if !status.applications.iter().any(|a| a.app_id == MIRRORING_RECEIVER) =>
            {
                tracing::info!("mirroring receiver closed");
                return Ok(());
            }
            _ => {}
        }
    }
    Ok(())
}
