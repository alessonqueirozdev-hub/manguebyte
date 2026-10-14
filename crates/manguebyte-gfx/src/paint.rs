//! What shapes are filled or stroked with, and how strokes are drawn.

use crate::color::Color;
use crate::geom::Point;

/// What lies beyond the ends of a gradient.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Extend {
    /// The end colours continue.
    #[default]
    Pad,
    /// The gradient starts again.
    Repeat,
    /// The gradient runs back and forth.
    Reflect,
}

/// A colour at a position along a gradient.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stop {
    /// Where, from 0 (the start) to 1 (the end). Values outside are
    /// clamped, and offsets that go backwards are raised to the one before.
    pub offset: f32,
    /// The colour there.
    pub color: Color,
}

impl Stop {
    /// A stop of `color` at `offset`.
    pub const fn new(offset: f32, color: Color) -> Self {
        Stop { offset, color }
    }
}

/// Colours that change along a line.
///
/// Colours are interpolated in sRGB space with straight alpha, as Skia and
/// `tiny-skia` do by default: each channel, alpha included, is mixed on its
/// own. CSS mixes premultiplied colours instead, and the two differ where a
/// stop is translucent: from transparent red to opaque blue, the middle
/// here is half-transparent purple, while CSS shows no red at all. To fade
/// a colour out, give the transparent stop the same colour at alpha 0.
///
/// When `start` and `end` are closer than 1/16384, the gradient has no
/// direction: with [`Extend::Pad`] it draws its last colour, and with
/// [`Extend::Repeat`] or [`Extend::Reflect`] the average of its colours
/// along its length. A gradient with a coordinate that is not finite draws
/// nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinearGradient<'a> {
    /// Where offset 0 lies.
    pub start: Point,
    /// Where offset 1 lies.
    pub end: Point,
    /// The colours; at least two for a gradient, one gives a solid colour,
    /// none draws nothing.
    pub stops: &'a [Stop],
    /// What lies beyond the ends.
    pub extend: Extend,
}

/// Colours that change with the distance from a centre.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RadialGradient<'a> {
    /// Where offset 0 lies.
    pub center: Point,
    /// The distance at which offset 1 lies. A radius that is zero,
    /// negative or not finite draws nothing; a positive radius of 1/16384
    /// or less draws one colour, as a [`LinearGradient`] whose ends
    /// coincide does.
    pub radius: f32,
    /// The colours, as for [`LinearGradient::stops`].
    pub stops: &'a [Stop],
    /// What lies beyond the circle.
    pub extend: Extend,
}

/// How the inside of a fill or a stroke is coloured.
///
/// Gradient coordinates are in the same space as the shape, so they move,
/// scale and rotate with it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Paint<'a> {
    /// One colour.
    Solid(Color),
    /// A linear gradient.
    Linear(LinearGradient<'a>),
    /// A radial gradient.
    Radial(RadialGradient<'a>),
}

impl From<Color> for Paint<'_> {
    fn from(color: Color) -> Self {
        Paint::Solid(color)
    }
}

impl<'a> From<LinearGradient<'a>> for Paint<'a> {
    fn from(gradient: LinearGradient<'a>) -> Self {
        Paint::Linear(gradient)
    }
}

impl<'a> From<RadialGradient<'a>> for Paint<'a> {
    fn from(gradient: RadialGradient<'a>) -> Self {
        Paint::Radial(gradient)
    }
}

/// How the open ends of a stroke are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Cap {
    /// Flat, at the end point.
    #[default]
    Butt,
    /// A half circle beyond the end point.
    Round,
    /// A half square beyond the end point.
    Square,
}

/// How a stroke turns a corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Join {
    /// A sharp point, cut to a bevel when it would reach farther than
    /// [`Stroke::miter_limit`] times half the width.
    #[default]
    Miter,
    /// A circular arc.
    Round,
    /// A straight cut.
    Bevel,
}

/// A dash pattern: `on` drawn, `off` skipped, repeated along the stroke.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dash {
    /// Length of each dash.
    pub on: f32,
    /// Length of each gap.
    pub off: f32,
    /// How far into the pattern the stroke starts.
    pub offset: f32,
}

impl Dash {
    /// Dashes of `on` separated by gaps of `off`, starting with a dash.
    pub const fn new(on: f32, off: f32) -> Self {
        Dash {
            on,
            off,
            offset: 0.0,
        }
    }
}

