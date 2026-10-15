//! The same headless editor driven end to end through VST 3's binary
//! interface: the controller's `createView`, an `IPlugView` attached to a
//! window handle with a fake `IPlugFrame`, and a fake
//! `IComponentHandler` that receives `beginEdit`, `performEdit` and
//! `endEdit`.
//!
//! Every `unsafe` block in this file is a call across that interface with
//! pointers the test owns and keeps alive for the duration of the call, so
//! the blocks are not commented one by one.
#![allow(clippy::undocumented_unsafe_blocks)]
#![allow(non_snake_case)]
// The bindings give enumerations a different integer type on Windows than
// elsewhere, so a cast needed on one platform is redundant on the other.
#![allow(clippy::unnecessary_cast)]

mod common;

use std::cell::RefCell;
use std::ffi::{CStr, c_char, c_void};
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::Ordering;

use manguebyte_gfx::{Point, Size};
use manguebyte_gui::HeadlessEditor;
use manguebyte_plugin::editor::HostSize;
use manguebyte_ui::Role;
use manguebyte_vst3::vst3::Steinberg::Vst::{
    self as vst, IAudioProcessor, IAudioProcessorTrait, IComponent, IComponentHandler,
    IComponentHandlerTrait, IComponentTrait, IEditController, IEditControllerTrait,
    IParamValueQueue, IParamValueQueueTrait, IParameterChanges, IParameterChangesTrait,
};
use manguebyte_vst3::vst3::Steinberg::{
    IPlugFrame, IPlugFrameTrait, IPlugView, IPlugViewContentScaleSupport,
    IPlugViewContentScaleSupportTrait, IPlugViewTrait, IPluginBaseTrait, IPluginFactory,
    IPluginFactoryTrait, ViewRect, kResultFalse, kResultOk, kResultTrue, tresult,
};
use manguebyte_vst3::vst3::{Class, ComPtr, ComRef, ComWrapper, Interface};
use manguebyte_vst3::{class_id, param_id, plugin_factory};

use common::{GAIN, GainTest, get, knob_text, level, params, watched};

/// Whether the host's content scale is taken on this system: everywhere
/// but on macOS, where sizes are points.
const SCALES: bool = !cfg!(target_os = "macos");

// --- The host's objects --------------------------------------------------------

/// Every call the plugin made to the host's objects, in order.
#[derive(Debug, Clone, Copy, PartialEq)]
enum HostCall {
    Begin(u32),
    Perform(u32, f64),
    End(u32),
    Restart(i32),
    ResizeView(i32, i32),
}

#[derive(Default)]
struct HostState {
    calls: RefCell<Vec<HostCall>>,
    /// Whether the frame grants resize requests.
    refuse_resizes: std::cell::Cell<bool>,
}

impl HostState {
    fn take(&self) -> Vec<HostCall> {
        self.calls.take()
    }
}

struct Handler {
    host: Rc<HostState>,
}

impl Class for Handler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for Handler {
    unsafe fn beginEdit(&self, id: u32) -> tresult {
        self.host.calls.borrow_mut().push(HostCall::Begin(id));
        kResultOk
    }
    unsafe fn performEdit(&self, id: u32, value: f64) -> tresult {
        self.host
            .calls
            .borrow_mut()
            .push(HostCall::Perform(id, value));
        kResultOk
    }
    unsafe fn endEdit(&self, id: u32) -> tresult {
        self.host.calls.borrow_mut().push(HostCall::End(id));
        kResultOk
    }
    unsafe fn restartComponent(&self, flags: i32) -> tresult {
        self.host.calls.borrow_mut().push(HostCall::Restart(flags));
        kResultOk
    }
}

/// The host's frame, without a run loop, as on Windows and macOS. Like a
/// host, it makes its window the size asked for and tells the view before
/// `resizeView` returns, unless told to refuse.
struct Frame {
    host: Rc<HostState>,
}

impl Class for Frame {
    type Interfaces = (IPlugFrame,);
}

impl IPlugFrameTrait for Frame {
    unsafe fn resizeView(&self, view: *mut IPlugView, rect: *mut ViewRect) -> tresult {
        let mut rect = unsafe { *rect };
        let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
        self.host
            .calls
            .borrow_mut()
            .push(HostCall::ResizeView(width, height));
        if self.host.refuse_resizes.get() {
            return kResultFalse;
        }
        unsafe { ComRef::from_raw(view).unwrap().onSize(&mut rect) }
    }
}

