//! menu/
//! Terminal menu (ratatui): pick a device found on the network, adjust the stream settings,
//! follow the running cast. This file holds the menu state; keys.rs runs the loop,
//! settings.rs, live.rs, draw.rs and panel.rs hold the rest.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

mod draw;
mod keys;
mod live;
mod panel;
mod settings;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::widgets::ListState;
use ratatui::DefaultTerminal;

use crate::device::CastDevice;
use live::Live;
pub use settings::Settings;
use settings::{change, AUDIO_DELAY_ROW};

enum Found {
    Device(CastDevice),
    Done,
}

/// What the user asked for.
#[derive(Debug)]
pub enum Action {
    /// Cast to this device, stopping the current cast first.
    Cast(CastDevice),
    Stop,
    /// Choose the screen again in the portal dialog.
    PickScreen,
    Quit,
}

/// Progress of the running cast, sent by its thread.
pub enum Status {
    Info(String),
    Error(String),
    Ended,
    /// Player state reported by the TV (Playing, Buffering...).
    Player(String),
    Reconnecting,
    /// Directory of the HLS segments, to measure the real bitrate.
    Segments(PathBuf),
    /// Bytes sent so far by the Matroska server, for the same measure.
    Served(ByteCounter),
    /// Frames dropped by the capture since the start.
    Dropped(Arc<AtomicU64>),
    /// Time to connect to the TV's Cast port, None if it did not answer.
    RoundTrip(Option<Duration>),
    /// The cast went down to this resolution (the TV refused the one set).
    Resolution((u32, u32)),
    /// The screen picker got a new screen: the running cast restarts on it.
    ScreenPicked,
}

pub type ByteCounter = Box<dyn Fn() -> u64 + Send>;

pub struct Menu {
    devices: BTreeMap<String, CastDevice>,
    device: ListState,
    row: ListState,
    on_settings: bool,
    scanning: bool,
    tick: usize,
    tx: Sender<Found>,
    rx: Receiver<Found>,
    status: Status,
    /// Id of the device being cast to.
    casting: Option<String>,
    status_tx: Sender<Status>,
    status_rx: Receiver<Status>,
    live: Live,
    /// The user's resolution while the menu shows the one a device took (Nest Hub: 1280x720).
    chosen_resolution: Option<(u32, u32)>,
    /// When to restart the running cast with a new audio delay: 1 s after the last key press.
    recast_at: Option<Instant>,
}

impl Menu {
    pub fn new() -> Self {
        let mut menu = Self::idle();
        menu.scan();
        menu
    }

    /// A menu that does not search the network yet.
    fn idle() -> Self {
        let (tx, rx) = mpsc::channel();
        let (status_tx, status_rx) = mpsc::channel();
        Self {
            devices: BTreeMap::new(),
            device: ListState::default().with_selected(Some(0)),
            row: ListState::default().with_selected(Some(0)),
            on_settings: false,
            scanning: false,
            tick: 0,
            tx,
            rx,
            status: Status::Info("Choose a device, then Enter".into()),
            casting: None,
            status_tx,
            status_rx,
            live: Live::default(),
            chosen_resolution: None,
            recast_at: None,
        }
    }

    /// Channel for a new cast thread to report its progress. Replaces the previous one, so a
    /// stopped cast's late messages (its "ended") are dropped instead of ending the new cast.
    pub fn status_sender(&mut self) -> Sender<Status> {
        (self.status_tx, self.status_rx) = mpsc::channel();
        self.status_tx.clone()
    }

    /// Current channel, for messages that are not from a cast (screen picker).
    pub fn current_sender(&self) -> Sender<Status> {
        self.status_tx.clone()
    }

    pub fn set_status(&mut self, status: Status) {
        match status {
            Status::Player(state) => {
                if state == "Buffering" && self.live.tv != "Buffering" {
                    self.live.buffering.push(Instant::now());
                }
                self.live.tv = state;
            }
            Status::Reconnecting => self.live.reconnects += 1,
            Status::Segments(dir) => self.live.segments = Some(dir),
            Status::Served(counter) => self.live.served = Some((counter, None)),
            Status::Dropped(counter) => self.live.dropped = Some(counter),
            Status::RoundTrip(time) => self.live.round_trip = Some(time),
            // Handled in next(), which holds the settings and returns actions.
            Status::Resolution(_) | Status::ScreenPicked => {}
            Status::Info(_) => self.status = status,
            Status::Error(_) | Status::Ended => {
                self.casting = None;
                self.recast_at = None;
                self.live.started = None;
                self.status = status;
            }
        }
    }

    /// Change the selected setting by hand. A new audio delay restarts the running cast.
    fn change_setting(&mut self, s: &mut Settings, dir: isize) {
        let row = self.row.selected().unwrap_or(0);
        self.chosen_resolution = None;
        change(s, row, dir);
        // Moving the offsets of a running pipeline froze the Sharp and doubled the CPU, in
        // mirror (2026-10-22) and in HLS: the delay is only set when a cast starts.
        if row == AUDIO_DELAY_ROW && self.casting.is_some() {
            self.recast_at = Some(Instant::now() + Duration::from_secs(1));
            self.status = Status::Info(format!("Audio delay {} ms: restarting the cast...", s.audio_delay));
        }
    }

    /// A device took another resolution: show it, keep the user's for the next cast.
    fn device_resolution(&mut self, s: &mut Settings, size: (u32, u32)) {
        self.chosen_resolution.get_or_insert(s.stream.resolution);
        s.stream.resolution = size;
    }

    /// Before a cast: back to the user's resolution, which the device may lower again.
    pub fn restore_resolution(&mut self, s: &mut Settings) {
        if let Some(size) = self.chosen_resolution.take() {
            s.stream.resolution = size;
        }
    }

    /// Mark the device being cast to; a new cast starts new stats.
    pub fn set_casting(&mut self, id: Option<String>) {
        if id.is_some() {
            self.live = Live::starting();
        }
        self.casting = id;
    }

    /// Redraw before a blocking step (portal dialog, stopping).
    pub fn draw_now(&mut self, terminal: &mut DefaultTerminal, s: &Settings) -> std::io::Result<()> {
        terminal.draw(|f| self.draw(f, s)).map(drop)
    }
}
