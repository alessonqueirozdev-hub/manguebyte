//! Drawing text: where pixels go, partial repaints, bounds, and the glyph
//! cache's budget.

use manguebyte_gfx::{
    Color, IRect, Layer, OpticalSize, Pixmap, Point, Rect, Renderer, RoundedRect, Scene,
    TextLayout, TextStyle, Transform,
};

/// A small generator of pseudo-random numbers (xorshift64*), so the cases
/// are the same on every run.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const WORDS: [&str; 12] = [
    "Gain",
    "Low cut",
    "\u{2212}12.5 dB",
    "AVATAR",
    "Resonance",
    "440 Hz",
    "a->b",
    "Caf\u{e9}",
    "Wet/Dry",
    "Attack 10 ms",
    "\u{c5}r\u{f8}",
    "Q 0.71",
];

fn label(r: &mut Random, size: f32) -> TextLayout {
    let text = WORDS[r.below(WORDS.len())];
    let style = TextStyle::new(size)
        .with_weight(100.0 + r.unit() * 800.0)
        .with_tabular_figures(r.below(2) == 0)
        .with_color(Color::rgba(
            r.next() as u8,
            r.next() as u8,
            r.next() as u8,
            if r.below(2) == 0 { 255 } else { r.next() as u8 },
        ));
    TextLayout::new(text, &style, None)
}

fn render(scene: &Scene, width: u32, height: u32, scale: f32) -> Pixmap {
    let mut pixels = Pixmap::new(width, height).expect("small");
    Renderer::new().render(scene, &mut pixels, scale);
    pixels
}

/// The pixels with any coverage that lie outside `area`.
fn drawn_outside(pixels: &Pixmap, area: IRect) -> Vec<(u32, u32, u8)> {
    let mut outside = Vec::new();
    for y in 0..pixels.height() {
        for x in 0..pixels.width() {
            let a = pixels.pixel(x, y).map_or(0, |p| p.alpha());
            if a > 0 && !area.contains(i64::from(x), i64::from(y)) {
                outside.push((x, y, a));
            }
        }
    }
    outside
}

#[test]
fn every_pixel_text_changes_lies_inside_the_scene_bounds() {
    // Unpadded: the bounds, mapped to device pixels and rounded out, hold
    // every pixel with any coverage. Glyphs drawn from masks (scales of a
    // half and more), and as outlines (smaller scales, turned text, and
    // glyphs over 256 device pixels per em).
    let mut r = Random(0x9E37_79B9_7F4A_7C15);
    for case in 0..160 {
        let size = match case % 4 {
            0 => 6.0 + r.unit() * 10.0,
            1 => 12.0 + r.unit() * 30.0,
            2 => 40.0 + r.unit() * 30.0,
            _ => 9.0 + r.unit(),
        };
        let layout = label(&mut r, size);
        let at = Point::new(3.0 + r.unit() * 7.0, 2.0 + r.unit() * 5.0);
        let mut scene = Scene::new();
        if case % 5 == 4 {
            let turn = Transform::rotate_about(r.unit() - 0.5, Point::new(40.0, 30.0));
            scene.transformed(turn, |s| s.text(&layout, at));
        } else {
            scene.text(&layout, at);
        }
        let Some(bounds) = scene.bounds() else {
            continue;
        };
        for scale in [0.3, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.5] {
            let pixels = render(&scene, 320, 200, scale);
            let outside = drawn_outside(&pixels, bounds.to_device(scale));
            assert!(
                outside.is_empty(),
                "case {case} at {scale}x, bounds {bounds:?}, outside: {outside:?}\n{}",
                scene.dump()
            );
        }
    }
}

#[test]
fn text_far_from_the_origin_stays_inside_the_scene_bounds() {
    // Far from the origin an f32 has no fraction left: at 10^7 a unit is
    // the last place. Glyph positions moved there are rounded, and the ink
    // box must follow them, as a rectangle's bounds follow the rectangle.
    // (A long timeline scrolled by a transform gets there: 10 minutes at
    // 48 kHz and a pixel per sample is 2.9 * 10^7.)
    let mut r = Random(0x6A09_E667_F3BC_C908);
    for case in 0..40 {
        let far = [1.0e6f32, 3.3e6, 1.0e7, 1.7e7, 2.9e7][case % 5];
        let size = 9.0 + r.unit() * 20.0;
        let layout = label(&mut r, size);
        let at = Point::new(far + 3.3 + r.unit() * 40.0, 2.0 + r.unit() * 8.0);
        let mut text = Scene::new();
        text.transformed(Transform::translate(-far, 0.0), |s| s.text(&layout, at));
        let mut rect = Scene::new();
        rect.transformed(Transform::translate(-far, 0.0), |s| {
            s.fill(Rect::new(at.x, at.y, 50.7, 9.3), Color::WHITE)
        });
        for (name, scene) in [("rectangle", &rect), ("text", &text)] {
            let Some(bounds) = scene.bounds() else {
                continue;
            };
            for scale in [1.0f32, 2.0] {
                let pixels = render(scene, 260, 100, scale);
                let outside = drawn_outside(&pixels, bounds.to_device(scale));
                assert!(
                    outside.is_empty(),
                    "case {case}, {name} at {scale}x: bounds {bounds:?}, drawn outside at {outside:?}"
                );
            }
        }
    }
}

