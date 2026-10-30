//! The host's `IComponentHandler`, through which edits reach the host.
//!
//! A host gives the controller its component handler with
//! `setComponentHandler`, and a plugin's editor reports a person's edits
//! through it: `beginEdit` when a control is taken hold of, `performEdit`
//! with each value, `endEdit` when it is let go. [`Handler`] keeps the
//! handler for as long as the host wants it kept and makes sure that each
//! handler sees well-formed edits whatever happens around them:
//!
//! - Edits on one parameter from several editors at once (two views of one
//!   plugin) reach the host as one: the first begin is sent, the last end is
//!   sent, and the ones in between are not.
//! - Each open edit remembers the handler its `beginEdit` went to. Its
//!   `performEdit` calls go to that handler while it is the current one, and
//!   its `endEdit` goes to that handler and to no other, so no handler is
//!   ever sent an end, or a value, for an edit it was not told had begun.
//! - A host that replaces its handler while an edit is open has the edit
//!   ended on the old handler and begun again on the new one.
//! - A host that takes its handler away, or a component that is terminated,
//!   has every open edit ended on the handler first.
//!
//! The host may call the plugin back from inside any of these calls,
//! replacing its handler or terminating the component among other things.
//! So nothing is borrowed while the host is called, and after every call
//! [`Handler::set`] looks again at which handler is current and which edits
//! are open, rather than working from what it saw before the call: a
//! handler replaced or taken away from inside a call is never called again
//! for edits it no longer holds. A handler that is being called when it is
//! replaced is released when that call returns.
//!
//! Nothing here infers an edit: the only callers are the editor's gestures,
//! through the wrapper's `EditorHost`, and `restart` for values the editor
//! changed without gestures.
//!
//! Main thread only.

use std::cell::RefCell;
use std::rc::Rc;

use vst3::ComPtr;
use vst3::Steinberg::Vst::{IComponentHandler, IComponentHandlerTrait};

/// The host's handler, shared out to each call so that the host may replace
/// it while one of its functions runs.
type Shared = Rc<ComPtr<IComponentHandler>>;

/// An edit one or more editors have open on one parameter.
struct OpenEdit {
    /// The parameter's VST 3 id.
    id: u32,
    /// How many editors hold it.
    holders: u32,
    /// The handler its `beginEdit` was sent to, until its `endEdit` is.
    /// `None` while there is no handler, or between being ended on a
    /// handler that was replaced and being begun on the next one.
    begun_on: Option<Shared>,
}

/// The host's component handler, if it gave one, and the edits open.
pub(crate) struct Handler {
    current: RefCell<Option<Shared>>,
    /// In the order they were opened.
    open: RefCell<Vec<OpenEdit>>,
}

impl Handler {
    /// No handler and no edits.
    pub(crate) fn new() -> Self {
        Self {
            current: RefCell::new(None),
            open: RefCell::new(Vec::new()),
        }
    }

    /// The handler, shared, without calling the host.
    fn current(&self) -> Option<Shared> {
        self.current.borrow().clone()
    }

    /// Whether the host has given a handler, so that `restart` reaches it.
    pub(crate) fn is_set(&self) -> bool {
        self.current.borrow().is_some()
    }

    /// Keeps `handler` from now on, in place of the one before, which is
    /// released. Edits begun on the old handler are ended on it and begun
    /// on the new one. Setting the handler that is already kept changes
    /// nothing.
    pub(crate) fn set(&self, handler: Option<ComPtr<IComponentHandler>>) {
        let same = match (&*self.current.borrow(), &handler) {
            (Some(old), Some(new)) => old.as_ptr() == new.as_ptr(),
            (None, None) => true,
            _ => false,
        };
        if same {
            // Our reference to the new copy is released as it drops.
            return;
        }
        let old = self.current.replace(handler.map(Rc::new));
        if let Some(old) = &old {
            // One edit at a time, looking again after each call: the host
            // may open, end or move edits from inside `endEdit`.
            while let Some(id) = self.take_edit_begun_on(old) {
                // SAFETY: the host's own object, kept alive by our
                // reference, on the main thread.
                unsafe { old.endEdit(id) };
            }
        }
        self.begin_open_edits();
        // The old handler is released last, once nothing here calls it.
        drop(old);
    }

