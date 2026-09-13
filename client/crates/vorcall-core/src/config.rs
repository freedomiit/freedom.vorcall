//! On-disk client preferences: who signed in last, how the client behaves and
//! how it looks. Tokens live in `session.toml` instead, and messages are never
//! stored.

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};

pub const DEFAULT_PTT_KEY: &str = "Control";

pub const VAD_MIN_DB: f32 = -60.0;
pub const VAD_MAX_DB: f32 = -20.0;
pub const VAD_DEFAULT_DB: f32 = -45.0;

/// What the settings screen offers for a screen share, coarsest last; "source"
/// keeps the captured size.
pub const SHARE_RESOLUTIONS: [&str; 5] = ["source", "720p", "1080p", "1440p", "2160p"];
pub const SHARE_DEFAULT_RESOLUTION: &str = "720p";
pub const SHARE_FRAME_RATES: [u32; 3] = [15, 30, 60];
pub const SHARE_DEFAULT_FPS: u32 = 30;
pub const SHARE_MIN_BITRATE_KBPS: u32 = 1_000;
pub const SHARE_MAX_BITRATE_KBPS: u32 = 30_000;

/// The built-in dark preset; a custom theme is `custom:<slug>` and names a file
/// under the config directory.
pub const DEFAULT_THEME: &str = "vorcall-dark";

/// The built-in light preset, the only other theme that is not a file.
pub const LIGHT_THEME: &str = "vorcall-light";

pub const FONT_SCALE_MIN: f32 = 0.85;
pub const FONT_SCALE_MAX: f32 = 1.3;
pub const FONT_SCALE_DEFAULT: f32 = 1.0;

/// How wide the channel and member panes may be dragged.
pub const PANE_MIN_WIDTH: f32 = 180.0;
pub const PANE_MAX_WIDTH: f32 = 360.0;
pub const DEFAULT_PANE_WIDTH: f32 = 240.0;

/// The action id push-to-talk is stored under. Before 0.5.0 it lived in
/// [`Config::ptt_key`], which is still read and still written.
pub const PUSH_TO_TALK: &str = "push_to_talk";

/// Every rebindable action: its id, its default binding and whether the binding
/// is captured system-wide (the rest only fire while a Vorcall window has
/// focus).
///
/// A binding is `[Ctrl+][Shift+][Alt+]<key>`, where `<key>` is an iced
/// `keyboard::key::Named` variant name, a single ASCII letter or digit, the
/// punctuation character a name like "Comma" stands for, or one of "MouseBack",
/// "MouseForward", "MouseMiddle". "Ctrl+," and "Ctrl+Comma" are therefore the
/// same binding, and a binding is saved back under the name.
///
/// The modifiers a binding asks for only have to be held, not held alone, so one
/// key event can match several bindings: `Alt+Shift+ArrowDown` matches
/// `Alt+ArrowDown` too. The most specific match wins — the binding asking for the
/// most modifiers — which is why the two default arrow chords can sit on one
/// arrow key. Two actions on the very same binding both fire, and that is what
/// [`Config::keybind_conflicts`] reports.
pub const KEYBIND_ACTIONS: [(&str, &str, bool); 12] = [
    (PUSH_TO_TALK, DEFAULT_PTT_KEY, true),
    ("toggle_mute", "Ctrl+Shift+M", true),
    ("toggle_deafen", "Ctrl+Shift+D", true),
    ("quick_switcher", "Ctrl+K", false),
    ("settings", "Ctrl+Comma", false),
    ("prev_channel", "Alt+ArrowUp", false),
    ("next_channel", "Alt+ArrowDown", false),
    // The prefixes only parse in the one order the grammar accepts, so these two
    // are spelled "Shift+Alt+" and not the other way round.
    ("next_unread", "Shift+Alt+ArrowDown", false),
    ("prev_unread", "Shift+Alt+ArrowUp", false),
    ("toggle_members", "Ctrl+U", false),
    ("edit_last", "ArrowUp", false),
    ("escape", "Escape", false),
];

/// How audio leaves the machine: while the push-to-talk binding is held, or whenever the noise gate is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TransmitMode {
    #[default]
    PushToTalk,
    VoiceActivation,
}

/// How tightly the message list and the sidebars are packed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Density {
    #[default]
    Cosy,
    Compact,
}

/// Which splash animation a launch plays. `VORCALL_ENTRANCE` still overrides it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Entrance {
    #[default]
    Random,
    Wink,
    Rare,
    Off,
}

/// Local playback settings for one other user. Never leaves this machine.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PeerAudio {
    /// 0.0..=2.0 (0 %..200 %).
    #[serde(default = "default_volume")]
    pub volume: f32,
    #[serde(default)]
    pub muted: bool,
}

