//! What the Audio Unit host tells a unit and what it hears back, through
//! small plugins written here and hosted, through the framework's AU
//! wrapper, in this process: the transport the host callbacks answer for
//! each slice, a unit whose render fails, and a unit with latency.
//!
//! Not under Miri, to keep that job short.
#![cfg(all(feature = "au", not(miri)))]

mod common;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use manguebyte_core::AudioBuffer;
use manguebyte_dsp::event::EventBuffer;
use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor, Transport};
use manguebyte_host::au::{AuEvent, AuFailure, AuPlugin, ComponentCodes};
use manguebyte_params::{FloatParam, FloatRange, param_set};
use manguebyte_plugin::{AudioLayout, Plugin, PluginInfo, PluginKind};

use common::au_library;

const RATE: f64 = 48_000.0;

fn create<P: Plugin>() -> AuPlugin {
    let [kind, subtype, manufacturer] = manguebyte_au::codes(&P::INFO);
    au_library::<P>()
        .create(ComponentCodes::new(kind, subtype, manufacturer))
        .expect("the unit is made")
}

struct NoParams {
    gain: FloatParam,
}
param_set!(NoParams { gain });

fn no_params() -> Arc<NoParams> {
    Arc::new(NoParams {
        gain: FloatParam::new("gain", "Gain", FloatRange::linear(0.0, 1.0), 1.0),
    })
}

// --- The transport ------------------------------------------------------------

/// What each render of the clock saw of the transport, and how long it was.
static SEEN: Mutex<Vec<(Transport, usize)>> = Mutex::new(Vec::new());

/// An instrument that only writes down the transport it is given.
struct Clock {
    params: Arc<NoParams>,
}

impl Plugin for Clock {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.clock",
        name: "Clock",
        vendor: "Manguebyte",
        version: "1.0.0",
        description: "Writes down the transport",
        kind: PluginKind::Instrument,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
    type Params = NoParams;

    fn new() -> Self {
        Self {
            params: no_params(),
        }
    }
    fn params(&self) -> Arc<NoParams> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        SEEN.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((*context.transport, buffer.num_frames()));
        buffer.clear();
    }
}

#[test]
fn each_slice_is_told_where_the_music_is() {
    let mut plugin = create::<Clock>();
    // Slices of 256 frames, so that one block of 1,000 is four.
    let mut processor = plugin.activate(RATE, 256).unwrap();
    let mut transport = Transport::playing_at(120.0);
    transport.recording = true;
    transport.time_signature = Some((3, 4));
    transport.position_beats = Some(2.5);
    transport.bar_start_beats = Some(0.0);
    transport.position_samples = Some(60_000);
    transport.loop_beats = Some((0.0, 6.0));
    SEEN.lock().unwrap_or_else(PoisonError::into_inner).clear();
    let mut block = AudioBuffer::<f32>::new(2, 1_000);
    let events = EventBuffer::new();
    processor.process_with(&mut block, &ProcessContext::new(&events, &transport));
    let seen = std::mem::take(&mut *SEEN.lock().unwrap_or_else(PoisonError::into_inner));
    let lengths: Vec<usize> = seen.iter().map(|(_, frames)| *frames).collect();
    assert_eq!(lengths, [256, 256, 256, 232]);
    let beats_per_frame = 120.0 / 60.0 / RATE;
    for (index, (given, _)) in seen.iter().enumerate() {
        let start = 256 * index;
        assert!(given.playing && given.recording, "slice {index}");
        assert_eq!(given.tempo_bpm, Some(120.0));
        assert_eq!(given.time_signature, Some((3, 4)));
        assert_eq!(given.position_samples, Some(60_000 + start as i64));
        assert_eq!(given.loop_beats, Some((0.0, 6.0)));
        let beat = given.position_beats.unwrap();
        let expected = 2.5 + start as f64 * beats_per_frame;
        assert!(
            (beat - expected).abs() < 1e-9,
            "slice {index}: {beat} against {expected}"
        );
        assert_eq!(given.bar_start_beats, Some(0.0));
    }

    // Stopped, the unit hears that, and where the music stands.
    processor.process_with(
        &mut block,
        &ProcessContext::new(&events, &Transport::STOPPED),
    );
    let seen = std::mem::take(&mut *SEEN.lock().unwrap_or_else(PoisonError::into_inner));
    assert_eq!(seen.len(), 4);
    for (given, _) in &seen {
        assert!(!given.playing && !given.recording);
        assert_eq!(given.tempo_bpm, None, "no tempo is made up");
        assert_eq!(given.time_signature, None);
        assert_eq!(given.loop_beats, None);
    }
    drop(processor);
    plugin.deactivate().unwrap();
    assert_eq!(plugin.failed_blocks(), 0);
}

