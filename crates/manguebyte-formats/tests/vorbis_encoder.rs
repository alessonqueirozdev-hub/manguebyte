//! The Ogg Vorbis encoder, through this crate's decoder and FFmpeg's.
//!
//! The `encoder-*.ogg` files in `data/` are what the encoder wrote from
//! three of the test sources, and the `.ffmpeg.ref` and `.libvorbis.ref`
//! files beside them what FFmpeg 7.1's own Vorbis decoder and libvorbis
//! decoded them to (see `data/README.md`): three decoders written apart
//! from each other give the same audio, to the rounding of floats. How
//! near that audio comes to its source is measured with a noise-to-mask
//! ratio of its own model (`common/nmr.rs`), apart from the encoder's.

mod common;

use std::io::Cursor;

use common::lossy::{closeness, fitted_closeness, read_in_random_blocks, source};
use common::nmr::nmr;
use common::opus::Reference;
use common::{Failing, Rng, slice};
use manguebyte_core::AudioBuffer;
use manguebyte_formats::metadata::{Metadata, Picture};
use manguebyte_formats::vorbis::{self, VorbisError, VorbisOptions};
use manguebyte_formats::{AudioWriter, FileFormat, FormatError, decode};

/// Encodes `buffer` whole.
fn encode(buffer: &AudioBuffer<f32>, rate: u32, options: &VorbisOptions) -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    vorbis::write(&mut bytes, buffer, rate, options).unwrap();
    bytes.into_inner()
}

/// The files in `data/`: the name, the source, its quality.
const FIXTURES: [(&str, &str, f32); 3] = [
    ("encoder-signal-1ch-22050-q5", "signal-1ch-22050", 5.0),
    ("encoder-signal-2ch-48000-q3", "signal-2ch-48000", 3.0),
    ("encoder-signal-6ch-48000-q0", "signal-6ch-48000", 0.0),
];

macro_rules! fixture {
    ($name:literal) => {
        (
            include_bytes!(concat!("data/", $name, ".ogg")).as_slice(),
            include_bytes!(concat!("data/", $name, ".ffmpeg.ref")).as_slice(),
            include_bytes!(concat!("data/", $name, ".libvorbis.ref")).as_slice(),
        )
    };
}

fn fixture_bytes(name: &str) -> (&'static [u8], &'static [u8], &'static [u8]) {
    match name {
        "encoder-signal-1ch-22050-q5" => fixture!("encoder-signal-1ch-22050-q5"),
        "encoder-signal-2ch-48000-q3" => fixture!("encoder-signal-2ch-48000-q3"),
        _ => fixture!("encoder-signal-6ch-48000-q0"),
    }
}

/// The largest difference between `decoded`, clipped to `-1.0..=clip`,
/// and the frames of `reference`.
fn largest_difference(decoded: &AudioBuffer<f32>, reference: &Reference, clip: f32) -> f32 {
    let mut worst = 0.0f32;
    for (kept, frame) in (0..reference.frames).step_by(reference.step).enumerate() {
        for channel in 0..reference.channels {
            let theirs = reference.samples[kept * reference.channels + channel];
            let ours = decoded.channel(channel)[frame].clamp(-1.0, clip);
            worst = worst.max((ours - theirs).abs());
        }
    }
    worst
}

