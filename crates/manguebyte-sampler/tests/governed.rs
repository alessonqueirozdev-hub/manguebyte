//! The sampler spending a processor budget (ADR 0018): what it demotes to
//! the cubic interpolator, cuts, suspends and steals, and that without a
//! budget, with room enough in it or offline, it renders what it would
//! have, bit for bit.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use manguebyte_core::AudioBuffer;
use manguebyte_core::governor::{Allowance, Clock, Governor, GovernorConfig};
use manguebyte_dsp::event::{Event, EventBuffer, EventKind};
use manguebyte_dsp::{
    BUDGET_FADE_SECONDS, ProcessContext, ProcessSpec, Processor, Synth, Transport,
};
use manguebyte_sampler::{Instrument, Interpolation, MemorySource, Sampler, SamplerVoice};

const FADE: usize = 288;
const BLOCK: usize = 128;

/// What a voice a fifth above its sample costs through the sinc: 16 times
/// the speed it reads at.
const FIFTH: f64 = 16.0 * 1.498_307_076_876_681_5;

/// A looping sine on keys 60 to 90, and one that plays once on 30 to 59.
const TEXT: &str = "
<global> pitch_keycenter=60 amp_veltrack=100 ampeg_release=0.02
<region> sample=loop.wav lokey=60 hikey=90 loop_mode=loop_continuous loop_start=0 loop_end=4799
<region> sample=once.wav lokey=30 hikey=59 pitch_keycenter=45
";

fn source() -> MemorySource {
    MemorySource::new()
        .with("loop.wav", sine_wav(440.0, 48_000, 0.1, 0.5))
        .with("once.wav", sine_wav(220.0, 48_000, 2.0, 0.5))
}

fn sampler(polyphony: usize) -> Sampler<f32> {
    sampler_with(TEXT, &mut source(), polyphony, 1)
}

/// How a block is rendered.
#[derive(Clone, Copy)]
enum How {
    Free,
    /// Offline, with no budget at all.
    Offline,
    Within(f64),
}

/// Renders `frames` frames in blocks of `block`.
fn play(
    sampler: &mut Sampler<f32>,
    script: &[(usize, EventKind)],
    frames: usize,
    block: usize,
    how: impl Fn(usize) -> How,
) -> Vec<f32> {
    let mut output = Vec::with_capacity(frames);
    let mut events = EventBuffer::with_capacity(64);
    let mut buffer = AudioBuffer::<f32>::new(1, block);
    let mut at = 0;
    while at < frames {
        let length = block.min(frames - at);
        events.clear();
        for &(frame, kind) in script {
            if (at..at + length).contains(&frame) {
                let offset = (frame - at) as u32;
                events.push(Event { offset, kind }).unwrap();
            }
        }
        buffer.set_num_frames(length);
        let context = ProcessContext::new(&events, &Transport::STOPPED);
        match how(at) {
            How::Free => sampler.process_with(&mut buffer, &context),
            How::Offline => sampler.process_budgeted(
                &mut buffer,
                &context.with_offline(true),
                &mut Allowance::new(0.0),
            ),
            How::Within(units) => {
                sampler.process_budgeted(&mut buffer, &context, &mut Allowance::new(units));
            }
        }
        output.extend_from_slice(buffer.channel(0));
        at += length;
    }
    output
}

/// One block of `frames` within `units`: the output, what was spent and
/// whether it was limited.
fn block(sampler: &mut Sampler<f32>, frames: usize, units: f64) -> (Vec<f32>, f64, bool) {
    let mut buffer = AudioBuffer::<f32>::new(1, frames);
    let mut allowance = Allowance::new(units);
    sampler.process_budgeted(&mut buffer, &ProcessContext::empty(), &mut allowance);
    (
        buffer.channel(0).to_vec(),
        allowance.spent(),
        allowance.limited(),
    )
}

fn phrase() -> Vec<(usize, EventKind)> {
    vec![
        (0, on(67, 0.8)),
        (3, on(45, 1.0)),
        (500, on(72, 0.4)),
        (700, controller(64, 127)),
        (900, off(67)),
        (1_000, bend(2_000)),
        (1_500, on(50, 0.7)),
        (1_900, controller(64, 0)),
        (2_500, on(64, 0.9)),
        (2_600, on(79, 0.9)),
        (3_000, off(72)),
        (3_500, EventKind::NoteChoke { note: note(64) }),
        (4_000, controller(123, 0)),
    ]
}

