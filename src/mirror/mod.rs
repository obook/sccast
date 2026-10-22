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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
/// Receiver's playout delay at the start. rate.rs raises it up to 400 ms on a lossy network,
/// when the receiver accepts the adaptive delay extension.
const TARGET_DELAY_MS: u32 = 100;
const VIDEO_SSRC: u32 = 0x5cca_5701;
const AUDIO_SSRC: u32 = 0x5cca_5702;
// openscreen's RtpPayloadType values.
const H264_PAYLOAD: u8 = 101;
const OPUS_PAYLOAD: u8 = 96;
const AUDIO_BITRATE: u32 = 128_000;
const REPORT_INTERVAL: Duration = Duration::from_millis(100);
/// Offered again when the receiver refuses the video. No device seen refusing yet: a Nest Hub
/// refuses 1080p in HLS but takes it in mirror (2026-10-21).
const SMALLER_VIDEO: (u32, u32) = (1280, 720);
/// The receiver repeats its key frame request in each report until one arrives.
const KEY_FRAME_SPACING: Duration = Duration::from_secs(1);
/// No RTCP from the TV for this long: the session is lost. Longer before the first report,
/// as chromiacast does: the TV may take a while to set its media path up.
const SILENCE_START: Duration = Duration::from_secs(10);
const SILENCE: Duration = Duration::from_secs(5);

/// How a session ended.
enum Outcome {
    /// Stopped by the user, or the app closed on the TV (remote, another sender).
    Ended,
    /// TV unreachable or session dropped: worth launching the receiver again.
    Lost(String),
}

/// Streams of a running session and the socket they share.
struct Session {
    socket: UdpSocket,
    video: Mutex<RtpStream>,
    audio: Option<Mutex<RtpStream>>,
    pipeline: gst::Element,
    /// Wall clock at running time 0, for the Sender Reports.
    wall_start: Duration,
    /// Set when the TV stopped sending RTCP.
    silent: AtomicBool,
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
            let report = stream.lock().unwrap().sender_report(wall, running, Instant::now());
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

/// Mirror `screen` to `device` until `stop`, or until the TV is taken. A lost session is
/// started again, without limit from the menu, up to 3 times in 5 min from the terminal.
/// The mirroring app is always closed at the end: `keep` of the menu does not apply.
/// Ok(false) when the device has no mirroring receiver: the caller casts in HLS instead.
pub fn cast(device: &Device, s: &menu::Settings, screen: &ScreenCast, stop: &'static AtomicBool, out: &Out) -> anyhow::Result<bool> {
    let receiver = out.step(&format!("Starting the mirroring receiver on {}...", device.name), || {
        cast::launch(device, MIRRORING_RECEIVER, false, stop)
    })?;
    if receiver.app_id() != MIRRORING_RECEIVER {
        // launch() fell back to the Default Media Receiver.
        tracing::info!(app_id = receiver.app_id(), "no mirroring receiver");
        receiver.close();
        return Ok(false);
    }
    // Changes at each reconnection.
    let session_id = Arc::new(Mutex::new(receiver.session_id().to_string()));
    // receive() blocks until the next message (up to ~5 s): close the app from a second
    // connection so a stop frees the TV at once.
    let closer = {
        let device = device.clone();
        let session_id = Arc::clone(&session_id);
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(100));
            }
            let id = session_id.lock().unwrap().clone();
            if let Err(e) = cast::stop(&device, MIRRORING_RECEIVER, Some(&id)) {
                tracing::warn!("cannot close the mirroring receiver: {e:#}");
            }
        })
    };
    let result = reconnecting(receiver, device, s, screen, stop, out, &session_id);
    stop.store(true, Ordering::SeqCst);
    tracing::info!("mirroring ended: closing the receiver");
    let _ = closer.join();
    tracing::info!("mirroring receiver closed by sccast");
    result.map(|()| true)
}

