//! H.264 through OpenH264, narrowed to what a screen share needs: BGRA in, one
//! Annex-B access unit out, I420 back on the other side.
//!
//! OpenH264 is compiled from its own C++ sources, so this is software encoding
//! on the sharer's CPU. That is what makes the thread count below matter: at
//! 1440p and above a single thread cannot keep up with 30 frames a second.

use std::ffi::c_void;
use std::fmt;
use std::sync::Once;

use openh264::OpenH264API;
use openh264::decoder::Decoder;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile,
    RateControlMode, SpsPpsStrategy, UsageType, VuiConfig,
};
use openh264::formats::{BgraSliceU8, YUVBuffer, YUVSource};
// The parameter struct and the option ids the threaded encoder sets are not
// re-exported by the `openh264` wrapper, so they come from the same `-sys2`
// crate it is built on.
use openh264_sys2::{ENCODER_OPTION_SVC_ENCODE_PARAM_EXT, SEncParamExt, SM_FIXEDSLCNUM_SLICE};

/// Seconds between keyframes. A late joiner and anyone who lost a frame both
/// wait for the next one, so this is a latency floor as much as a bitrate cost.
const KEYFRAME_SECONDS: u32 = 5;

static THREAD_FALLBACK_WARNED: Once = Once::new();

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("openh264: {0}")]
    OpenH264(#[from] openh264::Error),
    #[error("frame size {width}x{height} must be even and not zero")]
    Size { width: u32, height: u32 },
    #[error("a {width}x{height} frame at stride {stride} needs {needed} bytes, got {len}")]
    ShortFrame {
        width: u32,
        height: u32,
        stride: usize,
        needed: usize,
        len: usize,
    },
    #[error("openh264 {call} failed with {code}")]
    RawApi { call: &'static str, code: i32 },
    #[error("asked openh264 for {requested} encoder threads, got {effective}")]
    Threads { requested: u16, effective: u16 },
}

/// What the encoder is being pointed at. A desktop holds still and then changes
/// in blocks; a face moves everywhere at once, and OpenH264 tunes its rate
/// control for one or the other.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Usage {
    #[default]
    Screen,
    Camera,
}

#[derive(Clone, Copy, Debug)]
pub struct EncoderSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Slice threads to ask OpenH264 for. Anything up to 1 is the single-slice
    /// encoder; see [`VideoEncoder::threads`] for what was actually granted.
    pub threads: u16,
    pub usage: Usage,
}

#[derive(Clone, Copy, Debug)]
pub struct EncodedFrame {
    pub keyframe: bool,
    /// The encoder produced no bytes for this frame.
    pub skipped: bool,
}

pub struct VideoEncoder {
    encoder: Encoder,
    yuv: YUVBuffer,
    /// Rows of a padded frame, packed tight for the colour converter.
    packed: Vec<u8>,
    width: usize,
    height: usize,
    threads: u16,
}

