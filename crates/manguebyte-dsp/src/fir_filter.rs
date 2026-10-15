use manguebyte_core::{AudioBuffer, Sample};

use crate::convolution::{
    ConvolutionEngine, ConvolutionError, ConvolutionRouting, ConvolutionSettings,
};
use crate::{ProcessSpec, Processor};

/// How a [`FirFilter`] computes its output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FirMode {
    /// Every tap is multiplied with the input as it arrives: the cheapest
    /// way for short filters.
    Direct,
    /// The first taps are applied directly and the rest by partitioned
    /// convolution in the frequency domain: the cheapest way for long
    /// filters.
    Fft,
}

/// The largest partition considered for the frequency-domain mode.
const MAX_PARTITION: usize = 4_096;

/// Rough cost, in multiplications per sample and channel, of convolving
/// `taps` taps with partitions of `partition` samples and a direct head.
fn fft_cost(taps: usize, partition: usize) -> f64 {
    let partition_f = partition as f64;
    let partitions = taps.saturating_sub(partition).div_ceil(partition) as f64;
    // The head, then four multiplications per bin and partition spread over
    // the partition's samples, then a forward and an inverse transform,
    // weighted for their less regular memory access.
    partition_f
        + 4.0 * partitions * (partition_f + 1.0) / partition_f
        + 12.0 * (2.0 * partition_f).log2()
}

/// The cheaper mode for `taps` taps, and the partition size it uses.
fn choose(taps: usize) -> (FirMode, usize) {
    let mut best = (FirMode::Direct, taps.next_power_of_two(), taps as f64);
    let mut partition = 16;
    while partition <= MAX_PARTITION && partition < taps {
        let cost = fft_cost(taps, partition);
        if cost < best.2 {
            best = (FirMode::Fft, partition, cost);
        }
        partition *= 2;
    }
    (best.0, best.1)
}

/// A finite impulse response filter: each output sample is the sum of the
/// recent input samples weighted by the coefficients.
///
/// The coefficients can come from the [`fir`](crate::fir) design functions
/// or anywhere else. Short filters are computed directly and long ones in
/// the frequency domain, whichever costs fewer multiplications; both add no
/// latency, and the output is the same, bit for bit, however the audio is cut
/// into blocks. The two modes agree to within rounding error.
///
/// Each channel is filtered on its own with the same coefficients. A new
/// filter is built for one channel; [`prepare`](Processor::prepare) makes
/// room for the channel count of the spec. A block with more channels than
/// that is a mistake: debug builds stop on an assertion, and release builds
/// leave the extra channels unfiltered.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::fir;
/// use manguebyte_dsp::{FirFilter, FirMode, ProcessSpec, Processor};
///
/// // A 20-tap moving average is short enough to compute directly.
/// let mut average = FirFilter::<f32>::new(&[0.05; 20]).unwrap();
/// assert_eq!(average.mode(), FirMode::Direct);
///
/// // A long linear-phase low-pass goes through the FFT.
/// let taps = fir::lowpass(1_001, 0.1, 90.0);
/// let mut lowpass = FirFilter::<f32>::new(&taps).unwrap();
/// assert_eq!(lowpass.mode(), FirMode::Fft);
///
/// lowpass.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 512, num_channels: 1 });
/// let mut block = AudioBuffer::<f32>::new(1, 2_048);
/// block.channel_mut(0).fill(1.0);
/// lowpass.process(&mut block);
/// // A constant passes at unity gain once the filter is full.
/// assert!((block.channel(0)[2_047] - 1.0).abs() < 1e-4);
/// ```
#[derive(Debug, Clone)]
pub struct FirFilter<S: Sample = f32> {
    coefficients: Vec<S>,
    mode: FirMode,
    partition: usize,
    engine: ConvolutionEngine<S>,
}

impl<S: Sample> FirFilter<S> {
    /// Creates a filter, choosing the mode by length. Allocates.
    ///
    /// # Errors
    ///
    /// Fails if `coefficients` is empty or holds a value that is not finite.
    pub fn new(coefficients: &[f64]) -> Result<Self, ConvolutionError> {
        let (mode, partition) = choose(coefficients.len());
        Self::build(coefficients, mode, partition)
    }

    /// Creates a filter that uses `mode` whatever its length. Allocates.
    ///
    /// # Errors
    ///
    /// As for [`new`](Self::new).
    pub fn with_mode(coefficients: &[f64], mode: FirMode) -> Result<Self, ConvolutionError> {
        let partition = match mode {
            FirMode::Direct => coefficients.len().next_power_of_two(),
            FirMode::Fft => match choose(coefficients.len()) {
                (FirMode::Fft, partition) => partition,
                // Short filters: a partition of about a quarter of the taps
                // still leaves work for the transform.
                (FirMode::Direct, _) => (coefficients.len() / 4).next_power_of_two(),
            },
        };
        Self::build(coefficients, mode, partition)
    }

