//! A sampler's screen built from the audio widgets, driven headless and
//! drawn into PNG files.
//!
//! ```text
//! cargo run -p manguebyte-ui --example sampler_screen
//! cargo run -p manguebyte-ui --example sampler_screen -- <folder>
//! cargo run -p manguebyte-ui --example sampler_screen -- <folder> --pictures
//! ```
//!
//! With `--pictures` it draws only the pictures of the screen mid-session.
//!
//! The screen has a title bar with pills, a card with the list of
//! samples, a card with the waveform of the sample chosen with its start,
//! end and loop markers, a card with a zone map above a keyboard, lined
//! up key for key, and a card with an envelope editor and its four knobs.
//!
//! The program first draws the screen as a sampler would look mid-session,
//! in both themes at 1x, 1.25x and 2x (`sampler-dark@2x.png` and so on).
//! Then it drives the screen as a person would, with no window: drags
//! samples from the list onto keys, which makes zones; widens a zone by
//! its edge; picks a zone, which shows its sample and marks its keys on
//! the keyboard; drags the loop start; plays a chord; and shapes the
//! envelope, and draws that too (`sampler-mapped@2x.png`,
//! `sampler-playing@2x.png`). It writes into the folder given, or the
//! system's temporary folder, and prints what the application polled from
//! the widgets' states.
//!
//! The application's own logic lives in `Session::poll`, which runs once
//! a frame from a widget of the example's own (`Poller`): it reads the
//! states the widgets share, and acts on what changed. No widget calls
//! the application back.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use manguebyte_gfx::{
    Color, Extend, LinearGradient, Point, RadialGradient, Rect, RoundedRect, Scene, Shadow, Size,
    Stop, Transform,
};
use manguebyte_params::{FloatParam, FloatRange, param_set};
use manguebyte_plugin::editor::EditorContext;
use manguebyte_plugin::editor::testing::RecordingEditorHost;
use manguebyte_ui::access::{AccessNode, Role};
use manguebyte_ui::layout::Align;
use manguebyte_ui::{
    AccessCx, Button, DragPayload, Event, IntoView, Key, KeyRange, KeySpan, KeyboardState,
    ListData, ListEvent, ListState, MeasureCx, Modifiers, NoteEvent, PaintCx, ParamHandle, Root,
    State, Theme, UpdateCx, View, Waveform, WaveformEvent, WaveformState, Widget, Zone, ZoneEvent,
    ZoneMapState, canvas, card, column, envelope, knob, label, list_view, piano_keyboard, pill,
    row, stack, waveform_view, zone_map,
};

/// The screen's size in logical pixels.
const SIZE: Size = Size::new(1100.0, 640.0);

/// The keys the zone map and the keyboard show: C1 to C6.
fn span() -> KeySpan {
    KeySpan::new(24, 84)
}

/// The envelope's parameters.
struct Params {
    attack: FloatParam,
    decay: FloatParam,
    sustain: FloatParam,
    release: FloatParam,
    release_curve: FloatParam,
}

param_set!(Params {
    attack,
    decay,
    sustain,
    release,
    release_curve
});

fn params() -> Arc<Params> {
    let time = |id, name, ms| {
        FloatParam::new(id, name, FloatRange::logarithmic(1.0, 5000.0), ms)
            .with_unit("ms")
            .with_precision(0)
    };
    Arc::new(Params {
        attack: time("attack", "Attack", 4.0),
        decay: time("decay", "Decay", 54.0),
        sustain: FloatParam::new("sustain", "Sustain", FloatRange::linear(0.0, 100.0), 62.0)
            .with_unit("%")
            .with_precision(0),
        release: time("release", "Release", 1200.0),
        release_curve: FloatParam::new(
            "release_curve",
            "Release curve",
            FloatRange::linear(-1.0, 1.0),
            0.4,
        )
        .with_precision(2),
    })
}

/// A sample the application has loaded, and where its markers are.
struct Sample {
    wave: Arc<Waveform>,
    points: (u64, u64, std::ops::Range<u64>),
}

