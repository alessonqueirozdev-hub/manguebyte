//! Drives the LV2 wrapper through the C interface, the way a host does.
//!
//! Nothing here reaches into the wrapper's internals. The test plays the
//! part of a host: it maps URIDs, offers features and options, creates an
//! instance through the descriptor, connects ports in many orders, writes
//! atom sequences into raw memory and calls `run`. If these pass, a real
//! host sees the same behaviour.
//!
//! Every `unsafe` block in this file is a call across that interface with
//! pointers the test owns and keeps alive for as long as the plugin uses
//! them, so the blocks are not commented one by one.
#![allow(clippy::undocumented_unsafe_blocks)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::ffi::{CStr, CString, c_char, c_void};
use std::marker::PhantomData;
use std::ptr;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use manguebyte_core::{AudioBuffer, NoDenormals, ParamId};
use manguebyte_dsp::event::{EventKind, NoteRef};
use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor, Synth, Voice, split_at_events};
use manguebyte_lv2::sys::*;
use manguebyte_lv2::{Descriptor, PortLayout, PortRole, STATE_KEY_URI};
use manguebyte_params::{BoolParam, ChoiceParam, FloatParam, FloatRange, ParamSet, param_set};
use manguebyte_plugin::{AudioLayout, Plugin, PluginInfo, PluginKind};

// --- Watching the heap ---------------------------------------------------

thread_local! {
    static IN_RUN: Cell<bool> = const { Cell::new(false) };
    static HEAP_CALLS: Cell<usize> = const { Cell::new(0) };
}

struct CountingAllocator;

impl CountingAllocator {
    fn record() {
        let _ = IN_RUN.try_with(|watching| {
            if watching.get() {
                let _ = HEAP_CALLS.try_with(|calls| calls.set(calls.get() + 1));
            }
        });
    }
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::record();
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        Self::record();
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        Self::record();
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

// --- The plugins under test ---------------------------------------------

/// The plugins share globals, so the tests must not run at the same time.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn one_at_a_time() -> MutexGuard<'static, ()> {
    ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

struct LevelParams {
    level: FloatParam,
    shape: ChoiceParam,
    explode: BoolParam,
}
param_set!(LevelParams {
    level,
    shape,
    explode
});

const LEVEL: ParamId = ParamId::from_name("level");

static LEVEL_PARAMS: Mutex<Option<Arc<LevelParams>>> = Mutex::new(None);

fn level_params() -> Arc<LevelParams> {
    LEVEL_PARAMS.lock().unwrap().clone().unwrap()
}

/// Parameter value events the last `Level` was given, all blocks together.
static VALUE_EVENTS: AtomicUsize = AtomicUsize::new(0);

/// Half of the smallest normal number, worked out by the processor and not
/// by the compiler. It is subnormal, unless such numbers are being flushed.
fn half_of_the_smallest() -> f32 {
    use std::hint::black_box;
    black_box(black_box(f32::MIN_POSITIVE) * black_box(0.5))
}

/// What `half_of_the_smallest` came to inside the last `Level::process`.
static HALF_INSIDE_PROCESS: AtomicU32 = AtomicU32::new(u32::MAX);

/// Scales its input by `level`, following events alone and never polling.
struct Level {
    params: Arc<LevelParams>,
    current: f32,
}

impl Plugin for Level {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.level",
        name: "Level",
        vendor: "Manguebyte",
        version: "1.2.3",
        description: "Scales the input",
        kind: PluginKind::Effect,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT;
    type Params = LevelParams;

    fn new() -> Self {
        let params = Arc::new(LevelParams {
            level: FloatParam::new("level", "Level", FloatRange::linear(0.0, 4.0), 1.0)
                .with_unit("x"),
            shape: ChoiceParam::new("shape", "Shape", &["Sine", "Saw", "Square"], 1)
                .non_automatable(),
            explode: BoolParam::new("explode", "Explode", false).hidden(),
        });
        *LEVEL_PARAMS.lock().unwrap() = Some(Arc::clone(&params));
        Self {
            current: params.level.value(),
            params,
        }
    }
    fn params(&self) -> Arc<LevelParams> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        assert!(!self.params.explode.value(), "the plugin was told to fail");
        HALF_INSIDE_PROCESS.store(half_of_the_smallest().to_bits(), Ordering::Relaxed);
        split_at_events(
            &mut self.current,
            buffer,
            context.events,
            |current, event| {
                if let EventKind::ParamValue { param, value } = event.kind {
                    VALUE_EVENTS.fetch_add(1, Ordering::Relaxed);
                    if param == LEVEL {
                        *current = value as f32;
                    }
                }
            },
            |current, piece| piece.apply_gain(*current),
        );
    }
    fn latency_samples(&self) -> usize {
        7
    }
}

// The macro is what a real plugin crate uses, so the effect goes through it.
manguebyte_lv2::export_lv2!(Level);

/// The hidden function `export_lv2!` defines for `cargo mangue bundle` gives
/// the same files as `bundle_files`.
#[test]
fn the_exported_bundle_files_are_those_of_bundle_files() {
    // Describing the plugin creates a `Level`, which replaces LEVEL_PARAMS
    // under any test holding a host of its own.
    let _guard = one_at_a_time();
    let files = manguebyte_lv2::bundle_files::<Level>("level.so");
    let exported = __manguebyte_lv2_bundle_files("level.so");
    let expected: Vec<(String, String)> = files
        .files()
        .iter()
        .map(|&(name, text)| (name.to_owned(), text.to_owned()))
        .collect();
    assert_eq!(exported, expected);
    assert_eq!(exported[0].0, "manifest.ttl");
    assert!(exported[0].1.contains("lv2:binary <level.so> ;"));
}

/// The length of each piece `Clock` was asked to process, in order.
static PIECES: [AtomicUsize; 16] = [const { AtomicUsize::new(0) }; 16];
static PIECE_COUNT: AtomicUsize = AtomicUsize::new(0);

fn take_pieces() -> Vec<usize> {
    let count = PIECE_COUNT.swap(0, Ordering::Relaxed).min(PIECES.len());
    PIECES[..count]
        .iter()
        .map(|piece| piece.load(Ordering::Relaxed))
        .collect()
}

struct NoParams {}
param_set!(NoParams {});

/// Writes what the transport says on every frame: the tempo, the frame
/// position, the beat position, the start of the bar, whether it plays, and
/// the time signature as numerator times 100 plus denominator. -1 for
/// anything the transport does not know.
struct Clock {
    params: Arc<NoParams>,
    sample_rate: f64,
}

impl Plugin for Clock {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.clock",
        name: "Clock",
        vendor: "",
        version: "0.1.0",
        description: "",
        kind: PluginKind::Analyzer,
    };
    const LAYOUT: AudioLayout = AudioLayout {
        inputs: 0,
        outputs: 6,
        sidechain: 0,
    };
    type Params = NoParams;

    fn new() -> Self {
        Self {
            params: Arc::new(NoParams {}),
            sample_rate: 0.0,
        }
    }
    fn params(&self) -> Arc<NoParams> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.sample_rate = spec.sample_rate;
    }
    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        let count = PIECE_COUNT.fetch_add(1, Ordering::Relaxed);
        if let Some(slot) = PIECES.get(count) {
            slot.store(buffer.num_frames(), Ordering::Relaxed);
        }
        let transport = context.transport;
        for frame in 0..buffer.num_frames() {
            let moved = if transport.playing { frame as i64 } else { 0 };
            let values = [
                transport.tempo_bpm.map_or(-1.0, |tempo| tempo as f32),
                transport
                    .position_samples
                    .map_or(-1.0, |position| (position + moved) as f32),
                transport
                    .beats_at(frame, self.sample_rate)
                    .map_or(-1.0, |beats| beats as f32),
                transport.bar_start_beats.map_or(-1.0, |beats| beats as f32),
                f32::from(u8::from(transport.playing)),
                transport
                    .time_signature
                    .map_or(-1.0, |(numerator, denominator)| {
                        f32::from(numerator) * 100.0 + f32::from(denominator)
                    }),
            ];
            for (channel, value) in values.into_iter().enumerate() {
                buffer.channel_mut(channel)[frame] = value;
            }
        }
    }
}

/// Outputs its key number while held.
#[derive(Default)]
struct KeyVoice {
    level: f32,
}

impl Voice for KeyVoice {
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn note_on(&mut self, note: NoteRef, _: f32) {
        self.level = f32::from(note.key);
    }
    fn note_off(&mut self, _: f32) {
        self.level = 0.0;
    }
    fn choke(&mut self) {
        self.level = 0.0;
    }
    fn is_active(&self) -> bool {
        self.level > 0.0
    }
    fn render(&mut self, buffer: &mut AudioBuffer<f32>) {
        let level = self.level;
        for channel in buffer.iter_channels_mut() {
            channel.iter_mut().for_each(|out| *out += level);
        }
    }
}

struct KeysParams {
    brightness: FloatParam,
}
param_set!(KeysParams { brightness });

