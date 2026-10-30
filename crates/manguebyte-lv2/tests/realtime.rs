//! Proves that `run` never touches the heap.
//!
//! The real-time contract says nothing the audio thread runs may allocate
//! or free memory. This test replaces the global allocator with one that
//! counts, plays the part of an LV2 host through the real C descriptor, and
//! fails if any call to `run` allocates, reallocates or frees: with control
//! port changes, MIDI, `time:Position` objects inside the block and past its
//! end, blocks longer than the bound, blocks of no frames, a change made
//! outside the host, a restored state, a run while deactivated, and the
//! block after a panic, when every value is sent again.
//!
//! The one call left out is the block in which the plugin panics: a panic
//! allocates its message and payload before the wrapper catches it.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;
use std::sync::{Arc, Mutex};

use manguebyte_core::{AudioBuffer, ParamId};
use manguebyte_dsp::event::EventKind;
use manguebyte_dsp::{ProcessContext, ProcessSpec};
use manguebyte_lv2::sys::*;
use manguebyte_lv2::{Descriptor, PortLayout, PortRole};
use manguebyte_params::{BoolParam, ChoiceParam, FloatParam, FloatRange, ParamSet, param_set};
use manguebyte_plugin::{AudioLayout, Plugin, PluginInfo, PluginKind};

// --- Watching the heap ---------------------------------------------------

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

/// Heap calls made by `f` on this thread.
fn heap_calls(f: impl FnOnce()) -> usize {
    HEAP_CALLS.with(|calls| calls.set(0));
    WATCHING.with(|watching| watching.set(true));
    f();
    WATCHING.with(|watching| watching.set(false));
    HEAP_CALLS.with(Cell::get)
}

// --- The plugin under test --------------------------------------------------

struct BusyParams {
    level: FloatParam,
    shape: ChoiceParam,
    explode: BoolParam,
}
param_set!(BusyParams {
    level,
    shape,
    explode
});

const LEVEL: ParamId = ParamId::from_name("level");

/// Uses everything a block hands it: events, the sidechain and the
/// transport.
struct Busy {
    params: Arc<BusyParams>,
    level: f32,
    key: f32,
}

impl Plugin for Busy {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.busy",
        name: "Busy",
        vendor: "",
        version: "1.0.0",
        description: "",
        kind: PluginKind::Instrument,
    };
    const LAYOUT: AudioLayout = AudioLayout {
        inputs: 2,
        outputs: 2,
        sidechain: 1,
    };
    type Params = BusyParams;

    fn new() -> Self {
        Self {
            params: Arc::new(BusyParams {
                level: FloatParam::new("level", "Level", FloatRange::linear(0.0, 4.0), 1.0),
                shape: ChoiceParam::new("shape", "Shape", &["Sine", "Saw", "Square"], 1),
                explode: BoolParam::new("explode", "Explode", false),
            }),
            level: 1.0,
            key: 0.0,
        }
    }
    fn params(&self) -> Arc<BusyParams> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        assert!(!self.params.explode.value(), "the plugin was told to fail");
        for event in context.events {
            match event.kind {
                EventKind::ParamValue { param, value } if param == LEVEL => {
                    self.level = value as f32;
                }
                EventKind::NoteOn { note, .. } => self.key = f32::from(note.key),
                EventKind::NoteOff { .. } => self.key = 0.0,
                _ => {}
            }
        }
        let beats = context.transport.position_beats.unwrap_or(0.0) as f32;
        let side = context.sidechain.map_or(0.0, |sidechain| sidechain.peak());
        buffer.apply_gain(self.level);
        for sample in buffer.channel_mut(1).iter_mut() {
            *sample += self.key + side + beats;
        }
    }
    fn latency_samples(&self) -> usize {
        3
    }
}

// --- A host ----------------------------------------------------------------

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

unsafe extern "C" fn map_uri(_: *mut c_void, uri: *const c_char) -> u32 {
    // SAFETY: the plugin passes a C string, as `urid:map` requires.
    urid(unsafe { CStr::from_ptr(uri) })
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
    // SAFETY: `handle` is the `Store` passed to `save`, and `value` points
    // at `size` bytes, as the state extension requires.
    let (store, value) = unsafe {
        (
            &mut *handle.cast::<Store>(),
            std::slice::from_raw_parts(value.cast::<u8>(), size).to_vec(),
        )
    };
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
    // SAFETY: `handle` is the `Store` passed to `restore`.
    let store = unsafe { &*handle.cast::<Store>() };
    match store.items.iter().find(|item| item.0 == key) {
        // SAFETY: the plugin passes places for the three results.
        Some((_, value, value_type, value_flags)) => unsafe {
            *size = value.len();
            *type_ = *value_type;
            *flags = *value_flags;
            value.as_ptr().cast()
        },
        None => ptr::null(),
    }
}