/// Run sessions until one ends; after each loss, launch the mirroring receiver again.
fn reconnecting(
    mut receiver: cast::Receiver,
    device: &Device,
    s: &menu::Settings,
    screen: &ScreenCast,
    stop: &'static AtomicBool,
    out: &Out,
    session_id: &Mutex<String>,
) -> anyhow::Result<()> {
    let mut drops = cast::Drops::new(matches!(out, Out::Menu(_)));
    loop {
        // ponytail: leaks one flag per session (a byte), the threads want a 'static flag.
        let ended: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let outcome = stream(receiver, device, s, screen, stop, ended, out);
        ended.store(true, Ordering::SeqCst);
        let mut why = match outcome? {
            Outcome::Ended => return Ok(()),
            Outcome::Lost(why) => why,
        };
        loop {
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            tracing::warn!(%why, "mirroring lost");
            let count = drops.count(&device.name, &why)?;
            out.report(cast::Report::Reconnecting);
            out.say(&format!("Lost {} ({why}). Reconnecting ({count})...", device.name));
            std::thread::sleep(cast::RECONNECT_DELAY);
            match cast::launch(device, MIRRORING_RECEIVER, false, stop) {
                Ok(new) if new.app_id() == MIRRORING_RECEIVER => {
                    *session_id.lock().unwrap() = new.session_id().to_string();
                    receiver = new;
                    break;
                }
                Ok(other) => {
                    other.close();
                    why = "the mirroring receiver did not start".to_string();
                }
                Err(e) => why = format!("{e:#}"),
            }
        }
    }
}

