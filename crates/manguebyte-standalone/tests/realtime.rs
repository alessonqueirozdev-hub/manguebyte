//! Proves that a standalone plugin's audio path never touches the heap:
//! the device edge, the engine with its parameter changes, the program's
//! notes and the MIDI of the ports, before and after a change of device,
//! the end of the notes of a MIDI input port that goes, and the plugin's
//! notes on their way to a MIDI output port that changes while they flow.
//!
//! The headless backend calls the stream on the test's thread, exactly as
//! a sound card's thread would, while a counting allocator watches.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::time::Duration;

use manguebyte_core::AudioBuffer;
use manguebyte_devices::{DeviceManager, DeviceRef, Direction, MockBackend};
use manguebyte_dsp::event::{EventKind, NoteRef};
use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor, Synth, Voice};
use manguebyte_midi::MidiMessage;
use manguebyte_midi::mpe::Zone;
use manguebyte_params::{FloatParam, FloatRange, param_set};
use manguebyte_plugin::output::OutputBuffer;
use manguebyte_plugin::{AudioLayout, Plugin, PluginInfo, PluginKind};
use manguebyte_standalone::{Standalone, StandaloneOptions};

thread_local! {
    static WATCHING: Cell<bool> = const { Cell::new(false) };
    static HEAP_CALLS: Cell<usize> = const { Cell::new(0) };
}

struct CountingAllocator;

impl CountingAllocator {
    fn record() {
        // `try_with` because the allocator is still called while a thread's
        // locals are being torn down.
        let _ = WATCHING.try_with(|watching| {
            if watching.get() {
                let _ = HEAP_CALLS.try_with(|calls| calls.set(calls.get() + 1));
            }
        });
    }
}

// SAFETY: every method forwards to the system allocator unchanged; the only
// addition is bumping a thread-local counter, which does not allocate.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::record();
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        Self::record();
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        Self::record();
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Runs `work` and panics if it used the heap in any way.
fn assert_no_heap_use(what: &str, work: impl FnOnce()) {
    HEAP_CALLS.with(|calls| calls.set(0));
    WATCHING.with(|watching| watching.set(true));
    work();
    WATCHING.with(|watching| watching.set(false));
    let calls = HEAP_CALLS.with(Cell::get);
    assert_eq!(
        calls, 0,
        "{what} used the heap {calls} time(s) while processing"
    );
}

#[test]
fn the_guard_itself_catches_an_allocation() {
    let caught = std::panic::catch_unwind(|| {
        assert_no_heap_use("a Vec", || drop(std::hint::black_box(vec![0u8; 64])));
    });
    WATCHING.with(|watching| watching.set(false));
    assert!(caught.is_err(), "the allocation guard is not working");
}

struct Params {
    level: FloatParam,
}
param_set!(Params { level });

#[derive(Default)]
struct Sine {
    phase: f32,
    step: f32,
}

impl Voice for Sine {
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn note_on(&mut self, note: NoteRef, _: f32) {
        self.step = f32::from(note.key) * 1e-3;
    }
    fn note_off(&mut self, _: f32) {
        self.step = 0.0;
    }
    fn choke(&mut self) {
        self.step = 0.0;
    }
    fn is_active(&self) -> bool {
        self.step > 0.0
    }
    fn render(&mut self, buffer: &mut AudioBuffer<f32>) {
        for frame in 0..buffer.num_frames() {
            self.phase = (self.phase + self.step) % 1.0;
            for channel in buffer.iter_channels_mut() {
                channel[frame] += self.phase - 0.5;
            }
        }
    }
}

/// A small synth, the kind of plugin a standalone runs.
struct Keys {
    synth: Synth<Sine>,
    params: Arc<Params>,
}

impl Plugin for Keys {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.standalone-realtime",
        name: "Keys",
        vendor: "Manguebyte tests",
        version: "0.1.0",
        description: "Plays saws",
        kind: PluginKind::Instrument,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
    type Params = Params;

