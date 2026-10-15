//! Mangue Duck's editor, opened headless through the plugin harness and
//! `manguebyte-gui`, with a host that records every call (ADR 0007, phase
//! 1, item 9).
//!
//! The harness creates the plugin and takes its editor spec as a wrapper
//! does; the test opens it in a headless window and plays the person
//! (pointer and keys, in logical pixels) and the system (the frame timer)
//! through a `HeadlessEditor`. The host stores what the editor performs
//! into the parameters, as a standalone program does, so the harness hands
//! each edit to the processor as an event at the start of its next block.

mod common;

use mangue_duck::METER_HOLD;
use manguebyte::decibels::{db_to_gain, gain_to_db};
use manguebyte::dsp::event::EventKind;
use manguebyte::plugin::editor::testing::HostCall;
use manguebyte::plugin::editor::{HostSize, Key, KeyEvent, Modifiers};
use manguebyte_gfx::{Point, Rect};
use manguebyte_ui::Role;
use manguebyte_window::{Button, PhysicalPoint, PhysicalSize, RawInput};
use std::time::{Duration, Instant};

use common::*;

fn up(at: Point, by: f32) -> Point {
    Point::new(at.x, at.y - by)
}

/// Asserts that `got` is `expected` but for the rounding of normalized
/// values, which parameters store as `f32`.
fn assert_calls(got: &[HostCall], expected: &[HostCall]) {
    let close = |a: &HostCall, b: &HostCall| match (a, b) {
        (HostCall::Perform(p, x), HostCall::Perform(q, y)) => p == q && (x - y).abs() < 1e-6,
        _ => a == b,
    };
    assert!(
        got.len() == expected.len() && got.iter().zip(expected).all(|(a, b)| close(a, b)),
        "the host received {got:?}, not {expected:?}"
    );
}

#[test]
fn every_control_is_there_with_its_value_in_the_parameters_own_words() {
    let open = Open::new(None);
    let shown: Vec<(Role, String, Option<String>)> = open
        .ui
        .access_tree()
        .into_iter()
        .filter(|entry| entry.node.role != Role::Group)
        .map(|entry| (entry.node.role, entry.node.name, entry.node.value))
        .collect();
    let text = |s: &str| Some(s.to_owned());
    assert_eq!(
        shown,
        [
            (Role::Label, "Mangue Duck".to_owned(), None),
            (Role::Switch, "Bypass".to_owned(), text("Off")),
            (Role::Slider, "Threshold".to_owned(), text("-30.0 dB")),
            (Role::Slider, "Depth".to_owned(), text("12.0 dB")),
            (Role::Slider, "Attack".to_owned(), text("5.0 ms")),
            (Role::Slider, "Release".to_owned(), text("150 ms")),
            (Role::Meter, "Reduction".to_owned(), text("0.0 dB")),
        ]
    );
    // The knobs give assistive technology their plain values and ranges.
    let depth = open.get(Role::Slider, "Depth").node;
    assert_eq!(depth.numeric, Some(12.0));
    let range = depth.range.expect("a range");
    assert_eq!((range.min, range.max), (0.0, 40.0));
    assert_eq!(
        open.get(Role::Switch, "Bypass").node.state.checked,
        Some(false)
    );
    // Opening the editor told the host nothing.
    assert_eq!(open.host.take_calls(), []);
}

