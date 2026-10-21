//! screenshare/
//! Cast the screen to an interactive panel in its "classic" mode: a TCP command channel,
//! then one TCP connection for the video and one for the sound. Read from the panel's
//! Android sender; unverified on a real panel. The protocol is in docs/screenshare.md.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

mod approval;
mod media;
pub mod pin;
mod touch;
mod wire;

use std::io;
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde_json::{json, Value};

use crate::capture::{request_key_frame, Capture, ScreenCast};
use crate::menu::{Settings, Status};
use crate::session::Out;
use approval::{Refused, APPROVAL_TIMEOUT};
use media::send_frames;
use wire::{read_message, write_message, ScreenshareMessage};

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
/// The Android sender gives up after three read timeouts of 10 s.
const SILENCE: Duration = Duration::from_secs(30);
/// TCP connections and writes; the answers wait for a person (approval.rs).
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
        // A heartbeat must not block on a dead link.
        socket.set_write_timeout(Some(ANSWER_TIMEOUT))?;
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

    /// Wait for `wanted`, as long as someone may take to allow it on the panel; return its
    /// version and JSON payload. None at `stop`. `countdown` gets the seconds left, once a second.
    fn wait(&self, wanted: (i32, i32), stop: &AtomicBool, countdown: &dyn Fn(u64)) -> anyhow::Result<Option<(i32, Value)>> {
        let deadline = Instant::now() + APPROVAL_TIMEOUT;
        let mut shown = None;
        while !stop.load(Ordering::SeqCst) {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return Err(Refused("no answer from the panel in 90 s (nobody allowed the cast?)".into()).into());
            };
            if shown != Some(left.as_secs()) {
                shown = Some(left.as_secs());
                countdown(left.as_secs() + 1);
            }
            let m = match self.incoming.recv_timeout(Duration::from_millis(200)) {
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                other => other.context("the panel closed the connection")??,
            };
            tracing::debug!(t = m.command_type, id = m.command_id, payload = %m.payload, "panel message");
            if (m.command_type, m.command_id) == wanted {
                return Ok(Some((m.version, serde_json::from_str(&m.payload).unwrap_or(Value::Null))));
            }
        }
        Ok(None)
    }
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// Cast `screen` to the panel at `host` until `stop`. After a loss, connect again; after a
/// refusal, stop.
pub fn cast(host: SocketAddr, name: &str, s: &Settings, screen: &ScreenCast, stop: &'static AtomicBool, out: &Out) -> anyhow::Result<()> {
    // A first failure is a wrong address or a refusal: report it.
    let mut why = session(host, name, s, screen, stop, out)?;
    // Up to 3 times in 5 min from the terminal, without limit from the menu, as mirror.
    let mut drops = crate::device::Drops::new(matches!(out, Out::Menu(_)));
    while let Some(lost) = why {
        tracing::warn!(%lost, %host, "panel lost");
        let count = drops.count(name, &lost)?;
        out.report(crate::device::Report::Reconnecting);
        out.say(&format!("Lost {name} ({lost}). Reconnecting to the panel ({count})..."));
        std::thread::sleep(crate::device::RECONNECT_DELAY);
        if stop.load(Ordering::SeqCst) {
            break;
        }
        why = match session(host, name, s, screen, stop, out) {
            Err(e) if e.is::<Refused>() => return Err(e),
            other => other.unwrap_or_else(|e| Some(format!("{e:#}"))),
        };
    }
    Ok(())
}

