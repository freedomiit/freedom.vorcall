//! The signed-in session: the tokens `PROTOCOL.md` hands out, and the file they
//! survive a restart in.
//!
//! The client never parses the JWT; the server's `expires_in` is the only clock
//! it needs.

use std::fmt;
use std::io::{ErrorKind, Write as _};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use vorcall_proto::v1::TokenResponse;

const FILE: &str = "session.toml";
const TEMP_FILE: &str = "session.toml.tmp";

/// Refresh this many seconds before the access token expires, so a request
/// never spends a round trip on a 401 it could have avoided.
pub const REFRESH_MARGIN_SECS: i64 = 60;

#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_unix: i64,
    pub user_id: i64,
    pub username: String,
}

// Hand-written so no token ever reaches a log line.
impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("user_id", &self.user_id)
            .field("username", &self.username)
            .field("expires_at_unix", &self.expires_at_unix)
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .finish()
    }
}

impl Session {
    pub fn from_response(response: TokenResponse, now_unix: i64) -> Self {
        Self {
            access_token: response.access_token,
            refresh_token: response.refresh_token,
            expires_at_unix: now_unix.saturating_add(i64::from(response.expires_in)),
            user_id: response.user_id,
            username: response.username,
        }
    }

    pub fn needs_refresh(&self, now_unix: i64) -> bool {
        self.seconds_left(now_unix) < REFRESH_MARGIN_SECS
    }

    pub fn seconds_left(&self, now_unix: i64) -> i64 {
        self.expires_at_unix.saturating_sub(now_unix)
    }
}

/// `None` when the platform exposes no config directory at all.
pub fn path() -> Option<PathBuf> {
    directories::ProjectDirs::from("br.com", "freedomit", "vorcall")
        .map(|dirs| dirs.config_dir().join(FILE))
}

/// `Ok(None)` means "nobody is signed in"; an unreadable or malformed file is
/// an error so the caller can tell that apart from a broken install.
pub fn load() -> anyhow::Result<Option<Session>> {
    let Some(path) = path() else {
        return Ok(None);
    };

    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).with_context(|| format!("cannot read {}", path.display()));
        }
    };

    let session: Session =
        toml::from_str(&raw).with_context(|| format!("cannot parse {}", path.display()))?;
    Ok(Some(session))
}

/// Writes the tokens through a temporary file so a crash never leaves a
/// half-written `session.toml` behind, and so the tokens are never briefly
/// world-readable.
pub fn save(session: &Session) -> anyhow::Result<()> {
    let path = path().context("this platform exposes no configuration directory")?;
    let parent = path
        .parent()
        .context("the session path has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("cannot create {}", parent.display()))?;

    let raw = toml::to_string_pretty(session).context("cannot serialize the session")?;
    let temp = parent.join(TEMP_FILE);
    // `mode` only applies to a file this call creates, so a leftover from an
    // earlier crash could keep permissions we did not choose.
    let _ = std::fs::remove_file(&temp);

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }

    let mut file = options
        .open(&temp)
        .with_context(|| format!("cannot write {}", temp.display()))?;
    file.write_all(raw.as_bytes())
        .with_context(|| format!("cannot write {}", temp.display()))?;
    file.sync_all()
        .with_context(|| format!("cannot flush {}", temp.display()))?;
    drop(file);

    std::fs::rename(&temp, &path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

/// Signing out. A session file that is already gone is success.
pub fn delete() -> anyhow::Result<()> {
    let Some(path) = path() else {
        return Ok(());
    };

    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("cannot remove {}", path.display())),
    }
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}