/// Sounds made up for the example, at 48 kHz.
fn samples() -> Vec<Sample> {
    let rate = 48_000.0;
    let make = |name: &str, seconds: f32, channels: usize, voice: &dyn Fn(usize, f32) -> f32| {
        let len = (seconds * rate) as usize;
        let data: Vec<Vec<f32>> = (0..channels)
            .map(|channel| (0..len).map(|i| voice(channel, i as f32 / rate)).collect())
            .collect();
        let len = len as u64;
        Sample {
            wave: Arc::new(Waveform::new(data, f64::from(rate)).named(name)),
            points: (len / 50, len * 97 / 100, len * 36 / 100..len * 78 / 100),
        }
    };
    let tau = std::f32::consts::TAU;
    // A noise that is the same on every run.
    let noise = |t: f32| ((t * 12_345.678).sin() * 43_758.547).fract() * 2.0 - 1.0;
    vec![
        make("Kick.wav", 0.6, 1, &|_, t| {
            (tau * (50.0 + 120.0 * (-t * 30.0).exp()) * t).sin() * (-t * 6.0).exp() * 0.95
        }),
        make("Snare.wav", 0.4, 1, &|_, t| {
            (noise(t) * 0.7 + (tau * 190.0 * t).sin() * 0.3) * (-t * 14.0).exp()
        }),
        make("Pluck.wav", 2.48, 1, &|_, t| {
            // A plucked string: a fast attack, partials that die away, and
            // a slow beat between two of them.
            let attack = 1.0 - (-t * 60.0).exp();
            let body = (tau * 220.0 * t).sin()
                + (tau * 440.7 * t).sin() * 0.5 * (-t * 3.0).exp()
                + (tau * 661.0 * t).sin() * 0.3 * (-t * 5.0).exp();
            let beat = 0.8 + 0.2 * (tau * 3.1 * t).sin();
            body * attack * (-t * 1.3).exp() * beat * 0.55
        }),
        make("Pad.wav", 3.0, 2, &|channel, t| {
            let swell = (t / 0.8).min(1.0) * (1.0 - (t - 2.4).max(0.0) / 0.6);
            let spread = if channel == 0 { 1.0 } else { 1.005 };
            ((tau * 110.0 * spread * t).sin() + (tau * 165.3 * t).sin() * 0.5) * swell * 0.5
        }),
    ]
}

/// What dragging rows of the sample list carries: their indices.
struct Picked(Vec<usize>);

/// The list of samples: each with its hue, and the keys it plays.
struct SampleList(Rc<Session>);

impl ListData for SampleList {
    fn len(&self) -> usize {
        self.0.samples.len()
    }

    fn name(&self, row: usize) -> String {
        self.0
            .samples
            .get(row)
            .map_or_else(String::new, |sample| sample.wave.name().to_owned())
    }

    fn revision(&self) -> u64 {
        self.0.zones.version()
    }

    fn hue(&self, row: usize) -> Option<u8> {
        u8::try_from(row).ok()
    }

    fn detail(&self, row: usize) -> Option<String> {
        // The keys the sample's zones cover.
        let map = self.0.zone_samples.borrow();
        let zones: Vec<Zone> = map
            .iter()
            .filter(|(_, sample)| *sample == row)
            .filter_map(|(zone, _)| self.0.zones.zone(*zone))
            .collect();
        let low = zones.iter().map(|zone| zone.low_key).min()?;
        let high = zones.iter().map(|zone| zone.high_key).max()?;
        let name = manguebyte_ui::note_name;
        Some(if low == high {
            name(low)
        } else {
            format!("{}\u{2013}{}", name(low), name(high))
        })
    }

    fn drag_payload(&self, rows: &[usize]) -> Option<DragPayload> {
        let description = match rows {
            [row] => self.name(*row),
            rows => format!("{} samples", rows.len()),
        };
        Some(DragPayload::new(Picked(rows.to_vec()), description))
    }
}

/// The application: the samples, the states it shares with the widgets,
/// and what it has made of them.
struct Session {
    samples: Vec<Sample>,
    list: ListState,
    wave: WaveformState,
    zones: ZoneMapState,
    keys: KeyboardState,
    /// Which sample each zone plays.
    zone_samples: RefCell<Vec<(u64, usize)>>,
    /// The sample the waveform shows.
    shown: Cell<Option<usize>>,
    /// The version of the zones when last looked at.
    zones_seen: Cell<Option<u64>>,
    /// Notes playing, and when they started.
    voices: RefCell<Vec<(u8, Duration)>>,
    /// The title bar's pills, counted.
    undo: State<u64>,
    export: State<u64>,
    /// For the pictures: notes held and the playhead where it is, still.
    still: RefCell<Option<(Vec<u8>, u64)>>,
}

