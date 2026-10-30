//! The parts of Apple's C interfaces the wrapper uses.
//!
//! Written from Apple's public headers: `AudioComponent.h`,
//! `AUComponent.h`, `AudioUnitProperties.h` and `MusicDevice.h` of
//! AudioToolbox, `CoreAudioBaseTypes.h` of CoreAudioTypes, and the
//! CoreFoundation headers for the few CoreFoundation functions a saved
//! state needs. Types and constants keep their C names, so that they can
//! be checked against the headers line by line; fields named `type` in C
//! are `type_` here.
//!
//! On macOS the functions are CoreFoundation's and AudioToolbox's own. On
//! every other system the same names are a small stand-in written in Rust
//! ([`fake_cf`](super::fake_cf)), with the same signatures, so that the
//! wrapper is built and tested through its C interface everywhere; only a
//! Mac can load the component.

#![allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code
)]

use std::ffi::{c_char, c_void};

// --- CoreFoundation ---------------------------------------------------------

pub type Boolean = u8;
pub type CFIndex = isize;
pub type CFTypeID = usize;
pub type CFTypeRef = *const c_void;
pub type CFAllocatorRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFDataRef = *const c_void;
pub type CFNumberRef = *const c_void;
pub type CFArrayRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFMutableDictionaryRef = *mut c_void;
pub type CFPropertyListRef = *const c_void;
pub type CFStringEncoding = u32;
pub type CFNumberType = CFIndex;

pub const kCFStringEncodingUTF8: CFStringEncoding = 0x0800_0100;
pub const kCFNumberSInt32Type: CFNumberType = 3;
pub const kCFNumberSInt64Type: CFNumberType = 4;

/// `CFDictionaryKeyCallBacks`. Only ever used through the address of
/// CoreFoundation's own `kCFTypeDictionaryKeyCallBacks`.
#[repr(C)]
pub struct CFDictionaryKeyCallBacks {
    pub version: CFIndex,
    pub retain: *const c_void,
    pub release: *const c_void,
    pub copyDescription: *const c_void,
    pub equal: *const c_void,
    pub hash: *const c_void,
}

/// `CFDictionaryValueCallBacks`, likewise.
#[repr(C)]
pub struct CFDictionaryValueCallBacks {
    pub version: CFIndex,
    pub retain: *const c_void,
    pub release: *const c_void,
    pub copyDescription: *const c_void,
    pub equal: *const c_void,
}

/// `CFArrayCallBacks`, likewise.
#[repr(C)]
pub struct CFArrayCallBacks {
    pub version: CFIndex,
    pub retain: *const c_void,
    pub release: *const c_void,
    pub copyDescription: *const c_void,
    pub equal: *const c_void,
}

// --- CoreAudio types ----------------------------------------------------------

pub type OSStatus = i32;
pub type OSType = u32;
pub type AudioComponentInstance = *mut c_void;
pub type AudioUnit = AudioComponentInstance;
pub type AudioUnitPropertyID = u32;
pub type AudioUnitScope = u32;
pub type AudioUnitElement = u32;
pub type AudioUnitParameterID = u32;
pub type AudioUnitParameterValue = f32;
pub type AudioUnitRenderActionFlags = u32;
pub type AudioChannelLayoutTag = u32;

pub const noErr: OSStatus = 0;
pub const paramErr: OSStatus = -50;
pub const badComponentSelector: OSStatus = 0x8000_8002_u32 as i32;

pub const kAudioUnitErr_InvalidProperty: OSStatus = -10879;
pub const kAudioUnitErr_InvalidParameter: OSStatus = -10878;
pub const kAudioUnitErr_InvalidElement: OSStatus = -10877;
pub const kAudioUnitErr_NoConnection: OSStatus = -10876;
pub const kAudioUnitErr_FailedInitialization: OSStatus = -10875;
pub const kAudioUnitErr_TooManyFramesToProcess: OSStatus = -10874;
pub const kAudioUnitErr_FormatNotSupported: OSStatus = -10868;
pub const kAudioUnitErr_Uninitialized: OSStatus = -10867;
pub const kAudioUnitErr_InvalidScope: OSStatus = -10866;
pub const kAudioUnitErr_PropertyNotWritable: OSStatus = -10865;
pub const kAudioUnitErr_CannotDoInCurrentContext: OSStatus = -10863;
pub const kAudioUnitErr_InvalidPropertyValue: OSStatus = -10851;
pub const kAudioUnitErr_PropertyNotInUse: OSStatus = -10850;
pub const kAudioUnitErr_Initialized: OSStatus = -10849;

