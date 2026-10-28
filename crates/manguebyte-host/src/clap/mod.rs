//! Hosting [CLAP](https://cleveraudio.org) plugins: loading a plugin made
//! by anyone and running it as a [`Processor`](manguebyte_dsp::Processor).
//!
//! # The three types
//!
//! | Type | What it is | Threads |
//! |---|---|---|
//! | [`ClapLibrary`] | A loaded plugin file and the list of plugins in it | Any; can be cloned and sent |
//! | [`ClapPlugin`] | One plugin, as the main thread deals with it: ports, parameters, state, activation, its editor and the editor's timers | The thread that created it, and no other |
//! | [`ClapProcessor`] | The same plugin, as the audio thread deals with it: a `Processor<f32>` | One thread at a time; can be sent |
//!
//! CLAP says which thread may call each of a plugin's functions. Here those
//! rules are the shape of the API instead of a paragraph to remember.
//! `ClapPlugin` is not `Send`, so the compiler refuses to move it off the
//! thread it was created on. [`ClapPlugin::activate`] hands out the one
//! `ClapProcessor`, which is `Send` and whose methods take `&mut self`, so
//! exactly one thread at a time can be processing. Nothing on the audio
//! half can load state or activate, and nothing on the main half can
//! process.
//!
//! While audio is processed the two halves share no lock. A parameter set
//! on the main thread travels to the audio thread through a wait-free queue
//! and reaches the plugin as an event at the start of the next block; what
//! the plugin reports travels back through another. One lock does exist
//! between them. It is taken when the audio half is dropped, to hand its
//! end of the queue back to the main half, and dropping the audio half is
//! not a real-time operation.
//!
//! # Hosting a plugin from a file
//!
//! This example is compiled and not run, since it needs a plugin on disk.
//! Loading is the one step that is `unsafe`: see [`ClapLibrary::load`].
//!
//! ```no_run
//! use manguebyte_core::AudioBuffer;
//! use manguebyte_dsp::event::{Event, EventBuffer, EventKind, NoteRef};
//! use manguebyte_dsp::{ProcessContext, Processor, Transport};
//! use manguebyte_host::clap::ClapLibrary;
//!
//! # fn main() -> Result<(), manguebyte_host::clap::ClapError> {
//! // SAFETY: the file is a CLAP plugin that this program is willing to run.
//! let library = unsafe { ClapLibrary::load("Mangue Pluck.clap") }?;
//! for plugin in library.descriptors() {
//!     println!("{} ({})", plugin.name, plugin.id);
//! }
//!
//! // Main thread: create, inspect, activate.
//! let mut plugin = library.create("org.manguebyte.pluck")?;
//! for param in plugin.params() {
//!     println!("{}: {}..{}", param.name, param.min, param.max);
//! }
//! let mut processor = plugin.activate(48_000.0, 512)?;
//!
//! // Audio thread: `processor` is a `Processor<f32>` and can be moved there.
//! let mut events = EventBuffer::with_capacity(16);
//! let note = NoteRef::new(0, 60);
//! events.push(Event { offset: 0, kind: EventKind::NoteOn { note, velocity: 0.8 } }).ok();
//! let transport = Transport::playing_at(120.0);
//! let mut block = AudioBuffer::<f32>::new(2, 512);
//! processor.process_with(&mut block, &ProcessContext::new(&events, &transport));
//!
//! // Main thread, now and then: let the plugin have its say.
//! for event in plugin.pump() {
//!     println!("{event:?}");
//! }
//!
//! // The audio half goes first; then the plugin can be deactivated.
//! drop(processor);
//! plugin.deactivate()?;
//! # Ok(())
//! # }
//! ```
//!
//! # A session from start to end
//!
//! This example runs as a test. So that it needs no file, it hosts a
//! plugin of its own: a gain with one parameter, written against the C
//! interface in lines this page leaves out (they are in the source of this
//! module). A plugin from a file is used in exactly the same way.
//!
//! ```
//! use manguebyte_core::{AudioBuffer, ParamId};
//! use manguebyte_dsp::{ProcessContext, Processor};
//! use manguebyte_host::clap::{ClapLibrary, HostEvent, ProcessStatus};
//! # use std::ffi::{CStr, c_char, c_void};
//! # use std::sync::atomic::{AtomicU64, Ordering};
//! # use manguebyte_host::clap::clap_sys::entry::clap_plugin_entry;
//! # use manguebyte_host::clap::clap_sys::events::*;
//! # use manguebyte_host::clap::clap_sys::ext::audio_ports::*;
//! # use manguebyte_host::clap::clap_sys::ext::params::*;
//! # use manguebyte_host::clap::clap_sys::ext::state::*;
//! # use manguebyte_host::clap::clap_sys::factory::plugin_factory::*;
//! # use manguebyte_host::clap::clap_sys::host::clap_host;
//! # use manguebyte_host::clap::clap_sys::id::CLAP_INVALID_ID;
//! # use manguebyte_host::clap::clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
//! # use manguebyte_host::clap::clap_sys::process::*;
//! # use manguebyte_host::clap::clap_sys::stream::{clap_istream, clap_ostream};
//! # use manguebyte_host::clap::clap_sys::version::CLAP_VERSION;
//! #
//! # // --- The example plugin -------------------------------------------
//! # //
//! # // A stereo gain. There is only ever one of it, so the plugin and its
//! # // one value are statics: nothing is allocated and nothing destroyed.
//! #
//! # /// The gain, as the bits of an `f64`. It starts at 1.
//! # static GAIN_BITS: AtomicU64 = AtomicU64::new(1.0f64.to_bits());
//! #
//! # fn gain() -> f64 {
//! #     f64::from_bits(GAIN_BITS.load(Ordering::Relaxed))
//! # }
//! #
//! # /// A list of feature words, which is an array of pointers.
//! # struct Features([*const c_char; 2]);
//! # // SAFETY: the pointers are to a string literal and to nothing, and
//! # // neither ever changes.
//! # unsafe impl Sync for Features {}
//! # static FEATURES: Features = Features([c"audio-effect".as_ptr(), std::ptr::null()]);
//! #
//! # static DESCRIPTOR: clap_plugin_descriptor = clap_plugin_descriptor {
//! #     clap_version: CLAP_VERSION,
//! #     id: c"org.example.gain".as_ptr(),
//! #     name: c"Example Gain".as_ptr(),
//! #     vendor: c"Example".as_ptr(),
//! #     url: c"".as_ptr(),
//! #     manual_url: c"".as_ptr(),
//! #     support_url: c"".as_ptr(),
//! #     version: c"1.0".as_ptr(),
//! #     description: c"Makes things louder or quieter.".as_ptr(),
//! #     features: FEATURES.0.as_ptr(),
//! # };
//! #
//! # unsafe extern "C" fn agree(_plugin: *const clap_plugin) -> bool {
//! #     true
//! # }
//! # unsafe extern "C" fn nothing(_plugin: *const clap_plugin) {}
//! # unsafe extern "C" fn activate(_: *const clap_plugin, _rate: f64, _min: u32, _max: u32) -> bool {
//! #     true
//! # }
//! #
//! # /// Takes the parameter values out of a list of events.
//! # ///
//! # /// # Safety
//! # ///
//! # /// `events` must be a list a host handed over, valid for this call.
//! # unsafe fn read_events(events: *const clap_input_events) {
//! #     // SAFETY: per the contract the list and its two functions are
//! #     // valid, and each event is as large as its type says.
//! #     unsafe {
//! #         let (Some(size), Some(get)) = ((*events).size, (*events).get) else {
//! #             return;
//! #         };
//! #         for index in 0..size(events) {
//! #             let header = get(events, index);
//! #             if (*header).space_id == CLAP_CORE_EVENT_SPACE_ID
//! #                 && (*header).type_ == CLAP_EVENT_PARAM_VALUE
//! #             {
//! #                 let event = &*header.cast::<clap_event_param_value>();
//! #                 if event.param_id == 1 {
//! #                     GAIN_BITS.store(event.value.to_bits(), Ordering::Relaxed);
//! #                 }
//! #             }
//! #         }
//! #     }
//! # }
//! #
//! # unsafe extern "C" fn process(
//! #     _plugin: *const clap_plugin,
//! #     process: *const clap_process,
//! # ) -> clap_process_status {
//! #     // SAFETY: a host passes a valid block with the ports this plugin
//! #     // declares, one stereo input and one stereo output, each channel
//! #     // `frames_count` frames long, and input and output apart.
//! #     unsafe {
//! #         let process = &*process;
//! #         read_events(process.in_events);
//! #         let gain = gain() as f32;
//! #         let frames = process.frames_count as usize;
//! #         let (input, output) = (&*process.audio_inputs, &*process.audio_outputs);
//! #         for channel in 0..2 {
//! #             let source = std::slice::from_raw_parts(*input.data32.add(channel), frames);
//! #             let target = std::slice::from_raw_parts_mut(*output.data32.add(channel), frames);
//! #             for (out, sample) in target.iter_mut().zip(source) {
//! #                 *out = sample * gain;
//! #             }
//! #         }
//! #     }
//! #     CLAP_PROCESS_CONTINUE
//! # }
//! #
//! # /// Writes `text` and its terminator into a field of a struct.
//! # fn write_text(field: &mut [c_char], text: &[u8]) {
//! #     let room = field.len().saturating_sub(1);
//! #     for (slot, byte) in field.iter_mut().zip(text.iter().take(room).chain(&[0u8])) {
//! #         *slot = *byte as c_char;
//! #     }
//! # }
//! #
//! # unsafe extern "C" fn param_count(_plugin: *const clap_plugin) -> u32 {
//! #     1
//! # }
//! # unsafe extern "C" fn param_info(
//! #     _plugin: *const clap_plugin,
//! #     index: u32,
//! #     info: *mut clap_param_info,
//! # ) -> bool {
//! #     if index != 0 {
//! #         return false;
//! #     }
//! #     // SAFETY: a host passes a struct to fill in.
//! #     let info = unsafe { &mut *info };
//! #     info.id = 1;
//! #     info.flags = CLAP_PARAM_IS_AUTOMATABLE;
//! #     info.cookie = std::ptr::null_mut();
//! #     write_text(&mut info.name, b"Gain");
//! #     write_text(&mut info.module, b"");
//! #     (info.min_value, info.max_value, info.default_value) = (0.0, 2.0, 1.0);
//! #     true
//! # }
//! # unsafe extern "C" fn param_value(_: *const clap_plugin, id: u32, value: *mut f64) -> bool {
//! #     if id != 1 {
//! #         return false;
//! #     }
//! #     // SAFETY: a host passes a number to write to.
//! #     unsafe { *value = gain() };
//! #     true
//! # }
//! # unsafe extern "C" fn param_to_text(
//! #     _plugin: *const clap_plugin,
//! #     id: u32,
//! #     value: f64,
//! #     out: *mut c_char,
//! #     capacity: u32,
//! # ) -> bool {
//! #     if id != 1 || out.is_null() {
//! #         return false;
//! #     }
//! #     // SAFETY: a host passes `capacity` writable characters at `out`.
//! #     let out = unsafe { std::slice::from_raw_parts_mut(out, capacity as usize) };
//! #     write_text(out, format!("{value:.2} x").as_bytes());
//! #     true
//! # }
//! # unsafe extern "C" fn text_to_param(
//! #     _plugin: *const clap_plugin,
//! #     id: u32,
//! #     text: *const c_char,
//! #     value: *mut f64,
//! # ) -> bool {
//! #     // SAFETY: a host passes NUL-terminated text.
//! #     let text = unsafe { CStr::from_ptr(text) }.to_string_lossy();
//! #     match text.trim_end_matches(" x").trim().parse::<f64>() {
//! #         Ok(parsed) if id == 1 => {
//! #             // SAFETY: a host passes a number to write to.
//! #             unsafe { *value = parsed };
//! #             true
//! #         }
//! #         _ => false,
//! #     }
//! # }
//! # unsafe extern "C" fn param_flush(
//! #     _plugin: *const clap_plugin,
//! #     input: *const clap_input_events,
//! #     _output: *const clap_output_events,
//! # ) {
//! #     // SAFETY: passed on from the host.
//! #     unsafe { read_events(input) };
//! # }
//! # static PARAMS: clap_plugin_params = clap_plugin_params {
//! #     count: Some(param_count),
//! #     get_info: Some(param_info),
//! #     get_value: Some(param_value),
//! #     value_to_text: Some(param_to_text),
//! #     text_to_value: Some(text_to_param),
//! #     flush: Some(param_flush),
//! # };
//! #
//! # unsafe extern "C" fn port_count(_plugin: *const clap_plugin, _is_input: bool) -> u32 {
//! #     1
//! # }
//! # unsafe extern "C" fn port_info(
//! #     _plugin: *const clap_plugin,
//! #     index: u32,
//! #     is_input: bool,
//! #     info: *mut clap_audio_port_info,
//! # ) -> bool {
//! #     if index != 0 {
//! #         return false;
//! #     }
//! #     // SAFETY: a host passes a struct to fill in.
//! #     let info = unsafe { &mut *info };
//! #     info.id = u32::from(is_input);
//! #     write_text(&mut info.name, b"Main");
//! #     info.flags = CLAP_AUDIO_PORT_IS_MAIN;
//! #     info.channel_count = 2;
//! #     info.port_type = CLAP_PORT_STEREO.as_ptr();
//! #     info.in_place_pair = CLAP_INVALID_ID;
//! #     true
//! # }
//! # static AUDIO_PORTS: clap_plugin_audio_ports = clap_plugin_audio_ports {
//! #     count: Some(port_count),
//! #     get: Some(port_info),
//! # };
//! #
//! # unsafe extern "C" fn state_save(_: *const clap_plugin, stream: *const clap_ostream) -> bool {
//! #     let bytes = gain().to_le_bytes();
//! #     let mut rest = bytes.as_slice();
//! #     // A host may take fewer bytes than it is offered. The rest is
//! #     // offered again until all of it is taken.
//! #     while !rest.is_empty() {
//! #         // SAFETY: a host passes a valid stream, and `rest` is readable.
//! #         let taken = unsafe {
//! #             let Some(write) = (*stream).write else {
//! #                 return false;
//! #             };
//! #             write(stream, rest.as_ptr().cast(), rest.len() as u64)
//! #         };
//! #         if taken <= 0 || taken as usize > rest.len() {
//! #             return false;
//! #         }
//! #         rest = &rest[taken as usize..];
//! #     }
//! #     true
//! # }
//! # unsafe extern "C" fn state_load(_: *const clap_plugin, stream: *const clap_istream) -> bool {
//! #     let mut bytes = [0u8; 8];
//! #     let mut filled = 0;
//! #     // A host may give fewer bytes than it is asked for, too.
//! #     while filled < bytes.len() {
//! #         let room = &mut bytes[filled..];
//! #         // SAFETY: a host passes a valid stream, and `room` is writable.
//! #         let given = unsafe {
//! #             let Some(read) = (*stream).read else {
//! #                 return false;
//! #             };
//! #             read(stream, room.as_mut_ptr().cast(), room.len() as u64)
//! #         };
//! #         if given <= 0 || given as usize > room.len() {
//! #             return false;
//! #         }
//! #         filled += given as usize;
//! #     }
//! #     GAIN_BITS.store(f64::from_le_bytes(bytes).to_bits(), Ordering::Relaxed);
//! #     true
//! # }
//! # static STATE: clap_plugin_state = clap_plugin_state {
//! #     save: Some(state_save),
//! #     load: Some(state_load),
//! # };
//! #
//! # unsafe extern "C" fn extension(_plugin: *const clap_plugin, id: *const c_char) -> *const c_void {
//! #     // SAFETY: a host passes a NUL-terminated extension id.
//! #     let id = unsafe { CStr::from_ptr(id) };
//! #     if id == CLAP_EXT_PARAMS {
//! #         (&raw const PARAMS).cast()
//! #     } else if id == CLAP_EXT_AUDIO_PORTS {
//! #         (&raw const AUDIO_PORTS).cast()
//! #     } else if id == CLAP_EXT_STATE {
//! #         (&raw const STATE).cast()
//! #     } else {
//! #         std::ptr::null()
//! #     }
//! # }
//! #
//! # static PLUGIN: clap_plugin = clap_plugin {
//! #     desc: &raw const DESCRIPTOR,
//! #     plugin_data: std::ptr::null_mut(),
//! #     init: Some(agree),
//! #     destroy: Some(nothing),
//! #     activate: Some(activate),
//! #     deactivate: Some(nothing),
//! #     start_processing: Some(agree),
//! #     stop_processing: Some(nothing),
//! #     reset: Some(nothing),
//! #     process: Some(process),
//! #     get_extension: Some(extension),
//! #     on_main_thread: Some(nothing),
//! # };
//! #
//! # unsafe extern "C" fn plugin_count(_factory: *const clap_plugin_factory) -> u32 {
//! #     1
//! # }
//! # unsafe extern "C" fn plugin_descriptor(
//! #     _factory: *const clap_plugin_factory,
//! #     index: u32,
//! # ) -> *const clap_plugin_descriptor {
//! #     if index == 0 {
//! #         &raw const DESCRIPTOR
//! #     } else {
//! #         std::ptr::null()
//! #     }
//! # }
//! # unsafe extern "C" fn create_plugin(
//! #     _factory: *const clap_plugin_factory,
//! #     _host: *const clap_host,
//! #     _id: *const c_char,
//! # ) -> *const clap_plugin {
//! #     &raw const PLUGIN
//! # }
//! # static FACTORY: clap_plugin_factory = clap_plugin_factory {
//! #     get_plugin_count: Some(plugin_count),
//! #     get_plugin_descriptor: Some(plugin_descriptor),
//! #     create_plugin: Some(create_plugin),
//! # };
//! #
//! # unsafe extern "C" fn entry_init(_path: *const c_char) -> bool {
//! #     true
//! # }
//! # unsafe extern "C" fn entry_deinit() {}
//! # unsafe extern "C" fn entry_factory(id: *const c_char) -> *const c_void {
//! #     // SAFETY: a host passes a NUL-terminated factory id.
//! #     if unsafe { CStr::from_ptr(id) } == CLAP_PLUGIN_FACTORY_ID {
//! #         (&raw const FACTORY).cast()
//! #     } else {
//! #         std::ptr::null()
//! #     }
//! # }
//! # static ENTRY: clap_plugin_entry = clap_plugin_entry {
//! #     clap_version: CLAP_VERSION,
//! #     init: Some(entry_init),
//! #     deinit: Some(entry_deinit),
//! #     get_factory: Some(entry_factory),
//! # };
//! #
//! # // --- The host -----------------------------------------------------
//!
//! // SAFETY: `ENTRY` is the entry point of the example plugin, a static
//! // whose functions do what CLAP asks of them.
//! let library = unsafe { ClapLibrary::from_entry(&raw const ENTRY, "built in") }?;
//!
//! // Main thread: create the plugin and see what it is.
//! let mut plugin = library.create("org.example.gain")?;
//! const GAIN: ParamId = ParamId(1);
//! let gain = plugin.param(GAIN).expect("the plugin has a gain");
//! assert_eq!((gain.name.as_str(), gain.min, gain.max), ("Gain", 0.0, 2.0));
//! assert_eq!(plugin.param_value(GAIN), Some(1.0));
//! assert_eq!(plugin.param_to_text(GAIN, 0.5).as_deref(), Some("0.50 x"));
//! assert_eq!(plugin.text_to_param(GAIN, "1.25 x"), Some(1.25));
//!
//! // Activating hands out the audio half.
//! let mut processor = plugin.activate(48_000.0, 64)?;
//!
//! // Main thread: a parameter change. It travels through a queue, and a
//! // value beyond the range the plugin declared is held to that range.
//! plugin.set_param(GAIN, 0.5)?;
//!
//! // Audio thread: the change arrives with the next block.
//! let mut block = AudioBuffer::<f32>::new(2, 64);
//! block.channel_mut(0).fill(1.0);
//! block.channel_mut(1).fill(-1.0);
//! processor.process_with(&mut block, &ProcessContext::empty());
//! assert_eq!(processor.status(), ProcessStatus::Continue);
//! assert!(block.channel(0).iter().all(|&sample| sample == 0.5));
//! assert!(block.channel(1).iter().all(|&sample| sample == -0.5));
//!
//! // Main thread, now and then: let the plugin have its say, and keep
//! // what is worth keeping.
//! for event in plugin.pump() {
//!     assert!(!matches!(event, HostEvent::ProcessFailed(_)), "{event:?}");
//! }
//! assert_eq!(plugin.param_value(GAIN), Some(0.5));
//! let state = plugin.save_state()?;
//!
//! // The audio half goes first; then the plugin can be deactivated.
//! drop(processor);
//! plugin.deactivate()?;
//!
//! // With no audio half, a parameter is handed over there and then.
//! plugin.set_param(GAIN, 7.0)?;
//! assert_eq!(plugin.param_value(GAIN), Some(2.0));
//!
//! // And a saved state brings the plugin back to where it was.
//! plugin.load_state(&state)?;
//! assert_eq!(plugin.param_value(GAIN), Some(0.5));
//! # Ok::<(), manguebyte_host::clap::ClapError>(())
//! ```
//!
//! # Editors
//!
//! A plugin's editor goes inside a window the application owns, named by a
//! [`ParentWindow`](crate::editor::ParentWindow). Everything about it is
//! done on the main thread, through [`ClapPlugin`]:
//! [`open_editor`](ClapPlugin::open_editor) and
//! [`close_editor`](ClapPlugin::close_editor),
//! [`set_editor_size`](ClapPlugin::set_editor_size) when the window
//! changes size, [`set_editor_scale`](ClapPlugin::set_editor_scale) when
//! it moves to a screen of another density.
//!
//! This crate has no window and no event loop, so what the editor asks for
//! comes out of [`ClapPlugin::pump`] as [`HostEvent::Editor`] requests for
//! the application to carry out: a new size for the window, to be shown or
//! hidden, and the news that the editor closed by itself. The plugin's
//! timers are kept here and fire inside `pump`;
//! [`next_editor_timer`](ClapPlugin::next_editor_timer) says when `pump` is
//! next due. File descriptors, which an editor on Linux may want watched,
//! are listed by [`editor_fds`](ClapPlugin::editor_fds) for the
//! application's loop to watch, and passed back with
//! [`editor_fd_ready`](ClapPlugin::editor_fd_ready).
//!
//! The plugin may call the host back from inside any call to its editor,
//! and nothing it asks for is acted on there and then: a resize request
//! made from inside `set_size` waits for the next `pump`, like every other.
//!
//! This example is compiled and not run, since it needs a plugin on disk
//! and a window.
//!
//! ```no_run
//! use manguebyte_host::clap::{ClapError, ClapLibrary, ClapPlugin, HostEvent};
//! use manguebyte_host::editor::{EditorRequest, EditorSize, ParentWindow};
//!
//! /// What the application does with its own window: here, nothing.
//! fn resize_window(_size: EditorSize) {}
//!
//! /// Called on every frame of the application's loop, and by
//! /// `plugin.next_editor_timer()` at the latest.
//! fn on_frame(plugin: &mut ClapPlugin) -> Result<(), ClapError> {
//!     for event in plugin.pump() {
//!         match event {
//!             HostEvent::Editor(EditorRequest::Resize(wanted)) => {
//!                 // The window first, then the editor, which says what it
//!                 // took.
//!                 resize_window(wanted);
//!                 let taken = plugin.set_editor_size(wanted)?;
//!                 if taken != wanted {
//!                     resize_window(taken);
//!                 }
//!             }
//!             HostEvent::Editor(EditorRequest::Closed) => {
//!                 // The editor is gone already; the window can go too.
//!             }
//!             _ => {}
//!         }
//!     }
//!     Ok(())
//! }
//!
//! # fn main() -> Result<(), ClapError> {
//! // SAFETY: the file is a CLAP plugin that this program is willing to run.
//! let library = unsafe { ClapLibrary::load("Mangue Pluck.clap") }?;
//! let mut plugin = library.create("org.manguebyte.pluck")?;
//! // The application's window, made with its own toolkit.
//! let parent = ParentWindow::win32(0x0004_05A2).expect("a window");
//! if plugin.has_editor() {
//!     let size = plugin.open_editor(parent, Some(1.5))?;
//!     resize_window(size);
//! }
//! on_frame(&mut plugin)?;
//! // Before the window goes. Dropping the plugin would close it too.
//! plugin.close_editor();
//! # Ok(())
//! # }
//! ```
//!
//! # What is covered
//!
//! | CLAP feature | Behaviour |
//! |---|---|
//! | Entry point | `init` once per loaded file, `deinit` once when the last user is gone; files and bundle folders |
//! | Factory | Plugins listed with bounded text; created by identifier |
//! | Audio ports | Main input, main output and the first other input as a sidechain; further ports get silence or are discarded. 32-bit float |
//! | Note ports | The first note input, in CLAP's own dialect or as MIDI; a port that reads MIDI as MPE is sent plain MIDI |
//! | Events in | Notes with ids, note expression, parameter values, parameter modulation, MIDI, each on its frame |
//! | Events out | Note end, parameter values and gestures, notes and MIDI, in a buffer allocated beforehand |
//! | Parameters | Listing, values, text both ways, flush; every value held to the declared range, in both directions |
//! | State | Saved and restored through streams, with a size limit |
//! | Latency, tail | Read and reported |
//! | Editor (`gui`) | Embedded in a Win32, Cocoa or X11 window; opened in CLAP's order, sized, scaled (not in Cocoa) and closed. Floating windows are not asked for |
//! | Timers, descriptors | `timer-support` timers fire in [`ClapPlugin::pump`]; `posix-fd-support` descriptors are listed for the application to watch. At most 64 of each |
//! | Host callbacks | `request_restart`, `request_process` and `request_callback`, acted on by [`ClapPlugin::pump`] |
//! | Host extensions | `thread-check`, `log`, `params`, `state`, `latency`, `tail`, `audio-ports`, `note-ports`, `gui`, `timer-support`, `posix-fd-support` |
//!
//! # A plugin is not trusted
//!
//! Loading a plugin runs someone else's code inside this process, and
//! nothing can make that safe: a plugin that writes through a wild pointer
//! takes the host with it. That is why [`ClapLibrary::load`] and
//! [`ClapLibrary::from_entry`] are `unsafe`, and why they are the only
//! functions here that are: whoever calls them vouches that the code
//! behind the entry point keeps to the CLAP specification. What this
//! module does guarantee is that the plugin's *answers* are never taken on
//! faith:
//!
//! - every string is read up to a limit, and need not be valid text;
//! - every count is held to a limit, listed in the constants of this
//!   module, before anything is allocated for it;
//! - every pointer and every function pointer is checked for null before
//!   it is used, and a table that lacks a required function is an error;
//! - numbers that are not numbers are dropped. A parameter value is held
//!   to the range the plugin itself declared for that parameter, on its
//!   way to the plugin and on every way back: [`ClapPlugin::param_value`],
//!   [`ClapPlugin::text_to_param`], [`ClapProcessor::output_events`] and
//!   [`ClapPlugin::pump`]. A value or a gesture the plugin reports for a
//!   parameter it never listed is dropped, and `pump` says so. A note
//!   expression is held to the range CLAP gives it;
//! - a plugin that refuses to load, initialise, activate, start or process
//!   leaves the host running. The failure comes back as a [`ClapError`], or,
//!   on the audio thread, as silence at the output with the reason in
//!   [`ClapProcessor::status`] and in the events from [`ClapPlugin::pump`].
//!
//! # Limits
//!
//! No floating editor windows, no 64-bit audio, no presets, no system
//! exclusive or MIDI 2.0 events. One note port and three audio ports carry signal; a
//! plugin with more is still hosted, with silence on the rest. Events in
//! other vendors' event spaces are ignored.

