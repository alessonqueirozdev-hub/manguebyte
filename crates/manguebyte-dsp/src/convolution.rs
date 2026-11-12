//! Convolution with an impulse response of any length.
//!
//! [`ConvolutionEngine`] does the work. It splits the impulse response into
//! partitions of equal size and convolves each one in the frequency domain
//! with the crate's [FFT](crate::fft), using overlap-save: the uniformly
//! partitioned scheme described by W. G. Gardner, "Efficient convolution
//! without input-output delay", Journal of the Audio Engineering Society,
//! 1995, and by F. Wefers, "Partitioned convolution algorithms for real-time
//! auralization", PhD thesis, RWTH Aachen, 2015.
//!
//! In zero-latency mode the first partition is convolved directly in the time
//! domain (a direct-form "head"), so the output is not delayed at all. In
//! partition mode the head is left out and the output is late by exactly one
//! partition, which costs less.
//!
//! The engine buffers internally one sample at a time, so the output is the
//! same, bit for bit, however the host slices the audio into blocks.
//!
//! Both inner loops run on the processor's vector instructions through
//! [`manguebyte_core::vector`], with the same results on every processor.
//! The direct head works on all the frames of a piece of the block at once:
//! for each tap, the input under it, scaled, is added to every frame's sum
//! ([`add_scaled`](manguebyte_core::vector::add_scaled)). Each frame's sum
//! adds its taps in groups of 32, in order, last tap first, and
//! then the groups' sums in order, whatever the piece. The
//! frequency-domain sums are
//! [`complex_multiply_add`](manguebyte_core::vector::complex_multiply_add),
//! which gives exactly what the plain loop gives; the spectra are kept as
//! interleaved real and imaginary parts for it.
//!
//! The frequency-domain sums are spread over the samples of each partition:
//! while one partition of input fills, the contributions of the older ones
//! to the next output are added a few at a time, so when the partition is
//! complete only its own transforms and one product are left. With host
//! blocks smaller than a partition, no block carries the whole sum.
//!
//! [`ConvolutionSettings::with_non_uniform_partitions`] asks for a scheme
//! that is cheaper on long responses: after the first partitions the size
//! doubles from band to band, so taps `[s, 2s)` of the response are
//! convolved in partitions of `s`, up to a largest size where the tail runs
//! on. Each band is a uniform convolution of its own, all computed on the
//! audio thread as everything here is: the bands' boundaries are staggered
//! half a period apart, so at most one band's forward transform and one
//! band's inverse transform land in a block of `partition_size` samples,
//! and the inverse is deferred to the block after the boundary. The output
//! stays the same bit for bit however the host slices the blocks, and the
//! latency is the same as the uniform scheme's.
//!
//! [`Convolution`] is the processor to put in a chain. It runs one engine
//! and takes a new one from a [`ConvolutionLoader`] on another thread, with
//! an optional crossfade, and sends the old one back to be dropped there. The
//! audio thread never allocates or frees.
//!
//! ```
//! use manguebyte_core::AudioBuffer;
//! use manguebyte_dsp::{
//!     Convolution, ConvolutionEngine, ConvolutionRouting, ConvolutionSettings, Processor,
//! };
//!
//! // An echo: the sound now, and half as loud 1000 samples later.
//! let mut response = vec![0.0f32; 1001];
//! response[0] = 1.0;
//! response[1000] = 0.5;
//!
//! let engine = ConvolutionEngine::new(
//!     &[response],
//!     1,
//!     ConvolutionRouting::Parallel,
//!     ConvolutionSettings::new(),
//! )
//! .unwrap();
//! assert_eq!(engine.latency(), 0);
//!
//! // Off the audio thread: hand the engine over.
//! let (mut convolution, mut loader) = Convolution::<f32>::new();
//! loader.load(engine).unwrap();
//!
//! // On the audio thread: an impulse comes out as the response.
//! let mut block = AudioBuffer::<f32>::new(1, 1200);
//! block.channel_mut(0)[0] = 1.0;
//! convolution.process(&mut block);
//! assert!((block.channel(0)[0] - 1.0).abs() < 1e-6);
//! assert!((block.channel(0)[1000] - 0.5).abs() < 1e-6);
//! assert!(block.channel(0)[500].abs() < 1e-6);
//! ```

use core::fmt;

use manguebyte_core::spsc::{self, Consumer, Producer};
use manguebyte_core::{AudioBuffer, Sample, vector};

use crate::fft::{Complex, RealFft};
use crate::{ProcessSpec, Processor};

/// How many taps of the direct head are summed before their sum is added to
/// the others. Summing every tap in one run would let the rounding error
/// grow with the length of the head; in groups it grows with this plus the
/// number of groups.
const HEAD_GROUP: usize = 32;

/// Largest partition a [`ConvolutionEngine`] accepts.
pub const MAX_PARTITION_SIZE: usize = 1 << 16;

/// Most channels a [`ConvolutionEngine`] accepts.
pub const MAX_CHANNELS: usize = 256;

/// Most engines a [`ConvolutionLoader`] can have sent and not yet collected,
/// beyond the one playing and the one fading out. See
/// [`Convolution::with_capacity`].
pub const MAX_LOADER_CAPACITY: usize = 1_024;

/// How a [`ConvolutionEngine`] splits its impulse response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConvolutionSettings {
    partition_size: usize,
    largest_partition_size: usize,
    zero_latency: bool,
}

impl Default for ConvolutionSettings {
    fn default() -> Self {
        Self::new()
    }
}

impl ConvolutionSettings {
    /// Partitions of 128 samples, with no latency.
    pub const fn new() -> Self {
        Self {
            partition_size: 128,
            largest_partition_size: 128,
            zero_latency: true,
        }
    }

    /// Sets the partition size in samples. It must be a power of two from 1
    /// to [`MAX_PARTITION_SIZE`].
    ///
    /// Smaller partitions cost more per sample for a long response; in
    /// partition mode they also mean less latency. In zero-latency mode the
    /// first partition is convolved directly, which costs `partition_size`
    /// multiplications per sample and channel.
    ///
    /// With non-uniform partitions this is the size of the first, smallest
    /// partitions, which the cost of short responses and of the direct head
    /// follows. While the scheme is uniform, the largest partition follows
    /// this size.
    #[must_use]
    pub const fn with_partition_size(mut self, frames: usize) -> Self {
        if self.largest_partition_size == self.partition_size {
            self.largest_partition_size = frames;
        }
        self.partition_size = frames;
        self
    }

    /// Lets the partition size grow past the first partitions, doubling
    /// from band to band up to `largest` samples: response taps `[s, 2s)`
    /// are convolved in partitions of `s`. `largest` must be a power of
    /// two at least twice `partition_size`, up to [`MAX_PARTITION_SIZE`];
    /// equal to `partition_size` (the default) is the uniform scheme.
    ///
    /// For a long response the tail is convolved in a few large partitions
    /// instead of hundreds of small ones, which is cheaper per sample at
    /// small block sizes. The latency is unchanged. The price is that the
    /// largest transforms still land inside one block each, so the busiest
    /// block grows with `largest`; the work is staggered so that at most
    /// one band of partitions starts and one ends in any block of
    /// `partition_size` samples.
    #[must_use]
    pub const fn with_non_uniform_partitions(mut self, largest: usize) -> Self {
        self.largest_partition_size = largest;
        self
    }

    /// Chooses between no latency (`true`, the default) and a latency of one
    /// partition (`false`), which is cheaper.
    #[must_use]
    pub const fn with_zero_latency(mut self, zero_latency: bool) -> Self {
        self.zero_latency = zero_latency;
        self
    }

    /// The size of the first, smallest partitions, in samples.
    pub const fn partition_size(&self) -> usize {
        self.partition_size
    }

    /// The largest partition in samples: `partition_size` when the scheme
    /// is uniform.
    pub const fn largest_partition_size(&self) -> usize {
        self.largest_partition_size
    }

    /// Whether the first partition is convolved directly, for no latency.
    pub const fn zero_latency(&self) -> bool {
        self.zero_latency
    }
}

/// Which input feeds which output through which impulse response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConvolutionRouting {
    /// Each channel is convolved on its own. With one response, every
    /// channel uses it; with one response per channel, channel `n` uses
    /// response `n`. Stereo with a stereo response is this.
    Parallel,
    /// Two channels and four responses, in the order left to left, left to
    /// right, right to left, right to right. Each output is the sum of both
    /// inputs, each through its own response.
    TrueStereo,
}

/// Why a [`ConvolutionEngine`] could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConvolutionError {
    /// There is no response, or every response is empty.
    EmptyResponse,
    /// The number of responses does not fit the routing and channel count.
    ChannelCount {
        /// Responses given.
        responses: usize,
        /// Channels asked for.
        channels: usize,
    },
    /// The partition size is not a power of two from 1 to
    /// [`MAX_PARTITION_SIZE`].
    PartitionSize(usize),
    /// A sample of the response is not-a-number or infinite.
    NotFinite,
}

impl fmt::Display for ConvolutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyResponse => write!(f, "the impulse response is empty"),
            Self::ChannelCount {
                responses,
                channels,
            } => write!(
                f,
                "{responses} impulse response(s) do not fit {channels} channel(s) with this routing"
            ),
            Self::PartitionSize(size) => write!(
                f,
                "partition size {size} is not a power of two from 1 to {MAX_PARTITION_SIZE}"
            ),
            Self::NotFinite => write!(
                f,
                "the impulse response contains a value that is not finite"
            ),
        }
    }
}

impl std::error::Error for ConvolutionError {}

/// One prepared impulse response.
#[derive(Clone)]
struct Kernel<S> {
    /// The first `head_len` taps, last tap first, for the direct part.
    head: Vec<S>,
    /// Spectra of the remaining taps, one partition after another, as
    /// interleaved real and imaginary parts.
    partitions: Vec<S>,
}

/// One band of partitions of equal size in a non-uniform scheme: a uniform
/// convolution on its own, covering `partitions` response partitions of
/// `size` samples each. The band right below convolves the earlier taps in
/// partitions half the size, so the bands tile the response without gaps;
/// the last, largest band runs on to the end of the response.
///
/// A band's boundary, where a partition of input completes, lands on a
/// different block than any other band's: band `i` starts half a period
/// late, so its boundaries fall `size / 2` samples after a boundary of
/// every smaller band. Two boundary jobs are split between two blocks of
/// `partition_size` samples: the forward transform of the input just
/// completed runs at the boundary, and the last product and the inverse
/// transform run one block later, before the new `pending` is first read.
#[derive(Clone)]
struct Band<S: Sample> {
    /// The partition size of this band.
    size: usize,
    /// Frequency bins of the size-2·`size` transform: `size + 1`.
    bins: usize,
    /// Response partitions this band covers.
    partitions: usize,
    fft: RealFft<S>,
    /// Per response, the spectra of its partitions of this band, as
    /// interleaved real and imaginary parts.
    kernels: Vec<Vec<S>>,
    /// Per input channel, `2 * size` samples: the previous partition of
    /// input, then the current one as it fills.
    frames: Vec<S>,
    /// Per input channel, the spectra of the last `partitions` input
    /// frames, as a ring, as interleaved real and imaginary parts.
    spectra: Vec<S>,
    /// Per output channel, the partition of output computed last, played
    /// out while the next one fills.
    pending: Vec<S>,
    /// Per output channel, the spectrum of the next partition of output,
    /// as far as it has been summed, as interleaved real and imaginary
    /// parts.
    accumulators: Vec<S>,
    /// One spectrum, for the transforms.
    spectrum: Vec<Complex<S>>,
    time: Vec<S>,
    /// How much of the current partition has been filled.
    fill: usize,
    /// Where in the ring of spectra the newest one goes.
    position: usize,
    /// How many of the older partitions (1, 2, ...) have been added to
    /// `accumulators` for the next partition of output.
    summed: usize,
    /// Whether the last boundary's product and inverse transform are still
    /// owed before `pending` is read.
    inverse_due: bool,
}