#[test]
fn ffmpeg_and_libvorbis_decode_what_the_encoder_writes_as_this_crate_does() {
    for (name, source_name, quality) in FIXTURES {
        let (bytes, ffmpeg, libvorbis) = fixture_bytes(name);
        let (source, rate) = source(source_name);
        let file = vorbis::read(bytes).unwrap();
        assert_eq!(file.sample_rate, rate);
        assert_eq!(file.buffer.num_channels(), source.num_channels());
        assert_eq!(
            file.buffer.num_frames(),
            source.num_frames(),
            "{name}: length"
        );
        // FFmpeg's decoder gives floats; libvorbis, through FFmpeg, 16-bit
        // samples, which are rounded and clipped.
        for (decoder, reference, clip, tolerance) in [
            ("FFmpeg", ffmpeg, f32::MAX, 2e-6),
            ("libvorbis", libvorbis, 32_767.0 / 32_768.0, 2.5 / 32_768.0),
        ] {
            let reference = Reference::parse(reference);
            assert_eq!(
                reference.frames,
                source.num_frames(),
                "{name}: {decoder}'s length"
            );
            assert_eq!(reference.channels, source.num_channels());
            let worst = largest_difference(&file.buffer, &reference, clip);
            println!("{name}: {decoder} differs by {worst:.1e} at most");
            assert!(worst <= tolerance, "{name}: {decoder}: {worst}");
        }
        // The encoder still writes these bytes: when it changes, the files
        // are made again with `dump_files_for_other_software`. Only the
        // vendor, which names the version of this crate, may differ.
        let again = encode(&source, rate, &VorbisOptions::new(quality));
        let (theirs, ours) = (Headers::split(bytes), Headers::split(&again));
        assert!(
            theirs.vendor().starts_with(b"manguebyte-formats "),
            "{name}: the vendor of the file in data/"
        );
        assert_eq!(ours.vendor(), VENDOR.as_bytes(), "{name}: the vendor");
        assert!(
            ours.without_vendor() == theirs.without_vendor(),
            "{name}: the encoder no longer writes the headers in data/"
        );
        assert!(
            again[ours.audio..] == bytes[theirs.audio..],
            "{name}: the encoder no longer writes the audio in data/"
        );
    }
}

/// The vendor the encoder writes, as its source gives it.
const VENDOR: &str = concat!("manguebyte-formats ", env!("CARGO_PKG_VERSION"));

/// The three header packets of an Ogg Vorbis stream, the fields of the
/// pages they are on, and where the pages of audio after them start.
struct Headers {
    /// Identification, comment and setup.
    packets: [Vec<u8>; 3],
    /// Bytes 4 to 22 of each page of headers: the version, the flags, the
    /// granule position, the serial number and the page number, which the
    /// length of the vendor does not change.
    pages: Vec<[u8; 18]>,
    /// The offset of the first page of audio.
    audio: usize,
}

impl Headers {
    fn split(bytes: &[u8]) -> Self {
        let mut packets: Vec<Vec<u8>> = vec![Vec::new()];
        let mut pages = Vec::new();
        let ranges = common::ogg_pages(bytes);
        for (index, range) in ranges.iter().enumerate() {
            let page = &bytes[range.clone()];
            pages.push(page[4..22].try_into().unwrap());
            let segments = usize::from(page[26]);
            let mut at = 27 + segments;
            for &lacing in &page[27..27 + segments] {
                let length = usize::from(lacing);
                packets
                    .last_mut()
                    .unwrap()
                    .extend_from_slice(&page[at..at + length]);
                at += length;
                if length < 255 {
                    packets.push(Vec::new());
                }
            }
            if packets.len() > 3 {
                // The setup header ends a page of its own.
                assert_eq!(packets.len(), 4, "audio on a page of headers");
                assert!(packets[3].is_empty());
                packets.pop();
                return Self {
                    packets: packets.try_into().unwrap(),
                    pages,
                    audio: ranges
                        .get(index + 1)
                        .expect("audio after the headers")
                        .start,
                };
            }
        }
        panic!("fewer than three header packets");
    }

    /// The vendor in the comment header (section 5.2.1 of the Vorbis I
    /// specification): its length in four bytes after the packet type and
    /// "vorbis", then the text.
    fn vendor(&self) -> &[u8] {
        let comment = &self.packets[1];
        let length = u32::from_le_bytes(comment[7..11].try_into().unwrap()) as usize;
        &comment[11..11 + length]
    }

