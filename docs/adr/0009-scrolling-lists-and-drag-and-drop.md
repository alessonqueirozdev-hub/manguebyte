# ADR 0009: Scrolling, virtual list rows and drag payloads in the interface

Status: accepted, 2026-10-29

## Context

ADR 0007 gave `manguebyte-ui` a retained tree in one arena, in tree order,
laid out by a pure function and drawn widget by widget into a back buffer,
with damage tracking that a partial repaint matches bit for bit. Phase 3 of
the roadmap left drag and drop, scrolling and lists not started. Parabolic,
the sampler and plugin generator planned on the framework, needs all three
at once: lists of samples and zones that can be long, panels that scroll,
rows dragged between lists and onto controls.

Three questions had to be answered within the rules ADR 0007 and ADR 0003
already set (no listeners, state polled, every widget describes itself,
the harness finds widgets only through that description):

- where a scroll offset lives, given that layout is a pure function and
  every widget draws in the interface's own coordinates;
- how a list of a hundred thousand rows reaches assistive technology, and
  the harness, without a hundred thousand widgets;
- what a drag carries, so that an interface can drag several kinds of
  things, and later accept files dropped from outside the window, without
  the widgets knowing each other's types.

JUCE answers these with `Viewport`, which moves a child component and
lets the operating system clip it, `ListBox`, which recycles a row
component per visible row, and `DragAndDropContainer`, whose description
of what is dragged is a `var` and a source component (from its public
documentation). The design below is our own.

## Decision

**1. Scrolling is layout plus the root.** A scroll view is a layout
arrangement, `Arrange::Scroll { x, y }`: its children are as long as they
want on the axes it scrolls, start at its viewport's corner, and its
content size is the room it keeps for its bars. Layout stays pure and
knows nothing of offsets. The root keeps a `ScrollMetrics` per scroll node
(offset, viewport, content), moves the node's subtree, which is contiguous
in the arena, by the offset in whole device pixels after layout, and gives
every node inside the clip of the viewports it is in. A widget wholly
outside its clip is not asked to draw and cannot be hit; one partly
outside is cut at the viewport, as a widget that reaches past its own
rectangle already was. Scrolling therefore costs a layout of the tree and
a repaint of what moved, damage stays inside the view, and the result is
an ordinary composite scene that a full repaint reproduces exactly. The
widget of a scroll node reads its metrics from its context and asks to
move with `scroll_to`; the root alone moves the offset for Tab and
assistive technology (the focused widget is revealed in each view it is
in, innermost first) and for a drag near an edge. Offsets are rounded to
device pixels so that scrolled content stays as sharp as the rest; finer
wheel steps are carried over, not lost.

**2. List rows are accessibility items, not widgets.** A widget may
describe parts of itself as `AccessItem`s (`Widget::access_items`, default
none), each with a key, a node and bounds; the root lists them after the
widget, inside it, and routes actions on them to `Widget::act_item`. The
list view describes, draws and asks its data for only the rows in sight,
so its cost does not grow with the number of rows. Rows of different
heights would need the same machinery with a height per row and are left
for later. The list keeps its offset in `f64`, because a hundred thousand
rows of 24 pixels reach past the precision of an `f32` pixel, and only
positions relative to the viewport, which are small, become `f32`.

**3. A drag carries an `Arc<dyn Any + Send + Sync>` and a description.**
`DragPayload::new(data, description)` holds data of any type the
application defines; a target asks for the type it takes with `get::<T>()`
and refuses the rest. `Send + Sync` costs an application nothing for the
data drags carry (indices, ids, paths) and lets a payload be kept in a
`ListState` or `State`, which an editor spec may keep between openings.
The description is what the ghost shows and what assistive technology
reads. Targets take part through one method, `Widget::drag`, with
`DragEvent::Over` (accept with `Handled::Yes`), `Left`, `Drop` and, for the
source, `Ended`; the event enum the window layer delivers is unchanged,
because a payload is not `Copy` and the window layer has no business
carrying one. The root runs the drag: it hides tooltips and blocks menus
while it runs, draws the ghost as a widget after the interface's own (as
menus and tooltips are), offers the payload on every move and every frame,
scrolls views whose edge the pointer is near, and cancels on Escape, a
release where nothing accepts, a second button, or the pointer or the
keyboard being taken away.

**The seam for drops from outside.** Inside the root a drag's source is
optional. Files dropped on the window will be a payload of a type the
framework defines (a list of paths), offered to targets through the same
`Over`, `Left` and `Drop`, with no source and no ghost of ours (the system
draws its own). Nothing else in the target side has to change. That work
is not part of this decision's first change.

## Alternatives not taken

- **Scrolling inside layout.** An offset in `Style` would make layout
  depend on state that changes sixty times a second and would put an `f32`
  in a type that is compared and hashed today. Moving a subtree after
  layout keeps layout's tests about sizes, and the root already owns where
  widgets are.
- **A scroll view that draws its child into a scene of its own, offset by
  a transform.** It would need the root to clip and hit-test through
  transforms, and damage would be the whole view whatever moved.
- **Copying the back buffer when scrolling (a blit) and drawing only the
  strip that comes into view.** It is what makes scrolling cheap in most
  toolkits, but our damage is a list of rectangles drawn from scenes, and
  a copy would break the rule that a partial repaint equals a full one
  unless the copy is proven exact at every scale. Left for later, with a
  measurement first.
- **A pool of row widgets in the tree, recycled as the list scrolls.**
  Widgets are identified by their place in the arena, so a pool would have
  a fixed size chosen before the list knows how tall it is, or would move
  every id after it when it grows.
- **Typed drags through a trait the payload implements**, with a method
  per question a target may ask. `Any` already answers "is it a `T`", and
  a trait would make every application implement it for a plain struct.
- **New variants of `Event` for drags.** `Event` is `Copy` and comes from
  the window layer; drags are the root's own.

## Consequences

- Any widget can be put in a scroll view and works there unchanged; any
  widget can be a drop target by overriding one method.
- A long list costs what its visible rows cost; assistive technology sees
  only the rows in sight, with their position ("12 of 100000"), as screen
  readers present virtualised lists.
- Scrolling lays the whole tree out again and repaints everything visible
  in the view. That should be cheap for interfaces of a few hundred
  widgets, but no benchmark measures scrolling yet.
- What is left is in [TECH_DEBT](../TECH_DEBT.md) item 96.
