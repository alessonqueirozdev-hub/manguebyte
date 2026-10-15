//! Builds [LV2](https://lv2plug.in) plugins from a
//! [`Plugin`](manguebyte_plugin::Plugin).
//!
//! LV2 is the open plugin standard of Linux audio. A plugin is a folder, a
//! *bundle*, holding the plugin's library and Turtle files that describe it,
//! which a host reads before it loads the library.
//!
//! # Making a plugin
//!
//! In a crate of type `cdylib`, implement `Plugin` and export it:
//!
//! ```ignore
//! manguebyte_lv2::export_lv2!(MyPlugin);
//! ```
//!
//! This defines `lv2_descriptor`, the function an LV2 host looks for. The
//! bundle's Turtle files come from [`bundle_files`]: put them and the
//! library in a folder named `MyPlugin.lv2`, in a folder your host scans.
//!
//! # What the wrapper provides
//!
//! | LV2 | Behaviour |
//! |---|---|
//! | URI | `urn:manguebyte:` followed by the plugin's id ([`URI_PREFIX`], [`plugin_uri`]) |
//! | Features | `urid:map` is required. `options:options` with `buf-size:maxBlockLength` sets the longest block the plugin is prepared for (4096 frames without it, never more than 65536); `log:log`, when offered, is told why an instance or a state was refused |
//! | Audio ports | The main inputs and outputs of the plugin's layout, and the sidechain as more inputs with `lv2:isSideChain`. In-place processing is allowed |
//! | Control ports | One input per parameter, in the parameter's own unit and range. A port the host changes becomes a parameter event on frame 0 of the block |
//! | Atom ports | A `control` input sequence of MIDI events, each on its frame, and `time:Position` objects for the transport; a `notify` output sequence, written empty |
//! | Latency | A control output with `lv2:designation lv2:latency`, written on every block |
//! | State | `state:interface`, holding the text of [`save_state`](manguebyte_plugin::save_state) as an `atom:String` under [`STATE_KEY_URI`]. Another plugin's state is refused and changes nothing |
//!
//! The port indices are given by [`PortLayout`].
//!
//! Inside a block, the plugin is given one event stream, as the CLAP and
//! VST 3 wrappers give it: on frame 0, every parameter that changed without
//! the host (such as by a restored state), then the parameters whose ports
//! the host moved, then the MIDI messages on their frames. Where a
//! `time:Position` falls inside the block, the block is processed in two
//! pieces, so that the plugin sees the new transport from that frame on;
//! a block longer than the bound is processed in pieces of the bound. The
//! transport moves on by itself between positions, at the host's speed and
//! tempo. A position stamped past the end of the block describes a frame of
//! a later block; it is moved back by the difference, at its own speed and
//! tempo, and holds from the start of the next block. LV2 counts beats in
//! `time:beatUnit` notes, and the framework in quarter notes, so tempo and
//! positions are converted. The start of the bar is `time:beat` minus
//! `time:barBeat` when the host gives both, and `time:bar` times
//! `time:beatsPerBar` only when it does not give the running beat. A place
//! or tempo that does not come out as a finite number is passed on as
//! unknown.
//!
//! Nothing that `run` does allocates, locks or waits. Subnormal numbers are
//! flushed to zero while it runs and the host's setting is put back after.
//! A panic inside the plugin is caught at the boundary; that block's
//! outputs are silent, and the next block starts by giving the plugin every
//! parameter's value, since that block's changes may not have reached it.
//!
//! An editor is not offered: LV2 editors are separate libraries.
//!
//! # Driving a plugin as a host does
//!
//! ```
//! # use std::ffi::{CStr, c_char, c_void};
//! # use std::sync::{Arc, Mutex};
//! # use manguebyte_core::AudioBuffer;
//! # use manguebyte_dsp::{ProcessContext, ProcessSpec};
//! # use manguebyte_params::{FloatParam, FloatRange, param_set};
//! # use manguebyte_plugin::{AudioLayout, Plugin, PluginInfo, PluginKind};
//! use manguebyte_lv2::sys::{LV2_Feature, LV2_URID_Map, LV2_URID__map};
//! use manguebyte_lv2::{PortLayout, PortRole};
//!
//! # struct GainParams { gain: FloatParam }
//! # param_set!(GainParams { gain });
//! # struct Gain { params: Arc<GainParams> }
//! # impl Plugin for Gain {
//! #     const INFO: PluginInfo = PluginInfo {
//! #         id: "org.example.gain", name: "Gain", vendor: "Example", version: "1.0.0",
//! #         description: "", kind: PluginKind::Effect,
//! #     };
//! #     const LAYOUT: AudioLayout = AudioLayout::MONO_EFFECT;
//! #     type Params = GainParams;
//! #     fn new() -> Self {
//! #         let gain = FloatParam::new("gain", "Gain", FloatRange::linear(0.0, 2.0), 1.0);
//! #         Self { params: Arc::new(GainParams { gain }) }
//! #     }
//! #     fn params(&self) -> Arc<GainParams> { Arc::clone(&self.params) }
//! #     fn prepare(&mut self, _: &ProcessSpec) {}
//! #     fn process(&mut self, buffer: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {
//! #         buffer.apply_gain(self.params.gain.value());
//! #     }
//! # }
//! // A host's URID map: each URI gets the next number.
//! unsafe extern "C" fn map(handle: *mut c_void, uri: *const c_char) -> u32 {
//!     // SAFETY: `handle` is the list below, and `uri` a C string.
//!     let (uris, uri) = unsafe {
//!         (&*handle.cast::<Mutex<Vec<String>>>(), CStr::from_ptr(uri).to_str().unwrap())
//!     };
//!     let mut uris = uris.lock().unwrap();
//!     let index = uris.iter().position(|known| known == uri).unwrap_or_else(|| {
//!         uris.push(uri.to_owned());
//!         uris.len() - 1
//!     });
//!     index as u32 + 1
//! }
//! let uris = Mutex::new(Vec::<String>::new());
//! let mut urid_map = LV2_URID_Map { handle: (&raw const uris).cast_mut().cast(), map: Some(map) };
//! let feature = LV2_Feature { URI: LV2_URID__map.as_ptr(), data: (&raw mut urid_map).cast() };
//! let features = [&raw const feature, std::ptr::null()];
//!
//! let descriptor = manguebyte_lv2::descriptor!(Gain);
//! assert_eq!(descriptor.uri(), c"urn:manguebyte:org.example.gain");
//! let lv2 = descriptor.raw();
//! let ports = PortLayout::new(Gain::LAYOUT, 1);
//! let (mut input, mut output, mut gain) = ([1.0f32; 8], [0.0f32; 8], 0.5f32);
//! // SAFETY: each call follows the LV2 rules, with pointers that live for
//! // as long as the instance uses them.
//! unsafe {
//!     let plugin = lv2.instantiate.unwrap()(descriptor.as_ptr(), 48_000.0, c"".as_ptr(), features.as_ptr());
//!     assert!(!plugin.is_null());
//!     let connect = lv2.connect_port.unwrap();
//!     let index = |role| ports.index(role).unwrap() as u32;
//!     connect(plugin, index(PortRole::AudioInput(0)), input.as_mut_ptr().cast());
//!     connect(plugin, index(PortRole::AudioOutput(0)), output.as_mut_ptr().cast());
//!     connect(plugin, index(PortRole::Param(0)), (&raw mut gain).cast());
//!     lv2.activate.unwrap()(plugin);
//!     lv2.run.unwrap()(plugin, 8);
//!     assert_eq!(output, [0.5; 8]);
//!     lv2.deactivate.unwrap()(plugin);
//!     lv2.cleanup.unwrap()(plugin);
//! }
//! ```

