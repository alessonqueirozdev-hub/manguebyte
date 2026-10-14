//! What the renderer draws, checked pixel by pixel.
//!
//! Where coverage is whole, values are exact. Where an edge cuts a pixel,
//! the expected value is the covered fraction, and the test allows the
//! rasteriser's rounding of it, stated next to each case.

use manguebyte_gfx::{
    Arc, Circle, Color, Extend, IRect, Image, Layer, LinearGradient, Path, Pixmap, Point,
    PremulColor, Rect, Renderer, RoundedRect, Sampling, Scene, Stop, Stroke, Transform,
};

fn render(scene: &Scene, width: u32, height: u32, scale: f32) -> Pixmap {
    let mut pixels = Pixmap::new(width, height).expect("small");
    Renderer::new().render(scene, &mut pixels, scale);
    pixels
}

fn alpha(p: &Pixmap, x: u32, y: u32) -> u8 {
    p.pixel(x, y).expect("inside").alpha()
}

/// The pixels whose alpha is at least `at_least`, as (x, y) pairs.
fn covered(p: &Pixmap, at_least: u8) -> Vec<(u32, u32)> {
    (0..p.height())
        .flat_map(|y| (0..p.width()).map(move |x| (x, y)))
        .filter(|&(x, y)| alpha(p, x, y) >= at_least)
        .collect()
}

fn block(x0: u32, y0: u32, x1: u32, y1: u32) -> Vec<(u32, u32)> {
    (y0..y1)
        .flat_map(|y| (x0..x1).map(move |x| (x, y)))
        .collect()
}

/// A pattern no renderer would draw by accident, opaque so it is valid.
fn sentinel(width: u32, height: u32) -> Pixmap {
    let mut p = Pixmap::new(width, height).expect("small");
    for y in 0..height {
        for x in 0..width {
            let color = PremulColor::new((x * 13) as u8, (y * 29) as u8, (x ^ y) as u8 | 1, 255)
                .expect("opaque");
            p.set_pixel(x, y, color);
        }
    }
    p
}

#[test]
fn an_aligned_rectangle_covers_exactly_its_pixels_at_every_whole_scale() {
    let mut scene = Scene::new();
    scene.fill(Rect::new(1.0, 2.0, 3.0, 1.0), Color::hex(0x336699));
    for scale in [1u32, 2, 3] {
        let p = render(&scene, 8 * scale, 8 * scale, scale as f32);
        assert_eq!(
            covered(&p, 1),
            block(scale, 2 * scale, 4 * scale, 3 * scale),
            "at {scale}x"
        );
        assert_eq!(
            p.pixel(scale, 2 * scale).map(PremulColor::to_array),
            Some([0x33, 0x66, 0x99, 0xFF])
        );
    }
}

#[test]
fn an_edge_through_the_middle_of_a_pixel_covers_half_of_it() {
    let mut scene = Scene::new();
    scene.fill(Rect::new(0.5, 0.0, 2.0, 1.0), Color::WHITE);
    let p = render(&scene, 4, 1, 1.0);
    // 127.5 of 255: the rasteriser rounds it to 127 or 128.
    for x in [0, 2] {
        let a = alpha(&p, x, 0);
        assert!((127..=128).contains(&a), "pixel {x}: {a}");
    }
    assert_eq!(alpha(&p, 1, 0), 255);
    assert_eq!(alpha(&p, 3, 0), 0);
    // At 1.5x the same rectangle has edges at 0.75 and 3.75.
    let p = render(&scene, 6, 2, 1.5);
    let a = alpha(&p, 0, 0);
    assert!((63..=64).contains(&a), "a quarter: {a}");
    assert_eq!(alpha(&p, 1, 0), 255);
    let a = alpha(&p, 3, 0);
    assert!((191..=192).contains(&a), "three quarters: {a}");
}

#[test]
fn a_stroke_is_centred_on_the_outline() {
    let mut scene = Scene::new();
    // A 2-pixel stroke on a rectangle with whole edges covers one pixel
    // each side of every edge.
    scene.stroke(
        Rect::new(2.0, 2.0, 4.0, 4.0),
        Stroke::new(2.0),
        Color::BLACK,
    );
    let p = render(&scene, 8, 8, 1.0);
    let ring: Vec<(u32, u32)> = block(1, 1, 7, 7)
        .into_iter()
        .filter(|&(x, y)| !(3..5).contains(&x) || !(3..5).contains(&y))
        .collect();
    assert_eq!(covered(&p, 255), ring);
    assert_eq!(covered(&p, 1), ring);
}

#[test]
fn clips_cut_exactly_on_whole_pixels_and_nest_by_intersection() {
    let mut scene = Scene::new();
    scene.clipped(Rect::new(2.0, 2.0, 4.0, 4.0), |s| {
        s.clipped(Rect::new(4.0, 0.0, 10.0, 10.0), |s| {
            s.fill(Rect::new(0.0, 0.0, 8.0, 8.0), Color::WHITE);
        });
        // After the inner clip ends, only the outer one applies.
        s.fill(Rect::new(0.0, 0.0, 3.0, 3.0), Color::BLACK);
    });
    let p = render(&scene, 8, 8, 1.0);
    let mut expected = block(4, 2, 6, 6);
    expected.push((2, 2));
    expected.sort_by_key(|&(x, y)| (y, x));
    assert_eq!(covered(&p, 255), expected);
    assert_eq!(covered(&p, 1), expected);
    assert_eq!(
        p.pixel(2, 2).map(PremulColor::to_array),
        Some([0, 0, 0, 255])
    );
}

