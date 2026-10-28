//! Reads what hosted plugins send: a note effect and a plugin that sends
//! more than a host keeps, linked into this test and hosted through the
//! framework's own wrappers, in process, as LV2 plugins and as Audio
//! Units, and the note effect through the one interface over every
//! format.
//!
//! LV2 and Audio Units carry what a plugin sends as MIDI 1.0, which the
//! host reads back into the framework's events on their frames of the
//! caller's block. However the caller cuts its blocks, and however the
//! host cuts them again into the pieces a plugin is run for, the same
//! events have to come out on the same frames, and nothing of the host's
//! or the wrappers' code may use the heap while they do.
//!
//! Not under Miri, which is too slow for the wrappers.
#![cfg(all(any(feature = "lv2", feature = "au"), not(miri)))]

mod common;

use std::sync::Arc;

use manguebyte_core::AudioBuffer;
use manguebyte_dsp::event::{Event, EventBuffer, EventKind, Expression, NoteRef};
use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor, Transport};
use manguebyte_midi::MidiMessage;
use manguebyte_params::param_set;
use manguebyte_plugin::output::OutputBuffer;
use manguebyte_plugin::{AudioLayout, MidiControllers, Plugin, PluginInfo, PluginKind};

use common::heap_calls_during;

const RATE: f64 = 48_000.0;

// --- The plugins ----------------------------------------------------------------

struct NoParams {}
param_set!(NoParams {});

/// Plays every note it is given an octave up, with the velocity and the
/// pressure it is given, and sends every other MIDI message on as it is.
struct Octave {
    params: Arc<NoParams>,
}

impl Plugin for Octave {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.octave",
        name: "Octave",
        vendor: "Manguebyte",
        version: "0.1.0",
        description: "Plays an octave up",
        kind: PluginKind::NoteEffect,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
    const NOTE_OUTPUT: bool = true;
    // A plugin that passes controllers on keeps their channels, so it asks
    // VST 3, which moves controllers as parameters, for one per channel.
    const MIDI_CONTROLLERS: MidiControllers = MidiControllers::MUSICAL.per_channel();
    type Params = NoParams;

    fn new() -> Self {
        Self {
            params: Arc::new(NoParams {}),
        }
    }
    fn params(&self) -> Arc<NoParams> {
        Arc::clone(&self.params)
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
            let _ = match octave_of(event.kind) {
                Some(EventKind::NoteOn { note, velocity }) => {
                    output.note_on(event.offset, note, velocity)
                }
                Some(EventKind::NoteOff { note, velocity }) => {
                    output.note_off(event.offset, note, velocity)
                }
                Some(EventKind::NoteExpression {
                    note,
                    expression,
                    value,
                }) => output.expression(event.offset, note, expression, value),
                Some(EventKind::Midi(message)) => output.midi1(event.offset, message),
                _ => false,
            };
        }
    }
}

/// What Octave sends for an event it is given.
fn octave_of(kind: EventKind) -> Option<EventKind> {
    let up = |note: NoteRef| NoteRef::new(note.channel, note.key + 12);
    Some(match kind {
        EventKind::NoteOn { note, velocity } => EventKind::NoteOn {
            note: up(note),
            velocity,
        },
        EventKind::NoteOff { note, velocity } => EventKind::NoteOff {
            note: up(note),
            velocity,
        },
        EventKind::NoteExpression {
            note,
            expression,
            value,
        } => EventKind::NoteExpression {
            note: up(note),
            expression,
            value,
        },
        EventKind::Midi(message) => EventKind::Midi(message),
        _ => return None,
    })
}

/// Sends a controller on every frame it is run for, whatever it is given:
/// more than a host keeps for a long block.
struct Flood {
    params: Arc<NoParams>,
}

impl Plugin for Flood {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.flood",
        name: "Flood",
        vendor: "Manguebyte",
        version: "0.1.0",
        description: "Sends a controller on every frame",
        kind: PluginKind::NoteEffect,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
    const NOTE_OUTPUT: bool = true;
    type Params = NoParams;

    fn new() -> Self {
        Self {
            params: Arc::new(NoParams {}),
        }
    }
    fn params(&self) -> Arc<NoParams> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        self.process_with_output(buffer, context, &mut OutputBuffer::new());
    }
    fn process_with_output(
        &mut self,
        buffer: &mut AudioBuffer<f32>,
        _: &ProcessContext<'_, f32>,
        output: &mut OutputBuffer,
    ) {
        for frame in 0..buffer.num_frames() {
            let _ = output.midi1(frame as u32, flood_at(frame));
        }
    }
}

