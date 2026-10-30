use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};

use manguebyte_core::{AtomicF32, ParamId};

use crate::describe::decimal;
use crate::{Curve, FloatRange, ParamDescription, ParamScale, ParamSmoother, Smoothing};

/// What a host is allowed to do with a parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ParamFlags {
    /// The host may record and play back automation for it.
    pub automatable: bool,
    /// It is kept out of generic parameter lists.
    pub hidden: bool,
    /// It is the processor's bypass switch.
    pub bypass: bool,
    /// The processor can give it a value of its own for each sounding note,
    /// and an offset of its own for each note, so a host may automate and
    /// modulate it per note (CLAP's per-note-id and per-key automation and
    /// modulation flags). Such values arrive as
    /// `EventKind::NoteParamValue` and as `EventKind::ParamModulation` with
    /// a note, which a polyphonic `Synth` hands to the voice playing that
    /// note. Off unless asked for: a host that sees the flag offers
    /// per-note lanes, which a processor that ignores those events would
    /// not honour.
    pub per_note: bool,
}

impl Default for ParamFlags {
    fn default() -> Self {
        Self {
            automatable: true,
            hidden: false,
            bypass: false,
            per_note: false,
        }
    }
}

/// The names and unit a parameter shows now.
#[derive(Clone, PartialEq, Eq)]
struct Labels {
    name: Arc<str>,
    /// Empty when none was given: the name stands in for it.
    short_name: Arc<str>,
    unit: Arc<str>,
}

/// The facts about a parameter: its identity and flags, which never
/// change, and the names and unit it shows, which may.
///
/// # Names that change while the plugin runs
///
/// A plugin whose parameters stand for different things at different
/// times, such as an organ whose stop parameters drive whichever organ is
/// loaded, can give a parameter a new [`display_name`](Self::display_name),
/// [`short_name`](Self::short_name) and [`display_unit`](Self::display_unit)
/// at any time. The [`id`](Self::id) and [`param_id`](Self::param_id) never
/// change, so saved state and automation keep pointing at the same
/// parameter. [`name`](Self::name) and [`unit`](Self::unit) stay what the
/// parameter was made with.
///
/// The shown labels sit behind a lock, which the setters take to write and
/// the getters take to read. That is acceptable only because names are
/// text, and text belongs off the audio thread, as for every method that
/// returns or takes text: call none of the label methods on the audio
/// thread. What the audio thread may read is
/// [`labels_version`](Self::labels_version), an atomic that counts the
/// changes, which a wrapper polls to tell a host to read the names again
/// (ADR 0020).
///
/// ```
/// use manguebyte_params::{BoolParam, Param};
///
/// let stop = BoolParam::new("stop-01", "Stop 1", false);
/// let before = stop.info().labels_version();
///
/// // Another organ is loaded.
/// stop.info().set_display_name("Principal 8'");
/// stop.info().set_short_name("Prin 8");
/// assert_eq!(&*stop.display_name(), "Principal 8'");
/// assert_eq!(&*stop.info().short_name(), "Prin 8");
/// assert!(stop.info().labels_version() != before);
///
/// // What it was made with, and what it is known by, stay as they were.
/// assert_eq!(stop.name(), "Stop 1");
/// assert_eq!(stop.id(), "stop-01");
/// ```
pub struct ParamInfo {
    id: String,
    param_id: ParamId,
    name: String,
    unit: String,
    flags: ParamFlags,
    /// What is shown now. Taken off the audio thread only.
    shown: RwLock<Labels>,
    /// How many times `shown` has changed.
    version: AtomicU64,
}

impl ParamInfo {
    fn new(id: &str, name: &str) -> Self {
        assert!(
            is_valid_id(id),
            "parameter id {id:?} must be non-empty printable ASCII without spaces or '='"
        );
        Self {
            id: id.to_owned(),
            param_id: ParamId::from_name(id),
            name: name.to_owned(),
            unit: String::new(),
            flags: ParamFlags::default(),
            shown: RwLock::new(Labels {
                name: Arc::from(name),
                short_name: Arc::from(""),
                unit: Arc::from(""),
            }),
            version: AtomicU64::new(0),
        }
    }

    /// Sets the unit the parameter is made with, which it also shows.
    fn set_unit(&mut self, unit: &str) {
        self.unit = unit.to_owned();
        self.shown
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .unit = Arc::from(unit);
    }

    /// Sets the short name the parameter is made with.
    fn set_initial_short_name(&mut self, short_name: &str) {
        self.shown
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .short_name = Arc::from(short_name);
    }

    fn labels(&self) -> RwLockReadGuard<'_, Labels> {
        // A writer cannot panic while it holds the lock (it only swaps
        // `Arc`s), but if one did, the labels are still whole.
        self.shown.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Changes the shown labels with `change`, which says whether it
    /// changed anything, and counts the change if it did.
    fn relabel(&self, change: impl FnOnce(&mut Labels) -> bool) {
        let mut shown = self.shown.write().unwrap_or_else(PoisonError::into_inner);
        if change(&mut shown) {
            // Counted while the lock is held, so that whoever sees the new
            // count and then reads the labels reads the new ones.
            self.version.fetch_add(1, Ordering::Release);
        }
    }

    /// The identifier saved in state and sessions. It must never change once
    /// a version of the software has shipped.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The compact number that stands for [`id`](Self::id) in events and in
    /// plugin formats. It is a hash of the identifier, so it is just as
    /// stable.
    pub fn param_id(&self) -> ParamId {
        self.param_id
    }

    /// The name the parameter was made with. Free to change between
    /// versions. What people see is [`display_name`](Self::display_name),
    /// which is this name until it is changed.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The unit the parameter was made with, such as `"dB"`, or empty if
    /// there is none. What is shown after the value is
    /// [`display_unit`](Self::display_unit), which is this unit until it is
    /// changed.
    pub fn unit(&self) -> &str {
        &self.unit
    }

    /// What a host may do with the parameter.
    pub fn flags(&self) -> ParamFlags {
        self.flags
    }

    /// The name to show people now: the one the parameter was made with,
    /// or the last one given to [`set_display_name`](Self::set_display_name).
    /// Takes a read lock: not for the audio thread.
    pub fn display_name(&self) -> Arc<str> {
        Arc::clone(&self.labels().name)
    }

    /// The name for places with little room, as VST 3 hosts have: the last
    /// one given to [`set_short_name`](Self::set_short_name) or
    /// [`with_short_name`](FloatParam::with_short_name), or the
    /// [`display_name`](Self::display_name) if there is none. Takes a read
    /// lock: not for the audio thread.
    pub fn short_name(&self) -> Arc<str> {
        let labels = self.labels();
        let short = if labels.short_name.is_empty() {
            &labels.name
        } else {
            &labels.short_name
        };
        Arc::clone(short)
    }

    /// The unit shown after the value now: the one the parameter was made
    /// with, or the last one given to
    /// [`set_display_unit`](Self::set_display_unit). Takes a read lock: not
    /// for the audio thread.
    pub fn display_unit(&self) -> Arc<str> {
        Arc::clone(&self.labels().unit)
    }

    /// Shows the parameter as `name` from now on. Wrappers tell the host
    /// (ADR 0020). Any thread but the audio thread: it takes the lock, and
    /// allocates.
    pub fn set_display_name(&self, name: &str) {
        self.relabel(|labels| replace(&mut labels.name, name));
    }

    /// Gives the parameter `short_name` for places with little room, or,
    /// if it is empty, none, so that the display name is used. Any thread
    /// but the audio thread.
    pub fn set_short_name(&self, short_name: &str) {
        self.relabel(|labels| replace(&mut labels.short_name, short_name));
    }

