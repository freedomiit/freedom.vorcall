//! The applications playing sound, and the wiring that narrows a share's audio
//! to one of them.
//!
//! The portal never says which application owns the window it handed over, so
//! the user names one. A playback node's registry global says little about who
//! is behind it: `application.name` and the `client.id` it came through. Only
//! that client's own info, which takes a bind to read, says which process and
//! binary it is, whichever way the stream reached PipeWire — pipewire-pulse,
//! the ALSA plugin, or PipeWire itself. So every client is bound and every
//! playback node is judged through its client. A share follows every node
//! judged to be the chosen application: its ports are linked into the share's
//! own capture node as they appear, and the links go with them.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use libspa::utils::dict::DictRef;
use pipewire::client::{Client, ClientListener};
use pipewire::context::ContextRc;
use pipewire::core::{CoreRc, PW_ID_CORE};
use pipewire::link::Link;
use pipewire::loop_::Timeout;
use pipewire::main_loop::MainLoopRc;
use pipewire::properties::properties;
use pipewire::proxy::{ProxyListener, ProxyT};
use pipewire::registry::{GlobalObject, Listener, Registry, RegistryRc};
use pipewire::types::ObjectType;

use crate::{AudioApp, Unavailable};

/// How long the registry walk waits for the playback streams and their clients.
const LIST_BUDGET: Duration = Duration::from_secs(3);
const LIST_STEP: Duration = Duration::from_millis(20);

/// The media class of an application's playback stream.
const PLAYBACK: &str = "Stream/Output/Audio";

const LEFT: &[&str] = &["FL"];
const RIGHT: &[&str] = &["FR"];
const BOTH: &[&str] = &["FL", "FR"];

/// After this many removals of one wire it stays unlinked until it is no longer
/// wanted: a server that refuses a link, or a user who unlinks it, wins.
const MAX_RELINKS: u8 = 3;

/// Every application with a playback stream right now, this process left out.
pub(super) fn list() -> Result<Vec<AudioApp>, Unavailable> {
    pipewire::init();
    let broken =
        |error: pipewire::Error| Unavailable::Failed(format!("cannot reach PipeWire: {error}"));

    let mainloop = MainLoopRc::new(None).map_err(broken)?;
    let context = ContextRc::new(&mainloop, None).map_err(broken)?;
    let core = context.connect_rc(None).map_err(broken)?;
    let registry = core.get_registry_rc().map_err(|error| {
        Unavailable::Failed(format!("cannot read the PipeWire registry: {error}"))
    })?;

    let nodes = Rc::new(RefCell::new(Vec::new()));
    let clients = Rc::new(RefCell::new(HashMap::new()));
    let bound = Rc::new(RefCell::new(Vec::new()));
    let _listener = registry
        .add_listener_local()
        .global({
            let registry = registry.clone();
            let (nodes, clients, bound) = (nodes.clone(), clients.clone(), bound.clone());
            move |global| match global.type_ {
                ObjectType::Client => {
                    let (id, clients) = (global.id, clients.clone());
                    let answer = move |facts: ClientFacts| {
                        clients.borrow_mut().insert(id, facts);
                    };
                    if let Some(client) = bind_client(&registry, global, answer) {
                        bound.borrow_mut().push(client);
                    }
                }
                ObjectType::Node => {
                    let Some(props) = global.props else {
                        return;
                    };
                    if is_playback(props.get("media.class")) {
                        nodes.borrow_mut().push(NodeFacts::from_props(props));
                    }
                }
                _ => {}
            }
        })
        .register();

    // Each sync is answered once everything sent before it has been: the first
    // after the registry's opening burst, during which every client was bound,
    // and the second after the info of every client bound then. Nothing else
    // on this connection asks for one, so every `done` on the core is one of
    // those two answers.
    let answered = Rc::new(Cell::new(0_u32));
    let _core_listener = core
        .add_listener_local()
        .done({
            let answered = answered.clone();
            move |id, _| {
                if id == PW_ID_CORE {
                    answered.set(answered.get() + 1);
                }
            }
        })
        .register();

    let deadline = Instant::now() + LIST_BUDGET;
    for round in 1..=2 {
        core.sync(0).map_err(broken)?;
        while answered.get() < round && Instant::now() < deadline {
            mainloop.loop_().iterate(Timeout::Finite(LIST_STEP));
        }
    }

    // The listeners' closures still hold their own handles on these, and they
    // only ever run inside the loop above.
    let own_pid = std::process::id();
    let known = clients.borrow();
    let found: Vec<AudioApp> = nodes
        .borrow()
        .iter()
        .filter_map(|node| classify(node, node.client.and_then(|id| known.get(&id)), own_pid))
        .collect();
    // The client proxies go first, before the registry listener that made them
    // and everything it hangs off.
    bound.borrow_mut().clear();
    Ok(listed(found))
}

