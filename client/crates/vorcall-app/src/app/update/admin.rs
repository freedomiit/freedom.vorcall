//! The server settings forms.
//!
//! Every form here is a draft the page fills in and one frame the Save sends.
//! Nothing is applied locally: `PROTOCOL.md` § Channels and § Roles answer every
//! management frame with a broadcast, and that broadcast is what moves the model,
//! so a refusal leaves the window exactly as it was.
//!
//! The permission mirror greys the controls out; the server is still the
//! boundary, so a handler sends what its control offered and lets the server
//! have the last word.

use std::ffi::OsStr;
use std::path::Path;

use iced::Task;
use vorcall_core::connection::{AdminCommand, Blob, Command, RestKind, RestOutcome, RestRequest};
use vorcall_core::images::ImagePurpose;
use vorcall_core::{Channel, Image, Override, Role, attachments, permissions};

use crate::app::message::{
    AdminMsg, DragItem, Message, OverrideTargetKind, RoleIconDraft, ToastKind, TriState,
};
use crate::app::state::server::channel_kind;
use crate::app::state::settings::{
    CategoryDraft, ChannelDraft, DEFAULT_INVITE_DAYS, OverviewDraft, RestList, RoleDraft,
};
use crate::app::state::ui::Dialog;
use crate::app::update::drag;
use crate::app::{App, MainState};
use crate::workers::images::{self, ImageKey};

/// `PROTOCOL.md` § Channels and § Roles and permissions: what the server refuses
/// to exceed. The page mirrors them so a create that cannot succeed is not
/// offered.
pub const MAX_CATEGORIES: usize = 50;
pub const MAX_CHANNELS: usize = 200;
pub const MAX_OVERRIDES: usize = 100;
pub const MAX_ROLES: usize = 100;

/// `PROTOCOL.md` § REST: how long an invite may be asked to last.
pub const INVITE_DAYS_MIN: u32 = 1;
pub const INVITE_DAYS_MAX: u32 = 365;

/// `Role.icon_emoji` is at most two scalars.
const MAX_ICON_SCALARS: usize = 2;

/// What a form says when the loop is not there to take its frame.
const NOT_CONNECTED: &str = "Not connected";

