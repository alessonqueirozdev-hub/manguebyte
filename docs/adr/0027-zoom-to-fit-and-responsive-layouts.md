# ADR 0027: Zoom to fit and responsive layouts

Status: accepted, 2026-11-02

## Context

The Caecilia, a pipe organ on this framework, has a console designed at
1600 by 980 logical pixels with `Resize::Fixed`. On a 1080p screen at 125 %
the window gets about 840 logical pixels of height: the parts with a fixed
height keep it, the flexible middle collapses and controls are cut off. Its
user also wants the window snapped to half the screen beside a PDF of the
score, about 768 by 830 logical, where the keyboards should give way to a
miniature of each manual.

Reflowing does not serve a console: it has one good arrangement, drawn to
scale. Scaling everything does not serve half a screen either: at 0.48 the
stop names cannot be read. So two pieces are needed, which a program combines:

- an editor that keeps its layout at its design size and is drawn as large as
  the window allows, whatever the window, and goes back to reflowing below a
  size where the zoom would make it unreadable;
- a container that shows the first of several layouts that fits the room it
  is given, inside a window or a panel, without building anything again while
  a border is dragged and without a frame showing the wrong one.

## Decision

### Zoom is the root's scale, and it lives in the glue

`Editor::with_zoom_to_fit(min_zoom)` makes `manguebyte-gui`'s glue call
`Root::resize(design, system_scale * zoom)`, with zoom the smaller of the
window's size over the design's on each axis. The root knows nothing of zoom.
Since a root already draws, hits and describes itself in logical pixels at
any scale, everything follows from that one number: a window of 0.75 of the
design shows exactly what an editor opened at scale 0.75 shows, which a test
checks pixel for pixel. A window of the design size, however rounding left it
at a fractional scale, has no zoom at all, rather than a zoom of 0.999 that
softens everything.

The sum is a pure function (`zoom::fit`), tested on its own. The scale stays
within what a root draws (`MIN_SCALE..=MAX_SCALE`, system scale counted in):
past the largest the bars grow, and below the smallest it reflows as below
`min_zoom`.

### Bars are the window's, not the interface's

