//! Views of an oscillator's waveform: one cycle, and a wavetable's frames
//! stacked in depth.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use manguebyte_dsp::Wavetable;
use manguebyte_gfx::{
    Extend, Image, LinearGradient, Paint, Path, Pixmap, Point, Rect, Renderer, Sampling, Scene,
    Size, Stop, Stroke,
};

use super::{MARGIN, device_px, hline, vline};
use crate::access::{AccessNode, Role};
use crate::motion::Lerp;
use crate::source::{Source, TextSource};
use crate::theme::{Palette, Theme};
use crate::widget::{AccessCx, MeasureCx, PaintCx, UpdateCx, Widget};

/// The most frames a wavetable view draws lines for; frames between them
/// are left out, the frame playing never.
pub const MAX_STACKED_FRAMES: usize = 32;

/// The most points a frame's line has, however long the frame: more than
/// a view is wide in device pixels gains nothing.
const MAX_POINTS: usize = 512;

/// How soft the glow of the frame playing is: tight, as a knob's arc's,
/// so that it stays within the view.
const LINE_GLOW: f32 = 4.0;

/// The frames a [`WaveData`] holds: `count` frames of `len` samples each,
/// one after the other.
struct Frames {
    samples: Vec<f32>,
    len: usize,
    count: usize,
    /// What the samples are multiplied by to be drawn: one over their
    /// highest magnitude if that is above 1, else 1.
    gain: f32,
}

impl Frames {
    fn new(samples: Vec<f32>, len: usize) -> Frames {
        let len = len.max(1);
        let count = samples.len() / len;
        let mut samples = samples;
        samples.truncate(count * len);
        for sample in &mut samples {
            if !sample.is_finite() {
                *sample = 0.0;
            }
        }
        let peak = samples.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
        Frames {
            samples,
            len,
            count,
            gain: if peak > 1.0 { 1.0 / peak } else { 1.0 },
        }
    }

    fn empty() -> Frames {
        Frames {
            samples: Vec::new(),
            len: 1,
            count: 0,
            gain: 1.0,
        }
    }

    /// The samples of frame `index`.
    fn frame(&self, index: usize) -> &[f32] {
        let index = index.min(self.count.saturating_sub(1));
        self.samples
            .get(index * self.len..(index + 1) * self.len)
            .unwrap_or(&[])
    }

    /// The value at `phase` (0 to 1, wrapped) of the frame at `position`
    /// (in frames, between them blended), scaled to be drawn.
    fn value(&self, position: f32, phase: f32) -> f32 {
        if self.count == 0 {
            return 0.0;
        }
        let position = position.clamp(0.0, (self.count - 1) as f32);
        let below = position.floor() as usize;
        let t = position - below as f32;
        let here = read(self.frame(below), phase);
        let value = if t > 0.0 {
            here.lerp(read(self.frame(below + 1), phase), t)
        } else {
            here
        };
        value * self.gain
    }
}

/// `frame` at `phase`, 0 to 1 and wrapped, between samples linearly.
fn read(frame: &[f32], phase: f32) -> f32 {
    let len = frame.len();
    if len == 0 {
        return 0.0;
    }
    let at = phase.rem_euclid(1.0) * len as f32;
    let index = (at.floor() as usize).min(len - 1);
    let t = at - index as f32;
    let next = frame[(index + 1) % len];
    frame[index].lerp(next, t)
}

struct WaveShared {
    frames: Mutex<Arc<Frames>>,
    version: AtomicU64,
}

