//! The channel pane: the server header, the categories and their channels, the
//! voice card while in voice, and the user bar at the bottom.
//!
//! Every row the permission mirror says this account cannot see is left out. The
//! server filters the snapshot too; this is what keeps a stale delta from showing
//! a channel that is no longer readable.

use iced::alignment::Vertical;
use iced::widget::tooltip;
use iced::widget::{
    Column, Space, button, column, container, mouse_area, row, rule, scrollable, slider, text,
};
use iced::{Element, Length, Padding};
use vorcall_core::permissions;
use vorcall_core::{Category, Channel, ChannelKind, VoiceMember};

use crate::app::message::{ChannelsMsg, MenuTarget, Message, ShareMsg, UiMsg, VoiceMsg};
use crate::app::state::chat::Current;
use crate::app::state::server::channel_kind;
use crate::app::state::settings::ServerTab;
use crate::app::{App, MainState};
use crate::icons::{self, Icon};
use crate::theme::styles;
use crate::view::widgets::{self, Metrics};
use crate::view::{HEADER_HEIGHT, TEXT_BADGE, TEXT_ROW, TEXT_SECONDARY, TEXT_TITLE, bold};

/// How far an occupant under a voice channel is indented, and the glyph sizes the
/// rows draw at.
const OCCUPANT_INDENT: f32 = 36.0;
const CHEVRON: f32 = 12.0;
const ROW_ICON: f32 = 18.0;
/// The loudest a peer can be played, which is the range `PeerAudio` stores.
const PEER_VOLUME_MAX: f32 = 2.0;

pub fn pane<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let metrics = Metrics::of(&app.config);

    let mut list = column![]
        .spacing(2)
        .padding([12.0, 8.0])
        .width(Length::Fill);
    // Channels the server left without a category come first, under no heading of
    // their own.
    for channel in visible(main, None) {
        list = entries(app, main, channel, metrics, list);
    }
    for category in main.server.ordered_categories() {
        let channels = visible(main, Some(category.id));
        // A category whose every channel is hidden from this account is not a
        // heading with nothing under it — unless this account is the one who can
        // put a channel in it.
        if channels.is_empty() && !main.server.can(permissions::MANAGE_CHANNELS, None) {
            continue;
        }
        let collapsed = app.ui.collapsed.contains(&category.id);
        list = list.push(category_header(
            app, main, category, &channels, collapsed, metrics,
        ));
        if collapsed {
            continue;
        }
        for channel in channels {
            list = entries(app, main, channel, metrics, list);
        }
    }

    let body = scrollable(list)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(styles::scrollable(tokens));

    let mut pane = column![
        header(app, main, metrics),
        rule::horizontal(1.0).style(styles::rule(tokens)),
        body,
    ]
    .width(Length::Fill)
    .height(Length::Fill);
    if let Some(card) = voice_card(app, main, metrics) {
        pane = pane.push(card);
    }
    pane = pane.push(widgets::user_bar(app, main));

    container(pane)
        .width(app.config.sidebar_width)
        .height(Length::Fill)
        .style(styles::container::sidebar(tokens))
        .into()
}

/// The channels of one category this account may see, in sidebar order.
fn visible(main: &MainState, category: Option<i64>) -> Vec<&Channel> {
    main.server
        .channels_in(category)
        .into_iter()
        .filter(|channel| main.server.can(permissions::VIEW_CHANNEL, Some(channel.id)))
        .collect()
}

/// One channel's rows: itself, plus whoever is in it when it is a voice channel.
fn entries<'a>(
    app: &'a App,
    main: &'a MainState,
    channel: &'a Channel,
    metrics: Metrics,
    mut list: Column<'a, Message>,
) -> Column<'a, Message> {
    list = list.push(channel_row(app, main, channel, metrics));
    if channel_kind(channel) != ChannelKind::Voice {
        return list;
    }
    let Some(roster) = main.voice.rosters.get(&channel.id) else {
        return list;
    };
    for member in roster.members.values() {
        list = list.push(occupant(app, main, channel.id, member, metrics));
        // Somebody else's playback, tuned on this machine only.
        if main.voice.expanded_member == Some(member.user_id) && member.user_id != main.member_id {
            list = list.push(peer_audio(app, main, member.user_id, metrics));
        }
    }
    list
}

