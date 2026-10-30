//! A hosted LV2 plugin, as the thread that owns it deals with it.

use std::ffi::{CString, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use manguebyte_core::ParamId;
use manguebyte_core::log::LogLevel;
use manguebyte_core::spsc::{self, Producer};

use super::description::{Direction, Lv2PluginInfo, PortKind, PortProperties};
use super::features::{Features, PROVIDED};
use super::library::Lv2Library;
use super::processor::{Lv2Processor, Setup};
use super::state::{Property, State, Store, retrieve, store};
use super::sys::*;
use super::{Lv2Error, MAX_BLOCK_SIZE, MAX_LATENCY_FRAMES, QUEUE_CAPACITY};

/// Fewest bytes given to an atom port, enough for a block's worth of MIDI
/// and a transport position.
const ATOM_BYTES: usize = 128 << 10;

/// Most bytes given to an atom port, whatever its plugin asks for.
const MAX_ATOM_BYTES: usize = 16 << 20;

/// What a plugin says about one of its parameters: one of its control
/// inputs.
///
/// Values are in the port's own unit and range, from [`min`](Self::min) to
/// [`max`](Self::max): what the plugin reads from the port.
#[derive(Debug, Clone, PartialEq)]
pub struct Lv2Param {
    /// The number that stands for the parameter: [`ParamId::from_name`] of
    /// its symbol, so it stays the same as long as the symbol does, which
    /// LV2 asks plugins to keep.
    pub id: ParamId,
    /// The index of its port.
    pub port: u32,
    /// The port's symbol.
    pub symbol: String,
    /// The name shown to people.
    pub name: String,
    /// The unit's symbol, or empty.
    pub unit: String,
    /// The lowest value: `lv2:minimum`, or 0 if the plugin gives none.
    pub min: f64,
    /// The highest value: `lv2:maximum`, or 1 if the plugin gives none.
    pub max: f64,
    /// The value the parameter starts at: `lv2:default`, or the lowest
    /// value, held to the range.
    pub default: f64,
    /// How many steps the value moves in, or `None` for a continuous one:
    /// 1 for a toggle, the range for a whole number, one fewer than the
    /// values of `pprops:rangeSteps` or of an enumeration.
    pub steps: Option<u32>,
    /// The port's properties.
    pub properties: PortProperties,
    /// Named values, as `(value, label)`.
    pub scale_points: Vec<(f32, String)>,
}

impl Lv2Param {
    /// `value` held to the range, and to whole numbers where the port
    /// takes only those.
    pub fn hold(&self, value: f64) -> f64 {
        let value = value.clamp(self.min, self.max);
        // LV2 reads a toggle as on above zero and off at or below it.
        if self.properties.toggled {
            if value > 0.0 { self.max } else { self.min }
        } else if self.properties.integer || self.properties.enumeration {
            value.round().clamp(self.min, self.max)
        } else {
            value
        }
    }
}

/// What the audio half needs of a parameter, sorted by id.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ParamSlot {
    pub(crate) id: ParamId,
    /// Where it is in the plugin's list of parameters.
    pub(crate) index: u32,
    pub(crate) port: u32,
    pub(crate) min: f64,
    pub(crate) max: f64,
    pub(crate) toggled: bool,
    pub(crate) whole: bool,
}

impl ParamSlot {
    pub(crate) fn hold(&self, value: f64) -> f32 {
        let value = value.clamp(self.min, self.max);
        let value = if self.toggled {
            if value > 0.0 { self.max } else { self.min }
        } else if self.whole {
            value.round().clamp(self.min, self.max)
        } else {
            value
        };
        value as f32
    }
}

/// The slot of the parameter with this id.
pub(crate) fn find_param(slots: &[ParamSlot], id: ParamId) -> Option<&ParamSlot> {
    slots
        .binary_search_by_key(&id.0, |slot| slot.id.0)
        .ok()
        .and_then(|at| slots.get(at))
}

/// One audio output bus besides the main one.
#[derive(Debug, Clone, Default)]
pub(crate) struct AuxBus {
    /// The group's IRI, or `None` for ports in no group.
    pub(crate) group: Option<String>,
    /// Its ports, in index order.
    pub(crate) ports: Vec<u32>,
    /// Whether its ports are connected; a bus whose ports may all be left
    /// unconnected can be switched off.
    pub(crate) connected: bool,
}

