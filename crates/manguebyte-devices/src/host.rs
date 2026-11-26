//! Audio hosts: the system audio APIs a device can be reached through.

use std::fmt;
use std::time::Duration;

use crate::device::{Direction, list, summaries};
use crate::hotplug::DeviceSummary;
use crate::{DeviceError, DeviceInfo, InputDevice, OutputDevice};

/// The name of the host that holds WASAPI's devices exclusively, as
/// [`audio_hosts`] lists it on Windows and [`AudioHost::by_name`] takes it.
pub const WASAPI_EXCLUSIVE: &str = "WASAPI exclusive";

/// An audio host that can be used on this system, as listed by
/// [`audio_hosts`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HostInfo {
    /// The host's name, such as `"WASAPI"`, `"ASIO"`, `"CoreAudio"`,
    /// `"ALSA"`, `"JACK"` or `"AAudio"`. [`AudioHost::by_name`] takes it
    /// back, in any case.
    ///
    /// ASIO is a trademark of Steinberg Media Technologies GmbH, registered in Europe and other countries.
    pub name: String,
    /// Whether this is the host that [`AudioHost::default_host`] picks.
    pub is_default: bool,
}

/// Whether the [`WASAPI_EXCLUSIVE`] host can hold one device, as listed by
/// [`AudioHost::exclusive_support`].
///
/// It is what the device says without being opened, so it cannot tell that
/// another program holds the device, and a driver may still refuse on
/// opening what it said it takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExclusiveSupport {
    /// The device's identifier, as in [`DeviceInfo::id`].
    pub id: String,
    /// The device's name.
    pub name: String,
    /// What it takes exclusively, or why it takes nothing.
    pub outcome: Result<ExclusiveModes, ExclusiveRefusal>,
}

/// What a device takes in exclusive mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExclusiveModes {
    /// The channel counts it takes, of those asked about: the device's own,
    /// those the shared host lists, two and one. Never empty.
    pub channels: Vec<ExclusiveChannels>,
    /// The shortest period it runs on.
    pub shortest_period: Duration,
    /// The period it runs on when not told.
    pub default_period: Duration,
}

/// A channel count a device takes exclusively, and the rates it takes it at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExclusiveChannels {
    /// The number of channels.
    pub channels: u16,
    /// The rates accepted, in hertz, of 44.1, 48, 88.2, 96, 176.4 and
    /// 192 kHz.
    pub sample_rates: Vec<u32>,
}

/// Why a device cannot be held exclusively.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExclusiveRefusal {
    /// The driver took none of the formats asked about, at any rate or
    /// channel count. Drivers without exclusive mode, Windows' Bluetooth
    /// audio among them, answer so or with an error of their own.
    NoFormat,
    /// "Allow applications to take exclusive control" is off in the
    /// device's Windows properties.
    NotAllowed,
    /// Another program holds the device.
    InUse,
    /// Windows denies this program access, as its microphone privacy
    /// setting does for inputs.
    AccessDenied,
    /// The device went away or is no longer active.
    Gone,
    /// Anything else the system said.
    Other(String),
}

impl fmt::Display for ExclusiveRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoFormat => f.write_str("the driver takes no format exclusively"),
            Self::NotAllowed => f.write_str(
                "the device does not allow exclusive control (a setting in its Windows properties)",
            ),
            Self::InUse => f.write_str("another program holds the device"),
            Self::AccessDenied => f.write_str(
                "Windows denies access to the device (for an input, see the microphone privacy settings)",
            ),
            Self::Gone => f.write_str("the device went away"),
            Self::Other(why) => f.write_str(why),
        }
    }
}

