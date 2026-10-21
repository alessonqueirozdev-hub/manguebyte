//! The parts of Apple's C interfaces the Audio Unit host uses.
//!
//! The types, the selectors, the property identifiers and the
//! CoreFoundation calls are the framework's AU wrapper's own
//! (`manguebyte_au::sys`), written from Apple's headers, and are shared
//! here rather than declared twice. On macOS its CoreFoundation functions
//! are CoreFoundation's; elsewhere they are the wrapper's stand-in, which
//! is what lets a unit be hosted through its factory and tested on every
//! system.
//!
//! What only a host calls is declared below, from the same headers of the
//! macOS SDK: `AudioComponent.h` (finding, naming and instantiating
//! components), `AUComponent.h` (`AudioUnitInitialize` and the other
//! calls a host makes on a unit), `MusicDevice.h`
//! (`MusicDeviceMIDIEvent`), `AudioUnitProperties.h` (component types,
//! parameter flags and units) and CoreFoundation's `CFPropertyList.h`.
//! Names keep their C spelling, so that they can be checked against the
//! headers line by line.

#![allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    unused_imports
)]

use std::ffi::c_void;

pub(crate) use manguebyte_au::sys::*;

/// A four-character code as `OSType` holds it: the first character in the
/// high byte.
pub(crate) const fn code(text: &[u8; 4]) -> OSType {
    u32::from_be_bytes(*text)
}

// --- Component types (`AudioUnitProperties.h`, "Audio Unit Types") ---------

pub(crate) const kAudioUnitType_Output: OSType = code(b"auou");
pub(crate) const kAudioUnitType_MusicDevice: OSType = code(b"aumu");
pub(crate) const kAudioUnitType_MusicEffect: OSType = code(b"aumf");
pub(crate) const kAudioUnitType_FormatConverter: OSType = code(b"aufc");
pub(crate) const kAudioUnitType_Effect: OSType = code(b"aufx");
pub(crate) const kAudioUnitType_Mixer: OSType = code(b"aumx");
pub(crate) const kAudioUnitType_Panner: OSType = code(b"aupn");
pub(crate) const kAudioUnitType_Generator: OSType = code(b"augn");
pub(crate) const kAudioUnitType_OfflineEffect: OSType = code(b"auol");
pub(crate) const kAudioUnitType_MIDIProcessor: OSType = code(b"aumi");

// --- Parameters (`AudioUnitProperties.h`) ------------------------------------

pub(crate) const kAudioUnitParameterFlag_MeterReadOnly: u32 = 1 << 15;
pub(crate) const kAudioUnitParameterFlag_NonRealTime: u32 = 1 << 24;
pub(crate) const kAudioUnitParameterFlag_ExpertMode: u32 = 1 << 26;

pub(crate) const kAudioUnitParameterUnit_SampleFrames: u32 = 5;
pub(crate) const kAudioUnitParameterUnit_Phase: u32 = 6;
pub(crate) const kAudioUnitParameterUnit_Rate: u32 = 7;
pub(crate) const kAudioUnitParameterUnit_MIDINoteNumber: u32 = 11;
pub(crate) const kAudioUnitParameterUnit_MIDIController: u32 = 12;
pub(crate) const kAudioUnitParameterUnit_LinearGain: u32 = 14;
pub(crate) const kAudioUnitParameterUnit_Pan: u32 = 18;
pub(crate) const kAudioUnitParameterUnit_Meters: u32 = 19;
pub(crate) const kAudioUnitParameterUnit_AbsoluteCents: u32 = 20;

// --- Time stamps (`CoreAudioBaseTypes.h`) ----------------------------------------

pub(crate) const kAudioTimeStampSampleTimeValid: u32 = 1 << 0;

// --- CoreFoundation property lists (`CFPropertyList.h`) ---------------------

/// `kCFPropertyListXMLFormat_v1_0`.
pub(crate) const kCFPropertyListXMLFormat_v1_0: CFIndex = 100;
/// `kCFPropertyListImmutable`.
pub(crate) const kCFPropertyListImmutable: usize = 0;

/// `AudioComponent`: a registered component, which no code of its own has
/// run for yet.
pub(crate) type AudioComponent = *mut c_void;

/// `AudioComponentFactoryFunction`: what a component's bundle exports, and
/// what makes an instance's `AudioComponentPlugInInterface`.
pub(crate) type AudioComponentFactoryFunction =
    unsafe extern "C" fn(inDesc: *const c_void) -> *mut c_void;

