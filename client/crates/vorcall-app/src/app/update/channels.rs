//! Picking what is in view: channels, categories and DMs.

use std::collections::BTreeSet;

use iced::Task;
use iced::widget::{Id, operation, scrollable};
use vorcall_core::{ChannelKind, Command};

use crate::app::message::{ChannelsMsg, Message};
use crate::app::state::chat::{Current, MainView};
use crate::app::state::rules::{next_unread, step};
use crate::app::state::server::channel_kind;
use crate::app::state::ui::Dialog;
use crate::app::{App, MainState};
use crate::view;

pub fn update(app: &mut App, message: ChannelsMsg) -> Task<Message> {
    match message {
        ChannelsMsg::Select(channel_id) => select(app, channel_id),
        ChannelsMsg::ShowServer => {
            if let Some(main) = app.main_mut() {
                main.chat.view = MainView::Server;
            }
            Task::none()
        }
        ChannelsMsg::ShowDms => {
            if let Some(main) = app.main_mut() {
                main.chat.view = MainView::Dms;
            }
            Task::none()
        }
        ChannelsMsg::ToggleCategory(category_id) => {
            if app.ui.collapsed.contains(&category_id) {
                app.ui.collapsed.remove(&category_id);
            } else {
                app.ui.collapsed.insert(category_id);
            }
            app.config.collapsed_categories = app.ui.collapsed.clone();
            app.save_config();
            Task::none()
        }
        ChannelsMsg::OpenDm(user_id) => open_dm(app, user_id),
        ChannelsMsg::HideDm(channel_id) => hide_dm(app, channel_id),
        ChannelsMsg::PrevChannel => walk(app, false),
        ChannelsMsg::NextChannel => walk(app, true),
        ChannelsMsg::NextUnread => walk_unread(app, true),
        ChannelsMsg::PrevUnread => walk_unread(app, false),
        ChannelsMsg::CreateChannelIn(category_id) => {
            app.ui.dialog = Some(Dialog::CreateChannel {
                category_id,
                kind: ChannelKind::Text,
                name: String::new(),
                error: None,
            });
            Task::none()
        }
        ChannelsMsg::CreateCategory => {
            app.ui.dialog = Some(Dialog::CreateCategory {
                name: String::new(),
                error: None,
            });
            Task::none()
        }
        ChannelsMsg::RenameChannel(channel_id) => {
            let Some(channel) = app.main().and_then(|main| main.server.channel(channel_id)) else {
                return Task::none();
            };
            app.ui.dialog = Some(Dialog::EditChannel {
                channel_id,
                name: channel.name.clone(),
                topic: channel.topic.clone(),
            });
            Task::none()
        }
        ChannelsMsg::DeleteChannel(channel_id) => {
            app.ui.dialog = Some(Dialog::ConfirmDeleteChannel { channel_id });
            Task::none()
        }
        ChannelsMsg::MuteChannel(channel_id) => {
            app.config.muted_channels.insert(channel_id);
            app.save_config();
            Task::none()
        }
        ChannelsMsg::UnmuteChannel(channel_id) => {
            app.config.muted_channels.remove(&channel_id);
            app.save_config();
            Task::none()
        }
    }
}

/// Opens one channel: it goes in view, its history is asked for the first time,
/// and the list jumps to the bottom.
pub fn select(app: &mut App, channel_id: i64) -> Task<Message> {
    let focused = app.ui.focused;
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let Some(channel) = main.server.channel(channel_id) else {
        tracing::debug!(channel_id, "ignoring a channel the snapshot does not carry");
        return Task::none();
    };

    // A voice channel is joined, never read: `update::voice` owns that.
    if channel_kind(channel) == ChannelKind::Voice {
        return Task::none();
    }
    let dm = channel_kind(channel) == ChannelKind::Dm;

    // Re-selecting the channel in view still has to ask for the history a
    // reconnect left behind; only what the reader sees is left alone.
    let already = main.chat.current.is(channel_id);
    if !already {
        main.chat.current = if dm {
            Current::Dm(channel_id)
        } else {
            Current::Channel(channel_id)
        };
        // The reply target, the attachments and an edit belong to the channel that
        // was in view, and so do the row states; a draft of a new message follows
        // the reader, because only an edit's text was another channel's message.
        main.chat.leave_channel();
        // The picker belongs to the channel it was opened over, like the
        // composer's other popovers.
        main.sticker.picker_open = false;
    }

    // History is per channel and on demand: the first open is what asks for it.
    let ask = {
        let entry = main.chat.entry(channel_id);
        entry.at_bottom = true;
        entry.pending_new = 0;
        let ask = !entry.loaded && !entry.loading;
        entry.loading |= ask;
        ask
    };
    if ask && !main.send_command(Command::LoadHistory { channel_id }) {
        main.chat.entry(channel_id).loading = false;
    }
    if focused {
        main.chat.schedule_mark_read();
    }

    app.config.last_channel = channel_id;
    app.save_config();

    // Whatever is already held is drawn with its images rather than fetching them
    // once a row is on screen.
    let images = {
        let keys = app
            .main()
            .map(|main| main.chat.image_keys(channel_id))
            .unwrap_or_default();
        app.ensure_images(keys)
    };
    if already {
        return images;
    }
    Task::batch([
        images,
        operation::snap_to(
            Id::new(view::MESSAGES_ID),
            scrollable::RelativeOffset::START,
        ),
        operation::focus(Id::new(view::COMPOSER_ID)),
    ])
}

