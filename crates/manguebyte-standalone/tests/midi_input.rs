//! A MIDI input port that goes away while it holds notes: closed by the
//! program, unplugged, or replaced by a change of setup. The plugin must
//! hear the end of everything the port held, once, and nothing else,
//! against the headless backend, so no MIDI device is opened.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use manguebyte_core::AudioBuffer;
use manguebyte_devices::{DeviceManager, DeviceRef, Direction, MockBackend};
use manguebyte_dsp::event::{EventKind, NoteRef};
use manguebyte_dsp::note_input::Mpe;
use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor, Synth, Voice};
use manguebyte_midi::MidiMessage;
use manguebyte_params::{FloatParam, FloatRange, param_set};
use manguebyte_plugin::{AudioLayout, Plugin, PluginInfo, PluginKind};
use manguebyte_standalone::{MidiInputs, Standalone, StandaloneOptions, StandaloneSettings};

/// What a plugin hears of notes and of the sustain pedal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Heard {
    On(u8, u8),
    Off(u8, u8),
    Down(u8),
    Up(u8),
}
use Heard::{Down, Off, On, Up};

/// Everything a plugin heard, counted as a plugin that counts note-ons
/// would. It implements no channel message, so only note-offs and lifts end
/// anything for it: what a plugin may well do, since nothing obliges one to
/// read controllers 120 to 127.
#[derive(Default)]
struct Ears {
    log: Vec<Heard>,
    /// The velocity of the last note-on.
    velocity: f32,
    /// How many note-ons each note has had that no note-off has ended.
    sounding: BTreeMap<(u8, u8, Option<u32>), u32>,
    pedals: BTreeSet<u8>,
    /// Note-offs for a note that was not sounding.
    stray: usize,
}

impl Ears {
    fn hear(&mut self, kind: EventKind) {
        match kind {
            EventKind::NoteOn { note, velocity } => {
                self.log.push(On(note.channel, note.key));
                self.velocity = velocity;
                *self
                    .sounding
                    .entry((note.channel, note.key, note.id))
                    .or_default() += 1;
            }
            EventKind::NoteOff { note, .. } => {
                self.log.push(Off(note.channel, note.key));
                let key = (note.channel, note.key, note.id);
                match self.sounding.get_mut(&key) {
                    Some(1) => {
                        self.sounding.remove(&key);
                    }
                    Some(count) => *count -= 1,
                    None => self.stray += 1,
                }
            }
            EventKind::Midi(MidiMessage::ControlChange {
                channel,
                controller: 64,
                value,
            }) => {
                if value >= 64 {
                    self.log.push(Down(channel));
                    self.pedals.insert(channel);
                } else {
                    self.log.push(Up(channel));
                    self.pedals.remove(&channel);
                }
            }
            _ => {}
        }
    }
}

struct Params {
    level: FloatParam,
    ears: Mutex<Ears>,
    /// Voices of the plugin that sound, after the last block.
    voices: AtomicUsize,
}
param_set!(Params { level });

fn params() -> Arc<Params> {
    Arc::new(Params {
        level: FloatParam::new("level", "Level", FloatRange::linear(0.0, 1.0), 0.5),
        ears: Mutex::new(Ears::default()),
        voices: AtomicUsize::new(0),
    })
}

/// Writes down what it hears, reading MPE in a lower zone if asked.
struct Listener<const LOWER_ZONE: bool>(Arc<Params>);

impl<const LOWER_ZONE: bool> Plugin for Listener<LOWER_ZONE> {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.standalone-listener",
        name: "Listener",
        vendor: "Manguebyte tests",
        version: "0.1.0",
        description: "Writes down what it hears",
        kind: PluginKind::Instrument,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
    const MPE: Mpe = if LOWER_ZONE {
        Mpe::LOWER_ZONE
    } else {
        Mpe::Follow
    };
    type Params = Params;

    fn new() -> Self {
        Self(params())
    }
    fn params(&self) -> Arc<Params> {
        Arc::clone(&self.0)
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, _: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        let mut ears = self.0.ears.lock().unwrap();
        for event in context.events {
            ears.hear(event.kind);
        }
    }
}

type Plain = Listener<false>;

/// A voice that sounds from its note-on to its note-off.
#[derive(Default)]
struct Tone(bool);

