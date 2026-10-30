//! Floating panels over the interface, driven headless: opening and
//! closing from their state, moving by the title bar and staying inside
//! the window, the position kept in a state, the order they are drawn
//! in, the interface around them going on, Escape, the focus going in
//! and back, assistive technology and the language, damage, and closed
//! panels costing nothing.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use manguebyte_gfx::{IRect, Point, Size};
use manguebyte_params::{FloatParam, FloatRange, ParamSet, param_set};
use manguebyte_ui::layout::Align;
use manguebyte_ui::testing::{Found, Harness};
use manguebyte_ui::{
    Action, Event, FloatingPanel, IntoView, Key, NodeId, ParamHandle, Role, State,
    TITLE_BAR_HEIGHT, TextSource, Theme, View, column, floating_panel, knob, label, pill, row,
};

struct FxParams {
    drive: FloatParam,
    size: FloatParam,
    decay: FloatParam,
    time: FloatParam,
    feedback: FloatParam,
}

param_set!(FxParams {
    drive,
    size,
    decay,
    time,
    feedback
});

fn fx_params() -> Arc<FxParams> {
    let percent = |id: &'static str, name: &'static str| {
        FloatParam::new(id, name, FloatRange::linear(0.0, 100.0), 50.0)
            .with_unit("%")
            .with_precision(0)
    };
    Arc::new(FxParams {
        drive: percent("drive", "Drive"),
        size: percent("size", "Size"),
        decay: percent("decay", "Decay"),
        time: percent("time", "Time"),
        feedback: percent("feedback", "Feedback"),
    })
}

/// One panel's state, as an application keeps it.
#[derive(Clone)]
struct PanelState {
    open: State<bool>,
    at: State<Point>,
}

impl PanelState {
    fn new(open: bool, at: (f32, f32)) -> PanelState {
        PanelState {
            open: State::new(open),
            at: State::new(Point::new(at.0, at.1)),
        }
    }
}

const WINDOW: Size = Size::new(640.0, 400.0);

/// A chain of two effects as pills, a knob that is not in any panel, and
/// the reverb's and the delay's panels over it.
fn screen(params: &Arc<FxParams>, reverb: &PanelState, delay: &PanelState) -> View {
    let p = |get: fn(&FxParams) -> &FloatParam| knob(ParamHandle::new(Arc::clone(params), get));
    column((
        row((
            pill("Reverb").opens(&reverb.open),
            pill("Delay").opens(&delay.open),
        ))
        .spacing(6.0),
        p(|p| &p.drive),
    ))
    .spacing(12.0)
    .padding(16.0)
    .align(Align::Start)
    .with_panels([
        floating_panel(
            "Reverb",
            reverb.open.clone(),
            reverb.at.clone(),
            row((p(|p| &p.size), p(|p| &p.decay))).spacing(8.0),
        ),
        floating_panel(
            "Delay",
            delay.open.clone(),
            delay.at.clone(),
            row((p(|p| &p.time), p(|p| &p.feedback))).spacing(8.0),
        ),
    ])
}

fn harness(
    reverb: &PanelState,
    delay: &PanelState,
    scale: f32,
) -> (Harness, Arc<FxParams>, PanelState, PanelState) {
    let params = fx_params();
    let mut ui = Harness::with_params(
        screen(&params, reverb, delay),
        WINDOW,
        scale,
        Arc::clone(&params) as Arc<dyn ParamSet>,
    );
    ui.frame();
    (ui, params, reverb.clone(), delay.clone())
}

fn focused(ui: &Harness) -> Option<String> {
    ui.focused().map(|found| found.name().to_owned())
}

/// Whether widget `id` is `region` or inside it.
fn inside(ui: &Harness, id: NodeId, region: &Found) -> bool {
    let mut at = Some(id);
    while let Some(id) = at {
        if id == region.id {
            return true;
        }
        at = ui.root().access(id).and_then(|entry| entry.parent);
    }
    false
}

