//! The audio and MIDI settings of a standalone application: what to open,
//! saved as text, what a change opened, and what can be chosen.

use std::fmt;
use std::str::FromStr;

use manguebyte_devices::{
    DeviceChoice, DeviceInfo, DeviceRef, DeviceSetup, DeviceSummary, Direction, Fallback, HostInfo,
    ParseSetupError, SwapReport,
};
use manguebyte_midi::mpe::{Zone, ZoneKind};

const HEADER: &str = "manguebyte-standalone-settings";
const VERSION: u32 = 1;
/// The header of the device setup format the settings' lines follow.
const SETUP_HEADER: &str = "manguebyte-devices-setup 1";

/// The sample rates a settings screen offers, where a device can run at
/// them, in hertz.
pub const COMMON_SAMPLE_RATES: [u32; 8] = [
    22_050, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000,
];

/// The hardware buffer sizes a settings screen offers, where a device can
/// run with them, in frames.
pub const COMMON_BUFFER_SIZES: [u32; 9] = [16, 32, 64, 128, 256, 512, 1_024, 2_048, 4_096];

/// Which MIDI input ports a standalone application listens to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum MidiInputs {
    /// Every port that is connected, and every port connected later.
    #[default]
    All,
    /// These ports, each found by its identifier or, when that changed,
    /// by its name. Empty is none at all ([`MidiInputs::NONE`]).
    Only(Vec<DeviceRef>),
}

impl MidiInputs {
    /// No MIDI input port.
    pub const NONE: Self = Self::Only(Vec::new());

    /// The ports with these identifiers, with no names to find them by
    /// when an identifier changes.
    pub fn ids<I>(ids: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        Self::Only(
            ids.into_iter()
                .map(|id| DeviceRef::new(id, String::new()))
                .collect(),
        )
    }

    /// Whether the port with this identifier is one to listen to.
    pub fn includes(&self, id: &str) -> bool {
        match self {
            Self::All => true,
            Self::Only(ports) => ports.iter().any(|port| port.id == id),
        }
    }
}

/// How the plugin's sample rate follows the device's.
///
/// See [ADR 0021](https://github.com/alessonqueirozdev-hub/manguebyte/blob/main/docs/adr/0021-standalone-device-settings.md)
/// for why both exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RatePolicy {
    /// The plugin runs at the device's rate. A change that opens a device
    /// at another rate prepares the same plugin again at that rate, with a
    /// short faded gap, as a host does.
    #[default]
    FollowDevice,
    /// The plugin always runs at this rate, in hertz, and is prepared
    /// once. The devices are asked for it unless the settings name another
    /// rate; a device at another rate is fed through a resampler at its
    /// edge, which adds under a millisecond of latency, and a change of
    /// device crosses over without a gap where the system can play both.
    Fixed(u32),
}