/// What Flood sends on `frame` of the piece it is run for.
fn flood_at(frame: usize) -> MidiMessage {
    MidiMessage::ControlChange {
        channel: 0,
        controller: 1,
        value: (frame & 0x7F) as u8,
    }
}

// --- What the plugins are given -----------------------------------------------------

/// A velocity or a value MIDI 1.0 carries exactly: `steps` 127ths.
fn byte(steps: u8) -> f64 {
    f64::from(steps) / 127.0
}

/// Notes, pressure on one of them, a controller and the wheel, as MIDI
/// 1.0 carries them, on frames that fall at and around the edges of the
/// pieces the hosts cut, with two notes on one frame. (VST 3 keeps a
/// controller apart from notes, as a parameter, so a controller has a
/// frame of its own for the order to be the same in every format.)
fn song() -> Vec<(u64, EventKind)> {
    let low = NoteRef::new(0, 60);
    let bass = NoteRef::new(2, 36);
    let high = NoteRef::new(1, 64);
    let next = NoteRef::new(0, 62);
    let on = |note, steps| EventKind::NoteOn {
        note,
        velocity: byte(steps) as f32,
    };
    let off = |note, steps| EventKind::NoteOff {
        note,
        velocity: byte(steps) as f32,
    };
    vec![
        (10, on(low, 100)),
        (10, on(bass, 70)),
        (
            11,
            EventKind::Midi(MidiMessage::ControlChange {
                channel: 0,
                controller: 1,
                value: 64,
            }),
        ),
        (255, on(high, 90)),
        (
            256,
            EventKind::NoteExpression {
                note: high,
                expression: Expression::Pressure,
                value: byte(40),
            },
        ),
        (
            700,
            EventKind::Midi(MidiMessage::PitchBend {
                channel: 1,
                value: 1_234,
            }),
        ),
        (1_023, off(low, 0)),
        (1_024, on(next, 127)),
        (3_000, off(high, 30)),
        (4_000, off(bass, 10)),
        (5_999, off(next, 64)),
    ]
}

/// What Octave sends for the song, on the song's frames.
fn octave_song() -> Vec<(u64, EventKind)> {
    song()
        .into_iter()
        .filter_map(|(frame, kind)| Some((frame, octave_of(kind)?)))
        .collect()
}

/// How long the song is played for.
const FRAMES: usize = 6_000;

/// Plays `schedule` through `processor` for `frames` frames, in blocks
/// whose lengths cycle through `pattern`, and returns every event `sent`
/// reads after each block, on its frame counted from the start, with the
/// heap calls made inside the processor.
fn drive<P: Processor<f32>>(
    processor: &mut P,
    sent: fn(&P) -> &[Event],
    schedule: &[(u64, EventKind)],
    pattern: &[usize],
    frames: usize,
) -> (Vec<(u64, EventKind)>, usize) {
    let mut schedule = schedule.to_vec();
    schedule.sort_by_key(|&(frame, _)| frame);
    let mut due = schedule.iter().peekable();
    let mut events = EventBuffer::with_capacity(256);
    let mut transport = Transport::playing_at(120.0);
    let longest = pattern.iter().copied().max().unwrap_or(1);
    let mut audio = AudioBuffer::<f32>::new(2, longest);
    let mut heard = Vec::new();
    let mut heap_calls = 0;
    let mut done = 0;
    let mut blocks = 0;
    while done < frames {
        let length = pattern[blocks % pattern.len()].min(frames - done);
        let end = (done + length) as u64;
        events.clear();
        while let Some(&&(frame, kind)) = due.peek() {
            if frame >= end {
                break;
            }
            let offset = (frame - done as u64) as u32;
            events
                .push(Event { offset, kind })
                .expect("room for the song");
            due.next();
        }
        let context = ProcessContext::new(&events, &transport);
        audio.set_num_frames(length);
        heap_calls += heap_calls_during(|| processor.process_with(&mut audio, &context));
        heard.extend(
            sent(processor)
                .iter()
                .map(|event| (done as u64 + u64::from(event.offset), event.kind)),
        );
        transport.advance(length, RATE);
        done += length;
        blocks += 1;
    }
    (heard, heap_calls)
}

/// Block lengths, and the longest piece the plugin is activated for: one
/// block in one piece, blocks shorter than the pieces, and blocks the host
/// cuts into many pieces.
const CASES: [(usize, &[usize]); 4] = [
    (256, &[256]),
    (256, &[1, 255, 100, 3]),
    (1_024, &[2_048, 7, 1_000]),
    (64, &[FRAMES]),
];

