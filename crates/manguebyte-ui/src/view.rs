//! Views: how an interface is written down.
//!
//! A [`View`] is a recipe for part of the tree: a widget, how it is laid
//! out, and the views inside it. Views are plain values made by plain
//! functions; [`Root::new`](crate::Root::new) consumes one and builds the
//! tree from it once. There is no macro, no diffing and no reactive
//! runtime: what changes while the interface runs is read from
//! [`Source`](crate::Source)s and parameters.
//!
//! Any widget is a view through [`IntoView`], whose methods set how it is
//! laid out. Containers take their children as a tuple, an array or a
//! `Vec`.
//!
//! ```
//! use manguebyte_gfx::Size;
//! use manguebyte_ui::{column, label, row, IntoView};
//!
//! let view = column((
//!     label("Filter").title(),
//!     row((label("Low"), label("High").flex(1.0))).spacing(8.0),
//! ))
//! .padding(12.0)
//! .min_size(Size::new(200.0, 0.0));
//! # let _ = view;
//! ```

use manguebyte_gfx::{Color, Size};

use crate::layout::{Align, Arrange, Insets, Justify, Style};
use crate::widget::Widget;
use crate::widgets::Container;

/// A widget, how it is laid out, and the views inside it.
pub struct View {
    pub(crate) widget: Box<dyn Widget>,
    pub(crate) style: Style,
    pub(crate) children: Vec<View>,
    pub(crate) name: Option<String>,
    pub(crate) tooltip: Option<String>,
}

impl View {
    /// A view of `widget` with no children, laid out as a leaf.
    pub fn new(widget: impl Widget) -> View {
        View {
            widget: Box::new(widget),
            style: Style::leaf(),
            children: Vec::new(),
            name: None,
            tooltip: None,
        }
    }

    /// A view of `widget` holding `children`, laid out by `style`.
    pub fn with_children(widget: impl Widget, style: Style, children: impl Children) -> View {
        View {
            widget: Box::new(widget),
            style,
            children: children.into_children(),
            name: None,
            tooltip: None,
        }
    }

    /// How the view is laid out.
    pub fn style(&self) -> &Style {
        &self.style
    }

    /// The views inside it.
    pub fn children(&self) -> &[View] {
        &self.children
    }

    /// The same view laid out by `change(style)`.
    #[must_use]
    pub fn with_style(mut self, change: impl FnOnce(Style) -> Style) -> View {
        self.style = change(self.style);
        self
    }
}

/// Anything that can be put in an interface: every [`Widget`], and
/// [`View`] itself. The methods set how it is laid out (see
/// [`layout`](crate::layout)) and turn it into a view.
pub trait IntoView: Sized {
    /// The view.
    fn into_view(self) -> View;

    /// Space kept free inside its edges: a number for every side, or
    /// [`Insets`]. For a container it is space around its children; for a
    /// widget with no children, space around what the widget draws, which
    /// is added to its natural size. Either way the space is part of the
    /// view: a press there reaches it, and its bounds include it.
    ///
    /// ```
    /// use manguebyte_gfx::Size;
    /// use manguebyte_ui::layout::Align;
    /// use manguebyte_ui::testing::Harness;
    /// use manguebyte_ui::{IntoView, Role, label, row};
    ///
    /// let view = row((label("Gain").padding(10.0),)).align(Align::Start);
    /// let mut ui = Harness::new(view, Size::new(200.0, 60.0), 1.0);
    /// ui.frame();
    /// let gain = ui.get(Role::Label, "Gain");
    /// // The text, 15 high, with 10 above and below it.
    /// assert_eq!(gain.bounds.height, 35.0);
    /// assert!(gain.bounds.width > 20.0);
    /// ```
    fn padding(self, padding: impl Into<Insets>) -> View {
        let padding = padding.into();
        self.into_view().with_style(|s| s.with_padding(padding))
    }

    /// Space between its children, for a row or a column.
    fn spacing(self, spacing: f32) -> View {
        self.into_view().with_style(|s| s.with_spacing(spacing))
    }

