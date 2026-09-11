//! Downscaling a captured BGRA frame to the size the encoder was built for.
//!
//! A screen is mostly text, and dropping every other pixel turns text into
//! noise that then costs the encoder far more bits than it saved. So this is an
//! area average: each output pixel is the weighted mean of every source pixel
//! its footprint touches, edges included at a fractional weight.

/// Area-averages `src` down to `dst_size`, writing `dst_size.0 * dst_size.1 * 4`
/// BGRA bytes into `dst` (cleared first, its capacity kept).
///
/// `src` rows are `src_stride` bytes apart; the output is tightly packed. The
/// caller picks even output dimensions ([`crate::preset::Preset::output_size`]
/// does); they are used exactly as given.
///
/// Nothing is ever magnified: a target larger than the source in either
/// dimension is clamped to the source, so such a request copies the source
/// region instead. A `src` slice too short for its own size and stride leaves
/// `dst` empty.
pub fn scale_bgra(
    src: &[u8],
    src_stride: usize,
    src_size: (u32, u32),
    dst_size: (u32, u32),
    dst: &mut Vec<u8>,
) {
    dst.clear();

    let (src_width, src_height) = (src_size.0 as usize, src_size.1 as usize);
    let dst_width = (dst_size.0 as usize).min(src_width);
    let dst_height = (dst_size.1 as usize).min(src_height);
    if src_width == 0 || src_height == 0 || dst_width == 0 || dst_height == 0 {
        return;
    }

    let row = src_width * 4;
    if src_stride < row || src.len() < src_stride * (src_height - 1) + row {
        return;
    }

    if dst_width == src_width && dst_height == src_height {
        dst.reserve(row * src_height);
        for y in 0..src_height {
            let start = y * src_stride;
            dst.extend_from_slice(&src[start..start + row]);
        }
        return;
    }

    let columns = Axis::new(src_width, dst_width);
    let rows = Axis::new(src_height, dst_height);
    dst.resize(dst_width * dst_height * 4, 0);

    for (y, out_row) in dst.chunks_exact_mut(dst_width * 4).enumerate() {
        let (first_row, row_weights) = rows.span(y);
        for (x, pixel) in out_row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (first_column, column_weights) = columns.span(x);
            let mut channels = [0.0f64; 4];
            let mut total = 0.0f64;

            for (offset, weight_y) in row_weights.iter().enumerate() {
                let base = (first_row + offset) * src_stride + first_column * 4;
                for (index, weight_x) in column_weights.iter().enumerate() {
                    let weight = weight_x * weight_y;
                    let source = base + index * 4;
                    for (channel, value) in channels.iter_mut().zip(&src[source..source + 4]) {
                        *channel += weight * f64::from(*value);
                    }
                    total += weight;
                }
            }

            for (byte, channel) in pixel.iter_mut().zip(channels) {
                *byte = (channel / total + 0.5) as u8;
            }
        }
    }
}

/// One axis of the box filter: for every output position, the first source
/// position it touches and the weight of each source position from there on.
/// The weights of one span sum to the span's width in source pixels.
struct Axis {
    starts: Vec<usize>,
    bounds: Vec<usize>,
    weights: Vec<f64>,
}

impl Axis {
    fn new(src: usize, dst: usize) -> Axis {
        let ratio = src as f64 / dst as f64;
        let mut axis = Axis {
            starts: Vec::with_capacity(dst),
            bounds: Vec::with_capacity(dst + 1),
            weights: Vec::with_capacity(src + dst),
        };
        axis.bounds.push(0);

        for position in 0..dst {
            let low = position as f64 * ratio;
            let high = ((position + 1) as f64 * ratio).min(src as f64);
            let first = low as usize;
            let last = (high.ceil() as usize).min(src).max(first + 1);

            axis.starts.push(first);
            for source in first..last {
                let overlap = high.min((source + 1) as f64) - low.max(source as f64);
                axis.weights.push(overlap.max(0.0));
            }
            axis.bounds.push(axis.weights.len());
        }

        axis
    }

    fn span(&self, position: usize) -> (usize, &[f64]) {
        (
            self.starts[position],
            &self.weights[self.bounds[position]..self.bounds[position + 1]],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every 2x2 block holds two 0x00 and two 0xFE samples per colour channel,
    /// so its mean is exactly 127. Alpha stays opaque throughout.
    fn checkerboard(width: usize, height: usize, stride: usize) -> Vec<u8> {
        let mut pixels = vec![0u8; stride * height];
        for y in 0..height {
            for x in 0..width {
                let dark = (x + y) % 2 == 0;
                let pixel = y * stride + x * 4;
                pixels[pixel] = if dark { 0x00 } else { 0xFE };
                pixels[pixel + 1] = if dark { 0x00 } else { 0xFE };
                pixels[pixel + 2] = if dark { 0x00 } else { 0xFE };
                pixels[pixel + 3] = 0xFF;
            }
        }
        pixels
    }

    #[test]
    fn a_2_to_1_downscale_of_a_checkerboard_is_the_exact_average() {
        let source = checkerboard(8, 6, 8 * 4);
        let mut scaled = Vec::new();
        scale_bgra(&source, 8 * 4, (8, 6), (4, 3), &mut scaled);

        assert_eq!(scaled.len(), 4 * 3 * 4);
        for pixel in scaled.as_chunks::<4>().0 {
            assert_eq!(*pixel, [127, 127, 127, 255], "every pixel averages its 2x2");
        }
    }

    #[test]
    fn a_1_to_1_request_is_a_copy() {
        // A padded stride, so a copy that ignored it would show up.
        let stride = 8 * 4 + 12;
        let source = checkerboard(8, 6, stride);
        let mut scaled = Vec::new();
        scale_bgra(&source, stride, (8, 6), (8, 6), &mut scaled);

        assert_eq!(scaled.len(), 8 * 6 * 4);
        for y in 0..6 {
            let row = y * stride;
            assert_eq!(
                &scaled[y * 8 * 4..(y + 1) * 8 * 4],
                &source[row..row + 8 * 4],
                "row {y} was not copied verbatim"
            );
        }
    }

    #[test]
    fn an_odd_source_scales_without_panicking() {
        let source = checkerboard(7, 5, 7 * 4);
        let mut scaled = Vec::new();

        scale_bgra(&source, 7 * 4, (7, 5), (4, 2), &mut scaled);
        assert_eq!(scaled.len(), 4 * 2 * 4);
        assert!(
            scaled
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| pixel[3] == 255)
        );

        // Right down to a single pixel, which averages the whole picture.
        scale_bgra(&source, 7 * 4, (7, 5), (1, 1), &mut scaled);
        assert_eq!(scaled.len(), 4);
    }

    #[test]
    fn a_larger_target_copies_instead_of_upscaling() {
        let source = checkerboard(4, 4, 4 * 4);
        let mut scaled = Vec::new();
        scale_bgra(&source, 4 * 4, (4, 4), (16, 16), &mut scaled);

        assert_eq!(scaled, source, "a bigger target must not magnify");

        // Larger in one dimension only: the other still scales.
        scale_bgra(&source, 4 * 4, (4, 4), (2, 16), &mut scaled);
        assert_eq!(scaled.len(), 2 * 4 * 4);
    }
}
