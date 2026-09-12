//! Everything the connection loop reports.
//!
//! The connection's own transitions, the snapshot and every delta, the message
//! list and the transfers are answered here. The voice, share and server-settings
//! events are routed to the module that owns that area.

use std::collections::BTreeSet;

use iced::Task;
use iced::widget::{Id, operation, scrollable};
use vorcall_core::connection::Blob;
use vorcall_core::permissions;
use vorcall_core::{
    ChannelKind, ChatMessage, Command, DisconnectReason, ErrorCode, Event, ReadState,
};

use crate::app::message::{AdminMsg, Message, ToastKind};
use crate::app::state::chat::{self, Current, ImageState, MESSAGE_LIMIT, MainView};
use crate::app::state::rules::{
    addressed_to_me, mention_counts, notification_body, notification_rule, pending_channel_name,
    plain_text, unread_rule,
};
use crate::app::state::server::{ServerModel, channel_kind};
use crate::app::state::settings::ServerTab;
use crate::app::state::ui::{Dialog, Route};
use crate::app::update::{admin, channels, check, settings, share, voice};
use crate::app::{App, MainState, Status, decode_task};
use crate::view;
use crate::workers::images::{self, ImageKey};
use crate::workers::notify;

pub fn update(app: &mut App, event: Event) -> Task<Message> {
    // The reasons that end the session for good replace the whole screen, so they
    // are answered before the main state is touched.
    match &event {
        Event::Disconnected {
            reason: DisconnectReason::AuthRequired(detail),
            ..
        } => {
            let detail = if detail.trim().is_empty() {
                "Sign in again, please.".to_owned()
            } else {
                detail.trim().to_owned()
            };
            return app.sign_out(Some(detail));
        }
        Event::Disconnected {
            reason: DisconnectReason::SessionReplaced,
            ..
        } => {
            return app.sign_out(Some(
                "This account connected from another device.".to_owned(),
            ));
        }
        Event::Disconnected {
            reason: DisconnectReason::Kicked,
            ..
        } => {
            return app.sign_out(Some("A moderator removed you from the server.".to_owned()));
        }
        Event::Disconnected {
            reason: DisconnectReason::Banned,
            ..
        } => {
            return app.sign_out(Some("You are banned from this server.".to_owned()));
        }
        _ => {}
    }

    if app.main().is_none() {
        return Task::none();
    }
    apply(app, event)
}

