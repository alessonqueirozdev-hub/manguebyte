# ADR 0007: The user-interface layer

Status: accepted, 2026-10-14

## Context

Manguebyte has no user interface yet. Plugins built with it run in hosts with
the host's generic controls, and the standalone wrapper has no window of its
own. The layer we add has to:

- embed plugin editors in a host's window through the CLAP `gui` extension
  and VST 3's `IPlugView`, on Windows, macOS and Linux;
- survive many instances of many plugin binaries in one process, including
  binaries built with different versions of this framework, and hosts that
  open and close editors constantly while audio runs;
- keep the thread model of ADR 0002 and ADR 0003: no listeners, nothing that
  can make the audio thread wait or allocate;
- serve the standalone wrapper and ordinary audio applications as well;
- be verifiable in CI without a screen or a GPU, because CI is the only place
  this project can exercise macOS and Linux.

JUCE users' recurring complaints set the bar the other way: styling through
`LookAndFeel` inheritance, hand-written layout arithmetic, slow software
rendering on HiDPI screens, OpenGL deprecated on macOS, editor lifetime bugs
(dangling processor pointers, timers firing after destruction), parameter
attachments that race, accessibility added late, and no way to test an
interface in CI.

### How the decision was made

Four architects designed the layer independently, each from a different
conviction: own the stack; GPU first; build on the Rust ecosystem; developer
experience first. Three judges scored the designs on ten criteria, from the
points of view of a commercial plugin developer, the long-term maintainer
and the verification engineer, and checked the contested facts against the
crates' source and registries.

Two judges ranked "own the stack" first and one ranked "build on the
ecosystem" first by one point. All three ranked "GPU first" last, and all
four architects, including the GPU advocate, recommended rendering on the
CPU first.

### Facts that shaped it

Checked during judging, October 2026:

- `softbuffer` 0.4.8 registers an Objective-C class under a fixed name, so a
  second plugin binary in the same process panics on macOS; on Windows it
  keeps a process-global allocator with a detached thread. It cannot be used
  in plugins.
- `accesskit_macos` 0.27.1 also registers fixed class names, with the same
  result; `accesskit_unix` keeps process-global state and a detached thread.
- `baseview` 0.4.1 names its macOS view classes per view, which is right, but
  runs its X11 handling on a thread of its own and installs a per-binary
  message hook on Windows.
- `vello_cpu` 0.3 needs Rust 1.89, `parley` 0.12 needs 1.88 and Unicode-3.0
  data tables, `wgpu` 30 needs 1.87 and `egui` 0.36 needs 1.95. `skrifa` 0.48
  and `harfrust` 0.14 build on 1.85, the workspace minimum; `tiny-skia` 0.12
  declares no minimum.
- Found while building: `read-fonts` 0.45.0, which `skrifa` 0.48 and
  `harfrust` 0.14 require, depends on `yoke` 0.8 without making it optional,
  and `yoke` is under Unicode-3.0, outside the licences this project accepts.
  `read-fonts` 0.43.3 does not use it, so `manguebyte-gfx` pins
  `skrifa = 0.46.2` and `harfrust = 0.13.3`, both on `read-fonts` 0.43.3 and
  both declaring Rust 1.85. Moving up needs a `read-fonts` release that makes
  `yoke` optional again, or a decision here about Unicode-3.0.
- The Inter typeface declares no Reserved Font Name, so a subset can keep its
  name provided the OFL text ships with it.
- A Rust DAW built on `wgpu` reports graphics-driver crashes on Windows in
  its own documentation; a plugin cannot tell its users to set environment
  variables to work around its renderer.

## Decision

**1. Layers.** The editor contract lives in `manguebyte-plugin`, with no
third-party dependency and no `unsafe`. `Plugin::editor` returns an optional
factory that is `Send + Sync + 'static`, so it cannot capture the processor,
a borrow, an `Rc` or a raw pointer. The open editor is `!Send` and is closed
by being dropped. `EditorHost` gives an editor what a host offers: resize
requests, main-thread callbacks, and the host's timers and file descriptors.
The contract does not name any toolkit, so an author can still bring one.

