//! Two lists inside a scroll view, with rows dragged from one to the
//! other, drawn headless into PNG files.
//!
//! ```text
//! cargo run -p manguebyte-ui --example views_demo
//! cargo run -p manguebyte-ui --example views_demo -- <folder>
//! ```
//!
//! The program builds a small sample browser (samples on the left, the
//! zones of an instrument on the right, a picture and notes below, all in
//! a scroll view taller than the window), then drives it as a person
//! would, with no window: selects two samples, drags them onto the zones,
//! and scrolls the view down. It writes three pictures at scale 2 into the
//! folder given, or the system's temporary folder: during the drag, after
//! the drop, and scrolled. It prints what the application saw, which it
//! polls from the lists' states as an editor would once a frame.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use manguebyte_gfx::{Image, Pixmap, Point, Size};
use manguebyte_plugin::editor::EditorContext;
use manguebyte_plugin::editor::testing::RecordingEditorHost;
use manguebyte_ui::{
    Button, DragPayload, Event, Fit, IntoView, ListData, ListEvent, ListState, Modifiers, Role,
    Root, column, image, label, list_view, panel, row, scroll_view,
};

const SIZE: Size = Size::new(420.0, 300.0);
const SCALE: f32 = 2.0;

/// Names the application keeps, with a version it bumps on each change.
#[derive(Clone)]
struct Shelf {
    names: Rc<RefCell<Vec<String>>>,
    version: Rc<Cell<u64>>,
}

impl Shelf {
    fn new(names: &[&str]) -> Shelf {
        Shelf {
            names: Rc::new(RefCell::new(names.iter().map(|&n| n.to_owned()).collect())),
            version: Rc::new(Cell::new(0)),
        }
    }
}

/// What dragging rows carries: which shelf they come from, and which rows.
struct Rows {
    from: &'static str,
    rows: Vec<usize>,
}

/// A shelf as a list: its rows can be dragged out and rows dropped in.
struct ShelfRows {
    name: &'static str,
    shelf: Shelf,
}

impl ListData for ShelfRows {
    fn len(&self) -> usize {
        self.shelf.names.borrow().len()
    }

    fn name(&self, row: usize) -> String {
        self.shelf
            .names
            .borrow()
            .get(row)
            .cloned()
            .unwrap_or_default()
    }

    fn revision(&self) -> u64 {
        self.shelf.version.get()
    }

    fn drag_payload(&self, rows: &[usize]) -> Option<DragPayload> {
        let description = match rows {
            [row] => self.name(*row),
            rows => format!("{} samples", rows.len()),
        };
        Some(DragPayload::new(
            Rows {
                from: self.name,
                rows: rows.to_vec(),
            },
            description,
        ))
    }

    fn accepts(&self, payload: &DragPayload, _index: usize) -> bool {
        payload.is::<Rows>()
    }
}

/// A gradient, standing in for an instrument's picture: 128 by 64 pixels,
/// made for scale 2.
fn picture() -> Image {
    let (width, height) = (128u32, 64u32);
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            rgba.extend_from_slice(&[(x * 2) as u8, (80 + y) as u8, (255 - x * 2) as u8, 255]);
        }
    }
    let pixels = Pixmap::from_straight(width, height, &rgba)
        .unwrap_or_else(|_| Pixmap::new(1, 1).expect("one pixel"));
    Image::new(pixels)
}

struct Demo {
    root: Root,
    clock: Duration,
    _link: manguebyte_plugin::editor::HostLink,
}

impl Demo {
    fn frame(&mut self) {
        self.clock += Duration::from_millis(16);
        self.root.frame(self.clock);
    }

    fn send(&mut self, event: Event) {
        self.root.event(event);
    }

    /// Where the row named `name` is, found as a screen reader finds it.
    fn row(&self, name: &str) -> Point {
        self.root
            .access_tree()
            .into_iter()
            .find(|entry| entry.node.role == Role::ListItem && entry.node.name == name)
            .map(|entry| entry.bounds.center())
            .unwrap_or_else(|| panic!("no row named {name}"))
    }

    fn view_center(&self, name: &str) -> Point {
        self.root
            .access_tree()
            .into_iter()
            .find(|entry| entry.node.role == Role::ScrollView && entry.node.name == name)
            .map(|entry| entry.bounds.center())
            .unwrap_or_else(|| panic!("no view named {name}"))
    }

    fn click(&mut self, at: Point, modifiers: Modifiers) {
        self.send(Event::PointerDown {
            at,
            button: Button::Primary,
            clicks: 1,
            modifiers,
        });
        self.send(Event::PointerUp {
            at,
            button: Button::Primary,
            modifiers,
        });
        self.frame();
    }

    fn save(&self, folder: &std::path::Path, name: &str) -> std::io::Result<PathBuf> {
        let path = folder.join(name);
        let png = self
            .root
            .pixels()
            .encode_png()
            .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
        std::fs::write(&path, png)?;
        Ok(path)
    }
}

