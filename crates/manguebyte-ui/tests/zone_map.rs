//! The zone map: selecting, moving and resizing zones by whole keys and
//! velocities, overlaps, the keyboard, drops from a list and from other
//! programs, alignment with a keyboard, assistive technology, and drawing
//! that a partial repaint reproduces exactly at every scale.

use std::cell::Cell;
use std::rc::Rc;

use manguebyte_gfx::{Point, Size};
use manguebyte_ui::testing::Harness;
use manguebyte_ui::{
    Action, DragPayload, DropData, FileDrop, IntoView, Key, KeySpan, KeyboardState, ListData,
    ListState, Modifiers, Role, Zone, ZoneEvent, ZoneMapState, column, list_view, piano_keyboard,
    zone_map,
};

const SPAN: KeySpan = KeySpan::KEYS_61;

/// Two octaves from C3 over 240 by 128 logical pixels: a key is 10
/// pixels wide and a velocity 1 pixel tall.
fn two_octaves() -> KeySpan {
    KeySpan::new(48, 71)
}

fn open(zones: Vec<Zone>, scale: f32) -> (Harness, ZoneMapState) {
    let state = ZoneMapState::new();
    state.set_zones(zones);
    let mut ui = Harness::new(
        zone_map("Zones", two_octaves(), state.clone()),
        Size::new(240.0, 128.0),
        scale,
    );
    ui.frame();
    (ui, state)
}

fn keys(state: &ZoneMapState, id: u64) -> (u8, u8, u8, u8) {
    let zone = state.zone(id).expect("a zone");
    (
        zone.low_key,
        zone.high_key,
        zone.low_velocity,
        zone.high_velocity,
    )
}

fn edited(state: &ZoneMapState) -> Vec<Vec<u64>> {
    state
        .take_events()
        .into_iter()
        .filter_map(|event| match event {
            ZoneEvent::Edited(ids) => Some(ids),
            _ => None,
        })
        .collect()
}

const CONTROL: Modifiers = Modifiers {
    control: true,
    ..Modifiers::NONE
};

const SHIFT: Modifiers = Modifiers {
    shift: true,
    ..Modifiers::NONE
};

const ALT: Modifiers = Modifiers {
    alt: true,
    ..Modifiers::NONE
};

#[test]
fn clicks_select_and_ctrl_and_shift_add() {
    let (mut ui, state) = open(
        vec![
            Zone::new(1, "Bass", 48, 52),
            Zone::new(2, "Pad", 60, 64),
            Zone::new(3, "Lead", 66, 70),
        ],
        1.0,
    );
    ui.click(ui.get(Role::ListItem, "Bass").center());
    assert_eq!(state.selected(), [1]);
    ui.set_modifiers(CONTROL);
    ui.click(ui.get(Role::ListItem, "Lead").center());
    assert_eq!(state.selected(), [1, 3]);
    ui.click(ui.get(Role::ListItem, "Bass").center());
    assert_eq!(state.selected(), [3]);
    ui.set_modifiers(SHIFT);
    ui.click(ui.get(Role::ListItem, "Pad").center());
    assert_eq!(state.selected(), [3, 2]);
    // A click on nothing selects nothing.
    ui.set_modifiers(Modifiers::NONE);
    ui.click(Point::new(57.0, 64.0));
    assert!(state.selected().is_empty());
    ui.frame();
    assert_eq!(
        ui.get(Role::List, "Zones").value_text(),
        Some("3 zones, 0 selected")
    );
    assert!(edited(&state).is_empty(), "selecting edits nothing");
}

