//! WASAPI's `IAudioClient` in exclusive, event-driven mode.
//!
//! A stream's thread makes every COM object it uses, runs the device, and
//! releases them, in that order, inside one multithreaded apartment of its
//! own. Each period the driver signals an event; the thread takes the
//! whole buffer, has it filled, and hands it back.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED, AUDCLNT_E_DEVICE_IN_USE,
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED,
    AUDCLNT_E_UNSUPPORTED_FORMAT, AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    DEVICE_STATE_ACTIVE, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, WAVE_FORMAT_PCM, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    WAVEFORMATEXTENSIBLE_0,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
    WaitForSingleObject,
};
use windows::core::{HSTRING, w};

use crate::{Capabilities, Error, Format, Request, SampleKind, Samples, period_of};

/// The rates [`capabilities`] asks about.
const RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

/// The formats tried, best first: no conversion, then the widest integers.
const KINDS: [SampleKind; 4] = [
    SampleKind::F32,
    SampleKind::I32 { valid_bits: 32 },
    SampleKind::I32 { valid_bits: 24 },
    SampleKind::I16,
];

/// What fills a period: the buffer, and how long the stream has run.
type Render<'r> = dyn FnMut(Samples<'_>, Duration) + 'r;

/// Where a stream's thread says whether the device started.
type Ready = mpsc::SyncSender<Result<Format, Error>>;

/// How long to wait for the driver's event before calling the device lost.
const WAIT_MS: u32 = 2_000;

/// A COM apartment for this thread, left when dropped.
struct Apartment {
    entered: bool,
}

impl Apartment {
    fn enter() -> Self {
        // SAFETY: no reserved pointer is passed. Entering the multithreaded
        // apartment fails only if the thread is already in another one, and
        // then COM still works there and is not left by us.
        let entered = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        Self { entered }
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.entered {
            // SAFETY: balances the successful `CoInitializeEx` of this
            // thread; every COM object made under it was declared after the
            // apartment and has been released before it.
            unsafe { CoUninitialize() };
        }
    }
}

/// An auto-reset event the driver signals once a period, closed when
/// dropped.
struct Event(HANDLE);

impl Event {
    fn new() -> Result<Self, Error> {
        // SAFETY: no security attributes and no name; the handle returned is
        // owned by the `Event` and closed once.
        let handle = unsafe { CreateEventW(None, false, false, None) }.map_err(os)?;
        Ok(Self(handle))
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the handle came from `CreateEventW` and is closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The "Pro Audio" scheduling class for this thread, given up when
/// dropped. Without it the thread runs at normal priority, which plays,
/// with more risk of gaps.
struct ProAudio(Option<HANDLE>);

impl ProAudio {
    fn join() -> Self {
        let mut task = 0u32;
        // SAFETY: the task name is a static wide string and `task` a valid
        // place for the index.
        Self(unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task) }.ok())
    }
}

impl Drop for ProAudio {
    fn drop(&mut self) {
        if let Some(handle) = self.0 {
            // SAFETY: the handle came from `AvSetMmThreadCharacteristicsW`
            // on this thread and is reverted once.
            let _ = unsafe { AvRevertMmThreadCharacteristics(handle) };
        }
    }
}

fn os(error: windows::core::Error) -> Error {
    Error::Os(error.to_string())
}

/// What a failed call means for the caller.
fn classify(error: windows::core::Error) -> Error {
    let code = error.code();
    if code == AUDCLNT_E_DEVICE_INVALIDATED {
        Error::DeviceLost
    } else if code == AUDCLNT_E_DEVICE_IN_USE || code == AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED {
        Error::DeviceBusy(error.message().to_string())
    } else if code == AUDCLNT_E_UNSUPPORTED_FORMAT {
        Error::FormatNotSupported(error.message().to_string())
    } else {
        os(error)
    }
}

