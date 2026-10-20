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

use super::rate::Rate;
use super::rtcp::{parse_rtcp, Feedback};
use super::{Session, KEY_FRAME_SPACING, REPORT_INTERVAL, VIDEO_SSRC};
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

/// Read the receiver's RTCP: resend lost packets, force key frames, adapt the bitrate.
/// Sends the Sender Reports between two reads.
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
        while !stop.load(Ordering::SeqCst) {
            if last_report.elapsed() >= REPORT_INTERVAL {
                session.send_reports();
                last_report = Instant::now();
            }
            if logged.elapsed() >= Duration::from_secs(10) {
                tracing::info!(resent, key_frames, kbps = rate.current, "mirroring");
                logged = Instant::now();
            }
            // Errors are timeouts, or ICMP refusals once the receiver is gone.
            let Ok(len) = session.socket.recv(&mut buf) else { continue };
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
                        if last_key_frame.is_none_or(|t| t.elapsed() >= KEY_FRAME_SPACING) {
                            request_key_frame(&encoder);
                            last_key_frame = Some(Instant::now());
                            key_frames += 1;
                        }
                    }
                    Feedback::Loss { ssrc: VIDEO_SSRC, fraction } => {
                        if let Some(kbps) = rate.adapt(fraction, Instant::now()) {
                            tracing::info!(kbps, fraction, "video bitrate");
                            encoder.set_property("bitrate", kbps);
                        }
                    }
                    _ => {}
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