#[test]
fn a_layer_blends_its_group_once() {
    let draw = |scene: &mut Scene| {
        scene.fill(Rect::new(0.0, 0.0, 3.0, 1.0), Color::WHITE);
        scene.fill(Rect::new(1.0, 0.0, 3.0, 1.0), Color::WHITE);
    };
    let mut grouped = Scene::new();
    grouped.layer(Layer::with_opacity(0.5), draw);
    let p = render(&grouped, 4, 1, 1.0);
    // Every covered pixel is the same half-transparent white, overlap or
    // not: the group was drawn opaque, then composited once at 0.5.
    let alphas: Vec<u8> = (0..4).map(|x| alpha(&p, x, 0)).collect();
    assert!((127..=128).contains(&alphas[0]), "{alphas:?}");
    assert_eq!(alphas[..3], [alphas[0]; 3]);
    assert_eq!(alphas[3], alphas[0]);

    // Without a layer, the same colour at half alpha is darker where the
    // two rectangles overlap.
    let mut flat = Scene::new();
    flat.fill(
        Rect::new(0.0, 0.0, 3.0, 1.0),
        Color::WHITE.with_opacity(0.5),
    );
    flat.fill(
        Rect::new(1.0, 0.0, 3.0, 1.0),
        Color::WHITE.with_opacity(0.5),
    );
    let p = render(&flat, 4, 1, 1.0);
    assert!(alpha(&p, 1, 0) > alpha(&p, 0, 0) + 60);

    // Opacity 0 draws nothing, opacity 1 draws as if there were no layer.
    let mut hidden = Scene::new();
    hidden.layer(Layer::with_opacity(0.0), draw);
    assert_eq!(covered(&render(&hidden, 4, 1, 1.0), 1), vec![]);
    let mut opaque = Scene::new();
    opaque.layer(Layer::with_opacity(1.0), draw);
    let mut direct = Scene::new();
    draw(&mut direct);
    assert_eq!(render(&opaque, 4, 1, 1.0), render(&direct, 4, 1, 1.0));
}

#[test]
fn columns_cover_exact_fractions_of_their_pixels() {
    let mut scene = Scene::new();
    // A four-column meter, ten pixels high, levels from the bottom.
    scene.columns(
        Rect::new(0.0, 0.0, 4.0, 10.0),
        &[0.0, 0.0, 0.0, 0.0],
        &[1.0, 0.5, 0.25, 0.05],
        Color::WHITE,
    );
    let p = render(&scene, 4, 10, 1.0);
    let column = |x: u32| (0..10).map(|y| alpha(&p, x, y)).collect::<Vec<u8>>();
    assert_eq!(column(0), [255; 10]);
    assert_eq!(column(1), [0, 0, 0, 0, 0, 255, 255, 255, 255, 255]);
    // Level 0.25 ends at 7.5: row 7 is half covered (128 of 255).
    assert_eq!(column(2), [0, 0, 0, 0, 0, 0, 0, 128, 255, 255]);
    // Level 0.05 ends at 9.5.
    assert_eq!(column(3), [0, 0, 0, 0, 0, 0, 0, 0, 0, 128]);
    // A half-covered pixel of opaque white is premultiplied white at 128.
    assert_eq!(p.pixel(2, 7).map(PremulColor::to_array), Some([128; 4]));
}

#[test]
fn columns_anti_alias_fractional_left_and_right_edges() {
    let mut scene = Scene::new();
    scene.columns(Rect::new(0.5, 0.0, 2.0, 1.0), &[0.0], &[1.0], Color::WHITE);
    let p = render(&scene, 3, 1, 1.0);
    assert_eq!(
        [alpha(&p, 0, 0), alpha(&p, 1, 0), alpha(&p, 2, 0)],
        [128, 255, 128]
    );
}

#[test]
fn many_entries_per_column_draw_their_envelope() {
    // 50,000 entries across 100 device columns: 500 per column.
    let n = 50_000usize;
    let low: Vec<f32> = (0..n)
        .map(|i| 0.5 - 0.4 * ((i * 7919 % 1000) as f32 / 1000.0))
        .collect();
    let high: Vec<f32> = (0..n)
        .map(|i| 0.5 + 0.4 * ((i * 104_729 % 1000) as f32 / 1000.0))
        .collect();
    let envelope_low: Vec<f32> = low
        .chunks(500)
        .map(|c| c.iter().copied().fold(1.0, f32::min))
        .collect();
    let envelope_high: Vec<f32> = high
        .chunks(500)
        .map(|c| c.iter().copied().fold(0.0, f32::max))
        .collect();
    let area = Rect::new(0.0, 0.0, 50.0, 30.0);
    let mut dense = Scene::new();
    dense.columns(area, &low, &high, Color::WHITE);
    let mut reduced = Scene::new();
    reduced.columns(area, &envelope_low, &envelope_high, Color::WHITE);
    // At 2x, 50 logical pixels are 100 device columns.
    assert_eq!(render(&dense, 100, 60, 2.0), render(&reduced, 100, 60, 2.0));
}

#[test]
fn columns_take_any_paint_and_follow_clips() {
    let stops = [Stop::new(0.0, Color::BLACK), Stop::new(1.0, Color::WHITE)];
    let gradient = LinearGradient {
        start: Point::new(0.0, 0.0),
        end: Point::new(0.0, 8.0),
        stops: &stops,
        extend: Extend::Pad,
    };
    let mut scene = Scene::new();
    scene.clipped(Rect::new(0.0, 0.0, 2.0, 8.0), |s| {
        s.columns(
            Rect::new(0.0, 0.0, 4.0, 8.0),
            &[0.0; 4],
            &[1.0; 4],
            gradient,
        );
    });
    let p = render(&scene, 4, 8, 1.0);
    assert_eq!(covered(&p, 1), block(0, 0, 2, 8));
    // The gradient runs top to bottom, so red grows down the column.
    let reds: Vec<u8> = (0..8)
        .map(|y| p.pixel(0, y).expect("inside").red())
        .collect();
    assert!(reds.windows(2).all(|w| w[0] < w[1]), "{reds:?}");
}

