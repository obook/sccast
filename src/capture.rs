use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context};
use gstreamer as gst;
use gstreamer::prelude::*;
use tempfile::TempDir;

pub const PLAYLIST: &str = "live.m3u8";

// Keyframe every second (30 frames): segments cannot be shorter than the keyframe interval.
// Profile High with the encoder's auto level stays within 4.1 at 1080p30.
const PIPELINE: &str = "videotestsrc is-live=true \
    ! video/x-raw,width=1920,height=1080,framerate=30/1 \
    ! videoconvert \
    ! x264enc tune=zerolatency speed-preset=veryfast key-int-max=30 bitrate=4000 \
    ! video/x-h264,profile=high \
    ! h264parse \
    ! hlssink2 name=sink target-duration=1 playlist-length=3 max-files=6";

/// Running capture pipeline writing HLS into a temporary directory.
/// Dropping it stops the pipeline, then deletes the directory.
pub struct Hls {
    pipeline: gst::Element,
    dir: TempDir,
}

impl Hls {
    pub fn start() -> anyhow::Result<Self> {
        gst::init()?;
        // XDG_RUNTIME_DIR is a tmpfs: segments never hit the disk.
        let base = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir, Into::into);
        let dir = tempfile::Builder::new().prefix("sccast-").tempdir_in(base)?;

        let pipeline = gst::parse::launch(PIPELINE).context("cannot build pipeline (missing GStreamer plugin?)")?;
        let sink = pipeline
            .downcast_ref::<gst::Bin>()
            .and_then(|bin| bin.by_name("sink"))
            .context("hlssink2 not found in pipeline")?;
        sink.set_property("location", path_str(&dir.path().join("seg%05d.ts"))?);
        sink.set_property("playlist-location", path_str(&dir.path().join(PLAYLIST))?);

        pipeline.set_state(gst::State::Playing)?;
        tracing::info!(dir = %dir.path().display(), "capture started");
        Ok(Self { pipeline, dir })
    }

    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// Wait up to `timeout` for a pipeline message; fail on error or end of stream.
    pub fn poll(&self, timeout: Duration) -> anyhow::Result<()> {
        let bus = self.pipeline.bus().context("pipeline has no bus")?;
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_nseconds(timeout.as_nanos() as u64)) else {
            return Ok(());
        };
        match msg.view() {
            gst::MessageView::Error(err) => bail!(
                "pipeline error from {}: {} ({})",
                err.src().map(|s| s.path_string()).unwrap_or_default(),
                err.error(),
                err.debug().unwrap_or_default()
            ),
            gst::MessageView::Eos(_) => bail!("pipeline ended"),
            _ => Ok(()),
        }
    }
}

impl Drop for Hls {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn path_str(path: &Path) -> anyhow::Result<&str> {
    path.to_str().context("non UTF-8 temporary path")
}
