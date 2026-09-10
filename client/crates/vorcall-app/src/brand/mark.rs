//! The mark as canvas geometry.

use iced::widget::canvas::{self, Canvas, Frame, Path, Program};
use iced::{Element, Point, Rectangle, Renderer, Theme, Vector, mouse};

use super::{data, palette};

/// The mark as a filled path, in the 256-unit box of `data::VIEW`.
pub fn path() -> Path {
    Path::new(|builder| {
        for command in data::MARK {
            match *command {
                data::Cmd::M(x, y) => builder.move_to(Point::new(x, y)),
                data::Cmd::L(x, y) => builder.line_to(Point::new(x, y)),
                data::Cmd::C(x1, y1, x2, y2, x, y) => builder.bezier_curve_to(
                    Point::new(x1, y1),
                    Point::new(x2, y2),
                    Point::new(x, y),
                ),
                data::Cmd::Z => builder.close(),
            }
        }
    })
}

struct MarkProgram;

impl<Message> Program<Message> for MarkProgram {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let side = bounds.width.min(bounds.height);
        let mut frame = Frame::new(renderer, bounds.size());
        frame.translate(Vector::new(
            (bounds.width - side) / 2.0,
            (bounds.height - side) / 2.0,
        ));
        frame.scale(side / data::VIEW);
        frame.fill(&path(), palette::DEEP);
        vec![frame.into_geometry()]
    }
}

/// The mark, square, `size` units on a side.
pub fn mark<'a, Message: 'a>(size: f32) -> Element<'a, Message> {
    Canvas::new(MarkProgram).width(size).height(size).into()
}