pub fn update(app: &mut App, message: AdminMsg) -> Task<Message> {
    match message {
        // The drag has its own module; the arrows are the same arithmetic.
        AdminMsg::Drag(message) => drag::update(app, message),
        AdminMsg::MoveUp(item) => drag::nudge(app, item, true),
        AdminMsg::MoveDown(item) => drag::nudge(app, item, false),

        AdminMsg::OverviewName(name) => with_main(app, |main| {
            let mut draft = overview_draft(main);
            draft.name = name;
            main.admin.overview = draft;
        }),
        AdminMsg::OverviewDescription(description) => with_main(app, |main| {
            let mut draft = overview_draft(main);
            draft.description = description;
            main.admin.overview = draft;
        }),
        AdminMsg::OverviewPickIcon => {
            pick_image(ImagePurpose::ServerIcon, AdminMsg::OverviewIconPicked)
        }
        AdminMsg::OverviewIconPicked(picked) => match picked {
            // No bytes is how the page says "no icon"; see [`clear_icon`].
            Ok((_, bytes)) if bytes.is_empty() => with_main(app, |main| {
                let mut draft = overview_draft(main);
                draft.icon_image_id = 0;
                main.admin.overview = draft;
            }),
            picked => picked_image(app, ImagePurpose::ServerIcon, picked),
        },
        AdminMsg::OverviewSave => with_main(app, |main| {
            let draft = overview_draft(main);
            let name = draft.name.trim().to_owned();
            if name.is_empty() {
                main.notice = Some("A server name is required.".to_owned());
                return;
            }
            main.send_or_notice(Command::Admin(AdminCommand::UpdateServer {
                name,
                description: draft.description.trim().to_owned(),
                icon_image_id: draft.icon_image_id,
            }));
        }),
        AdminMsg::TransferOwnership(user_id) => {
            app.ui.dialog = Some(Dialog::ConfirmTransferOwnership { user_id });
            Task::none()
        }

        AdminMsg::ChannelDraftName(name) => with_main(app, |main| main.admin.channel.name = name),
        AdminMsg::ChannelDraftTopic(topic) => {
            with_main(app, |main| main.admin.channel.topic = topic)
        }
        AdminMsg::ChannelDraftKind(kind) => with_main(app, |main| main.admin.channel.kind = kind),
        AdminMsg::ChannelDraftCategory(category) => with_main(app, |main| {
            // A category only ever travels with a create — `UpdateChannel` carries
            // the name and the topic alone, and a move between categories is a
            // reorder — so choosing one is what starts a new channel.
            main.admin.channel.id = None;
            main.admin.channel.category_id = category;
        }),
        AdminMsg::ChannelSave => with_main(app, |main| {
            let draft = main.admin.channel.clone();
            let name = draft.name.trim().to_owned();
            if name.is_empty() {
                main.notice = Some("A channel name is required.".to_owned());
                return;
            }
            let topic = draft.topic.trim().to_owned();
            let command = match draft.id {
                Some(id) => AdminCommand::UpdateChannel { id, name, topic },
                None => AdminCommand::CreateChannel {
                    kind: draft.kind,
                    name,
                    topic,
                    category_id: draft.category_id.unwrap_or_default(),
                },
            };
            main.send_or_notice(Command::Admin(command));
            // A creation clears the form for the next one; an edit stays open.
            if draft.id.is_none() {
                main.admin.channel = ChannelDraft::default();
            }
        }),
        AdminMsg::ChannelDelete(id) => {
            app.ui.dialog = None;
            with_main(app, |main| {
                main.send_or_notice(Command::Admin(AdminCommand::DeleteChannel { id }));
                if main.admin.channel.id == Some(id) {
                    main.admin.channel = ChannelDraft::default();
                }
                if main
                    .admin
                    .override_target
                    .is_some_and(|(channel, ..)| channel == id)
                {
                    main.admin.override_target = None;
                }
            })
        }

        AdminMsg::CategoryDraftName(name) => with_main(app, |main| main.admin.category.name = name),
        AdminMsg::CategorySave => with_main(app, |main| {
            let draft = main.admin.category.clone();
            let name = draft.name.trim().to_owned();
            if name.is_empty() {
                main.notice = Some("A category name is required.".to_owned());
                return;
            }
            let command = match draft.id {
                Some(id) => AdminCommand::UpdateCategory { id, name },
                None => AdminCommand::CreateCategory { name },
            };
            main.send_or_notice(Command::Admin(command));
            if draft.id.is_none() {
                main.admin.category = CategoryDraft::default();
            }
        }),
        AdminMsg::CategoryDelete(id) => {
            app.ui.dialog = None;
            with_main(app, |main| {
                main.send_or_notice(Command::Admin(AdminCommand::DeleteCategory { id }));
                if main.admin.category.id == Some(id) {
                    main.admin.category = CategoryDraft::default();
                }
            })
        }

        AdminMsg::OverrideTarget(channel_id, kind, target_id) => with_main(app, |main| {
            main.admin.override_target = (target_id != 0).then_some((channel_id, kind, target_id));
            // Opening a channel's overrides is also what puts that channel in the
            // editor: no other frozen message names a channel to edit.
            if let Some(channel) = main.server.channel(channel_id) {
                main.admin.channel = ChannelDraft {
                    id: Some(channel.id),
                    kind: channel_kind(channel),
                    name: channel.name.clone(),
                    topic: channel.topic.clone(),
                    category_id: (channel.category_id != 0).then_some(channel.category_id),
                };
            }
        }),
        AdminMsg::OverrideSet(channel_id, kind, target_id, bit, state) => with_main(app, |main| {
            let Some(channel) = main.server.channel(channel_id) else {
                return;
            };
            let (allow, deny) = override_pair(channel, kind, target_id);
            let (allow, deny) = apply_tri(allow, deny, bit, state);
            main.send_or_notice(Command::Admin(AdminCommand::SetOverride {
                channel_id,
                override_: entry_for(kind, target_id, allow, deny),
            }));
        }),
        AdminMsg::OverrideRemove(channel_id, kind, target_id) => with_main(app, |main| {
            // `allow == deny == 0` is how the protocol spells a deletion.
            main.send_or_notice(Command::Admin(AdminCommand::SetOverride {
                channel_id,
                override_: entry_for(kind, target_id, 0, 0),
            }));
            if main.admin.override_target == Some((channel_id, kind, target_id)) {
                main.admin.override_target = None;
            }
        }),

        AdminMsg::RoleSelect(id) => with_main(app, |main| {
            main.admin.selected_role = Some(id);
            if let Some(role) = main.server.roles.get(&id) {
                main.admin.role = RoleDraft::from_role(role);
            }
        }),
        AdminMsg::RoleDraftName(name) => with_main(app, |main| main.admin.role.name = name),
        AdminMsg::RoleDraftColor(color) => with_main(app, |main| main.admin.role.color = color),
        AdminMsg::RoleDraftIcon(icon) => with_main(app, |main| {
            main.admin.role.icon = match icon {
                RoleIconDraft::Emoji(emoji) => RoleIconDraft::Emoji(clamp_emoji(&emoji)),
                other => other,
            };
        }),
        AdminMsg::RoleDraftHoist(hoist) => with_main(app, |main| main.admin.role.hoist = hoist),
        AdminMsg::RoleDraftPermission(bit, granted) => with_main(app, |main| {
            let permissions = main.admin.role.permissions;
            main.admin.role.permissions = permissions::clean(if granted {
                permissions | bit
            } else {
                permissions & !bit
            });
        }),
        AdminMsg::RoleSave => with_main(app, |main| {
            let draft = main.admin.role.clone();
            match draft.id {
                Some(id) => {
                    let Some(current) = main.server.roles.get(&id).cloned() else {
                        return;
                    };
                    // `@everyone` takes only a permission change; the rest of it is
                    // resent exactly as it stands.
                    let role = if current.everyone {
                        Role {
                            permissions: permissions::clean(draft.permissions),
                            ..current
                        }
                    } else {
                        role_from_draft(&draft, current.position, false)
                    };
                    if role.name.is_empty() {
                        main.notice = Some("A role name is required.".to_owned());
                        return;
                    }
                    main.send_or_notice(Command::Admin(AdminCommand::UpdateRole { role }));
                }
                None => {
                    let role = role_from_draft(&draft, 0, false);
                    if role.name.is_empty() {
                        main.notice = Some("A role name is required.".to_owned());
                        return;
                    }
                    main.send_or_notice(Command::Admin(AdminCommand::CreateRole {
                        name: role.name,
                        color: role.color,
                        icon_emoji: role.icon_emoji,
                        icon_image_id: role.icon_image_id,
                        permissions: role.permissions,
                        hoist: role.hoist,
                    }));
                    main.admin.role = RoleDraft::default();
                }
            }
        }),
        AdminMsg::RoleCreate => with_main(app, |main| {
            main.admin.selected_role = None;
            main.admin.role = RoleDraft::default();
        }),
        AdminMsg::RoleDelete(id) => {
            app.ui.dialog = None;
            with_main(app, |main| {
                main.send_or_notice(Command::Admin(AdminCommand::DeleteRole { id }));
                if main.admin.selected_role == Some(id) {
                    main.admin.selected_role = None;
                    main.admin.role = RoleDraft::default();
                }
            })
        }
        AdminMsg::RolePickIcon => pick_image(ImagePurpose::RoleIcon, AdminMsg::RoleIconPicked),
        AdminMsg::RoleIconPicked(picked) => picked_image(app, ImagePurpose::RoleIcon, picked),

        AdminMsg::MemberSearch(query) => with_main(app, |main| main.admin.member_search = query),
        AdminMsg::MemberAddRole(user_id, role_id) => with_main(app, |main| {
            let Some(member) = main.server.members.get(&user_id) else {
                return;
            };
            let role_ids = with_role(&member.role_ids, role_id);
            main.send_or_notice(Command::Admin(AdminCommand::SetMemberRoles {
                user_id,
                role_ids,
            }));
        }),
        AdminMsg::MemberRemoveRole(user_id, role_id) => with_main(app, |main| {
            let Some(member) = main.server.members.get(&user_id) else {
                return;
            };
            let role_ids = without_role(&member.role_ids, role_id);
            main.send_or_notice(Command::Admin(AdminCommand::SetMemberRoles {
                user_id,
                role_ids,
            }));
        }),
        AdminMsg::MemberNickname(user_id, nickname) => with_main(app, |main| {
            main.admin.nicknames.insert(user_id, nickname);
        }),
        AdminMsg::MemberNicknameSave(user_id) => with_main(app, |main| {
            let Some(nickname) = main.admin.nicknames.remove(&user_id) else {
                return;
            };
            main.send_or_notice(Command::Admin(AdminCommand::SetNickname {
                user_id,
                nickname: nickname.trim().to_owned(),
            }));
        }),
        AdminMsg::Ban(user_id) => {
            if let Some(main) = app.main_mut() {
                main.admin.ban_reason.clear();
            }
            app.ui.dialog = Some(Dialog::BanReason {
                user_id,
                reason: String::new(),
            });
            Task::none()
        }
        AdminMsg::BanReason(reason) => {
            if let Some(main) = app.main_mut() {
                main.admin.ban_reason = reason.clone();
            }
            // The dialog draws its own copy; the draft is what the confirm reads.
            if let Some(Dialog::BanReason { reason: typed, .. }) = app.ui.dialog.as_mut() {
                *typed = reason;
            }
            Task::none()
        }
        AdminMsg::BanConfirm => {
            let Some(Dialog::BanReason { user_id, reason }) = app.ui.dialog.clone() else {
                return Task::none();
            };
            app.ui.dialog = None;
            let Some(main) = app.main_mut() else {
                return Task::none();
            };
            // Whichever was typed into: the dialog while it was open, the draft
            // otherwise.
            let reason = if reason.trim().is_empty() {
                main.admin.ban_reason.clone()
            } else {
                reason
            };
            main.send_or_notice(Command::Admin(AdminCommand::BanMember {
                user_id,
                reason: reason.trim().to_owned(),
            }));
            main.admin.ban_reason.clear();
            // The list is asked for again by the `MemberRemoved` the committed ban
            // broadcasts, never here: a `GET /api/bans` sent now can be answered
            // before the ban transaction lands.
            Task::none()
        }
        AdminMsg::Unban(user_id) => {
            let Some(main) = app.main_mut() else {
                return Task::none();
            };
            main.send_or_notice(Command::Admin(AdminCommand::UnbanMember { user_id }));
            // The `MemberUpdated` carrying the profile back is what refreshes it.
            Task::none()
        }

        AdminMsg::InvitesRefresh => with_main(app, |main| {
            main.admin.invites = RestList::Loading;
            rest(main, RestKind::ListInvites);
        }),
        AdminMsg::InviteDays(days) => {
            with_main(app, |main| main.admin.invite_days = invite_days(days))
        }
        AdminMsg::InviteCreate => with_main(app, |main| {
            let days = invite_days(main.admin.invite_days);
            rest(main, RestKind::CreateInvite { days });
        }),
        AdminMsg::InviteRevoke(id) => {
            with_main(app, |main| rest(main, RestKind::RevokeInvite { id }))
        }
        AdminMsg::BansRefresh => with_main(app, |main| {
            main.admin.bans = RestList::Loading;
            rest(main, RestKind::ListBans);
        }),
    }
}

