//! Zoom to fit: how an interface designed at one size fills a window of
//! another (ADR 0027).
//!
//! Nothing here touches a window or a root: [`fit`] is a sum from sizes to
//! sizes, which the glue applies. The rest of the work is the root's scale:
//! the layout stays at the design size, and the scale takes the system's
//! times the zoom, so what is drawn, what is hit and what assistive
//! technology is told all follow from one number.

use manguebyte_gfx::{IRect, Pixmap, Size};
use manguebyte_plugin::editor::{HostSize, MAX_SCALE, MIN_SCALE};
use manguebyte_ui::RESPONSIVE_MARGIN;
use manguebyte_window::{PhysicalPoint, PhysicalSize};

/// What an editor asked for with `Editor::with_zoom_to_fit` and
/// `Editor::with_real_size_below`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ZoomRules {
    /// The smallest zoom; below it the layout reflows.
    pub(crate) min_zoom: f32,
    /// The window below which there is no zoom at all, in logical pixels.
    pub(crate) real_below: Option<Size>,
}

/// The size and scale a root takes in a window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Fitted {
    /// The layout's size, in logical pixels of the interface.
    pub(crate) size: Size,
    /// Device pixels per logical pixel: the system's scale times the zoom.
    pub(crate) scale: f32,
}

/// A side of the back buffer of `length` logical pixels at `scale`, as the
/// root works it out.
fn side(length: f64, scale: f64) -> f64 {
    (length * f64::from(scale as f32)).round()
}

/// Works out what the root takes for a window of `window` device pixels at
/// the system's scale `system`, for an interface designed at `design`.
/// `real` is whether the window was last below the size given with
/// `Editor::with_real_size_below`, `None` the first time.
pub(crate) fn fit(
    design: HostSize,
    rules: ZoomRules,
    window: PhysicalSize,
    system: f64,
    real: &mut Option<bool>,
) -> Fitted {
    let (width, height) = (f64::from(window.width), f64::from(window.height));
    let (logical_width, logical_height) = (width / system, height / system);
    let filling = |scale: f64| Fitted {
        size: Size::new((width / scale) as f32, (height / scale) as f32),
        scale: scale as f32,
    };

    if let Some(below) = rules.real_below {
        // The same rule as `responsive`'s, on the same numbers, so that a
        // layout zoomed and one at its real size change as one.
        let margin = if *real == Some(false) {
            RESPONSIVE_MARGIN
        } else {
            0.0
        };
        let slack = 0.501 / system;
        let under = |length: f64, limit: f32| length + slack < f64::from(limit - margin);
        let now = under(logical_width, below.width) || under(logical_height, below.height);
        *real = Some(now);
        if now {
            return filling(system);
        }
    }

    let (design_width, design_height) = (
        f64::from(design.width.max(1)),
        f64::from(design.height.max(1)),
    );
    // A window of the design size, as rounding left it at this scale, has
    // no zoom at all, not 0.999: that would soften everything for nothing.
    if side(design_width, system) == width && side(design_height, system) == height {
        return Fitted {
            size: Size::new(design_width as f32, design_height as f32),
            scale: system as f32,
        };
    }
    let zoom = (logical_width / design_width).min(logical_height / design_height);
    let most = MAX_SCALE / system;
    let least = f64::from(rules.min_zoom).max(MIN_SCALE / system).min(most);
    if zoom < least {
        return filling(system * least);
    }
    Fitted {
        size: Size::new(design_width as f32, design_height as f32),
        scale: (system * zoom.min(most)) as f32,
    }
}

/// A smallest zoom an editor may ask for: any that is not a positive number
/// counts as no limit but the smallest scale there is.
pub(crate) fn clean_min_zoom(min_zoom: f32) -> f32 {
    if min_zoom.is_finite() && min_zoom > 0.0 {
        min_zoom
    } else {
        0.0
    }
}

