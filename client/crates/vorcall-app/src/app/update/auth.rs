//! Signing in, signing out and changing a password.
//!
//! Implemented in full: the skeleton has to reach the shell, and the shell has to
//! be able to leave it again.

use iced::Task;
use vorcall_core::{ApiFailure, auth};

use crate::app::message::{AuthMsg, Message};
use crate::app::state::rules::{describe, validate_password, validate_username};
use crate::app::state::ui::Dialog;
use crate::app::{App, Screen};

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
        // The server cannot tell a wrong current password from a refused token,
        // and only one of the two is worth saying here.
        ApiFailure::AuthChallenge(_) | ApiFailure::Status(401, _) => {
            "Current password is wrong".to_owned()
        }
        other => describe(other),
    };
    if let Some(Dialog::ChangePassword { error, busy, .. }) = app.dialog_mut() {
        *error = Some(detail);
        *busy = false;
    }
    Task::none()
}
