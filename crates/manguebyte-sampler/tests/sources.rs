//! Where samples come from: files relative to the `.sfz` file, including
//! paths that climb out of its directory, and bytes in memory.

mod common;

use std::path::PathBuf;

use common::*;
use manguebyte_sampler::{Instrument, MemorySource};

/// Removes a directory when dropped, so a failing test leaves nothing.
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn sample_paths_that_climb_out_of_the_sfz_directory_are_found() {
    // A common library layout: Programs/piano.sfz refers to
    // ../Samples/a.wav.
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("sampler_sources_{}", std::process::id()));
    let _cleanup = Cleanup(root.clone());
    std::fs::create_dir_all(root.join("lib/Programs")).unwrap();
    std::fs::create_dir_all(root.join("lib/Samples")).unwrap();
    std::fs::write(
        root.join("lib/Samples/a.wav"),
        sine_wav(440.0, 48_000, 0.1, 0.5),
    )
    .unwrap();
    // A file of the same name where the rewritten path used to point, so
    // that finding it is not mistaken for success.
    std::fs::create_dir_all(root.join("lib/Programs/Samples")).unwrap();
    std::fs::write(root.join("lib/Programs/Samples/a.wav"), b"not audio").unwrap();
    std::fs::write(root.join("lib/common.sfz"), "<global> volume=-6\n").unwrap();
    for text in [
        "<region> sample=../Samples/a.wav",
        "<region> sample=..\\Samples\\a.wav",
        "<control> default_path=../Samples/\n<region> sample=a.wav",
        "<control> default_path=..\\Samples\n<region> sample=a.wav",
        "#include \"../common.sfz\"\n<region> sample=Samples/../../Samples/a.wav",
    ] {
        let sfz = root.join("lib/Programs/piano.sfz");
        std::fs::write(&sfz, text).unwrap();
        let instrument = Instrument::open(&sfz).unwrap_or_else(|error| panic!("{text:?}: {error}"));
        assert_eq!(instrument.num_samples(), 1, "{text:?}");
        assert_eq!(instrument.sample_bytes(), 4_800 * 4, "{text:?}");
    }
}

#[test]
fn a_memory_source_finds_climbing_paths_under_their_own_names() {
    let tone = sine_wav(440.0, 48_000, 0.1, 0.5);
    let mut source = MemorySource::new().with("../Samples/a.wav", tone.clone());
    for text in [
        "<region> sample=../Samples/a.wav",
        "<control> default_path=../Samples/\n<region> sample=a.wav",
    ] {
        let instrument = Instrument::from_sfz(text, &mut source).unwrap();
        assert_eq!(instrument.num_samples(), 1);
    }
    // Without the `../` it is another name, and a source holding only that
    // one does not answer the climbing path.
    let mut source = MemorySource::new().with("Samples/a.wav", tone);
    let error = Instrument::from_sfz("<region> sample=../Samples/a.wav", &mut source).unwrap_err();
    assert!(error.to_string().contains("../Samples/a.wav"), "{error}");
}