/// What a playback node's registry global says about it.
#[derive(Debug)]
struct NodeFacts {
    /// `client.id`.
    client: Option<u32>,
    /// `application.name`.
    name: Option<String>,
}

impl NodeFacts {
    fn from_props(props: &DictRef) -> NodeFacts {
        let client = props.get("client.id");
        let name = props.get("application.name");
        NodeFacts {
            client: client.and_then(|client| client.parse().ok()),
            name: name.map(str::to_string),
        }
    }
}

/// What a bound client's info says about the process behind it.
#[derive(Debug, PartialEq, Eq)]
struct ClientFacts {
    pid: Option<u32>,
    name: Option<String>,
    binary: Option<String>,
}

impl ClientFacts {
    fn from_props(props: &DictRef) -> ClientFacts {
        let pid = props.get("application.process.id");
        let name = props.get("application.name");
        let binary = props.get("application.process.binary");
        ClientFacts {
            pid: pid.and_then(|pid| pid.parse().ok()),
            name: name.map(str::to_string),
            binary: binary.map(str::to_string),
        }
    }
}

fn is_playback(media_class: Option<&str>) -> bool {
    media_class == Some(PLAYBACK)
}

/// An empty property is no answer at all.
fn present(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

/// The application a name and a binary make, if they make one.
fn identity(name: Option<&str>, binary: Option<&str>) -> Option<AudioApp> {
    let (name, binary) = (present(name), present(binary));
    if name.is_none() && binary.is_none() {
        return None;
    }
    Some(AudioApp {
        name: name.map(str::to_string),
        binary: binary.map(str::to_string),
    })
}

/// Which application a playback node belongs to, judged through its client.
/// Nothing is decided before the client's info has arrived. A client whose
/// info names no process is not this one: libpipewire names the process on
/// every connection this process opens.
fn classify(node: &NodeFacts, client: Option<&ClientFacts>, own_pid: u32) -> Option<AudioApp> {
    let client = client?;
    if client.pid == Some(own_pid) {
        return None;
    }
    let name = match present(node.name.as_deref()) {
        Some(name) => Some(name),
        None => present(client.name.as_deref()),
    };
    identity(name, client.binary.as_deref())
}

/// Which of the share's two channels a source channel feeds. The low-frequency
/// channel feeds neither, and one with no side of its own — centre, mono, an
/// auxiliary, or no position at all — feeds both.
fn route(channel: Option<&str>) -> &'static [&'static str] {
    match channel {
        Some("FL" | "SL" | "RL" | "FLC" | "FLW" | "FLH" | "TFL" | "TSL" | "TRL" | "BLC") => LEFT,
        Some("FR" | "SR" | "RR" | "FRC" | "FRW" | "FRH" | "TFR" | "TSR" | "TRR" | "BRC") => RIGHT,
        Some("LFE") => &[],
        _ => BOTH,
    }
}

/// One entry per application, in the order the user reads them.
fn listed(found: impl IntoIterator<Item = AudioApp>) -> Vec<AudioApp> {
    let mut apps: Vec<AudioApp> = found.into_iter().collect();
    apps.sort_by_cached_key(|app| (app.label().to_lowercase(), app.clone()));
    apps.dedup();
    apps
}

/// One link between a source's output port and one of the share's input ports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Wire {
    out_node: u32,
    out_port: u32,
    in_node: u32,
    in_port: u32,
}

