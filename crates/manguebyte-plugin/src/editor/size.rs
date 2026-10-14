//! Sizes in the host's unit, and the rules for resizing an editor.

/// A width and a height in the unit the plugin format uses on this platform.
///
/// That unit is device pixels for a [`Win32`](super::ParentWindow::Win32) or
/// [`X11`](super::ParentWindow::X11) parent and points for a
/// [`Cocoa`](super::ParentWindow::Cocoa) one, which is what both CLAP and
/// VST 3 use. On Windows and X11 a size therefore grows with the scale; on
/// macOS the system scales points itself and the scale passed around is 1.
///
/// ```
/// use manguebyte_plugin::editor::HostSize;
///
/// // A 300 by 170 editor on a screen at 150 %.
/// assert_eq!(HostSize::new(300, 170).scale_by(1.5), HostSize::new(450, 255));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct HostSize {
    /// The width.
    pub width: u32,
    /// The height.
    pub height: u32,
}

impl HostSize {
    /// A size of `width` by `height`.
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// This size multiplied by `scale`, each side rounded to the nearest
    /// whole unit.
    ///
    /// A side that is not zero stays at least 1, however small the scale,
    /// and a side too large for `u32` becomes `u32::MAX`. A scale that is
    /// not finite and positive leaves the size as it is. Editors are only
    /// ever given scales within [`MIN_SCALE`](super::MIN_SCALE)`..=`
    /// [`MAX_SCALE`](super::MAX_SCALE).
    ///
    /// ```
    /// use manguebyte_plugin::editor::{HostSize, MIN_SCALE};
    ///
    /// assert_eq!(HostSize::new(3, 5).scale_by(0.5), HostSize::new(2, 3));
    /// assert_eq!(HostSize::new(1, 1).scale_by(MIN_SCALE), HostSize::new(1, 1));
    /// assert_eq!(HostSize::new(0, 7).scale_by(2.0), HostSize::new(0, 14));
    /// ```
    #[must_use]
    pub fn scale_by(self, scale: f64) -> Self {
        if !(scale.is_finite() && scale > 0.0) {
            return self;
        }
        let side = |length: u32| {
            if length == 0 {
                return 0;
            }
            // `as` saturates at the bounds of `u32`, which is what is
            // wanted at the top.
            ((f64::from(length) * scale).round() as u32).max(1)
        };
        Self::new(side(self.width), side(self.height))
    }
}

/// How an editor may be resized.
///
/// Limits are given at scale 1, in the unit of
/// [`EditorSpec::size(1.0)`](super::EditorSpec::size), and turned into the
/// host's unit with [`scaled`](Self::scaled). [`fit`](Self::fit) then gives
/// the size the editor accepts that is nearest to what a host asks for. It
/// is a pure function, so a wrapper can answer CLAP's `adjust_size` and
/// VST 3's `checkSizeConstraint` with it before an editor is even open.
///
/// ```
/// use manguebyte_plugin::editor::{HostSize, Resize};
///
/// let resize = Resize::Ratio {
///     min: HostSize::new(320, 180),
///     max: HostSize::new(1920, 1080),
///     width: 16,
///     height: 9,
/// };
/// let current = HostSize::new(640, 360);
///
/// // The largest 16:9 size inside what the host asked for.
/// assert_eq!(resize.fit(current, HostSize::new(1000, 1000)), HostSize::new(1000, 563));
/// // Never smaller than the minimum, never larger than the maximum.
/// assert_eq!(resize.fit(current, HostSize::new(10, 10)), HostSize::new(320, 180));
/// assert_eq!(resize.fit(current, HostSize::new(9999, 9999)), HostSize::new(1920, 1080));
/// // At 200 % the limits double.
/// assert_eq!(
///     resize.scaled(2.0).fit(current, HostSize::new(10, 10)),
///     HostSize::new(640, 360),
/// );
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Resize {
    /// The editor keeps the size it has.
    #[default]
    Fixed,
    /// Width and height change independently within the limits.
    Free {
        /// The smallest size.
        min: HostSize,
        /// The largest size.
        max: HostSize,
    },
    /// The size keeps the shape `width` by `height` within the limits.
    Ratio {
        /// The smallest size.
        min: HostSize,
        /// The largest size.
        max: HostSize,
        /// The width of the shape, such as 16 for 16:9.
        width: u32,
        /// The height of the shape, such as 9 for 16:9.
        height: u32,
    },
}

/// What a host is told about how an editor resizes, in the form CLAP's
/// `get_resize_hints` wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResizeHints {
    /// The width can change.
    pub horizontal: bool,
    /// The height can change.
    pub vertical: bool,
    /// The shape width by height is kept, if there is one.
    pub ratio: Option<(u32, u32)>,
}

