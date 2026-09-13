// Vendored from iced_widget 0.14.2, `src/text/rich.rs`, and extended with
// pointer selection. iced 0.14 ships no selectable text widget: `rich_text`
// draws styled spans and clickable links but cannot be selected, and
// `text_editor` can be selected but draws plain text only, so mention chips,
// links and a character-level drag-selection cannot be had together from the
// stock widgets.
//
// Copyright 2019 Héctor Ramón, Iced contributors
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of
// this software and associated documentation files (the "Software"), to deal in
// the Software without restriction, including without limitation the rights to
// use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
// the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
// FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR
// COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER
// IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
// CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

//! Rich text the pointer can select and copy.
//!
//! Selection offsets are byte offsets into the text the spans concatenate to,
//! always on a character boundary. The highlight is not a new draw path: the
//! selected range is cut out into spans of its own, each carrying the selection
//! colour as its span background, so iced's own span-background machinery paints
//! it.

use std::borrow::Cow;
use std::ops::Range;

use iced::advanced::clipboard::{self, Clipboard};
use iced::advanced::text::{self, Paragraph, Span};
use iced::advanced::widget::text as text_style;
use iced::advanced::widget::text::{Alignment, Catalog, LineHeight, Shaping, Wrapping};
use iced::advanced::widget::tree::{self, Tree};
use iced::advanced::{Layout, Shell, Widget, layout, mouse, renderer};
use iced::{
    Color, Element, Event, Length, Pixels, Point, Rectangle, Size, Vector, alignment, keyboard,
};

/// The span list, boxed exactly as iced boxes it. Behind an alias so the field
/// stays readable.
type SpanList<'a, Link, Font> = Box<dyn AsRef<[Span<'a, Link, Font>]> + 'a>;

/// A bunch of rich text the pointer can select.
pub fn selectable_rich_text<'a, Link, Message, Theme, Renderer>(
    spans: impl AsRef<[Span<'a, Link, Renderer::Font>]> + 'a,
) -> SelectableRichText<'a, Link, Message, Theme, Renderer>
where
    Link: Clone + 'static,
    Theme: Catalog,
    Renderer: text::Renderer,
    Renderer::Font: 'a,
{
    SelectableRichText::with_spans(spans)
}

/// Rich text with links, span backgrounds and a pointer selection.
pub struct SelectableRichText<'a, Link, Message, Theme = iced::Theme, Renderer = iced::Renderer>
where
    Link: Clone + 'static,
    Theme: Catalog,
    Renderer: text::Renderer,
{
    spans: SpanList<'a, Link, Renderer::Font>,
    size: Option<Pixels>,
    line_height: LineHeight,
    width: Length,
    height: Length,
    font: Option<Renderer::Font>,
    align_x: Alignment,
    align_y: alignment::Vertical,
    wrapping: Wrapping,
    class: Theme::Class<'a>,
    hovered_link: Option<usize>,
    on_link_click: Option<Box<dyn Fn(Link) -> Message + 'a>>,
    on_select: Option<Box<dyn Fn() -> Message + 'a>>,
    selectable: bool,
    selection_colour: Color,
}