#[test]
fn translucent_text_is_blended_once_where_glyphs_overlap() {
    // Inter's "ff" and "ft" join at the crossbar, so their outlines
    // overlap. Drawn from masks or as one outline (forced by a skew too
    // small to move a point), the overlap gets the colour's alpha once,
    // not a second time on top.
    for weight in [400.0, 900.0] {
        let style = TextStyle::new(40.0)
            .with_weight(weight)
            .with_color(Color::rgba(255, 255, 255, 128));
        for text in ["ff", "ft", "fft"] {
            let layout = TextLayout::new(text, &style, None);
            let mut masked = Scene::new();
            masked.text(&layout, Point::new(4.0, 2.0));
            let mut outlined = Scene::new();
            outlined.transformed(Transform::new(1.0, 0.0, 1e-30, 1.0, 0.0, 0.0), |s| {
                s.text(&layout, Point::new(4.0, 2.0))
            });
            let most = |p: &Pixmap| {
                (0..p.height())
                    .flat_map(|y| (0..p.width()).map(move |x| (x, y)))
                    .map(|(x, y)| p.pixel(x, y).map_or(0, |c| c.alpha()))
                    .max()
                    .unwrap_or(0)
            };
            let a = render(&masked, 140, 60, 1.0);
            let b = render(&outlined, 140, 60, 1.0);
            assert_eq!(most(&b), 128, "{text} at {weight}: outlines");
            assert_eq!(most(&a), 128, "{text} at {weight}: masks");
            // Over an opaque background as well: nowhere darker than one
            // blend of the colour at full coverage.
            let mut over = Scene::new();
            over.fill(Rect::new(0.0, 0.0, 140.0, 60.0), Color::BLACK);
            over.text(&layout, Point::new(4.0, 2.0));
            let c = render(&over, 140, 60, 1.0);
            let brightest = (0..60)
                .flat_map(|y| (0..140).map(move |x| (x, y)))
                .map(|(x, y)| c.pixel(x, y).map_or(0, |p| p.to_array()[0]))
                .max()
                .unwrap_or(0);
            assert_eq!(brightest, 128, "{text} at {weight}");
        }
    }
}

/// Checks that a partial render changed nothing outside the damage and
/// drew everything inside it as the whole render did.
fn assert_partial_matches(
    case: usize,
    before: &Pixmap,
    whole: &Pixmap,
    part: &Pixmap,
    damage: &[IRect],
) {
    for y in 0..before.height() {
        for x in 0..before.width() {
            let inside = damage
                .iter()
                .any(|d| d.contains(i64::from(x), i64::from(y)));
            let expected = if inside { whole } else { before };
            assert_eq!(
                part.pixel(x, y),
                expected.pixel(x, y),
                "case {case}: ({x}, {y}) inside the damage: {inside}"
            );
        }
    }
}

#[test]
fn text_repaints_in_part_exactly_as_whole() {
    let mut r = Random(0x2545_F491_4F6C_DD1D);
    let mut renderer = Renderer::new();
    for case in 0..120 {
        let (w, h) = (100 + r.below(140) as u32, 60 + r.below(100) as u32);
        let scale = [0.4, 0.5, 1.0, 1.25, 2.0, 3.0][r.below(6)];
        let mut scene = Scene::new();
        scene.fill(
            Rect::new(0.0, 0.0, w as f32, h as f32),
            Color::hex(0x1C1F24),
        );
        for _ in 0..1 + r.below(5) {
            let size = 6.0 + r.unit() * 40.0;
            let layout = label(&mut r, size);
            let at = Point::new(
                r.unit() * w as f32 / scale - 10.0,
                r.unit() * h as f32 / scale - 10.0,
            );
            match r.below(5) {
                0 => scene.clipped(
                    RoundedRect::new(Rect::new(at.x, at.y, 30.0, 12.0), 4.0),
                    |s| s.text(&layout, at),
                ),
                1 => scene.layer(Layer::with_opacity(0.5), |s| s.text(&layout, at)),
                2 => scene.transformed(Transform::rotate_about(0.3, at), |s| s.text(&layout, at)),
                _ => scene.text(&layout, at),
            }
        }
        let damage: Vec<IRect> = (0..1 + r.below(4))
            .map(|_| {
                IRect::new(
                    r.below(w as usize) as i32 - 4,
                    r.below(h as usize) as i32 - 4,
                    1 + r.below(60) as u32,
                    1 + r.below(40) as u32,
                )
            })
            .collect();
        let mut before = Pixmap::new(w, h).expect("small");
        before.fill(Color::rgb(r.next() as u8, 40, 90));
        let mut whole = before.clone();
        renderer.render(&scene, &mut whole, scale);
        let mut part = before.clone();
        renderer.render_damage(&scene, &mut part, scale, &damage);
        assert_partial_matches(case, &before, &whole, &part, &damage);
    }
}