/// The server's name, and the way into its settings.
fn header<'a>(app: &'a App, main: &'a MainState, metrics: Metrics) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let name = if main.server.server.name.is_empty() {
        "Vorcall"
    } else {
        main.server.server.name.as_str()
    };

    let mut bar = row![widgets::clipped_name(
        text(name)
            .size(metrics.text(TEXT_TITLE))
            .font(bold())
            .color(tokens.text_primary),
        name,
        tokens,
    )]
    .spacing(8)
    .align_y(Vertical::Center);

    // Every server settings page needs a permission; without any of them the menu
    // would be empty, so there is nothing to open it with.
    let perms = main.server.resolve(main.server.me, None);
    if ServerTab::ALL.iter().any(|tab| tab.allowed(perms)) {
        bar = bar.push(widgets::icon_button(
            Icon::ChevronDown,
            "Server menu",
            Some(Message::Ui(UiMsg::ContextMenu(MenuTarget::Server))),
            tokens,
        ));
    }

    container(bar)
        .width(Length::Fill)
        .padding(Padding::ZERO.left(16.0).right(12.0))
        .center_y(metrics.height(HEADER_HEIGHT))
        .style(styles::container::sidebar(tokens))
        .into()
}

/// One category, which is a button that folds it away. Collapsed, it carries
/// whatever its channels have waiting.
fn category_header<'a>(
    app: &'a App,
    main: &'a MainState,
    category: &'a Category,
    channels: &[&Channel],
    collapsed: bool,
    metrics: Metrics,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let chevron = if collapsed {
        Icon::ChevronRight
    } else {
        Icon::ChevronDown
    };

    let mut bar = row![
        icons::icon(chevron, CHEVRON, tokens.text_secondary),
        widgets::group_label(&category.name, None, metrics, tokens),
        Space::new().width(Length::Fill),
    ]
    .spacing(4)
    .align_y(Vertical::Center);

    // Folded away, the rows cannot say what is waiting in them, so the heading
    // does.
    if collapsed {
        let (unread, mentions) = roll_up(main, channels);
        if mentions > 0 {
            bar = bar.push(widgets::mention_badge(mentions, tokens));
        } else if unread > 0 {
            bar = bar.push(widgets::unread_dot(tokens));
        }
    }
    if main.server.can(permissions::MANAGE_CHANNELS, None) {
        bar = bar.push(widgets::icon_button_in(
            Icon::Plus,
            "Create a channel",
            Some(Message::Channels(ChannelsMsg::CreateChannelIn(Some(
                category.id,
            )))),
            tokens.text_secondary,
            widgets::ICON_MARK,
            tokens,
        ));
    }

    let open = opened_menu(app, MenuTarget::Category(category.id));
    let fold = button(bar)
        .width(Length::Fill)
        .padding([metrics.row_padding(), 4.0])
        .style(styles::button::row_state(tokens, false, open))
        .on_press(Message::Channels(ChannelsMsg::ToggleCategory(category.id)));

    mouse_area(fold)
        .on_right_press(Message::Ui(UiMsg::ContextMenu(MenuTarget::Category(
            category.id,
        ))))
        .into()
}

/// What a collapsed category has waiting in it.
fn roll_up(main: &MainState, channels: &[&Channel]) -> (u32, u32) {
    channels
        .iter()
        .filter_map(|channel| main.chat.channel(channel.id))
        .fold((0, 0), |(unread, mentions), channel| {
            (
                unread.saturating_add(channel.unread),
                mentions.saturating_add(channel.mentions),
            )
        })
}

