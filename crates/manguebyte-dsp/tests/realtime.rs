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
use manguebyte_dsp::stft::{
    PitchShift, SpectralFrame, SpectralFreeze, SpectralGate, Stft, StftSettings, StftWindow,
};
use manguebyte_dsp::window::Window;
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

#[test]
fn spectral_processing_does_not_use_the_heap() {
    check(
        "Stft with a closure",
        Stft::from_fn(StftSettings::new(512), |frame: SpectralFrame<'_, f32>| {
            frame.bins[40..].fill(Complex::ZERO);
        })
        .unwrap(),
    );
    let root_hann = StftWindow::SquareRoot(Window::Hann);
    check(
        "Stft, root-Hann at 50 % on 480 samples",
        Stft::from_fn(
            StftSettings::new(480)
                .with_hop(240)
                .with_windows(root_hann, root_hann),
            |_: SpectralFrame<'_, f32>| {},
        )
        .unwrap(),
    );
    check(
        "SpectralGate",
        Stft::<_, f32>::new(StftSettings::new(1_024), SpectralGate::new(-30.0)).unwrap(),
    );
    check("SpectralFreeze", {
        let mut stft = Stft::<_, f32>::new(StftSettings::new(256), SpectralFreeze::new()).unwrap();
        stft.processor_mut().set_frozen(true);
        stft
    });
    check(
        "PitchShift",
        Stft::<_, f32>::new(StftSettings::new(512), PitchShift::new(1.5)).unwrap(),
    );
}

