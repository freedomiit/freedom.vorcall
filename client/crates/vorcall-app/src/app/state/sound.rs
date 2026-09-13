//! The soundpad: the shared library as this client mirrors it, what is playing
//! in the joined channel, and the geometry the trim view is drawn and dragged
//! in.
//!
//! The library is server-wide — `PROTOCOL.md` § Sounds — so there is nothing
//! per channel or per member in it, and the two deltas keep it current between
//! snapshots. Nothing here draws, decodes or talks to the network.

use std::collections::{BTreeMap, BTreeSet};

use iced::Point;
use vorcall_core::Sound;
use vorcall_voice::Sfx;

/// The pixel width the trim view draws at, and the width a drag is measured in.
pub const TRIM_WIDTH: f32 = 360.0;

/// The shortest selection the trim allows: one 20 ms Opus frame, which is the
/// unit a clip is made of.
pub const MIN_SPAN_MS: u32 = 20;

/// The soundpad as this window holds it.
#[derive(Debug, Default)]
pub struct SoundState {
    /// Every clip the server has, by id.
    pub library: BTreeMap<i64, Sound>,
    /// What is playing in the joined voice channel, as the server last said.
    pub playing: Option<PlayingClip>,
    /// Where the play popover is anchored; `None` is closed.
    pub popover: Option<Point>,
    /// The clips being fetched, so a second trigger does not start a second
    /// download of the same bytes.
    pub fetching: BTreeSet<i64>,
}

/// One clip playing in the joined channel. The server never says a clip
/// finished, so this is only replaced or cleared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayingClip {
    pub sound_id: i64,
    /// Who triggered it, which is half of the rule the stop control follows.
    pub user_id: i64,
}

impl SoundState {
    /// Replaces the whole library, the way a reconnect replaces everything else.
    pub fn apply_snapshot(&mut self, sounds: Vec<Sound>) {
        self.library = sounds.into_iter().map(|sound| (sound.id, sound)).collect();
    }

    pub fn upsert(&mut self, sound: Sound) {
        self.library.insert(sound.id, sound);
    }

    pub fn remove(&mut self, sound_id: i64) {
        self.library.remove(&sound_id);
    }

    /// The library in the order the pages and the popover list it: by name, so
    /// two clips uploaded minutes apart do not sit at opposite ends.
    pub fn ordered(&self) -> Vec<&Sound> {
        let mut clips: Vec<&Sound> = self.library.values().collect();
        clips.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then(left.id.cmp(&right.id))
        });
        clips
    }

    /// A second `SoundPlayed` is the cut: it replaces the record rather than
    /// stacking on it.
    pub fn played(&mut self, sound_id: i64, user_id: i64) {
        self.playing = Some(PlayingClip { sound_id, user_id });
    }

    pub fn stopped(&mut self) {
        self.playing = None;
    }

    /// The voice session this clip belonged to is gone. The popover goes with
    /// it: there is nothing left for it to play into.
    pub fn left(&mut self) {
        self.playing = None;
        self.popover = None;
    }
}

/// The two switches a client owns, and what un-deafening puts back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Switches {
    pub muted: bool,
    pub deafened: bool,
    pub muted_before_deafen: bool,
}

/// Which switch was pressed. Deafening also mutes and unmuting also un-deafens,
/// so the press has to be named: the pair of flags afterwards cannot say which
/// of the two a listener pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Switch {
    Mute,
    Deafen,
}

