//! The accessibility model: what each widget is, for screen readers and
//! for tests.
//!
//! Every widget describes itself with an [`AccessNode`]: its [`Role`], its
//! name, its value as text and as a number within a range, its state, and
//! the [`Action`]s it accepts. [`Widget::access`](crate::Widget::access)
//! has no default, so a widget cannot be written without saying what it
//! is.
//!
//! The model is the framework's own and has no dependency. Platform
//! adapters (UI Automation, NSAccessibility, AT-SPI) will translate it
//! later; until then it serves the test harness, which finds widgets only
//! by role and name, as a screen reader would.
//!
//! ```
//! use manguebyte_ui::access::{AccessNode, AccessRange, Actions, Role};
//!
//! let node = AccessNode::new(Role::Slider, "Cutoff")
//!     .with_value("1000 Hz")
//!     .with_numeric(1000.0, AccessRange::new(20.0, 20_000.0, None))
//!     .with_actions(Actions::ADJUST);
//! assert_eq!(node.value.as_deref(), Some("1000 Hz"));
//! assert!(node.actions.increment && node.actions.focus);
//! ```

use manguebyte_gfx::{Color, IRect, Rect};

use crate::theme::Theme;
use crate::tree::NodeId;

/// What kind of thing a widget is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Role {
    /// A container of other widgets.
    Group,
    /// Text that is read, not changed.
    Label,
    /// A value set within a range: knobs and sliders.
    Slider,
    /// Something that is on or off.
    Switch,
    /// Something that is pressed to do one thing.
    Button,
    /// A level that is shown, not changed.
    Meter,
    /// A picture or drawing.
    Image,
    /// One line of text a person types: its value is the text.
    TextInput,
    /// A value chosen from a list that opens as a menu.
    ComboBox,
    /// A list of things to choose from, open over the interface.
    Menu,
    /// One thing to choose in a menu.
    MenuItem,
    /// A line between groups of items in a menu.
    Separator,
    /// Help text shown near the pointer: its name is the text.
    Tooltip,
    /// A view that shows part of something larger and scrolls over it:
    /// its value is how far it is scrolled.
    ScrollView,
    /// A list of rows to choose from, some of which may be selected.
    List,
    /// One row of a list: it says whether it is selected.
    ListItem,
    /// What is being dragged, drawn under the pointer: its name is the
    /// description the drag was given.
    DragItem,
}

/// The range of a widget's numeric value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AccessRange {
    /// The lowest value.
    pub min: f64,
    /// The highest value.
    pub max: f64,
    /// The distance between neighbouring values, if they are stepped.
    pub step: Option<f64>,
}

impl AccessRange {
    /// A range from `min` to `max`, stepped by `step` if given.
    pub const fn new(min: f64, max: f64, step: Option<f64>) -> Self {
        AccessRange { min, max, step }
    }
}

/// The state of a widget that changes while it is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct AccessState {
    /// It has the keyboard focus. The root fills this in.
    pub focused: bool,
    /// It cannot be used now.
    pub disabled: bool,
    /// For something that is on or off, whether it is on.
    pub checked: Option<bool>,
    /// Its value is text that a person can change by typing, now.
    pub editable: bool,
    /// For something that opens a menu (a combo box, an item with a
    /// submenu), whether the menu is open.
    pub expanded: Option<bool>,
    /// For something that can be selected, such as a row of a list,
    /// whether it is.
    pub selected: Option<bool>,
}

/// Which [`Action`]s a widget accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Actions {
    /// It can take the keyboard focus.
    pub focus: bool,
    /// It can be pressed or toggled.
    pub press: bool,
    /// Its value can be raised by one step.
    pub increment: bool,
    /// Its value can be lowered by one step.
    pub decrement: bool,
    /// Its value can be set to a number.
    pub set_value: bool,
}

impl Actions {
    /// No action.
    pub const NONE: Actions = Actions {
        focus: false,
        press: false,
        increment: false,
        decrement: false,
        set_value: false,
    };

    /// What a control with a value accepts: focus, increment, decrement and
    /// set value.
    pub const ADJUST: Actions = Actions {
        focus: true,
        press: false,
        increment: true,
        decrement: true,
        set_value: true,
    };

