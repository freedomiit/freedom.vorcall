//! Video on the media socket: one encoded access unit split over as many
//! datagrams as it takes, and put back together on the other side.
//!
//! A screen share and a camera are framed identically and differ only in the
//! packet type that carries them, so everything here serves both; the streams
//! stay apart because each has a [`Depacketizer`] of its own.
//!
//! Each fragment's plaintext is a 9-byte header followed by its slice of the
//! access unit, big-endian:
//!
//! ```text
//! [0..4]  frame_id u32  one per access unit, wrapping
//! [4..6]  index    u16  0-based
//! [6..8]  count    u16  fragments in this access unit, >= 1
//! [8]     flags    u8   bit 0 = keyframe
//! [9..]   data          up to MAX_VIDEO_DATA bytes
//! ```
//!
//! The decoder downstream can only carry on from a keyframe, so the
//! [`Depacketizer`] refuses to hand it a unit that follows a hole: it raises
//! [`Depacketizer::needs_keyframe`] instead, and the engine turns that into a
//! keyframe request to the sharer.

use std::fmt;
use std::time::{Duration, Instant};

use crate::packet::{HEADER_LEN, MAX_DATAGRAM, PacketError, TAG_LEN};

pub const VIDEO_HEADER_LEN: usize = 9;
/// What is left of a datagram once the media header, the tag and the fragment
/// header have taken their share.
pub const MAX_VIDEO_DATA: usize = MAX_DATAGRAM - HEADER_LEN - TAG_LEN - VIDEO_HEADER_LEN;
/// A 4 MiB access unit is already far past anything a screen encoder emits; a
/// larger one is a lie from the wire and is refused at both ends.
pub const MAX_ACCESS_UNIT: usize = 4 * 1024 * 1024;
/// A frame whose first fragment is older than this will never be completed.
pub const FRAME_TIMEOUT: Duration = Duration::from_millis(300);

const KEYFRAME_FLAG: u8 = 0b0000_0001;
/// The most fragments a valid access unit can be split into.
const MAX_FRAGMENTS: usize = MAX_ACCESS_UNIT.div_ceil(MAX_VIDEO_DATA);
/// Frames being reassembled at once. Reordering spans a frame or two; anything
/// beyond that is a frame that will never complete.
const MAX_PENDING: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FragmentHeader {
    pub frame_id: u32,
    pub index: u16,
    pub count: u16,
    pub keyframe: bool,
}

impl FragmentHeader {
    pub fn encode(&self) -> [u8; VIDEO_HEADER_LEN] {
        let mut out = [0u8; VIDEO_HEADER_LEN];
        out[0..4].copy_from_slice(&self.frame_id.to_be_bytes());
        out[4..6].copy_from_slice(&self.index.to_be_bytes());
        out[6..8].copy_from_slice(&self.count.to_be_bytes());
        out[8] = if self.keyframe { KEYFRAME_FLAG } else { 0 };
        out
    }

    /// Splits a fragment's plaintext into its header and its slice of the
    /// access unit.
    pub fn decode(bytes: &[u8]) -> Result<(FragmentHeader, &[u8]), PacketError> {
        if bytes.len() < VIDEO_HEADER_LEN {
            return Err(PacketError::TooShort);
        }
        let flags = bytes[8];
        if flags & !KEYFRAME_FLAG != 0 {
            return Err(PacketError::BadFlags(flags));
        }
        let index = u16::from_be_bytes([bytes[4], bytes[5]]);
        let count = u16::from_be_bytes([bytes[6], bytes[7]]);
        if count == 0 || index >= count {
            return Err(PacketError::BadFragment { index, count });
        }
        Ok((
            FragmentHeader {
                frame_id: u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
                index,
                count,
                keyframe: flags & KEYFRAME_FLAG != 0,
            },
            &bytes[VIDEO_HEADER_LEN..],
        ))
    }
}