/// A point on the title bar of `region`, clear of its close button.
fn bar(region: &Found) -> Point {
    Point::new(
        region.bounds.x + 24.0,
        region.bounds.y + TITLE_BAR_HEIGHT * 0.5,
    )
}

/// How far the panels' shadow reaches past them, in device pixels.
fn shadow_reach(theme: &Theme, scale: f32) -> i32 {
    let shadow = theme.elevation.lg;
    let offset = shadow.offset.x.abs().max(shadow.offset.y.abs());
    let logical = (shadow.reach_exact() + offset).ceil();
    (logical * scale).ceil() as i32 + 1
}

fn grown(rect: IRect, by: i32) -> IRect {
    IRect::from_edges(
        rect.x - by,
        rect.y - by,
        rect.right() as i32 + by,
        rect.bottom() as i32 + by,
    )
}

#[test]
fn a_panel_opens_and_closes_with_its_state_and_its_close_button() {
    let (mut ui, _, reverb, delay) = harness(
        &PanelState::new(false, (200.0, 120.0)),
        &PanelState::new(false, (360.0, 160.0)),
        1.0,
    );
    assert!(ui.find(Role::Region, "Reverb").is_none());
    assert!(
        ui.find(Role::Slider, "Size").is_none(),
        "closed: not described"
    );

    reverb.open.set(true);
    let frame = ui.frame();
    assert!(frame.laid_out && frame.damaged > 0);
    let panel = ui.get(Role::Region, "Reverb");
    assert_eq!((panel.bounds.x, panel.bounds.y), (200.0, 120.0));
    for name in ["Size", "Decay", "Close"] {
        let role = if name == "Close" {
            Role::Button
        } else {
            Role::Slider
        };
        assert!(panel.bounds.contains(ui.get(role, name).center()), "{name}");
    }
    assert!(
        ui.find(Role::Region, "Delay").is_none(),
        "the other stays closed"
    );
    assert_eq!(ui.frame().damaged, 0, "settled");

    // Its close button closes it, through the state.
    ui.click(ui.get(Role::Button, "Close").center());
    assert!(!reverb.open.get());
    ui.frame();
    assert!(ui.find(Role::Region, "Reverb").is_none());
    assert_eq!(ui.frame().damaged, 0);

    // A pill opens it; assistive technology can close it.
    ui.click(ui.get(Role::Button, "Reverb").center());
    assert!(reverb.open.get());
    ui.frame();
    let close = ui.get(Role::Button, "Close");
    assert!(ui.act(&close, Action::Press));
    assert!(!reverb.open.get());
    delay.open.set(true);
    ui.frame();
    assert!(ui.find(Role::Region, "Reverb").is_none());
    assert!(ui.find(Role::Region, "Delay").is_some());
}

#[test]
fn a_closed_panel_is_not_polled_and_costs_nothing() {
    let reads = Rc::new(Cell::new(0_u32));
    let counted = {
        let reads = Rc::clone(&reads);
        TextSource::dynamic(move || {
            reads.set(reads.get() + 1);
            "Tail"
        })
    };
    let open = State::new(false);
    let view = label("Work").with_panels([floating_panel(
        "Reverb",
        open.clone(),
        State::new(Point::new(40.0, 40.0)),
        manguebyte_ui::Label::from_source(counted),
    )]);
    let mut ui = Harness::new(view, Size::new(400.0, 300.0), 1.0);
    ui.frame();
    let after_first = reads.get();
    for _ in 0..100 {
        let frame = ui.frame();
        assert!(!frame.animating);
        assert_eq!((frame.damaged, frame.laid_out), (0, false));
    }
    assert_eq!(reads.get(), after_first, "not polled while closed");
    // Open, it is polled every frame, and draws nothing when idle.
    open.set(true);
    ui.frame();
    let open_reads = reads.get();
    for _ in 0..20 {
        assert_eq!(ui.frame().damaged, 0);
    }
    assert_eq!(reads.get(), open_reads + 20);
    open.set(false);
    ui.frame();
    let closed_reads = reads.get();
    for _ in 0..20 {
        assert_eq!(ui.frame().damaged, 0);
    }
    assert_eq!(reads.get(), closed_reads);
}

