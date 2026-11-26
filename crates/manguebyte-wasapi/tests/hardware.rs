//! Real devices, run in exclusive mode: outputs play silence, and what
//! inputs record is counted and never read, kept or played.
//!
//! A test runs only on devices named by their WASAPI identifiers, so that
//! no test takes a device nobody chose:
//!
//! - `MANGUEBYTE_WASAPI_DEVICE` names an output (`{0.0.0.00000000}.{…}`);
//! - `MANGUEBYTE_WASAPI_INPUT_DEVICE` names an input
//!   (`{0.0.1.00000000}.{…}`).
//!
//! A test whose variables are not set opens nothing and passes. Before
//! anything else is asked of a device, its name is read from its property
//! store, which neither activates nor opens it: a name that says UMC or
//! Behringer, or that cannot be read, fails the test there.

#![cfg(windows)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use manguebyte_wasapi::{
    Capabilities, Error, Request, Samples, capabilities, start, start_capture,
};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::StructuredStorage::PropVariantToBSTR;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize, STGM_READ,
};
use windows::core::HSTRING;

/// A device is held by one stream at a time, so the tests take turns.
fn alone() -> MutexGuard<'static, ()> {
    static DEVICES: Mutex<()> = Mutex::new(());
    DEVICES.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether a device of this name is one no test may touch.
fn forbidden(name: &str) -> bool {
    let name = name.to_lowercase();
    name.trim().is_empty() || name.contains("umc") || name.contains("behringer")
}

/// The name Windows shows for the device, in a COM apartment this thread is
/// in already.
fn read_name(id: &str) -> windows::core::Result<String> {
    // SAFETY: a plain COM object creation in this thread's apartment.
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }?;
    // SAFETY: `HSTRING` hands a null-terminated wide string that outlives
    // the call.
    let device = unsafe { enumerator.GetDevice(&HSTRING::from(id)) }?;
    // SAFETY: `device` is live; the store is opened to be read.
    let store = unsafe { device.OpenPropertyStore(STGM_READ) }?;
    // SAFETY: the key is a constant that lives across the call.
    let value = unsafe { store.GetValue(&PKEY_Device_FriendlyName) }?;
    // SAFETY: `value` is the live property the store returned, cleared when
    // dropped.
    let name = unsafe { PropVariantToBSTR(&value) }?;
    Ok(name.to_string())
}

/// `PKEY_Device_FriendlyName` of the device `id`, with or without `wasapi:`
/// before it. Nothing is activated or opened.
fn friendly_name(id: &str) -> windows::core::Result<String> {
    let id = id.strip_prefix("wasapi:").unwrap_or(id);
    // SAFETY: no reserved pointer is passed.
    let entered = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    let name = read_name(id);
    if entered {
        // SAFETY: balances the successful `CoInitializeEx` above; every COM
        // object made since was released when `read_name` returned.
        unsafe { CoUninitialize() };
    }
    name
}

/// The device `variable` names, if it is set and a test may touch it.
fn chosen(variable: &str) -> Option<String> {
    let Ok(id) = std::env::var(variable) else {
        eprintln!("{variable} is not set: no device was opened");
        return None;
    };
    let name = friendly_name(&id).unwrap_or_else(|error| {
        panic!("{variable}: {id} is not touched, as its name cannot be read: {error}")
    });
    assert!(
        !forbidden(&name),
        "{variable} names {name:?}, which no test touches"
    );
    eprintln!("{variable}: {name}");
    Some(id)
}

/// What a stream reports from its own thread, where a panic would be lost:
/// kept, to be asserted on once the stream has stopped.
#[derive(Clone, Default)]
struct Failures(Arc<Mutex<Vec<Error>>>);

impl Failures {
    /// An `on_error` that keeps what it is told.
    fn sink(&self) -> impl FnMut(Error) + Send + 'static {
        let kept = Arc::clone(&self.0);
        move |error| {
            kept.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(error);
        }
    }

    /// Call after the stream is dropped.
    fn assert_none(&self, stream: &str) {
        let failures = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        assert!(failures.is_empty(), "the {stream} failed: {failures:?}");
    }
}

/// What the device takes, with the first of two or one channels it takes at
/// all, and a rate: 48 kHz if it takes it.
fn ask(id: &str) -> (Capabilities, u16, u32) {
    for channels in [2, 1, 4, 8] {
        let caps = capabilities(id, channels).expect("the device answers");
        eprintln!("{channels} channels: {caps:?}");
        assert!(caps.min_period > 0 && caps.min_period <= caps.default_period);
        let rate = if caps.sample_rates.contains(&48_000) {
            Some(48_000)
        } else {
            caps.sample_rates.first().copied()
        };
        if let Some(rate) = rate {
            return (caps, channels, rate);
        }
    }
    panic!("the device takes no format with 1, 2, 4 or 8 channels");
}

/// Whether about as many periods ran in half a second as should have.
fn check_periods(periods: usize, rate: u32, period_frames: u32) {
    let expected = 0.5 * f64::from(rate) / f64::from(period_frames);
    eprintln!("{periods} periods in 0.5 s, about {expected:.0} expected");
    assert!(
        periods as f64 > expected * 0.5,
        "the device ran too few periods"
    );
    assert!(
        (periods as f64) < expected * 1.5 + 2.0,
        "the device ran too many periods"
    );
}

