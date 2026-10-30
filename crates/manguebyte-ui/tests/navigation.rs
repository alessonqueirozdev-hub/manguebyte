//! Tabs and a sidebar of pages, driven headless: switching pages without
//! building anything again, hidden pages that cost nothing, the keys,
//! assistive technology, damage and the selection indicator's motion.

use std::cell::Cell;
use std::rc::Rc;

use manguebyte_gfx::{IRect, Scene, Size};
use manguebyte_ui::access::{AccessNode, Actions, Role};
use manguebyte_ui::testing::{Found, Harness};
use manguebyte_ui::{
    AccessCx, Action, Button, Event, EventCx, Handled, Icon, IntoView, Key, MeasureCx, Modifiers,
    PaintCx, State, TextSource, UpdateCx, View, Widget, column, label, pill, row, scroll_view,
    sidebar, tabs,
};

const CONTROL: Modifiers = Modifiers {
    control: true,
    ..Modifiers::NONE
};
const CONTROL_SHIFT: Modifiers = Modifiers {
    control: true,
    shift: true,
    ..Modifiers::NONE
};

/// What a probe saw.
#[derive(Default)]
struct Counts {
    updates: Cell<u32>,
    paints: Cell<u32>,
    measures: Cell<u32>,
}

/// A widget that counts how often the root calls it, keeps a count of its
/// own clicks as its state, and asks for a frame on every one it is
/// polled in, as something always moving would.
struct Probe {
    name: &'static str,
    counts: Rc<Counts>,
    clicks: u32,
    moving: bool,
}

fn probe(name: &'static str, counts: &Rc<Counts>, moving: bool) -> Probe {
    Probe {
        name,
        counts: Rc::clone(counts),
        clicks: 0,
        moving,
    }
}

impl Widget for Probe {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Button, self.name)
            .with_value(self.clicks.to_string())
            .with_actions(Actions::PRESS)
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        self.counts.measures.set(self.counts.measures.get() + 1);
        Size::new(60.0, 24.0)
    }

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        self.counts.updates.set(self.counts.updates.get() + 1);
        if self.moving {
            cx.request_frame();
            cx.request_paint();
        }
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        self.counts.paints.set(self.counts.paints.get() + 1);
        let shade = (self.clicks * 40 % 255) as u8;
        scene.fill(
            cx.rect(),
            manguebyte_gfx::Color::rgb(shade, 0x40, 0x80).with_alpha(0xFF),
        );
        // Something different every frame while moving.
        if self.moving {
            let at = (self.counts.paints.get() % 20) as f32;
            scene.fill(
                manguebyte_gfx::Rect::new(cx.rect().x + at, cx.rect().y, 2.0, 2.0),
                manguebyte_gfx::Color::WHITE,
            );
        }
    }

    fn event(&mut self, cx: &mut EventCx<'_>, event: &Event) -> Handled {
        if let Event::PointerDown {
            button: Button::Primary,
            ..
        } = event
        {
            self.clicks += 1;
            cx.request_paint();
            return Handled::Yes;
        }
        Handled::No
    }
}

/// A Parabolic-like screen: a title above three pages and a status line
/// below. The synth page has a pill and a probe that counts its clicks,
/// the effects page a probe that is always moving and a scroll view, the
/// presets page a label.
fn screen(page: &State<usize>, synth: &Rc<Counts>, effects: &Rc<Counts>) -> View {
    let rows: Vec<_> = (0..30)
        .map(|i| label(format!("Slot {i}")).height(20.0))
        .collect();
    column((
        label("Parabolic").title(),
        tabs("Sections", page.clone())
            .page(
                "Synth",
                column((pill("Play"), probe("Oscillator", synth, false))).spacing(8.0),
            )
            .page(
                "Effects",
                row((
                    probe("Reverb", effects, true),
                    scroll_view("Slots", column(rows)).flex(1.0),
                ))
                .spacing(8.0),
            )
            .page("Presets", label("Init patch"))
            .flex(1.0),
        label("Ready").caption(),
    ))
    .spacing(6.0)
    .padding(10.0)
}

fn open() -> (Harness, State<usize>, Rc<Counts>, Rc<Counts>) {
    let page = State::new(0);
    let (synth, effects) = (Rc::new(Counts::default()), Rc::new(Counts::default()));
    let mut ui = Harness::new(
        screen(&page, &synth, &effects),
        Size::new(420.0, 320.0),
        1.0,
    );
    ui.frame();
    (ui, page, synth, effects)
}

