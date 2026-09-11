//! The mouse pointer, which Desktop Duplication reports apart from the desktop
//! image: a shape only on the frames where it changed, a position on the frames
//! where it moved. Remembering the last of each is what lets every frame be
//! painted with the cursor the user is actually looking at.

use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Dxgi::{
    DXGI_OUTDUPL_POINTER_POSITION, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
};

#[derive(Default)]
pub(super) struct Cursor {
    shape: Option<Shape>,
    at: POINT,
    visible: bool,
}

struct Shape {
    kind: i32,
    width: usize,
    height: usize,
    pitch: usize,
    bits: Vec<u8>,
}

impl Cursor {
    pub(super) fn moved(&mut self, position: &DXGI_OUTDUPL_POINTER_POSITION) {
        self.visible = position.Visible.as_bool();
        if self.visible {
            self.at = position.Position;
        }
    }

    /// A monochrome pointer arrives as two stacked masks, so the height the
    /// shape reports is twice the height it draws at.
    pub(super) fn reshaped(&mut self, info: &DXGI_OUTDUPL_POINTER_SHAPE_INFO, bits: Vec<u8>) {
        let monochrome = info.Type as i32 == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0;
        let height = if monochrome {
            info.Height / 2
        } else {
            info.Height
        };
        self.shape = Some(Shape {
            kind: info.Type as i32,
            width: info.Width as usize,
            height: height as usize,
            pitch: info.Pitch as usize,
            bits,
        });
    }

    /// Paints the pointer into a frame's tightly packed BGRA rows, clipped to
    /// the frame. Does nothing until both a shape and a visible position have
    /// been seen.
    pub(super) fn composite(&self, bgra: &mut [u8], width: u32, height: u32) {
        let Some(shape) = &self.shape else {
            return;
        };
        if !self.visible {
            return;
        }
        let stride = width as usize * 4;

        for row in 0..shape.height {
            let Some(y) = offset(self.at.y, row, height) else {
                continue;
            };
            for column in 0..shape.width {
                let Some(x) = offset(self.at.x, column, width) else {
                    continue;
                };
                let Some(pixel) = bgra.get_mut(y * stride + x * 4..y * stride + x * 4 + 4) else {
                    continue;
                };
                shape.paint(pixel, row, column);
            }
        }
    }
}

impl Shape {
    fn paint(&self, pixel: &mut [u8], row: usize, column: usize) {
        if self.kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 {
            self.paint_monochrome(pixel, row, column);
            return;
        }

        let Some(source) = self
            .bits
            .get(row * self.pitch + column * 4..row * self.pitch + column * 4 + 4)
        else {
            return;
        };
        let alpha = u32::from(source[3]);

        if self.kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 {
            // A masked pointer says what to do with the alpha channel rather
            // than how opaque it is: 0xFF means invert the desktop underneath,
            // anything else means paint over it.
            for channel in 0..3 {
                pixel[channel] = if alpha == 0xFF {
                    pixel[channel] ^ source[channel]
                } else {
                    source[channel]
                };
            }
            return;
        }

        if self.kind != DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 {
            return;
        }
        for channel in 0..3 {
            let over = u32::from(source[channel]) * alpha;
            let under = u32::from(pixel[channel]) * (255 - alpha);
            pixel[channel] = ((over + under) / 255) as u8;
        }
    }

    /// The classic AND/XOR pair: the AND mask clears what the pointer covers,
    /// the XOR mask draws over it, and a set bit in both inverts the desktop.
    fn paint_monochrome(&self, pixel: &mut [u8], row: usize, column: usize) {
        let and = self.bit(row, column);
        let xor = self.bit(self.height + row, column);
        match (and, xor) {
            (false, false) => pixel[..3].fill(0x00),
            (false, true) => pixel[..3].fill(0xFF),
            (true, false) => {}
            (true, true) => {
                for channel in pixel.iter_mut().take(3) {
                    *channel = !*channel;
                }
            }
        }
    }

    fn bit(&self, row: usize, column: usize) -> bool {
        let Some(byte) = self.bits.get(row * self.pitch + column / 8) else {
            return false;
        };
        byte >> (7 - column % 8) & 1 == 1
    }
}

/// Where a row or column of the pointer lands in a frame, or `None` when it
/// falls outside it.
fn offset(origin: i32, within: usize, limit: u32) -> Option<usize> {
    let at = origin.checked_add(i32::try_from(within).ok()?)?;
    let at = usize::try_from(at).ok()?;
    (at < limit as usize).then_some(at)
}
