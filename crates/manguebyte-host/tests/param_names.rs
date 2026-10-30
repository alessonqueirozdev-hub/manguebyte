//! Hosts a plugin that renames its parameters while it runs, as an organ
//! that swaps whole instruments does, through the framework's own CLAP and
//! VST 3 wrappers, linked into this test and reached through their entry
//! points, in process.
//!
//! The wrapper tells the host once per batch of renames (CLAP's
//! `rescan(CLAP_PARAM_RESCAN_INFO)`, VST 3's
//! `restartComponent(kParamTitlesChanged)`), and this host reads the
//! parameter list again when told, so the names it lists follow the
//! plugin's, while the ids stay the same.
//!
//! Not under Miri, which is too slow for the wrappers.
#![cfg(all(feature = "clap", feature = "vst3", not(miri)))]

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use manguebyte_core::{AudioBuffer, ParamId};
use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor};
use manguebyte_host::clap::{ClapLibrary, HostEvent as ClapEvent, ParamsRescan};
use manguebyte_host::vst3::{HostEvent as Vst3Event, Vst3Module};
use manguebyte_params::{BoolParam, Param, ParamSet};
use manguebyte_plugin::{AudioLayout, Plugin, PluginInfo, PluginKind};

const RATE: f64 = 48_000.0;
const STOPS: usize = 8;

/// A fixed pool of stops driven by number.
struct Stops(Vec<BoolParam>);

impl ParamSet for Stops {
    fn visit<'a>(&'a self, visitor: &mut dyn FnMut(&'a dyn Param)) {
        for stop in &self.0 {
            visitor(stop);
        }
    }
}

/// The stops of the organ made last, for the test to rename.
static MADE: Mutex<Option<Arc<Stops>>> = Mutex::new(None);

/// The tests share `MADE`, so they take turns.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn turn() -> MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Renames every stop after `organ`'s ranks, from another thread, as a
/// plugin's loader thread would.
fn load_organ(organ: &'static str) {
    let stops = MADE.lock().unwrap().clone().expect("an organ was made");
    std::thread::spawn(move || {
        for (n, stop) in stops.0.iter().enumerate() {
            stop.info()
                .set_display_name(&format!("{organ} rank {}", n + 1));
        }
    })
    .join()
    .unwrap();
}

struct Organ {
    params: Arc<Stops>,
}

impl Plugin for Organ {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.organ",
        name: "Organ",
        vendor: "Manguebyte",
        version: "0.1.0",
        description: "Renames its stops",
        kind: PluginKind::Effect,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT;
    type Params = Stops;

    fn new() -> Self {
        let params = Arc::new(Stops(
            (1..=STOPS)
                .map(|n| BoolParam::new(&format!("stop-{n:02}"), &format!("Stop {n}"), false))
                .collect(),
        ));
        *MADE.lock().unwrap() = Some(Arc::clone(&params));
        Self { params }
    }
    fn params(&self) -> Arc<Stops> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
}

static ORGAN_CLAP: manguebyte_clap::clap_sys::entry::clap_plugin_entry =
    manguebyte_clap::plugin_entry::<Organ>();

/// While blocks run, the audio side sees the renames and asks for a call
/// on the main thread; the host's `pump` makes it, the plugin asks for its
/// parameter information to be read again, once, and the host lists the
/// new names under the same ids.
#[test]
fn a_clap_host_lists_the_names_a_running_plugin_gave_its_parameters() {
    let _turn = turn();
    // SAFETY: the framework's own wrapper around a plugin of this test,
    // which keeps to CLAP and lives as long as the test.
    let library = unsafe { ClapLibrary::from_entry(&raw const ORGAN_CLAP, "organ") }.unwrap();
    let mut plugin = library.create(Organ::INFO.id).unwrap();
    assert_eq!(plugin.params()[0].name, "Stop 1");
    let ids: Vec<ParamId> = plugin.params().iter().map(|param| param.id).collect();
    let mut processor = plugin.activate(RATE, 256).unwrap();
    let mut audio = AudioBuffer::new(2, 256);
    processor.process(&mut audio);
    assert_eq!(plugin.pump(), []);

    load_organ("Haarlem");
    processor.process(&mut audio);
    processor.process(&mut audio);
    let info = ParamsRescan {
        info: true,
        ..ParamsRescan::default()
    };
    assert_eq!(plugin.pump(), [ClapEvent::ParamsRescanned(info)]);
    let names: Vec<&str> = plugin
        .params()
        .iter()
        .map(|param| param.name.as_str())
        .collect();
    let expected: Vec<String> = (1..=STOPS).map(|n| format!("Haarlem rank {n}")).collect();
    assert_eq!(names, expected);
    assert_eq!(
        plugin
            .params()
            .iter()
            .map(|param| param.id)
            .collect::<Vec<_>>(),
        ids
    );
    // Told once: nothing more until the next organ.
    processor.process(&mut audio);
    assert_eq!(plugin.pump(), []);
    drop(processor);
    plugin.deactivate().unwrap();
}

/// VST 3 has no call on the main thread of the plugin's own, so the
/// renames are told at the next call that may carry them: here the
/// host's activation. The host then reads every title again.
#[test]
fn a_vst3_host_lists_the_titles_a_plugin_gave_its_parameters() {
    let _turn = turn();
    // SAFETY: as for CLAP, with the wrapper's own factory.
    let module =
        unsafe { Vst3Module::from_factory(manguebyte_vst3::plugin_factory::<Organ>(), "organ") }
            .unwrap();
    let mut plugin = module.create(module.classes()[0].id).unwrap();
    assert_eq!(plugin.params()[0].title, "Stop 1");
    let processor = plugin.activate(RATE, 256).unwrap();
    plugin.pump();
    drop(processor);
    plugin.deactivate().unwrap();
    plugin.pump();

    load_organ("Alkmaar");
    let processor = plugin.activate(RATE, 256).unwrap();
    let events = plugin.pump();
    assert!(
        events.contains(&Vst3Event::ParamsRescanned {
            values: false,
            info: true
        }),
        "{events:?}"
    );
    assert_eq!(plugin.params()[7].title, "Alkmaar rank 8");
    assert_eq!(plugin.params()[7].short_title, "Alkmaar rank 8");
    assert_eq!(plugin.pump(), []);
    drop(processor);
    plugin.deactivate().unwrap();
}
