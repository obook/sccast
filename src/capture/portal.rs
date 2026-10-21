//! portal.rs
//! Ask the desktop which screen or window to share, through the XDG ScreenCast portal.
//! Wayland lets no program read the screen directly: the portal returns a PipeWire node
//! that the capture pipeline (mod.rs) reads.
//! With touchback, the same dialog also grants pointer control (RemoteDesktop portal): the
//! panel's touches move the pointer, in the coordinates of the shared screen.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;

use anyhow::Context;
use ashpd::desktop::remote_desktop::{Axis, DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{PersistMode, Session};
use tokio::runtime::Runtime;

/// Linux input codes of the left and right buttons (BTN_LEFT, BTN_RIGHT).
const BUTTONS: [i32; 2] = [0x110, 0x111];

/// Screen or window chosen by the user in the desktop's ScreenCast dialog.
/// Keep it alive while capturing: dropping it closes the portal session.
pub struct ScreenCast {
    pub node_id: u32,
    fd: OwnedFd,
    /// RemoteDesktop session, when touchback was asked (the capture lives on it too).
    remote: Option<Arc<Input>>,
    _session: Option<Session<Screencast>>,
    // Drives the D-Bus connection that holds the session.
    _runtime: Arc<Runtime>,
}

/// The desktop's pointer, moved in the shared screen's coordinates.
pub struct Input {
    proxy: RemoteDesktop,
    session: Session<RemoteDesktop>,
    stream: u32,
    /// Size of the shared screen, in the desktop's logical pixels.
    pub size: (f64, f64),
    /// The desktop granted the pointer, not only the screen.
    granted: bool,
    runtime: Arc<Runtime>,
}

fn sources() -> SelectSourcesOptions {
    SelectSourcesOptions::default()
        .set_cursor_mode(CursorMode::Embedded)
        .set_sources(SourceType::Monitor | SourceType::Window)
        .set_multiple(false)
        .set_persist_mode(PersistMode::DoNot)
}

impl ScreenCast {
    /// Open the portal dialog and wait for the user's choice. `touchback`: also ask for the pointer.
    pub fn open(touchback: bool) -> anyhow::Result<Self> {
        let runtime = Arc::new(tokio::runtime::Builder::new_current_thread().enable_all().build()?);
        let (node_id, fd, session, input) = runtime
            .block_on(async {
                let screencast = Screencast::new().await?;
                if !touchback {
                    let session = screencast.create_session(Default::default()).await?;
                    screencast.select_sources(&session, sources()).await?.response()?;
                    let streams = screencast.start(&session, None, Default::default()).await?.response()?;
                    let stream = streams.streams().first().context("portal returned no stream")?;
                    tracing::info!(node = stream.pipe_wire_node_id(), size = ?stream.size(), "screen selected");
                    let fd = screencast.open_pipe_wire_remote(&session, Default::default()).await?;
                    return anyhow::Ok((stream.pipe_wire_node_id(), fd, Some(session), None));
                }
                let remote = RemoteDesktop::new().await?;
                let session = remote.create_session(Default::default()).await?;
                let devices = SelectDevicesOptions::default().set_devices(ashpd::enumflags2::BitFlags::from(DeviceType::Pointer));
                remote.select_devices(&session, devices).await?.response()?;
                screencast.select_sources(&session, sources()).await?.response()?;
                let started = remote.start(&session, None, Default::default()).await?.response()?;
                let stream = started.streams().first().context("portal returned no stream")?;
                let (width, height) = stream.size().context("portal gave no screen size")?;
                // The dialog may grant the screen and not the pointer: check what it gave.
                let pointer = started.devices().contains(DeviceType::Pointer);
                tracing::info!(node = stream.pipe_wire_node_id(), width, height, devices = ?started.devices(), pointer, "screen selected for touchback");
                if !pointer {
                    tracing::warn!("the desktop did not grant the pointer: touchback stays off");
                }
                let fd = screencast.open_pipe_wire_remote(&session, Default::default()).await?;
                let node = stream.pipe_wire_node_id();
                // The session is kept even without the pointer: the capture lives on it.
                anyhow::Ok((node, fd, None, Some((remote, session, node, (f64::from(width), f64::from(height)), pointer))))
            })
            .context("ScreenCast portal failed (dialog cancelled?)")?;
        let remote = input.map(|(proxy, session, stream, size, granted)| {
            Arc::new(Input { proxy, session, stream, size, granted, runtime: Arc::clone(&runtime) })
        });
        Ok(Self { node_id, fd, remote, _session: session, _runtime: runtime })
    }

    /// Pointer control on the shared screen, when touchback was asked and the desktop granted it.
    pub fn input(&self) -> Option<Arc<Input>> {
        self.remote.clone().filter(|input| input.granted)
    }

    pub fn fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }
}

impl Input {
    /// Move the pointer to (`x`, `y`) of the shared screen.
    pub fn move_to(&self, x: f64, y: f64) {
        let sent = self.runtime.block_on(self.proxy.notify_pointer_motion_absolute(&self.session, self.stream, x, y, Default::default()));
        log_failure(sent);
    }

    /// Press or release the left (`right` false) or right button.
    pub fn button(&self, right: bool, pressed: bool) {
        let state = if pressed { KeyState::Pressed } else { KeyState::Released };
        let sent = self.runtime.block_on(self.proxy.notify_pointer_button(&self.session, BUTTONS[usize::from(right)], state, Default::default()));
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
