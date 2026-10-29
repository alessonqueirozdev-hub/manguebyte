//! A plugin whose identity is worked out at run time, as the VST 3 factory
//! reports it.
//!
//! The plugin's `Plugin::info` gives another id, name, vendor and version
//! than its compiled `INFO`; the factory's every description, and the
//! class id it creates the plugin for, must be those, and the state the
//! component saves must carry that id.
//!
//! Every `unsafe` block in this file is a call across the VST 3 interface
//! with pointers the test owns and keeps alive for the duration of the
//! call, so the blocks are not commented one by one.
#![allow(clippy::undocumented_unsafe_blocks)]
#![allow(non_snake_case)]
// The bindings give enumerations a different integer type on Windows than
// elsewhere, so a cast needed on one platform is redundant on the other.
#![allow(clippy::unnecessary_cast)]

use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_char, c_void};
use std::ptr;
use std::sync::Arc;

use manguebyte_core::AudioBuffer;
use manguebyte_dsp::{ProcessContext, ProcessSpec};
use manguebyte_params::{FloatParam, FloatRange, param_set};
use manguebyte_plugin::{AudioLayout, Plugin, PluginInfo, PluginKind};
use manguebyte_vst3::vst3::Steinberg::Vst::{IComponent, IComponentTrait};
use manguebyte_vst3::vst3::Steinberg::{
    self as sb, IBStream, IBStreamTrait, IPluginBaseTrait, IPluginFactory, IPluginFactory2,
    IPluginFactory2Trait, IPluginFactory3, IPluginFactory3Trait, IPluginFactoryTrait,
    kInvalidArgument, kResultOk, tresult,
};
use manguebyte_vst3::vst3::{Class, ComPtr, ComWrapper, Interface};
use manguebyte_vst3::{class_id, plugin_factory};

struct GainParams {
    gain: FloatParam,
}
param_set!(GainParams { gain });

/// An effect compiled as `org.manguebyte.test.compiled`, which says it is
/// `org.manguebyte.test.found` once loaded.
struct Found(Arc<GainParams>);

impl Plugin for Found {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.compiled",
        name: "Compiled",
        vendor: "Compiled vendor",
        version: "0.0.1",
        description: "",
        kind: PluginKind::Effect,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT;
    type Params = GainParams;

    fn info() -> PluginInfo {
        PluginInfo {
            id: "org.manguebyte.test.found",
            name: "Found",
            vendor: "Found vendor",
            version: "7.8.9",
            description: "Worked out when loaded",
            kind: PluginKind::Instrument,
        }
    }
    fn new() -> Self {
        let gain = FloatParam::new("gain", "Gain", FloatRange::linear(0.0, 2.0), 1.0);
        Self(Arc::new(GainParams { gain }))
    }
    fn params(&self) -> Arc<GainParams> {
        Arc::clone(&self.0)
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
}

/// A stream in memory.
#[derive(Default)]
struct MemoryStream {
    bytes: RefCell<Vec<u8>>,
    position: Cell<usize>,
}

impl Class for MemoryStream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(&self, buffer: *mut c_void, count: i32, read: *mut i32) -> tresult {
        let bytes = self.bytes.borrow();
        let position = self.position.get();
        let take = (count.max(0) as usize).min(bytes.len() - position);
        unsafe {
            ptr::copy_nonoverlapping(bytes[position..].as_ptr(), buffer.cast::<u8>(), take);
            if !read.is_null() {
                *read = take as i32;
            }
        }
        self.position.set(position + take);
        kResultOk
    }
    unsafe fn write(&self, buffer: *mut c_void, count: i32, written: *mut i32) -> tresult {
        let count = count.max(0) as usize;
        let slice = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), count) };
        self.bytes.borrow_mut().extend_from_slice(slice);
        if !written.is_null() {
            unsafe { *written = count as i32 };
        }
        kResultOk
    }
    unsafe fn seek(&self, _: i64, _: i32, _: *mut i64) -> tresult {
        kInvalidArgument
    }
    unsafe fn tell(&self, position: *mut i64) -> tresult {
        unsafe { *position = self.position.get() as i64 };
        kResultOk
    }
}