/// The switches after one press, and the one motif that press is worth.
///
/// Exactly one motif, always the pressed switch's own: deafening drags `muted`
/// along with it and unmuting drops `deafened`, and neither of those is a press
/// anybody made.
pub fn press(switches: Switches, switch: Switch) -> (Switches, Sfx) {
    let mut next = switches;
    match switch {
        Switch::Mute => {
            next.muted = !switches.muted;
            // Speaking again while deafened means hearing again too.
            if !next.muted {
                next.deafened = false;
            }
            let motif = if next.muted { Sfx::Mute } else { Sfx::Unmute };
            (next, motif)
        }
        Switch::Deafen => {
            next.deafened = !switches.deafened;
            if next.deafened {
                next.muted_before_deafen = switches.muted;
                next.muted = true;
            } else {
                next.muted = switches.muted_before_deafen;
            }
            let motif = if next.deafened {
                Sfx::Deafen
            } else {
                Sfx::Undeafen
            };
            (next, motif)
        }
    }
}

/// Where the clip is cut, as fractions of the source, 0.0..=1.0.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrimState {
    pub start: f32,
    pub end: f32,
}

impl Default for TrimState {
    /// The whole of the source: pressing straight through keeps everything.
    fn default() -> Self {
        Self {
            start: 0.0,
            end: 1.0,
        }
    }
}

