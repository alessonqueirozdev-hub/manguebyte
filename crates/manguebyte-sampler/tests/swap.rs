//! Handing a new instrument to a playing sampler.

mod common;

use common::*;
use manguebyte_dsp::{ProcessSpec, Processor};
use manguebyte_sampler::{Instrument, MemorySource, Sampler};

fn instrument(frequency: f64) -> Instrument {
    let mut source = MemorySource::new().with("s.wav", sine_wav(frequency, 48_000, 1.0, 0.5));
    let text = "<region> sample=s.wav pitch_keytrack=0 amp_veltrack=0 ampeg_release=0";
    Instrument::from_sfz(text, &mut source).unwrap()
}

#[test]
fn old_notes_keep_their_instrument_and_it_is_freed_after_them() {
    let (mut sampler, mut loader) = Sampler::<f32>::with_instrument(instrument(300.0), 8);
    sampler.prepare(&ProcessSpec {
        sample_rate: RATE,
        max_block_size: 512,
        num_channels: 1,
    });
    assert_eq!(loader.in_flight(), 1);
    render_mono(&mut sampler, 512, &[(0, on(60, 1.0))]);

    // A new instrument arrives while the note is held; the next note plays
    // it, and the held note goes on with the old one.
    loader.load(instrument(500.0)).unwrap();
    assert_eq!(loader.in_flight(), 2);
    let output = render_mono(&mut sampler, 4_800, &[(0, on(62, 1.0))]);
    assert!(amplitude_at(&output[100..], 300.0, RATE) > 0.45);
    assert!(amplitude_at(&output[100..], 500.0, RATE) > 0.45);
    assert_eq!(loader.collect(), 0, "the old instrument is still playing");

    // When its last note ends, the old instrument goes back at the start of
    // the next block and is freed by the loader.
    render_mono(&mut sampler, 512, &[(0, off(60))]);
    render_mono(&mut sampler, 512, &[]);
    assert_eq!(loader.collect(), 1);
    assert_eq!(loader.in_flight(), 1);
}

#[test]
fn a_third_instrument_waits_for_the_oldest_to_fade() {
    let (mut sampler, mut loader) = Sampler::<f32>::with_instrument(instrument(300.0), 8);
    sampler.prepare(&ProcessSpec {
        sample_rate: RATE,
        max_block_size: 512,
        num_channels: 1,
    });
    render_mono(&mut sampler, 256, &[(0, on(60, 1.0))]);
    loader.load(instrument(500.0)).unwrap();
    render_mono(&mut sampler, 256, &[(0, on(62, 1.0))]);
    // Both notes still held; a third instrument comes. The oldest
    // instrument's note fades over 6 ms (288 frames), and at the start of
    // the first block after that (frame 512 in blocks of 256) the third
    // takes its place.
    loader.load(instrument(700.0)).unwrap();
    let output = render_mono(&mut sampler, 4_800, &[(1_000, on(64, 1.0))]);
    let late = &output[1_100..];
    assert!(amplitude_at(late, 300.0, RATE) < 1e-3);
    assert!(amplitude_at(late, 500.0, RATE) > 0.45);
    assert!(amplitude_at(late, 700.0, RATE) > 0.45);
    assert_eq!(loader.collect(), 1);
    assert_eq!(loader.in_flight(), 2);
}

#[test]
fn the_loader_refuses_more_than_it_can_track() {
    let (mut sampler, mut loader) = Sampler::<f32>::with_capacity(4, 1);
    sampler.prepare(&ProcessSpec {
        sample_rate: RATE,
        max_block_size: 64,
        num_channels: 1,
    });
    // One may wait in the queue, and three more are the sampler's own
    // places: the current, the previous and the one waiting.
    let mut sent = 0;
    while loader.load(instrument(300.0)).is_ok() {
        sent += 1;
        assert!(sent < 100);
    }
    assert_eq!(sent, 4);
    // The audio thread takes the newest and returns the ones it skipped.
    sampler.process(&mut manguebyte_core::AudioBuffer::new(1, 64));
    assert_eq!(loader.collect(), 3);
    assert!(loader.load(instrument(500.0)).is_ok());
    assert!(sampler.instrument().is_some());
}
