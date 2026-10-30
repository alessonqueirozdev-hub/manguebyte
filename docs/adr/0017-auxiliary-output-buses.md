# ADR 0017: Auxiliary output buses

Status: accepted, 2026-10-30

## Context

A plugin had one main output, a main input and a sidechain. Two products
the framework is built for need more: Caecilia 2.0, a pipe organ, wants
each division (Great, Swell, Choir, Pedal) and its reverb send on an
output of its own, so that a DAW can mix and place them; Parabolic's drum
page wants an output per pad. Every format has a way to say this: CLAP
has several audio ports per direction, VST 3 auxiliary buses, Audio Units
several output elements, LV2 as many ports as a plugin likes and the
port-groups extension to say which belong together. JUCE describes the
same thing with `BusesProperties`: named buses, each with a default
layout and a flag for whether it starts enabled.

The plugin API had to grow without breaking the plugins written against
it, without allocating on the audio thread, and so that a bus nobody
listens to costs the plugin nothing.

## Decision

**1. Declared, fixed buses.** A plugin lists its auxiliary outputs in a
constant, `Plugin::AUX_OUTPUTS`, as `AuxOutput { name, channels,
active_by_default }`, in the order hosts show them after the main output.
They do not change with the layout a host chooses from `Plugin::LAYOUTS`,
and each has one width. The formats fix a plugin's bus count, and the
products need named stems, not reconfigurable ones.

**2. A new process method with a default.** `Plugin::process_with_aux`
takes what `process_with_output` takes and `&mut AuxOutputs`. Its default
calls `process_with_output` and leaves the buses silent, so a plugin that
declares none, or never overrides it, behaves as before, and every runner
(each wrapper, the standalone runner, the harness) calls only the new
method. Nothing in the existing API changes.

**3. A bus switched off is absent.** `AuxOutputs::bus_mut(index)` returns
`None` for a bus the host has switched off. A plugin that renders only
the buses it is given spends nothing on the others, and the wrapper
writes silence to whatever memory the host gave for them. A bus that is
on comes cleared, the length of the block, in a buffer of its own. The
runner owns the buses, made when the plugin is activated; readying them
for a block, and cutting them with a block processed in pieces
(`AudioBuffer::narrow` and `widen`, the closure-free form of `windowed`,
since a run-time number of buffers cannot be narrowed with nested
closures), allocates nothing.

**4. Each format's own switch, and its own default.**

- CLAP: one audio port per bus, with its name, channel count and port
  type; `audio-ports-activation` is offered when a plugin has buses.
  CLAP says every port starts active, so `active_by_default` is not
  used, and a newly selected configuration turns them all on again.
- VST 3: one `kAux` audio output bus each, flagged `kDefaultActive` as
  declared, switched by `activateBus` while inactive;
  `setBusArrangements` takes each at its one width.
- Audio Units: one output element each, named, of fixed width. A host
  pulls each element of a slice on its own and in any order, so for a
  plugin with buses the plugin runs once per time stamp and the other
  elements of that slice are read from what it wrote, as Apple's own
  base classes do; a plugin without buses renders as before. An Audio
  Unit cannot switch an element off, so every bus is rendered.
- LV2: one audio output port per channel, after the main outputs and
  before the atom ports, each `lv2:connectionOptional`, in a port group
  per bus (`pg:StereoGroup` or `pg:MonoGroup` with `pg:left`, `pg:right`
  and `pg:center` designations where they fit). A bus whose ports are all
  unconnected is off. A plugin without buses keeps its port indices.
- Standalone: each bus is mixed into device channels of its own, by
  default the ones after the main output's, for the buses on by default;
  the application can move a bus or leave it unheard. A bus that is not
  heard, or that the device has no channel for, is not given to the
  plugin.

**5. The host reads them the same way in every format.** Each of the
framework's processors gains `process_with_aux(buffer, context, aux)`,
which copies the buses after the main one into buffers the caller owns,
so no format's memory leaves the processor and nothing is allocated.
Switching is per format, before activation, as the formats allow it.

## Consequences

- Organ divisions, sends and drum pads can each go to a mixer channel of
  their own in a DAW, through any of the four formats, and through the
  standalone runner to an interface's outputs.
- Plugins written before this compile and sound the same. Two public
  types gained something that code outside could have matched or built
  by hand: `manguebyte_lv2::PortRole` has a new variant, and
  `StandaloneOptions` a new field; the host's LV2 description gained
  fields too. The changelog lists them under Breaking.
- An Audio Unit renders every bus, whatever the host listens to.
- The one interface over hosted plugins (`host::any`) and the graph do not
  yet carry auxiliary buses; a graph node has one output.

## Alternatives not taken

- **Widening `AudioLayout` with a list of buses.** `AudioLayout` is a
  small `Copy` value compared and offered as a whole; a list of buses in
  it would multiply the layouts a plugin must list and change every
  `fits` rule, for buses that do not change width anyway.
- **One wide main buffer holding every bus, as JUCE's `processBlock`
  does.** It would change what `buffer` means to every plugin, and a
  plugin would have to find its buses by channel offsets that shift as
  buses are switched; a buffer per bus is what the formats hand over
  anyway.
- **Giving a switched-off bus as an empty buffer.** A plugin would then
  write into a buffer of no frames without noticing; `None` makes it say
  what it does with a bus nobody listens to.
