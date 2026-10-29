//! The piano keyboard: notes from the pointer, glissando, velocity from
//! where a key is pressed, the computer's letters, keys lit and ranges
//! marked by the application, assistive technology, and drawing at every
//! scale.

use manguebyte_gfx::{Color, Point, Size};
use manguebyte_ui::testing::Harness;
use manguebyte_ui::{
    Action, IntoView, Key, KeyEvent, KeyRange, KeySpan, KeyboardState, Modifiers, NoteEvent, Role,
    label, note_name_portuguese, piano_keyboard, row,
};

const SIZE: Size = Size::new(280.0, 90.0);

/// Two octaves from C3 at 280 by 90: the keys from y = 8 to 90.
fn two_octaves(scale: f32) -> (Harness, KeyboardState) {
    let state = KeyboardState::new();
    let view = piano_keyboard("Keys", KeySpan::new(48, 71), state.clone());
    let mut ui = Harness::new(view, SIZE, scale);
    ui.frame();
    (ui, state)
}

fn on(note: u8, velocity: u8) -> NoteEvent {
    NoteEvent::On { note, velocity }
}

fn off(note: u8) -> NoteEvent {
    NoteEvent::Off { note }
}

fn velocity_of(event: &NoteEvent) -> u8 {
    match event {
        NoteEvent::On { velocity, .. } => *velocity,
        _ => panic!("not a note on: {event:?}"),
    }
}

fn letter(ui: &mut Harness, c: char, pressed: bool) -> bool {
    ui.send(manguebyte_ui::Event::Key(KeyEvent {
        key: Key::Character(c),
        modifiers: Modifiers::NONE,
        pressed,
    }))
}

#[test]
fn a_click_plays_a_note_harder_the_further_down_the_key() {
    let (mut ui, state) = two_octaves(1.0);
    let c4 = ui.get(Role::Button, "C4");
    let x = c4.center().x;
    let (top, bottom) = (c4.bounds.y, c4.bounds.bottom());
    ui.click(Point::new(x, top + 0.1));
    ui.click(Point::new(x, (top + bottom) * 0.5));
    ui.click(Point::new(x, bottom - 0.1));
    let events = state.take_events();
    assert_eq!(events.len(), 6, "{events:?}");
    assert_eq!(velocity_of(&events[0]), 1);
    assert_eq!(velocity_of(&events[2]), 64);
    assert_eq!(velocity_of(&events[4]), 127);
    assert_eq!([events[1], events[3], events[5]], [off(60); 3]);
    // A black key is as long as it is, not the white keys.
    let black = ui.get(Role::Button, "C#4");
    ui.click(Point::new(black.center().x, black.bounds.bottom() - 0.1));
    assert_eq!(state.take_events(), [on(61, 127), off(61)]);
}