impl VideoEncoder {
    /// Builds an encoder fixed at `settings.width` x `settings.height`; both
    /// must be even, since H.264 subsamples chroma by two.
    ///
    /// A `threads` above one asks OpenH264 to cut each frame into that many
    /// slices and encode them in parallel. If it refuses, the encoder falls
    /// back to one thread and warns once per process.
    pub fn new(settings: EncoderSettings) -> Result<Self, CodecError> {
        if settings.width == 0
            || settings.height == 0
            || !settings.width.is_multiple_of(2)
            || !settings.height.is_multiple_of(2)
        {
            return Err(CodecError::Size {
                width: settings.width,
                height: settings.height,
            });
        }

        if settings.threads > 1 {
            match Self::threaded(settings) {
                Ok(encoder) => return Ok(encoder),
                Err(error) => THREAD_FALLBACK_WARNED.call_once(|| {
                    tracing::warn!(%error, "threaded H.264 encoding unavailable, using one thread");
                }),
            }
        }

        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config(&settings))?;
        Ok(Self::assemble(encoder, settings, 1))
    }

    /// The thread count OpenH264 settled on, which is 1 after a fallback and
    /// may be below what was asked for: OpenH264 caps itself at four threads
    /// and at however many slices it managed to lay out.
    pub fn threads(&self) -> u16 {
        self.threads
    }

    /// Encodes one frame. `bgra` holds `height` rows `stride` bytes apart, each
    /// starting with `width` BGRA pixels. The whole access unit — every layer's
    /// NAL units, start codes included — is appended to `out` after clearing it,
    /// so a buffer handed back in keeps its capacity.
    pub fn encode(
        &mut self,
        bgra: &[u8],
        stride: usize,
        force_keyframe: bool,
        out: &mut Vec<u8>,
    ) -> Result<EncodedFrame, CodecError> {
        out.clear();

        let row = self.width * 4;
        let needed = stride * (self.height - 1) + row;
        if stride < row || bgra.len() < needed {
            return Err(CodecError::ShortFrame {
                width: self.width as u32,
                height: self.height as u32,
                stride,
                needed,
                len: bgra.len(),
            });
        }

        if stride == row {
            let source = BgraSliceU8::new(&bgra[..row * self.height], (self.width, self.height));
            self.yuv.read_bgra8(source);
        } else {
            self.packed.resize(row * self.height, 0);
            for (index, target) in self.packed.chunks_exact_mut(row).enumerate() {
                let start = index * stride;
                target.copy_from_slice(&bgra[start..start + row]);
            }
            let source = BgraSliceU8::new(&self.packed, (self.width, self.height));
            self.yuv.read_bgra8(source);
        }

        if force_keyframe {
            self.encoder.force_intra_frame();
        }

        let bitstream = self.encoder.encode(&self.yuv)?;
        let frame_type = bitstream.frame_type();
        bitstream.write_vec(out);

        Ok(EncodedFrame {
            // Under SpsPpsStrategy::ConstantId an IDR carries its own SPS/PPS,
            // so it is the only frame a receiver can start from.
            keyframe: matches!(frame_type, FrameType::IDR),
            skipped: out.is_empty() || matches!(frame_type, FrameType::Skip | FrameType::Invalid),
        })
    }

    fn threaded(settings: EncoderSettings) -> Result<Self, CodecError> {
        let mut encoder = Encoder::with_api_config(
            OpenH264API::from_source(),
            config(&settings).num_threads(settings.threads),
        )?;

        // OpenH264 only accepts the parameters below once it is initialised,
        // which the crate does lazily on the first frame — hence this one,
        // whose bitstream is thrown away.
        let priming = YUVBuffer::new(settings.width as usize, settings.height as usize);
        let _ = encoder.encode(&priming)?;

        let threads = apply_slice_threads(&mut encoder, &settings)?;
        encoder.force_intra_frame();

        Ok(Self::assemble(encoder, settings, threads))
    }

    fn assemble(encoder: Encoder, settings: EncoderSettings, threads: u16) -> Self {
        let width = settings.width as usize;
        let height = settings.height as usize;
        Self {
            encoder,
            yuv: YUVBuffer::new(width, height),
            packed: Vec::new(),
            width,
            height,
            threads,
        }
    }
}

fn config(settings: &EncoderSettings) -> EncoderConfig {
    EncoderConfig::new()
        .usage_type(match settings.usage {
            Usage::Screen => UsageType::ScreenContentRealTime,
            Usage::Camera => UsageType::CameraVideoRealTime,
        })
        .rate_control_mode(RateControlMode::Bitrate)
        .bitrate(BitRate::from_bps(
            settings.bitrate_kbps.saturating_mul(1_000),
        ))
        .max_frame_rate(FrameRate::from_hz(settings.fps as f32))
        .intra_frame_period(IntraFramePeriod::from_num_frames(
            settings.fps * KEYFRAME_SECONDS,
        ))
        // The only lever OpenH264 has to hold a target bitrate is dropping a
        // frame it cannot afford, and the relay's per-session byte budget drops
        // datagrams outright once it is spent. A frame the encoder skips whole
        // therefore beats a frame the relay cuts in half; `EncodedFrame.skipped`
        // says when it happened.
        .skip_frames(true)
        .sps_pps_strategy(SpsPpsStrategy::ConstantId)
        .vui(VuiConfig::bt709())
        // OpenH264 encodes constrained baseline, which every decoder takes.
        .profile(Profile::Baseline)
}

