//! The right-click menu: what each target offers, and why a row is greyed out.
//!
//! Every item closes the menu before it acts. `UiMsg` has no message that does
//! both, so an item's own message travels inside a
//! [`DialogAction::Perform`][crate::app::state::ui::DialogAction::Perform], which
//! `update::ui` unwraps once the overlays are down.
//!
//! The permission checks here only hide and grey out — the server is still the
//! boundary — so a mirror that disagrees costs a disabled row, never an accepted
//! frame.

use iced::alignment::Vertical;
use iced::widget::{Space, button, column, container, mouse_area, row, stack, text, tooltip};
use iced::{Element, Length, Padding, Point, Size};
use vorcall_core::permissions;
use vorcall_core::{ChannelKind, VoiceMember};

use crate::app::message::{
    ChannelsMsg, ChatMsg, MenuTarget, Message, SettingsMsg, ShareMsg, UiMsg, VoiceMsg,
};
use crate::app::state::server::{ServerModel, channel_kind};
use crate::app::state::settings::ServerTab;
use crate::app::state::ui::Dialog;
use crate::app::{App, MainState};
use crate::icons::{self, Icon};
use crate::theme::ThemeTokens;
use crate::theme::styles;
use crate::view::TEXT_ROW;
use crate::view::overlays::{copies, opens, perform};
use crate::view::widgets;

/// How wide the menu is, and what one row of it costs — the height is estimated
/// rather than measured, because the popover has to be placed before it is laid
/// out.
const MENU_WIDTH: f32 = 200.0;
const MENU_PADDING: f32 = 6.0;
const ROW_SPACING: f32 = 2.0;
const ROW_HEIGHT: f32 = 26.0;
const LABEL_HEIGHT: f32 = 18.0;
const SEPARATOR_HEIGHT: f32 = 9.0;
const MENU_ICON: f32 = 14.0;

/// The reasons a row is greyed out, worded the way the server words its refusal.
const MISSING_CHANNELS: &str = "Missing permission: Manage channels";
const MISSING_ROLES: &str = "Missing permission: Manage roles";
const MISSING_MEMBERS: &str = "Missing permission: Manage members";
const MISSING_NICKNAME: &str = "Missing permission: Change nickname";
const MISSING_SEND: &str = "Missing permission: Send messages";
const MISSING_REACTIONS: &str = "Missing permission: Add reactions";
const MISSING_MESSAGES: &str = "Missing permission: Manage messages";
const MISSING_MUTE: &str = "Missing permission: Mute members";
const MISSING_DEAFEN: &str = "Missing permission: Deafen members";
const MISSING_MOVE: &str = "Missing permission: Move members";
const MISSING_KICK: &str = "Missing permission: Kick members";
const MISSING_BAN: &str = "Missing permission: Ban members";
const NOT_IN_CHANNEL: &str = "Join the channel to watch";
const IS_SELF: &str = "That is you";
const IS_OWNER: &str = "They own the server";
const OUTRANKED: &str = "They outrank you";
const DELETED: &str = "The message is deleted";

pub fn view<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let Some(menu) = app.ui.context_menu else {
        return Space::new().into();
    };
    let entries = entries(app, main, menu.target);
    if entries.is_empty() {
        return Space::new().into();
    }

    let tokens = &app.tokens;
    let at = app
        .ui
        .clamp_popover(menu.at, Size::new(MENU_WIDTH, height_of(&entries)));

    let mut list = column![].spacing(ROW_SPACING).width(Length::Fill);
    for entry in entries {
        list = list.push(row_of(entry, tokens));
    }
    let card = container(list)
        .width(MENU_WIDTH)
        .padding(MENU_PADDING)
        .style(styles::container::popover(tokens));

    // A press anywhere else is what closes it, whichever button it was.
    let dismiss = mouse_area(Space::new().width(Length::Fill).height(Length::Fill))
        .on_press(Message::Ui(UiMsg::CloseContextMenu))
        .on_right_press(Message::Ui(UiMsg::CloseContextMenu));

    stack![dismiss, anchored(card.into(), at)]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Puts one popover at `at`, in a layer that fills the window.
