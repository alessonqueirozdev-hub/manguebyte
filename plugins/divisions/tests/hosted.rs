//! The library cargo built, loaded as a host loads it, as a CLAP and as a
//! VST 3 plugin: the buses its exports present, and each division heard on
//! its own bus.
#![cfg(not(miri))]

use std::env::consts::{DLL_PREFIX, DLL_SUFFIX};
use std::path::PathBuf;

use manguebyte::dsp::event::{Event, EventBuffer, EventKind, NoteRef};
use manguebyte::prelude::*;
use manguebyte_host::clap::ClapLibrary;
use manguebyte_host::vst3::Vst3Module;

/// This package's own shared library, which cargo builds next to the test.
fn built_library() -> PathBuf {
    let executable = std::env::current_exe().expect("the test executable has a path");
    let folder = executable.parent().expect("it is in a folder");
    let file = folder.join(format!("{DLL_PREFIX}mangue_divisions{DLL_SUFFIX}"));
    assert!(file.is_file(), "cargo has not built {}", file.display());
    file
}

const NAMES: [&str; 4] = ["Great", "Swell", "Choir", "Pedal"];

/// How a test runs one block with its auxiliary buses.
type Run<'a> =
    &'a mut dyn FnMut(&mut AudioBuffer<f32>, &ProcessContext<'_, f32>, &mut [AudioBuffer<f32>]);

/// One block with a note on each of `channels`, through `run`, and the
/// peak of the main output and of each bus.
fn play(channels: &[u8], run: Run<'_>) -> (f32, [f32; 4]) {
    let mut events = EventBuffer::with_capacity(8);
    for &channel in channels {
        let kind = EventKind::NoteOn {
            note: NoteRef::new(channel, 60),
            velocity: 1.0,
        };
        events.push(Event { offset: 0, kind }).unwrap();
    }
    let transport = Transport::STOPPED;
    let context = ProcessContext::new(&events, &transport);
    let mut main = AudioBuffer::<f32>::new(2, 512);
    let mut aux: Vec<AudioBuffer<f32>> = (0..4).map(|_| AudioBuffer::new(2, 512)).collect();
    run(&mut main, &context, &mut aux);
    (main.peak(), std::array::from_fn(|bus| aux[bus].peak()))
}

#[test]
fn as_a_clap_plugin_each_division_has_a_port() {
    // SAFETY: this package's own library, whose entry point is the
    // framework's CLAP wrapper.
    let library = unsafe { ClapLibrary::load(built_library()) }.expect("it loads");
    let id = library.descriptors()[0].id.clone();
    let mut plugin = library.create(&id).unwrap();
    let names: Vec<&str> = plugin
        .audio_outputs()
        .iter()
        .map(|port| port.name.as_str())
        .collect();
    assert_eq!(names, ["Main", NAMES[0], NAMES[1], NAMES[2], NAMES[3]]);
    let mut processor = plugin.activate(48_000.0, 512).unwrap();
    let (main, buses) = play(&[1], &mut |main, context, aux| {
        processor.process_with_aux(main, context, aux);
    });
    assert!(main > 0.01 && buses[1] > 0.01, "the swell is heard");
    assert_eq!([buses[0], buses[2], buses[3]], [0.0; 3]);
    drop(processor);
    plugin.deactivate().unwrap();
}

#[test]
fn as_a_vst3_plugin_each_division_has_a_bus() {
    // SAFETY: as above, for the framework's VST 3 wrapper.
    let module = unsafe { Vst3Module::load(built_library()) }.expect("it loads");
    let mut plugin = module.create(module.classes()[0].id).unwrap();
    let names: Vec<&str> = plugin
        .audio_outputs()
        .iter()
        .map(|bus| bus.name.as_str())
        .collect();
    assert_eq!(names, ["Output", NAMES[0], NAMES[1], NAMES[2], NAMES[3]]);
    let mut processor = plugin.activate(48_000.0, 512).unwrap();
    let (main, buses) = play(&[3, 0], &mut |main, context, aux| {
        processor.process_with_aux(main, context, aux);
    });
    assert!(main > 0.01 && buses[0] > 0.01 && buses[3] > 0.01);
    assert_eq!([buses[1], buses[2]], [0.0; 2]);
    drop(processor);
    plugin.deactivate().unwrap();
}
