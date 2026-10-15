//! The parts of the LV2 C interface this crate uses.
//!
//! Written from the LV2 specification (<https://lv2plug.in/ns/>): the core
//! (`lv2core`), `urid`, `options`, `buf-size`, `log`, `atom`, `midi`,
//! `time` and `state`. Types keep their C names so that they can be checked
//! against the specification line by line; fields named `type` in C are
//! `type_` here.
//!
//! A host written in Rust, such as this crate's own tests, can use these to
//! drive a plugin through the same C interface a real host uses.

#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]

use std::ffi::{CStr, c_char, c_int, c_void};

// --- Core -------------------------------------------------------------------

/// An opaque pointer to a plugin instance.
pub type LV2_Handle = *mut c_void;

/// A feature the host offers a plugin when it creates it: a URI naming the
/// feature and data whose meaning the feature's specification defines.
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

/// The name of the function an LV2 library exports.
pub type LV2_Descriptor_Function = unsafe extern "C" fn(index: u32) -> *const LV2_Descriptor;

/// `lv2:hardRTCapable`: the plugin may run in a hard real-time thread.
pub const LV2_CORE__hardRTCapable: &CStr = c"http://lv2plug.in/ns/lv2core#hardRTCapable";

// --- URID -------------------------------------------------------------------

/// A URI mapped to a number by the host. Zero is never a valid URID.
pub type LV2_URID = u32;

/// The data of the `urid:map` feature.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LV2_URID_Map {
    /// The host's data, passed back to `map`.
    pub handle: *mut c_void,
    /// Maps a URI to its URID, or returns zero if it cannot.
    pub map: Option<unsafe extern "C" fn(handle: *mut c_void, uri: *const c_char) -> LV2_URID>,
}

/// The `urid:map` feature.
pub const LV2_URID__map: &CStr = c"http://lv2plug.in/ns/ext/urid#map";

// --- Options and buffer sizes -------------------------------------------------

/// `LV2_Options_Context`: what an option applies to.
pub type LV2_Options_Context = u32;
/// The option applies to the instance itself.
pub const LV2_OPTIONS_INSTANCE: LV2_Options_Context = 0;
/// The option applies to some other resource.
pub const LV2_OPTIONS_RESOURCE: LV2_Options_Context = 1;
/// The option applies to a blank node.
pub const LV2_OPTIONS_BLANK: LV2_Options_Context = 2;
/// The option applies to a port.
pub const LV2_OPTIONS_PORT: LV2_Options_Context = 3;

/// One option. A list of them ends with one whose `key` is zero and whose
/// `value` is null.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LV2_Options_Option {
    /// What the option applies to.
    pub context: LV2_Options_Context,
    /// The port index or resource URID the option applies to.
    pub subject: u32,
    /// The option's URID.
    pub key: LV2_URID,
    /// The size of `value` in bytes.
    pub size: u32,
    /// The URID of the value's atom type.
    pub type_: LV2_URID,
    /// The value.
    pub value: *const c_void,
}

/// The `options:options` feature, whose data is a list of options.
pub const LV2_OPTIONS__options: &CStr = c"http://lv2plug.in/ns/ext/options#options";
/// The `buf-size:boundedBlockLength` feature.
pub const LV2_BUF_SIZE__boundedBlockLength: &CStr =
    c"http://lv2plug.in/ns/ext/buf-size#boundedBlockLength";
/// The `buf-size:maxBlockLength` option, an `atom:Int`.
pub const LV2_BUF_SIZE__maxBlockLength: &CStr = c"http://lv2plug.in/ns/ext/buf-size#maxBlockLength";

// --- Log --------------------------------------------------------------------

/// The data of the `log:log` feature.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LV2_Log_Log {
    /// The host's data, passed back to the functions.
    pub handle: *mut c_void,
    /// Logs a message of a type such as `log:Warning`, formatted as by C's
    /// `printf`.
    pub printf: Option<
        unsafe extern "C" fn(
            handle: *mut c_void,
            type_: LV2_URID,
            fmt: *const c_char,
            ...
        ) -> c_int,
    >,
    /// The same with a `va_list`. Its last argument's type depends on the
    /// platform, and this crate never calls it; it is here for the layout.
    pub vprintf: Option<
        unsafe extern "C" fn(
            handle: *mut c_void,
            type_: LV2_URID,
            fmt: *const c_char,
            ap: *mut c_void,
        ) -> c_int,
    >,
}