    /// Everything but the vendor: the identification and setup headers,
    /// the comment header without its vendor, and the fields of the pages.
    fn without_vendor(&self) -> (&[u8], &[u8], &[u8], &[u8], &[[u8; 18]]) {
        let comment = &self.packets[1];
        (
            &self.packets[0],
            &comment[..7],
            &comment[11 + self.vendor().len()..],
            &self.packets[2],
            &self.pages,
        )
    }
}

#[test]
fn every_length_comes_back_exactly() {
    for channels in [1usize, 2, 3] {
        for (rate, frames) in [
            (44_100, 0usize),
            (44_100, 1),
            (44_100, 255),
            (44_100, 256),
            (44_100, 257),
            (44_100, 1_023),
            (44_100, 2_048),
            (44_100, 2_049),
            (44_100, 5_000),
            (22_050, 3_333),
            (8_000, 777),
            (96_000, 12_345),
            (48_000, 48_013),
        ] {
            let buffer = common::ramp(channels, frames);
            let bytes = encode(&buffer, rate, &VorbisOptions::new(4.0));
            let file = vorbis::read(bytes.as_slice()).unwrap();
            assert_eq!(
                file.buffer.num_frames(),
                frames,
                "{channels} channels at {rate} Hz"
            );
            assert_eq!(file.sample_rate, rate);
        }
    }
}

#[test]
fn quality_moves_the_noise_and_the_rate_together() {
    let (buffer, rate) = source("signal-2ch-44100");
    let frames = buffer.num_frames();
    let seconds = frames as f64 / f64::from(rate);
    let mut last: Option<(f64, f64)> = None;
    for (quality, nmr_at_most, tones_at_least, kbps_within) in [
        (0.0f32, 1.0, 7.0, 60.0..95.0),
        (3.0, -3.5, 10.0, 90.0..130.0),
        (5.0, -9.0, 15.5, 105.0..145.0),
        (8.0, -15.5, 20.0, 140.0..190.0),
        (10.0, -20.0, 25.0, 150.0..210.0),
    ] {
        let bytes = encode(&buffer, rate, &VorbisOptions::new(quality));
        let kbps = bytes.len() as f64 * 8.0 / seconds / 1_000.0;
        let decoded = vorbis::read(bytes.as_slice()).unwrap().buffer;
        let masked = nmr(&decoded, &buffer, rate, 0..frames);
        let (snr, _) = closeness(&decoded, &buffer, 0..frames);
        let (_, tones) = fitted_closeness(&decoded, &buffer, 0..frames * 3 / 10);
        println!(
            "quality {quality}: {kbps:.1} kbit/s, noise-to-mask {:.1} dB ({:.1} % of bands above the mask), {snr:.1} dB, {tones:.1} dB on the tones",
            masked.mean_db,
            100.0 * masked.audible
        );
        assert!(
            masked.mean_db <= nmr_at_most,
            "quality {quality}: {:.1} dB",
            masked.mean_db
        );
        assert!(tones >= tones_at_least, "quality {quality}: {tones:.1} dB");
        assert!(
            kbps_within.contains(&kbps),
            "quality {quality}: {kbps:.1} kbit/s"
        );
        if let Some((last_kbps, last_nmr)) = last {
            assert!(
                kbps > last_kbps && masked.mean_db < last_nmr,
                "quality {quality}"
            );
        }
        last = Some((kbps, masked.mean_db));
    }
}

