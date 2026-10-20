//! Mirror format: H.264 and Opus over encrypted RTP to Google's mirroring receiver.
//! Protocol from openscreen/cast/streaming/impl. Notes in docs/cast-streaming.md.

use std::collections::VecDeque;
use std::io::Read;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aes::Aes128;
use anyhow::{bail, Context};
use ctr::cipher::{KeyIvInit, StreamCipher};
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
const MAX_PACKET: usize = 1400;
/// 12 bytes of RTP header, 7 of Cast header.
const HEADER: usize = 19;
/// Frames kept to resend lost packets: the receiver gives up beyond 120 (kMaxUnackedFrames).
const CACHE_FRAMES: usize = 120;
const REPORT_INTERVAL: Duration = Duration::from_millis(100);
const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);
/// The receiver repeats its key frame request in each report until one arrives.
const KEY_FRAME_SPACING: Duration = Duration::from_secs(1);
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;
/// Bitrate adaptation, first guesses: down 20 % above 2 % of packets lost, up 10 % after 10 s without loss.
const LOSS_LIMIT: u8 = 5; // out of 256
const MIN_BITRATE: u32 = 500;
const CLEAN_PERIOD: Duration = Duration::from_secs(10);

/// AES-128 key and IV mask of one stream, sent in the OFFER.
#[derive(Clone, Copy)]
struct Keys {
    key: [u8; 16],
    iv_mask: [u8; 16],
}

impl Keys {
    fn random() -> anyhow::Result<Self> {
        let mut bytes = [0u8; 32];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        let (key, iv_mask) = bytes.split_at(16);
        Ok(Self { key: key.try_into()?, iv_mask: iv_mask.try_into()? })
    }
}

