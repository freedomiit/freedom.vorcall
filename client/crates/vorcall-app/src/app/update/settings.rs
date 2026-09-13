//! The user settings pages: the routes, the preferences each page writes, the
//! profile draft and the theme editor.
//!
//! Every preference is written to disk the moment it changes — a setting that is
//! not in `config.toml` is a setting that did not happen — and the ones that are
//! visible repaint the window through [`App::reload_theme`]. The file dialog, the
//! file reads, the theme JSON and the problem report all run off the UI thread;
//! a picked picture goes to the crop adjuster, which owns the decode and the
//! scaling.

use std::path::{Path, PathBuf};

use iced::Task;
use vorcall_core::connection::{AdminCommand, Blob, Command};
use vorcall_core::images::{self, ImagePurpose};
use vorcall_core::{Endpoints, Image, Profile, attachments, config, diagnostics, report};

use crate::app::message::{
    AdminMsg, ChannelsMsg, CropMsg, Message, SettingsMsg, ToastKind, UiMsg, VoiceMsg,
};
use crate::app::state::rules::{IMAGE_TOO_LARGE, NOT_AN_IMAGE, describe};
use crate::app::state::settings::{
    OverviewDraft, ProfileDraft, ReportState, ServerTab, SettingsTab, ThemeDraft, ThemeEntry,
    image_sentinel,
};
use crate::app::state::ui::{Dialog, Route};
use crate::app::{App, MainState, Screen};
use crate::theme::{self, ThemeTokens};
use crate::workers::images::ImageKey;
use crate::workers::voice;

/// A cancelled file dialog answers with this rather than a complaint: nothing was
/// picked, which is not a failure anybody needs to be told about.
pub const CANCELLED: &str = "";

/// What the pickers and the import dialog offer.
const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

/// The limits of `proto/vorcall.proto` § `Profile`, applied as the field is typed
/// so the server never has to refuse one.
const NICKNAME_MAX: usize = 32;
const DESCRIPTION_MAX: usize = 256;

