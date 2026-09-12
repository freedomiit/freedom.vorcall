//! The small pieces every pane is built from.
//!
//! Each one takes the theme it draws with, so a widget is never tied to a
//! particular surface: the same avatar appears in the member pane, the message
//! list and the profile card.

use std::time::Duration;

use iced::alignment::{Horizontal, Vertical};
use iced::widget::image::Handle;
use iced::widget::{Space, button, column, container, image, row, stack, text, tooltip};
use iced::{Background, Color, Element, Length, Theme, border};
use vorcall_core::config::Density;
use vorcall_core::{Config, Profile, Role};

use crate::app::message::{Message, SettingsMsg, ShareMsg, VoiceMsg};
use crate::app::state::chat::{ChatState, ImageState};
use crate::app::state::settings::SettingsTab;
use crate::app::{App, MainState};
use crate::brand::loading;
use crate::icons::{self, Icon};
use crate::theme::ThemeTokens;
use crate::theme::styles;
use crate::view::{
    AVATAR, AVATAR_SMALL, TEXT_BADGE, TEXT_BODY, TEXT_ROW, TEXT_SECONDARY, TEXT_SECTION,
    USER_BAR_HEIGHT, bold,
};
use crate::workers::images::ImageKey;

/// How wide a presence dot is, and the ring that cuts it out of the surface
/// under it.
const DOT: f32 = 10.0;
const RING: f32 = 2.0;
/// How wide the dot is that says a list has something unread in it, and the
/// louder one the rail draws over the DMs entry.
const UNREAD_DOT: f32 = 8.0;
const ALERT_DOT: f32 = 9.0;
/// The icon inside an icon button, and the button's own padding.
pub const ICON_SIZE: f32 = 18.0;
const ICON_PADDING: f32 = 6.0;
/// A marker beside a name: a role's glyph, a mute, a crown.
pub const ICON_MARK: f32 = 14.0;
/// The avatar a list row draws, and the smaller one under a voice channel.
pub const AVATAR_ROW: f32 = 32.0;
pub const AVATAR_OCCUPANT: f32 = 22.0;
/// The design's row heights: a channel row, a member or DM row, a control.
pub const ROW_HEIGHT: f32 = 32.0;
pub const MEMBER_ROW_HEIGHT: f32 = 40.0;
pub const CONTROL_HEIGHT: f32 = 32.0;
/// The creature beside a loading line.
const LOADING_CREATURE: f32 = 48.0;

/// The sizes one window draws at: the type scale, and what density does to the
/// paddings. The font scale is not in here — it magnifies the whole window
/// through [`crate::view::scale_factor`], so applying it again would scale the
/// panes twice.
#[derive(Debug, Clone, Copy)]
pub struct Metrics {
    pub compact: bool,
}

impl Metrics {
    pub fn of(config: &Config) -> Self {
        Self {
            compact: config.density == Density::Compact,
        }
    }

    /// One size off the type scale, unchanged: the window's scale factor carries
    /// the font scale now. The seam stays as the one place a density-dependent
    /// type scale would go, rather than in 56 call sites.
    pub fn text(self, size: f32) -> f32 {
        size
    }

    /// A control height, unchanged, and the seam for a density-dependent one —
    /// see [`Metrics::text`].
    pub fn height(self, height: f32) -> f32 {
        height
    }

    /// The vertical padding of a list row.
    pub fn row_padding(self) -> f32 {
        if self.compact { 2.0 } else { 4.0 }
    }

    /// The gap between two message groups.
    pub fn group_spacing(self) -> f32 {
        if self.compact { 8.0 } else { 16.0 }
    }

    /// The avatar a list row draws.
    pub fn row_avatar(self) -> f32 {
        if self.compact {
            AVATAR_SMALL
        } else {
            AVATAR_ROW
        }
    }

    /// The avatar a message's first line draws; `None` where the compact density
    /// drops the column altogether.
    pub fn message_avatar(self) -> Option<f32> {
        (!self.compact).then_some(AVATAR)
    }
}

/// A member's picture, or their initials on the accent.
pub fn avatar<'a>(
    profile: Option<&Profile>,
    size: f32,
    tokens: &'a ThemeTokens,
    picture: Option<Handle>,
) -> Element<'a, Message> {
    let radius = size / 2.0;
    let accent = accent_of(profile, tokens);
    let ground = move |_theme: &Theme| container::Style {
        background: Some(Background::Color(accent)),
        text_color: Some(tokens.text_on_accent),
        border: border::rounded(radius),
        ..container::Style::default()
    };

    let inner: Element<'_, Message> = match picture {
        Some(handle) => image(handle).width(size).height(size).into(),
        None => text(initials(profile))
            .size((size * 0.4).max(TEXT_BADGE))
            .into(),
    };

    container(inner).center(size).style(ground).into()
}

