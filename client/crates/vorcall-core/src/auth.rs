//! The account endpoints of `PROTOCOL.md`: register, sign in, refresh, sign out
//! and change the password.
//!
//! Every request body is protobuf and carries the door key; only the password
//! change also needs a bearer. Nothing here logs a body: they all hold secrets.

use prost::Message as _;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use vorcall_proto::v1::{
    ChangePasswordRequest, LoginRequest, LogoutRequest, RefreshRequest, RegisterRequest,
    TokenResponse,
};

use crate::endpoints::Endpoints;
use crate::http::{self, ApiFailure};
use crate::session::{self, Session};

const PROTOBUF: &str = "application/x-protobuf";

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
    tokens(endpoints, "/api/auth/register", body.encode_to_vec()).await
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
    tokens(endpoints, "/api/auth/login", body.encode_to_vec()).await
}

/// Rotates the pair: the presented refresh token is spent, whatever the answer.
pub async fn refresh(endpoints: &Endpoints, refresh_token: &str) -> Result<Session, ApiFailure> {
    let body = RefreshRequest {
        refresh_token: refresh_token.to_owned(),
    };
    tokens(endpoints, "/api/auth/refresh", body.encode_to_vec()).await
}

/// Best effort: the server answers 204 whatever it finds, and a session the
/// server refuses to end is a session the client stops holding anyway. Only a
/// server we could not reach at all is an error worth reporting.
pub async fn logout(endpoints: &Endpoints, refresh_token: &str) -> Result<(), ApiFailure> {
    let body = LogoutRequest {
        refresh_token: refresh_token.to_owned(),
    };
    let response = post(endpoints, "/api/auth/logout", body.encode_to_vec(), None).await?;

    if !response.status().is_success() {
        tracing::warn!(
            status = response.status().as_u16(),
            "logout was refused; dropping the session anyway"
        );
    }
    Ok(())
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
    let response = post(
        endpoints,
        "/api/auth/password",
        body.encode_to_vec(),
        Some(&session.access_token),
    )
    .await?;

    if response.status().is_success() {
        Ok(())
    } else {
        Err(http::failure_from(response).await)
    }
}

/// The three endpoints that answer with a `TokenResponse`.
async fn tokens(endpoints: &Endpoints, path: &str, body: Vec<u8>) -> Result<Session, ApiFailure> {
    let response = post(endpoints, path, body, None).await?;

    if !response.status().is_success() {
        return Err(http::failure_from(response).await);
    }

    let body = response
        .bytes()
        .await
        .map_err(|e| ApiFailure::Transport(e.to_string()))?;
    let tokens = TokenResponse::decode(body).map_err(|e| ApiFailure::Malformed(e.to_string()))?;

    Ok(Session::from_response(tokens, session::now_unix()))
}

async fn post(
    endpoints: &Endpoints,
    path: &str,
    body: Vec<u8>,
    access_token: Option<&str>,
) -> Result<reqwest::Response, ApiFailure> {
    let url = endpoints
        .http_base
        .join(path)
        .map_err(|e| ApiFailure::Malformed(e.to_string()))?;

    let mut request = http::client()?
        .post(url)
        .header(http::KEY_HEADER, &endpoints.key)
        .header(CONTENT_TYPE, PROTOBUF)
        .body(body);
    if let Some(access_token) = access_token {
        request = request.header(AUTHORIZATION, http::bearer(access_token));
    }

    request
        .send()
        .await
        .map_err(|e| ApiFailure::Transport(e.to_string()))
}
