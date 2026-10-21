//! The scanner process of `manguebyte_host::scan`: started by a `Scanner`
//! as `manguebyte-scan --manguebyte-scan <clap|vst3|lv2|au> <path>`, it lists
//! the plugins of one file on its standard output and exits. A plugin that
//! crashes or hangs takes this process down, not the host that started it.

use std::process::ExitCode;

#[cfg(any(feature = "clap", feature = "vst3", feature = "lv2", feature = "au"))]
fn main() -> ExitCode {
    use manguebyte_host::scan::{SCAN_ARGUMENT, run_scanner};

    let arguments: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    match arguments.split_first() {
        Some((first, rest)) if first == SCAN_ARGUMENT => {
            // SAFETY: this process exists to load the file it is given and
            // to take the consequences: it does nothing else, and exits as
            // soon as it has answered.
            unsafe { run_scanner(rest) }
        }
        _ => {
            eprintln!(
                "manguebyte-scan lists the plugins of one file for a host; it is not run by \
                 hand.\nusage: manguebyte-scan {SCAN_ARGUMENT} <clap|vst3|lv2|au> <path>"
            );
            ExitCode::from(2)
        }
    }
}

#[cfg(not(any(feature = "clap", feature = "vst3", feature = "lv2", feature = "au")))]
fn main() -> ExitCode {
    eprintln!("manguebyte-scan was built without any plugin format");
    ExitCode::from(2)
}
