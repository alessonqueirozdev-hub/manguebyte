//! Audio hosts: the system audio APIs a device can be reached through.

use crate::device::{Direction, list, summaries};
use crate::hotplug::DeviceSummary;
use crate::{DeviceError, DeviceInfo, InputDevice, OutputDevice};

/// The name of the host that plays WASAPI's output devices exclusively, as
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
/// output devices, each played by this program alone, without the Windows
/// mixer and its period, down to the device's shortest period (often
/// 3 ms). Nothing else plays on a device while it is held so, and it has
/// no inputs. Some drivers refuse it; Bluetooth ones do.
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
    /// WASAPI's devices, played exclusively.
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

    /// Whether this host plays WASAPI's devices exclusively.
    pub fn is_exclusive(&self) -> bool {
        self.exclusive
    }

    /// Lists the host's output devices.
    pub fn output_devices(&self) -> Result<Vec<DeviceInfo>, DeviceError> {
        list(&self.backend()?, Direction::Output)
    }

    /// Lists the host's input devices. The exclusive host has none.
    pub fn input_devices(&self) -> Result<Vec<DeviceInfo>, DeviceError> {
        if self.exclusive {
            return Ok(Vec::new());
        }
        list(&self.backend()?, Direction::Input)
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
        if self.exclusive && direction == Direction::Input {
            return Ok((Vec::new(), None));
        }
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
        assert_ne!(exclusive, AudioHost::by_name("WASAPI").unwrap());
        assert!(exclusive.input_devices().unwrap().is_empty());
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