/// A drag in flight: which edge is being moved, where in the frame the pointer
/// went down, and where that edge was when it did. The delta is measured from
/// there rather than from the last move, so an edge held against the other one
/// comes back off it in step with the pointer.
#[derive(Debug, Clone, Copy)]
pub struct TrimDrag {
    pub edge: TrimEdge,
    /// The pointer's x inside the frame, in [`TRIM_WIDTH`] pixels. Not a number
    /// until the first move: a press on a grip carries no position of its own.
    pub from: f32,
    /// The fraction that edge was at when the pointer went down.
    pub origin: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrimEdge {
    Start,
    End,
}

/// The trim one drag leaves behind. Only the dragged edge moves, and it stops at
/// the other one rather than passing it — the minimum span is [`span_ms`]'s to
/// enforce, because it is the one that knows how long the source is.
pub fn drag_to(trim: TrimState, drag: TrimDrag, at: f32) -> TrimState {
    let moved = (drag.origin + (at - drag.from) / TRIM_WIDTH).clamp(0.0, 1.0);
    match drag.edge {
        TrimEdge::Start => TrimState {
            start: moved.min(trim.end),
            end: trim.end,
        },
        TrimEdge::End => TrimState {
            start: trim.start,
            end: moved.max(trim.start),
        },
    }
}

/// The cut one trim names, in milliseconds of the source.
///
/// The fractions are clamped rather than trusted: they come from a drag, and the
/// answer has to lie inside the source, keep `start` before `end` and be at
/// least one 20 ms frame long — which is the shortest clip that can be encoded.
pub fn span_ms(trim: TrimState, duration_ms: u32) -> (u32, u32) {
    // A source shorter than one frame cannot hold the minimum, and is not a clip
    // anybody can cut: the whole of it is the only honest answer.
    let span = MIN_SPAN_MS.min(duration_ms);
    if span == 0 {
        return (0, 0);
    }

    let at = |fraction: f32| -> u32 {
        if fraction.is_nan() {
            return 0;
        }
        (fraction.clamp(0.0, 1.0) * duration_ms as f32).round() as u32
    };
    let mut start = at(trim.start).min(duration_ms - span);
    let mut end = at(trim.end).clamp(start, duration_ms);

    if end - start < span {
        end = start + span;
        if end > duration_ms {
            end = duration_ms;
            start = duration_ms - span;
        }
    }
    (start, end)
}

/// One duration as a clock reads it: `m:ss`, rounded down to the second the way
/// a media player counts.
pub fn clock(ms: u32) -> String {
    let seconds = ms / 1_000;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// Whether the stop control is offered: a clip is playing in the joined channel
/// and the viewer either triggered it or holds `MANAGE_SOUNDS`.
///
/// Mirrors `StopSound` in `server/Chat/ConnectionRegistry.Management.cs`; the
/// server is still the boundary, so disagreeing costs a control that is not
/// drawn, never an accepted frame.
pub fn can_stop(playing: Option<PlayingClip>, viewer: i64, manage_sounds: bool) -> bool {
    playing.is_some_and(|clip| clip.user_id == viewer || manage_sounds)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sound(id: i64, name: &str) -> Sound {
        Sound {
            id,
            name: name.to_owned(),
            uploader_id: 7,
            duration_ms: 1_000,
            size: 4_096,
        }
    }

    fn trim(start: f32, end: f32) -> TrimState {
        TrimState { start, end }
    }

    #[test]
    fn a_span_is_the_fractions_of_the_source() {
        assert_eq!(span_ms(trim(0.0, 1.0), 4_000), (0, 4_000));
        assert_eq!(span_ms(trim(0.25, 0.75), 4_000), (1_000, 3_000));
    }

    /// The fractions come from a drag, so nothing here trusts them.
    #[test]
    fn a_span_never_leaves_the_source() {
        assert_eq!(span_ms(trim(-3.0, 9.0), 4_000), (0, 4_000));
        assert_eq!(span_ms(trim(f32::NAN, f32::NAN), 4_000), (0, MIN_SPAN_MS));

        for duration in [MIN_SPAN_MS, 500, 4_000, 600_000] {
            for step in 0..=10u8 {
                let along = f32::from(step) / 10.0;
                let (start, end) = span_ms(trim(along, 1.0 - along), duration);
                assert!(start < end, "{start}..{end} of {duration} is empty");
                assert!(end <= duration, "{end} runs past {duration}");
                assert!(
                    end - start >= MIN_SPAN_MS.min(duration),
                    "{start}..{end} of {duration} is under one frame"
                );
            }
        }
    }

    /// An inverted or empty selection still comes back as one frame inside the
    /// source, at either end of it.
    #[test]
    fn a_span_is_at_least_one_frame() {
        assert_eq!(span_ms(trim(0.5, 0.5), 4_000), (2_000, 2_000 + MIN_SPAN_MS));
        assert_eq!(span_ms(trim(1.0, 1.0), 4_000), (4_000 - MIN_SPAN_MS, 4_000));
        // An inverted pair is read start-first: the end is pulled up to it.
        assert_eq!(span_ms(trim(0.9, 0.1), 4_000), (3_600, 3_600 + MIN_SPAN_MS));
        // Shorter than one frame: the whole of it, rather than nothing.
        assert_eq!(span_ms(trim(0.2, 0.4), 10), (0, 10));
    }

    #[test]
    fn a_drag_moves_only_the_edge_it_started_on() {
        let start = trim(0.2, 0.8);

        let dragged = drag_to(
            start,
            TrimDrag {
                edge: TrimEdge::Start,
                from: 0.0,
                origin: 0.2,
            },
            TRIM_WIDTH / 10.0,
        );
        assert_eq!(dragged.end, 0.8);
        assert!((dragged.start - 0.3).abs() < 1e-6, "{}", dragged.start);

        let dragged = drag_to(
            start,
            TrimDrag {
                edge: TrimEdge::End,
                from: TRIM_WIDTH,
                origin: 0.8,
            },
            TRIM_WIDTH / 2.0,
        );
        assert_eq!(dragged.start, 0.2);
        assert!((dragged.end - 0.3).abs() < 1e-6, "{}", dragged.end);
    }

    #[test]
    fn neither_edge_can_be_dragged_past_the_other() {
        let start = trim(0.2, 0.8);

        let crossed = drag_to(
            start,
            TrimDrag {
                edge: TrimEdge::Start,
                from: 0.0,
                origin: 0.2,
            },
            TRIM_WIDTH,
        );
        assert_eq!(crossed, trim(0.8, 0.8));

        let crossed = drag_to(
            start,
            TrimDrag {
                edge: TrimEdge::End,
                from: TRIM_WIDTH,
                origin: 0.8,
            },
            -TRIM_WIDTH,
        );
        assert_eq!(crossed, trim(0.2, 0.2));
    }

    fn switches(muted: bool, deafened: bool) -> Switches {
        Switches {
            muted,
            deafened,
            muted_before_deafen: false,
        }
    }

    /// Deafening mutes as well; that is not a mute anybody pressed.
    #[test]
    fn one_press_of_deafen_is_one_deafen_motif() {
        let (deafened, motif) = press(switches(false, false), Switch::Deafen);

        assert_eq!(motif, Sfx::Deafen);
        assert_eq!((deafened.muted, deafened.deafened), (true, true));

        let (heard, motif) = press(deafened, Switch::Deafen);
        assert_eq!(motif, Sfx::Undeafen);
        // Un-deafening puts back the switch the deafen found, not the one it set.
        assert_eq!((heard.muted, heard.deafened), (false, false));
    }

    /// Unmuting un-deafens as well; that is not a deafen anybody pressed.
    #[test]
    fn one_press_of_mute_is_one_mute_motif() {
        let (muted, motif) = press(switches(false, false), Switch::Mute);
        assert_eq!(motif, Sfx::Mute);
        assert!(muted.muted);

        let (deafened, _) = press(switches(false, false), Switch::Deafen);
        let (speaking, motif) = press(deafened, Switch::Mute);
        assert_eq!(motif, Sfx::Unmute);
        assert_eq!((speaking.muted, speaking.deafened), (false, false));
    }

    /// A mute held before deafening is the mute that comes back.
    #[test]
    fn un_deafening_restores_the_mute_the_deafen_found() {
        let (deafened, _) = press(switches(true, false), Switch::Deafen);
        let (back, motif) = press(deafened, Switch::Deafen);

        assert_eq!(motif, Sfx::Undeafen);
        assert_eq!((back.muted, back.deafened), (true, false));
    }

    #[test]
    fn stopping_belongs_to_whoever_started_it_and_to_a_manager() {
        let playing = Some(PlayingClip {
            sound_id: 3,
            user_id: 11,
        });

        assert!(can_stop(playing, 11, false));
        assert!(can_stop(playing, 42, true));
        assert!(!can_stop(playing, 42, false));
        // Nothing playing is nothing to stop, whatever the viewer holds.
        assert!(!can_stop(None, 11, true));
    }

    #[test]
    fn a_duration_reads_as_minutes_and_seconds() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(1_999), "0:01");
        assert_eq!(clock(61_000), "1:01");
        assert_eq!(clock(600_000), "10:00");
    }