#[test]
fn the_title_bar_moves_a_panel_and_keeps_it_inside_the_window_at_every_scale() {
    for scale in [1.0, 1.25, 2.0] {
        let (mut ui, _, reverb, _) = harness(
            &PanelState::new(true, (200.0, 120.0)),
            &PanelState::new(false, (0.0, 0.0)),
            scale,
        );
        let panel = ui.get(Role::Region, "Reverb");
        let size = panel.bounds.size();
        // Thirty pixels right and ten down, in three moves.
        ui.drag(bar(&panel), (30.0, 10.0), 3);
        assert_eq!(reverb.at.get(), Point::new(230.0, 130.0), "{scale}");
        ui.frame();
        let moved = ui.get(Role::Region, "Reverb");
        assert!((moved.bounds.x - 230.0).abs() <= 1.0 / scale, "{scale}");
        assert!((moved.bounds.y - 130.0).abs() <= 1.0 / scale, "{scale}");
        assert_eq!(moved.bounds.size(), size, "{scale}: moving keeps its size");

        // Far past the bottom-right corner: it stops at the window's edges.
        ui.drag(bar(&moved), (2000.0, 2000.0), 2);
        let kept = reverb.at.get();
        assert_eq!(
            kept,
            Point::new(WINDOW.width - size.width, WINDOW.height - size.height),
            "{scale}"
        );
        ui.frame();
        let corner = ui.get(Role::Region, "Reverb");
        let window = IRect::new(0, 0, ui.pixels().width(), ui.pixels().height());
        assert!(
            window.contains_rect(&corner.device),
            "{scale}: {:?}",
            corner.device
        );
        assert!(
            corner.device.right() >= window.right() - 1
                && corner.device.bottom() >= window.bottom() - 1,
            "{scale}: against the corner, {:?}",
            corner.device
        );
        // Far past the top-left: at the origin.
        ui.drag(bar(&corner), (-3000.0, -3000.0), 2);
        assert_eq!(reverb.at.get(), Point::ZERO, "{scale}");
        ui.frame();
        let origin = ui.get(Role::Region, "Reverb");
        assert_eq!((origin.device.x, origin.device.y), (0, 0), "{scale}");
        // Every edge on a device pixel, so the panel stays sharp.
        assert!(ui.pixels() == &ui.render_fresh(), "{scale}");
    }
}

#[test]
fn where_a_panel_was_is_kept_in_its_state_and_restored() {
    let reverb = PanelState::new(true, (100.0, 90.0));
    let delay = PanelState::new(false, (300.0, 60.0));
    let (mut ui, _, _, _) = harness(&reverb, &delay, 1.0);
    let panel = ui.get(Role::Region, "Reverb");
    ui.drag(bar(&panel), (140.0, 60.0), 4);
    ui.frame();
    assert_eq!(reverb.at.get(), Point::new(240.0, 150.0));
    drop(ui);

    // Reopened from the same states, as a project or an editor spec keeps
    // them: there again, without anything being moved.
    let (mut ui, _, _, _) = harness(&reverb, &delay, 1.0);
    let panel = ui.get(Role::Region, "Reverb");
    assert_eq!((panel.bounds.x, panel.bounds.y), (240.0, 150.0));
    assert!(ui.find(Role::Region, "Delay").is_none());
    // The application moves it.
    reverb.at.set(Point::new(20.0, 200.0));
    ui.frame();
    let panel = ui.get(Role::Region, "Reverb");
    assert_eq!((panel.bounds.x, panel.bounds.y), (20.0, 200.0));

    // A window too small for where it was keeps it inside, and leaves the
    // state as it was, for a larger window later.
    reverb.at.set(Point::new(500.0, 300.0));
    ui.resize(Size::new(360.0, 260.0), 1.0);
    ui.frame();
    let panel = ui.get(Role::Region, "Reverb");
    assert!(panel.bounds.right() <= 360.0 && panel.bounds.bottom() <= 260.0);
    assert_eq!(reverb.at.get(), Point::new(500.0, 300.0));
    ui.resize(WINDOW, 1.0);
    ui.frame();
    let panel = ui.get(Role::Region, "Reverb");
    let fits = (
        500.0_f32.min(WINDOW.width - panel.bounds.width),
        300.0_f32.min(WINDOW.height - panel.bounds.height),
    );
    assert_eq!((panel.bounds.x, panel.bounds.y), fits);
}