impl Voice for Tone {
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn note_on(&mut self, _: NoteRef, _: f32) {
        self.0 = true;
    }
    fn note_off(&mut self, _: f32) {
        self.0 = false;
    }
    fn choke(&mut self) {
        self.0 = false;
    }
    fn is_active(&self) -> bool {
        self.0
    }
    fn render(&mut self, _: &mut AudioBuffer<f32>) {}
}

/// A real synth, which says how many of its voices sound, reading MPE in
/// a lower zone if asked.
struct Keys<const LOWER_ZONE: bool = false> {
    synth: Synth<Tone>,
    params: Arc<Params>,
}

impl<const LOWER_ZONE: bool> Plugin for Keys<LOWER_ZONE> {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.standalone-keys",
        name: "Keys",
        vendor: "Manguebyte tests",
        version: "0.1.0",
        description: "Says how many voices sound",
        kind: PluginKind::Instrument,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
    const MPE: Mpe = if LOWER_ZONE {
        Mpe::LOWER_ZONE
    } else {
        Mpe::Follow
    };
    type Params = Params;

    fn new() -> Self {
        Self {
            synth: Synth::with_voices(8, Tone::default),
            params: params(),
        }
    }
    fn params(&self) -> Arc<Params> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.synth.prepare(spec);
    }
    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        self.synth.process_with(buffer, context);
        self.params
            .voices
            .store(self.synth.active_voices(), Ordering::Relaxed);
    }
}

/// Speakers, and two keyboards.
fn studio() -> MockBackend {
    let mock = MockBackend::new();
    mock.add_device(
        Direction::Output,
        MockBackend::device("speakers", "Speakers", 2, 48_000),
    );
    mock.add_midi_port(Direction::Input, "keys", "Keys");
    mock.add_midi_port(Direction::Input, "pads", "Pads");
    mock
}

fn start_with<P: Plugin>(mock: &MockBackend, settings: StandaloneSettings) -> Standalone<P> {
    let mut manager = DeviceManager::headless(mock);
    manager.set_crossfade(Duration::ZERO);
    let options = StandaloneOptions {
        settings,
        ..StandaloneOptions::default()
    };
    Standalone::<P>::start_on(manager, options).unwrap()
}

/// With every port open.
fn start<P: Plugin>(mock: &MockBackend) -> Standalone<P> {
    start_with(mock, StandaloneSettings::default())
}

/// The settings that name the keyboards one by one.
fn named() -> StandaloneSettings {
    StandaloneSettings::default().with_midi_inputs(MidiInputs::Only(vec![
        DeviceRef::new("keys", "Keys"),
        DeviceRef::new("pads", "Pads"),
    ]))
}

/// Plays a block, and takes what the plugin heard since the last call.
fn heard<P: Plugin<Params = Params>>(app: &Standalone<P>, mock: &MockBackend) -> Vec<Heard> {
    let mut block = [0.0f32; 2 * 64];
    assert!(mock.render(&mut block));
    std::mem::take(&mut app.params().ears.lock().unwrap().log)
}

/// That the plugin has been left with nothing sounding, and that it never
/// heard a note-off for a note that was not.
fn assert_silent<P: Plugin<Params = Params>>(app: &Standalone<P>) {
    let params = app.params();
    let ears = params.ears.lock().unwrap();
    assert!(ears.sounding.is_empty(), "{:?}", ears.sounding);
    assert!(ears.pedals.is_empty(), "{:?}", ears.pedals);
    assert_eq!(ears.stray, 0);
}

#[test]
fn a_note_held_on_a_closed_port_ends_in_the_plugin() {
    let mock = studio();
    let mut app = start::<Plain>(&mock);
    assert!(mock.send_midi("keys", &[0x90, 60, 100, 0x91, 64, 90]));
    assert_eq!(heard(&app, &mock), [On(0, 60), On(1, 64)]);

    app.set_midi_input("keys", false);
    assert_eq!(heard(&app, &mock), [Off(0, 60), Off(1, 64)]);
    assert_silent(&app);
    assert_eq!(heard(&app, &mock), [], "once");
}

