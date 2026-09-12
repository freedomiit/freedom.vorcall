//! The voice channel, the screen share and the stage.
//!
//! `intent` is what survives a reconnect: the media path, the roster and the
//! ssrc map all belong to one connection, and a reconnect rebuilds them from
//! the intent rather than from what is left of them.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex};

use iced::window;
use vorcall_core::VoiceMember;
use vorcall_core::config::PeerAudio;
use vorcall_hotkey::{ActionId, Backend, Binding, Listener, Unavailable};
use vorcall_screen::codec::Picture;
use vorcall_screen::preset::Preset;
use vorcall_screen::{AudioMode, CaptureRequest};
use vorcall_voice::{FrameSender, MediaEngine, Playout, Stats, VideoStats};

use crate::workers::share::{DecodeHandle, ShareStats};

/// The media path of one voice session: the UDP engine, the mixer it feeds and
/// the sender the audio and share threads write to.
pub struct MediaSession {
    pub ssrc: u32,
    pub engine: MediaEngine,
    pub playout: Arc<Mutex<Playout>>,
    pub sender: FrameSender,
    /// The devices the audio thread opened; no input means the microphone did
    /// not open and the session is listen-only.
    pub audio_input: Option<String>,
    pub audio_output: Option<String>,
}

/// Who is in one channel's voice session. The client can be in only one of
/// them; the others are drawn but not heard.
#[derive(Debug, Default, Clone)]
pub struct VoiceRoster {
    pub members: BTreeMap<i64, VoiceMember>,
    pub speaking: BTreeSet<i64>,
    /// Who is sharing a screen, and whether that share carries audio. Kept
    /// beside `members`, which only holds what the last frame about a member
    /// said and goes stale the moment somebody starts or stops sharing.
    pub sharing: BTreeMap<i64, bool>,
}

impl VoiceRoster {
    /// The roster one `VoiceState` describes.
    pub fn from_members(members: Vec<VoiceMember>) -> Self {
        let sharing = members
            .iter()
            .filter(|member| member.sharing)
            .map(|member| (member.user_id, member.share_audio))
            .collect();
        Self {
            members: members
                .into_iter()
                .map(|member| (member.user_id, member))
                .collect(),
            speaking: BTreeSet::new(),
            sharing,
        }
    }

    pub fn insert(&mut self, member: VoiceMember) {
        self.set_sharing(&member);
        self.members.insert(member.user_id, member);
    }

    pub fn remove(&mut self, user_id: i64) {
        self.members.remove(&user_id);
        self.sharing.remove(&user_id);
        self.speaking.remove(&user_id);
    }

    /// One member's share, as the frame that carried them describes it.
    pub fn set_sharing(&mut self, member: &VoiceMember) {
        if member.sharing {
            self.sharing.insert(member.user_id, member.share_audio);
        } else {
            self.sharing.remove(&member.user_id);
        }
    }

    /// What this channel's priority speakers mean for the mixer right now: every
    /// other peer is quietened while one of them talks, and the priority
    /// speakers themselves never are. `enabled` is the local preference: with it
    /// off nothing is ever quietened.
    pub fn ducking(&self, enabled: bool) -> Ducking {
        if !enabled {
            return Ducking::default();
        }
        Ducking {
            active: self
                .members
                .values()
                .any(|member| member.priority && self.speaking.contains(&member.user_id)),
            exempt: self
                .members
                .values()
                .filter(|member| member.priority)
                .map(|member| member.ssrc)
                .collect(),
        }
    }
}

/// Whether a priority speaker is talking, and whose gain is left alone while one
/// is. Held beside the roster it was derived from, so only a change reaches the
/// audio thread.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ducking {
    pub active: bool,
    /// The ssrcs that keep their own volume, in roster order.
    pub exempt: Vec<u32>,
}