/// Plays key numbers, plus the peak of its sidechain.
struct Keys {
    synth: Synth<KeyVoice>,
    params: Arc<KeysParams>,
}

impl Plugin for Keys {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.keys",
        name: "Keys",
        vendor: "Manguebyte",
        version: "0.1.0",
        description: "Plays key numbers",
        kind: PluginKind::Instrument,
    };
    const LAYOUT: AudioLayout = AudioLayout {
        inputs: 0,
        outputs: 1,
        sidechain: 1,
    };
    type Params = KeysParams;

    fn new() -> Self {
        Self {
            synth: Synth::with_voices(4, KeyVoice::default),
            params: Arc::new(KeysParams {
                brightness: FloatParam::new(
                    "brightness",
                    "Brightness",
                    FloatRange::linear(0.0, 100.0),
                    50.0,
                ),
            }),
        }
    }
    fn params(&self) -> Arc<KeysParams> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.synth.prepare(spec);
    }
    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        self.synth.process_with(buffer, context);
        if let Some(sidechain) = context.sidechain {
            let level = sidechain.peak();
            buffer.channel_mut(0).iter_mut().for_each(|s| *s += level);
        }
    }
    fn reset(&mut self) {
        self.synth.reset();
    }
}

// --- A host ----------------------------------------------------------------

/// The host's URID table: URID `n` is the `n`th URI mapped.
static URIS: Mutex<Vec<CString>> = Mutex::new(Vec::new());

fn urid(uri: &CStr) -> u32 {
    let mut uris = URIS.lock().unwrap_or_else(|poison| poison.into_inner());
    let index = match uris.iter().position(|known| known.as_c_str() == uri) {
        Some(index) => index,
        None => {
            uris.push(uri.to_owned());
            uris.len() - 1
        }
    };
    index as u32 + 1
}

/// The handle the host passes with its map, which `map_uri` checks.
static MAP_HANDLE: u8 = 0;

unsafe extern "C" fn map_uri(handle: *mut c_void, uri: *const c_char) -> u32 {
    assert!(ptr::eq(handle.cast_const().cast::<u8>(), &MAP_HANDLE));
    urid(unsafe { CStr::from_ptr(uri) })
}

unsafe extern "C" fn refuse_uri(_: *mut c_void, _: *const c_char) -> u32 {
    0
}

/// What the plugin logged: the URID of each message's type and its text.
static LOGGED: Mutex<Vec<(u32, String)>> = Mutex::new(Vec::new());

/// The host's `printf`, which the plugin only ever calls with the format
/// `"%s\n"` and one string.
///
/// Rust cannot define a C variadic function, so this one takes the string
/// as a fourth, ordinary argument, and the test hands it to the plugin as
/// the variadic function type. On x86-64, with both the Windows and the
/// System V calling conventions, a variadic call with these four integer
/// and pointer arguments passes them exactly as a call to this function
/// takes them. Elsewhere, and under Miri, which checks
/// that a function is called with its own type, the log test does not run.
unsafe extern "C" fn host_printf(
    _: *mut c_void,
    type_: u32,
    format: *const c_char,
    text: *const c_char,
) -> std::ffi::c_int {
    let format = unsafe { CStr::from_ptr(format) };
    assert_eq!(format, c"%s\n");
    let text = unsafe { CStr::from_ptr(text) }.to_str().unwrap().to_owned();
    LOGGED.lock().unwrap().push((type_, text));
    0
}

type Printf = unsafe extern "C" fn(*mut c_void, u32, *const c_char, ...) -> std::ffi::c_int;

/// What a host offers when it creates an instance.
#[derive(Clone, Copy)]
struct Offer {
    map: bool,
    /// The `buf-size:maxBlockLength` option, if offered.
    max_block: Option<i32>,
    log: bool,
    sample_rate: f64,
}

impl Offer {
    const USUAL: Self = Self {
        map: true,
        max_block: Some(64),
        log: false,
        sample_rate: 48_000.0,
    };
}

/// The features and everything they point at, kept in place while the
/// instance lives.
///
/// The plugin holds pointers into every one of them, so each is a `Kept`:
/// moving a `Box` asserts unique access to what it owns and would
/// invalidate those pointers (Miri reports it), while moving a `Kept`
/// moves only a raw pointer.
struct Features {
    _map: Kept<LV2_URID_Map>,
    _max_block: Kept<i32>,
    _options: Kept<[LV2_Options_Option]>,
    _log: Kept<LV2_Log_Log>,
    _features: Kept<[LV2_Feature]>,
    list: Kept<[*const LV2_Feature]>,
}

/// A heap value owned through a raw pointer, freed on drop.
struct Kept<T: ?Sized>(*mut T);

impl<T: ?Sized> Kept<T> {
    fn new(value: Box<T>) -> Self {
        Self(Box::into_raw(value))
    }

    fn as_ptr(&self) -> *mut T {
        self.0
    }
}

impl<T: ?Sized> Drop for Kept<T> {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `Box::into_raw` and is freed once.
        drop(unsafe { Box::from_raw(self.0) });
    }
}

impl Features {
    fn new(offer: Offer) -> Self {
        let map = Kept::new(Box::new(LV2_URID_Map {
            handle: (&raw const MAP_HANDLE).cast_mut().cast(),
            map: Some(map_uri),
        }));
        let max_block = Kept::new(Box::new(offer.max_block.unwrap_or(0)));
        let mut options = Vec::new();
        if offer.max_block.is_some() {
            options.push(LV2_Options_Option {
                context: LV2_OPTIONS_INSTANCE,
                subject: 0,
                key: urid(LV2_BUF_SIZE__maxBlockLength),
                size: 4,
                type_: urid(LV2_ATOM__Int),
                value: max_block.as_ptr().cast_const().cast(),
            });
        }
        options.push(LV2_Options_Option {
            context: LV2_OPTIONS_INSTANCE,
            subject: 0,
            key: 0,
            size: 0,
            type_: 0,
            value: ptr::null(),
        });
        let options = Kept::new(options.into_boxed_slice());
        let printf: Option<Printf> = if cfg!(all(target_arch = "x86_64", not(miri))) {
            // SAFETY: see `host_printf`.
            Some(unsafe {
                std::mem::transmute::<
                    unsafe extern "C" fn(
                        *mut c_void,
                        u32,
                        *const c_char,
                        *const c_char,
                    ) -> std::ffi::c_int,
                    Printf,
                >(host_printf)
            })
        } else {
            None
        };
        let log = Kept::new(Box::new(LV2_Log_Log {
            handle: ptr::null_mut(),
            printf,
            vprintf: None,
        }));
        let mut features = Vec::new();
        if offer.map {
            features.push(LV2_Feature {
                URI: LV2_URID__map.as_ptr(),
                data: map.as_ptr().cast(),
            });
        }
        features.push(LV2_Feature {
            URI: LV2_OPTIONS__options.as_ptr(),
            data: options.as_ptr().cast(),
        });
        if offer.log {
            features.push(LV2_Feature {
                URI: LV2_LOG__log.as_ptr(),
                data: log.as_ptr().cast(),
            });
        }
        let count = features.len();
        let features = Kept::new(features.into_boxed_slice());
        let first = features.as_ptr().cast::<LV2_Feature>().cast_const();
        // SAFETY: every index is within the `count` features.
        let mut list: Vec<*const LV2_Feature> =
            (0..count).map(|i| unsafe { first.add(i) }).collect();
        list.push(ptr::null());
        Self {
            _map: map,
            _max_block: max_block,
            _options: options,
            _log: log,
            _features: features,
            list: Kept::new(list.into_boxed_slice()),
        }
    }
}

/// An atom sequence being written, with the alignment hosts give it.
#[derive(Clone)]
struct Seq {
    bytes: Vec<u8>,
}

/// A value of a `time:Position` property.
#[derive(Clone, Copy)]
enum Value {
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
}

impl Value {
    fn atom(self) -> (u32, Vec<u8>) {
        match self {
            Self::Int(value) => (urid(LV2_ATOM__Int), value.to_ne_bytes().to_vec()),
            Self::Long(value) => (urid(LV2_ATOM__Long), value.to_ne_bytes().to_vec()),
            Self::Float(value) => (urid(LV2_ATOM__Float), value.to_ne_bytes().to_vec()),
            Self::Double(value) => (urid(LV2_ATOM__Double), value.to_ne_bytes().to_vec()),
        }
    }
}

fn pad(bytes: &mut Vec<u8>) {
    bytes.resize(bytes.len().next_multiple_of(8), 0);
}

impl Seq {
    fn new() -> Self {
        Self::with_unit(urid(LV2_ATOM__frameTime))
    }

    fn with_unit(unit: u32) -> Self {
        let mut bytes = Vec::new();
        bytes.extend(0u32.to_ne_bytes());
        bytes.extend(urid(LV2_ATOM__Sequence).to_ne_bytes());
        bytes.extend(unit.to_ne_bytes());
        bytes.extend(0u32.to_ne_bytes());
        Self { bytes }
    }