    /// Shows `unit` after the parameter's value from now on, and reads it
    /// after typed values. Any thread but the audio thread.
    pub fn set_display_unit(&self, unit: &str) {
        self.relabel(|labels| replace(&mut labels.unit, unit));
    }

    /// How many times the shown name, short name or unit has changed: a
    /// number that grows by one with each change, and only then. Setting a
    /// label to what it already is is no change.
    ///
    /// Real-time safe: one atomic load, no lock. It is what a wrapper polls,
    /// usually through [`ParamSet::labels_version`](crate::ParamSet::labels_version).
    pub fn labels_version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }
}

/// Puts `text` in `slot` unless it is there already, and says whether it
/// was not.
fn replace(slot: &mut Arc<str>, text: &str) -> bool {
    if **slot == *text {
        return false;
    }
    *slot = Arc::from(text);
    true
}

impl Clone for ParamInfo {
    /// A parameter of its own with the same facts, showing what this one
    /// shows now.
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            param_id: self.param_id,
            name: self.name.clone(),
            unit: self.unit.clone(),
            flags: self.flags,
            shown: RwLock::new(self.labels().clone()),
            version: AtomicU64::new(self.labels_version()),
        }
    }
}

impl PartialEq for ParamInfo {
    /// The same facts, showing the same labels. How many times the labels
    /// changed to get there does not count.
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.param_id == other.param_id
            && self.name == other.name
            && self.unit == other.unit
            && self.flags == other.flags
            && (std::ptr::eq(self, other) || *self.labels() == *other.labels())
    }
}

impl Eq for ParamInfo {}

impl fmt::Debug for ParamInfo {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let labels = self.labels();
        formatter
            .debug_struct("ParamInfo")
            .field("id", &self.id)
            .field("param_id", &self.param_id)
            .field("name", &self.name)
            .field("unit", &self.unit)
            .field("flags", &self.flags)
            .field("display_name", &labels.name)
            .field("short_name", &labels.short_name)
            .field("display_unit", &labels.unit)
            .finish()
    }
}

/// Whether `id` can be used as a parameter identifier.
pub(crate) fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'=')
}

/// A value a host can read, automate, display and save, whatever its type.
///
/// Every method takes `&self` and is safe to call from any thread at any
/// time: values live in atomics, so there is nothing to lock and no way to
/// race. The methods that return or take text allocate and belong off the
/// audio thread; everything else is real-time safe.
///
/// Two scales are used throughout:
///
/// - the *plain* value is the real one, in the parameter's own unit;
/// - the *normalized* value is its position in `0..=1`, which is what
///   automation and generic controls work with.
pub trait Param: Send + Sync {
    /// The fixed facts about the parameter.
    fn info(&self) -> &ParamInfo;

    /// The current value, in the parameter's own unit.
    fn plain(&self) -> f64;

    /// Sets the value from its own unit. It is clamped to the range and
    /// snapped to the step; a value that is not a number is ignored.
    fn set_plain(&self, value: f64);

    /// The value the parameter starts at, in its own unit.
    fn default_plain(&self) -> f64;

    /// Converts a position in `0..=1` to a value, clamping and snapping.
    fn normalized_to_plain(&self, normalized: f64) -> f64;

    /// Converts a value to its position in `0..=1`.
    fn plain_to_normalized(&self, plain: f64) -> f64;

    /// How many steps separate the lowest and the highest value, or `None`
    /// for a continuous parameter. A switch has one step.
    fn step_count(&self) -> Option<u32>;

    /// Renders a value as text, without its unit. Allocates.
    fn plain_to_text(&self, plain: f64) -> String;

    /// Reads a value from text a person typed. Allocates.
    fn text_to_plain(&self, text: &str) -> Option<f64>;

    /// The value [`plain`](Self::plain) returns after
    /// [`set_plain`](Self::set_plain)`(value)`, worked out without storing
    /// anything; `None` if `set_plain` would ignore `value`, as it ignores a
    /// value that is not a number. Real-time safe.
    ///
    /// A wrapper uses it to record what a parameter holds once it has
    /// stored a host's value, without reading the parameter back: another
    /// thread, such as an editor's, may have stored a newer value in
    /// between, and that value must still be reported as new.
    ///
    /// Unless overridden it returns `value` itself, which is right for a
    /// parameter that holds exactly what it is given, and for any value
    /// [`normalized_to_plain`](Self::normalized_to_plain) returns if
    /// `set_plain` keeps those as they are. The parameter types of this
    /// crate override it to clamp, snap and round exactly as they store.
    ///
    /// ```
    /// use manguebyte_params::{BoolParam, FloatParam, FloatRange, IntParam, Param};
    ///
    /// let level = FloatParam::new("level", "Level", FloatRange::linear(0.0, 4.0), 1.0);
    /// // Clamped to the range, and held as an `f32`.
    /// assert_eq!(level.stored_plain(9.0), Some(4.0));
    /// assert_eq!(level.stored_plain(1.2), Some(f64::from(1.2_f32)));
    /// assert_eq!(level.stored_plain(f64::NAN), None);
    /// // Nothing was stored.
    /// assert_eq!(level.value(), 1.0);
    ///
    /// let voices = IntParam::new("voices", "Voices", 1, 16, 8);
    /// assert_eq!(voices.stored_plain(3.4), Some(3.0));
    /// let bypass = BoolParam::new("bypass", "Bypass", false);
    /// assert_eq!(bypass.stored_plain(0.7), Some(1.0));
    /// ```
    fn stored_plain(&self, value: f64) -> Option<f64> {
        (!value.is_nan()).then_some(value)
    }

    /// The identifier saved in state and sessions.
    fn id(&self) -> &str {
        self.info().id()
    }

    /// The compact number that stands for the identifier in events.
    fn param_id(&self) -> ParamId {
        self.info().param_id()
    }

    /// The name the parameter was made with. To show people, use
    /// [`display_name`](Self::display_name), which may have changed since.
    fn name(&self) -> &str {
        self.info().name()
    }

    /// The name to show people now; see [`ParamInfo::display_name`]. Takes
    /// a read lock: not for the audio thread.
    fn display_name(&self) -> Arc<str> {
        self.info().display_name()
    }

    /// The current position in `0..=1`.
    fn normalized(&self) -> f64 {
        self.plain_to_normalized(self.plain())
    }

    /// Sets the value from a position in `0..=1`.
    fn set_normalized(&self, normalized: f64) {
        if normalized.is_finite() {
            self.set_plain(self.normalized_to_plain(normalized));
        }
    }

    /// The position in `0..=1` the parameter starts at.
    fn default_normalized(&self) -> f64 {
        self.plain_to_normalized(self.default_plain())
    }

    /// Puts the parameter back to its default.
    fn reset(&self) {
        self.set_plain(self.default_plain());
    }

    /// The current value as text, followed by the unit if there is one.
    /// Allocates.
    fn display(&self) -> String {
        self.display_plain(self.plain())
    }

