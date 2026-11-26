//! WASAPI's `IAudioClient` in exclusive, event-driven mode.
//!
//! A stream's thread makes every COM object it uses, runs the device, and
//! releases them, in that order, inside one multithreaded apartment of its
//! own. Each period the driver signals an event; an output's thread takes
//! the whole buffer, has it filled, and hands it back, and an input's takes
//! the packet recorded, has it read, and hands it back.
//!
//! What can be decided without a device (formats, periods, what a failure
//! means) is in `negotiate`, and what a packet becomes in `packet`, where
//! every system tests it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, DEVICE_STATE_ACTIVE, EDataFlow,
    IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
    IMMEndpoint, MMDeviceEnumerator, WAVE_FORMAT_PCM, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    WAVEFORMATEXTENSIBLE_0, eCapture, eRender,
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
use windows::core::{HSTRING, Interface, w};

use crate::negotiate::{
    Candidate, Failure, Header, RATES, candidates, code, error_for, layout, negotiate, period_of,
    refuses_the_device, wanted_period,
};
use crate::packet::Reader;
use crate::{Capabilities, Error, Format, Packet, Request, SampleKind, Samples};

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
    error_for(error.code().0, &error.message().to_string())
}

fn failure(error: windows::core::Error) -> Failure {
    Failure {
        hresult: error.code().0,
        message: error.message().to_string(),
    }
}

/// The active device `id`; with `flow`, only if it carries audio that way.
fn open_device(id: &str, flow: Option<EDataFlow>) -> Result<IMMDevice, Error> {
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
    if let Some(flow) = flow {
        let endpoint: IMMEndpoint = device.cast().map_err(os)?;
        // SAFETY: the endpoint is the live device above.
        if unsafe { endpoint.GetDataFlow() }.map_err(os)? != flow {
            let wanted = if flow == eCapture { "input" } else { "output" };
            return Err(Error::Unsupported(format!("{id} is not an {wanted}")));
        }
    }
    Ok(device)
}

fn activate(device: &IMMDevice) -> Result<IAudioClient, Error> {
    // SAFETY: `device` is live; no activation parameters.
    unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }.map_err(classify)
}

/// The structure that stands for `candidate`, at `sample_rate` with
/// `channels`.
fn wave_format(candidate: Candidate, sample_rate: u32, channels: u16) -> WAVEFORMATEXTENSIBLE {
    let layout = layout(candidate.kind, sample_rate, channels);
    let header = std::mem::size_of::<WAVEFORMATEX>();
    let extension = std::mem::size_of::<WAVEFORMATEXTENSIBLE>() - header;
    let (tag, size) = match candidate.header {
        Header::Extensible => (WAVE_FORMAT_EXTENSIBLE, extension as u16),
        Header::Plain => (WAVE_FORMAT_PCM, 0),
    };
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: tag as u16,
            nChannels: channels,
            nSamplesPerSec: sample_rate,
            nAvgBytesPerSec: layout.bytes_per_second,
            nBlockAlign: layout.block_align,
            wBitsPerSample: layout.container_bits,
            cbSize: size,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: layout.valid_bits,
        },
        dwChannelMask: layout.channel_mask,
        SubFormat: match candidate.kind {
            SampleKind::F32 => KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
            SampleKind::I32 { .. } | SampleKind::I16 => KSDATAFORMAT_SUBTYPE_PCM,
        },
    }
}

fn as_header(format: &WAVEFORMATEXTENSIBLE) -> *const WAVEFORMATEX {
    std::ptr::from_ref(format).cast()
}

/// The first format the device says it takes exclusively at this rate and
/// channel count, or the reason it refuses to be asked.
fn first_supported(
    client: &IAudioClient,
    sample_rate: u32,
    channels: u16,
) -> Result<Option<SampleKind>, Error> {
    for candidate in candidates(None) {
        let format = wave_format(candidate, sample_rate, channels);
        // SAFETY: the format lives across the call, and exclusive mode takes
        // no closest-match pointer.
        let answer = unsafe {
            client.IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, as_header(&format), None)
        };
        // Only `S_OK` is a yes: `S_FALSE` offers something else.
        if answer.0 == 0 {
            return Ok(Some(candidate.kind));
        }
        if refuses_the_device(answer.0) {
            return Err(error_for(answer.0, &answer.message().to_string()));
        }
    }
    Ok(None)
}