// --- A session with the plugin -------------------------------------------------

fn iid<I: Interface>() -> *const c_char {
    I::IID.as_ptr().cast()
}

struct Session {
    component: ComPtr<IComponent>,
    processor: ComPtr<IAudioProcessor>,
    controller: ComPtr<IEditController>,
    host: Rc<HostState>,
    processing: std::cell::Cell<bool>,
}

impl Session {
    fn new() -> Self {
        common::forget_editors();
        let host = Rc::new(HostState::default());
        unsafe {
            let factory =
                ComPtr::<IPluginFactory>::from_raw(plugin_factory::<GainTest>().cast()).unwrap();
            let class = class_id(<GainTest as manguebyte_plugin::Plugin>::INFO.id);
            let mut object = ptr::null_mut();
            assert_eq!(
                factory.createInstance(class.as_ptr(), iid::<IComponent>(), &mut object),
                kResultOk
            );
            let component = ComPtr::<IComponent>::from_raw(object.cast()).unwrap();
            assert_eq!(component.initialize(ptr::null_mut()), kResultOk);
            let controller: ComPtr<IEditController> = component.cast().unwrap();
            let handler = ComWrapper::new(Handler {
                host: Rc::clone(&host),
            });
            let pointer = handler.as_com_ref::<IComponentHandler>().unwrap().as_ptr();
            assert_eq!(controller.setComponentHandler(pointer), kResultOk);
            let processor = component.cast().unwrap();
            let mut setup = vst::ProcessSetup {
                processMode: vst::ProcessModes_::kRealtime as i32,
                symbolicSampleSize: vst::SymbolicSampleSizes_::kSample32 as i32,
                maxSamplesPerBlock: 64,
                sampleRate: 48_000.0,
            };
            let processor: ComPtr<IAudioProcessor> = processor;
            assert_eq!(processor.setupProcessing(&mut setup), kResultOk);
            assert_eq!(component.setActive(1), kResultOk);
            Self {
                component,
                processor,
                controller,
                host,
                processing: std::cell::Cell::new(false),
            }
        }
    }

    fn set_processing(&self, on: bool) {
        unsafe { assert_eq!(self.processor.setProcessing(u8::from(on)), kResultOk) };
        self.processing.set(on);
    }

    /// A view with the host's frame, scaled if the system takes scales.
    fn view(&self, scale: Option<f32>) -> View {
        let view = unsafe {
            ComPtr::<IPlugView>::from_raw(self.controller.createView(c"editor".as_ptr()))
        }
        .expect("a view");
        let frame = ComWrapper::new(Frame {
            host: Rc::clone(&self.host),
        });
        let pointer = frame.as_com_ref::<IPlugFrame>().unwrap().as_ptr();
        unsafe { assert_eq!(view.setFrame(pointer), kResultOk) };
        let view = View { view };
        if let Some(scale) = scale {
            let expected = if SCALES { kResultOk } else { kResultFalse };
            assert_eq!(view.scale(scale), expected);
        }
        view
    }

    /// The controller's value of the gain.
    fn shown(&self) -> f64 {
        unsafe { self.controller.getParamNormalized(param_id(GAIN)) }
    }

    /// One block with these automation points for the gain, after checking
    /// that `process` did not use the heap.
    fn process(&self, points: &[(i32, f64)]) {
        let changes = ComWrapper::new(Changes {
            queues: if points.is_empty() {
                Vec::new()
            } else {
                vec![ComWrapper::new(Queue {
                    id: param_id(GAIN),
                    points: points.to_vec(),
                })]
            },
        });
        let mut output = vec![0.0_f32; 32];
        let mut channels = [output.as_mut_ptr()];
        let mut outputs = [vst::AudioBusBuffers {
            numChannels: 1,
            silenceFlags: 0,
            __field0: vst::AudioBusBuffers__type0 {
                channelBuffers32: channels.as_mut_ptr(),
            },
        }];
        let mut data = vst::ProcessData {
            processMode: vst::ProcessModes_::kRealtime as i32,
            symbolicSampleSize: vst::SymbolicSampleSizes_::kSample32 as i32,
            numSamples: 32,
            numInputs: 0,
            numOutputs: 1,
            inputs: ptr::null_mut(),
            outputs: outputs.as_mut_ptr(),
            inputParameterChanges: changes.as_com_ref::<IParameterChanges>().unwrap().as_ptr(),
            outputParameterChanges: ptr::null_mut(),
            inputEvents: ptr::null_mut(),
            outputEvents: ptr::null_mut(),
            processContext: ptr::null_mut(),
        };
        let (heap, status) = watched(|| unsafe { self.processor.process(&mut data) });
        assert_eq!(status, kResultOk);
        assert_eq!(heap, 0, "process used the heap");
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            self.processor.setProcessing(0);
            self.component.setActive(0);
            self.component.terminate();
        }
        common::forget_editors();
    }
}

