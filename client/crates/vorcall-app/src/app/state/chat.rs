//! The message lists, the composer and the images they draw.
//!
//! History is per channel and on demand: a channel that has never been opened
//! holds nothing, and [`ChannelUi::loaded`] is what says so.

use std::collections::{BTreeMap, BTreeSet};

use iced::widget::image::Handle;
use iced::widget::text_editor;
use vorcall_core::{Attachment, ChatMessage, ReadState};

use crate::app::message::ImageKey;
use crate::app::state::rules::{counter, trim};

/// How many messages of one channel stay in memory; nothing is persisted.
pub const MESSAGE_LIMIT: usize = 2000;
/// What [`ChatState::reacting`] holds while the composer's emoji palette is open
/// rather than a message's reaction palette: a message id is always positive, so
/// zero names the composer and no row lights up for it.
pub const COMPOSER_PALETTE: i64 = 0;
/// The server refuses anything longer with a non-fatal INVALID_MESSAGE.
pub const MESSAGE_MAX_CHARS: usize = 2000;

/// Which list the chat column is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MainView {
    #[default]
    Server,
    Dms,
}

/// What is open in the chat column. `None` until the first channel is picked,
/// which is also what a fresh account with no readable channel sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Current {
    #[default]
    None,
    Channel(i64),
    Dm(i64),
}

impl Current {
    /// The channel id behind it, whichever kind it is.
    pub fn channel_id(self) -> Option<i64> {
        match self {
            Self::None => None,
            Self::Channel(id) | Self::Dm(id) => Some(id),
        }
    }

    pub fn is(self, channel_id: i64) -> bool {
        self.channel_id() == Some(channel_id)
    }
}

/// One channel as the window holds it: the messages, the page state and this
/// reader's counters.
#[derive(Debug)]
pub struct ChannelUi {
    /// Keyed by id: history, older pages and live frames overlap, and the id is
    /// the only order.
    pub messages: BTreeMap<i64, ChatMessage>,
    /// Whether a `History` for this channel has ever landed.
    pub loaded: bool,
    pub loading: bool,
    pub has_older: bool,
    pub loading_older: bool,
    pub history_error: Option<String>,
    pub at_bottom: bool,
    /// Messages that arrived while the list was not at the bottom, which is what
    /// the jump-to-latest pill counts.
    pub pending_new: u32,
    pub unread: u32,
    pub mentions: u32,
    /// The newest message id this channel is known to hold, counters included:
    /// the read cursor is reported against it.
    pub newest_seen: i64,
}

impl Default for ChannelUi {
    fn default() -> Self {
        Self::new()
    }
}

impl ChannelUi {
    /// A channel nothing has been read of yet. It starts at the bottom: that is
    /// where the first page lands.
    pub fn new() -> Self {
        Self {
            messages: BTreeMap::new(),
            loaded: false,
            loading: false,
            has_older: false,
            loading_older: false,
            history_error: None,
            at_bottom: true,
            pending_new: 0,
            unread: 0,
            mentions: 0,
            newest_seen: 0,
        }
    }

    /// Takes the counters of one `ReadState`.
    pub fn take_counters(&mut self, state: &ReadState) {
        self.unread = counter(state.unread);
        self.mentions = counter(state.mentions);
        self.newest_seen = self.newest_seen.max(state.last_message_id);
    }

    /// Merges a page of messages, keeping the buffer inside its limit.
    pub fn merge(&mut self, messages: Vec<ChatMessage>) {
        for message in messages {
            self.newest_seen = self.newest_seen.max(message.id);
            self.messages.insert(message.id, message);
        }
        trim(&mut self.messages);
    }

    /// The newest message, which is what a read cursor names.
    pub fn newest(&self) -> Option<&ChatMessage> {
        self.messages.last_key_value().map(|(_, message)| message)
    }

    pub fn oldest_id(&self) -> Option<i64> {
        self.messages.keys().next().copied()
    }