/// The waveforms a [`wave_view`] or a [`wavetable_view`] shows: one cycle
/// or a wavetable's frames, as the application hands them over.
///
/// The application sets them when a sound is loaded or edited, never
/// from the audio thread; a view reads its version, an atomic, once a
/// frame, and copies nothing until it changes. Cloning shares the data.
/// It is `Send` and `Sync`, so an editor spec can keep it and a loading
/// thread can set it.
///
/// ```
/// use manguebyte_dsp::Wavetable;
/// use manguebyte_ui::WaveData;
///
/// let saw: Vec<f32> = (0..64).map(|i| i as f32 / 32.0 - 1.0).collect();
/// let data = WaveData::from_cycle(&saw);
/// assert_eq!((data.frame_count(), data.frame_len()), (1, 64));
///
/// let square: Vec<f32> = (0..64).map(|i| if i < 32 { 1.0 } else { -1.0 }).collect();
/// let table = Wavetable::new(&[saw, square]).expect("a table");
/// let version = data.version();
/// data.set_wavetable(&table);
/// assert_ne!(data.version(), version);
/// assert_eq!(data.frame_count(), 2);
/// ```
#[derive(Clone)]
pub struct WaveData {
    shared: Arc<WaveShared>,
}

impl Default for WaveData {
    fn default() -> Self {
        WaveData::new()
    }
}

impl WaveData {
    /// No waveform.
    pub fn new() -> WaveData {
        WaveData {
            shared: Arc::new(WaveShared {
                frames: Mutex::new(Arc::new(Frames::empty())),
                version: AtomicU64::new(0),
            }),
        }
    }

    /// One cycle, from `samples`.
    pub fn from_cycle(samples: &[f32]) -> WaveData {
        let data = WaveData::new();
        data.set_cycle(samples);
        data
    }

    /// Frames of equal length, each one cycle (see
    /// [`set_frames`](WaveData::set_frames)).
    pub fn from_frames<F: AsRef<[f32]>>(frames: &[F]) -> WaveData {
        let data = WaveData::new();
        data.set_frames(frames);
        data
    }

    /// The frames of `table` (see
    /// [`set_wavetable`](WaveData::set_wavetable)).
    pub fn from_wavetable(table: &Wavetable) -> WaveData {
        let data = WaveData::new();
        data.set_wavetable(table);
        data
    }

    fn lock(&self) -> MutexGuard<'_, Arc<Frames>> {
        // Nothing that can panic runs while the lock is held.
        self.shared
            .frames
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn store(&self, frames: Frames) {
        let old = std::mem::replace(&mut *self.lock(), Arc::new(frames));
        self.shared.version.fetch_add(1, Ordering::AcqRel);
        drop(old);
    }

    fn load(&self) -> Arc<Frames> {
        Arc::clone(&self.lock())
    }

    /// Shows one cycle, from `samples`.
    pub fn set_cycle(&self, samples: &[f32]) {
        self.store(Frames::new(samples.to_vec(), samples.len()));
    }

    /// Shows `frames`, each one cycle. Frames are cut to the length of the
    /// shortest; samples that are not finite count as 0, and a set whose
    /// highest magnitude is above 1 is drawn scaled down to it.
    pub fn set_frames<F: AsRef<[f32]>>(&self, frames: &[F]) {
        let len = frames
            .iter()
            .map(|frame| frame.as_ref().len())
            .min()
            .unwrap_or(0);
        let mut samples = Vec::with_capacity(len * frames.len());
        for frame in frames {
            samples.extend_from_slice(&frame.as_ref()[..len]);
        }
        self.store(Frames::new(samples, len));
    }

    /// Shows the frames of `table`, read from its richest level at as many
    /// points as its frames had (at most 2 048), as the oscillator plays
    /// them at a low pitch.
    pub fn set_wavetable(&self, table: &Wavetable) {
        let len = table.frame_len().clamp(1, 2_048);
        let count = table.num_frames();
        let mut samples = Vec::with_capacity(len * count);
        for frame in 0..count {
            for point in 0..len {
                let phase = point as f64 / len as f64;
                samples.push(table.read(0, frame as f64, phase) as f32);
            }
        }
        self.store(Frames::new(samples, len));
    }

    /// A number that changes every time the waveforms are set.
    pub fn version(&self) -> u64 {
        self.shared.version.load(Ordering::Acquire)
    }

    /// How many frames there are.
    pub fn frame_count(&self) -> usize {
        self.lock().count
    }

    /// How many samples a frame has.
    pub fn frame_len(&self) -> usize {
        let frames = self.lock();
        if frames.count == 0 { 0 } else { frames.len }
    }
}

