//! A level meter for a value another thread publishes.

use std::fmt::Write as _;

use manguebyte_gfx::{Point, Rect, RoundedRect, Scene, Size, TextStyle};

use super::{
    Cached, GAP, MARGIN, draw_text, line, line_height, name_style, text_width, value_style,
};
use crate::access::{AccessNode, AccessRange, Role};
use crate::source::Source;
use crate::widget::{AccessCx, MeasureCx, PaintCx, UpdateCx, Widget};

/// A vertical bar for a level, with its name above and its value as text
/// below.
///
/// The level comes from a [`Source`], usually an
/// [`AtomicSource`](crate::source::AtomicSource) that the audio thread
/// stores into once per block; the meter reads it once per frame. A value
/// outside the range is shown at the nearer end, and one that is not a
/// number at the bottom. With a [`release`](Meter::release) rate the bar
/// rises at once and falls no faster than that rate, landing exactly on
/// the level, so a meter whose level stays put stops drawing as soon as it
/// gets there.
///
/// The value is always shown as text, so the level is not told by colour
/// alone.
pub struct Meter {
    name: String,
    source: Box<dyn Source<f32>>,
    min: f32,
    max: f32,
    unit: String,
    precision: usize,
    warning: Option<f32>,
    release: f32,
    /// The level shown; `NaN` before the first frame.
    shown: f32,
    readout: String,
    scratch: String,
    /// Grows by one whenever the readout's text changes.
    readout_version: u64,
    name_layout: Cached<TextStyle>,
    value_layout: Cached<(u64, TextStyle)>,
}

/// A meter called `name` for the level `source` gives.
///
/// ```
/// use std::sync::Arc;
/// use std::sync::atomic::Ordering;
/// use manguebyte_core::AtomicF32;
/// use manguebyte_gfx::Size;
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{AtomicSource, Role, meter};
///
/// // The audio thread keeps the other end and stores once per block.
/// let level = Arc::new(AtomicF32::new(-60.0));
/// let view = meter("Output", AtomicSource::new(Arc::clone(&level)))
///     .range(-60.0, 0.0)
///     .unit("dB")
///     .precision(1)
///     .warning(-6.0);
/// let mut ui = Harness::new(view, Size::new(60.0, 140.0), 1.0);
/// ui.frame();
/// assert_eq!(ui.get(Role::Meter, "Output").value_text(), Some("-60.0 dB"));
///
/// level.store(-3.5, Ordering::Relaxed);
/// assert_eq!(ui.frame().painted, 1);
/// let output = ui.get(Role::Meter, "Output");
/// assert_eq!(output.value_text(), Some("-3.5 dB"));
/// assert_eq!(output.numeric(), Some(-3.5));
/// // Nothing new: nothing drawn.
/// assert_eq!(ui.frame().damaged, 0);
/// ```
pub fn meter(name: impl Into<String>, source: impl Source<f32>) -> Meter {
    Meter::new(name, source)
}

impl Meter {
    /// A meter called `name` for levels from 0 to 1, shown with two
    /// decimals, following the level at once.
    pub fn new(name: impl Into<String>, source: impl Source<f32>) -> Meter {
        Meter {
            name: name.into(),
            source: Box::new(source),
            min: 0.0,
            max: 1.0,
            unit: String::new(),
            precision: 2,
            warning: None,
            release: 0.0,
            shown: f32::NAN,
            readout: String::new(),
            scratch: String::new(),
            readout_version: 0,
            name_layout: Cached::new(),
            value_layout: Cached::new(),
        }
    }

    /// The same meter for levels from `min` (the bottom) to `max` (the
    /// top). If either end is not a finite number the range is left as
    /// it was; ends given the wrong way round are swapped.
    #[must_use]
    pub fn range(mut self, min: f32, max: f32) -> Meter {
        if min.is_finite() && max.is_finite() {
            self.min = min.min(max);
            self.max = max.max(min);
        }
        self
    }

    /// The same meter with `unit` after the value, such as `"dB"`.
    #[must_use]
    pub fn unit(mut self, unit: impl Into<String>) -> Meter {
        self.unit = unit.into();
        self
    }

    /// The same meter showing `decimals` decimal places (at most 6).
    #[must_use]
    pub fn precision(mut self, decimals: usize) -> Meter {
        self.precision = decimals.min(6);
        self
    }

    /// The same meter drawing the bar in the warning colour from `level`
    /// up.
    #[must_use]
    pub fn warning(mut self, level: f32) -> Meter {
        self.warning = (!level.is_nan()).then_some(level);
        self
    }

