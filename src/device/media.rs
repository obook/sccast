//! media.rs
//! One playback session on a receiver: send the stream (LOAD), then follow the TV's messages
//! until the playback ends, someone else takes the TV, or the connection drops.
//! playback.rs runs these sessions one after the other and reconnects between them.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::time::Duration;

use anyhow::Context;
use rust_cast::channels::connection::ConnectionResponse;
use rust_cast::channels::heartbeat::HeartbeatResponse;
use rust_cast::channels::media::{IdleReason, Media, MediaResponse, PlayerState};
use rust_cast::channels::receiver::{Application, ReceiverResponse};
use rust_cast::{CastDevice, ChannelMessage};

use super::discovery::CastDevice as Device;

// ponytail: counted over the whole cast, not per hour; reset it if long casts hit the cap.
const MAX_RELOADS: u32 = 3;
// Idle screen of Chromecasts without Google TV: not an app that took the screen.
const BACKDROP: &str = "E8C28D3C";

/// What `Receiver::play` tells the user.
pub enum Report<'a> {
    Say(&'a str),
    Player(&'a PlayerState),
    Reconnecting,
    /// A LOAD succeeded: transport id and media session.
    Loaded(&'a str, i32),
}

/// Why a playback session stopped.
pub(super) enum Outcome {
    /// Another sender took the TV, or a media error closed the app.
    Ended,
    /// The player went idle: playback ended or was stopped (TV remote, or sccast itself).
    Idle,
    /// The device dropped the session: connection lost or receiver closed.
    Lost(String),
}

/// Load `media` in `app`, then follow the device's messages until the session stops.
/// `watcher` ends when the user stops the cast.
pub(super) fn play_session(
    cast: &CastDevice<'static>,
    app: &Application,
    device: &Device,
    media: &Media,
    live: bool,
    watcher: &std::thread::JoinHandle<()>,
    report: &dyn Fn(Report),
) -> anyhow::Result<Outcome> {
    let url = &media.content_id;
    let close_app = || {
        if let Err(e) = cast.receiver.stop_app(app.session_id.clone()) {
            tracing::warn!("cannot close the receiver app: {e}");
        }
    };
    let load = || -> anyhow::Result<i32> {
        let (mut unreadable, mut refused) = (0, 0);
        let loaded = loop {
            match cast.media.load(app.transport_id.clone(), app.session_id.clone(), media) {
                Ok(loaded) => break loaded,
                // A reused receiver may still report errors of the stream it played (e.g. 310,
                // unknown to rust_cast), which fail this LOAD: send it again.
                Err(e @ rust_cast::errors::Error::Parsing(_)) if unreadable < 3 => {
                    unreadable += 1;
                    tracing::warn!("unreadable answer to LOAD ({e}), sending it again");
                    std::thread::sleep(Duration::from_millis(500));
                }
                // rust_cast turns LOAD_FAILED into this message. The Sharp refused once right
                // after sccast was restarted, and accepted the next try.
                Err(rust_cast::errors::Error::Internal(m)) if m == "Failed to load media." => {
                    if refused == MAX_RELOADS {
                        close_app();
                        anyhow::bail!(
                            "{} refused the stream: it may not decode this format, \
                             try a lower --resolution (e.g. 1280x720) or --bitrate",
                            device.name
                        );
                    }
                    refused += 1;
                    tracing::warn!(attempt = refused, "stream refused, loading it again");
                    std::thread::sleep(Duration::from_secs(1));
                }
                Err(e) => return Err(e.into()),
            }
        };
        let media_session = loaded
            .entries
            .first()
            .map(|e| e.media_session_id)
            .context("LOAD returned no media session")?;
        tracing::info!(%url, media_session, "media loaded");
        report(Report::Loaded(&app.transport_id, media_session));
        Ok(media_session)
    };
    // A connection error during a LOAD is a drop: play() reconnects.
    let lost = |e: anyhow::Error| match e.downcast_ref::<rust_cast::errors::Error>() {
        Some(rust_cast::errors::Error::Io(_) | rust_cast::errors::Error::Tls(_)) => Ok(Outcome::Lost(format!("{e:#}"))),
        _ => Err(e),
    };
    let mut media_session = match load() {
        Ok(session) => session,
        Err(e) => return lost(e),
    };
    let mut reloads = 0;
    // Never played: a give-up then means the device cannot decode the stream.
    let mut played = false;

    report(Report::Say(&format!("Casting to {}. Ctrl+C stops.", device.name)));
    let mut app_seen = false;
    loop {
        if watcher.is_finished() {
            return Ok(Outcome::Ended);
        }
        let message = match cast.receive() {
            Ok(message) => message,
            // rust_cast rejects error codes it does not know (e.g. HLS network error 310).
            // The player may recover; a fatal error still arrives as an IDLE status.
            Err(e @ (rust_cast::errors::Error::Parsing(_) | rust_cast::errors::Error::Serialization(_))) => {
                tracing::warn!("unreadable message from the device: {e}");
                continue;
            }
            Err(e) => return Ok(Outcome::Lost(e.to_string())),
        };
        match message {
            ChannelMessage::Heartbeat(HeartbeatResponse::Ping) => {
                if let Err(e) = cast.heartbeat.pong() {
                    return Ok(Outcome::Lost(e.to_string()));
                }
            }
            ChannelMessage::Connection(ConnectionResponse::Close) => {
                return Ok(Outcome::Lost("connection closed by the device".into()));
            }
            ChannelMessage::Media(MediaResponse::Status(status)) => {
                // Skip statuses of the media we replaced (e.g. its IDLE/INTERRUPTED).
                let current = media_session;
                for entry in status.entries.into_iter().filter(|e| e.media_session_id == current) {
                    tracing::info!(state = ?entry.player_state, time = ?entry.current_time, "media");
                    report(Report::Player(&entry.player_state));
                    played |= matches!(entry.player_state, PlayerState::Playing);
                    if matches!(entry.player_state, PlayerState::Idle) && entry.idle_reason.is_some() {
                        tracing::info!(reason = ?entry.idle_reason, "playback ended");
                        // A live stream has no end: the player gave up, so load it again.
                        // Cancelled (TV remote) and Interrupted (another sender) are left alone.
                        let gave_up = matches!(entry.idle_reason, Some(IdleReason::Error | IdleReason::Finished));
                        if live && gave_up && reloads < MAX_RELOADS {
                            reloads += 1;
                            tracing::warn!(attempt = reloads, "reloading the stream");
                            media_session = match load() {
                                Ok(session) => session,
                                Err(e) => return lost(e),
                            };
                            break;
                        }
                        // Not a stop asked by the user: say why, or the menu shows "Stopped".
                        if live && gave_up {
                            close_app();
                            if !played {
                                // A Nest Hub does this with 1080p in HLS (2026-10-21).
                                anyhow::bail!(
                                    "{} could not play the stream: it may not decode this format, \
                                     try 1280x720 or a lower bitrate",
                                    device.name
                                );
                            }
                            anyhow::bail!("{} gave up the stream {} times", device.name, MAX_RELOADS + 1);
                        }
                        return Ok(Outcome::Idle);
                    }
                }
            }
            // A LOAD replaced by the next one (sent again after an unreadable answer): nothing to do.
            ChannelMessage::Media(MediaResponse::LoadCancelled(_)) => {}
            // load() handles the answer to its own LOAD: this one is for an older request.
            ChannelMessage::Media(MediaResponse::LoadFailed(_)) => {}
            ChannelMessage::Media(other) => {
                close_app();
                anyhow::bail!("media error: {other:?}");
            }
            ChannelMessage::Receiver(ReceiverResponse::Status(status)) => {
                // Statuses buffered during the launch may predate our app: only a
                // disappearance after it was seen running means it was closed.
                let running = status.applications.iter().any(|a| a.session_id == app.session_id);
                if app_seen && !running {
                    // Another app on screen: someone cast over us, leave the TV to them.
                    if let Some(other) = status.applications.iter().find(|a| a.app_id != BACKDROP) {
                        tracing::info!(app = %other.display_name, "receiver replaced");
                        return Ok(Outcome::Ended);
                    }
                    return Ok(Outcome::Lost("receiver closed".into()));
                }
                app_seen |= running;
            }
            _ => {}
        }
    }
}