#[test]
fn a_drag_on_the_depth_knob_is_one_gesture_that_reaches_the_processor() {
    let mut open = Open::new(None);
    // Ducked by the default depth first.
    let before = open.process(Some(1.0), 24_000);
    assert!((before.channel(0)[23_999] - db_to_gain(-12.0) as f32).abs() < 1e-4);

    // Twenty logical pixels up is a tenth of the theme's 200-pixel drag
    // range: a tenth of 0 to 40 dB.
    let at = open.get(Role::Slider, "Depth").bounds.center();
    open.ui.press(at);
    open.ui.move_to(up(at, 20.0));
    open.ui.move_to(up(at, 40.0));
    open.ui.release(up(at, 40.0));
    assert_calls(
        &open.host.take_calls(),
        &[
            HostCall::BeginGesture(DEPTH),
            HostCall::Perform(DEPTH, 0.4),
            HostCall::Perform(DEPTH, 0.5),
            HostCall::EndGesture(DEPTH),
        ],
    );
    assert_eq!(open.harness.params().depth.value(), 20.0);
    open.ui.tick();
    assert_eq!(open.text(Role::Slider, "Depth"), "20.0 dB");

    // The next block ducks by the new depth from its first frame on: the
    // follower has settled, so the gain steps to -20 dB at once.
    let after = open.process(Some(1.0), 4_800);
    assert!((after.channel(0)[0] - db_to_gain(-20.0) as f32).abs() < 1e-4);
    assert!((after.channel(0)[4_799] - db_to_gain(-20.0) as f32).abs() < 1e-4);
    open.ui.tick();
    assert_eq!(open.text(Role::Meter, "Reduction"), "20.0 dB");

    // The press gave the knob the keyboard focus: each arrow is one more
    // edit of one step, a hundredth of the range.
    assert!(open.ui.key(Key::Down));
    assert_calls(
        &open.host.take_calls(),
        &[
            HostCall::BeginGesture(DEPTH),
            HostCall::Perform(DEPTH, 0.49),
            HostCall::EndGesture(DEPTH),
        ],
    );
    assert!((open.harness.params().depth.value() - 19.6).abs() < 1e-4);
    assert_eq!(open.host.well_formed(), Ok(()));
}

#[test]
fn escape_during_a_drag_puts_the_starting_value_back() {
    let mut open = Open::new(None);
    let at = open.get(Role::Slider, "Threshold").bounds.center();
    open.ui.press(at);
    open.ui.move_to(up(at, 50.0));
    assert!(open.ui.key(Key::Escape));
    open.ui.release(up(at, 50.0));
    let calls = open.host.take_calls();
    assert_calls(
        &calls,
        &[
            HostCall::BeginGesture(THRESHOLD),
            HostCall::Perform(THRESHOLD, 0.75),
            HostCall::Perform(THRESHOLD, 0.5),
            HostCall::EndGesture(THRESHOLD),
        ],
    );
    assert_eq!(open.harness.params().threshold.value(), -30.0);
    open.process(None, 256);
    open.ui.tick();
    assert_eq!(open.text(Role::Slider, "Threshold"), "-30.0 dB");
}

#[test]
fn the_bypass_switch_turns_the_effect_off_and_the_meter_shows_it() {
    let mut open = Open::new(None);
    open.process(Some(1.0), 24_000);
    open.ui.tick();
    assert_eq!(open.text(Role::Meter, "Reduction"), "12.0 dB");

    let switch = open.get(Role::Switch, "Bypass").bounds.center();
    open.ui.press(switch);
    open.ui.release(switch);
    assert_calls(
        &open.host.take_calls(),
        &[
            HostCall::BeginGesture(BYPASS),
            HostCall::Perform(BYPASS, 1.0),
            HostCall::EndGesture(BYPASS),
        ],
    );
    assert!(open.harness.params().bypass.value());

    // The effect fades out over 10 ms (480 frames), so the input is
    // untouched soon after the switch, and the meter says so.
    let output = open.process(Some(1.0), 4_800);
    assert!(output.channel(0)[0] < 0.3);
    assert_eq!(output.channel(0)[1_000], 1.0);
    assert_eq!(output.channel(0)[4_799], 1.0);
    open.ui.tick();
    assert_eq!(open.text(Role::Meter, "Reduction"), "0.0 dB");
    let switch = open.get(Role::Switch, "Bypass");
    assert_eq!(switch.node.state.checked, Some(true));
    assert_eq!(switch.node.value.as_deref(), Some("On"));

    // A second click turns it back on: the input is ducked again.
    let at = switch.bounds.center();
    open.ui.press(at);
    open.ui.release(at);
    assert!(!open.harness.params().bypass.value());
    let output = open.process(Some(1.0), 4_800);
    assert!((output.channel(0)[4_799] - db_to_gain(-12.0) as f32).abs() < 1e-4);
    assert_eq!(open.host.well_formed(), Ok(()));
}

