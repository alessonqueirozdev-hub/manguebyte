//! The oscillator views: one cycle from samples or a wavetable's frame,
//! and a wavetable's frames stacked in depth with the frame playing lit,
//! drawn again only when the data or the position changes, and drawn
//! exactly as a whole repaint at every scale.

use std::f32::consts::TAU;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use manguebyte_core::AtomicF32;
use manguebyte_dsp::Wavetable;
use manguebyte_gfx::{IRect, Size};
use manguebyte_ui::testing::Harness;
use manguebyte_ui::{
    AtomicSource, IntoView, Role, State, TextSource, Theme, WaveData, label, row, wave_view,
    wavetable_view,
};

fn sine(len: usize, harmonic: f32) -> Vec<f32> {
    (0..len)
        .map(|i| (i as f32 / len as f32 * TAU * harmonic).sin())
        .collect()
}

/// Frames whose harmonic grows from 1 to `count`.
fn frames(count: usize) -> Vec<Vec<f32>> {
    (0..count).map(|k| sine(256, 1.0 + k as f32)).collect()
}

fn damage_inside(ui: &Harness, rect: IRect) -> bool {
    ui.last_damage()
        .iter()
        .all(|damage| rect.contains_rect(damage))
}

/// Whether the pixel at `(x, y)` is the accent's bright line: strongly
/// orange.
fn is_line(ui: &Harness, x: u32, y: u32) -> bool {
    ui.pixels()
        .pixel(x, y)
        .map(|p| p.to_array())
        .is_some_and(|[r, g, b, _]| r > 200 && g > 120 && b < 140)
}

#[test]
fn a_cycle_from_samples_is_drawn_where_its_samples_are() {
    for scale in [1.0, 1.25, 2.0] {
        let data = WaveData::from_cycle(&sine(480, 1.0));
        let mut ui = Harness::new(wave_view("Cycle", data), Size::new(200.0, 120.0), scale);
        ui.frame();
        let view = ui.get(Role::Image, "Cycle");
        assert_eq!(view.value_text(), Some("one cycle of 480 samples"));
        // The view keeps room for its glow: 6 pixels a side in the dark
        // theme. The line's peak is a quarter of the way along, at the
        // top; its trough three quarters along, at the bottom.
        let area = (6.0, 6.0, 188.0, 108.0);
        let at = |x: f32, y: f32| ((x * scale).round() as u32, (y * scale).round() as u32);
        for (fraction, value) in [
            (0.25, 1.0f32),
            (0.75, -1.0),
            (0.125, std::f32::consts::FRAC_1_SQRT_2),
        ] {
            let x = area.0 + area.2 * fraction;
            let y = area.1 + area.3 * 0.5 - value * (area.3 * 0.5 - 1.0 / scale);
            let (px, py) = at(x, y);
            let near = (py.saturating_sub(3)..py + 4).any(|y| is_line(&ui, px, y));
            assert!(near, "{scale}x: no line near ({x}, {y})");
        }
    }
}

#[test]
fn a_wavetable_s_frames_are_read_from_the_table() {
    let table = Wavetable::new(&frames(4)).expect("a table");
    let data = WaveData::from_wavetable(&table);
    assert_eq!((data.frame_count(), data.frame_len()), (4, 256));
    // A frame of one shown in a cycle view at a position.
    let position = State::new(0.0f32);
    let view = wave_view("Cycle", data.clone()).position(position.clone());
    let mut ui = Harness::new(view, Size::new(200.0, 120.0), 1.0);
    ui.frame();
    let first = ui.pixels().clone();
    position.set(3.0);
    assert_eq!(ui.frame().painted, 1);
    assert!(ui.pixels() != &first, "another frame looks different");
    assert_eq!(
        ui.get(Role::Image, "Cycle").value_text(),
        Some("frame 4.0 of 4, 256 samples each")
    );
    // Past the last frame, or not a number: held to the frames there are.
    position.set(99.0);
    ui.frame();
    let last = ui.pixels().clone();
    position.set(f32::NAN);
    ui.frame();
    assert!(ui.pixels() == &first);
    position.set(3.0);
    ui.frame();
    assert!(ui.pixels() == &last);
}

