//! The output is the same, bit for bit, however the timeline is cut into
//! blocks, and the same again after a reset.

mod common;

use common::*;
use manguebyte_dsp::Processor;
use manguebyte_dsp::event::{EventKind, Expression, NoteRef};
use manguebyte_sampler::{Interpolation, MemorySource, Sampler};

fn source() -> MemorySource {
    let stereo = wav_bytes(
        &[
            sine(330.0, 44_100.0, 22_050, 0.4),
            sine(331.0, 44_100.0, 22_050, 0.4),
        ],
        44_100,
    );
    MemorySource::new()
        .with("a.wav", sine_wav(440.0, 44_100, 0.6, 0.5))
        .with("b.wav", stereo)
        .with("c.wav", sine_wav(1_000.0, 96_000, 0.2, 0.3))
}

/// Everything at once: layers, round robin, random layers, release
/// triggers, loops with a crossfade, envelopes, choke groups and a group
/// polyphony limit.
const TEXT: &str = "
<global> ampeg_attack=0.003 ampeg_decay=0.05 ampeg_sustain=60 ampeg_release=0.08
<group> lokey=40 hikey=70 seq_length=2
<region> sample=a.wav seq_position=1 pitch_keycenter=69 loop_mode=loop_sustain loop_start=2000 loop_end=9999 loop_crossfade=0.01
<region> sample=b.wav seq_position=2 pan=-40 tune=13
<group> lokey=71 hikey=90 group=5 off_by=6 polyphony=2
<region> sample=c.wav hirand=0.5 offset=100
<region> sample=a.wav lorand=0.5 transpose=7 loop_mode=one_shot
<group> key=36 group=6
<region> sample=c.wav loop_mode=loop_continuous loop_start=100 loop_end=5000
<group> lokey=40 hikey=90
<region> sample=c.wav trigger=release volume=-12 rt_decay=10
";

fn events() -> Vec<(usize, EventKind)> {
    let mut random = Random::new(7);
    let mut events = Vec::new();
    let mut held: Vec<u8> = Vec::new();
    for step in 0..120 {
        let at = step * 397 + random.below(300);
        match random.below(10) {
            0..=3 => {
                let key = 40 + random.below(51) as u8;
                events.push((at, on(key, 0.2 + 0.8 * random.unit() as f32)));
                held.push(key);
            }
            4..=5 if !held.is_empty() => {
                let key = held.remove(random.below(held.len()));
                events.push((at, off(key)));
            }
            6 => events.push((at, on(36, 1.0))),
            7 => events.push((
                at,
                controller(64, if random.below(2) == 0 { 0 } else { 127 }),
            )),
            8 => events.push((at, bend(random.below(16_384) as i16 - 8_192))),
            _ => {
                let note = NoteRef::new(0, 40 + random.below(51) as u8);
                events.push((
                    at,
                    EventKind::NoteExpression {
                        note,
                        expression: Expression::Tuning,
                        value: random.unit() - 0.5,
                    },
                ));
            }
        }
    }
    events.sort_by_key(|&(at, _)| at);
    events
}

const FRAMES: usize = 52_000;

fn play(interpolation: Interpolation, blocks: &[usize]) -> Vec<Vec<f32>> {
    let mut source = source();
    let mut sampler = sampler_with(TEXT, &mut source, 12, 2);
    sampler.set_interpolation(interpolation);
    render(&mut sampler, 2, FRAMES, &events(), blocks)
}

#[test]
fn any_block_pattern_gives_the_same_bits() {
    for interpolation in [Interpolation::Sinc, Interpolation::Cubic] {
        let reference = play(interpolation, &[FRAMES]);
        assert!(reference[0].iter().any(|&s| s.abs() > 0.1));
        assert!(reference[1].iter().any(|&s| s.abs() > 0.1));
        for pattern in [&[1][..], &[7], &[64], &[256, 1, 13, 500], &[3, 1000, 2]] {
            let output = play(interpolation, pattern);
            for channel in 0..2 {
                let same = output[channel]
                    .iter()
                    .zip(&reference[channel])
                    .all(|(a, b)| a.to_bits() == b.to_bits());
                assert!(same, "{interpolation:?} {pattern:?} channel {channel}");
            }
        }
    }
}

#[test]
fn a_reset_plays_the_same_notes_the_same_way() {
    let mut source = source();
    let mut sampler: Sampler<f32> = sampler_with(TEXT, &mut source, 12, 2);
    let first = render(&mut sampler, 2, FRAMES, &events(), &[256]);
    sampler.reset();
    let second = render(&mut sampler, 2, FRAMES, &events(), &[100]);
    assert_eq!(first, second);
}

#[test]
fn double_precision_output_matches_single_within_rounding() {
    use manguebyte_core::AudioBuffer;
    use manguebyte_dsp::event::{Event, EventBuffer};
    use manguebyte_dsp::{ProcessContext, ProcessSpec, Transport};
    use manguebyte_sampler::Instrument;

    let mut source = source();
    let instrument = Instrument::from_sfz(TEXT, &mut source).unwrap();
    let (mut sampler, _loader) = Sampler::<f64>::with_instrument(instrument, 12);
    sampler.prepare(&ProcessSpec {
        sample_rate: RATE,
        max_block_size: FRAMES,
        num_channels: 2,
    });
    let mut list = EventBuffer::with_capacity(200);
    for (at, kind) in events() {
        list.push(Event {
            offset: at as u32,
            kind,
        })
        .unwrap();
    }
    let mut buffer = AudioBuffer::<f64>::new(2, FRAMES);
    sampler.process_with(
        &mut buffer,
        &ProcessContext::new(&list, &Transport::STOPPED),
    );
    let single = play(Interpolation::Sinc, &[FRAMES]);
    for (channel, single) in single.iter().enumerate() {
        for (&a, &b) in buffer.channel(channel).iter().zip(single) {
            assert!((a - f64::from(b)).abs() < 1e-5);
        }
    }
}
