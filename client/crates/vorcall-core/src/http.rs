//! The HTTP half of the protocol: the one reqwest client every REST call shares,
//! the TLS setup it must agree on with tokio-tungstenite, and how a non-success
//! response is turned into an [`ApiFailure`] the connection loop can act on.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use prost::Message as _;
use reqwest::header::{HeaderMap, RETRY_AFTER, WWW_AUTHENTICATE};
use vorcall_proto::v1::ApiError;

/// The header carrying the pre-shared door key on every request.
pub const KEY_HEADER: &str = "X-Vorcall-Key";

/// `PROTOCOL.md` promises `Retry-After` on every 429; a proxy that drops it
/// must not turn into a retry storm.
const DEFAULT_RETRY_AFTER_SECS: u64 = 60;

/// Why a REST call did not produce an answer the caller can use.
///
/// A 401 comes in three shapes, told apart by the `WWW-Authenticate` scheme:
/// `X-Vorcall-Key` is [`ApiFailure::StaleKey`], the build carries the wrong door
/// key; `Bearer` is [`ApiFailure::AuthChallenge`], fixed by a refresh or a
/// sign-in; no challenge at all is an answer from the auth endpoints themselves
/// — a wrong password or a refused refresh token — and stays a
/// [`ApiFailure::Status`] carrying the server's `ApiError` detail.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ApiFailure {
    #[error("the server rejected the pre-shared key")]
    StaleKey,
    #[error("{0}")]
    AuthChallenge(String),
    #[error("{1}")]
    Status(u16, String),
    #[error("too many attempts, try again in {0}s")]
    Throttled(u64),
    #[error("cannot reach the server: {0}")]
    Transport(String),
    #[error("malformed response: {0}")]
    Malformed(String),
}

/// The process-wide client. Built once: a session refreshes tokens and pages
/// history for hours, and a fresh client would throw the connection pool away
/// every time.
pub fn client() -> Result<&'static reqwest::Client, ApiFailure> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

    if let Some(client) = CLIENT.get() {
        return Ok(client);
    }

    let tls = tls_config().map_err(|e| ApiFailure::Transport(e.to_string()))?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(10))
        .tls_backend_preconfigured(tls.clone())
        .build()
        .map_err(|e| ApiFailure::Transport(e.to_string()))?;

    Ok(CLIENT.get_or_init(|| client))
}

pub fn bearer(access_token: &str) -> String {
    format!("Bearer {access_token}")
}

/// The bearer gate of `PROTOCOL.md` refused the request.
///
/// reqwest and tokio-tungstenite resolve to the same `http` crate, so this
/// reads an upgrade response's headers just as well as a REST one's.
pub fn is_bearer_challenge(headers: &HeaderMap) -> bool {
    scheme_is(headers, "bearer")
}

/// The door-key gate refused the request, which only a rebuilt client fixes.
pub fn is_key_challenge(headers: &HeaderMap) -> bool {
    scheme_is(headers, KEY_HEADER)
}

fn scheme_is(headers: &HeaderMap, scheme: &str) -> bool {
    headers
        .get(WWW_AUTHENTICATE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim_start().get(..scheme.len()))
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(scheme))
}

/// Classifies any non-success response. Consumes it: the body may carry the
/// `ApiError` detail the UI shows.
pub async fn failure_from(response: reqwest::Response) -> ApiFailure {
    let status = response.status();
    let key_challenge = is_key_challenge(response.headers());
    let bearer_challenge = is_bearer_challenge(response.headers());
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());

    let detail = detail(response).await;

    match status.as_u16() {
        401 if key_challenge => ApiFailure::StaleKey,
        401 if bearer_challenge => ApiFailure::AuthChallenge(detail),
        429 => ApiFailure::Throttled(retry_after.unwrap_or(DEFAULT_RETRY_AFTER_SECS)),
        // A 401 with no challenge is the auth endpoints answering: a wrong
        // password, or a refresh token the server refuses.
        code => ApiFailure::Status(code, detail),
    }
}

/// The server's own words when it sent an `ApiError`, the status line otherwise.
async fn detail(response: reqwest::Response) -> String {
    let status = response.status();
    let fallback = || {
        status
            .canonical_reason()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("HTTP {}", status.as_u16()))
    };

    let Ok(body) = response.bytes().await else {
        return fallback();
    };

    match ApiError::decode(body) {
        Ok(error) if !error.detail.trim().is_empty() => error.detail,
        _ => fallback(),
    }
}

/// The TLS setup shared by every REST call.
///
/// reqwest and tokio-tungstenite must trust the same roots: tungstenite is
/// built with `rustls-tls-webpki-roots`, so REST verifies against the same
/// bundled Mozilla set instead of reqwest's platform verifier. Built once —
/// parsing the root bundle on every fetch would be wasteful.
fn tls_config() -> Result<&'static rustls::ClientConfig, rustls::Error> {
    static CONFIG: OnceLock<rustls::ClientConfig> = OnceLock::new();

    if let Some(config) = CONFIG.get() {
        return Ok(config);
    }

    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    // reqwest only fills ALPN in on a config it builds itself; these are the
    // protocols it would have offered.
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(CONFIG.get_or_init(|| config))
}