// --- LV2 ----------------------------------------------------------------------------

/// The `lv2_descriptor` of a plugin of this test.
#[cfg(feature = "lv2")]
macro_rules! lv2_entry {
    ($plugin:ty) => {{
        unsafe extern "C" fn entry(index: u32) -> *const std::ffi::c_void {
            if index == 0 {
                manguebyte_lv2::descriptor!($plugin).as_ptr().cast()
            } else {
                std::ptr::null()
            }
        }
        entry as manguebyte_host::lv2::Lv2Entry
    }};
}

/// The bundle of `P`, its Turtle as the framework's wrapper writes it, with
/// a placeholder for the library, whose plugins come from `entry`.
#[cfg(feature = "lv2")]
fn lv2_library<P: Plugin>(
    entry: manguebyte_host::lv2::Lv2Entry,
) -> manguebyte_host::lv2::Lv2Library {
    let folder = common::scratch_folder(&format!("output-{}", P::INFO.name))
        .join(format!("{}.lv2", P::INFO.name));
    std::fs::create_dir_all(&folder).unwrap();
    let binary = "plugin.so";
    std::fs::write(folder.join(binary), b"").unwrap();
    for (file, text) in manguebyte_lv2::bundle_files::<P>(binary).files() {
        std::fs::write(folder.join(file), text).unwrap();
    }
    // SAFETY: the entry is the framework's own LV2 wrapper around a plugin
    // of this test, which keeps to the LV2 specification and is callable
    // for as long as the test runs.
    unsafe { manguebyte_host::lv2::Lv2Library::from_entry(&folder, entry) }
        .expect("the bundle reads")
}

#[cfg(feature = "lv2")]
mod lv2 {
    use manguebyte_host::lv2::{EVENTS_PER_BLOCK, Lv2Plugin, Lv2Processor};

    use super::*;

    fn create<P: Plugin>(
        entry: manguebyte_host::lv2::Lv2Entry,
        max_block: usize,
    ) -> (Lv2Plugin, Lv2Processor) {
        let library = lv2_library::<P>(entry);
        let uri = library.plugins()[0].uri.clone();
        let mut plugin = library.create_with(&uri, RATE, max_block).unwrap();
        let processor = plugin.activate(RATE, max_block).unwrap();
        (plugin, processor)
    }

    #[test]
    fn the_notes_an_lv2_plugin_sends_are_read_on_their_frames_whatever_the_blocks() {
        let expected = octave_song();
        for (max_block, pattern) in CASES {
            let what = format!("blocks of {pattern:?}, pieces of at most {max_block}");
            let (mut plugin, mut processor) = create::<Octave>(lv2_entry!(Octave), max_block);
            let (heard, heap_calls) = drive(
                &mut processor,
                Lv2Processor::output_events,
                &song(),
                pattern,
                FRAMES,
            );
            assert_eq!(heard, expected, "{what}");
            assert_eq!(heap_calls, 0, "{what}: the heap was used");
            drop(processor);
            plugin.deactivate().unwrap();
            assert_eq!(plugin.lost_events(), 0, "{what}");
            assert_eq!(plugin.undeliverable_events(), 0, "{what}");
        }
    }

    #[test]
    fn what_an_lv2_plugin_sends_beyond_the_room_for_a_block_is_counted() {
        let (mut plugin, mut processor) = create::<Flood>(lv2_entry!(Flood), 2_048);
        let quiet = EventBuffer::new();
        let transport = Transport::STOPPED;
        let context = ProcessContext::new(&quiet, &transport);
        // Four pieces of 2,048 frames, a controller on every frame.
        let mut audio = AudioBuffer::<f32>::new(2, 4 * 2_048);
        let calls = heap_calls_during(|| processor.process_with(&mut audio, &context));
        assert_eq!(calls, 0, "the heap was used");
        let sent = processor.output_events();
        assert_eq!(sent.len(), EVENTS_PER_BLOCK);
        for (frame, event) in sent.iter().enumerate() {
            assert_eq!(event.offset as usize, frame);
            assert_eq!(event.kind, EventKind::Midi(flood_at(frame % 2_048)));
        }
        // The next block starts afresh.
        audio.set_num_frames(100);
        processor.process_with(&mut audio, &context);
        assert_eq!(processor.output_events().len(), 100);
        assert_eq!(processor.output_events()[99].offset, 99);
        drop(processor);
        plugin.deactivate().unwrap();
        assert_eq!(plugin.lost_events(), (4 * 2_048 - EVENTS_PER_BLOCK) as u64);
    }