#[test]
fn a_note_held_on_an_unplugged_port_ends_in_the_plugin() {
    // Every port: the standalone closes it. Ports by name: the device
    // manager follows the setup.
    for settings in [StandaloneSettings::default(), named()] {
        let mock = studio();
        let mut app = start_with::<Plain>(&mock, settings);
        assert!(mock.send_midi("keys", &[0x90, 60, 100]));
        assert_eq!(heard(&app, &mock), [On(0, 60)]);

        mock.remove_midi_port(Direction::Input, "keys");
        assert_eq!(heard(&app, &mock), [], "not noticed until the next poll");
        assert!(app.poll_devices().is_some());
        assert_eq!(heard(&app, &mock), [Off(0, 60)]);
        assert_silent(&app);
        assert_eq!(heard(&app, &mock), [], "once");
    }
}

#[test]
fn a_port_a_change_of_setup_leaves_ends_its_notes() {
    let mock = studio();
    let settings = named();
    let mut app = start_with::<Plain>(&mock, settings.clone());
    assert!(mock.send_midi("keys", &[0x90, 60, 100]));
    assert!(mock.send_midi("pads", &[0x90, 64, 100]));
    assert_eq!(heard(&app, &mock), [On(0, 60), On(0, 64)]);

    // The port that stays keeps its note.
    app.apply_settings(
        settings.with_midi_inputs(MidiInputs::Only(vec![DeviceRef::new("pads", "Pads")])),
    );
    assert_eq!(heard(&app, &mock), [Off(0, 60)]);
    assert!(mock.send_midi("pads", &[0x80, 64, 0]));
    assert_eq!(heard(&app, &mock), [Off(0, 64)]);
    assert_silent(&app);
}

#[test]
fn a_port_replaced_by_one_of_its_name_ends_its_notes() {
    let mock = studio();
    let mut app = start_with::<Plain>(&mock, named());
    assert!(mock.send_midi("keys", &[0x90, 60, 100]));
    assert_eq!(heard(&app, &mock), [On(0, 60)]);

    // Plugged in again under another identifier: the setup finds it by
    // its name.
    mock.remove_midi_port(Direction::Input, "keys");
    mock.add_midi_port(Direction::Input, "keys-2", "Keys");
    assert!(app.poll_devices().is_some());
    assert_eq!(heard(&app, &mock), [Off(0, 60)]);
    assert_silent(&app);

    // The new port plays, and has nothing of the old one's to end.
    assert!(mock.send_midi("keys-2", &[0x80, 60, 0, 0x90, 62, 100, 0x80, 62, 0]));
    assert_eq!(heard(&app, &mock), [On(0, 62), Off(0, 62)]);
    assert_silent(&app);
}

#[test]
fn a_held_pedal_is_lifted_after_the_notes_end() {
    let mock = studio();
    let mut app = start::<Plain>(&mock);
    // Down on channels 1 and 4, down and up on 6, with a note let go under
    // the pedal on 1 and one still held on 4.
    assert!(mock.send_midi(
        "keys",
        &[
            0xB0, 64, 127, 0xB3, 64, 70, 0xB5, 64, 127, 0xB5, 64, 0, 0x90, 60, 100, 0x80, 60, 0,
            0x93, 62, 100,
        ],
    ));
    assert_eq!(
        heard(&app, &mock),
        [
            Down(0),
            Down(3),
            Down(5),
            Up(5),
            On(0, 60),
            Off(0, 60),
            On(3, 62)
        ]
    );

    app.set_midi_input("keys", false);
    assert_eq!(heard(&app, &mock), [Off(3, 62), Up(0), Up(3)]);
    assert_silent(&app);
    assert_eq!(heard(&app, &mock), [], "once");
}

#[test]
fn a_real_synth_lets_go_of_its_voices() {
    let mock = studio();
    let mut app = start::<Keys>(&mock);
    let params = app.params_arc();
    let voices = |mock: &MockBackend| {
        let mut block = [0.0f32; 2 * 64];
        assert!(mock.render(&mut block));
        params.voices.load(Ordering::Relaxed)
    };
    // One note held, and one kept by the pedal after its note-off.
    assert!(mock.send_midi(
        "keys",
        &[0xB0, 64, 127, 0x90, 60, 100, 0x90, 64, 100, 0x80, 64, 0],
    ));
    assert_eq!(voices(&mock), 2);
    mock.remove_midi_port(Direction::Input, "keys");
    assert_eq!(voices(&mock), 2, "not noticed until the next poll");
    app.poll_devices().unwrap();
    assert_eq!(voices(&mock), 0);
}