impl Session {
    fn new() -> Rc<Session> {
        Rc::new(Session {
            samples: samples(),
            list: ListState::new(),
            wave: WaveformState::new(),
            zones: ZoneMapState::new(),
            keys: KeyboardState::new(),
            zone_samples: RefCell::default(),
            shown: Cell::new(None),
            zones_seen: Cell::new(None),
            voices: RefCell::default(),
            undo: State::new(0),
            export: State::new(0),
            still: RefCell::default(),
        })
    }

    /// Shows sample `index` in the waveform, with its markers.
    fn show(&self, index: usize) {
        if self.shown.get() == Some(index) {
            return;
        }
        // Keep where the markers of the sample shown were left.
        self.remember_points();
        let Some(sample) = self.samples.get(index) else {
            return;
        };
        self.shown.set(Some(index));
        self.wave.set_waveform(Some(Arc::clone(&sample.wave)));
        let (start, end, looped) = sample.points.clone();
        self.wave.set_sample_points(start, end, Some(looped));
        self.wave.set_snap_to_zero(true);
    }

    fn remember_points(&self) {
        if let Some(index) = self.shown.get() {
            let frame = |id| self.wave.marker(id).map_or(0, |marker| marker.frame);
            println!(
                "sample {index}: start {}, end {}, loop {}..{}",
                frame(WaveformState::START),
                frame(WaveformState::END),
                frame(WaveformState::LOOP_START),
                frame(WaveformState::LOOP_END)
            );
        }
    }

    fn sample_of_zone(&self, id: u64) -> Option<usize> {
        self.zone_samples
            .borrow()
            .iter()
            .find(|(zone, _)| *zone == id)
            .map(|(_, sample)| *sample)
    }

    /// What the application does once a frame: reads what the widgets
    /// changed and acts on it. Returns whether something is moving.
    fn poll(&self, now: Duration) -> bool {
        for event in self.list.take_events() {
            if let ListEvent::Activated(row) = event {
                self.show(row);
            }
        }
        for event in self.zones.take_events() {
            match event {
                ZoneEvent::Created { ids, payload } => {
                    let rows = payload
                        .get::<Picked>()
                        .map(|picked| picked.0.clone())
                        .unwrap_or_default();
                    let mut map = self.zone_samples.borrow_mut();
                    for (id, row) in ids.iter().zip(&rows) {
                        map.push((*id, *row));
                    }
                    drop(map);
                    // Each new zone in its sample's hue.
                    let zones = self
                        .zones
                        .zones()
                        .into_iter()
                        .map(|zone| match self.sample_of_zone(zone.id) {
                            Some(row) if zone.hue.is_none() => {
                                zone.with_hue(u8::try_from(row).unwrap_or(0))
                            }
                            _ => zone,
                        })
                        .collect();
                    let selected = self.zones.selected();
                    self.zones.set_zones(zones);
                    self.zones.set_selected(selected);
                    println!("zones made: {ids:?}");
                }
                ZoneEvent::Edited(ids) => println!("zones edited: {ids:?}"),
                ZoneEvent::Removed(gone) => {
                    let mut map = self.zone_samples.borrow_mut();
                    map.retain(|(id, _)| gone.iter().all(|zone| zone.id != *id));
                }
                _ => {}
            }
        }
        // The zones selected, whenever the zones change: their keys are
        // marked on the keyboard, and the waveform shows the last one's sample.
        let version = self.zones.version();
        if self.zones_seen.get() != Some(version) {
            let selected = self.zones.selected();
            let ranges = selected
                .iter()
                .filter_map(|&id| self.zones.zone(id))
                .map(|zone| {
                    let range = KeyRange::new(zone.low_key, zone.high_key);
                    match zone.root {
                        Some(root) => range.with_root(root),
                        None => range,
                    }
                })
                .collect();
            self.keys.set_ranges(ranges);
            if let Some(sample) = selected.last().and_then(|&id| self.sample_of_zone(id)) {
                self.show(sample);
                self.list.set_selected([sample]);
            }
            self.zones_seen.set(Some(version));
        }
        // Notes: here they would go to the audio thread. The keyboard
        // lights the notes the "voices" play.
        for event in self.keys.take_events() {
            println!("note: {event:?}");
            let mut voices = self.voices.borrow_mut();
            match event {
                NoteEvent::On { note, .. } => voices.push((note, now)),
                NoteEvent::Off { note } => voices.retain(|(playing, _)| *playing != note),
                _ => {}
            }
        }
        if let Some((notes, frame)) = self.still.borrow().clone() {
            self.keys.set_sounding(notes);
            self.wave.set_playhead(Some(frame));
            return false;
        }
        let voices = self.voices.borrow();
        self.keys.set_sounding(voices.iter().map(|(note, _)| *note));
        // The playhead of the newest voice whose zone's sample is shown.
        let shown = self.shown.get();
        let playing = voices.iter().rev().find_map(|&(note, started)| {
            let zone = self
                .zones
                .zones()
                .into_iter()
                .find(|zone| zone.contains(note, 100))?;
            let sample = self.sample_of_zone(zone.id)?;
            (Some(sample) == shown).then_some((sample, now.saturating_sub(started)))
        });
        let playhead = playing.map(|(sample, elapsed)| {
            let wave = &self.samples[sample].wave;
            ((elapsed.as_secs_f64() * wave.sample_rate()) as u64).min(wave.len() as u64)
        });
        self.wave.set_playhead(playhead);
        for event in self.wave.take_events() {
            if let WaveformEvent::MarkerMoved { id, frame } = event {
                println!("marker {id} moved to frame {frame}");
            }
        }
        playhead.is_some()
    }