fn iid<I: Interface>() -> *const c_char {
    I::IID.as_ptr().cast()
}

fn narrow(field: &[c_char]) -> String {
    unsafe { CStr::from_ptr(field.as_ptr()).to_str().unwrap().to_owned() }
}

fn wide(field: &[u16]) -> String {
    let len = field.iter().position(|&unit| unit == 0).unwrap();
    String::from_utf16(&field[..len]).unwrap()
}

#[test]
fn the_factory_reports_the_identity_found_at_run_time() {
    unsafe {
        let factory = ComPtr::<IPluginFactory>::from_raw(plugin_factory::<Found>().cast()).unwrap();
        let mut factory_info: sb::PFactoryInfo = std::mem::zeroed();
        assert_eq!(factory.getFactoryInfo(&mut factory_info), kResultOk);
        assert_eq!(narrow(&factory_info.vendor), "Found vendor");

        let found = class_id("org.manguebyte.test.found");
        let mut basic: sb::PClassInfo = std::mem::zeroed();
        assert_eq!(factory.getClassInfo(0, &mut basic), kResultOk);
        assert_eq!(basic.cid, found);
        assert_eq!(narrow(&basic.name), "Found");

        let second: ComPtr<IPluginFactory2> = factory.cast().unwrap();
        let mut detailed: sb::PClassInfo2 = std::mem::zeroed();
        assert_eq!(second.getClassInfo2(0, &mut detailed), kResultOk);
        assert_eq!(detailed.cid, found);
        assert_eq!(narrow(&detailed.name), "Found");
        assert_eq!(narrow(&detailed.vendor), "Found vendor");
        assert_eq!(narrow(&detailed.version), "7.8.9");
        // The kind is the compiled one, whatever `info` says.
        assert_eq!(narrow(&detailed.subCategories), "Fx");

        let third: ComPtr<IPluginFactory3> = factory.cast().unwrap();
        let mut unicode: sb::PClassInfoW = std::mem::zeroed();
        assert_eq!(third.getClassInfoUnicode(0, &mut unicode), kResultOk);
        assert_eq!(unicode.cid, found);
        assert_eq!(wide(&unicode.name), "Found");
        assert_eq!(wide(&unicode.vendor), "Found vendor");
        assert_eq!(wide(&unicode.version), "7.8.9");

        // The class id of the compiled id is not this plugin's.
        let mut object = ptr::dangling_mut::<c_void>();
        let compiled = class_id("org.manguebyte.test.compiled");
        assert_eq!(
            factory.createInstance(compiled.as_ptr(), iid::<IComponent>(), &mut object),
            kInvalidArgument
        );
        assert!(object.is_null());

        // The found one is, and its state carries the found id.
        assert_eq!(
            factory.createInstance(found.as_ptr(), iid::<IComponent>(), &mut object),
            kResultOk
        );
        let component = ComPtr::<IComponent>::from_raw(object.cast()).unwrap();
        assert_eq!(component.initialize(ptr::null_mut()), kResultOk);
        let stream = ComWrapper::new(MemoryStream::default());
        let as_stream = stream.to_com_ptr::<IBStream>().unwrap();
        assert_eq!(component.getState(as_stream.as_ptr()), kResultOk);
        let saved = String::from_utf8(stream.bytes.borrow().clone()).unwrap();
        assert!(
            saved.starts_with("manguebyte-plugin 1 org.manguebyte.test.found\n"),
            "{saved}"
        );
        // State saved under the compiled id is another plugin's.
        let foreign = ComWrapper::new(MemoryStream::default());
        foreign
            .bytes
            .borrow_mut()
            .extend_from_slice(saved.replace("test.found", "test.compiled").as_bytes());
        let as_foreign = foreign.to_com_ptr::<IBStream>().unwrap();
        assert_ne!(component.setState(as_foreign.as_ptr()), kResultOk);
        stream.position.set(0);
        assert_eq!(component.setState(as_stream.as_ptr()), kResultOk);
        assert_eq!(component.terminate(), kResultOk);
    }
}
