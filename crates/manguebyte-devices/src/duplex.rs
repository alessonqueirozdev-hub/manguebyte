use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};
use manguebyte_core::spsc::{self, Consumer, Producer};
use manguebyte_core::{AtomicF64, AudioBuffer, NoDenormals};
use manguebyte_dsp::Resampler;

use crate::blocks::{BlockClock, interleave};
use crate::device::{watch_errors, with_sample_type};
use crate::drift::{DriftController, MAX_CORRECTION};
use crate::timing::{StreamHealth, frame_duration, input_timing, output_timing};
use crate::{CallbackInfo, DeviceError, InputDevice, OutputDevice};

/// Input frames converted at a time when following drift. Small, so that
/// little converted audio waits between blocks; large enough that the
/// per-call overhead of the converter does not matter.
const CHUNK: usize = 32;

/// How a [`DuplexStream`] should run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DuplexSettings {
    /// Largest block the callback will be handed, in frames.
    pub max_block_size: usize,
    /// How much captured audio to keep in reserve between the two devices, in
    /// frames. This is added latency: lower is tighter, higher survives more
    /// scheduling jitter. The stream raises it by itself if it proves too low.
    pub latency_frames: usize,
    /// Whether to follow the drift between the two devices' clocks by
    /// resampling the input (on by default). Off, the stream absorbs drift
    /// by dropping or inserting input, which is audible but costs nothing;
    /// turn it off only when both directions share one clock, as the two
    /// halves of one audio interface do.
    pub drift_correction: bool,
}

impl DuplexSettings {
    /// Settings with a reserve of one block, and drift correction on.
    pub fn new(max_block_size: usize) -> Self {
        Self {
            max_block_size,
            latency_frames: max_block_size,
            drift_correction: true,
        }
    }

    /// Sets the reserve of captured audio, in frames.
    #[must_use]
    pub fn with_latency_frames(mut self, frames: usize) -> Self {
        self.latency_frames = frames;
        self
    }

    /// Turns drift correction on or off.
    #[must_use]
    pub fn with_drift_correction(mut self, on: bool) -> Self {
        self.drift_correction = on;
        self
    }
}

/// What the two device threads tell the rest of the program.
pub(crate) struct Stats {
    underruns: AtomicUsize,
    overruns: AtomicUsize,
    /// Input frames read per output frame, minus one.
    drift: AtomicF64,
    /// The input device's latency in nanoseconds; `u64::MAX` when unknown.
    input_latency_ns: AtomicU64,
    /// When the input last queued audio, on [`monotonic_ns`]'s clock;
    /// `u64::MAX` before it first does.
    last_push_ns: AtomicU64,
    /// How many frames it queued then.
    last_push_frames: AtomicU64,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            underruns: AtomicUsize::new(0),
            overruns: AtomicUsize::new(0),
            drift: AtomicF64::new(0.0),
            input_latency_ns: AtomicU64::new(u64::MAX),
            last_push_ns: AtomicU64::new(u64::MAX),
            last_push_frames: AtomicU64::new(0),
        }
    }
}

impl Stats {
    /// Frames the input device has captured but not yet handed over, at
    /// `now_ns`, assuming it captures at `sample_rate` between deliveries.
    ///
    /// The queue fill seen from the output jumps by a whole input block
    /// at every delivery. Sampled once per output block, that sawtooth
    /// aliases down to the beat between the two block rates, which can be
    /// as slow as the drift itself and so cannot be filtered out
    /// afterwards. Adding what has been captured since the last delivery
    /// gives a fill that moves smoothly.
    fn undelivered(&self, now_ns: u64, sample_rate: f64) -> f64 {
        let at = self.last_push_ns.load(Ordering::Acquire);
        if at == u64::MAX || now_ns < at {
            return 0.0;
        }
        let block = self.last_push_frames.load(Ordering::Relaxed) as f64;
        ((now_ns - at) as f64 * 1e-9 * sample_rate).min(block)
    }

    fn input_latency(&self) -> Option<Duration> {
        match self.input_latency_ns.load(Ordering::Relaxed) {
            u64::MAX => None,
            nanos => Some(Duration::from_nanos(nanos)),
        }
    }
}

