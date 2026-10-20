//! menu_loop.rs
//! Run the menu: each Enter starts a cast in a thread while the menu stays on screen;
//! s stops it, q quits, p picks the screen again. The receiver stays open on the TV
//! between two casts to the same TV.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::{cast_screen, stop_flag, Out};
use crate::capture::ScreenCast;
use crate::device::{self, CastDevice};
use crate::{cli, menu};

/// Run the menu until the user quits.
pub fn run_menu(mut settings: menu::Settings) -> anyhow::Result<()> {
    // Ctrl+C is a key in the menu; the flag still catches SIGTERM.
    let quit = stop_flag()?;
    // The screen first, before the menu takes the terminal.
    let screen = ScreenCast::open()?;
    // A fresh log for each run.
    if let Ok(file) = std::fs::File::create(crate::log_path()) {
        let _ = crate::MENU_LOG.set(file);
    }
    let mut terminal = ratatui::init();
    let result = drive_menu(&mut terminal, &mut settings, screen, quit);
    // Not restore(): its eprintln! panics when the terminal was closed.
    let _ = ratatui::try_restore();
    result
}

fn drive_menu(
    terminal: &mut ratatui::DefaultTerminal,
    s: &mut menu::Settings,
    screen: ScreenCast,
    quit: &'static AtomicBool,
) -> anyhow::Result<()> {
    let mut menu = menu::Menu::new();
    // Chosen before the menu, kept for every cast; p replaces it.
    let screen = Arc::new(Mutex::new(Arc::new(screen)));
    // Stop and keep flags of the running cast, and its thread.
    let mut running: Option<(&'static AtomicBool, &'static AtomicBool, JoinHandle<()>)> = None;
    // TV where the receiver may be open, and its app id.
    let mut tv: Option<(CastDevice, String)> = None;
    // Error of a terminal closed without quitting: returned once the TV is freed.
    let mut lost = None;
    loop {
        let action = match menu.next(terminal, s, quit) {
            Ok(action) => action,
            Err(e) => {
                tracing::warn!("terminal lost: {e:#}");
                lost = Some(e);
                menu::Action::Quit
            }
        };
        if let menu::Action::PickScreen = action {
            pick_screen(&menu, &screen);
            continue;
        }
        // The receiver stays open on the TV between casts to the same TV, and after s:
        // the Sharp TV sometimes never answered a launch right after a close.
        let same_tv = matches!((&action, &tv), (menu::Action::Cast(d), Some((open, _))) if d.id == open.id);
        let keep_open = same_tv || matches!(action, menu::Action::Stop);
        if let Some((stop, keep, cast)) = running.take() {
            menu.set_status(menu::Status::Info("Stopping...".into()));
            // The terminal may be closed: freeing the TV comes first.
            let _ = menu.draw_now(terminal, s);
            keep.store(keep_open, Ordering::SeqCst);
            stop.store(true, Ordering::SeqCst);
            wait_for(&cast);
            menu.set_status(menu::Status::Ended);
        } else if !keep_open {
            // A receiver left open by s, on this TV or another one.
            if let Some((device, app_id)) = tv.take() {
                menu.set_status(menu::Status::Info(format!("Closing the receiver on {}...", device.name)));
                let _ = menu.draw_now(terminal, s);
                wait_for(&std::thread::spawn(move || drop(device::stop(&device, &app_id, None))));
            }
        }
        if !keep_open {
            tv = None;
        }
        let device = match action {
            menu::Action::Quit => return lost.map_or(Ok(()), Err),
            menu::Action::Stop | menu::Action::PickScreen => continue,
            menu::Action::Cast(device) => device,
        };
        // ponytail: two leaked flags per cast (a few bytes), play() wants 'static flags.
        let stop: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let keep: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let out = Out::Menu(menu.status_sender());
        let settings = s.clone();
        let screen = Arc::clone(&screen.lock().unwrap());
        menu.set_casting(Some(device.id.clone()));
        // The mirror format closes its own app: nothing left open to track.
        tv = (settings.stream.format != cli::Format::Mirror).then(|| (device.clone(), settings.receiver.clone()));
        let cast = std::thread::spawn(move || match cast_screen(&device, &settings, &screen, stop, Some(keep), &out) {
            Ok(()) => out.send(menu::Status::Ended),
            // The sharing may have been stopped from the desktop.
            Err(e) if format!("{e:#}").contains("capture") => {
                out.send(menu::Status::Error(format!("{e:#} (p: choose the screen again)")))
            }
            Err(e) => out.send(menu::Status::Error(format!("{e:#}"))),
        });
        running = Some((stop, keep, cast));
    }
}

/// Open the portal dialog in a thread: the menu keeps answering, the cast goes on meanwhile.
fn pick_screen(menu: &menu::Menu, screen: &Arc<Mutex<Arc<ScreenCast>>>) {
    let out = Out::Menu(menu.current_sender());
    let slot = Arc::clone(screen);
    std::thread::spawn(move || {
        out.say("Choose the screen in the dialog...");
        match ScreenCast::open() {
            Ok(opened) => {
                *slot.lock().unwrap() = Arc::new(opened);
                out.say("Screen changed: Enter casts it.");
            }
            // Not an Error status: that would end the stats of the running cast.
            Err(e) => out.say(&format!("Screen not changed: {e:#}")),
        }
    });
}

/// Wait for `thread`, at most 15 s: the TV can stop answering. Freeing it takes several round
/// trips (170 to 500 ms each on the school network), after a receive() that blocks up to 5 s.
fn wait_for<T>(thread: &JoinHandle<T>) {
    let started = Instant::now();
    while !thread.is_finished() && started.elapsed() < Duration::from_secs(15) {
        std::thread::sleep(Duration::from_millis(50));
    }
}
