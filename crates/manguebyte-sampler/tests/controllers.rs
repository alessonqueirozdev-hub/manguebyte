//! Controllers modulating the opcodes of playing notes (`<opcode>_onccN`),
//! set by MIDI messages or by a plugin through `Sampler::set_controller`,
//! with their initial values, their smoothing and their numbers past MIDI's.

mod common;

use core::f64::consts::{FRAC_PI_4, SQRT_2};

use common::*;
use manguebyte_dsp::Processor;
use manguebyte_dsp::event::EventKind;
use manguebyte_midi::MidiMessage;
use manguebyte_sampler::{Instrument, MemorySource, ModulationTarget, Sampler, Sfz};

/// A second of DC at 0.5, played at its own rate on key 60: what a voice
/// outputs is 0.5 times its gain.
fn dc() -> MemorySource {
    MemorySource::new().with("dc.wav", constant_wav(0.5, 48_000, 48_000))
}

fn dc_sampler(opcodes: &str, channels: usize) -> Sampler<f32> {
    let text = format!("<region> sample=dc.wav pitch_keycenter=60 {opcodes}");
    sampler_with(&text, &mut dc(), 8, channels)
}

fn middle_c() -> f64 {
    440.0 * 2f64.powf(-9.0 / 12.0)
}

fn mean(signal: &[f32]) -> f64 {
    signal.iter().map(|&x| f64::from(x)).sum::<f64>() / signal.len() as f64
}

fn assert_level(signal: &[f32], expected: f64, what: &str) {
    for (n, &x) in signal.iter().enumerate() {
        assert!(
            (f64::from(x) - expected).abs() < 1e-6,
            "{what}: frame {n} is {x}, not {expected}"
        );
    }
}

fn midi_cc(controller: u8, value: u8) -> EventKind {
    EventKind::Midi(MidiMessage::ControlChange {
        channel: 0,
        controller,
        value,
    })
}

#[test]
fn amplitude_follows_a_plugin_parameter_and_a_midi_controller() {
    let mut sampler = dc_sampler("amplitude=0 amplitude_oncc300=100 amplitude_oncc20=50", 1);
    let output = render_with(
        &mut sampler,
        1,
        12_000,
        &[(0, on(60, 1.0)), (7_200, midi_cc(20, 127))],
        &[(2_400, 300, 0.5)],
        &[256],
    );
    assert_level(&output[0][..2_400], 0.0, "both at 0");
    assert_level(&output[0][3_000..7_200], 0.25, "300 at a half");
    assert_level(&output[0][7_800..], 0.5, "and 20 at full");
    assert_eq!(sampler.controller(300), 0.5);
    assert_eq!(sampler.controller(20), 1.0);
}

#[test]
fn several_modulations_of_one_opcode_add_up() {
    let mut sampler = dc_sampler("volume_oncc300=-6 volume_oncc301=-6 gain_oncc302=12", 1);
    let output = render_with(
        &mut sampler,
        1,
        9_600,
        &[(960, on(60, 1.0))],
        &[(0, 300, 1.0), (0, 301, 1.0), (4_800, 302, 0.5)],
        &[256],
    );
    // Set before the note, which starts with them where they arrived.
    let quarter = 0.5 * 10f64.powf(-12.0 / 20.0);
    assert_level(&output[0][960..4_800], quarter, "-12 dB");
    assert_level(
        &output[0][5_400..],
        quarter * 10f64.powf(6.0 / 20.0),
        "-6 dB",
    );
}

