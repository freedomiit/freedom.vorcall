//! The REST half of the protocol: one page of the newest messages, fetched
//! right after `Welcome` so a fresh connection is not an empty room.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use prost::Message as _;
use vorcall_proto::v1::{ChatMessage, MessagePage};

use crate::endpoints::Endpoints;

/// The header carrying the pre-shared door key on every request.
pub const KEY_HEADER: &str = "X-Vorcall-Key";

/// Returned inside the `anyhow::Error` of [`fetch_latest`], so a caller that
/// cares (the UI distinguishes a stale key from a flaky network) can downcast.
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("the server rejected the pre-shared key")]
    Unauthorized,
    #[error("the server answered HTTP {0}")]
    Status(u16),
    #[error("cannot reach the server: {0}")]
    Transport(String),
    #[error("the server returned a malformed MessagePage: {0}")]
    Malformed(String),
}

/// Fetches the newest `limit` messages, ascending by id.
///
/// Needs no process-wide `rustls` provider: `tls_config` hands reqwest one of
/// its own, with the ring provider and the bundled roots. `main` still installs
/// a default, because tungstenite builds its `ClientConfig` from it.
pub async fn fetch_latest(endpoints: &Endpoints, limit: u32) -> anyhow::Result<Vec<ChatMessage>> {
    let mut url = endpoints
        .http_base
        .join("/api/messages")
        .map_err(|e| HistoryError::Malformed(e.to_string()))?;
    url.set_query(Some(&format!("limit={limit}")));

    let tls = tls_config().map_err(|e| HistoryError::Transport(e.to_string()))?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(10))
        .tls_backend_preconfigured(tls.clone())
        .build()
        .map_err(|e| HistoryError::Transport(e.to_string()))?;

    let response = client
        .get(url)
        .header(KEY_HEADER, &endpoints.key)
        .send()
        .await
        .map_err(|e| HistoryError::Transport(e.to_string()))?;

    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(HistoryError::Unauthorized.into());
    }
    if !status.is_success() {
        return Err(HistoryError::Status(status.as_u16()).into());
    }

    let body = response
        .bytes()
        .await
        .map_err(|e| HistoryError::Transport(e.to_string()))?;

    let page = MessagePage::decode(body).map_err(|e| HistoryError::Malformed(e.to_string()))?;

    tracing::debug!(
        count = page.messages.len(),
        has_more = page.has_more,
        "fetched history page"
    );

    Ok(page.messages)
}

/// The TLS setup shared by every history fetch.
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
