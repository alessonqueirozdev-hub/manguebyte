//! Builds [CLAP](https://cleveraudio.org) plugins from a
//! [`Plugin`](manguebyte_plugin::Plugin).
//!
//! CLAP is an open plugin standard whose model matches Manguebyte's own:
//! notes, per-note expression, parameter automation and modulation all
//! arrive as events with a frame position. Nothing has to be approximated on
//! the way in, so a plugin behaves in a host exactly as it does in the test
//! [`Harness`](manguebyte_plugin::Harness).
//!
//! # Making a plugin
//!
//! In a crate of type `cdylib`, implement `Plugin` and export it:
//!
//! ```ignore
//! manguebyte_clap::export_clap!(MyPlugin);
//! ```
//!
//! Build it, rename the resulting library to `MyPlugin.clap` and put it in a
//! folder your host scans.
//!
//! # What the wrapper provides
//!
//! | CLAP feature | Behaviour |
//! |---|---|
//! | Audio ports | Main input and output from the plugin's layout, plus a sidechain input if it has one. In-place processing is allowed when input and output match |
//! | Note port | For instruments and note effects, in the CLAP and MIDI dialects |
//! | Events in | Note on, off and choke; note expression; parameter value and modulation; MIDI. All keep their frame |
//! | Parameters | Listing, values, text conversion and flush. Continuous parameters are exposed as positions in `0..=1`, stepped ones as whole steps |
//! | Events out | From `process` and from `flush`: an editor's gestures as gesture begin, value and gesture end, and a parameter changed by anything other than the host as a value. The host's own values, restored state included, are never sent back. An event the host's list refuses is sent again on the next block or flush |
//! | State | Saved and restored as text; another plugin's state is refused |
//! | Latency | Reported from the plugin each time it is activated |
//! | Editor | For a plugin with an [editor](manguebyte_plugin::editor): `gui` for the `win32`, `cocoa` and `x11` window APIs, which passes the window handle the host gives on to the editor, and the host's `timer-support` and `posix-fd-support` as the editor's loop. There is no window backend yet: this is tested through the C interface against a fake editor and a fake host |
//!
//! A panic inside the plugin is caught at the boundary and reported to the
//! host as an error, rather than unwinding into it or aborting it.
//!
//! # Editors
//!
//! A plugin whose [`Plugin::editor`](manguebyte_plugin::Plugin::editor)
//! returns a spec offers CLAP's `gui` extension; one without offers nothing
//! more than before. The wrapper keeps to the
//! [editor contract](manguebyte_plugin::editor):
//!
//! - `create` and `set_parent` open the editor with the host's window as its
//!   parent, and `destroy` drops it, then its link, which ends any gesture
//!   it left open and unregisters its timers and descriptors. Floating
//!   windows are not offered.
//! - Sizes are the host's: physical pixels for Win32 and X11, scaled by
//!   `set_scale`, and points for Cocoa, where `set_scale` is refused.
//!   `adjust_size` and the resize hints come from the spec's
//!   [`Resize`](manguebyte_plugin::editor::Resize) rules. A size the host
//!   sets before `set_parent` is given to the editor once it is open, unless
//!   the host granted it another size while it opened.
//! - A call out of order, or with arguments that make no sense, returns
//!   false. So does one that comes back while the editor is calling the
//!   host and would need the editor, with these exceptions: a `set_size`
//!   from inside the editor's own `request_resize` is that request's
//!   answer; `get_size` and `adjust_size` are answered without the editor;
//!   `on_main_thread` is put off and asked for again; and a `destroy` waits
//!   until the editor is out of the call, and is done before anything else
//!   the host asks next.
//! - A begin or an end of a gesture is counted, a value is stored straight
//!   into the parameter, and `process`, or `flush` when the host is not
//!   processing, sends the host the begin, the value and the end, in that
//!   order. While the host is not processing, each edit asks it for a flush
//!   with `request_flush`, the only call to the host a gesture makes.
//!   Nothing waits or allocates, and nothing between the editor and the
//!   audio thread can fill up.
//!
//! ```
//! # use std::ffi::{c_char, c_void};
//! # use std::sync::Arc;
//! # use manguebyte_clap::clap_sys::ext::gui::{CLAP_EXT_GUI, CLAP_WINDOW_API_WIN32, clap_plugin_gui};
//! # use manguebyte_clap::clap_sys::factory::plugin_factory::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
//! # use manguebyte_clap::clap_sys::host::clap_host;
//! # use manguebyte_clap::clap_sys::version::CLAP_VERSION;
//! # use manguebyte_core::AudioBuffer;
//! # use manguebyte_dsp::{ProcessContext, ProcessSpec};
//! # use manguebyte_params::{FloatParam, FloatRange, param_set};
//! # use manguebyte_plugin::{AudioLayout, Plugin, PluginInfo, PluginKind};
//! use manguebyte_plugin::editor::{
//!     EditorContext, EditorError, EditorSpec, HostSize, OpenEditor, ParentWindow,
//! };
//!
//! # struct GainParams { gain: FloatParam }
//! # param_set!(GainParams { gain });
//! # struct Gain { params: Arc<GainParams> }
//! /// An editor 300 by 200 units at scale 1, which a toolkit would draw.
//! struct GainEditor;
//!
//! impl EditorSpec for GainEditor {
//!     fn size(&self, scale: f64) -> HostSize {
//!         HostSize::new(300, 200).scale_by(scale)
//!     }
//!     fn open(&self, parent: ParentWindow, _: Option<f64>, _: EditorContext)
//!         -> Result<Box<dyn OpenEditor>, EditorError>
//!     {
//!         Err(EditorError::Unsupported(parent.api()))
//!     }
//! }
//!
//! impl Plugin for Gain {
//! #   const INFO: PluginInfo = PluginInfo {
//! #       id: "org.example.gain", name: "Gain", vendor: "Example", version: "1.0.0",
//! #       description: "", kind: PluginKind::Effect,
//! #   };
//! #   const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT;
//! #   type Params = GainParams;
//! #   fn new() -> Self {
//! #       let gain = FloatParam::new("gain", "Gain", FloatRange::linear(0.0, 2.0), 1.0);
//! #       Self { params: Arc::new(GainParams { gain }) }
//! #   }
//! #   fn params(&self) -> Arc<GainParams> { Arc::clone(&self.params) }
//! #   fn prepare(&mut self, _: &ProcessSpec) {}
//! #   fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
//!     // INFO, LAYOUT, Params, new, params, prepare and process as usual.
//!     fn editor(&self) -> Option<Box<dyn EditorSpec>> {
//!         Some(Box::new(GainEditor))
//!     }
//! }
//!
//! // What a host does: create the plugin, find `gui`, ask for the size the
//! // editor needs on a screen at 150 %.
//! # unsafe extern "C" fn no_extension(_: *const clap_host, _: *const c_char) -> *const c_void {
//! #     std::ptr::null()
//! # }
//! # let host = Box::into_raw(Box::new(clap_host {
//! #     clap_version: CLAP_VERSION, host_data: std::ptr::null_mut(),
//! #     name: c"host".as_ptr(), vendor: c"".as_ptr(), url: c"".as_ptr(), version: c"1".as_ptr(),
//! #     get_extension: Some(no_extension), request_restart: None, request_process: None,
//! #     request_callback: None,
//! # }));
//! let entry = manguebyte_clap::plugin_entry::<Gain>();
//! // SAFETY: each call follows CLAP's rules, with pointers that live for
//! // the call.
//! unsafe {
//!     let factory = entry.get_factory.unwrap()(CLAP_PLUGIN_FACTORY_ID.as_ptr())
//!         .cast::<clap_plugin_factory>();
//!     let plugin = (*factory).create_plugin.unwrap()(factory, host, c"org.example.gain".as_ptr());
//!     assert!((*plugin).init.unwrap()(plugin));
//!     let gui = &*(*plugin).get_extension.unwrap()(plugin, CLAP_EXT_GUI.as_ptr())
//!         .cast::<clap_plugin_gui>();
//!
//!     assert!(gui.create.unwrap()(plugin, CLAP_WINDOW_API_WIN32.as_ptr(), false));
//!     assert!(gui.set_scale.unwrap()(plugin, 1.5));
//!     let (mut width, mut height) = (0, 0);
//!     assert!(gui.get_size.unwrap()(plugin, &mut width, &mut height));
//!     assert_eq!((width, height), (450, 300));
//!     gui.destroy.unwrap()(plugin);
//!
//!     (*plugin).destroy.unwrap()(plugin);
//! #   drop(Box::from_raw(host));
//! }
//! ```

mod gui;
mod wrapper;

pub use clap_sys;
pub use wrapper::plugin_entry;

/// Exports a plugin as the CLAP entry point of this library.
///
/// Use it once, in a crate whose `crate-type` is `cdylib`.
#[macro_export]
macro_rules! export_clap {
    ($plugin:ty) => {
        /// The entry point a CLAP host looks for.
        #[allow(non_upper_case_globals)]
        #[unsafe(no_mangle)]
        pub static clap_entry: $crate::clap_sys::entry::clap_plugin_entry =
            $crate::plugin_entry::<$plugin>();
    };
}
