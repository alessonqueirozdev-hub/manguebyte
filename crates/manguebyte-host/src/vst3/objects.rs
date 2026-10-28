//! The host as a VST 3 plugin sees it, away from the audio thread.
//!
//! A plugin is handed objects of the host's to call back: the host
//! application when it is initialised, a component handler to report edits
//! to, streams to save into and load from, and messages for its two halves
//! to send each other. VST 3 has all of these used on the main thread.
//!
//! Everything in them that changes sits behind a mutex or in an atomic, so
//! a plugin that calls from a thread it should not is slow or late, and
//! never a data race. As in the CLAP host, the callbacks do not act: they
//! record, and [`Vst3Plugin::pump`](super::Vst3Plugin::pump) acts later, on
//! the main thread and outside any call into the plugin.

// The bindings give enumerations a different integer type on Windows than
// elsewhere, so a cast that is needed on one platform is redundant on the
// other.
#![allow(clippy::unnecessary_cast)]

use std::ffi::{CStr, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use vst3::Steinberg::IBStream_::IStreamSeekMode_;
use vst3::Steinberg::Vst::IAttributeList_::AttrID;
use vst3::Steinberg::Vst::{
    self as vst, IAttributeList, IAttributeListTrait, IAudioProcessor, IComponent,
    IComponentHandler, IComponentHandler2, IComponentHandler2Trait, IComponentHandlerTrait,
    IConnectionPoint, IEditController, IHostApplication, IHostApplicationTrait, IMessage,
    IMessageTrait, IMidiMapping, IPlugInterfaceSupport, IPlugInterfaceSupportTrait,
};
use vst3::Steinberg::{
    FIDString, FUnknown, IBStream, IBStreamTrait, IPlugView, IPlugViewContentScaleSupport, TBool,
    TUID, kInvalidArgument, kNotImplemented, kResultFalse, kResultOk, kResultTrue, tresult,
};
use vst3::{Class, ComWrapper, Interface};

use super::text::{bounded_narrow, bounded_wide, copy_wide};
use super::{MAX_STATE_BYTES, MAX_TEXT_UNITS};
use crate::threads::{guard, thread_token};

/// Most attributes one message may carry.
const MAX_ATTRIBUTES: usize = 1_024;

/// Most bytes of text and binary data one message may carry, counting what
/// was replaced, which is kept until the message is released.
const MAX_ATTRIBUTE_BYTES: usize = 16 << 20;

/// Longest attribute name and longest message identifier, in bytes.
const MAX_KEY_BYTES: usize = 256;

/// How many times a message's identifier may be set. Each one is kept until
/// the message is released, because the plugin may hold a pointer to it.
const MAX_MESSAGE_IDS: usize = 64;

/// Most edits kept between two calls to `pump`.
const MAX_EDITS: usize = 4_096;

/// Locks a mutex, whatever became of the last thread that held it.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Reads an interface or class identifier a plugin passed.
///
/// # Safety
///
/// `pointer` must be null or point to 16 readable bytes.
unsafe fn read_id(pointer: *const TUID) -> Option<[u8; 16]> {
    if pointer.is_null() {
        return None;
    }
    // SAFETY: not null, 16 bytes per the contract, and bytes have no
    // alignment to get wrong.
    Some(unsafe { pointer.read() }.map(|unit| unit as u8))
}

// --- The host application -------------------------------------------------

/// What a plugin is told about the program hosting it, and where it gets
/// the messages its two halves exchange.
pub(crate) struct HostApplication {
    name: String,
}

impl HostApplication {
    pub(crate) fn new(name: &str) -> ComWrapper<Self> {
        ComWrapper::new(Self {
            name: name.to_owned(),
        })
    }
}

impl Class for HostApplication {
    type Interfaces = (IHostApplication, IPlugInterfaceSupport);
}

/// The pointer a plugin is given as its host context.
pub(crate) fn context_ptr(host: &ComWrapper<HostApplication>) -> *mut FUnknown {
    host.as_com_ref::<IHostApplication>()
        .map_or(ptr::null_mut(), |host| host.as_ptr().cast())
}

impl IHostApplicationTrait for HostApplication {
    unsafe fn getName(&self, name: *mut vst::String128) -> tresult {
        // SAFETY: a buffer the plugin owns, or null.
        let Some(name) = (unsafe { name.as_mut() }) else {
            return kInvalidArgument;
        };
        copy_wide(&self.name, name);
        kResultOk
    }

    unsafe fn createInstance(
        &self,
        cid: *mut TUID,
        iid: *mut TUID,
        obj: *mut *mut c_void,
    ) -> tresult {
        guard(kResultFalse, || {
            if obj.is_null() || !obj.is_aligned() {
                return kInvalidArgument;
            }
            // SAFETY: the plugin's own pointer, checked above, which is to
            // hold nothing if this fails.
            unsafe { obj.write(ptr::null_mut()) };
            // SAFETY: the plugin passes two 16-byte identifiers, or null.
            let (Some(class), Some(interface)) = (unsafe { read_id(cid) }, unsafe { read_id(iid) })
            else {
                return kInvalidArgument;
            };
            // The two kinds of object a plugin may ask its host to make. The
            // first interface of each is at the object's own address, which
            // is also where a plugin that asks for `FUnknown` finds it.
            let wanted =
                |own: [u8; 16]| class == own && (interface == own || interface == FUnknown::IID);
            let created: Option<*mut c_void> = if wanted(IMessage::IID) {
                ComWrapper::new(Message::new())
                    .to_com_ptr::<IMessage>()
                    .map(|message| message.into_raw().cast())
            } else if wanted(IAttributeList::IID) {
                ComWrapper::new(AttributeList::new())
                    .to_com_ptr::<IAttributeList>()
                    .map(|list| list.into_raw().cast())
            } else {
                None
            };
            match created {
                Some(object) => {
                    // SAFETY: as above. The plugin now owns one reference.
                    unsafe { obj.write(object) };
                    kResultOk
                }
                None => kNotImplemented,
            }
        })
    }
}

impl IPlugInterfaceSupportTrait for HostApplication {
    unsafe fn isPlugInterfaceSupported(&self, iid: *const TUID) -> tresult {
        // SAFETY: the plugin passes a 16-byte identifier, or null.
        let Some(asked) = (unsafe { read_id(iid) }) else {
            return kInvalidArgument;
        };
        // The interfaces of a plugin that this host calls.
        let used = [
            IComponent::IID,
            IAudioProcessor::IID,
            IEditController::IID,
            IConnectionPoint::IID,
            IMidiMapping::IID,
            IPlugView::IID,
            IPlugViewContentScaleSupport::IID,
        ];
        if used.contains(&asked) {
            kResultTrue
        } else {
            kResultFalse
        }
    }
}

// --- Messages -------------------------------------------------------------

/// One value in a message.
enum Attribute {
    Int(i64),
    Float(f64),
    Text(Vec<u16>),
    // A `Vec`, not a `Box<[u8]>`: a pointer handed to a plugin must stay
    // valid when the value is replaced and moved to `retired`, and moving a
    // `Box` would invalidate it under Rust's aliasing rules.
    Binary(Vec<u8>),
}

/// What an attribute list holds.
#[derive(Default)]
struct Attributes {
    live: Vec<(Vec<u8>, Attribute)>,
    /// Binary values that were replaced. A plugin may still hold the
    /// pointer it was given to one, so they are kept until the list goes.
    retired: Vec<Vec<u8>>,
    /// Bytes of text and binary data held, the retired ones included.
    bytes: usize,
}

/// Named values a plugin puts in a message for its other half.
pub(crate) struct AttributeList {
    attributes: Mutex<Attributes>,
}

impl AttributeList {
    fn new() -> Self {
        Self {
            attributes: Mutex::new(Attributes::default()),
        }
    }

    /// Stores `value` under the name at `id`, in place of what was there.
    fn set(&self, id: AttrID, value: Attribute) -> tresult {
        // SAFETY: the plugin passes a NUL-terminated name, or null.
        let key = unsafe { bounded_narrow(id, MAX_KEY_BYTES) };
        if id.is_null() {
            return kInvalidArgument;
        }
        let size = match &value {
            Attribute::Text(units) => units.len() * 2,
            Attribute::Binary(bytes) => bytes.len(),
            Attribute::Int(_) | Attribute::Float(_) => 0,
        };
        let mut attributes = lock(&self.attributes);
        if attributes.bytes.saturating_add(size) > MAX_ATTRIBUTE_BYTES {
            return kResultFalse;
        }
        let slot = attributes.live.iter().position(|(name, _)| *name == key);
        match slot {
            Some(index) => {
                let old = std::mem::replace(&mut attributes.live[index].1, value);
                if let Attribute::Binary(bytes) = old {
                    attributes.retired.push(bytes);
                }
            }
            None if attributes.live.len() < MAX_ATTRIBUTES => attributes.live.push((key, value)),
            None => return kResultFalse,
        }
        attributes.bytes += size;
        kResultOk
    }

    /// Looks up the name at `id` and hands the value to `read`.
    fn get(&self, id: AttrID, read: impl FnOnce(&Attribute) -> tresult) -> tresult {
        if id.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: the plugin passes a NUL-terminated name.
        let key = unsafe { bounded_narrow(id, MAX_KEY_BYTES) };
        let attributes = lock(&self.attributes);
        match attributes.live.iter().find(|(name, _)| *name == key) {
            Some((_, value)) => read(value),
            None => kResultFalse,
        }
    }
}

impl Class for AttributeList {
    type Interfaces = (IAttributeList,);
}

impl IAttributeListTrait for AttributeList {
    unsafe fn setInt(&self, id: AttrID, value: i64) -> tresult {
        guard(kResultFalse, || self.set(id, Attribute::Int(value)))
    }

    unsafe fn getInt(&self, id: AttrID, value: *mut i64) -> tresult {
        guard(kResultFalse, || {
            self.get(id, |found| match found {
                Attribute::Int(number) if !value.is_null() && value.is_aligned() => {
                    // SAFETY: the plugin's own number, checked above.
                    unsafe { value.write(*number) };
                    kResultOk
                }
                _ => kResultFalse,
            })
        })
    }

    unsafe fn setFloat(&self, id: AttrID, value: f64) -> tresult {
        guard(kResultFalse, || self.set(id, Attribute::Float(value)))
    }

    unsafe fn getFloat(&self, id: AttrID, value: *mut f64) -> tresult {
        guard(kResultFalse, || {
            self.get(id, |found| match found {
                Attribute::Float(number) if !value.is_null() && value.is_aligned() => {
                    // SAFETY: the plugin's own number, checked above.
                    unsafe { value.write(*number) };
                    kResultOk
                }
                _ => kResultFalse,
            })
        })
    }

    unsafe fn setString(&self, id: AttrID, string: *const vst::TChar) -> tresult {
        guard(kResultFalse, || {
            if string.is_null() {
                return kInvalidArgument;
            }
            // SAFETY: the plugin passes zero-terminated text.
            let units = unsafe { bounded_wide(string, MAX_TEXT_UNITS) };
            self.set(id, Attribute::Text(units))
        })
    }

    unsafe fn getString(&self, id: AttrID, string: *mut vst::TChar, size_in_bytes: u32) -> tresult {
        guard(kResultFalse, || {
            let room = size_in_bytes as usize / 2;
            if string.is_null() || !string.is_aligned() || room == 0 {
                return kInvalidArgument;
            }
            self.get(id, |found| {
                let Attribute::Text(units) = found else {
                    return kResultFalse;
                };
                // SAFETY: the plugin offers `size_in_bytes` writable bytes
                // at `string`, which is `room` units, and nothing of this
                // host's overlaps the plugin's buffer.
                let out = unsafe { std::slice::from_raw_parts_mut(string, room) };
                let length = units.len().min(room - 1);
                out[..length].copy_from_slice(&units[..length]);
                out[length] = 0;
                kResultOk
            })
        })
    }

    unsafe fn setBinary(&self, id: AttrID, data: *const c_void, size_in_bytes: u32) -> tresult {
        guard(kResultFalse, || {
            let size = size_in_bytes as usize;
            if size > MAX_ATTRIBUTE_BYTES {
                return kResultFalse;
            }
            let bytes: Vec<u8> = if size == 0 {
                Vec::new()
            } else if data.is_null() {
                return kInvalidArgument;
            } else {
                // SAFETY: the plugin offers `size_in_bytes` readable bytes
                // at `data`.
                unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size) }.to_vec()
            };
            self.set(id, Attribute::Binary(bytes))
        })
    }

    unsafe fn getBinary(
        &self,
        id: AttrID,
        data: *mut *const c_void,
        size_in_bytes: *mut u32,
    ) -> tresult {
        guard(kResultFalse, || {
            if data.is_null() || !data.is_aligned() || size_in_bytes.is_null() {
                return kInvalidArgument;
            }
            self.get(id, |found| {
                let Attribute::Binary(bytes) = found else {
                    return kResultFalse;
                };
                // SAFETY: the plugin's own pointers, checked above. The
                // bytes they are pointed at stay where they are until this
                // list is released: a replaced value is retired, not freed.
                unsafe {
                    data.write(bytes.as_ptr().cast());
                    size_in_bytes.write_unaligned(bytes.len() as u32);
                }
                kResultOk
            })
        })
    }
}