#[test]
fn pan_follows_its_controller_with_the_constant_power_law() {
    let law = |pan: f64| {
        let angle = (pan + 1.0) * FRAC_PI_4;
        (0.5 * SQRT_2 * angle.cos(), 0.5 * SQRT_2 * angle.sin())
    };
    for (opcodes, before, value, pan) in [
        ("pan_oncc302=100", 0.0, 0.5f32, 0.5f64),
        ("pan_oncc302=-100", 0.0, 1.0, -1.0),
        ("pan=-100 pan_oncc302=200", -1.0, 1.0, 1.0),
        ("pan=50 pan_oncc302=100", 0.5, 1.0, 1.0),
    ] {
        let mut sampler = dc_sampler(opcodes, 2);
        let output = render_with(
            &mut sampler,
            2,
            4_800,
            &[(0, on(60, 1.0))],
            &[(1_200, 302, value)],
            &[256],
        );
        let (left, right) = law(before);
        assert_level(&output[0][..1_200], left, opcodes);
        assert_level(&output[1][..1_200], right, opcodes);
        let (left, right) = law(pan);
        assert_level(&output[0][2_400..], left, opcodes);
        assert_level(&output[1][2_400..], right, opcodes);
    }
}

#[test]
fn tune_and_pitch_move_the_pitch_by_their_cents() {
    let text = "<region> sample=*sine pitch_keytrack=0 tune_oncc303=1200 pitch_oncc304=-100";
    let mut sampler = sampler_with(text, &mut MemorySource::new(), 8, 1);
    let output = render_with(
        &mut sampler,
        1,
        24_000,
        &[(0, on(60, 1.0))],
        &[(4_800, 303, 0.25), (14_400, 304, 1.0)],
        &[256],
    );
    let f = middle_c();
    let checks = [
        (0..4_800, 0.0),
        (6_000..14_400, 300.0),
        (15_600..24_000, 200.0),
    ];
    for (range, expected) in checks {
        let measured = cents(frequency(&output[0][range.clone()], RATE), f);
        assert!(
            (measured - expected).abs() < 0.1,
            "{range:?}: {measured} cents, not {expected}"
        );
    }
}

#[test]
fn cutoff_and_resonance_follow_their_controllers() {
    let f = middle_c();
    let base = "<region> sample=*sine pitch_keytrack=0";
    let mut plain = sampler_with(base, &mut MemorySource::new(), 8, 1);
    let plain = render_mono(&mut plain, 4_800, &[(0, on(60, 1.0))]);
    let reference = sine_fit(&plain[1_200..4_800], f, RATE).0;
    let text = format!(
        "{base} cutoff={} cutoff_oncc305=1200 resonance_oncc306=12 resonance=3 resonance_oncc307=-3",
        f / 2.0
    );
    let mut sampler = sampler_with(&text, &mut MemorySource::new(), 8, 1);
    let output = render_with(
        &mut sampler,
        1,
        19_200,
        &[(0, on(60, 1.0))],
        &[(0, 307, 1.0), (4_800, 305, 1.0), (12_000, 306, 1.0)],
        &[256],
    );
    let gain_at = |from: usize, to: usize| {
        20.0 * (sine_fit(&output[0][from..to], f, RATE).0 / reference).log10()
    };
    let octave = -10.0
        * (1.0
            + ((core::f64::consts::PI * f / RATE).tan()
                / (core::f64::consts::PI * f / 2.0 / RATE).tan())
            .powi(4))
        .log10();
    let checks = [
        (1_200, 4_800, octave, "an octave above the cutoff"),
        (6_000, 12_000, -3.0103, "at the cutoff"),
        (13_200, 19_200, 12.0 - 3.0103, "with 12 dB of resonance"),
    ];
    for (from, to, expected, what) in checks {
        let measured = gain_at(from, to);
        assert!((measured - expected).abs() < 0.02, "{what}: {measured} dB");
    }
}

#[test]
fn envelope_times_and_levels_are_read_when_their_stage_starts() {
    let opcodes = "ampeg_attack_oncc307=0.1 ampeg_sustain_oncc309=-50 ampeg_release_oncc308=1";
    let mut sampler = dc_sampler(opcodes, 1);
    let output = render_with(
        &mut sampler,
        1,
        36_000,
        &[(960, on(60, 1.0)), (10_560, off(60))],
        // Set before the note; the attack's time changes during the
        // attack and the release's after the release starts, and neither
        // stage notices.
        &[
            (0, 307, 1.0),
            (0, 309, 1.0),
            (0, 308, 1.0),
            (1_960, 307, 0.0),
            (10_660, 308, 0.0),
        ],
        &[256],
    );
    // A 0.1 s attack: a quarter of the full level half way.
    let at = |frame: usize| f64::from(output[0][960 + frame]);
    assert!((at(2_399) - 0.25).abs() < 1e-6, "{}", at(2_399));
    assert!((at(4_799) - 0.5).abs() < 1e-6, "{}", at(4_799));
    // Sustained at half.
    assert_level(&output[0][5_960..10_560], 0.25, "sustain");
    // The release falls 90 dB in its 1.001 s: 45 dB half way.
    let half = 10_560 + 24_024;
    let expected = 0.25 * 10f64.powf(-45.0 / 20.0);
    let measured = f64::from(output[0][half - 1]);
    assert!(
        (measured / expected - 1.0).abs() < 0.01,
        "{measured} against {expected}"
    );
}