#[derive(Clone, Debug)]
struct PortInfo {
    node: u32,
    output: bool,
    channel: Option<String>,
}

impl PortInfo {
    /// A port as the registry announces it, unless it is a monitor or says too
    /// little to be wired.
    fn from_props(props: &DictRef) -> Option<PortInfo> {
        if props.get("port.monitor") == Some("true") {
            return None;
        }
        let node = props.get("node.id")?.parse().ok()?;
        let output = match props.get("port.direction")? {
            "out" => true,
            "in" => false,
            _ => return None,
        };
        Some(PortInfo {
            node,
            output,
            channel: props.get("audio.channel").map(str::to_string),
        })
    }
}

/// Every link the graph should have right now: each output port of a source
/// into whichever of our own input ports its channel routes to.
fn desired(
    own_node: Option<u32>,
    sources: &HashSet<u32>,
    ports: &HashMap<u32, PortInfo>,
) -> BTreeSet<Wire> {
    let Some(own_node) = own_node else {
        return BTreeSet::new();
    };
    let inputs: Vec<(u32, &str)> = ports
        .iter()
        .filter(|(_, port)| !port.output && port.node == own_node)
        .filter_map(|(&id, port)| Some((id, port.channel.as_deref()?)))
        .collect();

    let mut wires = BTreeSet::new();
    for (&out_port, port) in ports {
        if !port.output || !sources.contains(&port.node) {
            continue;
        }
        let routed = route(port.channel.as_deref());
        for &(in_port, channel) in &inputs {
            if routed.contains(&channel) {
                wires.insert(Wire {
                    out_node: port.node,
                    out_port,
                    in_node: own_node,
                    in_port,
                });
            }
        }
    }
    wires
}

/// Whether the server has taken a wire away fewer than [`MAX_RELINKS`] times.
fn may_link(relinks: &HashMap<Wire, u8>, wire: &Wire) -> bool {
    relinks.get(wire).copied().unwrap_or(0) < MAX_RELINKS
}

/// Forgets the removals of every wire no longer wanted, so one that comes back
/// later starts afresh.
fn forget_unwanted(relinks: &mut HashMap<Wire, u8>, want: &BTreeSet<Wire>) {
    relinks.retain(|wire, _| want.contains(wire));
}

/// A bound client and the listener on its info. Both have to stay alive, and
/// the listener goes first: it unhooks from the proxy's list while the proxy
/// still exists, and only then does the proxy go.
struct Bound {
    _listener: ClientListener,
    _client: Client,
}

/// Binds a client global and calls `answer` with what its info says, each time
/// it says it, for as long as the returned pair lives.
fn bind_client(
    registry: &Registry,
    global: &GlobalObject<&DictRef>,
    answer: impl Fn(ClientFacts) + 'static,
) -> Option<Bound> {
    let client = match registry.bind::<Client, _>(global) {
        Ok(client) => client,
        Err(error) => {
            tracing::debug!(%error, "cannot bind a PipeWire client");
            return None;
        }
    };
    let listener = client
        .add_listener_local()
        .info(move |info| {
            if let Some(props) = info.props() {
                answer(ClientFacts::from_props(props));
            }
        })
        .register();
    Some(Bound {
        _listener: listener,
        _client: client,
    })
}

/// Feeds `linker` from `registry` for as long as the returned listener lives.
pub(super) fn follow(registry: &RegistryRc, linker: &Rc<RefCell<Linker>>) -> Listener {
    // PipeWire runs these, like a link's `removed`, only from inside the loop's
    // `iterate`, never from a call made here or by the linker (creating or
    // dropping a proxy included), so no borrow of the linker is taken twice.
    registry
        .add_listener_local()
        .global({
            let registry = registry.clone();
            let linker = linker.clone();
            move |global| {
                if global.type_ != ObjectType::Client {
                    linker.borrow_mut().global(global);
                    return;
                }
                // Weak, or the linker would own the very closure that owns it.
                let (id, weak) = (global.id, Rc::downgrade(&linker));
                let answer = move |facts: ClientFacts| {
                    if let Some(linker) = weak.upgrade() {
                        linker.borrow_mut().client_info(id, facts);
                    }
                };
                if let Some(client) = bind_client(&registry, global, answer) {
                    linker.borrow_mut().watch(id, client);
                }
            }
        })
        .global_remove({
            let linker = linker.clone();
            move |id| linker.borrow_mut().remove(id)
        })
        .register()
}

