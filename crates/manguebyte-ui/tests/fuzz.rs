//! Random and hostile input in random order: nothing panics, every
//! gesture the host sees is well formed and ends, and the back buffer is
//! always what one whole render of the scene gives.
//!
//! The sequences come from a fixed seed, so a failure repeats; the seed
//! and step are in the message.

mod common;

use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use manguebyte_gfx::{Point, Rect, Size};
use manguebyte_params::Param;
use manguebyte_plugin::editor::testing::RecordingEditorHost;
use manguebyte_ui::layout::{Insets, LayoutNode, Style, solve};
use manguebyte_ui::testing::{Found, Harness};
use manguebyte_ui::{Action, Button, Composition, Event, Key, KeyEvent, Modifiers};

use common::{PANEL, levels, panel_view, params};

struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn unit(&mut self) -> f32 {
        self.below(1 << 20) as f32 / (1 << 20) as f32
    }

    /// A number, sometimes one no input should carry.
    fn number(&mut self, typical: f32) -> f32 {
        match self.below(16) {
            0 => f32::NAN,
            1 => f32::INFINITY,
            2 => f32::NEG_INFINITY,
            3 => -1e30,
            4 => 1e30,
            5 => -0.0,
            _ => (self.unit() * 2.0 - 0.5) * typical,
        }
    }

    fn modifiers(&mut self) -> Modifiers {
        Modifiers {
            shift: self.below(3) == 0,
            control: self.below(8) == 0,
            alt: self.below(8) == 0,
            meta: false,
        }
    }
}

/// A point on a widget, near one, or nowhere sensible.
fn point(random: &mut Random, widgets: &[Found]) -> Point {
    if !widgets.is_empty() && random.below(3) != 0 {
        let widget = &widgets[random.below(widgets.len() as u64) as usize];
        let b = widget.bounds;
        return Point::new(
            b.x + b.width * random.unit(),
            b.y + b.height * random.unit(),
        );
    }
    Point::new(random.number(PANEL.width), random.number(PANEL.height))
}

const KEYS: [Key; 14] = [
    Key::Up,
    Key::Down,
    Key::Left,
    Key::Right,
    Key::PageUp,
    Key::PageDown,
    Key::Home,
    Key::End,
    Key::Escape,
    Key::Tab,
    Key::Enter,
    Key::Delete,
    Key::Character(' '),
    Key::Character('x'),
];

/// Characters to type: digits and units that read as values, and ones no
/// field should choke on.
const TEXT: [char; 16] = [
    '1',
    '5',
    '0',
    '-',
    '.',
    'k',
    'H',
    'z',
    ' ',
    'é',
    '\u{301}',
    '\u{200D}',
    '\u{1F44D}',
    '\u{1F3FD}',
    '\n',
    '\u{0}',
];

