//! Drives a VST 3 offline render through the COM interface, as a host does:
//! a processor budget governor in the plugin cuts nothing while the setup,
//! or the block, says the processing is offline.
//!
//! Every `unsafe` block in this file is a call across that interface with
//! pointers the test owns and keeps alive for the duration of the call, so
//! the blocks are not commented one by one.
#![allow(clippy::undocumented_unsafe_blocks)]

use std::ffi::c_char;
use std::ptr;
use std::sync::Arc;

use manguebyte_core::AudioBuffer;
use manguebyte_core::governor::{Allowance, Governor, GovernorConfig};
use manguebyte_dsp::event::NoteRef;
use manguebyte_dsp::instruments::{OrganParams, OrganVoice};
use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor, Synth};
use manguebyte_params::param_set;
use manguebyte_plugin::{AudioLayout, Plugin, PluginInfo, PluginKind};
use manguebyte_vst3::vst3::Steinberg::Vst::{
    self as vst, IAudioProcessor, IAudioProcessorTrait, IComponent, IComponentTrait, IEventList,
    IEventListTrait,
};
use manguebyte_vst3::vst3::Steinberg::{
    IPluginBaseTrait, IPluginFactory, IPluginFactoryTrait, kResultFalse, kResultOk, tresult,
};
use manguebyte_vst3::vst3::{Class, ComPtr, ComWrapper, Interface};
use manguebyte_vst3::{class_id, plugin_factory};

const REALTIME: i32 = vst::ProcessModes_::kRealtime as i32;
const PREFETCH: i32 = vst::ProcessModes_::kPrefetch as i32;
const OFFLINE: i32 = vst::ProcessModes_::kOffline as i32;
const BLOCK: usize = 64;

struct NoParams {}
param_set!(NoParams {});

/// An organ that renders within a governor whose maximum is nothing: live it
/// is silent, so anything it plays it was told to render offline. It does
/// nothing about offline renders itself.
struct Thrifty {
    organ: Synth<OrganVoice>,
    governor: Governor,
    params: Arc<NoParams>,
}

impl Plugin for Thrifty {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.vst3-thrifty",
        name: "Thrifty",
        vendor: "Manguebyte",
        version: "0.1.0",
        description: "Plays only offline",
        kind: PluginKind::Instrument,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
    type Params = NoParams;

    fn new() -> Self {
        let mut governor = Governor::new(GovernorConfig::default());
        governor.set_max_budget(0.0);
        Self {
            organ: Synth::new(OrganVoice::voices(4, OrganParams::jazz())),
            governor,
            params: Arc::new(NoParams {}),
        }
    }
    fn params(&self) -> Arc<NoParams> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.organ.prepare(spec);
    }
    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        self.organ
            .process_governed(buffer, context, &mut self.governor);
    }
    fn reset(&mut self) {
        self.organ.reset();
    }
}

/// The host's input events.
struct Events(Vec<vst::Event>);

impl Class for Events {
    type Interfaces = (IEventList,);
}

impl IEventListTrait for Events {
    unsafe fn getEventCount(&self) -> i32 {
        self.0.len() as i32
    }
    unsafe fn getEvent(&self, index: i32, event: *mut vst::Event) -> tresult {
        let Some(found) = self.0.get(index as usize) else {
            return kResultFalse;
        };
        unsafe { *event = *found };
        kResultOk
    }
    unsafe fn addEvent(&self, _: *mut vst::Event) -> tresult {
        kResultFalse
    }
}

fn note_on(pitch: i16) -> vst::Event {
    let mut event: vst::Event = unsafe { std::mem::zeroed() };
    event.r#type = vst::Event_::EventTypes_::kNoteOnEvent as u16;
    event.__field0.noteOn = vst::NoteOnEvent {
        channel: 0,
        pitch,
        tuning: 0.0,
        velocity: 1.0,
        length: 0,
        noteId: -1,
    };
    event
}

struct Host {
    component: ComPtr<IComponent>,
    processor: ComPtr<IAudioProcessor>,
}

impl Host {
    fn new() -> Self {
        unsafe {
            let factory =
                ComPtr::<IPluginFactory>::from_raw(plugin_factory::<Thrifty>().cast()).unwrap();
            let class = class_id(Thrifty::INFO.id);
            let mut object = ptr::null_mut();
            let iid = IComponent::IID.as_ptr().cast::<c_char>();
            let result = factory.createInstance(class.as_ptr(), iid, &mut object);
            assert_eq!(result, kResultOk);
            let component = ComPtr::<IComponent>::from_raw(object.cast()).unwrap();
            assert_eq!(component.initialize(ptr::null_mut()), kResultOk);
            let processor: ComPtr<IAudioProcessor> = component.cast().unwrap();
            Self {
                component,
                processor,
            }
        }
    }

    /// Sets processing up in `mode` and activates.
    fn start(&self, mode: i32) {
        let mut setup = vst::ProcessSetup {
            processMode: mode,
            symbolicSampleSize: vst::SymbolicSampleSizes_::kSample32 as i32,
            maxSamplesPerBlock: BLOCK as i32,
            sampleRate: 48_000.0,
        };
        unsafe {
            assert_eq!(self.processor.setupProcessing(&mut setup), kResultOk);
            assert_eq!(self.component.setActive(1), kResultOk);
        }
    }

