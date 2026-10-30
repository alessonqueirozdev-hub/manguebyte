//! Action buttons, driven headless: presses by every means, the disabled
//! state, the looks from the theme's tokens, motion, and labels that
//! follow the language.

use std::cell::Cell;
use std::rc::Rc;

use manguebyte_gfx::{Size, Stroke};
use manguebyte_ui::access::Role;
use manguebyte_ui::testing::Harness;
use manguebyte_ui::{
    Action, Icon, IntoView, Key, State, TextSource, Theme, View, action_button, row,
};

/// How a scene dump writes `color`.
fn hex(color: manguebyte_gfx::Color) -> String {
    format!(
        "#{:02x}{:02x}{:02x}{:02x}",
        color.r, color.g, color.b, color.a
    )
}

/// A cross, drawn as two strokes.
fn cross() -> Icon {
    Icon::new(|scene, square, color| {
        let mut path = manguebyte_gfx::Path::new();
        let r = square.outset(-3.0);
        path.move_to(r.origin())
            .line_to(manguebyte_gfx::Point::new(r.right(), r.bottom()))
            .move_to(manguebyte_gfx::Point::new(r.right(), r.y))
            .line_to(manguebyte_gfx::Point::new(r.x, r.bottom()));
        scene.stroke(&path, Stroke::round(1.6), color);
    })
}

struct Counts {
    save: State<u64>,
    discard: State<u64>,
    delete: State<u64>,
    later: State<u64>,
}

fn counts() -> Counts {
    Counts {
        save: State::new(0),
        discard: State::new(0),
        delete: State::new(0),
        later: State::new(0),
    }
}

/// A row of four buttons: secondary, primary, destructive with an icon,
/// and one disabled until `ready` is set.
fn buttons(counts: &Counts, ready: &Rc<Cell<bool>>) -> View {
    let ready = Rc::clone(ready);
    row((
        action_button("Discard").presses(counts.discard.clone()),
        action_button("Save").primary().presses(counts.save.clone()),
        action_button("Delete")
            .destructive()
            .icon(cross())
            .presses(counts.delete.clone()),
        action_button("Later")
            .presses(counts.later.clone())
            .enabled(move || ready.get()),
    ))
    .spacing(8.0)
    .padding(12.0)
}

fn open() -> (Harness, Counts, Rc<Cell<bool>>) {
    let counts = counts();
    let ready = Rc::new(Cell::new(false));
    let mut ui = Harness::new(buttons(&counts, &ready), Size::new(460.0, 70.0), 1.0);
    ui.frame();
    (ui, counts, ready)
}

#[test]
fn a_button_is_pressed_by_a_click_the_keys_and_assistive_technology() {
    let (mut ui, counts, _) = open();
    let save = ui.get(Role::Button, "Save");
    ui.click(save.center());
    assert_eq!(counts.save.get(), 1);
    assert!(
        ui.get(Role::Button, "Save").is_focused(),
        "a click focuses it"
    );
    ui.key(Key::Enter);
    ui.key(Key::Character(' '));
    assert_eq!(counts.save.get(), 3);
    assert!(ui.act(&save, Action::Press));
    assert_eq!(counts.save.get(), 4);
    // A press let go outside does nothing.
    ui.press(save.center());
    ui.release(manguebyte_gfx::Point::new(
        save.bounds.x - 30.0,
        save.center().y,
    ));
    assert_eq!(counts.save.get(), 4);
    // Tab goes through the usable buttons in order.
    ui.frame();
    assert!(ui.tab());
    assert_eq!(
        ui.focused().map(|f| f.name().to_owned()).as_deref(),
        Some("Delete")
    );
    ui.key(Key::Enter);
    assert_eq!(counts.delete.get(), 1);
    assert_eq!(counts.discard.get(), 0);
    ui.assert_accessible();
}

#[test]
fn a_disabled_button_cannot_be_used_until_it_is_enabled() {
    let (mut ui, counts, ready) = open();
    let later = ui.get(Role::Button, "Later");
    assert!(later.node.state.disabled);
    assert!(!later.node.actions.press && !later.node.actions.focus);
    ui.click(later.center());
    assert!(!ui.act(&later, Action::Press));
    assert!(!ui.act(&later, Action::Focus));
    assert_eq!(counts.later.get(), 0);
    // Tab passes it: from Delete, round to Discard.
    ui.act(&ui.get(Role::Button, "Delete"), Action::Focus);
    assert!(ui.tab());
    assert_eq!(
        ui.focused().map(|f| f.name().to_owned()).as_deref(),
        Some("Discard")
    );
    // Drawn faded.
    assert!(ui.dump_of(&later).contains("layer opacity 0.45 {"));
    ui.assert_accessible();

    // Enabled at run time: drawn again, told, usable.
    ui.frame();
    ready.set(true);
    let frame = ui.frame();
    assert_eq!(frame.painted, 1);
    let later = ui.get(Role::Button, "Later");
    assert!(!later.node.state.disabled && later.node.actions.press);
    assert!(ui.access_updates().iter().any(|f| f.name() == "Later"));
    ui.click(later.center());
    assert_eq!(counts.later.get(), 1);
    assert!(!ui.dump_of(&later).contains("layer "));
}

