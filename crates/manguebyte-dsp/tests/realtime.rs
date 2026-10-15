//! Proves that processing never touches the heap.
//!
//! The real-time contract says `process` must not allocate or free memory.
//! A promise in a doc comment is easy to break by accident, so this test
//! replaces the global allocator with one that counts, and fails if any
//! processor allocates, reallocates or frees while it runs.
//!
//! When you add a processor, add it here.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use manguebyte_core::AudioBuffer;
use manguebyte_dsp::event::{Event, EventBuffer, EventKind, Expression, NoteRef};
use manguebyte_dsp::fft::{Complex, Fft, RealFft};
use manguebyte_dsp::fir;
use manguebyte_dsp::iir::{Band, Design};
use manguebyte_dsp::{
    Adsr, AdsrParams, Biquad, BiquadCoefficients, Compressor, DcBlocker, Delay, Filter,
    FilterShape, Gain, Limiter, Noise, NoiseColor, NoiseGate, Oscillator, Oversampled,
    ProcessContext, ProcessSpec, Processor, Resampler, Reverb, ShaperCurve, StateVariableFilter,
    SvfMode, Synth, Transport, Voice, Waveform, Waveshaper,
};
use manguebyte_dsp::{
    BiquadCascade, Crossover, CrossoverSlope, IirFilter, LadderFilter, LadderMode, OnePole,
    OnePoleMode,
};
use manguebyte_dsp::{
    Chorus, DelayInterpolation, DelayLine, DryWet, EnvelopeFollower, LevelDetector, MixLaw,
    PanMode, Panner, Phaser,
};
use manguebyte_dsp::{
    Convolution, ConvolutionEngine, ConvolutionRouting, ConvolutionSettings, Expander, FirFilter,
    FirMode, KeySource, Wavetable, WavetableOscillator, split_at_events_with,
};

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

const SPEC: ProcessSpec = ProcessSpec {
    sample_rate: 48_000.0,
    max_block_size: 256,
    num_channels: 2,
};

/// Prepares a processor, then runs it over blocks of several sizes with the
/// allocator watched.
fn check<P: Processor<f32>>(what: &str, mut processor: P) {
    processor.prepare(&SPEC);
    let mut buffer = AudioBuffer::<f32>::new(SPEC.num_channels, SPEC.max_block_size);
    assert_no_heap_use(what, || {
        for frames in [256, 1, 64, 255, 256] {
            buffer.set_num_frames(frames);
            for channel in buffer.iter_channels_mut() {
                channel.fill(0.25);
            }
            processor.process(&mut buffer);
        }
        processor.reset();
        buffer.set_num_frames(256);
        processor.process(&mut buffer);
    });
}

#[test]
fn the_guard_itself_catches_an_allocation() {
    let caught = std::panic::catch_unwind(|| {
        assert_no_heap_use("a Vec", || drop(std::hint::black_box(vec![0u8; 64])));
    });
    WATCHING.with(|watching| watching.set(false));
    assert!(caught.is_err(), "the allocation guard is not working");
}

#[test]
fn processors_do_not_use_the_heap() {
    check("Gain", Gain::<f32>::from_db(-6.0));
    check("Oscillator", Oscillator::new(Waveform::Saw, 220.0));
    check(
        "Filter",
        Filter::<f32>::new(FilterShape::Lowpass, 1_000.0, 0.707),
    );
    check(
        "Biquad",
        Biquad::<f32>::new(BiquadCoefficients::design(
            FilterShape::Peak { gain_db: 6.0 },
            48_000.0,
            2_000.0,
            1.0,
        )),
    );
    check("Adsr", {
        let mut envelope = Adsr::new(AdsrParams::default());
        envelope.note_on();
        envelope
    });
    check(
        "Delay",
        Delay::<f32>::new(0.5)
            .with_time(0.125)
            .with_feedback(0.4)
            .with_mix(0.5),
    );
}

