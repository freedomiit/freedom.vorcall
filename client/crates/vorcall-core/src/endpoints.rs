//! Where the client talks to, and with which door key.
//!
//! Three layers, most specific first: an environment variable, then what the
//! user saved in `config.toml` through the sign-in screen's "Server" section,
//! then what the build baked in. That last layer is why a release binary
//! reaches the project's own server with nothing configured, and the middle one
//! is why the same binary can be pointed at a self-hosted server without a
//! rebuild.

use std::fmt;

use anyhow::{Context, anyhow, bail};
use url::Url;

/// Where a build that names no server of its own points. Release builds bake
/// their real address in through `VORCALL_SERVER_URL` (see
/// `scripts/build-client-*.sh`), so no deployment's hostname lives in the source;
/// a plain `cargo build` gets a local development server, and anyone else sets
/// the address in the sign-in screen's Server section.
pub const DEFAULT_SERVER_URL: &str = "http://localhost:5000";

/// The key that development builds carry; a real deployment overrides it.
pub const DEV_KEY: &str = "dev";

/// Which layer supplied the target, so the UI can say whether the build's own
/// server is in use or one the user typed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// `VORCALL_SERVER_URL` / `VORCALL_SERVER_KEY` in the environment. Wins over
    /// anything saved, so the sign-in screen shows the field read-only.
    Environment,
    /// Saved in `config.toml`.
    Stored,
    /// Compiled in, or the default above.
    Baked,
}

#[derive(Clone)]
pub struct Endpoints {
    pub http_base: Url,
    pub ws_url: Url,
    pub key: String,
    pub url_source: Source,
    pub key_source: Source,
}

impl Endpoints {
    /// Builds a target from a base URL and a key, validating both the way
    /// [`resolve_with`] does — the sign-in screen calls this to check what was
    /// typed before saving it.
    pub fn parse(base: &str, key: &str) -> anyhow::Result<Self> {
        Self::build(base, key, Source::Stored, Source::Stored)
    }

    fn build(
        base: &str,
        key: &str,
        url_source: Source,
        key_source: Source,
    ) -> anyhow::Result<Self> {
        let base = base.trim().trim_end_matches('/');
        if base.is_empty() {
            bail!("the server address is empty");
        }

        let http_base = Url::parse(base).with_context(|| format!("{base} is not a valid URL"))?;
        let ws_scheme = match http_base.scheme() {
            "https" => "wss",
            "http" => "ws",
            other => bail!("the server address must be http or https, got {other}"),
        };
        if http_base.host_str().is_none() {
            bail!("{base} names no host");
        }

        let mut ws_url = http_base.clone();
        ws_url
            .set_scheme(ws_scheme)
            .map_err(|()| anyhow!("cannot derive a WebSocket URL from {http_base}"))?;
        ws_url.set_path("/ws");
        ws_url.set_query(None);
        ws_url.set_fragment(None);

        let key = key.trim();
        if key.is_empty() {
            bail!("the server key is empty; the client cannot authenticate");
        }

        Ok(Self {
            http_base,
            ws_url,
            key: key.to_owned(),
            url_source,
            key_source,
        })
    }

    pub fn is_dev_key(&self) -> bool {
        self.key == DEV_KEY
    }

    /// The host the WebSocket connects to, without port: where media goes when
    /// `VoiceReady.host` is empty.
    pub fn host(&self) -> String {
        self.http_base.host_str().unwrap_or_default().to_owned()
    }

    /// What the sign-in screen puts in the address field.
    pub fn display_url(&self) -> String {
        self.http_base.as_str().trim_end_matches('/').to_owned()
    }
}

// Hand-written so the pre-shared key never reaches a log line.
impl fmt::Debug for Endpoints {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Endpoints")
            .field("http_base", &self.http_base.as_str())
            .field("ws_url", &self.ws_url.as_str())
            .field("key", &"<redacted>")
            .field("url_source", &self.url_source)
            .field("key_source", &self.key_source)
            .finish()
    }
}

/// The target with nothing saved: the environment, then the build.
pub fn resolve() -> anyhow::Result<Endpoints> {
    resolve_with(None, None)
}

