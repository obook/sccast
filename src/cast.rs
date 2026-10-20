use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;
use rust_cast::channels::heartbeat::HeartbeatResponse;
use rust_cast::channels::media::{Media, MediaResponse, PlayerState, StreamType};
use rust_cast::channels::receiver::{CastDeviceApp, ReceiverResponse};
use rust_cast::{CastDevice, ChannelMessage};

use crate::discovery::CastDevice as Device;

const RECEIVER: &str = "receiver-0";

/// Launch the Default Media Receiver on `device`, play `url`, and block until
/// playback ends, the app is closed elsewhere, or `stop` is set.
pub fn play_url(device: &Device, url: &str, content_type: &str, stop: &AtomicBool) -> anyhow::Result<()> {
    // Chromecasts use self-signed certificates.
    let cast = CastDevice::connect_without_host_verification(device.ip.to_string(), device.port)
        .with_context(|| format!("cannot connect to {} ({}:{})", device.name, device.ip, device.port))?;
    cast.connection.connect(RECEIVER)?;
    cast.heartbeat.ping()?;

    let app = cast.receiver.launch_app(&CastDeviceApp::DefaultMediaReceiver)?;
    tracing::info!(session = %app.session_id, "receiver launched");
    cast.connection.connect(app.transport_id.clone())?;

    let media = Media {
        content_id: url.to_string(),
        stream_type: StreamType::Buffered,
        content_type: content_type.to_string(),
        metadata: None,
        duration: None,
    };
    let loaded = cast.media.load(app.transport_id.clone(), app.session_id.clone(), &media)?;
    let media_session = loaded
        .entries
        .first()
        .map(|e| e.media_session_id)
        .context("LOAD returned no media session")?;
    tracing::info!(%url, media_session, "media loaded");

    // ponytail: the stop flag is checked between messages, so Ctrl+C can take up to
    // one heartbeat (~5 s) to act; move to a select loop with the J6 server.
    let mut app_seen = false;
    loop {
        if stop.load(Ordering::SeqCst) {
            tracing::info!("stopping");
            cast.receiver.stop_app(app.session_id.clone())?;
            return Ok(());
        }
        match cast.receive()? {
            ChannelMessage::Heartbeat(HeartbeatResponse::Ping) => cast.heartbeat.pong()?,
            ChannelMessage::Media(MediaResponse::Status(status)) => {
                // Skip statuses of the media we replaced (e.g. its IDLE/INTERRUPTED).
                for entry in status.entries.into_iter().filter(|e| e.media_session_id == media_session) {
                    tracing::info!(state = ?entry.player_state, time = ?entry.current_time, "media");
                    if matches!(entry.player_state, PlayerState::Idle) && entry.idle_reason.is_some() {
                        tracing::info!(reason = ?entry.idle_reason, "playback ended");
                        // Otherwise the TV stays on the Cast splash screen.
                        cast.receiver.stop_app(app.session_id.clone())?;
                        return Ok(());
                    }
                }
            }
            ChannelMessage::Media(other) => anyhow::bail!("media error: {other:?}"),
            ChannelMessage::Receiver(ReceiverResponse::Status(status)) => {
                // Statuses buffered during the launch may predate our app: only a
                // disappearance after it was seen running means it was closed.
                let running = status.applications.iter().any(|a| a.session_id == app.session_id);
                if app_seen && !running {
                    tracing::info!("receiver app closed by another sender");
                    return Ok(());
                }
                app_seen |= running;
            }
            _ => {}
        }
    }
}