/// The devices, rate, buffer and MIDI ports a standalone application asks
/// for: what a settings screen changes, and what the application saves
/// with its session and hands back in
/// [`StandaloneOptions::settings`](crate::StandaloneOptions::settings).
///
/// Devices and ports are named by their stable identifiers, with their
/// names to find them by when an identifier changes, as
/// [`DeviceSetup`] does; whatever cannot be had when the settings are
/// applied falls back, and the fallback is reported
/// ([`SettingsReport::fallbacks`]).
///
/// It is written as text, one `key=value` per line under a versioned
/// header, with the keys of [`DeviceSetup`]'s text (the MIDI output port
/// is its `midi_output`) and two more: `midi_inputs`, which is `all` or
/// `listed`, and `midi_output_mpe`, such as `lower 15`, written only when
/// the notes go out as MPE. Reading ignores keys it does not know, so
/// text written before the MIDI output was in the settings reads as no
/// MIDI output, and text written now reads in a program that does not
/// know the MIDI output yet as its other settings; the version is still
/// 1.
///
/// ```
/// use manguebyte_devices::{DeviceChoice, DeviceRef};
/// use manguebyte_standalone::{MidiInputs, StandaloneSettings};
///
/// let settings = StandaloneSettings::default()
///     .with_output(DeviceChoice::Device(DeviceRef::new("wasapi:{0.0.0}", "Speakers")))
///     .with_sample_rate(48_000)
///     .with_buffer_size(256)
///     .with_midi_inputs(MidiInputs::ids(["keys"]));
/// let saved = settings.to_string();
/// assert!(saved.starts_with("manguebyte-standalone-settings 1\n"));
/// assert_eq!(saved.parse::<StandaloneSettings>()?, settings);
/// # Ok::<(), manguebyte_devices::ParseSetupError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StandaloneSettings {
    /// The audio host, by [`HostInfo::name`]. `None` is the system's
    /// default host.
    pub host: Option<String>,
    /// The output device.
    pub output: DeviceChoice,
    /// The input device, for a plugin with inputs; a plugin without any
    /// opens none.
    pub input: DeviceChoice,
    /// Sample rate in hertz. `None` takes the output device's own rate,
    /// or the plugin's under [`RatePolicy::Fixed`].
    pub sample_rate: Option<u32>,
    /// Hardware buffer size in frames. `None` leaves it to the device.
    /// It does not change the largest block the plugin is given
    /// ([`StandaloneOptions::block_size`](crate::StandaloneOptions::block_size)):
    /// a longer buffer is cut into blocks.
    pub buffer_size: Option<u32>,
    /// Output channel count. `None` takes the device's own. Auxiliary
    /// output buses are heard only on channels the device has.
    pub output_channels: Option<u16>,
    /// Input channel count. `None` takes the device's own.
    pub input_channels: Option<u16>,
    /// The MIDI input ports to listen to.
    pub midi_inputs: MidiInputs,
    /// The MIDI output port the plugin's notes go to, for a plugin that
    /// sends notes ([`Plugin::NOTE_OUTPUT`](manguebyte_plugin::Plugin::NOTE_OUTPUT));
    /// `None` sends them nowhere. Found by its identifier or, when that
    /// changed, by its name.
    pub midi_output: Option<DeviceRef>,
    /// For a MIDI output port that speaks MIDI 1.0, as every port does
    /// for now: `Some` writes the plugin's notes as MPE in this zone, so
    /// that each keeps its own tuning, pressure and brightness, and `None`
    /// as plain MIDI 1.0 by the default translation, which keeps
    /// polyphonic pressure alone of them. Kept when the port is `None`.
    pub midi_output_mpe: Option<Zone>,
}

impl Default for StandaloneSettings {
    /// The default devices, at their own rate and buffer size, with every
    /// MIDI input port and no MIDI output.
    fn default() -> Self {
        Self {
            host: None,
            output: DeviceChoice::Default,
            input: DeviceChoice::Default,
            sample_rate: None,
            buffer_size: None,
            output_channels: None,
            input_channels: None,
            midi_inputs: MidiInputs::All,
            midi_output: None,
            midi_output_mpe: None,
        }
    }
}

impl StandaloneSettings {
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

    /// Sets the output channel count.
    #[must_use]
    pub fn with_output_channels(mut self, channels: u16) -> Self {
        self.output_channels = Some(channels);
        self
    }

    /// Sets the MIDI input ports.
    #[must_use]
    pub fn with_midi_inputs(mut self, ports: MidiInputs) -> Self {
        self.midi_inputs = ports;
        self
    }

    /// Sets the MIDI output port.
    #[must_use]
    pub fn with_midi_output(mut self, port: DeviceRef) -> Self {
        self.midi_output = Some(port);
        self
    }

    /// Writes the notes sent to the MIDI output port as MPE in `zone`.
    #[must_use]
    pub fn with_midi_output_mpe(mut self, zone: Zone) -> Self {
        self.midi_output_mpe = Some(zone);
        self
    }

    /// The device setup these settings stand for, for a plugin that takes
    /// input or not and sends notes or not, with `ports` as every MIDI
    /// input port connected.
    pub(crate) fn to_setup(
        &self,
        uses_input: bool,
        sends_notes: bool,
        ports: &[DeviceSummary],
    ) -> DeviceSetup {
        let midi_inputs = match &self.midi_inputs {
            MidiInputs::All => ports.iter().map(DeviceRef::from).collect(),
            MidiInputs::Only(ports) => ports.clone(),
        };
        DeviceSetup {
            host: self.host.clone(),
            output: self.output.clone(),
            input: if uses_input {
                self.input.clone()
            } else {
                DeviceChoice::None
            },
            sample_rate: self.sample_rate,
            buffer_size: self.buffer_size,
            output_channels: self.output_channels,
            input_channels: self.input_channels,
            midi_inputs,
            midi_output: self.midi_output.clone().filter(|_| sends_notes),
        }
    }
}

