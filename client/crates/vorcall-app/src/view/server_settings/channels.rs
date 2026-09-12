//! The Channels page: the tree, the editor for whichever channel is in focus,
//! and that channel's permission overrides.
//!
//! Reordering is a drag onto a slot under the pointer, or the two arrows. Neither
//! moves anything locally: `PROTOCOL.md` § Channels answers a reorder with a
//! broadcast, and the tree redraws from that.

use std::fmt;

use iced::alignment::Vertical;
use iced::widget::{
    Column, Row, Space, button, container, pick_list, row, text, text_input, tooltip,
};
use iced::{Element, Length};
use vorcall_core::{Category, Channel, ChannelKind, permissions};

use crate::app::message::{
    AdminMsg, DragItem, DragSlot, Message, OverrideTargetKind, TriState, UiMsg,
};
use crate::app::state::server::channel_kind;
use crate::app::state::ui::Dialog;
use crate::app::update::admin;
use crate::app::{App, MainState};
use crate::icons::{self, Icon};
use crate::theme::ThemeTokens;
use crate::theme::styles;
use crate::view::server_settings::{
    Kind, action, arrows, card, drag_banner, drop_slot, field, grip, hint,
};
use crate::view::widgets;
use crate::view::{TEXT_BADGE, TEXT_BODY, TEXT_ROW, TEXT_SECONDARY};

/// Why a row's controls are greyed out, worded the way the server words its
/// refusal.
const MISSING_CHANNELS: &str = "Missing permission: Manage channels";
/// How far a channel row sits inside its category.
const INDENT: f32 = 18.0;
/// The glyph beside a row.
const GLYPH: f32 = 14.0;

pub fn view<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    // A draft can outlive the channel it was opened on: a delta deletes the
    // channel, and the form falls back to describing a new one.
    let editing = main
        .admin
        .channel
        .id
        .and_then(|id| main.server.channel(id).map(|channel| (id, channel)));

    let mut page = Column::new().spacing(16).width(Length::Fill);
    page = page.push(tree(app, main));
    match dragged_label(app, main) {
        Some(dragged) => page = page.push(drag_banner(&dragged, tokens)),
        None => {
            page = page.push(hint(
                "Drag a row by its handle to reorder it, or use the arrows.",
                tokens,
            ));
        }
    }
    page = page.push(category_form(app, main));
    page = page.push(match editing {
        Some((id, channel)) => channel_editor(app, main, id, channel),
        None => new_channel(app, main),
    });
    page.into()
}

/// What the ghost says while something is in flight.
fn dragged_label(app: &App, main: &MainState) -> Option<String> {
    let drag = app.ui.drag.as_ref()?;
    match drag.item {
        DragItem::Channel(id) => Some(format!("#{}", main.server.channel_title(id))),
        DragItem::Category(id) => main
            .server
            .categories
            .get(&id)
            .map(|category| category.name.clone()),
        // A role is dragged on the Roles page, not this one.
        DragItem::Role(_) => None,
    }
}

