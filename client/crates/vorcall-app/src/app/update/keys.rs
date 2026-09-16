//! Raw input, before the keybinding map has said what it means.
//!
//! Order matters and is the whole of it: a keybinds tab waiting for a press takes
//! the key, then the quick switcher, then a dialog, then the map. A plain letter
//! or digit with no Ctrl or Alt on it belongs to whatever has the focus — it is
//! somebody typing — while a named key is nobody's to type and the three
//! system-wide actions are never held back at all, a dialog in front included: one
//! must not hold the microphone open.
//!
//! Several bindings can match one press, since the modifiers a binding asks for
//! only have to be held; the most specific one takes it.
//!
//! `iced::event::listen_with` reports a key whether or not a widget consumed it,
//! which is exactly why those guards are here.

use iced::keyboard::key::Named;
use iced::{Task, keyboard, mouse};
use vorcall_core::config::{KEYBIND_ACTIONS, TransmitMode};
use vorcall_hotkey::{ActionId, Binding, Edge};

use crate::app::App;
use crate::app::message::{
    ChannelsMsg, ChatMsg, DragMsg, KeyMsg, Message, SettingsMsg, UiMsg, VoiceMsg,
};
use crate::app::state::rules::{self, GLOBAL_ACTIONS, PUSH_TO_TALK, WINDOW_GENERATION};
use crate::app::state::settings::SettingsTab;
use crate::app::update::voice::hotkey_observes;
use crate::app::update::{channels, chat, drag, settings, ui, voice};

pub fn update(app: &mut App, message: KeyMsg) -> Task<Message> {
    match message {
        // The unread and notification rules read this, and the composer's focus
        // follows it.
        KeyMsg::Focus(focused) => {
            app.ui.focused = focused;
            // Whatever is on screen at the bottom has now been read.
            if focused
                && let Some(main) = app.main_mut()
                && main.chat.current().is_some_and(|channel| channel.at_bottom)
            {
                main.chat.schedule_mark_read();
            }
            // The first focus is the earliest moment the window certainly
            // exists, and the clipboard cannot be opened before it has been
            // asked which display it is on.
            if focused {
                return app.probe_display();
            }
            Task::none()
        }
        KeyMsg::KeyDown(key, modifiers) => key_down(app, &key, modifiers),
        KeyMsg::KeyUp(key, modifiers) => key_up(app, &key, modifiers),
        KeyMsg::MouseDown(button) => mouse_down(app, button),
        KeyMsg::MouseUp(button) => mouse_up(app, button),
        // A file dropped on the window goes the same way as one the dialog picked.
        KeyMsg::FileDropped(path) => chat::update(app, ChatMsg::FilesPicked(vec![path])),
    }
}

fn key_down(app: &mut App, key: &keyboard::Key, modifiers: keyboard::Modifiers) -> Task<Message> {
    tracing::trace!(?key, ?modifiers, "key down");

    if let Some(action) = capturing(app) {
        return capture(app, action, key, modifiers);
    }
    if app.ui.quick_switcher.open {
        return switcher_key(app, key);
    }
    // A dialog owns its own keys: Enter is the field's own submit. The three
    // system-wide actions still come through, the way a bound mouse button does —
    // a dialog must not hold the microphone open.
    if app.ui.dialog.is_some() {
        if named(key, Named::Escape) {
            return ui::update(app, UiMsg::Escape);
        }
        return match bound_action(app, key, modifiers, true) {
            Some(action) => act(app, action, Edge::Pressed),
            None => Task::none(),
        };
    }
    // The mention list takes Tab before the focus ring does, but only where it is
    // on screen: the chat route with no context menu over it. It is handled only
    // here: iced yields no binding of its own for Tab, so the composer never
    // sees it.
    if named(key, Named::Tab) {
        if app.ui.route.is_main()
            && app.ui.context_menu.is_none()
            && app.main().is_some_and(chat::mention_open)
        {
            return chat::update(app, ChatMsg::MentionAccept);
        }
        return ui::update(
            app,
            if modifiers.shift() {
                UiMsg::FocusPrevious
            } else {
                UiMsg::FocusNext
            },
        );
    }

    match bound_action(app, key, modifiers, false) {
        Some(action) => act(app, action, Edge::Pressed),
        None => Task::none(),
    }
}