/// An MPE zone as the settings' text writes it: `lower 15`, `upper 7`.
fn write_zone(zone: Zone) -> String {
    let kind = match zone.kind() {
        ZoneKind::Lower => "lower",
        ZoneKind::Upper => "upper",
    };
    format!("{kind} {}", zone.member_count())
}

fn parse_zone(text: &str) -> Option<Zone> {
    let (kind, members) = text.trim().split_once(' ')?;
    let members = members.trim().parse().ok()?;
    match kind {
        "lower" => Zone::lower(members),
        "upper" => Zone::upper(members),
        _ => None,
    }
}

impl fmt::Display for StandaloneSettings {
    /// Writes the settings as text that [`FromStr`] reads back.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut setup = self.to_setup(true, true, &[]);
        let mode = match &self.midi_inputs {
            MidiInputs::All => "all",
            MidiInputs::Only(_) => "listed",
        };
        if let MidiInputs::All = self.midi_inputs {
            setup.midi_inputs.clear();
        }
        writeln!(f, "{HEADER} {VERSION}")?;
        let text = setup.to_string();
        // The device setup's own lines, under this header instead of its.
        for line in text.lines().skip(1) {
            writeln!(f, "{line}")?;
        }
        writeln!(f, "midi_inputs={mode}")?;
        if let Some(zone) = self.midi_output_mpe {
            writeln!(f, "midi_output_mpe={}", write_zone(zone))?;
        }
        Ok(())
    }
}

impl FromStr for StandaloneSettings {
    type Err = ParseSetupError;

    /// Reads settings written by [`Display`](fmt::Display). Errors are
    /// those of [`DeviceSetup`]'s text, with the line numbers of this one.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut lines = text.lines();
        let version = lines
            .next()
            .and_then(|line| line.trim().strip_prefix(HEADER))
            .and_then(|rest| rest.trim().parse::<u32>().ok())
            .ok_or(ParseSetupError::MissingHeader)?;
        if version != VERSION {
            return Err(ParseSetupError::UnsupportedVersion(version));
        }
        let mut setup_text = String::with_capacity(text.len());
        setup_text.push_str(SETUP_HEADER);
        setup_text.push('\n');
        let mut mode = None;
        let mut mpe = None;
        for (index, line) in lines.enumerate() {
            let trimmed = line.trim_end_matches('\r');
            // The header is line 1, and this line is after it.
            let malformed = ParseSetupError::MalformedLine(index + 2);
            if let Some((key, value)) = trimmed.split_once('=') {
                match key.trim() {
                    "midi_inputs" => {
                        mode = Some(match value.trim() {
                            "all" => true,
                            "listed" => false,
                            _ => return Err(malformed),
                        });
                    }
                    "midi_output_mpe" => mpe = Some(parse_zone(value).ok_or(malformed)?),
                    _ => {}
                }
            }
            // Kept, so that the setup's line numbers are this text's.
            setup_text.push_str(trimmed);
            setup_text.push('\n');
        }
        let setup: DeviceSetup = setup_text.parse()?;
        // Without the key, listed ports are the ones to open, and no
        // ports at all is every port.
        let all = mode.unwrap_or(setup.midi_inputs.is_empty());
        Ok(Self {
            host: setup.host,
            output: setup.output,
            input: setup.input,
            sample_rate: setup.sample_rate,
            buffer_size: setup.buffer_size,
            output_channels: setup.output_channels,
            input_channels: setup.input_channels,
            midi_inputs: if all {
                MidiInputs::All
            } else {
                MidiInputs::Only(setup.midi_inputs)
            },
            midi_output: setup.midi_output,
            midi_output_mpe: mpe,
        })
    }
}