    fn event(mut self, frame: i64, type_: u32, body: &[u8]) -> Self {
        self.bytes.extend(frame.to_ne_bytes());
        self.bytes.extend((body.len() as u32).to_ne_bytes());
        self.bytes.extend(type_.to_ne_bytes());
        self.bytes.extend(body);
        pad(&mut self.bytes);
        self
    }

    fn midi(self, frame: i64, message: &[u8]) -> Self {
        self.event(frame, urid(LV2_MIDI__MidiEvent), message)
    }

    fn position(self, frame: i64, properties: &[(&CStr, Value)]) -> Self {
        let mut body = Vec::new();
        body.extend(0u32.to_ne_bytes());
        body.extend(urid(LV2_TIME__Position).to_ne_bytes());
        for &(key, value) in properties {
            let (type_, value) = value.atom();
            body.extend(urid(key).to_ne_bytes());
            body.extend(0u32.to_ne_bytes());
            body.extend((value.len() as u32).to_ne_bytes());
            body.extend(type_.to_ne_bytes());
            body.extend(value);
            pad(&mut body);
        }
        self.event(frame, urid(LV2_ATOM__Object), &body)
    }

    /// The sequence as a host hands it over: in 8-byte aligned memory, with
    /// its header's size filled in.
    fn finish(&self) -> Vec<u64> {
        let mut bytes = self.bytes.clone();
        let size = (bytes.len() - 8) as u32;
        bytes[..4].copy_from_slice(&size.to_ne_bytes());
        aligned(&bytes)
    }
}

fn aligned(bytes: &[u8]) -> Vec<u64> {
    let mut words = vec![0u64; bytes.len().div_ceil(8)];
    for (word, chunk) in words.iter_mut().zip(bytes.chunks(8)) {
        let mut full = [0u8; 8];
        full[..chunk.len()].copy_from_slice(chunk);
        *word = u64::from_ne_bytes(full);
    }
    words
}

/// What one `run` produced.
struct Ran {
    /// Each audio output, in channel order.
    output: Vec<Vec<f32>>,
    heap_calls: usize,
    latency: f32,
    /// The notify port's header and sequence body as four words.
    notify: [u32; 4],
}

/// Properties the host stored when saving: key, value, type and flags.
#[derive(Default)]
struct Store {
    items: Vec<(u32, Vec<u8>, u32, u32)>,
}

unsafe extern "C" fn store_property(
    handle: LV2_State_Handle,
    key: u32,
    value: *const c_void,
    size: usize,
    type_: u32,
    flags: u32,
) -> LV2_State_Status {
    let store = unsafe { &mut *handle.cast::<Store>() };
    let value = unsafe { std::slice::from_raw_parts(value.cast::<u8>(), size) }.to_vec();
    store.items.push((key, value, type_, flags));
    LV2_STATE_SUCCESS
}

unsafe extern "C" fn retrieve_property(
    handle: LV2_State_Handle,
    key: u32,
    size: *mut usize,
    type_: *mut u32,
    flags: *mut u32,
) -> *const c_void {
    let store = unsafe { &*handle.cast::<Store>() };
    match store.items.iter().find(|item| item.0 == key) {
        Some((_, value, value_type, value_flags)) => unsafe {
            *size = value.len();
            *type_ = *value_type;
            *flags = *value_flags;
            value.as_ptr().cast()
        },
        None => ptr::null(),
    }
}

/// One instance of `P`, created and driven as a host does.
struct Host<P: Plugin> {
    descriptor: &'static Descriptor,
    handle: LV2_Handle,
    ports: PortLayout,
    /// The value of each parameter's control port.
    params: Box<[f32]>,
    latency: Box<f32>,
    active: bool,
    /// Changes the order ports are connected in, run after run.
    order: u64,
    /// Whether outputs share the memory of the inputs of the same channel.
    in_place: bool,
    _features: Features,
    _plugin: PhantomData<P>,
}

impl<P: Plugin> Host<P> {
    fn try_new(descriptor: &'static Descriptor, offer: Offer) -> Option<Self> {
        // Counted before the instance exists: `Level` keeps the parameters
        // of the one made last where the test can reach them.
        let count = P::new().params().len();
        let features = Features::new(offer);
        let handle = unsafe {
            descriptor.raw().instantiate.unwrap()(
                descriptor.as_ptr(),
                offer.sample_rate,
                c"/nowhere/".as_ptr(),
                features
                    .list
                    .as_ptr()
                    .cast::<*const LV2_Feature>()
                    .cast_const(),
            )
        };
        if handle.is_null() {
            return None;
        }
        Some(Self {
            descriptor,
            handle,
            ports: PortLayout::new(P::LAYOUT, count),
            params: vec![f32::NAN; count].into_boxed_slice(),
            latency: Box::new(-1.0),
            active: false,
            order: 0,
            in_place: false,
            _features: features,
            _plugin: PhantomData,
        })
    }

    fn new(descriptor: &'static Descriptor, offer: Offer) -> Self {
        let mut host = Self::try_new(descriptor, offer).expect("the plugin refused to start");
        host.activate();
        host.active = true;
        host
    }

    fn lv2(&self) -> &LV2_Descriptor {
        self.descriptor.raw()
    }

    fn activate(&self) {
        unsafe { self.lv2().activate.unwrap()(self.handle) };
    }

    fn deactivate(&mut self) {
        unsafe { self.lv2().deactivate.unwrap()(self.handle) };
        self.active = false;
    }

    fn connect(&self, port: usize, data: *mut c_void) {
        unsafe { self.lv2().connect_port.unwrap()(self.handle, port as u32, data) };
    }

    fn index(&self, role: PortRole) -> usize {
        self.ports.index(role).unwrap()
    }

    /// Runs one block. `inputs` are the main inputs then the sidechain, each
    /// at least `frames` long; missing ones are silent.
    fn run(&mut self, inputs: &[&[f32]], frames: usize, control: &Seq) -> Ran {
        let layout = P::LAYOUT;
        let ins = layout.inputs + layout.sidechain;
        let mut audio: Vec<Vec<f32>> = (0..ins)
            .map(|channel| {
                let mut samples = inputs.get(channel).map_or(vec![], |s| s.to_vec());
                samples.resize(frames, 0.0);
                samples
            })
            .collect();
        // Outputs start out as nonsense, so that a frame left unwritten
        // shows.
        let mut outputs: Vec<Vec<f32>> = vec![vec![99.0; frames]; layout.outputs];
        let mut control = control.finish();
        let mut notify = aligned(&{
            let mut bytes = 120u32.to_ne_bytes().to_vec();
            bytes.resize(128, 0);
            bytes
        });

        // Every port, connected in an order that differs from run to run.
        let mut order: Vec<usize> = (0..self.ports.count()).collect();
        let mut state = self.order.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        for index in (1..order.len()).rev() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            order.swap(index, (state % (index as u64 + 1)) as usize);
        }
        self.order += 1;
        for &port in &order {
            let data: *mut c_void = match self.ports.role(port).unwrap() {
                PortRole::AudioInput(channel) => audio[channel].as_mut_ptr().cast(),
                PortRole::Sidechain(channel) => audio[layout.inputs + channel].as_mut_ptr().cast(),
                PortRole::AudioOutput(channel) => {
                    if self.in_place && channel < layout.inputs {
                        audio[channel].as_mut_ptr().cast()
                    } else {
                        outputs[channel].as_mut_ptr().cast()
                    }
                }
                // The plugins here have no auxiliary buses.
                PortRole::AuxOutput { .. } => ptr::null_mut(),
                PortRole::Control => control.as_mut_ptr().cast(),
                PortRole::Notify => notify.as_mut_ptr().cast(),
                PortRole::Latency => (&raw mut *self.latency).cast(),
                PortRole::Param(index) => (&raw mut self.params[index]).cast(),
            };
            self.connect(port, data);
        }

        HEAP_CALLS.with(|calls| calls.set(0));
        IN_RUN.with(|watching| watching.set(true));
        unsafe { self.lv2().run.unwrap()(self.handle, frames as u32) };
        IN_RUN.with(|watching| watching.set(false));
        let heap_calls = HEAP_CALLS.with(Cell::get);