/// Which port does what, worked out once from the description.
#[derive(Debug, Clone, Default)]
pub(crate) struct Layout {
    pub(crate) port_count: usize,
    pub(crate) main_inputs: Vec<u32>,
    pub(crate) sidechain_inputs: Vec<u32>,
    pub(crate) main_outputs: Vec<u32>,
    pub(crate) other_outputs: Vec<u32>,
    /// The audio output buses besides the main one: audio outputs in a
    /// port group other than the first output's, one bus per group, in
    /// the order the groups first appear.
    pub(crate) aux_outputs: Vec<AuxBus>,
    pub(crate) cv: Vec<u32>,
    /// Every control port, inputs and outputs, with its starting value.
    pub(crate) controls: Vec<(u32, f32)>,
    pub(crate) latency: Option<u32>,
    /// The atom input that carries MIDI and the transport.
    pub(crate) events: Option<u32>,
    pub(crate) events_take_midi: bool,
    pub(crate) events_take_time: bool,
    pub(crate) atom_inputs: Vec<u32>,
    pub(crate) atom_outputs: Vec<u32>,
    /// For each atom output, whether it says it carries `midi:MidiEvent`,
    /// and so is read after every call of `run`.
    pub(crate) midi_outputs: Vec<bool>,
    pub(crate) atom_bytes: usize,
    /// Ports of a kind this host does not know, which may stay unconnected.
    pub(crate) unconnected: Vec<u32>,
}

impl Layout {
    fn new(info: &Lv2PluginInfo) -> Result<Self, Lv2Error> {
        let mut layout = Self {
            port_count: info.ports.len(),
            atom_bytes: ATOM_BYTES,
            ..Self::default()
        };
        // The group of the first audio output, whose ports are the main
        // output.
        let mut main_group: Option<Option<&str>> = None;
        for port in &info.ports {
            let index = port.index;
            match (port.kind, port.direction) {
                (PortKind::Audio, Direction::Input) => {
                    if port.properties.side_chain {
                        layout.sidechain_inputs.push(index);
                    } else {
                        layout.main_inputs.push(index);
                    }
                }
                (PortKind::Audio, Direction::Output) => {
                    if port.properties.side_chain {
                        layout.other_outputs.push(index);
                    } else if main_group.get_or_insert(port.group.as_deref())
                        == &port.group.as_deref()
                    {
                        layout.main_outputs.push(index);
                    } else {
                        let bus = match layout
                            .aux_outputs
                            .iter_mut()
                            .position(|bus| bus.group == port.group)
                        {
                            Some(found) => found,
                            None => {
                                layout.aux_outputs.push(AuxBus {
                                    group: port.group.clone(),
                                    ports: Vec::new(),
                                    connected: true,
                                });
                                layout.aux_outputs.len() - 1
                            }
                        };
                        layout.aux_outputs[bus].ports.push(index);
                    }
                }
                (PortKind::Cv, _) => layout.cv.push(index),
                (PortKind::Control, direction) => {
                    let (min, max) = range(port.minimum, port.maximum);
                    let start = if port.is_designated("freeWheeling") {
                        0.0
                    } else if port.is_designated("enabled") {
                        max
                    } else {
                        f64::from(port.default.unwrap_or(min as f32)).clamp(min, max)
                    };
                    layout.controls.push((index, start as f32));
                    let reports = port.is_designated("latency") || port.properties.reports_latency;
                    if direction == Direction::Output && reports && layout.latency.is_none() {
                        layout.latency = Some(index);
                    }
                }
                (PortKind::Atom, Direction::Input) => layout.atom_inputs.push(index),
                (PortKind::Atom, Direction::Output) => {
                    layout.atom_outputs.push(index);
                    layout.midi_outputs.push(port.supports_midi);
                }
                _ => {
                    if !port.properties.connection_optional {
                        return Err(Lv2Error::Unsupported(
                            "a port of a kind this host cannot connect",
                        ));
                    }
                    layout.unconnected.push(index);
                }
            }
            if port.kind == PortKind::Atom {
                let wanted = port.minimum_size.unwrap_or(0);
                if wanted > MAX_ATOM_BYTES {
                    return Err(Lv2Error::Unsupported(
                        "an atom port asks for more memory than this host gives",
                    ));
                }
                layout.atom_bytes = layout.atom_bytes.max(wanted);
            }
        }
        // MIDI and the transport go to the atom input designated for
        // control, or else to the first that reads MIDI, or else to the
        // first there is.
        let inputs: Vec<_> = layout
            .atom_inputs
            .iter()
            .filter_map(|&index| info.ports.get(index as usize))
            .collect();
        let chosen = inputs
            .iter()
            .find(|port| port.is_designated("control"))
            .or_else(|| inputs.iter().find(|port| port.supports_midi))
            .or_else(|| inputs.first());
        if let Some(port) = chosen {
            layout.events = Some(port.index);
            layout.events_take_midi = port.supports_midi;
            layout.events_take_time = port.supports_time;
            layout.atom_inputs.retain(|&index| index != port.index);
        }
        Ok(layout)
    }
}

/// A port's range: 0 to 1 where the plugin gives none, the lower first.
fn range(minimum: Option<f32>, maximum: Option<f32>) -> (f64, f64) {
    let min = f64::from(minimum.unwrap_or(0.0));
    let max = maximum.map_or(if min < 1.0 { 1.0 } else { min }, f64::from);
    if min <= max { (min, max) } else { (max, min) }
}