impl Default for PeerAudio {
    fn default() -> Self {
        Self {
            volume: 1.0,
            muted: false,
        }
    }
}

fn default_volume() -> f32 {
    1.0
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    /// `nickname` is what pre-accounts builds wrote here.
    #[serde(default, alias = "nickname")]
    pub username: String,
    /// The self-hosted server this client talks to, set from the sign-in
    /// screen's "Server" section. `None` = the address the build baked in. An
    /// environment variable of the same name still wins over both; see
    /// [`crate::endpoints`].
    #[serde(default)]
    pub server_url: Option<String>,
    /// The pre-shared door key for [`Self::server_url`]. Not a credential of the
    /// account — every client of a given server carries the same one.
    #[serde(default)]
    pub server_key: Option<String>,
    #[serde(default = "default_true")]
    pub notifications: bool,
    #[serde(default)]
    pub sound: bool,
    /// Input device name as reported by the audio host; `None` = system default.
    #[serde(default)]
    pub input_device: Option<String>,
    #[serde(default)]
    pub output_device: Option<String>,
    /// Where push-to-talk lived before 0.5.0. `keybinds` is the home now;
    /// `normalize` folds this in and `save` writes it back from the map, so a
    /// rollback to an older client keeps the binding.
    #[serde(default = "default_ptt_key")]
    pub ptt_key: String,
    #[serde(default)]
    pub transmit_mode: TransmitMode,
    /// Noise-gate threshold in dBFS for voice activation; clamped to
    /// VAD_MIN_DB..=VAD_MAX_DB on load.
    #[serde(default = "default_vad_threshold_db")]
    pub vad_threshold_db: f32,
    /// The settings screen's "Input cleanup" switches: noise suppression and
    /// echo cancellation default on, automatic gain off. Applied live by the
    /// audio thread.
    #[serde(default = "default_true")]
    pub noise_suppression: bool,
    #[serde(default = "default_true")]
    pub echo_cancellation: bool,
    #[serde(default)]
    pub auto_gain: bool,
    /// Whether a priority speaker quietens every other voice while they talk;
    /// off by default. A playback preference on this machine only: the server
    /// still marks priority speakers and the member pane still shows them.
    #[serde(default)]
    pub priority_ducking: bool,
    /// One of [`SHARE_RESOLUTIONS`]; anything else falls back to the default on load.
    #[serde(default = "default_share_resolution")]
    pub share_resolution: String,
    /// One of [`SHARE_FRAME_RATES`].
    #[serde(default = "default_share_fps")]
    pub share_fps: u32,
    /// `None` = let the encoder pick from resolution and frame rate; a value is
    /// clamped to SHARE_MIN_BITRATE_KBPS..=SHARE_MAX_BITRATE_KBPS on load.
    #[serde(default)]
    pub share_bitrate_kbps: Option<u32>,
    /// Whether a share the local user starts carries audio.
    #[serde(default = "default_true")]
    pub share_audio: bool,
    /// Playback volume for a watched share, 0.0..=2.0.
    #[serde(default = "default_volume")]
    pub share_volume: f32,
    /// `"vorcall-dark"`, `"vorcall-light"` or `"custom:<slug>"`.
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default)]
    pub density: Density,
    /// Multiplies every text size; clamped to FONT_SCALE_MIN..=FONT_SCALE_MAX.
    #[serde(default = "default_font_scale")]
    pub font_scale: f32,
    #[serde(default)]
    pub entrance: Entrance,
    /// Draw the reaction palette as text labels instead of emoji, for a machine
    /// whose fonts cannot render them. `VORCALL_TEXT_REACTIONS` still overrides.
    #[serde(default)]
    pub text_reactions: bool,
    /// Never notify for `@everyone` or `@here`.
    #[serde(default)]
    pub suppress_everyone: bool,
    #[serde(default = "default_true")]
    pub show_members: bool,
    /// Channel pane width, clamped to PANE_MIN_WIDTH..=PANE_MAX_WIDTH.
    #[serde(default = "default_pane_width")]
    pub sidebar_width: f32,
    /// Member pane width, same range.
    #[serde(default = "default_pane_width")]
    pub members_width: f32,
    /// The channel to reopen on the next launch; `0` = none yet.
    #[serde(default)]
    pub last_channel: i64,
    /// Channels whose messages raise no notification.
    #[serde(default)]
    pub muted_channels: BTreeSet<i64>,
    /// Categories the sidebar draws collapsed.
    #[serde(default)]
    pub collapsed_categories: BTreeSet<i64>,
    /// DM channels the user closed; reopened by a new message or by the member
    /// list.
    #[serde(default)]
    pub hidden_dms: BTreeSet<i64>,
    /// Action id → binding, for the actions whose binding is not the default.
    /// Read through [`Config::keybind`], which falls back to
    /// [`KEYBIND_ACTIONS`].
    #[serde(default)]
    pub keybinds: BTreeMap<String, String>,
    /// Keyed by the peer's user id in decimal — TOML table keys are strings.
    #[serde(default)]
    pub peer_audio: BTreeMap<String, PeerAudio>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            username: String::new(),
            server_url: None,
            server_key: None,
            notifications: true,
            sound: false,
            input_device: None,
            output_device: None,
            ptt_key: DEFAULT_PTT_KEY.to_owned(),
            transmit_mode: TransmitMode::default(),
            vad_threshold_db: VAD_DEFAULT_DB,
            noise_suppression: true,
            echo_cancellation: true,
            auto_gain: false,
            priority_ducking: false,
            share_resolution: SHARE_DEFAULT_RESOLUTION.to_owned(),
            share_fps: SHARE_DEFAULT_FPS,
            share_bitrate_kbps: None,
            share_audio: true,
            share_volume: 1.0,
            theme: DEFAULT_THEME.to_owned(),
            density: Density::default(),
            font_scale: FONT_SCALE_DEFAULT,
            entrance: Entrance::default(),
            text_reactions: false,
            suppress_everyone: false,
            show_members: true,
            sidebar_width: DEFAULT_PANE_WIDTH,
            members_width: DEFAULT_PANE_WIDTH,
            last_channel: 0,
            muted_channels: BTreeSet::new(),
            collapsed_categories: BTreeSet::new(),
            hidden_dms: BTreeSet::new(),
            keybinds: BTreeMap::new(),
            peer_audio: BTreeMap::new(),
        }
    }
}