#[test]
fn the_glyph_cache_never_passes_its_budget() {
    // A budget of 12 KiB holds a few dozen small masks: drawing many sizes
    // and weights forces it to drop masks again and again. What is drawn
    // does not depend on the budget.
    let mut r = Random(0xD1B5_4A32_D192_ED03);
    let mut small = Renderer::new();
    small.set_glyph_cache_limit(12 * 1024);
    let mut none = Renderer::new();
    none.set_glyph_cache_limit(0);
    let mut roomy = Renderer::new();
    for case in 0..60 {
        let mut scene = Scene::new();
        for row in 0..4 {
            let size = 8.0 + r.unit() * 30.0;
            let layout = label(&mut r, size);
            scene.text(&layout, Point::new(2.0 + r.unit(), 2.0 + row as f32 * 30.0));
        }
        let scale = [1.0, 1.5, 2.0][case % 3];
        let mut a = Pixmap::new(260, 260).expect("small");
        let mut b = a.clone();
        let mut c = a.clone();
        small.render(&scene, &mut a, scale);
        none.render(&scene, &mut b, scale);
        roomy.render(&scene, &mut c, scale);
        let stats = small.glyph_cache();
        assert!(stats.bytes <= stats.limit, "case {case}: {stats:?}");
        assert_eq!(none.glyph_cache().bytes, 0);
        assert_eq!(none.glyph_cache().masks, 0);
        assert_eq!(a, c, "case {case}");
        assert_eq!(b, c, "case {case}");
    }
    let stats = small.glyph_cache();
    assert!(stats.evictions > 100, "{stats:?}");
    assert!(roomy.glyph_cache().bytes <= roomy.glyph_cache().limit);
    assert_eq!(roomy.glyph_cache().limit, 4 << 20);
    // Shrinking the budget drops masks at once.
    roomy.set_glyph_cache_limit(1000);
    assert!(roomy.glyph_cache().bytes <= 1000);
    roomy.clear_glyph_cache();
    assert_eq!(roomy.glyph_cache().masks, 0);
}

#[test]
fn a_warm_cache_draws_every_glyph_from_a_mask_it_holds() {
    let layout = TextLayout::new(
        "Attack 10 ms, Release 200 ms",
        &TextStyle::new(12.0).with_color(Color::WHITE),
        None,
    );
    let mut scene = Scene::new();
    scene.text(&layout, Point::new(2.0, 2.0));
    let mut pixels = Pixmap::new(400, 40).expect("small");
    let mut renderer = Renderer::new();
    renderer.render(&scene, &mut pixels, 2.0);
    let cold = renderer.glyph_cache();
    // Every glyph was looked up once; masks were drawn only for glyphs
    // not seen before at the same quarter pixel.
    assert_eq!(cold.hits + cold.misses, layout.glyphs().len() as u64);
    assert!(cold.misses >= 1 && cold.misses <= layout.glyphs().len() as u64);
    let first = pixels.clone();
    renderer.render(&scene, &mut pixels, 2.0);
    let warm = renderer.glyph_cache();
    assert_eq!(warm.misses, cold.misses, "no mask was drawn again");
    assert_eq!(warm.hits, cold.hits + layout.glyphs().len() as u64);
    assert_eq!(warm.masks, cold.masks);
    // Drawn over itself in opaque white, the image is the same as before
    // only where coverage was full or empty, so compare a fresh render.
    let mut again = Pixmap::new(400, 40).expect("small");
    renderer.render(&scene, &mut again, 2.0);
    let mut fresh = Pixmap::new(400, 40).expect("small");
    Renderer::new().render(&scene, &mut fresh, 2.0);
    assert_eq!(again, fresh);
    assert_ne!(first, Pixmap::new(400, 40).expect("small"));
}