/// Everything the voice channel adds to the main screen.
#[derive(Default)]
pub struct VoiceUi {
    /// Whether this client means to be in voice. A reconnect rejoins on it.
    pub intent: bool,
    /// Which channel's voice session that is.
    pub channel_id: i64,
    /// `JoinVoice` is out and `VoiceReady` has not come back yet.
    pub joining: bool,
    pub session: Option<MediaSession>,
    pub muted: bool,
    pub deafened: bool,
    /// What un-deafening puts back.
    pub muted_before_deafen: bool,
    pub ptt_held: bool,
    /// What the audio thread says about its own microphone: under voice
    /// activation the gate decides this, not the key.
    pub transmitting: bool,
    /// The last level the audio thread reported, in dBFS, and whether the gate
    /// stood open — the settings meter, and nothing else.
    pub input_level: Option<(f32, bool)>,
    pub stats: Stats,
    /// Who is in which channel's voice session, the joined one included.
    pub rosters: BTreeMap<i64, VoiceRoster>,
    /// Whom the server marked as speaking in the joined channel. The roster's own
    /// set is this plus whoever the local decoder heard, which the tick expires,
    /// so the two cannot be kept in one place.
    pub speaking_server: BTreeSet<i64>,
    /// What the audio thread was last told about priority speakers.
    pub ducking: Ducking,
    pub share: ShareUi,
    pub watch: WatchUi,
    /// The system-wide listener. Dropping it stops it.
    pub hotkey: Option<Listener>,
    pub hotkey_status: HotkeyStatus,
    /// Which listener the session is on. Every start, edge stream and answer
    /// names the generation it belongs to, so a start still in flight when the
    /// next one begins can be told apart from it and dropped.
    pub hotkey_generation: u64,
    /// A mirror of the configured per-peer tuning, so applying it needs no
    /// `Config`.
    pub peer_audio: BTreeMap<i64, PeerAudio>,
    /// Whose volume and mute the member row has open.
    pub expanded_member: Option<i64>,
    /// The ssrc of the last `VoiceReady`, waiting for its engine.
    pub pending_ssrc: u32,
    pub by_ssrc: BTreeMap<u32, i64>,
    /// Whether a `VoiceState` for the joined channel has landed since this
    /// session's media path was set up. Until one has, the roster still
    /// describes the session that was replaced.
    pub roster_seen: bool,
    /// Ticks since the statistics were last read.
    pub ticks: u32,
}

impl VoiceUi {
    /// The joined channel's roster, if there is one.
    pub fn roster(&self) -> Option<&VoiceRoster> {
        self.rosters.get(&self.channel_id)
    }

    /// The joined channel's roster, created empty if this is the first word
    /// about it.
    pub fn roster_mut(&mut self, channel_id: i64) -> &mut VoiceRoster {
        self.rosters.entry(channel_id).or_default()
    }

    /// Whether `user_id` is sharing in the joined session, and with audio.
    pub fn sharing(&self, user_id: i64) -> Option<bool> {
        self.roster()?.sharing.get(&user_id).copied()
    }

    /// Which channel's voice session `user_id` is in, the joined one first: a
    /// moderation frame names the session its target is actually in.
    pub fn channel_of(&self, user_id: i64) -> Option<i64> {
        if self
            .roster()
            .is_some_and(|roster| roster.members.contains_key(&user_id))
        {
            return Some(self.channel_id);
        }
        self.rosters
            .iter()
            .find(|(_, roster)| roster.members.contains_key(&user_id))
            .map(|(channel_id, _)| *channel_id)
    }

    /// The local volume and mute stored for one member.
    pub fn peer_audio(&self, user_id: i64) -> PeerAudio {
        self.peer_audio.get(&user_id).copied().unwrap_or_default()
    }

    /// Every other member's ssrc and stored tuning, for the audio thread. There is
    /// nothing to tune about oneself: the local mixer never plays this client
    /// back.
    pub fn peers(&self, me: i64) -> Vec<(u32, PeerAudio)> {
        let Some(roster) = self.roster() else {
            return Vec::new();
        };
        roster
            .members
            .values()
            .filter(|member| member.user_id != me)
            .map(|member| (member.ssrc, self.peer_audio(member.user_id)))
            .collect()
    }

    /// One member's ssrc in the joined session, which is what their tuning is
    /// keyed by.
    pub fn ssrc_of(&self, user_id: i64) -> Option<u32> {
        Some(self.roster()?.members.get(&user_id)?.ssrc)
    }