/// One finished REST request: whichever list asked for it, or the toast a failure
/// deserves.
pub fn on_rest_result(
    app: &mut App,
    request_id: u64,
    outcome: Result<RestOutcome, String>,
) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    // A request this page never made: another area's, or one from a session that
    // is gone.
    let Some(kind) = main.admin.pending.remove(&request_id) else {
        return Task::none();
    };

    // What is left for the window once the lists have taken their own part of the
    // answer: a code to show, a list to read again, a complaint to raise.
    let mut code = None;
    let mut refresh = false;
    let mut failure = None;

    match outcome {
        Ok(RestOutcome::Invites(invites)) => main.admin.invites = RestList::Ready(invites),
        Ok(RestOutcome::Bans(bans)) => main.admin.bans = RestList::Ready(bans),
        Ok(RestOutcome::InviteCreated(created)) => {
            code = Some(created.code);
            refresh = true;
        }
        Ok(RestOutcome::InviteRevoked { .. }) => refresh = true,
        // The snapshot already carries the roster; nothing here asks for it.
        Ok(RestOutcome::Members(members)) => {
            tracing::debug!(count = members.len(), "the member list was not asked for");
        }
        Err(error) => {
            match kind {
                RestKind::ListInvites => main.admin.invites = RestList::Failed(error.clone()),
                RestKind::ListBans => main.admin.bans = RestList::Failed(error.clone()),
                RestKind::CreateInvite { .. }
                | RestKind::RevokeInvite { .. }
                | RestKind::ListMembers => {}
            }
            failure = Some(error);
        }
    }

    // Shown once and never logged: the server keeps only its hash.
    if let Some(code) = code {
        app.ui.dialog = Some(Dialog::InviteCreated { code });
    }
    if let Some(error) = failure {
        app.toast(ToastKind::Error, error);
    }
    if refresh {
        return Task::done(Message::Admin(AdminMsg::InvitesRefresh));
    }
    Task::none()
}