    /// The session as the pictures show it: the four samples mapped, the
    /// pluck's loud zone chosen, a chord sounding.
    fn mid_session(&self) {
        let zones = vec![
            Zone::new(0, "Kick", 24, 29).with_hue(0),
            Zone::new(1, "Snare", 30, 35).with_hue(1),
            Zone::new(2, "Pluck \u{b7} forte", 48, 67)
                .with_velocities(64, 127)
                .with_hue(2),
            Zone::new(3, "Pluck \u{b7} soft", 48, 67)
                .with_velocities(0, 63)
                .with_hue(2),
            Zone::new(4, "Pad", 69, 84).with_hue(3),
        ];
        *self.zone_samples.borrow_mut() = vec![(0, 0), (1, 1), (2, 2), (3, 2), (4, 3)];
        self.zones.set_zones(zones);
        self.zones.set_selected([2]);
        // A chord sounding, and the playhead a fifth of the way in.
        let len = self.samples[2].wave.len() as u64;
        *self.still.borrow_mut() = Some((vec![60, 64, 67], len * 21 / 100));
    }
}

/// A widget with nothing to show that runs the application's poll once a
/// frame.
struct Poller(Rc<Session>);

impl Widget for Poller {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Group, "")
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        Size::new(0.0, 0.0)
    }

    fn paint(&mut self, _cx: &PaintCx<'_>, _scene: &mut Scene) {}

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        if self.0.poll(cx.now()) {
            cx.request_frame();
        }
    }
}

/// A card's heading and the small print at its other end.
fn header(title: &str, meta: impl Into<String>) -> View {
    row((
        label(title).heading().flex(1.0),
        label(meta.into()).caption(),
    ))
    .align(Align::Center)
}

/// Behind everything, in a theme that glows: two wide, faint glows, the
/// accent's from the top left and a violet one from the right.
fn backdrop() -> View {
    canvas("", Size::new(1.0, 1.0), |cx, scene| {
        let theme = cx.theme();
        if theme.elevation.glow.color.a == 0 {
            return;
        }
        let size = cx.size();
        let glow = |scene: &mut Scene, at: Point, (rx, ry): (f32, f32), color: Color, reach| {
            let stops = [Stop::new(0.0, color), Stop::new(reach, color.with_alpha(0))];
            let squash = Transform::scale(1.0, ry / rx).then(Transform::translate(at.x, at.y));
            scene.transformed(squash, |scene| {
                scene.fill(
                    Rect::new(-rx, -rx, 2.0 * rx, 2.0 * rx),
                    RadialGradient {
                        center: Point::ZERO,
                        radius: rx,
                        stops: &stops,
                        extend: Extend::Pad,
                    },
                );
            });
        };
        glow(
            scene,
            Point::new(size.width * 0.2, -size.height * 0.1),
            (1200.0, 500.0),
            theme.palette.accent.with_alpha(0x1A),
            0.6,
        );
        glow(
            scene,
            Point::new(size.width * 1.1, size.height * 0.1),
            (900.0, 500.0),
            theme.palette.hues[0].color.with_alpha(0x1A),
            0.55,
        );
    })
    .into_view()
}