fn open_device(id: &str) -> Result<IMMDevice, Error> {
    // SAFETY: a plain COM object creation in this thread's apartment.
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(os)?;
    // SAFETY: `HSTRING` hands a null-terminated wide string that outlives
    // the call.
    let device = unsafe { enumerator.GetDevice(&HSTRING::from(id)) }
        .map_err(|_| Error::DeviceNotFound(id.to_owned()))?;
    // SAFETY: `device` is a live device the enumerator returned.
    let state = unsafe { device.GetState() }.map_err(os)?;
    if state != DEVICE_STATE_ACTIVE {
        return Err(Error::DeviceNotFound(id.to_owned()));
    }
    Ok(device)
}

fn activate(device: &IMMDevice) -> Result<IAudioClient, Error> {
    // SAFETY: `device` is live; no activation parameters.
    unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }.map_err(classify)
}

/// The format `kind` stands for, at `sample_rate` with `channels`.
fn wave_format(kind: SampleKind, sample_rate: u32, channels: u16) -> WAVEFORMATEXTENSIBLE {
    let (container, valid, sub_format) = match kind {
        SampleKind::F32 => (32, 32, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT),
        SampleKind::I32 { valid_bits } => (32, valid_bits, KSDATAFORMAT_SUBTYPE_PCM),
        SampleKind::I16 => (16, 16, KSDATAFORMAT_SUBTYPE_PCM),
    };
    let block_align = channels * (container / 8);
    let header = std::mem::size_of::<WAVEFORMATEX>();
    let extension = std::mem::size_of::<WAVEFORMATEXTENSIBLE>() - header;
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: channels,
            nSamplesPerSec: sample_rate,
            nAvgBytesPerSec: sample_rate * u32::from(block_align),
            nBlockAlign: block_align,
            wBitsPerSample: container,
            cbSize: extension as u16,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: valid,
        },
        // Front centre for one channel, front left and right for two, and
        // straight through otherwise, as `cpal` does.
        dwChannelMask: match channels {
            1 => 0x4,
            2 => 0x3,
            _ => 0,
        },
        SubFormat: sub_format,
    }
}

fn as_header(format: &WAVEFORMATEXTENSIBLE) -> *const WAVEFORMATEX {
    std::ptr::from_ref(format).cast()
}

/// The best format the device takes exclusively at this rate and channel
/// count.
fn pick_kind(client: &IAudioClient, sample_rate: u32, channels: u16) -> Option<SampleKind> {
    KINDS.into_iter().find(|&kind| {
        let format = wave_format(kind, sample_rate, channels);
        // SAFETY: the format lives across the call, and exclusive mode takes
        // no closest-match pointer.
        let answer = unsafe {
            client.IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, as_header(&format), None)
        };
        answer.is_ok() && answer.0 == 0
    })
}

fn min_period(client: &IAudioClient) -> Result<(i64, i64), Error> {
    let (mut default, mut min) = (0i64, 0i64);
    // SAFETY: both pointers are to live locals.
    unsafe { client.GetDevicePeriod(Some(&mut default), Some(&mut min)) }.map_err(classify)?;
    Ok((default, min))
}

pub(crate) fn capabilities(id: &str, channels: u16) -> Result<Capabilities, Error> {
    let _apartment = Apartment::enter();
    let device = open_device(id)?;
    let client = activate(&device)?;
    let (default_period, min_period) = min_period(&client)?;
    let sample_rates = RATES
        .into_iter()
        .filter(|&rate| pick_kind(&client, rate, channels).is_some())
        .collect();
    Ok(Capabilities {
        sample_rates,
        min_period,
        default_period,
    })
}

/// An initialised exclusive client and what it runs.
struct Opened {
    client: IAudioClient,
    render: IAudioRenderClient,
    event: Event,
    format: Format,
}

/// A device's PCM in a plain `WAVEFORMATEX`, with no extension: some
/// drivers (Bluetooth ones among them) take 16-bit audio only so.
fn plain(format: &WAVEFORMATEXTENSIBLE) -> WAVEFORMATEXTENSIBLE {
    let mut plain = *format;
    plain.Format.wFormatTag = WAVE_FORMAT_PCM as u16;
    plain.Format.cbSize = 0;
    plain
}