    /// `plain` as text, followed by the unit if there is one: what
    /// [`display`](Self::display) would show if the parameter held `plain`.
    /// Allocates.
    ///
    /// ```
    /// use manguebyte_params::{ChoiceParam, FloatParam, FloatRange, Param};
    ///
    /// let gain = FloatParam::new("gain", "Gain", FloatRange::linear(-60.0, 12.0), 0.0)
    ///     .with_unit("dB")
    ///     .with_precision(1);
    /// assert_eq!(gain.display_plain(-6.0), "-6.0 dB");
    /// assert_eq!(gain.parse_display("-6.0 dB"), Some(-6.0));
    ///
    /// let shape = ChoiceParam::new("shape", "Shape", &["Sine", "Saw"], 0);
    /// assert_eq!(shape.display_plain(1.0), "Saw");
    /// assert_eq!(shape.parse_display("saw"), Some(1.0));
    /// ```
    fn display_plain(&self, plain: f64) -> String {
        let text = self.plain_to_text(plain);
        match &*self.info().display_unit() {
            "" => text,
            unit => format!("{text} {unit}"),
        }
    }

    /// Reads a value from text a person typed, or from the text
    /// [`display_plain`](Self::display_plain) wrote. Allocates.
    ///
    /// It gives [`text_to_plain`](Self::text_to_plain) the text without its
    /// unit first, so that the text of a switch or a choice that has a unit
    /// reads back as itself, even when one option's name ends with the
    /// unit. In turn, and taking the first that reads:
    ///
    /// 1. if the text ends with a space and the unit, exactly as
    ///    [`display_plain`](Self::display_plain) writes it, the text without
    ///    them;
    /// 2. if the text, less trailing white space, ends with the unit in any
    ///    ASCII case, the text without it (`"Off°"`, `"-6 DB"`);
    /// 3. the text as it is.
    ///
    /// The result is clamped and snapped like the value
    /// [`text_to_plain`](Self::text_to_plain) returns.
    fn parse_display(&self, text: &str) -> Option<f64> {
        let unit = self.info().display_unit();
        let unit = &*unit;
        if !unit.is_empty() {
            let exact = text
                .strip_suffix(unit)
                .and_then(|rest| rest.strip_suffix(' '));
            if let Some(plain) = exact.and_then(|rest| self.text_to_plain(rest)) {
                return Some(plain);
            }
            let loose = text.trim_end();
            if let Some(cut) = loose.len().checked_sub(unit.len()) {
                if loose.is_char_boundary(cut) && loose[cut..].eq_ignore_ascii_case(unit) {
                    if let Some(plain) = self.text_to_plain(&loose[..cut]) {
                        return Some(plain);
                    }
                }
            }
        }
        self.text_to_plain(text)
    }

    /// Everything an editor needs to draw a control for the parameter, as
    /// data that [`ParamDescription::to_json`] writes as JSON. Allocates.
    ///
    /// Unless overridden it returns [`ParamDescription::derived`], which
    /// works from the other methods of this trait. The parameter types of
    /// this crate override it to give their exact scale, names and
    /// smoothing; a wrapper around one of them should forward it.
    ///
    /// ```
    /// use manguebyte_params::{ChoiceParam, Curve, Param, ParamScale};
    ///
    /// let shape = ChoiceParam::new("shape", "Shape", &["Sine", "Saw", "Square"], 1);
    /// let description = shape.describe();
    /// assert_eq!((description.min, description.max, description.default), (0.0, 2.0, 1.0));
    /// assert_eq!(
    ///     description.scale,
    ///     ParamScale::Stepped {
    ///         steps: 2,
    ///         step: 1.0,
    ///         names: vec!["Sine".into(), "Saw".into(), "Square".into()],
    ///         curve: Curve::Linear,
    ///         end: 2.0,
    ///     }
    /// );
    /// ```
    fn describe(&self) -> ParamDescription {
        ParamDescription::derived(self)
    }
}

type Formatter = Arc<dyn Fn(f32) -> String + Send + Sync>;
type Parser = Arc<dyn Fn(&str) -> Option<f32> + Send + Sync>;

/// Shared builder methods for every parameter type.
macro_rules! common_builders {
    () => {
        /// Sets the unit shown after the value, such as `"dB"`.
        #[must_use]
        pub fn with_unit(mut self, unit: &str) -> Self {
            self.info.set_unit(unit);
            self
        }

        /// Gives the parameter a short name, for hosts that show one where
        /// there is little room. Without one they are given the name. See
        /// [`ParamInfo::short_name`].
        #[must_use]
        pub fn with_short_name(mut self, short_name: &str) -> Self {
            self.info.set_initial_short_name(short_name);
            self
        }

        /// Keeps hosts from automating the parameter.
        #[must_use]
        pub fn non_automatable(mut self) -> Self {
            self.info.flags.automatable = false;
            self
        }

        /// Keeps the parameter out of generic parameter lists.
        #[must_use]
        pub fn hidden(mut self) -> Self {
            self.info.flags.hidden = true;
            self
        }

        /// Lets hosts automate and modulate the parameter for each note on
        /// its own. See [`ParamFlags::per_note`].
        #[must_use]
        pub fn per_note(mut self) -> Self {
            self.info.flags.per_note = true;
            self
        }
    };
}

/// A continuous parameter.
///
/// ```
/// use manguebyte_params::{FloatParam, FloatRange, Param};
///
/// let cutoff = FloatParam::new("cutoff", "Cutoff", FloatRange::logarithmic(20.0, 20_000.0), 1_000.0)
///     .with_unit("Hz")
///     .with_precision(0);
///
/// cutoff.set(440.0);
/// assert_eq!(cutoff.value(), 440.0);
/// assert_eq!(cutoff.display(), "440 Hz");
///
/// // What a person types into a text box.
/// cutoff.set_plain(cutoff.text_to_plain("2.5 khz").unwrap());
/// assert_eq!(cutoff.value(), 2_500.0);
/// ```
pub struct FloatParam {
    info: ParamInfo,
    range: FloatRange,
    default: f32,
    step: Option<f32>,
    precision: usize,
    value: AtomicF32,
    formatter: Option<Formatter>,
    parser: Option<Parser>,
    smoothing: Option<Smoothing>,
}

impl FloatParam {
    /// Creates a parameter. `default` is clamped to `range`.
    ///
    /// # Panics
    ///
    /// Panics if `id` is empty or contains spaces, `=` or anything other than
    /// printable ASCII.
    pub fn new(id: &str, name: &str, range: FloatRange, default: f32) -> Self {
        let default = range.clamp(default);
        Self {
            info: ParamInfo::new(id, name),
            range,
            default,
            step: None,
            precision: 2,
            value: AtomicF32::new(default),
            formatter: None,
            parser: None,
            smoothing: None,
        }
    }

    common_builders!();

    /// Restricts the value to multiples of `step` above the range's minimum.
    ///
    /// # Panics
    ///
    /// Panics unless `step` is positive and finite.
    #[must_use]
    pub fn with_step(mut self, step: f32) -> Self {
        assert!(
            step.is_finite() && step > 0.0,
            "a step must be positive, got {step}"
        );
        self.step = Some(step);
        self.default = self.snap(self.default);
        self.value = AtomicF32::new(self.default);
        self
    }

    /// Sets how many decimal places the default text conversion shows.
    #[must_use]
    pub fn with_precision(mut self, decimals: usize) -> Self {
        self.precision = decimals;
        self
    }

    /// Replaces how the value is turned into text.
    #[must_use]
    pub fn with_formatter(
        mut self,
        formatter: impl Fn(f32) -> String + Send + Sync + 'static,
    ) -> Self {
        self.formatter = Some(Arc::new(formatter));
        self
    }

    /// Replaces how typed text is turned into a value.
    #[must_use]
    pub fn with_parser(
        mut self,
        parser: impl Fn(&str) -> Option<f32> + Send + Sync + 'static,
    ) -> Self {
        self.parser = Some(Arc::new(parser));
        self
    }