With room to spare on an axis the buffer goes in the middle of the window on
whole device pixels, and the rest shows the theme's background colour. The
buffer is the size of the design at the scale, so nothing in the renderer,
the damage or the widgets changes. Showing it is new API on the window:
`present_at(pixels, damage, origin)` and `present_color(color, rects)`, which
write into the window's existing back buffer. The glue remembers what the
window last showed around the buffer (`Letterbox`: the window's size, the
buffer's place and size, the colour) and shows everything again when it
changes. So a window that grows on the axis with room to spare draws the bars
and asks the interface for nothing; a quiet frame copies nothing.

Input arrives in device pixels (`PointerEvent::physical`), and the glue takes
off the origin and divides by the root's scale. A press or a wheel turn on the
bars does not reach the root at all, so it leaves the focus where it is: they
are not part of the interface. Two things do pass: a move, since a drag from
inside goes on over them with coordinates outside the layout, as a drag
outside a window does; and a press while a menu is open, since any press
outside a menu closes it.

`Root::set_origin` tells the root where its buffer is, only so that
`AccessEntry::device` counts from the window's corner. The text caret is added
up the same way in the glue.

### Below `min_zoom`, reflow in the room there is

The layout then gets the whole window at `min_zoom`: its size is the window's
over that scale, and there are no bars. For a window of the design's shape the
room is continuous at the boundary. For one with bars it is not: at a `min_zoom`
of 0.5, a window of 800 by 2000 lays out at 1600 by 980, and one of 799 by 2000 at
1598 by 4000, since the axis with room to spare gets all of it at once. That was
kept, and has no margin: the layout only gains room in that jump, and what it
shows is the window it has.

Past the largest zoom (a scale of 8, or the buffer's side of `MAX_BUFFER_SIDE`
device pixels, whichever comes first) the layout stays whole and the bars grow,
since a buffer that was cut would leave part of the layout undrawn. A window of
no area changes nothing that is remembered, as it says nothing about what fits.

A window of the design size, within the half a device pixel that rounding allows
at the system's scale, is treated just as an editor with no zoom treats it: the
scale is the system's and the size the window's own, so nothing is softened.

### `responsive` decides during layout

`responsive(children).below(size)...` builds every child once and shows one
with `Shown::Only`. The question was how a widget learns the room it was given
when `Shown` was only asked for from `update`, which runs before layout with
the rectangle of the frame before: a window resized would draw one frame with
the old child in the new room, and the first frame would know no room at all.

So the frame gained a step. Each time the interface is laid out, a node laid
out as `Arrange::Adaptive` is asked first, through `Widget::adapt(cx, room)`,
which children to show, in the room `layout::rooms` works out. A different
answer hides and shows them, polls what came into sight (it was not polled this
frame, and tabs say which page they show when polled), and works the rooms out
again; only then is the layout solved and placed. **No frame shows the wrong
child**, the first frame included, which a test proves by counting paints. A
node changes at most once a frame, so the loop ends. An interface with an
adaptive node works its layout out once more than one without; a quiet frame
does not lay out at all. Hidden children are not measured, as with `Shown` from
`update`.

The frame around it had to keep to this too. An application's window hears of a
new monitor as a scale and then a size. A frame between them laid out the old
size at the new scale, which is half or twice the room, so `responsive` switched
for one frame and focus, typed text and gestures went with it. So in an
application's window a new scale runs no frame by itself: the size's frame shows
both, and with no new size the next frame the timer runs does. In a host the
editor asks for the new size in the same call, and a host that grants it draws
one frame; a host that grants it later has, until then, a window that really is
that small ([TECH_DEBT](../TECH_DEBT.md) item 122).

### The margin is on the way down

A child comes in as soon as the room reaches its smallest size, and once
showing stays until the room falls `RESPONSIVE_MARGIN` (16 logical pixels, or
what `margin` says) below it. The other way round, needing the size plus the
margin to come in, is a trap with zoom: there the room is exactly the design
size whatever the window, and a first child whose smallest size is the design
size would never come back. The cost is that a child can be up to the margin
short of what it asked for; the docs say to allow for it.

A first decision has no margin, as nothing is showing to hold on to. A room with
no area decides nothing: a minimised window is not a small one.

### The room it chooses by does not depend on the child showing, or the scale

In a column, with no weight, a child is given its natural size. If that were
the child showing, a small layout would give itself too little room to ever
bring the large one back. A floor under the natural size is not enough either:
in a column too short for what is in it, each child gives up a share of what it
asks for, so a child asking for more gets more, and the choice flipped on every
layout (with a header of 100 in 580, and a floor of 500: a child of 100 leaves a
room of 483, which chooses the child of 800, which gets 515, which chooses back).

So the choice and the placing are two sums. `layout::rooms` works out the room
as if an adaptive node's natural size, on each axis with a floor, were that floor
(`responsive`'s is the largest smallest size), whichever child shows. Then
`layout::solve` places what was chosen by its natural size, with that floor under
it: a large child keeps its natural size, and a child can be placed in more room
than it was chosen by.

An axis with no floor keeps the child's natural size in the sum, or a second node
beside it would be given room that is not there. The choice cannot depend on a
floor's axis, so a node's choice does not depend on its own child. Two nodes could
still feed each other across axes: in a row too narrow for it, around a column
holding one node with a smallest width and one with a smallest height, each
changing the natural size of the axis the other chooses by. That is left: it needs
all of that together, and a node changes only once a frame in any case.

`rooms` snaps nothing to device pixels, so the choice does not depend on the scale.
A comparison against rooms that had been snapped would need a slack, and that slack
grows with what is around the node: two edges snapped apart make a room almost a
device pixel short, and a padding one more rounding. The comparisons here allow
only for adding up lengths in single precision (0.01 logical pixels).

### Focus

A widget in a child that is hidden loses the focus, and the focus goes nowhere:
Tab then brings it to the first control of the child that came in. A text input
takes what was typed, as on any loss of focus. Moving the focus to some control
of the new layout would be a guess: nothing says which matches, and a screen
reader would announce a control the person did not go to. Gestures end, open
menus and tooltips close, and the pointer's hover is worked out again, from where
it is: a pointer that has left the window, as it has in any drag of a border,
hovers nothing, and when a zoom moves the interface under a pointer that stays,
the glue says where the pointer now is (`Root::set_pointer`).

### A dialog in a child that is hidden

A modal subtree used to end when it was hidden, which was right: hiding it was how
its own dialog closed, and while it was modal nothing could hide it from outside
but the program. A layout that changes with the window is a new way out, and the
dialog came back drawn open and not modal, with Tab reaching what is under the
scrim. So the root tells the two apart. A modal subtree hidden by the widget that
made it modal ends. One hidden by a container around it is suspended, still open,
with where the focus was in it, and is modal again when it is shown, before it is
polled, so that one answered meanwhile closes as usual. Each layout can have its
own open dialog.

### How they combine: a breakpoint on the window

The Caecilia wants zoom on the full layout and the compact one at its real size.
`min_zoom` alone gives compact at that zoom. So
`Editor::with_real_size_below(window)` says that below that window the zoom is 1
and the layout gets the window. A `responsive` at the root with the same size
needs no knowledge of zoom: zoomed, its room is the design size and its first
child fits; below, its room is the window and its first child does not. The
window has the same rule as `responsive`, on the same numbers, so the two change
in the same frame:

```rust
let breakpoint = Size::new(1100.0, 700.0);
Editor::new(HostSize::new(1600, 980), move || {
    responsive((console(), summary())).below(breakpoint)
})
.with_resize(Resize::Free { min, max })
.with_zoom_to_fit(0.5)
.with_real_size_below(breakpoint)
```

## Alternatives not taken

- **Scaling in the scene, keeping the root at the window's size.** A transform
  around the composite would leave `node.rect`, hit testing and damage in other
  units than what is drawn, with each to be converted, and glyphs and hairlines
  rastered for the wrong scale. The root's scale already does this exactly.
- **A second window-sized pixmap for the letterbox**, with the glue copying the
  damage in. No change to the window crate, but the memory of a second buffer and
  a second copy of every damaged pixel, in an interface that already has two
  buffers.
- **A canvas window-sized root with an offset in the renderer.** Every damage
  rectangle and clip would have to carry the offset.
- **Telling the widget its room from `update`, a frame late.** That is the one
  wrong frame the task rules out, and the first frame would need a special case.
- **A measure pass with constraints going down**, as other toolkits have. It is
  what a responsive child really wants, but `layout` is two passes with natural
  sizes going up, and the rest of the interface has no need for it.
- **Zoom per child**, a container that scales what is inside it. It would serve a
  program that wants one panel zoomed and another not; but hit testing, damage,
  text at the right scale and the caret would all need nested scales, and the
  Caecilia needs only the window's. If a second program wants it, this design does
  not stand in the way.
- **Giving the focus to the matching control of the child that came in.** Nothing
  says two controls in different layouts are the same; a program that binds both to
  a parameter or a `State` keeps the value, which is what matters most.

## Consequences

- A program writes one line for zoom and one container for its layouts, and needs
  no resize code. Inside a host and in an application's window it is the same code.
- `Widget` has one more method, with a default, and `Arrange` one more variant,
  which a `match` outside the crate has to take in (before 1.0).
- Each change of zoom draws everything again, as any change of scale does: a border
  dragged across a large console redraws it each step. Nothing is built again, but
  it is not free ([TECH_DEBT](../TECH_DEBT.md) item 122).
- A plain `responsive` with no zoom gains the same: rooms that do not depend on the
  scale, and no switch between a monitor's scale and its size.
- Natural sizes that depend on the scale, such as a text's height rounded to device
  pixels, can differ by under a logical pixel between zooms, which a flexible
  neighbour absorbs. "No reflow" means the layout's size and arrangement, not every
  bit of every rectangle.
