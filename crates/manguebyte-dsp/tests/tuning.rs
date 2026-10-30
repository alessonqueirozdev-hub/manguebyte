//! Tunings against Caecilia's reference tables, handed to a synth, and
//! used on the audio thread without touching the heap.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use manguebyte_core::{AudioBuffer, spsc};
use manguebyte_dsp::event::NoteRef;
use manguebyte_dsp::instruments::{FmParams, FmVoice, PolySynthVoice, poly};
use manguebyte_dsp::tuning::{
    KeyboardMapping, MtsMessage, ScalaScale, Scale, Stretch, Temperament, Tuning, TuningSettings,
};
use manguebyte_dsp::{ProcessSpec, Processor, Synth, Voice};

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

/// Runs `work` and panics if it used the heap in any way.
fn assert_no_heap_use(what: &str, work: impl FnOnce()) {
    HEAP_CALLS.with(|calls| calls.set(0));
    WATCHING.with(|watching| watching.set(true));
    work();
    WATCHING.with(|watching| watching.set(false));
    let calls = HEAP_CALLS.with(Cell::get);
    assert_eq!(calls, 0, "{what} used the heap {calls} time(s)");
}

#[test]
fn the_guard_itself_catches_an_allocation() {
    let caught = std::panic::catch_unwind(|| {
        assert_no_heap_use("a vector", || drop(std::hint::black_box(vec![1_u8; 8])));
    });
    assert!(caught.is_err());
}

/// Caecilia 1.x's tables: the temperament's name, A4, and 128 frequencies.
fn caecilia_tables() -> Vec<(String, f64, Vec<f64>)> {
    include_str!("data/caecilia_tuning.txt")
        .lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| {
            let mut words = line.split_whitespace();
            let name = words.next().unwrap().to_string();
            let a4: f64 = words.next().unwrap().parse().unwrap();
            let hz: Vec<f64> = words.map(|word| word.parse().unwrap()).collect();
            assert_eq!(hz.len(), 128);
            (name, a4, hz)
        })
        .collect()
}

/// The largest difference between Caecilia 1.x's tables and ours, in
/// cents, as measured when this test was written: the two compute the
/// same definitions in double precision and differ only in rounding.
const CAECILIA_TOLERANCE_CENTS: f64 = 1e-9;

#[test]
fn caecilia_reference_tables_are_matched() {
    let tables = caecilia_tables();
    assert_eq!(tables.len(), 12);
    let mut worst: f64 = 0.0;
    for (name, a4, hz) in tables {
        // Caecilia centres every temperament on C and anchors A4.
        let temperament = match name.as_str() {
            "Equal" => Temperament::equal(),
            "QuarterMeantone" => Temperament::quarter_comma_meantone(),
            "Werckmeister3" => Temperament::werckmeister_iii(),
            "Kirnberger3" => Temperament::kirnberger_iii(),
            "Pythagorean" => Temperament::pythagorean(),
            "Young" => Temperament::young(),
            other => panic!("a temperament this test does not know: {other}"),
        };
        let tuning = TuningSettings {
            scale: Scale::Temperament {
                temperament,
                key: 0,
            },
            reference_hz: a4,
            ..TuningSettings::default()
        }
        .build()
        .unwrap();
        for (key, &expected) in hz.iter().enumerate() {
            let cents = 1_200.0 * (tuning.key_frequency(key as u8) / expected).log2();
            worst = worst.max(cents.abs());
            assert!(
                cents.abs() < CAECILIA_TOLERANCE_CENTS,
                "{name} at {a4} Hz, key {key}: {cents:e} cents off"
            );
        }
    }
    println!("largest difference from Caecilia 1.x: {worst:e} cents");
}

/// A voice that sounds the frequency its tuning gives its key, as a level,
/// and remembers the tuning.
#[derive(Debug, Default)]
struct Pitch {
    tuning: Tuning,
    key: Option<u8>,
    bend: f64,
}

impl Voice for Pitch {
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn note_on(&mut self, note: NoteRef, _: f32) {
        self.key = Some(note.key);
    }
    fn note_off(&mut self, _: f32) {
        self.key = None;
    }
    fn choke(&mut self) {
        self.key = None;
    }
    fn is_active(&self) -> bool {
        self.key.is_some()
    }
    fn render(&mut self, buffer: &mut AudioBuffer) {
        if let Some(key) = self.key {
            let level = self.tuning.frequency(f64::from(key) + self.bend) as f32;
            buffer.channel_mut(0).iter_mut().for_each(|s| *s += level);
        }
    }
    fn pitch_bend(&mut self, semitones: f64) {
        self.bend = semitones;
    }
    fn set_tuning(&mut self, tuning: &Tuning) {
        self.tuning.clone_from(tuning);
    }
}

const SPEC: ProcessSpec = ProcessSpec {
    sample_rate: 48_000.0,
    max_block_size: 64,
    num_channels: 1,
};