mod atom;
mod ports;
pub mod sys;
mod turtle;
mod uri;
mod wrapper;

pub use ports::{PortLayout, PortRole};
pub use turtle::{BundleFiles, bundle_files};
pub use uri::{STATE_KEY_URI, URI_PREFIX, plugin_uri, uri_for_id};
#[doc(hidden)]
pub use uri::{c_str, uri_c_bytes, uri_c_len};
pub use wrapper::{DEFAULT_MAX_BLOCK, Descriptor, EVENTS_PER_BLOCK, LARGEST_MAX_BLOCK};

#[doc(hidden)]
pub mod __private {
    pub use manguebyte_plugin::Plugin;
}

/// The LV2 descriptor of a plugin type, as a `&'static`
/// [`Descriptor`].
///
/// The descriptor lives in an immutable static, with the plugin's URI
/// worked out from its id at compile time. A host written in Rust, such as
/// a test, can drive the plugin through it; [`export_lv2!`] hands it to
/// real hosts.
///
/// ```ignore
/// let descriptor = manguebyte_lv2::descriptor!(MyPlugin);
/// ```
#[macro_export]
macro_rules! descriptor {
    ($plugin:ty) => {{
        const LEN: usize = $crate::uri_c_len(<$plugin as $crate::__private::Plugin>::INFO.id);
        const BYTES: [u8; LEN] =
            $crate::uri_c_bytes::<LEN>(<$plugin as $crate::__private::Plugin>::INFO.id);
        const URI: &::core::ffi::CStr = $crate::c_str(&BYTES);
        static DESCRIPTOR: $crate::Descriptor = $crate::Descriptor::new::<$plugin>(URI);
        &DESCRIPTOR
    }};
}

/// Exports a plugin as the LV2 entry point of this library.
///
/// Use it once, in a crate whose `crate-type` is `cdylib`. It defines
/// `lv2_descriptor`, which returns the plugin's descriptor for index 0 and
/// null for any other.
#[macro_export]
macro_rules! export_lv2 {
    ($plugin:ty) => {
        /// The entry point an LV2 host looks for.
        #[unsafe(no_mangle)]
        pub extern "C" fn lv2_descriptor(index: u32) -> *const $crate::sys::LV2_Descriptor {
            if index == 0 {
                $crate::descriptor!($plugin).as_ptr()
            } else {
                ::core::ptr::null()
            }
        }
    };
}
