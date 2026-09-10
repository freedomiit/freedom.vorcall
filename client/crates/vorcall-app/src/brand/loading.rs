//! The loading creature, `assets/brand/loading.svg` drawn on a canvas.
//!
//! The SVG animates in CSS; this mirrors that schedule. One 9.6 s cycle runs two scenes of
//! 4.8 s: the creature bobs in place with a little squash at the bottom, then it types on a
//! laptop while its eyes scan the screen. `gen.py` writes both the SVG and the geometry in
//! `data.rs`, so the numbers here are only the timing the CSS carries.

use std::time::{Duration, Instant};

use iced::widget::canvas::{self, Canvas, Frame, Path, Program};
use iced::{Element, Point, Rectangle, Renderer, Size, Theme, Vector, border, mouse};

use super::{data, mark, palette};

/// How high the bob lifts the creature (`ld-bob`).
const BOB_RISE: f32 = 6.0;
/// The squash at the ends of the bob and the stretch at its top (`ld-squash`).
const SQUASH_LOW: [f32; 2] = [1.04, 0.96];
const SQUASH_HIGH: [f32; 2] = [0.99, 1.01];
/// What the squash scales about: the creature's feet (`transform-origin` of `.b-squash`).
const GROUND: [f32; 2] = [128.0, 226.0];
/// The typing scene sits the creature higher, behind the laptop.
const TYPING_RISE: f32 = -20.0;
/// One tap of a typing hand, in seconds (`typing_arm_keyframes`'s `tap`).
const TAP_SECS: f32 = 0.2;
/// How far a tapping hand lifts off the keys.
const TAP_RISE: f32 = -4.0;
/// The two windows `typing_arm_keyframes` fills with taps, and the rest between them, as
/// fractions of the typing scene. A window takes eight keyframes, the last one short of its end.
const TAPS_PER_RUN: usize = 8;
const RUN_ONE_END: f32 = 0.333;
const REST: [f32; 2] = [0.334, 0.666];
const RUN_TWO_START: f32 = 0.667;
const TAP_KEYS: usize = TAPS_PER_RUN * 2 + 3;

/// `ld-scan`: the eyes sweep the screen left to right, twice, then settle.
const SCAN: [(f32, [f32; 2]); 8] = [
    (0.00, [0.0, 6.0]),
    (0.33, [0.0, 6.0]),
    (0.35, [-7.0, 5.0]),
    (0.49, [7.0, 5.0]),
    (0.51, [-7.0, 5.0]),
    (0.65, [7.0, 5.0]),
    (0.67, [0.0, 6.0]),
    (1.00, [0.0, 6.0]),
];

/// The clock the canvas reads; the creature loops for as long as it is on screen.
pub struct Loading {
    started: Instant,
}

impl Loading {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }

    pub fn elapsed(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.started)
    }
}

impl Default for Loading {
    fn default() -> Self {
        Self::new()
    }
}

/// The loading creature (`assets/brand/loading.svg`) as a canvas, `size` units square.
pub fn view<'a, Message: 'a>(elapsed: Duration, size: f32) -> Element<'a, Message> {
    Canvas::new(LoadingProgram { elapsed })
        .width(size)
        .height(size)
        .into()
}

/// Which scene the cycle is in, with that scene's own clock: the bob counts seconds because
/// it loops three times inside its window, the typing scene runs `0..1` once.
enum Scene {
    Bob(f32),
    Typing(f32),
}