        if self.in_place {
            for (channel, output) in outputs.iter_mut().enumerate().take(layout.inputs) {
                output.copy_from_slice(&audio[channel]);
            }
        }
        // Nothing may point at this block's memory any more.
        self.disconnect_audio();
        let words: Vec<u32> = notify[..2]
            .iter()
            .flat_map(|word| {
                let bytes = word.to_ne_bytes();
                [
                    u32::from_ne_bytes(bytes[..4].try_into().unwrap()),
                    u32::from_ne_bytes(bytes[4..].try_into().unwrap()),
                ]
            })
            .collect();
        Ran {
            output: outputs,
            heap_calls,
            latency: *self.latency,
            notify: words.try_into().unwrap(),
        }
    }

    fn disconnect_audio(&self) {
        for port in 0..self.ports.count() {
            match self.ports.role(port).unwrap() {
                PortRole::Latency | PortRole::Param(_) => {}
                _ => self.connect(port, ptr::null_mut()),
            }
        }
    }

    fn state(&self) -> &LV2_State_Interface {
        let interface =
            unsafe { self.lv2().extension_data.unwrap()(LV2_STATE__interface.as_ptr()) };
        unsafe { &*interface.cast::<LV2_State_Interface>() }
    }

    fn save(&self) -> (LV2_State_Status, Store) {
        let mut store = Store::default();
        let status = unsafe {
            self.state().save.unwrap()(
                self.handle,
                Some(store_property),
                (&raw mut store).cast(),
                LV2_STATE_IS_POD | LV2_STATE_IS_PORTABLE,
                ptr::null(),
            )
        };
        (status, store)
    }

    fn restore(&self, store: &Store) -> LV2_State_Status {
        unsafe {
            self.state().restore.unwrap()(
                self.handle,
                Some(retrieve_property),
                ptr::from_ref(store).cast_mut().cast(),
                0,
                ptr::null(),
            )
        }
    }
}

impl<P: Plugin> Drop for Host<P> {
    fn drop(&mut self) {
        if self.active {
            unsafe { self.lv2().deactivate.unwrap()(self.handle) };
        }
        unsafe { self.lv2().cleanup.unwrap()(self.handle) };
    }
}

fn level_host() -> Host<Level> {
    let host = Host::<Level>::new(manguebyte_lv2::descriptor!(Level), Offer::USUAL);
    VALUE_EVENTS.store(0, Ordering::Relaxed);
    host
}

fn keys_host(offer: Offer) -> Host<Keys> {
    Host::<Keys>::new(manguebyte_lv2::descriptor!(Keys), offer)
}

fn clock_host(offer: Offer) -> Host<Clock> {
    let host = Host::<Clock>::new(manguebyte_lv2::descriptor!(Clock), offer);
    take_pieces();
    host
}

fn ones(frames: usize) -> Vec<f32> {
    vec![1.0; frames]
}

/// `value` repeated, then `then` repeated, and so on: `steps(&[(1.0, 3), (2.0, 2)])`
/// is `[1, 1, 1, 2, 2]`.
fn steps(parts: &[(f32, usize)]) -> Vec<f32> {
    parts
        .iter()
        .flat_map(|&(value, count)| std::iter::repeat_n(value, count))
        .collect()
}

// --- The tests -----------------------------------------------------------

#[test]
fn the_heap_watcher_itself_works() {
    HEAP_CALLS.with(|calls| calls.set(0));
    IN_RUN.with(|watching| watching.set(true));
    drop(std::hint::black_box(vec![0u8; 64]));
    IN_RUN.with(|watching| watching.set(false));
    assert!(
        HEAP_CALLS.with(Cell::get) >= 2,
        "an allocation went unnoticed"
    );
}

#[test]
fn the_exported_descriptor_names_the_plugin_and_its_state_extension() {
    let _guard = one_at_a_time();
    let exported = unsafe { &*lv2_descriptor(0) };
    assert_eq!(
        unsafe { CStr::from_ptr(exported.URI) },
        c"urn:manguebyte:org.manguebyte.test.level"
    );
    assert!(lv2_descriptor(1).is_null());
    assert!(lv2_descriptor(u32::MAX).is_null());

    let ours = manguebyte_lv2::descriptor!(Level);
    assert_eq!(ours.uri(), c"urn:manguebyte:org.manguebyte.test.level");
    assert_eq!(
        ours.uri().to_str().unwrap(),
        manguebyte_lv2::plugin_uri::<Level>()
    );
    // Every function is there, and the exported one makes a working
    // instance.
    assert!(exported.connect_port.is_some() && exported.activate.is_some());
    assert!(exported.deactivate.is_some() && exported.cleanup.is_some());
    let features = Features::new(Offer::USUAL);
    unsafe {
        let handle = exported.instantiate.unwrap()(
            exported,
            44_100.0,
            c"".as_ptr(),
            features
                .list
                .as_ptr()
                .cast::<*const LV2_Feature>()
                .cast_const(),
        );
        assert!(!handle.is_null());
        exported.activate.unwrap()(handle);
        exported.run.unwrap()(handle, 16);
        exported.deactivate.unwrap()(handle);
        exported.cleanup.unwrap()(handle);
    }

    let extension = exported.extension_data.unwrap();
    unsafe {
        let state = extension(LV2_STATE__interface.as_ptr()).cast::<LV2_State_Interface>();
        assert!(!state.is_null());
        assert!((*state).save.is_some() && (*state).restore.is_some());
        assert!(extension(c"http://lv2plug.in/ns/ext/worker#interface".as_ptr()).is_null());
        assert!(extension(ptr::null()).is_null());
    }
}

#[test]
fn an_instance_needs_a_urid_map_and_a_sample_rate() {
    let _guard = one_at_a_time();
    let descriptor = manguebyte_lv2::descriptor!(Level);
    let instantiate = descriptor.raw().instantiate.unwrap();

    let without_map = Offer {
        map: false,
        ..Offer::USUAL
    };
    assert!(Host::<Level>::try_new(descriptor, without_map).is_none());
    for sample_rate in [0.0, -48_000.0, f64::NAN, f64::INFINITY] {
        let offer = Offer {
            sample_rate,
            ..Offer::USUAL
        };
        assert!(
            Host::<Level>::try_new(descriptor, offer).is_none(),
            "{sample_rate}"
        );
    }
    unsafe {
        // No feature list at all.
        let handle = instantiate(descriptor.as_ptr(), 48_000.0, ptr::null(), ptr::null());
        assert!(handle.is_null());

        // A map that maps nothing, and features with null fields.
        let mut refusing = LV2_URID_Map {
            handle: ptr::null_mut(),
            map: Some(refuse_uri),
        };
        let no_function = LV2_URID_Map {
            handle: ptr::null_mut(),
            map: None,
        };
        let features = [
            LV2_Feature {
                URI: ptr::null(),
                data: ptr::null_mut(),
            },
            LV2_Feature {
                URI: LV2_URID__map.as_ptr(),
                data: ptr::null_mut(),
            },
            LV2_Feature {
                URI: LV2_URID__map.as_ptr(),
                data: (&raw mut refusing).cast(),
            },
        ];
        let list: Vec<*const LV2_Feature> = features
            .iter()
            .map(ptr::from_ref)
            .chain([ptr::null()])
            .collect();
        assert!(instantiate(descriptor.as_ptr(), 48_000.0, ptr::null(), list.as_ptr()).is_null());
        let feature = LV2_Feature {
            URI: LV2_URID__map.as_ptr(),
            data: ptr::from_ref(&no_function).cast_mut().cast(),
        };
        let list = [&raw const feature, ptr::null()];
        assert!(instantiate(descriptor.as_ptr(), 48_000.0, ptr::null(), list.as_ptr()).is_null());
    }
    // Options and log are optional.
    let bare = Offer {
        max_block: None,
        ..Offer::USUAL
    };
    assert!(Host::<Level>::try_new(descriptor, bare).is_some());
}

#[test]
fn ports_connected_in_any_order_carry_audio_and_control_values() {
    let _guard = one_at_a_time();
    let mut host = level_host();
    let input = steps(&[(1.0, 32), (-0.5, 32)]);
    for round in 0..12 {
        let gain = 1.0 + (round % 4) as f32 * 0.5;
        host.params[0] = gain;
        let ran = host.run(&[&input, &input], 64, &Seq::new());
        assert_eq!(ran.heap_calls, 0, "run used the heap");
        let expected: Vec<f32> = input.iter().map(|sample| sample * gain).collect();
        assert_eq!(ran.output, [expected.clone(), expected], "round {round}");
    }
    // The plugin follows events alone: every change of the port reached it
    // as one event, and a port left as it was sent none.
    assert_eq!(VALUE_EVENTS.load(Ordering::Relaxed), 11);
    assert_eq!(level_params().level.value(), 2.5);

    // In place, sharing the host's buffers.
    host.in_place = true;
    host.params[0] = 3.0;
    let ran = host.run(&[&input, &input], 64, &Seq::new());
    let expected: Vec<f32> = input.iter().map(|sample| sample * 3.0).collect();
    assert_eq!(ran.output, [expected.clone(), expected]);
    assert_eq!(ran.heap_calls, 0);
}

#[test]
fn control_values_are_clamped_and_nonsense_is_ignored() {
    let _guard = one_at_a_time();
    let mut host = level_host();
    let input = ones(16);
    let cases = [
        (2.0, 2.0),
        (f32::NAN, 2.0),
        (99.0, 4.0),
        (-5.0, 0.0),
        (f32::INFINITY, 4.0),
        (f32::NEG_INFINITY, 0.0),
        (0.75, 0.75),
    ];
    for (port, heard) in cases {
        host.params[0] = port;
        let ran = host.run(&[&input, &input], 16, &Seq::new());
        assert_eq!(ran.output[0], vec![heard; 16], "port at {port}");
    }
    // A choice port lands on its nearest option.
    host.params[1] = 1.6;
    host.run(&[&input, &input], 16, &Seq::new());
    assert_eq!(level_params().shape.index(), 2);
}

