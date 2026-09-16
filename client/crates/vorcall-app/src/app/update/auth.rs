//! Signing in, signing out and changing a password.
//!
//! Implemented in full: the skeleton has to reach the shell, and the shell has to
//! be able to leave it again.

use iced::Task;
use vorcall_core::{ApiFailure, Endpoints, auth};

use crate::app::message::{AuthMsg, Message};
use crate::app::state::rules::{describe, validate_password, validate_username};
use crate::app::state::ui::Dialog;
use crate::app::{App, Screen, ServerForm};

pub fn update(app: &mut App, message: AuthMsg) -> Task<Message> {
    match message {
        AuthMsg::UsernameChanged(value) => {
            match &mut app.screen {
                Screen::Login { username, .. } | Screen::Register { username, .. } => {
                    *username = value;
                }
                Screen::Main(_) => {}
            }
            Task::none()
        }
        AuthMsg::PasswordChanged(value) => {
            match &mut app.screen {
                Screen::Login { password, .. } | Screen::Register { password, .. } => {
                    *password = value;
                }
                Screen::Main(_) => {}
            }
            Task::none()
        }
        AuthMsg::ConfirmChanged(value) => {
            if let Screen::Register { confirm, .. } = &mut app.screen {
                *confirm = value;
            }
            Task::none()
        }
        AuthMsg::InviteChanged(value) => {
            if let Screen::Register { invite, .. } = &mut app.screen {
                *invite = value;
            }
            Task::none()
        }
        AuthMsg::ShowLogin => {
            let username = carried_username(app);
            app.screen = Screen::login(username);
            focus_username()
        }
        AuthMsg::ShowRegister => {
            let username = carried_username(app);
            app.screen = Screen::Register {
                username,
                password: String::new(),
                confirm: String::new(),
                invite: String::new(),
                error: None,
                busy: false,
            };
            focus_username()
        }
        AuthMsg::ServerToggle => {
            app.server_form.open = !app.server_form.open;
            app.server_form.error = None;
            Task::none()
        }
        AuthMsg::ServerUrlChanged(value) => {
            app.server_form.url = value;
            Task::none()
        }
        AuthMsg::ServerKeyChanged(value) => {
            app.server_form.key = value;
            Task::none()
        }
        AuthMsg::ServerSave => save_server(app),
        AuthMsg::ServerReset => reset_server(app),
        AuthMsg::LoginSubmit => submit_login(app),
        AuthMsg::RegisterSubmit => submit_register(app),
        AuthMsg::LoginResult(Ok(session)) => app.signed_in(session),
        AuthMsg::LoginResult(Err(failure)) => {
            let detail = describe(&failure);
            match &mut app.screen {
                Screen::Login { error, busy, .. } | Screen::Register { error, busy, .. } => {
                    *error = Some(detail);
                    *busy = false;
                }
                Screen::Main(_) => {}
            }
            Task::none()
        }
        AuthMsg::Logout => logout(app),
        AuthMsg::ChangePasswordSubmit => submit_change_password(app),
        AuthMsg::ChangePasswordResult(result) => on_change_password(app, result),
        AuthMsg::DialogCurrentChanged(value) => {
            if let Some(Dialog::ChangePassword { current, .. }) = app.dialog_mut() {
                *current = value;
            }
            Task::none()
        }
        AuthMsg::DialogNewChanged(value) => {
            if let Some(Dialog::ChangePassword { new, .. }) = app.dialog_mut() {
                *new = value;
            }
            Task::none()
        }
        AuthMsg::DialogConfirmChanged(value) => {
            if let Some(Dialog::ChangePassword { confirm, .. }) = app.dialog_mut() {
                *confirm = value;
            }
            Task::none()
        }
    }
}

/// The name the other screen should start with: whatever was typed, else the one
/// that signed in last.
fn carried_username(app: &App) -> String {
    match &app.screen {
        Screen::Login { username, .. } | Screen::Register { username, .. } => username.clone(),
        Screen::Main(_) => app.config.username.clone(),
    }
}

fn focus_username() -> Task<Message> {
    iced::widget::operation::focus(iced::widget::Id::new(crate::view::USERNAME_ID))
}

fn submit_login(app: &mut App) -> Task<Message> {
    let endpoints = app.endpoints.clone();
    let Screen::Login {
        username,
        password,
        error,
        busy,
    } = &mut app.screen
    else {
        return Task::none();
    };
    if *busy {
        return Task::none();
    }

    let username = username.trim().to_owned();
    if let Err(detail) = validate_username(&username) {
        *error = Some(detail);
        return Task::none();
    }
    if password.is_empty() {
        *error = Some("Enter your password.".to_owned());
        return Task::none();
    }

    let password = password.clone();
    *error = None;
    *busy = true;

    Task::perform(
        async move { auth::login(&endpoints, &username, &password).await },
        |result| Message::Auth(AuthMsg::LoginResult(result)),
    )
}

