//! The waveform view: its summary, markers and their rules, snapping,
//! zoom and scrolling, selection, the keyboard, assistive technology, and
//! drawing that a partial repaint reproduces exactly at every scale.

use std::sync::Arc;

use manguebyte_gfx::{IRect, Point, Size};
use manguebyte_ui::testing::{Found, Harness};
use manguebyte_ui::{
    Action, IntoView, Key, Modifiers, Role, Waveform, WaveformEvent, WaveformState, label, row,
    waveform_view,
};

const SHIFT: Modifiers = Modifiers {
    shift: true,
    ..Modifiers::NONE
};

const CONTROL: Modifiers = Modifiers {
    control: true,
    ..Modifiers::NONE
};

/// A sine of `period` frames, `len` frames long.
fn sine(len: usize, period: f32) -> Vec<f32> {
    (0..len)
        .map(|i| (i as f32 * std::f32::consts::TAU / period).sin() * 0.8)
        .collect()
}

/// A view of `samples` at 48 kHz with the sampler's four markers.
fn sampler(samples: Vec<f32>, size: Size, scale: f32) -> (Harness, WaveformState) {
    let len = samples.len() as u64;
    let state = WaveformState::new();
    state.set_waveform(Some(Arc::new(
        Waveform::new(vec![samples], 48_000.0).named("Tone"),
    )));
    state.set_sample_points(0, len, Some(len / 4..len * 3 / 4));
    let mut ui = Harness::new(waveform_view("Sample", state.clone()), size, scale);
    ui.frame();
    (ui, state)
}

fn frame_of(state: &WaveformState, id: u32) -> u64 {
    state.marker(id).expect("a marker").frame
}

fn marker(ui: &Harness, name: &str) -> Found {
    ui.get(Role::Slider, name)
}

/// Where the marker named `name` is drawn: the middle of its item.
fn marker_x(ui: &Harness, name: &str) -> f32 {
    marker(ui, name).center().x
}

fn damage_inside(ui: &Harness, rect: IRect) -> bool {
    ui.last_damage()
        .iter()
        .all(|damage| rect.contains_rect(damage))
}

#[test]
fn the_summary_agrees_with_reading_every_sample_of_every_channel() {
    // Interleaved stereo, the right channel the left upside down and half
    // as loud.
    let left = sine(10_000, 333.0);
    let interleaved: Vec<f32> = left.iter().flat_map(|&s| [s, -0.5 * s]).collect();
    let wave = Waveform::from_interleaved(&interleaved, 2, 44_100.0);
    assert_eq!((wave.len(), wave.channel_count()), (10_000, 2));
    let scan = |channel: usize, from: usize, to: usize| {
        let samples = wave.channel(channel)[from..to].to_vec();
        samples
            .iter()
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(l, h), &s| {
                (l.min(s), h.max(s))
            })
    };
    for (from, to) in [
        (0, 10_000),
        (1, 9_999),
        (63, 65),
        (64, 128),
        (500, 7_777),
        (9_000, 9_999),
    ] {
        for channel in 0..2 {
            assert_eq!(
                wave.min_max(channel, from..to),
                Some(scan(channel, from, to)),
                "channel {channel}, {from}..{to}"
            );
        }
    }
    assert_eq!(wave.min_max(2, 0..10), None, "no third channel");
    assert_eq!(wave.min_max(0, 9_999..20_000), Some(scan(0, 9_999, 10_000)));
}

