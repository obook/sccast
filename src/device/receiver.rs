//! receiver.rs
//! Start and close the receiver app on a Cast device: sccast's own page, Google's Default
//! Media Receiver as a fallback, or the mirroring receiver. playback.rs plays on it.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use rust_cast::channels::receiver::{Application, CastDeviceApp};
use rust_cast::CastDevice;

use super::discovery::CastDevice as Device;

const RECEIVER: &str = "receiver-0";
pub const DEFAULT_MEDIA_RECEIVER: &str = "CC1AD845";
/// sccast's own receiver (docs/receiver/), registered in the Cast console: plays closer to live.
pub const SCCAST_RECEIVER: &str = "8FE12008";
// At its first launch a Chromecast gave up loading sccast's receiver after 63 s (CAST_INIT_TIMEOUT).
// Wait for that answer: retrying sooner may restart its loading (not checked).
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(75);

fn connect(device: &Device) -> anyhow::Result<CastDevice<'static>> {
    // Chromecasts use self-signed certificates.
    let cast = CastDevice::connect_without_host_verification(device.ip.to_string(), device.port)
        .with_context(|| format!("cannot connect to {} ({}:{})", device.name, device.ip, device.port))?;
    cast.connection.connect(RECEIVER)?;
    Ok(cast)
}

/// Close a receiver app on `device`: the session `session`, or whichever of `app_id` and the
/// Default Media Receiver (its fallback) runs.
/// Apps launched by other senders (YouTube, Netflix...) are left alone. A Default Media
/// Receiver started by another sender cannot be told from sccast's fallback: it is closed too.
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
pub(super) fn stop_media(device: &Device, transport_id: &str, media_session: i32) -> anyhow::Result<()> {
    let cast = connect(device)?;
    cast.connection.connect(transport_id.to_string())?;
    cast.media.stop(transport_id.to_string(), media_session)?;
    Ok(())
}

/// Receiver app running on a device, ready to play.
pub struct Receiver {
    pub(super) cast: CastDevice<'static>,
    pub(super) app: Application,
    pub(super) device: Device,
    /// Launched by the menu: reconnect without limit, the user sees it and can stop it.
    pub(super) forever: bool,
}

/// Launch the receiver app `app_id` on `device`. If a custom receiver does not start
/// (unknown to the device, page not loading), fall back to the Default Media Receiver.
/// The TV shows the Cast screen.
/// With `reuse`, attach to that receiver if it already runs (left open by the menu between
/// two casts): the Sharp TV sometimes never answered a launch right after a close.
/// Gives up after LAUNCH_TIMEOUT or when `stop` is set: rust_cast has no timeout of its own.
pub fn launch(device: &Device, app_id: &str, reuse: bool, stop: &'static AtomicBool) -> anyhow::Result<Receiver> {
    let (sender, answer) = std::sync::mpsc::channel();
    let (target, id) = (device.clone(), app_id.to_string());
    // ponytail: a launch given up stays blocked on its socket, until the device answers or closes it.
    std::thread::spawn(move || {
        // Nobody waits any more (stop or timeout): close the receiver a late answer opened.
        if let Err(std::sync::mpsc::SendError(Ok(late))) = sender.send(start(&target, &id, reuse)) {
            late.close();
        }
    });
    let started = Instant::now();
    while started.elapsed() < LAUNCH_TIMEOUT {
        if stop.load(Ordering::SeqCst) {
            anyhow::bail!("stopped before the receiver started");
        }
        match answer.recv_timeout(Duration::from_millis(100)) {
            Ok(result) => return result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(e) => anyhow::bail!("launch thread ended: {e}"),
        }
    }
    anyhow::bail!("{} did not start the receiver within {} s", device.name, LAUNCH_TIMEOUT.as_secs())
}

fn start(device: &Device, app_id: &str, reuse: bool) -> anyhow::Result<Receiver> {
    let cast = connect(device)?;
    cast.heartbeat.ping()?;

    let status = cast.receiver.get_status()?;
    let ours = |a: &&Application| a.app_id == app_id || a.app_id == DEFAULT_MEDIA_RECEIVER;
    if reuse {
        if let Some(app) = status.applications.iter().find(ours) {
            tracing::info!(session = %app.session_id, app_id = %app.app_id, "reusing the receiver");
            cast.connection.connect(app.transport_id.clone())?;
            return Ok(Receiver { cast, app: app.clone(), device: device.clone(), forever: reuse });
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
        // The menu tries sccast's receiver again: its next launch took 2 s, Google's plays 5 s late.
        Err(e) if reuse && e.to_string().contains("CAST_INIT_TIMEOUT") => {
            return Err(e).with_context(|| format!("{} could not load the sccast receiver", device.name));
        }
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
    Ok(Receiver { cast, app, device: device.clone(), forever: reuse })
}

impl Receiver {
    /// Id of the receiver app that actually runs (differs from the requested one after a fallback).
    pub fn app_id(&self) -> &str {
        &self.app.app_id
    }

    /// Connection to the device, attached to the receiver app.
    pub fn channel(&self) -> &CastDevice<'static> {
        &self.cast
    }

    pub fn session_id(&self) -> &str {
        &self.app.session_id
    }

    /// Close the receiver app, freeing the TV.
    pub fn close(&self) {
        if let Err(e) = self.cast.receiver.stop_app(self.app.session_id.clone()) {
            tracing::warn!("cannot close the receiver app: {e}");
        }
    }
}
