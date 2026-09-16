//! The decisions the window makes that are worth testing on their own: whether
//! a message counts as unread, what a key press is bound to, which share can be
//! watched, what a failure reads as.
//!
//! Everything here is a pure function of its arguments. The handlers in
//! `app::update` call them; nothing here touches `App`.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use iced::{keyboard, mouse};
use vorcall_core::config::{self, Config, TransmitMode};
use vorcall_core::mentions::{self, Segment};
use vorcall_core::{ApiFailure, Channel, ChannelKind, ChatMessage};
use vorcall_hotkey::{ActionId, Backend, Binding, Key, MouseButton, Trigger};
use vorcall_screen::Capabilities;
use vorcall_screen::preset::{self, FrameRate, Preset, Resolution};

use crate::app::state::chat::MESSAGE_LIMIT;
use crate::app::state::server::{ServerModel, channel_kind};
use crate::app::state::voice::{HotkeyStatus, VoiceUi};

/// Push to talk keeps action 0: `vorcall-hotkey` describes that one to a Wayland
/// compositor as push to talk, and the ids are what every edge is tagged with.
pub const PUSH_TO_TALK: ActionId = 0;
pub const TOGGLE_MUTE: ActionId = 1;
pub const TOGGLE_DEAFEN: ActionId = 2;

/// How many rows the mention list offers before it stops being a shortcut. The
/// two broadcast words count against it like any other row.
pub const SUGGESTIONS: usize = 8;

/// The generation the window's own fallback edges carry. No listener can have it:
/// the first one started is generation 1.
pub const WINDOW_GENERATION: u64 = 0;

/// The three actions observed system-wide: the id an edge arrives under, the
/// configuration's name for the binding, and what a compositor shows the user
/// while it asks about it.
pub const GLOBAL_ACTIONS: [(ActionId, &str, &str); 3] = [
    (PUSH_TO_TALK, config::PUSH_TO_TALK, "Push to talk"),
    (TOGGLE_MUTE, "toggle_mute", "Mute the microphone"),
    (TOGGLE_DEAFEN, "toggle_deafen", "Mute everything"),
];

/// What the settings say when the server's answer makes no sense.
pub const UNEXPECTED: &str = "Unexpected server answer";
/// What [`vorcall_screen::capabilities`] calls a system that cannot capture.
pub const NO_CAPTURE: &str = "none";
/// Whose screen the stage shows while the roster has not caught up.
pub const UNKNOWN_SHARER: &str = "someone";
/// The two picture rules, which attachments no longer share: an avatar, a
/// banner, the server icon and a role icon are still one of four types at 8 MiB,
/// magic-checked by the server.
pub const IMAGE_TOO_LARGE: &str = "Images must be 8 MiB or smaller";
pub const NOT_AN_IMAGE: &str = "Only PNG, JPEG, GIF and WebP images can be used";
/// A file above [`vorcall_core::attachments::MAX_BYTES`] is offered as a
/// streamed file rather than stored, so nothing but a size the protocol cannot
/// name reaches this.
pub const FILE_TOO_LARGE: &str = "That file is too large to send";
/// A streamed file is served by the sender's own client, so it is readable only
/// while they are online.
pub const SENDER_OFFLINE: &str = "The sender is offline";
/// The clipboard held something, but nothing this can send.
pub const NOTHING_TO_PASTE: &str = "Nothing on the clipboard to paste";
/// The platform, the session or a permission is what says no — not the content.
pub const CLIPBOARD_UNAVAILABLE: &str = "Vorcall cannot read this system's clipboard";
/// The pick list offers the system default as an entry; the configuration spells
/// it `None`.
pub const SYSTEM_DEFAULT: &str = "System default";
/// The loudest a watched share can be played, which is the range the stage's
/// slider offers.
pub const SHARE_VOLUME_MAX: f32 = 2.0;
/// What a notification says about a message that carries nothing but files. Any
/// file of any type is an attachment now, so a picture is not what it names.
pub const FILE_ONLY: &str = "[file]";

/// The units a byte count is said in, smallest first. Binary, like every limit
/// the client states: an attachment stops at 2 GiB and a picture at 8 MiB.
const BYTE_UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

const USERNAME_MAX: usize = 32;
const PASSWORD_MIN: usize = 8;
const PASSWORD_MAX: usize = 128;

/// Whether a message that just landed leaves its channel unread. Reading it takes
/// having that channel in view, at the bottom, in a focused window.
pub fn unread_rule(foreign: bool, is_current: bool, at_bottom: bool, focused: bool) -> bool {
    foreign && (!is_current || !at_bottom || !focused)
}

/// Whether a foreign message is worth a notification. Anything addressed to this
/// account interrupts unless it is already on screen; everything else only while
/// the window is away.
pub fn notification_rule(mention_or_dm: bool, focused: bool, viewing: bool) -> bool {
    if mention_or_dm { !viewing } else { !focused }
}

/// Whether one message counts against a channel's stored mention counter.
///
/// This mirrors what the server counts in `ReadState.mentions`, so a live
/// message and the next snapshot never disagree: `@everyone` counts like a direct
/// mention, whatever this account asked notifications to do about it, and `@here`
/// is live-only and is never counted there.
pub fn mention_counts(mention_ids: &[i64], me: i64, mention_everyone: bool) -> bool {
    mentions::mentions_me(mention_ids, me) || mention_everyone
}

/// Whether one message is addressed to this account, which is what makes it
/// worth interrupting for.
///
/// `@here` is in it although [`mention_counts`] leaves it out: it reaches only
/// the members who are online at the time, and this client is one of them.
pub fn addressed_to_me(
    message: &ChatMessage,
    me: i64,
    suppress_everyone: bool,
    is_dm: bool,
) -> bool {
    is_dm
        || mentions::mentions_me(&message.mention_ids, me)
        || ((message.mention_everyone || message.mention_here) && !suppress_everyone)
}

/// The stored text as a person reads it: every `<@id>` token becomes the name it
/// stands for, and the two channel-wide words stay literal. A notification, a
/// clipboard copy and the composer of an edit all carry this form.
pub fn plain_text(text: &str, users: &[(i64, String)]) -> String {
    mentions::segments(text, users)
        .into_iter()
        .map(|segment| match segment {
            Segment::Text(text) => text,
            Segment::Mention { username, .. } => format!("@{username}"),
            Segment::Everyone => mentions::EVERYONE.to_owned(),
            Segment::Here => mentions::HERE.to_owned(),
            // A link is drawn as one, but read and copied as the URL it is.
            Segment::Link(url) => url,
        })
        .collect()
}

/// What a notification carries. A message that is nothing but files still has to
/// say something. `files` counts the attachments and the streamed files
/// together: to a notification they are the same thing.
pub fn notification_body(text: &str, files: usize) -> String {
    if text.trim().is_empty() && files > 0 {
        return FILE_ONLY.to_owned();
    }
    text.to_owned()
}

/// One step through a list that wraps at both ends; `None` when there is nowhere
/// to step. An entry the list does not hold starts from whichever end the step
/// comes from.
pub fn step(order: &[i64], current: Option<i64>, forward: bool) -> Option<i64> {
    let length = order.len();
    if length == 0 {
        return None;
    }
    let at = current.and_then(|id| order.iter().position(|entry| *entry == id));
    let next = match (at, forward) {
        (Some(at), true) => (at + 1) % length,
        (Some(at), false) => (at + length - 1) % length,
        (None, true) => 0,
        (None, false) => length - 1,
    };
    Some(order[next])
}