#[test]
fn the_meter_follows_what_the_processor_publishes_block_by_block() {
    let mut open = Open::new(None);
    let meter = open.harness.plugin_mut().reduction_meter();
    assert_eq!(meter.reduction_db(), 0.0);
    let (_, presented) = open.ui.frames();

    // Sidechain levels from silence to far above the threshold and back:
    // after each block the meter shows exactly what the processor stored,
    // which is how far it turned the last frame down.
    let mut levels = vec![0.0, 0.02, 0.05, 0.05, 0.1, 0.5, 1.0, 1.0, 0.3];
    levels.extend([0.0; 12]);
    let mut readings = Vec::new();
    for (block, &level) in levels.iter().enumerate() {
        let output = open.process(Some(level), 2_400);
        let stored = meter.reduction_db();
        // The input is 1.0, so the last sample is the gain applied to it.
        let applied = -gain_to_db(f64::from(output.channel(0)[2_399])) as f32;
        assert!(
            (stored - applied.max(0.0)).abs() < 1e-4,
            "block {block}: stored {stored}, applied {applied}"
        );
        open.ui.tick();
        let shown = open.get(Role::Meter, "Reduction").node;
        assert_eq!(shown.numeric, Some(f64::from(stored)), "block {block}");
        assert_eq!(
            shown.value,
            Some(format!("{stored:.1} dB")),
            "block {block}"
        );
        readings.push(stored);
    }
    // Nothing below the threshold (-30 dB, and 0.02 is -34 dB); 0.05 is
    // -26 dB, a third of the way up the 12 dB range, so a third of the
    // depth; full depth from -18 dB up; then back down to nothing over the
    // release, never rising on the way.
    assert_eq!(readings[..2], [0.0, 0.0], "{readings:?}");
    let third = 12.0 * (gain_to_db(0.05) + 30.0) / 12.0;
    assert!(
        (f64::from(readings[3]) - third).abs() < 0.01,
        "{readings:?}"
    );
    assert!(readings[7] > 11.99 && readings[7] <= 12.0, "{readings:?}");
    assert!(
        readings[9..].windows(2).all(|pair| pair[1] <= pair[0]),
        "{readings:?}"
    );
    assert!(
        readings[14..18].iter().all(|&db| db > 0.0 && db < 12.0),
        "{readings:?}"
    );
    assert_eq!(*readings.last().unwrap(), 0.0, "{readings:?}");

    // Blocks that leave the reduction where it was draw nothing new.
    let (_, before) = open.ui.frames();
    assert!(before > presented);
    open.process(Some(0.0), 2_400);
    open.ui.tick();
    open.ui.tick();
    assert_eq!(open.ui.frames().1, before);
}

/// A host that stops calling the processor (it deactivated the plugin, or
/// suspended a silent track) leaves the last reading behind. The meter
/// shows it for `METER_HOLD`, then no reduction, and the processor's next
/// block brings the reading back at once.
#[test]
fn the_meter_drops_a_reading_the_processor_no_longer_refreshes() {
    let mut open = Open::new(None);
    let meter = open.harness.plugin_mut().reduction_meter();
    open.process(Some(1.0), 24_000);
    let published = Instant::now();
    open.ui.tick();
    assert_eq!(open.text(Role::Meter, "Reduction"), "12.0 dB");
    // Still shown while the hold lasts (checked only if this machine got
    // here in time).
    open.ui.tick();
    if published.elapsed() < METER_HOLD {
        assert_eq!(open.text(Role::Meter, "Reduction"), "12.0 dB");
    }

    // No block for longer than the hold: no reduction shown, while the
    // processor's own reading is untouched.
    std::thread::sleep(METER_HOLD + Duration::from_millis(100));
    open.ui.tick();
    assert_eq!(open.text(Role::Meter, "Reduction"), "0.0 dB");
    assert_eq!(open.get(Role::Meter, "Reduction").node.numeric, Some(0.0));
    assert!((meter.reduction_db() - 12.0).abs() < 1e-3);
    open.ui.tick();
    assert_eq!(open.text(Role::Meter, "Reduction"), "0.0 dB");

    // Processing again shows the reading on the next frame, even one that
    // equals the reading left behind.
    let before = meter.blocks();
    open.process(Some(1.0), 256);
    assert_eq!(meter.blocks(), before.wrapping_add(1));
    open.ui.tick();
    assert_eq!(open.text(Role::Meter, "Reduction"), "12.0 dB");

    // A reset publishes too: no reduction, at once.
    open.harness.reset();
    open.ui.tick();
    assert_eq!(open.text(Role::Meter, "Reduction"), "0.0 dB");
}