/// What a change of settings opened, as a settings screen shows it.
///
/// Every change made through [`Standalone`](crate::Standalone) returns
/// one, and [`Standalone::report`](crate::Standalone::report) keeps the
/// last. A change that fails is undone: [`error`](Self::error) says why,
/// and the rest of the report describes what runs after the earlier
/// settings were put back.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SettingsReport {
    /// The output device that plays, if any.
    pub output: Option<DeviceSummary>,
    /// The input device that records, if any.
    pub input: Option<DeviceSummary>,
    /// The devices' sample rate, in hertz, when a device runs.
    pub device_sample_rate: Option<u32>,
    /// The hardware buffer size asked of the output device and accepted,
    /// in frames; `None` when the device chooses.
    pub buffer_size: Option<u32>,
    /// The output channels the device runs with, when one runs.
    pub output_channels: Option<usize>,
    /// The sample rate the plugin runs at, in hertz.
    pub plugin_sample_rate: u32,
    /// Whether the plugin was prepared again by this change, because its
    /// sample rate changed.
    pub prepared_again: bool,
    /// The MIDI input ports that are open.
    pub midi_inputs: Vec<DeviceSummary>,
    /// The MIDI output port the plugin's notes go to, if one is open. A
    /// port of the settings that is missing or could not be opened is a
    /// [`Fallback::MidiPortMissing`] in [`fallbacks`](Self::fallbacks).
    pub midi_output: Option<DeviceSummary>,
    /// Everything that could not be had as asked, and what was used
    /// instead.
    pub fallbacks: Vec<Fallback>,
    /// Why the change failed, if it did. The earlier settings were put
    /// back.
    pub error: Option<String>,
    /// How the device change went, when the devices changed: crossfade or
    /// gap, and how long nothing was heard.
    pub swap: Option<SwapReport>,
}

impl SettingsReport {
    /// Whether everything is as asked: nothing fell back and nothing
    /// failed.
    pub fn is_exact(&self) -> bool {
        self.fallbacks.is_empty() && self.error.is_none()
    }

    /// Whether the device runs at another rate than the plugin, through a
    /// resampler.
    pub fn is_resampled(&self) -> bool {
        self.device_sample_rate
            .is_some_and(|rate| rate != self.plugin_sample_rate)
    }
}

impl fmt::Display for SettingsReport {
    /// One line for what plays, then one for each fallback and the error,
    /// in plain words.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.output, self.device_sample_rate) {
            (Some(output), Some(rate)) => {
                write!(f, "Playing on \"{}\" at {rate} Hz", output.name)?;
                if let Some(frames) = self.buffer_size {
                    write!(f, ", {frames}-frame buffer")?;
                }
                if self.is_resampled() {
                    write!(f, ", resampled from {} Hz", self.plugin_sample_rate)?;
                }
            }
            _ => f.write_str("No output device is playing")?,
        }
        if let Some(input) = &self.input {
            write!(f, "; recording from \"{}\"", input.name)?;
        }
        if let Some(port) = &self.midi_output {
            write!(f, "; sending notes to \"{}\"", port.name)?;
        }
        for fallback in &self.fallbacks {
            write!(f, "\n{fallback}")?;
        }
        if let Some(error) = &self.error {
            write!(f, "\nThe change failed and was undone: {error}")?;
        }
        Ok(())
    }
}

/// One device a settings screen can offer, with what it can run at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceOption {
    /// The device.
    pub device: DeviceSummary,
    /// Whether it is the system's default in its direction.
    pub is_default: bool,
    /// The rates of [`COMMON_SAMPLE_RATES`] it can run at, and its own
    /// rate if that is another. Empty when it could not be described.
    pub sample_rates: Vec<u32>,
    /// The sizes of [`COMMON_BUFFER_SIZES`] it accepts. Empty when it
    /// could not be described.
    pub buffer_sizes: Vec<u32>,
    /// The most channels it has; zero when it could not be described.
    pub max_channels: u16,
}