/// Initialises `client` exclusively with `format` on `period`; when the
/// driver asks for an aligned period, makes a new client and asks again.
/// Returns the client that took it, and the period it took.
fn initialise(
    device: &IMMDevice,
    client: IAudioClient,
    format: &WAVEFORMATEXTENSIBLE,
    sample_rate: u32,
    period: i64,
) -> Result<(IAudioClient, i64), windows::core::Error> {
    let attempt = |client: &IAudioClient, period: i64| {
        // SAFETY: the format lives across the call; in exclusive,
        // event-driven mode the buffer duration and the period are the
        // same, and no session is named.
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_EXCLUSIVE,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                period,
                period,
                as_header(format),
                None,
            )
        }
    };
    match attempt(&client, period) {
        Ok(()) => Ok((client, period)),
        Err(error) if error.code() == AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED => {
            // The driver says, in frames, the size it can align to, and the
            // client must be made again to be asked twice.
            // SAFETY: the client reports its aligned size after this failure.
            let aligned = unsafe { client.GetBufferSize() }?;
            let period = period_of(aligned, sample_rate);
            // SAFETY: `device` is live; no activation parameters.
            let client = unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }?;
            attempt(&client, period)?;
            Ok((client, period))
        }
        Err(error) => Err(error),
    }
}

fn open(id: &str, request: Request) -> Result<Opened, Error> {
    let Request {
        sample_rate,
        channels,
        period_frames,
    } = request;
    let device = open_device(id)?;
    let probe = activate(&device)?;
    let (_, min) = min_period(&probe)?;
    let wanted = period_frames.map_or(min, |frames| period_of(frames, sample_rate).max(min));
    // The format the device says it takes goes first; drivers do not always
    // keep their word, so every other one is tried too, and 16-bit audio in
    // a plain header last.
    let preferred = pick_kind(&probe, sample_rate, channels);
    drop(probe);
    let mut tries: Vec<(SampleKind, WAVEFORMATEXTENSIBLE)> = preferred
        .into_iter()
        .chain(KINDS.into_iter().filter(|&kind| Some(kind) != preferred))
        .map(|kind| (kind, wave_format(kind, sample_rate, channels)))
        .collect();
    tries.push((
        SampleKind::I16,
        plain(&wave_format(SampleKind::I16, sample_rate, channels)),
    ));

    let mut taken = None;
    for (kind, format) in &tries {
        match initialise(&device, activate(&device)?, format, sample_rate, wanted) {
            Ok((client, period)) => {
                taken = Some((client, *kind, period));
                break;
            }
            Err(error) if error.code() == AUDCLNT_E_UNSUPPORTED_FORMAT => {}
            Err(error) => return Err(classify(error)),
        }
    }
    let Some((client, kind, period)) = taken else {
        return Err(Error::FormatNotSupported(format!(
            "{sample_rate} Hz, {channels} channels"
        )));
    };

    // SAFETY: the client is initialised.
    let frames = unsafe { client.GetBufferSize() }.map_err(classify)?;
    let event = Event::new()?;
    // SAFETY: the client is initialised for event callbacks and the handle
    // lives as long as the client does (`Opened` drops the client first).
    unsafe { client.SetEventHandle(event.0) }.map_err(classify)?;
    // SAFETY: the client is initialised for rendering.
    let render: IAudioRenderClient = unsafe { client.GetService() }.map_err(classify)?;
    // SAFETY: the client is initialised.
    let reported = unsafe { client.GetStreamLatency() }.unwrap_or(0);
    let latency = if reported > 0 { reported } else { period };
    Ok(Opened {
        client,
        render,
        event,
        format: Format {
            sample_rate,
            channels,
            kind,
            period_frames: frames,
            latency: Duration::from_nanos(u64::try_from(latency).unwrap_or(0) * 100),
        },
    })
}

/// `len` values of `T` at `data`, silenced, or `None` if `data` cannot
/// hold them.
///
/// # Safety
///
/// `data` must be null or point to at least `len` values' worth of
/// writable bytes that nothing else touches for `'a`.
unsafe fn silenced<'a, T: Copy + Default>(data: *mut u8, len: usize) -> Option<&'a mut [T]> {
    if data.is_null() || data.align_offset(std::mem::align_of::<T>()) != 0 {
        return None;
    }
    // SAFETY: the caller's promise, and the pointer is non-null and
    // aligned for `T`.
    let slice = unsafe { std::slice::from_raw_parts_mut(data.cast::<T>(), len) };
    slice.fill(T::default());
    Some(slice)
}