#[test]
fn a_drag_moves_the_zones_selected_by_whole_keys_and_velocities_within_the_map() {
    let (mut ui, state) = open(
        vec![
            Zone::new(1, "Bass", 48, 52)
                .with_velocities(0, 90)
                .with_root(48),
            Zone::new(2, "Pad", 60, 64).with_velocities(0, 90),
        ],
        1.0,
    );
    ui.click(ui.get(Role::ListItem, "Bass").center());
    ui.set_modifiers(SHIFT);
    ui.click(ui.get(Role::ListItem, "Pad").center());
    ui.set_modifiers(Modifiers::NONE);
    // 2.6 keys right and 14.4 velocities up: 3 and 14.
    let pad = ui.get(Role::ListItem, "Pad").center();
    ui.drag(pad, (26.0, -14.4), 4);
    assert_eq!(keys(&state, 1), (51, 55, 14, 104));
    assert_eq!(keys(&state, 2), (63, 67, 14, 104));
    assert_eq!(state.zone(1).and_then(|zone| zone.root), Some(51));
    assert_eq!(edited(&state), [vec![1, 2]]);
    // Far right and up: the highest zone stops at the last key and the
    // loudest velocity, and the other keeps its distance.
    ui.frame();
    let pad = ui.get(Role::ListItem, "Pad").center();
    ui.drag(pad, (200.0, -100.0), 4);
    assert_eq!(keys(&state, 2), (67, 71, 37, 127));
    assert_eq!(keys(&state, 1), (55, 59, 37, 127));
}

#[test]
fn edges_resize_onto_key_and_velocity_boundaries_and_never_cross() {
    let (mut ui, state) = open(
        vec![Zone::new(1, "Pad", 52, 59).with_velocities(20, 100)],
        1.0,
    );
    // The right edge is at x = 120; dragged to 143, it lands between keys
    // at 140: the zone ends on key 61.
    ui.drag(Point::new(119.0, 60.0), (24.0, 0.0), 3);
    assert_eq!(keys(&state, 1), (52, 61, 20, 100));
    // The left edge, at x = 40, dragged past the right one: one key wide.
    ui.frame();
    ui.drag(Point::new(41.0, 60.0), (150.0, 0.0), 3);
    assert_eq!(keys(&state, 1), (61, 61, 20, 100));
    // The top edge, at y = 27, dragged up 10: velocity 110.
    ui.frame();
    ui.drag(Point::new(135.0, 27.0), (0.0, -10.0), 2);
    assert_eq!(keys(&state, 1), (61, 61, 20, 110));
    // The bottom-right corner, both at once: the edges go where the
    // pointer is, between keys 63 and 64 and velocities 11 and 10.
    ui.frame();
    let zone = ui.get(Role::ListItem, "Pad").bounds;
    ui.drag(
        Point::new(zone.right() - 1.0, zone.bottom() - 1.0),
        (20.0, 10.0),
        2,
    );
    assert_eq!(keys(&state, 1), (61, 63, 11, 110));
    assert_eq!(edited(&state).len(), 4);
}

#[test]
fn a_press_inside_a_narrow_zone_moves_it_rather_than_the_edge_beside_it() {
    // A one-key zone at C3, and a wide zone drawn over it from C#3 on:
    // the middle of C3 is within reach of the wide zone's left edge.
    // Keys 8 pixels wide.
    let state = ZoneMapState::new();
    state.set_zones(vec![
        Zone::new(1, "Narrow", 48, 48),
        Zone::new(2, "Wide", 49, 60),
    ]);
    let view = zone_map("Zones", two_octaves(), state.clone());
    let mut ui = Harness::new(view, Size::new(192.0, 128.0), 1.0);
    ui.frame();
    let narrow = ui.get(Role::ListItem, "Narrow").center();
    let wide = ui.get(Role::ListItem, "Wide").bounds;
    assert!(
        wide.x - narrow.x <= 4.0,
        "within the wide zone's edge reach"
    );
    ui.drag(narrow, (16.0, 0.0), 2);
    assert_eq!(keys(&state, 1).0, 50);
    assert_eq!(keys(&state, 2), (49, 60, 0, 127));
}