#[test]
fn a_ten_minute_file_draws_from_its_summary_without_reading_it_all() {
    let len = 48_000 * 600;
    let samples: Arc<[f32]> = (0..len)
        .map(|i| ((i % 4801) as f32 / 2400.0 - 1.0) * 0.9)
        .collect();
    let wave = Arc::new(Waveform::from_shared(vec![samples], 48_000.0).named("Long"));
    let state = WaveformState::new();
    state.set_waveform(Some(Arc::clone(&wave)));
    state.set_sample_points(0, len as u64, None);
    let mut ui = Harness::new(
        waveform_view("Sample", state.clone()),
        Size::new(1000.0, 120.0),
        2.0,
    );
    let before = wave.samples_read();
    ui.frame();
    let first = wave.samples_read() - before;
    // 2000 device columns, each at most 126 samples at its ends and two
    // entries a level in between: far below the 28.8 million frames.
    assert!(first > 0 && first < 2_000 * (128 + 2 * 20), "{first}");
    // The playhead moves every frame: nothing is read again.
    for frame in 0..10 {
        state.set_playhead(Some(frame * 48_000));
        let read = wave.samples_read();
        assert!(ui.frame().painted > 0);
        assert_eq!(wave.samples_read(), read, "the columns are kept");
    }
    // Zooming in reads what the new columns need, and no more.
    let at = ui.get(Role::Group, "Sample").center();
    ui.set_modifiers(CONTROL);
    let read = wave.samples_read();
    assert!(ui.wheel(at, 3.0));
    ui.frame();
    assert!(wave.samples_read() - read < 2_000 * (128 + 2 * 20));
}

#[test]
fn markers_drag_within_their_rules() {
    let len = 48_000;
    let (mut ui, state) = sampler(sine(len, 480.0), Size::new(480.0, 120.0), 1.0);
    // A hundred frames a pixel. The end, dragged far to the left, stops at
    // the loop end, which it may not pass.
    let end = marker(&ui, "End").center();
    ui.drag(end, (-400.0, 0.0), 8);
    assert_eq!(frame_of(&state, WaveformState::END), 36_000);
    // The loop end, dragged left of the loop start, stops a frame after it.
    let loop_end = marker(&ui, "Loop end").center();
    ui.drag(loop_end, (-300.0, 0.0), 5);
    assert_eq!(frame_of(&state, WaveformState::LOOP_END), 12_001);
    // The start, dragged right, stops at the loop start.
    let start = marker(&ui, "Start").center();
    ui.drag(start, (300.0, 0.0), 5);
    assert_eq!(frame_of(&state, WaveformState::START), 12_000);
    // The loop start can go no lower than the start, nor past the loop end.
    let loop_start = marker(&ui, "Loop start").center();
    ui.drag(loop_start, (-200.0, 0.0), 5);
    assert_eq!(frame_of(&state, WaveformState::LOOP_START), 12_000);
    ui.frame();
    let loop_start = marker(&ui, "Loop start").center();
    ui.drag(loop_start, (100.0, 0.0), 5);
    assert_eq!(frame_of(&state, WaveformState::LOOP_START), 12_000);
    // One event per drag that moved something, when it ended.
    assert_eq!(
        state.take_events(),
        [
            WaveformEvent::MarkerMoved {
                id: WaveformState::END,
                frame: 36_000
            },
            WaveformEvent::MarkerMoved {
                id: WaveformState::LOOP_END,
                frame: 12_001
            },
            WaveformEvent::MarkerMoved {
                id: WaveformState::START,
                frame: 12_000
            },
        ]
    );
}

#[test]
fn a_marker_follows_the_pointer_from_where_it_was_taken_and_escape_puts_it_back() {
    let (mut ui, state) = sampler(sine(48_000, 480.0), Size::new(480.0, 120.0), 1.0);
    let start = marker(&ui, "Start").center();
    ui.press(Point::new(start.x + 2.0, start.y));
    ui.move_to(Point::new(start.x + 52.0, start.y));
    assert_eq!(
        frame_of(&state, WaveformState::START),
        5_000,
        "no jump to the pointer"
    );
    ui.key(Key::Escape);
    assert_eq!(frame_of(&state, WaveformState::START), 0);
    ui.release(Point::new(start.x + 52.0, start.y));
    assert!(state.take_events().is_empty(), "nothing moved in the end");
}