/// The nearest entry with something unread in it, looking forward or back from
/// the one in view and going round once. The one in view is considered last, so
/// a single unread channel is still an answer rather than nothing.
pub fn next_unread(
    order: &[i64],
    current: Option<i64>,
    unread: &BTreeSet<i64>,
    forward: bool,
) -> Option<i64> {
    let length = order.len();
    if length == 0 {
        return None;
    }
    let at = current
        .and_then(|id| order.iter().position(|entry| *entry == id))
        .unwrap_or(0);
    (1..=length)
        .map(|step| {
            let index = if forward {
                at + step
            } else {
                at + length - step
            };
            order[index % length]
        })
        .find(|id| unread.contains(id))
}

/// Watching takes a live voice session in the channel whose roster is on screen,
/// a peer sharing in it, and somebody other than oneself.
pub fn watch_rule(in_this_voice_channel: bool, sharing: bool, is_me: bool) -> bool {
    in_this_voice_channel && sharing && !is_me
}

/// Whether `user_id`'s share can be watched from the channel `channel_id` names.
pub fn can_watch(voice: &VoiceUi, me: i64, channel_id: i64, user_id: i64) -> bool {
    watch_rule(
        voice.is_live() && voice.channel_id == channel_id,
        voice.sharing(user_id).is_some(),
        user_id == me,
    )
}

/// Whether a screen can be shared from here: a live voice session, nothing of
/// ours already on the wire, and a backend that can capture at all.
pub fn can_share(voice: &VoiceUi, capabilities: &Capabilities) -> bool {
    capabilities.backend != NO_CAPTURE
        && voice.is_live()
        && !voice.share.active
        && !voice.share.starting
}

/// What a new watcher count means for the capture: nobody watching pauses the
/// encoder, and whoever arrives after a pause can only start at a keyframe.
pub fn pause_decision(previous: u32, now: u32) -> (bool, bool) {
    (now == 0, previous == 0 && now > 0)
}

/// What a fresh media session does about a watch intent a reconnect kept.
#[derive(Debug, PartialEq, Eq)]
pub enum WatchResume {
    /// That screen is still being shared: ask for the stream again.
    Request(i64),
    /// The roster is in and it is not: there is nothing to go back to.
    Clear,
    /// No roster for this session yet, so nothing can be judged.
    Pending,
    Nothing,
}

/// A reconnect keeps the watch intent; whether it is worth asking for again is
/// the fresh roster's word, and without one the answer has to wait for it.
pub fn watch_resume(
    intent: Option<i64>,
    sharing: &BTreeMap<i64, bool>,
    roster_seen: bool,
) -> WatchResume {
    let Some(user_id) = intent else {
        return WatchResume::Nothing;
    };
    if !roster_seen {
        return WatchResume::Pending;
    }
    if sharing.contains_key(&user_id) {
        WatchResume::Request(user_id)
    } else {
        WatchResume::Clear
    }
}

/// Everyone sharing in the joined channel, as the stage's picker lists them. This
/// client is never in it: its own screen is not watched here.
pub fn sharer_list(voice: &VoiceUi, me: i64) -> Vec<(i64, String)> {
    let Some(roster) = voice.roster() else {
        return Vec::new();
    };
    roster
        .sharing
        .keys()
        .filter(|user_id| **user_id != me)
        .filter_map(|user_id| {
            let member = roster.members.get(user_id)?;
            Some((*user_id, member.username.clone()))
        })
        .collect()
}

/// Whose screen the stage is showing.
pub fn sharer_name<'a>(voice: &'a VoiceUi, server: &'a ServerModel) -> &'a str {
    let Some(user_id) = voice.watch.state else {
        return UNKNOWN_SHARER;
    };
    if let Some(member) = voice
        .roster()
        .and_then(|roster| roster.members.get(&user_id))
    {
        return member.username.as_str();
    }
    if server.members.contains_key(&user_id) {
        return server.display_name(user_id);
    }
    UNKNOWN_SHARER
}

/// What this machine can share, in the sentence under the settings. `running` is
/// the backend a live share actually got, which is the one worth naming.
pub fn share_sentence(capabilities: &Capabilities, running: Option<&'static str>) -> String {
    if capabilities.backend == NO_CAPTURE {
        return "This system cannot share a screen.".to_owned();
    }

    let mut parts = vec![
        format!("Capture: {}", running.unwrap_or(capabilities.backend)),
        if capabilities.windows {
            "whole screens and single windows".to_owned()
        } else {
            "whole screens only".to_owned()
        },
        if capabilities.audio {
            "shared audio carries what this machine plays, without Vorcall's own voices".to_owned()
        } else {
            "no audio with the share on this system".to_owned()
        },
    ];
    if capabilities.portal_picker {
        parts.push("the system dialog picks the screen or window".to_owned());
    }
    #[cfg(target_os = "macos")]
    parts.push(
        "Screen Recording permission is required, and re-granted after every update".to_owned(),
    );
    parts.join(" · ")
}

/// The preset a share starts with. Anything the configuration cannot name is the
/// default rather than a refusal to share.
pub fn share_preset(config: &Config) -> Preset {
    Preset {
        resolution: config.share_resolution.parse().unwrap_or(Resolution::P720),
        fps: FrameRate::from_hz(config.share_fps).unwrap_or(FrameRate::F30),
        bitrate_kbps: config.share_bitrate_kbps,
    }
}

/// The frame the capture backend is asked to aim for where it can scale for us.
/// A share at the source's own resolution asks for nothing.
pub fn capture_box(resolution: Resolution) -> Option<(u32, u32)> {
    match resolution {
        Resolution::Source => None,
        Resolution::P720 => Some((1280, 720)),
        Resolution::P1080 => Some((1920, 1080)),
        Resolution::P1440 => Some((2560, 1440)),
        Resolution::P2160 => Some((3840, 2160)),
    }
}

/// What the preset would have asked the encoder for on its own, which is where a
/// manual bitrate starts.
pub fn auto_bitrate_kbps(config: &Config) -> u32 {
    let preset = Preset {
        bitrate_kbps: None,
        ..share_preset(config)
    };
    preset.bitrate_kbps(capture_box(preset.resolution).unwrap_or(preset::MAX_SOURCE))
}

/// The `@…` being typed, as byte offsets into `text`: the run of non-whitespace
/// characters ending exactly at the caret, when it opens with an `@` and carries
/// at least one character after it. What follows the caret is not part of it, so
/// a name completed in the middle of a sentence still finds its fragment.
pub fn mention_fragment(text: &str, caret: usize) -> Option<Range<usize>> {
    if caret == 0 || !text.is_char_boundary(caret) {
        return None;
    }
    let start = text[..caret]
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace())
        .map_or(0, |(at, character)| at + character.len_utf8());
    let name = text[start..caret].strip_prefix('@')?;
    (!name.is_empty()).then_some(start..caret)
}

/// One row the mention list offers. The two words that name a whole channel are
/// not members, and carry no id of their own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MentionCandidate {
    Everyone,
    Here,
    Member { user_id: i64, username: String },
}

impl MentionCandidate {
    /// What a pick writes after the `@`. The two broadcast words are spelled
    /// once, in `vorcall_core::mentions`, and carry the `@` there.
    pub fn word(&self) -> &str {
        match self {
            Self::Everyone => mentions::EVERYONE.trim_start_matches('@'),
            Self::Here => mentions::HERE.trim_start_matches('@'),
            Self::Member { username, .. } => username,
        }
    }
}

