# ADR 0020: Parameter names that change at run time

Status: accepted, 2026-10-30

## Context

Caecilia, an organ plugin and one of the framework's first users, swaps
the whole organ while it runs. Its stop and coupler parameters are fixed
pools driven by number (`stop-01` to `stop-NN`), so that saved state and
automation keep pointing at the same parameters, and after a swap they
drive the new organ's stops. Hosts went on showing the first organ's
names until the plugin was reloaded: a parameter's name was a `String`
fixed when it was made, and no wrapper had a way to tell a host that it
had changed.

What is needed:

- a way to give a parameter a new display name, and its unit and a short
  name with it, from any thread but the audio thread, while its id and
  `ParamId` never change;
- each wrapper reporting the current names, and telling its host to read
  them again, in the way its format allows, once for a whole batch of
  renames: an organ swap renames sixty stops, and a host that rebuilt its
  parameter list sixty times would stall;
- ADR 0002 and 0003 kept: nothing the audio thread runs may lock,
  allocate or wait, and there are no listeners; state is polled.

The formats differ in what they let a plugin say and when:

- CLAP has `clap_host_params.rescan(CLAP_PARAM_RESCAN_INFO)`, which the
  specification allows while the plugin is active and which must be
  called on the main thread. The plugin can ask for a call on the main
  thread from any thread with `request_callback`.
- VST 3 has `IComponentHandler::restartComponent(kParamTitlesChanged)` on
  the main thread, but gives a plugin no way to ask for a call there.
- Audio Units have property listeners, which the wrapper already calls
  for the latency and the stream formats; `kAudioUnitProperty_ParameterList`
  is the property hosts watch for a changed list. There is no call on the
  main thread either.
- LV2 names live in the Turtle files shipped with the plugin.

JUCE's `AudioProcessor::updateHostDisplay`, with
`ChangeDetails().withParameterInfoChanged(true)`, is the analogue: the
processor says that parameter information changed and the wrapper tells
the host.

## Decision

**1. Names live behind a lock in `ParamInfo`, the count beside them in an
atomic.** `ParamInfo` keeps what the parameter was made with, as before,
and adds what it shows now: a display name, a short name and a unit, as
`Arc<str>`s in one `RwLock`, and an `AtomicU64` that counts the changes.
`set_display_name`, `set_short_name` and `set_display_unit` take the lock
to write, and count only a real change; `display_name`, `short_name` and
`display_unit` take it to read and hand back a clone of the `Arc`, so the
lock is held for a pointer copy. A lock is acceptable here, and only here,
because names are text and text has always been off the audio thread
(every method of `Param` that takes or returns text allocates). The
audio thread never touches it: what it may read is `labels_version`, one
atomic load.

**2. The API is additive.** `ParamInfo::name()` and `unit()` keep their
signatures and now mean what the parameter was made with; `display_name()`,
`short_name()` and `display_unit()` are new, and `Param` gains
`display_name()`. Code that compiled before compiles now and behaves the
same for a plugin that never renames. The framework's own users of
names that people see were switched to the current ones: the CLAP, VST 3
and AU wrappers, `ParamDescription` (so editors outside Rust get the new
names), and the text of values (`display_plain`, `parse_display`, the
float and integer parsers), which follow the current unit.

**3. The set polls; the wrappers coalesce.** `ParamSet::labels_version`
is the sum of its parameters' counts: it changes whenever any parameter
is renamed and only then, without a shared counter that every parameter
would need a pointer to. It walks the set and loads one atomic each,
which is what the wrappers already do for values before each block. Each
wrapper keeps the count it last told its host. On a poll, if the count
has moved, it tells the host once, however many renames there were in
between, and notes the new count; a rename to the same name is no change
and tells nothing.

**4. Each format where its rules allow.**

- CLAP: `get_info` reports the current name. The audio side polls the
  count before each block and in `flush`; when it moves and the host is
  not yet told, it calls `request_callback`, once per change it sees.
  `on_main_thread`, `activate`, a state load and the editor's
  `values_changed` compare the count with the one told and call
  `rescan(CLAP_PARAM_RESCAN_INFO)`, combined with `VALUES` in the last
  two, so a preset or an organ that brings values and names is one
  rescan.
- VST 3: `getParameterInfo` reports the current title, short title and
  units. With no call on the main thread to ask for, the count is
  compared where the host calls in anyway: `setActive`, the component's
  `setState`, `setComponentState`, `setComponentHandler`, and an open
  editor's `values_changed`, which asks for `kParamValuesChanged` and
  `kParamTitlesChanged` in one restart. Only with a handler to tell.
- AU: `ParameterInfo` gives the current name, in both its fields, and the
  unit is chosen from the current unit text. `Initialize` and a restored
  `ClassInfo` compare the count and call the listeners of
  `kAudioUnitProperty_ParameterList` once.
- LV2: hosts read the names from the shipped Turtle files; a rename is
  not seen. The plugin's own editor can show what it likes.

## Alternatives not taken

- **`name()` returning `Arc<str>`.** One meaning for one method, and every
  site that shows a name would have been found by the compiler. But it
  breaks every caller that matches or compares a `&str`, in the framework
  and in plugins, for a feature few plugins use; and the shipped name is
  still wanted, for LV2's Turtle and for anything generated at build time.
- **A lock-free swap of the names (`arc-swap` or an epoch scheme).** No
  lock at all, but a new dependency or `unsafe` in a crate that forbids
  it, to make lock-free a path that never runs on the audio thread.
- **One counter in the set that every parameter bumps.** One load instead
  of one per parameter, but a parameter is a plain field of a plain
  struct and does not know its set; it would need a pointer handed to it
  after construction, by every way a set can be made.
- **A process-wide counter as a cheap gate.** One load for the common
  case of nothing renamed, but each instance of a plugin, and each plugin
  built from the framework in the same library, would see the others'
  renames and walk its set for nothing. The walk costs what the value
  watcher's walk costs, so the gate was not worth the coupling.
- **A notification call (`EditorHost::labels_changed` or a channel).**
  ADR 0003: no listeners, and a callback from whichever thread renamed is
  how interface code ends up on the wrong thread. Polling gives the
  coalescing for free.
- **A timer of the wrapper's own for VST 3 and AU.** A main-thread timer
  per system (`SetTimer`, `CFRunLoopTimer`, the host's `IRunLoop` where
  there is one) would let renames reach the host within a fraction of a
  second whatever the host calls. It is the next step, not taken here
  (TECH_DEBT item 113).

## Consequences

- A plugin renames from any thread but the audio thread with
  `param.info().set_display_name(..)`; ids, state and automation are
  untouched.
- CLAP hosts are told within a block and one main-thread callback while
  the plugin processes, and at the next activation or state load while it
  does not. VST 3 and AU hosts are told at the next call that may carry
  it; a plugin that renames from its editor should call `values_changed`
  afterwards, which also covers values the swap set.
- The framework's own host (`manguebyte-host`) already read the parameter
  list again on `CLAP_PARAM_RESCAN_INFO` and `kParamTitlesChanged`; a
  test hosts a renaming plugin through both wrappers in process. Its AU
  side does not listen to property changes.
- `manguebyte-ui`'s parameter handles still show the name and unit a
  parameter was made with.
- What is left is in [TECH_DEBT](../TECH_DEBT.md) item 113.