impl Config {
    pub fn peer_audio(&self, user_id: i64) -> PeerAudio {
        self.peer_audio
            .get(&user_id.to_string())
            .copied()
            .unwrap_or_default()
    }

    /// Stores `audio`; an entry equal to the default is removed so the file stays small.
    pub fn set_peer_audio(&mut self, user_id: i64, mut audio: PeerAudio) {
        audio.volume = audio.volume.clamp(0.0, 2.0);
        let key = user_id.to_string();
        if audio == PeerAudio::default() {
            self.peer_audio.remove(&key);
        } else {
            self.peer_audio.insert(key, audio);
        }
    }

    /// The binding in force for `action`: what the user set, or the default from
    /// [`KEYBIND_ACTIONS`]. Empty for an action this build does not know.
    pub fn keybind(&self, action: &str) -> &str {
        match self.keybinds.get(action) {
            Some(binding) => binding,
            None => default_keybind(action),
        }
    }

    /// Rebinds `action`. An empty binding restores its default; an unknown
    /// action is ignored, since loading would drop it anyway.
    pub fn set_keybind(&mut self, action: &str, binding: &str) {
        let default = default_keybind(action);
        if default.is_empty() {
            return;
        }

        let binding = binding.trim();
        let binding = if binding.is_empty() { default } else { binding };
        self.keybinds.insert(action.to_owned(), binding.to_owned());
        if action == PUSH_TO_TALK {
            self.ptt_key = binding.to_owned();
        }
    }

    /// Pairs of actions sharing a binding, in [`KEYBIND_ACTIONS`] order. The
    /// keybinds tab paints both sides of every pair as a warning; nothing here
    /// refuses the binding.
    ///
    /// Sharing means the very same binding, however it is spelled: a binding with
    /// more modifiers on the same key is not a clash but the more specific match,
    /// and wins the press outright.
    pub fn keybind_conflicts(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();

        for (index, (first, _, _)) in KEYBIND_ACTIONS.iter().enumerate() {
            let binding = self.keybind(first);
            if binding.is_empty() {
                continue;
            }
            let binding = canonical_binding(binding);
            for (second, _, _) in KEYBIND_ACTIONS.iter().skip(index + 1) {
                if canonical_binding(self.keybind(second)) == binding {
                    out.push(((*first).to_owned(), (*second).to_owned()));
                }
            }
        }

        out
    }
}

/// One binding as it is compared against another: lowercased, with the
/// punctuation characters folded onto the names they alias, so that "Ctrl+," and
/// "Ctrl+Comma" are one binding. Only ever compared against another binding read
/// the same way, never stored or shown.
fn canonical_binding(binding: &str) -> String {
    let mut rest = binding.trim();
    let mut canonical = String::new();
    for prefix in ["ctrl+", "shift+", "alt+"] {
        if rest
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        {
            canonical.push_str(prefix);
            rest = &rest[prefix.len()..];
        }
    }
    canonical.push_str(&punctuation_name(rest).to_ascii_lowercase());
    canonical
}