/// A management frame the loop could not take.
pub fn on_admin_dropped(app: &mut App, kind: &'static str) -> Task<Message> {
    tracing::warn!(frame = kind, "a management frame was not sent");
    app.toast(ToastKind::Error, NOT_CONNECTED.to_owned());
    Task::none()
}

/// An image this page uploaded, which becomes the draft's new picture.
pub fn on_image_uploaded(app: &mut App, request_id: u64, image: Image) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    // An upload nothing here started: the profile pages answered it already.
    let Some(purpose) = main.admin.pending_images.remove(&request_id) else {
        return Task::none();
    };
    match purpose {
        ImagePurpose::ServerIcon => {
            let mut draft = overview_draft(main);
            draft.icon_image_id = image.id;
            main.admin.overview = draft;
        }
        ImagePurpose::RoleIcon => main.admin.role.icon = RoleIconDraft::Image(image.id),
        // An avatar or a banner belongs to the profile page, which answered first.
        ImagePurpose::Avatar | ImagePurpose::Banner => return Task::none(),
    }
    app.ensure_image(ImageKey::Image(image.id))
}

/// An upload this page started that the server refused.
pub fn on_image_upload_failed(app: &mut App, request_id: u64, error: String) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    if main.admin.pending_images.remove(&request_id).is_none() {
        return Task::none();
    }
    app.toast(ToastKind::Error, error);
    Task::none()
}