/// The parameters of a plugin: its control inputs, leaving out the one
/// that says whether the host is running freely.
fn params_of(info: &Lv2PluginInfo) -> (Vec<Lv2Param>, Vec<ParamSlot>, usize) {
    let mut params = Vec::new();
    for port in &info.ports {
        if port.kind != PortKind::Control
            || port.direction != Direction::Input
            || port.is_designated("freeWheeling")
        {
            continue;
        }
        let (min, max) = range(port.minimum, port.maximum);
        let default = if port.is_designated("enabled") {
            max
        } else {
            f64::from(port.default.unwrap_or(min as f32)).clamp(min, max)
        };
        let properties = port.properties;
        let steps = if properties.toggled {
            Some(1)
        } else if properties.integer {
            Some((max - min).round().clamp(0.0, f64::from(u32::MAX)) as u32)
        } else if properties.enumeration && port.scale_points.len() > 1 {
            Some(port.scale_points.len() as u32 - 1)
        } else {
            port.range_steps.map(|steps| steps - 1)
        };
        params.push(Lv2Param {
            id: ParamId::from_name(&port.symbol),
            port: port.index,
            symbol: port.symbol.clone(),
            name: port.name.clone(),
            unit: port.unit.clone(),
            min,
            max,
            default,
            steps,
            properties,
            scale_points: port.scale_points.clone(),
        });
    }
    // Two symbols whose ids collide cannot be told apart: the first stays.
    let before = params.len();
    let mut seen: Vec<u32> = Vec::new();
    params.retain(|param| {
        if seen.contains(&param.id.0) {
            false
        } else {
            seen.push(param.id.0);
            true
        }
    });
    let dropped = before - params.len();
    let mut slots: Vec<ParamSlot> = params
        .iter()
        .enumerate()
        .map(|(index, param)| ParamSlot {
            id: param.id,
            index: index as u32,
            port: param.port,
            min: param.min,
            max: param.max,
            toggled: param.properties.toggled,
            whole: param.properties.integer || param.properties.enumeration,
        })
        .collect();
    slots.sort_by_key(|slot| slot.id.0);
    (params, slots, dropped)
}

// --- The instance ---------------------------------------------------------------

/// One LV2 instance and what it was created with.
pub(crate) struct Instance {
    pub(crate) table: LV2_Descriptor,
    pub(crate) handle: LV2_Handle,
    pub(crate) state: Option<LV2_State_Interface>,
    pub(crate) sample_rate: f64,
    pub(crate) max_block: usize,
    /// Held across every call of LV2's instantiation class (`activate`,
    /// `deactivate`, the state extension's `restore`) and `save`, which
    /// must not overlap them. `run` never takes it.
    pub(crate) lock: Mutex<()>,
    pub(crate) features: Features,
    /// Keeps the library loaded until the instance has been cleaned up.
    _library: Lv2Library,
}

// SAFETY: the handle is the instance LV2 created, whose functions LV2 lets
// any thread call within its threading classes; the calls that may not
// overlap are serialised by `lock` and by the halves' `&mut self`.
unsafe impl Send for Instance {}
// SAFETY: as above.
unsafe impl Sync for Instance {}

impl Instance {
    pub(crate) fn guard(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        if let Some(cleanup) = self.table.cleanup {
            // SAFETY: the instance is alive, and this is the last value
            // that holds it, so nothing else is calling it.
            unsafe { cleanup(self.handle) };
        }
    }
}

/// What the two halves share.
pub(crate) struct Shared {
    pub(crate) instance: Instance,
    pub(crate) processor_alive: AtomicBool,
    /// The value of each parameter, by its place in the list, as the audio
    /// half last left it.
    pub(crate) values: Box<[AtomicU32]>,
    pub(crate) latency: AtomicU32,
    pub(crate) lost_events: AtomicU64,
    pub(crate) undeliverable_events: AtomicU64,
    pub(crate) failed_blocks: AtomicU64,
    /// Set when a block was silent because of the sample rate.
    pub(crate) wrong_rate: AtomicBool,
}

/// What the main half sends to the audio half.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ToAudio {
    /// A control input is to take this value from the next block on.
    Param { port: u32, value: f32 },
}

/// Why a block came out silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Lv2Failure {
    /// The processor was prepared for a sample rate other than the one the
    /// plugin was created with. Drop it and activate the plugin again at
    /// the new rate.
    SampleRateMismatch,
}