/// A message from one half of a plugin to the other.
pub(crate) struct Message {
    /// Every identifier the message has had, the current one last.
    /// Each id with its terminating zero. A `Vec<u8>` rather than a
    /// `CString` (a `Box` inside): the list may grow and move its elements,
    /// and the pointers handed out earlier must stay valid.
    ids: Mutex<Vec<Vec<u8>>>,
    attributes: ComWrapper<AttributeList>,
}

impl Message {
    fn new() -> Self {
        Self {
            ids: Mutex::new(Vec::new()),
            attributes: ComWrapper::new(AttributeList::new()),
        }
    }
}

impl Class for Message {
    type Interfaces = (IMessage,);
}

impl IMessageTrait for Message {
    unsafe fn getMessageID(&self) -> FIDString {
        guard(ptr::null(), || {
            // Each id keeps its bytes on the heap, and none is removed while
            // the message lives, so the pointer outlasts the lock.
            lock(&self.ids)
                .last()
                .map_or(ptr::null(), |id| id.as_ptr().cast())
        })
    }

    unsafe fn setMessageID(&self, id: FIDString) {
        guard((), || {
            // SAFETY: the plugin passes a NUL-terminated identifier, or
            // null.
            let bytes = unsafe { bounded_narrow(id, MAX_KEY_BYTES) };
            let mut ids = lock(&self.ids);
            if ids.len() < MAX_MESSAGE_IDS {
                // The bytes were read up to their first NUL, so none is in
                // them; the terminator is added here.
                let mut id = bytes;
                id.push(0);
                ids.push(id);
            }
        });
    }

    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        self.attributes
            .as_com_ref::<IAttributeList>()
            .map_or(ptr::null_mut(), |list| list.as_ptr())
    }
}

