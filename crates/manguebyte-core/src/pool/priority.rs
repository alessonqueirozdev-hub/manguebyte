//! Real-time scheduling for the calling thread, where the system grants it
//! without administrator rights.
//!
//! | System | What is asked |
//! |---|---|
//! | Windows | MMCSS: `AvSetMmThreadCharacteristicsW("Pro Audio")`, reverted with `AvRevertMmThreadCharacteristics` |
//! | Linux, Android, FreeBSD | `pthread_setschedparam` with `SCHED_FIFO`, trying priorities 70, 40, 20, 10, 5 and 1 |
//! | macOS, iOS | `pthread_setschedparam` with `SCHED_FIFO`, the same priorities |
//! | Others, WebAssembly, Miri | Nothing: [`Priority::Normal`] |
//!
//! A refusal is not an error: the thread keeps its normal priority and
//! [`Priority::Refused`] says so. On Linux `SCHED_FIFO` needs a real-time
//! limit (`RLIMIT_RTPRIO`, set for the `audio` group on most audio
//! distributions) or `CAP_SYS_NICE`; without one every priority is refused.

use std::marker::PhantomData;

/// What a thread obtained when it asked to be scheduled as real-time work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Priority {
    /// Windows: the thread joined the "Pro Audio" task of the Multimedia
    /// Class Scheduler Service, which raises it while it runs audio work.
    ProAudio,
    /// Unix: `SCHED_FIFO` at this priority.
    Fifo(i32),
    /// The system refused every request, usually for lack of rights; the
    /// thread runs at normal priority.
    Refused,
    /// Nothing was asked: real-time scheduling was turned off, or the pool
    /// does not know how on this system (WebAssembly, Miri, others).
    Normal,
}

impl Priority {
    /// Whether the thread runs at a real-time priority.
    pub fn is_real_time(self) -> bool {
        matches!(self, Priority::ProAudio | Priority::Fifo(_))
    }
}

/// The calling thread's real-time scheduling, given back when dropped.
///
/// It is tied to the thread that asked (it is neither `Send` nor `Sync`),
/// because the system reverts a thread's scheduling on that thread.
///
/// ```
/// use manguebyte_core::pool::Promotion;
///
/// let promotion = Promotion::current_thread();
/// println!("this thread runs at {:?}", promotion.priority());
/// // Normal priority again here.
/// drop(promotion);
/// ```
pub struct Promotion {
    priority: Priority,
    restore: system::Restore,
    _thread_bound: PhantomData<*const ()>,
}

impl Promotion {
    /// Asks the system to schedule the calling thread as real-time audio
    /// work. It may make a few system calls: call it once when a thread
    /// starts, never on the audio thread while it processes.
    pub fn current_thread() -> Promotion {
        let (priority, restore) = system::promote();
        Promotion {
            priority,
            restore,
            _thread_bound: PhantomData,
        }
    }

    /// A promotion that asked for nothing, and so restores nothing.
    pub(crate) fn none() -> Promotion {
        Promotion {
            priority: Priority::Normal,
            restore: system::Restore::default(),
            _thread_bound: PhantomData,
        }
    }

    /// What the system granted.
    pub fn priority(&self) -> Priority {
        self.priority
    }
}

impl Drop for Promotion {
    fn drop(&mut self) {
        system::restore(&mut self.restore);
    }
}

impl std::fmt::Debug for Promotion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Promotion")
            .field("priority", &self.priority)
            .finish()
    }
}

#[cfg(all(windows, not(miri)))]
mod system {
    use std::ffi::c_void;
    use std::ptr;

    use super::Priority;

    #[link(name = "avrt")]
    unsafe extern "system" {
        fn AvSetMmThreadCharacteristicsW(
            task_name: *const u16,
            task_index: *mut u32,
        ) -> *mut c_void;
        fn AvRevertMmThreadCharacteristics(handle: *mut c_void) -> i32;
    }

    /// "Pro Audio" in UTF-16, with its NUL.
    const PRO_AUDIO: [u16; 10] = {
        let text = b"Pro Audio\0";
        let mut wide = [0u16; 10];
        let mut index = 0;
        while index < text.len() {
            wide[index] = text[index] as u16;
            index += 1;
        }
        wide
    };

    /// The MMCSS handle to give back, or null.
    pub(super) struct Restore(*mut c_void);

    impl Default for Restore {
        fn default() -> Self {
            Restore(ptr::null_mut())
        }
    }