impl<'a, Link, Message, Theme, Renderer> SelectableRichText<'a, Link, Message, Theme, Renderer>
where
    Link: Clone + 'static,
    Theme: Catalog,
    Renderer: text::Renderer,
    Renderer::Font: 'a,
{
    /// Creates a new empty [`SelectableRichText`].
    pub fn new() -> Self {
        Self {
            spans: Box::new([]),
            size: None,
            line_height: LineHeight::default(),
            width: Length::Shrink,
            height: Length::Shrink,
            font: None,
            align_x: Alignment::Default,
            align_y: alignment::Vertical::Top,
            wrapping: Wrapping::default(),
            class: Theme::default(),
            hovered_link: None,
            on_link_click: None,
            on_select: None,
            selectable: false,
            selection_colour: DEFAULT_SELECTION,
        }
    }

    /// Creates a new [`SelectableRichText`] with the given text spans.
    pub fn with_spans(spans: impl AsRef<[Span<'a, Link, Renderer::Font>]> + 'a) -> Self {
        Self {
            spans: Box::new(spans),
            ..Self::new()
        }
    }

    /// Sets the default size of the text.
    pub fn size(mut self, size: impl Into<Pixels>) -> Self {
        self.size = Some(size.into());
        self
    }

    /// Sets the width of the text boundaries.
    pub fn width(mut self, width: impl Into<Length>) -> Self {
        self.width = width.into();
        self
    }

    /// Sets the message produced when a link is clicked.
    ///
    /// If the spans contain no links, you may need to call this with
    /// `on_link_click(never)` for the compiler to infer the `Link` generic.
    pub fn on_link_click(mut self, on_link_click: impl Fn(Link) -> Message + 'a) -> Self {
        self.on_link_click = Some(Box::new(on_link_click));
        self
    }

    /// Sets the message published on the press that starts a selection, so the
    /// parent can take the selection away from whatever held it before.
    pub fn on_select(mut self, on_select: impl Fn() -> Message + 'a) -> Self {
        self.on_select = Some(Box::new(on_select));
        self
    }

    /// Whether the pointer may select this text. With `false` the widget behaves
    /// exactly like iced's `rich_text`.
    pub fn selectable(mut self, selectable: bool) -> Self {
        self.selectable = selectable;
        self
    }

    /// The colour painted behind the selected text. The widget is theme-agnostic:
    /// the caller supplies this from its own palette.
    pub fn selection_colour(mut self, colour: Color) -> Self {
        self.selection_colour = colour;
        self
    }
}

impl<'a, Link, Message, Theme, Renderer> Default
    for SelectableRichText<'a, Link, Message, Theme, Renderer>
where
    Link: Clone + 'a,
    Theme: Catalog,
    Renderer: text::Renderer,
    Renderer::Font: 'a,
{
    fn default() -> Self {
        Self::new()
    }
}

/// A neutral highlight, in case the caller never supplies one.
const DEFAULT_SELECTION: Color = Color {
    r: 0.35,
    g: 0.55,
    b: 0.95,
    a: 0.35,
};

/// What the pointer has selected, as byte offsets into the concatenated text.
#[derive(Debug, Clone, Copy, Default)]
struct Selection {
    anchor: Option<usize>,
    focus: Option<usize>,
    dragging: bool,
}

impl Selection {
    fn range(self) -> Option<Range<usize>> {
        let (anchor, focus) = (self.anchor?, self.focus?);

        (anchor != focus).then(|| anchor.min(focus)..anchor.max(focus))
    }
}

struct State<Link, P: Paragraph> {
    /// The spans the paragraph was laid out from — the cut ones, not the
    /// caller's. Every span index in this file, iced's own included, indexes
    /// this list.
    spans: Vec<Span<'static, Link, P::Font>>,
    /// Where each of those spans starts in `text`.
    starts: Vec<usize>,
    /// Where each hard line starts in `text`.
    lines: Vec<usize>,
    text: String,
    span_pressed: Option<usize>,
    paragraph: P,
    selection: Selection,
    last_click: Option<mouse::Click>,
}

impl<Link, P: Paragraph> State<Link, P> {
    /// Drops the selection, reporting whether there was one to drop.
    fn deselect(&mut self) -> bool {
        let had = self.selection.anchor.is_some() || self.selection.focus.is_some();
        self.selection = Selection::default();
        had
    }

    /// The offset the given point, relative to the widget, falls on.
    fn offset_at(&self, point: Point) -> usize {
        let Some(hit) = self.paragraph.hit_test(point) else {
            return if point.y < 0.0 { 0 } else { self.text.len() };
        };

        // `Paragraph::hit_test` reports the offset within the hard line that was
        // hit and drops which line that was, so the line has to be recovered
        // from the span under the pointer. No span crosses a hard line: they are
        // cut there by `split_selection`.
        let line = if self.lines.len() > 1 {
            self.line_at(point)
        } else {
            0
        };

        snap(&self.text, (line + hit.cursor()).min(self.line_end(line)))
    }

    fn line_at(&self, point: Point) -> usize {
        let Some(span) = self.span_at(point) else {
            return 0;
        };
        let start = self.starts.get(span).copied().unwrap_or(0);

        self.lines
            .iter()
            .rev()
            .find(|line| **line <= start)
            .copied()
            .unwrap_or(0)
    }

    fn line_end(&self, line: usize) -> usize {
        self.lines
            .iter()
            .find(|start| **start > line)
            .map_or(self.text.len(), |start| start - 1)
    }

    /// The span whose drawn rectangles lie nearest the point.
    fn span_at(&self, point: Point) -> Option<usize> {
        let mut nearest: Option<(usize, f32, f32)> = None;

        for index in 0..self.spans.len() {
            for bounds in self.paragraph.span_bounds(index) {
                let dy = gap(point.y, bounds.y, bounds.y + bounds.height);
                let dx = gap(point.x, bounds.x, bounds.x + bounds.width);

                if nearest.is_none_or(|(_, near_y, near_x)| (dy, dx) < (near_y, near_x)) {
                    nearest = Some((index, dy, dx));
                }
            }
        }

        nearest.map(|(index, _, _)| index)
    }

    /// The selected text, if any.
    fn selected(&self) -> Option<&str> {
        self.text.get(self.selection.range()?)
    }
}

/// How far `value` sits outside `[from, to]`.
fn gap(value: f32, from: f32, to: f32) -> f32 {
    if value < from {
        from - value
    } else if value > to {
        value - to
    } else {
        0.0
    }
}

impl<Link, Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for SelectableRichText<'_, Link, Message, Theme, Renderer>
where
    Link: Clone + 'static,
    Theme: Catalog,
    Renderer: text::Renderer,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State<Link, Renderer::Paragraph>>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::<Link, Renderer::Paragraph> {
            spans: Vec::new(),
            starts: Vec::new(),
            lines: Vec::new(),
            text: String::new(),
            span_pressed: None,
            paragraph: Renderer::Paragraph::default(),
            selection: Selection::default(),
            last_click: None,
        })
    }

    fn size(&self) -> Size<Length> {
        Size {
            width: self.width,
            height: self.height,
        }
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let state = tree
            .state
            .downcast_mut::<State<Link, Renderer::Paragraph>>();
        let source = self.spans.as_ref().as_ref();

        let spans = if self.selectable {
            split_selection(source, state.selection.range(), self.selection_colour)
        } else {
            source.to_vec()
        };

        layout::sized(limits, self.width, self.height, |limits| {
            let bounds = limits.max();

            let size = self.size.unwrap_or_else(|| renderer.default_size());
            let font = self.font.unwrap_or_else(|| renderer.default_font());

            let text_with_spans = || text::Text {
                content: spans.as_slice(),
                bounds,
                size,
                line_height: self.line_height,
                font,
                align_x: self.align_x,
                align_y: self.align_y,
                shaping: Shaping::Advanced,
                wrapping: self.wrapping,
            };

            if state.spans != spans {
                state.paragraph = Renderer::Paragraph::with_spans(text_with_spans());
            } else {
                match state.paragraph.compare(text::Text {
                    content: (),
                    bounds,
                    size,
                    line_height: self.line_height,
                    font,
                    align_x: self.align_x,
                    align_y: self.align_y,
                    shaping: Shaping::Advanced,
                    wrapping: self.wrapping,
                }) {
                    text::Difference::None => {}
                    text::Difference::Bounds => {
                        state.paragraph.resize(bounds);
                    }
                    text::Difference::Shape => {
                        state.paragraph = Renderer::Paragraph::with_spans(text_with_spans());
                    }
                }
            }

            // iced's `PartialEq` for a span ignores its highlight, so a selection
            // that moved onto an existing span boundary compares equal there and
            // has to be caught separately or the cached list would keep drawing a
            // stale highlight.
            if !decorated_alike(&state.spans, &spans) {
                state.spans = spans.iter().cloned().map(Span::to_static).collect();
                state.starts = span_starts(&state.spans);
                state.text = spans_text(&state.spans);
                state.lines = line_starts(&state.text);
            }

            state.paragraph.min_bounds()
        })
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        defaults: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        if !layout.bounds().intersects(viewport) {
            return;
        }

        let state = tree
            .state
            .downcast_ref::<State<Link, Renderer::Paragraph>>();

        let style = theme.style(&self.class);

        for (index, span) in state.spans.iter().enumerate() {
            let is_hovered_link = self.on_link_click.is_some() && Some(index) == self.hovered_link;

            if span.highlight.is_some() || span.underline || span.strikethrough || is_hovered_link {
                let translation = layout.position() - Point::ORIGIN;
                let regions = state.paragraph.span_bounds(index);

                if let Some(highlight) = span.highlight {
                    for bounds in &regions {
                        let bounds = Rectangle::new(
                            bounds.position() - Vector::new(span.padding.left, span.padding.top),
                            bounds.size() + Size::new(span.padding.x(), span.padding.y()),
                        );

                        renderer.fill_quad(
                            renderer::Quad {
                                bounds: bounds + translation,
                                border: highlight.border,
                                ..Default::default()
                            },
                            highlight.background,
                        );
                    }
                }

                if span.underline || span.strikethrough || is_hovered_link {
                    let size = span.size.or(self.size).unwrap_or(renderer.default_size());

                    let line_height = span
                        .line_height
                        .unwrap_or(self.line_height)
                        .to_absolute(size);

                    let color = span.color.or(style.color).unwrap_or(defaults.text_color);

                    let baseline =
                        translation + Vector::new(0.0, size.0 + (line_height.0 - size.0) / 2.0);

                    if span.underline || is_hovered_link {
                        for bounds in &regions {
                            renderer.fill_quad(
                                renderer::Quad {
                                    bounds: Rectangle::new(
                                        bounds.position() + baseline
                                            - Vector::new(0.0, size.0 * 0.08),
                                        Size::new(bounds.width, 1.0),
                                    ),
                                    ..Default::default()
                                },
                                color,
                            );
                        }
                    }

                    if span.strikethrough {
                        for bounds in &regions {
                            renderer.fill_quad(
                                renderer::Quad {
                                    bounds: Rectangle::new(
                                        bounds.position() + baseline
                                            - Vector::new(0.0, size.0 / 2.0),
                                        Size::new(bounds.width, 1.0),
                                    ),
                                    ..Default::default()
                                },
                                color,
                            );
                        }
                    }
                }
            }
        }

        text_style::draw(
            renderer,
            defaults,
            layout.bounds(),
            &state.paragraph,
            style,
            viewport,
        );
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let was_hovered = self.hovered_link.is_some();

        self.hovered_link = cursor
            .position_in(bounds)
            .filter(|_| self.on_link_click.is_some())
            .and_then(|position| {
                let state = tree
                    .state
                    .downcast_ref::<State<Link, Renderer::Paragraph>>();

                state.paragraph.hit_span(position).filter(|index| {
                    state
                        .spans
                        .get(*index)
                        .is_some_and(|span| span.link.is_some())
                })
            });

        if was_hovered != self.hovered_link.is_some() {
            shell.request_redraw();
        }

        self.update_links(tree, event, shell);

        // Links are handled first and a selection never starts on a hovered one,
        // so a click on a link stays a click on a link.
        if self.selectable {
            self.update_selection(tree, event, bounds, cursor, clipboard, shell);
        } else {
            let state = tree
                .state
                .downcast_mut::<State<Link, Renderer::Paragraph>>();

            if state.deselect() {
                shell.invalidate_layout();
                shell.request_redraw();
            }
        }
    }

    fn mouse_interaction(
        &self,
        _tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        if self.hovered_link.is_some() {
            mouse::Interaction::Pointer
        } else if self.selectable && cursor.is_over(layout.bounds()) {
            mouse::Interaction::Text
        } else {
            mouse::Interaction::None
        }
    }
}

