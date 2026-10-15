//! The plugin's editor: a knob for each parameter, the bypass switch and a
//! meter for how far the input is being turned down.

use std::cell::Cell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use manguebyte::plugin::editor::HostSize;
use manguebyte_gui::Editor;
use manguebyte_ui::layout::Align;
use manguebyte_ui::{IntoView, ParamHandle, Source, column, knob, label, meter, row, toggle};

use crate::{DuckParams, ReductionMeter};

/// The editor's size at scale 1, in logical pixels. At scale `s` the host
/// is asked for `s` times as many device pixels; the layout is the same at
/// every scale.
pub const EDITOR_SIZE: HostSize = HostSize::new(440, 210);

/// The most the meter shows, in decibels: the largest depth there is.
pub const METER_TOP_DB: f32 = 40.0;

/// How long the meter keeps showing a reading the processor no longer
/// refreshes. After that it shows no reduction: a host that stops calling
/// the processor (deactivating it, or suspending a silent track) is not
/// turning anything down, and nothing else tells the editor so.
pub const METER_HOLD: Duration = Duration::from_millis(500);

/// What the meter shows: the processor's reading while it keeps
/// publishing, and 0 once it has published nothing new for
/// [`METER_HOLD`].
struct Reading {
    meter: Arc<ReductionMeter>,
    /// The publication count last seen, and when it was first seen.
    seen: Cell<Option<(u32, Instant)>>,
}

impl Source<f32> for Reading {
    fn read(&self) -> f32 {
        let blocks = self.meter.blocks();
        let now = Instant::now();
        match self.seen.get() {
            Some((seen, since)) if seen == blocks => {
                if now.saturating_duration_since(since) >= METER_HOLD {
                    0.0
                } else {
                    self.meter.reduction_db()
                }
            }
            _ => {
                self.seen.set(Some((blocks, now)));
                self.meter.reduction_db()
            }
        }
    }
}

/// The editor for `params`, with its meter reading `reduction`, which the
/// processor publishes into once per block.
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
///   its value as text. It drops to 0 when the processor has published
///   nothing for [`METER_HOLD`].
///
/// It is fixed in size; the host's scale (1 to 2 and beyond) multiplies
/// every length, so the same layout is drawn sharper.
///
/// ```
/// use std::sync::Arc;
/// use mangue_duck::{Duck, EDITOR_SIZE, ReductionMeter, editor};
/// use manguebyte::plugin::Plugin;
/// use manguebyte::plugin::editor::{EditorSpec, HostSize, Resize};
///
/// let duck = Duck::new();
/// let spec = editor(duck.params(), Arc::new(ReductionMeter::new()));
/// assert_eq!(spec.size(1.0), EDITOR_SIZE);
/// assert_eq!(spec.size(2.0), HostSize::new(880, 420));
/// assert_eq!(spec.resize(), Resize::Fixed);
/// ```
pub fn editor(params: Arc<DuckParams>, reduction: Arc<ReductionMeter>) -> Editor {
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
                meter(
                    "Reduction",
                    Reading {
                        meter: Arc::clone(&reduction),
                        seen: Cell::new(None),
                    },
                )
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
