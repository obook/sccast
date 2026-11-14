//! keys.rs
//! The menu loop: device search, messages from the cast, key presses. Returns what the
//! user asked for.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;

use super::settings::ROWS;
use super::{Action, Found, Menu, Settings, Status};
use crate::device::{discovery, CastDevice};

impl Menu {
    /// Browse in a thread while the menu is open; devices show up as they answer.
    /// A busy network loses answers: each round asks again and adds the late devices.
    pub fn scan(&mut self) {
        self.scanning = true;
        // Devices gone since the last scan (unplugged, new address) must not stay listed, except
        // the remembered ones: a busy network may hide devices that are there.
        self.devices = discovery::remembered().into_iter().map(|d| (d.id.clone(), d)).collect();
        self.answered.clear();
        self.round.clear();
        self.previous_round.clear();
        if self.browsing {
            // The round under way began before the clear: wait for the next full one.
            self.skip_done = true;
            return;
        }
        self.browsing = true;
        let tx = self.tx.clone();
        // ponytail: a new round every 12 s for the menu's lifetime; slow it down if a large network minds the traffic.
        std::thread::spawn(move || loop {
            // A school Wi-Fi drops most mDNS multicast but lets a ping through. The file is read
            // each round, for the devices first seen during the previous one.
            if discovery::ping_all(&discovery::remembered()).into_iter().any(|id| tx.send(Found::Alive(id)).is_err()) {
                break;
            }
            // Stop early once the menu is gone (send fails).
            let result = discovery::browse(Duration::from_secs(12), |d| tx.send(Found::Device(d)).is_err());
            if let Err(e) = result {
                tracing::warn!("discovery failed: {e:#}");
                std::thread::sleep(Duration::from_secs(12));
            }
            if tx.send(Found::Done).is_err() {
                break;
            }
        });
    }