/// The overview as the page edits it. A draft still untouched has never been
/// seeded — the page can be opened without anything being typed — so it reads the
/// server instead.
pub fn overview_draft(main: &MainState) -> OverviewDraft {
    if main.admin.overview == OverviewDraft::default() {
        OverviewDraft::from_server(&main.server.server)
    } else {
        main.admin.overview.clone()
    }
}

/// What the Overview page's Remove sends. `AdminMsg` has no "clear the icon" of
/// its own, and a pick that carries no bytes is exactly that.
pub fn clear_icon() -> Message {
    Message::Admin(AdminMsg::OverviewIconPicked(Ok((
        String::new(),
        Blob::from(Vec::new()),
    ))))
}

/// How long the next invite is asked to last; `0` is a draft nobody has set.
pub fn invite_days(days: u32) -> u32 {
    if days == 0 {
        DEFAULT_INVITE_DAYS
    } else {
        days.clamp(INVITE_DAYS_MIN, INVITE_DAYS_MAX)
    }
}

/// At most two scalars, as `Role.icon_emoji` allows.
pub fn clamp_emoji(emoji: &str) -> String {
    emoji.chars().take(MAX_ICON_SCALARS).collect()
}

/// The role one draft describes. `position` comes from the role as it stands:
/// `UpdateRole` ignores it and reordering is a frame of its own.
pub fn role_from_draft(draft: &RoleDraft, position: i32, everyone: bool) -> Role {
    let (icon_emoji, icon_image_id) = match &draft.icon {
        RoleIconDraft::None => (String::new(), 0),
        RoleIconDraft::Emoji(emoji) => (clamp_emoji(emoji), 0),
        RoleIconDraft::Image(id) => (String::new(), *id),
    };
    Role {
        id: draft.id.unwrap_or_default(),
        name: draft.name.trim().to_owned(),
        color: draft.color,
        icon_emoji,
        icon_image_id,
        position,
        permissions: permissions::clean(draft.permissions),
        hoist: draft.hoist,
        everyone,
    }
}