/// The accent a member's avatar is drawn on: their own, else the theme's.
fn accent_of(profile: Option<&Profile>, tokens: &ThemeTokens) -> Color {
    match profile.map(|profile| profile.accent_color) {
        Some(rgb) if rgb != 0 => color_of(rgb),
        _ => tokens.accent,
    }
}

/// A `0xRRGGBB` the server sends, as a colour.
pub fn color_of(rgb: u32) -> Color {
    let [_, r, g, b] = rgb.to_be_bytes();
    Color::from_rgb8(r, g, b)
}

/// At most two letters, from the name a member is shown under.
fn initials(profile: Option<&Profile>) -> String {
    let Some(profile) = profile else {
        return "?".to_owned();
    };
    let name = if profile.nickname.is_empty() {
        profile.username.as_str()
    } else {
        profile.nickname.as_str()
    };
    let letters: String = name
        .split_whitespace()
        .filter_map(|word| word.chars().next())
        .take(2)
        .collect();
    if letters.is_empty() {
        "?".to_owned()
    } else {
        letters.to_uppercase()
    }
}

/// One icon as a button, with the tooltip that says what it does. `on_press` of
/// `None` draws it disabled, which is how a missing permission reads.
pub fn icon_button<'a>(
    glyph: Icon,
    tip: &str,
    on_press: Option<Message>,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    icon_button_in(
        glyph,
        tip,
        on_press,
        tokens.text_secondary,
        ICON_SIZE,
        tokens,
    )
}

/// [`icon_button`] in a colour and a size of its own: a switch that is on reads
/// in a status colour, and a hover strip draws smaller than a bar does.
pub fn icon_button_in<'a>(
    glyph: Icon,
    tip: &str,
    on_press: Option<Message>,
    color: Color,
    size: f32,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    // The glyph is an SVG in a colour of its own, so the button's disabled text
    // colour never reaches it: fading it here is what makes one read as disabled.
    let color = if on_press.is_some() {
        color
    } else {
        styles::faded(color)
    };
    let control = button(icons::icon(glyph, size, color))
        .padding(ICON_PADDING)
        .style(styles::button::icon(tokens))
        .on_press_maybe(on_press);
    tooltip_of(control, tip, tooltip::Position::Bottom, tokens)
}

/// One icon as a filled square control: the voice card's own switches.
pub fn icon_control<'a>(
    glyph: Icon,
    tip: &str,
    on_press: Option<Message>,
    danger: bool,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    let color = if danger {
        tokens.text_on_accent
    } else {
        tokens.text_primary
    };
    let color = if on_press.is_some() {
        color
    } else {
        styles::faded(color)
    };
    let control = button(container(icons::icon(glyph, ICON_SIZE, color)).center(Length::Fill))
        .width(CONTROL_HEIGHT)
        .height(CONTROL_HEIGHT)
        .padding(0)
        .on_press_maybe(on_press);
    let control = if danger {
        control.style(styles::button::danger(tokens))
    } else {
        control.style(styles::button::icon_filled(tokens))
    };
    tooltip_of(control, tip, tooltip::Position::Top, tokens)
}

/// Anything, with a tooltip over it. An empty tip draws the content alone.
pub fn tooltip_of<'a>(
    content: impl Into<Element<'a, Message>>,
    tip: &str,
    position: tooltip::Position,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    let content = content.into();
    if tip.is_empty() {
        return content;
    }
    tooltip(
        content,
        container(
            text(tip.to_owned())
                .size(TEXT_SECONDARY)
                .color(tokens.text_primary),
        )
        .padding([4.0, 8.0])
        .style(styles::container::popover(tokens)),
        position,
    )
    .into()
}

/// An unread count. Nothing is drawn for zero.
pub fn badge<'a>(count: u32, tokens: &'a ThemeTokens) -> Element<'a, Message> {
    if count == 0 {
        return Space::new().into();
    }
    container(text(label_count(count)).size(TEXT_BADGE))
        .padding([1.0, 6.0])
        .style(styles::container::badge(tokens))
        .into()
}