/// The tree: the channels with no category, then every category with its own.
fn tree<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let slot = app.ui.drag.and_then(|drag| drag.slot);
    let item = app.ui.drag.map(|drag| drag.item);
    let moving_channel = matches!(item, Some(DragItem::Channel(_)));
    let moving_category = matches!(item, Some(DragItem::Category(_)));

    let loose = main.server.channels_in(None);
    let categories = main.server.ordered_categories();
    let mut rows = Column::new().spacing(1).width(Length::Fill);

    for (index, channel) in loose.iter().copied().enumerate() {
        if moving_channel {
            rows = rows.push(channel_slot(channel.id, slot, tokens));
        }
        rows = rows.push(channel_row(
            app,
            main,
            channel,
            index > 0,
            index + 1 < loose.len() || !categories.is_empty(),
        ));
    }
    if moving_channel {
        rows = rows.push(end_slot(None, slot, tokens));
    }
    if loose.is_empty() && !moving_channel {
        rows = rows.push(hint("No channel sits outside a category.", tokens));
    }

    for (index, category) in categories.iter().copied().enumerate() {
        if moving_category {
            let before = DragSlot::BeforeCategory(category.id);
            rows = rows.push(drop_slot(before, slot == Some(before), 0.0, tokens));
        }
        rows = rows.push(category_row(
            app,
            main,
            category,
            index > 0,
            index + 1 < categories.len(),
        ));

        let channels = main.server.channels_in(Some(category.id));
        for (at, channel) in channels.iter().copied().enumerate() {
            if moving_channel {
                rows = rows.push(channel_slot(channel.id, slot, tokens));
            }
            rows = rows.push(channel_row(
                app,
                main,
                channel,
                at > 0 || index > 0 || !loose.is_empty(),
                at + 1 < channels.len() || index + 1 < categories.len(),
            ));
        }
        if moving_channel {
            rows = rows.push(end_slot(Some(category.id), slot, tokens));
        }
    }
    if moving_category {
        rows = rows.push(drop_slot(
            DragSlot::EndOfCategories,
            slot == Some(DragSlot::EndOfCategories),
            0.0,
            tokens,
        ));
    }

    container(rows)
        .padding(12)
        .width(Length::Fill)
        .style(styles::container::card(tokens))
        .into()
}

fn channel_slot<'a>(
    id: i64,
    hovered: Option<DragSlot>,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    let slot = DragSlot::BeforeChannel(id);
    drop_slot(slot, hovered == Some(slot), INDENT, tokens)
}

fn end_slot<'a>(
    category: Option<i64>,
    hovered: Option<DragSlot>,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    let slot = DragSlot::EndOfCategory(category);
    drop_slot(slot, hovered == Some(slot), INDENT, tokens)
}

/// One category: its name, what it holds, and everything that can be done to it.
fn category_row<'a>(
    app: &'a App,
    main: &'a MainState,
    category: &'a Category,
    up: bool,
    down: bool,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let id = category.id;
    let count = main.server.channels_in(Some(id)).len();

    container(
        row![
            grip(DragItem::Category(id), true, "Drag to reorder", tokens),
            text(category.name.clone())
                .size(TEXT_ROW)
                .color(tokens.text_primary),
            text(count.to_string())
                .size(TEXT_BADGE)
                .color(tokens.text_muted),
            Space::new().width(Length::Fill),
            arrows(DragItem::Category(id), up, down, tokens),
            widgets::icon_button(
                Icon::Plus,
                "New channel in this category",
                Some(Message::Admin(AdminMsg::ChannelDraftCategory(Some(id)))),
                tokens,
            ),
            widgets::icon_button(
                Icon::Edit,
                "Rename",
                Some(Message::Ui(UiMsg::OpenDialog(Dialog::EditCategory {
                    id,
                    name: category.name.clone(),
                }))),
                tokens,
            ),
            widgets::icon_button(
                Icon::Trash,
                "Delete (its channels move out of it)",
                Some(Message::Ui(UiMsg::OpenDialog(
                    Dialog::ConfirmDeleteCategory { id }
                ))),
                tokens,
            ),
        ]
        .spacing(6)
        .align_y(Vertical::Center),
    )
    .padding([2.0, 4.0])
    .width(Length::Fill)
    .into()
}

