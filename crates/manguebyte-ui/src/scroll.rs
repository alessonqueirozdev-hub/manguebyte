//! Scrolling: where a scroll view is, and the scroll bars that the scroll
//! view and the list share.
//!
//! A node laid out as [`Arrange::Scroll`](crate::layout::Arrange::Scroll)
//! has [`ScrollMetrics`], kept by the [`Root`](crate::Root): its offset,
//! its viewport and the size of what scrolls. The root moves the node's
//! children by the offset and clips them to the viewport; the widget of
//! the node reads the metrics from its context (`cx.scroll()`) and moves
//! with [`EventCx::scroll_to`](crate::EventCx::scroll_to). Offsets are
//! always whole device pixels, so that what scrolls stays as sharp as
//! what does not.

use std::time::Duration;

use manguebyte_gfx::{Point, Rect, RoundedRect, Scene, Size};

use crate::event::{Button, Event, Handled, Key, KeyEvent};
use crate::theme::Theme;

/// Where a scroll node is scrolled to, and how far it can go.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ScrollMetrics {
    /// How far the content is scrolled from its top-left corner, in
    /// logical pixels: never below 0 nor past
    /// [`max_offset`](ScrollMetrics::max_offset).
    pub offset: Point,
    /// The part of the node its content shows through, in the interface's
    /// logical pixels.
    pub viewport: Rect,
    /// The size of the content, in logical pixels.
    pub content: Size,
    /// It scrolls sideways.
    pub x: bool,
    /// It scrolls up and down.
    pub y: bool,
}

impl ScrollMetrics {
    /// The largest offset on each axis: how far the content reaches past
    /// the viewport, or 0 on an axis it does not scroll on.
    pub fn max_offset(&self) -> Point {
        let reach = |on: bool, content: f32, view: f32| {
            if on { (content - view).max(0.0) } else { 0.0 }
        };
        Point::new(
            reach(self.x, self.content.width, self.viewport.width),
            reach(self.y, self.content.height, self.viewport.height),
        )
    }

    /// `offset` kept between 0 and [`max_offset`](Self::max_offset); a
    /// coordinate that is not a number counts as 0.
    pub fn clamp(&self, offset: Point) -> Point {
        let max = self.max_offset();
        let keep = |v: f32, max: f32| if v.is_nan() { 0.0 } else { v.clamp(0.0, max) };
        Point::new(keep(offset.x, max.x), keep(offset.y, max.y))
    }

    /// Whether the content is wider than the viewport, on an axis it
    /// scrolls on.
    pub fn overflows_x(&self) -> bool {
        self.max_offset().x > 0.0
    }

    /// Whether the content is taller than the viewport, on an axis it
    /// scrolls on.
    pub fn overflows_y(&self) -> bool {
        self.max_offset().y > 0.0
    }
}

/// `value` rounded to the nearest whole device pixel at `scale`.
pub(crate) fn snap(value: f64, scale: f32) -> f64 {
    let scale = f64::from(scale);
    if scale > 0.0 && scale.is_finite() {
        (value * scale).round() / scale
    } else {
        value.round()
    }
}

/// The shortest a scroll bar's thumb is, in logical pixels.
const MIN_THUMB: f64 = 16.0;

/// How close the pointer comes to a viewport's edge, in logical pixels,
/// before a drag scrolls it.
pub(crate) const AUTO_EDGE: f32 = 24.0;

/// How fast a drag scrolls a view with the pointer on its very edge, in
/// logical pixels a second; it slows to nothing at [`AUTO_EDGE`] from it.
pub(crate) const AUTO_SPEED: f32 = 1200.0;

/// The longest step of time one auto-scroll takes, so that a late frame
/// does not jump.
pub(crate) const AUTO_STEP: Duration = Duration::from_millis(50);