impl DeviceOption {
    /// What a settings screen offers of `device`, described by `info` when
    /// it could be.
    pub(crate) fn new(device: DeviceSummary, is_default: bool, info: Option<&DeviceInfo>) -> Self {
        let Some(info) = info else {
            return Self {
                device,
                is_default,
                sample_rates: Vec::new(),
                buffer_sizes: Vec::new(),
                max_channels: 0,
            };
        };
        // As the device manager plans: a device whose configurations are
        // unknown runs at its own rate only, and a buffer size fits unless
        // a configuration's range leaves it out.
        let runs_at = |rate: u32| {
            if info.configs.is_empty() {
                info.default_sample_rate == Some(rate)
            } else {
                info.supports_sample_rate(rate)
            }
        };
        let mut sample_rates: Vec<u32> = COMMON_SAMPLE_RATES
            .iter()
            .copied()
            .filter(|&rate| runs_at(rate))
            .collect();
        if let Some(own) = info.default_sample_rate {
            if !sample_rates.contains(&own) {
                sample_rates.push(own);
                sample_rates.sort_unstable();
            }
        }
        let buffer_sizes = COMMON_BUFFER_SIZES
            .iter()
            .copied()
            .filter(|&frames| {
                info.configs.iter().all(|config| {
                    config
                        .buffer_size
                        .is_none_or(|(min, max)| (min..=max).contains(&frames))
                })
            })
            .collect();
        Self {
            device,
            is_default,
            sample_rates,
            buffer_sizes,
            max_channels: info.max_channels(),
        }
    }
}

/// One MIDI port a settings screen can offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiPortOption {
    /// The port.
    pub port: DeviceSummary,
    /// Whether it is open.
    pub open: bool,
}

/// Everything a settings screen can offer now: hosts, devices with the
/// rates and buffer sizes each accepts, and MIDI ports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsChoices {
    /// The audio hosts that can be chosen.
    pub hosts: Vec<HostInfo>,
    /// The host in use.
    pub host: String,
    /// The output devices.
    pub outputs: Vec<DeviceOption>,
    /// The input devices; empty for a plugin with no inputs.
    pub inputs: Vec<DeviceOption>,
    /// The MIDI input ports.
    pub midi_inputs: Vec<MidiPortOption>,
    /// The MIDI output ports, the one the plugin's notes go to marked
    /// open; empty for a plugin that sends no notes.
    pub midi_outputs: Vec<MidiPortOption>,
}

