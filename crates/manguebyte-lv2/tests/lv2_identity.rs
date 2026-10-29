//! A plugin whose identity is worked out at run time, as its LV2
//! descriptor and its bundle's Turtle give it.

use std::sync::Arc;

use manguebyte_core::AudioBuffer;
use manguebyte_dsp::{ProcessContext, ProcessSpec};
use manguebyte_params::param_set;
use manguebyte_plugin::{AudioLayout, Plugin, PluginInfo, PluginKind};

struct NoParams {}
param_set!(NoParams {});

/// Compiled as `org.manguebyte.test.compiled`; says it is
/// `org.manguebyte.test.found` once loaded.
struct Found(Arc<NoParams>);

impl Plugin for Found {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.compiled",
        name: "Compiled",
        vendor: "Compiled vendor",
        version: "0.0.1",
        description: "",
        kind: PluginKind::Effect,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT;
    type Params = NoParams;

    fn info() -> PluginInfo {
        PluginInfo {
            id: "org.manguebyte.test.found",
            name: "Found",
            vendor: "Found vendor",
            ..Self::INFO
        }
    }
    fn new() -> Self {
        Self(Arc::new(NoParams {}))
    }
    fn params(&self) -> Arc<NoParams> {
        Arc::clone(&self.0)
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
}

manguebyte_lv2::export_lv2!(Found);

#[test]
fn the_descriptor_and_the_turtle_carry_the_run_time_uri() {
    let uri = "urn:manguebyte:org.manguebyte.test.found";
    assert_eq!(manguebyte_lv2::plugin_uri::<Found>(), uri);
    let descriptor = manguebyte_lv2::descriptor!(Found);
    assert_eq!(descriptor.uri().to_str(), Ok(uri));
    // Built once: the entry point hands out the same descriptor.
    assert!(std::ptr::eq(
        descriptor,
        manguebyte_lv2::plugin_descriptor::<Found>()
    ));
    assert_eq!(lv2_descriptor(0), descriptor.as_ptr());
    assert!(lv2_descriptor(1).is_null());

    let files = __manguebyte_lv2_bundle_files("found.dll");
    let all: String = files.iter().map(|(_, text)| text.as_str()).collect();
    assert!(all.contains(&format!("<{uri}>")), "{all}");
    assert!(all.contains("\"Found\""), "{all}");
    assert!(!all.contains("test.compiled"), "{all}");
}