As built, `EditorHost` is the trait the wrappers implement, and editors never
hold it. An editor is given an `EditorContext`, which the wrapper ties to its
`EditorHost` through a `HostLink` and cuts off by dropping the link: from
then on every clone of the context does nothing, open gestures are ended and
the timers and descriptor watches it registered are cancelled. Gestures are
the only way an edit reaches the host. Main-thread callbacks go through
`EditorHost::main_thread_callback`, a `Send` `MainThreadWaker` and
`OpenEditor::on_main_thread`; timers and descriptors through
`EditorContext::every` and `watch_fd`, which return handles that cancel when
dropped. `request_resize` returns the size the host granted and never calls
back into the editor, so a wrapper never re-enters an editor it is already
calling.

Four new crates sit above it, and none of them may become a dependency of a
crate that processes audio (CI checks the dependency tree):

| Crate | Holds |
|---|---|
| `manguebyte-gfx` | The scene (a display list per widget), the rasteriser behind a private seam, text |
| `manguebyte-window` | Platform windows behind our own `Window` seam (see decision 4) |
| `manguebyte-ui` | Retained tree, layout, design tokens, widgets, the accessibility model, a test harness |
| `manguebyte-gui` | Glue between the plugin wrappers and the interface |

**2. CPU first.** Each widget records a display list. The framework derives
damage from what changed, keeps a back buffer, repaints at most a few damage
rectangles, and answers an expose by copying the buffer. The rasteriser of
the first phase is `tiny-skia`, used only behind our seam. Its possible
replacements, an own rasteriser (for byte-identical output across processor
architectures) or `vello_cpu` (for speed, once the interface crates may carry
their own minimum Rust version), will be chosen by benchmark, not by
conviction. GPU rendering may come later, behind the same seam, and will not
be the default while CI cannot test its pixels. This replaces the
"GPU-accelerated renderer" of the roadmap.

**3. Text.** `skrifa` for outlines and metrics, `harfrust` for shaping, a
small layout of our own, and an embedded subset of Inter with its OFL text.
The versions are pinned as the facts above explain. Every program that ships
`manguebyte-gfx` ships the typeface, so it has to ship the OFL with it: as a
file beside the program, or in an about screen through
`manguebyte_gfx::TYPEFACE_LICENCE`.

**4. Windows.** A thin backend per platform behind our seam, presenting with
the platform's plain copy (GDI on Windows, layer contents on macOS,
`PutImage` on X11). The interface runs on the host's main thread on every
platform; on Linux it runs through the host's loop (CLAP `posix-fd-support`
and `timer-support`, VST 3 `IRunLoop`), and an editor refuses to open where a
host offers neither. Windows comes first. macOS gets a fixed time box; if our
backend is not green on the CI runner by then, `baseview`'s macOS view code is
adapted behind the seam. X11 follows once CI has shown it can read a window
back.

As built, the seam is not a public trait. `Window` is a concrete type over
a crate-private enum of backends: Win32, and a headless backend for tests
that runs the same damage merging, back buffer, pointer, timer and dispatch
code with no system window. A new backend is added inside the crate, as a
new arm. A public backend trait, which would let a backend live outside the
crate, can come later as a change of its own. The Win32 backend refuses a
parent window that belongs to another process: a child of such a window
shares that process's input queue, so a hang in either process would hang
both.