    fn build(
        coefficients: &[f64],
        mode: FirMode,
        partition: usize,
    ) -> Result<Self, ConvolutionError> {
        if coefficients.iter().any(|c| !c.is_finite()) {
            return Err(ConvolutionError::NotFinite);
        }
        let coefficients: Vec<S> = coefficients.iter().map(|&c| S::from_f64(c)).collect();
        if coefficients.is_empty() {
            return Err(ConvolutionError::EmptyResponse);
        }
        if coefficients.len().next_power_of_two() > crate::convolution::MAX_PARTITION_SIZE
            && mode == FirMode::Direct
        {
            return Err(ConvolutionError::PartitionSize(
                coefficients.len().next_power_of_two(),
            ));
        }
        let engine = Self::engine(&coefficients, partition, 1)?;
        Ok(Self {
            coefficients,
            mode,
            partition,
            engine,
        })
    }

    fn engine(
        coefficients: &[S],
        partition: usize,
        channels: usize,
    ) -> Result<ConvolutionEngine<S>, ConvolutionError> {
        ConvolutionEngine::new(
            &[coefficients],
            channels,
            ConvolutionRouting::Parallel,
            ConvolutionSettings::new()
                .with_partition_size(partition)
                .with_zero_latency(true),
        )
    }

    /// How the output is computed.
    pub fn mode(&self) -> FirMode {
        self.mode
    }

    /// The number of taps.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.coefficients.len()
    }

    /// The coefficients.
    pub fn coefficients(&self) -> &[S] {
        &self.coefficients
    }

    /// The partition size in frequency-domain mode; in direct mode, the
    /// power of two the taps fit in.
    pub fn partition_size(&self) -> usize {
        self.partition
    }
}

impl<S: Sample> Processor<S> for FirFilter<S> {
    /// Makes room for `spec.num_channels` channels, which allocates when the
    /// count changes, and clears the filter.
    fn prepare(&mut self, spec: &ProcessSpec) {
        let channels = spec.num_channels.max(1);
        if channels != self.engine.channels() {
            if let Ok(engine) = Self::engine(&self.coefficients, self.partition, channels) {
                self.engine = engine;
            }
        }
        self.engine.clear();
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        debug_assert!(
            buffer.num_channels() <= self.engine.channels(),
            "buffer has more channels than the filter was prepared for"
        );
        self.engine.process_block(buffer);
    }