/// The target a stored configuration asks for, still behind the environment.
pub fn resolve_with(
    stored_url: Option<&str>,
    stored_key: Option<&str>,
) -> anyhow::Result<Endpoints> {
    let (base, url_source) = layer(
        runtime_var("VORCALL_SERVER_URL"),
        non_empty(stored_url),
        Some(baked_url()),
    )
    .expect("baked_url always yields a value");

    let (key, key_source) = layer(
        runtime_var("VORCALL_SERVER_KEY"),
        non_empty(stored_key),
        baked_key(),
    )
    .ok_or_else(|| {
        anyhow!(
            "no server key: set one in the Server section of the sign-in screen, \
             or in VORCALL_SERVER_KEY"
        )
    })?;

    Endpoints::build(&base, &key, url_source, key_source)
}

/// The three layers, most specific first. Split out from [`resolve_with`] so it
/// can be tested without an environment: cargo's `[env]` table puts
/// `VORCALL_SERVER_KEY` into the test process too.
fn layer(
    from_env: Option<String>,
    stored: Option<String>,
    baked: Option<String>,
) -> Option<(String, Source)> {
    from_env
        .map(|v| (v, Source::Environment))
        .or_else(|| stored.map(|v| (v, Source::Stored)))
        .or_else(|| baked.map(|v| (v, Source::Baked)))
}

/// The address compiled into this build, or the project's own server.
pub fn baked_url() -> String {
    non_empty(option_env!("VORCALL_SERVER_URL")).unwrap_or_else(|| DEFAULT_SERVER_URL.to_owned())
}

/// The key compiled into this build. `None` only for a build made without one.
pub fn baked_key() -> Option<String> {
    non_empty(option_env!("VORCALL_SERVER_KEY"))
}

/// Whether the environment pins the target. The sign-in screen shows the server
/// fields read-only when it does, because saving them would change nothing.
pub fn pinned_by_env() -> bool {
    runtime_var("VORCALL_SERVER_URL").is_some() || runtime_var("VORCALL_SERVER_KEY").is_some()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stored_target_derives_its_websocket_url() {
        let endpoints = Endpoints::parse("https://chat.example.org/", "hunter2").unwrap();
        assert_eq!(endpoints.ws_url.as_str(), "wss://chat.example.org/ws");
        assert_eq!(endpoints.display_url(), "https://chat.example.org");
        assert_eq!(endpoints.host(), "chat.example.org");
        assert_eq!(endpoints.url_source, Source::Stored);
    }

    #[test]
    fn plain_http_gives_a_plain_websocket() {
        let endpoints = Endpoints::parse("http://localhost:5000", "dev").unwrap();
        assert_eq!(endpoints.ws_url.as_str(), "ws://localhost:5000/ws");
        assert!(endpoints.is_dev_key());
    }

    #[test]
    fn a_path_query_and_fragment_are_dropped_from_the_websocket_url() {
        let endpoints = Endpoints::parse("https://example.org/base?a=1#frag", "k").unwrap();
        assert_eq!(endpoints.ws_url.as_str(), "wss://example.org/ws");
    }

    #[test]
    fn what_the_sign_in_screen_must_refuse() {
        for (base, key) in [
            ("", "k"),
            ("   ", "k"),
            ("not a url", "k"),
            ("ftp://example.org", "k"),
            ("https://example.org", ""),
            ("https://example.org", "  "),
        ] {
            assert!(
                Endpoints::parse(base, key).is_err(),
                "expected {base:?} / {key:?} to be refused"
            );
        }
    }

    #[test]
    fn the_environment_beats_a_stored_target_which_beats_the_baked_one() {
        let env = || Some("env".to_owned());
        let stored = || Some("stored".to_owned());
        let baked = || Some("baked".to_owned());

        assert_eq!(
            layer(env(), stored(), baked()),
            Some(("env".to_owned(), Source::Environment))
        );
        assert_eq!(
            layer(None, stored(), baked()),
            Some(("stored".to_owned(), Source::Stored))
        );
        assert_eq!(
            layer(None, None, baked()),
            Some(("baked".to_owned(), Source::Baked))
        );
        assert_eq!(layer(None, None, None), None);
    }

    #[test]
    fn a_stored_url_is_what_resolution_uses() {
        // Only the URL: cargo's [env] pins VORCALL_SERVER_KEY for this process,
        // and a developer may have exported an address of their own.
        if std::env::var_os("VORCALL_SERVER_URL").is_some() {
            return;
        }
        let endpoints = resolve_with(Some("https://mine.example.org"), None).unwrap();
        assert_eq!(endpoints.display_url(), "https://mine.example.org");
        assert_eq!(endpoints.url_source, Source::Stored);
    }
}
