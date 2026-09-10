//! The window icon, rasterised from the mark at startup.

use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Transform};

use super::{data, palette};

const SIDE: u32 = 128;
/// The tile's corner radius: the `rx="56"` of `assets/brand/icon.svg` in the 128 box.
const RADIUS: f32 = 28.0;
/// The mark covers 74% of the box, centred, as in `assets/brand/icon.svg`.
const MARK_SCALE: f32 = 0.74;
/// Control-point ratio that turns a cubic segment into a quarter circle.
const KAPPA: f32 = 0.5522847;

/// The rounded square the mark sits on.
fn tile() -> Option<tiny_skia::Path> {
    let side = SIDE as f32;
    let r = RADIUS;
    let k = RADIUS * KAPPA;

    let mut builder = PathBuilder::new();
    builder.move_to(r, 0.0);
    builder.line_to(side - r, 0.0);
    builder.cubic_to(side - r + k, 0.0, side, r - k, side, r);
    builder.line_to(side, side - r);
    builder.cubic_to(side, side - r + k, side - r + k, side, side - r, side);
    builder.line_to(r, side);
    builder.cubic_to(r - k, side, 0.0, side - r + k, 0.0, side - r);
    builder.line_to(0.0, r);
    builder.cubic_to(0.0, r - k, r - k, 0.0, r, 0.0);
    builder.close();
    builder.finish()
}

/// The mark, in the 256-unit box of `data::VIEW`.
fn mark() -> Option<tiny_skia::Path> {
    let mut builder = PathBuilder::new();
    for command in data::MARK {
        match *command {
            data::Cmd::M(x, y) => builder.move_to(x, y),
            data::Cmd::L(x, y) => builder.line_to(x, y),
            data::Cmd::C(x1, y1, x2, y2, x, y) => builder.cubic_to(x1, y1, x2, y2, x, y),
            data::Cmd::Z => builder.close(),
        }
    }
    builder.finish()
}

/// The icon as straight (non-premultiplied) RGBA bytes, row-major.
fn rgba() -> Option<Vec<u8>> {
    let mut pixmap = Pixmap::new(SIDE, SIDE)?;
    let tile = tile()?;
    let mark = mark()?;

    let mut paint = Paint {
        anti_alias: true,
        ..Default::default()
    };
    let [r, g, b, a] = palette::DEEP.into_rgba8();
    paint.set_color_rgba8(r, g, b, a);
    pixmap.fill_path(
        &tile,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    // `translate(128 128) scale(0.74) translate(-128 -128)` in the 256 box, then 256 -> 128:
    // a plain scale about the origin plus the offset that keeps the box centre centred.
    let scale = SIDE as f32 / data::VIEW * MARK_SCALE;
    let offset = SIDE as f32 / 2.0 * (1.0 - MARK_SCALE);
    paint.set_color_rgba8(255, 255, 255, 255);
    pixmap.fill_path(
        &mark,
        &paint,
        FillRule::Winding,
        Transform::from_scale(scale, scale).post_translate(offset, offset),
        None,
    );

    let mut bytes = Vec::with_capacity(pixmap.pixels().len() * 4);
    for pixel in pixmap.pixels() {
        let color = pixel.demultiply();
        bytes.extend_from_slice(&[color.red(), color.green(), color.blue(), color.alpha()]);
    }
    Some(bytes)
}

/// The window icon, or `None` if it cannot be rasterised — the app starts either way.
pub fn window_icon() -> Option<iced::window::Icon> {
    let Some(bytes) = rgba() else {
        tracing::warn!("brand icon: rasterisation failed, starting without a window icon");
        return None;
    };
    match iced::window::icon::from_rgba(bytes, SIDE, SIDE) {
        Ok(icon) => Some(icon),
        Err(error) => {
            tracing::warn!(%error, "brand icon: rejected, starting without a window icon");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(bytes: &[u8], x: u32, y: u32) -> [u8; 4] {
        let start = ((y * SIDE + x) * 4) as usize;
        [
            bytes[start],
            bytes[start + 1],
            bytes[start + 2],
            bytes[start + 3],
        ]
    }

    #[test]
    fn icon_is_a_deep_tile_carrying_a_white_mark() {
        let bytes = rgba().expect("the icon rasterises");

        // Outside the rounded corner.
        assert_eq!(pixel(&bytes, 0, 0)[3], 0);
        // The body, between the eyes.
        assert_eq!(pixel(&bytes, 64, 64), [255, 255, 255, 255]);
        // The tile, above the horn tips.
        assert_eq!(pixel(&bytes, 64, 4), [0xC8, 0x10, 0x2E, 255]);
        // The left eye is a hole: the tile shows through it.
        assert_eq!(pixel(&bytes, 54, 65), [0xC8, 0x10, 0x2E, 255]);
    }

    #[test]
    fn window_icon_is_built() {
        assert!(window_icon().is_some());
    }
}