#[test]
fn a_change_made_outside_the_host_is_announced_once_on_frame_0() {
    let _guard = one_at_a_time();
    let mut host = level_host();
    host.params[0] = 1.0;
    host.run(&[&ones(8), &ones(8)], 8, &Seq::new());
    VALUE_EVENTS.store(0, Ordering::Relaxed);

    // A user interface writes straight to the parameter.
    level_params().level.set(3.0);
    let ran = host.run(&[&ones(8), &ones(8)], 8, &Seq::new());
    assert_eq!(ran.output[0], vec![3.0; 8]);
    assert_eq!(ran.heap_calls, 0, "announcing a change used the heap");
    assert_eq!(VALUE_EVENTS.load(Ordering::Relaxed), 1);

    let ran = host.run(&[&ones(8), &ones(8)], 8, &Seq::new());
    assert_eq!(ran.output[0], vec![3.0; 8]);
    assert_eq!(VALUE_EVENTS.load(Ordering::Relaxed), 1, "announced twice");

    // The host's port, unchanged, does not undo the change; moving it does.
    host.params[0] = 0.5;
    let ran = host.run(&[&ones(8), &ones(8)], 8, &Seq::new());
    assert_eq!(ran.output[0], vec![0.5; 8]);
}

#[test]
fn midi_lands_on_its_frame() {
    let _guard = one_at_a_time();
    let mut host = keys_host(Offer::USUAL);
    let notes = Seq::new()
        .midi(10, &[0x90, 60, 100])
        .midi(40, &[0x80, 60, 0])
        .midi(50, &[0x91, 70, 1])
        // A note-on with velocity zero is a note-off.
        .midi(52, &[0x91, 70, 0]);
    let ran = host.run(&[], 64, &notes);
    assert_eq!(ran.heap_calls, 0);
    assert_eq!(
        ran.output[0],
        steps(&[(0.0, 10), (60.0, 30), (0.0, 10), (70.0, 2), (0.0, 12)])
    );

    // The sidechain arrives too.
    let ran = host.run(&[&vec![0.25; 64]], 64, &Seq::new());
    assert_eq!(ran.output[0], vec![0.25; 64]);
}

#[test]
fn mpe_notes_belong_to_their_zone() {
    let _guard = one_at_a_time();
    let mut host = keys_host(Offer::USUAL);
    // A lower zone, configured by the stream, and the same key on two of
    // its member channels: two notes of the zone, which the master
    // channel's "all sound off" stops together. As plain MIDI it would
    // not reach them.
    let notes = Seq::new()
        .midi(0, &[0xB0, 100, 6])
        .midi(0, &[0xB0, 101, 0])
        .midi(0, &[0xB0, 6, 15])
        .midi(10, &[0x91, 60, 100])
        .midi(20, &[0x92, 60, 100])
        .midi(30, &[0xB0, 120, 0]);
    let ran = host.run(&[], 40, &notes);
    assert_eq!(ran.heap_calls, 0);
    assert_eq!(
        ran.output[0],
        steps(&[(0.0, 10), (60.0, 10), (120.0, 10), (0.0, 10)])
    );
}

/// Exactly 2^-15 quarter notes per frame at 48 kHz.
const TEMPO: f64 = 87.890625;
const BEAT: f64 = 1.0 / 32_768.0;

#[test]
fn the_transport_changes_on_its_frame_and_moves_on_between_positions() {
    let _guard = one_at_a_time();
    let mut host = clock_host(Offer::USUAL);
    let playing = Seq::new()
        .position(
            0,
            &[
                (LV2_TIME__speed, Value::Float(1.0)),
                (LV2_TIME__frame, Value::Long(1_000)),
                (LV2_TIME__beatsPerMinute, Value::Float(TEMPO as f32)),
                (LV2_TIME__beatsPerBar, Value::Float(4.0)),
                (LV2_TIME__beatUnit, Value::Int(4)),
                (LV2_TIME__bar, Value::Long(2)),
                (LV2_TIME__barBeat, Value::Float(3.5)),
                (LV2_TIME__beat, Value::Double(11.5)),
            ],
        )
        // The host stops on frame 40, and moves the playhead.
        .position(
            40,
            &[
                (LV2_TIME__speed, Value::Float(0.0)),
                (LV2_TIME__frame, Value::Long(5_000)),
                (LV2_TIME__bar, Value::Long(5)),
                (LV2_TIME__barBeat, Value::Float(0.0)),
                (LV2_TIME__beat, Value::Double(20.0)),
            ],
        );
    let ran = host.run(&[], 64, &playing);
    assert_eq!(ran.heap_calls, 0, "following the transport used the heap");
    assert_eq!(take_pieces(), [40, 24]);
    let at = |frame: usize| (11.5 + frame as f64 * BEAT) as f32;
    assert_eq!(ran.output[0], vec![TEMPO as f32; 64]);
    let frames: Vec<f32> = (0..40)
        .map(|frame| 1_000.0 + frame as f32)
        .chain(std::iter::repeat_n(5_000.0, 24))
        .collect();
    assert_eq!(ran.output[1], frames);
    let beats: Vec<f32> = (0..40)
        .map(at)
        .chain(std::iter::repeat_n(20.0, 24))
        .collect();
    assert_eq!(ran.output[2], beats);
    // Bar 2 of 4/4 starts on beat 8, bar 5 on beat 20.
    assert_eq!(ran.output[3], steps(&[(8.0, 40), (20.0, 24)]));
    assert_eq!(ran.output[4], steps(&[(1.0, 40), (0.0, 24)]));
    assert_eq!(ran.output[5], vec![404.0; 64]);

    // Stopped, nothing moves, and the block is one piece.
    let ran = host.run(&[], 64, &Seq::new());
    assert_eq!(take_pieces(), [64]);
    assert_eq!(ran.output[1], vec![5_000.0; 64]);
    assert_eq!(ran.output[2], vec![20.0; 64]);

    // Playing again just before a bar line, with the position given in bars
    // only: the next block finds itself in the next bar.
    let near_the_bar = Seq::new().position(
        0,
        &[
            (LV2_TIME__speed, Value::Float(1.0)),
            (LV2_TIME__frame, Value::Long(0)),
            (LV2_TIME__bar, Value::Long(0)),
            (LV2_TIME__barBeat, Value::Double(4.0 - 32.0 * BEAT)),
        ],
    );
    let ran = host.run(&[], 64, &near_the_bar);
    assert_eq!(ran.output[2][0], (4.0 - 32.0 * BEAT) as f32);
    assert_eq!(ran.output[3][0], 0.0);
    let ran = host.run(&[], 64, &Seq::new());
    assert_eq!(ran.output[1][0], 64.0);
    assert_eq!(ran.output[2][0], (4.0 + 32.0 * BEAT) as f32);
    assert_eq!(ran.output[3][0], 4.0, "the bar moved on");

    // In 6/8 a beat is an eighth note: 120 eighths a minute is 60 quarters,
    // and bar 1 starts three quarter notes in.
    let six_eight = Seq::new().position(
        0,
        &[
            (LV2_TIME__speed, Value::Float(1.0)),
            (LV2_TIME__beatsPerMinute, Value::Double(120.0)),
            (LV2_TIME__beatsPerBar, Value::Float(6.0)),
            (LV2_TIME__beatUnit, Value::Int(8)),
            (LV2_TIME__bar, Value::Long(1)),
            (LV2_TIME__barBeat, Value::Float(0.0)),
        ],
    );
    let ran = host.run(&[], 16, &six_eight);
    assert_eq!(ran.output[0][0], 60.0);
    assert_eq!(ran.output[2][0], 3.0);
    assert_eq!(ran.output[3][0], 3.0);
    assert_eq!(ran.output[5][0], 608.0);
}