#[test]
fn snapping_puts_a_dragged_marker_on_a_zero_crossing() {
    let samples = sine(48_000, 501.0);
    let (mut ui, state) = sampler(samples.clone(), Size::new(480.0, 120.0), 1.0);
    state.set_snap_to_zero(true);
    let start = marker(&ui, "Start").center();
    ui.drag(start, (77.3, 0.0), 3);
    let frame = frame_of(&state, WaveformState::START) as usize;
    assert!(
        (samples[frame - 1] < 0.0) != (samples[frame] < 0.0),
        "frame {frame} is no crossing"
    );
    // The nearest one: no other crossing is closer to where it was dropped.
    let wave = state.waveform().expect("a waveform");
    let unsnapped = 7_730;
    let nearest = wave.zero_crossing_near(unsnapped, 600).expect("a crossing");
    assert_eq!(frame as u64, nearest);
    // The keys go from crossing to crossing.
    assert!(ui.key(Key::Right));
    let next = frame_of(&state, WaveformState::START);
    assert_eq!(
        Some(next),
        wave.zero_crossing_beyond(frame as u64, true, 48_000)
    );
    assert!(ui.key(Key::Left));
    assert_eq!(frame_of(&state, WaveformState::START), frame as u64);
}

#[test]
fn ctrl_and_the_wheel_zoom_around_the_pointer() {
    let (mut ui, state) = sampler(sine(48_000, 480.0), Size::new(480.0, 120.0), 1.0);
    let group = ui.get(Role::Group, "Sample");
    let at = Point::new(120.0, group.center().y);
    let under = |state: &WaveformState| {
        let shown = state.shown();
        shown.start + (shown.end - shown.start) * f64::from(at.x / 480.0)
    };
    let before = under(&state);
    ui.set_modifiers(CONTROL);
    assert!(ui.wheel(at, 2.0));
    let shown = state.shown();
    assert!(
        (shown.end - shown.start - 48_000.0 / 2.25).abs() < 1e-6,
        "{shown:?}"
    );
    assert!(
        (under(&state) - before).abs() < 1e-6,
        "the frame under the pointer stays"
    );
    // A pinch, in fractions of a notch, does the same.
    let before = under(&state);
    assert!(ui.wheel(at, 0.37));
    assert!((under(&state) - before).abs() < 1e-6);
    // No further in than MAX_PIXELS_PER_FRAME, no further out than all.
    assert!(ui.wheel(at, 100.0));
    let shown = state.shown();
    assert!((shown.end - shown.start - 30.0).abs() < 1e-6, "{shown:?}");
    assert!(ui.wheel(at, -100.0));
    assert_eq!(state.shown(), 0.0..48_000.0);
    // Zoomed in, the wheel alone scrolls later and earlier; at an end, or
    // showing everything, it is left to whatever is around.
    ui.set_modifiers(Modifiers::NONE);
    assert!(!ui.wheel(at, -1.0), "nothing to scroll");
    ui.set_modifiers(CONTROL);
    ui.wheel(at, 4.0);
    ui.set_modifiers(Modifiers::NONE);
    let zoomed = state.shown();
    assert!(ui.wheel(at, -1.0));
    let later = state.shown();
    assert!(later.start > zoomed.start);
    assert!((later.end - later.start - (zoomed.end - zoomed.start)).abs() < 1e-6);
    assert!(ui.wheel_by(at, Point::new(-1.0, 0.0)), "sideways");
    assert!((state.shown().start - zoomed.start).abs() < 1e-6);
    while ui.wheel(at, 5.0) {}
    assert_eq!(state.shown().start, 0.0);
    // Through the state, the application zooms too.
    state.show(1_000.0..2_000.0);
    assert_eq!(state.shown(), 1_000.0..2_000.0);
    state.show_all();
    assert_eq!(state.shown(), 0.0..48_000.0);
}

#[test]
fn a_drag_selects_and_a_click_reports_where_and_clears_the_selection() {
    let (mut ui, state) = sampler(sine(48_000, 480.0), Size::new(480.0, 120.0), 1.0);
    let y = ui.get(Role::Group, "Sample").center().y;
    ui.drag(Point::new(150.0, y), (100.0, 0.0), 4);
    assert_eq!(state.selection(), Some(15_000..25_000));
    ui.drag(Point::new(300.0, y), (-50.0, 0.0), 2);
    assert_eq!(state.selection(), Some(25_000..30_000));
    ui.click(Point::new(320.0, y));
    assert_eq!(state.selection(), None);
    assert_eq!(
        state.take_events(),
        [
            WaveformEvent::Selected(Some(15_000..25_000)),
            WaveformEvent::Selected(Some(25_000..30_000)),
            WaveformEvent::Selected(None),
            WaveformEvent::Clicked(32_000),
        ]
    );
    ui.frame();
    let value = ui.get(Role::Group, "Sample").node.value.unwrap_or_default();
    assert!(
        value.starts_with("Tone, 1.00 s, 1 channel; showing 0.00 s to 1.00 s"),
        "{value}"
    );
}