/// Re-initialises `encoder` with several slice threads and returns the count it
/// actually runs.
///
/// `iMultipleThreadIdc` on its own does nothing: OpenH264 parallelises across
/// slices and clamps the thread count to the number of slices, which the
/// `openh264` crate's configuration always leaves at one. The parameters
/// therefore go in through the raw API. `InitializeExt` is private there, so
/// this uses `SetOption(ENCODER_OPTION_SVC_ENCODE_PARAM_EXT)` — the same call
/// the crate makes when a frame changes size, and one OpenH264 answers by
/// tearing the encoder down and building it again whenever the thread count or
/// the slice layout differs.
///
/// The encoder must already be initialised; OpenH264 rejects the option before
/// that.
fn apply_slice_threads(
    encoder: &mut Encoder,
    settings: &EncoderSettings,
) -> Result<u16, CodecError> {
    // SAFETY: the raw API is used exactly as the crate uses it internally, on
    // an encoder that is initialised and stays borrowed for the whole call.
    let raw = unsafe { encoder.raw_api() };

    // Zeroed, then loaded with the parameters the encoder is really running, so
    // only the three fields below differ from what it was built with.
    let mut params = SEncParamExt::default();
    check("GetOption", unsafe {
        raw.get_option(
            ENCODER_OPTION_SVC_ENCODE_PARAM_EXT,
            (&raw mut params).cast::<c_void>(),
        )
    })?;

    params.iMultipleThreadIdc = settings.threads;
    params.sSpatialLayers[0].sSliceArgument.uiSliceMode = SM_FIXEDSLCNUM_SLICE;
    params.sSpatialLayers[0].sSliceArgument.uiSliceNum = u32::from(settings.threads);
    check("SetOption", unsafe {
        raw.set_option(
            ENCODER_OPTION_SVC_ENCODE_PARAM_EXT,
            (&raw mut params).cast::<c_void>(),
        )
    })?;

    // Clear the three fields before reading them back, so an option OpenH264
    // ignored cannot pass this check by leaving what was just written in place.
    params.iPicWidth = 0;
    params.iPicHeight = 0;
    params.iMultipleThreadIdc = 0;
    check("GetOption", unsafe {
        raw.get_option(
            ENCODER_OPTION_SVC_ENCODE_PARAM_EXT,
            (&raw mut params).cast::<c_void>(),
        )
    })?;

    let effective = params.iMultipleThreadIdc;
    if params.iPicWidth != settings.width as i32
        || params.iPicHeight != settings.height as i32
        || effective < 2
    {
        return Err(CodecError::Threads {
            requested: settings.threads,
            effective,
        });
    }
    Ok(effective)
}

fn check(call: &'static str, code: i32) -> Result<(), CodecError> {
    if code == 0 {
        Ok(())
    } else {
        Err(CodecError::RawApi { call, code })
    }
}

/// One decoded picture, I420, copied out of the decoder's own buffers.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub y_stride: usize,
    pub uv_stride: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl fmt::Debug for Picture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Picture")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

pub struct VideoDecoder {
    decoder: Decoder,
}

impl VideoDecoder {
    /// Single-threaded on purpose: the `openh264` crate marks the decoder's
    /// thread option `unsafe` because it segfaults.
    pub fn new() -> Result<Self, CodecError> {
        Ok(Self {
            decoder: Decoder::new()?,
        })
    }