/// Which input reaches which output through which kernel.
#[derive(Debug, Clone, Copy)]
struct Path {
    input: usize,
    output: usize,
    kernel: usize,
}

/// A prepared convolution: impulse responses already transformed, and all
/// the memory it will need already allocated.
///
/// Building one allocates and costs time in proportion to the response, so
/// it is done off the audio thread. Processing allocates nothing.
///
/// It is a [`Processor`] in its own right. Use it directly when the
/// response never changes, or hand it to a [`Convolution`] through a
/// [`ConvolutionLoader`] to swap responses while playing.
///
/// # Accuracy
///
/// Against direct time-domain convolution in double precision, the
/// unit tests measure a largest error below `2e-6` of the output's RMS level
/// in single precision, for random responses from 1 to 100 000 samples, and
/// below `1e-13` in double precision for responses up to 20 000 samples.
/// Responses up to 3 000 samples are compared on every output frame; longer
/// ones on about 400 frames picked at random, plus the last frame and the
/// frame where the response ends.
///
/// # Channels
///
/// Channels of a block beyond the engine's are left as they are, unfiltered.
/// Channels the block lacks are treated as silent inputs.
#[derive(Clone)]
pub struct ConvolutionEngine<S: Sample = f32> {
    channels: usize,
    block: usize,
    bins: usize,
    head_len: usize,
    num_partitions: usize,
    response_len: usize,
    zero_latency: bool,
    kernels: Vec<Kernel<S>>,
    paths: Vec<Path>,
    fft: RealFft<S>,
    /// Per input channel, `2 * block` samples: the previous partition of
    /// input, then the current one as it fills.
    frames: Vec<S>,
    /// Per input channel, the spectra of the last `num_partitions` input
    /// frames, as a ring, as interleaved real and imaginary parts.
    spectra: Vec<S>,
    /// Per output channel, the partition of output computed last, played out
    /// while the next one fills.
    pending: Vec<S>,
    /// Per output channel, the spectrum of the next partition of output, as
    /// far as it has been summed, as interleaved real and imaginary parts.
    accumulators: Vec<S>,
    /// One spectrum, for the transforms.
    spectrum: Vec<Complex<S>>,
    time: Vec<S>,
    /// The direct head's sums for the frames of one piece of a block, and
    /// the sums of one group of taps.
    head_sums: Vec<S>,
    head_group: Vec<S>,
    /// How much of the current partition has been filled.
    fill: usize,
    /// Where in the ring of spectra the newest one goes.
    position: usize,
    /// How many of the older partitions (1, 2, ...) have been added to
    /// `accumulators` for the next partition of output.
    summed: usize,
    /// The bands of larger partitions a non-uniform scheme uses past the
    /// first partition, in order of size. Empty in the uniform scheme.
    bands: Vec<Band<S>>,
}

impl<S: Sample> Band<S> {
    /// Adds the older partitions' share of the next output, in proportion
    /// to how much of the current partition has been filled, exactly as the
    /// smallest partitions' `sum_ahead` does.
    fn sum_ahead(&mut self, paths: &[Path]) {
        if self.partitions < 2 {
            return;
        }
        let older = (self.partitions - 1) as u64;
        // Below `older * size`, so it fits.
        let due = (older * self.fill as u64 / self.size as u64) as usize;
        self.sum_partitions(due, paths);
    }

    /// Adds partitions `summed + 1` to `upto` of the response band to the
    /// next output, as `sum_partitions` does for the smallest partitions.
    fn sum_partitions(&mut self, upto: usize, paths: &[Path]) {
        let (bins, count) = (self.bins, self.partitions);
        while self.summed < upto {
            let partition = self.summed + 1;
            let slot = (self.position + count - partition) % count;
            let len = 2 * bins;
            for path in paths {
                let input = &self.spectra[(path.input * count + slot) * len..][..len];
                let response = &self.kernels[path.kernel][partition * len..][..len];
                let sum = &mut self.accumulators[path.output * len..][..len];
                vector::complex_multiply_add(sum, input, response);
            }
            self.summed += 1;
        }
    }

    /// Advances by `count` samples of input already recorded, doing the
    /// boundary work when a partition completes: whatever is left of the
    /// older partitions' share, then the forward transform, whose spectrum
    /// goes to the ring. The last product and the inverse transform are
    /// owed to the next block (`inverse_due`), so two transforms of the
    /// band's size never land in one block.
    fn advance(&mut self, count: usize, paths: &[Path]) {
        self.fill += count;
        if self.fill == self.size {
            self.sum_partitions(self.partitions - 1, paths);
            let len = 2 * self.bins;
            for (channel, frame) in self.frames.chunks_exact(2 * self.size).enumerate() {
                self.fft.forward(frame, &mut self.spectrum);
                let slot =
                    &mut self.spectra[(channel * self.partitions + self.position) * len..][..len];
                split_parts(&self.spectrum, slot);
            }
            for frame in self.frames.chunks_exact_mut(2 * self.size) {
                frame.copy_within(self.size.., 0);
            }
            self.fill = 0;
            self.inverse_due = true;
        } else {
            self.sum_ahead(paths);
        }
    }

    /// The work deferred to the block after a boundary: the newest input
    /// spectrum meets the band's first partition, and the sum goes back to
    /// the time domain to be played out from this block on.
    fn finish_pending(&mut self, paths: &[Path]) {
        let len = 2 * self.bins;
        for path in paths {
            let input =
                &self.spectra[(path.input * self.partitions + self.position) * len..][..len];
            let response = &self.kernels[path.kernel][..len];
            let sum = &mut self.accumulators[path.output * len..][..len];
            vector::complex_multiply_add(sum, input, response);
        }
        for (output, sum) in self.accumulators.chunks_exact_mut(len).enumerate() {
            join_parts(sum, &mut self.spectrum);
            self.fft.inverse(&self.spectrum, &mut self.time);
            self.pending[output * self.size..][..self.size]
                .copy_from_slice(&self.time[self.size..]);
            sum.fill(S::ZERO);
        }
        self.summed = 0;
        self.position = (self.position + 1) % self.partitions;
        self.inverse_due = false;
    }

    /// Forgets all past input, and puts the boundary back half a period
    /// after every smaller band's, where the staggering wants it.
    fn clear(&mut self) {
        self.frames.fill(S::ZERO);
        self.spectra.fill(S::ZERO);
        self.pending.fill(S::ZERO);
        self.accumulators.fill(S::ZERO);
        self.fill = self.size / 2;
        self.position = 0;
        self.summed = 0;
        self.inverse_due = false;
    }
}

impl<S: Sample> fmt::Debug for ConvolutionEngine<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConvolutionEngine")
            .field("channels", &self.channels)
            .field("partition_size", &self.block)
            .field("largest_partition_size", &self.largest_partition_size())
            .field("partitions", &self.num_partitions())
            .field("response_len", &self.response_len)
            .field("latency", &self.latency())
            .finish_non_exhaustive()
    }
}