/// The application's mark: a rounded square in the accent's gradient,
/// glowing in a theme that glows, ink in one that does not.
fn logo() -> View {
    canvas("Mangue", Size::new(30.0, 30.0), |cx, scene| {
        let theme = cx.theme();
        let palette = &theme.palette;
        let size = cx.size();
        let shape = RoundedRect::new(Rect::new(0.0, 0.0, size.width, size.height), 9.0);
        if theme.elevation.glow.color.a > 0 {
            let stops = [
                Stop::new(0.0, palette.accent),
                Stop::new(1.0, Color::hex(0xFF3D6E)),
            ];
            scene.shadow(
                shape,
                Shadow::new(Color::hex(0xFF5C50).with_alpha(0x99))
                    .with_offset(0.0, 6.0)
                    .with_blur(18.0)
                    .with_spread(-4.0),
            );
            scene.fill(
                shape,
                LinearGradient {
                    start: Point::ZERO,
                    end: Point::new(size.width, size.height),
                    stops: &stops,
                    extend: Extend::Pad,
                },
            );
        } else {
            scene.fill(shape, palette.text);
        }
    })
    .width(30.0)
    .height(30.0)
}

/// The sampler screen.
fn screen(session: &Rc<Session>, params: &Arc<Params>) -> View {
    let handle = |field: fn(&Params) -> &FloatParam| ParamHandle::new(Arc::clone(params), field);
    let amp = envelope(
        "Amp envelope",
        handle(|p| &p.attack),
        handle(|p| &p.decay),
        handle(|p| &p.sustain),
        handle(|p| &p.release),
    )
    .release_curve(handle(|p| &p.release_curve));
    let list = Rc::clone(session);
    let teal = Theme::dark().palette.hues[1].color;
    let title_bar = row((
        logo(),
        column((
            label("Mangue Sampler").title(),
            label("Pluck kit \u{b7} 48 kHz").caption(),
        )),
        Poller(Rc::clone(session)).flex(1.0),
        pill("Playing").dot(teal),
        pill("Undo").presses(session.undo.clone()),
        pill("Export plugin")
            .primary()
            .presses(session.export.clone()),
    ))
    .spacing(12.0)
    .align(Align::Center);
    let samples = card(
        column((
            header("Samples", format!("{}", session.samples.len())),
            list_view(
                "Samples",
                SampleList(Rc::clone(session)),
                session.list.clone(),
            )
            .flex(1.0),
        ))
        .spacing(8.0),
    )
    .width(220.0);
    let wave = card(
        column((
            header("Pluck.wav", "2.48 s \u{b7} mono"),
            waveform_view("Sample", session.wave.clone()).height(150.0),
        ))
        .spacing(10.0),
    );
    let zones = card(
        column((
            header("Zone map", "note \u{d7} velocity"),
            zone_map("Zones", span(), session.zones.clone())
                .takes(move |payload| {
                    // Rows of the list, a zone each named by its sample;
                    // files from other programs are not loaded here.
                    payload.get::<Picked>().map(|picked| {
                        picked
                            .0
                            .iter()
                            .filter_map(|&row| list.samples.get(row))
                            .map(|sample| sample.wave.name().to_owned())
                            .collect()
                    })
                })
                .flex(1.0),
            piano_keyboard("Keyboard", span(), session.keys.clone()).height(104.0),
        ))
        .spacing(10.0),
    )
    .flex(1.0);
    let knobs = row((
        knob(handle(|p| &p.attack)),
        knob(handle(|p| &p.decay)),
        knob(handle(|p| &p.sustain)),
        knob(handle(|p| &p.release)),
    ))
    .justify(manguebyte_ui::layout::Justify::SpaceBetween);
    let shape = card(column((header("Envelope", "ADSR"), amp.height(150.0), knobs)).spacing(10.0))
        .width(320.0);
    stack((
        backdrop(),
        column((
            title_bar,
            row((
                samples,
                column((wave, row((zones, shape)).spacing(14.0).flex(1.0)))
                    .spacing(14.0)
                    .flex(1.0),
            ))
            .spacing(14.0)
            .flex(1.0),
        ))
        .spacing(18.0)
        .padding(22.0),
    ))
}