/// A DM this client already knows opens straight away; otherwise the server is
/// asked for it and [`MainState::pending_dm`] waits for the answer.
fn open_dm(app: &mut App, user_id: i64) -> Task<Message> {
    let existing = {
        let Some(main) = app.main_mut() else {
            return Task::none();
        };
        main.chat.view = MainView::Dms;
        match main.server.dm_with(user_id) {
            Some(channel_id) => channel_id,
            None => {
                main.pending_dm = Some(user_id);
                main.send_or_notice(Command::OpenDm { user_id });
                return Task::none();
            }
        }
    };

    // Asking for a conversation again is what brings a closed one back.
    if app.config.hidden_dms.remove(&existing) {
        app.save_config();
    }
    select(app, existing)
}

/// Takes one DM out of the list. It is not left — a DM has no membership — so a
/// new message in it brings it back.
fn hide_dm(app: &mut App, channel_id: i64) -> Task<Message> {
    app.config.hidden_dms.insert(channel_id);
    app.save_config();

    if !app
        .main()
        .is_some_and(|main| main.chat.current.is(channel_id))
    {
        return Task::none();
    }
    let next = app.main().and_then(|main| {
        main.server
            .dms()
            .into_iter()
            .find(|channel| !app.config.hidden_dms.contains(&channel.id))
            .map(|channel| channel.id)
    });
    match next {
        Some(channel_id) => select(app, channel_id),
        None => {
            if let Some(main) = app.main_mut() {
                main.chat.current = Current::None;
            }
            Task::none()
        }
    }
}

/// One step through the list in view, wrapping at both ends.
fn walk(app: &mut App, forward: bool) -> Task<Message> {
    let target = {
        let Some(main) = app.main() else {
            return Task::none();
        };
        let order = walk_order(main, &app.config.hidden_dms);
        step(&order, main.chat.current.channel_id(), forward)
    };
    match target {
        Some(channel_id) => select(app, channel_id),
        None => Task::none(),
    }
}

/// The nearest channel in the list with something unread in it.
fn walk_unread(app: &mut App, forward: bool) -> Task<Message> {
    let target = {
        let Some(main) = app.main() else {
            return Task::none();
        };
        let order = walk_order(main, &app.config.hidden_dms);
        let unread: BTreeSet<i64> = main
            .chat
            .channels
            .iter()
            .filter(|(_, channel)| channel.unread > 0)
            .map(|(channel_id, _)| *channel_id)
            .collect();
        next_unread(&order, main.chat.current.channel_id(), &unread, forward)
    };
    match target {
        Some(channel_id) => select(app, channel_id),
        None => Task::none(),
    }
}

/// What the keyboard steps through: the text channels in sidebar order, or the
/// DMs that are still in the list. A voice channel is joined, never read, so it
/// is never stepped onto.
fn walk_order(main: &MainState, hidden: &BTreeSet<i64>) -> Vec<i64> {
    match main.chat.view {
        MainView::Server => main
            .server
            .channels_of_kind(ChannelKind::Text)
            .into_iter()
            .map(|channel| channel.id)
            .collect(),
        MainView::Dms => main
            .server
            .dms()
            .into_iter()
            .filter(|channel| !hidden.contains(&channel.id))
            .map(|channel| channel.id)
            .collect(),
    }
}