    /// The newest message of mine that can still be edited, which is what the
    /// edit-last binding reaches for.
    pub fn last_own_message_id(&self, me: i64) -> Option<i64> {
        self.messages
            .values()
            .rev()
            .find(|message| message.author_id == me && !message.deleted)
            .map(|message| message.id)
    }
}

/// What the message being written carries besides its text.
#[derive(Default)]
pub struct Composer {
    pub content: text_editor::Content,
    pub reply_to: Option<i64>,
    pub editing: Option<i64>,
    /// Uploaded already, and linked to the message once it is sent.
    pub attachments: Vec<Attachment>,
    /// Uploads still in flight, which count against the per-message limit.
    pub uploading: usize,
    /// What is being typed after an `@`, without the `@` itself.
    pub mention_query: Option<String>,
}

impl Composer {
    /// The text as it stands, trailing newline and all.
    pub fn text(&self) -> String {
        self.content.text()
    }

    /// Puts `text` in the composer, replacing whatever was there.
    pub fn set_text(&mut self, text: &str) {
        self.content = text_editor::Content::with_text(text);
    }

    /// Empties the text and forgets the edit, keeping the attachments: an edit
    /// carries none of its own, so whatever is held still belongs to the message
    /// being written next.
    pub fn finish_edit(&mut self) {
        self.content = text_editor::Content::new();
        self.editing = None;
        self.mention_query = None;
    }

    /// Empties it and forgets the reply, the edit and the attachments.
    pub fn clear(&mut self) {
        self.content = text_editor::Content::new();
        self.reply_to = None;
        self.editing = None;
        self.attachments.clear();
        self.mention_query = None;
    }

    /// What the channel leaving view takes with it: the reply, the attachments
    /// and the mention being typed. A half-written message survives the switch,
    /// unless an edit is in progress — that text is the other channel's message.
    pub fn leave_channel(&mut self) {
        if self.editing.is_some() {
            self.content = text_editor::Content::new();
        }
        self.reply_to = None;
        self.editing = None;
        self.attachments.clear();
        self.mention_query = None;
    }

    /// Whether there is anything to send.
    pub fn is_empty(&self) -> bool {
        self.text().trim().is_empty() && self.attachments.is_empty()
    }
}

/// A new message is what brings a conversation taken off the list back: a DM has
/// no membership to leave, so hiding it is only a line in the preferences.
/// Answers whether the set changed, which is what makes it worth writing out.
pub fn reopen_on_message(hidden: &mut BTreeSet<i64>, channel_id: i64) -> bool {
    hidden.remove(&channel_id)
}

/// Where one image's pixels are. `Failed` is not retried on its own: the row
/// draws a placeholder instead.
#[derive(Debug, Clone)]
pub enum ImageState {
    Loading,
    Ready(Handle),
    Failed,
}

pub struct ChatState {
    /// Every channel the window has state for, keyed by channel id.
    pub channels: BTreeMap<i64, ChannelUi>,
    pub current: Current,
    pub view: MainView,
    pub composer: Composer,
    pub images: BTreeMap<ImageKey, ImageState>,
    /// The newest message of a channel that has been read but not reported yet.
    pub mark_read_due: Option<(i64, i64)>,
    /// The message row under the pointer, which is what shows its actions.
    pub hovered: Option<i64>,
    /// The message whose reaction palette is open.
    pub reacting: Option<i64>,
    pub confirm_delete: Option<i64>,
}

impl Default for ChatState {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatState {
    pub fn new() -> Self {
        Self {
            channels: BTreeMap::new(),
            current: Current::None,
            view: MainView::Server,
            composer: Composer::default(),
            images: BTreeMap::new(),
            mark_read_due: None,
            hovered: None,
            reacting: None,
            confirm_delete: None,
        }
    }

    pub fn channel(&self, id: i64) -> Option<&ChannelUi> {
        self.channels.get(&id)
    }

    /// The channel's state, created empty if this is the first mention of it.
    pub fn entry(&mut self, id: i64) -> &mut ChannelUi {
        self.channels.entry(id).or_default()
    }

    pub fn current(&self) -> Option<&ChannelUi> {
        self.channels.get(&self.current.channel_id()?)
    }