#[test]
fn the_filter_envelope_takes_its_depth_and_stages_from_controllers() {
    let f = middle_c();
    let base = "<region> sample=*sine pitch_keytrack=0";
    let mut plain = sampler_with(base, &mut MemorySource::new(), 8, 1);
    let plain = render_mono(&mut plain, 4_800, &[(0, on(60, 1.0))]);
    let reference = sine_fit(&plain[1_200..4_800], f, RATE).0;
    // Two octaves of depth at full controller, and a sustain the
    // controller raises from 0 to full.
    let text = format!(
        "{base} cutoff={} fileg_depth_oncc310=2400 fileg_sustain_oncc311=100",
        f / 4.0
    );
    let mut sampler = sampler_with(&text, &mut MemorySource::new(), 8, 1);
    let output = render_with(
        &mut sampler,
        1,
        14_400,
        &[(960, on(60, 1.0))],
        &[(0, 311, 1.0), (4_800, 310, 1.0)],
        &[256],
    );
    let gain_at = |from: usize, to: usize| {
        20.0 * (sine_fit(&output[0][from..to], f, RATE).0 / reference).log10()
    };
    let two_octaves = -10.0
        * (1.0
            + ((core::f64::consts::PI * f / RATE).tan()
                / (core::f64::consts::PI * f / 4.0 / RATE).tan())
            .powi(4))
        .log10();
    let before = gain_at(2_400, 4_800);
    assert!(
        (before - two_octaves).abs() < 0.05,
        "no depth yet: {before} dB"
    );
    let after = gain_at(7_200, 14_400);
    assert!((after + 3.0103).abs() < 0.05, "full depth: {after} dB");
}

#[test]
fn extended_controllers_are_out_of_reach_of_midi_messages() {
    let mut sampler = dc_sampler("amplitude=0 amplitude_oncc300=100 amplitude_oncc172=100", 1);
    // 300 is 44 past 256 and 172 is 44 past 128: neither is reached by
    // controller 44, nor by a message that claims controller 172.
    let output = render_with(
        &mut sampler,
        1,
        4_800,
        &[
            (0, on(60, 1.0)),
            (10, midi_cc(44, 127)),
            (20, midi_cc(172, 127)),
        ],
        &[(2_400, 300, 1.0)],
        &[256],
    );
    assert_level(&output[0][..2_400], 0.0, "MIDI");
    assert_level(&output[0][3_000..], 0.5, "the plugin");
    assert_eq!(sampler.controller(44), 1.0);
    assert_eq!(sampler.controller(172), 0.0);
    // Numbers past the table are ignored.
    sampler.set_controller(512, 1.0, 0);
    sampler.set_controller(u16::MAX, 1.0, 0);
    assert_eq!(sampler.controller(512), 0.0);
}