/// Something to know about a plugin, as returned by [`Lv2Plugin::pump`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Lv2Event {
    /// The plugin's latency is now this many frames.
    LatencyChanged(u32),
    /// At least one block came out silent, for this reason, since the last
    /// call.
    ProcessFailed(Lv2Failure),
    /// A message the plugin logged.
    Log {
        /// How serious it is.
        level: LogLevel,
        /// What it says.
        message: String,
    },
    /// Something the host noticed about the plugin.
    Remark(String),
}

/// A hosted LV2 plugin, as the thread that owns it deals with it: its
/// parameters, its state, and activation, which hands out the
/// [`Lv2Processor`] that runs it.
///
/// LV2 has no main thread: a plugin's functions are sorted into classes
/// that must not overlap, and this type and its processor keep them apart,
/// so both can be sent to any thread.
///
/// ```
/// fn can_be_sent<T: Send>() {}
/// can_be_sent::<manguebyte_host::lv2::Lv2Plugin>();
/// can_be_sent::<manguebyte_host::lv2::Lv2Processor>();
/// ```
///
/// Dropping the plugin while its processor exists leaves the instance to
/// the processor, which cleans it up when it is dropped in turn.
pub struct Lv2Plugin {
    library: Lv2Library,
    info: Lv2PluginInfo,
    layout: Arc<Layout>,
    params: Vec<Lv2Param>,
    slots: Arc<[ParamSlot]>,
    shared: Arc<Shared>,
    /// Each parameter's value while no processor exists.
    values: Vec<f32>,
    link: Option<Producer<ToAudio>>,
    waiting: Vec<Lv2Event>,
    latency: u32,
}

impl Lv2Plugin {
    pub(crate) fn create(
        library: Lv2Library,
        uri: &str,
        sample_rate: f64,
        max_block: usize,
    ) -> Result<Self, Lv2Error> {
        check_rate_and_block(sample_rate, max_block)?;
        let info = library
            .plugin(uri)
            .cloned()
            .ok_or_else(|| Lv2Error::UnknownPlugin(uri.to_owned()))?;
        if let Some(missing) = info
            .required_features
            .iter()
            .find(|feature| !PROVIDED.contains(&feature.as_str()))
        {
            return Err(Lv2Error::MissingFeature(missing.clone()));
        }
        let layout = Arc::new(Layout::new(&info)?);
        let (params, slots, dropped) = params_of(&info);
        let values: Vec<f32> = params.iter().map(|param| param.default as f32).collect();
        let instance = instantiate(&library, &info, &layout, sample_rate, max_block)?;
        let shared = Arc::new(new_shared(instance, &values));
        let mut waiting = Vec::new();
        if dropped > 0 {
            waiting.push(Lv2Event::Remark(format!(
                "{dropped} control input(s) were left out: their symbols give the same id as \
                 another's"
            )));
        }
        Ok(Self {
            library,
            info,
            layout,
            params,
            slots: slots.into(),
            shared,
            values,
            link: None,
            waiting,
            latency: 0,
        })
    }

    /// What the bundle says about the plugin.
    pub fn info(&self) -> &Lv2PluginInfo {
        &self.info
    }

    /// The plugin's parameters: its control inputs, in port order.
    pub fn params(&self) -> &[Lv2Param] {
        &self.params
    }

    /// The parameter with this id, if the plugin has one.
    pub fn param(&self, id: ParamId) -> Option<&Lv2Param> {
        let slot = find_param(&self.slots, id)?;
        self.params.get(slot.index as usize)
    }

    /// How many main audio inputs, sidechain inputs and audio outputs the
    /// plugin has: each LV2 audio port is one channel.
    pub fn audio_channels(&self) -> (usize, usize, usize) {
        (
            self.layout.main_inputs.len(),
            self.layout.sidechain_inputs.len(),
            self.layout.main_outputs.len(),
        )
    }

    /// The plugin's audio output buses besides the main one, in their
    /// order, each as its name and channel count: the audio outputs in a
    /// port group other than the first output's, one bus per group. Their
    /// output is read with
    /// [`Lv2Processor::process_with_aux`](super::Lv2Processor::process_with_aux).
    pub fn aux_outputs(&self) -> Vec<(String, usize)> {
        let info = &self.info;
        self.layout
            .aux_outputs
            .iter()
            .map(|bus| {
                let named = bus.group.as_deref().and_then(|iri| {
                    info.port_groups
                        .iter()
                        .find(|group| group.iri == iri)
                        .map(|group| group.name.clone())
                });
                (named.unwrap_or_default(), bus.ports.len())
            })
            .collect()
    }