fn apply(app: &mut App, event: Event) -> Task<Message> {
    match event {
        Event::Ready(sender) => {
            if let Some(main) = app.main_mut() {
                main.cmd = Some(sender);
            }
            Task::none()
        }
        Event::Connecting => {
            if let Some(main) = app.main_mut() {
                main.status = Status::Connecting;
            }
            Task::none()
        }
        Event::Connected {
            member_id,
            username,
            ..
        } => {
            {
                let Some(main) = app.main_mut() else {
                    return Task::none();
                };
                main.status = Status::Connected;
                main.member_id = member_id;
                main.server.me = member_id;
                main.username = username;
                main.notice = None;

                // History is per channel and on demand, so the channel in view and
                // every channel already loaded ask again: that is what fills the
                // gap the reconnect left.
                let asking: BTreeSet<i64> = main
                    .chat
                    .current
                    .channel_id()
                    .into_iter()
                    .chain(
                        main.chat
                            .channels
                            .iter()
                            .filter(|(_, channel)| channel.loaded)
                            .map(|(channel_id, _)| *channel_id),
                    )
                    .collect();
                for channel_id in asking {
                    main.chat.entry(channel_id).loading = true;
                    main.send_command(Command::LoadHistory { channel_id });
                }
            }

            // One check per run, started by the first connection this process
            // makes; every reconnect after it asks nothing.
            let checking = if app.checked_on_connect {
                Task::none()
            } else {
                app.checked_on_connect = true;
                check(app)
            };
            Task::batch([checking, voice::on_connected(app)])
        }
        Event::Disconnected { reason, retry_in } => {
            let status = match reason {
                DisconnectReason::Unauthorized => Status::Unauthorized,
                other => match retry_in {
                    Some(delay) => Status::Reconnecting {
                        in_secs: delay.as_secs().max(1),
                    },
                    None => Status::Disconnected(other.to_string()),
                },
            };
            if let Some(main) = app.main_mut() {
                main.status = status;
                main.cmd = None;
                // Nothing is in flight any more; the next `Connected` asks again.
                for channel in main.chat.channels.values_mut() {
                    channel.loading = false;
                    channel.loading_older = false;
                }
                for member in main.server.members.values_mut() {
                    member.online = false;
                }
            }
            voice::on_disconnected(app)
        }
        // Already persisted by the loop; the subscription must not restart, so its
        // identity does not include the tokens.
        Event::SessionUpdated(session) => {
            app.session = Some(session);
            Task::none()
        }
        Event::ServerError {
            code,
            detail,
            fatal,
        } => on_server_error(app, code, detail, fatal),
        Event::Snapshot(snapshot) => {
            let images = {
                let Some(main) = app.main_mut() else {
                    return Task::none();
                };
                main.server.apply_snapshot(snapshot);
                main.refresh_user_pairs();

                // The counters arrive with the snapshot; the buffers do not.
                let states: Vec<ReadState> = main.server.reads.values().cloned().collect();
                for state in states {
                    main.chat.entry(state.channel_id).take_counters(&state);
                }
                profile_images(&main.server)
            };

            let keep = app
                .main()
                .and_then(|main| {
                    main.chat
                        .current
                        .channel_id()
                        .filter(|id| main.server.channels.contains_key(id))
                })
                .or_else(|| first_channel(app));
            let selected = match keep {
                Some(channel_id) => channels::select(app, channel_id),
                None => Task::none(),
            };
            Task::batch([app.ensure_images(images), selected])
        }
        Event::ServerUpdated(server) => {
            let icon = server.icon_image_id;
            if let Some(main) = app.main_mut() {
                main.server.server = server;
            }
            app.ensure_images(one_image(icon))
        }
        Event::RoleUpserted(role) => {
            let icon = role.icon_image_id;
            if let Some(main) = app.main_mut() {
                main.server.upsert_role(role);
            }
            app.ensure_images(one_image(icon))
        }
        Event::RoleDeleted { id } => {
            if let Some(main) = app.main_mut() {
                main.server.delete_role(id);
            }
            Task::none()
        }
        Event::RoleOrder { ids } => {
            if let Some(main) = app.main_mut() {
                main.server.set_role_order(&ids);
            }
            Task::none()
        }
        Event::CategoryUpserted(category) => {
            if let Some(main) = app.main_mut() {
                main.server.upsert_category(category);
            }
            Task::none()
        }
        Event::CategoryDeleted { id } => {
            if let Some(main) = app.main_mut() {
                main.server.delete_category(id);
            }
            Task::none()
        }
        Event::ChannelUpserted(channel) => {
            let id = channel.id;
            let dm = channel_kind(&channel) == ChannelKind::Dm;
            let open = {
                let Some(main) = app.main_mut() else {
                    return Task::none();
                };
                main.server.upsert_channel(channel);
                main.chat.entry(id);
                opens(main, id, dm)
            };
            let Some(open) = open else {
                return Task::none();
            };
            // The create dialog stays up until the channel it asked for arrives.
            if matches!(open, Opened::Created) {
                app.ui.dialog = None;
            }
            channels::select(app, id)
        }
        // A channel this account may no longer see is gone as far as the window is
        // concerned.
        Event::ChannelDeleted { id } => {
            let Some(main) = app.main_mut() else {
                return Task::none();
            };
            main.server.delete_channel(id);
            main.chat.channels.remove(&id);
            if main.chat.current.is(id) {
                main.chat.current = Current::None;
                return match first_channel(app) {
                    Some(channel_id) => channels::select(app, channel_id),
                    None => Task::none(),
                };
            }
            Task::none()
        }
        Event::ChannelOrder { positions } => {
            if let Some(main) = app.main_mut() {
                main.server.set_channel_order(&positions);
            }
            Task::none()
        }
        Event::MemberUpdated(member) => {
            let images = [member.avatar_image_id, member.banner_image_id];
            if let Some(main) = app.main_mut() {
                main.server.upsert_member(member);
                main.refresh_user_pairs();
            }
            let fetched = app.ensure_images(
                images
                    .into_iter()
                    .filter(|id| *id != 0)
                    .map(ImageKey::Image)
                    .collect(),
            );
            // An unban is broadcast as the member's profile coming back.
            Task::batch([fetched, bans_refresh(app)])
        }
        Event::MemberRemoved { user_id } => {
            if let Some(main) = app.main_mut() {
                main.server.remove_member(user_id);
                main.refresh_user_pairs();
            }
            bans_refresh(app)
        }
        Event::History {
            channel_id,
            messages,
            has_more,
        } => {
            let focused = app.ui.focused;
            let current = {
                let Some(main) = app.main_mut() else {
                    return Task::none();
                };
                let channel = main.chat.entry(channel_id);
                channel.history_error = None;
                channel.loaded = true;
                channel.loading = false;
                channel.merge(messages);
                // A reconnect starts the page state over.
                channel.loading_older = false;
                channel.has_older = has_more;
                channel.at_bottom = true;
                channel.pending_new = 0;

                let current = main.chat.current.is(channel_id);
                if current && focused {
                    main.chat.schedule_mark_read();
                }
                current
            };
            if !current {
                return Task::none();
            }
            Task::batch([channel_images(app, channel_id), snap_to_bottom()])
        }
        Event::HistoryFailed { channel_id, error } => {
            if let Some(main) = app.main_mut() {
                let channel = main.chat.entry(channel_id);
                channel.loading = false;
                channel.history_error = Some(error);
            }
            Task::none()
        }
        Event::Message(message) => on_message(app, message),
        Event::MessageEdited(message) => {
            if let Some(main) = app.main_mut()
                && let Some(channel) = main.chat.channels.get_mut(&message.channel_id)
                && channel.messages.contains_key(&message.id)
            {
                channel.messages.insert(message.id, message);
            }
            Task::none()
        }
        Event::MessageDeleted { channel_id, id } => {
            let Some(main) = app.main_mut() else {
                return Task::none();
            };
            // The tombstone stays in history and in every page, so the row keeps
            // its place and only loses its content.
            if let Some(message) = main
                .chat
                .channels
                .get_mut(&channel_id)
                .and_then(|channel| channel.messages.get_mut(&id))
            {
                message.deleted = true;
                message.text.clear();
                message.mention_ids.clear();
                message.reactions.clear();
                message.attachments.clear();
            }
            if main.chat.composer.editing == Some(id) {
                main.chat.composer.finish_edit();
            }
            if main.chat.confirm_delete == Some(id) {
                main.chat.confirm_delete = None;
            }
            Task::none()
        }
        Event::ReactionsChanged {
            channel_id,
            message_id,
            reactions,
        } => {
            if let Some(main) = app.main_mut()
                && let Some(message) = main
                    .chat
                    .channels
                    .get_mut(&channel_id)
                    .and_then(|channel| channel.messages.get_mut(&message_id))
            {
                message.reactions = reactions;
            }
            Task::none()
        }
        // No scroll operation: the bottom anchor keeps the viewport where it is
        // while the list grows upwards.
        Event::OlderPage {
            channel_id,
            messages,
            has_more,
        } => {
            let current = {
                let Some(main) = app.main_mut() else {
                    return Task::none();
                };
                let Some(channel) = main.chat.channels.get_mut(&channel_id) else {
                    tracing::debug!(channel_id, "ignoring an older page for an unknown channel");
                    return Task::none();
                };
                channel.merge(messages);
                channel.loading_older = false;
                // Another page would only push out what this one added.
                channel.has_older = has_more && channel.messages.len() < MESSAGE_LIMIT;
                main.chat.current.is(channel_id)
            };
            if !current {
                return Task::none();
            }
            channel_images(app, channel_id)
        }
        Event::OlderFailed { channel_id, error } => {
            if let Some(main) = app.main_mut() {
                let channel = main.chat.entry(channel_id);
                channel.loading_older = false;
                channel.history_error = Some(error);
            }
            Task::none()
        }
        Event::AttachmentUploaded {
            request_id,
            attachment,
        } => {
            let Some(main) = app.main_mut() else {
                return Task::none();
            };
            let Some(pending) = main.pending_uploads.remove(&request_id) else {
                tracing::debug!(request_id, "ignoring an upload nothing is waiting for");
                return Task::none();
            };
            main.chat.composer.uploading = main.chat.composer.uploading.saturating_sub(1);
            if main.chat.current.is(pending.channel_id) {
                main.chat.composer.attachments.push(attachment);
            } else {
                // Never linked to a message, so the server sweeps it.
                tracing::debug!(
                    id = attachment.id,
                    channel_id = pending.channel_id,
                    "dropping an upload for a channel no longer being written in"
                );
            }
            Task::none()
        }
        Event::UploadFailed { request_id, error } => {
            if let Some(main) = app.main_mut()
                && let Some(pending) = main.pending_uploads.remove(&request_id)
            {
                main.chat.composer.uploading = main.chat.composer.uploading.saturating_sub(1);
                tracing::warn!(
                    file_name = %pending.file_name,
                    channel_id = pending.channel_id,
                    %error,
                    "an attachment did not upload"
                );
            }
            app.toast(ToastKind::Error, error);
            Task::none()
        }
        Event::AttachmentFetched {
            request_id,
            id,
            bytes,
        } => on_fetched(app, request_id, ImageKey::Attachment(id), bytes),
        Event::FetchFailed {
            request_id,
            id,
            error,
        } => on_fetch_failed(app, request_id, ImageKey::Attachment(id), &error),
        Event::ImageFetched {
            request_id,
            id,
            bytes,
        } => on_fetched(app, request_id, ImageKey::Image(id), bytes),
        Event::ImageFetchFailed {
            request_id,
            id,
            error,
        } => on_fetch_failed(app, request_id, ImageKey::Image(id), &error),
        // An upload belongs to whichever page asked for it: the profile pages
        // first, the server settings otherwise.
        Event::ImageUploaded { request_id, image } => {
            match settings::on_image_uploaded(app, request_id, &image) {
                Some(task) => task,
                None => admin::on_image_uploaded(app, request_id, image),
            }
        }
        Event::ImageUploadFailed { request_id, error } => {
            match settings::on_image_upload_failed(app, request_id, &error) {
                Some(task) => task,
                None => admin::on_image_upload_failed(app, request_id, error),
            }
        }
        Event::SendDropped => {
            app.toast(ToastKind::Error, "Not connected".to_owned());
            Task::none()
        }
        Event::RestResult {
            request_id,
            outcome,
        } => admin::on_rest_result(app, request_id, outcome),
        Event::AdminDropped { kind } => admin::on_admin_dropped(app, kind),
        Event::VoiceReady { .. }
        | Event::VoiceState { .. }
        | Event::VoiceMemberJoined { .. }
        | Event::VoiceMemberLeft { .. }
        | Event::VoiceMoved { .. }
        | Event::Speaking { .. } => voice::on_event(app, event),
        Event::ShareStarted { .. }
        | Event::ShareStopped { .. }
        | Event::WatchState { .. }
        | Event::ShareWatchers { .. } => share::on_event(app, event),
    }
}