#[test]
fn a_budget_that_covers_every_voice_changes_nothing() {
    for interpolation in [Interpolation::Sinc, Interpolation::Cubic] {
        for blocks in [1, 7, 256, 512] {
            let run = |how: How| {
                // Three voices for a phrase that wants five.
                let mut sampler = sampler(3);
                sampler.set_interpolation(interpolation);
                play(&mut sampler, &phrase(), 6_000, blocks, move |_| how)
            };
            let free = run(How::Free);
            assert!(free.iter().any(|&sample| sample != 0.0));
            for how in [How::Within(f64::INFINITY), How::Within(1e6), How::Offline] {
                assert!(run(how) == free, "{interpolation:?}, blocks of {blocks}");
            }
        }
    }
}

#[test]
fn a_voice_the_sinc_does_not_fit_for_reads_through_the_cubic() {
    let cubic = |script: &[(usize, EventKind)]| {
        let mut sampler = sampler(4);
        sampler.set_interpolation(Interpolation::Cubic);
        play(&mut sampler, script, 3_000, BLOCK, |_| How::Free)
    };
    let sinc =
        |script: &[(usize, EventKind)]| play(&mut sampler(4), script, 3_000, BLOCK, |_| How::Free);
    let script = [(0, on(67, 1.0))];
    assert_ne!(cubic(&script), sinc(&script));
    // Demoted from its first frame.
    let mut demoted = sampler(4);
    let output = play(&mut demoted, &script, 3_000, BLOCK, |_| How::Within(2.0));
    assert!(output == cubic(&script));
    assert_eq!((demoted.demoted_voices(), demoted.active_voices()), (1, 1));

    // Demoted while it sounds, and promoted again: each way over the fade,
    // after which it is what the other interpolator reads, bit for bit,
    // with no step larger than the sound's own.
    let mut governed = sampler(4);
    let output = play(&mut governed, &script, 3_000, BLOCK, |at| {
        How::Within(if (1_024..2_048).contains(&at) {
            2.0
        } else {
            FIFTH + 0.01
        })
    });
    let (sinc, cubic) = (sinc(&script), cubic(&script));
    assert_eq!(output[..1_024], sinc[..1_024]);
    assert_eq!(output[1_024 + FADE..2_048], cubic[1_024 + FADE..2_048]);
    assert_eq!(output[2_048 + FADE..], sinc[2_048 + FADE..]);
    for frame in 1_024..1_024 + FADE {
        let (low, high) = (sinc[frame].min(cubic[frame]), sinc[frame].max(cubic[frame]));
        assert!(
            (low - 1e-6..=high + 1e-6).contains(&output[frame]),
            "{frame}"
        );
    }
}

#[test]
fn units_spent_follow_the_way_each_voice_is_read() {
    let mut sampler = sampler(8);
    // At its sample's own pitch and rate: read as it is.
    sampler.note_on(note(60), 1.0);
    assert_eq!(block(&mut sampler, BLOCK, 100.0).1, 1.0);
    // A fifth up through the sinc, an octave down below it.
    sampler.note_on(note(67), 1.0);
    sampler.note_on(note(33), 1.0);
    let (_, spent, limited) = block(&mut sampler, BLOCK, 100.0);
    assert!((spent - (1.0 + FIFTH + 5.0)).abs() < 1e-6, "{spent}");
    assert!(!limited);
    sampler.set_interpolation(Interpolation::Cubic);
    assert_eq!(block(&mut sampler, BLOCK, 100.0).1, 1.0 + 1.5 + 1.5);
}

#[test]
fn the_loudest_voices_not_released_get_the_budget_first() {
    let alone = |key: u8, velocity: f32| {
        play(
            &mut sampler(4),
            &[(0, on(key, velocity))],
            512,
            BLOCK,
            |_| How::Free,
        )
    };
    // Room for one voice through the cubic only.
    let mut sampler = sampler(4);
    sampler.set_interpolation(Interpolation::Cubic);
    let output = play(
        &mut sampler,
        &[(0, on(67, 0.3)), (0, on(72, 0.9))],
        512,
        BLOCK,
        |_| How::Within(1.5),
    );
    let loud = {
        let mut alone = self::sampler(4);
        alone.set_interpolation(Interpolation::Cubic);
        play(&mut alone, &[(0, on(72, 0.9))], 512, BLOCK, |_| How::Free)
    };
    assert!(output == loud);
    assert_ne!(output, alone(67, 0.3));
    // The soft loop waits.
    assert_eq!(
        (sampler.active_voices(), sampler.suspended_voices()),
        (1, 1)
    );

    // A released voice gives way to a softer one that is held.
    sampler.note_off(note(72), 0.0);
    block(&mut sampler, FADE, 1.5);
    let (_, spent, _) = block(&mut sampler, BLOCK, 1.5);
    assert_eq!(spent, 1.5);
    assert_eq!(
        (sampler.active_voices(), sampler.suspended_voices()),
        (1, 0)
    );
}