#[test]
fn a_note_the_port_already_ended_gets_nothing_more() {
    let mock = studio();
    let mut app = start::<Plain>(&mock);
    assert!(mock.send_midi(
        "keys",
        &[
            // A note-off, a note-on of velocity zero.
            0x90, 60, 100, 0x80, 60, 40, 0x90, 62, 100, 0x90, 62, 0, // A pedal let up.
            0xB0, 64, 127, 0xB0, 64, 0,
        ],
    ));
    assert_eq!(heard(&app, &mock).len(), 6);
    assert_silent(&app);

    app.set_midi_input("keys", false);
    assert_eq!(heard(&app, &mock), []);
    assert_silent(&app);
}

#[test]
fn a_plugin_that_reads_no_channel_messages_hears_the_end_on_closing() {
    // Only the plugin knows whether these ended anything, and a note-off
    // or a lift more does no harm.
    for controller in [120, 123, 124, 125, 126, 127] {
        let mock = studio();
        let mut app = start::<Plain>(&mock);
        assert!(mock.send_midi("keys", &[0x90, 60, 100, 0xB0, controller, 0]));
        assert_eq!(heard(&app, &mock), [On(0, 60)]);
        app.set_midi_input("keys", false);
        assert_eq!(heard(&app, &mock), [Off(0, 60)], "controller {controller}");
        assert_silent(&app);
        assert_eq!(heard(&app, &mock), [], "once");
    }

    let mock = studio();
    let mut app = start::<Plain>(&mock);
    assert!(mock.send_midi("keys", &[0xB1, 64, 127, 0xB1, 121, 0]));
    assert_eq!(heard(&app, &mock), [Down(1)]);
    app.set_midi_input("keys", false);
    assert_eq!(heard(&app, &mock), [Up(1)]);
    assert_silent(&app);
    assert_eq!(heard(&app, &mock), [], "once");
}

#[test]
fn a_note_ended_by_a_channel_message_and_let_go_gets_nothing_more() {
    let mock = studio();
    let mut app = start::<Plain>(&mock);
    assert!(mock.send_midi("keys", &[0x90, 60, 100, 0xB0, 123, 0, 0x80, 60, 0]));
    assert!(mock.send_midi("keys", &[0xB1, 64, 127, 0xB1, 121, 0, 0xB1, 64, 0]));
    assert_eq!(heard(&app, &mock), [On(0, 60), Off(0, 60), Down(1), Up(1)]);
    app.set_midi_input("keys", false);
    assert_eq!(heard(&app, &mock), []);
    assert_silent(&app);
}

#[test]
fn a_real_synth_lets_go_at_every_channel_message_and_port_closing() {
    let mock = studio();
    let mut app = start::<Keys>(&mock);
    let params = app.params_arc();
    let voices = |mock: &MockBackend| {
        let mut block = [0.0f32; 2 * 64];
        assert!(mock.render(&mut block));
        params.voices.load(Ordering::Relaxed)
    };
    // Omni off, omni on, mono, poly and all notes off release; all sound
    // off cuts.
    for controller in [120, 123, 124, 125, 126, 127] {
        assert!(mock.send_midi("keys", &[0x90, 60, 100]));
        assert_eq!(voices(&mock), 1);
        assert!(mock.send_midi("keys", &[0xB0, controller, 0]));
        assert_eq!(voices(&mock), 0, "controller {controller}");
    }
    // A key still down, and the port closes: nothing comes back.
    app.set_midi_input("keys", false);
    assert_eq!(voices(&mock), 0);
}

#[test]
fn a_note_off_after_all_notes_off_still_passes() {
    // A plugin reading MPE ignores all notes off on a member channel, so
    // the note goes on, and its note-off must reach it.
    let mock = studio();
    let app = start::<Listener<true>>(&mock);
    assert!(mock.send_midi("keys", &[0x91, 60, 100, 0xB1, 123, 0]));
    assert_eq!(heard(&app, &mock), [On(0, 60)]);
    assert!(mock.send_midi("keys", &[0x81, 60, 0]));
    assert_eq!(heard(&app, &mock), [Off(0, 60)]);
    assert_silent(&app);
    // Once.
    assert!(mock.send_midi("keys", &[0x81, 60, 0]));
    assert_eq!(heard(&app, &mock), []);
}

