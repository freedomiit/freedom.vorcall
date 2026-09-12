//! The Keybinds page: one row per action, the binding in force, and the capture
//! that replaces it.
//!
//! A clash is drawn rather than refused — both rows of every pair say so — because
//! which of two bindings wins is a thing to see, not a thing to be stopped by.

use iced::alignment::Vertical;
use iced::widget::{Space, button, column, container, row, text};
use iced::{Element, Length};
use vorcall_core::config::KEYBIND_ACTIONS;

use crate::app::message::{Message, SettingsMsg};
use crate::app::state::rules::key_label;
use crate::app::state::settings::conflict_notes;
use crate::app::{App, MainState};
use crate::theme::{ThemeTokens, styles};
use crate::view::settings::section;
use crate::view::widgets;
use crate::view::{TEXT_BADGE, TEXT_ROW, TEXT_SECONDARY};

/// The actions whose binding is not the user's to change: the composer's own
/// Up arrow, and the key that unwinds whatever is in front.
const FIXED: [&str; 2] = ["edit_last", "escape"];

pub fn view<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let notes = conflict_notes(&app.config);

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    for (action, _, global) in KEYBIND_ACTIONS {
        rows.push(entry(
            app,
            main,
            action,
            global,
            notes.get(action).map(Vec::as_slice).unwrap_or_default(),
        ));
    }

    column![
        section("Keybinds", tokens, rows),
        text(platform_note())
            .size(TEXT_SECONDARY)
            .color(tokens.text_muted),
    ]
    .spacing(16)
    .width(Length::Fill)
    .into()
}

fn entry<'a>(
    app: &'a App,
    main: &'a MainState,
    action: &'static str,
    global: bool,
    clashes: &[String],
) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let capturing = main.settings.capturing.as_deref() == Some(action);
    let fixed = FIXED.contains(&action);

    let mut left = row![
        text(label(action))
            .size(TEXT_ROW)
            .color(tokens.text_primary),
    ]
    .spacing(8)
    .align_y(Vertical::Center);
    if global {
        left = left.push(chip("Global", tokens));
    }

    let state: Element<'_, Message> = if capturing {
        text("Press a key… (Esc cancels)")
            .size(TEXT_ROW)
            .color(tokens.warning)
            .into()
    } else {
        widgets::key_hint(&key_label(app.config.keybind(action)), tokens)
    };

    let mut line = row![left.width(Length::Fill), state,]
        .spacing(12)
        .align_y(Vertical::Center);

    if !fixed {
        line = line.push(
            button(text(if capturing { "Cancel" } else { "Change" }).size(TEXT_ROW))
                .padding([4.0, 10.0])
                .style(styles::button::secondary(tokens))
                .on_press(Message::Settings(if capturing {
                    SettingsMsg::KeybindCancel
                } else {
                    SettingsMsg::KeybindCapture(action.to_owned())
                })),
        );
        line = line.push(
            button(text("Reset").size(TEXT_ROW))
                .padding([4.0, 10.0])
                .style(styles::button::ghost(tokens))
                .on_press(Message::Settings(SettingsMsg::KeybindReset(
                    action.to_owned(),
                ))),
        );
    } else {
        line = line.push(Space::new().width(96.0));
    }

    let mut stack = column![line].spacing(4).width(Length::Fill);
    if !clashes.is_empty() {
        let names: Vec<&str> = clashes.iter().map(String::as_str).map(label).collect();
        stack = stack.push(
            text(format!("Shares this binding with {}", names.join(", ")))
                .size(TEXT_SECONDARY)
                .color(tokens.warning),
        );
    }

    container(stack)
        .padding([6.0, 10.0])
        .width(Length::Fill)
        .style(styles::container::card(tokens))
        .into()
}

fn chip<'a>(label: &str, tokens: &'a ThemeTokens) -> Element<'a, Message> {
    container(
        text(label.to_owned())
            .size(TEXT_BADGE)
            .color(tokens.text_secondary),
    )
    .padding([1.0, 5.0])
    .style(styles::container::chip(tokens))
    .into()
}

/// What one action is called on the page. An id this build does not know is shown
/// as it is stored, which is also what a binding from an older build does.
fn label(action: &str) -> &str {
    match action {
        "push_to_talk" => "Push to talk",
        "toggle_mute" => "Mute",
        "toggle_deafen" => "Deafen",
        "quick_switcher" => "Quick switcher",
        "settings" => "Settings",
        "prev_channel" => "Previous channel",
        "next_channel" => "Next channel",
        "next_unread" => "Next unread",
        "prev_unread" => "Previous unread",
        "toggle_members" => "Member list",
        "edit_last" => "Edit your last message",
        "escape" => "Close what is in front",
        // Nothing else is in `KEYBIND_ACTIONS`, and the page only draws those.
        other => other,
    }
}

/// What "Global" costs on this platform.
fn platform_note() -> &'static str {
    if cfg!(target_os = "macos") {
        "Global bindings are captured outside the window and need the Input Monitoring \
         grant, which macOS ties to this exact build: it has to be given again after \
         every update. Caps Lock cannot be one of them."
    } else if cfg!(target_os = "linux") {
        "Global bindings are captured outside the window. Under Wayland the portal \
         asks once per run and takes keyboard bindings only — no mouse buttons."
    } else {
        "Global bindings are captured outside the window; everything else only \
         works while a Vorcall window has focus."
    }
}