pub fn update(app: &mut App, message: SettingsMsg) -> Task<Message> {
    match message {
        SettingsMsg::Open(tab) | SettingsMsg::Tab(tab) => {
            app.ui.route = Route::Settings(tab);
            open_page(app, tab)
        }
        SettingsMsg::ServerTab(tab) => {
            app.ui.route = Route::ServerSettings(tab);
            // The two pages that read their rows over REST ask for them here:
            // opening the page is the only thing that refreshes a list.
            match tab {
                // The form starts from the server as it stands, so an abandoned
                // edit is not what the page comes back to.
                ServerTab::Overview => {
                    if let Some(main) = app.main_mut() {
                        main.admin.overview = OverviewDraft::from_server(&main.server.server);
                    }
                    Task::none()
                }
                ServerTab::Invites => Task::done(Message::Admin(AdminMsg::InvitesRefresh)),
                ServerTab::Bans => Task::done(Message::Admin(AdminMsg::BansRefresh)),
                _ => Task::none(),
            }
        }
        SettingsMsg::Close => {
            app.ui.route = Route::Main;
            Task::none()
        }

        SettingsMsg::SetNotifications(on) => {
            app.config.notifications = on;
            app.save_config();
            Task::none()
        }
        SettingsMsg::SetSound(on) => {
            app.config.sound = on;
            app.save_config();
            Task::none()
        }
        SettingsMsg::SetSuppressEveryone(on) => {
            app.config.suppress_everyone = on;
            app.save_config();
            Task::none()
        }

        SettingsMsg::SetTheme(name) => {
            app.config.theme = name;
            app.save_config();
            app.reload_theme();
            seed_theme_draft(app);
            Task::none()
        }
        // Neither of the two reaches for the theme: `ThemeTokens::iced_theme` is
        // built from the colour tokens alone, and resolving a custom theme reads
        // and parses its file — not something a slider step may do on the UI
        // thread.
        SettingsMsg::SetDensity(density) => {
            app.config.density = density;
            app.save_config();
            Task::none()
        }
        // Dragging the slider is live; the end of the drag is what reaches the
        // disk, so a drag is not a write per frame.
        SettingsMsg::SetFontScale(scale) => {
            app.config.font_scale = scale.clamp(config::FONT_SCALE_MIN, config::FONT_SCALE_MAX);
            Task::none()
        }
        SettingsMsg::FontScaleReleased => {
            app.save_config();
            Task::none()
        }
        SettingsMsg::SetEntrance(entrance) => {
            app.config.entrance = entrance;
            app.save_config();
            Task::none()
        }
        SettingsMsg::SetTextReactions(on) => {
            app.config.text_reactions = on;
            app.save_config();
            Task::none()
        }

        SettingsMsg::ThemeEditorToken(token, typed) => {
            if let Some(main) = app.main_mut() {
                main.settings.theme.set_token(&token, typed);
            }
            Task::none()
        }
        SettingsMsg::ThemeEditorName(name) => {
            // The same field serves the editor and the Save as prompt.
            if let Some(Dialog::ThemeSaveAs { name: typed }) = app.dialog_mut() {
                *typed = name;
                return Task::none();
            }
            if let Some(main) = app.main_mut() {
                main.settings.theme.name = name;
            }
            Task::none()
        }
        SettingsMsg::ThemeSave => {
            let Some((slug, tokens, error)) = app.main().map(|main| {
                let draft = &main.settings.theme;
                (draft.slug(), draft.tokens, draft.error.clone())
            }) else {
                return Task::none();
            };
            if let Some(error) = error {
                app.toast(ToastKind::Error, error);
                return Task::none();
            }
            write_theme(slug, tokens)
        }
        SettingsMsg::ThemeSaveAs => {
            let name = app
                .main()
                .map(|main| main.settings.theme.name.clone())
                .unwrap_or_default();
            app.ui.dialog = Some(Dialog::ThemeSaveAs { name });
            Task::none()
        }
        SettingsMsg::ThemeSaveAsConfirm => {
            let Some(Dialog::ThemeSaveAs { name }) = app.ui.dialog.take() else {
                return Task::none();
            };
            let Some(main) = app.main_mut() else {
                return Task::none();
            };
            main.settings.theme.name = name;
            let draft = &main.settings.theme;
            write_theme(theme::file::slugify(&draft.name), draft.tokens)
        }
        SettingsMsg::ThemeExport => match app.main() {
            Some(main) => export_theme(main.settings.theme.slug(), main.settings.theme.tokens),
            None => Task::none(),
        },
        SettingsMsg::ThemeImport => import_theme(),
        SettingsMsg::ThemeFileResult(result) => match result {
            Ok(slug) => {
                if let Some(main) = app.main_mut() {
                    main.settings.theme.base = theme::file::custom_name(&slug);
                }
                app.config.theme = theme::file::custom_name(&slug);
                app.save_config();
                app.reload_theme();
                refresh_themes(app);
                let name = theme::file::display_name(&slug);
                app.toast(ToastKind::Info, format!("{name} is in use"));
                Task::none()
            }
            Err(error) if error == CANCELLED => Task::none(),
            Err(error) => {
                app.toast(ToastKind::Error, error);
                Task::none()
            }
        },

        SettingsMsg::ProfileNickname(nickname) => {
            if let Some(main) = app.main_mut() {
                main.settings.profile.nickname = truncate(nickname, NICKNAME_MAX);
            }
            Task::none()
        }
        SettingsMsg::ProfileDescription(description) => {
            if let Some(main) = app.main_mut() {
                main.settings.profile.description = truncate(description, DESCRIPTION_MAX);
            }
            Task::none()
        }
        SettingsMsg::ProfileAccent(rgb) => {
            if let Some(main) = app.main_mut() {
                main.settings.profile.accent_color = rgb;
            }
            Task::none()
        }
        SettingsMsg::ProfilePickAvatar => pick_image(ImagePurpose::Avatar),
        SettingsMsg::ProfilePickBanner => pick_image(ImagePurpose::Banner),
        // Nothing is uploaded until the adjuster has framed it.
        SettingsMsg::ProfileImagePicked(purpose, Ok((_, bytes))) => {
            Task::done(Message::Crop(CropMsg::Open { purpose, bytes }))
        }
        SettingsMsg::ProfileImagePicked(purpose, Err(error)) => {
            if error == CANCELLED {
                return Task::none();
            }
            // Whatever was in flight for this purpose never lands, so its answer
            // must not be waited on.
            if let Some(main) = app.main_mut() {
                main.settings
                    .pending
                    .retain(|_, pending| *pending != purpose);
            }
            app.toast(ToastKind::Error, error);
            Task::none()
        }
        SettingsMsg::ProfileClearAvatar => {
            if let Some(main) = app.main_mut() {
                main.settings.profile.avatar_image_id = 0;
            }
            Task::none()
        }
        SettingsMsg::ProfileClearBanner => {
            if let Some(main) = app.main_mut() {
                main.settings.profile.banner_image_id = 0;
            }
            Task::none()
        }
        SettingsMsg::ProfileSave => save_profile(app),
        SettingsMsg::ProfileReset => {
            if let Some(main) = app.main_mut() {
                main.settings.profile = my_draft(main);
            }
            Task::none()
        }

        SettingsMsg::KeybindCapture(action) => {
            if let Some(main) = app.main_mut() {
                main.settings.capturing = Some(action);
            }
            Task::none()
        }
        SettingsMsg::KeybindCaptured(action, binding) => {
            if let Some(main) = app.main_mut() {
                main.settings.capturing = None;
            }
            rebind(app, &action, &binding)
        }
        SettingsMsg::KeybindReset(action) => {
            if let Some(main) = app.main_mut() {
                main.settings.capturing = None;
            }
            // An empty binding is how the configuration spells "the default".
            rebind(app, &action, "")
        }
        SettingsMsg::KeybindCancel => {
            if let Some(main) = app.main_mut() {
                main.settings.capturing = None;
            }
            Task::none()
        }

        SettingsMsg::ReportProblem => report_problem(app),
        SettingsMsg::ReportFinished(result) => on_report_finished(app, result),
        SettingsMsg::SendCrashReport => {
            close_crash_offer(app);
            report_problem(app)
        }
        SettingsMsg::DismissCrashReport => {
            // The files stay on disk: the diagnostics section can still send them.
            close_crash_offer(app);
            Task::none()
        }
    }
}

