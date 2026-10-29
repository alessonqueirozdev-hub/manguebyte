//! The audio widgets in an application's own window: a sampler's screen
//! to try by hand.
//!
//! ```text
//! cargo run -p manguebyte-gui --example sampler_window
//! ```
//!
//! Drag samples from the list onto the keys of the zone map to make
//! zones; drag zones and their edges; click a zone to see its sample and
//! its keys marked on the keyboard; drag the markers of the waveform
//! (Ctrl and the wheel zoom); play the keyboard with the mouse or, once it
//! has the focus, with the letters A to K; shape the envelope. Files
//! dragged from the file manager onto the map make zones too, named by
//! the files (this example does not load them). What the application
//! polls is printed. Windows only for now: elsewhere it says so and
//! stops.
//!
//! The same screen is drawn headless into PNG files by
//! `cargo run -p manguebyte-ui --example sampler_screen`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use manguebyte_gfx::{Scene, Size};
use manguebyte_gui::open_app_window;
use manguebyte_params::{FloatParam, FloatRange, ParamSet, param_set};
use manguebyte_ui::access::{AccessNode, Role};
use manguebyte_ui::layout::{Align, Justify};
use manguebyte_ui::{
    AccessCx, DragPayload, DropData, IntoView, KeyRange, KeySpan, KeyboardState, ListData,
    ListEvent, ListState, MeasureCx, NoteEvent, PaintCx, ParamHandle, Theme, UpdateCx, View,
    Waveform, WaveformEvent, WaveformState, Widget, ZoneEvent, ZoneMapState, card, column,
    envelope, knob, label, list_view, piano_keyboard, pill, row, waveform_view, zone_map,
};
use manguebyte_window::{App, AppWindowOptions};

struct Params {
    attack: FloatParam,
    decay: FloatParam,
    sustain: FloatParam,
    release: FloatParam,
}

param_set!(Params {
    attack,
    decay,
    sustain,
    release
});

fn params() -> Arc<Params> {
    let time = |id, name, ms| {
        FloatParam::new(id, name, FloatRange::logarithmic(1.0, 5000.0), ms)
            .with_unit("ms")
            .with_precision(0)
    };
    Arc::new(Params {
        attack: time("attack", "Attack", 5.0),
        decay: time("decay", "Decay", 400.0),
        sustain: FloatParam::new("sustain", "Sustain", FloatRange::linear(0.0, 100.0), 60.0)
            .with_unit("%")
            .with_precision(0),
        release: time("release", "Release", 800.0),
    })
}

/// Sounds made up for the example, at 48 kHz.
fn samples() -> Vec<Arc<Waveform>> {
    let rate = 48_000.0_f32;
    let tau = std::f32::consts::TAU;
    let make = |name: &str, seconds: f32, voice: &dyn Fn(f32) -> f32| {
        let data = (0..(seconds * rate) as usize)
            .map(|i| voice(i as f32 / rate))
            .collect();
        Arc::new(Waveform::new(vec![data], f64::from(rate)).named(name))
    };
    vec![
        make("Kick.wav", 0.6, &|t| {
            (tau * (50.0 + 120.0 * (-t * 30.0).exp()) * t).sin() * (-t * 6.0).exp() * 0.95
        }),
        make("Pluck.wav", 1.5, &|t| {
            (tau * 220.0 * t).sin() * (-t * 2.5).exp() * 0.8
        }),
        make("Pad.wav", 3.0, &|t| {
            let swell = (t / 0.8).min(1.0) * (1.0 - (t - 2.4).max(0.0) / 0.6);
            ((tau * 110.0 * t).sin() + (tau * 165.3 * t).sin() * 0.5) * swell * 0.5
        }),
    ]
}

/// What dragging rows of the list carries.
struct Picked(Vec<usize>);

struct Session {
    samples: Vec<Arc<Waveform>>,
    list: ListState,
    wave: WaveformState,
    zones: ZoneMapState,
    keys: KeyboardState,
    /// Which sample each zone plays, if it is one of ours.
    zone_samples: RefCell<Vec<(u64, usize)>>,
    zones_seen: Cell<Option<u64>>,
    shown: Cell<Option<usize>>,
}

