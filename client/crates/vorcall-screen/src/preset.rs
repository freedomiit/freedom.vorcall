//! What a share is worth: the box the picture is fitted into, how often it is
//! sent, and the bitrate that follows from the two.
//!
//! The table lives here rather than next to the encoder so the settings screen
//! and the encoder quote the same numbers.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

/// Bounds on a manually chosen bitrate, in kbit/s.
pub const MIN_BITRATE_KBPS: u32 = 1_000;
pub const MAX_BITRATE_KBPS: u32 = 30_000;
/// The largest picture the encoder will be asked for. OpenH264 refuses anything
/// past 3840x2160 (or 2160x3840) outright, so [`Resolution::Source`] stops here.
pub const MAX_SOURCE: (u32, u32) = (3840, 2160);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    Source,
    P720,
    P1080,
    P1440,
    P2160,
}

impl Resolution {
    /// The box the output has to fit inside.
    const fn box_size(self) -> (u32, u32) {
        match self {
            Resolution::Source => MAX_SOURCE,
            Resolution::P720 => (1280, 720),
            Resolution::P1080 => (1920, 1080),
            Resolution::P1440 => (2560, 1440),
            Resolution::P2160 => (3840, 2160),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Resolution::Source => "source",
            Resolution::P720 => "720p",
            Resolution::P1080 => "1080p",
            Resolution::P1440 => "1440p",
            Resolution::P2160 => "2160p",
        }
    }

    /// The smallest named box holding at least as many pixels as `output`,
    /// which is the row a [`Resolution::Source`] capture is billed at. Pixels
    /// rather than a width and a height, so a portrait 1080x1920 costs what
    /// 1080p costs instead of what 2160p does: it is the same picture turned on
    /// its side, and the encoder cares about area.
    fn by_pixel_count(output: (u32, u32)) -> Resolution {
        let pixels = u64::from(output.0) * u64::from(output.1);
        [Resolution::P720, Resolution::P1080, Resolution::P1440]
            .into_iter()
            .find(|candidate| {
                let (width, height) = candidate.box_size();
                pixels <= u64::from(width) * u64::from(height)
            })
            .unwrap_or(Resolution::P2160)
    }
}

impl fmt::Display for Resolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The string was none of `source`, `720p`, `1080p`, `1440p`, `2160p`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown resolution: {0}")]
pub struct UnknownResolution(pub String);