/// What opening one page asks for: the devices the voice page lists, and the
/// drafts the profile and appearance pages edit.
fn open_page(app: &mut App, tab: SettingsTab) -> Task<Message> {
    match tab {
        SettingsTab::Voice => {
            Task::perform(tokio::task::spawn_blocking(voice::list_devices), |joined| {
                Message::Voice(VoiceMsg::DevicesListed(joined.unwrap_or_default()))
            })
        }
        SettingsTab::Profile => {
            // A draft that has never been filled in is seeded from the server;
            // one being edited is left alone, so leaving the page and coming back
            // does not throw the work away.
            let Some(main) = app.main_mut() else {
                return Task::none();
            };
            if main.settings.profile == ProfileDraft::default() {
                main.settings.profile = my_draft(main);
            }
            let pictures = [
                main.settings.profile.avatar_image_id,
                main.settings.profile.banner_image_id,
            ];
            app.ensure_images(
                pictures
                    .into_iter()
                    .filter(|id| *id != 0)
                    .map(ImageKey::Image)
                    .collect(),
            )
        }
        SettingsTab::Appearance => {
            seed_theme_draft(app);
            refresh_themes(app);
            Task::none()
        }
        SettingsTab::Account | SettingsTab::Notifications | SettingsTab::Keybinds => Task::none(),
    }
}

/// Points the editor at the theme in force, unless it is already on it: reopening
/// the page must not undo what is half typed into it.
fn seed_theme_draft(app: &mut App) {
    let base = app.config.theme.clone();
    let name = theme_name(&base);
    let tokens = app.tokens;
    if let Some(main) = app.main_mut()
        && main.settings.theme.base != base
    {
        main.settings.theme = ThemeDraft::new(base, name, tokens);
    }
}

/// Re-reads the themes the page lists. The files are small and this runs when the
/// page opens or a theme is written, never per frame.
fn refresh_themes(app: &mut App) {
    let entries = theme_entries();
    if let Some(main) = app.main_mut() {
        main.settings.themes = entries;
    }
}