/// Cuts one access unit into the fragments that carry it, without copying it.
pub fn fragments(
    frame_id: u32,
    keyframe: bool,
    data: &[u8],
) -> Result<impl Iterator<Item = (FragmentHeader, &[u8])>, PacketError> {
    if data.is_empty() {
        return Err(PacketError::TooShort);
    }
    if data.len() > MAX_ACCESS_UNIT {
        return Err(PacketError::TooLong);
    }
    // Bounded by MAX_FRAGMENTS, which is far below u16::MAX.
    let count = data.len().div_ceil(MAX_VIDEO_DATA) as u16;
    Ok(data
        .chunks(MAX_VIDEO_DATA)
        .enumerate()
        .map(move |(index, chunk)| {
            (
                FragmentHeader {
                    frame_id,
                    index: index as u16,
                    count,
                    keyframe,
                },
                chunk,
            )
        }))
}

/// One reassembled encoded frame, ready for the decoder.
pub struct AccessUnit {
    pub frame_id: u32,
    pub keyframe: bool,
    /// The sender's 48 kHz clock at capture, from the media header.
    pub ts: u32,
    pub data: Vec<u8>,
    pub completed: Instant,
}

impl fmt::Debug for AccessUnit {
    /// Sizes only: the payload is somebody's screen.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessUnit")
            .field("frame_id", &self.frame_id)
            .field("keyframe", &self.keyframe)
            .field("len", &self.data.len())
            .finish()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VideoStats {
    /// Access units delivered to the decoder.
    pub frames: u64,
    pub keyframes: u64,
    /// Access units thrown away: incomplete, oversized, or gated behind a
    /// keyframe that has not arrived yet.
    pub dropped: u64,
    pub fragments: u64,
    pub duplicates: u64,
    pub bytes: u64,
    /// Filled in by the engine, which is what sends the requests.
    pub keyframe_requests: u64,
}

struct Pending {
    frame_id: u32,
    count: u16,
    keyframe: bool,
    ts: u32,
    first_seen: Instant,
    parts: Vec<Option<Vec<u8>>>,
    have: u16,
    bytes: usize,
}

/// Reassembles one video stream. One per watched ssrc and per stream kind: a
/// new sharer or camera gets a new depacketizer rather than this one's history.
pub struct Depacketizer {
    pending: Vec<Pending>,
    /// The newest frame id that has been decided on, delivered or dropped.
    last_decided: Option<u32>,
    needs_keyframe: bool,
    stats: VideoStats,
}

impl Depacketizer {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            last_decided: None,
            // Nothing before the first keyframe can be decoded.
            needs_keyframe: true,
            stats: VideoStats::default(),
        }
    }

    /// Delivers an access unit when this fragment completes it.
    ///
    /// Delivery is in frame id order. An incomplete older frame is discarded
    /// once a newer one completes or once its first fragment is older than
    /// [`FRAME_TIMEOUT`]; a discard, like a completed frame whose predecessor
    /// never arrived, raises [`needs_keyframe`](Self::needs_keyframe). While
    /// that flag is up, completed frames that are not keyframes are dropped
    /// instead of delivered, because the decoder cannot use them.
    pub fn push(
        &mut self,
        now: Instant,
        ts: u32,
        header: FragmentHeader,
        data: &[u8],
    ) -> Option<AccessUnit> {
        self.stats.fragments += 1;
        self.stats.bytes += data.len() as u64;
        self.expire(now);

        // Anything at or behind the last decision is a straggler: that frame
        // has already been delivered or given up on.
        if self
            .last_decided
            .is_some_and(|last| !is_newer(header.frame_id, last))
        {
            self.stats.duplicates += 1;
            return None;
        }

        let slot = match self
            .pending
            .iter()
            .position(|frame| frame.frame_id == header.frame_id)
        {
            Some(slot) => {
                // Two fragments of one frame disagreeing about its size means
                // one of them is not what it claims to be.
                if self.pending[slot].count != header.count {
                    self.discard(slot);
                    return None;
                }
                slot
            }
            None => {
                if usize::from(header.count) > MAX_FRAGMENTS {
                    self.stats.dropped += 1;
                    self.needs_keyframe = true;
                    return None;
                }
                if self.pending.len() >= MAX_PENDING
                    && let Some(oldest) = self.oldest()
                {
                    self.discard(oldest);
                }
                self.pending.push(Pending {
                    frame_id: header.frame_id,
                    count: header.count,
                    keyframe: header.keyframe,
                    ts,
                    first_seen: now,
                    parts: vec![None; usize::from(header.count)],
                    have: 0,
                    bytes: 0,
                });
                self.pending.len() - 1
            }
        };

        let frame = &mut self.pending[slot];
        frame.keyframe |= header.keyframe;
        if frame.parts[usize::from(header.index)].is_some() {
            self.stats.duplicates += 1;
            return None;
        }
        frame.bytes += data.len();
        if frame.bytes > MAX_ACCESS_UNIT {
            self.discard(slot);
            return None;
        }
        frame.parts[usize::from(header.index)] = Some(data.to_vec());
        frame.have += 1;
        if frame.have < frame.count {
            return None;
        }

        let frame = self.pending.swap_remove(slot);
        // Read before the discards below move it along.
        let previous = self.last_decided;
        // Whatever is still waiting behind a completed frame is never coming.
        while let Some(stale) = self
            .pending
            .iter()
            .position(|pending| !is_newer(pending.frame_id, frame.frame_id))
        {
            self.discard(stale);
        }

        if previous.is_none_or(|last| frame.frame_id != last.wrapping_add(1)) {
            self.needs_keyframe = true;
        }
        self.last_decided = Some(frame.frame_id);

        if self.needs_keyframe && !frame.keyframe {
            self.stats.dropped += 1;
            return None;
        }
        self.needs_keyframe = false;
        self.stats.frames += 1;
        if frame.keyframe {
            self.stats.keyframes += 1;
        }

        let mut data = Vec::with_capacity(frame.bytes);
        for part in frame.parts.into_iter().flatten() {
            data.extend_from_slice(&part);
        }
        Some(AccessUnit {
            frame_id: frame.frame_id,
            keyframe: frame.keyframe,
            ts: frame.ts,
            data,
            completed: now,
        })
    }

    pub fn needs_keyframe(&self) -> bool {
        self.needs_keyframe
    }

    pub fn stats(&self) -> VideoStats {
        self.stats.clone()
    }

    /// Gives up on a frame that is still in pieces.
    fn discard(&mut self, slot: usize) {
        let frame = self.pending.swap_remove(slot);
        self.stats.dropped += 1;
        self.needs_keyframe = true;
        if self
            .last_decided
            .is_none_or(|last| is_newer(frame.frame_id, last))
        {
            self.last_decided = Some(frame.frame_id);
        }
    }

    fn expire(&mut self, now: Instant) {
        while let Some(slot) = self
            .pending
            .iter()
            .position(|frame| now.saturating_duration_since(frame.first_seen) > FRAME_TIMEOUT)
        {
            self.discard(slot);
        }
    }

    fn oldest(&self) -> Option<usize> {
        self.pending
            .iter()
            .enumerate()
            .min_by_key(|(_, frame)| frame.first_seen)
            .map(|(slot, _)| slot)
    }
}

