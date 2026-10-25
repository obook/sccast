//! capture/
//! Everything on the PC side: the screen chosen through the portal (portal.rs), and the
//! GStreamer pipeline that captures the screen with the sound and encodes it (encode.rs
//! describes it, this file runs and watches it).
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

mod encode;
mod portal;

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{bail, Context};
use gstreamer as gst;
use gstreamer::prelude::*;
use tempfile::TempDir;

use crate::cli::{Format, Stream};
pub use encode::{MKV, PLAYLIST};
pub use portal::ScreenCast;

/// Segments to wait for before casting: the window must already be twice the receiver's
/// 3-segment hold-back, or it starts at the window edge and stalls until the playlist grows.
pub const MIN_SEGMENTS: usize = 6;

/// Running capture pipeline, writing HLS into a temporary directory or Matroska to a local TCP port.
/// Dropping it stops the pipeline, then deletes the directory. The portal session outlives it,
/// so a new capture can start on the same screen without asking again.
pub struct Capture {
    pipeline: gst::Element,
    dir: TempDir,
    /// Local port of the Matroska stream (mkv format only).
    pub tcp_port: Option<u16>,
    /// Frames dropped before the encoder since the start: the PC is too slow.
    pub dropped: Arc<AtomicU64>,
}

impl Capture {
    /// Capture `screen` and the PC sound, or a test pattern and test ticks when `screen` is None.
    /// `audio_delay_ms` shifts the sound later to line it up with the image, or the image when negative.
    pub fn start(screen: Option<&ScreenCast>, stream: &Stream, audio_delay_ms: i64) -> anyhow::Result<Self> {
        gst::init()?;
        // XDG_RUNTIME_DIR is a tmpfs: segments never hit the disk.
        let base = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir, Into::into);
        let dir = tempfile::Builder::new().prefix("sccast-").tempdir_in(base)?;

        let description = encode::describe(screen, stream);
        tracing::debug!(%description, "pipeline");

        if let Some((element, package)) = missing_plugin() {
            bail!("GStreamer has no {element}: install {package} (Debian and Ubuntu name)");
        }
        let pipeline = gst::parse::launch(&description).context("cannot build the GStreamer pipeline")?;
        let bin = pipeline.downcast_ref::<gst::Bin>().context("pipeline is not a bin")?;
        if let Some(src) = bin.by_name("src") {
            restamp_on_arrival(&src, &pipeline)?;
            // The size PipeWire really sends: a whole multi-monitor workspace shows here.
            if let Some(pad) = src.static_pad("src") {
                pad.connect_notify(Some("caps"), |pad, _| {
                    if let Some(caps) = pad.current_caps() {
                        tracing::info!(%caps, "screen caps");
                    }
                });
            }
        }
        // The leaky queue before the encoder overflows when the PC cannot encode fast enough:
        // each overrun drops a frame.
        let dropped = Arc::new(AtomicU64::new(0));
        if let Some(queue) = bin.by_name("vq") {
            let dropped = Arc::clone(&dropped);
            queue.connect("overrun", false, move |_| {
                dropped.fetch_add(1, Ordering::Relaxed);
                None
            });
        }
        if let (Some(limit), Some(rate)) = (bin.by_name("limit"), bin.by_name("rate")) {
            log_frame_rate(limit, &rate)?;
        }
        if stream.format == Format::Hls {
            let sink = bin.by_name("sink").context("hlssink2 not found in pipeline")?;
            sink.set_property("location", path_str(&dir.path().join("seg%05d.ts"))?);
            sink.set_property("playlist-location", path_str(&dir.path().join(PLAYLIST))?);
        }

