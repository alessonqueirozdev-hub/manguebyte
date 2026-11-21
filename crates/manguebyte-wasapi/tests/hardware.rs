//! A real device, played silence in exclusive mode.
//!
//! It runs only when `MANGUEBYTE_WASAPI_DEVICE` names an output device by
//! its WASAPI identifier (`{0.0.0.00000000}.{…}`), so that no test takes a
//! device nobody chose. The buffers are left as they come, silent.

#![cfg(windows)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use manguebyte_wasapi::{Request, Samples, capabilities, start};

#[test]
fn a_chosen_device_runs_exclusively_on_its_shortest_period() {
    let Ok(id) = std::env::var("MANGUEBYTE_WASAPI_DEVICE") else {
        eprintln!("MANGUEBYTE_WASAPI_DEVICE is not set: no device was opened");
        return;
    };
    let caps = capabilities(&id, 2).expect("the device answers");
    eprintln!("capabilities: {caps:?}");
    assert!(caps.min_period > 0 && caps.min_period <= caps.default_period);
    let rate = if caps.sample_rates.contains(&48_000) {
        48_000
    } else {
        *caps.sample_rates.first().expect("some rate is taken")
    };

    let periods = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&periods);
    let (stream, format) = start(
        &id,
        Request {
            sample_rate: rate,
            channels: 2,
            period_frames: None,
        },
        move |samples, _| {
            let len = match samples {
                Samples::F32(s) => s.len(),
                Samples::I32(s) => s.len(),
                Samples::I16(s) => s.len(),
            };
            assert!(len > 0);
            counted.fetch_add(1, Ordering::Relaxed);
        },
        |error| panic!("the device failed: {error}"),
    )
    .expect("the device opens exclusively");
    eprintln!("format: {format:?}");
    // Auto asks for the shortest period; alignment may only lengthen it.
    assert!(format.period_frames + 1 >= caps.min_period_frames(rate));
    std::thread::sleep(Duration::from_millis(500));
    drop(stream);
    let played = periods.load(Ordering::Relaxed);
    let expected = 0.5 * f64::from(rate) / f64::from(format.period_frames);
    eprintln!("{played} periods in 0.5 s, about {expected:.0} expected");
    assert!(
        played as f64 > expected * 0.5,
        "the device ran too few periods"
    );

    // Free again: it opens a second time.
    let (again, _) = start(
        &id,
        Request {
            sample_rate: rate,
            channels: 2,
            period_frames: Some(512),
        },
        |_, _| {},
        |error| panic!("the device failed: {error}"),
    )
    .expect("the device was given back");
    drop(again);
}
