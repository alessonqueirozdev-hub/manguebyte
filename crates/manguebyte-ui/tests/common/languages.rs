//! A screen whose every name follows the language the person picks, as
//! Parabolic's do: a knob, a slider, a switch, a combo box, a list, a
//! zone map, a waveform view and a keyboard, all named from one table in
//! English, Portuguese and Spanish, chosen by one `State<String>`.

use std::sync::Arc;

use manguebyte_gfx::Size;
use manguebyte_params::{BoolParam, ChoiceParam, FloatParam, FloatRange, ParamSet, param_set};
use manguebyte_ui::layout::Align;
use manguebyte_ui::testing::Harness;
use manguebyte_ui::{
    IntoView, KeySpan, KeyboardState, ListData, ListState, ParamHandle, Role, State, TextSource,
    WaveformState, ZoneMapState, column, combo, knob, list_view_named, piano_keyboard_named, row,
    slider, toggle, waveform_view_named, zone_map_named,
};

pub struct LanguageParams {
    pub cutoff: FloatParam,
    pub mix: FloatParam,
    pub bypass: BoolParam,
    pub shape: ChoiceParam,
}

param_set!(LanguageParams {
    cutoff,
    mix,
    bypass,
    shape
});

pub fn language_params() -> Arc<LanguageParams> {
    Arc::new(LanguageParams {
        cutoff: FloatParam::new("cutoff", "cutoff", FloatRange::linear(0.0, 100.0), 50.0)
            .with_precision(0),
        mix: FloatParam::new("mix", "mix", FloatRange::linear(0.0, 100.0), 100.0)
            .with_unit("%")
            .with_precision(0),
        bypass: BoolParam::new("bypass", "bypass", false),
        shape: ChoiceParam::new("shape", "shape", &["Sine", "Saw"], 0),
    })
}

/// The languages, in the table's order.
pub const LANGUAGES: [&str; 3] = ["en", "pt", "es"];

/// Each text in each language. No text is the same in two languages, so
/// that every switch changes every name.
pub const TEXTS: &[(&str, [&str; 3])] = &[
    ("cutoff", ["Cutoff", "Frequência", "Frecuencia"]),
    ("mix", ["Mix", "Mistura", "Mezcla"]),
    ("bypass", ["Bypass", "Contornar", "Omitir"]),
    ("shape", ["Shape", "Formato", "Forma"]),
    ("samples", ["Samples", "Amostras", "Muestras"]),
    (
        "zones",
        ["Key zones", "Zonas de teclas", "Zonas de teclado"],
    ),
    (
        "waveform",
        ["Waveform", "Forma de onda", "Forma de la onda"],
    ),
    (
        "keyboard",
        ["Keyboard", "Teclado do piano", "Teclado del piano"],
    ),
];

/// The keys of the texts each widget is named by, its role, and whether
/// it draws its name.
pub const WIDGETS: [(&str, Role, bool); 8] = [
    ("cutoff", Role::Slider, true),
    ("mix", Role::Slider, true),
    ("bypass", Role::Switch, true),
    ("shape", Role::ComboBox, true),
    ("samples", Role::List, false),
    ("zones", Role::List, false),
    ("waveform", Role::Group, false),
    ("keyboard", Role::Group, false),
];

/// Text `key` in `language` ("en", "pt" or "es"; English for anything
/// else). Looked up without touching the heap.
pub fn translate(language: &str, key: &str) -> &'static str {
    let column = LANGUAGES
        .iter()
        .position(|code| *code == language)
        .unwrap_or(0);
    TEXTS
        .iter()
        .find(|(name, _)| *name == key)
        .map_or("", |(_, texts)| texts[column])
}

/// Text `key` in the language `language` holds, read once per frame.
pub fn text(language: &State<String>, key: &'static str) -> TextSource {
    let language = language.clone();
    TextSource::dynamic(move || language.with(|code| translate(code, key)))
}

struct Samples;

impl ListData for Samples {
    fn len(&self) -> usize {
        3
    }

    fn name(&self, row: usize) -> String {
        ["Kick", "Snare", "Hat"][row].to_owned()
    }

    fn revision(&self) -> u64 {
        0
    }
}

pub const SCREEN: Size = Size::new(640.0, 420.0);

/// The screen, its controls in a row at the top and the others below.
pub fn screen(params: &Arc<LanguageParams>, language: &State<String>) -> Harness {
    let handle = |f: fn(&LanguageParams) -> &FloatParam| ParamHandle::new(Arc::clone(params), f);
    let span = KeySpan::new(48, 71);
    let view = column((
        row((
            knob(handle(|p| &p.cutoff)).label_source(text(language, "cutoff")),
            slider(handle(|p| &p.mix)).label_source(text(language, "mix")),
            toggle(ParamHandle::new(Arc::clone(params), |p| &p.bypass))
                .label_source(text(language, "bypass")),
            combo(ParamHandle::new(Arc::clone(params), |p| &p.shape))
                .label_source(text(language, "shape")),
        ))
        .spacing(8.0)
        .align(Align::Start),
        row((
            list_view_named(text(language, "samples"), Samples, ListState::new()).width(140.0),
            zone_map_named(text(language, "zones"), span, ZoneMapState::new()).width(216.0),
            waveform_view_named(text(language, "waveform"), WaveformState::new()).flex(1.0),
        ))
        .spacing(8.0)
        .flex(1.0),
        piano_keyboard_named(text(language, "keyboard"), span, KeyboardState::new()),
    ))
    .spacing(8.0)
    .padding(8.0);
    let mut ui = Harness::with_params(view, SCREEN, 2.0, Arc::clone(params) as Arc<dyn ParamSet>);
    ui.frame();
    ui
}