#[test]
fn a_loop_waits_for_the_budget_and_a_sample_played_once_is_stolen() {
    let mut sampler = sampler(4);
    sampler.note_on(note(60), 1.0);
    sampler.note_on(note(45), 1.0);
    block(&mut sampler, BLOCK, 100.0);
    assert_eq!(sampler.active_voices(), 2);

    // No room at all: both fade out, costing what they do while they last.
    let (output, spent, limited) = block(&mut sampler, 512, 0.0);
    assert!(limited);
    assert!((spent - 2.0 * FADE as f64 / 512.0).abs() < 1e-9, "{spent}");
    assert!(output[..32].iter().any(|&sample| sample != 0.0));
    assert!(output[FADE..].iter().all(|&sample| sample == 0.0));
    assert_eq!(
        (sampler.active_voices(), sampler.suspended_voices()),
        (0, 1)
    );

    // Room again: the loop comes back, from where it stood.
    let (output, spent, _) = block(&mut sampler, 512, 100.0);
    assert!((spent - 1.0).abs() < 1e-9);
    assert!(output[0].abs() < 0.01 && rms(&output[FADE..]) > 0.1);
    assert_eq!(sampler.active_voices(), 1);

    // Its key comes up while it waits: it ends.
    block(&mut sampler, 512, 0.0);
    sampler.note_off(note(60), 0.0);
    let (output, spent, _) = block(&mut sampler, BLOCK, 100.0);
    assert_eq!((output, spent), (vec![0.0; BLOCK], 0.0));
    assert_eq!(
        (sampler.active_voices(), sampler.suspended_voices()),
        (0, 0)
    );

    // A new note that nothing fits for: a loop waits unheard, anything else
    // is not played.
    sampler.note_on(note(45), 1.0);
    sampler.note_on(note(60), 1.0);
    let (output, spent, limited) = block(&mut sampler, BLOCK, 0.5);
    assert_eq!((output, spent, limited), (vec![0.0; BLOCK], 0.0, true));
    assert_eq!(
        (sampler.active_voices(), sampler.suspended_voices()),
        (0, 1)
    );
}

#[test]
fn a_waiting_voice_gives_way_first_to_a_new_note() {
    let mut sampler = sampler(2);
    // The louder first, so that the one left waiting is the newer.
    sampler.note_on(note(72), 1.0);
    sampler.note_on(note(60), 0.2);
    block(&mut sampler, BLOCK, 100.0);
    // Room for the louder, an octave up, and not a unit more.
    block(&mut sampler, 512, 32.5);
    assert_eq!(sampler.suspended_voices(), 1);
    sampler.note_on(note(67), 1.0);
    block(&mut sampler, 512, 1_000.0);
    // The waiting one went, though the other is the older.
    assert_eq!(
        (sampler.active_voices(), sampler.suspended_voices()),
        (2, 0)
    );
    // What it left room for is the other two.
    let (_, spent, _) = block(&mut sampler, BLOCK, 1_000.0);
    assert!((spent - (32.0 + FIFTH)).abs() < 1e-6, "{spent}");
}

#[test]
fn process_with_brings_thinned_voices_back_to_full_quality() {
    let script = [(0, on(67, 1.0)), (0, on(72, 1.0))];
    let mut sampler = sampler(4);
    play(&mut sampler, &script, 1_024, BLOCK, |_| How::Within(1.5));
    assert_eq!(
        (sampler.demoted_voices(), sampler.suspended_voices()),
        (1, 1)
    );
    play(&mut sampler, &[], 1_024, BLOCK, |_| How::Free);
    assert_eq!(
        (
            sampler.active_voices(),
            sampler.demoted_voices(),
            sampler.suspended_voices()
        ),
        (2, 0, 0)
    );
}

#[derive(Clone, Default)]
struct FakeClock(std::rc::Rc<std::cell::Cell<(Duration, u64)>>);

impl Clock for FakeClock {
    fn now(&mut self) -> Duration {
        let (now, reads) = self.0.get();
        self.0.set((now, reads + 1));
        now
    }
}

