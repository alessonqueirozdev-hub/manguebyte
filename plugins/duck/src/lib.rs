//! Mangue Duck: an example effect built with Manguebyte.
//!
//! A ducker turns its input down while something else is playing: music
//! under a voice, a bass under a kick drum. The "something else" arrives on
//! the sidechain, a second input that is listened to and never heard.
//!
//! Like [Mangue Pluck](../mangue_pluck/index.html) it is small on purpose.
//! It shows what an effect needs that an instrument does not:
//!
//! - a main input processed in place, and a sidechain input;
//! - a bypass switch the host knows about, that does not click;
//! - automation applied on its exact frame while audio runs through;
//! - a test that feeds both inputs, with no host;
//! - an editor ([`editor()`]) with a knob for each parameter, the bypass
//!   switch and a gain-reduction meter the audio thread feeds through an
//!   atomic.
//!
//! Run `scripts/bundle-clap.sh mangue-duck "Mangue Duck"` or
//! `scripts/bundle-vst3.sh mangue-duck "Mangue Duck"` to get a plugin a
//! host can load.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use manguebyte::decibels::{db_to_gain, gain_to_db};
use manguebyte::plugin::editor::EditorSpec;
use manguebyte::prelude::*;

mod editor;

pub use editor::{EDITOR_SIZE, METER_HOLD, METER_TOP_DB, editor};

/// The plugin's parameters.
pub struct DuckParams {
    /// Sidechain level, in decibels, at which the input starts to go down.
    pub threshold: FloatParam,
    /// The most the input is turned down, in decibels.
    pub depth: FloatParam,
    /// How fast the input goes down, in milliseconds.
    pub attack: FloatParam,
    /// How fast it comes back, in milliseconds.
    pub release: FloatParam,
    /// Lets the input through untouched.
    pub bypass: BoolParam,
}
param_set!(DuckParams {
    threshold,
    depth,
    attack,
    release,
    bypass
});

const THRESHOLD: ParamId = ParamId::from_name("threshold");
const DEPTH: ParamId = ParamId::from_name("depth");
const ATTACK: ParamId = ParamId::from_name("attack");
const RELEASE: ParamId = ParamId::from_name("release");
const BYPASS: ParamId = ParamId::from_name("bypass");

/// How far above the threshold the sidechain has to be for the full depth,
/// in decibels. In between, the input goes down in proportion.
const RANGE_DB: f64 = 12.0;

/// How long switching bypass takes, in seconds. Long enough not to click.
const BYPASS_FADE: f64 = 0.01;

impl DuckParams {
    fn new() -> Self {
        Self {
            threshold: FloatParam::new(
                "threshold",
                "Threshold",
                FloatRange::linear(-60.0, 0.0),
                -30.0,
            )
            .with_unit("dB")
            .with_precision(1),
            depth: FloatParam::new("depth", "Depth", FloatRange::linear(0.0, 40.0), 12.0)
                .with_unit("dB")
                .with_precision(1),
            attack: FloatParam::new(
                "attack",
                "Attack",
                FloatRange::skewed_around(0.1, 200.0, 10.0),
                5.0,
            )
            .with_unit("ms")
            .with_precision(1),
            release: FloatParam::new(
                "release",
                "Release",
                FloatRange::skewed_around(5.0, 2_000.0, 150.0),
                150.0,
            )
            .with_unit("ms")
            .with_precision(0),
            bypass: BoolParam::new("bypass", "Bypass", false).as_bypass(),
        }
    }
}

/// The plugin.
pub struct Duck {
    params: Arc<DuckParams>,
    sample_rate: f64,
    /// The values in effect on the frame being rendered. They follow the
    /// events, so each change lands on its frame.
    threshold_db: f64,
    depth_db: f64,
    attack_ms: f64,
    release_ms: f64,
    /// How much of the distance to the sidechain's level the follower
    /// covers per frame, going up and coming down.
    rise: f64,
    fall: f64,
    /// The sidechain's level as the follower sees it.
    envelope: f64,
    /// One while the effect is in and zero while bypassed, gliding between.
    effect: Smoothed<f32>,
    /// How far into the block rendering has got, to read the sidechain at
    /// the same place.
    cursor: usize,
    /// The gain applied on the last frame rendered, for a meter.
    last_gain: f64,
    /// The gain applied on the last frame rendered, after the bypass fade:
    /// what the input was actually multiplied by.
    last_applied: f32,
    /// How far the input was turned down at the end of the last block, for
    /// the editor's meter. Stored once per block.
    reduction: Arc<ReductionMeter>,
}