#[test]
fn a_behringer_or_nameless_device_is_refused() {
    for name in [
        "Speakers (UMC404HD 192k)",
        "Line (BEHRINGER USB WDM AUDIO)",
        "umc202hd",
        "  ",
    ] {
        assert!(forbidden(name), "{name:?}");
    }
    assert!(!forbidden("Speakers (Realtek(R) Audio)"));
}

#[test]
fn a_chosen_device_runs_exclusively_on_its_shortest_period() {
    let _alone = alone();
    let Some(id) = chosen("MANGUEBYTE_WASAPI_DEVICE") else {
        return;
    };
    let (caps, channels, rate) = ask(&id);

    let periods = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&periods);
    let failures = Failures::default();
    let (stream, format) = start(
        &id,
        Request {
            sample_rate: rate,
            channels,
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
        failures.sink(),
    )
    .expect("the device opens exclusively");
    eprintln!("format: {format:?}");
    // Auto asks for the shortest period; alignment may only lengthen it.
    assert!(format.period_frames + 1 >= caps.min_period_frames(rate));
    std::thread::sleep(Duration::from_millis(500));
    drop(stream);
    failures.assert_none("output");
    check_periods(periods.load(Ordering::Relaxed), rate, format.period_frames);

    // Free again: it opens a second time.
    let (again, _) = start(
        &id,
        Request {
            sample_rate: rate,
            channels,
            period_frames: Some(512),
        },
        |_, _| {},
        failures.sink(),
    )
    .expect("the device was given back");
    drop(again);
    failures.assert_none("output");
}

#[test]
fn a_chosen_input_records_exclusively_on_its_shortest_period() {
    let _alone = alone();
    let Some(id) = chosen("MANGUEBYTE_WASAPI_INPUT_DEVICE") else {
        return;
    };
    let (caps, channels, rate) = ask(&id);
    let request = Request {
        sample_rate: rate,
        channels,
        period_frames: None,
    };
    let wrong_way = start(&id, request, |_, _| {}, |_| {}).expect_err("an input does not play");
    assert!(matches!(wrong_way, Error::Unsupported(_)), "{wrong_way:?}");

    // Packets, frames and discontinuities; the samples themselves are not
    // looked at.
    let counts = Arc::new([const { AtomicUsize::new(0) }; 3]);
    let counted = Arc::clone(&counts);
    let failures = Failures::default();
    let (stream, format) = start_capture(
        &id,
        request,
        move |packet, _| {
            let frames = packet.samples.len() / usize::from(channels);
            counted[0].fetch_add(1, Ordering::Relaxed);
            counted[1].fetch_add(frames, Ordering::Relaxed);
            counted[2].fetch_add(usize::from(packet.discontinuity), Ordering::Relaxed);
        },
        failures.sink(),
    )
    .expect("the input opens exclusively");
    eprintln!("format: {format:?}");
    assert!(format.period_frames + 1 >= caps.min_period_frames(rate));
    std::thread::sleep(Duration::from_millis(500));
    drop(stream);
    failures.assert_none("input");
    let [packets, frames, discontinuities] =
        std::array::from_fn(|i| counts[i].load(Ordering::Relaxed));
    eprintln!("{frames} frames, {discontinuities} discontinuities");
    check_periods(packets, rate, format.period_frames);
    assert_eq!(frames, packets * format.period_frames as usize);
    // The first packet may report one.
    assert!(discontinuities <= 1 + packets / 10, "audio is being lost");

    // Free again: it opens a second time.
    let (again, _) = start_capture(
        &id,
        Request {
            period_frames: Some(512),
            ..request
        },
        |_, _| {},
        failures.sink(),
    )
    .expect("the input was given back");
    drop(again);
    failures.assert_none("input");
}

#[test]
fn a_chosen_input_and_output_run_together() {
    let _alone = alone();
    let output = chosen("MANGUEBYTE_WASAPI_DEVICE");
    let input = chosen("MANGUEBYTE_WASAPI_INPUT_DEVICE");
    let (Some(output), Some(input)) = (output, input) else {
        return;
    };
    let (_, output_channels, rate) = ask(&output);
    let (input_caps, input_channels, _) = ask(&input);
    assert!(
        input_caps.sample_rates.contains(&rate),
        "the input does not run at the output's {rate} Hz"
    );

    let periods = Arc::new([const { AtomicUsize::new(0) }; 2]);
    let (played, recorded) = (Arc::clone(&periods), Arc::clone(&periods));
    let request = |channels| Request {
        sample_rate: rate,
        channels,
        period_frames: None,
    };
    let (out_failures, in_failures) = (Failures::default(), Failures::default());
    // The output is left silent: nothing recorded reaches it.
    let (out_stream, out_format) = start(
        &output,
        request(output_channels),
        move |_, _| {
            played[0].fetch_add(1, Ordering::Relaxed);
        },
        out_failures.sink(),
    )
    .expect("the output opens exclusively");
    let (in_stream, in_format) = start_capture(
        &input,
        request(input_channels),
        move |_, _| {
            recorded[1].fetch_add(1, Ordering::Relaxed);
        },
        in_failures.sink(),
    )
    .expect("the input opens beside it");
    eprintln!("output: {out_format:?}");
    eprintln!("input: {in_format:?}");
    std::thread::sleep(Duration::from_millis(500));
    drop(in_stream);
    drop(out_stream);
    out_failures.assert_none("output");
    in_failures.assert_none("input");
    check_periods(
        periods[0].load(Ordering::Relaxed),
        rate,
        out_format.period_frames,
    );
    check_periods(
        periods[1].load(Ordering::Relaxed),
        rate,
        in_format.period_frames,
    );
}
