//! mirror/
//! Mirror format (`--format mirror`): H.264 and Opus over encrypted RTP to Google's
//! mirroring receiver, about 150 ms behind the screen instead of 2 to 3 s with HLS.
//! This file launches the receiver and reconnects. session.rs runs one session,
//! offer.rs opens it, send.rs runs its threads, control.rs watches the Cast channel,
//! rtp.rs builds the packets, rtcp.rs reads the receiver's reports, rate.rs adapts the bitrate.
//! Protocol from openscreen/cast/streaming/impl. Notes in docs/cast-streaming.md.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

mod control;
mod offer;
mod rate;
mod rtcp;
mod rtp;
mod send;
mod session;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::capture::ScreenCast;
use crate::device::{self as cast, CastDevice as Device};
use crate::menu;
use crate::session::Out;
use session::{stream, Session};

/// Google's mirroring receiver ("Chrome Mirroring").
pub const MIRRORING_RECEIVER: &str = "0F5096E8";
const NAMESPACE: &str = "urn:x-cast:com.google.cast.webrtc";
/// Receiver's playout delay at the start. rate.rs raises it up to 400 ms on a lossy network,
/// when the receiver accepts the adaptive delay extension.
const TARGET_DELAY_MS: u32 = 100;
const VIDEO_SSRC: u32 = 0x5cca_5701;
const AUDIO_SSRC: u32 = 0x5cca_5702;
// openscreen's RtpPayloadType values.
const H264_PAYLOAD: u8 = 101;
const OPUS_PAYLOAD: u8 = 96;
const AUDIO_BITRATE: u32 = 128_000;
const REPORT_INTERVAL: Duration = Duration::from_millis(100);
/// Offered again when the receiver refuses the video. No device seen refusing yet: a Nest Hub
/// refuses 1080p in HLS but takes it in mirror (2026-10-21).
const SMALLER_VIDEO: (u32, u32) = (1280, 720);
/// The receiver repeats its key frame request in each report until one arrives.
const KEY_FRAME_SPACING: Duration = Duration::from_secs(1);
/// No RTCP from the TV for this long: the session is lost. Longer before the first report,
/// as chromiacast does: the TV may take a while to set its media path up.
const SILENCE_START: Duration = Duration::from_secs(10);
const SILENCE: Duration = Duration::from_secs(5);

/// How a session ended.
enum Outcome {
    /// Stopped by the user, or the app closed on the TV (remote, another sender).
    Ended,
    /// TV unreachable or session dropped: worth launching the receiver again.
    Lost(String),
}

/// Mirror `screen` to `device` until `stop`, or until the TV is taken. A lost session is
/// started again, without limit from the menu, up to 3 times in 5 min from the terminal.
/// The mirroring app is always closed at the end: `keep` of the menu does not apply.
/// Ok(false) when the device has no mirroring receiver: the caller casts in HLS instead.
pub fn cast(device: &Device, s: &menu::Settings, screen: &ScreenCast, stop: &'static AtomicBool, out: &Out) -> anyhow::Result<bool> {
    let receiver = out.step(&format!("Starting the mirroring receiver on {}...", device.name), || {
        cast::launch(device, MIRRORING_RECEIVER, false, stop)
    })?;
    if receiver.app_id() != MIRRORING_RECEIVER {
        // launch() fell back to the Default Media Receiver.
        tracing::info!(app_id = receiver.app_id(), "no mirroring receiver");
        receiver.close();
        return Ok(false);
    }
    // Changes at each reconnection.
    let session_id = Arc::new(Mutex::new(receiver.session_id().to_string()));
    // receive() blocks until the next message (up to ~5 s): close the app from a second
    // connection so a stop frees the TV at once.
    let closer = {
        let device = device.clone();
        let session_id = Arc::clone(&session_id);
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(100));
            }
            let id = session_id.lock().unwrap().clone();
            if let Err(e) = cast::stop(&device, MIRRORING_RECEIVER, Some(&id)) {
                tracing::warn!("cannot close the mirroring receiver: {e:#}");
            }
        })
    };
    let result = reconnecting(receiver, device, s, screen, stop, out, &session_id);
    stop.store(true, Ordering::SeqCst);
    tracing::info!("mirroring ended: closing the receiver");
    let _ = closer.join();
    tracing::info!("mirroring receiver closed by sccast");
    result.map(|()| true)
}

/// Run sessions until one ends; after each loss, launch the mirroring receiver again.
fn reconnecting(
    mut receiver: cast::Receiver,
    device: &Device,
    s: &menu::Settings,
    screen: &ScreenCast,
    stop: &'static AtomicBool,
    out: &Out,
    session_id: &Mutex<String>,
) -> anyhow::Result<()> {
    let mut drops = cast::Drops::new(matches!(out, Out::Menu(_)));
    loop {
        // ponytail: leaks one flag per session (a byte), the threads want a 'static flag.
        let ended: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let outcome = stream(receiver, device, s, screen, stop, ended, out);
        ended.store(true, Ordering::SeqCst);
        let mut why = match outcome? {
            Outcome::Ended => return Ok(()),
            Outcome::Lost(why) => why,
        };
        loop {
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            tracing::warn!(%why, "mirroring lost");
            let count = drops.count(&device.name, &why)?;
            out.report(cast::Report::Reconnecting);
            out.say(&format!("Lost {} ({why}). Reconnecting ({count})...", device.name));
            std::thread::sleep(cast::RECONNECT_DELAY);
            match cast::launch(device, MIRRORING_RECEIVER, false, stop) {
                Ok(new) if new.app_id() == MIRRORING_RECEIVER => {
                    *session_id.lock().unwrap() = new.session_id().to_string();
                    receiver = new;
                    break;
                }
                Ok(other) => {
                    other.close();
                    why = "the mirroring receiver did not start".to_string();
                }
                Err(e) => why = format!("{e:#}"),
            }
        }
    }
}
