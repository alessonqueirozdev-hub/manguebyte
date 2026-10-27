# Where Manguebyte goes beyond JUCE

JUCE is the reference for what a complete audio framework contains, and
Manguebyte's first goal is to match it. This page is about the second goal:
the places where Manguebyte already does something JUCE does not, and how
each claim can be checked.

Three rules keep this page honest:

1. **Every claim points at a test.** If it cannot be verified by running
   something in this repository, it is not listed.
2. **Every statement about JUCE has a source**, and describes JUCE's `master`
   branch at version 9.0.4 (October 2026). JUCE keeps moving; the JUCE team
   has announced an "AudioProcessor v2" that targets several of these points.
3. **Limits are stated next to the claim**, not left for the reader to find.

JUCE is far ahead in breadth, and [the last section](#where-juce-is-ahead)
says where. Nothing here contains JUCE code: see
[ADR 0004](adr/0004-juce-as-reference.md).

## Summary

| | JUCE 9.0.4 | Manguebyte | Checked by |
|---|---|---|---|
| [Automation timing](#1-automation-on-the-exact-sample) | Per block in `AudioProcessor` | On the exact sample | [`harness.rs`](../crates/manguebyte-plugin/src/harness.rs), [`abi.rs`](../crates/manguebyte-clap/tests/abi.rs) |
| [Block size and sound](#2-the-sound-does-not-depend-on-the-hosts-block-size) | Left to each plugin | Bit-identical across block sizes, tested | [`offline.rs`](../crates/manguebyte/tests/offline.rs) |
| [Changes from a UI or a restored state](#3-every-parameter-change-reaches-the-plugin-as-an-event) | Seen by polling, or by listeners that may run on the audio thread | Delivered as events in the same stream as automation | [`harness.rs`](../crates/manguebyte-plugin/src/harness.rs) |
| [Real-time safety](#4-real-time-safety-is-tested-not-promised) | A convention | A heap call while processing fails CI | [`realtime.rs`](../crates/manguebyte-dsp/tests/realtime.rs), [`abi.rs`](../crates/manguebyte-clap/tests/abi.rs) |
| [Per-note control](#5-per-note-expression-and-modulation) | Through MPE channels | Per-note expression and modulation events, which MPE and MIDI 2.0 arrive as too | [`synth.rs`](../crates/manguebyte-dsp/src/synth.rs), [`note_input.rs`](../crates/manguebyte-dsp/tests/note_input.rs) |
| [CLAP](#6-native-clap) | Not available natively | Native, passes `clap-validator` | [CI](../.github/workflows/ci.yml) |
| [Testing a plugin](#7-a-plugin-can-be-tested-without-a-host) | Needs a host or a hand-built rig | In a unit test, with `Harness` | [`plugins/pluck`](../plugins/pluck/src/lib.rs) |
| [Thread safety of parameters and state](#8-parameters-and-state-cannot-race) | Synchronised by each plugin | Atomics; nothing to lock | [`set.rs`](../crates/manguebyte-params/src/set.rs) |
| [Licence](#9-licence) | AGPLv3 or commercial | MIT or Apache-2.0 | [`LICENSE-MIT`](../LICENSE-MIT) |
| [Hosting](#11-hosting-a-plugin-thread-rules-in-the-types-every-event-on-its-frame) | No CLAP hosting; one object called from every thread; parameter changes carry no frame | CLAP and VST 3; thread rules in the types; every event on its frame | [`clap_pluck.rs`](../crates/manguebyte-host/tests/clap_pluck.rs), [`vst3_pluck.rs`](../crates/manguebyte-host/tests/vst3_pluck.rs) |
| [Loudness and broadcast meters](#12-loudness-true-peak-and-broadcast-meters-ready-to-use) | None documented | BS.1770 / R 128 loudness, true peak, VU, PPM, correlation, checked against the EBU's test signals | [`ebu_tech_3341.rs`](../crates/manguebyte-analysis/tests/ebu_tech_3341.rs) |
| [OSC timing](#13-open-sound-control-that-lands-on-the-sample) | Listeners on the message or network thread | Time-tagged bundles land on their frame; every OSC 1.0 type; TCP | [`bridge.rs`](../crates/manguebyte-osc/tests/bridge.rs) |
| [Pitch and band analysis](#14-pitch-detection-and-fractional-octave-analysis) | None documented | YIN and McLeod pitch; IEC octave bands and weighting | [`pitch.rs`](../crates/manguebyte-analysis/tests/pitch.rs) |
| [Keyboard state](#15-a-keyboard-an-interface-can-read-without-listening) | Listeners that may run on the audio thread | Atomics an interface polls; no listener, no lock | [`realtime.rs`](../crates/manguebyte-midi/tests/realtime.rs) |
| [Slow-downs in silence](#10-a-fading-sound-cannot-slow-the-audio-thread-down) | A guard each plugin places in its own callback | Set by the framework around every plugin and every device callback | [`denormal.rs`](../crates/manguebyte-core/src/denormal.rs), both `abi.rs` |
| [Instruments ready to play](#16-instruments-ready-to-play-measured-in-tests) | Classes to build an instrument from; none ready to play | Synthesised and modelled instruments and drum kits, their tuning and spectra measured in tests | [`instruments`](../crates/manguebyte-dsp/src/instruments) |
| [Changing the audio device](#17-changing-the-audio-device-in-the-middle-of-a-song) | The device stops, and the callback is prepared again for the next one | Prepared once; the output changes under the engine with a crossfade or a short faded gap, the engine's output the same bit for bit | [`hotswap.rs`](../crates/manguebyte-devices/tests/hotswap.rs) |

## 1. Automation on the exact sample

**The problem.** A host records automation with sample positions. If a
plugin only learns the value once per block, a change that should happen on
frame 300 of a 512-frame block is applied on frame 0 or frame 512 instead.
Fast automation turns into steps, and the size of the steps depends on the
host's buffer setting.

**In JUCE.** `AudioProcessor::processBlock` receives audio and a MIDI buffer.
Parameter changes are not part of the call; a processor reads each
parameter's current value, which the host updates between blocks. In May 2026
a member of the JUCE team said sample-accurate automation would not be ready
for the first release of JUCE 9
([forum thread](https://forum.juce.com/t/sample-accurate-parameter-automation/68800)).

**In Manguebyte.** The process call carries an `EventBuffer` in which every
parameter change has the frame it falls on. `split_at_events` renders up to
each event, applies it, and continues, using `AudioBuffer::windowed` to
narrow the buffer in place. Nothing is copied or allocated.

**Check it.** `automation_lands_on_its_exact_frame_and_updates_the_parameter`
schedules changes on frames 100, 101 and 250 and asserts the output sample by
sample. `automation_lands_on_its_frame_when_processing_in_place` does the
same through the CLAP C interface, and a test of the same name in
[`manguebyte-vst3`](../crates/manguebyte-vst3/tests/abi.rs) does it through
VST 3, where a host sends a queue of points per parameter and every point
is delivered. (JUCE's VST3 wrapper applies the last point of each queue:
`processParameterChanges` in `juce_audio_plugin_client_VST3.cpp`.)

VST 3 adds a trap of its own. A host reports each change twice: to the
processor with a frame number, and to the controller on the main thread so
that an editor can follow. If the controller's copy is stored into the
parameter, the plugin jumps to a value that is slightly early or late until
the next point corrects it. While blocks are running, Manguebyte's wrapper
keeps the controller's copy for display only;
`what_the_controller_is_told_does_not_disturb_running_automation` checks it.

**Limit.** The framework delivers the timing; a processor chooses whether to
use it. Reading a parameter once per block remains valid and is cheaper. The
example plugin follows events for its voices and reads its reverb mix and
output level once per block, with smoothing.

## 2. The sound does not depend on the host's block size

**The problem.** Hosts hand plugins blocks of different lengths, and some
change the length on every call. If anything in a plugin is tied to block
boundaries, the same project sounds slightly different at 64 and at 1024
samples, and an offline bounce differs from what was heard while mixing.

**In JUCE.** Nothing in the framework prevents or tests this. It is up to
each plugin author to split blocks at events by hand and to notice when
something depends on the block length.

**In Manguebyte.** Because events carry their frame and the synth resets a
voice before every fresh note, the output is a function of the timeline and
not of how it was cut up. This is tested rather than assumed.

**Check it.** `the_song_is_the_same_whatever_the_block_size` renders a
polyphonic song with effects and automation six ways: 64, 256, 997 and 1024
frames, an irregular pattern of `1, 511, 7, 300, 64, 2, 129`, and blocks
larger than the instrument's own maximum. The results must be equal bit for
bit. The example plugin has the same test.

The first time this test ran, it failed. A voice that had finished in the
middle of a block kept advancing until the end of it, so the next note
started from a state that depended on the block length. The test exists to
find exactly that, and the fix is part of the `Voice` contract.

**Limit.** The guarantee covers what follows events. A value that a processor
reads once per block changes at block boundaries by definition.

## 3. Every parameter change reaches the plugin as an event

**The problem.** Host automation is not the only thing that changes a
parameter. A user turns a knob in the plugin's own window, or the host
restores a saved state. Those writes do not arrive through the host's event
list, so a plugin that tracks a value from events alone never hears about
them.

**In JUCE.** A processor finds out by polling the parameter, or through a
listener. Parameter listeners are called synchronously on whichever thread
made the change, which during host automation is the audio thread, and the
framework's attachment classes respond by posting to the operating system's
message queue, which takes a lock
([forum thread](https://forum.juce.com/t/apvts-updates-thread-realtime-safety/36928),
[bug report](https://forum.juce.com/t/bug-report-asyncupdater-for-real-time-use-in-parameterattachment/50645)).

**In Manguebyte.** There are no listeners. Before each block, `ParamWatcher`
compares every parameter with the value the plugin last saw and adds an event
on frame 0 for each one that differs, ahead of the host's own events. The
event stream is therefore complete: a plugin that follows it never needs to
poll, and no user code runs as a side effect of a write.

**Check it.** `changes_made_outside_the_host_reach_the_plugin_as_events` uses
a test plugin that never reads its parameter, only events. It passes for a
direct write, for a restored state, and for host automation on the same
frame, which takes precedence.

**Limit.** The plugin receives the *latest* value of each changed parameter
at the start of the next block. If a knob is moved through ten values between
two blocks, the plugin sees the tenth, not all ten. That is the intended
behaviour for a control, and it is why a plugin that wants no steps smooths
the value.

## 4. Real-time safety is tested, not promised

**The problem.** Code on the audio thread must not allocate or free memory:
the allocator can take a lock or ask the operating system for pages, and
either can outlast the few milliseconds a block has. One stray allocation is
an audible dropout that appears only under load.

**In JUCE.** Real-time safety is a rule in the documentation. The framework
does not check user code against it, and its own parameter attachments break
it, as described above.

**In Manguebyte.** The test suite replaces the global allocator with one
that counts. Any allocation, reallocation or free made while a processor is
processing fails the test, and so fails CI. The same guard wraps the CLAP
wrapper's `process` call, measured from the host's side of the C interface.

**Check it.** `processors_do_not_use_the_heap` and its neighbours cover every
processor, the FFT, a synth handling more notes than it has voices, and
parameter changes made while running. In `abi.rs`, `heap_calls` must be zero
for automation, notes, changes reported to the host and the block after a
state restore.

The guard has already earned its place. On macOS it reported one heap call
inside `process`: a test plugin was recording a value through a standard
mutex, which on that platform allocates the first time it is locked.

**Limit.** This is a test that runs, not a property the compiler proves. It
catches heap use on the paths the tests exercise, and it does not detect a
lock that never allocates. Checking this at compile time is a goal listed in
the roadmap; Rust has no stable way to express it yet. The `unsafe` code that
handles memory, in the wait-free queue and the CLAP and VST 3 wrappers, also
runs under Miri, which found and led to the fix of an invalid pointer in the
CLAP wrapper.

## 5. Per-note expression and modulation

**The problem.** Bending, swelling or filtering one note of a chord needs a
way to address that note. So does a host's modulation system that wants to
move a parameter for one voice without overwriting the value the user set.

**In JUCE.** Per-note control is available through MPE, which gives each note
its own MIDI channel. The processor model has no note expression events and
no modulation distinct from changing a parameter's value, which is why the
third-party CLAP wrapper adds its own extension interfaces for them
([clap-juce-extensions](https://github.com/free-audio/clap-juce-extensions)).

**In Manguebyte.** Notes have an identity (`NoteRef`: channel, key and an
optional id), so two notes on the same key can overlap. `NoteExpression`
shapes one note. `ParamModulation` adds an offset on top of a parameter,
optionally for a single note, and never changes the stored value, so it
cannot fight automation or end up in a saved preset. `Synth` routes each to
the voice that is playing the note.

MPE and MIDI 2.0 arrive as the same events. One converter,
`dsp::note_input::NoteInput`, reads the MIDI of every device input, the
standalone runner, the test harness and the CLAP, VST 3 and LV2 wrappers:
a note of an MPE zone gets an identifier and is reported on the zone's
master channel, its member channel's pitch bend (under the zone's bend
range), pressure and controller 74 become its tuning, pressure and
brightness, and MIDI 2.0's per-note pitch bend, pressure, pitch attributes
and registered per-note controllers do the same. MIDI 1.0 values are
widened with the specification's own method first, so an MPE stream and
the MIDI 2.0 stream that says the same thing give identical events, bit for
bit. A plugin says in `Plugin::MPE` whether it expects zones from the
start.

**Check it.** `expression_and_modulation_reach_one_note_and_parameters_reach_all`
and `notes_with_identifiers_overlap_on_one_key_and_end_separately` in
`synth.rs`; `an_instrument_takes_notes_expression_modulation_and_a_sidechain`
in `abi.rs` does it through CLAP.
`an_mpe_stream_and_the_same_midi_2_stream_give_identical_events` in
[`note_input.rs`](../crates/manguebyte-dsp/tests/note_input.rs);
`clap_notes_mpe_and_midi_2_end_in_the_same_events` in the CLAP wrapper's
`abi.rs` plays one phrase as CLAP notes, as MPE and as MIDI 2.0 packets
and requires the same output from all three, with no heap use.

**Limit.** Per-note *automation* from a CLAP host arrives for a note id
or a key, not for a whole channel, and reaches a voice only if the voice
overrides `Voice::set_note_param` ([TECH_DEBT](TECH_DEBT.md) item 91).
A plugin sends notes and per-note expression out, as MIDI 2.0 packets
where the host takes them, through CLAP, VST 3 and the standalone runner
(`Plugin::process_with_output`), and as MIDI 1.0 through AU and LV2, but
a `Processor` on its own cannot send events. A MIDI output port opened
for MIDI 2.0 gets the packets unchanged through CoreMIDI on macOS and iOS
and the ALSA sequencer on Linux; on Windows, and on every other port, it
gets MIDI 1.0 bytes ([TECH_DEBT](TECH_DEBT.md) items 82 and 90). Per-note controllers with no `Expression` to become
(modulation, breath, the assignable ones) are dropped, and channel-wide
MIDI 2.0 messages reach the processor at MIDI 1.0 resolution
([TECH_DEBT](TECH_DEBT.md) item 76).

## 6. Native CLAP

**In JUCE.** Building CLAP plugins is not supported natively. The plugin
client module covers VST, VST3, AU, AUv3, LV2 and AAX. CLAP was listed for
JUCE 9 in the 2024 roadmap and is not in the 9.0.x releases; the route today
is the unofficial `clap-juce-extensions`.

**In Manguebyte.** `export_clap!(MyPlugin)` is the whole integration. CLAP's
event model is the one this framework is built on, so nothing is approximated
on the way in.

**Check it.** CI builds both example plugins and runs the CLAP project's own
[`clap-validator`](https://github.com/free-audio/clap-validator) on them,
failing the job on any failed test. Run by hand on Windows on 20 October
2026, the two pass 71 of their 88 tests, with none failed. The rest are
skipped because they test what the plugins do not offer: preset discovery,
and `audio-ports-config` and `configurable-audio-ports`, since each offers
one layout (the wrapper offers `audio-ports-config` to a plugin that lists
more in `Plugin::LAYOUTS`); the effect's `audio-ports-activation` is
tested and passes. Also skipped: the denormal timing for the instrument (it
has no audio input), the note tests for the effect (it takes no notes) and
a test for Unix-like systems only.

`cargo mangue bundle` writes the `.clap` beside the `.vst3` in each
system's layout, and `cargo mangue validate` runs the CLAP project's
validator on it as well as Steinberg's on the `.vst3`. JUCE's
[`juce_add_plugin`](https://raw.githubusercontent.com/juce-framework/JUCE/master/docs/CMake%20API.md)
accepts no CLAP format.

**Limit.** Editors open in the host's window on Windows only, through
`manguebyte-window`'s Win32 backend, which CI tests with real windows; macOS
and Linux windows are not there yet. By hand, one editor has been opened
inside a DAW: Mangue Pluck's, as a CLAP in REAPER on Windows, where it
showed its title and knobs; working its knobs there has not been tested.
64-bit audio runs through the plugin's 32-bit path, in CLAP as in VST 3;
there are no presets. VST 3, LV2 and AU (version 2, whose example
plugins pass Apple's `auval` in a macOS CI job) are the other formats on
`main`: JUCE's AUv3 and AAX support has no counterpart here yet. Both
example plugins were also loaded and tested by hand in REAPER before they
had editors; see [HOST_TESTS.md](HOST_TESTS.md).

## 7. A plugin can be tested without a host

**In JUCE.** Exercising a plugin's audio path needs a host, or a test rig the
author builds around `AudioProcessor`.

**In Manguebyte.** `Harness` is that rig. It prepares the plugin, schedules
notes and automation on a timeline, cuts time into any pattern of block
sizes, keeps a transport moving and saves and restores state. The plugin's
sound becomes something an ordinary unit test can assert, on a CI machine
with no sound card.

**Check it.** The example plugin's tests check that it is silent until
played, that it plays the pitch it is asked for (by FFT), that automation and
per-note modulation change the sound, that state round-trips, and that the
output is independent of block size.

## 8. Parameters and state cannot race

**In JUCE.** `getStateInformation` and `setStateInformation` can be called
from any non-audio thread while `processBlock` is running, and the framework
does not serialise them; each plugin has to
([forum thread](https://forum.juce.com/t/are-getstateinformation-and-setstateinformation-always-called-on-the-audio-thread/53467)).
Between JUCE 5.0 and 5.2 the tree that state was saved from could lag behind
the real parameter values
([forum thread](https://forum.juce.com/t/audioprocessorvaluetreestate-thread-safety/21811)).

**In Manguebyte.** A parameter is an atomic, and every method takes `&self`.
Saving reads the atomics; loading writes them. There is one copy of each
value, so state cannot go stale, and the compiler rejects code that would
share anything else between threads without synchronisation. Saved state
holds real values keyed by identifier, so a range can be reshaped in a later
version without changing how an old session sounds.

**Check it.** `state_from_another_version_loads_as_far_as_it_can` and
`params_are_shareable_across_threads` in the parameters crate.

**Limit.** This covers parameters. Larger state, such as a loaded sample, has
a documented pattern (build off-thread, hand over through a queue) but no
ready-made type yet.

## 9. Licence

JUCE is offered under the AGPLv3 or a commercial licence. Manguebyte is under
MIT or Apache-2.0, at your option: no copyleft, no per-seat fee and no splash
screen, for open and closed products alike.

## 10. A fading sound cannot slow the audio thread down

**The problem.** A fading sound does not reach zero. Filter memory and reverb
tails shrink until their values are *subnormal*: still not zero, and tens of
times slower for a processor to compute with. The plugin then costs the most
in the silence after a note. Processors have a per-thread setting that rounds
such numbers to zero, and using it is standard practice.

**In JUCE.** The setting is available as
[`ScopedNoDenormals`](https://docs.juce.com/master/classScopedNoDenormals.html),
a guard that the author of each plugin places at the top of `processBlock`.
The plugin template includes it. Code that did not start from the template,
or a callback written later, is unprotected until someone notices.

**In Manguebyte.** The framework holds the guard itself: the CLAP and VST 3
wrappers and the standalone engine around `Plugin::process`, and the device
streams around every callback. There is nothing for a plugin author to
remember. The setting is put back on the way out, so the host's own code
runs as it expects.

**Check it.** `numbers_too_small_to_hear_are_zero_while_the_plugin_runs_and_only_then`,
in the tests of both wrappers, has a plugin report what the processor does
with a subnormal number from inside `process`, then checks that the host's
thread is back to its own setting afterwards, including after the plugin
panics. Measured on the example effect, 4,000 blocks of subnormal input took
442 ms without the guard and 31 ms with it.

**Limit.** `x86_64` and `aarch64` only; elsewhere the guard does nothing.
Rust does not define floating-point behaviour under a changed setting, and
[ADR 0006](adr/0006-subnormal-numbers-are-flushed.md) says what that means
in practice and why the trade is taken. A program that calls a processor in
its own loop, outside the wrappers, streams and test harness, creates the
guard itself.

## 11. Hosting a plugin: thread rules in the types, every event on its frame

**The problem.** A host calls a plugin from two kinds of thread, and the
formats say which calls belong where. Getting it wrong crashes plugins in the
field, and nothing stops a host from getting it wrong. A host that wants to
pass automation to a plugin precisely also needs a way to say on which frame
each change happens.

**In JUCE.** JUCE 9.0.4 does not host CLAP plugins. For the formats it hosts,
a plugin is one
[`AudioPluginInstance`](https://docs.juce.com/master/classAudioPluginInstance.html),
an [`AudioProcessor`](https://docs.juce.com/master/classAudioProcessor.html)
that the host calls from the message thread and the audio thread; which
method may run where is set out in the documentation, and the compiler
checks none of it. The hosting interface is `processBlock(AudioBuffer,
MidiBuffer)`: MIDI events carry a sample position, but a parameter change,
made through
[`AudioProcessorParameter::setValue`](https://docs.juce.com/master/classAudioProcessorParameter.html),
carries none, so a host can place a change on an exact frame only by cutting
the block there. JUCE has announced sample-accurate automation for a later
JUCE 9 update.

**In Manguebyte.** `manguebyte-host` loads CLAP and VST 3 plugins in two
halves. The main-thread half (parameters, state, activation) is not `Send`,
so code that would load state from the audio thread does not compile; the
audio half is `Send`, is the only thing that can process, and processes
through `&mut self`. Every event of the framework's event buffer reaches the
plugin on its frame: CLAP events with their offsets, and for VST 3 one
queue per parameter with every point on its frame, notes with ids, per-note
pressure and note expression.

**Check it.** The compile-fail doc-tests on `ClapPlugin` and `Vst3Plugin`
show that the main-thread half cannot be sent to another thread.
`the_hosted_plugin_sounds_exactly_as_the_plugin_does_alone`, in
[`clap_pluck.rs`](../crates/manguebyte-host/tests/clap_pluck.rs) and
[`vst3_pluck.rs`](../crates/manguebyte-host/tests/vst3_pluck.rs), loads the
example instrument through its real entry point and requires its output to
be identical bit for bit to the same plugin run in process by `Harness`,
for four block layouts, with automation of five parameters, notes with ids,
expression and pitch bend; removing any one kind of event changes the
output, so the identity is not two deaf paths agreeing.
`automation_lands_on_its_frame_through_the_host` sends 59 VST 3 points 61
frames apart.

**Limit.** A second audio half, and deactivating while one exists, are
refused at run time, not at compile time. "Main thread" means the thread
that created the plugin; the types do not check that it is the program's
main thread. At most 4,096 events per call; the rest are counted. No
modulation is sent to VST 3 plugins. Tested with the framework's own
plugins and deliberately misbehaving test plugins, not yet with plugins
from other vendors; no editors of hosted plugins. Scanning lists CLAP and
VST 3 files out of process and LV2 bundles from their Turtle files, and has
not met plugins from other vendors either.

## 12. Loudness, true peak and broadcast meters, ready to use

**The problem.** Anything that delivers audio for broadcast or streaming has
to measure loudness to ITU-R BS.1770 and EBU R 128, and true peak. Getting
the gating, the windows and the K-weighting right at every sample rate is
easy to get subtly wrong.

**In JUCE.** JUCE 9.0.4's documented API has no loudness, LUFS, K-weighting
or true-peak class or function, and no VU, PPM, correlation or balance meter
([class list](https://docs.juce.com/master/annotated.html)). Its level tools
are `AudioBuffer::getRMSLevel` and `getMagnitude`,
[`AudioDeviceManager::LevelMeter`](https://docs.juce.com/master/structjuce_1_1AudioDeviceManager_1_1LevelMeter.html)
and [`dsp::BallisticsFilter`](https://docs.juce.com/master/classjuce_1_1dsp_1_1BallisticsFilter.html).
Users on the forum write their own
([loudness](https://forum.juce.com/t/does-juce-have-any-loudness-related-functions-or-classes/57424),
[true peak](https://forum.juce.com/t/does-juce-have-a-function-to-read-true-peak/43454)).

**In Manguebyte.** `manguebyte-analysis` has a `LoudnessMeter` (momentary,
short-term, integrated, loudness range, maxima), a `TruePeakMeter`
(BS.1770-4 Annex 2, 4x by default), a VU meter, a PPM with IEC 60268-10
Type I timing, and a correlation and balance meter. They run on the audio
thread in memory fixed when they are built, and any thread reads them
through atomics without waiting.

**Check it.** [`ebu_tech_3341.rs`](../crates/manguebyte-analysis/tests/ebu_tech_3341.rs)
synthesises the EBU Tech 3341 test signals and requires cases 1 to 6 and 9
to 14 within 0.1 LU, and the true-peak cases 15 to 23 within +0.2/-0.4 dB;
[`ebu_tech_3342.rs`](../crates/manguebyte-analysis/tests/ebu_tech_3342.rs)
does the same for loudness range, cases 1 to 4. The VU needle reaches 99% in
300 ms ±3 ms with 1.0 to 1.5% overshoot at 44.1 to 192 kHz. Readings are
identical to the bit whatever the block size, and measuring makes no heap
call.

**Limit.** The signals are synthesised from the documents' tables; the
EBU's own files and recorded items were not run. Written to BS.1770-4, which
the ITU has since superseded with BS.1770-5. The loudness range is kept in
0.01 LU histograms and can differ from Tech 3342's function when short-term
values straddle the relative gate within one bin; this is documented and
tested. The PPM matches Type I at its 5 ms integration point and its return
time; other burst readings miss the nominal values by up to 0.6 dB, and it
reads samples, not the waveform. JUCE ships the pieces to build such meters;
the claim is that it ships none ready.

## 13. Open Sound Control that lands on the sample

**The problem.** OSC bundles carry a time tag so that a controller or a
live-coding program can say when something should happen. An audio engine
that applies messages when they arrive, or once per block, throws that
timing away.

**In JUCE.** [`OSCReceiver`](https://docs.juce.com/master/classOSCReceiver.html)
calls its listeners on the message thread or directly on the network thread;
its documentation describes no delivery into the audio callback and no
scheduling by time tag. [`OSCArgument`](https://docs.juce.com/master/classOSCArgument.html)
documents five argument types (int32, float32, string, blob, colour).
[`OSCAddressPattern`](https://docs.juce.com/master/classOSCAddressPattern.html)
documents the OSC 1.0 wildcards, and
[`OSCSender`](https://docs.juce.com/master/classOSCSender.html) and
`OSCReceiver` document UDP only.

**In Manguebyte.** `manguebyte-osc` decodes on a network thread and hands
fixed events to the audio thread through wait-free queues, and a message in
a time-tagged bundle lands on the frame nearest its time tag. Every argument
type the OSC 1.0 specification lists is supported, nested arrays included;
the `//` wildcard proposed for OSC 1.1
([Freed and Schmeder, 2009](https://ccrma.stanford.edu/groups/osc/spec-1_1.html))
is matched; and TCP with the OSC 1.1 SLIP framing sits beside UDP.

**Check it.** [`bridge.rs`](../crates/manguebyte-osc/tests/bridge.rs):
`a_time_tag_lands_on_its_exact_frame`, `every_frame_of_a_block_can_be_hit`,
and `where_an_event_lands_does_not_depend_on_the_block_size` (400 events,
seven block patterns, identical frames); reading on the audio thread makes
no heap call. [`codec.rs`](../crates/manguebyte-osc/tests/codec.rs) encodes
the specification's examples byte for byte and round-trips every type.

**Limit.** Exact relative to the block start the caller passes; the
variant that reads the system clock inherits the callback's jitter and any
offset between the sender's clock and this one. Fixed capacities (1,024
events, a pool for messages over 64 bytes); what does not fit is dropped
whole and counted. OSC 1.1 is a conference paper, not a ratified
specification, and `//` is read as XPath's descendant step. OSC 1.0's
size-prefixed stream framing is not implemented. A JUCE user can build the
same path by hand; JUCE ships none.

## 14. Pitch detection and fractional-octave analysis

**In JUCE.** JUCE 9.0.4's documented class list has no pitch detection and
no fractional-octave band analyser or A- and C-weighting class
([classes](https://docs.juce.com/master/classes.html)); it provides
`dsp::FFT` and `dsp::WindowingFunction` as building blocks, and spectrum
display appears in its tutorials as code the reader writes.

**In Manguebyte.** `PitchDetector` offers YIN and the McLeod pitch method
with a confidence value and the nearest note named, refined between samples
by band-limited interpolation. `SpectrumAnalyzer` gives octave to
twenty-fourth-octave bands on the exact frequencies of IEC 61260-1 and A and
C weighting from IEC 61672-1, with ballistics and peak hold, handing whole
frames to an interface through a wait-free triple buffer.

**Check it.** [`pitch.rs`](../crates/manguebyte-analysis/tests/pitch.rs):
sines, sawtooths, square waves, tones without their fundamental and tones
with a strong second harmonic from 55 Hz to 2 kHz read within 2 cents at
44.1 and 48 kHz, with no octave errors.
[`spectrum.rs`](../crates/manguebyte-analysis/tests/spectrum.rs): a
full-scale sine reads 0 dB in its band within 0.01 dB, and the third-octave
bands of white and pink noise add up within 0.05 dB.

**Limit.** Synthetic, steady, single tones. At 8 to 22 kHz sample rates the
error grows to 13 cents; a second harmonic more than about four times the
fundamental makes the detector read an octave high; low-frequency noise can
read a confidence near 0.7. The bands are formed from FFT bins, not an IEC
61260-1 filter bank, and bands narrower than about four bins are not
resolved. Third-party pitch modules for JUCE exist.

## 15. A keyboard an interface can read without listening

**In JUCE.** [`MidiKeyboardState`](https://docs.juce.com/master/classMidiKeyboardState.html)
reports key changes to its listeners synchronously, on whichever thread
updates it, and its documentation warns that the callback
"[could happen from an audio callback thread](https://docs.juce.com/master/classMidiKeyboardState_1_1Listener.html)".
An interface can poll `isNoteOn()` instead, but the documentation gives no
guarantee that updates or reads are free of locks or allocation, and the
class tracks keys, not pedals.

**In Manguebyte.** `midi::keyboard::KeyboardState` keeps keys, velocities
and the sustain pedal in atomics. The audio thread updates it with no
listener code, no lock and no heap call; an interface polls it. A reader
never sees a key that was not struck, and never sees a sustained key go
silent while it passes from the hand to the pedal.

**Check it.** `the_keyboard_state_does_not_use_the_heap` in
[`realtime.rs`](../crates/manguebyte-midi/tests/realtime.rs) (20,000 random
messages, reads interleaved), and
`a_key_that_goes_on_sounding_is_never_read_as_silent` and
`an_interface_can_read_while_another_thread_writes` in
[`keyboard.rs`](../crates/manguebyte-midi/src/keyboard.rs).

**Limit.** One writing thread. A read is consistent within each group of 16
keys, not across the whole keyboard, and a velocity can briefly lead or lag
its key. Only the sustain pedal is tracked. "No lock" and "no listener"
follow from the code (atomic operations only, no callback interface), not
from a test.

## 16. Instruments ready to play, measured in tests

**In JUCE.** JUCE gives the parts to build an instrument from:
[`Synthesiser`](https://docs.juce.com/master/classSynthesiser.html) and
[`SynthesiserVoice`](https://docs.juce.com/master/classSynthesiserVoice.html),
a basic sample player in
[`SamplerSound`](https://docs.juce.com/master/classSamplerSound.html) and
[`SamplerVoice`](https://docs.juce.com/master/classSamplerVoice.html), and
building blocks such as
[`dsp::Oscillator`](https://docs.juce.com/master/classdsp_1_1Oscillator.html)
and [`dsp::LadderFilter`](https://docs.juce.com/master/classdsp_1_1LadderFilter.html).
Its [class list](https://docs.juce.com/master/annotated.html) has no
instrument ready to play: no modelled acoustic instrument, no drum machine,
no synthesiser with presets.

**In Manguebyte.** `manguebyte_dsp::instruments` holds voices for the
polyphonic `Synth` that play as they are: plucked guitars and basses,
four-operator FM, a tonewheel organ with a rotary speaker, mallets by modal
synthesis, bowed strings and winds by digital waveguide, choirs by formant
synthesis, an acid bass with accent and slide, drum kits (acoustic drums and
cymbals by modal synthesis, and the 808, 909, 606 and CR-78 with genre kits),
and a deep polyphonic synthesiser with 68 presets. Every one of them takes
velocity and accents, is free of the heap while it plays, and renders the
same samples whatever the block size.

**Check it.** Tuning is measured across each instrument's range:
`every_note_is_in_tune_across_the_keyboard` in
[`poly/tests.rs`](../crates/manguebyte-dsp/src/instruments/poly/tests.rs)
(within 0.012 cent),
`every_note_is_within_a_cent_across_the_keyboard` in
[`string.rs`](../crates/manguebyte-dsp/src/instruments/string.rs),
`every_bowed_note_is_in_tune_across_its_range` in
[`bowed.rs`](../crates/manguebyte-dsp/src/instruments/bowed.rs) and the
tuning tests of the mallets, winds, choir, organ, FM and
acid voices. Spectra are measured too:
`every_mode_rings_within_half_a_percent_of_its_model` in
[`mallet.rs`](../crates/manguebyte-dsp/src/instruments/mallet.rs),
`the_formants_peak_at_the_vowels_targets` in
[`choir.rs`](../crates/manguebyte-dsp/src/instruments/choir.rs),
`high_notes_are_kept_free_of_aliasing` in
[`fm.rs`](../crates/manguebyte-dsp/src/instruments/fm.rs) and
[`poly/tests.rs`](../crates/manguebyte-dsp/src/instruments/poly/tests.rs),
and `the_snare_measures_as_the_one_recorded` and
`the_crash_measures_as_the_one_recorded` in
[`kit.rs`](../crates/manguebyte-dsp/src/instruments/kit.rs), which compare
the modelled snare and crash with recordings of real ones. The allocation
tests are in [`realtime.rs`](../crates/manguebyte-dsp/tests/realtime.rs).

**Limit.** These numbers are measurements, not opinions: nobody outside the
project has judged how the instruments sound yet. Only the snare and the
crash are calibrated against recordings, from lossy files. The low notes of
the bowed cello and double bass take up to half a second to settle, the
saxophone's bore is cylindrical, the choir sings speech vowels and no
consonants, and the instruments' levels are matched only in the browser
engine, not in `manguebyte-dsp` itself; [TECH_DEBT.md](TECH_DEBT.md) items
66 to 71 list the rest.

## 17. Changing the audio device in the middle of a song

**The problem.** Plugging in headphones, moving from the laptop's speakers
to an interface, or a driver that asks to be reset should not stop the
music. In most applications it does: the audio stops, everything is
prepared again for the new device, and some programs ask to be restarted.

**In JUCE.** An [`AudioIODeviceCallback`](https://docs.juce.com/master/classAudioIODeviceCallback.html)
is told `audioDeviceStopped` when its device stops, and
`audioDeviceAboutToStart`, with the new device, before it starts again,
which the documentation says also happens "after the device has been
restarted because of a sample-rate or block-size change". Changing the
device through [`AudioDeviceManager::setAudioDeviceSetup`](https://docs.juce.com/master/classAudioDeviceManager.html)
goes through that pair, and `audioDeviceAboutToStart` is where a callback
that plays a processor, such as
[`AudioProcessorPlayer`](https://docs.juce.com/master/classAudioProcessorPlayer.html),
has to prepare it again for the new rate and block size. Nothing in the
documented classes keeps one device playing while the next one starts, or
runs the processing at a rate of its own while the device's rate changes.

**In Manguebyte.** `DeviceManager::start_engine` fixes the engine's rate,
largest block and channels once, as an `EngineFormat`, and prepares the
callback once. Only the edge between the engine and the device adapts:
a device at the engine's rate gets its output directly, with nothing
copied or delayed; one at another rate gets it through a resampler whose
output does not depend on the device's blocks (0.7 ms more); other channel
counts are repeated or folded. When the backend can run two streams and
the output device changes, the new device fades in while the old one fades
out, the old device copying what the engine renders into a wait-free ring
until it hands the engine itself over; no audio thread allocates, locks or
waits. When it cannot (one ASIO driver per process, the same device opened
again, a driver's reset request), the old device fades out, closes, and the
new one opens and fades in, the engine waiting so that no frame is lost. A
lost device is replaced the same way, without the fade-out. The
[`hotswap`](../crates/manguebyte-devices/src/hotswap.rs) module documents it.

**Check it.** `the_song_goes_on_while_the_device_changes_under_it` in
[`hotswap.rs`](../crates/manguebyte-devices/tests/hotswap.rs) plays a song
through the manager on four simulated sound cards, each called on its own
clock in real time, and changes the output under it four times: to another
device at the same rate with a smaller block, to one at 44.1 kHz, through
that driver's reset request, and to one with four channels and blocks of
64 frames. The callback is prepared once and never released; what it
renders is the song rendered with no device, bit for bit; each device at
the engine's rate plays one unbroken stretch of the engine's frames,
exactly, scaled only inside its fades; a reference tone never breaks on
any device, the level never dips while two devices play, and the reset
leaves under 100 ms of silence (9 ms when it was measured); the devices'
thread never touches the heap.
`devices_that_cannot_overlap_hand_over_after_a_short_faded_gap` shows the
second device starting on the very frame after the last one the first
played, `a_device_at_another_rate_plays_the_same_whatever_its_blocks` the
resampled edge's independence from the device's blocks, and
`a_lost_device_is_replaced_without_preparing_again` a device unplugged.
On real drivers, `the_engine_runs_on_while_the_driver_changes` in
`manguebyte-pro-io`'s `hardware.rs` moves a running engine from FlexASIO
to ASIO4ALL and back, playing silence.

**Limit.** Two devices crossfade only when the backend can run both and
they are different devices; otherwise the gap is as long as the backend
takes to close one stream and open the next: 90 ms for the WASAPI device
of the machine it was tried on, and 0.37 to 2.6 s between two ASIO
drivers, which load slowly. During a gap the engine waits, so the song
resumes late by it. The crossfade has been run on the headless backend
only, the machine having one output, and its level can rise by up to
3 dB while the fades are a block apart. [TECH_DEBT.md](TECH_DEBT.md) item
85 lists the rest.

## Where JUCE is ahead

JUCE has been in production for more than twenty years, and it shows. Today
it has, and Manguebyte does not:

- a complete GUI toolkit, with plugin editor windows on macOS and Linux as
  well as Windows (Manguebyte's editors open on Windows only so far);
- the AUv3 and AAX plugin formats, and Audio Units checked in real hosts
  (Manguebyte's AUv2 wrapper has only its own tests and `auval` in CI);
- the editors of hosted plugins (Manguebyte hosts CLAP, VST 3, LV2 and
  Audio Unit plugins, without their editors, and scans for them);
- SIMD wrappers and lookup tables among its DSP utilities;
- iOS and Android (Manguebyte's audio and device crates are built for both
  in CI, but not run on a device);
- documentation, tutorials and a large community.

[JUCE_PARITY.md](JUCE_PARITY.md) tracks that gap module by module, and
[ROADMAP.md](../ROADMAP.md) says in what order it is being closed.