/// What the processor tells the editor's meter: how far the input was
/// turned down at the end of the last block, and how many blocks it has
/// published, so that the editor can tell a reading that is still being
/// refreshed from one left behind when the host stopped processing.
///
/// The processor writes both with relaxed atomic stores, which do not
/// wait, lock or allocate; any thread may read them.
///
/// ```
/// use std::sync::Arc;
/// use mangue_duck::ReductionMeter;
///
/// let meter = Arc::new(ReductionMeter::new());
/// assert_eq!(meter.reduction_db(), 0.0);
/// assert_eq!(meter.blocks(), 0);
/// ```
#[derive(Debug, Default)]
pub struct ReductionMeter {
    db: AtomicF32,
    blocks: AtomicU32,
}

impl ReductionMeter {
    /// A meter reading no reduction, with no block published.
    pub fn new() -> Self {
        Self::default()
    }

    /// How far the input was turned down on the last frame of the last
    /// block, after the bypass fade, in decibels: 0 when bypassed or not
    /// ducking, and never more than the largest depth, 40.
    pub fn reduction_db(&self) -> f32 {
        self.db.load(Ordering::Relaxed)
    }

    /// How many times the processor has published, counting from 0 and
    /// wrapping: once per block, and once on each reset.
    pub fn blocks(&self) -> u32 {
        self.blocks.load(Ordering::Relaxed)
    }

    fn publish(&self, db: f32) {
        self.db.store(db, Ordering::Relaxed);
        self.blocks.fetch_add(1, Ordering::Relaxed);
    }
}

impl Duck {
    /// How far the input is being turned down right now, in decibels.
    pub fn reduction_db(&self) -> f64 {
        -gain_to_db(self.last_gain)
    }

    /// What the editor's meter reads (see [`ReductionMeter`]). `process`
    /// publishes into it once per block.
    pub fn reduction_meter(&self) -> Arc<ReductionMeter> {
        Arc::clone(&self.reduction)
    }

    /// The plugin's editor, as [`editor()`] makes it for this instance's
    /// parameters and meter. [`Plugin::editor`] returns the same, boxed.
    ///
    /// ```
    /// use mangue_duck::{Duck, EDITOR_SIZE};
    /// use manguebyte::plugin::Plugin;
    /// use manguebyte::plugin::editor::{EditorSpec, WindowApi};
    ///
    /// let duck = Duck::new();
    /// let spec = duck.editor_spec();
    /// assert_eq!(spec.logical_size(), EDITOR_SIZE);
    /// assert_eq!(spec.supports(WindowApi::Win32), cfg!(windows));
    /// assert!(duck.editor().is_some());
    /// ```
    pub fn editor_spec(&self) -> manguebyte_gui::Editor {
        editor(Arc::clone(&self.params), Arc::clone(&self.reduction))
    }

    /// Publishes the reduction applied on the last frame for the meter.
    fn publish_reduction(&self) {
        let db = -gain_to_db(f64::from(self.last_applied)) as f32;
        // No "-0": unity gain is no reduction. And no more than the largest
        // depth, which the gain held as an `f32` overshoots by a few
        // millionths of a decibel at full depth.
        let db = if db > 0.0 { db.min(METER_TOP_DB) } else { 0.0 };
        self.reduction.publish(db);
    }

    /// The share of the remaining distance a follower with this time
    /// constant covers in one frame.
    fn step(&self, milliseconds: f64) -> f64 {
        1.0 - (-1.0 / (milliseconds.max(0.01) * 0.001 * self.sample_rate)).exp()
    }

    fn update_times(&mut self) {
        self.rise = self.step(self.attack_ms);
        self.fall = self.step(self.release_ms);
    }