#[test]
fn set_cc_gives_the_initial_values_and_a_reset_returns_to_them() {
    let text = "<control> set_cc20=64 set_hdcc300=0.25 set_cc301=127 label_cc300=Bright
                <region> sample=dc.wav pitch_keycenter=60 amplitude=0 amplitude_oncc300=100";
    let sfz = Sfz::parse(text).unwrap();
    assert!(sfz.warnings.is_empty(), "{:?}", sfz.warnings);
    assert_eq!(sfz.control.label_cc, [(300, "Bright".to_owned())]);
    let instrument = Instrument::from_sfz(text, &mut dc()).unwrap();
    assert_eq!(instrument.controller_default(20), 64.0 / 127.0);
    assert_eq!(instrument.controller_default(301), 1.0);
    let mut sampler = sampler_with(text, &mut dc(), 8, 1);
    assert_eq!(sampler.controller(20), (64.0f64 / 127.0) as f32);
    assert_eq!(sampler.controller(300), 0.25);
    // The note starts at the initial value, from its first frame.
    let output = render_mono(&mut sampler, 2_400, &[(0, on(60, 1.0))]);
    assert_level(&output, 0.125, "initial value");

    sampler.set_controller(300, 1.0, 0);
    let output = render_mono(&mut sampler, 2_400, &[(1_000, midi_cc(20, 0))]);
    assert_level(&output[960..], 0.5, "moved");
    // Reset all controllers puts MIDI's back, never the extended ones.
    render_mono(&mut sampler, 64, &[(0, midi_cc(121, 0))]);
    assert_eq!(sampler.controller(20), (64.0f64 / 127.0) as f32);
    assert_eq!(sampler.controller(300), 1.0);
    // A reset puts them all back.
    sampler.reset();
    assert_eq!(sampler.controller(300), 0.25);
}

#[test]
fn a_new_instrument_leaves_set_controllers_alone() {
    let first = "<control> set_hdcc300=0.25 set_hdcc301=0.25 <region> sample=*sine";
    let second = "<control> set_hdcc300=0.75 set_hdcc301=0.75 <region> sample=*sine";
    let (mut sampler, mut loader) = Sampler::<f32>::new(8);
    loader
        .load(Instrument::from_sfz(first, &mut MemorySource::new()).unwrap())
        .unwrap();
    render_mono(&mut sampler, 64, &[]);
    assert_eq!(
        (sampler.controller(300), sampler.controller(301)),
        (0.25, 0.25)
    );
    sampler.set_controller(300, 0.5, 0);
    render_mono(&mut sampler, 64, &[]);
    loader
        .load(Instrument::from_sfz(second, &mut MemorySource::new()).unwrap())
        .unwrap();
    render_mono(&mut sampler, 64, &[]);
    assert_eq!(
        (sampler.controller(300), sampler.controller(301)),
        (0.5, 0.75)
    );
}

#[test]
fn a_jump_glides_without_a_step() {
    // Amplitude: a straight line over 10 ms.
    let mut sampler = dc_sampler("amplitude=0 amplitude_oncc300=100", 1);
    let output = render_with(
        &mut sampler,
        1,
        9_600,
        &[(0, on(60, 1.0))],
        &[(4_800, 300, 1.0)],
        &[256],
    );
    let largest = |signal: &[f32]| {
        signal
            .windows(2)
            .map(|pair| (f64::from(pair[1]) - f64::from(pair[0])).abs())
            .fold(0.0, f64::max)
    };
    let step = largest(&output[0]);
    assert!(step <= 0.5 / 480.0 + 1e-6, "{step}");
    assert_level(&output[0][4_800 + 480 + 32..], 0.5, "arrived");

    // Volume: 40 dB in 10 ms, its steepest part at the top.
    let mut sampler = dc_sampler("volume=-40 volume_oncc301=40", 1);
    let output = render_with(
        &mut sampler,
        1,
        9_600,
        &[(0, on(60, 1.0))],
        &[(4_800, 301, 1.0)],
        &[256],
    );
    let steepest = 0.5 * 2.0 * core::f64::consts::LN_10 / 480.0;
    let step = largest(&output[0]);
    assert!(step <= steepest * 1.05, "{step} against {steepest}");

    // Cutoff: four octaves up in 10 ms. A step would show as a large
    // second difference; a sine of full level has at most A (2πf/fs)².
    let text = format!(
        "<region> sample=*sine pitch_keytrack=0 cutoff={} cutoff_oncc302=4800",
        middle_c() / 4.0
    );
    let mut sampler = sampler_with(&text, &mut MemorySource::new(), 8, 1);
    let output = render_with(
        &mut sampler,
        1,
        9_600,
        &[(0, on(60, 1.0))],
        &[(4_800, 302, 1.0)],
        &[256],
    );
    let bend = output[0]
        .windows(3)
        .map(|w| (f64::from(w[2]) - 2.0 * f64::from(w[1]) + f64::from(w[0])).abs())
        .fold(0.0, f64::max);
    let sine = (core::f64::consts::TAU * middle_c() / RATE).powi(2);
    assert!(bend < 1.5 * sine, "{bend} against {sine}");
    assert!(mean(&output[0]).abs() < 0.01);
}

