//! Round robin and random layers.

mod common;

use common::*;
use manguebyte_dsp::Processor;
use manguebyte_sampler::{Instrument, MemorySource, Sampler};

const FREQUENCIES: [f64; 4] = [300.0, 500.0, 700.0, 900.0];

fn source() -> MemorySource {
    let mut source = MemorySource::new();
    for (index, &frequency) in FREQUENCIES.iter().enumerate() {
        source.insert(
            &format!("r{index}.wav"),
            sine_wav(frequency, 48_000, 0.2, 0.5),
        );
    }
    source
}

/// Plays `count` notes one after another, each 0.1 s long, and says which
/// region each one played.
fn sequence(sampler: &mut Sampler<f32>, count: usize, key: u8, velocity: f32) -> Vec<usize> {
    let mut played = Vec::new();
    for _ in 0..count {
        let output = render_mono(sampler, 4_800, &[(0, on(key, velocity)), (4_700, off(key))]);
        let levels: Vec<f64> = FREQUENCIES
            .iter()
            .map(|&frequency| amplitude_at(&output[200..4_600], frequency, RATE))
            .collect();
        let loud: Vec<usize> = (0..FREQUENCIES.len())
            .filter(|&i| levels[i] > 0.2)
            .collect();
        assert_eq!(loud.len(), 1, "levels {levels:?}");
        assert!(levels.iter().all(|&level| !(0.01..=0.2).contains(&level)));
        played.push(loud[0]);
        // Let the release end.
        render_mono(sampler, 480, &[]);
    }
    played
}

#[test]
fn round_robin_plays_its_regions_in_turn() {
    let mut source = source();
    let text = "<global> amp_veltrack=0 ampeg_release=0.001 seq_length=3
                <region> sample=r0.wav seq_position=1
                <region> sample=r1.wav seq_position=2
                <region> sample=r2.wav seq_position=3";
    let mut sampler = sampler_with(text, &mut source, 8, 1);
    assert_eq!(sequence(&mut sampler, 7, 60, 1.0), [0, 1, 2, 0, 1, 2, 0]);
    // A reset starts the cycle again.
    sampler.reset();
    assert_eq!(sequence(&mut sampler, 2, 60, 1.0), [0, 1]);
}

#[test]
fn round_robins_of_separate_layers_and_keys_count_separately() {
    let mut source = source();
    // Two velocity layers with a cycle of two each, and a key elsewhere
    // that does not advance them.
    let text = "<global> amp_veltrack=0 ampeg_release=0.001 seq_length=2 hikey=70
                <region> sample=r0.wav hivel=63 seq_position=1
                <region> sample=r1.wav hivel=63 seq_position=2
                <region> sample=r2.wav lovel=64 seq_position=1
                <region> sample=r3.wav lovel=64 seq_position=2";
    let mut sampler = sampler_with(text, &mut source, 8, 1);
    let mut heard = Vec::new();
    for velocity in [0.2, 0.2, 0.9, 0.2, 0.9, 0.9] {
        heard.extend(sequence(&mut sampler, 1, 60, velocity));
        // Key 80 matches no region and leaves the counters alone.
        render_mono(&mut sampler, 480, &[(0, on(80, 1.0)), (100, off(80))]);
    }
    assert_eq!(heard, [0, 1, 2, 0, 3, 2]);
}

#[test]
fn random_layers_are_chosen_by_a_seeded_generator() {
    let text = "<global> amp_veltrack=0 ampeg_release=0.001
                <region> sample=r0.wav hirand=0.25
                <region> sample=r1.wav lorand=0.25 hirand=0.5
                <region> sample=r2.wav lorand=0.5 hirand=0.75
                <region> sample=r3.wav lorand=0.75";
    let play = |seed: Option<u64>| {
        let mut source = source();
        let mut instrument = Instrument::from_sfz(text, &mut source).unwrap();
        if let Some(seed) = seed {
            instrument.set_seed(seed);
        }
        let (mut sampler, _loader) = Sampler::<f32>::with_instrument(instrument, 8);
        sampler.prepare(&manguebyte_dsp::ProcessSpec {
            sample_rate: RATE,
            max_block_size: 512,
            num_channels: 1,
        });
        sequence(&mut sampler, 200, 60, 1.0)
    };
    let first = play(None);
    // The same seed gives the same choices, note for note.
    assert_eq!(first, play(None));
    // Another seed gives other choices.
    let other = play(Some(12_345));
    assert_ne!(first, other);
    assert_eq!(other, play(Some(12_345)));
    // Every range is used about as often as its width says: 50 of 200
    // expected each, and a fair generator lands within 25 to 75.
    for region in 0..4 {
        let count = first.iter().filter(|&&r| r == region).count();
        assert!(
            (25..=75).contains(&count),
            "region {region}: {count} of 200"
        );
    }
}