/// The device's default and shortest periods, in 100 ns units.
fn periods(client: &IAudioClient) -> Result<(i64, i64), Error> {
    let (mut default, mut min) = (0i64, 0i64);
    // SAFETY: both pointers are to live locals.
    unsafe { client.GetDevicePeriod(Some(&mut default), Some(&mut min)) }.map_err(classify)?;
    Ok((default, min))
}

pub(crate) fn capabilities(id: &str, channels: u16) -> Result<Capabilities, Error> {
    let _apartment = Apartment::enter();
    let device = open_device(id, None)?;
    let client = activate(&device)?;
    let (default_period, min_period) = periods(&client)?;
    let mut sample_rates = Vec::new();
    for rate in RATES {
        if first_supported(&client, rate, channels)?.is_some() {
            sample_rates.push(rate);
        }
    }
    Ok(Capabilities {
        sample_rates,
        min_period,
        default_period,
    })
}

/// An initialised exclusive client, the event it signals, and its format.
/// The client is declared first, so it is released before the event is
/// closed.
struct Opened {
    client: IAudioClient,
    event: Event,
    format: Format,
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
        Err(error) if error.code().0 == code::BUFFER_SIZE_NOT_ALIGNED => {
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

fn open(id: &str, request: Request, flow: EDataFlow) -> Result<Opened, Error> {
    let Request {
        sample_rate,
        channels,
        period_frames,
    } = request;
    let device = open_device(id, Some(flow))?;
    let probe = activate(&device)?;
    let (_, min) = periods(&probe)?;
    let wanted = wanted_period(period_frames, sample_rate, min);
    // What the device says it takes only orders the tries: whether it is
    // taken, and why not, is what `Initialize` answers.
    let preferred = first_supported(&probe, sample_rate, channels)
        .ok()
        .flatten();
    drop(probe);

    let what = format!("{sample_rate} Hz, {channels} channels");
    let (candidate, (client, period)) = negotiate(&candidates(preferred), &what, |candidate| {
        let format = wave_format(candidate, sample_rate, channels);
        // SAFETY: `device` is live; no activation parameters.
        let client =
            unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }.map_err(failure)?;
        initialise(&device, client, &format, sample_rate, wanted).map_err(failure)
    })?;

    // SAFETY: the client is initialised.
    let frames = unsafe { client.GetBufferSize() }.map_err(classify)?;
    let event = Event::new()?;
    // SAFETY: the client is initialised for event callbacks and the handle
    // lives as long as the client does (`Opened` drops the client first).
    unsafe { client.SetEventHandle(event.0) }.map_err(classify)?;
    // SAFETY: the client is initialised.
    let reported = unsafe { client.GetStreamLatency() }.unwrap_or(0);
    let latency = if reported > 0 { reported } else { period };
    Ok(Opened {
        client,
        event,
        format: Format {
            sample_rate,
            channels,
            kind: candidate.kind,
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

/// What a stream's thread does with its device: the direction's own part.
trait Pump {
    /// Before the device starts.
    fn prime(&mut self, format: &Format) -> Result<(), Error> {
        let _ = format;
        Ok(())
    }

    /// Once for each of the driver's events, `time` after the start.
    fn period(&mut self, format: &Format, time: Duration) -> Result<(), Error>;
}

/// An output's part: `render` fills the device's buffer.
struct Player<R> {
    service: IAudioRenderClient,
    render: R,
}

impl<R: FnMut(Samples<'_>, Duration)> Player<R> {
    /// Takes the device's buffer, has `render` fill it, and hands it back.
    /// With no `time`, plays a period of silence without calling `render`.
    fn hand_over(&mut self, format: &Format, time: Option<Duration>) -> Result<(), Error> {
        let frames = format.period_frames;
        // SAFETY: in exclusive, event-driven mode the whole buffer is free
        // when the event fires, and before the stream starts.
        let data = unsafe { self.service.GetBuffer(frames) }.map_err(classify)?;
        let len = frames as usize * usize::from(format.channels);
        let mut flags = 0;
        match time {
            None => flags = AUDCLNT_BUFFERFLAGS_SILENT.0 as u32,
            Some(time) => {
                // SAFETY: `GetBuffer` returned `frames` frames of the format
                // the client was initialised with, writable and ours until
                // `ReleaseBuffer`.
                let samples = unsafe {
                    match format.kind {
                        SampleKind::F32 => silenced(data, len).map(Samples::F32),
                        SampleKind::I32 { .. } => silenced(data, len).map(Samples::I32),
                        SampleKind::I16 => silenced(data, len).map(Samples::I16),
                    }
                };
                match samples {
                    Some(samples) => (self.render)(samples, time),
                    None => flags = AUDCLNT_BUFFERFLAGS_SILENT.0 as u32,
                }
            }
        }
        // SAFETY: hands back exactly the frames taken.
        unsafe { self.service.ReleaseBuffer(frames, flags) }.map_err(classify)
    }
}

impl<R: FnMut(Samples<'_>, Duration)> Pump for Player<R> {
    /// A period of silence is in the buffer before the device starts, as
    /// exclusive mode asks.
    fn prime(&mut self, format: &Format) -> Result<(), Error> {
        self.hand_over(format, None)
    }

    fn period(&mut self, format: &Format, time: Duration) -> Result<(), Error> {
        self.hand_over(format, Some(time))
    }
}

/// An input's part: `capture` reads what the device recorded.
struct Recorder<C> {
    service: IAudioCaptureClient,
    reader: Reader,
    capture: C,
}

impl<C: FnMut(Packet<'_>, Duration)> Pump for Recorder<C> {
    /// Takes the packet recorded, has `capture` read it, and hands it back.
    /// Exclusive mode hands over one packet an event, a period long
    /// (`GetNextPacketSize` is shared mode's).
    fn period(&mut self, format: &Format, time: Duration) -> Result<(), Error> {
        let mut data = std::ptr::null_mut();
        let (mut frames, mut flags) = (0u32, 0u32);
        // SAFETY: the three pointers are to live locals; the packet's
        // positions are not asked for.
        unsafe {
            self.service
                .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
        }
        .map_err(classify)?;
        if frames == 0 {
            // Nothing was there (`AUDCLNT_S_BUFFER_EMPTY`, a success), so
            // nothing is held; handing back nothing is harmless either way.
            // SAFETY: no frames are handed back.
            let _ = unsafe { self.service.ReleaseBuffer(0) };
            return Ok(());
        }
        let len = frames as usize * usize::from(format.channels);
        let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
        let discontinuity = flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0;
        let capture = &mut self.capture;
        // SAFETY: `GetBuffer` returned `frames` frames of the format the
        // client was initialised with, which is the reader's, readable and
        // unchanged until `ReleaseBuffer`.
        unsafe {
            self.reader
                .read(data, len, silent, discontinuity, &mut |packet| {
                    capture(packet, time);
                });
        }
        // SAFETY: hands back exactly the frames taken.
        unsafe { self.service.ReleaseBuffer(frames) }.map_err(classify)
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
    spawn("manguebyte-wasapi", on_error, move || {
        let opened = open(&id, request, eRender)?;
        // SAFETY: the client is initialised for rendering.
        let service: IAudioRenderClient =
            unsafe { opened.client.GetService() }.map_err(classify)?;
        Ok((opened, Player { service, render }))
    })
}

pub(crate) fn start_capture<C, E>(
    id: String,
    request: Request,
    capture: C,
    on_error: E,
) -> Result<(Running, Format), Error>
where
    C: FnMut(Packet<'_>, Duration) + Send + 'static,
    E: FnMut(Error) + Send + 'static,
{
    spawn("manguebyte-wasapi-capture", on_error, move || {
        let opened = open(&id, request, eCapture)?;
        // SAFETY: the client is initialised for capturing.
        let service: IAudioCaptureClient =
            unsafe { opened.client.GetService() }.map_err(classify)?;
        let period = opened.format.period_frames as usize * usize::from(opened.format.channels);
        let recorder = Recorder {
            service,
            reader: Reader::new(opened.format.kind, period),
            capture,
        };
        Ok((opened, recorder))
    })
}

/// Starts a stream's thread, which makes its device with `prepare` and runs
/// it, and waits until it runs or fails.
fn spawn<P, S, E>(name: &str, on_error: E, prepare: S) -> Result<(Running, Format), Error>
where
    P: Pump,
    S: FnOnce() -> Result<(Opened, P), Error> + Send + 'static,
    E: FnMut(Error) + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let (ready, started) = mpsc::sync_channel(1);
    let stopping = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name(String::from(name))
        .spawn(move || run(prepare, on_error, &stopping, &ready))
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

fn run<P, S, E>(prepare: S, mut on_error: E, stop: &AtomicBool, ready: &Ready)
where
    P: Pump,
    S: FnOnce() -> Result<(Opened, P), Error>,
    E: FnMut(Error),
{
    let _apartment = Apartment::enter();
    let (opened, mut pump) = match prepare() {
        Ok(prepared) => prepared,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let _priority = ProAudio::join();
    // SAFETY: the client is initialised and not yet started.
    let begun = pump
        .prime(&opened.format)
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
        if let Err(error) = pump.period(&opened.format, origin.elapsed()) {
            on_error(error);
            break;
        }
    }
    // SAFETY: the client is initialised; stopping a stopped one is harmless.
    let _ = unsafe { opened.client.Stop() };
}

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::E_ACCESSDENIED;
    use windows::Win32::Media::Audio::{
        AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED, AUDCLNT_E_DEVICE_IN_USE, AUDCLNT_E_DEVICE_INVALIDATED,
        AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED, AUDCLNT_E_SERVICE_NOT_RUNNING,
        AUDCLNT_E_UNSUPPORTED_FORMAT,
    };

    use super::*;

    #[test]
    fn the_codes_told_apart_are_the_systems() {
        for (ours, theirs) in [
            (code::DEVICE_INVALIDATED, AUDCLNT_E_DEVICE_INVALIDATED),
            (code::UNSUPPORTED_FORMAT, AUDCLNT_E_UNSUPPORTED_FORMAT),
            (code::DEVICE_IN_USE, AUDCLNT_E_DEVICE_IN_USE),
            (
                code::EXCLUSIVE_MODE_NOT_ALLOWED,
                AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED,
            ),
            (code::SERVICE_NOT_RUNNING, AUDCLNT_E_SERVICE_NOT_RUNNING),
            (
                code::BUFFER_SIZE_NOT_ALIGNED,
                AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED,
            ),
            (code::ACCESS_DENIED, E_ACCESSDENIED),
        ] {
            assert_eq!(ours, theirs.0);
        }
    }

    #[test]
    fn a_plain_header_has_no_extension() {
        let [.., plain] = candidates(None)[..] else {
            panic!("there are formats to try");
        };
        let format = wave_format(plain, 48_000, 2);
        let (tag, size, align) = (
            format.Format.wFormatTag,
            format.Format.cbSize,
            format.Format.nBlockAlign,
        );
        assert_eq!((u32::from(tag), size, align), (WAVE_FORMAT_PCM, 0, 4));
        let format = wave_format(candidates(None)[0], 48_000, 2);
        let (tag, size) = (format.Format.wFormatTag, format.Format.cbSize);
        assert_eq!((u32::from(tag), size), (WAVE_FORMAT_EXTENSIBLE, 22));
    }
}
