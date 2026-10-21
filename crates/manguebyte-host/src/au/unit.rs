//! One Audio Unit instance and the calls a host makes on it.
//!
//! A host reaches a unit's methods in one of two ways, and the rest of the
//! host does not care which:
//!
//! - Through AudioToolbox, on macOS: `AudioComponentInstanceNew` makes the
//!   instance, and `AudioUnitGetProperty`, `AudioUnitRender` and the other
//!   calls of `AUComponent.h` dispatch to it, wherever it lives.
//! - Through the instance's own `AudioComponentPlugInInterface`, on any
//!   system: the component's factory function makes it, `Open` hands it the
//!   instance, and `Lookup` gives the method for each selector, which is
//!   called with the interface as its first argument. This is what
//!   AudioToolbox itself does with a version 2 unit in the host's process
//!   (`AudioComponent.h`, "AudioComponentPlugInInterface").
//!
//! Either way a method is a C function whose first argument is the unit,
//! so both are the same table of function pointers here.

use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::ptr;

use super::sys::*;
use super::{AuError, Status};

type Simple = unsafe extern "C" fn(*mut c_void) -> OSStatus;
type GetPropertyInfo =
    unsafe extern "C" fn(*mut c_void, u32, u32, u32, *mut u32, *mut Boolean) -> OSStatus;
type GetProperty =
    unsafe extern "C" fn(*mut c_void, u32, u32, u32, *mut c_void, *mut u32) -> OSStatus;
type SetProperty = unsafe extern "C" fn(*mut c_void, u32, u32, u32, *const c_void, u32) -> OSStatus;
type GetParameter = unsafe extern "C" fn(*mut c_void, u32, u32, u32, *mut f32) -> OSStatus;
type SetParameter = unsafe extern "C" fn(*mut c_void, u32, u32, u32, f32, u32) -> OSStatus;
type ScheduleParameters =
    unsafe extern "C" fn(*mut c_void, *const AudioUnitParameterEvent, u32) -> OSStatus;
type Render = unsafe extern "C" fn(
    *mut c_void,
    *mut AudioUnitRenderActionFlags,
    *const AudioTimeStamp,
    u32,
    u32,
    *mut AudioBufferList,
) -> OSStatus;
type Reset = unsafe extern "C" fn(*mut c_void, u32, u32) -> OSStatus;
type MidiEvent = unsafe extern "C" fn(*mut c_void, u32, u32, u32, u32) -> OSStatus;

/// The methods of a unit.
#[derive(Clone, Copy)]
struct Methods {
    initialize: Simple,
    uninitialize: Simple,
    get_property_info: Option<GetPropertyInfo>,
    get_property: GetProperty,
    set_property: SetProperty,
    get_parameter: GetParameter,
    set_parameter: SetParameter,
    schedule_parameters: Option<ScheduleParameters>,
    render: Render,
    reset: Option<Reset>,
    midi_event: Option<MidiEvent>,
}

/// Who made the instance, and so how it is put away.
enum Owner {
    /// A factory function, in this process: closed through its interface.
    /// The token is what the unit was told is its instance.
    Factory { close: Simple, _token: Box<u64> },
    /// AudioToolbox: disposed of through it.
    #[cfg(target_os = "macos")]
    System,
}

/// One instance of a unit, with its methods. Put away when dropped.
pub(crate) struct Unit {
    this: *mut c_void,
    methods: Methods,
    owner: Owner,
}

// SAFETY: an Audio Unit's calls may come from any thread: Apple's
// conventions keep the render calls (`AudioUnitRender`,
// `AudioUnitScheduleParameters`, `MusicDeviceMIDIEvent`) on one thread at
// a time and the others off it, which the plugin and processor types that
// hold a unit keep to.
unsafe impl Send for Unit {}
// SAFETY: as above.
unsafe impl Sync for Unit {}

/// The method of `selector`, as the type the host calls it by, or `None`.
///
/// # Safety
///
/// `lookup` must be a unit's `Lookup`, and `T` the type of the method for
/// `selector` as `AUComponent.h` or `MusicDevice.h` gives it.
unsafe fn method<T: Copy>(
    lookup: unsafe extern "C" fn(i16) -> *const c_void,
    selector: i16,
) -> Option<T> {
    // SAFETY: the unit's own `Lookup`.
    let found = unsafe { lookup(selector) };
    if found.is_null() {
        return None;
    }
    debug_assert_eq!(size_of::<T>(), size_of::<*const c_void>());
    // SAFETY: a function pointer, which the host casts to the method's own
    // type before calling it, as AudioToolbox does.
    Some(unsafe { std::mem::transmute_copy::<*const c_void, T>(&found) })
}