        pipeline.set_state(gst::State::Playing)?;
        // tcpserversink binds port 0 when it starts: read the port it got.
        let tcp_port = match bin.by_name("out") {
            Some(out) => Some(u16::try_from(out.property::<i32>("current-port")).context("no Matroska port")?),
            None => None,
        };
        tracing::info!(dir = %dir.path().display(), ?tcp_port, "capture started");
        let capture = Self { pipeline, dir, tcp_port, dropped };
        set_audio_delay(&capture.pipeline, audio_delay_ms);
        Ok(capture)
    }

    /// Bytes sent so far by the Matroska server (mkv format only), to measure the bitrate.
    pub fn served(&self) -> Option<impl Fn() -> u64 + Send + 'static> {
        let out = self.pipeline.downcast_ref::<gst::Bin>()?.by_name("out")?;
        Some(move || out.property::<u64>("bytes-served"))
    }

    /// The pipeline, for its running time (mirror format).
    pub fn pipeline(&self) -> gst::Element {
        self.pipeline.clone()
    }

    /// Element `name` of the pipeline: appsinks and encoder of the mirror format.
    pub fn element(&self, name: &str) -> Option<gst::Element> {
        self.pipeline.downcast_ref::<gst::Bin>()?.by_name(name)
    }

    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// Number of segments currently on disk.
    pub fn segments(&self) -> std::io::Result<usize> {
        let ts = |e: &std::fs::DirEntry| e.path().extension().is_some_and(|ext| ext == "ts");
        Ok(std::fs::read_dir(self.dir())?.flatten().filter(ts).count())
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

impl Drop for Capture {
    fn drop(&mut self) {
        // Timed: a stop that never ends shows in the log as a start without its end.
        let started = std::time::Instant::now();
        tracing::info!("capture stopping");
        let _ = self.pipeline.set_state(gst::State::Null);
        tracing::info!(ms = started.elapsed().as_millis() as u64, "capture stopped");
    }
}

/// Delay the sound by `ms`, or the screen image when `ms` is negative (the Sharp plays the
/// mirror sound late: -300 ms fixed it). Only at the start: changed live, it froze the image.
fn set_audio_delay(pipeline: &gst::Element, ms: i64) {
    let Some(bin) = pipeline.downcast_ref::<gst::Bin>() else { return };
    for (source, delay) in [("asrc", ms), ("src", -ms)] {
        if let Some(pad) = bin.by_name(source).and_then(|e| e.static_pad("src")) {
            pad.set_offset(delay.max(0).saturating_mul(1_000_000));
        }
    }
}

/// Ask `encoder` for a key frame with SPS and PPS.
/// Upstream on the src pad: not serialized, so no wait for the encoder's stream lock.
pub fn request_key_frame(encoder: &gst::Element) {
    let event = gst::Structure::builder("GstForceKeyUnit").field("all-headers", true).build();
    if let Some(pad) = encoder.static_pad("src") {
        pad.send_event(gst::event::CustomUpstream::new(event));
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

/// Every 5 s, log the frames the screen really sent against the ones videorate made up:
/// the fps setting is the output rate, `limit` drops what the screen sends above it and
/// `rate` duplicates to fill what it does not send.
fn log_frame_rate(limit: gst::Element, rate: &gst::Element) -> anyhow::Result<()> {
    const PERIOD: Duration = Duration::from_secs(5);
    let pad = rate.static_pad("src").context("videorate has no src pad")?;
    let last = std::sync::Mutex::new((std::time::Instant::now(), 0u64, 0u64, 0u64));
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, _| {
        let Ok(mut last) = last.lock() else { return gst::PadProbeReturn::Ok };
        let elapsed = last.0.elapsed();
        if elapsed >= PERIOD {
            if let Some(rate) = pad.parent_element() {
                let (input, dup, drop) = (
                    limit.property::<u64>("in"),
                    rate.property::<u64>("duplicate"),
                    limit.property::<u64>("drop") + rate.property::<u64>("drop"),
                );
                let secs = elapsed.as_secs_f64();
                tracing::info!(
                    screen_fps = format!("{:.1}", (input - last.1) as f64 / secs),
                    duplicated_fps = format!("{:.1}", (dup - last.2) as f64 / secs),
                    dropped = drop - last.3,
                    "frame rate"
                );
                *last = (std::time::Instant::now(), input, dup, drop);
            }
        }
        gst::PadProbeReturn::Ok
    });
    Ok(())
}

/// First element sccast needs that GStreamer lacks, with its package. The .deb pulls them all
/// in; a build from source may miss one (x264enc is not in the default plugins).
fn missing_plugin() -> Option<(&'static str, &'static str)> {
    const NEEDED: [(&str, &str); 7] = [
        ("x264enc", "gstreamer1.0-plugins-ugly"),
        ("pipewiresrc", "gstreamer1.0-pipewire"),
        ("pulsesrc", "gstreamer1.0-plugins-good"),
        ("hlssink2", "gstreamer1.0-plugins-bad"),
        ("avenc_aac", "gstreamer1.0-libav"),
        ("opusenc", "gstreamer1.0-plugins-base"),
        ("appsink", "gstreamer1.0-plugins-base"),
    ];
    NEEDED.into_iter().find(|(element, _)| gst::ElementFactory::find(element).is_none())
}

fn path_str(path: &Path) -> anyhow::Result<&str> {
    path.to_str().context("non UTF-8 temporary path")
}