    fn reset(&mut self) {
        self.engine.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fir;
    use crate::testing::{Random, noise};

    fn direct(signal: &[f64], taps: &[f64]) -> Vec<f64> {
        (0..signal.len())
            .map(|n| {
                taps.iter()
                    .enumerate()
                    .take(n + 1)
                    .map(|(k, &tap)| tap * signal[n - k])
                    .sum()
            })
            .collect()
    }

    fn spec(channels: usize) -> ProcessSpec {
        ProcessSpec {
            sample_rate: 48_000.0,
            max_block_size: 512,
            num_channels: channels,
        }
    }

    #[test]
    fn the_mode_follows_the_length() {
        for (taps, mode) in [
            (1, FirMode::Direct),
            (16, FirMode::Direct),
            (48, FirMode::Direct),
            (300, FirMode::Fft),
            (100_000, FirMode::Fft),
        ] {
            assert_eq!(
                FirFilter::<f32>::new(&vec![0.1; taps]).unwrap().mode(),
                mode,
                "{taps}"
            );
        }
        // Somewhere between, the transform starts to pay.
        let switch = (1..400)
            .find(|&taps| choose(taps).0 == FirMode::Fft)
            .unwrap();
        assert!((60..200).contains(&switch), "switches at {switch} taps");
        // For the longest filters the partition is a few thousand samples.
        assert!(
            FirFilter::<f32>::new(&vec![0.0; 100_000])
                .unwrap()
                .partition_size()
                >= 512
        );
    }

    #[test]
    fn both_modes_match_direct_convolution_in_any_blocks() {
        let mut random = Random::new(5);
        for taps in [1, 2, 5, 33, 64, 65, 200, 1_500] {
            let coefficients: Vec<f64> = (0..taps).map(|_| random.between(-0.5, 0.5)).collect();
            let input = noise::<f64>(2, 4_000, taps as u64);
            let expected: Vec<Vec<f64>> = input
                .iter_channels()
                .map(|channel| direct(channel, &coefficients))
                .collect();
            for mode in [FirMode::Direct, FirMode::Fft] {
                let mut bits = None;
                for pattern in [1, 64, 257, 4_000] {
                    let mut filter = FirFilter::<f64>::with_mode(&coefficients, mode).unwrap();
                    assert_eq!(filter.mode(), mode);
                    filter.prepare(&spec(2));
                    let mut signal = input.clone();
                    crate::testing::run_in_blocks(&mut filter, &mut signal, pattern);
                    for (channel, wanted) in expected.iter().enumerate() {
                        for (frame, (&got, &want)) in
                            signal.channel(channel).iter().zip(wanted).enumerate()
                        {
                            assert!(
                                (got - want).abs() < 1e-12,
                                "{taps} taps, {mode:?}, blocks of {pattern}, frame {frame}"
                            );
                        }
                    }
                    let these: Vec<u64> = signal
                        .iter_channels()
                        .flat_map(|channel| channel.iter().map(|s| s.to_bits()))
                        .collect();
                    match &bits {
                        None => bits = Some(these),
                        Some(first) => assert!(*first == these, "{taps} taps, {mode:?}"),
                    }
                }
            }
        }
    }

    #[test]
    fn a_designed_lowpass_passes_low_and_stops_high() {
        let taps = fir::lowpass(fir::kaiser_taps(80.0, 0.02), 0.1, 80.0);
        let mut filter = FirFilter::<f32>::new(&taps).unwrap();
        assert_eq!(filter.mode(), FirMode::Fft);
        filter.prepare(&spec(1));
        let delay = (taps.len() - 1) / 2;
        for (frequency, low) in [(1_000.0, true), (9_000.0, false)] {
            filter.reset();
            let mut signal = crate::testing::sine::<f32>(1, 16_384, frequency, 48_000.0, 1.0);
            crate::testing::run_in_blocks(&mut filter, &mut signal, 100);
            let tail = &signal.channel(0)[8_192..];
            let peak = tail.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
            if low {
                assert!((peak - 1.0).abs() < 1e-3, "{peak}");
                // Linear phase: the sine comes out delayed by half the length.
                let n = 10_000;
                let expected =
                    (core::f64::consts::TAU * frequency * (n - delay) as f64 / 48_000.0).sin();
                assert!((f64::from(signal.channel(0)[n]) - expected).abs() < 1e-3);
            } else {
                assert!(peak < 2e-4, "{peak}");
            }
        }
    }

    #[test]
    fn prepare_follows_the_channel_count_and_reset_clears() {
        let mut filter = FirFilter::<f64>::new(&[0.0, 1.0]).unwrap();
        filter.prepare(&spec(3));
        let mut buffer = AudioBuffer::<f64>::new(3, 2);
        for channel in 0..3 {
            buffer.channel_mut(channel).fill(channel as f64 + 1.0);
        }
        filter.process(&mut buffer);
        for channel in 0..3 {
            assert_eq!(buffer.channel(channel), &[0.0, channel as f64 + 1.0]);
        }
        filter.reset();
        let mut silence = AudioBuffer::<f64>::new(3, 2);
        filter.process(&mut silence);
        assert_eq!(silence.peak(), 0.0);
        assert_eq!(filter.len(), 2);
        assert_eq!(filter.coefficients(), &[0.0, 1.0]);
        assert_eq!(filter.latency_samples(), 0);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "more channels than the filter was prepared for")]
    fn more_channels_than_prepared_trip_an_assertion() {
        // Without `prepare` the filter has one channel; the second would
        // otherwise pass through unfiltered.
        let mut filter = FirFilter::<f32>::new(&[0.5]).unwrap();
        let mut stereo = AudioBuffer::<f32>::new(2, 4);
        filter.process(&mut stereo);
    }

    #[test]
    fn prepared_for_two_channels_both_are_filtered() {
        let mut filter = FirFilter::<f32>::new(&[0.5]).unwrap();
        filter.prepare(&spec(2));
        let mut stereo = AudioBuffer::<f32>::new(2, 4);
        stereo.channel_mut(0).fill(1.0);
        stereo.channel_mut(1).fill(1.0);
        filter.process(&mut stereo);
        assert_eq!(stereo.channel(0), &[0.5; 4]);
        assert_eq!(stereo.channel(1), &[0.5; 4]);
    }

    #[test]
    fn bad_coefficients_are_refused() {
        assert_eq!(
            FirFilter::<f32>::new(&[]).unwrap_err(),
            ConvolutionError::EmptyResponse
        );
        assert_eq!(
            FirFilter::<f32>::new(&[1.0, f64::NAN]).unwrap_err(),
            ConvolutionError::NotFinite
        );
        assert_eq!(
            FirFilter::<f32>::with_mode(&[1.0, f64::INFINITY], FirMode::Fft).unwrap_err(),
            ConvolutionError::NotFinite
        );
        // Values too large for single precision become infinite.
        assert_eq!(
            FirFilter::<f32>::new(&[1e300]).unwrap_err(),
            ConvolutionError::NotFinite
        );
        assert!(matches!(
            FirFilter::<f32>::with_mode(&vec![0.0; 70_000], FirMode::Direct),
            Err(ConvolutionError::PartitionSize(_))
        ));
    }
}
