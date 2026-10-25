//! encode.rs
//! Text description of the GStreamer pipeline (gst-launch syntax): screen or test pattern,
//! sound, encoders, and the output of each format (HLS files, Matroska over TCP, appsinks
//! for mirror). mod.rs builds and runs it.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use super::portal::ScreenCast;
use crate::cli::{Format, Stream};

pub const PLAYLIST: &str = "live.m3u8";
pub const MKV: &str = "live.mkv";

/// The whole pipeline for `stream`, from `screen` or a test pattern when it is None.
pub fn describe(screen: Option<&ScreenCast>, stream: &Stream) -> String {
    let (source, rate) = match screen {
        None => ("videotestsrc is-live=true".to_string(), ""),
        // `path` takes the node id; `target-object` wants the node serial, which the portal does not give.
        // PipeWire only sends frames on screen damage: keepalive resends the last one so a still
        // screen keeps producing segments, and videorate turns it into a constant frame rate.
        // Frames are stamped when pipewiresrc hands them out: the queue gives the encoding
        // its own thread, so a slow frame no longer holds the next ones in PipeWire (late
        // stamps put the sound ahead of the image). Leaky: drop frames rather than lag.
        // Conversion sees only real frames: `limit` drops those above the fps setting before it
        // (a video sends up to 60), `rate` adds the duplicates after it. drop-only needs the
        // fixed framerate after it: max-rate lets everything through from PipeWire's variable rate.
        Some(screen) => (
            format!(
                "pipewiresrc name=src fd={} path={} keepalive-time=100 \
                 ! queue name=vq leaky=downstream max-size-buffers=3 max-size-bytes=0 max-size-time=0 \
                 ! videorate name=limit drop-only=true ! video/x-raw,framerate={}/1",
                screen.fd(),
                screen.node_id,
                stream.fps
            ),
            " ! videorate name=rate",
        ),
    };
    // One thread by default: BGRA to I420 at 1080p60 does not fit on one core (frames lost on an i5-7600).
    let mut description =
        format!("{source} ! videoconvert n-threads=0 ! videoscale n-threads=0{rate} ! {}", video_encode(stream));
    if !stream.no_audio {
        let audio_source = match screen {
            None => "audiotestsrc name=asrc is-live=true wave=ticks volume=0.3",
            // Monitor of the default output: what the PC is playing. Works with pipewire-pulse.
            Some(_) => "pulsesrc name=asrc device=@DEFAULT_MONITOR@",
        };
        description.push_str(&format!(" {audio_source} ! {}", audio_encode(stream.format)));
    }
    description
}

/// Raw format and encoder for `stream`.
/// Letterboxed to the output size whatever the screen shape; I420 is what profile High accepts.
/// Keyframe every second: segments cannot be shorter than the keyframe interval.
/// Profile High with the encoder's auto level stays within 4.1 at 1080p30.
/// The receiver plays ~3 segments behind the live edge. With a playlist barely longer than
/// that, the playhead sat at the start of the window and the receiver kept jumping forward
/// and rebuffering, each stall adding latency for good. A 20 s window leaves it room;
/// latency depends on the 3 segments, not on the window length.
/// max-files must stay above playlist-length.
/// Matroska: tcpserversink sends each new client the stream header, then starts it at the
/// latest keyframe (as GNOME Network Displays does); the HTTP server relays it to the TV.
/// Mirror: one access unit per buffer, SPS/PPS before each key frame (the receiver can start on
/// any). Key frames every 5 s only: the receiver asks for one when it loses a frame.
fn video_encode(stream: &Stream) -> String {
    let Stream { resolution: (width, height), fps, bitrate, format, .. } = stream;
    let (key_interval, sink) = match format {
        Format::Hls => (*fps, "h264parse ! hlssink2 name=sink target-duration=1 playlist-length=20 max-files=25"),
        Format::Mkv => (
            *fps,
            "h264parse ! matroskamux name=sink streamable=true \
             ! tcpserversink name=out host=127.0.0.1 port=0 sync=false \
             sync-method=latest-keyframe recover-policy=keyframe",
        ),
        Format::Mirror => (
            fps * 5,
            "h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au \
             ! appsink name=sink sync=false max-buffers=4",
        ),
    };
    format!(
        "video/x-raw,format=I420,width={width},height={height},pixel-aspect-ratio=1/1,framerate={fps}/1 \
         ! x264enc name=venc tune=zerolatency speed-preset=veryfast key-int-max={key_interval} bitrate={bitrate} \
         ! video/x-h264,profile=high \
         ! {sink}"
    )
}

/// Mirror: Opus in 10 ms frames, each one a Cast frame.
fn audio_encode(format: Format) -> String {
    let encode = match format {
        Format::Hls => "avenc_aac bitrate=128000 ! aacparse ! sink.audio",
        Format::Mkv => "avenc_aac bitrate=128000 ! aacparse ! sink.audio_%u",
        Format::Mirror => "opusenc bitrate=128000 frame-size=10 ! appsink name=asink sync=false max-buffers=20",
    };
    format!("audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 ! {encode}")
}