pub const kAudioUnitScope_Global: AudioUnitScope = 0;
pub const kAudioUnitScope_Input: AudioUnitScope = 1;
pub const kAudioUnitScope_Output: AudioUnitScope = 2;

/// `AudioComponentDescription`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioComponentDescription {
    pub componentType: OSType,
    pub componentSubType: OSType,
    pub componentManufacturer: OSType,
    pub componentFlags: u32,
    pub componentFlagsMask: u32,
}

/// `AudioComponentPlugInInterface`: what a factory function returns. The
/// host calls `Open` with the instance it made, looks methods up by
/// selector, and calls `Close`, which frees it.
#[repr(C)]
pub struct AudioComponentPlugInInterface {
    pub Open: unsafe extern "C" fn(this: *mut c_void, instance: AudioComponentInstance) -> OSStatus,
    pub Close: unsafe extern "C" fn(this: *mut c_void) -> OSStatus,
    /// Returns the method for a selector, or null. In C its type is
    /// `AudioComponentMethod`, a function pointer the host casts to the
    /// method's own type before it calls it.
    pub Lookup: unsafe extern "C" fn(selector: i16) -> *const c_void,
    pub reserved: *mut c_void,
}

// Selectors (`AUComponent.h`, `MusicDevice.h`).
pub const kAudioUnitInitializeSelect: i16 = 0x0001;
pub const kAudioUnitUninitializeSelect: i16 = 0x0002;
pub const kAudioUnitGetPropertyInfoSelect: i16 = 0x0003;
pub const kAudioUnitGetPropertySelect: i16 = 0x0004;
pub const kAudioUnitSetPropertySelect: i16 = 0x0005;
pub const kAudioUnitGetParameterSelect: i16 = 0x0006;
pub const kAudioUnitSetParameterSelect: i16 = 0x0007;
pub const kAudioUnitResetSelect: i16 = 0x0009;
pub const kAudioUnitAddPropertyListenerSelect: i16 = 0x000A;
pub const kAudioUnitRemovePropertyListenerSelect: i16 = 0x000B;
pub const kAudioUnitRenderSelect: i16 = 0x000E;
pub const kAudioUnitAddRenderNotifySelect: i16 = 0x000F;
pub const kAudioUnitRemoveRenderNotifySelect: i16 = 0x0010;
pub const kAudioUnitScheduleParametersSelect: i16 = 0x0011;
pub const kAudioUnitRemovePropertyListenerWithUserDataSelect: i16 = 0x0012;
pub const kMusicDeviceMIDIEventSelect: i16 = 0x0101;
pub const kMusicDeviceSysExSelect: i16 = 0x0102;
pub const kMusicDeviceStartNoteSelect: i16 = 0x0105;
pub const kMusicDeviceStopNoteSelect: i16 = 0x0106;

/// The head of `MusicDeviceNoteParams`: how many arguments there are, then
/// the pitch (a MIDI key, possibly fractional) and the velocity (0 to
/// 127), then `argCount - 2` controls the wrapper does not read.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MusicDeviceNoteParams {
    pub argCount: u32,
    pub mPitch: f32,
    pub mVelocity: f32,
}