#[test]
fn newer_processors_do_not_use_the_heap() {
    check(
        "StateVariableFilter",
        StateVariableFilter::<f32>::new(SvfMode::Bandpass, 1_000.0, 2.0),
    );
    check(
        "Compressor",
        Compressor::<f32>::new()
            .with_threshold_db(-30.0)
            .with_knee_db(6.0),
    );
    check(
        "NoiseGate",
        NoiseGate::<f32>::new().with_threshold_db(-20.0),
    );
    check("Reverb", {
        let mut reverb = Reverb::<f32>::new();
        // Settings may be changed from the audio thread too.
        reverb.set_size(0.8);
        reverb.set_decay_seconds(4.0);
        reverb
    });
}

#[test]
fn a_chain_does_not_use_the_heap_and_can_change_settings_while_running() {
    let mut chain = (
        Oscillator::new(Waveform::Square, 110.0),
        Filter::<f32>::new(FilterShape::Lowpass, 800.0, 1.2),
        Gain::<f32>::new(0.5),
    );
    chain.prepare(&SPEC);
    let mut buffer = AudioBuffer::<f32>::new(2, 256);
    assert_no_heap_use("a tuple chain", || {
        for step in 0..8 {
            // Parameter changes are part of what the audio thread does.
            chain.0.set_frequency(110.0 + 20.0 * f64::from(step));
            chain.1.set_frequency(800.0 + 300.0 * f64::from(step));
            chain.1.set_shape(FilterShape::Highpass);
            chain.2.set_gain_db(-f64::from(step));
            chain.process(&mut buffer);
        }
    });
}

#[test]
fn the_second_batch_of_processors_does_not_use_the_heap() {
    check("Limiter", Limiter::<f32>::new().with_ceiling_db(-12.0));
    check(
        "Waveshaper",
        Waveshaper::<f32>::new(ShaperCurve::Fold).with_drive_db(24.0),
    );
    check("DcBlocker", DcBlocker::<f32>::new());
    check("Noise", Noise::new(NoiseColor::Pink));
    check(
        "Oversampled",
        Oversampled::<_, f32>::new(Waveshaper::<f32>::new(ShaperCurve::Tanh), 8),
    );
}

#[test]
fn resampling_does_not_use_the_heap() {
    for (from, to) in [(44_100.0, 48_000.0), (48_000.0, 16_000.0)] {
        let mut resampler = Resampler::<f32>::new(from, to, 2, 256);
        let mut input = AudioBuffer::<f32>::new(2, 256);
        let mut output = AudioBuffer::<f32>::new(2, resampler.max_output_frames(256));
        assert_no_heap_use("Resampler", || {
            for frames in [256, 1, 100, 256] {
                input.set_num_frames(frames);
                resampler.process(&input, &mut output);
            }
        });
    }
}

/// A voice made of the library's own parts.
struct ToneVoice {
    oscillator: Oscillator,
    envelope: Adsr,
}

impl Voice for ToneVoice {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.oscillator.set_sample_rate(spec.sample_rate);
        self.envelope.set_sample_rate(spec.sample_rate);
    }
    fn note_on(&mut self, note: NoteRef, _: f32) {
        self.oscillator
            .set_frequency(110.0 * f64::from(note.key) / 45.0);
        self.envelope.note_on();
    }
    fn note_off(&mut self, _: f32) {
        self.envelope.note_off();
    }
    fn choke(&mut self) {
        self.envelope.silence();
        self.oscillator.reset_phase();
    }
    fn is_active(&self) -> bool {
        self.envelope.is_active()
    }
    fn render(&mut self, buffer: &mut AudioBuffer<f32>) {
        for frame in 0..buffer.num_frames() {
            let sample = (self.oscillator.next_sample() * self.envelope.next_sample()) as f32;
            for channel in buffer.iter_channels_mut() {
                channel[frame] += sample;
            }
        }
    }
}