    /// What one `VoiceState` says about a channel, keeping the speaking marks of
    /// whoever is still in it. For the joined channel it is also what re-keys the
    /// ssrc map, so the stored per-peer tuning can follow a rejoin.
    pub fn set_roster(&mut self, channel_id: i64, members: Vec<VoiceMember>) {
        let mut roster = VoiceRoster::from_members(members);
        if let Some(previous) = self.rosters.get(&channel_id) {
            roster.speaking = previous
                .speaking
                .iter()
                .copied()
                .filter(|user_id| roster.members.contains_key(user_id))
                .collect();
        }

        if channel_id == self.channel_id {
            self.roster_seen = true;
            self.by_ssrc = roster
                .members
                .values()
                .map(|member| (member.ssrc, member.user_id))
                .collect();
            self.speaking_server
                .retain(|user_id| roster.members.contains_key(user_id));
            if let Some(user_id) = self.expanded_member
                && !roster.members.contains_key(&user_id)
            {
                self.expanded_member = None;
            }
        }
        self.rosters.insert(channel_id, roster);
    }

    pub fn insert_member(&mut self, channel_id: i64, member: VoiceMember) {
        if channel_id == self.channel_id {
            // A Speaking(true) can outlive the member it was about; a rejoin
            // starts silent rather than lit up for good.
            self.speaking_server.remove(&member.user_id);
            self.by_ssrc.insert(member.ssrc, member.user_id);
        }
        let roster = self.roster_mut(channel_id);
        roster.speaking.remove(&member.user_id);
        roster.insert(member);
    }

    pub fn remove_member(&mut self, channel_id: i64, user_id: i64) {
        let ssrc = self.rosters.get_mut(&channel_id).and_then(|roster| {
            let ssrc = roster.members.get(&user_id).map(|member| member.ssrc);
            roster.remove(user_id);
            ssrc
        });
        if channel_id != self.channel_id {
            return;
        }
        if let Some(ssrc) = ssrc {
            self.by_ssrc.remove(&ssrc);
        }
        self.speaking_server.remove(&user_id);
        if self.expanded_member == Some(user_id) {
            self.expanded_member = None;
        }
    }

    /// One `Speaking` frame. The joined channel keeps the server's word of its own
    /// as well, because [`VoiceUi::refresh_speaking`] rebuilds the roster's set
    /// from it.
    pub fn set_speaking(&mut self, channel_id: i64, user_id: i64, speaking: bool) {
        let roster = self.roster_mut(channel_id);
        if speaking {
            roster.speaking.insert(user_id);
        } else {
            roster.speaking.remove(&user_id);
        }
        if channel_id != self.channel_id {
            return;
        }
        if speaking {
            self.speaking_server.insert(user_id);
        } else {
            self.speaking_server.remove(&user_id);
        }
    }

    /// The joined channel's speaking marks: what the server saw, plus whoever the
    /// local decoder heard in the last window. Anyone the decoder has stopped
    /// hearing and the server has not marked is no longer in it, which is how a
    /// locally derived mark expires.
    pub fn refresh_speaking(&mut self, heard: &[i64]) {
        let channel_id = self.channel_id;
        let marked: BTreeSet<i64> = self
            .speaking_server
            .iter()
            .copied()
            .chain(heard.iter().copied())
            .collect();
        let roster = self.roster_mut(channel_id);
        let kept = marked
            .into_iter()
            .filter(|user_id| roster.members.contains_key(user_id))
            .collect();
        roster.speaking = kept;
    }

    /// Joining another channel's voice session: the share and the watch belonged
    /// to the one being left, so neither is asserted again.
    pub fn switch_to(&mut self, channel_id: i64) {
        self.intent = true;
        self.joining = true;
        self.channel_id = channel_id;
        self.share.intent = None;
        self.watch.intent = None;
    }

    /// What leaving gives up: the voice session and both screen-share intents, so
    /// none of them is asserted again on the next connection.
    pub fn give_up_intents(&mut self) {
        self.intent = false;
        self.joining = false;
        self.share.intent = None;
        self.watch.intent = None;
    }

