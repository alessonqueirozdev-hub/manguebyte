//! Builds VST 3 plugins from a [`Plugin`](manguebyte_plugin::Plugin).
//!
//! # Making a plugin
//!
//! In a crate of type `cdylib`, implement `Plugin` and export it:
//!
//! ```ignore
//! manguebyte_vst3::export_vst3!(MyPlugin);
//! ```
//!
//! Build it and put the library inside a VST 3 bundle, which is a folder
//! with a fixed layout; `scripts/bundle-vst3.sh` in the Manguebyte
//! repository does this. The same library can also export CLAP: the two
//! formats use different symbols.
//!
//! # What the wrapper provides
//!
//! | VST 3 feature | Behaviour |
//! |---|---|
//! | Audio buses | Main input and output from the plugin's layout, plus a sidechain input if it has one. The layout is fixed |
//! | Event bus | For instruments and note effects |
//! | Automation | Every point of a parameter's queue is delivered on its frame, so automation is sample-accurate |
//! | Notes | Note on and off with their ids, note tuning, polyphonic pressure, and the volume, pan, tuning, vibrato and brightness note expressions |
//! | MIDI controllers | Controllers, channel pressure and pitch bend, which VST 3 delivers only as parameters, arrive as MIDI events again |
//! | Parameters | Listing, values, text both ways. The hidden parameters that carry MIDI controllers are filed in a unit of their own |
//! | State | Saved and restored as text; another plugin's state is refused |
//! | Latency | Reported from the plugin each time it is activated |
//! | Blocks with no audio | Parameter changes are stored and announced on the next block that has audio; controller moves are kept for it |
//!
//! The processor and the controller are one object. The class id a host
//! stores is derived from the plugin's text identifier by [`class_id`], and
//! parameter ids by [`param_id`]. Both are the same on every platform, so a
//! session moves between them.
//!
//! Not there yet: an editor, 64-bit processing, changing the bus layout,
//! program lists, and telling the host about a change the plugin made to
//! its own parameter.
//!
//! # How it is checked
//!
//! `tests/abi.rs` plays the part of a host: it creates the plugin through
//! the factory and drives it through the COM interfaces, with a counting
//! allocator around every `process` call. The same tests run under Miri.
//! The example plugin is also run through Steinberg's `validator`, from the
//! VST 3 SDK, on Windows, Linux and macOS.
//!
//! A panic inside the plugin is caught at the boundary and reported to the
//! host as an error, rather than unwinding into it or aborting it.
//!
//! # Trademark
//!
//! VST is a trademark of Steinberg Media Technologies GmbH. Distributing a
//! VST 3 plugin is subject to Steinberg's trademark and logo guidelines.

mod factory;
mod handler;
mod run_loop;
mod text;
mod view;
mod wrapper;

pub use factory::plugin_factory;
pub use text::{class_id, param_id};
pub use vst3;

/// Exports a plugin as the VST 3 entry points of this library.
///
/// Use it once, in a crate whose `crate-type` is `cdylib`.
#[macro_export]
macro_rules! export_vst3 {
    ($plugin:ty) => {
        /// The entry point a VST 3 host looks for.
        #[allow(non_snake_case)]
        #[unsafe(no_mangle)]
        pub extern "system" fn GetPluginFactory() -> *mut ::core::ffi::c_void {
            $crate::plugin_factory::<$plugin>()
        }

        /// Called by a VST 3 host when the library is loaded.
        #[cfg(target_os = "windows")]
        #[allow(non_snake_case)]
        #[unsafe(no_mangle)]
        pub extern "system" fn InitDll() -> bool {
            true
        }

        /// Called by a VST 3 host before the library is unloaded.
        #[cfg(target_os = "windows")]
        #[allow(non_snake_case)]
        #[unsafe(no_mangle)]
        pub extern "system" fn ExitDll() -> bool {
            true
        }

        /// Called by a VST 3 host when the bundle is loaded.
        #[cfg(target_os = "macos")]
        #[allow(non_snake_case)]
        #[unsafe(no_mangle)]
        pub extern "system" fn bundleEntry(_bundle: *mut ::core::ffi::c_void) -> bool {
            true
        }

        /// Called by a VST 3 host before the bundle is unloaded.
        #[cfg(target_os = "macos")]
        #[allow(non_snake_case)]
        #[unsafe(no_mangle)]
        pub extern "system" fn bundleExit() -> bool {
            true
        }

        /// Called by a VST 3 host when the library is loaded.
        #[cfg(all(unix, not(target_os = "macos")))]
        #[allow(non_snake_case)]
        #[unsafe(no_mangle)]
        pub extern "system" fn ModuleEntry(_library: *mut ::core::ffi::c_void) -> bool {
            true
        }

        /// Called by a VST 3 host before the library is unloaded.
        #[cfg(all(unix, not(target_os = "macos")))]
        #[allow(non_snake_case)]
        #[unsafe(no_mangle)]
        pub extern "system" fn ModuleExit() -> bool {
            true
        }
    };
}