struct Queue {
    id: u32,
    points: Vec<(i32, f64)>,
}

impl Class for Queue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for Queue {
    unsafe fn getParameterId(&self) -> u32 {
        self.id
    }
    unsafe fn getPointCount(&self) -> i32 {
        self.points.len() as i32
    }
    unsafe fn getPoint(&self, index: i32, offset: *mut i32, value: *mut f64) -> tresult {
        let Some(&(at, to)) = self.points.get(index as usize) else {
            return kResultFalse;
        };
        unsafe {
            *offset = at;
            *value = to;
        }
        kResultOk
    }
    unsafe fn addPoint(&self, _offset: i32, _value: f64, _index: *mut i32) -> tresult {
        kResultFalse
    }
}

struct Changes {
    queues: Vec<ComWrapper<Queue>>,
}

impl Class for Changes {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for Changes {
    unsafe fn getParameterCount(&self) -> i32 {
        self.queues.len() as i32
    }
    unsafe fn getParameterData(&self, index: i32) -> *mut IParamValueQueue {
        self.queues
            .get(index as usize)
            .and_then(|queue| queue.as_com_ref::<IParamValueQueue>())
            .map_or(ptr::null_mut(), |queue| queue.as_ptr())
    }
    unsafe fn addParameterData(&self, _id: *const u32, _index: *mut i32) -> *mut IParamValueQueue {
        ptr::null_mut()
    }
}

/// The plugin's view, as the host holds it.
struct View {
    view: ComPtr<IPlugView>,
}

/// The window handle the host gives. Headless windows never look at it.
const PARENT: usize = 0x0004_05A2;

impl View {
    /// Attaches the view to a Win32 window, as a host on Windows does, and
    /// returns the editor that opened.
    fn attach(&self) -> HeadlessEditor {
        let kind: &CStr = c"HWND";
        let result = unsafe {
            self.view
                .attached(ptr::without_provenance_mut::<c_void>(PARENT), kind.as_ptr())
        };
        assert_eq!(result, kResultOk);
        common::editor()
    }

    fn remove(&self) -> tresult {
        unsafe { self.view.removed() }
    }

    fn size(&self) -> HostSize {
        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        unsafe { assert_eq!(self.view.getSize(&mut rect), kResultOk) };
        HostSize::new(
            (rect.right - rect.left) as u32,
            (rect.bottom - rect.top) as u32,
        )
    }

    fn on_size(&self, width: i32, height: i32) -> tresult {
        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        unsafe { self.view.onSize(&mut rect) }
    }

    fn check(&self, width: i32, height: i32) -> HostSize {
        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        unsafe { assert_eq!(self.view.checkSizeConstraint(&mut rect), kResultTrue) };
        HostSize::new(rect.right as u32, rect.bottom as u32)
    }

    fn scale(&self, factor: f32) -> tresult {
        let scaling: ComPtr<IPlugViewContentScaleSupport> = self.view.cast().unwrap();
        unsafe { scaling.setContentScaleFactor(factor) }
    }

