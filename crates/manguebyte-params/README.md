# manguebyte-params

Thread-safe parameters for the Manguebyte Audio Framework: ranges, skew, text conversion, smoothing and state.

Part of the [Manguebyte Audio Framework](https://github.com/alessonqueirozdev-hub/manguebyte). Most projects depend on
[`manguebyte`](https://crates.io/crates/manguebyte), which gathers every part
under one name; depend on this crate directly to use only this part.

A parameter's value is an atomic and every method takes `&self`, so a set
is shared between the audio thread, an editor and a host with no locks
(ADR 0003). Its display name, short name and unit may change while it
runs (`ParamInfo::set_display_name`, `set_short_name`,
`set_display_unit`, ADR 0020): they sit behind a lock taken off the
audio thread only, while `labels_version` counts the changes in an atomic
that the plugin wrappers poll to tell the host, once per batch. The id
never changes; `name` and `unit` stay what the parameter was made with.

Documentation: <https://docs.rs/manguebyte-params>

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
