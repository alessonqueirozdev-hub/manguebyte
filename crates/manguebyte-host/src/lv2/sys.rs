//! The parts of the LV2 C interface the host uses.
//!
//! Written from the LV2 specification (<https://lv2plug.in/ns/>): the core
//! (`lv2core`), `urid`, `options`, `buf-size`, `parameters`, `log`,
//! `atom`, `midi`, `time` and `state`. Types keep their C names so that
//! they can be checked against the specification line by line; fields
//! named `type` in C are `type_` here.
//!
//! The framework's LV2 wrapper, `manguebyte-lv2`, declares the plugin's
//! side of the same interface. The host keeps its own copy so that it
//! does not depend on the plugin API, as the CLAP and VST 3 hosts depend
//! on their formats' headers and not on the framework's wrappers.

#![allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code
)]

use std::ffi::{CStr, c_char, c_int, c_void};

// --- Core -------------------------------------------------------------------

/// An opaque pointer to a plugin instance.
pub type LV2_Handle = *mut c_void;

/// A feature the host offers a plugin when it creates it.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LV2_Feature {
    /// The feature's URI.
    pub URI: *const c_char,
    /// The feature's data, or null.
    pub data: *mut c_void,
}

/// What `lv2_descriptor` returns: a plugin type and its functions.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LV2_Descriptor {
    /// The plugin's URI, as in its Turtle description.
    pub URI: *const c_char,
    /// Creates an instance at a sample rate, with the host's features.
    pub instantiate: Option<
        unsafe extern "C" fn(
            descriptor: *const LV2_Descriptor,
            sample_rate: f64,
            bundle_path: *const c_char,
            features: *const *const LV2_Feature,
        ) -> LV2_Handle,
    >,
    /// Tells the instance where the data of one port is.
    pub connect_port:
        Option<unsafe extern "C" fn(instance: LV2_Handle, port: u32, data_location: *mut c_void)>,
    /// Gets the instance ready to run.
    pub activate: Option<unsafe extern "C" fn(instance: LV2_Handle)>,
    /// Processes `sample_count` frames.
    pub run: Option<unsafe extern "C" fn(instance: LV2_Handle, sample_count: u32)>,
    /// Stops the instance running.
    pub deactivate: Option<unsafe extern "C" fn(instance: LV2_Handle)>,
    /// Destroys the instance.
    pub cleanup: Option<unsafe extern "C" fn(instance: LV2_Handle)>,
    /// Returns the data of an extension the plugin type supports, or null.
    pub extension_data: Option<unsafe extern "C" fn(uri: *const c_char) -> *const c_void>,
}

/// The function an LV2 library exports.
pub type LV2_Descriptor_Function = unsafe extern "C" fn(index: u32) -> *const LV2_Descriptor;

pub const LV2_CORE_PREFIX: &str = "http://lv2plug.in/ns/lv2core#";
pub const LV2_CORE__hardRTCapable: &str = "http://lv2plug.in/ns/lv2core#hardRTCapable";
pub const LV2_CORE__inPlaceBroken: &str = "http://lv2plug.in/ns/lv2core#inPlaceBroken";
pub const LV2_CORE__isLive: &str = "http://lv2plug.in/ns/lv2core#isLive";

// --- URID -------------------------------------------------------------------

/// A URI mapped to a number by the host. Zero is never a valid URID.
pub type LV2_URID = u32;

/// The data of the `urid:map` feature.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LV2_URID_Map {
    pub handle: *mut c_void,
    pub map: Option<unsafe extern "C" fn(handle: *mut c_void, uri: *const c_char) -> LV2_URID>,
}

/// The data of the `urid:unmap` feature.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LV2_URID_Unmap {
    pub handle: *mut c_void,
    pub unmap: Option<unsafe extern "C" fn(handle: *mut c_void, urid: LV2_URID) -> *const c_char>,
}

pub const LV2_URID__map: &CStr = c"http://lv2plug.in/ns/ext/urid#map";
pub const LV2_URID__unmap: &CStr = c"http://lv2plug.in/ns/ext/urid#unmap";

// --- Options, buffer sizes and parameters --------------------------------------

/// The option applies to the instance itself.
pub const LV2_OPTIONS_INSTANCE: u32 = 0;