#[test]
fn nearest_sampling_copies_image_pixels_exactly() {
    let colors = [
        Color::rgb(255, 0, 0),
        Color::rgb(0, 255, 0),
        Color::rgb(0, 0, 255),
        Color::rgba(255, 255, 255, 128),
    ];
    let mut source = Pixmap::new(2, 2).expect("small");
    for (i, color) in colors.iter().enumerate() {
        source.set_pixel(i as u32 % 2, i as u32 / 2, color.premultiply());
    }
    let image = Image::new(source);
    let mut scene = Scene::new();
    scene.image(&image, Rect::new(0.0, 0.0, 4.0, 4.0), Sampling::Nearest);
    let p = render(&scene, 4, 4, 1.0);
    for y in 0..4 {
        for x in 0..4 {
            let expected = colors[(y / 2 * 2 + x / 2) as usize].premultiply();
            assert_eq!(p.pixel(x, y), Some(expected), "({x}, {y})");
        }
    }
}

#[test]
fn a_whole_pixel_translation_moves_pixels_without_changing_them() {
    let draw = |s: &mut Scene| {
        s.fill(Circle::new(Point::new(5.0, 5.0), 3.7), Color::hex(0xFF7A00));
        s.stroke(
            Arc::new(Point::new(5.0, 5.0), 4.5, 0.3, 2.0),
            Stroke::round(1.5),
            Color::WHITE.with_opacity(0.7),
        );
    };
    let mut here = Scene::new();
    draw(&mut here);
    let mut moved = Scene::new();
    moved.transformed(Transform::translate(3.0, 2.0), draw);
    let a = render(&here, 16, 16, 1.0);
    let b = render(&moved, 16, 16, 1.0);
    for y in 0..12 {
        for x in 0..12 {
            assert_eq!(a.pixel(x, y), b.pixel(x + 3, y + 2), "({x}, {y})");
        }
    }
}

#[test]
fn a_quarter_turn_puts_a_rectangle_where_it_belongs() {
    // A 4 by 2 rectangle turned a quarter turn clockwise about the origin,
    // then moved right by 4: it covers x 2..4, y 0..4.
    let mut scene = Scene::new();
    scene.transformed(
        Transform::rotate(std::f32::consts::FRAC_PI_2).then(Transform::translate(4.0, 0.0)),
        |s| s.fill(Rect::new(0.0, 0.0, 4.0, 2.0), Color::WHITE),
    );
    let p = render(&scene, 6, 6, 1.0);
    assert_eq!(covered(&p, 250), block(2, 0, 4, 4));
    assert_eq!(covered(&p, 6), block(2, 0, 4, 4));
}

#[test]
fn a_linear_gradient_runs_from_start_to_end_and_pads_beyond() {
    let stops = [Stop::new(0.0, Color::BLACK), Stop::new(1.0, Color::WHITE)];
    let mut scene = Scene::new();
    scene.fill(
        Rect::new(0.0, 0.0, 300.0, 1.0),
        LinearGradient {
            start: Point::new(22.0, 0.0),
            end: Point::new(277.0, 0.0),
            stops: &stops,
            extend: Extend::Pad,
        },
    );
    let p = render(&scene, 300, 1, 1.0);
    let red = |x: u32| i32::from(p.pixel(x, 0).expect("inside").red());
    // Pixel centre x + 0.5 lies (x + 0.5 - 22) / 255 of the way along.
    for x in 22..277 {
        let ideal = f64::from(x) + 0.5 - 22.0;
        assert!((f64::from(red(x)) - ideal).abs() <= 1.0, "{x}: {}", red(x));
    }
    assert_eq!(red(0), 0);
    assert_eq!(red(299), 255);
}

#[test]
fn rounded_rectangles_keep_their_corners_clear() {
    let mut scene = Scene::new();
    scene.fill(
        RoundedRect::new(Rect::new(0.0, 0.0, 20.0, 20.0), 6.0),
        Color::WHITE,
    );
    let p = render(&scene, 20, 20, 1.0);
    for (x, y) in [(0, 0), (19, 0), (0, 19), (19, 19)] {
        assert_eq!(alpha(&p, x, y), 0, "corner ({x}, {y})");
    }
    for (x, y) in [(10, 0), (0, 10), (6, 6), (10, 19)] {
        assert_eq!(alpha(&p, x, y), 255, "edge ({x}, {y})");
    }
}

#[test]
fn even_odd_paths_make_holes_and_non_zero_paths_do_not() {
    let mut path = Path::new();
    for inset in [0.0, 2.0] {
        path.move_to(Point::new(inset, inset))
            .line_to(Point::new(8.0 - inset, inset))
            .line_to(Point::new(8.0 - inset, 8.0 - inset))
            .line_to(Point::new(inset, 8.0 - inset))
            .close();
    }
    let mut scene = Scene::new();
    scene.fill(&path, Color::WHITE);
    assert_eq!(alpha(&render(&scene, 8, 8, 1.0), 4, 4), 255);
    path.set_fill_rule(manguebyte_gfx::FillRule::EvenOdd);
    let mut scene = Scene::new();
    scene.fill(&path, Color::WHITE);
    let p = render(&scene, 8, 8, 1.0);
    assert_eq!(alpha(&p, 4, 4), 0);
    assert_eq!(alpha(&p, 1, 4), 255);
}

#[test]
fn bad_scales_and_empty_targets_draw_nothing() {
    let mut scene = Scene::new();
    scene.fill(Rect::new(0.0, 0.0, 4.0, 4.0), Color::WHITE);
    for scale in [0.0, -1.0, f32::NAN, f32::INFINITY] {
        assert_eq!(covered(&render(&scene, 4, 4, scale), 1), vec![], "{scale}");
    }
    let mut empty = Pixmap::new(0, 5).expect("empty is allowed");
    Renderer::new().render(&scene, &mut empty, 1.0);
    assert!(empty.is_empty());
}