fn selected_tab(ui: &Harness) -> String {
    let tabs: Vec<Found> = ui
        .everything()
        .into_iter()
        .filter(|found| found.role() == Role::Tab && found.node.state.selected == Some(true))
        .collect();
    assert_eq!(tabs.len(), 1, "one tab is selected");
    tabs[0].name().to_owned()
}

#[test]
fn tabs_change_page_by_clicks_keys_and_the_application() {
    let (mut ui, page, ..) = open();
    assert_eq!(selected_tab(&ui), "Synth");
    // A click on a tab.
    ui.click(ui.get(Role::Tab, "Presets").center());
    ui.frame();
    assert_eq!((page.get(), selected_tab(&ui).as_str()), (2, "Presets"));
    assert!(ui.find(Role::Label, "Init patch").is_some());
    assert!(
        ui.get(Role::TabList, "Sections").is_focused(),
        "a click focuses the bar"
    );
    // The arrows go round at the ends; Home and End go to them.
    ui.key(Key::Right);
    ui.frame();
    assert_eq!(selected_tab(&ui), "Synth");
    ui.key(Key::Left);
    ui.frame();
    assert_eq!(selected_tab(&ui), "Presets");
    ui.key(Key::Home);
    ui.frame();
    assert_eq!(selected_tab(&ui), "Synth");
    ui.key(Key::End);
    ui.frame();
    assert_eq!(selected_tab(&ui), "Presets");
    // Up and Down are the sidebar's: a tab bar leaves them.
    assert!(!ui.key(Key::Down));
    // Keys pressed between two frames add up.
    ui.key(Key::Right);
    ui.key(Key::Right);
    ui.frame();
    assert_eq!(selected_tab(&ui), "Effects");

    // Control+Tab from inside a page, where the pill has the focus.
    page.set(0);
    ui.frame();
    let play = ui.get(Role::Button, "Play");
    assert!(ui.act(&play, Action::Focus));
    assert!(ui.key_with(Key::Tab, CONTROL));
    ui.frame();
    assert_eq!(selected_tab(&ui), "Effects");
    // The pill went with its page, and the focus with it.
    assert!(ui.find(Role::Button, "Play").is_none());
    assert!(ui.focused().is_none());
    // With nothing focused Control+Tab still reaches the tabs, round both
    // ways, and Tab itself moves the focus as ever.
    assert!(ui.key_with(Key::Tab, CONTROL));
    ui.frame();
    assert_eq!(selected_tab(&ui), "Presets");
    assert!(ui.key_with(Key::Tab, CONTROL));
    ui.frame();
    assert_eq!(selected_tab(&ui), "Synth");
    assert!(ui.key_with(Key::Tab, CONTROL_SHIFT));
    ui.frame();
    assert_eq!(selected_tab(&ui), "Presets");
    assert!(ui.tab());
    assert_eq!(
        ui.focused().map(|found| found.node.role),
        Some(Role::TabList)
    );

    // The application changes page itself; a number past the last shows
    // the last.
    page.set(1);
    ui.frame();
    assert_eq!(selected_tab(&ui), "Effects");
    page.set(99);
    ui.frame();
    assert_eq!(selected_tab(&ui), "Presets");
    assert!(ui.find(Role::Label, "Init patch").is_some());
    ui.assert_accessible();
}

