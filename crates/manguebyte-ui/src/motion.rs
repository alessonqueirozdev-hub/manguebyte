//! Motion: values that move to a new target over a short time, polled on
//! the frame clock.
//!
//! An [`Animated`] value is plain data a widget keeps. It is given a new
//! target with the frame's time ([`Animated::set`]) and asked for its
//! value at a later frame's time ([`Animated::tick`]); nothing runs on its
//! own, nothing is called back and no timer is started (ADR 0003, ADR
//! 0011). The root drives every frame from the window's frame timer with
//! the time it is given, so a test that gives the times decides exactly
//! what each frame shows.
//!
//! How long a change takes and how it eases come from the theme's
//! [`Motion`] tokens: [`fast`](Motion::fast) (about 120 ms) for hover and
//! press, [`base`](Motion::base) (about 180 ms) for focus, selection and
//! switches, [`slow`](Motion::slow) (about 250 ms) for things that travel
//! further. With motion turned off, by the theme or by the system's
//! setting (see [`Root::set_system_motion`](crate::Root::set_system_motion)),
//! every duration is zero and changes jump to their end.
//!
//! The root animates three amounts for every widget itself, which a
//! widget reads while it draws: how far the pointer's hover
//! ([`PaintCx::hover`](crate::PaintCx::hover)), a press
//! ([`PaintCx::press`](crate::PaintCx::press)) and the visible focus ring
//! ([`PaintCx::focus_ring`](crate::PaintCx::focus_ring)) have come in.
//! While one moves, only that widget is drawn again.
//!
//! ```
//! use std::time::Duration;
//! use manguebyte_ui::motion::{Animated, Easing};
//!
//! let ms = Duration::from_millis;
//! let mut opacity = Animated::new(0.0_f32);
//! opacity.set(1.0, ms(0), ms(100), Easing::Linear);
//! assert!(opacity.tick(ms(50)));
//! assert_eq!(opacity.value(), 0.5);
//! assert!(opacity.is_running());
//! assert!(opacity.tick(ms(100)));
//! assert_eq!(opacity.value(), 1.0);
//! assert!(!opacity.is_running());
//! // Settled: nothing moves, nothing to draw.
//! assert!(!opacity.tick(ms(200)));
//! ```

use std::time::Duration;

use manguebyte_gfx::{Color, Point, Rect};

/// How a change is spread over its duration: a function from the
/// fraction of the time passed, 0 to 1, to the fraction of the way
/// travelled, which starts at 0 and ends at 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Easing {
    /// The same speed throughout.
    Linear,
    /// A cubic Bézier curve from (0, 0) to (1, 1) through two control
    /// points, as CSS's `cubic-bezier(x1, y1, x2, y2)`. The `x`s are
    /// clamped to `0 ..= 1`.
    CubicBezier(f32, f32, f32, f32),
    /// A spring released towards the target: with `bounce` 0 it settles
    /// without passing the target (critically damped); towards 1 it
    /// overshoots and swings back more. It is scaled so that it has
    /// settled to a thousandth of the way by the end of the duration, and
    /// ends exactly on the target.
    Spring {
        /// How much it overshoots, from 0 to 0.9.
        bounce: f32,
    },
}

impl Easing {
    /// The standard curve: quick to start, gentle to land,
    /// `cubic-bezier(.2, .8, .2, 1)`.
    pub const STANDARD: Easing = Easing::CubicBezier(0.2, 0.8, 0.2, 1.0);

    /// A spring that overshoots a little.
    pub const SPRING: Easing = Easing::Spring { bounce: 0.25 };

    /// The fraction of the way travelled once `t` of the time has passed:
    /// 0 at or before 0, 1 at or after 1.
    pub fn apply(self, t: f32) -> f32 {
        if t.is_nan() || t <= 0.0 {
            return 0.0;
        }
        if t >= 1.0 {
            return 1.0;
        }
        match self {
            Easing::Linear => t,
            Easing::CubicBezier(x1, y1, x2, y2) => bezier(
                f64::from(x1.clamp(0.0, 1.0)),
                f64::from(y1),
                f64::from(x2.clamp(0.0, 1.0)),
                f64::from(y2),
                f64::from(t),
            ) as f32,
            Easing::Spring { bounce } => spring(f64::from(bounce), f64::from(t)) as f32,
        }
    }
}

