//! A picture to feed the pipeline when there is no screen to capture.
//!
//! Tests and the headless probe need frames that are cheap to make, obviously
//! wrong when something mangles them, and different from one frame to the next
//! so a stuck encoder or a repeated datagram shows up.

/// Eight scrolling colour bars, a black block in the top-left corner that grows
/// with `frame_index`, and a one-pixel white border.
///
/// Writes exactly `width * height * 4` BGRA bytes into `out`, which is cleared
/// first (its capacity is kept). A zero-sized picture leaves `out` empty.
pub fn test_pattern(width: u32, height: u32, frame_index: u32, out: &mut Vec<u8>) {
    out.clear();
    if width == 0 || height == 0 {
        return;
    }

    // B, G, R, A: white, yellow, cyan, green, magenta, red, blue, black.
    const BARS: [[u8; 4]; 8] = [
        [255, 255, 255, 255],
        [0, 255, 255, 255],
        [255, 255, 0, 255],
        [0, 255, 0, 255],
        [255, 0, 255, 255],
        [0, 0, 255, 255],
        [255, 0, 0, 255],
        [0, 0, 0, 255],
    ];
    const BLACK: [u8; 4] = [0, 0, 0, 255];
    const WHITE: [u8; 4] = [255, 255, 255, 255];

    let (width, height) = (width as usize, height as usize);
    let scroll = frame_index as usize % width;
    let block_width = (((frame_index % 16) as usize + 1) * 4).min(width);
    let block_height = (height / 8).max(1);

    out.resize(width * height * 4, 0);
    for (y, row) in out.chunks_exact_mut(width * 4).enumerate() {
        for (x, pixel) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let bar = ((x + scroll) % width) * BARS.len() / width;
            *pixel = if x < block_width && y < block_height {
                BLACK
            } else if x == 0 || y == 0 || x == width - 1 || y == height - 1 {
                WHITE
            } else {
                BARS[bar]
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consecutive_frames_differ() {
        let mut previous = Vec::new();
        let mut current = Vec::new();

        test_pattern(64, 32, 0, &mut previous);
        for frame in 1..40 {
            test_pattern(64, 32, frame, &mut current);
            assert_ne!(
                current,
                previous,
                "frame {frame} repeats frame {}",
                frame - 1
            );
            std::mem::swap(&mut previous, &mut current);
        }
    }

    #[test]
    fn the_buffer_is_exactly_width_height_4() {
        let mut frame = Vec::new();
        for (width, height) in [(64, 32), (2, 2), (1, 1), (17, 5)] {
            test_pattern(width, height, 3, &mut frame);
            assert_eq!(
                frame.len(),
                (width * height * 4) as usize,
                "{width}x{height}"
            );
        }

        test_pattern(0, 32, 3, &mut frame);
        assert!(frame.is_empty());

        // The border is white and the top-left corner is the growing block.
        test_pattern(64, 32, 0, &mut frame);
        assert_eq!(&frame[..4], &[0, 0, 0, 255], "the block covers the corner");
        let last_row = 31 * 64 * 4;
        assert_eq!(&frame[last_row..last_row + 4], &[255, 255, 255, 255]);
    }
}
