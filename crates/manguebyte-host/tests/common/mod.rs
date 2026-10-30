//! What the hosting tests share: a heap watcher, a way to find the example
//! plugins cargo built, and a loop that drives a hosted plugin exactly as
//! the plugin harness drives the same plugin directly.
#![allow(dead_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::env::consts::{DLL_PREFIX, DLL_SUFFIX};
#[cfg_attr(
    not(any(feature = "clap", feature = "vst3", feature = "lv2")),
    allow(unused_imports)
)]
use std::path::{Path, PathBuf};

use manguebyte_core::AudioBuffer;
use manguebyte_core::ParamId;
use manguebyte_dsp::event::{Event, EventBuffer, EventKind};
use manguebyte_dsp::{ProcessContext, Processor, Transport};
#[cfg(feature = "clap")]
use manguebyte_host::clap::{ClapError, ClapLibrary};
#[cfg(feature = "vst3")]
use manguebyte_host::vst3::{Vst3Error, Vst3Module};
use manguebyte_params::{Param, ParamSet};

// --- Watching the heap ---------------------------------------------------
//
// The audio half of a hosted plugin runs on the audio thread, so the host's
// own code must not allocate or free there. The allocator below counts heap
// calls made by this executable while a thread is being watched. A plugin
// loaded from its own library has its own allocator and is not counted,
// which is the point: these tests are about the host.

thread_local! {
    static WATCHING: Cell<bool> = const { Cell::new(false) };
    static HEAP_CALLS: Cell<usize> = const { Cell::new(0) };
}

struct CountingAllocator;

impl CountingAllocator {
    fn record() {
        // `try_with` because the allocator is still called while a thread's
        // locals are being torn down.
        let _ = WATCHING.try_with(|watching| {
            if watching.get() {
                let _ = HEAP_CALLS.try_with(|calls| calls.set(calls.get() + 1));
            }
        });
    }
}

// SAFETY: every method forwards to the system allocator unchanged; the only
// addition is bumping a thread-local counter, which does not allocate.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::record();
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        Self::record();
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        Self::record();
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Runs `work` and returns how many times this thread used the heap
/// meanwhile.
pub fn heap_calls_during(work: impl FnOnce()) -> usize {
    HEAP_CALLS.with(|calls| calls.set(0));
    WATCHING.with(|watching| watching.set(true));
    work();
    WATCHING.with(|watching| watching.set(false));
    HEAP_CALLS.with(Cell::get)
}

// --- Finding the plugins -------------------------------------------------

/// The shared library cargo built for the example plugin `crate_name`.
///
/// The example plugins are dev-dependencies of this crate. Cargo compiles
/// every crate type of a library in one go, so building the `rlib` these
/// tests link also writes the `cdylib` a host loads, into the same folder
/// as the test executable: `target/<profile>/deps`. For a package in the
/// workspace the file carries no hash: `mangue_pluck.dll`,
/// `libmangue_pluck.so`, `libmangue_pluck.dylib`.
pub fn built_library(crate_name: &str) -> PathBuf {
    let executable = std::env::current_exe().expect("the test executable has a path");
    let folder = executable
        .parent()
        .expect("the test executable is in a folder");
    let file = format!("{DLL_PREFIX}{crate_name}{DLL_SUFFIX}");
    let beside = folder.join(&file);
    if beside.is_file() {
        return beside;
    }

    // Should a toolchain ever add a hash to the name, take the newest.
    let prefix = format!("{DLL_PREFIX}{crate_name}-");
    let hashed = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(&prefix) && name.ends_with(DLL_SUFFIX)
        })
        .max_by_key(|entry| entry.metadata().and_then(|data| data.modified()).ok())
        .map(|entry| entry.path());
    if let Some(hashed) = hashed {
        return hashed;
    }

    // A copy cargo places one level up when the plugin itself was built.
    if let Some(above) = folder.parent().map(|parent| parent.join(&file)) {
        if above.is_file() {
            return above;
        }
    }
    panic!(
        "cargo has not built {file} in {}; the example plugins must be dev-dependencies",
        folder.display()
    );
}

