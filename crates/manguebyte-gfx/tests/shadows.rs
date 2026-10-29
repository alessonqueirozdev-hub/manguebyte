//! Shadows and glows: where they draw, that a partial render draws them as
//! a whole one does, and that their blurred masks are made once.

use std::f32::consts::PI;

use manguebyte_gfx::{
    Arc, Circle, Color, IRect, Layer, Path, Pixmap, Point, PremulColor, Rect, Renderer,
    RoundedRect, Scene, Shadow, Stroke, Transform,
};

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

/// Cards with soft shadows, a glowing arc and curve, a hard shadow, one in
/// a clip, one in a layer and one turned.
fn shadows() -> Scene {
    let mut scene = Scene::new();
    scene.fill(Rect::new(0.0, 0.0, 120.0, 90.0), Color::hex(0x101116));
    let card = RoundedRect::new(Rect::new(10.0, 8.0, 50.0, 34.0), 9.0);
    scene.shadow(
        card,
        Shadow::new(Color::BLACK.with_alpha(0xB3))
            .with_offset(0.0, 9.0)
            .with_blur(20.0)
            .with_spread(-6.0),
    );
    scene.fill(card, Color::hex(0x1C1F28));
    let accent = Color::hex(0xFF7A3D);
    let arc = Arc::new(Point::new(88.0, 26.0), 14.0, 0.75 * PI, 1.5 * PI * 0.6);
    scene.stroke_shadow(
        arc,
        Stroke::round(3.0),
        Shadow::glow(8.0, accent.with_alpha(0xB0)),
    );
    scene.stroke(arc, Stroke::round(3.0), accent);
    let mut curve = Path::new();
    curve
        .move_to(Point::new(8.0, 80.0))
        .line_to(Point::new(20.0, 56.0))
        .cubic_to(
            Point::new(26.0, 66.0),
            Point::new(32.0, 70.0),
            Point::new(42.0, 70.0),
        )
        .line_to(Point::new(56.0, 82.0));
    scene.stroke_shadow(&curve, Stroke::round(2.0), Shadow::glow(6.0, accent));
    scene.stroke(&curve, Stroke::round(2.0), Color::hex(0xFF9A5C));
    // A hard shadow, a glow grown by its spread, and a circle's glow.
    scene.shadow(
        Rect::new(66.0, 52.0, 14.0, 10.0),
        Shadow::new(Color::hex(0x2FD3C0)).with_offset(2.0, 2.0),
    );
    scene.fill(Rect::new(66.0, 52.0, 14.0, 10.0), Color::hex(0xE8E9EE));
    scene.shadow(
        Circle::new(Point::new(98.0, 70.0), 6.0),
        Shadow::glow(10.0, Color::hex(0x8B7BFF)).with_spread(2.0),
    );
    scene.fill(Circle::new(Point::new(98.0, 70.0), 6.0), Color::WHITE);
    // Under a clip, in a half-transparent layer, turned.
    scene.clipped(Rect::new(62.0, 64.0, 20.0, 26.0), |s| {
        s.layer(Layer::with_opacity(0.6), |s| {
            s.shadow(
                RoundedRect::new(Rect::new(66.0, 72.0, 12.0, 10.0), 3.0),
                Shadow::new(Color::hex(0xFF5C9A)).with_blur(6.0),
            );
        });
    });
    scene.transformed(Transform::rotate_about(0.3, Point::new(104.0, 50.0)), |s| {
        s.shadow(
            Rect::new(98.0, 46.0, 12.0, 8.0),
            Shadow::new(Color::BLACK.with_alpha(0x99)).with_blur(4.0),
        );
        s.fill(Rect::new(98.0, 46.0, 12.0, 8.0), Color::hex(0x242834));
    });
    scene
}