// Properties (`AudioUnitProperties.h`).
pub const kAudioUnitProperty_ClassInfo: AudioUnitPropertyID = 0;
pub const kAudioUnitProperty_MakeConnection: AudioUnitPropertyID = 1;
pub const kAudioUnitProperty_SampleRate: AudioUnitPropertyID = 2;
pub const kAudioUnitProperty_ParameterList: AudioUnitPropertyID = 3;
pub const kAudioUnitProperty_ParameterInfo: AudioUnitPropertyID = 4;
pub const kAudioUnitProperty_StreamFormat: AudioUnitPropertyID = 8;
pub const kAudioUnitProperty_ElementCount: AudioUnitPropertyID = 11;
pub const kAudioUnitProperty_Latency: AudioUnitPropertyID = 12;
pub const kAudioUnitProperty_SupportedNumChannels: AudioUnitPropertyID = 13;
pub const kAudioUnitProperty_MaximumFramesPerSlice: AudioUnitPropertyID = 14;
pub const kAudioUnitProperty_ParameterValueStrings: AudioUnitPropertyID = 16;
pub const kAudioUnitProperty_AudioChannelLayout: AudioUnitPropertyID = 19;
pub const kAudioUnitProperty_TailTime: AudioUnitPropertyID = 20;
pub const kAudioUnitProperty_BypassEffect: AudioUnitPropertyID = 21;
pub const kAudioUnitProperty_LastRenderError: AudioUnitPropertyID = 22;
pub const kAudioUnitProperty_SetRenderCallback: AudioUnitPropertyID = 23;
pub const kAudioUnitProperty_HostCallbacks: AudioUnitPropertyID = 27;
/// Deprecated in favour of `kAudioUnitProperty_PresentPreset`; its name is
/// not retained for the caller.
pub const kAudioUnitProperty_CurrentPreset: AudioUnitPropertyID = 28;
pub const kAudioUnitProperty_InPlaceProcessing: AudioUnitPropertyID = 29;
pub const kAudioUnitProperty_ElementName: AudioUnitPropertyID = 30;
pub const kAudioUnitProperty_SupportedChannelLayoutTags: AudioUnitPropertyID = 32;
pub const kAudioUnitProperty_ParameterStringFromValue: AudioUnitPropertyID = 33;
pub const kAudioUnitProperty_PresentPreset: AudioUnitPropertyID = 36;
pub const kAudioUnitProperty_ParameterValueFromString: AudioUnitPropertyID = 38;
pub const kAudioUnitProperty_MIDIOutputCallbackInfo: AudioUnitPropertyID = 47;
pub const kAudioUnitProperty_MIDIOutputCallback: AudioUnitPropertyID = 48;
pub const kAudioUnitProperty_ShouldAllocateBuffer: AudioUnitPropertyID = 51;

/// `AUMIDIOutputCallback`: the host's function that takes the MIDI a unit
/// produced in a render, on output `midiOutNum`, as a `MIDIPacketList`.
pub type AUMIDIOutputCallback = Option<
    unsafe extern "C" fn(
        userData: *mut c_void,
        timeStamp: *const AudioTimeStamp,
        midiOutNum: u32,
        pktlist: *const c_void,
    ) -> OSStatus,
>;

/// `AUMIDIOutputCallbackStruct`, the value of
/// `kAudioUnitProperty_MIDIOutputCallback`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AUMIDIOutputCallbackStruct {
    pub midiOutputCallback: AUMIDIOutputCallback,
    pub userData: *mut c_void,
}

/// The layout of CoreMIDI's `MIDIPacketList`, which the headers declare
/// under `#pragma pack(push, 4)`: a `UInt32` count, then packets of a
/// `UInt64` time stamp, a `UInt16` length and the bytes. The first packet
/// starts right after the count.
pub const MIDI_PACKET_LIST_PACKETS: usize = 4;
/// Where a packet's length is, from the packet's start.
pub const MIDI_PACKET_LENGTH: usize = 8;
/// Where a packet's bytes are, from the packet's start.
pub const MIDI_PACKET_DATA: usize = 10;
/// How the next packet's start is rounded up after a packet's bytes, as
/// `MIDIPacketNext` does: to four bytes on ARM, not at all on Intel.
pub const MIDI_PACKET_ALIGN: usize = if cfg!(any(target_arch = "aarch64", target_arch = "arm")) {
    4
} else {
    1
};

/// `AUPreset`: a preset's number, negative for a user's preset, and its
/// name.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AUPreset {
    pub presetNumber: i32,
    pub presetName: CFStringRef,
}

/// `AudioStreamBasicDescription`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AudioStreamBasicDescription {
    pub mSampleRate: f64,
    pub mFormatID: u32,
    pub mFormatFlags: u32,
    pub mBytesPerPacket: u32,
    pub mFramesPerPacket: u32,
    pub mBytesPerFrame: u32,
    pub mChannelsPerFrame: u32,
    pub mBitsPerChannel: u32,
    pub mReserved: u32,
}