/// After four bars of 3/4 and one of 4/4, bar 5 starts on beat 16, not on
/// 5 × 4 = 20: a host that gives the running beat and the beat in the bar
/// has said where the bar starts, whatever the time signature was before.
#[test]
fn the_bar_start_follows_the_hosts_running_beat_after_a_change_of_metre() {
    let _guard = one_at_a_time();
    let mut host = clock_host(Offer::USUAL);
    let after_three_four = Seq::new().position(
        0,
        &[
            (LV2_TIME__speed, Value::Float(0.0)),
            (LV2_TIME__beatsPerMinute, Value::Float(120.0)),
            (LV2_TIME__beatsPerBar, Value::Float(4.0)),
            (LV2_TIME__beatUnit, Value::Float(4.0)),
            (LV2_TIME__bar, Value::Long(5)),
            (LV2_TIME__barBeat, Value::Float(1.5)),
            (LV2_TIME__beat, Value::Double(17.5)),
        ],
    );
    let ran = host.run(&[], 8, &after_three_four);
    assert_eq!(ran.heap_calls, 0);
    assert_eq!(ran.output[2], vec![17.5; 8], "position in quarter notes");
    assert_eq!(ran.output[3], vec![16.0; 8], "start of the bar");

    // In 6/8 the same rule holds, in quarter notes: beat 30 of eighths with
    // barBeat 2 is quarter 15, in a bar that starts on quarter 14.
    let six_eight = Seq::new().position(
        0,
        &[
            (LV2_TIME__beatsPerBar, Value::Float(6.0)),
            (LV2_TIME__beatUnit, Value::Int(8)),
            (LV2_TIME__bar, Value::Long(3)),
            (LV2_TIME__barBeat, Value::Float(2.0)),
            (LV2_TIME__beat, Value::Double(30.0)),
        ],
    );
    let ran = host.run(&[], 8, &six_eight);
    assert_eq!(ran.output[2], vec![15.0; 8]);
    assert_eq!(ran.output[3], vec![14.0; 8]);
}

/// A finite speed so large that the place overflows after one block: the
/// place is forgotten, and the plugin is never handed an infinite or
/// not-a-number position.
#[test]
fn a_huge_but_finite_speed_never_hands_the_plugin_non_finite_positions() {
    let _guard = one_at_a_time();
    let mut host = clock_host(Offer::USUAL);
    let huge = Seq::new().position(
        0,
        &[
            (LV2_TIME__speed, Value::Double(f64::MAX)),
            (LV2_TIME__frame, Value::Long(0)),
            (LV2_TIME__beatsPerMinute, Value::Float(120.0)),
            (LV2_TIME__beatsPerBar, Value::Float(4.0)),
            (LV2_TIME__beatUnit, Value::Float(4.0)),
            (LV2_TIME__bar, Value::Long(0)),
            (LV2_TIME__barBeat, Value::Float(0.0)),
            (LV2_TIME__beat, Value::Double(0.0)),
        ],
    );
    let ran = host.run(&[], 8, &huge);
    assert_eq!(ran.output[1][0], 0.0);
    assert_eq!(ran.output[3], vec![0.0; 8]);
    let ran = host.run(&[], 8, &Seq::new());
    assert_eq!(ran.heap_calls, 0);
    for (channel, samples) in ran.output.iter().enumerate() {
        assert!(
            samples.iter().all(|sample| sample.is_finite()),
            "channel {channel}: {samples:?}"
        );
    }
    assert_eq!(ran.output[0], vec![120.0; 8], "the tempo is still known");
    for channel in [1, 2, 3] {
        assert_eq!(ran.output[channel], vec![-1.0; 8], "channel {channel}");
    }

    // A tempo or a bar count that only overflows once converted is left
    // unknown too.
    let tempo = Seq::new().position(
        0,
        &[
            (LV2_TIME__speed, Value::Float(0.0)),
            (LV2_TIME__beatsPerMinute, Value::Double(f64::MAX)),
            (LV2_TIME__beatUnit, Value::Float(1.0)),
            (LV2_TIME__beatsPerBar, Value::Double(f64::MAX)),
            (LV2_TIME__bar, Value::Long(i64::MAX)),
            (LV2_TIME__barBeat, Value::Float(0.0)),
        ],
    );
    let ran = host.run(&[], 8, &tempo);
    for channel in [0, 2, 3] {
        assert_eq!(ran.output[channel], vec![-1.0; 8], "channel {channel}");
    }
}

/// A position stamped on frame 100 of a 64-frame block describes frame 36
/// of the next block. It does not change this block, and the next block
/// starts 36 frames before it.
#[test]
fn a_position_stamped_past_the_block_is_moved_back_to_the_next_blocks_start() {
    let _guard = one_at_a_time();
    let mut host = clock_host(Offer::USUAL);
    let late = Seq::new().position(
        100,
        &[
            (LV2_TIME__speed, Value::Float(1.0)),
            (LV2_TIME__frame, Value::Long(1_000)),
            (LV2_TIME__beatsPerMinute, Value::Float(TEMPO as f32)),
            (LV2_TIME__beat, Value::Double(10.0)),
        ],
    );
    let ran = host.run(&[], 64, &late);
    assert_eq!(ran.heap_calls, 0);
    assert_eq!(take_pieces(), [64]);
    assert_eq!(
        ran.output[1],
        vec![-1.0; 64],
        "nothing changed in this block"
    );
    let ran = host.run(&[], 64, &Seq::new());
    assert_eq!(ran.output[1][0], 964.0);
    assert_eq!(ran.output[1][36], 1_000.0);
    assert_eq!(ran.output[2][0], (10.0 - 36.0 * BEAT) as f32);
    assert_eq!(ran.output[2][36], 10.0);

    // A position that only changes the tempo does not move the place.
    let tempo_only = Seq::new().position(
        90,
        &[(LV2_TIME__beatsPerMinute, Value::Float(TEMPO as f32))],
    );
    host.run(&[], 64, &tempo_only);
    let ran = host.run(&[], 64, &Seq::new());
    assert_eq!(ran.output[1][0], 1_092.0);

    // In a block of no frames, every position is past the end.
    let ran = host.run(
        &[],
        0,
        &Seq::new().position(
            10,
            &[
                (LV2_TIME__frame, Value::Long(5_000)),
                (LV2_TIME__beat, Value::Double(10.0)),
            ],
        ),
    );
    assert_eq!(ran.heap_calls, 0);
    let ran = host.run(&[], 64, &Seq::new());
    assert_eq!(ran.output[1][0], 4_990.0);
    assert_eq!(ran.output[2][10], 10.0);

    // Stopped, the place is where the host said, wherever it was stamped.
    let stopped = Seq::new().position(
        100,
        &[
            (LV2_TIME__speed, Value::Float(0.0)),
            (LV2_TIME__frame, Value::Long(7_000)),
        ],
    );
    host.run(&[], 64, &stopped);
    let ran = host.run(&[], 64, &Seq::new());
    assert_eq!(ran.output[1], vec![7_000.0; 64]);
}

#[test]
fn a_host_that_sends_no_position_gets_a_stopped_transport() {
    let _guard = one_at_a_time();
    let mut host = clock_host(Offer::USUAL);
    let ran = host.run(&[], 8, &Seq::new());
    for channel in [0, 1, 2, 3, 5] {
        assert_eq!(ran.output[channel], vec![-1.0; 8]);
    }
    assert_eq!(ran.output[4], vec![0.0; 8]);
}

#[test]
fn a_block_longer_than_the_bound_is_processed_in_pieces_without_losing_a_frame() {
    let _guard = one_at_a_time();
    let mut clock = clock_host(Offer::USUAL);
    let ran = clock.run(&[], 200, &Seq::new());
    assert_eq!(ran.heap_calls, 0);
    assert_eq!(take_pieces(), [64, 64, 64, 8]);
    assert!(ran.output.iter().all(|channel| channel.len() == 200));

    // A position inside a piece splits it again.
    let position = Seq::new().position(
        100,
        &[
            (LV2_TIME__speed, Value::Float(1.0)),
            (LV2_TIME__frame, Value::Long(0)),
        ],
    );
    let ran = clock.run(&[], 200, &position);
    assert_eq!(take_pieces(), [64, 36, 64, 36]);
    let frames: Vec<f32> = std::iter::repeat_n(-1.0, 100)
        .chain((0..100).map(|frame| frame as f32))
        .collect();
    assert_eq!(ran.output[1], frames);

    let mut keys = keys_host(Offer::USUAL);
    let notes = Seq::new()
        .midi(3, &[0x90, 61, 100])
        .midi(70, &[0x80, 61, 0])
        .midi(150, &[0x90, 62, 100])
        .midi(190, &[0x80, 62, 0]);
    let ran = keys.run(&[], 200, &notes);
    assert_eq!(ran.heap_calls, 0);
    assert_eq!(
        ran.output[0],
        steps(&[(0.0, 3), (61.0, 67), (0.0, 80), (62.0, 40), (0.0, 10)])
    );

    // Without a bound from the host, pieces are at most 4096 frames.
    let mut clock = clock_host(Offer {
        max_block: None,
        ..Offer::USUAL
    });
    clock.run(&[], 5_000, &Seq::new());
    assert_eq!(take_pieces(), [4_096, 904]);
    // A bound that makes no sense counts as none.
    let mut clock = clock_host(Offer {
        max_block: Some(-3),
        ..Offer::USUAL
    });
    clock.run(&[], 4_097, &Seq::new());
    assert_eq!(take_pieces(), [4_096, 1]);
}