#[test]
fn a_synth_handles_a_flood_of_events_without_using_the_heap() {
    let mut synth = Synth::with_voices(4, || ToneVoice {
        oscillator: Oscillator::new(Waveform::Saw, 220.0),
        envelope: Adsr::new(AdsrParams::default()),
    });
    synth.prepare(&SPEC);

    let mut events = EventBuffer::with_capacity(64);
    let mut buffer = AudioBuffer::<f32>::new(2, 256);
    let transport = Transport::playing_at(120.0);
    assert_no_heap_use("Synth", || {
        for round in 0..16u8 {
            events.clear();
            // More notes than voices, so voices are taken over as well.
            for step in 0..6u8 {
                let note = NoteRef::new(0, 40 + (round + step) % 12);
                let velocity = 0.7;
                let offset = u32::from(step) * 40;
                let _ = events.push(Event {
                    offset,
                    kind: EventKind::NoteOn { note, velocity },
                });
                let _ = events.push(Event {
                    offset: offset + 20,
                    kind: EventKind::NoteExpression {
                        note,
                        expression: Expression::Tuning,
                        value: 0.1,
                    },
                });
                let _ = events.push(Event {
                    offset: offset + 30,
                    kind: EventKind::NoteOff { note, velocity },
                });
            }
            synth.process_with(&mut buffer, &ProcessContext::new(&events, &transport));
        }
    });
    assert!(buffer.peak() > 0.0);
}

#[test]
fn transforms_do_not_use_the_heap() {
    for len in [256, 480] {
        let mut fft = Fft::<f32>::new(len);
        let mut data = vec![Complex::new(0.5, -0.5); len];
        assert_no_heap_use("Fft", || {
            fft.forward(&mut data);
            fft.inverse(&mut data);
        });

        let mut real = RealFft::<f32>::new(len);
        let signal = vec![0.25f32; len];
        let mut spectrum = vec![Complex::ZERO; real.spectrum_len()];
        let mut restored = vec![0.0f32; len];
        assert_no_heap_use("RealFft", || {
            real.forward(&signal, &mut spectrum);
            real.inverse(&spectrum, &mut restored);
        });
    }
}

const INTERPOLATIONS: [DelayInterpolation; 4] = [
    DelayInterpolation::None,
    DelayInterpolation::Linear,
    DelayInterpolation::Cubic,
    DelayInterpolation::Thiran,
];

/// Whether every sample of `buffer` is a finite number.
///
/// `AudioBuffer::peak` cannot tell: it compares with `>`, which is false for
/// not-a-number, so a buffer that holds nothing else has a peak of zero.
fn all_finite(buffer: &AudioBuffer<f32>) -> bool {
    buffer
        .iter_channels()
        .all(|channel| channel.iter().all(|sample| sample.is_finite()))
}

#[test]
fn the_finiteness_check_catches_nan_and_infinity() {
    let mut buffer = AudioBuffer::<f32>::new(2, 8);
    assert!(all_finite(&buffer));
    buffer.channel_mut(1)[7] = f32::NAN;
    assert!(!all_finite(&buffer));
    assert!(
        buffer.peak().is_nan(),
        "the peak does not hide a NaN either"
    );
    buffer.channel_mut(1)[7] = f32::NEG_INFINITY;
    assert!(!all_finite(&buffer));
}

