//! Hostile SFZ text, hostile sample files and hostile event streams: errors
//! name a line that exists, and nothing panics.

mod common;

use common::*;
use manguebyte_dsp::event::{EventKind, NoteRef};
use manguebyte_sampler::{Instrument, MemorySource, SampleSource, Sfz, SfzErrorKind};

/// Pieces SFZ text is made of, valid and not.
const PIECES: &[&str] = &[
    "<region>",
    "<group>",
    "<global>",
    "<master>",
    "<control>",
    "<curve>",
    "<effect>",
    "<regoin>",
    "<",
    ">",
    "<>",
    "< region >",
    "sample=a.wav",
    "sample=b.wav",
    "sample=",
    "sample=*sine",
    "sample=*silence",
    "sample=*saw",
    "lokey=",
    "lokey=c#4",
    "lokey=-5",
    "hikey=999",
    "key=g9",
    "key=h2",
    "pitch_keycenter=sample",
    "lovel=64",
    "hivel=0",
    "seq_length=0",
    "seq_length=3",
    "seq_position=2",
    "lorand=0.3",
    "hirand=2",
    "trigger=release",
    "trigger=first",
    "trigger=legato",
    "trigger=release_key",
    "trigger=never",
    "volume=-inf",
    "volume=1e308",
    "amplitude=50",
    "pan=-100",
    "tune=-20",
    "transpose=48",
    "pitch_keytrack=1200",
    "amp_veltrack=-100",
    "rt_decay=200",
    "offset=1000000",
    "end=-1",
    "end=0",
    "end=5",
    "loop_mode=one_shot",
    "loop_mode=loop_sustain",
    "loop_mode=loop_continuous",
    "loop_mode=no_loop",
    "loop_mode=sometimes",
    "loop_start=3",
    "loop_end=2",
    "loop_end=99999999999",
    "loop_crossfade=10",
    "ampeg_attack=100",
    "ampeg_release=0",
    "ampeg_sustain=0",
    "ampeg_hold=0.001",
    "ampeg_decay=0.5",
    "ampeg_vel2attack=-100",
    "group=-1",
    "off_by=1",
    "off_by=-1",
    "off_mode=time",
    "off_time=0",
    "off_mode=normal",
    "polyphony=1",
    "polyphony=0",
    "bend_up=9600",
    "note_offset=200",
    "octave_offset=-11",
    "default_path=../x/",
    "sw_last=36",
    "=",
    "==",
    "a=b=c",
    "=5",
    "x",
    "#define $A 5",
    "#define $AB $A$A",
    "#define",
    "#define $ 1",
    "$A",
    "$AB",
    "$UNDEFINED",
    "#include \"inc.sfz\"",
    "#include \"self.sfz\"",
    "#include \"missing.sfz\"",
    "#include inc.sfz",
    "//",
    "/*",
    "*/",
    "\n",
    "\n",
    "\n",
    " ",
    "\t",
    "\r\n",
    "\u{feff}",
    "é",
    "\0",
    "😀",
];

/// A source with a valid sample, a broken one, and include files, one of
/// which includes itself.
fn source() -> MemorySource {
    MemorySource::new()
        .with("a.wav", sine_wav(440.0, 48_000, 0.05, 0.5))
        .with("b.wav", b"RIFF\x24\x00\x00\x00WAVEfmt junk".to_vec())
        .with(
            "inc.sfz",
            "<region> sample=a.wav key=60\n".as_bytes().to_vec(),
        )
        .with("self.sfz", "#include \"self.sfz\"\n".as_bytes().to_vec())
        .with("x/a.wav", sine_wav(220.0, 22_050, 0.05, 0.5))
        // Where `default_path=../x/` sends `sample=a.wav`.
        .with("../x/a.wav", sine_wav(220.0, 22_050, 0.05, 0.5))
}

fn random_text(random: &mut Random) -> String {
    let count = 1 + random.below(40);
    let mut text = String::new();
    for _ in 0..count {
        if random.below(12) == 0 {
            // Raw bytes, as text.
            let bytes: Vec<u8> = (0..random.below(8))
                .map(|_| random.next_u64() as u8)
                .collect();
            text.push_str(&String::from_utf8_lossy(&bytes));
        } else {
            text.push_str(PIECES[random.below(PIECES.len())]);
        }
        text.push_str([" ", "\n", "", "\t"][random.below(4)]);
    }
    text
}

