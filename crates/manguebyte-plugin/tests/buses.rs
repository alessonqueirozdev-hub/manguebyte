//! Auxiliary output buses through the harness: each bus gets what the
//! plugin writes into it, a bus switched off is neither given to the plugin
//! nor rendered, and the result does not depend on how blocks fall.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use manguebyte_core::AudioBuffer;
use manguebyte_dsp::{ProcessContext, ProcessSpec};
use manguebyte_params::param_set;
use manguebyte_plugin::output::OutputBuffer;
use manguebyte_plugin::{
    AudioLayout, AuxOutput, AuxOutputs, Harness, Plugin, PluginInfo, PluginKind,
};

struct NoParams {}
param_set!(NoParams {});

/// How many times each bus was given to the plugin.
static GIVEN: [AtomicUsize; 4] = [const { AtomicUsize::new(0) }; 4];

/// Writes, on bus `b` and channel `c`, frame `f` counted from the start of
/// the timeline, `(b + 1) + c / 10 + f / 100_000`: a ramp that tells every
/// bus, channel and frame apart.
struct Four {
    params: Arc<NoParams>,
    frame: u64,
}

impl Plugin for Four {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.four",
        name: "Four",
        vendor: "Manguebyte",
        version: "1.0.0",
        description: "Four buses",
        kind: PluginKind::Instrument,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
    const AUX_OUTPUTS: &'static [AuxOutput] = &[
        AuxOutput::stereo("One"),
        AuxOutput::stereo("Two"),
        AuxOutput::new("Three", 1),
        AuxOutput::stereo("Four").inactive(),
    ];
    type Params = NoParams;

    fn new() -> Self {
        Self {
            params: Arc::new(NoParams {}),
            frame: 0,
        }
    }
    fn params(&self) -> Arc<NoParams> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {
        unreachable!("the harness calls process_with_aux");
    }
    fn process_with_aux(
        &mut self,
        buffer: &mut AudioBuffer<f32>,
        _: &ProcessContext<'_, f32>,
        _: &mut OutputBuffer,
        aux: &mut AuxOutputs,
    ) {
        let frames = buffer.num_frames();
        buffer.channel_mut(0).fill(0.5);
        for (index, given) in GIVEN.iter().enumerate().take(aux.len()) {
            let Some(bus) = aux.bus_mut(index) else {
                continue;
            };
            given.fetch_add(1, Ordering::Relaxed);
            assert_eq!(bus.num_frames(), frames, "as long as the block");
            for (channel, samples) in bus.iter_channels_mut().enumerate() {
                assert!(samples.iter().all(|&s| s == 0.0), "given silent");
                for (offset, sample) in samples.iter_mut().enumerate() {
                    *sample = expected(index, channel, self.frame + offset as u64);
                }
            }
        }
        self.frame += frames as u64;
    }
}

fn expected(bus: usize, channel: usize, frame: u64) -> f32 {
    (bus + 1) as f32 + channel as f32 / 10.0 + frame as f32 / 100_000.0
}

fn check(harness: &Harness<Four>, bus: usize, from: u64, frames: usize) {
    let written = harness.aux_output(bus).expect("the bus is on");
    assert_eq!(written.num_frames(), frames);
    assert_eq!(written.num_channels(), Four::AUX_OUTPUTS[bus].channels);
    for (channel, samples) in written.iter_channels().enumerate() {
        for (offset, &sample) in samples.iter().enumerate() {
            assert_eq!(
                sample,
                expected(bus, channel, from + offset as u64),
                "bus {bus}, channel {channel}, frame {offset}"
            );
        }
    }
}

#[test]
fn each_bus_gets_its_own_signal_and_a_bus_switched_off_is_not_rendered() {
    let mut harness = Harness::<Four>::new(48_000.0, 64).with_block_pattern(&[64, 7, 33]);
    let main = harness.render(500);
    assert_eq!(
        main.channel(0),
        &[0.5; 500][..],
        "the main output as before"
    );
    for bus in 0..3 {
        check(&harness, bus, 0, 500);
    }
    assert!(harness.aux_output(3).is_none(), "off by default");
    assert_eq!(GIVEN[3].load(Ordering::Relaxed), 0, "never given");

    // Switched: the fourth on, the second off.
    let before = GIVEN[1].load(Ordering::Relaxed);
    harness.set_aux_active(3, true);
    harness.set_aux_active(1, false);
    harness.render(200);
    check(&harness, 3, 500, 200);
    check(&harness, 0, 500, 200);
    assert!(harness.aux_output(1).is_none());
    assert_eq!(
        GIVEN[1].load(Ordering::Relaxed),
        before,
        "not given once off"
    );
    assert!(GIVEN[3].load(Ordering::Relaxed) > 0);
    assert!(harness.aux_output(4).is_none(), "no such bus");
}

#[test]
#[should_panic(expected = "4 auxiliary outputs, not 5")]
fn switching_a_bus_the_plugin_lacks_is_refused() {
    Harness::<Four>::new(48_000.0, 64).set_aux_active(4, true);
}

#[test]
fn a_plugin_without_buses_runs_as_it_always_has() {
    struct Plain(Arc<NoParams>);
    impl Plugin for Plain {
        const INFO: PluginInfo = PluginInfo {
            id: "org.manguebyte.test.plain",
            name: "Plain",
            vendor: "Manguebyte",
            version: "1.0.0",
            description: "",
            kind: PluginKind::Effect,
        };
        const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT;
        type Params = NoParams;
        fn new() -> Self {
            Self(Arc::new(NoParams {}))
        }
        fn params(&self) -> Arc<NoParams> {
            Arc::clone(&self.0)
        }
        fn prepare(&mut self, _: &ProcessSpec) {}
        fn process(&mut self, buffer: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {
            buffer.apply_gain(2.0);
        }
    }
    let mut harness = Harness::<Plain>::new(48_000.0, 64);
    let mut input = AudioBuffer::new(2, 100);
    input.channel_mut(1).fill(0.25);
    assert_eq!(harness.process(&input).channel(1), &[0.5; 100][..]);
    assert!(harness.aux_output(0).is_none());
}
