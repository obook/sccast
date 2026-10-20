use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{bail, Context};
use gstreamer as gst;
use gstreamer::prelude::*;
use tempfile::TempDir;

use crate::cli::Stream;
use crate::portal::ScreenCast;

pub const PLAYLIST: &str = "live.m3u8";

/// Raw format and encoder for `stream`.
/// Letterboxed to the output size whatever the screen shape; I420 is what profile High accepts.
/// Keyframe every second: segments cannot be shorter than the keyframe interval.
/// Profile High with the encoder's auto level stays within 4.1 at 1080p30.
/// The receiver plays ~3 segments behind the live edge. With a playlist barely longer than
/// that, the playhead sat at the start of the window and the receiver kept jumping forward
/// and rebuffering, each stall adding latency for good. A 20 s window leaves it room;
/// latency depends on the 3 segments, not on the window length.
/// max-files must stay above playlist-length.
fn video_encode(stream: &Stream) -> String {
    let Stream { resolution: (width, height), fps, bitrate, .. } = stream;
    format!(
        "video/x-raw,format=I420,width={width},height={height},pixel-aspect-ratio=1/1,framerate={fps}/1 \
         ! x264enc tune=zerolatency speed-preset=veryfast key-int-max={fps} bitrate={bitrate} \
         ! video/x-h264,profile=high \
         ! h264parse \
         ! hlssink2 name=sink target-duration=1 playlist-length=20 max-files=25"
    )
}

const AUDIO_ENCODE: &str = "audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 \
    ! avenc_aac bitrate=128000 ! aacparse ! sink.audio";

/// Segments to wait for before casting: the window must already be twice the receiver's
/// 3-segment hold-back, or it starts at the window edge and stalls until the playlist grows.
pub const MIN_SEGMENTS: usize = 6;

/// Running capture pipeline writing HLS into a temporary directory.
/// Dropping it stops the pipeline, then deletes the directory and closes the portal session.
pub struct Hls {
    pipeline: gst::Element,
    dir: TempDir,
    _screen: Option<ScreenCast>,
}

impl Hls {
    /// Capture `screen` and the PC sound, or a test pattern and test ticks when `screen` is None.
    /// `audio_delay` shifts the sound later to line it up with the image.
    pub fn start(screen: Option<ScreenCast>, stream: &Stream, audio_delay: Duration) -> anyhow::Result<Self> {
        gst::init()?;
        // XDG_RUNTIME_DIR is a tmpfs: segments never hit the disk.
        let base = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir, Into::into);
        let dir = tempfile::Builder::new().prefix("sccast-").tempdir_in(base)?;

        let source = match &screen {
            None => "videotestsrc is-live=true".to_string(),
            // `path` takes the node id; `target-object` wants the node serial, which the portal does not give.
            // PipeWire only sends frames on screen damage: keepalive resends the last one so a still
            // screen keeps producing segments, and videorate turns it into a constant frame rate.
            // Frames are stamped when pipewiresrc hands them out: the queue gives the encoding
            // its own thread, so a slow frame no longer holds the next ones in PipeWire (late
            // stamps put the sound ahead of the image). Leaky: drop frames rather than lag.
            Some(screen) => format!(
                "pipewiresrc name=src fd={} path={} keepalive-time=100 \
                 ! queue leaky=downstream max-size-buffers=3 max-size-bytes=0 max-size-time=0 \
                 ! videorate",
                screen.fd(),
                screen.node_id
            ),
        };
        let mut description = format!("{source} ! videoconvert ! videoscale ! {}", video_encode(stream));
        if !stream.no_audio {
            let audio_source = match &screen {
                None => "audiotestsrc name=asrc is-live=true wave=ticks volume=0.3",
                // Monitor of the default output: what the PC is playing. Works with pipewire-pulse.
                Some(_) => "pulsesrc name=asrc device=@DEFAULT_MONITOR@",
            };
            description.push_str(&format!(" {audio_source} ! {AUDIO_ENCODE}"));
        }
        tracing::debug!(%description, "pipeline");

        let pipeline = gst::parse::launch(&description).context("cannot build pipeline (missing GStreamer plugin?)")?;
        let bin = pipeline.downcast_ref::<gst::Bin>().context("pipeline is not a bin")?;
        if let Some(src) = bin.by_name("src") {
            restamp_on_arrival(&src, &pipeline)?;
        }
        if let Some(asrc) = bin.by_name("asrc") {
            let pad = asrc.static_pad("src").context("audio source has no src pad")?;
            pad.set_offset(audio_delay.as_nanos() as i64);
        }
        let sink = bin.by_name("sink").context("hlssink2 not found in pipeline")?;
        sink.set_property("location", path_str(&dir.path().join("seg%05d.ts"))?);
        sink.set_property("playlist-location", path_str(&dir.path().join(PLAYLIST))?);

        pipeline.set_state(gst::State::Playing)?;
        tracing::info!(dir = %dir.path().display(), "capture started");
        Ok(Self { pipeline, dir, _screen: screen })
    }

    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// Number of segments currently on disk.
    pub fn segments(&self) -> std::io::Result<usize> {
        let mut count = 0;
        for entry in std::fs::read_dir(self.dir())? {
            if entry?.path().extension().is_some_and(|ext| ext == "ts") {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Wait up to `timeout` for a pipeline message; fail on error or end of stream.
    pub fn poll(&self, timeout: Duration) -> anyhow::Result<()> {
        poll_bus(&self.pipeline.bus().context("pipeline has no bus")?, timeout)
    }

    /// Watch the pipeline from a thread until `stop` is set.
    /// On a capture failure, set `stop` (which frees the TV) and return the error.
    pub fn watch(&self, stop: &'static AtomicBool) -> anyhow::Result<JoinHandle<anyhow::Result<()>>> {
        let bus = self.pipeline.bus().context("pipeline has no bus")?;
        Ok(std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                if let Err(e) = poll_bus(&bus, Duration::from_millis(200)) {
                    stop.store(true, Ordering::SeqCst);
                    return Err(e);
                }
            }
            Ok(())
        }))
    }
}

fn poll_bus(bus: &gst::Bus, timeout: Duration) -> anyhow::Result<()> {
    let Some(msg) = bus.timed_pop(gst::ClockTime::from_nseconds(timeout.as_nanos() as u64)) else {
        return Ok(());
    };
    match msg.view() {
        gst::MessageView::Error(err) => bail!(
            "capture failed in {}: {} ({})",
            err.src().map(|s| s.path_string()).unwrap_or_default(),
            err.error(),
            err.debug().unwrap_or_default()
        ),
        gst::MessageView::Eos(_) => bail!("capture ended"),
        _ => Ok(()),
    }
}

impl Drop for Hls {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Replace the timestamps of `src` buffers with the pipeline running time at arrival.
/// With pipewiresrc on KWin, even with do-timestamp, videorate produced 3 s of video
/// per wall-clock second: the source timestamps cannot be trusted.
fn restamp_on_arrival(src: &gst::Element, pipeline: &gst::Element) -> anyhow::Result<()> {
    let pad = src.static_pad("src").context("source has no src pad")?;
    let pipeline = pipeline.downgrade();
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        let now = pipeline.upgrade().and_then(|p| p.current_running_time());
        if let Some(gst::PadProbeData::Buffer(buffer)) = &mut info.data {
            let buffer = buffer.make_mut();
            buffer.set_pts(now);
            buffer.set_dts(gst::ClockTime::NONE);
        }
        gst::PadProbeReturn::Ok
    });
    Ok(())
}

fn path_str(path: &Path) -> anyhow::Result<&str> {
    path.to_str().context("non UTF-8 temporary path")
}