/// A link this linker created, and the watch on it. The listener goes first,
/// as a bound client's does.
struct Linked {
    _listener: ProxyListener,
    _link: Link,
}

/// Keeps one application's playback streams linked into the share's capture
/// node for as long as it lives, fed by [`follow`] from the registry of the
/// tap's own connection. Born, fed and dropped on the capture thread.
pub(super) struct Linker {
    // All three hold proxies, which must not outlive their connection, so all
    // three drop before `core`.
    bound: HashMap<u32, Bound>,
    links: HashMap<Wire, Linked>,
    /// Links the server took away, kept until none of their callbacks can be
    /// running: a link is lost from inside its own callback, so the lot is
    /// dropped at the next registry or client-info event instead.
    lost: Vec<Linked>,
    core: CoreRc,
    /// This linker itself, for a link's callback to reach it by.
    me: Weak<RefCell<Linker>>,
    /// How many times the server has taken each wanted wire away.
    relinks: HashMap<Wire, u8>,
    app: AudioApp,
    /// The capture stream's `node.name`, which is how its node is recognised.
    own_name: String,
    own_pid: u32,
    own_node: Option<u32>,
    /// Every playback node, whoever it turns out to belong to.
    playback: HashMap<u32, NodeFacts>,
    clients: HashMap<u32, ClientFacts>,
    ports: HashMap<u32, PortInfo>,
}

impl Linker {
    pub(super) fn new(core: CoreRc, app: AudioApp, own_name: String) -> Rc<RefCell<Linker>> {
        Rc::new_cyclic(|me| {
            RefCell::new(Linker {
                bound: HashMap::new(),
                links: HashMap::new(),
                lost: Vec::new(),
                core,
                me: me.clone(),
                relinks: HashMap::new(),
                app,
                own_name,
                own_pid: std::process::id(),
                own_node: None,
                playback: HashMap::new(),
                clients: HashMap::new(),
                ports: HashMap::new(),
            })
        })
    }

    /// A node or port the registry has announced, at the start or since.
    fn global(&mut self, global: &GlobalObject<&DictRef>) {
        self.lost.clear();
        let Some(props) = global.props else {
            return;
        };
        match global.type_ {
            ObjectType::Node => {
                if props.get("node.name") == Some(self.own_name.as_str()) {
                    self.own_node = Some(global.id);
                } else if is_playback(props.get("media.class")) {
                    self.playback
                        .insert(global.id, NodeFacts::from_props(props));
                }
            }
            ObjectType::Port => {
                let Some(port) = PortInfo::from_props(props) else {
                    return;
                };
                self.ports.insert(global.id, port);
            }
            _ => return,
        }
        self.reconcile();
    }

    /// What a bound client's info says, the first time or since.
    fn client_info(&mut self, id: u32, facts: ClientFacts) {
        self.lost.clear();
        self.clients.insert(id, facts);
        self.reconcile();
    }

    fn watch(&mut self, id: u32, client: Bound) {
        self.bound.insert(id, client);
    }

    /// A global the registry says is gone, whatever it was.
    fn remove(&mut self, id: u32) {
        self.lost.clear();
        self.playback.remove(&id);
        self.clients.remove(&id);
        self.bound.remove(&id);
        self.ports.remove(&id);
        if self.own_node == Some(id) {
            self.own_node = None;
        }
        self.reconcile();
    }

    /// The playback nodes that belong to the application being shared.
    fn sources(&self) -> HashSet<u32> {
        self.playback
            .iter()
            .filter(|(_, node)| {
                let client = node.client.and_then(|id| self.clients.get(&id));
                classify(node, client, self.own_pid).as_ref() == Some(&self.app)
            })
            .map(|(&id, _)| id)
            .collect()
    }