/// The override one channel holds for that role or member.
pub fn override_of(
    channel: &Channel,
    kind: OverrideTargetKind,
    target_id: i64,
) -> Option<&Override> {
    channel.overrides.iter().find(|entry| match kind {
        OverrideTargetKind::Role => entry.role_id == target_id,
        OverrideTargetKind::Member => entry.user_id == target_id,
    })
}

/// What that override allows and denies today; nothing at all is inherit-only.
pub fn override_pair(channel: &Channel, kind: OverrideTargetKind, target_id: i64) -> (u64, u64) {
    override_of(channel, kind, target_id).map_or((0, 0), |entry| (entry.allow, entry.deny))
}

/// One bit of an override set to one state, against what the channel holds now.
/// Only channel-scoped bits survive: `PROTOCOL.md` § Roles and permissions masks
/// the rest away anyway.
pub fn apply_tri(allow: u64, deny: u64, bit: u64, state: TriState) -> (u64, u64) {
    let bit = bit & permissions::CHANNEL_SCOPED;
    let mut allow = allow & permissions::CHANNEL_SCOPED & !bit;
    let mut deny = deny & permissions::CHANNEL_SCOPED & !bit;
    match state {
        TriState::Inherit => {}
        TriState::Allow => allow |= bit,
        TriState::Deny => deny |= bit,
    }
    (allow, deny)
}

/// Which of the three states one bit of an override is in.
pub fn tri_of(allow: u64, deny: u64, bit: u64) -> TriState {
    if permissions::has(allow, bit) {
        TriState::Allow
    } else if permissions::has(deny, bit) {
        TriState::Deny
    } else {
        TriState::Inherit
    }
}

/// A member's roles with one added, ascending and without repeats.
pub fn with_role(role_ids: &[i64], role_id: i64) -> Vec<i64> {
    let mut roles: Vec<i64> = role_ids.to_vec();
    if !roles.contains(&role_id) {
        roles.push(role_id);
    }
    roles.sort_unstable();
    roles.dedup();
    roles
}

/// A member's roles with one taken away.
pub fn without_role(role_ids: &[i64], role_id: i64) -> Vec<i64> {
    role_ids
        .iter()
        .copied()
        .filter(|id| *id != role_id)
        .collect()
}

/// Which arrow the page drew, as the nudge takes it.
pub fn move_item(item: DragItem, up: bool) -> AdminMsg {
    if up {
        AdminMsg::MoveUp(item)
    } else {
        AdminMsg::MoveDown(item)
    }
}

/// One override row addressed to a role or to a single member.
fn entry_for(kind: OverrideTargetKind, target_id: i64, allow: u64, deny: u64) -> Override {
    let (role_id, user_id) = match kind {
        OverrideTargetKind::Role => (target_id, 0),
        OverrideTargetKind::Member => (0, target_id),
    };
    Override {
        role_id,
        user_id,
        allow,
        deny,
    }
}

/// Runs one form action against the main state, or nothing at all while the
/// window is still on a sign-in screen.
fn with_main(app: &mut App, act: impl FnOnce(&mut MainState)) -> Task<Message> {
    if let Some(main) = app.main_mut() {
        act(main);
    }
    Task::none()
}

/// Sends one REST request and remembers what its answer is for.
fn rest(main: &mut MainState, kind: RestKind) {
    let request_id = main.next_request_id();
    if main.send_command(Command::Rest(RestRequest {
        request_id,
        kind: kind.clone(),
    })) {
        main.admin.pending.insert(request_id, kind);
        return;
    }

    main.notice = Some(NOT_CONNECTED.to_owned());
    // Nothing is in flight, so the list says so rather than spinning for good.
    match kind {
        RestKind::ListInvites => main.admin.invites = RestList::Failed(NOT_CONNECTED.to_owned()),
        RestKind::ListBans => main.admin.bans = RestList::Failed(NOT_CONNECTED.to_owned()),
        RestKind::CreateInvite { .. } | RestKind::RevokeInvite { .. } | RestKind::ListMembers => {}
    }
}

