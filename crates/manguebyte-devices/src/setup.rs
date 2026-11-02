//! A device setup that can be saved, and restored with fallbacks.

use std::fmt;
use std::str::FromStr;

use crate::{DeviceInfo, DeviceSnapshot, DeviceSummary, Direction, StreamSettings};

const HEADER: &str = "manguebyte-devices-setup";
const VERSION: u32 = 1;

/// A device or MIDI port as a setup remembers it: its identifier, and its
/// name in case the identifier changes, as ALSA's and some MIDI ports' do
/// when they are plugged in again in another order.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceRef {
    /// Stable identifier, as in [`DeviceInfo::id`].
    pub id: String,
    /// Human-readable name.
    pub name: String,
}

impl DeviceRef {
    /// A reference to a device by identifier and name.
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
        }
    }
}

impl From<&DeviceSummary> for DeviceRef {
    fn from(device: &DeviceSummary) -> Self {
        Self::new(device.id.clone(), device.name.clone())
    }
}

impl From<&DeviceInfo> for DeviceRef {
    fn from(device: &DeviceInfo) -> Self {
        Self::new(device.id.clone(), device.name.clone())
    }
}

/// Which device to use in one direction.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DeviceChoice {
    /// None: the direction is off.
    None,
    /// Whichever device the system has as its default.
    Default,
    /// This device, or, when it is missing, one with the same name, and
    /// failing that the default.
    Device(DeviceRef),
}

/// Everything needed to open the same devices again in a later run.
///
/// It is written as text, one `key=value` per line under a versioned
/// header, like parameter state (`manguebyte_params::ParamState`), so it
/// can go into any settings file. Reading ignores keys it does not know, so
/// a setup saved by a newer version still opens.
///
/// ```
/// use manguebyte_devices::{DeviceChoice, DeviceRef, DeviceSetup};
///
/// let setup = DeviceSetup::default()
///     .with_output(DeviceChoice::Device(DeviceRef::new("wasapi:{0.0.0}", "Speakers")))
///     .with_input(DeviceChoice::Default)
///     .with_sample_rate(48_000)
///     .with_buffer_size(256);
/// let saved = setup.to_string();
/// assert!(saved.starts_with("manguebyte-devices-setup 1\n"));
/// assert_eq!(saved.parse::<DeviceSetup>()?, setup);
/// # Ok::<(), manguebyte_devices::ParseSetupError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceSetup {
    /// The audio host, by [`HostInfo::name`](crate::HostInfo::name). `None`
    /// is the system's default host.
    pub host: Option<String>,
    /// The output device.
    pub output: DeviceChoice,
    /// The input device.
    pub input: DeviceChoice,
    /// Sample rate in hertz. `None` takes the output device's (or, with no
    /// output, the input device's) own rate.
    pub sample_rate: Option<u32>,
    /// Hardware buffer size in frames. `None` leaves it to the device.
    pub buffer_size: Option<u32>,
    /// Output channel count. `None` takes the device's own.
    pub output_channels: Option<u16>,
    /// Input channel count. `None` takes the device's own.
    pub input_channels: Option<u16>,
    /// MIDI input ports, merged into the callback's MIDI.
    pub midi_inputs: Vec<DeviceRef>,
    /// The MIDI output port that `DeviceManager::send_midi` sends to.
    pub midi_output: Option<DeviceRef>,
}

impl Default for DeviceSetup {
    /// The default output device and no input: asking for an input can
    /// make the system ask the user for the microphone.
    fn default() -> Self {
        Self {
            host: None,
            output: DeviceChoice::Default,
            input: DeviceChoice::None,
            sample_rate: None,
            buffer_size: None,
            output_channels: None,
            input_channels: None,
            midi_inputs: Vec::new(),
            midi_output: None,
        }
    }
}

impl DeviceSetup {
    /// Sets the audio host by name.
    #[must_use]
    pub fn with_host(mut self, name: impl Into<String>) -> Self {
        self.host = Some(name.into());
        self
    }

    /// Sets the output device.
    #[must_use]
    pub fn with_output(mut self, choice: DeviceChoice) -> Self {
        self.output = choice;
        self
    }

    /// Sets the input device.
    #[must_use]
    pub fn with_input(mut self, choice: DeviceChoice) -> Self {
        self.input = choice;
        self
    }