/// How far a drag at `at` scrolls `viewport` in `elapsed`, along each
/// axis: towards the edge it is near, faster the nearer.
pub(crate) fn auto_scroll(viewport: Rect, at: Point, elapsed: Duration) -> Point {
    let seconds = elapsed.min(AUTO_STEP).as_secs_f32();
    let edge = AUTO_EDGE
        .min(viewport.width * 0.25)
        .min(viewport.height * 0.25);
    if edge.is_nan() || edge <= 0.0 || !viewport.contains(at) {
        return Point::ZERO;
    }
    let speed = |near_start: f32, near_end: f32| {
        if near_start < edge {
            -AUTO_SPEED * (edge - near_start) / edge
        } else if near_end < edge {
            AUTO_SPEED * (edge - near_end) / edge
        } else {
            0.0
        }
    };
    Point::new(
        speed(at.x - viewport.x, viewport.right() - at.x) * seconds,
        speed(at.y - viewport.y, viewport.bottom() - at.y) * seconds,
    )
}

/// Everything a scroll bar needs to know, in `f64` so that a list of a
/// hundred thousand rows keeps whole pixels: the rectangle the bars sit
/// at the edges of, the viewport, the offset and the content's size.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Geometry {
    pub(crate) viewport: Rect,
    pub(crate) offset: (f64, f64),
    pub(crate) content: (f64, f64),
    pub(crate) x: bool,
    pub(crate) y: bool,
    /// The bars' thickness.
    pub(crate) bar: f32,
}

impl Geometry {
    pub(crate) fn from_metrics(metrics: &ScrollMetrics, bar: f32) -> Geometry {
        Geometry {
            viewport: metrics.viewport,
            offset: (f64::from(metrics.offset.x), f64::from(metrics.offset.y)),
            content: (
                f64::from(metrics.content.width),
                f64::from(metrics.content.height),
            ),
            x: metrics.x,
            y: metrics.y,
            bar,
        }
    }

    pub(crate) fn max(&self) -> (f64, f64) {
        let reach = |on: bool, content: f64, view: f32| {
            if on {
                (content - f64::from(view)).max(0.0)
            } else {
                0.0
            }
        };
        (
            reach(self.x, self.content.0, self.viewport.width),
            reach(self.y, self.content.1, self.viewport.height),
        )
    }

    pub(crate) fn clamp(&self, offset: (f64, f64)) -> (f64, f64) {
        let (max_x, max_y) = self.max();
        let keep = |v: f64, max: f64| if v.is_nan() { 0.0 } else { v.clamp(0.0, max) };
        (keep(offset.0, max_x), keep(offset.1, max_y))
    }

    /// The track of the vertical bar, at the right of the viewport.
    pub(crate) fn track_y(&self) -> Option<Rect> {
        self.y.then(|| {
            Rect::new(
                self.viewport.right(),
                self.viewport.y,
                self.bar,
                self.viewport.height,
            )
        })
    }

    /// The track of the horizontal bar, below the viewport.
    pub(crate) fn track_x(&self) -> Option<Rect> {
        self.x.then(|| {
            Rect::new(
                self.viewport.x,
                self.viewport.bottom(),
                self.viewport.width,
                self.bar,
            )
        })
    }

    /// The thumb on the vertical bar, if the content overflows.
    pub(crate) fn thumb_y(&self) -> Option<Rect> {
        let track = self.track_y()?;
        let (start, length) = thumb(
            f64::from(track.height),
            f64::from(self.viewport.height),
            self.content.1,
            self.offset.1,
            self.max().1,
        )?;
        Some(Rect::new(
            track.x,
            track.y + start as f32,
            track.width,
            length as f32,
        ))
    }

    /// The thumb on the horizontal bar, if the content overflows.
    pub(crate) fn thumb_x(&self) -> Option<Rect> {
        let track = self.track_x()?;
        let (start, length) = thumb(
            f64::from(track.width),
            f64::from(self.viewport.width),
            self.content.0,
            self.offset.0,
            self.max().0,
        )?;
        Some(Rect::new(
            track.x + start as f32,
            track.y,
            length as f32,
            track.height,
        ))
    }
}

/// Where a thumb starts along a track `track` long and how long it is,
/// for a view `view` long over content `content` long scrolled to
/// `offset` of `max`; `None` when nothing overflows.
fn thumb(track: f64, view: f64, content: f64, offset: f64, max: f64) -> Option<(f64, f64)> {
    if !(max > 0.0 && content > 0.0 && track > 0.0) {
        return None;
    }
    let length = (track * view / content).clamp(MIN_THUMB.min(track), track);
    let start = (track - length) * (offset / max).clamp(0.0, 1.0);
    Some((start, length))
}

