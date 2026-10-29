//! Files dragged from other programs are offered to widgets as drags with
//! no source: to the widget under the pointer, then the ones containing
//! it, until one takes them.

use std::cell::RefCell;
use std::rc::Rc;

use manguebyte_gfx::{Color, Point, Rect, Scene, Size};
use manguebyte_ui::access::{AccessNode, Role};
use manguebyte_ui::layout::{Arrange, Style};
use manguebyte_ui::testing::Harness;
use manguebyte_ui::{
    AccessCx, Button, DragEvent, DragPayload, DropData, Event, EventCx, FileDrop, Handled,
    MeasureCx, Menu, MenuItem, Modifiers, PaintCx, View, Widget,
};

type Log = Rc<RefCell<Vec<(&'static str, &'static str)>>>;

/// What a zone takes.
#[derive(Clone, Copy)]
enum Takes {
    Wav,
    Text,
    AnyFile,
}

/// A drop zone that logs what it hears, takes what it is made to, and
/// lights up while it accepts the drag.
struct Zone {
    name: &'static str,
    takes: Takes,
    log: Log,
    lit: bool,
    dropped: Rc<RefCell<Vec<DropData>>>,
}

impl Zone {
    fn new(name: &'static str, takes: Takes, log: &Log) -> Zone {
        Zone {
            name,
            takes,
            log: Rc::clone(log),
            lit: false,
            dropped: Rc::default(),
        }
    }

    fn takes(&self, payload: &DragPayload) -> bool {
        payload
            .get::<DropData>()
            .is_some_and(|data| match self.takes {
                Takes::Wav => data.has_file_with_extension(&["wav"]),
                Takes::Text => data.text.is_some(),
                Takes::AnyFile => !data.files.is_empty(),
            })
    }

    fn light(&mut self, cx: &mut EventCx<'_>, lit: bool) {
        if lit != self.lit {
            self.lit = lit;
            cx.request_paint();
        }
    }
}

impl Widget for Zone {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Group, self.name).with_value(if self.lit { "lit" } else { "dark" })
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        Size::new(100.0, 100.0)
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let color = if self.lit {
            Color::rgb(200, 40, 40)
        } else {
            Color::rgb(40, 40, 40)
        };
        let rect = cx.rect();
        scene.fill(Rect::new(rect.x, rect.y, rect.width, rect.height), color);
    }

    /// A press of the second button opens a menu.
    fn event(&mut self, cx: &mut EventCx<'_>, event: &Event) -> Handled {
        match event {
            Event::PointerDown {
                button: Button::Secondary,
                ..
            } => {
                cx.open_menu(Menu::new(self.name).item(MenuItem::new(1, "Clear")));
                Handled::Yes
            }
            _ => Handled::No,
        }
    }

    fn drag(&mut self, cx: &mut EventCx<'_>, event: &DragEvent<'_>) -> Handled {
        match event {
            DragEvent::Over { payload, .. } => {
                self.log.borrow_mut().push((self.name, "over"));
                let takes = self.takes(payload);
                self.light(cx, takes);
                if takes { Handled::Yes } else { Handled::No }
            }
            DragEvent::Left => {
                self.log.borrow_mut().push((self.name, "left"));
                self.light(cx, false);
                Handled::No
            }
            DragEvent::Drop { payload, .. } => {
                self.log.borrow_mut().push((self.name, "drop"));
                self.light(cx, false);
                if let Some(data) = payload.get::<DropData>().filter(|_| self.takes(payload)) {
                    self.dropped.borrow_mut().push(data.clone());
                    Handled::Yes
                } else {
                    Handled::No
                }
            }
            _ => Handled::No,
        }
    }
}

/// "Bin" takes any file and holds "Wav" (`.wav` only) on the left and
/// "Text" (text only) on the right, each 100 logical pixels square.
fn bin(log: &Log) -> (View, Rc<RefCell<Vec<DropData>>>) {
    let outer = Zone::new("Bin", Takes::AnyFile, log);
    let dropped = Rc::clone(&outer.dropped);
    let row = Style {
        arrange: Arrange::Row,
        ..Style::leaf()
    };
    let view = View::with_children(
        outer,
        row,
        (
            Zone::new("Wav", Takes::Wav, log),
            Zone::new("Text", Takes::Text, log),
        ),
    );
    (view, dropped)
}

