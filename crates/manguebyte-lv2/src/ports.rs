//! Which port has which index.

use manguebyte_plugin::{AudioLayout, AuxOutput};

/// What one port of the plugin carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PortRole {
    /// A channel of the main audio input, counted from zero.
    AudioInput(usize),
    /// A channel of the sidechain input, counted from zero.
    Sidechain(usize),
    /// A channel of the audio output, counted from zero.
    AudioOutput(usize),
    /// A channel of an auxiliary output bus: the bus's place in
    /// [`Plugin::AUX_OUTPUTS`](manguebyte_plugin::Plugin::AUX_OUTPUTS) and
    /// the channel in it, both counted from zero.
    AuxOutput {
        /// Which bus.
        bus: usize,
        /// Which of its channels.
        channel: usize,
    },
    /// The atom sequence of MIDI and transport events coming in.
    Control,
    /// The atom sequence going out.
    Notify,
    /// The control output that reports the latency in frames.
    Latency,
    /// The control input of a parameter, by its place in the parameter set.
    Param(usize),
}

/// The ports of a plugin, in index order: main audio inputs, sidechain
/// inputs, audio outputs, the channels of each auxiliary output bus in
/// turn, the control and notify atom ports, the latency output, then one
/// control input per parameter in the order the parameter set visits them.
///
/// Indices only change when the layout, the auxiliary buses or the
/// parameters do, and adding a parameter at the end of the set leaves
/// every earlier index as it was. A plugin without auxiliary buses has the
/// ports it had before they existed.
///
/// ```
/// use manguebyte_lv2::{PortLayout, PortRole};
/// use manguebyte_plugin::AudioLayout;
///
/// let ports = PortLayout::new(AudioLayout::STEREO_EFFECT, 3);
/// assert_eq!(ports.count(), 10);
/// assert_eq!(ports.index(PortRole::AudioOutput(1)), Some(3));
/// assert_eq!(ports.index(PortRole::Param(0)), Some(7));
/// assert_eq!(ports.role(6), Some(PortRole::Latency));
/// assert_eq!(ports.role(10), None);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PortLayout {
    inputs: usize,
    sidechain: usize,
    outputs: usize,
    aux: &'static [AuxOutput],
    params: usize,
}

impl PortLayout {
    /// The ports of a plugin with this audio layout and this many
    /// parameters.
    pub const fn new(layout: AudioLayout, params: usize) -> Self {
        Self {
            inputs: layout.inputs,
            sidechain: layout.sidechain,
            outputs: layout.outputs,
            aux: &[],
            params,
        }
    }

    /// The same ports with the channels of `aux` after the main outputs.
    ///
    /// ```
    /// use manguebyte_lv2::{PortLayout, PortRole};
    /// use manguebyte_plugin::{AudioLayout, AuxOutput};
    ///
    /// const BUSES: &[AuxOutput] = &[AuxOutput::stereo("Great"), AuxOutput::new("Pedal", 1)];
    /// let ports = PortLayout::new(AudioLayout::STEREO_INSTRUMENT, 1).with_aux(BUSES);
    /// assert_eq!(ports.index(PortRole::AuxOutput { bus: 1, channel: 0 }), Some(4));
    /// assert_eq!(ports.role(5), Some(PortRole::Control));
    /// ```
    #[must_use]
    pub const fn with_aux(self, aux: &'static [AuxOutput]) -> Self {
        Self { aux, ..self }
    }

    /// How many channels the auxiliary buses have together.
    const fn aux_channels(&self) -> usize {
        let mut total = 0;
        let mut bus = 0;
        while bus < self.aux.len() {
            total += self.aux[bus].channels;
            bus += 1;
        }
        total
    }

    /// The index of the first auxiliary port.
    const fn aux_index(&self) -> usize {
        self.inputs + self.sidechain + self.outputs
    }

    const fn control_index(&self) -> usize {
        self.aux_index() + self.aux_channels()
    }

    /// How many ports there are.
    pub const fn count(&self) -> usize {
        self.control_index() + 3 + self.params
    }