#[test]
fn host_automation_moves_the_knobs_and_is_not_reported_back() {
    let mut open = Open::new(None);
    open.harness.automate(100, "threshold", -12.0);
    open.harness.automate(200, "release", 900.0);
    open.harness.automate(300, "bypass", 1.0);
    open.process(Some(0.5), 512);
    open.ui.tick();
    assert_eq!(open.text(Role::Slider, "Threshold"), "-12.0 dB");
    assert_eq!(open.text(Role::Slider, "Release"), "900 ms");
    assert_eq!(
        open.get(Role::Switch, "Bypass").node.state.checked,
        Some(true)
    );
    for _ in 0..4 {
        open.process(Some(0.5), 512);
        open.ui.tick();
    }
    assert_eq!(open.host.take_calls(), []);
}

#[test]
fn closing_ends_a_held_gesture_and_reopening_shows_the_values() {
    let mut open = Open::new(None);
    let at = open.get(Role::Slider, "Attack").bounds.center();
    open.ui.press(at);
    open.ui.move_to(up(at, 100.0));
    // The host closes the editor in the middle of the drag.
    open.close();
    assert!(!open.ui.is_open());
    let calls = open.host.take_calls();
    assert_eq!(calls.first(), Some(&HostCall::BeginGesture(ATTACK)));
    assert_eq!(calls.last(), Some(&HostCall::EndGesture(ATTACK)));
    assert_eq!(calls.len(), 3, "{calls:?}");
    let attack = open.harness.params().attack.value();
    assert!(attack > 5.0, "the drag raised it to {attack}");

    open.reopen(Some(1.5));
    assert_eq!(open.text(Role::Slider, "Attack"), format!("{attack:.1} ms"));
    assert_eq!(open.ui.size(), Some(PhysicalSize::new(660, 315)));
    assert_eq!(open.host.take_calls(), []);
}

/// The leaves of the tree: what a person sees and uses.
fn controls(open: &Open) -> Vec<(Role, String, Rect, manguebyte_gfx::IRect)> {
    open.ui
        .access_tree()
        .into_iter()
        .filter(|entry| entry.node.role != Role::Group)
        .map(|entry| (entry.node.role, entry.node.name, entry.bounds, entry.device))
        .collect()
}

/// Whether `inner` lies within `outer`.
fn inside(outer: &Rect, inner: &Rect) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.x + inner.width <= outer.x + outer.width
        && inner.y + inner.height <= outer.y + outer.height
}