/// Lists the audio hosts that can be used now.
///
/// Which hosts exist depends on the system and on how the crate was built:
///
/// | System | Hosts |
/// |---|---|
/// | Windows | WASAPI (default); ASIO when built with it (see below); JACK |
/// | macOS | CoreAudio (default); JACK |
/// | iOS | CoreAudio |
/// | Linux, BSD | ALSA (default); JACK |
/// | Android | AAudio |
///
/// JACK needs this crate's `jack` feature, which on Linux needs the JACK
/// development files to build (`libjack-jackd2-dev` on Debian and Ubuntu).
///
/// A host is listed only when it is usable: JACK only while its server
/// runs, ASIO only when a driver is installed.
///
/// # ASIO
///
/// ASIO needs Steinberg's ASIO SDK, which is under Steinberg's own licence
/// and which this crate neither ships nor fetches. The backend that speaks
/// ASIO is `cpal`'s `asio` feature. Its build script downloads the SDK by
/// itself when the `CPAL_ASIO_DIR` environment variable does not point at
/// a copy, which would accept Steinberg's licence on your behalf, so this
/// crate does not offer a feature that turns it on: a workspace build with
/// `--all-features` would do it without anyone asking. To use ASIO,
/// download the SDK from Steinberg yourself, accept its licence, set
/// `CPAL_ASIO_DIR` to where you unpacked it, install LLVM (the bindings are
/// generated with `bindgen`), and depend on `cpal` with the feature in your
/// application:
///
/// ```toml
/// [target.'cfg(windows)'.dependencies]
/// cpal = { version = "0.18", features = ["asio"] }
/// ```
///
/// Cargo builds `cpal` once for the whole program, so this crate then sees
/// the ASIO host too, and everything here works with it. This path keeps
/// the program under licences of its own choosing, closed source included,
/// as long as it keeps to Steinberg's licence for the SDK.
///
/// The other path is the workspace's `manguebyte-pro-io` crate, which
/// speaks ASIO itself under the SDK's GPLv3 option and plugs into a
/// [`DeviceManager`](crate::DeviceManager) through
/// [`AudioBackend`](crate::AudioBackend). It needs no SDK download, LLVM
/// or `bindgen`, but a program that uses it is under the GPLv3 as a whole.
///
/// ASIO is a trademark of Steinberg Media Technologies GmbH, registered in Europe and other countries.
///
/// # WASAPI exclusive
///
/// On Windows, `"WASAPI exclusive"` follows WASAPI in the list: the same
/// devices, each held by this program alone, without the Windows mixer and
/// its period, down to the device's shortest period (often 3 ms). Nothing
/// else uses a device while it is held so. Some drivers refuse it;
/// Bluetooth ones do, and [`AudioHost::exclusive_support`] says which
/// devices can be held and why the others cannot.
///
/// An output, an input or both can run. Both run as two streams joined as a
/// [`DuplexStream`](crate::DuplexStream)'s are, following the drift between
/// the two clocks. Neither side reports a latency to the callback.
pub fn audio_hosts() -> Vec<HostInfo> {
    let default = cpal::default_host().id();
    let mut hosts = Vec::new();
    for id in cpal::available_hosts() {
        hosts.push(HostInfo {
            name: id.name().to_owned(),
            is_default: id == default,
        });
        #[cfg(windows)]
        if id == cpal::HostId::Wasapi {
            hosts.push(HostInfo {
                name: WASAPI_EXCLUSIVE.to_owned(),
                is_default: false,
            });
        }
    }
    hosts
}

/// One audio host, chosen by name or the system's default.
///
/// ```no_run
/// use manguebyte_devices::{AudioHost, audio_hosts};
///
/// for host in audio_hosts() {
///     println!("{}{}", host.name, if host.is_default { " (default)" } else { "" });
/// }
/// let host = AudioHost::by_name("WASAPI").unwrap_or_else(|_| AudioHost::default_host());
/// for device in host.output_devices()? {
///     println!("  {}", device.name);
/// }
/// let output = host.default_output_device()?;
/// # Ok::<(), manguebyte_devices::DeviceError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AudioHost {
    id: cpal::HostId,
    /// WASAPI's devices, held exclusively.
    exclusive: bool,
}

impl AudioHost {
    /// The system's default host.
    pub fn default_host() -> Self {
        Self {
            id: cpal::default_host().id(),
            exclusive: false,
        }
    }

    /// The host with this name, as found in [`HostInfo::name`]. Case does
    /// not matter.
    ///
    /// Fails with [`DeviceError::HostUnavailable`] when the host was not
    /// built in or cannot be used now, for example JACK without a running
    /// server.
    pub fn by_name(name: &str) -> Result<Self, DeviceError> {
        let unavailable = || DeviceError::HostUnavailable(name.to_owned());
        #[cfg(windows)]
        if name.trim().eq_ignore_ascii_case(WASAPI_EXCLUSIVE) {
            return Ok(Self {
                id: cpal::HostId::Wasapi,
                exclusive: true,
            });
        }
        let id: cpal::HostId = name.trim().parse().map_err(|_| unavailable())?;
        if !cpal::available_hosts().contains(&id) {
            return Err(unavailable());
        }
        Ok(Self {
            id,
            exclusive: false,
        })
    }