/// The two presets, then every custom theme that still reads as one.
fn theme_entries() -> Vec<ThemeEntry> {
    let mut entries: Vec<ThemeEntry> = theme::presets::PRESETS
        .iter()
        .map(|(slug, label)| ThemeEntry {
            theme: (*slug).to_owned(),
            name: (*label).to_owned(),
            detail: "preset".to_owned(),
            tokens: theme::presets::by_name(slug)
                .copied()
                .unwrap_or(theme::VORCALL_DARK),
        })
        .collect();

    for (slug, name) in theme::file::list() {
        match theme::file::load(&slug) {
            Ok(tokens) => entries.push(ThemeEntry {
                theme: theme::file::custom_name(&slug),
                name,
                detail: slug,
                tokens,
            }),
            // A file that will not parse is left out rather than offered.
            Err(e) => tracing::debug!(slug, error = %format!("{e:#}"), "skipping a theme"),
        }
    }
    entries
}

/// What a theme is called, whichever of the three forms `Config::theme` takes.
pub fn theme_name(theme: &str) -> String {
    if let Some(label) = theme::presets::label(theme) {
        return label.to_owned();
    }
    match theme.strip_prefix(theme::file::CUSTOM_PREFIX) {
        Some(slug) => theme::file::display_name(slug),
        None => theme::file::display_name(theme),
    }
}

/// The caller's own profile as a draft; every field empty when the snapshot has
/// not landed yet.
fn my_draft(main: &MainState) -> ProfileDraft {
    main.server
        .members
        .get(&main.member_id)
        .map(ProfileDraft::from_profile)
        .unwrap_or_default()
}

/// Keeps a field inside the limit the wire allows, counted in characters rather
/// than bytes: the server counts scalars too.
fn truncate(mut value: String, max: usize) -> String {
    if value.chars().count() <= max {
        return value;
    }
    let end = value
        .char_indices()
        .nth(max)
        .map_or(value.len(), |(at, _)| at);
    value.truncate(end);
    value
}

/// Sends the draft. The nickname is a frame of its own — it is the one field
/// whose own permission the server checks.
fn save_profile(app: &mut App) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    let current: Profile = main
        .server
        .members
        .get(&main.member_id)
        .cloned()
        .unwrap_or_default();
    let draft = main.settings.profile.clone();
    if !draft.differs_from(&current) {
        return Task::none();
    }

    main.send_or_notice(Command::Admin(AdminCommand::UpdateProfile {
        description: draft.description.clone(),
        accent_color: draft.accent_color,
        avatar_image_id: image_sentinel(draft.avatar_image_id, current.avatar_image_id),
        banner_image_id: image_sentinel(draft.banner_image_id, current.banner_image_id),
    }));
    if draft.nickname != current.nickname {
        main.send_or_notice(Command::Admin(AdminCommand::SetNickname {
            user_id: 0,
            nickname: draft.nickname,
        }));
    }
    Task::none()
}

/// Rebinds one action: the conflicts are drawn rather than refused, and the three
/// global actions need the listener restarted on the new binding.
fn rebind(app: &mut App, action: &str, binding: &str) -> Task<Message> {
    app.config.set_keybind(action, binding);
    app.save_config();
    if is_global(action) {
        return Task::done(Message::Voice(VoiceMsg::RetryHotkey));
    }
    Task::none()
}

/// Whether an action's binding is captured system-wide.
pub fn is_global(action: &str) -> bool {
    config::KEYBIND_ACTIONS
        .iter()
        .any(|(id, _, global)| *id == action && *global)
}

/// One image off the disk, picked and read without ever touching the UI thread.
fn pick_image(purpose: ImagePurpose) -> Task<Message> {
    Task::perform(read_picked_image(), move |result| {
        Message::Settings(SettingsMsg::ProfileImagePicked(purpose, result))
    })
}

async fn read_picked_image() -> Result<(String, Blob), String> {
    let Some(handle) = rfd::AsyncFileDialog::new()
        .add_filter("Images", &IMAGE_EXTENSIONS)
        .pick_file()
        .await
    else {
        return Err(CANCELLED.to_owned());
    };

    let path = handle.path().to_path_buf();
    let read = tokio::task::spawn_blocking(move || read_file(&path))
        .await
        .map_err(|e| e.to_string())?;
    read.map(|(name, bytes)| (name, Blob::from(bytes)))
}