/// A scene with every kind of command, half-transparent where possible,
/// so that drawing any pixel twice would show.
fn busy_scene() -> Scene {
    let stops = [
        Stop::new(0.0, Color::rgba(255, 0, 0, 200)),
        Stop::new(0.6, Color::rgba(0, 0, 255, 90)),
        Stop::new(1.0, Color::rgba(0, 255, 0, 255)),
    ];
    let mut checker = Pixmap::new(4, 4).expect("small");
    for y in 0..4 {
        for x in 0..4 {
            let c = if (x + y) % 2 == 0 {
                Color::rgba(255, 255, 0, 160)
            } else {
                Color::rgba(0, 0, 0, 60)
            };
            checker.set_pixel(x, y, c.premultiply());
        }
    }
    let image = Image::new(checker);
    let mut wave = Path::new();
    wave.move_to(Point::new(1.3, 20.0));
    for i in 0..40 {
        let x = 1.3 + i as f32 * 0.9;
        wave.line_to(Point::new(x, 20.0 + ((i * 37) % 11) as f32 - 5.0));
    }
    let mut scene = Scene::new();
    scene.fill(
        Rect::new(0.0, 0.0, 40.0, 30.0),
        LinearGradient {
            start: Point::new(0.0, 0.0),
            end: Point::new(37.0, 21.0),
            stops: &stops,
            extend: Extend::Reflect,
        },
    );
    scene.stroke(&wave, Stroke::round(1.7), Color::rgba(255, 255, 255, 170));
    scene.clipped(
        RoundedRect::new(Rect::new(3.3, 2.1, 30.0, 20.0), 5.5),
        |s| {
            s.layer(Layer::with_opacity(0.6), |s| {
                s.fill(
                    Circle::new(Point::new(12.0, 10.0), 7.3),
                    Color::rgba(0, 200, 255, 220),
                );
                s.fill(
                    Circle::new(Point::new(17.0, 12.0), 6.1),
                    Color::rgba(255, 0, 200, 220),
                );
            });
            s.image(&image, Rect::new(20.5, 3.5, 9.0, 9.0), Sampling::Bilinear);
        },
    );
    scene.transformed(Transform::rotate_about(0.4, Point::new(30.0, 22.0)), |s| {
        s.stroke(
            Arc::new(Point::new(30.0, 22.0), 5.0, -2.0, 4.5),
            Stroke::new(1.3),
            Color::rgba(250, 250, 250, 140),
        );
    });
    let low: Vec<f32> = (0..23).map(|i| ((i * 7) % 5) as f32 / 20.0).collect();
    let high: Vec<f32> = (0..23).map(|i| 0.4 + ((i * 3) % 7) as f32 / 12.0).collect();
    scene.columns(
        Rect::new(2.25, 18.5, 35.5, 10.0),
        &low,
        &high,
        Color::rgba(120, 255, 120, 150),
    );
    scene
}

/// The largest channel difference between two pixmaps inside `damage`,
/// and the number of pixels that differ there.
fn difference_inside(a: &Pixmap, b: &Pixmap, damage: &[IRect]) -> (u8, usize, usize) {
    let (mut worst, mut differing, mut inside) = (0u8, 0, 0);
    for y in 0..a.height() {
        for x in 0..a.width() {
            if !damage
                .iter()
                .any(|r| r.contains(i64::from(x), i64::from(y)))
            {
                continue;
            }
            inside += 1;
            let p = a.pixel(x, y).expect("valid").to_array();
            let q = b.pixel(x, y).expect("valid").to_array();
            let d = p
                .iter()
                .zip(q)
                .map(|(m, n)| m.abs_diff(n))
                .max()
                .unwrap_or(0);
            worst = worst.max(d);
            if d > 0 {
                differing += 1;
            }
        }
    }
    (worst, differing, inside)
}

#[test]
fn damage_leaves_every_other_pixel_untouched_and_draws_the_rest_as_a_whole_render() {
    // The invariant partial repaint rests on (ADR 0007): rendering only the
    // damage equals rendering the whole target and keeping the damage.
    let scene = busy_scene();
    for scale in [1.0f32, 1.25, 1.5, 2.0] {
        let (w, h) = ((40.0 * scale) as u32, (30.0 * scale) as u32);
        let damage = [
            IRect::new(3, 4, 17, 9),
            IRect::new(10, 8, 20, 30),
            IRect::new(-5, h as i32 - 6, 9, 50),
            IRect::new(w as i32 - 3, 0, 10, 10),
        ];
        let mut partial = sentinel(w, h);
        Renderer::new().render_damage(&scene, &mut partial, scale, &damage);
        let untouched = sentinel(w, h);
        let mut changed_inside = 0;
        for y in 0..h {
            for x in 0..w {
                let damaged = damage
                    .iter()
                    .any(|r| r.contains(i64::from(x), i64::from(y)));
                let same = partial.pixel(x, y) == untouched.pixel(x, y);
                if damaged {
                    changed_inside += usize::from(!same);
                } else {
                    assert!(same, "({x}, {y}) at {scale}x changed outside the damage");
                }
            }
        }
        // The scene covers the damage, so the damage was really drawn.
        assert!(changed_inside > 300, "{changed_inside} at {scale}x");

        // Inside, every pixel is the whole render's, to the bit: edges that
        // cross the damage's border, gradients, the filtered image, the
        // clip and the layer included.
        let mut full = sentinel(w, h);
        Renderer::new().render(&scene, &mut full, scale);
        let (worst, differing, inside) = difference_inside(&partial, &full, &damage);
        assert_eq!(
            (worst, differing),
            (0, 0),
            "at {scale}x, {differing} of {inside} pixels differ by up to {worst}"
        );
    }
}