/// What one live message leaves behind.
struct Landed {
    /// The attachments to draw it with, when it landed in the channel in view.
    images: Vec<ImageKey>,
    /// The notification to raise, when it is worth one.
    notify: Option<(String, String)>,
}

fn on_message(app: &mut App, message: ChatMessage) -> Task<Message> {
    let focused = app.ui.focused;
    let suppress = app.config.suppress_everyone;
    let channel_id = message.channel_id;
    let muted = app.config.muted_channels.contains(&channel_id);
    if chat::reopen_on_message(&mut app.config.hidden_dms, channel_id) {
        app.save_config();
    }

    let landed = {
        let Some(main) = app.main_mut() else {
            return Task::none();
        };
        land(main, message, focused, suppress, muted)
    };

    let images = app.ensure_images(landed.images);
    match landed.notify {
        Some((title, body)) => Task::batch([images, app.notify_once(title, body)]),
        None => images,
    }
}

/// Puts one live message in its channel and works out what it means for the
/// counters, the jump pill and the notification.
fn land(
    main: &mut MainState,
    message: ChatMessage,
    focused: bool,
    suppress: bool,
    muted: bool,
) -> Landed {
    let channel_id = message.channel_id;
    let me = main.member_id;
    let foreign = message.author_id != me;
    let is_current = main.chat.current.is(channel_id);
    let is_dm = main
        .server
        .channel(channel_id)
        .is_some_and(|channel| channel_kind(channel) == ChannelKind::Dm);
    // A DM reads like a mention: it is addressed to this account and nobody else.
    let addressed = addressed_to_me(&message, me, suppress, is_dm);
    let counts = mention_counts(&message.mention_ids, me, message.mention_everyone);

    let author = if main.server.members.contains_key(&message.author_id) {
        main.server.display_name(message.author_id).to_owned()
    } else {
        // A message older than the account that wrote it, or one from somebody who
        // is no longer a member: the author's name travelled with it.
        message.author.clone()
    };
    let title = if is_dm {
        author
    } else {
        format!("{} · {author}", main.server.channel_title(channel_id))
    };
    let body = plain_text(&message.text, &main.user_pairs);
    let attachments = message.attachments.len();
    let images: Vec<ImageKey> = message
        .attachments
        .iter()
        .map(|attachment| ImageKey::Attachment(attachment.id))
        .collect();

    let channel = main.chat.entry(channel_id);
    let at_bottom = channel.at_bottom;
    let unread = unread_rule(foreign, is_current, at_bottom, focused);
    channel.merge(vec![message]);
    // At the bottom the anchor already keeps the newest message in view.
    if foreign && !at_bottom {
        channel.pending_new = channel.pending_new.saturating_add(1);
    }
    if unread {
        channel.unread = channel.unread.saturating_add(1);
        if counts {
            channel.mentions = channel.mentions.saturating_add(1);
        }
    }

    main.notice = None;
    if !unread && is_current && at_bottom && focused {
        main.chat.schedule_mark_read();
    }

    // Only the channel in view draws rows, so only its images are worth fetching.
    let images = if is_current { images } else { Vec::new() };
    let viewing = focused && is_current;
    // A muted channel still counts, it just never interrupts.
    if !foreign || muted || !notification_rule(addressed, focused, viewing) {
        return Landed {
            images,
            notify: None,
        };
    }
    Landed {
        images,
        notify: Some((
            title,
            notify::preview(&notification_body(&body, attachments)),
        )),
    }
}

