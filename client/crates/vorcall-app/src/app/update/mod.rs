//! Dispatch: one module per area, each matching its own message enum
//! exhaustively.
//!
//! The areas marked below are stubs that later packets fill in. They still match
//! every variant, so a packet that forgets one does not compile.

pub mod admin;
pub mod auth;
pub mod channels;
pub mod chat;
pub mod crop;
pub mod drag;
pub mod events;
pub mod keys;
pub mod settings;
pub mod share;
pub mod ui;
pub mod voice;

use std::sync::Arc;
use std::time::Instant;

use futures::Stream;
use iced::Task;
use vorcall_core::update::{self, Checker, Outcome, Progress, Version};

use crate::app::message::{Message, TickMsg, UpdateMsg};
use crate::app::state::update::UpdateState;
use crate::app::{App, PROGRESS_STEP, RESTART_GRACE};

pub fn update(app: &mut App, message: Message) -> Task<Message> {
    match message {
        Message::Auth(message) => auth::update(app, message),
        Message::Conn(event) => events::update(app, event),
        Message::Chat(message) => chat::update(app, message),
        Message::Channels(message) => channels::update(app, message),
        Message::Voice(message) => voice::update(app, message),
        Message::Share(message) => share::update(app, message),
        Message::Settings(message) => settings::update(app, message),
        Message::Admin(message) => admin::update(app, message),
        Message::Crop(message) => crop::update(app, message),
        Message::Ui(message) => ui::update(app, message),
        Message::Keys(message) => keys::update(app, message),
        Message::Update(message) => updates(app, message),
        Message::Window(message) => app.window_message(message),
        Message::Tick(message) => ticks(app, message),
        Message::Noop => Task::none(),
    }
}

/// The self-updater's messages.
fn updates(app: &mut App, message: UpdateMsg) -> Task<Message> {
    match message {
        UpdateMsg::Check | UpdateMsg::Tick => check(app),
        UpdateMsg::Progress(progress) => on_progress(app, progress),
        UpdateMsg::Result(result) => on_result(app, result),
        UpdateMsg::Restart => restart_for_update(app),
        UpdateMsg::Apply => apply_update(app),
        // "Later" only puts the banner away; the file stays on disk and the next
        // check brings the banner back with it.
        UpdateMsg::Dismiss => {
            if let UpdateState::Ready { dismissed, .. } = &mut app.update {
                *dismissed = true;
            }
            Task::none()
        }
    }
}

/// Starts one check, unless one is already going or this build does not update
/// itself. The token is a clone taken now: a 401 comes back as a failure and the
/// next tick tries again with whatever the loop rotated to.
pub fn check(app: &mut App) -> Task<Message> {
    if app.update.busy() {
        return Task::none();
    }
    let Some(session) = &app.session else {
        return Task::none();
    };

    let token = session.access_token.clone();
    let checker = Checker {
        endpoints: app.endpoints.clone(),
        keys: app.update_keys.clone(),
        current: Version::current(),
        platform: update::platform(),
    };
    app.update = UpdateState::Checking;
    Task::run(check_stream(checker, token), std::convert::identity)
}

fn on_progress(app: &mut App, progress: Progress) -> Task<Message> {
    // Only what a live check reports: a message that arrives late must not undo a
    // restart already on its way.
    if !matches!(
        app.update,
        UpdateState::Checking | UpdateState::Downloading { .. }
    ) {
        return Task::none();
    }

    app.update = match progress {
        Progress::Checking => UpdateState::Checking,
        Progress::Downloading {
            version,
            received,
            total,
            required,
        } => {
            // `Failed` and `Restarting` carry no manifest of their own; this is
            // what they fall back on.
            app.force_required = required;
            UpdateState::Downloading {
                version,
                received,
                total,
                required,
            }
        }
    };
    Task::none()
}

fn on_result(app: &mut App, result: Result<Arc<Outcome>, String>) -> Task<Message> {
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(message) => {
            tracing::warn!(%message, "the update check failed");
            app.update = UpdateState::Failed {
                message,
                required: app.force_required,
                at: Instant::now(),
            };
            return Task::none();
        }
    };

    match &*outcome {
        Outcome::UpToDate { .. } => {
            app.force_required = false;
            app.update = UpdateState::UpToDate { at: Instant::now() };
            Task::none()
        }
        Outcome::NoBuildForPlatform { .. } => {
            app.force_required = false;
            app.update = UpdateState::NoBuild {
                platform: update::platform(),
            };
            Task::none()
        }
        Outcome::Downloaded(ready) => {
            let ready = ready.clone();
            app.force_required = ready.required;
            if !ready.manifest.notes.trim().is_empty() {
                app.update_notes = Some((ready.manifest.version, ready.manifest.notes.clone()));
            }

            // A download that landed outside the install directory is the one
            // case a required update still waits for a person.
            let restart_now = ready.required && !ready.manual;
            app.update = UpdateState::Ready {
                ready,
                dismissed: false,
            };
            if restart_now {
                return restart_for_update(app);
            }
            Task::none()
        }
    }
}

/// Leaves the voice channel and closes the media engine before the binary is
/// replaced: the swap ends this process without another chance to say so.
fn restart_for_update(app: &mut App) -> Task<Message> {
    let UpdateState::Ready { ready, .. } = &app.update else {
        return Task::none();
    };
    if ready.manual {
        return Task::none();
    }

    app.pending_restart = Some(ready.clone());
    app.update = UpdateState::Restarting;

    let closing = app.close_voice();
    let swap = Task::perform(tokio::time::sleep(RESTART_GRACE), |()| {
        Message::Update(UpdateMsg::Apply)
    });
    closing.chain(swap)
}