/// `y` of the cubic Bézier curve with control points (x1, y1) and (x2, y2)
/// where its `x` is `x`, found by bisection: 40 halvings, so the same
/// answer on every machine, to far below a pixel.
fn bezier(x1: f64, y1: f64, x2: f64, y2: f64, x: f64) -> f64 {
    let along = |a: f64, b: f64, t: f64| {
        let u = 1.0 - t;
        3.0 * u * u * t * a + 3.0 * u * t * t * b + t * t * t
    };
    // `x` grows with `t` while the control `x`s lie within 0 ..= 1.
    let (mut low, mut high) = (0.0f64, 1.0f64);
    for _ in 0..40 {
        let mid = 0.5 * (low + high);
        if along(x1, x2, mid) < x {
            low = mid;
        } else {
            high = mid;
        }
    }
    along(y1, y2, 0.5 * (low + high))
}

/// A damped spring's way travelled at time `t` of 1 (see
/// [`Easing::Spring`]).
fn spring(bounce: f64, t: f64) -> f64 {
    let zeta = 1.0 - bounce.clamp(0.0, 0.9);
    // e^(-zeta * omega) = 1/1000 at t = 1.
    let decay = 1000.0f64.ln();
    let omega = decay / zeta;
    if zeta >= 1.0 {
        return 1.0 - (-omega * t).exp() * (1.0 + omega * t);
    }
    let damped = omega * (1.0 - zeta * zeta).sqrt();
    let envelope = (-decay * t).exp();
    1.0 - envelope * ((damped * t).cos() + (decay / damped) * (damped * t).sin())
}

/// Something that can be blended between two values.
pub trait Lerp: Copy + PartialEq {
    /// The value `t` of the way from `self` to `to`; `t` may pass 1 for a
    /// spring that overshoots.
    fn lerp(self, to: Self, t: f32) -> Self;
}

impl Lerp for f32 {
    fn lerp(self, to: f32, t: f32) -> f32 {
        if t == 1.0 {
            return to;
        }
        self + (to - self) * t
    }
}

impl Lerp for Point {
    fn lerp(self, to: Point, t: f32) -> Point {
        Point::new(self.x.lerp(to.x, t), self.y.lerp(to.y, t))
    }
}

impl Lerp for Rect {
    fn lerp(self, to: Rect, t: f32) -> Rect {
        Rect::new(
            self.x.lerp(to.x, t),
            self.y.lerp(to.y, t),
            self.width.lerp(to.width, t),
            self.height.lerp(to.height, t),
        )
    }
}

impl Lerp for Color {
    /// Each channel on its own, straight alpha, as gradients mix them;
    /// rounded to the nearest level and clamped.
    fn lerp(self, to: Color, t: f32) -> Color {
        let mix = |a: u8, b: u8| {
            let v = f32::from(a) + (f32::from(b) - f32::from(a)) * t;
            (v + 0.5).clamp(0.0, 255.0) as u8
        };
        Color::rgba(
            mix(self.r, to.r),
            mix(self.g, to.g),
            mix(self.b, to.b),
            mix(self.a, to.a),
        )
    }
}

/// A value moving towards a target over a duration, on times the caller
/// gives.
///
/// It is cheap and inert: [`set`](Animated::set) remembers where it
/// starts, where it goes and when; [`tick`](Animated::tick) works out the
/// value at a frame's time. A change of target while it moves starts from
/// where it is, so a hover that ends half way turns back smoothly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Animated<T> {
    from: T,
    to: T,
    value: T,
    start: Duration,
    duration: Duration,
    easing: Easing,
    running: bool,
}

impl<T: Lerp> Animated<T> {
    /// A value resting at `value`.
    pub fn new(value: T) -> Self {
        Animated {
            from: value,
            to: value,
            value,
            start: Duration::ZERO,
            duration: Duration::ZERO,
            easing: Easing::Linear,
            running: false,
        }
    }

    /// The value as of the last [`tick`](Animated::tick) (or change).
    pub fn value(&self) -> T {
        self.value
    }

    /// Where it is going.
    pub fn target(&self) -> T {
        self.to
    }

    /// Whether it has not reached its target yet.
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Moves towards `target` from where it is now, starting at `now` and
    /// taking `duration` with `easing`. A target it already has changes
    /// nothing; a zero duration jumps there at once. Returns whether the
    /// target changed.
    pub fn set(&mut self, target: T, now: Duration, duration: Duration, easing: Easing) -> bool {
        if target == self.to {
            return false;
        }
        self.to = target;
        if duration.is_zero() {
            self.jump(target);
            return true;
        }
        self.from = self.value;
        self.start = now;
        self.duration = duration;
        self.easing = easing;
        self.running = true;
        true
    }

