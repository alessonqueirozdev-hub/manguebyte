//! Icons: small drawings in the colour of the text beside them.

use std::fmt;
use std::rc::Rc;

use manguebyte_gfx::{Color, Rect, Scene};

/// A small drawing that goes beside a text, such as a page's name in a
/// [`sidebar`](crate::sidebar) or an
/// [`action_button`](crate::action_button)'s label.
///
/// It is a function that draws into a scene, inside the square it is
/// given, in the colour it is given: the colour of the text beside it, so
/// that it follows the theme, the hover and the selection with the text.
/// It is called when the widget is drawn, never on its own. Cloning an
/// icon shares the function.
///
/// ```
/// use manguebyte_gfx::{Circle, Size, Stroke};
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{Icon, Role, State, label, sidebar};
///
/// let ring = Icon::new(|scene, square, color| {
///     let radius = square.width.min(square.height) * 0.4;
///     scene.stroke(Circle::new(square.center(), radius), Stroke::new(1.5), color);
/// });
/// let view = sidebar("Pages", State::new(0)).page("Record", label("Takes")).icon(ring);
/// let mut ui = Harness::new(view, Size::new(300.0, 120.0), 1.0);
/// ui.frame();
/// assert!(ui.dump_of(&ui.get(Role::TabList, "Pages")).contains("stroke circle("));
/// ```
#[derive(Clone)]
pub struct Icon {
    draw: Rc<Draw>,
}

/// What draws an icon: into a scene, inside a square, in a colour.
type Draw = dyn Fn(&mut Scene, Rect, Color);

impl Icon {
    /// An icon drawn by `draw`, given the scene, the square to draw in, in
    /// logical pixels, and the colour.
    pub fn new(draw: impl Fn(&mut Scene, Rect, Color) + 'static) -> Icon {
        Icon {
            draw: Rc::new(draw),
        }
    }

    /// Draws the icon into `scene`, inside `square`, in `color`.
    pub fn draw(&self, scene: &mut Scene, square: Rect, color: Color) {
        (self.draw)(scene, square, color);
    }
}

impl fmt::Debug for Icon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Icon").finish_non_exhaustive()
    }
}
