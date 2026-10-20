use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use rust_cast::channels::connection::ConnectionResponse;
use rust_cast::channels::heartbeat::HeartbeatResponse;
use rust_cast::channels::media::{IdleReason, Media, MediaResponse, PlayerState, StreamType};
use rust_cast::channels::receiver::{Application, CastDeviceApp, ReceiverResponse};
use rust_cast::{CastDevice, ChannelMessage};

use crate::discovery::CastDevice as Device;

const RECEIVER: &str = "receiver-0";
pub const DEFAULT_MEDIA_RECEIVER: &str = "CC1AD845";
/// sccast's own receiver (docs/receiver/), registered in the Cast console: plays closer to live.
pub const SCCAST_RECEIVER: &str = "8FE12008";
// ponytail: counted over the whole cast, not per hour; reset it if long casts hit the cap.
const MAX_RELOADS: u32 = 3;
// Idle screen of Chromecasts without Google TV: not an app that took the screen.
const BACKDROP: &str = "E8C28D3C";
const MAX_RECONNECTS: u32 = 3;
// A drop this long after the previous one starts a new count.
const RECONNECT_WINDOW: Duration = Duration::from_secs(300);
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

fn connect(device: &Device) -> anyhow::Result<CastDevice<'static>> {
    // Chromecasts use self-signed certificates.
    let cast = CastDevice::connect_without_host_verification(device.ip.to_string(), device.port)
        .with_context(|| format!("cannot connect to {} ({}:{})", device.name, device.ip, device.port))?;
    cast.connection.connect(RECEIVER)?;
    Ok(cast)
}

/// Close a receiver app on `device`: the session `session`, or whichever of `app_id` and the
/// Default Media Receiver (its fallback) runs.
/// Apps launched by other senders (YouTube, Netflix...) are left alone.
/// Returns false when there was nothing to close.
pub fn stop(device: &Device, app_id: &str, session: Option<&str>) -> anyhow::Result<bool> {
    let cast = connect(device)?;
    let status = cast.receiver.get_status()?;
    let app = status.applications.iter().find(|a| match session {
        Some(session) => a.session_id == session,
        None => a.app_id == app_id || a.app_id == DEFAULT_MEDIA_RECEIVER,
    });
    let Some(app) = app else { return Ok(false) };
    cast.receiver.stop_app(app.session_id.clone())?;
    Ok(true)
}

/// Stop the media playing in a receiver app, leaving the app open.
fn stop_media(device: &Device, transport_id: &str, media_session: i32) -> anyhow::Result<()> {
    let cast = connect(device)?;
    cast.connection.connect(transport_id.to_string())?;
    cast.media.stop(transport_id.to_string(), media_session)?;
    Ok(())
}

/// Receiver app running on a device, ready to play.
pub struct Receiver {
    cast: CastDevice<'static>,
    app: Application,
    device: Device,
}