#[test]
fn modulation_mixing_and_level_processors_do_not_use_the_heap() {
    check(
        "Chorus",
        Chorus::<f32>::new().with_voices(8).with_feedback(-0.9),
    );
    check("Chorus as a flanger", Chorus::<f32>::flanger());
    check(
        "Phaser",
        Phaser::<f32>::new()
            .with_stages(12)
            .with_feedback(0.9)
            .with_spread(1.0),
    );
    check("Panner", Panner::<f32>::new().with_pan(-0.4));
    check(
        "Panner from mono",
        Panner::<f32>::new()
            .with_mode(PanMode::MonoToStereo)
            .with_law(MixLaw::Sine4Point5dB)
            .with_pan(0.7),
    );
    check(
        "EnvelopeFollower",
        EnvelopeFollower::<f32>::new().with_detector(LevelDetector::Rms),
    );
    check(
        "linked EnvelopeFollower",
        EnvelopeFollower::<f32>::new().with_linked(true),
    );
    for interpolation in INTERPOLATIONS {
        check(
            "Delay between samples",
            Delay::<f32>::new(0.1)
                .with_time(0.012_3)
                .with_feedback(0.5)
                .with_interpolation(interpolation),
        );
    }
}

#[test]
fn their_settings_can_change_while_running_without_using_the_heap() {
    let mut chain = (
        Chorus::<f32>::new(),
        Phaser::<f32>::new(),
        Panner::<f32>::new(),
        EnvelopeFollower::<f32>::new(),
    );
    chain.prepare(&SPEC);
    let mut buffer = AudioBuffer::<f32>::new(2, 256);
    assert_no_heap_use("changing settings", || {
        for step in 0..14u8 {
            let turn = f64::from(step);
            for channel in buffer.iter_channels_mut() {
                channel.fill(0.25);
            }

            chain.0.set_voices(1 + usize::from(step % 8));
            chain.0.set_rate_hz(0.2 + turn);
            chain.0.set_depth_ms(turn);
            chain.0.set_center_delay_ms(5.0 + 3.0 * turn);
            chain.0.set_feedback(0.9 - 0.15 * turn);
            chain.0.set_spread(turn / 14.0);
            chain.0.set_mix(turn / 14.0);

            chain.1.set_stages(2 + usize::from(step));
            chain.1.set_rate_hz(0.2 + turn);
            chain.1.set_depth_octaves(turn / 4.0);
            chain.1.set_center_hz(200.0 + 500.0 * turn);
            chain.1.set_feedback(0.15 * turn - 0.9);
            chain.1.set_spread(turn / 14.0);
            chain.1.set_mix(1.0 - turn / 14.0);

            let law = MixLaw::ALL[usize::from(step) % MixLaw::ALL.len()];
            chain.2.set_pan(turn / 7.0 - 1.0);
            chain.2.set_law(law);
            chain.2.set_mode(if step % 2 == 0 {
                PanMode::MonoToStereo
            } else {
                PanMode::StereoBalance
            });

            chain.3.set_detector(if step % 3 == 0 {
                LevelDetector::Rms
            } else {
                LevelDetector::Peak
            });
            chain.3.set_attack_ms(turn);
            chain.3.set_release_ms(10.0 * turn);
            chain.3.set_linked(step % 2 == 1);

            chain.process(&mut buffer);
        }
    });
    assert!(all_finite(&buffer));
}

#[test]
fn a_dry_wet_mixer_does_not_use_the_heap() {
    let mut limiter = Limiter::<f32>::new();
    limiter.prepare(&SPEC);
    let mut mixer = DryWet::<f32>::new()
        .with_law(MixLaw::EqualPower)
        .with_mix(0.5)
        .with_max_latency(512)
        .with_latency(limiter.latency_samples());
    mixer.prepare(&SPEC);

    let mut buffer = AudioBuffer::<f32>::new(SPEC.num_channels, SPEC.max_block_size);
    assert_no_heap_use("DryWet", || {
        for (round, frames) in [256, 1, 64, 255, 256, 256, 256].into_iter().enumerate() {
            buffer.set_num_frames(frames);
            for channel in buffer.iter_channels_mut() {
                channel.fill(0.25);
            }
            // Settings may be changed from the audio thread too. A new
            // maximum latency is only a number until the next `prepare`, so
            // the latency stays within the 512 samples there is memory for.
            mixer.set_mix(round as f64 / 6.0);
            mixer.set_law(MixLaw::ALL[round % MixLaw::ALL.len()]);
            mixer.set_max_latency(1_000 * round);
            mixer.set_latency(100 * round);
            assert!(mixer.latency() <= 512);

            mixer.push_dry(&buffer);
            limiter.process(&mut buffer);
            mixer.mix_wet(&mut buffer);
        }
        mixer.reset();
    });
    assert!(all_finite(&buffer));
}

