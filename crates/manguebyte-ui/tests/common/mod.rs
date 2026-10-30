//! A small panel used by several tests: three knobs, a slider, a switch,
//! two meters, a title and a canvas.

#![allow(dead_code)]

use std::sync::Arc;

use manguebyte_core::AtomicF32;
use manguebyte_gfx::{Path, Point, Size, Stroke};
use manguebyte_params::{BoolParam, FloatParam, FloatRange, param_set};
use manguebyte_ui::layout::Align;
use manguebyte_ui::{
    AtomicSource, IntoView, ParamHandle, View, canvas, column, knob, label, meter, panel, row,
    slider, toggle,
};

pub struct PanelParams {
    pub cutoff: FloatParam,
    pub resonance: FloatParam,
    pub drive: FloatParam,
    pub mix: FloatParam,
    pub bypass: BoolParam,
}

param_set!(PanelParams {
    cutoff,
    resonance,
    drive,
    mix,
    bypass
});

pub fn params() -> Arc<PanelParams> {
    Arc::new(PanelParams {
        cutoff: FloatParam::new(
            "cutoff",
            "Cutoff",
            FloatRange::logarithmic(20.0, 20_000.0),
            1_000.0,
        )
        .with_unit("Hz")
        .with_precision(0),
        resonance: FloatParam::new("resonance", "Resonance", FloatRange::linear(0.0, 1.0), 0.25)
            .with_precision(2),
        drive: FloatParam::new("drive", "Drive", FloatRange::linear(0.0, 24.0), 6.0)
            .with_unit("dB")
            .with_precision(1),
        mix: FloatParam::new("mix", "Mix", FloatRange::linear(0.0, 100.0), 100.0)
            .with_unit("%")
            .with_precision(0),
        bypass: BoolParam::new("bypass", "Bypass", false),
    })
}

/// What the audio thread publishes.
pub struct Levels {
    pub left: Arc<AtomicF32>,
    pub right: Arc<AtomicF32>,
}

pub fn levels() -> Levels {
    Levels {
        left: Arc::new(AtomicF32::new(-60.0)),
        right: Arc::new(AtomicF32::new(-60.0)),
    }
}

pub const PANEL: Size = Size::new(400.0, 300.0);

/// A triangle wave from 0 to 1 and back, period 1, from `floor`, which is
/// exact on every platform.
fn triangle(x: f32) -> f32 {
    2.0 * (x - x.floor() - 0.5).abs()
}

pub fn panel_view(params: &Arc<PanelParams>, levels: &Levels) -> View {
    let level = |name: &str, value: &Arc<AtomicF32>| {
        meter(name, AtomicSource::new(Arc::clone(value)))
            .range(-60.0, 0.0)
            .unit("dB")
            .precision(1)
            .warning(-6.0)
    };
    let scope = canvas("Scope", Size::new(120.0, 40.0), |cx, scene| {
        let size = cx.size();
        let mut wave = Path::new();
        let steps = 48;
        for step in 0..=steps {
            let t = step as f32 / steps as f32;
            let at = Point::new(
                t * size.width,
                4.0 + (size.height - 8.0) * triangle(t * 3.0),
            );
            if step == 0 {
                wave.move_to(at);
            } else {
                wave.line_to(at);
            }
        }
        scene.stroke(&wave, Stroke::round(1.5), cx.theme().palette.accent);
    });
    panel(
        column((
            row((
                label("Filter").title().flex(1.0),
                toggle(ParamHandle::new(Arc::clone(params), |p| &p.bypass)),
            ))
            .align(Align::Center),
            row((
                knob(ParamHandle::new(Arc::clone(params), |p| &p.cutoff)),
                knob(ParamHandle::new(Arc::clone(params), |p| &p.resonance)),
                knob(ParamHandle::new(Arc::clone(params), |p| &p.drive)),
                level("Left", &levels.left),
                level("Right", &levels.right),
            ))
            .spacing(8.0)
            .named("Controls"),
            slider(ParamHandle::new(Arc::clone(params), |p| &p.mix)),
            scope.flex(1.0),
        ))
        .spacing(8.0)
        .padding(12.0),
    )
}

pub mod languages;
