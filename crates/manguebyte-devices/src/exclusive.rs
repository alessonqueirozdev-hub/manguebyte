//! The `"WASAPI exclusive"` host: WASAPI's devices, each held by this
//! program alone, through `manguebyte-wasapi`.
//!
//! The devices are the WASAPI host's, by the same identifiers, so a saved
//! setup names the same device in either host. An output, an input, or one
//! of each can run. With both, each device has its own stream and thread,
//! and the captured audio crosses to the output's through the same bridge as
//! a [`DuplexStream`](crate::DuplexStream)'s, which follows the drift between
//! their clocks.
//!
//! The system is reached through [`Driver`], so that everything decided here
//! is tested on every system, with a stand-in for the devices.

use std::sync::Arc;
use std::time::Duration;

use manguebyte_core::AudioBuffer;
use manguebyte_wasapi::{
    Capabilities, Captured, Error as WasapiError, Format, Packet, Request, Samples,
};

use crate::backend::{
    Endpoint, EngineCallback, ErrorSink, OpenedEndpoint, OpenedFormat, Started, StreamRequest,
};
use crate::blocks::{BlockCapturer, BlockClock, BlockRenderer};
use crate::duplex::{Stats, bridge, monotonic_ns};
use crate::host::{ExclusiveChannels, ExclusiveModes, ExclusiveRefusal, ExclusiveSupport};
use crate::hotplug::DeviceSummary;
use crate::timing::StreamHealth;
use crate::{
    CallbackInfo, ConfigRange, DeviceError, DeviceInfo, Direction, DuplexSettings, StreamSettings,
};

/// The longest period the buffer sizes offered go up to, in frames.
const LONGEST_PERIOD: u32 = 8_192;

/// A device as the shared host knows it, which fills in what the settings
/// leave open.
#[derive(Debug, Clone)]
pub(crate) struct OwnDevice {
    pub(crate) name: String,
    pub(crate) sample_rate: u32,
    pub(crate) channels: u16,
}

/// The channel counts a device takes exclusively, each with what it does
/// at it.
type Modes = Vec<(u16, Capabilities)>;

/// What this host needs from the system.
pub(crate) trait Driver {
    /// A running stream; dropping it stops it and drops its callbacks.
    type Stream: Send + 'static;

    /// The device `id` as the shared host describes it.
    fn own(&self, direction: Direction, id: &str) -> Result<OwnDevice, DeviceError>;

    /// [`manguebyte_wasapi::capabilities`].
    fn capabilities(&self, id: &str, channels: u16) -> Result<Capabilities, WasapiError>;

