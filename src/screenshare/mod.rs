//! screenshare/
//! Cast the screen to an interactive panel in its "classic" mode: a TCP command channel,
//! then one TCP connection for the video and one for the sound. Read from the panel's
//! Android sender; unverified on a real panel. The protocol is in docs/screenshare.md.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

mod touch;
mod wire;

use std::io;
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use gstreamer as gst;
use gstreamer::prelude::*;
use serde_json::{json, Value};

use crate::capture::{request_key_frame, Capture, ScreenCast};
use crate::menu::Settings;
use crate::session::Out;
use wire::{media_frame, read_message, split_config, write_message, ScreenshareMessage};

/// (commandType, commandId) pairs, from the sender's ProtocolConstant.
const HEARTBEAT: (i32, i32) = (0, 0);
const CONNECT: (i32, i32) = (1, 1);
const CONNECTED: (i32, i32) = (1, 101);
const DISCONNECTED: (i32, i32) = (1, 102);
const CAST: (i32, i32) = (6, 2);
const STOP_CAST: (i32, i32) = (6, 3);
const CAST_ANSWER: (i32, i32) = (6, 102);
const CAST_STOPPED: (i32, i32) = (6, 103);
const KEY_FRAME: (i32, i32) = (6, 113);
/// Receiver version from which video frames carry an index.
const INDEX_VERSION: i32 = 63;
/// The Android sender sends a heartbeat after 3 s without writing.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);
/// Twice the sender's 10 s read timeout: the panel is gone.
const SILENCE: Duration = Duration::from_secs(20);
const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);

/// Command channel: writes here, a thread reads into `incoming`.
struct Commands {
    socket: TcpStream,
    sequence: i32,
    incoming: mpsc::Receiver<io::Result<ScreenshareMessage>>,
}

impl Commands {
    fn open(host: SocketAddr) -> anyhow::Result<Self> {
        let socket = TcpStream::connect_timeout(&host, ANSWER_TIMEOUT).with_context(|| format!("cannot reach {host}"))?;
        socket.set_nodelay(true)?;
        let mut reader = socket.try_clone()?;
        let (tx, incoming) = mpsc::channel();
        std::thread::spawn(move || loop {
            let message = read_message(&mut reader);
            let failed = message.is_err();
            if tx.send(message).is_err() || failed {
                break;
            }
        });
        Ok(Self { socket, sequence: 0, incoming })
    }

    fn send(&mut self, (command_type, command_id): (i32, i32), payload: &Value) -> io::Result<()> {
        self.sequence += 1;
        self.reply((command_type, command_id), self.sequence, payload)
    }

    /// Send with a given sequence: an answer repeats its request's.
    fn reply(&mut self, (command_type, command_id): (i32, i32), sequence: i32, payload: &Value) -> io::Result<()> {
        let (timestamp, payload) = (now_ms(), payload.to_string());
        write_message(&mut self.socket, &ScreenshareMessage { version: 1, command_type, command_id, sequence, timestamp, payload })
    }