#[test]
fn a_press_brings_a_panel_to_the_front() {
    // Two panels overlapping; the delay opened last is in front.
    let (mut ui, _, reverb, delay) = harness(
        &PanelState::new(false, (120.0, 100.0)),
        &PanelState::new(false, (200.0, 140.0)),
        1.0,
    );
    reverb.open.set(true);
    ui.frame();
    delay.open.set(true);
    ui.frame();
    let (back, front) = (
        ui.get(Role::Region, "Reverb"),
        ui.get(Role::Region, "Delay"),
    );
    let overlap = Point::new(
        (front.bounds.x + back.bounds.right()) * 0.5,
        (front.bounds.y + back.bounds.bottom()) * 0.5,
    );
    assert!(back.bounds.contains(overlap) && front.bounds.contains(overlap));
    let hit = ui.root().hit(overlap).expect("a panel is there");
    assert!(inside(&ui, hit, &front), "the delay is in front");
    assert!(ui.pixels() == &ui.render_fresh());

    // A press on the reverb where the delay does not cover it brings it to
    // the front, and draws again what it now covers.
    let clear = Point::new(back.bounds.x + 10.0, back.bounds.bottom() - 10.0);
    assert!(!front.bounds.contains(clear));
    ui.click(clear);
    let frame = ui.frame();
    assert!(frame.damaged > 0);
    let hit = ui.root().hit(overlap).expect("a panel is there");
    assert!(inside(&ui, hit, &back), "the reverb is in front now");
    assert!(ui.pixels() == &ui.render_fresh());
    assert!(
        ui.dump().rfind("text \"Reverb\"") > ui.dump().rfind("text \"Delay\""),
        "the reverb is drawn after the delay"
    );
    // A press on the panel in front changes nothing.
    ui.click(clear);
    assert_eq!(ui.frame().damaged, 0);

    // The focus moving into the one behind, by Tab, brings it forward.
    ui.root_mut().set_focus_to(None);
    let time = ui.get(Role::Slider, "Time");
    assert!(ui.act(&time, Action::Focus));
    ui.frame();
    let hit = ui.root().hit(overlap).expect("a panel is there");
    assert!(inside(&ui, hit, &front), "the delay is in front again");
    assert!(ui.pixels() == &ui.render_fresh());

    // Opening one that is open brings it forward too: its pill again.
    ui.click(ui.get(Role::Button, "Reverb").center());
    ui.frame();
    let hit = ui.root().hit(overlap).expect("a panel is there");
    assert!(inside(&ui, hit, &back));
}