#[test]
fn an_unrotated_rectangle_crossing_the_damage_is_drawn_as_in_a_whole_render() {
    // A translucent rectangle whose top edge lies above the damage and whose
    // left edge lies inside it. Clipped to the damage, the rasteriser would
    // fill a rectangle one pixel high, a case it rounds one level lower.
    let mut scene = Scene::new();
    scene.fill(
        Rect::new(3.739_994, 34.291_187, 26.458_641, 28.062_037),
        Color::rgba(200, 100, 50, 180),
    );
    for damage in [
        IRect::new(1, 45, 4, 9),
        IRect::new(4, 42, 1, 1),
        IRect::new(4, 63, 40, 1),
        IRect::new(0, 0, 64, 64),
    ] {
        let mut full = sentinel(50, 80);
        Renderer::new().render(&scene, &mut full, 1.25);
        let mut partial = sentinel(50, 80);
        Renderer::new().render_damage(&scene, &mut partial, 1.25, &[damage]);
        let (worst, differing, _) = difference_inside(&full, &partial, &[damage]);
        assert_eq!((worst, differing), (0, 0), "{damage:?}");
    }
}

#[test]
fn a_shape_inside_or_across_the_damage_is_drawn_as_in_a_whole_render() {
    let stops = [
        Stop::new(0.0, Color::rgba(255, 0, 0, 200)),
        Stop::new(1.0, Color::rgba(0, 90, 255, 120)),
    ];
    let mut star = Path::new();
    star.move_to(Point::new(20.0, 3.3));
    for i in 1..10 {
        let angle = i as f32 * std::f32::consts::PI / 5.0;
        let r = if i % 2 == 0 { 9.7 } else { 4.1 };
        star.line_to(Point::new(20.0 + r * angle.sin(), 13.0 - r * angle.cos()));
    }
    star.close();
    let arc = Arc::new(Point::new(20.0, 15.0), 9.0, 2.3, 4.8);
    let gradient = LinearGradient {
        start: Point::new(10.0, 5.0),
        end: Point::new(30.0, 25.0),
        stops: &stops,
        extend: Extend::Pad,
    };
    type Draw<'a> = Box<dyn Fn(&mut Scene) + 'a>;
    let cases: Vec<Draw<'_>> = vec![
        Box::new(|s| {
            s.fill(
                Circle::new(Point::new(12.3, 10.1), 7.3),
                Color::rgba(0, 200, 255, 220),
            )
        }),
        Box::new(|s| s.stroke(arc, Stroke::round(2.6), Color::rgba(255, 0, 0, 200))),
        Box::new(|s| s.stroke(arc, Stroke::round(2.6), gradient)),
        Box::new(|s| s.fill(&star, Color::rgba(255, 255, 255, 180))),
        Box::new(|s| {
            s.stroke(
                &star,
                Stroke::new(0.8).with_dash(manguebyte_gfx::Dash::new(2.0, 1.5)),
                Color::BLACK,
            )
        }),
        Box::new(|s| {
            s.transformed(Transform::rotate_about(0.7, Point::new(20.0, 15.0)), |s| {
                s.clipped(
                    RoundedRect::new(Rect::new(12.0, 8.0, 16.0, 14.0), 4.5),
                    |s| {
                        s.layer(Layer::with_opacity(0.5), |s| {
                            s.fill(Rect::new(0.0, 0.0, 40.0, 30.0), Color::rgb(250, 120, 0));
                        });
                    },
                );
            })
        }),
    ];
    for (i, draw) in cases.iter().enumerate() {
        let mut scene = Scene::new();
        draw(&mut scene);
        let bounds = scene.bounds().expect("draws something");
        for scale in [1.0f32, 1.25, 1.5, 2.0] {
            let (w, h) = ((40.0 * scale) as u32, (30.0 * scale) as u32);
            let device = bounds.to_device(scale);
            let (cx, cy) = (
                device.x + device.width as i32 / 2,
                device.y + device.height as i32 / 2,
            );
            // The scene's bounds exactly, then rectangles that cut through
            // the shape: its left half, a band across its middle, a single
            // pixel, and a column.
            let damages = [
                device,
                IRect::new(device.x, device.y, device.width / 2, device.height),
                IRect::new(0, cy - 2, w, 5),
                IRect::new(cx, cy, 1, 1),
                IRect::new(cx - 1, 0, 3, h),
            ];
            let mut full = sentinel(w, h);
            Renderer::new().render(&scene, &mut full, scale);
            for damage in damages {
                let mut partial = sentinel(w, h);
                Renderer::new().render_damage(&scene, &mut partial, scale, &[damage]);
                let (worst, differing, _) = difference_inside(&full, &partial, &[damage]);
                assert_eq!(
                    (worst, differing),
                    (0, 0),
                    "case {i} at {scale}x, damage {damage:?}"
                );
            }
        }
    }
}

#[test]
fn a_clip_on_whole_pixels_equals_cropping_a_whole_render() {
    // The other half of the invariant: a scene drawn inside a clip that
    // falls on pixel boundaries is, inside the clip, the scene drawn
    // without it.
    let scene = busy_scene();
    let mut checked = 0;
    for scale in [1.0f32, 1.25, 1.5, 2.0] {
        let (w, h) = ((40.0 * scale) as u32, (30.0 * scale) as u32);
        let mut whole = sentinel(w, h);
        Renderer::new().render(&scene, &mut whole, scale);
        for area in [
            IRect::new(3, 4, 17, 9),
            IRect::new(0, 0, w / 2, h),
            IRect::new(7, 7, 1, 1),
        ] {
            let mut clipped = Scene::new();
            let logical = Rect::new(
                area.x as f32 / scale,
                area.y as f32 / scale,
                area.width as f32 / scale,
                area.height as f32 / scale,
            );
            // Only scales whose division is exact give pixel boundaries.
            if logical.to_device(scale) != area {
                continue;
            }
            clipped.clipped(logical, |s| s.append(&scene));
            let mut drawn = sentinel(w, h);
            Renderer::new().render(&clipped, &mut drawn, scale);
            let (worst, differing, _) = difference_inside(&whole, &drawn, &[area]);
            assert_eq!((worst, differing), (0, 0), "at {scale}x inside {area:?}");
            // Outside the clip nothing was drawn.
            let (_, outside, _) =
                difference_inside(&drawn, &sentinel(w, h), &[IRect::new(0, 0, w, h)]);
            let (_, inside, _) = difference_inside(&drawn, &sentinel(w, h), &[area]);
            assert_eq!(outside, inside, "at {scale}x around {area:?}");
            checked += 1;
        }
    }
    // On this scene and these scales every clip falls on whole pixels.
    assert_eq!(checked, 12);
}

