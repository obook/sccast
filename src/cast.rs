use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use rust_cast::channels::heartbeat::HeartbeatResponse;
use rust_cast::channels::media::{IdleReason, Media, MediaResponse, PlayerState, StreamType};
use rust_cast::channels::receiver::{CastDeviceApp, ReceiverResponse};
use rust_cast::{CastDevice, ChannelMessage};

use crate::discovery::CastDevice as Device;

const RECEIVER: &str = "receiver-0";
const DEFAULT_MEDIA_RECEIVER: &str = "CC1AD845";
// ponytail: counted over the whole cast, not per hour; reset it if long casts hit the cap.
const MAX_RELOADS: u32 = 3;

fn connect(device: &Device) -> anyhow::Result<CastDevice<'static>> {
    // Chromecasts use self-signed certificates.
    let cast = CastDevice::connect_without_host_verification(device.ip.to_string(), device.port)
        .with_context(|| format!("cannot connect to {} ({}:{})", device.name, device.ip, device.port))?;
    cast.connection.connect(RECEIVER)?;
    Ok(cast)
}

/// Close the Default Media Receiver on `device`: the session `session`, or whichever one runs.
/// Apps launched by other senders (YouTube, Netflix...) are left alone.
/// Returns false when there was nothing to close.
pub fn stop(device: &Device, session: Option<&str>) -> anyhow::Result<bool> {
    let cast = connect(device)?;
    let status = cast.receiver.get_status()?;
    let app = status.applications.iter().find(|a| match session {
        Some(session) => a.session_id == session,
        None => a.app_id == DEFAULT_MEDIA_RECEIVER,
    });
    let Some(app) = app else { return Ok(false) };
    cast.receiver.stop_app(app.session_id.clone())?;
    Ok(true)
}

/// Launch the Default Media Receiver on `device`, play `url`, and block until
/// playback ends, the app is closed elsewhere, or `stop` is set.
/// `live` tells the receiver not to buffer ahead (HLS capture).
pub fn play_url(
    device: &Device,
    url: &str,
    content_type: &str,
    live: bool,
    stop: &'static AtomicBool,
) -> anyhow::Result<()> {
    let cast = connect(device)?;
    cast.heartbeat.ping()?;

    // A receiver left running in the background (closed with the TV remote) is reused by
    // LAUNCH without coming back on screen: close it so the TV switches to the new one.
    let status = cast.receiver.get_status()?;
    if let Some(old) = status.applications.iter().find(|a| a.app_id == DEFAULT_MEDIA_RECEIVER) {
        tracing::info!(session = %old.session_id, "closing previous receiver");
        cast.receiver.stop_app(old.session_id.clone())?;
    }

    let app = cast.receiver.launch_app(&CastDeviceApp::DefaultMediaReceiver)?;
    tracing::info!(session = %app.session_id, "receiver launched");
    cast.connection.connect(app.transport_id.clone())?;

    // receive() below blocks until the next message (up to ~5 s): close the app
    // from a second connection so Ctrl+C frees the TV at once.
    let watcher = {
        let device = device.clone();
        let session = app.session_id.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(100));
            }
            tracing::info!("stopping");
            if let Err(e) = self::stop(&device, Some(&session)) {
                tracing::warn!("cannot close the receiver app: {e:#}");
            }
        })
    };

    let media = Media {
        content_id: url.to_string(),
        stream_type: if live { StreamType::Live } else { StreamType::Buffered },
        content_type: content_type.to_string(),
        metadata: None,
        duration: None,
    };
    let load = || -> anyhow::Result<i32> {
        let loaded = cast.media.load(app.transport_id.clone(), app.session_id.clone(), &media)?;
        let media_session = loaded
            .entries
            .first()
            .map(|e| e.media_session_id)
            .context("LOAD returned no media session")?;
        tracing::info!(%url, media_session, "media loaded");
        Ok(media_session)
    };
    let mut media_session = load()?;
    let mut reloads = 0;

    let close_app = || {
        if let Err(e) = cast.receiver.stop_app(app.session_id.clone()) {
            tracing::warn!("cannot close the receiver app: {e}");
        }
    };
    let mut app_seen = false;
    loop {
        if watcher.is_finished() {
            return Ok(());
        }
        let message = match cast.receive() {
            Ok(message) => message,
            // rust_cast rejects error codes it does not know (e.g. HLS network error 310).
            // The player may recover; a fatal error still arrives as an IDLE status.
            Err(e @ (rust_cast::errors::Error::Parsing(_) | rust_cast::errors::Error::Serialization(_))) => {
                tracing::warn!("unreadable message from the device: {e}");
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        match message {
            ChannelMessage::Heartbeat(HeartbeatResponse::Ping) => cast.heartbeat.pong()?,
            ChannelMessage::Media(MediaResponse::Status(status)) => {
                // Skip statuses of the media we replaced (e.g. its IDLE/INTERRUPTED).
                let current = media_session;
                for entry in status.entries.into_iter().filter(|e| e.media_session_id == current) {
                    tracing::info!(state = ?entry.player_state, time = ?entry.current_time, "media");
                    if matches!(entry.player_state, PlayerState::Idle) && entry.idle_reason.is_some() {
                        tracing::info!(reason = ?entry.idle_reason, "playback ended");
                        // A live stream has no end: the player gave up, so load it again.
                        // Cancelled (TV remote) and Interrupted (another sender) are left alone.
                        let gave_up = matches!(entry.idle_reason, Some(IdleReason::Error | IdleReason::Finished));
                        if live && gave_up && reloads < MAX_RELOADS {
                            reloads += 1;
                            tracing::warn!(attempt = reloads, "reloading the stream");
                            media_session = load()?;
                            break;
                        }
                        // Otherwise the TV stays on the Cast splash screen.
                        close_app();
                        return Ok(());
                    }
                }
            }
            ChannelMessage::Media(other) => {
                close_app();
                if matches!(other, MediaResponse::LoadFailed(_)) {
                    anyhow::bail!(
                        "{} refused the stream: it may not decode this format, \
                         try a lower --resolution (e.g. 1280x720) or --bitrate",
                        device.name
                    );
                }
                anyhow::bail!("media error: {other:?}");
            }
            ChannelMessage::Receiver(ReceiverResponse::Status(status)) => {
                // Statuses buffered during the launch may predate our app: only a
                // disappearance after it was seen running means it was closed.
                let running = status.applications.iter().any(|a| a.session_id == app.session_id);
                if app_seen && !running {
                    tracing::info!("receiver app closed");
                    return Ok(());
                }
                app_seen |= running;
            }
            _ => {}
        }
    }
}