/// An input and an output device running together, with captured audio handed
/// to the same callback that produces the output.
///
/// The two devices may be different pieces of hardware. Each has its own
/// clock, so over time one runs slightly ahead of the other. The stream
/// follows that drift by resampling the captured audio at a ratio that keeps
/// the reserve between the two devices steady, so nothing is ever dropped or
/// padded once it runs (see [`clock_drift_ppm`](Self::clock_drift_ppm)). If
/// one device stalls, the stream drops or pads input and counts each time it
/// does in [`overruns`](Self::overruns) and [`underruns`](Self::underruns).
///
/// ```no_run
/// use manguebyte_devices::{DuplexStream, InputDevice, OutputDevice};
///
/// let input = InputDevice::open_default()?;
/// let output = OutputDevice::open_default()?;
///
/// // Monitor the first input channel on every output channel.
/// let _stream = DuplexStream::start(&input, &output, 256, |input, output| {
///     if input.num_channels() == 0 {
///         return;
///     }
///     for channel in output.iter_channels_mut() {
///         channel.copy_from_slice(input.channel(0));
///     }
/// })?;
/// std::thread::sleep(std::time::Duration::from_secs(5));
/// # Ok::<(), manguebyte_devices::DeviceError>(())
/// ```
pub struct DuplexStream {
    input: cpal::Stream,
    output: cpal::Stream,
    stats: Arc<Stats>,
    pub(crate) input_health: Arc<StreamHealth>,
    pub(crate) output_health: Arc<StreamHealth>,
}

impl DuplexStream {
    /// Starts both devices, reporting stream errors on standard error.
    ///
    /// See [`start_with`](Self::start_with).
    pub fn start<F>(
        input: &InputDevice,
        output: &OutputDevice,
        max_block_size: usize,
        callback: F,
    ) -> Result<Self, DeviceError>
    where
        F: FnMut(&AudioBuffer<f32>, &mut AudioBuffer<f32>) + Send + 'static,
    {
        Self::start_with(
            input,
            output,
            DuplexSettings::new(max_block_size),
            callback,
            |error| eprintln!("manguebyte: audio stream error: {error}"),
        )
    }

    /// Starts both devices.
    ///
    /// `callback` runs on the output device's audio thread. It receives the
    /// captured audio and a silent output buffer of the same length, at most
    /// `settings.max_block_size` frames, and must not allocate, lock or
    /// block. The two buffers have the channel counts of their devices, which
    /// may differ.
    ///
    /// `on_error` is told about problems on either device.
    ///
    /// Both devices must run at the same sample rate; use
    /// [`with_settings`](InputDevice::with_settings) to make them agree.
    ///
    /// # Panics
    ///
    /// Panics if `settings.max_block_size` is zero.
    pub fn start_with<F, E>(
        input: &InputDevice,
        output: &OutputDevice,
        settings: DuplexSettings,
        mut callback: F,
        on_error: E,
    ) -> Result<Self, DeviceError>
    where
        F: FnMut(&AudioBuffer<f32>, &mut AudioBuffer<f32>) + Send + 'static,
        E: FnMut(DeviceError) + Send + 'static,
    {
        Self::start_timed(
            input,
            output,
            settings,
            move |input, output, _| callback(input, output),
            on_error,
        )
    }

    /// Starts both devices, telling the callback when each block was
    /// captured and will be heard.
    ///
    /// The same as [`start_with`](Self::start_with), except that `callback`
    /// also receives a [`CallbackInfo`]. Its frame position and stream time
    /// are the output device's; its input latency includes the reserve kept
    /// between the devices, so that it and the output latency add up to the
    /// whole way from input to output.
    ///
    /// # Panics
    ///
    /// Panics if `settings.max_block_size` is zero.
    pub fn start_timed<F, E>(
        input: &InputDevice,
        output: &OutputDevice,
        settings: DuplexSettings,
        callback: F,
        on_error: E,
    ) -> Result<Self, DeviceError>
    where
        F: FnMut(&AudioBuffer<f32>, &mut AudioBuffer<f32>, &CallbackInfo) + Send + 'static,
        E: FnMut(DeviceError) + Send + 'static,
    {
        assert!(
            settings.max_block_size > 0,
            "max_block_size must be non-zero"
        );
        let sample_rate = output.opened.sample_rate();
        if input.opened.sample_rate() != sample_rate {
            return Err(DeviceError::SampleRateMismatch {
                input: input.opened.sample_rate(),
                output: sample_rate,
            });
        }

        let stats = Arc::new(Stats::default());
        let input_health = StreamHealth::new();
        let output_health = StreamHealth::new();
        let (feeder, renderer) = bridge(
            input.num_channels(),
            output.num_channels(),
            sample_rate as usize,
            settings,
            callback,
            Arc::clone(&stats),
            Arc::clone(&input_health),
            Arc::clone(&output_health),
        );

        // Errors can arrive from either device's thread, so the handler is
        // shared. This lock is never taken on the path that moves audio.
        let on_error = Arc::new(Mutex::new(on_error));
        let shared = |on_error: &Arc<Mutex<E>>| {
            let on_error = Arc::clone(on_error);
            move |error: DeviceError| {
                if let Ok(mut handler) = on_error.lock() {
                    handler(error);
                }
            }
        };
        let report_input = watch_errors(&input_health, shared(&on_error));
        let report_output = watch_errors(&output_health, shared(&on_error));

        macro_rules! build_input {
            ($sample:ty) => {
                build_input::<$sample>(input, feeder, report_input)
            };
        }
        let input_stream = with_sample_type!(input.opened.resolved.format, build_input)?;

        macro_rules! build_output {
            ($sample:ty) => {
                build_output::<$sample, _>(output, renderer, report_output)
            };
        }
        let output_stream = with_sample_type!(output.opened.resolved.format, build_output)?;

        input_stream.play()?;
        output_stream.play()?;
        Ok(Self {
            input: input_stream,
            output: output_stream,
            stats,
            input_health,
            output_health,
        })
    }