    #[test]
    fn the_library_takes_an_upsert_and_a_delete() {
        let mut state = SoundState::default();
        state.apply_snapshot(vec![sound(1, "airhorn"), sound(2, "drums")]);

        state.upsert(sound(2, "drum roll"));
        state.upsert(sound(3, "applause"));
        assert_eq!(
            state.library.get(&2).map(|sound| sound.name.as_str()),
            Some("drum roll")
        );

        state.remove(1);
        let names: Vec<&str> = state
            .ordered()
            .into_iter()
            .map(|sound| sound.name.as_str())
            .collect();
        assert_eq!(names, ["applause", "drum roll"]);
    }

    /// A reconnect replaces the library rather than merging into it.
    #[test]
    fn a_snapshot_replaces_the_library_wholesale() {
        let mut state = SoundState::default();
        state.apply_snapshot(vec![sound(1, "airhorn"), sound(2, "drums")]);

        state.apply_snapshot(vec![sound(5, "bell")]);

        assert_eq!(state.library.keys().copied().collect::<Vec<_>>(), [5]);
    }

    #[test]
    fn a_second_clip_replaces_the_first_and_a_stop_clears_it() {
        let mut state = SoundState::default();

        state.played(1, 11);
        state.played(2, 42);
        assert_eq!(
            state.playing,
            Some(PlayingClip {
                sound_id: 2,
                user_id: 42
            })
        );

        state.stopped();
        assert_eq!(state.playing, None);

        state.played(3, 11);
        state.left();
        assert_eq!(state.playing, None);
    }
}
