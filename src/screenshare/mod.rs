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
mod pointer;
mod touch;
pub mod trace;
mod wire;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use anyhow::Context;
use serde_json::{json, Value};

use crate::capture::{request_key_frame, Capture, ScreenCast};
use crate::menu::{Settings, Status};
use crate::session::Out;
use approval::{Refused, APPROVAL_TIMEOUT};
use commands::{connect_request, Commands, CAST_STOPPED, DISCONNECTED};
use media::send_frames;

/// (commandType, commandId) pairs, from the sender's ProtocolConstant (more in commands.rs).
const CONNECT: (i32, i32) = (1, 1);
const CONNECTED: (i32, i32) = (1, 101);
const CAST: (i32, i32) = (6, 2);
const STOP_CAST: (i32, i32) = (6, 3);
const CAST_ANSWER: (i32, i32) = (6, 102);
const KEY_FRAME: (i32, i32) = (6, 113);
/// Receiver version from which video frames carry an index.
const INDEX_VERSION: i32 = 63;
/// The Android sender gives up after three read timeouts of 10 s, the Windows client after 30 s.
const SILENCE: Duration = Duration::from_secs(30);
/// While the panel still holds the lost session: a try every 5 s, for a bit more than its 30 s.
const BUSY_RETRY: Duration = Duration::from_secs(5);
const BUSY_WAIT: Duration = Duration::from_secs(35);

/// Check that the panel answers; return the address that did (one of the code table's ports).
pub fn find(host: SocketAddr) -> anyhow::Result<SocketAddr> {
    commands::reach(host, &AtomicBool::new(false)).map(|(_, address)| address)
}

/// Cast `screen` to the panel at `host` until `stop`. After a loss, connect again; after a
/// refusal, stop.
pub fn cast(host: SocketAddr, name: &str, s: &Settings, screen: &ScreenCast, stop: &'static AtomicBool, out: &Out) -> anyhow::Result<()> {
    let panel = Panel { host, name, settings: s, screen, stop, out };
    trace::line(&format!("sccast {} casting to {name} at {host}, {:?}", env!("CARGO_PKG_VERSION"), s.stream.resolution));
    let pointer = panel.input().map(|input| format!("pointer granted on a {:?} screen", input.size));
    trace::line(&format!("touchback: {}", pointer.unwrap_or_else(|| "off (setting off, or pointer not granted)".into())));
    // A first failure is a wrong address or a refusal: report it.
    let mut why = panel.attempt()?;
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
        why = match panel.attempt() {
            Err(e) if e.is::<Refused>() => return Err(e),
            other => other.unwrap_or_else(|e| Some(format!("{e:#}"))),
        };
    }
    Ok(())
}

/// One cast to one panel: what every session needs.
struct Panel<'a> {
    host: SocketAddr,
    name: &'a str,
    settings: &'a Settings,
    screen: &'a ScreenCast,
    stop: &'static AtomicBool,
    out: &'a Out,
}