    /// Pauses both devices.
    pub fn pause(&self) -> Result<(), DeviceError> {
        self.output.pause()?;
        Ok(self.input.pause()?)
    }

    /// Resumes both devices after [`pause`](Self::pause).
    pub fn resume(&self) -> Result<(), DeviceError> {
        self.input.play()?;
        Ok(self.output.play()?)
    }

    /// How many times the output needed captured audio that had not arrived
    /// yet and played silence for the missing part.
    pub fn underruns(&self) -> usize {
        self.stats.underruns.load(Ordering::Relaxed)
    }

    /// How many times captured audio was thrown away because the output was
    /// not keeping up with the input.
    pub fn overruns(&self) -> usize {
        self.stats.overruns.load(Ordering::Relaxed)
    }

    /// How much faster the input device's clock runs than the output's, in
    /// parts per million, as the drift correction has measured it. Zero
    /// with drift correction off.
    pub fn clock_drift_ppm(&self) -> f64 {
        self.stats.drift.load(Ordering::Relaxed) * 1e6
    }

    /// How many frames the output callback has produced so far.
    pub fn frames_processed(&self) -> u64 {
        self.output_health.frames()
    }

    /// Whether the backend reported that either device went away. The
    /// stream will not produce audio again.
    pub fn is_device_lost(&self) -> bool {
        self.input_health.is_lost() || self.output_health.is_lost()
    }
}

fn build_input<T>(
    device: &InputDevice,
    mut feeder: Feeder,
    report: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream, DeviceError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    Ok(device.opened.device.build_input_stream(
        device.opened.config(),
        move |data: &[T], info: &cpal::InputCallbackInfo| {
            feeder.feed(data, input_timing(info).1, monotonic_ns());
        },
        report,
        None,
    )?)
}

fn build_output<T, F>(
    device: &OutputDevice,
    mut renderer: Renderer<F>,
    report: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream, DeviceError>
where
    T: SizedSample + FromSample<f32>,
    F: FnMut(&AudioBuffer<f32>, &mut AudioBuffer<f32>, &CallbackInfo) + Send + 'static,
{
    Ok(device.opened.device.build_output_stream(
        device.opened.config(),
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            let (time, latency) = output_timing(info);
            renderer.render(data, time, latency, monotonic_ns());
        },
        report,
        None,
    )?)
}

/// Nanoseconds on a monotonic clock shared by every stream of the process.
/// Real-time safe once a bridge has been built, which starts the clock.
pub(crate) fn monotonic_ns() -> u64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    let elapsed = ORIGIN.get_or_init(Instant::now).elapsed();
    u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX - 1)
}

