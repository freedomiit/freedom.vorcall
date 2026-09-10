//! The entrance animation played by the splash window.

use std::time::{Duration, Instant};

use iced::widget::canvas::{self, Canvas, Frame, Path, Program};
use iced::{
    Color, Element, Event, Length, Point, Rectangle, Renderer, Theme, Vector, keyboard, mouse,
};

use super::data::{self, Track};
use super::palette;

/// How long the last pose stays up before the splash gives way to the window.
const HOLD: Duration = Duration::from_millis(350);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    Wink,
    Rare,
}

/// Which entrance this launch plays, or `None` when the splash is switched off.
pub fn choose() -> Option<Variant> {
    variant_from(
        std::env::var("VORCALL_ENTRANCE").ok().as_deref(),
        rand::random_range(0..10),
    )
}

fn variant_from(setting: Option<&str>, roll: u32) -> Option<Variant> {
    let rolled = if roll == 0 {
        Variant::Rare
    } else {
        Variant::Wink
    };
    let setting = setting.map(|value| value.trim().to_ascii_lowercase());
    match setting.as_deref() {
        Some("off") => None,
        Some("rare") => Some(Variant::Rare),
        Some("wink") => Some(Variant::Wink),
        None | Some("") => Some(rolled),
        Some(other) => {
            tracing::warn!(value = other, "VORCALL_ENTRANCE takes off, wink or rare");
            Some(rolled)
        }
    }
}

pub struct Entrance {
    variant: Variant,
    started: Option<Instant>,
    elapsed: Duration,
}

impl Entrance {
    pub fn new(variant: Variant) -> Self {
        Self {
            variant,
            started: None,
            elapsed: Duration::ZERO,
        }
    }

    /// Moves the clock to `now`; the first call is what starts it.
    pub fn tick(&mut self, now: Instant) {
        let started = *self.started.get_or_insert(now);
        self.elapsed = now.saturating_duration_since(started);
    }

    pub fn finished(&self) -> bool {
        self.elapsed >= Duration::from_secs_f32(data::DURATION_SECS) + HOLD
    }

    fn progress(&self) -> f32 {
        (self.elapsed.as_secs_f32() / data::DURATION_SECS).clamp(0.0, 1.0)
    }

