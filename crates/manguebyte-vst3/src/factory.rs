//! The object a host asks for a plugin's classes.

// See the note at the top of `wrapper.rs`.
#![allow(clippy::unnecessary_cast)]

use std::ffi::{CStr, c_char, c_void};
use std::marker::PhantomData;

use manguebyte_plugin::{Plugin, PluginInfo, PluginKind, plugin_info};
use vst3::Steinberg::{
    self as sb, FIDString, FUnknown, IPluginFactory, IPluginFactory2Trait, IPluginFactory3,
    IPluginFactory3Trait, IPluginFactoryTrait, TUID, kInvalidArgument, kResultOk, tresult,
};
use vst3::{Class, ComWrapper};

use crate::text::{class_id, copy_narrow, copy_wide};
use crate::wrapper::{create, guard};

/// The category every audio plugin class is listed under.
const AUDIO_CLASS: &str = "Audio Module Class";

/// The version of the VST 3 interfaces the bindings were generated from.
fn sdk_version() -> &'static str {
    // SAFETY: a constant in the bindings, a string literal ending in a
    // zero.
    unsafe { CStr::from_ptr(sb::Vst::SDKVersionString) }
        .to_str()
        .unwrap_or("VST 3.8.0")
}

/// How a host should file the plugin, in VST 3's own vocabulary.
fn sub_categories(kind: PluginKind) -> &'static str {
    match kind {
        PluginKind::Instrument => "Instrument|Synth",
        PluginKind::Effect => "Fx",
        PluginKind::NoteEffect => "Fx|Instrument",
        PluginKind::Analyzer => "Fx|Analyzer",
    }
}

/// The factory for a library that exports the single plugin `P`, with the
/// plugin's identity as [`plugin_info`] gave it when the factory was made,
/// and the class id derived from it.
pub(crate) struct Factory<P> {
    info: &'static PluginInfo,
    class: [c_char; 16],
    plugin: PhantomData<fn() -> P>,
}

impl<P: Plugin> Class for Factory<P> {
    type Interfaces = (IPluginFactory3,);
}

impl<P: Plugin> IPluginFactoryTrait for Factory<P> {
    unsafe fn getFactoryInfo(&self, info: *mut sb::PFactoryInfo) -> tresult {
        // SAFETY: a struct the host owns, or null.
        let Some(info) = (unsafe { info.as_mut() }) else {
            return kInvalidArgument;
        };
        copy_narrow(self.info.vendor, &mut info.vendor);
        copy_narrow("", &mut info.url);
        copy_narrow("", &mut info.email);
        info.flags = sb::PFactoryInfo_::FactoryFlags_::kUnicode as i32;
        kResultOk
    }

    unsafe fn countClasses(&self) -> i32 {
        1
    }

    unsafe fn getClassInfo(&self, index: i32, info: *mut sb::PClassInfo) -> tresult {
        // SAFETY: a struct the host owns, or null.
        let (Some(info), 0) = (unsafe { info.as_mut() }, index) else {
            return kInvalidArgument;
        };
        info.cid = self.class;
        info.cardinality = sb::PClassInfo_::ClassCardinality_::kManyInstances as i32;
        copy_narrow(AUDIO_CLASS, &mut info.category);
        copy_narrow(self.info.name, &mut info.name);
        kResultOk
    }

    unsafe fn createInstance(
        &self,
        class: FIDString,
        interface: FIDString,
        obj: *mut *mut c_void,
    ) -> tresult {
        if class.is_null() || interface.is_null() || obj.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: the host passes a 16-byte class id; `obj` is its own.
        let wanted = unsafe {
            *obj = std::ptr::null_mut();
            *class.cast::<TUID>()
        };
        if wanted != self.class {
            return kInvalidArgument;
        }
        // SAFETY: the host passes a 16-byte interface id and a pointer of
        // its own to receive the object.
        unsafe { create::<P>(interface.cast(), obj) }
    }
}

impl<P: Plugin> IPluginFactory2Trait for Factory<P> {
    unsafe fn getClassInfo2(&self, index: i32, info: *mut sb::PClassInfo2) -> tresult {
        // SAFETY: a struct the host owns, or null.
        let (Some(info), 0) = (unsafe { info.as_mut() }, index) else {
            return kInvalidArgument;
        };
        info.cid = self.class;
        info.cardinality = sb::PClassInfo_::ClassCardinality_::kManyInstances as i32;
        copy_narrow(AUDIO_CLASS, &mut info.category);
        copy_narrow(self.info.name, &mut info.name);
        info.classFlags = 0;
        copy_narrow(sub_categories(P::INFO.kind), &mut info.subCategories);
        copy_narrow(self.info.vendor, &mut info.vendor);
        copy_narrow(self.info.version, &mut info.version);
        copy_narrow(sdk_version(), &mut info.sdkVersion);
        kResultOk
    }
}

impl<P: Plugin> IPluginFactory3Trait for Factory<P> {
    unsafe fn getClassInfoUnicode(&self, index: i32, info: *mut sb::PClassInfoW) -> tresult {
        // SAFETY: a struct the host owns, or null.
        let (Some(info), 0) = (unsafe { info.as_mut() }, index) else {
            return kInvalidArgument;
        };
        info.cid = self.class;
        info.cardinality = sb::PClassInfo_::ClassCardinality_::kManyInstances as i32;
        copy_narrow(AUDIO_CLASS, &mut info.category);
        copy_wide(self.info.name, &mut info.name);
        info.classFlags = 0;
        copy_narrow(sub_categories(P::INFO.kind), &mut info.subCategories);
        copy_wide(self.info.vendor, &mut info.vendor);
        copy_wide(self.info.version, &mut info.version);
        copy_wide(sdk_version(), &mut info.sdkVersion);
        kResultOk
    }

    unsafe fn setHostContext(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }
}

/// The VST 3 factory for a library that exports the single plugin `P`, as
/// the pointer `GetPluginFactory` must return.
///
/// [`export_vst3!`](crate::export_vst3) puts this behind the symbols a host
/// looks for. Tests can call it directly to drive a plugin through the real
/// binary interface. The caller owns one reference to the result.
///
/// The factory reports the identity [`plugin_info`] gives, so the first
/// call is when [`Plugin::info`] runs, before any instance exists; the
/// class id is [`class_id`] of that identity's id. A panic in `info`
/// gives null.
pub fn plugin_factory<P: Plugin>() -> *mut c_void {
    guard(std::ptr::null_mut(), || {
        let info = plugin_info::<P>();
        let factory = Factory::<P> {
            info,
            class: class_id(info.id),
            plugin: PhantomData,
        };
        ComWrapper::new(factory)
            .to_com_ptr::<IPluginFactory>()
            .map_or(std::ptr::null_mut(), |factory| factory.into_raw().cast())
    })
}
