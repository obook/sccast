//! Mirror format: H.264 and Opus over encrypted RTP to Google's mirroring receiver.
//! Protocol from openscreen/cast/streaming/impl. Notes in docs/cast-streaming.md.

mod rtp;

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use gstreamer as gst;
use gstreamer::prelude::*;
use rust_cast::channels::connection::ConnectionResponse;
use rust_cast::channels::heartbeat::HeartbeatResponse;
use rust_cast::channels::media::MediaResponse;
use rust_cast::channels::receiver::ReceiverResponse;
use rust_cast::message_manager::CastMessagePayload;
use rust_cast::{CastDevice, ChannelMessage};
use serde_json::{json, Value};

use crate::capture::Capture;
use crate::cli::Stream;
use crate::discovery::CastDevice as Device;
use crate::portal::ScreenCast;
use crate::{cast, menu, Out};
use rtp::{hex, parse_rtcp, Feedback, Keys, Rate, RtpStream};

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
const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);
/// The receiver repeats its key frame request in each report until one arrives.
const KEY_FRAME_SPACING: Duration = Duration::from_secs(1);

fn offer(stream: &Stream, video: &Keys, audio: Option<&Keys>) -> Value {
    let (width, height) = stream.resolution;
    let mut streams = vec![json!({
        "index": 0, "type": "video_source", "codecName": "h264", "rtpProfile": "cast",
        "rtpPayloadType": H264_PAYLOAD, "ssrc": VIDEO_SSRC, "maxFrameRate": format!("{}/1", stream.fps),
        "timeBase": "1/90000", "maxBitRate": stream.bitrate * 1000, "targetDelay": TARGET_DELAY_MS,
        "aesKey": hex(&video.key), "aesIvMask": hex(&video.iv_mask), "receiverRtcpEventLog": false,
        "resolutions": [{"width": width, "height": height}],
    })];
    if let Some(audio) = audio {
        streams.push(json!({
            "index": 1, "type": "audio_source", "codecName": "opus", "rtpProfile": "cast",
            "rtpPayloadType": OPUS_PAYLOAD, "ssrc": AUDIO_SSRC, "bitRate": AUDIO_BITRATE,
            "timeBase": "1/48000", "channels": 2, "targetDelay": TARGET_DELAY_MS,
            "aesKey": hex(&audio.key), "aesIvMask": hex(&audio.iv_mask), "receiverRtcpEventLog": false,
        }));
    }
    json!({"type": "OFFER", "seqNum": 1, "offer": {"castMode": "mirroring", "supportedStreams": streams}})
}

/// What the receiver accepted.
#[derive(Debug, PartialEq)]
struct Answer {
    port: u16,
    video: bool,
    audio: bool,
}

fn parse_answer(answer: &Value) -> anyhow::Result<Answer> {
    if answer["result"] != "ok" {
        bail!("the TV refused the stream: {}", answer["error"]);
    }
    let body = &answer["answer"];
    let port = body["udpPort"].as_u64().and_then(|p| u16::try_from(p).ok()).context("ANSWER without udpPort")?;
    let accepted = |index| body["sendIndexes"].as_array().is_some_and(|a| a.contains(&json!(index)));
    Ok(Answer { port, video: accepted(0), audio: accepted(1) })
}

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
        self.pipeline.current_running_time().map_or(Duration::ZERO, Duration::from)
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
        let video = u64::from(self.video.lock().unwrap().octets);
        let audio = self.audio.as_ref().map_or(0, |a| u64::from(a.lock().unwrap().octets));
        video + audio
    }
}

