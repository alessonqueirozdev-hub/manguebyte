//! Signal processing building blocks.
//!
//! Everything here implements [`Processor`], and a tuple of processors is
//! itself a processor that runs its members in order. The chain is resolved at
//! compile time, so it costs nothing over calling each stage by hand:
//!
//! ```
//! use manguebyte_core::AudioBuffer;
//! use manguebyte_dsp::{Filter, FilterShape, Gain, Oscillator, ProcessSpec, Processor, Waveform};
//!
//! let mut voice = (
//!     Oscillator::new(Waveform::Saw, 110.0),
//!     Filter::<f32>::new(FilterShape::Lowpass, 800.0, 0.707),
//!     Gain::from_db(-12.0),
//! );
//!
//! let spec = ProcessSpec { sample_rate: 48_000.0, max_block_size: 256, num_channels: 2 };
//! voice.prepare(&spec);
//!
//! let mut block = AudioBuffer::<f32>::new(2, 256);
//! voice.process(&mut block);
//! assert!(block.peak() > 0.0);
//! ```

#![forbid(unsafe_code)]

mod biquad;
mod bitcrusher;
mod cascade;
mod chorus;
mod context;
pub mod convolution;
mod crossover;
pub mod curve;
mod cutoff;
mod delay;
mod dynamics;
mod elliptic;
mod envelope;
pub mod event;
pub mod fft;
pub mod fir;
mod fir_filter;
mod follower;
mod gain;
pub mod iir;
pub mod instruments;
mod ladder;
mod limiter;
mod mix;
mod modulation;
pub mod note_input;
mod numeric;
mod onepole;
mod oscillator;
mod oversample;
mod pan;
mod partial_bank;
mod phaser;
mod resample;
mod reverb;
mod ring;
mod shaper;
mod shifter;
pub mod sources;
pub mod stft;
mod svf;
mod synth;
#[cfg(test)]
mod testing;
pub mod timeline;
mod transient;
mod tremolo;
pub mod wavetable;
mod widener;
pub mod window;

pub use biquad::{Biquad, BiquadCoefficients, Filter, FilterShape};
pub use bitcrusher::Bitcrusher;
pub use cascade::{BiquadCascade, IirFilter};
pub use chorus::Chorus;
pub use context::{ProcessContext, Transport, split_at_events, split_at_events_with};
pub use convolution::{
    Convolution, ConvolutionEngine, ConvolutionError, ConvolutionLoader, ConvolutionRouting,
    ConvolutionSettings,
};
pub use crossover::{Crossover, CrossoverSlope};
pub use delay::{Delay, DelayInterpolation, DelayLine};
pub use dynamics::{Compressor, Expander, KeySource, NoiseGate};
pub use envelope::{Adsr, AdsrParams};
pub use fir_filter::{FirFilter, FirMode};
pub use follower::{EnvelopeFollower, LevelDetector};
pub use gain::Gain;
pub use ladder::{LadderFilter, LadderMode};
pub use limiter::Limiter;
pub use mix::{DryWet, MixLaw};
pub use modulation::{Lfo, LfoShape, Noise, NoiseColor};
pub use onepole::{OnePole, OnePoleMode};
pub use oscillator::{Oscillator, Waveform};
pub use oversample::Oversampled;
pub use pan::{PanMode, Panner};
pub use partial_bank::{Partial, PartialBank};
pub use phaser::Phaser;
pub use resample::{Resampler, resample};
pub use reverb::Reverb;
pub use ring::{RingCarrier, RingModulator};
pub use shaper::{DcBlocker, ShaperCurve, Waveshaper};
pub use shifter::FrequencyShifter;
pub use stft::{SpectralProcessor, Stft, StftError, StftSettings};
pub use svf::{StateVariableFilter, SvfMode};
pub use synth::{Synth, Voice};
pub use transient::TransientShaper;
pub use tremolo::{AutoPan, Tremolo};
pub use wavetable::{Wavetable, WavetableError, WavetableOscillator};
pub use widener::StereoWidener;

use manguebyte_core::{AudioBuffer, Sample};

/// The conditions a processor will run under.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcessSpec {
    /// Sample rate in hertz.
    pub sample_rate: f64,
    /// Largest block [`Processor::process`] will be handed.
    pub max_block_size: usize,
    /// Number of channels in the blocks to come.
    pub num_channels: usize,
}

/// A unit that transforms audio in place, one block at a time.
///
/// # Real-time contract
///
/// [`prepare`](Self::prepare) may allocate and take its time.
/// [`process`](Self::process) and [`reset`](Self::reset) run on the audio
/// thread: they must not allocate, lock, or do I/O.
pub trait Processor<S: Sample = f32> {
    /// Gets ready to process under `spec`. Called before the first block and
    /// again whenever the sample rate, block size or channel count changes.
    fn prepare(&mut self, spec: &ProcessSpec);

    /// Processes one block in place.
    fn process(&mut self, buffer: &mut AudioBuffer<S>);

    /// Processes one block in place, knowing what happens during it: notes,
    /// parameter changes and modulation with the frame each one falls on,
    /// the transport position, and a sidechain input.
    ///
    /// The default ignores all of that and calls
    /// [`process`](Self::process), which is right for a processor that only
    /// transforms audio. Instruments and anything that follows tempo or
    /// automation override it; [`split_at_events`] does the work of applying
    /// each event on its exact frame.
    fn process_with(&mut self, buffer: &mut AudioBuffer<S>, context: &ProcessContext<'_, S>) {
        let _ = context;
        self.process(buffer);
    }

    /// Clears internal state such as filter memory and delay tails, without
    /// touching parameters.
    fn reset(&mut self) {}