    /// Moves towards `target` as [`set`](Animated::set) does, with a
    /// transition's duration and easing.
    pub fn transition(&mut self, target: T, now: Duration, transition: Transition) -> bool {
        self.set(target, now, transition.duration, transition.easing)
    }

    /// Puts it at `value` at once, and stops it.
    pub fn jump(&mut self, value: T) {
        self.from = value;
        self.to = value;
        self.value = value;
        self.running = false;
    }

    /// Works out the value at `now`, and returns whether it changed. Once
    /// the duration has passed it is the target and it stops; a time
    /// before the start counts as the start.
    pub fn tick(&mut self, now: Duration) -> bool {
        if !self.running {
            return false;
        }
        let passed = now.saturating_sub(self.start);
        let value = if passed >= self.duration {
            self.running = false;
            self.to
        } else {
            let t = passed.as_secs_f64() / self.duration.as_secs_f64();
            self.from.lerp(self.to, self.easing.apply(t as f32))
        };
        let changed = value != self.value;
        self.value = value;
        changed
    }
}

impl<T: Lerp + Default> Default for Animated<T> {
    fn default() -> Self {
        Animated::new(T::default())
    }
}

/// How long a change takes and how it eases.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transition {
    /// How long.
    pub duration: Duration,
    /// How it eases.
    pub easing: Easing,
}

impl Transition {
    /// A change of `millis` milliseconds with `easing`.
    pub const fn millis(millis: u64, easing: Easing) -> Self {
        Transition {
            duration: Duration::from_millis(millis),
            easing,
        }
    }

    /// No transition: a jump.
    pub const NONE: Transition = Transition::millis(0, Easing::Linear);
}

/// Motion tokens: how long changes take and how they ease, and whether
/// the interface moves at all.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Motion {
    /// Whether changes animate. When false every transition is a jump, as
    /// people who ask their system for less motion expect.
    pub enabled: bool,
    /// Small, frequent changes: hover, press. About 120 ms.
    pub fast: Duration,
    /// Most changes: focus, selection, a switch. About 180 ms.
    pub base: Duration,
    /// Changes that travel further: panels, menus. About 250 ms.
    pub slow: Duration,
    /// The curve most changes ease along.
    pub easing: Easing,
    /// The curve for things that should feel physical, such as a switch's
    /// thumb.
    pub spring: Easing,
}

impl Motion {
    /// The default tokens: 120, 180 and 250 ms, the standard curve and a
    /// gentle spring.
    pub const STANDARD: Motion = Motion {
        enabled: true,
        fast: Duration::from_millis(120),
        base: Duration::from_millis(180),
        slow: Duration::from_millis(250),
        easing: Easing::STANDARD,
        spring: Easing::SPRING,
    };

    /// The same tokens with motion turned off.
    pub const fn reduced(self) -> Motion {
        Motion {
            enabled: false,
            ..self
        }
    }

    fn make(&self, duration: Duration, easing: Easing) -> Transition {
        if self.enabled {
            Transition { duration, easing }
        } else {
            Transition::NONE
        }
    }

    /// The transition for hover and press.
    pub fn fast(&self) -> Transition {
        self.make(self.fast, self.easing)
    }

    /// The transition for focus, selection and most changes.
    pub fn base(&self) -> Transition {
        self.make(self.base, self.easing)
    }

    /// The transition for changes that travel further.
    pub fn slow(&self) -> Transition {
        self.make(self.slow, self.easing)
    }

    /// The base duration, on the spring.
    pub fn springy(&self) -> Transition {
        self.make(self.base, self.spring)
    }
}

impl Default for Motion {
    fn default() -> Self {
        Motion::STANDARD
    }
}

/// The amounts the root animates for every widget: hover, press and the
/// visible focus ring, each from 0 to 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct NodeMotion {
    pub(crate) hover: Animated<f32>,
    pub(crate) press: Animated<f32>,
    pub(crate) focus: Animated<f32>,
}

impl NodeMotion {
    pub(crate) fn new() -> NodeMotion {
        NodeMotion {
            hover: Animated::new(0.0),
            press: Animated::new(0.0),
            focus: Animated::new(0.0),
        }
    }

