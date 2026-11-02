use cpal::{
    BufferSize, SampleFormat, StreamConfig, SupportedBufferSize, SupportedStreamConfig,
    SupportedStreamConfigRange,
};

use crate::DeviceError;

/// What to ask a device for. Anything left as `None` uses the device's own
/// preference.
///
/// ```
/// use manguebyte_devices::StreamSettings;
///
/// let settings = StreamSettings::default()
///     .with_sample_rate(48_000)
///     .with_buffer_size(256);
/// assert_eq!(settings.channels, None);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct StreamSettings {
    /// Sample rate in hertz.
    pub sample_rate: Option<u32>,
    /// Number of channels.
    pub channels: Option<u16>,
    /// Hardware buffer size in frames. Smaller means less latency and more
    /// risk of dropouts.
    pub buffer_size: Option<u32>,
}

impl StreamSettings {
    /// Requests a sample rate in hertz.
    #[must_use]
    pub fn with_sample_rate(mut self, sample_rate: u32) -> Self {
        self.sample_rate = Some(sample_rate);
        self
    }

    /// Requests a channel count.
    #[must_use]
    pub fn with_channels(mut self, channels: u16) -> Self {
        self.channels = Some(channels);
        self
    }

    /// Requests a hardware buffer size in frames.
    #[must_use]
    pub fn with_buffer_size(mut self, frames: u32) -> Self {
        self.buffer_size = Some(frames);
        self
    }
}

/// One family of configurations a device can run with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConfigRange {
    /// Number of channels.
    pub channels: u16,
    /// Lowest supported sample rate, in hertz.
    pub min_sample_rate: u32,
    /// Highest supported sample rate, in hertz.
    pub max_sample_rate: u32,
    /// Smallest and largest hardware buffer size in frames, when the platform
    /// can tell.
    pub buffer_size: Option<(u32, u32)>,
}

impl ConfigRange {
    pub(crate) fn from_backend(range: &SupportedStreamConfigRange) -> Self {
        Self {
            channels: range.channels(),
            min_sample_rate: range.min_sample_rate(),
            max_sample_rate: range.max_sample_rate(),
            buffer_size: match *range.buffer_size() {
                SupportedBufferSize::Range { min, max } => Some((min, max)),
                SupportedBufferSize::Unknown => None,
            },
        }
    }

    /// Whether the device can run at `sample_rate` in this configuration.
    pub fn supports_sample_rate(&self, sample_rate: u32) -> bool {
        (self.min_sample_rate..=self.max_sample_rate).contains(&sample_rate)
    }
}

/// A concrete configuration, ready to build a stream with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Resolved {
    pub(crate) config: StreamConfig,
    pub(crate) format: SampleFormat,
}

impl Resolved {
    pub(crate) fn from_default(default: SupportedStreamConfig) -> Self {
        Self {
            format: default.sample_format(),
            config: default.into(),
        }
    }
}

/// How much we like a device sample format. Lower is better; `None` means we
/// cannot convert it.
///
/// `f32` needs no conversion at all. After that, wider integers lose less.
fn format_rank(format: SampleFormat) -> Option<u8> {
    Some(match format {
        SampleFormat::F32 => 0,
        SampleFormat::I32 => 1,
        SampleFormat::I24 => 2,
        SampleFormat::F64 => 3,
        SampleFormat::I64 => 4,
        SampleFormat::U32 => 5,
        SampleFormat::U64 => 6,
        SampleFormat::I16 => 7,
        SampleFormat::U16 => 8,
        SampleFormat::I8 => 9,
        SampleFormat::U8 => 10,
        _ => return None,
    })
}

