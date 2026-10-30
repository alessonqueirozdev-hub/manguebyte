//! Auxiliary output buses: outputs a plugin has besides its main one.

use manguebyte_core::{AudioBuffer, Window};

use crate::Plugin;

/// An output bus a plugin has besides its main output, such as one
/// division of an organ, a reverb send, or one pad of a drum machine, so
/// that a host can mix and place each on its own.
///
/// A plugin lists them in [`Plugin::AUX_OUTPUTS`]. Each has a fixed name
/// and width: unlike the main buses, a host cannot make one wider or
/// narrower, only switch it on or off.
///
/// ```
/// use manguebyte_plugin::AuxOutput;
///
/// const DIVISIONS: &[AuxOutput] = &[
///     AuxOutput::stereo("Great"),
///     AuxOutput::stereo("Swell"),
///     AuxOutput::new("Pedal", 1),
///     AuxOutput::stereo("Reverb send").inactive(),
/// ];
/// assert_eq!(DIVISIONS[2].channels, 1);
/// assert!(DIVISIONS[0].active_by_default && !DIVISIONS[3].active_by_default);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AuxOutput {
    /// The name hosts show for the bus.
    pub name: &'static str,
    /// How many channels it has; more than zero.
    pub channels: usize,
    /// Whether the bus is on until the host switches it off. A host that
    /// cannot switch buses, or a format that has no switch, may leave it
    /// as it is either way: see [`Plugin::AUX_OUTPUTS`].
    pub active_by_default: bool,
}

impl AuxOutput {
    /// A bus of `channels` channels, on by default.
    pub const fn new(name: &'static str, channels: usize) -> Self {
        Self {
            name,
            channels,
            active_by_default: true,
        }
    }

    /// A stereo bus, on by default.
    pub const fn stereo(name: &'static str) -> Self {
        Self::new(name, 2)
    }

    /// The same bus, off until the host switches it on.
    #[must_use]
    pub const fn inactive(self) -> Self {
        Self {
            active_by_default: false,
            ..self
        }
    }
}

/// The auxiliary output buses of one block, as the plugin is given them in
/// [`Plugin::process_with_aux`].
///
/// Every bus the plugin declares in [`Plugin::AUX_OUTPUTS`] is here, in
/// that order. An active bus comes silent, the length of the block, for
/// the plugin to write into; an inactive one is not given at all
/// ([`bus_mut`](Self::bus_mut) returns `None`), so a plugin that renders
/// only what it is given spends nothing on a bus nobody listens to, and
/// the host receives silence from it.
///
/// Whatever runs the plugin (a wrapper, the standalone runner, the
/// [`Harness`](crate::Harness)) owns one, made with
/// [`for_plugin`](Self::for_plugin) while audio is stopped, and calls
/// [`start_block`](Self::start_block) before each block. Nothing after
/// `for_plugin` allocates.
///
/// ```
/// use manguebyte_plugin::{AuxOutput, AuxOutputs};
///
/// let mut aux = AuxOutputs::with_buses(&[AuxOutput::stereo("A"), AuxOutput::stereo("B")], 64);
/// aux.set_active(1, false);
/// aux.start_block(32);
/// // What a plugin does: render into the buses it is given.
/// for index in 0..aux.len() {
///     if let Some(bus) = aux.bus_mut(index) {
///         bus.channel_mut(0).fill(0.5);
///     }
/// }
/// assert_eq!(aux.bus(0).unwrap().channel(0), &[0.5; 32]);
/// assert!(aux.bus(1).is_none());
/// ```
#[derive(Debug, Clone, Default)]
pub struct AuxOutputs {
    buses: Vec<AudioBuffer<f32>>,
    active: Vec<bool>,
    /// What `narrow` took from each bus, while a block is cut in pieces.
    windows: Vec<Option<Window>>,
}

impl AuxOutputs {
    /// No buses, for a plugin that declares none. Does not allocate.
    pub const fn new() -> Self {
        Self {
            buses: Vec::new(),
            active: Vec::new(),
            windows: Vec::new(),
        }
    }

    /// The buses `P` declares, each with room for `max_frames` frames and
    /// switched on or off as it is by default. Allocates.
    pub fn for_plugin<P: Plugin>(max_frames: usize) -> Self {
        Self::with_buses(P::AUX_OUTPUTS, max_frames)
    }

    /// The buses `declared` lists, each with room for `max_frames` frames
    /// and switched on or off as it is by default. Allocates.
    pub fn with_buses(declared: &[AuxOutput], max_frames: usize) -> Self {
        Self {
            buses: declared
                .iter()
                .map(|bus| AudioBuffer::new(bus.channels, max_frames))
                .collect(),
            active: declared.iter().map(|bus| bus.active_by_default).collect(),
            windows: vec![None; declared.len()],
        }
    }

    /// How many buses there are, active or not.
    pub fn len(&self) -> usize {
        self.buses.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.buses.is_empty()
    }

    /// Whether bus `index` is switched on; `false` past the last bus.
    pub fn is_active(&self, index: usize) -> bool {
        self.active.get(index).copied().unwrap_or(false)
    }

    /// Switches bus `index` on or off, for whatever runs the plugin, from
    /// the host's choice. Does nothing past the last bus. Real-time safe,
    /// but a host changes it while audio is stopped.
    pub fn set_active(&mut self, index: usize, active: bool) {
        if let Some(slot) = self.active.get_mut(index) {
            *slot = active;
        }
    }

    /// Bus `index`, to write the block into, or `None` if it is switched
    /// off or there is no such bus.
    pub fn bus_mut(&mut self, index: usize) -> Option<&mut AudioBuffer<f32>> {
        if self.is_active(index) {
            self.buses.get_mut(index)
        } else {
            None
        }
    }

    /// Bus `index` as written, or `None` if it is switched off or there is
    /// no such bus.
    pub fn bus(&self, index: usize) -> Option<&AudioBuffer<f32>> {
        if self.is_active(index) {
            self.buses.get(index)
        } else {
            None
        }
    }

    /// Readies every bus for a block of `frames` frames: active ones are
    /// made silent, inactive ones are not touched. Real-time safe.
    ///
    /// # Panics
    ///
    /// Panics if `frames` is more than the buses have room for.
    pub fn start_block(&mut self, frames: usize) {
        let windows = self.windows.iter_mut();
        for ((bus, &active), window) in self.buses.iter_mut().zip(&self.active).zip(windows) {
            // Left narrowed by a piece whose plugin panicked.
            if let Some(window) = window.take() {
                bus.widen(window);
            }
            bus.set_num_frames(frames);
            if active {
                bus.clear();
            }
        }
    }

    /// Runs `work` with every bus narrowed to the frames in `range`, as
    /// [`AudioBuffer::windowed`] does for one buffer, for a block that is
    /// processed in pieces. Real-time safe. Not to be nested: `work` must
    /// not call it again.
    ///
    /// # Panics
    ///
    /// Panics if `range` does not lie within the block.
    pub fn windowed<R>(
        &mut self,
        range: core::ops::Range<usize>,
        work: impl FnOnce(&mut Self) -> R,
    ) -> R {
        for (bus, window) in self.buses.iter_mut().zip(self.windows.iter_mut()) {
            *window = Some(bus.narrow(range.clone()));
        }
        let result = work(self);
        for (bus, window) in self.buses.iter_mut().zip(self.windows.iter_mut()) {
            if let Some(window) = window.take() {
                bus.widen(window);
            }
        }
        result
    }
}