#[test]
fn the_layout_fits_and_keeps_its_shape_at_every_scale_from_1_to_2() {
    let reference = controls(&Open::new(Some(1.0)));
    assert_eq!(reference.len(), 7);
    for scale in [1.0, 1.25, 1.5, 1.75, 2.0] {
        let mut open = Open::new(Some(scale));
        let size = mangue_duck::EDITOR_SIZE.scale_by(scale);
        assert_eq!(open.editor().size(), size, "at {scale}");
        assert_eq!(
            open.ui.size(),
            Some(PhysicalSize::new(size.width, size.height)),
            "at {scale}"
        );
        let found = controls(&open);
        let window = manguebyte_gfx::IRect::new(0, 0, size.width, size.height);
        let logical = Rect::new(4.0, 4.0, 432.0, 202.0);
        for (index, (role, name, bounds, device)) in found.iter().enumerate() {
            // Inside the window, clear of its edges, and in the same place
            // as at scale 1 but for rounding to device pixels (text lines
            // snap to whole device pixels, which at 1.75 is 1.14 logical
            // pixels on a 20-pixel line).
            assert!(
                window.contains_rect(device),
                "{name} at {scale}: {device:?}"
            );
            assert!(inside(&logical, bounds), "{name} at {scale}: {bounds:?}");
            let (_, _, at_one, _) = &reference[index];
            for (a, b) in [
                (bounds.x, at_one.x),
                (bounds.y, at_one.y),
                (bounds.width, at_one.width),
                (bounds.height, at_one.height),
            ] {
                assert!((a - b).abs() <= 1.5, "{name} at {scale}: {bounds:?}");
            }
            assert_eq!(
                (*role, name.as_str()),
                (reference[index].0, reference[index].1.as_str())
            );
            // No two controls overlap.
            for (_, _, other_bounds, other_device) in &found[index + 1..] {
                assert!(
                    bounds
                        .intersect(other_bounds)
                        .is_none_or(|r| r.width * r.height == 0.0),
                    "{name} overlaps at {scale}"
                );
                assert!(
                    device
                        .intersect(other_device)
                        .is_none_or(|r| r.width * r.height == 0),
                    "{name} overlaps in device pixels at {scale}"
                );
            }
        }
        // A drag of the same logical length sets the same value at every
        // scale.
        let at = open.get(Role::Slider, "Depth").bounds.center();
        open.ui.press(at);
        open.ui.move_to(up(at, 20.0));
        open.ui.release(up(at, 20.0));
        assert!(
            (open.harness.params().depth.value() - 16.0).abs() < 1e-4,
            "at {scale}"
        );
    }
}

/// A small generator of numbers, the same on every run.
struct Numbers(u64);

impl Numbers {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn coordinate(&mut self, extent: u32) -> i32 {
        match self.below(8) {
            0 => i32::MIN,
            1 => i32::MAX,
            2 => -(self.below(1_000) as i32),
            3 => extent as i32 + self.below(1_000) as i32,
            _ => self.below(u64::from(extent)) as i32,
        }
    }
}

