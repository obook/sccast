//! mirror/control.rs
//! Watches the Cast control channel during a mirroring session: heartbeats, player
//! states, and the end of the session on the TV side.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::bail;
use rust_cast::channels::connection::ConnectionResponse;
use rust_cast::channels::heartbeat::HeartbeatResponse;
use rust_cast::channels::media::MediaResponse;
use rust_cast::channels::receiver::ReceiverResponse;
use rust_cast::{CastDevice, ChannelMessage};

use super::MIRRORING_RECEIVER;
use crate::device as cast;
use crate::session::Out;

/// Watch the control channel until `stop`, the TV taken by another sender, or the connection lost.
pub(super) fn control(cast: &CastDevice<'static>, stop: &AtomicBool, out: &Out) -> anyhow::Result<()> {
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
            // The app answered the OFFER: a status without it means it was closed or replaced.
            ChannelMessage::Receiver(ReceiverResponse::Status(status))
                if !status.applications.iter().any(|a| a.app_id == MIRRORING_RECEIVER) =>
            {
                tracing::info!("mirroring receiver closed");
                return Ok(());
            }
            _ => {}
        }
    }
    Ok(())
}