/// One channel: `#name` for text, a speaker and its occupant count for voice.
fn channel_row<'a>(
    app: &'a App,
    main: &'a MainState,
    channel: &'a Channel,
    metrics: Metrics,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let voice = channel_kind(channel) == ChannelKind::Voice;
    let ui = main.chat.channel(channel.id);
    let unread = ui.map_or(0, |channel| channel.unread);
    let mentions = ui.map_or(0, |channel| channel.mentions);
    let muted = app.config.muted_channels.contains(&channel.id);
    let selected = if voice {
        main.voice.intent && main.voice.channel_id == channel.id
    } else {
        main.chat.current.is(channel.id)
    };

    let color = match (muted, selected || unread > 0) {
        (true, _) => tokens.text_muted,
        (false, true) => tokens.text_primary,
        (false, false) => tokens.text_secondary,
    };
    let name = text(channel.name.as_str())
        .size(metrics.text(TEXT_ROW))
        .color(color);
    // Bold is what an unread text channel reads as, the way the design has it.
    let name = if unread > 0 && !muted && !voice {
        name.font(bold())
    } else {
        name
    };

    let mut line = row![
        icons::icon(
            if voice { Icon::Speaker } else { Icon::Hash },
            ROW_ICON,
            color
        ),
        widgets::clipped_name(name, channel.name.as_str(), tokens),
    ]
    .spacing(8)
    .align_y(Vertical::Center);

    if muted {
        line = line.push(widgets::tooltip_of(
            icons::icon(Icon::BellOff, widgets::ICON_MARK, tokens.text_muted),
            "Muted",
            tooltip::Position::Bottom,
            tokens,
        ));
    }
    if voice {
        let occupants = main
            .voice
            .rosters
            .get(&channel.id)
            .map_or(0, |roster| roster.members.len());
        if occupants > 0 {
            line = line.push(
                text(occupants.to_string())
                    .size(metrics.text(TEXT_BADGE))
                    .color(tokens.text_secondary),
            );
        }
    } else if mentions > 0 {
        line = line.push(widgets::mention_badge(mentions, tokens));
    } else if unread > 0 && !muted {
        line = line.push(widgets::unread_dot(tokens));
    }

    let press = if voice {
        // A voice channel is joined, never read.
        Message::Voice(VoiceMsg::Join(channel.id))
    } else {
        Message::Channels(ChannelsMsg::Select(channel.id))
    };
    let open = opened_menu(app, MenuTarget::Channel(channel.id));

    let entry = button(widgets::row_body(line))
        .width(Length::Fill)
        .height(metrics.height(widgets::ROW_HEIGHT))
        .padding([0.0, 8.0])
        .clip(true)
        .style(styles::button::row_state(tokens, selected, open))
        .on_press(press);

    mouse_area(entry)
        .on_right_press(Message::Ui(UiMsg::ContextMenu(MenuTarget::Channel(
            channel.id,
        ))))
        .into()
}