/// One channel: its kind, its name, its topic, and the controls for it.
fn channel_row<'a>(
    app: &'a App,
    main: &'a MainState,
    channel: &'a Channel,
    up: bool,
    down: bool,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let id = channel.id;
    let general = main.server.is_general(id);
    let open = main.admin.channel.id == Some(id);
    // The server resolves Manage channels inside the channel, so the page being
    // open says nothing about this row: a denying override here takes every
    // control away.
    let manage = main.server.can(permissions::MANAGE_CHANNELS, Some(id));

    let mut line = row![
        Space::new().width(INDENT),
        grip(
            DragItem::Channel(id),
            manage,
            if manage {
                "Drag to reorder"
            } else {
                MISSING_CHANNELS
            },
            tokens,
        ),
        icons::icon(kind_glyph(channel), GLYPH, tokens.text_muted),
        text(channel.name.clone()).size(TEXT_ROW).color(if open {
            tokens.text_primary
        } else {
            tokens.text_secondary
        }),
    ]
    .spacing(6)
    .align_y(Vertical::Center);

    if !channel.topic.is_empty() {
        line = line.push(
            text(channel.topic.clone())
                .size(TEXT_BADGE)
                .color(tokens.text_muted),
        );
    }
    line = line.push(Space::new().width(Length::Fill));
    line = line.push(arrows(
        DragItem::Channel(id),
        up && manage,
        down && manage,
        tokens,
    ));
    line = line.push(widgets::icon_button(
        Icon::Gear,
        if manage {
            "Channel settings and permissions"
        } else {
            MISSING_CHANNELS
        },
        manage.then_some(Message::Admin(AdminMsg::OverrideTarget(
            id,
            OverrideTargetKind::Role,
            0,
        ))),
        tokens,
    ));
    let delete_tip = match (general, manage) {
        (true, _) => "general can never be deleted",
        (false, false) => MISSING_CHANNELS,
        (false, true) => "Delete",
    };
    line = line.push(widgets::icon_button(
        Icon::Trash,
        delete_tip,
        (!general && manage).then_some(Message::Ui(UiMsg::OpenDialog(
            Dialog::ConfirmDeleteChannel { channel_id: id },
        ))),
        tokens,
    ));

    let body = container(line).padding([2.0, 4.0]).width(Length::Fill);
    if open {
        body.style(styles::container::elevated(tokens)).into()
    } else {
        body.into()
    }
}

/// The new-category form. Renaming one is its own dialog, off the row itself.
fn category_form<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let draft = &main.admin.category;
    let full = main.server.categories.len() >= admin::MAX_CATEGORIES;
    let ready = !draft.name.trim().is_empty() && !full;

    card(
        "New category",
        vec![
            row![
                text_input("Off topic", &draft.name)
                    .on_input(|value| Message::Admin(AdminMsg::CategoryDraftName(value)))
                    .on_submit(Message::Admin(AdminMsg::CategorySave))
                    .padding(8)
                    .size(TEXT_BODY)
                    .style(styles::text_input(tokens)),
                action(
                    Kind::Primary,
                    "Create category",
                    ready.then_some(Message::Admin(AdminMsg::CategorySave)),
                    &cap_tip(full, admin::MAX_CATEGORIES, "categories"),
                    tokens,
                ),
            ]
            .spacing(8)
            .align_y(Vertical::Center)
            .into(),
        ],
        tokens,
    )
}

/// The form a channel that does not exist yet is described in.
fn new_channel<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let draft = &main.admin.channel;
    let full = channel_count(main) >= admin::MAX_CHANNELS;
    let ready = !draft.name.trim().is_empty() && !full;

    card(
        "New channel",
        vec![
            field("Name", name_input(&draft.name, tokens), tokens),
            field("Topic", topic_input(&draft.topic, tokens), tokens),
            row![
                field("Kind", kind_picker(draft.kind, tokens), tokens),
                field(
                    "Category",
                    category_picker(main, draft.category_id, tokens),
                    tokens,
                ),
            ]
            .spacing(12)
            .into(),
            action(
                Kind::Primary,
                "Create channel",
                ready.then_some(Message::Admin(AdminMsg::ChannelSave)),
                &cap_tip(full, admin::MAX_CHANNELS, "channels"),
                tokens,
            ),
        ],
        tokens,
    )
}

