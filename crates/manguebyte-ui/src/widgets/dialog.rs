//! A confirmation dialog inside the window: a scrim over the interface,
//! a card with a title, a message and two buttons, and all input while
//! it is open.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::rc::Rc;

use manguebyte_gfx::{Align as TextAlign, Color, Point, Scene, Size, TextLayout, TextStyle};

use super::action::{ActionButton, action_button};
use super::{floating, floating_overflow, line_height};
use crate::access::{AccessNode, Role};
use crate::event::{Event, Handled, Key, KeyEvent};
use crate::layout::{Align, Insets, Justify, Style};
use crate::motion::Animated;
use crate::source::{State, TextSource};
use crate::view::View;
use crate::widget::{AccessCx, EventCx, MeasureCx, PaintCx, Shown, UpdateCx, Widget};

/// How wide a dialog's text is laid out, at most.
const TEXT_WIDTH: f32 = 360.0;

/// The most lines of message shown; more are cut with an ellipsis.
const MESSAGE_LINES: u32 = 8;

/// Space between the title and the message.
const TITLE_GAP: f32 = 8.0;

/// Space around the card, inside the scrim.
const MARGIN: f32 = 24.0;

/// Space inside the card, around its contents.
const PADDING: f32 = 20.0;

/// How a dialog was answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DialogResult {
    /// Its confirming button, or Enter where that is the default.
    Confirmed,
    /// Its cancelling button, Escape, or Enter where cancelling is the
    /// default.
    Cancelled,
}

/// Where a dialog is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Closed,
    Open,
    Answered(DialogResult),
}

/// Whether a dialog is open, and how it was answered: shared by the
/// application and the dialog, and polled by both (ADR 0003: nothing is
/// called back).
///
/// The application [`open`](DialogState::open)s it, from a button's
/// presses it polls, a window's close request or anywhere else, or gives
/// it to a button with [`ActionButton::opens`]. The dialog shows on the
/// next frame; once answered it closes on the frame after, and the answer
/// waits in [`result`](DialogState::result) until the application
/// [`take_result`](DialogState::take_result)s it. Cloning shares it; it is
/// `Send` and `Sync`, so a plugin's editor spec can keep one.
#[derive(Clone)]
pub struct DialogState {
    phase: State<Phase>,
}

impl DialogState {
    /// A dialog that is closed and has no answer.
    pub fn new() -> DialogState {
        DialogState {
            phase: State::new(Phase::Closed),
        }
    }

    /// Opens the dialog, forgetting an answer not taken yet. Nothing if it
    /// is open already.
    pub fn open(&self) {
        if !self.is_open() {
            self.phase.set(Phase::Open);
        }
    }

    /// Whether the dialog is open, waiting for an answer.
    pub fn is_open(&self) -> bool {
        self.phase.get() == Phase::Open
    }

    /// Answers the dialog with `result`, as its buttons and keys do, which
    /// closes it on the next frame. Nothing if it is not open.
    pub fn answer(&self, result: DialogResult) {
        if self.is_open() {
            self.phase.set(Phase::Answered(result));
        }
    }

    /// How the dialog was last answered, if that answer has not been
    /// taken.
    pub fn result(&self) -> Option<DialogResult> {
        match self.phase.get() {
            Phase::Answered(result) => Some(result),
            Phase::Closed | Phase::Open => None,
        }
    }

    /// How the dialog was last answered, forgotten once taken.
    pub fn take_result(&self) -> Option<DialogResult> {
        let result = self.result();
        if result.is_some() {
            self.phase.set(Phase::Closed);
        }
        result
    }

    /// Counts up on every change, as [`State::version`] does.
    pub fn version(&self) -> u64 {
        self.phase.version()
    }
}

impl Default for DialogState {
    fn default() -> Self {
        DialogState::new()
    }
}

impl fmt::Debug for DialogState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DialogState")
            .field("phase", &self.phase.get())
            .finish()
    }
}

