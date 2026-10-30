//! The confirmation dialog inside the window, driven headless: opening
//! it, the focus trapped inside and given back, Escape and Enter, the
//! pointer and assistive technology kept inside, a closed dialog costing
//! nothing, motion, and texts that follow the language while it is open.

use std::cell::Cell;
use std::rc::Rc;

use manguebyte_gfx::Size;
use manguebyte_ui::access::Role;
use manguebyte_ui::testing::Harness;
use manguebyte_ui::{
    Action, DialogResult, DialogState, IntoView, Key, Modifiers, State, TextSource, View,
    action_button, column, confirm_dialog, label, row,
};

const SHIFT: Modifiers = Modifiers {
    shift: true,
    ..Modifiers::NONE
};

/// A screen with a Save button, a Delete button that opens the dialog,
/// and the dialog over all of it.
fn screen(ask: &DialogState, saves: &State<u64>, cancel_default: bool) -> View {
    let dialog = confirm_dialog(
        ask.clone(),
        "Delete the preset?",
        "It is removed from the library. This cannot be undone.",
    )
    .confirm_label("Delete")
    .cancel_label("Keep")
    .destructive();
    let dialog = if cancel_default {
        dialog.cancel_by_default()
    } else {
        dialog
    };
    column((
        label("Presets").title(),
        row((
            action_button("Save").presses(saves.clone()),
            action_button("Delete preset").destructive().opens(ask),
        ))
        .spacing(8.0),
    ))
    .spacing(10.0)
    .padding(16.0)
    .with_dialog(dialog)
}

fn open(cancel_default: bool) -> (Harness, DialogState, State<u64>) {
    let ask = DialogState::new();
    let saves = State::new(0);
    let mut ui = Harness::new(
        screen(&ask, &saves, cancel_default),
        Size::new(560.0, 360.0),
        1.0,
    );
    ui.frame();
    (ui, ask, saves)
}

fn focused(ui: &Harness) -> Option<String> {
    ui.focused().map(|found| found.name().to_owned())
}

#[test]
fn the_dialog_traps_the_focus_and_gives_it_back_to_its_opener() {
    let (mut ui, ask, saves) = open(false);
    // From the keyboard: Tab to the Delete preset button, Enter.
    assert!(ui.tab());
    assert!(ui.tab());
    assert_eq!(focused(&ui).as_deref(), Some("Delete preset"));
    ui.key(Key::Enter);
    assert!(ask.is_open());
    ui.frame();
    assert!(ui.root().modal().is_some());
    // The default button has the focus.
    assert_eq!(focused(&ui).as_deref(), Some("Delete"));
    // Tab and Shift+Tab go round the dialog's two buttons only.
    for expected in ["Keep", "Delete", "Keep"] {
        assert!(ui.tab());
        assert_eq!(focused(&ui).as_deref(), Some(expected));
    }
    assert!(ui.shift_tab());
    assert_eq!(focused(&ui).as_deref(), Some("Delete"));
    // Nothing behind it can be focused or used.
    let save = ui.get(Role::Button, "Save");
    assert!(!ui.root_mut().set_focus_to(Some(save.id)));
    assert!(!ui.act(&save, Action::Press));
    assert!(!ui.act(&save, Action::Focus));
    ui.click(save.center());
    assert_eq!(saves.get(), 0);
    assert!(ask.is_open(), "a press on the scrim does not answer");
    assert_eq!(focused(&ui).as_deref(), Some("Delete"));
    // Every key is the dialog's while it is open, used or not.
    assert!(ui.key(Key::Character('x')));
    assert!(ui.key(Key::Function(5)));

    // Escape cancels; the next frame closes it and gives the focus back.
    assert!(ui.key(Key::Escape));
    assert_eq!(ask.result(), Some(DialogResult::Cancelled));
    ui.frame();
    assert!(ui.find(Role::Dialog, "Delete the preset?").is_none());
    assert!(ui.root().modal().is_none());
    assert_eq!(focused(&ui).as_deref(), Some("Delete preset"));
    assert_eq!(ask.take_result(), Some(DialogResult::Cancelled));
    assert_eq!(ask.take_result(), None, "taken once");
    // The window is usable again.
    ui.click(ui.get(Role::Button, "Save").center());
    assert_eq!(saves.get(), 1);
    assert!(!ui.key(Key::Function(5)), "keys go to the host again");
}