/// The editor for the channel in focus, with its overrides under it.
fn channel_editor<'a>(
    app: &'a App,
    main: &'a MainState,
    id: i64,
    channel: &'a Channel,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let draft = &main.admin.channel;
    let ready = !draft.name.trim().is_empty();
    let general = main.server.is_general(id);

    card(
        &format!("Channel settings — {}", channel.name),
        vec![
            field("Name", name_input(&draft.name, tokens), tokens),
            field("Topic", topic_input(&draft.topic, tokens), tokens),
            hint(
                "The kind is fixed, and a channel changes category by being dragged there.",
                tokens,
            ),
            row![
                action(
                    Kind::Primary,
                    "Save changes",
                    ready.then_some(Message::Admin(AdminMsg::ChannelSave)),
                    if ready {
                        ""
                    } else {
                        "A channel name is required"
                    },
                    tokens,
                ),
                Space::new().width(Length::Fill),
                action(
                    Kind::Danger,
                    "Delete channel",
                    (!general).then_some(Message::Ui(UiMsg::OpenDialog(
                        Dialog::ConfirmDeleteChannel { channel_id: id },
                    ))),
                    if general {
                        "general can never be deleted"
                    } else {
                        ""
                    },
                    tokens,
                ),
            ]
            .align_y(Vertical::Center)
            .into(),
            overrides(app, main, id, channel),
        ],
        tokens,
    )
}

fn name_input<'a>(value: &str, tokens: &'a ThemeTokens) -> Element<'a, Message> {
    text_input("valorant", value)
        .on_input(|value| Message::Admin(AdminMsg::ChannelDraftName(value)))
        .on_submit(Message::Admin(AdminMsg::ChannelSave))
        .padding(8)
        .size(TEXT_BODY)
        .style(styles::text_input(tokens))
        .into()
}

fn topic_input<'a>(value: &str, tokens: &'a ThemeTokens) -> Element<'a, Message> {
    text_input("ranked nights, tue and thu", value)
        .on_input(|value| Message::Admin(AdminMsg::ChannelDraftTopic(value)))
        .padding(8)
        .size(TEXT_BODY)
        .style(styles::text_input(tokens))
        .into()
}

/// The override editor: who has one, who to give one to, and the fourteen bits.
fn overrides<'a>(
    app: &'a App,
    main: &'a MainState,
    id: i64,
    channel: &'a Channel,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let selected = main
        .admin
        .override_target
        .filter(|(channel_id, _, _)| *channel_id == id);

    let mut body = Column::new()
        .push(widgets::section_label("Permission overrides", tokens))
        .spacing(8)
        .width(Length::Fill);

    let mut chips = Row::new().spacing(6).align_y(Vertical::Center);
    for entry in &channel.overrides {
        let (kind, target) = if entry.role_id != 0 {
            (OverrideTargetKind::Role, entry.role_id)
        } else {
            (OverrideTargetKind::Member, entry.user_id)
        };
        let open = selected
            .is_some_and(|(_, open_kind, open_target)| open_kind == kind && open_target == target);
        chips = chips.push(
            button(text(target_name(main, kind, target)).size(TEXT_BADGE))
                .padding([2.0, 8.0])
                .style(styles::button::row_for(tokens, open))
                .on_press(Message::Admin(AdminMsg::OverrideTarget(id, kind, target))),
        );
    }
    if channel.overrides.is_empty() {
        chips = chips.push(hint("Nobody has one here yet.", tokens));
    }
    body = body.push(chips);

    let full = channel.overrides.len() >= admin::MAX_OVERRIDES;
    body = body.push(target_picker(main, id, full, tokens));

    if let Some((_, kind, target)) = selected {
        body = body.push(bit_table(app, main, id, channel, kind, target));
    }
    body.into()
}