/// Which action a key press belongs to, the most specific binding first. With
/// `only_global` nothing but the three system-wide actions is considered, which is
/// what a dialog leaves through.
fn bound_action(
    app: &App,
    key: &keyboard::Key,
    modifiers: keyboard::Modifiers,
    only_global: bool,
) -> Option<&'static str> {
    let candidates = KEYBIND_ACTIONS
        .into_iter()
        .filter(|(_, _, global)| *global || !only_global)
        .map(|(action, _, global)| (action, app.config.keybind(action), global));
    rules::bound_action(key, modifiers, candidates)
}

/// Only a global action has anything to do with a key going up: the talk spurt it
/// started ends here.
fn key_up(app: &mut App, key: &keyboard::Key, modifiers: keyboard::Modifiers) -> Task<Message> {
    tracing::trace!(?key, ?modifiers, "key up");

    if capturing(app).is_some() {
        return Task::none();
    }
    for (_, action, _) in GLOBAL_ACTIONS {
        if releases(key, modifiers, app.config.keybind(action)) {
            return act(app, action, Edge::Released);
        }
    }
    Task::none()
}

fn mouse_down(app: &mut App, button: mouse::Button) -> Task<Message> {
    tracing::trace!(?button, "mouse down");

    if let Some(action) = capturing(app) {
        // A mouse button is captured the way a key is; the ones no binding may
        // have never reach this.
        let Some(binding) = rules::binding_from_mouse(button) else {
            return Task::none();
        };
        return settings::update(app, SettingsMsg::KeybindCaptured(action, binding.name()));
    }
    mouse_action(app, button, Edge::Pressed)
}

fn mouse_up(app: &mut App, button: mouse::Button) -> Task<Message> {
    tracing::trace!(?button, "mouse up");

    // A drag ends where the button is let go, which is not necessarily over the
    // list that started it.
    if button == mouse::Button::Left && app.ui.drag.is_some() {
        return drag::update(app, DragMsg::DragEnd);
    }
    if capturing(app).is_some() {
        return Task::none();
    }
    mouse_action(app, button, Edge::Released)
}

/// A bound mouse button, whatever is in front: a dialog must not hold the
/// microphone open.
fn mouse_action(app: &mut App, button: mouse::Button, edge: Edge) -> Task<Message> {
    for (action, _, _) in KEYBIND_ACTIONS {
        if rules::mouse_matches(button, app.config.keybind(action)) {
            return act(app, action, edge);
        }
    }
    Task::none()
}

/// One bound action, by its id in [`KEYBIND_ACTIONS`].
fn act(app: &mut App, action: &str, edge: Edge) -> Task<Message> {
    // The three global actions belong to `vorcall-hotkey` while it is listening;
    // the window only stands in for it when it is not, and its edges carry
    // [`WINDOW_GENERATION`] so no listener's can be mistaken for them.
    if let Some(id) = global_action(action) {
        if hotkey_observes(app, id) {
            return Task::none();
        }
        // There is no push to talk in the mode that has none, and it is the only
        // global action with a release: the other two are toggles.
        if id == PUSH_TO_TALK && app.config.transmit_mode != TransmitMode::PushToTalk {
            return Task::none();
        }
        if edge == Edge::Released && id != PUSH_TO_TALK {
            return Task::none();
        }
        return voice::update(
            app,
            VoiceMsg::Hotkey {
                generation: WINDOW_GENERATION,
                action: id,
                edge,
            },
        );
    }
    if edge == Edge::Released {
        return Task::none();
    }

    match action {
        "quick_switcher" => ui::update(app, UiMsg::OpenQuickSwitcher),
        "settings" => settings::update(app, SettingsMsg::Open(SettingsTab::Account)),
        "prev_channel" => channels::update(app, ChannelsMsg::PrevChannel),
        "next_channel" => channels::update(app, ChannelsMsg::NextChannel),
        "next_unread" => channels::update(app, ChannelsMsg::NextUnread),
        "prev_unread" => channels::update(app, ChannelsMsg::PrevUnread),
        "toggle_members" => ui::update(app, UiMsg::ToggleMembers),
        "edit_last" => edit_last(app),
        "escape" => ui::update(app, UiMsg::Escape),
        // An action a later build added to the configuration but not here.
        _ => Task::none(),
    }
}

