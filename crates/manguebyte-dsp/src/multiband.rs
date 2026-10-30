use manguebyte_core::{AudioBuffer, Sample};

use crate::{Compressor, Crossover, CrossoverSlope, ProcessSpec, Processor};

/// How many bands a [`MultibandCompressor`] has.
const BANDS: usize = 3;

/// A three-band compressor: the signal split into low, mid and high bands
/// by a Linkwitz-Riley [`Crossover`], each band through a [`Compressor`] of
/// its own, and the bands added back up.
///
/// Each band is turned down only by what is loud in it, so a heavy bass
/// note does not duck the cymbals above it, and each can have its own
/// threshold, ratio, times and makeup: reach a band's compressor through
/// [`band`](Self::band) and [`band_mut`](Self::band_mut).
///
/// The crossover is of the fourth order (24 dB per octave). Its bands add
/// up to an all-pass, so while no compressor is reducing gain the output
/// has the input's level at every frequency: measured, flat to within
/// 1e-7 dB. Only the phase turns, around the crossover frequencies.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::{MultibandCompressor, ProcessSpec, Processor};
///
/// let mut compressor = MultibandCompressor::<f32>::new(200.0, 2_000.0);
/// for band in 0..3 {
///     compressor.band_mut(band).set_threshold_db(-20.0);
///     compressor.band_mut(band).set_ratio(4.0);
/// }
/// compressor.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 4_800, num_channels: 1 });
///
/// // A loud 60 Hz tone is turned down in the low band only.
/// let mut block = AudioBuffer::<f32>::new(1, 4_800);
/// for (n, sample) in block.channel_mut(0).iter_mut().enumerate() {
///     *sample = (std::f32::consts::TAU * 60.0 * n as f32 / 48_000.0).sin();
/// }
/// compressor.process(&mut block);
/// assert!(compressor.gain_reduction_db(0) > 10.0);
/// assert_eq!(compressor.gain_reduction_db(2), 0.0);
/// ```
///
/// # Limits
///
/// Each compressor measures its own band, through
/// [`process`](Processor::process): a sidechain in the context of
/// [`process_with`](Processor::process_with) is not split into bands and is
/// not used. There is no look-ahead, and so no latency. The crossover
/// frequencies glide as the [`Crossover`]'s do; a block longer than the
/// compressor was prepared for is handled in pieces.
#[derive(Debug, Clone)]
pub struct MultibandCompressor<S: Sample = f32> {
    crossover: Crossover<S>,
    compressors: [Compressor<S>; BANDS],
}

impl<S: Sample> MultibandCompressor<S> {
    /// The number of bands.
    pub const BANDS: usize = BANDS;

    /// Creates a compressor whose bands meet at `low_hz` and `high_hz`, with
    /// a [`Compressor::new`] on each band.
    pub fn new(low_hz: f64, high_hz: f64) -> Self {
        Self {
            crossover: Crossover::new(CrossoverSlope::Lr4, &[low_hz, high_hz]),
            compressors: [Compressor::new(), Compressor::new(), Compressor::new()],
        }
    }

    /// Moves the frequency where band `index` meets the one above it: 0
    /// for low and mid, 1 for mid and high. It glides there. Real-time
    /// safe. Another `index` is ignored.
    pub fn set_crossover_hz(&mut self, index: usize, hertz: f64) {
        self.crossover.set_frequency(index, hertz);
    }

    /// The frequency where band `index` meets the one above it, after the
    /// crossover's limits.
    pub fn crossover_hz(&self, index: usize) -> Option<f64> {
        self.crossover.frequency(index)
    }

    /// The compressor of band `index`, from 0 for the lowest.
    ///
    /// # Panics
    ///
    /// Panics if `index` is not below [`BANDS`](Self::BANDS).
    pub fn band(&self, index: usize) -> &Compressor<S> {
        &self.compressors[index]
    }

    /// The compressor of band `index`, for its settings. Real-time safe to
    /// change while running.
    ///
    /// # Panics
    ///
    /// Panics if `index` is not below [`BANDS`](Self::BANDS).
    pub fn band_mut(&mut self, index: usize) -> &mut Compressor<S> {
        &mut self.compressors[index]
    }