#[test]
fn enter_presses_the_focused_button_or_the_default_one() {
    // The confirming button is the default: Enter on it confirms.
    let (mut ui, ask, _) = open(false);
    ask.open();
    ui.frame();
    ui.key(Key::Enter);
    assert_eq!(ask.take_result(), Some(DialogResult::Confirmed));
    ui.frame();
    // Enter on the cancelling button cancels.
    ask.open();
    ui.frame();
    ui.tab();
    assert_eq!(focused(&ui).as_deref(), Some("Keep"));
    ui.key(Key::Enter);
    assert_eq!(ask.take_result(), Some(DialogResult::Cancelled));
    ui.frame();
    // With nothing focused, Enter is the dialog's own and means the
    // default; so is a click on its button.
    ask.open();
    ui.frame();
    assert!(ui.root_mut().set_focus_to(None));
    ui.key(Key::Enter);
    assert_eq!(ask.take_result(), Some(DialogResult::Confirmed));
    ui.frame();
    ask.open();
    ui.frame();
    ui.click(ui.get(Role::Button, "Delete").center());
    assert_eq!(ask.take_result(), Some(DialogResult::Confirmed));
    ui.frame();
    // Assistive technology presses the buttons too.
    ask.open();
    ui.frame();
    let keep = ui.get(Role::Button, "Keep");
    assert!(ui.act(&keep, Action::Press));
    assert_eq!(ask.take_result(), Some(DialogResult::Cancelled));

    // Cancelling by default: the focus starts on Keep, and Enter with
    // nothing focused cancels.
    let (mut ui, ask, _) = open(true);
    ask.open();
    ui.frame();
    assert_eq!(focused(&ui).as_deref(), Some("Keep"));
    ui.root_mut().set_focus_to(None);
    ui.key(Key::Enter);
    assert_eq!(ask.take_result(), Some(DialogResult::Cancelled));
}

#[test]
fn the_dialog_is_described_as_a_dialog_and_closed_is_not_there() {
    let (mut ui, ask, _) = open(false);
    assert!(ui.find(Role::Dialog, "Delete the preset?").is_none());
    assert!(ui.find(Role::Button, "Keep").is_none());
    ui.assert_accessible();
    ask.open();
    ui.frame();
    let dialog = ui.get(Role::Dialog, "Delete the preset?");
    assert_eq!(
        dialog.node.description.as_deref(),
        Some("It is removed from the library. This cannot be undone.")
    );
    // It covers the window; its buttons are on its card, inside it.
    assert_eq!(dialog.bounds.width, 560.0);
    for name in ["Keep", "Delete"] {
        let button = ui.get(Role::Button, name);
        assert!(dialog.bounds.contains(button.center()), "{name}");
    }
    let keep = ui.get(Role::Button, "Keep");
    let delete = ui.get(Role::Button, "Delete");
    assert!(
        keep.bounds.right() <= delete.bounds.x,
        "confirm on the right"
    );
    assert!(
        ui.find(
            Role::Label,
            "It is removed from the library. This cannot be undone."
        )
        .is_some()
    );
    ui.assert_accessible();
}

#[test]
fn a_closed_dialog_costs_nothing() {
    let reads = Rc::new(Cell::new(0_u32));
    let counted = {
        let reads = Rc::clone(&reads);
        TextSource::dynamic(move || {
            reads.set(reads.get() + 1);
            "Quit?"
        })
    };
    let ask = DialogState::new();
    let view = label("Work").with_dialog(confirm_dialog(ask.clone(), counted, "Unsaved changes"));
    let mut ui = Harness::new(view, Size::new(400.0, 300.0), 1.0);
    ui.frame();
    let after_first = reads.get();
    for _ in 0..100 {
        let frame = ui.frame();
        assert!(!frame.animating);
        assert_eq!(frame.damaged, 0);
    }
    assert_eq!(
        reads.get(),
        after_first,
        "its texts are not polled while closed"
    );
    // Open, it is polled every frame; closed again, it stops.
    ask.open();
    ui.frame();
    let open_reads = reads.get();
    ui.frame();
    assert!(reads.get() > open_reads);
    ask.answer(DialogResult::Cancelled);
    ui.frame();
    let closed_reads = reads.get();
    for _ in 0..20 {
        assert_eq!(ui.frame().damaged, 0);
    }
    assert_eq!(reads.get(), closed_reads);
}