/// Moves the rows a drop names to `index` of `to`, as the application
/// does when it polls a drop, and selects them there.
fn apply(
    shelves: &[(&'static str, &Shelf, &ListState)],
    to: &'static str,
    index: usize,
    payload: &DragPayload,
) {
    let Some(rows) = payload.get::<Rows>() else {
        return;
    };
    let find = |name: &str| shelves.iter().find(|(n, _, _)| *n == name);
    let (Some((_, from, from_state)), Some((_, target, state))) = (find(rows.from), find(to))
    else {
        return;
    };
    let mut index = index;
    let mut moved = Vec::new();
    for &row in rows.rows.iter().rev() {
        moved.push(from.names.borrow_mut().remove(row));
        if rows.from == to && row < index {
            index -= 1;
        }
    }
    moved.reverse();
    let count = moved.len();
    for (at, name) in moved.into_iter().enumerate() {
        target.names.borrow_mut().insert(index + at, name);
    }
    from.version.set(from.version.get() + 1);
    target.version.set(target.version.get() + 1);
    from_state.clear_selection();
    state.set_selected(index..index + count);
}

fn main() -> std::io::Result<()> {
    let folder = std::env::args_os()
        .nth(1)
        .map_or_else(std::env::temp_dir, PathBuf::from);

    let samples = Shelf::new(&[
        "Kick.wav",
        "Snare.wav",
        "Clap.wav",
        "Closed hat.wav",
        "Open hat.wav",
        "Ride.wav",
        "Crash.wav",
        "Low tom.wav",
        "High tom.wav",
        "Shaker.wav",
    ]);
    let zones = Shelf::new(&["C1 Bass.wav", "C2 Pad.wav"]);
    let samples_state = ListState::new();
    let zones_state = ListState::new();

    let lists = row((
        list_view(
            "Samples",
            ShelfRows {
                name: "Samples",
                shelf: samples.clone(),
            },
            samples_state.clone(),
        )
        .flex(1.0),
        list_view(
            "Zones",
            ShelfRows {
                name: "Zones",
                shelf: zones.clone(),
            },
            zones_state.clone(),
        )
        .flex(1.0),
    ))
    .spacing(12.0)
    .height(168.0);
    let notes: Vec<_> = [
        "Drag samples onto the zones to map them.",
        "Ctrl and a click picks more than one; Shift picks a run.",
        "Double-click a sample, or press Enter, to hear it.",
        "The view scrolls with the wheel, its bar and the keys.",
    ]
    .into_iter()
    .map(label)
    .collect();
    let content = column((
        label("Sample browser").title(),
        lists,
        image("Instrument", picture())
            .density(2.0)
            .fit(Fit::Contain)
            .height(64.0),
        column(notes).spacing(4.0),
    ))
    .spacing(10.0)
    .padding(12.0);
    let view = panel(scroll_view("Browser", content));

    let (context, link) = EditorContext::attach(Rc::new(RecordingEditorHost::new()));
    let mut demo = Demo {
        root: Root::new(view, SIZE, SCALE, context),
        clock: Duration::ZERO,
        _link: link,
    };
    demo.frame();

    // Pick two samples, then drag them, from the second, to between the
    // two zones.
    let control = Modifiers {
        control: true,
        ..Modifiers::NONE
    };
    demo.click(demo.row("Snare.wav"), Modifiers::NONE);
    demo.click(demo.row("Clap.wav"), control);
    let from = demo.row("Clap.wav");
    let to = {
        let first = demo.row("C1 Bass.wav");
        Point::new(first.x, first.y + 12.0)
    };
    demo.send(Event::PointerDown {
        at: from,
        button: Button::Primary,
        clicks: 1,
        modifiers: Modifiers::NONE,
    });
    let steps = 12;
    for step in 1..=steps {
        let t = step as f32 / steps as f32;
        let at = Point::new(from.x + (to.x - from.x) * t, from.y + (to.y - from.y) * t);
        demo.send(Event::PointerMove {
            at,
            modifiers: Modifiers::NONE,
        });
        demo.frame();
    }
    let dragging = demo.save(&folder, "views_demo_dragging.png")?;
    demo.send(Event::PointerUp {
        at: to,
        button: Button::Primary,
        modifiers: Modifiers::NONE,
    });

    // What an editor polls once a frame.
    let shelves = [
        ("Samples", &samples, &samples_state),
        ("Zones", &zones, &zones_state),
    ];
    for (name, _, state) in shelves {
        for event in state.take_events() {
            match event {
                ListEvent::Dropped { index, payload } => {
                    println!("{name}: dropped {:?} at {index}", payload.description());
                    apply(&shelves, name, index, &payload);
                }
                ListEvent::Activated(row) => println!("{name}: activated row {row}"),
                other => println!("{name}: {other:?}"),
            }
        }
    }
    demo.frame();
    let dropped = demo.save(&folder, "views_demo_dropped.png")?;

    // Scroll the browser down to the notes.
    let center = demo.view_center("Browser");
    for _ in 0..3 {
        demo.send(Event::Wheel {
            at: Point::new(center.x, SIZE.height - 20.0),
            delta: Point::new(0.0, -1.0),
            modifiers: Modifiers::NONE,
        });
    }
    demo.frame();
    let scrolled = demo.save(&folder, "views_demo_scrolled.png")?;

    println!("Samples: {:?}", samples.names.borrow());
    println!("Zones:   {:?}", zones.names.borrow());
    println!("Zones selected: {:?}", zones_state.selected());
    for path in [dragging, dropped, scrolled] {
        println!("wrote {}", path.display());
    }
    Ok(())
}