/// One person in a voice channel, indented under it.
fn occupant<'a>(
    app: &'a App,
    main: &'a MainState,
    channel_id: i64,
    member: &'a VoiceMember,
    metrics: Metrics,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let user_id = member.user_id;
    let me = user_id == main.member_id;
    let roster = main.voice.rosters.get(&channel_id);
    let speaking = roster.is_some_and(|roster| roster.speaking.contains(&user_id));
    let sharing = roster.is_some_and(|roster| roster.sharing.contains_key(&user_id));
    // One's own switches are known here before the server has echoed them, so the
    // icon flips on the press rather than on the round trip.
    let self_muted = if me {
        main.voice.muted
    } else {
        member.self_muted
    };
    let self_deafened = if me {
        main.voice.deafened
    } else {
        member.self_deafened
    };

    let name = main.server.display_name(user_id);
    let mut line = row![
        widgets::speaking_avatar(main, user_id, widgets::AVATAR_OCCUPANT, speaking, tokens),
        widgets::clipped_name(
            text(name).size(metrics.text(TEXT_ROW)).color(if speaking {
                tokens.text_primary
            } else {
                tokens.text_secondary
            }),
            name,
            tokens,
        ),
    ]
    .spacing(8)
    .align_y(Vertical::Center);

    if member.priority {
        line = line.push(widgets::tooltip_of(
            icons::icon(Icon::Star, widgets::ICON_MARK, tokens.warning),
            "Priority speaker",
            tooltip::Position::Bottom,
            tokens,
        ));
    }
    if let Some(mark) = widgets::voice_flag(
        Icon::MicOff,
        member.server_muted,
        self_muted,
        "Muted by a moderator",
        "Muted",
        tooltip::Position::Bottom,
        tokens,
    ) {
        line = line.push(mark);
    }
    if let Some(mark) = widgets::voice_flag(
        Icon::HeadphonesOff,
        member.server_deafened,
        self_deafened,
        "Deafened by a moderator",
        "Deafened",
        tooltip::Position::Bottom,
        tokens,
    ) {
        line = line.push(mark);
    }
    if sharing {
        line = line.push(widgets::watch_badge(main, channel_id, user_id, tokens));
    }
    // How loud somebody else is played here is this machine's own business, so the
    // row keeps it one press away rather than in a menu.
    if !me {
        let expanded = main.voice.expanded_member == Some(user_id);
        line = line.push(widgets::icon_button_in(
            Icon::Speaker,
            "Volume",
            Some(Message::Ui(UiMsg::ExpandMember(
                (!expanded).then_some(user_id),
            ))),
            if expanded {
                tokens.text_primary
            } else {
                tokens.text_muted
            },
            widgets::ICON_MARK,
            tokens,
        ));
    }

    let open = opened_menu(app, MenuTarget::Member(user_id));
    let entry = button(line)
        .width(Length::Fill)
        .padding(
            Padding::ZERO
                .top(metrics.row_padding())
                .bottom(metrics.row_padding())
                .right(8.0),
        )
        .clip(true)
        .style(styles::button::row_state(tokens, false, open))
        .on_press(Message::Ui(UiMsg::OpenProfileCard(user_id)));

    mouse_area(
        row![Space::new().width(OCCUPANT_INDENT), entry]
            .width(Length::Fill)
            .align_y(Vertical::Center),
    )
    .on_right_press(Message::Ui(UiMsg::ContextMenu(MenuTarget::Member(user_id))))
    .into()
}

/// How loud one peer is played here, and whether they are heard at all. Both are
/// this machine's own: nobody else is told.
fn peer_audio<'a>(
    app: &'a App,
    main: &'a MainState,
    user_id: i64,
    metrics: Metrics,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let tuning = main
        .voice
        .peer_audio
        .get(&user_id)
        .copied()
        .unwrap_or_default();
    let (glyph, tip, color) = if tuning.muted {
        (Icon::SpeakerOff, "Unmute for me", tokens.danger)
    } else {
        (Icon::Speaker, "Mute for me", tokens.text_secondary)
    };

    let line = row![
        widgets::icon_button_in(
            glyph,
            tip,
            Some(Message::Voice(VoiceMsg::TogglePeerMute(user_id))),
            color,
            widgets::ICON_MARK,
            tokens,
        ),
        slider(0.0..=PEER_VOLUME_MAX, tuning.volume, move |volume| {
            Message::Voice(VoiceMsg::SetPeerVolume(user_id, volume))
        })
        .on_release(Message::Voice(VoiceMsg::PeerVolumeReleased(user_id)))
        .step(0.05_f32)
        .width(Length::Fill)
        .style(styles::slider(tokens)),
        text(format!("{:.0}%", tuning.volume * 100.0))
            .size(metrics.text(TEXT_BADGE))
            .color(tokens.text_muted),
    ]
    .spacing(8)
    .align_y(Vertical::Center);

    row![Space::new().width(OCCUPANT_INDENT), line]
        .width(Length::Fill)
        .into()
}

