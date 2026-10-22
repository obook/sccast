//! mirror/session.rs
//! One mirroring session: OFFER and ANSWER, capture, RTP streams and their threads,
//! until the user stops or the session is lost.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use gstreamer as gst;
use gstreamer::prelude::*;

use super::control::control;
use super::offer::{offer, wait_answer};
use super::rtp::{Keys, RtpStream};
use super::send::{feedback, send_frames};
use super::{Outcome, AUDIO_SSRC, H264_PAYLOAD, NAMESPACE, OPUS_PAYLOAD, REPORT_INTERVAL, SMALLER_VIDEO, TARGET_DELAY_MS, VIDEO_SSRC};
use crate::capture::{Capture, ScreenCast};
use crate::device::{self as cast, CastDevice as Device};
use crate::menu;
use crate::session::Out;

/// Streams of a running session and the socket they share.
pub(super) struct Session {
    pub(super) socket: UdpSocket,
    pub(super) video: Mutex<RtpStream>,
    pub(super) audio: Option<Mutex<RtpStream>>,
    pipeline: gst::Element,
    /// Wall clock at running time 0, for the Sender Reports.
    wall_start: Duration,
    /// Set when the TV stopped sending RTCP.
    pub(super) silent: AtomicBool,
}

impl Session {
    pub(super) fn stream(&self, ssrc: u32) -> Option<&Mutex<RtpStream>> {
        match ssrc {
            VIDEO_SSRC => Some(&self.video),
            AUDIO_SSRC => self.audio.as_ref(),
            _ => None,
        }
    }

    pub(super) fn running(&self) -> Duration {
        running_time(&self.pipeline)
    }

    pub(super) fn send_reports(&self) {
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

/// One session on `receiver`, until `stop` (user) or the session's own end. Sets `ended`,
/// which stops its threads, whatever the reason.
pub(super) fn stream(
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

    out.send(menu::Status::Dropped(Arc::clone(&capture.dropped)));
    // Weak: the menu keeps the counter after the cast, the socket and packets must not stay.
    let counted = Arc::downgrade(&session);
    out.send(menu::Status::Served(Box::new(move || counted.upgrade().map_or(0, |s| s.octets()))));
    out.say(&format!("Mirroring to {}. Ctrl+C stops.", device.name));
    out.state("casting");
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