#[test]
fn a_band_selects_what_it_crosses_and_escape_puts_everything_back() {
    let (mut ui, state) = open(
        vec![
            Zone::new(1, "Bass", 48, 50).with_velocities(0, 40),
            Zone::new(2, "Pad", 60, 64).with_velocities(0, 40),
            Zone::new(3, "Lead", 66, 70).with_velocities(0, 40),
        ],
        1.0,
    );
    // A band from beside Pad to the left edge crosses Bass and Pad.
    ui.drag(Point::new(175.0, 80.0), (-165.0, 40.0), 4);
    assert_eq!(state.selected(), [1, 2]);
    // A drag of the two, cancelled with Escape.
    ui.frame();
    let pad = ui.get(Role::ListItem, "Pad").center();
    ui.press(pad);
    ui.move_to(Point::new(pad.x + 30.0, pad.y));
    assert_eq!(keys(&state, 2).0, 63);
    assert!(ui.key(Key::Escape));
    ui.release(Point::new(pad.x + 30.0, pad.y));
    assert_eq!(keys(&state, 2).0, 60);
    assert_eq!(keys(&state, 1).0, 48);
    assert!(edited(&state).is_empty());
}

#[test]
fn overlaps_are_marked_and_said() {
    let (ui, _) = open(
        vec![
            Zone::new(1, "Soft", 48, 59).with_velocities(0, 80),
            Zone::new(2, "Hard", 55, 65).with_velocities(60, 127),
            Zone::new(3, "Apart", 68, 71),
        ],
        1.0,
    );
    assert_eq!(
        ui.get(Role::ListItem, "Soft").value_text(),
        Some("C3 to B3, velocity 0 to 80, overlaps Hard")
    );
    assert_eq!(
        ui.get(Role::ListItem, "Apart").value_text(),
        Some("G#4 to B4, velocity 0 to 127")
    );
    // Inside the overlap (keys 55 to 59, velocities 60 to 80) the colour
    // differs from either zone alone.
    let pixel = |x: u32, y: u32| ui.pixels().pixel(x, y).map(|p| p.to_array());
    let both = pixel(85, 57);
    assert_ne!(both, pixel(85, 100), "below the overlap: Soft alone");
    assert_ne!(both, pixel(85, 30), "above it: Hard alone");
    ui.assert_accessible();
}

#[test]
fn the_keyboard_chooses_moves_resizes_and_removes_zones() {
    let (mut ui, state) = open(
        vec![
            Zone::new(1, "Pad", 60, 64).with_velocities(0, 100),
            Zone::new(2, "Bass", 48, 50),
        ],
        1.0,
    );
    assert!(ui.tab());
    // Page Down goes through the zones from the lowest key.
    assert!(ui.key(Key::PageDown));
    assert_eq!(state.selected(), [2]);
    assert!(ui.key(Key::PageDown));
    assert_eq!(state.selected(), [1]);
    ui.frame();
    assert!(ui.get(Role::ListItem, "Pad").is_focused());
    assert!(ui.key(Key::Right));
    assert!(ui.key_with(Key::Up, SHIFT));
    assert_eq!(keys(&state, 1), (61, 65, 10, 110));
    assert!(ui.key_with(Key::Right, ALT));
    assert!(ui.key_with(
        Key::Left,
        Modifiers {
            alt: true,
            shift: true,
            ..Modifiers::NONE
        }
    ));
    assert_eq!(keys(&state, 1), (60, 66, 10, 110));
    assert_eq!(edited(&state).len(), 4);
    // Ctrl+A, then Delete: both go, and the application hears which.
    assert!(ui.key_with(Key::Character('a'), CONTROL));
    assert!(ui.key(Key::Delete));
    assert!(state.zones().is_empty());
    let removed = state.take_events();
    assert!(matches!(removed.as_slice(), [ZoneEvent::Removed(gone)] if gone.len() == 2));
}

#[test]
fn assistive_technology_selects_a_zone() {
    let (mut ui, state) = open(
        vec![Zone::new(4, "Pad", 60, 64), Zone::new(9, "Bass", 48, 50)],
        1.0,
    );
    ui.assert_accessible();
    let bass = ui.get(Role::ListItem, "Bass");
    assert_eq!(bass.node.state.selected, Some(false));
    assert!(ui.act(&bass, Action::Press));
    ui.frame();
    assert_eq!(state.selected(), [9]);
    assert_eq!(
        ui.get(Role::ListItem, "Bass").node.state.selected,
        Some(true)
    );
}