impl Unit {
    /// Makes an instance with a component's factory function, in this
    /// process, opens it and looks its methods up.
    ///
    /// # Safety
    ///
    /// `factory` must be an `AudioComponentFactoryFunction` that returns
    /// null or an `AudioComponentPlugInInterface` whose methods keep to
    /// Apple's interface, for the description given.
    pub(crate) unsafe fn from_factory(
        factory: AudioComponentFactoryFunction,
        description: &AudioComponentDescription,
    ) -> Result<Self, AuError> {
        // SAFETY: per this function's contract.
        let this = unsafe { factory(ptr::from_ref(description).cast()) };
        if this.is_null() || !this.cast::<AudioComponentPlugInInterface>().is_aligned() {
            return Err(AuError::InstantiateFailed(None));
        }
        // SAFETY: the factory returned an interface.
        let interface = unsafe { &*this.cast::<AudioComponentPlugInInterface>() };
        let close = interface.Close;
        let token = Box::new(0u64);
        // The unit is told its instance is the token: it hands it back to
        // its listeners, and nothing here takes it for an AudioToolbox
        // instance.
        let instance = ptr::from_ref::<u64>(&token).cast_mut().cast::<c_void>();
        // SAFETY: the interface's own `Open`, once, as AudioToolbox calls it.
        let status = unsafe { (interface.Open)(this, instance) };
        if status != noErr {
            // SAFETY: the interface's own `Close`, which frees it.
            unsafe { close(this) };
            return Err(AuError::InstantiateFailed(Some(status)));
        }
        let lookup = interface.Lookup;
        // SAFETY: the unit's `Lookup`, with each selector's own type.
        let methods = unsafe {
            (|| {
                Some(Methods {
                    initialize: method(lookup, kAudioUnitInitializeSelect)?,
                    uninitialize: method(lookup, kAudioUnitUninitializeSelect)?,
                    get_property_info: method(lookup, kAudioUnitGetPropertyInfoSelect),
                    get_property: method(lookup, kAudioUnitGetPropertySelect)?,
                    set_property: method(lookup, kAudioUnitSetPropertySelect)?,
                    get_parameter: method(lookup, kAudioUnitGetParameterSelect)?,
                    set_parameter: method(lookup, kAudioUnitSetParameterSelect)?,
                    schedule_parameters: method(lookup, kAudioUnitScheduleParametersSelect),
                    render: method(lookup, kAudioUnitRenderSelect)?,
                    reset: method(lookup, kAudioUnitResetSelect),
                    midi_event: method(lookup, kMusicDeviceMIDIEventSelect),
                })
            })()
        };
        let owner = Owner::Factory {
            close,
            _token: token,
        };
        match methods {
            Some(methods) => Ok(Self {
                this,
                methods,
                owner,
            }),
            None => {
                // SAFETY: as above.
                unsafe { close(this) };
                Err(AuError::Unsupported(
                    "the unit lacks a method every unit has",
                ))
            }
        }
    }

    /// Makes an instance of a registered component through AudioToolbox.
    ///
    /// # Safety
    ///
    /// `component` must be a component AudioToolbox returned, and its code
    /// must keep to Apple's interface: making an instance runs it.
    #[cfg(target_os = "macos")]
    pub(crate) unsafe fn from_component(
        component: AudioComponent,
        takes_midi: bool,
    ) -> Result<Self, AuError> {
        let mut this: AudioComponentInstance = ptr::null_mut();
        // SAFETY: a registered component and a place for the instance.
        let status = unsafe { AudioComponentInstanceNew(component, &mut this) };
        if status != noErr || this.is_null() {
            return Err(AuError::InstantiateFailed(Some(status)));
        }
        let methods = Methods {
            initialize: AudioUnitInitialize,
            uninitialize: AudioUnitUninitialize,
            get_property_info: Some(AudioUnitGetPropertyInfo),
            get_property: AudioUnitGetProperty,
            set_property: AudioUnitSetProperty,
            get_parameter: AudioUnitGetParameter,
            set_parameter: AudioUnitSetParameter,
            schedule_parameters: Some(AudioUnitScheduleParameters),
            render: AudioUnitRender,
            reset: Some(AudioUnitReset),
            // A unit that is not a music device answers this selector with
            // an error; it is not called at all.
            midi_event: takes_midi.then_some(MusicDeviceMIDIEvent as MidiEvent),
        };
        Ok(Self {
            this,
            methods,
            owner: Owner::System,
        })
    }

