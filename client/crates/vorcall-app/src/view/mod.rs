//! Drawing. Every function here is a pure read of the state in [`crate::app`].
//!
//! The root picks what one window shows: the splash's entrance, the popped-out
//! stage, the sign-in screens, the update that takes the whole window, or the
//! shell — the rail, a page, and whatever overlay is in front of it.

pub mod channels;
pub mod chat;
pub mod composer;
pub mod context_menu;
pub mod dms;
pub mod members;
pub mod message;
pub mod overlays;
pub mod profile_card;
pub mod quick_switcher;
pub mod rail;
pub mod selectable;
pub mod server_settings;
pub mod settings;
pub mod stage;
pub mod toasts;
pub mod widgets;

use iced::alignment::{Horizontal, Vertical};
use iced::widget::{Id, Space, button, column, container, keyed, row, stack, text, text_input};
use iced::{Element, Font, Length, font, window};
use vorcall_core::config;

use crate::app::message::{AuthMsg, Message, WindowMsg};
use crate::app::state::chat::MainView;
use crate::app::state::ui::Route;
use crate::app::{App, MainState, Screen};
use crate::brand::mark::mark;
use crate::theme::styles;
use crate::update_ui::{self, UpdateView};

/// The widget ids the keyboard and the focus operations name.
pub const MESSAGES_ID: &str = "vorcall-messages";
pub const COMPOSER_ID: &str = "vorcall-composer";
pub const USERNAME_ID: &str = "vorcall-username";
pub const PASSWORD_ID: &str = "vorcall-password";
pub const CURRENT_PASSWORD_ID: &str = "vorcall-current-password";
pub const QUICK_SWITCHER_ID: &str = "vorcall-quick-switcher";

/// The type scale, in points, from the design: badge and meta, secondary, list
/// rows, body, a channel title, a section heading, a page title.
pub const TEXT_BADGE: f32 = 11.0;
pub const TEXT_SECONDARY: f32 = 12.0;
pub const TEXT_ROW: f32 = 13.0;
pub const TEXT_BODY: f32 = 14.0;
pub const TEXT_TITLE: f32 = 15.0;
pub const TEXT_SECTION: f32 = 18.0;
pub const TEXT_PAGE: f32 = 22.0;

/// The far-left rail, which never changes width.
pub const RAIL_WIDTH: f32 = 56.0;
/// The chat header and the user bar under the channel list.
pub const HEADER_HEIGHT: f32 = 48.0;
pub const USER_BAR_HEIGHT: f32 = 56.0;
/// An avatar in the message list, and the same in a compact list.
pub const AVATAR: f32 = 40.0;
pub const AVATAR_SMALL: f32 = 24.0;
/// How wide a field on the sign-in screen is.
const FIELD_WIDTH: f32 = 320.0;

/// The shell's stacked layers, keyed so the update banner coming and going does
/// not reset the state of everything under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layer {
    Banner,
    Body,
}

pub fn bold() -> Font {
    Font {
        weight: font::Weight::Bold,
        ..Font::DEFAULT
    }
}

/// What one window draws.
pub fn view(app: &App, window: window::Id) -> Element<'_, Message> {
    if app.splash == Some(window) {
        return match &app.entrance {
            Some(entrance) => entrance.view(Message::Window(WindowMsg::SplashSkip)),
            None => Space::new().into(),
        };
    }

    // The popped-out stage is its own window, and nothing else is in it.
    if let Screen::Main(main) = &app.screen
        && main.voice.watch.popped == Some(window)
    {
        return stage::popped_window(app, main);
    }

    match &app.screen {
        Screen::Login { .. } | Screen::Register { .. } => auth_screen(app),
        Screen::Main(main) => {
            if app.update.shows_required(app.force_required) {
                return update_ui::required(UpdateView {
                    state: &app.update,
                    notes: app.update_notes.as_ref(),
                    elapsed: app.loading_elapsed,
                    tokens: &app.tokens,
                });
            }
            shell(app, main)
        }
    }
}