    /// Feeds one access unit and returns the picture it completed, if any.
    ///
    /// A stream that starts mid-GOP, or one with a hole in it, errors instead
    /// of panicking; the caller drops what it has and waits for the next
    /// keyframe.
    pub fn decode(&mut self, access_unit: &[u8]) -> Result<Option<Picture>, CodecError> {
        let Some(decoded) = self.decoder.decode(access_unit)? else {
            return Ok(None);
        };

        let (width, height) = decoded.dimensions();
        let (y_stride, uv_stride, _) = decoded.strides();

        Ok(Some(Picture {
            width: width as u32,
            height: height as u32,
            y_stride,
            uv_stride,
            y: decoded.y().to_vec(),
            u: decoded.u().to_vec(),
            v: decoded.v().to_vec(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pattern::test_pattern;

    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 360;

    fn settings(threads: u16) -> EncoderSettings {
        EncoderSettings {
            width: WIDTH,
            height: HEIGHT,
            fps: 30,
            bitrate_kbps: 4_000,
            threads,
            usage: Usage::Screen,
        }
    }

    fn frame(index: u32) -> Vec<u8> {
        let mut bgra = Vec::new();
        test_pattern(WIDTH, HEIGHT, index, &mut bgra);
        bgra
    }

    /// The luma plane openh264's own BGRA converter produces, which is what the
    /// encoder was handed and therefore the only fair reference for the decoded
    /// picture.
    fn reference_luma(bgra: &[u8]) -> YUVBuffer {
        YUVBuffer::from_bgra8_source(BgraSliceU8::new(bgra, (WIDTH as usize, HEIGHT as usize)))
    }

    fn mean_absolute_luma_error(picture: &Picture, reference: &YUVBuffer) -> f64 {
        let width = WIDTH as usize;
        let height = HEIGHT as usize;
        let mut total = 0u64;
        for y in 0..height {
            let decoded = &picture.y[y * picture.y_stride..][..width];
            let source = &reference.y()[y * reference.strides().0..][..width];
            for (left, right) in decoded.iter().zip(source) {
                total += u64::from(left.abs_diff(*right));
            }
        }
        total as f64 / (width * height) as f64
    }

    fn round_trip(threads: u16) -> (VideoEncoder, f64) {
        let mut encoder = VideoEncoder::new(settings(threads)).expect("the encoder builds");
        let mut decoder = VideoDecoder::new().expect("the decoder builds");

        let bgra = frame(0);
        let mut unit = Vec::new();
        let encoded = encoder
            .encode(&bgra, WIDTH as usize * 4, false, &mut unit)
            .expect("the frame encodes");

        assert!(encoded.keyframe, "the first frame must be an IDR");
        assert!(!encoded.skipped);

        let picture = decoder
            .decode(&unit)
            .expect("the access unit decodes")
            .expect("an access unit with an IDR completes a picture");

        assert_eq!((picture.width, picture.height), (WIDTH, HEIGHT));
        let error = mean_absolute_luma_error(&picture, &reference_luma(&bgra));
        (encoder, error)
    }

    #[test]
    fn a_pattern_round_trips_through_the_codec() {
        let (encoder, error) = round_trip(1);
        assert_eq!(encoder.threads(), 1);
        assert!(error < 8.0, "mean absolute luma error was {error}");
    }

    #[test]
    fn a_threaded_encoder_round_trips_too() {
        let (encoder, error) = round_trip(2);
        assert_eq!(
            encoder.threads(),
            2,
            "openh264 did not grant two slice threads"
        );
        assert!(error < 8.0, "mean absolute luma error was {error}");
    }

    #[test]
    fn a_camera_encoder_round_trips_too() {
        let mut encoder = VideoEncoder::new(EncoderSettings {
            usage: Usage::Camera,
            ..settings(1)
        })
        .expect("the camera encoder builds");
        let mut decoder = VideoDecoder::new().expect("the decoder builds");

        let bgra = frame(0);
        let mut unit = Vec::new();
        let encoded = encoder
            .encode(&bgra, WIDTH as usize * 4, false, &mut unit)
            .expect("the frame encodes");
        assert!(encoded.keyframe);

        let picture = decoder
            .decode(&unit)
            .expect("the access unit decodes")
            .expect("an access unit with an IDR completes a picture");
        assert_eq!((picture.width, picture.height), (WIDTH, HEIGHT));
    }

    #[test]
    fn a_forced_keyframe_is_an_idr() {
        let mut encoder = VideoEncoder::new(settings(1)).expect("the encoder builds");
        let stride = WIDTH as usize * 4;
        let bgra = frame(0);
        let mut unit = Vec::new();

        let first = encoder.encode(&bgra, stride, false, &mut unit).unwrap();
        assert!(first.keyframe);

        // The same picture again, so nothing but a forced request can make a
        // second keyframe: the intra period is 150 frames away. Rate control may
        // skip it outright, which is not a keyframe either.
        let second = encoder.encode(&bgra, stride, false, &mut unit).unwrap();
        assert!(
            !second.keyframe,
            "an unchanged frame must not become a keyframe on its own"
        );

        // Rate control can skip a forced frame too, so ask until one is coded.
        let mut forced = encoder.encode(&bgra, stride, true, &mut unit).unwrap();
        for _ in 0..8 {
            if !forced.skipped {
                break;
            }
            forced = encoder.encode(&bgra, stride, true, &mut unit).unwrap();
        }
        assert!(!forced.skipped, "the encoder never coded the forced frame");
        assert!(forced.keyframe);

        let mut decoder = VideoDecoder::new().expect("the decoder builds");
        assert!(
            decoder
                .decode(&unit)
                .expect("the forced unit decodes")
                .is_some(),
            "a forced keyframe must be decodable on its own"
        );
    }

    #[test]
    fn feeding_a_p_frame_first_does_not_panic() {
        let mut encoder = VideoEncoder::new(settings(1)).expect("the encoder builds");
        let stride = WIDTH as usize * 4;
        let mut unit = Vec::new();

        encoder.encode(&frame(0), stride, false, &mut unit).unwrap();

        // A coded frame that is not a keyframe, whichever frame that turns out
        // to be: rate control is free to skip some of them.
        let mut index = 1;
        let delta = loop {
            let encoded = encoder
                .encode(&frame(index), stride, false, &mut unit)
                .unwrap();
            if !encoded.skipped {
                break encoded;
            }
            index += 1;
            assert!(index < 10, "the encoder coded nothing after the keyframe");
        };
        assert!(!delta.keyframe, "a follow-up frame should be a P frame");

        let mut decoder = VideoDecoder::new().expect("the decoder builds");
        // Either answer is fine; hanging or panicking is not.
        assert!(matches!(decoder.decode(&unit), Ok(None) | Err(_)));
    }

    #[test]
    fn an_odd_size_is_rejected() {
        for (width, height) in [(641, 360), (640, 361), (641, 361), (0, 360), (640, 0)] {
            let attempt = VideoEncoder::new(EncoderSettings {
                width,
                height,
                ..settings(1)
            });
            assert!(
                matches!(attempt, Err(CodecError::Size { .. })),
                "{width}x{height} should be rejected"
            );
        }
    }

    #[test]
    fn the_access_unit_starts_with_a_start_code() {
        let mut encoder = VideoEncoder::new(settings(1)).expect("the encoder builds");
        let mut unit = Vec::new();
        let encoded = encoder
            .encode(&frame(0), WIDTH as usize * 4, false, &mut unit)
            .unwrap();
        assert!(!encoded.skipped, "the opening frame is never skipped");

        // Annex B, ITU-T H.264 B.1.1: the first NAL of a stream is preceded by
        // a four-byte start code.
        assert_eq!(&unit[..4], &[0, 0, 0, 1]);
    }

    #[test]
    fn a_padded_stride_encodes_the_same_picture() {
        let mut encoder = VideoEncoder::new(settings(1)).expect("the encoder builds");
        let stride = WIDTH as usize * 4 + 64;
        let tight = frame(0);

        let row = WIDTH as usize * 4;
        let mut padded = vec![0u8; stride * HEIGHT as usize];
        for index in 0..HEIGHT as usize {
            padded[index * stride..index * stride + row]
                .copy_from_slice(&tight[index * row..][..row]);
        }

        let mut unit = Vec::new();
        encoder.encode(&padded, stride, false, &mut unit).unwrap();

        let mut decoder = VideoDecoder::new().expect("the decoder builds");
        let picture = decoder.decode(&unit).unwrap().expect("a picture");
        let error = mean_absolute_luma_error(&picture, &reference_luma(&tight));
        assert!(error < 8.0, "mean absolute luma error was {error}");

        let short = encoder.encode(&padded[..stride], stride, false, &mut unit);
        assert!(matches!(short, Err(CodecError::ShortFrame { .. })));
    }
}