    /// Takes every parameter's current value, with no fade. From here on
    /// the plugin follows the events.
    fn take_params(&mut self) {
        self.threshold_db = f64::from(self.params.threshold.value());
        self.depth_db = f64::from(self.params.depth.value());
        self.attack_ms = f64::from(self.params.attack.value());
        self.release_ms = f64::from(self.params.release.value());
        self.update_times();
        let effect = if self.params.bypass.value() { 0.0 } else { 1.0 };
        self.effect.set_immediate(effect);
    }

    fn handle(&mut self, event: &Event) {
        let EventKind::ParamValue { param, value } = event.kind else {
            return;
        };
        // The value as the parameter holds it (see `Param::stored_plain`):
        // limited to the range and rounded to an `f32`, which is what
        // `take_params` starts from, so a value sent again changes nothing.
        // A value that is not a number is a fault, not a setting, and is
        // ignored, as the parameter ignores it: otherwise the audio would go
        // silent or not a number while the editor showed the old value.
        let params = &*self.params;
        let held = match param {
            THRESHOLD => params.threshold.stored_plain(value),
            DEPTH => params.depth.stored_plain(value),
            ATTACK => params.attack.stored_plain(value),
            RELEASE => params.release.stored_plain(value),
            BYPASS => params.bypass.stored_plain(value),
            _ => None,
        };
        let Some(value) = held else {
            return;
        };
        match param {
            THRESHOLD => self.threshold_db = value,
            DEPTH => self.depth_db = value,
            ATTACK => {
                self.attack_ms = value;
                self.update_times();
            }
            RELEASE => {
                self.release_ms = value;
                self.update_times();
            }
            BYPASS => self.effect.set_target(if value >= 0.5 { 0.0 } else { 1.0 }),
            _ => {}
        }
    }

    /// Renders one stretch of the block during which no parameter changes.
    fn render(&mut self, piece: &mut AudioBuffer<f32>, sidechain: Option<&AudioBuffer<f32>>) {
        let start = self.cursor;
        self.cursor += piece.num_frames();
        for frame in 0..piece.num_frames() {
            // The loudest sidechain channel on this frame. With nothing
            // routed to the sidechain there is nothing to duck under.
            let key = sidechain.map_or(0.0, |sidechain| {
                sidechain
                    .iter_channels()
                    .map(|channel| loudness(channel[start + frame]))
                    .fold(0.0, f64::max)
            });
            let pace = if key > self.envelope {
                self.rise
            } else {
                self.fall
            };
            self.envelope += pace * (key - self.envelope);
            // Stop the fall before it reaches numbers too small to be worth
            // computing with, which some processors handle very slowly.
            if self.envelope < 1e-9 {
                self.envelope = 0.0;
            }

            let over = gain_to_db(self.envelope) - self.threshold_db;
            let gain = if over > 0.0 {
                db_to_gain(-self.depth_db * (over / RANGE_DB).min(1.0))
            } else {
                1.0
            };
            self.last_gain = gain;
            // Bypass fades the effect out instead of switching it off.
            let applied = 1.0 + self.effect.next_value() * (gain as f32 - 1.0);
            self.last_applied = applied;
            for channel in piece.iter_channels_mut() {
                channel[frame] *= applied;
            }
        }
    }
}

/// How loud one sidechain sample is to the follower: its magnitude. A
/// sample that is infinite or not a number is a fault, not a level, and
/// counts as silence: otherwise one such sample from a host would leave the
/// follower infinite, and on the next quiet frame not a number, for good.
fn loudness(sample: f32) -> f64 {
    if sample.is_finite() {
        f64::from(sample.abs())
    } else {
        0.0
    }
}

