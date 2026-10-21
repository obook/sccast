//! rate.rs
//! Video bitrate and playout delay of the mirror format, following the receiver's reports.
//! send.rs feeds them the loss share of each Receiver Report and the key frame requests,
//! then sets the encoder's bitrate and the delay written in the packets.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::time::{Duration, Instant};

// First guesses: down 20 % above 2 % of packets lost, up 10 % after 10 s without loss.
/// Loss share that triggers a cut, out of 256.
pub const LOSS_LIMIT: u8 = 5;
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

/// Playout delays in ms, as CastMirror's steps: a longer delay leaves time for resent
/// packets on a busy Wi-Fi, a shorter one keeps the TV close to the screen.
const DELAYS: [u16; 4] = [super::TARGET_DELAY_MS as u16, 200, 300, 400];
/// One step up at most this often: a burst of reports must not climb every step at once.
const RAISE_SPACING: Duration = Duration::from_secs(2);
/// One step down after this long without trouble.
const CALM_PERIOD: Duration = Duration::from_secs(30);

/// Playout delay of the receiver, raised on trouble, lowered once the network is calm.
pub struct Delay {
    step: usize,
    calm_since: Instant,
    last_raise: Option<Instant>,
}

impl Delay {
    pub fn new(now: Instant) -> Self {
        Self { step: 0, calm_since: now, last_raise: None }
    }

    /// New delay to send, if it changes. `trouble`: packets lost above LOSS_LIMIT, or a key
    /// frame request (a frame missed its playout time).
    pub fn adapt(&mut self, trouble: bool, now: Instant) -> Option<u16> {
        if trouble {
            self.calm_since = now;
            let spaced = self.last_raise.is_none_or(|at| now.duration_since(at) >= RAISE_SPACING);
            if self.step + 1 == DELAYS.len() || !spaced {
                return None;
            }
            self.step += 1;
            self.last_raise = Some(now);
        } else if self.step > 0 && now.duration_since(self.calm_since) >= CALM_PERIOD {
            self.step -= 1;
            self.calm_since = now;
        } else {
            return None;
        }
        Some(DELAYS[self.step])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapts_delay() {
        let start = Instant::now();
        let mut delay = Delay::new(start);
        assert_eq!(delay.adapt(false, start + CALM_PERIOD), None, "already at the lowest");
        assert_eq!(delay.adapt(true, start), Some(200));
        assert_eq!(delay.adapt(true, start + Duration::from_secs(1)), None, "one raise per 2 s");
        assert_eq!(delay.adapt(true, start + RAISE_SPACING), Some(300));
        assert_eq!(delay.adapt(true, start + RAISE_SPACING * 2), Some(400));
        assert_eq!(delay.adapt(true, start + RAISE_SPACING * 3), None, "already at the highest");
        let calm = start + RAISE_SPACING * 3;
        assert_eq!(delay.adapt(false, calm + Duration::from_secs(29)), None);
        assert_eq!(delay.adapt(false, calm + CALM_PERIOD), Some(300));
        assert_eq!(delay.adapt(false, calm + CALM_PERIOD * 2), Some(200));
    }

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