    /// Switches auxiliary output bus `index` of
    /// [`aux_outputs`](Self::aux_outputs) on or off from the next
    /// activation: off, its ports are left unconnected, which tells the
    /// plugin nobody listens to them, and the bus is silent. Every bus
    /// starts on.
    ///
    /// # Errors
    ///
    /// - [`Lv2Error::AlreadyActive`] while the plugin is active.
    /// - [`Lv2Error::InvalidArgument`] for a bus the plugin lacks.
    /// - [`Lv2Error::Unsupported`] to switch off a bus with a port the
    ///   plugin does not allow to be left unconnected.
    pub fn set_aux_output_active(&mut self, index: usize, active: bool) -> Result<(), Lv2Error> {
        if self.is_active() {
            return Err(Lv2Error::AlreadyActive);
        }
        let Some(bus) = self.layout.aux_outputs.get(index) else {
            return Err(Lv2Error::InvalidArgument("no such auxiliary output bus"));
        };
        let optional = bus.ports.iter().all(|&port| {
            self.info
                .ports
                .get(port as usize)
                .is_some_and(|port| port.properties.connection_optional)
        });
        if !active && !optional {
            return Err(Lv2Error::Unsupported(
                "a port of the bus may not be left unconnected",
            ));
        }
        Arc::make_mut(&mut self.layout).aux_outputs[index].connected = active;
        Ok(())
    }

    /// Whether the plugin takes MIDI: it has an atom input that says it
    /// carries `midi:MidiEvent`.
    pub fn takes_notes(&self) -> bool {
        self.layout.events.is_some() && self.layout.events_take_midi
    }

    /// The sample rate the instance was created for.
    pub fn sample_rate(&self) -> f64 {
        self.shared.instance.sample_rate
    }

    /// Takes back what a processor that has been dropped left behind.
    fn settle(&mut self) {
        if self.shared.processor_alive.load(Ordering::Acquire) || self.link.is_none() {
            return;
        }
        self.link = None;
        for (value, shared) in self.values.iter_mut().zip(self.shared.values.iter()) {
            *value = f32::from_bits(shared.load(Ordering::Relaxed));
        }
    }

    /// The current value of a parameter, in its port's unit.
    ///
    /// While a processor exists, this is the value the plugin was given in
    /// the last block, so a value set with [`set_param`](Self::set_param)
    /// shows here once a block has been processed.
    pub fn param_value(&self, id: ParamId) -> Option<f64> {
        let slot = find_param(&self.slots, id)?;
        let index = slot.index as usize;
        let value = if self.shared.processor_alive.load(Ordering::Acquire) {
            f32::from_bits(self.shared.values.get(index)?.load(Ordering::Relaxed))
        } else if self.link.is_some() {
            // A processor was dropped and this half has not settled yet.
            f32::from_bits(self.shared.values.get(index)?.load(Ordering::Relaxed))
        } else {
            *self.values.get(index)?
        };
        Some(f64::from(value))
    }

    /// Sets a parameter, in its port's unit, held to its range.
    ///
    /// While a processor exists, the value travels to it through a
    /// wait-free queue and the plugin reads it from the first frame of the
    /// next block. Otherwise it is stored at once.
    ///
    /// # Errors
    ///
    /// [`Lv2Error::UnknownParam`], [`Lv2Error::InvalidArgument`] for a
    /// value that is not a number, and [`Lv2Error::QueueFull`] if 1,024
    /// changes are already waiting for the audio thread.
    pub fn set_param(&mut self, id: ParamId, value: f64) -> Result<(), Lv2Error> {
        self.settle();
        let slot = *find_param(&self.slots, id).ok_or(Lv2Error::UnknownParam(id))?;
        if value.is_nan() {
            return Err(Lv2Error::InvalidArgument(
                "a parameter value has to be a number",
            ));
        }
        let value = slot.hold(value);
        if let Some(link) = &mut self.link {
            let message = ToAudio::Param {
                port: slot.port,
                value,
            };
            return link.push(message).map_err(|_| Lv2Error::QueueFull);
        }
        if let Some(stored) = self.values.get_mut(slot.index as usize) {
            *stored = value;
        }
        Ok(())
    }

    /// The text a person should see for `value` of a parameter: the label
    /// of a scale point it lands on, "On" or "Off" for a toggle, or the
    /// number with the unit.
    pub fn param_to_text(&self, id: ParamId, value: f64) -> Option<String> {
        let param = self.param(id)?;
        if value.is_nan() {
            return None;
        }
        let value = param.hold(value);
        if let Some((_, label)) = param
            .scale_points
            .iter()
            .find(|(point, _)| f64::from(*point) == value)
        {
            return Some(label.clone());
        }
        let number = if param.properties.toggled {
            return Some(if value > 0.0 { "On" } else { "Off" }.to_owned());
        } else if param.properties.integer {
            format!("{}", value as i64)
        } else {
            format!("{}", value as f32)
        };
        Some(if param.unit.is_empty() {
            number
        } else {
            format!("{number} {}", param.unit)
        })
    }

