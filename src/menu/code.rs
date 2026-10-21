//! code.rs
//! The code an interactive panel shows before it accepts a cast: Enter on a panel asks for
//! it on the status line. Empty is allowed, for a panel that does not ask.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use ratatui::crossterm::event::KeyCode;

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

/// Status line while the code is typed.
pub(super) fn prompt(panel: &str, code: &str) -> String {
    format!("Code shown on {panel}: {code}_   Enter: cast (empty if none). Esc: cancel")
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
}