    /// Its share of the room left in the row or column it is in.
    fn flex(self, weight: f32) -> View {
        self.into_view().with_style(|s| s.with_flex(weight))
    }

    /// The smallest it may be.
    fn min_size(self, min: Size) -> View {
        self.into_view().with_style(|s| s.with_min(min))
    }

    /// The largest it may be.
    fn max_size(self, max: Size) -> View {
        self.into_view().with_style(|s| s.with_max(max))
    }

    /// Exactly `width` wide.
    fn width(self, width: f32) -> View {
        self.into_view().with_style(|s| Style {
            min: Size::new(width, s.min.height),
            max: Size::new(width, s.max.height),
            ..s
        })
    }

    /// Exactly `height` tall.
    fn height(self, height: f32) -> View {
        self.into_view().with_style(|s| Style {
            min: Size::new(s.min.width, height),
            max: Size::new(s.max.width, height),
            ..s
        })
    }

    /// Where its children sit across its row or column, or in its stack.
    fn align(self, align: Align) -> View {
        self.into_view().with_style(|s| s.with_align(align))
    }

    /// Where it sits itself, whatever its container says.
    fn align_self(self, align: Align) -> View {
        self.into_view().with_style(|s| s.with_align_self(align))
    }

    /// Where room left over in its row or column goes.
    fn justify(self, justify: Justify) -> View {
        self.into_view().with_style(|s| s.with_justify(justify))
    }

    /// The name assistive technology and tests know it by, in place of
    /// the one it gives itself: a group's heading, or a knob's name where
    /// two parameters share one.
    fn named(self, name: impl Into<String>) -> View {
        let mut view = self.into_view();
        view.name = Some(name.into());
        view
    }

    /// Help text, shown in a tooltip once the pointer has rested on the
    /// view, or on a widget inside it, for
    /// [`TOOLTIP_DELAY`](crate::TOOLTIP_DELAY), and given to assistive
    /// technology as the view's description, shown or not.
    ///
    /// ```
    /// use std::time::Duration;
    /// use manguebyte_gfx::Size;
    /// use manguebyte_ui::testing::Harness;
    /// use manguebyte_ui::{IntoView, Role, TOOLTIP_DELAY, label};
    ///
    /// let view = label("Drive").tooltip("How hard the signal hits the clipper");
    /// let mut ui = Harness::new(view, Size::new(200.0, 80.0), 1.0);
    /// ui.frame();
    /// let drive = ui.get(Role::Label, "Drive");
    /// assert_eq!(
    ///     drive.node.description.as_deref(),
    ///     Some("How hard the signal hits the clipper")
    /// );
    /// ui.move_to(drive.center());
    /// ui.advance(TOOLTIP_DELAY + Duration::from_millis(20));
    /// assert!(ui.find(Role::Tooltip, "How hard the signal hits the clipper").is_some());
    /// ```
    fn tooltip(self, text: impl Into<String>) -> View {
        let mut view = self.into_view();
        view.tooltip = Some(text.into());
        view
    }

    /// This view with `dialog` over it: the dialog's scrim and card cover
    /// the view's rectangle while it is open, and nothing of the view can
    /// be used then (see [`ConfirmDialog`](crate::ConfirmDialog)). Put it
    /// on the outermost view for a dialog over the whole window. Closed,
    /// the dialog is hidden and costs nothing.
    fn with_dialog(self, dialog: crate::ConfirmDialog) -> View {
        dialog.over(self.into_view())
    }
}

impl<W: Widget> IntoView for W {
    fn into_view(self) -> View {
        View::new(self)
    }
}

impl IntoView for View {
    fn into_view(self) -> View {
        self
    }
}

impl From<f32> for Insets {
    fn from(all: f32) -> Self {
        Insets::all(all)
    }
}

/// The children of a container: a tuple of up to twelve views of any
/// kinds, or an array or `Vec` of one kind.
pub trait Children {
    /// The views, in order.
    fn into_children(self) -> Vec<View>;
}