    fn key_down(&self, code: u32) -> tresult {
        unsafe { self.view.onKeyDown(0, code as i16, 0) }
    }
}

fn knob_center(editor: &HeadlessEditor) -> Point {
    get(editor, Role::Slider, "Gain").bounds.center()
}

fn drag(editor: &HeadlessEditor, by: &[f32]) {
    let at = knob_center(editor);
    editor.press(at);
    for &step in by {
        editor.move_to(Point::new(at.x, at.y - step));
    }
    editor.release(Point::new(at.x, at.y - by.last().copied().unwrap_or(0.0)));
}

/// Asserts that the edits are these, with values compared as `f32`, which
/// is what parameters store.
fn assert_edits(got: &[HostCall], expected: &[HostCall]) {
    let close = |a: &HostCall, b: &HostCall| match (a, b) {
        (HostCall::Perform(p, x), HostCall::Perform(q, y)) => p == q && (x - y).abs() < 1e-6,
        _ => a == b,
    };
    assert!(
        got.len() == expected.len() && got.iter().zip(expected).all(|(a, b)| close(a, b)),
        "the host received {got:?}, not {expected:?}"
    );
}

#[test]
fn a_drag_on_the_knob_reaches_the_component_handler() {
    let session = Session::new();
    let view = session.view(None);
    let editor = view.attach();
    assert_eq!(editor.tick(), 1);
    assert_eq!(knob_text(&editor).as_deref(), Some("0.50"));
    session.host.take();

    // Begin, every value the control set, end; no block runs, so the
    // value is stored at once.
    drag(&editor, &[10.0, 20.0]);
    let id = param_id(GAIN);
    assert_edits(
        &session.host.take(),
        &[
            HostCall::Begin(id),
            HostCall::Perform(id, 0.55),
            HostCall::Perform(id, 0.6),
            HostCall::End(id),
        ],
    );
    assert!((session.shown() - 0.6).abs() < 1e-6);
    assert!((params().gain.value() - 0.6).abs() < 1e-6);
    editor.tick();
    assert_eq!(knob_text(&editor).as_deref(), Some("0.60"));
    assert_eq!(view.remove(), kResultOk);
    assert!(!editor.is_open());
}

#[test]
fn while_blocks_run_the_knob_shows_its_edit_until_the_host_echoes_it() {
    let session = Session::new();
    session.set_processing(true);
    session.process(&[]);
    let view = session.view(None);
    let editor = view.attach();
    editor.tick();
    session.host.take();

    drag(&editor, &[20.0]);
    let id = param_id(GAIN);
    assert_edits(
        &session.host.take(),
        &[
            HostCall::Begin(id),
            HostCall::Perform(id, 0.6),
            HostCall::End(id),
        ],
    );
    // The host's points stay the only source of the value, but the knob
    // shows the value it sent.
    assert_eq!(params().gain.value(), 0.5);
    session.process(&[]);
    editor.tick();
    assert_eq!(knob_text(&editor).as_deref(), Some("0.60"));
    // The echo.
    session.process(&[(0, 0.6)]);
    assert!((params().gain.value() - 0.6).abs() < 1e-6);
    editor.tick();
    assert_eq!(knob_text(&editor).as_deref(), Some("0.60"));
    assert_eq!(session.host.take(), []);
    assert_eq!(view.remove(), kResultOk);
}

#[test]
fn host_automation_moves_the_knob_without_an_edit() {
    let session = Session::new();
    session.set_processing(true);
    let view = session.view(None);
    let editor = view.attach();
    editor.tick();
    session.host.take();
    session.process(&[(0, 0.8)]);
    editor.tick();
    assert_eq!(knob_text(&editor).as_deref(), Some("0.80"));
    // The meter shows the level of the block at 0.8: -1.9 dB.
    assert!((level().load(Ordering::Relaxed) + 1.938).abs() < 1e-3);
    assert_eq!(
        get(&editor, Role::Meter, "Output").node.value.as_deref(),
        Some("-1.9 dB")
    );
    for _ in 0..4 {
        session.process(&[]);
        editor.tick();
    }
    assert_eq!(session.host.take(), [], "automation was reported back");
    assert_eq!(view.remove(), kResultOk);
}

#[test]
fn keys_the_host_delivers_reach_the_knob() {
    use manguebyte_vst3::vst3::Steinberg::VirtualKeyCodes_ as codes;

    let session = Session::new();
    let view = session.view(None);
    let editor = view.attach();
    editor.tick();
    session.host.take();
    // Nothing has the focus: the key goes back to the host.
    assert_eq!(view.key_down(codes::KEY_UP as u32), kResultFalse);
    assert_eq!(view.key_down(codes::KEY_TAB as u32), kResultTrue);
    assert_eq!(view.key_down(codes::KEY_UP as u32), kResultTrue);
    let id = param_id(GAIN);
    assert_edits(
        &session.host.take(),
        &[
            HostCall::Begin(id),
            HostCall::Perform(id, 0.51),
            HostCall::End(id),
        ],
    );
    editor.tick();
    assert_eq!(knob_text(&editor).as_deref(), Some("0.51"));
    let knob = get(&editor, Role::Slider, "Gain");
    assert!(knob.node.state.focused);
    assert_eq!(view.remove(), kResultOk);
}

#[test]
fn the_view_follows_the_hosts_scale_and_sizes() {
    let session = Session::new();
    let view = session.view(Some(2.0));
    let scale = if SCALES { 2.0 } else { 1.0 };
    let opening = common::BASE.scale_by(scale);
    assert_eq!(view.size(), opening);
    let editor = view.attach();
    editor.tick();
    assert_eq!(view.size(), opening);
    assert_eq!(
        editor.layout(),
        Some((Size::new(240.0, 180.0), scale as f32))
    );
    session.host.take();

    // The rules at the scale in use.
    let max = HostSize::new(480, 360).scale_by(scale);
    assert_eq!(view.check(100_000, 100_000), max);
    // The host makes its window another size the rules allow.
    let wanted = view.check(400, 300);
    assert_eq!(
        view.on_size(wanted.width as i32, wanted.height as i32),
        kResultOk
    );
    assert_eq!(view.size(), wanted);
    editor.tick();
    assert_eq!(
        editor.size().map(|size| (size.width, size.height)),
        Some((wanted.width, wanted.height))
    );

    if SCALES {
        // A new scale while attached: the editor asks the frame for its
        // size there, and the frame grants it from inside the request.
        assert_eq!(view.scale(1.0), kResultOk);
        assert_eq!(session.host.take(), [HostCall::ResizeView(200, 150)]);
        assert_eq!(view.size(), HostSize::new(200, 150));
        editor.tick();
        assert_eq!(editor.layout(), Some((Size::new(200.0, 150.0), 1.0)));
        // A frame that refuses: the window keeps its size.
        session.host.refuse_resizes.set(true);
        assert_eq!(view.scale(1.25), kResultOk);
        assert_eq!(session.host.take(), [HostCall::ResizeView(250, 188)]);
        assert_eq!(view.size(), HostSize::new(200, 150));
        editor.tick();
        assert_eq!(editor.layout(), Some((Size::new(160.0, 120.0), 1.25)));
    }
    assert_eq!(view.remove(), kResultOk);
}

/// Views attached and removed a thousand times while blocks run, with a
/// drag now and then, and one removed in the middle of a drag: `process`
/// never uses the heap, and every edit the handler sees is whole.
#[test]
fn a_thousand_views_attach_and_remove_while_blocks_run() {
    let session = Session::new();
    session.set_processing(true);
    let id = param_id(GAIN);
    let mut edits = Vec::new();
    for round in 0..1_000 {
        let view = session.view(Some([1.0, 1.5, 2.0][round % 3]));
        let editor = view.attach();
        session.process(&[]);
        if round % 100 == 0 {
            editor.tick();
            drag(&editor, &[1.0, 2.0]);
        }
        if round % 250 == 3 {
            // Removed while the knob is held.
            editor.tick();
            editor.press(knob_center(&editor));
        }
        session.process(&[]);
        assert_eq!(view.remove(), kResultOk);
        assert!(!editor.is_open());
        edits.extend(session.host.take());
    }
    let begins = edits
        .iter()
        .filter(|call| **call == HostCall::Begin(id))
        .count();
    let ends = edits
        .iter()
        .filter(|call| **call == HostCall::End(id))
        .count();
    assert_eq!((begins, ends), (14, 14), "{edits:?}");
    // Begins and ends alternate.
    let mut open = false;
    for call in &edits {
        match call {
            HostCall::Begin(_) => {
                assert!(!open);
                open = true;
            }
            HostCall::End(_) => {
                assert!(open);
                open = false;
            }
            HostCall::Perform(_, value) => assert!(open && (0.0..=1.0).contains(value)),
            _ => {}
        }
    }
}

/// A small deterministic generator (xorshift64*), so that failures repeat.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn pick<T: Copy>(&mut self, from: &[T]) -> T {
        from[self.below(from.len() as u64) as usize]
    }
}

