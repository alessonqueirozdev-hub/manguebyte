//! The system's dialogs for opening files, picking folders and saving.

use std::fmt;
use std::path::PathBuf;
use std::rc::{Rc, Weak};

use crate::window::{Inner, WindowError};

/// A kind of file, as a dialog lists it: a name and the extensions that
/// make it.
///
/// ```
/// use manguebyte_window::FileFilter;
///
/// let audio = FileFilter::new("Audio", ["wav", "flac", ".aiff"]);
/// assert_eq!(audio.pattern(), "*.wav;*.flac;*.aiff");
/// assert_eq!(FileFilter::new("Everything", [""; 0]).pattern(), "*.*");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileFilter {
    /// What the person reads, such as "Audio".
    pub name: String,
    /// The extensions, without the dot, such as `wav`. None means every
    /// file.
    pub extensions: Vec<String>,
}

impl FileFilter {
    /// Files named `name` with any of `extensions`; a leading dot is
    /// dropped.
    pub fn new<E: Into<String>>(
        name: impl Into<String>,
        extensions: impl IntoIterator<Item = E>,
    ) -> FileFilter {
        FileFilter {
            name: name.into(),
            extensions: extensions
                .into_iter()
                .map(|extension| {
                    let extension: String = extension.into();
                    extension.trim_start_matches('.').to_owned()
                })
                .filter(|extension| !extension.is_empty())
                .collect(),
        }
    }

    /// The patterns the system matches, separated by semicolons: `*.wav`
    /// for each extension, or `*.*` for every file.
    pub fn pattern(&self) -> String {
        if self.extensions.is_empty() {
            return "*.*".to_owned();
        }
        self.extensions
            .iter()
            .map(|extension| format!("*.{extension}"))
            .collect::<Vec<_>>()
            .join(";")
    }
}

/// What a file dialog asks: its title, the kinds of file it lists, where
/// it starts and the name it offers.
///
/// The same description serves the three dialogs, each of which uses what
/// applies to it: [`open_files`](crate::WindowCx::open_files) the filters
/// and [`multiple`](Self::multiple), [`pick_folder`](crate::WindowCx::pick_folder)
/// neither, and [`save_file`](crate::WindowCx::save_file) the filters and
/// the [`file_name`](Self::file_name), and adds the first filter's first
/// extension to a name typed without one.
///
/// ```
/// use std::path::PathBuf;
/// use manguebyte_window::FileDialog;
///
/// let dialog = FileDialog::new()
///     .with_title("Add samples")
///     .with_filter("Audio", ["wav", "flac"])
///     .in_folder("C:/Samples")
///     .multiple();
/// assert_eq!(dialog.filters.len(), 1);
/// assert_eq!(dialog.folder, Some(PathBuf::from("C:/Samples")));
/// assert!(dialog.multiple);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct FileDialog {
    /// The dialog's title, or the system's own when `None`.
    pub title: Option<String>,
    /// The kinds of file listed, the first chosen at the start. None lists
    /// every file.
    pub filters: Vec<FileFilter>,
    /// The folder the dialog starts in, or the system's choice (usually
    /// the last one used) when `None`.
    pub folder: Option<PathBuf>,
    /// The name a save dialog offers.
    pub file_name: Option<String>,
    /// Whether an open dialog takes several files.
    pub multiple: bool,
}

impl FileDialog {
    /// A dialog with the system's title, every file listed and the
    /// system's starting folder.
    pub fn new() -> FileDialog {
        FileDialog::default()
    }

    /// The same, titled `title`.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> FileDialog {
        self.title = Some(title.into());
        self
    }

    /// The same, listing files named `name` with any of `extensions` as
    /// well (see [`FileFilter::new`]).
    #[must_use]
    pub fn with_filter<E: Into<String>>(
        mut self,
        name: impl Into<String>,
        extensions: impl IntoIterator<Item = E>,
    ) -> FileDialog {
        self.filters.push(FileFilter::new(name, extensions));
        self
    }

    /// The same, starting in `folder`.
    #[must_use]
    pub fn in_folder(mut self, folder: impl Into<PathBuf>) -> FileDialog {
        self.folder = Some(folder.into());
        self
    }

    /// The same, offering `name` to save as.
    #[must_use]
    pub fn with_file_name(mut self, name: impl Into<String>) -> FileDialog {
        self.file_name = Some(name.into());
        self
    }

    /// The same, taking several files when it opens.
    #[must_use]
    pub fn multiple(mut self) -> FileDialog {
        self.multiple = true;
        self
    }

    /// The extension a save dialog adds to a name typed without one: the
    /// first filter's first.
    pub fn default_extension(&self) -> Option<&str> {
        self.filters
            .first()
            .and_then(|filter| filter.extensions.first())
            .map(String::as_str)
    }
}

/// Which dialog was shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DialogKind {
    /// [`open_files`](crate::WindowCx::open_files).
    OpenFiles,
    /// [`pick_folder`](crate::WindowCx::pick_folder).
    PickFolder,
    /// [`save_file`](crate::WindowCx::save_file).
    SaveFile,
}

