//! Zoom to fit, on the headless backend: an editor in a host and in an
//! application window, with input sent as the system sends it, in device
//! pixels, and what the window shows read back.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use manguebyte_gfx::snapshot::Snapshots;
use manguebyte_gfx::{Color, IRect, Pixmap, Point, Scene, Size};
use manguebyte_gui::{Editor, HeadlessEditor};
use manguebyte_params::{ChoiceParam, FloatParam, FloatRange, Param, param_set};
use manguebyte_plugin::editor::testing::{HostCall, RecordingEditorHost};
use manguebyte_plugin::editor::{
    EditorContext, EditorSpec, HostLink, HostSize, OpenEditor, ParentWindow, Resize,
};
use manguebyte_ui::access::{AccessNode, Role};
use manguebyte_ui::layout::Align;
use manguebyte_ui::{
    AccessCx, Event, EventCx, Handled, IntoView, MeasureCx, PaintCx, ParamHandle,
    RESPONSIVE_MARGIN, State, Theme, UpdateCx, View, Widget, column, combo, knob, label,
    responsive, row, tabs, text_field,
};
use manguebyte_window::{App, Button, Modifiers, PhysicalPoint, PhysicalSize, RawInput};

/// The size the test interfaces are designed at.
const DESIGN: HostSize = HostSize::new(400, 300);

const ANY_SIZE: Resize = Resize::Free {
    min: HostSize::new(40, 40),
    max: HostSize::new(4000, 4000),
};

thread_local! {
    static OPENED: RefCell<Option<HeadlessEditor>> = const { RefCell::new(None) };
}

/// An editor open in a recording host, with its controller.
struct Open {
    ui: HeadlessEditor,
    editor: Box<dyn OpenEditor>,
    host: Rc<RecordingEditorHost>,
    _link: HostLink,
}

impl Open {
    fn new(spec: Editor, scale: Option<f64>) -> Open {
        Open::with_host(spec, scale, Rc::new(RecordingEditorHost::new()))
    }

    fn with_host(spec: Editor, scale: Option<f64>, host: Rc<RecordingEditorHost>) -> Open {
        let spec =
            spec.on_headless_open(|editor| OPENED.with(|slot| *slot.borrow_mut() = Some(editor)));
        let (context, link) = EditorContext::attach(host.clone());
        let parent = ParentWindow::win32(0x10).expect("not null");
        let mut editor = spec.open(parent, scale, context).expect("headless windows");
        let ui = OPENED
            .with(|slot| slot.borrow_mut().take())
            .expect("the hook ran");
        editor.set_visible(true);
        ui.tick();
        Open {
            ui,
            editor,
            host,
            _link: link,
        }
    }

    /// The host makes the editor `width` by `height`; a frame runs at once.
    fn resize(&mut self, width: u32, height: u32) {
        assert!(self.editor.set_size(HostSize::new(width, height)));
    }

    /// The bounds of the widget of `role` and `name`.
    fn get(&self, role: Role, name: &str) -> manguebyte_ui::AccessEntry {
        self.ui
            .find(role, name)
            .unwrap_or_else(|| panic!("no {role:?} named {name:?}"))
    }

    fn scale(&self) -> f32 {
        self.ui.layout().expect("open").1
    }

    fn layout_size(&self) -> Size {
        self.ui.layout().expect("open").0
    }

    /// A press and release of the main button at `at`, in the window's
    /// device pixels, as the system sends them.
    fn click_device(&self, at: PhysicalPoint) {
        self.ui.window().input(RawInput::PointerDown {
            at,
            button: Button::Primary,
            modifiers: Modifiers::NONE,
            time_ms: 0,
        });
        self.ui.window().input(RawInput::PointerUp {
            at,
            button: Button::Primary,
            modifiers: Modifiers::NONE,
        });
    }

    /// The window shows the back buffer at its origin, and the theme's
    /// `background` everywhere else.
    fn assert_letterboxed(&self, background: Color, label: &str) {
        let shown = self.ui.shown().expect("open");
        let pixels = self.ui.pixels().expect("open");
        let (x, y) = self.ui.origin().expect("open");
        let buffer = IRect::new(x, y, pixels.width(), pixels.height());
        assert!(
            shown.crop(buffer).as_ref() == Some(&pixels),
            "{label}: the buffer is not where it should be"
        );
        let bar = background.premultiply();
        for py in 0..shown.height() {
            for px in 0..shown.width() {
                let inside = (px as i32) >= x
                    && (py as i32) >= y
                    && (px as i32) < x + pixels.width() as i32
                    && (py as i32) < y + pixels.height() as i32;
                if !inside {
                    assert_eq!(shown.pixel(px, py), Some(bar), "{label}: bar at {px}, {py}");
                }
            }
        }
    }
}

/// A square that counts what happens to it.
#[derive(Clone, Default)]
struct Pad {
    presses: Arc<AtomicU32>,
    paints: Arc<AtomicU32>,
    drops: Arc<AtomicU32>,
    hovered: Arc<AtomicBool>,
}

impl Pad {
    fn presses(&self) -> u32 {
        self.presses.load(Ordering::Relaxed)
    }

    fn paints(&self) -> u32 {
        self.paints.load(Ordering::Relaxed)
    }

    fn is_hovered(&self) -> bool {
        self.hovered.load(Ordering::Relaxed)
    }

    fn view(&self, name: &'static str) -> View {
        PadWidget {
            name,
            pad: self.clone(),
        }
        .flex(1.0)
    }
}

struct PadWidget {
    name: &'static str,
    pad: Pad,
}

impl Drop for PadWidget {
    fn drop(&mut self) {
        self.pad.drops.fetch_add(1, Ordering::Relaxed);
    }
}

impl Widget for PadWidget {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Button, self.name)
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        Size::new(20.0, 20.0)
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        self.pad.paints.fetch_add(1, Ordering::Relaxed);
        scene.fill(cx.rect(), Color::rgb(30, 160, 90));
    }

    fn hover_changed(&mut self, _cx: &mut EventCx<'_>, hovered: bool) {
        self.pad.hovered.store(hovered, Ordering::Relaxed);
    }

    fn event(&mut self, _cx: &mut EventCx<'_>, event: &Event) -> Handled {
        if matches!(event, Event::PointerDown { .. }) {
            self.pad.presses.fetch_add(1, Ordering::Relaxed);
            return Handled::Yes;
        }
        Handled::No
    }
}

