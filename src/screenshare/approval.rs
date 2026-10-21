//! approval.rs
//! A person allows each sender on the panel (a dialog on the panel, sometimes a code): the
//! answers to the connect and cast requests can take a while, and a refusal ends the cast
//! instead of a reconnection loop. Result codes from the Android sender; unverified on a panel.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::fmt;
use std::time::Duration;

use serde_json::Value;

/// Time left to someone at the panel to allow the cast.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(90);

/// The panel said no, or nobody answered: connecting again would not help.
#[derive(Debug)]
pub struct Refused(pub String);

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

/// Ok when `answer` (to the `what` request) accepts, else why not, in plain words.
/// Codes as the Android sender reads them (its action names; the panel's texts are not decoded).
pub fn check(what: &str, answer: &Value) -> Result<(), Refused> {
    if answer["resultType"] == 1 {
        return Ok(());
    }
    let reason = answer["failReason"].as_str().filter(|r| !r.is_empty());
    let why = match answer["resultType"].as_i64() {
        _ if answer["receiverType"] == -104 => "the panel wants a newer sender",
        Some(-100) => reason.unwrap_or("error on the panel"),
        Some(-101) => "too many devices on the panel",
        Some(-102) => "refused on the panel",
        Some(-103) => "the panel is not ready to receive",
        _ => reason.unwrap_or("no reason given"),
    };
    Err(Refused(format!("the panel refused the {what}: {why}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn explains_refusals() {
        assert!(check("cast", &json!({"resultType": 1, "videoPort": 1})).is_ok());
        assert_eq!(check("cast", &json!({"resultType": -102})).unwrap_err().0, "the panel refused the cast: refused on the panel");
        let other = check("connection", &json!({"resultType": -7, "failReason": "busy"})).unwrap_err().0;
        assert_eq!(other, "the panel refused the connection: busy");
    }
}