    /// Whether there is a live media path.
    pub fn is_live(&self) -> bool {
        self.session.is_some()
    }

    /// Takes the media session out, for the caller to close off the UI thread.
    /// Everything reset here is derived from the media path, so nothing is reset
    /// when there is none to take: `VoiceState` lands between a `VoiceReady` and
    /// its engine, and what it said about the session being opened — the ssrc map,
    /// `roster_seen` — has to survive that engine arriving.
    pub fn take_session(&mut self) -> Option<MediaSession> {
        let session = self.session.take()?;
        let channel_id = self.channel_id;
        self.pending_ssrc = 0;
        self.joining = false;
        self.by_ssrc.clear();
        self.roster_seen = false;
        self.ptt_held = false;
        self.transmitting = false;
        self.input_level = None;
        self.stats = Stats::default();
        self.expanded_member = None;
        self.ducking = Ducking::default();
        // The speaking marks belong to the session that is going: half of them
        // were the local decoder's, and nothing will ever take them back.
        self.speaking_server.clear();
        if let Some(roster) = self.rosters.get_mut(&channel_id) {
            roster.speaking.clear();
        }
        Some(session)
    }

    /// What one connection owns: every roster, the ssrc map and the media path.
    /// The intents are the caller's business — a reconnect keeps them, leaving
    /// voice gives them up.
    pub fn clear_connection(&mut self) {
        self.rosters.clear();
        self.share.stopped();
        self.watch.stopped();
    }
}

/// This client's own screen share. `intent` is what makes a reconnect start it
/// again; nothing else here survives one.
#[derive(Default)]
pub struct ShareUi {
    pub intent: Option<ShareIntent>,
    /// A capture is being started and the server has not answered for it yet.
    pub starting: bool,
    pub active: bool,
    pub watchers: u32,
    pub stats: Option<ShareStats>,
    pub backend: Option<&'static str>,
    /// What the backend really captured, which is not always what was asked
    /// for: `None` is a share without audio.
    pub audio: Option<AudioMode>,
}

impl ShareUi {
    /// Everything about a share that is over. The intent is the caller's: a
    /// reconnect keeps it, a failure gives it up.
    pub fn stopped(&mut self) {
        self.starting = false;
        self.active = false;
        self.stats = None;
        self.watchers = 0;
        self.backend = None;
        self.audio = None;
    }
}

/// What a share was started with, so a reconnect can start the same one again.
pub struct ShareIntent {
    pub request: CaptureRequest,
    pub preset: Preset,
}

/// The share being watched: the intent that survives a reconnect, and the
/// decoded picture the stage draws.
pub struct WatchUi {
    pub intent: Option<i64>,
    /// Whose stream the server has actually put this client on.
    pub state: Option<i64>,
    pub picture: Option<Arc<Picture>>,
    pub seq: u64,
    /// Dropping it stops the decode thread; one thread serves a whole session,
    /// because the access units are handed out once per engine.
    pub decoder: Option<DecodeHandle>,
    /// Decoded frames per second, pictures and errors, as the decode thread last
    /// reported them.
    pub stats: Option<(f32, u64, u64)>,
    pub video: VideoStats,
    pub popped: Option<window::Id>,
    /// The window the stage has taken over, which is not always the main one.
    pub fullscreen: Option<window::Id>,
    pub volume: f32,
    /// `video.bytes` as of the last report, for the rate below.
    pub last_bytes: u64,
    pub kbps: u32,
    /// Whether the next `VoiceState` for the joined channel decides the watch a
    /// reconnect kept: it is only worth asking for again while that user shares.
    pub resume_pending: bool,
}

impl Default for WatchUi {
    fn default() -> Self {
        Self {
            intent: None,
            state: None,
            picture: None,
            seq: 0,
            decoder: None,
            stats: None,
            video: VideoStats::default(),
            popped: None,
            fullscreen: None,
            volume: 1.0,
            last_bytes: 0,
            kbps: 0,
            resume_pending: false,
        }
    }
}