/// Picks the device configuration that satisfies `settings`.
///
/// `default` is the device's own preference and fills in whatever `settings`
/// leaves open.
pub(crate) fn resolve(
    ranges: &[SupportedStreamConfigRange],
    default: Option<SupportedStreamConfig>,
    settings: &StreamSettings,
) -> Result<Resolved, DeviceError> {
    if *settings == StreamSettings::default() {
        if let Some(default) = default {
            return Ok(Resolved::from_default(default));
        }
    }

    let sample_rate = settings
        .sample_rate
        .or(default.map(|config| config.sample_rate()));
    let preferred_channels = settings
        .channels
        .or(default.map(|config| config.channels()));

    let best = ranges
        .iter()
        .filter(|range| settings.channels.is_none_or(|c| range.channels() == c))
        .filter(|range| sample_rate.is_none_or(|rate| range.contains_rate(rate)))
        .filter(|range| match (settings.buffer_size, *range.buffer_size()) {
            (Some(frames), SupportedBufferSize::Range { min, max }) => {
                (min..=max).contains(&frames)
            }
            _ => true,
        })
        .filter_map(|range| Some((format_rank(range.sample_format())?, range)))
        .min_by_key(|(rank, range)| {
            let channel_distance =
                preferred_channels.map_or(0, |wanted| range.channels().abs_diff(wanted));
            (channel_distance, *rank)
        })
        .map(|(_, range)| range);

    let Some(range) = best else {
        return Err(DeviceError::UnsupportedSettings(describe(settings)));
    };

    let sample_rate = sample_rate.unwrap_or_else(|| {
        // No preference from anyone: take the common rate closest to 48 kHz.
        48_000.clamp(range.min_sample_rate(), range.max_sample_rate())
    });
    Ok(Resolved {
        format: range.sample_format(),
        config: StreamConfig {
            channels: range.channels(),
            sample_rate,
            buffer_size: settings
                .buffer_size
                .map_or(BufferSize::Default, BufferSize::Fixed),
        },
    })
}

fn describe(settings: &StreamSettings) -> String {
    let mut parts = Vec::new();
    if let Some(rate) = settings.sample_rate {
        parts.push(format!("{rate} Hz"));
    }
    if let Some(channels) = settings.channels {
        parts.push(format!("{channels} channels"));
    }
    if let Some(frames) = settings.buffer_size {
        parts.push(format!("{frames}-frame buffer"));
    }
    if parts.is_empty() {
        String::from("the device reports no usable configuration")
    } else {
        format!("the device cannot run with {}", parts.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(
        channels: u16,
        min: u32,
        max: u32,
        format: SampleFormat,
    ) -> SupportedStreamConfigRange {
        SupportedStreamConfigRange::new(
            channels,
            min,
            max,
            SupportedBufferSize::Range { min: 64, max: 4096 },
            format,
        )
    }

    fn ranges() -> Vec<SupportedStreamConfigRange> {
        vec![
            range(2, 44_100, 96_000, SampleFormat::I16),
            range(2, 44_100, 96_000, SampleFormat::F32),
            range(1, 44_100, 48_000, SampleFormat::I24),
            range(8, 48_000, 48_000, SampleFormat::I32),
        ]
    }

    #[test]
    fn no_settings_means_the_device_default() {
        let default = range(2, 44_100, 96_000, SampleFormat::I16).with_sample_rate(44_100);
        let resolved = resolve(&ranges(), Some(default), &StreamSettings::default()).unwrap();
        assert_eq!(resolved.format, SampleFormat::I16);
        assert_eq!(resolved.config.sample_rate, 44_100);
        assert_eq!(resolved.config.channels, 2);
        assert_eq!(resolved.config.buffer_size, BufferSize::Default);
    }

    #[test]
    fn prefers_float_and_keeps_the_default_channel_count() {
        let default = range(2, 44_100, 96_000, SampleFormat::I16).with_sample_rate(44_100);
        let settings = StreamSettings::default().with_sample_rate(96_000);
        let resolved = resolve(&ranges(), Some(default), &settings).unwrap();
        assert_eq!(resolved.format, SampleFormat::F32);
        assert_eq!(resolved.config.sample_rate, 96_000);
        assert_eq!(resolved.config.channels, 2);
    }

    #[test]
    fn honours_channel_and_buffer_requests() {
        let settings = StreamSettings::default()
            .with_channels(1)
            .with_buffer_size(128);
        let resolved = resolve(&ranges(), None, &settings).unwrap();
        assert_eq!(resolved.config.channels, 1);
        assert_eq!(resolved.format, SampleFormat::I24);
        assert_eq!(resolved.config.sample_rate, 48_000);
        assert_eq!(resolved.config.buffer_size, BufferSize::Fixed(128));
    }

    #[test]
    fn rejects_what_the_device_cannot_do() {
        for settings in [
            StreamSettings::default().with_sample_rate(192_000),
            StreamSettings::default().with_channels(6),
            StreamSettings::default().with_buffer_size(16),
            StreamSettings::default()
                .with_channels(8)
                .with_sample_rate(44_100),
        ] {
            let error = resolve(&ranges(), None, &settings).unwrap_err();
            assert!(
                matches!(error, DeviceError::UnsupportedSettings(_)),
                "{settings:?} gave {error:?}"
            );
        }
    }

    #[test]
    fn a_device_with_no_configurations_is_an_error() {
        let error = resolve(&[], None, &StreamSettings::default()).unwrap_err();
        assert!(matches!(error, DeviceError::UnsupportedSettings(_)));
    }
}
