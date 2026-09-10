//! On-disk client configuration. The only thing we persist is the nickname;
//! messages are never stored on the client.

use std::io::ErrorKind;
use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub nickname: String,
}

/// `None` when the platform exposes no config directory at all.
pub fn path() -> Option<PathBuf> {
    directories::ProjectDirs::from("br.com", "freedomit", "vorcall")
        .map(|dirs| dirs.config_dir().join("config.toml"))
}

/// `Ok(None)` means "no config yet"; an unreadable or malformed file is an error
/// so the caller can tell a first run apart from a broken install.
pub fn load() -> anyhow::Result<Option<Config>> {
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

    let config: Config =
        toml::from_str(&raw).with_context(|| format!("cannot parse {}", path.display()))?;
    Ok(Some(config))
}

pub fn save(config: &Config) -> anyhow::Result<()> {
    let path = path().context("this platform exposes no configuration directory")?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }

    let raw = toml::to_string_pretty(config).context("cannot serialize the configuration")?;
    std::fs::write(&path, raw).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}