#[test]
fn a_delay_line_and_a_follower_used_by_the_sample_do_not_use_the_heap() {
    let mut line = DelayLine::<f32>::new(64);
    assert_no_heap_use("DelayLine", || {
        for interpolation in INTERPOLATIONS {
            line.set_interpolation(interpolation);
            for step in 0..300u16 {
                let delay = 1.0 + 0.31 * f64::from(step);
                let read = line.read_interpolated(delay);
                line.push(0.25 + 0.5 * read);
            }
        }
        line.clear();
    });

    let mut follower = EnvelopeFollower::<f32>::new();
    follower.prepare(&SPEC);
    let block = AudioBuffer::<f32>::new(2, 256);
    assert_no_heap_use("EnvelopeFollower", || {
        for step in 0..256u16 {
            let sample = f32::from(step) / 256.0;
            follower.process_sample(0, sample);
            follower.process_sample(1, -sample);
            follower.process_frame(&[sample, -sample]);
        }
        follower.follow(&block);
        follower.set_linked(true);
        follower.follow(&block);
        assert!(follower.level(0) >= 0.0);
        assert!(follower.channel_level(1) >= 0.0);
        assert!(follower.shared_level() >= 0.0);
        follower.set_linked(false);
        follower.follow(&block);
    });

    let mut panner = Panner::<f32>::new();
    panner.prepare(&SPEC);
    assert_no_heap_use("Panner", || {
        panner.set_pan(1.0);
        for _ in 0..2_000 {
            panner.next_gains();
        }
        assert_eq!(panner.next_gains().0, 0.0);
    });
}

#[test]
fn the_filter_family_does_not_use_the_heap() {
    check(
        "OnePole",
        OnePole::<f32>::new(OnePoleMode::Allpass, 1_000.0),
    );
    check(
        "LadderFilter",
        LadderFilter::<f32>::new(LadderMode::Lp24, 800.0)
            .with_resonance(1.0)
            .with_drive_db(24.0),
    );
    let band = Band::Bandpass {
        low_hz: 300.0,
        high_hz: 3_000.0,
    };
    let design = Design::chebyshev_ii(band, 16, 80.0);
    check(
        "BiquadCascade",
        BiquadCascade::<f32>::new(&design.sections(SPEC.sample_rate)),
    );
    check(
        "BiquadCascade from sections",
        BiquadCascade::<f32>::from_sections(&design.sections(SPEC.sample_rate)),
    );
    check("IirFilter", IirFilter::<f32>::new(design));
    for slope in CrossoverSlope::ALL {
        check(
            "Crossover",
            Crossover::<f32>::new(slope, &[100.0, 1_000.0, 5_000.0, 10_000.0]),
        );
    }
}

#[test]
fn a_crossover_splits_and_adds_up_without_using_the_heap() {
    let mut crossover = Crossover::<f32>::new(CrossoverSlope::Lr4, &[200.0, 2_000.0]);
    crossover.prepare(&SPEC);
    let mut input = AudioBuffer::<f32>::new(SPEC.num_channels, SPEC.max_block_size);
    let mut output = AudioBuffer::<f32>::new(SPEC.num_channels, SPEC.max_block_size);
    assert_no_heap_use("Crossover", || {
        for (step, frames) in [256, 1, 64, 255, 256, 256].into_iter().enumerate() {
            input.set_num_frames(frames);
            output.set_num_frames(frames);
            for channel in input.iter_channels_mut() {
                channel.fill(0.25);
            }
            crossover.set_frequency(0, 100.0 + 50.0 * step as f64);
            crossover.set_frequency(1, 3_000.0 - 200.0 * step as f64);
            crossover.split(&input);
            // A band processed in place, as a multiband effect does.
            crossover.band_mut(1).apply_gain(0.5);
            crossover.sum_into(&mut output);
        }
        crossover.reset();
    });
    assert!(all_finite(&output));
    assert!(all_finite(crossover.band(2)));
}