#[test]
fn a_channel_message_on_the_master_channel_ends_a_member_channels_note() {
    for controller in [120, 123, 126] {
        let mock = studio();
        let mut app = start::<Keys<true>>(&mock);
        let params = app.params_arc();
        let voices = |mock: &MockBackend| {
            let mut block = [0.0f32; 2 * 64];
            assert!(mock.render(&mut block));
            params.voices.load(Ordering::Relaxed)
        };
        assert!(mock.send_midi("keys", &[0x91, 60, 100, 0x92, 64, 100]));
        assert_eq!(voices(&mock), 2);
        assert!(mock.send_midi("keys", &[0xB0, controller, 0]));
        assert_eq!(voices(&mock), 0, "controller {controller}");
        app.set_midi_input("keys", false);
        assert_eq!(voices(&mock), 0);
    }
}

#[test]
fn all_notes_off_on_a_member_channel_alone_leaves_nothing_hanging() {
    // `NoteInput` ignores it there, so the note goes on, and the port's
    // closing ends it.
    let mock = studio();
    let mut app = start::<Listener<true>>(&mock);
    assert!(mock.send_midi("keys", &[0x91, 60, 100, 0xB1, 123, 0]));
    assert_eq!(heard(&app, &mock), [On(0, 60)]);
    app.set_midi_input("keys", false);
    assert_eq!(heard(&app, &mock), [Off(0, 60)]);
    assert_silent(&app);
}

#[test]
fn a_ports_channel_message_reaches_the_notes_of_every_port() {
    // These speak for a channel, and the plugin hears no ports, so the
    // rule that a note lasts until the last port lets go stops here.
    let mock = studio();
    let app = start::<Keys>(&mock);
    let params = app.params_arc();
    let voices = |mock: &MockBackend| {
        let mut block = [0.0f32; 2 * 64];
        assert!(mock.render(&mut block));
        params.voices.load(Ordering::Relaxed)
    };
    assert!(mock.send_midi("keys", &[0x90, 60, 100]));
    assert!(mock.send_midi("pads", &[0x90, 64, 100]));
    assert_eq!(voices(&mock), 2);
    assert!(mock.send_midi("keys", &[0xB0, 123, 0]));
    assert_eq!(voices(&mock), 0, "the other port's key is still down");
    assert!(mock.send_midi("pads", &[0x80, 64, 0]));
    assert_eq!(voices(&mock), 0);

    // A reset of the controllers lifts the pedal another port holds down.
    assert!(mock.send_midi("pads", &[0xB0, 64, 127, 0x90, 64, 100, 0x80, 64, 0]));
    assert_eq!(voices(&mock), 1);
    assert!(mock.send_midi("keys", &[0xB0, 121, 0]));
    assert_eq!(voices(&mock), 0);
}

#[test]
fn a_velocity_prefix_reaches_the_note_it_came_with() {
    let mock = studio();
    let app = start::<Plain>(&mock);
    let velocity = |low: u16| f32::from((0x40u16 << 7) | low) / 16_383.0;
    let last = || app.params().ears.lock().unwrap().velocity;

    assert!(mock.send_midi("keys", &[0xB0, 88, 0x55, 0x90, 60, 0x40]));
    assert_eq!(heard(&app, &mock), [On(0, 60)]);
    assert_eq!(last(), velocity(0x55));

    // The note-off of a second strike goes before the prefix, not between
    // it and its note.
    assert!(mock.send_midi("keys", &[0xB0, 88, 0x2A, 0x90, 60, 0x40]));
    assert_eq!(heard(&app, &mock), [Off(0, 60), On(0, 60)]);
    assert_eq!(last(), velocity(0x2A));

    // A prefix goes with a note-off that is dropped, and the next note,
    // of any port, has none.
    assert!(mock.send_midi("keys", &[0xB0, 88, 0x11, 0x80, 61, 0x40]));
    assert_eq!(heard(&app, &mock), []);
    assert!(mock.send_midi("pads", &[0x90, 62, 0x40]));
    assert_eq!(heard(&app, &mock), [On(0, 62)]);
    assert_eq!(last(), 0x40 as f32 / 127.0);

    // Two ports' prefixes do not mix.
    assert!(mock.send_midi("keys", &[0xB0, 88, 0x33]));
    assert!(mock.send_midi("pads", &[0xB0, 88, 0x22, 0x90, 63, 0x40]));
    assert_eq!(heard(&app, &mock), [On(0, 63)]);
    assert_eq!(last(), velocity(0x22));
    assert!(mock.send_midi("keys", &[0x90, 64, 0x40]));
    assert_eq!(heard(&app, &mock), [On(0, 64)]);
    assert_eq!(last(), velocity(0x33));
}