fn take(log: &Log) -> Vec<(&'static str, &'static str)> {
    log.borrow_mut().drain(..).collect()
}

const LEFT: Point = Point::new(50.0, 50.0);
const RIGHT: Point = Point::new(150.0, 50.0);

fn enter(at: Point) -> FileDrop {
    FileDrop::Enter { at }
}

fn over(at: Point) -> FileDrop {
    FileDrop::Over { at }
}

#[test]
fn a_drag_from_outside_goes_to_the_innermost_widget_that_takes_it() {
    let log: Log = Rc::default();
    let (view, dropped) = bin(&log);
    let mut ui = Harness::new(view, Size::new(200.0, 100.0), 1.0);
    ui.frame();
    let samples = DropData::files(["C:/kit/kick.wav", "C:/kit/notes.txt"]);
    let wav = ui.get(Role::Group, "Wav");

    assert_eq!(ui.root_mut().file_drop(enter(LEFT), &samples), Handled::Yes);
    assert_eq!(take(&log), [("Wav", "over")]);
    assert_eq!(ui.root().drop_target(), Some(wav.id));
    // A drag with no source and no ghost: the system draws its own.
    let payload = ui.root().dragging().expect("running");
    assert_eq!(payload.description(), "2 files");
    assert_eq!(payload.get::<DropData>(), Some(&samples));
    ui.frame();
    assert!(ui.find(Role::DragItem, "2 files").is_none());
    take(&log);

    // Over "Text", which takes no files: the drag goes out to "Bin", and
    // "Wav" hears that it left.
    assert_eq!(ui.root_mut().file_drop(over(RIGHT), &samples), Handled::Yes);
    assert_eq!(
        take(&log),
        [("Text", "over"), ("Bin", "over"), ("Wav", "left")]
    );
    assert_eq!(
        ui.root_mut()
            .file_drop(FileDrop::Drop { at: RIGHT }, &samples),
        Handled::Yes
    );
    assert_eq!(
        take(&log),
        [("Text", "over"), ("Bin", "over"), ("Bin", "drop")]
    );
    assert_eq!(*dropped.borrow(), [samples]);
    assert!(ui.root().dragging().is_none());
}

#[test]
fn a_drag_nothing_takes_is_refused_and_a_leave_tells_the_target() {
    let log: Log = Rc::default();
    let (view, dropped) = bin(&log);
    let mut ui = Harness::new(view, Size::new(200.0, 100.0), 1.0);
    ui.frame();
    let words = DropData::text("a phrase");
    assert_eq!(ui.root_mut().file_drop(enter(LEFT), &words), Handled::No);
    assert_eq!(take(&log), [("Wav", "over"), ("Bin", "over")]);
    // A drop nothing takes reaches nobody, and ends the drag.
    assert_eq!(
        ui.root_mut().file_drop(FileDrop::Drop { at: LEFT }, &words),
        Handled::No
    );
    assert_eq!(take(&log), [("Wav", "over"), ("Bin", "over")]);
    assert!(dropped.borrow().is_empty());
    assert!(ui.root().dragging().is_none());

    assert_eq!(ui.root_mut().file_drop(enter(RIGHT), &words), Handled::Yes);
    assert_eq!(take(&log), [("Text", "over")]);
    ui.root_mut().file_drop(FileDrop::Leave, &words);
    assert_eq!(take(&log), [("Text", "left")]);
    assert!(ui.root().dragging().is_none());
    // A leave with no drag tells nobody; a drag of nothing never starts.
    ui.root_mut().file_drop(FileDrop::Leave, &words);
    assert_eq!(
        ui.root_mut().file_drop(enter(LEFT), &DropData::default()),
        Handled::No
    );
    assert_eq!(take(&log), []);
    assert!(ui.root().dragging().is_none());
}