    fn new() -> Self {
        Self {
            synth: Synth::with_voices(8, Sine::default),
            params: Arc::new(Params {
                level: FloatParam::new("level", "Level", FloatRange::linear(0.0, 1.0), 0.5),
            }),
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
        buffer.apply_gain(self.params.level.value());
    }
}

/// Sizes the device asks for, including some over the largest block, which
/// the edge cuts.
const SIZES: [usize; 6] = [256, 1, 64, 255, 512, 1_500];

#[test]
fn a_running_standalone_does_not_use_the_heap() {
    let mock = MockBackend::new();
    for (id, rate) in [("a", 48_000), ("b", 44_100)] {
        mock.add_device(Direction::Output, MockBackend::device(id, id, 2, rate));
    }
    mock.add_midi_port(Direction::Input, "keys", "Keys");
    let mut manager = DeviceManager::headless(&mock);
    manager.set_crossfade(Duration::ZERO);
    let mut app = Standalone::<Keys>::start_on(manager, StandaloneOptions::default()).unwrap();
    let mut out = vec![0.0f32; 2 * 1_500];

    for device in ["a", "b"] {
        // Notes from a port and from the program, and a parameter written
        // from this thread, all waiting for the next blocks.
        assert!(mock.send_midi("keys", &[0x90, 60, 100, 0x90, 64, 90]));
        assert!(app.send(MidiMessage::NoteOn {
            channel: 0,
            note: 67,
            velocity: 80,
        }));
        app.params().level.set(0.75);
        assert_no_heap_use(&format!("the plugin on {device}"), || {
            for frames in SIZES {
                assert!(mock.render(&mut out[..2 * frames]));
            }
        });
        assert!(out.iter().any(|&sample| sample != 0.0), "it played");
        // On to the other device, at another rate: prepared again, off
        // the audio thread.
        app.set_output_device("b");
    }
    assert_eq!(app.sample_rate(), 44_100.0);
}

#[test]
fn ending_the_notes_of_a_port_that_goes_does_not_use_the_heap() {
    let mock = MockBackend::new();
    mock.add_device(Direction::Output, MockBackend::device("a", "a", 2, 48_000));
    mock.add_midi_port(Direction::Input, "keys", "Keys");
    mock.add_midi_port(Direction::Input, "pads", "Pads");
    let mut manager = DeviceManager::headless(&mock);
    manager.set_crossfade(Duration::ZERO);
    let mut app = Standalone::<Keys>::start_on(manager, StandaloneOptions::default()).unwrap();
    let mut out = vec![0.0f32; 2 * 1_500];

    // Notes and a pedal held on a port closed by the program, and on one
    // unplugged: the audio thread reads what ends them as any other MIDI.
    for (port, unplug) in [("keys", false), ("pads", true)] {
        assert!(mock.send_midi(port, &[0xB0, 64, 127, 0x90, 60, 100, 0x90, 64, 90]));
        assert_no_heap_use("the notes starting", || {
            for frames in SIZES {
                assert!(mock.render(&mut out[..2 * frames]));
            }
        });
        if unplug {
            mock.remove_midi_port(Direction::Input, port);
            assert!(app.poll_devices().is_some());
        } else {
            app.set_midi_input(port, false);
        }
        assert_no_heap_use("the notes ending", || {
            for frames in SIZES {
                assert!(mock.render(&mut out[..2 * frames]));
            }
        });
    }
}

/// Sends every note it is given back out, so that notes flow to the MIDI
/// output port.
struct Echo(Arc<Params>);

impl Plugin for Echo {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.standalone-realtime-echo",
        name: "Echo",
        vendor: "Manguebyte tests",
        version: "0.1.0",
        description: "Sends its notes out",
        kind: PluginKind::Instrument,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
    const NOTE_OUTPUT: bool = true;
    type Params = Params;

    fn new() -> Self {
        Self(Arc::new(Params {
            level: FloatParam::new("level", "Level", FloatRange::linear(0.0, 1.0), 0.5),
        }))
    }
    fn params(&self) -> Arc<Params> {
        Arc::clone(&self.0)
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        self.process_with_output(buffer, context, &mut OutputBuffer::new());
    }
    fn process_with_output(
        &mut self,
        _: &mut AudioBuffer<f32>,
        context: &ProcessContext<'_, f32>,
        output: &mut OutputBuffer,
    ) {
        for event in context.events {
            match event.kind {
                EventKind::NoteOn { note, velocity } => {
                    output.note_on(event.offset, note, velocity);
                }
                EventKind::NoteOff { note, velocity } => {
                    output.note_off(event.offset, note, velocity);
                }
                _ => {}
            }
        }
    }
}

#[test]
fn changing_the_midi_output_while_notes_flow_does_not_use_the_heap() {
    let mock = MockBackend::new();
    mock.add_device(Direction::Output, MockBackend::device("a", "a", 2, 48_000));
    mock.add_midi_port(Direction::Output, "synth", "Synth");
    mock.add_midi_port(Direction::Output, "drums", "Drums");
    let mut manager = DeviceManager::headless(&mock);
    manager.set_crossfade(Duration::ZERO);
    let mut options = StandaloneOptions::default();
    options.settings.midi_output = Some(DeviceRef::new("synth", "Synth"));
    let mut app = Standalone::<Echo>::start_on(manager, options).unwrap();
    let mut out = vec![0.0f32; 2 * 1_500];

    // Each round, notes start and end, the port or the protocol changes
    // from this thread, and the device plays on with notes still sounding
    // and still waiting to be sent.
    for round in 0..8u8 {
        for key in [60, 64, 67] {
            assert!(app.send(MidiMessage::NoteOn {
                channel: 0,
                note: key + round,
                velocity: 90,
            }));
        }
        assert!(app.send(MidiMessage::NoteOff {
            channel: 0,
            note: 64 + round,
            velocity: 0,
        }));
        assert_no_heap_use(&format!("the plugin in round {round}"), || {
            for frames in SIZES {
                assert!(mock.render(&mut out[..2 * frames]));
            }
        });
        match round % 4 {
            0 => app.set_midi_output(Some("drums")),
            1 => app.set_midi_output_mpe(Zone::lower(15)),
            2 => app.set_midi_output(Some("synth")),
            _ => app.set_midi_output_mpe(None),
        };
        // The engine takes a new protocol here, ending the notes of the
        // old one, still without the heap.
        assert_no_heap_use(&format!("the change after round {round}"), || {
            assert!(mock.render(&mut out[..2 * 256]));
        });
    }
    assert_eq!(app.midi_output_name(), Some("Synth"));
    drop(app);
    assert!(!mock.take_sent_midi().is_empty(), "notes went out");
}