impl<Link, Message, Theme, Renderer> SelectableRichText<'_, Link, Message, Theme, Renderer>
where
    Link: Clone + 'static,
    Theme: Catalog,
    Renderer: text::Renderer,
{
    fn update_links(&self, tree: &mut Tree, event: &Event, shell: &mut Shell<'_, Message>) {
        let Some(on_link_clicked) = &self.on_link_click else {
            return;
        };

        let state = tree
            .state
            .downcast_mut::<State<Link, Renderer::Paragraph>>();

        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if self.hovered_link.is_some() {
                    state.span_pressed = self.hovered_link;
                    shell.capture_event();
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if let Some(span) = state.span_pressed
                    && Some(span) == self.hovered_link
                    && let Some(link) = state.spans.get(span).and_then(|span| span.link.clone())
                {
                    shell.publish(on_link_clicked(link));
                }

                state.span_pressed = None;
            }
            _ => {}
        }
    }

    fn update_selection(
        &self,
        tree: &mut Tree,
        event: &Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
    ) {
        let state = tree
            .state
            .downcast_mut::<State<Link, Renderer::Paragraph>>();

        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let Some(position) = cursor.position_in(bounds) else {
                    // A press anywhere else gives the selection up.
                    if state.deselect() {
                        shell.invalidate_layout();
                        shell.request_redraw();
                    }
                    return;
                };

                if self.hovered_link.is_some() {
                    return;
                }

                let click = mouse::Click::new(position, mouse::Button::Left, state.last_click);
                let offset = state.offset_at(position);

                let (anchor, focus) = match click.kind() {
                    mouse::click::Kind::Single => (offset, offset),
                    mouse::click::Kind::Double => {
                        let word = word_range(&state.text, offset);
                        (word.start, word.end)
                    }
                    mouse::click::Kind::Triple => (0, state.text.len()),
                };

                state.last_click = Some(click);
                state.selection = Selection {
                    anchor: Some(anchor),
                    focus: Some(focus),
                    dragging: true,
                };

                if let Some(on_select) = &self.on_select {
                    shell.publish(on_select());
                }

                shell.invalidate_layout();
                shell.request_redraw();
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) if state.selection.dragging => {
                // Outside the widget the paragraph still answers, clamping to its
                // first or last line, which is the drag behaviour wanted anyway.
                let Some(position) = cursor.position() else {
                    return;
                };

                let focus = state.offset_at(position - Vector::new(bounds.x, bounds.y));

                if state.selection.focus != Some(focus) {
                    state.selection.focus = Some(focus);
                    shell.invalidate_layout();
                    shell.request_redraw();
                }

                shell.capture_event();
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
                if state.selection.dragging =>
            {
                state.selection.dragging = false;

                // A press and release on one spot is a plain click, which
                // deselects.
                if state.selection.range().is_none() && state.deselect() {
                    shell.invalidate_layout();
                    shell.request_redraw();
                }

                shell.capture_event();
            }
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. })
                if modifiers.command() && is_copy_key(key) =>
            {
                if let Some(selected) = state.selected().filter(|text| !text.is_empty()) {
                    clipboard.write(clipboard::Kind::Standard, selected.to_owned());
                    shell.capture_event();
                }
            }
            _ => {}
        }
    }
}