    /// Declares how the processor smooths changes to this parameter.
    ///
    /// It is a hint for editors and hosts, reported by
    /// [`describe`](Param::describe) as `smoothing_ms`, and the style
    /// [`smoother`](Self::smoother) starts from. It changes nothing else:
    /// the parameter still stores each value as it is set.
    ///
    /// ```
    /// use manguebyte_params::{FloatParam, FloatRange, Smoothing};
    ///
    /// let gain = FloatParam::new("gain", "Gain", FloatRange::linear(0.0, 1.0), 1.0)
    ///     .with_smoothing(Smoothing::Linear(10.0));
    /// assert_eq!(gain.smoothing(), Some(Smoothing::Linear(10.0)));
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if the time is negative or not finite.
    #[must_use]
    pub fn with_smoothing(mut self, smoothing: Smoothing) -> Self {
        self.smoothing = Some(checked_smoothing(smoothing));
        self
    }

    /// The smoothing declared with [`with_smoothing`](Self::with_smoothing),
    /// if any.
    pub fn smoothing(&self) -> Option<Smoothing> {
        self.smoothing
    }

    /// A smoother in the declared style, or one that jumps straight to each
    /// new value if none was declared. Call
    /// [`prepare`](ParamSmoother::prepare) before using it.
    pub fn smoother(&self) -> ParamSmoother {
        ParamSmoother::new(self.smoothing.unwrap_or(Smoothing::None))
    }

    /// The current value. Real-time safe.
    #[inline]
    pub fn value(&self) -> f32 {
        self.value.load(Ordering::Relaxed)
    }

    /// Sets the value, clamped to the range and snapped to the step. A value
    /// that is not a number is ignored. Real-time safe.
    #[inline]
    pub fn set(&self, value: f32) {
        if !value.is_nan() {
            self.value.store(self.snap(value), Ordering::Relaxed);
        }
    }

    /// The value the parameter starts at.
    pub fn default_value(&self) -> f32 {
        self.default
    }

    /// The mapping between value and position.
    pub fn range(&self) -> &FloatRange {
        &self.range
    }

    fn snap(&self, value: f32) -> f32 {
        let value = self.range.clamp(value);
        match self.step {
            Some(step) => {
                let min = self.range.min();
                self.range
                    .clamp(min + ((value - min) / step).round() * step)
            }
            None => value,
        }
    }
}

impl Param for FloatParam {
    fn info(&self) -> &ParamInfo {
        &self.info
    }
    fn plain(&self) -> f64 {
        f64::from(self.value())
    }
    fn set_plain(&self, value: f64) {
        self.set(value as f32);
    }
    fn default_plain(&self) -> f64 {
        f64::from(self.default)
    }
    fn normalized_to_plain(&self, normalized: f64) -> f64 {
        f64::from(self.snap(self.range.denormalize(normalized)))
    }
    fn plain_to_normalized(&self, plain: f64) -> f64 {
        self.range.normalize(plain as f32)
    }
    fn step_count(&self) -> Option<u32> {
        let step = self.step?;
        Some(((self.range.max() - self.range.min()) / step).round() as u32)
    }
    fn plain_to_text(&self, plain: f64) -> String {
        match &self.formatter {
            Some(formatter) => formatter(plain as f32),
            None => {
                without_negative_zero(format!("{plain:.precision$}", precision = self.precision))
            }
        }
    }
    fn text_to_plain(&self, text: &str) -> Option<f64> {
        let value = match &self.parser {
            Some(parser) => parser(text)?,
            None => parse_number(text, &self.info.display_unit())? as f32,
        };
        (!value.is_nan()).then(|| f64::from(self.snap(value)))
    }
    fn stored_plain(&self, value: f64) -> Option<f64> {
        // What `set_plain` does: `set(value as f32)`, which snaps.
        let value = value as f32;
        (!value.is_nan()).then(|| f64::from(self.snap(value)))
    }
    fn describe(&self) -> ParamDescription {
        let curve = match self.range {
            FloatRange::Linear { .. } => Curve::Linear,
            FloatRange::Logarithmic { .. } => Curve::Log,
            FloatRange::Skewed { factor, .. } => Curve::Skew {
                factor: decimal(factor),
                symmetric: false,
            },
            FloatRange::SymmetricSkewed { factor, .. } => Curve::Skew {
                factor: decimal(factor),
                symmetric: true,
            },
        };
        let end = decimal(self.range.max());
        // The highest value the parameter can hold: the end of the range
        // snapped to the step, which is below the end when the step does not
        // divide the range.
        let max = decimal(self.snap(self.range.max()));
        let scale = match (self.step, curve) {
            (Some(step), _) => ParamScale::Stepped {
                steps: self.step_count().unwrap_or(0),
                step: decimal(step),
                names: Vec::new(),
                curve,
                end,
            },
            (None, Curve::Linear) => ParamScale::Linear,
            (None, Curve::Log) => ParamScale::Log,
            (None, Curve::Skew { factor, symmetric }) => ParamScale::Skew { factor, symmetric },
        };
        ParamDescription::with(
            &self.info,
            decimal(self.range.min()),
            max,
            decimal(self.default),
            scale,
            self.smoothing,
        )
    }
}

/// A whole-number parameter.
///
/// ```
/// use manguebyte_params::{IntParam, Param};
///
/// let voices = IntParam::new("voices", "Voices", 1, 16, 8);
/// voices.set(40);
/// assert_eq!(voices.value(), 16);
/// assert_eq!(voices.step_count(), Some(15));
/// ```
pub struct IntParam {
    info: ParamInfo,
    min: i32,
    max: i32,
    default: i32,
    value: AtomicI32,
    smoothing: Option<Smoothing>,
}

impl IntParam {
    /// Creates a parameter covering `min..=max`. `default` is clamped.
    ///
    /// # Panics
    ///
    /// Panics if `min >= max`, or if `id` is not a valid identifier (see
    /// [`FloatParam::new`]).
    pub fn new(id: &str, name: &str, min: i32, max: i32, default: i32) -> Self {
        assert!(min < max, "an integer range needs min < max");
        let default = default.clamp(min, max);
        Self {
            info: ParamInfo::new(id, name),
            min,
            max,
            default,
            value: AtomicI32::new(default),
            smoothing: None,
        }
    }

    common_builders!();

    /// Declares how the processor smooths changes to this parameter, as
    /// for [`FloatParam::with_smoothing`]: a transpose in semitones that
    /// glides from one pitch to the next, for example.
    ///
    /// It is a hint for editors and hosts, reported by
    /// [`describe`](Param::describe) as `smoothing_ms`. It changes nothing
    /// else: the parameter still holds whole numbers.
    ///
    /// ```
    /// use manguebyte_params::{IntParam, Param, Smoothing};
    ///
    /// let transpose = IntParam::new("transpose", "Transpose", -24, 24, 0)
    ///     .with_unit("st")
    ///     .with_smoothing(Smoothing::Exponential(50.0));
    /// assert_eq!(transpose.smoothing(), Some(Smoothing::Exponential(50.0)));
    /// assert_eq!(transpose.describe().smoothing_ms(), Some(50.0));
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if the time is negative or not finite.
    #[must_use]
    pub fn with_smoothing(mut self, smoothing: Smoothing) -> Self {
        self.smoothing = Some(checked_smoothing(smoothing));
        self
    }

    /// The smoothing declared with [`with_smoothing`](Self::with_smoothing),
    /// if any.
    pub fn smoothing(&self) -> Option<Smoothing> {
        self.smoothing
    }