impl Resize {
    /// These rules with their limits multiplied by `scale` (see
    /// [`HostSize::scale_by`]). The shape of a [`Ratio`](Self::Ratio) does
    /// not change.
    #[must_use]
    pub fn scaled(self, scale: f64) -> Self {
        match self {
            Self::Fixed => Self::Fixed,
            Self::Free { min, max } => Self::Free {
                min: min.scale_by(scale),
                max: max.scale_by(scale),
            },
            Self::Ratio {
                min,
                max,
                width,
                height,
            } => Self::Ratio {
                min: min.scale_by(scale),
                max: max.scale_by(scale),
                width,
                height,
            },
        }
    }

    /// The size nearest to `wanted` that these rules accept, for an editor
    /// whose size is now `current`.
    ///
    /// - [`Fixed`](Self::Fixed): `current`.
    /// - [`Free`](Self::Free): each side of `wanted` kept within its limits.
    /// - [`Ratio`](Self::Ratio): the largest size of the shape that fits
    ///   inside `wanted`, kept within the limits. The height is the width
    ///   times the shape's height over its width, rounded to the nearest
    ///   unit (halves up).
    ///
    /// Limits are cleaned up first, so that any input gives an answer: a
    /// side is never smaller than 1, and a minimum larger than its maximum
    /// wins. A ratio with a zero side, or one that no size within the
    /// limits can have, is treated as [`Free`](Self::Free).
    ///
    /// Applying `fit` to its own result gives the same result.
    pub fn fit(&self, current: HostSize, wanted: HostSize) -> HostSize {
        match *self {
            Self::Fixed => current,
            Self::Free { min, max } => Limits::new(min, max).clamp(wanted),
            Self::Ratio {
                min,
                max,
                width,
                height,
            } => {
                let limits = Limits::new(min, max);
                match Shape::new(width, height, limits) {
                    Some(shape) => shape.fit(wanted),
                    None => limits.clamp(wanted),
                }
            }
        }
    }

    /// Whether the size can change at all.
    pub fn can_resize(&self) -> bool {
        let hints = self.hints();
        hints.horizontal || hints.vertical
    }

    /// What to tell a host about these rules.
    pub fn hints(&self) -> ResizeHints {
        match *self {
            Self::Fixed => ResizeHints {
                horizontal: false,
                vertical: false,
                ratio: None,
            },
            Self::Free { min, max } => Limits::new(min, max).hints(),
            Self::Ratio {
                min,
                max,
                width,
                height,
            } => {
                let limits = Limits::new(min, max);
                match Shape::new(width, height, limits) {
                    Some(shape) => ResizeHints {
                        horizontal: shape.low < shape.high,
                        vertical: shape.height_at(shape.low) < shape.height_at(shape.high),
                        ratio: Some((width, height)),
                    },
                    None => limits.hints(),
                }
            }
        }
    }
}

/// Limits cleaned up so that every side is at least 1 and no minimum
/// exceeds its maximum.
#[derive(Debug, Clone, Copy)]
struct Limits {
    min: HostSize,
    max: HostSize,
}

impl Limits {
    fn new(min: HostSize, max: HostSize) -> Self {
        let min = HostSize::new(min.width.max(1), min.height.max(1));
        let max = HostSize::new(max.width.max(min.width), max.height.max(min.height));
        Self { min, max }
    }

    fn clamp(self, wanted: HostSize) -> HostSize {
        HostSize::new(
            wanted.width.max(self.min.width).min(self.max.width),
            wanted.height.max(self.min.height).min(self.max.height),
        )
    }

    fn hints(self) -> ResizeHints {
        ResizeHints {
            horizontal: self.min.width < self.max.width,
            vertical: self.min.height < self.max.height,
            ratio: None,
        }
    }
}

/// A ratio that some size within the limits can have, with the range of
/// widths that give such sizes.
///
/// Sizes are worked out from the width: the height at width `w` is
/// `floor(w * height / width + 1/2)`, which never decreases as `w` grows.
/// Everything is computed in `u128`, where no product of two `u32` values
/// can overflow.
#[derive(Debug, Clone, Copy)]
struct Shape {
    width: u128,
    height: u128,
    /// The smallest width whose size is within the limits.
    low: u128,
    /// The largest width whose size is within the limits.
    high: u128,
}

impl Shape {
    fn new(width: u32, height: u32, limits: Limits) -> Option<Self> {
        if width == 0 || height == 0 {
            return None;
        }
        let mut shape = Self {
            width: u128::from(width),
            height: u128::from(height),
            low: 0,
            high: 0,
        };
        shape.low =
            u128::from(limits.min.width).max(shape.smallest_width_reaching(limits.min.height));
        shape.high =
            u128::from(limits.max.width).min(shape.largest_width_within(limits.max.height));
        (shape.low <= shape.high).then_some(shape)
    }