/// A dialog a window was asked to show, as a headless window records it
/// ([`Headless::take_dialogs`](crate::Headless::take_dialogs)).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DialogRequest {
    /// Which dialog.
    pub kind: DialogKind,
    /// What it was asked with.
    pub dialog: FileDialog,
}

/// What the person chose, as the dialog of `kind` gives it: one path
/// unless an open dialog takes several, and `None` for nothing.
pub(crate) fn chosen(
    kind: DialogKind,
    dialog: &FileDialog,
    paths: Option<Vec<PathBuf>>,
) -> Option<Vec<PathBuf>> {
    let mut paths = paths.filter(|paths| !paths.is_empty())?;
    if !(kind == DialogKind::OpenFiles && dialog.multiple) {
        paths.truncate(1);
    }
    Some(paths)
}

/// The system's file dialogs, through a window, for an interface to keep:
/// what its buttons call. A handle from
/// [`Window::dialogs`](crate::Window::dialogs), the same as the window's
/// own [`open_files`](crate::WindowCx::open_files) and its siblings. It
/// holds the window weakly: once the window is gone every call fails with
/// [`WindowError::Closed`]. It belongs to the window's thread.
///
/// ```
/// use std::path::PathBuf;
/// use manguebyte_window::{DialogKind, FileDialog, Handled, PhysicalSize, Window, WindowCx, WindowOptions};
///
/// let (window, headless) = Window::headless(
///     WindowOptions::new(PhysicalSize::new(10, 10)),
///     |_: &WindowCx<'_>, _| Handled::Yes,
/// )
/// .expect("valid options");
/// let dialogs = window.dialogs();
/// headless.answer_next_dialog(Some(vec![PathBuf::from("drums")]));
/// let folder = dialogs.pick_folder(&FileDialog::new().with_title("Samples")).expect("open");
/// assert_eq!(folder, Some(PathBuf::from("drums")));
/// // Nothing scripted: the person cancels.
/// assert_eq!(dialogs.open_files(&FileDialog::new()).expect("open"), None);
/// let asked = headless.take_dialogs();
/// assert_eq!(asked[0].kind, DialogKind::PickFolder);
/// assert_eq!(asked[1].kind, DialogKind::OpenFiles);
/// ```
#[derive(Clone)]
pub struct Dialogs {
    inner: Weak<Inner>,
}

impl Dialogs {
    pub(crate) fn new(inner: &Rc<Inner>) -> Dialogs {
        Dialogs {
            inner: Rc::downgrade(inner),
        }
    }

    fn show(
        &self,
        kind: DialogKind,
        dialog: &FileDialog,
    ) -> Result<Option<Vec<PathBuf>>, WindowError> {
        let inner = self.inner.upgrade().ok_or(WindowError::Closed)?;
        inner.dialog(kind, dialog)
    }

    /// As [`WindowCx::open_files`](crate::WindowCx::open_files).
    ///
    /// # Errors
    ///
    /// As there, and [`WindowError::Closed`] once the window is gone.
    pub fn open_files(&self, dialog: &FileDialog) -> Result<Option<Vec<PathBuf>>, WindowError> {
        self.show(DialogKind::OpenFiles, dialog)
    }

    /// As [`WindowCx::pick_folder`](crate::WindowCx::pick_folder).
    ///
    /// # Errors
    ///
    /// As there, and [`WindowError::Closed`] once the window is gone.
    pub fn pick_folder(&self, dialog: &FileDialog) -> Result<Option<PathBuf>, WindowError> {
        Ok(self
            .show(DialogKind::PickFolder, dialog)?
            .and_then(|paths| paths.into_iter().next()))
    }

    /// As [`WindowCx::save_file`](crate::WindowCx::save_file).
    ///
    /// # Errors
    ///
    /// As there, and [`WindowError::Closed`] once the window is gone.
    pub fn save_file(&self, dialog: &FileDialog) -> Result<Option<PathBuf>, WindowError> {
        Ok(self
            .show(DialogKind::SaveFile, dialog)?
            .and_then(|paths| paths.into_iter().next()))
    }
}

impl fmt::Debug for Dialogs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dialogs")
            .field("window", &(self.inner.strong_count() > 0))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_open_dialog_for_several_files_gives_several() {
        let paths = || Some(vec![PathBuf::from("a"), PathBuf::from("b")]);
        let many = FileDialog::new().multiple();
        let one = FileDialog::new();
        assert_eq!(chosen(DialogKind::OpenFiles, &many, paths()), paths());
        assert_eq!(
            chosen(DialogKind::OpenFiles, &one, paths()),
            Some(vec![PathBuf::from("a")])
        );
        assert_eq!(
            chosen(DialogKind::SaveFile, &many, paths()),
            Some(vec![PathBuf::from("a")])
        );
        assert_eq!(chosen(DialogKind::PickFolder, &one, Some(Vec::new())), None);
        assert_eq!(chosen(DialogKind::PickFolder, &one, None), None);
    }

    #[test]
    fn the_default_extension_is_the_first_filters_first() {
        let dialog = FileDialog::new()
            .with_filter("Presets", ["mbpreset", "json"])
            .with_filter("Text", ["txt"]);
        assert_eq!(dialog.default_extension(), Some("mbpreset"));
        assert_eq!(FileDialog::new().default_extension(), None);
    }
}