#[test]
fn malformed_text_gives_errors_on_real_lines_and_never_panics() {
    let mut random = Random::new(2026);
    let (mut parsed, mut failed, mut loaded) = (0, 0, 0);
    for _ in 0..8_000 {
        let text = random_text(&mut random);
        let lines = text.split('\n').count();
        let mut source = source();
        match Sfz::parse_with(&text, &mut source) {
            Ok(sfz) => {
                parsed += 1;
                for warning in &sfz.warnings {
                    assert!(warning.location.line >= 1);
                    assert!(!warning.to_string().is_empty());
                }
                if let Ok(instrument) = Instrument::load(&sfz, &mut source) {
                    loaded += 1;
                    assert_eq!(instrument.num_regions(), sfz.regions.len());
                }
            }
            Err(error) => {
                failed += 1;
                // Errors in the given text name one of its lines; errors in
                // an included file name that file.
                if error.location.file.is_none() {
                    assert!(
                        (1..=lines).contains(&error.location.line),
                        "line {} of {lines} for {text:?}: {error}",
                        error.location.line
                    );
                }
                assert!(!error.to_string().is_empty());
            }
        }
    }
    // The generator reaches both outcomes often.
    assert!(
        parsed > 800 && failed > 800 && loaded > 400,
        "{parsed} {failed} {loaded}"
    );
}

#[test]
fn includes_resolve_and_cycles_and_floods_are_refused() {
    let mut source = source();
    let sfz = Sfz::parse_with("<group> lovel=10\n#include \"inc.sfz\"", &mut source).unwrap();
    assert_eq!(sfz.regions.len(), 1);
    assert_eq!(sfz.regions[0].lovel, 10);
    assert_eq!(sfz.regions[0].location.file.as_deref(), Some("inc.sfz"));

    let error = Sfz::parse_with("\n#include \"self.sfz\"", &mut source).unwrap_err();
    assert!(matches!(error.kind, SfzErrorKind::IncludeCycle(_)));

    // A file that includes another twice, thirty levels deep, would be
    // read 2^30 times; the depth limit stops it.
    let mut flood = MemorySource::new();
    for level in 0..30 {
        let next = format!("#include \"f{}.sfz\"\n", level + 1);
        flood.insert(
            &format!("f{level}.sfz"),
            format!("{next}{next}").into_bytes(),
        );
    }
    let error = Sfz::parse_with("#include \"f0.sfz\"", &mut flood).unwrap_err();
    assert_eq!(error.kind, SfzErrorKind::TooManyIncludes);

    // Defines that double at every step are held to a bounded size.
    let mut text = String::from("#define $A xx\n");
    for _ in 0..20 {
        text.push_str("#define $A $A$A\n");
    }
    let error = Sfz::parse(&text).unwrap_err();
    assert_eq!(error.kind, SfzErrorKind::DefineTooLong);
    assert!(error.location.line <= 21);

    // A missing include is an error with the path.
    let error = Sfz::parse_with("#include \"missing.sfz\"", &mut source).unwrap_err();
    assert!(error.to_string().contains("missing.sfz"));
    assert!(source.read("missing.sfz").is_err());
}

#[test]
fn inherited_text_is_shared_not_copied_into_every_region() {
    // About 6 KB of text: one 64 KiB value in <global>, inherited by 200
    // regions. Were each region to keep its own copy, memory would grow
    // with value length times region count (10 000 regions and a 1 MiB
    // line, both allowed, would need about 10 GB).
    let mut text = format!(
        "#define $A {}\n<global> sample=*sine mystery=",
        "x".repeat(4_096)
    );
    text.push_str(&"$A".repeat(16));
    text.push('\n');
    text.push_str(&"<region>".repeat(200));
    let sfz = Sfz::parse(&text).unwrap();
    assert_eq!(sfz.regions.len(), 200);
    // Every region keeps the opcode it does not know...
    for region in &sfz.regions {
        assert_eq!(region.unknown.len(), 1);
        assert_eq!(region.unknown[0].value.len(), 65_536);
    }
    // ...and the bytes held are those of the distinct values, once each.
    let mut seen = std::collections::HashSet::new();
    let held: usize = sfz
        .regions
        .iter()
        .flat_map(|region| region.unknown.iter().map(|opcode| &opcode.value))
        .chain(sfz.regions.iter().map(|region| &region.sample))
        .filter(|value| seen.insert(std::sync::Arc::as_ptr(value)))
        .map(|value| value.len())
        .sum();
    assert!(
        held <= 64 * text.len(),
        "{} bytes of text keep {held} bytes of opcode values",
        text.len()
    );
    // It is reported once, not once per region.
    assert_eq!(sfz.warnings.len(), 1);
}

