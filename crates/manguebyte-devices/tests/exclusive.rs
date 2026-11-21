//! The "WASAPI exclusive" host through a device manager, on a real device.
//!
//! It runs only on Windows and only when `MANGUEBYTE_WASAPI_DEVICE` names
//! an output device by its WASAPI identifier (`{0.0.0.00000000}.{…}`, with
//! or without `wasapi:` before it), so that no test takes a device nobody
//! chose. The callback leaves every buffer silent.

#![cfg(windows)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use manguebyte_devices::{
    DeviceChoice, DeviceIo, DeviceManager, DeviceRef, DeviceSetup, Direction, WASAPI_EXCLUSIVE,
};

#[test]
fn a_device_manager_plays_a_chosen_device_exclusively() {
    let Ok(id) = std::env::var("MANGUEBYTE_WASAPI_DEVICE") else {
        eprintln!("MANGUEBYTE_WASAPI_DEVICE is not set: no device was opened");
        return;
    };
    // Windows lists identifiers in lower case; Device Manager shows them in
    // upper case.
    let id = id.to_lowercase();
    let id = if id.starts_with("wasapi:") {
        id
    } else {
        format!("wasapi:{id}")
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
        .with_output(DeviceChoice::Device(DeviceRef::new(id.clone(), "")))
        .with_input(DeviceChoice::None);
    let blocks = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&blocks);
    let report = manager
        .start(&setup, move |io: &mut DeviceIo<'_>| {
            assert!(io.num_frames() > 0);
            counted.fetch_add(1, Ordering::Relaxed);
        })
        .expect("the manager starts");
    eprintln!("report: {report:?}");
    assert_eq!(manager.host_name(), WASAPI_EXCLUSIVE);
    assert!(manager.is_running(), "nothing runs: {report:?}");
    assert_eq!(
        manager
            .running_device(Direction::Output)
            .map(|d| d.id.as_str()),
        Some(id.as_str())
    );
    let settings = manager.running_settings(Direction::Output).unwrap();
    eprintln!("running: {settings:?}");
    let period = settings.buffer_size.expect("the period is known");
    assert!(period > 0 && period <= 1_024);

    let info = manager.device_info(Direction::Output, &id).unwrap();
    let (shortest, _) = info.configs[0].buffer_size.expect("exclusive sizes");
    assert!(shortest <= period);

    std::thread::sleep(Duration::from_millis(300));
    manager.stop();
    let ran = blocks.load(Ordering::Relaxed);
    eprintln!("{ran} blocks of {period} frames in 0.3 s");
    assert!(ran > 10);
}
