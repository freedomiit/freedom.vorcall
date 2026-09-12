//! What the self-updater puts on screen: the banner under the header, the screen
//! a required update takes over, and the settings section.
//!
//! A pure read of [`UpdateState`], with one exception: the state line says how
//! long ago a check ended, which it reads off the clock rather than keeping a
//! timer alive for it.

use std::time::{Duration, Instant};

use iced::alignment::{Horizontal, Vertical};
use iced::widget::{Text, button, column, container, progress_bar, row, text};
use iced::{Element, Length};
use vorcall_core::update::{self, Ready, Version};

use crate::app::message::{Message, UpdateMsg};
use crate::app::state::rules::progress_fraction;
use crate::app::state::update::UpdateState;
use crate::brand::loading;
use crate::theme::{ThemeTokens, styles};
use crate::view::widgets::section_label;
use crate::view::{TEXT_BODY, TEXT_PAGE, TEXT_ROW, bold};

/// The creature beside a line of text, beside the settings line, and the one that
/// owns the required screen.
const INLINE_CREATURE: f32 = 24.0;
const SETTINGS_CREATURE: f32 = 20.0;
const REQUIRED_CREATURE: f32 = 128.0;
const BAR_LENGTH: f32 = 180.0;
const BAR_GIRTH: f32 = 8.0;

/// Everything the three views below read. Copied whole because the shell hands
/// the same thing to the banner and to the settings page.
#[derive(Clone, Copy)]
pub struct UpdateView<'a> {
    pub state: &'a UpdateState,
    pub notes: Option<&'a (Version, String)>,
    pub elapsed: Duration,
    pub tokens: &'a ThemeTokens,
}

/// The strip under the header, or nothing at all: most states have no news worth
/// a permanent line above the channel.
pub fn banner(view: UpdateView<'_>) -> Option<Element<'_, Message>> {
    let tokens = view.tokens;
    let content: Element<'_, Message> = match view.state {
        UpdateState::Downloading {
            version,
            received,
            total,
            ..
        } => row![
            loading::view(view.elapsed, INLINE_CREATURE),
            text(format!("Downloading Vorcall {version}…"))
                .size(TEXT_ROW)
                .color(tokens.text_primary),
            progress_bar(0.0..=1.0, progress_fraction(*received, *total))
                .length(BAR_LENGTH)
                .girth(BAR_GIRTH),
            text(format!("{}%", percent(*received, *total)))
                .size(TEXT_ROW)
                .color(tokens.text_muted),
        ]
        .spacing(12)
        .align_y(Vertical::Center)
        .into(),
        UpdateState::Ready {
            ready,
            dismissed: false,
        } if ready.manual => manual_notice(ready, false)
            .size(TEXT_ROW)
            .color(tokens.warning)
            .into(),
        UpdateState::Ready {
            ready,
            dismissed: false,
        } => row![
            text(format!("Vorcall {} is ready.", ready.manifest.version))
                .size(TEXT_ROW)
                .color(tokens.text_primary),
            button(text("Restart now").size(TEXT_ROW))
                .padding([4.0, 10.0])
                .style(styles::button::primary(tokens))
                .on_press(Message::Update(UpdateMsg::Restart)),
            button(text("Later").size(TEXT_ROW))
                .padding([4.0, 10.0])
                .style(styles::button::secondary(tokens))
                .on_press(Message::Update(UpdateMsg::Dismiss)),
        ]
        .spacing(12)
        .align_y(Vertical::Center)
        .into(),
        _ => return None,
    };

    Some(
        container(content)
            .width(Length::Fill)
            .padding(8)
            .style(styles::container::elevated(tokens))
            .into(),
    )
}