    /// Moves towards the states the widget is in, and works out the
    /// amounts at `now`. Returns `(changed, running)`: whether any amount
    /// changed, and whether any is still moving.
    pub(crate) fn follow(
        &mut self,
        (hovered, pressed, focused): (bool, bool, bool),
        now: Duration,
        motion: &Motion,
    ) -> (bool, bool) {
        let level = |on: bool| if on { 1.0 } else { 0.0 };
        let mut changed = false;
        for (amount, on, transition) in [
            (&mut self.hover, hovered, motion.fast()),
            (&mut self.press, pressed, motion.fast()),
            (&mut self.focus, focused, motion.base()),
        ] {
            amount.transition(level(on), now, transition);
            changed |= amount.tick(now);
        }
        let running = self.hover.is_running() || self.press.is_running() || self.focus.is_running();
        (changed, running)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn the_standard_curve_starts_fast_and_lands_gently() {
        let e = Easing::STANDARD;
        assert_eq!(e.apply(0.0), 0.0);
        assert_eq!(e.apply(1.0), 1.0);
        assert_eq!(e.apply(-1.0), 0.0);
        assert_eq!(e.apply(f32::NAN), 0.0);
        // cubic-bezier(.2, .8, .2, 1) is past 80 % by a third of the time.
        assert!(e.apply(0.33) > 0.8, "{}", e.apply(0.33));
        let mut last = 0.0;
        for i in 1..=100 {
            let v = e.apply(i as f32 / 100.0);
            assert!(v >= last, "not monotonic at {i}");
            last = v;
        }
        // Linear Bézier control points give a straight line.
        let line = Easing::CubicBezier(1.0 / 3.0, 1.0 / 3.0, 2.0 / 3.0, 2.0 / 3.0);
        assert!((line.apply(0.3) - 0.3).abs() < 1e-5);
    }

    #[test]
    fn a_spring_overshoots_with_bounce_and_settles_by_the_end() {
        let stiff = Easing::Spring { bounce: 0.0 };
        let springy = Easing::Spring { bounce: 0.5 };
        let peak = |e: Easing| {
            (1..100)
                .map(|i| e.apply(i as f32 / 100.0))
                .fold(0.0f32, f32::max)
        };
        assert!(peak(stiff) <= 1.0);
        assert!(peak(springy) > 1.02);
        for e in [stiff, springy] {
            assert!((e.apply(0.99) - 1.0).abs() < 0.01);
            assert_eq!(e.apply(1.0), 1.0);
        }
    }

    #[test]
    fn a_new_target_starts_from_where_the_value_is() {
        let mut a = Animated::new(0.0f32);
        a.set(1.0, ms(0), ms(100), Easing::Linear);
        a.tick(ms(40));
        assert!((a.value() - 0.4).abs() < 1e-6);
        // Back half way.
        a.set(0.0, ms(40), ms(100), Easing::Linear);
        assert!((a.value() - 0.4).abs() < 1e-6);
        a.tick(ms(90));
        assert!((a.value() - 0.2).abs() < 1e-6);
        a.tick(ms(140));
        assert_eq!(a.value(), 0.0);
        assert!(!a.is_running());
        // The same target again changes nothing.
        assert!(!a.set(0.0, ms(150), ms(100), Easing::Linear));
        assert!(!a.is_running());
    }

    #[test]
    fn reduced_motion_jumps() {
        let motion = Motion::STANDARD.reduced();
        let mut a = Animated::new(0.0f32);
        a.transition(1.0, ms(0), motion.base());
        assert_eq!(a.value(), 1.0);
        assert!(!a.is_running());
        assert_eq!(motion.fast(), Transition::NONE);
    }

    #[test]
    fn colours_and_rectangles_blend() {
        let c = Color::rgba(0, 100, 200, 255).lerp(Color::rgba(100, 100, 0, 55), 0.5);
        assert_eq!(c, Color::rgba(50, 100, 100, 155));
        let r = Rect::new(0.0, 0.0, 10.0, 10.0).lerp(Rect::new(10.0, 20.0, 30.0, 10.0), 0.5);
        assert_eq!(r, Rect::new(5.0, 10.0, 20.0, 10.0));
    }

    #[test]
    fn node_motion_runs_only_while_a_state_changes() {
        let motion = Motion::STANDARD;
        let mut node = NodeMotion::new();
        assert_eq!(
            node.follow((false, false, false), ms(0), &motion),
            (false, false)
        );
        assert_eq!(
            node.follow((true, false, false), ms(0), &motion),
            (false, true)
        );
        assert_eq!(
            node.follow((true, false, false), ms(16), &motion),
            (true, true)
        );
        assert_eq!(
            node.follow((true, false, false), ms(120), &motion),
            (true, false)
        );
        assert_eq!(node.hover.value(), 1.0);
        assert_eq!(
            node.follow((true, false, false), ms(200), &motion),
            (false, false)
        );
    }
}