mod bundle;
mod events;
mod host;
mod library;
mod mailbox;
mod plugin;
mod processor;
mod text;

use std::fmt;
use std::path::PathBuf;

use manguebyte_core::ParamId;

use crate::editor::WindowApi;

pub use clap_sys;
pub use events::{OutputEvent, OutputKind};
pub use library::{ClapLibrary, PluginDescriptor};
pub use plugin::{
    AudioPortInfo, ClapPlugin, HostEvent, LogSeverity, NotePortInfo, ParamFlags, ParamInfo,
    ParamsRescan, Tail,
};
pub use processor::{ClapProcessor, ProcessStatus};

/// Most plugins listed from one library.
pub const MAX_PLUGINS_PER_LIBRARY: usize = 1_024;

/// Most bytes read from any one string a plugin supplies through a
/// pointer: an identifier, a name, a description.
pub const MAX_TEXT_BYTES: usize = 4_096;

/// Most feature words read from one plugin descriptor.
pub const MAX_FEATURES: usize = 64;

/// Most parameters listed from one plugin.
pub const MAX_PARAMS: usize = 65_536;

/// Most audio ports, and most note ports, read in each direction. A plugin
/// that declares more audio ports cannot be activated, because its process
/// call would expect a buffer for every one of them.
pub const MAX_PORTS: usize = 32;