/// The file dialog, the read and the downscale, none of them on the UI thread.
/// Cancelling the dialog picks nothing, which is not an error.
fn pick_image(
    purpose: ImagePurpose,
    into: fn(Result<(String, Blob), String>) -> AdminMsg,
) -> Task<Message> {
    Task::perform(
        async move {
            let picked = rfd::AsyncFileDialog::new()
                .add_filter("Images", &["png", "jpg", "jpeg", "gif", "webp"])
                .pick_file()
                .await;
            let path = picked?.path().to_path_buf();
            Some(
                tokio::task::spawn_blocking(move || read_and_resize(&path, purpose))
                    .await
                    .unwrap_or_else(|e| Err(e.to_string())),
            )
        },
        move |picked| match picked {
            Some(result) => Message::Admin(into(result)),
            None => Message::Noop,
        },
    )
}

/// One picked file, read and scaled into the box its purpose allows, so nothing
/// larger than what will ever be drawn leaves this machine.
fn read_and_resize(path: &Path, purpose: ImagePurpose) -> Result<(String, Blob), String> {
    let name = file_name(path);
    // Asked before the read: nothing the server would refuse belongs in memory.
    let size = std::fs::metadata(path)
        .map_err(|e| format!("cannot read that file: {e}"))?
        .len();
    if size > attachments::MAX_BYTES as u64 {
        return Err("that image is over 8 MiB".to_owned());
    }

    let bytes = std::fs::read(path).map_err(|e| format!("cannot read that file: {e}"))?;
    let (_content_type, scaled) = images::resize_for_upload(&bytes, purpose)?;
    Ok((name, Blob::from(scaled)))
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("image")
        .to_owned()
}

/// What one picked image does: go up, or say why it cannot.
fn picked_image(
    app: &mut App,
    purpose: ImagePurpose,
    picked: Result<(String, Blob), String>,
) -> Task<Message> {
    with_main(app, |main| match picked {
        Ok((_name, bytes)) => upload_image(main, purpose, bytes),
        Err(error) => main.notice = Some(error),
    })
}