/// Every role and member that could be given an override here.
fn target_picker<'a>(
    main: &'a MainState,
    id: i64,
    full: bool,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    if full {
        return hint(
            &format!(
                "A channel holds at most {} overrides.",
                admin::MAX_OVERRIDES
            ),
            tokens,
        );
    }

    let mut options: Vec<Target> = main
        .server
        .roles
        .values()
        .map(|role| Target {
            kind: OverrideTargetKind::Role,
            id: role.id,
            label: role.name.clone(),
        })
        .collect();
    options.sort_by(|left, right| left.label.cmp(&right.label));
    options.extend(main.server.members.values().map(|member| Target {
        kind: OverrideTargetKind::Member,
        id: member.user_id,
        label: format!("@{}", member.username),
    }));

    pick_list(options, None::<Target>, move |target| {
        Message::Admin(AdminMsg::OverrideTarget(id, target.kind, target.id))
    })
    .placeholder("Add a role or a member…")
    .text_size(TEXT_ROW)
    .padding([6.0, 10.0])
    .style(styles::pick_list(tokens))
    .menu_style(styles::menu(tokens))
    .into()
}

/// The fourteen channel-scoped bits, each in one of its three states.
fn bit_table<'a>(
    app: &'a App,
    main: &'a MainState,
    id: i64,
    channel: &'a Channel,
    kind: OverrideTargetKind,
    target: i64,
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let (allow, deny) = admin::override_pair(channel, kind, target);
    let everyone = kind == OverrideTargetKind::Role
        && main.server.everyone().is_some_and(|role| role.id == target);
    // `PROTOCOL.md` § Channels: general's `VIEW_CHANNEL` can never be taken from
    // `@everyone` — the write is refused and resolution forces it back on anyway.
    let general_view = everyone && main.server.is_general(id);

    let mut rows = Column::new()
        .push(
            text(format!("Override for {}", target_name(main, kind, target)))
                .size(TEXT_SECONDARY)
                .color(tokens.text_secondary),
        )
        .spacing(4)
        .width(Length::Fill);

    for (bit, _) in permissions::BITS {
        if !permissions::has(permissions::CHANNEL_SCOPED, bit) {
            continue;
        }
        let grantable = main.server.can_grant(bit);
        let locked = general_view && bit == permissions::VIEW_CHANNEL;
        rows = rows.push(
            row![
                Column::new()
                    .push(
                        text(permissions::label(bit))
                            .size(TEXT_ROW)
                            .color(tokens.text_primary),
                    )
                    .push(
                        text(permissions::describe(bit))
                            .size(TEXT_BADGE)
                            .color(tokens.text_muted),
                    )
                    .spacing(1)
                    .width(Length::Fill),
                tri_state(
                    (id, kind, target),
                    bit,
                    admin::tri_of(allow, deny, bit),
                    grantable,
                    !locked,
                    &bit_tip(grantable, locked, bit),
                    tokens,
                ),
            ]
            .spacing(8)
            .align_y(Vertical::Center),
        );
    }

    rows = rows.push(action(
        Kind::Danger,
        "Remove override",
        Some(Message::Admin(AdminMsg::OverrideRemove(id, kind, target))),
        "",
        tokens,
    ));
    container(rows)
        .padding(10)
        .width(Length::Fill)
        .style(styles::container::elevated(tokens))
        .into()
}

/// Why one row of the table cannot be changed.
fn bit_tip(grantable: bool, locked: bool, bit: u64) -> String {
    if locked {
        return "general is always visible".to_owned();
    }
    if !grantable {
        return format!(
            "Allow and Deny need {}; Inherit needs nothing",
            permissions::name(bit).unwrap_or_default()
        );
    }
    String::new()
}