    /// Sets the sample rate, in hertz.
    #[must_use]
    pub fn with_sample_rate(mut self, sample_rate: u32) -> Self {
        self.sample_rate = Some(sample_rate);
        self
    }

    /// Sets the hardware buffer size, in frames.
    #[must_use]
    pub fn with_buffer_size(mut self, frames: u32) -> Self {
        self.buffer_size = Some(frames);
        self
    }

    /// Sets the channel count of one direction.
    #[must_use]
    pub fn with_channels(mut self, direction: Direction, channels: u16) -> Self {
        *self.channels_mut(direction) = Some(channels);
        self
    }

    /// Adds a MIDI input port.
    #[must_use]
    pub fn with_midi_input(mut self, port: DeviceRef) -> Self {
        self.midi_inputs.push(port);
        self
    }

    /// Sets the MIDI output port.
    #[must_use]
    pub fn with_midi_output(mut self, port: DeviceRef) -> Self {
        self.midi_output = Some(port);
        self
    }

    /// The device choice of one direction.
    pub fn device(&self, direction: Direction) -> &DeviceChoice {
        match direction {
            Direction::Input => &self.input,
            Direction::Output => &self.output,
        }
    }

    /// The device choice of one direction, to change.
    pub fn device_mut(&mut self, direction: Direction) -> &mut DeviceChoice {
        match direction {
            Direction::Input => &mut self.input,
            Direction::Output => &mut self.output,
        }
    }

    /// The channel count of one direction.
    pub fn channels(&self, direction: Direction) -> Option<u16> {
        match direction {
            Direction::Input => self.input_channels,
            Direction::Output => self.output_channels,
        }
    }

    fn channels_mut(&mut self, direction: Direction) -> &mut Option<u16> {
        match direction {
            Direction::Input => &mut self.input_channels,
            Direction::Output => &mut self.output_channels,
        }
    }
}

/// Escapes what would break a line: backslash, tab, carriage return and
/// line feed.
fn escape(text: &str, out: &mut fmt::Formatter<'_>) -> fmt::Result {
    for character in text.chars() {
        match character {
            '\\' => out.write_str("\\\\")?,
            '\t' => out.write_str("\\t")?,
            '\n' => out.write_str("\\n")?,
            '\r' => out.write_str("\\r")?,
            other => fmt::Write::write_char(out, other)?,
        }
    }
    Ok(())
}

fn unescape(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        out.push(match characters.next()? {
            '\\' => '\\',
            't' => '\t',
            'n' => '\n',
            'r' => '\r',
            _ => return None,
        });
    }
    Some(out)
}

fn write_ref(port: &DeviceRef, out: &mut fmt::Formatter<'_>) -> fmt::Result {
    escape(&port.id, out)?;
    out.write_str("\t")?;
    escape(&port.name, out)
}

fn parse_ref(value: &str) -> Option<DeviceRef> {
    let (id, name) = value.split_once('\t').unwrap_or((value, ""));
    let id = unescape(id)?;
    if id.is_empty() {
        return None;
    }
    Some(DeviceRef {
        id,
        name: unescape(name)?,
    })
}

impl fmt::Display for DeviceSetup {
    /// Writes the setup as text that [`FromStr`] reads back.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{HEADER} {VERSION}")?;
        if let Some(host) = &self.host {
            f.write_str("host=")?;
            escape(host, f)?;
            writeln!(f)?;
        }
        for (key, choice) in [("output", &self.output), ("input", &self.input)] {
            write!(f, "{key}=")?;
            match choice {
                DeviceChoice::None => f.write_str("none")?,
                DeviceChoice::Default => f.write_str("default")?,
                DeviceChoice::Device(device) => {
                    f.write_str("device:")?;
                    write_ref(device, f)?;
                }
            }
            writeln!(f)?;
        }
        if let Some(rate) = self.sample_rate {
            writeln!(f, "sample_rate={rate}")?;
        }
        if let Some(frames) = self.buffer_size {
            writeln!(f, "buffer_size={frames}")?;
        }
        if let Some(channels) = self.output_channels {
            writeln!(f, "output_channels={channels}")?;
        }
        if let Some(channels) = self.input_channels {
            writeln!(f, "input_channels={channels}")?;
        }
        for port in &self.midi_inputs {
            f.write_str("midi_input=")?;
            write_ref(port, f)?;
            writeln!(f)?;
        }
        if let Some(port) = &self.midi_output {
            f.write_str("midi_output=")?;
            write_ref(port, f)?;
            writeln!(f)?;
        }
        Ok(())
    }
}