impl Session {
    fn show(&self, index: usize) {
        if self.shown.replace(Some(index)) == Some(index) {
            return;
        }
        let Some(wave) = self.samples.get(index) else {
            return;
        };
        let len = wave.len() as u64;
        self.wave.set_waveform(Some(Arc::clone(wave)));
        self.wave
            .set_sample_points(0, len, Some(len / 3..len * 4 / 5));
        self.wave.set_snap_to_zero(true);
    }

    /// What the application does once a frame.
    fn poll(&self) {
        for event in self.list.take_events() {
            if let ListEvent::Activated(row) = event {
                self.show(row);
            }
        }
        for event in self.zones.take_events() {
            match event {
                ZoneEvent::Created { ids, payload } => {
                    if let Some(picked) = payload.get::<Picked>() {
                        let mut map = self.zone_samples.borrow_mut();
                        map.extend(ids.iter().copied().zip(picked.0.iter().copied()));
                        // Each zone in its sample's hue, as the list shows it.
                        let hues: Vec<(u64, usize)> = map.clone();
                        drop(map);
                        let zones = self
                            .zones
                            .zones()
                            .into_iter()
                            .map(|zone| {
                                let sample = hues.iter().find(|(id, _)| *id == zone.id);
                                match sample {
                                    Some(&(_, row)) if zone.hue.is_none() => {
                                        zone.with_hue(u8::try_from(row).unwrap_or(0))
                                    }
                                    _ => zone,
                                }
                            })
                            .collect();
                        let selected = self.zones.selected();
                        self.zones.set_zones(zones);
                        self.zones.set_selected(selected);
                    }
                    if let Some(files) = payload.get::<DropData>() {
                        println!("files dropped, not loaded: {:?}", files.files);
                    }
                    println!("zones made: {ids:?}");
                }
                ZoneEvent::Edited(ids) => println!("zones edited: {ids:?}"),
                other => println!("{other:?}"),
            }
        }
        let version = self.zones.version();
        if self.zones_seen.replace(Some(version)) != Some(version) {
            let selected = self.zones.selected();
            let ranges = selected
                .iter()
                .filter_map(|&id| self.zones.zone(id))
                .map(|zone| {
                    let range = KeyRange::new(zone.low_key, zone.high_key);
                    zone.root.map_or(range, |root| range.with_root(root))
                })
                .collect();
            self.keys.set_ranges(ranges);
            let sample = selected.last().and_then(|&id| {
                self.zone_samples
                    .borrow()
                    .iter()
                    .find(|(zone, _)| *zone == id)
                    .map(|(_, sample)| *sample)
            });
            if let Some(sample) = sample {
                self.show(sample);
            }
        }
        // The notes would go to the audio thread; here the keyboard lights
        // what is held.
        let events = self.keys.take_events();
        if !events.is_empty() {
            for event in &events {
                if let NoteEvent::On { note, velocity } = event {
                    println!("note on {note} at {velocity}");
                }
            }
            self.keys.set_sounding(self.keys.down());
        }
        for event in self.wave.take_events() {
            if let WaveformEvent::MarkerMoved { id, frame } = event {
                println!("marker {id} moved to frame {frame}");
            }
        }
    }
}

struct SampleList(Rc<Session>);

impl ListData for SampleList {
    fn len(&self) -> usize {
        self.0.samples.len()
    }

    fn name(&self, row: usize) -> String {
        self.0
            .samples
            .get(row)
            .map_or_else(String::new, |wave| wave.name().to_owned())
    }

    fn revision(&self) -> u64 {
        0
    }

    fn hue(&self, row: usize) -> Option<u8> {
        u8::try_from(row).ok()
    }

    fn drag_payload(&self, rows: &[usize]) -> Option<DragPayload> {
        let description = match rows {
            [row] => self.name(*row),
            rows => format!("{} samples", rows.len()),
        };
        Some(DragPayload::new(Picked(rows.to_vec()), description))
    }
}

