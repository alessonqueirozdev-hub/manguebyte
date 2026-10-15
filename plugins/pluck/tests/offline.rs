//! Offline renders of Mangue Pluck are bit-exact (TECH_DEBT 42).
//!
//! In REAPER, two offline renders of one project differed by one unit in
//! the last place once many voices overlapped. The material here is that
//! project's: every parameter at an extreme (cutoff 12 kHz, resonance at
//! its minimum, decay and release 5 s, reverb 1, level +6 dB) and 37 notes
//! at full velocity in 25 seconds, most of them overlapping, with more
//! notes sounding than there are voices.
//!
//! The renders are compared bit for bit, through the plugin harness and
//! through the CLAP entry point in this process, driven as a host drives
//! it:
//!
//! - with many block patterns, as a host cuts the timeline differently on
//!   every render, with and without automation of every parameter;
//! - after a reset, and after the plugin is deactivated and activated
//!   again, which is what a host does between two renders.
//!
//! The cause was the synth's voice ages, which a reset kept: the next
//! render's notes landed on other voices, which were added up in another
//! order (see `Synth::reset`).

use std::ffi::{c_char, c_void};
use std::ptr;

use mangue_pluck::{Pluck, PluckParams};
use manguebyte::clap::clap_sys::audio_buffer::clap_audio_buffer;
use manguebyte::clap::clap_sys::entry::clap_plugin_entry;
use manguebyte::clap::clap_sys::events::{
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON, CLAP_EVENT_PARAM_VALUE,
    clap_event_header, clap_event_note, clap_event_param_value, clap_input_events,
    clap_output_events,
};
use manguebyte::clap::clap_sys::factory::plugin_factory::{
    CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory,
};
use manguebyte::clap::clap_sys::host::clap_host;
use manguebyte::clap::clap_sys::plugin::clap_plugin;
use manguebyte::clap::clap_sys::process::{CLAP_PROCESS_ERROR, clap_process};
use manguebyte::clap::clap_sys::version::CLAP_VERSION;
use manguebyte::prelude::*;

const RATE: f64 = 48_000.0;

/// Seconds of material; the render runs on for the release and the reverb.
const SONG_SECONDS: f64 = 25.0;

/// One note of the material: start and length in frames, and key.
#[derive(Debug, Clone, Copy)]
struct Note {
    start: u64,
    length: u64,
    key: u8,
}

/// A small deterministic generator, so the material is the same on every
/// run and every machine.
struct Numbers(u64);