impl Panel<'_> {
    /// The pointer for touchback. The setting decides: a pointer granted earlier stays
    /// unused when it is off.
    fn input(&self) -> Option<Arc<crate::capture::Input>> {
        self.screen.input().filter(|_| self.settings.touchback)
    }

    /// A session, tried again every 5 s for 35 s while the panel still holds the previous one:
    /// it drops a lost session only after its own 30 s without news (Windows client).
    fn attempt(&self) -> anyhow::Result<Option<String>> {
        let started = Instant::now();
        loop {
            let result = self.session();
            // A stop during a session or a retry ends it quietly, without a new connection.
            if self.stop.load(Ordering::SeqCst) {
                return Ok(None);
            }
            let busy = matches!(&result, Err(e) if e.downcast_ref::<Refused>().is_some_and(|refused| refused.retry));
            if !busy || started.elapsed() >= BUSY_WAIT {
                return result.inspect_err(|e| trace::line(&format!("failed: {e:#}")));
            }
            trace::line("the panel still holds the previous session: trying again in 5 s");
            self.out.say(&format!("{} still holds the previous session: trying again in 5 s...", self.name));
            std::thread::sleep(BUSY_RETRY);
        }
    }

    /// One session: Ok(None) at `stop` or when the panel ends the cast, Ok(Some(why)) when it
    /// is lost.
    fn session(&self) -> anyhow::Result<Option<String>> {
        let mut commands = Commands::open(self.host, self.stop)?;
        let Some((version, answer)) = self.negotiate(&mut commands)? else {
            return Ok(None);
        };
        self.stream(commands, version, &answer)
    }

    /// Connect, then ask for the cast, each answer waiting for a person at the panel.
    /// Returns the panel's version and its answer to the cast request; None at `stop`.
    fn negotiate(&self, commands: &mut Commands) -> anyhow::Result<Option<(i32, Value)>> {
        let local_ip = commands.socket.local_addr()?.ip();
        commands.send(CONNECT, &connect_request(self.settings.pin.as_deref(), local_ip))?;
        tracing::info!(host = %self.host, "waiting for approval on the panel");
        // The menu counts down; the terminal says it once per wait.
        let countdown = |left: u64| {
            if matches!(self.out, Out::Menu(_)) || left == APPROVAL_TIMEOUT.as_secs() {
                self.out.say(&format!("Click Allow on the panel {} ({left} s)...", self.name));
            }
        };
        let Some((_, answer)) = commands.wait(CONNECTED, self.stop, &countdown)? else {
            return Ok(None);
        };
        approval::check("connection", &answer)?;
        let stream = &self.settings.stream;
        let (width, height) = stream.resolution;
        let audio = json!({"codec": 11, "sampleRate": 44100, "channel": 2, "bitDepth": 16, "samplePerPacket": 1024, "adtsEnabled": true});
        let request = json!({"authorizeMode": 0, "castMode": 0, "displayMode": 0, "supportAudio": !stream.no_audio,
            "audio": audio, "video": {"width": width, "height": height}});
        commands.send(CAST, &request)?;
        let Some((version, answer)) = commands.wait(CAST_ANSWER, self.stop, &countdown)? else {
            return Ok(None);
        };
        tracing::info!(version, %answer, "cast answer");
        approval::check("cast", &answer)?;
        self.out.say(&format!("Casting to {}", self.name));
        Ok(Some((version, answer)))
    }

    /// Send the screen and sound on the ports of `answer` and follow the commands until the
    /// end of the session.
    fn stream(&self, mut commands: Commands, version: i32, answer: &Value) -> anyhow::Result<Option<String>> {
        let s = self.settings;
        let port = |key: &str| answer[key].as_u64().and_then(|p| u16::try_from(p).ok()).with_context(|| format!("no {key} in {answer}"));
        let video_socket = media::connect(self.host, port("videoPort")?)?;
        let audio_socket = if s.stream.no_audio { None } else { Some(media::connect(self.host, port("audioPort")?)?) };

        // A new capture starts with a key frame, after a reconnection too.
        let capture = Capture::start(Some(self.screen), &s.stream, s.audio_delay)?;
        let encoder = capture.element("venc").context("no encoder in the pipeline")?;
        // ponytail: leaks one flag per session (a byte), as mirror does: the threads want a 'static flag.
        let ended: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let link = media::Link::default();
        self.show_stats(&capture, &link);
        let index = (version >= INDEX_VERSION).then_some(0);
        let mut senders = vec![send_frames(&capture, "sink", video_socket, index, s.stream.bitrate, &link, ended)?];
        if let Some(socket) = audio_socket {
            senders.push(send_frames(&capture, "asink", socket, None, s.stream.bitrate, &link, ended)?);
        }

        let mut touchback = touch::Touchback::new(self.input(), self.host.ip(), s.stream.resolution);
        let mut last_heard = Instant::now();
        let lost = loop {
            if self.stop.load(Ordering::SeqCst) {
                let _ = commands.send(STOP_CAST, &json!({}));
                break None;
            }
            if ended.load(Ordering::SeqCst) {
                break Some(link.lost().unwrap_or_else(|| "a media connection closed".into()));
            }
            if last_heard.elapsed() > SILENCE {
                break Some(format!("commands: nothing read for {} s", SILENCE.as_secs()));
            }
            if let Err(e) = commands.keep_alive() {
                break Some(format!("commands: {}", media::cause(&e)));
            }
            let message = match commands.incoming.recv_timeout(Duration::from_millis(200)) {
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break Some("commands: reader stopped".into()),
                Ok(Err(e)) => break Some(format!("commands: {}", media::cause(&e))),
                Ok(Ok(message)) => message,
            };
            last_heard = Instant::now();
            match (message.command_type, message.command_id) {
                KEY_FRAME => request_key_frame(&encoder),
                CAST_STOPPED | DISCONNECTED => break None,
                _ => match touchback.handle(&message) {
                    Some((ids, payload)) => drop(commands.reply(ids, message.sequence, &payload)),
                    None => tracing::debug!(?message, "panel message"),
                },
            }
        };
        ended.store(true, Ordering::SeqCst);
        drop(commands);
        for sender in senders {
            let _ = sender.join();
        }
        Ok(lost)
    }

    /// Live panel of the menu: bytes sent (bitrate), frames dropped, audio delay during the cast.
    fn show_stats(&self, capture: &Capture, link: &media::Link) {
        let sent = Arc::clone(&link.sent);
        self.out.send(Status::Served(Box::new(move || sent.load(Ordering::Relaxed))));
        self.out.send(Status::Dropped(Arc::clone(&capture.dropped)));
        self.out.send(Status::NetworkDropped(Arc::clone(&link.dropped)));
        if let Some(pad) = capture.audio_pad() {
            self.out.send(Status::AudioPad(pad));
        }
    }
}
