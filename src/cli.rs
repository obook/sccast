//! cli.rs
//! Command line options (clap): the commands list, cast, stop and serve, and the stream
//! settings they share. The menu starts from these values.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Cast the Linux desktop to a Chromecast.
#[derive(Parser)]
#[command(name = "sccast", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// List Chromecast devices on the local network.
    List {
        /// Discovery duration in seconds.
        #[arg(long, default_value_t = 8)]
        timeout: u64,
    },
    /// Cast the screen (or a media URL) to a device.
    Cast {
        /// Device name or id. Without it, choose from a list.
        #[arg(long)]
        device: Option<String>,
        /// Play this media URL instead of the screen.
        #[arg(long, conflicts_with = "file")]
        url: Option<String>,
        /// Play this local MP4 file, served over HTTP on the LAN.
        #[arg(long)]
        file: Option<PathBuf>,
        /// Delay the sound by this many milliseconds, if it comes before the image on the TV.
        /// The menu changes it during a cast.
        #[arg(long, default_value_t = 0)]
        audio_delay: u64,
        /// Receiver app id. Default: sccast's own (2-3 s of delay), falling back to Google's
        /// Default Media Receiver CC1AD845 (about 5 s) if it does not start.
        #[arg(long, default_value = crate::device::SCCAST_RECEIVER)]
        receiver: String,
        /// Seconds sccast's receiver stays behind live. At 1 s the Sharp buffered as soon as the
        /// PC was in use (2026-10-21); 1.5 s did not.
        #[arg(long, default_value_t = 1.5)]
        latency: f64,
        #[command(flatten)]
        stream: Stream,
    },
    /// Close the media receiver left running on a device.
    Stop {
        /// Device name or id. Without it, choose from a list.
        #[arg(long)]
        device: Option<String>,
        /// Receiver app id to close (Google's Default Media Receiver is closed too).
        #[arg(long, default_value = crate::device::SCCAST_RECEIVER)]
        receiver: String,
    },
    /// Serve the capture as HLS without casting, to check it with ffplay (VLC stutters on it).
    Serve {
        /// Capture a test pattern instead of the screen.
        #[arg(long)]
        test_pattern: bool,
        #[command(flatten)]
        stream: Stream,
    },
}

/// Capture and HTTP settings shared by `cast` and `serve`.
#[derive(Args, Clone)]
pub struct Stream {
    /// Output size, letterboxed if the screen has another shape.
    #[arg(long, default_value = "1920x1080", value_parser = parse_resolution)]
    pub resolution: (u32, u32),
    /// Frames per second. Above 30 at 1080p, some TVs cannot decode the stream.
    #[arg(long, default_value_t = 30)]
    pub fps: u32,
    /// Video bitrate in kbit/s. 4000 froze an entry-level TV on moving content.
    #[arg(long, default_value_t = 2500)]
    pub bitrate: u32,
    /// HTTP port the TV fetches the stream from (open it in the firewall).
    #[arg(long, default_value_t = 8090)]
    pub port: u16,
    /// No sound.
    #[arg(long)]
    pub no_audio: bool,
    /// mirror: Cast Streaming, as Chrome's tab casting, about 0.15 s behind the screen; HLS on a
    /// device without it. hls: 1-second segments, 2 to 3 s. mkv: one Matroska stream (experimental).
    /// The screen only: serve, --url and --file always use HLS.
    #[arg(long, value_enum, default_value_t = Format::Mirror)]
    pub format: Format,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    Hls,
    Mkv,
    Mirror,
}

fn parse_resolution(s: &str) -> Result<(u32, u32), String> {
    let (w, h) = s.split_once('x').ok_or("expected WIDTHxHEIGHT, e.g. 1280x720")?;
    let size = |v: &str| v.parse::<u32>().ok().filter(|n| *n > 0 && n % 2 == 0);
    match (size(w), size(h)) {
        (Some(w), Some(h)) => Ok((w, h)),
        _ => Err("width and height must be even positive numbers".into()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_resolution() {
        assert_eq!(super::parse_resolution("1280x720"), Ok((1280, 720)));
        assert!(super::parse_resolution("1280").is_err());
        assert!(super::parse_resolution("1281x720").is_err());
        assert!(super::parse_resolution("0x720").is_err());
    }
}