#[test]
fn spectral_effects_can_change_settings_while_running_without_using_the_heap() {
    let mut gate = Stft::<_, f32>::new(StftSettings::new(256), SpectralGate::new(-40.0)).unwrap();
    let mut freeze = Stft::<_, f32>::new(StftSettings::new(128), SpectralFreeze::new()).unwrap();
    let mut shift = Stft::<_, f32>::new(StftSettings::new(256), PitchShift::new(1.0)).unwrap();
    gate.prepare(&SPEC);
    freeze.prepare(&SPEC);
    shift.prepare(&SPEC);
    let mut buffer = AudioBuffer::<f32>::new(SPEC.num_channels, SPEC.max_block_size);
    assert_no_heap_use("spectral effects", || {
        for round in 0..24usize {
            buffer.set_num_frames([256, 1, 77, 200][round % 4]);
            for (index, channel) in buffer.iter_channels_mut().enumerate() {
                for (n, sample) in channel.iter_mut().enumerate() {
                    *sample = 0.5 * ((n * (round + 3 + index)) as f32 * 0.05).sin();
                }
            }
            gate.processor_mut().set_learning(round % 6 < 2);
            gate.processor_mut().set_bin_threshold_db(round, -20.0);
            gate.processor_mut().set_floor_db(-30.0);
            gate.process(&mut buffer);
            freeze.processor_mut().set_frozen(round % 4 >= 2);
            freeze.process(&mut buffer);
            shift.processor_mut().set_semitones(round as f64 - 12.0);
            shift.process(&mut buffer);
        }
        gate.reset();
        freeze.reset();
        shift.reset();
    });
    assert!(all_finite(&buffer));
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

/// Plays a flood of notes on a synth of instrument voices, with accents,
/// slides, bends, tuning, settings changes and notes taken over, and fails
/// if anything touches the heap.
fn flood<V: Voice>(what: &str, mut synth: Synth<V>, mut change: impl FnMut(&mut Synth<V>, u8)) {
    synth.prepare(&SPEC);
    let mut events = EventBuffer::with_capacity(64);
    let mut buffer = AudioBuffer::<f32>::new(2, 256);
    let transport = Transport::playing_at(120.0);
    assert_no_heap_use(what, || {
        for round in 0..24u8 {
            synth.set_legato(round % 3 == 0);
            change(&mut synth, round);
            // Accented notes go straight to the synth, as a sequencer
            // would send them.
            synth.note_on_accented(NoteRef::new(9, 36 + round % 40), 0.9, round % 2 == 0);
            events.clear();
            for step in 0..6u8 {
                let note = NoteRef::new(9, 30 + (round * 7 + step * 5) % 60);
                let offset = u32::from(step) * 40;
                let _ = events.push(Event {
                    offset,
                    kind: EventKind::NoteOn {
                        note,
                        velocity: 0.3 + f32::from(step) * 0.1,
                    },
                });
                let _ = events.push(Event {
                    offset: offset + 20,
                    kind: EventKind::NoteExpression {
                        note,
                        expression: Expression::Tuning,
                        value: 0.3,
                    },
                });
                let _ = events.push(Event {
                    offset: offset + 25,
                    kind: EventKind::Midi(manguebyte_midi::MidiMessage::PitchBend {
                        channel: 9,
                        value: i16::from(step) * 1_000 - 2_000,
                    }),
                });
                if step % 2 == 1 {
                    let _ = events.push(Event {
                        offset: offset + 30,
                        kind: EventKind::NoteOff {
                            note,
                            velocity: 0.5,
                        },
                    });
                }
            }
            buffer.set_num_frames([256, 64, 1, 200][usize::from(round) % 4]);
            synth.process_with(&mut buffer, &ProcessContext::new(&events, &transport));
        }
        synth.reset();
        synth.process(&mut buffer);
    });
    assert!(all_finite(&buffer));
}

/// Shapes every note of `synth` the way a player of an acoustic instrument
/// does while it sounds: pressure, brightness, vibrato, volume, and the
/// breath controller, the expression pedal and the modulation wheel.
fn shape_notes<V: Voice>(synth: &mut Synth<V>, round: u8) {
    let amount = f64::from(round % 8) / 7.0;
    for voice in synth.voices_mut() {
        for expression in [
            Expression::Pressure,
            Expression::Brightness,
            Expression::Vibrato,
            Expression::Volume,
        ] {
            voice.expression(expression, amount);
        }
        for controller in [1, 2, 11] {
            voice.midi(manguebyte_midi::MidiMessage::ControlChange {
                channel: 9,
                controller,
                value: 127 - round * 5,
            });
        }
    }
}

#[test]
fn the_modelled_acoustic_instruments_do_not_use_the_heap() {
    use manguebyte_dsp::instruments::{
        BowedParams, BowedString, ChoirParams, ChoirVoice, MalletParams, MalletVoice, WindParams,
        WindVoice,
    };

    let mallets = [
        MalletParams::marimba(),
        MalletParams::xylophone(),
        MalletParams::vibraphone(),
        MalletParams::glockenspiel(),
        MalletParams::celesta(),
        MalletParams::kalimba(),
        MalletParams::steel_pan(),
        MalletParams::tubular_bells(),
    ];
    flood(
        "MalletVoice",
        Synth::with_voices(4, || MalletVoice::new(MalletParams::marimba())),
        |synth, round| {
            let params = mallets[usize::from(round) % mallets.len()];
            synth
                .voices_mut()
                .for_each(|voice| voice.set_params(params));
            shape_notes(synth, round);
        },
    );

    let strings = [
        BowedParams::violin(),
        BowedParams::viola(),
        BowedParams::cello(),
        BowedParams::contrabass(),
        BowedParams::string_ensemble(),
        BowedParams::violin_pizzicato(),
        BowedParams::cello_pizzicato(),
        BowedParams::contrabass_pizzicato(),
    ];
    flood(
        "BowedString",
        Synth::with_voices(3, || BowedString::new(BowedParams::violin())),
        |synth, round| {
            let params = strings[usize::from(round) % strings.len()];
            synth
                .voices_mut()
                .for_each(|voice| voice.set_params(params));
            shape_notes(synth, round);
        },
    );

    let winds = [
        WindParams::flute(),
        WindParams::clarinet(),
        WindParams::saxophone(),
        WindParams::trumpet(),
        WindParams::trombone(),
    ];
    flood(
        "WindVoice",
        Synth::with_voices(3, || WindVoice::new(WindParams::clarinet())),
        |synth, round| {
            let params = winds[usize::from(round) % winds.len()];
            synth
                .voices_mut()
                .for_each(|voice| voice.set_params(params));
            shape_notes(synth, round);
        },
    );

    let choirs = [
        ChoirParams::aah_choir(),
        ChoirParams::ooh_choir(),
        ChoirParams::solo_voice(),
    ];
    flood(
        "ChoirVoice",
        Synth::with_voices(3, || ChoirVoice::new(ChoirParams::aah_choir())),
        |synth, round| {
            let params = choirs[usize::from(round) % choirs.len()];
            synth
                .voices_mut()
                .for_each(|voice| voice.set_params(params));
            shape_notes(synth, round);
        },
    );
}

#[test]
fn the_instruments_do_not_use_the_heap() {
    use manguebyte_dsp::instruments::{
        AcidParams, AcidVoice, AnalogKitParams, AnalogKitVoice, DrumKit, DrumKitVoice, FmParams,
        FmVoice, KickBassParams, KickBassVoice, OrganParams, OrganVoice, PluckedString,
        RotaryParams, RotarySpeaker, RotarySpeed, StringParams, percussion,
    };

    let strings = [
        StringParams::nylon_guitar(),
        StringParams::steel_guitar(),
        StringParams::finger_bass(),
        StringParams::slap_bass(),
    ];
    flood(
        "PluckedString",
        Synth::with_voices(3, || PluckedString::new(StringParams::nylon_guitar())),
        |synth, round| {
            let params = strings[usize::from(round) % strings.len()];
            synth
                .voices_mut()
                .for_each(|voice| voice.set_params(params));
        },
    );
    let fm = [
        FmParams::electric_piano(),
        FmParams::tubular_bell(),
        FmParams::fm_bass(),
    ];
    flood(
        "FmVoice",
        Synth::with_voices(3, || FmVoice::new(FmParams::electric_piano())),
        |synth, round| {
            let params = fm[usize::from(round) % fm.len()];
            synth
                .voices_mut()
                .for_each(|voice| voice.set_params(params));
        },
    );
    let organs = [
        OrganParams::jazz(),
        OrganParams::full(),
        OrganParams::ballad(),
    ];
    flood(
        "OrganVoice",
        Synth::new(OrganVoice::voices(4, OrganParams::jazz())),
        |synth, round| {
            let params = organs[usize::from(round) % organs.len()];
            synth
                .voices_mut()
                .for_each(|voice| voice.set_params(params));
        },
    );
    flood(
        "AnalogKitVoice",
        Synth::new(AnalogKitVoice::voices(4, AnalogKitParams::default())),
        |synth, round| {
            let mut params = AnalogKitParams::default();
            params.drums[usize::from(round) % 16].decay = 2.0;
            synth
                .voices_mut()
                .for_each(|voice| voice.set_params(params));
        },
    );
    let kits = [
        DrumKit::acoustic(),
        DrumKit::techno(),
        DrumKit::lo_fi(),
        DrumKit::style_cr78(),
        DrumKit::trap(),
    ];
    flood(
        "DrumKitVoice",
        Synth::new(DrumKitVoice::voices(6, DrumKit::acoustic())),
        |synth, round| {
            let kit = kits[usize::from(round) % kits.len()];
            synth.voices_mut().for_each(|voice| voice.set_params(kit));
        },
    );
    // The Brazilian percussion, every stroke of it struck again and again
    // over the rounds, muted strokes choking open ones.
    let brazilian = [
        DrumKit::brazilian(),
        DrumKit::lo_fi().with_brazilian_percussion(1.0),
    ];
    flood(
        "DrumKitVoice, Brazilian percussion",
        Synth::new(DrumKitVoice::voices(6, DrumKit::brazilian())),
        |synth, round| {
            let kit = brazilian[usize::from(round) % brazilian.len()];
            synth.voices_mut().for_each(|voice| voice.set_params(kit));
            for step in 0..4 {
                let key = percussion::ALL[(usize::from(round) * 4 + step) % percussion::ALL.len()];
                synth.note_on_accented(NoteRef::new(9, key), 0.6, step % 2 == 0);
            }
        },
    );
    flood(
        "KickBassVoice",
        Synth::with_voices(1, || KickBassVoice::new(KickBassParams::default())),
        |synth, round| {
            let params = KickBassParams {
                drive: f64::from(round) / 24.0,
                ..KickBassParams::default()
            };
            synth
                .voices_mut()
                .for_each(|voice| voice.set_params(params));
        },
    );
    let acid = [
        AcidParams::classic(),
        AcidParams::squelch(),
        AcidParams::square(),
    ];
    flood(
        "AcidVoice",
        Synth::with_voices(1, || AcidVoice::new(AcidParams::classic())),
        |synth, round| {
            let params = acid[usize::from(round) % acid.len()];
            synth
                .voices_mut()
                .for_each(|voice| voice.set_params(params));
        },
    );

    check(
        "RotarySpeaker",
        RotarySpeaker::<f32>::new(RotaryParams::default()),
    );
    let mut leslie = RotarySpeaker::<f32>::new(RotaryParams::default());
    leslie.prepare(&SPEC);
    let mut buffer = AudioBuffer::<f32>::new(2, 256);
    assert_no_heap_use("RotarySpeaker, changing speed", || {
        for (round, speed) in [RotarySpeed::Fast, RotarySpeed::Stop, RotarySpeed::Slow]
            .into_iter()
            .cycle()
            .take(9)
            .enumerate()
        {
            leslie.set_speed(speed);
            leslie.set_params(RotaryParams {
                crossover: 600.0 + 100.0 * round as f64,
                ..RotaryParams::default()
            });
            buffer.channel_mut(0).fill(0.25);
            leslie.process(&mut buffer);
        }
    });
    assert!(all_finite(&buffer));
}

#[test]
fn the_poly_synth_does_not_use_the_heap() {
    use manguebyte_dsp::instruments::poly::{self, PolySynthParams, PolySynthVoice, VoiceMode};

    // Every preset in turn, on the voices of a poly synth and of a legato
    // one, switched while notes sound: shapes, filter models, unison counts
    // and modes all change under the notes.
    let presets = &poly::PRESETS;
    for mode in [VoiceMode::Poly, VoiceMode::Legato] {
        let first = PolySynthParams {
            mode,
            ..presets[0].params
        };
        flood(
            "PolySynthVoice",
            PolySynthVoice::synth(&first, 4),
            |synth, round| {
                let params = presets[usize::from(round) % presets.len()].params;
                synth.voices_mut().for_each(|voice| {
                    voice.set_params(params);
                    voice.set_tempo(90.0 + f64::from(round));
                });
            },
        );
    }
}

#[test]
fn reading_mpe_and_midi_2_does_not_use_the_heap() {
    use manguebyte_dsp::note_input::{Mpe, NoteInput};
    use manguebyte_midi::MidiMessage;
    use manguebyte_midi::mpe::{Configuration, ZoneKind};
    use manguebyte_midi::ump::{ChannelVoice2, Message, NoteAttribute};

    let mut synth = Synth::with_voices(4, || ToneVoice {
        oscillator: Oscillator::new(Waveform::Saw, 220.0),
        envelope: Adsr::new(AdsrParams::default()),
    });
    synth.prepare(&SPEC);
    // A small capacity, so that notes are also pushed out.
    let mut input = NoteInput::with_capacity(Mpe::Follow, 8);
    let mut events = EventBuffer::with_capacity(1_024);
    let mut buffer = AudioBuffer::<f32>::new(2, 256);
    let transport = Transport::playing_at(120.0);
    let configure = Configuration {
        zone: ZoneKind::Lower,
        members: 15,
    }
    .messages();
    assert_no_heap_use("NoteInput", || {
        for round in 0..32u8 {
            events.clear();
            let mut push = |event| {
                let _ = events.push(event);
            };
            if round % 8 == 0 {
                // Zones come and go, releasing what they hold.
                let members = if round % 16 == 0 { 15 } else { 0 };
                for &message in configure.as_slice() {
                    let message = match message {
                        MidiMessage::ControlChange {
                            controller: 6,
                            channel,
                            ..
                        } => MidiMessage::ControlChange {
                            channel,
                            controller: 6,
                            value: members,
                        },
                        other => other,
                    };
                    input.midi1(0, message, &mut push);
                }
            }
            for step in 0..12u8 {
                let channel = 1 + step % 15;
                let key = 40 + (round + step) % 24;
                let offset = u32::from(step) * 20;
                input.midi1(
                    offset,
                    MidiMessage::PitchBend {
                        channel,
                        value: 300 * i16::from(step),
                    },
                    &mut push,
                );
                input.midi1(
                    offset,
                    MidiMessage::NoteOn {
                        channel,
                        note: key,
                        velocity: 90,
                    },
                    &mut push,
                );
                input.midi1(
                    offset + 5,
                    MidiMessage::ChannelPressure {
                        channel,
                        pressure: step * 9,
                    },
                    &mut push,
                );
                input.midi1(
                    offset + 6,
                    MidiMessage::ControlChange {
                        channel,
                        controller: 74,
                        value: step * 10,
                    },
                    &mut push,
                );
                let two = Message::Midi2 {
                    group: 0,
                    message: ChannelVoice2::NoteOn {
                        channel: 0,
                        note: key,
                        velocity: 40_000,
                        attribute: NoteAttribute::pitch(f32::from(key) + 0.5),
                    },
                }
                .to_packet();
                input.packet(offset + 7, &two, &mut push);
                input.midi2(
                    offset + 8,
                    ChannelVoice2::PerNotePitchBend {
                        channel: 0,
                        note: key,
                        value: 0x9000_0000,
                    },
                    &mut push,
                );
                input.midi2(
                    offset + 9,
                    ChannelVoice2::PerNoteManagement {
                        channel: 0,
                        note: key,
                        detach: step % 2 == 0,
                        reset: step % 3 == 0,
                    },
                    &mut push,
                );
                input.midi2(
                    offset + 10,
                    ChannelVoice2::RegisteredPerNoteController {
                        channel: 0,
                        note: key,
                        controller: 74,
                        value: u32::from(step) << 28,
                    },
                    &mut push,
                );
                input.midi1(
                    offset + 15,
                    MidiMessage::NoteOff {
                        channel,
                        note: key,
                        velocity: 0,
                    },
                    &mut push,
                );
            }
            synth.process_with(&mut buffer, &ProcessContext::new(&events, &transport));
        }
    });
    assert!(input.held_notes() <= 8);
}

#[test]
fn pulling_from_sources_does_not_use_the_heap() {
    use manguebyte_core::source::{AudioSource, BufferSource, PositionableSource};
    use manguebyte_dsp::sources::{
        ChannelRemapSource, MixerSource, ProcessedSource, ResamplingSource, SourceProcessor,
    };

    let mut loop_a = AudioBuffer::<f32>::new(1, 1_000);
    loop_a.channel_mut(0).fill(0.25);
    let mut loop_b = AudioBuffer::<f32>::new(2, 333);
    loop_b.channel_mut(1).fill(-0.5);
    // A mono loop at 44.1 kHz on both channels, a stereo one at 96 kHz
    // through a filter, mixed.
    let mono = ResamplingSource::new(BufferSource::new(loop_a).looping(true), 44_100.0, 1);
    let both = ChannelRemapSource::new(mono, 1, &[Some(0), Some(0)]);
    let filtered = ProcessedSource::new(
        ResamplingSource::new(BufferSource::new(loop_b).looping(true), 96_000.0, 2),
        Filter::<f32>::new(FilterShape::Lowpass, 2_000.0, 0.707),
        2,
    );
    let mut mixer = MixerSource::new(2);
    mixer.add(both, 1.0);
    mixer.add(filtered, 0.5);
    let mut player = SourceProcessor::new(mixer);
    player.prepare(&SPEC);
    let mut buffer = AudioBuffer::<f32>::new(2, 256);
    let mut memory = BufferSource::new(AudioBuffer::<f32>::new(2, 10));
    assert_no_heap_use("sources", || {
        for round in 0..20 {
            for frames in [256, 1, 64, 255, 0, 17] {
                buffer.set_num_frames(frames);
                player.process(&mut buffer);
            }
            let mixer = player.source_mut();
            mixer.set_gain(1, 0.1 * round as f32);
            if let Some(input) = mixer.input_mut(0) {
                input.reset();
            }
        }
        memory.seek(5);
        memory.pull(&mut buffer);
    });
    assert!(buffer.peak().is_finite());
}

#[test]
fn advancing_a_transport_clock_does_not_use_the_heap() {
    use manguebyte_dsp::timeline::{TempoCurve, TempoMap, TimeSignature, TransportClock};

    let mut map = TempoMap::new(120.0, TimeSignature::FOUR_FOUR).unwrap();
    map.set_tempo(2.0, 120.0, TempoCurve::Linear).unwrap();
    map.set_tempo(6.0, 90.0, TempoCurve::Step).unwrap();
    map.set_signature(1, TimeSignature::new(7, 8)).unwrap();
    let (mut clock, control) = TransportClock::with_control(map.clone(), 48_000.0);
    let mut faster = map.clone();
    faster.set_tempo(0.0, 140.0, TempoCurve::Step).unwrap();
    // Commands and maps are sent off the audio thread, which allocates.
    control.play();
    control.set_loop_beats(Some((1.0, 3.5)));
    control.set_tempo_map(faster).unwrap();
    let events = EventBuffer::with_capacity(1);

    let mut counted = 0usize;
    assert_no_heap_use("transport clock", || {
        for round in 0..40 {
            for frames in [256, 1, 64, 255, 0, 4_096, 17] {
                let block = clock.next_block(frames);
                let timeline = block.with_map(clock.map());
                let transport = timeline.transport();
                let context =
                    ProcessContext::<f32>::new(&events, &transport).with_timeline(timeline);
                counted += block.pieces().count() + block.wraps().count();
                counted += timeline.changes().count();
                counted += timeline.offsets_of_beat(2.0).count();
                if let Some(timeline) = context.timeline {
                    let last = frames.saturating_sub(1);
                    counted += (timeline.beat_at(last) + timeline.tempo_at(last)) as usize;
                    counted += timeline.position_at(last / 2).bar as usize;
                }
                let _ = map.bbt_at_beat(transport.position_beats.unwrap(), 960);
            }
            // Working the clock on the audio thread itself.
            match round % 4 {
                0 => clock.seek_beat(0.5),
                1 => clock.set_loop_frames(Some((1_000, 5_000))),
                2 => clock.set_loop_beats(Some((1.0, 3.5))),
                _ => clock.set_sample_rate(if round % 8 == 3 { 96_000.0 } else { 48_000.0 }),
            }
        }
    });
    assert!(counted > 0);
    // The map the new one replaced came back to be freed here.
    assert_eq!(control.collect(), 1);
}

#[test]
fn a_partial_bank_does_not_use_the_heap() {
    use manguebyte_core::vector::Backend;
    use manguebyte_dsp::{Partial, PartialBank};

    let mut bank = PartialBank::new(3_000);
    bank.set_sample_rate(48_000.0);
    bank.set_len(3_000);
    let (mut left, mut right) = (vec![0.0f32; 4_096], vec![0.0f32; 4_096]);
    let backends: Vec<Backend> = Backend::available().collect();
    assert_no_heap_use("partial bank", || {
        for (round, &backend) in backends.iter().cycle().take(8).enumerate() {
            bank.set_backend(backend);
            for i in 0..bank.len() {
                bank.set(
                    i,
                    Partial {
                        freq_hz: 30.0 * (i % 700 + 1) as f64,
                        gain: 1e-3,
                        pan: (i % 3) as f32 - 1.0,
                    },
                );
                bank.ramp_to(i, 2e-3, 64 + i % 7);
                bank.set_rotation(i, 0.999, 0.0447);
            }
            for frames in [64, 1, 4_096, 255, 0, 17] {
                bank.render_add(&mut left[..frames], &mut right[..frames]);
            }
            bank.set_len(if round % 2 == 0 { 17 } else { 3_000 });
        }
    });
    assert!(left.iter().all(|x| x.is_finite()));
}

/// The effects a synth workstation needs, at their heaviest settings.
#[test]
fn the_workstation_effects_do_not_use_the_heap() {
    use manguebyte_dsp::{
        AutoPan, Bitcrusher, FrequencyShifter, LfoShape, MultibandCompressor, RingCarrier,
        RingModulator, StereoWidener, TapeSaturation, TransientShaper, Tremolo, Vocoder,
    };

    check(
        "Bitcrusher",
        Bitcrusher::<f32>::new()
            .with_bits(3.5)
            .with_downsample(5.5)
            .with_dither(true)
            .with_anti_alias(true)
            .with_mix(0.7),
    );
    check(
        "Tremolo",
        Tremolo::<f32>::new()
            .with_shape(LfoShape::SampleAndHold)
            .with_stereo_phase(0.5),
    );
    check(
        "AutoPan",
        AutoPan::<f32>::new()
            .with_mode(PanMode::MonoToStereo)
            .with_shape(LfoShape::Square),
    );

    check(
        "RingModulator",
        RingModulator::<f32>::new()
            .with_frequency_hz(777.0)
            .with_stereo_phase(0.25)
            .with_mix(0.6),
    );

    check(
        "FrequencyShifter",
        FrequencyShifter::<f32>::new()
            .with_shift_hz(-170.0)
            .with_feedback(0.9)
            .with_mix(0.8),
    );
    let mut shifter = FrequencyShifter::<f32>::new();
    shifter.prepare(&SPEC);
    assert_no_heap_use("FrequencyShifter by the sample", || {
        for n in 0..512 {
            let x = (n as f32 * 0.01).sin();
            let _ = shifter.process_sample(0, x, false);
            let _ = shifter.process_sample(1, x, true);
        }
    });

    check(
        "StereoWidener",
        StereoWidener::<f32>::new()
            .with_width(2.0)
            .with_bass_mono(true)
            .with_bass_mono_hz(150.0),
    );

    check(
        "TransientShaper",
        TransientShaper::<f32>::new()
            .with_attack_db(18.0)
            .with_sustain_db(-12.0),
    );

    check("MultibandCompressor", {
        let mut compressor = MultibandCompressor::<f32>::new(150.0, 3_000.0);
        for band in 0..3 {
            compressor.band_mut(band).set_threshold_db(-30.0);
            compressor.band_mut(band).set_knee_db(6.0);
        }
        compressor
    });

    check(
        "TapeSaturation",
        TapeSaturation::<f32>::new()
            .with_drive_db(30.0)
            .with_wow_cents(30.0)
            .with_flutter_cents(10.0)
            .with_drift(1.0)
            .with_mix(0.5),
    );

    // Locked to a transport that moves on, block by block.
    let mut tremolo = Tremolo::<f32>::new().with_sync_beats(Some(0.5));
    let mut pan = AutoPan::<f32>::new().with_sync_beats(Some(2.0));
    tremolo.prepare(&SPEC);
    pan.prepare(&SPEC);
    let events = EventBuffer::with_capacity(1);
    let mut transport = Transport::playing_at(128.0);
    let mut buffer = AudioBuffer::<f32>::new(2, 256);
    assert_no_heap_use("tempo-synced tremolo and auto-pan", || {
        for frames in [256, 1, 64, 255] {
            buffer.set_num_frames(frames);
            let context = ProcessContext::new(&events, &transport);
            tremolo.process_with(&mut buffer, &context);
            pan.process_with(&mut buffer, &context);
            transport.advance(frames, SPEC.sample_rate);
        }
    });

    // Vocoded by a sidechain, at the most bands.
    let mut vocoder = Vocoder::<f32>::new()
        .with_bands(32)
        .with_formant_shift(-5.0);
    vocoder.prepare(&SPEC);
    let mut voice = AudioBuffer::<f32>::new(1, 256);
    voice.channel_mut(0).fill(0.3);
    let voiced = ProcessContext::new(&events, &transport).with_sidechain(&voice);
    assert_no_heap_use("Vocoder", || {
        for frames in [256, 1, 100] {
            buffer.set_num_frames(frames);
            vocoder.process_with(&mut buffer, &voiced);
            vocoder.process_with(&mut buffer, &voiced.for_piece(50));
            vocoder.process(&mut buffer);
        }
        vocoder.reset();
    });

    // Multiplied by a sidechain, in pieces.
    let mut key = AudioBuffer::<f32>::new(1, 256);
    key.channel_mut(0).fill(0.5);
    let context = ProcessContext::new(&events, &transport).with_sidechain(&key);
    let mut ring = RingModulator::<f32>::new().with_carrier(RingCarrier::Sidechain);
    ring.prepare(&SPEC);
    assert_no_heap_use("ring modulation by a sidechain", || {
        for frames in [256, 1, 100] {
            buffer.set_num_frames(frames);
            ring.process_with(&mut buffer, &context);
            split_at_events_with(
                &mut ring,
                &mut buffer,
                &context,
                |_, _| {},
                |ring, piece, piece_context| ring.process_with(piece, piece_context),
            );
        }
    });
}

#[test]
fn the_workstation_effects_can_change_settings_while_running_without_using_the_heap() {
    use manguebyte_dsp::{
        AutoPan, Bitcrusher, FrequencyShifter, LfoShape, MultibandCompressor, RingModulator,
        StereoWidener, TapeSaturation, TransientShaper, Tremolo, Vocoder,
    };

    let mut crusher = Bitcrusher::<f32>::new();
    crusher.prepare(&SPEC);
    let mut tremolo = Tremolo::<f32>::new();
    tremolo.prepare(&SPEC);
    let mut pan = AutoPan::<f32>::new();
    pan.prepare(&SPEC);
    let mut ring = RingModulator::<f32>::new();
    ring.prepare(&SPEC);
    let mut shifter = FrequencyShifter::<f32>::new();
    shifter.prepare(&SPEC);
    let mut widener = StereoWidener::<f32>::new();
    widener.prepare(&SPEC);
    let mut shaper = TransientShaper::<f32>::new();
    shaper.prepare(&SPEC);
    let mut multiband = MultibandCompressor::<f32>::new(200.0, 2_000.0);
    multiband.prepare(&SPEC);
    let mut tape = TapeSaturation::<f32>::new();
    tape.prepare(&SPEC);
    let mut vocoder = Vocoder::<f32>::new();
    vocoder.prepare(&SPEC);
    let shapes = [
        LfoShape::Sine,
        LfoShape::Triangle,
        LfoShape::SawUp,
        LfoShape::SawDown,
        LfoShape::Square,
        LfoShape::SampleAndHold,
    ];
    let mut buffer = AudioBuffer::<f32>::new(2, 256);
    assert_no_heap_use("changing workstation effect settings", || {
        for step in 0..14u8 {
            let turn = f64::from(step);
            for channel in buffer.iter_channels_mut() {
                channel.fill(0.25);
            }
            crusher.set_bits(1.0 + 2.0 * turn);
            crusher.set_downsample(1.0 + 4.5 * turn);
            crusher.set_dither(step % 2 == 0);
            crusher.set_anti_alias(step % 3 != 0);
            crusher.set_mix(turn / 14.0);
            crusher.process(&mut buffer);

            let shape = shapes[usize::from(step) % shapes.len()];
            tremolo.set_shape(shape);
            tremolo.set_rate_hz(0.5 + turn);
            tremolo.set_depth(turn / 14.0);
            tremolo.set_stereo_phase(1.0 - turn / 14.0);
            tremolo.set_smoothing_ms(turn);
            tremolo.set_sync_beats((step % 2 == 0).then_some(0.25 * turn));
            tremolo.process(&mut buffer);
            pan.set_shape(shape);
            pan.set_rate_hz(0.5 + turn);
            pan.set_width(turn / 14.0);
            pan.set_law(MixLaw::ALL[usize::from(step) % MixLaw::ALL.len()]);
            pan.set_mode(if step % 2 == 0 {
                PanMode::MonoToStereo
            } else {
                PanMode::StereoBalance
            });
            pan.set_smoothing_ms(turn);
            pan.process(&mut buffer);

            ring.set_frequency_hz(100.0 * turn);
            ring.set_mix(turn / 14.0);
            ring.set_stereo_phase(turn / 14.0);
            ring.process(&mut buffer);

            shifter.set_shift_hz(300.0 * turn - 2_000.0);
            shifter.set_feedback(0.9 - turn / 7.0);
            shifter.set_mix(turn / 14.0);
            shifter.process(&mut buffer);

            widener.set_width(turn / 7.0);
            widener.set_bass_mono(step % 3 != 1);
            widener.set_bass_mono_hz(40.0 + 60.0 * turn);
            widener.process(&mut buffer);

            shaper.set_attack_db(3.0 * turn - 20.0);
            shaper.set_sustain_db(20.0 - 3.0 * turn);
            shaper.set_attack_ms(1.0 + 10.0 * turn);
            shaper.set_sustain_ms(50.0 + 200.0 * turn);
            shaper.process(&mut buffer);

            multiband.set_crossover_hz(0, 50.0 + 40.0 * turn);
            multiband.set_crossover_hz(1, 1_000.0 + 500.0 * turn);
            for band in 0..3 {
                let compressor = multiband.band_mut(band);
                compressor.set_threshold_db(-3.0 * turn);
                compressor.set_ratio(1.0 + turn);
                compressor.set_attack_ms(turn);
            }
            multiband.process(&mut buffer);

            tape.set_drive_db(2.0 * turn);
            tape.set_asymmetry(turn / 28.0);
            tape.set_wow_cents(2.0 * turn);
            tape.set_wow_hz(0.5 + turn / 4.0);
            tape.set_flutter_cents(turn / 1.4);
            tape.set_flutter_hz(2.0 + turn);
            tape.set_drift(turn / 14.0);
            tape.set_mix(1.0 - turn / 14.0);
            tape.process(&mut buffer);

            vocoder.set_bands(8 + 2 * usize::from(step));
            vocoder.set_range_hz(50.0 + 10.0 * turn, 4_000.0 + 800.0 * turn);
            vocoder.set_formant_shift(turn - 7.0);
            vocoder.set_attack_ms(turn);
            vocoder.set_release_ms(5.0 * turn);
            vocoder.set_gain_db(turn);
            vocoder.set_mix(turn / 14.0);
            vocoder.process(&mut buffer);
        }
    });
    assert!(all_finite(&buffer));
}