#[test]
fn the_stack_draws_again_only_when_the_data_or_the_position_changes() {
    for scale in [1.0, 1.25, 2.0] {
        let position = Arc::new(AtomicF32::new(0.0));
        let data = WaveData::from_frames(&frames(64));
        let view = row((
            label("Beside").width(60.0),
            wavetable_view(
                "Table",
                data.clone(),
                AtomicSource::new(Arc::clone(&position)),
            )
            .flex(1.0),
        ));
        let mut ui = Harness::new(view, Size::new(360.0, 180.0), scale);
        ui.frame();
        assert!(ui.pixels() == &ui.render_fresh(), "{scale}x at first");
        let table = ui.get(Role::Image, "Table");
        // Nothing changes, nothing is drawn.
        assert_eq!(ui.frames_rendered_during(|ui| ui.advance_ms(500)), 0);
        // The audio thread sweeps the position: each step draws the view
        // only, as a whole repaint would.
        for step in 1..=6 {
            position.store(step as f32 * 7.3, Ordering::Relaxed);
            let summary = ui.frame();
            assert_eq!(summary.painted, 1, "{scale}x step {step}");
            assert!(damage_inside(&ui, table.device), "{scale}x step {step}");
            assert!(ui.pixels() == &ui.render_fresh(), "{scale}x step {step}");
            assert_eq!(ui.frame().painted, 0, "{scale}x: once");
        }
        // New data, the same position: drawn again.
        data.set_frames(&frames(8));
        assert_eq!(ui.frame().painted, 1);
        assert!(ui.pixels() == &ui.render_fresh(), "{scale}x new data");
        assert_eq!(
            ui.get(Role::Image, "Table").value_text(),
            Some("frame 8.0 of 8, 256 samples each"),
            "the position is held to the frames there are"
        );
        assert_eq!(ui.frames_rendered_during(|ui| ui.advance_ms(200)), 0);
    }
}

#[test]
fn the_frame_playing_is_lit_and_moves_with_the_position() {
    let position = State::new(0.0f32);
    let view = wavetable_view(
        "Table",
        WaveData::from_frames(&frames(16)),
        position.clone(),
    );
    let mut ui = Harness::new(view, Size::new(300.0, 180.0), 1.0);
    ui.frame();
    // The lit line's lowest row: the front frame sits lowest, the back
    // frame highest.
    let lit_bottom = |ui: &Harness| {
        (0..180u32)
            .rev()
            .find(|&y| (0..300u32).any(|x| is_line(ui, x, y)))
            .expect("a lit line")
    };
    let front = lit_bottom(&ui);
    position.set(15.0);
    ui.frame();
    let back = lit_bottom(&ui);
    assert!(back + 30 < front, "front {front}, back {back}");
    position.set(7.5);
    ui.frame();
    let middle = lit_bottom(&ui);
    assert!(back < middle && middle < front, "{back} {middle} {front}");
}

#[test]
fn an_empty_view_draws_no_line_and_says_so() {
    let data = WaveData::new();
    let mut ui = Harness::new(
        row((
            wave_view("Cycle", data.clone()),
            wavetable_view("Table", data.clone(), || 0.0),
        )),
        Size::new(420.0, 160.0),
        1.0,
    );
    ui.frame();
    assert_eq!(
        ui.get(Role::Image, "Table").value_text(),
        Some("no waveform")
    );
    assert!((0..420).all(|x| (0..160).all(|y| !is_line(&ui, x, y))));
    ui.assert_accessible();
    data.set_cycle(&sine(64, 1.0));
    assert_eq!(ui.frame().painted, 2);
    assert!((0..420).any(|x| (0..160).any(|y| is_line(&ui, x, y))));
}

#[test]
fn the_views_follow_the_language_without_drawing() {
    let language = State::new(String::from("en"));
    let name = |texts: [&'static str; 2]| {
        let language = language.clone();
        TextSource::dynamic(move || {
            if language.with(|l| l == "pt") {
                texts[1]
            } else {
                texts[0]
            }
        })
    };
    let data = WaveData::from_frames(&frames(4));
    let view = row((
        wave_view(name(["Cycle", "Ciclo"]), data.clone()),
        wavetable_view(name(["Table", "Tabela"]), data, || 1.0),
    ));
    let mut ui = Harness::new(view, Size::new(420.0, 160.0), 2.0);
    ui.frame();
    language.set(String::from("pt"));
    let summary = ui.frame();
    assert_eq!(summary.painted, 0, "the names are not drawn");
    assert_eq!(ui.access_updates().len(), 2);
    assert!(ui.find(Role::Image, "Ciclo").is_some());
    assert!(ui.find(Role::Image, "Tabela").is_some());
}

#[test]
fn both_themes_draw_and_switch() {
    let data = WaveData::from_frames(&frames(8));
    let view = row((
        wave_view("Cycle", data.clone()).position(|| 2.0),
        wavetable_view("Table", data, || 2.0),
    ));
    let mut ui = Harness::new(view, Size::new(420.0, 160.0), 1.0);
    ui.frame();
    let dark = ui.pixels().clone();
    ui.set_theme(Theme::light());
    ui.frame();
    assert!(ui.pixels() != &dark);
    assert!(ui.pixels() == &ui.render_fresh());
}