fn scene(elapsed: Duration) -> Scene {
    // Taken in f64 so hours of uptime keep their resolution; a remainder a hair under the
    // period rounds up to it in f32, and that is the top of the next cycle, not the end of
    // this one.
    let t = (elapsed.as_secs_f64() % f64::from(data::LOADING_PERIOD_SECS)) as f32;
    let t = if t >= data::LOADING_PERIOD_SECS {
        0.0
    } else {
        t
    };
    if t < data::LOADING_SCENE_SECS {
        Scene::Bob(t)
    } else {
        Scene::Typing((t - data::LOADING_SCENE_SECS) / data::LOADING_SCENE_SECS)
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Linear interpolation through CSS keyframes given as `(time, value)` in ascending time.
fn sample<const N: usize>(keys: &[(f32, [f32; N])], f: f32) -> [f32; N] {
    let last = keys.len().saturating_sub(2);
    let k = keys
        .iter()
        .rposition(|(time, _)| *time <= f)
        .unwrap_or(0)
        .min(last);
    let (time, from) = keys[k];
    let (next, to) = keys[k + 1];
    let span = next - time;
    let u = if span > 0.0 {
        ((f - time) / span).clamp(0.0, 1.0)
    } else {
        0.0
    };
    std::array::from_fn(|i| lerp(from[i], to[i], u))
}

/// CSS `ease-in-out`, cubic-bezier(0.42, 0, 0.58, 1): the y of the curve where its x reaches
/// `u`. Bisection, because the spline is fixed and monotonic and this runs once a frame.
fn ease_in_out(u: f32) -> f32 {
    const X1: f32 = 0.42;
    const X2: f32 = 0.58;

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

    let (mut low, mut high) = (0.0, 1.0);
    let mut t = u;
    for _ in 0..24 {
        t = 0.5 * (low + high);
        if bezier(X1, X2, t) < u {
            low = t;
        } else {
            high = t;
        }
    }
    // y1 = 0, y2 = 1.
    bezier(0.0, 1.0, t).clamp(0.0, 1.0)
}

/// The bob's triangle over one loop: 0 at the ends, 1 in the middle, each half eased the way
/// CSS eases the segment between two keyframes.
fn tri(p: f32) -> f32 {
    if p < 0.5 {
        ease_in_out(p * 2.0)
    } else {
        1.0 - ease_in_out(p * 2.0 - 1.0)
    }
}

/// `ld-tap-l` and `ld-tap-r`: eight alternating keyframes in each typing window, the two
/// hands starting on opposite feet, flat between the windows and back to rest at the end.
/// `lifted_first` is the hand that starts off the keys, at `TAP_RISE`.
fn tap_keys(lifted_first: bool) -> [(f32, [f32; 1]); TAP_KEYS] {
    let step = TAP_SECS / data::LOADING_SCENE_SECS;
    let level = |k: usize| {
        if k.is_multiple_of(2) == lifted_first {
            [TAP_RISE]
        } else {
            [0.0]
        }
    };

    // What `typing_arm_keyframes`'s `while t < end` writes out: the window takes one more
    // keyframe than fits whole taps, and stops short of its end.
    debug_assert!((TAPS_PER_RUN - 1) as f32 * step < RUN_ONE_END);
    debug_assert!(TAPS_PER_RUN as f32 * step >= RUN_ONE_END);

    let mut keys = [(0.0, [0.0]); TAP_KEYS];
    for k in 0..TAPS_PER_RUN {
        keys[k] = (k as f32 * step, level(k));
        keys[TAPS_PER_RUN + 2 + k] = (RUN_TWO_START + k as f32 * step, level(k));
    }
    keys[TAPS_PER_RUN] = (REST[0], [0.0]);
    keys[TAPS_PER_RUN + 1] = (REST[1], [0.0]);
    // The keyframes stop short of 100 %; CSS carries the hand back to its untransformed
    // place over what is left.
    keys[TAP_KEYS - 1] = (1.0, [0.0]);
    keys
}

/// One typing hand's lift at `f`, `lifted_first` starting it off the keys. The keyframes
/// alternate between lifted and resting, but the animation is `linear`, so every pair is a
/// ramp: a tap is a triangle, not a step.
fn tap(f: f32, lifted_first: bool) -> f32 {
    sample(&tap_keys(lifted_first), f)[0]
}

fn chain(builder: &mut canvas::path::Builder, commands: &[data::Cmd], offset: Vector) {
    let at = |x: f32, y: f32| Point::new(x + offset.x, y + offset.y);
    for command in commands {
        match *command {
            data::Cmd::M(x, y) => builder.move_to(at(x, y)),
            data::Cmd::L(x, y) => builder.line_to(at(x, y)),
            data::Cmd::C(x1, y1, x2, y2, x, y) => {
                builder.bezier_curve_to(at(x1, y1), at(x2, y2), at(x, y));
            }
            data::Cmd::Z => builder.close(),
        }
    }
}

fn polygon(builder: &mut canvas::path::Builder, points: &[[f32; 2]], offset: Vector) {
    let Some((first, rest)) = points.split_first() else {
        return;
    };
    builder.move_to(Point::new(first[0] + offset.x, first[1] + offset.y));
    for point in rest {
        builder.line_to(Point::new(point[0] + offset.x, point[1] + offset.y));
    }
    builder.close();
}

/// One typing arm and its hand, lifted off the keys by `lift`.
fn typing_arm(builder: &mut canvas::path::Builder, arm: &[[f32; 2]], hand: [f32; 3], lift: f32) {
    polygon(builder, arm, Vector::new(0.0, lift));
    builder.circle(Point::new(hand[0], hand[1] + lift), hand[2]);
}

fn rounded(rect: [f32; 5]) -> Path {
    let [x, y, width, height, radius] = rect;
    Path::rounded_rectangle(
        Point::new(x, y),
        Size::new(width, height),
        border::Radius::from(radius),
    )
}

fn draw_bob(frame: &mut Frame, seconds: f32) {
    let rise = tri((seconds % data::LOADING_BOB_SECS) / data::LOADING_BOB_SECS);
    let creature = Path::new(|builder| {
        chain(builder, data::LOADING_BODY, Vector::ZERO);
        chain(builder, data::LOADING_ARMS, Vector::ZERO);
        chain(builder, data::LOADING_EYES, Vector::ZERO);
    });

    frame.with_save(|frame| {
        frame.translate(Vector::new(0.0, -BOB_RISE * rise));
        frame.translate(Vector::new(GROUND[0], GROUND[1]));
        frame.scale_nonuniform(Vector::new(
            lerp(SQUASH_LOW[0], SQUASH_HIGH[0], rise),
            lerp(SQUASH_LOW[1], SQUASH_HIGH[1], rise),
        ));
        frame.translate(Vector::new(-GROUND[0], -GROUND[1]));
        // One path with the eyes wound against the body: a `Color` fills nonzero, so they
        // come out as holes instead of red on red.
        frame.fill(&creature, palette::DEEP);
    });
}

fn draw_typing(frame: &mut Frame, f: f32) {
    let rise = Vector::new(0.0, TYPING_RISE);
    let scan = sample(&SCAN, f);
    let creature = Path::new(|builder| {
        chain(builder, data::LOADING_BODY, rise);
        chain(
            builder,
            data::LOADING_EYES,
            Vector::new(rise.x + scan[0], rise.y + scan[1]),
        );
    });
    frame.fill(&creature, palette::DEEP);

    let deck = Path::new(|builder| polygon(builder, data::LOADING_DECK, Vector::ZERO));
    frame.fill(&deck, palette::DECK);
    frame.fill(&rounded(data::LOADING_KEYS), palette::KEYS);

    let hands = Path::new(|builder| {
        typing_arm(
            builder,
            data::LOADING_TYPING_ARM_L,
            data::LOADING_HAND_L,
            tap(f, true),
        );
        typing_arm(
            builder,
            data::LOADING_TYPING_ARM_R,
            data::LOADING_HAND_R,
            tap(f, false),
        );
    });
    frame.fill(&hands, palette::DEEP);

    frame.fill(&rounded(data::LOADING_LID), palette::STEEL);

    let [tx, ty, sx, sy] = data::LOADING_LOGO;
    frame.with_save(|frame| {
        frame.translate(Vector::new(tx, ty));
        frame.scale_nonuniform(Vector::new(sx, sy));
        frame.translate(Vector::new(-data::VIEW / 2.0, -data::VIEW / 2.0));
        frame.fill(&mark::path(), palette::DEEP);
    });
}

struct LoadingProgram {
    elapsed: Duration,
}

impl<Message> Program<Message> for LoadingProgram {
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

        match scene(self.elapsed) {
            Scene::Bob(seconds) => draw_bob(&mut frame, seconds),
            Scene::Typing(f) => draw_typing(&mut frame, f),
        }

        vec![frame.into_geometry()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(seconds: f32) -> Duration {
        Duration::from_secs_f32(seconds)
    }

    #[test]
    fn the_cycle_bobs_then_types() {
        assert!(matches!(scene(secs(0.0)), Scene::Bob(t) if t == 0.0));
        assert!(matches!(scene(secs(4.79)), Scene::Bob(t) if (t - 4.79).abs() < 1e-3));
        assert!(matches!(scene(secs(4.8)), Scene::Typing(f) if f.abs() < 1e-3));
        assert!(matches!(scene(secs(9.59)), Scene::Typing(f) if (f - 0.998).abs() < 1e-3));
        // The end of the cycle is the top of the next one.
        assert!(matches!(scene(secs(9.6)), Scene::Bob(t) if t == 0.0));
        // Hours in, the cycle still lands where it should.
        assert!(matches!(scene(secs(9.6 * 1000.0 + 2.4)), Scene::Bob(t) if (t - 2.4).abs() < 1e-2));
    }

    #[test]
    fn the_bob_rises_and_falls() {
        assert_eq!(tri(0.0), 0.0);
        assert_eq!(tri(0.5), 1.0);
        assert!(tri(1.0).abs() < 1e-4);
        assert_eq!(-BOB_RISE * tri(0.5), -6.0);
    }

    #[test]
    fn the_bob_is_eased_at_both_ends() {
        assert_eq!(ease_in_out(0.0), 0.0);
        assert_eq!(ease_in_out(1.0), 1.0);
        assert!((ease_in_out(0.5) - 0.5).abs() < 1e-3);
        assert!(ease_in_out(0.25) < 0.25, "the start should be slow");
        assert!(ease_in_out(0.75) > 0.75, "the end should be slow");
    }

    #[test]
    fn the_eyes_scan_and_settle() {
        assert_eq!(sample(&SCAN, 0.2), [0.0, 6.0]);
        assert_eq!(sample(&SCAN, 0.35), [-7.0, 5.0]);
        assert_eq!(sample(&SCAN, 0.8), [0.0, 6.0]);
    }

    #[test]
    fn the_hands_tap_in_turn() {
        let step = TAP_SECS / data::LOADING_SCENE_SECS;
        // `lifted_first`: the hand that starts off the keys, against the one resting on them.
        assert_eq!(tap(0.0, true), TAP_RISE);
        assert_eq!(tap(0.0, false), 0.0);
        assert!(tap(step, true).abs() < 1e-3);
        assert_eq!(tap(step, false), TAP_RISE);
        assert!(tap(0.0417, true).abs() < 1e-2);

        // Both rest between the two typing windows.
        assert_eq!(tap(0.5, true), 0.0);
        assert_eq!(tap(0.5, false), 0.0);

        // `linear` timing: halfway between two keyframes is halfway down, not a step.
        assert!((tap(step / 2.0, true) - TAP_RISE / 2.0).abs() < 1e-3);

        // The second window starts the same way round as the first.
        assert!((tap(RUN_TWO_START, true) - TAP_RISE).abs() < 1e-3);
        assert!(tap(RUN_TWO_START, false).abs() < 1e-3);

        // The right hand is still lifted at the last keyframe and comes down by the end.
        assert!((tap(RUN_TWO_START + 7.0 * step, false) - TAP_RISE).abs() < 1e-3);
        assert_eq!(tap(1.0, false), 0.0);
    }
}
