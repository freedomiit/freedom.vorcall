//! What a camera sends, turned into the BGRA the rest of the pipeline reads.
//!
//! A webcam offers YUY2 or NV12 far more often than it offers anything with an
//! R, a G and a B in it, and no compositor ever hands a screen over that way —
//! hence this module, on the camera's side of the backend only.
//!
//! The colour matrix is ITU-T H.273 / ITU-R BT.601 with the usual studio swing
//! (Y in 16..=235, chroma centred on 128), which is what V4L2 devices produce
//! unless they say otherwise. The integer form below is the standard one:
//!
//! ```text
//! R = clip((298 * (Y - 16) + 409 * (V - 128) + 128) >> 8)
//! G = clip((298 * (Y - 16) - 100 * (U - 128) - 208 * (V - 128) + 128) >> 8)
//! B = clip((298 * (Y - 16) + 516 * (U - 128) + 128) >> 8)
//! ```
//!
//! Every function here writes tight BGRA rows (`width * 4` bytes each) into
//! `out`, clearing it first, and answers `false` without writing a frame when
//! the source is shorter than the format it claims to be.

/// Opaque: a camera frame has no transparency, and the encoder's colour
/// converter reads three of the four bytes anyway.
const OPAQUE: u8 = 0xFF;

/// One pixel of studio-swing Y'CbCr as BGR, in that order.
fn bgr(y: u8, u: u8, v: u8) -> [u8; 3] {
    let luma = 298 * (i32::from(y) - 16);
    let cb = i32::from(u) - 128;
    let cr = i32::from(v) - 128;

    let r = (luma + 409 * cr + 128) >> 8;
    let g = (luma - 100 * cb - 208 * cr + 128) >> 8;
    let b = (luma + 516 * cb + 128) >> 8;
    [clip(b), clip(g), clip(r)]
}