/// Why text could not be read as a [`DeviceSetup`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseSetupError {
    /// The text does not start with the `manguebyte-devices-setup` header.
    MissingHeader,
    /// The setup was written by a newer, incompatible version of the format.
    UnsupportedVersion(u32),
    /// A line is not of the form `key=value`, or a known key has a value it
    /// cannot take. Holds the 1-based line number.
    MalformedLine(usize),
}

impl fmt::Display for ParseSetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHeader => f.write_str("not a device setup: header line missing"),
            Self::UnsupportedVersion(version) => {
                write!(f, "device setup format version {version} is not supported")
            }
            Self::MalformedLine(line) => write!(f, "device setup line {line} is malformed"),
        }
    }
}

impl std::error::Error for ParseSetupError {}

fn positive<T: FromStr + Default + PartialOrd>(value: &str) -> Option<T> {
    value
        .trim()
        .parse::<T>()
        .ok()
        .filter(|number| *number > T::default())
}

impl FromStr for DeviceSetup {
    type Err = ParseSetupError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut lines = text.lines().enumerate();
        let version = lines
            .next()
            .and_then(|(_, line)| line.trim().strip_prefix(HEADER))
            .and_then(|rest| rest.trim().parse::<u32>().ok())
            .ok_or(ParseSetupError::MissingHeader)?;
        if version != VERSION {
            return Err(ParseSetupError::UnsupportedVersion(version));
        }

        let mut setup = Self::default();
        for (index, line) in lines {
            // Only the line break is trimmed: a value may end in spaces.
            let line = line.trim_end_matches('\r');
            if line.trim().is_empty() {
                continue;
            }
            let malformed = ParseSetupError::MalformedLine(index + 1);
            let (key, value) = line.split_once('=').ok_or(malformed.clone())?;
            let ok = match key.trim() {
                "host" => unescape(value).map(|host| setup.host = Some(host)),
                "output" | "input" => {
                    let choice = match value {
                        "none" => Some(DeviceChoice::None),
                        "default" => Some(DeviceChoice::Default),
                        other => other
                            .strip_prefix("device:")
                            .and_then(parse_ref)
                            .map(DeviceChoice::Device),
                    };
                    let direction = if key.trim() == "output" {
                        Direction::Output
                    } else {
                        Direction::Input
                    };
                    choice.map(|choice| *setup.device_mut(direction) = choice)
                }
                "sample_rate" => positive(value).map(|rate| setup.sample_rate = Some(rate)),
                "buffer_size" => positive(value).map(|frames| setup.buffer_size = Some(frames)),
                "output_channels" => positive(value).map(|n| setup.output_channels = Some(n)),
                "input_channels" => positive(value).map(|n| setup.input_channels = Some(n)),
                "midi_input" => parse_ref(value).map(|port| setup.midi_inputs.push(port)),
                "midi_output" => parse_ref(value).map(|port| setup.midi_output = Some(port)),
                // Written by a newer version: skipped, so the rest still
                // opens.
                _ => Some(()),
            };
            ok.ok_or(malformed)?;
        }
        Ok(setup)
    }
}

/// Something a restored setup could not have as it was saved, and what
/// was used instead.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Fallback {
    /// The saved host cannot be used now; another one was.
    HostUnavailable {
        /// The saved host.
        wanted: String,
        /// The host used instead.
        used: String,
    },
    /// The saved device is not connected; another one was used: one with
    /// the same name when there is one (its identifier changed), else the
    /// default.
    DeviceReplaced {
        /// Which side.
        direction: Direction,
        /// The saved device.
        wanted: DeviceRef,
        /// The device used instead.
        used: DeviceSummary,
        /// Whether `used` was found by its name.
        by_name: bool,
    },
    /// No device at all could be used in this direction, so it is off.
    NoDevice {
        /// Which side.
        direction: Direction,
        /// The saved device, when one was named.
        wanted: Option<DeviceRef>,
    },
    /// The saved sample rate is not possible with the devices in use.
    SampleRate {
        /// The saved rate, in hertz.
        wanted: u32,
        /// The rate used instead.
        used: u32,
    },
    /// The saved channel count is not possible on the device in use.
    Channels {
        /// Which side.
        direction: Direction,
        /// The saved count.
        wanted: u16,
        /// The count used instead.
        used: u16,
    },
    /// The saved buffer size is not possible; the device chooses.
    BufferSize {
        /// The saved size, in frames.
        wanted: u32,
    },
    /// The input could not run together with the output, so it is off.
    InputDropped {
        /// The input device.
        device: DeviceSummary,
        /// Why.
        reason: String,
    },
    /// The saved MIDI port is not connected, but one with the same name
    /// is, and was used.
    MidiPortReplaced {
        /// Which side.
        direction: Direction,
        /// The saved port.
        wanted: DeviceRef,
        /// The port used instead.
        used: DeviceSummary,
    },
    /// The saved MIDI port could not be opened, and was left out.
    MidiPortMissing {
        /// Which side.
        direction: Direction,
        /// The saved port.
        wanted: DeviceRef,
        /// Why.
        reason: String,
    },
}