    /// The current value. Real-time safe.
    #[inline]
    pub fn value(&self) -> i32 {
        self.value.load(Ordering::Relaxed)
    }

    /// Sets the value, clamped to the range. Real-time safe.
    #[inline]
    pub fn set(&self, value: i32) {
        self.value
            .store(value.clamp(self.min, self.max), Ordering::Relaxed);
    }

    /// The lowest value.
    pub fn min(&self) -> i32 {
        self.min
    }

    /// The highest value.
    pub fn max(&self) -> i32 {
        self.max
    }

    fn span(&self) -> f64 {
        f64::from(self.max) - f64::from(self.min)
    }

    fn snap(&self, plain: f64) -> f64 {
        plain
            .round()
            .clamp(f64::from(self.min), f64::from(self.max))
    }
}

impl Param for IntParam {
    fn info(&self) -> &ParamInfo {
        &self.info
    }
    fn plain(&self) -> f64 {
        f64::from(self.value())
    }
    fn set_plain(&self, value: f64) {
        if !value.is_nan() {
            self.set(self.snap(value) as i32);
        }
    }
    fn default_plain(&self) -> f64 {
        f64::from(self.default)
    }
    fn normalized_to_plain(&self, normalized: f64) -> f64 {
        self.snap(f64::from(self.min) + normalized.clamp(0.0, 1.0) * self.span())
    }
    fn plain_to_normalized(&self, plain: f64) -> f64 {
        (self.snap(plain) - f64::from(self.min)) / self.span()
    }
    fn step_count(&self) -> Option<u32> {
        Some(self.span() as u32)
    }
    fn plain_to_text(&self, plain: f64) -> String {
        format!("{}", self.snap(plain) as i64)
    }
    fn text_to_plain(&self, text: &str) -> Option<f64> {
        parse_number(text, &self.info.display_unit()).map(|value| self.snap(value))
    }
    fn stored_plain(&self, value: f64) -> Option<f64> {
        (!value.is_nan()).then(|| f64::from((self.snap(value) as i32).clamp(self.min, self.max)))
    }
    fn describe(&self) -> ParamDescription {
        let scale = ParamScale::Stepped {
            steps: self.span() as u32,
            step: 1.0,
            names: Vec::new(),
            curve: Curve::Linear,
            end: f64::from(self.max),
        };
        ParamDescription::with(
            &self.info,
            f64::from(self.min),
            f64::from(self.max),
            f64::from(self.default),
            scale,
            self.smoothing,
        )
    }
}

/// An on/off parameter.
///
/// ```
/// use manguebyte_params::{BoolParam, Param};
///
/// let bypass = BoolParam::new("bypass", "Bypass", false).as_bypass();
/// bypass.set_normalized(0.8);
/// assert!(bypass.value());
/// assert_eq!(bypass.display(), "On");
/// ```
pub struct BoolParam {
    info: ParamInfo,
    default: bool,
    value: AtomicBool,
    labels: [String; 2],
    smoothing: Option<Smoothing>,
}

impl BoolParam {
    /// Creates a switch.
    ///
    /// # Panics
    ///
    /// Panics if `id` is not a valid identifier (see [`FloatParam::new`]).
    pub fn new(id: &str, name: &str, default: bool) -> Self {
        Self {
            info: ParamInfo::new(id, name),
            default,
            value: AtomicBool::new(default),
            labels: [String::from("Off"), String::from("On")],
            smoothing: None,
        }
    }

    common_builders!();

    /// Declares how the processor smooths a change of state, such as a
    /// bypass that crossfades instead of cutting.
    ///
    /// It is a hint for editors and hosts, reported by
    /// [`describe`](Param::describe) as `smoothing_ms`. It changes nothing
    /// else: the parameter is still either off or on.
    ///
    /// ```
    /// use manguebyte_params::{BoolParam, Param, Smoothing};
    ///
    /// let bypass = BoolParam::new("bypass", "Bypass", false)
    ///     .as_bypass()
    ///     .with_smoothing(Smoothing::Linear(10.0));
    /// assert_eq!(bypass.smoothing(), Some(Smoothing::Linear(10.0)));
    /// assert_eq!(bypass.describe().smoothing_ms(), Some(10.0));
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if the time is negative or not finite.
    #[must_use]
    pub fn with_smoothing(mut self, smoothing: Smoothing) -> Self {
        self.smoothing = Some(checked_smoothing(smoothing));
        self
    }

    /// The smoothing declared with [`with_smoothing`](Self::with_smoothing),
    /// if any.
    pub fn smoothing(&self) -> Option<Smoothing> {
        self.smoothing
    }

    /// Marks this as the processor's bypass switch, which hosts treat
    /// specially.
    #[must_use]
    pub fn as_bypass(mut self) -> Self {
        self.info.flags.bypass = true;
        self
    }

    /// Replaces the text shown for the two states.
    #[must_use]
    pub fn with_labels(mut self, off: &str, on: &str) -> Self {
        self.labels = [off.to_owned(), on.to_owned()];
        self
    }

    /// The current state. Real-time safe.
    #[inline]
    pub fn value(&self) -> bool {
        self.value.load(Ordering::Relaxed)
    }

    /// Sets the state. Real-time safe.
    #[inline]
    pub fn set(&self, value: bool) {
        self.value.store(value, Ordering::Relaxed);
    }
}

impl Param for BoolParam {
    fn info(&self) -> &ParamInfo {
        &self.info
    }
    fn plain(&self) -> f64 {
        f64::from(u8::from(self.value()))
    }
    fn set_plain(&self, value: f64) {
        if !value.is_nan() {
            self.set(value >= 0.5);
        }
    }
    fn default_plain(&self) -> f64 {
        f64::from(u8::from(self.default))
    }
    fn normalized_to_plain(&self, normalized: f64) -> f64 {
        f64::from(u8::from(normalized >= 0.5))
    }
    fn plain_to_normalized(&self, plain: f64) -> f64 {
        f64::from(u8::from(plain >= 0.5))
    }
    fn step_count(&self) -> Option<u32> {
        Some(1)
    }
    fn plain_to_text(&self, plain: f64) -> String {
        self.labels[usize::from(plain >= 0.5)].clone()
    }
    /// Matches a label as a choice's names are matched (see
    /// [`ChoiceParam`]), then, if no label matches, `on`, `true`, `yes`,
    /// `1`, `off`, `false`, `no` or `0` in any ASCII case.
    fn text_to_plain(&self, text: &str) -> Option<f64> {
        if let Some(index) = matching_name(&self.labels, text) {
            return Some(index as f64);
        }
        match text.trim().to_ascii_lowercase().as_str() {
            "on" | "true" | "yes" | "1" => Some(1.0),
            "off" | "false" | "no" | "0" => Some(0.0),
            _ => None,
        }
    }
    fn stored_plain(&self, value: f64) -> Option<f64> {
        (!value.is_nan()).then(|| f64::from(u8::from(value >= 0.5)))
    }
    fn describe(&self) -> ParamDescription {
        let scale = ParamScale::Stepped {
            steps: 1,
            step: 1.0,
            names: self.labels.to_vec(),
            curve: Curve::Linear,
            end: 1.0,
        };
        ParamDescription::with(
            &self.info,
            0.0,
            1.0,
            self.default_plain(),
            scale,
            self.smoothing,
        )
    }
}

