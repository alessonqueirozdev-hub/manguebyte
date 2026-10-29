//! Files and text dragged onto a window from other windows and programs.

use std::path::PathBuf;
use std::rc::Rc;

use crate::event::{Modifiers, PhysicalPoint};

/// The most files a drag is read with. A drag of more brings the first
/// this many, so that a drag of a whole disk costs no more than this.
pub const MAX_DRAGGED_FILES: usize = 4096;

/// What a drag over a window carries: files, text, or both.
///
/// A window reads it once, when the drag comes over it, and keeps it until
/// the drag is dropped or leaves ([`WindowCx::drag_data`]). On Win32 the
/// files come from the drag's `CF_HDROP` format, at most
/// [`MAX_DRAGGED_FILES`] of them, and the text from `CF_UNICODETEXT`, at
/// most [`MAX_CLIPBOARD_UNITS`](crate::MAX_CLIPBOARD_UNITS) UTF-16 units.
///
/// ```
/// use std::path::PathBuf;
/// use manguebyte_window::DragData;
///
/// let samples = DragData::files(["kick.wav", "snare.wav"]);
/// assert_eq!(samples.files, [PathBuf::from("kick.wav"), PathBuf::from("snare.wav")]);
/// assert!(samples.text.is_none());
/// assert!(DragData::default().is_empty());
/// ```
///
/// [`WindowCx::drag_data`]: crate::WindowCx::drag_data
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct DragData {
    /// The paths of the files and folders dragged, in the order the source
    /// gave them.
    pub files: Vec<PathBuf>,
    /// The text dragged, if any.
    pub text: Option<String>,
}

impl DragData {
    /// A drag of `files`.
    pub fn files<P: Into<PathBuf>>(files: impl IntoIterator<Item = P>) -> DragData {
        DragData {
            files: files.into_iter().map(Into::into).collect(),
            text: None,
        }
    }

    /// A drag of `text`.
    pub fn text(text: impl Into<String>) -> DragData {
        DragData {
            files: Vec::new(),
            text: Some(text.into()),
        }
    }

    /// The same, with `text` as well.
    #[must_use]
    pub fn with_text(mut self, text: impl Into<String>) -> DragData {
        self.text = Some(text.into());
        self
    }

    /// Whether it carries nothing a window takes: no files and no text.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.text.is_none()
    }
}

/// The drag over a window.
pub(crate) struct DragState {
    pub(crate) data: Rc<DragData>,
    /// Whether the handler heard the drag come in: it then hears it leave
    /// or drop.
    pub(crate) told: bool,
}

/// A drag as a platform reports it, in device pixels of the window.
#[derive(Debug)]
pub(crate) enum RawDrag {
    /// A drag came over the window.
    Enter {
        at: PhysicalPoint,
        data: DragData,
        modifiers: Modifiers,
    },
    /// It moved.
    Over {
        at: PhysicalPoint,
        modifiers: Modifiers,
    },
    /// It left, or was cancelled.
    Leave,
    /// It was dropped.
    Drop {
        at: PhysicalPoint,
        modifiers: Modifiers,
    },
}