/// One session on `receiver`, until `stop` (user) or the session's own end. Sets `ended`,
/// which stops its threads, whatever the reason.
fn stream(
    receiver: cast::Receiver,
    device: &Device,
    s: &menu::Settings,
    screen: &ScreenCast,
    stop: &'static AtomicBool,
    ended: &'static AtomicBool,
    out: &Out,
) -> anyhow::Result<Outcome> {
    // The user's stop ends the session too.
    std::thread::spawn(move || {
        while !ended.load(Ordering::SeqCst) {
            if stop.load(Ordering::SeqCst) {
                ended.store(true, Ordering::SeqCst);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    let cast = receiver.channel();
    let video_keys = Keys::random()?;
    let audio_keys = if s.stream.no_audio { None } else { Some(Keys::random()?) };
    let mut settings = s.stream.clone();
    let mut seq = 1;
    let answer = loop {
        let offered = offer(&settings, seq, &video_keys, audio_keys.as_ref());
        if let Err(e) = cast.receiver.broadcast_message(NAMESPACE, &offered) {
            return Ok(Outcome::Lost(format!("{e:#}")));
        }
        let answer = match out.step("Waiting for the TV's answer...", || wait_answer(cast, ended)) {
            Ok(Some(answer)) => answer,
            // Stopped by the user meanwhile.
            Ok(None) => return Ok(Outcome::Ended),
            Err(e) => return Ok(Outcome::Lost(format!("{e:#}"))),
        };
        tracing::info!(?answer, "mirroring answer");
        if answer.video {
            break answer;
        }
        // A refusal is not a lost session: no reconnection, it would be refused again.
        let (width, height) = settings.resolution;
        if height <= SMALLER_VIDEO.1 {
            bail!("{} refused the H.264 video at {width}x{height}", device.name);
        }
        out.say(&format!("{} refused {width}x{height}: trying 1280x720...", device.name));
        settings.resolution = SMALLER_VIDEO;
        seq += 1;
    };
    // A screen smaller than the video (Nest Hub: 1280x720): encode at its size, it would scale
    // the video down anyway. The OFFER gave a maximum, so no new one.
    if let Some((width, height)) = answer.display {
        let (width, height) = (width & !1, height & !1);
        if width < settings.resolution.0 && height < settings.resolution.1 {
            tracing::info!(width, height, "encoding at the receiver's screen size");
            settings.resolution = (width, height);
        }
    }
    if settings.resolution != s.stream.resolution {
        let (width, height) = settings.resolution;
        out.say(&format!("Sending {width}x{height}, the size {} takes.", device.name));
        out.send(menu::Status::Resolution(settings.resolution));
    }
    // The capture starts once the TV is ready: frames would pile up before.
    settings.no_audio = !answer.audio;
    let capture = Capture::start(Some(screen), &settings, s.audio_delay)?;
    if let Some(pad) = capture.audio_pad() {
        out.send(menu::Status::AudioPad(pad));
    }
    let pipeline = capture.pipeline();
    // Before PLAYING the running time reads 0, which would date every frame too early.
    let _ = pipeline.state(gst::ClockTime::from_seconds(2));
    let wall_start = SystemTime::now().duration_since(UNIX_EPOCH)?.saturating_sub(running_time(&pipeline));
    let socket = UdpSocket::bind(("0.0.0.0", 0))?;
    socket.connect((device.ip, answer.port))?;
    socket.set_read_timeout(Some(REPORT_INTERVAL))?;
    if !answer.adaptive {
        tracing::info!("the receiver keeps a fixed playout delay of {TARGET_DELAY_MS} ms");
    }
    let new_stream = |ssrc, payload, clock, keys| {
        let mut stream = RtpStream::new(ssrc, payload, clock, keys);
        stream.playout_delay = answer.adaptive.then_some(TARGET_DELAY_MS as u16);
        Mutex::new(stream)
    };
    let audio = match audio_keys {
        Some(keys) if answer.audio => Some(new_stream(AUDIO_SSRC, OPUS_PAYLOAD, 48_000, keys)),
        _ => None,
    };
    let session = Arc::new(Session {
        socket,
        video: new_stream(VIDEO_SSRC, H264_PAYLOAD, 90_000, video_keys),
        audio,
        pipeline,
        wall_start,
        silent: AtomicBool::new(false),
    });
    // The receiver drops packets that arrive before the first Sender Report.
    session.send_reports();

    let encoder = capture.element("venc").context("no video encoder")?;
    let mut threads = vec![send_frames(&session, &capture, "sink", true, ended)?];
    if session.audio.is_some() {
        threads.push(send_frames(&session, &capture, "asink", false, ended)?);
    }
    threads.push(feedback(&session, encoder, s.stream.bitrate, ended));
    // A capture failure sets `ended` and comes back as an error: no reconnection then.
    let watcher = capture.watch(ended)?;

    // Segment times of an earlier HLS cast would show in the stats.
    crate::hls::forget_fetches();
    out.send(menu::Status::Dropped(Arc::clone(&capture.dropped)));
    // Weak: the menu keeps the counter after the cast, the socket and packets must not stay.
    let counted = Arc::downgrade(&session);
    out.send(menu::Status::Served(Box::new(move || counted.upgrade().map_or(0, |s| s.octets()))));
    out.say(&format!("Mirroring to {}. Ctrl+C stops.", device.name));
    let (sender, control_end) = std::sync::mpsc::channel();
    let control_out = out.clone();
    // Not joined: receive() can block for minutes on a TV gone from the network. The RTCP
    // silence ends the session then, and this thread dies with its socket.
    std::thread::spawn(move || {
        let result = control(receiver.channel(), ended, &control_out);
        ended.store(true, Ordering::SeqCst);
        let _ = sender.send(result);
    });
    while !ended.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(100));
    }
    for thread in threads {
        let _ = thread.join();
    }
    // A capture failure explains why the cast stopped: report it first.
    watcher.join().expect("capture watcher panicked")?;
    if stop.load(Ordering::SeqCst) {
        return Ok(Outcome::Ended);
    }
    if session.silent.load(Ordering::SeqCst) {
        return Ok(Outcome::Lost("no report from the TV".to_string()));
    }
    Ok(match control_end.recv_timeout(Duration::from_secs(1)) {
        Ok(Ok(())) => Outcome::Ended,
        Ok(Err(e)) => Outcome::Lost(format!("{e:#}")),
        Err(_) => Outcome::Lost("no answer from the TV".to_string()),
    })
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