/// What the mention list offers for what has been typed after the `@`: the two
/// broadcast words first when this member may use them, then everyone whose
/// username starts with the query, by name.
pub fn mention_candidates(
    pairs: &[(i64, String)],
    query: &str,
    can_everyone: bool,
) -> Vec<MentionCandidate> {
    let query = query.to_lowercase();
    let mut candidates: Vec<MentionCandidate> = Vec::new();
    if can_everyone {
        candidates.extend(
            [MentionCandidate::Everyone, MentionCandidate::Here]
                .into_iter()
                .filter(|candidate| candidate.word().starts_with(&query)),
        );
    }

    let mut names: Vec<(i64, &str)> = pairs
        .iter()
        .filter(|(_, name)| name.to_lowercase().starts_with(&query))
        .map(|(user_id, name)| (*user_id, name.as_str()))
        .collect();
    names.sort_by_cached_key(|(_, name)| name.to_lowercase());
    candidates.extend(
        names
            .into_iter()
            .map(|(user_id, username)| MentionCandidate::Member {
                user_id,
                username: username.to_owned(),
            }),
    );

    candidates.truncate(SUGGESTIONS);
    candidates
}

/// What a pick leaves behind and where the caret lands in it: the fragment
/// replaced by `@word `, everything past it untouched. The editor itself is
/// edited in place rather than rebuilt, so this is the mirror the pick is
/// checked against and the shape the tests read.
pub fn mention_completion(text: &str, fragment: Range<usize>, word: &str) -> (String, usize) {
    let inserted = format!("@{word} ");
    let caret = fragment.start + inserted.len();
    let mut completed =
        String::with_capacity(text.len() - (fragment.end - fragment.start) + inserted.len());
    completed.push_str(&text[..fragment.start]);
    completed.push_str(&inserted);
    completed.push_str(&text[fragment.end..]);
    (completed, caret)
}

/// The three modifiers a binding can ask for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Required {
    ctrl: bool,
    shift: bool,
    alt: bool,
}

impl Required {
    /// Whether everything this asks for is held. Extra modifiers never stand in
    /// the way, which is what keeps an unmodified binding firing the way it
    /// always has — the same rule every hotkey backend applies.
    fn held_by(self, modifiers: keyboard::Modifiers) -> bool {
        (!self.ctrl || modifiers.control())
            && (!self.shift || modifiers.shift())
            && (!self.alt || modifiers.alt())
    }
}

/// Splits the modifier prefixes off a stored binding, in the one order the
/// grammar accepts.
fn split_binding(name: &str) -> (Required, &str) {
    let mut rest = name;
    let mut required = Required::default();
    for (prefix, flag) in [
        ("Ctrl+", &mut required.ctrl),
        ("Shift+", &mut required.shift),
        ("Alt+", &mut required.alt),
    ] {
        if let Some(tail) = rest.strip_prefix(prefix) {
            rest = tail;
            *flag = true;
        }
    }
    (required, rest)
}

/// The trigger an in-window key event stands for. A `Named` key is stored under
/// its variant name, which is exactly what [`Binding::parse`] reads; location is
/// deliberately ignored, so either control key holds the same talk.
fn trigger_from_key(key: &keyboard::Key) -> Option<Trigger> {
    let binding = match key {
        keyboard::Key::Named(named) => Binding::parse(&format!("{named:?}"))?,
        keyboard::Key::Character(character) => Binding::parse(character.as_str())?,
        keyboard::Key::Unidentified => return None,
    };
    Some(binding.trigger)
}

/// The binding one key alone stands for, modifiers left out of it.
pub fn binding_from_key(key: &keyboard::Key) -> Option<Binding> {
    trigger_from_key(key).map(Binding::simple)
}

/// The primary and secondary buttons are deliberately absent: binding them would
/// make ordinary clicking transmit.
pub fn binding_from_mouse(button: mouse::Button) -> Option<Binding> {
    let trigger = match button {
        mouse::Button::Back => Trigger::Mouse(MouseButton::Back),
        mouse::Button::Forward => Trigger::Mouse(MouseButton::Forward),
        mouse::Button::Middle => Trigger::Mouse(MouseButton::Middle),
        _ => return None,
    };
    Some(Binding::simple(trigger))
}

/// The binding the keybinds tab stores for a key press: its trigger plus
/// whatever modifiers were held with it. Escape is how the capture is cancelled
/// and can never be a binding.
pub fn capture_binding(key: &keyboard::Key, modifiers: keyboard::Modifiers) -> Option<Binding> {
    if matches!(key, keyboard::Key::Named(keyboard::key::Named::Escape)) {
        return None;
    }
    let trigger = trigger_from_key(key)?;
    let binding = Binding {
        trigger,
        ctrl: modifiers.control(),
        shift: modifiers.shift(),
        alt: modifiers.alt(),
    };
    // A modifier that is the trigger itself is not a chord; the bare key is.
    Binding::parse(&binding.name()).or(Some(Binding::simple(trigger)))
}

/// What the settings page and the hints call a binding. A binding from an older
/// build is spelled the way it was stored.
pub fn key_label(bound: &str) -> String {
    Binding::parse(bound).map_or_else(|| bound.to_owned(), |binding| binding.label())
}

/// Whether an in-window key edge is the bound input.
///
/// A binding stored by an older build can be outside the grammar the backends
/// understand — "Pause", "ç" — and those keep working by name: no listener ever
/// starts on one, so this is the only thing that can match them.
pub fn key_matches(key: &keyboard::Key, modifiers: keyboard::Modifiers, bound: &str) -> bool {
    if let Some(binding) = Binding::parse(bound) {
        let required = Required {
            ctrl: binding.ctrl,
            shift: binding.shift,
            alt: binding.alt,
        };
        return trigger_from_key(key) == Some(binding.trigger) && required.held_by(modifiers);
    }

    let (required, spelling) = split_binding(bound);
    if !required.held_by(modifiers) {
        return false;
    }
    match key {
        keyboard::Key::Named(named) => format!("{named:?}") == spelling,
        keyboard::Key::Character(character) => character.eq_ignore_ascii_case(spelling),
        keyboard::Key::Unidentified => false,
    }
}

/// How many modifiers a stored binding asks for, which is how specific it is. A
/// spelling outside the grammar still has its prefixes read off it.
fn binding_specificity(bound: &str) -> u32 {
    let (required, _) = split_binding(bound);
    u32::from(required.ctrl) + u32::from(required.shift) + u32::from(required.alt)
}

/// Whether a key event holding neither Ctrl nor Alt may fire `bound`.
///
/// A plain letter or digit is somebody typing, so a binding on one waits for a
/// modifier. A named key — `F8`, an arrow, Escape — is never typing, and the
/// three system-wide actions are never held back at all: working while something
/// else has the focus is their whole point, and a bare `F8` push-to-talk would
/// otherwise do nothing whenever the window is the one holding it.
pub fn plain_binding_fires(bound: &str, global: bool) -> bool {
    if global {
        return true;
    }
    match Binding::parse(bound) {
        Some(binding) => !matches!(binding.trigger, Trigger::Key(Key::Char(_))),
        // A spelling outside the grammar is a key name, not a typed character.
        None => true,
    }
}