#[test]
fn every_way_of_writing_gives_the_same_bytes_however_the_audio_is_cut() {
    let (buffer, rate) = source("signal-2ch-24000");
    let options = VorbisOptions::new(6.0).with_tag("ARTIST", "Chico Science");
    let expected = encode(&buffer, rate, &options);
    // Twice the same: the encoder is deterministic.
    assert!(encode(&buffer, rate, &options) == expected);
    let frames = buffer.num_frames();
    for seed in 1..4u64 {
        let mut rng = Rng::new(seed);
        let mut writer = AudioWriter::vorbis(Cursor::new(Vec::new()), 2, rate, &options).unwrap();
        assert_eq!(writer.format(), FileFormat::OggVorbis);
        let mut done = 0;
        while done < frames {
            let count = (rng.between(0, 3_000) as usize).min(frames - done);
            let piece = slice(&buffer, done..done + count);
            match rng.below(3) {
                0 => writer.write(&piece).unwrap(),
                1 => {
                    let mut interleaved = Vec::with_capacity(count * 2);
                    for i in 0..count {
                        interleaved.push(piece.channel(0)[i]);
                        interleaved.push(piece.channel(1)[i]);
                    }
                    writer.write_interleaved(&interleaved).unwrap();
                }
                _ if piece
                    .iter_channels()
                    .all(|channel| channel.iter().all(|&s| s == 0.0)) =>
                {
                    writer.write_silence(count as u64).unwrap();
                }
                _ => writer.write(&piece).unwrap(),
            }
            done += count;
            assert_eq!(writer.frames_written(), done as u64);
        }
        let bytes = writer.finish().unwrap().into_inner();
        assert!(bytes == expected, "seed {seed}");
    }
    // Silence written as silence is the same as zeros.
    let zeros = AudioBuffer::<f32>::new(2, 10_000);
    let mut writer = AudioWriter::vorbis(Cursor::new(Vec::new()), 2, rate, &options).unwrap();
    writer.write_silence(4_000).unwrap();
    writer.write(&slice(&zeros, 0..6_000)).unwrap();
    assert!(writer.finish().unwrap().into_inner() == encode(&zeros, rate, &options));
}

#[test]
fn what_is_written_reads_back_through_every_reader() {
    let (buffer, rate) = source("signal-1ch-48000");
    let bytes = encode(&buffer, rate, &VorbisOptions::new(5.0));
    let whole = decode(&bytes).unwrap();
    assert_eq!(whole.format, FileFormat::OggVorbis);
    assert_eq!(whole.buffer.num_frames(), buffer.num_frames());
    assert_eq!(read_in_random_blocks(&bytes, 7), whole.buffer);
    common::lossy::check_seeks_are_exact("encoder", &bytes, &whole.buffer, 40, 3);
}

#[test]
fn comments_and_pictures_come_back() {
    let metadata = Metadata {
        title: Some("Manguetown".to_owned()),
        artist: Some("NaÃ§Ã£o Zumbi".to_owned()),
        pictures: vec![Picture {
            kind: 3,
            mime: "image/png".to_owned(),
            description: "capa".to_owned(),
            width: 1,
            height: 1,
            depth: 24,
            colors: 0,
            data: vec![0x89, b'P', b'N', b'G', 1, 2, 3],
        }],
        ..Metadata::default()
    };
    let options = VorbisOptions::new(2.0)
        .with_metadata(&metadata)
        .with_tag("comment", "a=b");
    let buffer = common::ramp(2, 3_000);
    let bytes = encode(&buffer, 32_000, &options);
    let file = vorbis::read(bytes.as_slice()).unwrap();
    assert!(file.vendor.starts_with("manguebyte-formats "));
    assert_eq!(file.metadata.title, metadata.title);
    assert_eq!(file.metadata.artist, metadata.artist);
    assert_eq!(file.metadata.pictures, metadata.pictures);
    assert!(
        file.comments
            .contains(&("comment".to_owned(), "a=b".to_owned()))
    );
    // A writer takes them in its options, not afterwards.
    let mut writer = AudioWriter::vorbis(Cursor::new(Vec::new()), 2, 32_000, &options).unwrap();
    assert!(matches!(
        writer.set_metadata(&metadata),
        Err(FormatError::Unsupported(_))
    ));
}

