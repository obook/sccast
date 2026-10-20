use std::path::PathBuf;

use clap::{Parser, Subcommand};

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
        /// Device name or id.
        #[arg(long)]
        device: Option<String>,
        /// Play this media URL instead of the screen.
        #[arg(long, conflicts_with = "file")]
        url: Option<String>,
        /// Play this local MP4 file, served over HTTP on the LAN.
        #[arg(long)]
        file: Option<PathBuf>,
        /// Cast the screen without sound.
        #[arg(long)]
        no_audio: bool,
        /// Delay the sound by this many milliseconds, if it comes before the image on the TV.
        #[arg(long, default_value_t = 0)]
        audio_delay: u64,
    },
    /// Close the media receiver left running on a device.
    Stop {
        /// Device name or id.
        #[arg(long)]
        device: Option<String>,
    },
    /// Serve the capture as HLS without casting, to check it with ffplay or VLC.
    Serve {
        /// Capture a test pattern instead of the screen.
        #[arg(long)]
        test_pattern: bool,
        /// Serve the capture without sound.
        #[arg(long)]
        no_audio: bool,
    },
}