#[test]
fn latency_is_reported_on_its_port() {
    let _guard = one_at_a_time();
    let mut host = level_host();
    let ran = host.run(&[&ones(4), &ones(4)], 4, &Seq::new());
    assert_eq!(ran.latency, 7.0);
    let ran = host.run(&[], 0, &Seq::new());
    assert_eq!(ran.latency, 7.0, "a block of no frames reports it too");
}

#[test]
fn the_notify_port_is_written_as_an_empty_sequence() {
    let _guard = one_at_a_time();
    let mut host = level_host();
    let ran = host.run(&[&ones(4), &ones(4)], 4, &Seq::new());
    assert_eq!(
        ran.notify,
        [8, urid(LV2_ATOM__Sequence), urid(LV2_ATOM__frameTime), 0]
    );
}

#[test]
fn state_survives_a_round_trip_and_another_plugins_state_is_refused() {
    let _guard = one_at_a_time();
    let mut host = level_host();
    host.params[0] = 2.5;
    host.params[1] = 0.0;
    host.run(&[&ones(8), &ones(8)], 8, &Seq::new());

    let (status, saved) = host.save();
    assert_eq!(status, LV2_STATE_SUCCESS);
    assert_eq!(saved.items.len(), 1);
    let (key, value, type_, flags) = &saved.items[0];
    assert_eq!(*key, urid(&CString::new(STATE_KEY_URI).unwrap()));
    assert_eq!(*type_, urid(LV2_ATOM__String));
    assert_eq!(*flags, LV2_STATE_IS_POD | LV2_STATE_IS_PORTABLE);
    assert_eq!(
        value.as_slice(),
        b"manguebyte-plugin 1 org.manguebyte.test.level\n\
          manguebyte-params 1\nlevel=2.5\nshape=0\nexplode=0\n\0"
    );

    // Something else changes the level; restoring brings it back, and the
    // plugin, which follows events alone, hears the restored value.
    level_params().level.set(0.5);
    host.run(&[&ones(8), &ones(8)], 8, &Seq::new());
    assert_eq!(host.restore(&saved), LV2_STATE_SUCCESS);
    assert_eq!(level_params().level.value(), 2.5);
    let ran = host.run(&[&ones(8), &ones(8)], 8, &Seq::new());
    assert_eq!(ran.output[0], vec![2.5; 8]);
    assert_eq!(ran.heap_calls, 0, "the block after a restore used the heap");

    // Another plugin's state, a wrong type, text that is not state, bytes
    // that are not text and no property at all are refused, and change
    // nothing.
    let text = String::from_utf8(value.clone()).unwrap();
    let state = |bytes: &[u8], type_: u32| Store {
        items: vec![(*key, bytes.to_vec(), type_, *flags)],
    };
    let string = urid(LV2_ATOM__String);
    let foreign = text.replace("test.level", "test.other");
    let refusals = [
        (state(foreign.as_bytes(), string), LV2_STATE_ERR_UNKNOWN),
        (state(value, urid(LV2_ATOM__Int)), LV2_STATE_ERR_BAD_TYPE),
        (state(b"not a state\0", string), LV2_STATE_ERR_UNKNOWN),
        (state(&[0xFF, 0xFE, 0x00], string), LV2_STATE_ERR_UNKNOWN),
        (state(b"", string), LV2_STATE_ERR_UNKNOWN),
        (Store::default(), LV2_STATE_ERR_NO_PROPERTY),
    ];
    level_params().level.set(1.5);
    for (store, status) in refusals {
        assert_eq!(host.restore(&store), status);
        assert_eq!(
            level_params().level.value(),
            1.5,
            "a refused restore changed a value"
        );
    }

    // A host without the functions gets an error, not a crash.
    unsafe {
        let state = host.state();
        let none: LV2_State_Store_Function = None;
        assert_eq!(
            state.save.unwrap()(host.handle, none, ptr::null_mut(), 0, ptr::null()),
            LV2_STATE_ERR_UNKNOWN
        );
        assert_eq!(
            state.restore.unwrap()(host.handle, None, ptr::null_mut(), 0, ptr::null()),
            LV2_STATE_ERR_UNKNOWN
        );
    }
}

#[test]
fn malformed_atoms_are_skipped_and_what_comes_before_them_still_lands() {
    let _guard = one_at_a_time();
    let mut host = keys_host(Offer::USUAL);
    let other = urid(c"http://example.org/other#Thing");

    // A sequence whose header names another type is ignored.
    let mut wrong = Seq::new().midi(0, &[0x90, 60, 100]);
    wrong.bytes[4..8].copy_from_slice(&other.to_ne_bytes());
    let ran = host.run(&[], 16, &wrong);
    assert_eq!(ran.output[0], vec![0.0; 16]);

    // Unknown events, MIDI that is not a channel message, objects of other
    // types and broken positions are skipped. A time stamp before the
    // block counts as its first frame, and one that goes back in time as
    // the frame before it.
    let mut object = Vec::new();
    object.extend(0u32.to_ne_bytes());
    object.extend(other.to_ne_bytes());
    let messy = Seq::new()
        .event(0, other, &[1, 2, 3])
        .midi(-50, &[0x90, 50, 100])
        .midi(2, &[])
        .midi(3, &[0xF0, 0x7E, 0xF7])
        .midi(4, &[0x90])
        .event(5, urid(LV2_ATOM__Object), &object)
        .event(6, urid(LV2_ATOM__Object), &[0, 0, 0])
        .position(
            7,
            &[
                (LV2_TIME__beatsPerMinute, Value::Double(f64::NAN)),
                (LV2_TIME__speed, Value::Double(f64::INFINITY)),
            ],
        )
        .midi(10, &[0x80, 50, 0])
        .midi(8, &[0x90, 51, 100]);
    let ran = host.run(&[], 16, &messy);
    assert_eq!(ran.heap_calls, 0);
    assert_eq!(ran.output[0], steps(&[(50.0, 10), (51.0, 6)]));
    host.run(&[], 16, &Seq::new().midi(0, &[0x80, 51, 0]));

    // An event that claims more bytes than the sequence holds ends it:
    // nothing after it is trusted.
    let mut lying = Seq::new()
        .midi(2, &[0x90, 60, 100])
        .midi(4, &[0x90, 70, 100]);
    let second = 16 + 24;
    lying.bytes[second + 8..second + 12].copy_from_slice(&1_000u32.to_ne_bytes());
    let ran = host.run(&[], 8, &lying);
    assert_eq!(ran.output[0], steps(&[(0.0, 2), (60.0, 6)]));
    host.run(&[], 8, &Seq::new().midi(0, &[0x80, 60, 0]));

    // A sequence in beats has no frames to give: its events land on frame 0.
    let beats = Seq::with_unit(urid(LV2_ATOM__beatTime)).midi(5, &[0x90, 40, 100]);
    let ran = host.run(&[], 8, &beats);
    assert_eq!(ran.output[0], vec![40.0; 8]);
    host.run(&[], 8, &Seq::new().midi(0, &[0x80, 40, 0]));

    // An event stamped past the end of the block is handled after its last
    // frame, so the note sounds from the start of the next block.
    let ran = host.run(&[], 8, &Seq::new().midi(100, &[0x90, 30, 100]));
    assert_eq!(ran.output[0], vec![0.0; 8]);
    let ran = host.run(&[], 8, &Seq::new());
    assert_eq!(ran.output[0], vec![30.0; 8]);
}

#[test]
fn null_ports_unknown_ports_and_running_before_activate_do_no_harm() {
    let _guard = one_at_a_time();
    let descriptor = manguebyte_lv2::descriptor!(Level);
    let lv2 = descriptor.raw();
    let host = Host::<Level>::try_new(descriptor, Offer::USUAL).unwrap();
    let mut out = vec![5.0f32; 8];
    unsafe {
        // Run before activate: silence on what is connected.
        host.connect(
            host.index(PortRole::AudioOutput(0)),
            out.as_mut_ptr().cast(),
        );
        lv2.run.unwrap()(host.handle, 8);
        assert_eq!(out, vec![0.0; 8]);

        host.activate();
        // Nothing but one output connected: the input is silence.
        out.fill(5.0);
        lv2.run.unwrap()(host.handle, 8);
        assert_eq!(out, vec![0.0; 8]);
        // Ports that do not exist are ignored.
        let mut junk = [0u64; 4];
        for port in [host.ports.count() as u32, 999, u32::MAX] {
            lv2.connect_port.unwrap()(host.handle, port, junk.as_mut_ptr().cast());
        }
        // Everything disconnected.
        for port in 0..host.ports.count() {
            host.connect(port, ptr::null_mut());
        }
        lv2.run.unwrap()(host.handle, 8);
        // Null handles.
        lv2.run.unwrap()(ptr::null_mut(), 8);
        lv2.connect_port.unwrap()(ptr::null_mut(), 0, ptr::null_mut());
        lv2.activate.unwrap()(ptr::null_mut());
        lv2.deactivate.unwrap()(ptr::null_mut());
        lv2.cleanup.unwrap()(ptr::null_mut());
        assert_eq!(
            host.state().save.unwrap()(
                ptr::null_mut(),
                Some(store_property),
                ptr::null_mut(),
                0,
                ptr::null()
            ),
            LV2_STATE_ERR_UNKNOWN
        );
        // Deactivated, the plugin is silent again until activated.
        lv2.deactivate.unwrap()(host.handle);
        host.connect(
            host.index(PortRole::AudioOutput(0)),
            out.as_mut_ptr().cast(),
        );
        out.fill(5.0);
        lv2.run.unwrap()(host.handle, 8);
        assert_eq!(out, vec![0.0; 8]);
        host.connect(host.index(PortRole::AudioOutput(0)), ptr::null_mut());
    }
    // Small room in the notify buffer: nothing is written into it.
    let mut host = level_host();
    let index = host.index(PortRole::Notify);
    let mut tiny = [4u32, 0, 0xAAAA_AAAA, 0];
    host.run(&[], 4, &Seq::new());
    host.connect(index, tiny.as_mut_ptr().cast());
    unsafe { lv2.run.unwrap()(host.handle, 0) };
    assert_eq!(tiny, [4, 0, 0xAAAA_AAAA, 0]);
    host.connect(index, ptr::null_mut());
}