/// A dialog asking to confirm something: a title, a message, a confirming
/// and a cancelling button, shown inside the window over the view it is
/// put on with [`IntoView::with_dialog`](crate::IntoView::with_dialog). Every text is a [`TextSource`],
/// so the dialog follows the language, open or not.
///
/// While it is open the view under it is behind a scrim and takes no
/// input: the pointer reaches only the dialog, Tab goes round its two
/// buttons, assistive technology can act only inside it, and every key is
/// the dialog's (see [`UpdateCx::set_modal`](crate::UpdateCx::set_modal)).
/// It opens with the focus on its default button, the confirming one
/// unless [`cancel_by_default`](ConfirmDialog::cancel_by_default) says
/// otherwise. Escape cancels; Enter presses the button with the focus, or
/// the default one if neither has it. When it closes the focus goes back
/// to the widget that had it, usually the button that opened it.
///
/// For assistive technology it is a [`Dialog`](Role::Dialog) named by its
/// title, whose description is its message. Closed, it is hidden: it is
/// not polled, drawn or described, and costs nothing.
///
/// The answer is polled from the [`DialogState`], as other results in the
/// interface are; nothing is called back, and no audio thread is
/// involved.
///
/// ```
/// use manguebyte_gfx::Size;
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{
///     DialogResult, DialogState, IntoView, Key, Role, action_button, column, confirm_dialog,
/// };
///
/// let ask = DialogState::new();
/// let view = column((action_button("Delete preset").destructive().opens(&ask),))
///     .padding(12.0)
///     .with_dialog(
///         confirm_dialog(ask.clone(), "Delete the preset?", "It cannot be undone.")
///             .confirm_label("Delete")
///             .destructive(),
///     );
/// let mut ui = Harness::new(view, Size::new(500.0, 300.0), 1.0);
/// ui.frame();
/// assert!(ui.find(Role::Dialog, "Delete the preset?").is_none(), "closed");
///
/// ui.click(ui.get(Role::Button, "Delete preset").center());
/// ui.frame();
/// let dialog = ui.get(Role::Dialog, "Delete the preset?");
/// assert_eq!(dialog.node.description.as_deref(), Some("It cannot be undone."));
/// assert!(ui.get(Role::Button, "Delete").is_focused());
///
/// // Escape cancels, and the focus goes back to the button that opened it.
/// ui.key(Key::Escape);
/// assert_eq!(ask.take_result(), Some(DialogResult::Cancelled));
/// ui.frame();
/// assert!(ui.find(Role::Dialog, "Delete the preset?").is_none());
/// assert!(ui.get(Role::Button, "Delete preset").is_focused());
/// ```
pub struct ConfirmDialog {
    state: DialogState,
    title: TextSource,
    message: TextSource,
    confirm: TextSource,
    cancel: TextSource,
    destructive: bool,
    cancel_default: bool,
}

/// A dialog that asks `title`, explains `message`, and answers into
/// `state` (see [`ConfirmDialog`]). Its buttons say "OK" and "Cancel"
/// until given labels in the interface's language.
pub fn confirm_dialog(
    state: DialogState,
    title: impl Into<TextSource>,
    message: impl Into<TextSource>,
) -> ConfirmDialog {
    ConfirmDialog {
        state,
        title: title.into(),
        message: message.into(),
        confirm: TextSource::fixed("OK"),
        cancel: TextSource::fixed("Cancel"),
        destructive: false,
        cancel_default: false,
    }
}

impl ConfirmDialog {
    /// The same dialog with its confirming button labelled `label`.
    #[must_use]
    pub fn confirm_label(mut self, label: impl Into<TextSource>) -> ConfirmDialog {
        self.confirm = label.into();
        self
    }

    /// The same dialog with its cancelling button labelled `label`.
    #[must_use]
    pub fn cancel_label(mut self, label: impl Into<TextSource>) -> ConfirmDialog {
        self.cancel = label.into();
        self
    }

    /// The same dialog confirming something that destroys: its confirming
    /// button is drawn [`destructive`](ActionButton::destructive).
    #[must_use]
    pub fn destructive(mut self) -> ConfirmDialog {
        self.destructive = true;
        self
    }

    /// The same dialog opening with the focus on its cancelling button,
    /// which Enter then presses: for a question whose safe answer is no.
    #[must_use]
    pub fn cancel_by_default(mut self) -> ConfirmDialog {
        self.cancel_default = true;
        self
    }

    /// `content` with this dialog over it.
    pub(crate) fn over(self, content: View) -> View {
        let ConfirmDialog {
            state,
            title,
            message,
            confirm,
            cancel,
            destructive,
            cancel_default,
        } = self;
        let shared = Rc::new(Shared {
            title: RefCell::new(title.as_str().to_owned()),
            message: RefCell::new(message.as_str().to_owned()),
            version: Cell::new(0),
            fresh: Cell::new(false),
            focus_confirm: Rc::new(Cell::new(false)),
            focus_cancel: Rc::new(Cell::new(false)),
        });
        let answer = |result: DialogResult| {
            let state = state.clone();
            Box::new(move || state.answer(result)) as Box<dyn Fn()>
        };
        let mut confirm = action_button(confirm);
        confirm = if destructive {
            confirm.destructive()
        } else {
            confirm.primary()
        };
        confirm.hook = Some(answer(DialogResult::Confirmed));
        confirm.autofocus = Some(Rc::clone(&shared.focus_confirm));
        let mut cancel = action_button(cancel);
        cancel.hook = Some(answer(DialogResult::Cancelled));
        cancel.autofocus = Some(Rc::clone(&shared.focus_cancel));
        let buttons = View::with_children(
            super::Container::new(None),
            Style::row().with_spacing(8.0).with_justify(Justify::End),
            (cancel, confirm),
        );
        let card = View::with_children(
            Card,
            Style::column()
                .with_padding(Insets::all(PADDING))
                .with_spacing(18.0),
            (
                View::new(Texts {
                    shared: Rc::clone(&shared),
                    seen: 0,
                    layouts: None,
                }),
                buttons,
            ),
        );
        let layer = View::with_children(
            Layer {
                state: state.clone(),
                title,
                message,
                shared: Rc::clone(&shared),
                fade: Animated::new(0.0),
                cancel_default,
            },
            Style::stack()
                .with_align(Align::Center)
                .with_padding(Insets::all(MARGIN)),
            (card,),
        );
        View::with_children(
            Host {
                state,
                seen: None,
                open: false,
                shared,
                cancel_default,
            },
            Style::stack(),
            (content, layer),
        )
    }
}

impl fmt::Debug for ConfirmDialog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfirmDialog")
            .field("title", &self.title.as_str())
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

/// What the parts of a dialog share.
struct Shared {
    /// The title and message as last read, for the card to draw.
    title: RefCell<String>,
    message: RefCell<String>,
    /// Counts up when either changes.
    version: Cell<u64>,
    /// The dialog has just opened: the scrim fades in.
    fresh: Cell<bool>,
    /// Which button takes the focus when it opens.
    focus_confirm: Rc<Cell<bool>>,
    focus_cancel: Rc<Cell<bool>>,
}

/// The outermost widget: the content and, while open, the dialog over it.
struct Host {
    state: DialogState,
    seen: Option<u64>,
    open: bool,
    shared: Rc<Shared>,
    cancel_default: bool,
}

impl Widget for Host {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Group, "")
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        Size::default()
    }

    fn paint(&mut self, _cx: &PaintCx<'_>, _scene: &mut Scene) {}

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        let version = self.state.version();
        if self.seen == Some(version) {
            return;
        }
        let first = self.seen.is_none();
        self.seen = Some(version);
        let open = self.state.is_open();
        if open == self.open && !first {
            return;
        }
        self.open = open;
        if open {
            cx.show(Shown::All);
            cx.set_modal(Some(1));
            self.shared.fresh.set(true);
            let default = if self.cancel_default {
                &self.shared.focus_cancel
            } else {
                &self.shared.focus_confirm
            };
            default.set(true);
        } else {
            cx.show(Shown::Only(0));
            cx.set_modal(None);
        }
    }
}