fn is_copy_key(key: &keyboard::Key) -> bool {
    matches!(key, keyboard::Key::Character(character) if character.eq_ignore_ascii_case("c"))
}

impl<'a, Link, Message, Theme, Renderer> FromIterator<Span<'a, Link, Renderer::Font>>
    for SelectableRichText<'a, Link, Message, Theme, Renderer>
where
    Link: Clone + 'a,
    Theme: Catalog,
    Renderer: text::Renderer,
    Renderer::Font: 'a,
{
    fn from_iter<T: IntoIterator<Item = Span<'a, Link, Renderer::Font>>>(spans: T) -> Self {
        Self::with_spans(spans.into_iter().collect::<Vec<_>>())
    }
}

impl<'a, Link, Message, Theme, Renderer>
    From<SelectableRichText<'a, Link, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Link: Clone + 'a,
    Theme: Catalog + 'a,
    Renderer: text::Renderer + 'a,
{
    fn from(
        text: SelectableRichText<'a, Link, Message, Theme, Renderer>,
    ) -> Element<'a, Message, Theme, Renderer> {
        Element::new(text)
    }
}

/// The text the spans concatenate to. Every offset in this file indexes it.
pub fn spans_text<Link, Font>(spans: &[Span<'_, Link, Font>]) -> String {
    spans.iter().map(|span| span.text.as_ref()).collect()
}

/// Where each span starts in [`spans_text`].
pub fn span_starts<Link, Font>(spans: &[Span<'_, Link, Font>]) -> Vec<usize> {
    let mut start = 0;

    spans
        .iter()
        .map(|span| {
            let at = start;
            start += span.text.len();
            at
        })
        .collect()
}

/// Where each hard line starts in `text`.
pub fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(text.match_indices('\n').map(|(at, _)| at + 1));
    starts
}

/// Whether two laid-out span lists are the same down to their decoration.
///
/// iced's own `PartialEq` for a span covers only what changes the shaping.
fn decorated_alike<'a, Link, Font: PartialEq>(
    left: &[Span<'a, Link, Font>],
    right: &[Span<'a, Link, Font>],
) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left == right
                && left.highlight == right.highlight
                && left.padding == right.padding
                && left.underline == right.underline
                && left.strikethrough == right.strikethrough
        })
}