#[test]
fn options_are_checked_before_anything_is_written() {
    let buffer = AudioBuffer::<f32>::new(1, 100);
    for (options, channels, rate) in [
        (VorbisOptions::new(10.5), 1usize, 44_100u32),
        (VorbisOptions::new(-0.5), 1, 44_100),
        (VorbisOptions::new(f32::NAN), 1, 44_100),
        (VorbisOptions::new(5.0).with_tag("A=B", "c"), 1, 44_100),
        (VorbisOptions::new(5.0).with_tag("", "c"), 1, 44_100),
        (VorbisOptions::new(5.0).with_tag("TÃTULO", "c"), 1, 44_100),
        (VorbisOptions::new(5.0), 0, 44_100),
        (VorbisOptions::new(5.0), 256, 44_100),
        (VorbisOptions::new(5.0), 1, 0),
    ] {
        let mut out = Cursor::new(Vec::new());
        let result = AudioWriter::vorbis(&mut out, channels, rate, &options);
        assert!(
            matches!(
                result,
                Err(FormatError::Vorbis(VorbisError::Unsupported(_)))
            ),
            "{options:?} {channels} {rate}"
        );
        assert!(out.into_inner().is_empty());
        if channels == 1 && rate > 0 {
            let mut out = Cursor::new(Vec::new());
            assert!(matches!(
                vorbis::write(&mut out, &buffer, rate, &options),
                Err(VorbisError::Unsupported(_))
            ));
        }
    }
}

#[test]
fn hostile_samples_are_written_and_read_back_finite() {
    let frames = 20_000;
    let mut buffer = AudioBuffer::<f32>::new(2, frames);
    let mut rng = Rng::new(99);
    for channel in buffer.iter_channels_mut() {
        for (i, sample) in channel.iter_mut().enumerate() {
            *sample = match i % 7 {
                0 => f32::NAN,
                1 => f32::INFINITY,
                2 => f32::NEG_INFINITY,
                3 => f32::MAX,
                4 => 1e-40,
                _ => (rng.below(2_000) as f32 - 1_000.0) * 1e3,
            };
        }
    }
    for quality in [0.0, 10.0] {
        let bytes = encode(&buffer, 44_100, &VorbisOptions::new(quality));
        let decoded = vorbis::read(bytes.as_slice()).unwrap().buffer;
        assert_eq!(decoded.num_frames(), frames);
        assert!(
            decoded
                .iter_channels()
                .all(|channel| channel.iter().all(|s| s.is_finite()))
        );
    }
    // A single channel of full-scale square waves and clicks, at the
    // rates and channel counts of the extremes.
    for (channels, rate) in [(1usize, 1u32), (8, 192_000), (255, 8_000)] {
        let mut buffer = AudioBuffer::<f32>::new(channels, 3_000);
        for (index, channel) in buffer.iter_channels_mut().enumerate() {
            for (i, sample) in channel.iter_mut().enumerate() {
                *sample = if (i / (index + 3)) % 2 == 0 {
                    1.0
                } else {
                    -1.0
                };
            }
        }
        let bytes = encode(&buffer, rate, &VorbisOptions::new(5.0));
        let file = vorbis::read(bytes.as_slice()).unwrap();
        assert_eq!(
            (file.buffer.num_channels(), file.buffer.num_frames()),
            (channels, 3_000)
        );
    }
}

#[test]
fn a_page_that_could_not_be_written_is_written_again() {
    let (buffer, rate) = source("signal-2ch-32000");
    let options = VorbisOptions::default();
    let expected = encode(&buffer, rate, &options);
    let disk = Failing::new(Cursor::new(Vec::new()), 6_000);
    let budget = std::sync::Arc::clone(&disk.budget);
    let mut writer = AudioWriter::vorbis(disk, 2, rate, &options).unwrap();
    let mut done = 0;
    let mut failures = 0;
    while done < buffer.num_frames() {
        let count = 1_500.min(buffer.num_frames() - done);
        let before = writer.frames_written();
        match writer.write(&slice(&buffer, done..done + count)) {
            Ok(()) => done += count,
            Err(FormatError::Io(_)) => {
                failures += 1;
                done += (writer.frames_written() - before) as usize;
                // Room for the pages kept, which are written again whole.
                budget.fetch_add(5_000, std::sync::atomic::Ordering::Relaxed);
            }
            Err(other) => panic!("{other}"),
        }
    }
    assert!(failures >= 3, "{failures} failures");
    budget.fetch_add(1 << 20, std::sync::atomic::Ordering::Relaxed);
    let bytes = writer.finish().unwrap().into_inner().into_inner();
    assert!(bytes == expected);
}