/// A mention count, which is louder than an unread one.
pub fn mention_badge<'a>(count: u32, tokens: &'a ThemeTokens) -> Element<'a, Message> {
    if count == 0 {
        return Space::new().into();
    }
    container(text(label_count(count)).size(TEXT_BADGE))
        .padding([1.0, 6.0])
        .style(styles::container::mention_badge(tokens))
        .into()
}

/// A count as a badge spells it; anything past 99 is "99+".
fn label_count(count: u32) -> String {
    if count > 99 {
        "99+".to_owned()
    } else {
        count.to_string()
    }
}

/// Whether somebody has a live connection.
pub fn presence_dot<'a>(online: bool, tokens: &'a ThemeTokens) -> Element<'a, Message> {
    dot(DOT, presence_color(online, tokens), None)
}

/// The same dot where it overlaps an avatar, cut out of the surface behind it.
pub fn presence_dot_on<'a>(
    online: bool,
    ground: Color,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    dot(DOT, presence_color(online, tokens), Some(ground))
}

fn presence_color(online: bool, tokens: &ThemeTokens) -> Color {
    if online {
        tokens.online
    } else {
        tokens.text_muted
    }
}

/// What a collapsed category shows instead of a count: something in it is
/// unread.
pub fn unread_dot<'a>(tokens: &'a ThemeTokens) -> Element<'a, Message> {
    dot(UNREAD_DOT, tokens.text_primary, None)
}

/// The louder dot the rail draws over an entry with something waiting in it.
pub fn alert_dot<'a>(ground: Color, tokens: &'a ThemeTokens) -> Element<'a, Message> {
    dot(ALERT_DOT, tokens.mention, Some(ground))
}

/// One filled circle, `size` across, optionally ringed by the surface it sits
/// on: the ring is what keeps a dot legible over an avatar.
fn dot<'a>(size: f32, color: Color, ground: Option<Color>) -> Element<'a, Message> {
    let ring = if ground.is_some() { RING } else { 0.0 };
    let outer = size + ring * 2.0;
    container(Space::new())
        .width(outer)
        .height(outer)
        .style(move |_theme: &Theme| container::Style {
            background: Some(Background::Color(color)),
            border: border::rounded(outer / 2.0)
                .width(ring)
                .color(ground.unwrap_or(Color::TRANSPARENT)),
            ..container::Style::default()
        })
        .into()
}

/// The pixels of one cached image, once they are decoded. A view never sees
/// anything else: the bytes belong to the worker that fetched them.
pub fn image_handle(chat: &ChatState, key: ImageKey) -> Option<Handle> {
    match chat.images.get(&key) {
        Some(ImageState::Ready(handle)) => Some(handle.clone()),
        Some(ImageState::Loading | ImageState::Failed) | None => None,
    }
}

/// One member's avatar, with their picture once it has been decoded.
pub fn member_avatar<'a>(
    main: &'a MainState,
    user_id: i64,
    size: f32,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    let profile = main.server.members.get(&user_id);
    let picture = profile
        .map(|profile| profile.avatar_image_id)
        .filter(|id| *id != 0)
        .and_then(|id| image_handle(&main.chat, ImageKey::Image(id)));
    avatar(profile, size, tokens, picture)
}

/// The same, with the presence dot on its corner. `ground` is the surface the row
/// sits on, which the dot is cut out of.
pub fn member_avatar_on<'a>(
    main: &'a MainState,
    user_id: i64,
    size: f32,
    ground: Color,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    stack![
        member_avatar(main, user_id, size, tokens),
        container(presence_dot_on(
            main.server.is_online(user_id),
            ground,
            tokens
        ))
        .align_right(Length::Fill)
        .align_bottom(Length::Fill),
    ]
    .into()
}

/// An avatar with the ring that says its owner is talking.
pub fn speaking_avatar<'a>(
    main: &'a MainState,
    user_id: i64,
    size: f32,
    speaking: bool,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    let color = if speaking {
        tokens.success
    } else {
        Color::TRANSPARENT
    };
    let radius = size / 2.0 + RING;
    container(member_avatar(main, user_id, size, tokens))
        .padding(RING)
        .style(move |_theme: &Theme| container::Style {
            border: border::rounded(radius).width(RING).color(color),
            ..container::Style::default()
        })
        .into()
}