/// Why the channel that just arrived is worth opening.
enum Opened {
    /// The create dialog asked for it, and closes with it.
    Created,
    /// An `OpenDm` this window sent.
    Dm,
    /// The first channel this account can look at.
    First,
}

/// Whether the channel that just arrived is the one to open, clearing whatever
/// asked for it. A channel arrives as a delta like any other, so being new is no
/// sign at all.
fn opens(main: &mut MainState, id: i64, dm: bool) -> Option<Opened> {
    let channel = main.server.channel(id)?;
    if dm {
        let partner = main.server.dm_partner(channel);
        if main.pending_dm.is_none() || main.pending_dm != partner {
            // Somebody else opened this conversation: it belongs in the list, not
            // in front of whatever is being read.
            return None;
        }
        main.pending_dm = None;
        main.chat.view = MainView::Dms;
        return Some(Opened::Dm);
    }
    if pending_channel_name(main.pending_channel.as_deref(), channel) {
        main.pending_channel = None;
        return Some(Opened::Created);
    }
    // An account that has nothing in view yet — a fresh one, or one that just
    // became able to see a channel.
    main.chat
        .current
        .channel_id()
        .is_none()
        .then_some(Opened::First)
}

/// Where a refusal is said. What the create dialogs asked for is answered inside
/// them: the status line behind a modal is not where a refused name goes.
fn on_server_error(app: &mut App, code: i32, detail: String, fatal: bool) -> Task<Message> {
    tracing::warn!(code, %detail, fatal, "the server refused something");

    // A refused share or join must not leave the voice card waiting for an answer
    // that is not coming; asserting either again on the next reconnect would only
    // repeat the refusal.
    share::on_server_error(app, code);
    if refused_join(code)
        && let Some(main) = app.main_mut()
        && main.voice.joining
    {
        main.voice.give_up_intents();
    }

    if code == ErrorCode::InvalidName as i32
        && matches!(
            app.ui.dialog,
            Some(Dialog::CreateChannel { .. } | Dialog::CreateCategory { .. })
        )
    {
        if let Some(main) = app.main_mut() {
            main.pending_channel = None;
        }
        if let Some(Dialog::CreateChannel { error, .. } | Dialog::CreateCategory { error, .. }) =
            &mut app.ui.dialog
        {
            *error = Some(detail);
        }
        return Task::none();
    }

    // A refusal the reader can do something about is said out loud, and the detail
    // of a `PERMISSION_DENIED` is the missing bit's wire name — never copy.
    if code == ErrorCode::PermissionDenied as i32 {
        let missing = permissions::parse(detail.trim())
            .map(permissions::label)
            .filter(|label| !label.is_empty());
        let text = match missing {
            Some(label) => format!("You need {label}"),
            None => "You don't have permission to do that".to_owned(),
        };
        app.toast(ToastKind::Error, text);
        return Task::none();
    }
    if code == ErrorCode::Hierarchy as i32 {
        app.toast(
            ToastKind::Error,
            "That member or role is above you".to_owned(),
        );
        return Task::none();
    }

    if let Some(main) = app.main_mut() {
        main.notice = Some(if detail.is_empty() {
            format!("The server refused that (code {code})")
        } else {
            detail
        });
    }
    Task::none()
}