    /// The host's name, such as `"WASAPI"`.
    pub fn name(&self) -> &'static str {
        if self.exclusive {
            WASAPI_EXCLUSIVE
        } else {
            self.id.name()
        }
    }

    /// Whether this is the system's default host.
    pub fn is_default(&self) -> bool {
        !self.exclusive && cpal::default_host().id() == self.id
    }

    /// Whether this host holds WASAPI's devices exclusively.
    pub fn is_exclusive(&self) -> bool {
        self.exclusive
    }

    /// Lists the host's output devices.
    pub fn output_devices(&self) -> Result<Vec<DeviceInfo>, DeviceError> {
        list(&self.backend()?, Direction::Output)
    }

    /// Lists the host's input devices.
    pub fn input_devices(&self) -> Result<Vec<DeviceInfo>, DeviceError> {
        list(&self.backend()?, Direction::Input)
    }

    /// Asks each device of the [`WASAPI_EXCLUSIVE`] host in `direction`
    /// whether it can be held exclusively, and why not when it cannot. It
    /// opens no stream.
    ///
    /// Fails with [`DeviceError::UnsupportedSettings`] on any other host.
    ///
    /// ```no_run
    /// use manguebyte_devices::{AudioHost, Direction, WASAPI_EXCLUSIVE};
    ///
    /// let host = AudioHost::by_name(WASAPI_EXCLUSIVE)?;
    /// for device in host.exclusive_support(Direction::Input)? {
    ///     match device.outcome {
    ///         Ok(modes) => println!("{}: {:?}", device.name, modes),
    ///         Err(why) => println!("{}: {why}", device.name),
    ///     }
    /// }
    /// # Ok::<(), manguebyte_devices::DeviceError>(())
    /// ```
    pub fn exclusive_support(
        &self,
        direction: Direction,
    ) -> Result<Vec<ExclusiveSupport>, DeviceError> {
        #[cfg(windows)]
        if self.exclusive {
            return Ok(list(&self.backend()?, direction)?
                .iter()
                .map(|info| crate::exclusive::support(&crate::exclusive::System, info))
                .collect());
        }
        let _ = direction;
        Err(DeviceError::UnsupportedSettings(format!(
            "only the {WASAPI_EXCLUSIVE} host holds devices exclusively"
        )))
    }

    /// Opens the host's default output device with its preferred
    /// configuration.
    pub fn default_output_device(&self) -> Result<OutputDevice, DeviceError> {
        OutputDevice::open_default_of(&self.backend()?)
    }

    /// Opens the host's default input device with its preferred
    /// configuration.
    pub fn default_input_device(&self) -> Result<InputDevice, DeviceError> {
        InputDevice::open_default_of(&self.backend()?)
    }

    /// The identifier and name of every device in `direction`, and which
    /// one is the default, without asking each device what it supports.
    /// Cheap enough to call every second.
    pub(crate) fn summaries(
        &self,
        direction: Direction,
    ) -> Result<(Vec<DeviceSummary>, Option<String>), DeviceError> {
        summaries(&self.backend()?, direction)
    }

    pub(crate) fn backend(&self) -> Result<cpal::Host, DeviceError> {
        cpal::host_from_id(self.id).map_err(|_| DeviceError::HostUnavailable(self.name().into()))
    }
}

impl Default for AudioHost {
    fn default() -> Self {
        Self::default_host()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_host_is_listed_and_found_by_name() {
        let hosts = audio_hosts();
        let default = AudioHost::default_host();
        assert_eq!(hosts.iter().filter(|host| host.is_default).count(), 1);
        let listed = hosts.iter().find(|host| host.is_default).unwrap();
        assert_eq!(listed.name, default.name());
        assert!(default.is_default());

        let found = AudioHost::by_name(&listed.name.to_lowercase()).unwrap();
        assert_eq!(found, default);
    }

    #[cfg(windows)]
    #[test]
    fn the_exclusive_host_follows_wasapi_and_is_found_by_name() {
        let hosts = audio_hosts();
        let names: Vec<&str> = hosts.iter().map(|host| host.name.as_str()).collect();
        let wasapi = names.iter().position(|&name| name == "WASAPI").unwrap();
        assert_eq!(names[wasapi + 1], "WASAPI exclusive");
        assert!(!hosts[wasapi + 1].is_default);

        let exclusive = AudioHost::by_name(" wasapi EXCLUSIVE ").unwrap();
        assert!(exclusive.is_exclusive() && !exclusive.is_default());
        assert_eq!(exclusive.name(), "WASAPI exclusive");
        let shared = AudioHost::by_name("WASAPI").unwrap();
        assert_ne!(exclusive, shared);

        // The same devices by the same identifiers, inputs among them, and
        // an answer or a reason for each. A machine without audio devices
        // lists none.
        for direction in [Direction::Input, Direction::Output] {
            let ids = |host: AudioHost| match direction {
                Direction::Input => host.input_devices(),
                Direction::Output => host.output_devices(),
            };
            let ids =
                |host| -> Vec<String> { ids(host).unwrap().into_iter().map(|d| d.id).collect() };
            let held = ids(exclusive);
            assert_eq!(held, ids(shared));
            let support = exclusive.exclusive_support(direction).unwrap();
            let asked: Vec<String> = support.iter().map(|d| d.id.clone()).collect();
            assert_eq!(asked, held);
            for device in support {
                eprintln!("{}: {:?}", direction.as_str(), device.outcome);
            }
            assert!(shared.exclusive_support(direction).is_err());
        }
    }

    #[test]
    fn only_the_exclusive_host_says_what_it_can_hold() {
        let host = AudioHost::default_host();
        for direction in [Direction::Input, Direction::Output] {
            assert!(matches!(
                host.exclusive_support(direction),
                Err(DeviceError::UnsupportedSettings(_))
            ));
        }
        assert_eq!(
            ExclusiveRefusal::Other(String::from("why")).to_string(),
            "why"
        );
    }

    #[test]
    fn an_unknown_host_is_unavailable() {
        for name in ["", "no such host", "ASIO4ALL\n"] {
            assert!(matches!(
                AudioHost::by_name(name),
                Err(DeviceError::HostUnavailable(_))
            ));
        }
    }
}
