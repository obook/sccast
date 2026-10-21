//! trace.rs
//! Protocol trace of the panel casts, always written, to read after a test on site: every
//! command sent and received, the first video frame headers, each connection attempt and each
//! loss. In ~/.local/state/sccast/screenshare.log (kept after logout, unlike the menu's log),
//! started over at each run of sccast.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Longer payloads are cut: the trace must stay readable.
const MAX_PAYLOAD: usize = 500;

static FILE: OnceLock<Option<Mutex<File>>> = OnceLock::new();

/// Where the trace goes.
pub fn path() -> PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).or_else(|| {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
    });
    state.unwrap_or_else(std::env::temp_dir).join("sccast/screenshare.log")
}

/// Add a line, with the UTC time of day in ms. A trace that cannot be written is skipped.
pub fn line(text: &str) {
    let file = FILE.get_or_init(|| {
        let path = path();
        let _ = std::fs::create_dir_all(path.parent()?);
        File::create(&path).ok().map(Mutex::new)
    });
    let Some(file) = file else { return };
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis()) % 86_400_000;
    let (h, m, s, ms) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    let _ = writeln!(file.lock().unwrap(), "{h:02}:{m:02}:{s:02}.{ms:03} UTC {text}");
}

/// One command: `arrow` is "->" sent or "<-" received.
pub fn command(arrow: &str, command_type: i32, command_id: i32, sequence: i32, payload: &str) {
    let cut = payload.char_indices().nth(MAX_PAYLOAD).map_or(payload, |(at, _)| &payload[..at]);
    let more = if cut.len() < payload.len() { "..." } else { "" };
    line(&format!("{arrow} type {command_type} id {command_id} seq {sequence} {cut}{more}"));
}