    /// Deactivates, which resets the plugin.
    fn stop(&self) {
        unsafe { assert_eq!(self.component.setActive(0), kResultOk) };
    }

    /// One block whose data says `block_mode`, with these events; returns
    /// the left channel.
    fn block(&self, block_mode: i32, events: Vec<vst::Event>) -> Vec<f32> {
        let mut channels = vec![vec![0.0f32; BLOCK]; 2];
        let mut pointers: Vec<*mut f32> = channels.iter_mut().map(|c| c.as_mut_ptr()).collect();
        let mut outputs = [vst::AudioBusBuffers {
            numChannels: 2,
            silenceFlags: 0,
            __field0: vst::AudioBusBuffers__type0 {
                channelBuffers32: pointers.as_mut_ptr(),
            },
        }];
        let input = ComWrapper::new(Events(events));
        let mut data = vst::ProcessData {
            processMode: block_mode,
            symbolicSampleSize: vst::SymbolicSampleSizes_::kSample32 as i32,
            numSamples: BLOCK as i32,
            numInputs: 0,
            numOutputs: 1,
            inputs: ptr::null_mut(),
            outputs: outputs.as_mut_ptr(),
            inputParameterChanges: ptr::null_mut(),
            outputParameterChanges: ptr::null_mut(),
            inputEvents: input.as_com_ref::<IEventList>().unwrap().as_ptr(),
            outputEvents: ptr::null_mut(),
            processContext: ptr::null_mut(),
        };
        unsafe { assert_eq!(self.processor.process(&mut data), kResultOk) };
        channels.swap_remove(0)
    }

    /// Plays two notes for one block, from a reset.
    fn play(&self, mode: i32, block_mode: i32) -> Vec<f32> {
        self.start(mode);
        let played = self.block(block_mode, vec![note_on(48), note_on(55)]);
        self.stop();
        played
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        unsafe { self.component.terminate() };
    }
}

#[test]
fn offline_processing_plays_everything_whatever_the_budget() {
    // What the organ plays with every voice it is asked for.
    let mut organ = Synth::new(OrganVoice::voices(4, OrganParams::jazz()));
    organ.prepare(&ProcessSpec {
        sample_rate: 48_000.0,
        max_block_size: BLOCK,
        num_channels: 2,
    });
    organ.note_on(NoteRef::new(0, 48), 1.0);
    organ.note_on(NoteRef::new(0, 55), 1.0);
    let mut expected = AudioBuffer::<f32>::new(2, BLOCK);
    organ.process(&mut expected);
    let expected = expected.channel(0).to_vec();
    assert!(expected.iter().any(|&sample| sample != 0.0));

    let host = Host::new();
    let silence = vec![0.0; BLOCK];
    assert_eq!(host.play(REALTIME, REALTIME), silence);
    // Offline as the setup and the block both say, or either alone.
    for (setup, block) in [(OFFLINE, OFFLINE), (OFFLINE, REALTIME), (REALTIME, OFFLINE)] {
        assert_eq!(host.play(setup, block), expected, "{setup} {block}");
    }
    // Prefetch is live processing, only early.
    assert_eq!(host.play(PREFETCH, PREFETCH), silence);
    // And back to live.
    assert_eq!(host.play(REALTIME, REALTIME), silence);
}

#[test]
fn switching_to_offline_without_a_reset_takes_effect_at_the_next_block() {
    // What the organ itself does when a budget of nothing is lifted after a
    // block: the voices fade back in from the first frame of the next.
    let mut organ = Synth::new(OrganVoice::voices(4, OrganParams::jazz()));
    organ.prepare(&ProcessSpec {
        sample_rate: 48_000.0,
        max_block_size: BLOCK,
        num_channels: 2,
    });
    organ.note_on(NoteRef::new(0, 48), 1.0);
    organ.note_on(NoteRef::new(0, 55), 1.0);
    let mut block = AudioBuffer::<f32>::new(2, BLOCK);
    let mut render = |context: &ProcessContext<'_, f32>| {
        organ.process_budgeted(&mut block, context, &mut Allowance::new(0.0));
        block.channel(0).to_vec()
    };
    render(&ProcessContext::empty());
    render(&ProcessContext::empty());
    let offline = ProcessContext::empty().with_offline(true);
    let expected: Vec<f32> = (0..8).flat_map(|_| render(&offline)).collect();

    let host = Host::new();
    // Live, the two held notes have no room and wait.
    host.start(REALTIME);
    let silence = vec![0.0; BLOCK];
    assert_eq!(
        host.block(REALTIME, vec![note_on(48), note_on(55)]),
        silence
    );
    assert_eq!(host.block(REALTIME, Vec::new()), silence);
    let played: Vec<f32> = (0..8)
        .flat_map(|_| host.block(OFFLINE, Vec::new()))
        .collect();
    assert_eq!(played, expected);
    let peak = |samples: &[f32]| samples.iter().fold(0.0f32, |top, &s| top.max(s.abs()));
    assert!(peak(&played[..BLOCK]) > 0.0);
    assert!(peak(&played[..32]) < 0.25 * peak(&played[288..]));

    // And back to live: the voices fade out again, and then not a sound.
    let live: Vec<f32> = (0..8)
        .flat_map(|_| host.block(REALTIME, Vec::new()))
        .collect();
    assert!(peak(&live[..BLOCK]) > 0.0);
    assert_eq!(live[320..], [0.0; 8 * BLOCK - 320]);
    host.stop();
}
