//! input.rs
//! Pointer control for touchback, through the RemoteDesktop portal, in a session of its own.
//! KDE's RemoteDesktop dialog lets the user pick no screen (it shares what it wants: the
//! whole workspace was cast once), so the capture keeps its own ScreenCast session
//! (portal.rs) and this one only moves the pointer. Absolute motion needs a stream of this
//! session: the one that contains the shared monitor is used, with the monitor's offset in it.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::sync::Arc;

use ashpd::desktop::remote_desktop::{Axis, DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions};
use ashpd::desktop::screencast::{Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::Session;
use ashpd::enumflags2::BitFlags;
use tokio::runtime::Runtime;

use super::portal::describe;

/// Linux input codes of the left and right buttons (BTN_LEFT, BTN_RIGHT).
const BUTTONS: [i32; 2] = [0x110, 0x111];

/// A rectangle of the desktop: position and size, in logical pixels.
pub type Rect = ((i32, i32), (i32, i32));

/// The desktop's pointer, moved in the shared monitor's coordinates.
pub struct Input {
    proxy: RemoteDesktop,
    session: Session<RemoteDesktop>,
    /// Stream of this session that contains the shared monitor.
    stream: u32,
    /// Where the shared monitor starts in that stream.
    offset: (f64, f64),
    /// Size of the shared monitor, in the desktop's logical pixels.
    pub size: (f64, f64),
    runtime: Arc<Runtime>,
}

/// The stream of `streams` (node, rectangle) that contains `monitor`, and the monitor's offset in it.
fn containing(monitor: Rect, streams: &[(u32, Rect)]) -> Option<(u32, (f64, f64))> {
    let ((x, y), (width, height)) = monitor;
    streams
        .iter()
        .find(|(_, ((left, top), (w, h)))| x >= *left && y >= *top && x + width <= left + w && y + height <= top + h)
        .map(|(node, ((left, top), _))| (*node, (f64::from(x - left), f64::from(y - top))))
}

impl Input {
    /// Open the RemoteDesktop session (KDE asks to allow remote control) for `monitor`, the
    /// shared screen. None, logged, when the desktop has no such portal, refuses the pointer,
    /// or gives no stream that contains the monitor.
    pub(super) async fn open(monitor: Rect, runtime: &Arc<Runtime>) -> Option<Self> {
        let opened = async {
            let proxy = RemoteDesktop::new().await?;
            let session = proxy.create_session(Default::default()).await?;
            let devices = SelectDevicesOptions::default().set_devices(BitFlags::from(DeviceType::Pointer));
            proxy.select_devices(&session, devices).await?.response()?;
            // Absolute motion needs a stream of this session; KDE picks it without asking.
            let monitors = SelectSourcesOptions::default().set_sources(BitFlags::from(SourceType::Monitor)).set_multiple(true);
            Screencast::new().await?.select_sources(&session, monitors).await?.response()?;
            let started = proxy.start(&session, None, Default::default()).await?.response()?;
            ashpd::Result::Ok((proxy, session, started))
        };
        let (proxy, session, started) = opened.await.inspect_err(|e| tracing::warn!("no RemoteDesktop portal, touchback off: {e}")).ok()?;
        describe("pointer", started.streams());
        if !started.devices().contains(DeviceType::Pointer) {
            tracing::warn!("the desktop did not grant the pointer: touchback off");
            return None;
        }
        let streams: Vec<(u32, Rect)> = started
            .streams()
            .iter()
            .filter_map(|stream| Some((stream.pipe_wire_node_id(), (stream.position().unwrap_or((0, 0)), stream.size()?))))
            .collect();
        let Some((stream, offset)) = containing(monitor, &streams) else {
            tracing::warn!(?monitor, "no pointer stream contains the shared screen: touchback off");
            return None;
        };
        tracing::info!(stream, ?offset, "touchback on");
        let size = (f64::from(monitor.1 .0), f64::from(monitor.1 .1));
        Some(Self { proxy, session, stream, offset, size, runtime: Arc::clone(runtime) })
    }

    /// Move the pointer to (`x`, `y`) of the shared monitor.
    pub fn move_to(&self, x: f64, y: f64) {
        let (x, y) = (x + self.offset.0, y + self.offset.1);
        let sent = self.runtime.block_on(self.proxy.notify_pointer_motion_absolute(&self.session, self.stream, x, y, Default::default()));
        log_failure(sent);
    }

    /// Press or release the left (`right` false) or right button.
    pub fn button(&self, right: bool, pressed: bool) {
        let state = if pressed { KeyState::Pressed } else { KeyState::Released };
        let button = BUTTONS[usize::from(right)];
        let sent = self.runtime.block_on(self.proxy.notify_pointer_button(&self.session, button, state, Default::default()));
        log_failure(sent);
    }

    /// Scroll by `steps` wheel notches, down when positive.
    pub fn wheel(&self, steps: i32) {
        let sent = self.runtime.block_on(self.proxy.notify_pointer_axis_discrete(&self.session, Axis::Vertical, steps, Default::default()));
        log_failure(sent);
    }
}

fn log_failure(sent: ashpd::Result<()>) {
    if let Err(e) = sent {
        tracing::warn!("pointer event refused by the portal: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_monitor_in_the_pointer_streams() {
        // The middle one of three 1920x1080 monitors.
        let middle = ((1920, 0), (1920, 1080));
        // One stream per monitor.
        let per_monitor = [(10, ((0, 0), (1920, 1080))), (11, ((1920, 0), (1920, 1080))), (12, ((3840, 0), (1920, 1080)))];
        assert_eq!(containing(middle, &per_monitor), Some((11, (0.0, 0.0))));
        // One stream for the whole workspace: the monitor starts 1920 px in.
        let workspace = [(20, ((0, 0), (5760, 1080)))];
        assert_eq!(containing(middle, &workspace), Some((20, (1920.0, 0.0))));
        // A stream that does not hold the whole monitor.
        assert_eq!(containing(middle, &[(30, ((0, 0), (1920, 1080)))]), None);
    }
}