/// Launch the receiver app `app_id` on `device`. If a custom receiver does not start
/// (unknown to the device, page not loading), fall back to the Default Media Receiver.
/// The TV shows the Cast screen.
/// With `reuse`, attach to that receiver if it already runs (left open by the menu between
/// two casts): the Sharp TV sometimes never answered a launch right after a close.
pub fn launch(device: &Device, app_id: &str, reuse: bool) -> anyhow::Result<Receiver> {
    let cast = connect(device)?;
    cast.heartbeat.ping()?;

    let status = cast.receiver.get_status()?;
    let ours = |a: &&Application| a.app_id == app_id || a.app_id == DEFAULT_MEDIA_RECEIVER;
    if reuse {
        if let Some(app) = status.applications.iter().find(ours) {
            tracing::info!(session = %app.session_id, app_id = %app.app_id, "reusing the receiver");
            cast.connection.connect(app.transport_id.clone())?;
            return Ok(Receiver { cast, app: app.clone(), device: device.clone() });
        }
    }
    // A receiver left running in the background (closed with the TV remote) is reused by
    // LAUNCH without coming back on screen: close it so the TV switches to the new one.
    for old in status.applications.iter().filter(ours) {
        tracing::info!(session = %old.session_id, "closing previous receiver");
        cast.receiver.stop_app(old.session_id.clone())?;
    }

    let app = match cast.receiver.launch_app(&CastDeviceApp::Custom(app_id.to_string())) {
        Ok(app) => app,
        Err(e) if app_id != DEFAULT_MEDIA_RECEIVER => {
            tracing::warn!("cannot launch the receiver app {app_id}: {e}");
            cast.receiver
                .launch_app(&CastDeviceApp::DefaultMediaReceiver)
                .with_context(|| format!("cannot launch the Default Media Receiver on {}", device.name))?
        }
        Err(e) => return Err(e).with_context(|| format!("cannot launch the receiver app {app_id} on {}", device.name)),
    };
    tracing::info!(session = %app.session_id, app_id = %app.app_id, "receiver launched");
    cast.connection.connect(app.transport_id.clone())?;
    Ok(Receiver { cast, app, device: device.clone() })
}

impl Receiver {
    /// Id of the receiver app that actually runs (differs from the requested one after a fallback).
    pub fn app_id(&self) -> &str {
        &self.app.app_id
    }

    /// Close the receiver app, freeing the TV.
    pub fn close(&self) {
        if let Err(e) = self.cast.receiver.stop_app(self.app.session_id.clone()) {
            tracing::warn!("cannot close the receiver app: {e}");
        }
    }

    /// Play `url` and block until playback ends, the TV is taken by another sender, or `stop` is set.
    /// If the device drops the session, launch the receiver again and resume.
    /// `live` tells the receiver not to buffer ahead (HLS capture).
    /// `report` shows progress to the user (terminal or menu).
    /// If `keep` is set when `stop` comes, the receiver app stays open for the next cast.
    pub fn play(
        self,
        url: &str,
        content_type: &str,
        live: bool,
        stop: &'static AtomicBool,
        keep: &'static AtomicBool,
        report: &dyn Fn(Report),
    ) -> anyhow::Result<()> {
        let mut receiver = self;
        // receive() blocks until the next message (up to ~5 s): close the app
        // from a second connection so Ctrl+C frees the TV at once.
        let session = Arc::new(Mutex::new(receiver.app.session_id.clone()));
        // Transport and media session of the current playback, set by each LOAD.
        let playing: Arc<Mutex<Option<(String, i32)>>> = Arc::default();
        let app_id = receiver.app.app_id.clone();
        let watcher = {
            let device = receiver.device.clone();
            let app_id = app_id.clone();
            let session = Arc::clone(&session);
            let playing = Arc::clone(&playing);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(100));
                }
                tracing::info!("stopping");
                if keep.load(Ordering::SeqCst) {
                    // Leave the app open, but stop its player: it would retry the dead stream forever.
                    let current = playing.lock().unwrap().clone();
                    if let Some((transport, media_session)) = current {
                        if let Err(e) = stop_media(&device, &transport, media_session) {
                            tracing::warn!("cannot stop the media: {e:#}");
                        }
                    }
                    return;
                }
                let session = session.lock().unwrap().clone();
                if let Err(e) = self::stop(&device, &app_id, Some(&session)) {
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
        let device = receiver.device.clone();
        let mut drops = 0;
        let mut last_drop = Instant::now();
        loop {
            let track = |r: Report| {
                if let Report::Loaded(transport, media_session) = r {
                    *playing.lock().unwrap() = Some((transport.to_string(), media_session));
                }
                report(r);
            };
            let mut why = match play_session(&receiver.cast, &receiver.app, &device, &media, live, &watcher, &track)? {
                Outcome::Ended => return Ok(()),
                // Otherwise the TV stays on the Cast splash screen. Kept open when the menu
                // stopped the media itself (s, or Enter to cast again).
                Outcome::Idle => {
                    if !keep.load(Ordering::SeqCst) {
                        receiver.close();
                    }
                    return Ok(());
                }
                Outcome::Lost(why) => why,
            };
            loop {
                // Ctrl+C also closes the session: not a drop.
                if stop.load(Ordering::SeqCst) {
                    return Ok(());
                }
                tracing::warn!(%why, "session lost");
                if last_drop.elapsed() > RECONNECT_WINDOW {
                    drops = 0;
                }
                drops += 1;
                last_drop = Instant::now();
                if drops > MAX_RECONNECTS {
                    anyhow::bail!(
                        "lost {} ({why}), gave up after {MAX_RECONNECTS} reconnections. \
                         If it no longer shows in `sccast list`, unplug it and plug it back in",
                        device.name
                    );
                }
                report(Report::Reconnecting);
                report(Report::Say(&format!("Lost {} ({why}). Reconnecting ({drops}/{MAX_RECONNECTS})...", device.name)));
                std::thread::sleep(RECONNECT_DELAY);
                // A fresh launch: the session was lost, an attached app may be in the background.
                match launch(&device, &app_id, false) {
                    Ok(new) => {
                        receiver = new;
                        *session.lock().unwrap() = receiver.app.session_id.clone();
                        break;
                    }
                    Err(e) => why = format!("{e:#}"),
                }
            }
            if stop.load(Ordering::SeqCst) {
                if !keep.load(Ordering::SeqCst) {
                    receiver.close();
                }
                return Ok(());
            }
        }
    }
}