#[test]
fn a_shadow_never_draws_outside_the_scene_bounds() {
    let mut only = Scene::new();
    only.shadow(
        RoundedRect::new(Rect::new(30.0, 20.0, 40.0, 20.0), 6.0),
        Shadow::new(Color::BLACK)
            .with_offset(3.0, 7.0)
            .with_blur(18.0)
            .with_spread(4.0),
    );
    let arc = Arc::new(Point::new(50.0, 30.0), 12.0, 0.0, 4.0);
    only.stroke_shadow(arc, Stroke::round(4.0), Shadow::glow(9.0, Color::WHITE));
    let bounds = only.bounds().expect("draws");
    for scale in [0.5f32, 1.0, 1.25, 1.5, 2.0, 3.0] {
        let (w, h) = ((100.0 * scale) as u32, (80.0 * scale) as u32);
        let mut pixels = Pixmap::new(w, h).expect("small");
        Renderer::new().render(&only, &mut pixels, scale);
        let allowed = bounds.to_device(scale);
        let mut drawn = 0;
        for y in 0..h {
            for x in 0..w {
                if pixels.pixel(x, y).expect("inside").alpha() == 0 {
                    continue;
                }
                drawn += 1;
                assert!(
                    allowed.contains(i64::from(x), i64::from(y)),
                    "({x}, {y}) at {scale}x is outside {allowed:?}"
                );
            }
        }
        assert!(drawn > 100, "{drawn} pixels at {scale}x");
        // The blur's tail is cut by no more than a whisper: the pixels on
        // the edge of the bounds hold almost nothing.
        let edge = (0..w)
            .map(|x| (x, allowed.y.max(0) as u32))
            .chain((0..h).map(|y| (allowed.x.max(0) as u32, y)))
            .filter(|&(x, y)| x < w && y < h)
            .map(|(x, y)| pixels.pixel(x, y).expect("inside").alpha())
            .max()
            .unwrap_or(0);
        assert!(edge <= 6, "{edge} at the edge at {scale}x");
    }
}

#[test]
fn damage_through_a_shadow_draws_it_as_a_whole_render_does() {
    let scene = shadows();
    for scale in [1.0f32, 1.25, 2.0] {
        let (w, h) = ((120.0 * scale) as u32, (90.0 * scale) as u32);
        // Rectangles that cut the cards' shadows where they spill below
        // and beside the cards, the glows, and the clip.
        let damage = [
            IRect::new(0, (40.0 * scale) as i32, (30.0 * scale) as u32, 9),
            IRect::new((55.0 * scale) as i32, 0, 13, (60.0 * scale) as u32),
            IRect::new((80.0 * scale) as i32, (10.0 * scale) as i32, 7, 30),
            IRect::new((60.0 * scale) as i32, (66.0 * scale) as i32, 30, 21),
            IRect::new(3, (60.0 * scale) as i32, 40, 11),
        ];
        let mut whole = Pixmap::new(w, h).expect("small");
        Renderer::new().render(&scene, &mut whole, scale);
        let mut partial = sentinel(w, h);
        for rect in &damage {
            partial.fill_rect(*rect, Color::TRANSPARENT);
        }
        Renderer::new().render_damage(&scene, &mut partial, scale, &damage);
        let untouched = sentinel(w, h);
        for y in 0..h {
            for x in 0..w {
                let inside = damage
                    .iter()
                    .any(|r| r.contains(i64::from(x), i64::from(y)));
                let expected = if inside {
                    whole.pixel(x, y)
                } else {
                    untouched.pixel(x, y)
                };
                assert_eq!(partial.pixel(x, y), expected, "({x}, {y}) at {scale}x");
            }
        }
    }
}

