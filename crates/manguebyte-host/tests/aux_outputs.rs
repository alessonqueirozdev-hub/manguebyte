//! Auxiliary output buses through every format: an organ with four stereo
//! buses, one per division, linked into this test and hosted through the
//! framework's own wrappers, in process. Each host has to report the
//! buses by name and width, give each bus its own signal on the caller's
//! frames however the blocks fall, keep a bus that is switched off silent
//! and never have the plugin render it, and use no heap while it runs.
//!
//! Not under Miri, which is too slow for the wrappers.
#![cfg(all(
    any(feature = "clap", feature = "vst3", feature = "lv2", feature = "au"),
    not(miri)
))]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use manguebyte_core::AudioBuffer;
use manguebyte_dsp::event::EventBuffer;
use manguebyte_dsp::{ProcessContext, ProcessSpec, Transport};
use manguebyte_params::param_set;
use manguebyte_plugin::output::OutputBuffer;
use manguebyte_plugin::{AudioLayout, AuxOutput, AuxOutputs, Plugin, PluginInfo, PluginKind};

use common::heap_calls_during;

const RATE: f64 = 48_000.0;

/// The divisions, in the order the buses come after the main output. The
/// pedal is off until a host switches it on.
const DIVISIONS: &[AuxOutput] = &[
    AuxOutput::stereo("Great"),
    AuxOutput::stereo("Swell"),
    AuxOutput::stereo("Choir"),
    AuxOutput::stereo("Pedal").inactive(),
];

/// How many blocks each bus was given in, per format (the plugin's `F`).
static GIVEN: [[AtomicUsize; 4]; 4] = [const { [const { AtomicUsize::new(0) }; 4] }; 4];

struct NoParams {}
param_set!(NoParams {});

/// Writes 0.5 on the main output and, on channel `c` of bus `b`, from frame
/// `f` of the timeline on, `(b + 1) / 10 + c / 100 + f / 1_000_000`: every
/// bus, channel and frame its own value. `F` keeps the counts of each
/// format's test apart.
struct Organ<const F: usize> {
    params: Arc<NoParams>,
    frame: u64,
}

impl<const F: usize> Plugin for Organ<F> {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.organ",
        name: "Organ",
        vendor: "Manguebyte",
        version: "0.1.0",
        description: "Each division on a bus of its own",
        kind: PluginKind::Instrument,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
    const AUX_OUTPUTS: &'static [AuxOutput] = DIVISIONS;
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
    fn reset(&mut self) {
        self.frame = 0;
    }
    fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
    fn process_with_aux(
        &mut self,
        buffer: &mut AudioBuffer<f32>,
        _: &ProcessContext<'_, f32>,
        _: &mut OutputBuffer,
        aux: &mut AuxOutputs,
    ) {
        for channel in buffer.iter_channels_mut() {
            channel.fill(0.5);
        }
        for (bus, given) in GIVEN[F].iter().enumerate().take(aux.len()) {
            let Some(written) = aux.bus_mut(bus) else {
                continue;
            };
            given.fetch_add(1, Ordering::Relaxed);
            for (channel, samples) in written.iter_channels_mut().enumerate() {
                for (offset, sample) in samples.iter_mut().enumerate() {
                    *sample = level(bus, channel, self.frame + offset as u64);
                }
            }
        }
        self.frame += buffer.num_frames() as u64;
    }
}

fn level(bus: usize, channel: usize, frame: u64) -> f32 {
    (bus + 1) as f32 / 10.0 + channel as f32 / 100.0 + frame as f32 / 1_000_000.0
}

/// How the tests drive a processor: one block, with the auxiliary buses.
type Run<'a> =
    &'a mut dyn FnMut(&mut AudioBuffer<f32>, &ProcessContext<'_, f32>, &mut [AudioBuffer<f32>]);

/// Plays `blocks` through `run` with buffers that start full of noise, and
/// checks every frame: the main output, the three buses that are on, and
/// the pedal, which is off and has to come back silent. No heap may be
/// used while the plugin runs.
fn check_buses(format: usize, blocks: &[usize], run: Run<'_>) {
    check_buses_with(format, blocks, false, run);
}

