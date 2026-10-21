//! code.rs
//! The code an interactive panel shows: Enter on a listed panel asks for it on the status line
//! (empty is allowed, for a panel that does not ask); `c` casts to a panel by its code alone.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use ratatui::crossterm::event::KeyCode;

use crate::device::discovery::Kind;
use crate::device::CastDevice;

/// Longer input is a wrong key, not a code.
const MAX_LEN: usize = 12;

#[derive(Debug, PartialEq)]
pub(super) enum Typed {
    Editing,
    Done,
    Cancelled,
}

/// Apply `key` to the code being typed.
pub(super) fn key(code: &mut String, key: KeyCode) -> Typed {
    match key {
        KeyCode::Char(c) if c.is_ascii_alphanumeric() && code.len() < MAX_LEN => code.push(c),
        KeyCode::Backspace => drop(code.pop()),
        KeyCode::Enter => return Typed::Done,
        KeyCode::Esc => return Typed::Cancelled,
        _ => {}
    }
    Typed::Editing
}

/// Status line while the code is typed, for a listed panel or (None) one found by its code.
pub(super) fn prompt(panel: Option<&CastDevice>, code: &str) -> String {
    match panel {
        Some(panel) => format!("Code shown on {}: {code}_   Enter: cast (empty if none). Esc: cancel", panel.name),
        None => format!("Code shown on the panel: {code}_   Enter: cast. Esc: cancel"),
    }
}

/// Panel at the address its code gives.
pub(super) fn panel(code: &str) -> Result<CastDevice, String> {
    let address = crate::screenshare::pin::decode(code)?;
    let name = format!("Panel {}", address.ip());
    Ok(CastDevice { id: address.to_string(), name, model: "Screenshare".into(), ip: *address.ip(), port: address.port(), kind: Kind::Screenshare })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_a_code() {
        let mut code = String::new();
        for c in "12a-4".chars() {
            assert_eq!(key(&mut code, KeyCode::Char(c)), Typed::Editing);
        }
        key(&mut code, KeyCode::Backspace);
        assert_eq!(code, "12a");
        assert_eq!(key(&mut code, KeyCode::Enter), Typed::Done);
        assert_eq!(key(&mut code, KeyCode::Esc), Typed::Cancelled);
    }

    #[test]
    fn finds_a_panel_by_its_code() {
        let found = panel("010753").unwrap();
        assert_eq!((found.ip.to_string(), found.port, found.kind), ("192.168.1.42".to_string(), 7385, Kind::Screenshare));
        assert!(panel("12").is_err());
    }
}