/// The answers a `JoinVoice` gets that no reconnect can turn into a session: the
/// channel is gone or hidden, the relay is off, or `CONNECT` is missing.
fn refused_join(code: i32) -> bool {
    [
        ErrorCode::UnknownChannel,
        ErrorCode::VoiceUnavailable,
        ErrorCode::PermissionDenied,
    ]
    .iter()
    .any(|known| *known as i32 == code)
}

/// Caches one transferred image and turns it into pixels off the UI thread.
fn on_fetched(app: &mut App, request_id: u64, key: ImageKey, bytes: Blob) -> Task<Message> {
    if let Some(main) = app.main_mut() {
        main.pending_fetches.remove(&request_id);
    }
    decode_task(key, move || {
        images::store(key, &bytes);
        Ok(bytes)
    })
}

fn on_fetch_failed(app: &mut App, request_id: u64, key: ImageKey, error: &str) -> Task<Message> {
    tracing::warn!(?key, %error, "cannot fetch an image");
    if let Some(main) = app.main_mut() {
        main.pending_fetches.remove(&request_id);
        main.chat.images.insert(key, ImageState::Failed);
    }
    Task::none()
}

/// Every attachment of one channel, so a row is drawn with its image rather than
/// fetching one once it is on screen.
fn channel_images(app: &mut App, channel_id: i64) -> Task<Message> {
    let keys = app
        .main()
        .map(|main| main.chat.image_keys(channel_id))
        .unwrap_or_default();
    app.ensure_images(keys)
}