fn apply_update(app: &mut App) -> Task<Message> {
    // The swap only ever follows the restart above, which is what left the voice
    // channel.
    if !matches!(app.update, UpdateState::Restarting) {
        return Task::none();
    }
    let Some(ready) = app.pending_restart.take() else {
        return Task::none();
    };

    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match update::swap::apply_and_relaunch(&ready.file, &args) {
        // On unix the process image is already gone by now; this is Windows,
        // where the replacement is up and this one is in its way.
        Ok(update::swap::Relaunched::Spawned) => iced::exit(),
        Err(e) => {
            tracing::warn!(error = %e, "the update was not applied");
            app.update = UpdateState::Failed {
                message: format!("restart by hand: {e}"),
                required: app.force_required,
                at: Instant::now(),
            };
            Task::none()
        }
    }
}

/// One check as a stream of messages: what the core reports on the way, then the
/// outcome. [`Task::run`] turns it into the task [`check`] hands back.
fn check_stream(checker: Checker, token: String) -> impl Stream<Item = Message> {
    iced::stream::channel(16, async move |mut output| {
        let mut throttle = ProgressThrottle::default();
        let mut reports = output.clone();

        let result = update::check_and_download(&checker, &token, |progress| {
            if !throttle.admit(&progress) {
                return;
            }
            // A full queue only means the window is behind on a number the next
            // report replaces anyway.
            let _ = reports.try_send(Message::Update(UpdateMsg::Progress(progress)));
        })
        .await;

        let _ = futures::SinkExt::send(
            &mut output,
            Message::Update(UpdateMsg::Result(
                result.map(Arc::new).map_err(|e| e.to_string()),
            )),
        )
        .await;
    })
}

/// The downloader reports every chunk it writes and every report redraws the
/// window, so only a step worth looking at is passed on: one percent of the
/// release, at most [`PROGRESS_STEP`], and the last chunk whatever its size.
#[derive(Default)]
struct ProgressThrottle {
    last: Option<u64>,
}

impl ProgressThrottle {
    fn admit(&mut self, progress: &Progress) -> bool {
        let Progress::Downloading {
            received, total, ..
        } = progress
        else {
            self.last = None;
            return true;
        };
        let (received, total) = (*received, *total);

        let step = (total / 100).clamp(1, PROGRESS_STEP);
        let done = total > 0 && received >= total;
        if let Some(last) = self.last
            && !done
            && received.saturating_sub(last) < step
        {
            return false;
        }

        self.last = Some(received);
        true
    }
}

/// The clocks. Each one only fires while its own subscription is alive.
fn ticks(app: &mut App, message: TickMsg) -> Task<Message> {
    match message {
        TickMsg::Voice => voice::tick(app),
        TickMsg::MarkRead => chat::flush_mark_read(app),
        TickMsg::Toasts => {
            ui::expire_toasts(app);
            Task::none()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(received: u64, total: u64) -> Progress {
        Progress::Downloading {
            version: "0.5.0".parse().expect("a version"),
            received,
            total,
            required: false,
        }
    }

    #[test]
    fn the_first_report_of_a_download_always_passes() {
        let mut throttle = ProgressThrottle::default();

        assert!(throttle.admit(&Progress::Checking));
        assert!(throttle.admit(&report(0, 10_000)));
    }

    /// One percent of 10 000 bytes is 100 of them.
    #[test]
    fn a_step_under_one_percent_is_dropped() {
        let mut throttle = ProgressThrottle::default();
        assert!(throttle.admit(&report(0, 10_000)));

        assert!(!throttle.admit(&report(50, 10_000)));
        assert!(!throttle.admit(&report(99, 10_000)));
        assert!(throttle.admit(&report(100, 10_000)));
        assert!(!throttle.admit(&report(150, 10_000)));
    }

    /// One percent of ten gibibytes is far more than a mebibyte, and a mebibyte
    /// is already worth redrawing.
    #[test]
    fn a_huge_release_steps_by_a_mebibyte() {
        const TOTAL: u64 = 10 * 1024 * 1024 * 1024;
        let mut throttle = ProgressThrottle::default();
        assert!(throttle.admit(&report(0, TOTAL)));

        assert!(!throttle.admit(&report(PROGRESS_STEP - 1, TOTAL)));
        assert!(throttle.admit(&report(PROGRESS_STEP, TOTAL)));
    }

    #[test]
    fn the_last_chunk_always_passes() {
        let mut throttle = ProgressThrottle::default();
        assert!(throttle.admit(&report(0, 10_000)));
        assert!(throttle.admit(&report(9_999, 10_000)));

        assert!(throttle.admit(&report(10_000, 10_000)));
    }

    /// A second check starts over: its first report is not measured against what
    /// the last download had reached.
    #[test]
    fn checking_again_forgets_the_last_download() {
        let mut throttle = ProgressThrottle::default();
        assert!(throttle.admit(&report(0, 10_000)));
        assert!(throttle.admit(&report(5_000, 10_000)));

        assert!(throttle.admit(&Progress::Checking));
        assert!(throttle.admit(&report(0, 10_000)));
    }
}