/// Every view call a host can make, and input a person gives the window,
/// in random orders with hostile arguments: nothing panics, `process`
/// never uses the heap, the handler's edits are always whole, and the size
/// the host reads is the editor's.
#[test]
fn random_orders_of_view_calls_and_input_keep_every_promise() {
    use manguebyte_vst3::vst3::Steinberg::VirtualKeyCodes_ as codes;

    common::watch_panics();
    let kinds: [&CStr; 4] = [c"HWND", c"NSView", c"X11EmbedWindowID", c"nonsense"];
    let parents = [0, 1, PARENT, usize::MAX];
    let scales = [
        f32::NAN,
        -1.0,
        0.0,
        0.1,
        1.0,
        1.25,
        1.5,
        2.0,
        9.0,
        f32::INFINITY,
    ];
    let sides = [i32::MIN, -1, 0, 1, 199, 240, 300, 480, 720, 5000, i32::MAX];
    let keys = [
        codes::KEY_UP as u32,
        codes::KEY_DOWN as u32,
        codes::KEY_TAB as u32,
        codes::KEY_ESCAPE as u32,
        codes::KEY_HOME as u32,
        0,
        0xFFFF,
    ];
    let coordinates = [-1.0e9, -3.0, 0.0, 40.0, 80.0, 120.0, 239.0, 1.0e9];
    for seed in 1..=2_u64 {
        let mut random = Random(0xD1B5_4A32_D192_ED03 ^ seed);
        let session = Session::new();
        let mut processing = false;
        let mut view = session.view(None);
        let mut editor: Option<HeadlessEditor> = None;
        for step in 0..1_000 {
            let point = |random: &mut Random| {
                Point::new(random.pick(&coordinates), random.pick(&coordinates))
            };
            match random.below(16) {
                0 | 1 => {
                    let kind = random.pick(&kinds);
                    let parent = random.pick(&parents);
                    let result = unsafe {
                        view.view
                            .attached(ptr::without_provenance_mut::<c_void>(parent), kind.as_ptr())
                    };
                    if result == kResultOk {
                        editor = Some(common::editor());
                    }
                }
                2 => {
                    let _ = view.remove();
                }
                3 => {
                    // The host lets go of the view and makes another.
                    let _ = view.remove();
                    view = session.view(None);
                }
                4 => {
                    let _ = view.scale(random.pick(&scales));
                }
                5 => {
                    let mut rect = ViewRect {
                        left: random.pick(&sides),
                        top: random.pick(&sides),
                        right: random.pick(&sides),
                        bottom: random.pick(&sides),
                    };
                    unsafe { view.view.checkSizeConstraint(&mut rect) };
                }
                6 => {
                    let _ = view.on_size(random.pick(&sides), random.pick(&sides));
                }
                7 => {
                    let _ = view.key_down(random.pick(&keys));
                }
                8 | 9 => {
                    if let Some(editor) = &editor {
                        editor.tick();
                    }
                }
                10 => {
                    if let Some(editor) = &editor {
                        editor.press(point(&mut random));
                    }
                }
                11 => {
                    if let Some(editor) = &editor {
                        editor.move_to(point(&mut random));
                    }
                }
                12 => {
                    if let Some(editor) = &editor {
                        editor.release(point(&mut random));
                    }
                }
                13 => {
                    processing = !processing;
                    session.set_processing(processing);
                }
                14 => {
                    if processing {
                        let point = random.below(101) as f64 / 100.0;
                        session.process(&[(0, point)][..random.below(2) as usize]);
                    }
                }
                _ => {
                    if let Some(editor) = &editor {
                        if random.below(6) == 0 {
                            editor.window().destroy_parent();
                        }
                    }
                }
            }
            if let Some(editor) = editor.as_ref().filter(|editor| editor.is_open()) {
                let shown = editor.size().expect("open");
                assert_eq!(
                    view.size(),
                    HostSize::new(shown.width, shown.height),
                    "seed {seed}, step {step}"
                );
            }
            assert_eq!(common::panics(), 0, "seed {seed}, step {step}");
        }
        let _ = view.remove();
        drop(view);
        // Every edit the handler saw is whole.
        let mut open = false;
        for call in session.host.take() {
            match call {
                HostCall::Begin(_) => {
                    assert!(!open, "seed {seed}");
                    open = true;
                }
                HostCall::End(_) => {
                    assert!(open, "seed {seed}");
                    open = false;
                }
                HostCall::Perform(_, value) => {
                    assert!(open && (0.0..=1.0).contains(&value), "seed {seed}")
                }
                _ => {}
            }
        }
        assert!(!open, "seed {seed}: an edit was left open");
    }
}
