# ADR 0012: Plugin identity at run time

Status: accepted, 2026-10-29

## Context

A plugin's identity is what hosts know it by: an id, which sessions
store, and a name, a vendor and a version, which people see. Until now it
was `Plugin::INFO`, a constant of type `PluginInfo` with `&'static str`
fields, fixed when the plugin is compiled. The CLAP descriptor, the VST 3
factory and the class id derived from the id, the header of saved state,
an Audio Unit's codes, the LV2 URI and Turtle, and the test `Harness` all
read it as a constant.

Parabolic, a separate product built on the framework, exports plugins
without a compiler. Every exported plugin is the same pre-built engine,
one `cdylib` with `export_clap!` and `export_vst3!`, byte for byte, with
the instrument appended after the end of the file (its ADR 0001). Two
exported plugins are two copies of that file at different paths, and each
must be a plugin of its own, with its own id (its ADR 0002). The engine
knows who it is only once it has read its own file, so:

- the identity has to be worked out when the library is loaded, before
  any instance exists, since a host lists plugins by asking the factory;
- the library has to know which file it was loaded from;
- the formats hand hosts pointers that must stay valid while the library
  is loaded, CLAP's descriptor and LV2's URI among them;
- two copies loaded in one host must not be confused: not their ids,
  their class ids, their saved state nor their editors' windows;
- Parabolic's crates forbid `unsafe`, so whatever needs a system call
  lives in the framework.

## Decision

**1. `Plugin::info()`, with `INFO` as its default.** The trait gains

```rust
fn info() -> PluginInfo where Self: Sized { Self::INFO }
```

A plugin that knows its identity when compiled writes nothing. One whose
identity is data overrides `info`, and may read files, allocate and leak
the strings it builds: it is called once per process.

**2. One cached identity per plugin type.** `plugin_info::<P>()` calls
`P::info()` the first time and returns the same `&'static PluginInfo` for
the life of the library, from any thread. The cache is a list of
`(TypeId, &'static OnceLock<PluginInfo>)`, each cell leaked once, behind a
mutex taken only to find the cell; `info` runs outside it, so it may take
its time or ask for another type's identity, and runs exactly once even
when several threads ask at once. A panic in `info` leaves the cell empty,
and the next caller asks again. Every place that reported `P::INFO` at run
time now reads `plugin_info::<P>()`:

| Where | What comes from it |
|---|---|
| CLAP descriptor (`manguebyte-clap`) | id, name, vendor, version, description; built on the first `get_plugin_descriptor` and leaked, as before |
| CLAP `create_plugin` | the id it accepts |
| VST 3 factory (`manguebyte-vst3`) | `getFactoryInfo`'s vendor, every class info, and the class id, `class_id(info.id)`, worked out once when `plugin_factory` makes the factory |
| Saved state (`save_state`, `load_state`) | the id in the header, and the id state must carry to load |
| Audio Unit (`manguebyte-au`) | the subtype and manufacturer codes in the state dictionary and in the `Info.plist` that `export_au!`'s hidden function writes |
| LV2 (`manguebyte-lv2`) | the URI of the descriptor, now built on first use and leaked like CLAP's, and of the Turtle files |
| `Harness` | the id it checks and names in its messages |

The wrappers call it when a host first asks about the plugin, never on
the audio thread.

**3. What stays compile-time.** `INFO` stays a constant, and two things
are read from it alone:

- the **kind**: `plugin_info` always takes `INFO.kind`, whatever `info`
  returns, because the buses (`LAYOUT`), CLAP's features, VST 3's
  sub-categories and its hidden MIDI controller parameters (worked out in
  a `const` context), and an Audio Unit's type code all follow it, and
  must agree with each other;
- the **whole identity when `info` gives an id that cannot be used**:
  empty, or with white space or control characters (`is_valid_id`), which
  saved state, CLAP's C strings and session files could not carry.

`cargo mangue` does not read `INFO` either: the Audio Unit `Info.plist`
and LV2 Turtle files are written by small programs it builds against the
plugin's crate, which now call `plugin_info` too. A plugin whose identity
depends on where its library is gets, in those programs, whatever its
`info` makes of the helper program's own file, usually its fallback, so
an exporter that makes copies with other identities must write those
files for each copy itself (`manguebyte_au::info_plist` takes any
`PluginInfo`).