#[test]
fn overlapping_damage_is_drawn_once() {
    // Half-transparent rectangles, columns and an image: drawing any pixel
    // twice would darken it.
    let mut scene = Scene::new();
    scene.fill(Rect::new(0.0, 0.0, 40.0, 30.0), Color::rgba(200, 0, 0, 100));
    scene.fill(Rect::new(4.5, 3.25, 30.0, 20.0), Color::rgba(0, 0, 200, 90));
    scene.columns(
        Rect::new(1.5, 10.0, 37.0, 18.0),
        &[0.1, 0.2, 0.0],
        &[0.9, 0.5, 0.7],
        Color::rgba(0, 255, 0, 120),
    );
    let mut source = Pixmap::new(2, 1).expect("small");
    source.set_pixel(0, 0, Color::rgba(255, 255, 0, 90).premultiply());
    source.set_pixel(1, 0, Color::rgba(0, 255, 255, 140).premultiply());
    scene.image(
        &Image::new(source),
        Rect::new(10.25, 2.0, 15.0, 9.0),
        Sampling::Bilinear,
    );
    let mut full = sentinel(40, 30);
    Renderer::new().render(&scene, &mut full, 1.0);
    let mut partial = sentinel(40, 30);
    // The whole target, as three overlapping rectangles.
    Renderer::new().render_damage(
        &scene,
        &mut partial,
        1.0,
        &[
            IRect::new(0, 0, 30, 30),
            IRect::new(10, 0, 30, 30),
            IRect::new(5, 5, 20, 20),
        ],
    );
    assert_eq!(partial, full);
}

#[test]
fn a_renderer_can_be_reused_across_sizes() {
    let scene = busy_scene();
    let mut renderer = Renderer::new();
    let mut fresh = Vec::new();
    for (w, h, scale) in [
        (40, 30, 1.0f32),
        (80, 60, 2.0),
        (20, 15, 0.5),
        (40, 30, 1.0),
    ] {
        let mut reused = Pixmap::new(w, h).expect("small");
        renderer.render(&scene, &mut reused, scale);
        let mut once = Pixmap::new(w, h).expect("small");
        Renderer::new().render(&scene, &mut once, scale);
        assert_eq!(reused, once, "{w}x{h}");
        fresh.push(once);
    }
    assert_eq!(fresh[0], fresh[3]);
}

#[test]
fn columns_fast_paths_draw_what_the_general_path_draws() {
    // One colour and gradients that run straight down or across are
    // blended by the columns code itself; any other gradient goes through a
    // coverage mask and the rasteriser's shader. A gradient a hair off
    // vertical takes the general path, so the two can be compared.
    let stops = [
        Stop::new(0.0, Color::rgba(255, 40, 0, 255)),
        Stop::new(1.0, Color::rgba(0, 120, 255, 160)),
    ];
    let same = [
        Stop::new(0.0, Color::rgba(30, 200, 90, 220)),
        Stop::new(1.0, Color::rgba(30, 200, 90, 220)),
    ];
    let low: Vec<f32> = (0..37).map(|i| ((i * 7) % 5) as f32 / 25.0).collect();
    let high: Vec<f32> = (0..37).map(|i| 0.3 + ((i * 3) % 7) as f32 / 10.0).collect();
    let area = Rect::new(1.5, 2.25, 36.0, 26.5);
    let draw = |paint: manguebyte_gfx::Paint<'_>| {
        let mut scene = Scene::new();
        scene.fill(
            Rect::new(0.0, 0.0, 40.0, 30.0),
            Color::rgba(20, 20, 40, 255),
        );
        scene.columns(area, &low, &high, paint);
        render(&scene, 80, 60, 2.0)
    };
    let pairs = [
        (
            draw(
                LinearGradient {
                    start: Point::new(5.0, 28.0),
                    end: Point::new(5.0, 2.0),
                    stops: &stops,
                    extend: Extend::Pad,
                }
                .into(),
            ),
            draw(
                LinearGradient {
                    start: Point::new(5.0, 28.0),
                    end: Point::new(5.0001, 2.0),
                    stops: &stops,
                    extend: Extend::Pad,
                }
                .into(),
            ),
        ),
        (
            draw(
                LinearGradient {
                    start: Point::new(2.0, 9.0),
                    end: Point::new(38.0, 9.0),
                    stops: &stops,
                    extend: Extend::Reflect,
                }
                .into(),
            ),
            draw(
                LinearGradient {
                    start: Point::new(2.0, 9.0),
                    end: Point::new(38.0, 9.0001),
                    stops: &stops,
                    extend: Extend::Reflect,
                }
                .into(),
            ),
        ),
        (
            draw(Color::rgba(30, 200, 90, 220).into()),
            draw(
                LinearGradient {
                    start: Point::new(0.0, 0.0),
                    end: Point::new(10.0, 10.0),
                    stops: &same,
                    extend: Extend::Pad,
                }
                .into(),
            ),
        ),
    ];
    let everything = [IRect::new(0, 0, 80, 60)];
    let mut background = Scene::new();
    background.fill(
        Rect::new(0.0, 0.0, 40.0, 30.0),
        Color::rgba(20, 20, 40, 255),
    );
    let background = render(&background, 80, 60, 2.0);
    for (i, (fast, general)) in pairs.iter().enumerate() {
        // Both really drew columns: hundreds of pixels changed.
        let (_, drawn, _) = difference_inside(fast, &background, &everything);
        assert!(drawn > 1_000, "pair {i}: {drawn}");
        let (worst, _, _) = difference_inside(fast, general, &everything);
        // The two blend with different rounding: at most two levels apart.
        assert!(worst <= 2, "pair {i}: {worst}");
    }
}

