//! A test plugin whose identity is data appended to its own library file.
//!
//! This is the situation of a plugin exporter that ships one pre-built
//! engine and makes each plugin a copy of it with the instrument's data
//! after the end of the file, as Parabolic does: every copy is the same
//! code, and each says who it is from what it finds in its file, through
//! [`library_path`] and [`Plugin::info`]. The workspace's test,
//! `tests/two_copies.rs`, copies this package's library twice, appends a
//! different identity to each copy, and hosts both in one process.
//!
//! The appended identity is, at the very end of the file:
//!
//! ```text
//! <id> LF <name> LF <vendor>   UTF-8, at most MAX_IDENTITY_BYTES bytes
//! <length of that text>        4 bytes, little-endian
//! "MBIDPROB"                   8 bytes, MAGIC
//! ```
//!
//! A copy without it, or with a damaged one, is the plugin [`Probe::INFO`]
//! names. The plugin itself is a gain with an editor that shows its name.
//! It is not an example to copy: it exists for the test.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use manguebyte::AudioBuffer;
use manguebyte::dsp::{ProcessContext, ProcessSpec};
use manguebyte::params::{FloatParam, FloatRange, param_set};
use manguebyte::plugin::editor::{EditorSpec, HostSize};
use manguebyte::plugin::{AudioLayout, Plugin, PluginInfo, PluginKind, library_path};
use manguebyte_gui::Editor;
use manguebyte_ui::{IntoView, ParamHandle, column, knob, label};

/// The last eight bytes of a copy that carries an identity.
pub const MAGIC: &[u8; 8] = b"MBIDPROB";

/// The longest identity text read.
pub const MAX_IDENTITY_BYTES: u32 = 4_096;

/// The editor's size at scale 1, in logical pixels.
pub const EDITOR_SIZE: HostSize = HostSize::new(240, 160);

/// The plugin's one parameter.
pub struct ProbeParams {
    /// What the input is multiplied by, from 0 to 1.
    pub gain: FloatParam,
}
param_set!(ProbeParams { gain });

/// The plugin: multiplies its input by its gain.
pub struct Probe {
    params: Arc<ProbeParams>,
}

/// The identity appended to the file at `path`, as id, name and vendor.
pub fn read_identity(path: &Path) -> Option<(String, String, String)> {
    let mut file = File::open(path).ok()?;
    let length = file.seek(SeekFrom::End(0)).ok()?;
    let mut tail = [0u8; 12];
    if length < tail.len() as u64 {
        return None;
    }
    file.seek(SeekFrom::End(-12)).ok()?;
    file.read_exact(&mut tail).ok()?;
    if &tail[4..] != MAGIC {
        return None;
    }
    let size = u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]);
    if size > MAX_IDENTITY_BYTES || u64::from(size) + 12 > length {
        return None;
    }
    file.seek(SeekFrom::End(-12 - i64::from(size))).ok()?;
    let mut text = vec![0u8; size as usize];
    file.read_exact(&mut text).ok()?;
    let text = String::from_utf8(text).ok()?;
    let mut lines = text.split('\n');
    let (Some(id), Some(name), Some(vendor), None) =
        (lines.next(), lines.next(), lines.next(), lines.next())
    else {
        return None;
    };
    Some((id.to_owned(), name.to_owned(), vendor.to_owned()))
}

impl Plugin for Probe {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.identity-probe",
        name: "Identity Probe",
        vendor: "Manguebyte tests",
        version: env!("CARGO_PKG_VERSION"),
        description: "Reads its identity from its own file",
        kind: PluginKind::Effect,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT;
    type Params = ProbeParams;

    fn info() -> PluginInfo {
        let Some((id, name, vendor)) = library_path().and_then(read_identity) else {
            return Self::INFO;
        };
        // Asked once per process, so leaking costs one copy.
        PluginInfo {
            id: id.leak(),
            name: name.leak(),
            vendor: vendor.leak(),
            ..Self::INFO
        }
    }

    fn new() -> Self {
        let gain = FloatParam::new("gain", "Gain", FloatRange::linear(0.0, 1.0), 1.0);
        Self {
            params: Arc::new(ProbeParams { gain }),
        }
    }

    fn params(&self) -> Arc<ProbeParams> {
        Arc::clone(&self.params)
    }

    fn prepare(&mut self, _: &ProcessSpec) {}

    fn process(&mut self, buffer: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {
        buffer.apply_gain(self.params.gain.value());
    }

    fn editor(&self) -> Option<Box<dyn EditorSpec>> {
        let params = Arc::clone(&self.params);
        let name = manguebyte::plugin::plugin_info::<Self>().name;
        Some(Box::new(Editor::new(EDITOR_SIZE, move || {
            column((
                label(name).title(),
                knob(ParamHandle::new(Arc::clone(&params), |p: &ProbeParams| {
                    &p.gain
                }))
                .flex(1.0),
            ))
            .spacing(8.0)
            .padding(12.0)
        })))
    }
}

manguebyte::clap::export_clap!(Probe);
manguebyte::vst3::export_vst3!(Probe);
