# ADR 0013: Pages that hide, and dialogs inside the window

Status: accepted, 2026-10-30

## Context

The two products being built on the interface switch what part of a
window shows. Parabolic, the plugin generator, has Synth, Effects,
Presets and Drums pages; Caecilia 2.0, an organ console, shows one
division at a time. Both also need to ask before something that cannot
be undone ("delete the preset?").

ADR 0007 built the tree once from a view, in one arena in tree order,
and polled every widget on every frame. Nothing in it could take part of
the tree out of sight. Rebuilding the window on every switch would lose
every widget's state (a scroll offset, a selection, a text being typed)
and cost a whole layout and repaint; keeping every page drawn and polled
would make a window of five pages cost five times one, every frame, even
idle (ADR 0011 made idle cost a rule). A dialog inside the window needs
the opposite: part of the tree that takes every input while the rest
takes none.

JUCE answers these with `TabbedComponent`, which makes the shown page's
component visible and the others invisible, and with `AlertWindow` and
`DialogWindow`, entered modally with a callback for the answer (from its
public documentation). The design below is our own.

## Decision

**1. A container can show only some of its children.** A widget asks in
its `update` with `UpdateCx::show(Shown::Only(i))` or `Shown::All`. The
root keeps, per node, which children it shows and a `hidden` mark for
everything inside the others, worked out again only for that subtree
when it changes. A hidden widget keeps its place in the arena and all
its state, but the frame skips its subtree when it polls (the loop jumps
to the subtree's end), layout is handed only the children shown (the
layout tree's child list is rewritten, so hidden pages are not measured
or placed at all), and painting, hit testing, focus order, the
accessibility tree (`Root::access` returns `None`) and actions pass it
by. When a subtree is hidden its scenes are dropped and what they covered
is drawn again from what is under it, as when a menu closes; shown again,
it is laid out and drawn afresh. A widget hidden while it holds the
focus, the pointer or the hover loses them.

So a switch builds nothing, keeps every page's state, damages only the
bar and the content area, and an idle hidden page costs nothing: no
poll, no paint, no frames asked for. `tabs` and `sidebar` are built on
this, with the page chosen read from a `State<usize>` that the bar, the
keys and the application all set.

**2. A subtree can be modal.** `UpdateCx::set_modal(Some(child))` makes
a child's subtree the only part of the interface that the pointer hits,
that takes the focus (Tab goes round inside it), that assistive
technology can act on, and that keys go to: a key the focused widget
does not use goes to the subtree's outermost widget, and every key is
used. Menus close and a drag or a pointer held outside ends when it
starts; when it ends the focus goes back to the widget that had it. One
subtree is modal at a time.

**3. A dialog is a hidden child, not an overlay.** `with_dialog` puts a
host over a view with two children, the view and the dialog. Closed,
the host shows only the view; open, it shows both and makes the dialog
modal. The dialog's texts are `TextSource`s that live in its widgets the
whole time, so it follows the language open or closed, and costs
nothing closed. Its answer is a `DialogState` the application polls, as
ADR 0003 asks of every result; its buttons and keys set it, and the host
closes the dialog on the next frame.

**4. Control+Tab goes to containers.** A Control+Tab the focused widget
does not use is offered to the widgets containing it, innermost first,
and never moves the focus. Before, it moved the focus as Tab does.

## Alternatives not taken

- **Building pages when shown and dropping them when hidden.** It would
  lose every widget's state, or need every widget to save and restore
  it, and each switch would cost what opening the window costs.
- **Hiding with a flag the widget reads and drawing nothing.** The page
  would still be polled every frame and laid out, so five pages would
  cost five times one.
- **A dialog in the root's overlay, as menus are.** Menus are built when
  they open from a `Menu` that can be cloned; a dialog's texts follow the
  language and are not cloned, and the application must be able to open
  it from outside any widget. A hidden child keeps them, and reuses the
  hiding that pages need.
- **A callback with the answer.** ADR 0003: results are polled.

## Consequences

- Every page and every dialog is in memory for as long as the window is
  open; a hidden page's scenes are dropped, its widgets are not.
- The content area's natural size is the shown page's, so it should be
  given room (flex or a size) to stay steady across switches.
- What is left is in [TECH_DEBT](../TECH_DEBT.md) item 105.