/// [`check_buses`], with the pedal on or off.
fn check_buses_with(format: usize, blocks: &[usize], pedal_on: bool, run: Run<'_>) {
    let quiet = EventBuffer::new();
    let transport = Transport::STOPPED;
    let context = ProcessContext::new(&quiet, &transport);
    let longest = blocks.iter().copied().max().unwrap();
    let mut main = AudioBuffer::<f32>::new(2, longest);
    let mut aux: Vec<AudioBuffer<f32>> = (0..4).map(|_| AudioBuffer::new(2, longest)).collect();
    let pedal_before = GIVEN[format][3].load(Ordering::Relaxed);
    let mut frame = 0;
    for &length in blocks {
        main.set_num_frames(length);
        for bus in &mut aux {
            bus.set_num_frames(length);
            for channel in bus.iter_channels_mut() {
                channel.fill(9.0);
            }
        }
        let calls = heap_calls_during(|| run(&mut main, &context, &mut aux));
        assert_eq!(calls, 0, "the heap was used");
        for channel in main.iter_channels() {
            assert!(channel.iter().all(|&s| s == 0.5), "the main output");
        }
        for (bus, written) in aux.iter().enumerate() {
            for (channel, samples) in written.iter_channels().enumerate() {
                for (offset, &sample) in samples.iter().enumerate() {
                    let expected = if bus == 3 && !pedal_on {
                        0.0
                    } else {
                        level(bus, channel, frame + offset as u64)
                    };
                    assert_eq!(
                        sample, expected,
                        "bus {bus}, channel {channel}, frame {offset} of a block of {length}"
                    );
                }
            }
        }
        frame += length as u64;
    }
    for (bus, given) in GIVEN[format].iter().enumerate().take(3) {
        assert!(given.load(Ordering::Relaxed) > 0, "bus {bus}");
    }
    let pedal = GIVEN[format][3].load(Ordering::Relaxed);
    if pedal_on {
        assert!(pedal > pedal_before, "the pedal is on");
    } else {
        assert_eq!(pedal, pedal_before, "the pedal is off and never rendered");
    }
}

/// Blocks shorter, as long as and longer than the plugin's pieces (256).
const BLOCKS: &[usize] = &[256, 1, 100, 700, 256, 33];

// --- CLAP -----------------------------------------------------------------------

#[cfg(feature = "clap")]
mod clap {
    use manguebyte_host::clap::ClapLibrary;

    use super::*;

    static ORGAN: manguebyte_clap::clap_sys::entry::clap_plugin_entry =
        manguebyte_clap::plugin_entry::<Organ<0>>();

