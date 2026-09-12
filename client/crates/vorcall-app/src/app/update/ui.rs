//! The window's own state: the pointer, the panes, the overlays and the toasts.
//!
//! Every dialog is submitted here rather than in the area it belongs to, because
//! the dialog *is* the form: what is being typed lives in [`Dialog`], and the
//! command it turns into is this module's to send.

use std::time::Instant;

use iced::Task;
use iced::widget::{Id, operation};
use vorcall_core::connection::{AdminCommand, Command};

use crate::app::App;
use crate::app::message::{
    AdminMsg, AuthMsg, ChannelsMsg, ChatMsg, CropMsg, DragMsg, Message, SettingsMsg, ShareMsg,
    ToastKind, UiMsg, VoiceMsg,
};
use crate::app::state::ui::{
    ContextMenu, Dialog, DialogAction, ProfileCard, Route, SwitchEntry, SwitchTarget,
    rank_switcher, switcher_entries, validate_long, validate_name, wrap_index,
};
use crate::app::update::{channels, chat, drag, voice};
use crate::view;

/// What a dialog says when the connection loop is not there to take its command:
/// the wording `MainState::send_or_notice` puts in the notice line.
const NOT_CONNECTED: &str = "Not connected";

pub fn update(app: &mut App, message: UiMsg) -> Task<Message> {
    match message {
        // Where the pointer is decides where a menu or a drag ghost is drawn, and
        // every mouse move reaches this.
        UiMsg::CursorMoved(at) => {
            app.ui.cursor = at;
            if let Some(drag) = &mut app.ui.drag {
                drag.at = at;
            }
            Task::none()
        }
        UiMsg::WindowResized(size) => {
            app.ui.window_size = size;
            Task::none()
        }
        UiMsg::ToggleMembers => {
            app.ui.show_members = !app.ui.show_members;
            app.config.show_members = app.ui.show_members;
            app.save_config();
            Task::none()
        }
        UiMsg::OpenDialog(Dialog::Action(action)) => perform(app, action),
        UiMsg::OpenDialog(dialog) => {
            app.ui.dialog = Some(dialog);
            Task::none()
        }
        UiMsg::CloseDialog => {
            app.ui.dialog = None;
            Task::none()
        }
        UiMsg::OpenProfileCard(user_id) => {
            // A card opened from a menu replaces it, rather than sitting under it.
            app.ui.context_menu = None;
            app.ui.profile_card = Some(ProfileCard {
                user_id,
                at: app.ui.cursor,
            });
            Task::none()
        }
        UiMsg::CloseProfileCard => {
            app.ui.profile_card = None;
            Task::none()
        }
        UiMsg::ExpandMember(user_id) => {
            if let Some(main) = app.main_mut() {
                main.voice.expanded_member = user_id;
            }
            Task::none()
        }
        UiMsg::ContextMenu(target) => {
            app.ui.profile_card = None;
            app.ui.context_menu = Some(ContextMenu {
                target,
                at: app.ui.cursor,
            });
            Task::none()
        }
        UiMsg::CloseContextMenu => {
            app.ui.context_menu = None;
            Task::none()
        }
        UiMsg::Toast(kind, text) => {
            app.toast(kind, text);
            Task::none()
        }
        UiMsg::DismissToast(id) => {
            app.ui.toasts.retain(|toast| toast.id != id);
            Task::none()
        }
        UiMsg::Escape => escape(app),
        UiMsg::OpenQuickSwitcher => {
            let switcher = &mut app.ui.quick_switcher;
            switcher.open = true;
            switcher.query.clear();
            switcher.selected = 0;
            operation::focus(Id::new(view::QUICK_SWITCHER_ID))
        }
        UiMsg::QuickSwitcherQuery(query) => {
            let switcher = &mut app.ui.quick_switcher;
            switcher.query = query;
            // The best match is what Enter takes, whatever was selected before.
            switcher.selected = 0;
            Task::none()
        }
        UiMsg::QuickSwitcherMove(delta) => {
            let count = switcher_matches(app).len();
            let switcher = &mut app.ui.quick_switcher;
            switcher.selected = wrap_index(switcher.selected, delta, count);
            Task::none()
        }
        UiMsg::QuickSwitcherPick => pick(app),
        UiMsg::CloseQuickSwitcher => {
            close_switcher(app);
            Task::none()
        }
        UiMsg::FocusNext => operation::focus_next(),
        UiMsg::FocusPrevious => operation::focus_previous(),
    }
}

/// Sweeps the toast stack. The tick only runs while there is something in it.
pub fn expire_toasts(app: &mut App) {
    app.ui.expire_toasts(Instant::now());
}