/// The voice card over the user bar, while this client means to be in voice.
fn voice_card<'a>(
    app: &'a App,
    main: &'a MainState,
    metrics: Metrics,
) -> Option<Element<'a, Message>> {
    if !main.voice.intent {
        return None;
    }
    let tokens = &app.tokens;
    let voice = &main.voice;

    let (state, state_color) = if voice.is_live() {
        ("Voice connected", tokens.success)
    } else if voice.joining {
        ("Connecting…", tokens.warning)
    } else {
        ("Not connected", tokens.text_muted)
    };
    let ping = match voice.stats.rtt_last_ms {
        Some(rtt) => format!(" · {rtt:.0} ms"),
        None => String::new(),
    };

    let mut switches = row![share_button(app, main)].spacing(6);
    if voice.watch.state.is_some() {
        let popped = voice.watch.popped.is_some();
        switches = switches.push(widgets::icon_control(
            Icon::Pin,
            if popped { "Pop in" } else { "Pop out" },
            Some(Message::Share(if popped {
                ShareMsg::PopIn
            } else {
                ShareMsg::PopOut
            })),
            false,
            tokens,
        ));
    }
    switches = switches.push(widgets::icon_control(
        Icon::Close,
        "Disconnect",
        Some(Message::Voice(VoiceMsg::Leave)),
        true,
        tokens,
    ));

    let title = format!("{}{ping}", main.server.channel_title(voice.channel_id));
    let mut lines = column![
        text(state)
            .size(metrics.text(TEXT_SECONDARY))
            .font(bold())
            .color(state_color),
        widgets::clipped_name(
            text(title.clone())
                .size(metrics.text(TEXT_SECONDARY))
                .color(tokens.text_secondary),
            &title,
            tokens,
        ),
    ];
    if let Some(stats) = voice.share.stats.as_ref().filter(|_| voice.share.active) {
        let mut share_line = row![
            text(format!("Sharing {} kbit/s", stats.kbps))
                .size(metrics.text(TEXT_SECONDARY))
                .color(tokens.text_secondary)
        ]
        .spacing(4)
        .align_y(Vertical::Center);
        // Datagrams the socket refuses are what a frozen watcher looks like from
        // this side, so the sharer is the one told about them.
        if stats.send_failures > 0 {
            share_line = share_line.push(
                text(format!("· {} datagrams lost", stats.send_failures))
                    .size(metrics.text(TEXT_SECONDARY))
                    .color(tokens.warning),
            );
        }
        lines = lines.push(share_line);
    }

    let card = container(column![lines, switches].spacing(8))
        .width(Length::Fill)
        .padding([10.0, 12.0])
        .style(styles::container::card(tokens));

    Some(
        container(card)
            .width(Length::Fill)
            .padding(Padding::ZERO.left(8.0).right(8.0).bottom(8.0))
            .into(),
    )
}

/// Sharing from the voice card: stop what is on the wire, or start a capture when
/// this account may.
fn share_button<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let share = &main.voice.share;

    if share.active {
        let watchers = share.watchers;
        let tip = match watchers {
            1 => "Stop sharing · 1 watching".to_owned(),
            _ => format!("Stop sharing · {watchers} watching"),
        };
        return widgets::icon_control(
            Icon::ScreenOff,
            &tip,
            Some(Message::Share(ShareMsg::Stop)),
            false,
            tokens,
        );
    }
    if share.starting {
        return widgets::icon_control(Icon::Screen, "Starting the share…", None, false, tokens);
    }

    let allowed = main.voice.is_live()
        && main
            .server
            .can(permissions::SHARE_SCREEN, Some(main.voice.channel_id));
    widgets::icon_control(
        Icon::Screen,
        if allowed {
            "Share your screen"
        } else {
            "Requires Share Screen"
        },
        allowed.then_some(Message::Share(ShareMsg::OpenPicker)),
        false,
        tokens,
    )
}

/// Whether the context menu that is open belongs to this row.
fn opened_menu(app: &App, target: MenuTarget) -> bool {
    app.ui
        .context_menu
        .is_some_and(|menu| menu.target == target)
}

/// What the "nothing is selected" line says, used by the chat column too.
pub fn nothing_selected(app: &App) -> Element<'_, Message> {
    widgets::empty_state(
        Icon::Hash,
        "Pick a channel",
        "Channels you can see are listed on the left.",
        &app.tokens,
    )
}

/// Whether the current selection is a DM, which the chat header draws
/// differently.
pub fn is_dm(main: &MainState) -> bool {
    matches!(main.chat.current, Current::Dm(_))
}