    /// The value a piece of text stands for: a scale point's label, "on"
    /// or "off" for a toggle, or a number, with or without the unit. Held
    /// to the range.
    pub fn text_to_param(&self, id: ParamId, text: &str) -> Option<f64> {
        let param = self.param(id)?;
        let text = text.trim();
        if let Some((value, _)) = param
            .scale_points
            .iter()
            .find(|(_, label)| label.eq_ignore_ascii_case(text))
        {
            return Some(param.hold(f64::from(*value)));
        }
        if param.properties.toggled {
            if text.eq_ignore_ascii_case("on") {
                return Some(param.hold(1.0));
            }
            if text.eq_ignore_ascii_case("off") {
                return Some(param.hold(0.0));
            }
        }
        let number = text
            .strip_suffix(param.unit.as_str())
            .filter(|_| !param.unit.is_empty())
            .unwrap_or(text)
            .trim();
        let value: f64 = number.parse().ok()?;
        value.is_finite().then(|| param.hold(value))
    }

    /// How many frames late the plugin's output is, as its latency port
    /// last said. Zero for a plugin without one. At most
    /// [`MAX_LATENCY_FRAMES`].
    pub fn latency(&self) -> u32 {
        self.shared.latency.load(Ordering::Relaxed)
    }

    // --- State -------------------------------------------------------------

    /// Everything the plugin needs to come back as it is now: the value of
    /// every parameter, by its port's symbol, and what the plugin stores
    /// through LV2's state extension, by URI. See [`load_state`](Self::load_state).
    ///
    /// LV2 lets this run while the processor runs.
    ///
    /// # Errors
    ///
    /// [`Lv2Error::StateSaveFailed`] if the plugin reports a failure or
    /// stores something that is not plain data, and
    /// [`Lv2Error::StateTooLarge`] past
    /// [`MAX_STATE_BYTES`](super::MAX_STATE_BYTES).
    pub fn save_state(&mut self) -> Result<Vec<u8>, Lv2Error> {
        self.settle();
        let ports = self
            .params
            .iter()
            .filter_map(|param| Some((param.symbol.clone(), self.param_value(param.id)? as f32)))
            .collect();
        let properties = save_properties(&self.shared.instance)?;
        State { ports, properties }
            .to_bytes()
            .ok_or(Lv2Error::StateTooLarge)
    }

    /// Gives the plugin back a state saved by [`save_state`](Self::save_state):
    /// every parameter whose symbol it names takes its value, and the
    /// plugin's own values are handed to its state extension.
    ///
    /// # Errors
    ///
    /// [`Lv2Error::ProcessorAlive`] while a processor exists, since LV2
    /// does not let a state be restored while the plugin runs;
    /// [`Lv2Error::BadState`] for bytes that are not such a state; and
    /// [`Lv2Error::StateLoadFailed`] if the plugin refuses its part.
    pub fn load_state(&mut self, bytes: &[u8]) -> Result<(), Lv2Error> {
        self.settle();
        if self.shared.processor_alive.load(Ordering::Acquire) {
            return Err(Lv2Error::ProcessorAlive);
        }
        let state = State::from_bytes(bytes).ok_or(Lv2Error::BadState)?;
        restore_properties(&self.shared.instance, &state.properties)?;
        for (symbol, value) in &state.ports {
            if let Some(index) = self.params.iter().position(|param| &param.symbol == symbol) {
                let held = self.params[index].hold(f64::from(*value)) as f32;
                self.values[index] = held;
            }
        }
        Ok(())
    }

    // --- Activation ----------------------------------------------------------

    /// Whether a processor exists: LV2's `activate` has been called and its
    /// `deactivate` has not.
    pub fn is_active(&self) -> bool {
        self.shared.processor_alive.load(Ordering::Acquire)
    }

    /// The same as [`is_active`](Self::is_active): in LV2 the instance is
    /// active for as long as its processor exists.
    pub fn has_processor(&self) -> bool {
        self.is_active()
    }