/// The `log:log` feature.
pub const LV2_LOG__log: &CStr = c"http://lv2plug.in/ns/ext/log#log";
/// A message about an error.
pub const LV2_LOG__Error: &CStr = c"http://lv2plug.in/ns/ext/log#Error";
/// A message about something that may be wrong.
pub const LV2_LOG__Warning: &CStr = c"http://lv2plug.in/ns/ext/log#Warning";
/// A message for information.
pub const LV2_LOG__Note: &CStr = c"http://lv2plug.in/ns/ext/log#Note";

// --- Atoms ------------------------------------------------------------------

/// The header of every atom: the size of the body that follows, in bytes,
/// and the URID of its type.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LV2_Atom {
    /// The size of the body, not counting this header.
    pub size: u32,
    /// The URID of the atom's type.
    pub type_: u32,
}

/// The body of a sequence, before its events.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LV2_Atom_Sequence_Body {
    /// The URID of the unit of the events' time stamps: `atom:frameTime`,
    /// `atom:beatTime`, or zero for frames.
    pub unit: u32,
    /// Unused; zero.
    pub pad: u32,
}

/// A sequence of time-stamped events.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LV2_Atom_Sequence {
    /// The header; `size` covers the body and every event.
    pub atom: LV2_Atom,
    /// The sequence's body.
    pub body: LV2_Atom_Sequence_Body,
}

/// The header of one event of a sequence. The event's body follows it and
/// is padded to a multiple of 8 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LV2_Atom_Event {
    /// The time stamp: a frame in the block for a sequence in frames. (A
    /// union with a `double` of beats in C.)
    pub time_frames: i64,
    /// The event's atom header.
    pub body: LV2_Atom,
}

/// The body of an object, before its properties.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LV2_Atom_Object_Body {
    /// The URID of the object's subject, or zero.
    pub id: u32,
    /// The URID of the object's type, such as `time:Position`.
    pub otype: u32,
}

/// The header of one property of an object. The value's body follows it
/// and is padded to a multiple of 8 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LV2_Atom_Property_Body {
    /// The URID of the property.
    pub key: u32,
    /// The URID of its context, usually zero.
    pub context: u32,
    /// The value's atom header.
    pub value: LV2_Atom,
}

/// `atom:Sequence`.
pub const LV2_ATOM__Sequence: &CStr = c"http://lv2plug.in/ns/ext/atom#Sequence";
/// `atom:Object`.
pub const LV2_ATOM__Object: &CStr = c"http://lv2plug.in/ns/ext/atom#Object";
/// `atom:Blank`, an object type older hosts still send.
pub const LV2_ATOM__Blank: &CStr = c"http://lv2plug.in/ns/ext/atom#Blank";
/// `atom:Resource`, an object type older hosts still send.
pub const LV2_ATOM__Resource: &CStr = c"http://lv2plug.in/ns/ext/atom#Resource";
/// `atom:Int`, a 32-bit signed integer.
pub const LV2_ATOM__Int: &CStr = c"http://lv2plug.in/ns/ext/atom#Int";
/// `atom:Long`, a 64-bit signed integer.
pub const LV2_ATOM__Long: &CStr = c"http://lv2plug.in/ns/ext/atom#Long";
/// `atom:Float`, a 32-bit float.
pub const LV2_ATOM__Float: &CStr = c"http://lv2plug.in/ns/ext/atom#Float";
/// `atom:Double`, a 64-bit float.
pub const LV2_ATOM__Double: &CStr = c"http://lv2plug.in/ns/ext/atom#Double";
/// `atom:String`, UTF-8 text ending with a NUL that the size counts.
pub const LV2_ATOM__String: &CStr = c"http://lv2plug.in/ns/ext/atom#String";
/// `atom:frameTime`: time stamps in frames.
pub const LV2_ATOM__frameTime: &CStr = c"http://lv2plug.in/ns/ext/atom#frameTime";
/// `atom:beatTime`: time stamps in beats.
pub const LV2_ATOM__beatTime: &CStr = c"http://lv2plug.in/ns/ext/atom#beatTime";

/// `midi:MidiEvent`: an atom whose body is one MIDI message.
pub const LV2_MIDI__MidiEvent: &CStr = c"http://lv2plug.in/ns/ext/midi#MidiEvent";

// --- Time -------------------------------------------------------------------