/// Drives a root as a person would, frame by frame.
struct Driver {
    root: Root,
    clock: Duration,
}

impl Driver {
    fn frame(&mut self) {
        self.clock += Duration::from_millis(16);
        self.root.frame(self.clock);
    }

    /// Runs frames until nothing moves.
    fn settle(&mut self) {
        for _ in 0..200 {
            self.frame();
            if !self.root.is_animating() {
                break;
            }
        }
    }

    fn find(&self, role: Role, name: &str) -> manguebyte_gfx::Rect {
        self.root
            .access_tree()
            .into_iter()
            .find(|entry| entry.node.role == role && entry.node.name == name)
            .map(|entry| entry.bounds)
            .unwrap_or_else(|| panic!("no {role:?} named {name}"))
    }

    /// Where the zone named `name` is: an item of the zone map, as the
    /// list's rows have the same names.
    fn zone(&self, name: &str) -> manguebyte_gfx::Rect {
        let tree = self.root.access_tree();
        let map = tree
            .iter()
            .find(|entry| entry.node.role == Role::List && entry.node.name == "Zones")
            .map(|entry| entry.id);
        tree.iter()
            .find(|entry| entry.item.is_some() && Some(entry.id) == map && entry.node.name == name)
            .map(|entry| entry.bounds)
            .unwrap_or_else(|| panic!("no zone named {name}"))
    }

    fn press(&mut self, at: Point, modifiers: Modifiers) {
        self.root.event(Event::PointerDown {
            at,
            button: Button::Primary,
            clicks: 1,
            modifiers,
        });
    }

    fn release(&mut self, at: Point) {
        self.root.event(Event::PointerUp {
            at,
            button: Button::Primary,
            modifiers: Modifiers::NONE,
        });
    }

    fn click(&mut self, at: Point, modifiers: Modifiers) {
        self.press(at, modifiers);
        self.release(at);
        self.frame();
    }

    /// Presses at `from`, moves to `to` in steps with a frame each, and
    /// releases.
    fn drag(&mut self, from: Point, to: Point) {
        self.press(from, Modifiers::NONE);
        for step in 1..=8 {
            let t = step as f32 / 8.0;
            let at = Point::new(from.x + (to.x - from.x) * t, from.y + (to.y - from.y) * t);
            self.root.event(Event::PointerMove {
                at,
                modifiers: Modifiers::NONE,
            });
            self.frame();
        }
        self.release(to);
        self.frame();
    }

    /// Where key `note` is in the zone map's columns, at velocity 64.
    fn on_key(&self, note: u8) -> Point {
        let map = self.find(Role::List, "Zones");
        let column = span().column(map, note).expect("on the keyboard");
        Point::new(column.center().x, map.y + map.height * 0.5)
    }

    fn save(&self, folder: &Path, name: &str) -> std::io::Result<PathBuf> {
        let path = folder.join(name);
        let png = self
            .root
            .pixels()
            .encode_png()
            .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
        std::fs::write(&path, png)?;
        Ok(path)
    }
}

/// A root showing `session` at `scale` in `theme`.
fn open(session: &Rc<Session>, params: &Arc<Params>, scale: f32, theme: Theme) -> Driver {
    let host = Rc::new(RecordingEditorHost::new().with_params(params.clone()));
    let (context, _link) = EditorContext::attach(host);
    Driver {
        root: Root::new(screen(session, params), SIZE, scale, context).with_theme(theme),
        clock: Duration::ZERO,
    }
}