/// Every profile, role and server image the panes draw.
fn profile_images(server: &ServerModel) -> Vec<ImageKey> {
    server
        .image_ids()
        .into_iter()
        .map(ImageKey::Image)
        .collect()
}

/// One image id, or nothing when there is none.
fn one_image(id: i64) -> Vec<ImageKey> {
    if id == 0 {
        Vec::new()
    } else {
        vec![ImageKey::Image(id)]
    }
}

/// A ban or an unban has landed once the member delta arrives — the ban
/// transaction is committed by then, which `GET /api/bans` sent alongside the
/// command would not be — so that is when the open Bans page asks again.
fn bans_refresh(app: &App) -> Task<Message> {
    if matches!(app.ui.route, Route::ServerSettings(ServerTab::Bans)) {
        Task::done(Message::Admin(AdminMsg::BansRefresh))
    } else {
        Task::none()
    }
}

/// The channel the window opens when it has no better idea: the one that was in
/// view last, else general, else the first text channel there is.
fn first_channel(app: &App) -> Option<i64> {
    app.main()?.server.fallback_channel(app.config.last_channel)
}

/// Under `Anchor::End` a relative offset of zero is the bottom of the list.
fn snap_to_bottom() -> Task<Message> {
    operation::snap_to(
        Id::new(view::MESSAGES_ID),
        scrollable::RelativeOffset::START,
    )
}
