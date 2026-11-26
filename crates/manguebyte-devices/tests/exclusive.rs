//! The "WASAPI exclusive" host through a device manager, on real devices.
//!
//! It runs only on Windows and only on devices named by their WASAPI
//! identifiers, so that no test takes a device nobody chose:
//!
//! - `MANGUEBYTE_WASAPI_DEVICE` names an output (`{0.0.0.00000000}.{…}`);
//! - `MANGUEBYTE_WASAPI_INPUT_DEVICE` names an input
//!   (`{0.0.1.00000000}.{…}`).
//!
//! Either may have `wasapi:` before it. A test whose variables are not set
//! opens nothing and passes. Outputs are left silent, and what an input
//! records is counted and never read, kept or played.
//!
//! A chosen device is looked up in the manager's list of names, which asks
//! no device what it supports, and one whose name says UMC or Behringer, or
//! cannot be read, is refused there. After that only the chosen devices are
//! asked anything.

#![cfg(windows)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use manguebyte_devices::{
    DeviceChoice, DeviceIo, DeviceManager, DeviceRef, DeviceSetup, Direction, WASAPI_EXCLUSIVE,
};

/// A device is held by one stream at a time, so the tests take turns.
fn alone() -> MutexGuard<'static, ()> {
    static DEVICES: Mutex<()> = Mutex::new(());
    DEVICES.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The device `variable` names, as the host lists it, if it is set.
fn chosen(variable: &str, direction: Direction) -> Option<String> {
    let Ok(id) = std::env::var(variable) else {
        eprintln!("{variable} is not set: no device was opened");
        return None;
    };
    // Windows lists identifiers in lower case; Device Manager shows them in
    // upper case.
    let id = id.to_lowercase();
    let id = if id.starts_with("wasapi:") {
        id
    } else {
        format!("wasapi:{id}")
    };
    // The shared host's devices are the exclusive host's, by the same
    // identifiers, and listing them reads only identifiers and names.
    let snapshot = DeviceManager::new()
        .devices()
        .expect("Windows lists devices");
    let device = snapshot
        .devices(direction)
        .iter()
        .find(|device| device.id == id)
        .unwrap_or_else(|| panic!("{variable} names no {}: {id}", direction.as_str()));
    let name = device.name.to_lowercase();
    // "unknown device" stands for a name that could not be read.
    assert!(
        !["umc", "behringer", "unknown device"]
            .iter()
            .any(|forbidden| name.contains(forbidden)),
        "{variable} names {:?}, which no test touches",
        device.name
    );
    eprintln!("{variable}: {}", device.name);
    Some(id)
}

/// Runs the manager's exclusive host on these devices for 0.3 s, with a
/// silent callback, and checks what ran.
fn run(output: Option<String>, input: Option<String>) {
    let choice = |id: &Option<String>| {
        id.as_ref().map_or(DeviceChoice::None, |id| {
            DeviceChoice::Device(DeviceRef::new(id.clone(), ""))
        })
    };
    let mut manager = DeviceManager::new();
    assert!(
        manager
            .hosts()
            .iter()
            .any(|host| host.name == WASAPI_EXCLUSIVE)
    );

    let setup = DeviceSetup::default()
        .with_host(WASAPI_EXCLUSIVE)
        .with_output(choice(&output))
        .with_input(choice(&input));
    // What the streams report from their threads, where a panic would be
    // lost.
    let failures = Arc::new(Mutex::new(Vec::new()));
    let kept = Arc::clone(&failures);
    manager.set_error_handler(move |error| {
        kept.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(error.to_string());
    });
    let blocks = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&blocks);
    let with_input = input.is_some();
    let report = manager
        .start(&setup, move |io: &mut DeviceIo<'_>| {
            assert!(io.num_frames() > 0);
            assert_eq!(io.input.num_channels() > 0, with_input);
            counted.fetch_add(1, Ordering::Relaxed);
        })
        .expect("the manager starts");
    eprintln!("report: {report:?}");
    assert_eq!(manager.host_name(), WASAPI_EXCLUSIVE);
    assert!(manager.is_running(), "nothing runs: {report:?}");

    let mut period = 0;
    for (direction, id) in [(Direction::Output, &output), (Direction::Input, &input)] {
        // A device that would not open is not among those running.
        assert_eq!(
            manager.running_device(direction).map(|d| d.id.as_str()),
            id.as_deref(),
            "{report:?}"
        );
        let Some(id) = id else { continue };
        let settings = manager.running_settings(direction).unwrap();
        eprintln!("running {}: {settings:?}", direction.as_str());
        let frames = settings.buffer_size.expect("the period is known");
        assert!(frames > 0 && frames <= 8_192);
        period = period.max(frames);
        let info = manager.device_info(direction, id).unwrap();
        let (shortest, _) = info.configs[0].buffer_size.expect("exclusive sizes");
        assert!(shortest <= frames);
    }

    std::thread::sleep(Duration::from_millis(300));
    manager.stop();
    let failures = failures.lock().unwrap_or_else(PoisonError::into_inner);
    assert!(failures.is_empty(), "a stream failed: {failures:?}");
    let ran = blocks.load(Ordering::Relaxed);
    eprintln!("{ran} blocks, periods of up to {period} frames, in 0.3 s");
    assert!(ran > 3);
}

#[test]
fn a_device_manager_plays_a_chosen_device_exclusively() {
    let _alone = alone();
    let Some(output) = chosen("MANGUEBYTE_WASAPI_DEVICE", Direction::Output) else {
        return;
    };
    run(Some(output), None);
}

#[test]
fn a_device_manager_records_a_chosen_input_exclusively() {
    let _alone = alone();
    let Some(input) = chosen("MANGUEBYTE_WASAPI_INPUT_DEVICE", Direction::Input) else {
        return;
    };
    run(None, Some(input));
}

#[test]
fn a_device_manager_runs_chosen_devices_in_duplex_exclusively() {
    let _alone = alone();
    let output = chosen("MANGUEBYTE_WASAPI_DEVICE", Direction::Output);
    let input = chosen("MANGUEBYTE_WASAPI_INPUT_DEVICE", Direction::Input);
    let (Some(output), Some(input)) = (output, input) else {
        return;
    };
    run(Some(output), Some(input));
}