/// Loads a file as a CLAP library, the way the tests may.
///
/// Loading is `unsafe` because it runs the file's code. The tests hand
/// this function three kinds of file, and each meets what
/// `ClapLibrary::load` asks:
///
/// - an example plugin built from this workspace, alone or inside a bundle
///   folder made for the test. Its code is the framework's own CLAP
///   wrapper, which keeps to the CLAP specification;
/// - a path with nothing at it, and files that are not shared libraries:
///   text, nothing at all, the first 16 bytes of a library (fewer than any
///   executable header). The system
///   refuses these before any of their content is run;
/// - a library of the operating system itself, which is sound to load
///   into any process and exports no `clap_entry`.
#[cfg(feature = "clap")]
pub fn load(path: impl AsRef<Path>) -> Result<ClapLibrary, ClapError> {
    // SAFETY: every file the tests pass is of one of the three kinds
    // listed above.
    unsafe { ClapLibrary::load(path) }
}

/// Loads a file or a bundle folder as a VST 3 module, the way the tests
/// may.
///
/// The tests hand this function the same three kinds of file as
/// [`load`], and each meets what `Vst3Module::load` asks: an example plugin
/// built from this workspace, whose `GetPluginFactory` and entry functions
/// are the framework's own VST 3 wrapper's; paths and files the system
/// refuses before running anything; and a library of the operating system,
/// which exports no `GetPluginFactory`.
#[cfg(feature = "vst3")]
pub fn load_vst3(path: impl AsRef<Path>) -> Result<Vst3Module, Vst3Error> {
    // SAFETY: every file the tests pass is of one of the three kinds
    // listed above.
    unsafe { Vst3Module::load(path) }
}

// --- Speaking VST 3's scale ------------------------------------------------
//
// VST 3 moves every parameter as a normalized value between 0 and 1, and
// the framework's VST 3 wrapper turns a point into an event with the plain
// value `normalized_to_plain` gives. It also gives each parameter, as its
// VST 3 id, the parameter's own id with the top bit cleared, since VST 3
// keeps the upper half of the range for hosts. These restate both rules.

/// The VST 3 id the framework's wrapper gives a parameter.
pub fn vst3_id(id: ParamId) -> ParamId {
    ParamId(id.0 & 0x7FFF_FFFF)
}

/// The parameter whose VST 3 id is `id`.
pub fn by_vst3_id(params: &dyn ParamSet, id: ParamId) -> Option<&dyn Param> {
    let mut found = None;
    params.visit(&mut |param| {
        if found.is_none() && vst3_id(param.param_id()) == id {
            found = Some(param.param_id());
        }
    });
    params.find_param(found?)
}

/// Restates an event written for a VST 3 host, with normalized values and
/// VST 3 ids, as the event the harness has to be given for the plugin to
/// receive the same thing.
pub fn for_harness_vst3(params: &dyn ParamSet, kind: EventKind) -> EventKind {
    match kind {
        EventKind::ParamValue { param, value } => {
            let target = by_vst3_id(params, param).expect("a known parameter");
            EventKind::ParamValue {
                param: target.param_id(),
                value: target.normalized_to_plain(value),
            }
        }
        other => other,
    }
}

/// A folder for files a test creates, made empty, and removed when this
/// value is dropped.
///
/// It is inside cargo's target directory, so nothing is written to the
/// repository. Its name carries the process id: two runs of the same tests
/// that share a target directory (two agents, or two CI-like runs on one
/// machine) each get their own folder instead of deleting or overwriting
/// the other's. Removal is best effort, since a library a test still has
/// loaded cannot be deleted on Windows; keep the value alive for as long
/// as the test uses what is in it.
pub struct Scratch {
    path: PathBuf,
}

impl Scratch {
    /// The folder.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl std::ops::Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The path of the scratch folder called `name` for this process.
fn scratch_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()))
}

/// A fresh, empty [`Scratch`] folder called `name`, plus the process id
/// and a number of its own, since two tests of one process run at once
/// and may ask for the same name.
pub fn scratch_folder(name: &str) -> Scratch {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let number = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = scratch_path(&format!("{name}-{number}"));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("the scratch folder can be created");
    Scratch { path }
}

