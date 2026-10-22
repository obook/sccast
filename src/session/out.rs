//! out.rs
//! Where cast progress goes: the terminal (messages on stderr, states on stdout) or the menu.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::io::Write;
use std::sync::mpsc::Sender;
use std::sync::Mutex;

use super::spin;
use crate::{device, menu};

/// Where cast progress goes: the terminal, with spinners, or the menu.
#[derive(Clone)]
pub enum Out {
    Term,
    Menu(Sender<menu::Status>),
}

impl Out {
    pub fn say(&self, msg: &str) {
        match self {
            Out::Term => eprintln!("{msg}"),
            Out::Menu(_) => self.send(menu::Status::Info(msg.to_string())),
        }
    }

    /// Menu only: the terminal shows messages, not stats.
    pub fn send(&self, status: menu::Status) {
        if let Out::Menu(tx) = self {
            let _ = tx.send(status);
        }
    }

    pub fn report(&self, r: device::Report) {
        match r {
            device::Report::Say(m) => self.say(m),
            device::Report::Player(state) => {
                let state = format!("{state:?}");
                match state.as_str() {
                    "Playing" => self.state("casting"),
                    "Buffering" => self.state("buffering"),
                    _ => {}
                }
                self.send(menu::Status::Player(state));
            }
            device::Report::Reconnecting => {
                self.state("reconnecting");
                self.send(menu::Status::Reconnecting);
            }
            device::Report::Loaded(..) => {}
        }
    }

    /// Terminal only: "state casting", "buffering", "reconnecting", "stopped" or "error" on
    /// stdout, for a program that runs sccast (an OpenBoard button). Repeats are dropped.
    pub fn state(&self, name: &'static str) {
        static LAST: Mutex<&str> = Mutex::new("");
        if let Out::Term = self {
            let mut last = LAST.lock().unwrap();
            if *last != name {
                *last = name;
                // Not println!: it panics when the reader is gone.
                let _ = writeln!(std::io::stdout(), "state {name}");
            }
        }
    }

    /// Run `f`, showing `msg` while it runs.
    pub fn step<T>(&self, msg: &str, f: impl FnOnce() -> T) -> T {
        match self {
            Out::Term => spin(msg, f),
            Out::Menu(_) => {
                self.say(msg);
                f()
            }
        }
    }
}
