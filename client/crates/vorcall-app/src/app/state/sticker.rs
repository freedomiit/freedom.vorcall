//! The sticker library as this client mirrors it, the composer picker's switch,
//! and the two pure rules a sticker message is drawn and sent by.
//!
//! The library is server-wide — `PROTOCOL.md` § Stickers — so there is nothing
//! per channel or per member in it, and the two deltas keep it current between
//! snapshots. Nothing here draws, decodes or talks to the network.

use std::collections::BTreeMap;

use vorcall_core::Sticker;
use vorcall_core::connection::Command;

/// The stickers as this window holds them.
#[derive(Debug, Default)]
pub struct StickerState {
    /// Every sticker the server has, by id.
    pub library: BTreeMap<i64, Sticker>,
    /// Whether the composer's picker is showing.
    pub picker_open: bool,
    /// An upload is on its way, which is what keeps a second press off the
    /// settings page's button.
    pub uploading: bool,
}

impl StickerState {
    /// Replaces the whole library, the way a reconnect replaces everything else.
    pub fn apply_snapshot(&mut self, stickers: Vec<Sticker>) {
        self.library = stickers
            .into_iter()
            .map(|sticker| (sticker.id, sticker))
            .collect();
    }

    pub fn upsert(&mut self, sticker: Sticker) {
        self.library.insert(sticker.id, sticker);
    }

    pub fn remove(&mut self, sticker_id: i64) {
        self.library.remove(&sticker_id);
    }

    /// The library in the order the picker and the settings page list it: by
    /// name, so two stickers added minutes apart do not sit at opposite ends.
    pub fn ordered(&self) -> Vec<&Sticker> {
        let mut stickers: Vec<&Sticker> = self.library.values().collect();
        stickers.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then(left.id.cmp(&right.id))
        });
        stickers
    }
}

/// What one press in the picker sends. A sticker message is only a sticker: the
/// server refuses one carrying text, attachments or streamed files — `PROTOCOL.md`
/// § Stickers — and the reply the composer was holding is the one thing it may
/// still carry.
pub fn send(channel_id: i64, sticker_id: i64, reply_to_id: Option<i64>) -> Command {
    Command::Send {
        channel_id,
        text: String::new(),
        reply_to_id,
        attachment_ids: Vec::new(),
        streamed_file_ids: Vec::new(),
        sticker_id: Some(sticker_id),
    }
}

/// Whether a message's sticker is gone: the message is still a sticker message,
/// but the library no longer has the row it named. The server clears `sticker_id`
/// and leaves `sticker` set, which is the only difference between this and a
/// sticker that has simply not been fetched yet.
pub fn is_removed(sticker: bool, sticker_id: i64) -> bool {
    sticker && sticker_id == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sticker(id: i64, name: &str) -> Sticker {
        Sticker {
            id,
            name: name.to_owned(),
            uploader_id: 7,
            content_type: "image/png".to_owned(),
            size: 4_096,
        }
    }

    #[test]
    fn the_library_takes_an_upsert_and_a_delete() {
        let mut state = StickerState::default();
        state.apply_snapshot(vec![sticker(1, "wave"), sticker(2, "shrug")]);

        state.upsert(sticker(2, "big shrug"));
        state.upsert(sticker(3, "applause"));
        assert_eq!(
            state.library.get(&2).map(|sticker| sticker.name.as_str()),
            Some("big shrug")
        );

        state.remove(1);
        let names: Vec<&str> = state
            .ordered()
            .into_iter()
            .map(|sticker| sticker.name.as_str())
            .collect();
        assert_eq!(names, ["applause", "big shrug"]);
    }

    /// A reconnect replaces the library rather than merging into it.
    #[test]
    fn a_snapshot_replaces_the_library_wholesale() {
        let mut state = StickerState::default();
        state.apply_snapshot(vec![sticker(1, "wave"), sticker(2, "shrug")]);

        state.apply_snapshot(vec![sticker(5, "bell")]);

        assert_eq!(state.library.keys().copied().collect::<Vec<_>>(), [5]);
    }

    #[test]
    fn a_sticker_send_carries_nothing_but_its_reply() {
        let Command::Send {
            channel_id,
            text,
            reply_to_id,
            attachment_ids,
            streamed_file_ids,
            sticker_id,
        } = send(10, 42, Some(7))
        else {
            panic!("a sticker is sent as a message");
        };

        assert_eq!(channel_id, 10);
        assert!(text.is_empty());
        assert_eq!(reply_to_id, Some(7));
        assert!(attachment_ids.is_empty());
        assert!(streamed_file_ids.is_empty());
        assert_eq!(sticker_id, Some(42));
    }

    #[test]
    fn a_send_without_a_reply_carries_none() {
        let Command::Send { reply_to_id, .. } = send(10, 42, None) else {
            panic!("a sticker is sent as a message");
        };
        assert_eq!(reply_to_id, None);
    }

    /// Only a sticker message whose row is gone reads as removed: an ordinary
    /// message carries no sticker at all, and a live one carries its id.
    #[test]
    fn a_sticker_reads_as_removed_only_once_its_row_is_gone() {
        assert!(is_removed(true, 0));
        assert!(!is_removed(true, 42));
        assert!(!is_removed(false, 0));
        assert!(!is_removed(false, 42));
    }
}