    /// Read in a thread what `device` plays, so the menu goes on; Found::Checked brings the answer.
    // ponytail: get_status has no timeout; a device that accepts the connection but never answers
    // leaves "Checking..." until a key press cancels it.
    pub fn check(&mut self, device: CastDevice) {
        self.status = Status::Info(format!("Checking {}...", device.name));
        self.checking = Some(device.id.clone());
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            // A device that does not answer: cast anyway, the launch reports the error.
            let app = crate::device::running(&device).unwrap_or_else(|e| {
                tracing::warn!("cannot read the status of {}: {e:#}", device.name);
                None
            });
            let _ = tx.send(Found::Checked(device, app));
        });
    }

    /// Devices sorted by name, as listed.
    pub(super) fn list(&self) -> Vec<&CastDevice> {
        let mut list: Vec<_> = self.devices.values().collect();
        list.sort_by_key(|d| d.name.to_lowercase());
        list
    }

    /// Take in what the browse and cast threads sent. Returns the cast to start: a device
    /// found idle after its check, or the running cast again (new screen, new audio delay).
    /// A device found playing another sender's app comes back as Busy.
    pub fn poll(&mut self, s: &mut Settings) -> Option<Action> {
        while let Ok(found) = self.rx.try_recv() {
            match found {
                // Keyed by id: a device answers several times.
                Found::Device(d) => {
                    self.answered.insert(d.id.clone());
                    self.round.insert(d.id.clone());
                    discovery::seen(&d);
                    self.devices.insert(d.id.clone(), d);
                }
                Found::Alive(id) => {
                    if let Some(d) = self.devices.get(&id) {
                        discovery::seen(d);
                    }
                    self.answered.insert(id.clone());
                    self.round.insert(id);
                }
                // A device silent for two whole rounds (ping and browse) goes to "not answering".
                // One lost round is common on a school Wi-Fi: a cut of a few seconds must not gray the list.
                Found::Done => {
                    let round = std::mem::take(&mut self.round);
                    self.answered = round.union(&self.previous_round).cloned().collect();
                    self.previous_round = round;
                    if self.skip_done {
                        self.skip_done = false;
                    } else {
                        self.scanning = false;
                    }
                }
                Found::Checked(d, app) if self.checking.as_deref() == Some(d.id.as_str()) => {
                    self.checking = None;
                    return Some(match app {
                        Some(app) => Action::Busy(d, app),
                        None => Action::Cast(d),
                    });
                }
                // Cancelled by a key press meanwhile.
                Found::Checked(..) => {}
            }
        }
        while let Ok(status) = self.status_rx.try_recv() {
            if let Status::Stream(stream) = status {
                self.device_stream(s, stream);
                continue;
            }
            // A device gone from the list since (rescan): Enter recasts by hand.
            if let Status::ScreenPicked = status {
                if let Some(device) = self.casting.as_ref().and_then(|id| self.devices.get(id)) {
                    return Some(Action::Cast(device.clone()));
                }
                continue;
            }
            self.set_status(status);
        }
        if self.recast_at.is_some_and(|at| Instant::now() >= at) {
            self.recast_at = None;
            if let Some(device) = self.casting.as_ref().and_then(|id| self.devices.get(id)) {
                tracing::info!(ms = s.audio_delay, "audio delay changed: restarting the cast");
                return Some(Action::Cast(device.clone()));
            }
        }
        if self.live.started.is_some() {
            self.live.sample();
        }
        None
    }

    /// Draw and handle keys until the user asks for something. `quit` is set by SIGTERM.
    pub fn next(&mut self, terminal: &mut DefaultTerminal, s: &mut Settings, quit: &AtomicBool) -> anyhow::Result<Action> {
        loop {
            if quit.load(Ordering::SeqCst) {
                return Ok(Action::Quit);
            }
            match self.poll(s) {
                Some(Action::Busy(d, app)) => {
                    self.status = Status::Error(format!("{} is playing {app}. Enter: replace it, another key: cancel", d.name));
                    self.confirm = Some(d.id);
                }
                Some(action) => return Ok(action),
                None => {}
            }
            terminal.draw(|f| self.draw(f, s))?;
            self.tick += 1;
            // About once a minute: a frozen menu shows in the log as the end of these lines.
            if self.tick.is_multiple_of(600) {
                tracing::info!(casting = self.casting.is_some(), "menu loop alive");
            }
            if !event::poll(Duration::from_millis(100))? {
                continue;
            }
            let Event::Key(key) = event::read()? else { continue };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            tracing::info!(key = ?key.code, "key");
            let confirm = self.confirm.take();
            if self.checking.take().is_some() || confirm.is_some() {
                self.status = Status::Info("Cast cancelled".into());
            }
            let count = if self.on_settings { ROWS.len() } else { self.devices.len() };
            let state = if self.on_settings { &mut self.row } else { &mut self.device };
            match key.code {
                // Raw mode turns Ctrl+C into a key, not a signal.
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(Action::Quit),
                KeyCode::Char('q') | KeyCode::Esc => return Ok(Action::Quit),
                KeyCode::Char('s') if self.casting.is_some() => return Ok(Action::Stop),
                KeyCode::Char('p') => return Ok(Action::PickScreen),
                KeyCode::Char('f') if self.casting.is_some() => {
                    let frozen = !crate::capture::FREEZE.fetch_xor(true, Ordering::SeqCst);
                    let said = if frozen { "Frozen: the TV keeps this image (f: live again)" } else { "Live again" };
                    self.status = Status::Info(said.into());
                }
                KeyCode::Up | KeyCode::Char('k') if count > 0 => {
                    state.select(Some(state.selected().unwrap_or(0).checked_sub(1).unwrap_or(count - 1)));
                }
                KeyCode::Down | KeyCode::Char('j') if count > 0 => {
                    state.select(Some((state.selected().unwrap_or(0) + 1) % count));
                }
                // Long device lists (a school network): jump by pages or to the ends.
                KeyCode::PageDown if count > 0 => state.select(Some((state.selected().unwrap_or(0) + 10).min(count - 1))),
                KeyCode::PageUp => state.select(Some(state.selected().unwrap_or(0).saturating_sub(10))),
                KeyCode::Home => state.select(Some(0)),
                KeyCode::End if count > 0 => state.select(Some(count - 1)),
                KeyCode::Tab | KeyCode::BackTab => self.on_settings = !self.on_settings,
                // A change by hand becomes the user's choice.
                KeyCode::Left | KeyCode::Char('h') if self.on_settings => self.change_setting(s, -1),
                KeyCode::Right | KeyCode::Char('l') if self.on_settings => self.change_setting(s, 1),
                KeyCode::Char('r') => self.scan(),
                KeyCode::Enter => {
                    let list = self.list();
                    let Some(device) = self.device.selected().and_then(|i| list.get(i)).map(|d| (*d).clone()) else { continue };
                    // Our own cast restarts, a busy device confirmed: cast.
                    if self.casting.as_deref() == Some(device.id.as_str()) || confirm.as_deref() == Some(device.id.as_str()) {
                        return Ok(Action::Cast(device));
                    }
                    self.check(device);
                }
                _ => {}
            }
        }
    }
}