#[test]
fn filters_can_be_redesigned_and_modulated_while_running_without_using_the_heap() {
    let mut one_pole = OnePole::<f32>::new(OnePoleMode::Lowpass, 100.0);
    let mut ladder = LadderFilter::<f32>::new(LadderMode::Lp24, 100.0);
    let mut iir = IirFilter::<f32>::new(Design::butterworth(Band::Lowpass { cutoff_hz: 100.0 }, 4));
    let mut cascade = BiquadCascade::<f32>::new(&[]);
    let mut exact = BiquadCascade::<f32>::new(&[]);
    one_pole.prepare(&SPEC);
    ladder.prepare(&SPEC);
    iir.prepare(&SPEC);
    cascade.prepare(&SPEC);
    exact.prepare(&SPEC);
    let mut buffer = AudioBuffer::<f32>::new(SPEC.num_channels, SPEC.max_block_size);
    assert_no_heap_use("redesigning and modulating filters", || {
        for step in 0..16u8 {
            let turn = f64::from(step);
            for channel in buffer.iter_channels_mut() {
                channel.fill(0.25);
            }
            let hz = 100.0 * 2f64.powf(turn / 2.0);
            one_pole.set_frequency(hz);
            one_pole.set_mode([OnePoleMode::Highpass, OnePoleMode::Allpass][usize::from(step % 2)]);
            ladder.set_frequency(hz);
            ladder.set_resonance(turn / 15.0);
            ladder.set_drive_db(3.0 * turn);
            ladder.set_mode(LadderMode::ALL[usize::from(step) % LadderMode::ALL.len()]);

            // A new design on every block, computed on the audio thread.
            let band = match step % 4 {
                0 => Band::Lowpass { cutoff_hz: hz },
                1 => Band::Highpass { cutoff_hz: hz },
                2 => Band::Bandpass {
                    low_hz: hz,
                    high_hz: 2.0 * hz,
                },
                _ => Band::Bandstop {
                    low_hz: hz,
                    high_hz: 2.0 * hz,
                },
            };
            let order = 1 + usize::from(step);
            let design = match step % 4 {
                0 => Design::butterworth(band, order),
                1 => Design::chebyshev_i(band, order, 0.5),
                2 => Design::chebyshev_ii(band, order, 60.0),
                // Too loose for orders 8, 12 and 16: the least attenuation
                // is searched for, on the audio thread too.
                _ => Design::elliptic(band, order, 3.0, 4.0),
            };
            iir.set_design(design);
            cascade.set_sections(&design.sections(SPEC.sample_rate));
            exact.set_from_sections(&design.sections(SPEC.sample_rate));

            one_pole.process(&mut buffer);
            ladder.process(&mut buffer);
            iir.process(&mut buffer);
            cascade.process(&mut buffer);
            exact.process(&mut buffer);
        }
        // By the sample, as audio-rate modulation does it.
        for n in 0..512u16 {
            let hz = 200.0 + 10.0 * f64::from(n);
            one_pole.set_frequency(hz);
            ladder.set_frequency(hz);
            let x = one_pole.process_sample(0, 0.25);
            ladder.process_sample(1, x);
        }
    });
    assert!(all_finite(&buffer));
}

/// A decaying, repeatable impulse response.
fn room(len: usize, seed: u32) -> Vec<f32> {
    let mut state = seed.wrapping_mul(2_654_435_761) | 1;
    (0..len)
        .map(|n| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let unit = state as f32 / u32::MAX as f32 * 2.0 - 1.0;
            0.1 * unit * (-4.0 * n as f32 / len as f32).exp()
        })
        .collect()
}