    pub fn current_mut(&mut self) -> Option<&mut ChannelUi> {
        let id = self.current.channel_id()?;
        self.channels.get_mut(&id)
    }

    /// What the window title counts.
    pub fn total_unread(&self) -> u32 {
        self.channels
            .values()
            .map(|channel| channel.unread)
            .fold(0u32, u32::saturating_add)
    }

    /// One message, wherever it is.
    pub fn message(&self, id: i64) -> Option<&ChatMessage> {
        self.channels
            .values()
            .find_map(|channel| channel.messages.get(&id))
    }

    /// Starts holding one image, answering whether the caller has to load it: a
    /// key that is already there is a request somebody else made.
    pub fn ensure_image(&mut self, key: ImageKey) -> bool {
        if self.images.contains_key(&key) {
            return false;
        }
        self.images.insert(key, ImageState::Loading);
        true
    }

    /// Every attachment one channel holds, which is what its rows draw.
    pub fn image_keys(&self, channel_id: i64) -> Vec<ImageKey> {
        let Some(channel) = self.channels.get(&channel_id) else {
            return Vec::new();
        };
        channel
            .messages
            .values()
            .flat_map(|message| message.attachments.iter())
            .map(|attachment| ImageKey::Attachment(attachment.id))
            .collect()
    }

    /// Whether this account already reacted to one message with that emoji,
    /// which is what makes the next click a removal.
    pub fn reacted(&self, message_id: i64, emoji: &str, me: i64) -> bool {
        self.message(message_id).is_some_and(|message| {
            message
                .reactions
                .iter()
                .any(|reaction| reaction.emoji == emoji && reaction.user_ids.contains(&me))
        })
    }

    /// Clears one channel's counters and answers with the cursor to report for
    /// it. [`ChannelUi::newest_seen`] rather than the buffer: a channel that has
    /// never been opened is read as of whatever its read state named.
    pub fn read_cursor(&mut self, channel_id: i64) -> Option<i64> {
        let channel = self.channels.get_mut(&channel_id)?;
        channel.unread = 0;
        channel.mentions = 0;
        (channel.newest_seen != 0).then_some(channel.newest_seen)
    }

    /// Marks the channel in view read as of its newest message, to be reported
    /// by the next tick.
    pub fn schedule_mark_read(&mut self) {
        let Some(channel_id) = self.current.channel_id() else {
            return;
        };
        let Some(newest) = self
            .channels
            .get(&channel_id)
            .and_then(|channel| channel.newest().map(|message| message.id))
        else {
            return;
        };
        if let Some(channel) = self.channels.get_mut(&channel_id) {
            channel.unread = 0;
            channel.mentions = 0;
        }
        self.mark_read_due = Some((channel_id, newest));
    }

    /// What the channel leaving view takes with it: the message being written,
    /// its reply and edit included, and the row states that named one of its
    /// messages. A half-typed text stays, as [`Composer::leave_channel`] says.
    pub fn leave_channel(&mut self) {
        self.composer.leave_channel();
        self.hovered = None;
        self.reacting = None;
        self.confirm_delete = None;
    }

