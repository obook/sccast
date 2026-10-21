//! screenshare/
//! Cast the screen to an interactive panel in its "classic" mode: a TCP command channel,
//! then one TCP connection for the video and one for the sound. Read from the panel's
//! Android sender; unverified on a real panel. The protocol is in docs/screenshare.md.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

mod approval;
mod commands;
mod media;
pub mod pin;
mod touch;
pub mod trace;
mod wire;

use std::net::{Shutdown, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use anyhow::Context;
use serde_json::{json, Value};

use crate::capture::{request_key_frame, Capture, ScreenCast};
use crate::menu::{Settings, Status};
use crate::session::Out;
use approval::{Refused, APPROVAL_TIMEOUT};
use commands::{now_ms, Commands, ANSWER_TIMEOUT};
use media::send_frames;

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
/// The Android sender gives up after three read timeouts of 10 s, the Windows client after 30 s.
const SILENCE: Duration = Duration::from_secs(30);
/// While the panel still holds the lost session: a try every 5 s, for a bit more than its 30 s.
const BUSY_RETRY: Duration = Duration::from_secs(5);
const BUSY_WAIT: Duration = Duration::from_secs(35);

/// Cast `screen` to the panel at `host` until `stop`. After a loss, connect again; after a
/// refusal, stop.
pub fn cast(host: SocketAddr, name: &str, s: &Settings, screen: &ScreenCast, stop: &'static AtomicBool, out: &Out) -> anyhow::Result<()> {
    trace::line(&format!("sccast {} casting to {name} at {host}, {:?}", env!("CARGO_PKG_VERSION"), s.stream.resolution));
    // A first failure is a wrong address or a refusal: report it.
    let mut why = attempt(host, name, s, screen, stop, out)?;
    // Up to 3 times in 5 min from the terminal, without limit from the menu, as mirror.
    let mut drops = crate::device::Drops::new(matches!(out, Out::Menu(_)));
    while let Some(lost) = why {
        tracing::warn!(%lost, %host, "panel lost");
        trace::line(&format!("lost: {lost}"));
        let count = drops.count(name, &lost)?;
        out.report(crate::device::Report::Reconnecting);
        out.say(&format!("Lost {name} ({lost}). Reconnecting to the panel ({count})..."));
        std::thread::sleep(crate::device::RECONNECT_DELAY);
        if stop.load(Ordering::SeqCst) {
            break;
        }
        why = match attempt(host, name, s, screen, stop, out) {
            Err(e) if e.is::<Refused>() => return Err(e),
            other => other.unwrap_or_else(|e| Some(format!("{e:#}"))),
        };
    }
    Ok(())
}

/// A session, tried again every 5 s for 35 s while the panel still holds the previous one:
/// it drops a lost session only after its own 30 s without news (Windows client).
fn attempt(host: SocketAddr, name: &str, s: &Settings, screen: &ScreenCast, stop: &'static AtomicBool, out: &Out) -> anyhow::Result<Option<String>> {
    let started = Instant::now();
    loop {
        match session(host, name, s, screen, stop, out) {
            Err(e) if e.downcast_ref::<Refused>().is_some_and(|r| r.retry) && started.elapsed() < BUSY_WAIT && !stop.load(Ordering::SeqCst) => {
                trace::line(&format!("{e:#}: trying again in 5 s"));
                out.say(&format!("{name} still holds the previous session: trying again in 5 s..."));
                std::thread::sleep(BUSY_RETRY);
            }
            other => return other.inspect_err(|e| trace::line(&format!("failed: {e:#}"))),
        }
    }
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

/// Connect request, with the fields the Windows client fills (keys from its builder; both
/// spellings of the code appear there). Which ones the panel needs is unverified. Hypothesis:
/// deviceType 5, written by the Windows client, stands for a PC (the Android sender writes 1).
/// The device id stays the same across reconnections (machine-id).
fn connect_request(pin: Option<&str>, local_ip: std::net::IpAddr) -> Value {
    let read = |path| std::fs::read_to_string(path).map(|s| s.trim().to_string()).ok().filter(|s| !s.is_empty());
    let name = read("/etc/hostname").unwrap_or_else(|| "sccast".to_string());
    let uuid = read("/etc/machine-id").unwrap_or_else(|| name.clone());
    let pin = pin.unwrap_or("");
    json!({"pincode": pin, "pinCode": pin, "deviceName": name, "deviceType": 5, "connectType": 0,
        "deviceVersion": env!("CARGO_PKG_VERSION"), "deviceUuid": uuid, "originIp": local_ip.to_string(),
        "supportMultiTouch": false, "supportPen": false, "functions": [], "systemName": "Linux"})
}