/// Most channels one audio port may have. CLAP itself describes the
/// channels of a port in a 64-bit mask.
pub const MAX_CHANNELS_PER_PORT: usize = 64;

/// Largest block size a plugin can be activated with.
pub const MAX_BLOCK_SIZE: usize = 1 << 20;

/// Most samples of working memory allocated for one activation: every
/// channel of every port times the block size. 64 MiB of `f32`.
pub const MAX_SCRATCH_SAMPLES: usize = 1 << 24;

/// Largest state accepted from a plugin, so that one that never stops
/// writing cannot exhaust memory.
pub const MAX_STATE_BYTES: usize = 64 << 20;

/// Largest latency taken from a plugin, in frames. A larger report is cut
/// to this, since a host allocates delay lines by it.
pub const MAX_LATENCY_FRAMES: u32 = 1 << 24;

/// Most events handed to a plugin in one call of its process function, and
/// most kept of what it reports during one call of
/// [`process_with`](manguebyte_dsp::Processor::process_with).
///
/// The two are not counted over the same stretch. A block longer than the
/// size the plugin was activated with is processed in several calls to the
/// plugin, and the limit on events for the plugin applies to each of those
/// calls. The limit on events from the plugin applies to the caller's
/// whole block, since that is what
/// [`ClapProcessor::output_events`] returns.
///
/// Further events are dropped and counted, never allocated for.
pub const EVENTS_PER_BLOCK: usize = 4_096;

