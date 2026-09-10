//! One adaptive jitter buffer per remote ssrc.
//!
//! Packets are held in sequence order for `target_ms` before playout starts, so
//! reordering and network jitter are absorbed; whatever is still missing when
//! its slot comes up is reported as [`Frame::Lost`] and concealed by the codec.
//! Sequence numbers are shared with the engine's keepalive pings, so a hole in
//! them is not by itself a lost frame: how much audio a hole really holds is
//! read off the 48 kHz timestamps.
//! The target walks between [`MIN_TARGET_MS`] and [`MAX_TARGET_MS`]: it grows
//! when packets arrive after their deadline, and shrinks again at the end of a
//! talk spurt once the line has been clean for a while.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use crate::{FRAME_MS, FRAME_SAMPLES};

pub const MIN_TARGET_MS: u32 = 60;
pub const MAX_TARGET_MS: u32 = 100;
pub const STEP_MS: u32 = 20;
pub const SPURT_GAP: Duration = Duration::from_millis(500);

/// A `ts` step this large means the talker stopped and started again rather
/// than that packets were lost: 500 ms at 48 kHz.
const TS_JUMP_SAMPLES: u32 = 24_000;
/// A late arrival grows the target only if the line was already jittering.
const LATE_GROW_WINDOW: Duration = Duration::from_secs(2);
/// How long a clean line must stay clean before the target shrinks again.
const LATE_SHRINK_WINDOW: Duration = Duration::from_secs(10);
/// Concealment past this many frames in a row is noise, not speech.
const MAX_CONSECUTIVE_LOST: u32 = 5;
/// One 20 ms frame on the media clock.
const TS_PER_FRAME: u32 = FRAME_SAMPLES as u32;

#[derive(Clone, Debug)]
pub struct Incoming {
    pub seq: u64,
    pub ts: u32,
    pub marker: bool,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    Packet(Vec<u8>),
    Lost,
    Idle,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JitterStats {
    pub received: u64,
    /// Audio frames the timestamps prove were never delivered.
    pub lost: u64,
    /// Frames handed to the decoder's concealment, whether or not audio was
    /// truly lost: a delayed packet is concealed and then still played.
    pub concealed: u64,
    pub late: u64,
    pub duplicates: u64,
}

pub struct JitterBuffer {
    packets: BTreeMap<u64, Incoming>,
    next_expected: u64,
    /// No talk spurt in progress: the next packet starts one.
    idle: bool,
    playing: bool,
    spurt_started_at: Option<Instant>,
    last_packet_at: Option<Instant>,
    last_ts: Option<u32>,
    /// `ts` of the last frame actually played, which is what the size of a
    /// hole is measured against. `None` until the first frame of a spurt.
    last_played_ts: Option<u32>,
    /// Frames concealed since the last real packet was played.
    gap_concealed: u32,
    target_ms: u32,
    /// The target captured when the current spurt started; a target change
    /// therefore only takes effect from the next spurt.
    spurt_target_ms: u32,
    late_events: VecDeque<Instant>,
    consecutive_lost: u32,
    stats: JitterStats,
}

impl JitterBuffer {
    pub fn new() -> Self {
        Self {
            packets: BTreeMap::new(),
            next_expected: 0,
            idle: true,
            playing: false,
            spurt_started_at: None,
            last_packet_at: None,
            last_ts: None,
            last_played_ts: None,
            gap_concealed: 0,
            target_ms: MIN_TARGET_MS,
            spurt_target_ms: MIN_TARGET_MS,
            late_events: VecDeque::new(),
            consecutive_lost: 0,
            stats: JitterStats::default(),
        }
    }

    pub fn push(&mut self, now: Instant, packet: Incoming) {
        self.stats.received += 1;

        let ts_jump = self.last_ts.is_some_and(|last| {
            (packet.ts.wrapping_sub(last) as i32).unsigned_abs() > TS_JUMP_SAMPLES
        });
        if self.idle || packet.marker || ts_jump {
            self.start_spurt(now, packet.seq);
        }

        if self.packets.contains_key(&packet.seq) {
            self.stats.duplicates += 1;
            return;
        }
        if packet.seq < self.next_expected {
            if self.playing {
                self.stats.late += 1;
                self.note_late(now);
                return;
            }
            // Still buffering, so nothing has been played yet: a reordered
            // packet just extends this spurt backwards.
            self.next_expected = packet.seq;
        }

        self.last_packet_at = Some(now);
        self.last_ts = Some(packet.ts);
        self.packets.insert(packet.seq, packet);
        self.enforce_depth();
        self.maybe_start_playout(now);
    }