    /// Wait for `wanted`; return its version and JSON payload.
    fn wait(&self, wanted: (i32, i32)) -> anyhow::Result<(i32, Value)> {
        let deadline = Instant::now() + ANSWER_TIMEOUT;
        loop {
            let m = self.incoming.recv_timeout(deadline.saturating_duration_since(Instant::now())).context("no answer from the panel")??;
            tracing::debug!(t = m.command_type, id = m.command_id, payload = %m.payload, "panel message");
            if (m.command_type, m.command_id) == wanted {
                return Ok((m.version, serde_json::from_str(&m.payload).unwrap_or(Value::Null)));
            }
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// Cast `screen` to the panel at `host` until `stop`. After a loss, connect again.
pub fn cast(host: SocketAddr, pin: Option<&str>, s: &Settings, screen: &ScreenCast, stop: &'static AtomicBool, out: &Out) -> anyhow::Result<()> {
    // A first failure is a wrong address or a refusal: report it.
    let mut why = out.step(&format!("Connecting to the panel {host}..."), || session(host, pin, s, screen, stop))?;
    while let Some(lost) = why {
        tracing::warn!(%lost, "panel lost");
        out.say(&format!("Lost the panel ({lost}). Reconnecting..."));
        std::thread::sleep(crate::device::RECONNECT_DELAY);
        if stop.load(Ordering::SeqCst) {
            break;
        }
        why = session(host, pin, s, screen, stop).unwrap_or_else(|e| Some(format!("{e:#}")));
    }
    Ok(())
}

/// One session: Ok(None) at `stop` or when the panel ends the cast, Ok(Some(why)) when it is lost.
fn session(host: SocketAddr, pin: Option<&str>, s: &Settings, screen: &ScreenCast, stop: &'static AtomicBool) -> anyhow::Result<Option<String>> {
    let mut commands = Commands::open(host)?;
    let local_ip = commands.socket.local_addr()?.ip();
    commands.send(CONNECT, &connect_request(pin, local_ip))?;
    let (_, answer) = commands.wait(CONNECTED)?;
    if answer["resultType"] != 1 {
        bail!("the panel refused the connection: {answer}");
    }
    let (width, height) = s.stream.resolution;
    let audio = json!({"codec": 11, "sampleRate": 44100, "channel": 2, "bitDepth": 16, "samplePerPacket": 1024, "adtsEnabled": true});
    let request = json!({"authorizeMode": 0, "castMode": 0, "displayMode": 0, "supportAudio": !s.stream.no_audio,
        "audio": audio, "video": {"width": width, "height": height}});
    commands.send(CAST, &request)?;
    let (version, answer) = commands.wait(CAST_ANSWER)?;
    tracing::info!(version, %answer, "cast answer");
    if answer["resultType"] != 1 {
        bail!("the panel refused the cast: {answer}");
    }
    let port = |key: &str| answer[key].as_u64().and_then(|p| u16::try_from(p).ok()).with_context(|| format!("no {key} in {answer}"));
    let video_socket = media_socket(host, port("videoPort")?)?;
    let audio_socket = if s.stream.no_audio { None } else { Some(media_socket(host, port("audioPort")?)?) };

    let capture = Capture::start(Some(screen), &s.stream, s.audio_delay)?;
    let encoder = capture.element("venc").context("no encoder in the pipeline")?;
    // ponytail: leaks one flag per session (a byte), as mirror does: the threads want a 'static flag.
    let ended: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
    let index = (version >= INDEX_VERSION).then_some(0);
    let mut senders = vec![send_frames(&capture, "sink", video_socket, index, ended)?];
    if let Some(socket) = audio_socket {
        senders.push(send_frames(&capture, "asink", socket, None, ended)?);
    }

    let mut touchback = touch::Touchback::default();
    let mut last_heartbeat = Instant::now();
    let mut last_heard = Instant::now();
    let lost = loop {
        if stop.load(Ordering::SeqCst) {
            let _ = commands.send(STOP_CAST, &json!({}));
            break None;
        }
        if ended.load(Ordering::SeqCst) {
            break Some("a media connection closed".to_string());
        }
        if last_heard.elapsed() > SILENCE {
            break Some("no message for 20 s".to_string());
        }
        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
            if let Err(e) = commands.send(HEARTBEAT, &json!({"sentTimeMs": now_ms()})) {
                break Some(e.to_string());
            }
            last_heartbeat = Instant::now();
        }
        let m = match commands.incoming.recv_timeout(Duration::from_millis(200)) {
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break Some("command channel closed".to_string()),
            Ok(Err(e)) => break Some(e.to_string()),
            Ok(Ok(m)) => m,
        };
        last_heard = Instant::now();
        match (m.command_type, m.command_id) {
            KEY_FRAME => request_key_frame(&encoder),
            CAST_STOPPED | DISCONNECTED => break None,
            _ => match touchback.handle(&m) {
                Some((ids, payload)) => drop(commands.reply(ids, m.sequence, &payload)),
                None => tracing::debug!(t = m.command_type, id = m.command_id, payload = %m.payload, "panel message"),
            },
        }
    };
    ended.store(true, Ordering::SeqCst);
    let _ = commands.socket.shutdown(Shutdown::Both);
    for sender in senders {
        let _ = sender.join();
    }
    Ok(lost)
}

/// Connect request, with the fields the Android sender fills. Which ones the panel needs is unverified.
fn connect_request(pin: Option<&str>, local_ip: std::net::IpAddr) -> Value {
    let read = |path| std::fs::read_to_string(path).map(|s| s.trim().to_string()).ok().filter(|s| !s.is_empty());
    let name = read("/etc/hostname").unwrap_or_else(|| "sccast".to_string());
    let uuid = read("/etc/machine-id").unwrap_or_else(|| name.clone());
    json!({"deviceName": name, "deviceType": 1, "pinCode": pin.unwrap_or(""), "connectType": 0,
        "deviceVersion": env!("CARGO_PKG_VERSION"), "deviceUuid": uuid, "functions": [],
        "originIp": local_ip.to_string(), "systemName": "Linux"})
}

fn media_socket(host: SocketAddr, port: u16) -> anyhow::Result<TcpStream> {
    let address = SocketAddr::new(host.ip(), port);
    let socket = TcpStream::connect_timeout(&address, ANSWER_TIMEOUT).with_context(|| format!("cannot reach {address}"))?;
    socket.set_nodelay(true)?;
    // A panel that stops reading must not hang the sender thread.
    socket.set_write_timeout(Some(SILENCE))?;
    Ok(socket)
}

/// Write each frame of appsink `name` to `socket` until `ended`. Video (`index` Some or the
/// "sink" appsink): SPS and PPS apart, type 0. Audio: the type field carries a clock in ms,
/// as the Android sender does.
fn send_frames(capture: &Capture, name: &'static str, mut socket: TcpStream, mut index: Option<i32>, ended: &'static AtomicBool) -> anyhow::Result<JoinHandle<()>> {
    use std::io::Write;
    let sink = capture.element(name).with_context(|| format!("no {name} in the pipeline"))?;
    let video = name == "sink";
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
                if let Err(e) = socket.write_all(&media_frame(index, kind, part)) {
                    tracing::warn!(name, "media connection: {e}");
                    ended.store(true, Ordering::SeqCst);
                    return;
                }
                index = index.map(|i| i.wrapping_add(1));
            }
        }
    }))
}
