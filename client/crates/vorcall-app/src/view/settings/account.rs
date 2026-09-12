//! The Account page: who is signed in, the password, the diagnostics this
//! installation can hand over, the way out and the updater.

use iced::alignment::Vertical;
use iced::widget::{button, column, container, row, text};
use iced::{Element, Length};

use crate::app::message::{AuthMsg, Message, SettingsMsg, UiMsg};
use crate::app::state::settings::ReportState;
use crate::app::state::ui::Dialog;
use crate::app::{App, MainState};
use crate::theme::{ThemeTokens, styles};
use crate::update_ui::{self, UpdateView};
use crate::view::settings::{field, section};
use crate::view::widgets;
use crate::view::{AVATAR, TEXT_BODY, TEXT_ROW, TEXT_SECONDARY, TEXT_SECTION, bold};

pub fn view<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;

    let identity = container(
        row![
            widgets::member_avatar(main, main.member_id, AVATAR, tokens),
            column![
                text(main.server.display_name(main.member_id).to_owned())
                    .size(TEXT_SECTION)
                    .font(bold())
                    .color(tokens.text_primary),
                text(format!("@{}", app.username()))
                    .size(TEXT_ROW)
                    .color(tokens.text_secondary),
            ]
            .spacing(2),
        ]
        .spacing(12)
        .align_y(Vertical::Center),
    )
    .padding(16)
    .width(Length::Fill)
    .style(styles::container::card(tokens));

    let password = button(text("Change password").size(TEXT_ROW))
        .padding([6.0, 14.0])
        .style(styles::button::secondary(tokens))
        .on_press(Message::Ui(UiMsg::OpenDialog(Dialog::ChangePassword {
            current: String::new(),
            new: String::new(),
            confirm: String::new(),
            error: None,
            busy: false,
        })));

    // Listing the directory is all this reads: nothing is opened until the button
    // is pressed, and then off the UI thread.
    let log = match vorcall_core::diagnostics::log_path() {
        Some(path) => format!("Log file: {}", path.display()),
        None => "No log file on this system".to_owned(),
    };

    let logout = button(text("Log out").size(TEXT_ROW))
        .padding([6.0, 14.0])
        .style(styles::button::danger(tokens))
        .on_press(Message::Auth(AuthMsg::Logout));

    column![
        identity,
        section(
            "Password",
            tokens,
            vec![field(
                "Password",
                password,
                Some("You stay signed in on this machine; other machines are not signed out."),
                tokens,
            ),],
        ),
        section(
            "Diagnostics",
            tokens,
            vec![field(
                "Problem report",
                report(main, tokens),
                Some(&log),
                tokens,
            )],
        ),
        section(
            "Updates",
            tokens,
            vec![update_ui::section(UpdateView {
                state: &app.update,
                notes: app.update_notes.as_ref(),
                elapsed: app.loading_elapsed,
                tokens,
            })],
        ),
        section(
            "This account",
            tokens,
            vec![
                text("Logging out keeps nothing on this machine but your preferences.")
                    .size(TEXT_BODY)
                    .color(tokens.text_secondary)
                    .into(),
                logout.into(),
            ],
        ),
    ]
    .spacing(24)
    .width(Length::Fill)
    .into()
}

/// The one button that hands the log and every crash report to the server, with
/// what the last press came to beside it.
fn report<'a>(main: &'a MainState, tokens: &'a ThemeTokens) -> Element<'a, Message> {
    let sending = main.settings.report == ReportState::Sending;
    let send = button(text("Report a problem").size(TEXT_ROW))
        .padding([6.0, 14.0])
        .style(styles::button::secondary(tokens))
        .on_press_maybe((!sending).then_some(Message::Settings(SettingsMsg::ReportProblem)));

    row![send, report_line(&main.settings.report, tokens)]
        .spacing(12)
        .align_y(Vertical::Center)
        .into()
}

fn report_line<'a>(state: &ReportState, tokens: &'a ThemeTokens) -> Element<'a, Message> {
    let (line, color) = match state {
        ReportState::Idle => (
            "Sends the log and any crash reports to the server".to_owned(),
            tokens.text_muted,
        ),
        ReportState::Sending => ("Sending…".to_owned(), tokens.text_muted),
        ReportState::Sent(count) => (format!("Sent {count} files"), tokens.text_secondary),
        ReportState::Failed(error) => (error.clone(), tokens.warning),
    };
    text(line).size(TEXT_SECONDARY).color(color).into()
}