/// Opaque stops at 7 %, 27 % and 100 %, as a meter might use.
fn meter_stops() -> [Stop; 3] {
    [
        Stop::new(0.07, Color::hex(0x41D27A)),
        Stop::new(0.27, Color::hex(0xFFD23F)),
        Stop::new(1.0, Color::hex(0xFF3B30)),
    ]
}

#[test]
fn a_gradient_whose_ends_coincide_draws_one_colour() {
    // A gradient laid across a widget of no height has its start and end at
    // the same point. It has no direction: padded, it draws its last
    // colour; repeated or reflected, the average of its colours along its
    // length. (In `tiny-skia` that average is computed in `f32`; for these
    // offsets its alpha came to 1.0000001 and a check panicked.)
    let stops = meter_stops();
    // The average, from the stops: 65 * 0.07 + (65 + 255) / 2 * 0.2 +
    // (255 + 255) / 2 * 0.73 = 222.7 for red, 154.885 for green and
    // 67.555 for blue.
    let average = [223, 155, 68, 255];
    let last = [0xFF, 0x3B, 0x30, 0xFF];
    for (extend, expected) in [
        (Extend::Pad, last),
        (Extend::Repeat, average),
        (Extend::Reflect, average),
    ] {
        for end in [Point::new(4.0, 6.0), Point::new(4.0, 6.0 + 1.0 / 32_768.0)] {
            let gradient = LinearGradient {
                start: Point::new(4.0, 6.0),
                end,
                stops: &stops,
                extend,
            };
            let mut fill = Scene::new();
            fill.fill(Rect::new(0.0, 0.0, 8.0, 8.0), gradient);
            let p = render(&fill, 8, 8, 1.0);
            for (x, y) in block(0, 0, 8, 8) {
                assert_eq!(p.pixel(x, y).map(PremulColor::to_array), Some(expected));
            }
            // Columns take the same colour, in the bottom half they cover.
            let mut meter = Scene::new();
            meter.columns(Rect::new(0.0, 0.0, 8.0, 8.0), &[0.0], &[0.5], gradient);
            let p = render(&meter, 8, 8, 1.0);
            assert_eq!(covered(&p, 1), block(0, 4, 8, 8), "{extend:?}");
            assert_eq!(p.pixel(3, 6).map(PremulColor::to_array), Some(expected));
        }
    }
    // A radial gradient with a radius of almost nothing does the same.
    let mut scene = Scene::new();
    scene.fill(
        Rect::new(0.0, 0.0, 2.0, 2.0),
        manguebyte_gfx::RadialGradient {
            center: Point::new(1.0, 1.0),
            radius: 1e-7,
            stops: &stops,
            extend: Extend::Reflect,
        },
    );
    assert_eq!(
        render(&scene, 2, 2, 1.0)
            .pixel(0, 0)
            .map(PremulColor::to_array),
        Some(average)
    );
}

#[test]
fn a_bicubic_image_under_an_extreme_finite_transform_draws_nothing() {
    // Every coefficient is finite, so the transform is accepted, but the
    // image's corners map to x = inf - inf = NaN. Nothing can be drawn, and
    // the step that keeps bicubic results premultiplied must not run over
    // the rows it would compute from them.
    let mut source = Pixmap::new(2, 2).expect("small");
    source.fill(Color::rgb(200, 100, 50));
    let image = Image::new(source);
    let mut scene = Scene::new();
    scene.transformed(
        Transform::new(f32::MAX, 0.0, f32::MAX, -1.0, 0.0, 0.0),
        |s| s.image(&image, Rect::new(2.0, -10.0, 1.0, 1.0), Sampling::Bicubic),
    );
    for damage in [None, Some(IRect::new(0, 8, 16, 4))] {
        let mut pixels = sentinel(16, 16);
        match damage {
            None => Renderer::new().render(&scene, &mut pixels, 1.0),
            Some(d) => Renderer::new().render_damage(&scene, &mut pixels, 1.0, &[d]),
        }
        assert_eq!(pixels, sentinel(16, 16));
    }
}

/// The pixels with any coverage that lie outside `area`.
fn drawn_outside(pixels: &Pixmap, area: IRect) -> Vec<(u32, u32, u8)> {
    let mut outside = Vec::new();
    for y in 0..pixels.height() {
        for x in 0..pixels.width() {
            let a = alpha(pixels, x, y);
            if a > 0 && !area.contains(i64::from(x), i64::from(y)) {
                outside.push((x, y, a));
            }
        }
    }
    outside
}

#[test]
fn a_stroked_knob_arc_stays_inside_its_bounds() {
    // An ordinary knob: radius 12, a 2-pixel round stroke, half way, at 1x.
    // Its outline reaches 40 - 13 = 27 at its leftmost point. Stroked at
    // the device scale, the rasteriser's outline strayed a quarter of a
    // pixel beyond that and covered column 26 too; damage built from the
    // bounds then left a ghost behind when the knob moved. Strokes are now
    // computed at four times the scale, and the bounds count the
    // tolerance that remains: half a unit and 1/1024 of the reach.
    let mut scene = Scene::new();
    scene.stroke(
        Arc::new(
            Point::new(40.0, 40.0),
            12.0,
            0.75 * std::f32::consts::PI,
            0.75 * std::f32::consts::PI,
        ),
        Stroke::round(2.0),
        Color::WHITE,
    );
    let reach = 1.0 + 0.5 + 1.0 / 1024.0;
    let bounds = scene.bounds().expect("draws something");
    assert_eq!(
        bounds,
        Rect::new(
            28.0 - reach,
            28.0 - reach,
            24.0 + 2.0 * reach,
            24.0 + 2.0 * reach
        )
    );
    for scale in [1.0, 1.25, 1.5, 2.0] {
        let pixels = render(&scene, 120, 120, scale);
        let outside = drawn_outside(&pixels, bounds.to_device(scale));
        assert!(
            outside.is_empty(),
            "at {scale}x, (x, y, alpha) outside: {outside:?}"
        );
    }
}

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