**4. The library's own path.** `library_path() -> Option<&'static Path>`
in `manguebyte-plugin` gives the file the plugin's code was loaded from,
decided once for the life of the library by the first of:

- CLAP's `clap_entry.init`, which hosts call before anything else with
  the path of the plugin; the wrapper records it with `set_library_path`
  when it names a file (made absolute). On macOS hosts pass the bundle's
  folder, which is not recorded;
- the system, asked by address on the first call:
  `manguebyte_core::module::file` passes the address of a static of its
  own, which is linked into the same binary as the wrapper, to
  `GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS |
  GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT)` and
  `GetModuleFileNameW` on Windows, and to `dladdr` on macOS, Linux and
  the BSDs. This is what VST 3, LV2 and Audio Units get, since their
  factory functions are given no path (LV2 gives the bundle's folder to
  `instantiate`, after the descriptor already exists).

It is always the library's file, never a bundle folder; on macOS its
bundle's resources are `../Resources` from it. Because a wrapper calls
`plugin_info` only after the host's first call into it, the path is known
inside `info`. `manguebyte-plugin` keeps `#![forbid(unsafe_code)]`; the
two system calls are in `manguebyte-core`, each with a `SAFETY` comment,
and return `None` under Miri and on WebAssembly.

**5. State follows the run-time id.** Saved state names the id
`plugin_info` gives, and `load_state` takes only state with that id. Two
copies that work out different ids refuse each other's state with
`StateError::WrongPlugin`, as two different plugins do; a copy whose
identity falls back to `INFO` takes state saved under `INFO`'s id and
nothing else. There is no list of other ids a plugin also accepts.

**6. Proved by two copies in one process.** `plugins/identity-probe`
(`publish = false`) reads its id, name and vendor from bytes appended to
its own file. Its test copies the built library to two folders under one
file name, appends two identities, and through `manguebyte-host` lists two
CLAP descriptors and two VST 3 class ids, processes audio in each with its
own gain, saves and restores each one's state and refuses the other's,
and, on Windows, has both editors open at once in real top-level windows,
through CLAP and through VST 3, in window classes registered by two
modules (the class names already carry the module's address, ADR 0007,
decision 7). A third copy with nothing appended is the compiled plugin.
Each wrapper also has a test in process with a plugin whose `info`
differs from `INFO`.

## Consequences

- Nothing that compiled breaks: `info` has a default, and every plugin
  that does not override it reports exactly what it did.
- One engine binary can be many plugins, which is what an exporter
  without a compiler needs; the framework's `cargo mangue` `layout::plan`
  and `bundle::write` copy such a library as it is and are documented as
  public API for that use.
- The first host call that reports the identity may take as long as
  `info` does, inside a host's scan.
- The identity cannot change while a library is loaded. A host that
  unloads a library and loads it again gets `info` asked again.
- Saved state of a plugin that changes its id (a copy given a new
  identity) does not load; that is the rule, not an accident.
- An Audio Unit's `Info.plist` and LV2's Turtle are files in the bundle,
  so a run-time identity reaches them only if whoever makes the bundle
  writes them for it (TECH_DEBT item 100).
- Appending bytes to a signed macOS binary breaks its signature; there an
  exporter keeps the data in the bundle's `Resources`, and the two-copy
  test does not run on macOS.

## Alternatives not taken

- **A `&'static PluginInfo` returned by `info` itself.** It would push the
  caching onto every plugin; one cache in the framework makes "asked
  once" a guarantee.
- **A runtime `kind`.** The layout, VST 3's controller parameters and the
  Audio Unit type would all have to follow it, which is a different
  plugin, not a different identity.
- **A process-wide registry keyed by id.** Each copy of a library has its
  own statics, which is exactly what keeps two copies apart; nothing needs
  to be shared between them.
- **Reading the path only from the formats' entry points.** VST 3, LV2 and
  Audio Units do not give it before the factory is asked, and CLAP's on
  macOS is a folder; asking the system by address works for all of them.