/// Builds the two halves that carry audio from the input device's thread to
/// the output device's thread.
#[allow(clippy::too_many_arguments)]
pub(crate) fn bridge<F>(
    input_channels: usize,
    output_channels: usize,
    sample_rate: usize,
    settings: DuplexSettings,
    callback: F,
    stats: Arc<Stats>,
    input_health: Arc<StreamHealth>,
    output_health: Arc<StreamHealth>,
) -> (Feeder, Renderer<F>) {
    let block = settings.max_block_size;
    // Start the clock here, so that the audio threads only read it.
    monotonic_ns();
    // One second of audio, and never less than a comfortable multiple of what
    // was asked for, so the reserve has room to grow.
    let capacity_frames = sample_rate.max(8 * (block + settings.latency_frames));
    let (producer, consumer) = spsc::channel(capacity_frames * input_channels.max(1));
    let capacity_frames = producer.capacity() / input_channels.max(1);

    let drift = (settings.drift_correction && input_channels > 0)
        .then(|| Drift::new(input_channels, block, sample_rate as f64));

    let feeder = Feeder {
        producer,
        scratch: vec![0.0; block * input_channels],
        channels: input_channels,
        stats: Arc::clone(&stats),
        health: input_health,
    };
    let renderer = Renderer {
        consumer,
        interleaved: vec![0.0; block.max(CHUNK) * input_channels],
        input: AudioBuffer::new(input_channels, block),
        output: AudioBuffer::new(output_channels, block),
        callback,
        primed: false,
        reserve: settings.latency_frames,
        max_reserve: capacity_frames / 4,
        high_water: capacity_frames / 2,
        sample_rate: sample_rate as f64,
        clock: BlockClock::new(sample_rate as f64, output_health),
        drift,
        stats,
    };
    (feeder, renderer)
}

/// Runs on the input device's thread: converts captured audio to `f32` and
/// queues it for the output side.
pub(crate) struct Feeder {
    producer: Producer<f32>,
    scratch: Vec<f32>,
    channels: usize,
    stats: Arc<Stats>,
    health: Arc<StreamHealth>,
}