#[test]
fn the_interface_around_and_under_open_panels_goes_on_working() {
    let (mut ui, params, reverb, delay) = harness(
        &PanelState::new(true, (300.0, 40.0)),
        &PanelState::new(true, (300.0, 220.0)),
        1.0,
    );
    // The drive knob beside the panels turns, and the pills press.
    let drive = ui.get(Role::Slider, "Drive");
    let before = params.drive.value();
    ui.drag(drive.center(), (0.0, -40.0), 4);
    ui.frame();
    assert!(params.drive.value() > before, "dragged beside the panels");
    ui.key(Key::Up);
    assert!(ui.get(Role::Slider, "Drive").is_focused());
    // A knob inside a panel turns as well.
    let size = ui.get(Role::Slider, "Size");
    let before = params.size.value();
    ui.drag(size.center(), (0.0, -20.0), 2);
    assert!(params.size.value() > before);

    // A panel moved over the drive knob covers it: a press there is the
    // panel's, and the knob does not move.
    reverb
        .at
        .set(Point::new(drive.bounds.x - 20.0, drive.bounds.y - 10.0));
    ui.frame();
    let panel = ui.get(Role::Region, "Reverb");
    assert!(panel.bounds.contains(drive.center()));
    let hit = ui.root().hit(drive.center()).expect("something is there");
    assert!(inside(&ui, hit, &panel));
    let before = params.drive.value();
    ui.drag(drive.center(), (0.0, -40.0), 4);
    assert_eq!(params.drive.value(), before, "covered");
    // Moved off, the knob is there again; no scrim was ever drawn.
    reverb.at.set(Point::new(300.0, 40.0));
    ui.frame();
    ui.drag(drive.center(), (0.0, -40.0), 4);
    assert!(params.drive.value() > before);
    assert!(ui.root().modal().is_none());
    assert!(ui.find(Role::Region, "Delay").is_some());
    assert!(delay.open.get());
}

#[test]
fn escape_closes_the_panel_with_the_focus_and_no_other() {
    let (mut ui, _, reverb, delay) = harness(
        &PanelState::new(true, (200.0, 40.0)),
        &PanelState::new(true, (200.0, 220.0)),
        1.0,
    );
    // Nothing focused: Escape is left to the host and closes nothing.
    assert!(!ui.key(Key::Escape));
    assert!(reverb.open.get() && delay.open.get());
    // The focus on a knob in the delay's panel: Escape closes the delay.
    let feedback = ui.get(Role::Slider, "Feedback");
    assert!(ui.act(&feedback, Action::Focus));
    assert!(ui.key(Key::Escape));
    assert!(!delay.open.get());
    assert!(reverb.open.get(), "only the panel with the focus");
    ui.frame();
    assert!(ui.find(Role::Region, "Delay").is_none());
    // The focus on the drive knob, outside any panel: Escape is not used.
    let drive = ui.get(Role::Slider, "Drive");
    assert!(ui.act(&drive, Action::Focus));
    assert!(!ui.key(Key::Escape));
    assert!(reverb.open.get());
    // On the reverb's close button.
    let close = ui.get(Role::Button, "Close");
    assert!(ui.act(&close, Action::Focus));
    assert!(ui.key(Key::Escape));
    assert!(!reverb.open.get());
}

#[test]
fn the_focus_enters_a_panel_when_it_opens_and_goes_back_when_it_closes() {
    let (mut ui, _, reverb, delay) = harness(
        &PanelState::new(false, (200.0, 40.0)),
        &PanelState::new(false, (200.0, 220.0)),
        1.0,
    );
    // From the keyboard: Tab to the reverb's pill, Enter.
    assert!(ui.tab());
    assert_eq!(focused(&ui).as_deref(), Some("Reverb"));
    ui.key(Key::Enter);
    ui.frame();
    assert!(reverb.open.get());
    assert_eq!(
        focused(&ui).as_deref(),
        Some("Size"),
        "the first knob inside"
    );
    // Tab goes through its content, then its close button, then on.
    assert!(ui.tab());
    assert_eq!(focused(&ui).as_deref(), Some("Decay"));
    assert!(ui.tab());
    assert_eq!(focused(&ui).as_deref(), Some("Close"));
    // Escape closes it; the focus is back on the pill.
    ui.key(Key::Escape);
    ui.frame();
    assert_eq!(focused(&ui).as_deref(), Some("Reverb"));

    // With the pointer: a click on the delay's pill (which does not take
    // the focus) opens it with the focus inside; a click on its close
    // button gives the focus to the pill.
    ui.root_mut().set_focus_to(None);
    ui.click(ui.get(Role::Button, "Delay").center());
    ui.frame();
    assert_eq!(focused(&ui).as_deref(), Some("Time"));
    ui.click(ui.get(Role::Button, "Close").center());
    ui.frame();
    assert!(!delay.open.get());
    assert_eq!(focused(&ui).as_deref(), Some("Delay"));

    // Closed from outside while the focus is elsewhere, it leaves the
    // focus where it is.
    reverb.open.set(true);
    ui.frame();
    assert_eq!(focused(&ui).as_deref(), Some("Size"));
    let drive = ui.get(Role::Slider, "Drive");
    assert!(ui.act(&drive, Action::Focus));
    reverb.open.set(false);
    ui.frame();
    assert_eq!(focused(&ui).as_deref(), Some("Drive"));

    // A press on a panel's title bar leaves the focus inside it.
    delay.open.set(true);
    ui.frame();
    assert_eq!(focused(&ui).as_deref(), Some("Time"));
    ui.click(bar(&ui.get(Role::Region, "Delay")));
    assert_eq!(focused(&ui).as_deref(), Some("Time"));

    // Panels open when the interface is built take no focus.
    let (ui, _, _, _) = harness(
        &PanelState::new(true, (200.0, 40.0)),
        &PanelState::new(true, (200.0, 220.0)),
        1.0,
    );
    assert_eq!(focused(&ui), None);
}