    /// What something pressed accepts: focus and press.
    pub const PRESS: Actions = Actions {
        focus: true,
        press: true,
        increment: false,
        decrement: false,
        set_value: false,
    };

    /// Whether `action` is one of these.
    pub fn allows(&self, action: Action) -> bool {
        match action {
            Action::Focus => self.focus,
            Action::Press => self.press,
            Action::Increment => self.increment,
            Action::Decrement => self.decrement,
            Action::SetValue(_) => self.set_value,
        }
    }
}

/// Something a screen reader, or a test, asks a widget to do.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    /// Take the keyboard focus.
    Focus,
    /// Press, or toggle.
    Press,
    /// Raise the value by one step.
    Increment,
    /// Lower the value by one step.
    Decrement,
    /// Set the value to this number, in the units of
    /// [`AccessNode::numeric`].
    SetValue(f64),
}

/// How a widget describes itself.
#[derive(Debug, Clone, PartialEq)]
pub struct AccessNode {
    /// What it is.
    pub role: Role,
    /// What it is called, such as a parameter's name. Tests find widgets
    /// by role and name.
    pub name: String,
    /// Its value as a person reads it, such as "-6.0 dB".
    pub value: Option<String>,
    /// Its value as a number, in the units of [`range`](Self::range).
    pub numeric: Option<f64>,
    /// The range of [`numeric`](Self::numeric).
    pub range: Option<AccessRange>,
    /// What state it is in.
    pub state: AccessState,
    /// What it can be asked to do.
    pub actions: Actions,
    /// What it is for, in a sentence, as help text: what a tooltip shows
    /// (see [`IntoView::tooltip`](crate::IntoView::tooltip)).
    pub description: Option<String>,
}

impl AccessNode {
    /// A node with `role` and `name`, no value, the default state and no
    /// actions.
    pub fn new(role: Role, name: impl Into<String>) -> Self {
        AccessNode {
            role,
            name: name.into(),
            value: None,
            numeric: None,
            range: None,
            state: AccessState::default(),
            actions: Actions::NONE,
            description: None,
        }
    }

    /// The same node with `description` as its help text.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// The same node with its value as text.
    #[must_use]
    pub fn with_value(mut self, value: impl Into<String>) -> Self {
        self.value = Some(value.into());
        self
    }

    /// The same node with its value as a number within `range`.
    #[must_use]
    pub fn with_numeric(mut self, value: f64, range: AccessRange) -> Self {
        self.numeric = Some(value);
        self.range = Some(range);
        self
    }

    /// The same node, on or off.
    #[must_use]
    pub fn with_checked(mut self, checked: bool) -> Self {
        self.state.checked = Some(checked);
        self
    }

    /// The same node, whose value is text a person can type into now.
    #[must_use]
    pub fn editable(mut self) -> Self {
        self.state.editable = true;
        self
    }

    /// The same node, with the menu it opens open or not.
    #[must_use]
    pub fn with_expanded(mut self, expanded: bool) -> Self {
        self.state.expanded = Some(expanded);
        self
    }

    /// The same node, selected or not.
    #[must_use]
    pub fn with_selected(mut self, selected: bool) -> Self {
        self.state.selected = Some(selected);
        self
    }

    /// The same node, which cannot be used now.
    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.state.disabled = true;
        self
    }

    /// The same node, accepting `actions`.
    #[must_use]
    pub fn with_actions(mut self, actions: Actions) -> Self {
        self.actions = actions;
        self
    }
}

/// A part of a widget that assistive technology sees as a node of its own
/// although it is not a widget: a row of a list, of which only the rows
/// shown exist (see [`Widget::access_items`](crate::Widget::access_items)).
#[derive(Debug, Clone, PartialEq)]
pub struct AccessItem {
    /// What the widget calls it, such as the row's index. The widget is
    /// asked for actions on it by this key.
    pub key: usize,
    /// How it describes itself.
    pub node: AccessNode,
    /// Where it is, in logical pixels.
    pub bounds: Rect,
}

impl AccessItem {
    /// An item `key`, described by `node`, at `bounds`.
    pub fn new(key: usize, node: AccessNode, bounds: Rect) -> Self {
        AccessItem { key, node, bounds }
    }
}