    /// Activates the plugin for `sample_rate`, in blocks of up to
    /// `max_block_size` frames, and returns the processor that runs it.
    ///
    /// LV2 fixes both when an instance is created. If they differ from
    /// what the instance was created with, it is created again, and its
    /// state, saved as [`save_state`](Self::save_state) saves it, is given
    /// to the new one before the old one is cleaned up.
    ///
    /// Everything processing will need is allocated here, and every port
    /// is connected.
    ///
    /// # Errors
    ///
    /// [`Lv2Error::AlreadyActive`] while a processor exists,
    /// [`Lv2Error::InvalidArgument`] for a sample rate that is not a
    /// positive number or a block size of zero or above
    /// [`MAX_BLOCK_SIZE`], and the errors of creating an instance.
    pub fn activate(
        &mut self,
        sample_rate: f64,
        max_block_size: usize,
    ) -> Result<Lv2Processor, Lv2Error> {
        self.settle();
        if self.is_active() {
            return Err(Lv2Error::AlreadyActive);
        }
        check_rate_and_block(sample_rate, max_block_size)?;
        let instance = &self.shared.instance;
        if instance.sample_rate != sample_rate || instance.max_block != max_block_size {
            let saved = save_properties(instance).unwrap_or_default();
            let fresh = instantiate(
                &self.library,
                &self.info,
                &self.layout,
                sample_rate,
                max_block_size,
            )?;
            if let Err(error) = restore_properties(&fresh, &saved) {
                self.waiting.push(Lv2Event::Remark(format!(
                    "the plugin, created again for {sample_rate} Hz, did not take its state \
                     back: {error}"
                )));
            }
            self.shared = Arc::new(new_shared(fresh, &self.values));
        }
        for (shared, value) in self.shared.values.iter().zip(&self.values) {
            shared.store(value.to_bits(), Ordering::Relaxed);
        }
        let (to_audio, from_main) = spsc::channel(QUEUE_CAPACITY);
        let processor = Lv2Processor::new(Setup {
            shared: Arc::clone(&self.shared),
            layout: Arc::clone(&self.layout),
            slots: Arc::clone(&self.slots),
            params: &self.params,
            values: &self.values,
            sample_rate,
            max_frames: max_block_size,
            from_main,
        });
        self.link = Some(to_audio);
        Ok(processor)
    }

    /// Does nothing once the processor has been dropped, which deactivates
    /// the instance: LV2's `deactivate` belongs with it. Kept so that a
    /// plugin of any format is put away the same way.
    ///
    /// # Errors
    ///
    /// [`Lv2Error::ProcessorAlive`] if the processor still exists.
    pub fn deactivate(&mut self) -> Result<(), Lv2Error> {
        if self.is_active() {
            return Err(Lv2Error::ProcessorAlive);
        }
        self.settle();
        Ok(())
    }

    // --- What happened -------------------------------------------------------

    /// How many blocks have come out silent, since the plugin was created.
    pub fn failed_blocks(&self) -> u64 {
        self.shared.failed_blocks.load(Ordering::Relaxed)
    }

    /// How many events have been dropped for want of room in a sequence
    /// or in the list that carries events to the next block.
    pub fn lost_events(&self) -> u64 {
        self.shared.lost_events.load(Ordering::Relaxed)
    }

    /// How many events have not been sent because the plugin has no way to
    /// receive them: notes for a plugin without a MIDI input, values for
    /// parameters it does not have, and what MIDI 1.0 cannot carry (note
    /// expression other than pressure, chokes, modulation).
    pub fn undeliverable_events(&self) -> u64 {
        self.shared.undeliverable_events.load(Ordering::Relaxed)
    }

    /// What there is to know since the last call: messages the plugin
    /// logged, a new latency, silent blocks, and the host's own remarks.
    pub fn pump(&mut self) -> Vec<Lv2Event> {
        self.settle();
        let latency = self.latency();
        if latency != self.latency {
            self.latency = latency;
            self.waiting.push(Lv2Event::LatencyChanged(latency));
        }
        if self.shared.wrong_rate.swap(false, Ordering::Relaxed) {
            self.waiting
                .push(Lv2Event::ProcessFailed(Lv2Failure::SampleRateMismatch));
        }
        let drained = self.shared.instance.features.log().drain();
        for record in drained.records {
            self.waiting.push(Lv2Event::Log {
                level: record.level,
                message: record.text,
            });
        }
        if drained.lost > 0 {
            self.waiting.push(Lv2Event::Remark(format!(
                "{} log message(s) from the plugin were dropped: it logs faster than it is read",
                drained.lost
            )));
        }
        std::mem::take(&mut self.waiting)
    }
}

impl std::fmt::Debug for Lv2Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lv2Plugin")
            .field("uri", &self.info.uri)
            .field("active", &self.is_active())
            .field("params", &self.params.len())
            .finish_non_exhaustive()
    }
}

fn check_rate_and_block(sample_rate: f64, max_block: usize) -> Result<(), Lv2Error> {
    if !(sample_rate.is_finite() && sample_rate > 0.0) {
        return Err(Lv2Error::InvalidArgument(
            "the sample rate has to be a positive number",
        ));
    }
    if max_block == 0 || max_block > MAX_BLOCK_SIZE {
        return Err(Lv2Error::InvalidArgument(
            "the block size has to be between 1 and MAX_BLOCK_SIZE",
        ));
    }
    Ok(())
}

fn new_shared(instance: Instance, values: &[f32]) -> Shared {
    Shared {
        instance,
        processor_alive: AtomicBool::new(false),
        values: values
            .iter()
            .map(|value| AtomicU32::new(value.to_bits()))
            .collect(),
        latency: AtomicU32::new(0),
        lost_events: AtomicU64::new(0),
        undeliverable_events: AtomicU64::new(0),
        failed_blocks: AtomicU64::new(0),
        wrong_rate: AtomicBool::new(false),
    }
}