impl fmt::Display for Fallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostUnavailable { wanted, used } => {
                write!(f, "the audio host {wanted} is not available; using {used}")
            }
            Self::DeviceReplaced {
                direction,
                wanted,
                used,
                by_name,
            } => write!(
                f,
                "the {} device \"{}\" is not connected; using {}\"{}\"",
                direction.as_str(),
                wanted.name,
                if *by_name {
                    "the one with its name, "
                } else {
                    ""
                },
                used.name
            ),
            Self::NoDevice { direction, .. } => {
                write!(f, "no {} device can be used", direction.as_str())
            }
            Self::SampleRate { wanted, used } => {
                write!(f, "{wanted} Hz is not possible; running at {used} Hz")
            }
            Self::Channels {
                direction,
                wanted,
                used,
            } => write!(
                f,
                "{wanted} {} channels are not possible; using {used}",
                direction.as_str()
            ),
            Self::BufferSize { wanted } => write!(
                f,
                "a buffer of {wanted} frames is not possible; the device chooses"
            ),
            Self::InputDropped { device, reason } => {
                write!(f, "the input \"{}\" is off: {reason}", device.name)
            }
            Self::MidiPortReplaced {
                direction,
                wanted,
                used,
            } => write!(
                f,
                "the MIDI {} \"{}\" is now \"{}\"",
                direction.as_str(),
                wanted.name,
                used.name
            ),
            Self::MidiPortMissing {
                direction,
                wanted,
                reason,
            } => write!(
                f,
                "the MIDI {} \"{}\" is left out: {reason}",
                direction.as_str(),
                wanted.name
            ),
        }
    }
}

/// What restoring a setup had to change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreReport {
    /// Every difference between the setup asked for and what runs, in the
    /// order they were found.
    pub fallbacks: Vec<Fallback>,
}

impl RestoreReport {
    /// Whether everything is as the setup asked.
    pub fn is_exact(&self) -> bool {
        self.fallbacks.is_empty()
    }
}

/// What to open, worked out from a setup and what is connected.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Plan {
    pub(crate) input: Option<(DeviceSummary, StreamSettings)>,
    pub(crate) output: Option<(DeviceSummary, StreamSettings)>,
    pub(crate) midi_inputs: Vec<DeviceSummary>,
    pub(crate) midi_output: Option<DeviceSummary>,
    pub(crate) fallbacks: Vec<Fallback>,
}

/// Whether `device` can run at `rate`. A device whose configurations
/// could not be read is taken at its default rate only.
fn runs_at(device: &DeviceInfo, rate: u32) -> bool {
    if device.configs.is_empty() {
        device.default_sample_rate == Some(rate)
    } else {
        device.supports_sample_rate(rate)
    }
}

fn channels_at(device: &DeviceInfo, rate: u32, channels: u16) -> bool {
    if device.configs.is_empty() {
        device.default_channels == Some(channels)
    } else {
        device
            .configs
            .iter()
            .any(|config| config.channels == channels && config.supports_sample_rate(rate))
    }
}

fn buffer_fits(device: &DeviceInfo, frames: u32) -> bool {
    device.configs.iter().all(|config| {
        config
            .buffer_size
            .is_none_or(|(min, max)| (min..=max).contains(&frames))
    })
}