/// A scratch folder for something a `static` keeps for the whole run,
/// which no drop will remove. It is per process, like [`scratch_folder`];
/// folders of the same name that earlier runs left behind are removed
/// first when they are more than an hour old, which no run that may still
/// be using them is.
pub fn lasting_scratch_folder(name: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let prefix = format!("{name}-");
    let hour = std::time::Duration::from_secs(3600);
    for entry in std::fs::read_dir(&root).into_iter().flatten().flatten() {
        let file_name = entry.file_name();
        let Some(rest) = file_name.to_str().and_then(|n| n.strip_prefix(&prefix)) else {
            continue;
        };
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > hour);
        if rest.bytes().all(|b| b.is_ascii_digit()) && old {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    let path = scratch_path(name);
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("the scratch folder can be created");
    path
}

// --- Speaking the plugin's scale -----------------------------------------
//
// A host knows a parameter by the range the plugin declares for it. The
// framework's CLAP wrapper declares a continuous parameter as a position in
// 0..=1 and a stepped one as a whole number of steps. The harness, which
// drives the plugin with no format in between, takes the value in the
// parameter's own unit. These two functions say the same thing in both.

/// The value in the parameter's own unit that the CLAP value stands for.
pub fn plain_from_clap(param: &dyn Param, value: f64) -> f64 {
    match param.step_count().filter(|&steps| steps > 0) {
        Some(steps) => param.normalized_to_plain(value / f64::from(steps)),
        None => param.normalized_to_plain(value),
    }
}

/// The CLAP value that stands for a value in the parameter's own unit.
pub fn clap_from_plain(param: &dyn Param, plain: f64) -> f64 {
    let position = param.plain_to_normalized(plain);
    match param.step_count().filter(|&steps| steps > 0) {
        Some(steps) => (position * f64::from(steps)).round(),
        None => position,
    }
}

/// Restates an event written for a host, with values in the plugin's CLAP
/// scale, as the event the harness has to be given for the plugin to
/// receive the same thing.
///
/// `params` must hold the values the plugin's parameters have when the
/// event arrives, because a modulation amount in the CLAP scale means an
/// offset in the parameter's own unit that depends on where the parameter
/// is. The tests only modulate parameters they do not automate, so a fresh
/// set of parameters will do.
pub fn for_harness(params: &dyn ParamSet, kind: EventKind) -> EventKind {
    match kind {
        EventKind::ParamValue { param, value } => {
            let target = params.find_param(param).expect("a known parameter");
            EventKind::ParamValue {
                param,
                value: plain_from_clap(target, value),
            }
        }
        EventKind::ParamModulation {
            param,
            amount,
            note,
        } => {
            let target = params.find_param(param).expect("a known parameter");
            let plain = target.plain();
            let moved = plain_from_clap(target, clap_from_plain(target, plain) + amount);
            EventKind::ParamModulation {
                param,
                amount: moved - plain,
                note,
            }
        }
        other => other,
    }
}

// --- Driving a processor the way the harness drives a plugin -------------

/// What one render came to.
pub struct Rendered {
    /// Heap calls made by this executable inside the processor.
    pub heap_calls: usize,
    /// How many blocks were processed.
    pub blocks: usize,
}

/// Runs `processor` over the whole of `audio`, in place, in blocks whose
/// lengths cycle through `pattern` and never exceed `max_block`, with
/// `schedule` delivered on its frames and `sidechain` alongside.
///
/// This is `Harness::run` from the plugin crate, restated for any
/// processor: the same block lengths, the same event offsets, the same
/// transport. Whatever differs in the output therefore comes from what is
/// between the caller and the plugin.
pub fn render(
    processor: &mut impl Processor<f32>,
    sample_rate: f64,
    schedule: &[(u64, EventKind)],
    pattern: &[usize],
    max_block: usize,
    audio: &mut AudioBuffer<f32>,
    sidechain: Option<&AudioBuffer<f32>>,
) -> Rendered {
    // The harness keeps events on one frame in the order they were
    // scheduled; a stable sort does the same.
    let mut schedule = schedule.to_vec();
    schedule.sort_by_key(|&(frame, _)| frame);
    let mut due = schedule.iter().peekable();

    let mut events = EventBuffer::with_capacity(4_096);
    let side_channels = sidechain.map_or(0, AudioBuffer::num_channels);
    let mut side = AudioBuffer::<f32>::new(side_channels, max_block);
    let mut transport = Transport::playing_at(120.0);
    let mut rendered = Rendered {
        heap_calls: 0,
        blocks: 0,
    };

    let frames = audio.num_frames();
    let mut done = 0;
    while done < frames {
        let wanted = pattern[rendered.blocks % pattern.len()];
        let length = wanted.min(max_block).min(frames - done);
        let end = (done + length) as u64;

        events.clear();
        while let Some(&&(frame, kind)) = due.peek() {
            if frame >= end {
                break;
            }
            let offset = frame.saturating_sub(done as u64) as u32;
            events
                .push(Event { offset, kind })
                .expect("more events in one block than the test can deliver");
            due.next();
        }
        let mut context = ProcessContext::new(&events, &transport);
        if let Some(whole) = sidechain {
            side.set_num_frames(length);
            for channel in 0..side_channels {
                side.channel_mut(channel)
                    .copy_from_slice(&whole.channel(channel)[done..done + length]);
            }
            context = context.with_sidechain(&side);
        }

        rendered.heap_calls += heap_calls_during(|| {
            audio.windowed(done..done + length, |block| {
                processor.process_with(block, &context);
            });
        });
        rendered.blocks += 1;
        transport.advance(length, sample_rate);
        done += length;
    }
    rendered
}

/// Fails unless the two buffers hold the same samples, bit for bit.
#[track_caller]
pub fn assert_same_bits(actual: &AudioBuffer<f32>, expected: &AudioBuffer<f32>, what: &str) {
    assert_eq!(
        (actual.num_channels(), actual.num_frames()),
        (expected.num_channels(), expected.num_frames()),
        "{what}: the shapes differ"
    );
    for channel in 0..actual.num_channels() {
        let (ours, theirs) = (actual.channel(channel), expected.channel(channel));
        if let Some(frame) = (0..ours.len()).find(|&f| ours[f].to_bits() != theirs[f].to_bits()) {
            let different = (0..ours.len())
                .filter(|&f| ours[f].to_bits() != theirs[f].to_bits())
                .count();
            panic!(
                "{what}: channel {channel} differs on {different} of {} frames, first on frame \
                 {frame}: {} against {}",
                ours.len(),
                ours[frame],
                theirs[frame]
            );
        }
    }
}

// --- LV2 bundles -------------------------------------------------------------

/// An LV2 bundle a test wrote, inside a [`Scratch`] folder that is removed
/// when this value is dropped. It reads as the bundle's own path.
pub struct Lv2Bundle {
    path: PathBuf,
    // Dropped after `path` is no longer needed; removes the folder.
    _scratch: Scratch,
}

impl std::ops::Deref for Lv2Bundle {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for Lv2Bundle {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

/// Writes the LV2 bundle of an example plugin into a fresh folder named
/// `<name>.lv2`: its Turtle files, as the plugin's own `export_lv2!`
/// describes them through `files`, and a copy of the library cargo built
/// for `crate_name`.
#[cfg(feature = "lv2")]
pub fn lv2_bundle(
    crate_name: &str,
    files: fn(&str) -> Vec<(String, String)>,
    name: &str,
) -> Lv2Bundle {
    let library = built_library(crate_name);
    let binary = library.file_name().unwrap().to_str().unwrap().to_owned();
    let scratch = scratch_folder(&format!("lv2-{name}"));
    let folder = scratch.join(format!("{name}.lv2"));
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::copy(&library, folder.join(&binary)).unwrap();
    for (file, text) in files(&binary) {
        std::fs::write(folder.join(file), text).unwrap();
    }
    Lv2Bundle {
        path: folder,
        _scratch: scratch,
    }
}

/// Loads an LV2 bundle a test wrote with [`lv2_bundle`].
///
/// Its library is an example plugin built from this workspace, whose
/// `lv2_descriptor` is the framework's own LV2 wrapper, which keeps to the
/// LV2 specification.
#[cfg(feature = "lv2")]
pub fn load_lv2(path: &Path) -> manguebyte_host::lv2::Lv2Library {
    // SAFETY: see above.
    unsafe { manguebyte_host::lv2::Lv2Library::load(path) }.expect("the bundle loads")
}

/// The event the plugin with no host has to be given for it to receive
/// what the LV2 host delivers: notes and pressure through MIDI 1.0, and a
/// parameter value as the 32-bit float a control port holds.
pub fn as_lv2_delivers(kind: EventKind) -> EventKind {
    use manguebyte_dsp::event::Expression;
    use manguebyte_midi::MidiMessage;
    let byte = |value: f64| (value * 127.0).round().clamp(0.0, 127.0) as u8;
    let message = match kind {
        EventKind::ParamValue { param, value } => {
            return EventKind::ParamValue {
                param,
                value: f64::from(value as f32),
            };
        }
        EventKind::NoteOn { note, velocity } => MidiMessage::NoteOn {
            channel: note.channel,
            note: note.key,
            velocity: byte(f64::from(velocity)).max(1),
        },
        EventKind::NoteOff { note, velocity } => MidiMessage::NoteOff {
            channel: note.channel,
            note: note.key,
            velocity: byte(f64::from(velocity)),
        },
        EventKind::NoteExpression {
            note,
            expression: Expression::Pressure,
            value,
        } => MidiMessage::PolyPressure {
            channel: note.channel,
            note: note.key,
            pressure: byte(value),
        },
        other => return other,
    };
    Event::from_midi(0, message).kind
}

/// A plain value of a parameter, at a position between 0 and 1 of its
/// range, as the 32-bit float the plugin keeps.
pub fn plain_at(params: &dyn ParamSet, id: ParamId, position: f64) -> f64 {
    let param = params.find_param(id).expect("a known parameter");
    f64::from(param.normalized_to_plain(position) as f32)
}

// --- Audio Units -------------------------------------------------------------

/// The factory function of the framework's AU wrapper for `P`, as a unit's
/// bundle exports it on macOS (`ManguebyteAudioUnitFactory`); here it is
/// the plugin linked into the test, so that it is hosted on every system.
#[cfg(feature = "au")]
unsafe extern "C" fn au_factory<P: manguebyte_plugin::Plugin>(
    _description: *const std::ffi::c_void,
) -> *mut std::ffi::c_void {
    manguebyte_au::__private::factory::<P>()
}

/// The components of `P` as its bundle's `Info.plist` lists them, written
/// by the wrapper and read by the host, made in this process through the
/// wrapper's factory, as AudioToolbox makes a version 2 unit.
#[cfg(feature = "au")]
pub fn au_library<P: manguebyte_plugin::Plugin>() -> manguebyte_host::au::AuLibrary {
    let plist = manguebyte_au::info_plist(&P::INFO, P::INFO.name, P::INFO.id, P::INFO.version);
    let components = manguebyte_host::au::read_info_plist(plist.as_bytes())
        .expect("the wrapper's Info.plist lists the unit");
    // SAFETY: the factory is the framework's own AU wrapper around a plugin
    // of this workspace, linked into the test, which keeps to Apple's Audio
    // Unit interface and is callable for as long as the test runs.
    unsafe {
        manguebyte_host::au::AuLibrary::from_factory(
            au_factory::<P>,
            components,
            format!("{}.component", P::INFO.name),
        )
    }
}

/// What an Audio Unit is given for an event: notes and pressure as MIDI
/// 1.0, and a value as the 32-bit float `AudioUnitParameterEvent` holds,
/// in the parameter's own unit, which is what the LV2 host delivers too.
pub fn as_au_delivers(kind: EventKind) -> EventKind {
    as_lv2_delivers(kind)
}