impl<V: IntoView> Children for Vec<V> {
    fn into_children(self) -> Vec<View> {
        self.into_iter().map(IntoView::into_view).collect()
    }
}

impl<V: IntoView, const N: usize> Children for [V; N] {
    fn into_children(self) -> Vec<View> {
        self.into_iter().map(IntoView::into_view).collect()
    }
}

impl Children for () {
    fn into_children(self) -> Vec<View> {
        Vec::new()
    }
}

macro_rules! tuple_children {
    ($($name:ident)+) => {
        impl<$($name: IntoView),+> Children for ($($name,)+) {
            #[allow(non_snake_case)]
            fn into_children(self) -> Vec<View> {
                let ($($name,)+) = self;
                vec![$($name.into_view()),+]
            }
        }
    };
}

tuple_children!(A);
tuple_children!(A B);
tuple_children!(A B C);
tuple_children!(A B C D);
tuple_children!(A B C D E);
tuple_children!(A B C D E F);
tuple_children!(A B C D E F G);
tuple_children!(A B C D E F G H);
tuple_children!(A B C D E F G H I);
tuple_children!(A B C D E F G H I J);
tuple_children!(A B C D E F G H I J K);
tuple_children!(A B C D E F G H I J K L);

fn container(arrange: Arrange, fill: Option<Color>, children: impl Children) -> View {
    let style = Style {
        arrange,
        ..Style::leaf()
    };
    View::with_children(Container::new(fill), style, children)
}

/// Children left to right.
pub fn row(children: impl Children) -> View {
    container(Arrange::Row, None, children)
}

/// Children top to bottom.
pub fn column(children: impl Children) -> View {
    container(Arrange::Column, None, children)
}

/// Children on top of each other, the last one on top.
pub fn stack(children: impl Children) -> View {
    container(Arrange::Stack, None, children)
}

/// `child` on a panel: a card (see [`Container::panel`]) with no padding of
/// its own.
///
/// ```
/// use manguebyte_gfx::Size;
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{IntoView, Role, Theme, column, label, panel};
///
/// let view = panel(column((label("Filter").title(), label("Low pass"))).padding(8.0)).named("Filter");
/// let mut ui = Harness::new(view, Size::new(120.0, 60.0), 1.0);
/// ui.frame();
/// // A group named for assistive technology, drawn as a card: from the
/// // raised surface at the top to the surface at the bottom.
/// let group = ui.get(Role::Group, "Filter");
/// assert_eq!(group.bounds.width, 120.0);
/// let palette = Theme::dark().palette;
/// let top = ui.pixels().pixel(60, 1).expect("inside").to_array();
/// let bottom = ui.pixels().pixel(60, 58).expect("inside").to_array();
/// assert!(top[0].abs_diff(palette.raised.r) <= 2, "{top:?}");
/// assert!(bottom[0].abs_diff(palette.surface.r) <= 2, "{bottom:?}");
/// ```
pub fn panel(child: impl IntoView) -> View {
    View::with_children(Container::panel(), Style::stack(), (child,))
}

/// `child` on a card, with room around it: the card [`panel`] draws
/// (raised surface, card radius, hairline, the light along its top and a
/// soft shadow that reaches over what is around it), and 14 logical pixels
/// of padding.
///
/// ```
/// use manguebyte_gfx::Size;
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{IntoView, Role, card, label};
///
/// let view = card(label("Envelope").heading()).named("Envelope card").padding(20.0);
/// let mut ui = Harness::new(view, Size::new(200.0, 90.0), 1.0);
/// ui.frame();
/// let heading = ui.get(Role::Label, "Envelope");
/// assert!(heading.bounds.x >= 14.0);
/// // The card casts a shadow.
/// assert!(ui.dump().contains("shadow rounded_rect("));
/// ```
pub fn card(child: impl IntoView) -> View {
    View::with_children(
        Container::panel(),
        Style::stack().with_padding(Insets::all(14.0)),
        (child,),
    )
}

/// `child` on a rectangle of `color`.
pub fn filled(color: Color, child: impl IntoView) -> View {
    container(Arrange::Stack, Some(color), (child,))
}