    pub(super) fn promote() -> (Priority, Restore) {
        let mut task_index = 0u32;
        // SAFETY: the task name is a NUL-terminated UTF-16 string that
        // outlives the call, and `task_index` is ours to write.
        let handle = unsafe { AvSetMmThreadCharacteristicsW(PRO_AUDIO.as_ptr(), &mut task_index) };
        if handle.is_null() {
            (Priority::Refused, Restore::default())
        } else {
            (Priority::ProAudio, Restore(handle))
        }
    }

    pub(super) fn restore(restore: &mut Restore) {
        if !restore.0.is_null() {
            // SAFETY: the handle came from `AvSetMmThreadCharacteristicsW`
            // on this thread (`Promotion` is not `Send`) and is given back
            // once.
            unsafe { AvRevertMmThreadCharacteristics(restore.0) };
            restore.0 = ptr::null_mut();
        }
    }
}

#[cfg(all(
    not(miri),
    any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_vendor = "apple"
    )
))]
mod system {
    use std::ffi::c_int;
    use std::os::unix::thread::RawPthread;

    use super::Priority;

    /// `struct sched_param`: one `int` on Linux and FreeBSD; macOS adds
    /// four opaque bytes, which the larger size covers everywhere.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct SchedParam {
        sched_priority: c_int,
        opaque: [u8; 4],
    }

    #[cfg(target_vendor = "apple")]
    const SCHED_FIFO: c_int = 4;
    #[cfg(not(target_vendor = "apple"))]
    const SCHED_FIFO: c_int = 1;

    /// Tried from the first down until one is granted. A real-time limit
    /// below 70 still gets the highest priority it allows.
    const PRIORITIES: [c_int; 6] = [70, 40, 20, 10, 5, 1];

    unsafe extern "C" {
        fn pthread_self() -> RawPthread;
        fn pthread_getschedparam(
            thread: RawPthread,
            policy: *mut c_int,
            param: *mut SchedParam,
        ) -> c_int;
        fn pthread_setschedparam(
            thread: RawPthread,
            policy: c_int,
            param: *const SchedParam,
        ) -> c_int;
        fn sched_get_priority_min(policy: c_int) -> c_int;
        fn sched_get_priority_max(policy: c_int) -> c_int;
    }

    /// The scheduling the thread had before, to put back.
    #[derive(Default)]
    pub(super) struct Restore(Option<(c_int, SchedParam)>);

    pub(super) fn promote() -> (Priority, Restore) {
        let mut policy: c_int = 0;
        let mut previous = SchedParam::default();
        // SAFETY: plain calls on the calling thread; `policy` and
        // `previous` are ours to write, and `previous` is at least as large
        // as the system's `sched_param`.
        let (thread, known) = unsafe {
            let thread = pthread_self();
            let known = pthread_getschedparam(thread, &mut policy, &mut previous) == 0;
            (thread, known)
        };
        // SAFETY: these only read a constant of the system.
        let (lowest, highest) = unsafe {
            (
                sched_get_priority_min(SCHED_FIFO),
                sched_get_priority_max(SCHED_FIFO),
            )
        };
        for wanted in PRIORITIES {
            let priority = wanted.clamp(lowest, highest.max(lowest));
            let param = SchedParam {
                sched_priority: priority,
                opaque: [0; 4],
            };
            // SAFETY: `param` is a valid `sched_param` that outlives the
            // call, for the calling thread.
            if unsafe { pthread_setschedparam(thread, SCHED_FIFO, &param) } == 0 {
                let restore = Restore(known.then_some((policy, previous)));
                return (Priority::Fifo(priority), restore);
            }
        }
        (Priority::Refused, Restore::default())
    }

    pub(super) fn restore(restore: &mut Restore) {
        if let Some((policy, param)) = restore.0.take() {
            // SAFETY: the policy and parameters the thread had, read on
            // this same thread (`Promotion` is not `Send`). A failure
            // leaves the thread as it is, which is all that can be done.
            unsafe { pthread_setschedparam(pthread_self(), policy, &param) };
        }
    }
}

#[cfg(not(any(
    all(windows, not(miri)),
    all(
        not(miri),
        any(
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_vendor = "apple"
        )
    )
)))]
mod system {
    use super::Priority;

    #[derive(Default)]
    pub(super) struct Restore;

    pub(super) fn promote() -> (Priority, Restore) {
        (Priority::Normal, Restore)
    }

    pub(super) fn restore(_restore: &mut Restore) {}
}
