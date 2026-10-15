//! Golden images and dumps of the test plugin's editor, opened through the
//! real CLAP interface at scale 1 and 2.
//!
//! The image is what the editor's window shows, read from the headless
//! window's back buffer after presenting; the dump is the exact scene it
//! was drawn from. Each dump must match exactly; images may differ by up to
//! [`TOLERANCE`] in any channel, which `manguebyte-gfx` allows for the
//! rasteriser's rounding on other processors. To regenerate the files after
//! an intended change, run locally:
//!
//! ```text
//! MANGUEBYTE_BLESS=1 cargo test -p manguebyte-gui --test golden
//! ```
//!
//! and look at the new images before committing them. With `CI` set,
//! blessing is refused.

mod common;

use std::path::PathBuf;

use manguebyte_gfx::Point;
use manguebyte_gfx::snapshot::Snapshots;
use manguebyte_ui::Role;

use common::*;

/// The same tolerance as the goldens of `manguebyte-gfx` and
/// `manguebyte-ui`.
const TOLERANCE: u8 = 1;

fn snapshots() -> Snapshots {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let failures = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("gui-golden-failures");
    Snapshots::new(dir, failures).with_tolerance(TOLERANCE)
}

#[test]
fn the_test_plugins_editor_matches_its_goldens_at_1x_and_2x() {
    let snapshots = snapshots();
    for (name, scale) in [("editor@1x", 1.0), ("editor@2x", 2.0)] {
        let session = Session::new();
        session.start();
        let ui = session.ui();
        let editor = ui.open(Some(scale));
        // A block at the default gain publishes -6 dB; the first frame
        // draws everything.
        session.process();
        editor.tick();
        // The knob dragged 40 logical pixels up, to 0.7, and still held, so
        // it has the keyboard focus.
        let knob = get(&editor, Role::Slider, "Gain").bounds.center();
        editor.press(knob);
        editor.move_to(Point::new(knob.x, knob.y - 40.0));
        session.process();
        editor.tick();
        assert_eq!(knob_text(&editor).as_deref(), Some("0.70"));
        assert_eq!(
            get(&editor, Role::Meter, "Output").node.value.as_deref(),
            Some("-3.1 dB")
        );

        let shown = editor.shown().expect("open");
        // The window shows exactly what the interface drew, after several
        // partial frames.
        assert!(Some(&shown) == editor.pixels().as_ref(), "{name}");
        let dump = editor.dump().expect("open");
        if let Err(mismatch) = snapshots.check(name, &shown, &dump) {
            panic!("{mismatch}");
        }
        editor.release(Point::new(knob.x, knob.y - 40.0));
        ui.destroy();
    }
}
