//! tray/
//! Icon in the desktop's notification area (StatusNotifierItem, through ksni): the devices
//! found, the presets and the running cast's controls. Its items send actions to
//! session/tray_loop.rs, which runs the casts; this file only shows what the loop gives it.
//! A device busy with another sender asks in a kdialog box, and errors show as KDE
//! notifications.
//!
//! Author: O. Booklage
//! Date: November 2026
//! Licence: MIT

use std::process::Command;
use std::sync::mpsc::Sender;

use ksni::menu::{CheckmarkItem, RadioGroup, RadioItem, StandardItem, SubMenu};
use ksni::{MenuItem, ToolTip};

use crate::device::CastDevice;

/// What the user clicked.
pub enum Action {
    /// A listed device: what it plays is checked first, unless sccast casts to it already.
    Choose(CastDevice),
    /// Cast to this device although it plays another sender's app.
    Replace(CastDevice),
    /// Search the network again.
    Rescan,
    /// Forget the devices that did not answer: asked first, then confirmed.
    Forget,
    Forgotten,
    Stop,
    Freeze,
    PickScreen,
    Preset(usize),
    TerminalMenu,
    Quit,
}

/// What the tray shows, built by the loop from the menu state.
#[derive(Clone, Default, PartialEq)]
pub struct View {
    /// Devices found, each with whether it answered since the last scan.
    pub devices: Vec<(CastDevice, bool)>,
    /// Id of the device being cast to.
    pub casting: Option<String>,
    pub frozen: bool,
    /// Index in menu::PRESETS; None for settings given by hand on the command line.
    pub preset: Option<usize>,
    pub status: String,
}

// Menu icons: PNG drawn from media/menu/*.svg (albert-go's line style), rendered at 32 px with
// `inkscape x.svg -w 32 -h 32 -o x.png`.
const ICON_DEVICE: &[u8] = include_bytes!("../../media/menu/device.png");
const ICON_STOP: &[u8] = include_bytes!("../../media/menu/stop.png");
const ICON_FREEZE: &[u8] = include_bytes!("../../media/menu/freeze.png");
const ICON_SCREEN: &[u8] = include_bytes!("../../media/menu/screen.png");
const ICON_PRESET: &[u8] = include_bytes!("../../media/menu/preset.png");
const ICON_TERMINAL: &[u8] = include_bytes!("../../media/menu/terminal.png");
const ICON_QUIT: &[u8] = include_bytes!("../../media/menu/quit.png");
const ICON_RESCAN: &[u8] = include_bytes!("../../media/menu/rescan.png");
const ICON_FORGET: &[u8] = include_bytes!("../../media/menu/forget.png");

pub struct Tray {
    pub view: View,
    actions: Sender<Action>,
}

impl Tray {
    pub fn new(actions: Sender<Action>) -> Self {
        Self { view: View::default(), actions }
    }

    fn item(&self, label: &str, icon: &[u8], action: fn() -> Action) -> MenuItem<Self> {
        StandardItem {
            label: label.into(),
            icon_data: icon.to_vec(),
            activate: Box::new(move |t: &mut Self| t.send(action())),
            ..Default::default()
        }
        .into()
    }

    fn send(&self, action: Action) {
        // The loop is gone only when sccast quits.
        let _ = self.actions.send(action);
    }
}