#[test]
fn what_the_program_plays_is_not_told_apart_from_a_port() {
    let mock = studio();
    let mut app = start::<Plain>(&mock);
    assert!(app.send(MidiMessage::NoteOn {
        channel: 0,
        note: 60,
        velocity: 100,
    }));
    assert!(mock.send_midi("keys", &[0x90, 60, 100]));
    assert_eq!(heard(&app, &mock), [On(0, 60), On(0, 60)]);
    // One note-off, which a plugin that does not count takes for both.
    app.set_midi_input("keys", false);
    assert_eq!(heard(&app, &mock), [Off(0, 60)]);
}

/// Also in TECH_DEBT item 120: the port never left the list, so nothing
/// closes it and nothing ends its note. What the connection then does on a
/// real device is not known; the mock's goes on delivering.
#[test]
fn a_port_gone_and_back_between_two_polls_is_not_seen_to_go() {
    let mock = studio();
    let mut app = start::<Plain>(&mock);
    assert!(mock.send_midi("keys", &[0x90, 60, 100]));
    assert_eq!(heard(&app, &mock), [On(0, 60)]);
    mock.remove_midi_port(Direction::Input, "keys");
    mock.add_midi_port(Direction::Input, "keys", "Keys");
    assert!(app.poll_devices().is_none());
    assert_eq!(heard(&app, &mock), []);
    assert_eq!(app.params().ears.lock().unwrap().sounding.len(), 1);
}

#[test]
fn another_open_ports_notes_keep_sounding() {
    let mock = studio();
    let mut app = start::<Plain>(&mock);
    assert!(mock.send_midi("keys", &[0x90, 60, 100]));
    assert!(mock.send_midi("pads", &[0x90, 64, 100, 0xB1, 64, 127]));
    assert_eq!(heard(&app, &mock), [On(0, 60), On(0, 64), Down(1)]);

    app.set_midi_input("keys", false);
    assert_eq!(heard(&app, &mock), [Off(0, 60)]);
    assert_eq!(
        app.params()
            .ears
            .lock()
            .unwrap()
            .sounding
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        [(0, 64, None)]
    );

    // And it goes on as if nothing happened.
    assert!(mock.send_midi("pads", &[0x80, 64, 0, 0xB1, 64, 0]));
    assert_eq!(heard(&app, &mock), [Off(0, 64), Up(1)]);
    assert_silent(&app);
}

#[test]
fn closing_one_of_two_ports_on_a_note_leaves_it_to_the_other() {
    let mock = studio();
    let mut app = start::<Plain>(&mock);
    assert!(mock.send_midi("keys", &[0x90, 60, 100]));
    assert_eq!(heard(&app, &mock), [On(0, 60)]);
    // The second strike is heard, and a plugin that counts is left with
    // one note.
    assert!(mock.send_midi("pads", &[0x90, 60, 80]));
    assert_eq!(heard(&app, &mock), [Off(0, 60), On(0, 60)]);

    app.set_midi_input("keys", false);
    assert_eq!(heard(&app, &mock), [], "the other port holds it");
    assert!(mock.send_midi("pads", &[0x80, 60, 0]));
    assert_eq!(heard(&app, &mock), [Off(0, 60)]);
    assert_silent(&app);
}