pub const kAudioFormatLinearPCM: u32 = u32::from_be_bytes(*b"lpcm");
pub const kAudioFormatFlagIsFloat: u32 = 1 << 0;
pub const kAudioFormatFlagIsBigEndian: u32 = 1 << 1;
pub const kAudioFormatFlagIsPacked: u32 = 1 << 3;
pub const kAudioFormatFlagIsNonInterleaved: u32 = 1 << 5;

/// `AudioBuffer`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AudioBuffer {
    pub mNumberChannels: u32,
    pub mDataByteSize: u32,
    pub mData: *mut c_void,
}

/// The head of `AudioBufferList`: a count, then that many `AudioBuffer`s
/// starting at [`AUDIO_BUFFER_LIST_BUFFERS`] bytes in.
#[repr(C)]
pub struct AudioBufferList {
    pub mNumberBuffers: u32,
    pub mBuffers: [AudioBuffer; 1],
}

/// Where the first `AudioBuffer` of an `AudioBufferList` is.
pub const AUDIO_BUFFER_LIST_BUFFERS: usize = std::mem::offset_of!(AudioBufferList, mBuffers);

/// `SMPTETime`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct SMPTETime {
    pub mSubframes: i16,
    pub mSubframeDivisor: i16,
    pub mCounter: u32,
    pub mType: u32,
    pub mFlags: u32,
    pub mHours: i16,
    pub mMinutes: i16,
    pub mSeconds: i16,
    pub mFrames: i16,
}

/// `AudioTimeStamp`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct AudioTimeStamp {
    pub mSampleTime: f64,
    pub mHostTime: u64,
    pub mRateScalar: f64,
    pub mWordClockTime: u64,
    pub mSMPTETime: SMPTETime,
    pub mFlags: u32,
    pub mReserved: u32,
}

/// `AudioTimeStamp::mFlags`: `mSampleTime` is valid.
pub const kAudioTimeStampSampleTimeValid: u32 = 1 << 0;

pub const kAudioUnitRenderAction_PreRender: AudioUnitRenderActionFlags = 1 << 2;
pub const kAudioUnitRenderAction_PostRender: AudioUnitRenderActionFlags = 1 << 3;
pub const kAudioUnitRenderAction_OutputIsSilence: AudioUnitRenderActionFlags = 1 << 4;

/// `AURenderCallback`.
pub type AURenderCallback = Option<
    unsafe extern "C" fn(
        inRefCon: *mut c_void,
        ioActionFlags: *mut AudioUnitRenderActionFlags,
        inTimeStamp: *const AudioTimeStamp,
        inBusNumber: u32,
        inNumberFrames: u32,
        ioData: *mut AudioBufferList,
    ) -> OSStatus,
>;

/// `AURenderCallbackStruct`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AURenderCallbackStruct {
    pub inputProc: AURenderCallback,
    pub inputProcRefCon: *mut c_void,
}

/// `AudioUnitConnection`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AudioUnitConnection {
    pub sourceAudioUnit: AudioUnit,
    pub sourceOutputNumber: u32,
    pub destInputNumber: u32,
}

/// `AUChannelInfo`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AUChannelInfo {
    pub inChannels: i16,
    pub outChannels: i16,
}

/// `AudioChannelLayout`, without its variable list of descriptions, which
/// starts at [`CHANNEL_LAYOUT_DESCRIPTIONS`] bytes in and which the
/// wrapper never fills in.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioChannelLayout {
    pub mChannelLayoutTag: AudioChannelLayoutTag,
    pub mChannelBitmap: u32,
    pub mNumberChannelDescriptions: u32,
}

/// Where the descriptions of an `AudioChannelLayout` start, and so the size
/// of one that has none.
pub const CHANNEL_LAYOUT_DESCRIPTIONS: usize = 12;
/// The size of one `AudioChannelDescription`, which the C struct includes
/// once in its own size.
pub const CHANNEL_DESCRIPTION_BYTES: usize = 20;

pub const kAudioChannelLayoutTag_Mono: AudioChannelLayoutTag = (100 << 16) | 1;
pub const kAudioChannelLayoutTag_Stereo: AudioChannelLayoutTag = (101 << 16) | 2;
pub const kAudioChannelLayoutTag_DiscreteInOrder: AudioChannelLayoutTag = 147 << 16;