#[test]
fn voices_degrade_under_a_governor_and_recover() {
    const NOTES: usize = 8;
    let clock = FakeClock::default();
    let mut governor = Governor::with_clock(GovernorConfig::default(), clock.clone());
    governor.set_sample_rate(RATE);
    let mut sampler = sampler(NOTES);
    for key in 0..NOTES as u8 {
        sampler.note_on(note(61 + key), 1.0);
    }
    let mut buffer = AudioBuffer::<f32>::new(1, BLOCK);
    // What `Sampler::process_governed` does, with the clock moved by what
    // was spent.
    let mut run = |sampler: &mut Sampler<f32>,
                   governor: &mut Governor<FakeClock>,
                   unit_load: f64,
                   blocks: usize| {
        for _ in 0..blocks {
            governor.begin_block();
            let mut allowance = governor.allowance();
            sampler.process_budgeted(&mut buffer, &ProcessContext::empty(), &mut allowance);
            let (now, reads) = clock.0.get();
            let time = unit_load * allowance.spent() * BLOCK as f64 / RATE;
            clock.0.set((now + Duration::from_secs_f64(time), reads));
            governor.end_block(BLOCK, allowance.spent(), allowance.limited());
        }
    };
    // Every voice through the sinc, 16 to 25 units each, takes well over a
    // deadline.
    run(&mut sampler, &mut governor, 0.01, 150);
    assert!(governor.last_load() <= 0.7, "{}", governor.last_load());
    assert!(sampler.demoted_voices() > 0);
    let heard = sampler.active_voices();
    assert_eq!(heard + sampler.suspended_voices(), NOTES);

    run(&mut sampler, &mut governor, 0.000_2, 800);
    assert_eq!(
        (
            sampler.active_voices(),
            sampler.demoted_voices(),
            sampler.suspended_voices()
        ),
        (NOTES, 0, 0)
    );
}

#[test]
fn an_offline_block_reads_no_clock() {
    let clock = FakeClock::default();
    let mut governor = Governor::with_clock(GovernorConfig::default(), clock.clone());
    governor.set_max_budget(0.0);
    let mut sampler = sampler(4);
    sampler.note_on(note(67), 1.0);
    let mut buffer = AudioBuffer::<f32>::new(1, BLOCK);
    let offline = ProcessContext::empty().with_offline(true);
    sampler.process_governed(&mut buffer, &offline, &mut governor);
    assert!(buffer.peak() > 0.1);
    assert_eq!((clock.0.get().1, governor.stats().blocks), (0, 0));
    sampler.process_governed(&mut buffer, &ProcessContext::empty(), &mut governor);
    assert_eq!((clock.0.get().1, governor.stats().blocks), (2, 1));
}

#[test]
fn sampler_voices_in_a_synth_spend_a_budget_too() {
    let instrument = Arc::new(Instrument::from_sfz(TEXT, &mut source()).unwrap());
    let make = |interpolation| {
        let mut synth = Synth::with_voices(4, || {
            let mut voice = SamplerVoice::new(Arc::clone(&instrument));
            voice.set_interpolation(interpolation);
            voice
        });
        synth.prepare(&ProcessSpec {
            sample_rate: RATE,
            max_block_size: BLOCK,
            num_channels: 1,
        });
        synth
    };
    let render = |synth: &mut Synth<SamplerVoice>, units: Option<f64>, blocks: usize| {
        let mut buffer = AudioBuffer::<f32>::new(1, BLOCK);
        let mut output = Vec::new();
        let mut spent = 0.0;
        for _ in 0..blocks {
            match units {
                Some(units) => {
                    let mut allowance = Allowance::new(units);
                    synth.process_budgeted(&mut buffer, &ProcessContext::empty(), &mut allowance);
                    spent = allowance.spent();
                }
                None => synth.process(&mut buffer),
            }
            output.extend_from_slice(buffer.channel(0));
        }
        (output, spent)
    };
    let start = |synth: &mut Synth<SamplerVoice>| {
        synth.note_on(note(67), 1.0);
    };

    let (mut free, mut roomy) = (make(Interpolation::Sinc), make(Interpolation::Sinc));
    start(&mut free);
    start(&mut roomy);
    let whole = render(&mut free, None, 8).0;
    let (output, spent) = render(&mut roomy, Some(100.0), 8);
    assert!(output == whole && (spent - FIFTH).abs() < 1e-6);

    // Demoted: as a voice that reads through the cubic.
    let (mut tight, mut cubic) = (make(Interpolation::Sinc), make(Interpolation::Cubic));
    start(&mut tight);
    start(&mut cubic);
    let (output, spent) = render(&mut tight, Some(2.0), 8);
    assert!(output == render(&mut cubic, None, 8).0);
    assert_eq!((spent, tight.demoted_voices()), (1.5, 1));

    // A loop waits.
    render(&mut tight, Some(0.0), 8);
    assert_eq!((tight.active_voices(), tight.suspended_voices()), (0, 1));
    assert!(
        render(&mut tight, Some(100.0), 8).0[FADE..]
            .iter()
            .any(|&sample| sample.abs() > 0.1)
    );
    assert_eq!(FADE, (BUDGET_FADE_SECONDS * RATE) as usize);
}
