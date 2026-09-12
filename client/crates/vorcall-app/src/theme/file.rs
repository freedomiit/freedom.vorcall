//! Custom themes on disk: `themes/<slug>.json` beside the configuration.
//!
//! A theme file is the token object of [`ThemeTokens`] and nothing else, so a
//! theme can be written by hand, mailed to a friend and dropped in.

use std::path::PathBuf;

use anyhow::Context as _;

use super::presets::{self, VORCALL_DARK};
use super::tokens::ThemeTokens;

/// What `Config::theme` puts in front of a custom theme's slug.
pub const CUSTOM_PREFIX: &str = "custom:";

/// Where custom themes live; `None` when the platform exposes no configuration
/// directory, which only means custom themes are unavailable.
pub fn dir() -> Option<PathBuf> {
    let config = vorcall_core::config::path()?;
    Some(config.parent()?.join("themes"))
}

/// The file one slug is stored as.
pub fn path(slug: &str) -> Option<PathBuf> {
    Some(dir()?.join(format!("{slug}.json")))
}

/// Every custom theme on disk as `(slug, name)`, by slug. A file that cannot be
/// read is left out rather than reported: the list is for a pick list.
pub fn list() -> Vec<(String, String)> {
    let Some(dir) = dir() else {
        return Vec::new();
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        // Nobody has saved a theme yet, which is the common case.
        Err(e) => {
            tracing::debug!(error = %e, "no themes directory");
            return Vec::new();
        }
    };

    let mut themes: Vec<(String, String)> = entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| {
            let stem = entry.path().file_stem()?.to_str()?.to_owned();
            (!stem.is_empty()).then(|| (stem.clone(), display_name(&stem)))
        })
        .collect();
    themes.sort();
    themes.dedup();
    themes
}

/// Reads one custom theme. Every token has to be there.
pub fn load(slug: &str) -> anyhow::Result<ThemeTokens> {
    let path = path(slug).context("there is no configuration directory")?;
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let tokens: ThemeTokens =
        serde_json::from_str(&raw).with_context(|| format!("{} is not a theme", path.display()))?;
    Ok(tokens)
}

/// Writes one custom theme, creating the directory on the way.
pub fn save(slug: &str, tokens: &ThemeTokens) -> anyhow::Result<()> {
    let dir = dir().context("there is no configuration directory")?;
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let path = dir.join(format!("{slug}.json"));
    let body = serde_json::to_string_pretty(tokens).context("cannot encode the theme")?;
    std::fs::write(&path, body).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

/// The file name a theme the user named gets: lowercase, one dash between runs
/// of anything that is not a letter or a digit.
pub fn slugify(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-').to_owned();
    if slug.is_empty() {
        "theme".to_owned()
    } else {
        slug
    }
}

/// What a slug is called in the interface, since the file holds tokens only.
pub fn display_name(slug: &str) -> String {
    let mut name = String::with_capacity(slug.len());
    for (index, word) in slug.split('-').filter(|word| !word.is_empty()).enumerate() {
        if index > 0 {
            name.push(' ');
        }
        let mut characters = word.chars();
        if let Some(first) = characters.next() {
            name.extend(first.to_uppercase());
            name.push_str(characters.as_str());
        }
    }
    if name.is_empty() {
        slug.to_owned()
    } else {
        name
    }
}

/// The tokens `Config::theme` names. A theme that is gone or broken is the dark
/// preset: the window always has a theme.
pub fn resolve(theme: &str) -> ThemeTokens {
    if let Some(preset) = presets::by_name(theme) {
        return *preset;
    }
    let Some(slug) = theme.strip_prefix(CUSTOM_PREFIX) else {
        if !theme.is_empty() {
            tracing::warn!(theme, "unknown theme, using Vorcall Dark");
        }
        return VORCALL_DARK;
    };
    match load(slug) {
        Ok(tokens) => tokens,
        Err(e) => {
            let detail = format!("{e:#}");
            tracing::warn!(slug, error = %detail, "cannot read the theme, using Vorcall Dark");
            VORCALL_DARK
        }
    }
}

/// What `Config::theme` stores for a custom theme.
pub fn custom_name(slug: &str) -> String {
    format!("{CUSTOM_PREFIX}{slug}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::presets::{LIGHT_SLUG, VORCALL_LIGHT};

    #[test]
    fn resolve_falls_back_to_dark() {
        assert_eq!(resolve(LIGHT_SLUG), VORCALL_LIGHT);
        assert_eq!(resolve("vorcall-dark"), VORCALL_DARK);
        // Neither a preset nor a readable file.
        assert_eq!(resolve(""), VORCALL_DARK);
        assert_eq!(resolve("solarized"), VORCALL_DARK);
        assert_eq!(
            resolve("custom:nothing-saved-under-this-name"),
            VORCALL_DARK
        );
    }

    #[test]
    fn a_name_becomes_one_file_name() {
        assert_eq!(slugify("Midnight Oil"), "midnight-oil");
        assert_eq!(slugify("  olá/mundo  "), "ol-mundo");
        assert_eq!(slugify("###"), "theme");
        assert_eq!(slugify(""), "theme");
        assert_eq!(custom_name("midnight-oil"), "custom:midnight-oil");
    }

    #[test]
    fn a_slug_reads_back_as_a_name() {
        assert_eq!(display_name("midnight-oil"), "Midnight Oil");
        assert_eq!(display_name("mine"), "Mine");
        assert_eq!(display_name(""), "");
    }
}