// --- The component handler ------------------------------------------------

/// Something a plugin's controller did to one of its own parameters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Edit {
    /// A control was taken hold of.
    Begin(u32),
    /// The parameter has this normalized value now.
    Perform(u32, f64),
    /// The control was let go.
    End(u32),
}

/// Things a plugin asked for that are not edits or restarts, as bits of
/// [`Inbox::requests`].
pub(crate) mod request {
    pub(crate) const DIRTY: u32 = 1 << 0;
    pub(crate) const OPEN_EDITOR: u32 = 1 << 1;
    /// A view other than the editor was asked for.
    pub(crate) const OTHER_VIEW: u32 = 1 << 2;
}

/// What a plugin has told its host and the main thread has not handled yet.
pub(crate) struct Inbox {
    /// The thread the plugin was created on, as its
    /// [`thread_token`](crate::threads::thread_token).
    main_thread: u64,
    /// `RestartFlags` gathered since they were last handled.
    restart: AtomicU32,
    /// [`request`] bits.
    requests: AtomicU32,
    edits: Mutex<Vec<Edit>>,
    /// Edits dropped because [`MAX_EDITS`] were already waiting.
    lost_edits: AtomicU32,
    /// Calls made on a thread other than the main one.
    off_thread: AtomicU32,
    /// Whether the plugin's editor is open, for `requestOpenEditor`.
    editor_open: AtomicBool,
}