impl Plugin for Duck {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.duck",
        name: "Mangue Duck",
        vendor: "Manguebyte",
        version: env!("CARGO_PKG_VERSION"),
        description: "A sidechain ducker, as an example of the Manguebyte framework",
        kind: PluginKind::Effect,
    };
    const LAYOUT: AudioLayout = AudioLayout {
        inputs: 2,
        outputs: 2,
        sidechain: 2,
    };
    type Params = DuckParams;

    fn new() -> Self {
        let params = Arc::new(DuckParams::new());
        let mut duck = Self {
            sample_rate: 48_000.0,
            threshold_db: 0.0,
            depth_db: 0.0,
            attack_ms: 0.0,
            release_ms: 0.0,
            rise: 0.0,
            fall: 0.0,
            envelope: 0.0,
            effect: Smoothed::new(1.0),
            cursor: 0,
            last_gain: 1.0,
            last_applied: 1.0,
            reduction: Arc::new(ReductionMeter::new()),
            params,
        };
        duck.take_params();
        duck
    }

    fn params(&self) -> Arc<DuckParams> {
        Arc::clone(&self.params)
    }

    fn prepare(&mut self, spec: &ProcessSpec) {
        self.sample_rate = spec.sample_rate;
        self.effect.set_ramp(spec.sample_rate, BYPASS_FADE);
        // A state restored before the plugin was switched on takes effect
        // whole, bypass included, rather than fading in.
        self.take_params();
        self.reset();
    }

    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        self.cursor = 0;
        let sidechain = context.sidechain;
        split_at_events(self, buffer, context.events, Self::handle, |duck, piece| {
            duck.render(piece, sidechain)
        });
        self.publish_reduction();
    }

    fn reset(&mut self) {
        self.envelope = 0.0;
        self.last_gain = 1.0;
        self.last_applied = 1.0;
        self.effect.set_immediate(self.effect.target());
        self.publish_reduction();
    }

    fn editor(&self) -> Option<Box<dyn EditorSpec>> {
        Some(Box::new(self.editor_spec()))
    }
}