impl<S: Sample> ConvolutionEngine<S> {
    /// Prepares a convolution of `channels` channels with `responses`.
    ///
    /// The responses may have different lengths; the shorter ones are padded
    /// with silence. Allocates; call it off the audio thread.
    ///
    /// # Errors
    ///
    /// Fails if there is no response or all are empty, if the number of
    /// responses does not fit `routing` and `channels` (see
    /// [`ConvolutionRouting`]) or `channels` is more than [`MAX_CHANNELS`],
    /// if the partition size is not a power of two up to
    /// [`MAX_PARTITION_SIZE`], or if a sample is not finite.
    pub fn new<R: AsRef<[S]>>(
        responses: &[R],
        channels: usize,
        routing: ConvolutionRouting,
        settings: ConvolutionSettings,
    ) -> Result<Self, ConvolutionError> {
        let block = settings.partition_size;
        if !block.is_power_of_two() || block > MAX_PARTITION_SIZE {
            return Err(ConvolutionError::PartitionSize(block));
        }
        let largest = settings.largest_partition_size;
        if largest != block
            && (!largest.is_power_of_two() || largest < 2 * block || largest > MAX_PARTITION_SIZE)
        {
            return Err(ConvolutionError::PartitionSize(largest));
        }
        let count_error = ConvolutionError::ChannelCount {
            responses: responses.len(),
            channels,
        };
        if channels > MAX_CHANNELS {
            return Err(count_error);
        }
        let paths: Vec<Path> = match routing {
            ConvolutionRouting::Parallel => {
                if channels == 0 || (responses.len() != 1 && responses.len() != channels) {
                    return Err(count_error);
                }
                (0..channels)
                    .map(|channel| Path {
                        input: channel,
                        output: channel,
                        kernel: if responses.len() == 1 { 0 } else { channel },
                    })
                    .collect()
            }
            ConvolutionRouting::TrueStereo => {
                if channels != 2 || responses.len() != 4 {
                    return Err(count_error);
                }
                [(0, 0), (0, 1), (1, 0), (1, 1)]
                    .into_iter()
                    .enumerate()
                    .map(|(kernel, (input, output))| Path {
                        input,
                        output,
                        kernel,
                    })
                    .collect()
            }
        };

        let response_len = responses
            .iter()
            .map(|response| response.as_ref().len())
            .max()
            .unwrap_or(0);
        if response_len == 0 {
            return Err(ConvolutionError::EmptyResponse);
        }
        if responses
            .iter()
            .any(|response| response.as_ref().iter().any(|s| !s.to_f64().is_finite()))
        {
            return Err(ConvolutionError::NotFinite);
        }

        let zero_latency = settings.zero_latency;
        let head_len = if zero_latency {
            response_len.min(block)
        } else {
            0
        };
        let non_uniform = largest != block;
        // In a non-uniform scheme the first, smallest partitions cover one
        // partition of the response after the head; the bands take over
        // from there.
        let num_partitions = if non_uniform {
            (response_len - head_len).div_ceil(block).min(1)
        } else {
            (response_len - head_len).div_ceil(block)
        };
        let bins = block + 1;
        let mut fft = RealFft::<S>::new(2 * block);

        let mut frame = vec![S::ZERO; 2 * block];
        let mut spectrum = vec![Complex::ZERO; bins];
        let kernels = responses
            .iter()
            .map(|response| {
                let response = response.as_ref();
                let tap = |index: usize| response.get(index).copied().unwrap_or(S::ZERO);
                let head = (0..head_len).rev().map(tap).collect();
                let mut partitions = vec![S::ZERO; num_partitions * 2 * bins];
                for (index, parts) in partitions.chunks_exact_mut(2 * bins).enumerate() {
                    let start = head_len + index * block;
                    for (offset, slot) in frame[..block].iter_mut().enumerate() {
                        *slot = tap(start + offset);
                    }
                    frame[block..].fill(S::ZERO);
                    fft.forward(&frame, &mut spectrum);
                    split_parts(&spectrum, parts);
                }
                Kernel { head, partitions }
            })
            .collect();

        // The bands of a non-uniform scheme: partitions doubling in size
        // from `2 * block` to `largest`. The first tap of a band sits
        // `delta` samples before its size: at `size` in zero-latency mode,
        // where the direct head covers the first `block` taps and the
        // bands the rest, and at `size - block` in partition mode, where
        // every band's output is late by the same `block` as the smallest
        // partitions'. A middle band covers exactly one partition, up to
        // where the band twice its size begins; the largest band runs on
        // to the end of the response.
        let delta = block - head_len;
        let mut bands = Vec::new();
        if non_uniform {
            let mut size = 2 * block;
            while size <= largest {
                let offset = size - delta;
                if offset >= response_len {
                    break;
                }
                let end = if 2 * size <= largest {
                    (2 * size - delta).min(response_len)
                } else {
                    response_len
                };
                let partitions = (end - offset).div_ceil(size);
                let bins = size + 1;
                let mut fft = RealFft::<S>::new(2 * size);
                let mut frame = vec![S::ZERO; 2 * size];
                let mut spectrum = vec![Complex::ZERO; bins];
                let kernels = responses
                    .iter()
                    .map(|response| {
                        let response = response.as_ref();
                        let mut parts = vec![S::ZERO; partitions * 2 * bins];
                        for (index, part) in parts.chunks_exact_mut(2 * bins).enumerate() {
                            let start = offset + index * size;
                            for (o, slot) in frame[..size].iter_mut().enumerate() {
                                *slot = response.get(start + o).copied().unwrap_or(S::ZERO);
                            }
                            frame[size..].fill(S::ZERO);
                            fft.forward(&frame, &mut spectrum);
                            split_parts(&spectrum, part);
                        }
                        parts
                    })
                    .collect();
                // `fill` starts half a partition in, so this band's
                // boundaries land `size / 2` samples after every smaller
                // band's and never share a block with them.
                bands.push(Band {
                    size,
                    bins,
                    partitions,
                    fft,
                    kernels,
                    frames: vec![S::ZERO; channels * 2 * size],
                    spectra: vec![S::ZERO; channels * partitions * 2 * bins],
                    pending: vec![S::ZERO; channels * size],
                    accumulators: vec![S::ZERO; channels * 2 * bins],
                    spectrum,
                    time: vec![S::ZERO; 2 * size],
                    fill: size / 2,
                    position: 0,
                    summed: 0,
                    inverse_due: false,
                });
                size *= 2;
            }
        }

        Ok(Self {
            channels,
            block,
            bins,
            head_len,
            num_partitions,
            response_len,
            zero_latency,
            kernels,
            paths,
            fft,
            frames: vec![S::ZERO; channels * 2 * block],
            spectra: vec![S::ZERO; channels * num_partitions * 2 * bins],
            pending: vec![S::ZERO; channels * block],
            accumulators: vec![S::ZERO; channels * 2 * bins],
            spectrum,
            time: vec![S::ZERO; 2 * block],
            head_sums: vec![S::ZERO; if head_len > 0 { block } else { 0 }],
            head_group: vec![S::ZERO; if head_len > 0 { block } else { 0 }],
            fill: 0,
            position: 0,
            summed: 0,
            bands,
        })
    }

    /// The number of channels it processes.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// The size of the first, smallest partitions in samples.
    pub fn partition_size(&self) -> usize {
        self.block
    }

    /// The largest partition in samples: the partition size when the
    /// scheme is uniform.
    pub fn largest_partition_size(&self) -> usize {
        self.bands.last().map_or(self.block, |band| band.size)
    }

    /// The number of partitions convolved in the frequency domain: the
    /// first partition plus, in a non-uniform scheme, every band's.
    pub fn num_partitions(&self) -> usize {
        self.num_partitions + self.bands.iter().map(|band| band.partitions).sum::<usize>()
    }

    /// The length of the longest response, in samples.
    pub fn response_len(&self) -> usize {
        self.response_len
    }

    /// How many samples late the output is: zero in zero-latency mode, the
    /// partition size otherwise.
    pub fn latency(&self) -> usize {
        if self.zero_latency { 0 } else { self.block }
    }

    /// Convolves a block in place. Real-time safe.
    pub fn process_block(&mut self, buffer: &mut AudioBuffer<S>) {
        let frames = buffer.num_frames();
        let present = buffer.num_channels().min(self.channels);
        let (block, head_len) = (self.block, self.head_len);
        let mut start = 0;
        while start < frames {
            let count = (block - self.fill).min(frames - start);
            let at = block + self.fill;

            // A band boundary left the newest product and the inverse
            // transform to this block; they run before the fresh pending
            // is first read below.
            for band in &mut self.bands {
                if band.inverse_due {
                    band.finish_pending(&self.paths);
                }
            }

            // Record the input first: with true stereo, every output needs
            // every input of the same frame.
            for (channel, frame) in self.frames.chunks_exact_mut(2 * block).enumerate() {
                let target = &mut frame[at..at + count];
                if channel < present {
                    target.copy_from_slice(&buffer.channel(channel)[start..start + count]);
                } else {
                    target.fill(S::ZERO);
                }
            }
            for band in self.bands.iter_mut() {
                let at = band.size + band.fill;
                for (channel, frame) in band.frames.chunks_exact_mut(2 * band.size).enumerate() {
                    let target = &mut frame[at..at + count];
                    if channel < present {
                        target.copy_from_slice(&buffer.channel(channel)[start..start + count]);
                    } else {
                        target.fill(S::ZERO);
                    }
                }
            }

            for output in 0..present {
                let out = &mut buffer.channel_mut(output)[start..start + count];
                let pending = &self.pending[output * block + self.fill..][..count];
                out.copy_from_slice(pending);
                for band in &self.bands {
                    let at = output * band.size + band.fill;
                    vector::add(out, &band.pending[at..at + count]);
                }
                if head_len == 0 {
                    continue;
                }
                // Frame `at + n` meets the input from `at + n + 1 - head_len`
                // on: tap `i` of the head, which is stored last tap first,
                // meets the input `i` frames after that, for every frame
                // of the piece at once.
                let sums = &mut self.head_sums[..count];
                let group = &mut self.head_group[..count];
                for path in self.paths.iter().filter(|path| path.output == output) {
                    let head = &self.kernels[path.kernel].head;
                    let frame = &self.frames[path.input * 2 * block..][..2 * block];
                    sums.fill(S::ZERO);
                    for (g, taps) in head.chunks(HEAD_GROUP).enumerate() {
                        group.fill(S::ZERO);
                        for (k, &tap) in taps.iter().enumerate() {
                            let first = at + 1 - head_len + g * HEAD_GROUP + k;
                            vector::add_scaled(group, &frame[first..first + count], tap);
                        }
                        vector::add(sums, group);
                    }
                    vector::add(out, sums);
                }
            }

            self.fill += count;
            start += count;
            if self.fill == block {
                self.finish_partition();
            } else {
                self.sum_ahead();
            }
            for band in &mut self.bands {
                band.advance(count, &self.paths);
            }
        }
    }

    /// Adds the older partitions' share of the next output, in proportion
    /// to how much of the current partition has been filled.
    fn sum_ahead(&mut self) {
        if self.num_partitions < 2 {
            return;
        }
        let older = (self.num_partitions - 1) as u64;
        // Below `older * block`, so it fits.
        let due = (older * self.fill as u64 / self.block as u64) as usize;
        self.sum_partitions(due);
    }

    /// Adds partitions `summed + 1` to `upto` of the response to the next
    /// output. Each one meets the input it is `partition` partitions older
    /// than; none of them needs the input still filling. The order of the
    /// additions is always the same, whatever the host's blocks.
    fn sum_partitions(&mut self, upto: usize) {
        let (bins, count) = (self.bins, self.num_partitions);
        while self.summed < upto {
            let partition = self.summed + 1;
            // The newest spectrum goes to `position` at the end of this
            // partition; the one `partition` before it is here.
            let slot = (self.position + count - partition) % count;
            let len = 2 * bins;
            for path in &self.paths {
                let input = &self.spectra[(path.input * count + slot) * len..][..len];
                let response = &self.kernels[path.kernel].partitions[partition * len..][..len];
                let sum = &mut self.accumulators[path.output * len..][..len];
                vector::complex_multiply_add(sum, input, response);
            }
            self.summed += 1;
        }
    }

    /// Runs the frequency-domain part once a partition of input is complete.
    fn finish_partition(&mut self) {
        let (block, bins, count) = (self.block, self.bins, self.num_partitions);
        if count > 0 {
            // Whatever the blocks left of the older partitions' share.
            self.sum_partitions(count - 1);
            let len = 2 * bins;
            for (channel, frame) in self.frames.chunks_exact(2 * block).enumerate() {
                self.fft.forward(frame, &mut self.spectrum);
                let slot = &mut self.spectra[(channel * count + self.position) * len..][..len];
                split_parts(&self.spectrum, slot);
            }
            // The first partition of the response meets the input just
            // completed.
            for path in &self.paths {
                let input = &self.spectra[(path.input * count + self.position) * len..][..len];
                let response = &self.kernels[path.kernel].partitions[..len];
                let sum = &mut self.accumulators[path.output * len..][..len];
                vector::complex_multiply_add(sum, input, response);
            }
            for (output, sum) in self.accumulators.chunks_exact_mut(len).enumerate() {
                join_parts(sum, &mut self.spectrum);
                self.fft.inverse(&self.spectrum, &mut self.time);
                self.pending[output * block..][..block].copy_from_slice(&self.time[block..]);
                sum.fill(S::ZERO);
            }
            self.summed = 0;
            self.position = (self.position + 1) % count;
        }
        for frame in self.frames.chunks_exact_mut(2 * block) {
            frame.copy_within(block.., 0);
        }
        self.fill = 0;
    }

    /// Forgets all past input. Real-time safe.
    pub fn clear(&mut self) {
        self.frames.fill(S::ZERO);
        self.spectra.fill(S::ZERO);
        self.pending.fill(S::ZERO);
        self.accumulators.fill(S::ZERO);
        self.fill = 0;
        self.position = 0;
        self.summed = 0;
        for band in &mut self.bands {
            band.clear();
        }
    }
}

/// A spectrum as interleaved real and imaginary parts.
fn split_parts<S: Sample>(spectrum: &[Complex<S>], parts: &mut [S]) {
    for (pair, value) in parts.chunks_exact_mut(2).zip(spectrum) {
        pair[0] = value.re;
        pair[1] = value.im;
    }
}