/// How much one window is magnified. The font scale is the whole interface's
/// scale, so it rides the window rather than every text size: the splash plays a
/// fixed-size entrance and the popped-out stage is a picture, so both stay at
/// one.
pub fn scale_factor(app: &App, window: window::Id) -> f32 {
    if app.splash == Some(window) {
        return 1.0;
    }
    if let Screen::Main(main) = &app.screen
        && main.voice.watch.popped == Some(window)
    {
        return 1.0;
    }
    // A scale out of the slider's range, or not a number at all, would leave the
    // window unusable with no way back to the setting that did it.
    if app.config.font_scale.is_nan() {
        return config::FONT_SCALE_DEFAULT;
    }
    app.config
        .font_scale
        .clamp(config::FONT_SCALE_MIN, config::FONT_SCALE_MAX)
}

/// The rail, the page beside it, and every overlay over both.
fn shell<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let page: Element<'_, Message> = match app.ui.route {
        Route::Main => match main.chat.view {
            MainView::Server => server_page(app, main),
            MainView::Dms => dms_page(app, main),
        },
        Route::Settings(tab) => settings::view(app, main, tab),
        Route::ServerSettings(tab) => server_settings::view(app, main, tab),
    };

    let body = row![rail::view(app, main), page]
        .width(Length::Fill)
        .height(Length::Fill);

    // The banner belongs to the window, not to a page: it stays put while the
    // settings are open. Keyed, because a positional column would rebuild every
    // widget's state below it — the composer's focus included — the moment the
    // banner appears or goes.
    let layers = keyed::Column::new()
        .width(Length::Fill)
        .height(Length::Fill)
        .push_maybe(
            Layer::Banner,
            update_ui::banner(UpdateView {
                state: &app.update,
                notes: app.update_notes.as_ref(),
                elapsed: app.loading_elapsed,
                tokens: &app.tokens,
            }),
        )
        .push(Layer::Body, body);

    stack![
        container(layers).style(styles::container::chat(&app.tokens)),
        overlays::view(app, main),
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

/// The server view: the channel list, the conversation and the member pane.
fn server_page<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let mut panes = row![channels::pane(app, main), chat::pane(app, main)]
        .width(Length::Fill)
        .height(Length::Fill);
    if app.ui.show_members {
        panes = panes.push(members::pane(app, main));
    }
    panes.into()
}

/// The DM view: the conversation list, the conversation and the other person.
fn dms_page<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    row![
        dms::pane(app, main),
        chat::pane(app, main),
        dms::profile_pane(app, main),
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

/// Sign in, or create an account: the same card with different fields.
fn auth_screen(app: &App) -> Element<'_, Message> {
    let tokens = &app.tokens;
    let registering = matches!(app.screen, Screen::Register { .. });

    let (username, password, error, busy) = match &app.screen {
        Screen::Login {
            username,
            password,
            error,
            busy,
        } => (username, password, error, *busy),
        Screen::Register {
            username,
            password,
            error,
            busy,
            ..
        } => (username, password, error, *busy),
        Screen::Main(_) => return Space::new().into(),
    };

    let submit = if registering {
        Message::Auth(AuthMsg::RegisterSubmit)
    } else {
        Message::Auth(AuthMsg::LoginSubmit)
    };

    let mut fields = column![
        row![mark(40.0), text("Vorcall").size(34).font(bold())]
            .spacing(10)
            .align_y(Vertical::Center),
        text(if registering {
            "Create account"
        } else {
            "Sign in"
        })
        .size(TEXT_SECTION)
        .color(tokens.text_secondary),
        text_input("Username", username)
            .id(Id::new(USERNAME_ID))
            .on_input(|value| Message::Auth(AuthMsg::UsernameChanged(value)))
            .on_submit(submit.clone())
            .padding(12)
            .width(FIELD_WIDTH)
            .style(styles::text_input(tokens)),
        text_input("Password", password)
            .id(Id::new(PASSWORD_ID))
            .secure(true)
            .on_input(|value| Message::Auth(AuthMsg::PasswordChanged(value)))
            .on_submit(submit.clone())
            .padding(12)
            .width(FIELD_WIDTH)
            .style(styles::text_input(tokens)),
    ]
    .spacing(16)
    .align_x(Horizontal::Center);

    if let Screen::Register {
        confirm, invite, ..
    } = &app.screen
    {
        fields = fields.push(
            text_input("Confirm password", confirm)
                .secure(true)
                .on_input(|value| Message::Auth(AuthMsg::ConfirmChanged(value)))
                .on_submit(submit.clone())
                .padding(12)
                .width(FIELD_WIDTH)
                .style(styles::text_input(tokens)),
        );
        fields = fields.push(
            text_input("XXXXX-XXXXX-XXXXX-XXXXX", invite)
                .on_input(|value| Message::Auth(AuthMsg::InviteChanged(value)))
                .on_submit(submit.clone())
                .padding(12)
                .width(FIELD_WIDTH)
                .style(styles::text_input(tokens)),
        );
    }

    let mut action = button(
        text(if registering {
            "Create account"
        } else {
            "Sign in"
        })
        .size(TEXT_BODY),
    )
    .padding(12)
    .width(FIELD_WIDTH)
    .style(styles::button::primary(tokens));
    if !busy {
        action = action.on_press(submit);
    }
    fields = fields.push(action);

    fields = fields.push(
        button(
            text(if registering {
                "Back to sign in"
            } else {
                "Create account"
            })
            .size(TEXT_ROW),
        )
        .on_press(Message::Auth(if registering {
            AuthMsg::ShowLogin
        } else {
            AuthMsg::ShowRegister
        }))
        .style(styles::button::ghost(tokens)),
    );

    if let Some(error) = error {
        fields = fields.push(text(error.clone()).size(TEXT_ROW).color(app.tokens.danger));
    }

    fields = fields.push(server_section(app));

    container(fields)
        .center(Length::Fill)
        .style(styles::container::chat(tokens))
        .into()
}

