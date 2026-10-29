# ADR 0011: Design tokens and motion

Status: accepted, 2026-10-29

## Context

The interface of ADR 0007 drew correctly and cheaply, and looked like the
1990s: flat hard rectangles, saturated "programmer" colours (pure orange,
blue and green), a waveform as a jagged line, muddy translucent zones, a
thick blue ring on every click, no depth and no motion. The project lead
asked for the most modern look there is and approved a direction from an
HTML mockup of the sampler screen: **A**, deep dark with a warm glow, as
the default, and **B**, light and editorial, as the light theme. One
accent, the orange "mangue", a few categorical hues for zones and
samples, semantic roles for good, close and over.

He was as explicit about the limit: the performance belongs to the audio,
not the design. It must be beautiful without taking the main role. Depth
and motion are the usual way an interface starts costing CPU when nothing
happens, redrawing the whole window to animate one control, or blurring
shadows every frame. Each of those had to be ruled out by a test or a
measurement, not by intent.

## Decision

**1. Tokens in layers, two themes.** `Theme` is a struct of public fields
grouped as tokens:

- `Palette`: four surfaces from the window up (`background`, `surface`
  for cards, `raised` for controls and pills, `overlay` for what floats),
  `hairline` and `hairline_strong` borders and the `highlight` along the
  top of raised surfaces; three levels of text (`text`,
  `text_secondary`, `text_muted`); one accent (`accent`, its bright end,
  `accent_soft` for tints, `on_accent` for text on it) and the soft
  `focus` ring; six categorical `Hue`s, each with its colour, a text tint,
  a fill and a ring; `success`, `warning`, `danger`; the faces of
  controls and the keys.
- `Elevation`: shadows `sm` (keys, thumbs), `md` (cards), `lg` (menus,
  tooltips) and the accent's `glow`, fully transparent in a theme without
  glow.
- `Corners` (`sm`, `md` for controls, `lg`, `xl` for cards, `full` for
  pills), `Spacing` (4, 8, 12, 16, 24), `Typography` (Inter at 400, 500
  and 600; label, value with tabular figures, title, heading, caption,
  chip), `Sizes` for controls, and `Motion`.

`Theme::dark()` is direction A and the default; `Theme::light()` is B.
`Root::set_theme` switches at run time. The colour ramps are designed in
OKLCH, where equal steps of lightness look equal, and stored as sRGB
constants with their OKLCH coordinates beside them: no colour science
runs in the interface. Dark surfaces step lightness by about 0.03 on one
cool hue; each text level keeps a contrast of at least 4.5 to 1 on every
surface it is drawn on, which the accessibility check now enforces for
the three levels, menus, the selected row, the text selection and each
hue's text on its fill. That forced one change from the mockup: its
faintest text (`#5d6373` in A, `#a4a197` in B) was below AA and is
lighter or darker here.

**2. Shadows and glows are scene primitives with a cached, integer
blur.** `Scene::shadow` and `Scene::stroke_shadow` record a `Shadow`
(offset, blur, spread, colour) of a filled shape or a stroked outline.
The renderer fills the outline into a mask in device pixels relative to a
whole-pixel corner, blurs it with three box passes each way (the SVG
filter specification's stand-in for a Gaussian, on whole numbers, so the
same bit for bit everywhere), and keeps it in a cache keyed by exactly
what it depends on: the outline relative to that corner, the box size
and the fill rule. Drawing it again, or the same shadow moved by whole
device pixels, only blends the mask. A shadow never draws beyond its
reach, which `Scene::bounds` includes, so damage built from bounds covers
it, and a partial render equals a whole one.

A widget whose shadow reaches past its rectangle says so with
`Widget::overflow`; the root clips it there instead of at its rectangle,
and draws that reach again when it changes. Widgets that should stay
inside their rectangle (knobs, sliders, switches) keep room for their
glow and thumb shadows instead.

**3. Motion is polled values on the frame clock.** `Animated<T>` holds a
start, a target, a start time, a duration and an easing (the standard
`cubic-bezier(.2,.8,.2,1)`, any cubic Bézier, or a damped spring); it is
set with the frame's time and ticked with a later frame's time. Nothing
runs on its own, nothing calls back, no timer is started: ADR 0003 holds.
The root eases a hover, a press and a focus-ring amount for every widget
from states it already tracks, and widgets tick their own values (a
switch's thumb, a list's hover and selection tints, scroll bars fading,
keys and zones under the pointer) with `UpdateCx::animate`, which asks
for a repaint only when the value changed and for frames only while it
runs. Durations are tokens: fast 120 ms, base 180 ms, slow 250 ms.

**4. Motion can be turned off.** `Motion::enabled` in the theme, and
`Root::set_system_motion` from the window layer, which reads Windows'
"Animation effects" setting (`SPI_GETCLIENTAREAANIMATION`, through
`manguebyte_window::system_motion`) when it opens a real window. With
either off every transition is a jump. Headless windows keep the theme's
motion so that what a test sees does not depend on the machine, and the
test harness starts with motion off so that one frame after an input
shows its end.

**5. The focus shows after the keyboard.** As `:focus-visible` on the
web: `Root::focus_visible` turns on when a key is pressed or assistive
technology moves the focus, and off at a press of the pointer. The ring
is soft (the accent at about two thirds) around the control's face,
eased in. The focus itself, and what assistive technology is told, does
not change.

## The rules, and how each is tested

