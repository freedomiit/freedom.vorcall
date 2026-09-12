//! On-disk client preferences: who signed in last and how the client behaves.
//! Tokens live in `session.toml` instead, and messages are never stored.

use std::collections::BTreeMap;
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

/// How audio leaves the machine: while the push-to-talk binding is held, or whenever the noise gate is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TransmitMode {
    #[default]
    PushToTalk,
    VoiceActivation,
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
    #[serde(default = "default_true")]
    pub notifications: bool,
    #[serde(default)]
    pub sound: bool,
    /// Input device name as reported by the audio host; `None` = system default.
    #[serde(default)]
    pub input_device: Option<String>,
    #[serde(default)]
    pub output_device: Option<String>,
    /// An iced `keyboard::key::Named` variant name ("Control", "F8", "Space", ...),
    /// a single ASCII letter or digit, or one of "MouseBack", "MouseForward",
    /// "MouseMiddle". Left/right location is ignored.
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
    /// Keyed by the peer's user id in decimal — TOML table keys are strings.
    #[serde(default)]
    pub peer_audio: BTreeMap<String, PeerAudio>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            username: String::new(),
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
            share_resolution: SHARE_DEFAULT_RESOLUTION.to_owned(),
            share_fps: SHARE_DEFAULT_FPS,
            share_bitrate_kbps: None,
            share_audio: true,
            share_volume: 1.0,
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
}

/// `None` when the platform exposes no config directory at all.
pub fn path() -> Option<PathBuf> {
    log_dir().map(|dir| dir.join("config.toml"))
}

/// The directory holding `config.toml`, which is also where the rolling log and
/// the crash reports go; `None` when the platform exposes no config directory
/// at all.
pub fn log_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("br.com", "freedomit", "vorcall")
        .map(|dirs| dirs.config_dir().to_path_buf())
}

/// Where anything the client can re-fetch belongs; `None` when the platform
/// exposes no cache directory at all.
pub fn cache_dir() -> Option<PathBuf> {
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

    let raw = toml::to_string_pretty(config).context("cannot serialize the configuration")?;
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
            ..Default::default()
        };
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
}
