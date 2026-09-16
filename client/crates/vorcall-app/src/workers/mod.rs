//! The threads and blocking work the interface must never do itself: the audio
//! devices, the screen-share and camera pipelines and their decoders, the image
//! and soundpad clip caches, importing a picked audio file, desktop notifications
//! and the chime.
//!
//! Every worker is reached the same way — a handle that never blocks goes in, an
//! event stream comes out — and every handle's `Drop` is what stops its thread.

pub mod audio;
pub mod camera;
pub mod clips;
pub mod images;
pub mod notify;
pub mod share;
pub mod sounds;
pub mod video;
pub mod voice;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

/// The latest value a worker has for the interface, and nothing older.
///
/// A producer faster than the interface can redraw — the 10 Hz input level, a
/// decoded picture per frame — must never queue more than one message: while the
/// window is hidden the interface stops draining, and an unbounded stream of
/// messages that each cost a full rebuild turns into seconds of catching up on
/// state that is stale by then. The value lives here and only a marker travels,
/// so the queue holds at most one of them and the interface always reads the
/// newest value.
pub struct Mailbox<T> {
    slot: Mutex<Option<T>>,
    /// Whether a marker for this mailbox is already on its way.
    pending: AtomicBool,
}

/// What one post did: whether a marker still has to travel for it, and whether
/// it wrote over a value the interface never read. The two are independent — a
/// marker can be in flight for a slot that a take has already emptied, and a
/// post into that empty slot then loses nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Posted {
    pub marker: bool,
    pub replaced: bool,
}

impl<T> Default for Mailbox<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Mailbox<T> {
    pub fn new() -> Self {
        Self {
            slot: Mutex::new(None),
            pending: AtomicBool::new(false),
        }
    }

    /// Stores the latest value and reports what that did: `marker` is whether a
    /// marker still has to be sent for it, `replaced` whether it wrote over a
    /// value the interface never read — a lost value, which a caller counting
    /// dropped work counts here and nowhere else.
    ///
    /// The slot is written before the flag is raised, the mirror of `take`.
    pub fn post(&self, value: T) -> Posted {
        let replaced = lock(&self.slot).replace(value).is_some();
        let marker = !self.pending.swap(true, Ordering::AcqRel);
        Posted { marker, replaced }
    }

    /// Takes the latest value, if there is one.
    ///
    /// The marker is cleared first and the value read second, never the other way
    /// around: a post landing between a taken value and a still-set flag would
    /// see the flag and send nothing, stranding its value until some later post
    /// rescued it — and the last picture of a share has no later post. With the
    /// flag cleared first, every post from then on sends a marker of its own, and
    /// its value is either returned by this very take (the marker then finds an
    /// empty slot, which the reader treats as nothing to do) or waits for it.
    pub fn take(&self) -> Option<T> {
        self.pending.store(false, Ordering::Release);
        lock(&self.slot).take()
    }

    /// The state a post reaches when a take slipped in between its two steps.
    #[cfg(test)]
    fn set_pending_for_test(&self) {
        self.pending.store(true, Ordering::Release);
    }
}

/// A poisoned lock still holds a usable value — a ring, a playout, a mailbox —
/// and losing the call or what the interface draws over it would be worse than
/// carrying on.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::{Mailbox, Posted};

    #[test]
    fn only_the_first_post_asks_for_a_marker() {
        let mailbox = Mailbox::new();
        assert!(mailbox.post(1).marker);
        assert!(!mailbox.post(2).marker);
        assert_eq!(mailbox.take(), Some(2));
    }

    #[test]
    fn a_post_after_a_take_asks_for_a_marker_again() {
        let mailbox = Mailbox::new();
        assert!(mailbox.post(1).marker);
        assert_eq!(mailbox.take(), Some(1));
        assert!(mailbox.post(2).marker);
    }

    #[test]
    fn a_post_over_an_unread_value_reports_it_replaced() {
        let mailbox = Mailbox::new();
        assert_eq!(
            mailbox.post(1),
            Posted {
                marker: true,
                replaced: false,
            }
        );
        assert_eq!(
            mailbox.post(2),
            Posted {
                marker: false,
                replaced: true,
            }
        );
        assert_eq!(mailbox.take(), Some(2));
        assert_eq!(
            mailbox.post(3),
            Posted {
                marker: true,
                replaced: false,
            }
        );
    }

    #[test]
    fn a_post_into_a_slot_a_take_emptied_loses_nothing() {
        let mailbox = Mailbox::new();
        assert!(mailbox.post(1).marker);
        assert_eq!(mailbox.take(), Some(1));

        // A marker is in flight for a value the take already returned.
        mailbox.set_pending_for_test();
        assert_eq!(
            mailbox.post(2),
            Posted {
                marker: false,
                replaced: false,
            }
        );
        assert_eq!(mailbox.take(), Some(2));
    }

    #[test]
    fn taking_an_empty_mailbox_yields_nothing() {
        let mailbox: Mailbox<u8> = Mailbox::new();
        assert_eq!(mailbox.take(), None);
        assert_eq!(mailbox.take(), None);
    }
}
