//! A sampler's screen built from the audio widgets, driven headless and
//! drawn into PNG files.
//!
//! ```text
//! cargo run -p manguebyte-ui --example sampler_screen
//! cargo run -p manguebyte-ui --example sampler_screen -- <folder>
//! ```
//!
//! The screen has a list of samples, the waveform of the sample chosen
//! with its start, end and loop markers, a zone map above a 61-key
//! keyboard, lined up key for key, and an envelope editor beside them.
//! The program drives it as a person would, with no window: drags samples
//! from the list onto keys, which makes zones; widens a zone by its edge;
//! picks a zone, which shows its sample and marks its keys on the
//! keyboard; drags the loop start; plays a chord; and shapes the
//! envelope. It writes pictures into the folder given, or the system's
//! temporary folder, at scale 2 and at scale 1.25, and prints what the
//! application polled from the widgets' states.
//!
//! The application's own logic lives in `Session::poll`, which runs once
//! a frame from a widget of the example's own (`Poller`): it reads the
//! states the widgets share, and acts on what changed. No widget calls
//! the application back.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use manguebyte_gfx::{Point, Scene, Size};
use manguebyte_params::{FloatParam, FloatRange, param_set};
use manguebyte_plugin::editor::EditorContext;
use manguebyte_plugin::editor::testing::RecordingEditorHost;
use manguebyte_ui::access::{AccessNode, Role};
use manguebyte_ui::{
    AccessCx, Button, DragPayload, Event, IntoView, Key, KeyRange, KeySpan, KeyboardState,
    ListData, ListEvent, ListState, MeasureCx, Modifiers, NoteEvent, PaintCx, ParamHandle, Root,
    UpdateCx, View, Waveform, WaveformEvent, WaveformState, Widget, ZoneEvent, ZoneMapState,
    column, envelope, label, list_view, piano_keyboard, row, waveform_view, zone_map,
};

const SIZE: Size = Size::new(900.0, 560.0);

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
        attack: time("attack", "Attack", 5.0),
        decay: time("decay", "Decay", 400.0),
        sustain: FloatParam::new("sustain", "Sustain", FloatRange::linear(0.0, 100.0), 60.0)
            .with_unit("%")
            .with_precision(0),
        release: time("release", "Release", 800.0),
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
            points: (0, len, len / 3..len * 4 / 5),
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
        make("Pluck.wav", 1.5, 2, &|channel, t| {
            let detune = 1.0 + channel as f32 * 0.003;
            (tau * 220.0 * detune * t).sin() * (-t * 2.5).exp() * 0.8
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

/// The list of samples.
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
        0
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
                    for (id, row) in ids.iter().zip(rows) {
                        map.push((*id, row));
                    }
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

/// The sampler screen.
fn screen(session: &Rc<Session>, params: &Arc<Params>) -> View {
    let span = KeySpan::KEYS_61;
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
    column((
        row((
            label("Sampler").title().flex(1.0),
            Poller(Rc::clone(session)),
        )),
        waveform_view("Sample", session.wave.clone()).height(150.0),
        row((
            list_view(
                "Samples",
                SampleList(Rc::clone(session)),
                session.list.clone(),
            )
            .width(150.0),
            column((
                zone_map("Zones", span, session.zones.clone())
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
                piano_keyboard("Keyboard", span, session.keys.clone()).height(80.0),
            ))
            .flex(1.0),
            amp.width(220.0),
        ))
        .spacing(10.0)
        .flex(1.0),
    ))
    .spacing(10.0)
    .padding(12.0)
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
        let column = KeySpan::KEYS_61.column(map, note).expect("on the keyboard");
        Point::new(column.center().x, map.y + map.height * 0.5)
    }

    fn save(&self, folder: &std::path::Path, name: &str) -> std::io::Result<PathBuf> {
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

fn main() -> std::io::Result<()> {
    let folder = std::env::args_os()
        .nth(1)
        .map_or_else(std::env::temp_dir, PathBuf::from);
    let mut written = Vec::new();
    for (scale, suffix) in [(2.0, "@2x"), (1.25, "@1.25x")] {
        let session = Session::new();
        let params = params();
        let host = Rc::new(RecordingEditorHost::new().with_params(params.clone()));
        let (context, _link) = EditorContext::attach(host);
        let mut app = Driver {
            root: Root::new(screen(&session, &params), SIZE, scale, context),
            clock: Duration::ZERO,
        };
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
            "{suffix}: {} zones, envelope decay {:.0} ms, sustain {:.0} %",
            session.zones.zones().len(),
            params.decay.value(),
            params.sustain.value()
        );
    }
    for path in written {
        println!("wrote {}", path.display());
    }
    Ok(())
}