/// The whole window while an update the server insists on is on its way. The
/// state underneath is left alone: a restart that fails goes back to a channel
/// that is still there.
pub fn required(view: UpdateView<'_>) -> Element<'_, Message> {
    let tokens = view.tokens;
    let mut content = column![
        loading::view(view.elapsed, REQUIRED_CREATURE),
        text("Update required")
            .size(TEXT_PAGE)
            .font(bold())
            .color(tokens.text_primary),
    ]
    .spacing(16)
    .align_x(Horizontal::Center);

    if let Some(version) = target_version(view) {
        content = content.push(
            text(format!("Vorcall {version} is needed to keep chatting."))
                .size(TEXT_BODY)
                .color(tokens.text_secondary),
        );
    }

    match view.state {
        UpdateState::Downloading {
            received, total, ..
        } => {
            content = content
                .push(
                    progress_bar(0.0..=1.0, progress_fraction(*received, *total))
                        .length(BAR_LENGTH)
                        .girth(BAR_GIRTH),
                )
                .push(
                    text(format!("{}%", percent(*received, *total)))
                        .size(TEXT_ROW)
                        .color(tokens.text_muted),
                );
        }
        UpdateState::Ready { ready, .. } if ready.manual => {
            content = content.push(
                manual_notice(ready, true)
                    .size(TEXT_BODY)
                    .color(tokens.warning),
            );
        }
        // Whatever produced this `Ready` already asked for the restart.
        UpdateState::Ready { .. } | UpdateState::Restarting => {
            content = content.push(text("Restarting…").size(TEXT_BODY).color(tokens.text_muted));
        }
        UpdateState::Failed { message, .. } => {
            content = content
                .push(text(message.as_str()).size(TEXT_BODY).color(tokens.danger))
                .push(
                    button(text("Retry").size(TEXT_ROW))
                        .padding([4.0, 10.0])
                        .style(styles::button::primary(tokens))
                        .on_press(Message::Update(UpdateMsg::Check)),
                );
        }
        _ => {}
    }

    if let Some(notes) = view.notes {
        content = content.extend(whats_new(notes, tokens));
    }

    container(content)
        .center(Length::Fill)
        .padding(24)
        .style(styles::container::chat(tokens))
        .into()
}

/// The version line, the manual check and whatever the last one found.
pub fn section(view: UpdateView<'_>) -> Element<'_, Message> {
    let tokens = view.tokens;
    let check = button(text("Check for updates").size(TEXT_ROW))
        .padding([4.0, 10.0])
        .style(styles::button::secondary(tokens))
        .on_press_maybe((!view.state.busy()).then_some(Message::Update(UpdateMsg::Check)));

    let mut content = column![
        text(format!(
            "Version {} ({})",
            Version::current(),
            update::platform()
        ))
        .size(TEXT_ROW)
        .color(tokens.text_secondary),
        row![check, state_line(view)]
            .spacing(12)
            .align_y(Vertical::Center),
    ]
    .spacing(12);

    if let Some(notes) = view.notes {
        content = content.extend(whats_new(notes, tokens));
    }
    content.into()
}

/// Where the updater stands, in one line — with the one thing there is to press
/// about it, when there is one.
fn state_line(view: UpdateView<'_>) -> Element<'_, Message> {
    let tokens = view.tokens;
    match view.state {
        UpdateState::Disabled(reason) => text(format!("Updates are off: {reason}"))
            .size(TEXT_ROW)
            .color(tokens.text_muted)
            .into(),
        UpdateState::Idle => text("").size(TEXT_ROW).into(),
        UpdateState::Checking => row![
            loading::view(view.elapsed, SETTINGS_CREATURE),
            text("Checking…").size(TEXT_ROW).color(tokens.text_muted),
        ]
        .spacing(8)
        .align_y(Vertical::Center)
        .into(),
        UpdateState::UpToDate { at } => text(format!("Up to date · checked {}", ago(*at)))
            .size(TEXT_ROW)
            .color(tokens.success)
            .into(),
        UpdateState::NoBuild { platform } => {
            text(format!("No build for {platform} in this release"))
                .size(TEXT_ROW)
                .color(tokens.text_muted)
                .into()
        }
        UpdateState::Downloading {
            received, total, ..
        } => text(format!("Downloading… {}%", percent(*received, *total)))
            .size(TEXT_ROW)
            .color(tokens.text_muted)
            .into(),
        UpdateState::Ready { ready, .. } if ready.manual => manual_notice(ready, false)
            .size(TEXT_ROW)
            .color(tokens.warning)
            .into(),
        UpdateState::Ready { ready, .. } => row![
            text(format!("Vorcall {} is ready", ready.manifest.version))
                .size(TEXT_ROW)
                .color(tokens.success),
            button(text("Restart now").size(TEXT_ROW))
                .padding([4.0, 10.0])
                .style(styles::button::primary(tokens))
                .on_press(Message::Update(UpdateMsg::Restart)),
        ]
        .spacing(12)
        .align_y(Vertical::Center)
        .into(),
        UpdateState::Failed { message, at, .. } => {
            text(format!("Update check failed: {message} · {}", ago(*at)))
                .size(TEXT_ROW)
                .color(tokens.danger)
                .into()
        }
        UpdateState::Restarting => text("Restarting…")
            .size(TEXT_ROW)
            .color(tokens.text_muted)
            .into(),
    }
}

/// A download that landed outside the install directory. `and_restart` is for the
/// required screen, the one place the app is waiting on the person rather than
/// the other way round.
fn manual_notice<'a>(ready: &Ready, and_restart: bool) -> Text<'a> {
    let tail = if and_restart {
        " and start Vorcall again."
    } else {
        "."
    };
    text(format!(
        "Vorcall {} is ready — install it by hand from {}{tail}",
        ready.manifest.version,
        ready.file.display()
    ))
}

/// The release notes under their heading, as the two rows the caller's column
/// spaces itself.
fn whats_new<'a>(
    notes: &'a (Version, String),
    tokens: &'a ThemeTokens,
) -> [Element<'a, Message>; 2] {
    let (version, body) = notes;
    [
        section_label(&format!("What's new in {version}"), tokens),
        text(body.as_str())
            .size(TEXT_ROW)
            .color(tokens.text_secondary)
            .into(),
    ]
}

/// Which release the screen is talking about. `Failed` carries no manifest, so it
/// falls back to the notes of the download that got that far.
fn target_version(view: UpdateView<'_>) -> Option<Version> {
    match view.state {
        UpdateState::Downloading { version, .. } => Some(*version),
        UpdateState::Ready { ready, .. } => Some(ready.manifest.version),
        _ => view.notes.map(|(version, _)| *version),
    }
}

fn percent(received: u64, total: u64) -> u32 {
    (progress_fraction(received, total) * 100.0).round() as u32
}

/// How long ago something happened, in the coarsest unit that still says it.
fn ago(at: Instant) -> String {
    let secs = at.elapsed().as_secs();
    if secs < 60 {
        "just now".to_owned()
    } else if secs < 3600 {
        format!("{} min ago", secs / 60)
    } else {
        format!("{} h ago", secs / 3600)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_fresh_check_reads_as_just_now() {
        assert_eq!(ago(Instant::now()), "just now");
    }

    #[test]
    fn minutes_and_hours_each_get_their_own_unit() {
        let at = Instant::now() - Duration::from_secs(90);
        assert_eq!(ago(at), "1 min ago");

        let at = Instant::now() - Duration::from_secs(59 * 60);
        assert_eq!(ago(at), "59 min ago");

        let at = Instant::now() - Duration::from_secs(2 * 3600 + 600);
        assert_eq!(ago(at), "2 h ago");
    }

    /// The bar's own clamp, read through the percentage the views print.
    #[test]
    fn a_percentage_never_passes_a_hundred() {
        assert_eq!(percent(0, 0), 0);
        assert_eq!(percent(50, 200), 25);
        assert_eq!(percent(400, 200), 100);
    }
}