impl Numbers {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// 37 notes at full velocity in 25 seconds: chords and runs, keys from 36
/// to 96, lengths from half a second to six seconds, so that with a
/// five-second release more than sixteen often sound at once and voices
/// are stolen. The first starts on the first frame.
fn material(rate: f64) -> Vec<Note> {
    let mut numbers = Numbers(0x9E37_79B9_7F4A_7C15);
    let mut notes: Vec<Note> = Vec::new();
    let span = (SONG_SECONDS * rate) as u64;
    for index in 0..37_u64 {
        // Roughly even starts, some of them on the same frame as the note
        // before, as in a chord.
        let start = match index {
            0 => 0,
            _ if index % 5 == 3 => notes[notes.len() - 1].start,
            _ => index * span / 38 + numbers.below(rate as u64 / 4),
        };
        // Every note ends within the song, so that nothing is left
        // scheduled after a render.
        let length = (rate as u64 / 2 + numbers.below(rate as u64 * 11 / 2)).min(span - start);
        let key = 36 + numbers.below(61) as u8;
        notes.push(Note { start, length, key });
    }
    notes
}

/// Frames to render: the material, then the release and reverb tail.
fn frames(rate: f64) -> usize {
    ((SONG_SECONDS + 4.0) * rate) as usize
}

/// Every parameter at the extreme the REAPER project used.
fn extremes(params: &PluckParams) {
    params.cutoff.set(12_000.0);
    params.resonance.set(0.5);
    params.decay.set(5.0);
    params.release.set(5.0);
    params.reverb.set(1.0);
    params.level.set(6.0);
}

fn schedule(harness: &mut Harness<Pluck>, from: u64, notes: &[Note]) {
    for note in notes {
        harness.note(from + note.start, note.length, note.key, 1.0);
    }
}

/// Automation of every parameter at frames that line up with nothing:
/// sweeps, jumps, values sent again unchanged, and values an `f32` cannot
/// hold exactly.
fn automate_everything(harness: &mut Harness<Pluck>) {
    let lanes: [(&str, [f64; 5]); 6] = [
        ("cutoff", [12_000.0, 300.0, 4_321.5, 12_000.0, 800.0]),
        ("resonance", [0.5, 6.5, 0.5, 8.0, 1.3]),
        ("decay", [5.0, 0.3, 2.2, 0.01, 5.0]),
        ("release", [5.0, 0.4, 5.0, 1.7, 0.05]),
        ("reverb", [1.0, 0.0, 0.35, 1.0, 0.6]),
        ("level", [6.0, -20.0, 0.0, 6.0, -3.3]),
    ];
    let mut numbers = Numbers(0x2545_F491_4F6C_DD1D);
    let span = (SONG_SECONDS * RATE) as u64;
    for round in 0..12 {
        for (lane, (name, values)) in lanes.iter().enumerate() {
            let frame = numbers.below(span);
            harness.automate(frame, name, values[(round + lane) % values.len()]);
        }
    }
}

/// Where two renders part, if they do: the first frame and channel, and
/// the largest difference against the loudest sample, in decibels, to tell
/// a rounding difference from a different sound.
fn first_difference(a: &AudioBuffer<f32>, b: &AudioBuffer<f32>) -> Option<String> {
    assert_eq!(a.num_frames(), b.num_frames());
    assert_eq!(a.num_channels(), b.num_channels());
    let mut first: Option<(usize, usize)> = None;
    let mut largest = 0.0_f32;
    for channel in 0..a.num_channels() {
        for (frame, (x, y)) in a
            .channel(channel)
            .iter()
            .zip(b.channel(channel))
            .enumerate()
        {
            if x.to_bits() != y.to_bits() {
                if first.is_none_or(|(at, _)| frame < at) {
                    first = Some((frame, channel));
                }
                largest = largest.max((x - y).abs());
            }
        }
    }
    first.map(|(frame, channel)| {
        let below = 20.0 * (largest / a.peak().max(b.peak())).log10();
        format!("first at frame {frame}, channel {channel}; at most {largest:e}, {below:.0} dB")
    })
}

fn render_harness(max: usize, pattern: &[usize], automated: bool) -> AudioBuffer<f32> {
    let mut harness = Harness::<Pluck>::new(RATE, max).with_block_pattern(pattern);
    extremes(harness.params());
    schedule(&mut harness, 0, &material(RATE));
    if automated {
        automate_everything(&mut harness);
    }
    harness.render(frames(RATE))
}

const PATTERNS: [(usize, &[usize]); 4] = [
    (64, &[64]),
    (1_024, &[1, 999, 13, 1_024]),
    (512, &[512, 3, 77, 128, 511]),
    (4_096, &[4_096, 17, 2_048]),
];

#[test]
fn dense_extreme_material_is_the_same_whatever_the_block_pattern() {
    let reference = render_harness(512, &[512], false);
    assert!(reference.peak() > 0.1);
    for (max, pattern) in PATTERNS {
        let output = render_harness(max, pattern, false);
        assert_eq!(
            first_difference(&output, &reference),
            None,
            "with blocks of {pattern:?}"
        );
    }
}

#[test]
fn automation_of_every_parameter_lands_on_its_frame_whatever_the_block_pattern() {
    let reference = render_harness(512, &[512], true);
    assert!(reference.peak() > 0.1);
    // The automation makes a difference, so the comparison means something.
    assert!(first_difference(&reference, &render_harness(512, &[512], false)).is_some());
    for (max, pattern) in PATTERNS {
        let output = render_harness(max, pattern, true);
        assert_eq!(
            first_difference(&output, &reference),
            None,
            "with blocks of {pattern:?}"
        );
    }
}

#[test]
fn a_render_after_a_reset_is_the_same_as_the_first() {
    let notes = material(RATE);
    let mut harness = Harness::<Pluck>::new(RATE, 512).with_block_pattern(&[512, 333, 64]);
    extremes(harness.params());
    schedule(&mut harness, 0, &notes);
    let first = harness.render(frames(RATE));
    assert!(first.peak() > 0.1);

    // A whole render first, and one cut short with notes still sounding:
    // the notes that end before the cut, so that nothing is left scheduled.
    for stop in [frames(RATE), 300_000] {
        let mut harness = Harness::<Pluck>::new(RATE, 512).with_block_pattern(&[441, 7]);
        extremes(harness.params());
        let before: Vec<Note> = notes
            .iter()
            .copied()
            .filter(|note| ((note.start + note.length) as usize) < stop)
            .collect();
        schedule(&mut harness, 0, &before);
        harness.render(stop);
        harness.reset();
        let from = harness.position();
        schedule(&mut harness, from, &notes);
        let again = harness.render(frames(RATE));
        assert_eq!(
            first_difference(&again, &first),
            None,
            "after a reset at frame {stop}"
        );
    }
}

// --- A small CLAP host -------------------------------------------------

unsafe extern "C" fn host_get_extension(_: *const clap_host, _: *const c_char) -> *const c_void {
    ptr::null()
}
unsafe extern "C" fn host_request(_: *const clap_host) {}

/// One event the host sends, stored as its CLAP struct.
enum Sent {
    Note(clap_event_note),
    Value(clap_event_param_value),
}

impl Sent {
    fn header(&self) -> *const clap_event_header {
        match self {
            Self::Note(event) => ptr::from_ref(event).cast(),
            Self::Value(event) => ptr::from_ref(event).cast(),
        }
    }
}

fn header<T>(time: u32, type_: u16) -> clap_event_header {
    clap_event_header {
        size: size_of::<T>() as u32,
        time,
        space_id: CLAP_CORE_EVENT_SPACE_ID,
        type_,
        flags: 0,
    }
}

unsafe extern "C" fn events_size(list: *const clap_input_events) -> u32 {
    // SAFETY: `ctx` points at the `Vec<Sent>` of the call in progress.
    unsafe { (*(*list).ctx.cast::<Vec<Sent>>()).len() as u32 }
}

unsafe extern "C" fn events_get(
    list: *const clap_input_events,
    index: u32,
) -> *const clap_event_header {
    // SAFETY: as above; the plugin asks only for indices below the size.
    unsafe { (&(*(*list).ctx.cast::<Vec<Sent>>()))[index as usize].header() }
}

unsafe extern "C" fn events_push(
    _: *const clap_output_events,
    _: *const clap_event_header,
) -> bool {
    true
}

/// What the host schedules, in frames from the start of a render.
#[derive(Debug, Clone, Copy)]
enum Scheduled {
    NoteOn(u8),
    NoteOff(u8),
    /// A parameter's position in 0..=1, as CLAP carries it.
    Value(ParamId, f64),
}

struct ClapHost {
    entry: &'static clap_plugin_entry,
    plugin: *const clap_plugin,
    host: *mut clap_host,
}

impl ClapHost {
    fn new() -> Self {
        let entry = &mangue_pluck::clap_entry;
        let host = Box::into_raw(Box::new(clap_host {
            clap_version: CLAP_VERSION,
            host_data: ptr::null_mut(),
            name: c"offline test host".as_ptr(),
            vendor: c"Manguebyte".as_ptr(),
            url: c"".as_ptr(),
            version: c"1".as_ptr(),
            get_extension: Some(host_get_extension),
            request_restart: Some(host_request),
            request_process: Some(host_request),
            request_callback: Some(host_request),
        }));
        // SAFETY: the calls follow CLAP's order: init the entry, get the
        // factory, create the plugin with a host that outlives it, init it.
        unsafe {
            assert!(entry.init.unwrap()(c"/plugins/pluck.clap".as_ptr()));
            let factory = entry.get_factory.unwrap()(CLAP_PLUGIN_FACTORY_ID.as_ptr())
                .cast::<clap_plugin_factory>();
            assert!(!factory.is_null());
            let plugin =
                (*factory).create_plugin.unwrap()(factory, host, c"org.manguebyte.pluck".as_ptr());
            assert!(!plugin.is_null());
            assert!((*plugin).init.unwrap()(plugin));
            Self {
                entry,
                plugin,
                host,
            }
        }
    }

