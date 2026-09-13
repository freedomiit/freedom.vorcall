//! Wayland backend: the seat's own selection, on the application's connection.
//!
//! On Wayland only the client holding keyboard focus may read the selection.
//! `data-control`, the protocol that exists to bypass exactly that, is not
//! implemented by mutter — GNOME 50 advertises neither `ext-data-control-v1`
//! nor `wlr-data-control-unstable-v1` — and any reader outside the application
//! has to map a surface and take focus away to get one, which is what makes
//! `wl-paste` block when nothing hands it focus.
//!
//! The window the user pressed the paste shortcut in already *is* the focused
//! client, so this backend borrows its `wl_display`, opens an event queue and a
//! `wl_data_device` of its own on that connection, and reads what the
//! compositor hands that device. It is the route smithay-clipboard takes for
//! iced's text paste, which is why it is known to work here.
//!
//! The display is borrowed and never owned: `Backend::from_foreign_display`
//! records `owns_display: false`, so dropping this never disconnects the
//! application.

use std::collections::HashMap;
use std::ffi::c_void;
use std::io::Read;
use std::os::fd::AsFd;
use std::sync::Mutex;

use wayland_client::backend::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_data_device::{self, WlDataDevice};
use wayland_client::protocol::wl_data_device_manager::WlDataDeviceManager;
use wayland_client::protocol::wl_data_offer::{self, WlDataOffer};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, event_created_child};

use crate::{Flavour, Pasted, Unavailable, uri};

/// Ceiling on one flavour's transfer. The source writes into a pipe and can
/// keep writing for as long as it likes, so an unbounded read would hang the
/// paste on a misbehaving application.
const MAX_BYTES: usize = 64 * 1024 * 1024;

const URI_LIST: &str = "text/uri-list";
const GNOME_COPIED_FILES: &str = "x-special/gnome-copied-files";
const PNG: &str = "image/png";
/// The text flavours a source may offer, best first. `text/plain` without a
/// charset is whatever the source felt like, so it comes after the one that
/// promises UTF-8; the two X11 atom names are what an XWayland source offers.
const TEXT: [&str; 4] = [
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
];

pub(crate) struct Wayland {
    /// The queue and the state it dispatches into never move apart, and one
    /// read at a time is all a paste ever needs.
    session: Mutex<Session>,
}

struct Session {
    queue: EventQueue<State>,
    state: State,
}

struct State {
    /// Held for the life of the backend: a data device that goes away stops
    /// being told about the selection.
    _device: WlDataDevice,
    /// The MIME types each live offer has announced so far, by object id — the
    /// `offer` events arrive before the `selection` that adopts them.
    offers: HashMap<ObjectId, Vec<String>>,
    selection: Option<WlDataOffer>,
    /// A drag passing over one of the application's surfaces. This data device
    /// never joins one; the offer is tracked only so it can be destroyed.
    dragged: Option<WlDataOffer>,
}

impl State {
    /// The protocol makes the client responsible for destroying an offer it is
    /// finished with: the previous selection, and a drag that has left.
    fn forget(&mut self, offer: &WlDataOffer) {
        self.offers.remove(&offer.id());
        offer.destroy();
    }
}

impl Wayland {
    /// # Safety
    ///
    /// `display` must be a valid `*mut wl_display` outliving the returned
    /// value.
    pub(crate) unsafe fn new(display: *mut c_void) -> Result<Wayland, Unavailable> {
        // SAFETY: the caller's promise about `display`, passed through from
        // `Clipboard::new_wayland`.
        let backend = unsafe { Backend::from_foreign_display(display.cast()) };
        Wayland::on(Connection::from_backend(backend))
    }

    /// Everything past the display pointer, so a test can hand this a
    /// connection of its own.
    fn on(connection: Connection) -> Result<Wayland, Unavailable> {
        let (globals, queue) = registry_queue_init::<State>(&connection)
            .map_err(|err| Unavailable::Failed(format!("no Wayland registry: {err}")))?;
        let handle = queue.handle();

        // The first seat: a second one would have its own focus and its own
        // selection, which is past what a paste shortcut can tell apart.
        let seat: WlSeat = globals
            .bind(&handle, 1..=1, ())
            .map_err(|err| Unavailable::Unsupported(format!("no seat: {err}")))?;
        let manager: WlDataDeviceManager = globals
            .bind(&handle, 1..=3, ())
            .map_err(|err| Unavailable::Unsupported(format!("no data device manager: {err}")))?;

        let mut session = Session {
            state: State {
                _device: manager.get_data_device(&seat, &handle, ()),
                offers: HashMap::new(),
                selection: None,
                dragged: None,
            },
            queue,
        };
        session.roundtrip()?;

        Ok(Wayland {
            session: Mutex::new(session),
        })
    }