/// A thumb being dragged.
#[derive(Debug, Clone, Copy)]
struct ThumbDrag {
    vertical: bool,
    /// Where the pointer went down, along the bar.
    from: f32,
    /// The offset then.
    start: f64,
}

/// What a scroll bar did with an event.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Outcome {
    pub(crate) handled: Handled,
    /// Where the view should be scrolled to, already within its range and
    /// on whole device pixels.
    pub(crate) scroll_to: Option<(f64, f64)>,
}

impl Outcome {
    const NO: Outcome = Outcome {
        handled: Handled::No,
        scroll_to: None,
    };

    fn yes(scroll_to: Option<(f64, f64)>) -> Outcome {
        Outcome {
            handled: Handled::Yes,
            scroll_to,
        }
    }
}

/// The state of a view's scroll bars: a thumb being dragged, and what the
/// wheel moved by less than a device pixel.
#[derive(Debug, Default)]
pub(crate) struct Bars {
    drag: Option<ThumbDrag>,
    rest: (f64, f64),
}

impl Bars {
    /// Whether a thumb is being dragged.
    pub(crate) fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// Handles the wheel, presses and drags on the bars, and, if
    /// `focused`, the keys that scroll. `line` is how far a notch or an
    /// arrow goes. Returns whether the event was used and where to scroll
    /// to.
    ///
    /// The wheel is used only if the view can move the way it turns, so
    /// that a view at its end leaves the wheel to the view around it.
    pub(crate) fn event(
        &mut self,
        geometry: &Geometry,
        event: &Event,
        focused: bool,
        line: f32,
        scale: f32,
    ) -> Outcome {
        let line = f64::from(line);
        match *event {
            Event::Wheel {
                delta, modifiers, ..
            } => {
                let (mut dx, mut dy) = (f64::from(delta.x), f64::from(delta.y));
                if modifiers.shift && dx == 0.0 {
                    // Shift turns the wheel sideways, as on most systems:
                    // towards the person goes right.
                    (dx, dy) = (-dy, 0.0);
                }
                if !(dx.is_finite() && dy.is_finite()) {
                    return Outcome::NO;
                }
                // Up the wheel (away from the person) goes up the content;
                // right goes right.
                let by = (dx * line, -dy * line);
                let (max_x, max_y) = geometry.max();
                let (x, y) = geometry.offset;
                let moves =
                    |by: f64, at: f64, max: f64| (by > 0.0 && at < max) || (by < 0.0 && at > 0.0);
                if !(moves(by.0, x, max_x) || moves(by.1, y, max_y)) {
                    self.rest = (0.0, 0.0);
                    return Outcome::NO;
                }
                let exact = geometry.clamp((x + by.0 + self.rest.0, y + by.1 + self.rest.1));
                let snapped = (snap(exact.0, scale), snap(exact.1, scale));
                self.rest = (exact.0 - snapped.0, exact.1 - snapped.1);
                Outcome::yes(Some(geometry.clamp(snapped)))
            }
            Event::PointerDown {
                at,
                button: Button::Primary,
                ..
            } => self.press(geometry, at, scale),
            Event::PointerMove { at, .. } => match self.drag {
                Some(drag) => Outcome::yes(Some(dragged(geometry, drag, at, scale))),
                None => Outcome::NO,
            },
            Event::PointerUp { .. } | Event::PointerCancel => {
                if self.drag.take().is_some() {
                    Outcome::yes(None)
                } else {
                    Outcome::NO
                }
            }
            Event::Key(KeyEvent {
                key,
                pressed: true,
                modifiers,
            }) if focused && !modifiers.control && !modifiers.alt && !modifiers.meta => {
                keys(geometry, key, line, scale)
            }
            _ => Outcome::NO,
        }
    }