/// Picks the device for one direction.
fn choose(
    direction: Direction,
    choice: &DeviceChoice,
    snapshot: &DeviceSnapshot,
    fallbacks: &mut Vec<Fallback>,
) -> Option<DeviceSummary> {
    let devices = snapshot.devices(direction);
    let fallback_device = || {
        snapshot
            .default_device(direction)
            .or_else(|| devices.first())
            .cloned()
    };
    match choice {
        DeviceChoice::None => None,
        DeviceChoice::Default => {
            let found = fallback_device();
            if found.is_none() {
                fallbacks.push(Fallback::NoDevice {
                    direction,
                    wanted: None,
                });
            }
            found
        }
        DeviceChoice::Device(wanted) => {
            if let Some(device) = devices.iter().find(|device| device.id == wanted.id) {
                return Some(device.clone());
            }
            let by_name = devices
                .iter()
                .find(|device| !wanted.name.is_empty() && device.name == wanted.name)
                .cloned();
            let (used, by_name) = match by_name {
                Some(device) => (Some(device), true),
                None => (fallback_device(), false),
            };
            match &used {
                Some(device) => fallbacks.push(Fallback::DeviceReplaced {
                    direction,
                    wanted: wanted.clone(),
                    used: device.clone(),
                    by_name,
                }),
                None => fallbacks.push(Fallback::NoDevice {
                    direction,
                    wanted: Some(wanted.clone()),
                }),
            }
            used
        }
    }
}