struct Params {
    gain: FloatParam,
}
param_set!(Params { gain });

fn params() -> Arc<Params> {
    Arc::new(Params {
        gain: FloatParam::new("gain", "Gain", FloatRange::linear(0.0, 1.0), 0.5).with_precision(2),
    })
}

/// Two pads side by side above a knob and a text field, filling the design.
fn console(left: &Pad, right: &Pad, params: &Arc<Params>) -> Editor {
    let (left, right, params) = (left.clone(), right.clone(), Arc::clone(params));
    Editor::new(DESIGN, move || {
        column((
            row((left.view("Left"), right.view("Right"))).height(100.0),
            knob(ParamHandle::new(Arc::clone(&params), |p| &p.gain)).flex(1.0),
            text_field("Title", State::new(String::new())),
        ))
    })
    .with_resize(ANY_SIZE)
}

/// What each test looks through: a window at each zoom, with bars on
/// neither axis, on the right and left, and above and below.
fn windows(zoom: f32) -> [(u32, u32, &'static str); 3] {
    let width = (400.0 * zoom).round() as u32;
    let height = (300.0 * zoom).round() as u32;
    [
        (width, height, "no bars"),
        (width + 60, height, "bars at the sides"),
        (width, height + 90, "bars above and below"),
    ]
}

#[test]
fn clicks_and_focus_land_right_at_zooms_above_and_below_one() {
    for zoom in [0.7_f32, 1.3] {
        for (width, height, how) in windows(zoom) {
            let (left, right) = (Pad::default(), Pad::default());
            let params = params();
            let spec = console(&left, &right, &params).with_zoom_to_fit(0.25);
            let mut open = Open::new(spec, Some(1.0));
            open.resize(width, height);
            let context = format!("zoom {zoom}, {how}");
            assert!((open.scale() - zoom).abs() < 1e-3, "{context}");
            assert_eq!(open.layout_size(), Size::new(400.0, 300.0), "{context}");

            // The first and last device pixels of the pad, as assistive
            // technology is told where it is, and the first of the next.
            let pad = open.get(Role::Button, "Left").device;
            let (x0, y0) = (pad.x, pad.y);
            let (x1, y1) = (pad.right() as i32 - 1, pad.bottom() as i32 - 1);
            for (x, y, on_left) in [(x0, y0, true), (x1, y1, true), (x1 + 1, y0, false)] {
                let (before_left, before_right) = (left.presses(), right.presses());
                open.click_device(PhysicalPoint::new(x, y));
                assert_eq!(
                    left.presses() - before_left,
                    u32::from(on_left),
                    "{context}"
                );
                assert_eq!(
                    right.presses() - before_right,
                    u32::from(!on_left),
                    "{context} at {x}, {y}"
                );
            }

            // Pressed where it is, the knob takes the focus; and helpers
            // that speak in the design's pixels hit the same things.
            let knob = open.get(Role::Slider, "Gain");
            assert!(!knob.node.state.focused);
            let center = knob.device;
            open.click_device(PhysicalPoint::new(
                center.x + center.width as i32 / 2,
                center.y + center.height as i32 / 2,
            ));
            assert!(
                open.get(Role::Slider, "Gain").node.state.focused,
                "{context}"
            );
            let before = right.presses();
            let on_right = open.get(Role::Button, "Right").bounds.center();
            open.ui.press(on_right);
            open.ui.release(on_right);
            assert_eq!(right.presses(), before + 1, "{context}");
        }
    }
}

#[test]
fn a_drag_goes_by_the_zoom_as_a_drag_at_one_does() {
    // 40 device pixels at 2.0 is 20 logical pixels: a tenth of the theme's
    // drag range, as at scale 1.
    let params = params();
    let host = Rc::new(RecordingEditorHost::new().with_params(params.clone()));
    let spec = console(&Pad::default(), &Pad::default(), &params).with_zoom_to_fit(0.25);
    let mut open = Open::with_host(spec, Some(1.0), host);
    open.resize(800, 600);
    let at = open.get(Role::Slider, "Gain").bounds.center();
    open.ui.press(at);
    open.ui.move_to(Point::new(at.x, at.y - 20.0));
    open.ui.release(Point::new(at.x, at.y - 20.0));
    assert!(
        (params.gain.value() - 0.6).abs() < 1e-6,
        "{}",
        params.gain.value()
    );
}

#[test]
fn the_text_caret_is_where_the_zoomed_text_is() {
    let (left, right) = (Pad::default(), Pad::default());
    let spec = console(&left, &right, &params()).with_zoom_to_fit(0.25);
    let mut open = Open::new(spec, Some(1.0));
    let field = open.get(Role::TextInput, "Title").bounds.center();
    open.ui.press(field);
    open.ui.release(field);
    open.ui.type_text("Fugue");
    open.ui.tick();
    let at_one = open.ui.window().system_caret().expect("typing");

    for (width, height) in [(280, 210), (520, 390), (800, 300), (400, 600)] {
        open.resize(width, height);
        open.ui.tick();
        let caret = open.ui.window().system_caret().expect("typing");
        let box_on_screen = open.get(Role::TextInput, "Title").device;
        assert!(
            box_on_screen.contains_rect(&caret),
            "{caret:?} not in {box_on_screen:?} at {width} by {height}"
        );
        let expected = at_one.height as f32 * open.scale();
        assert!(
            (caret.height as f32 - expected).abs() <= 2.0,
            "{caret:?} at {}",
            open.scale()
        );
    }
}

#[test]
fn the_letterbox_centres_the_layout_on_each_axis_in_the_themes_colour() {
    let (left, right) = (Pad::default(), Pad::default());
    for theme in [Theme::dark(), Theme::light()] {
        let spec = console(&left, &right, &params())
            .with_theme(theme)
            .with_zoom_to_fit(0.25);
        let mut open = Open::new(spec, Some(1.0));
        let background = theme.palette.background;
        // Each axis, a window that does not divide evenly, and one larger
        // than the design.
        for (width, height, origin, buffer) in [
            (800, 300, (200, 0), (400, 300)),
            (400, 600, (0, 150), (400, 300)),
            (401, 300, (0, 0), (400, 300)),
            (801, 300, (200, 0), (400, 300)),
            (800, 400, (133, 0), (533, 400)),
        ] {
            open.resize(width, height);
            let label = format!("{width} by {height}");
            assert_eq!(open.ui.origin(), Some(origin), "{label}");
            let pixels = open.ui.pixels().expect("open");
            assert_eq!((pixels.width(), pixels.height()), buffer, "{label}");
            open.assert_letterboxed(background, &label);
        }
    }
}

#[test]
fn the_bars_take_no_clicks_and_keep_the_focus_where_it_is() {
    let (left, right) = (Pad::default(), Pad::default());
    let spec = console(&left, &right, &params()).with_zoom_to_fit(0.25);
    let mut open = Open::new(spec, Some(1.0));
    let field = open.get(Role::TextInput, "Title").bounds.center();
    open.ui.press(field);
    open.ui.release(field);
    assert!(open.get(Role::TextInput, "Title").node.state.focused);

    // Pads run across the whole design; each bar is just past their ends.
    open.resize(800, 300);
    for x in [0, 199, 600, 799] {
        open.click_device(PhysicalPoint::new(x, 50));
    }
    assert_eq!((left.presses(), right.presses()), (0, 0));
    assert!(open.get(Role::TextInput, "Title").node.state.focused);
    // And the first pixel past each is the pad's.
    open.click_device(PhysicalPoint::new(200, 50));
    open.click_device(PhysicalPoint::new(599, 50));
    assert_eq!((left.presses(), right.presses()), (1, 1));

    open.resize(400, 600);
    let (before_left, before_right) = (left.presses(), right.presses());
    for y in [0, 149, 450, 599] {
        open.click_device(PhysicalPoint::new(100, y));
        open.click_device(PhysicalPoint::new(300, y));
    }
    assert_eq!(
        (left.presses(), right.presses()),
        (before_left, before_right)
    );
    open.click_device(PhysicalPoint::new(100, 150));
    assert_eq!(left.presses(), before_left + 1);
}

#[test]
fn a_press_on_the_bars_closes_a_menu_as_any_press_outside_it_does() {
    struct Shapes {
        shape: ChoiceParam,
    }
    param_set!(Shapes { shape });
    let params = Arc::new(Shapes {
        shape: ChoiceParam::new("shape", "Shape", &["Sine", "Saw"], 0),
    });
    let handle = Arc::clone(&params);
    let spec = Editor::new(DESIGN, move || {
        combo(ParamHandle::new(Arc::clone(&handle), |p| &p.shape))
            .into_view()
            .align_self(Align::Start)
    })
    .with_resize(ANY_SIZE)
    .with_zoom_to_fit(0.25);
    let mut open = Open::new(spec, Some(1.0));
    open.resize(800, 300);
    let device = open.get(Role::ComboBox, "Shape").device;
    let on_combo = PhysicalPoint::new(device.x + 5, device.y + 5);
    open.click_device(on_combo);
    open.ui.tick();
    assert!(
        open.ui.find(Role::MenuItem, "Saw").is_some(),
        "the menu opened"
    );
    open.click_device(PhysicalPoint::new(5, 5));
    open.ui.tick();
    assert!(
        open.ui.find(Role::MenuItem, "Saw").is_none(),
        "the menu closed"
    );
}

#[test]
fn a_drag_that_goes_over_the_bars_goes_on() {
    let params = params();
    let host = Rc::new(RecordingEditorHost::new().with_params(params.clone()));
    let spec = console(&Pad::default(), &Pad::default(), &params).with_zoom_to_fit(0.25);
    let mut open = Open::with_host(spec, Some(1.0), host);
    open.resize(400, 1000);
    assert_eq!(open.ui.origin(), Some((0, 350)));
    let knob = open.get(Role::Slider, "Gain").device;
    let x = knob.x + knob.width as i32 / 2;
    let y = knob.y + knob.height as i32 / 2;
    open.ui.window().input(RawInput::PointerDown {
        at: PhysicalPoint::new(x, y),
        button: Button::Primary,
        modifiers: Modifiers::NONE,
        time_ms: 0,
    });
    // Up into the bar above: as far as it asks.
    let top = 100;
    open.ui.window().input(RawInput::PointerMove {
        at: PhysicalPoint::new(x, top),
        modifiers: Modifiers::NONE,
    });
    open.ui.window().input(RawInput::PointerUp {
        at: PhysicalPoint::new(x, top),
        button: Button::Primary,
        modifiers: Modifiers::NONE,
    });
    assert_eq!(params.gain.value(), 1.0, "a drag of {} pixels", y - top);
    let id = params.gain.param_id();
    let calls = open.host.take_calls();
    assert_eq!(calls.last(), Some(&HostCall::EndGesture(id)), "{calls:?}");
}

#[test]
fn the_access_trees_bounds_are_in_the_zoomed_window() {
    let params = params();
    let view_params = Arc::clone(&params);
    let spec = Editor::new(DESIGN, move || {
        tabs("Sections", State::new(0_usize))
            .page(
                "Stops",
                knob(ParamHandle::new(Arc::clone(&view_params), |p| &p.gain)),
            )
            .page("Tuning", label("Hidden page"))
    })
    .with_resize(ANY_SIZE)
    .with_zoom_to_fit(0.25);
    let mut open = Open::new(spec, Some(1.0));
    let design = open.ui.access_tree();

    for (width, height) in [(280, 210), (520, 390), (800, 300), (400, 600), (333, 601)] {
        open.resize(width, height);
        let scale = open.scale();
        let (x, y) = open.ui.origin().expect("open");
        let tree = open.ui.access_tree();
        assert_eq!(tree.len(), design.len());
        let shown = IRect::new(0, 0, width, height);
        let mut items = 0;
        for (entry, at_one) in tree.iter().zip(&design) {
            assert_eq!(entry.node.name, at_one.node.name);
            items += usize::from(entry.item.is_some());
            // Logical pixels are the design's: at most a device pixel off,
            // where the edges snap at this scale.
            let slack = 1.0 / scale + 0.01;
            assert!(
                (entry.bounds.x - at_one.bounds.x).abs() <= slack,
                "{entry:?}"
            );
            assert!((entry.bounds.right() - at_one.bounds.right()).abs() <= slack);
            // Device pixels are the window's.
            let drawn = entry.bounds.to_device(scale);
            assert!((entry.device.x - (drawn.x + x)).abs() <= 1, "{entry:?}");
            assert!((entry.device.y - (drawn.y + y)).abs() <= 1, "{entry:?}");
            assert!(entry.device.width.abs_diff(drawn.width) <= 1, "{entry:?}");
            if !entry.device.is_empty() {
                assert!(shown.contains_rect(&entry.device), "{entry:?}");
            }
        }
        assert!(items >= 2, "the tabs are items, with their own bounds");
    }
}

#[test]
fn a_zoom_of_three_quarters_is_the_same_layout_drawn_at_three_quarters() {
    let (left, right) = (Pad::default(), Pad::default());
    let params = params();

    // Opened at the design size, then made smaller.
    let spec = console(&left, &right, &params).with_zoom_to_fit(0.25);
    let mut zoomed = Open::new(spec, Some(1.0));
    zoomed.resize(300, 225);
    zoomed.ui.tick();
    assert_eq!(zoomed.scale(), 0.75);

    // The same editor with no zoom, at the host's scale of 0.75.
    let plain = Open::new(console(&left, &right, &params), Some(0.75));
    plain.ui.tick();
    assert_eq!(plain.scale(), 0.75);
    assert_eq!(plain.layout_size(), Size::new(400.0, 300.0));

    let shown = zoomed.ui.shown().expect("open");
    assert!(shown == plain.ui.shown().expect("open"));
    assert_eq!(zoomed.ui.dump(), plain.ui.dump());
    let dump = zoomed.ui.dump().expect("open");
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let failures = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("gui-golden-failures");
    let snapshots = Snapshots::new(dir, failures).with_tolerance(1);
    if let Err(mismatch) = snapshots.check("zoom@0.75", &shown, &dump) {
        panic!("{mismatch}");
    }
}

#[test]
fn a_window_of_the_design_size_is_as_if_there_were_no_zoom() {
    let (left, right) = (Pad::default(), Pad::default());
    let params = params();
    for scale in [1.0, 1.25, 2.0] {
        let zoomed = Open::new(
            console(&left, &right, &params).with_zoom_to_fit(0.25),
            Some(scale),
        );
        let plain = Open::new(console(&left, &right, &params), Some(scale));
        assert_eq!(zoomed.scale(), scale as f32);
        assert_eq!(zoomed.ui.origin(), Some((0, 0)));
        assert!(zoomed.ui.shown() == plain.ui.shown(), "at {scale}");
        assert_eq!(zoomed.ui.dump(), plain.ui.dump());
    }
}

#[test]
fn the_zoom_counts_on_top_of_the_scale_it_is_given() {
    let (left, right) = (Pad::default(), Pad::default());
    let spec = console(&left, &right, &params()).with_zoom_to_fit(0.25);
    // At 1.5 the design is 600 by 450 device pixels.
    let mut open = Open::new(spec, Some(1.5));
    assert_eq!(open.scale(), 1.5);
    open.resize(300, 225);
    assert_eq!(open.scale(), 0.75, "half the logical size");
    assert_eq!(open.layout_size(), Size::new(400.0, 300.0));
    // A new scale from the host, as a monitor's: the window, as the host
    // grants it, decides.
    assert!(open.editor.set_scale(2.0));
    open.ui.tick();
    let window = open.ui.size().expect("open");
    let zoom = (f64::from(window.width) / 2.0 / 400.0).min(f64::from(window.height) / 2.0 / 300.0);
    assert!(
        (f64::from(open.scale()) - 2.0 * zoom).abs() < 1e-3,
        "{} in {window:?}",
        open.scale()
    );
    open.click_device(PhysicalPoint::new(
        open.get(Role::Button, "Left").device.x + 1,
        open.get(Role::Button, "Left").device.y + 1,
    ));
    assert_eq!(left.presses(), 1);
}

#[test]
fn below_the_smallest_zoom_it_stops_and_reflows() {
    let (left, right) = (Pad::default(), Pad::default());
    let spec = console(&left, &right, &params()).with_zoom_to_fit(0.75);
    let mut open = Open::new(spec, Some(1.0));
    // At the smallest zoom, the design still.
    open.resize(300, 225);
    assert_eq!(open.scale(), 0.75);
    assert_eq!(open.layout_size(), Size::new(400.0, 300.0));

    // Narrower: the same zoom, and a layout in the room at that zoom.
    open.resize(150, 225);
    assert_eq!(open.scale(), 0.75);
    assert_eq!(open.layout_size(), Size::new(200.0, 300.0));
    assert_eq!(
        open.get(Role::Button, "Left").bounds.width,
        100.0,
        "half of 200"
    );
    // It fills the window: no bars, and clicks land.
    assert_eq!(open.ui.origin(), Some((0, 0)));
    assert!(open.ui.shown().as_ref() == open.ui.pixels().as_ref());
    open.click_device(PhysicalPoint::new(100, 10));
    assert_eq!((left.presses(), right.presses()), (0, 1));

    // Back, and larger: the design again, zoomed.
    open.resize(800, 600);
    assert_eq!(open.scale(), 2.0);
    assert_eq!(open.layout_size(), Size::new(400.0, 300.0));
}

#[test]
fn a_resize_changes_the_zoom_with_no_reflow() {
    for rules in [
        ANY_SIZE,
        Resize::Ratio {
            min: HostSize::new(40, 30),
            max: HostSize::new(4000, 3000),
            width: 4,
            height: 3,
        },
    ] {
        let (left, right) = (Pad::default(), Pad::default());
        let spec = console(&left, &right, &params())
            .with_resize(rules)
            .with_zoom_to_fit(0.25);
        let mut open = Open::new(spec, Some(1.0));
        let design = open.ui.access_tree();
        for (width, height) in [(800, 600), (600, 450), (300, 225), (1000, 750)] {
            open.resize(width, height);
            assert_eq!(open.layout_size(), Size::new(400.0, 300.0), "{rules:?}");
            assert!((open.scale() - width as f32 / 400.0).abs() < 1e-5);
            // What is laid out is where it was: within a device pixel, where
            // its edges now snap, and a logical pixel more, as text that
            // measures itself in device pixels rounds its height.
            for (entry, at_one) in open.ui.access_tree().iter().zip(&design) {
                let slack = 1.0 / open.scale() + 1.0;
                let (now, was) = (entry.bounds, at_one.bounds);
                for (a, b) in [
                    (now.x, was.x),
                    (now.y, was.y),
                    (now.right(), was.right()),
                    (now.bottom(), was.bottom()),
                ] {
                    assert!(
                        (a - b).abs() <= slack,
                        "{entry:?} was {at_one:?} at {width}"
                    );
                }
            }
        }
        // Nothing was built again.
        assert_eq!(left.drops.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn nothing_is_drawn_again_on_a_quiet_frame() {
    let (left, right) = (Pad::default(), Pad::default());
    let params = params();
    let spec = console(&left, &right, &params).with_zoom_to_fit(0.25);
    let mut open = Open::new(spec, Some(1.0));
    open.resize(800, 300);
    open.ui.tick();
    open.ui.window().take_blits();
    let (frames, presented) = open.ui.frames();
    let work = open.ui.work();
    let paints = left.paints();
    for _ in 0..10 {
        assert_eq!(open.ui.tick(), 1);
    }
    assert_eq!(open.ui.frames(), (frames + 10, presented));
    assert_eq!(open.ui.work(), work);
    assert_eq!(left.paints(), paints);
    assert_eq!(open.ui.window().take_blits(), []);

    // A change in the interface shows only what changed, where it now is.
    params.gain.set(0.25);
    open.ui.tick();
    let blits = open.ui.window().take_blits();
    assert!(!blits.is_empty());
    let knob = open.get(Role::Slider, "Gain").device;
    for blit in &blits {
        assert!(knob.contains_rect(blit), "{blit:?} outside {knob:?}");
    }
    open.assert_letterboxed(Theme::default().palette.background, "after a change");
}

#[test]
fn room_to_spare_changing_redraws_the_bars_and_nothing_else() {
    let (left, right) = (Pad::default(), Pad::default());
    let spec = console(&left, &right, &params()).with_zoom_to_fit(0.25);
    let mut open = Open::new(spec, Some(1.0));
    open.resize(800, 300);
    open.ui.tick();
    let background = Theme::default().palette.background;
    let work = open.ui.work();
    let paints = left.paints();
    // Wider and narrower, the height the limit all along.
    for width in [1000, 450, 700] {
        open.resize(width, 300);
        assert_eq!(open.scale(), 1.0);
        assert_eq!(open.ui.work(), work, "the interface did nothing");
        assert_eq!(left.paints(), paints);
        open.assert_letterboxed(background, &format!("{width} wide"));
    }
}

#[test]
fn an_expose_shows_the_bars_and_the_buffer_where_they_go() {
    let (left, right) = (Pad::default(), Pad::default());
    let spec = console(&left, &right, &params()).with_zoom_to_fit(0.25);
    let mut open = Open::new(spec, Some(1.0));
    open.resize(800, 300);
    open.ui.tick();
    // What the window asks to be drawn: the strip it grew by.
    open.ui.window().resize(PhysicalSize::new(900, 300));
    assert!(!open.ui.window().pending_damage().is_empty());
    open.ui.window().take_blits();
    assert!(open.ui.window().expose());
    let background = Theme::default().palette.background;
    open.assert_letterboxed(background, "after an expose");
    assert!(!open.ui.window().take_blits().is_empty());
}

#[test]
fn the_bars_follow_a_theme_that_changes() {
    struct Look(Arc<AtomicBool>);
    impl Widget for Look {
        fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
            AccessNode::new(Role::Group, "Look")
        }
        fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
            Size::new(10.0, 10.0)
        }
        fn paint(&mut self, _cx: &PaintCx<'_>, _scene: &mut Scene) {}
        fn update(&mut self, cx: &mut UpdateCx<'_>) {
            if self.0.load(Ordering::Relaxed) {
                cx.set_theme(Theme::light());
            }
        }
    }
    let light = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&light);
    let spec = Editor::new(DESIGN, move || Look(Arc::clone(&flag)))
        .with_resize(ANY_SIZE)
        .with_zoom_to_fit(0.25);
    let mut open = Open::new(spec, Some(1.0));
    open.resize(800, 300);
    open.assert_letterboxed(Theme::dark().palette.background, "dark");
    light.store(true, Ordering::Relaxed);
    open.ui.tick();
    open.assert_letterboxed(Theme::light().palette.background, "light");
}

#[test]
fn past_the_largest_scale_the_bars_grow_on_both_axes() {
    let spec = Editor::new(HostSize::new(20, 10), move || label("Small"))
        .with_resize(ANY_SIZE)
        .with_zoom_to_fit(0.25);
    let mut open = Open::new(spec, Some(1.0));
    open.resize(400, 400);
    assert_eq!(open.scale(), 8.0);
    assert_eq!(open.ui.origin(), Some((120, 160)));
    open.assert_letterboxed(Theme::default().palette.background, "both axes");
}

/// The Caecilia's case: the full console zoomed, a compact one at its real
/// size below a breakpoint, on a screen at 125 %.
#[test]
fn the_full_layout_zooms_and_the_compact_layout_has_its_real_size() {
    let breakpoint = Size::new(1100.0, 700.0);
    let (wide, compact) = (Pad::default(), Pad::default());
    let (wide_pad, compact_pad) = (wide.clone(), compact.clone());
    let spec = Editor::new(HostSize::new(1600, 980), move || {
        responsive((
            column((
                wide_pad.view("Keyboards"),
                text_field("Title", State::new(String::new())),
            )),
            compact_pad.view("Summary"),
        ))
        .below(breakpoint)
    })
    .with_resize(Resize::Free {
        min: HostSize::new(300, 200),
        max: HostSize::new(4000, 3000),
    })
    .with_zoom_to_fit(0.5)
    .with_real_size_below(breakpoint);
    let design = Size::new(1600.0, 980.0);
    let system = 1.25_f64;
    let mut open = Open::new(spec, Some(system));
    assert_eq!(open.layout_size(), design);
    assert!(open.ui.find(Role::Button, "Keyboards").is_some());
    let widgets = open.ui.access_tree().len();
    let field = open.get(Role::TextInput, "Title").bounds.center();
    open.ui.press(field);
    open.ui.release(field);
    open.ui.type_text("Fugue");
    let device = |logical: f64| (logical * system).round() as u32;

    // A 1080p screen at 125 %: 1536 by 840 logical. Zoomed, not cut off.
    open.resize(device(1536.0), device(840.0));
    assert_eq!(open.layout_size(), design);
    let expected = 840.0 / 980.0 * system as f32;
    assert!((open.scale() - expected).abs() < 1e-3, "{}", open.scale());
    assert_eq!(
        compact.paints(),
        0,
        "never drawn in a room it has no business in"
    );
    let keyboards = open.get(Role::Button, "Keyboards").device;
    let (x, _) = open.ui.origin().expect("open");
    assert_eq!(keyboards.x, x);
    assert!(x > 0, "bars at the sides of a window wider than the design");

    // Half the screen, beside a score: 768 by 830 logical. One frame: the
    // resize runs it, and the wide child is not drawn in it.
    let wide_paints = wide.paints();
    open.resize(device(768.0), device(830.0));
    assert_eq!(wide.paints(), wide_paints, "the wrong child was drawn");
    assert_eq!(open.scale(), system as f32, "at its real size");
    let room = open.layout_size();
    assert!(
        (room.width - 768.0).abs() < 0.5 && (room.height - 830.0).abs() < 0.5,
        "{room:?}"
    );
    assert!(open.ui.find(Role::Button, "Keyboards").is_none());
    let summary = open.get(Role::Button, "Summary");
    assert_eq!(open.ui.origin(), Some((0, 0)));
    assert!(open.ui.shown().as_ref() == open.ui.pixels().as_ref());
    // Assistive technology and clicks go with it.
    let inside = PhysicalPoint::new(summary.device.x + 5, summary.device.y + 5);
    open.click_device(inside);
    assert_eq!(compact.presses(), 1);

    // Across the boundary each way, with the margin, as one.
    let margin = f64::from(RESPONSIVE_MARGIN);
    let at = |open: &mut Open, width: f64, height: f64, zoomed: bool, how: &str| {
        open.resize(device(width), device(height));
        assert_eq!(
            open.layout_size() == design,
            zoomed,
            "{how}: {:?}",
            open.layout_size()
        );
        assert_eq!(
            open.ui.find(Role::Button, "Keyboards").is_some(),
            zoomed,
            "{how}"
        );
        assert_eq!(
            open.ui.find(Role::Button, "Summary").is_some(),
            !zoomed,
            "{how}"
        );
    };
    at(&mut open, 1099.0, 800.0, false, "up, just short");
    at(&mut open, 1100.0, 800.0, true, "up, at the breakpoint");
    at(
        &mut open,
        1100.0 - margin + 1.0,
        800.0,
        true,
        "down, inside the margin",
    );
    at(
        &mut open,
        1100.0 - margin - 1.0,
        800.0,
        false,
        "down, past the margin",
    );
    at(&mut open, 1100.0, 800.0, true, "up again");
    at(
        &mut open,
        1400.0,
        700.0 - margin - 1.0,
        false,
        "down, by the height",
    );
    at(&mut open, 1400.0, 700.0, true, "up, by the height");

    // Nothing was built again and what was typed is still there.
    assert_eq!(open.ui.access_tree().len(), widgets);
    assert_eq!(wide.drops.load(Ordering::Relaxed), 0);
    assert_eq!(compact.drops.load(Ordering::Relaxed), 0);
    let title = open.get(Role::TextInput, "Title");
    assert_eq!(title.node.value.as_deref(), Some("Fugue"));
}

/// The other way: no breakpoint on the window, and a compact layout at the
/// smallest zoom.
#[test]
fn below_the_smallest_zoom_a_responsive_gets_the_room_at_that_zoom() {
    let spec = Editor::new(DESIGN, || {
        responsive((label("Full"), label("Compact"))).below(Size::new(400.0, 300.0))
    })
    .with_resize(ANY_SIZE)
    .with_zoom_to_fit(0.5);
    let mut open = Open::new(spec, Some(1.0));
    // Exactly the design's room, however it is zoomed, at scales where
    // snapping leaves it a fraction short.
    for (width, height) in [
        (280, 210),
        (311, 233),
        (520, 390),
        (800, 600),
        (801, 601),
        (200, 150),
    ] {
        open.resize(width, height);
        assert!(
            open.ui.find(Role::Label, "Full").is_some(),
            "{width} by {height}"
        );
    }
    // Below a zoom of one half: a room of 2 logical pixels per device pixel.
    open.resize(100, 150);
    assert_eq!(open.scale(), 0.5);
    assert_eq!(open.layout_size(), Size::new(200.0, 300.0));
    assert!(open.ui.find(Role::Label, "Compact").is_some());
    assert!(open.ui.find(Role::Label, "Full").is_none());
    open.resize(800, 600);
    assert!(open.ui.find(Role::Label, "Full").is_some());
}

#[test]
fn an_application_window_zooms_as_a_hosts_does() {
    let (app, _test) = App::headless();
    let (left, right) = (Pad::default(), Pad::default());
    let params = params();
    let spec = console(&left, &right, &params).with_zoom_to_fit(0.25);
    let window = spec
        .open_in_app(&app, "Console", Some(params.clone() as Arc<_>))
        .expect("headless windows open");
    let ui = window.headless().expect("a headless application");
    assert_eq!(ui.tick(), 1);
    assert_eq!(ui.layout(), Some((Size::new(400.0, 300.0), 1.0)));

    // The person drags the frame: wider than the design's shape.
    assert_eq!(
        ui.window().drag_frame_to(PhysicalSize::new(800, 225)),
        Some(PhysicalSize::new(800, 225))
    );
    ui.tick();
    assert_eq!(ui.layout(), Some((Size::new(400.0, 300.0), 0.75)));
    let (x, y) = ui.origin().expect("open");
    assert_eq!((x, y), (250, 0));

    // A click on the bar does nothing, one past it lands, and gestures set
    // the parameters as a standalone's do.
    let press = |at: PhysicalPoint| {
        ui.window().input(RawInput::PointerDown {
            at,
            button: Button::Primary,
            modifiers: Modifiers::NONE,
            time_ms: 0,
        });
        ui.window().input(RawInput::PointerUp {
            at,
            button: Button::Primary,
            modifiers: Modifiers::NONE,
        });
    };
    press(PhysicalPoint::new(249, 10));
    assert_eq!(left.presses(), 0);
    press(PhysicalPoint::new(250, 10));
    assert_eq!(left.presses(), 1);
    let knob = ui
        .find(Role::Slider, "Gain")
        .expect("shown")
        .bounds
        .center();
    ui.press(knob);
    ui.move_to(Point::new(knob.x, knob.y - 20.0));
    ui.release(Point::new(knob.x, knob.y - 20.0));
    assert!(
        (params.gain.value() - 0.6).abs() < 1e-6,
        "{}",
        params.gain.value()
    );

    // A monitor of another scale: the same logical window, the same zoom.
    assert!(ui.window().move_to_monitor(2.0));
    ui.tick();
    let (size, scale) = ui.layout().expect("open");
    assert_eq!(size, Size::new(400.0, 300.0));
    assert!((scale - 1.5).abs() < 1e-3, "{scale}");

    let shown = ui.shown().expect("open");
    let pixels: Pixmap = ui.pixels().expect("open");
    let (x, y) = ui.origin().expect("open");
    let buffer = IRect::new(x, y, pixels.width(), pixels.height());
    assert!(shown.crop(buffer).as_ref() == Some(&pixels));
}

/// The full console and a compact one, as the Caecilia has them, with what
/// is typed in the full one and which was drawn counted.
fn full_or_compact(wide: &Pad, compact: &Pad, breakpoint: Size) -> Editor {
    let (wide, compact) = (wide.clone(), compact.clone());
    Editor::new(HostSize::new(1600, 980), move || {
        responsive((
            column((
                wide.view("Keyboards"),
                text_field("Title", State::new(String::new())),
            )),
            compact.view("Summary"),
        ))
        .below(breakpoint)
    })
    .with_resize(Resize::Free {
        min: HostSize::new(300, 200),
        max: HostSize::new(4000, 3000),
    })
}

#[test]
fn moving_an_application_window_between_monitors_keeps_its_layout_and_its_state() {
    let breakpoint = Size::new(1100.0, 700.0);
    for zoomed in [true, false] {
        let (wide, compact) = (Pad::default(), Pad::default());
        let spec = full_or_compact(&wide, &compact, breakpoint);
        let spec = if zoomed {
            spec.with_zoom_to_fit(0.5).with_real_size_below(breakpoint)
        } else {
            spec
        };
        let (app, _test) = App::headless();
        let window = spec.open_in_app(&app, "Console", None).expect("headless");
        let ui = window.headless().expect("headless");
        ui.tick();
        ui.window().drag_frame_to(PhysicalSize::new(1536, 840));
        ui.tick();
        // Typing, with the focus in the full console.
        let field = ui
            .find(Role::TextInput, "Title")
            .expect("shown")
            .bounds
            .center();
        ui.press(field);
        ui.release(field);
        ui.type_text("Fugue");
        let focused = |ui: &HeadlessEditor| {
            ui.find(Role::TextInput, "Title")
                .is_some_and(|entry| entry.node.state.focused)
        };
        assert!(focused(&ui));

        // To twice the scale and back: the same logical window throughout.
        for scale in [2.0, 1.0, 1.5, 1.0] {
            assert!(ui.window().move_to_monitor(scale));
            assert!(focused(&ui), "the focus went at {scale}, zoomed {zoomed}");
            assert_eq!(
                compact.paints(),
                0,
                "the compact layout was shown at {scale}, zoomed {zoomed}"
            );
            ui.tick();
            assert!(focused(&ui));
        }
        assert_eq!(compact.paints(), 0);
        drop(window);
    }
}

#[test]
fn moving_a_compact_application_window_between_monitors_keeps_it_compact() {
    let breakpoint = Size::new(1100.0, 700.0);
    let (wide, compact) = (Pad::default(), Pad::default());
    let spec = full_or_compact(&wide, &compact, breakpoint)
        .with_zoom_to_fit(0.5)
        .with_real_size_below(breakpoint);
    let (app, _test) = App::headless();
    let window = spec.open_in_app(&app, "Console", None).expect("headless");
    let ui = window.headless().expect("headless");
    ui.tick();
    assert!(ui.window().move_to_monitor(2.0));
    // 768 by 830 logical, at 2.0.
    ui.window().drag_frame_to(PhysicalSize::new(1536, 1660));
    ui.tick();
    assert!(ui.find(Role::Button, "Summary").is_some());
    let painted = wide.paints();
    // At 1.0 the old size would read 1536 by 1660: the full console.
    assert!(ui.window().move_to_monitor(1.0));
    assert_eq!(wide.paints(), painted, "the full layout was shown");
    assert!(ui.find(Role::Button, "Summary").is_some());
    assert_eq!(ui.layout(), Some((Size::new(768.0, 830.0), 1.0)));
    drop(window);
}

#[test]
fn a_scale_that_brings_no_new_size_is_followed_by_the_next_frame() {
    let (left, right) = (Pad::default(), Pad::default());
    let (app, _test) = App::headless();
    let spec = console(&left, &right, &params()).with_zoom_to_fit(0.25);
    let window = spec.open_in_app(&app, "Console", None).expect("headless");
    let ui = window.headless().expect("headless");
    ui.tick();
    assert_eq!(ui.layout(), Some((Size::new(400.0, 300.0), 1.0)));
    // Not a move to a monitor: the scale alone.
    assert!(ui.window().set_system_scale(2.0));
    ui.tick();
    let (_, scale) = ui.layout().expect("open");
    assert_eq!(scale, 1.0, "half the logical size at twice the scale");
    drop(window);
}

#[test]
fn what_is_hovered_follows_a_zoom_under_a_pointer_that_stays() {
    let (left, right) = (Pad::default(), Pad::default());
    let spec = console(&left, &right, &params()).with_zoom_to_fit(0.25);
    let mut open = Open::new(spec, Some(1.0));
    // At zoom 1, 150 is over the left pad.
    open.ui.window().input(RawInput::PointerMove {
        at: PhysicalPoint::new(150, 20),
        modifiers: Modifiers::NONE,
    });
    assert!(left.is_hovered());
    // At zoom 0.5, the same pixel is 300 logical pixels in: the right pad.
    open.resize(200, 150);
    assert!(right.is_hovered(), "the old zoom's position");
    assert!(!left.is_hovered());
    // Only the bars moving: 500 wide at 150 tall has the layout from 150, so
    // the pixel is the left pad's first.
    open.resize(500, 150);
    assert_eq!(open.ui.origin(), Some((150, 0)));
    assert!(left.is_hovered());
    assert!(!right.is_hovered());
    // And further: it is on a bar, over nothing.
    open.resize(800, 150);
    assert!(!left.is_hovered() && !right.is_hovered());
}

#[test]
fn a_window_with_no_area_leaves_the_zoom_where_it_was() {
    let breakpoint = Size::new(1100.0, 700.0);
    let (wide, compact) = (Pad::default(), Pad::default());
    let spec = full_or_compact(&wide, &compact, breakpoint)
        .with_zoom_to_fit(0.5)
        .with_real_size_below(breakpoint);
    let mut open = Open::new(spec, Some(1.0));
    // Inside the margin, on the way down: still zoomed.
    open.resize(1090, 800);
    assert_eq!(open.layout_size(), Size::new(1600.0, 980.0));
    // Minimised, as a host may do it, and back.
    open.ui.window().resize(PhysicalSize::new(0, 0));
    open.ui.tick();
    open.ui.window().resize(PhysicalSize::new(1090, 800));
    open.ui.tick();
    assert_eq!(
        open.layout_size(),
        Size::new(1600.0, 980.0),
        "the full console at its real size, in less room than it was designed for"
    );
    assert!(open.ui.find(Role::Button, "Keyboards").is_some());
    assert_eq!(compact.paints(), 0);
}

#[test]
fn a_zoom_larger_than_the_buffer_can_hold_leaves_nothing_undrawn() {
    let (left, right) = (Pad::default(), Pad::default());
    let (view_left, view_right) = (left.clone(), right.clone());
    let spec = Editor::new(HostSize::new(4000, 100), move || {
        row((view_left.view("Left"), view_right.view("Right")))
    })
    .with_resize(Resize::Free {
        min: HostSize::new(40, 40),
        max: HostSize::new(16000, 400),
    })
    .with_zoom_to_fit(0.25);
    let open = Open::new(spec, Some(1.0));
    open.ui.window().resize(PhysicalSize::new(16000, 400));
    open.ui.tick();
    assert_eq!(open.layout_size(), Size::new(4000.0, 100.0));
    let pixels = open.ui.pixels().expect("open");
    let scale = open.scale();
    assert_eq!(
        (f64::from(4000.0 * scale)).round() as u32,
        pixels.width(),
        "cut at the largest buffer: the layout is not all there at {scale}"
    );
    assert!(pixels.width() <= manguebyte_ui::MAX_BUFFER_SIDE);
    // The far end of the layout is drawn, and takes a press.
    let right_pad = open.get(Role::Button, "Right").device;
    let end = PhysicalPoint::new(right_pad.right() as i32 - 2, right_pad.y + 2);
    open.click_device(end);
    assert_eq!(right.presses(), 1, "the far end counted as a bar");
}

#[test]
fn a_design_sized_window_is_not_zoomed_whatever_the_scale_rounds_to() {
    // At 1.3, 975 is 1267.5: the window rounds that up in double precision,
    // and the scale in single precision comes out just under.
    let (left, right) = (Pad::default(), Pad::default());
    let build = |zoomed: bool| {
        let (left, right) = (left.clone(), right.clone());
        let spec = Editor::new(HostSize::new(1605, 975), move || {
            row((left.view("Left"), right.view("Right")))
        });
        if zoomed {
            spec.with_zoom_to_fit(0.25)
        } else {
            spec
        }
    };
    for scale in [1.1, 1.3, 1.7, 2.3] {
        let zoomed = Open::new(build(true), Some(scale));
        let plain = Open::new(build(false), Some(scale));
        assert_eq!(zoomed.scale(), scale as f32, "soft at {scale}");
        assert_eq!(zoomed.ui.origin(), Some((0, 0)));
        assert_eq!(zoomed.ui.layout(), plain.ui.layout());
        assert!(zoomed.ui.shown() == plain.ui.shown(), "at {scale}");
    }
}

#[test]
fn an_editor_with_no_zoom_is_as_it_was() {
    // The rule and the real size mean nothing without `with_zoom_to_fit`.
    let (left, right) = (Pad::default(), Pad::default());
    let spec = console(&left, &right, &params()).with_real_size_below(Size::new(1000.0, 1000.0));
    let mut open = Open::new(spec, Some(1.0));
    open.resize(800, 300);
    assert_eq!(open.layout_size(), Size::new(800.0, 300.0));
    assert_eq!(open.scale(), 1.0);
    assert_eq!(open.ui.origin(), Some((0, 0)));
    assert!(open.ui.shown().as_ref() == open.ui.pixels().as_ref());
}