    /// Whether the unit has `MusicDeviceMIDIEvent`.
    pub(crate) fn takes_midi(&self) -> bool {
        self.methods.midi_event.is_some()
    }

    pub(crate) fn initialize(&self) -> Status {
        // SAFETY: the unit's own method, with the unit.
        check(unsafe { (self.methods.initialize)(self.this) })
    }

    pub(crate) fn uninitialize(&self) -> Status {
        // SAFETY: as above.
        check(unsafe { (self.methods.uninitialize)(self.this) })
    }

    /// The size of a property and whether it can be set.
    pub(crate) fn property_info(
        &self,
        id: AudioUnitPropertyID,
        scope: AudioUnitScope,
        element: AudioUnitElement,
    ) -> Result<(u32, bool), OSStatus> {
        let Some(info) = self.methods.get_property_info else {
            return Err(kAudioUnitErr_InvalidProperty);
        };
        let (mut size, mut writable) = (0u32, 0u8);
        // SAFETY: the unit's own method, with places for its answers.
        let status = unsafe { info(self.this, id, scope, element, &mut size, &mut writable) };
        check(status).map(|()| (size, writable != 0))
    }

    /// Reads a property of fixed size into `out`, which it must fill.
    fn get_into<T: Copy>(
        &self,
        id: AudioUnitPropertyID,
        scope: AudioUnitScope,
        element: AudioUnitElement,
        out: *mut T,
    ) -> Status {
        let mut size = size_of::<T>() as u32;
        // SAFETY: the unit's own method, with room for a `T`.
        let status = unsafe {
            (self.methods.get_property)(self.this, id, scope, element, out.cast(), &mut size)
        };
        check(status)?;
        if size as usize == size_of::<T>() {
            Ok(())
        } else {
            Err(kAudioUnitErr_InvalidPropertyValue)
        }
    }

    /// Reads a property whose value is a `T`.
    ///
    /// `T` must be plain data, for which every pattern of bytes is a value.
    pub(crate) fn get<T: Copy>(
        &self,
        id: AudioUnitPropertyID,
        scope: AudioUnitScope,
        element: AudioUnitElement,
    ) -> Result<T, OSStatus> {
        let mut value = MaybeUninit::<T>::zeroed();
        self.get_into(id, scope, element, value.as_mut_ptr())?;
        // SAFETY: zeroed, then filled by the unit; `T` is plain data.
        Ok(unsafe { value.assume_init() })
    }

    /// Reads a property that is a list of `T`s, as long as the unit says.
    ///
    /// `T` must be plain data, for which every pattern of bytes is a value.
    pub(crate) fn get_list<T: Copy>(
        &self,
        id: AudioUnitPropertyID,
        scope: AudioUnitScope,
        element: AudioUnitElement,
        most: usize,
    ) -> Result<Vec<T>, OSStatus> {
        let (bytes, _) = self.property_info(id, scope, element)?;
        let count = (bytes as usize / size_of::<T>().max(1)).min(most);
        if count == 0 {
            return Ok(Vec::new());
        }
        // SAFETY: `T` is plain data, for which zero bytes are a value.
        let mut values = vec![unsafe { std::mem::zeroed::<T>() }; count];
        let mut size = (count * size_of::<T>()) as u32;
        // SAFETY: the unit's own method, with room for `count` values.
        let status = unsafe {
            (self.methods.get_property)(
                self.this,
                id,
                scope,
                element,
                values.as_mut_ptr().cast(),
                &mut size,
            )
        };
        check(status)?;
        values.truncate(size as usize / size_of::<T>().max(1));
        Ok(values)
    }