manguebyte::clap::export_clap!(Duck);
manguebyte::vst3::export_vst3!(Duck);

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 48_000.0;

    /// A stereo buffer holding the same value on every frame.
    fn steady(level: f32, frames: usize) -> AudioBuffer<f32> {
        let mut buffer = AudioBuffer::new(2, frames);
        for channel in buffer.iter_channels_mut() {
            channel.fill(level);
        }
        buffer
    }

    /// Something that is not the same on every frame, to pass through.
    fn programme(frames: usize) -> AudioBuffer<f32> {
        let mut buffer = AudioBuffer::new(2, frames);
        for (index, channel) in buffer.iter_channels_mut().enumerate() {
            for (frame, sample) in channel.iter_mut().enumerate() {
                let phase = frame as f32 * (0.05 + 0.01 * index as f32);
                *sample = 0.5 * phase.sin();
            }
        }
        buffer
    }

    #[test]
    fn leaves_the_input_alone_while_the_sidechain_is_quiet() {
        let mut harness = Harness::<Duck>::new(RATE, 256);
        let input = programme(4_800);
        // Nothing routed to the sidechain at all.
        assert!(harness.process(&input) == input);
        // Silence on it, and then something below the threshold.
        assert!(harness.process_with_sidechain(&input, &steady(0.0, 4_800)) == input);
        let quiet = steady(db_to_gain(-40.0) as f32, 4_800);
        assert!(harness.process_with_sidechain(&input, &quiet) == input);
        assert_eq!(harness.plugin_mut().reduction_db(), 0.0);
    }

    #[test]
    fn turns_the_input_down_by_the_depth_and_lets_it_come_back() {
        let mut harness = Harness::<Duck>::new(RATE, 256);
        let input = steady(1.0, 24_000);

        // A sidechain at full level is far above the threshold.
        let ducked = harness.process_with_sidechain(&input, &steady(1.0, 24_000));
        let expected = db_to_gain(-12.0) as f32;
        assert!((ducked.channel(0)[23_999] - expected).abs() < 1e-4);
        assert!((harness.plugin_mut().reduction_db() - 12.0).abs() < 1e-3);
        assert_eq!(ducked.channel(0), ducked.channel(1));
        // It went down over the attack time, not at once.
        assert_eq!(ducked.channel(0)[4], 1.0);
        let on_the_way = ducked.channel(0)[12];
        assert!(on_the_way < 0.9 && on_the_way > expected + 0.05);

        // Half-way between the threshold and the top of the range, half
        // the depth.
        harness.reset();
        let half = steady(db_to_gain(-24.0) as f32, 24_000);
        let ducked = harness.process_with_sidechain(&input, &half);
        assert!((ducked.channel(0)[23_999] - db_to_gain(-6.0) as f32).abs() < 1e-3);

        // The sidechain stops and the input comes back up.
        let input = steady(1.0, 48_000);
        let after = harness.process_with_sidechain(&input, &steady(0.0, 48_000));
        assert!(after.channel(0)[0] < 0.6);
        assert_eq!(after.channel(0)[47_999], 1.0);
    }

    #[test]
    fn automation_lands_on_its_frame_with_audio_running_through() {
        let mut harness = Harness::<Duck>::new(RATE, 256);
        let input = steady(1.0, 24_000);
        let loud = steady(1.0, 24_000);
        // Let the follower settle, then ask for a different depth on one
        // particular frame in the middle of a block.
        harness.process_with_sidechain(&input, &loud);
        harness.automate(24_000 + 1_000, "depth", 24.0);
        let output = harness.process_with_sidechain(&input, &loud);

        let before = db_to_gain(-12.0) as f32;
        let after = db_to_gain(-24.0) as f32;
        assert!((output.channel(0)[999] - before).abs() < 1e-4);
        assert!((output.channel(0)[1_000] - after).abs() < 1e-4);
    }

    #[test]
    fn bypass_lets_the_input_through_without_a_click() {
        let mut harness = Harness::<Duck>::new(RATE, 256);
        let input = steady(1.0, 24_000);
        let loud = steady(1.0, 24_000);
        harness.process_with_sidechain(&input, &loud);

        harness.automate(24_000 + 100, "bypass", 1.0);
        let output = harness.process_with_sidechain(&input, &loud);
        let samples = output.channel(0);
        // Still ducked up to the frame of the switch, untouched soon after.
        assert!(samples[99] < 0.3);
        assert_eq!(samples[2_000], 1.0);
        assert_eq!(samples[23_999], 1.0);
        // In between it glides: no step larger than the fade allows.
        let largest_step = samples
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).abs())
            .fold(0.0, f32::max);
        assert!(largest_step < 0.01, "a step of {largest_step}");

        // A plugin prepared with bypass already on is bypassed from its
        // first frame, with no fade.
        harness.plugin_mut().prepare(&ProcessSpec {
            sample_rate: RATE,
            max_block_size: 256,
            num_channels: 2,
        });
        assert!(harness.process_with_sidechain(&input, &loud) == input);
    }

    #[test]
    fn sounds_the_same_whatever_the_block_size() {
        let render = |max: usize, pattern: &[usize]| {
            let mut harness = Harness::<Duck>::new(RATE, max).with_block_pattern(pattern);
            let input = programme(24_000);
            // A sidechain that comes and goes.
            let mut sidechain = AudioBuffer::new(2, 24_000);
            for (frame, sample) in sidechain.channel_mut(1).iter_mut().enumerate() {
                *sample = if (frame / 3_000) % 2 == 0 { 0.8 } else { 0.0 };
            }
            harness.automate(2_500, "depth", 30.0);
            harness.automate(7_777, "release", 20.0);
            harness.automate(9_001, "threshold", -12.0);
            harness.automate(15_000, "bypass", 1.0);
            harness.automate(18_123, "bypass", 0.0);
            harness.process_with_sidechain(&input, &sidechain)
        };
        let reference = render(256, &[256]);
        assert!(reference != programme(24_000), "nothing was ducked");
        assert!(render(64, &[64]) == reference);
        assert!(render(1_024, &[1, 999, 13, 1_024]) == reference);
    }

    #[test]
    fn a_sidechain_that_is_not_a_number_or_infinite_does_not_stop_the_ducking() {
        let mut harness = Harness::<Duck>::new(RATE, 256);
        let input = steady(1.0, 24_000);
        let ducked = db_to_gain(-12.0) as f32;
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            harness.reset();
            // A few bad frames among silence, then silence long enough for
            // the follower to come all the way back.
            let mut sidechain = steady(0.0, 48_000);
            for frame in [10, 11, 500] {
                sidechain.channel_mut(1)[frame] = bad;
            }
            let output = harness.process_with_sidechain(&steady(1.0, 48_000), &sidechain);
            assert!(output.channel(0).iter().all(|sample| sample.is_finite()));
            assert_eq!(output.channel(0)[47_999], 1.0, "{bad}");
            let meter = harness.plugin_mut().reduction_meter();
            assert_eq!(meter.reduction_db(), 0.0, "{bad}");
            // And a loud sidechain ducks by the depth again.
            let output = harness.process_with_sidechain(&input, &steady(1.0, 24_000));
            assert!(
                (output.channel(0)[23_999] - ducked).abs() < 1e-4,
                "after {bad}: {}",
                output.channel(0)[23_999]
            );
            assert!((meter.reduction_db() - 12.0).abs() < 1e-3, "{bad}");
        }
    }

    /// Builds a parameter event as both wrappers do, from the position the
    /// host sends (`Param::normalized_to_plain`), and schedules it.
    fn from_host(harness: &mut Harness<Duck>, frame: u64, name: &str, position: f64) {
        let param = harness
            .params()
            .find(name)
            .unwrap_or_else(|| panic!("no parameter {name}"));
        let kind = EventKind::ParamValue {
            param: param.param_id(),
            value: param.normalized_to_plain(position),
        };
        harness.schedule(frame, kind);
    }

    /// A host that sends a position that is not a number gets it converted
    /// by the wrappers to a value that is not a number either, and the
    /// parameter refuses to store it. The processor ignores it too: the
    /// output stays finite, it goes on ducking by the depth the editor
    /// shows, and the meter stays within its range.
    #[test]
    fn a_parameter_value_that_is_not_a_number_changes_nothing() {
        let ducked = db_to_gain(-12.0) as f32;
        for name in ["threshold", "depth", "attack", "release", "bypass"] {
            let mut harness = Harness::<Duck>::new(RATE, 256);
            // During the attack, and again once it has settled.
            from_host(&mut harness, 3, name, f64::NAN);
            from_host(&mut harness, 100, name, f64::NAN);
            from_host(&mut harness, 20_000, name, f64::NAN);
            let output = harness.process_with_sidechain(&steady(1.0, 24_000), &steady(1.0, 24_000));
            let first_bad = output.channel(0).iter().position(|s| !s.is_finite());
            assert_eq!(first_bad, None, "{name}");
            assert!(
                (output.channel(0)[23_999] - ducked).abs() < 1e-4,
                "{name}: {}",
                output.channel(0)[23_999]
            );
            let meter = harness.plugin_mut().reduction_meter().reduction_db();
            assert!(
                (meter - 12.0).abs() < 1e-3,
                "{name}: the meter reads {meter}"
            );
            // The same as with no event at all, frame for frame.
            let mut plain = Harness::<Duck>::new(RATE, 256);
            let reference =
                plain.process_with_sidechain(&steady(1.0, 24_000), &steady(1.0, 24_000));
            assert!(output == reference, "{name}");
            // And the parameters, which the editor shows, are untouched.
            let params = harness.params();
            assert_eq!(params.threshold.value(), -30.0);
            assert_eq!(params.depth.value(), 12.0);
        }
    }

    /// Values beyond a parameter's range, from a host's infinite position
    /// or from an event made by hand, act as the range's nearer end, which
    /// is what the parameter would store.
    #[test]
    fn a_value_beyond_the_range_acts_as_the_nearer_end() {
        let render = |events: &[(&str, f64)]| {
            let mut harness = Harness::<Duck>::new(RATE, 256);
            for &(name, value) in events {
                harness.automate(1_000, name, value);
            }
            let output = harness.process_with_sidechain(&steady(1.0, 24_000), &steady(0.2, 24_000));
            assert!(
                output.channel(0).iter().all(|s| s.is_finite()),
                "{events:?}"
            );
            let meter = harness.plugin_mut().reduction_meter().reduction_db();
            assert!((0.0..=METER_TOP_DB).contains(&meter), "{events:?}: {meter}");
            output
        };
        for (name, low, high) in [
            ("threshold", -60.0, 0.0),
            ("depth", 0.0, 40.0),
            ("attack", 0.1, 200.0),
            ("release", 5.0, 2_000.0),
        ] {
            for (beyond, end) in [
                (f64::INFINITY, high),
                (1e30, high),
                (f64::NEG_INFINITY, low),
                (-1e30, low),
            ] {
                assert!(
                    render(&[(name, beyond)]) == render(&[(name, end)]),
                    "{name} at {beyond}"
                );
            }
        }
        // What the wrappers make of an infinite position is the same.
        let mut harness = Harness::<Duck>::new(RATE, 256);
        from_host(&mut harness, 1_000, "depth", f64::INFINITY);
        let output = harness.process_with_sidechain(&steady(1.0, 24_000), &steady(0.2, 24_000));
        assert!(output == render(&[("depth", 40.0)]));
    }

    /// The values the wrappers deliver are already what the parameters
    /// hold, so an event and `take_params` agree on them without rounding.
    /// (This is why rounding events to `f32` did not explain the
    /// differences between REAPER renders in TECH_DEBT 42.)
    #[test]
    fn the_values_the_wrappers_deliver_are_what_the_parameters_hold() {
        let duck = Duck::new();
        let params = duck.params();
        let mut all = Vec::new();
        params.visit(&mut |param| all.push(param));
        assert_eq!(all.len(), 5);
        for param in all {
            for step in 0..=10_000_u32 {
                let position = f64::from(step) / 10_000.0;
                let plain = param.normalized_to_plain(position);
                assert_eq!(
                    param.stored_plain(plain).map(f64::to_bits),
                    Some(plain.to_bits()),
                    "{} at {position}",
                    param.id()
                );
            }
        }
    }

    /// Hosts send a parameter's value again, and where it lands depends on
    /// where blocks start. A value sent again must change nothing, even one
    /// an `f32` cannot hold exactly, as `Harness::automate` passes it on.
    #[test]
    fn values_sent_again_change_nothing() {
        let held = [
            ("threshold", -27.3),
            ("depth", 17.7),
            ("attack", 7.3),
            ("release", 333.3),
        ];
        let render = |again: Option<(&str, f64)>| {
            let mut harness = Harness::<Duck>::new(RATE, 256);
            let params = harness.params();
            for (field, value) in [
                (&params.threshold, held[0].1),
                (&params.depth, held[1].1),
                (&params.attack, held[2].1),
                (&params.release, held[3].1),
            ] {
                field.set(value as f32);
            }
            // As a session restored before playback: the plugin takes the
            // values the parameters hold.
            harness.plugin_mut().prepare(&ProcessSpec {
                sample_rate: RATE,
                max_block_size: 256,
                num_channels: 2,
            });
            if let Some((name, value)) = again {
                for frame in (0..48_000).step_by(997) {
                    harness.automate(frame, name, value);
                }
            }
            let mut sidechain = AudioBuffer::new(2, 48_000);
            for (frame, sample) in sidechain.channel_mut(0).iter_mut().enumerate() {
                *sample = if (frame / 4_000) % 2 == 0 { 0.7 } else { 0.0 };
            }
            harness.process_with_sidechain(&programme(48_000), &sidechain)
        };
        let reference = render(None);
        assert!(reference != programme(48_000), "nothing was ducked");
        for again in held {
            let output = render(Some(again));
            let first = (0..48_000).find(|&frame| {
                (0..2).any(|channel| {
                    output.channel(channel)[frame].to_bits()
                        != reference.channel(channel)[frame].to_bits()
                })
            });
            assert_eq!(first, None, "{again:?} sent again");
        }
    }

    #[test]
    fn state_round_trips_and_survives_a_reset() {
        let harness = Harness::<Duck>::new(RATE, 256);
        harness.params().depth.set(33.0);
        harness.params().bypass.set(true);
        let saved = harness.save_state();

        let restored = Harness::<Duck>::new(RATE, 256);
        restored.load_state(&saved).unwrap();
        assert_eq!(restored.params().depth.value(), 33.0);
        assert!(restored.params().bypass.value());
    }
}
