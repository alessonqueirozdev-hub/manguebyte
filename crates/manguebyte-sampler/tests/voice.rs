//! The sampler as a voice of `Synth`.

mod common;

use std::sync::Arc;

use common::*;
use manguebyte_core::AudioBuffer;
use manguebyte_dsp::event::{Event, EventBuffer, EventKind, Expression, NoteRef};
use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor, Synth, Transport, Voice};
use manguebyte_sampler::{Instrument, MemorySource, SamplerVoice};

fn instrument(text: &str) -> Arc<Instrument> {
    let mut source = MemorySource::new()
        .with("a4.wav", sine_wav(440.0, 44_100, 1.0, 0.5))
        .with("r0.wav", sine_wav(300.0, 48_000, 0.5, 0.5))
        .with("r1.wav", sine_wav(500.0, 48_000, 0.5, 0.5))
        .with("rel.wav", sine_wav(1_000.0, 48_000, 0.1, 0.5));
    Arc::new(Instrument::from_sfz(text, &mut source).unwrap())
}

fn synth(instrument: &Arc<Instrument>, voices: usize) -> Synth<SamplerVoice> {
    let mut synth = Synth::with_voices(voices, || SamplerVoice::new(Arc::clone(instrument)));
    synth.prepare(&ProcessSpec {
        sample_rate: RATE,
        max_block_size: 512,
        num_channels: 1,
    });
    synth
}

fn run(synth: &mut Synth<SamplerVoice>, frames: usize, events: &[(usize, EventKind)]) -> Vec<f32> {
    let mut out = Vec::new();
    let mut buffer = AudioBuffer::<f32>::new(1, 256);
    let mut list = EventBuffer::with_capacity(16);
    let mut position = 0;
    while position < frames {
        let size = 256.min(frames - position);
        list.clear();
        for &(at, kind) in events {
            if (position..position + size).contains(&at) {
                list.push(Event {
                    offset: (at - position) as u32,
                    kind,
                })
                .unwrap();
            }
        }
        buffer.set_num_frames(size);
        synth.process_with(
            &mut buffer,
            &ProcessContext::new(&list, &Transport::STOPPED),
        );
        out.extend_from_slice(buffer.channel(0));
        position += size;
    }
    out
}

#[test]
fn pitch_layers_and_the_wheel() {
    let instrument = instrument("<region> sample=a4.wav pitch_keycenter=69");
    let mut synth = synth(&instrument, 4);
    synth.set_pitch_bend_range(12.0);
    for key in [57u8, 69, 81] {
        synth.reset();
        let output = run(&mut synth, 9_600, &[(0, on(key, 1.0))]);
        let expected = 440.0 * 2f64.powf((f64::from(key) - 69.0) / 12.0);
        assert!(cents(frequency(&output[480..], RATE), expected).abs() < 0.05);
    }
    // The synth's wheel range applies: half up is six semitones.
    synth.reset();
    let output = run(&mut synth, 9_600, &[(0, on(69, 1.0)), (0, bend(4_096))]);
    let expected = 440.0 * 2f64.powf(0.5);
    assert!(cents(frequency(&output[480..], RATE), expected).abs() < 0.05);
    // Per-note tuning reaches the note. A reset leaves the synth's wheel
    // where it was, so it is centred first.
    synth.reset();
    run(&mut synth, 1, &[(0, bend(0))]);
    let tuning = EventKind::NoteExpression {
        note: NoteRef::new(0, 69),
        expression: Expression::Tuning,
        value: -12.0,
    };
    let output = run(&mut synth, 9_600, &[(0, on(69, 1.0)), (0, tuning)]);
    assert!(cents(frequency(&output[480..], RATE), 220.0).abs() < 0.05);
}

#[test]
fn round_robin_is_shared_by_the_voices_and_release_regions_follow_note_off() {
    let instrument = instrument(
        "<global> amp_veltrack=0 ampeg_release=0 pitch_keytrack=0
         <region> sample=r0.wav seq_length=2 seq_position=1
         <region> sample=r1.wav seq_length=2 seq_position=2
         <region> sample=rel.wav trigger=release",
    );
    let mut synth = synth(&instrument, 4);
    // Two notes on two different voices alternate between the regions.
    let output = run(&mut synth, 9_600, &[(0, on(60, 1.0)), (4_800, on(64, 1.0))]);
    let first = &output[200..4_700];
    let both = &output[5_000..9_500];
    assert!(amplitude_at(first, 300.0, RATE) > 0.45);
    assert!(amplitude_at(first, 500.0, RATE) < 1e-3);
    assert!(amplitude_at(both, 300.0, RATE) > 0.45);
    assert!(amplitude_at(both, 500.0, RATE) > 0.45);
    // Releasing 64 stops its region and starts the release region there.
    let output = run(&mut synth, 4_800, &[(0, off(64))]);
    let released = &output[100..4_700];
    assert!(amplitude_at(released, 500.0, RATE) < 1e-3);
    assert!(amplitude_at(released, 1_000.0, RATE) > 0.45);
    assert!(amplitude_at(released, 300.0, RATE) > 0.45);
}

#[test]
fn choke_returns_the_voice_to_new() {
    let instrument = instrument("<region> sample=a4.wav off_by=1 <region> sample=r0.wav group=1");
    let mut voice = SamplerVoice::new(Arc::clone(&instrument));
    Voice::<f32>::prepare(
        &mut voice,
        &ProcessSpec {
            sample_rate: RATE,
            max_block_size: 64,
            num_channels: 1,
        },
    );
    Voice::<f32>::note_on(&mut voice, NoteRef::new(0, 60), 1.0);
    // The second region's group does not choke the first: they started
    // together.
    assert_eq!(voice.active_layers(), 2);
    assert!(Voice::<f32>::is_active(&voice));
    let mut buffer = AudioBuffer::<f32>::new(1, 64);
    Voice::<f32>::render(&mut voice, &mut buffer);
    assert!(buffer.peak() > 0.0);
    Voice::<f32>::choke(&mut voice);
    assert!(!Voice::<f32>::is_active(&voice));
    assert!(Arc::ptr_eq(voice.instrument(), &instrument));
}

#[test]
fn a_note_with_only_release_regions_still_plays_them() {
    let instrument = instrument("<region> sample=rel.wav trigger=release amp_veltrack=0");
    let mut synth = synth(&instrument, 2);
    let output = run(&mut synth, 4_800, &[(0, on(60, 1.0)), (1_000, off(60))]);
    assert!(output[..1_000].iter().all(|&s| s == 0.0));
    assert!(amplitude_at(&output[1_100..4_700], 1_000.0, RATE) > 0.45);
}
