//! Texts that change while the window is open: switching the language
//! renames every widget in the next frame, for the eye and for the screen
//! reader, draws again only the widgets that show their names, and lays
//! out again when a name grows. That the interface is idle again after a
//! switch is in `motion.rs`, and that steady frames touch no heap in
//! `realtime.rs`.

mod common;

use std::sync::Arc;

use manguebyte_gfx::{Size, TextLayout, TextStyle};
use manguebyte_params::{FloatParam, ParamSet};
use manguebyte_ui::testing::{Found, Harness};
use manguebyte_ui::{
    IntoView, Label, ParamHandle, Role, State, TextSource, Theme, View, combo, knob, row, toggle,
};

use common::languages::{LanguageParams, WIDGETS, language_params, screen, translate};

/// Each widget of the screen, found by its name in `language`, with
/// whether it draws its name.
fn widgets(ui: &Harness, language: &str) -> Vec<(Found, bool)> {
    WIDGETS
        .iter()
        .map(|&(key, role, drawn)| (ui.get(role, translate(language, key)), drawn))
        .collect()
}

/// Whether the scene `found` recorded draws `text` as one run.
fn draws(ui: &Harness, found: &Found, text: &str) -> bool {
    let needle = format!("text \"{}\"", text.escape_debug());
    ui.dump_of(found).contains(&needle)
}

#[test]
fn switching_the_language_renames_every_widget_in_the_next_frame() {
    let params = language_params();
    let language = State::new(String::from("en"));
    let mut ui = screen(&params, &language);
    ui.advance_ms(100);
    assert!(ui.access_updates().is_empty(), "nothing changed yet");
    for (found, drawn) in widgets(&ui, "en") {
        assert_eq!(draws(&ui, &found, found.name()), drawn, "{}", found.name());
    }

    for (from, to) in [("en", "pt"), ("pt", "es"), ("es", "en")] {
        let before = widgets(&ui, from);
        language.set(to.to_owned());
        let frame = ui.frame();
        assert!(frame.laid_out, "{from} to {to}: names lay out again");
        // Only the four controls that draw their names drew anything.
        assert_eq!(frame.painted, 4, "{from} to {to}");
        let after = widgets(&ui, to);
        for ((old, _), (new, drawn)) in before.iter().zip(&after) {
            assert_eq!(old.id, new.id);
            assert!(
                ui.find(old.role(), old.name()).is_none(),
                "{from} to {to}: {:?} is still there",
                old.name()
            );
            if *drawn {
                assert!(draws(&ui, new, new.name()), "{:?} is not drawn", new.name());
                assert!(!draws(&ui, new, old.name()), "{:?} is drawn", old.name());
            } else {
                // Not drawn, so not damaged.
                for rect in ui.last_damage() {
                    assert!(
                        rect.intersect(&new.device).is_none(),
                        "{from} to {to}: {rect:?} reaches {:?} {:?}",
                        new.name(),
                        new.device
                    );
                }
            }
        }
        // And the screen reader hears about all eight, with their new names.
        let told: Vec<(Role, String)> = ui
            .access_updates()
            .iter()
            .map(|found| (found.role(), found.name().to_owned()))
            .collect();
        let expected: Vec<(Role, String)> = WIDGETS
            .iter()
            .map(|&(key, role, _)| (role, translate(to, key).to_owned()))
            .collect();
        assert_eq!(told, expected, "{from} to {to}");
        ui.assert_accessible();

        // The next frame has nothing to say, and draws nothing.
        let frame = ui.frame();
        assert_eq!(
            (frame.damaged, frame.painted, frame.laid_out),
            (0, 0, false)
        );
        assert!(ui.access_updates().is_empty());
    }
}

/// The width the theme's label style gives `text` on one line.
fn width_of(text: &str, style: &TextStyle) -> f32 {
    TextLayout::new(text, style, None).size().width
}

/// A knob, a switch and a combo box named by `name`, and a label showing
/// `caption`, side by side.
fn named_row(params: &Arc<LanguageParams>, name: &State<String>, caption: &State<String>) -> View {
    let handle = |f: fn(&LanguageParams) -> &FloatParam| ParamHandle::new(Arc::clone(params), f);
    row((
        knob(handle(|p| &p.cutoff)).label_source(name.clone()),
        toggle(ParamHandle::new(Arc::clone(params), |p| &p.bypass))
            .label_source(TextSource::from_state(name.clone())),
        combo(ParamHandle::new(Arc::clone(params), |p| &p.shape)).label_source(name.clone()),
        Label::from_state(caption.clone()),
    ))
    .spacing(4.0)
    .padding(4.0)
}

#[test]
fn a_name_that_grows_lays_out_again_without_overlapping_its_neighbours() {
    let params = language_params();
    let style = Theme::default().typography.label;
    let grown = "Ganho de entrada do pré-amplificador";
    let longer = "Ligado, com o sinal passando direto";
    for scale in [1.0, 1.25, 2.0] {
        let name = State::new(String::from("Gain"));
        let caption = State::new(String::from("On"));
        let mut ui = Harness::with_params(
            named_row(&params, &name, &caption),
            Size::new(1400.0, 120.0),
            scale,
            Arc::clone(&params) as Arc<dyn ParamSet>,
        );
        ui.frame();
        let switch_before = ui.get(Role::Switch, "Gain").bounds;
        name.set(grown.to_owned());
        caption.set(longer.to_owned());
        assert!(ui.frame().laid_out, "at {scale}x");
        let found = [
            ui.get(Role::Slider, grown),
            ui.get(Role::Switch, grown),
            ui.get(Role::ComboBox, grown),
            ui.get(Role::Label, longer),
        ];
        assert!(
            found[1].bounds.x > switch_before.x,
            "at {scale}x the switch moved over for the knob's longer name"
        );
        for (index, widget) in found.iter().enumerate() {
            // Wide enough for the whole name, which is drawn.
            assert!(
                widget.bounds.width >= width_of(widget.name(), &style),
                "at {scale}x {:?} is {} wide",
                widget.role(),
                widget.bounds.width
            );
            assert!(draws(&ui, widget, widget.name()), "at {scale}x");
            for other in &found[index + 1..] {
                assert!(
                    widget.bounds.intersect(&other.bounds).is_none(),
                    "at {scale}x {:?} {:?} overlaps {:?} {:?}",
                    widget.role(),
                    widget.bounds,
                    other.role(),
                    other.bounds
                );
            }
        }
    }
}