/// A card's heading and the small print at its other end.
fn header(title: &str, meta: &str) -> View {
    row((label(title).heading().flex(1.0), label(meta).caption())).align(Align::Center)
}

/// Runs the application's poll once a frame.
struct Poller(Rc<Session>);

impl Widget for Poller {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Group, "")
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        Size::new(0.0, 0.0)
    }

    fn paint(&mut self, _cx: &PaintCx<'_>, _scene: &mut Scene) {}

    fn update(&mut self, _cx: &mut UpdateCx<'_>) {
        self.0.poll();
    }
}

fn main() {
    let app = match App::new() {
        Ok(app) => app,
        Err(error) => {
            eprintln!("no application windows here: {error}");
            return;
        }
    };
    let session = Rc::new(Session {
        samples: samples(),
        list: ListState::new(),
        wave: WaveformState::new(),
        zones: ZoneMapState::new(),
        keys: KeyboardState::new(),
        zone_samples: RefCell::default(),
        zones_seen: Cell::new(None),
        shown: Cell::new(None),
    });
    session.show(1);
    let params = params();
    let handle = |field: fn(&Params) -> &FloatParam| ParamHandle::new(Arc::clone(&params), field);
    let span = KeySpan::KEYS_61;
    let names = Rc::clone(&session);
    let view = column((
        row((
            column((
                label("Mangue Sampler").title(),
                label("Drag samples onto the keys").caption(),
            )),
            Poller(Rc::clone(&session)).flex(1.0),
            pill("Undo"),
            pill("Export plugin").primary(),
        ))
        .spacing(12.0)
        .align(Align::Center),
        card(
            column((
                header("Sample", "markers, loop, zoom with Ctrl and the wheel"),
                waveform_view("Sample", session.wave.clone()).height(150.0),
            ))
            .spacing(10.0),
        ),
        row((
            card(
                column((
                    header("Samples", ""),
                    list_view(
                        "Samples",
                        SampleList(Rc::clone(&session)),
                        session.list.clone(),
                    )
                    .flex(1.0),
                ))
                .spacing(8.0),
            )
            .width(200.0),
            card(
                column((
                    header("Zone map", "note \u{d7} velocity"),
                    zone_map("Zones", span, session.zones.clone())
                        .takes(move |payload| {
                            if let Some(picked) = payload.get::<Picked>() {
                                return Some(
                                    picked
                                        .0
                                        .iter()
                                        .filter_map(|&row| names.samples.get(row))
                                        .map(|wave| wave.name().to_owned())
                                        .collect(),
                                );
                            }
                            let files = payload.get::<DropData>()?;
                            let names: Vec<String> = files
                                .files
                                .iter()
                                .filter_map(|file| file.file_name())
                                .map(|name| name.to_string_lossy().into_owned())
                                .collect();
                            (!names.is_empty()).then_some(names)
                        })
                        .flex(1.0),
                    piano_keyboard("Keyboard", span, session.keys.clone()).height(96.0),
                ))
                .spacing(10.0),
            )
            .flex(1.0),
            card(
                column((
                    header("Envelope", "ADSR"),
                    envelope(
                        "Amp envelope",
                        handle(|p| &p.attack),
                        handle(|p| &p.decay),
                        handle(|p| &p.sustain),
                        handle(|p| &p.release),
                    )
                    .flex(1.0),
                    row((
                        knob(handle(|p| &p.attack)),
                        knob(handle(|p| &p.decay)),
                        knob(handle(|p| &p.sustain)),
                        knob(handle(|p| &p.release)),
                    ))
                    .justify(Justify::SpaceBetween),
                ))
                .spacing(10.0),
            )
            .width(320.0),
        ))
        .spacing(14.0)
        .flex(1.0),
    ))
    .spacing(14.0)
    .padding(20.0);

    let set: Arc<dyn ParamSet> = params.clone();
    let window = match open_app_window(
        &app,
        AppWindowOptions::new("Manguebyte sampler", Size::new(1100.0, 680.0))
            .with_min_size(Size::new(900.0, 560.0)),
        Theme::default(),
        Some(set),
        view,
    ) {
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