/// Which bound action a key press belongs to, given every action with the binding
/// in force and whether it is one of the system-wide three.
///
/// Several bindings can match one press, because the modifiers a binding asks for
/// only have to be held and not held alone: `Shift+Alt+ArrowDown` matches
/// `Alt+ArrowDown` too. The most specific of them is what the user meant, which is
/// what lets the unread chords sit on the same arrows as the channel ones. Two
/// actions on the very same binding are a clash the keybinds tab warns about, and
/// the first in `KEYBIND_ACTIONS` order takes the press.
pub fn bound_action<'a, 'b>(
    key: &keyboard::Key,
    modifiers: keyboard::Modifiers,
    actions: impl IntoIterator<Item = (&'a str, &'b str, bool)>,
) -> Option<&'a str> {
    let plain = !modifiers.control() && !modifiers.alt();
    let mut best: Option<(u32, &'a str)> = None;
    for (action, bound, global) in actions {
        if !key_matches(key, modifiers, bound) {
            continue;
        }
        if plain && !plain_binding_fires(bound, global) {
            continue;
        }
        let specificity = binding_specificity(bound);
        if best.is_none_or(|(best, _)| specificity > best) {
            best = Some((specificity, action));
        }
    }
    best.map(|(_, action)| action)
}

/// Whether an in-window mouse edge is the bound input.
pub fn mouse_matches(button: mouse::Button, bound: &str) -> bool {
    match (binding_from_mouse(button), Binding::parse(bound)) {
        (Some(pressed), Some(binding)) => pressed.trigger == binding.trigger,
        _ => false,
    }
}

/// What the settings page says about where push-to-talk edges come from.
pub fn hotkey_sentence(status: &HotkeyStatus, mode: TransmitMode) -> String {
    if mode == TransmitMode::VoiceActivation {
        return "Off (voice activation)".to_owned();
    }
    match status {
        HotkeyStatus::Off => "Global capture starts when you join voice".to_owned(),
        HotkeyStatus::Starting => "Starting global capture…".to_owned(),
        HotkeyStatus::Global {
            backend, trigger, ..
        } => {
            // A listener runs, but not necessarily for this binding: one the
            // backend could not observe is still the window's own.
            if let Some(reason) = status.window_only_reason(PUSH_TO_TALK) {
                return format!("Window only: {reason}");
            }
            let mut sentence = match backend {
                Backend::WindowsHook => "Global (low-level hook)".to_owned(),
                Backend::MacEventTap => "Global (event tap)".to_owned(),
                Backend::X11Raw => "Global (X11)".to_owned(),
                // The compositor is what decided what it bound, and it does not
                // have to be what was asked for.
                Backend::WaylandPortal => "Global via portal".to_owned(),
            };
            if let Some(trigger) = trigger {
                sentence.push_str(": ");
                sentence.push_str(trigger);
            }
            sentence
        }
        HotkeyStatus::WindowOnly(reason) => format!("Window only: {reason}"),
    }
}

/// How far a download has come, for the progress bar. An unknown total reads as
/// nothing done rather than as finished.
pub fn progress_fraction(received: u64, total: u64) -> f32 {
    if total == 0 {
        return 0.0;
    }
    (received as f64 / total as f64).clamp(0.0, 1.0) as f32
}

/// The pick list offers the system default as an entry; the configuration spells
/// it `None`.
pub fn device_choice(name: String) -> Option<String> {
    (name != SYSTEM_DEFAULT).then_some(name)
}

/// A counter the server sends as an `i64`, as the window holds it.
pub fn counter(value: i64) -> u32 {
    value.clamp(0, i64::from(u32::MAX)) as u32
}

/// Keeps one channel's buffer inside [`MESSAGE_LIMIT`], oldest out first.
pub fn trim(messages: &mut BTreeMap<i64, ChatMessage>) {
    while messages.len() > MESSAGE_LIMIT {
        if messages.pop_first().is_none() {
            break;
        }
    }
}

/// Whether one `ChannelUpserted` is the channel the create dialog just asked
/// for. The name is what matches it: a channel arrives as a delta like any
/// other, so being new is no sign at all.
pub fn pending_channel_name(pending: Option<&str>, channel: &Channel) -> bool {
    channel_kind(channel) != ChannelKind::Dm && pending == Some(channel.name.as_str())
}

/// The one place an [`ApiFailure`] becomes something a person can act on.
pub fn describe(failure: &ApiFailure) -> String {
    match failure {
        ApiFailure::AuthChallenge(detail) | ApiFailure::Status(401, detail) => {
            non_empty(detail).unwrap_or_else(|| "Invalid username or password".to_owned())
        }
        ApiFailure::Status(403, detail) => non_empty(detail)
            .unwrap_or_else(|| "Invite code is invalid, used or expired".to_owned()),
        ApiFailure::Status(409, _) => "That username is taken".to_owned(),
        ApiFailure::Throttled(secs) => format!("Too many attempts, try again in {secs}s"),
        ApiFailure::StaleKey => "Unauthorized: rebuild the client with the current key".to_owned(),
        ApiFailure::Transport(_) => "Cannot reach the server".to_owned(),
        ApiFailure::Malformed(_) => UNEXPECTED.to_owned(),
        // The disk, not the server, put an end to it: it carries its own words,
        // and "cannot reach the server" would be a lie about a full volume.
        ApiFailure::Io(detail) => non_empty(detail).unwrap_or_else(|| UNEXPECTED.to_owned()),
        ApiFailure::Status(_, detail) => non_empty(detail).unwrap_or_else(|| UNEXPECTED.to_owned()),
    }
}

/// One byte count as a person reads it: whole bytes below a kibibyte, and at
/// most one decimal place above it, so `2 GiB` is not written `2.0 GiB`.
pub fn format_bytes(bytes: u64) -> String {
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < BYTE_UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        return format!("{bytes} B");
    }

    let mut rounded = (value * 10.0).round() / 10.0;
    // One byte short of a mebibyte rounds to 1024.0 KiB, which is a unit nobody
    // writes: carry it instead.
    if rounded >= 1024.0 && unit + 1 < BYTE_UNITS.len() {
        rounded /= 1024.0;
        unit += 1;
    }

    let suffix = BYTE_UNITS[unit];
    if rounded.fract() == 0.0 {
        format!("{rounded:.0} {suffix}")
    } else {
        format!("{rounded:.1} {suffix}")
    }
}

fn non_empty(detail: &str) -> Option<String> {
    let detail = detail.trim();
    (!detail.is_empty()).then(|| detail.to_owned())
}

pub fn validate_username(username: &str) -> Result<(), String> {
    let length = username.chars().count();
    if length == 0 {
        return Err("Enter a username.".to_owned());
    }
    if length > USERNAME_MAX {
        return Err(format!("At most {USERNAME_MAX} characters."));
    }
    if username.chars().any(char::is_control) {
        return Err("No control characters.".to_owned());
    }
    Ok(())
}