impl fmt::Debug for WaveData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let frames = self.load();
        f.debug_struct("WaveData")
            .field("frames", &frames.count)
            .field("len", &frames.len)
            .field("version", &self.version())
            .finish()
    }
}

/// What a view polls: the data's version and a position, and what it last
/// read of them.
struct Polled {
    data: WaveData,
    version: Option<u64>,
    position: Option<Box<dyn Source<f32>>>,
    /// The position last read, as clamped and cleaned.
    at: f32,
}

impl Polled {
    fn new(data: WaveData) -> Polled {
        Polled {
            data,
            version: None,
            position: None,
            at: 0.0,
        }
    }

    /// Reads the version and the position, and returns whether the data
    /// changed and whether the position did.
    fn poll(&mut self) -> (bool, bool) {
        let version = self.data.version();
        let data = self.version != Some(version);
        self.version = Some(version);
        let at = self.position.as_ref().map_or(0.0, |source| {
            let at = source.read();
            if at.is_finite() { at.max(0.0) } else { 0.0 }
        });
        let moved = at.to_bits() != self.at.to_bits();
        self.at = at;
        (data, moved)
    }

    /// The position within `frames`.
    fn position(&self, frames: &Frames) -> f32 {
        self.at.min(frames.count.saturating_sub(1) as f32)
    }

    /// What assistive technology hears of the frames and the position.
    fn describe(&self) -> String {
        let frames = self.data.load();
        match frames.count {
            0 => "no waveform".to_owned(),
            1 => format!("one cycle of {} samples", frames.len),
            count => format!(
                "frame {:.1} of {count}, {} samples each",
                self.position(&frames) + 1.0,
                frames.len
            ),
        }
    }
}

/// How many points a line across `width` logical pixels at `scale` takes
/// for frames of `len` samples.
fn points(width: f32, scale: f32, len: usize) -> usize {
    let columns = (width * scale).ceil().max(2.0) as usize;
    columns.min(len.max(64)).min(MAX_POINTS)
}

/// A frame traced across a strip: its line, and for each device pixel
/// column of the strip the lowest and highest point of it there, from
/// which the fill under the line and its glow are drawn as columns
/// ([`Scene::columns`]).
///
/// Columns cost what the area they cover costs. A filled outline of the
/// same line, or a blurred shadow of it, measured three to five times
/// what the line itself costs, every time the line moves.
struct Traced {
    /// The strip grown by the glow's reach above and below: what the
    /// columns are fractions of.
    outer: Rect,
    line: Path,
    fill_low: Vec<f32>,
    fill_high: Vec<f32>,
    /// The glow, wide and narrow.
    glow: [(Vec<f32>, Vec<f32>); 2],
}

/// How far, in logical pixels, the two layers of a line's glow reach
/// above and below it, and how strong each is.
const GLOW_LAYERS: [(f32, f32); 2] = [(LINE_GLOW, 0.14), (LINE_GLOW * 0.5, 0.22)];

impl Traced {
    fn new() -> Traced {
        Traced {
            outer: Rect::new(0.0, 0.0, 0.0, 0.0),
            line: Path::new(),
            fill_low: Vec::new(),
            fill_high: Vec::new(),
            glow: [(Vec::new(), Vec::new()), (Vec::new(), Vec::new())],
        }
    }