    pub fn view<'a, Message: Clone + 'a>(&'a self, on_skip: Message) -> Element<'a, Message> {
        Canvas::new(EntranceProgram {
            entrance: self,
            on_skip,
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }
}

/// The tracks a variant morphs, with the subpath split of the solid one.
struct Tracks {
    solid: &'static Track,
    counts: &'static [usize],
    eye_far: &'static Track,
    eye_near: &'static Track,
}

fn tracks_for(variant: Variant) -> Tracks {
    match variant {
        Variant::Wink => Tracks {
            solid: &data::INTRO_SOLID,
            counts: data::INTRO_SOLID_COUNTS,
            eye_far: &data::INTRO_EYE_FAR,
            eye_near: &data::INTRO_EYE_NEAR,
        },
        Variant::Rare => Tracks {
            solid: &data::RARE_SOLID,
            counts: data::RARE_SOLID_COUNTS,
            eye_far: &data::RARE_EYE_FAR,
            eye_near: &data::RARE_EYE_NEAR,
        },
    }
}

/// The key `p` falls in and how far it has travelled towards the next one.
fn segment(times: &[f32], p: f32) -> (usize, f32) {
    let last = times.len().saturating_sub(2);
    let k = times.iter().rposition(|&t| t <= p).unwrap_or(0).min(last);
    let span = times[k + 1] - times[k];
    let u = if span > 0.0 {
        (p - times[k]) / span
    } else {
        0.0
    };
    (k, u.clamp(0.0, 1.0))
}

/// A SMIL `keySplines` segment: the y of the cubic Bézier (0,0)-(x1,y1)-(x2,y2)-(1,1)
/// at the t where its x equals `u`, so solving `bx(t) = u` comes first. Newton lands it
/// in a step or two for these splines; bisection picks up the flat stretches where the
/// derivative all but vanishes and Newton overshoots.
fn ease(spline: [f32; 4], u: f32) -> f32 {
    let [x1, y1, x2, y2] = spline;
    if spline == [0.0, 0.0, 1.0, 1.0] {
        return u;
    }
    if u <= 0.0 {
        return 0.0;
    }
    if u >= 1.0 {
        return 1.0;
    }

    let bezier = |c1: f32, c2: f32, t: f32| {
        let v = 1.0 - t;
        3.0 * v * v * t * c1 + 3.0 * v * t * t * c2 + t * t * t
    };

    let mut t = u;
    for _ in 0..8 {
        let error = bezier(x1, x2, t) - u;
        if error.abs() < 1e-5 {
            break;
        }
        let v = 1.0 - t;
        let slope = 3.0 * v * v * x1 + 6.0 * v * t * (x2 - x1) + 3.0 * t * t * (1.0 - x2);
        if slope.abs() < 1e-6 {
            break;
        }
        t -= error / slope;
    }
    if (bezier(x1, x2, t) - u).abs() > 1e-4 {
        let (mut low, mut high) = (0.0, 1.0);
        for _ in 0..24 {
            t = 0.5 * (low + high);
            if bezier(x1, x2, t) < u {
                low = t;
            } else {
                high = t;
            }
        }
    }
    bezier(y1, y2, t).clamp(0.0, 1.0)
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// The track's points at `progress`, one entry per point of a key.
fn sample(track: &Track, progress: f32) -> Vec<[f32; 2]> {
    let (k, u) = segment(track.times, progress);
    let eased = ease(track.splines[k], u);
    let (from, to) = (track.keys[k], track.keys[k + 1]);
    from.iter()
        .zip(to.iter())
        .map(|(a, b)| [lerp(a[0], b[0], eased), lerp(a[1], b[1], eased)])
        .collect()
}

/// Linear interpolation over a `values`/`times` pair, the plain SMIL `calcMode="linear"`.
fn alpha(times: &[f32], values: &[f32], p: f32) -> f32 {
    let (k, u) = segment(times, p);
    lerp(values[k], values[k + 1], u)
}

fn subpath(builder: &mut canvas::path::Builder, points: &[[f32; 2]]) {
    let Some((first, rest)) = points.split_first() else {
        return;
    };
    builder.move_to(Point::new(first[0], first[1]));
    for point in rest {
        builder.line_to(Point::new(point[0], point[1]));
    }
    builder.close();
}

fn subpaths(builder: &mut canvas::path::Builder, points: &[[f32; 2]], counts: &[usize]) {
    let mut start = 0;
    for &count in counts {
        subpath(builder, &points[start..start + count]);
        start += count;
    }
}

struct EntranceProgram<'a, Message> {
    entrance: &'a Entrance,
    on_skip: Message,
}

impl<Message: Clone> Program<Message> for EntranceProgram<'_, Message> {
    type State = ();

    fn update(
        &self,
        _state: &mut Self::State,
        event: &Event,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(_))
            | Event::Keyboard(keyboard::Event::KeyPressed { .. }) => {
                Some(canvas::Action::publish(self.on_skip.clone()))
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let p = self.entrance.progress();
        let tracks = tracks_for(self.entrance.variant);

        let side = bounds.width.min(bounds.height);
        let mut frame = Frame::new(renderer, bounds.size());
        frame.translate(Vector::new(
            (bounds.width - side) / 2.0,
            (bounds.height - side) / 2.0,
        ));
        frame.scale(side / data::VIEW);

        let solids = sample(tracks.solid, p);
        let eye_far = sample(tracks.eye_far, p);
        let eye_near = sample(tracks.eye_near, p);
        let creature = Path::new(|builder| {
            subpaths(builder, &solids, tracks.counts);
            subpath(builder, &eye_far);
            subpath(builder, &eye_near);
        });
        // A `Color` fills nonzero, which is what the winding of the keys is built for:
        // even-odd would punch holes where the horns and the arms cross the body.
        frame.fill(&creature, palette::DEEP);

        if self.entrance.variant == Variant::Rare {
            let opacity = alpha(data::RARE_CREASE_ALPHA_TIMES, data::RARE_CREASE_ALPHA, p);
            if opacity > 0.0 {
                let points = sample(&data::RARE_CREASES, p);
                let creases = Path::new(|builder| {
                    subpaths(builder, &points, data::RARE_CREASE_COUNTS);
                });
                frame.fill(
                    &creases,
                    Color {
                        a: opacity,
                        ..palette::CREASE
                    },
                );
            }
        }

        vec![frame.into_geometry()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_shape(track: &Track, points: usize) {
        assert_eq!(track.keys.len(), track.times.len());
        assert_eq!(track.splines.len(), track.times.len() - 1);
        for key in track.keys {
            assert_eq!(key.len(), points);
        }
    }

    #[test]
    fn tracks_hold_the_generator_contract() {
        let intro: usize = data::INTRO_SOLID_COUNTS.iter().sum();
        assert_shape(&data::INTRO_SOLID, intro);
        assert_shape(&data::INTRO_EYE_FAR, 24);
        assert_shape(&data::INTRO_EYE_NEAR, 24);

        let rare: usize = data::RARE_SOLID_COUNTS.iter().sum();
        assert_shape(&data::RARE_SOLID, rare);
        assert_shape(&data::RARE_EYE_FAR, 24);
        assert_shape(&data::RARE_EYE_NEAR, 24);

        let creases: usize = data::RARE_CREASE_COUNTS.iter().sum();
        assert_eq!(creases, 36);
        assert_shape(&data::RARE_CREASES, creases);

        assert_eq!(
            data::RARE_CREASE_ALPHA.len(),
            data::RARE_CREASE_ALPHA_TIMES.len()
        );
    }

    #[test]
    fn the_linear_spline_is_the_identity() {
        for u in [0.0, 0.2, 0.5, 0.9, 1.0] {
            assert_eq!(ease([0.0, 0.0, 1.0, 1.0], u), u);
        }
    }

    #[test]
    fn a_symmetric_spline_rises_through_its_middle() {
        let spline = [0.4, 0.0, 0.6, 1.0];
        assert_eq!(ease(spline, 0.0), 0.0);
        assert_eq!(ease(spline, 1.0), 1.0);
        assert!((ease(spline, 0.5) - 0.5).abs() < 1e-3);

        let mut previous = -1.0;
        for step in 0..=10 {
            let eased = ease(spline, step as f32 / 10.0);
            assert!(eased > previous, "not increasing at step {step}");
            previous = eased;
        }
    }

    #[test]
    fn segments_cover_both_ends() {
        let track = &data::INTRO_EYE_NEAR;
        assert_eq!(segment(track.times, 0.0), (0, 0.0));

        // The boundary belongs to either segment; both must land on the key itself.
        for (got, want) in sample(track, track.times[1]).iter().zip(track.keys[1]) {
            assert!((got[0] - want[0]).abs() < 1e-4, "{got:?} vs {want:?}");
            assert!((got[1] - want[1]).abs() < 1e-4, "{got:?} vs {want:?}");
        }

        assert_eq!(segment(track.times, 1.0), (track.times.len() - 2, 1.0));
        assert_eq!(
            sample(track, 1.0),
            track.keys[track.keys.len() - 1].to_vec()
        );
    }

    #[test]
    fn the_creases_fade_in_between_their_times() {
        let times = data::RARE_CREASE_ALPHA_TIMES;
        let values = data::RARE_CREASE_ALPHA;
        assert_eq!(alpha(times, values, 0.5), 0.0);
        assert!((alpha(times, values, 0.55) - 0.5).abs() < 1e-4);
        assert_eq!(alpha(times, values, 0.6), 1.0);
    }

    #[test]
    fn the_setting_beats_the_roll() {
        assert_eq!(variant_from(Some("off"), 0), None);
        assert_eq!(variant_from(Some("RARE "), 7), Some(Variant::Rare));
        assert_eq!(variant_from(Some("wink"), 0), Some(Variant::Wink));
        assert_eq!(variant_from(None, 0), Some(Variant::Rare));
        assert_eq!(variant_from(None, 3), Some(Variant::Wink));
        assert_eq!(variant_from(Some(""), 3), Some(Variant::Wink));
        assert_eq!(variant_from(Some("bogus"), 0), Some(Variant::Rare));
    }

    #[test]
    fn the_clock_starts_on_the_first_tick() {
        let start = Instant::now();
        let mut entrance = Entrance::new(Variant::Wink);
        assert!(!entrance.finished());

        entrance.tick(start);
        assert!(!entrance.finished());
        assert_eq!(entrance.progress(), 0.0);

        entrance.tick(start + Duration::from_millis(900));
        assert!((entrance.progress() - 0.5).abs() < 1e-3);
        assert!(!entrance.finished());

        entrance.tick(start + Duration::from_millis(2200));
        assert_eq!(entrance.progress(), 1.0);
        assert!(entrance.finished());
    }
}