#[test]
fn a_flood_of_inherited_opcodes_is_refused() {
    // 200 opcodes in <global> under 6 000 regions: 1.2 million opcodes for
    // the regions to take, in 50 KB of text.
    let mut text = String::from("<global> sample=*sine");
    for _ in 0..200 {
        text.push_str(" pan=1");
    }
    text.push('\n');
    text.push_str(&"<region>\n".repeat(6_000));
    let error = Sfz::parse(&text).unwrap_err();
    assert_eq!(error.kind, SfzErrorKind::TooManyOpcodes);
    // The region that went over the limit: 1 048 576 / 201 regions fit.
    let fit = manguebyte_sampler::sfz::MAX_REGION_OPCODES / 201;
    assert_eq!(error.location.line, fit + 2, "{error}");
    assert!(error.to_string().contains("opcodes"));
    // Half as many regions parse.
    let text = text.replacen(&"<region>\n".repeat(3_000), "", 1);
    assert_eq!(Sfz::parse(&text).unwrap().regions.len(), 3_000);
}

#[test]
fn hostile_sample_files_are_load_errors() {
    let wav = sine_wav(440.0, 48_000, 0.01, 0.5);
    let mut random = Random::new(99);
    for round in 0..2_000 {
        let mut bytes = wav.clone();
        match round % 3 {
            0 => bytes.truncate(random.below(bytes.len())),
            1 => {
                for _ in 0..1 + random.below(8) {
                    let at = random.below(64.min(bytes.len()));
                    bytes[at] = random.next_u64() as u8;
                }
            }
            _ => {
                bytes = (0..random.below(200))
                    .map(|_| random.next_u64() as u8)
                    .collect()
            }
        }
        let mut source = MemorySource::new().with("s.wav", bytes);
        if let Ok(instrument) = Instrument::from_sfz("<region> sample=s.wav", &mut source) {
            // Whatever loads, plays.
            let (mut sampler, _loader) =
                manguebyte_sampler::Sampler::<f32>::with_instrument(instrument, 4);
            use manguebyte_dsp::Processor;
            sampler.prepare(&manguebyte_dsp::ProcessSpec {
                sample_rate: RATE,
                max_block_size: 64,
                num_channels: 2,
            });
            let output = render(&mut sampler, 2, 600, &[(0, on(60, 1.0))], &[64]);
            assert!(output.iter().flatten().all(|s| s.is_finite()));
        }
    }
}

#[test]
fn hostile_instruments_and_events_play_without_panicking() {
    let mut random = Random::new(4242);
    let mut played = 0;
    for _ in 0..400 {
        let text = random_text(&mut random);
        let mut source = source();
        let Ok(instrument) = Instrument::from_sfz(&text, &mut source) else {
            continue;
        };
        let (mut sampler, _loader) =
            manguebyte_sampler::Sampler::<f32>::with_instrument(instrument, 1 + random.below(6));
        use manguebyte_dsp::Processor;
        sampler.prepare(&manguebyte_dsp::ProcessSpec {
            sample_rate: [8_000.0, 48_000.0, 192_000.0][random.below(3)],
            max_block_size: 128,
            num_channels: 1 + random.below(3),
        });
        let mut events = Vec::new();
        for _ in 0..60 {
            let at = random.below(4_000);
            let note = NoteRef::new(random.below(16) as u8, random.below(128) as u8);
            let kind = match random.below(6) {
                0 | 1 => EventKind::NoteOn {
                    note,
                    velocity: [0.0, 1e-9, 0.5, 1.0, 7.0, f32::NAN, -1.0][random.below(7)],
                },
                2 => EventKind::NoteOff {
                    note,
                    velocity: 0.0,
                },
                3 => controller(
                    [64, 120, 121, 123, 7][random.below(5)],
                    random.below(128) as u8,
                ),
                4 => bend([i16::MIN, -1, 0, 8_191, i16::MAX][random.below(5)]),
                _ => EventKind::NoteChoke { note },
            };
            events.push((at, kind));
        }
        events.sort_by_key(|&(at, _)| at);
        let channels = 2;
        let output = render(
            &mut sampler,
            channels,
            4_000,
            &events,
            &[1 + random.below(128)],
        );
        assert!(output.iter().flatten().all(|s| s.is_finite()));
        played += 1;
    }
    assert!(played > 50, "{played}");
}