/// Interleaved real and imaginary parts as a spectrum.
fn join_parts<S: Sample>(parts: &[S], spectrum: &mut [Complex<S>]) {
    for (value, pair) in spectrum.iter_mut().zip(parts.chunks_exact(2)) {
        *value = Complex::new(pair[0], pair[1]);
    }
}

impl<S: Sample> Processor<S> for ConvolutionEngine<S> {
    /// Clears the engine. The impulse response is not resampled: build a new
    /// engine if the sample rate changes.
    fn prepare(&mut self, _spec: &ProcessSpec) {
        self.clear();
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        self.process_block(buffer);
    }

    fn reset(&mut self) {
        self.clear();
    }

    fn latency_samples(&self) -> usize {
        self.latency()
    }
}

/// A new engine on its way to the audio thread.
struct Swap<S: Sample> {
    engine: Box<ConvolutionEngine<S>>,
    crossfade: usize,
}

/// Engines that may be handed over and not yet collected, by default.
const DEFAULT_CAPACITY: usize = 4;

/// A convolution whose impulse response can be replaced while it plays.
///
/// Engines are built off the audio thread and sent with a
/// [`ConvolutionLoader`]. At the start of each block the processor takes
/// whatever has arrived, optionally crossfades from the old engine to the new
/// one, and sends the old one back to the loader to be dropped there.
///
/// With no engine loaded, audio passes through unchanged.
///
/// The latency it reports is that of the engine playing, so it changes when
/// an engine with the other latency mode (or another partition size in
/// partition mode) arrives, and a crossfade between the two mixes signals
/// that are out of step. Keep the latency the same across swaps, or tell the
/// host when it changes: [`ConvolutionLoader::latency`] says, off the audio
/// thread, what the processor will report once the engine sent last has
/// arrived. A new engine starts with no memory of past input, so its
/// reverberation builds up from the moment it takes over; the crossfade
/// covers that.
///
/// A crossfade is linear in amplitude and is counted in samples. Engines
/// that arrive during a crossfade wait for it to end, so the output never
/// jumps; then the newest of them fades in, and any sent before it are
/// returned unplayed.
///
/// For the crossfade, [`prepare`](Processor::prepare) reserves a copy of a
/// block of `max_block_size` frames and `num_channels` channels. Without
/// `prepare`, swaps happen at once.
///
/// Dropping the processor frees the engines it holds, so drop it off the
/// audio thread, as with any processor that owns memory.
pub struct Convolution<S: Sample = f32> {
    current: Option<Box<ConvolutionEngine<S>>>,
    fading: Option<Box<ConvolutionEngine<S>>>,
    fade_len: usize,
    fade_position: usize,
    incoming: Consumer<Swap<S>>,
    retired: Producer<Box<ConvolutionEngine<S>>>,
    scratch: AudioBuffer<S>,
}

impl<S: Sample> fmt::Debug for Convolution<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Convolution")
            .field("current", &self.current)
            .field("fading", &self.fading)
            .field("fade_len", &self.fade_len)
            .field("fade_position", &self.fade_position)
            .finish_non_exhaustive()
    }
}

/// The half of a [`Convolution`] that lives off the audio thread: it sends
/// new engines and drops the ones that come back.
pub struct ConvolutionLoader<S: Sample = f32> {
    outgoing: Producer<Swap<S>>,
    retired: Consumer<Box<ConvolutionEngine<S>>>,
    /// Engines sent and not yet collected, the one playing included.
    in_flight: usize,
    limit: usize,
    /// The latency of the engine sent last.
    latency: Option<usize>,
}

impl<S: Sample> fmt::Debug for ConvolutionLoader<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConvolutionLoader")
            .field("in_flight", &self.in_flight)
            .field("limit", &self.limit)
            .finish_non_exhaustive()
    }
}

impl<S: Sample> Convolution<S> {
    /// Creates a convolution with no engine, and the loader that feeds it.
    /// Up to four engines can be sent before any is collected.
    pub fn new() -> (Self, ConvolutionLoader<S>) {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Like [`new`](Self::new), but allowing `capacity` engines to be sent
    /// and not yet taken by the audio thread. It is held between 1 and
    /// [`MAX_LOADER_CAPACITY`].
    pub fn with_capacity(capacity: usize) -> (Self, ConvolutionLoader<S>) {
        // The playing engine and the one fading out still count as in
        // flight, so they get room of their own.
        let limit = capacity.clamp(1, MAX_LOADER_CAPACITY) + 2;
        // Every engine that comes back was counted in flight, so neither
        // queue can hold more than `limit`.
        let (outgoing, incoming) = spsc::channel(limit);
        let (retired_producer, retired_consumer) = spsc::channel(limit);
        (
            Self {
                current: None,
                fading: None,
                fade_len: 0,
                fade_position: 0,
                incoming,
                retired: retired_producer,
                scratch: AudioBuffer::new(0, 0),
            },
            ConvolutionLoader {
                outgoing,
                retired: retired_consumer,
                in_flight: 0,
                limit,
                latency: None,
            },
        )
    }

    /// Creates a convolution already playing `engine`.
    pub fn from_engine(engine: ConvolutionEngine<S>) -> (Self, ConvolutionLoader<S>) {
        let (mut convolution, mut loader) = Self::new();
        loader.latency = Some(engine.latency());
        convolution.current = Some(Box::new(engine));
        loader.in_flight = 1;
        (convolution, loader)
    }

    /// The engine playing, if any.
    pub fn engine(&self) -> Option<&ConvolutionEngine<S>> {
        self.current.as_deref()
    }

    /// Whether a crossfade is under way.
    pub fn is_crossfading(&self) -> bool {
        self.fading.is_some()
    }

    fn retire(&mut self, engine: Box<ConvolutionEngine<S>>) {
        if let Err(engine) = self.retired.push(engine) {
            // The loader refuses to send more engines than this queue holds,
            // so this cannot happen. Should it, freeing here could stall the
            // audio, so the memory is abandoned instead.
            core::mem::forget(engine);
        }
    }

    fn take_arrivals(&mut self) {
        // A crossfade under way runs to its end first, so that the output
        // never jumps; whatever arrived meanwhile waits in the queue.
        if self.fading.is_some() {
            return;
        }
        // Of several engines that arrived together only the newest plays.
        // The loader cleared every engine before sending it.
        let mut newest: Option<Swap<S>> = None;
        while let Some(swap) = self.incoming.pop() {
            if let Some(skipped) = newest.replace(swap) {
                self.retire(skipped.engine);
            }
        }
        let Some(Swap { engine, crossfade }) = newest else {
            return;
        };
        if let Some(previous) = self.current.replace(engine) {
            if crossfade > 0 && self.scratch.num_channels() > 0 {
                self.fading = Some(previous);
                self.fade_len = crossfade;
                self.fade_position = 0;
            } else {
                self.retire(previous);
            }
        }
    }

    fn crossfade(&mut self, buffer: &mut AudioBuffer<S>) {
        let (Some(current), Some(fading)) = (self.current.as_mut(), self.fading.as_mut()) else {
            return;
        };
        let channels = buffer.num_channels().min(self.scratch.num_channels());
        let chunk = self.scratch.capacity();
        let frames = buffer.num_frames();
        let mut start = 0;
        while start < frames && self.fade_position < self.fade_len {
            let count = chunk.min(frames - start);
            self.scratch.set_num_frames(count);
            for channel in 0..channels {
                self.scratch
                    .channel_mut(channel)
                    .copy_from_slice(&buffer.channel(channel)[start..start + count]);
            }
            // Channels the block lacks are silent inputs, not what the
            // previous chunk left in the copy.
            for channel in channels..self.scratch.num_channels() {
                self.scratch.channel_mut(channel).fill(S::ZERO);
            }
            fading.process_block(&mut self.scratch);
            let (fade_len, fade_position) = (self.fade_len, self.fade_position);
            buffer.windowed(start..start + count, |piece| {
                current.process_block(piece);
                for channel in 0..channels {
                    let old = self.scratch.channel(channel);
                    for (index, (sample, &old)) in
                        piece.channel_mut(channel).iter_mut().zip(old).enumerate()
                    {
                        let step = fade_position + index + 1;
                        if step < fade_len {
                            let gain = S::from_f64(step as f64 / fade_len as f64);
                            *sample = old * (S::ONE - gain) + *sample * gain;
                        }
                    }
                }
            });
            self.fade_position = (fade_position + count).min(fade_len);
            start += count;
        }
        if start < frames {
            buffer.windowed(start..frames, |rest| current.process_block(rest));
        }
        if self.fade_position >= self.fade_len {
            if let Some(done) = self.fading.take() {
                self.retire(done);
            }
        }
    }
}

impl<S: Sample> Processor<S> for Convolution<S> {
    /// Reserves the crossfade copy and clears the engines. Ends a crossfade
    /// under way.
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.scratch = AudioBuffer::new(spec.num_channels, spec.max_block_size);
        if let Some(done) = self.fading.take() {
            self.retire(done);
        }
        if let Some(engine) = self.current.as_mut() {
            engine.clear();
        }
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        self.take_arrivals();
        if self.fading.is_some() {
            self.crossfade(buffer);
        } else if let Some(engine) = self.current.as_mut() {
            engine.process_block(buffer);
        }
    }

    /// Clears the engine and ends a crossfade under way.
    fn reset(&mut self) {
        if let Some(done) = self.fading.take() {
            self.retire(done);
        }
        if let Some(engine) = self.current.as_mut() {
            engine.clear();
        }
    }

    fn latency_samples(&self) -> usize {
        self.current.as_ref().map_or(0, |engine| engine.latency())
    }
}

impl<S: Sample> ConvolutionLoader<S> {
    /// Sends `engine` to replace the one playing, at once.
    ///
    /// # Errors
    ///
    /// Hands the engine back, boxed, if too many engines have been sent and
    /// not collected; call [`collect`](Self::collect) after the audio thread
    /// has run, and try again.
    pub fn load(&mut self, engine: ConvolutionEngine<S>) -> Result<(), Box<ConvolutionEngine<S>>> {
        self.load_with_crossfade(engine, 0)
    }

    /// Sends `engine` to replace the one playing, crossfading over
    /// `crossfade` samples. Zero swaps at once. If a crossfade is under way
    /// when it arrives, it waits for that to end.
    ///
    /// The engine is cleared here, before it is sent, so an engine that has
    /// already processed audio starts from silence, and the audio thread
    /// does not spend time clearing it.
    ///
    /// # Errors
    ///
    /// As for [`load`](Self::load).
    pub fn load_with_crossfade(
        &mut self,
        engine: ConvolutionEngine<S>,
        crossfade: usize,
    ) -> Result<(), Box<ConvolutionEngine<S>>> {
        self.collect();
        let latency = engine.latency();
        let mut engine = Box::new(engine);
        if self.in_flight >= self.limit {
            return Err(engine);
        }
        engine.clear();
        match self.outgoing.push(Swap { engine, crossfade }) {
            Ok(()) => {
                self.in_flight += 1;
                self.latency = Some(latency);
                Ok(())
            }
            Err(swap) => Err(swap.engine),
        }
    }