/// Creates an instance of the plugin.
fn instantiate(
    library: &Lv2Library,
    info: &Lv2PluginInfo,
    layout: &Layout,
    sample_rate: f64,
    max_block: usize,
) -> Result<Instance, Lv2Error> {
    let descriptor = library
        .descriptor(&info.uri)
        .ok_or_else(|| Lv2Error::UnknownPlugin(info.uri.clone()))?;
    // SAFETY: a descriptor from the library, which LV2 keeps valid while
    // the library is loaded; the instance keeps it loaded.
    let table = unsafe { *descriptor };
    let (Some(create), Some(_), Some(_)) = (table.instantiate, table.connect_port, table.run)
    else {
        return Err(Lv2Error::Unsupported(
            "the plugin's descriptor lacks a function every plugin has",
        ));
    };
    let features = Features::new(sample_rate, max_block, layout.atom_bytes);
    // LV2 gives the bundle's path with a separator at the end.
    let mut bundle = info.bundle.to_string_lossy().into_owned();
    if !bundle.ends_with(std::path::MAIN_SEPARATOR) {
        bundle.push(std::path::MAIN_SEPARATOR);
    }
    let bundle = CString::new(bundle.replace('\0', "")).unwrap_or_default();
    // SAFETY: the descriptor's own function, with the descriptor, a
    // positive sample rate, a C string and the null-terminated feature
    // list, which lives as long as the instance.
    let handle = unsafe { create(descriptor, sample_rate, bundle.as_ptr(), features.list()) };
    if handle.is_null() {
        let why: Vec<String> = features
            .log()
            .drain()
            .records
            .into_iter()
            .map(|record| record.text)
            .collect();
        return Err(Lv2Error::InstantiateFailed(why.join("; ")));
    }
    let state = table.extension_data.and_then(|extension| {
        // SAFETY: the descriptor's own function and a C string.
        let data = unsafe { extension(LV2_STATE__interface.as_ptr()) };
        let data = data.cast::<LV2_State_Interface>();
        // SAFETY: not null, aligned, and the plugin's state interface,
        // valid while the library is loaded.
        (!data.is_null() && data.is_aligned()).then(|| unsafe { *data })
    });
    Ok(Instance {
        table,
        handle,
        state,
        sample_rate,
        max_block,
        lock: Mutex::new(()),
        features,
        _library: library.clone(),
    })
}

/// What the plugin stores through its state extension; nothing if it has
/// none.
fn save_properties(instance: &Instance) -> Result<Vec<Property>, Lv2Error> {
    let Some(save) = instance.state.and_then(|state| state.save) else {
        return Ok(Vec::new());
    };
    let mut target = Store {
        properties: Vec::new(),
        bytes: 0,
        too_large: false,
    };
    let features: [*const LV2_Feature; 1] = [ptr::null()];
    let status = {
        let _held = instance.guard();
        // SAFETY: the plugin's own function with its instance, the host's
        // store function and its target, which outlive the call, and an
        // empty feature list. The lock keeps it apart from activation and
        // restoring.
        unsafe {
            save(
                instance.handle,
                Some(store),
                (&raw mut target).cast::<c_void>(),
                LV2_STATE_IS_POD | LV2_STATE_IS_PORTABLE,
                features.as_ptr(),
            )
        }
    };
    if target.too_large {
        return Err(Lv2Error::StateTooLarge);
    }
    if status != LV2_STATE_SUCCESS {
        return Err(Lv2Error::StateSaveFailed(status));
    }
    Ok(target.properties)
}

/// Hands the plugin's own values back to its state extension.
fn restore_properties(instance: &Instance, properties: &[Property]) -> Result<(), Lv2Error> {
    let Some(restore) = instance.state.and_then(|state| state.restore) else {
        return Ok(());
    };
    if properties.is_empty() {
        return Ok(());
    }
    let list: Vec<Property> = properties.to_vec();
    let features: [*const LV2_Feature; 1] = [ptr::null()];
    let status = {
        let _held = instance.guard();
        // SAFETY: the plugin's own function with its instance, the host's
        // retrieve function and the list it serves from, which outlive the
        // call, and an empty feature list. No processor exists, and the
        // lock keeps it apart from `save`.
        unsafe {
            restore(
                instance.handle,
                Some(retrieve),
                (&raw const list).cast_mut().cast::<c_void>(),
                0,
                features.as_ptr(),
            )
        }
    };
    if status == LV2_STATE_SUCCESS {
        Ok(())
    } else {
        Err(Lv2Error::StateLoadFailed(status))
    }
}

pub(crate) fn clamp_latency(frames: f32) -> u32 {
    if frames.is_finite() && frames > 0.0 {
        (frames.round() as u64).min(u64::from(MAX_LATENCY_FRAMES)) as u32
    } else {
        0
    }
}