impl Feeder {
    /// Queues `data`, which the device captured `latency` before handing
    /// it over at `now_ns`, on [`monotonic_ns`]'s clock.
    pub(crate) fn feed<T: Sample>(&mut self, data: &[T], latency: Option<Duration>, now_ns: u64)
    where
        f32: FromSample<T>,
    {
        let nanos = latency.map_or(u64::MAX, |latency| {
            u64::try_from(latency.as_nanos()).unwrap_or(u64::MAX - 1)
        });
        self.stats.input_latency_ns.store(nanos, Ordering::Relaxed);
        if self.channels == 0 {
            return;
        }
        self.health.advance(data.len() / self.channels);
        for chunk in data.chunks(self.scratch.len()) {
            // Only whole frames go in, so the channels can never slip out of
            // step with each other.
            let frames =
                (chunk.len() / self.channels).min(self.producer.free_len() / self.channels);
            let samples = frames * self.channels;
            for (slot, &value) in self.scratch[..samples].iter_mut().zip(chunk) {
                *slot = f32::from_sample(value);
            }
            self.producer.push_slice(&self.scratch[..samples]);
            if samples < chunk.len() {
                self.stats.overruns.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.stats
            .last_push_frames
            .store((data.len() / self.channels) as u64, Ordering::Relaxed);
        self.stats.last_push_ns.store(now_ns, Ordering::Release);
    }
}

/// The input side's converter, which follows the drift between the clocks.
struct Drift {
    resampler: Resampler<f32>,
    controller: DriftController,
    /// Queued input on its way into the converter.
    chunk: AudioBuffer<f32>,
    /// What the converter made of one chunk.
    produced: AudioBuffer<f32>,
    /// Converted input waiting for the callback; `fifo_len` frames of it.
    fifo: AudioBuffer<f32>,
    fifo_len: usize,
}

impl Drift {
    fn new(channels: usize, block: usize, sample_rate: f64) -> Self {
        let resampler = Resampler::new(sample_rate, sample_rate, channels, CHUNK);
        // The most a chunk can become at the largest correction.
        let most = (CHUNK as f64 / (1.0 - MAX_CORRECTION)).ceil() as usize + 2;
        Self {
            resampler,
            controller: DriftController::new(sample_rate),
            chunk: AudioBuffer::new(channels, CHUNK),
            produced: AudioBuffer::new(channels, most),
            fifo: AudioBuffer::new(channels, block + most),
            fifo_len: 0,
        }
    }
}

/// Runs on the output device's thread: pairs queued input with the output
/// buffer and calls the user's callback.
pub(crate) struct Renderer<F> {
    consumer: Consumer<f32>,
    interleaved: Vec<f32>,
    input: AudioBuffer<f32>,
    output: AudioBuffer<f32>,
    callback: F,
    /// Whether the reserve has been built up and input is flowing.
    primed: bool,
    /// Frames of input to hold back before starting to consume.
    reserve: usize,
    max_reserve: usize,
    /// Queue fill, in frames, beyond which the excess is discarded.
    high_water: usize,
    sample_rate: f64,
    clock: BlockClock,
    drift: Option<Drift>,
    stats: Arc<Stats>,
}

impl<F: FnMut(&AudioBuffer<f32>, &mut AudioBuffer<f32>, &CallbackInfo)> Renderer<F> {
    /// Fills `data`, which the device will start playing `latency` after
    /// `stream_time`, and which was asked for at `now_ns` on
    /// [`monotonic_ns`]'s clock.
    pub(crate) fn render<T: Sample + FromSample<f32>>(
        &mut self,
        data: &mut [T],
        stream_time: Duration,
        latency: Option<Duration>,
        now_ns: u64,
    ) {
        // A fading sound must not slow the audio thread down.
        let _quiet = NoDenormals::new();
        let channels = self.output.num_channels();
        if channels == 0 {
            return;
        }
        let mut info = self.clock.info(stream_time);
        info.output_latency = latency;
        for chunk in data.chunks_mut(self.output.capacity() * channels) {
            let frames = chunk.len() / channels;
            self.input.set_num_frames(frames);
            self.output.set_num_frames(frames);
            self.output.clear();
            let held = self.pull_input(frames, now_ns);
            info.input_latency = self
                .stats
                .input_latency()
                .map(|device| device + frame_duration(held, self.sample_rate));
            (self.callback)(&self.input, &mut self.output, &info);
            interleave(&self.output, chunk);
            info = info.advanced(frames);
        }
        self.clock.finish(&info);
    }

    /// Fills `self.input` with the next `frames` of captured audio and
    /// returns how many frames were held between the devices.
    fn pull_input(&mut self, frames: usize, now_ns: u64) -> usize {
        let channels = self.input.num_channels();
        if channels == 0 {
            return 0;
        }
        let mut available = self.consumer.len() / channels;

        // The input device is running faster than the output, or the output
        // stalled. Drop the backlog rather than let latency pile up.
        if available > self.high_water {
            self.consumer.skip((available - self.reserve) * channels);
            available = self.reserve;
            self.stats.overruns.fetch_add(1, Ordering::Relaxed);
            if let Some(drift) = &mut self.drift {
                drift.controller.restart();
            }
        }

        if !self.primed && available >= self.reserve.max(frames) {
            self.primed = true;
            if let Some(drift) = &mut self.drift {
                drift.controller.restart();
            }
        }
        if !self.primed {
            self.input.clear();
            return available;
        }

        let (filled, held) = match &mut self.drift {
            None => {
                let take = frames.min(available);
                let samples = &mut self.interleaved[..frames * channels];
                let filled = self.consumer.pop_slice(&mut samples[..take * channels]);
                samples[filled..].fill(0.0);
                self.input.copy_from_interleaved(samples);
                (filled / channels, available)
            }
            Some(drift) => {
                let held = available + drift.fifo_len;
                let fill = held as f64 + self.stats.undelivered(now_ns, self.sample_rate);
                let ratio = drift.controller.update(fill, frames);
                drift.resampler.set_ratio(ratio);
                self.stats
                    .drift
                    .store(drift.controller.correction(), Ordering::Relaxed);
                let filled = pull_resampled(
                    drift,
                    &mut self.consumer,
                    &mut self.interleaved,
                    &mut self.input,
                    frames,
                    available,
                );
                (
                    filled,
                    held + drift.resampler.latency_frames().round() as usize,
                )
            }
        };

        if filled < frames {
            // Ran dry: the reserve was too small for how these two devices
            // deliver audio. Rebuild it a little larger.
            self.primed = false;
            self.reserve = (self.reserve + frames).min(self.max_reserve);
            self.stats.underruns.fetch_add(1, Ordering::Relaxed);
        }
        held
    }
}

/// Converts queued input through the drift converter until `frames` of it
/// are ready, and hands them to `input`, padded with silence if the queue
/// ran dry. Returns how many frames were real input.
fn pull_resampled(
    drift: &mut Drift,
    consumer: &mut Consumer<f32>,
    interleaved: &mut [f32],
    input: &mut AudioBuffer<f32>,
    frames: usize,
    mut available: usize,
) -> usize {
    let channels = input.num_channels();
    while drift.fifo_len < frames && available > 0 {
        let take = CHUNK.min(available);
        let samples = &mut interleaved[..take * channels];
        let popped = consumer.pop_slice(samples) / channels;
        if popped == 0 {
            break;
        }
        drift.chunk.set_num_frames(popped);
        drift
            .chunk
            .copy_from_interleaved(&samples[..popped * channels]);
        let made = drift.resampler.process(&drift.chunk, &mut drift.produced);
        let start = drift.fifo_len;
        for channel in 0..channels {
            drift.fifo.channel_mut(channel)[start..start + made]
                .copy_from_slice(&drift.produced.channel(channel)[..made]);
        }
        drift.fifo_len += made;
        available -= popped;
    }

    let ready = drift.fifo_len.min(frames);
    for channel in 0..channels {
        let waiting = drift.fifo.channel_mut(channel);
        let destination = input.channel_mut(channel);
        destination[..ready].copy_from_slice(&waiting[..ready]);
        destination[ready..].fill(0.0);
        waiting.copy_within(ready..drift.fifo_len, 0);
    }
    drift.fifo_len -= ready;
    ready
}

#[cfg(test)]
mod tests {
    use super::*;

    type Echo = fn(&AudioBuffer<f32>, &mut AudioBuffer<f32>, &CallbackInfo);

    fn echo(input: &AudioBuffer<f32>, output: &mut AudioBuffer<f32>, _: &CallbackInfo) {
        for (index, channel) in output.iter_channels_mut().enumerate() {
            channel.copy_from_slice(input.channel(index % input.num_channels()));
        }
    }

    fn make_with(
        input_channels: usize,
        settings: DuplexSettings,
        rate: usize,
    ) -> (Feeder, Renderer<Echo>, Arc<Stats>) {
        let stats = Arc::new(Stats::default());
        let (feeder, renderer) = bridge(
            input_channels,
            2,
            rate,
            settings,
            echo as Echo,
            Arc::clone(&stats),
            StreamHealth::new(),
            StreamHealth::new(),
        );
        (feeder, renderer, stats)
    }

    /// The queue's own behaviour, with drift correction off so that input
    /// comes out sample for sample.
    fn make(
        input_channels: usize,
        block: usize,
        latency: usize,
    ) -> (Feeder, Renderer<Echo>, Arc<Stats>) {
        let settings = DuplexSettings::new(block)
            .with_latency_frames(latency)
            .with_drift_correction(false);
        make_with(input_channels, settings, 1_000)
    }

    fn feed(feeder: &mut Feeder, data: &[f32]) {
        feeder.feed(data, None, 0);
    }

    fn render<T: Sample + FromSample<f32>>(renderer: &mut Renderer<Echo>, out: &mut [T]) {
        renderer.render(out, Duration::ZERO, None, 0);
    }

    /// A stereo ramp where the right channel is the negated left one.
    fn ramp(start: usize, frames: usize) -> Vec<f32> {
        (start..start + frames)
            .flat_map(|frame| {
                let value = (frame + 1) as f32;
                [value, -value]
            })
            .collect()
    }

    #[test]
    fn plays_silence_until_the_reserve_is_built() {
        let (mut feeder, mut renderer, stats) = make(2, 4, 8);
        let mut out = [9.0f32; 8];

        render(&mut renderer, &mut out);
        assert_eq!(out, [0.0; 8]);

        feed(&mut feeder, &ramp(0, 6));
        render(&mut renderer, &mut out);
        assert_eq!(
            out, [0.0; 8],
            "six frames are fewer than the reserve of eight"
        );

        feed(&mut feeder, &ramp(6, 2));
        render(&mut renderer, &mut out);
        assert_eq!(out, ramp(0, 4).as_slice());
        render(&mut renderer, &mut out);
        assert_eq!(out, ramp(4, 4).as_slice());
        assert_eq!(stats.underruns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn an_underrun_pads_with_silence_and_grows_the_reserve() {
        let (mut feeder, mut renderer, stats) = make(2, 4, 4);
        feed(&mut feeder, &ramp(0, 6));

        let mut out = [9.0f32; 8];
        render(&mut renderer, &mut out);
        assert_eq!(out, ramp(0, 4).as_slice());

        render(&mut renderer, &mut out);
        assert_eq!(&out[..4], ramp(4, 2).as_slice());
        assert_eq!(&out[4..], &[0.0; 4]);
        assert_eq!(stats.underruns.load(Ordering::Relaxed), 1);
        assert_eq!(renderer.reserve, 8);

        // Input is held back again until the larger reserve is there.
        feed(&mut feeder, &ramp(6, 4));
        render(&mut renderer, &mut out);
        assert_eq!(out, [0.0; 8]);
        feed(&mut feeder, &ramp(10, 4));
        render(&mut renderer, &mut out);
        assert_eq!(out, ramp(6, 4).as_slice());
    }

    #[test]
    fn a_backlog_is_trimmed_to_the_reserve() {
        let (mut feeder, mut renderer, stats) = make(2, 4, 4);
        let capacity = feeder.producer.capacity() / 2;
        let backlog = renderer.high_water + 10;
        assert!(backlog < capacity);
        for start in (0..backlog).step_by(2) {
            feed(&mut feeder, &ramp(start, 2));
        }

        let mut out = [0.0f32; 8];
        render(&mut renderer, &mut out);
        assert_eq!(stats.overruns.load(Ordering::Relaxed), 1);
        // The newest `reserve` frames survive, so playback resumes there.
        assert_eq!(out, ramp(backlog - 4, 4).as_slice());
    }

    #[test]
    fn a_full_queue_drops_whole_frames_only() {
        let (mut feeder, mut renderer, stats) = make(2, 4, 4);
        let capacity = feeder.producer.capacity() / 2;
        for start in (0..capacity + 8).step_by(4) {
            feed(&mut feeder, &ramp(start, 4));
        }
        assert!(stats.overruns.load(Ordering::Relaxed) >= 1);
        assert_eq!(renderer.consumer.len() % 2, 0);

        let mut out = [0.0f32; 8];
        render(&mut renderer, &mut out);
        for frame in out.chunks_exact(2) {
            assert_eq!(frame[0], -frame[1], "channels slipped out of step");
        }
    }

    #[test]
    fn mono_input_can_feed_stereo_output() {
        let (mut feeder, mut renderer, _) = make(1, 4, 2);
        feed(&mut feeder, &[0.1f32, 0.2, 0.3, 0.4]);
        let mut out = [0i16; 8];
        render(&mut renderer, &mut out);
        assert_eq!(out[0], out[1]);
        assert!(out[6] > out[0]);
    }

    #[test]
    fn the_callback_is_told_the_whole_input_latency() {
        let (mut feeder, mut renderer, _) = make(1, 4, 8);
        let mut seen = Vec::new();
        let stats = Arc::clone(&renderer.stats);
        let mut renderer = Renderer {
            callback: |_: &AudioBuffer<f32>, _: &mut AudioBuffer<f32>, info: &CallbackInfo| {
                seen.push(*info);
            },
            consumer: renderer.consumer,
            interleaved: std::mem::take(&mut renderer.interleaved),
            input: renderer.input,
            output: renderer.output,
            primed: false,
            reserve: renderer.reserve,
            max_reserve: renderer.max_reserve,
            high_water: renderer.high_water,
            sample_rate: renderer.sample_rate,
            clock: renderer.clock,
            drift: None,
            stats,
        };
        feeder.feed(&[0.0f32; 12], Some(Duration::from_millis(3)), 0);
        let mut out = [0.0f32; 8];
        renderer.render(
            &mut out,
            Duration::from_secs(1),
            Some(Duration::from_millis(5)),
            0,
        );
        drop(renderer);
        let info = seen[0];
        assert_eq!(info.frame, 0);
        assert_eq!(info.stream_time, Duration::from_secs(1));
        assert_eq!(info.output_latency, Some(Duration::from_millis(5)));
        // 3 ms in the device, then 12 frames at 1 kHz between the devices.
        assert_eq!(info.input_latency, Some(Duration::from_millis(15)));
        assert_eq!(info.round_trip_latency(), Some(Duration::from_millis(20)));
    }

    /// What came out of a simulated run of two devices.
    struct Run {
        output: Vec<f32>,
        underruns: usize,
        overruns: usize,
        /// Mean fill of the queue over the last second, minus the target.
        late_fill_error: f64,
        correction: f64,
    }

    const RATE: usize = 48_000;
    const TONE: f64 = 997.0;

    /// Two devices whose clocks differ by `drift_ppm`: the input delivers
    /// `input_block` frames at a time at 48 kHz times (1 + drift), the
    /// output asks for `output_block` frames at 48 kHz. The input captures
    /// a steady sine, which the callback passes straight through.
    fn simulate(
        drift_ppm: f64,
        input_block: usize,
        output_block: usize,
        latency: usize,
        correct: bool,
        seconds: f64,
    ) -> Run {
        let settings = DuplexSettings::new(output_block)
            .with_latency_frames(latency)
            .with_drift_correction(correct);
        let (mut feeder, mut renderer, stats) = make_with(1, settings, RATE);
        let input_rate = RATE as f64 * (1.0 + drift_ppm * 1e-6);

        let mut captured = 0usize;
        let mut block = vec![0.0f32; input_block];
        let mut out = vec![0.0f32; output_block * 2];
        let mut output = Vec::new();
        let mut next_input = input_block as f64 / input_rate;
        let mut next_output = 0.000_37;
        let (mut fill_sum, mut fill_count) = (0.0, 0usize);
        while next_output < seconds {
            if next_input <= next_output {
                for sample in &mut block {
                    let phase = std::f64::consts::TAU * TONE * captured as f64 / input_rate;
                    *sample = phase.sin() as f32;
                    captured += 1;
                }
                // Delivered once its last frame has been captured.
                feeder.feed(&block, None, (next_input * 1e9) as u64);
                next_input += input_block as f64 / input_rate;
            } else {
                let now = (next_output * 1e9) as u64;
                if next_output > seconds - 1.0 {
                    let waiting = renderer.drift.as_ref().map_or(0, |drift| drift.fifo_len);
                    fill_sum += (renderer.consumer.len() + waiting) as f64
                        + stats.undelivered(now, RATE as f64);
                    fill_count += 1;
                }
                renderer.render(&mut out, Duration::ZERO, None, now);
                output.extend(out.iter().step_by(2));
                next_output += output_block as f64 / RATE as f64;
            }
        }
        Run {
            output,
            underruns: stats.underruns.load(Ordering::Relaxed),
            overruns: stats.overruns.load(Ordering::Relaxed),
            late_fill_error: fill_sum / fill_count as f64
                - renderer
                    .drift
                    .as_ref()
                    .map_or(renderer.reserve as f64, |drift| drift.controller.target()),
            correction: stats.drift.load(Ordering::Relaxed),
        }
    }

    /// The largest amount by which the output breaks the recurrence every
    /// sine obeys, y[n] = 2 cos(w) y[n-1] - y[n-2], after `skip` frames. A
    /// dropped or repeated frame breaks it by about w, 0.13 here.
    fn worst_step(output: &[f32], skip: usize) -> f64 {
        let w = std::f64::consts::TAU * TONE / RATE as f64;
        output[skip..]
            .windows(3)
            .map(|y| (f64::from(y[2]) - 2.0 * w.cos() * f64::from(y[1]) + f64::from(y[0])).abs())
            .fold(0.0, f64::max)
    }

    #[test]
    fn a_clock_mismatch_is_followed_without_a_discontinuity() {
        for (drift_ppm, input_block, output_block, latency) in [
            (300.0, 128, 256, 512),
            (-300.0, 128, 256, 512),
            (-150.0, 1_024, 192, 1_536),
        ] {
            let run = simulate(drift_ppm, input_block, output_block, latency, true, 20.0);
            let what = format!("{drift_ppm} ppm, blocks {input_block} in, {output_block} out");
            assert_eq!((run.underruns, run.overruns), (0, 0), "{what}");
            // Wait for the reserve to be built, and the sine's start to pass
            // through the converter.
            let start = latency + 1_000;
            let step = worst_step(&run.output, start);
            assert!(step < 1e-4, "{what}: a step of {step:e}");
            let peak = run.output[start..]
                .iter()
                .fold(0.0f32, |p, s| p.max(s.abs()));
            assert!(peak > 0.99, "{what}: peak {peak}");
            assert!(
                (run.correction * 1e6 - drift_ppm).abs() < 5.0,
                "{what}: learnt {} ppm",
                run.correction * 1e6
            );
            assert!(
                run.late_fill_error.abs() < 16.0,
                "{what}: the reserve is {} frames off",
                run.late_fill_error
            );
        }
    }

    #[test]
    fn without_correction_the_same_mismatch_drops_or_pads() {
        // The behaviour drift correction replaces: the reserve drains, and
        // the stream has to pad the input with silence.
        let run = simulate(-300.0, 128, 256, 512, false, 30.0);
        assert!(run.underruns > 0);
        assert!(worst_step(&run.output, 1_512) > 0.05);
    }
}