    fn height_at(&self, width: u128) -> u128 {
        (2 * width * self.height + self.width) / (2 * self.width)
    }

    /// The largest width whose height is at most `limit`:
    /// `height_at(w) <= limit` exactly when `2 w height < (2 limit + 1) width`.
    fn largest_width_within(&self, limit: u32) -> u128 {
        ((2 * u128::from(limit) + 1) * self.width - 1) / (2 * self.height)
    }

    /// The smallest width whose height is at least `limit`, which is 1 or
    /// more: `height_at(w) >= limit` exactly when
    /// `2 w height >= (2 limit - 1) width`.
    fn smallest_width_reaching(&self, limit: u32) -> u128 {
        ((2 * u128::from(limit) - 1) * self.width).div_ceil(2 * self.height)
    }

    fn fit(&self, wanted: HostSize) -> HostSize {
        let inside = u128::from(wanted.width).min(self.largest_width_within(wanted.height));
        let width = inside.max(self.low).min(self.high);
        // Both fit in `u32`: the width is at most the maximum width, and the
        // height at most the maximum height, because `high` was chosen so.
        let height = self.height_at(width);
        HostSize::new(
            u32::try_from(width).unwrap_or(u32::MAX),
            u32::try_from(height).unwrap_or(u32::MAX),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(width: u32, height: u32) -> HostSize {
        HostSize::new(width, height)
    }

    #[test]
    fn scaling_rounds_to_the_nearest_unit_and_saturates() {
        assert_eq!(size(300, 170).scale_by(1.25), size(375, 213)); // 212.5 rounds up
        assert_eq!(size(3, 5).scale_by(0.5), size(2, 3));
        assert_eq!(size(u32::MAX, 1).scale_by(2.0), size(u32::MAX, 2));
        for hostile in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(size(300, 170).scale_by(hostile), size(300, 170));
        }
    }

    /// Scales small enough to round a side down to nothing keep it at one
    /// unit instead; only a side that was zero stays zero.
    #[test]
    fn scaling_never_turns_a_side_into_zero() {
        assert_eq!(size(300, 170).scale_by(f64::MIN_POSITIVE), size(1, 1));
        assert_eq!(size(1, 1).scale_by(crate::editor::MIN_SCALE), size(1, 1));
        assert_eq!(size(3, 1).scale_by(0.1), size(1, 1));
        assert_eq!(size(0, 0).scale_by(f64::MIN_POSITIVE), size(0, 0));
        let mut random = Random(0xA5A5_5A5A_DEAD_BEEF);
        for _ in 0..if cfg!(miri) { 100 } else { 10_000 } {
            let original = size(random.next() as u32, (random.next() % 4) as u32);
            // Any non-negative bit pattern: tiny, huge, infinite or NaN.
            let scale = f64::from_bits(random.next() >> 1);
            let scaled = original.scale_by(scale);
            assert_eq!(
                scaled.width == 0,
                original.width == 0,
                "{original:?} by {scale}"
            );
            assert_eq!(
                scaled.height == 0,
                original.height == 0,
                "{original:?} by {scale}"
            );
        }
    }

    #[test]
    fn a_fixed_editor_keeps_its_size() {
        let fixed = Resize::Fixed;
        assert_eq!(fixed.fit(size(300, 170), size(1, 99_999)), size(300, 170));
        assert!(!fixed.can_resize());
        assert_eq!(fixed.scaled(2.0), Resize::Fixed);
    }

    #[test]
    fn a_free_editor_clamps_each_side() {
        let free = Resize::Free {
            min: size(200, 100),
            max: size(800, 600),
        };
        let current = size(300, 170);
        assert_eq!(free.fit(current, size(500, 400)), size(500, 400));
        assert_eq!(free.fit(current, size(10, 9_000)), size(200, 600));
        assert_eq!(free.fit(current, size(0, 0)), size(200, 100));
        assert_eq!(
            free.hints(),
            ResizeHints {
                horizontal: true,
                vertical: true,
                ratio: None
            }
        );
    }

    #[test]
    fn untidy_limits_still_give_an_answer() {
        // A minimum above its maximum wins; zero becomes one.
        let crossed = Resize::Free {
            min: size(500, 0),
            max: size(100, 0),
        };
        assert_eq!(crossed.fit(size(1, 1), size(300, 300)), size(500, 1));
        assert!(!crossed.can_resize());

        // A shape with a zero side is ignored.
        let flat = Resize::Ratio {
            min: size(10, 10),
            max: size(20, 20),
            width: 0,
            height: 9,
        };
        assert_eq!(flat.fit(size(1, 1), size(15, 99)), size(15, 20));
        assert_eq!(flat.hints().ratio, None);

        // No 1:10 size lies within 100..=200 by 100..=200.
        let impossible = Resize::Ratio {
            min: size(100, 100),
            max: size(200, 200),
            width: 1,
            height: 10,
        };
        assert_eq!(impossible.fit(size(1, 1), size(150, 999)), size(150, 200));
    }

    #[test]
    fn a_ratio_editor_keeps_its_shape() {
        let wide = Resize::Ratio {
            min: size(160, 90),
            max: size(1600, 900),
            width: 16,
            height: 9,
        };
        let current = size(320, 180);
        // Limited by the width, then by the height.
        assert_eq!(wide.fit(current, size(320, 999)), size(320, 180));
        assert_eq!(wide.fit(current, size(999, 180)), size(320, 180));
        // 321 * 9 / 16 = 180.56, which rounds up to 181: too tall for 180.
        assert_eq!(wide.fit(current, size(321, 180)), size(320, 180));
        assert_eq!(wide.fit(current, size(321, 181)), size(321, 181));
        assert_eq!(
            wide.hints(),
            ResizeHints {
                horizontal: true,
                vertical: true,
                ratio: Some((16, 9))
            }
        );
    }

    /// Every input, including the extremes of `u32`, gives a size within
    /// the cleaned-up limits, and the answer is stable.
    #[test]
    fn hostile_sizes_and_limits_never_escape_the_limits() {
        let mut random = Random(0x9E37_79B9_7F4A_7C15);
        let pick = |random: &mut Random| match random.next() % 6 {
            0 => 0,
            1 => u32::MAX,
            2 => u32::MAX - (random.next() % 3) as u32,
            3 => (random.next() % 8) as u32,
            _ => (random.next() % 5_000) as u32,
        };
        let rounds = if cfg!(miri) { 200 } else { 20_000 };
        for _ in 0..rounds {
            let min = size(pick(&mut random), pick(&mut random));
            let max = size(pick(&mut random), pick(&mut random));
            let current = size(pick(&mut random), pick(&mut random));
            let wanted = size(pick(&mut random), pick(&mut random));
            let (width, height) = (pick(&mut random), pick(&mut random));
            let limits = Limits::new(min, max);
            let shape = Shape::new(width, height, limits);
            for resize in [
                Resize::Free { min, max },
                Resize::Ratio {
                    min,
                    max,
                    width,
                    height,
                },
            ] {
                let got = resize.fit(current, wanted);
                assert!(
                    (limits.min.width..=limits.max.width).contains(&got.width)
                        && (limits.min.height..=limits.max.height).contains(&got.height),
                    "{resize:?} fit {wanted:?} to {got:?}"
                );
                assert_eq!(resize.fit(current, got), got, "{resize:?} is not stable");
                if let (Resize::Ratio { .. }, Some(shape)) = (resize, shape) {
                    assert_eq!(u128::from(got.height), shape.height_at(got.width.into()));
                }
            }
        }
    }

    /// The closed forms agree with a search over every width.
    #[test]
    fn ratio_fitting_matches_a_brute_force_search() {
        let mut random = Random(42);
        let rounds = if cfg!(miri) { 40 } else { 400 };
        for _ in 0..rounds {
            let width = 1 + (random.next() % 21) as u32;
            let height = 1 + (random.next() % 21) as u32;
            let min = size((random.next() % 40) as u32, (random.next() % 40) as u32);
            let max = size(
                min.width + (random.next() % 60) as u32,
                min.height + (random.next() % 60) as u32,
            );
            let wanted = size((random.next() % 120) as u32, (random.next() % 120) as u32);
            let limits = Limits::new(min, max);
            let shape_height = |w: u32| (2 * w * height + width) / (2 * width);
            let allowed: Vec<u32> = (0..=limits.max.width)
                .filter(|&w| {
                    let h = shape_height(w);
                    w >= limits.min.width && h >= limits.min.height && h <= limits.max.height
                })
                .collect();
            let resize = Resize::Ratio {
                min,
                max,
                width,
                height,
            };
            let got = resize.fit(size(1, 1), wanted);
            let Some(&low) = allowed.first() else {
                assert_eq!(got, limits.clamp(wanted), "no shaped size: behaves as free");
                continue;
            };
            // The widest allowed size inside `wanted`, or the smallest
            // allowed size when none is inside. (Allowed widths form one
            // unbroken range, because the height never decreases.)
            let expected = allowed
                .iter()
                .copied()
                .filter(|&w| w <= wanted.width && shape_height(w) <= wanted.height)
                .max()
                .unwrap_or(low);
            assert_eq!(
                got,
                size(expected, shape_height(expected)),
                "{resize:?} {wanted:?}"
            );
        }
    }

    /// A small deterministic generator (xorshift64*), so that failures
    /// repeat.
    struct Random(u64);

    impl Random {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }
}