    fn activate(&self, rate: f64, max: u32) {
        // SAFETY: the plugin is initialised and not active.
        unsafe {
            assert!((*self.plugin).activate.unwrap()(self.plugin, rate, 1, max));
            assert!((*self.plugin).start_processing.unwrap()(self.plugin));
        }
    }

    fn deactivate(&self) {
        // SAFETY: the plugin is active and processing.
        unsafe {
            (*self.plugin).stop_processing.unwrap()(self.plugin);
            (*self.plugin).deactivate.unwrap()(self.plugin);
        }
    }

    fn reset(&self) {
        // SAFETY: the plugin is active; the test is its audio thread.
        unsafe { (*self.plugin).reset.unwrap()(self.plugin) }
    }

    /// Renders `frames` frames in blocks of the lengths in `pattern`,
    /// delivering `scheduled` on its frames.
    fn render(
        &self,
        frames: usize,
        pattern: &[usize],
        scheduled: &[(u64, Scheduled)],
    ) -> AudioBuffer<f32> {
        let mut output = AudioBuffer::<f32>::new(2, frames);
        let mut position = 0;
        let mut next_event = 0;
        let mut lengths = pattern.iter().cycle();
        let mut left = Vec::new();
        let mut right = Vec::new();
        while position < frames {
            let length = (*lengths.next().unwrap()).min(frames - position);
            let end = (position + length) as u64;
            let mut events = Vec::new();
            while next_event < scheduled.len() && scheduled[next_event].0 < end {
                let (frame, what) = scheduled[next_event];
                let time = (frame - position as u64) as u32;
                events.push(match what {
                    Scheduled::NoteOn(key) | Scheduled::NoteOff(key) => {
                        Sent::Note(clap_event_note {
                            header: header::<clap_event_note>(
                                time,
                                if matches!(what, Scheduled::NoteOn(_)) {
                                    CLAP_EVENT_NOTE_ON
                                } else {
                                    CLAP_EVENT_NOTE_OFF
                                },
                            ),
                            note_id: -1,
                            port_index: 0,
                            channel: 0,
                            key: i16::from(key),
                            velocity: 1.0,
                        })
                    }
                    Scheduled::Value(param, value) => Sent::Value(clap_event_param_value {
                        header: header::<clap_event_param_value>(time, CLAP_EVENT_PARAM_VALUE),
                        param_id: param.0,
                        cookie: ptr::null_mut(),
                        note_id: -1,
                        port_index: -1,
                        channel: -1,
                        key: -1,
                        value,
                    }),
                });
                next_event += 1;
            }
            left.clear();
            left.resize(length, 0.0_f32);
            right.clear();
            right.resize(length, 0.0_f32);
            let mut channels = [left.as_mut_ptr(), right.as_mut_ptr()];
            let mut outputs = [clap_audio_buffer {
                data32: channels.as_mut_ptr(),
                data64: ptr::null_mut(),
                channel_count: 2,
                latency: 0,
                constant_mask: 0,
            }];
            let input_events = clap_input_events {
                ctx: (&raw mut events).cast(),
                size: Some(events_size),
                get: Some(events_get),
            };
            let output_events = clap_output_events {
                ctx: ptr::null_mut(),
                try_push: Some(events_push),
            };
            let process = clap_process {
                steady_time: position as i64,
                frames_count: length as u32,
                transport: ptr::null(),
                audio_inputs: ptr::null(),
                audio_outputs: outputs.as_mut_ptr(),
                audio_inputs_count: 0,
                audio_outputs_count: 1,
                in_events: &raw const input_events,
                out_events: &raw const output_events,
            };
            // SAFETY: every pointer in `process` is valid for the call.
            let status =
                unsafe { (*self.plugin).process.unwrap()(self.plugin, &raw const process) };
            assert_ne!(status, CLAP_PROCESS_ERROR);
            output.channel_mut(0)[position..position + length].copy_from_slice(&left);
            output.channel_mut(1)[position..position + length].copy_from_slice(&right);
            position += length;
        }
        output
    }
}

impl Drop for ClapHost {
    fn drop(&mut self) {
        // SAFETY: the plugin is deactivated, and nothing uses the host
        // after the plugin is destroyed.
        unsafe {
            (*self.plugin).destroy.unwrap()(self.plugin);
            self.entry.deinit.unwrap()();
            drop(Box::from_raw(self.host));
        }
    }
}

/// The REAPER project as a CLAP host sends it: every parameter's position
/// at the start, then the notes.
fn clap_material(rate: f64) -> Vec<(u64, Scheduled)> {
    let mut scheduled = vec![
        (0, Scheduled::Value(ParamId::from_name("cutoff"), 1.0)),
        (0, Scheduled::Value(ParamId::from_name("resonance"), 0.0)),
        (0, Scheduled::Value(ParamId::from_name("decay"), 1.0)),
        (0, Scheduled::Value(ParamId::from_name("release"), 1.0)),
        (0, Scheduled::Value(ParamId::from_name("reverb"), 1.0)),
        (0, Scheduled::Value(ParamId::from_name("level"), 1.0)),
    ];
    for note in material(rate) {
        scheduled.push((note.start, Scheduled::NoteOn(note.key)));
        scheduled.push((note.start + note.length, Scheduled::NoteOff(note.key)));
    }
    // Stable, so a note off and a note on of one frame keep their order.
    scheduled.sort_by_key(|&(frame, _)| frame);
    scheduled
}

#[test]
fn renders_through_clap_are_the_same_after_deactivate_and_reset() {
    let scheduled = clap_material(RATE);
    let host = ClapHost::new();
    host.activate(RATE, 1_024);
    let first = host.render(frames(RATE), &[1_024], &scheduled);
    assert!(first.peak() > 0.1);

    for (index, pattern) in [&[512, 3, 77][..], &[1, 1_023, 64][..]].iter().enumerate() {
        if index == 0 {
            // Between two renders a host deactivates and activates again.
            host.deactivate();
            host.activate(RATE, 1_024);
        } else {
            host.reset();
        }
        let again = host.render(frames(RATE), pattern, &scheduled);
        assert_eq!(
            first_difference(&again, &first),
            None,
            "render {index} with blocks of {pattern:?}"
        );
    }
    host.deactivate();
}