#[test]
fn a_block_of_no_frames_stores_control_values_for_the_next_block() {
    let _guard = one_at_a_time();
    let mut host = level_host();
    host.params[0] = 1.0;
    host.run(&[&ones(4), &ones(4)], 4, &Seq::new());
    VALUE_EVENTS.store(0, Ordering::Relaxed);

    host.params[0] = 3.0;
    let ran = host.run(&[], 0, &Seq::new().midi(0, &[0x90, 60, 1]));
    assert_eq!(ran.heap_calls, 0);
    assert_eq!(level_params().level.value(), 3.0);
    let ran = host.run(&[&ones(4), &ones(4)], 4, &Seq::new());
    assert_eq!(ran.output[0], vec![3.0; 4]);
    assert_eq!(VALUE_EVENTS.load(Ordering::Relaxed), 1, "announced once");
}

#[test]
fn numbers_too_small_to_hear_are_zero_while_the_plugin_runs_and_only_then() {
    let _guard = one_at_a_time();
    let mut host = level_host();
    assert!(half_of_the_smallest() > 0.0, "the test thread flushes them");
    host.run(&[&ones(8), &ones(8)], 8, &Seq::new());
    let inside = f32::from_bits(HALF_INSIDE_PROCESS.load(Ordering::Relaxed));
    if NoDenormals::SUPPORTED {
        assert_eq!(inside, 0.0, "the plugin ran with subnormal numbers on");
    }
    assert!(
        half_of_the_smallest() > 0.0,
        "the setting leaked into the host"
    );

    // Also when the plugin fails half-way.
    level_params().explode.set(true);
    host.run(&[&ones(8), &ones(8)], 8, &Seq::new());
    assert!(
        half_of_the_smallest() > 0.0,
        "the setting leaked after a panic"
    );
    level_params().explode.set(false);
}

#[test]
fn a_panic_in_the_plugin_gives_a_silent_block_and_the_next_block_plays() {
    let _guard = one_at_a_time();
    let mut host = level_host();
    host.params[0] = 2.0;
    level_params().explode.set(true);
    let ran = host.run(&[&ones(8), &ones(8)], 8, &Seq::new());
    assert_eq!(ran.output, [vec![0.0; 8], vec![0.0; 8]]);
    // The notify sequence is still valid.
    assert_eq!(ran.notify[0], 8);

    level_params().explode.set(false);
    let ran = host.run(&[&ones(8), &ones(8)], 8, &Seq::new());
    assert_eq!(ran.output, [vec![2.0; 8], vec![2.0; 8]]);
    // Sending every value again after the panic stays off the heap.
    assert_eq!(ran.heap_calls, 0, "the block after a panic used the heap");
}

/// Random calls in random orders, with random atoms, for many rounds.
/// Nothing may crash, every output must be written, and `run` must never
/// touch the heap.
#[test]
fn random_calls_and_random_atoms_never_crash_or_allocate() {
    let _guard = one_at_a_time();
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let midi = urid(LV2_MIDI__MidiEvent);
    let object = urid(LV2_ATOM__Object);
    let keys = [
        LV2_TIME__speed,
        LV2_TIME__frame,
        LV2_TIME__beatsPerMinute,
        LV2_TIME__beatsPerBar,
        LV2_TIME__beatUnit,
        LV2_TIME__bar,
        LV2_TIME__barBeat,
        LV2_TIME__beat,
    ];
    let mut host = keys_host(Offer::USUAL);
    let mut level = level_host();
    let rounds = if cfg!(miri) { 20 } else { 400 };
    for round in 0..rounds {
        let frames = (next() % 300) as usize;
        let mut seq = Seq::new();
        for _ in 0..next() % 12 {
            let frame = (next() % 400) as i64 - 50;
            match next() % 4 {
                0 => {
                    let message = [next() as u8, (next() % 128) as u8, (next() % 128) as u8];
                    seq = seq.event(frame, midi, &message[..(next() % 4) as usize]);
                }
                1 => {
                    let key = keys[(next() % keys.len() as u64) as usize];
                    let value = match next() % 4 {
                        0 => Value::Int(next() as i32),
                        1 => Value::Long(next() as i64),
                        2 => Value::Float(f32::from_bits(next() as u32)),
                        _ => Value::Double(f64::from_bits(next())),
                    };
                    seq = seq.position(frame, &[(key, value)]);
                }
                2 => {
                    let body: Vec<u8> = (0..next() % 40).map(|_| next() as u8).collect();
                    seq = seq.event(frame, object, &body);
                }
                _ => {
                    // Random bytes where an event header should be.
                    let junk: Vec<u8> = (0..(next() % 5) * 8).map(|_| next() as u8).collect();
                    seq.bytes.extend(junk);
                }
            }
        }
        match next() % 10 {
            0 => {
                host.deactivate();
                host.activate();
                host.active = true;
            }
            1 => {
                let (_, saved) = host.save();
                assert_eq!(host.restore(&saved), LV2_STATE_SUCCESS);
            }
            // Out of order: deactivated twice, or activated twice.
            2 => host.deactivate(),
            3 => {
                host.activate();
                host.active = true;
            }
            _ => {}
        }
        host.params[0] = f32::from_bits(next() as u32);
        let sidechain: Vec<f32> = (0..frames)
            .map(|_| (next() % 1000) as f32 / 1000.0)
            .collect();
        let ran = host.run(&[&sidechain], frames, &seq);
        assert_eq!(ran.heap_calls, 0, "round {round}");
        assert!(
            ran.output[0]
                .iter()
                .all(|sample| sample.is_finite() && *sample != 99.0),
            "round {round}: an output frame was not written"
        );

        level.params[(next() % 2) as usize] = f32::from_bits(next() as u32);
        level_params().explode.set(false);
        let ran = level.run(&[&ones(frames), &ones(frames)], frames, &seq);
        assert_eq!(ran.heap_calls, 0, "round {round}");
        assert!(ran.output[1].iter().all(|sample| *sample != 99.0));
    }
}

#[test]
#[cfg(all(target_arch = "x86_64", not(miri)))]
fn the_host_log_hears_why_things_were_refused() {
    let _guard = one_at_a_time();
    LOGGED.lock().unwrap().clear();
    let offer = Offer {
        log: true,
        max_block: None,
        ..Offer::USUAL
    };
    let host = Host::<Level>::new(manguebyte_lv2::descriptor!(Level), offer);
    let note = urid(LV2_LOG__Note);
    let warning = urid(LV2_LOG__Warning);
    assert_eq!(
        *LOGGED.lock().unwrap(),
        [(
            note,
            "the host gave no maximum block length; longer blocks are processed in pieces of \
             4096 frames"
                .to_owned()
        )]
    );
    LOGGED.lock().unwrap().clear();
    let (_, saved) = host.save();
    let mut foreign = Store::default();
    let (key, value, type_, flags) = &saved.items[0];
    let text = String::from_utf8(value.clone())
        .unwrap()
        .replace("test.level", "test.other");
    foreign
        .items
        .push((*key, text.into_bytes(), *type_, *flags));
    assert_eq!(host.restore(&foreign), LV2_STATE_ERR_UNKNOWN);
    assert_eq!(
        *LOGGED.lock().unwrap(),
        [(
            warning,
            "state not restored: this state belongs to the plugin \"org.manguebyte.test.other\""
                .to_owned()
        )]
    );

    let offer = Offer {
        log: true,
        sample_rate: 0.0,
        ..Offer::USUAL
    };
    LOGGED.lock().unwrap().clear();
    assert!(Host::<Level>::try_new(manguebyte_lv2::descriptor!(Level), offer).is_none());
    assert_eq!(
        *LOGGED.lock().unwrap(),
        [(
            urid(LV2_LOG__Error),
            "the host gave no usable sample rate".to_owned()
        )]
    );
}