// --- What a host calls on macOS ------------------------------------------------------

#[cfg(target_os = "macos")]
#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    // AudioComponent.h
    pub(crate) fn AudioComponentFindNext(
        inComponent: AudioComponent,
        inDesc: *const AudioComponentDescription,
    ) -> AudioComponent;
    pub(crate) fn AudioComponentCopyName(
        inComponent: AudioComponent,
        outName: *mut CFStringRef,
    ) -> OSStatus;
    pub(crate) fn AudioComponentGetDescription(
        inComponent: AudioComponent,
        outDesc: *mut AudioComponentDescription,
    ) -> OSStatus;
    pub(crate) fn AudioComponentGetVersion(
        inComponent: AudioComponent,
        outVersion: *mut u32,
    ) -> OSStatus;
    pub(crate) fn AudioComponentInstanceNew(
        inComponent: AudioComponent,
        outInstance: *mut AudioComponentInstance,
    ) -> OSStatus;
    pub(crate) fn AudioComponentInstanceDispose(inInstance: AudioComponentInstance) -> OSStatus;

    // AUComponent.h
    pub(crate) fn AudioUnitInitialize(inUnit: AudioUnit) -> OSStatus;
    pub(crate) fn AudioUnitUninitialize(inUnit: AudioUnit) -> OSStatus;
    pub(crate) fn AudioUnitGetPropertyInfo(
        inUnit: AudioUnit,
        inID: AudioUnitPropertyID,
        inScope: AudioUnitScope,
        inElement: AudioUnitElement,
        outDataSize: *mut u32,
        outWritable: *mut Boolean,
    ) -> OSStatus;
    pub(crate) fn AudioUnitGetProperty(
        inUnit: AudioUnit,
        inID: AudioUnitPropertyID,
        inScope: AudioUnitScope,
        inElement: AudioUnitElement,
        outData: *mut c_void,
        ioDataSize: *mut u32,
    ) -> OSStatus;
    pub(crate) fn AudioUnitSetProperty(
        inUnit: AudioUnit,
        inID: AudioUnitPropertyID,
        inScope: AudioUnitScope,
        inElement: AudioUnitElement,
        inData: *const c_void,
        inDataSize: u32,
    ) -> OSStatus;
    pub(crate) fn AudioUnitGetParameter(
        inUnit: AudioUnit,
        inID: AudioUnitParameterID,
        inScope: AudioUnitScope,
        inElement: AudioUnitElement,
        outValue: *mut AudioUnitParameterValue,
    ) -> OSStatus;
    pub(crate) fn AudioUnitSetParameter(
        inUnit: AudioUnit,
        inID: AudioUnitParameterID,
        inScope: AudioUnitScope,
        inElement: AudioUnitElement,
        inValue: AudioUnitParameterValue,
        inBufferOffsetInFrames: u32,
    ) -> OSStatus;
    pub(crate) fn AudioUnitScheduleParameters(
        inUnit: AudioUnit,
        inParameterEvent: *const AudioUnitParameterEvent,
        inNumParamEvents: u32,
    ) -> OSStatus;
    pub(crate) fn AudioUnitReset(
        inUnit: AudioUnit,
        inScope: AudioUnitScope,
        inElement: AudioUnitElement,
    ) -> OSStatus;

    // MusicDevice.h
    pub(crate) fn MusicDeviceMIDIEvent(
        inUnit: AudioUnit,
        inStatus: u32,
        inData1: u32,
        inData2: u32,
        inOffsetSampleFrame: u32,
    ) -> OSStatus;
}

#[cfg(target_os = "macos")]
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub(crate) fn CFPropertyListCreateData(
        allocator: CFAllocatorRef,
        propertyList: CFPropertyListRef,
        format: CFIndex,
        options: usize,
        error: *mut *const c_void,
    ) -> CFDataRef;
    pub(crate) fn CFPropertyListCreateWithData(
        allocator: CFAllocatorRef,
        data: CFDataRef,
        options: usize,
        format: *mut CFIndex,
        error: *mut *const c_void,
    ) -> CFPropertyListRef;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_the_headers_numbers() {
        assert_eq!(kAudioUnitType_MusicDevice, 0x6175_6D75);
        assert_eq!(kAudioUnitType_Effect, 0x6175_6678);
        assert_eq!(size_of::<AudioUnitParameterEvent>(), 32);
    }
}
