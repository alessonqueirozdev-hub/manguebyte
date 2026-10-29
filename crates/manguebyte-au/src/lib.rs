//! Builds Audio Units (version 2) from a
//! [`Plugin`](manguebyte_plugin::Plugin), for macOS.
//!
//! An Audio Unit is a component bundle, `Name.component`, whose
//! `Info.plist` names the unit's codes and the factory function its library
//! exports. Hosts on macOS (Logic Pro, GarageBand, Ableton Live, Reaper and
//! others) find it in `~/Library/Audio/Plug-Ins/Components`.
//!
//! # Making a plugin
//!
//! In a crate of type `cdylib` (and `rlib`, for the bundle step),
//! implement `Plugin` and export it:
//!
//! ```ignore
//! manguebyte_au::export_au!(MyPlugin);
//! ```
//!
//! On macOS this defines `ManguebyteAudioUnitFactory`, the factory function
//! the `Info.plist` names; elsewhere it defines nothing a host could load.
//! `cargo mangue bundle --format au` builds the bundle on a Mac, with the
//! `Info.plist` [`info_plist`] writes, and `cargo mangue install --format
//! au` copies it where hosts look. The same library can export CLAP, VST 3
//! and LV2 too: the formats use different symbols.
//!
//! # What the wrapper provides
//!
//! | Audio Unit | Behaviour |
//! |---|---|
//! | Type | `aumu` for an instrument, `aufx` for an effect or analyzer, `aumf` for a note effect ([`component_type`]) |
//! | Codes | Subtype and manufacturer worked out from the plugin's id ([`subtype_code`], [`manufacturer_code`]), the same on every machine |
//! | Elements | Input 0 is the main input and input 1 the sidechain, each if the plugin has one; output 0. `SupportedNumChannels` lists the main input and output of each layout the plugin offers ([`Plugin::LAYOUTS`](manguebyte_plugin::Plugin::LAYOUTS)), and each element takes any width it has in one of them; `Initialize` refuses widths that add up to no offered layout, and gives the plugin the one they make |
//! | Stream format | 32-bit float, one buffer per channel, at any sample rate; changed only while uninitialised |
//! | Input | Pulled from the render callback or the connection the host set; the main input must have one, the sidechain is silent without |
//! | Output | Into the host's buffers, or the unit's own when the host gives none |
//! | Parameters | Listed in their own unit and range, with their names and value text both ways; stepped ones with names are indexed, with their names listed; switches are booleans. `SetParameter` reaches the plugin on the first frame of the next render, and `ScheduleParameters` on its frame, a ramp as points 32 frames apart |
//! | MIDI | For instruments and note effects, `MusicDeviceMIDIEvent`, and `MusicDeviceStartNote` and `StopNote` as MIDI notes, on their frames |
//! | MIDI out | For a plugin that sends notes ([`Plugin::NOTE_OUTPUT`](manguebyte_plugin::Plugin::NOTE_OUTPUT)), one MIDI output named in `MIDIOutputCallbackInfo`; after each render the host's `MIDIOutputCallback` is handed what the plugin sent, as MIDI 1.0 by the default translation, in a `MIDIPacketList` with one message to a packet, each stamped with its frame in the render |
//! | Transport | From the host's callbacks: tempo and beat, the bar's start and the time signature, playing, recording, the position in frames and the cycle |
//! | State | `ClassInfo`: a dictionary with the unit's codes, the present preset's name and the text of [`save_state`](manguebyte_plugin::save_state); another unit's state is refused and changes nothing |
//! | Presets | `PresentPreset` (and the deprecated `CurrentPreset`): the number and name the host sets, -1 and "Untitled" until then; restoring a state makes its name the present preset's. No factory presets |
//! | Latency | Reported from the plugin when initialised and on every render; tail time 0 |
//! | Other properties | Maximum frames per slice (changeable while initialised, which prepares the plugin again), element count, element names, supported channel layouts and the layout itself, last render error, host callbacks, bypass (for effects), in-place processing and buffer allocation (recorded) |
//! | Listeners | Property listeners are called for what changes; render notifications before and after each render |
//!
//! Nothing that `Render`, `ScheduleParameters` or `MusicDeviceMIDIEvent`
//! does allocates, waits or blocks: everything is allocated by
//! `Initialize`, and the audio thread takes none of the system's locks: it
//! only tries the ownership flags the other calls take (see the module
//! documentation of the wrapper, in the
//! source). A panic inside the plugin is caught at the boundary, and the
//! slice is silent.
//!
//! Not there yet: an editor (`kAudioUnitProperty_CocoaUI`), MIDI 2.0
//! output (`kAudioUnitProperty_MIDIOutputEventListCallback`), factory
//! presets, switching a sidechain off, and telling the host's parameter
//! listeners about a change the plugin made itself.
//!
//! # How it is checked
//!
//! The wrapper is plain Rust around a C interface, and is built on every
//! system: on macOS against CoreFoundation and AudioToolbox, and elsewhere
//! against a small stand-in for the few CoreFoundation calls it makes, so
//! that `tests/abi.rs` can act as a host through the factory and the
//! methods it looks up, on any machine. On macOS, continuous integration
//! also builds both example plugins as components, installs them and runs
//! Apple's `auval` on each.

