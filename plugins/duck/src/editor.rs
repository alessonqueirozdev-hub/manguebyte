//! The plugin's editor: a knob for each parameter, the bypass switch and a
//! meter for how far the input is being turned down.

use std::sync::Arc;

use manguebyte::AtomicF32;
use manguebyte::plugin::editor::HostSize;
use manguebyte_gui::Editor;
use manguebyte_ui::layout::Align;
use manguebyte_ui::{AtomicSource, IntoView, ParamHandle, column, knob, label, meter, row, toggle};

use crate::DuckParams;

/// The editor's size at scale 1, in logical pixels. At scale `s` the host
/// is asked for `s` times as many device pixels; the layout is the same at
/// every scale.
pub const EDITOR_SIZE: HostSize = HostSize::new(440, 210);

/// The most the meter shows, in decibels: the largest depth there is.
pub const METER_TOP_DB: f32 = 40.0;

/// The editor for `params`, with its meter reading `reduction`, the
/// reduction in decibels the processor stores once per block.
///
/// This is what [`Plugin::editor`](manguebyte::plugin::Plugin::editor)
/// returns, boxed. It is public so that a test can add a headless hook to
/// it with `manguebyte-gui`'s `headless` feature; see
/// [`Duck::editor_spec`](crate::Duck::editor_spec).
///
/// The view, built each time the host opens the editor:
///
/// - a title and the bypass switch, on one line;
/// - knobs for threshold, depth, attack and release, each showing its
///   value in the parameter's own words (`-30.0 dB`, `5.0 ms`);
/// - a meter named "Reduction", from 0 to [`METER_TOP_DB`] decibels, with
///   its value as text.
///
/// It is fixed in size; the host's scale (1 to 2 and beyond) multiplies
/// every length, so the same layout is drawn sharper.
///
/// ```
/// use std::sync::Arc;
/// use mangue_duck::{Duck, EDITOR_SIZE, editor};
/// use manguebyte::AtomicF32;
/// use manguebyte::plugin::Plugin;
/// use manguebyte::plugin::editor::{EditorSpec, HostSize, Resize};
///
/// let duck = Duck::new();
/// let spec = editor(duck.params(), Arc::new(AtomicF32::new(0.0)));
/// assert_eq!(spec.size(1.0), EDITOR_SIZE);
/// assert_eq!(spec.size(2.0), HostSize::new(880, 420));
/// assert_eq!(spec.resize(), Resize::Fixed);
/// ```
pub fn editor(params: Arc<DuckParams>, reduction: Arc<AtomicF32>) -> Editor {
    Editor::new(EDITOR_SIZE, move || {
        let param = |field: fn(&DuckParams) -> &manguebyte::params::FloatParam| {
            // The knobs share the width the meter leaves.
            knob(ParamHandle::new(Arc::clone(&params), field)).flex(1.0)
        };
        column((
            row((
                label("Mangue Duck").title().flex(1.0),
                toggle(ParamHandle::new(Arc::clone(&params), |p: &DuckParams| {
                    &p.bypass
                })),
            ))
            .align(Align::Center),
            row((
                param(|p| &p.threshold),
                param(|p| &p.depth),
                param(|p| &p.attack),
                param(|p| &p.release),
                meter("Reduction", AtomicSource::new(Arc::clone(&reduction)))
                    .range(0.0, METER_TOP_DB)
                    .unit("dB")
                    .precision(1),
            ))
            .spacing(12.0)
            .flex(1.0),
        ))
        .spacing(10.0)
        .padding(14.0)
    })
}