#[test]
fn tabs_are_a_tab_list_of_tabs_and_a_named_panel() {
    let (mut ui, _page, ..) = open();
    let list = ui.get(Role::TabList, "Sections");
    assert_eq!(list.value_text(), Some("Synth"));
    assert!(list.node.actions.focus);
    let names: Vec<(String, Option<String>, Option<bool>)> = ui
        .everything()
        .into_iter()
        .filter(|found| found.role() == Role::Tab)
        .map(|found| {
            (
                found.name().to_owned(),
                found.value_text().map(str::to_owned),
                found.node.state.selected,
            )
        })
        .collect();
    assert_eq!(
        names,
        [
            ("Synth".into(), Some("1 of 3".into()), Some(true)),
            ("Effects".into(), Some("2 of 3".into()), Some(false)),
            ("Presets".into(), Some("3 of 3".into()), Some(false)),
        ]
    );
    // One panel is there, the shown page's, named as its tab, holding the
    // page; the hidden pages' panels are not described.
    let panels: Vec<Found> = ui
        .everything()
        .into_iter()
        .filter(|found| found.role() == Role::TabPanel)
        .collect();
    assert_eq!(panels.len(), 1);
    assert_eq!(panels[0].name(), "Synth");
    let play = ui.get(Role::Button, "Play");
    assert!(panels[0].bounds.contains(play.center()));
    assert!(ui.find(Role::Button, "Reverb").is_none());
    ui.assert_accessible();

    // Pressing a tab, as assistive technology does, selects it; focusing
    // one focuses the bar and selects it too.
    let effects = ui.get(Role::Tab, "Effects");
    assert!(ui.act(&effects, Action::Press));
    ui.frame();
    assert_eq!(ui.get(Role::TabPanel, "Effects").name(), "Effects");
    assert!(
        ui.access_updates()
            .iter()
            .any(|f| f.role() == Role::TabList)
    );
    let presets = ui.get(Role::Tab, "Presets");
    assert!(ui.act(&presets, Action::Focus));
    ui.frame();
    assert_eq!(selected_tab(&ui), "Presets");
    let focused = ui.get(Role::Tab, "Presets");
    assert!(focused.is_focused(), "the selected tab of a focused bar");
    assert!(!ui.get(Role::Tab, "Synth").is_focused());
    ui.assert_accessible();
}

#[test]
fn hidden_pages_are_not_polled_laid_out_or_drawn_and_ask_for_no_frames() {
    let (mut ui, page, synth, effects) = open();
    // The effects page holds something always moving; hidden, it is never
    // reached.
    let (updates, paints, measures) = (
        effects.updates.get(),
        effects.paints.get(),
        effects.measures.get(),
    );
    assert_eq!((updates, paints), (0, 0), "a hidden page is never polled");
    let before = ui.frames_rendered();
    for _ in 0..200 {
        let frame = ui.frame();
        assert!(!frame.animating, "a hidden page asks for no frames");
        assert_eq!(frame.damaged, 0);
    }
    assert_eq!(ui.frames_rendered(), before);
    assert_eq!(effects.updates.get(), 0);
    assert_eq!(effects.paints.get(), 0);
    assert_eq!(effects.measures.get(), measures);
    let synth_updates = synth.updates.get();
    assert!(synth_updates >= 200, "the page shown is polled every frame");

    // Shown, it is polled, laid out, drawn, and keeps frames coming.
    page.set(1);
    let frame = ui.frame();
    assert!(frame.laid_out && frame.animating);
    assert!(effects.updates.get() == 1 && effects.paints.get() == 1);
    assert!(effects.measures.get() > measures);
    // Hidden again, it stops at once, and so does the synth page shown.
    page.set(0);
    let (updates, paints) = (effects.updates.get(), effects.paints.get());
    ui.frame();
    for _ in 0..50 {
        assert!(!ui.frame().animating);
    }
    assert_eq!(
        (effects.updates.get(), effects.paints.get()),
        (updates, paints)
    );
    // The synth page was not polled while hidden.
    let synth_seen = synth.updates.get();
    page.set(2);
    for _ in 0..20 {
        ui.frame();
    }
    assert_eq!(synth.updates.get(), synth_seen);
}

#[test]
fn a_hidden_page_keeps_its_widgets_and_their_state() {
    let (mut ui, page, ..) = open();
    let widgets = ui.root().len();
    let oscillator = ui.get(Role::Button, "Oscillator");
    for _ in 0..3 {
        ui.click(oscillator.center());
    }
    ui.frame();
    assert_eq!(ui.get(Role::Button, "Oscillator").value_text(), Some("3"));
    // The effects page's scroll view scrolled down a little.
    page.set(1);
    ui.frame();
    let slots = ui.get(Role::ScrollView, "Slots");
    ui.wheel(slots.center(), -3.0);
    ui.frame();
    let scrolled = ui.root().scroll_metrics(slots.id).expect("scrolls").offset;
    assert!(scrolled.y > 0.0);

    // Round trips through the other pages build nothing and lose nothing.
    for to in [2, 0, 2, 1, 0] {
        page.set(to);
        ui.frame();
        assert_eq!(ui.root().len(), widgets, "nothing was built again");
    }
    let again = ui.get(Role::Button, "Oscillator");
    assert_eq!(again.id, oscillator.id, "the same widget");
    assert_eq!(again.value_text(), Some("3"));
    page.set(1);
    ui.frame();
    assert_eq!(
        ui.root()
            .scroll_metrics(ui.get(Role::ScrollView, "Slots").id)
            .expect("scrolls")
            .offset,
        scrolled
    );
    // What was drawn is what a whole render of the scene draws.
    assert!(ui.pixels() == &ui.render_fresh());
}