impl Inbox {
    /// An empty inbox. The calling thread is the plugin's main thread.
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            main_thread: thread_token(),
            restart: AtomicU32::new(0),
            requests: AtomicU32::new(0),
            edits: Mutex::new(Vec::new()),
            lost_edits: AtomicU32::new(0),
            off_thread: AtomicU32::new(0),
            editor_open: AtomicBool::new(false),
        })
    }

    /// Notes which thread a call came on.
    fn called(&self) {
        if thread_token() != self.main_thread {
            self.off_thread.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn record(&self, edit: Edit) {
        self.called();
        let mut edits = lock(&self.edits);
        if edits.len() < MAX_EDITS {
            edits.push(edit);
        } else {
            self.lost_edits.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The edits made since the last call, in order.
    pub(crate) fn take_edits(&self) -> Vec<Edit> {
        std::mem::take(&mut *lock(&self.edits))
    }

    /// The restart flags raised since the last call.
    pub(crate) fn take_restart(&self) -> u32 {
        self.restart.swap(0, Ordering::Relaxed)
    }

    /// The [`request`] bits raised since the last call.
    pub(crate) fn take_requests(&self) -> u32 {
        self.requests.swap(0, Ordering::Relaxed)
    }

    /// How many edits were dropped since the last call.
    pub(crate) fn take_lost_edits(&self) -> u32 {
        self.lost_edits.swap(0, Ordering::Relaxed)
    }

    /// How many calls came on another thread since the last call.
    pub(crate) fn take_off_thread(&self) -> u32 {
        self.off_thread.swap(0, Ordering::Relaxed)
    }

    /// Records whether the plugin's editor is open.
    pub(crate) fn set_editor_open(&self, open: bool) {
        self.editor_open.store(open, Ordering::Relaxed);
    }
}

/// The object a plugin's controller reports to.
pub(crate) struct ComponentHandler {
    inbox: Arc<Inbox>,
}

impl ComponentHandler {
    pub(crate) fn new(inbox: Arc<Inbox>) -> ComWrapper<Self> {
        ComWrapper::new(Self { inbox })
    }
}

impl Class for ComponentHandler {
    type Interfaces = (IComponentHandler, IComponentHandler2);
}

impl IComponentHandlerTrait for ComponentHandler {
    unsafe fn beginEdit(&self, id: vst::ParamID) -> tresult {
        guard(kResultFalse, || {
            self.inbox.record(Edit::Begin(id));
            kResultOk
        })
    }

    unsafe fn performEdit(&self, id: vst::ParamID, value: vst::ParamValue) -> tresult {
        guard(kResultFalse, || {
            if value.is_nan() {
                return kInvalidArgument;
            }
            self.inbox.record(Edit::Perform(id, value));
            kResultOk
        })
    }

    unsafe fn endEdit(&self, id: vst::ParamID) -> tresult {
        guard(kResultFalse, || {
            self.inbox.record(Edit::End(id));
            kResultOk
        })
    }

    unsafe fn restartComponent(&self, flags: i32) -> tresult {
        self.inbox.called();
        self.inbox.restart.fetch_or(flags as u32, Ordering::Relaxed);
        kResultOk
    }
}

impl IComponentHandler2Trait for ComponentHandler {
    unsafe fn setDirty(&self, state: TBool) -> tresult {
        self.inbox.called();
        if state != 0 {
            self.inbox
                .requests
                .fetch_or(request::DIRTY, Ordering::Relaxed);
        }
        kResultOk
    }

    unsafe fn requestOpenEditor(&self, name: FIDString) -> tresult {
        guard(kResultFalse, || {
            self.inbox.called();
            // VST 3 has the editor asked for by default; null is taken to
            // mean the same.
            // SAFETY: the plugin passes a NUL-terminated name, or null, and
            // the bindings' constant is a literal ending in a zero.
            let editor = name.is_null()
                || unsafe { CStr::from_ptr(name) == CStr::from_ptr(vst::ViewType::kEditor) };
            if !editor {
                // The editor is the one view this host opens.
                self.inbox
                    .requests
                    .fetch_or(request::OTHER_VIEW, Ordering::Relaxed);
                return kResultFalse;
            }
            // Only the application can make a window to open the editor
            // in, and this host cannot tell whether it will. So the request
            // is passed on, and the plugin is told it was met only if the
            // editor is open already: the application is then asked to
            // bring it forward, which needs nothing new.
            self.inbox
                .requests
                .fetch_or(request::OPEN_EDITOR, Ordering::Relaxed);
            if self.inbox.editor_open.load(Ordering::Relaxed) {
                kResultTrue
            } else {
                kResultFalse
            }
        })
    }

    unsafe fn startGroupEdit(&self) -> tresult {
        // Edits are reported one by one whether or not they were grouped.
        kResultOk
    }

    unsafe fn finishGroupEdit(&self) -> tresult {
        kResultOk
    }
}

/// The pointer a controller is given as its handler.
pub(crate) fn handler_ptr(handler: &ComWrapper<ComponentHandler>) -> *mut IComponentHandler {
    handler
        .as_com_ref::<IComponentHandler>()
        .map_or(ptr::null_mut(), |handler| handler.as_ptr())
}

// --- Streams --------------------------------------------------------------

struct Stream {
    bytes: Vec<u8>,
    position: usize,
    /// Most bytes moved per call.
    per_call: usize,
    /// Most bytes the stream may come to hold.
    limit: usize,
    /// Set when the plugin tried to write past `limit`.
    too_large: bool,
}

/// A stream in memory, which a plugin saves its state into or loads it
/// from.
pub(crate) struct MemoryStream {
    stream: Mutex<Stream>,
}

impl MemoryStream {
    /// A stream that holds `bytes`, to be read from its start. `per_call`
    /// is the most bytes one call moves.
    pub(crate) fn holding(bytes: Vec<u8>, per_call: usize) -> ComWrapper<Self> {
        ComWrapper::new(Self {
            stream: Mutex::new(Stream {
                // What is there may be overwritten, but the stream does not
                // grow beyond what any state may be.
                limit: bytes.len().max(MAX_STATE_BYTES),
                bytes,
                position: 0,
                per_call,
                too_large: false,
            }),
        })
    }

    /// An empty stream that takes at most [`MAX_STATE_BYTES`].
    pub(crate) fn empty(per_call: usize) -> ComWrapper<Self> {
        Self::holding(Vec::new(), per_call)
    }

    /// Puts the position back at the start.
    pub(crate) fn rewind(&self) {
        lock(&self.stream).position = 0;
    }

    /// Takes what the stream holds, or `None` if the plugin tried to put
    /// more into it than a state may be.
    pub(crate) fn finish(&self) -> Option<Vec<u8>> {
        let mut stream = lock(&self.stream);
        stream.position = 0;
        (!stream.too_large).then(|| std::mem::take(&mut stream.bytes))
    }
}

impl Class for MemoryStream {
    type Interfaces = (IBStream,);
}

/// The pointer a plugin is given to a stream.
pub(crate) fn stream_ptr(stream: &ComWrapper<MemoryStream>) -> *mut IBStream {
    stream
        .as_com_ref::<IBStream>()
        .map_or(ptr::null_mut(), |stream| stream.as_ptr())
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(&self, buffer: *mut c_void, count: i32, read: *mut i32) -> tresult {
        guard(kResultFalse, || {
            let Ok(wanted) = usize::try_from(count) else {
                return kInvalidArgument;
            };
            let mut stream = lock(&self.stream);
            let rest = stream.bytes.get(stream.position..).unwrap_or_default();
            // Fewer bytes than asked for is allowed: the plugin asks again.
            // None means the end.
            let taken = wanted.min(stream.per_call).min(rest.len());
            if taken > 0 {
                if buffer.is_null() {
                    return kInvalidArgument;
                }
                // SAFETY: the plugin offers `count` writable bytes at
                // `buffer`, `taken` is no more than that and no more than
                // `rest` holds, and the plugin's buffer cannot overlap bytes
                // this host owns.
                unsafe { ptr::copy_nonoverlapping(rest.as_ptr(), buffer.cast::<u8>(), taken) };
            }
            stream.position += taken;
            if !read.is_null() {
                // SAFETY: the plugin's own number.
                unsafe { read.write_unaligned(taken as i32) };
            }
            kResultOk
        })
    }

    unsafe fn write(&self, buffer: *mut c_void, count: i32, written: *mut i32) -> tresult {
        guard(kResultFalse, || {
            let Ok(offered) = usize::try_from(count) else {
                return kInvalidArgument;
            };
            let report = |taken: usize| {
                if !written.is_null() {
                    // SAFETY: the plugin's own number.
                    unsafe { written.write_unaligned(taken as i32) };
                }
            };
            let mut stream = lock(&self.stream);
            // Fewer bytes than offered is allowed: the plugin offers the
            // rest again.
            let taken = offered.min(stream.per_call);
            let end = stream.position.saturating_add(taken);
            if end > stream.limit {
                stream.too_large = true;
                report(0);
                return kResultFalse;
            }
            if taken > 0 {
                if buffer.is_null() {
                    report(0);
                    return kInvalidArgument;
                }
                // SAFETY: the plugin offers `count` readable bytes at
                // `buffer`, and `taken` is no more than that.
                let data = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), taken) };
                if end > stream.bytes.len() {
                    stream.bytes.resize(end, 0);
                }
                let start = stream.position;
                stream.bytes[start..end].copy_from_slice(data);
                stream.position = end;
            }
            report(taken);
            kResultOk
        })
    }

    unsafe fn seek(&self, position: i64, mode: i32, result: *mut i64) -> tresult {
        guard(kResultFalse, || {
            let mut stream = lock(&self.stream);
            let length = stream.bytes.len() as i64;
            let base = if mode == IStreamSeekMode_::kIBSeekSet as i32 {
                0
            } else if mode == IStreamSeekMode_::kIBSeekCur as i32 {
                stream.position as i64
            } else if mode == IStreamSeekMode_::kIBSeekEnd as i32 {
                length
            } else {
                return kInvalidArgument;
            };
            // A position outside what the stream holds is refused, and the
            // stream stays where it was.
            let target = base
                .checked_add(position)
                .filter(|target| (0..=length).contains(target));
            if let Some(target) = target {
                stream.position = target as usize;
            }
            if !result.is_null() {
                // SAFETY: the plugin's own number.
                unsafe { result.write_unaligned(stream.position as i64) };
            }
            if target.is_some() {
                kResultOk
            } else {
                kResultFalse
            }
        })
    }

    unsafe fn tell(&self, position: *mut i64) -> tresult {
        if position.is_null() {
            return kInvalidArgument;
        }
        let at = lock(&self.stream).position as i64;
        // SAFETY: the plugin's own number, not null.
        unsafe { position.write_unaligned(at) };
        kResultOk
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::c_char;

    use vst3::ComPtr;

    use super::*;

    fn stream_of(stream: &ComWrapper<MemoryStream>) -> ComPtr<IBStream> {
        stream.to_com_ptr::<IBStream>().unwrap()
    }

    #[test]
    fn a_stream_reads_writes_seeks_and_tells() {
        let wrapper = MemoryStream::empty(usize::MAX);
        let stream = stream_of(&wrapper);
        let mut moved = -1i32;
        let mut at = -1i64;
        // SAFETY: every pointer is to a local that outlives the call, and
        // every count is the length of the buffer passed with it.
        unsafe {
            let mut data = *b"mangue beat";
            assert_eq!(
                stream.write(data.as_mut_ptr().cast(), 11, &mut moved),
                kResultOk
            );
            assert_eq!(moved, 11);
            assert_eq!(stream.tell(&mut at), kResultOk);
            assert_eq!(at, 11);

            // Back to patch what was written, as plugins do with a header.
            let set = IStreamSeekMode_::kIBSeekSet as i32;
            let current = IStreamSeekMode_::kIBSeekCur as i32;
            let end = IStreamSeekMode_::kIBSeekEnd as i32;
            assert_eq!(stream.seek(7, set, &mut at), kResultOk);
            assert_eq!(at, 7);
            let mut patch = *b"BEAT";
            assert_eq!(
                stream.write(patch.as_mut_ptr().cast(), 4, &mut moved),
                kResultOk
            );
            assert_eq!(stream.seek(-4, current, &mut at), kResultOk);
            assert_eq!(at, 7);
            assert_eq!(stream.seek(-11, end, &mut at), kResultOk);
            assert_eq!(at, 0);

            let mut back = [0u8; 32];
            assert_eq!(
                stream.read(back.as_mut_ptr().cast(), 32, &mut moved),
                kResultOk
            );
            assert_eq!(&back[..moved as usize], b"mangue BEAT");
            // At the end there is nothing more, and saying so is not an
            // error.
            assert_eq!(
                stream.read(back.as_mut_ptr().cast(), 32, &mut moved),
                kResultOk
            );
            assert_eq!(moved, 0);

            // Positions outside the stream, modes that do not exist and
            // counts below zero are refused, and nothing moves.
            assert_eq!(stream.seek(-1, set, &mut at), kResultFalse);
            assert_eq!(stream.seek(1, end, &mut at), kResultFalse);
            assert_eq!(stream.seek(i64::MAX, current, &mut at), kResultFalse);
            assert_eq!(at, 11);
            assert_eq!(stream.seek(0, 3, &mut at), kInvalidArgument);
            assert_eq!(
                stream.read(back.as_mut_ptr().cast(), -1, &mut moved),
                kInvalidArgument
            );
            assert_eq!(
                stream.write(back.as_mut_ptr().cast(), -1, &mut moved),
                kInvalidArgument
            );
            assert_eq!(stream.tell(ptr::null_mut()), kInvalidArgument);
            // Null out-pointers are allowed where the plugin has no use for
            // the answer.
            assert_eq!(stream.seek(0, set, ptr::null_mut()), kResultOk);
            assert_eq!(
                stream.read(back.as_mut_ptr().cast(), 3, ptr::null_mut()),
                kResultOk
            );
        }
        assert_eq!(wrapper.finish().as_deref(), Some(&b"mangue BEAT"[..]));
    }

    #[test]
    fn a_stream_moves_no_more_per_call_than_it_was_told_and_stops_at_its_limit() {
        let wrapper = MemoryStream::holding(b"0123456789".to_vec(), 4);
        let stream = stream_of(&wrapper);
        let mut moved = 0i32;
        let mut seen = Vec::new();
        // SAFETY: as in the test above.
        unsafe {
            loop {
                let mut chunk = [0u8; 16];
                assert_eq!(
                    stream.read(chunk.as_mut_ptr().cast(), 16, &mut moved),
                    kResultOk
                );
                if moved == 0 {
                    break;
                }
                assert!(moved <= 4);
                seen.extend_from_slice(&chunk[..moved as usize]);
            }
        }
        assert_eq!(seen, b"0123456789");

        // A stream that has reached what a state may be takes no more, and
        // says the state was too large instead of handing over a part.
        let full = ComWrapper::new(MemoryStream {
            stream: Mutex::new(Stream {
                bytes: Vec::new(),
                position: 0,
                per_call: usize::MAX,
                limit: 8,
                too_large: false,
            }),
        });
        let stream = stream_of(&full);
        // SAFETY: as in the test above.
        unsafe {
            let mut data = [1u8; 16];
            assert_eq!(
                stream.write(data.as_mut_ptr().cast(), 8, &mut moved),
                kResultOk
            );
            assert_eq!(moved, 8);
            assert_eq!(
                stream.write(data.as_mut_ptr().cast(), 1, &mut moved),
                kResultFalse
            );
            assert_eq!(moved, 0);
        }
        assert_eq!(full.finish(), None);
    }

    #[test]
    fn a_message_carries_values_of_every_kind_between_the_two_halves() {
        let host = HostApplication::new("Test Host");
        let application = host.to_com_ptr::<IHostApplication>().unwrap();
        // SAFETY: every pointer is to a local that outlives the call, and
        // every text is terminated.
        unsafe {
            let mut name = [0xFFFFu16; 128];
            assert_eq!(application.getName(&mut name), kResultOk);
            assert_eq!(super::super::text::wide_text(&name), "Test Host");
            assert_eq!(application.getName(ptr::null_mut()), kInvalidArgument);

            // One pointer for both identifiers: two `&mut` borrows of the
            // same local alive at once would be undefined behaviour.
            let mut class = IMessage::IID.map(|byte| byte as c_char);
            let class_and_interface = &raw mut class;
            let mut object = ptr::null_mut();
            assert_eq!(
                application.createInstance(class_and_interface, class_and_interface, &mut object),
                kResultOk
            );
            let message = ComPtr::<IMessage>::from_raw(object.cast()).unwrap();
            assert!(message.getMessageID().is_null());
            message.setMessageID(c"meter".as_ptr());
            assert_eq!(std::ffi::CStr::from_ptr(message.getMessageID()), c"meter");

            let list = vst3::ComRef::<IAttributeList>::from_raw(message.getAttributes()).unwrap();
            assert_eq!(list.setInt(c"count".as_ptr(), -7), kResultOk);
            assert_eq!(list.setFloat(c"level".as_ptr(), 0.25), kResultOk);
            let text: Vec<u16> = "olá\0".encode_utf16().collect();
            assert_eq!(list.setString(c"name".as_ptr(), text.as_ptr()), kResultOk);
            assert_eq!(
                list.setBinary(c"blob".as_ptr(), b"abc".as_ptr().cast(), 3),
                kResultOk
            );

            let (mut int, mut float) = (0i64, 0f64);
            assert_eq!(list.getInt(c"count".as_ptr(), &mut int), kResultOk);
            assert_eq!(list.getFloat(c"level".as_ptr(), &mut float), kResultOk);
            assert_eq!((int, float), (-7, 0.25));
            let mut back = [0xFFFFu16; 8];
            assert_eq!(
                list.getString(c"name".as_ptr(), back.as_mut_ptr(), 16),
                kResultOk
            );
            assert_eq!(super::super::text::wide_text(&back), "olá");
            // Too little room cuts the text and still ends it.
            assert_eq!(
                list.getString(c"name".as_ptr(), back.as_mut_ptr(), 4),
                kResultOk
            );
            assert_eq!(back[..2], [u16::from(b'o'), 0]);
            let (mut data, mut size) = (ptr::null(), 0u32);
            assert_eq!(
                list.getBinary(c"blob".as_ptr(), &mut data, &mut size),
                kResultOk
            );
            assert_eq!(std::slice::from_raw_parts(data.cast::<u8>(), 3), b"abc");

            // A replaced value leaves the pointer handed out before it
            // readable.
            assert_eq!(
                list.setBinary(c"blob".as_ptr(), b"xy".as_ptr().cast(), 2),
                kResultOk
            );
            assert_eq!(std::slice::from_raw_parts(data.cast::<u8>(), 3), b"abc");

            // The wrong kind, a name never set and null are all refusals.
            assert_eq!(list.getInt(c"level".as_ptr(), &mut int), kResultFalse);
            assert_eq!(list.getInt(c"nothing".as_ptr(), &mut int), kResultFalse);
            assert_eq!(list.getInt(ptr::null(), &mut int), kInvalidArgument);
            assert_eq!(list.setInt(ptr::null(), 1), kInvalidArgument);
            assert_eq!(
                list.getString(c"name".as_ptr(), back.as_mut_ptr(), 1),
                kInvalidArgument
            );

            // Anything else the plugin asks its host to make is declined.
            let mut other = IComponent::IID.map(|byte| byte as c_char);
            let other_both = &raw mut other;
            let mut object = ptr::dangling_mut::<c_void>();
            assert_eq!(
                application.createInstance(other_both, other_both, &mut object),
                kNotImplemented
            );
            assert!(object.is_null());
        }
    }

    #[test]
    fn the_handler_records_and_does_not_act() {
        let inbox = Inbox::new();
        let wrapper = ComponentHandler::new(Arc::clone(&inbox));
        let handler = wrapper.to_com_ptr::<IComponentHandler>().unwrap();
        let second = wrapper.to_com_ptr::<IComponentHandler2>().unwrap();
        // SAFETY: the calls take numbers only.
        unsafe {
            assert_eq!(handler.beginEdit(7), kResultOk);
            assert_eq!(handler.performEdit(7, 0.25), kResultOk);
            assert_eq!(handler.performEdit(7, f64::NAN), kInvalidArgument);
            assert_eq!(handler.endEdit(7), kResultOk);
            assert_eq!(handler.restartComponent(8), kResultOk);
            assert_eq!(handler.restartComponent(4), kResultOk);
            assert_eq!(second.setDirty(1), kResultOk);
            assert_eq!(second.setDirty(0), kResultOk);
            assert_eq!(second.requestOpenEditor(ptr::null()), kResultFalse);
        }
        assert_eq!(
            inbox.take_edits(),
            [Edit::Begin(7), Edit::Perform(7, 0.25), Edit::End(7)]
        );
        assert!(inbox.take_edits().is_empty(), "an edit was reported twice");
        assert_eq!(inbox.take_restart(), 12);
        assert_eq!(inbox.take_restart(), 0);
        assert_eq!(inbox.take_requests(), request::DIRTY | request::OPEN_EDITOR);
        assert_eq!(inbox.take_off_thread(), 0);

        // The editor asked for while it is open is met: the application is
        // asked to bring it forward. Any other view is refused.
        inbox.set_editor_open(true);
        // SAFETY: terminated names.
        unsafe {
            assert_eq!(second.requestOpenEditor(c"editor".as_ptr()), kResultTrue);
            assert_eq!(second.requestOpenEditor(c"other".as_ptr()), kResultFalse);
        }
        assert_eq!(
            inbox.take_requests(),
            request::OPEN_EDITOR | request::OTHER_VIEW
        );
        inbox.set_editor_open(false);

        // From another thread the call is still taken, and remarked on.
        let elsewhere = wrapper.clone();
        std::thread::spawn(move || {
            let handler = elsewhere.to_com_ptr::<IComponentHandler>().unwrap();
            // SAFETY: the call takes numbers only.
            unsafe { handler.performEdit(1, 1.0) };
        })
        .join()
        .unwrap();
        assert_eq!(inbox.take_edits(), [Edit::Perform(1, 1.0)]);
        assert_eq!(inbox.take_off_thread(), 1);

        // More edits than can wait are counted, not kept.
        for index in 0..MAX_EDITS as u32 + 3 {
            // SAFETY: the call takes numbers only.
            unsafe { handler.beginEdit(index) };
        }
        assert_eq!(inbox.take_edits().len(), MAX_EDITS);
        assert_eq!(inbox.take_lost_edits(), 3);
    }
}
