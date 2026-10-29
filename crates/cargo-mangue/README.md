# cargo-mangue

The command-line tool of the Manguebyte Audio Framework: create, bundle, install and validate audio plugins.

Install it with:

```sh
cargo install cargo-mangue
```

Then `cargo mangue new my-plugin` starts a plugin, and `cargo mangue bundle`,
`install` and `validate` package it as CLAP, VST 3, LV2 or, on macOS, an
Audio Unit, put it where hosts look, and run the official validators
(`clap-validator`, Steinberg's validator and Apple's `auval`; LV2 has none
here).

As a library, `layout::plan` (which files a bundle holds) and
`bundle::write` (writing them, copying the library as it is) are public
API for programs that bundle plugins without cargo, such as an exporter
that copies one pre-built engine with data appended to it (ADR 0012).

Part of the [Manguebyte Audio Framework](https://github.com/alessonqueirozdev-hub/manguebyte).

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