impl SettingsChoices {
    /// The device with this identifier, in one direction.
    pub fn device(&self, direction: Direction, id: &str) -> Option<&DeviceOption> {
        let devices = match direction {
            Direction::Output => &self.outputs,
            Direction::Input => &self.inputs,
        };
        devices.iter().find(|option| option.device.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> StandaloneSettings {
        StandaloneSettings::default()
            .with_host("ASIO")
            .with_output(DeviceChoice::Device(DeviceRef::new("out\tid", "Out = 1")))
            .with_input(DeviceChoice::None)
            .with_sample_rate(96_000)
            .with_buffer_size(64)
            .with_output_channels(8)
            .with_midi_inputs(MidiInputs::Only(vec![
                DeviceRef::new("keys", "Keys"),
                DeviceRef::new("pads", ""),
            ]))
    }

    #[test]
    fn settings_round_trip_through_text() {
        let with_output = full().with_midi_output(DeviceRef::new("synth\t2", "Synth = A"));
        let mut mpe_without_port = full();
        mpe_without_port.midi_output_mpe = Zone::upper(7);
        for settings in [
            StandaloneSettings::default(),
            full(),
            full().with_midi_inputs(MidiInputs::NONE),
            full().with_midi_inputs(MidiInputs::All),
            with_output.clone(),
            with_output.with_midi_output_mpe(Zone::lower(15).unwrap()),
            mpe_without_port,
        ] {
            let text = settings.to_string();
            assert_eq!(text.parse::<StandaloneSettings>(), Ok(settings), "{text}");
        }
        // A device setup is not standalone settings, and the other way
        // round.
        let setup = DeviceSetup::default().to_string();
        assert_eq!(
            setup.parse::<StandaloneSettings>(),
            Err(ParseSetupError::MissingHeader)
        );
        assert!(full().to_string().parse::<DeviceSetup>().is_err());
    }

    #[test]
    fn reading_reports_the_line_at_fault_and_skips_unknown_keys() {
        let text = "manguebyte-standalone-settings 1\nfuture=1\nsample_rate=44100\n";
        let settings: StandaloneSettings = text.parse().unwrap();
        assert_eq!(settings.sample_rate, Some(44_100));
        assert_eq!(settings.midi_inputs, MidiInputs::All);
        assert_eq!(
            "manguebyte-standalone-settings 2\n".parse::<StandaloneSettings>(),
            Err(ParseSetupError::UnsupportedVersion(2))
        );
        for (bad, line) in [
            ("output=nowhere", 3),
            ("midi_inputs=some", 3),
            ("buffer_size=0", 3),
        ] {
            let text = format!("manguebyte-standalone-settings 1\nsample_rate=48000\n{bad}\n");
            assert_eq!(
                text.parse::<StandaloneSettings>(),
                Err(ParseSetupError::MalformedLine(line)),
                "{bad}"
            );
        }
        // Without the key, listed ports are the ones to open.
        let text = "manguebyte-standalone-settings 1\nmidi_input=keys\tKeys\n";
        assert_eq!(
            text.parse::<StandaloneSettings>().unwrap().midi_inputs,
            MidiInputs::Only(vec![DeviceRef::new("keys", "Keys")])
        );
    }

    #[test]
    fn settings_written_before_the_midi_output_still_read() {
        // As the settings of commit 3369941 wrote them, before there was
        // a MIDI output in them.
        let old = "manguebyte-standalone-settings 1\n\
                   host=ASIO\n\
                   output=device:speakers\tSpeakers\n\
                   input=none\n\
                   sample_rate=96000\n\
                   buffer_size=64\n\
                   output_channels=8\n\
                   midi_input=keys\tKeys\n\
                   midi_inputs=listed\n";
        let settings: StandaloneSettings = old.parse().unwrap();
        let expected = StandaloneSettings::default()
            .with_host("ASIO")
            .with_output(DeviceChoice::Device(DeviceRef::new("speakers", "Speakers")))
            .with_input(DeviceChoice::None)
            .with_sample_rate(96_000)
            .with_buffer_size(64)
            .with_output_channels(8)
            .with_midi_inputs(MidiInputs::Only(vec![DeviceRef::new("keys", "Keys")]));
        assert_eq!(settings, expected);
        assert_eq!(settings.midi_output, None);
        assert_eq!(settings.midi_output_mpe, None);
        // Without a MIDI output they are written as they were.
        assert_eq!(settings.to_string(), old);

        // The new keys, in the order they are written, and what is wrong
        // in them.
        let text = format!("{old}midi_output=synth\tSynth\nmidi_output_mpe=upper 7\n");
        let settings: StandaloneSettings = text.parse().unwrap();
        assert_eq!(settings.midi_output, Some(DeviceRef::new("synth", "Synth")));
        assert_eq!(settings.midi_output_mpe, Zone::upper(7));
        for bad in ["middle 3", "lower 0", "upper 16", "lower"] {
            let text = format!("{old}midi_output_mpe={bad}\n");
            assert_eq!(
                text.parse::<StandaloneSettings>(),
                Err(ParseSetupError::MalformedLine(10)),
                "{bad}"
            );
        }
        // A device setup's own reader takes the port, and skips the rest.
        let written = settings.to_string();
        let setup: DeviceSetup = written
            .replacen(
                "manguebyte-standalone-settings 1",
                "manguebyte-devices-setup 1",
                1,
            )
            .parse()
            .unwrap();
        assert_eq!(setup.midi_output, Some(DeviceRef::new("synth", "Synth")));
    }

    #[test]
    fn a_device_offers_the_rates_and_buffers_it_accepts() {
        let mut info = manguebyte_devices::MockBackend::device("d", "D", 2, 47_000);
        for config in &mut info.configs {
            config.min_sample_rate = 44_100;
            config.max_sample_rate = 96_000;
            config.buffer_size = Some((64, 1_024));
        }
        let summary = DeviceSummary {
            id: "d".into(),
            name: "D".into(),
        };
        let option = DeviceOption::new(summary.clone(), true, Some(&info));
        assert_eq!(
            option.sample_rates,
            [44_100, 47_000, 48_000, 88_200, 96_000]
        );
        assert_eq!(option.buffer_sizes, [64, 128, 256, 512, 1_024]);
        assert_eq!(option.max_channels, 2);
        let unknown = DeviceOption::new(summary, false, None);
        assert!(unknown.sample_rates.is_empty() && unknown.buffer_sizes.is_empty());
    }
}