    /// The same meter falling at most `per_second` units of its range per
    /// second. 0, or anything that is not a positive finite number,
    /// follows the level at once.
    #[must_use]
    pub fn release(mut self, per_second: f32) -> Meter {
        self.release = if per_second.is_finite() && per_second > 0.0 {
            per_second
        } else {
            0.0
        };
        self
    }

    /// The level now shown.
    pub fn shown(&self) -> f32 {
        self.shown
    }

    /// `level` as the readout shows it, into `out`.
    fn format(&self, level: f32, out: &mut String) {
        out.clear();
        // No "-0.0".
        let level = if level == 0.0 { 0.0 } else { level };
        // Writing into a `String` cannot fail.
        let _ = write!(out, "{level:.precision$}", precision = self.precision);
        if !self.unit.is_empty() {
            out.push(' ');
            out.push_str(&self.unit);
        }
    }

    fn fraction(&self) -> f32 {
        let span = self.max - self.min;
        if self.shown.is_nan() || span <= 0.0 {
            return 0.0;
        }
        ((self.shown - self.min) / span).clamp(0.0, 1.0)
    }
}

impl Widget for Meter {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        let shown = if self.shown.is_nan() {
            self.min
        } else {
            self.shown
        };
        let mut value = String::new();
        self.format(shown, &mut value);
        AccessNode::new(Role::Meter, self.name.as_str())
            .with_value(value)
            .with_numeric(
                f64::from(shown),
                AccessRange::new(f64::from(self.min), f64::from(self.max), None),
            )
    }

    fn measure(&mut self, cx: &MeasureCx<'_>) -> Size {
        let theme = cx.theme();
        let name = name_style(theme);
        let value = value_style(theme);
        let mut text = String::new();
        let mut widest = text_width(&self.name, &name);
        for level in [self.min, self.max] {
            self.format(level, &mut text);
            widest = widest.max(text_width(&text, &value) + 4.0);
        }
        let sizes = theme.sizes;
        Size::new(
            widest.max(sizes.meter_width) + 2.0 * MARGIN,
            2.0 * MARGIN
                + line_height(&name)
                + GAP
                + sizes.meter_height
                + GAP
                + line_height(&value),
        )
    }

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        let raw = self.source.read();
        let target = if raw.is_nan() {
            self.min
        } else {
            raw.clamp(self.min, self.max)
        };
        let previous = self.shown;
        let next = if previous.is_nan() || self.release == 0.0 || target >= previous {
            target
        } else {
            let fall = self.release * cx.elapsed().as_secs_f32();
            (previous - fall).max(target)
        };
        if next.to_bits() != previous.to_bits() {
            self.shown = next;
            let mut scratch = std::mem::take(&mut self.scratch);
            self.format(next, &mut scratch);
            if scratch != self.readout {
                std::mem::swap(&mut scratch, &mut self.readout);
                self.readout_version = self.readout_version.wrapping_add(1);
            }
            self.scratch = scratch;
            cx.request_paint();
        }
        if next > target {
            cx.request_frame();
        }
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let theme = cx.theme();
        let rect = cx.rect();
        let width = (rect.width - 2.0 * MARGIN).max(0.0);
        let name_style = name_style(theme);
        let value_style = value_style(theme);
        let name_height = line_height(&name_style);
        let value_height = line_height(&value_style);
        let top = rect.y + MARGIN + name_height + GAP;
        let bottom = rect.bottom() - MARGIN - value_height - GAP;
        let height = (bottom - top).max(0.0);
        let bar = theme.sizes.meter_width.min(width).max(0.0);
        let well = Rect::new(rect.x + (rect.width - bar) * 0.5, top, bar, height);
        scene.fill(
            RoundedRect::new(well, theme.corners.sm),
            theme.palette.track,
        );
        let level = (height - 2.0).max(0.0) * self.fraction();
        if level > 0.0 {
            let warning = self.warning.is_some_and(|at| self.shown >= at);
            let color = if warning {
                theme.palette.danger
            } else {
                theme.palette.success
            };
            scene.fill(
                Rect::new(
                    well.x + 1.0,
                    well.bottom() - 1.0 - level,
                    (bar - 2.0).max(0.0),
                    level,
                ),
                color,
            );
        }
        let label = &self.name;
        let name = self
            .name_layout
            .get(width, name_style, || line(label, &name_style, width));
        draw_text(scene, name, Point::new(rect.x + MARGIN, rect.y + MARGIN));
        let readout = &self.readout;
        let key = (self.readout_version, value_style);
        let value = self
            .value_layout
            .get(width, key, || line(readout, &value_style, width));
        let y = rect.bottom() - MARGIN - value_height;
        draw_text(scene, value, Point::new(rect.x + MARGIN, y));
    }
}