/// What `Receiver::play` tells the user.
pub enum Report<'a> {
    Say(&'a str),
    Player(&'a PlayerState),
    Reconnecting,
    /// A LOAD succeeded: transport id and media session.
    Loaded(&'a str, i32),
}

/// Why a playback session stopped.
enum Outcome {
    /// Another sender took the TV, or a media error closed the app.
    Ended,
    /// The player went idle: playback ended or was stopped (TV remote, or sccast itself).
    Idle,
    /// The device dropped the session: connection lost or receiver closed.
    Lost(String),
}

fn play_session(
    cast: &CastDevice<'static>,
    app: &Application,
    device: &Device,
    media: &Media,
    live: bool,
    watcher: &std::thread::JoinHandle<()>,
    report: &dyn Fn(Report),
) -> anyhow::Result<Outcome> {
    let url = &media.content_id;
    let load = || -> anyhow::Result<i32> {
        let mut tries = 0;
        let loaded = loop {
            match cast.media.load(app.transport_id.clone(), app.session_id.clone(), media) {
                Ok(loaded) => break loaded,
                // A reused receiver may still report errors of the stream it played (e.g. 310,
                // unknown to rust_cast), which fail this LOAD: send it again.
                Err(e @ rust_cast::errors::Error::Parsing(_)) if tries < 3 => {
                    tries += 1;
                    tracing::warn!("unreadable answer to LOAD ({e}), sending it again");
                    std::thread::sleep(Duration::from_millis(500));
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
    let mut media_session = load()?;
    let mut reloads = 0;

    let close_app = || {
        if let Err(e) = cast.receiver.stop_app(app.session_id.clone()) {
            tracing::warn!("cannot close the receiver app: {e}");
        }
    };
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
                        return Ok(Outcome::Idle);
                    }
                }
            }
            // A LOAD replaced by the next one (sent again after an unreadable answer): nothing to do.
            ChannelMessage::Media(MediaResponse::LoadCancelled(_)) => {}
            ChannelMessage::Media(other) => {
                // The Sharp refused once right after sccast was restarted, and accepted the next try.
                if matches!(other, MediaResponse::LoadFailed(_)) && reloads < MAX_RELOADS {
                    reloads += 1;
                    tracing::warn!(attempt = reloads, "stream refused, loading it again");
                    std::thread::sleep(Duration::from_secs(1));
                    media_session = load()?;
                    continue;
                }
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