/// The keys the quick switcher owns while it is up. Everything else is typing,
/// which belongs to its field.
fn switcher_key(app: &mut App, key: &keyboard::Key) -> Task<Message> {
    let message = if named(key, Named::Escape) {
        UiMsg::Escape
    } else if named(key, Named::ArrowDown) {
        UiMsg::QuickSwitcherMove(1)
    } else if named(key, Named::ArrowUp) {
        UiMsg::QuickSwitcherMove(-1)
    } else if named(key, Named::Enter) {
        UiMsg::QuickSwitcherPick
    } else {
        return Task::none();
    };
    ui::update(app, message)
}

/// The press the keybinds tab is waiting for. Escape gives the capture up, so it
/// can never be a binding; a key no backend can bind leaves the tab waiting.
fn capture(
    app: &mut App,
    action: String,
    key: &keyboard::Key,
    modifiers: keyboard::Modifiers,
) -> Task<Message> {
    if named(key, Named::Escape) {
        return settings::update(app, SettingsMsg::KeybindCancel);
    }
    let Some(binding) = rules::capture_binding(key, modifiers) else {
        return Task::none();
    };
    settings::update(app, SettingsMsg::KeybindCaptured(action, binding.name()))
}

/// Up in an empty composer edits the last message of one's own. A composer with
/// anything in it owns the key: that is the caret moving.
fn edit_last(app: &mut App) -> Task<Message> {
    let wanted = {
        let Some(main) = app.main() else {
            return Task::none();
        };
        let composer = &main.chat.composer;
        if composer.editing.is_some() || !composer.text().trim().is_empty() {
            return Task::none();
        }
        main.chat
            .current()
            .and_then(|channel| channel.last_own_message_id(main.member_id))
    };
    match wanted {
        Some(id) => chat::update(app, ChatMsg::StartEdit(id)),
        None => Task::none(),
    }
}

/// Whether a key release ends `bound`. Only the trigger is compared: a chord
/// whose modifier was let go first still has to end the talk spurt, which is what
/// the global backends do as well. A binding outside the grammar has no trigger to
/// compare, and goes by its spelling.
fn releases(key: &keyboard::Key, modifiers: keyboard::Modifiers, bound: &str) -> bool {
    match (rules::binding_from_key(key), Binding::parse(bound)) {
        (Some(pressed), Some(binding)) => pressed.trigger == binding.trigger,
        _ => rules::key_matches(key, modifiers, bound),
    }
}

/// The id the edges of one of the three system-wide actions are tagged with.
fn global_action(action: &str) -> Option<ActionId> {
    GLOBAL_ACTIONS
        .iter()
        .find(|(_, name, _)| *name == action)
        .map(|(id, _, _)| *id)
}

/// The action whose next press is its new binding.
fn capturing(app: &App) -> Option<String> {
    app.main().and_then(|main| main.settings.capturing.clone())
}

fn named(key: &keyboard::Key, wanted: Named) -> bool {
    matches!(key, keyboard::Key::Named(pressed) if *pressed == wanted)
}