/// One session: Ok(None) at `stop` or when the panel ends the cast, Ok(Some(why)) when it is lost.
fn session(host: SocketAddr, name: &str, s: &Settings, screen: &ScreenCast, stop: &'static AtomicBool, out: &Out) -> anyhow::Result<Option<String>> {
    let mut commands = Commands::open(host)?;
    let local_ip = commands.socket.local_addr()?.ip();
    commands.send(CONNECT, &connect_request(s.pin.as_deref(), local_ip))?;
    tracing::info!(%host, "waiting for approval on the panel");
    // The menu counts down; the terminal says it once per wait.
    let countdown = |left: u64| {
        if matches!(out, Out::Menu(_)) || left == APPROVAL_TIMEOUT.as_secs() {
            out.say(&format!("Click Allow on the panel {name} ({left} s)..."));
        }
    };
    let Some((_, answer)) = commands.wait(CONNECTED, stop, &countdown)? else { return Ok(None) };
    approval::check("connection", &answer)?;
    let (width, height) = s.stream.resolution;
    let audio = json!({"codec": 11, "sampleRate": 44100, "channel": 2, "bitDepth": 16, "samplePerPacket": 1024, "adtsEnabled": true});
    let request = json!({"authorizeMode": 0, "castMode": 0, "displayMode": 0, "supportAudio": !s.stream.no_audio,
        "audio": audio, "video": {"width": width, "height": height}});
    commands.send(CAST, &request)?;
    let Some((version, answer)) = commands.wait(CAST_ANSWER, stop, &countdown)? else { return Ok(None) };
    tracing::info!(version, %answer, "cast answer");
    approval::check("cast", &answer)?;
    out.say(&format!("Casting to {name}"));
    let port = |key: &str| answer[key].as_u64().and_then(|p| u16::try_from(p).ok()).with_context(|| format!("no {key} in {answer}"));
    let video_socket = media::connect(host, port("videoPort")?, ANSWER_TIMEOUT)?;
    let audio_socket = if s.stream.no_audio { None } else { Some(media::connect(host, port("audioPort")?, ANSWER_TIMEOUT)?) };

    // A new capture starts with a key frame, after a reconnection too.
    let capture = Capture::start(Some(screen), &s.stream, s.audio_delay)?;
    let encoder = capture.element("venc").context("no encoder in the pipeline")?;
    // ponytail: leaks one flag per session (a byte), as mirror does: the threads want a 'static flag.
    let ended: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
    let index = (version >= INDEX_VERSION).then_some(0);
    // Live panel of the menu: bytes sent (bitrate), frames dropped, audio delay during the cast.
    let link = media::Link::default();
    let sent = Arc::clone(&link.sent);
    out.send(Status::Served(Box::new(move || sent.load(Ordering::Relaxed))));
    out.send(Status::Dropped(Arc::clone(&capture.dropped)));
    out.send(Status::NetworkDropped(Arc::clone(&link.dropped)));
    if let Some(pad) = capture.audio_pad() {
        out.send(Status::AudioPad(pad));
    }
    let bitrate = s.stream.bitrate;
    let mut senders = vec![send_frames(&capture, "sink", video_socket, index, bitrate, &link, ended)?];
    if let Some(socket) = audio_socket {
        senders.push(send_frames(&capture, "asink", socket, None, bitrate, &link, ended)?);
    }

    let mut touchback = touch::Touchback::new(screen.input.clone(), s.stream.resolution);
    let mut last_heartbeat = Instant::now();
    let mut last_heard = Instant::now();
    let lost = loop {
        if stop.load(Ordering::SeqCst) {
            let _ = commands.send(STOP_CAST, &json!({}));
            break None;
        }
        if ended.load(Ordering::SeqCst) {
            break Some(link.lost().unwrap_or_else(|| "a media connection closed".into()));
        }
        if last_heard.elapsed() > SILENCE {
            break Some(format!("commands: nothing read for {} s", SILENCE.as_secs()));
        }
        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
            if let Err(e) = commands.send(HEARTBEAT, &json!({"sentTimeMs": now_ms()})) {
                break Some(format!("commands: {}", media::cause(&e)));
            }
            last_heartbeat = Instant::now();
        }
        let m = match commands.incoming.recv_timeout(Duration::from_millis(200)) {
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break Some("commands: reader stopped".into()),
            Ok(Err(e)) => break Some(format!("commands: {}", media::cause(&e))),
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