    pub(crate) fn read(&self) -> Result<Pasted, Unavailable> {
        // The state behind a poisoned lock is a cache of what the compositor
        // last said, so a panicking reader costs the next one nothing.
        let mut session = self
            .session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // Drains every selection and focus event queued for this data device
        // since the last read. `wl_display.sync` bounds the roundtrip, so an
        // empty clipboard comes back as one rather than waiting for an event
        // that is never coming.
        session.roundtrip()?;

        let Some(offer) = session.state.selection.clone() else {
            return Ok(Pasted::Nothing);
        };
        let offered = session
            .state
            .offers
            .get(&offer.id())
            .cloned()
            .unwrap_or_default();
        let offers = |mime: &str| offered.iter().any(|offered| offered == mime);

        Ok(crate::pick(|flavour| match flavour {
            Flavour::Files => {
                for mime in [URI_LIST, GNOME_COPIED_FILES] {
                    if !offers(mime) {
                        continue;
                    }
                    let bytes = session.receive(&offer, mime)?;
                    let paths = if mime == URI_LIST {
                        uri::from_uri_list(&bytes)
                    } else {
                        uri::from_gnome_copied_files(&bytes)
                    };
                    if !paths.is_empty() {
                        return Ok(Some(Pasted::Files(paths)));
                    }
                }
                Ok(None)
            }
            Flavour::Png => {
                if !offers(PNG) {
                    return Ok(None);
                }
                Ok(Some(Pasted::Png(session.receive(&offer, PNG)?)))
            }
            // Wayland has no raw-bitmap flavour: an image is offered as one of
            // the `image/*` types, encoded.
            Flavour::Rgba => Ok(None),
            Flavour::Text => {
                for mime in TEXT {
                    if !offers(mime) {
                        continue;
                    }
                    let bytes = session.receive(&offer, mime)?;
                    return Ok(Some(Pasted::Text(
                        String::from_utf8_lossy(&bytes).into_owned(),
                    )));
                }
                Ok(None)
            }
        }))
    }
}

impl Session {
    fn roundtrip(&mut self) -> Result<(), Unavailable> {
        self.queue
            .roundtrip(&mut self.state)
            .map(|_| ())
            .map_err(|err| Unavailable::Failed(format!("cannot reach the compositor: {err}")))
    }

    /// One flavour of the offer, transferred over a pipe the source writes into.
    fn receive(&mut self, offer: &WlDataOffer, mime: &str) -> Result<Vec<u8>, String> {
        let (reader, writer) = std::io::pipe().map_err(|err| err.to_string())?;
        offer.receive(mime.to_string(), writer.as_fd());
        // Our own end has to close before the read, or it never reaches EOF.
        drop(writer);
        // A flush is not enough: the compositor has to have acted on the
        // request before the source is asked for the bytes, or the read comes
        // back empty.
        self.roundtrip().map_err(|err| err.to_string())?;
        drain(reader)
    }
}

fn drain(pipe: impl Read) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    pipe.take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| err.to_string())?;
    if bytes.len() > MAX_BYTES {
        return Err(format!("more than {MAX_BYTES} bytes offered"));
    }
    Ok(bytes)
}

impl Dispatch<WlDataDevice, ()> for State {
    fn event(
        state: &mut State,
        _device: &WlDataDevice,
        event: wl_data_device::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<State>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                state.offers.insert(id.id(), Vec::new());
            }
            wl_data_device::Event::Selection { id } => {
                if let Some(previous) = state.selection.take() {
                    state.forget(&previous);
                }
                state.selection = id;
            }
            wl_data_device::Event::Enter { id, .. } => state.dragged = id,
            wl_data_device::Event::Leave => {
                if let Some(dragged) = state.dragged.take() {
                    state.forget(&dragged);
                }
            }
            // Motion and Drop: this data device never accepts a drag, so the
            // compositor ends the session and sends Leave.
            _ => (),
        }
    }

    event_created_child!(State, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ()),
    ]);
}

impl Dispatch<WlDataOffer, ()> for State {
    fn event(
        state: &mut State,
        offer: &WlDataOffer,
        event: wl_data_offer::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<State>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event
            && let Some(offered) = state.offers.get_mut(&offer.id())
        {
            offered.push(mime_type);
        }
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _state: &mut State,
        _registry: &WlRegistry,
        _event: <WlRegistry as Proxy>::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _handle: &QueueHandle<State>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _state: &mut State,
        _seat: &WlSeat,
        _event: <WlSeat as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<State>,
    ) {
    }
}

impl Dispatch<WlDataDeviceManager, ()> for State {
    fn event(
        _state: &mut State,
        _manager: &WlDataDeviceManager,
        _event: <WlDataDeviceManager as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _handle: &QueueHandle<State>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything past the display pointer, against the compositor that is
    /// actually running: the registry, the seat, the data device and the
    /// roundtrips. The selection itself only reaches a client with keyboard
    /// focus, which a test run has no surface to hold, so this proves the
    /// plumbing and the absence of a protocol error — never the paste. That
    /// last step is the owner running the application.
    #[test]
    #[ignore = "needs a live Wayland session"]
    fn binds_a_data_device_on_the_running_compositor() {
        let connection = Connection::connect_to_env().expect("a Wayland session");
        let wayland = Wayland::on(connection).expect("a seat and a data device");
        assert_eq!(
            wayland.read().expect("the read completes"),
            Pasted::Nothing,
            "an unfocused client is offered no selection"
        );
    }
}