fn convolution_engine(
    len: usize,
    routing: ConvolutionRouting,
    zero_latency: bool,
) -> ConvolutionEngine<f32> {
    let count = match routing {
        ConvolutionRouting::Parallel => 2,
        ConvolutionRouting::TrueStereo => 4,
    };
    let responses: Vec<Vec<f32>> = (0..count).map(|seed| room(len, seed)).collect();
    let settings = ConvolutionSettings::new()
        .with_partition_size(64)
        .with_zero_latency(zero_latency);
    ConvolutionEngine::new(&responses, 2, routing, settings).unwrap()
}

#[test]
fn convolution_fir_filters_the_expander_and_the_wavetable_do_not_use_the_heap() {
    check(
        "ConvolutionEngine",
        convolution_engine(5_000, ConvolutionRouting::Parallel, true),
    );
    check(
        "ConvolutionEngine, true stereo with latency",
        convolution_engine(3_000, ConvolutionRouting::TrueStereo, false),
    );
    check("Convolution", {
        let (convolution, mut loader) = Convolution::<f32>::new();
        // Taken on the first block, inside the watched region.
        loader
            .load(convolution_engine(
                2_000,
                ConvolutionRouting::TrueStereo,
                true,
            ))
            .unwrap();
        convolution
    });
    check(
        "FirFilter, direct",
        FirFilter::<f32>::with_mode(&[0.05; 20], FirMode::Direct).unwrap(),
    );
    check(
        "FirFilter, FFT",
        FirFilter::<f32>::new(&fir::lowpass(1_001, 0.1, 80.0)).unwrap(),
    );
    check(
        "Expander",
        Expander::<f32>::new()
            .with_threshold_db(-6.0)
            .with_knee_db(6.0),
    );
    check("WavetableOscillator", {
        let saw: Vec<f32> = (0..2_048).map(|n| n as f32 / 1_024.0 - 1.0).collect();
        let square: Vec<f32> = (0..2_048)
            .map(|n| if n < 1_024 { 1.0 } else { -1.0 })
            .collect();
        let table = Arc::new(Wavetable::new(&[saw, square]).unwrap());
        let mut oscillator = WavetableOscillator::new(table, 110.0);
        oscillator.set_phase_modulation(0.5);
        oscillator.set_position_glide_ms(5.0);
        oscillator.set_position(0.7);
        oscillator
    });
}

#[test]
fn a_wavetable_can_be_retuned_morphed_and_swapped_while_running_without_using_the_heap() {
    let saw: Vec<f32> = (0..600).map(|n| n as f32 / 300.0 - 1.0).collect();
    let sine: Vec<f32> = (0..600)
        .map(|n| (std::f32::consts::TAU * n as f32 / 600.0).sin())
        .collect();
    let first = Arc::new(Wavetable::new(&[saw.clone(), sine.clone()]).unwrap());
    let second = Arc::new(Wavetable::new(&[sine, saw]).unwrap());
    let mut oscillator = WavetableOscillator::new(Arc::clone(&first), 55.0);
    Processor::<f32>::prepare(&mut oscillator, &SPEC);
    let mut buffer = AudioBuffer::<f32>::new(2, 256);
    let mut spare = Some(Arc::clone(&second));
    assert_no_heap_use("WavetableOscillator, while playing", || {
        for step in 0..32u8 {
            oscillator.set_frequency(55.0 * 2f64.powf(f64::from(step) / 4.0));
            oscillator.set_position(f64::from(step % 3) / 2.0);
            oscillator.set_phase_modulation(f64::from(step) / 32.0);
            if let Some(table) = spare.take() {
                // The old table comes back instead of being dropped here.
                spare = Some(oscillator.set_table(table));
            }
            oscillator.process(&mut buffer);
        }
    });
    assert!(all_finite(&buffer));
}

