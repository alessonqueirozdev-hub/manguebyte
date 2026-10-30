# ADR 0019: Floating panels inside the window

Status: accepted, 2026-10-30

## Context

Parabolic, the synth workstation built on the interface, shows its
twenty effects as a compact chain of pills, each a name and a light.
Clicking a pill opens that effect's panel over the screen; several can
be open at once, the person moves them about, and the project remembers
where each one was. The rest of the screen (the keyboard, the other
knobs) has to keep working while they are open. It must behave the same
in the standalone program and inside a plugin editor in a host, where a
window of the system's per panel would fight the host: focus taken from
the host's window, panels left behind when the editor moves or closes,
and a child window per effect for every instance.

ADR 0013 gave the root hidden children (`Shown`), a modal subtree and a
dialog inside the window. A dialog is the opposite of what is asked: it
covers the window with a scrim and takes every input. What was missing:

- a widget placed where the application says, not where its parent's
  layout puts it, and moved there cheaply while dragged;
- an order of drawing that is not the tree's, so that the panel clicked
  last is in front, without moving widgets in the arena (their ids are
  their positions);
- a way to give the focus back to whoever opened a panel, as the dialog
  does, without making the panel modal;
- Escape reaching the panel that has the focus, from any widget inside
  it.

JUCE answers with `DocumentWindow` and `ResizableWindow`, which are
windows of the system's (or components added to the desktop), and with
`CallOutBox`, a modal box that points at a component and is dismissed
by a click outside; `Component::toFront` reorders siblings (from its
public documentation). The design below is our own.

## Decision

**1. A widget can float.** `UpdateCx::float_at(position)` makes a widget
float at `position` from its parent's top-left corner, in logical
pixels. Layout is untouched: it places the widget as any child (a
floating panel aligns itself to the start of a stack, so it gets its
natural size). After layout the root moves the widget and its subtree,
which is contiguous in the arena, by whole device pixels to where it
asked, as near as keeps it inside its parent's rectangle, the same way
it moves a scrolled subtree (ADR 0009). Moving costs a layout of the
tree and a repaint of what moved: the panel's widgets are drawn again,
and the damage is where they were and where they are, shadow included.
Whole device pixels keep edges sharp and let a cached shadow be reused
(ADR 0011). Where it ended up is given to the widgets inside as
`FloatMetrics` (`EventCx::floating`): what a title bar reads to move its
panel and keep it inside.

**2. The root keeps a drawing order.** The arena stays in tree order.
Beside it the root keeps the floating widgets, back to front, and from
them a drawing order of the arena in runs, in which each node's floating
children come after its other children, in the order they were raised.
It is worked out again only when a widget starts floating or is raised,
and is one run when nothing floats. The back buffer is composed in that
order and hit testing walks it backwards, so what is drawn in front is
what the pointer reaches. The focus order and the accessibility tree
stay in tree order. `UpdateCx::raise` brings a floating widget in front
of its floating siblings; a press anywhere inside one, and the focus
moving inside one, raise it. Raising draws the raised subtree again
where it now covers others, and nothing else.

**3. Focus goes in and comes back.** `UpdateCx::focus_within` gives the
focus to the first widget inside that takes it. For a floating widget
the root remembers who had the focus then, or else the widget pressed
last if it takes the focus (a pill takes no focus when clicked, and is
still the opener); when the floating widget next hides what is inside it
while the focus is inside or nowhere, that widget gets the focus back. A
press on a floating panel leaves the focus inside it, as a press on a
window's title bar does.

**4. Escape goes outwards.** An Escape that the focused widget does not
use is offered to the widgets containing it, innermost first, as
Control+Tab already was (ADR 0013). A panel closes on it; a widget that
uses Escape itself (a value being typed, a drag) keeps it, and with
nothing focused Escape is left to the host.

**5. The panel is built from these.** `floating_panel(title, open,
position, content)` and `IntoView::with_panels` put, over a view, a
stack holding the view and one floating slot per panel. The slot polls
the panel's `State<bool>` and `State<Point>`; closed, it hides its card
with `Shown`, so a closed panel costs one version comparison a frame and
nothing else. The card is a `Role::Region` named by its title, drawn on
the overlay surface with the large shadow, as menus are; its title bar
moves it and writes the position into the state, and its close button
and Escape set the open state to `false`.

## Alternatives not taken

- **A window of the system's per panel.** It is what JUCE's
  `DocumentWindow` is, and what the request ruled out: inside a host it
  steals the focus from the host's window, needs a parent for every
  system, and is left behind when the editor moves.
- **Panels in the root's overlay, as menus are.** Overlays are built
  when they open and dropped when they close; a panel's widgets have
  state (a knob being dragged, a list's scroll) that must live while it
  is closed, and its texts follow the language. Hidden children keep
  them, as for the dialog.
- **Moving the panel's nodes to the end of the arena when raised.**
  Ids are positions in the arena; moving a subtree would change the ids
  of everything after it, which the focus, the hover, menus and the
  accessibility tree hold.
- **A position in `Style`, for layout to place.** Layout is pure and
  compared and hashed; ADR 0009 kept scroll offsets out of it for the
  same reasons, and moving after layout reuses that work.
- **A modal panel.** The request is explicit that the keyboard and the
  rest of the screen keep working; the dialog is there for what must
  stop the window.

## Consequences

- Any widget can float and be raised; `with_panels` is one use of it.
- Hit testing and composing the back buffer walk the drawing order,
  runs of the arena; with nothing floating it is one run and costs what
  it did.
- Dragging a panel lays the whole tree out on each move, as scrolling
  does.
- What is left is in [TECH_DEBT](../TECH_DEBT.md) item 112.