/// The scrim over the content, which holds the card and has every key the
/// buttons do not use: the dialog for assistive technology.
struct Layer {
    state: DialogState,
    title: TextSource,
    message: TextSource,
    shared: Rc<Shared>,
    fade: Animated<f32>,
    cancel_default: bool,
}

impl Layer {
    /// The scrim's colour in a theme: dark over a dark theme, a veil of
    /// ink over a light one.
    fn scrim(theme: &crate::Theme) -> Color {
        if theme.elevation.glow.color.a > 0 {
            Color::BLACK.with_alpha(0x9E)
        } else {
            theme.palette.text.with_alpha(0x52)
        }
    }
}

impl Widget for Layer {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Dialog, self.title.as_str()).with_description(self.message.as_str())
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        Size::default()
    }

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        let title = self.title.poll();
        let message = self.message.poll();
        if title || message {
            if title {
                let mut kept = self.shared.title.borrow_mut();
                kept.clear();
                kept.push_str(self.title.as_str());
            }
            if message {
                let mut kept = self.shared.message.borrow_mut();
                kept.clear();
                kept.push_str(self.message.as_str());
            }
            self.shared
                .version
                .set(self.shared.version.get().wrapping_add(1));
            cx.request_access_update();
        }
        if self.shared.fresh.replace(false) {
            self.fade.jump(0.0);
            let (now, base) = (cx.now(), cx.motion().base());
            self.fade.transition(1.0, now, base);
            cx.request_paint();
        }
        cx.animate(&mut self.fade);
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let color = Layer::scrim(cx.theme()).with_opacity(self.fade.value());
        scene.fill(cx.rect(), color);
    }

    fn event(&mut self, _cx: &mut EventCx<'_>, event: &Event) -> Handled {
        match *event {
            Event::Key(KeyEvent {
                key: Key::Escape,
                pressed: true,
                ..
            }) => {
                self.state.answer(DialogResult::Cancelled);
                Handled::Yes
            }
            Event::Key(KeyEvent {
                key: Key::Enter,
                pressed: true,
                ..
            }) => {
                self.state.answer(if self.cancel_default {
                    DialogResult::Cancelled
                } else {
                    DialogResult::Confirmed
                });
                Handled::Yes
            }
            // The scrim takes the pointer, so nothing under it hears it; a
            // press on it does not answer.
            Event::PointerDown { .. }
            | Event::PointerUp { .. }
            | Event::PointerMove { .. }
            | Event::Wheel { .. } => Handled::Yes,
            _ => Handled::No,
        }
    }
}

