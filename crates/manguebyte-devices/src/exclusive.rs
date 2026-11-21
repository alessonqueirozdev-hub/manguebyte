//! The `"WASAPI exclusive"` host: WASAPI's output devices, each played by
//! this program alone, through `manguebyte-wasapi`.
//!
//! The devices are the WASAPI host's, by the same identifiers, so a saved
//! setup names the same device in either host. Only outputs are listed:
//! exclusive capture is not there.

use std::sync::Arc;

use manguebyte_core::AudioBuffer;
use manguebyte_wasapi::{Error as WasapiError, Request, Samples};

use crate::backend::{
    EngineCallback, ErrorSink, OpenedEndpoint, OpenedFormat, Started, StreamRequest,
};
use crate::blocks::{BlockClock, BlockRenderer};
use crate::host::WASAPI_EXCLUSIVE;
use crate::hotplug::DeviceSummary;
use crate::timing::StreamHealth;
use crate::{DeviceError, DeviceInfo, OutputDevice, StreamSettings};

/// The longest period the buffer sizes offered go up to, in frames.
const LONGEST_PERIOD: u32 = 8_192;

fn device_error(error: WasapiError) -> DeviceError {
    match error {
        WasapiError::DeviceNotFound(id) => DeviceError::DeviceNotFound(id),
        WasapiError::FormatNotSupported(what) => DeviceError::UnsupportedSettings(format!(
            "the device takes no format exclusively at {what}"
        )),
        other => DeviceError::Other(Box::new(other)),
    }
}

/// `info`, as the device is in exclusive mode: the rates it takes there,
/// and buffer sizes from its shortest period up. A device that cannot say
/// is described as `cpal` described it.
pub(crate) fn describe(mut info: DeviceInfo) -> DeviceInfo {
    let channels = info.default_channels.unwrap_or(2);
    let Ok(caps) = manguebyte_wasapi::capabilities(&info.id, channels) else {
        return info;
    };
    let rate = info.default_sample_rate.unwrap_or(48_000);
    let shortest = caps.min_period_frames(rate).max(1);
    let (low, high) = match (caps.sample_rates.first(), caps.sample_rates.last()) {
        (Some(&low), Some(&high)) => (low, high),
        _ => (rate, rate),
    };
    for config in &mut info.configs {
        config.min_sample_rate = low;
        config.max_sample_rate = high;
        config.buffer_size = Some((shortest, LONGEST_PERIOD));
    }
    info
}

/// Plays `request`'s output exclusively. An input cannot be opened so.
pub(crate) fn start(
    request: &StreamRequest,
    make: &mut dyn FnMut(OpenedFormat) -> EngineCallback,
    on_error: ErrorSink,
) -> Result<Started, DeviceError> {
    if request.input.is_some() {
        return Err(DeviceError::UnsupportedSettings(format!(
            "the {WASAPI_EXCLUSIVE} host only plays: choose WASAPI to record"
        )));
    }
    let Some(endpoint) = request.output.as_ref() else {
        return Err(DeviceError::UnsupportedSettings(String::from(
            "neither an input nor an output was asked for",
        )));
    };
    // The device's own rate and channels fill in what the settings leave.
    let (name, own_rate, own_channels) = {
        let device = OutputDevice::open_by_id(&endpoint.id)?;
        (
            device.name().to_owned(),
            device.sample_rate(),
            device.num_channels(),
        )
    };
    let sample_rate = endpoint.settings.sample_rate.unwrap_or(own_rate as u32);
    let channels = endpoint
        .settings
        .channels
        .unwrap_or_else(|| u16::try_from(own_channels).unwrap_or(2));
    let block = request.duplex.max_block_size.max(1);
    let format = OpenedFormat {
        sample_rate,
        input_channels: 0,
        output_channels: usize::from(channels),
    };

    let health = StreamHealth::new();
    let mut callback = make(format);
    let mut nothing = AudioBuffer::new(0, block);
    let clock = BlockClock::new(f64::from(sample_rate), Arc::clone(&health));
    let mut renderer = BlockRenderer::new(
        usize::from(channels),
        block,
        clock,
        move |played: &mut AudioBuffer<f32>, info: &crate::CallbackInfo| {
            nothing.set_num_frames(played.num_frames());
            callback(&nothing, played, info);
        },
    );
    let watched = Arc::clone(&health);
    let (stream, opened) = manguebyte_wasapi::start(
        &endpoint.id,
        Request {
            sample_rate,
            channels,
            period_frames: endpoint.settings.buffer_size,
        },
        move |samples, time| {
            // Exclusive streams give no presentation time per period.
            let latency = None;
            match samples {
                Samples::F32(data) => renderer.render(data, time, latency),
                Samples::I32(data) => renderer.render(data, time, latency),
                Samples::I16(data) => renderer.render(data, time, latency),
            }
        },
        move |error| {
            watched.mark_lost();
            on_error(device_error(error));
        },
    )
    .map_err(device_error)?;

    Ok(Started {
        stream: Box::new(stream),
        format,
        input: None,
        output: Some(OpenedEndpoint {
            device: DeviceSummary {
                id: endpoint.id.clone(),
                name,
            },
            settings: StreamSettings {
                sample_rate: Some(opened.sample_rate),
                channels: Some(opened.channels),
                buffer_size: Some(opened.period_frames),
            },
            health,
        }),
    })
}