    /// Traces the frame at `position` of `frames` across `strip`, a whole
    /// cycle from its left edge to its right, its peaks at its top and
    /// bottom, at `scale`; the fill reaches from the line to `base`, a
    /// value (0 for the middle, -1 for the bottom).
    fn trace(&mut self, frames: &Frames, position: f32, (strip, scale): (Rect, f32), base: f32) {
        self.line.clear();
        for list in [&mut self.fill_low, &mut self.fill_high] {
            list.clear();
        }
        for (low, high) in &mut self.glow {
            low.clear();
            high.clear();
        }
        let middle = strip.y + strip.height * 0.5;
        let half = strip.height * 0.5;
        let count = points(strip.width, scale, frames.len).max(2);
        for point in 0..count {
            let phase = point as f32 / (count - 1) as f32;
            // The last point is the first again, a cycle later.
            let value = if point == count - 1 {
                frames.value(position, 0.0)
            } else {
                frames.value(position, phase)
            };
            let at = Point::new(
                strip.x + phase * strip.width,
                middle - value.clamp(-1.0, 1.0) * half,
            );
            if point == 0 {
                self.line.move_to(at);
            } else {
                self.line.line_to(at);
            }
        }
        let reach = LINE_GLOW;
        self.outer = Rect::new(
            strip.x,
            strip.y - reach,
            strip.width,
            strip.height + 2.0 * reach,
        );
        let height = self.outer.height.max(f32::MIN_POSITIVE);
        // A value as a fraction of the outer strip from its bottom.
        let fraction = |value: f32| (reach + (value.clamp(-1.0, 1.0) + 1.0) * half) / height;
        let base = fraction(base);
        let columns = (strip.width * scale).round().max(1.0) as usize;
        let len = frames.len.max(1);
        for column in 0..columns {
            let from = column as f32 / columns as f32;
            let to = (column + 1) as f32 / columns as f32;
            let a = frames.value(position, from);
            let b = frames.value(position, if column + 1 == columns { 0.0 } else { to });
            let (mut low, mut high) = (a.min(b), a.max(b));
            // The samples between the column's edges.
            let first = (from * len as f32).floor() as usize + 1;
            let last = (to * len as f32).ceil() as usize;
            for sample in first..last.min(len) {
                let value = frames.value(position, sample as f32 / len as f32);
                low = low.min(value);
                high = high.max(value);
            }
            let (low, high) = (fraction(low), fraction(high));
            self.fill_low.push(low.min(base));
            self.fill_high.push(high.max(base));
            for ((glow_low, glow_high), (spread, _)) in self.glow.iter_mut().zip(GLOW_LAYERS) {
                glow_low.push(low - spread / height);
                glow_high.push(high + spread / height);
            }
        }
    }

    /// Draws the fill under the line in `fill`, then, in a theme that
    /// glows, the glow, then the line itself in the accent's bright end,
    /// two pixels wide.
    fn draw<'p>(&self, scene: &mut Scene, theme: &Theme, fill: impl Into<Paint<'p>>) {
        scene.columns(self.outer, &self.fill_low, &self.fill_high, fill);
        let glow = theme.elevation.glow;
        if glow.color.a > 0 {
            for ((low, high), (_, strength)) in self.glow.iter().zip(GLOW_LAYERS) {
                let color = theme.palette.accent.with_opacity(strength);
                scene.columns(self.outer, low, high, color);
            }
        }
        scene.stroke(&self.line, Stroke::round(2.0), theme.palette.accent_bright);
    }
}

/// The room a view keeps around what it draws for the glow of its line,
/// in a theme that glows.
fn room(theme: &Theme) -> f32 {
    if theme.elevation.glow.color.a == 0 {
        MARGIN
    } else {
        MARGIN + LINE_GLOW
    }
}

/// What a view drew last, kept until what it was drawn from changes.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Key {
    version: u64,
    rect: Rect,
    scale: f32,
}

/// One cycle of a waveform: an oscillator's shape, from a [`WaveData`],
/// drawn as a glowing line in the accent over a faint fill, on a centre
/// line and quarter marks. With several frames it shows the one at its
/// position, blended between frames, as a wavetable oscillator plays it.
///
/// It draws again only when the data's version or the position it polls
/// changes, and keeps what it traced between, so a steady view costs a
/// version read and a position read a frame. To assistive technology it
/// is an [`Image`](Role::Image) whose value says what it shows.
pub struct WaveView {
    name: TextSource,
    polled: Polled,
    traced: Option<(Key, f32)>,
    wave: Traced,
}