/// The card: a floating surface with the theme's large elevation.
struct Card;

impl Widget for Card {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Group, "")
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        Size::default()
    }

    fn overflow(&self, cx: &MeasureCx<'_>) -> f32 {
        floating_overflow(cx.theme())
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let theme = cx.theme();
        floating(scene, cx.rect(), theme.corners.xl, theme, cx.scale());
    }
}

/// The title and the message, drawn in the card.
struct Texts {
    shared: Rc<Shared>,
    seen: u64,
    /// The title and the message laid out, for a width.
    layouts: Option<(f32, TextLayout, TextLayout)>,
}

impl Texts {
    fn styles(theme: &crate::Theme) -> (TextStyle, TextStyle) {
        let title = theme
            .typography
            .title
            .with_color(theme.palette.text)
            .with_align(TextAlign::Start);
        let mut message = theme
            .typography
            .label
            .with_weight(400.0)
            .with_color(theme.palette.text_secondary)
            .with_align(TextAlign::Start);
        message.max_lines = Some(MESSAGE_LINES);
        (title, message)
    }

    fn lay_out(&self, theme: &crate::Theme, width: f32) -> (TextLayout, TextLayout) {
        let (title, message) = Texts::styles(theme);
        (
            TextLayout::new(&self.shared.title.borrow(), &title, Some(width)),
            TextLayout::new(&self.shared.message.borrow(), &message, Some(width)),
        )
    }
}

impl Widget for Texts {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Label, self.shared.message.borrow().as_str())
    }

    fn measure(&mut self, cx: &MeasureCx<'_>) -> Size {
        let (title, message) = self.lay_out(cx.theme(), TEXT_WIDTH);
        let (title_style, _) = Texts::styles(cx.theme());
        Size::new(
            TEXT_WIDTH,
            title.size().height.max(line_height(&title_style)) + TITLE_GAP + message.size().height,
        )
    }

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        let version = self.shared.version.get();
        if version != self.seen {
            self.seen = version;
            self.layouts = None;
            cx.request_layout();
        }
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let rect = cx.rect();
        let theme = cx.theme();
        let stale = self.layouts.as_ref().is_none_or(|(width, title, _)| {
            *width != rect.width || *title.style() != Texts::styles(theme).0
        });
        if stale {
            let (title, message) = self.lay_out(theme, rect.width);
            self.layouts = Some((rect.width, title, message));
        }
        if let Some((_, title, message)) = &self.layouts {
            let (title_style, _) = Texts::styles(theme);
            scene.text(title, Point::new(rect.x, rect.y));
            let below = title.size().height.max(line_height(&title_style)) + TITLE_GAP;
            scene.text(message, Point::new(rect.x, rect.y + below));
        }
    }
}

impl ActionButton {
    /// The same button opening `dialog` when pressed (see
    /// [`DialogState::open`]), as well as counting its presses if it does.
    #[must_use]
    pub fn opens(mut self, dialog: &DialogState) -> ActionButton {
        let dialog = dialog.clone();
        self.hook = Some(Box::new(move || dialog.open()));
        self
    }
}