    /// Brings the links in line with [`desired`]. A link that could not be
    /// created is simply missing, and the next change tries it again.
    fn reconcile(&mut self) {
        let want = desired(self.own_node, &self.sources(), &self.ports);
        let before = self.links.len();
        // Dropping a link's proxy is what removes it: none of them linger.
        self.links.retain(|wire, _| want.contains(wire));
        forget_unwanted(&mut self.relinks, &want);
        let mut changed = self.links.len() != before;

        for wire in want {
            if self.links.contains_key(&wire) || !may_link(&self.relinks, &wire) {
                continue;
            }
            let created = self.core.create_object::<Link>(
                "link-factory",
                &properties! {
                    "link.output.node" => wire.out_node.to_string(),
                    "link.output.port" => wire.out_port.to_string(),
                    "link.input.node" => wire.in_node.to_string(),
                    "link.input.port" => wire.in_port.to_string(),
                    "object.linger" => "false",
                },
            );
            match created {
                Ok(link) => {
                    // `removed` alone: only the server sends it, through
                    // `iterate`, while `destroy` comes from tearing the proxy
                    // down, which can happen with the linker borrowed.
                    let listener = link
                        .upcast_ref()
                        .add_listener_local()
                        .removed({
                            let me = self.me.clone();
                            move || {
                                if let Some(linker) = me.upgrade() {
                                    linker.borrow_mut().lost_link(wire);
                                }
                            }
                        })
                        .register();
                    self.links.insert(
                        wire,
                        Linked {
                            _listener: listener,
                            _link: link,
                        },
                    );
                    changed = true;
                }
                Err(error) => tracing::debug!(%error, "cannot link the application's audio"),
            }
        }

        if changed {
            tracing::debug!(
                app = self.app.label(),
                wires = self.links.len(),
                "rewired the shared application's audio"
            );
        }
    }

