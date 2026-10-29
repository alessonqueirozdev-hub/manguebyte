//! An application's own window: files dropped onto it are listed, and
//! buttons open the system's dialogs.
//!
//! ```text
//! cargo run -p manguebyte-gui --example app_window
//! ```
//!
//! Drag audio files (or any files) from the file manager onto the list;
//! "Open files" and "Pick folder" add what the dialogs return, and "Save
//! list" writes the list to the file the save dialog names. Closing the
//! window ends the program. Windows only for now: elsewhere it says so and
//! stops.
//!
//! The list and the buttons are widgets of the example's own, written on
//! `manguebyte_ui::Widget`: the list takes drags whose payload is a
//! `DropData` (the files and text a drag from another program carries),
//! and lights up while it would take one.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use manguebyte_gfx::{Point, RoundedRect, Scene, Size, Stroke, TextLayout};
use manguebyte_gui::open_app_window;
use manguebyte_ui::access::{AccessNode, Actions, Role};
use manguebyte_ui::{
    AccessCx, Action, Button, DragEvent, DropData, Event, EventCx, Handled, IntoView, MeasureCx,
    PaintCx, Theme, Widget, column, label, row,
};
use manguebyte_window::{App, AppWindowOptions, Dialogs, FileDialog};

/// The files the program has, shared by the list and the buttons.
type Files = Rc<RefCell<Vec<PathBuf>>>;

/// The window's dialogs, known once the window is open.
type DialogSlot = Rc<RefCell<Option<Dialogs>>>;

/// The list of files: takes drops of files, and shows what it has.
struct FileList {
    files: Files,
    /// How many files it showed when it last drew.
    shown: usize,
    /// Whether a drag it would take is over it.
    lit: bool,
}

impl FileList {
    fn light(&mut self, cx: &mut EventCx<'_>, lit: bool) {
        if self.lit != lit {
            self.lit = lit;
            cx.request_paint();
        }
    }
}

impl Widget for FileList {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        let count = self.files.borrow().len();
        AccessNode::new(Role::Group, "Files").with_value(format!("{count} files"))
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        Size::new(320.0, 160.0)
    }

    fn update(&mut self, cx: &mut manguebyte_ui::UpdateCx<'_>) {
        // The buttons add files too: drawn again when the count changes.
        let count = self.files.borrow().len();
        if count != self.shown {
            self.shown = count;
            cx.request_paint();
        }
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let theme = cx.theme();
        let rect = cx.rect();
        let edge = if self.lit {
            theme.palette.accent
        } else {
            theme.palette.track
        };
        scene.fill(
            RoundedRect::new(rect, theme.corners.medium),
            theme.palette.surface,
        );
        scene.stroke(
            RoundedRect::new(rect, theme.corners.medium),
            Stroke::new(if self.lit { 2.0 } else { 1.0 }),
            edge,
        );
        let style = theme.typography.label.with_color(theme.palette.text);
        let files = self.files.borrow();
        let text = if files.is_empty() {
            "Drop files here".to_owned()
        } else {
            files
                .iter()
                .rev()
                .take(8)
                .map(|file| file.display().to_string())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let layout = TextLayout::new(&text, &style, Some((rect.width - 16.0).max(0.0)));
        scene.text(&layout, Point::new(rect.x + 8.0, rect.y + 8.0));
    }

    fn drag(&mut self, cx: &mut EventCx<'_>, event: &DragEvent<'_>) -> Handled {
        let files = |payload: &manguebyte_ui::DragPayload| {
            payload
                .get::<DropData>()
                .filter(|data| !data.files.is_empty())
                .map(|data| data.files.clone())
        };
        match event {
            DragEvent::Over { payload, .. } => {
                let takes = files(payload).is_some();
                self.light(cx, takes);
                if takes { Handled::Yes } else { Handled::No }
            }
            DragEvent::Drop { payload, .. } => {
                self.light(cx, false);
                match files(payload) {
                    Some(dropped) => {
                        println!("dropped: {dropped:?}");
                        self.files.borrow_mut().extend(dropped);
                        Handled::Yes
                    }
                    None => Handled::No,
                }
            }
            _ => {
                self.light(cx, false);
                Handled::No
            }
        }
    }
}

/// A button that runs `press` when clicked, or pressed by assistive
/// technology.
struct PushButton {
    text: &'static str,
    press: Box<dyn FnMut()>,
    down: bool,
}

fn button(text: &'static str, press: impl FnMut() + 'static) -> PushButton {
    PushButton {
        text,
        press: Box::new(press),
        down: false,
    }
}

