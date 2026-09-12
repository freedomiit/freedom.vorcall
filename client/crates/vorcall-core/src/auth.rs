//! The account endpoints of `PROTOCOL.md`: register, sign in, refresh, sign out
//! and change the password.
//!
//! Every request body is protobuf and carries the door key; only the password
//! change also needs a bearer. Nothing here logs a body: they all hold secrets.

use prost::Message as _;
use vorcall_proto::v1::{
    ChangePasswordRequest, LoginRequest, LogoutRequest, RefreshRequest, RegisterRequest,
    TokenResponse,
};

use crate::endpoints::Endpoints;
use crate::http::{self, ApiFailure};
use crate::session::{self, Session};

/// Registration signs the user in: the 201 carries the same tokens a login does.
pub async fn register(
    endpoints: &Endpoints,
    username: &str,
    password: &str,
    invite_code: &str,
) -> Result<Session, ApiFailure> {
    let body = RegisterRequest {
        username: username.to_owned(),
        password: password.to_owned(),
        invite_code: invite_code.to_owned(),
    };
    tokens(endpoints, "/api/auth/register", &body).await
}

pub async fn login(
    endpoints: &Endpoints,
    username: &str,
    password: &str,
) -> Result<Session, ApiFailure> {
    let body = LoginRequest {
        username: username.to_owned(),
        password: password.to_owned(),
    };
    tokens(endpoints, "/api/auth/login", &body).await
}

/// Rotates the pair: the presented refresh token is spent, whatever the answer.
pub async fn refresh(endpoints: &Endpoints, refresh_token: &str) -> Result<Session, ApiFailure> {
    let body = RefreshRequest {
        refresh_token: refresh_token.to_owned(),
    };
    tokens(endpoints, "/api/auth/refresh", &body).await
}

/// Best effort: the server answers 204 whatever it finds, and a session the
/// server refuses to end is a session the client stops holding anyway. Only a
/// server we could not reach at all is an error worth reporting.
pub async fn logout(endpoints: &Endpoints, refresh_token: &str) -> Result<(), ApiFailure> {
    let body = LogoutRequest {
        refresh_token: refresh_token.to_owned(),
    };
    let url = http::api_url(endpoints, "/api/auth/logout")?;

    match http::post_proto(endpoints, None, url, &body).await {
        Ok(_) => Ok(()),
        Err(failure @ ApiFailure::Transport(_)) => Err(failure),
        Err(failure) => {
            tracing::warn!(%failure, "logout was refused; dropping the session anyway");
            Ok(())
        }
    }
}

/// Keeps the caller's own refresh token alive; the server revokes every other
/// one of that user.
pub async fn change_password(
    endpoints: &Endpoints,
    session: &Session,
    current: &str,
    new: &str,
) -> Result<(), ApiFailure> {
    let body = ChangePasswordRequest {
        current_password: current.to_owned(),
        new_password: new.to_owned(),
        refresh_token: session.refresh_token.clone(),
    };
    let url = http::api_url(endpoints, "/api/auth/password")?;
    http::post_proto(endpoints, Some(&session.access_token), url, &body).await?;
    Ok(())
}

/// The three endpoints that answer with a `TokenResponse`.
async fn tokens<M: prost::Message>(
    endpoints: &Endpoints,
    path: &str,
    body: &M,
) -> Result<Session, ApiFailure> {
    let url = http::api_url(endpoints, path)?;
    let response = http::post_proto(endpoints, None, url, body).await?;
    let tokens =
        TokenResponse::decode(response).map_err(|e| ApiFailure::Malformed(e.to_string()))?;

    Ok(Session::from_response(tokens, session::now_unix()))
}