    /// Asks a property whose value is a structure the unit fills in from
    /// what the host put in it, such as
    /// `kAudioUnitProperty_ParameterStringFromValue`.
    pub(crate) fn query<T: Copy>(
        &self,
        id: AudioUnitPropertyID,
        scope: AudioUnitScope,
        element: AudioUnitElement,
        mut value: T,
    ) -> Result<T, OSStatus> {
        self.get_into(id, scope, element, &mut value)?;
        Ok(value)
    }

    /// Sets a property to `value`.
    pub(crate) fn set<T>(
        &self,
        id: AudioUnitPropertyID,
        scope: AudioUnitScope,
        element: AudioUnitElement,
        value: &T,
    ) -> Status {
        // SAFETY: the unit's own method, with a value of the size given.
        check(unsafe {
            (self.methods.set_property)(
                self.this,
                id,
                scope,
                element,
                ptr::from_ref(value).cast(),
                size_of::<T>() as u32,
            )
        })
    }

    pub(crate) fn parameter(&self, id: AudioUnitParameterID) -> Result<f32, OSStatus> {
        let mut value = 0.0f32;
        // SAFETY: the unit's own method, with a place for the value.
        let status = unsafe {
            (self.methods.get_parameter)(self.this, id, kAudioUnitScope_Global, 0, &mut value)
        };
        check(status).map(|()| value)
    }

    pub(crate) fn set_parameter(&self, id: AudioUnitParameterID, value: f32) -> Status {
        // SAFETY: the unit's own method.
        check(unsafe {
            (self.methods.set_parameter)(self.this, id, kAudioUnitScope_Global, 0, value, 0)
        })
    }

    /// Schedules parameter changes for the next render. Falls back on
    /// `SetParameter`, which the unit hears from the next render's start,
    /// for a unit without `ScheduleParameters`.
    pub(crate) fn schedule(&self, event: &AudioUnitParameterEvent) -> Status {
        match self.methods.schedule_parameters {
            // SAFETY: the unit's own method, with one event.
            Some(schedule) => check(unsafe { schedule(self.this, event, 1) }),
            None => {
                let value = f32::from_bits(event.eventValues[1]);
                self.set_parameter(event.parameter, value)
            }
        }
    }

    /// Sends a MIDI message for the next render, on its frame.
    pub(crate) fn midi(&self, bytes: [u8; 3], offset: u32) -> Status {
        let Some(midi) = self.methods.midi_event else {
            return Err(badComponentSelector);
        };
        // SAFETY: the unit's own method.
        check(unsafe {
            midi(
                self.this,
                u32::from(bytes[0]),
                u32::from(bytes[1]),
                u32::from(bytes[2]),
                offset,
            )
        })
    }

    /// Renders output element 0.
    ///
    /// # Safety
    ///
    /// `list` must hold as many buffers as it says, each with
    /// `mDataByteSize` writable bytes at `mData`, and the unit must be
    /// initialised for at least `frames` frames.
    pub(crate) unsafe fn render(
        &self,
        flags: &mut AudioUnitRenderActionFlags,
        timestamp: &AudioTimeStamp,
        frames: u32,
        list: *mut AudioBufferList,
    ) -> Status {
        // SAFETY: the unit's own method, with the host's arguments.
        check(unsafe { (self.methods.render)(self.this, flags, timestamp, 0, frames, list) })
    }

    pub(crate) fn reset(&self) -> Status {
        match self.methods.reset {
            // SAFETY: the unit's own method.
            Some(reset) => check(unsafe { reset(self.this, kAudioUnitScope_Global, 0) }),
            None => Ok(()),
        }
    }
}

impl Drop for Unit {
    fn drop(&mut self) {
        // What a unit answers while being put away cannot change anything.
        match &self.owner {
            // SAFETY: the interface's own `Close`, once, when nothing else
            // calls the unit.
            Owner::Factory { close, .. } => unsafe {
                close(self.this);
            },
            // SAFETY: an instance AudioToolbox made, disposed of once.
            #[cfg(target_os = "macos")]
            Owner::System => unsafe {
                AudioComponentInstanceDispose(self.this);
            },
        }
    }
}

/// `noErr` as `Ok`, anything else as the status.
pub(crate) fn check(status: OSStatus) -> Status {
    if status == noErr { Ok(()) } else { Err(status) }
}