#[test]
fn the_keyboard_chooses_and_moves_markers_and_zooms() {
    let (mut ui, state) = sampler(sine(48_000, 480.0), Size::new(480.0, 120.0), 1.0);
    assert!(ui.tab());
    assert!(ui.focused().is_some_and(|found| found.name() == "Sample"));
    // The keyboard starts on the first marker, the start.
    assert!(ui.key(Key::Right));
    assert_eq!(
        frame_of(&state, WaveformState::START),
        100,
        "a pixel's worth"
    );
    assert!(ui.key_with(Key::Right, SHIFT));
    assert_eq!(frame_of(&state, WaveformState::START), 101, "a frame");
    // Down goes to the next marker, the end; Home takes it as low as it may go.
    assert!(ui.key(Key::Down));
    assert!(ui.key(Key::Home));
    assert_eq!(frame_of(&state, WaveformState::END), 36_000);
    assert!(ui.key(Key::End));
    assert_eq!(frame_of(&state, WaveformState::END), 48_000);
    ui.frame();
    assert!(marker(&ui, "End").is_focused());
    // + zooms in around it, - back out.
    assert!(ui.key(Key::Character('=')));
    let shown = state.shown();
    assert!(shown.end - shown.start < 48_000.0);
    assert!(ui.key(Key::Character('-')));
    assert_eq!(state.shown(), 0.0..48_000.0);
    assert_eq!(state.take_events().len(), 4, "each key that moved a marker");
}

#[test]
fn assistive_technology_reads_and_sets_markers() {
    let (mut ui, state) = sampler(sine(48_000, 480.0), Size::new(480.0, 120.0), 1.0);
    ui.assert_accessible();
    let end = marker(&ui, "End");
    assert_eq!(end.value_text(), Some("1.000 s, frame 48000"));
    assert_eq!(end.numeric(), Some(48_000.0));
    let range = end.node.range.expect("a range");
    assert_eq!(
        (range.min, range.max, range.step),
        (0.0, 48_000.0, Some(1.0))
    );
    // Set below the loop end: as low as the rules allow.
    assert!(ui.act(&end, Action::SetValue(10.0)));
    assert_eq!(frame_of(&state, WaveformState::END), 36_000);
    let start = marker(&ui, "Start");
    assert!(ui.act(&start, Action::Increment));
    assert_eq!(frame_of(&state, WaveformState::START), 100);
    assert!(ui.act(&start, Action::Focus));
    ui.frame();
    assert!(marker(&ui, "Start").is_focused());
    assert!(ui.key(Key::Left));
    assert_eq!(frame_of(&state, WaveformState::START), 0);
}

#[test]
fn every_lane_shows_its_own_channel() {
    // The left channel silent, the right one loud.
    let state = WaveformState::new();
    state.set_waveform(Some(Arc::new(Waveform::new(
        vec![vec![0.0; 4_800], sine(4_800, 48.0)],
        48_000.0,
    ))));
    let mut ui = Harness::new(waveform_view("Stereo", state), Size::new(240.0, 120.0), 1.0);
    ui.frame();
    // The waveform is the accent, in a gradient: strongly red where it is
    // drawn, against a dark background.
    let is_accent = |x: u32, y: u32| {
        ui.pixels()
            .pixel(x, y)
            .is_some_and(|p| p.red() > 150 && p.red() > p.blue() + 60)
    };
    let group = ui.get(Role::Group, "Stereo");
    let top = group.bounds.y as u32;
    let height = group.bounds.height as u32;
    // A quarter of the way down the area: the top lane, silent, has no
    // waveform; the bottom lane, three quarters down, has.
    let strip = 15;
    let area = height - strip;
    let lane = |fraction: f32| top + strip + (area as f32 * fraction) as u32;
    assert!((10..230).all(|x| !is_accent(x, lane(0.2))));
    assert!((10..230).any(|x| is_accent(x, lane(0.65))));
    assert!(
        ui.get(Role::Group, "Stereo")
            .node
            .value
            .unwrap_or_default()
            .contains("2 channels")
    );
}