/// Cuts `spans` so the selected range stands on its own, carrying `colour` as
/// its background.
///
/// A span that already has a background — a mention chip — keeps it outside the
/// selection and hands it over to `colour` inside, which is why the selection is
/// applied to a piece of the chip rather than to the chip as a whole. Everything
/// else about the span it came from is preserved, borders and padding included.
///
/// Spans are also cut at every hard line, whether or not anything is selected:
/// each line is laid out on its own, so a span crossing one would break the map
/// from a hit test back onto an offset.
pub fn split_selection<'a, Link, Font>(
    spans: &[Span<'a, Link, Font>],
    selection: Option<Range<usize>>,
    colour: Color,
) -> Vec<Span<'a, Link, Font>>
where
    Link: Clone,
    Font: Clone,
{
    let selection = selection.filter(|range| range.start < range.end);

    let mut cut = Vec::with_capacity(spans.len());
    let mut base = 0;

    for span in spans {
        let length = span.text.len();
        let mut at: Vec<usize> = span
            .text
            .match_indices('\n')
            .map(|(at, _)| at + 1)
            .filter(|at| *at < length)
            .collect();

        if let Some(range) = &selection {
            // An edge that is not a character boundary cuts nothing rather than
            // panicking on the slice.
            for edge in [range.start, range.end] {
                if edge > base && edge < base + length && span.text.is_char_boundary(edge - base) {
                    at.push(edge - base);
                }
            }
        }

        at.sort_unstable();
        at.dedup();

        let mut from = 0;
        for to in at.into_iter().chain(std::iter::once(length)) {
            let start = base + from;
            let piece = slice(span, from..to);

            cut.push(match &selection {
                Some(range) if range.contains(&start) => piece.background(colour),
                _ => piece,
            });

            from = to;
        }

        base += length;
    }

    cut
}

