//! Where a picked picture is cropped before it goes up: a centre, a zoom, and
//! the source rectangle those two stand for.
//!
//! The crop is held as a centre and a zoom rather than as a rectangle because
//! that is what survives a drag: the same state answers for the preview, drawn
//! at whatever size the dialog gives it, and for the pixels the upload cuts out
//! of the source.

use iced::Rectangle;

/// How tight the crop may be pulled. `1.0` is the whole of the largest rectangle
/// of the target aspect that fits the source; nothing is ever taken past
/// [`ZOOM_MAX`].
pub const ZOOM_MIN: f32 = 1.0;
pub const ZOOM_MAX: f32 = 4.0;

/// How wide the adjuster draws the picture. The drag is measured in these
/// pixels, so `update::crop` scales a pointer delta by it and the frame the view
/// lays out has to be the same width.
pub const FRAME_WIDTH: f32 = 360.0;

/// Where the crop sits on the source and how tight it is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CropState {
    /// Centre of the crop in normalised source coordinates, each in `0.0..=1.0`.
    pub centre: (f32, f32),
    /// `1.0` is the largest rectangle of the target aspect that fits the
    /// source; larger values tighten the crop.
    pub zoom: f32,
}

/// A pan in flight: where in the frame the pointer went down, and the centre it
/// went down on. The delta is measured from there rather than from the last
/// move, so a crop held against an edge comes back off it in step with the
/// pointer.
#[derive(Debug, Clone, Copy)]
pub struct CropDrag {
    /// `None` until the first move: a press carries no position of its own.
    pub from: Option<iced::Point>,
    pub centre: (f32, f32),
}

/// The part of `source` one crop names, in source pixels, for a target of
/// `aspect`.
///
/// The rectangle always lies wholly inside the source: a centre that would push
/// it over an edge slides it back instead, keeping its size, which is what stops
/// a drag from changing how much of the picture is taken. The caller's zoom and
/// centre are clamped here rather than trusted.
pub fn region(source: (u32, u32), aspect: (u32, u32), state: CropState) -> Rectangle<u32> {
    let source_width = f64::from(source.0.max(1));
    let source_height = f64::from(source.1.max(1));
    // A zero side would divide by zero; a square is the harmless reading of one.
    let ratio = f64::from(aspect.0.max(1)) / f64::from(aspect.1.max(1));

    // The largest rectangle of the wanted ratio that fits is as wide as the
    // source, unless the source is the wider shape of the two and its height is
    // what binds.
    let base_width = if source_width / source_height > ratio {
        source_height * ratio
    } else {
        source_width
    };

    let zoom = f64::from(state.zoom.clamp(ZOOM_MIN, ZOOM_MAX));
    let width = (base_width / zoom).round().clamp(1.0, source_width);
    // Derived from the rounded width rather than scaled on its own: two
    // independent roundings would pull the aspect apart.
    let height = (width / ratio).round().clamp(1.0, source_height);

    let centre_x = f64::from(state.centre.0.clamp(0.0, 1.0)) * source_width;
    let centre_y = f64::from(state.centre.1.clamp(0.0, 1.0)) * source_height;
    let x = (centre_x - width / 2.0)
        .round()
        .clamp(0.0, source_width - width);
    let y = (centre_y - height / 2.0)
        .round()
        .clamp(0.0, source_height - height);

    Rectangle {
        x: x as u32,
        y: y as u32,
        width: width as u32,
        height: height as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SQUARE: (u32, u32) = (512, 512);
    const WIDE: (u32, u32) = (1600, 600);

    fn state(centre: (f32, f32), zoom: f32) -> CropState {
        CropState { centre, zoom }
    }

    /// The height is the width's own ratio, to within the pixel the rounding
    /// costs.
    fn aspect_holds(rect: Rectangle<u32>, aspect: (u32, u32)) -> bool {
        let ratio = f64::from(aspect.0) / f64::from(aspect.1);
        (f64::from(rect.width) / ratio - f64::from(rect.height)).abs() <= 1.0
    }

    #[test]
    fn a_square_target_on_a_wide_source_is_full_height_and_centred() {
        let rect = region((2000, 1000), SQUARE, state((0.5, 0.5), ZOOM_MIN));

        assert_eq!(
            rect,
            Rectangle {
                x: 500,
                y: 0,
                width: 1000,
                height: 1000,
            }
        );
    }

    #[test]
    fn an_eight_by_three_target_on_a_square_source_is_full_width() {
        let rect = region((1000, 1000), WIDE, state((0.5, 0.5), ZOOM_MIN));

        assert_eq!(rect.x, 0);
        assert_eq!((rect.width, rect.height), (1000, 375));
        // 1000 tall less the 375 taken, halved.
        assert_eq!(rect.y, 313);
    }

    #[test]
    fn zoom_shrinks_the_crop_about_the_centre_and_keeps_the_aspect() {
        let full = region((1000, 1000), SQUARE, state((0.5, 0.5), ZOOM_MIN));
        assert_eq!((full.width, full.height), (1000, 1000));

        let tight = region((1000, 1000), SQUARE, state((0.5, 0.5), 2.0));
        assert_eq!(
            tight,
            Rectangle {
                x: 250,
                y: 250,
                width: 500,
                height: 500,
            }
        );

        let tighter = region((1000, 1000), WIDE, state((0.5, 0.5), 4.0));
        assert_eq!((tighter.width, tighter.height), (250, 94));
        assert!(aspect_holds(tighter, WIDE));
    }

    /// A centre against any edge keeps the crop's size and slides it back inside.
    #[test]
    fn a_centre_at_an_extreme_slides_the_crop_back_inside() {
        let corners = [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)];
        for centre in corners {
            let rect = region((2000, 1000), SQUARE, state(centre, ZOOM_MIN));

            assert_eq!((rect.width, rect.height), (1000, 1000));
            assert!(rect.x + rect.width <= 2000);
            assert!(rect.y + rect.height <= 1000);
        }

        // A centre past either end is the end, not a smaller crop.
        assert_eq!(
            region((2000, 1000), SQUARE, state((2.0, -1.0), ZOOM_MIN)),
            region((2000, 1000), SQUARE, state((1.0, 0.0), ZOOM_MIN))
        );
    }

    /// Every centre at every zoom, for a source of each shape.
    #[test]
    fn the_crop_never_leaves_the_source() {
        for source in [(2000, 1000), (1000, 1000), (800, 1200)] {
            for aspect in [SQUARE, WIDE, (128, 128)] {
                for step in 0..=10u8 {
                    let along = f32::from(step) / 10.0;
                    // Past both ends of the zoom range too: the caller is not
                    // trusted to have clamped either.
                    for zoom in [-3.0, ZOOM_MIN, 1.7, 2.5, ZOOM_MAX, 99.0] {
                        let rect = region(source, aspect, state((along, 1.0 - along), zoom));

                        assert!(rect.width >= 1 && rect.height >= 1);
                        assert!(rect.x + rect.width <= source.0);
                        assert!(rect.y + rect.height <= source.1);
                        assert!(aspect_holds(rect, aspect));
                    }
                }
            }
        }
    }

    /// The one source that cannot hold the target's shape at all still names a
    /// pixel rather than nothing.
    #[test]
    fn a_single_pixel_source_still_yields_a_pixel() {
        assert_eq!(
            region((1, 1), WIDE, state((0.5, 0.5), ZOOM_MAX)),
            Rectangle {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            }
        );
    }
}
