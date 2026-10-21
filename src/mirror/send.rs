//! send.rs
//! The two threads of a mirroring session: one sends each encoded frame as it comes out of
//! the capture, the other reads the receiver's reports to resend lost packets, force key
//! frames and adapt the bitrate.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context;
use gstreamer as gst;
use gstreamer::prelude::*;

use super::rate::{Delay, Rate, LOSS_LIMIT};
use super::rtcp::{parse_rtcp, Feedback};
use super::{Session, KEY_FRAME_SPACING, REPORT_INTERVAL, SILENCE, SILENCE_START, VIDEO_SSRC};
use crate::capture::Capture;

/// Send each encoded frame of appsink `name` as it comes, until `stop`.
pub fn send_frames(
    session: &Arc<Session>,
    capture: &Capture,
    name: &str,
    video: bool,
    stop: &'static AtomicBool,
) -> anyhow::Result<JoinHandle<()>> {
    let sink = capture.element(name).with_context(|| format!("no {name} in the pipeline"))?;
    let session = Arc::clone(session);
    Ok(std::thread::spawn(move || {
        let stream = if video { &session.video } else { session.audio.as_ref().expect("audio stream") };
        while !stop.load(Ordering::SeqCst) {
            let Some(sample) = sink.emit_by_name::<Option<gst::Sample>>("try-pull-sample", &[&100_000_000u64]) else {
                // At the end of stream the pull returns at once: stop instead of spinning.
                if sink.property::<bool>("eos") {
                    break;
                }
                continue;
            };
            let Some(buffer) = sample.buffer() else { continue };
            // x264enc shifts timestamps by 1000 h: the segment maps them back to running time.
            let running = sample
                .segment()
                .and_then(|segment| segment.downcast_ref::<gst::ClockTime>())
                .and_then(|segment| segment.to_running_time(buffer.pts()));
            let Some(running) = running else { continue };
            let Ok(map) = buffer.map_readable() else { continue };
            let key_frame = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
            let mut stream = stream.lock().unwrap();
            for packet in stream.packetize(&map, running.into(), key_frame) {
                let _ = session.socket.send(packet);
            }
        }
    }))
}

/// Read the receiver's RTCP: resend lost packets, force key frames, adapt the bitrate and
/// the playout delay.
/// Sends the Sender Reports between two reads. Sets `stop` and `session.silent` when the
/// receiver stays silent too long.
pub fn feedback(session: &Arc<Session>, encoder: gst::Element, bitrate: u32, stop: &'static AtomicBool) -> JoinHandle<()> {
    let session = Arc::clone(session);
    std::thread::spawn(move || {
        let mut buf = [0u8; 1500];
        let mut last_report = Instant::now();
        let mut last_key_frame: Option<Instant> = None;
        let mut rate = Rate::new(bitrate);
        let mut resent = 0u64;
        let mut key_frames = 0u64;
        let mut logged = Instant::now();
        let started = Instant::now();
        let mut heard: Option<Instant> = None;
        // Delay the TV reports applying, to check it follows ours.
        let mut tv_delay_ms: Option<u16> = None;
        let mut delay = Delay::new(started);
        let adaptive = session.video.lock().unwrap().playout_delay.is_some();
        let mut retarget = |trouble: bool| {
            if !adaptive {
                return;
            }
            if let Some(ms) = delay.adapt(trouble, Instant::now()) {
                tracing::info!(ms, "playout delay");
                // Same delay on both streams, or lip sync would drift.
                for stream in std::iter::once(&session.video).chain(&session.audio) {
                    stream.lock().unwrap().playout_delay = Some(ms);
                }
            }
        };
        while !stop.load(Ordering::SeqCst) {
            if last_report.elapsed() >= REPORT_INTERVAL {
                session.send_reports();
                last_report = Instant::now();
            }
            if logged.elapsed() >= Duration::from_secs(10) {
                let rtt_ms = session.video.lock().unwrap().rtt.map(|rtt| rtt.as_millis());
                tracing::info!(resent, key_frames, kbps = rate.current, ?rtt_ms, ?tv_delay_ms, "mirroring");
                logged = Instant::now();
            }
            // Errors are timeouts, or ICMP refusals once the receiver is gone.
            let Ok(len) = session.socket.recv(&mut buf) else {
                let (since, limit) = heard.map_or((started, SILENCE_START), |at| (at, SILENCE));
                if since.elapsed() >= limit {
                    tracing::warn!(seconds = limit.as_secs(), "no RTCP from the receiver");
                    session.silent.store(true, Ordering::SeqCst);
                    stop.store(true, Ordering::SeqCst);
                }
                continue;
            };
            heard = Some(Instant::now());
            for item in parse_rtcp(&buf[..len]) {
                match item {
                    Feedback::Lost { ssrc, frame, packet, bits } => {
                        let Some(stream) = session.stream(ssrc) else { continue };
                        let mut stream = stream.lock().unwrap();
                        for lost_packet in stream.lost(frame, packet, bits, Instant::now()) {
                            let _ = session.socket.send(lost_packet);
                            resent += 1;
                        }
                    }
                    Feedback::KeyFrame { ssrc: VIDEO_SSRC } => {
                        // A frame missed its playout time: give resends more room.
                        retarget(true);
                        if last_key_frame.is_none_or(|t| t.elapsed() >= KEY_FRAME_SPACING) {
                            request_key_frame(&encoder);
                            last_key_frame = Some(Instant::now());
                            key_frames += 1;
                        }
                    }
                    Feedback::Ack { ssrc, frame, delay_ms } => {
                        if ssrc == VIDEO_SSRC {
                            tv_delay_ms = Some(delay_ms);
                        }
                        if let Some(stream) = session.stream(ssrc) {
                            stream.lock().unwrap().acked(frame);
                        }
                    }
                    Feedback::Report { ssrc, fraction, last_sr, since_sr } => {
                        let Some(stream) = session.stream(ssrc) else { continue };
                        stream.lock().unwrap().receiver_report(last_sr, since_sr, Instant::now());
                        if ssrc == VIDEO_SSRC {
                            retarget(fraction > LOSS_LIMIT);
                            if let Some(kbps) = rate.adapt(fraction, Instant::now()) {
                                tracing::info!(kbps, fraction, "video bitrate");
                                encoder.set_property("bitrate", kbps);
                            }
                        }
                    }
                    Feedback::KeyFrame { .. } => {}
                }
            }
        }
    })
}

/// Upstream on the src pad: not serialized, so no wait for the encoder's stream lock.
fn request_key_frame(encoder: &gst::Element) {
    let event = gst::Structure::builder("GstForceKeyUnit").field("all-headers", true).build();
    if let Some(pad) = encoder.static_pad("src") {
        pad.send_event(gst::event::CustomUpstream::new(event));
    }
}