#[test]
fn opening_and_closing_draw_what_the_dialog_covers_and_nothing_after() {
    for scale in [1.0, 1.25, 2.0] {
        let ask = DialogState::new();
        let saves = State::new(0);
        let mut ui = Harness::new(screen(&ask, &saves, false), Size::new(560.0, 360.0), scale);
        ui.frame();
        ask.open();
        let frame = ui.frame();
        assert!(frame.damaged > 0 && frame.laid_out);
        assert!(ui.pixels() == &ui.render_fresh(), "{scale}: open");
        assert_eq!(ui.frame().damaged, 0, "{scale}: settled");
        ui.key(Key::Escape);
        ui.frame();
        assert!(ui.pixels() == &ui.render_fresh(), "{scale}: closed");
        assert_eq!(ui.frame().damaged, 0);
    }
}

#[test]
fn the_pointer_reaches_only_the_dialog() {
    let (mut ui, ask, _) = open(false);
    let save = ui.get(Role::Button, "Save");
    ask.open();
    ui.frame();
    // Over a button behind the scrim, nothing behind is hovered.
    ui.move_to(save.center());
    ui.frame();
    let hovered = ui.root().hovered().expect("the scrim is under the pointer");
    let dialog = ui.get(Role::Dialog, "Delete the preset?");
    assert_eq!(hovered, dialog.id);
    assert!(ui.wheel(save.center(), -1.0), "the scrim takes the wheel");
    // Over the dialog's button, it is.
    let keep = ui.get(Role::Button, "Keep");
    ui.move_to(keep.center());
    ui.frame();
    assert_eq!(ui.root().hovered(), Some(keep.id));
}

#[test]
fn the_scrim_fades_in_with_motion_and_jumps_without() {
    let (mut ui, ask, _) = open(false);
    ui.set_motion(true);
    ui.advance_ms(50);
    ask.open();
    let mut frames = 0;
    ui.frame();
    while ui.root().is_animating() && frames < 60 {
        ui.advance_ms(16);
        frames += 1;
    }
    // The base duration, 180 ms.
    assert!((8..=14).contains(&frames), "{frames}");
    assert_eq!(ui.frame().damaged, 0);
    ui.key(Key::Escape);
    ui.frame();
    ui.set_motion(false);
    ask.open();
    let frame = ui.frame();
    assert!(!frame.animating);
    assert_eq!(ui.frame().damaged, 0);
}

#[test]
fn an_open_dialog_follows_the_language_in_the_next_frame() {
    let language = State::new(0_usize);
    let text = |words: [&'static str; 2]| {
        let language = language.clone();
        TextSource::dynamic(move || words[language.get()])
    };
    let ask = DialogState::new();
    let view = column((action_button(text(["Quit", "Sair"])).opens(&ask),))
        .padding(12.0)
        .with_dialog(
            confirm_dialog(
                ask.clone(),
                text(["Quit without saving?", "Sair sem salvar?"]),
                text([
                    "Your changes will be lost.",
                    "Suas alterações serão perdidas.",
                ]),
            )
            .confirm_label(text(["Quit", "Sair"]))
            .cancel_label(text(["Stay", "Ficar"])),
        );
    let mut ui = Harness::new(view, Size::new(520.0, 320.0), 1.0);
    ui.frame();
    ask.open();
    ui.frame();
    ui.frame();
    language.set(1);
    let frame = ui.frame();
    assert!(frame.laid_out);
    let dialog = ui.get(Role::Dialog, "Sair sem salvar?");
    assert_eq!(
        dialog.node.description.as_deref(),
        Some("Suas alterações serão perdidas.")
    );
    assert!(ui.find(Role::Button, "Ficar").is_some());
    assert_eq!(ui.find_all(Role::Button, "Sair").len(), 2, "the opener too");
    assert!(ui.find(Role::Dialog, "Quit without saving?").is_none());
    assert!(ui.dump().contains("text \"Sair sem salvar?\""));
    let told: Vec<Role> = ui.access_updates().iter().map(|f| f.role()).collect();
    assert!(told.contains(&Role::Dialog), "{told:?}");
    assert_eq!(told.iter().filter(|&&r| r == Role::Button).count(), 3);
    let frame = ui.frame();
    assert_eq!((frame.damaged, frame.laid_out), (0, false));
}

#[test]
fn shift_tab_from_the_first_button_goes_to_the_last() {
    let (mut ui, ask, _) = open(true);
    ask.open();
    ui.frame();
    assert_eq!(focused(&ui).as_deref(), Some("Keep"));
    ui.key_with(Key::Tab, SHIFT);
    assert_eq!(focused(&ui).as_deref(), Some("Delete"));
}