    /// Called every 20 ms by the playout clock.
    pub fn pull(&mut self, now: Instant) -> Frame {
        if self.idle {
            return Frame::Idle;
        }
        self.maybe_start_playout(now);
        if !self.playing {
            return Frame::Idle;
        }

        if let Some(packet) = self.packets.remove(&self.next_expected) {
            self.next_expected = self.next_expected.wrapping_add(1);
            return self.play(packet);
        }

        if let Some((first_seq, first_ts)) = self
            .packets
            .iter()
            .next()
            .map(|(seq, packet)| (*seq, packet.ts))
        {
            // The hole may be pings rather than audio, so ask the media clock
            // how many frames it really covers.
            let missing_audio = match self.last_played_ts {
                None => 0,
                Some(last) => (first_ts.wrapping_sub(last) / TS_PER_FRAME).saturating_sub(1),
            };
            // The second arm bounds a run of concealment the way the empty
            // buffer is bounded: a bogus timestamp far in the future must not
            // conceal forever while a playable packet waits.
            if self.gap_concealed >= missing_audio || self.consecutive_lost >= MAX_CONSECUTIVE_LOST
            {
                self.stats.lost += u64::from(missing_audio);
                self.next_expected = first_seq.wrapping_add(1);
                if let Some(packet) = self.packets.remove(&first_seq) {
                    return self.play(packet);
                }
            }
            return self.conceal();
        }

        let silent_for = self
            .last_packet_at
            .map(|last| now.saturating_duration_since(last));
        if silent_for.is_none_or(|gap| gap > SPURT_GAP)
            || self.consecutive_lost >= MAX_CONSECUTIVE_LOST
        {
            self.end_spurt(now);
            return Frame::Idle;
        }
        self.conceal()
    }

    pub fn target_ms(&self) -> u32 {
        self.target_ms
    }

    pub fn stats(&self) -> JitterStats {
        self.stats
    }

    pub fn last_packet_at(&self) -> Option<Instant> {
        self.last_packet_at
    }

    /// Conceals one frame without advancing `next_expected`: a packet that is
    /// merely late still plays when it arrives, and how much of the hole was
    /// real loss is settled once the next packet's timestamp says so.
    fn conceal(&mut self) -> Frame {
        self.stats.concealed += 1;
        self.gap_concealed += 1;
        self.consecutive_lost += 1;
        Frame::Lost
    }

    fn play(&mut self, packet: Incoming) -> Frame {
        self.last_played_ts = Some(packet.ts);
        self.gap_concealed = 0;
        self.consecutive_lost = 0;
        Frame::Packet(packet.payload)
    }

    fn start_spurt(&mut self, now: Instant, seq: u64) {
        // Anything still buffered belongs to the spurt that just ended.
        self.packets.clear();
        self.next_expected = seq;
        self.idle = false;
        self.playing = false;
        self.consecutive_lost = 0;
        self.last_played_ts = None;
        self.gap_concealed = 0;
        self.spurt_started_at = Some(now);
        self.spurt_target_ms = self.target_ms;
    }

    fn end_spurt(&mut self, now: Instant) {
        self.packets.clear();
        self.idle = true;
        self.playing = false;
        self.consecutive_lost = 0;
        self.last_played_ts = None;
        self.gap_concealed = 0;
        self.spurt_started_at = None;
        self.prune_late(now);
        if self.late_events.is_empty() && self.target_ms > MIN_TARGET_MS {
            self.target_ms -= STEP_MS;
        }
    }

    fn maybe_start_playout(&mut self, now: Instant) {
        if self.playing || self.idle {
            return;
        }
        let buffered = u32::try_from(self.packets.len()).unwrap_or(u32::MAX);
        let enough = buffered >= self.spurt_target_ms / FRAME_MS as u32;
        let waited = self.spurt_started_at.is_some_and(|start| {
            now.saturating_duration_since(start)
                >= Duration::from_millis(self.spurt_target_ms.into())
        });
        self.playing = enough || waited;
    }

    /// Keeps the buffer from growing without bound when a sender runs fast or a
    /// burst arrives at once.
    fn enforce_depth(&mut self) {
        let cap = (self.spurt_target_ms + 100) / FRAME_MS as u32;
        let keep = (self.spurt_target_ms / FRAME_MS as u32) as usize;
        if u32::try_from(self.packets.len()).unwrap_or(u32::MAX) <= cap {
            return;
        }
        while self.packets.len() > keep {
            let Some(&oldest) = self.packets.keys().next() else {
                break;
            };
            self.packets.remove(&oldest);
            // Counted as late for the caller, but deliberately not fed to the
            // adaptation: an over-full buffer argues for a smaller target, not
            // a larger one.
            self.stats.late += 1;
        }
        if let Some(&oldest) = self.packets.keys().next() {
            self.next_expected = oldest;
        }
    }