    #[test]
    fn a_plugin_that_sends_nothing_reads_back_nothing() {
        // Mangue Pluck sends nothing, and its notify port says no MIDI.
        let library = common::load_lv2(&common::lv2_bundle(
            "mangue_pluck",
            mangue_pluck::__manguebyte_lv2_bundle_files,
            "pluck-quiet",
        ));
        let uri = library.plugins()[0].uri.clone();
        let mut plugin = library.create_with(&uri, RATE, 256).unwrap();
        let mut processor = plugin.activate(RATE, 256).unwrap();
        let (heard, _) = drive(
            &mut processor,
            Lv2Processor::output_events,
            &song(),
            &[256],
            2_048,
        );
        assert!(heard.is_empty(), "{heard:?}");
    }
}

// --- Audio Units ----------------------------------------------------------------------

#[cfg(feature = "au")]
mod au {
    use std::sync::{Mutex, MutexGuard, PoisonError};

    use manguebyte_host::au::{AuPlugin, AuProcessor, EVENTS_PER_BLOCK};

    use super::*;

    /// One unit at a time, as the other tests of units are run, so that
    /// the wrapper's CoreFoundation stand-in serves one test.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    fn serial() -> MutexGuard<'static, ()> {
        ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn create<P: Plugin>(max_block: usize) -> (AuPlugin, AuProcessor) {
        let library = common::au_library::<P>();
        let codes = library.components()[0].codes;
        let mut plugin = library.create(codes).unwrap();
        let processor = plugin.activate(RATE, max_block).unwrap();
        (plugin, processor)
    }

    #[test]
    fn the_notes_a_unit_sends_are_read_on_their_frames_whatever_the_blocks() {
        let _one = serial();
        let expected = octave_song();
        for (max_block, pattern) in CASES {
            let what = format!("blocks of {pattern:?}, slices of at most {max_block}");
            let (mut plugin, mut processor) = create::<Octave>(max_block);
            let (heard, heap_calls) = drive(
                &mut processor,
                AuProcessor::output_events,
                &song(),
                pattern,
                FRAMES,
            );
            assert_eq!(heard, expected, "{what}");
            assert_eq!(heap_calls, 0, "{what}: the heap was used");
            drop(processor);
            plugin.deactivate().unwrap();
            assert_eq!(plugin.lost_events(), 0, "{what}");
            assert_eq!(plugin.undeliverable_events(), 0, "{what}");
        }
    }

    #[test]
    fn what_a_unit_sends_beyond_the_room_for_a_block_is_counted() {
        let _one = serial();
        let (mut plugin, mut processor) = create::<Flood>(2_048);
        let quiet = EventBuffer::new();
        let transport = Transport::STOPPED;
        let context = ProcessContext::new(&quiet, &transport);
        let mut audio = AudioBuffer::<f32>::new(2, 4 * 2_048);
        let calls = heap_calls_during(|| processor.process_with(&mut audio, &context));
        assert_eq!(calls, 0, "the heap was used");
        let sent = processor.output_events();
        assert_eq!(sent.len(), EVENTS_PER_BLOCK);
        for (frame, event) in sent.iter().enumerate() {
            assert_eq!(event.offset as usize, frame);
            assert_eq!(event.kind, EventKind::Midi(flood_at(frame % 2_048)));
        }
        audio.set_num_frames(100);
        processor.process_with(&mut audio, &context);
        assert_eq!(processor.output_events().len(), 100);
        drop(processor);
        plugin.deactivate().unwrap();
        assert_eq!(plugin.lost_events(), (4 * 2_048 - EVENTS_PER_BLOCK) as u64);
    }

    #[test]
    fn a_unit_with_no_midi_output_reads_back_nothing() {
        let _one = serial();
        let (_plugin, mut processor) = create::<mangue_pluck::Pluck>(256);
        let (heard, _) = drive(
            &mut processor,
            AuProcessor::output_events,
            &song(),
            &[256],
            2_048,
        );
        assert!(heard.is_empty(), "{heard:?}");
    }
}

// --- Every format, through one interface ---------------------------------------------

#[cfg(all(feature = "clap", feature = "vst3", feature = "lv2", feature = "au"))]
mod any {
    use manguebyte_host::any::{AnyProcessor, Format, Library};
    use manguebyte_host::clap::{ClapLibrary, OutputKind};
    use manguebyte_host::vst3::Vst3Module;