// --- A render that fails ----------------------------------------------------------

/// How many more renders the fragile effect gets through before it panics.
static RENDERS_LEFT: AtomicU32 = AtomicU32::new(u32::MAX);

/// An effect that passes its input through, until it panics: the wrapper
/// catches the panic at its boundary and fails the render.
struct Fragile {
    params: Arc<NoParams>,
}

impl Plugin for Fragile {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.fragile",
        name: "Fragile",
        vendor: "Manguebyte",
        version: "1.0.0",
        description: "Breaks",
        kind: PluginKind::Effect,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT;
    type Params = NoParams;

    fn new() -> Self {
        Self {
            params: no_params(),
        }
    }
    fn params(&self) -> Arc<NoParams> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, _buffer: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {
        let left = RENDERS_LEFT.load(Ordering::Relaxed);
        if left == 0 {
            panic!("the fragile effect broke, as it was written to");
        }
        RENDERS_LEFT.store(left - 1, Ordering::Relaxed);
    }
}

#[test]
fn a_render_that_fails_is_silent_and_reported() {
    let mut plugin = create::<Fragile>();
    let mut processor = plugin.activate(RATE, 128).unwrap();
    let mut input = AudioBuffer::<f32>::new(2, 128);
    input.channel_mut(0).fill(0.25);
    input.channel_mut(1).fill(-0.5);

    RENDERS_LEFT.store(1, Ordering::Relaxed);
    let mut audio = input.clone();
    processor.process(&mut audio);
    assert_eq!(audio, input, "the effect passes its input through");
    // Quiet the panic message of the one that fails.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut audio = input.clone();
    processor.process(&mut audio);
    std::panic::set_hook(previous);
    assert!(audio.channel(0).iter().all(|&sample| sample == 0.0));
    assert_eq!(plugin.failed_blocks(), 1);
    let events = plugin.pump();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AuEvent::ProcessFailed(AuFailure::Render(status)) if *status != 0)),
        "{events:?}"
    );
    RENDERS_LEFT.store(u32::MAX, Ordering::Relaxed);
    drop(processor);
    plugin.deactivate().unwrap();
}

// --- Latency ------------------------------------------------------------------------

/// The latency the late effect reports, in frames.
static LATENCY: AtomicU32 = AtomicU32::new(64);

/// An effect that reports a latency and does nothing else.
struct Late {
    params: Arc<NoParams>,
}

impl Plugin for Late {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.late",
        name: "Late",
        vendor: "Manguebyte",
        version: "1.0.0",
        description: "Late",
        kind: PluginKind::Effect,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT;
    type Params = NoParams;

    fn new() -> Self {
        Self {
            params: no_params(),
        }
    }
    fn params(&self) -> Arc<NoParams> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
    fn latency_samples(&self) -> usize {
        LATENCY.load(Ordering::Relaxed) as usize
    }
}

#[test]
fn the_latency_is_read_in_frames_and_followed() {
    let mut plugin = create::<Late>();
    assert_eq!(plugin.latency(), 0, "not known before activation");
    let mut processor = plugin.activate(RATE, 256).unwrap();
    assert_eq!(plugin.latency(), 64);
    assert_eq!(processor.latency_samples(), 64);
    assert!(plugin.pump().contains(&AuEvent::LatencyChanged(64)));
    // The unit reports another as it renders; the host reads it on pump.
    LATENCY.store(100, Ordering::Relaxed);
    let mut block = AudioBuffer::<f32>::new(2, 256);
    processor.process(&mut block);
    assert!(plugin.pump().contains(&AuEvent::LatencyChanged(100)));
    assert_eq!(processor.latency_samples(), 100);
    drop(processor);
    plugin.deactivate().unwrap();
}