fn clip(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

/// YUY2 (a.k.a. YUYV): two pixels in four bytes, `Y0 U Y1 V`, sharing the pair
/// of chroma samples. An odd width leaves the last macropixel half used; its
/// `Y0` is the pixel, and its chroma is read all the same.
pub(super) fn yuy2(
    source: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    out: &mut Vec<u8>,
) -> bool {
    let row_bytes = width.div_ceil(2) * 4;
    if !fits(source, height, stride, row_bytes) {
        return false;
    }

    out.clear();
    out.reserve(width * height * 4);
    for line in 0..height {
        let row = &source[line * stride..][..row_bytes];
        for x in 0..width {
            let pair = &row[(x / 2) * 4..][..4];
            let [b, g, r] = bgr(pair[(x & 1) * 2], pair[1], pair[3]);
            out.extend_from_slice(&[b, g, r, OPAQUE]);
        }
    }
    true
}

/// NV12: a full-size luma plane and a half-size plane of interleaved `U V`
/// pairs, one pair per 2x2 block. The two planes may sit in one buffer or in
/// two; the caller has already worked out which and passes the slices.
pub(super) fn nv12(
    luma: &[u8],
    luma_stride: usize,
    chroma: &[u8],
    chroma_stride: usize,
    width: usize,
    height: usize,
    out: &mut Vec<u8>,
) -> bool {
    let chroma_bytes = width.div_ceil(2) * 2;
    if !fits(luma, height, luma_stride, width)
        || !fits(chroma, height.div_ceil(2), chroma_stride, chroma_bytes)
    {
        return false;
    }

    out.clear();
    out.reserve(width * height * 4);
    for line in 0..height {
        let luma_row = &luma[line * luma_stride..][..width];
        let chroma_row = &chroma[(line / 2) * chroma_stride..][..chroma_bytes];
        for (x, y) in luma_row.iter().enumerate() {
            let pair = &chroma_row[(x / 2) * 2..][..2];
            let [b, g, r] = bgr(*y, pair[0], pair[1]);
            out.extend_from_slice(&[b, g, r, OPAQUE]);
        }
    }
    true
}

/// A four-byte-per-pixel layout: BGRx and BGRA as they are, RGBx and RGBA with
/// the red and blue ends swapped.
pub(super) fn packed(
    source: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    swap: bool,
    out: &mut Vec<u8>,
) -> bool {
    let row_bytes = width * 4;
    if !fits(source, height, stride, row_bytes) {
        return false;
    }

    out.clear();
    out.reserve(row_bytes * height);
    for line in 0..height {
        let row = &source[line * stride..][..row_bytes];
        for pixel in row.as_chunks::<4>().0 {
            let (b, r) = if swap {
                (pixel[2], pixel[0])
            } else {
                (pixel[0], pixel[2])
            };
            out.extend_from_slice(&[b, pixel[1], r, OPAQUE]);
        }
    }
    true
}

/// Whether `source` really holds `rows` rows `stride` bytes apart, each
/// beginning with `row_bytes` of picture. A zero-width or zero-height frame
/// fails here too, its `row_bytes` or `rows` being zero.
fn fits(source: &[u8], rows: usize, stride: usize, row_bytes: usize) -> bool {
    // Saturating, because the stride is a number a device chose: one that runs
    // away simply fails the comparison instead of overflowing it.
    rows > 0
        && row_bytes > 0
        && stride >= row_bytes
        && source.len() >= stride.saturating_mul(rows - 1).saturating_add(row_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Y'CbCr triples and the BGR they stand for, from the float definition of
    /// studio-swing BT.601 — `R = 1.164(Y-16) + 1.596(V-128)` and its two
    /// siblings — rounded and clipped by hand. The integer matrix above is an
    /// approximation of that definition, so it is allowed to land a step or two
    /// away from it.
    const COLOURS: [(u8, u8, u8, [u8; 3]); 6] = [
        // Black, white and mid grey: chroma centred, so only luma speaks.
        (16, 128, 128, [0, 0, 0]),
        (235, 128, 128, [255, 255, 255]),
        (126, 128, 128, [128, 128, 128]),
        // The three primaries, as BT.601 encodes them.
        (81, 90, 240, [0, 0, 255]),
        (145, 54, 34, [0, 255, 0]),
        (41, 240, 110, [255, 0, 0]),
    ];

    const TOLERANCE: i32 = 2;

    #[track_caller]
    fn assert_pixel(actual: &[u8], expected: [u8; 3], what: &str) {
        for (channel, (left, right)) in actual.iter().zip(expected).enumerate() {
            let off = (i32::from(*left) - i32::from(right)).abs();
            assert!(
                off <= TOLERANCE,
                "{what}: channel {channel} was {left}, expected {right}"
            );
        }
        assert_eq!(actual[3], OPAQUE, "{what}: alpha");
    }

    #[test]
    fn yuy2_decodes_the_bt601_primaries() {
        // One row of six pixels: three macropixels, each carrying two of the
        // colours above with the chroma of the first.
        let mut source = Vec::new();
        for pair in COLOURS.as_chunks::<2>().0 {
            let (y0, u, v, _) = pair[0];
            let (y1, ..) = pair[1];
            source.extend_from_slice(&[y0, u, y1, v]);
        }

        let mut out = Vec::new();
        assert!(yuy2(&source, 6, 1, 12, &mut out));
        assert_eq!(out.len(), 6 * 4);
        for (index, pair) in COLOURS.as_chunks::<2>().0.iter().enumerate() {
            let (_, u, v, expected) = pair[0];
            assert_pixel(&out[index * 8..][..4], expected, "even pixel");
            // The odd pixel keeps its own luma and borrows that chroma.
            let (y1, ..) = pair[1];
            let borrowed = bgr(y1, u, v);
            assert_eq!(&out[index * 8 + 4..][..3], &borrowed);
        }
    }

    #[test]
    fn yuy2_reads_an_odd_width_and_a_padded_stride() {
        // Three pixels wide: two macropixels, the second half used. 24 bytes of
        // row for 12 bytes of picture.
        let (y, u, v, white) = COLOURS[1];
        let (dark, ..) = COLOURS[0];
        let mut source = vec![0u8; 24 * 2];
        for line in 0..2 {
            source[line * 24..][..8].copy_from_slice(&[y, u, dark, v, y, u, dark, v]);
        }

        let mut out = Vec::new();
        assert!(yuy2(&source, 3, 2, 24, &mut out));
        assert_eq!(out.len(), 3 * 2 * 4);
        for line in 0..2 {
            let row = &out[line * 12..][..12];
            assert_pixel(&row[..4], white, "first");
            assert_pixel(&row[4..8], COLOURS[0].3, "second");
            assert_pixel(&row[8..], white, "third, the half macropixel");
        }

        // One byte short of the last row's picture is not a frame: the second
        // row starts at 24 and needs 8 bytes of its own.
        assert!(!yuy2(&source[..31], 3, 2, 24, &mut out));
        assert!(yuy2(&source[..32], 3, 2, 24, &mut out));
    }

    #[test]
    fn nv12_decodes_the_bt601_primaries() {
        // 2x2 of one colour per block, four blocks across one 8x2 picture.
        let width = 8;
        let height = 2;
        let mut luma = vec![0u8; width * height];
        let mut chroma = vec![0u8; width];
        for (block, (y, u, v, _)) in COLOURS[..4].iter().enumerate() {
            for line in 0..height {
                luma[line * width + block * 2] = *y;
                luma[line * width + block * 2 + 1] = *y;
            }
            chroma[block * 2] = *u;
            chroma[block * 2 + 1] = *v;
        }

        let mut out = Vec::new();
        assert!(nv12(&luma, width, &chroma, width, width, height, &mut out));
        assert_eq!(out.len(), width * height * 4);
        for line in 0..height {
            for (block, (.., expected)) in COLOURS[..4].iter().enumerate() {
                let at = (line * width + block * 2) * 4;
                assert_pixel(&out[at..][..4], *expected, "left of the block");
                assert_pixel(&out[at + 4..][..4], *expected, "right of the block");
            }
        }
    }

    #[test]
    fn nv12_reads_an_odd_size_and_padded_planes() {
        // 3x3 of flat red: two chroma columns and two chroma rows, the second
        // of each half used, both planes padded well past their picture.
        let (luma_value, u, v, red) = COLOURS[3];
        let luma_stride = 16;
        let chroma_stride = 16;
        let mut luma = vec![0u8; luma_stride * 3];
        for line in 0..3 {
            luma[line * luma_stride..][..3].copy_from_slice(&[luma_value; 3]);
        }
        let mut chroma = vec![0u8; chroma_stride * 2];
        for line in 0..2 {
            chroma[line * chroma_stride..][..4].copy_from_slice(&[u, v, u, v]);
        }

        let mut out = Vec::new();
        assert!(nv12(
            &luma,
            luma_stride,
            &chroma,
            chroma_stride,
            3,
            3,
            &mut out
        ));
        assert_eq!(out.len(), 3 * 3 * 4);
        for pixel in out.as_chunks::<4>().0 {
            assert_pixel(pixel, red, "every pixel of a flat picture");
        }

        // A chroma plane one row short of the three luma rows is not a frame.
        assert!(!nv12(
            &luma,
            luma_stride,
            &chroma[..chroma_stride],
            chroma_stride,
            3,
            3,
            &mut out
        ));
    }

    #[test]
    fn packed_swaps_only_what_it_is_told_to() {
        // Two pixels of a row 20 bytes wide, so the padding is never read.
        let stride = 20;
        let mut source = vec![0xAA; stride * 2];
        for line in 0..2 {
            source[line * stride..][..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        }

        let mut out = Vec::new();
        assert!(packed(&source, 2, 2, stride, false, &mut out));
        assert_eq!(&out[..8], &[1, 2, 3, OPAQUE, 5, 6, 7, OPAQUE]);
        assert_eq!(&out[8..], &[1, 2, 3, OPAQUE, 5, 6, 7, OPAQUE]);

        assert!(packed(&source, 2, 2, stride, true, &mut out));
        assert_eq!(&out[..8], &[3, 2, 1, OPAQUE, 7, 6, 5, OPAQUE]);

        // An odd width is fine here: each pixel is its own four bytes.
        assert!(packed(&source, 1, 2, stride, false, &mut out));
        assert_eq!(out, vec![1, 2, 3, OPAQUE, 1, 2, 3, OPAQUE]);
    }

    #[test]
    fn a_frame_shorter_than_its_format_is_refused() {
        let mut out = vec![0xFF; 4];
        assert!(!packed(&[], 2, 2, 8, false, &mut out));
        assert!(!packed(&[0; 16], 0, 2, 8, false, &mut out));
        assert!(!packed(&[0; 16], 2, 0, 8, false, &mut out));
        // A stride below one row of picture is a format nobody can read.
        assert!(!packed(&[0; 16], 2, 2, 4, false, &mut out));
        assert!(!yuy2(&[0; 4], 4, 2, 8, &mut out));
        assert!(!nv12(&[0; 4], 4, &[0; 4], 4, 4, 4, &mut out));
        assert_eq!(out, vec![0xFF; 4], "a refused frame leaves `out` alone");
    }
}