/// Mirror `screen` to `device` until `stop`, or until the TV is taken or lost.
/// The mirroring app is always closed at the end: `keep` of the menu does not apply.
pub fn cast(device: &Device, s: &menu::Settings, screen: &ScreenCast, stop: &'static AtomicBool, out: &Out) -> anyhow::Result<()> {
    if let Out::Menu(_) = out {
        crate::watch_round_trip(device, stop, out.clone());
    }
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
    let answer = out.step("Waiting for the TV's answer...", || wait_answer(cast, stop))?;
    tracing::info!(?answer, "mirroring accepted");
    if !answer.video {
        bail!("{} refused the H.264 video: try a lower resolution", device.name);
    }
    // The capture starts once the TV is ready: frames would pile up before.
    let mut settings = s.stream.clone();
    settings.no_audio = !answer.audio;
    let capture = Capture::start(Some(screen), &settings, Duration::from_millis(s.audio_delay))?;
    let pipeline = capture.pipeline();
    let running = pipeline.current_running_time().map_or(Duration::ZERO, Duration::from);
    let wall_start = SystemTime::now().duration_since(UNIX_EPOCH)?.saturating_sub(running);
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
    crate::server::forget_fetches();
    out.send(menu::Status::Dropped(Arc::clone(&capture.dropped)));
    let counted = Arc::clone(&session);
    out.send(menu::Status::Served(Box::new(move || counted.octets())));
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

/// Wait for the ANSWER to the OFFER, answering the device's pings meanwhile.
fn wait_answer(cast: &CastDevice<'static>, stop: &AtomicBool) -> anyhow::Result<Answer> {
    let started = Instant::now();
    while started.elapsed() < ANSWER_TIMEOUT && !stop.load(Ordering::SeqCst) {
        let message = match cast.receive() {
            Ok(message) => message,
            Err(rust_cast::errors::Error::Parsing(_) | rust_cast::errors::Error::Serialization(_)) => continue,
            Err(e) => return Err(e.into()),
        };
        match message {
            ChannelMessage::Heartbeat(HeartbeatResponse::Ping) => cast.heartbeat.pong()?,
            ChannelMessage::Raw(message) if message.namespace == NAMESPACE => {
                let CastMessagePayload::String(text) = message.payload else { continue };
                let value: Value = serde_json::from_str(&text)?;
                if value["type"] == "ANSWER" {
                    return parse_answer(&value);
                }
            }
            _ => {}
        }
    }
    bail!("no answer from the mirroring receiver")
}

/// Send each encoded frame of appsink `name` as it comes, until `stop`.
fn send_frames(session: &Arc<Session>, capture: &Capture, name: &str, video: bool, stop: &'static AtomicBool) -> anyhow::Result<JoinHandle<()>> {
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
fn feedback(session: &Arc<Session>, encoder: gst::Element, bitrate: u32, stop: &'static AtomicBool) -> JoinHandle<()> {
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
                        let stream = stream.lock().unwrap();
                        for lost_packet in stream.lost(frame, packet, bits) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_offer_and_reads_answer() {
        let keys = Keys { key: [0x11; 16], iv_mask: [0x22; 16] };
        let stream = Stream {
            resolution: (1280, 720), fps: 30, bitrate: 2500, port: 8090, no_audio: false,
            format: crate::cli::Format::Mirror,
        };
        let offer = offer(&stream, &keys, Some(&keys));
        let streams = offer["offer"]["supportedStreams"].as_array().unwrap();
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0]["codecName"], "h264");
        assert_eq!(streams[0]["maxBitRate"], 2_500_000);
        assert_eq!(streams[0]["aesKey"], "11111111111111111111111111111111");
        assert_eq!(streams[1]["ssrc"], AUDIO_SSRC);
        // The Sharp TV's answer, video only.
        let answer: Value = serde_json::from_str(
            r#"{"answer":{"castMode":"mirroring","rtpExtensions":[[]],"sendIndexes":[0],"ssrcs":[2],"udpPort":36026},"result":"ok","seqNum":1,"type":"ANSWER"}"#,
        )
        .unwrap();
        assert_eq!(parse_answer(&answer).unwrap(), Answer { port: 36026, video: true, audio: false });
        assert!(parse_answer(&json!({"result": "error", "error": {"code": 2}})).is_err());
    }
}