const BUSY: &str = "
<control> set_cc74=64 set_hdcc300=0.5
<global> ampeg_release=0.05 cutoff=1500 resonance=6 fil_keytrack=50 fileg_depth=1200
         fileg_attack=0.01 fileg_decay=0.05 fileg_sustain=30 fileg_release=0.03
         cutoff_oncc74=2400 resonance_oncc300=12 volume_oncc300=-6 pan_oncc20=50
         tune_oncc1=100 ampeg_attack_oncc300=0.01 fileg_depth_oncc301=-600
<group> lokey=0 hikey=63 fil_type=lpf_4p
<region> sample=*sine
<group> lokey=64 hikey=127 fil_type=hpf_2p
<region> sample=dc.wav pitch_keycenter=60
";

fn busy_run(blocks: &[usize]) -> Vec<Vec<f32>> {
    let mut sampler = sampler_with(BUSY, &mut dc(), 16, 2);
    let events = [
        (0, on(48, 0.8)),
        (100, on(70, 0.5)),
        (777, midi_cc(74, 10)),
        (1_500, midi_cc(20, 127)),
        (2_000, midi_cc(1, 90)),
        (3_333, off(48)),
        (4_000, on(55, 1.0)),
        (6_100, off(70)),
        (7_000, off(55)),
    ];
    let changes = [
        (0, 300, 0.1),
        (50, 301, 1.0),
        (1_234, 300, 0.9),
        (1_240, 300, 0.2),
        (4_000, 301, 0.0),
        (5_555, 300, 1.0),
    ];
    render_with(&mut sampler, 2, 9_600, &events, &changes, blocks)
}

#[test]
fn controllers_and_filters_do_not_depend_on_the_blocks_and_repeat_exactly() {
    let whole = busy_run(&[512]);
    assert!(whole[0].iter().any(|&x| x.abs() > 0.05));
    assert_eq!(busy_run(&[512]), whole, "the same run twice");
    for blocks in [&[1usize, 7, 64, 333][..], &[16], &[4_095]] {
        assert_eq!(busy_run(blocks), whole, "{blocks:?}");
    }
}

#[test]
fn modulation_opcodes_are_read_in_every_spelling() {
    let sfz = Sfz::parse(
        "<group> cutoff_cc5=100 ampeg_attackcc6=0.5 volume_oncc7=-3
         <region> sample=a.wav cutoff_oncc5=200 fileg_release_oncc511=1 amp_hold_oncc8=2
         pitch_oncc9=50 cutoff_curvecc1=2 tune_oncc512=1",
    )
    .unwrap();
    let region = &sfz.regions[0];
    let found: Vec<(ModulationTarget, u16, f64)> = region
        .modulations
        .iter()
        .map(|m| (m.target, m.controller, m.depth))
        .collect();
    use manguebyte_sampler::EnvelopeStage::{Attack, Hold, Release};
    assert_eq!(
        found,
        [
            (ModulationTarget::Cutoff, 5, 200.0),
            (ModulationTarget::Ampeg(Attack), 6, 0.5),
            (ModulationTarget::Volume, 7, -3.0),
            (ModulationTarget::Fileg(Release), 511, 1.0),
            (ModulationTarget::Ampeg(Hold), 8, 2.0),
            (ModulationTarget::Pitch, 9, 50.0),
        ]
    );
    // Curves are not played, and 512 is past the last controller.
    let unknown: Vec<&str> = region.unknown.iter().map(|o| &*o.name).collect();
    assert_eq!(unknown, ["cutoff_curvecc1", "tune_oncc512"]);
}