#[test]
fn frames_offer_a_still_drag_again_and_no_position_cancels_it() {
    let log: Log = Rc::default();
    let (view, _) = bin(&log);
    let mut ui = Harness::new(view, Size::new(300.0, 100.0), 1.0);
    ui.frame();
    let samples = DropData::files(["loop.wav"]);
    assert_eq!(ui.root_mut().file_drop(enter(LEFT), &samples), Handled::Yes);
    take(&log);
    // What is under a still pointer may change, so each frame offers it.
    assert!(ui.frame().animating);
    assert_eq!(take(&log), [("Wav", "over")]);
    // Right of both zones, only the bin itself is under the pointer.
    let beside = Point::new(290.0, 50.0);
    assert_eq!(ui.root().hit(beside), Some(ui.get(Role::Group, "Bin").id));
    assert_eq!(
        ui.root_mut().file_drop(over(beside), &samples),
        Handled::Yes
    );
    assert_eq!(take(&log), [("Bin", "over"), ("Wav", "left")]);
    let nowhere = Point::new(f32::NAN, 1.0);
    assert_eq!(
        ui.root_mut().file_drop(over(nowhere), &samples),
        Handled::No
    );
    assert_eq!(take(&log), [("Bin", "left")]);
    assert!(ui.root().dragging().is_none());
}

#[test]
fn a_drag_from_outside_closes_an_open_menu() {
    let log: Log = Rc::default();
    let (view, _) = bin(&log);
    let mut ui = Harness::new(view, Size::new(200.0, 100.0), 1.0);
    ui.frame();
    ui.send(Event::PointerDown {
        at: RIGHT,
        button: Button::Secondary,
        clicks: 1,
        modifiers: Modifiers::NONE,
    });
    ui.frame();
    assert!(ui.root().has_menu());
    let samples = DropData::files(["hat.wav"]);
    assert_eq!(ui.root_mut().file_drop(enter(LEFT), &samples), Handled::Yes);
    assert!(!ui.root().has_menu());
    ui.root_mut().file_drop(FileDrop::Leave, &samples);
    take(&log);
}

/// Starts a drag of its own when pressed and moved.
struct Source;

impl Widget for Source {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Image, "Source")
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        Size::new(100.0, 100.0)
    }

    fn paint(&mut self, _cx: &PaintCx<'_>, _scene: &mut Scene) {}

    fn event(&mut self, cx: &mut EventCx<'_>, event: &Event) -> Handled {
        match event {
            Event::PointerDown { .. } => {
                cx.capture_pointer();
                Handled::Yes
            }
            Event::PointerMove { .. } if cx.has_pointer() => {
                cx.start_drag(DragPayload::new(7u8, "Seven"));
                Handled::Yes
            }
            _ => Handled::No,
        }
    }
}

#[test]
fn a_drag_from_outside_waits_for_one_inside_the_interface() {
    let log: Log = Rc::default();
    let row = Style {
        arrange: Arrange::Row,
        ..Style::leaf()
    };
    let view = View::with_children(
        Zone::new("Bin", Takes::AnyFile, &log),
        row,
        (Source, Zone::new("Wav", Takes::Wav, &log)),
    );
    let mut ui = Harness::new(view, Size::new(200.0, 100.0), 1.0);
    ui.frame();
    ui.press(LEFT);
    ui.move_to(Point::new(60.0, 50.0));
    assert_eq!(
        ui.root().dragging().map(|payload| payload.description()),
        Some("Seven")
    );
    let samples = DropData::files(["late.wav"]);
    assert_eq!(ui.root_mut().file_drop(enter(RIGHT), &samples), Handled::No);
    assert_eq!(
        ui.root_mut()
            .file_drop(FileDrop::Drop { at: RIGHT }, &samples),
        Handled::No
    );
    // The leave of the drag from outside leaves the one inside running.
    ui.root_mut().file_drop(FileDrop::Leave, &samples);
    assert_eq!(
        ui.root().dragging().map(|payload| payload.description()),
        Some("Seven")
    );
}

#[test]
fn a_zone_that_lights_up_is_drawn_again_and_only_it() {
    let log: Log = Rc::default();
    let (view, _) = bin(&log);
    let mut ui = Harness::new(view, Size::new(200.0, 100.0), 2.0);
    ui.frame();
    let samples = DropData::files(["snare.wav"]);
    ui.root_mut().file_drop(enter(LEFT), &samples);
    assert_eq!(ui.get(Role::Group, "Wav").value_text(), Some("lit"));
    let frame = ui.frame();
    assert_eq!(frame.damaged, 1);
    let wav = ui.get(Role::Group, "Wav");
    assert_eq!(ui.last_damage_bounds(), Some(wav.device));
    ui.root_mut().file_drop(FileDrop::Leave, &samples);
    assert_eq!(ui.get(Role::Group, "Wav").value_text(), Some("dark"));
    assert_eq!(ui.frame().damaged, 1);
}