    /// [`manguebyte_wasapi::start`].
    fn play<R, E>(
        &self,
        id: &str,
        request: Request,
        render: R,
        on_error: E,
    ) -> Result<(Self::Stream, Format), WasapiError>
    where
        R: FnMut(Samples<'_>, Duration) + Send + 'static,
        E: FnMut(WasapiError) + Send + 'static;

    /// [`manguebyte_wasapi::start_capture`].
    fn record<C, E>(
        &self,
        id: &str,
        request: Request,
        capture: C,
        on_error: E,
    ) -> Result<(Self::Stream, Format), WasapiError>
    where
        C: FnMut(Packet<'_>, Duration) + Send + 'static,
        E: FnMut(WasapiError) + Send + 'static;
}

/// WASAPI itself.
#[cfg(windows)]
pub(crate) struct System;

#[cfg(windows)]
impl Driver for System {
    type Stream = manguebyte_wasapi::Stream;

    fn own(&self, direction: Direction, id: &str) -> Result<OwnDevice, DeviceError> {
        use crate::{InputDevice, OutputDevice};

        let (name, sample_rate, channels) = match direction {
            Direction::Output => {
                let device = OutputDevice::open_by_id(id)?;
                (
                    device.name().to_owned(),
                    device.sample_rate(),
                    device.num_channels(),
                )
            }
            Direction::Input => {
                let device = InputDevice::open_by_id(id)?;
                (
                    device.name().to_owned(),
                    device.sample_rate(),
                    device.num_channels(),
                )
            }
        };
        Ok(OwnDevice {
            name,
            sample_rate: sample_rate as u32,
            channels: u16::try_from(channels).unwrap_or(2),
        })
    }

    fn capabilities(&self, id: &str, channels: u16) -> Result<Capabilities, WasapiError> {
        manguebyte_wasapi::capabilities(id, channels)
    }

    fn play<R, E>(
        &self,
        id: &str,
        request: Request,
        render: R,
        on_error: E,
    ) -> Result<(Self::Stream, Format), WasapiError>
    where
        R: FnMut(Samples<'_>, Duration) + Send + 'static,
        E: FnMut(WasapiError) + Send + 'static,
    {
        manguebyte_wasapi::start(id, request, render, on_error)
    }

    fn record<C, E>(
        &self,
        id: &str,
        request: Request,
        capture: C,
        on_error: E,
    ) -> Result<(Self::Stream, Format), WasapiError>
    where
        C: FnMut(Packet<'_>, Duration) + Send + 'static,
        E: FnMut(WasapiError) + Send + 'static,
    {
        manguebyte_wasapi::start_capture(id, request, capture, on_error)
    }
}

fn device_error(error: WasapiError) -> DeviceError {
    match error {
        WasapiError::DeviceNotFound(id) => DeviceError::DeviceNotFound(id),
        WasapiError::FormatNotSupported(what) => DeviceError::UnsupportedSettings(format!(
            "the device takes no format exclusively at {what}"
        )),
        other => DeviceError::Other(Box::new(other)),
    }
}

fn refusal(error: WasapiError) -> ExclusiveRefusal {
    match error {
        WasapiError::FormatNotSupported(_) => ExclusiveRefusal::NoFormat,
        WasapiError::ExclusiveNotAllowed => ExclusiveRefusal::NotAllowed,
        WasapiError::DeviceBusy(_) => ExclusiveRefusal::InUse,
        WasapiError::AccessDenied => ExclusiveRefusal::AccessDenied,
        WasapiError::DeviceNotFound(_) | WasapiError::DeviceLost => ExclusiveRefusal::Gone,
        other => ExclusiveRefusal::Other(other.to_string()),
    }
}

/// The channel counts worth asking a device about: its own, those the
/// shared host lists, stereo and mono.
fn channel_counts(info: &DeviceInfo) -> Vec<u16> {
    let mut counts: Vec<u16> = Vec::new();
    for count in info
        .default_channels
        .into_iter()
        .chain(info.configs.iter().map(|config| config.channels))
        .chain([2, 1])
    {
        if count > 0 && !counts.contains(&count) {
            counts.push(count);
        }
    }
    counts
}

/// Which of `counts` the device takes at some rate, in their order.
fn probe<D: Driver>(driver: &D, id: &str, counts: &[u16]) -> Result<Modes, WasapiError> {
    let mut modes = Modes::new();
    for &channels in counts {
        let capabilities = driver.capabilities(id, channels)?;
        if !capabilities.sample_rates.is_empty() {
            modes.push((channels, capabilities));
        }
    }
    Ok(modes)
}

/// The channel count to open a device with when the settings name none: its
/// own if it takes it exclusively at `sample_rate`, else stereo or mono if
/// it takes one of those, else its own, so that opening says what is wrong.
fn channels_for<D: Driver>(driver: &D, id: &str, own: u16, sample_rate: u32) -> u16 {
    [own, 2, 1]
        .into_iter()
        .find(|&channels| {
            channels > 0
                && driver
                    .capabilities(id, channels)
                    .is_ok_and(|caps| caps.sample_rates.contains(&sample_rate))
        })
        .unwrap_or(own)
}

/// `info`, as the device is in exclusive mode: the channel counts and rates
/// it takes there, and buffer sizes from its shortest period up. A device
/// that takes nothing, or will not say, is described as `cpal` described it.
pub(crate) fn describe<D: Driver>(driver: &D, mut info: DeviceInfo) -> DeviceInfo {
    let Ok(modes) = probe(driver, &info.id, &channel_counts(&info)) else {
        return info;
    };
    // The device's own channel count if it is among them, else the first.
    let Some((channels, caps)) = modes
        .iter()
        .find(|(channels, _)| info.default_channels == Some(*channels))
        .or(modes.first())
    else {
        return info;
    };
    let rate = info
        .default_sample_rate
        .filter(|rate| caps.sample_rates.contains(rate))
        .or_else(|| caps.sample_rates.iter().copied().find(|&r| r == 48_000))
        .or_else(|| caps.sample_rates.first().copied());
    info.default_channels = Some(*channels);
    info.default_sample_rate = rate;
    info.configs = modes
        .iter()
        .filter_map(|(channels, caps)| {
            let shortest = caps.min_period_frames(rate?).max(1);
            Some(ConfigRange {
                channels: *channels,
                min_sample_rate: *caps.sample_rates.first()?,
                max_sample_rate: *caps.sample_rates.last()?,
                buffer_size: Some((shortest, LONGEST_PERIOD)),
            })
        })
        .collect();
    info.configs.sort_by_key(|config| config.channels);
    info
}

/// A period in WASAPI's 100 ns units.
fn period(hundreds_of_nanos: i64) -> Duration {
    Duration::from_nanos(
        u64::try_from(hundreds_of_nanos)
            .unwrap_or(0)
            .saturating_mul(100),
    )
}

/// Whether the device of `info` can be held exclusively, and if not, why,
/// as far as it says without being opened.
pub(crate) fn support<D: Driver>(driver: &D, info: &DeviceInfo) -> ExclusiveSupport {
    let outcome = match probe(driver, &info.id, &channel_counts(info)) {
        Err(error) => Err(refusal(error)),
        Ok(modes) => match modes.first() {
            None => Err(ExclusiveRefusal::NoFormat),
            Some((_, first)) => Ok(ExclusiveModes {
                shortest_period: period(first.min_period),
                default_period: period(first.default_period),
                channels: modes
                    .iter()
                    .map(|(channels, caps)| ExclusiveChannels {
                        channels: *channels,
                        sample_rates: caps.sample_rates.clone(),
                    })
                    .collect(),
            }),
        },
    };
    ExclusiveSupport {
        id: info.id.clone(),
        name: info.name.clone(),
        outcome,
    }
}

/// One side of a stream to open, with everything decided.
struct Side {
    id: String,
    name: String,
    request: Request,
}

impl Side {
    /// What to ask the device of `endpoint` for. A rate the settings leave
    /// open is `rate`, or else the device's own.
    fn plan<D: Driver>(
        driver: &D,
        direction: Direction,
        endpoint: &Endpoint,
        rate: Option<u32>,
    ) -> Result<Self, DeviceError> {
        let own = driver.own(direction, &endpoint.id)?;
        let sample_rate = endpoint
            .settings
            .sample_rate
            .or(rate)
            .unwrap_or(own.sample_rate);
        let channels = endpoint
            .settings
            .channels
            .unwrap_or_else(|| channels_for(driver, &endpoint.id, own.channels, sample_rate));
        Ok(Self {
            id: endpoint.id.clone(),
            name: own.name,
            request: Request {
                sample_rate,
                channels,
                period_frames: endpoint.settings.buffer_size,
            },
        })
    }

    fn channels(&self) -> usize {
        usize::from(self.request.channels)
    }

    fn opened(&self, format: Format, health: Arc<StreamHealth>) -> OpenedEndpoint {
        OpenedEndpoint {
            device: DeviceSummary {
                id: self.id.clone(),
                name: self.name.clone(),
            },
            settings: StreamSettings {
                sample_rate: Some(format.sample_rate),
                channels: Some(format.channels),
                buffer_size: Some(format.period_frames),
            },
            health,
        }
    }
}

/// What a stream calls when its device fails.
fn lost(
    health: &Arc<StreamHealth>,
    on_error: &ErrorSink,
) -> impl FnMut(WasapiError) + Send + 'static + use<> {
    let health = Arc::clone(health);
    let on_error = Arc::clone(on_error);
    move |error| {
        health.mark_lost();
        on_error(device_error(error));
    }
}

/// Runs `request`'s output, input, or both exclusively.
pub(crate) fn start<D: Driver>(
    driver: &D,
    request: &StreamRequest,
    make: &mut dyn FnMut(OpenedFormat) -> EngineCallback,
    on_error: ErrorSink,
) -> Result<Started, DeviceError> {
    let output = request
        .output
        .as_ref()
        .map(|endpoint| Side::plan(driver, Direction::Output, endpoint, None))
        .transpose()?;
    let output_rate = output.as_ref().map(|side| side.request.sample_rate);
    let input = request
        .input
        .as_ref()
        .map(|endpoint| Side::plan(driver, Direction::Input, endpoint, output_rate))
        .transpose()?;
    let block = request.duplex.max_block_size.max(1);
    match (input, output) {
        (None, Some(output)) => play(driver, &output, block, make, &on_error),
        (Some(input), None) => record(driver, &input, block, make, &on_error),
        (Some(input), Some(output)) => {
            let settings = DuplexSettings {
                max_block_size: block,
                ..request.duplex
            };
            duplex(driver, &input, &output, settings, make, &on_error)
        }
        (None, None) => Err(DeviceError::UnsupportedSettings(String::from(
            "neither an input nor an output was asked for",
        ))),
    }
}

// Exclusive streams give no presentation time per period, so the callbacks
// are told no latencies.

fn play<D: Driver>(
    driver: &D,
    output: &Side,
    block: usize,
    make: &mut dyn FnMut(OpenedFormat) -> EngineCallback,
    on_error: &ErrorSink,
) -> Result<Started, DeviceError> {
    let format = OpenedFormat {
        sample_rate: output.request.sample_rate,
        input_channels: 0,
        output_channels: output.channels(),
    };
    let health = StreamHealth::new();
    let mut callback = make(format);
    let mut nothing = AudioBuffer::new(0, block);
    let clock = BlockClock::new(f64::from(format.sample_rate), Arc::clone(&health));
    let mut renderer = BlockRenderer::new(
        format.output_channels,
        block,
        clock,
        move |played: &mut AudioBuffer<f32>, info: &CallbackInfo| {
            nothing.set_num_frames(played.num_frames());
            callback(&nothing, played, info);
        },
    );
    let (stream, opened) = driver
        .play(
            &output.id,
            output.request,
            move |samples, time| match samples {
                Samples::F32(data) => renderer.render(data, time, None),
                Samples::I32(data) => renderer.render(data, time, None),
                Samples::I16(data) => renderer.render(data, time, None),
            },
            lost(&health, on_error),
        )
        .map_err(device_error)?;
    Ok(Started {
        stream: Box::new(stream),
        format,
        input: None,
        output: Some(output.opened(opened, health)),
    })
}

fn record<D: Driver>(
    driver: &D,
    input: &Side,
    block: usize,
    make: &mut dyn FnMut(OpenedFormat) -> EngineCallback,
    on_error: &ErrorSink,
) -> Result<Started, DeviceError> {
    let format = OpenedFormat {
        sample_rate: input.request.sample_rate,
        input_channels: input.channels(),
        output_channels: 0,
    };
    let health = StreamHealth::new();
    let mut callback = make(format);
    let mut nothing = AudioBuffer::new(0, block);
    let clock = BlockClock::new(f64::from(format.sample_rate), Arc::clone(&health));
    let mut capturer = BlockCapturer::new(
        format.input_channels,
        block,
        clock,
        move |captured: &AudioBuffer<f32>, info: &CallbackInfo| {
            nothing.set_num_frames(captured.num_frames());
            callback(captured, &mut nothing, info);
        },
    );
    let (stream, opened) = driver
        .record(
            &input.id,
            input.request,
            move |packet, time| match packet.samples {
                Captured::F32(data) => capturer.capture(data, time, None),
                Captured::I32(data) => capturer.capture(data, time, None),
                Captured::I16(data) => capturer.capture(data, time, None),
            },
            lost(&health, on_error),
        )
        .map_err(device_error)?;
    Ok(Started {
        stream: Box::new(stream),
        format,
        input: Some(input.opened(opened, health)),
        output: None,
    })
}

fn duplex<D: Driver>(
    driver: &D,
    input: &Side,
    output: &Side,
    settings: DuplexSettings,
    make: &mut dyn FnMut(OpenedFormat) -> EngineCallback,
    on_error: &ErrorSink,
) -> Result<Started, DeviceError> {
    let sample_rate = output.request.sample_rate;
    if input.request.sample_rate != sample_rate {
        return Err(DeviceError::SampleRateMismatch {
            input: input.request.sample_rate,
            output: sample_rate,
        });
    }
    let format = OpenedFormat {
        sample_rate,
        input_channels: input.channels(),
        output_channels: output.channels(),
    };
    let input_health = StreamHealth::new();
    let output_health = StreamHealth::new();
    let (mut feeder, mut renderer) = bridge(
        format.input_channels,
        format.output_channels,
        sample_rate as usize,
        settings,
        make(format),
        Arc::new(Stats::default()),
        Arc::clone(&input_health),
        Arc::clone(&output_health),
    );
    // The output first: opening a device takes a while, and what an input
    // recorded meanwhile would stay in the queue as latency.
    let (played, output_format) = driver
        .play(
            &output.id,
            output.request,
            move |samples, time| match samples {
                Samples::F32(data) => renderer.render(data, time, None, monotonic_ns()),
                Samples::I32(data) => renderer.render(data, time, None, monotonic_ns()),
                Samples::I16(data) => renderer.render(data, time, None, monotonic_ns()),
            },
            lost(&output_health, on_error),
        )
        .map_err(device_error)?;
    // An input that will not open drops the output again on the way out.
    let (recorded, input_format) = driver
        .record(
            &input.id,
            input.request,
            move |packet, _| match packet.samples {
                Captured::F32(data) => feeder.feed(data, None, monotonic_ns()),
                Captured::I32(data) => feeder.feed(data, None, monotonic_ns()),
                Captured::I16(data) => feeder.feed(data, None, monotonic_ns()),
            },
            lost(&input_health, on_error),
        )
        .map_err(device_error)?;
    Ok(Started {
        // The input stops first.
        stream: Box::new((recorded, played)),
        format,
        input: Some(input.opened(input_format, input_health)),
        output: Some(output.opened(output_format, output_health)),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;
    use crate::backend::Backend;
    #[cfg(feature = "midi")]
    use crate::midi::{MidiError, MidiSink};
    use crate::{
        DeviceChoice, DeviceIo, DeviceManager, DeviceSetup, DeviceSnapshot, Fallback, HostInfo,
    };

    type Render = Box<dyn FnMut(Samples<'_>, Duration) + Send>;
    type Capture = Box<dyn FnMut(Packet<'_>, Duration) + Send>;
    type Fail = Box<dyn FnMut(WasapiError) + Send>;

    /// The period every stand-in device runs on, in frames: 3 ms at 48 kHz.
    const PERIOD: u32 = 144;

    #[derive(Default)]
    struct State {
        /// Each device's own rate and channels.
        own: HashMap<&'static str, (u32, u16)>,
        /// The rates each device takes with a channel count.
        takes: HashMap<(&'static str, u16), Vec<u32>>,
        /// Devices that will not even say what they take.
        refuses: HashMap<&'static str, WasapiError>,
        /// Devices that will not open.
        fails: HashMap<&'static str, WasapiError>,
        log: Vec<String>,
        render: Option<Render>,
        capture: Option<Capture>,
        errors: HashMap<String, Fail>,
    }

    /// Devices that exist in memory; a test calls their streams' callbacks.
    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<State>>);

    struct FakeStream {
        fake: Fake,
        id: String,
        input: bool,
    }

    impl Drop for FakeStream {
        fn drop(&mut self) {
            let mut state = self.fake.0.lock().unwrap();
            state.log.push(format!("close {}", self.id));
            let render = (!self.input).then(|| state.render.take());
            let capture = self.input.then(|| state.capture.take());
            let error = state.errors.remove(&self.id);
            drop(state);
            drop((render, capture, error));
        }
    }

    impl Fake {
        /// An output "out" and an input "in", both stereo at 48 kHz.
        fn pair() -> Self {
            let fake = Self::default();
            {
                let mut state = fake.0.lock().unwrap();
                for id in ["out", "in"] {
                    state.own.insert(id, (48_000, 2));
                    state.takes.insert((id, 2), vec![44_100, 48_000]);
                }
            }
            fake
        }

        fn state(&self) -> std::sync::MutexGuard<'_, State> {
            self.0.lock().unwrap()
        }

        fn open(
            &self,
            id: &str,
            request: Request,
            input: bool,
            on_error: Fail,
        ) -> Result<(FakeStream, Format), WasapiError> {
            let mut state = self.state();
            state.log.push(format!(
                "open {id} {} Hz {} ch",
                request.sample_rate, request.channels
            ));
            if let Some(error) = state.fails.get(id) {
                return Err(error.clone());
            }
            state.errors.insert(id.to_owned(), on_error);
            let stream = FakeStream {
                fake: self.clone(),
                id: id.to_owned(),
                input,
            };
            let format = Format {
                sample_rate: request.sample_rate,
                channels: request.channels,
                kind: manguebyte_wasapi::SampleKind::F32,
                period_frames: request.period_frames.unwrap_or(PERIOD),
                latency: Duration::from_millis(3),
            };
            Ok((stream, format))
        }

        /// Has the output ask for `frames` stereo frames.
        fn play_period(&self, frames: usize) -> Vec<f32> {
            let mut render = self.state().render.take().expect("an output runs");
            let mut data = vec![0.0f32; frames * 2];
            render(Samples::F32(&mut data), Duration::ZERO);
            self.state().render = Some(render);
            data
        }

        /// Has the input deliver `samples`.
        fn deliver(&self, samples: Captured<'_>) {
            let mut capture = self.state().capture.take().expect("an input runs");
            capture(
                Packet {
                    samples,
                    discontinuity: false,
                },
                Duration::ZERO,
            );
            self.state().capture = Some(capture);
        }

        fn fail(&self, id: &str, error: WasapiError) {
            let mut on_error = self.state().errors.remove(id).expect("it runs");
            on_error(error);
        }
    }

    impl Driver for Fake {
        type Stream = FakeStream;

        fn own(&self, _: Direction, id: &str) -> Result<OwnDevice, DeviceError> {
            let state = self.state();
            let (sample_rate, channels) = *state
                .own
                .get(id)
                .ok_or_else(|| DeviceError::DeviceNotFound(id.to_owned()))?;
            Ok(OwnDevice {
                name: format!("the {id} device"),
                sample_rate,
                channels,
            })
        }

        fn capabilities(&self, id: &str, channels: u16) -> Result<Capabilities, WasapiError> {
            let state = self.state();
            if let Some(error) = state.refuses.get(id) {
                return Err(error.clone());
            }
            let sample_rates = state
                .takes
                .iter()
                .find(|((device, count), _)| *device == id && *count == channels)
                .map(|(_, rates)| rates.clone())
                .unwrap_or_default();
            Ok(Capabilities {
                sample_rates,
                min_period: 30_000,
                default_period: 100_000,
            })
        }

        fn play<R, E>(
            &self,
            id: &str,
            request: Request,
            render: R,
            on_error: E,
        ) -> Result<(FakeStream, Format), WasapiError>
        where
            R: FnMut(Samples<'_>, Duration) + Send + 'static,
            E: FnMut(WasapiError) + Send + 'static,
        {
            let opened = self.open(id, request, false, Box::new(on_error))?;
            self.state().render = Some(Box::new(render));
            Ok(opened)
        }

        fn record<C, E>(
            &self,
            id: &str,
            request: Request,
            capture: C,
            on_error: E,
        ) -> Result<(FakeStream, Format), WasapiError>
        where
            C: FnMut(Packet<'_>, Duration) + Send + 'static,
            E: FnMut(WasapiError) + Send + 'static,
        {
            let opened = self.open(id, request, true, Box::new(on_error))?;
            self.state().capture = Some(Box::new(capture));
            Ok(opened)
        }
    }

    /// What the callback saw of each block: the first input channel, if
    /// any, and the block's timing.
    type Seen = Arc<Mutex<Vec<(Option<Vec<f32>>, usize, CallbackInfo)>>>;

    /// A request, started on `fake`, with a callback that records what it
    /// is given and plays its input back.
    struct Run {
        started: Result<Started, DeviceError>,
        formats: Vec<OpenedFormat>,
        seen: Seen,
        errors: Arc<Mutex<Vec<String>>>,
        /// Held by the callback while it lives.
        callback: Arc<()>,
    }

    fn endpoint(id: &str, settings: StreamSettings) -> Option<Endpoint> {
        Some(Endpoint {
            id: id.to_owned(),
            settings,
        })
    }

    fn run(fake: &Fake, input: Option<Endpoint>, output: Option<Endpoint>) -> Run {
        let request = StreamRequest {
            input,
            output,
            duplex: DuplexSettings::new(256).with_drift_correction(false),
        };
        let seen: Seen = Arc::default();
        let errors: Arc<Mutex<Vec<String>>> = Arc::default();
        let callback = Arc::new(());
        let mut formats = Vec::new();
        let reported = Arc::clone(&errors);
        let started = start(
            fake,
            &request,
            &mut |format| {
                formats.push(format);
                let seen = Arc::clone(&seen);
                let held = Arc::clone(&callback);
                Box::new(move |input, output, info| {
                    let _held = &held;
                    let first = (input.num_channels() > 0).then(|| input.channel(0).to_vec());
                    for (index, channel) in output.iter_channels_mut().enumerate() {
                        if input.num_channels() > 0 {
                            channel.copy_from_slice(input.channel(index % input.num_channels()));
                        } else {
                            channel.fill(0.5);
                        }
                    }
                    seen.lock()
                        .unwrap()
                        .push((first, output.num_channels(), *info));
                })
            },
            Arc::new(move |error: DeviceError| {
                reported.lock().unwrap().push(error.to_string());
            }),
        );
        Run {
            started,
            formats,
            seen,
            errors,
            callback,
        }
    }

    fn plain() -> StreamSettings {
        StreamSettings::default()
    }

    #[test]
    fn an_output_runs_alone() {
        let fake = Fake::pair();
        let run = run(&fake, None, endpoint("out", plain()));
        let started = run.started.unwrap();
        assert!(started.input.is_none());
        let output = started.output.unwrap();
        assert_eq!(output.device.name, "the out device");
        assert_eq!(
            output.settings,
            StreamSettings {
                sample_rate: Some(48_000),
                channels: Some(2),
                buffer_size: Some(PERIOD),
            }
        );
        assert_eq!(fake.play_period(144), vec![0.5; 288]);
        assert_eq!(output.health.frames(), 144);
        let seen = run.seen.lock().unwrap();
        assert_eq!(seen[0].0, None);
        assert_eq!(seen[0].2.output_latency, None);
    }

    #[test]
    fn an_input_runs_alone_and_its_audio_reaches_the_callback() {
        let fake = Fake::pair();
        let run = run(&fake, endpoint("in", plain()), None);
        let started = run.started.unwrap();
        assert!(started.output.is_none());
        assert_eq!(
            run.formats,
            [OpenedFormat {
                sample_rate: 48_000,
                input_channels: 2,
                output_channels: 0,
            }]
        );
        fake.deliver(Captured::I32(&[1 << 30, 0, i32::MIN, 0]));
        fake.deliver(Captured::I16(&[16_384, 0]));
        fake.deliver(Captured::F32(&[0.25, 0.0]));
        // A delivery longer than a block is split.
        fake.deliver(Captured::F32(&[0.0; 600]));
        let input = started.input.unwrap();
        assert_eq!(input.health.frames(), 304);
        let seen = run.seen.lock().unwrap();
        let blocks: Vec<usize> = seen.iter().map(|s| s.0.as_ref().unwrap().len()).collect();
        assert_eq!(blocks, [2, 1, 1, 256, 44]);
        assert_eq!(seen[0].0.as_deref(), Some(&[0.5, -1.0][..]));
        assert_eq!(seen[1].0.as_deref(), Some(&[0.5][..]));
        assert_eq!(seen[2].0.as_deref(), Some(&[0.25][..]));
        assert_eq!(seen[3].2.frame, 4);
        assert!(seen.iter().all(|s| s.1 == 0));
    }

    #[test]
    fn a_duplex_stream_carries_the_input_to_the_output() {
        let fake = Fake::pair();
        let run = run(&fake, endpoint("in", plain()), endpoint("out", plain()));
        let started = run.started.unwrap();
        // The output opens first.
        assert_eq!(
            fake.state().log,
            ["open out 48000 Hz 2 ch", "open in 48000 Hz 2 ch"]
        );
        // Before the reserve of one block is there, the input is silent.
        assert_eq!(fake.play_period(144), vec![0.0; 288]);
        let ramp: Vec<f32> = (0..288).flat_map(|i| [i as f32 / 512.0, 0.0]).collect();
        fake.deliver(Captured::F32(&ramp));
        let heard = fake.play_period(144);
        assert_eq!(heard, ramp[..288]);
        assert_eq!(fake.play_period(144), ramp[288..]);
        let input = started.input.unwrap();
        let output = started.output.unwrap();
        assert_eq!((input.health.frames(), output.health.frames()), (288, 432));
        assert_eq!(input.settings.buffer_size, Some(PERIOD));
        assert_eq!(run.seen.lock().unwrap()[0].2.input_latency, None);

        drop(started.stream);
        assert_eq!(fake.state().log[2..], ["close in", "close out"]);
        assert_eq!(
            Arc::strong_count(&run.callback),
            1,
            "the callback is dropped"
        );
    }

    #[test]
    fn each_side_of_a_duplex_stream_reports_its_own_loss() {
        let fake = Fake::pair();
        let run = run(&fake, endpoint("in", plain()), endpoint("out", plain()));
        let started = run.started.unwrap();
        let input = started.input.unwrap();
        let output = started.output.unwrap();
        fake.fail("in", WasapiError::DeviceLost);
        assert!(input.health.is_lost() && !output.health.is_lost());
        fake.fail("out", WasapiError::Os(String::from("broke")));
        assert!(output.health.is_lost());
        assert_eq!(
            *run.errors.lock().unwrap(),
            [
                "audio backend error: the device went away",
                "audio backend error: WASAPI: broke"
            ]
        );
    }

    #[test]
    fn an_input_that_will_not_open_leaves_nothing_running() {
        let fake = Fake::pair();
        fake.state().fails.insert("in", WasapiError::AccessDenied);
        let run = run(&fake, endpoint("in", plain()), endpoint("out", plain()));
        let error = run.started.err().unwrap();
        assert!(error.to_string().contains("denies access"), "{error}");
        assert_eq!(
            fake.state().log,
            [
                "open out 48000 Hz 2 ch",
                "open in 48000 Hz 2 ch",
                "close out"
            ]
        );
        assert_eq!(Arc::strong_count(&run.callback), 1);
    }

    #[test]
    fn an_output_that_will_not_open_leaves_the_input_alone() {
        let fake = Fake::pair();
        fake.state()
            .fails
            .insert("out", WasapiError::FormatNotSupported(String::from("x")));
        let run = run(&fake, endpoint("in", plain()), endpoint("out", plain()));
        assert!(matches!(
            run.started,
            Err(DeviceError::UnsupportedSettings(_))
        ));
        assert_eq!(fake.state().log, ["open out 48000 Hz 2 ch"]);
        assert_eq!(Arc::strong_count(&run.callback), 1);
    }

    #[test]
    fn an_input_follows_the_output_rate_and_two_rates_are_refused() {
        let fake = Fake::pair();
        fake.state().own.insert("in", (44_100, 2));
        let first = run(&fake, endpoint("in", plain()), endpoint("out", plain()));
        assert_eq!(first.formats[0].sample_rate, 48_000);
        drop(first);

        fake.state().log.clear();
        let second = run(
            &fake,
            endpoint("in", plain().with_sample_rate(44_100)),
            endpoint("out", plain()),
        );
        assert!(matches!(
            second.started,
            Err(DeviceError::SampleRateMismatch {
                input: 44_100,
                output: 48_000
            })
        ));
        assert!(second.formats.is_empty() && fake.state().log.is_empty());
    }

    #[test]
    fn unnamed_channels_are_ones_the_device_takes_exclusively() {
        let fake = Fake::pair();
        // A microphone that is four channels shared, one exclusive.
        fake.state().own.insert("in", (48_000, 4));
        fake.state().takes.remove(&("in", 2));
        fake.state().takes.insert(("in", 1), vec![48_000]);
        let chosen = run(&fake, endpoint("in", plain()), None);
        assert_eq!(chosen.formats[0].input_channels, 1);

        // What the settings name is asked for, whatever the device takes.
        let named = run(&fake, endpoint("in", plain().with_channels(4)), None);
        assert_eq!(named.formats[0].input_channels, 4);

        // Nothing taken: the device's own, so that opening says why.
        let none = run(
            &fake,
            endpoint("in", plain().with_sample_rate(96_000)),
            None,
        );
        assert_eq!(none.formats[0].input_channels, 4);
    }

    #[test]
    fn nothing_asked_for_is_an_error() {
        let fake = Fake::pair();
        let run = run(&fake, None, None);
        assert!(matches!(
            run.started,
            Err(DeviceError::UnsupportedSettings(_))
        ));
        let missing = super::tests::run(&fake, endpoint("gone", plain()), None);
        assert!(matches!(
            missing.started,
            Err(DeviceError::DeviceNotFound(_))
        ));
    }

    fn shared(id: &str, channels: u16) -> DeviceInfo {
        DeviceInfo {
            id: id.to_owned(),
            name: format!("the {id} device"),
            is_default: true,
            default_sample_rate: Some(48_000),
            default_channels: Some(channels),
            configs: vec![ConfigRange {
                channels,
                min_sample_rate: 48_000,
                max_sample_rate: 48_000,
                buffer_size: None,
            }],
        }
    }

    #[test]
    fn a_device_is_described_by_what_it_takes_exclusively() {
        let fake = Fake::pair();
        fake.state().takes.insert(("in", 1), vec![96_000]);
        let info = describe(&fake, shared("in", 2));
        assert_eq!(info.default_channels, Some(2));
        assert_eq!(info.default_sample_rate, Some(48_000));
        let ranges: Vec<_> = info
            .configs
            .iter()
            .map(|c| (c.channels, c.min_sample_rate, c.max_sample_rate))
            .collect();
        assert_eq!(ranges, [(1, 96_000, 96_000), (2, 44_100, 48_000)]);
        assert!(
            info.configs
                .iter()
                .all(|c| c.buffer_size == Some((144, 8_192)))
        );

        // Its own channel count and rate are refused: others stand in.
        let info = describe(&fake, shared("in", 6));
        assert_eq!(info.default_channels, Some(2));
        fake.state().takes.remove(&("in", 2));
        let info = describe(&fake, shared("in", 2));
        assert_eq!(info.default_channels, Some(1));
        assert_eq!(info.default_sample_rate, Some(96_000));
        assert_eq!(info.configs[0].buffer_size, Some((288, 8_192)));

        // A device that takes nothing, or will not say, stays as it was.
        fake.state().takes.clear();
        assert_eq!(describe(&fake, shared("in", 2)), shared("in", 2));
        fake.state()
            .refuses
            .insert("in", WasapiError::ExclusiveNotAllowed);
        assert_eq!(describe(&fake, shared("in", 2)), shared("in", 2));
    }

    /// The exclusive host as the device manager sees it, on stand-ins.
    struct Host(Fake);

    impl Backend for Host {
        fn host_name(&self) -> String {
            String::from(crate::WASAPI_EXCLUSIVE)
        }

        fn hosts(&self) -> Vec<HostInfo> {
            Vec::new()
        }

        fn select_host(&self, _: Option<&str>) -> Result<(), DeviceError> {
            Ok(())
        }

        fn snapshot(&self) -> Result<DeviceSnapshot, DeviceError> {
            let summary = |id: &str| DeviceSummary {
                id: id.to_owned(),
                name: format!("the {id} device"),
            };
            Ok(DeviceSnapshot {
                outputs: vec![summary("out")],
                inputs: vec![summary("in")],
                ..DeviceSnapshot::default()
            })
        }

        fn device_info(&self, _: Direction, id: &str) -> Result<DeviceInfo, DeviceError> {
            Ok(describe(&self.0, shared(id, 2)))
        }

        fn concurrent_streams(&self) -> bool {
            false
        }

        fn start(
            &self,
            request: &StreamRequest,
            make: &mut dyn FnMut(OpenedFormat) -> EngineCallback,
            on_error: ErrorSink,
        ) -> Result<Started, DeviceError> {
            start(&self.0, request, make, on_error)
        }

        #[cfg(feature = "midi")]
        fn connect_midi_input(&self, id: &str, _: MidiSink) -> Result<Box<dyn Send>, MidiError> {
            Err(MidiError::PortNotFound(id.to_owned()))
        }

        #[cfg(feature = "midi")]
        fn connect_midi_output(
            &self,
            id: &str,
        ) -> Result<Box<dyn crate::backend::MidiSender>, MidiError> {
            Err(MidiError::PortNotFound(id.to_owned()))
        }
    }

    #[test]
    fn a_device_manager_keeps_the_output_when_the_input_is_refused() {
        let fake = Fake::pair();
        let mut manager = DeviceManager::with_backend(Arc::new(Host(fake.clone())));
        let setup = DeviceSetup::default().with_input(DeviceChoice::Default);
        let blocks = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&blocks);
        let report = manager
            .start(&setup, move |io: &mut DeviceIo<'_>| {
                seen.lock()
                    .unwrap()
                    .push((io.input.num_channels(), io.num_frames()));
            })
            .unwrap();
        assert!(report.fallbacks.is_empty(), "{report:?}");
        fake.play_period(144);
        assert_eq!(*blocks.lock().unwrap(), [(2, 144)]);
        manager.stop();

        fake.state().fails.insert("in", WasapiError::AccessDenied);
        let report = manager.restart().unwrap();
        assert!(
            matches!(
                &report.fallbacks[..],
                [Fallback::InputDropped { reason, .. }] if reason.contains("denies access")
            ),
            "{report:?}"
        );
        assert!(manager.running_device(Direction::Input).is_none());
        fake.play_period(144);
        assert_eq!(blocks.lock().unwrap().last(), Some(&(0, 144)));
    }

    #[test]
    fn support_says_what_a_device_takes_or_why_not() {
        let fake = Fake::pair();
        let listed = support(&fake, &shared("in", 8));
        assert_eq!(listed.id, "in");
        assert_eq!(listed.name, "the in device");
        assert_eq!(
            listed.outcome,
            Ok(ExclusiveModes {
                channels: vec![ExclusiveChannels {
                    channels: 2,
                    sample_rates: vec![44_100, 48_000],
                }],
                shortest_period: Duration::from_millis(3),
                default_period: Duration::from_millis(10),
            })
        );

        fake.state().takes.clear();
        assert_eq!(
            support(&fake, &shared("in", 2)).outcome,
            Err(ExclusiveRefusal::NoFormat)
        );
        for (error, why) in [
            (
                WasapiError::ExclusiveNotAllowed,
                ExclusiveRefusal::NotAllowed,
            ),
            (
                WasapiError::DeviceBusy(String::new()),
                ExclusiveRefusal::InUse,
            ),
            (WasapiError::AccessDenied, ExclusiveRefusal::AccessDenied),
            (WasapiError::DeviceLost, ExclusiveRefusal::Gone),
            (
                WasapiError::DeviceNotFound(String::new()),
                ExclusiveRefusal::Gone,
            ),
            (
                WasapiError::FormatNotSupported(String::new()),
                ExclusiveRefusal::NoFormat,
            ),
            (
                WasapiError::Os(String::from("odd")),
                ExclusiveRefusal::Other(String::from("WASAPI: odd")),
            ),
        ] {
            fake.state().refuses.insert("in", error);
            assert_eq!(support(&fake, &shared("in", 2)).outcome, Err(why));
        }
    }
}