#[test]
fn dragging_across_the_keys_plays_each_in_turn_and_leaving_them_stops() {
    let (mut ui, state) = two_octaves(1.0);
    let c4 = ui.get(Role::Button, "C4");
    let e4 = ui.get(Role::Button, "E4");
    // Along the bottom, below the black keys.
    let y = c4.bounds.bottom() - 5.0;
    ui.press(Point::new(c4.center().x, y));
    for step in 1..=10 {
        let x = c4.center().x + (e4.center().x - c4.center().x) * step as f32 / 10.0;
        ui.move_to(Point::new(x, y));
    }
    assert_eq!(state.down(), [64]);
    // Off the keys, above them: the note stops; back on, another starts.
    ui.move_to(Point::new(e4.center().x, -20.0));
    assert!(state.down().is_empty());
    ui.move_to(Point::new(e4.center().x, y));
    ui.release(Point::new(e4.center().x, y));
    let notes: Vec<_> = state
        .take_events()
        .into_iter()
        .map(|event| match event {
            NoteEvent::On { note, .. } => (true, note),
            NoteEvent::Off { note } => (false, note),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(
        notes,
        [
            (true, 60),
            (false, 60),
            (true, 62),
            (false, 62),
            (true, 64),
            (false, 64),
            (true, 64),
            (false, 64),
        ]
    );
}

#[test]
fn the_letters_play_notes_once_each_and_stop_when_the_focus_goes() {
    let (mut ui, state) = two_octaves(1.0);
    assert!(!letter(&mut ui, 'a', true), "no focus, no notes");
    assert!(ui.tab());
    assert!(letter(&mut ui, 'a', true));
    assert!(letter(&mut ui, 'a', true), "the system's repeat");
    assert!(letter(&mut ui, 'w', true));
    assert_eq!(state.down(), [60, 61]);
    assert!(letter(&mut ui, 'a', false));
    // X moves the letters an octave up; a key held keeps its note.
    assert!(letter(&mut ui, 'x', true));
    letter(&mut ui, 'x', false);
    assert!(letter(&mut ui, 'k', true));
    // With Ctrl, a letter is left to the host.
    assert!(!ui.key_with(
        Key::Character('s'),
        Modifiers {
            control: true,
            ..Modifiers::NONE
        }
    ));
    // The window loses the keyboard: every note stops.
    ui.send(manguebyte_ui::Event::Focus(false));
    assert!(state.down().is_empty());
    assert_eq!(
        state.take_events(),
        [
            on(60, 100),
            on(61, 100),
            off(60),
            on(84, 100),
            off(61),
            off(84)
        ]
    );
}

#[test]
fn the_letters_can_be_turned_off_and_space_plays_the_key_the_keyboard_is_on() {
    let state = KeyboardState::new();
    let view = piano_keyboard("Keys", KeySpan::new(48, 71), state.clone())
        .computer_keys(false)
        .letter_velocity(90);
    let mut ui = Harness::new(view, SIZE, 1.0);
    ui.frame();
    assert!(ui.tab());
    assert!(!letter(&mut ui, 'a', true));
    // The keyboard starts on middle C; Right and Up move it.
    assert!(ui.key(Key::Right));
    assert!(ui.key(Key::Down));
    ui.frame();
    assert!(ui.get(Role::Button, "C#3").is_focused());
    assert!(letter(&mut ui, ' ', true));
    assert_eq!(state.down(), [49]);
    assert!(letter(&mut ui, ' ', false));
    assert_eq!(state.take_events(), [on(49, 90), off(49)]);
}

#[test]
fn keys_the_application_lights_are_drawn_lit_and_said_to_sound() {
    let palette = manguebyte_ui::Theme::dark().palette;
    let (accent, white) = (palette.accent, palette.key_white);
    for scale in [1.0, 1.25, 2.0] {
        let state = KeyboardState::new();
        let view = row((
            label("Beside").width(40.0),
            piano_keyboard("Keys", KeySpan::new(48, 71), state.clone()).flex(1.0),
        ));
        let mut ui = Harness::new(view, Size::new(320.0, 90.0), scale);
        ui.frame();
        let keys = ui.get(Role::Group, "Keys");
        let pixel = |ui: &Harness, at: Point| {
            ui.pixels()
                .pixel((at.x * scale) as u32, (at.y * scale) as u32)
                .map(|p| p.to_array())
        };
        let rgb = |c: Color| Some([c.r, c.g, c.b, 255]);
        let d4 = ui.get(Role::Button, "D4");
        let low = Point::new(d4.center().x, d4.bounds.bottom() - 4.0);
        assert_eq!(pixel(&ui, low), rgb(white), "{scale}x");
        state.set_sounding([62, 66]);
        let summary = ui.frame();
        assert!(summary.painted > 0);
        assert_eq!(pixel(&ui, low), rgb(accent), "{scale}x");
        assert_eq!(
            ui.get(Role::Button, "D4").value_text(),
            Some("up, sounding")
        );
        assert!(
            ui.last_damage()
                .iter()
                .all(|damage| keys.device.contains_rect(damage)),
            "{scale}x: {:?}",
            ui.last_damage()
        );
        assert!(ui.pixels() == &ui.render_fresh(), "{scale}x lit");
        // Lighting the same notes again draws nothing.
        state.set_sounding([66, 62]);
        assert_eq!(ui.frame().damaged, 0);
        // A press, a range and the focus, each the same as a whole repaint.
        let c4 = ui.get(Role::Button, "C4").center();
        ui.press(c4);
        ui.frame();
        assert!(ui.pixels() == &ui.render_fresh(), "{scale}x pressed");
        ui.release(c4);
        state.set_ranges(vec![KeyRange::new(50, 57).with_root(53)]);
        ui.frame();
        assert!(ui.pixels() == &ui.render_fresh(), "{scale}x ranges");
        assert_eq!(ui.frames_rendered_during(|ui| ui.advance_ms(200)), 0);
    }
}

#[test]
fn white_keys_are_parted_by_whole_device_pixels_at_every_scale() {
    let palette = manguebyte_ui::Theme::dark().palette;
    let white = palette.key_white;
    for scale in [1.0, 1.25, 2.0] {
        let state = KeyboardState::new();
        let mut ui = Harness::new(
            piano_keyboard("Piano", KeySpan::PIANO, state),
            Size::new(1040.0, 100.0),
            scale,
        );
        ui.frame();
        // Along a row below the black keys, every pixel is a key's white
        // or the gap's colour, the background, never a blend of them.
        let y = (80.0 * scale) as u32;
        let track = palette.background;
        let width = (1040.0 * scale) as u32;
        let mut gaps = 0;
        for x in 0..width {
            let p = ui.pixels().pixel(x, y).map(|p| p.to_array());
            if p == Some([track.r, track.g, track.b, 255]) {
                gaps += 1;
            } else {
                assert_eq!(p, Some([white.r, white.g, white.b, 255]), "{scale}x at {x}");
            }
        }
        // 52 white keys: a gap of one device pixel after each.
        assert_eq!(gaps, 52, "{scale}x");
    }
}

#[test]
fn ranges_are_marked_and_said() {
    let (mut ui, state) = two_octaves(1.0);
    state.set_ranges(vec![KeyRange::new(50, 57).with_root(53)]);
    ui.frame();
    assert_eq!(
        ui.get(Role::Button, "D3").value_text(),
        Some("up, in a range")
    );
    assert_eq!(
        ui.get(Role::Button, "F3").value_text(),
        Some("up, root of a range")
    );
    assert_eq!(
        ui.get(Role::Button, "A3").value_text(),
        Some("up, in a range")
    );
    assert_eq!(ui.get(Role::Button, "B3").value_text(), Some("up"));
    // The strip above the root key is the accent.
    let focus = manguebyte_ui::Theme::dark().palette.accent;
    let f3 = ui.get(Role::Button, "F3");
    let column = KeySpan::new(48, 71)
        .column(manguebyte_gfx::Rect::new(0.0, 0.0, 280.0, 8.0), 53)
        .expect("in the span");
    let p = ui
        .pixels()
        .pixel(column.center().x as u32, 4)
        .map(|p| p.to_array());
    assert_eq!(p, Some([focus.r, focus.g, focus.b, 255]));
    assert!(f3.bounds.x <= column.x && column.right() <= f3.bounds.right());
}

#[test]
fn assistive_technology_names_every_key_and_holds_them_down() {
    let state = KeyboardState::new();
    let mut ui = Harness::new(
        piano_keyboard("Piano", KeySpan::PIANO, state.clone()),
        Size::new(1040.0, 100.0),
        1.0,
    );
    ui.frame();
    ui.assert_accessible();
    let keys: Vec<_> = ui
        .everything()
        .into_iter()
        .filter(|found| found.role() == Role::Button)
        .collect();
    assert_eq!(keys.len(), 88);
    assert_eq!(keys[0].name(), "A0");
    assert_eq!(keys[87].name(), "C8");
    assert_eq!(
        ui.get(Role::Group, "Piano").value_text(),
        Some("A0 to C8; the letters play from C4")
    );
    let g4 = ui.get(Role::Button, "G4");
    assert_eq!(g4.node.state.checked, Some(false));
    assert!(ui.act(&g4, Action::Press));
    ui.frame();
    let g4 = ui.get(Role::Button, "G4");
    assert_eq!(g4.node.state.checked, Some(true));
    assert_eq!(g4.value_text(), Some("down"));
    assert!(ui.act(&g4, Action::Press));
    assert_eq!(state.take_events(), [on(67, 100), off(67)]);
    // Names of the application's own.
    let view = piano_keyboard("Teclado", KeySpan::new(60, 72), KeyboardState::new())
        .note_names(note_name_portuguese);
    let mut ui = Harness::new(view, Size::new(200.0, 80.0), 1.0);
    ui.frame();
    assert!(ui.find(Role::Button, "Dó4").is_some());
    assert!(ui.find(Role::Button, "Fá#4").is_some());
    ui.assert_accessible();
}

#[test]
fn closing_the_interface_stops_what_it_holds() {
    let (mut ui, state) = two_octaves(1.0);
    let a3 = ui.get(Role::Button, "A3");
    ui.press(a3.center());
    drop(ui);
    let events = state.take_events();
    assert_eq!(events.last(), Some(&off(57)));
    assert!(state.down().is_empty());
}

#[test]
fn the_keys_line_up_with_the_columns_of_the_same_span() {
    let (ui, _) = two_octaves(1.0);
    let span = KeySpan::new(48, 71);
    let keys = manguebyte_gfx::Rect::new(0.0, 8.0, 280.0, 82.0);
    for note in span.notes() {
        let column = span.column(keys, note).expect("in the span");
        let key = ui.get(Role::Button, &manguebyte_ui::note_name(note));
        // A white key holds its column; a black key sits inside its own.
        let (outer, inner) = if manguebyte_ui::is_black(note) {
            (column, key.bounds)
        } else {
            (key.bounds, column)
        };
        assert!(
            outer.x <= inner.x + 0.01 && inner.right() <= outer.right() + 0.01,
            "{note}: {:?} {:?}",
            key.bounds,
            column
        );
    }
}