    /// A press: on a thumb, it starts dragging it; on a track, it moves a
    /// page towards the press.
    fn press(&mut self, geometry: &Geometry, at: Point, scale: f32) -> Outcome {
        let (x, y) = geometry.offset;
        for vertical in [true, false] {
            let (track, thumb) = if vertical {
                (geometry.track_y(), geometry.thumb_y())
            } else {
                (geometry.track_x(), geometry.thumb_x())
            };
            if !track.is_some_and(|track| track.contains(at)) {
                continue;
            }
            let Some(thumb) = thumb else {
                // Nothing to scroll: the press lands on an empty track.
                return Outcome::yes(None);
            };
            let along = if vertical { at.y } else { at.x };
            if thumb.contains(at) {
                self.drag = Some(ThumbDrag {
                    vertical,
                    from: along,
                    start: if vertical { y } else { x },
                });
                return Outcome::yes(None);
            }
            let before = if vertical {
                at.y < thumb.y
            } else {
                at.x < thumb.x
            };
            let page = f64::from(if vertical {
                geometry.viewport.height
            } else {
                geometry.viewport.width
            });
            let by = if before { -page } else { page };
            let target = if vertical { (x, y + by) } else { (x + by, y) };
            let target = geometry.clamp(target);
            return Outcome::yes(Some(
                geometry.clamp((snap(target.0, scale), snap(target.1, scale))),
            ));
        }
        Outcome::NO
    }
}

/// The offset a thumb drag gives with the pointer at `at`.
fn dragged(geometry: &Geometry, drag: ThumbDrag, at: Point, scale: f32) -> (f64, f64) {
    let (x, y) = geometry.offset;
    let (track, thumb, max) = if drag.vertical {
        (geometry.track_y(), geometry.thumb_y(), geometry.max().1)
    } else {
        (geometry.track_x(), geometry.thumb_x(), geometry.max().0)
    };
    let (Some(track), Some(thumb)) = (track, thumb) else {
        return geometry.offset;
    };
    let (track, thumb, along) = if drag.vertical {
        (track.height, thumb.height, at.y)
    } else {
        (track.width, thumb.width, at.x)
    };
    let room = f64::from(track - thumb);
    let moved = f64::from(along - drag.from);
    let offset = if room > 0.0 {
        drag.start + moved * max / room
    } else {
        drag.start
    };
    let offset = snap(offset, scale);
    geometry.clamp(if drag.vertical {
        (x, offset)
    } else {
        (offset, y)
    })
}

/// What the keys that scroll do to a focused view.
fn keys(geometry: &Geometry, key: Key, line: f64, scale: f32) -> Outcome {
    let (x, y) = geometry.offset;
    let (max_x, max_y) = geometry.max();
    // Home, End and the page keys work on the vertical axis, or the
    // horizontal one in a view that only scrolls sideways.
    let vertical = geometry.y && max_y > 0.0 || !(geometry.x && max_x > 0.0);
    let page = f64::from(if vertical {
        geometry.viewport.height
    } else {
        geometry.viewport.width
    });
    let target = match key {
        Key::Up if max_y > 0.0 => (x, y - line),
        Key::Down if max_y > 0.0 => (x, y + line),
        Key::Left if max_x > 0.0 => (x - line, y),
        Key::Right if max_x > 0.0 => (x + line, y),
        Key::PageUp | Key::PageDown | Key::Home | Key::End
            if (vertical && max_y > 0.0) || (!vertical && max_x > 0.0) =>
        {
            let (at, max) = if vertical { (y, max_y) } else { (x, max_x) };
            let to = match key {
                Key::PageUp => at - page,
                Key::PageDown => at + page,
                Key::Home => 0.0,
                _ => max,
            };
            if vertical { (x, to) } else { (to, y) }
        }
        _ => return Outcome::NO,
    };
    let target = geometry.clamp(target);
    Outcome::yes(Some(
        geometry.clamp((snap(target.0, scale), snap(target.1, scale))),
    ))
}