#[test]
fn the_three_looks_come_from_the_theme() {
    for theme in [Theme::dark(), Theme::light()] {
        let (mut ui, ..) = open();
        ui.set_theme(theme);
        ui.frame();
        let palette = theme.palette;
        let dump = |name: &str| ui.dump_of(&ui.get(Role::Button, name));
        // Secondary: the raised face, a hairline.
        assert!(dump("Discard").contains(&hex(palette.raised)));
        // Primary: the accent's gradient, or ink.
        let primary = if theme.elevation.glow.color.a > 0 {
            palette.accent
        } else {
            palette.text
        };
        assert!(dump("Save").contains(&hex(primary)));
        // Destructive: the danger colour, its text on it, and its icon.
        let delete = dump("Delete");
        assert!(delete.contains(&hex(palette.danger)));
        assert!(delete.contains(&hex(palette.on_accent)));
        assert!(delete.contains("stroke path("));
        ui.assert_accessible();
    }
}

#[test]
fn hover_press_and_focus_move_as_the_other_widgets_do() {
    let (mut ui, ..) = open();
    ui.set_motion(true);
    ui.advance_ms(100);
    let save = ui.get(Role::Button, "Save");
    let near = |rect: &manguebyte_gfx::IRect| {
        // The button and the reach of its shadow.
        let reach = 40;
        rect.x >= save.device.x - reach
            && rect.right() <= save.device.right() + i64::from(reach)
            && rect.bottom() <= save.device.bottom() + i64::from(reach)
    };
    ui.move_to(save.center());
    let mut frames = 0;
    while {
        ui.advance_ms(16);
        ui.root().is_animating()
    } {
        frames += 1;
        assert!(ui.last_damage().iter().all(near), "{:?}", ui.last_damage());
        assert!(frames < 30);
    }
    // The fast duration, 120 ms, in frames of 16 ms.
    assert!((5..=9).contains(&frames), "{frames}");
    let hovered = ui.dump_of(&ui.get(Role::Button, "Save"));
    // Held down, it settles and darkens.
    ui.press(save.center());
    ui.advance_ms(200);
    let held = ui.dump_of(&ui.get(Role::Button, "Save"));
    assert_ne!(hovered, held);
    ui.release(save.center());
    ui.advance_ms(200);
    assert_eq!(ui.frame().damaged, 0, "settled");
    // A press showed no ring; the keyboard does, eased in.
    let ring = hex(ui.root().theme().palette.focus);
    assert!(ui.get(Role::Button, "Save").is_focused());
    assert!(!ui.dump_of(&ui.get(Role::Button, "Save")).contains(&ring));
    assert!(ui.tab());
    ui.advance_ms(300);
    let focused = ui.focused().expect("focused");
    assert_eq!(focused.name(), "Delete");
    assert!(ui.dump_of(&focused).contains(&ring));

    // With motion off a hover is one frame.
    ui.set_motion(false);
    let discard = ui.get(Role::Button, "Discard");
    ui.move_to(discard.center());
    let frame = ui.frame();
    assert!(!frame.animating && frame.damaged > 0);
    assert_eq!(ui.frame().damaged, 0);
}

#[test]
fn a_label_that_follows_the_language_renames_the_button_in_the_next_frame() {
    let language = State::new(0_usize);
    let text = |words: [&'static str; 2]| {
        let language = language.clone();
        TextSource::dynamic(move || words[language.get()])
    };
    let view = row((
        action_button(text(["Save", "Salvar"])).primary(),
        action_button(text(["Cancel", "Cancelar"])),
    ))
    .padding(8.0);
    let mut ui = Harness::new(view, Size::new(300.0, 60.0), 1.0);
    ui.frame();
    let save = ui.get(Role::Button, "Save");
    language.set(1);
    let frame = ui.frame();
    assert!(frame.laid_out, "a longer label is laid out again");
    let salvar = ui.get(Role::Button, "Salvar");
    assert_eq!(salvar.id, save.id);
    assert!(salvar.bounds.width > save.bounds.width);
    assert!(ui.dump_of(&salvar).contains("text \"Salvar\""));
    assert!(ui.find(Role::Button, "Cancelar").is_some());
    let told: Vec<String> = ui
        .access_updates()
        .iter()
        .map(|f| f.name().to_owned())
        .collect();
    assert_eq!(told, ["Salvar", "Cancelar"]);
    let frame = ui.frame();
    assert_eq!((frame.damaged, frame.laid_out), (0, false));
}

#[test]
fn a_button_given_less_room_stays_inside_its_rectangle() {
    // Squeezed, its text is cut and nothing reaches its neighbours.
    let view = row((
        action_button("A rather long label").width(60.0),
        action_button("Next").into_view(),
    ))
    .spacing(4.0);
    let mut ui = Harness::new(view, Size::new(200.0, 50.0), 2.0);
    ui.frame();
    let long = ui.get(Role::Button, "A rather long label");
    assert_eq!(long.bounds.width, 60.0);
    ui.move_to(long.center());
    ui.frame();
    assert!(ui.pixels() == &ui.render_fresh());
}