impl WatchUi {
    /// What a watch that is over leaves behind: the windows and the decode
    /// thread are the caller's to close.
    pub fn stopped(&mut self) {
        self.state = None;
        self.picture = None;
        self.seq = 0;
        self.stats = None;
        self.video = VideoStats::default();
        self.last_bytes = 0;
        self.kbps = 0;
        self.resume_pending = false;
    }
}

/// What a `ShareStopped` leaves of the watch intent: the screen being watched is
/// gone, and there is nothing to go back to. Anybody else's share ending never
/// touches it.
pub fn watch_intent_after_stop(intent: Option<i64>, stopped_user_id: i64) -> Option<i64> {
    intent.filter(|watched| *watched != stopped_user_id)
}

/// Where push-to-talk edges come from. `Global` is a listener actually running;
/// `WindowOnly` carries why there is none, which the settings page shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum HotkeyStatus {
    #[default]
    Off,
    Starting,
    Global {
        backend: Backend,
        /// The compositor's own description of what it bound for push to talk,
        /// which does not have to be what was asked for. Wayland only.
        trigger: Option<String>,
        /// The actions the backend could not observe, each with the reason it
        /// gave. A listener runs for everything else; these stay the window's
        /// own, so one unbindable key costs only itself.
        window_only: Vec<(ActionId, String)>,
    },
    WindowOnly(String),
}

impl HotkeyStatus {
    /// Whether the running listener is what drives `action`. While it is, the
    /// window must not act on the same input: the listener reports that press
    /// too.
    pub fn observes(&self, action: ActionId) -> bool {
        match self {
            HotkeyStatus::Global { window_only, .. } => {
                !window_only.iter().any(|(skipped, _)| *skipped == action)
            }
            HotkeyStatus::Off | HotkeyStatus::Starting | HotkeyStatus::WindowOnly(_) => false,
        }
    }

    /// Why `action` is the window's own, if it is one the backend left out.
    pub fn window_only_reason(&self, action: ActionId) -> Option<&str> {
        match self {
            HotkeyStatus::Global { window_only, .. } => window_only
                .iter()
                .find(|(skipped, _)| *skipped == action)
                .map(|(_, reason)| reason.as_str()),
            HotkeyStatus::Off | HotkeyStatus::Starting | HotkeyStatus::WindowOnly(_) => None,
        }
    }
}

/// A [`MediaEngine`] is not `Clone` and every message is, so the engine travels
/// in this instead: the handler that gets there first takes it out. `ssrc` says
/// which `VoiceReady` asked for it, so an engine the session has already moved
/// past can be told apart from the one being waited on.
#[derive(Clone)]
pub struct EngineHandoff {
    pub ssrc: u32,
    engine: Arc<Mutex<Option<MediaEngine>>>,
}

impl EngineHandoff {
    pub fn new(ssrc: u32, engine: MediaEngine) -> Self {
        Self {
            ssrc,
            engine: Arc::new(Mutex::new(Some(engine))),
        }
    }

    /// The engine, once. Every later caller gets nothing.
    pub fn take(&self) -> Option<MediaEngine> {
        self.engine
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

impl fmt::Debug for EngineHandoff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EngineHandoff")
    }
}

/// A [`Listener`] is not `Clone` either, so a started listener travels the same
/// way an engine does. `generation` and `ssrc` say what it was started for: an
/// answer that outlived either is dropped, which stops it, rather than becoming
/// a second listener.
#[derive(Clone)]
pub struct HotkeyHandoff {
    pub generation: u64,
    pub ssrc: u32,
    /// What was asked for, for the log line and the settings sentence.
    pub bindings: Vec<Binding>,
    started: Arc<Mutex<Option<Result<Listener, Unavailable>>>>,
}

impl HotkeyHandoff {
    pub fn new(
        generation: u64,
        ssrc: u32,
        bindings: Vec<Binding>,
        started: Result<Listener, Unavailable>,
    ) -> Self {
        Self {
            generation,
            ssrc,
            bindings,
            started: Arc::new(Mutex::new(Some(started))),
        }
    }