fn submit_register(app: &mut App) -> Task<Message> {
    let endpoints = app.endpoints.clone();
    let Screen::Register {
        username,
        password,
        confirm,
        invite,
        error,
        busy,
    } = &mut app.screen
    else {
        return Task::none();
    };
    if *busy {
        return Task::none();
    }

    let username = username.trim().to_owned();
    let invite = invite.trim().to_owned();
    if let Err(detail) = validate_username(&username) {
        *error = Some(detail);
        return Task::none();
    }
    if let Err(detail) = validate_password(password) {
        *error = Some(detail);
        return Task::none();
    }
    if password != confirm {
        *error = Some("Passwords do not match".to_owned());
        return Task::none();
    }
    if invite.is_empty() {
        *error = Some("Enter your invite code.".to_owned());
        return Task::none();
    }

    let password = password.clone();
    *error = None;
    *busy = true;

    Task::perform(
        async move { auth::register(&endpoints, &username, &password, &invite).await },
        |result| Message::Auth(AuthMsg::LoginResult(result)),
    )
}

/// Tells the server, then drops the session whatever it answered.
fn logout(app: &mut App) -> Task<Message> {
    let told = match app.session.take() {
        Some(session) => {
            let endpoints = app.endpoints.clone();
            let refresh_token = session.refresh_token;
            Task::perform(
                async move {
                    if let Err(e) = auth::logout(&endpoints, &refresh_token).await {
                        tracing::warn!(error = %e, "the server did not confirm the sign-out");
                    }
                },
                |()| Message::Noop,
            )
        }
        None => Task::none(),
    };

    Task::batch([told, app.sign_out(None)])
}

/// Points this client at the typed server and saves it.
///
/// An empty key field keeps whatever key is already in force, so a self-hoster
/// who only moved their address does not have to type the key again.
fn save_server(app: &mut App) -> Task<Message> {
    if app.server_form.pinned {
        return Task::none();
    }

    let url = app.server_form.url.trim().to_owned();
    let typed_key = app.server_form.key.trim().to_owned();
    let key = if typed_key.is_empty() {
        app.endpoints.key.clone()
    } else {
        typed_key.clone()
    };

    let endpoints = match Endpoints::parse(&url, &key) {
        Ok(endpoints) => endpoints,
        Err(e) => {
            app.server_form.error = Some(format!("{e:#}"));
            return Task::none();
        }
    };

    app.config.server_url = Some(endpoints.display_url());
    if !typed_key.is_empty() {
        app.config.server_key = Some(typed_key);
    }
    app.save_config();
    app.server_form = ServerForm::new(&endpoints, &app.config);
    app.endpoints = endpoints;

    // A session is a token this server never issued; the account behind it may
    // not even exist here.
    forget_session(app)
}

/// Drops the saved server and goes back to the one the build carries.
fn reset_server(app: &mut App) -> Task<Message> {
    if app.server_form.pinned {
        return Task::none();
    }

    app.config.server_url = None;
    app.config.server_key = None;
    app.save_config();

    match vorcall_core::endpoints::resolve() {
        Ok(endpoints) => {
            app.server_form = ServerForm::new(&endpoints, &app.config);
            app.endpoints = endpoints;
            forget_session(app)
        }
        // Only a build that baked no key at all lands here, and it has nothing to
        // fall back to: keep the server the user typed rather than strand them.
        Err(e) => {
            app.config.server_url = Some(app.endpoints.display_url());
            app.config.server_key = Some(app.endpoints.key.clone());
            app.save_config();
            app.server_form.error = Some(format!("{e:#}"));
            Task::none()
        }
    }
}

/// Back to a clean sign-in screen after the server underneath changed.
fn forget_session(app: &mut App) -> Task<Message> {
    if app.session.is_some() {
        return app.sign_out(None);
    }
    app.screen = Screen::login(carried_username(app));
    Task::none()
}

fn submit_change_password(app: &mut App) -> Task<Message> {
    let Some(session) = app.session.clone() else {
        return Task::none();
    };
    let endpoints = app.endpoints.clone();
    let Some(Dialog::ChangePassword {
        current,
        new,
        confirm,
        error,
        busy,
    }) = app.dialog_mut()
    else {
        return Task::none();
    };
    if *busy {
        return Task::none();
    }

    if current.is_empty() {
        *error = Some("Enter your current password.".to_owned());
        return Task::none();
    }
    if let Err(detail) = validate_password(new) {
        *error = Some(detail);
        return Task::none();
    }
    if new != confirm {
        *error = Some("Passwords do not match".to_owned());
        return Task::none();
    }

    let current = current.clone();
    let new = new.clone();
    *error = None;
    *busy = true;

    Task::perform(
        async move { auth::change_password(&endpoints, &session, &current, &new).await },
        |result| Message::Auth(AuthMsg::ChangePasswordResult(result)),
    )
}

fn on_change_password(app: &mut App, result: Result<(), ApiFailure>) -> Task<Message> {
    let failure = match result {
        Ok(()) => {
            app.ui.dialog = None;
            if let Some(main) = app.main_mut() {
                main.notice = Some("Password changed".to_owned());
            }
            return Task::none();
        }
        Err(failure) => failure,
    };

    let detail = match &failure {
        // A Bearer challenge is the access token, never the password: the
        // endpoint answers a wrong current password with a plain 401 carrying
        // its own `ApiError` and no `WWW-Authenticate` header.
        ApiFailure::AuthChallenge(_) => "Your session expired, try again".to_owned(),
        ApiFailure::Status(401, _) => "Current password is wrong".to_owned(),
        other => describe(other),
    };
    if let Some(Dialog::ChangePassword { error, busy, .. }) = app.dialog_mut() {
        *error = Some(detail);
        *busy = false;
    }
    Task::none()
}