/// One widget, or one item of a widget, in the accessibility tree of a
/// [`Root`](crate::Root).
#[derive(Debug, Clone, PartialEq)]
pub struct AccessEntry {
    /// The widget, or for an item the widget it belongs to.
    pub id: NodeId,
    /// For an item ([`AccessItem`]), its key; `None` for the widget
    /// itself.
    pub item: Option<usize>,
    /// The widget that contains it, if any: for an item, its widget.
    pub parent: Option<NodeId>,
    /// How it describes itself, with the state the root knows filled in.
    pub node: AccessNode,
    /// Where it is, in logical pixels.
    pub bounds: Rect,
    /// Where it is, in device pixels.
    pub device: IRect,
}

/// The contrast WCAG 2 asks of text at level AA: 4.5 to 1.
pub const MIN_TEXT_CONTRAST: f64 = 4.5;

/// The contrast ratio of `text` drawn on `background`, from 1 (none) to 21
/// (black on white), as WCAG 2 defines it: the lighter colour's relative
/// luminance plus 0.05, over the darker's plus 0.05.
///
/// `text` is first blended over `background` by its alpha, as it is drawn;
/// `background` counts as opaque.
///
/// ```
/// use manguebyte_gfx::Color;
/// use manguebyte_ui::access::contrast_ratio;
///
/// assert!((contrast_ratio(Color::hex(0x000000), Color::hex(0xFFFFFF)) - 21.0).abs() < 1e-9);
/// assert_eq!(contrast_ratio(Color::hex(0x777777), Color::hex(0x777777)), 1.0);
/// // Text at no opacity is the background itself.
/// assert_eq!(contrast_ratio(Color::hex(0x000000).with_alpha(0), Color::hex(0xFFFFFF)), 1.0);
/// ```
pub fn contrast_ratio(text: Color, background: Color) -> f64 {
    let alpha = f64::from(text.a) / 255.0;
    let blend = |over: u8, under: u8| f64::from(over) * alpha + f64::from(under) * (1.0 - alpha);
    let luminance = |r: f64, g: f64, b: f64| {
        // The sRGB transfer function, on channels from 0 to 255.
        let linear = |channel: f64| {
            let v = channel / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
    };
    let front = luminance(
        blend(text.r, background.r),
        blend(text.g, background.g),
        blend(text.b, background.b),
    );
    let back = luminance(
        f64::from(background.r),
        f64::from(background.g),
        f64::from(background.b),
    );
    (front.max(back) + 0.05) / (front.min(back) + 0.05)
}

/// The colour selected text is drawn on in `theme`: its accent, at a
/// third, over the field.
pub(crate) fn selection_color(theme: &Theme) -> Color {
    theme.palette.accent.with_alpha(0x55)
}

/// The colour behind the highlighted item of a menu in `theme`: the
/// accent's tint over the menu's surface.
pub(crate) fn menu_highlight(theme: &Theme) -> Color {
    blend(theme.palette.accent_soft, theme.palette.overlay)
}

/// `over`, drawn at its alpha on the opaque `under`.
fn blend(over: Color, under: Color) -> Color {
    let alpha = u32::from(over.a);
    let mix =
        |a: u8, b: u8| ((u32::from(a) * alpha + u32::from(b) * (255 - alpha) + 127) / 255) as u8;
    Color::rgb(
        mix(over.r, under.r),
        mix(over.g, under.g),
        mix(over.b, under.b),
    )
}

/// The first pair of the theme's text and background colours whose
/// contrast is below [`MIN_TEXT_CONTRAST`], if any: each of the three
/// levels of text on the background, the surface of cards and the raised
/// faces of controls; text and secondary text on the overlay of menus and
/// tooltips; text on a menu's highlighted item (the accent's tint over the
/// overlay), on a selected row (the same tint over a card) and on the
/// selection of a text input; and each categorical hue's text on its fill,
/// over a card.
fn contrast_problem(theme: &Theme) -> Option<String> {
    let palette = &theme.palette;
    let selection = blend(selection_color(theme), palette.raised);
    let selected_row = blend(palette.accent_soft, palette.surface);
    let mut pairs = vec![
        ("text", palette.text, "background", palette.background),
        ("text", palette.text, "surface", palette.surface),
        ("text", palette.text, "raised", palette.raised),
        ("text", palette.text, "overlay", palette.overlay),
        (
            "text_secondary",
            palette.text_secondary,
            "background",
            palette.background,
        ),
        (
            "text_secondary",
            palette.text_secondary,
            "surface",
            palette.surface,
        ),
        (
            "text_secondary",
            palette.text_secondary,
            "raised",
            palette.raised,
        ),
        (
            "text_secondary",
            palette.text_secondary,
            "overlay",
            palette.overlay,
        ),
        (
            "text_muted",
            palette.text_muted,
            "background",
            palette.background,
        ),
        ("text_muted", palette.text_muted, "surface", palette.surface),
        ("text_muted", palette.text_muted, "raised", palette.raised),
        (
            "text",
            palette.text,
            "menu highlight (accent tint over overlay)",
            menu_highlight(theme),
        ),
        (
            "text",
            palette.text,
            "selected row (accent tint over surface)",
            selected_row,
        ),
        (
            "text",
            palette.text,
            "selection (accent over raised)",
            selection,
        ),
    ];
    for hue in &palette.hues {
        pairs.push((
            "categorical text",
            hue.text,
            "its fill over the surface",
            blend(hue.fill, palette.surface),
        ));
    }
    for (text_name, text, back_name, back) in pairs {
        let ratio = contrast_ratio(text, back);
        if ratio < MIN_TEXT_CONTRAST {
            return Some(format!(
                "the theme's {text_name} colour on its {back_name} colour has a contrast of \
                 {ratio:.2} to 1, below {MIN_TEXT_CONTRAST} to 1"
            ));
        }
    }
    None
}

/// The first rule an accessibility tree, drawn in `theme`, breaks, if any:
///
/// - every widget that takes the focus has a name;
/// - every widget that can be pressed or adjusted takes the focus, so the
///   keyboard reaches it;
/// - every slider has its value as text and as a number within a range,
///   and can be raised, lowered and set;
/// - every meter has its value as text;
/// - every switch says whether it is on;
/// - every text input has its text as its value and takes the focus;
/// - every combo box has its value as text, can be pressed to open, and
///   says whether it is open;
/// - every list and scroll view has a name, and every list item says
///   whether it is selected;
/// - no two widgets with a name share their role and name within the same
///   named group (groups with no name only arrange their children, and a
///   screen reader does not tell them apart);
/// - the theme's text colours have a contrast of at least
///   [`MIN_TEXT_CONTRAST`] on its background and surface colours, and its
///   text colour on its track colour (a menu's highlighted item) and on the
///   selection of a text input.
pub(crate) fn first_problem(entries: &[AccessEntry], theme: &Theme) -> Option<String> {
    for entry in entries {
        let node = &entry.node;
        let actions = node.actions;
        if actions.focus && node.name.trim().is_empty() {
            return Some(format!(
                "{:?} {:?} takes the focus but has no name",
                node.role, entry.id
            ));
        }
        let usable = actions.press || actions.increment || actions.decrement || actions.set_value;
        if usable && !actions.focus {
            return Some(format!(
                "{:?} {:?} can be used but cannot take the focus, so the keyboard cannot reach it",
                node.role, node.name
            ));
        }
        if matches!(node.role, Role::Slider | Role::Meter) && node.value.is_none() {
            return Some(format!(
                "{:?} {:?} has no value as text",
                node.role, node.name
            ));
        }
        if node.role == Role::Slider {
            if node.numeric.is_none() || node.range.is_none() {
                return Some(format!(
                    "slider {:?} has no value as a number within a range",
                    node.name
                ));
            }
            if !(actions.increment && actions.decrement && actions.set_value) {
                return Some(format!(
                    "slider {:?} cannot be raised, lowered and set by assistive technology",
                    node.name
                ));
            }
        }
        if node.role == Role::Switch && node.state.checked.is_none() {
            return Some(format!(
                "switch {:?} does not say whether it is on",
                node.name
            ));
        }
        if matches!(node.role, Role::TextInput | Role::ComboBox) && node.value.is_none() {
            return Some(format!(
                "{:?} {:?} has no value as text",
                node.role, node.name
            ));
        }
        if node.role == Role::ComboBox && !(actions.press && node.state.expanded.is_some()) {
            return Some(format!(
                "combo box {:?} cannot be opened by assistive technology, or does not say \
                 whether it is open",
                node.name
            ));
        }
        if node.role == Role::TextInput && !actions.focus {
            return Some(format!(
                "text input {:?} cannot take the focus, so nothing can be typed into it",
                node.name
            ));
        }
        if node.role == Role::ListItem && node.state.selected.is_none() {
            return Some(format!(
                "list item {:?} does not say whether it is selected",
                node.name
            ));
        }
        if matches!(node.role, Role::List | Role::ScrollView) && node.name.trim().is_empty() {
            return Some(format!(
                "{:?} {:?} has no name, so a screen reader cannot say what it shows",
                node.role, entry.id
            ));
        }
    }
    if let Some(problem) = shared_names(entries) {
        return Some(problem);
    }
    contrast_problem(theme)
}

/// Two widgets with the same role and name in the same named group, if
/// there are any.
fn shared_names(entries: &[AccessEntry]) -> Option<String> {
    // The nearest ancestor with a name, as an index into `entries`, for
    // each entry. Parents come before their children.
    let mut group: Vec<Option<usize>> = Vec::with_capacity(entries.len());
    for entry in entries {
        let parent = entry.parent.and_then(|parent| {
            entries
                .iter()
                .position(|other| other.id == parent && other.item.is_none())
        });
        let nearest = parent.and_then(|parent| {
            if entries[parent].node.name.trim().is_empty() {
                group.get(parent).copied().flatten()
            } else {
                Some(parent)
            }
        });
        group.push(nearest);
    }
    for (index, entry) in entries.iter().enumerate() {
        let node = &entry.node;
        if node.name.trim().is_empty() {
            continue;
        }
        let twin = entries[..index].iter().enumerate().any(|(other, earlier)| {
            earlier.node.role == node.role
                && earlier.node.name == node.name
                && group[other] == group[index]
        });
        if twin {
            let place = match group[index] {
                Some(at) => format!("the group {:?}", entries[at].node.name),
                None => String::from("the interface"),
            };
            return Some(format!(
                "two {:?} widgets are named {:?} in {place}; assistive technology and tests cannot \
                 tell them apart",
                node.role, node.name
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_allow_only_what_they_list() {
        assert!(Actions::ADJUST.allows(Action::SetValue(0.5)));
        assert!(!Actions::ADJUST.allows(Action::Press));
        assert!(Actions::PRESS.allows(Action::Press));
        assert!(!Actions::NONE.allows(Action::Focus));
    }

    fn entry(id: usize, parent: Option<usize>, node: AccessNode) -> AccessEntry {
        AccessEntry {
            id: NodeId::from_index(id),
            item: None,
            parent: parent.map(NodeId::from_index),
            node,
            bounds: Rect::default(),
            device: IRect::default(),
        }
    }

    fn problem(entries: &[AccessEntry]) -> Option<String> {
        first_problem(entries, &Theme::dark())
    }

    #[test]
    fn problems_name_the_widget() {
        let unnamed = entry(
            3,
            None,
            AccessNode::new(Role::Button, " ").with_actions(Actions::PRESS),
        );
        assert!(problem(&[unnamed]).is_some_and(|p| p.contains("no name")));
        let silent = entry(3, None, AccessNode::new(Role::Meter, "Level"));
        assert!(problem(&[silent]).is_some_and(|p| p.contains("Level")));
        let fine = entry(
            3,
            None,
            AccessNode::new(Role::Switch, "Bypass").with_checked(false),
        );
        assert_eq!(problem(&[fine]), None);
    }

    #[test]
    fn a_list_and_a_scroll_view_have_names_and_rows_say_whether_they_are_selected() {
        let list = entry(0, None, AccessNode::new(Role::List, ""));
        assert!(problem(&[list]).is_some_and(|p| p.contains("no name")));
        let view = entry(0, None, AccessNode::new(Role::ScrollView, " "));
        assert!(problem(&[view]).is_some_and(|p| p.contains("no name")));
        let list = entry(0, None, AccessNode::new(Role::List, "Samples"));
        let mut row = entry(0, Some(0), AccessNode::new(Role::ListItem, "Kick"));
        row.item = Some(3);
        assert!(
            problem(&[list.clone(), row.clone()])
                .is_some_and(|p| p.contains("\"Kick\"") && p.contains("selected"))
        );
        row.node = row.node.with_selected(false);
        assert_eq!(problem(&[list.clone(), row.clone()]), None);
        // Two rows of one list with the same name cannot be told apart; in
        // two lists they can.
        let mut twin = row.clone();
        twin.item = Some(4);
        assert!(problem(&[list.clone(), row.clone(), twin.clone()]).is_some());
        let other = entry(1, None, AccessNode::new(Role::List, "Takes"));
        twin.id = NodeId::from_index(1);
        twin.parent = Some(NodeId::from_index(1));
        assert_eq!(problem(&[list, row, other, twin]), None);
    }

    #[test]
    fn what_can_be_used_can_be_reached_by_the_keyboard() {
        let mut press = Actions::PRESS;
        press.focus = false;
        let button = entry(
            0,
            None,
            AccessNode::new(Role::Button, "Save").with_actions(press),
        );
        assert!(problem(&[button]).is_some_and(|p| p.contains("cannot take the focus")));
    }

    #[test]
    fn a_slider_has_a_number_a_range_and_its_actions() {
        let slider = |node: AccessNode| [entry(0, None, node)];
        let bare = AccessNode::new(Role::Slider, "Gain").with_value("0 dB");
        assert!(problem(&slider(bare.clone())).is_some_and(|p| p.contains("number")));
        let numeric = bare.with_numeric(0.0, AccessRange::new(-24.0, 24.0, None));
        let mut read_only = Actions::ADJUST;
        read_only.set_value = false;
        let stuck = numeric.clone().with_actions(read_only);
        assert!(problem(&slider(stuck)).is_some_and(|p| p.contains("raised, lowered and set")));
        assert_eq!(
            problem(&slider(numeric.with_actions(Actions::ADJUST))),
            None
        );
    }

    #[test]
    fn names_are_told_apart_within_each_named_group() {
        let group = |id, parent, name: &str| entry(id, parent, AccessNode::new(Role::Group, name));
        let meter = |id, parent| {
            entry(
                id,
                Some(parent),
                AccessNode::new(Role::Meter, "Level").with_value("0"),
            )
        };
        // In two named groups: told apart by the groups.
        let apart = [
            group(0, None, ""),
            group(1, Some(0), "Input"),
            meter(2, 1),
            group(3, Some(0), "Output"),
            meter(4, 3),
        ];
        assert_eq!(problem(&apart), None);
        // In two groups with no name, inside one named group: not.
        let together = [
            group(0, None, "Levels"),
            group(1, Some(0), ""),
            meter(2, 1),
            group(3, Some(0), ""),
            meter(4, 3),
        ];
        assert!(
            problem(&together).is_some_and(|p| p.contains("\"Level\"") && p.contains("\"Levels\""))
        );
        // Groups with no name never clash with each other.
        let rows = [
            group(0, None, ""),
            group(1, Some(0), ""),
            group(2, Some(0), ""),
        ];
        assert_eq!(problem(&rows), None);
    }

    #[test]
    fn the_built_in_themes_have_enough_contrast_and_a_bad_one_does_not() {
        for theme in [Theme::dark(), Theme::light()] {
            assert_eq!(contrast_problem(&theme), None);
        }
        let mut faint = Theme::light();
        faint.palette.text_muted = Color::hex(0xA4A197);
        let found = contrast_problem(&faint).expect("too faint");
        assert!(
            found.contains("text_muted") && found.contains("background"),
            "{found}"
        );
        // A categorical hue whose text does not stand out from its fill.
        let mut washed = Theme::dark();
        washed.palette.hues[1].text = washed.palette.surface;
        let found = contrast_problem(&washed).expect("washed out");
        assert!(found.contains("categorical"), "{found}");
        // Half-transparent text is judged as it is drawn.
        let mut veiled = Theme::dark();
        veiled.palette.text = veiled.palette.text.with_alpha(64);
        assert!(contrast_problem(&veiled).is_some());
    }
}
