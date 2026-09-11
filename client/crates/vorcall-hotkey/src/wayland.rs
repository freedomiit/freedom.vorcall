//! Wayland backend: the `org.freedesktop.portal.GlobalShortcuts` portal.
//!
//! A Wayland client cannot read input it does not have focus for, by design.
//! The portal is the sanctioned way around that: the compositor keeps the
//! binding and tells us when it activates and deactivates, so the key is never
//! grabbed by us and never consumed on our behalf.

use std::sync::mpsc;
use std::time::Duration;

use ashpd::desktop::CreateSessionOptions;
use ashpd::desktop::global_shortcuts::{BindShortcutsOptions, GlobalShortcuts, NewShortcut};
use futures::StreamExt;
use futures::channel::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::{Backend, Binding, Edge, EdgeFilter, Listener, Stop, Unavailable, keymap};

const SHORTCUT_ID: &str = "push-to-talk";

/// Far beyond the budget the other backends keep, because binding a shortcut
/// can put a confirmation dialog in front of the user first.
const BIND_TIMEOUT: Duration = Duration::from_secs(10);

/// What the portal thread reports back once it has bound, or failed to bind:
/// the compositor's own description of the trigger it settled on.
type Ready = Result<Option<String>, Unavailable>;

pub(crate) fn start(
    binding: Binding,
    edges: UnboundedSender<Edge>,
) -> Result<Listener, Unavailable> {
    if matches!(binding, Binding::Mouse(_)) {
        return Err(Unavailable::Unsupported(
            "mouse buttons are window-only on Wayland".to_string(),
        ));
    }

    let (ready_tx, ready_rx) = mpsc::channel::<Ready>();
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    std::thread::Builder::new()
        .name("vorcall-hotkey".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    let _ = ready_tx.send(Err(Unavailable::Failed(format!(
                        "cannot start the portal runtime: {err}"
                    ))));
                    return;
                }
            };
            runtime.block_on(portal(binding, edges, &ready_tx, stop_rx));
        })
        .map_err(|err| Unavailable::Failed(format!("cannot start the listener thread: {err}")))?;

    let handle = Handle {
        stop: Some(stop_tx),
    };
    match ready_rx.recv_timeout(BIND_TIMEOUT) {
        Ok(Ok(description)) => Ok(Listener::new(
            Backend::WaylandPortal,
            description,
            Box::new(handle),
        )),
        Ok(Err(err)) => Err(err),
        // Dropping `handle` here signals the thread, which is still parked on
        // DBus; it winds itself down once the portal answers.
        Err(_) => Err(Unavailable::Failed(
            "the compositor did not answer the shortcut request".to_string(),
        )),
    }
}

async fn portal(
    binding: Binding,
    edges: UnboundedSender<Edge>,
    ready: &mpsc::Sender<Ready>,
    stop: oneshot::Receiver<()>,
) {
    // Everything up to and including the session is about whether the portal is
    // there at all; only `bind` can be refused.
    let no_portal =
        || Unavailable::Unsupported("the compositor has no GlobalShortcuts portal".to_string());

    let Ok(shortcuts) = GlobalShortcuts::new().await else {
        let _ = ready.send(Err(no_portal()));
        return;
    };
    // Still the portal's own availability, not the binding's: a compositor that
    // cannot open a session cannot bind anything either.
    let Ok(session) = shortcuts
        .create_session(CreateSessionOptions::default())
        .await
    else {
        let _ = ready.send(Err(no_portal()));
        return;
    };

    match bind(&shortcuts, &session, binding, edges, ready, stop).await {
        Ok(()) => {}
        Err(err) => {
            let _ = ready.send(Err(err));
        }
    }
    let _ = session.close().await;
}

async fn bind(
    shortcuts: &GlobalShortcuts,
    session: &ashpd::desktop::Session<GlobalShortcuts>,
    binding: Binding,
    edges: UnboundedSender<Edge>,
    ready: &mpsc::Sender<Ready>,
    stop: oneshot::Receiver<()>,
) -> Result<(), Unavailable> {
    let refused = || Unavailable::Failed("the compositor refused the shortcut".to_string());

    // Subscribed before binding, so an activation that lands between the reply
    // and the first poll is not lost.
    let (Ok(activated), Ok(deactivated)) = (
        shortcuts.receive_activated().await,
        shortcuts.receive_deactivated().await,
    ) else {
        return Err(refused());
    };

    let trigger = keymap::xdg_trigger(&binding);
    let shortcut =
        NewShortcut::new(SHORTCUT_ID, "Vorcall push to talk").preferred_trigger(trigger.as_deref());
    let bound = shortcuts
        .bind_shortcuts(session, &[shortcut], None, BindShortcutsOptions::default())
        .await
        .and_then(|request| request.response())
        .map_err(|_| refused())?;
    if bound.shortcuts().is_empty() {
        return Err(refused());
    }
    let description = bound
        .shortcuts()
        .iter()
        .find(|shortcut| shortcut.id() == SHORTCUT_ID)
        .map(|shortcut| shortcut.trigger_description().to_string());
    if ready.send(Ok(description)).is_err() {
        return Ok(());
    }

    let mut filter = EdgeFilter::default();
    futures::pin_mut!(activated, deactivated, stop);
    loop {
        let edge = tokio::select! {
            Some(signal) = activated.next() => {
                (signal.shortcut_id() == SHORTCUT_ID).then_some(Edge::Pressed)
            }
            Some(signal) = deactivated.next() => {
                (signal.shortcut_id() == SHORTCUT_ID).then_some(Edge::Released)
            }
            _ = &mut stop => break,
            else => break,
        };
        let Some(edge) = edge else {
            continue;
        };
        if filter.admit(edge) && edges.unbounded_send(edge).is_err() {
            break;
        }
    }
    Ok(())
}

struct Handle {
    stop: Option<oneshot::Sender<()>>,
}

impl Stop for Handle {
    fn stop(&mut self) {
        // Signal only, never join: the thread still owes the portal a session
        // close, and this must not stall the caller's drop.
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}