impl Default for Depacketizer {
    fn default() -> Self {
        Self::new()
    }
}

/// Frame ids wrap, so "newer" is the signed distance between them.
fn is_newer(frame_id: u32, than: u32) -> bool {
    (frame_id.wrapping_sub(than) as i32) > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    fn header(frame_id: u32, index: u16, count: u16, keyframe: bool) -> FragmentHeader {
        FragmentHeader {
            frame_id,
            index,
            count,
            keyframe,
        }
    }

    /// The `n`th fragment of a synthetic unit, distinct per frame and index.
    fn payload(frame_id: u32, index: u16) -> Vec<u8> {
        vec![(frame_id as u8).wrapping_add(index as u8); 16]
    }

    /// Pushes every fragment of a `count`-fragment frame, in `order`.
    fn push_frame(
        depacketizer: &mut Depacketizer,
        now: Instant,
        frame_id: u32,
        count: u16,
        keyframe: bool,
        order: impl IntoIterator<Item = u16>,
    ) -> Option<AccessUnit> {
        let mut delivered = None;
        for index in order {
            let unit = depacketizer.push(
                now,
                frame_id.wrapping_mul(960),
                header(frame_id, index, count, keyframe),
                &payload(frame_id, index),
            );
            if unit.is_some() {
                delivered = unit;
            }
        }
        delivered
    }

    #[test]
    fn a_fragment_header_round_trips() {
        let original = header(0xDEAD_BEEF, 3, 9, true);
        let mut bytes = original.encode().to_vec();
        bytes.extend_from_slice(b"payload");
        let (decoded, data) = FragmentHeader::decode(&bytes).expect("decodes");
        assert_eq!(decoded, original);
        assert_eq!(data, b"payload");

        let plain = header(1, 0, 1, false);
        let bytes = plain.encode();
        let (decoded, data) = FragmentHeader::decode(&bytes).expect("decodes");
        assert_eq!(decoded, plain);
        assert!(data.is_empty());
    }

    #[test]
    fn a_fragment_header_rejects_reserved_flags_and_impossible_counts() {
        let mut bytes = header(1, 0, 1, true).encode();
        bytes[8] = 0b0000_0010;
        assert!(matches!(
            FragmentHeader::decode(&bytes),
            Err(PacketError::BadFlags(0b0000_0010))
        ));

        let mut bytes = header(1, 0, 1, false).encode();
        bytes[6..8].copy_from_slice(&0u16.to_be_bytes());
        assert!(matches!(
            FragmentHeader::decode(&bytes),
            Err(PacketError::BadFragment { index: 0, count: 0 })
        ));

        let bytes = header(1, 4, 4, false).encode();
        assert!(matches!(
            FragmentHeader::decode(&bytes),
            Err(PacketError::BadFragment { index: 4, count: 4 })
        ));

        assert!(matches!(
            FragmentHeader::decode(&bytes[..VIDEO_HEADER_LEN - 1]),
            Err(PacketError::TooShort)
        ));
    }

    #[test]
    fn an_access_unit_is_cut_into_full_datagrams() {
        assert_eq!(MAX_VIDEO_DATA, 1156);

        assert!(matches!(
            fragments(1, true, &[]),
            Err(PacketError::TooShort)
        ));
        assert!(matches!(
            fragments(1, true, &vec![0u8; MAX_ACCESS_UNIT + 1]),
            Err(PacketError::TooLong)
        ));

        for (len, expected) in [
            (1usize, 1u16),
            (MAX_VIDEO_DATA, 1),
            (MAX_VIDEO_DATA + 1, 2),
            (3 * MAX_VIDEO_DATA + 1, 4),
        ] {
            let unit: Vec<u8> = (0..len).map(|index| index as u8).collect();
            let cut: Vec<(FragmentHeader, &[u8])> =
                fragments(7, true, &unit).expect("fragments").collect();
            assert_eq!(cut.len(), usize::from(expected), "{len} bytes");

            let mut rejoined = Vec::new();
            for (index, (header, data)) in cut.iter().enumerate() {
                assert_eq!(header.frame_id, 7);
                assert_eq!(header.index, index as u16);
                assert_eq!(header.count, expected);
                assert!(header.keyframe);
                assert!(data.len() <= MAX_VIDEO_DATA);
                rejoined.extend_from_slice(data);
            }
            assert_eq!(rejoined, unit);
        }
    }

    #[test]
    fn frames_are_delivered_in_order_once_a_keyframe_has_arrived() {
        let base = Instant::now();
        let mut depacketizer = Depacketizer::new();
        assert!(depacketizer.needs_keyframe());

        let unit = push_frame(&mut depacketizer, base, 1, 2, true, [0, 1]).expect("delivered");
        assert_eq!(unit.frame_id, 1);
        assert!(unit.keyframe);
        assert_eq!(unit.ts, 960);
        assert_eq!(unit.data, [payload(1, 0), payload(1, 1)].concat());
        assert!(!depacketizer.needs_keyframe());

        // Reordered within the frame, and still delivered whole.
        let unit =
            push_frame(&mut depacketizer, at(base, 20), 2, 3, false, [2, 0, 1]).expect("delivered");
        assert_eq!(unit.frame_id, 2);
        assert!(!unit.keyframe);
        assert_eq!(
            unit.data,
            [payload(2, 0), payload(2, 1), payload(2, 2)].concat()
        );

        let stats = depacketizer.stats();
        assert_eq!(stats.frames, 2);
        assert_eq!(stats.keyframes, 1);
        assert_eq!(stats.fragments, 5);
        assert_eq!(stats.dropped, 0);
        assert_eq!(stats.duplicates, 0);
        assert_eq!(stats.bytes, 5 * 16);
        assert_eq!(stats.keyframe_requests, 0);
    }

    #[test]
    fn duplicate_and_late_fragments_are_counted_and_ignored() {
        let base = Instant::now();
        let mut depacketizer = Depacketizer::new();
        push_frame(&mut depacketizer, base, 1, 2, true, [0, 1]).expect("delivered");

        // A fragment of a frame that has already been delivered.
        assert!(
            depacketizer
                .push(base, 960, header(1, 0, 2, true), &payload(1, 0))
                .is_none()
        );
        // A repeat within a frame still being assembled.
        assert!(
            depacketizer
                .push(base, 1_920, header(2, 0, 2, false), &payload(2, 0))
                .is_none()
        );
        assert!(
            depacketizer
                .push(base, 1_920, header(2, 0, 2, false), &payload(2, 0))
                .is_none()
        );
        let unit = depacketizer
            .push(base, 1_920, header(2, 1, 2, false), &payload(2, 1))
            .expect("delivered");
        assert_eq!(unit.data, [payload(2, 0), payload(2, 1)].concat());

        let stats = depacketizer.stats();
        assert_eq!(stats.duplicates, 2);
        assert_eq!(stats.frames, 2);
        assert_eq!(stats.dropped, 0);
    }

    #[test]
    fn a_hole_gates_everything_until_the_next_keyframe() {
        let base = Instant::now();
        let mut depacketizer = Depacketizer::new();
        push_frame(&mut depacketizer, base, 1, 1, true, [0]).expect("delivered");

        // Frame 2 loses its middle fragment; frame 3 completing gives up on it.
        depacketizer.push(at(base, 20), 1_920, header(2, 0, 3, false), &payload(2, 0));
        depacketizer.push(at(base, 20), 1_920, header(2, 2, 3, false), &payload(2, 2));
        assert!(!depacketizer.needs_keyframe());

        assert!(push_frame(&mut depacketizer, at(base, 40), 3, 1, false, [0]).is_none());
        assert!(depacketizer.needs_keyframe());

        // A plain frame cannot restart the decoder, a keyframe can.
        assert!(push_frame(&mut depacketizer, at(base, 60), 4, 1, false, [0]).is_none());
        let unit = push_frame(&mut depacketizer, at(base, 80), 5, 1, true, [0]).expect("delivered");
        assert_eq!(unit.frame_id, 5);
        assert!(!depacketizer.needs_keyframe());

        let stats = depacketizer.stats();
        assert_eq!(stats.frames, 2);
        assert_eq!(stats.keyframes, 2);
        // Frame 2 abandoned, frames 3 and 4 gated.
        assert_eq!(stats.dropped, 3);
    }

    #[test]
    fn an_unfinished_frame_is_given_up_on_after_the_timeout() {
        let base = Instant::now();
        let mut depacketizer = Depacketizer::new();
        push_frame(&mut depacketizer, base, 1, 1, true, [0]).expect("delivered");

        depacketizer.push(at(base, 20), 1_920, header(2, 0, 2, false), &payload(2, 0));
        assert!(!depacketizer.needs_keyframe());

        // The second fragment arrives after the frame has been written off.
        let late = at(base, 20) + FRAME_TIMEOUT + Duration::from_millis(1);
        assert!(
            depacketizer
                .push(late, 1_920, header(2, 1, 2, false), &payload(2, 1))
                .is_none()
        );
        assert!(depacketizer.needs_keyframe());
        assert_eq!(depacketizer.stats().dropped, 1);
        assert_eq!(depacketizer.stats().duplicates, 1);
    }

    #[test]
    fn frame_ids_wrap_past_u32_max() {
        let base = Instant::now();
        let mut depacketizer = Depacketizer::new();

        let unit = push_frame(&mut depacketizer, base, u32::MAX - 1, 1, true, [0]).expect("keyed");
        assert_eq!(unit.frame_id, u32::MAX - 1);
        let unit = push_frame(&mut depacketizer, base, u32::MAX, 1, false, [0]).expect("delivered");
        assert_eq!(unit.frame_id, u32::MAX);
        let unit = push_frame(&mut depacketizer, base, 0, 1, false, [0]).expect("delivered");
        assert_eq!(unit.frame_id, 0);
        let unit = push_frame(&mut depacketizer, base, 1, 1, false, [0]).expect("delivered");
        assert_eq!(unit.frame_id, 1);

        // The frame before the wrap is now a straggler, not a new frame.
        assert!(
            depacketizer
                .push(
                    base,
                    0,
                    header(u32::MAX, 0, 1, false),
                    &payload(u32::MAX, 0)
                )
                .is_none()
        );
        assert_eq!(depacketizer.stats().frames, 4);
        assert_eq!(depacketizer.stats().duplicates, 1);
        assert!(!depacketizer.needs_keyframe());
    }

    #[test]
    fn only_eight_frames_are_assembled_at_once() {
        let base = Instant::now();
        let mut depacketizer = Depacketizer::new();
        push_frame(&mut depacketizer, base, 1, 1, true, [0]).expect("delivered");

        // Nine frames, each missing its second fragment.
        for frame_id in 2..=10u32 {
            depacketizer.push(
                at(base, u64::from(frame_id)),
                frame_id * 960,
                header(frame_id, 0, 2, false),
                &payload(frame_id, 0),
            );
        }
        assert_eq!(depacketizer.stats().dropped, 1);
        assert!(depacketizer.needs_keyframe());

        // Frame 2 was the one evicted, so its tail is a straggler now.
        assert!(
            depacketizer
                .push(at(base, 20), 1_920, header(2, 1, 2, false), &payload(2, 1))
                .is_none()
        );
        assert_eq!(depacketizer.stats().duplicates, 1);
        // Frame 3 is still being assembled and completes.
        assert!(
            depacketizer
                .push(at(base, 20), 2_880, header(3, 1, 2, false), &payload(3, 1))
                .is_none(),
            "a non-keyframe must stay gated"
        );
        assert_eq!(depacketizer.stats().frames, 1);
    }

    #[test]
    fn fragments_that_disagree_about_the_count_discard_the_frame() {
        let base = Instant::now();
        let mut depacketizer = Depacketizer::new();
        push_frame(&mut depacketizer, base, 1, 1, true, [0]).expect("delivered");

        depacketizer.push(base, 1_920, header(2, 0, 2, false), &payload(2, 0));
        assert!(
            depacketizer
                .push(base, 1_920, header(2, 1, 3, false), &payload(2, 1))
                .is_none()
        );
        assert_eq!(depacketizer.stats().dropped, 1);
        assert!(depacketizer.needs_keyframe());
    }
}