#[test]
fn a_shadow_is_blurred_once_and_again_only_when_it_changes() {
    let card = |width: f32| RoundedRect::new(Rect::new(20.0, 20.0, width, 40.0), 12.0);
    let shadow = Shadow::new(Color::BLACK.with_alpha(0x99))
        .with_offset(0.0, 8.0)
        .with_blur(24.0);
    let draw = |scene: &mut Scene, at: Point, width: f32| {
        scene.clear();
        scene.transformed(Transform::translate(at.x, at.y), |s| {
            s.shadow(card(width), shadow);
            s.fill(card(width), Color::hex(0x1C1F28));
        });
    };
    let mut scene = Scene::new();
    let mut pixels = Pixmap::new(400, 300).expect("small");
    let mut renderer = Renderer::new();
    draw(&mut scene, Point::ZERO, 100.0);
    renderer.render(&scene, &mut pixels, 2.0);
    let first = pixels.clone();
    assert_eq!(renderer.shadow_cache().misses, 1);
    // The same frame again: drawn from the mask, to the same pixels.
    pixels = Pixmap::new(400, 300).expect("small");
    renderer.render(&scene, &mut pixels, 2.0);
    let stats = renderer.shadow_cache();
    assert_eq!((stats.misses, stats.hits, stats.masks), (1, 1, 1));
    assert_eq!(pixels, first);
    // A partial render of the same scene uses the mask too.
    renderer.render_damage(&scene, &mut pixels, 2.0, &[IRect::new(10, 10, 50, 300)]);
    assert_eq!(renderer.shadow_cache().misses, 1);
    // Moved by whole pixels: the same mask.
    draw(&mut scene, Point::new(7.0, 3.0), 100.0);
    renderer.render(&scene, &mut pixels, 2.0);
    assert_eq!(renderer.shadow_cache().misses, 1);
    // Resized, or drawn at another scale: blurred again.
    draw(&mut scene, Point::ZERO, 120.0);
    renderer.render(&scene, &mut pixels, 2.0);
    assert_eq!(renderer.shadow_cache().misses, 2);
    renderer.render(&scene, &mut pixels, 1.0);
    assert_eq!(renderer.shadow_cache().misses, 3);
    let stats = renderer.shadow_cache();
    assert!(stats.bytes <= stats.limit && stats.masks == 3);
    // With no budget nothing is kept, and every render blurs afresh.
    renderer.set_shadow_cache_limit(0);
    assert_eq!(renderer.shadow_cache().masks, 0);
    renderer.render(&scene, &mut pixels, 1.0);
    renderer.render(&scene, &mut pixels, 1.0);
    assert_eq!(renderer.shadow_cache().misses, 5);
}

#[test]
fn a_shadow_is_the_same_bit_for_bit_from_the_cache_and_without_it() {
    let scene = shadows();
    let mut cached = Pixmap::new(240, 180).expect("small");
    let mut renderer = Renderer::new();
    renderer.render(&scene, &mut cached, 2.0);
    renderer.render(&scene, &mut cached, 2.0);
    let mut fresh = Pixmap::new(240, 180).expect("small");
    let mut uncached = Renderer::new();
    uncached.set_shadow_cache_limit(0);
    uncached.render(&scene, &mut fresh, 2.0);
    // `cached` was drawn twice over itself: compare with one drawn twice.
    uncached.render(&scene, &mut fresh, 2.0);
    assert_eq!(cached, fresh);
}

#[test]
fn shadows_are_dumped_and_survive_appending() {
    let mut scene = Scene::new();
    scene.shadow(
        Rect::new(1.0, 2.0, 3.0, 4.0),
        Shadow::new(Color::BLACK).with_blur(2.5),
    );
    let mut joined = Scene::new();
    joined.fill(Rect::new(0.0, 0.0, 1.0, 1.0), Color::WHITE);
    joined.append(&scene);
    assert_eq!(
        joined.dump(),
        "fill rect(0 0 1 1) solid(#ffffffff)\n\
         shadow rect(1 2 3 4) offset 0 0 blur 2.5 spread 0 solid(#000000ff)\n"
    );
    // A transparent shadow, or one of nothing, draws nothing.
    let mut nothing = Scene::new();
    nothing.shadow(
        Rect::new(0.0, 0.0, 4.0, 4.0),
        Shadow::new(Color::TRANSPARENT),
    );
    nothing.shadow(Rect::new(0.0, 0.0, 0.0, 4.0), Shadow::new(Color::BLACK));
    assert_eq!(nothing.bounds(), None);
}

#[test]
fn shadows_match_their_golden_images_and_dump() {
    use manguebyte_gfx::snapshot::Snapshots;
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let failures = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("golden-failures");
    // Blurs are whole-number arithmetic, but the outlines under them come
    // from the rasteriser: the same tolerance as the other goldens.
    let snapshots = Snapshots::new(dir, failures).with_tolerance(1);
    let scene = shadows();
    let mut results = vec![snapshots.check_dump("shadows", &scene.dump())];
    for scale in [1u32, 2] {
        let mut pixels = Pixmap::new(120 * scale, 90 * scale).expect("small");
        Renderer::new().render(&scene, &mut pixels, scale as f32);
        results.push(snapshots.check_image(&format!("shadows@{scale}x"), &pixels));
    }
    let problems: Vec<String> = results
        .into_iter()
        .filter_map(|result| result.err().map(|mismatch| mismatch.to_string()))
        .collect();
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}