/// The span over a byte range of its own text, keeping everything else.
fn slice<'a, Link, Font>(span: &Span<'a, Link, Font>, range: Range<usize>) -> Span<'a, Link, Font>
where
    Link: Clone,
    Font: Clone,
{
    let mut piece = span.clone();

    piece.text = match span.text {
        Cow::Borrowed(text) => Cow::Borrowed(&text[range]),
        Cow::Owned(ref text) => Cow::Owned(text[range].to_owned()),
    };

    piece
}

/// The nearest offset at or before `offset` that starts a grapheme cluster.
///
/// No segmentation crate is in the client's dependency graph, so cluster
/// detection is an approximation over the combining ranges that actually turn up
/// in chat: marks, variation selectors, keycaps, emoji modifiers, zero-width
/// joiners and flags. Anything it misses still lands on a character boundary, so
/// the worst case is an ugly cut, never invalid UTF-8 and never a panic.
pub fn snap(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());

    loop {
        while offset > 0 && !text.is_char_boundary(offset) {
            offset -= 1;
        }

        if offset == 0 || !continues_cluster(text, offset) {
            return offset;
        }

        offset -= 1;
    }
}

fn continues_cluster(text: &str, offset: usize) -> bool {
    let (Some(after), Some(before)) = (
        text[offset..].chars().next(),
        text[..offset].chars().next_back(),
    ) else {
        return false;
    };

    if before == ZERO_WIDTH_JOINER || is_extender(after) {
        return true;
    }

    // A flag is a pair of regional indicators, so an odd run of them before the
    // offset means it falls inside one.
    is_regional(after)
        && text[..offset]
            .chars()
            .rev()
            .take_while(|character| is_regional(*character))
            .count()
            % 2
            == 1
}

const ZERO_WIDTH_JOINER: char = '\u{200d}';

fn is_extender(character: char) -> bool {
    matches!(
        character,
        '\u{0300}'..='\u{036f}'
            | '\u{1ab0}'..='\u{1aff}'
            | '\u{1dc0}'..='\u{1dff}'
            | '\u{200d}'
            | '\u{20d0}'..='\u{20f0}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{fe20}'..='\u{fe2f}'
            | '\u{1f3fb}'..='\u{1f3ff}'
            | '\u{e0100}'..='\u{e01ef}'
    )
}

fn is_regional(character: char) -> bool {
    ('\u{1f1e6}'..='\u{1f1ff}').contains(&character)
}

/// The run of same-kind characters around `offset`: a word, a stretch of
/// whitespace, or a stretch of punctuation.
pub fn word_range(text: &str, offset: usize) -> Range<usize> {
    let offset = snap(text, offset);

    // At the very end there is no character to classify, so the one before the
    // offset is what the click meant.
    let (at, character) = match text[offset..].chars().next() {
        Some(character) => (offset, character),
        None => match text[..offset].chars().next_back() {
            Some(character) => (offset - character.len_utf8(), character),
            None => return 0..0,
        },
    };

    let kind = kind_of(character);

    let mut start = at;
    for (index, character) in text[..at].char_indices().rev() {
        if kind_of(character) != kind {
            break;
        }
        start = index;
    }

    let mut end = at;
    for (index, character) in text[at..].char_indices() {
        if kind_of(character) != kind {
            break;
        }
        end = at + index + character.len_utf8();
    }

    start..end
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Word,
    Space,
    Punctuation,
}

