//! The XDG ScreenCast portal.
//!
//! A Wayland client cannot reach for a monitor or a window by itself, and the
//! portal is the sanctioned way around that on X11 too: the compositor runs the
//! picker, holds the consent, and answers with a PipeWire node to read the
//! source from. Nothing here touches pixels.

use std::os::fd::OwnedFd;
use std::pin::Pin;

use ashpd::desktop::screencast::{
    CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
    StartCastOptions,
};
use ashpd::desktop::{CreateSessionOptions, PersistMode, ResponseError, Session};
use futures::{Stream, StreamExt};

use crate::{CaptureRequest, Unavailable};

/// The session's `Closed` signal, reduced to "the share is over". It borrows the
/// [`Cast`] it came from, so it has to be dropped before the cast is closed.
pub(super) type Revoked<'a> = Pin<Box<dyn Stream<Item = ()> + 'a>>;

/// A screen cast the user has agreed to.
///
/// Dropping this leaves the session open on the portal's side: [`Cast::close`]
/// is what takes the compositor's "screen is being shared" indicator down.
///
/// Drop it, and the [`Revoked`] stream, only from inside the tokio runtime's
/// context: zbus unsubscribes from D-Bus by spawning a task, and spawning
/// outside a runtime panics.
pub(super) struct Cast {
    session: Session<Screencast>,
    pub(super) node_id: u32,
    /// The compositor's own idea of the source's size, in its coordinate space
    /// rather than in pixels, so no more than a starting point for the format
    /// negotiation.
    pub(super) size: Option<(u32, u32)>,
}

impl Cast {
    /// Fires when the compositor ends the cast — the user pressed stop, or the
    /// shared window went away. `None` when the signal cannot be subscribed to,
    /// which only costs us the second way of noticing; the stream dying is the
    /// first.
    pub(super) async fn revoked(&self) -> Option<Revoked<'_>> {
        match self.session.receive_closed().await {
            Ok(closed) => Some(Box::pin(closed.map(|_| ()))),
            Err(error) => {
                tracing::debug!(%error, "no Closed signal on the screen cast session");
                None
            }
        }
    }

    pub(super) async fn close(self) {
        if let Err(error) = self.session.close().await {
            tracing::debug!(%error, "the screen cast session did not close cleanly");
        }
    }
}

/// Asks the portal for a source and for the PipeWire remote it lives on.
///
/// Calls `opened` once the portal has answered with a session, which is the last
/// thing here that happens without a human: from there on this blocks on the
/// user for as long as the picker is up.
pub(super) async fn open(
    request: &CaptureRequest,
    opened: impl FnOnce(),
) -> Result<(Cast, OwnedFd), Unavailable> {
    let missing = || Unavailable::Unsupported("no desktop portal".to_string());

    let screencast = Screencast::new().await.map_err(|_| missing())?;
    // Still about the portal's own presence rather than about consent: a
    // compositor that cannot open a session cannot cast anything either.
    let session = screencast
        .create_session(CreateSessionOptions::default())
        .await
        .map_err(|_| missing())?;
    opened();

    match cast(&screencast, &session, request).await {
        Ok((node_id, size, remote)) => Ok((
            Cast {
                session,
                node_id,
                size,
            },
            remote,
        )),
        Err(err) => {
            let _ = session.close().await;
            Err(err)
        }
    }
}

async fn cast(
    screencast: &Screencast,
    session: &Session<Screencast>,
    request: &CaptureRequest,
) -> Result<(u32, Option<(u32, u32)>, OwnedFd), Unavailable> {
    // No restore token and nothing for the portal to persist, so every share
    // raises the picker: a share that repeated itself silently could put a
    // window the user has forgotten was chosen in front of the room.
    tracing::debug!(
        cursor = request.cursor,
        "opening a screen cast portal session"
    );

    let sources = SelectSourcesOptions::default()
        .set_cursor_mode(if request.cursor {
            CursorMode::Embedded
        } else {
            CursorMode::Hidden
        })
        .set_sources(SourceType::Monitor | SourceType::Window)
        .set_multiple(false)
        .set_persist_mode(PersistMode::DoNot);
    screencast
        .select_sources(session, sources)
        .await
        .map_err(refused)?;

    let streams = screencast
        .start(session, None, StartCastOptions::default())
        .await
        .and_then(|request| request.response())
        .map_err(refused)?;

    let stream = streams
        .streams()
        .first()
        .ok_or_else(|| Unavailable::PermissionDenied("no source selected".to_string()))?;
    let size = stream
        .size()
        .map(|(width, height)| (width.max(0) as u32, height.max(0) as u32));
    tracing::debug!(
        source = ?stream.source_type(),
        ?size,
        "the portal picked a screen cast source"
    );

    let remote = screencast
        .open_pipe_wire_remote(session, OpenPipeWireRemoteOptions::default())
        .await
        .map_err(|error| {
            Unavailable::Failed(format!("the portal gave no PipeWire remote: {error}"))
        })?;
    Ok((stream.pipe_wire_node_id(), size, remote))
}

/// A dismissed picker is the user saying no, not a broken portal.
fn refused(error: ashpd::Error) -> Unavailable {
    match error {
        ashpd::Error::Response(ResponseError::Cancelled) => {
            Unavailable::PermissionDenied("no source selected".to_string())
        }
        error => Unavailable::Failed(format!("the portal refused the screen cast: {error}")),
    }
}