fn read_file(path: &Path) -> Result<(String, Vec<u8>), String> {
    let name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("image")
        .to_owned();
    // Asked before the read: nothing the server would refuse belongs in memory,
    // and a picked file is only bounded by the disk it came off.
    let size = std::fs::metadata(path)
        .map_err(|e| format!("cannot read that file: {e}"))?
        .len();
    if size > images::MAX_BYTES {
        return Err(IMAGE_TOO_LARGE.to_owned());
    }

    let bytes = std::fs::read(path).map_err(|e| format!("cannot read that file: {e}"))?;
    // The dialog's extension filter is advisory: a renamed file passes it, and
    // the magic number is what the server judges these bytes by.
    if attachments::sniff_image(&bytes).is_none() {
        return Err(NOT_AN_IMAGE.to_owned());
    }
    Ok((name, bytes))
}

/// Uploads a picture the crop adjuster has already framed and scaled. The
/// adjuster is the only place either happens now, so the bytes go up as they
/// came out of it.
pub fn upload_cropped(
    app: &mut App,
    purpose: ImagePurpose,
    content_type: &'static str,
    bytes: Blob,
) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };

    // A second pick supersedes the first: an answer for the older one would put
    // the wrong image in the draft.
    main.settings
        .pending
        .retain(|_, pending| *pending != purpose);
    let request_id = main.next_request_id();
    if main.send_command(Command::UploadImage {
        request_id,
        purpose,
        content_type: content_type.to_owned(),
        bytes,
    }) {
        main.settings.pending.insert(request_id, purpose);
        return Task::none();
    }

    app.toast(ToastKind::Error, "Not connected".to_owned());
    Task::none()
}

/// One upload this page asked for landed: `None` when the id belongs to somebody
/// else's page.
pub fn on_image_uploaded(app: &mut App, request_id: u64, image: &Image) -> Option<Task<Message>> {
    let main = app.main_mut()?;
    let purpose = main.settings.pending.remove(&request_id)?;
    match purpose {
        ImagePurpose::Avatar => main.settings.profile.avatar_image_id = image.id,
        ImagePurpose::Banner => main.settings.profile.banner_image_id = image.id,
        // Only the two profile images are ever asked for here.
        ImagePurpose::ServerIcon | ImagePurpose::RoleIcon => return Some(Task::none()),
    }
    // The preview draws it straight away, which means fetching the bytes back.
    Some(app.ensure_image(ImageKey::Image(image.id)))
}

pub fn on_image_upload_failed(
    app: &mut App,
    request_id: u64,
    error: &str,
) -> Option<Task<Message>> {
    let main = app.main_mut()?;
    main.settings.pending.remove(&request_id)?;
    app.toast(ToastKind::Error, error.to_owned());
    Some(Task::none())
}

/// Writes one custom theme and selects it. The answer goes through
/// [`SettingsMsg::ThemeFileResult`], the one place a theme is put in force, so
/// the window never repaints from a file that is still being written.
fn write_theme(slug: String, tokens: ThemeTokens) -> Task<Message> {
    Task::perform(
        tokio::task::spawn_blocking(move || {
            theme::file::save(&slug, &tokens)
                .map(|()| slug)
                .map_err(|e| format!("{e:#}"))
        }),
        |joined| {
            Message::Settings(SettingsMsg::ThemeFileResult(
                joined.unwrap_or_else(|e| Err(e.to_string())),
            ))
        },
    )
}

/// The theme as a JSON file anywhere on the disk, for mailing to a friend. The
/// success is a note rather than a selection: the theme in force did not change.
fn export_theme(slug: String, tokens: ThemeTokens) -> Task<Message> {
    Task::perform(
        async move {
            let Some(handle) = rfd::AsyncFileDialog::new()
                .add_filter("Theme", &["json"])
                .set_file_name(format!("{slug}.json"))
                .save_file()
                .await
            else {
                return Err(CANCELLED.to_owned());
            };

            let path = handle.path().to_path_buf();
            tokio::task::spawn_blocking(move || write_json(&path, &tokens))
                .await
                .map_err(|e| e.to_string())?
        },
        |result| match result {
            Ok(path) => Message::Ui(UiMsg::Toast(
                ToastKind::Info,
                format!("Theme written to {path}"),
            )),
            Err(error) => Message::Settings(SettingsMsg::ThemeFileResult(Err(error))),
        },
    )
}