/// Samples in a list, which drag as their names.
struct Samples(Vec<&'static str>);

struct Dragged(Vec<String>);

impl ListData for Samples {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn name(&self, row: usize) -> String {
        self.0
            .get(row)
            .map(|name| (*name).to_owned())
            .unwrap_or_default()
    }
    fn revision(&self) -> u64 {
        0
    }
    fn drag_payload(&self, rows: &[usize]) -> Option<DragPayload> {
        let names: Vec<String> = rows.iter().map(|&row| self.name(row)).collect();
        let description = names.join(", ");
        Some(DragPayload::new(Dragged(names), description))
    }
}

#[test]
fn a_sample_dragged_from_a_list_onto_a_key_makes_a_zone_there() {
    let state = ZoneMapState::new();
    let list = ListState::new();
    let refused = Rc::new(Cell::new(0));
    let counted = Rc::clone(&refused);
    let view = column((
        list_view(
            "Samples",
            Samples(vec!["Kick.wav", "Snare.wav"]),
            list.clone(),
        )
        .height(60.0),
        zone_map("Zones", two_octaves(), state.clone())
            .takes(move |payload| {
                let names = payload.get::<Dragged>().map(|dragged| dragged.0.clone());
                if names.is_none() {
                    counted.set(counted.get() + 1);
                }
                names
            })
            .height(128.0),
    ));
    let mut ui = Harness::new(view, Size::new(240.0, 188.0), 1.0);
    ui.frame();
    let snare = ui.get(Role::ListItem, "Snare.wav").center();
    // E3 is key 52, the fifth column: x from 40 to 50.
    let e3 = Point::new(45.0, 60.0 + 64.0);
    ui.press(snare);
    ui.move_to(Point::new(snare.x, snare.y + 10.0));
    ui.move_to(e3);
    ui.frame();
    ui.release(e3);
    ui.frame();
    let zones = state.zones();
    assert_eq!(zones.len(), 1);
    assert_eq!(
        (
            zones[0].name.as_str(),
            zones[0].low_key,
            zones[0].high_key,
            zones[0].root
        ),
        ("Snare.wav", 52, 52, Some(52))
    );
    assert_eq!(state.selected(), [zones[0].id]);
    let events = state.take_events();
    match events.as_slice() {
        [ZoneEvent::Created { ids, payload }] => {
            assert_eq!(ids, &[zones[0].id]);
            assert!(payload.is::<Dragged>());
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(refused.get(), 0);
    ui.assert_accessible();
}

#[test]
fn files_dropped_from_another_program_make_a_zone_each_on_the_keys_after() {
    let (mut ui, state) = open(vec![], 1.0);
    let files = DropData::files(["C:/kits/kick.wav", "C:/kits/snare.wav", "C:/kits/hat.wav"]);
    // On B4 (71), the last key: only one fits.
    let b4 = Point::new(235.0, 64.0);
    ui.root_mut().file_drop(FileDrop::Enter { at: b4 }, &files);
    ui.frame();
    // On C4 (60): three zones, 60 to 62.
    let c4 = Point::new(125.0, 64.0);
    ui.root_mut().file_drop(FileDrop::Over { at: c4 }, &files);
    ui.frame();
    assert!(ui.pixels() == &ui.render_fresh(), "the drop highlight");
    ui.root_mut().file_drop(FileDrop::Drop { at: c4 }, &files);
    ui.frame();
    let made: Vec<(String, u8)> = state
        .zones()
        .into_iter()
        .map(|zone| (zone.name, zone.low_key))
        .collect();
    assert_eq!(
        made,
        [
            ("kick.wav".to_owned(), 60),
            ("snare.wav".to_owned(), 61),
            ("hat.wav".to_owned(), 62)
        ]
    );
    // Text is not taken.
    let text = DropData::text("hello");
    ui.root_mut().file_drop(FileDrop::Enter { at: c4 }, &text);
    ui.root_mut().file_drop(FileDrop::Drop { at: c4 }, &text);
    assert_eq!(state.zones().len(), 3);
    // Near the top end, what does not fit is left out.
    ui.root_mut().file_drop(FileDrop::Enter { at: b4 }, &files);
    ui.root_mut().file_drop(FileDrop::Drop { at: b4 }, &files);
    assert_eq!(state.zones().len(), 4);
    assert_eq!(state.zones()[3].low_key, 71);
}

#[test]
fn a_map_above_a_keyboard_lines_up_with_its_keys() {
    let state = ZoneMapState::new();
    state.set_zones(vec![
        Zone::new(1, "C", 60, 60),
        Zone::new(2, "C sharp", 61, 61),
        Zone::new(3, "Octave", 48, 59),
    ]);
    for scale in [1.0, 1.25, 2.0] {
        let view = column((
            zone_map("Zones", SPAN, state.clone()).height(100.0),
            piano_keyboard("Keys", SPAN, KeyboardState::new()).height(80.0),
        ));
        let mut ui = Harness::new(view, Size::new(610.0, 180.0), scale);
        ui.frame();
        let zone = ui.get(Role::ListItem, "C").bounds;
        let white = ui.get(Role::Button, "C4").bounds;
        assert!(
            white.x <= zone.x + 1e-3 && zone.right() <= white.right() + 1e-3,
            "{scale}x"
        );
        let zone = ui.get(Role::ListItem, "C sharp").bounds;
        let black = ui.get(Role::Button, "C#4").bounds;
        assert!(
            zone.x <= black.x && black.right() <= zone.right(),
            "{scale}x"
        );
        // An octave of zones spans the octave of white keys exactly.
        let zone = ui.get(Role::ListItem, "Octave").bounds;
        let c3 = ui.get(Role::Button, "C3").bounds;
        let b3 = ui.get(Role::Button, "B3").bounds;
        assert!((zone.x - c3.x).abs() < 1e-3 && (zone.right() - b3.right()).abs() < 1e-3);
        ui.assert_accessible();
    }
}

#[test]
fn edits_draw_only_the_map_and_equal_a_whole_repaint() {
    for scale in [1.0, 1.25, 2.0] {
        let state = ZoneMapState::new();
        state.set_zones(vec![
            Zone::new(1, "Soft", 48, 59)
                .with_velocities(0, 80)
                .with_root(52),
            Zone::new(2, "Hard", 55, 65).with_velocities(60, 127),
        ]);
        let view = column((
            manguebyte_ui::label("Above").height(20.0),
            zone_map("Zones", two_octaves(), state.clone()).flex(1.0),
        ));
        let mut ui = Harness::new(view, Size::new(240.0, 148.0), scale);
        ui.frame();
        let found = ui.get(Role::List, "Zones");
        let check = |ui: &mut Harness, what: &str| {
            assert!(ui.frame().painted > 0, "{scale}x {what}");
            assert!(
                ui.last_damage()
                    .iter()
                    .all(|damage| found.device.contains_rect(damage)),
                "{scale}x {what}"
            );
            assert!(ui.pixels() == &ui.render_fresh(), "{scale}x after {what}");
        };
        let soft = ui.get(Role::ListItem, "Soft").center();
        ui.drag(soft, (13.0, -7.3), 3);
        check(&mut ui, "a move");
        ui.drag(Point::new(225.0, 140.0), (-60.0, -5.0), 3);
        check(&mut ui, "a band");
        assert!(ui.key(Key::PageDown));
        check(&mut ui, "a key");
        assert_eq!(ui.frames_rendered_during(|ui| ui.advance_ms(200)), 0);
    }
}

/// The labels a zone map drew, read from its scene's dump: each text, and
/// the rectangle it takes (from its first glyph's origin to its measured
/// width, from a size above its baseline to a fifth of a size below, past the descenders).
fn labels(ui: &Harness) -> Vec<(String, manguebyte_gfx::Rect)> {
    let map = ui.get(Role::List, "Zones");
    let dump = ui.dump_of(&map);
    dump.lines()
        .filter_map(|line| {
            let line = line.trim_start().strip_prefix("text \"")?;
            let (text, rest) = line.split_once('"')?;
            let number = |key: &str| -> Option<f32> {
                let at = rest.find(key)? + key.len();
                rest[at..].split_whitespace().next()?.parse().ok()
            };
            let (size, weight) = (number(" size ")?, number(" weight ")?);
            let glyphs = rest.split_once('[')?.1;
            let mut first = glyphs.split(',').next()?.split_whitespace().skip(1);
            let x: f32 = first.next()?.parse().ok()?;
            let baseline: f32 = first.next()?.parse().ok()?;
            let style = manguebyte_gfx::TextStyle::new(size).with_weight(weight);
            let width = manguebyte_gfx::TextLayout::new(text, &style, None)
                .size()
                .width;
            let rect = manguebyte_gfx::Rect::new(x, baseline - size, width, size * 1.2);
            Some((text.to_owned(), rect))
        })
        .collect()
}

#[test]
fn a_narrow_zone_drawn_over_a_wide_one_keeps_their_labels_apart_and_whole() {
    // The layout the sampler's scripted session ends in: Pad's zone, F3
    // to the end, and over it Pluck's, E3 to G3, narrow and chosen.
    for scale in [1.0, 1.25, 2.0] {
        let state = ZoneMapState::new();
        state.set_zones(vec![
            Zone::new(1, "Pad.wav", 53, 72),
            Zone::new(2, "Pluck.wav", 52, 55),
        ]);
        state.set_selected([2]);
        let mut ui = Harness::new(
            zone_map("Zones", two_octaves(), state.clone()),
            Size::new(240.0, 128.0),
            scale,
        );
        ui.frame();
        let found = labels(&ui);
        // Nothing is cut: a label is whole or left out.
        for (text, _) in &found {
            assert!(!text.contains('\u{2026}'), "{scale}x: {text:?} was cut");
        }
        // Pluck's zone is too narrow for its name; Pad's is there, beside
        // Pluck's zone, with its range.
        let texts: Vec<&str> = found.iter().map(|(text, _)| text.as_str()).collect();
        assert!(texts.contains(&"Pad.wav"), "{scale}x: {texts:?}");
        assert!(texts.contains(&"F3\u{2013}C5"), "{scale}x: {texts:?}");
        assert!(
            !texts.iter().any(|t| t.starts_with("Pl")),
            "{scale}x: {texts:?}"
        );
        // No two labels meet.
        for (i, (a, ra)) in found.iter().enumerate() {
            for (b, rb) in &found[i + 1..] {
                assert!(
                    ra.intersect(rb).is_none_or(|r| r.is_empty()),
                    "{scale}x: {a:?} {ra:?} meets {b:?} {rb:?}"
                );
            }
        }
        // No label of Pad's lies over Pluck's zone, which is drawn above it.
        let pluck = ui
            .everything()
            .into_iter()
            .find(|found| found.item.is_some() && found.node.name == "Pluck.wav")
            .expect("Pluck's zone")
            .bounds;
        for (text, rect) in &found {
            assert!(
                rect.intersect(&pluck).is_none_or(|r| r.is_empty()),
                "{scale}x: {text:?} {rect:?} is over Pluck's zone {pluck:?}"
            );
        }
    }
}

#[test]
fn labels_of_overlapping_zones_never_overlap_and_tiny_zones_go_unlabelled() {
    // Two zones over each other, and one a key wide.
    let (ui, _state) = open(
        vec![
            Zone::new(1, "Strings", 50, 60),
            Zone::new(2, "Brass", 52, 63),
            Zone::new(3, "Bell", 66, 66),
        ],
        1.0,
    );
    let dump = ui.dump();
    // The larger of the two that overlap keeps its label, the other gives
    // way; a zone one key (10 pixels) wide has no room for one.
    assert!(dump.contains("text \"Brass\""), "{dump}");
    assert!(!dump.contains("text \"Strings\""), "{dump}");
    assert!(!dump.contains("text \"Bell"), "{dump}");
    // The range is said under the name.
    assert!(dump.contains("text \"E3\u{2013}D#4\""), "{dump}");
}