#[test]
fn a_note_two_ports_hold_ends_when_the_last_lets_go() {
    let mock = studio();
    let mut app = start::<Plain>(&mock);
    assert!(mock.send_midi("keys", &[0x90, 60, 100]));
    assert!(mock.send_midi("pads", &[0x90, 60, 80]));
    assert_eq!(heard(&app, &mock), [On(0, 60), Off(0, 60), On(0, 60)]);
    assert!(mock.send_midi("keys", &[0x80, 60, 0]));
    assert_eq!(heard(&app, &mock), []);

    // By closing, too, and just the once.
    app.set_midi_input("pads", false);
    assert_eq!(heard(&app, &mock), [Off(0, 60)]);
    assert_silent(&app);
    assert_eq!(heard(&app, &mock), []);
}

#[test]
fn the_same_channel_on_two_ports_is_not_the_same_note() {
    // The other way a pair of ports could get in each other's way: another
    // channel or key is another note, and ends on its own.
    let mock = studio();
    let mut app = start::<Plain>(&mock);
    assert!(mock.send_midi("keys", &[0x90, 60, 100, 0x91, 62, 100]));
    assert!(mock.send_midi("pads", &[0x90, 62, 100, 0x91, 60, 100]));
    heard(&app, &mock);
    app.set_midi_input("keys", false);
    assert_eq!(heard(&app, &mock), [Off(0, 60), Off(1, 62)]);
    app.set_midi_input("pads", false);
    assert_eq!(heard(&app, &mock), [Off(0, 62), Off(1, 60)]);
    assert_silent(&app);
}

#[test]
fn a_pedal_two_ports_hold_lifts_when_the_last_lets_go() {
    let mock = studio();
    let mut app = start::<Plain>(&mock);
    assert!(mock.send_midi("keys", &[0xB0, 64, 127]));
    assert!(mock.send_midi("pads", &[0xB0, 64, 127]));
    assert_eq!(heard(&app, &mock), [Down(0), Down(0)]);

    app.set_midi_input("keys", false);
    assert_eq!(heard(&app, &mock), [], "the other port holds it down");
    assert!(mock.send_midi("pads", &[0xB0, 64, 0]));
    assert_eq!(heard(&app, &mock), [Up(0)]);

    // A port letting up while another holds it down is not heard either.
    app.set_midi_input("keys", true);
    assert!(mock.send_midi("keys", &[0xB0, 64, 127]));
    assert!(mock.send_midi("pads", &[0xB0, 64, 127]));
    assert_eq!(heard(&app, &mock), [Down(0), Down(0)]);
    assert!(mock.send_midi("pads", &[0xB0, 64, 0]));
    assert_eq!(heard(&app, &mock), []);
    assert!(mock.send_midi("keys", &[0xB0, 64, 0]));
    assert_eq!(heard(&app, &mock), [Up(0)]);
    assert_silent(&app);

    // With no pedal down anywhere, a lift changes nothing, and passes.
    assert!(mock.send_midi("pads", &[0xB0, 64, 0]));
    assert_eq!(heard(&app, &mock), [Up(0)]);
}

#[test]
fn a_key_struck_twice_by_one_port_needs_one_note_off() {
    let mock = studio();
    let mut app = start::<Plain>(&mock);
    assert!(mock.send_midi("keys", &[0x90, 60, 100, 0x90, 60, 80]));
    assert_eq!(heard(&app, &mock), [On(0, 60), Off(0, 60), On(0, 60)]);
    app.set_midi_input("keys", false);
    assert_eq!(heard(&app, &mock), [Off(0, 60)]);
    assert_silent(&app);
}

#[test]
fn a_port_opened_again_plays_again() {
    let mock = studio();
    let mut app = start::<Plain>(&mock);
    assert!(mock.send_midi("keys", &[0x90, 60, 100, 0x90, 62, 100]));
    heard(&app, &mock);
    app.set_midi_input("keys", false);
    assert_eq!(heard(&app, &mock), [Off(0, 60), Off(0, 62)]);
    assert!(!mock.send_midi("keys", &[0x90, 60, 100]), "closed");

    app.set_midi_input("keys", true);
    // A key held down all the while comes up: the plugin has heard its end.
    assert!(mock.send_midi("keys", &[0x80, 62, 0]));
    assert_eq!(heard(&app, &mock), []);
    // New notes play and end as ever, one of them on the same key.
    assert!(mock.send_midi("keys", &[0x90, 62, 90, 0x90, 67, 90]));
    assert_eq!(heard(&app, &mock), [On(0, 62), On(0, 67)]);
    assert!(mock.send_midi("keys", &[0x80, 62, 0]));
    assert_eq!(heard(&app, &mock), [Off(0, 62)]);
    // And what is held when it closes again ends again.
    app.set_midi_input("keys", false);
    assert_eq!(heard(&app, &mock), [Off(0, 67)]);
    assert_silent(&app);
}