mod cf;
mod codes;
mod exclusive;
#[cfg(not(target_os = "macos"))]
#[doc(hidden)]
#[allow(missing_docs)]
pub mod fake_cf;
mod plist;
#[doc(hidden)]
#[allow(missing_docs)]
pub mod sys;
mod wrapper;

pub use codes::{
    FourCharCode, as_os_type, code_text, codes, component_type, manufacturer_code, subtype_code,
    version_number,
};
pub use plist::{FACTORY_FUNCTION, display_name, info_plist};
pub use wrapper::{DEFAULT_MAX_FRAMES, DEFAULT_SAMPLE_RATE, EVENTS_PER_BLOCK, LARGEST_MAX_FRAMES};

#[doc(hidden)]
pub mod __private {
    pub use manguebyte_plugin::Plugin;

    /// Makes an instance of `P` and returns its
    /// `AudioComponentPlugInInterface`, or null. What the factory function
    /// of [`export_au!`](crate::export_au) returns; a host written in Rust,
    /// such as a test, can call it directly.
    pub fn factory<P: Plugin>() -> *mut std::ffi::c_void {
        crate::wrapper::factory::<P>()
    }

    /// The `Info.plist` of `P`'s component bundle, from the identity
    /// [`plugin_info`](manguebyte_plugin::plugin_info) gives: what
    /// `__manguebyte_au_info_plist` returns.
    pub fn info_plist<P: Plugin>(executable: &str, identifier: &str, version: &str) -> String {
        crate::info_plist(
            manguebyte_plugin::plugin_info::<P>(),
            executable,
            identifier,
            version,
        )
    }
}

/// Exports a plugin as the Audio Unit of this library.
///
/// Use it once, at the root of a crate whose `crate-type` is `cdylib` and
/// `rlib`. On macOS it defines the factory function the component's
/// `Info.plist` names ([`FACTORY_FUNCTION`]). On every system it defines a
/// hidden function, `__manguebyte_au_info_plist`, which gives that
/// `Info.plist` as [`info_plist`] writes it; `cargo mangue bundle --format
/// au` calls it from a small program it builds against the plugin's crate,
/// as it does for an LV2 bundle's Turtle files.
#[macro_export]
macro_rules! export_au {
    ($plugin:ty) => {
        /// The factory function an Audio Unit host calls, named in the
        /// component's `Info.plist`.
        #[cfg(target_os = "macos")]
        #[allow(non_snake_case)]
        #[unsafe(no_mangle)]
        pub extern "C" fn ManguebyteAudioUnitFactory(
            _description: *const ::core::ffi::c_void,
        ) -> *mut ::core::ffi::c_void {
            $crate::__private::factory::<$plugin>()
        }

        /// The `Info.plist` of this plugin's component bundle, for an
        /// executable named `executable`, a bundle identifier and a bundle
        /// version, from the plugin's identity as `plugin_info` gives it.
        /// Used by `cargo mangue bundle`.
        #[doc(hidden)]
        pub fn __manguebyte_au_info_plist(
            executable: &str,
            identifier: &str,
            version: &str,
        ) -> ::std::string::String {
            $crate::__private::info_plist::<$plugin>(executable, identifier, version)
        }
    };
}