**5. Parameters.** No listeners. The interface reads the parameters' atomics
once per frame. An edit is a `Gesture` guard: begin, set, end, and cancel to
restore the starting value. In CLAP, per-parameter atomic counters record
gestures; the wrapper turns them into gesture events in `process` or
`flush`, `flush` writes its output list, and the editor asks for a flush when
the host is not processing. In VST 3 the guard calls `beginEdit`,
`performEdit` and `endEdit` on the main thread with the value the control
set. Neither wrapper ever infers an edit by comparing values, which would
report host automation and restored state back to the host as user edits;
while blocks run, the host's points stay the only source of a value, as the
VST 3 wrapper already ensures.

**6. Accessibility.** The interface has its own accessibility model, and
every widget must describe itself: the method has no default. The test
harness finds widgets only by role and name, so a widget that a screen reader
cannot find cannot be tested either. Platform adapters come later, each only
after a test with two plugin binaries in one process passes on that platform.

**7. Hygiene.** No code of ours and no dependency we adopt may register an
operating-system name that is the same in two binaries, change a process-wide
setting such as DPI awareness, install hooks, start global threads or keep
mutable statics. Before a dependency is adopted its source is read for these,
and the result is recorded here. CI loads two separately built plugin
binaries, opens and closes their editors, and unloads them in both orders.

Reading recorded so far:

- `tiny-skia` 0.12.0 and its tree (`tiny-skia-path`, `png`, `fdeflate`,
  `flate2`, `miniz_oxide`, `crc32fast`, `simd-adler32`, `adler2`,
  `bytemuck`, `arrayvec`, `arrayref`, `strict-num`): no statics written at
  run time, no threads, no operating-system names. The one exception is
  `log`, which keeps a per-binary logger that only `log::set_logger` writes;
  the framework never calls it, and `tiny-skia` only emits warnings through
  it. Three behaviours are worked around in our code: a repeating or
  reflecting linear gradient whose two ends coincide can panic, so such
  gradients are resolved to one colour first; `Pixmap::decode_png` sizes its
  buffer from the header alone, so headers are checked against 64 MiB
  first; targets larger than 8191 pixels a side are drawn in internal tiles,
  so a partial repaint is exact only up to that size.
- `skrifa` 0.46.2, `harfrust` 0.13.3, `read-fonts` 0.43.3 and `font-types`
  0.12.6: no operating-system names, no process-wide settings or environment
  reads, no hooks; threads only in their own tests; statics only as
  immutable tables; `harfrust` keeps its lookup caches inside each shaper
  value, not globally. The only `unsafe` is `font-types`' `bytemuck`
  implementations. `font-types` turns on `bytemuck`'s derive, which brings
  `bytemuck_derive`, `syn`, `quote`, `proc-macro2` and `unicode-ident` in as
  build-time dependencies; `unicode-ident` was already in the tree, and the
  licence check has to allow it as a build-only dependency.