/// One option. A list ends with one whose `key` is zero and whose `value`
/// is null.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LV2_Options_Option {
    pub context: u32,
    pub subject: u32,
    pub key: LV2_URID,
    pub size: u32,
    pub type_: LV2_URID,
    pub value: *const c_void,
}

pub const LV2_OPTIONS__options: &CStr = c"http://lv2plug.in/ns/ext/options#options";
pub const LV2_BUF_SIZE__boundedBlockLength: &CStr =
    c"http://lv2plug.in/ns/ext/buf-size#boundedBlockLength";
pub const LV2_BUF_SIZE__maxBlockLength: &CStr = c"http://lv2plug.in/ns/ext/buf-size#maxBlockLength";
pub const LV2_BUF_SIZE__minBlockLength: &CStr = c"http://lv2plug.in/ns/ext/buf-size#minBlockLength";
pub const LV2_BUF_SIZE__nominalBlockLength: &CStr =
    c"http://lv2plug.in/ns/ext/buf-size#nominalBlockLength";
pub const LV2_BUF_SIZE__sequenceSize: &CStr = c"http://lv2plug.in/ns/ext/buf-size#sequenceSize";
pub const LV2_PARAMETERS__sampleRate: &CStr = c"http://lv2plug.in/ns/ext/parameters#sampleRate";

// --- Log --------------------------------------------------------------------

/// `printf` of the `log:log` feature: C variadic in the specification.
pub type LV2_Log_Printf =
    unsafe extern "C" fn(handle: *mut c_void, type_: LV2_URID, fmt: *const c_char, ...) -> c_int;

/// `vprintf` of the `log:log` feature. The last argument is a `va_list`,
/// whose type depends on the platform; the host never reads it.
pub type LV2_Log_VPrintf = unsafe extern "C" fn(
    handle: *mut c_void,
    type_: LV2_URID,
    fmt: *const c_char,
    ap: *mut c_void,
) -> c_int;

/// The data of the `log:log` feature.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LV2_Log_Log {
    pub handle: *mut c_void,
    pub printf: Option<LV2_Log_Printf>,
    pub vprintf: Option<LV2_Log_VPrintf>,
}

pub const LV2_LOG__log: &CStr = c"http://lv2plug.in/ns/ext/log#log";
pub const LV2_LOG__Error: &CStr = c"http://lv2plug.in/ns/ext/log#Error";
pub const LV2_LOG__Warning: &CStr = c"http://lv2plug.in/ns/ext/log#Warning";
pub const LV2_LOG__Note: &CStr = c"http://lv2plug.in/ns/ext/log#Note";
pub const LV2_LOG__Trace: &CStr = c"http://lv2plug.in/ns/ext/log#Trace";

// --- Atoms ------------------------------------------------------------------

/// The header of every atom.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LV2_Atom {
    pub size: u32,
    pub type_: u32,
}

pub const LV2_ATOM__AtomPort: &str = "http://lv2plug.in/ns/ext/atom#AtomPort";
pub const LV2_ATOM__bufferType: &str = "http://lv2plug.in/ns/ext/atom#bufferType";
pub const LV2_ATOM__supports: &str = "http://lv2plug.in/ns/ext/atom#supports";
pub const LV2_ATOM__Sequence: &CStr = c"http://lv2plug.in/ns/ext/atom#Sequence";
pub const LV2_ATOM__Chunk: &CStr = c"http://lv2plug.in/ns/ext/atom#Chunk";
pub const LV2_ATOM__Object: &CStr = c"http://lv2plug.in/ns/ext/atom#Object";
pub const LV2_ATOM__Int: &CStr = c"http://lv2plug.in/ns/ext/atom#Int";
pub const LV2_ATOM__Long: &CStr = c"http://lv2plug.in/ns/ext/atom#Long";
pub const LV2_ATOM__Float: &CStr = c"http://lv2plug.in/ns/ext/atom#Float";
pub const LV2_ATOM__Double: &CStr = c"http://lv2plug.in/ns/ext/atom#Double";
pub const LV2_ATOM__frameTime: &CStr = c"http://lv2plug.in/ns/ext/atom#frameTime";
pub const LV2_ATOM__beatTime: &CStr = c"http://lv2plug.in/ns/ext/atom#beatTime";