#[test]
fn every_pixel_a_stroke_changes_lies_inside_the_scene_bounds() {
    // Unpadded: the bounds, mapped to device pixels and rounded out, must
    // hold every pixel with any coverage, at 0.5x to 3x. Knob arcs of many
    // radii, widths and sweeps, and random curved paths with every cap and
    // join, dashed or not, turned and stretched.
    use manguebyte_gfx::{Cap, Dash, Join};
    let mut r = Random(0x2545_F491_4F6C_DD1D);
    for case in 0..400 {
        let mut scene = Scene::new();
        let center = Point::new(30.0 + r.unit(), 30.0 + r.unit());
        if case % 2 == 0 {
            let radius = 4.0 + r.unit() * 20.0;
            let width = 0.5 + r.unit() * 6.0;
            let sweep = r.unit() * 1.5 * std::f32::consts::PI;
            scene.stroke(
                Arc::new(center, radius, 0.75 * std::f32::consts::PI, sweep),
                Stroke::round(width),
                Color::WHITE,
            );
        } else {
            let mut path = Path::new();
            let mut at = || Point::new(10.0 + r.unit() * 40.0, 10.0 + r.unit() * 40.0);
            path.move_to(at());
            for k in 0..4 {
                match k % 3 {
                    0 => path.quad_to(at(), at()),
                    1 => path.cubic_to(at(), at(), at()),
                    _ => path.line_to(at()),
                };
            }
            let mut stroke = Stroke::new(0.3 + r.unit() * 8.0)
                .with_cap([Cap::Butt, Cap::Round, Cap::Square][r.below(3)])
                .with_join([Join::Miter, Join::Round, Join::Bevel][r.below(3)])
                .with_miter_limit(1.0 + r.unit() * 6.0);
            if r.below(3) == 0 {
                stroke = stroke.with_dash(Dash::new(1.0 + r.unit() * 4.0, 1.0 + r.unit() * 3.0));
            }
            let turn = Transform::rotate_about(r.unit() * 6.3, center)
                .then(Transform::scale(0.6 + r.unit() * 0.8, 0.6 + r.unit() * 0.8));
            scene.transformed(turn, |s| s.stroke(&path, stroke, Color::WHITE));
        }
        let Some(bounds) = scene.bounds() else {
            continue;
        };
        for scale in [0.5, 1.0, 1.25, 1.5, 2.0, 3.0] {
            let pixels = render(&scene, 200, 200, scale);
            let outside = drawn_outside(&pixels, bounds.to_device(scale));
            assert!(
                outside.is_empty(),
                "case {case} at {scale}x, bounds {bounds:?}, outside: {outside:?}\n{}",
                scene.dump()
            );
        }
    }
}

/// The fastest of three runs.
fn fastest(mut work: impl FnMut()) -> std::time::Duration {
    (0..3)
        .map(|_| {
            let started = std::time::Instant::now();
            work();
            started.elapsed()
        })
        .min()
        .unwrap_or_default()
}

#[test]
fn sixteen_damaged_pixels_cost_far_less_than_a_whole_render() {
    // A pass skips every command that cannot reach its damage, so a
    // waveform stroked across the left half costs nothing when the damage
    // is at the right edge. (Each pass used to stroke and rasterise the
    // whole scene: sixteen pixels cost 60 to 70 % of a whole render.)
    let mut wave = Path::with_capacity(4_000);
    for i in 0..4_000 {
        let t = i as f32 * 0.37;
        let y = 50.0 + 40.0 * (2.0 * (t - t.floor()) - 1.0);
        wave.line_to(Point::new(i as f32 * 0.05, y));
    }
    let mut scene = Scene::new();
    scene.stroke(&wave, Stroke::new(1.0), Color::WHITE);
    let damage: Vec<IRect> = (0..16).map(|i| IRect::new(380 + i, 2 * i, 1, 1)).collect();
    let mut target = Pixmap::new(400, 100).expect("small");
    let mut renderer = Renderer::new();
    renderer.render(&scene, &mut target, 1.0);
    let whole = fastest(|| renderer.render(&scene, &mut target, 1.0));
    let damaged = fastest(|| renderer.render_damage(&scene, &mut target, 1.0, &damage));
    assert!(
        damaged * 4 < whole,
        "16 damaged pixels took {damaged:?}; all 40,000 pixels took {whole:?}"
    );
}

#[test]
fn gradients_mix_colours_with_straight_alpha() {
    // As documented: each channel, alpha included, is mixed on its own, as
    // Skia does by default. From transparent red to opaque blue, the middle
    // is half-transparent purple; CSS, which mixes premultiplied colours,
    // would show no red there.
    let stops = [
        Stop::new(0.0, Color::rgba(255, 0, 0, 0)),
        Stop::new(1.0, Color::rgba(0, 0, 255, 255)),
    ];
    let mut scene = Scene::new();
    scene.fill(
        Rect::new(0.0, 0.0, 256.0, 1.0),
        LinearGradient {
            start: Point::new(0.0, 0.0),
            end: Point::new(256.0, 0.0),
            stops: &stops,
            extend: Extend::Pad,
        },
    );
    let pixels = render(&scene, 256, 1, 1.0);
    // At the centre of pixel 128, t = 128.5 / 256: straight red 127, blue
    // and alpha 128, premultiplied 64, 0, 64, 128.
    let middle = pixels.pixel(128, 0).expect("inside").to_array();
    let expected = [64u8, 0, 64, 128];
    for (got, want) in middle.iter().zip(expected) {
        assert!(got.abs_diff(want) <= 1, "{middle:?}");
    }
}