impl Widget for PushButton {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Button, self.text).with_actions(Actions::PRESS)
    }

    fn measure(&mut self, cx: &MeasureCx<'_>) -> Size {
        let style = cx.theme().typography.label;
        let size = TextLayout::new(self.text, &style, None).size();
        Size::new(size.width.ceil() + 24.0, size.height.ceil() + 12.0)
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let theme = cx.theme();
        let rect = cx.rect();
        let fill = if self.down {
            theme.palette.accent
        } else {
            theme.palette.track
        };
        scene.fill(RoundedRect::new(rect, theme.corners.medium), fill);
        let style = theme.typography.label.with_color(theme.palette.text);
        let layout = TextLayout::new(self.text, &style, None);
        let size = layout.size();
        let at = Point::new(
            rect.x + (rect.width - size.width) / 2.0,
            rect.y + (rect.height - size.height) / 2.0,
        );
        scene.text(&layout, at);
    }

    fn event(&mut self, cx: &mut EventCx<'_>, event: &Event) -> Handled {
        match *event {
            Event::PointerDown {
                button: Button::Primary,
                ..
            } => {
                self.down = true;
                cx.capture_pointer();
                cx.request_paint();
                Handled::Yes
            }
            Event::PointerUp { at, .. } if self.down => {
                self.down = false;
                cx.request_paint();
                if cx.rect().contains(at) {
                    (self.press)();
                }
                Handled::Yes
            }
            _ => Handled::No,
        }
    }

    fn act(&mut self, _cx: &mut EventCx<'_>, action: Action) -> bool {
        if action == Action::Press {
            (self.press)();
            return true;
        }
        false
    }
}

/// Runs `show` with the window's dialogs, and adds what it returns.
fn ask(dialogs: &DialogSlot, files: &Files, show: impl FnOnce(&Dialogs) -> Vec<PathBuf>) {
    let Some(dialogs) = dialogs.borrow().clone() else {
        return;
    };
    let chosen = show(&dialogs);
    println!("chosen: {chosen:?}");
    files.borrow_mut().extend(chosen);
}

fn main() {
    let app = match App::new() {
        Ok(app) => app,
        Err(error) => {
            eprintln!("no application windows here: {error}");
            return;
        }
    };
    let files: Files = Rc::default();
    let dialogs: DialogSlot = Rc::default();

    let open = {
        let (dialogs, files) = (Rc::clone(&dialogs), Rc::clone(&files));
        move || {
            ask(&dialogs, &files, |dialogs| {
                let dialog = FileDialog::new()
                    .with_title("Add samples")
                    .with_filter("Audio", ["wav", "flac", "aiff", "mp3", "ogg"])
                    .with_filter("All files", [""; 0])
                    .multiple();
                dialogs
                    .open_files(&dialog)
                    .ok()
                    .flatten()
                    .unwrap_or_default()
            });
        }
    };
    let folder = {
        let (dialogs, files) = (Rc::clone(&dialogs), Rc::clone(&files));
        move || {
            ask(&dialogs, &files, |dialogs| {
                let dialog = FileDialog::new().with_title("Add a folder of samples");
                dialogs
                    .pick_folder(&dialog)
                    .ok()
                    .flatten()
                    .into_iter()
                    .collect()
            });
        }
    };
    let save = {
        let (dialogs, files) = (Rc::clone(&dialogs), Rc::clone(&files));
        move || {
            let Some(dialogs) = dialogs.borrow().clone() else {
                return;
            };
            let dialog = FileDialog::new()
                .with_title("Save the list")
                .with_filter("Text", ["txt"])
                .with_file_name("samples.txt");
            if let Ok(Some(path)) = dialogs.save_file(&dialog) {
                let list: Vec<String> = files
                    .borrow()
                    .iter()
                    .map(|file| file.display().to_string())
                    .collect();
                match std::fs::write(&path, list.join("\n")) {
                    Ok(()) => println!("saved {}", path.display()),
                    Err(error) => eprintln!("could not save {}: {error}", path.display()),
                }
            }
        }
    };

    let view = column((
        label("Drop files on the list, or use the buttons").title(),
        FileList {
            files: Rc::clone(&files),
            shown: 0,
            lit: false,
        }
        .flex(1.0),
        row((
            button("Open files", open),
            button("Pick folder", folder),
            button("Save list", save),
        ))
        .spacing(8.0),
    ))
    .padding(12.0)
    .spacing(8.0);

    let window = match open_app_window(
        &app,
        AppWindowOptions::new("Manguebyte application window", Size::new(520.0, 340.0))
            .with_min_size(Size::new(400.0, 260.0)),
        Theme::default(),
        None,
        view,
    ) {
        Ok(window) => window,
        Err(error) => {
            eprintln!("the window did not open: {error}");
            return;
        }
    };
    *dialogs.borrow_mut() = Some(window.dialogs());
    match app.run() {
        Ok(end) => println!("the loop ended: {end:?}; {} files", files.borrow().len()),
        Err(error) => eprintln!("the loop failed: {error}"),
    }
}
