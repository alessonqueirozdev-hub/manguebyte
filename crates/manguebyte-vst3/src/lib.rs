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
//! | MIDI controllers | Controllers, channel pressure and pitch bend, which VST 3 delivers only as parameters, arrive as MIDI events again, on their frames. Only the ones in [`Plugin::MIDI_CONTROLLERS`](manguebyte_plugin::Plugin::MIDI_CONTROLLERS) are exposed: one hidden parameter each, which `IMidiMapping` gives for every channel and whose moves arrive on all 16 channels, or, for a plugin that asked for channels, one per controller and channel, whose moves arrive on their own channel. See [MIDI controllers](#midi-controllers) |
//! | Parameters | Listing, values, text both ways. The hidden parameters that carry MIDI controllers come after the plugin's own and are filed in a unit of their own |
//! | State | Saved and restored as text; another plugin's state is refused |
//! | Latency | Reported from the plugin each time it is activated |
//! | Blocks with no audio | Parameter changes are stored and announced on the next block that has audio; controller moves are kept for it |
//! | Component handler | Kept from `setComponentHandler` until the host replaces it or takes it away, the component is terminated, or it goes away. The editor's gestures reach the host through it as `beginEdit`, `performEdit` with the value the control set, and `endEdit`. Each handler sees begins and ends in pairs, also when the host replaces it or terminates the component from inside one of its calls |
//! | Editor | For a plugin with an [editor](manguebyte_plugin::editor): `createView("editor")` gives an `IPlugView` with `IPlugViewContentScaleSupport`, for `HWND`, `NSView` and `X11EmbedWindowID` parents, which passes the window the host gives on to the editor. The host's `IPlugFrame` takes the editor's resize requests, and the `IRunLoop` behind it, where there is one, is the editor's loop. There is no window backend yet: this is tested through the COM interfaces against a fake editor and a fake host |
//!
//! The processor and the controller are one object. The class id a host
//! stores is derived from the plugin's text identifier by [`class_id`], and
//! parameter ids by [`param_id`]. Both are the same on every platform, so a
//! session moves between them.
//!
//! Not there yet: a window for the editor, 64-bit processing, changing the
//! bus layout, program lists, and telling the host about a change the
//! plugin made to its own parameter outside an editor's gesture.
//!
//! # MIDI controllers
//!
//! VST 3 has no MIDI controller events. A host that receives a controller,
//! channel pressure or pitch bend asks the plugin's `IMidiMapping` which
//! parameter stands for it on that channel and moves that parameter, and
//! the wrapper turns the move back into the MIDI message on its frame.
//!
//! What a plugin declares in
//! [`Plugin::MIDI_CONTROLLERS`](manguebyte_plugin::Plugin::MIDI_CONTROLLERS)
//! decides which parameters exist. With the default,
//! [`MidiControllers::MUSICAL`](manguebyte_plugin::MidiControllers::MUSICAL),
//! an instrument gets 13 hidden parameters, titled `MIDI Pitch Bend`,
//! `MIDI Pressure` and `MIDI CC 64` and so on, after its own. Every channel
//! maps to the same parameter, so the channel a move came from is lost;
//! the wrapper delivers each move as 16 messages, one on each channel in
//! channel order, on the move's frame. A plugin that keeps controllers per
//! channel, such as one built on `manguebyte_dsp::Synth`, then bends,
//! holds and silences notes on every channel, at the price of one
//! channel's controllers reaching the notes of the others. A plugin that
//! asked for channels gets one parameter per controller and channel,
//! titled `MIDI 1 CC 64` to `MIDI 16 CC 64`, and each move arrives once,
//! on its own channel. A block carries at most 4,096 events, so 256 moves
//! of shared controllers fill it. A controller outside the set is mapped
//! to nothing and never arrives. An effect has no event bus and no
//! controller parameters.
//!
//! The parameters have the `kIsHidden` flag, but some hosts, REAPER among
//! them, list hidden parameters in their generic view anyway; with the
//! default set that is 13 entries, not the 2,080 that every controller on
//! every channel would make. Their ids are fixed: the controller with
//! index `c` (as [`MidiController::index`](manguebyte_plugin::MidiController::index)
//! numbers it) on channel `ch` is `0x7F00_0000 + ch * 130 + c`, and a
//! plugin's own parameters may not use that range. They are not part of
//! the plugin's state.
//!
//! # Editors
//!
//! A plugin whose [`Plugin::editor`](manguebyte_plugin::Plugin::editor)
//! returns a spec answers `createView("editor")` with a view; one without
//! answers null, as before. The wrapper keeps to the
//! [editor contract](manguebyte_plugin::editor):
//!
//! - `attached` opens the editor inside the host's window and `removed`
//!   drops it, then its link, which ends any gesture it left open and
//!   unregisters its timers and descriptors. A view may be attached and
//!   removed again any number of times; one attached twice, or removed
//!   without being attached, says `kResultFalse`. A view the host keeps
//!   after the component is terminated or released answers without an
//!   editor and opens none.
//! - Sizes are the host's: physical pixels for `HWND` and
//!   `X11EmbedWindowID`, scaled by `setContentScaleFactor`, and points for
//!   `NSView`, where the scale is refused. On macOS, where every parent is
//!   an `NSView`, the scale is refused before the view is attached too, so
//!   that `getSize` there reports the size the editor opens at.
//!   `checkSizeConstraint` answers
//!   with the editor's [`fit`](manguebyte_plugin::editor::OpenEditor::fit),
//!   or the spec's [`Resize`](manguebyte_plugin::editor::Resize) rules
//!   before it is open; `canResize` comes from those rules. A size set with
//!   `onSize` before the editor is open is given to it once it is.
//! - `onKeyDown` and `onKeyUp` go to the editor's
//!   [`key`](manguebyte_plugin::editor::OpenEditor::key), and a key it does
//!   not use goes back to the host. `onWheel` stays with the host.
//! - While blocks run, an edit changes what the controller reports and
//!   reaches the processor only through the host, which sends the value
//!   back as a point in `process`; while they do not, it is stored at once.
//!   Nothing compares values to find edits. See the module documentation of
//!   the wrapper, in the source, for why.
//! - The host may call the plugin back from inside any of its functions,
//!   such as `performEdit`, `endEdit`, `resizeView` and its own loop.
//!   Nothing reaches the editor, or drops it, while it is calling out:
//!   `onSize` from inside `resizeView` is that request's answer, sizes are
//!   answered from what is known, and a size, a scale or a `removed` that
//!   would need the editor is held until nothing is in progress. It is
//!   carried out at the end of the host's call into the view, if one was in
//!   progress, at the host's next call into the view, or the next time the
//!   host's `IRunLoop`, if it offers one, calls the editor's timers,
//!   descriptors or wake, whichever comes first. Until then the editor of a
//!   removed view is not called, its timers do not fire, and it cannot ask
//!   the host for a size.
//!
//! ```
//! # use std::ffi::c_char;
//! # use std::sync::Arc;
//! # use manguebyte_core::AudioBuffer;
//! # use manguebyte_dsp::{ProcessContext, ProcessSpec};
//! # use manguebyte_params::{FloatParam, FloatRange, param_set};
//! # use manguebyte_plugin::{AudioLayout, Plugin, PluginInfo, PluginKind};
//! use manguebyte_plugin::editor::{
//!     EditorContext, EditorError, EditorSpec, HostSize, OpenEditor, ParentWindow,
//! };
//! use manguebyte_vst3::vst3::Steinberg::Vst::{IComponent, IEditController, IEditControllerTrait};
//! use manguebyte_vst3::vst3::Steinberg::{
//!     IPlugView, IPlugViewContentScaleSupport, IPlugViewContentScaleSupportTrait,
//!     IPlugViewTrait, IPluginBaseTrait, IPluginFactory, IPluginFactoryTrait, ViewRect,
//!     kResultOk,
//! };
//! use manguebyte_vst3::vst3::{ComPtr, Interface};
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
//! // What a host does: create the plugin, ask for its editor's view, and
//! // ask the size it needs on a screen at 150 %.
//! // SAFETY: each call follows VST 3's rules, with pointers that live for
//! // the call.
//! unsafe {
//!     let factory = manguebyte_vst3::plugin_factory::<Gain>();
//!     let factory = ComPtr::<IPluginFactory>::from_raw(factory.cast()).unwrap();
//!     let class = manguebyte_vst3::class_id(Gain::INFO.id);
//!     let mut object = std::ptr::null_mut();
//!     let iid: *const c_char = IComponent::IID.as_ptr().cast();
//!     assert_eq!(factory.createInstance(class.as_ptr(), iid, &mut object), kResultOk);
//!     let component = ComPtr::<IComponent>::from_raw(object.cast()).unwrap();
//!     assert_eq!(component.initialize(std::ptr::null_mut()), kResultOk);
//!     let controller: ComPtr<IEditController> = component.cast().unwrap();
//!
//!     let view = controller.createView(c"editor".as_ptr());
//!     let view = ComPtr::<IPlugView>::from_raw(view).expect("Gain has an editor");
//!     let scaling: ComPtr<IPlugViewContentScaleSupport> = view.cast().unwrap();
//!     let scaled = scaling.setContentScaleFactor(1.5) == kResultOk;
//!     let mut rect = ViewRect { left: 0, top: 0, right: 0, bottom: 0 };
//!     assert_eq!(view.getSize(&mut rect), kResultOk);
//!     if cfg!(target_os = "macos") {
//!         // Points, which the system scales: the host's scale is refused.
//!         assert!(!scaled);
//!         assert_eq!((rect.right, rect.bottom), (300, 200));
//!     } else {
//!         assert!(scaled);
//!         assert_eq!((rect.right, rect.bottom), (450, 300));
//!     }
//!
//!     drop(view);
//!     assert_eq!(component.terminate(), kResultOk);
//! }
//! ```
//!
//! # How it is checked
//!
//! `tests/abi.rs` plays the part of a host: it creates the plugin through
//! the factory and drives it through the COM interfaces, with a counting
//! allocator around every `process` call. `tests/view.rs` does the same
//! for a plugin with a fake editor, with a fake component handler, frame
//! and run loop that record every call and can call the plugin back from
//! inside their own functions. Continuous integration also runs these tests
//! under Miri, on Linux, with fewer rounds, and with two differences: Miri
//! cannot switch a socket to non-blocking mode, so the build it checks has
//! no Unix wake and the test of the wake is left out, and the test of a
//! view released from inside its editor's own call, which leaks the editor
//! on purpose, is skipped. The example plugins, which have no editor, are
//! also run through Steinberg's `validator`, from the VST 3 SDK, on
//! Windows, Linux and macOS.
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
