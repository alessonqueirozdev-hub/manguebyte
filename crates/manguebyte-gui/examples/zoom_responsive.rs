//! Zoom to fit and a responsive layout together: a small organ console
//! designed at 1200 by 760 logical pixels.
//!
//! ```text
//! cargo run -p manguebyte-gui --example zoom_responsive
//! ```
//!
//! Drag the window's border. Down to the breakpoint the console stays as it
//! was designed and only gets smaller, or larger, with bars in the theme's
//! colour on the axis with room to spare. Below 900 by 560, as when the
//! window is snapped to half a screen beside a score, the keyboards give way
//! to a miniature of each manual at its real size, which goes on showing the
//! notes held: both layouts read the same `KeyboardState`. Nothing is built
//! again as the layouts change, so the position of the knob stays too.
//! Windows only for now: elsewhere it says so and stops.

use manguebyte_gfx::{Color, Rect, RoundedRect, Scene, Size};
use manguebyte_gui::Editor;
use manguebyte_params::{FloatParam, FloatRange, param_set};
use manguebyte_plugin::editor::{HostSize, Resize};
use manguebyte_ui::{
    CanvasCx, IntoView, KeySpan, KeyboardState, ParamHandle, View, canvas, card, column, is_black,
    knob, label, piano_keyboard, responsive, row,
};
use manguebyte_window::App;
use std::sync::Arc;

/// Below this the program has its compact layout, at its real size.
const BREAKPOINT: Size = Size::new(900.0, 560.0);

const MANUALS: [&str; 3] = ["Swell", "Great", "Choir"];

struct Params {
    tremulant: FloatParam,
}
param_set!(Params { tremulant });

/// A manual as a strip of its keys, those held lit.
fn miniature(cx: &CanvasCx<'_>, scene: &mut Scene, state: &KeyboardState) {
    let span = KeySpan::KEYS_61;
    let bounds = cx.bounds();
    let keys = u32::from(span.high() - span.low()) + 1;
    let width = bounds.width / keys as f32;
    let palette = &cx.theme().palette;
    let track = RoundedRect::new(bounds, 2.0);
    scene.fill(track, palette.track);
    for note in span.low()..=span.high() {
        let x = bounds.x + f32::from(note - span.low()) * width;
        let key = Rect::new(x, bounds.y, width, bounds.height);
        if state.is_sounding(note) || state.down().contains(&note) {
            scene.fill(key, palette.accent);
        } else if is_black(note) {
            scene.fill(key, Color::BLACK.with_opacity(0.25));
        }
    }
}

/// The console as it was designed: the keyboards, one over the other.
fn full(keyboards: &[KeyboardState], params: &Arc<Params>) -> View {
    let manuals: Vec<View> = MANUALS
        .iter()
        .zip(keyboards)
        .map(|(&name, state)| {
            column((
                label(name).heading(),
                piano_keyboard(name, KeySpan::KEYS_61, state.clone()).flex(1.0),
            ))
            .spacing(6.0)
            .flex(1.0)
        })
        .collect();
    row((
        column(manuals).spacing(12.0).flex(1.0),
        card(knob(ParamHandle::new(Arc::clone(params), |p| &p.tremulant))).width(160.0),
    ))
    .spacing(12.0)
    .padding(16.0)
}

/// Half a screen: a miniature of each manual, at its real size.
fn compact(keyboards: &[KeyboardState], params: &Arc<Params>) -> View {
    let manuals: Vec<View> = MANUALS
        .iter()
        .zip(keyboards)
        .map(|(&name, state)| {
            let shown = state.clone();
            let version = state.clone();
            column((
                label(name),
                canvas(
                    format!("{name} miniature"),
                    Size::new(200.0, 20.0),
                    move |cx, scene| miniature(cx, scene, &shown),
                )
                .redraw_on(move || version.version()),
            ))
            .spacing(4.0)
        })
        .collect();
    column((
        column(manuals).spacing(12.0),
        knob(ParamHandle::new(Arc::clone(params), |p| &p.tremulant)),
    ))
    .spacing(16.0)
    .padding(16.0)
}

fn main() {
    let app = match App::new() {
        Ok(app) => app,
        Err(error) => {
            eprintln!("no application windows here: {error}");
            return;
        }
    };
    let params = Arc::new(Params {
        tremulant: FloatParam::new("tremulant", "Tremulant", FloatRange::linear(0.0, 1.0), 0.5)
            .with_precision(2),
    });
    let view_params = Arc::clone(&params);
    let editor = Editor::new(HostSize::new(1200, 760), move || {
        let keyboards: Vec<KeyboardState> = MANUALS.iter().map(|_| KeyboardState::new()).collect();
        responsive((
            full(&keyboards, &view_params),
            compact(&keyboards, &view_params),
        ))
        .below(BREAKPOINT)
    })
    .with_resize(Resize::Free {
        min: HostSize::new(360, 320),
        max: HostSize::new(3840, 2160),
    })
    .with_zoom_to_fit(0.5)
    .with_real_size_below(BREAKPOINT);
    let window = match editor.open_in_app(&app, "Zoom and responsive", Some(params)) {
        Ok(window) => window,
        Err(error) => {
            eprintln!("the window did not open: {error}");
            return;
        }
    };
    match app.run() {
        Ok(end) => println!("the loop ended: {end:?}"),
        Err(error) => eprintln!("the loop failed: {error}"),
    }
    drop(window);
}