/// Most events that wait for the next block: the ones that fall beyond the
/// end of a block, and the ones of blocks the plugin could not be called
/// for.
pub(crate) const CARRIED_EVENTS: usize = 1_024;

/// How many parameter changes can wait for the audio thread, and how many
/// reports can wait for the main thread.
pub(crate) const QUEUE_CAPACITY: usize = 1_024;

/// What the plugin is told about the program hosting it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostInfo {
    /// The host's name.
    pub name: String,
    /// Who makes it.
    pub vendor: String,
    /// Its web page.
    pub url: String,
    /// Its version.
    pub version: String,
}

impl Default for HostInfo {
    /// The framework itself, at this crate's version.
    fn default() -> Self {
        Self {
            name: "Manguebyte".to_owned(),
            vendor: "Manguebyte".to_owned(),
            url: env!("CARGO_PKG_REPOSITORY").to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

/// Why a block came out silent.
///
/// More reasons may be added, so a `match` on this needs a catch-all arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProcessFailure {
    /// The plugin's process function reported an error, or returned a
    /// status CLAP does not define.
    Error,
    /// The plugin refused to start processing. It is asked again on every
    /// block.
    StartRefused,
    /// The processor was prepared for a sample rate other than the one the
    /// plugin was activated with. Deactivate the plugin and activate it
    /// again at the new rate.
    SampleRateMismatch,
}

impl ProcessFailure {
    const ALL: [Self; 3] = [Self::Error, Self::StartRefused, Self::SampleRateMismatch];

    /// This failure as one bit of a set.
    pub(crate) fn bit(self) -> u32 {
        match self {
            Self::Error => 1 << 0,
            Self::StartRefused => 1 << 1,
            Self::SampleRateMismatch => 1 << 2,
        }
    }

    /// The failures whose bits are set in `bits`.
    pub(crate) fn from_bits(bits: u32) -> impl Iterator<Item = Self> {
        Self::ALL
            .into_iter()
            .filter(move |failure| bits & failure.bit() != 0)
    }
}

impl fmt::Display for ProcessFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Error => "the plugin reported an error while processing",
            Self::StartRefused => "the plugin refused to start processing",
            Self::SampleRateMismatch => {
                "the plugin was activated for a different sample rate than it is asked to run at"
            }
        })
    }
}