#[test]
fn a_glyph_from_its_mask_matches_its_outline_filled_as_a_path() {
    // At 32 pixels a font unit is 1/64 of a pixel and the ascent (1984
    // units) is 31 pixels, so a glyph at (x, 0) with whole or quarter x
    // sits exactly on the quarter-pixel grid masks use. Drawn from its mask
    // and as a path (forced by a skew of 1e-30, too small to move a point),
    // the two images differ only by the rasteriser's rounding at edges.
    let style = TextStyle::new(32.0)
        .with_optical_size(OpticalSize::Fixed(14.0))
        .with_color(Color::WHITE);
    for (text, x) in [("W", 0.0), ("g", 0.25), ("&", 3.5), ("Q", 7.75)] {
        let layout = TextLayout::new(text, &style, None);
        assert_eq!(layout.baseline(), 31.0);
        let mut masked = Scene::new();
        masked.text(&layout, Point::new(x, 0.0));
        let mut outlined = Scene::new();
        outlined.transformed(Transform::new(1.0, 0.0, 1e-30, 1.0, 0.0, 0.0), |s| {
            s.text(&layout, Point::new(x, 0.0))
        });
        let a = render(&masked, 48, 44, 1.0);
        let b = render(&outlined, 48, 44, 1.0);
        let mut worst = 0u8;
        let mut drawn = 0;
        for y in 0..44 {
            for x in 0..48 {
                let (pa, pb) = (
                    a.pixel(x, y).map_or(0, |p| p.alpha()),
                    b.pixel(x, y).map_or(0, |p| p.alpha()),
                );
                worst = worst.max(pa.abs_diff(pb));
                drawn += usize::from(pa > 0);
            }
        }
        assert!(drawn > 40, "{text}: {drawn} pixels drawn");
        assert!(
            worst <= 8,
            "{text} at {x}: masks and outlines differ by {worst}"
        );
    }
}

#[test]
fn text_follows_clips_and_colours() {
    let layout = TextLayout::new(
        "WWWWWWWW",
        &TextStyle::new(20.0).with_color(Color::WHITE),
        None,
    );
    let mut clipped = Scene::new();
    clipped.clipped(Rect::new(10.0, 0.0, 20.0, 30.0), |s| {
        s.text(&layout, Point::ZERO)
    });
    let pixels = render(&clipped, 120, 30, 1.0);
    assert!(drawn_outside(&pixels, IRect::new(10, 0, 20, 30)).is_empty());
    let inside = (0..30)
        .flat_map(|y| (10..30).map(move |x| (x, y)))
        .filter(|&(x, y)| pixels.pixel(x, y).is_some_and(|p| p.alpha() > 0))
        .count();
    assert!(inside > 50, "{inside} pixels drawn inside the clip");

    // A transparent colour draws nothing; changing the colour needs no new
    // layout and shows in the dump.
    let mut invisible = layout.clone();
    invisible.set_color(Color::TRANSPARENT);
    let mut scene = Scene::new();
    scene.text(&invisible, Point::ZERO);
    assert_eq!(
        render(&scene, 120, 30, 1.0),
        Pixmap::new(120, 30).expect("small")
    );
    assert!(scene.dump().contains("solid(#00000000)"));
    assert_eq!(invisible.glyphs(), layout.glyphs());
}

#[test]
fn the_dump_gives_glyph_numbers_and_positions_exactly() {
    // From the font: A is glyph 1, V glyph 103; A advances 1413 units and
    // the pair is kerned by -140. At 2048 pixels a unit is a pixel.
    let style = TextStyle::new(2048.0).with_optical_size(OpticalSize::Fixed(14.0));
    let layout = TextLayout::new("AV", &style, None);
    let mut scene = Scene::new();
    scene.text(&layout, Point::new(0.5, -2.0));
    assert_eq!(
        scene.dump(),
        "text \"AV\" size 2048 weight 400 opsz 14 solid(#000000ff) [1 0.5 1982, 103 1273.5 1982]\n"
    );
    // Long text is summarised, and quotes and line feeds are escaped.
    let long: String = "ab\"\n".repeat(30);
    let layout = TextLayout::new(&long, &TextStyle::new(10.0), None);
    let mut scene = Scene::new();
    scene.text(&layout, Point::ZERO);
    let dump = scene.dump();
    assert!(dump.starts_with("text \"ab\\\"\\nab\\\""), "{dump}");
    assert!(dump.contains("\"... size 10 weight 400 opsz 14"), "{dump}");
    assert!(dump.contains("[90 glyphs at rect(0 "), "{dump}");
    assert!(dump.ends_with("]\n"), "{dump}");
}
