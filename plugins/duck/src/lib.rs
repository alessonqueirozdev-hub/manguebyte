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
use std::sync::atomic::Ordering;

use manguebyte::decibels::{db_to_gain, gain_to_db};
use manguebyte::plugin::editor::EditorSpec;
use manguebyte::prelude::*;

mod editor;

pub use editor::{EDITOR_SIZE, METER_TOP_DB, editor};

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
    /// How far the input was turned down at the end of the last block, in
    /// decibels, for the editor's meter. Stored once per block.
    reduction: Arc<AtomicF32>,
}

impl Duck {
    /// How far the input is being turned down right now, in decibels.
    pub fn reduction_db(&self) -> f64 {
        -gain_to_db(self.last_gain)
    }

    /// What the editor's meter reads: how far the input was turned down on
    /// the last frame of the last block, after the bypass fade, in
    /// decibels (0 when bypassed or not ducking). `process` stores it once
    /// per block with a relaxed atomic store, which does not wait, lock or
    /// allocate.
    pub fn reduction_meter(&self) -> Arc<AtomicF32> {
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
        // No "-0": unity gain is no reduction.
        let db = if db > 0.0 { db } else { 0.0 };
        self.reduction.store(db, Ordering::Relaxed);
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
                    .map(|channel| f64::from(channel[start + frame].abs()))
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
            reduction: Arc::new(AtomicF32::new(0.0)),
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