impl Opened {
    /// Takes the device's buffer, has `render` fill it, and hands it back.
    /// With no `render`, plays a period of silence.
    fn period(&self, render: Option<&mut Render<'_>>, time: Duration) -> Result<(), Error> {
        let frames = self.format.period_frames;
        // SAFETY: in exclusive, event-driven mode the whole buffer is free
        // when the event fires, and before the stream starts.
        let data = unsafe { self.render.GetBuffer(frames) }.map_err(classify)?;
        let len = frames as usize * usize::from(self.format.channels);
        let mut flags = 0;
        match render {
            None => flags = AUDCLNT_BUFFERFLAGS_SILENT.0 as u32,
            Some(render) => {
                // SAFETY: `GetBuffer` returned `frames` frames of the format
                // the client was initialised with, writable and ours until
                // `ReleaseBuffer`.
                let samples = unsafe {
                    match self.format.kind {
                        SampleKind::F32 => silenced(data, len).map(Samples::F32),
                        SampleKind::I32 { .. } => silenced(data, len).map(Samples::I32),
                        SampleKind::I16 => silenced(data, len).map(Samples::I16),
                    }
                };
                match samples {
                    Some(samples) => render(samples, time),
                    None => flags = AUDCLNT_BUFFERFLAGS_SILENT.0 as u32,
                }
            }
        }
        // SAFETY: hands back exactly the frames taken.
        unsafe { self.render.ReleaseBuffer(frames, flags) }.map_err(classify)
    }
}

/// A stream's thread, stopped and joined when dropped.
pub(crate) struct Running {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(crate) fn start<R, E>(
    id: String,
    request: Request,
    render: R,
    on_error: E,
) -> Result<(Running, Format), Error>
where
    R: FnMut(Samples<'_>, Duration) + Send + 'static,
    E: FnMut(Error) + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let (ready, started) = mpsc::sync_channel(1);
    let stopping = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name(String::from("manguebyte-wasapi"))
        .spawn(move || run(&id, request, render, on_error, &stopping, &ready))
        .map_err(|error| Error::Os(error.to_string()))?;
    match started.recv() {
        Ok(Ok(format)) => Ok((
            Running {
                stop,
                thread: Some(thread),
            },
            format,
        )),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(error)
        }
        Err(_) => {
            let _ = thread.join();
            Err(Error::Os(String::from(
                "the stream's thread ended before it started",
            )))
        }
    }
}

fn run<R, E>(
    id: &str,
    request: Request,
    mut render: R,
    mut on_error: E,
    stop: &AtomicBool,
    ready: &Ready,
) where
    R: FnMut(Samples<'_>, Duration),
    E: FnMut(Error),
{
    let _apartment = Apartment::enter();
    let opened = match open(id, request) {
        Ok(opened) => opened,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let _priority = ProAudio::join();
    // A period of silence is in the buffer before the device starts, as
    // exclusive mode asks.
    // SAFETY: the client is initialised and not yet started.
    let begun = opened
        .period(None, Duration::ZERO)
        .and_then(|()| unsafe { opened.client.Start() }.map_err(classify));
    if let Err(error) = begun {
        let _ = ready.send(Err(error));
        return;
    }
    let _ = ready.send(Ok(opened.format));

    let origin = Instant::now();
    loop {
        // SAFETY: the event handle is live for as long as `opened`.
        let woke = unsafe { WaitForSingleObject(opened.event.0, WAIT_MS) };
        if stop.load(Ordering::Acquire) {
            break;
        }
        if woke != WAIT_OBJECT_0 {
            on_error(Error::DeviceLost);
            break;
        }
        if let Err(error) = opened.period(Some(&mut render), origin.elapsed()) {
            on_error(error);
            break;
        }
    }
    // SAFETY: the client is initialised; stopping a stopped one is harmless.
    let _ = unsafe { opened.client.Stop() };
}