    /// Marks the first edit begun on `handler` as begun nowhere, and returns
    /// its id, so that it can be ended there.
    fn take_edit_begun_on(&self, handler: &Shared) -> Option<u32> {
        let mut open = self.open.borrow_mut();
        let edit = open.iter_mut().find(|edit| {
            edit.begun_on
                .as_ref()
                .is_some_and(|begun_on| Rc::ptr_eq(begun_on, handler))
        })?;
        // Not the last reference: the caller holds one.
        edit.begun_on = None;
        Some(edit.id)
    }

    /// Begins on the current handler every open edit that is begun nowhere,
    /// one at a time, looking again after each call at which handler is
    /// current.
    fn begin_open_edits(&self) {
        loop {
            let Some(handler) = self.current() else {
                return;
            };
            let id = {
                let mut open = self.open.borrow_mut();
                let Some(edit) = open.iter_mut().find(|edit| edit.begun_on.is_none()) else {
                    return;
                };
                edit.begun_on = Some(Rc::clone(&handler));
                edit.id
            };
            // SAFETY: as in `set`.
            unsafe { handler.beginEdit(id) };
        }
    }

    /// An editor has begun an edit of the parameter with VST 3 id `id`.
    pub(crate) fn begin_edit(&self, id: u32) {
        let target = {
            let mut open = self.open.borrow_mut();
            if let Some(edit) = open.iter_mut().find(|edit| edit.id == id) {
                edit.holders = edit.holders.saturating_add(1);
                return;
            }
            // Noted as begun on the handler before the host hears of it, so
            // that a handler replaced from inside `beginEdit` has the edit
            // ended on it.
            let current = self.current();
            open.push(OpenEdit {
                id,
                holders: 1,
                begun_on: current.clone(),
            });
            current
        };
        if let Some(handler) = target {
            // SAFETY: as in `set`.
            unsafe { handler.beginEdit(id) };
        }
    }

    /// An editor has set the parameter with VST 3 id `id` to `normalized`.
    /// Sent only to the current handler, and only if the edit was begun on
    /// it, as VST 3 requires.
    pub(crate) fn perform_edit(&self, id: u32, normalized: f64) {
        let target = {
            let open = self.open.borrow();
            let current = self.current.borrow();
            match (
                open.iter()
                    .find(|edit| edit.id == id)
                    .and_then(|edit| edit.begun_on.as_ref()),
                current.as_ref(),
            ) {
                (Some(begun_on), Some(current)) if Rc::ptr_eq(begun_on, current) => {
                    Some(Rc::clone(current))
                }
                _ => None,
            }
        };
        if let Some(handler) = target {
            // SAFETY: as in `set`.
            unsafe { handler.performEdit(id, normalized) };
        }
    }

    /// An editor has ended an edit of the parameter with VST 3 id `id`. The
    /// last end is sent to the handler the edit was begun on, if any.
    pub(crate) fn end_edit(&self, id: u32) {
        let target = {
            let mut open = self.open.borrow_mut();
            let Some(index) = open.iter().position(|edit| edit.id == id) else {
                return;
            };
            let holders = &mut open[index].holders;
            *holders = holders.saturating_sub(1);
            if *holders > 0 {
                return;
            }
            open.remove(index).begun_on
        };
        if let Some(handler) = target {
            // SAFETY: as in `set`.
            unsafe { handler.endEdit(id) };
        }
        // `target` is released here, after the call.
    }

    /// Asks the host to read again what `flags` names, such as every
    /// parameter's value.
    pub(crate) fn restart(&self, flags: i32) {
        if let Some(handler) = self.current() {
            // SAFETY: as in `set`.
            unsafe { handler.restartComponent(flags) };
        }
    }
}