/// How an outline is drawn.
///
/// The width is in the coordinates of the shape, so a transform that scales
/// the shape scales the stroke too. A width that is zero, negative or not
/// finite draws nothing; so does a dash pattern whose lengths are negative
/// or add up to zero. A stroke is drawn as the filled outline of the
/// stroke, however thin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stroke {
    /// The width of the line.
    pub width: f32,
    /// How open ends are drawn.
    pub cap: Cap,
    /// How corners are drawn.
    pub join: Join,
    /// For [`Join::Miter`]: the longest point allowed, in half widths.
    pub miter_limit: f32,
    /// An optional dash pattern.
    pub dash: Option<Dash>,
}

impl Default for Stroke {
    fn default() -> Self {
        Stroke::new(1.0)
    }
}

impl Stroke {
    /// A solid line of `width` with butt caps and mitred corners (limit 4).
    pub const fn new(width: f32) -> Self {
        Stroke {
            width,
            cap: Cap::Butt,
            join: Join::Miter,
            miter_limit: 4.0,
            dash: None,
        }
    }

    /// A line of `width` with round caps and round corners, as knob arcs
    /// and curves usually want.
    pub const fn round(width: f32) -> Self {
        Stroke {
            width,
            cap: Cap::Round,
            join: Join::Round,
            miter_limit: 4.0,
            dash: None,
        }
    }

    /// The same stroke with other caps.
    pub const fn with_cap(self, cap: Cap) -> Self {
        Stroke { cap, ..self }
    }

    /// The same stroke with other corners.
    pub const fn with_join(self, join: Join) -> Self {
        Stroke { join, ..self }
    }

    /// The same stroke with another miter limit.
    pub const fn with_miter_limit(self, miter_limit: f32) -> Self {
        Stroke {
            miter_limit,
            ..self
        }
    }

    /// The same stroke, dashed.
    pub const fn with_dash(self, dash: Dash) -> Self {
        Stroke {
            dash: Some(dash),
            ..self
        }
    }

    /// How far, at most, a stroke covers beyond the outline it follows, in
    /// the shape's coordinates: half the width, times the miter limit for
    /// mitred corners and times the square root of two for square caps.
    ///
    /// The miter limit counts as it is drawn: one below 1 (which draws
    /// bevelled corners) counts as 1, and one that is not finite as 4.
    /// This is the exact geometry; [`Scene::bounds`](crate::Scene::bounds)
    /// adds the rasteriser's tolerance to it.
    pub fn reach(&self) -> f32 {
        let half = self.width * 0.5;
        let join = match self.join {
            Join::Miter => self.drawn_miter_limit(),
            Join::Round | Join::Bevel => 1.0,
        };
        let cap = match self.cap {
            Cap::Square => std::f32::consts::SQRT_2,
            Cap::Butt | Cap::Round => 1.0,
        };
        half * join.max(cap)
    }

    /// The miter limit the rasteriser uses: at least 1, and 4 in place of
    /// a value that is not finite.
    pub(crate) fn drawn_miter_limit(&self) -> f32 {
        if self.miter_limit.is_finite() {
            self.miter_limit.max(1.0)
        } else {
            4.0
        }
    }

    /// How far, at most, the outline the rasteriser draws for this stroke
    /// lies beyond the outline it follows, in the shape's coordinates: the
    /// [`reach`](Stroke::reach), plus [`STROKE_TOLERANCE`] for the
    /// approximation of round caps, round joins and curved outlines by
    /// quadratic curves, plus 1/1024 of the reach for very wide round
    /// strokes, whose approximation is coarser.
    pub(crate) fn drawn_reach(&self) -> f32 {
        let reach = self.reach();
        reach + STROKE_TOLERANCE + reach / 1024.0
    }
}

/// How far, in the shape's coordinates, the outline of a stroke as the
/// rasteriser draws it may stray outside the exact outline.
///
/// Round caps and joins are approximated by quadratic curves to within a
/// quarter of a unit, and the offset of a curved outline to within a
/// quarter of a unit over the resolution it is computed at; the renderer
/// computes it at four times the device scale and never below four device
/// pixels per unit, so that term is at most 1/16 of a unit. Half a unit
/// covers both.
pub(crate) const STROKE_TOLERANCE: f32 = 0.5;