/// `time:Position`: an object describing where the transport is.
pub const LV2_TIME__Position: &CStr = c"http://lv2plug.in/ns/ext/time#Position";
/// `time:frame`: the frame position, an `atom:Long`.
pub const LV2_TIME__frame: &CStr = c"http://lv2plug.in/ns/ext/time#frame";
/// `time:speed`: transport speed, 1 when playing, 0 when stopped.
pub const LV2_TIME__speed: &CStr = c"http://lv2plug.in/ns/ext/time#speed";
/// `time:beatsPerMinute`: tempo in beats of `time:beatUnit` per minute.
pub const LV2_TIME__beatsPerMinute: &CStr = c"http://lv2plug.in/ns/ext/time#beatsPerMinute";
/// `time:beatsPerBar`: the time signature's numerator.
pub const LV2_TIME__beatsPerBar: &CStr = c"http://lv2plug.in/ns/ext/time#beatsPerBar";
/// `time:beatUnit`: the note value of a beat, the time signature's
/// denominator.
pub const LV2_TIME__beatUnit: &CStr = c"http://lv2plug.in/ns/ext/time#beatUnit";
/// `time:bar`: the bar number, from zero.
pub const LV2_TIME__bar: &CStr = c"http://lv2plug.in/ns/ext/time#bar";
/// `time:barBeat`: the beat within the bar, from zero.
pub const LV2_TIME__barBeat: &CStr = c"http://lv2plug.in/ns/ext/time#barBeat";
/// `time:beat`: the position in beats from the start of the timeline.
pub const LV2_TIME__beat: &CStr = c"http://lv2plug.in/ns/ext/time#beat";

// --- State ------------------------------------------------------------------

/// The host's data, passed back to the store and retrieve functions.
pub type LV2_State_Handle = *mut c_void;

/// `LV2_State_Status`: how a state call went.
pub type LV2_State_Status = u32;
/// It worked.
pub const LV2_STATE_SUCCESS: LV2_State_Status = 0;
/// It failed for a reason not listed below.
pub const LV2_STATE_ERR_UNKNOWN: LV2_State_Status = 1;
/// A value had a type that could not be handled.
pub const LV2_STATE_ERR_BAD_TYPE: LV2_State_Status = 2;
/// A value had flags that could not be handled.
pub const LV2_STATE_ERR_BAD_FLAGS: LV2_State_Status = 3;
/// A feature the call needs was not given.
pub const LV2_STATE_ERR_NO_FEATURE: LV2_State_Status = 4;
/// A property was missing.
pub const LV2_STATE_ERR_NO_PROPERTY: LV2_State_Status = 5;
/// There was not enough space.
pub const LV2_STATE_ERR_NO_SPACE: LV2_State_Status = 6;

/// `LV2_State_Flags`: what kind of value is stored.
pub type LV2_State_Flags = u32;
/// Plain old data: the value can be copied as bytes.
pub const LV2_STATE_IS_POD: LV2_State_Flags = 1;
/// The value means the same on any machine.
pub const LV2_STATE_IS_PORTABLE: LV2_State_Flags = 2;
/// The value is in the machine's native format.
pub const LV2_STATE_IS_NATIVE: LV2_State_Flags = 4;

/// Stores one property. The host copies `value`.
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

/// Retrieves one property, or returns null if there is none.
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
    /// Saves the instance's state through `store`.
    pub save: Option<
        unsafe extern "C" fn(
            instance: LV2_Handle,
            store: LV2_State_Store_Function,
            handle: LV2_State_Handle,
            flags: u32,
            features: *const *const LV2_Feature,
        ) -> LV2_State_Status,
    >,
    /// Restores the instance's state through `retrieve`.
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

/// The `state:interface` extension.
pub const LV2_STATE__interface: &CStr = c"http://lv2plug.in/ns/ext/state#interface";

#[cfg(test)]
mod tests {
    use super::*;

    /// The layouts the specification's headers give, on every platform.
    #[test]
    fn structures_have_the_c_layout() {
        assert_eq!(size_of::<LV2_Atom>(), 8);
        assert_eq!(size_of::<LV2_Atom_Sequence>(), 16);
        assert_eq!(size_of::<LV2_Atom_Event>(), 16);
        assert_eq!(size_of::<LV2_Atom_Property_Body>(), 16);
        assert_eq!(size_of::<LV2_Atom_Object_Body>(), 8);
        let pointer = size_of::<*const c_void>();
        assert_eq!(size_of::<LV2_Feature>(), 2 * pointer);
        assert_eq!(size_of::<LV2_URID_Map>(), 2 * pointer);
        assert_eq!(size_of::<LV2_Log_Log>(), 3 * pointer);
        assert_eq!(size_of::<LV2_Descriptor>(), 8 * pointer);
        assert_eq!(size_of::<LV2_State_Interface>(), 2 * pointer);
        // Five 32-bit fields (20 bytes), then the pointer at its alignment.
        assert_eq!(
            size_of::<LV2_Options_Option>(),
            20usize.next_multiple_of(pointer) + pointer
        );
        assert_eq!(
            std::mem::offset_of!(LV2_Options_Option, value),
            20usize.next_multiple_of(pointer)
        );
    }
}