    /// The listener, once. Every later caller gets nothing.
    pub fn take(&self) -> Option<Result<Listener, Unavailable>> {
        self.started
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

impl fmt::Debug for HotkeyHandoff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HotkeyHandoff")
    }
}

#[cfg(test)]
mod tests {
    use vorcall_hotkey::{Key, Trigger};
    use vorcall_screen::preset::{FrameRate, Resolution};

    use super::*;

    fn voice_member(user_id: i64, sharing: bool) -> VoiceMember {
        VoiceMember {
            user_id,
            username: format!("user{user_id}"),
            ssrc: user_id as u32,
            sharing,
            share_audio: sharing,
            server_muted: false,
            server_deafened: false,
            priority: false,
            self_muted: false,
            self_deafened: false,
        }
    }

    /// A member with their own ssrc, which is what the mixer keys their tuning by.
    fn member_with_ssrc(user_id: i64, ssrc: u32) -> VoiceMember {
        VoiceMember {
            ssrc,
            ..voice_member(user_id, false)
        }
    }

    /// One channel's roster, joined by this client.
    fn joined(channel_id: i64, members: Vec<VoiceMember>) -> VoiceUi {
        let mut voice = VoiceUi {
            intent: true,
            channel_id,
            ..VoiceUi::default()
        };
        voice.set_roster(channel_id, members);
        voice
    }

    /// The stage has nothing left to draw once the screen it was watching is
    /// gone, so the intent no reconnect should resume goes with it.
    #[test]
    fn a_share_stopped_for_the_watched_user_drops_the_watch_intent() {
        let mut voice = joined(10, vec![voice_member(9, true)]);
        voice.watch.intent = Some(9);

        voice.roster_mut(10).sharing.remove(&9);
        voice.watch.intent = watch_intent_after_stop(voice.watch.intent, 9);

        assert_eq!(voice.watch.intent, None);
        assert_eq!(voice.sharing(9), None);
        // Somebody else's share ending leaves the watch where it was.
        assert_eq!(watch_intent_after_stop(Some(9), 4), Some(9));
        assert_eq!(watch_intent_after_stop(None, 9), None);
    }

    #[test]
    fn a_roster_reads_who_is_sharing_off_the_members() {
        let mut roster =
            VoiceRoster::from_members(vec![voice_member(4, false), voice_member(9, true)]);

        assert_eq!(roster.members.len(), 2);
        assert_eq!(roster.sharing.get(&9), Some(&true));
        assert_eq!(roster.sharing.get(&4), None);

        roster.insert(voice_member(4, true));
        assert_eq!(roster.sharing.get(&4), Some(&true));
        roster.remove(4);
        assert_eq!(roster.sharing.get(&4), None);
        assert!(!roster.members.is_empty());
    }

    /// Two handlers can see the same message; the second must not get a listener
    /// of its own.
    #[test]
    fn a_hotkey_handoff_gives_its_answer_up_once() {
        let handoff = HotkeyHandoff::new(
            1,
            7,
            vec![Binding::simple(Trigger::Key(Key::Control))],
            Err(Unavailable::Unsupported("no display".to_owned())),
        );

        assert!(handoff.take().is_some());
        assert!(handoff.take().is_none());
    }

    /// A listener that could not observe one of its bindings holds the rest; that
    /// one action stays the window's own.
    #[test]
    fn a_skipped_binding_is_the_only_one_the_listener_does_not_hold() {
        let status = HotkeyStatus::Global {
            backend: Backend::WaylandPortal,
            trigger: None,
            window_only: vec![(0, "mouse buttons are window-only on Wayland".to_owned())],
        };

        assert!(!status.observes(0));
        assert!(status.observes(1));
        assert_eq!(
            status.window_only_reason(0),
            Some("mouse buttons are window-only on Wayland")
        );
        assert_eq!(status.window_only_reason(1), None);
        // Nothing is observed while no listener runs.
        assert!(!HotkeyStatus::Starting.observes(1));
        assert!(!HotkeyStatus::WindowOnly("no display".to_owned()).observes(1));
    }

