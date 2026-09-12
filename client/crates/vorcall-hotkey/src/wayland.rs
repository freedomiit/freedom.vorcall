//! Wayland backend: the `org.freedesktop.portal.GlobalShortcuts` portal.
//!
//! A Wayland client cannot read input it does not have focus for, by design.
//! The portal is the sanctioned way around that: the compositor keeps the
//! bindings and tells us when one activates and deactivates, so the key is never
//! grabbed by us and never consumed on our behalf.
//!
//! Every action is one shortcut of a single `BindShortcuts` call, which is also
//! the only confirmation dialog the user sees. The compositor matches the
//! modifiers itself — the trigger string carries them — so nothing here tracks
//! modifier state.

use std::sync::mpsc;
use std::time::Duration;

use ashpd::desktop::CreateSessionOptions;
use ashpd::desktop::global_shortcuts::{BindShortcutsOptions, GlobalShortcuts, NewShortcut};
use futures::StreamExt;
use futures::channel::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::{
    ActionId, Backend, Edge, Listener, Mods, Router, Shortcut, Stop, Trigger, Unavailable, keymap,
};

/// The action the app documents as push to talk, which is the one the compositor
/// names in its dialog.
const PUSH_TO_TALK: ActionId = 0;

/// Far beyond the budget the other backends keep, because binding a shortcut
/// can put a confirmation dialog in front of the user first.
const BIND_TIMEOUT: Duration = Duration::from_secs(10);

/// What the portal thread reports back once it has bound, or failed to bind:
/// the compositor's own description of the trigger it settled on, per action.
type Ready = Result<Vec<(ActionId, String)>, Unavailable>;

/// One shortcut as the portal wants it: an id of our own making, the trigger we
/// would prefer, and the sentence the user is asked to confirm.
struct Request {
    action: ActionId,
    id: String,
    trigger: Option<String>,
    description: String,
}

impl Request {
    fn new(shortcut: &Shortcut) -> Request {
        let description = if shortcut.description.is_empty() {
            default_description(shortcut.action)
        } else {
            shortcut.description.clone()
        };
        Request {
            action: shortcut.action,
            id: format!("action-{}", shortcut.action),
            trigger: keymap::xdg_trigger(&shortcut.binding),
            description,
        }
    }
}

fn default_description(action: ActionId) -> String {
    if action == PUSH_TO_TALK {
        "Vorcall push to talk".to_string()
    } else {
        format!("Vorcall shortcut {action}")
    }
}

pub(crate) fn start(
    bindings: Vec<Shortcut>,
    edges: UnboundedSender<(ActionId, Edge)>,
) -> Result<Listener, Unavailable> {
    // The portal is keyboard-only, so a mouse binding is left out of the one
    // `BindShortcuts` call instead of costing the keyboard ones their listener.
    let (bound, unavailable) =
        crate::partition(&bindings, |shortcut| match shortcut.binding.trigger {
            Trigger::Mouse(_) => Err(Unavailable::Unsupported(
                "mouse buttons are window-only on Wayland".to_string(),
            )),
            Trigger::Key(_) => Ok(Request::new(shortcut)),
        });
    if bound.is_empty() {
        return Err(crate::nothing_bindable(&unavailable));
    }
    let requests: Vec<Request> = bound.into_iter().map(|(_, _, request)| request).collect();

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
            runtime.block_on(portal(requests, edges, &ready_tx, stop_rx));
        })
        .map_err(|err| Unavailable::Failed(format!("cannot start the listener thread: {err}")))?;

    let handle = Handle {
        stop: Some(stop_tx),
    };
    match ready_rx.recv_timeout(BIND_TIMEOUT) {
        Ok(Ok(descriptions)) => Ok(Listener::new(
            Backend::WaylandPortal,
            descriptions,
            unavailable,
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
    requests: Vec<Request>,
    edges: UnboundedSender<(ActionId, Edge)>,
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
    // Still the portal's own availability, not the bindings': a compositor that
    // cannot open a session cannot bind anything either.
    let Ok(session) = shortcuts
        .create_session(CreateSessionOptions::default())
        .await
    else {
        let _ = ready.send(Err(no_portal()));
        return;
    };

    match bind(&shortcuts, &session, requests, edges, ready, stop).await {
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
    requests: Vec<Request>,
    edges: UnboundedSender<(ActionId, Edge)>,
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

    let wanted: Vec<NewShortcut> = requests
        .iter()
        .map(|request| {
            NewShortcut::new(request.id.as_str(), request.description.as_str())
                .preferred_trigger(request.trigger.as_deref())
        })
        .collect();
    let reply = shortcuts
        .bind_shortcuts(session, &wanted, None, BindShortcutsOptions::default())
        .await
        .and_then(|request| request.response())
        .map_err(|_| refused())?;

    let missing: Vec<&str> = requests
        .iter()
        .filter(|request| {
            !reply
                .shortcuts()
                .iter()
                .any(|shortcut| shortcut.id() == request.id)
        })
        .map(|request| request.id.as_str())
        .collect();
    if !missing.is_empty() {
        return Err(Unavailable::Failed(format!(
            "the compositor refused a shortcut ({})",
            missing.join(", ")
        )));
    }

    let descriptions: Vec<(ActionId, String)> = requests
        .iter()
        .filter_map(|request| {
            let bound = reply
                .shortcuts()
                .iter()
                .find(|shortcut| shortcut.id() == request.id)?;
            Some((request.action, bound.trigger_description().to_string()))
        })
        .collect();
    if ready.send(Ok(descriptions)).is_err() {
        return Ok(());
    }

    // The compositor, not this crate, decides when a shortcut is active, so
    // every action is routed without modifiers of its own.
    let mut router = Router::new(edges);
    for request in &requests {
        router.push(request.action, Mods::default(), request.id.clone());
    }

    futures::pin_mut!(activated, deactivated, stop);
    loop {
        tokio::select! {
            Some(signal) = activated.next() => {
                router.trigger(Edge::Pressed, |bound| bound.as_str() == signal.shortcut_id());
            }
            Some(signal) = deactivated.next() => {
                router.trigger(Edge::Released, |bound| bound.as_str() == signal.shortcut_id());
            }
            _ = &mut stop => break,
            else => break,
        }
        if router.closed() {
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