/// The screen mid-session, in both themes at three scales.
fn pictures(folder: &Path, written: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for (theme, theme_name) in [(Theme::dark(), "dark"), (Theme::light(), "light")] {
        for (scale, suffix) in [(1.0, "@1x"), (1.25, "@1.25x"), (2.0, "@2x")] {
            let session = Session::new();
            let params = params();
            session.mid_session();
            let mut app = open(&session, &params, scale, theme);
            app.settle();
            written.push(app.save(folder, &format!("sampler-{theme_name}{suffix}.png"))?);
        }
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    let folder = std::env::args_os()
        .nth(1)
        .map_or_else(std::env::temp_dir, PathBuf::from);
    let mut written = Vec::new();
    pictures(&folder, &mut written)?;
    let drive = !std::env::args().any(|arg| arg == "--pictures");
    let scales = if drive {
        vec![(2.0, "@2x"), (1.25, "@1.25x")]
    } else {
        Vec::new()
    };
    for (scale, suffix) in scales {
        let session = Session::new();
        let params = params();
        let mut app = open(&session, &params, scale, Theme::dark());
        app.frame();

        // The kick onto C2 and the snare onto D2.
        let kick = app.find(Role::ListItem, "Kick.wav").center();
        let c2 = app.on_key(36);
        app.drag(kick, c2);
        let snare = app.find(Role::ListItem, "Snare.wav").center();
        let d2 = app.on_key(38);
        app.drag(snare, d2);
        // The pluck and the pad together onto C3: a zone each, on C3 and C#3.
        app.click(
            app.find(Role::ListItem, "Pluck.wav").center(),
            Modifiers::NONE,
        );
        let control = Modifiers {
            control: true,
            ..Modifiers::NONE
        };
        app.click(app.find(Role::ListItem, "Pad.wav").center(), control);
        let pluck = app.find(Role::ListItem, "Pluck.wav").center();
        app.drag(pluck, app.on_key(48));
        // The pad's zone, picked alone, widened by its edges to F3..C5.
        let pad = app.zone("Pad.wav");
        app.click(pad.center(), Modifiers::NONE);
        let edge = Point::new(pad.x + pad.width - 1.0, pad.y + pad.height * 0.5);
        let to = app.on_key(72);
        app.drag(edge, Point::new(to.x + 4.0, edge.y));
        let pad = app.zone("Pad.wav");
        let edge = Point::new(pad.x + 1.0, pad.y + pad.height * 0.5);
        let to = app.on_key(53);
        app.drag(edge, Point::new(to.x - 4.0, edge.y));
        // The pluck's zone picked, which shows its sample, moved to E3 and
        // widened with Alt and Right to G3.
        let zone = app.zone("Pluck.wav");
        app.click(zone.center(), Modifiers::NONE);
        app.drag(zone.center(), Point::new(app.on_key(52).x, zone.center().y));
        for _ in 0..3 {
            app.root.event(Event::Key(manguebyte_ui::KeyEvent {
                key: Key::Right,
                modifiers: Modifiers {
                    alt: true,
                    ..Modifiers::NONE
                },
                pressed: true,
            }));
            app.frame();
        }
        // The loop start dragged later.
        let loop_start = app.find(Role::Slider, "Loop start").center();
        app.drag(loop_start, Point::new(loop_start.x + 60.0, loop_start.y));
        // The envelope's decay shortened and the sustain raised.
        let decay = app.find(Role::Slider, "Decay").center();
        app.drag(decay, Point::new(decay.x - 12.0, decay.y - 15.0));
        app.settle();
        written.push(app.save(&folder, &format!("sampler-mapped{suffix}.png"))?);

        // A chord: E3 held by the pointer, G4 and C5 by the letters, which
        // play from C4.
        let e3 = app.find(Role::Button, "E3");
        app.press(
            Point::new(e3.x + e3.width * 0.5, e3.y + e3.height * 0.8),
            Modifiers::NONE,
        );
        app.frame();
        for c in ['g', 'k'] {
            app.root.event(Event::Key(manguebyte_ui::KeyEvent {
                key: Key::Character(c),
                modifiers: Modifiers::NONE,
                pressed: true,
            }));
        }
        for _ in 0..20 {
            app.frame();
        }
        written.push(app.save(&folder, &format!("sampler-playing{suffix}.png"))?);
        app.release(Point::new(e3.x + e3.width * 0.5, e3.y + e3.height * 0.8));
        app.frame();
        println!(
            "{suffix}: {} zones, envelope decay {:.0} ms, sustain {:.0} %, presses {} {}",
            session.zones.zones().len(),
            params.decay.value(),
            params.sustain.value(),
            session.undo.get(),
            session.export.get()
        );
    }
    for path in written {
        println!("wrote {}", path.display());
    }
    Ok(())
}