pub fn validate_password(password: &str) -> Result<(), String> {
    let length = password.chars().count();
    if !(PASSWORD_MIN..=PASSWORD_MAX).contains(&length) {
        return Err(format!(
            "Password must be {PASSWORD_MIN} to {PASSWORD_MAX} characters."
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use iced::keyboard::Modifiers;
    use iced::keyboard::key::Named;
    use vorcall_core::VoiceMember;

    use super::*;
    use crate::app::state::voice::VoiceRoster;

    fn named(key: Named) -> keyboard::Key {
        keyboard::Key::Named(key)
    }

    fn typed(character: &str) -> keyboard::Key {
        keyboard::Key::Character(character.into())
    }

    fn voice_member(user_id: i64, username: &str, sharing: bool) -> VoiceMember {
        VoiceMember {
            user_id,
            username: username.to_owned(),
            ssrc: user_id as u32,
            sharing,
            share_audio: false,
            server_muted: false,
            server_deafened: false,
            priority: false,
            self_muted: false,
            self_deafened: false,
        }
    }

    /// Everything but a message sitting on screen, in a focused window, in the
    /// channel being read.
    #[test]
    fn a_foreign_message_is_unread_unless_it_is_being_read() {
        assert!(!unread_rule(true, true, true, true));

        assert!(unread_rule(true, false, true, true));
        assert!(unread_rule(true, true, false, true));
        assert!(unread_rule(true, true, true, false));
    }

    #[test]
    fn my_own_message_is_never_unread() {
        assert!(!unread_rule(false, false, false, false));
        assert!(!unread_rule(false, true, true, true));
    }

    /// A mention and a DM interrupt wherever the window is, unless they land
    /// where they can already be read.
    #[test]
    fn a_mention_notifies_unless_it_is_already_on_screen() {
        assert!(notification_rule(true, false, false));
        assert!(notification_rule(true, true, false));
        assert!(!notification_rule(true, true, true));
    }

    #[test]
    fn an_ordinary_message_notifies_only_an_unfocused_window() {
        assert!(notification_rule(false, false, false));
        assert!(!notification_rule(false, true, false));
        assert!(!notification_rule(false, true, true));
    }

    #[test]
    fn watch_is_offered_only_in_voice_for_a_sharing_peer_that_is_not_me() {
        assert!(watch_rule(true, true, false));
        // Not in that channel's voice session, so there is no stream to ask for.
        assert!(!watch_rule(false, true, false));
        // In voice with them, but they are sharing nothing.
        assert!(!watch_rule(true, false, false));
        // One's own screen is not watched here.
        assert!(!watch_rule(true, true, true));
    }

    #[test]
    fn a_watcher_count_rising_from_zero_forces_a_keyframe() {
        // Nobody watching: the encoder stops, and nothing has to be forced.
        assert_eq!(pause_decision(0, 0), (true, false));
        // The first watcher can only start at a keyframe.
        assert_eq!(pause_decision(0, 1), (false, true));
        // A second one joins a stream that is already running.
        assert_eq!(pause_decision(1, 2), (false, false));
        assert_eq!(pause_decision(2, 0), (true, false));
    }

    #[test]
    fn a_reconnect_re_requests_the_watch_when_the_sharer_is_still_there() {
        let sharing: BTreeMap<i64, bool> = [(9, true)].into_iter().collect();

        assert_eq!(
            watch_resume(Some(9), &sharing, true),
            WatchResume::Request(9)
        );
        // The roster is in and that screen is gone with it.
        assert_eq!(watch_resume(Some(4), &sharing, true), WatchResume::Clear);
        // The media path came up first: nothing can be judged until the roster
        // for this session lands.
        assert_eq!(
            watch_resume(Some(9), &BTreeMap::new(), false),
            WatchResume::Pending
        );
        assert_eq!(watch_resume(None, &sharing, true), WatchResume::Nothing);
    }

    #[test]
    fn the_stage_sharer_list_excludes_me_and_keeps_usernames() {
        let mut voice = VoiceUi {
            channel_id: 10,
            ..VoiceUi::default()
        };
        let mut roster = VoiceRoster::from_members(vec![
            voice_member(7, "me", false),
            voice_member(9, "bea", true),
            voice_member(4, "ana", true),
        ]);
        // 11 shares but is not in the roster, so there is no name to list it
        // under.
        roster.sharing.insert(11, false);
        voice.rosters.insert(10, roster);

        assert_eq!(
            sharer_list(&voice, 7),
            vec![(4, "ana".to_owned()), (9, "bea".to_owned())]
        );
        // Without a roster there is nobody to list.
        voice.channel_id = 0;
        assert!(sharer_list(&voice, 7).is_empty());
    }

    /// The mirror of what the server counts in `ReadState.mentions`, so a live
    /// message and the next snapshot never disagree.
    #[test]
    fn the_mention_badge_counts_what_the_server_counts() {
        assert!(mention_counts(&[7], 7, false));
        assert!(!mention_counts(&[9], 7, false));
        // The server counts `@everyone` like a direct mention.
        assert!(mention_counts(&[], 7, true));
        assert!(mention_counts(&[7], 7, true));
    }

    /// Suppressing the channel-wide words is a notification setting and nothing
    /// more: the badge still counts `@everyone`, because the counter the next
    /// snapshot carries does.
    #[test]
    fn suppressing_everyone_silences_it_without_uncounting_it() {
        let message = ChatMessage {
            mention_everyone: true,
            ..ChatMessage::default()
        };

        assert!(!addressed_to_me(&message, 7, true, false));
        assert!(mention_counts(
            &message.mention_ids,
            7,
            message.mention_everyone
        ));
    }

    /// `@here` reaches only the members online at the time, so it interrupts
    /// without ever bumping the counter the server keeps.
    #[test]
    fn here_interrupts_but_is_never_counted() {
        let mut message = ChatMessage {
            mention_here: true,
            ..ChatMessage::default()
        };

        assert!(!mention_counts(
            &message.mention_ids,
            7,
            message.mention_everyone
        ));
        assert!(addressed_to_me(&message, 7, false, false));
        // Suppressing the channel-wide words covers `@here` as well.
        assert!(!addressed_to_me(&message, 7, true, false));

        message.mention_here = false;
        assert!(!addressed_to_me(&message, 7, false, false));
        // A DM is addressed to this account whatever it says.
        assert!(addressed_to_me(&message, 7, true, true));
    }

    #[test]
    fn a_message_that_names_me_is_addressed_to_me() {
        let named = ChatMessage {
            mention_ids: vec![7],
            ..ChatMessage::default()
        };
        let everyone = ChatMessage {
            mention_everyone: true,
            ..ChatMessage::default()
        };

        assert!(addressed_to_me(&named, 7, true, false));
        assert!(!addressed_to_me(&named, 9, false, false));
        assert!(addressed_to_me(&everyone, 7, false, false));
        assert!(!addressed_to_me(&everyone, 7, true, false));
    }

    #[test]
    fn a_stored_message_reads_as_the_names_it_carries() {
        let users = [(4, "ana".to_owned())];

        assert_eq!(plain_text("hi <@4>", &users), "hi @ana");
        // An id this client knows nothing about must not leak into the text.
        assert_eq!(plain_text("hi <@9>", &users), "hi @unknown");
        assert_eq!(plain_text("@everyone up", &users), "@everyone up");
        assert_eq!(plain_text("@here up", &users), "@here up");
        assert_eq!(plain_text("plain", &users), "plain");
        // A link is a run of its own in the message view; copied, it is the URL.
        assert_eq!(
            plain_text("see https://vorcall.example/x now", &users),
            "see https://vorcall.example/x now"
        );
    }

    #[test]
    fn a_notification_for_files_alone_still_says_something() {
        assert_eq!(notification_body("", 2), FILE_ONLY);
        assert_eq!(notification_body("   ", 1), FILE_ONLY);
        assert_eq!(notification_body("look", 1), "look");
        // Nothing at all is what a tombstone's notification would carry; there is
        // no file to name instead.
        assert_eq!(notification_body("", 0), "");
    }

    /// The expected strings are what a person writes by hand, not what the
    /// function's own arithmetic produces.
    #[test]
    fn a_byte_count_reads_as_a_person_would_write_it() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1), "1 B");
        assert_eq!(format_bytes(999), "999 B");
        // Every unit's own boundary is exact, and an exact value carries no
        // decimal point at all.
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1 MiB");
        assert_eq!(format_bytes(8 * 1024 * 1024), "8 MiB");
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2 GiB");
        assert_eq!(format_bytes(1024 * 1024 * 1024 * 1024), "1 TiB");
        // 1.4 MiB is 1_468_006 bytes; the tenth is what survives the rounding.
        assert_eq!(format_bytes(1_468_006), "1.4 MiB");
        // One byte short of a mebibyte is 1023.999… KiB, which is written as the
        // next unit rather than as "1024 KiB".
        assert_eq!(format_bytes(1024 * 1024 - 1), "1 MiB");
        // Nothing above the largest unit, however big it gets.
        assert!(format_bytes(u64::MAX).ends_with(" TiB"));
    }

    /// A size is a glance, not a measurement: one decimal place at most,
    /// whatever the count.
    #[test]
    fn a_byte_count_never_carries_more_than_one_decimal() {
        for bytes in [
            0,
            1,
            1023,
            1025,
            1_468_006,
            3_333_333,
            1_234_567_890,
            u64::MAX / 3,
            u64::MAX,
        ] {
            let said = format_bytes(bytes);
            let decimals = said
                .split_once('.')
                .map(|(_, rest)| rest.trim_end_matches(char::is_alphabetic).trim().len());
            assert!(
                decimals.is_none_or(|places| places == 1),
                "{bytes} said as {said}"
            );
        }
    }

    /// A disk that filled up is not a server that cannot be reached.
    #[test]
    fn a_local_failure_is_said_in_its_own_words() {
        assert_eq!(
            describe(&ApiFailure::Io("no space left on device".to_owned())),
            "no space left on device"
        );
        assert_eq!(describe(&ApiFailure::Io(String::new())), UNEXPECTED);
    }

    #[test]
    fn stepping_through_the_list_wraps_at_both_ends() {
        let order = [10, 11, 12];

        assert_eq!(step(&order, Some(10), true), Some(11));
        assert_eq!(step(&order, Some(12), true), Some(10));
        assert_eq!(step(&order, Some(10), false), Some(12));
        assert_eq!(step(&order, Some(11), false), Some(10));
        // Nothing in view, or something outside the list, starts at whichever end
        // the step comes from.
        assert_eq!(step(&order, None, true), Some(10));
        assert_eq!(step(&order, Some(99), false), Some(12));
        assert_eq!(step(&[], Some(10), true), None);
    }

    #[test]
    fn the_next_unread_goes_round_once() {
        let order = [10, 11, 12];
        let unread: BTreeSet<i64> = [12].into_iter().collect();

        assert_eq!(next_unread(&order, Some(10), &unread, true), Some(12));
        assert_eq!(next_unread(&order, Some(10), &unread, false), Some(12));
        assert_eq!(next_unread(&order, Some(11), &BTreeSet::new(), true), None);
        assert_eq!(next_unread(&[], None, &unread, true), None);
    }

    /// The one in view is considered last, so a single unread channel is still an
    /// answer rather than nothing.
    #[test]
    fn the_only_unread_channel_is_an_answer_even_when_it_is_the_one_in_view() {
        let order = [10, 11];
        let unread: BTreeSet<i64> = [10].into_iter().collect();

        assert_eq!(next_unread(&order, Some(10), &unread, true), Some(10));
        assert_eq!(next_unread(&order, Some(10), &unread, false), Some(10));
    }

    fn mention_users() -> Vec<(i64, String)> {
        [(1, "Bruno"), (2, "ana"), (3, "Ana Maria"), (4, "anders")]
            .into_iter()
            .map(|(id, name)| (id, name.to_owned()))
            .collect()
    }

    fn words(candidates: &[MentionCandidate]) -> Vec<&str> {
        candidates.iter().map(MentionCandidate::word).collect()
    }

    #[test]
    fn a_mention_query_is_the_unfinished_fragment() {
        assert_eq!(mention_fragment("@a", 2), Some(0..2));
        assert_eq!(mention_fragment("hi @an", 6), Some(3..6));
        // The caret ends the fragment; a name completed mid-sentence still
        // finds it.
        assert_eq!(mention_fragment("hi @an there", 6), Some(3..6));
    }

    #[test]
    fn a_finished_or_empty_fragment_queries_nothing() {
        // Whitespace right before the caret leaves no fragment at all.
        assert_eq!(mention_fragment("hi @an ", 7), None);
        assert_eq!(mention_fragment("@", 1), None);
        assert_eq!(mention_fragment("hi", 2), None);
        assert_eq!(mention_fragment("hi @an there", 12), None);
        // An address is not a mention: the run does not open with the `@`.
        assert_eq!(mention_fragment("mail@x", 6), None);
        assert_eq!(mention_fragment("", 0), None);
        assert_eq!(mention_fragment("@ana", 0), None);
        // Half of a multi-byte character is nowhere.
        assert_eq!(mention_fragment("@joão", 4), None);
    }

    #[test]
    fn a_prefix_matches_whatever_its_case_and_is_offered_by_name() {
        let users = mention_users();

        assert_eq!(
            words(&mention_candidates(&users, "AN", false)),
            ["ana", "Ana Maria", "anders"]
        );
        assert!(mention_candidates(&users, "zz", false).is_empty());
        assert_eq!(mention_candidates(&users, "", false).len(), 4);
    }

    #[test]
    fn the_two_broadcast_words_lead_the_list_for_a_member_who_may_use_them() {
        let users = mention_users();

        assert_eq!(words(&mention_candidates(&users, "e", true)), ["everyone"]);
        assert_eq!(words(&mention_candidates(&users, "h", true)), ["here"]);
        assert_eq!(words(&mention_candidates(&users, "ev", true)), ["everyone"]);
        assert_eq!(
            words(&mention_candidates(&users, "", true)),
            ["everyone", "here", "ana", "Ana Maria", "anders", "Bruno"]
        );
    }

    #[test]
    fn the_two_broadcast_words_are_absent_without_the_permission() {
        let users = mention_users();

        assert!(mention_candidates(&users, "e", false).is_empty());
        assert_eq!(
            words(&mention_candidates(&users, "", false)),
            ["ana", "Ana Maria", "anders", "Bruno"]
        );
    }

    #[test]
    fn the_mention_list_stops_at_eight() {
        let many: Vec<(i64, String)> = (0..12).map(|id| (id, format!("a{id:02}"))).collect();

        assert_eq!(mention_candidates(&many, "a", false).len(), SUGGESTIONS);
        // The two broadcast words take rows of the eight, not rows beyond them.
        let capped = mention_candidates(&many, "", true);
        assert_eq!(capped.len(), SUGGESTIONS);
        assert_eq!(words(&capped)[..2], ["everyone", "here"]);
    }

    #[test]
    fn picking_a_name_replaces_the_fragment() {
        assert_eq!(
            mention_completion("hi @an", 3..6, "ana"),
            ("hi @ana ".to_owned(), 8)
        );
        assert_eq!(
            mention_completion("@a", 0..2, "ana"),
            ("@ana ".to_owned(), 5)
        );
        // What was written past the caret keeps its own spacing, the inserted
        // space included.
        assert_eq!(
            mention_completion("hi @an there", 3..6, "ana"),
            ("hi @ana  there".to_owned(), 8)
        );
    }

    /// The fragment starts after the whitespace, whatever its length in bytes.
    #[test]
    fn picking_a_name_keeps_what_was_written_before_it() {
        assert_eq!(
            mention_completion("olá @jo", 5..8, "joão"),
            ("olá @joão ".to_owned(), 12)
        );
    }

    #[test]
    fn a_named_key_binds_under_its_variant_name() {
        assert_eq!(
            binding_from_key(&named(Named::Control)),
            Some(Binding::simple(Trigger::Key(Key::Control)))
        );
        assert_eq!(
            binding_from_key(&named(Named::F8)),
            Some(Binding::simple(Trigger::Key(Key::F(8))))
        );
    }

    #[test]
    fn a_typed_character_binds_lowercase() {
        assert_eq!(
            binding_from_key(&typed("a")),
            Some(Binding::simple(Trigger::Key(Key::Char('a'))))
        );
        assert_eq!(
            binding_from_key(&typed("A")),
            Some(Binding::simple(Trigger::Key(Key::Char('a'))))
        );
        assert_eq!(
            binding_from_key(&typed("7")),
            Some(Binding::simple(Trigger::Key(Key::Char('7'))))
        );
    }

    /// A key no backend can name is refused rather than stored.
    #[test]
    fn a_key_that_cannot_be_observed_binds_to_nothing() {
        assert_eq!(binding_from_key(&typed("é")), None);
        assert_eq!(binding_from_key(&named(Named::Pause)), None);
        assert_eq!(binding_from_key(&keyboard::Key::Unidentified), None);
    }

    /// Escape cancels the capture and can never become the binding, even though
    /// it is a key the backends can observe.
    #[test]
    fn escape_is_never_captured_as_a_binding() {
        assert!(binding_from_key(&named(Named::Escape)).is_some());
        assert_eq!(
            capture_binding(&named(Named::Escape), Modifiers::empty()),
            None
        );
    }

    #[test]
    fn a_capture_keeps_the_modifiers_that_were_held() {
        assert_eq!(
            capture_binding(&typed("m"), Modifiers::CTRL | Modifiers::SHIFT),
            Binding::parse("Ctrl+Shift+m")
        );
        // A modifier that is the trigger itself is not a chord: the bare key is.
        assert_eq!(
            capture_binding(&named(Named::Control), Modifiers::CTRL),
            Some(Binding::simple(Trigger::Key(Key::Control)))
        );
    }

    #[test]
    fn only_the_three_secondary_mouse_buttons_bind() {
        assert_eq!(
            binding_from_mouse(mouse::Button::Back),
            Some(Binding::simple(Trigger::Mouse(MouseButton::Back)))
        );
        assert_eq!(
            binding_from_mouse(mouse::Button::Middle),
            Some(Binding::simple(Trigger::Mouse(MouseButton::Middle)))
        );
        assert_eq!(binding_from_mouse(mouse::Button::Left), None);
        assert_eq!(binding_from_mouse(mouse::Button::Right), None);
    }

    #[test]
    fn a_key_from_an_older_build_keeps_the_name_it_was_stored_under() {
        assert_eq!(key_label("Control"), "Ctrl");
        assert_eq!(key_label("MouseBack"), "Mouse back");
        assert_eq!(key_label("Ctrl+Shift+m"), "Ctrl+Shift+M");
        assert_eq!(key_label("Pause"), "Pause");
    }

    #[test]
    fn a_bound_key_matches_by_binding() {
        let none = Modifiers::empty();

        assert!(key_matches(&named(Named::Control), none, "Control"));
        assert!(key_matches(&typed("A"), none, "a"));
        assert!(!key_matches(&named(Named::Shift), none, "Control"));
        assert!(!key_matches(&keyboard::Key::Unidentified, none, "Control"));
    }

    /// A chord only fires while its modifiers are held; extra ones never stand in
    /// the way.
    #[test]
    fn a_chord_matches_only_while_its_modifiers_are_held() {
        let ctrl_shift = Modifiers::CTRL | Modifiers::SHIFT;

        assert!(key_matches(&typed("m"), ctrl_shift, "Ctrl+Shift+M"));
        assert!(key_matches(&typed("M"), ctrl_shift, "Ctrl+Shift+m"));
        assert!(key_matches(
            &typed("m"),
            ctrl_shift | Modifiers::ALT,
            "Ctrl+Shift+M"
        ));
        assert!(!key_matches(&typed("m"), Modifiers::CTRL, "Ctrl+Shift+M"));
        assert!(!key_matches(
            &typed("m"),
            Modifiers::empty(),
            "Ctrl+Shift+M"
        ));
        // The unmodified binding still fires with a modifier held, the way push
        // to talk always has.
        assert!(key_matches(&typed("m"), ctrl_shift, "m"));
    }

    /// No backend can observe these, so the window is the only thing that ever
    /// sees them — and it has to keep working for whoever bound one.
    #[test]
    fn a_key_from_an_older_build_still_matches_in_the_window() {
        let none = Modifiers::empty();

        assert!(key_matches(&named(Named::Pause), none, "Pause"));
        assert!(key_matches(&typed("ç"), none, "ç"));
        assert!(!key_matches(&named(Named::ScrollLock), none, "Pause"));
        // Its prefixes are still read, so a stored chord on such a key works.
        assert!(key_matches(&typed("ç"), Modifiers::CTRL, "Ctrl+ç"));
        assert!(!key_matches(&typed("ç"), none, "Ctrl+ç"));
    }

    #[test]
    fn a_bound_mouse_button_matches_only_itself() {
        assert!(mouse_matches(mouse::Button::Back, "MouseBack"));
        assert!(!mouse_matches(mouse::Button::Forward, "MouseBack"));
        assert!(!mouse_matches(mouse::Button::Left, "MouseBack"));
        // A key binding is never a button, whatever the button is.
        assert!(!mouse_matches(mouse::Button::Middle, "Control"));
    }

    #[test]
    fn the_hotkey_sentence_names_the_backend() {
        let global = |backend, trigger: Option<&str>| {
            hotkey_sentence(
                &HotkeyStatus::Global {
                    backend,
                    trigger: trigger.map(str::to_owned),
                    window_only: Vec::new(),
                },
                TransmitMode::PushToTalk,
            )
        };

        assert_eq!(
            global(Backend::WindowsHook, None),
            "Global (low-level hook)"
        );
        assert_eq!(global(Backend::MacEventTap, None), "Global (event tap)");
        assert_eq!(global(Backend::X11Raw, None), "Global (X11)");
        assert_eq!(global(Backend::WaylandPortal, None), "Global via portal");
        assert_eq!(
            global(Backend::WaylandPortal, Some("CTRL")),
            "Global via portal: CTRL"
        );
    }

    #[test]
    fn the_hotkey_sentence_says_where_a_missing_listener_stands() {
        let ptt = |status: &HotkeyStatus| hotkey_sentence(status, TransmitMode::PushToTalk);

        let window_only = HotkeyStatus::WindowOnly("no display".to_owned());
        // A listener that bound the other two actions but not this one.
        let partly_global = HotkeyStatus::Global {
            backend: Backend::WaylandPortal,
            trigger: None,
            window_only: vec![(
                PUSH_TO_TALK,
                "mouse buttons are window-only on Wayland".to_owned(),
            )],
        };

        assert_eq!(
            ptt(&HotkeyStatus::Off),
            "Global capture starts when you join voice"
        );
        assert_eq!(ptt(&HotkeyStatus::Starting), "Starting global capture…");
        assert_eq!(ptt(&window_only), "Window only: no display");
        assert_eq!(
            ptt(&partly_global),
            "Window only: mouse buttons are window-only on Wayland"
        );
    }

    /// A plain letter is somebody typing; a named key and the system-wide three
    /// are not.
    #[test]
    fn the_plain_guard_holds_back_typed_characters_only() {
        assert!(!plain_binding_fires("m", false));
        assert!(!plain_binding_fires("Shift+m", false));
        assert!(!plain_binding_fires("5", false));
        assert!(plain_binding_fires("m", true));
        assert!(plain_binding_fires("F8", false));
        assert!(plain_binding_fires("Control", true));
        assert!(plain_binding_fires("ArrowUp", false));
        assert!(plain_binding_fires("Escape", false));
        // A spelling no backend understands is a key name from an older build.
        assert!(plain_binding_fires("Pause", false));
    }

    /// The press goes to the binding that asked for the most modifiers, which is
    /// what puts the unread chords on the same arrows as the channel ones.
    #[test]
    fn the_most_specific_binding_takes_the_press() {
        let arrows = [
            ("next_channel", "Alt+ArrowDown", false),
            ("next_unread", "Shift+Alt+ArrowDown", false),
            ("edit_last", "ArrowUp", false),
        ];
        let down = named(Named::ArrowDown);

        assert_eq!(
            bound_action(&down, Modifiers::ALT | Modifiers::SHIFT, arrows),
            Some("next_unread")
        );
        assert_eq!(
            bound_action(&down, Modifiers::ALT, arrows),
            Some("next_channel")
        );
        assert_eq!(bound_action(&down, Modifiers::empty(), arrows), None);
        assert_eq!(
            bound_action(&named(Named::ArrowUp), Modifiers::empty(), arrows),
            Some("edit_last")
        );
    }

    /// A bare key is the window's to hold for a system-wide action, and a typed
    /// letter is nobody's.
    #[test]
    fn a_global_action_on_a_bare_key_still_takes_the_press() {
        let actions = [
            ("push_to_talk", "F8", true),
            ("toggle_mute", "m", true),
            ("quick_switcher", "k", false),
        ];

        assert_eq!(
            bound_action(&named(Named::F8), Modifiers::empty(), actions),
            Some("push_to_talk")
        );
        assert_eq!(
            bound_action(&typed("m"), Modifiers::empty(), actions),
            Some("toggle_mute")
        );
        assert_eq!(bound_action(&typed("k"), Modifiers::empty(), actions), None);
    }

    /// Voice activation needs no binding at all, whatever the listener was last
    /// doing.
    #[test]
    fn voice_activation_reads_as_off() {
        for status in [
            HotkeyStatus::Off,
            HotkeyStatus::Starting,
            HotkeyStatus::Global {
                backend: Backend::X11Raw,
                trigger: None,
                window_only: Vec::new(),
            },
            HotkeyStatus::WindowOnly("no display".to_owned()),
        ] {
            assert_eq!(
                hotkey_sentence(&status, TransmitMode::VoiceActivation),
                "Off (voice activation)"
            );
        }
    }

    fn close(actual: f32, expected: f32) -> bool {
        (actual - expected).abs() < 0.001
    }

    #[test]
    fn a_fraction_without_a_total_is_zero() {
        assert!(close(progress_fraction(0, 0), 0.0));
        assert!(close(progress_fraction(1024, 0), 0.0));
    }

    #[test]
    fn a_fraction_is_the_share_received() {
        assert!(close(progress_fraction(0, 100), 0.0));
        assert!(close(progress_fraction(25, 100), 0.25));
        assert!(close(progress_fraction(100, 100), 1.0));
    }

    #[test]
    fn a_fraction_never_passes_one() {
        assert!(close(progress_fraction(200, 100), 1.0));
    }

    #[test]
    fn a_device_name_that_is_the_system_default_is_no_name_at_all() {
        assert_eq!(device_choice(SYSTEM_DEFAULT.to_owned()), None);
        assert_eq!(
            device_choice("Webcam".to_owned()).as_deref(),
            Some("Webcam")
        );
    }

    #[test]
    fn a_counter_the_server_sends_is_clamped_into_the_window() {
        assert_eq!(counter(-1), 0);
        assert_eq!(counter(0), 0);
        assert_eq!(counter(7), 7);
        assert_eq!(counter(i64::MAX), u32::MAX);
    }

    #[test]
    fn the_dialogs_channel_is_the_one_it_named() {
        let mut channel = Channel {
            id: 4,
            kind: ChannelKind::Text as i32,
            name: "music".to_owned(),
            topic: String::new(),
            category_id: 1,
            position: 0,
            overrides: Vec::new(),
            dm_member_ids: Vec::new(),
        };

        assert!(pending_channel_name(Some("music"), &channel));
        assert!(!pending_channel_name(Some("other"), &channel));
        assert!(!pending_channel_name(None, &channel));
        // A DM never answers the dialog, whatever it is called.
        channel.kind = ChannelKind::Dm as i32;
        assert!(!pending_channel_name(Some("music"), &channel));
    }

    #[test]
    fn a_failure_reads_as_something_to_act_on() {
        assert_eq!(
            describe(&ApiFailure::Status(409, String::new())),
            "That username is taken"
        );
        assert_eq!(
            describe(&ApiFailure::Status(401, "  ".to_owned())),
            "Invalid username or password"
        );
        assert_eq!(
            describe(&ApiFailure::AuthChallenge("banned".to_owned())),
            "banned"
        );
        assert_eq!(
            describe(&ApiFailure::Throttled(30)),
            "Too many attempts, try again in 30s"
        );
        assert_eq!(
            describe(&ApiFailure::Malformed("junk".to_owned())),
            UNEXPECTED
        );
    }

    #[test]
    fn a_username_and_a_password_are_checked_before_the_request() {
        assert!(validate_username("ana").is_ok());
        assert_eq!(validate_username("").unwrap_err(), "Enter a username.");
        assert_eq!(
            validate_username(&"a".repeat(33)).unwrap_err(),
            "At most 32 characters."
        );
        assert_eq!(
            validate_username("a\nb").unwrap_err(),
            "No control characters."
        );

        assert!(validate_password(&"a".repeat(8)).is_ok());
        assert_eq!(
            validate_password("short").unwrap_err(),
            "Password must be 8 to 128 characters."
        );
        assert!(validate_password(&"a".repeat(129)).is_err());
    }

    #[test]
    fn an_unnamed_share_preset_falls_back_to_the_default() {
        let config = Config {
            share_resolution: "nonsense".to_owned(),
            share_fps: 7,
            ..Config::default()
        };

        let preset = share_preset(&config);
        assert_eq!(preset.resolution, Resolution::P720);
        assert_eq!(preset.fps, FrameRate::F30);
        assert_eq!(capture_box(Resolution::P720), Some((1280, 720)));
        assert_eq!(capture_box(Resolution::Source), None);
        // The table's 720p at 30 fps.
        assert_eq!(auto_bitrate_kbps(&config), 2_500);
    }

    #[test]
    fn trimming_drops_the_oldest_messages_first() {
        let mut messages: BTreeMap<i64, ChatMessage> = (1..=(MESSAGE_LIMIT as i64 + 2))
            .map(|id| {
                (
                    id,
                    ChatMessage {
                        id,
                        ..ChatMessage::default()
                    },
                )
            })
            .collect();

        trim(&mut messages);

        assert_eq!(messages.len(), MESSAGE_LIMIT);
        assert_eq!(messages.keys().next().copied(), Some(3));
    }
}
