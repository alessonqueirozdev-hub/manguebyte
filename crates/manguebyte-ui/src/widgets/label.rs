//! Text.

use manguebyte_gfx::{Align, Color, Point, Scene, Size, TextLayout, TextStyle};

use crate::access::{AccessNode, Role};
use crate::source::{Source, State, Watch};
use crate::theme::Theme;
use crate::widget::{AccessCx, MeasureCx, PaintCx, UpdateCx, Widget};

/// Where a label's text comes from.
enum Text {
    Fixed(String),
    Watched(Watch<String>),
    Shared { state: State<String>, seen: u64 },
}

/// Which of the theme's text styles a label uses.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Label,
    Title,
    Value,
    Custom(TextStyle),
}

/// One line of text, cut with an ellipsis if it does not fit.
///
/// The text is fixed, or read once per frame from a [`Source`] or a
/// [`State`]; a change lays the interface out again, since the text's
/// width may have changed. It is drawn in the theme's label style unless
/// told otherwise, centred vertically in the label's rectangle.
pub struct Label {
    text: Text,
    kind: Kind,
    muted: bool,
    color: Option<Color>,
    align: Align,
    layout: Option<(f32, TextLayout)>,
}

/// A label showing `text`.
///
/// ```
/// use std::cell::Cell;
/// use std::rc::Rc;
/// use manguebyte_gfx::Size;
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{IntoView, Label, Role, column, label};
///
/// let presets = Rc::new(Cell::new(3_u32));
/// let count = Rc::clone(&presets);
/// let view = column((
///     label("Presets").title(),
///     Label::dynamic(move || format!("{} saved", count.get())).muted(),
/// ));
/// let mut ui = Harness::new(view, Size::new(160.0, 50.0), 1.0);
/// ui.frame();
/// assert!(ui.find(Role::Label, "Presets").is_some());
/// assert!(ui.find(Role::Label, "3 saved").is_some());
///
/// // Read once per frame: a new text is laid out and drawn again.
/// presets.set(4);
/// assert!(ui.frame().laid_out);
/// assert!(ui.find(Role::Label, "4 saved").is_some());
/// ```
pub fn label(text: impl Into<String>) -> Label {
    Label::new(text)
}

impl Label {
    fn with_text(text: Text) -> Label {
        Label {
            text,
            kind: Kind::Label,
            muted: false,
            color: None,
            align: Align::Start,
            layout: None,
        }
    }

    /// A label showing `text`.
    pub fn new(text: impl Into<String>) -> Label {
        Label::with_text(Text::Fixed(text.into()))
    }

    /// A label showing what `source` gives, read once per frame. Reading
    /// it makes a `String` every frame; [`from_state`](Label::from_state)
    /// does not.
    pub fn dynamic(source: impl Source<String>) -> Label {
        Label::with_text(Text::Watched(Watch::new(source)))
    }

    /// A label showing the text in `state`, read again only when its
    /// version changes.
    pub fn from_state(state: State<String>) -> Label {
        Label::with_text(Text::Shared {
            seen: state.version(),
            state,
        })
    }

    /// The same label in the theme's title style.
    #[must_use]
    pub fn title(mut self) -> Label {
        self.kind = Kind::Title;
        self
    }

    /// The same label in the theme's value style.
    #[must_use]
    pub fn value_style(mut self) -> Label {
        self.kind = Kind::Value;
        self
    }

    /// The same label in `style`, whose colour is used unless
    /// [`color`](Label::color) says otherwise.
    #[must_use]
    pub fn style(mut self, style: TextStyle) -> Label {
        self.kind = Kind::Custom(style);
        self
    }

    /// The same label in the theme's muted text colour.
    #[must_use]
    pub fn muted(mut self) -> Label {
        self.muted = true;
        self
    }

    /// The same label in `color`.
    #[must_use]
    pub fn color(mut self, color: Color) -> Label {
        self.color = Some(color);
        self
    }

    /// The same label with its text placed by `align` across its
    /// rectangle.
    #[must_use]
    pub fn align(mut self, align: Align) -> Label {
        self.align = align;
        self
    }

    /// The text shown.
    pub fn text(&self) -> String {
        match &self.text {
            Text::Fixed(text) => text.clone(),
            Text::Watched(watch) => watch.get().cloned().unwrap_or_default(),
            Text::Shared { state, .. } => state.get(),
        }
    }

    fn style_in(&self, theme: &Theme) -> TextStyle {
        let typography = &theme.typography;
        let (style, own_color) = match self.kind {
            Kind::Label => (typography.label, false),
            Kind::Title => (typography.title, false),
            Kind::Value => (typography.value, false),
            Kind::Custom(style) => (style, true),
        };
        let color = match (self.color, self.muted, own_color) {
            (Some(color), _, _) => color,
            (None, true, _) => theme.palette.text_secondary,
            (None, false, true) => style.color,
            (None, false, false) => theme.palette.text,
        };
        style
            .with_color(color)
            .with_align(self.align)
            .with_max_lines(1)
    }
}

impl Widget for Label {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Label, self.text())
    }

    fn measure(&mut self, cx: &MeasureCx<'_>) -> Size {
        let style = self.style_in(cx.theme());
        let size = TextLayout::new(&self.text(), &style, None).size();
        // Rounded up, and a pixel more: layout snaps a widget's edges to
        // the nearest device pixels and may take up to a device pixel
        // from it, which would cut the text with an ellipsis. An empty
        // label still takes a line.
        let width = if size.width > 0.0 {
            size.width.ceil() + 1.0
        } else {
            0.0
        };
        Size::new(width, size.height.max(style.metrics().line_height))
    }

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        let changed = match &mut self.text {
            Text::Fixed(_) => false,
            Text::Watched(watch) => watch.poll(),
            Text::Shared { state, seen } => {
                let version = state.version();
                let changed = version != *seen;
                *seen = version;
                changed
            }
        };
        if changed {
            self.layout = None;
            cx.request_layout();
        }
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let rect = cx.rect();
        let style = self.style_in(cx.theme());
        if self
            .layout
            .as_ref()
            .is_none_or(|(width, layout)| *width != rect.width || *layout.style() != style)
        {
            let layout = TextLayout::new(&self.text(), &style, Some(rect.width));
            self.layout = Some((rect.width, layout));
        }
        if let Some((_, layout)) = &self.layout {
            let height = layout.size().height.max(style.metrics().line_height);
            let y = rect.y + (rect.height - height) * 0.5;
            scene.text(layout, Point::new(rect.x, y));
        }
    }
}