#[test]
fn sidechained_dynamics_do_not_use_the_heap() {
    let mut events = EventBuffer::with_capacity(4);
    for offset in [3, 50, 50] {
        events
            .push(Event {
                offset,
                kind: EventKind::NoteOn {
                    note: NoteRef::new(0, 60),
                    velocity: 1.0,
                },
            })
            .unwrap();
    }
    let transport = Transport::STOPPED;
    let mut key = AudioBuffer::<f32>::new(1, 256);
    key.channel_mut(0).fill(0.9);
    let context = ProcessContext::new(&events, &transport).with_sidechain(&key);
    let mut chain = (
        Compressor::<f32>::new().with_key_source(KeySource::Sidechain),
        NoiseGate::<f32>::new().with_key_source(KeySource::Sidechain),
        Expander::<f32>::new().with_key_source(KeySource::Sidechain),
    );
    chain.prepare(&SPEC);
    let mut buffer = AudioBuffer::<f32>::new(2, 256);
    assert_no_heap_use("sidechained dynamics", || {
        for frames in [256, 1, 100] {
            buffer.set_num_frames(frames);
            for channel in buffer.iter_channels_mut() {
                channel.fill(0.25);
            }
            chain.process_with(&mut buffer, &context);
            // Split at the events, each piece keyed on its own frames.
            split_at_events_with(
                &mut chain,
                &mut buffer,
                &context,
                |_, _| {},
                |chain, piece, piece_context| chain.process_with(piece, piece_context),
            );
        }
    });
    assert!(all_finite(&buffer));
}

#[test]
fn swapping_impulse_responses_while_playing_does_not_use_the_heap() {
    const LOADS: usize = 24;
    let (mut convolution, mut loader) = Convolution::<f32>::with_capacity(2);
    convolution.prepare(&SPEC);
    // Lets the builder give up if engines never come back, so that a broken
    // processor fails the test instead of hanging it.
    let stop = Arc::new(AtomicBool::new(false));
    let give_up = Arc::clone(&stop);

    // Engines are built, sent and dropped on another thread, as an editor
    // would. Only this thread is watched.
    let builder = std::thread::spawn(move || {
        for index in 0..LOADS {
            let routing = if index % 2 == 0 {
                ConvolutionRouting::Parallel
            } else {
                ConvolutionRouting::TrueStereo
            };
            let mut engine = convolution_engine(500 + 300 * index, routing, index % 3 != 0);
            loop {
                if give_up.load(Ordering::Relaxed) {
                    return (index, loader.in_flight());
                }
                match loader.load_with_crossfade(engine, 64 * (index % 4)) {
                    Ok(()) => break,
                    Err(refused) => {
                        engine = *refused;
                        std::thread::yield_now();
                    }
                }
            }
        }
        // Wait until the audio thread has taken the last one.
        while loader.in_flight() > 1 && !give_up.load(Ordering::Relaxed) {
            loader.collect();
            std::thread::yield_now();
        }
        (LOADS, loader.in_flight())
    });

    let mut buffer = AudioBuffer::<f32>::new(2, 256);
    let mut blocks = 0usize;
    assert_no_heap_use("Convolution, while engines are swapped", || {
        while !builder.is_finished() && blocks < 100_000 {
            buffer.set_num_frames([256, 64, 1, 200][blocks % 4]);
            for channel in buffer.iter_channels_mut() {
                channel.fill(0.25);
            }
            convolution.process(&mut buffer);
            blocks += 1;
        }
    });
    stop.store(true, Ordering::Relaxed);
    let (loaded, in_flight) = builder.join().unwrap();
    assert_eq!(loaded, LOADS, "the audio thread stopped taking engines");
    assert_eq!(in_flight, 1, "engines were lost");
    assert!(all_finite(&buffer));
    assert!(convolution.engine().is_some());
}
