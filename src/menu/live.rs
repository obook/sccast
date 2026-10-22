//! live.rs
//! Measures of the running cast, refreshed once a second: bitrate, lost frames, CPU,
//! network times. Turns them into the alert shown under the status line.
//! panel.rs draws them.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::style::Color;

use super::ByteCounter;

// Alert thresholds. ponytail: first guesses, to adjust with tests (the school network in particular).
/// One second of video lost per minute.
pub(super) const DROPPED_PER_MINUTE: u64 = 30;
/// One second of video must reach the TV well within one second.
pub(super) const SLOW_FETCH: Duration = Duration::from_millis(500);
const CONGESTED_FETCH: Duration = Duration::from_millis(900);
pub(super) const SLOW_ROUND_TRIP: Duration = Duration::from_millis(100);
/// More buffering periods than this in a minute: the TV does not get the stream in time.
const BUFFERING_PER_MINUTE: usize = 2;

/// Stats of the current cast, shown in the Live panel.
#[derive(Default)]
pub(super) struct Live {
    pub(super) started: Option<Instant>,
    pub(super) tv: String,
    /// Start of each buffering period.
    pub(super) buffering: Vec<Instant>,
    pub(super) reconnects: u32,
    pub(super) segments: Option<PathBuf>,
    /// Matroska byte counter, and its last reading.
    pub(super) served: Option<(ByteCounter, Option<(u64, Instant)>)>,
    /// kbit/s, one sample per second, last minute.
    pub(super) bitrate: Vec<u64>,
    /// sccast's CPU time in clock ticks, and when it was read.
    cpu_ticks: Option<(u64, Instant)>,
    /// Percent of one core.
    pub(super) cpu: f64,
    sampled: Option<Instant>,
    pub(super) dropped: Option<Arc<AtomicU64>>,
    /// Dropped frames total, one sample per second, last minute.
    pub(super) dropped_history: VecDeque<u64>,
    /// Average download time of the last segments by the TV.
    pub(super) fetch: Option<Duration>,
    /// None until measured; Some(None) when the TV did not answer.
    pub(super) round_trip: Option<Option<Duration>>,
    /// Seconds sampled, to log a summary every 10.
    samples: u64,
}

impl Live {
    /// Fresh stats for a cast starting now.
    pub(super) fn starting() -> Self {
        Self { started: Some(Instant::now()), ..Self::default() }
    }

    /// Refresh the measures once a second.
    pub(super) fn sample(&mut self) {
        if self.sampled.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return;
        }
        self.sampled = Some(Instant::now());
        if let Some(dir) = &self.segments {
            // Segments written in the last 5 s: about 5 one-second segments.
            let bytes: u64 = std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.modified().ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age.as_secs() < 5))
                .map(|m| m.len())
                .sum();
            self.bitrate.push(bytes * 8 / 5 / 1000);
        }
        if let Some((counter, last)) = &mut self.served {
            let bytes = counter();
            if let Some((before, at)) = *last {
                self.bitrate.push((bytes.saturating_sub(before) as f64 * 8.0 / 1000.0 / at.elapsed().as_secs_f64()) as u64);
            }
            *last = Some((bytes, Instant::now()));
        }
        if self.bitrate.len() > 60 {
            self.bitrate.remove(0);
        }
        if let Some(dropped) = &self.dropped {
            self.dropped_history.push_back(dropped.load(Ordering::Relaxed));
            if self.dropped_history.len() > 61 {
                self.dropped_history.pop_front();
            }
        }
        self.fetch = crate::hls::segment_fetch_time();
        self.samples += 1;
        if self.samples.is_multiple_of(10) {
            tracing::info!(
                lost_last_minute = self.dropped_last_minute(),
                fetch_ms = self.fetch.map(|f| f.as_millis() as u64),
                ping_ms = ?self.round_trip.map(|r| r.map(|r| r.as_millis() as u64)),
                cpu = self.cpu as u64,
                "live"
            );
        }
        if let Some(ticks) = cpu_ticks() {
            if let Some((before, at)) = self.cpu_ticks {
                // ponytail: assumes 100 ticks per second (the Linux default), read sysconf if it ever differs.
                self.cpu = (ticks - before) as f64 / at.elapsed().as_secs_f64();
            }
            self.cpu_ticks = Some((ticks, Instant::now()));
        }
    }

    pub(super) fn dropped_last_minute(&self) -> u64 {
        match (self.dropped_history.front(), self.dropped_history.back()) {
            (Some(first), Some(last)) => last - first,
            _ => 0,
        }
    }

    pub(super) fn buffering_last_minute(&self) -> usize {
        self.buffering.iter().filter(|t| t.elapsed() < Duration::from_secs(60)).count()
    }

    /// The worst problem seen, with its color, or None when all is well.
    pub(super) fn alert(&self) -> Option<(String, Color)> {
        let dropped = self.dropped_last_minute();
        if self.round_trip == Some(None) {
            return Some(("The TV does not answer on the network".into(), Color::Red));
        }
        if dropped > DROPPED_PER_MINUTE {
            return Some((format!("PC too slow: {dropped} frames lost in a minute. Lower the resolution or close programs"), Color::Red));
        }
        let buffering = self.buffering_last_minute();
        if buffering > BUFFERING_PER_MINUTE {
            return Some((format!("The TV buffered {buffering} times in a minute. Lower the bitrate"), Color::Red));
        }
        if let Some(fetch) = self.fetch.filter(|f| *f > SLOW_FETCH) {
            let color = if fetch > CONGESTED_FETCH { Color::Red } else { Color::Yellow };
            return Some((format!("Network slow: 1 s of video takes {:.1} s to reach the TV. Lower the bitrate", fetch.as_secs_f64()), color));
        }
        if let Some(Some(rtt)) = self.round_trip.filter(|r| r.is_some_and(|r| r > SLOW_ROUND_TRIP)) {
            return Some((format!("Network slow: {} ms to reach the TV", rtt.as_millis()), Color::Yellow));
        }
        None
    }
}

/// utime + stime of this process, from /proc/self/stat (fields 14 and 15).
fn cpu_ticks() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // The command name can hold spaces: count fields after its closing parenthesis.
    let mut fields = stat.rsplit_once(')')?.1.split_whitespace().skip(11);
    Some(fields.next()?.parse::<u64>().ok()? + fields.next()?.parse::<u64>().ok()?)
}

/// System load over the last minute, out of the number of CPUs (e.g. "1.20/8").
pub(super) fn load_average() -> Option<String> {
    let load = std::fs::read_to_string("/proc/loadavg").ok()?;
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    Some(format!("{}/{cpus}", load.split_whitespace().next()?))
}