pub fn anchored<'a>(content: Element<'a, Message>, at: Point) -> Element<'a, Message> {
    container(content)
        .padding(Padding {
            top: at.y,
            right: 0.0,
            bottom: 0.0,
            left: at.x,
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// One row of a menu.
struct Item {
    label: String,
    icon: Icon,
    message: Message,
    danger: bool,
    enabled: bool,
    /// Why it cannot be used, which is what its tooltip says.
    why_disabled: &'static str,
}

impl Item {
    fn new(label: impl Into<String>, icon: Icon, message: Message) -> Self {
        Self {
            label: label.into(),
            icon,
            message,
            danger: false,
            enabled: true,
            why_disabled: "",
        }
    }

    /// Anything that destroys something.
    fn danger(mut self) -> Self {
        self.danger = true;
        self
    }

    /// Greys the row out with a reason. The first reason that applies is the one
    /// shown: it is the most specific, because the checks are written in that
    /// order.
    fn needs(mut self, allowed: bool, why: &'static str) -> Self {
        if self.enabled && !allowed {
            self.enabled = false;
            self.why_disabled = why;
        }
        self
    }
}

enum Entry {
    /// Boxed: an item carries a whole [`Message`], and the other two entries are
    /// a word at most.
    Item(Box<Item>),
    Separator,
    /// A heading over a group of rows, for the voice channels to move into.
    Label(&'static str),
}

impl Entry {
    fn item(item: Item) -> Self {
        Self::Item(Box::new(item))
    }
}

/// What the menu costs in height, near enough to keep it inside the window.
fn height_of(entries: &[Entry]) -> f32 {
    let rows: f32 = entries
        .iter()
        .map(|entry| {
            ROW_SPACING
                + match entry {
                    Entry::Item(_) => ROW_HEIGHT,
                    Entry::Label(_) => LABEL_HEIGHT,
                    Entry::Separator => SEPARATOR_HEIGHT,
                }
        })
        .sum();
    rows + MENU_PADDING * 2.0
}

fn row_of<'a>(entry: Entry, tokens: &'a ThemeTokens) -> Element<'a, Message> {
    let item = match entry {
        Entry::Item(item) => item,
        Entry::Label(label) => {
            return container(widgets::section_label(label, tokens))
                .padding([4.0, 6.0])
                .into();
        }
        Entry::Separator => {
            return container(
                container(Space::new().width(Length::Fill).height(1.0))
                    .style(styles::container::divider(tokens)),
            )
            .padding([4.0, 2.0])
            .into();
        }
    };

    let color = if !item.enabled {
        tokens.text_muted
    } else if item.danger {
        tokens.danger
    } else {
        tokens.text_secondary
    };
    let line = row![
        icons::icon(item.icon, MENU_ICON, color),
        text(item.label).size(TEXT_ROW).color(color),
    ]
    .spacing(8)
    .align_y(Vertical::Center);

    let mut control = button(line)
        .width(Length::Fill)
        .padding([4.0, 6.0])
        .style(styles::button::row(tokens));
    if item.enabled {
        control = control.on_press(item.message);
    }
    widgets::tooltip_of(control, item.why_disabled, tooltip::Position::Right, tokens)
}

fn entries(app: &App, main: &MainState, target: MenuTarget) -> Vec<Entry> {
    match target {
        MenuTarget::Server => server_items(main),
        MenuTarget::Message(id) => message_items(main, id),
        MenuTarget::Member(user_id) => member_items(main, user_id),
        MenuTarget::Channel(id) => channel_items(app, main, id),
        MenuTarget::Category(id) => category_items(app, main, id),
    }
}

/// The server header's menu: its settings, and the invites page by the shortest
/// way. Each entry is left out rather than greyed out, because a member who may
/// open none of the pages has no menu at all.
fn server_items(main: &MainState) -> Vec<Entry> {
    let perms = main.server.resolve(main.server.me, None);
    let mut entries = Vec::new();
    if let Some(tab) = ServerTab::ALL.into_iter().find(|tab| tab.allowed(perms)) {
        entries.push(Entry::item(Item::new(
            "Server settings",
            Icon::Gear,
            perform(Message::Settings(SettingsMsg::ServerTab(tab))),
        )));
    }
    if ServerTab::Invites.allowed(perms) {
        entries.push(Entry::item(Item::new(
            "Invite people",
            Icon::Link,
            perform(Message::Settings(SettingsMsg::ServerTab(
                ServerTab::Invites,
            ))),
        )));
    }
    entries
}

fn message_items(main: &MainState, id: i64) -> Vec<Entry> {
    let Some(message) = main.chat.message(id) else {
        return Vec::new();
    };
    let server = &main.server;
    let channel = Some(message.channel_id);
    let mine = message.author_id == main.member_id;
    let alive = !message.deleted;

    vec![
        Entry::item(
            Item::new(
                "Reply",
                Icon::Reply,
                perform(Message::Chat(ChatMsg::ReplyTo(id))),
            )
            .needs(alive, DELETED)
            .needs(
                server.can(permissions::SEND_MESSAGES, channel),
                MISSING_SEND,
            ),
        ),
        Entry::item(
            Item::new(
                "Add reaction",
                Icon::Smile,
                perform(Message::Chat(ChatMsg::OpenReactions(Some(id)))),
            )
            .needs(alive, DELETED)
            .needs(
                server.can(permissions::ADD_REACTIONS, channel),
                MISSING_REACTIONS,
            ),
        ),
        Entry::item(
            Item::new(
                "Copy text",
                Icon::Paperclip,
                perform(Message::Chat(ChatMsg::CopyText(id))),
            )
            .needs(alive, DELETED),
        ),
        Entry::item(
            Item::new(
                "Edit",
                Icon::Edit,
                perform(Message::Chat(ChatMsg::StartEdit(id))),
            )
            .needs(alive, DELETED)
            .needs(mine, "Only your own messages"),
        ),
        Entry::item(
            Item::new(
                "Delete",
                Icon::Trash,
                opens(Dialog::ConfirmDeleteMessage { message_id: id }),
            )
            .danger()
            .needs(alive, DELETED)
            .needs(
                mine || server.can(permissions::MANAGE_MESSAGES, channel),
                MISSING_MESSAGES,
            ),
        ),
        Entry::Separator,
        Entry::item(Item::new(
            "Copy message id",
            Icon::Link,
            copies("Message id", id.to_string()),
        )),
    ]
}

fn member_items(main: &MainState, user_id: i64) -> Vec<Entry> {
    let server = &main.server;
    let me = user_id == main.member_id;
    let owner = user_id != 0 && user_id == server.server.owner_id;
    let reachable = server.can_target(user_id);
    let profile = server.members.get(&user_id);
    let username = profile
        .map(|profile| profile.username.clone())
        .unwrap_or_default();
    let nickname = profile
        .map(|profile| profile.nickname.clone())
        .unwrap_or_default();

    let (nickname_allowed, nickname_reason) = if me {
        (
            server.can(permissions::CHANGE_NICKNAME, None),
            MISSING_NICKNAME,
        )
    } else {
        (
            server.can(permissions::MANAGE_MEMBERS, None) && reachable,
            MISSING_MEMBERS,
        )
    };

    let mut entries = vec![
        Entry::item(Item::new(
            "Profile",
            Icon::User,
            perform(Message::Ui(UiMsg::OpenProfileCard(user_id))),
        )),
        Entry::item(
            Item::new(
                "Message",
                Icon::Reply,
                perform(Message::Channels(ChannelsMsg::OpenDm(user_id))),
            )
            .needs(!me, IS_SELF),
        ),
        Entry::item(
            Item::new(
                format!("Mention {}", server.display_name(user_id)),
                Icon::Hash,
                perform(Message::Chat(ChatMsg::MentionPick(username))),
            )
            .needs(is_member(server, user_id), "They are no longer a member"),
        ),
        Entry::item(
            Item::new(
                "Change nickname",
                Icon::Edit,
                opens(Dialog::Nickname {
                    user_id,
                    draft: nickname,
                }),
            )
            .needs(nickname_allowed, nickname_reason),
        ),
        Entry::item(
            Item::new(
                "Roles…",
                Icon::Shield,
                perform(Message::Settings(SettingsMsg::ServerTab(
                    ServerTab::Members,
                ))),
            )
            .needs(server.can(permissions::MANAGE_ROLES, None), MISSING_ROLES),
        ),
    ];

    // The voice section only exists while they are in a voice channel this
    // account may see.
    if let Some(channel_id) = main
        .voice
        .channel_of(user_id)
        .filter(|id| server.can(permissions::VIEW_CHANNEL, Some(*id)))
    {
        entries.push(Entry::Separator);
        entries.extend(voice_items(main, channel_id, user_id, reachable));
    }

    entries.push(Entry::Separator);
    entries.push(Entry::item(
        Item::new("Kick", Icon::Boot, opens(Dialog::ConfirmKick { user_id }))
            .danger()
            .needs(!me, IS_SELF)
            .needs(!owner, IS_OWNER)
            .needs(reachable, OUTRANKED)
            .needs(server.can(permissions::KICK_MEMBERS, None), MISSING_KICK),
    ));
    entries.push(Entry::item(
        Item::new(
            "Ban",
            Icon::Ban,
            opens(Dialog::BanReason {
                user_id,
                reason: String::new(),
            }),
        )
        .danger()
        .needs(!me, IS_SELF)
        .needs(!owner, IS_OWNER)
        .needs(reachable, OUTRANKED)
        .needs(server.can(permissions::BAN_MEMBERS, None), MISSING_BAN),
    ));
    entries
}

/// Moderating somebody's voice session: the two flags, and where they can be
/// moved.
fn voice_items(main: &MainState, channel_id: i64, user_id: i64, reachable: bool) -> Vec<Entry> {
    let server = &main.server;
    let in_channel = Some(channel_id);
    let (muted, deafened) = moderation_state(main, channel_id, user_id);

    let mut entries = Vec::new();
    // Watching somebody is receiving their media, which only happens from inside
    // the voice channel they are sharing in.
    let sharing = main
        .voice
        .rosters
        .get(&channel_id)
        .is_some_and(|roster| roster.sharing.contains_key(&user_id));
    if sharing && user_id != main.member_id {
        let watching = main.voice.watch.intent == Some(user_id);
        let joined = main.voice.is_live() && main.voice.channel_id == channel_id;
        entries.push(Entry::item(
            Item::new(
                if watching {
                    "Stop watching"
                } else {
                    "Watch screen"
                },
                if watching {
                    Icon::ScreenOff
                } else {
                    Icon::Screen
                },
                perform(Message::Share(if watching {
                    ShareMsg::StopWatching
                } else {
                    ShareMsg::Watch(user_id)
                })),
            )
            .needs(joined, NOT_IN_CHANNEL),
        ));
    }

    entries.extend([
        Entry::item(
            Item::new(
                if muted {
                    "Server unmute"
                } else {
                    "Server mute"
                },
                if muted { Icon::Mic } else { Icon::MicOff },
                perform(Message::Voice(VoiceMsg::Moderate {
                    user_id,
                    muted: Some(!muted),
                    deafened: None,
                    move_to: None,
                })),
            )
            .needs(reachable, OUTRANKED)
            .needs(
                server.can(permissions::MUTE_MEMBERS, in_channel),
                MISSING_MUTE,
            ),
        ),
        Entry::item(
            Item::new(
                if deafened {
                    "Server undeafen"
                } else {
                    "Server deafen"
                },
                if deafened {
                    Icon::Headphones
                } else {
                    Icon::HeadphonesOff
                },
                perform(Message::Voice(VoiceMsg::Moderate {
                    user_id,
                    muted: None,
                    deafened: Some(!deafened),
                    move_to: None,
                })),
            )
            .needs(reachable, OUTRANKED)
            .needs(
                server.can(permissions::DEAFEN_MEMBERS, in_channel),
                MISSING_DEAFEN,
            ),
        ),
    ]);

    let allowed = reachable && server.can(permissions::MOVE_MEMBERS, in_channel);
    if !allowed {
        entries.push(Entry::item(
            Item::new("Move to…", Icon::Move, Message::Noop)
                .needs(reachable, OUTRANKED)
                .needs(false, MISSING_MOVE),
        ));
        return entries;
    }

    let elsewhere: Vec<(i64, String)> = server
        .channels_of_kind(ChannelKind::Voice)
        .into_iter()
        .filter(|channel| {
            channel.id != channel_id && server.can(permissions::VIEW_CHANNEL, Some(channel.id))
        })
        .map(|channel| (channel.id, channel.name.clone()))
        .collect();
    if elsewhere.is_empty() {
        return entries;
    }

    entries.push(Entry::Label("Move to"));
    for (id, name) in elsewhere {
        entries.push(Entry::item(Item::new(
            name,
            Icon::Speaker,
            perform(Message::Voice(VoiceMsg::Moderate {
                user_id,
                muted: None,
                deafened: None,
                move_to: Some(id),
            })),
        )));
    }
    entries
}

fn channel_items(app: &App, main: &MainState, id: i64) -> Vec<Entry> {
    let server = &main.server;
    let Some(channel) = server.channel(id) else {
        return Vec::new();
    };
    let muted = app.config.muted_channels.contains(&id);
    let unread = main.chat.channel(id).is_some_and(|ui| ui.unread > 0);

    let mut entries = vec![
        Entry::item(
            Item::new(
                "Mark as read",
                Icon::Check,
                perform(Message::Chat(ChatMsg::MarkChannelRead(id))),
            )
            .needs(unread, "Nothing unread"),
        ),
        Entry::item(Item::new(
            if muted {
                "Unmute notifications"
            } else {
                "Mute notifications"
            },
            if muted { Icon::Bell } else { Icon::BellOff },
            perform(Message::Channels(if muted {
                ChannelsMsg::UnmuteChannel(id)
            } else {
                ChannelsMsg::MuteChannel(id)
            })),
        )),
        Entry::item(Item::new(
            "Copy channel id",
            Icon::Link,
            copies("Channel id", id.to_string()),
        )),
        Entry::Separator,
    ];

    // A conversation has no name, no topic and no permissions of its own; it is
    // closed rather than deleted, and a new message brings it back.
    if channel_kind(channel) == ChannelKind::Dm {
        entries.push(Entry::item(Item::new(
            "Close conversation",
            Icon::Close,
            perform(Message::Channels(ChannelsMsg::HideDm(id))),
        )));
        return entries;
    }

    let manage = server.can(permissions::MANAGE_CHANNELS, Some(id));
    entries.push(Entry::item(
        Item::new(
            "Edit channel",
            Icon::Edit,
            perform(Message::Channels(ChannelsMsg::RenameChannel(id))),
        )
        .needs(manage, MISSING_CHANNELS),
    ));
    entries.push(Entry::item(
        Item::new(
            "Permissions",
            Icon::Shield,
            perform(Message::Settings(SettingsMsg::ServerTab(
                ServerTab::Channels,
            ))),
        )
        // The page itself is gated on the server-wide bit, so an override held in
        // this one channel is not enough to open it.
        .needs(
            server.can(permissions::MANAGE_CHANNELS, None),
            MISSING_CHANNELS,
        ),
    ));
    entries.push(Entry::item(
        Item::new(
            "Delete channel",
            Icon::Trash,
            perform(Message::Channels(ChannelsMsg::DeleteChannel(id))),
        )
        .danger()
        .needs(!server.is_general(id), "general cannot be deleted")
        .needs(manage, MISSING_CHANNELS),
    ));
    entries
}

fn category_items(app: &App, main: &MainState, id: i64) -> Vec<Entry> {
    let Some(category) = main.server.categories.get(&id) else {
        return Vec::new();
    };
    let manage = main.server.can(permissions::MANAGE_CHANNELS, None);
    let collapsed = app.ui.collapsed.contains(&id);

    vec![
        Entry::item(Item::new(
            if collapsed { "Expand" } else { "Collapse" },
            if collapsed {
                Icon::ChevronRight
            } else {
                Icon::ChevronDown
            },
            perform(Message::Channels(ChannelsMsg::ToggleCategory(id))),
        )),
        Entry::item(
            Item::new(
                "Create channel",
                Icon::Plus,
                perform(Message::Channels(ChannelsMsg::CreateChannelIn(Some(id)))),
            )
            .needs(manage, MISSING_CHANNELS),
        ),
        // A category is not created inside another one: this is the only place the
        // sidebar offers it at all.
        Entry::item(
            Item::new(
                "Create category",
                Icon::Plus,
                perform(Message::Channels(ChannelsMsg::CreateCategory)),
            )
            .needs(manage, MISSING_CHANNELS),
        ),
        Entry::item(
            Item::new(
                "Rename",
                Icon::Edit,
                opens(Dialog::EditCategory {
                    id,
                    name: category.name.clone(),
                }),
            )
            .needs(manage, MISSING_CHANNELS),
        ),
        Entry::Separator,
        Entry::item(
            Item::new(
                "Delete category",
                Icon::Trash,
                opens(Dialog::ConfirmDeleteCategory { id }),
            )
            .danger()
            .needs(manage, MISSING_CHANNELS),
        ),
    ]
}

/// Whether they are still a member: somebody who has left has nothing to mention.
fn is_member(server: &ServerModel, user_id: i64) -> bool {
    server.members.contains_key(&user_id)
}

/// How somebody's voice session stands: the roster is the live answer, the
/// profile what is known of them otherwise.
fn moderation_state(main: &MainState, channel_id: i64, user_id: i64) -> (bool, bool) {
    let in_voice: Option<&VoiceMember> = main
        .voice
        .rosters
        .get(&channel_id)
        .and_then(|roster| roster.members.get(&user_id));
    if let Some(member) = in_voice {
        return (member.server_muted, member.server_deafened);
    }
    main.server
        .members
        .get(&user_id)
        .map_or((false, false), |profile| {
            (profile.server_muted, profile.server_deafened)
        })
}