fn write_json(path: &Path, tokens: &ThemeTokens) -> Result<String, String> {
    let body = serde_json::to_string_pretty(tokens).map_err(|e| e.to_string())?;
    std::fs::write(path, body).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path.display().to_string())
}

/// A theme file somebody else wrote: read, checked, saved under the config
/// directory as its own custom theme, and put in force.
fn import_theme() -> Task<Message> {
    Task::perform(
        async {
            let Some(handle) = rfd::AsyncFileDialog::new()
                .add_filter("Theme", &["json"])
                .pick_file()
                .await
            else {
                return Err(CANCELLED.to_owned());
            };

            let path = handle.path().to_path_buf();
            tokio::task::spawn_blocking(move || read_theme(&path))
                .await
                .map_err(|e| e.to_string())?
        },
        |result| Message::Settings(SettingsMsg::ThemeFileResult(result)),
    )
}

fn read_theme(path: &Path) -> Result<String, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("cannot read that file: {e}"))?;
    let tokens: ThemeTokens =
        serde_json::from_str(&raw).map_err(|e| format!("that is not a theme: {e}"))?;
    let slug = theme::file::slugify(
        path.file_stem()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or("theme"),
    );
    theme::file::save(&slug, &tokens).map_err(|e| format!("{e:#}"))?;
    Ok(slug)
}

/// What the notifications page lists, as `(channel id, title)` in the order the
/// titles read. A muted channel the snapshot does not describe is still listed, so
/// it can be unmuted.
pub fn muted_channels(app: &App, main: &MainState) -> Vec<(i64, String)> {
    let mut rows: Vec<(i64, String)> = app
        .config
        .muted_channels
        .iter()
        .map(|id| (*id, main.server.channel_title(*id)))
        .collect();
    rows.sort_by_key(|(_, title)| title.to_lowercase());
    rows
}

/// Unmuting is the channel area's own message: the list here only points at it.
pub fn unmute(channel_id: i64) -> Message {
    Message::Channels(ChannelsMsg::UnmuteChannel(channel_id))
}

/// Opens the offer on entering the shell, never over another dialog, until it is
/// answered; from then on the diagnostics section is the only way to send. Keeping
/// the offer until an answer is what lets it survive a stale stored session, whose
/// shell gives way to sign-in before anybody can act on it.
pub fn offer_crash_report(app: &mut App) {
    if !app.crash_offer || app.session.is_none() || !matches!(app.screen, Screen::Main(_)) {
        return;
    }
    if app.ui.dialog.is_none() {
        app.ui.dialog = Some(Dialog::CrashReport);
    }
}

fn close_crash_offer(app: &mut App) {
    app.crash_offer = false;
    if matches!(app.ui.dialog, Some(Dialog::CrashReport)) {
        app.ui.dialog = None;
    }
}

/// Sends the log files and every crash report, one upload at a time. The token is
/// a clone taken now: a 401 comes back as a failure the reader can press again,
/// the same way an update check does.
fn report_problem(app: &mut App) -> Task<Message> {
    let Some(token) = app
        .session
        .as_ref()
        .map(|session| session.access_token.clone())
    else {
        return Task::none();
    };
    if reporting(app) {
        return Task::none();
    }

    let endpoints = app.endpoints.clone();
    if let Some(main) = app.main_mut() {
        main.settings.report = ReportState::Sending;
    }
    Task::perform(send_report(endpoints, token), |result| {
        Message::Settings(SettingsMsg::ReportFinished(result))
    })
}

/// Whether a report is already on its way. The page itself need not be open for
/// that: the crash offer sends from wherever the window is.
fn reporting(app: &App) -> bool {
    app.main()
        .is_some_and(|main| main.settings.report == ReportState::Sending)
}

fn on_report_finished(app: &mut App, result: Result<usize, String>) -> Task<Message> {
    let (state, kind, note) = report_outcome(result);
    if let Some(main) = app.main_mut() {
        main.settings.report = state;
    }
    app.toast(kind, note);
    Task::none()
}

/// What a finished report leaves behind: the diagnostics section's state, and the
/// toast that says so wherever the reader happens to be.
fn report_outcome(result: Result<usize, String>) -> (ReportState, ToastKind, String) {
    match result {
        Ok(count) => (
            ReportState::Sent(count),
            ToastKind::Info,
            format!("Report sent ({count} files)"),
        ),
        Err(error) => (
            ReportState::Failed(error.clone()),
            ToastKind::Error,
            format!("Report failed: {error}"),
        ),
    }
}

