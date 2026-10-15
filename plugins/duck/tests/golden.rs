//! Golden images and scene dumps of Mangue Duck's editor at scale 1 and 2.
//!
//! The editor is opened headless through the harness and `manguebyte-gui`,
//! the plugin ducks a loud sidechain by its default 12 dB, and the depth
//! knob is held in the middle of a drag to 20 dB, so the images show a
//! moved knob, the keyboard focus ring and a full meter. Each dump must
//! match exactly; images may differ by up to [`TOLERANCE`] in any channel,
//! which `manguebyte-gfx` allows for the rasteriser's rounding on other
//! processors. To regenerate the files after an intended change, run
//! locally:
//!
//! ```text
//! MANGUEBYTE_BLESS=1 cargo test -p mangue-duck --test golden
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

/// The same tolerance as the goldens of `manguebyte-gfx`, `manguebyte-ui`
/// and `manguebyte-gui`.
const TOLERANCE: u8 = 1;

fn snapshots() -> Snapshots {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let failures = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("duck-golden-failures");
    Snapshots::new(dir, failures).with_tolerance(TOLERANCE)
}

#[test]
fn the_editor_matches_its_goldens_at_1x_and_2x() {
    let snapshots = snapshots();
    for (name, scale) in [("duck@1x", 1.0), ("duck@2x", 2.0)] {
        let mut open = Open::new(Some(scale));
        open.process(Some(1.0), 24_000);
        let depth = open.get(Role::Slider, "Depth").bounds.center();
        open.ui.press(depth);
        open.ui.move_to(Point::new(depth.x, depth.y - 40.0));
        open.process(Some(1.0), 24_000);
        open.ui.tick();
        assert_eq!(open.text(Role::Slider, "Depth"), "20.0 dB", "{name}");
        assert_eq!(open.text(Role::Meter, "Reduction"), "20.0 dB", "{name}");

        let shown = open.ui.shown().expect("open");
        assert_eq!(
            (shown.width(), shown.height()),
            ((440.0 * scale) as u32, (210.0 * scale) as u32)
        );
        // The window shows exactly what the interface drew, after a full
        // frame and partial ones.
        assert!(Some(&shown) == open.ui.pixels().as_ref(), "{name}");
        let dump = open.ui.dump().expect("open");
        if let Err(mismatch) = snapshots.check(name, &shown, &dump) {
            panic!("{mismatch}");
        }
        open.ui.release(Point::new(depth.x, depth.y - 40.0));
    }
}
