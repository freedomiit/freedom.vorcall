//! What the self-updater puts on screen: the banner under the header, the
//! screen a required update takes over, and the settings section.
//!
//! A pure read of [`crate::app::UpdateState`], with one exception: the settings
//! line says how long ago a check ended, which it reads off the clock rather
//! than keeping a timer alive for it.

use std::time::{Duration, Instant};

use iced::alignment::{Horizontal, Vertical};
use iced::widget::{Text, button, column, container, progress_bar, row, text};
use iced::{Color, Element, Length, Theme};
use vorcall_core::update::{self, Ready, Version};

use crate::app::{Message, UpdateState, progress_fraction};
use crate::brand::loading;
use crate::brand::palette::{DANGER, DEEP, INK, MUTED, SUCCESS};
use crate::view::bold;

/// The creature beside a line of text, beside the settings line, and the one
/// that owns the required screen.
const INLINE_CREATURE: f32 = 24.0;
const SETTINGS_CREATURE: f32 = 20.0;
const REQUIRED_CREATURE: f32 = 128.0;
const BAR_LENGTH: f32 = 180.0;
const BAR_GIRTH: f32 = 8.0;

/// Everything the three views below read. Copied whole because the chat screen
/// hands the same thing to the banner and to the settings page.
#[derive(Clone, Copy)]
pub struct UpdateView<'a> {
    pub state: &'a UpdateState,
    pub notes: Option<&'a (Version, String)>,
    pub elapsed: Duration,
}

/// The strip under the header, or nothing at all: most states have no news
/// worth a permanent line above the room.
pub fn banner(view: UpdateView<'_>) -> Option<Element<'_, Message>> {
    let content: Element<'_, Message> = match view.state {
        UpdateState::Downloading {
            version,
            received,
            total,
            ..
        } => row![
            loading::view(view.elapsed, INLINE_CREATURE),
            text(format!("Downloading Vorcall {version}…")),
            progress_bar(0.0..=1.0, progress_fraction(*received, *total))
                .length(BAR_LENGTH)
                .girth(BAR_GIRTH),
            text(format!("{}%", percent(*received, *total))).color(MUTED),
        ]
        .spacing(12)
        .align_y(Vertical::Center)
        .into(),
        UpdateState::Ready {
            ready,
            dismissed: false,
        } if ready.manual => manual_notice(ready, false).into(),
        UpdateState::Ready {
            ready,
            dismissed: false,
        } => row![
            text(format!("Vorcall {} is ready.", ready.manifest.version)),
            button(text("Restart now")).on_press(Message::RestartForUpdate),
            button(text("Later")).on_press(Message::DismissUpdate),
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
            .style(banner_style)
            .into(),
    )
}

/// The whole window while an update the server insists on is on its way. The
/// connection underneath is left alone: the chat state is untouched, so a
/// restart that fails goes back to a room that is still there.
pub fn required(view: UpdateView<'_>) -> Element<'_, Message> {
    let mut content = column![
        loading::view(view.elapsed, REQUIRED_CREATURE),
        text("Update required").size(24).font(bold()),
    ]
    .spacing(16)
    .align_x(Horizontal::Center);

    if let Some(version) = target_version(view) {
        content = content.push(text(format!(
            "Vorcall {version} is needed to keep chatting."
        )));
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
                .push(text(format!("{}%", percent(*received, *total))).color(MUTED));
        }
        UpdateState::Ready { ready, .. } if ready.manual => {
            content = content.push(manual_notice(ready, true));
        }
        // Whatever produced this `Ready` already asked for the restart.
        UpdateState::Ready { .. } | UpdateState::Restarting => {
            content = content.push(text("Restarting…").color(MUTED));
        }
        UpdateState::Failed { message, .. } => {
            content = content
                .push(text(message.as_str()).color(DANGER))
                .push(button(text("Retry")).on_press(Message::CheckForUpdates));
        }
        _ => {}
    }

    if let Some(notes) = view.notes {
        content = content.extend(whats_new(notes));
    }

    container(content).center(Length::Fill).padding(24).into()
}

/// The version line, the manual check and whatever the last one found.
pub fn section(view: UpdateView<'_>) -> Element<'_, Message> {
    let check = button(text("Check for updates"))
        .on_press_maybe((!view.state.busy()).then_some(Message::CheckForUpdates));

    let mut content = column![
        text(format!(
            "Version {} ({})",
            Version::current(),
            update::platform()
        )),
        row![check, state_line(view)]
            .spacing(12)
            .align_y(Vertical::Center),
    ]
    .spacing(12);

    if let Some(notes) = view.notes {
        content = content.extend(whats_new(notes));
    }
    content.into()
}

fn state_line(view: UpdateView<'_>) -> Element<'_, Message> {
    match view.state {
        UpdateState::Disabled(reason) => text(format!("Updates are off: {reason}"))
            .color(MUTED)
            .into(),
        UpdateState::Idle => text("").into(),
        UpdateState::Checking => row![
            loading::view(view.elapsed, SETTINGS_CREATURE),
            text("Checking…").color(MUTED),
        ]
        .spacing(8)
        .align_y(Vertical::Center)
        .into(),
        UpdateState::UpToDate { at } => text(format!("Up to date · checked {}", ago(*at)))
            .color(SUCCESS)
            .into(),
        UpdateState::NoBuild { platform } => {
            text(format!("No build for {platform} in this release"))
                .color(MUTED)
                .into()
        }
        UpdateState::Downloading {
            received, total, ..
        } => text(format!("Downloading… {}%", percent(*received, *total)))
            .color(MUTED)
            .into(),
        UpdateState::Ready { ready, .. } if ready.manual => {
            manual_notice(ready, false).color(MUTED).into()
        }
        UpdateState::Ready { ready, .. } => row![
            text(format!("Vorcall {} is ready", ready.manifest.version)),
            button(text("Restart now")).on_press(Message::RestartForUpdate),
        ]
        .spacing(12)
        .align_y(Vertical::Center)
        .into(),
        UpdateState::Failed { message, at, .. } => {
            text(format!("Update check failed: {message} · {}", ago(*at)))
                .color(DANGER)
                .into()
        }
        UpdateState::Restarting => text("Restarting…").color(MUTED).into(),
    }
}

/// A download that landed outside the install directory. `and_restart` is for
/// the required screen, the one place the app is waiting on the person rather
/// than the other way round.
fn manual_notice<'a>(ready: &Ready, and_restart: bool) -> Text<'a> {
    let tail = if and_restart {
        " and start Vorcall again."
    } else {
        "."
    };
    text(format!(
        "Update downloaded to {}. Install it by hand{tail}",
        ready.file.display()
    ))
}

/// The release notes under their heading, as the two rows the caller's column
/// spaces itself.
fn whats_new(notes: &(Version, String)) -> [Element<'_, Message>; 2] {
    let (version, body) = notes;
    [
        text(format!("What's new in {version}"))
            .size(16)
            .font(bold())
            .into(),
        text(body.as_str()).color(MUTED).into(),
    ]
}

/// Which release the screen is talking about. `Failed` carries no manifest, so
/// it falls back to the notes of the download that got that far.
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

/// Enough of the brand red to read as part of the header rather than as
/// something said in the room.
fn banner_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Color { a: 0.15, ..DEEP }.into()),
        text_color: Some(INK),
        ..container::Style::default()
    }
}