/// A view called `name` of one cycle of `data`: its first frame, or with
/// [`WaveView::position`] the frame at a polled position.
///
/// ```
/// use manguebyte_gfx::Size;
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{Role, WaveData, wave_view};
///
/// let sine: Vec<f32> = (0..256).map(|i| (i as f32 / 256.0 * std::f32::consts::TAU).sin()).collect();
/// let data = WaveData::from_cycle(&sine);
/// let mut ui = Harness::new(wave_view("Oscillator", data.clone()), Size::new(160.0, 90.0), 1.0);
/// ui.frame();
/// assert_eq!(ui.get(Role::Image, "Oscillator").value_text(), Some("one cycle of 256 samples"));
/// assert_eq!(ui.frame().painted, 0, "nothing changed");
/// data.set_cycle(&sine[..128]);
/// assert_eq!(ui.frame().painted, 1);
/// ```
pub fn wave_view(name: impl Into<TextSource>, data: WaveData) -> WaveView {
    WaveView {
        name: name.into(),
        polled: Polled::new(data),
        traced: None,
        wave: Traced::new(),
    }
}

impl WaveView {
    /// The same view showing the frame at `position`, in frames (1.5 is
    /// half way from the second to the third), read once a frame: from
    /// the audio thread through an [`AtomicSource`](crate::AtomicSource),
    /// say.
    #[must_use]
    pub fn position(mut self, position: impl Source<f32>) -> WaveView {
        self.polled.position = Some(Box::new(position));
        self
    }

    /// The data it shows.
    pub fn data(&self) -> &WaveData {
        &self.polled.data
    }
}

impl Widget for WaveView {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Image, self.name.as_str()).with_value(self.polled.describe())
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        Size::new(160.0, 96.0)
    }

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        super::poll_name(&mut self.name, cx);
        let (data, moved) = self.polled.poll();
        if data || moved {
            cx.request_paint();
        }
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let theme = cx.theme();
        let palette = &theme.palette;
        let (rect, scale) = (cx.rect(), cx.scale());
        let pad = room(theme);
        let area = Rect::new(
            rect.x + pad,
            rect.y + pad,
            (rect.width - 2.0 * pad).max(0.0),
            (rect.height - 2.0 * pad).max(0.0),
        );
        let middle = area.y + area.height * 0.5;
        // The centre line and quarter marks.
        hline(
            scene,
            middle,
            (area.x, area.right()),
            1.0,
            scale,
            palette.hairline_strong,
        );
        for quarter in 1..4 {
            let x = area.x + area.width * quarter as f32 / 4.0;
            vline(
                scene,
                x,
                (area.y, area.bottom()),
                1.0,
                scale,
                palette.hairline,
            );
        }
        let frames = self.polled.data.load();
        if frames.count == 0 || area.is_empty() {
            return;
        }
        let position = self.polled.position(&frames);
        let key = Key {
            version: self.polled.version.unwrap_or(0),
            rect: area,
            scale,
        };
        if self.traced != Some((key, position)) {
            self.traced = Some((key, position));
            // The peaks a device pixel inside the area, so that the line
            // is whole at the top and bottom.
            let inset = device_px(1.0, scale);
            let strip = Rect::new(
                area.x,
                area.y + inset,
                area.width,
                (area.height - 2.0 * inset).max(0.0),
            );
            self.wave.trace(&frames, position, (strip, scale), 0.0);
        }
        self.wave
            .draw(scene, theme, palette.accent.with_opacity(0.30));
    }
}

impl fmt::Debug for WaveView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WaveView")
            .field("name", &self.name.as_str())
            .field("data", &self.polled.data)
            .finish_non_exhaustive()
    }
}

/// What the stack's picture was drawn from.
#[derive(Debug, Clone, Copy, PartialEq)]
struct StackKey {
    key: Key,
    rect: Rect,
    palette: Palette,
}