/// AES-CTR over a frame. The counter starts at the IV mask XOR the frame id in bytes 8..12.
fn crypt(keys: &Keys, frame_id: u32, data: &mut [u8]) {
    let mut nonce = [0u8; 16];
    nonce[8..12].copy_from_slice(&frame_id.to_be_bytes());
    for (n, m) in nonce.iter_mut().zip(keys.iv_mask) {
        *n ^= m;
    }
    ctr::Ctr128BE::<Aes128>::new(&keys.key.into(), &nonce.into()).apply_keystream(data);
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

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

/// One RTP stream (video or audio): numbering, encryption, and the packets kept for resending.
struct RtpStream {
    ssrc: u32,
    payload_type: u8,
    clock_rate: u64,
    keys: Keys,
    rtp_base: u32,
    seq: u16,
    next_frame: u32,
    sent: VecDeque<(u32, Vec<Vec<u8>>)>,
    packets: u32,
    octets: u32,
}

impl RtpStream {
    fn new(ssrc: u32, payload_type: u8, clock_rate: u64, keys: Keys) -> Self {
        // ponytail: fixed start values; RFC 3550 wants random ones, the receiver does not care.
        Self { ssrc, payload_type, clock_rate, keys, rtp_base: 0, seq: 0, next_frame: 0, sent: VecDeque::new(), packets: 0, octets: 0 }
    }

    fn rtp_time(&self, running: Duration) -> u32 {
        self.rtp_base.wrapping_add((running.as_nanos() * u128::from(self.clock_rate) / 1_000_000_000) as u32)
    }

    /// Encrypt a frame captured at `running` and split it into packets, kept for resending.
    fn packetize(&mut self, data: &[u8], running: Duration, key_frame: bool) -> &[Vec<u8>] {
        let id = self.next_frame;
        self.next_frame = id.wrapping_add(1);
        let mut payload = data.to_vec();
        crypt(&self.keys, id, &mut payload);
        let chunk = MAX_PACKET - HEADER;
        let count = payload.len().div_ceil(chunk).max(1);
        // A delta frame of H.264 without B-frames refers to the previous one.
        let reference = if key_frame { id } else { id.wrapping_sub(1) };
        let timestamp = self.rtp_time(running);
        let mut packets = Vec::with_capacity(count);
        for (i, part) in payload.chunks(chunk).chain(payload.is_empty().then_some(&[][..])).enumerate() {
            let mut p = Vec::with_capacity(HEADER + part.len());
            let marker = if i == count - 1 { 0x80 } else { 0 };
            p.extend([0x80, marker | self.payload_type]);
            p.extend(self.seq.to_be_bytes());
            p.extend(timestamp.to_be_bytes());
            p.extend(self.ssrc.to_be_bytes());
            // Key frame flag, "has reference frame id" flag, then truncated frame ids.
            p.push(if key_frame { 0xc0 } else { 0x40 });
            p.push(id as u8);
            p.extend((i as u16).to_be_bytes());
            p.extend(((count - 1) as u16).to_be_bytes());
            p.push(reference as u8);
            p.extend(part);
            self.seq = self.seq.wrapping_add(1);
            self.packets = self.packets.wrapping_add(1);
            self.octets = self.octets.wrapping_add(part.len() as u32);
            packets.push(p);
        }
        if self.sent.len() == CACHE_FRAMES {
            self.sent.pop_front();
        }
        self.sent.push_back((id, packets));
        &self.sent.back().expect("just pushed").1
    }

    /// Packets the receiver reported lost: `packet`, or all of the frame for 0xffff, plus the
    /// next 8 packets flagged in `bits`. `frame` is the frame id truncated to 8 bits.
    fn lost(&self, frame: u8, packet: u16, bits: u8) -> Vec<&[u8]> {
        let latest = self.next_frame.wrapping_sub(1);
        let mut id = (latest & !0xff) | u32::from(frame);
        if id > latest {
            id = id.wrapping_sub(256);
        }
        let Some((_, packets)) = self.sent.iter().find(|(f, _)| *f == id) else { return Vec::new() };
        let wanted: Vec<usize> = if packet == 0xffff {
            (0..packets.len()).collect()
        } else {
            let first = usize::from(packet);
            std::iter::once(first).chain((0..8).filter(|b| bits >> b & 1 == 1).map(|b| first + 1 + b)).collect()
        };
        wanted.into_iter().filter_map(|i| packets.get(i).map(Vec::as_slice)).collect()
    }

    /// RTCP Sender Report: maps the RTP clock to the wall clock, so the receiver can schedule frames.
    fn sender_report(&self, wall: Duration, running: Duration) -> Vec<u8> {
        let ntp = ((wall.as_secs() + NTP_UNIX_OFFSET) << 32) | ((u64::from(wall.subsec_nanos()) << 32) / 1_000_000_000);
        let mut p = vec![0x80, 200, 0, 6];
        p.extend(self.ssrc.to_be_bytes());
        p.extend(ntp.to_be_bytes());
        p.extend(self.rtp_time(running).to_be_bytes());
        p.extend(self.packets.to_be_bytes());
        p.extend(self.octets.to_be_bytes());
        p
    }
}

/// What the receiver reports in RTCP.
#[derive(Debug, PartialEq)]
enum Feedback {
    KeyFrame { ssrc: u32 },
    Lost { ssrc: u32, frame: u8, packet: u16, bits: u8 },
    /// Share of packets lost since the last report, out of 256.
    Loss { ssrc: u32, fraction: u8 },
}

/// Parse a compound RTCP packet from the receiver. Unknown parts are skipped.
fn parse_rtcp(buf: &[u8]) -> Vec<Feedback> {
    let word = |b: &[u8], at: usize| b.get(at..at + 4).map(|w| u32::from_be_bytes(w.try_into().unwrap()));
    let mut found = Vec::new();
    let mut at = 0;
    while at + 4 <= buf.len() {
        let (subtype, kind) = (buf[at] & 0x1f, buf[at + 1]);
        let end = (at + 4 + 4 * usize::from(u16::from_be_bytes([buf[at + 2], buf[at + 3]]))).min(buf.len());
        let body = &buf[at + 4..end];
        at = end;
        match (kind, subtype) {
            // Receiver Report: its blocks follow the reporter's SSRC.
            (201, count) => {
                for block in body.get(4..).unwrap_or_default().chunks_exact(24).take(usize::from(count)) {
                    found.push(Feedback::Loss { ssrc: word(block, 0).unwrap(), fraction: block[4] });
                }
            }
            (206, 1) => found.extend(word(body, 4).map(|ssrc| Feedback::KeyFrame { ssrc })),
            (206, 15) if body.get(8..12) == Some(b"CAST") && body.len() >= 16 => {
                let ssrc = word(body, 4).unwrap();
                for loss in body[16..].chunks_exact(4).take(usize::from(body[13])) {
                    let packet = u16::from_be_bytes([loss[1], loss[2]]);
                    found.push(Feedback::Lost { ssrc, frame: loss[0], packet, bits: loss[3] });
                }
            }
            _ => {}
        }
    }
    found
}

/// Video bitrate in kbit/s, following the receiver's loss reports, never above the user's setting.
struct Rate {
    max: u32,
    current: u32,
    clean_since: Instant,
}

impl Rate {
    /// New bitrate to set, if it changes.
    fn adapt(&mut self, fraction_lost: u8, now: Instant) -> Option<u32> {
        let next = if fraction_lost > LOSS_LIMIT {
            self.clean_since = now;
            (self.current * 4 / 5).max(MIN_BITRATE)
        } else if now.duration_since(self.clean_since) >= CLEAN_PERIOD {
            self.clean_since = now;
            (self.current * 11 / 10).min(self.max)
        } else {
            self.current
        };
        (next != self.current).then(|| {
            self.current = next;
            next
        })
    }
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
    let wall_start = SystemTime::now().duration_since(UNIX_EPOCH)?
        .saturating_sub(pipeline.current_running_time().map_or(Duration::ZERO, Duration::from));
    let socket = UdpSocket::bind(("0.0.0.0", 0))?;
    socket.connect((device.ip, answer.port))?;
    socket.set_read_timeout(Some(REPORT_INTERVAL))?;
    let session = Arc::new(Session {
        socket,
        video: Mutex::new(RtpStream::new(VIDEO_SSRC, H264_PAYLOAD, 90_000, video_keys)),
        audio: audio_keys.filter(|_| answer.audio).map(|k| Mutex::new(RtpStream::new(AUDIO_SSRC, OPUS_PAYLOAD, 48_000, k))),
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

    out.send(menu::Status::Dropped(Arc::clone(&capture.dropped)));
    let counted = Arc::clone(&session);
    out.send(menu::Status::Served(Box::new(move || {
        let video = u64::from(counted.video.lock().unwrap().octets);
        video + counted.audio.as_ref().map_or(0, |a| u64::from(a.lock().unwrap().octets))
    })));
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
        match cast.receive()? {
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
fn send_frames(session: &Arc<Session>, capture: &Capture, name: &str, video: bool, stop: &'static AtomicBool) -> anyhow::Result<std::thread::JoinHandle<()>> {
    let sink = capture.element(name).with_context(|| format!("no {name} in the pipeline"))?;
    let session = Arc::clone(session);
    Ok(std::thread::spawn(move || {
        let stream = if video { &session.video } else { session.audio.as_ref().expect("audio stream") };
        while !stop.load(Ordering::SeqCst) {
            let Some(sample) = sink.emit_by_name::<Option<gst::Sample>>("try-pull-sample", &[&100_000_000u64]) else {
                continue;
            };
            let Some(buffer) = sample.buffer() else { continue };
            // x264enc shifts timestamps by 1000 h: the segment maps them back to running time.
            let running = sample
                .segment()
                .and_then(|s| s.downcast_ref::<gst::ClockTime>())
                .and_then(|s| s.to_running_time(buffer.pts()));
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
fn feedback(session: &Arc<Session>, encoder: gst::Element, bitrate: u32, stop: &'static AtomicBool) -> std::thread::JoinHandle<()> {
    let session = Arc::clone(session);
    std::thread::spawn(move || {
        let mut buf = [0u8; 1500];
        let mut last_report = Instant::now();
        let mut last_key_frame: Option<Instant> = None;
        let mut rate = Rate { max: bitrate, current: bitrate, clean_since: Instant::now() };
        let (mut resent, mut key_frames, mut logged) = (0u64, 0u64, Instant::now());
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
                        for p in stream.lost(frame, packet, bits) {
                            let _ = session.socket.send(p);
                            resent += 1;
                        }
                    }
                    Feedback::KeyFrame { ssrc: VIDEO_SSRC } => {
                        if last_key_frame.is_none_or(|t| t.elapsed() >= KEY_FRAME_SPACING) {
                            let event = gst::Structure::builder("GstForceKeyUnit").field("all-headers", true).build();
                            encoder.send_event(gst::event::CustomDownstream::new(event));
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

/// Watch the control channel until `stop`, the TV taken by another sender, or the connection lost.
fn control(cast: &CastDevice<'static>, stop: &AtomicBool, out: &Out) -> anyhow::Result<()> {
    let mut app_seen = false;
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
            ChannelMessage::Receiver(ReceiverResponse::Status(status)) => {
                let running = status.applications.iter().any(|a| a.app_id == MIRRORING_RECEIVER);
                if app_seen && !running {
                    tracing::info!("mirroring receiver closed");
                    return Ok(());
                }
                app_seen |= running;
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYS: Keys = Keys {
        key: [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        iv_mask: [0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa, 0xfb, 0xfc, 0xfd, 0xfe, 0xff],
    };

    #[test]
    fn encrypts_like_the_prototype() {
        // Expected bytes from proto/stream_proto.py (Python cryptography), whose frames the Sharp TV decoded.
        let mut data = b"Cast Streaming test payload, 40 bytes...".to_vec();
        crypt(&KEYS, 0x0102_0307, &mut data);
        assert_eq!(hex(&data), "8219ecebefdbbe9c7c63cce6247d2093800ae9fa86ea2a75f6798bc377a6b91a1347f8a6846536e1");
        crypt(&KEYS, 0x0102_0307, &mut data);
        assert_eq!(data, b"Cast Streaming test payload, 40 bytes...");
    }

    #[test]
    fn packetizes_and_resends() {
        let mut stream = RtpStream::new(VIDEO_SSRC, H264_PAYLOAD, 90_000, KEYS);
        let frame = vec![7u8; 3000];
        let packets = stream.packetize(&frame, Duration::from_secs(1), true).to_vec();
        assert_eq!(packets.len(), 3);
        let first = &packets[0];
        assert_eq!(first[..2], [0x80, H264_PAYLOAD]);
        assert_eq!(first[4..8], 90_000u32.to_be_bytes());
        assert_eq!(first[8..12], VIDEO_SSRC.to_be_bytes());
        // Key frame, frame 0, packet 0 of 0..=2, refers to itself.
        assert_eq!(first[12..19], [0xc0, 0, 0, 0, 0, 2, 0]);
        assert_eq!(packets[2][1], 0x80 | H264_PAYLOAD, "marker on the last packet");
        assert_eq!(packets.iter().map(|p| p.len() - HEADER).sum::<usize>(), 3000);
        assert_eq!(packets[1][2..4], 1u16.to_be_bytes());

        let delta = stream.packetize(&[1, 2, 3], Duration::from_secs(2), false).to_vec();
        assert_eq!(delta[0][12..19], [0x40, 1, 0, 0, 0, 0, 0]);
        assert_eq!(stream.lost(0, 1, 0b1), vec![&packets[1][..], &packets[2][..]]);
        assert_eq!(stream.lost(0, 0xffff, 0).len(), 3);
        assert!(stream.lost(9, 0, 0).is_empty());
    }

    #[test]
    fn builds_offer_and_reads_answer() {
        let stream = Stream {
            resolution: (1280, 720), fps: 30, bitrate: 2500, port: 8090, no_audio: false,
            format: crate::cli::Format::Mirror,
        };
        let offer = offer(&stream, &KEYS, Some(&KEYS));
        let streams = offer["offer"]["supportedStreams"].as_array().unwrap();
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0]["codecName"], "h264");
        assert_eq!(streams[0]["maxBitRate"], 2_500_000);
        assert_eq!(streams[0]["aesKey"], "000102030405060708090a0b0c0d0e0f");
        assert_eq!(streams[1]["ssrc"], AUDIO_SSRC);
        // The Sharp TV's answer, video only.
        let answer: Value = serde_json::from_str(
            r#"{"answer":{"castMode":"mirroring","rtpExtensions":[[]],"sendIndexes":[0],"ssrcs":[2],"udpPort":36026},"result":"ok","seqNum":1,"type":"ANSWER"}"#,
        )
        .unwrap();
        assert_eq!(parse_answer(&answer).unwrap(), Answer { port: 36026, video: true, audio: false });
        assert!(parse_answer(&json!({"result": "error", "error": {"code": 2}})).is_err());
    }

    #[test]
    fn parses_receiver_feedback() {
        let mut buf = vec![0x81, 201, 0, 7];
        buf.extend(2u32.to_be_bytes());
        buf.extend(VIDEO_SSRC.to_be_bytes());
        buf.extend([12, 0, 0, 1]);
        buf.extend([0; 16]);
        buf.extend([0x81, 206, 0, 2]);
        buf.extend(2u32.to_be_bytes());
        buf.extend(VIDEO_SSRC.to_be_bytes());
        buf.extend([0x8f, 206, 0, 5]);
        buf.extend(2u32.to_be_bytes());
        buf.extend(VIDEO_SSRC.to_be_bytes());
        buf.extend(b"CAST");
        buf.extend([4, 1, 0, 100]);
        buf.extend([5, 0, 3, 0b10]);
        assert_eq!(
            parse_rtcp(&buf),
            vec![
                Feedback::Loss { ssrc: VIDEO_SSRC, fraction: 12 },
                Feedback::KeyFrame { ssrc: VIDEO_SSRC },
                Feedback::Lost { ssrc: VIDEO_SSRC, frame: 5, packet: 3, bits: 0b10 },
            ]
        );
        assert!(parse_rtcp(&[0x80, 201]).is_empty());
    }

    #[test]
    fn adapts_bitrate() {
        let start = Instant::now();
        let mut rate = Rate { max: 2500, current: 2500, clean_since: start };
        assert_eq!(rate.adapt(0, start), None);
        assert_eq!(rate.adapt(20, start), Some(2000));
        assert_eq!(rate.adapt(0, start + Duration::from_secs(5)), None);
        assert_eq!(rate.adapt(0, start + CLEAN_PERIOD), Some(2200));
        assert_eq!(rate.adapt(0, start + CLEAN_PERIOD * 3), Some(2420));
        assert_eq!(rate.adapt(0, start + CLEAN_PERIOD * 4), Some(2500));
    }
}