/// Works out what to open for `setup` among what `snapshot` lists.
/// `describe` gives a device's configurations; a device it knows nothing
/// about cannot be opened.
pub(crate) fn plan(
    setup: &DeviceSetup,
    snapshot: &DeviceSnapshot,
    describe: &dyn Fn(Direction, &str) -> Option<DeviceInfo>,
) -> Plan {
    let mut fallbacks = Vec::new();
    let chosen = |direction: Direction, fallbacks: &mut Vec<Fallback>| {
        let device = choose(direction, setup.device(direction), snapshot, fallbacks)?;
        match describe(direction, &device.id) {
            Some(info) => Some((device, info)),
            None => {
                fallbacks.push(Fallback::NoDevice {
                    direction,
                    wanted: Some(DeviceRef::from(&device)),
                });
                None
            }
        }
    };
    let output = chosen(Direction::Output, &mut fallbacks);
    let mut input = chosen(Direction::Input, &mut fallbacks);

    // One rate for both: the saved one if both can run at it, then each
    // device's own, then the common rates.
    let mut candidates: Vec<u32> = Vec::new();
    candidates.extend(setup.sample_rate);
    for (_, info) in output.iter().chain(&input) {
        candidates.extend(info.default_sample_rate);
    }
    candidates.extend([48_000, 44_100]);
    let all_run_at = |rate: u32, devices: &[&(DeviceSummary, DeviceInfo)]| {
        devices.iter().all(|(_, info)| runs_at(info, rate))
    };
    let both: Vec<_> = output.iter().chain(&input).collect();
    let mut rate = candidates
        .iter()
        .copied()
        .find(|&rate| all_run_at(rate, &both));
    if rate.is_none() {
        // The two cannot agree. Keep the output, and drop the input.
        if let (Some(_), Some((device, _))) = (&output, &input) {
            fallbacks.push(Fallback::InputDropped {
                device: device.clone(),
                reason: String::from("it cannot run at any sample rate the output can"),
            });
            input = None;
        }
        let only: Vec<_> = output.iter().chain(&input).collect();
        rate = candidates
            .iter()
            .copied()
            .find(|&rate| all_run_at(rate, &only));
    }
    if let (Some(wanted), Some(used)) = (setup.sample_rate, rate) {
        if wanted != used {
            fallbacks.push(Fallback::SampleRate { wanted, used });
        }
    }

    let settings = |direction: Direction,
                    info: &DeviceInfo,
                    fallbacks: &mut Vec<Fallback>|
     -> StreamSettings {
        let mut settings = StreamSettings {
            sample_rate: rate,
            ..StreamSettings::default()
        };
        if let (Some(wanted), Some(rate)) = (setup.channels(direction), rate) {
            if channels_at(info, rate, wanted) {
                settings.channels = Some(wanted);
            } else {
                let used = info
                    .default_channels
                    .filter(|&channels| channels_at(info, rate, channels))
                    .or_else(|| {
                        info.configs
                            .iter()
                            .filter(|config| config.supports_sample_rate(rate))
                            .map(|config| config.channels)
                            .max()
                    });
                if let Some(used) = used {
                    settings.channels = Some(used);
                    fallbacks.push(Fallback::Channels {
                        direction,
                        wanted,
                        used,
                    });
                }
            }
        }
        settings
    };
    let output = output.map(|(device, info)| {
        let settings = settings(Direction::Output, &info, &mut fallbacks);
        (device, info, settings)
    });
    let input = input.map(|(device, info)| {
        let settings = settings(Direction::Input, &info, &mut fallbacks);
        (device, info, settings)
    });

    let mut buffer_size = setup.buffer_size;
    if let Some(wanted) = buffer_size {
        let fits = output
            .iter()
            .chain(&input)
            .all(|(_, info, _)| buffer_fits(info, wanted));
        if !fits {
            fallbacks.push(Fallback::BufferSize { wanted });
            buffer_size = None;
        }
    }
    let finish = |(device, _, mut settings): (DeviceSummary, DeviceInfo, StreamSettings)| {
        settings.buffer_size = buffer_size;
        (device, settings)
    };

    let midi = |direction: Direction, wanted: &DeviceRef, fallbacks: &mut Vec<Fallback>| {
        let ports = snapshot.midi_ports(direction);
        if let Some(port) = ports.iter().find(|port| port.id == wanted.id) {
            return Some(port.clone());
        }
        match ports
            .iter()
            .find(|port| !wanted.name.is_empty() && port.name == wanted.name)
        {
            Some(port) => {
                fallbacks.push(Fallback::MidiPortReplaced {
                    direction,
                    wanted: wanted.clone(),
                    used: port.clone(),
                });
                Some(port.clone())
            }
            None => {
                fallbacks.push(Fallback::MidiPortMissing {
                    direction,
                    wanted: wanted.clone(),
                    reason: String::from("it is not connected"),
                });
                None
            }
        }
    };
    let mut midi_inputs: Vec<DeviceSummary> = Vec::new();
    for wanted in &setup.midi_inputs {
        if let Some(port) = midi(Direction::Input, wanted, &mut fallbacks) {
            if !midi_inputs.contains(&port) {
                midi_inputs.push(port);
            }
        }
    }
    let midi_output = setup
        .midi_output
        .as_ref()
        .and_then(|wanted| midi(Direction::Output, wanted, &mut fallbacks));

    Plan {
        output: output.map(finish),
        input: input.map(finish),
        midi_inputs,
        midi_output,
        fallbacks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ConfigRange;

    fn device(id: &str, name: &str, channels: u16, rates: (u32, u32), default: u32) -> DeviceInfo {
        DeviceInfo {
            id: id.into(),
            name: name.into(),
            is_default: false,
            default_sample_rate: Some(default),
            default_channels: Some(channels),
            configs: (1..=channels)
                .map(|channels| ConfigRange {
                    channels,
                    min_sample_rate: rates.0,
                    max_sample_rate: rates.1,
                    buffer_size: Some((32, 2_048)),
                })
                .collect(),
        }
    }

    fn summary(info: &DeviceInfo) -> DeviceSummary {
        DeviceSummary {
            id: info.id.clone(),
            name: info.name.clone(),
        }
    }

    struct World {
        outputs: Vec<DeviceInfo>,
        inputs: Vec<DeviceInfo>,
        snapshot: DeviceSnapshot,
    }

    impl World {
        fn new(outputs: Vec<DeviceInfo>, inputs: Vec<DeviceInfo>) -> Self {
            let snapshot = DeviceSnapshot {
                outputs: outputs.iter().map(summary).collect(),
                inputs: inputs.iter().map(summary).collect(),
                default_output: outputs.first().map(|d| d.id.clone()),
                default_input: inputs.first().map(|d| d.id.clone()),
                midi_inputs: vec![
                    DeviceSummary {
                        id: "midi-1".into(),
                        name: "Keys".into(),
                    },
                    DeviceSummary {
                        id: "midi-7".into(),
                        name: "Pads".into(),
                    },
                ],
                midi_outputs: Vec::new(),
            };
            Self {
                outputs,
                inputs,
                snapshot,
            }
        }

        fn plan(&self, setup: &DeviceSetup) -> Plan {
            plan(setup, &self.snapshot, &|direction, id| {
                let list = match direction {
                    Direction::Output => &self.outputs,
                    Direction::Input => &self.inputs,
                };
                list.iter().find(|device| device.id == id).cloned()
            })
        }
    }

    fn standard() -> World {
        World::new(
            vec![
                device("out-builtin", "Speakers", 2, (44_100, 48_000), 48_000),
                device("out-usb", "Interface", 8, (44_100, 192_000), 44_100),
            ],
            vec![
                device("in-builtin", "Microphone", 1, (48_000, 48_000), 48_000),
                device("in-usb", "Interface", 8, (44_100, 192_000), 44_100),
            ],
        )
    }

    fn usb_setup() -> DeviceSetup {
        DeviceSetup::default()
            .with_output(DeviceChoice::Device(DeviceRef::new("out-usb", "Interface")))
            .with_input(DeviceChoice::Device(DeviceRef::new("in-usb", "Interface")))
            .with_sample_rate(96_000)
            .with_buffer_size(128)
            .with_channels(Direction::Output, 4)
            .with_channels(Direction::Input, 2)
            .with_midi_input(DeviceRef::new("midi-1", "Keys"))
    }

    #[test]
    fn a_setup_round_trips_through_text() {
        let setup = usb_setup()
            .with_host("ALSA")
            .with_midi_input(DeviceRef::new(
                "odd\tid\\with\nbreaks",
                "name = with = signs ",
            ))
            .with_midi_output(DeviceRef::new("128:0", ""));
        let text = setup.to_string();
        assert_eq!(text.lines().count(), 11, "{text}");
        assert_eq!(text.parse::<DeviceSetup>().unwrap(), setup);

        let empty = DeviceSetup::default().with_output(DeviceChoice::None);
        assert_eq!(empty.to_string().parse::<DeviceSetup>().unwrap(), empty);
        assert_eq!(
            DeviceSetup::default()
                .to_string()
                .parse::<DeviceSetup>()
                .unwrap(),
            DeviceSetup::default()
        );
    }

    #[test]
    fn reading_skips_unknown_keys_and_refuses_bad_values() {
        let text = "manguebyte-devices-setup 1\r\nfuture_key=whatever\r\n\r\nsample_rate=44100\r\n";
        let setup: DeviceSetup = text.parse().unwrap();
        assert_eq!(setup.sample_rate, Some(44_100));

        assert_eq!(
            "".parse::<DeviceSetup>(),
            Err(ParseSetupError::MissingHeader)
        );
        assert_eq!(
            "manguebyte-devices-setup 2\n".parse::<DeviceSetup>(),
            Err(ParseSetupError::UnsupportedVersion(2))
        );
        for bad in [
            "sample_rate=0",
            "sample_rate=-48000",
            "buffer_size=lots",
            "output_channels=70000",
            "output=somewhere",
            "output=device:",
            "input=device:\\q",
            "midi_input=",
            "no equals sign",
        ] {
            let text = format!("manguebyte-devices-setup 1\nsample_rate=48000\n{bad}\n");
            assert_eq!(
                text.parse::<DeviceSetup>(),
                Err(ParseSetupError::MalformedLine(3)),
                "{bad}"
            );
        }
    }

    #[test]
    fn hostile_text_never_panics() {
        let mut text = String::from("manguebyte-devices-setup 1\n");
        for byte in 0u8..=255 {
            text.push(char::from(byte));
            text.push_str("=\\\t\\");
            text.push('\n');
        }
        let _ = text.parse::<DeviceSetup>();
        for cut in 0..usb_setup().to_string().len() {
            let saved = usb_setup().to_string();
            if let Some(prefix) = saved.get(..cut) {
                let _ = prefix.parse::<DeviceSetup>();
            }
        }
    }

    #[test]
    fn a_setup_that_fits_is_planned_exactly() {
        let plan = standard().plan(&usb_setup());
        assert_eq!(plan.fallbacks, []);
        let (output, settings) = plan.output.unwrap();
        assert_eq!(output.id, "out-usb");
        assert_eq!(
            settings,
            StreamSettings::default()
                .with_sample_rate(96_000)
                .with_channels(4)
                .with_buffer_size(128)
        );
        let (input, settings) = plan.input.unwrap();
        assert_eq!(input.id, "in-usb");
        assert_eq!(settings.channels, Some(2));
        assert_eq!(plan.midi_inputs.len(), 1);
    }

    #[test]
    fn a_missing_device_falls_back_to_its_name_then_to_the_default() {
        // The interface came back under another identifier.
        let mut world = standard();
        world.outputs[1].id = "out-usb-2".into();
        world.snapshot.outputs[1].id = "out-usb-2".into();
        let plan = world.plan(&usb_setup());
        assert_eq!(plan.output.as_ref().unwrap().0.id, "out-usb-2");
        assert!(matches!(
            &plan.fallbacks[..],
            [Fallback::DeviceReplaced { by_name: true, .. }]
        ));

        // The interface is gone: the built-in devices take over, at a rate
        // they can do, with what channels they have.
        let world = World::new(
            vec![device(
                "out-builtin",
                "Speakers",
                2,
                (44_100, 48_000),
                48_000,
            )],
            vec![device(
                "in-builtin",
                "Microphone",
                1,
                (48_000, 48_000),
                48_000,
            )],
        );
        let plan = world.plan(&usb_setup());
        assert_eq!(plan.output.as_ref().unwrap().0.id, "out-builtin");
        assert_eq!(plan.input.as_ref().unwrap().0.id, "in-builtin");
        assert_eq!(plan.output.as_ref().unwrap().1.sample_rate, Some(48_000));
        assert_eq!(
            plan.fallbacks,
            [
                Fallback::DeviceReplaced {
                    direction: Direction::Output,
                    wanted: DeviceRef::new("out-usb", "Interface"),
                    used: summary(&world.outputs[0]),
                    by_name: false,
                },
                Fallback::DeviceReplaced {
                    direction: Direction::Input,
                    wanted: DeviceRef::new("in-usb", "Interface"),
                    used: summary(&world.inputs[0]),
                    by_name: false,
                },
                Fallback::SampleRate {
                    wanted: 96_000,
                    used: 48_000
                },
                Fallback::Channels {
                    direction: Direction::Output,
                    wanted: 4,
                    used: 2
                },
                Fallback::Channels {
                    direction: Direction::Input,
                    wanted: 2,
                    used: 1
                },
            ]
        );
        for fallback in &plan.fallbacks {
            assert!(!fallback.to_string().is_empty());
        }
    }

    #[test]
    fn devices_that_cannot_agree_on_a_rate_drop_the_input() {
        let world = World::new(
            vec![device("out", "Out", 2, (44_100, 44_100), 44_100)],
            vec![device("in", "In", 2, (48_000, 48_000), 48_000)],
        );
        let plan = world.plan(&DeviceSetup::default().with_input(DeviceChoice::Default));
        assert!(plan.input.is_none());
        assert_eq!(plan.output.unwrap().1.sample_rate, Some(44_100));
        assert!(matches!(
            &plan.fallbacks[..],
            [Fallback::InputDropped { .. }]
        ));
    }

    #[test]
    fn nothing_connected_turns_both_directions_off() {
        let world = World::new(Vec::new(), Vec::new());
        let plan = world.plan(&usb_setup().with_midi_output(DeviceRef::new("x", "Synth")));
        assert_eq!((plan.output.is_none(), plan.input.is_none()), (true, true));
        assert!(matches!(
            &plan.fallbacks[..],
            [
                Fallback::NoDevice {
                    direction: Direction::Output,
                    ..
                },
                Fallback::NoDevice {
                    direction: Direction::Input,
                    ..
                },
                Fallback::MidiPortMissing {
                    direction: Direction::Output,
                    ..
                },
            ]
        ));
    }

    #[test]
    fn a_buffer_size_out_of_range_is_left_to_the_device() {
        let plan = standard().plan(&usb_setup().with_buffer_size(8));
        assert_eq!(plan.fallbacks, [Fallback::BufferSize { wanted: 8 }]);
        assert_eq!(plan.output.unwrap().1.buffer_size, None);
    }

    #[test]
    fn midi_ports_are_found_by_name_or_left_out() {
        let setup = DeviceSetup::default()
            .with_midi_input(DeviceRef::new("midi-3", "Pads"))
            .with_midi_input(DeviceRef::new("midi-9", "Gone"))
            .with_midi_input(DeviceRef::new("midi-7", "Pads"));
        let plan = standard().plan(&setup);
        // The port found by name and the one found by id are the same, and
        // it is opened once.
        assert_eq!(plan.midi_inputs.len(), 1);
        assert_eq!(plan.midi_inputs[0].id, "midi-7");
        assert!(matches!(
            &plan.fallbacks[..],
            [
                Fallback::MidiPortReplaced { .. },
                Fallback::MidiPortMissing { .. }
            ]
        ));
    }
}