/// The marker on somebody who is sharing their screen, and the way into their
/// stream. Without a press it still draws, so the tooltip can say why.
pub fn live_badge<'a>(
    tip: &str,
    on_press: Option<Message>,
    watching: bool,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    let tokens_for_style = *tokens;
    let control = button(text("LIVE").size(TEXT_BADGE).font(bold()))
        .padding([0.0, 5.0])
        .style(move |_theme: &Theme, status: button::Status| {
            let accent = match (watching, status) {
                (true, _) | (false, button::Status::Pressed) => tokens_for_style.accent_pressed,
                (false, button::Status::Hovered) => tokens_for_style.accent_hover,
                (false, _) => tokens_for_style.accent,
            };
            button::Style {
                background: Some(Background::Color(
                    if matches!(status, button::Status::Disabled) {
                        styles::faded(accent)
                    } else {
                        accent
                    },
                )),
                text_color: tokens_for_style.text_on_accent,
                border: border::rounded(styles::RADIUS_POPOVER),
                ..button::Style::default()
            }
        })
        .on_press_maybe(on_press);
    tooltip_of(control, tip, tooltip::Position::Bottom, tokens)
}

/// The LIVE badge a sharer's row draws, pressable into their stream. Watching is
/// receiving the media, which only happens from inside the voice channel, so the
/// badge is inert anywhere else.
pub fn watch_badge<'a>(
    main: &'a MainState,
    channel_id: i64,
    user_id: i64,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    if user_id == main.member_id {
        return live_badge("You are sharing your screen", None, false, tokens);
    }
    let joined = main.voice.is_live() && main.voice.channel_id == channel_id;
    let watching = main.voice.watch.intent == Some(user_id);
    if !joined {
        return live_badge("Join the channel to watch", None, false, tokens);
    }
    if watching {
        return live_badge(
            "Stop watching",
            Some(Message::Share(ShareMsg::StopWatching)),
            true,
            tokens,
        );
    }
    live_badge(
        "Watch their screen",
        Some(Message::Share(ShareMsg::Watch(user_id))),
        false,
        tokens,
    )
}

/// What marks a member's row: their highest role's emoji, the icon uploaded for
/// it, or the role glyph in the role's own colour.
pub fn role_icon<'a>(
    main: &'a MainState,
    role: &Role,
    size: f32,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    if !role.icon_emoji.is_empty() {
        return text(role.icon_emoji.clone()).size(size).into();
    }
    if role.icon_image_id != 0
        && let Some(handle) = image_handle(&main.chat, ImageKey::Image(role.icon_image_id))
    {
        return image(handle).width(size).height(size).into();
    }
    icons::icon(Icon::Shield, size, role_color(role, tokens))
}

/// The crown every pane marks the owner with.
pub fn owner_crown<'a>(tokens: &'a ThemeTokens) -> Element<'a, Message> {
    tooltip_of(
        icons::icon(Icon::Crown, ICON_MARK, tokens.warning),
        "Owner",
        tooltip::Position::Bottom,
        tokens,
    )
}

/// The colour a role paints what it marks, the theme's own where it has none.
pub fn role_color(role: &Role, tokens: &ThemeTokens) -> Color {
    if role.color == 0 {
        tokens.text_secondary
    } else {
        color_of(role.color)
    }
}

/// The heading over a group of rows, with the count the member pane spells out.
pub fn group_label<'a>(
    label: &str,
    count: Option<usize>,
    metrics: Metrics,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    let label = match count {
        Some(count) => format!("{} — {count}", label.to_uppercase()),
        None => label.to_uppercase(),
    };
    text(label)
        .size(metrics.text(TEXT_BADGE))
        .font(bold())
        .color(tokens.text_secondary)
        .into()
}

/// The bar at the bottom of either sidebar: who is signed in, their microphone,
/// their headphones and the way into the settings.
pub fn user_bar<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let metrics = Metrics::of(&app.config);

    let (mic, mic_tip, mic_color) = if main.voice.muted {
        (Icon::MicOff, "Unmute", tokens.danger)
    } else {
        (Icon::Mic, "Mute", tokens.text_secondary)
    };
    let (ear, ear_tip, ear_color) = if main.voice.deafened {
        (Icon::HeadphonesOff, "Undeafen", tokens.danger)
    } else {
        (Icon::Headphones, "Deafen", tokens.text_secondary)
    };

    let bar = row![
        member_avatar_on(main, main.member_id, AVATAR_ROW, tokens.bg_rail, tokens),
        column![
            text(main.server.display_name(main.member_id).to_owned())
                .size(metrics.text(TEXT_ROW))
                .font(bold())
                .color(tokens.text_primary),
            text(format!("@{}", main.username))
                .size(metrics.text(TEXT_BADGE))
                .color(tokens.text_secondary),
        ]
        .width(Length::Fill),
        icon_button_in(
            mic,
            mic_tip,
            Some(Message::Voice(VoiceMsg::ToggleMute)),
            mic_color,
            ICON_SIZE,
            tokens,
        ),
        icon_button_in(
            ear,
            ear_tip,
            Some(Message::Voice(VoiceMsg::ToggleDeafen)),
            ear_color,
            ICON_SIZE,
            tokens,
        ),
        icon_button(
            Icon::Gear,
            "User settings",
            Some(Message::Settings(SettingsMsg::Open(SettingsTab::Account))),
            tokens,
        ),
    ]
    .spacing(8)
    .align_y(Vertical::Center);

    container(bar)
        .width(Length::Fill)
        .padding([0.0, 8.0])
        .center_y(metrics.height(USER_BAR_HEIGHT))
        .style(styles::container::rail(tokens))
        .into()
}

