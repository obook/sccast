//! tray_loop.rs
//! Run the tray: take its clicks and the menu state (devices found, the running cast's
//! progress) and show them on the icon. The casts run as in the menu (menu_loop.rs), the
//! receiver kept open the same way. The screen is shared only while a cast runs, so the
//! desktop's sharing indicator goes off between casts; the screen dialog then comes back
//! at each cast.
//!
//! Author: O. Booklage
//! Date: November 2026
//! Licence: MIT

use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use anyhow::Context;
use ksni::blocking::TrayMethods;

use super::menu_loop::{pick_screen, Casts};
use super::{stop_flag, Out};
use crate::device::CastDevice;
use crate::menu;
use crate::tray::{self, Action};

/// Run the tray until Quit, or the end of the session.
pub fn run_tray(mut settings: menu::Settings) -> anyhow::Result<()> {
    let quit = stop_flag()?;
    // Started again from the application menu, a second tray would show a second icon.
    let lock = std::fs::File::create(crate::runtime_file("sccast-tray.lock"))?;
    if lock.try_lock().is_err() {
        eprintln!("sccast tray is already running.");
        return Ok(());
    }
    if let Ok(file) = std::fs::File::create(crate::runtime_file("sccast-tray.log")) {
        let _ = crate::MENU_LOG.set(file);
    }
    let (tx, actions) = mpsc::channel();
    // Started with the session, the tray may come before the desktop's panel: it waits for it.
    let icon = tray::Tray::new(tx.clone()).assume_sni_available(true).spawn().context("no notification area")?;
    let mut menu = menu::Menu::new();
    let mut casts = Casts::new(None, quit);
    let mut shown = None;
    // Last error notified: each shows once.
    let mut told = String::new();
    'run: while !quit.load(Ordering::SeqCst) {
        let mut act = match menu.poll(&mut settings) {
            Some(menu::Action::Busy(device, app)) => {
                ask_replace(device, app, &tx);
                None
            }
            // A device found idle, or the running cast again on a new screen.
            action => action,
        };
        while let Ok(action) = actions.try_recv() {
            match action {
                // Our own cast restarts at once; another device may play another sender's app.
                Action::Choose(device) if menu.casting() == Some(device.id.as_str()) => act = Some(menu::Action::Cast(device)),
                Action::Choose(device) => menu.check(device),
                Action::Replace(device) => act = Some(menu::Action::Cast(device)),
                Action::Rescan => menu.scan(),
                Action::Forget => {
                    let (count, tx) = (menu.devices().iter().filter(|(_, answered)| !answered).count(), tx.clone());
                    // In a thread: the icon keeps answering while the box is open.
                    std::thread::spawn(move || {
                        if tray::confirm_forget(count) {
                            let _ = tx.send(Action::Forgotten);
                        }
                    });
                }
                Action::Forgotten => menu.forget_silent(),
                Action::Stop => act = Some(menu::Action::Stop),
                Action::Freeze => toggle_freeze(),
                Action::PickScreen => pick_screen(Out::Menu(menu.current_sender()), &casts.screen),
                Action::Preset(i) => menu.set_preset(&mut settings, i),
                Action::TerminalMenu => terminal_menu(),
                Action::Quit => break 'run,
            }
        }
        if let Some(action) = act {
            tracing::info!(?action, "tray action");
            told.clear();
            casts.act(&action, &mut menu, &mut settings, &mut |_, _| {});
        }
        casts.release_screen();
        if let Some((error, true)) = menu.status_line().filter(|(text, _)| *text != told) {
            told = error.to_string();
            tray::notify(error);
        }
        let view = view(&menu, &settings);
        if shown.as_ref() != Some(&view) {
            if icon.update(|t| t.view = view.clone()).is_none() {
                break;
            }
            shown = Some(view);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // Frees the TV: stops the cast and closes the receiver left open.
    casts.act(&menu::Action::Quit, &mut menu, &mut settings, &mut |_, _| {});
    icon.shutdown().wait();
    Ok(())
}

/// What the icon shows of the menu state.
fn view(menu: &menu::Menu, s: &menu::Settings) -> tray::View {
    let status = match (menu.casting(), menu.status_line()) {
        (_, Some((text, true))) | (Some(_), Some((text, false))) => text.to_string(),
        // Not the menu's "Choose a device, then Enter".
        _ => "Not casting".to_string(),
    };
    tray::View {
        devices: menu.devices(),
        casting: menu.casting().map(str::to_string),
        frozen: crate::capture::FREEZE.load(Ordering::SeqCst),
        preset: menu::preset(s),
        status,
    }
}

/// Ask in a dialog, in a thread, whether to replace what `device` plays: the icon keeps
/// answering meanwhile.
fn ask_replace(device: CastDevice, app: String, tx: &Sender<Action>) {
    let tx = tx.clone();
    std::thread::spawn(move || {
        if tray::replace(&device, &app) {
            let _ = tx.send(Action::Replace(device));
        }
    });
}

/// Go live, or freeze once the tray menu has closed: the frozen image must not show it.
fn toggle_freeze() {
    // Time for the menu to fade out, with a margin.
    const MENU_CLOSED: Duration = Duration::from_millis(500);
    // shortcut: a cast started within this delay starts frozen, check a cast counter if that matters.
    if !crate::capture::FREEZE.swap(false, Ordering::SeqCst) {
        std::thread::spawn(|| {
            std::thread::sleep(MENU_CLOSED);
            crate::capture::FREEZE.store(true, Ordering::SeqCst);
        });
    }
}

/// Open the terminal menu in Konsole.
// shortcut: Konsole only (KDE), switch to xdg-terminal-exec once the distributions ship it.
fn terminal_menu() {
    // After an upgrade, the running file is gone: the one installed then.
    let sccast = std::env::current_exe().ok().filter(|exe| exe.exists()).unwrap_or_else(|| "sccast".into());
    match Command::new("konsole").arg("-e").arg(sccast).spawn() {
        Ok(mut konsole) => drop(std::thread::spawn(move || konsole.wait())),
        Err(e) => tray::notify(&format!("Konsole did not start: {e}")),
    }
}