/// Which server the sign-in goes to, folded shut until someone needs it.
///
/// The point of it is a self-hosted Vorcall: the released binary carries the
/// project's own address, and this is how it is pointed somewhere else without a
/// rebuild.
fn server_section(app: &App) -> Element<'_, Message> {
    let tokens = &app.tokens;
    let form = &app.server_form;

    let summary = button(
        text(format!(
            "{} Server: {}",
            if form.open { "▾" } else { "▸" },
            app.endpoints.host()
        ))
        .size(TEXT_ROW),
    )
    .on_press(Message::Auth(AuthMsg::ServerToggle))
    .style(styles::button::ghost(tokens));

    if !form.open {
        return summary.into();
    }

    let mut body = column![summary].spacing(10).align_x(Horizontal::Center);

    if form.pinned {
        body = body.push(
            text("Pinned by VORCALL_SERVER_URL / VORCALL_SERVER_KEY in the environment.")
                .size(TEXT_ROW)
                .color(tokens.text_secondary)
                .width(FIELD_WIDTH),
        );
    }

    let mut address = text_input("https://vorcall.example.org", &form.url)
        .padding(12)
        .width(FIELD_WIDTH)
        .style(styles::text_input(tokens));
    let mut key = text_input(
        if app.config.server_key.is_some() {
            "Server key"
        } else {
            "Server key (leave blank to keep the current one)"
        },
        &form.key,
    )
    .secure(true)
    .padding(12)
    .width(FIELD_WIDTH)
    .style(styles::text_input(tokens));

    if !form.pinned {
        address = address
            .on_input(|value| Message::Auth(AuthMsg::ServerUrlChanged(value)))
            .on_submit(Message::Auth(AuthMsg::ServerSave));
        key = key
            .on_input(|value| Message::Auth(AuthMsg::ServerKeyChanged(value)))
            .on_submit(Message::Auth(AuthMsg::ServerSave));
    }

    body = body.push(address).push(key);

    if !form.pinned {
        let mut buttons = row![
            button(text("Use this server").size(TEXT_ROW))
                .on_press(Message::Auth(AuthMsg::ServerSave))
                .padding(10)
                .style(styles::button::primary(tokens)),
        ]
        .spacing(8);

        if form.overridden(&app.config) {
            buttons = buttons.push(
                button(text("Built-in server").size(TEXT_ROW))
                    .on_press(Message::Auth(AuthMsg::ServerReset))
                    .padding(10)
                    .style(styles::button::ghost(tokens)),
            );
        }
        body = body.push(buttons);
    }

    if let Some(error) = &form.error {
        body = body.push(
            text(error.clone())
                .size(TEXT_ROW)
                .color(tokens.danger)
                .width(FIELD_WIDTH),
        );
    }

    body.into()
}