#[test]
fn an_unplugged_port_that_comes_back_plays_again() {
    let mock = studio();
    let mut app = start::<Plain>(&mock);
    assert!(mock.send_midi("keys", &[0x90, 60, 100, 0xB0, 64, 127]));
    heard(&app, &mock);
    mock.remove_midi_port(Direction::Input, "keys");
    app.poll_devices().unwrap();
    assert_eq!(heard(&app, &mock), [Off(0, 60), Up(0)]);

    mock.add_midi_port(Direction::Input, "keys", "Keys");
    app.poll_devices().unwrap();
    assert!(mock.send_midi("keys", &[0x90, 60, 100, 0x80, 60, 0]));
    assert_eq!(heard(&app, &mock), [On(0, 60), Off(0, 60)]);
    assert_silent(&app);
}

#[test]
fn notes_on_member_channels_end_like_any_others() {
    let mock = studio();
    let mut app = start::<Listener<true>>(&mock);
    // One key on two member channels: two notes, reported on the master
    // channel with an identifier each, and the zone's pedal.
    assert!(mock.send_midi("keys", &[0x91, 60, 100, 0x92, 60, 90, 0xB0, 64, 127]));
    assert!(mock.send_midi("pads", &[0x93, 64, 100]));
    assert_eq!(
        heard(&app, &mock),
        [On(0, 60), On(0, 60), Down(0), On(0, 64)]
    );
    assert_eq!(app.params().ears.lock().unwrap().sounding.len(), 3);

    app.set_midi_input("keys", false);
    assert_eq!(heard(&app, &mock), [Off(0, 60), Off(0, 60), Up(0)]);
    assert_eq!(app.params().ears.lock().unwrap().sounding.len(), 1);
    assert!(mock.send_midi("pads", &[0x83, 64, 0]));
    assert_eq!(heard(&app, &mock), [Off(0, 64)]);
    assert_silent(&app);
}

#[test]
fn ports_closed_while_notes_flow_leave_nothing_hanging() {
    let mock = studio();
    let mut app = start::<Plain>(&mock);
    // Two threads play on the two ports, as the system's would, striking
    // and letting go a few keys at random while the program closes and
    // opens the ports and the audio plays.
    let players: Vec<_> = [("keys", 0x2545_F491u32), ("pads", 0x9E37_79B9)]
        .into_iter()
        .map(|(port, mut seed)| {
            let mock = mock.clone();
            std::thread::spawn(move || {
                for _ in 0..300 {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    let channel = (seed & 1) as u8;
                    let key = 60 + (seed >> 1 & 3) as u8;
                    let message = match seed >> 3 & 3 {
                        0 => [0xB0 | channel, 64, 127],
                        1 => [0xB0 | channel, 64, 0],
                        2 => [0x90 | channel, key, 100],
                        _ => [0x80 | channel, key, 0],
                    };
                    mock.send_midi(port, &message);
                    std::thread::sleep(Duration::from_micros(200));
                }
            })
        })
        .collect();
    let mut block = [0.0f32; 2 * 64];
    let mut open = [true, true];
    let mut turn = 0;
    while players.iter().any(|player| !player.is_finished()) {
        assert!(mock.render(&mut block));
        turn += 1;
        if turn % 8 == 0 {
            let which = (turn / 8) % 2;
            open[which] = !open[which];
            app.set_midi_input(["keys", "pads"][which], open[which]);
        }
        std::thread::sleep(Duration::from_micros(200));
    }
    for player in players {
        player.join().unwrap();
    }
    app.set_midi_input("keys", false);
    app.set_midi_input("pads", false);
    assert!(mock.render(&mut block));
    assert_silent(&app);
    let params = app.params();
    let ears = params.ears.lock().unwrap();
    assert!(ears.log.iter().any(|heard| matches!(heard, Off(..))));
}