impl ksni::Tray for Tray {
    // A left click opens the menu too: the tray has no window to show.
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "sccast".into()
    }

    fn title(&self) -> String {
        "sccast".into()
    }

    // Pixels, not the icon name: a name the theme lacks (sccast not installed) shows nothing.
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![ksni::Icon { width: 64, height: 64, data: include_bytes!("../../media/icon-64.argb").to_vec() }]
    }

    fn overlay_icon_name(&self) -> String {
        match (&self.view.casting, self.view.frozen) {
            (None, _) => "",
            (Some(_), false) => "media-record",
            (Some(_), true) => "media-playback-pause",
        }
        .into()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip { title: "sccast".into(), description: self.view.status.clone(), ..Default::default() }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = self
            .view
            .devices
            .iter()
            .map(|(device, answered)| {
                let name = label(&device.name);
                let device = device.clone();
                CheckmarkItem {
                    // The menu shows these in dark gray: there before, silent now.
                    label: if *answered { name } else { format!("{name} (not answering)") },
                    checked: self.view.casting.as_deref() == Some(device.id.as_str()),
                    icon_data: ICON_DEVICE.to_vec(),
                    activate: Box::new(move |t: &mut Self| t.send(Action::Choose(device.clone()))),
                    ..Default::default()
                }
                .into()
            })
            .collect();
        if items.is_empty() {
            items.push(StandardItem { label: "Searching for devices...".into(), enabled: false, ..Default::default() }.into());
        }
        items.push(MenuItem::Separator);
        items.push(self.item("Search again", ICON_RESCAN, || Action::Rescan));
        if self.view.devices.iter().any(|(_, answered)| !answered) {
            items.push(self.item("Forget the devices not answering...", ICON_FORGET, || Action::Forget));
        }
        items.push(MenuItem::Separator);
        if self.view.casting.is_some() {
            items.push(self.item("Stop", ICON_STOP, || Action::Stop));
            items.push(
                CheckmarkItem {
                    label: "Freeze".into(),
                    checked: self.view.frozen,
                    icon_data: ICON_FREEZE.to_vec(),
                    activate: Box::new(|t: &mut Self| t.send(Action::Freeze)),
                    ..Default::default()
                }
                .into(),
            );
            items.push(self.item("Choose screen...", ICON_SCREEN, || Action::PickScreen));
            items.push(MenuItem::Separator);
        }
        let options = crate::menu::PRESETS.iter().map(|(name, ..)| RadioItem { label: label(name), ..Default::default() }).collect();
        let presets = RadioGroup {
            // Out of range: no preset ticked.
            selected: self.view.preset.unwrap_or(usize::MAX),
            select: Box::new(|t: &mut Self, i| t.send(Action::Preset(i))),
            options,
        };
        items.push(SubMenu { label: "Preset".into(), icon_data: ICON_PRESET.to_vec(), submenu: vec![presets.into()], ..Default::default() }.into());
        items.push(self.item("Terminal menu", ICON_TERMINAL, || Action::TerminalMenu));
        items.push(self.item("Quit", ICON_QUIT, || Action::Quit));
        items
    }
}

/// A menu label shows `text` as is: an underscore alone marks the access key.
fn label(text: &str) -> String {
    text.replace('_', "__")
}

/// Ask whether to replace the `app` that `device` plays. No when kdialog is missing.
pub fn replace(device: &CastDevice, app: &str) -> bool {
    let question = format!("{} is playing {app}. Replace it?", device.name);
    match Command::new("kdialog").args(["--title", "sccast", "--yesno", &question]).status() {
        Ok(status) => status.success(),
        Err(e) => {
            tracing::warn!("kdialog: {e}");
            false
        }
    }
}

/// Ask to confirm forgetting `count` devices. False when refused, or when kdialog is missing.
pub fn confirm_forget(count: usize) -> bool {
    let text = format!("Forget the {count} device(s) not answering?\nA device comes back once it answers again.");
    match Command::new("kdialog").args(["--title", "sccast", "--warningyesno", &text]).status() {
        Ok(status) => status.success(),
        Err(e) => {
            tracing::warn!("kdialog: {e}");
            false
        }
    }
}

/// Show `text` as a desktop notification for a few seconds, without waiting for it.
pub fn notify(text: &str) {
    let text = text.to_string();
    std::thread::spawn(move || {
        if let Err(e) = Command::new("kdialog").args(["--title", "sccast", "--icon", "sccast", "--passivepopup", &text, "8"]).status() {
            tracing::warn!("kdialog: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ksni::Tray as _;

    fn labels(tray: &Tray) -> Vec<String> {
        tray.menu()
            .into_iter()
            .filter_map(|item| match item {
                MenuItem::Standard(i) => Some(i.label),
                MenuItem::Checkmark(i) => Some(format!("{}{}", i.label, if i.checked { " [x]" } else { "" })),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn shows_devices_and_the_running_cast() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut tray = Tray::new(tx);
        assert_eq!(labels(&tray), ["Searching for devices...", "Search again", "Terminal menu", "Quit"]);
        let device = |id: &str, name: &str| CastDevice {
            id: id.into(),
            name: name.into(),
            model: "Chromecast".into(),
            ip: [10, 0, 0, 7].into(),
            port: 8009,
            busy: String::new(),
        };
        tray.view.devices = vec![(device("a", "Room_104"), true), (device("b", "Lab"), false)];
        tray.view.casting = Some("a".into());
        tray.view.frozen = true;
        assert_eq!(
            labels(&tray),
            [
                "Room__104 [x]",
                "Lab (not answering)",
                "Search again",
                "Forget the devices not answering...",
                "Stop",
                "Freeze [x]",
                "Choose screen...",
                "Terminal menu",
                "Quit"
            ]
        );
        // All answering: nothing to forget.
        tray.view.devices[1].1 = true;
        assert!(!labels(&tray).iter().any(|l| l.starts_with("Forget")));
    }
}