- `windows-sys` 0.61.2 and `windows-link` 0.2.1, which `manguebyte-window`
  uses on Windows only: `#![no_std]`, no build script, only types,
  constants and function declarations, linked through `windows-link`'s
  `raw-dylib` macro. No statics, no threads, no operating-system names.
  The modules turned on later for text entry (`System_DataExchange` and
  `System_Memory` for the clipboard, `UI_Input_Ime` for placing an input
  method's windows, from imm32.dll) are the same: declarations only.

How `manguebyte-window` itself keeps the rule on Win32: its window class is
named after the framework's version, the module handle of the binary it is
in and the address of an immutable static of that copy of the crate, so two
plugins, or two copies of the framework in one plugin, never share a class.
The class is registered with the binary's own module handle when a window
is made, and a class of that name with another window procedure is refused.
`UnregisterClassW` is called after every window is destroyed; it succeeds
only once no window of the class is left, so the system's own count of
windows decides, and the crate keeps no count of its own. A window keeps its
state in the class's extra bytes, not in `GWLP_USERDATA`, which anything in
the process may write. There are no hooks, no threads and no mutable
statics, and the process's DPI awareness is never changed:
`GetDpiForWindow` is looked up at run time, with the window's device
context as the fallback on systems that lack it. The helpers that make
top-level windows, run message loops or set a thread's DPI awareness for
tests are behind the crate's `testing` feature, which only tests and the
probe turn on. `manguebyte-gui` has no `unsafe` code, keeps no global
state and starts no thread; everything it asks of the system goes through
`manguebyte-window`. The system caret, which input methods and screen
magnifiers follow, belongs to the thread, not the window: a window makes
one only while it has the keyboard and text is being typed into it, and
destroys it when it loses the keyboard, as the system's convention asks,
so it never takes the caret of a host's own field. The clipboard is
opened for one read or write and closed before the call returns.

**8. Verification.** A probe job opens a window and reads its pixels back on
the three CI systems before any backend depends on that. Scenes are checked
by an exact text dump of the display list and by images with a measured
tolerance. A headless editor is driven end to end through the real CLAP
interface. Editors are opened and closed thousands of times while `process`
runs under the counting allocator. Calls to the `gui` extension are fuzzed in
order and arguments. Nothing about the interface goes into the README,
`BEYOND_JUCE.md` or the website before the test that proves it exists.

## Phase 1, in order

1. This ADR; the roadmap and parity table amended; a layering check, a
   licence check and the window probe job in CI.
2. The editor contract, `Gesture` and the gesture counters in
   `manguebyte-plugin`, with property, allocation and Miri tests. *Done.*
3. CLAP: gesture events, `flush` output, `request_flush`, and the `gui`
   extension tested against a fake editor through the C interface. *Done.*
4. VST 3: the component handler kept, `IPlugView`, content scale,
   `IPlugFrame` and `IRunLoop`, tested against fake host objects. *Done.*
5. `manguebyte-gfx`: scene, rasteriser seam on `tiny-skia`, text, image and
   dump tests, a benchmark that prints its numbers. *Done.*
6. `manguebyte-ui`: tree, layout (row, column, stack), tokens, sources, the
   accessibility model, damage, label, knob, toggle, meter, canvas, harness.
   *Done.*

Items 2 to 6 were each built, attacked by an independent reviewer who wrote
failing tests, and fixed; every fix kept the reviewer's test. Items 7, 8
and the built part of item 9 went through the same process. One reviewer's
test, for a fault that needed a change in the VST 3 wrapper, was ignored
until the wrapper changed: the VST 3 view now shows the editor once
`attached` succeeds and hides it at the start of `removed`, the glue opens
an editor hidden, with no frame timer, until the host shows it, and the
test runs (TECH_DEBT 47, paid back). None of them has been opened in a real
host yet: that is what is left of item 9.
7. A headless editor driven through the real CLAP interface. *Done*, and
   through VST 3's real COM interfaces as well: drags, keys, automation,
   meters, scales and sizes, editors opened and closed thousands of times
   while `process` runs under the counting allocator, and fuzzed `gui` and
   `IPlugView` calls. An editor opens headless only when a test gives it a
   hook (`Editor::on_headless_open`); the `headless` feature only adds that
   API, so turning it on changes no other editor in the build.
8. `manguebyte-window` on Windows and `manguebyte-gui`: embedding and
   read-back tests, open and close cycles, two binaries in one process.
   *Done.* The glue, embedding in a real Win32 parent, read-back of the
   presented pixels and open and close cycles are built and tested on
   Windows; the probe job checks read-back on the CI runner. The test with
   two plugin binaries in one process (`plugins/duck/tests/two_binaries.rs`)
   loads Mangue Duck's and Mangue Pluck's libraries with `LoadLibraryW`,
   opens both editors through the CLAP `gui` extension in real windows,
   reads their pixels back, fuzzes the `gui` calls in random orders with
   hostile arguments against a model of CLAP's rules, unloads the libraries
   in either order and checks that no GDI or USER object and no window
   class is left behind. It runs on the debug libraries in every Windows
   `cargo test`; the same tests on the release bundles, which are built
   without the test features, are `#[ignore]`d and run by the Windows CI
   job after bundling. `manguebyte-gui` reads "no frames when nothing is damaged" this
   way: while the editor is visible, a timer the window owns polls about
   60 times a second, because parameters and meter levels are atomics with
   no change notification, and a poll that finds nothing changed lays out,
   draws, renders and presents nothing. The timer is killed while the
   editor is hidden. An idle backoff would delay the first change after a
   quiet spell by the backoff period.
9. An editor for `plugins/duck`, opened by hand in at least one host on
   Windows and recorded. *Built, not yet opened by hand.* Mangue Duck's
   editor (440 by 210 logical pixels) has a title, the Bypass switch, knobs
   for Threshold, Depth, Attack and Release that show their values in the
   parameters' own words, and a Reduction meter from 0 to 40 dB, which the
   processor feeds through a `ReductionMeter` (one atomic store per block)
   and which falls to 0 when nothing has been published for half a second.
   Mangue Pluck has an editor too (520 by 160, a knob for each of its six
   parameters), as the second binary of item 8's test. Both are tested
   headless (layout at scales 1 to 2, drags, keys, automation, the meter),
   Duck's against golden images too. The hand test in REAPER is still to be done
   and recorded for Duck's editor. Mangue Pluck's CLAP editor was opened by
   hand in REAPER 7.82 on Windows on 19 October 2026 and showed its title
   and knobs ([HOST_TESTS.md](../HOST_TESTS.md)); working its knobs there
   was not tested.

Not in phase 1: macOS and X11 backends, platform accessibility adapters,
resizing, popups, text entry, GPU code, hot reloading of themes.

Built since, on the same rules: text entry (a text field, values typed
into knobs and sliders, the clipboard, input methods' committed text),
popup menus and a combo box, hover and tooltips. Menus and tooltips are
not windows of their own: the root draws them over the interface, inside
the editor's window, as ordinary widgets it adds after the interface's
own and takes away when they close, so they need no second system window,
no window class and nothing a host would have to embed, and they are
tested headless like everything else. The price is that a menu cannot
reach past the editor's window: one taller than the room above and below
its widget starts at the top and is cut at the bottom, as menus do not
scroll yet.

Scroll views, lists of rows of which only those in sight exist, drag and
drop inside the interface, and pictures followed, as
[ADR 0009](0009-scrolling-lists-and-drag-and-drop.md) describes. Menus
still do not scroll.

## Consequences

- Plugin authors get an interface layer with no process-wide state, one
  thread model and tests that run in CI, at the cost of a slower start: the
  first phase ships Windows only, and macOS and Linux claims wait for their
  tests.
- More code is ours (the window backends), but it is small, and the pure
  parts (scene, layout, widgets, accessibility) are safe code that is cheap
  to keep alive.
- Rendering quality starts at what `tiny-skia` gives. Heavy visuals that
  need a GPU wait for a later phase.

Open questions, to be answered by the first phase rather than by argument:
whether GitHub's macOS runners let a test read a window back; whether
`tiny-skia` builds on Rust 1.85 and is fast enough at 2x scale; how far
images differ between the x86-64 and aarch64 runners; which Linux hosts
offer no loop support; keyboard focus in specific hosts, which only a manual
matrix can show; and how to give VoiceOver users an accessible interface on
macOS before AccessKit's class names are fixed upstream.

## Alternatives not taken

- **GPU first** (`wgpu` with a vector renderer): minimum Rust version above
  ours, a much larger binary, driver failures inside other people's
  processes, and pixels CI cannot check.
- **An existing toolkit as the framework's interface** (`egui`, `iced`,
  `vizia`, `slint`): minimum versions well above ours, a licence outside our
  list in one case, and thread or global-state models that do not fit
  plugins. The contract stays open to them for authors who want one.
- **A web view as the core**: possible later as an optional backend, not as
  the foundation.