    fn note_late(&mut self, now: Instant) {
        self.prune_late(now);
        // Judged on the events that were already there: one reordered packet is
        // not jitter, a second one within the window is.
        let jittering = self
            .late_events
            .iter()
            .any(|at| now.saturating_duration_since(*at) <= LATE_GROW_WINDOW);
        self.late_events.push_back(now);
        if jittering && self.target_ms < MAX_TARGET_MS {
            self.target_ms = (self.target_ms + STEP_MS).min(MAX_TARGET_MS);
        }
    }

    fn prune_late(&mut self, now: Instant) {
        while self
            .late_events
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) > LATE_SHRINK_WINDOW)
        {
            self.late_events.pop_front();
        }
    }
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(seq: u64, marker: bool) -> Incoming {
        Incoming {
            seq,
            ts: (seq as u32).wrapping_mul(960),
            marker,
            payload: vec![seq as u8],
        }
    }

    /// A packet whose media timestamp is set independently of its sequence
    /// number, so a hole can be made of pings, of lost audio, or of both.
    fn frame(seq: u64, ts: u32, marker: bool) -> Incoming {
        Incoming {
            seq,
            ts,
            marker,
            payload: vec![seq as u8],
        }
    }

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn buffers_to_the_target_then_plays_in_order() {
        let t0 = Instant::now();
        let mut buffer = JitterBuffer::new();
        assert_eq!(buffer.target_ms(), MIN_TARGET_MS);

        buffer.push(t0, packet(0, true));
        assert_eq!(buffer.pull(t0), Frame::Idle);
        buffer.push(at(t0, 20), packet(1, false));
        assert_eq!(buffer.pull(at(t0, 20)), Frame::Idle);
        buffer.push(at(t0, 40), packet(2, false));

        assert_eq!(buffer.pull(at(t0, 40)), Frame::Packet(vec![0]));
        assert_eq!(buffer.pull(at(t0, 60)), Frame::Packet(vec![1]));
        assert_eq!(buffer.pull(at(t0, 80)), Frame::Packet(vec![2]));
        assert_eq!(buffer.stats().received, 3);
        assert_eq!(buffer.stats().lost, 0);
    }

    #[test]
    fn starts_playing_on_the_deadline_even_when_starved() {
        let t0 = Instant::now();
        let mut buffer = JitterBuffer::new();
        buffer.push(t0, packet(0, true));
        assert_eq!(buffer.pull(at(t0, 40)), Frame::Idle);
        assert_eq!(buffer.pull(at(t0, 60)), Frame::Packet(vec![0]));
    }

    #[test]
    fn a_hole_is_reported_lost_and_the_stream_continues() {
        let t0 = Instant::now();
        let mut buffer = JitterBuffer::new();
        for seq in [0, 1, 3] {
            buffer.push(t0, packet(seq, seq == 0));
        }
        assert_eq!(buffer.pull(t0), Frame::Packet(vec![0]));
        assert_eq!(buffer.pull(at(t0, 20)), Frame::Packet(vec![1]));
        assert_eq!(buffer.pull(at(t0, 40)), Frame::Lost);
        assert_eq!(buffer.pull(at(t0, 60)), Frame::Packet(vec![3]));
        assert_eq!(buffer.stats().lost, 1);
        assert_eq!(buffer.stats().concealed, 1);
    }

    #[test]
    fn ping_hole_is_not_a_loss() {
        let t0 = Instant::now();
        let mut buffer = JitterBuffer::new();
        buffer.push(t0, frame(0, 0, true));
        buffer.push(t0, frame(1, 960, false));
        buffer.push(t0, frame(2, 1_920, false));
        // Seq 3 went to a keepalive ping: the timestamps are still contiguous,
        // so no audio is missing.
        buffer.push(t0, frame(4, 2_880, false));

        assert_eq!(buffer.pull(t0), Frame::Packet(vec![0]));
        assert_eq!(buffer.pull(at(t0, 20)), Frame::Packet(vec![1]));
        assert_eq!(buffer.pull(at(t0, 40)), Frame::Packet(vec![2]));
        assert_eq!(buffer.pull(at(t0, 60)), Frame::Packet(vec![4]));
        assert_eq!(buffer.stats().lost, 0);
        assert_eq!(buffer.stats().concealed, 0);
    }

    #[test]
    fn real_hole_is_concealed_once() {
        let t0 = Instant::now();
        let mut buffer = JitterBuffer::new();
        buffer.push(t0, frame(0, 0, true));
        buffer.push(t0, frame(1, 960, false));
        // Seq 2 carried the frame at ts 1_920 and never arrived.
        buffer.push(t0, frame(3, 2_880, false));

        assert_eq!(buffer.pull(t0), Frame::Packet(vec![0]));
        assert_eq!(buffer.pull(at(t0, 20)), Frame::Packet(vec![1]));
        assert_eq!(buffer.pull(at(t0, 40)), Frame::Lost);
        assert_eq!(buffer.pull(at(t0, 60)), Frame::Packet(vec![3]));
        assert_eq!(buffer.stats().lost, 1);
        assert_eq!(buffer.stats().concealed, 1);
    }

    #[test]
    fn mixed_hole_conceals_only_the_missing_audio() {
        let t0 = Instant::now();
        let mut buffer = JitterBuffer::new();
        buffer.push(t0, frame(0, 0, true));
        // Seq 1 was the lost frame at ts 960 and seq 2 was a ping.
        buffer.push(t0, frame(3, 1_920, false));

        assert_eq!(buffer.pull(at(t0, 60)), Frame::Packet(vec![0]));
        assert_eq!(buffer.pull(at(t0, 80)), Frame::Lost);
        assert_eq!(buffer.pull(at(t0, 100)), Frame::Packet(vec![3]));
        assert_eq!(buffer.stats().lost, 1);
        assert_eq!(buffer.stats().concealed, 1);
    }

    #[test]
    fn underrun_does_not_drop_the_late_packet() {
        let t0 = Instant::now();
        let mut buffer = JitterBuffer::new();
        for seq in 0..3 {
            buffer.push(t0, packet(seq, seq == 0));
        }
        for _ in 0..3 {
            assert!(matches!(buffer.pull(t0), Frame::Packet(_)));
        }

        // The buffer runs dry and conceals, but keeps the slot open.
        assert_eq!(buffer.pull(at(t0, 20)), Frame::Lost);
        assert_eq!(buffer.stats().concealed, 1);
        assert_eq!(buffer.stats().lost, 0);

        buffer.push(at(t0, 30), frame(3, 2_880, false));
        assert_eq!(buffer.pull(at(t0, 40)), Frame::Packet(vec![3]));
        assert_eq!(buffer.stats().late, 0);
        assert_eq!(buffer.stats().lost, 0);
    }

    #[test]
    fn reordered_packets_come_out_in_order() {
        let t0 = Instant::now();
        let mut buffer = JitterBuffer::new();
        buffer.push(t0, packet(0, true));
        buffer.push(at(t0, 5), packet(2, false));
        buffer.push(at(t0, 10), packet(1, false));

        assert_eq!(buffer.pull(at(t0, 10)), Frame::Packet(vec![0]));
        assert_eq!(buffer.pull(at(t0, 30)), Frame::Packet(vec![1]));
        assert_eq!(buffer.pull(at(t0, 50)), Frame::Packet(vec![2]));
        assert_eq!(buffer.stats().late, 0);
    }

    #[test]
    fn a_packet_past_its_slot_is_counted_late_and_dropped() {
        let t0 = Instant::now();
        let mut buffer = JitterBuffer::new();
        for seq in 0..3 {
            buffer.push(t0, packet(seq, seq == 0));
        }
        for _ in 0..3 {
            assert!(matches!(buffer.pull(t0), Frame::Packet(_)));
        }

        buffer.push(at(t0, 60), packet(1, false));
        assert_eq!(buffer.stats().late, 1);
        assert_eq!(buffer.pull(at(t0, 60)), Frame::Lost);
    }

    #[test]
    fn duplicates_are_counted_and_ignored() {
        let t0 = Instant::now();
        let mut buffer = JitterBuffer::new();
        buffer.push(t0, packet(0, true));
        buffer.push(t0, packet(0, false));
        buffer.push(t0, packet(1, false));
        buffer.push(t0, packet(2, false));

        assert_eq!(buffer.stats().duplicates, 1);
        assert_eq!(buffer.pull(t0), Frame::Packet(vec![0]));
        assert_eq!(buffer.pull(at(t0, 20)), Frame::Packet(vec![1]));
        assert_eq!(buffer.pull(at(t0, 40)), Frame::Packet(vec![2]));
    }

    #[test]
    fn the_target_grows_on_late_packets_and_shrinks_on_a_clean_spurt_end() {
        let t0 = Instant::now();
        let mut buffer = JitterBuffer::new();
        for seq in 0..3 {
            buffer.push(t0, packet(seq, seq == 0));
        }
        for _ in 0..3 {
            assert!(matches!(buffer.pull(t0), Frame::Packet(_)));
        }

        // One reordered packet is not jitter, so the first late arrival leaves
        // the target alone; a second one inside the window grows it.
        buffer.push(at(t0, 60), packet(1, false));
        assert_eq!(buffer.target_ms(), MIN_TARGET_MS);
        buffer.push(at(t0, 70), packet(0, false));
        assert_eq!(buffer.target_ms(), MIN_TARGET_MS + STEP_MS);
        buffer.push(at(t0, 80), packet(0, false));
        assert_eq!(buffer.target_ms(), MIN_TARGET_MS + 2 * STEP_MS);
        buffer.push(at(t0, 90), packet(0, false));
        assert_eq!(buffer.target_ms(), MAX_TARGET_MS, "the target is capped");

        // Long after the last late arrival the spurt ends clean, so the target
        // walks back down one step.
        assert_eq!(buffer.pull(at(t0, 11_000)), Frame::Idle);
        assert_eq!(buffer.target_ms(), MAX_TARGET_MS - STEP_MS);
    }

    #[test]
    fn a_long_silence_ends_the_spurt() {
        let t0 = Instant::now();
        let mut buffer = JitterBuffer::new();
        for seq in 0..3 {
            buffer.push(t0, packet(seq, seq == 0));
        }
        for _ in 0..3 {
            assert!(matches!(buffer.pull(t0), Frame::Packet(_)));
        }

        assert_eq!(buffer.pull(at(t0, 20)), Frame::Lost);
        assert_eq!(buffer.pull(at(t0, 2_000)), Frame::Idle);
        // Idle stays idle until the talker comes back.
        assert_eq!(buffer.pull(at(t0, 2_020)), Frame::Idle);
    }

    #[test]
    fn concealment_stops_after_five_frames() {
        let t0 = Instant::now();
        let mut buffer = JitterBuffer::new();
        for seq in 0..3 {
            buffer.push(t0, packet(seq, seq == 0));
        }
        for _ in 0..3 {
            assert!(matches!(buffer.pull(t0), Frame::Packet(_)));
        }
        for step in 0..5 {
            assert_eq!(buffer.pull(at(t0, 20 + step * 20)), Frame::Lost);
        }
        assert_eq!(buffer.pull(at(t0, 120)), Frame::Idle);
        assert_eq!(buffer.stats().concealed, 5);
        // Nothing later ever arrived, so no timestamp proves a frame was lost.
        assert_eq!(buffer.stats().lost, 0);
    }

    #[test]
    fn a_timestamp_jump_starts_a_new_spurt() {
        let t0 = Instant::now();
        let mut buffer = JitterBuffer::new();
        for seq in 0..3 {
            buffer.push(t0, packet(seq, seq == 0));
        }
        for _ in 0..3 {
            assert!(matches!(buffer.pull(t0), Frame::Packet(_)));
        }

        let jumped = Incoming {
            seq: 3,
            ts: 2 * 960 + TS_JUMP_SAMPLES + 960,
            marker: false,
            payload: vec![3],
        };
        buffer.push(at(t0, 20), jumped);
        assert_eq!(buffer.pull(at(t0, 20)), Frame::Idle, "rebuffering");

        buffer.push(at(t0, 40), packet(4, false));
        buffer.push(at(t0, 60), packet(5, false));
        assert_eq!(buffer.pull(at(t0, 60)), Frame::Packet(vec![3]));
    }

    #[test]
    fn a_burst_deeper_than_the_bound_is_trimmed() {
        let t0 = Instant::now();
        let mut buffer = JitterBuffer::new();
        for seq in 0..12 {
            buffer.push(t0, packet(seq, seq == 0));
        }
        // A 60 ms target trims back to 3 frames the moment the depth passes 8,
        // which happens once on the ninth packet and drops seqs 0..=5.
        assert_eq!(buffer.stats().late, 6);
        assert_eq!(buffer.pull(t0), Frame::Packet(vec![6]));
        assert_eq!(buffer.pull(at(t0, 20)), Frame::Packet(vec![7]));
        assert_eq!(buffer.pull(at(t0, 40)), Frame::Packet(vec![8]));
        assert_eq!(buffer.pull(at(t0, 60)), Frame::Packet(vec![9]));
    }
}