    /// A link of ours the server took away, whether it refused the link after
    /// creating it or somebody unlinked it by hand.
    fn lost_link(&mut self, wire: Wire) {
        let Some(linked) = self.links.remove(&wire) else {
            return;
        };
        // Its listener is the very closure running this.
        self.lost.push(linked);
        let removals = self.relinks.entry(wire).or_default();
        *removals = removals.saturating_add(1);
        tracing::debug!(
            app = self.app.label(),
            "the server removed a link of the shared application's audio"
        );
        self.reconcile();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Our own capture node.
    const OWN: u32 = 50;
    const OWN_PID: u32 = 4242;

    fn app(name: Option<&str>, binary: Option<&str>) -> AudioApp {
        AudioApp {
            name: name.map(str::to_string),
            binary: binary.map(str::to_string),
        }
    }

    fn node(client: Option<u32>, name: Option<&str>) -> NodeFacts {
        NodeFacts {
            client,
            name: name.map(str::to_string),
        }
    }

    fn client(pid: Option<u32>, name: Option<&str>, binary: Option<&str>) -> ClientFacts {
        ClientFacts {
            pid,
            name: name.map(str::to_string),
            binary: binary.map(str::to_string),
        }
    }

    fn port(node: u32, output: bool, channel: Option<&str>) -> PortInfo {
        PortInfo {
            node,
            output,
            channel: channel.map(str::to_string),
        }
    }

    fn wire(out_node: u32, out_port: u32, in_node: u32, in_port: u32) -> Wire {
        Wire {
            out_node,
            out_port,
            in_node,
            in_port,
        }
    }

    /// Our own node's two input ports, positioned as the app tap's format asks.
    fn own_inputs() -> [(u32, PortInfo); 2] {
        [
            (51, port(OWN, false, Some("FL"))),
            (52, port(OWN, false, Some("FR"))),
        ]
    }

    #[test]
    fn a_stream_with_no_identity_names_no_application() {
        assert_eq!(identity(None, None), None);
        assert_eq!(identity(Some(""), Some("")), None);
        assert_eq!(identity(Some(""), None), None);
    }

    #[test]
    fn an_identity_keeps_whichever_half_it_has() {
        let named = Some(app(Some("Firefox"), None));
        let binary = Some(app(None, Some("mpv")));
        let both = Some(app(Some("Firefox"), Some("firefox")));

        assert_eq!(identity(Some("Firefox"), None), named);
        assert_eq!(identity(None, Some("mpv")), binary);
        assert_eq!(identity(Some(""), Some("mpv")), binary);
        assert_eq!(identity(Some("Firefox"), Some("firefox")), both);
    }

    #[test]
    fn only_a_playback_stream_is_followed() {
        assert!(is_playback(Some("Stream/Output/Audio")));
        assert!(!is_playback(Some("Stream/Input/Audio")));
        assert!(!is_playback(Some("Audio/Sink")));
        assert!(!is_playback(None));
    }

    #[test]
    fn a_node_is_not_judged_before_its_client_has_answered() {
        let stream = node(Some(7), Some("Firefox"));

        assert_eq!(classify(&stream, None, OWN_PID), None);
    }

    #[test]
    fn this_process_s_own_streams_belong_to_no_application() {
        let voice = node(Some(7), Some("PipeWire ALSA [vorcall]"));
        let own = client(Some(OWN_PID), Some("vorcall"), Some("vorcall"));

        assert_eq!(classify(&voice, Some(&own), OWN_PID), None);
    }

    #[test]
    fn a_client_that_names_no_process_is_someone_else_s() {
        let stream = node(Some(7), Some("pw-play"));
        let unnamed = client(None, None, Some("pw-cat"));
        let expected = Some(app(Some("pw-play"), Some("pw-cat")));

        assert_eq!(classify(&stream, Some(&unnamed), OWN_PID), expected);
    }

    #[test]
    fn the_node_s_name_comes_before_its_client_s() {
        let stream = node(Some(7), Some("Firefox"));
        let pulse = client(Some(1000), Some("Firefox Nightly"), Some("firefox"));
        let expected = Some(app(Some("Firefox"), Some("firefox")));

        assert_eq!(classify(&stream, Some(&pulse), OWN_PID), expected);
    }

    #[test]
    fn the_client_s_name_stands_in_for_a_node_with_none() {
        let nameless = node(Some(7), None);
        let blank = node(Some(7), Some(""));
        let player = client(Some(1000), Some("mpv"), Some("mpv"));
        let expected = Some(app(Some("mpv"), Some("mpv")));

        assert_eq!(classify(&nameless, Some(&player), OWN_PID), expected);
        assert_eq!(classify(&blank, Some(&player), OWN_PID), expected);
    }

    #[test]
    fn the_binary_is_always_the_client_s() {
        let stream = node(Some(7), Some("PipeWire ALSA [aplay]"));
        let with = client(Some(1000), None, Some("aplay"));
        let without = client(Some(1000), None, None);
        let both = Some(app(Some("PipeWire ALSA [aplay]"), Some("aplay")));
        let named = Some(app(Some("PipeWire ALSA [aplay]"), None));

        assert_eq!(classify(&stream, Some(&with), OWN_PID), both);
        assert_eq!(classify(&stream, Some(&without), OWN_PID), named);
    }

    #[test]
    fn a_stream_nobody_names_belongs_to_no_application() {
        let stream = node(Some(7), None);
        let unnamed = client(Some(1000), Some(""), None);

        assert_eq!(classify(&stream, Some(&unnamed), OWN_PID), None);
    }

    #[test]
    fn a_client_s_info_is_read_off_its_properties() {
        let props = properties! {
            "application.process.id" => "1234",
            "application.name" => "Firefox",
            "application.process.binary" => "firefox",
        };
        let expected = client(Some(1234), Some("Firefox"), Some("firefox"));
        assert_eq!(ClientFacts::from_props(props.dict()), expected);

        let props = properties! { "application.process.id" => "not a pid" };
        assert_eq!(ClientFacts::from_props(props.dict()).pid, None);
    }

    #[test]
    fn each_side_feeds_its_own_channel_and_the_rest_feeds_both() {
        assert_eq!(route(Some("FL")), ["FL"]);
        assert_eq!(route(Some("RL")), ["FL"]);
        assert_eq!(route(Some("FR")), ["FR"]);
        assert_eq!(route(Some("SR")), ["FR"]);
        assert!(route(Some("LFE")).is_empty());
        for centred in [Some("MONO"), Some("FC"), None, Some("AUX0")] {
            assert_eq!(route(centred), ["FL", "FR"], "{centred:?}");
        }
    }

    #[test]
    fn the_list_has_one_entry_per_application_sorted_by_label() {
        let apps = listed([
            app(Some("mpv"), Some("mpv")),
            app(Some("Firefox"), Some("firefox")),
            app(None, Some("audacious")),
            app(Some("Firefox"), Some("firefox")),
            app(Some("Zoom"), None),
        ]);

        assert_eq!(
            apps,
            vec![
                app(None, Some("audacious")),
                app(Some("Firefox"), Some("firefox")),
                app(Some("mpv"), Some("mpv")),
                app(Some("Zoom"), None),
            ]
        );
    }

    #[test]
    fn nothing_is_wired_before_our_own_node_appears() {
        let sources = HashSet::from([10]);
        let mut ports = HashMap::from(own_inputs());
        ports.insert(11, port(10, true, Some("FL")));

        assert!(desired(None, &sources, &ports).is_empty());
    }

    #[test]
    fn a_stereo_source_is_wired_side_to_side() {
        let sources = HashSet::from([10]);
        let mut ports = HashMap::from(own_inputs());
        ports.insert(11, port(10, true, Some("FL")));
        ports.insert(12, port(10, true, Some("FR")));

        assert_eq!(
            desired(Some(OWN), &sources, &ports),
            BTreeSet::from([wire(10, 11, OWN, 51), wire(10, 12, OWN, 52)])
        );
    }

    #[test]
    fn a_mono_source_is_wired_to_both_sides() {
        let sources = HashSet::from([10]);
        let mut ports = HashMap::from(own_inputs());
        ports.insert(11, port(10, true, Some("MONO")));

        assert_eq!(
            desired(Some(OWN), &sources, &ports),
            BTreeSet::from([wire(10, 11, OWN, 51), wire(10, 11, OWN, 52)])
        );
    }

    #[test]
    fn other_nodes_and_a_source_s_inputs_are_left_alone() {
        let sources = HashSet::from([10]);
        let mut ports = HashMap::from(own_inputs());
        ports.insert(11, port(10, true, Some("FL")));
        // A playback stream of some other application.
        ports.insert(21, port(20, true, Some("FL")));
        // An input port on the source itself.
        ports.insert(13, port(10, false, Some("FR")));

        assert_eq!(
            desired(Some(OWN), &sources, &ports),
            BTreeSet::from([wire(10, 11, OWN, 51)])
        );
    }

    #[test]
    fn two_streams_of_one_application_are_both_wired() {
        let sources = HashSet::from([10, 30]);
        let mut ports = HashMap::from(own_inputs());
        ports.insert(11, port(10, true, Some("FL")));
        ports.insert(12, port(10, true, Some("FR")));
        ports.insert(31, port(30, true, Some("FL")));
        ports.insert(32, port(30, true, Some("FR")));

        assert_eq!(
            desired(Some(OWN), &sources, &ports),
            BTreeSet::from([
                wire(10, 11, OWN, 51),
                wire(10, 12, OWN, 52),
                wire(30, 31, OWN, 51),
                wire(30, 32, OWN, 52),
            ])
        );
    }

    #[test]
    fn a_wire_the_server_keeps_removing_is_given_up_on_at_the_third_time() {
        let lost = wire(10, 11, OWN, 51);
        let mut relinks = HashMap::new();
        assert!(may_link(&relinks, &lost));

        relinks.insert(lost, 2);
        assert!(may_link(&relinks, &lost));

        relinks.insert(lost, 3);
        assert!(!may_link(&relinks, &lost));
    }

    #[test]
    fn a_wire_no_longer_wanted_forgets_its_removals() {
        let kept = wire(10, 11, OWN, 51);
        let gone = wire(10, 12, OWN, 52);
        let mut relinks = HashMap::from([(kept, 2), (gone, 3)]);

        forget_unwanted(&mut relinks, &BTreeSet::from([kept]));

        assert_eq!(relinks, HashMap::from([(kept, 2)]));
    }
}