#[test]
fn a_panel_is_a_named_region_that_follows_the_language() {
    let language = State::new(0_usize);
    let text = |words: [&'static str; 3]| {
        let language = language.clone();
        TextSource::dynamic(move || words[language.get()])
    };
    let open = State::new(true);
    let panel: FloatingPanel = floating_panel(
        text(["Delay", "Atraso", "Retardo"]),
        open.clone(),
        State::new(Point::new(60.0, 50.0)),
        column((
            manguebyte_ui::Label::from_source(text(["Time", "Tempo de repetição", "Tiempo"])),
            pill("Sync"),
        )),
    )
    .close_label(text(["Close", "Fechar", "Cerrar"]));
    let view = label("Effects").with_panels([panel]);
    let mut ui = Harness::new(view, Size::new(500.0, 300.0), 1.0);
    ui.frame();
    let region = ui.get(Role::Region, "Delay");
    ui.assert_accessible();
    assert!(
        region
            .bounds
            .contains(ui.get(Role::Button, "Close").center())
    );
    assert!(
        region
            .bounds
            .contains(ui.get(Role::Button, "Sync").center())
    );
    let narrow = region.bounds.width;

    language.set(1);
    let frame = ui.frame();
    assert!(frame.laid_out);
    let region = ui.get(Role::Region, "Atraso");
    assert!(ui.find(Role::Region, "Delay").is_none());
    assert!(ui.find(Role::Button, "Fechar").is_some());
    assert!(region.bounds.width > narrow, "a longer text widens it");
    assert!(
        region
            .bounds
            .contains(ui.get(Role::Button, "Fechar").center())
    );
    assert!(ui.dump().contains("text \"Atraso\""));
    let told: Vec<Role> = ui.access_updates().iter().map(Found::role).collect();
    assert!(told.contains(&Role::Region), "{told:?}");
    assert!(told.contains(&Role::Button), "{told:?}");
    ui.assert_accessible();
    assert!(ui.pixels() == &ui.render_fresh());

    let wide = region.bounds.width;
    language.set(2);
    ui.frame();
    let region = ui.get(Role::Region, "Retardo");
    assert!(region.bounds.width < wide, "and a shorter one narrows it");
    let frame = ui.frame();
    assert_eq!((frame.damaged, frame.laid_out), (0, false));
}