fn pad(bytes: &mut Vec<u8>) {
    bytes.resize(bytes.len().next_multiple_of(8), 0);
}

/// An atom sequence being written.
struct Seq {
    bytes: Vec<u8>,
}

impl Seq {
    fn new() -> Self {
        let mut bytes = Vec::new();
        bytes.extend(0u32.to_ne_bytes());
        bytes.extend(urid(LV2_ATOM__Sequence).to_ne_bytes());
        bytes.extend(urid(LV2_ATOM__frameTime).to_ne_bytes());
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

    /// A `time:Position` giving each `(key, value)` as an `atom:Double`.
    fn position(self, frame: i64, properties: &[(&CStr, f64)]) -> Self {
        let mut body = Vec::new();
        body.extend(0u32.to_ne_bytes());
        body.extend(urid(LV2_TIME__Position).to_ne_bytes());
        for &(key, value) in properties {
            body.extend(urid(key).to_ne_bytes());
            body.extend(0u32.to_ne_bytes());
            body.extend(8u32.to_ne_bytes());
            body.extend(urid(LV2_ATOM__Double).to_ne_bytes());
            body.extend(value.to_ne_bytes());
        }
        self.event(frame, urid(LV2_ATOM__Object), &body)
    }

    /// The sequence in 8-byte aligned memory, with its size filled in.
    fn finish(&self) -> Vec<u64> {
        let mut bytes = self.bytes.clone();
        let size = (bytes.len() - 8) as u32;
        bytes[..4].copy_from_slice(&size.to_ne_bytes());
        let mut words = vec![0u64; bytes.len().div_ceil(8)];
        for (word, chunk) in words.iter_mut().zip(bytes.chunks(8)) {
            let mut full = [0u8; 8];
            full[..chunk.len()].copy_from_slice(chunk);
            *word = u64::from_ne_bytes(full);
        }
        words
    }
}

/// One instance of `Busy`, with the features it was given kept alive.
struct Host {
    descriptor: &'static Descriptor,
    handle: LV2_Handle,
    ports: PortLayout,
    active: bool,
    /// The value of each parameter's control port.
    controls: [f32; 3],
    latency: f32,
    _map: Box<LV2_URID_Map>,
    _max_block: Box<i32>,
    _options: Box<[LV2_Options_Option]>,
    _features: Box<[LV2_Feature]>,
    _list: Box<[*const LV2_Feature]>,
}

impl Host {
    fn new() -> Self {
        let descriptor = manguebyte_lv2::descriptor!(Busy);
        let mut map = Box::new(LV2_URID_Map {
            handle: ptr::null_mut(),
            map: Some(map_uri),
        });
        let mut max_block = Box::new(64i32);
        let mut options = vec![
            LV2_Options_Option {
                context: LV2_OPTIONS_INSTANCE,
                subject: 0,
                key: urid(LV2_BUF_SIZE__maxBlockLength),
                size: 4,
                type_: urid(LV2_ATOM__Int),
                value: (&raw mut *max_block).cast_const().cast(),
            },
            LV2_Options_Option {
                context: LV2_OPTIONS_INSTANCE,
                subject: 0,
                key: 0,
                size: 0,
                type_: 0,
                value: ptr::null(),
            },
        ]
        .into_boxed_slice();
        let features = vec![
            LV2_Feature {
                URI: LV2_URID__map.as_ptr(),
                data: (&raw mut *map).cast(),
            },
            LV2_Feature {
                URI: LV2_OPTIONS__options.as_ptr(),
                data: options.as_mut_ptr().cast(),
            },
        ]
        .into_boxed_slice();
        let mut list: Vec<*const LV2_Feature> = features.iter().map(ptr::from_ref).collect();
        list.push(ptr::null());
        let list = list.into_boxed_slice();
        let lv2 = descriptor.raw();
        // SAFETY: the features and everything they point at live in the
        // host until `cleanup`.
        let handle = unsafe {
            lv2.instantiate.unwrap()(
                descriptor.as_ptr(),
                48_000.0,
                c"/nowhere/".as_ptr(),
                list.as_ptr(),
            )
        };
        assert!(!handle.is_null());
        // SAFETY: a live instance, not yet active.
        unsafe { lv2.activate.unwrap()(handle) };
        let params = Busy::new().params();
        Self {
            descriptor,
            handle,
            ports: PortLayout::new(Busy::LAYOUT, params.len()),
            active: true,
            controls: [f32::NAN; 3],
            latency: -1.0,
            _map: map,
            _max_block: max_block,
            _options: options,
            _features: features,
            _list: list,
        }
    }

