//! Audio device input and output.
//!
//! Finds the system's audio devices, opens them with the sample rate, channel
//! count and buffer size you ask for, and drives callbacks that work on planar
//! [`AudioBuffer`](manguebyte_core::AudioBuffer)s whatever sample format and
//! layout the hardware actually uses. The platform work is done by [`cpal`].
//!
//! - [`audio_hosts`] lists the system's audio APIs (WASAPI, ASIO, CoreAudio,
//!   ALSA, JACK, AAudio) and [`AudioHost`] picks one.
//! - [`output_devices`] and [`input_devices`] list what is connected.
//! - [`OutputDevice`] plays audio.
//! - [`InputDevice`] records it.
//! - [`DuplexStream`] does both at once, for effects and monitoring, and
//!   follows the drift between two devices' clocks.
//! - [`DeviceManager`] opens the devices a saved [`DeviceSetup`] names, with
//!   fallbacks for what is missing, runs one [`AudioCallback`] on them, and
//!   follows the system as devices come and go ([`DeviceEvent`],
//!   [`DeviceWatcher`]).
//! - [`DeviceManager::start_engine`] runs the callback at a fixed
//!   [`EngineFormat`], prepared once, and changes devices under it in the
//!   middle of a song without stopping it: a crossfade where two devices
//!   can play at once, a short faded gap where they cannot, a resampler
//!   for a device at another rate ([`hotswap`]).
//! - [`MockBackend`] stands in for the system with no hardware, for tests.
//! - [`midi`] opens MIDI ports, real and virtual, keeps a player's timing
//!   on the way to the audio thread, and sends packets from it on time,
//!   unchanged to ports that speak MIDI 2.0 on macOS, iOS and Linux, as
//!   MIDI 1.0 bytes to the others (feature `midi`, on by default).
//! - [`AudioBackend`] lets a crate of its own put another driver model
//!   behind a [`DeviceManager`] ([`DeviceManager::with_audio_backend`]).
//!
//! Every callback can be told when its audio is heard or was captured: the
//! `start_timed` methods and the device manager hand it a [`CallbackInfo`]
//! with the frame position, the backend's clock and the latencies it
//! reports.
//!
//! # Features
//!
//! - `midi` (default): MIDI ports, through `midir`, and the systems' own
//!   Universal MIDI Packet output, through `manguebyte-ump-io`.
//! - `jack`: the JACK audio host. On Linux, building it needs the JACK
//!   development files (`libjack-jackd2-dev` on Debian and Ubuntu).
//!
//! ASIO has no feature here, because the backend's build script fetches
//! Steinberg's SDK by itself; [`audio_hosts`] says how an application turns
//! it on, and names the other way, the GPLv3 crate `manguebyte-pro-io`.
//!
//! ASIO is a trademark of Steinberg Media Technologies GmbH, registered in Europe and other countries.
//!
//! # Platforms
//!
//! Windows, macOS and Linux are built and tested in CI, Android
//! (`aarch64-linux-android`) and iOS (`aarch64-apple-ios`) are built there.
//! Virtual MIDI ports exist where the system allows them; see
//! [`midi`](midi#virtual-ports).
//!
//! ```no_run
//! use manguebyte_devices::OutputDevice;
//!
//! let device = OutputDevice::open_default()?;
//! let step = 440.0 * std::f32::consts::TAU / device.sample_rate() as f32;
//! let mut phase = 0.0f32;
//!
//! let _stream = device.start(512, move |buffer| {
//!     for frame in 0..buffer.num_frames() {
//!         let sample = 0.1 * phase.sin();
//!         phase = (phase + step) % std::f32::consts::TAU;
//!         for channel in 0..buffer.num_channels() {
//!             buffer.channel_mut(channel)[frame] = sample;
//!         }
//!     }
//! })?;
//!
//! std::thread::sleep(std::time::Duration::from_secs(1));
//! # Ok::<(), manguebyte_devices::DeviceError>(())
//! ```
//!
//! # Callbacks run on the audio thread
//!
//! Every callback handed to this crate is called by the operating system's
//! audio thread and has a few milliseconds to finish, every time. It must not
//! allocate, lock, block or do I/O. Share values with it through atomics and
//! move data in and out through an [`spsc`](manguebyte_core::spsc) queue.

#![forbid(unsafe_code)]

mod backend;
mod blocks;
mod device;
mod drift;
mod duplex;
mod error;
mod external;
mod host;
mod hotplug;
pub mod hotswap;
mod manager;
#[cfg(feature = "midi")]
pub mod midi;
mod mock;
mod settings;
mod setup;
mod timing;

pub use device::{
    DeviceInfo, Direction, InputDevice, InputStream, OutputDevice, OutputStream, input_devices,
    output_devices,
};
pub use duplex::{DuplexSettings, DuplexStream};
pub use error::DeviceError;
pub use external::{
    AudioBackend, BackendCallback, BackendEndpoint, BackendFormat, BackendRequest, BackendStream,
    StreamStatus,
};
pub use host::{AudioHost, HostInfo, audio_hosts};
pub use hotplug::{
    DEFAULT_POLL_INTERVAL, DeviceEvent, DeviceSnapshot, DeviceSummary, DeviceWatcher, LossCause,
    MIN_POLL_INTERVAL, STALL_TIMEOUT,
};
pub use hotswap::{DEFAULT_CROSSFADE, EngineFormat, SwapKind, SwapReport};
pub use manager::{
    AudioCallback, DEFAULT_MAX_BLOCK_SIZE, DeviceIo, DeviceManager, SetupPreview, StreamFormat,
};
pub use mock::MockBackend;
pub use settings::{ConfigRange, StreamSettings};
pub use setup::{DeviceChoice, DeviceRef, DeviceSetup, Fallback, ParseSetupError, RestoreReport};
pub use timing::CallbackInfo;