/// `AudioUnitParameterInfo`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AudioUnitParameterInfo {
    pub name: [c_char; 52],
    pub unitName: CFStringRef,
    pub clumpID: u32,
    pub cfNameString: CFStringRef,
    pub unit: u32,
    pub minValue: AudioUnitParameterValue,
    pub maxValue: AudioUnitParameterValue,
    pub defaultValue: AudioUnitParameterValue,
    pub flags: u32,
}

pub const kAudioUnitParameterUnit_Generic: u32 = 0;
pub const kAudioUnitParameterUnit_Indexed: u32 = 1;
pub const kAudioUnitParameterUnit_Boolean: u32 = 2;
pub const kAudioUnitParameterUnit_Percent: u32 = 3;
pub const kAudioUnitParameterUnit_Seconds: u32 = 4;
pub const kAudioUnitParameterUnit_Hertz: u32 = 8;
pub const kAudioUnitParameterUnit_Cents: u32 = 9;
pub const kAudioUnitParameterUnit_RelativeSemiTones: u32 = 10;
pub const kAudioUnitParameterUnit_Decibels: u32 = 13;
pub const kAudioUnitParameterUnit_Degrees: u32 = 15;
pub const kAudioUnitParameterUnit_Octaves: u32 = 21;
pub const kAudioUnitParameterUnit_BPM: u32 = 22;
pub const kAudioUnitParameterUnit_Beats: u32 = 23;
pub const kAudioUnitParameterUnit_Milliseconds: u32 = 24;
pub const kAudioUnitParameterUnit_Ratio: u32 = 25;
pub const kAudioUnitParameterUnit_CustomUnit: u32 = 26;

pub const kAudioUnitParameterFlag_CFNameRelease: u32 = 1 << 4;
pub const kAudioUnitParameterFlag_ValuesHaveStrings: u32 = 1 << 21;
pub const kAudioUnitParameterFlag_DisplayLogarithmic: u32 = 1 << 22;
pub const kAudioUnitParameterFlag_HasCFNameString: u32 = 1 << 27;
pub const kAudioUnitParameterFlag_IsReadable: u32 = 1 << 30;
pub const kAudioUnitParameterFlag_IsWritable: u32 = 1 << 31;

/// `AudioUnitParameterStringFromValue`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AudioUnitParameterStringFromValue {
    pub inParamID: AudioUnitParameterID,
    pub inValue: *const AudioUnitParameterValue,
    pub outString: CFStringRef,
}

/// `AudioUnitParameterValueFromString`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AudioUnitParameterValueFromString {
    pub inParamID: AudioUnitParameterID,
    pub inString: CFStringRef,
    pub outValue: AudioUnitParameterValue,
}

/// `AudioUnitParameterEvent`: one parameter change a host schedules, with
/// the union of its two kinds as the larger of them.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AudioUnitParameterEvent {
    pub scope: AudioUnitScope,
    pub element: AudioUnitElement,
    pub parameter: AudioUnitParameterID,
    pub eventType: u32,
    /// The union: for an immediate change, the frame (as a `u32`) and the
    /// value; for a ramp, the first frame (an `i32`, which may be before
    /// this slice), the number of frames, and the values at both ends.
    pub eventValues: [u32; 4],
}

pub const kParameterEvent_Immediate: u32 = 1;
pub const kParameterEvent_Ramped: u32 = 2;

/// `AudioUnitPropertyListenerProc`.
pub type AudioUnitPropertyListenerProc = Option<
    unsafe extern "C" fn(
        inRefCon: *mut c_void,
        inUnit: AudioUnit,
        inID: AudioUnitPropertyID,
        inScope: AudioUnitScope,
        inElement: AudioUnitElement,
    ),
>;

