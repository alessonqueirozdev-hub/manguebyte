//! The sampler in tunings other than 12-tone equal temperament: each key
//! sounds its tuned pitch, unmapped keys stay silent, and changing the
//! tuning never touches the heap.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use common::*;
use manguebyte_core::AudioBuffer;
use manguebyte_dsp::event::NoteRef;
use manguebyte_dsp::tuning::{
    KeyboardMapping, ScalaScale, Scale, Stretch, Temperament, Tuning, TuningSettings,
};
use manguebyte_dsp::{ProcessSpec, Processor, Synth};
use manguebyte_sampler::{Instrument, MemorySource, Sampler, SamplerVoice};

thread_local! {
    static WATCHING: Cell<bool> = const { Cell::new(false) };
    static HEAP_CALLS: Cell<usize> = const { Cell::new(0) };
}

struct CountingAllocator;

impl CountingAllocator {
    fn record() {
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

fn assert_no_heap_use(what: &str, work: impl FnOnce()) {
    HEAP_CALLS.with(|calls| calls.set(0));
    WATCHING.with(|watching| watching.set(true));
    work();
    WATCHING.with(|watching| watching.set(false));
    let calls = HEAP_CALLS.with(Cell::get);
    assert_eq!(calls, 0, "{what} used the heap {calls} time(s)");
}

/// A4 recorded at 44.1 kHz.
fn source() -> MemorySource {
    MemorySource::new().with("a4.wav", sine_wav(440.0, 44_100, 2.0, 0.5))
}

fn werckmeister_at_415() -> Tuning {
    TuningSettings {
        scale: Scale::Temperament {
            temperament: Temperament::werckmeister_iii(),
            key: 0,
        },
        reference_hz: 415.0,
        stretch: Stretch {
            bass_cents_per_octave: 1.0,
            treble_cents_per_octave: 2.0,
            pivot: 69,
        },
        ..TuningSettings::default()
    }
    .build()
    .unwrap()
}

/// The black keys unmapped, the white keys a just major scale on C.
fn white_keys() -> Tuning {
    let scale = ScalaScale::parse("just major\n7\n9/8\n5/4\n4/3\n3/2\n5/3\n15/8\n2/1\n").unwrap();
    let mapping =
        KeyboardMapping::parse("12\n0\n127\n60\n69\n440\n7\n0\nx\n1\nx\n2\n3\nx\n4\nx\n5\nx\n6\n")
            .unwrap();
    TuningSettings {
        scale: Scale::Scala {
            scale,
            mapping: Some(mapping),
        },
        ..TuningSettings::default()
    }
    .build()
    .unwrap()
}

fn measure(sampler: &mut Sampler<f32>, key: u8) -> f64 {
    sampler.reset();
    let output = render_mono(sampler, 19_200, &[(0, on(key, 1.0))]);
    frequency(&output[960..19_200], RATE)
}

#[test]
fn every_key_sounds_its_tuned_pitch() {
    let mut source = source();
    let mut sampler = sampler_with(
        "<region> sample=a4.wav pitch_keycenter=69",
        &mut source,
        4,
        1,
    );
    assert_eq!(sampler.tuning(), &Tuning::default());
    let tuning = werckmeister_at_415();
    sampler.set_tuning(&tuning);
    assert_eq!(sampler.tuning(), &tuning);
    for key in 45..=93_u8 {
        let error = cents(measure(&mut sampler, key), tuning.key_frequency(key));
        assert!(error.abs() < 0.05, "key {key}: {error} cents");
    }

    // The instrument's note offset moves the tuned pitch by semitones, and
    // `tune` by cents.
    let mut sampler = sampler_with(
        "<control> note_offset=12 <region> sample=a4.wav pitch_keycenter=81 tune=-30",
        &mut source,
        4,
        1,
    );
    sampler.set_tuning(&tuning);
    for key in [57_u8, 60, 64, 69] {
        let expected = tuning.key_frequency(key) * 2f64.powf(-30.0 / 1_200.0);
        let error = cents(measure(&mut sampler, key), expected);
        assert!(error.abs() < 0.05, "offset, key {key}: {error} cents");
    }
}

#[test]
fn unmapped_keys_stay_silent() {
    let mut source = source();
    let mut sampler = sampler_with(
        "<region> sample=a4.wav pitch_keycenter=69",
        &mut source,
        4,
        1,
    );
    let white = white_keys();
    sampler.set_tuning(&white);
    let output = render_mono(&mut sampler, 4_800, &[(0, on(61, 1.0)), (100, off(61))]);
    assert!(output.iter().all(|&sample| sample == 0.0));
    assert_eq!(sampler.active_voices(), 0);
    // A white key plays its just pitch: E4 a pure major third over C4,
    // which is a major sixth (5/3) below A4.
    let error = cents(measure(&mut sampler, 64), 440.0 * 3.0 / 5.0 * 5.0 / 4.0);
    assert!(error.abs() < 0.05, "{error} cents");
}

#[test]
fn sampler_voices_take_the_synths_tuning() {
    let instrument = Arc::new(
        Instrument::from_sfz("<region> sample=a4.wav pitch_keycenter=69", &mut source()).unwrap(),
    );
    let tuning = werckmeister_at_415();
    for key in [48_u8, 64, 76] {
        let mut synth = Synth::with_voices(2, || SamplerVoice::new(Arc::clone(&instrument)));
        synth.prepare(&ProcessSpec {
            sample_rate: RATE,
            max_block_size: 256,
            num_channels: 1,
        });
        synth.set_tuning(&tuning);
        synth.note_on(NoteRef::new(0, key), 1.0);
        let mut block = AudioBuffer::<f32>::new(1, 256);
        let mut output = Vec::new();
        for _ in 0..75 {
            synth.process(&mut block);
            output.extend_from_slice(block.channel(0));
        }
        let error = cents(frequency(&output[960..], RATE), tuning.key_frequency(key));
        assert!(error.abs() < 0.05, "key {key}: {error} cents");
    }
}

#[test]
fn the_default_tuning_changes_nothing() {
    // Set explicitly, built from settings, the default renders exactly what
    // a sampler never given a tuning renders.
    let text = "<region> sample=a4.wav pitch_keycenter=69 pitch_keytrack=73 tune=13";
    let events = [
        (0, on(50, 0.7)),
        (300, on(77, 1.0)),
        (900, bend(3_000)),
        (2_000, off(50)),
    ];
    let mut source = source();
    let mut untouched = sampler_with(text, &mut source, 4, 2);
    let mut tuned = sampler_with(text, &mut source, 4, 2);
    tuned.set_tuning(&TuningSettings::default().build().unwrap());
    let a = render(&mut untouched, 2, 9_600, &events, &[64, 256, 17]);
    let b = render(&mut tuned, 2, 9_600, &events, &[64, 256, 17]);
    for (a, b) in a.iter().zip(&b) {
        assert!(a.iter().zip(b).all(|(a, b)| a.to_bits() == b.to_bits()));
    }
}

#[test]
fn changing_the_tuning_never_touches_the_heap() {
    let mut source = source();
    let mut sampler = sampler_with(
        "<region> sample=a4.wav pitch_keycenter=69",
        &mut source,
        8,
        1,
    );
    let tunings = [werckmeister_at_415(), white_keys(), Tuning::default()];
    let mut block = AudioBuffer::<f32>::new(1, 256);
    assert_no_heap_use("the sampler", || {
        for tuning in &tunings {
            sampler.set_tuning(tuning);
            for key in [57, 61, 64] {
                sampler.note_on(NoteRef::new(0, key), 0.8);
            }
            for _ in 0..4 {
                sampler.process(&mut block);
            }
            sampler.note_off(NoteRef::new(0, 61), 0.0);
        }
    });
}