    /// How much band `index` is being turned down, in decibels; zero for a
    /// band it does not have. For a meter.
    pub fn gain_reduction_db(&self, index: usize) -> f64 {
        self.compressors
            .get(index)
            .map_or(0.0, Compressor::gain_reduction_db)
    }

    /// The crossover, for drawing its bands.
    pub fn crossover(&self) -> &Crossover<S> {
        &self.crossover
    }
}

impl<S: Sample> Processor<S> for MultibandCompressor<S> {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.crossover.prepare(spec);
        for compressor in &mut self.compressors {
            compressor.prepare(spec);
        }
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        let piece = self.crossover.band(0).capacity();
        if self.crossover.band(0).num_channels() == 0 {
            // Not prepared: there are no bands to work in.
            return;
        }
        let frames = buffer.num_frames();
        let mut start = 0;
        while start < frames {
            let end = (start + piece).min(frames);
            buffer.windowed(start..end, |part| {
                self.crossover.split(part);
                for (index, compressor) in self.compressors.iter_mut().enumerate() {
                    compressor.process(self.crossover.band_mut(index));
                }
                self.crossover.sum_into(part);
            });
            start = end;
        }
    }

    fn reset(&mut self) {
        self.crossover.reset();
        for compressor in &mut self.compressors {
            Processor::<S>::reset(compressor);
        }
    }
}

#[cfg(test)]
mod tests {
    use core::f64::consts::TAU;

    use super::*;
    use crate::fft::{Complex, RealFft};
    use crate::numeric::strict_buffer_peak;
    use crate::testing::{db, noise, run_in_blocks};

    const RATE: f64 = 48_000.0;

    fn prepared(compressor: MultibandCompressor<f64>, channels: usize) -> MultibandCompressor<f64> {
        let mut compressor = compressor;
        compressor.prepare(&ProcessSpec {
            sample_rate: RATE,
            max_block_size: 512,
            num_channels: channels,
        });
        compressor
    }

    /// A multiband compressor with every band at -20 dB and 4:1.
    fn squeezing(low_hz: f64, high_hz: f64) -> MultibandCompressor<f64> {
        let mut compressor = MultibandCompressor::new(low_hz, high_hz);
        for band in 0..3 {
            compressor.band_mut(band).set_threshold_db(-20.0);
            compressor.band_mut(band).set_ratio(4.0);
        }
        compressor
    }

    #[test]
    fn idle_bands_sum_flat() {
        // Quiet enough that no compressor does anything: the response is the
        // crossover's all-pass, of level 1 at every frequency.
        const LEN: usize = 1 << 15;
        for (low, high) in [(200.0, 2_000.0), (500.0, 1_000.0), (80.0, 12_000.0)] {
            let mut compressor = prepared(squeezing(low, high), 1);
            let mut impulse = AudioBuffer::<f64>::new(1, LEN);
            impulse.channel_mut(0)[0] = 0.01;
            run_in_blocks(&mut compressor, &mut impulse, 512);
            for band in 0..3 {
                assert_eq!(compressor.gain_reduction_db(band), 0.0);
            }
            let mut fft = RealFft::<f64>::new(LEN);
            let mut bins = vec![Complex::ZERO; fft.spectrum_len()];
            fft.forward(impulse.channel(0), &mut bins);
            let worst = bins
                .iter()
                .map(|bin| db(bin.norm() / 0.01).abs())
                .fold(0.0, f64::max);
            assert!(worst < 1e-7, "{low} and {high} Hz: {worst} dB off flat");
        }
    }

    /// The amplitude at each bin of the last `len` frames.
    fn spectrum(signal: &[f64], len: usize) -> Vec<f64> {
        let mut fft = RealFft::<f64>::new(len);
        let mut bins = vec![Complex::ZERO; fft.spectrum_len()];
        fft.forward(&signal[signal.len() - len..], &mut bins);
        bins.iter()
            .map(|bin| 2.0 * bin.norm() / len as f64)
            .collect()
    }