#[test]
fn random_input_keeps_gestures_well_formed_and_frames_exact() {
    // Steps, over every seed, after which a value was being typed.
    let mut typing = 0;
    for seed in 1..=12_u64 {
        let params = params();
        let levels = levels();
        // Seeds 1 to 8 with a host that stores every value at once; 9 to
        // 12 with one that stores nothing, as a host that echoes late or
        // never, so that controls wait for echoes that do not come.
        let host = if seed <= 8 {
            Rc::new(
                RecordingEditorHost::new()
                    .with_params(Arc::clone(&params) as Arc<dyn manguebyte_params::ParamSet>),
            )
        } else {
            Rc::new(RecordingEditorHost::new())
        };
        let scale = [1.0, 1.25, 1.5, 2.0][seed as usize % 4];
        let mut ui = Harness::with_host(panel_view(&params, &levels), PANEL, scale, host.clone());
        ui.frame();
        let mut random = Random(0x9E37_79B9_7F4A_7C15 ^ seed);
        let mut clock = Duration::ZERO;
        let mut checks = 0;
        for step in 0..500 {
            let widgets = ui.everything();
            let at = point(&mut random, &widgets);
            let modifiers = random.modifiers();
            match random.below(16) {
                0 | 1 => {
                    ui.send(Event::PointerDown {
                        at,
                        button: if random.below(5) == 0 {
                            Button::Secondary
                        } else {
                            Button::Primary
                        },
                        clicks: 1 + random.below(3) as u32,
                        modifiers,
                    });
                }
                2..=4 => {
                    ui.send(Event::PointerMove { at, modifiers });
                }
                5 => {
                    ui.send(Event::PointerUp {
                        at,
                        button: Button::Primary,
                        modifiers,
                    });
                }
                6 => {
                    ui.send(Event::PointerCancel);
                }
                7 => {
                    let delta = Point::new(random.number(2.0), random.number(4.0));
                    ui.send(Event::Wheel {
                        at,
                        delta,
                        modifiers,
                    });
                }
                8 | 9 => match random.below(5) {
                    // Typed text and input methods, which knobs and sliders
                    // take while a value is being typed into them.
                    0 => {
                        let c = TEXT[random.below(TEXT.len() as u64) as usize];
                        ui.send(Event::Text(c));
                    }
                    1 => {
                        let step = match random.below(5) {
                            0 => Composition::Start,
                            1 => Composition::Clear,
                            2 => Composition::Caret(random.next() as u32),
                            3 => Composition::End,
                            _ => Composition::Push(TEXT[random.below(TEXT.len() as u64) as usize]),
                        };
                        ui.send(Event::Composition(step));
                    }
                    _ => {
                        let key = KEYS[random.below(KEYS.len() as u64) as usize];
                        ui.send(Event::Key(KeyEvent {
                            key,
                            modifiers,
                            pressed: random.below(4) != 0,
                        }));
                    }
                },
                10 => {
                    ui.send(Event::Focus(random.below(2) == 0));
                }
                11 => {
                    if !widgets.is_empty() {
                        let widget = &widgets[random.below(widgets.len() as u64) as usize];
                        let action = match random.below(5) {
                            0 => Action::Focus,
                            1 => Action::Press,
                            2 => Action::Increment,
                            3 => Action::Decrement,
                            _ => Action::SetValue(f64::from(random.number(30.0))),
                        };
                        ui.act(widget, action);
                    }
                }
                12 => {
                    // What the audio thread publishes, sane or not.
                    levels.left.store(random.number(80.0), Ordering::Relaxed);
                    levels.right.store(random.number(80.0), Ordering::Relaxed);
                }
                13 => {
                    // Host automation, written to the parameters directly.
                    params
                        .resonance
                        .set_normalized(f64::from(random.number(1.0)));
                    params.cutoff.set_plain(f64::from(random.number(20_000.0)));
                    params.bypass.set_normalized(f64::from(random.unit()));
                }
                14 => {
                    if random.below(8) == 0 {
                        // Sizes that make no sense, and sensible ones; not
                        // huge ones, which only cost time here (the buffer
                        // is capped; see `the_buffer_is_capped`).
                        let mut side = |most: f32| match random.below(6) {
                            0 => f32::NAN,
                            1 => f32::INFINITY,
                            2 => -5.0,
                            3 => 0.0,
                            _ => random.unit() * most,
                        };
                        let size = Size::new(side(600.0), side(400.0));
                        let scale =
                            [0.0, 0.5, 1.0, 1.5, 2.0, f32::NAN, 9.0][random.below(7) as usize];
                        ui.resize(size, scale);
                    }
                }
                _ => {
                    // The clock may even go backwards.
                    clock = if random.below(10) == 0 {
                        clock.saturating_sub(Duration::from_millis(random.below(50)))
                    } else {
                        clock + Duration::from_millis(random.below(40))
                    };
                    ui.root_mut().frame(clock);
                    if random.below(3) == 0 {
                        checks += 1;
                        assert!(
                            ui.pixels() == &ui.render_fresh(),
                            "seed {seed}, step {step}: the buffer is not a whole render"
                        );
                    }
                }
            }
            if let Err(problem) = host.well_formed() {
                panic!("seed {seed}, step {step}: {problem}");
            }
            if ui.root().wants_text() {
                typing += 1;
            }
        }
        assert!(checks > 0);
        let begins = host
            .calls()
            .iter()
            .filter(|call| {
                matches!(
                    call,
                    manguebyte_plugin::editor::testing::HostCall::BeginGesture(_)
                )
            })
            .count();
        assert!(begins > 0, "seed {seed}: no gesture was ever begun");
        ui.close();
        assert_eq!(host.well_formed(), Ok(()), "seed {seed}");
        assert!(
            host.open_at_host().is_empty(),
            "seed {seed}: a gesture never ended"
        );
    }
    eprintln!("a value was being typed after {typing} of 6000 steps");
    assert!(typing > 0, "no value was ever typed");
}