    #[test]
    fn a_disconnect_forgets_every_roster_but_not_the_intent() {
        let mut voice = VoiceUi {
            intent: true,
            channel_id: 10,
            ..VoiceUi::default()
        };
        voice.share.active = true;
        voice.watch.intent = Some(9);
        voice.watch.state = Some(9);
        voice
            .rosters
            .insert(10, VoiceRoster::from_members(vec![voice_member(9, true)]));

        voice.clear_connection();

        assert!(voice.intent);
        assert_eq!(voice.channel_id, 10);
        assert!(voice.rosters.is_empty());
        assert!(!voice.share.active);
        // The watch intent outlives the connection; what it was watching does
        // not.
        assert_eq!(voice.watch.intent, Some(9));
        assert_eq!(voice.watch.state, None);
    }

    /// Holding the permission is not talking: the room is only quietened while a
    /// priority speaker actually says something, and never for that speaker.
    #[test]
    fn ducking_waits_for_a_priority_speaker_to_talk() {
        let priority = VoiceMember {
            priority: true,
            ..member_with_ssrc(4, 40)
        };
        let mut voice = joined(10, vec![priority, member_with_ssrc(9, 90)]);

        let quiet = voice.roster().expect("the joined roster").ducking(true);
        assert!(!quiet.active);
        assert_eq!(quiet.exempt, vec![40]);

        // The other member talking is nobody's priority.
        voice.set_speaking(10, 9, true);
        assert!(
            !voice
                .roster()
                .expect("the joined roster")
                .ducking(true)
                .active
        );

        voice.set_speaking(10, 4, true);
        let ducked = voice.roster().expect("the joined roster").ducking(true);
        assert!(ducked.active);
        assert_eq!(ducked.exempt, vec![40]);

        // Another channel's priority speaker is nothing to this one.
        voice.set_speaking(10, 4, false);
        voice.set_roster(
            11,
            vec![VoiceMember {
                priority: true,
                ..member_with_ssrc(7, 70)
            }],
        );
        voice.set_speaking(11, 7, true);
        assert!(
            !voice
                .roster()
                .expect("the joined roster")
                .ducking(true)
                .active
        );
    }

    /// The preference is a local playback choice: the roster still says who the
    /// priority speakers are, the mixer is just never told to quieten anybody.
    #[test]
    fn the_preference_off_leaves_a_talking_priority_speaker_ducking_nothing() {
        let priority = VoiceMember {
            priority: true,
            ..member_with_ssrc(4, 40)
        };
        let mut voice = joined(10, vec![priority, member_with_ssrc(9, 90)]);
        voice.set_speaking(10, 4, true);

        assert!(
            voice
                .roster()
                .expect("the joined roster")
                .ducking(true)
                .active
        );

        let off = voice.roster().expect("the joined roster").ducking(false);
        assert!(!off.active);
        assert!(off.exempt.is_empty());
        assert_eq!(off, Ducking::default());
    }

    /// A `VoiceState` lands between a `VoiceReady` and the engine answering it, so
    /// closing a media path that never came up must not forget what it said — nor
    /// the ssrc the engine on its way is for.
    #[test]
    fn closing_nothing_keeps_what_the_fresh_roster_said() {
        let mut voice = joined(10, vec![member_with_ssrc(9, 90)]);
        voice.pending_ssrc = 7;

        assert!(voice.take_session().is_none());

        assert!(voice.roster_seen);
        assert_eq!(voice.by_ssrc.get(&90), Some(&9));
        assert_eq!(voice.pending_ssrc, 7);
    }

    /// A moderator's move, and picking another voice channel: the session that is
    /// left takes the share and the watch with it.
    #[test]
    fn switching_channels_keeps_the_voice_intent_and_drops_the_share_and_watch() {
        let preset = Preset {
            resolution: Resolution::P720,
            fps: FrameRate::F30,
            bitrate_kbps: None,
        };
        let mut voice = joined(10, vec![voice_member(9, true)]);
        voice.share.intent = Some(ShareIntent {
            request: CaptureRequest {
                source: None,
                fps: preset.fps,
                cursor: true,
                audio: false,
                max_size: None,
            },
            preset,
        });
        voice.watch.intent = Some(9);

        voice.switch_to(11);

        assert!(voice.intent);
        assert!(voice.joining);
        assert_eq!(voice.channel_id, 11);
        assert!(voice.share.intent.is_none());
        assert_eq!(voice.watch.intent, None);

        // Leaving gives up the voice session as well.
        voice.switch_to(11);
        voice.give_up_intents();
        assert!(!voice.intent);
        assert!(!voice.joining);
    }