#[test]
fn marker_lines_are_whole_device_pixels_at_every_scale() {
    // Start and end markers are drawn in the text colour.
    let focus = manguebyte_ui::Theme::dark().palette.text;
    for scale in [1.0, 1.25, 2.0] {
        let (mut ui, state) = sampler(vec![0.0; 48_000], Size::new(480.0, 120.0), scale);
        state.set_marker_frame(WaveformState::END, 40_123);
        ui.frame();
        let x = (marker_x(&ui, "End") * scale).round() as u32;
        let y = (90.0 * scale) as u32;
        let is_focus = |x: u32| {
            ui.pixels().pixel(x, y).map(|p| p.to_array()) == Some([focus.r, focus.g, focus.b, 255])
        };
        let lit: Vec<u32> = (x.saturating_sub(3)..x + 3)
            .filter(|&x| is_focus(x))
            .collect();
        assert_eq!(lit.len(), 1, "{scale}x: one device column, solid: {lit:?}");
    }
}

#[test]
fn every_change_draws_only_the_view_and_equals_a_whole_repaint() {
    for scale in [1.0, 1.25, 2.0] {
        let state = WaveformState::new();
        state.set_waveform(Some(Arc::new(Waveform::new(
            vec![sine(96_000, 700.0), sine(96_000, 333.0)],
            48_000.0,
        ))));
        state.set_sample_points(100, 90_000, Some(20_000..60_000));
        let view = row((
            label("Beside").width(60.0),
            waveform_view("Sample", state.clone()).flex(1.0),
        ));
        let mut ui = Harness::new(view, Size::new(400.0, 140.0), scale);
        ui.frame();
        assert!(ui.pixels() == &ui.render_fresh(), "{scale}x at first");
        let found = ui.get(Role::Group, "Sample");
        let at = found.center();
        let check = |ui: &mut Harness, what: &str| {
            let summary = ui.frame();
            assert!(summary.painted > 0, "{scale}x {what}: nothing drawn");
            assert!(
                damage_inside(ui, found.device),
                "{scale}x {what}: {:?} outside {:?}",
                ui.last_damage(),
                found.device
            );
            assert!(ui.pixels() == &ui.render_fresh(), "{scale}x after {what}");
        };
        let loop_start = marker(&ui, "Loop start").center();
        ui.drag(loop_start, (23.7, 0.0), 3);
        check(&mut ui, "a marker drag");
        state.set_playhead(Some(33_333));
        check(&mut ui, "the playhead");
        ui.set_modifiers(CONTROL);
        ui.wheel(at, 1.3);
        check(&mut ui, "a zoom");
        ui.wheel(at, 12.0);
        check(&mut ui, "a zoom past a frame per pixel");
        ui.set_modifiers(Modifiers::NONE);
        ui.wheel(at, -0.6);
        check(&mut ui, "a scroll");
        ui.drag(Point::new(at.x - 30.0, at.y), (40.0, 0.0), 2);
        check(&mut ui, "a selection");
        // Nothing moving, nothing drawn.
        assert_eq!(ui.frames_rendered_during(|ui| ui.advance_ms(200)), 0);
    }
}

#[test]
fn an_empty_view_says_so_and_takes_no_wheel() {
    let state = WaveformState::new();
    let mut ui = Harness::new(
        waveform_view("Sample", state.clone()),
        Size::new(200.0, 80.0),
        1.0,
    );
    ui.frame();
    assert_eq!(
        ui.get(Role::Group, "Sample").value_text(),
        Some("no sample")
    );
    assert!(!ui.wheel(Point::new(100.0, 40.0), 1.0));
    ui.assert_accessible();
}