    #[test]
    fn a_loud_band_is_turned_down_without_touching_the_others() {
        // A full-scale 60 Hz tone and a 10 kHz tone 40 dB down. The bands
        // meet at 250 Hz and 2.5 kHz; the low band's compressor sees the
        // bass 20 dB over its threshold and settles near 15 dB of
        // reduction, while the high band stays below its threshold.
        const LEN: usize = 24_000;
        let mut signal = AudioBuffer::<f64>::new(1, 2 * LEN);
        for (n, sample) in signal.channel_mut(0).iter_mut().enumerate() {
            let t = n as f64 / RATE;
            *sample = (TAU * 60.0 * t).sin() + 0.01 * (TAU * 10_000.0 * t).sin();
        }
        let mut compressor = prepared(
            {
                let mut c = squeezing(250.0, 2_500.0);
                c.band_mut(0).set_release_ms(500.0);
                c
            },
            1,
        );
        run_in_blocks(&mut compressor, &mut signal, 512);
        let low = compressor.gain_reduction_db(0);
        assert!((low - 15.0).abs() < 0.5, "the low band is down {low} dB");
        assert_eq!(compressor.gain_reduction_db(1), 0.0);
        assert_eq!(compressor.gain_reduction_db(2), 0.0);

        let bins = spectrum(signal.channel(0), LEN);
        // Bin k is 2k hertz.
        let bass = db(bins[30]);
        assert!((bass + low).abs() < 0.5, "the bass came out at {bass} dB");
        let treble = db(bins[5_000] / 0.01);
        assert!(treble.abs() < 0.05, "the treble moved by {treble} dB");
    }

    #[test]
    fn output_does_not_depend_on_block_size() {
        let render = |block: usize| {
            let mut compressor = prepared(squeezing(300.0, 3_000.0), 2);
            let mut signal = noise::<f64>(2, 6_000, 3);
            let mut start = 0;
            for (end, hz) in [(2_000, 150.0), (4_000, 800.0), (6_000, 400.0)] {
                signal.windowed(start..end, |part| {
                    run_in_blocks(&mut compressor, part, block)
                });
                compressor.set_crossover_hz(0, hz);
                compressor.band_mut(1).set_ratio(hz / 100.0);
                start = end;
            }
            signal
        };
        let reference = render(512);
        for block in [1, 7, 64, 333, 6_000] {
            assert!(render(block) == reference, "blocks of {block}");
        }
    }

    #[test]
    fn it_stays_bounded_and_survives_garbage() {
        let mut compressor = prepared(squeezing(200.0, 2_000.0), 2);
        for garbage in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 1e300] {
            compressor.set_crossover_hz(0, garbage);
            compressor.set_crossover_hz(1, garbage);
            compressor.set_crossover_hz(7, garbage);
            let mut signal = noise::<f64>(2, 4_800, 2);
            run_in_blocks(&mut compressor, &mut signal, 512);
            let peak = strict_buffer_peak(&signal);
            assert!(peak < 3.0, "after {garbage}: {peak}");
        }
        assert_eq!(compressor.gain_reduction_db(9), 0.0);
        assert!(compressor.crossover_hz(0).is_some() && compressor.crossover_hz(2).is_none());
    }

    #[test]
    fn reset_clears_the_bands_and_the_detectors() {
        let build = || prepared(squeezing(200.0, 2_000.0), 2);
        let mut fresh = build();
        let mut expected = noise::<f64>(2, 2_000, 1);
        run_in_blocks(&mut fresh, &mut expected, 512);
        let mut used = build();
        let mut other = noise::<f64>(2, 2_000, 5);
        run_in_blocks(&mut used, &mut other, 512);
        assert!(used.gain_reduction_db(1) > 0.0);
        used.reset();
        assert_eq!(used.gain_reduction_db(1), 0.0);
        let mut again = noise::<f64>(2, 2_000, 1);
        run_in_blocks(&mut used, &mut again, 512);
        assert!(again == expected);
    }

    #[test]
    fn single_precision_and_unprepared_use_work() {
        let mut compressor = MultibandCompressor::<f32>::new(200.0, 2_000.0);
        let mut buffer = AudioBuffer::<f32>::new(2, 64);
        buffer.channel_mut(0).fill(0.25);
        compressor.process(&mut buffer);
        assert_eq!(buffer.channel(0), &[0.25; 64]);
        compressor.prepare(&ProcessSpec {
            sample_rate: 44_100.0,
            max_block_size: 16,
            num_channels: 2,
        });
        compressor.process(&mut buffer);
        assert_eq!(compressor.band(0).key_source(), crate::KeySource::Input);
        assert_eq!(Processor::<f32>::latency_samples(&compressor), 0);
        assert_eq!(compressor.crossover().num_bands(), 3);
    }
}