/// The name the grammar spells a punctuation key with; anything else is already
/// its own spelling.
fn punctuation_name(key: &str) -> &str {
    match key {
        "," => "Comma",
        "." => "Period",
        "/" => "Slash",
        ";" => "Semicolon",
        "-" => "Minus",
        "=" => "Equal",
        "[" => "BracketLeft",
        "]" => "BracketRight",
        "`" => "Backquote",
        "'" => "Quote",
        "\\" => "Backslash",
        other => other,
    }
}

/// The stored theme, if it is one of the three forms a theme can take: the two
/// presets, and `custom:<slug>` naming a file under the config directory. A slug
/// is lowercase letters, digits and dashes, so nothing here can reach outside
/// that directory.
fn known_theme(theme: &str) -> bool {
    if theme == DEFAULT_THEME || theme == LIGHT_THEME {
        return true;
    }
    theme.strip_prefix("custom:").is_some_and(|slug| {
        !slug.is_empty()
            && slug
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    })
}

fn default_true() -> bool {
    true
}

fn default_ptt_key() -> String {
    DEFAULT_PTT_KEY.to_owned()
}

fn default_vad_threshold_db() -> f32 {
    VAD_DEFAULT_DB
}

fn default_share_resolution() -> String {
    SHARE_DEFAULT_RESOLUTION.to_owned()
}

fn default_share_fps() -> u32 {
    SHARE_DEFAULT_FPS
}

fn default_theme() -> String {
    DEFAULT_THEME.to_owned()
}

fn default_font_scale() -> f32 {
    FONT_SCALE_DEFAULT
}

fn default_pane_width() -> f32 {
    DEFAULT_PANE_WIDTH
}

fn default_keybind(action: &str) -> &'static str {
    KEYBIND_ACTIONS
        .iter()
        .find(|(id, _, _)| *id == action)
        .map(|(_, binding, _)| *binding)
        .unwrap_or_default()
}

fn pane_width(value: f32) -> f32 {
    if value.is_nan() {
        DEFAULT_PANE_WIDTH
    } else {
        value.clamp(PANE_MIN_WIDTH, PANE_MAX_WIDTH)
    }
}

/// Clamps values that could only have reached here from a hand-edited or
/// stale `config.toml`, so every other caller can trust the ranges hold.
fn normalize(config: &mut Config) {
    config.vad_threshold_db = if config.vad_threshold_db.is_nan() {
        VAD_DEFAULT_DB
    } else {
        config.vad_threshold_db.clamp(VAD_MIN_DB, VAD_MAX_DB)
    };

    let resolution = config.share_resolution.to_ascii_lowercase();
    config.share_resolution = SHARE_RESOLUTIONS
        .iter()
        .find(|known| **known == resolution)
        .map(|known| (*known).to_owned())
        .unwrap_or_else(default_share_resolution);

    if !SHARE_FRAME_RATES.contains(&config.share_fps) {
        config.share_fps = SHARE_DEFAULT_FPS;
    }

    config.share_bitrate_kbps = config
        .share_bitrate_kbps
        .map(|kbps| kbps.clamp(SHARE_MIN_BITRATE_KBPS, SHARE_MAX_BITRATE_KBPS));

    config.share_volume = if config.share_volume.is_nan() {
        1.0
    } else {
        config.share_volume.clamp(0.0, 2.0)
    };

    for audio in config.peer_audio.values_mut() {
        audio.volume = if audio.volume.is_nan() {
            1.0
        } else {
            audio.volume.clamp(0.0, 2.0)
        };
    }

    let theme = config.theme.trim();
    config.theme = if known_theme(theme) {
        theme.to_owned()
    } else {
        DEFAULT_THEME.to_owned()
    };

    config.font_scale = if config.font_scale.is_nan() {
        FONT_SCALE_DEFAULT
    } else {
        config.font_scale.clamp(FONT_SCALE_MIN, FONT_SCALE_MAX)
    };

    config.sidebar_width = pane_width(config.sidebar_width);
    config.members_width = pane_width(config.members_width);

    // An action this build does not know, or an entry a hand edit emptied, would
    // only ever shadow a default.
    config.keybinds = std::mem::take(&mut config.keybinds)
        .into_iter()
        .filter(|(action, _)| !default_keybind(action).is_empty())
        .map(|(action, binding)| (action, binding.trim().to_owned()))
        .filter(|(_, binding)| !binding.is_empty())
        .collect();

    // Pre-0.5.0 files carry the push-to-talk binding in `ptt_key` alone; the map
    // is the source of truth from here on, and `save` writes the old key back
    // from it.
    let ptt_key = config.ptt_key.trim().to_owned();
    let ptt_key = if ptt_key.is_empty() {
        DEFAULT_PTT_KEY.to_owned()
    } else {
        ptt_key
    };
    config
        .keybinds
        .entry(PUSH_TO_TALK.to_owned())
        .or_insert(ptt_key);
    let in_force = config.keybind(PUSH_TO_TALK).to_owned();
    config.ptt_key = in_force;
}