    fn lv2(&self) -> &LV2_Descriptor {
        self.descriptor.raw()
    }

    fn state(&self) -> &LV2_State_Interface {
        // SAFETY: the descriptor's own function, given a C string.
        let interface =
            unsafe { self.lv2().extension_data.unwrap()(LV2_STATE__interface.as_ptr()) };
        // SAFETY: the plugin returns its static state interface.
        unsafe { &*interface.cast::<LV2_State_Interface>() }
    }

    fn save(&self) -> Store {
        let mut store = Store::default();
        // SAFETY: a live instance, and a store that lives through the call.
        let status = unsafe {
            self.state().save.unwrap()(
                self.handle,
                Some(store_property),
                (&raw mut store).cast(),
                LV2_STATE_IS_POD | LV2_STATE_IS_PORTABLE,
                ptr::null(),
            )
        };
        assert_eq!(status, LV2_STATE_SUCCESS);
        store
    }

    fn restore(&self, store: &Store) {
        // SAFETY: as in `save`; `restore` only reads the store.
        let status = unsafe {
            self.state().restore.unwrap()(
                self.handle,
                Some(retrieve_property),
                ptr::from_ref(store).cast_mut().cast(),
                0,
                ptr::null(),
            )
        };
        assert_eq!(status, LV2_STATE_SUCCESS);
    }

    fn set_active(&mut self, active: bool) {
        if active == self.active {
            return;
        }
        let lv2 = self.lv2();
        let call = if active { lv2.activate } else { lv2.deactivate };
        // SAFETY: a live instance, activated only while inactive and
        // deactivated only while active.
        unsafe { call.unwrap()(self.handle) };
        self.active = active;
    }