#[test]
fn a_switch_draws_only_the_bar_and_the_content_area() {
    for scale in [1.0, 1.25, 2.0] {
        let page = State::new(0);
        let (synth, effects) = (Rc::new(Counts::default()), Rc::new(Counts::default()));
        let mut ui = Harness::new(
            screen(&page, &synth, &effects),
            Size::new(420.0, 320.0),
            scale,
        );
        ui.frame();
        let bar = ui.get(Role::TabList, "Sections").device;
        let content = ui.get(Role::TabPanel, "Synth").device;
        let title = ui.get(Role::Label, "Parabolic").device;
        let status = ui.get(Role::Label, "Ready").device;
        for to in [2, 1, 2] {
            page.set(to);
            let frame = ui.frame();
            assert!(frame.damaged > 0, "{scale}: something is drawn");
            // The bar and the content area are one above the other, the
            // same width: together they are their union, which damage
            // merged across both stays inside.
            let both = bar.union(&content);
            assert_eq!(both.area(), bar.area() + content.area(), "{scale}");
            for rect in ui.last_damage() {
                assert!(
                    both.contains_rect(rect),
                    "{scale}: {rect:?} is outside the bar {bar:?} and the content {content:?}"
                );
                for other in [title, status] {
                    assert!(rect.intersect(&other).is_none(), "{scale}: {rect:?}");
                }
            }
            // The panel shown fills the same area as the one before.
            let shown = ["Synth", "Effects", "Presets"][to];
            assert_eq!(ui.get(Role::TabPanel, shown).device, content, "{scale}");
            // And the result is what a whole render gives.
            assert!(ui.pixels() == &ui.render_fresh(), "{scale}");
            page.set(0);
            ui.frame();
            if to == 1 {
                // The moving probe stopped being polled.
                assert!(!ui.frame().animating);
            }
        }
    }
}