/// Unwinds one layer, outermost first, so nothing can trap the window and one
/// press never closes two things.
fn escape(app: &mut App) -> Task<Message> {
    // A row's own palette or armed delete is what the reader is looking at, even
    // with a menu open behind it.
    let in_a_row = app
        .main()
        .is_some_and(|main| main.chat.reacting.is_some() || main.chat.confirm_delete.is_some());
    if in_a_row {
        if let Some(main) = app.main_mut() {
            main.chat.reacting = None;
            main.chat.confirm_delete = None;
        }
        return Task::none();
    }
    if app.ui.context_menu.take().is_some() || app.ui.profile_card.take().is_some() {
        return Task::none();
    }
    if app.ui.quick_switcher.open {
        close_switcher(app);
        return Task::none();
    }
    if app.ui.dialog.take().is_some() {
        return Task::none();
    }
    if app.ui.drag.is_some() {
        return drag::update(app, DragMsg::DragCancel);
    }

    // A reply or an edit being written is the next thing in front of the reader.
    let composing = app.main().map(|main| {
        (
            main.chat.composer.editing.is_some(),
            main.chat.composer.reply_to.is_some(),
        )
    });
    match composing {
        Some((true, _)) => return chat::update(app, ChatMsg::CancelEdit),
        Some((_, true)) => return chat::update(app, ChatMsg::CancelReply),
        _ => {}
    }

    if !app.ui.route.is_main() {
        app.ui.route = Route::Main;
    }
    Task::none()
}

/// What a control in an overlay asked for. Every one of them dismisses the
/// transient overlays first: a menu, a card or the switcher is gone the moment it
/// is used.
fn perform(app: &mut App, action: DialogAction) -> Task<Message> {
    app.ui.context_menu = None;
    app.ui.profile_card = None;
    close_switcher(app);

    match action {
        DialogAction::Submit => submit(app),
        DialogAction::Copy { what, value } => {
            app.toast(ToastKind::Info, format!("{what} copied"));
            iced::clipboard::write(value)
        }
        DialogAction::Perform(message) => Task::done(*message),
    }
}