#[test]
fn a_file_can_be_written_to_and_read_from_a_path() {
    let directory = std::env::temp_dir().join(format!("manguebyte-vorbis-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("tone.ogg");
    let buffer = common::ramp(1, 4_000);
    vorbis::write_file(&path, &buffer, 16_000, &VorbisOptions::new(1.0)).unwrap();
    let file = vorbis::read_file(&path).unwrap();
    assert_eq!(file.buffer.num_frames(), 4_000);
    let mut writer =
        AudioWriter::create_vorbis(&path, 1, 16_000, &VorbisOptions::new(1.0)).unwrap();
    writer.write(&buffer).unwrap();
    writer.finish().unwrap();
    assert_eq!(vorbis::read_file(&path).unwrap().buffer, file.buffer);
    // Options refused leave the file as it was.
    assert!(AudioWriter::create_vorbis(&path, 0, 16_000, &VorbisOptions::new(1.0)).is_err());
    assert_eq!(vorbis::read_file(&path).unwrap().buffer, file.buffer);
    std::fs::remove_dir_all(&directory).unwrap();
}

/// Encodes the test sources at every quality and prints what comes out,
/// with the speed; with `MANGUEBYTE_DUMP_DIR` set, writes the files there
/// too.
#[test]
#[ignore = "prints measurements"]
fn measure_the_qualities() {
    let dump = std::env::var("MANGUEBYTE_DUMP_DIR").ok();
    for name in [
        "signal-2ch-44100",
        "signal-1ch-22050",
        "tones-2ch-48000",
        "signal-6ch-48000",
    ] {
        let (buffer, rate) = source(name);
        for quality in [0.0f32, 2.0, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0] {
            let options = VorbisOptions::new(quality);
            let started = std::time::Instant::now();
            let bytes = encode(&buffer, rate, &options);
            let elapsed = started.elapsed().as_secs_f64();
            let seconds = buffer.num_frames() as f64 / f64::from(rate);
            let kbps = bytes.len() as f64 * 8.0 / seconds / 1_000.0;
            let decoded = vorbis::read(bytes.as_slice()).unwrap();
            let frames = buffer.num_frames();
            let (snr, worst) = closeness(&decoded.buffer, &buffer, 0..frames);
            let (_, tones) = fitted_closeness(&decoded.buffer, &buffer, 0..frames * 3 / 10);
            let masked = nmr(&decoded.buffer, &buffer, rate, 0..frames);
            println!(
                "{name} q{quality}: {kbps:.1} kbit/s, {snr:.1} dB ({tones:.1} dB on the tones), NMR {:.1} dB ({:.1}% audible), worst {worst:.3}, {:.0}x real time",
                masked.mean_db,
                100.0 * masked.audible,
                seconds / elapsed
            );
            if let Some(directory) = &dump {
                let path = std::path::Path::new(directory).join(format!("{name}-q{quality}.ogg"));
                std::fs::write(path, &bytes).unwrap();
            }
        }
    }
}

/// Writes the files of [`FIXTURES`] into `MANGUEBYTE_DUMP_DIR`, for FFmpeg
/// to decode: see `data/README.md`.
#[test]
#[ignore = "writes files for other software"]
fn dump_files_for_other_software() {
    let directory = std::env::var("MANGUEBYTE_DUMP_DIR").expect("MANGUEBYTE_DUMP_DIR");
    for (name, source_name, quality) in FIXTURES {
        let (buffer, rate) = source(source_name);
        let bytes = encode(&buffer, rate, &VorbisOptions::new(quality));
        std::fs::write(
            std::path::Path::new(&directory).join(format!("{name}.ogg")),
            bytes,
        )
        .unwrap();
    }
}