/// One file of a report, as it goes over the wire.
struct ReportFile {
    kind: &'static str,
    name: String,
    path: PathBuf,
    body: Vec<u8>,
}

/// Collects the files off the UI thread and uploads them one at a time, stopping
/// at the first refusal: a half-sent report is still worth reading. A crash file
/// goes away as soon as its own upload lands, so a report the server's hourly
/// limit cut short picks up where it stopped on the next press instead of
/// replaying files already sent.
async fn send_report(endpoints: Endpoints, token: String) -> Result<usize, String> {
    let files = tokio::task::spawn_blocking(collect_report)
        .await
        .map_err(|e| e.to_string())?;
    if files.is_empty() {
        return Err("there is nothing to send".to_owned());
    }

    let count = files.len();
    let bytes: usize = files.iter().map(|file| file.body.len()).sum();
    tracing::info!(files = count, bytes, "sending a problem report");

    for file in files {
        report::upload(&endpoints, &token, file.kind, &file.name, file.body)
            .await
            .map_err(|failure| describe(&failure))?;
        if file.kind != "crash" {
            continue;
        }
        let removed = tokio::task::spawn_blocking(move || std::fs::remove_file(file.path))
            .await
            .map_err(|e| e.to_string())
            .and_then(|result| result.map_err(|e| e.to_string()));
        if let Err(error) = removed {
            tracing::warn!(error = %error, "could not delete a sent crash report");
        }
    }

    Ok(count)
}

/// The live log, the generation behind it and every crash report on disk. A file
/// that cannot be read is left out rather than losing the whole report.
fn collect_report() -> Vec<ReportFile> {
    let mut files = Vec::new();

    if let Some(live) = diagnostics::log_path() {
        let mut rotated = live.clone().into_os_string();
        rotated.push(".1");
        for path in [live, PathBuf::from(rotated)] {
            files.extend(read_report(&path, "log"));
        }
    }
    for path in diagnostics::crash_reports() {
        files.extend(read_report(&path, "crash"));
    }

    files
}

fn read_report(path: &Path, kind: &'static str) -> Option<ReportFile> {
    let body = std::fs::read(path).ok()?;
    let name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)?
        .to_owned();

    Some(ReportFile {
        kind,
        name,
        path: path.to_path_buf(),
        body: report::tail(body, report::MAX_BYTES),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_is_cut_by_characters_not_bytes() {
        assert_eq!(truncate("ana".to_owned(), 32), "ana");
        assert_eq!(truncate("abcdef".to_owned(), 3), "abc");
        // Each of these is two bytes, so a byte cut would split one in half.
        assert_eq!(truncate("ááááá".to_owned(), 2), "áá");
    }

    #[test]
    fn every_form_a_theme_name_takes_reads_back() {
        assert_eq!(theme_name("vorcall-dark"), "Vorcall Dark");
        assert_eq!(theme_name("vorcall-light"), "Vorcall Light");
        assert_eq!(theme_name("custom:midnight-oil"), "Midnight Oil");
    }

    /// The count is what the reader can quote back; a failure says what went
    /// wrong, and both reach the page and a toast.
    #[test]
    fn a_finished_report_says_how_it_went() {
        let (state, kind, note) = report_outcome(Ok(3));
        assert_eq!(state, ReportState::Sent(3));
        assert_eq!(kind, ToastKind::Info);
        assert_eq!(note, "Report sent (3 files)");

        let (state, kind, note) = report_outcome(Err("Cannot reach the server".to_owned()));
        assert_eq!(
            state,
            ReportState::Failed("Cannot reach the server".to_owned())
        );
        assert_eq!(kind, ToastKind::Error);
        assert_eq!(note, "Report failed: Cannot reach the server");
    }

    /// Only the three the listener captures system-wide restart it.
    #[test]
    fn the_global_actions_are_the_voice_ones() {
        assert!(is_global("push_to_talk"));
        assert!(is_global("toggle_mute"));
        assert!(is_global("toggle_deafen"));
        assert!(!is_global("quick_switcher"));
        assert!(!is_global("not_an_action"));
    }
}
