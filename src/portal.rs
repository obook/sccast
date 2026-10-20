use std::os::fd::{AsRawFd, OwnedFd};

use anyhow::Context;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{PersistMode, Session};

/// Screen or window chosen by the user in the desktop's ScreenCast dialog.
/// Keep it alive while capturing: dropping it closes the portal session.
pub struct ScreenCast {
    pub node_id: u32,
    fd: OwnedFd,
    _session: Session<Screencast>,
    // Drives the D-Bus connection that holds the session.
    _runtime: tokio::runtime::Runtime,
}

impl ScreenCast {
    /// Open the portal dialog and wait for the user's choice.
    pub fn open() -> anyhow::Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let (session, node_id, fd) = runtime
            .block_on(async {
                let proxy = Screencast::new().await?;
                let session = proxy.create_session(Default::default()).await?;
                proxy
                    .select_sources(
                        &session,
                        SelectSourcesOptions::default()
                            .set_cursor_mode(CursorMode::Embedded)
                            .set_sources(SourceType::Monitor | SourceType::Window)
                            .set_multiple(false)
                            .set_persist_mode(PersistMode::DoNot),
                    )
                    .await?
                    .response()?;
                let streams = proxy.start(&session, None, Default::default()).await?.response()?;
                let stream = streams.streams().first().context("portal returned no stream")?;
                tracing::info!(node = stream.pipe_wire_node_id(), size = ?stream.size(), "screen selected");
                let node_id = stream.pipe_wire_node_id();
                let fd = proxy.open_pipe_wire_remote(&session, Default::default()).await?;
                anyhow::Ok((session, node_id, fd))
            })
            .context("ScreenCast portal failed (dialog cancelled?)")?;
        Ok(Self { node_id, fd, _session: session, _runtime: runtime })
    }

    pub fn fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }
}