    use super::*;

    static OCTAVE_CLAP: manguebyte_clap::clap_sys::entry::clap_plugin_entry =
        manguebyte_clap::plugin_entry::<Octave>();

    /// Octave in every format, each from the entry point the framework's
    /// wrapper for it makes.
    fn libraries() -> Vec<Library> {
        // SAFETY: every entry point is the framework's own wrapper around
        // a plugin of this test, which keeps to its format's interface and
        // is callable for as long as the test runs.
        unsafe {
            vec![
                Library::from(ClapLibrary::from_entry(&raw const OCTAVE_CLAP, "octave").unwrap()),
                Library::from(
                    Vst3Module::from_factory(manguebyte_vst3::plugin_factory::<Octave>(), "octave")
                        .unwrap(),
                ),
                Library::from(lv2_library::<Octave>(lv2_entry!(Octave))),
                Library::from(common::au_library::<Octave>()),
            ]
        }
    }

    /// An event as every format can carry it: no note ids, which VST 3
    /// makes up and MIDI 1.0 has none of, and values in 7 bits, which is
    /// what MIDI 1.0 has and what VST 3's 32-bit pressure rounds back to.
    fn as_all_carry((frame, kind): (u64, EventKind)) -> (u64, EventKind) {
        let plain = |note: NoteRef| NoteRef::new(note.channel, note.key);
        let steps = |value: f64| (value * 127.0).round() / 127.0;
        let kind = match kind {
            EventKind::NoteOn { note, velocity } => EventKind::NoteOn {
                note: plain(note),
                velocity: steps(f64::from(velocity)) as f32,
            },
            EventKind::NoteOff { note, velocity } => EventKind::NoteOff {
                note: plain(note),
                velocity: steps(f64::from(velocity)) as f32,
            },
            EventKind::NoteExpression {
                note,
                expression,
                value,
            } => EventKind::NoteExpression {
                note: plain(note),
                expression,
                value: steps(value),
            },
            other => other,
        };
        (frame, kind)
    }

    #[test]
    fn one_method_reads_what_a_plugin_sends_in_every_format() {
        let mut heard = Vec::new();
        for library in libraries() {
            let format = library.format();
            let mut plugin = library.create(&library.plugins()[0].id).unwrap();
            let mut processor = plugin.activate_processor(RATE, 256).unwrap();
            assert_eq!(processor.format(), format);
            let (events, heap_calls) = drive(
                &mut processor,
                AnyProcessor::output_events,
                &song(),
                &[1, 255, 100, 3, 512],
                FRAMES,
            );
            assert_eq!(heap_calls, 0, "{format}: the heap was used");
            // What the interface reads is what the format's own processor
            // has: for CLAP, the kinds that are the framework's events.
            let own: Vec<Event> = match format {
                Format::Clap => processor
                    .as_clap()
                    .unwrap()
                    .output_events()
                    .iter()
                    .filter_map(|event| match event.kind {
                        OutputKind::Event(kind) => Some(Event {
                            offset: event.offset,
                            kind,
                        }),
                        _ => None,
                    })
                    .collect(),
                Format::Vst3 => processor.as_vst3().unwrap().output_events().to_vec(),
                Format::Lv2 => processor.as_lv2().unwrap().output_events().to_vec(),
                Format::Au => processor.as_au().unwrap().output_events().to_vec(),
                _ => unreachable!("only four formats are hosted here"),
            };
            assert!(!own.is_empty(), "{format}: the last block sent nothing");
            assert_eq!(own, processor.output_events(), "{format}");
            drop(processor);
            plugin.deactivate().unwrap();
            assert_eq!(plugin.lost_events(), 0, "{format}");
            assert_eq!(plugin.undeliverable_events(), 0, "{format}");
            heard.push((format, events));
        }

        let expected: Vec<_> = octave_song().into_iter().map(as_all_carry).collect();
        for (format, events) in &heard {
            let carried: Vec<_> = events.iter().copied().map(as_all_carry).collect();
            assert_eq!(carried, expected, "{format}");
        }
        // The two formats that carry MIDI 1.0 carry exactly the same, and
        // so does CLAP, which carries the plugin's own values and no ids
        // the plugin did not give.
        let [clap, _, lv2, au] = &heard[..] else {
            panic!("four formats");
        };
        assert_eq!(lv2.1, au.1);
        assert_eq!(clap.1, lv2.1);
        assert_eq!(lv2.1, octave_song());
    }
}