pub const LV2_MIDI__MidiEvent: &CStr = c"http://lv2plug.in/ns/ext/midi#MidiEvent";

pub const LV2_TIME__Position: &CStr = c"http://lv2plug.in/ns/ext/time#Position";
pub const LV2_TIME__frame: &CStr = c"http://lv2plug.in/ns/ext/time#frame";
pub const LV2_TIME__speed: &CStr = c"http://lv2plug.in/ns/ext/time#speed";
pub const LV2_TIME__beatsPerMinute: &CStr = c"http://lv2plug.in/ns/ext/time#beatsPerMinute";
pub const LV2_TIME__beatsPerBar: &CStr = c"http://lv2plug.in/ns/ext/time#beatsPerBar";
pub const LV2_TIME__beatUnit: &CStr = c"http://lv2plug.in/ns/ext/time#beatUnit";
pub const LV2_TIME__barBeat: &CStr = c"http://lv2plug.in/ns/ext/time#barBeat";
pub const LV2_TIME__beat: &CStr = c"http://lv2plug.in/ns/ext/time#beat";

pub const LV2_RESIZE_PORT__minimumSize: &str = "http://lv2plug.in/ns/ext/resize-port#minimumSize";

// --- State ------------------------------------------------------------------

pub type LV2_State_Handle = *mut c_void;
pub type LV2_State_Status = u32;
pub const LV2_STATE_SUCCESS: LV2_State_Status = 0;
pub const LV2_STATE_ERR_UNKNOWN: LV2_State_Status = 1;
pub const LV2_STATE_ERR_BAD_TYPE: LV2_State_Status = 2;
pub const LV2_STATE_ERR_BAD_FLAGS: LV2_State_Status = 3;
pub const LV2_STATE_ERR_NO_FEATURE: LV2_State_Status = 4;
pub const LV2_STATE_ERR_NO_PROPERTY: LV2_State_Status = 5;
pub const LV2_STATE_ERR_NO_SPACE: LV2_State_Status = 6;

pub const LV2_STATE_IS_POD: u32 = 1;
pub const LV2_STATE_IS_PORTABLE: u32 = 2;

pub type LV2_State_Store_Function = Option<
    unsafe extern "C" fn(
        handle: LV2_State_Handle,
        key: u32,
        value: *const c_void,
        size: usize,
        type_: u32,
        flags: u32,
    ) -> LV2_State_Status,
>;

pub type LV2_State_Retrieve_Function = Option<
    unsafe extern "C" fn(
        handle: LV2_State_Handle,
        key: u32,
        size: *mut usize,
        type_: *mut u32,
        flags: *mut u32,
    ) -> *const c_void,
>;

/// The data of the `state:interface` extension.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LV2_State_Interface {
    pub save: Option<
        unsafe extern "C" fn(
            instance: LV2_Handle,
            store: LV2_State_Store_Function,
            handle: LV2_State_Handle,
            flags: u32,
            features: *const *const LV2_Feature,
        ) -> LV2_State_Status,
    >,
    pub restore: Option<
        unsafe extern "C" fn(
            instance: LV2_Handle,
            retrieve: LV2_State_Retrieve_Function,
            handle: LV2_State_Handle,
            flags: u32,
            features: *const *const LV2_Feature,
        ) -> LV2_State_Status,
    >,
}

pub const LV2_STATE__interface: &CStr = c"http://lv2plug.in/ns/ext/state#interface";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structures_have_the_c_layout() {
        let pointer = size_of::<*const c_void>();
        assert_eq!(size_of::<LV2_Atom>(), 8);
        assert_eq!(size_of::<LV2_Feature>(), 2 * pointer);
        assert_eq!(size_of::<LV2_URID_Map>(), 2 * pointer);
        assert_eq!(size_of::<LV2_URID_Unmap>(), 2 * pointer);
        assert_eq!(size_of::<LV2_Log_Log>(), 3 * pointer);
        assert_eq!(size_of::<LV2_Descriptor>(), 8 * pointer);
        assert_eq!(size_of::<LV2_State_Interface>(), 2 * pointer);
        assert_eq!(
            std::mem::offset_of!(LV2_Options_Option, value),
            20usize.next_multiple_of(pointer)
        );
    }
}