/// Why hosting a CLAP plugin did not work.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClapError {
    /// Nothing exists at this path.
    NotFound(PathBuf),
    /// The path is a bundle folder with no executable this host can find
    /// in its `Contents/MacOS`.
    BundleWithoutExecutable(PathBuf),
    /// The operating system could not load the file as a shared library.
    Load {
        /// The file.
        path: PathBuf,
        /// What the operating system said.
        reason: String,
    },
    /// The file is a shared library but exports no `clap_entry`: it is not
    /// a CLAP plugin.
    NoEntryPoint(PathBuf),
    /// The entry point is null, or lacks one of its three functions.
    MalformedEntry,
    /// The library was built for a version of CLAP this host cannot talk
    /// to.
    IncompatibleVersion {
        /// Major version; compatible versions have 1 or more here.
        major: u32,
        /// Minor version.
        minor: u32,
        /// Revision.
        revision: u32,
    },
    /// The library's `init` returned false.
    InitFailed,
    /// The library has no plugin factory.
    NoFactory,
    /// The library lists no plugin with this identifier.
    UnknownPlugin(String),
    /// The factory listed this plugin but returned nothing when asked to
    /// create it.
    CreateFailed(String),
    /// The plugin lacks a function no plugin can be hosted without. The
    /// text names it.
    MalformedPlugin(&'static str),
    /// The plugin's own `init` returned false. It has been destroyed.
    PluginInitFailed,
    /// The plugin does not offer what was asked for, or needs something
    /// this host does not provide. The text says what.
    Unsupported(&'static str),
    /// An argument made no sense. The text says which.
    InvalidArgument(&'static str),
    /// The plugin is already active. Deactivate it first.
    AlreadyActive,
    /// The audio half still exists, so the plugin may be processing. Drop
    /// the [`ClapProcessor`] first.
    ProcessorAlive,
    /// The plugin's `activate` returned false.
    ActivationFailed,
    /// The plugin has no parameter with this id.
    UnknownParam(ParamId),
    /// Too many parameter changes are waiting for the audio thread, which
    /// is not running or cannot keep up. Nothing was changed; try again.
    QueueFull,
    /// The plugin could not save its state.
    StateSaveFailed,
    /// The plugin tried to save more than [`MAX_STATE_BYTES`].
    StateTooLarge,
    /// The plugin refused the state it was given.
    StateLoadFailed,
    /// The plugin has no editor this host can show: it offers no `gui`
    /// extension, or one without a function every editor needs.
    NoEditor,
    /// The plugin's editor cannot be put inside a window of this kind.
    EditorApiUnsupported(WindowApi),
    /// The editor is already open. Close it first.
    EditorAlreadyOpen,
    /// The editor is not open.
    EditorNotOpen,
    /// The plugin's editor refused a call. The text names the call. If the
    /// call was part of opening the editor, it has been destroyed again.
    EditorRefused(&'static str),
}

impl fmt::Display for ClapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(path) => write!(f, "there is nothing at {}", path.display()),
            Self::BundleWithoutExecutable(path) => {
                write!(f, "the bundle {} has no executable", path.display())
            }
            Self::Load { path, reason } => {
                write!(f, "{} could not be loaded: {reason}", path.display())
            }
            Self::NoEntryPoint(path) => {
                write!(
                    f,
                    "{} is not a CLAP plugin: it has no clap_entry",
                    path.display()
                )
            }
            Self::MalformedEntry => f.write_str("the CLAP entry point is incomplete"),
            Self::IncompatibleVersion {
                major,
                minor,
                revision,
            } => write!(
                f,
                "the plugin was built for CLAP {major}.{minor}.{revision}, which is not compatible"
            ),
            Self::InitFailed => f.write_str("the plugin library refused to initialise"),
            Self::NoFactory => f.write_str("the plugin library has no plugin factory"),
            Self::UnknownPlugin(id) => write!(f, "the library has no plugin {id:?}"),
            Self::CreateFailed(id) => write!(f, "the plugin {id:?} could not be created"),
            Self::MalformedPlugin(what) => write!(f, "the plugin has no {what} function"),
            Self::PluginInitFailed => f.write_str("the plugin refused to initialise"),
            Self::Unsupported(what) => write!(f, "not supported: {what}"),
            Self::InvalidArgument(what) => write!(f, "invalid argument: {what}"),
            Self::AlreadyActive => f.write_str("the plugin is already active"),
            Self::ProcessorAlive => {
                f.write_str("the plugin's audio half still exists and may be processing")
            }
            Self::ActivationFailed => f.write_str("the plugin refused to activate"),
            Self::UnknownParam(id) => write!(f, "the plugin has no parameter with id {}", id.0),
            Self::QueueFull => {
                f.write_str("too many parameter changes are waiting for the audio thread")
            }
            Self::StateSaveFailed => f.write_str("the plugin could not save its state"),
            Self::StateTooLarge => {
                f.write_str("the plugin's state is larger than this host accepts")
            }
            Self::StateLoadFailed => f.write_str("the plugin refused the state"),
            Self::NoEditor => f.write_str("the plugin has no editor this host can show"),
            Self::EditorApiUnsupported(api) => {
                write!(f, "the plugin's editor cannot go inside a {api:?} window")
            }
            Self::EditorAlreadyOpen => f.write_str("the plugin's editor is already open"),
            Self::EditorNotOpen => f.write_str("the plugin's editor is not open"),
            Self::EditorRefused(call) => write!(f, "the plugin's editor refused {call}"),
        }
    }
}

impl std::error::Error for ClapError {}

/// What the main thread sends to the audio thread.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ToAudio {
    /// A parameter is to take a value, in the plugin's own scale.
    ParamValue { id: u32, value: f64 },
}

/// What the audio thread sends to the main thread.
#[derive(Debug, Clone, Copy)]
pub(crate) enum FromAudio {
    /// The plugin changed a parameter by itself.
    ParamValue { id: u32, value: f64 },
    /// A control in the plugin's interface was taken hold of.
    GestureBegin(u32),
    /// And let go.
    GestureEnd(u32),
}