    #[test]
    fn a_member_joining_and_leaving_keeps_the_ssrc_map() {
        let mut voice = joined(10, vec![member_with_ssrc(9, 90)]);
        assert_eq!(voice.by_ssrc.get(&90), Some(&9));

        voice.insert_member(10, member_with_ssrc(4, 40));
        assert_eq!(voice.by_ssrc.get(&40), Some(&4));
        assert_eq!(voice.roster().map(|roster| roster.members.len()), Some(2));

        // A Speaking(true) does not outlive the member it was about.
        voice.set_speaking(10, 4, true);
        voice.remove_member(10, 4);
        voice.insert_member(10, member_with_ssrc(4, 41));
        assert!(!voice.speaking_server.contains(&4));
        assert_eq!(voice.by_ssrc.get(&40), None);
        assert_eq!(voice.by_ssrc.get(&41), Some(&4));

        voice.remove_member(10, 4);
        assert_eq!(voice.by_ssrc.get(&41), None);
        assert_eq!(voice.roster().map(|roster| roster.members.len()), Some(1));

        // Another channel's roster is kept apart from the joined one's ssrc map.
        voice.insert_member(11, member_with_ssrc(7, 70));
        assert_eq!(voice.by_ssrc.get(&70), None);
        assert_eq!(
            voice.rosters.get(&11).map(|roster| roster.members.len()),
            Some(1)
        );
    }

    /// An ssrc belongs to one session, so a rejoin has to carry the stored volume
    /// and mute over to the new one.
    #[test]
    fn a_fresh_roster_rekeys_the_stored_tuning() {
        let mut voice = joined(10, vec![member_with_ssrc(9, 90)]);
        voice.peer_audio.insert(
            9,
            PeerAudio {
                volume: 0.5,
                muted: true,
            },
        );
        voice.speaking_server.insert(9);

        assert_eq!(
            voice.peers(7),
            vec![(
                90,
                PeerAudio {
                    volume: 0.5,
                    muted: true
                }
            )]
        );

        voice.set_roster(10, vec![member_with_ssrc(9, 91), member_with_ssrc(7, 71)]);

        assert!(voice.roster_seen);
        assert_eq!(voice.ssrc_of(9), Some(91));
        // This client's own ssrc is never tuned: the mixer does not play it back.
        assert_eq!(
            voice.peers(7),
            vec![(
                91,
                PeerAudio {
                    volume: 0.5,
                    muted: true
                }
            )]
        );
        // The server's mark survives a roster that still holds the member.
        assert!(voice.speaking_server.contains(&9));

        voice.set_roster(10, vec![member_with_ssrc(7, 71)]);
        assert!(!voice.speaking_server.contains(&9));
        assert!(voice.peers(7).is_empty());
    }

    /// The two halves of a speaking mark: the server's word, which only another
    /// frame takes back, and the local decoder's, which the tick expires.
    #[test]
    fn a_mark_the_decoder_stops_hearing_expires_and_the_servers_does_not() {
        let mut voice = joined(10, vec![member_with_ssrc(9, 90), member_with_ssrc(4, 40)]);
        voice.set_speaking(10, 9, true);

        voice.refresh_speaking(&[4]);
        let speaking = &voice.roster().expect("the joined roster").speaking;
        assert!(speaking.contains(&9));
        assert!(speaking.contains(&4));

        voice.refresh_speaking(&[]);
        let speaking = &voice.roster().expect("the joined roster").speaking;
        assert!(speaking.contains(&9));
        assert!(!speaking.contains(&4));

        // Nobody outside the roster is ever marked.
        voice.refresh_speaking(&[11]);
        assert!(
            !voice
                .roster()
                .expect("the joined roster")
                .speaking
                .contains(&11)
        );
    }
}
