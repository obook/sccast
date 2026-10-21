//! portal.rs
//! Ask the desktop which screen or window to share, through the XDG ScreenCast portal.
//! Wayland lets no program read the screen directly: the portal returns a PipeWire node
//! that the capture pipeline (mod.rs) reads.
//! With touchback, a second session grants pointer control on the shared monitor (input.rs).
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::{Arc, OnceLock};

use anyhow::Context;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType, Stream};
use ashpd::desktop::{PersistMode, Session};
use tokio::runtime::Runtime;

use super::input::Input;

/// The one runtime of all portal calls. ashpd keeps a single D-Bus connection for the whole
/// process, driven by the runtime that opened it: dropping that runtime would stall every
/// later call, so it lives as long as the process.
pub(super) fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime"))
}

/// Screen or window chosen by the user in the desktop's ScreenCast dialog.
/// Keep it alive while capturing: dropping it closes the portal session.
pub struct ScreenCast {
    pub node_id: u32,
    fd: OwnedFd,
    /// Streams the portal gave; sccast casts the first one.
    streams: usize,
    touchback: bool,
    /// Pointer control on the shared monitor, when touchback was asked and could be set up.
    input: Option<Arc<Input>>,
    session: Session<Screencast>,
}

/// Log what a portal session gave, one line: with several monitors, the dialog may offer the
/// whole workspace or one monitor. Returns the stream count.
pub(super) fn describe(what: &str, streams: &[Stream]) -> usize {
    let each: Vec<String> = streams
        .iter()
        .map(|stream| {
            let (node, kind) = (stream.pipe_wire_node_id(), stream.source_type());
            format!("node {node} {kind:?} at {:?} size {:?}", stream.position(), stream.size())
        })
        .collect();
    tracing::info!(what, count = streams.len(), streams = %each.join("; "), "portal streams");
    streams.len()
}

fn sources() -> SelectSourcesOptions {
    SelectSourcesOptions::default()
        .set_cursor_mode(CursorMode::Embedded)
        .set_sources(SourceType::Monitor | SourceType::Window)
        .set_multiple(false)
        .set_persist_mode(PersistMode::DoNot)
}

impl ScreenCast {
    /// Open the portal dialog and wait for the user's choice. `touchback`: then also ask for
    /// the pointer, in a second dialog.
    pub fn open(touchback: bool) -> anyhow::Result<Self> {
        let (node_id, fd, session, streams, input) = runtime()
            .block_on(async {
                let screencast = Screencast::new().await?;
                let session = screencast.create_session(Default::default()).await?;
                screencast.select_sources(&session, sources()).await?.response()?;
                let started = screencast.start(&session, None, Default::default()).await?.response()?;
                let count = describe("screen", started.streams());
                let stream = started.streams().first().context("portal returned no stream")?;
                let node = stream.pipe_wire_node_id();
                let fd = screencast.open_pipe_wire_remote(&session, Default::default()).await?;
                // A window has no place on the desktop: touchback needs a whole monitor. The
                // portal may leave out the position of a single monitor: it is then at 0,0.
                let is_window = stream.source_type() == Some(SourceType::Window);
                let monitor = stream.size().filter(|_| !is_window).map(|size| (stream.position().unwrap_or((0, 0)), size));
                let input = match (touchback, monitor) {
                    (true, Some(monitor)) => Input::open(monitor).await,
                    (true, None) => {
                        tracing::warn!("a window was shared, not a screen: touchback off");
                        None
                    }
                    (false, _) => None,
                };
                anyhow::Ok((node, fd, session, count, input))
            })
            .context("ScreenCast portal failed (dialog cancelled?)")?;
        let input = input.map(Arc::new);
        Ok(Self { node_id, fd, streams, touchback, input, session })
    }

    /// Pointer control on the shared monitor, when touchback was asked and could be set up.
    pub fn input(&self) -> Option<Arc<Input>> {
        self.input.clone()
    }

    /// A word for the menu: several streams (only the first is cast), touchback refused, or both.
    pub fn note(&self) -> Option<String> {
        let mut notes = Vec::new();
        if self.streams > 1 {
            notes.push(format!("The desktop shared {} screens: only the first is cast.", self.streams));
        }
        if self.touchback && self.input.is_none() {
            notes.push("Touchback off: share a whole screen and allow remote control.".to_string());
        }
        (!notes.is_empty()).then(|| format!("{} p: choose again", notes.join(" ")))
    }

    pub fn fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }
}

/// Close a portal session: ashpd sessions stay open until closed, and the desktop keeps
/// sharing. At most 2 s: a portal that does not answer must not hang sccast.
pub(super) async fn close<T: ashpd::desktop::SessionPortal>(session: &Session<T>, what: &str) {
    match tokio::time::timeout(std::time::Duration::from_secs(2), session.close()).await {
        Ok(Ok(())) => tracing::info!(what, "portal session closed"),
        Ok(Err(e)) => tracing::warn!(what, "cannot close the portal session: {e}"),
        Err(_) => tracing::warn!(what, "the portal did not close the session in 2 s"),
    }
}

impl Drop for ScreenCast {
    fn drop(&mut self) {
        runtime().block_on(close(&self.session, "screen"));
    }
}