#[test]
fn input_before_the_first_frame_and_after_closing_a_drag_is_harmless() {
    let params = params();
    let levels = levels();
    let host = Rc::new(RecordingEditorHost::new());
    let mut ui = Harness::with_host(panel_view(&params, &levels), PANEL, 1.0, host.clone());
    // Nothing is laid out yet, so nothing is hit.
    assert!(!ui.press(Point::new(100.0, 100.0)));
    assert!(!ui.move_to(Point::new(100.0, 50.0)));
    assert!(!ui.release(Point::new(100.0, 50.0)));
    assert!(!ui.key(Key::Up));
    assert!(ui.tab(), "Tab moves the focus even before a frame");
    assert!(host.calls().is_empty() || host.well_formed().is_ok());
    ui.frame();
    // Releases and cancels without a press.
    assert!(!ui.release(Point::new(1.0, 1.0)));
    assert!(!ui.send(Event::PointerCancel));
    let calls = ui.close();
    assert!(calls.iter().all(|call| !matches!(
        call,
        manguebyte_plugin::editor::testing::HostCall::Perform(_, v) if !(0.0..=1.0).contains(v)
    )));
    assert_eq!(host.well_formed(), Ok(()));
}

#[test]
fn the_buffer_is_capped() {
    let params = params();
    let levels = levels();
    let mut ui = Harness::new(panel_view(&params, &levels), Size::new(5_000.0, 10.0), 8.0);
    ui.frame();
    let side = manguebyte_ui::MAX_BUFFER_SIDE;
    assert_eq!(ui.pixels().width(), side);
    assert_eq!(ui.pixels().height(), 80);
    ui.resize(Size::new(f32::INFINITY, f32::NAN), 1.0);
    ui.frame();
    assert_eq!((ui.pixels().width(), ui.pixels().height()), (1, 1));
}

#[test]
fn hostile_layout_trees_never_panic() {
    let mut random = Random(0xBAD_C0DE);
    for case in 0..500 {
        let count = 1 + random.below(12) as usize;
        let nodes: Vec<LayoutNode> = (0..count)
            .map(|_| {
                let style = Style {
                    arrange: [
                        manguebyte_ui::layout::Arrange::Leaf,
                        manguebyte_ui::layout::Arrange::Row,
                        manguebyte_ui::layout::Arrange::Column,
                        manguebyte_ui::layout::Arrange::Stack,
                        manguebyte_ui::layout::Arrange::Scroll { x: true, y: false },
                        manguebyte_ui::layout::Arrange::Scroll { x: false, y: true },
                        manguebyte_ui::layout::Arrange::Scroll { x: true, y: true },
                    ][random.below(7) as usize],
                    padding: Insets::new(
                        random.number(10.0),
                        random.number(10.0),
                        random.number(10.0),
                        random.number(10.0),
                    ),
                    spacing: random.number(10.0),
                    flex: random.number(3.0),
                    min: Size::new(random.number(50.0), random.number(50.0)),
                    max: Size::new(random.number(100.0), random.number(100.0)),
                    ..Style::leaf()
                };
                // Children anywhere, out of range, repeated, cycles.
                let children = (0..random.below(5))
                    .map(|_| random.below(count as u64 + 3) as usize)
                    .collect();
                LayoutNode::new(
                    style,
                    Size::new(random.number(80.0), random.number(80.0)),
                    children,
                )
            })
            .collect();
        let area = Rect::new(
            random.number(10.0),
            random.number(10.0),
            random.number(500.0),
            random.number(500.0),
        );
        let scale = random.number(3.0);
        let root = random.below(count as u64 + 1) as usize;
        let placed = solve(&nodes, root, area, scale);
        assert_eq!(placed.len(), count, "case {case}");
        for placement in &placed {
            assert!(
                placement.rect.width >= 0.0 && placement.rect.height >= 0.0,
                "case {case}: {placement:?}"
            );
            assert!(
                !placement.rect.x.is_nan() && !placement.rect.y.is_nan(),
                "case {case}"
            );
        }
    }
}