#[test]
fn hostile_input_in_any_order_never_breaks_the_gestures_or_the_values() {
    let keys = [
        Key::Up,
        Key::Down,
        Key::Left,
        Key::Right,
        Key::PageUp,
        Key::PageDown,
        Key::Home,
        Key::End,
        Key::Escape,
        Key::Tab,
        Key::Enter,
        Key::Delete,
        Key::Backspace,
        Key::Character(' '),
        Key::Character('x'),
    ];
    let buttons = [Button::Primary, Button::Secondary, Button::Middle];
    for seed in 1..=8_u64 {
        let mut open = Open::new(Some(1.0));
        let mut numbers = Numbers(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        for step in 0..2_000 {
            // The window's size as the editor tells the host. (Reading the
            // window's frame would copy it on every step.)
            let size = open.editor().size();
            let point = PhysicalPoint::new(
                numbers.coordinate(size.width),
                numbers.coordinate(size.height),
            );
            let modifiers = Modifiers {
                shift: numbers.below(4) == 0,
                control: numbers.below(8) == 0,
                alt: numbers.below(16) == 0,
                meta: false,
            };
            let window = open.ui.window();
            match numbers.below(16) {
                0..=2 => {
                    window.input(RawInput::PointerDown {
                        at: point,
                        button: buttons[numbers.below(3) as usize],
                        modifiers,
                        time_ms: numbers.below(2_000) as u32,
                    });
                }
                3..=6 => {
                    window.input(RawInput::PointerMove {
                        at: point,
                        modifiers,
                    });
                }
                7..=8 => {
                    window.input(RawInput::PointerUp {
                        at: point,
                        button: buttons[numbers.below(3) as usize],
                        modifiers,
                    });
                }
                9 => {
                    let delta = match numbers.below(3) {
                        0 => i32::MIN,
                        1 => i32::MAX,
                        _ => numbers.below(480) as i32 - 240,
                    };
                    window.input(RawInput::Wheel {
                        at: point,
                        delta_x: 0,
                        delta_y: delta,
                        modifiers,
                    });
                }
                10 => {
                    let key = keys[numbers.below(keys.len() as u64) as usize];
                    window.input(RawInput::Key {
                        key: KeyEvent {
                            key,
                            modifiers,
                            pressed: numbers.below(3) != 0,
                        },
                        repeat: numbers.below(4) == 0,
                    });
                }
                11 => {
                    window.input(if numbers.below(2) == 0 {
                        RawInput::CaptureLost
                    } else {
                        RawInput::PointerLeft
                    });
                }
                12 => {
                    window.input(RawInput::Focus(numbers.below(2) == 0));
                }
                13 => {
                    let level = [0.0, 0.1, 1.0, f32::MAX, f32::INFINITY, f32::NAN]
                        [numbers.below(6) as usize];
                    let frames = 1 + numbers.below(700);
                    if numbers.below(2) == 0 {
                        // A host's value for a parameter, as the wrappers
                        // deliver it, or one made by hand: some of them not
                        // a number, infinite or far beyond the range.
                        let param =
                            [THRESHOLD, DEPTH, ATTACK, RELEASE, BYPASS][numbers.below(5) as usize];
                        let value = [
                            f64::NAN,
                            f64::INFINITY,
                            f64::NEG_INFINITY,
                            1e300,
                            -1e300,
                            f64::MIN_POSITIVE,
                            0.0,
                            12.0,
                        ][numbers.below(8) as usize];
                        let at = open.harness.position() + numbers.below(frames);
                        open.harness
                            .schedule(at, EventKind::ParamValue { param, value });
                    }
                    let output = open.process(Some(level), frames as usize);
                    let bad = output.channel(0).iter().position(|s| !s.is_finite());
                    assert_eq!(bad, None, "seed {seed}, step {step}: the output");
                }
                14 => {
                    let editor = open.editor();
                    match numbers.below(5) {
                        0 => {
                            let scale = [0.0, 1.0, 1.25, 2.0, 3.0, f64::NAN, -1.0, 1e300]
                                [numbers.below(8) as usize];
                            editor.set_scale(scale);
                        }
                        1 => editor.set_visible(numbers.below(2) == 0),
                        2 => {
                            let wanted = HostSize::new(
                                numbers.below(10_000) as u32,
                                numbers.below(10_000) as u32,
                            );
                            let fitted = editor.fit(wanted);
                            editor.set_size(fitted);
                            editor.set_size(wanted);
                        }
                        3 => {
                            editor.key(KeyEvent {
                                key: keys[numbers.below(keys.len() as u64) as usize],
                                modifiers,
                                pressed: true,
                            });
                        }
                        _ => editor.on_main_thread(),
                    }
                }
                _ => {
                    open.ui.tick();
                }
            }
            if let Err(error) = open.host.well_formed() {
                panic!("seed {seed}, step {step}: {error}");
            }
            let params = open.harness.params();
            for (value, min, max) in [
                (params.threshold.value(), -60.0, 0.0),
                (params.depth.value(), 0.0, 40.0),
                (params.attack.value(), 0.1, 200.0),
                (params.release.value(), 5.0, 2_000.0),
            ] {
                assert!(
                    (min..=max).contains(&value),
                    "seed {seed}, step {step}: {value} outside {min}..={max}"
                );
            }
            let reduction = open.harness.plugin_mut().reduction_meter().reduction_db();
            assert!((0.0..=40.0).contains(&reduction), "{reduction}");
        }
        assert!(open.ui.is_open(), "seed {seed}: the editor survived");
        // Closing ends whatever gesture is still open.
        open.close();
        assert_eq!(open.host.open_at_host(), [], "seed {seed}");
        assert_eq!(open.host.well_formed(), Ok(()), "seed {seed}");
    }
}
