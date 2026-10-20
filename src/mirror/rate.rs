//! rate.rs
//! Video bitrate of the mirror format, following the receiver's loss reports.
//! send.rs feeds it the loss share of each Receiver Report and sets the encoder's bitrate.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::time::{Duration, Instant};

// First guesses: down 20 % above 2 % of packets lost, up 10 % after 10 s without loss.
/// Loss share that triggers a cut, out of 256.
const LOSS_LIMIT: u8 = 5;
const MIN_BITRATE: u32 = 500;
const CLEAN_PERIOD: Duration = Duration::from_secs(10);
/// The receiver sends up to 10 reports a second: one cut per second, or a short loss burst
/// would drop the video to the floor for minutes.
const CUT_SPACING: Duration = Duration::from_secs(1);

/// Video bitrate in kbit/s, never above the user's setting.
pub struct Rate {
    max: u32,
    pub current: u32,
    clean_since: Instant,
    last_cut: Option<Instant>,
}

impl Rate {
    pub fn new(max: u32) -> Self {
        Self { max, current: max, clean_since: Instant::now(), last_cut: None }
    }

    /// New bitrate to set, if it changes.
    pub fn adapt(&mut self, fraction_lost: u8, now: Instant) -> Option<u32> {
        let next = if fraction_lost > LOSS_LIMIT {
            self.clean_since = now;
            if self.last_cut.is_some_and(|at| now.duration_since(at) < CUT_SPACING) {
                return None;
            }
            self.last_cut = Some(now);
            (self.current * 4 / 5).max(MIN_BITRATE).min(self.max)
        } else if now.duration_since(self.clean_since) >= CLEAN_PERIOD {
            self.clean_since = now;
            (self.current * 11 / 10).min(self.max)
        } else {
            self.current
        };
        if next == self.current {
            return None;
        }
        self.current = next;
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapts_bitrate() {
        let start = Instant::now();
        let mut rate = Rate { max: 2500, current: 2500, clean_since: start, last_cut: None };
        assert_eq!(rate.adapt(0, start), None);
        assert_eq!(rate.adapt(20, start), Some(2000));
        assert_eq!(rate.adapt(20, start + Duration::from_millis(100)), None, "one cut per second");

        let clean = start + Duration::from_millis(100);
        assert_eq!(rate.adapt(0, clean + Duration::from_secs(5)), None);
        assert_eq!(rate.adapt(0, clean + CLEAN_PERIOD), Some(2200));
        assert_eq!(rate.adapt(0, clean + CLEAN_PERIOD * 3), Some(2420));
        assert_eq!(rate.adapt(0, clean + CLEAN_PERIOD * 4), Some(2500));

        let mut low = Rate { max: 300, current: 300, clean_since: start, last_cut: None };
        assert_eq!(low.adapt(20, start), None, "never above the user's setting");
    }
}