/// One bit as deny, inherit or allow.
fn tri_state<'a>(
    at: (i64, OverrideTargetKind, i64),
    bit: u64,
    current: TriState,
    grantable: bool,
    deny_allowed: bool,
    tip: &str,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    let (id, kind, target) = at;
    let mut bar = Row::new().spacing(2).align_y(Vertical::Center);
    for (state, glyph) in [
        (TriState::Deny, "✕"),
        (TriState::Inherit, "/"),
        (TriState::Allow, "✓"),
    ] {
        let chosen = current == state;
        // `SetOverride` asks for every bit in `allow | deny`, so clearing a bit
        // back to inherited needs nothing at all.
        let held = match state {
            TriState::Inherit => true,
            TriState::Allow => grantable,
            TriState::Deny => grantable && deny_allowed,
        };
        let offered = held && !chosen;
        bar = bar.push(
            button(text(glyph).size(TEXT_BADGE))
                .padding([2.0, 7.0])
                .style(styles::button::row_for(tokens, chosen))
                .on_press_maybe(offered.then_some(Message::Admin(AdminMsg::OverrideSet(
                    id, kind, target, bit, state,
                )))),
        );
    }
    widgets::tooltip_of(bar, tip, tooltip::Position::Left, tokens)
}

/// What one override's target is called.
fn target_name(main: &MainState, kind: OverrideTargetKind, target: i64) -> String {
    match kind {
        OverrideTargetKind::Role => main
            .server
            .roles
            .get(&target)
            .map_or_else(|| "a deleted role".to_owned(), |role| role.name.clone()),
        OverrideTargetKind::Member => format!("@{}", main.server.display_name(target)),
    }
}

/// How many channels exist, DMs excluded: the cap counts those alone.
fn channel_count(main: &MainState) -> usize {
    main.server
        .channels
        .values()
        .filter(|channel| channel_kind(channel) != ChannelKind::Dm)
        .count()
}

/// What a control disabled by a cap says.
fn cap_tip(full: bool, cap: usize, what: &str) -> String {
    if full {
        format!("A server holds at most {cap} {what}")
    } else {
        String::new()
    }
}

fn kind_glyph(channel: &Channel) -> Icon {
    match channel_kind(channel) {
        ChannelKind::Voice => Icon::Speaker,
        _ => Icon::Hash,
    }
}

fn kind_picker<'a>(current: ChannelKind, tokens: &'a ThemeTokens) -> Element<'a, Message> {
    let options = vec![
        KindOption(ChannelKind::Text),
        KindOption(ChannelKind::Voice),
    ];
    pick_list(options, Some(KindOption(current)), |option| {
        Message::Admin(AdminMsg::ChannelDraftKind(option.0))
    })
    .text_size(TEXT_ROW)
    .padding([6.0, 10.0])
    .style(styles::pick_list(tokens))
    .menu_style(styles::menu(tokens))
    .into()
}

fn category_picker<'a>(
    main: &'a MainState,
    current: Option<i64>,
    tokens: &'a ThemeTokens,
) -> Element<'a, Message> {
    let mut options = vec![CategoryOption {
        id: None,
        name: "No category".to_owned(),
    }];
    options.extend(
        main.server
            .ordered_categories()
            .into_iter()
            .map(|category| CategoryOption {
                id: Some(category.id),
                name: category.name.clone(),
            }),
    );
    let selected = options.iter().find(|option| option.id == current).cloned();

    pick_list(options, selected, |option| {
        Message::Admin(AdminMsg::ChannelDraftCategory(option.id))
    })
    .text_size(TEXT_ROW)
    .padding([6.0, 10.0])
    .style(styles::pick_list(tokens))
    .menu_style(styles::menu(tokens))
    .into()
}

/// A pick-list entry for a channel's kind.
#[derive(Clone, PartialEq, Eq)]
struct KindOption(ChannelKind);

impl fmt::Display for KindOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.0 {
            ChannelKind::Voice => "Voice",
            _ => "Text",
        })
    }
}

/// A pick-list entry for a category; `None` is the group above them all.
#[derive(Clone, PartialEq, Eq)]
struct CategoryOption {
    id: Option<i64>,
    name: String,
}

impl fmt::Display for CategoryOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
    }
}

/// A pick-list entry for an override's target.
#[derive(Clone, PartialEq, Eq)]
struct Target {
    kind: OverrideTargetKind,
    id: i64,
    label: String,
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}
