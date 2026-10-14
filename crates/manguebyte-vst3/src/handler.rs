//! The host's `IComponentHandler`, through which edits reach the host.
//!
//! A host gives the controller its component handler with
//! `setComponentHandler`, and a plugin's editor reports a person's edits
//! through it: `beginEdit` when a control is taken hold of, `performEdit`
//! with each value, `endEdit` when it is let go. [`Handler`] keeps the
//! handler for as long as the host wants it kept and makes sure the host
//! sees well-formed edits whatever happens around them:
//!
//! - Edits on one parameter from several editors at once (two views of one
//!   plugin) reach the host as one: the first begin is sent, the last end is
//!   sent, and the ones in between are not.
//! - A host that replaces its handler while an edit is open has the edit
//!   ended on the old handler and begun again on the new one, so each
//!   handler sees begins and ends in pairs.
//! - A host that takes its handler away has every open edit ended first.
//!
//! Nothing here infers an edit: the only callers are the editor's gestures,
//! through the wrapper's `EditorHost`, and `restart` for values the editor
//! changed without gestures.
//!
//! Main thread only. No borrow is held while the host is called, so a host
//! may call the plugin back from inside any of its functions.

use std::cell::RefCell;
use std::rc::Rc;

use vst3::ComPtr;
use vst3::Steinberg::Vst::{IComponentHandler, IComponentHandlerTrait};

/// The host's component handler, if it gave one, and the edits open on it.
pub(crate) struct Handler {
    /// Shared out to each call, so that the host may replace it while one
    /// of its functions runs.
    current: RefCell<Option<Rc<ComPtr<IComponentHandler>>>>,
    /// The VST 3 ids of the parameters with an edit open, and how many
    /// editors hold each, in the order they were opened.
    open: RefCell<Vec<(u32, u32)>>,
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
    fn current(&self) -> Option<Rc<ComPtr<IComponentHandler>>> {
        self.current.borrow().clone()
    }

    /// Keeps `handler` from now on, in place of the one before, which is
    /// released. Edits open on the old handler are ended on it and begun on
    /// the new one. Setting the handler that is already kept changes
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
        let new = handler.map(Rc::new);
        let old = self.current.replace(new.clone());
        let open: Vec<u32> = self.open.borrow().iter().map(|&(id, _)| id).collect();
        if let Some(old) = &old {
            for &id in &open {
                // SAFETY: the host's own object, kept alive by our
                // reference, on the main thread.
                unsafe { old.endEdit(id) };
            }
        }
        if let Some(new) = &new {
            for &id in &open {
                // SAFETY: as above.
                unsafe { new.beginEdit(id) };
            }
        }
        // The old handler is released last, after nothing borrowed here is
        // in use.
        drop(old);
    }

    /// An editor has begun an edit of the parameter with VST 3 id `id`.
    pub(crate) fn begin_edit(&self, id: u32) {
        let first = {
            let mut open = self.open.borrow_mut();
            match open.iter_mut().find(|(open_id, _)| *open_id == id) {
                Some((_, depth)) => {
                    *depth = depth.saturating_add(1);
                    false
                }
                None => {
                    open.push((id, 1));
                    true
                }
            }
        };
        if first {
            if let Some(handler) = self.current() {
                // SAFETY: as in `set`.
                unsafe { handler.beginEdit(id) };
            }
        }
    }

    /// An editor has set the parameter with VST 3 id `id` to `normalized`.
    /// Sent only while an edit of it is open, as VST 3 requires.
    pub(crate) fn perform_edit(&self, id: u32, normalized: f64) {
        if !self.open.borrow().iter().any(|&(open_id, _)| open_id == id) {
            return;
        }
        if let Some(handler) = self.current() {
            // SAFETY: as in `set`.
            unsafe { handler.performEdit(id, normalized) };
        }
    }

    /// An editor has ended an edit of the parameter with VST 3 id `id`.
    pub(crate) fn end_edit(&self, id: u32) {
        let last = {
            let mut open = self.open.borrow_mut();
            let Some(index) = open.iter().position(|&(open_id, _)| open_id == id) else {
                return;
            };
            let depth = &mut open[index].1;
            *depth = depth.saturating_sub(1);
            if *depth == 0 {
                open.remove(index);
                true
            } else {
                false
            }
        };
        if last {
            if let Some(handler) = self.current() {
                // SAFETY: as in `set`.
                unsafe { handler.endEdit(id) };
            }
        }
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