/// One role, as the profile panes list it: a swatch of its own colour, its name,
/// and whatever it is marked with.
pub fn role_chip<'a>(role: &Role, tokens: &'a ThemeTokens) -> Element<'a, Message> {
    let mut content = row![
        dot(UNREAD_DOT, role_color(role, tokens), None),
        text(role.name.clone())
            .size(TEXT_SECONDARY)
            .color(tokens.text_primary),
    ]
    .spacing(6)
    .align_y(Vertical::Center);
    if !role.icon_emoji.is_empty() {
        content = content.push(text(role.icon_emoji.clone()).size(TEXT_BADGE));
    }
    container(content)
        .padding([1.0, 8.0])
        .style(styles::container::chip(tokens))
        .into()
}

/// The small heading over a group of rows.
pub fn section_label<'a>(label: &str, tokens: &'a ThemeTokens) -> Element<'a, Message> {
    text(label.to_uppercase())
        .size(TEXT_BADGE)
        .color(tokens.text_muted)
        .into()
}

/// What a pane with nothing in it says.
pub fn empty_state<'a>(
    glyph: Icon,
    title: &str,
    hint: &str,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    container(
        column![
            icons::icon(glyph, 32.0, tokens.text_muted),
            text(title.to_owned())
                .size(TEXT_SECTION)
                .color(tokens.text_secondary),
            text(hint.to_owned())
                .size(TEXT_ROW)
                .color(tokens.text_muted),
        ]
        .spacing(8)
        .align_x(Horizontal::Center),
    )
    .center(Length::Fill)
    .into()
}

/// What a pane waiting on the server says, with the creature to watch.
pub fn loading_state<'a>(elapsed: Duration, tokens: &'a ThemeTokens) -> Element<'a, Message> {
    container(
        column![
            loading::view(elapsed, LOADING_CREATURE),
            text("Loading…").size(TEXT_BODY).color(tokens.text_muted),
        ]
        .spacing(8)
        .align_x(Horizontal::Center),
    )
    .center(Length::Fill)
    .into()
}

/// A keyboard hint, the way the quick switcher and the composer spell one.
pub fn key_hint<'a>(binding: &str, tokens: &'a ThemeTokens) -> Element<'a, Message> {
    container(
        text(binding.to_owned())
            .size(TEXT_BADGE)
            .color(tokens.text_secondary),
    )
    .padding([1.0, 5.0])
    .style(styles::container::chip(tokens))
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(username: &str, nickname: &str) -> Profile {
        Profile {
            user_id: 1,
            username: username.to_owned(),
            nickname: nickname.to_owned(),
            ..Profile::default()
        }
    }

    #[test]
    fn initials_come_off_the_name_a_member_is_shown_under() {
        assert_eq!(initials(None), "?");
        assert_eq!(initials(Some(&profile("ana", ""))), "A");
        assert_eq!(initials(Some(&profile("ana", "Ana Paula"))), "AP");
        assert_eq!(initials(Some(&profile("ana", "  "))), "?");
    }

    #[test]
    fn a_badge_counts_up_to_ninety_nine() {
        assert_eq!(label_count(1), "1");
        assert_eq!(label_count(99), "99");
        assert_eq!(label_count(100), "99+");
    }

    #[test]
    fn a_colour_the_server_sends_is_read_as_rgb() {
        assert_eq!(color_of(0x00_C8_10_2E), Color::from_rgb8(0xC8, 0x10, 0x2E));
        assert_eq!(color_of(0), Color::BLACK);
    }
}