/// Starts one image upload. The content type is read back off the scaled bytes:
/// a `&'static str` cannot travel inside the picked message's `(String, Blob)`.
fn upload_image(main: &mut MainState, purpose: ImagePurpose, bytes: Blob) {
    let Some(content_type) = attachments::sniff(&bytes) else {
        main.notice = Some("that is not a PNG, JPEG, GIF or WebP".to_owned());
        return;
    };

    let request_id = main.next_request_id();
    if main.send_command(Command::UploadImage {
        request_id,
        purpose,
        content_type,
        bytes,
    }) {
        main.admin.pending_images.insert(request_id, purpose);
    } else {
        main.notice = Some(NOT_CONNECTED.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use vorcall_core::permissions as perms;

    use super::*;

    #[test]
    fn one_bit_of_an_override_moves_between_the_three_states() {
        // Inherit leaves neither side carrying it.
        let (allow, deny) = apply_tri(0, 0, perms::SEND_MESSAGES, TriState::Deny);
        assert_eq!((allow, deny), (0, perms::SEND_MESSAGES));
        assert_eq!(tri_of(allow, deny, perms::SEND_MESSAGES), TriState::Deny);

        // Allowing it takes it off the deny side in the same step.
        let (allow, deny) = apply_tri(allow, deny, perms::SEND_MESSAGES, TriState::Allow);
        assert_eq!((allow, deny), (perms::SEND_MESSAGES, 0));
        assert_eq!(tri_of(allow, deny, perms::SEND_MESSAGES), TriState::Allow);

        let (allow, deny) = apply_tri(allow, deny, perms::SEND_MESSAGES, TriState::Inherit);
        assert_eq!((allow, deny), (0, 0));
        assert_eq!(tri_of(allow, deny, perms::SEND_MESSAGES), TriState::Inherit);
    }

    #[test]
    fn an_override_keeps_the_bits_it_already_carried() {
        let (allow, deny) = apply_tri(
            perms::VIEW_CHANNEL,
            perms::SPEAK,
            perms::ATTACH_FILES,
            TriState::Deny,
        );

        assert_eq!(allow, perms::VIEW_CHANNEL);
        assert_eq!(deny, perms::SPEAK | perms::ATTACH_FILES);
    }

    /// `PROTOCOL.md` § Roles and permissions: an override never carries a
    /// server-scoped bit, whatever the page asks for.
    #[test]
    fn a_server_scoped_bit_never_reaches_an_override() {
        let (allow, deny) = apply_tri(perms::BAN_MEMBERS, 0, perms::MANAGE_SERVER, TriState::Allow);

        assert_eq!(allow, 0);
        assert_eq!(deny, 0);
    }

    #[test]
    fn a_role_draft_becomes_the_role_the_frame_carries() {
        let draft = RoleDraft {
            id: Some(4),
            name: "  Mods  ".to_owned(),
            color: 0x00_5C_C7_75,
            icon: RoleIconDraft::Emoji("🛡️🛡️🛡️".to_owned()),
            hoist: true,
            // An unknown bit is dropped, exactly as the server drops it.
            permissions: perms::KICK_MEMBERS | (1 << 40),
        };

        let role = role_from_draft(&draft, 3, false);
        assert_eq!(role.id, 4);
        assert_eq!(role.name, "Mods");
        assert_eq!(role.position, 3);
        assert_eq!(role.permissions, perms::KICK_MEMBERS);
        assert_eq!(role.icon_image_id, 0);
        assert_eq!(role.icon_emoji.chars().count(), 2);
        assert!(role.hoist);
        assert!(!role.everyone);
    }

    #[test]
    fn a_role_draft_with_an_image_carries_no_emoji() {
        let draft = RoleDraft {
            id: None,
            name: "Gamers".to_owned(),
            color: 0,
            icon: RoleIconDraft::Image(9),
            hoist: false,
            permissions: 0,
        };

        let role = role_from_draft(&draft, 0, false);
        assert_eq!(role.icon_image_id, 9);
        assert!(role.icon_emoji.is_empty());
        assert_eq!(role.id, 0);
    }

    #[test]
    fn a_member_role_set_adds_and_removes_one_role() {
        assert_eq!(with_role(&[3, 1], 2), [1, 2, 3]);
        // Already held: the set is unchanged but still ordered.
        assert_eq!(with_role(&[3, 1], 3), [1, 3]);
        assert_eq!(without_role(&[1, 2, 3], 2), [1, 3]);
        assert_eq!(without_role(&[1, 2], 9), [1, 2]);
        assert!(without_role(&[4], 4).is_empty());
    }

    #[test]
    fn an_invite_lasts_a_week_until_the_page_says_otherwise() {
        assert_eq!(invite_days(0), DEFAULT_INVITE_DAYS);
        assert_eq!(invite_days(1), 1);
        assert_eq!(invite_days(365), 365);
        assert_eq!(invite_days(10_000), INVITE_DAYS_MAX);
    }

    #[test]
    fn an_emoji_icon_is_two_scalars_at_most() {
        assert_eq!(clamp_emoji(""), "");
        assert_eq!(clamp_emoji("🛡"), "🛡");
        assert_eq!(clamp_emoji("abc").chars().count(), 2);
    }

    #[test]
    fn an_override_row_names_a_role_or_a_member_but_never_both() {
        let role = entry_for(OverrideTargetKind::Role, 7, 1, 2);
        assert_eq!((role.role_id, role.user_id), (7, 0));

        let member = entry_for(OverrideTargetKind::Member, 7, 1, 2);
        assert_eq!((member.role_id, member.user_id), (0, 7));
    }

    #[test]
    fn the_override_a_channel_holds_is_found_by_its_target() {
        let channel = Channel {
            id: 1,
            overrides: vec![
                Override {
                    role_id: 2,
                    user_id: 0,
                    allow: perms::VIEW_CHANNEL,
                    deny: 0,
                },
                Override {
                    role_id: 0,
                    user_id: 5,
                    allow: 0,
                    deny: perms::SEND_MESSAGES,
                },
            ],
            ..Channel::default()
        };

        assert_eq!(
            override_pair(&channel, OverrideTargetKind::Role, 2),
            (perms::VIEW_CHANNEL, 0)
        );
        assert_eq!(
            override_pair(&channel, OverrideTargetKind::Member, 5),
            (0, perms::SEND_MESSAGES)
        );
        assert_eq!(override_pair(&channel, OverrideTargetKind::Role, 5), (0, 0));
    }
}