/// A tuning that leaves the black keys unmapped.
fn white_keys() -> Tuning {
    let scale = ScalaScale::parse("diatonic\n7\n9/8\n5/4\n4/3\n3/2\n5/3\n15/8\n2/1\n").unwrap();
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

#[test]
fn a_synth_hands_its_tuning_to_every_voice_and_skips_unmapped_keys() {
    let mut synth = Synth::with_voices(4, Pitch::default);
    synth.prepare(&SPEC);
    assert_eq!(synth.tuning(), &Tuning::default());

    let white = white_keys();
    synth.set_tuning(&white);
    assert_eq!(synth.tuning(), &white);
    assert!(synth.voices_mut().all(|voice| voice.tuning == white));

    // C4 sounds; C♯4 is unmapped and does not start.
    synth.note_on(NoteRef::new(0, 60), 1.0);
    synth.note_on(NoteRef::new(0, 61), 1.0);
    assert_eq!(synth.active_voices(), 1);
    let mut block = AudioBuffer::<f32>::new(1, 4);
    synth.process(&mut block);
    let c4 = white.key_frequency(60) as f32;
    assert_eq!(block.channel(0), &[c4; 4]);
    assert!((f64::from(c4) - 440.0 * 3.0 / 5.0).abs() < 1e-3);

    // Back to the default: every key plays again.
    synth.set_tuning(&Tuning::default());
    synth.note_on(NoteRef::new(0, 61), 1.0);
    assert_eq!(synth.active_voices(), 2);
}

/// One second of key 57 (A3) on a synth of `voice`, in `tuning`, at 48 kHz.
fn render<V: Voice<f64>>(voice: V, tuning: &Tuning, key: u8) -> Vec<f64> {
    let mut synth = Synth::new([voice]);
    synth.prepare(&ProcessSpec {
        sample_rate: 48_000.0,
        max_block_size: 512,
        num_channels: 1,
    });
    synth.set_tuning(tuning);
    synth.note_on(NoteRef::new(0, key), 0.8);
    let mut block = AudioBuffer::<f64>::new(1, 512);
    let mut output = Vec::new();
    for _ in 0..48_000 / 512 {
        synth.process(&mut block);
        output.extend_from_slice(block.channel(0));
    }
    output
}

#[test]
fn the_default_tuning_set_explicitly_changes_nothing() {
    // A voice given the default tuning, or 12-tone equal temperament built
    // from settings, renders bit for bit what it renders untouched.
    let built = TuningSettings::default().build().unwrap();
    for key in [33, 57, 69, 96] {
        let fm = || FmVoice::new(FmParams::electric_piano());
        let untouched = {
            let mut synth = Synth::new([fm()]);
            synth.prepare(&ProcessSpec {
                sample_rate: 48_000.0,
                max_block_size: 512,
                num_channels: 1,
            });
            synth.note_on(NoteRef::new(0, key), 0.8);
            let mut block = AudioBuffer::<f64>::new(1, 512);
            let mut output = Vec::new();
            for _ in 0..20 {
                synth.process(&mut block);
                output.extend_from_slice(block.channel(0));
            }
            output
        };
        let tuned = render(fm(), &built, key);
        assert!(
            untouched
                .iter()
                .zip(&tuned)
                .all(|(a, b)| a.to_bits() == b.to_bits())
        );
    }
}

#[test]
fn tunings_change_and_play_without_using_the_heap() {
    let (mut to_audio, mut from_ui) = spsc::channel::<Tuning>(4);
    let stretch = Stretch {
        bass_cents_per_octave: 2.0,
        treble_cents_per_octave: 3.0,
        pivot: 69,
    };
    let tunings = [
        TuningSettings {
            scale: Scale::Temperament {
                temperament: Temperament::kirnberger_iii(),
                key: 2,
            },
            reference_hz: 415.0,
            stretch,
            ..TuningSettings::default()
        }
        .build()
        .unwrap(),
        TuningSettings {
            scale: Scale::equal(31),
            ..TuningSettings::default()
        }
        .build()
        .unwrap(),
        white_keys(),
        Tuning::default(),
    ];
    for tuning in &tunings {
        to_audio.push(tuning.clone()).unwrap();
    }
    let change = [
        0x7F, 0x7F, 0x08, 0x02, 0x00, 2, 60, 60, 0x20, 0, 64, 64, 0, 0,
    ];

    let mut fm = Synth::with_voices(4, || FmVoice::new(FmParams::tubular_bell()));
    let mut lead = Synth::with_voices(2, || PolySynthVoice::new(poly::PRESETS[0].params));
    fm.prepare(&SPEC);
    lead.prepare(&SPEC);
    let mut block = AudioBuffer::<f32>::new(1, 64);
    let mut heard = 0.0_f64;
    assert_no_heap_use("changing tunings while playing", || {
        while let Some(tuning) = from_ui.pop() {
            fm.set_tuning(&tuning);
            lead.set_tuning(&tuning);
            for key in [57, 60, 64, 67] {
                fm.note_on(NoteRef::new(0, key), 0.7);
                lead.note_on(NoteRef::new(0, key), 0.7);
            }
            for _ in 0..8 {
                fm.process(&mut block);
                heard += f64::from(block.peak());
                lead.process(&mut block);
                heard += f64::from(block.peak());
            }
            for note in -10..1_400 {
                heard += tuning.frequency(f64::from(note) * 0.1);
            }
            // A real-time note change read and applied here too.
            let message = MtsMessage::parse(&change).unwrap();
            let changed = message.apply(&tuning);
            heard += changed.key_frequency(60);
            // `tuning` and `changed` are dropped here, on the audio thread.
        }
    });
    assert!(heard.is_finite() && heard > 0.0);
}