/// `HostCallbackInfo`: the host's functions for the transport. A host may
/// give an older, shorter one.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct HostCallbackInfo {
    pub hostUserData: *mut c_void,
    pub beatAndTempoProc: Option<
        unsafe extern "C" fn(
            inHostUserData: *mut c_void,
            outCurrentBeat: *mut f64,
            outCurrentTempo: *mut f64,
        ) -> OSStatus,
    >,
    pub musicalTimeLocationProc: Option<
        unsafe extern "C" fn(
            inHostUserData: *mut c_void,
            outDeltaSampleOffsetToNextBeat: *mut u32,
            outTimeSig_Numerator: *mut f32,
            outTimeSig_Denominator: *mut u32,
            outCurrentMeasureDownBeat: *mut f64,
        ) -> OSStatus,
    >,
    pub transportStateProc: Option<
        unsafe extern "C" fn(
            inHostUserData: *mut c_void,
            outIsPlaying: *mut Boolean,
            outTransportStateChanged: *mut Boolean,
            outCurrentSampleInTimeLine: *mut f64,
            outIsCycling: *mut Boolean,
            outCycleStartBeat: *mut f64,
            outCycleEndBeat: *mut f64,
        ) -> OSStatus,
    >,
    pub transportStateProc2: Option<
        unsafe extern "C" fn(
            inHostUserData: *mut c_void,
            outIsPlaying: *mut Boolean,
            outIsRecording: *mut Boolean,
            outTransportStateChanged: *mut Boolean,
            outCurrentSampleInTimeLine: *mut f64,
            outIsCycling: *mut Boolean,
            outCycleStartBeat: *mut f64,
            outCycleEndBeat: *mut f64,
        ) -> OSStatus,
    >,
}

// --- The functions ---------------------------------------------------------

/// Declares the functions the wrapper calls: CoreFoundation's and
/// AudioToolbox's own on macOS, and on other systems the stand-ins of
/// `fake_cf`, checked to have the same signatures.
macro_rules! system_functions {
    ($library:literal { $(fn $name:ident($($arg:ident: $ty:ty),* $(,)?) $(-> $ret:ty)?;)* }) => {
        #[cfg(target_os = "macos")]
        #[link(name = $library, kind = "framework")]
        unsafe extern "C" {
            $(pub fn $name($($arg: $ty),*) $(-> $ret)?;)*
        }
        #[cfg(not(target_os = "macos"))]
        pub use super::fake_cf::{$($name),*};
        #[cfg(not(target_os = "macos"))]
        const _: () = {
            $(let _: unsafe fn($($ty),*) $(-> $ret)? = super::fake_cf::$name;)*
        };
    };
}

system_functions!("CoreFoundation" {
    fn CFRelease(cf: CFTypeRef);
    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    fn CFGetTypeID(cf: CFTypeRef) -> CFTypeID;
    fn CFStringGetTypeID() -> CFTypeID;
    fn CFDataGetTypeID() -> CFTypeID;
    fn CFNumberGetTypeID() -> CFTypeID;
    fn CFDictionaryGetTypeID() -> CFTypeID;
    fn CFStringCreateWithBytes(
        alloc: CFAllocatorRef,
        bytes: *const u8,
        numBytes: CFIndex,
        encoding: CFStringEncoding,
        isExternalRepresentation: Boolean,
    ) -> CFStringRef;
    fn CFStringGetLength(theString: CFStringRef) -> CFIndex;
    fn CFStringGetMaximumSizeForEncoding(length: CFIndex, encoding: CFStringEncoding) -> CFIndex;
    fn CFStringGetCString(
        theString: CFStringRef,
        buffer: *mut c_char,
        bufferSize: CFIndex,
        encoding: CFStringEncoding,
    ) -> Boolean;
    fn CFDataCreate(alloc: CFAllocatorRef, bytes: *const u8, length: CFIndex) -> CFDataRef;
    fn CFDataGetLength(theData: CFDataRef) -> CFIndex;
    fn CFDataGetBytePtr(theData: CFDataRef) -> *const u8;
    fn CFNumberCreate(
        allocator: CFAllocatorRef,
        theType: CFNumberType,
        valuePtr: *const c_void,
    ) -> CFNumberRef;
    fn CFNumberGetValue(number: CFNumberRef, theType: CFNumberType, valuePtr: *mut c_void) -> Boolean;
    fn CFDictionaryCreateMutable(
        allocator: CFAllocatorRef,
        capacity: CFIndex,
        keyCallBacks: *const CFDictionaryKeyCallBacks,
        valueCallBacks: *const CFDictionaryValueCallBacks,
    ) -> CFMutableDictionaryRef;
    fn CFDictionarySetValue(theDict: CFMutableDictionaryRef, key: *const c_void, value: *const c_void);
    fn CFDictionaryGetValue(theDict: CFDictionaryRef, key: *const c_void) -> *const c_void;
    fn CFArrayCreate(
        allocator: CFAllocatorRef,
        values: *const *const c_void,
        numValues: CFIndex,
        callBacks: *const CFArrayCallBacks,
    ) -> CFArrayRef;
});