- *Nothing of the interface runs on or touches the audio thread.*
  Unchanged: motion and shadows live in `manguebyte-ui` and
  `manguebyte-gfx`, which no audio crate depends on
  (`scripts/check-layering.sh`); `tests/realtime.rs` still holds.
- *Idle costs nothing.* `motion.rs`:
  `an_idle_interface_draws_nothing_and_wants_no_frames` runs a thousand
  frames with nothing happening: no frame draws, none asks for more.
  `manguebyte-gui`'s `editor.rs`:
  `an_idle_editor_presents_nothing_and_a_hover_presents_only_while_it_moves`
  fires the window's frame timer 600 times on a headless editor: no frame
  is presented and no widget is painted, and the frame timer is the only
  timer. The frame timer itself still fires every 16 ms while an editor
  is visible, because parameters and meters are polled (ADR 0003); a
  frame that finds nothing changed polls and returns.
- *Animation redraws only what animates, and stops.* `motion.rs`:
  `a_hover_transition_redraws_only_the_hovered_knob_until_it_settles`
  checks every frame's damage lies inside the knob, that the transition
  takes the fast duration (five to nine frames), and that nothing is drawn
  or asked for after; the headless editor test counts the frames
  presented for one hover. `frames_at_the_same_times_draw_the_same_things`
  checks the clock is the one given.
- *Blur and shadows are cached and deterministic.* `manguebyte-gfx`'s
  `shadows.rs`: `a_shadow_is_blurred_once_and_again_only_when_it_changes`
  (same frame, partial render and whole-pixel moves hit the cache; a new
  size or scale misses it; no budget keeps nothing),
  `a_shadow_is_the_same_bit_for_bit_from_the_cache_and_without_it`, unit
  tests of the box blur against floating-point boxes, and golden images
  at 1x and 2x. `allocation.rs` shows that redrawing shadows from the
  cache makes no heap call.
- *Damage-limited repaint equals a full one, shadows included.*
  `shadows.rs`: `damage_through_a_shadow_draws_it_as_a_whole_render_does`
  and `a_shadow_never_draws_outside_the_scene_bounds` at six scales;
  `motion.rs`: `a_shadow_that_spills_past_its_widget_is_drawn_again_with_it`
  compares every frame of a primary pill's hover, whose shadow spills over
  a card, with a fresh whole render at 1x, 1.25x and 2x.
- *Frame rate capped, time injected.* Frames come only from the window's
  frame timer (16 ms, about 60 a second) and from paint requests; the
  root takes the time it is given, which tests set.
- *Reduce motion.* `motion.rs`: `with_motion_off_a_change_jumps_in_one_frame`
  for the system flag and the theme flag.

## Measurements

`cargo run -p manguebyte-ui --example design_bench` on the development
machine (Windows 11, x86-64, with other builds sometimes running), for a
sampler screen of cards, 1100 by 640 logical pixels: the median of 60
runs optimised (`--release`) and of 10 unoptimised (the `dev` profile
without debug information).

| case | release 1x | release 2x | unoptimised 1x | unoptimised 2x |
|---|---|---|---|---|
| first frame, every shadow blurred | 39 ms | 223 ms | 938 ms | 3774 ms |
| whole repaint, shadows from the cache | 20 ms | 90 ms | 968 ms | 2253 ms |
| idle frame (polls only) | 0.003 ms | 0.002 ms | 0.021 ms | 0.022 ms |
| a frame of a knob's hover transition | 0.7 ms | 1.5 ms | 15 ms | 34 ms |

And one card of 300 by 200 at 2x: its shadow blurred and drawn, 14.5 ms
(unoptimised 223 ms); drawn from the cached shadow, 2.7 ms (39 ms); the
card alone with no shadow, 0.2 ms (3.5 ms).

A whole repaint happens when an editor opens, its theme changes or it is
resized; everything else repaints only what changed. While the bench was
written, a first measurement showed whole repaints four to six times
slower, dominated by clip masks cleared over the whole target, shadows
blended a channel at a time and gradients run through the rasteriser's
shader per pixel; all three were fixed before the numbers above.

## Consequences

- The default look changes for everyone; `Theme`, `Palette`, `Corners`
  and `Sizes` changed shape (see the changelog's breaking changes).
- Shadow masks cost memory: a card of 300 by 200 at 2x with the card
  shadow is a mask of about 0.6 megapixels, kept under a 16 MiB budget.
  A shadow whose shape changes every frame (a knob's glowing arc while it
  turns, a drag's ghost) is blurred every frame; the arc's glow is kept
  small for that reason.
- Cards' shadows spill over their neighbours, so a card's own change
  repaints a margin around it. Cards rarely change; controls inside them
  repaint only themselves.
- Goldens changed wholesale once, deliberately.

## Alternatives not taken

- **Blurring on the GPU, or a Gaussian in floating point.** Neither gives
  the same bytes on every machine, which the goldens and CI need.
- **A layer effect (`Layer::with_shadow`) instead of a primitive.** It
  would blur whatever a layer drew, which cannot be cached by shape and
  costs a buffer the size of the target per layer.
- **An animation timer, or callbacks when transitions end.** Both break
  ADR 0003 and make idle cost something; polled values on the frame clock
  cost nothing when nothing moves.
- **Showing the focus ring on every focus change.** It is what made every
  click draw a thick blue box; the web settled on `:focus-visible` for
  the same reason.