/// A wavetable's frames stacked in depth, as a modern synth draws them:
/// the first frame in front, at the bottom left, the last at the back, up
/// and to the right and a little narrower, each hiding what is behind it,
/// on an axis that runs through them. The frame at the position it polls
/// (fractional, blended between frames) is drawn over the stack at its
/// place in depth, in the accent, with a glowing line and a fill fading
/// down from it. At most [`MAX_STACKED_FRAMES`] frames are stacked,
/// evenly chosen.
///
/// The stack is drawn into a picture of the view's size in device pixels
/// once, and again only when the data, the size, the scale or the theme
/// change; a change of the position draws the picture, copied pixel for
/// pixel, and the one frame over it. A steady view costs a version read
/// and a position read a frame. To assistive technology it is an
/// [`Image`](Role::Image) whose value says which frame plays.
pub struct WavetableView {
    name: TextSource,
    polled: Polled,
    stack: Option<(StackKey, Image)>,
    renderer: Option<Box<Renderer>>,
    traced: Option<(Key, f32)>,
    current: Traced,
}

/// A view called `name` of the frames of `data`, the frame at `position`
/// (in frames, read once a frame) lit.
///
/// ```
/// use std::sync::Arc;
/// use std::sync::atomic::Ordering;
/// use manguebyte_core::AtomicF32;
/// use manguebyte_gfx::Size;
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{AtomicSource, Role, WaveData, wavetable_view};
///
/// let frames: Vec<Vec<f32>> = (0..8)
///     .map(|k| (0..128).map(|i| ((i as f32 / 128.0) * std::f32::consts::TAU * (1.0 + k as f32)).sin()).collect())
///     .collect();
/// // What the audio thread publishes as it sweeps the table.
/// let position = Arc::new(AtomicF32::new(0.0));
/// let view = wavetable_view("Table", WaveData::from_frames(&frames), AtomicSource::new(position.clone()));
/// let mut ui = Harness::new(view, Size::new(260.0, 160.0), 1.0);
/// ui.frame();
/// position.store(2.5, Ordering::Relaxed);
/// assert_eq!(ui.frame().painted, 1);
/// assert_eq!(ui.get(Role::Image, "Table").value_text(), Some("frame 3.5 of 8, 128 samples each"));
/// ```
pub fn wavetable_view(
    name: impl Into<TextSource>,
    data: WaveData,
    position: impl Source<f32>,
) -> WavetableView {
    let mut polled = Polled::new(data);
    polled.position = Some(Box::new(position));
    WavetableView {
        name: name.into(),
        polled,
        stack: None,
        renderer: None,
        traced: None,
        current: Traced::new(),
    }
}

impl WavetableView {
    /// The data it shows.
    pub fn data(&self) -> &WaveData {
        &self.polled.data
    }
}

/// The strip the frame at `depth` (0 in front, 1 at the back) of a stack
/// in `area` is traced across: the back is higher, further right and a
/// little narrower and flatter, as if further away.
fn plane(area: Rect, depth: f32) -> Rect {
    let run = area.width * 0.18;
    let rise = area.height * 0.36;
    let front_width = area.width - run;
    let width = front_width * (1.0 - 0.18 * depth);
    let left = area.x + run * depth + (front_width - width) * 0.5 * depth;
    let half = (area.height - rise) * 0.47 * (1.0 - 0.18 * depth);
    let middle = area.bottom() - (area.height - rise) * 0.5 - rise * depth;
    Rect::new(left, middle - half, width, 2.0 * half)
}

/// The depth of the frame at `position` in a stack of frames up to
/// `last`.
fn depth(position: f32, last: f32) -> f32 {
    if last > 0.0 { position / last } else { 0.0 }
}