    /// The index of a port, or `None` if the plugin has no such port.
    pub const fn index(&self, role: PortRole) -> Option<usize> {
        let (index, exists) = match role {
            PortRole::AudioInput(channel) => (channel, channel < self.inputs),
            PortRole::Sidechain(channel) => (self.inputs + channel, channel < self.sidechain),
            PortRole::AudioOutput(channel) => (
                self.inputs + self.sidechain + channel,
                channel < self.outputs,
            ),
            PortRole::AuxOutput { bus, channel } => {
                let mut index = self.aux_index();
                let mut before = 0;
                while before < bus && before < self.aux.len() {
                    index += self.aux[before].channels;
                    before += 1;
                }
                (
                    index + channel,
                    bus < self.aux.len() && channel < self.aux[bus].channels,
                )
            }
            PortRole::Control => (self.control_index(), true),
            PortRole::Notify => (self.control_index() + 1, true),
            PortRole::Latency => (self.control_index() + 2, true),
            PortRole::Param(param) => (self.control_index() + 3 + param, param < self.params),
        };
        if exists { Some(index) } else { None }
    }

    /// What the port at `index` carries, or `None` past the last port.
    pub const fn role(&self, index: usize) -> Option<PortRole> {
        let control = self.control_index();
        Some(if index < self.inputs {
            PortRole::AudioInput(index)
        } else if index < self.inputs + self.sidechain {
            PortRole::Sidechain(index - self.inputs)
        } else if index < self.aux_index() {
            PortRole::AudioOutput(index - self.inputs - self.sidechain)
        } else if index < control {
            let mut channel = index - self.aux_index();
            let mut bus = 0;
            while channel >= self.aux[bus].channels {
                channel -= self.aux[bus].channels;
                bus += 1;
            }
            PortRole::AuxOutput { bus, channel }
        } else if index == control {
            PortRole::Control
        } else if index == control + 1 {
            PortRole::Notify
        } else if index == control + 2 {
            PortRole::Latency
        } else if index < self.count() {
            PortRole::Param(index - control - 3)
        } else {
            return None;
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_index_has_one_role_and_back() {
        for layout in [
            AudioLayout::STEREO_EFFECT,
            AudioLayout::STEREO_INSTRUMENT,
            AudioLayout {
                inputs: 1,
                outputs: 3,
                sidechain: 2,
            },
            AudioLayout {
                inputs: 0,
                outputs: 0,
                sidechain: 0,
            },
        ] {
            const BUSES: &[AuxOutput] = &[
                AuxOutput::stereo("a"),
                AuxOutput::new("b", 1),
                AuxOutput::new("c", 3),
            ];
            for (params, aux) in [(0, &[][..]), (1, BUSES), (5, &[][..]), (5, BUSES)] {
                let ports = PortLayout::new(layout, params).with_aux(aux);
                let aux_channels: usize = aux.iter().map(|bus| bus.channels).sum();
                assert_eq!(
                    ports.count(),
                    layout.inputs + layout.sidechain + layout.outputs + aux_channels + 3 + params
                );
                for index in 0..ports.count() {
                    let role = ports.role(index).unwrap();
                    assert_eq!(ports.index(role), Some(index), "{role:?}");
                }
                assert_eq!(ports.role(ports.count()), None);
                assert_eq!(ports.index(PortRole::Param(params)), None);
                assert_eq!(ports.index(PortRole::AudioInput(layout.inputs)), None);
                assert_eq!(ports.index(PortRole::Sidechain(layout.sidechain)), None);
                assert_eq!(ports.index(PortRole::AudioOutput(layout.outputs)), None);
                let past = PortRole::AuxOutput {
                    bus: aux.len(),
                    channel: 0,
                };
                assert_eq!(ports.index(past), None);
                if let Some(last) = aux.last() {
                    let wide = PortRole::AuxOutput {
                        bus: aux.len() - 1,
                        channel: last.channels,
                    };
                    assert_eq!(ports.index(wide), None);
                }
            }
        }
    }
}