/// `None` when the platform exposes no config directory at all.
pub fn path() -> Option<PathBuf> {
    log_dir().map(|dir| dir.join("config.toml"))
}

/// The directory holding `config.toml`, which is also where the rolling log and
/// the crash reports go; `None` when the platform exposes no config directory
/// at all.
pub fn log_dir() -> Option<PathBuf> {
    // These three strings pick the platform config/cache/log directory for
    // every existing install; they are not placeholders to tidy up. Change any
    // of them and an existing user's config.toml silently stops resolving —
    // indistinguishable from a fresh install, so their settings are gone and
    // they're signed out. "freedomit" is the real org id, not the kind of
    // identifier that gets scrubbed from a public repo.
    directories::ProjectDirs::from("br.com", "freedomit", "vorcall")
        .map(|dirs| dirs.config_dir().to_path_buf())
}

/// Where anything the client can re-fetch belongs; `None` when the platform
/// exposes no cache directory at all.
pub fn cache_dir() -> Option<PathBuf> {
    // Same load-bearing strings as `log_dir` above — see that comment.
    directories::ProjectDirs::from("br.com", "freedomit", "vorcall")
        .map(|dirs| dirs.cache_dir().to_path_buf())
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

    let mut config: Config =
        toml::from_str(&raw).with_context(|| format!("cannot parse {}", path.display()))?;
    normalize(&mut config);
    Ok(Some(config))
}