    #[test]
    fn a_clap_host_sees_each_division_on_a_port_of_its_own() {
        // SAFETY: the entry point is the framework's own CLAP wrapper
        // around a plugin of this test, callable for as long as it runs.
        let library = unsafe { ClapLibrary::from_entry(&raw const ORGAN, "organ") }.unwrap();
        let mut plugin = library.create(Organ::<0>::INFO.id).unwrap();
        let ports: Vec<(&str, u32, bool, Option<&str>)> = plugin
            .audio_outputs()
            .iter()
            .map(|port| {
                (
                    port.name.as_str(),
                    port.channels,
                    port.is_main,
                    port.port_type.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            ports,
            [
                ("Main", 2, true, Some("stereo")),
                ("Great", 2, false, Some("stereo")),
                ("Swell", 2, false, Some("stereo")),
                ("Choir", 2, false, Some("stereo")),
                ("Pedal", 2, false, Some("stereo")),
            ]
        );
        // CLAP has every port on until the host says otherwise; this host
        // switches the pedal off, and the main output stays on.
        plugin.set_audio_port_active(false, 4, false).unwrap();
        plugin.set_audio_port_active(false, 0, true).unwrap();
        assert!(plugin.set_audio_port_active(false, 5, false).is_err());

        let mut processor = plugin.activate(RATE, 256).unwrap();
        assert!(
            plugin.set_audio_port_active(false, 1, false).is_err(),
            "active"
        );
        check_buses(0, BLOCKS, &mut |main, context, aux| {
            processor.process_with_aux(main, context, aux);
        });
        drop(processor);
        plugin.deactivate().unwrap();
        assert_eq!(plugin.failed_blocks(), 0);
    }
}

// --- VST 3 ----------------------------------------------------------------------

#[cfg(feature = "vst3")]
mod vst3 {
    use manguebyte_host::vst3::Vst3Module;

    use super::*;

    #[test]
    fn a_vst3_host_sees_each_division_on_a_bus_of_its_own() {
        // SAFETY: the factory is the framework's own VST 3 wrapper around a
        // plugin of this test, callable for as long as it runs.
        let module = unsafe {
            Vst3Module::from_factory(manguebyte_vst3::plugin_factory::<Organ<1>>(), "organ")
        }
        .unwrap();
        let class = module.classes()[0].id;
        let mut plugin = module.create(class).unwrap();
        let buses: Vec<(&str, u32, bool, bool)> = plugin
            .audio_outputs()
            .iter()
            .map(|bus| {
                (
                    bus.name.as_str(),
                    bus.channels,
                    bus.is_main,
                    bus.default_active,
                )
            })
            .collect();
        assert_eq!(
            buses,
            [
                ("Output", 2, true, true),
                ("Great", 2, false, true),
                ("Swell", 2, false, true),
                ("Choir", 2, false, true),
                ("Pedal", 2, false, false),
            ]
        );
        assert!(plugin.set_audio_output_active(0, false).is_err(), "main");
        assert!(plugin.set_audio_output_active(5, true).is_err());

        // The pedal stays off, as the plugin wants it.
        let mut processor = plugin.activate(RATE, 256).unwrap();
        let active: Vec<bool> = plugin
            .audio_outputs()
            .iter()
            .map(|bus| bus.active)
            .collect();
        assert_eq!(active, [true, true, true, true, false]);
        check_buses(1, BLOCKS, &mut |main, context, aux| {
            processor.process_with_aux(main, context, aux);
        });
        drop(processor);
        plugin.deactivate().unwrap();

        // Switched on by the host, the pedal is heard; the swell, switched
        // off, is not given to the plugin.
        plugin.set_audio_output_active(4, true).unwrap();
        plugin.set_audio_output_active(2, false).unwrap();
        let mut processor = plugin.activate(RATE, 256).unwrap();
        let swell = GIVEN[1][1].load(Ordering::Relaxed);
        let quiet = EventBuffer::new();
        let transport = Transport::STOPPED;
        let context = ProcessContext::new(&quiet, &transport);
        let mut main = AudioBuffer::<f32>::new(2, 64);
        let mut aux: Vec<AudioBuffer<f32>> = (0..4).map(|_| AudioBuffer::new(2, 64)).collect();
        for bus in &mut aux {
            bus.channel_mut(0).fill(9.0);
        }
        processor.process_with_aux(&mut main, &context, &mut aux);
        // A new activation starts the plugin's timeline again.
        assert_eq!(aux[3].channel(1)[10], level(3, 1, 10));
        assert!(
            aux[1].channel(0).iter().all(|&s| s == 0.0),
            "the swell is off"
        );
        assert_eq!(GIVEN[1][1].load(Ordering::Relaxed), swell);
        drop(processor);
        plugin.deactivate().unwrap();
        assert_eq!(plugin.failed_blocks(), 0);
    }
}

// --- LV2 ------------------------------------------------------------------------

#[cfg(feature = "lv2")]
mod lv2 {
    use manguebyte_host::lv2::Lv2Library;

    use super::*;

    unsafe extern "C" fn entry(index: u32) -> *const std::ffi::c_void {
        if index == 0 {
            manguebyte_lv2::descriptor!(Organ<2>).as_ptr().cast()
        } else {
            std::ptr::null()
        }
    }

    #[test]
    fn an_lv2_host_sees_each_division_in_a_port_group_of_its_own() {
        // The bundle as the framework's wrapper writes it, with a stand-in
        // for the library, whose plugin comes from `entry`.
        let scratch = common::scratch_folder("aux-outputs-lv2");
        let folder = scratch.join("organ.lv2");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("plugin.so"), b"").unwrap();
        for (file, text) in manguebyte_lv2::bundle_files::<Organ<2>>("plugin.so").files() {
            std::fs::write(folder.join(file), text).unwrap();
        }
        // SAFETY: the entry is the framework's own LV2 wrapper around a
        // plugin of this test, callable for as long as it runs.
        let library = unsafe { Lv2Library::from_entry(&folder, entry) }.unwrap();
        let info = &library.plugins()[0];
        let names: Vec<&str> = info
            .port_groups
            .iter()
            .map(|group| group.name.as_str())
            .collect();
        assert_eq!(names, ["Great", "Swell", "Choir", "Pedal"]);

        let uri = info.uri.clone();
        let mut plugin = library.create_with(&uri, RATE, 256).unwrap();
        assert_eq!(plugin.audio_channels(), (0, 0, 2), "the main output alone");
        let buses = plugin.aux_outputs();
        let expected: Vec<(String, usize)> = DIVISIONS
            .iter()
            .map(|bus| (bus.name.to_owned(), bus.channels))
            .collect();
        assert_eq!(buses, expected);
        // LV2 has no switch but leaving ports unconnected, which the
        // wrapper allows for every auxiliary port.
        plugin.set_aux_output_active(3, false).unwrap();
        assert!(plugin.set_aux_output_active(4, false).is_err());

        let mut processor = plugin.activate(RATE, 256).unwrap();
        check_buses(2, BLOCKS, &mut |main, context, aux| {
            processor.process_with_aux(main, context, aux);
        });
        drop(processor);
        plugin.deactivate().unwrap();
        assert_eq!(plugin.failed_blocks(), 0);
    }
}

// --- Audio Units ----------------------------------------------------------------

#[cfg(feature = "au")]
mod au {
    use super::*;

    #[test]
    fn an_audio_unit_host_sees_each_division_on_an_element_of_its_own() {
        let library = common::au_library::<Organ<3>>();
        let codes = library.components()[0].codes;
        let mut plugin = library.create(codes).unwrap();
        let elements: Vec<(u32, &str, u32)> = plugin
            .audio_outputs()
            .iter()
            .map(|bus| (bus.element, bus.name.as_str(), bus.channels))
            .collect();
        assert_eq!(
            elements,
            [
                (0, "Output", 2),
                (1, "Great", 2),
                (2, "Swell", 2),
                (3, "Choir", 2),
                (4, "Pedal", 2),
            ]
        );
        // An Audio Unit cannot switch an element off: the pedal, off by
        // default elsewhere, is rendered too, once per slice whichever
        // element is pulled first.
        let mut processor = plugin.activate(RATE, 256).unwrap();
        check_buses_with(3, BLOCKS, true, &mut |main, context, aux| {
            processor.process_with_aux(main, context, aux);
        });
        drop(processor);
        plugin.deactivate().unwrap();
        assert_eq!(plugin.failed_blocks(), 0);
    }
}
