//! Where the client talks to, and with which door key.
//!
//! Both the base URL and the key may be baked in at build time and overridden
//! at runtime, so a single binary can be pointed at a local server.

use std::fmt;

use anyhow::{Context, anyhow, bail};
use url::Url;

pub const DEFAULT_SERVER_URL: &str = "https://vorcall.example.com";

/// The key that development builds carry; a real deployment overrides it.
pub const DEV_KEY: &str = "dev";

#[derive(Clone)]
pub struct Endpoints {
    pub http_base: Url,
    pub ws_url: Url,
    pub key: String,
}

impl Endpoints {
    pub fn is_dev_key(&self) -> bool {
        self.key == DEV_KEY
    }
}

// Hand-written so the pre-shared key never reaches a log line.
impl fmt::Debug for Endpoints {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Endpoints")
            .field("http_base", &self.http_base.as_str())
            .field("ws_url", &self.ws_url.as_str())
            .field("key", &"<redacted>")
            .finish()
    }
}

pub fn resolve() -> anyhow::Result<Endpoints> {
    let base = runtime_var("VORCALL_SERVER_URL")
        .or_else(|| non_empty(option_env!("VORCALL_SERVER_URL")))
        .unwrap_or_else(|| DEFAULT_SERVER_URL.to_owned());
    let base = base.trim().trim_end_matches('/');

    let http_base = Url::parse(base).with_context(|| format!("{base} is not a valid URL"))?;
    let ws_scheme = match http_base.scheme() {
        "https" => "wss",
        "http" => "ws",
        other => bail!("VORCALL_SERVER_URL must be http or https, got {other}"),
    };

    let mut ws_url = http_base.clone();
    ws_url
        .set_scheme(ws_scheme)
        .map_err(|()| anyhow!("cannot derive a WebSocket URL from {http_base}"))?;
    ws_url.set_path("/ws");
    ws_url.set_query(None);
    ws_url.set_fragment(None);

    let key = runtime_var("VORCALL_SERVER_KEY")
        .or_else(|| non_empty(Some(env!("VORCALL_SERVER_KEY"))))
        .ok_or_else(|| anyhow!("VORCALL_SERVER_KEY is empty; the client cannot authenticate"))?;

    Ok(Endpoints {
        http_base,
        ws_url,
        key,
    })
}

fn runtime_var(name: &str) -> Option<String> {
    non_empty(std::env::var(name).ok().as_deref())
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}
