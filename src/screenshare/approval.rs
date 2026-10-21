//! approval.rs
//! A person allows each sender on the panel (a dialog on the panel, sometimes a code): the
//! answers to the connect and cast requests can take a while, and a refusal ends the cast
//! instead of a reconnection loop. Result codes from the Android sender (-100 to -104) and
//! the Windows client's error table (-7 to -13); unverified on a panel.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::fmt;
use std::time::Duration;

use serde_json::Value;

/// Time left to someone at the panel to allow the cast.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(90);

/// The panel said no, or nobody answered. `retry`: the panel only needs time (it still holds
/// the session that was just lost, until its own 30 s timeout).
#[derive(Debug)]
pub struct Refused {
    pub why: String,
    pub retry: bool,
}

impl Refused {
    pub fn new(why: impl Into<String>) -> Self {
        Self { why: why.into(), retry: false }
    }
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.why)
    }
}

impl std::error::Error for Refused {}

/// Ok when `answer` (to the `what` request) accepts, else why not, in plain words.
pub fn check(what: &str, answer: &Value) -> Result<(), Refused> {
    if answer["resultType"] == 1 {
        return Ok(());
    }
    let reason = answer["failReason"].as_str().filter(|r| !r.is_empty());
    let code = answer["resultType"].as_i64();
    let why = match code {
        _ if answer["receiverType"] == -104 => "the panel wants a newer sender",
        Some(-7) => "the panel lost this session",
        Some(-8) | Some(-102) => "refused on the panel",
        Some(-9) => "the panel still holds the previous session",
        Some(-10) => "another sender took the panel",
        Some(-11) | Some(-101) => "too many devices on the panel",
        Some(-12) => "the session was disconnected",
        Some(-13) => "this sender casts too many windows",
        Some(-103) => "the panel is not ready to receive",
        Some(-100) => reason.unwrap_or("error on the panel"),
        _ => reason.unwrap_or("no reason given"),
    };
    let retry = matches!(code, Some(-9) | Some(-12));
    Err(Refused { why: format!("the panel refused the {what}: {why}"), retry })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn explains_refusals() {
        assert!(check("cast", &json!({"resultType": 1, "videoPort": 1})).is_ok());
        assert_eq!(check("cast", &json!({"resultType": -102})).unwrap_err().why, "the panel refused the cast: refused on the panel");
        let other = check("connection", &json!({"resultType": -77, "failReason": "busy"})).unwrap_err();
        assert_eq!((other.why.as_str(), other.retry), ("the panel refused the connection: busy", false));
        // The old session is still there: worth trying again; a person's no is not.
        assert!(check("connection", &json!({"resultType": -9})).unwrap_err().retry);
        assert!(!check("connection", &json!({"resultType": -8})).unwrap_err().retry);
    }
}