fn kind_of(character: char) -> Kind {
    if character.is_alphanumeric() || character == '_' {
        Kind::Word
    } else if character.is_whitespace() {
        Kind::Space
    } else {
        Kind::Punctuation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use iced::{Font, border};

    type TestSpan = Span<'static, (), Font>;

    const HIGHLIGHT: Color = Color {
        r: 1.0,
        g: 0.0,
        b: 0.0,
        a: 0.5,
    };
    const CHIP: Color = Color {
        r: 0.0,
        g: 0.0,
        b: 1.0,
        a: 1.0,
    };

    fn spans(parts: &[&'static str]) -> Vec<TestSpan> {
        parts.iter().map(|part| Span::new(*part)).collect()
    }

    fn texts(spans: &[TestSpan]) -> Vec<&str> {
        spans.iter().map(|span| span.text.as_ref()).collect()
    }

    fn selected(spans: &[TestSpan], colour: Color) -> String {
        spans
            .iter()
            .filter(|span| {
                span.highlight
                    .is_some_and(|highlight| highlight.background == colour.into())
            })
            .map(|span| span.text.as_ref())
            .collect()
    }

    #[test]
    fn selection_inside_one_span() {
        let source = spans(&["hello world"]);
        let cut = split_selection(&source, Some(6..11), HIGHLIGHT);

        assert_eq!(texts(&cut), vec!["hello ", "world"]);
        assert_eq!(selected(&cut, HIGHLIGHT), "world");
    }

    #[test]
    fn selection_spanning_two_spans() {
        let source = spans(&["abc", "def"]);
        let cut = split_selection(&source, Some(2..5), HIGHLIGHT);

        assert_eq!(texts(&cut), vec!["ab", "c", "de", "f"]);
        assert_eq!(selected(&cut, HIGHLIGHT), "cde");
    }

    #[test]
    fn selection_covering_every_span() {
        let source = spans(&["abc", "def"]);
        let cut = split_selection(&source, Some(0..6), HIGHLIGHT);

        assert_eq!(texts(&cut), vec!["abc", "def"]);
        assert_eq!(selected(&cut, HIGHLIGHT), "abcdef");
    }

    #[test]
    fn no_selection_leaves_the_spans_alone() {
        let source = spans(&["abc", "def"]);

        // The last one is inverted, which is no selection either.
        for selection in [None, Some(3..3), Some(Range { start: 4, end: 2 })] {
            let cut = split_selection(&source, selection, HIGHLIGHT);

            assert_eq!(texts(&cut), vec!["abc", "def"]);
            assert_eq!(selected(&cut, HIGHLIGHT), "");
            assert!(cut.iter().all(|span| span.highlight.is_none()));
        }
    }

    #[test]
    fn selection_at_offset_zero() {
        let source = spans(&["abc"]);
        let cut = split_selection(&source, Some(0..1), HIGHLIGHT);

        assert_eq!(texts(&cut), vec!["a", "bc"]);
        assert_eq!(selected(&cut, HIGHLIGHT), "a");
    }

    #[test]
    fn selection_at_the_very_end() {
        let source = spans(&["abc"]);
        let cut = split_selection(&source, Some(2..3), HIGHLIGHT);

        assert_eq!(texts(&cut), vec!["ab", "c"]);
        assert_eq!(selected(&cut, HIGHLIGHT), "c");
    }

    #[test]
    fn spans_are_cut_at_every_hard_line() {
        let source = spans(&["one\ntwo", "\nthree"]);
        let cut = split_selection(&source, None, HIGHLIGHT);

        assert_eq!(texts(&cut), vec!["one\n", "two", "\n", "three"]);
        assert_eq!(span_starts(&cut), vec![0, 4, 7, 8]);
        assert_eq!(line_starts(&spans_text(&source)), vec![0, 4, 8]);
    }

    #[test]
    fn every_offset_round_trips_through_a_multibyte_split() {
        let source = spans(&["héllo ", "😀 wörld", " ☃"]);
        let text = spans_text(&source);

        for start in 0..=text.len() {
            for end in start..=text.len() {
                let cut = split_selection(&source, Some(start..end), HIGHLIGHT);

                assert_eq!(texts(&cut).concat(), text, "{start}..{end}");
                assert!(
                    selected(&cut, HIGHLIGHT).is_empty()
                        || text.contains(&selected(&cut, HIGHLIGHT)),
                    "{start}..{end}"
                );
            }
        }
    }

    #[test]
    fn a_split_never_lands_inside_a_character() {
        let source = spans(&["é😀"]);
        let text = spans_text(&source);

        for offset in 0..=text.len() {
            let cut = split_selection(&source, Some(offset..text.len()), HIGHLIGHT);

            assert!(
                cut.iter().all(|span| !span.text.is_empty()),
                "empty piece at {offset}"
            );
            assert_eq!(texts(&cut).concat(), text);
        }
    }

    #[test]
    fn a_chip_keeps_its_own_background_outside_the_selection() {
        let chip: TestSpan = Span::new("@alice")
            .color(CHIP)
            .background(CHIP)
            .border(border::rounded(4))
            .padding([0.0, 4.0])
            .underline(true);
        let source = vec![chip];

        let cut = split_selection(&source, Some(0..3), HIGHLIGHT);

        assert_eq!(texts(&cut), vec!["@al", "ice"]);
        assert_eq!(
            cut[0].highlight.map(|highlight| highlight.background),
            Some(HIGHLIGHT.into())
        );
        assert_eq!(
            cut[1].highlight.map(|highlight| highlight.background),
            Some(CHIP.into())
        );

        for piece in &cut {
            assert_eq!(piece.color, Some(CHIP));
            assert_eq!(piece.padding, source[0].padding);
            assert!(piece.underline);
            assert_eq!(
                piece.highlight.map(|highlight| highlight.border),
                source[0].highlight.map(|highlight| highlight.border)
            );
        }
    }

    #[test]
    fn snapping_lands_on_a_grapheme_boundary() {
        let text = "aé😀b";

        assert_eq!(snap(text, 0), 0);
        // Inside the two bytes of the accented letter.
        assert_eq!(snap(text, 2), 1);
        assert_eq!(snap(text, 3), 3);
        // Inside the four bytes of the emoji.
        assert_eq!(snap(text, 5), 3);
        assert_eq!(snap(text, 7), 7);
        // Past the end.
        assert_eq!(snap(text, 999), text.len());
    }

    #[test]
    fn snapping_steps_back_over_a_combining_mark() {
        let text = "e\u{0301}x";

        assert_eq!(snap(text, 1), 0);
        assert_eq!(snap(text, 3), 3);
    }

    #[test]
    fn snapping_steps_back_over_a_joined_emoji() {
        let family = "👩\u{200d}👧";

        assert_eq!(snap(family, 4), 0);
        assert_eq!(snap(family, 7), 0);
        // The end of the text ends the cluster too.
        assert_eq!(snap(family, family.len()), family.len());
    }

    #[test]
    fn snapping_steps_back_inside_a_flag() {
        let flag = "🇧🇷!";

        assert_eq!(snap(flag, 4), 0);
        assert_eq!(snap(flag, 8), 8);
    }

    #[test]
    fn a_word_is_bounded_by_whitespace_and_punctuation() {
        let text = "hello, big_world!";

        assert_eq!(word_range(text, 0), 0..5);
        assert_eq!(word_range(text, 4), 0..5);
        assert_eq!(word_range(text, 5), 5..6);
        assert_eq!(word_range(text, 6), 6..7);
        assert_eq!(word_range(text, 7), 7..16);
        assert_eq!(word_range(text, 16), 16..17);
    }

    #[test]
    fn a_run_of_punctuation_is_one_word() {
        let text = "wait...  really";

        assert_eq!(word_range(text, 4), 4..7);
        assert_eq!(word_range(text, 7), 7..9);
        assert_eq!(word_range(text, 9), 9..15);
    }

    #[test]
    fn a_word_at_the_end_of_the_text_is_the_one_before_the_offset() {
        let text = "one two";

        assert_eq!(word_range(text, text.len()), 4..7);
        assert_eq!(word_range("", 0), 0..0);
        assert_eq!(word_range("", 7), 0..0);
    }

    #[test]
    fn a_word_of_accented_letters_is_one_word() {
        let text = "olá mundo";

        assert_eq!(word_range(text, 0), 0..4);
        assert_eq!(&text[word_range(text, 0)], "olá");
        assert_eq!(&text[word_range(text, 5)], "mundo");
    }
}
