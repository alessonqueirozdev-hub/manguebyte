use std::fmt;

/// What went wrong while talking to an audio device.
#[derive(Debug)]
#[non_exhaustive]
pub enum DeviceError {
    /// The system has no default output device.
    NoOutputDevice,
    /// The system has no default input device.
    NoInputDevice,
    /// No device with this identifier is connected.
    DeviceNotFound(String),
    /// No audio host by this name was built in, or it cannot be used now,
    /// for example JACK while its server is not running.
    HostUnavailable(String),
    /// The device wants a sample format this crate cannot convert.
    UnsupportedSampleFormat(String),
    /// The device cannot run with the requested [`StreamSettings`].
    ///
    /// [`StreamSettings`]: crate::StreamSettings
    UnsupportedSettings(String),
    /// A duplex stream was asked to join two devices running at different
    /// sample rates.
    SampleRateMismatch {
        /// Sample rate of the input device, in hertz.
        input: u32,
        /// Sample rate of the output device, in hertz.
        output: u32,
    },
    /// A device manager was asked to restart before it was ever started,
    /// or a stream that stopped never gave its callback back.
    NoCallback,
    /// The platform audio backend reported an error.
    Backend(cpal::Error),
    /// A backend outside this crate, plugged in through
    /// [`AudioBackend`](crate::AudioBackend), reported an error of its own.
    Other(Box<dyn std::error::Error + Send + Sync>),
}

impl DeviceError {
    /// Whether the device went away, for example because it was unplugged.
    ///
    /// A stream that reports this will not produce audio again; open a device
    /// and start a new one.
    pub fn is_device_lost(&self) -> bool {
        matches!(
            self,
            Self::Backend(error) if error.kind() == cpal::ErrorKind::DeviceNotAvailable
        )
    }
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoOutputDevice => f.write_str("no default audio output device"),
            Self::NoInputDevice => f.write_str("no default audio input device"),
            Self::DeviceNotFound(id) => write!(f, "no audio device with id \"{id}\""),
            Self::HostUnavailable(name) => write!(f, "the audio host \"{name}\" is not available"),
            Self::UnsupportedSampleFormat(format) => {
                write!(f, "unsupported device sample format: {format}")
            }
            Self::UnsupportedSettings(reason) => {
                write!(f, "unsupported stream settings: {reason}")
            }
            Self::SampleRateMismatch { input, output } => {
                write!(f, "input runs at {input} Hz but output runs at {output} Hz")
            }
            Self::NoCallback => f.write_str("the device manager has no callback to run"),
            Self::Backend(error) => write!(f, "audio backend error: {error}"),
            Self::Other(error) => write!(f, "audio backend error: {error}"),
        }
    }
}

impl std::error::Error for DeviceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Backend(error) => Some(error),
            Self::Other(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

impl From<cpal::Error> for DeviceError {
    fn from(error: cpal::Error) -> Self {
        Self::Backend(error)
    }
}