impl FromStr for Resolution {
    type Err = UnknownResolution;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let lowercase = raw.to_ascii_lowercase();
        [
            Resolution::Source,
            Resolution::P720,
            Resolution::P1080,
            Resolution::P1440,
            Resolution::P2160,
        ]
        .into_iter()
        .find(|candidate| candidate.as_str() == lowercase)
        .ok_or_else(|| UnknownResolution(raw.to_string()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameRate {
    F15 = 15,
    F30 = 30,
    F60 = 60,
}

impl FrameRate {
    pub fn hz(self) -> u32 {
        self as u32
    }

    pub fn from_hz(hz: u32) -> Option<FrameRate> {
        match hz {
            15 => Some(FrameRate::F15),
            30 => Some(FrameRate::F30),
            60 => Some(FrameRate::F60),
            _ => None,
        }
    }

    /// How long one frame lasts, which is how long a capture loop waits.
    pub fn interval(self) -> Duration {
        Duration::from_nanos(1_000_000_000 / u64::from(self.hz()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Preset {
    pub resolution: Resolution,
    pub fps: FrameRate,
    /// `None` is Auto: the table below decides.
    pub bitrate_kbps: Option<u32>,
}

impl Preset {
    /// Fits `source` inside the preset's box keeping the aspect ratio, never
    /// upscaling. Both dimensions come back even (rounded down) and at least
    /// 2x2, because H.264 chroma is subsampled by two. A portrait source is
    /// bounded by the box's height just as a landscape one is by its width.
    pub fn output_size(&self, source: (u32, u32)) -> (u32, u32) {
        let (source_width, source_height) = (source.0.max(1), source.1.max(1));
        let (box_width, box_height) = self.resolution.box_size();

        let fitted = if source_width <= box_width && source_height <= box_height {
            (source_width, source_height)
        } else {
            let by_width = (
                box_width,
                scale(source_height, box_width, source_width).max(1),
            );
            if by_width.1 <= box_height {
                by_width
            } else {
                (
                    scale(source_width, box_height, source_height).max(1),
                    box_height,
                )
            }
        };

        (even(fitted.0), even(fitted.1))
    }

    /// The bitrate this preset asks the encoder for, in kbit/s. A manual choice
    /// wins, clamped to [`MIN_BITRATE_KBPS`]..=[`MAX_BITRATE_KBPS`]; otherwise
    /// the box and the frame rate decide. A [`Resolution::Source`] capture has
    /// no box of its own, so it pays for the smallest one with at least as many
    /// pixels as the picture it produces.
    pub fn bitrate_kbps(&self, source: (u32, u32)) -> u32 {
        if let Some(manual) = self.bitrate_kbps {
            return manual.clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS);
        }

        let sized = match self.resolution {
            Resolution::Source => Resolution::by_pixel_count(self.output_size(source)),
            named => named,
        };

        match (sized, self.fps) {
            (Resolution::P720, FrameRate::F15) => 1_500,
            (Resolution::P720, FrameRate::F30) => 2_500,
            (Resolution::P720, FrameRate::F60) => 4_000,
            (Resolution::P1080, FrameRate::F15) => 3_000,
            (Resolution::P1080, FrameRate::F30) => 4_500,
            (Resolution::P1080, FrameRate::F60) => 7_000,
            (Resolution::P1440, FrameRate::F15) => 5_000,
            (Resolution::P1440, FrameRate::F30) => 8_000,
            (Resolution::P1440, FrameRate::F60) => 12_000,
            (Resolution::P2160 | Resolution::Source, FrameRate::F15) => 10_000,
            (Resolution::P2160 | Resolution::Source, FrameRate::F30) => 15_000,
            (Resolution::P2160 | Resolution::Source, FrameRate::F60) => 24_000,
        }
    }
}

/// `value * numerator / denominator` without overflowing at 4K.
fn scale(value: u32, numerator: u32, denominator: u32) -> u32 {
    (u64::from(value) * u64::from(numerator) / u64::from(denominator)) as u32
}

fn even(value: u32) -> u32 {
    (value & !1).max(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auto(resolution: Resolution, fps: FrameRate) -> Preset {
        Preset {
            resolution,
            fps,
            bitrate_kbps: None,
        }
    }

    #[test]
    fn a_1440p_source_at_1080p_fits_inside_the_box_keeping_aspect() {
        let preset = auto(Resolution::P1080, FrameRate::F30);
        assert_eq!(preset.output_size((2560, 1440)), (1920, 1080));
        // 16:10 keeps its own aspect: 1920 wide would be 1200 tall, too tall,
        // so the height binds instead.
        assert_eq!(preset.output_size((2560, 1600)), (1728, 1080));
    }

    #[test]
    fn a_portrait_source_fits_by_height() {
        let preset = auto(Resolution::P1080, FrameRate::F30);
        let (width, height) = preset.output_size((1080, 1920));
        assert_eq!(height, 1080);
        assert_eq!(width, 606);
    }

    #[test]
    fn source_never_upscales_and_caps_at_2160p() {
        let preset = auto(Resolution::Source, FrameRate::F30);
        assert_eq!(preset.output_size((1366, 768)), (1366, 768));
        assert_eq!(preset.output_size((5120, 2880)), (3840, 2160));

        // A smaller source stays itself under every named box too.
        let p2160 = auto(Resolution::P2160, FrameRate::F30);
        assert_eq!(p2160.output_size((1280, 720)), (1280, 720));
    }

    #[test]
    fn output_dimensions_are_even() {
        let preset = auto(Resolution::Source, FrameRate::F30);
        assert_eq!(preset.output_size((1365, 767)), (1364, 766));
        assert_eq!(preset.output_size((1, 1)), (2, 2));

        let p720 = auto(Resolution::P720, FrameRate::F30);
        for width in 1001..1100 {
            let (out_width, out_height) = p720.output_size((width, 999));
            assert_eq!(out_width % 2, 0, "{width} gave an odd width");
            assert_eq!(out_height % 2, 0, "{width} gave an odd height");
        }
    }

    #[test]
    fn the_bitrate_table_matches_the_box_and_fps() {
        let cases = [
            (Resolution::P720, FrameRate::F15, 1_500),
            (Resolution::P720, FrameRate::F30, 2_500),
            (Resolution::P720, FrameRate::F60, 4_000),
            (Resolution::P1080, FrameRate::F15, 3_000),
            (Resolution::P1080, FrameRate::F30, 4_500),
            (Resolution::P1080, FrameRate::F60, 7_000),
            (Resolution::P1440, FrameRate::F15, 5_000),
            (Resolution::P1440, FrameRate::F30, 8_000),
            (Resolution::P1440, FrameRate::F60, 12_000),
            (Resolution::P2160, FrameRate::F15, 10_000),
            (Resolution::P2160, FrameRate::F30, 15_000),
            (Resolution::P2160, FrameRate::F60, 24_000),
        ];
        for (resolution, fps, expected) in cases {
            assert_eq!(
                auto(resolution, fps).bitrate_kbps((3840, 2160)),
                expected,
                "{resolution} at {} fps",
                fps.hz()
            );
        }

        // Source bills at the smallest box with at least as many pixels.
        let source = auto(Resolution::Source, FrameRate::F30);
        assert_eq!(source.bitrate_kbps((1366, 768)), 4_500);
        assert_eq!(source.bitrate_kbps((1280, 720)), 2_500);
        assert_eq!(source.bitrate_kbps((3840, 2160)), 15_000);
    }

    #[test]
    fn a_portrait_output_bills_by_its_pixel_count() {
        // 1080x1920 is 2 073 600 pixels, exactly 1080p's own count, so it pays
        // the 1080p row even though no box is that tall.
        let source = auto(Resolution::Source, FrameRate::F30);
        assert_eq!(source.output_size((1080, 1920)), (1080, 1920));
        assert_eq!(source.bitrate_kbps((1080, 1920)), 4_500);

        // One pixel row more is past 1080p's count and pays 1440p.
        assert_eq!(source.bitrate_kbps((1080, 1922)), 8_000);

        // And the landscape picture of the same size pays the same.
        assert_eq!(source.bitrate_kbps((1920, 1080)), 4_500);
    }

    #[test]
    fn a_manual_bitrate_is_clamped_and_wins() {
        let preset = |kbps| Preset {
            resolution: Resolution::P720,
            fps: FrameRate::F15,
            bitrate_kbps: Some(kbps),
        };
        assert_eq!(preset(6_000).bitrate_kbps((1280, 720)), 6_000);
        assert_eq!(preset(10).bitrate_kbps((1280, 720)), MIN_BITRATE_KBPS);
        assert_eq!(preset(999_999).bitrate_kbps((1280, 720)), MAX_BITRATE_KBPS);
    }

    #[test]
    fn resolution_strings_round_trip() {
        let cases = [
            (Resolution::Source, "source"),
            (Resolution::P720, "720p"),
            (Resolution::P1080, "1080p"),
            (Resolution::P1440, "1440p"),
            (Resolution::P2160, "2160p"),
        ];
        for (resolution, name) in cases {
            assert_eq!(resolution.to_string(), name);
            assert_eq!(name.parse(), Ok(resolution));
        }

        assert_eq!("SOURCE".parse(), Ok(Resolution::Source));
        assert_eq!("1080P".parse(), Ok(Resolution::P1080));
        assert!("4k".parse::<Resolution>().is_err());
        assert!("".parse::<Resolution>().is_err());
    }

    #[test]
    fn frame_rate_from_hz_rejects_unknown_values() {
        for rate in [FrameRate::F15, FrameRate::F30, FrameRate::F60] {
            assert_eq!(FrameRate::from_hz(rate.hz()), Some(rate));
        }
        for hz in [0, 1, 24, 25, 29, 31, 50, 59, 61, 120] {
            assert_eq!(FrameRate::from_hz(hz), None, "{hz} should not be a preset");
        }

        assert_eq!(FrameRate::F30.interval(), Duration::from_nanos(33_333_333));
        assert_eq!(FrameRate::F60.interval(), Duration::from_nanos(16_666_666));
    }
}