/// A parameter that picks one of a list of named options.
///
/// ```
/// use manguebyte_params::{ChoiceParam, Param};
///
/// let shape = ChoiceParam::new("shape", "Shape", &["Sine", "Saw", "Square"], 1);
/// assert_eq!(shape.display(), "Saw");
///
/// shape.set_plain(shape.text_to_plain("square").unwrap());
/// assert_eq!(shape.index(), 2);
/// ```
///
/// The saved state holds the index, so only ever add new options at the end
/// of the list; reordering or removing them changes what old sessions mean.
///
/// Text reads as the option whose name it is exactly, then as one whose
/// name it is once outer white space is removed from both, then ignoring
/// ASCII case as well. So every name reads back as its own option, even
/// `"saw"` next to `"Saw"`, and a person may type `" SAW"`. Two options
/// with exactly the same name cannot be told apart; the first one wins.
pub struct ChoiceParam {
    info: ParamInfo,
    choices: Vec<String>,
    default: u32,
    value: AtomicU32,
    smoothing: Option<Smoothing>,
}

impl ChoiceParam {
    /// Creates a parameter. `default` is the index of the starting option and
    /// is clamped to the list.
    ///
    /// # Panics
    ///
    /// Panics if fewer than two options are given, or if `id` is not a valid
    /// identifier (see [`FloatParam::new`]).
    pub fn new(id: &str, name: &str, choices: &[&str], default: usize) -> Self {
        assert!(choices.len() >= 2, "a choice needs at least two options");
        let default = default.min(choices.len() - 1) as u32;
        Self {
            info: ParamInfo::new(id, name),
            choices: choices.iter().map(|&choice| choice.to_owned()).collect(),
            default,
            value: AtomicU32::new(default),
            smoothing: None,
        }
    }

    common_builders!();

    /// Declares how the processor smooths a change of option, such as a
    /// crossfade from one waveform to the next.
    ///
    /// It is a hint for editors and hosts, reported by
    /// [`describe`](Param::describe) as `smoothing_ms`. It changes nothing
    /// else: the parameter still holds one index.
    ///
    /// ```
    /// use manguebyte_params::{ChoiceParam, Param, Smoothing};
    ///
    /// let shape = ChoiceParam::new("shape", "Shape", &["Sine", "Saw"], 0)
    ///     .with_smoothing(Smoothing::Linear(5.0));
    /// assert_eq!(shape.smoothing(), Some(Smoothing::Linear(5.0)));
    /// assert_eq!(shape.describe().smoothing_ms(), Some(5.0));
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if the time is negative or not finite.
    #[must_use]
    pub fn with_smoothing(mut self, smoothing: Smoothing) -> Self {
        self.smoothing = Some(checked_smoothing(smoothing));
        self
    }

    /// The smoothing declared with [`with_smoothing`](Self::with_smoothing),
    /// if any.
    pub fn smoothing(&self) -> Option<Smoothing> {
        self.smoothing
    }

    /// Index of the selected option. Real-time safe.
    #[inline]
    pub fn index(&self) -> usize {
        self.value.load(Ordering::Relaxed) as usize
    }

    /// Selects an option by index, clamped to the list. Real-time safe.
    #[inline]
    pub fn set_index(&self, index: usize) {
        self.value
            .store(index.min(self.choices.len() - 1) as u32, Ordering::Relaxed);
    }

    /// The names of the options.
    pub fn choices(&self) -> &[String] {
        &self.choices
    }

    fn last(&self) -> f64 {
        (self.choices.len() - 1) as f64
    }

    fn snap(&self, plain: f64) -> f64 {
        plain.round().clamp(0.0, self.last())
    }
}

impl Param for ChoiceParam {
    fn info(&self) -> &ParamInfo {
        &self.info
    }
    fn plain(&self) -> f64 {
        self.index() as f64
    }
    fn set_plain(&self, value: f64) {
        if !value.is_nan() {
            self.set_index(self.snap(value) as usize);
        }
    }
    fn default_plain(&self) -> f64 {
        f64::from(self.default)
    }
    fn normalized_to_plain(&self, normalized: f64) -> f64 {
        self.snap(normalized.clamp(0.0, 1.0) * self.last())
    }
    fn plain_to_normalized(&self, plain: f64) -> f64 {
        self.snap(plain) / self.last()
    }
    fn step_count(&self) -> Option<u32> {
        Some(self.choices.len() as u32 - 1)
    }
    fn plain_to_text(&self, plain: f64) -> String {
        self.choices[self.snap(plain) as usize].clone()
    }
    fn text_to_plain(&self, text: &str) -> Option<f64> {
        matching_name(&self.choices, text).map(|index| index as f64)
    }
    fn stored_plain(&self, value: f64) -> Option<f64> {
        (!value.is_nan()).then(|| (self.snap(value) as usize).min(self.choices.len() - 1) as f64)
    }
    fn describe(&self) -> ParamDescription {
        let scale = ParamScale::Stepped {
            steps: self.choices.len() as u32 - 1,
            step: 1.0,
            names: self.choices.clone(),
            curve: Curve::Linear,
            end: self.last(),
        };
        ParamDescription::with(
            &self.info,
            0.0,
            self.last(),
            self.default_plain(),
            scale,
            self.smoothing,
        )
    }
}

/// `smoothing`, after checking that its time is a real duration.
fn checked_smoothing(smoothing: Smoothing) -> Smoothing {
    if let Smoothing::Linear(milliseconds) | Smoothing::Exponential(milliseconds) = smoothing {
        assert!(
            milliseconds.is_finite() && milliseconds >= 0.0,
            "a smoothing time must be zero or more milliseconds, got {milliseconds}"
        );
    }
    smoothing
}

/// Drops the sign from a number that rounded to zero.
///
/// A small negative value formats as `-0.0`, which reads back as zero and
/// then formats as `0.0`: the same value would show two different texts.
fn without_negative_zero(text: String) -> String {
    match text.strip_prefix('-') {
        Some(digits) if digits.chars().all(|c| c == '0' || c == '.') => digits.to_owned(),
        _ => text,
    }
}

/// The index of the name `text` stands for, trying in turn, and taking the
/// first name that matches at the first stage that finds one:
///
/// 1. the name exactly as it is, so that every name reads back as itself
///    (names that differ only in case or in outer white space included);
/// 2. the name and the text with outer white space removed;
/// 3. the same, ignoring ASCII case.
///
/// Names that are exactly the same cannot be told apart: the first wins.
fn matching_name(names: &[String], text: &str) -> Option<usize> {
    let trimmed = text.trim();
    names
        .iter()
        .position(|name| name == text)
        .or_else(|| names.iter().position(|name| name.trim() == trimmed))
        .or_else(|| {
            names
                .iter()
                .position(|name| name.trim().eq_ignore_ascii_case(trimmed))
        })
}