#[test]
fn moving_opening_and_closing_draw_what_a_whole_repaint_draws() {
    let theme = Theme::dark();
    for scale in [1.0, 1.25, 2.0] {
        let (mut ui, _, reverb, delay) = harness(
            &PanelState::new(true, (180.0, 60.0)),
            &PanelState::new(false, (330.0, 170.0)),
            scale,
        );
        let reach = shadow_reach(&theme, scale);
        // Opening draws the panel and its shadow, and nothing else.
        delay.open.set(true);
        ui.frame();
        let opened = ui.get(Role::Region, "Delay");
        let damage = ui.last_damage_bounds().expect("drawn");
        assert!(
            grown(opened.device, reach).contains_rect(&damage),
            "{scale}: {damage:?}"
        );
        assert!(ui.pixels() == &ui.render_fresh(), "{scale}: opened");

        // Each step of a drag draws where the panel was and where it is.
        let panel = ui.get(Role::Region, "Reverb");
        let mut at = bar(&panel);
        ui.press(at);
        let mut was = panel.device;
        for _ in 0..6 {
            at = Point::new(at.x + 7.3, at.y + 3.1);
            ui.move_to(at);
            let frame = ui.frame();
            assert!(frame.damaged > 0, "{scale}");
            let now = ui.get(Role::Region, "Reverb").device;
            let allowed = grown(was, reach).union(&grown(now, reach));
            for rect in ui.last_damage() {
                assert!(
                    allowed.contains_rect(rect),
                    "{scale}: {rect:?} outside {allowed:?}"
                );
            }
            assert!(ui.pixels() == &ui.render_fresh(), "{scale}: moving");
            was = now;
        }
        ui.release(at);
        ui.send(Event::PointerLeave);
        ui.frame();
        assert_eq!(ui.frame().damaged, 0, "{scale}: idle");

        // The buffer drawn in pieces equals a new interface drawn whole
        // from the same states, once the reverb, which the drag brought
        // forward, is brought forward in it too.
        let (mut fresh, _, _, _) = harness(&reverb, &delay, scale);
        fresh.click(bar(&fresh.get(Role::Region, "Reverb")));
        fresh.send(Event::PointerLeave);
        fresh.frame();
        assert!(ui.pixels() == fresh.pixels(), "{scale}: as a whole repaint");

        // Closing draws again what it covered.
        reverb.open.set(false);
        ui.frame();
        let damage = ui.last_damage_bounds().expect("drawn");
        assert!(
            grown(was, reach).contains_rect(&damage),
            "{scale}: {damage:?}"
        );
        assert!(ui.pixels() == &ui.render_fresh(), "{scale}: closed");
        assert_eq!(ui.frame().damaged, 0);
        let (fresh, _, _, _) = harness(&reverb, &delay, scale);
        assert!(
            ui.pixels() == fresh.pixels(),
            "{scale}: closed, as a whole repaint"
        );
    }
}

#[test]
fn a_dialog_over_open_panels_covers_them() {
    use manguebyte_ui::{DialogState, action_button, confirm_dialog};

    let ask = DialogState::new();
    let open = State::new(true);
    let view = column((action_button("Delete preset").opens(&ask),))
        .padding(16.0)
        .with_panels([floating_panel(
            "Reverb",
            open.clone(),
            State::new(Point::new(120.0, 80.0)),
            pill("Freeze"),
        )])
        .with_dialog(confirm_dialog(
            ask.clone(),
            "Delete it?",
            "It cannot be undone.",
        ));
    let mut ui = Harness::new(view, Size::new(560.0, 360.0), 1.0);
    ui.frame();
    let freeze = ui.get(Role::Button, "Freeze");
    // Brought to the front, it is still under the dialog.
    ui.click(freeze.center());
    ask.open();
    ui.frame();
    let hit = ui.root().hit(freeze.center()).expect("the dialog is there");
    assert!(inside(&ui, hit, &ui.get(Role::Dialog, "Delete it?")));
    assert!(ui.dump().rfind("text \"Delete it?\"") > ui.dump().rfind("text \"Freeze\""));
    // Escape is the dialog's, and leaves the panel open.
    ui.key(Key::Escape);
    ui.frame();
    assert!(open.get());
    assert!(ui.find(Role::Region, "Reverb").is_some());
    assert!(ui.pixels() == &ui.render_fresh());
}
