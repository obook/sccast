//! settings.rs
//! Stream settings the menu edits: the values each row cycles through, the presets, and
//! the text shown for each row.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use crate::cli::{Format, Stream};
use crate::device::{DEFAULT_MEDIA_RECEIVER, SCCAST_RECEIVER};

/// Settings the menu edits, prefilled from the command line.
#[derive(Clone)]
pub struct Settings {
    pub stream: Stream,
    pub audio_delay: u64,
    pub latency: Option<f64>,
    pub receiver: String,
}

/// Name, resolution, bitrate. "Entry-level TV" comes from the Sharp tests: 4000 kbit/s froze on moving content.
const PRESETS: [(&str, (u32, u32), u32); 3] = [
    ("Entry-level TV", (1920, 1080), 2500),
    ("Full HD", (1920, 1080), 4000),
    ("720p", (1280, 720), 2500),
];
const RESOLUTIONS: [(u32, u32); 3] = [(1920, 1080), (1280, 720), (854, 480)];
const BITRATES: [u32; 6] = [1500, 2500, 4000, 6000, 8000, 10000];
const FPS: [u32; 4] = [25, 30, 50, 60];
/// None: the receiver page's default (1 s).
const LATENCIES: [Option<f64>; 5] = [None, Some(1.5), Some(2.0), Some(3.0), Some(5.0)];
/// 50 ms steps near 0, to tune lip sync by ear during a cast.
const AUDIO_DELAYS: [u64; 12] = [0, 50, 100, 150, 200, 250, 300, 400, 500, 750, 1000, 1500];
/// Row of the audio delay, applied at once to a running cast.
pub(super) const AUDIO_DELAY_ROW: usize = 6;

pub(super) const ROWS: [&str; 9] = [
    "Preset", "Resolution", "Bitrate", "Frame rate", "Latency", "Sound", "Audio delay", "Receiver", "Format",
];

/// Next (dir 1) or previous (dir -1) entry of `list`, wrapping. A value not in the list jumps to its first entry.
pub(super) fn cycle<T: PartialEq + Copy>(list: &[T], current: T, dir: isize) -> T {
    let len = list.len() as isize;
    let next = list.iter().position(|v| *v == current).map_or(0, |i| (i as isize + dir).rem_euclid(len));
    list[next as usize]
}

/// Index of the preset matching the current settings.
fn preset(s: &Settings) -> Option<usize> {
    PRESETS.iter().position(|(_, res, rate)| *res == s.stream.resolution && *rate == s.stream.bitrate)
}

/// Move the setting of `row` to its next (dir 1) or previous (dir -1) value.
pub(super) fn change(s: &mut Settings, row: usize, dir: isize) {
    let st = &mut s.stream;
    match row {
        0 => {
            let i = preset(s).map_or(0, |i| (i as isize + dir).rem_euclid(PRESETS.len() as isize) as usize);
            (_, s.stream.resolution, s.stream.bitrate) = PRESETS[i];
        }
        1 => st.resolution = cycle(&RESOLUTIONS, st.resolution, dir),
        2 => st.bitrate = cycle(&BITRATES, st.bitrate, dir),
        3 => st.fps = cycle(&FPS, st.fps, dir),
        4 => s.latency = cycle(&LATENCIES, s.latency, dir),
        5 => st.no_audio = !st.no_audio,
        AUDIO_DELAY_ROW => s.audio_delay = cycle(&AUDIO_DELAYS, s.audio_delay, dir),
        7 => s.receiver = cycle(&[SCCAST_RECEIVER, DEFAULT_MEDIA_RECEIVER], s.receiver.as_str(), dir).to_string(),
        _ => st.format = cycle(&[Format::Hls, Format::Mkv, Format::Mirror], st.format, dir),
    }
}

/// Text shown for the setting of `row`.
pub(super) fn value(s: &Settings, row: usize) -> String {
    let st = &s.stream;
    match row {
        0 => preset(s).map_or("Custom", |i| PRESETS[i].0).to_string(),
        1 => format!("{}x{}", st.resolution.0, st.resolution.1),
        2 => format!("{} kbit/s", st.bitrate),
        3 => format!("{} fps", st.fps),
        4 => s.latency.map_or("1 s (default)".to_string(), |l| format!("{l} s")),
        5 => if st.no_audio { "off" } else { "on" }.to_string(),
        6 => format!("{} ms", s.audio_delay),
        7 => match s.receiver.as_str() {
            SCCAST_RECEIVER => "sccast".to_string(),
            DEFAULT_MEDIA_RECEIVER => "Google".to_string(),
            other => other.to_string(),
        },
        _ => match st.format {
            Format::Hls => "HLS",
            Format::Mkv => "Matroska",
            Format::Mirror => "Mirror",
        }
        .to_string(),
    }
}