    /// The latency of the engine sent last: what
    /// [`latency_samples`](Processor::latency_samples) of the [`Convolution`]
    /// reports once that engine has arrived. `None` until an engine is sent.
    ///
    /// A plugin that may swap to an engine of another latency compares this
    /// with what it last told the host, and tells the host again when it
    /// differs.
    pub fn latency(&self) -> Option<usize> {
        self.latency
    }

    /// Drops the engines the audio thread has finished with, and returns how
    /// many there were.
    pub fn collect(&mut self) -> usize {
        let mut count = 0;
        while let Some(engine) = self.retired.pop() {
            drop(engine);
            count += 1;
        }
        self.in_flight = self.in_flight.saturating_sub(count);
        count
    }

    /// Engines sent and not yet collected, including the one playing.
    pub fn in_flight(&self) -> usize {
        self.in_flight
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Random;

    fn random_signal(len: usize, seed: u64) -> Vec<f64> {
        let mut random = Random::new(seed);
        (0..len).map(|_| random.between(-1.0, 1.0)).collect()
    }

    /// A random response that decays, like a room.
    fn random_response(len: usize, seed: u64) -> Vec<f64> {
        let mut random = Random::new(seed);
        (0..len)
            .map(|n| random.between(-1.0, 1.0) * (-3.0 * n as f64 / len as f64).exp())
            .collect()
    }

    /// Direct convolution at one output frame.
    fn direct_at(signal: &[f64], response: &[f64], frame: usize) -> f64 {
        let first = frame.saturating_sub(signal.len() - 1);
        let last = frame.min(response.len() - 1);
        if first > last {
            return 0.0;
        }
        (first..=last)
            .map(|k| response[k] * signal[frame - k])
            .sum()
    }

    fn build_engine<S: Sample>(
        responses: &[Vec<f64>],
        channels: usize,
        routing: ConvolutionRouting,
        settings: ConvolutionSettings,
    ) -> ConvolutionEngine<S> {
        let converted: Vec<Vec<S>> = responses
            .iter()
            .map(|response| response.iter().map(|&s| S::from_f64(s)).collect())
            .collect();
        ConvolutionEngine::new(&converted, channels, routing, settings).unwrap()
    }

    fn to_buffer<S: Sample>(channels: &[Vec<f64>]) -> AudioBuffer<S> {
        let mut buffer = AudioBuffer::new(channels.len(), channels[0].len());
        for (index, channel) in channels.iter().enumerate() {
            for (sample, &value) in buffer.channel_mut(index).iter_mut().zip(channel) {
                *sample = S::from_f64(value);
            }
        }
        buffer
    }

    /// Runs `buffer` through `processor` in blocks whose lengths cycle
    /// through `pattern`.
    fn run_pattern<S: Sample, P: Processor<S>>(
        processor: &mut P,
        buffer: &mut AudioBuffer<S>,
        pattern: &[usize],
    ) {
        let frames = buffer.num_frames();
        let mut start = 0;
        for &length in pattern.iter().cycle() {
            if start >= frames {
                break;
            }
            let end = (start + length).min(frames);
            buffer.windowed(start..end, |piece| processor.process(piece));
            start = end;
        }
    }

    /// The largest error against direct convolution, relative to the RMS of
    /// the reference, checked at `frames` (or every frame when `None`).
    fn relative_error<S: Sample>(
        len: usize,
        settings: ConvolutionSettings,
        seed: u64,
        checked: Option<usize>,
    ) -> f64 {
        let response = random_response(len, seed);
        let frames = len + 3 * settings.partition_size() + 100;
        let signal = random_signal(frames, seed + 1);
        let mut engine = build_engine::<S>(
            core::slice::from_ref(&response),
            1,
            ConvolutionRouting::Parallel,
            settings,
        );
        let mut buffer = to_buffer::<S>(core::slice::from_ref(&signal));
        run_pattern(&mut engine, &mut buffer, &[61, 1, 300]);
        let latency = engine.latency();

        let frames_to_check: Vec<usize> = match checked {
            None => (latency..frames).collect(),
            Some(count) => {
                let mut random = Random::new(seed + 2);
                let mut picks: Vec<usize> = (0..count)
                    .map(|_| latency + (random.unit() * (frames - latency) as f64) as usize)
                    .collect();
                picks.push(frames - 1);
                picks.push(latency + len - 1);
                picks
            }
        };
        let (mut worst, mut power) = (0.0f64, 0.0f64);
        for &frame in &frames_to_check {
            let expected = direct_at(&signal, &response, frame - latency);
            let got = buffer.channel(0)[frame].to_f64();
            worst = worst.max((got - expected).abs());
            power += expected * expected;
        }
        // Before the latency has passed, the output is silent.
        for frame in 0..latency {
            assert_eq!(buffer.channel(0)[frame].to_f64(), 0.0);
        }
        worst / (power / frames_to_check.len() as f64).sqrt()
    }

    const CASES: [(usize, usize); 14] = [
        (1, 1),
        (1, 64),
        (2, 1),
        (3, 2),
        (7, 4),
        (64, 64),
        (127, 64),
        (128, 128),
        (129, 128),
        (1_000, 32),
        (4_097, 2_048),
        (20_000, 128),
        (48_000, 256),
        (100_000, 512),
    ];

    #[test]
    fn matches_direct_convolution_from_one_to_a_hundred_thousand_samples() {
        let mut worst32 = 0.0f64;
        let mut worst64 = 0.0f64;
        for (case, &(len, partition)) in CASES.iter().enumerate() {
            let checked = if len > 3_000 { Some(400) } else { None };
            for zero_latency in [true, false] {
                let settings = ConvolutionSettings::new()
                    .with_partition_size(partition)
                    .with_zero_latency(zero_latency);
                let seed = 10 * case as u64 + u64::from(zero_latency);
                let error32 = relative_error::<f32>(len, settings, seed, checked);
                assert!(
                    error32 < 2e-6,
                    "f32, {len} taps, {settings:?}: error {error32:e}"
                );
                worst32 = worst32.max(error32);
                // Double precision costs twice the time of an unoptimised
                // test build, so it stops at 20 000 taps.
                if len <= 20_000 {
                    let error64 = relative_error::<f64>(len, settings, seed, checked);
                    assert!(
                        error64 < 1e-13,
                        "f64, {len} taps, {settings:?}: error {error64:e}"
                    );
                    worst64 = worst64.max(error64);
                }
            }
        }
        eprintln!("largest relative error: f32 {worst32:e}, f64 {worst64:e}");
    }

    /// Response length, first partition and largest partition, for the
    /// non-uniform scheme: short and long responses, a scheme of one band,
    /// and one where the largest size never engages.
    const NUP_CASES: [(usize, usize, usize); 8] = [
        (1, 1, 2),
        (700, 64, 256),
        (1_000, 32, 1_024),
        (4_097, 64, 512),
        (20_000, 64, 2_048),
        (48_000, 128, 4_096),
        (100_000, 64, 8_192),
        (144_000, 64, 16_384),
    ];

    #[test]
    fn non_uniform_matches_direct_convolution() {
        let mut worst32 = 0.0f64;
        let mut worst64 = 0.0f64;
        for (case, &(len, partition, largest)) in NUP_CASES.iter().enumerate() {
            let checked = if len > 3_000 { Some(400) } else { None };
            for zero_latency in [true, false] {
                let settings = ConvolutionSettings::new()
                    .with_partition_size(partition)
                    .with_non_uniform_partitions(largest)
                    .with_zero_latency(zero_latency);
                let seed = 200 * case as u64 + u64::from(zero_latency);
                let error32 = relative_error::<f32>(len, settings, seed, checked);
                assert!(
                    error32 < 2e-6,
                    "f32, {len} taps, {settings:?}: error {error32:e}"
                );
                worst32 = worst32.max(error32);
                if len <= 20_000 {
                    let error64 = relative_error::<f64>(len, settings, seed, checked);
                    assert!(
                        error64 < 1e-13,
                        "f64, {len} taps, {settings:?}: error {error64:e}"
                    );
                    worst64 = worst64.max(error64);
                }
            }
        }
        eprintln!("largest non-uniform relative error: f32 {worst32:e}, f64 {worst64:e}");
    }

    /// The two schemes convolve the same response differently, so their
    /// sums round differently; each stays within its measured distance of
    /// direct convolution, and so of the other.
    #[test]
    fn non_uniform_stays_within_the_same_distance_of_uniform() {
        let response = random_response(40_000, 41);
        let frames = 50_000;
        let signal = random_signal(frames, 42);
        let mut outputs = Vec::new();
        for largest in [128, 8_192] {
            let settings = ConvolutionSettings::new()
                .with_partition_size(128)
                .with_non_uniform_partitions(largest);
            let mut engine = build_engine::<f32>(
                core::slice::from_ref(&response),
                1,
                ConvolutionRouting::Parallel,
                settings,
            );
            let mut buffer = to_buffer::<f32>(core::slice::from_ref(&signal));
            run_pattern(&mut engine, &mut buffer, &[61, 1, 300]);
            outputs.push(buffer.channel(0).to_vec());
        }
        let mut power = 0.0f64;
        let mut worst = 0.0f64;
        for (&uniform, &non_uniform) in outputs[0].iter().zip(&outputs[1]) {
            worst = worst.max((uniform - non_uniform).abs() as f64);
            power += uniform as f64 * uniform as f64;
        }
        let rms = (power / frames as f64).sqrt();
        assert!(worst / rms < 4e-6, "the schemes differ by {}", worst / rms);
    }

    #[test]
    fn non_uniform_output_does_not_depend_on_block_sizes() {
        let patterns: [&[usize]; 6] = [
            &[1],
            &[64],
            &[128],
            &[7, 300, 1, 33, 129],
            &[1_000],
            &[4_096],
        ];
        let responses = [
            random_response(30_000, 1),
            random_response(25_000, 2),
            random_response(30_000, 3),
            random_response(10, 4),
        ];
        let input = [random_signal(40_000, 5), random_signal(40_000, 6)];
        for zero_latency in [true, false] {
            for routing in [ConvolutionRouting::Parallel, ConvolutionRouting::TrueStereo] {
                let settings = ConvolutionSettings::new()
                    .with_partition_size(128)
                    .with_non_uniform_partitions(4_096)
                    .with_zero_latency(zero_latency);
                let used = match routing {
                    ConvolutionRouting::Parallel => &responses[..2],
                    ConvolutionRouting::TrueStereo => &responses[..],
                };
                let mut reference = None;
                for pattern in patterns {
                    let mut engine = build_engine::<f32>(used, 2, routing, settings);
                    let mut buffer = to_buffer::<f32>(&input);
                    run_pattern(&mut engine, &mut buffer, pattern);
                    let bits: Vec<u32> = buffer
                        .iter_channels()
                        .flat_map(|channel| channel.iter().map(|s| s.to_bits()))
                        .collect();
                    match &reference {
                        None => reference = Some(bits),
                        Some(expected) => assert!(
                            *expected == bits,
                            "blocks of {pattern:?} changed the output ({routing:?}, {settings:?})"
                        ),
                    }
                }
            }
        }
    }

    #[test]
    fn non_uniform_latency_is_none_or_one_partition() {
        for (zero_latency, expected) in [(true, 0), (false, 256)] {
            let settings = ConvolutionSettings::new()
                .with_partition_size(256)
                .with_non_uniform_partitions(2_048)
                .with_zero_latency(zero_latency);
            // Long enough that the 2 048 band is built; a single tap at 2.
            let mut taps = vec![0.0; 4_096];
            taps[2] = 1.0;
            let response = vec![taps];
            let mut engine =
                build_engine::<f64>(&response, 1, ConvolutionRouting::Parallel, settings);
            assert_eq!(engine.latency(), expected);
            assert_eq!(engine.latency_samples(), expected);
            assert_eq!(engine.largest_partition_size(), 2_048);
            let mut buffer = AudioBuffer::<f64>::new(1, 600);
            buffer.channel_mut(0)[0] = 1.0;
            engine.process(&mut buffer);
            for (frame, &sample) in buffer.channel(0).iter().enumerate() {
                let wanted = if frame == expected + 2 { 1.0 } else { 0.0 };
                assert!((sample - wanted).abs() < 1e-15, "frame {frame}: {sample}");
            }
        }
    }

    /// A NaN at the input lives in a band's state for at most twice its
    /// partition and the response's length; it does not poison the engine.
    #[test]
    fn non_uniform_recovers_from_nan() {
        let response = random_response(20_000, 51);
        let settings = ConvolutionSettings::new()
            .with_partition_size(64)
            .with_non_uniform_partitions(2_048);
        let mut engine = build_engine::<f32>(
            core::slice::from_ref(&response),
            1,
            ConvolutionRouting::Parallel,
            settings,
        );
        let mut spoiled = AudioBuffer::<f32>::new(1, 2_048);
        spoiled.channel_mut(0)[0] = f32::NAN;
        engine.process(&mut spoiled);
        assert!(spoiled.channel(0).iter().any(|s| s.is_nan()));

        // A spectrum holding the NaN stays in a band's ring for the
        // band's own `partitions` periods, on top of twice the partition
        // it took to wash out of the window: the largest band asks for
        // about (9 + 3) * 2 048 silent samples here. Sixteen periods is
        // generous, and keeps every boundary where a fresh engine's is.
        let mut silence = constant(16 * 2_048, 0.0);
        engine.process(&mut silence);
        // The last full period of the largest band is clean.
        assert!(
            silence.channel(0)[16 * 2_048 - 2_048..]
                .iter()
                .all(|&s| s == 0.0)
        );

        // From there the engine is indistinguishable from a fresh one.
        let signal = random_signal(30_000, 52);
        let mut buffer = to_buffer::<f32>(core::slice::from_ref(&signal));
        run_pattern(&mut engine, &mut buffer, &[61, 1, 300]);
        let mut fresh = build_engine::<f32>(&[response], 1, ConvolutionRouting::Parallel, settings);
        let mut reference = to_buffer::<f32>(core::slice::from_ref(&signal));
        run_pattern(&mut fresh, &mut reference, &[61, 1, 300]);
        assert_eq!(buffer.channel(0), reference.channel(0));
    }

    /// A crossfade between non-uniform engines mixes each one's own output,
    /// the old one's tail included, as it does between uniform engines.
    #[test]
    fn a_crossfade_between_non_uniform_engines_mixes_their_outputs() {
        let settings = ConvolutionSettings::new()
            .with_partition_size(64)
            .with_non_uniform_partitions(2_048);
        let responses = [random_response(10_000, 61), random_response(8_000, 62)];
        let engine = |response: &Vec<f64>| {
            build_engine::<f32>(
                core::slice::from_ref(response),
                1,
                ConvolutionRouting::Parallel,
                settings,
            )
        };
        let (mut convolution, mut loader) = Convolution::<f32>::from_engine(engine(&responses[0]));
        convolution.prepare(&ProcessSpec {
            sample_rate: 48_000.0,
            max_block_size: 64,
            num_channels: 1,
        });

        // A few blocks on the first engine alone, so its tail sounds under
        // the fade; the swap is taken at the start of the next block.
        let signal = random_signal(1_111, 63);
        let mut out = Vec::new();
        let mut played = 0;
        let mut run = |convolution: &mut Convolution<f32>, frames: usize| {
            let mut buffer = AudioBuffer::<f32>::new(1, frames);
            for (sample, &value) in buffer
                .channel_mut(0)
                .iter_mut()
                .zip(&signal[played..played + frames])
            {
                *sample = value as f32;
            }
            convolution.process(&mut buffer);
            played += frames;
            out.extend_from_slice(buffer.channel(0));
        };
        run(&mut convolution, 50);
        run(&mut convolution, 64);
        loader
            .load_with_crossfade(engine(&responses[1]), 200)
            .unwrap();
        run(&mut convolution, 7);
        run(&mut convolution, 100);
        run(&mut convolution, 90);
        run(&mut convolution, 800);
        assert_eq!(played, signal.len());

        // From index 114 on, for 200 samples, the output is the old
        // engine's fading into the new one's; the new one started from
        // silence when it took over.
        for (index, &sample) in out.iter().enumerate() {
            let old = direct_at(&signal, &responses[0], index);
            let expected = if index < 114 {
                old
            } else {
                let new = direct_at(&signal[114..], &responses[1], index - 114);
                let step = index - 114 + 1;
                if step < 200 {
                    let gain = step as f64 / 200.0;
                    old * (1.0 - gain) + new * gain
                } else {
                    new
                }
            };
            assert!(
                (sample as f64 - expected).abs() < 1e-4,
                "frame {index}: {sample} vs {expected}"
            );
        }
        assert!(!convolution.is_crossfading());
        assert_eq!(loader.collect(), 1);
    }

    /// Every band starts half a period late, so two bands never do their
    /// boundary work in the same block of `partition_size` samples, and
    /// the inverse transform always lands in the block after the boundary.
    #[test]
    fn band_boundaries_are_staggered_and_the_inverse_is_deferred() {
        // Response long enough for five bands: 256 to 4 096.
        let response = random_response(8_192, 71);
        let settings = ConvolutionSettings::new()
            .with_partition_size(128)
            .with_non_uniform_partitions(4_096);
        let mut engine =
            build_engine::<f64>(&[response], 1, ConvolutionRouting::Parallel, settings);
        assert_eq!(engine.bands.len(), 5);
        for band in &engine.bands {
            assert_eq!(band.fill, band.size / 2, "band of {}", band.size);
        }
        let mut due_blocks = 0;
        for step in 0..64 {
            let mut block = AudioBuffer::<f64>::new(1, 128);
            block.channel_mut(0)[0] = 0.5;
            engine.process(&mut block);
            let boundaries = engine.bands.iter().filter(|band| band.fill == 0).count();
            assert!(
                boundaries <= 1,
                "{boundaries} bands bounded in block {step}"
            );
            let dues = engine.bands.iter().filter(|band| band.inverse_due).count();
            // The band that bounded at this block's end owes its inverse
            // to the next block; at most one can.
            assert!(dues <= 1, "{dues} inverses due after block {step}");
            due_blocks += usize::from(boundaries == 0);
        }
        assert!(due_blocks > 0, "no band ever bounded");
    }

    #[test]
    fn the_output_does_not_depend_on_block_sizes() {
        let patterns: [&[usize]; 6] = [
            &[1],
            &[64],
            &[128],
            &[7, 300, 1, 33, 129],
            &[1_000],
            &[127, 2],
        ];
        let responses = [
            random_response(3_000, 1),
            random_response(2_500, 2),
            random_response(3_000, 3),
            random_response(10, 4),
        ];
        let input = [random_signal(6_000, 5), random_signal(6_000, 6)];
        for zero_latency in [true, false] {
            for routing in [ConvolutionRouting::Parallel, ConvolutionRouting::TrueStereo] {
                let settings = ConvolutionSettings::new()
                    .with_partition_size(128)
                    .with_zero_latency(zero_latency);
                let used = match routing {
                    ConvolutionRouting::Parallel => &responses[..2],
                    ConvolutionRouting::TrueStereo => &responses[..],
                };
                let mut reference = None;
                for pattern in patterns {
                    let mut engine = build_engine::<f32>(used, 2, routing, settings);
                    let mut buffer = to_buffer::<f32>(&input);
                    run_pattern(&mut engine, &mut buffer, pattern);
                    let bits: Vec<u32> = buffer
                        .iter_channels()
                        .flat_map(|channel| channel.iter().map(|s| s.to_bits()))
                        .collect();
                    match &reference {
                        None => reference = Some(bits),
                        Some(expected) => assert!(
                            *expected == bits,
                            "blocks of {pattern:?} changed the output ({routing:?}, {settings:?})"
                        ),
                    }
                }
            }
        }
    }

    #[test]
    fn latency_is_none_or_one_partition() {
        for (zero_latency, expected) in [(true, 0), (false, 256)] {
            let settings = ConvolutionSettings::new()
                .with_partition_size(256)
                .with_zero_latency(zero_latency);
            let response = vec![vec![0.0, 0.0, 1.0]];
            let mut engine =
                build_engine::<f64>(&response, 1, ConvolutionRouting::Parallel, settings);
            assert_eq!(engine.latency(), expected);
            assert_eq!(engine.latency_samples(), expected);
            let mut buffer = AudioBuffer::<f64>::new(1, 600);
            buffer.channel_mut(0)[0] = 1.0;
            engine.process(&mut buffer);
            for (frame, &sample) in buffer.channel(0).iter().enumerate() {
                let wanted = if frame == expected + 2 { 1.0 } else { 0.0 };
                assert!((sample - wanted).abs() < 1e-15, "frame {frame}: {sample}");
            }
        }
    }

    #[test]
    fn true_stereo_adds_four_paths() {
        let responses = [
            random_response(700, 11),
            random_response(300, 12),
            random_response(500, 13),
            random_response(900, 14),
        ];
        let input = [random_signal(2_000, 15), random_signal(2_000, 16)];
        let settings = ConvolutionSettings::new().with_partition_size(64);
        let mut engine =
            build_engine::<f64>(&responses, 2, ConvolutionRouting::TrueStereo, settings);
        let mut buffer = to_buffer::<f64>(&input);
        run_pattern(&mut engine, &mut buffer, &[100, 3]);
        for frame in 0..2_000 {
            let left = direct_at(&input[0], &responses[0], frame)
                + direct_at(&input[1], &responses[2], frame);
            let right = direct_at(&input[0], &responses[1], frame)
                + direct_at(&input[1], &responses[3], frame);
            assert!((buffer.channel(0)[frame] - left).abs() < 1e-11, "{frame}");
            assert!((buffer.channel(1)[frame] - right).abs() < 1e-11, "{frame}");
        }
    }

    #[test]
    fn one_response_serves_every_channel_and_one_per_channel_keeps_them_apart() {
        let input = [
            random_signal(500, 21),
            random_signal(500, 22),
            random_signal(500, 23),
        ];
        let settings = ConvolutionSettings::new().with_partition_size(32);

        let shared = [random_response(200, 24)];
        let mut engine = build_engine::<f64>(&shared, 3, ConvolutionRouting::Parallel, settings);
        let mut buffer = to_buffer::<f64>(&input);
        engine.process(&mut buffer);
        for (channel, signal) in input.iter().enumerate() {
            for frame in 0..500 {
                let expected = direct_at(signal, &shared[0], frame);
                assert!((buffer.channel(channel)[frame] - expected).abs() < 1e-12);
            }
        }

        let own = [
            random_response(200, 25),
            random_response(50, 26),
            random_response(1, 27),
        ];
        let mut engine = build_engine::<f64>(&own, 3, ConvolutionRouting::Parallel, settings);
        let mut buffer = to_buffer::<f64>(&input);
        engine.process(&mut buffer);
        for (channel, signal) in input.iter().enumerate() {
            for frame in 0..500 {
                let expected = direct_at(signal, &own[channel], frame);
                assert!((buffer.channel(channel)[frame] - expected).abs() < 1e-12);
            }
        }
    }

    #[test]
    fn missing_channels_are_silent_and_extra_channels_are_left_alone() {
        let responses = [vec![1.0], vec![0.0], vec![0.0], vec![1.0]];
        let mut engine = build_engine::<f64>(
            &responses,
            2,
            ConvolutionRouting::TrueStereo,
            ConvolutionSettings::new(),
        );
        // One channel only: the left output, with a silent right input.
        let mut mono = AudioBuffer::<f64>::new(1, 4);
        mono.channel_mut(0).fill(0.5);
        engine.process(&mut mono);
        assert_eq!(mono.channel(0), &[0.5; 4]);

        // A third channel is not the engine's.
        let swapped = [vec![0.0], vec![1.0], vec![1.0], vec![0.0]];
        let mut engine = build_engine::<f64>(
            &swapped,
            2,
            ConvolutionRouting::TrueStereo,
            ConvolutionSettings::new(),
        );
        let mut wide = AudioBuffer::<f64>::new(3, 4);
        wide.channel_mut(0).fill(1.0);
        wide.channel_mut(1).fill(2.0);
        wide.channel_mut(2).fill(3.0);
        engine.process(&mut wide);
        assert_eq!(wide.channel(0), &[2.0; 4]);
        assert_eq!(wide.channel(1), &[1.0; 4]);
        assert_eq!(wide.channel(2), &[3.0; 4]);

        // An empty block changes nothing.
        let mut empty = AudioBuffer::<f64>::new(2, 0);
        engine.process(&mut empty);
    }

    #[test]
    fn bad_requests_are_refused() {
        let ok = ConvolutionSettings::new();
        let parallel = ConvolutionRouting::Parallel;
        let stereo = ConvolutionRouting::TrueStereo;
        let one = [vec![1.0f32]];
        let build = |responses: &[Vec<f32>], channels, routing, settings| {
            ConvolutionEngine::<f32>::new(responses, channels, routing, settings).map(|_| ())
        };
        assert_eq!(
            build(&[], 1, parallel, ok),
            Err(ConvolutionError::ChannelCount {
                responses: 0,
                channels: 1
            })
        );
        assert_eq!(
            build(&[vec![], vec![]], 2, parallel, ok),
            Err(ConvolutionError::EmptyResponse)
        );
        assert!(matches!(
            build(&one, 0, parallel, ok),
            Err(ConvolutionError::ChannelCount { .. })
        ));
        assert!(matches!(
            build(&[vec![1.0], vec![1.0]], 3, parallel, ok),
            Err(ConvolutionError::ChannelCount { .. })
        ));
        assert!(matches!(
            build(&one, 2, stereo, ok),
            Err(ConvolutionError::ChannelCount { .. })
        ));
        for size in [0, 3, 100, MAX_PARTITION_SIZE * 2] {
            assert_eq!(
                build(&one, 1, parallel, ok.with_partition_size(size)),
                Err(ConvolutionError::PartitionSize(size))
            );
        }
        // A largest partition that is not a power of two at least twice
        // the first, or is too big, is refused; one equal to the first is
        // the uniform scheme.
        for largest in [64, 192, 384, MAX_PARTITION_SIZE * 2] {
            assert_eq!(
                build(&one, 1, parallel, ok.with_non_uniform_partitions(largest)),
                Err(ConvolutionError::PartitionSize(largest)),
                "largest {largest}"
            );
        }
        assert_eq!(
            build(&one, 1, parallel, ok.with_non_uniform_partitions(128)),
            Ok(())
        );
        assert_eq!(
            build(&one, 1, parallel, ok.with_non_uniform_partitions(256)),
            Ok(())
        );
        for bad in [f32::NAN, f32::INFINITY] {
            assert_eq!(
                build(&[vec![0.5, bad]], 1, parallel, ok),
                Err(ConvolutionError::NotFinite)
            );
        }
        // An empty channel beside a full one is padded, not refused.
        assert_eq!(build(&[vec![], vec![1.0]], 2, parallel, ok), Ok(()));
        assert_eq!(build(&one, 1, parallel, ok.with_partition_size(1)), Ok(()));
        assert!(!ConvolutionError::NotFinite.to_string().is_empty());
    }

    fn gain_engine(gain: f32) -> ConvolutionEngine<f32> {
        ConvolutionEngine::new(
            &[[gain]],
            1,
            ConvolutionRouting::Parallel,
            ConvolutionSettings::new(),
        )
        .unwrap()
    }

    fn constant(frames: usize, value: f32) -> AudioBuffer<f32> {
        let mut buffer = AudioBuffer::new(1, frames);
        buffer.channel_mut(0).fill(value);
        buffer
    }

    const SPEC: ProcessSpec = ProcessSpec {
        sample_rate: 48_000.0,
        max_block_size: 64,
        num_channels: 1,
    };

    #[test]
    fn with_no_engine_audio_passes_through() {
        let (mut convolution, _loader) = Convolution::<f32>::new();
        convolution.prepare(&SPEC);
        let mut buffer = constant(16, 0.25);
        convolution.process(&mut buffer);
        assert_eq!(buffer.channel(0), &[0.25; 16]);
        assert_eq!(convolution.latency_samples(), 0);
        assert!(convolution.engine().is_none());
    }

    #[test]
    fn a_new_engine_takes_over_at_the_next_block_and_the_old_one_comes_back() {
        let (mut convolution, mut loader) = Convolution::<f32>::from_engine(gain_engine(2.0));
        convolution.prepare(&SPEC);
        let mut buffer = constant(8, 1.0);
        convolution.process(&mut buffer);
        assert_eq!(buffer.channel(0), &[2.0; 8]);

        loader.load(gain_engine(3.0)).unwrap();
        assert_eq!(loader.in_flight(), 2);
        let mut buffer = constant(8, 1.0);
        convolution.process(&mut buffer);
        assert_eq!(buffer.channel(0), &[3.0; 8]);
        assert!(!convolution.is_crossfading());

        // The engine from `from_engine` came back; the new one is playing.
        assert_eq!(loader.collect(), 1);
        assert_eq!(loader.in_flight(), 1);

        loader.load(gain_engine(4.0)).unwrap();
        convolution.process(&mut constant(8, 1.0));
        assert_eq!(loader.collect(), 1);
        assert_eq!(loader.in_flight(), 1);
    }

    #[test]
    fn a_crossfade_is_linear_and_lasts_as_long_as_asked() {
        let (mut convolution, mut loader) = Convolution::<f32>::from_engine(gain_engine(1.0));
        convolution.prepare(&SPEC);
        loader.load_with_crossfade(gain_engine(0.5), 100).unwrap();

        // 250 frames in uneven blocks, some longer than the reserved copy.
        let mut out = Vec::new();
        for frames in [30, 64, 7, 200, 1] {
            let mut buffer = constant(frames, 1.0);
            convolution.process(&mut buffer);
            out.extend_from_slice(buffer.channel(0));
        }
        for (index, &sample) in out.iter().enumerate() {
            let step = index + 1;
            let expected = if step < 100 {
                let gain = (step as f64 / 100.0) as f32;
                1.0 * (1.0 - gain) + 0.5 * gain
            } else {
                0.5
            };
            assert_eq!(sample, expected, "frame {index}");
        }
        assert!(!convolution.is_crossfading());
        assert_eq!(loader.collect(), 1);
    }

    /// The output of a linear crossfade from gain `from` to gain `to` over
    /// `len` samples, `index` samples in, for an input of 1.
    fn faded(from: f32, to: f32, len: usize, index: usize) -> f32 {
        let step = index + 1;
        if step < len {
            let gain = (step as f64 / len as f64) as f32;
            from * (1.0 - gain) + to * gain
        } else {
            to
        }
    }

    #[test]
    fn engines_arriving_during_a_crossfade_wait_for_it_to_end() {
        let (mut convolution, mut loader) = Convolution::<f32>::from_engine(gain_engine(1.0));
        convolution.prepare(&SPEC);
        loader.load_with_crossfade(gain_engine(2.0), 1_000).unwrap();
        let mut out = Vec::new();
        let mut buffer = constant(10, 1.0);
        convolution.process(&mut buffer);
        out.extend_from_slice(buffer.channel(0));
        assert!(convolution.is_crossfading());

        // Two more arrive mid-fade; only the newest will play.
        loader.load_with_crossfade(gain_engine(3.0), 50).unwrap();
        loader.load_with_crossfade(gain_engine(4.0), 4).unwrap();
        while out.len() < 1_020 {
            let mut buffer = constant(10, 1.0);
            convolution.process(&mut buffer);
            out.extend_from_slice(buffer.channel(0));
        }
        for (index, &sample) in out.iter().enumerate() {
            let expected = if index < 1_000 {
                faded(1.0, 2.0, 1_000, index)
            } else {
                faded(2.0, 4.0, 4, index - 1_000)
            };
            assert_eq!(sample, expected, "frame {index}");
        }
        // The first engine, the second after its fade, and the third,
        // never played.
        assert_eq!(loader.collect(), 3);
        assert_eq!(loader.in_flight(), 1);
    }

    #[test]
    fn an_engine_arriving_mid_crossfade_does_not_click() {
        let (mut convolution, mut loader) = Convolution::<f32>::from_engine(gain_engine(1.0));
        convolution.prepare(&SPEC);
        loader.load_with_crossfade(gain_engine(2.0), 1_000).unwrap();
        let mut first = constant(10, 1.0);
        convolution.process(&mut first);
        let mut previous = first.channel(0)[9];
        assert!((previous - 1.01).abs() < 1e-3, "{previous}");

        // A four-sample fade to 4: one step of it is at most (4 - 1) / 4.
        loader.load_with_crossfade(gain_engine(4.0), 4).unwrap();
        let largest_step = (4.0 - 1.0) / 4.0 + 1e-3;
        for _ in 0..200 {
            let mut block = constant(6, 1.0);
            convolution.process(&mut block);
            for &sample in block.channel(0) {
                assert!(
                    (sample - previous).abs() <= largest_step,
                    "the output jumped from {previous} to {sample}"
                );
                previous = sample;
            }
        }
        assert_eq!(previous, 4.0);
    }

    #[test]
    fn a_crossfade_treats_channels_the_block_lacks_as_silent() {
        // Both outputs are the right input. Processing is in place, so after
        // a stereo block the copy's right channel holds the right output.
        let cross = ConvolutionEngine::new(
            &[[0.0f32], [0.0], [1.0], [1.0]],
            2,
            ConvolutionRouting::TrueStereo,
            ConvolutionSettings::new(),
        )
        .unwrap();
        // On its own, a mono block gives a silent left output.
        let mut alone = cross.clone();
        let mut mono = constant(8, 0.0);
        alone.process(&mut mono);
        assert_eq!(mono.channel(0), &[0.0; 8]);

        let (mut convolution, mut loader) = Convolution::<f32>::from_engine(cross);
        convolution.prepare(&ProcessSpec {
            num_channels: 2,
            ..SPEC
        });
        // The new engine is silent, so the output is only the faded old one.
        loader.load_with_crossfade(gain_engine(0.0), 1_000).unwrap();

        let mut stereo = AudioBuffer::<f32>::new(2, 8);
        stereo.channel_mut(1).fill(1.0);
        convolution.process(&mut stereo);
        assert!(convolution.is_crossfading());
        // The left output is the old engine's alone (the new one is silent).
        for index in 0..8 {
            assert_eq!(stereo.channel(0)[index], faded(1.0, 0.0, 1_000, index));
        }

        // Now a mono block: the old engine's right input is silent.
        let mut mono = constant(8, 0.0);
        convolution.process(&mut mono);
        assert_eq!(mono.channel(0), &[0.0; 8], "a stale right input leaked in");
    }

    #[test]
    fn an_engine_that_has_played_starts_from_silence_when_loaded() {
        let mut echo = ConvolutionEngine::new(
            &[vec![0.0f32, 0.0, 0.0, 1.0]],
            1,
            ConvolutionRouting::Parallel,
            ConvolutionSettings::new().with_partition_size(2),
        )
        .unwrap();
        // Used on its own first: an impulse is still on its way out.
        let mut impulse = constant(2, 0.0);
        impulse.channel_mut(0)[0] = 1.0;
        echo.process(&mut impulse);
        assert_eq!(impulse.channel(0), &[0.0, 0.0]);

        let (mut convolution, mut loader) = Convolution::<f32>::from_engine(gain_engine(1.0));
        convolution.prepare(&SPEC);
        loader.load(echo).unwrap();
        let mut silence = constant(8, 0.0);
        convolution.process(&mut silence);
        assert_eq!(silence.channel(0), &[0.0; 8], "the old impulse came out");
    }

    #[test]
    fn the_loader_reports_the_latency_to_come() {
        let (_, loader) = Convolution::<f32>::new();
        assert_eq!(loader.latency(), None);
        let (mut convolution, mut loader) = Convolution::<f32>::from_engine(gain_engine(1.0));
        assert_eq!(loader.latency(), Some(0));
        let late = ConvolutionEngine::new(
            &[[1.0f32]],
            1,
            ConvolutionRouting::Parallel,
            ConvolutionSettings::new()
                .with_partition_size(64)
                .with_zero_latency(false),
        )
        .unwrap();
        loader.load(late).unwrap();
        // Told before the audio thread has seen it.
        assert_eq!(loader.latency(), Some(64));
        assert_eq!(convolution.latency_samples(), 0);
        convolution.process(&mut constant(1, 0.0));
        assert_eq!(convolution.latency_samples(), 64);
        // A refused engine changes nothing.
        let (_playing, mut full) = Convolution::<f32>::with_capacity(1);
        for _ in 0..3 {
            full.load(gain_engine(1.0)).unwrap();
        }
        let late = ConvolutionEngine::new(
            &[[1.0f32]],
            1,
            ConvolutionRouting::Parallel,
            ConvolutionSettings::new().with_zero_latency(false),
        )
        .unwrap();
        assert!(full.load(late).is_err());
        assert_eq!(full.latency(), Some(0));
    }

    #[test]
    fn hostile_sizes_are_refused_or_held_without_panicking() {
        let one = [[1.0f32]];
        let build = |channels| {
            ConvolutionEngine::<f32>::new(
                &one,
                channels,
                ConvolutionRouting::Parallel,
                ConvolutionSettings::new(),
            )
            .map(|engine| engine.channels())
        };
        for channels in [MAX_CHANNELS + 1, usize::MAX / 2, usize::MAX] {
            assert_eq!(
                build(channels),
                Err(ConvolutionError::ChannelCount {
                    responses: 1,
                    channels
                })
            );
        }
        assert_eq!(build(MAX_CHANNELS), Ok(MAX_CHANNELS));

        for (capacity, limit) in [
            (0, 3),
            (1, 3),
            (MAX_LOADER_CAPACITY, MAX_LOADER_CAPACITY + 2),
            (usize::MAX, MAX_LOADER_CAPACITY + 2),
        ] {
            let (_, loader) = Convolution::<f32>::with_capacity(capacity);
            assert_eq!(loader.limit, limit, "capacity {capacity}");
        }
    }

    #[test]
    fn the_older_partitions_are_summed_while_the_partition_fills() {
        // Ten partitions of 64 after the head: nine older ones to spread
        // over each partition's 64 samples.
        let response = random_response(64 * 11, 31);
        let settings = ConvolutionSettings::new().with_partition_size(64);
        let mut engine =
            build_engine::<f64>(&[response], 1, ConvolutionRouting::Parallel, settings);
        assert_eq!(engine.num_partitions(), 10);
        let mut progress = Vec::new();
        for _ in 0..128 {
            let mut sample = AudioBuffer::<f64>::new(1, 1);
            sample.channel_mut(0)[0] = 0.5;
            engine.process(&mut sample);
            progress.push(engine.summed);
        }
        // After `fill` samples, 9 * fill / 64 partitions are done; at the
        // end of a partition the count starts again.
        for (index, &summed) in progress.iter().enumerate() {
            let fill = (index + 1) % 64;
            assert_eq!(summed, 9 * fill / 64, "sample {index}");
        }
        // One block of a whole partition does all of it at the end.
        engine.clear();
        let mut block = AudioBuffer::<f64>::new(1, 63);
        engine.process(&mut block);
        assert_eq!(engine.summed, 9 * 63 / 64);
        let mut block = AudioBuffer::<f64>::new(1, 1);
        engine.process(&mut block);
        assert_eq!(engine.summed, 0);
    }

    #[test]
    fn reset_and_prepare_end_a_crossfade_and_clear_the_tail() {
        let echo = ConvolutionEngine::new(
            &[vec![0.0f32, 0.0, 1.0]],
            1,
            ConvolutionRouting::Parallel,
            ConvolutionSettings::new(),
        )
        .unwrap();
        let (mut convolution, mut loader) = Convolution::<f32>::from_engine(echo);
        convolution.prepare(&SPEC);
        let mut buffer = constant(2, 1.0);
        convolution.process(&mut buffer);
        assert_eq!(buffer.channel(0), &[0.0, 0.0]);
        convolution.reset();
        let mut buffer = constant(4, 0.0);
        convolution.process(&mut buffer);
        assert_eq!(buffer.channel(0), &[0.0; 4], "the tail survived a reset");

        loader.load_with_crossfade(gain_engine(1.0), 50).unwrap();
        convolution.process(&mut constant(4, 1.0));
        assert!(convolution.is_crossfading());
        convolution.reset();
        assert!(!convolution.is_crossfading());
        loader.load_with_crossfade(gain_engine(1.0), 50).unwrap();
        convolution.process(&mut constant(4, 1.0));
        convolution.prepare(&SPEC);
        assert!(!convolution.is_crossfading());
        // Every engine but the one playing has come back (some were
        // collected by the second load).
        loader.collect();
        assert_eq!(loader.in_flight(), 1);
    }

    #[test]
    fn without_prepare_a_crossfade_becomes_a_swap() {
        let (mut convolution, mut loader) = Convolution::<f32>::from_engine(gain_engine(1.0));
        loader.load_with_crossfade(gain_engine(0.5), 100).unwrap();
        let mut buffer = constant(4, 1.0);
        convolution.process(&mut buffer);
        assert_eq!(buffer.channel(0), &[0.5; 4]);
    }

    #[test]
    fn the_loader_refuses_more_than_it_can_track() {
        let (mut convolution, mut loader) = Convolution::<f32>::with_capacity(1);
        // One engine of room, plus room for the one playing and the one
        // fading out.
        for gain in [1.0, 2.0, 3.0] {
            loader.load(gain_engine(gain)).unwrap();
        }
        let refused = loader.load(gain_engine(4.0)).unwrap_err();
        assert_eq!(refused.response_len(), 1);
        convolution.process(&mut constant(1, 1.0));
        // Two of the three were replaced within the block and came back.
        assert_eq!(loader.collect(), 2);
        loader.load(gain_engine(4.0)).unwrap();
    }

    #[test]
    fn fuzzed_loads_blocks_and_resets_keep_the_books_straight() {
        let mut random = Random::new(99);
        let (mut convolution, mut loader) = Convolution::<f32>::with_capacity(2);
        convolution.prepare(&ProcessSpec {
            sample_rate: 48_000.0,
            max_block_size: 32,
            num_channels: 2,
        });
        let mut buffer = AudioBuffer::<f32>::new(3, 100);
        let mut sent = 0usize;
        for _ in 0..2_000 {
            match (random.unit() * 6.0) as usize {
                0 | 1 => {
                    let len = 1 + (random.unit() * 300.0) as usize;
                    let partition = 1 << (random.unit() * 7.0) as u32;
                    let settings = ConvolutionSettings::new()
                        .with_partition_size(partition)
                        .with_zero_latency(random.unit() < 0.5);
                    let routing = if random.unit() < 0.5 {
                        ConvolutionRouting::Parallel
                    } else {
                        ConvolutionRouting::TrueStereo
                    };
                    let responses: Vec<Vec<f32>> = (0..4)
                        .map(|seed| {
                            random_response(len, seed)
                                .into_iter()
                                .map(|s| s as f32 * 0.1)
                                .collect()
                        })
                        .collect();
                    let count = if routing == ConvolutionRouting::Parallel {
                        2
                    } else {
                        4
                    };
                    let engine =
                        ConvolutionEngine::new(&responses[..count], 2, routing, settings).unwrap();
                    let crossfade = (random.unit() * 80.0) as usize;
                    if loader.load_with_crossfade(engine, crossfade).is_ok() {
                        sent += 1;
                    }
                }
                2 | 3 => {
                    let frames = (random.unit() * 100.0) as usize;
                    let channels = (random.unit() * 4.0) as usize;
                    buffer.resize(channels, frames);
                    for channel in buffer.iter_channels_mut() {
                        for sample in channel {
                            *sample = random.between(-1.0, 1.0) as f32;
                        }
                    }
                    convolution.process(&mut buffer);
                    assert!(
                        buffer
                            .iter_channels()
                            .all(|channel| channel.iter().all(|s| s.is_finite()))
                    );
                }
                4 => {
                    loader.collect();
                }
                _ => convolution.reset(),
            }
            assert!(loader.in_flight() <= loader.limit);
        }
        assert!(sent > 100, "only {sent} engines were accepted");
        convolution.reset();
        loader.collect();
        // Only the engine playing is still out.
        assert_eq!(
            loader.in_flight(),
            usize::from(convolution.engine().is_some())
        );
    }
}
