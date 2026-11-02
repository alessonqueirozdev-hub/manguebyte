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
over that scale, and there are no bars. At the boundary the room is continuous
with the design size, so it needs no margin.

### `responsive` decides during layout

`responsive(children).below(size)...` builds every child once and shows one
with `Shown::Only`. The question was how a widget learns the room it was given
when `Shown` was only asked for from `update`, which runs before layout with
the rectangle of the frame before: a window resized would draw one frame with
the old child in the new room, and the first frame would know no room at all.

So the frame gained a step. A node laid out as `Arrange::Adaptive` is asked
after each `layout::solve`, through `Widget::adapt(cx, room)`, which children
to show. A different answer hides and shows them, polls what came into sight
(it was not polled this frame, and tabs say which page they show when polled),
and solves again, all before any widget is placed or painted. **No frame shows
the wrong child**, the first frame included, which a test proves by counting
paints. A node changes at most once a frame, so the loop ends, after at most
one solve per adaptive node more. Solving again costs only when a switch
happens; a quiet frame does not lay out at all. Hidden children are not
measured, as with `Shown` from `update`.

### The margin is on the way down

A child comes in as soon as the room reaches its smallest size, and once
showing stays until the room falls `RESPONSIVE_MARGIN` (16 logical pixels, or
what `margin` says) below it. The other way round, needing the size plus the
margin to come in, is a trap with zoom: there the room is exactly the design
size whatever the window, and a first child whose smallest size is the design
size would never come back. The cost is that a child can be up to the margin
short of what it asked for; the docs say to allow for it.

Comparisons allow half a device pixel, as layout snaps edges to the nearest one
and a room of exactly a smallest size can come out a fraction short (at 0.7,
1602 logical pixels is 1121.4 device pixels, which snaps to 1121, or 1601.43
logical pixels). A first
decision has no margin, as nothing is showing to hold on to. A room with no
area decides nothing: a minimised window is not a small one.

### The natural size does not depend on the child showing

In a column, with no weight, a child is given its natural size. If that were
the child showing, a small layout would give itself too little room to ever
bring the large one back. `Arrange::Adaptive` is a stack whose content size is
a floor under its natural size, and `responsive` gives its first child's
smallest size there, so what a parent gives does not fall below what the first
child needs while there is room for it.

### Focus

A widget in a child that is hidden loses the focus, and the focus goes nowhere:
Tab then brings it to the first control of the child that came in. A text input
takes what was typed, as on any loss of focus. Moving the focus to some control
of the new layout would be a guess: nothing says which matches, and a screen
reader would announce a control the person did not go to. Gestures end, open
menus and tooltips close, and the pointer's hover is worked out again.

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
- Natural sizes that depend on the scale, such as a text's height rounded to device
  pixels, can differ by under a logical pixel between zooms, which a flexible
  neighbour absorbs. "No reflow" means the layout's size and arrangement, not every
  bit of every rectangle.