    /// Runs one block, in place when `in_place`, and returns the heap calls
    /// `run` made and the outputs.
    fn run(&mut self, frames: usize, control: &Seq, in_place: bool) -> (usize, Vec<Vec<f32>>) {
        let mut inputs = vec![vec![0.5f32; frames]; 3];
        let mut outputs = vec![vec![99.0f32; frames]; 2];
        let mut control = control.finish();
        let mut notify = vec![0u64; 16];
        notify[0] = u64::from(120u32);
        let connect = self.lv2().connect_port.unwrap();
        for port in 0..self.ports.count() {
            let data: *mut c_void = match self.ports.role(port).unwrap() {
                PortRole::AudioInput(channel) => inputs[channel].as_mut_ptr().cast(),
                PortRole::Sidechain(_) => inputs[2].as_mut_ptr().cast(),
                PortRole::AudioOutput(channel) if in_place => inputs[channel].as_mut_ptr().cast(),
                PortRole::AudioOutput(channel) => outputs[channel].as_mut_ptr().cast(),
                // The plugins here have no auxiliary buses.
                PortRole::AuxOutput { .. } => std::ptr::null_mut(),
                PortRole::Control => control.as_mut_ptr().cast(),
                PortRole::Notify => notify.as_mut_ptr().cast(),
                PortRole::Latency => (&raw mut self.latency).cast(),
                PortRole::Param(index) => (&raw mut self.controls[index]).cast(),
            };
            // SAFETY: every buffer lives until the ports are disconnected
            // below, and holds at least `frames` samples or a whole atom.
            unsafe { connect(self.handle, port as u32, data) };
        }
        let run = self.lv2().run.unwrap();
        let handle = self.handle;
        // SAFETY: every port is connected to memory valid for this block.
        let calls = heap_calls(|| unsafe { run(handle, frames as u32) });
        for port in 0..self.ports.count() {
            // SAFETY: disconnecting is always allowed.
            unsafe { connect(self.handle, port as u32, ptr::null_mut()) };
        }
        if in_place {
            outputs[0].copy_from_slice(&inputs[0]);
            outputs[1].copy_from_slice(&inputs[1]);
        }
        (calls, outputs)
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let lv2 = self.lv2();
        // SAFETY: a live instance, deactivated if active and then freed.
        unsafe {
            if self.active {
                lv2.deactivate.unwrap()(self.handle);
            }
            lv2.cleanup.unwrap()(self.handle);
        }
    }
}

// --- The tests -----------------------------------------------------------

/// Changes the level without its control port, as a restored state does:
/// the next block announces it.
fn change_outside_the_host(host: &Host, level: f32) {
    let mut store = host.save();
    let (_, value, _, _) = &mut store.items[0];
    let text = String::from_utf8(value.clone()).unwrap();
    let start = text.find("level=").unwrap();
    let end = start + text[start..].find('\n').unwrap();
    let text = format!("{}level={level}{}", &text[..start], &text[end..]);
    *value = text.into_bytes();
    host.restore(&store);
}

#[test]
fn the_heap_watcher_itself_works() {
    let calls = heap_calls(|| drop(std::hint::black_box(vec![0u8; 64])));
    assert!(calls >= 2, "an allocation went unnoticed");
}

#[test]
fn every_path_through_run_stays_off_the_heap() {
    let mut host = Host::new();
    let busy = Seq::new()
        .midi(3, &[0x90, 60, 100])
        .position(
            20,
            &[
                (LV2_TIME__speed, 1.0),
                (LV2_TIME__frame, 0.0),
                (LV2_TIME__beatsPerMinute, 120.0),
                (LV2_TIME__beatsPerBar, 4.0),
                (LV2_TIME__beatUnit, 4.0),
                (LV2_TIME__bar, 2.0),
                (LV2_TIME__barBeat, 1.0),
                (LV2_TIME__beat, 9.0),
            ],
        )
        .midi(40, &[0x80, 60, 0])
        .midi(150, &[0x90, 62, 100])
        .position(300, &[(LV2_TIME__frame, 900.0), (LV2_TIME__beat, 4.0)]);

    let mut cases: Vec<(&str, usize)> = Vec::new();
    host.controls = [2.0, 0.0, 0.0];
    cases.push((
        "control ports, MIDI, positions",
        host.run(64, &busy, false).0,
    ));
    host.controls = [3.0, 2.0, 0.0];
    cases.push(("longer than the bound", host.run(200, &busy, false).0));
    cases.push(("in place", host.run(64, &busy, true).0));
    cases.push(("no frames", host.run(0, &busy, false).0));
    change_outside_the_host(&host, 0.25);
    cases.push((
        "after a change outside the host",
        host.run(64, &busy, false).0,
    ));
    host.set_active(false);
    cases.push(("deactivated", host.run(64, &busy, false).0));
    host.set_active(true);
    cases.push(("activated again", host.run(64, &busy, false).0));

    host.controls[2] = 1.0;
    let (_, silent) = host.run(64, &busy, false);
    host.controls[2] = 0.0;
    assert_eq!(
        silent,
        [vec![0.0; 64], vec![0.0; 64]],
        "the panic was caught"
    );
    let (calls, played) = host.run(64, &Seq::new(), false);
    cases.push(("after a panic, sending every value again", calls));
    assert_eq!(
        played[0],
        vec![0.5 * 0.25; 64],
        "the values reached the plugin"
    );

    for (case, calls) in &cases {
        assert_eq!(*calls, 0, "{case}: run used the heap");
    }
    assert_eq!(host.latency, 3.0);
}

/// Random blocks, with random lengths, events, control values, restores
/// and panics.
#[test]
fn random_blocks_stay_off_the_heap() {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
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
    let mut host = Host::new();
    let rounds = if cfg!(miri) { 10 } else { 300 };
    for round in 0..rounds {
        let frames = (next() % 300) as usize;
        let mut seq = Seq::new();
        for _ in 0..next() % 10 {
            let frame = (next() % 400) as i64 - 50;
            if next() % 2 == 0 {
                let message = [0x80 | (next() % 0x30) as u8, (next() % 128) as u8, 100];
                seq = seq.midi(frame, &message);
            } else {
                let key = keys[(next() % keys.len() as u64) as usize];
                let value = match next() % 3 {
                    0 => f64::from_bits(next()),
                    1 => (next() % 1000) as f64 - 100.0,
                    _ => f64::MAX,
                };
                seq = seq.position(frame, &[(key, value)]);
            }
        }
        host.controls[(next() % 2) as usize] = f32::from_bits(next() as u32);
        // The second round always panics, so the block after it is tried.
        match if round == 1 { 3 } else { next() % 12 } {
            0 => change_outside_the_host(&host, (next() % 400) as f32 / 100.0),
            1 => host.set_active(false),
            2 => host.set_active(true),
            3 => {
                host.controls[2] = 1.0;
                host.run(frames, &seq, false);
                host.controls[2] = 0.0;
                continue;
            }
            _ => {}
        }
        let (calls, outputs) = host.run(frames, &seq, next() % 2 == 0);
        assert_eq!(calls, 0, "round {round}: run used the heap");
        assert!(
            outputs.iter().flatten().all(|sample| *sample != 99.0),
            "round {round}: an output frame was not written"
        );
    }
}