/// The selection indicator, as the bar's scene records it: the last
/// rounded rectangle it fills, the underline drawn over the tabs.
fn indicator(ui: &Harness) -> String {
    let bar = ui.get(Role::TabList, "Sections");
    let dump = ui.dump_of(&bar);
    dump.lines()
        .rfind(|line| line.starts_with("fill rounded_rect("))
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn the_indicator_slides_with_motion_and_jumps_without() {
    let (mut ui, page, ..) = open();
    ui.set_motion(true);
    ui.advance_ms(50);
    let start = indicator(&ui);
    let bar = ui.get(Role::TabList, "Sections").device;
    page.set(2);
    let mut frames = 0;
    let mut seen = vec![start.clone()];
    ui.frame();
    loop {
        ui.advance_ms(16);
        let now = indicator(&ui);
        if seen.last() != Some(&now) {
            seen.push(now);
        }
        frames += 1;
        // After the switch itself, only the bar is drawn while it slides.
        for rect in ui.last_damage() {
            assert!(bar.contains_rect(rect), "{rect:?} outside the bar {bar:?}");
        }
        if !ui.root().is_animating() || frames > 60 {
            break;
        }
    }
    // The base duration, 180 ms, is about eleven frames of 16 ms.
    assert!((8..=14).contains(&frames), "{frames} frames");
    assert!(seen.len() >= 6, "it moved through {} places", seen.len());
    let end = indicator(&ui);
    assert_ne!(start, end);
    // Settled: nothing more is drawn.
    assert_eq!(ui.frame().damaged, 0);

    // With motion off it jumps in one frame.
    ui.set_motion(false);
    page.set(0);
    let frame = ui.frame();
    assert!(!frame.animating);
    assert_eq!(indicator(&ui), start);
    assert_eq!(ui.frame().damaged, 0);
}

/// A small square icon.
fn square() -> Icon {
    Icon::new(|scene, rect, color| scene.fill(rect.outset(-3.0), color))
}

fn console(division: &State<usize>) -> View {
    sidebar("Divisions", division.clone())
        .section("Manuals")
        .page("Great", label("Great stops"))
        .icon(square())
        .page("Swell", label("Swell stops"))
        .icon(square())
        .section("Pedal")
        .page("Pedal", label("Pedal stops"))
        .into_view()
}

#[test]
fn a_sidebar_has_sections_icons_and_the_keys_down_its_length() {
    let division = State::new(0);
    let mut ui = Harness::new(console(&division), Size::new(480.0, 300.0), 1.0);
    ui.frame();
    let list = ui.get(Role::TabList, "Divisions");
    // The sidebar is down the left, the page beside it.
    let page = ui.get(Role::TabPanel, "Great");
    assert!(list.bounds.right() <= page.bounds.x);
    assert!(list.bounds.width >= 184.0);
    // Headers are labels among the tabs, above the tabs they head.
    let manuals = ui.get(Role::Label, "Manuals");
    let pedal_header = ui.get(Role::Label, "Pedal");
    let great = ui.get(Role::Tab, "Great");
    let pedal = ui.get(Role::Tab, "Pedal");
    assert!(manuals.bounds.bottom() <= great.bounds.y);
    assert!(pedal_header.bounds.bottom() <= pedal.bounds.y);
    assert!(manuals.id == list.id && manuals.item.is_some());
    // Icons are drawn in their text's colour: the selected page's in the
    // text colour, the other's in the secondary one.
    let dump = ui.dump_of(&list);
    let hex = |c: manguebyte_gfx::Color| format!("#{:02x}{:02x}{:02x}{:02x}", c.r, c.g, c.b, c.a);
    let palette = ui.root().theme().palette;
    let squares: Vec<&str> = dump
        .lines()
        .filter(|line| line.starts_with("fill rect(") && !line.contains(&hex(palette.hairline)))
        .collect();
    assert_eq!(squares.len(), 2, "{dump}");
    assert!(squares[0].ends_with(&format!("solid({})", hex(palette.text))));
    assert!(squares[1].ends_with(&format!("solid({})", hex(palette.text_secondary))));
    ui.assert_accessible();

    ui.act(&list, Action::Focus);
    for (key, expected) in [
        (Key::Down, 1),
        (Key::Down, 2),
        (Key::Down, 0),
        (Key::Up, 2),
        (Key::Home, 0),
        (Key::End, 2),
    ] {
        assert!(ui.key(key));
        ui.frame();
        assert_eq!(division.get(), expected, "{key:?}");
    }
    assert!(ui.find(Role::Label, "Pedal stops").is_some());
    // Left and Right are a tab bar's: a sidebar leaves them.
    assert!(!ui.key(Key::Left));
    // A click on a page's row.
    ui.click(ui.get(Role::Tab, "Swell").center());
    ui.frame();
    assert_eq!(division.get(), 1);
    // A click on a header does nothing.
    ui.click(ui.get(Role::Label, "Pedal").center());
    ui.frame();
    assert_eq!(division.get(), 1);
}

#[test]
fn switching_the_language_renames_tabs_and_panels_in_the_next_frame() {
    let language = State::new(0_usize);
    let text = |words: [&'static str; 2]| {
        let language = language.clone();
        TextSource::dynamic(move || words[language.get()])
    };
    let page = State::new(1);
    let view = tabs(text(["Sections", "Seções"]), page.clone())
        .page(text(["Synth", "Síntese"]), label("a"))
        .page(text(["Effects", "Efeitos"]), label("b"))
        .page(text(["Presets", "Predefinições"]), label("c"));
    let mut ui = Harness::new(view, Size::new(480.0, 200.0), 1.0);
    ui.frame();
    ui.frame();
    let bar = ui.get(Role::TabList, "Sections");
    language.set(1);
    let frame = ui.frame();
    assert!(frame.laid_out, "the tabs' widths follow their names");
    assert!(ui.find(Role::TabList, "Seções").is_some());
    for name in ["Síntese", "Efeitos", "Predefinições"] {
        assert!(ui.find(Role::Tab, name).is_some(), "{name}");
    }
    assert!(ui.find(Role::Tab, "Effects").is_none());
    assert_eq!(ui.get(Role::TabPanel, "Efeitos").name(), "Efeitos");
    assert_eq!(
        ui.get(Role::TabList, "Seções").value_text(),
        Some("Efeitos")
    );
    assert!(ui.dump_of(&bar).contains("text \"Predefinições\""));
    let told: Vec<Role> = ui.access_updates().iter().map(Found::role).collect();
    assert_eq!(told, [Role::TabList, Role::TabPanel]);
    ui.assert_accessible();
    let frame = ui.frame();
    assert_eq!((frame.damaged, frame.laid_out), (0, false));
    // Bounds of the bar did not move: only its tabs grew.
    let bar_after = ui.get(Role::TabList, "Seções");
    assert_eq!(bar_after.bounds.y, bar.bounds.y);
    let _: IRect = bar_after.device;
}