/// Where a back buffer of `pixels` goes in a window of `window` device
/// pixels: in the middle, on whole pixels.
pub(crate) fn origin(window: PhysicalSize, pixels: &Pixmap) -> PhysicalPoint {
    let middle = |window: u32, buffer: u32| (window.saturating_sub(buffer) / 2) as i32;
    PhysicalPoint::new(
        middle(window.width, pixels.width()),
        middle(window.height, pixels.height()),
    )
}

/// The rectangles of a window of `window` device pixels that a back buffer
/// of `pixels` at `origin` leaves: above, below, left and right of it.
/// Those it does not leave are empty.
pub(crate) fn bars(window: PhysicalSize, pixels: &Pixmap, origin: PhysicalPoint) -> [IRect; 4] {
    let (width, height) = (i64::from(window.width), i64::from(window.height));
    let (left, top) = (i64::from(origin.x), i64::from(origin.y));
    let right = left + i64::from(pixels.width());
    let bottom = top + i64::from(pixels.height());
    let rect = |x0: i64, y0: i64, x1: i64, y1: i64| {
        IRect::new(
            x0 as i32,
            y0 as i32,
            (x1 - x0).max(0) as u32,
            (y1 - y0).max(0) as u32,
        )
    };
    [
        rect(0, 0, width, top),
        rect(0, bottom, width, height),
        rect(0, top, left, bottom),
        rect(right, top, width, bottom),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESIGN: HostSize = HostSize::new(1600, 980);

    fn rules(min_zoom: f32) -> ZoomRules {
        ZoomRules {
            min_zoom,
            real_below: None,
        }
    }

    fn fitted(min_zoom: f32, width: u32, height: u32, system: f64) -> Fitted {
        fit(
            DESIGN,
            rules(min_zoom),
            PhysicalSize::new(width, height),
            system,
            &mut None,
        )
    }

    #[test]
    fn the_zoom_is_the_smaller_of_the_two_axes_and_the_layout_stays() {
        let design = Size::new(1600.0, 980.0);
        // 1920 by 1050 at 125 %: 1536 by 840 logical, so 840 of 980.
        let at = fitted(0.25, 1920, 1050, 1.25);
        assert_eq!(at.size, design);
        assert!((at.scale - 1050.0 / 980.0).abs() < 1e-6, "{}", at.scale);
        // Wide and short, tall and narrow, larger than the design.
        assert_eq!(fitted(0.25, 3200, 490, 1.0).scale, 0.5);
        assert_eq!(fitted(0.25, 800, 980, 1.0).scale, 0.5);
        assert_eq!(fitted(0.25, 3200, 1960, 1.0).scale, 2.0);
    }

    #[test]
    fn a_window_of_the_design_size_has_no_zoom_whatever_the_rounding() {
        // 1601 at 1.25 is 2001.25, which a window can only be as 2001.
        let design = HostSize::new(1601, 981);
        for system in [1.0_f64, 1.25, 1.5, 1.75, 2.0] {
            let width = (1601.0 * system).round() as u32;
            let height = (981.0 * system).round() as u32;
            let at = fit(
                design,
                rules(0.25),
                PhysicalSize::new(width, height),
                system,
                &mut None,
            );
            assert_eq!(at.scale, system as f32);
            assert_eq!(at.size, Size::new(1601.0, 981.0));
        }
    }

    #[test]
    fn a_buffer_of_the_limiting_axis_is_exactly_the_windows_side() {
        // The bars come only on the other axis, with no line of them on
        // this one from rounding, for every width.
        let design = HostSize::new(1601, 977);
        for width in 400..1900_u32 {
            let window = PhysicalSize::new(width, 5000);
            let at = fit(design, rules(0.0), window, 1.0, &mut None);
            assert_eq!(
                side(1601.0, f64::from(at.scale)),
                f64::from(width),
                "{at:?}"
            );
        }
    }

    #[test]
    fn below_the_smallest_zoom_the_layout_fills_the_window_at_that_zoom() {
        let at = fitted(0.75, 1000, 500, 1.0);
        assert_eq!(at.scale, 0.75);
        assert!((at.size.width - 1000.0 / 0.75).abs() < 1e-3);
        assert!((at.size.height - 500.0 / 0.75).abs() < 1e-3);
        // Just above it there is still the design size.
        assert_eq!(fitted(0.75, 1200, 735, 1.0).size, Size::new(1600.0, 980.0));
        // It counts on top of the system's scale.
        assert_eq!(fitted(0.5, 800, 300, 2.0).scale, 1.0);
    }

    #[test]
    fn no_scale_leaves_what_a_root_can_draw() {
        // No limit asked for: the smallest scale there is.
        let at = fitted(0.0, 100, 100, 1.0);
        assert_eq!(at.scale, MIN_SCALE as f32);
        assert_eq!(at.size, Size::new(400.0, 400.0));
        // At the largest, the layout does not grow: the bars do.
        let at = fitted(0.25, 16000, 9800, 1.0);
        assert_eq!(at.scale, MAX_SCALE as f32);
        assert_eq!(at.size, Size::new(1600.0, 980.0));
        // A smallest zoom past the largest gives way to it.
        let at = fitted(100.0, 1600, 980, 1.0);
        assert_eq!(at.scale, 1.0, "the design size is no zoom");
        assert_eq!(fitted(100.0, 1599, 980, 1.0).scale, MAX_SCALE as f32);
    }

    #[test]
    fn a_window_with_no_area_makes_nothing_odd() {
        let at = fitted(0.5, 0, 0, 1.0);
        assert_eq!(at.size, Size::new(0.0, 0.0));
        assert_eq!(at.scale, 0.5);
    }

    #[test]
    fn the_real_size_has_a_margin_on_the_way_down_only() {
        let rules = ZoomRules {
            min_zoom: 0.25,
            real_below: Some(Size::new(1100.0, 700.0)),
        };
        let mut real = None;
        let mut at = |width: u32, height: u32| {
            fit(
                DESIGN,
                rules,
                PhysicalSize::new(width, height),
                1.0,
                &mut real,
            )
        };
        // The first time, the size itself.
        assert_eq!(at(1099, 800).scale, 1.0);
        assert_eq!(at(1099, 800).size, Size::new(1099.0, 800.0));
        // Up: at the size itself. Down: the margin below it.
        assert_eq!(at(1100, 800).size, Size::new(1600.0, 980.0));
        let margin = RESPONSIVE_MARGIN as u32;
        assert_eq!(at(1100 - margin, 800).size, Size::new(1600.0, 980.0));
        assert_eq!(
            at(1100 - margin - 1, 800).size,
            Size::new((1100 - margin - 1) as f32, 800.0)
        );
        // Either axis takes it there.
        assert_eq!(at(1500, 800).size, Size::new(1600.0, 980.0));
        assert_eq!(at(1500, 600).size, Size::new(1500.0, 600.0));
    }

    #[test]
    fn the_bars_are_what_the_buffer_leaves() {
        let pixels = Pixmap::new(100, 50).expect("small");
        let window = PhysicalSize::new(301, 50);
        let at = origin(window, &pixels);
        assert_eq!(at, PhysicalPoint::new(100, 0));
        let [above, below, left, right] = bars(window, &pixels, at);
        assert!(above.is_empty() && below.is_empty());
        assert_eq!(left, IRect::new(0, 0, 100, 50));
        assert_eq!(right, IRect::new(200, 0, 101, 50));

        let window = PhysicalSize::new(120, 70);
        let at = origin(window, &pixels);
        assert_eq!(at, PhysicalPoint::new(10, 10));
        let found = bars(window, &pixels, at);
        assert_eq!(found[0], IRect::new(0, 0, 120, 10));
        assert_eq!(found[1], IRect::new(0, 60, 120, 10));
        assert_eq!(found[2], IRect::new(0, 10, 10, 50));
        assert_eq!(found[3], IRect::new(110, 10, 10, 50));

        // No window, no bars.
        let window = PhysicalSize::new(100, 50);
        for bar in bars(window, &pixels, origin(window, &pixels)) {
            assert!(bar.is_empty());
        }
    }
}