/// Draws the bars of `geometry`: each track faint, its thumb if the
/// content overflows, both in the focus colour when the view has the
/// keyboard focus, which shows there because the content covers the rest
/// of the view.
pub(crate) fn paint_bars(scene: &mut Scene, geometry: &Geometry, theme: &Theme, focused: bool) {
    let palette = &theme.palette;
    let track_color = if focused {
        palette.focus.with_alpha(0x55)
    } else {
        palette.track.with_alpha(0x66)
    };
    let thumb_color = if focused {
        palette.focus
    } else {
        palette.text_muted
    };
    // A margin inside the gutter, so that a bar never touches the content.
    let inset = |rect: Rect| {
        let by = (rect.width.min(rect.height) * 0.2).min(2.0);
        Rect::new(
            rect.x + by,
            rect.y + by,
            (rect.width - 2.0 * by).max(0.0),
            (rect.height - 2.0 * by).max(0.0),
        )
    };
    let radius = theme.corners.small;
    for (track, thumb) in [
        (geometry.track_y(), geometry.thumb_y()),
        (geometry.track_x(), geometry.thumb_x()),
    ] {
        let Some(track) = track else {
            continue;
        };
        scene.fill(RoundedRect::new(inset(track), radius), track_color);
        if let Some(thumb) = thumb {
            scene.fill(RoundedRect::new(inset(thumb), radius), thumb_color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry(offset: f64, content: f64) -> Geometry {
        Geometry {
            viewport: Rect::new(0.0, 0.0, 90.0, 100.0),
            offset: (0.0, offset),
            content: (90.0, content),
            x: false,
            y: true,
            bar: 10.0,
        }
    }

    #[test]
    fn a_thumb_is_as_long_as_the_part_shown_and_moves_with_the_offset() {
        let top = geometry(0.0, 400.0);
        assert_eq!(top.thumb_y(), Some(Rect::new(90.0, 0.0, 10.0, 25.0)));
        let end = geometry(300.0, 400.0);
        assert_eq!(end.thumb_y(), Some(Rect::new(90.0, 75.0, 10.0, 25.0)));
        // A long list keeps a thumb that can be grabbed.
        let long = geometry(0.0, 2_400_000.0);
        assert_eq!(long.thumb_y().map(|thumb| thumb.height), Some(16.0));
        // Nothing overflows: no thumb.
        assert_eq!(geometry(0.0, 80.0).thumb_y(), None);
    }

    #[test]
    fn the_wheel_is_left_to_the_view_outside_at_an_end() {
        let mut bars = Bars::default();
        let wheel = |notches: f32| Event::Wheel {
            at: Point::new(10.0, 10.0),
            delta: Point::new(0.0, notches),
            modifiers: crate::Modifiers::NONE,
        };
        let top = geometry(0.0, 400.0);
        let up = bars.event(&top, &wheel(1.0), false, 40.0, 1.0);
        assert_eq!(up.handled, Handled::No);
        let down = bars.event(&top, &wheel(-1.0), false, 40.0, 1.0);
        assert_eq!(down.scroll_to, Some((0.0, 40.0)));
        let end = geometry(300.0, 400.0);
        assert_eq!(
            bars.event(&end, &wheel(-1.0), false, 40.0, 1.0).handled,
            Handled::No
        );
    }

    #[test]
    fn a_drag_scrolls_faster_nearer_the_edge_and_not_at_all_inside() {
        let viewport = Rect::new(0.0, 0.0, 200.0, 200.0);
        let step = Duration::from_millis(10);
        let middle = auto_scroll(viewport, Point::new(100.0, 100.0), step);
        assert_eq!(middle, Point::ZERO);
        let near = auto_scroll(viewport, Point::new(100.0, 190.0), step);
        let nearer = auto_scroll(viewport, Point::new(100.0, 199.0), step);
        assert!(near.y > 0.0 && nearer.y > near.y, "{near:?} {nearer:?}");
        let top = auto_scroll(viewport, Point::new(100.0, 1.0), step);
        assert!(top.y < 0.0 && top.x == 0.0);
        // A late frame does not jump.
        let late = auto_scroll(viewport, Point::new(100.0, 199.0), Duration::from_secs(2));
        assert!(late.y <= AUTO_SPEED * AUTO_STEP.as_secs_f32());
    }
}