/// The dialog that is open, as the command it stands for. What the server owns —
/// a password, a theme name, the share that is starting — is handed to the area
/// that owns it instead.
fn submit(app: &mut App) -> Task<Message> {
    let Some(dialog) = app.ui.dialog.clone() else {
        return Task::none();
    };

    match dialog {
        Dialog::ChangePassword { .. } => Task::done(Message::Auth(AuthMsg::ChangePasswordSubmit)),
        Dialog::ThemeSaveAs { .. } => {
            Task::done(Message::Settings(SettingsMsg::ThemeSaveAsConfirm))
        }
        Dialog::SharePicker { .. } => Task::done(Message::Share(ShareMsg::Confirm)),
        // The adjuster owns the crop and the upload that follows it; pressing
        // through is all this dialog had to say.
        Dialog::CropImage { .. } => Task::done(Message::Crop(CropMsg::Apply)),
        // The diagnostics section owns the upload; this only answered the offer.
        Dialog::CrashReport => Task::done(Message::Settings(SettingsMsg::SendCrashReport)),
        Dialog::CreateChannel {
            category_id,
            kind,
            name,
            ..
        } => {
            let name = match validate_name(&name, "channel name") {
                Ok(name) => name,
                Err(complaint) => return complain(app, complaint),
            };
            let Some(main) = app.main_mut() else {
                return Task::none();
            };
            // The dialog stays up until the channel it asked for arrives, which
            // is what `update::events` closes it on — and where a refused name is
            // shown.
            main.pending_channel = Some(name.clone());
            let sent = main.send_command(Command::Admin(AdminCommand::CreateChannel {
                kind,
                name,
                topic: String::new(),
                category_id: category_id.unwrap_or_default(),
            }));
            if sent {
                return Task::none();
            }
            main.pending_channel = None;
            complain(app, NOT_CONNECTED.to_owned())
        }
        Dialog::CreateCategory { name, .. } => {
            let name = match validate_name(&name, "category name") {
                Ok(name) => name,
                Err(complaint) => return complain(app, complaint),
            };
            send_and_close(app, Command::Admin(AdminCommand::CreateCategory { name }))
        }
        Dialog::EditChannel {
            channel_id,
            name,
            topic,
        } => {
            let name = match validate_name(&name, "channel name") {
                Ok(name) => name,
                Err(complaint) => return complain(app, complaint),
            };
            let topic = match validate_long(&topic) {
                Ok(topic) => topic,
                Err(complaint) => return complain(app, complaint),
            };
            send_and_close(
                app,
                Command::Admin(AdminCommand::UpdateChannel {
                    id: channel_id,
                    name,
                    topic,
                }),
            )
        }
        Dialog::EditCategory { id, name } => {
            let name = match validate_name(&name, "category name") {
                Ok(name) => name,
                Err(complaint) => return complain(app, complaint),
            };
            send_and_close(
                app,
                Command::Admin(AdminCommand::UpdateCategory { id, name }),
            )
        }
        // The server settings own these three: their handlers send the command,
        // close this dialog and clear the draft the deleted thing was being
        // edited in.
        Dialog::ConfirmDeleteChannel { channel_id } => {
            Task::done(Message::Admin(AdminMsg::ChannelDelete(channel_id)))
        }
        Dialog::ConfirmDeleteCategory { id } => {
            Task::done(Message::Admin(AdminMsg::CategoryDelete(id)))
        }
        Dialog::ConfirmDeleteRole { role_id } => {
            Task::done(Message::Admin(AdminMsg::RoleDelete(role_id)))
        }
        // The message list owns what a deletion does to the row; this only asked.
        Dialog::ConfirmDeleteMessage { message_id } => {
            app.ui.dialog = None;
            chat::update(app, ChatMsg::ConfirmDelete(message_id))
        }
        Dialog::ConfirmKick { user_id } => {
            send_and_close(app, Command::Admin(AdminCommand::KickMember { user_id }))
        }
        Dialog::ConfirmTransferOwnership { user_id } => send_and_close(
            app,
            Command::Admin(AdminCommand::TransferOwnership { user_id }),
        ),
        // The bans page owns the reason draft and the confirm.
        Dialog::BanReason { .. } => Task::done(Message::Admin(AdminMsg::BanConfirm)),
        Dialog::Nickname { user_id, draft } => {
            // An empty nickname is a value: it clears whatever was set.
            let nickname = if draft.trim().is_empty() {
                String::new()
            } else {
                match validate_name(&draft, "nickname") {
                    Ok(nickname) => nickname,
                    Err(complaint) => return complain(app, complaint),
                }
            };
            send_and_close(
                app,
                Command::Admin(AdminCommand::SetNickname { user_id, nickname }),
            )
        }
        // Nothing to submit: these are read, not filled in.
        Dialog::Image(_) | Dialog::InviteCreated { .. } | Dialog::Action(_) => Task::none(),
    }
}

/// Sends one command and closes the dialog that asked for it.
fn send_and_close(app: &mut App, command: Command) -> Task<Message> {
    app.ui.dialog = None;
    if let Some(main) = app.main_mut() {
        main.send_or_notice(command);
    }
    Task::none()
}

/// Says a refusal in the dialog that caused it, or as a toast for the dialogs
/// that carry no error line of their own.
fn complain(app: &mut App, complaint: String) -> Task<Message> {
    if let Some(Dialog::CreateChannel { error, .. } | Dialog::CreateCategory { error, .. }) =
        app.dialog_mut()
    {
        *error = Some(complaint);
        return Task::none();
    }
    app.toast(ToastKind::Error, complaint);
    Task::none()
}

/// Opens whatever the quick switcher is on, and closes it.
fn pick(app: &mut App) -> Task<Message> {
    let chosen = switcher_matches(app)
        .get(app.ui.quick_switcher.selected)
        .map(|entry| entry.target);
    close_switcher(app);

    match chosen {
        // A conversation is opened the same way a channel is: both are channels.
        Some(SwitchTarget::Channel(id) | SwitchTarget::Dm(id)) => {
            channels::update(app, ChannelsMsg::Select(id))
        }
        Some(SwitchTarget::Voice(id)) => voice::update(app, VoiceMsg::Join(id)),
        Some(SwitchTarget::Member(user_id)) => channels::update(app, ChannelsMsg::OpenDm(user_id)),
        None => Task::none(),
    }
}

/// What the quick switcher is offering for what is typed in it. The view works
/// the same list out, so the row the keyboard is on is the row that is drawn.
pub fn switcher_matches(app: &App) -> Vec<SwitchEntry> {
    let Some(main) = app.main() else {
        return Vec::new();
    };
    rank_switcher(
        switcher_entries(main, &app.config.hidden_dms),
        &app.ui.quick_switcher.query,
    )
}

fn close_switcher(app: &mut App) {
    let switcher = &mut app.ui.quick_switcher;
    switcher.open = false;
    switcher.query.clear();
    switcher.selected = 0;
}
