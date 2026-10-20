//! playback.rs
//! Play a stream on a receiver until the user stops, with reconnection when the TV drops
//! the session. Each session runs in media.rs; a stop from the user is handled here, by a
//! second connection, because the main one blocks while it waits for the TV.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rust_cast::channels::media::{Media, StreamType};

use super::media::{play_session, Outcome, Report};
use super::receiver::{launch, stop, stop_media, Receiver};

const MAX_RECONNECTS: u32 = 3;
// A drop this long after the previous one starts a new count.
const RECONNECT_WINDOW: Duration = Duration::from_secs(300);
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

impl Receiver {
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
        let watcher = watch_stop(&receiver, &app_id, &session, &playing, stop, keep);

        let media = Media {
            content_id: url.to_string(),
            stream_type: if live { StreamType::Live } else { StreamType::Buffered },
            content_type: content_type.to_string(),
            metadata: None,
            duration: None,
        };
        let device = receiver.device.clone();
        let limit = if receiver.forever { u32::MAX } else { MAX_RECONNECTS };
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
                if drops > limit {
                    anyhow::bail!(
                        "lost {} ({why}), gave up after {MAX_RECONNECTS} reconnections. \
                         If it no longer shows in `sccast list`, unplug it and plug it back in",
                        device.name
                    );
                }
                report(Report::Reconnecting);
                let count = if receiver.forever { drops.to_string() } else { format!("{drops}/{MAX_RECONNECTS}") };
                report(Report::Say(&format!("Lost {} ({why}). Reconnecting ({count})...", device.name)));
                std::thread::sleep(RECONNECT_DELAY);
                // A fresh launch: the session was lost, an attached app may be in the background.
                match launch(&device, &app_id, false, stop) {
                    Ok(mut new) => {
                        new.forever = receiver.forever;
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

/// Thread that waits for `stop`, then stops the media (`keep` set) or closes the app.
fn watch_stop(
    receiver: &Receiver,
    app_id: &str,
    session: &Arc<Mutex<String>>,
    playing: &Arc<Mutex<Option<(String, i32)>>>,
    stop_flag: &'static AtomicBool,
    keep: &'static AtomicBool,
) -> std::thread::JoinHandle<()> {
    let device = receiver.device.clone();
    let app_id = app_id.to_string();
    let session = Arc::clone(session);
    let playing = Arc::clone(playing);
    std::thread::spawn(move || {
        while !stop_flag.load(Ordering::SeqCst) {
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
        if let Err(e) = stop(&device, &app_id, Some(&session)) {
            tracing::warn!("cannot close the receiver app: {e:#}");
        }
    })
}