    /// Whether the pointer left the row it is being told about, rather than one
    /// the pointer has already moved on from.
    pub fn unhover(&mut self, id: i64) {
        if self.hovered == Some(id) {
            self.hovered = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use vorcall_core::Reaction;

    use super::*;

    fn attachment(id: i64) -> Attachment {
        Attachment {
            id,
            file_name: format!("{id}.png"),
            content_type: "image/png".to_owned(),
            size: 64,
        }
    }

    fn message(id: i64, channel_id: i64) -> ChatMessage {
        ChatMessage {
            id,
            author: "ana".to_owned(),
            text: format!("message {id}"),
            sent_at_unix_ms: id,
            author_id: 4,
            edited_at_unix_ms: 0,
            deleted: false,
            reply_to: None,
            mention_ids: Vec::new(),
            reactions: Vec::new(),
            attachments: Vec::new(),
            channel_id,
            mention_everyone: false,
            mention_here: false,
        }
    }

    #[test]
    fn a_channel_starts_at_the_bottom_with_nothing_in_it() {
        let channel = ChannelUi::new();

        assert!(channel.at_bottom);
        assert!(!channel.loaded);
        assert_eq!(channel.newest(), None);
        assert_eq!(channel.oldest_id(), None);
    }

    #[test]
    fn merging_keeps_one_copy_of_every_id_and_the_newest_seen() {
        let mut channel = ChannelUi::new();
        channel.merge(vec![message(2, 1), message(1, 1)]);
        channel.merge(vec![message(2, 1), message(3, 1)]);

        assert_eq!(channel.messages.len(), 3);
        assert_eq!(channel.newest().map(|message| message.id), Some(3));
        assert_eq!(channel.oldest_id(), Some(1));
        assert_eq!(channel.newest_seen, 3);
    }

    #[test]
    fn the_counters_come_off_the_read_state() {
        let mut channel = ChannelUi::new();
        channel.take_counters(&ReadState {
            channel_id: 1,
            unread: 4,
            mentions: 2,
            last_message_id: 40,
        });

        assert_eq!((channel.unread, channel.mentions), (4, 2));
        assert_eq!(channel.newest_seen, 40);
    }

    #[test]
    fn reading_a_channel_clears_its_counters_and_queues_the_cursor() {
        let mut chat = ChatState::new();
        chat.current = Current::Channel(1);
        let channel = chat.entry(1);
        channel.unread = 3;
        channel.mentions = 1;
        channel.merge(vec![message(40, 1)]);

        chat.schedule_mark_read();

        assert_eq!(chat.mark_read_due, Some((1, 40)));
        assert_eq!(chat.total_unread(), 0);
        assert_eq!(chat.channel(1).map(|channel| channel.mentions), Some(0));
    }

    /// Nothing to report in a channel with no messages in it yet.
    #[test]
    fn an_empty_channel_queues_no_cursor() {
        let mut chat = ChatState::new();
        chat.current = Current::Channel(1);
        chat.entry(1);

        chat.schedule_mark_read();

        assert_eq!(chat.mark_read_due, None);
    }

    #[test]
    fn leaving_a_row_never_clears_the_one_the_pointer_moved_on_to() {
        let mut chat = ChatState::new();
        chat.hovered = Some(2);

        chat.unhover(1);
        assert_eq!(chat.hovered, Some(2));
        chat.unhover(2);
        assert_eq!(chat.hovered, None);
    }

    /// An edit is finished by sending it, and what was held for the next message
    /// is still held.
    #[test]
    fn finishing_an_edit_keeps_the_attachments_that_were_held() {
        let mut composer = Composer::default();
        composer.set_text("typo fixed");
        composer.editing = Some(7);
        composer.attachments = vec![attachment(3)];

        composer.finish_edit();

        assert_eq!(composer.editing, None);
        assert!(composer.text().trim().is_empty());
        assert_eq!(composer.attachments.len(), 1);
    }

    #[test]
    fn the_last_message_of_mine_is_the_one_an_edit_reaches_for() {
        let mut channel = ChannelUi::new();
        let mut mine = message(2, 1);
        mine.author_id = 7;
        let mut tombstone = message(3, 1);
        tombstone.author_id = 7;
        tombstone.deleted = true;
        channel.merge(vec![message(1, 1), mine, tombstone]);

        assert_eq!(channel.last_own_message_id(7), Some(2));
        // The author of message 1 is somebody else.
        assert_eq!(channel.last_own_message_id(99), None);
    }

    /// Two rows naming the same attachment must not fetch it twice.
    #[test]
    fn an_image_is_claimed_once() {
        let mut chat = ChatState::new();

        assert!(chat.ensure_image(ImageKey::Attachment(4)));
        assert!(!chat.ensure_image(ImageKey::Attachment(4)));
        assert!(chat.ensure_image(ImageKey::Image(4)));
    }

    #[test]
    fn a_channels_images_are_every_attachment_in_it() {
        let mut chat = ChatState::new();
        let mut with_image = message(2, 1);
        with_image.attachments = vec![attachment(40), attachment(41)];
        chat.entry(1).merge(vec![message(1, 1), with_image]);

        assert_eq!(
            chat.image_keys(1),
            [ImageKey::Attachment(40), ImageKey::Attachment(41)]
        );
        assert!(chat.image_keys(99).is_empty());
    }

    #[test]
    fn a_reaction_of_mine_is_what_makes_the_next_click_a_removal() {
        let mut chat = ChatState::new();
        let mut reacted = message(1, 1);
        reacted.reactions = vec![Reaction {
            emoji: "👍".to_owned(),
            user_ids: vec![7],
        }];
        chat.entry(1).merge(vec![reacted]);

        assert!(chat.reacted(1, "👍", 7));
        assert!(!chat.reacted(1, "👍", 9));
        assert!(!chat.reacted(1, "🔥", 7));
        assert!(!chat.reacted(99, "👍", 7));
    }

    /// Marking a channel read from the list works without ever having opened it,
    /// which is exactly when there is no message in the buffer to name.
    #[test]
    fn a_channel_that_was_never_opened_is_read_as_of_its_read_state() {
        let mut chat = ChatState::new();
        chat.entry(1).take_counters(&ReadState {
            channel_id: 1,
            unread: 5,
            mentions: 2,
            last_message_id: 40,
        });
        chat.entry(2);

        assert_eq!(chat.read_cursor(1), Some(40));
        assert_eq!(chat.total_unread(), 0);
        assert_eq!(chat.channel(1).map(|channel| channel.mentions), Some(0));
        // Nothing has ever been sent in this one, so there is no cursor.
        assert_eq!(chat.read_cursor(2), None);
        assert_eq!(chat.read_cursor(99), None);
    }

    /// The message being written belongs to the channel it was written in.
    #[test]
    fn opening_another_channel_starts_the_message_over() {
        let mut chat = ChatState::new();
        chat.current = Current::Channel(1);
        chat.composer.reply_to = Some(4);
        chat.composer.attachments.push(attachment(3));
        chat.composer.set_text("half a sentence");
        chat.hovered = Some(5);
        chat.reacting = Some(5);
        chat.confirm_delete = Some(5);

        chat.leave_channel();

        assert_eq!(chat.composer.reply_to, None);
        assert!(chat.composer.attachments.is_empty());
        // The draft itself survives: it is the reader's, not the channel's.
        assert_eq!(chat.composer.text().trim(), "half a sentence");
        assert_eq!(chat.hovered, None);
        assert_eq!(chat.reacting, None);
        assert_eq!(chat.confirm_delete, None);
    }

    /// An edit owns the input too: that text is the other channel's message.
    #[test]
    fn opening_another_channel_while_editing_drops_the_text_as_well() {
        let mut chat = ChatState::new();
        chat.composer.editing = Some(4);
        chat.composer.set_text("the other channel's message");
        chat.composer.mention_query = Some("an".to_owned());

        chat.leave_channel();

        assert_eq!(chat.composer.editing, None);
        assert!(chat.composer.text().trim().is_empty());
        assert_eq!(chat.composer.mention_query, None);
    }

    #[test]
    fn a_message_reopens_a_closed_conversation() {
        let mut hidden = BTreeSet::from([4]);

        assert!(reopen_on_message(&mut hidden, 4));
        assert!(!hidden.contains(&4));
        // A message in a conversation that was never closed changes nothing.
        assert!(!reopen_on_message(&mut hidden, 4));
        assert!(!reopen_on_message(&mut hidden, 9));
    }

    #[test]
    fn the_composer_knows_when_there_is_nothing_to_send() {
        let mut composer = Composer::default();
        assert!(composer.is_empty());

        composer.set_text("  \n ");
        assert!(composer.is_empty());
        composer.set_text("hello");
        assert!(!composer.is_empty());

        composer.reply_to = Some(4);
        composer.clear();
        assert!(composer.is_empty());
        assert_eq!(composer.reply_to, None);
    }
}