pub fn save(config: &Config) -> anyhow::Result<()> {
    let path = path().context("this platform exposes no configuration directory")?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }

    let mut synced = config.clone();
    let in_force = synced.keybind(PUSH_TO_TALK).to_owned();
    synced.ptt_key = in_force;

    let raw = toml::to_string_pretty(&synced).context("cannot serialize the configuration")?;
    std::fs::write(&path, raw).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_push_to_talk_at_minus_forty_five() {
        let config = Config::default();
        assert_eq!(config.transmit_mode, TransmitMode::PushToTalk);
        assert_eq!(config.vad_threshold_db, VAD_DEFAULT_DB);
        assert!(config.peer_audio.is_empty());
        assert!(config.noise_suppression);
        assert!(config.echo_cancellation);
        assert!(!config.auto_gain);
        assert!(!config.priority_ducking);
    }

    #[test]
    fn defaults_cover_every_appearance_and_layout_key() {
        let config = Config::default();
        assert_eq!(config.theme, DEFAULT_THEME);
        assert_eq!(config.density, Density::Cosy);
        assert_eq!(config.font_scale, 1.0);
        assert_eq!(config.entrance, Entrance::Random);
        assert!(!config.text_reactions);
        assert!(!config.suppress_everyone);
        assert!(config.show_members);
        assert_eq!(config.sidebar_width, 240.0);
        assert_eq!(config.members_width, 240.0);
        assert_eq!(config.last_channel, 0);
        assert!(config.muted_channels.is_empty());
        assert!(config.collapsed_categories.is_empty());
        assert!(config.hidden_dms.is_empty());
        assert!(config.keybinds.is_empty());
    }

    #[test]
    fn a_config_without_the_new_keys_still_loads() {
        let raw = r#"
            username = "x"
            ptt_key = "F8"
        "#;
        let mut config: Config = toml::from_str(raw).expect("parses a config missing new keys");
        normalize(&mut config);

        assert_eq!(config.ptt_key, "F8");
        assert_eq!(config.transmit_mode, TransmitMode::PushToTalk);
        assert_eq!(config.vad_threshold_db, VAD_DEFAULT_DB);
        assert!(config.peer_audio.is_empty());
        assert!(config.noise_suppression);
        assert!(config.echo_cancellation);
        assert!(!config.auto_gain);
        assert!(!config.priority_ducking);

        assert_eq!(config.theme, DEFAULT_THEME);
        assert_eq!(config.density, Density::Cosy);
        assert_eq!(config.font_scale, FONT_SCALE_DEFAULT);
        assert_eq!(config.entrance, Entrance::Random);
        assert!(!config.text_reactions);
        assert!(!config.suppress_everyone);
        assert!(config.show_members);
        assert_eq!(config.sidebar_width, DEFAULT_PANE_WIDTH);
        assert_eq!(config.members_width, DEFAULT_PANE_WIDTH);
        assert_eq!(config.last_channel, 0);
        assert!(config.muted_channels.is_empty());
        assert!(config.collapsed_categories.is_empty());
        assert!(config.hidden_dms.is_empty());
    }

    #[test]
    fn a_config_without_the_share_keys_still_loads() {
        let raw = r#"
            username = "x"
            ptt_key = "F8"
        "#;
        let mut config: Config = toml::from_str(raw).expect("parses a config missing share keys");
        normalize(&mut config);

        assert_eq!(config.share_resolution, SHARE_DEFAULT_RESOLUTION);
        assert_eq!(config.share_fps, SHARE_DEFAULT_FPS);
        assert_eq!(config.share_bitrate_kbps, None);
        assert!(config.share_audio);
        assert_eq!(config.share_volume, 1.0);
    }

    #[test]
    fn the_old_ptt_key_becomes_the_push_to_talk_binding() {
        let raw = r#"
            username = "x"
            ptt_key = "F8"
        "#;
        let mut config: Config = toml::from_str(raw).expect("parses a pre-0.5.0 config");
        normalize(&mut config);

        assert_eq!(config.keybind(PUSH_TO_TALK), "F8");
        assert_eq!(
            config.keybinds.get(PUSH_TO_TALK).map(String::as_str),
            Some("F8")
        );
        assert_eq!(config.ptt_key, "F8");
    }

    #[test]
    fn the_keybind_map_wins_over_a_stale_ptt_key() {
        let raw = r#"
            username = "x"
            ptt_key = "F8"

            [keybinds]
            push_to_talk = "F9"
        "#;
        let mut config: Config = toml::from_str(raw).expect("parses both homes of the binding");
        normalize(&mut config);

        assert_eq!(config.keybind(PUSH_TO_TALK), "F9");
        assert_eq!(config.ptt_key, "F9");
    }

    #[test]
    fn an_empty_ptt_key_falls_back_to_the_default() {
        let raw = r#"
            ptt_key = "  "
        "#;
        let mut config: Config = toml::from_str(raw).expect("parses an emptied binding");
        normalize(&mut config);

        assert_eq!(config.keybind(PUSH_TO_TALK), DEFAULT_PTT_KEY);
        assert_eq!(config.ptt_key, DEFAULT_PTT_KEY);
    }

    #[test]
    fn every_action_has_a_default_binding_and_a_scope() {
        let config = Config::default();
        for (action, binding, _) in KEYBIND_ACTIONS {
            assert!(!binding.is_empty(), "{action} has no default binding");
            assert_eq!(config.keybind(action), binding);
        }
        assert_eq!(config.keybind("not_an_action"), "");

        let global: Vec<&str> = KEYBIND_ACTIONS
            .iter()
            .filter(|(_, _, global)| *global)
            .map(|(action, _, _)| *action)
            .collect();
        assert_eq!(global, vec![PUSH_TO_TALK, "toggle_mute", "toggle_deafen"]);

        let mut ids: Vec<&str> = KEYBIND_ACTIONS.iter().map(|(id, _, _)| *id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "two actions share an id");
    }

    #[test]
    fn set_keybind_keeps_the_old_key_in_step_and_ignores_the_unknown() {
        let mut config = Config::default();

        config.set_keybind(PUSH_TO_TALK, " F10 ");
        assert_eq!(config.keybind(PUSH_TO_TALK), "F10");
        assert_eq!(config.ptt_key, "F10");

        config.set_keybind(PUSH_TO_TALK, "");
        assert_eq!(config.keybind(PUSH_TO_TALK), DEFAULT_PTT_KEY);
        assert_eq!(config.ptt_key, DEFAULT_PTT_KEY);

        config.set_keybind("quick_switcher", "Ctrl+J");
        assert_eq!(config.keybind("quick_switcher"), "Ctrl+J");

        config.set_keybind("not_an_action", "Ctrl+Z");
        assert!(!config.keybinds.contains_key("not_an_action"));
    }

    #[test]
    fn normalize_drops_unknown_and_empty_keybinds() {
        let mut config = Config::default();
        config
            .keybinds
            .insert("not_an_action".to_owned(), "Ctrl+Z".to_owned());
        config
            .keybinds
            .insert("quick_switcher".to_owned(), "  ".to_owned());
        config
            .keybinds
            .insert("settings".to_owned(), "  Ctrl+P  ".to_owned());
        normalize(&mut config);

        assert!(!config.keybinds.contains_key("not_an_action"));
        assert!(!config.keybinds.contains_key("quick_switcher"));
        assert_eq!(config.keybind("quick_switcher"), "Ctrl+K");
        assert_eq!(config.keybind("settings"), "Ctrl+P");
    }

    #[test]
    fn the_defaults_conflict_with_nothing() {
        assert!(Config::default().keybind_conflicts().is_empty());
    }

    #[test]
    fn keybind_conflicts_ignore_case() {
        let mut config = Config::default();
        config.set_keybind("quick_switcher", "ctrl+u");

        assert_eq!(
            config.keybind_conflicts(),
            vec![("quick_switcher".to_owned(), "toggle_members".to_owned())]
        );
    }

    /// The punctuation character and the name it aliases are one binding, so a
    /// file written by an earlier build clashes the way the name does.
    #[test]
    fn a_punctuation_character_is_the_binding_its_name_spells() {
        let mut config = Config::default();
        config.set_keybind("quick_switcher", "Ctrl+,");

        assert_eq!(config.keybind("settings"), "Ctrl+Comma");
        assert_eq!(
            config.keybind_conflicts(),
            vec![("quick_switcher".to_owned(), "settings".to_owned())]
        );
    }

    /// A binding with more modifiers on the same key is the more specific match,
    /// not a clash: `Ctrl+M` wins its own press and `M` keeps the bare one.
    #[test]
    fn more_modifiers_on_one_key_is_no_conflict() {
        let mut config = Config::default();
        config.set_keybind("toggle_mute", "Ctrl+m");
        config.set_keybind("toggle_deafen", "m");

        assert!(config.keybind_conflicts().is_empty());
    }

    #[test]
    fn share_settings_are_normalised() {
        let mut config = Config {
            share_resolution: "1080P".to_owned(),
            share_fps: 25,
            share_bitrate_kbps: Some(90_000),
            share_volume: 7.5,
            ..Config::default()
        };
        normalize(&mut config);

        assert_eq!(config.share_resolution, "1080p");
        assert_eq!(config.share_fps, SHARE_DEFAULT_FPS);
        assert_eq!(config.share_bitrate_kbps, Some(SHARE_MAX_BITRATE_KBPS));
        assert_eq!(config.share_volume, 2.0);

        config.share_resolution = "potato".to_owned();
        config.share_bitrate_kbps = Some(10);
        config.share_volume = f32::NAN;
        normalize(&mut config);

        assert_eq!(config.share_resolution, SHARE_DEFAULT_RESOLUTION);
        assert_eq!(config.share_bitrate_kbps, Some(SHARE_MIN_BITRATE_KBPS));
        assert_eq!(config.share_volume, 1.0);
    }

    #[test]
    fn round_trips_through_toml() {
        let mut config = Config {
            transmit_mode: TransmitMode::VoiceActivation,
            vad_threshold_db: -30.0,
            echo_cancellation: false,
            auto_gain: true,
            share_resolution: "1440p".to_owned(),
            share_fps: 60,
            share_bitrate_kbps: Some(8_000),
            share_audio: false,
            share_volume: 0.75,
            theme: "custom:noir".to_owned(),
            density: Density::Compact,
            font_scale: 1.25,
            entrance: Entrance::Off,
            text_reactions: true,
            suppress_everyone: true,
            show_members: false,
            sidebar_width: 300.0,
            members_width: 200.0,
            last_channel: 12,
            muted_channels: BTreeSet::from([3, 9]),
            collapsed_categories: BTreeSet::from([1]),
            hidden_dms: BTreeSet::from([4, 5, 6]),
            ..Default::default()
        };
        config.set_keybind("quick_switcher", "Ctrl+J");
        config.set_peer_audio(
            1,
            PeerAudio {
                volume: 0.5,
                muted: false,
            },
        );
        config.set_peer_audio(
            2,
            PeerAudio {
                volume: 1.5,
                muted: true,
            },
        );
        // The load-time migration is what fills `keybinds[push_to_talk]`, so the
        // original has to have been through it too.
        normalize(&mut config);

        let raw = toml::to_string_pretty(&config).expect("serializes");
        let mut round_tripped: Config = toml::from_str(&raw).expect("parses back");
        normalize(&mut round_tripped);

        assert_eq!(round_tripped, config);
    }

    #[test]
    fn peer_audio_defaults_and_removal() {
        let mut config = Config::default();
        assert_eq!(config.peer_audio(42), PeerAudio::default());

        config.set_peer_audio(
            42,
            PeerAudio {
                volume: 0.25,
                muted: true,
            },
        );
        assert_eq!(
            config.peer_audio(42),
            PeerAudio {
                volume: 0.25,
                muted: true,
            }
        );

        config.set_peer_audio(42, PeerAudio::default());
        assert!(!config.peer_audio.contains_key("42"));

        config.set_peer_audio(
            7,
            PeerAudio {
                volume: 3.0,
                muted: false,
            },
        );
        assert_eq!(config.peer_audio(7).volume, 2.0);
    }

    #[test]
    fn normalize_clamps_out_of_range_values() {
        let mut config = Config {
            vad_threshold_db: -80.0,
            ..Config::default()
        };
        normalize(&mut config);
        assert_eq!(config.vad_threshold_db, VAD_MIN_DB);

        config.vad_threshold_db = -10.0;
        normalize(&mut config);
        assert_eq!(config.vad_threshold_db, VAD_MAX_DB);

        config.vad_threshold_db = f32::NAN;
        normalize(&mut config);
        assert_eq!(config.vad_threshold_db, VAD_DEFAULT_DB);

        config.peer_audio.insert(
            "9".to_owned(),
            PeerAudio {
                volume: 9.0,
                muted: false,
            },
        );
        normalize(&mut config);
        assert_eq!(
            config.peer_audio.get("9").expect("entry present").volume,
            2.0
        );
    }

    #[test]
    fn normalize_clamps_the_appearance_ranges() {
        let mut config = Config {
            font_scale: 4.0,
            sidebar_width: 10.0,
            members_width: 4_000.0,
            theme: "  ".to_owned(),
            ..Config::default()
        };
        normalize(&mut config);
        assert_eq!(config.font_scale, FONT_SCALE_MAX);
        assert_eq!(config.sidebar_width, PANE_MIN_WIDTH);
        assert_eq!(config.members_width, PANE_MAX_WIDTH);
        assert_eq!(config.theme, DEFAULT_THEME);

        config.font_scale = 0.1;
        config.sidebar_width = f32::NAN;
        config.theme = "  custom:noir  ".to_owned();
        normalize(&mut config);
        assert_eq!(config.font_scale, FONT_SCALE_MIN);
        assert_eq!(config.sidebar_width, DEFAULT_PANE_WIDTH);
        assert_eq!(config.theme, "custom:noir");

        config.font_scale = f32::NAN;
        normalize(&mut config);
        assert_eq!(config.font_scale, FONT_SCALE_DEFAULT);
    }

    /// A theme is one of exactly three forms; anything else — a name no build
    /// knows, or a slug that would reach outside the config directory — is the
    /// default.
    #[test]
    fn normalize_keeps_only_the_three_theme_forms() {
        let theme = |stored: &str| {
            let mut config = Config {
                theme: stored.to_owned(),
                ..Config::default()
            };
            normalize(&mut config);
            config.theme
        };

        assert_eq!(theme(DEFAULT_THEME), DEFAULT_THEME);
        assert_eq!(theme(LIGHT_THEME), LIGHT_THEME);
        assert_eq!(theme("custom:my-theme"), "custom:my-theme");
        assert_eq!(theme("solarized"), DEFAULT_THEME);
        assert_eq!(theme("custom:"), DEFAULT_THEME);
        assert_eq!(theme("custom:../../x"), DEFAULT_THEME);
        assert_eq!(theme("custom:My-Theme"), DEFAULT_THEME);
        assert_eq!(theme("Vorcall-Dark"), DEFAULT_THEME);
    }

    #[test]
    fn transmit_mode_serializes_snake_case() {
        let config = Config {
            transmit_mode: TransmitMode::VoiceActivation,
            ..Default::default()
        };
        let raw = toml::to_string(&config).expect("serializes");
        assert!(raw.contains(r#"transmit_mode = "voice_activation""#));
    }

    #[test]
    fn cleanup_keys_serialize_snake_case() {
        let raw = toml::to_string(&Config::default()).expect("serializes");
        assert!(raw.contains("noise_suppression = true"));
        assert!(raw.contains("echo_cancellation = true"));
        assert!(raw.contains("auto_gain = false"));
        assert!(raw.contains("priority_ducking = false"));
    }

    /// Like `input_device`, an unset bitrate leaves no key behind at all.
    #[test]
    fn share_keys_serialize_snake_case_without_an_unset_bitrate() {
        let raw = toml::to_string(&Config::default()).expect("serializes");
        assert!(raw.contains(r#"share_resolution = "720p""#));
        assert!(raw.contains("share_fps = 30"));
        assert!(raw.contains("share_audio = true"));
        assert!(!raw.contains("share_bitrate_kbps"));
        assert!(!raw.contains("input_device"));
    }

    #[test]
    fn the_appearance_keys_serialize_snake_case() {
        let mut config = Config {
            density: Density::Compact,
            entrance: Entrance::Wink,
            muted_channels: BTreeSet::from([3]),
            ..Config::default()
        };
        normalize(&mut config);

        let raw = toml::to_string(&config).expect("serializes");
        assert!(raw.contains(r#"theme = "vorcall-dark""#));
        assert!(raw.contains(r#"density = "compact""#));
        assert!(raw.contains(r#"entrance = "wink""#));
        assert!(raw.contains("text_reactions = false"));
        assert!(raw.contains("show_members = true"));
        assert!(raw.contains("muted_channels = ["));
        assert!(raw.contains("[keybinds]"));
        assert!(raw.contains(r#"push_to_talk = "Control""#));
    }
}
