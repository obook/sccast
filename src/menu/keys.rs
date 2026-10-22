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
    /// Browse in a thread; devices show up as they answer.
    pub(super) fn scan(&mut self) {
        if self.scanning {
            return;
        }
        self.scanning = true;
        // Devices gone since the last scan (unplugged, new address) must not stay listed.
        self.devices.clear();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            // Stop early once the menu is gone (send fails).
            let result = discovery::browse(Duration::from_secs(8), |d| tx.send(Found::Device(d)).is_err());
            if let Err(e) = result {
                tracing::warn!("discovery failed: {e:#}");
            }
            let _ = tx.send(Found::Done);
        });
    }

    /// Devices sorted by name, as listed.
    pub(super) fn list(&self) -> Vec<&CastDevice> {
        let mut list: Vec<_> = self.devices.values().collect();
        list.sort_by_key(|d| d.name.to_lowercase());
        list
    }

    /// Draw and handle keys until the user asks for something. `quit` is set by SIGTERM.
    pub fn next(&mut self, terminal: &mut DefaultTerminal, s: &mut Settings, quit: &AtomicBool) -> anyhow::Result<Action> {
        loop {
            if quit.load(Ordering::SeqCst) {
                return Ok(Action::Quit);
            }
            while let Ok(found) = self.rx.try_recv() {
                match found {
                    // Keyed by id: a device answers several times.
                    Found::Device(d) => drop(self.devices.insert(d.id.clone(), d)),
                    Found::Done => self.scanning = false,
                }
            }
            while let Ok(status) = self.status_rx.try_recv() {
                if let Status::Resolution(size) = status {
                    self.device_resolution(s, size);
                    continue;
                }
                // A device gone from the list since (rescan): Enter recasts by hand.
                if let Status::ScreenPicked = status {
                    if let Some(device) = self.casting.as_ref().and_then(|id| self.devices.get(id)) {
                        return Ok(Action::Cast(device.clone()));
                    }
                    continue;
                }
                self.set_status(status);
            }
            if self.recast_at.is_some_and(|at| Instant::now() >= at) {
                self.recast_at = None;
                if let Some(device) = self.casting.as_ref().and_then(|id| self.devices.get(id)) {
                    tracing::info!(ms = s.audio_delay, "audio delay changed: restarting the cast");
                    return Ok(Action::Cast(device.clone()));
                }
            }
            if self.live.started.is_some() {
                self.live.sample();
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
            let count = if self.on_settings { ROWS.len() } else { self.devices.len() };
            let state = if self.on_settings { &mut self.row } else { &mut self.device };
            match key.code {
                // Raw mode turns Ctrl+C into a key, not a signal.
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(Action::Quit),
                KeyCode::Char('q') | KeyCode::Esc => return Ok(Action::Quit),
                KeyCode::Char('s') if self.casting.is_some() => return Ok(Action::Stop),
                KeyCode::Char('p') => return Ok(Action::PickScreen),
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
                    return Ok(Action::Cast(device));
                }
                _ => {}
            }
        }
    }
}