system_functions!("AudioToolbox" {
    fn AudioUnitRender(
        inUnit: AudioUnit,
        ioActionFlags: *mut AudioUnitRenderActionFlags,
        inTimeStamp: *const AudioTimeStamp,
        inOutputBusNumber: u32,
        inNumberFrames: u32,
        ioData: *mut AudioBufferList,
    ) -> OSStatus;
});

/// Something only ever reached through its address.
#[cfg(target_os = "macos")]
#[repr(C)]
pub struct Opaque {
    _private: [u8; 0],
}

// CoreFoundation defines these for the life of the process and never
// changes them, so naming them is safe; only their addresses are taken,
// and they are declared opaque so that nothing can read them here.
#[cfg(target_os = "macos")]
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    safe static kCFTypeDictionaryKeyCallBacks: Opaque;
    safe static kCFTypeDictionaryValueCallBacks: Opaque;
    safe static kCFTypeArrayCallBacks: Opaque;
}

/// The key callbacks of a dictionary of CoreFoundation objects.
#[cfg(target_os = "macos")]
pub fn type_dictionary_key_callbacks() -> *const CFDictionaryKeyCallBacks {
    (&raw const kCFTypeDictionaryKeyCallBacks).cast()
}

/// The value callbacks of a dictionary of CoreFoundation objects.
#[cfg(target_os = "macos")]
pub fn type_dictionary_value_callbacks() -> *const CFDictionaryValueCallBacks {
    (&raw const kCFTypeDictionaryValueCallBacks).cast()
}

/// The callbacks of an array of CoreFoundation objects.
#[cfg(target_os = "macos")]
pub fn type_array_callbacks() -> *const CFArrayCallBacks {
    (&raw const kCFTypeArrayCallBacks).cast()
}

#[cfg(not(target_os = "macos"))]
pub use super::fake_cf::{
    type_array_callbacks, type_dictionary_key_callbacks, type_dictionary_value_callbacks,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// The sizes and offsets the headers give on 64-bit systems.
    #[test]
    fn structures_have_the_c_layout() {
        assert_eq!(size_of::<AudioComponentDescription>(), 20);
        assert_eq!(size_of::<AudioStreamBasicDescription>(), 40);
        assert_eq!(size_of::<AudioBuffer>(), 16);
        assert_eq!(AUDIO_BUFFER_LIST_BUFFERS, 8);
        assert_eq!(size_of::<SMPTETime>(), 24);
        assert_eq!(size_of::<AudioTimeStamp>(), 64);
        assert_eq!(size_of::<AURenderCallbackStruct>(), 16);
        assert_eq!(size_of::<AudioUnitConnection>(), 16);
        assert_eq!(size_of::<AUChannelInfo>(), 4);
        assert_eq!(size_of::<AudioChannelLayout>(), CHANNEL_LAYOUT_DESCRIPTIONS);
        assert_eq!(size_of::<AudioUnitParameterInfo>(), 104);
        assert_eq!(std::mem::offset_of!(AudioUnitParameterInfo, unitName), 56);
        assert_eq!(
            std::mem::offset_of!(AudioUnitParameterInfo, cfNameString),
            72
        );
        assert_eq!(std::mem::offset_of!(AudioUnitParameterInfo, flags), 96);
        assert_eq!(size_of::<AudioUnitParameterStringFromValue>(), 24);
        assert_eq!(size_of::<AudioUnitParameterValueFromString>(), 24);
        assert_eq!(size_of::<AudioUnitParameterEvent>(), 32);
        assert_eq!(size_of::<HostCallbackInfo>(), 40);
        assert_eq!(size_of::<AUPreset>(), 16);
        assert_eq!(size_of::<AUMIDIOutputCallbackStruct>(), 16);
        assert_eq!(size_of::<AudioComponentPlugInInterface>(), 32);
        assert_eq!(kAudioFormatLinearPCM, 0x6C70_636D);
    }
}