/// Records the stack of `frames` in `area` into `scene`: the depth axis,
/// then the frames from the back, each hiding what is behind it with the
/// card's surface, its line lighter the nearer it is.
fn stack_scene(scene: &mut Scene, frames: &Frames, (area, scale): (Rect, f32), theme: &Theme) {
    let palette = &theme.palette;
    let last = frames.count.saturating_sub(1) as f32;
    let (front, back) = (plane(area, 0.0), plane(area, 1.0));
    let mut axis = Path::new();
    axis.move_to(Point::new(front.x, front.center().y))
        .line_to(Point::new(back.x, back.center().y));
    scene.stroke(
        &axis,
        Stroke::new(device_px(1.0, scale)),
        palette.hairline_strong,
    );
    let shown = frames.count.min(MAX_STACKED_FRAMES);
    let mut traced = Traced::new();
    for k in (0..shown).rev() {
        let index = if shown == 1 {
            0.0
        } else {
            (k as f32 * last / (shown - 1) as f32).round()
        };
        let depth = depth(index, last);
        let strip = plane(area, depth);
        traced.trace(frames, index, (strip, scale), -1.0);
        // Hiding most near the line, and less towards the strip's foot,
        // so that the stack does not end in an edge of its own.
        let stops = [
            Stop::new(0.0, palette.surface.with_opacity(0.92)),
            Stop::new(1.0, palette.surface.with_opacity(0.0)),
        ];
        let fill = LinearGradient {
            start: Point::new(strip.x, strip.y),
            end: Point::new(strip.x, strip.bottom()),
            stops: &stops,
            extend: Extend::Pad,
        };
        scene.columns(traced.outer, &traced.fill_low, &traced.fill_high, fill);
        let color = palette.text_muted.lerp(palette.text_secondary, 1.0 - depth);
        scene.stroke(
            &traced.line,
            Stroke::new(1.0),
            color.with_opacity(0.85 - 0.45 * depth),
        );
    }
}

impl Widget for WavetableView {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Image, self.name.as_str()).with_value(self.polled.describe())
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        Size::new(240.0, 150.0)
    }

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        super::poll_name(&mut self.name, cx);
        let (data, moved) = self.polled.poll();
        if data || moved {
            cx.request_paint();
        }
        if moved {
            // The frame playing is part of what it says.
            cx.request_access_update();
        }
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let theme = cx.theme();
        let (rect, scale) = (cx.rect(), cx.scale());
        let pad = room(theme);
        let area = Rect::new(
            rect.x + pad,
            rect.y + pad,
            (rect.width - 2.0 * pad).max(0.0),
            (rect.height - 2.0 * pad).max(0.0),
        );
        let frames = self.polled.data.load();
        if frames.count == 0 || area.is_empty() {
            return;
        }
        let last = (frames.count - 1) as f32;
        let position = self.polled.position(&frames);
        let key = Key {
            version: self.polled.version.unwrap_or(0),
            rect: area,
            scale,
        };
        let stack_key = StackKey {
            key,
            rect,
            palette: theme.palette,
        };
        if self
            .stack
            .as_ref()
            .is_none_or(|(kept, _)| *kept != stack_key)
        {
            // Drawn with the view's top-left corner, which lies on a device
            // pixel, at the picture's origin.
            let local = Rect::new(pad, pad, area.width, area.height);
            let mut stack = Scene::new();
            stack_scene(&mut stack, &frames, (local, scale), theme);
            let width = (rect.width * scale).round().max(1.0) as u32;
            let height = (rect.height * scale).round().max(1.0) as u32;
            let renderer = self
                .renderer
                .get_or_insert_with(|| Box::new(Renderer::new()));
            self.stack = Pixmap::new(width, height).map(|mut pixels| {
                renderer.render(&stack, &mut pixels, scale);
                (stack_key, Image::new(pixels))
            });
        }
        if let Some((_, image)) = &self.stack {
            scene.image(image, rect, Sampling::Nearest);
        }
        let strip = plane(area, depth(position, last));
        if self.traced != Some((key, position)) {
            self.traced = Some((key, position));
            self.current.trace(&frames, position, (strip, scale), -1.0);
        }
        let accent = theme.palette.accent;
        let stops = [
            Stop::new(0.0, accent.with_opacity(0.45)),
            Stop::new(1.0, accent.with_opacity(0.0)),
        ];
        let fill = LinearGradient {
            start: Point::new(strip.x, strip.y),
            end: Point::new(strip.x, strip.bottom()),
            stops: &stops,
            extend: Extend::Pad,
        };
        self.current.draw(scene, theme, fill);
    }
}

impl fmt::Debug for WavetableView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WavetableView")
            .field("name", &self.name.as_str())
            .field("data", &self.polled.data)
            .finish_non_exhaustive()
    }
}