/// Reads a number a person typed: an optional unit after it is ignored, a
/// decimal comma is accepted, and `k` or `m` scale by a thousand or a
/// thousandth (`"2.5k"`, `"2,5 kHz"`, `"20 ms"` when the unit is `"s"`).
fn parse_number(text: &str, unit: &str) -> Option<f64> {
    let text = text.trim();
    let split = text
        .char_indices()
        .find(|&(index, c)| {
            let sign = (c == '-' || c == '+') && index == 0;
            !(c.is_ascii_digit() || c == '.' || c == ',' || sign)
        })
        .map_or(text.len(), |(index, _)| index);
    let (number, suffix) = text.split_at(split);
    let value: f64 = number.replace(',', ".").parse().ok()?;
    if !value.is_finite() {
        return None;
    }

    let suffix = suffix.trim();
    if suffix.is_empty() || suffix.eq_ignore_ascii_case(unit) {
        return Some(value);
    }
    let mut chars = suffix.chars();
    let scale = match chars.next() {
        Some('k' | 'K') => 1e3,
        Some('m') => 1e-3,
        _ => return None,
    };
    let rest = chars.as_str().trim();
    (rest.is_empty() || rest.eq_ignore_ascii_case(unit)).then_some(value * scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gain() -> FloatParam {
        FloatParam::new("gain", "Gain", FloatRange::linear(-60.0, 12.0), 0.0)
            .with_unit("dB")
            .with_precision(1)
    }

    #[test]
    fn float_clamps_and_ignores_nan() {
        let gain = gain();
        gain.set(100.0);
        assert_eq!(gain.value(), 12.0);
        gain.set(f32::NEG_INFINITY);
        assert_eq!(gain.value(), -60.0);
        gain.set(-6.0);
        gain.set(f32::NAN);
        gain.set_plain(f64::NAN);
        gain.set_normalized(f64::NAN);
        assert_eq!(gain.value(), -6.0);
    }

    #[test]
    fn float_normalized_round_trip_and_reset() {
        let gain = gain();
        gain.set_normalized(0.5);
        assert_eq!(gain.value(), -24.0);
        assert!((gain.normalized() - 0.5).abs() < 1e-9);
        assert!((gain.default_normalized() - 60.0 / 72.0).abs() < 1e-9);
        gain.reset();
        assert_eq!(gain.value(), 0.0);
    }

    #[test]
    fn float_step_snaps_everywhere() {
        let param = FloatParam::new("x", "X", FloatRange::linear(0.0, 1.0), 0.33).with_step(0.25);
        assert_eq!(param.default_value(), 0.25);
        assert_eq!(param.value(), 0.25);
        assert_eq!(param.step_count(), Some(4));
        param.set(0.7);
        assert_eq!(param.value(), 0.75);
        assert_eq!(param.normalized_to_plain(0.9), 1.0);
        assert_eq!(param.text_to_plain("0.4"), Some(0.5));
        assert_eq!(gain().step_count(), None);
    }

    #[test]
    fn float_text_conversion() {
        let gain = gain();
        gain.set(-6.04);
        assert_eq!(gain.display(), "-6.0 dB");
        assert_eq!(gain.text_to_plain(" -12 dB "), Some(-12.0));
        assert_eq!(gain.text_to_plain("3,5db"), Some(3.5));
        assert_eq!(gain.text_to_plain("+3"), Some(3.0));
        assert_eq!(gain.text_to_plain("999"), Some(12.0));
        assert_eq!(gain.text_to_plain("loud"), None);
        assert_eq!(gain.text_to_plain("3 Hz"), None);
        assert_eq!(gain.text_to_plain(""), None);

        let time = FloatParam::new("t", "Time", FloatRange::linear(0.0, 10.0), 1.0).with_unit("s");
        assert!((time.text_to_plain("20 ms").unwrap() - 0.02).abs() < 1e-7);
        let cutoff = FloatParam::new(
            "f",
            "Cutoff",
            FloatRange::logarithmic(20.0, 20_000.0),
            1_000.0,
        )
        .with_unit("Hz");
        assert_eq!(cutoff.text_to_plain("2.5k"), Some(2_500.0));
        assert_eq!(cutoff.text_to_plain("2.5 kHz"), Some(2_500.0));
    }

    #[test]
    fn text_round_trips_to_the_same_text() {
        // Found by clap-validator: a value just below zero printed as
        // "-0.0", read back as zero, and then printed as "0.0".
        let gain = gain();
        assert_eq!(gain.plain_to_text(-0.04), "0.0");
        assert_eq!(gain.plain_to_text(-0.06), "-0.1");
        assert_eq!(gain.plain_to_text(0.0), "0.0");
        for step in 0..=720 {
            let plain = -60.0 + 0.1 * f64::from(step) + 0.037;
            let first = gain.plain_to_text(plain);
            let again = gain.plain_to_text(gain.text_to_plain(&first).unwrap());
            assert_eq!(first, again, "from {plain}");
        }
    }

    #[test]
    fn custom_formatter_and_parser() {
        let pan = FloatParam::new("pan", "Pan", FloatRange::linear(-1.0, 1.0), 0.0)
            .with_formatter(|value| match value {
                v if v < 0.0 => format!("{:.0}L", -v * 100.0),
                v if v > 0.0 => format!("{:.0}R", v * 100.0),
                _ => String::from("C"),
            })
            .with_parser(|text| match text.trim() {
                "C" | "c" => Some(0.0),
                _ => None,
            });
        assert_eq!(pan.display(), "C");
        pan.set(-0.5);
        assert_eq!(pan.display(), "50L");
        assert_eq!(pan.text_to_plain("c"), Some(0.0));
        assert_eq!(pan.text_to_plain("12"), None);
    }

    #[test]
    fn int_param() {
        let voices = IntParam::new("voices", "Voices", 1, 16, 8);
        assert_eq!(voices.normalized_to_plain(0.0), 1.0);
        assert_eq!(voices.normalized_to_plain(1.0), 16.0);
        voices.set_normalized(0.5);
        assert_eq!(voices.value(), 9);
        voices.set_plain(3.4);
        assert_eq!(voices.value(), 3);
        voices.set(-5);
        assert_eq!(voices.value(), 1);
        assert_eq!(voices.text_to_plain("12"), Some(12.0));
        assert_eq!(voices.plain_to_text(7.0), "7");
    }

    #[test]
    fn bool_param() {
        let bypass = BoolParam::new("bypass", "Bypass", false).as_bypass();
        assert!(bypass.info().flags().bypass);
        assert!(!bypass.value());
        bypass.set_normalized(1.0);
        assert!(bypass.value());
        assert_eq!(bypass.text_to_plain("OFF"), Some(0.0));
        assert_eq!(bypass.text_to_plain("yes"), Some(1.0));
        assert_eq!(bypass.text_to_plain("maybe"), None);

        let mode = BoolParam::new("mode", "Mode", true).with_labels("Mono", "Stereo");
        assert_eq!(mode.display(), "Stereo");
        assert_eq!(mode.text_to_plain("mono"), Some(0.0));
    }

    #[test]
    fn choice_param() {
        let shape = ChoiceParam::new("shape", "Shape", &["Sine", "Saw", "Square"], 9);
        assert_eq!(shape.index(), 2);
        assert_eq!(shape.step_count(), Some(2));
        shape.set_normalized(0.5);
        assert_eq!(shape.index(), 1);
        assert_eq!(shape.normalized(), 0.5);
        shape.set_index(99);
        assert_eq!(shape.index(), 2);
        assert_eq!(shape.text_to_plain("sine"), Some(0.0));
        assert_eq!(shape.text_to_plain("noise"), None);
    }

    /// For every parameter type, `stored_plain` gives exactly what the
    /// parameter holds after `set_plain`, bit for bit, or `None` exactly
    /// when `set_plain` leaves the value alone, for hostile values too.
    #[test]
    fn stored_plain_is_what_set_plain_stores() {
        let params: [Box<dyn Param>; 6] = [
            Box::new(gain()),
            Box::new(FloatParam::new("x", "X", FloatRange::linear(0.1, 1.0), 0.33).with_step(0.3)),
            Box::new(FloatParam::new(
                "f",
                "Cutoff",
                FloatRange::logarithmic(20.0, 20_000.0),
                1_000.0,
            )),
            Box::new(IntParam::new("voices", "Voices", -3, 16, 8)),
            Box::new(BoolParam::new("bypass", "Bypass", false)),
            Box::new(ChoiceParam::new("shape", "Shape", &["A", "B", "C"], 1)),
        ];
        let mut values = vec![
            f64::NAN,
            -f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MAX,
            f64::MIN,
            f64::MIN_POSITIVE,
            -0.0,
            0.0,
            0.5,
            0.49999999999999994,
            1.2,
            2.5,
            -2.5,
            1e40,
            -1e40,
            f64::from(i32::MAX) + 10.0,
        ];
        let mut random = 0x0DDB_A11C_AFE5_EED5_u64;
        for _ in 0..2_000 {
            random ^= random >> 12;
            random ^= random << 25;
            random ^= random >> 27;
            let bits = random.wrapping_mul(0x2545_F491_4F6C_DD1D);
            // Any bit pattern, and numbers around the ranges above.
            values.push(f64::from_bits(bits));
            values.push((bits % 50_000) as f64 / 100.0 - 100.0);
        }
        for param in &params {
            for &value in &values {
                param.reset();
                let before = param.plain().to_bits();
                let predicted = param.stored_plain(value);
                param.set_plain(value);
                let after = param.plain();
                match predicted {
                    Some(stored) => assert_eq!(
                        stored.to_bits(),
                        after.to_bits(),
                        "{}: {value:e} gave {after:e}, predicted {stored:e}",
                        param.id()
                    ),
                    None => assert_eq!(after.to_bits(), before, "{}: {value:e}", param.id()),
                }
            }
        }
    }

    /// Someone else's parameter that keeps the default `stored_plain`: it
    /// holds exactly what it is given, as the bits of an `f64`.
    struct Exact {
        info: ParamInfo,
        bits: std::sync::atomic::AtomicU64,
    }

    impl Param for Exact {
        fn info(&self) -> &ParamInfo {
            &self.info
        }
        fn plain(&self) -> f64 {
            f64::from_bits(self.bits.load(Ordering::Relaxed))
        }
        fn set_plain(&self, value: f64) {
            if !value.is_nan() {
                self.bits.store(value.to_bits(), Ordering::Relaxed);
            }
        }
        fn default_plain(&self) -> f64 {
            0.0
        }
        fn normalized_to_plain(&self, normalized: f64) -> f64 {
            normalized
        }
        fn plain_to_normalized(&self, plain: f64) -> f64 {
            plain
        }
        fn step_count(&self) -> Option<u32> {
            None
        }
        fn plain_to_text(&self, plain: f64) -> String {
            plain.to_string()
        }
        fn text_to_plain(&self, text: &str) -> Option<f64> {
            text.parse().ok()
        }
    }

    /// The default `stored_plain` is the value itself, or `None` for a
    /// value that is not a number, which is right for such a parameter.
    #[test]
    fn the_default_stored_plain_is_the_value_itself() {
        let param = Exact {
            info: ParamInfo::new("exact", "Exact"),
            bits: std::sync::atomic::AtomicU64::new(0),
        };
        for value in [0.25, -7.0, 1e300, f64::INFINITY, -0.0, f64::NAN] {
            let predicted = param.stored_plain(value);
            param.set_plain(value);
            match predicted {
                Some(stored) => assert_eq!(stored.to_bits(), param.plain().to_bits()),
                None => assert!(value.is_nan()),
            }
        }
        assert_eq!(param.stored_plain(0.25), Some(0.25));
        assert_eq!(param.stored_plain(f64::NAN), None);
    }

    #[test]
    #[should_panic(expected = "parameter id")]
    fn refuses_an_id_that_could_corrupt_saved_state() {
        let _ = BoolParam::new("a=b", "Broken", false);
    }

    #[test]
    fn a_new_name_and_unit_are_shown_and_read_while_the_old_ones_are_kept() {
        let gain = gain().with_short_name("Gn");
        assert_eq!(&*gain.info().short_name(), "Gn");
        assert_eq!(gain.info().labels_version(), 0);

        gain.info().set_display_name("Level");
        gain.info().set_display_unit("dBFS");
        gain.info().set_short_name("");
        assert_eq!(gain.info().labels_version(), 3);
        assert_eq!(&*gain.display_name(), "Level");
        // With no short name of its own, the display name stands in.
        assert_eq!(&*gain.info().short_name(), "Level");
        assert_eq!(gain.display_plain(-6.0), "-6.0 dBFS");
        assert_eq!(gain.parse_display("-6.0 dBFS"), Some(-6.0));
        assert_eq!(gain.text_to_plain("-3 dbfs"), Some(-3.0));
        let description = gain.describe();
        assert_eq!((&*description.name, &*description.unit), ("Level", "dBFS"));

        // What it was made with stays.
        assert_eq!((gain.name(), gain.info().unit()), ("Gain", "dB"));

        // The same label again is no change, and counts as none.
        gain.info().set_display_name("Level");
        gain.info().set_display_unit("dBFS");
        assert_eq!(gain.info().labels_version(), 3);
    }

    #[test]
    fn a_copy_shows_what_the_original_shows_and_then_goes_its_own_way() {
        let gain = gain();
        gain.info().set_display_name("Level");
        let copy = gain.info().clone();
        assert_eq!(copy, *gain.info());
        assert_eq!(&*copy.display_name(), "Level");
        copy.set_display_name("Volume");
        assert_ne!(copy, *gain.info());
        assert_eq!(&*gain.display_name(), "Level");
        assert!(format!("{copy:?}").contains("Volume"));
    }

    /// Names change on one thread while others read them: every read is a
    /// whole name that was set, never a torn one, and the count ends at
    /// the number of changes.
    #[test]
    fn renaming_from_another_thread_while_others_read() {
        let names = ["Principal 8'", "Bourdon 16'", "Octave 4'"];
        let stop = Arc::new(BoolParam::new("stop-01", "Stop 1", false));
        let renames = if cfg!(miri) { 20 } else { 2_000 };
        let writer = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                for turn in 0..renames {
                    stop.info().set_display_name(names[turn % names.len()]);
                }
            })
        };
        let readers: Vec<_> = (0..2)
            .map(|_| {
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    let mut last = 0;
                    for _ in 0..renames {
                        let version = stop.info().labels_version();
                        assert!(version >= last, "the count went back");
                        last = version;
                        let name = stop.display_name();
                        assert!(
                            &*name == "Stop 1" || names.contains(&&*name),
                            "read {name:?}"
                        );
                    }
                })
            })
            .collect();
        writer.join().unwrap();
        for reader in readers {
            reader.join().unwrap();
        }
        // Each turn names something other than the turn before.
        assert_eq!(stop.info().labels_version(), renames as u64);
        assert_eq!(&*stop.display_name(), names[(renames - 1) % names.len()]);
    }

    /// The count is what the audio thread polls, so it must never wait for
    /// the lock: read while a writer holds the lock, it still answers.
    #[test]
    fn the_count_does_not_wait_for_a_writer() {
        let stop = Arc::new(BoolParam::new("stop-01", "Stop 1", false));
        let held = stop.info.shown.write().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let reader = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || sender.send(stop.info().labels_version()).unwrap())
        };
        let answer = receiver.recv_timeout(std::time::Duration::from_secs(10));
        drop(held);
        reader.join().unwrap();
        assert_eq!(answer, Ok(0), "reading the count waited for the lock");
    }

    #[test]
    fn params_are_shareable_across_threads() {
        fn assert_shareable<T: Send + Sync>() {}
        assert_shareable::<FloatParam>();
        assert_shareable::<IntParam>();
        assert_shareable::<BoolParam>();
        assert_shareable::<ChoiceParam>();
    }
}