    /// How many frames late the output is compared with the input, for
    /// example because of look-ahead. A host uses this to keep tracks
    /// aligned. A chain reports the sum of its stages.
    fn latency_samples(&self) -> usize {
        0
    }
}

impl<S: Sample, P: Processor<S> + ?Sized> Processor<S> for Box<P> {
    fn prepare(&mut self, spec: &ProcessSpec) {
        (**self).prepare(spec);
    }
    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        (**self).process(buffer);
    }
    fn process_with(&mut self, buffer: &mut AudioBuffer<S>, context: &ProcessContext<'_, S>) {
        (**self).process_with(buffer, context);
    }
    fn reset(&mut self) {
        (**self).reset();
    }
    fn latency_samples(&self) -> usize {
        (**self).latency_samples()
    }
}

macro_rules! impl_processor_for_tuple {
    ($($member:ident),+) => {
        #[allow(non_snake_case)]
        impl<S: Sample, $($member: Processor<S>),+> Processor<S> for ($($member,)+) {
            fn prepare(&mut self, spec: &ProcessSpec) {
                let ($($member,)+) = self;
                $($member.prepare(spec);)+
            }
            fn process(&mut self, buffer: &mut AudioBuffer<S>) {
                let ($($member,)+) = self;
                $($member.process(buffer);)+
            }
            fn process_with(
                &mut self,
                buffer: &mut AudioBuffer<S>,
                context: &ProcessContext<'_, S>,
            ) {
                let ($($member,)+) = self;
                $($member.process_with(buffer, context);)+
            }
            fn reset(&mut self) {
                let ($($member,)+) = self;
                $($member.reset();)+
            }
            fn latency_samples(&self) -> usize {
                let ($($member,)+) = self;
                0 $(+ $member.latency_samples())+
            }
        }
    };
}

impl_processor_for_tuple!(A);
impl_processor_for_tuple!(A, B);
impl_processor_for_tuple!(A, B, C);
impl_processor_for_tuple!(A, B, C, D);
impl_processor_for_tuple!(A, B, C, D, E);
impl_processor_for_tuple!(A, B, C, D, E, F);
impl_processor_for_tuple!(A, B, C, D, E, F, G);
impl_processor_for_tuple!(A, B, C, D, E, F, G, H);

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: ProcessSpec = ProcessSpec {
        sample_rate: 48_000.0,
        max_block_size: 64,
        num_channels: 1,
    };

    #[test]
    fn tuple_runs_members_in_order() {
        struct Add(f32);
        struct Mul(f32);
        impl Processor for Add {
            fn prepare(&mut self, _: &ProcessSpec) {}
            fn process(&mut self, buffer: &mut AudioBuffer) {
                buffer.channel_mut(0).iter_mut().for_each(|s| *s += self.0);
            }
        }
        impl Processor for Mul {
            fn prepare(&mut self, _: &ProcessSpec) {}
            fn process(&mut self, buffer: &mut AudioBuffer) {
                buffer.apply_gain(self.0);
            }
        }

        let mut chain = (Add(1.0), Mul(3.0), Add(0.5));
        chain.prepare(&SPEC);
        let mut buffer = AudioBuffer::new(1, 4);
        chain.process(&mut buffer);
        assert_eq!(buffer.channel(0), &[3.5; 4]);
    }

    #[test]
    fn boxed_processors_can_be_chained_dynamically() {
        let mut chain: Vec<Box<dyn Processor>> = vec![
            Box::new(Oscillator::new(Waveform::Sine, 440.0)),
            Box::new(Gain::new(0.5)),
        ];
        let mut buffer = AudioBuffer::new(1, 64);
        for stage in &mut chain {
            stage.prepare(&SPEC);
        }
        for stage in &mut chain {
            stage.process(&mut buffer);
        }
        let peak = buffer.peak();
        assert!(peak > 0.2 && peak <= 0.5, "peak was {peak}");
    }

    #[test]
    fn context_and_latency_pass_through_chains_and_boxes() {
        use crate::event::{Event, EventBuffer, EventKind, NoteRef};

        /// Adds the number of events it was shown, and claims some latency.
        struct CountEvents(usize);
        impl Processor for CountEvents {
            fn prepare(&mut self, _: &ProcessSpec) {}
            fn process(&mut self, _: &mut AudioBuffer) {}
            fn process_with(&mut self, buffer: &mut AudioBuffer, context: &ProcessContext<'_>) {
                let count = context.events.len() as f32;
                buffer.channel_mut(0).iter_mut().for_each(|s| *s += count);
            }
            fn latency_samples(&self) -> usize {
                self.0
            }
        }
        /// Doubles the signal.
        struct Double;
        impl Processor for Double {
            fn prepare(&mut self, _: &ProcessSpec) {}
            fn process(&mut self, buffer: &mut AudioBuffer) {
                buffer.apply_gain(2.0);
            }
        }

        let mut events = EventBuffer::with_capacity(4);
        for offset in [0, 1] {
            let note = NoteRef::new(0, 60);
            let kind = EventKind::NoteOn {
                note,
                velocity: 1.0,
            };
            events.push(Event { offset, kind }).unwrap();
        }
        let transport = Transport::STOPPED;
        let context = ProcessContext::new(&events, &transport);

        let boxed: Box<dyn Processor> = Box::new(CountEvents(7));
        let mut chain = (CountEvents(3), Double, boxed);
        chain.prepare(&SPEC);
        assert_eq!(chain.latency_samples(), 10);

        // (0 + 2 events) doubled, then + 2 events again.
        let mut buffer = AudioBuffer::new(1, 4);
        chain.process_with(&mut buffer, &context);
        assert_eq!(buffer.channel(0), &[6.0; 4]);

        // Without a context, the same processors see no events.
        buffer.clear();
        chain.process(&mut buffer);
        assert_eq!(buffer.peak(), 0.0);
    }
}
