//! A check that the sampler runs in WebAssembly, with no file system and no
//! threads.
//!
//! Built as a library for `wasm32-unknown-unknown`:
//!
//! ```text
//! cargo build -p manguebyte-sampler --example wasm_smoke --target wasm32-unknown-unknown
//! ```
//!
//! it exports `smoke_frequency(key)`, which writes a WAV file of A4 into
//! memory, loads an SFZ instrument from it, plays `key` for a second at
//! 48 kHz and returns the frequency it measures in the output, and
//! `smoke_hostile_wav()`, which loads a WAV file whose chunk sizes would
//! overflow 32-bit offsets.
//! Any JavaScript engine can call it, for example Node.js:
//!
//! ```text
//! const bytes = fs.readFileSync("target/wasm32-unknown-unknown/debug/examples/wasm_smoke.wasm");
//! const { instance } = await WebAssembly.instantiate(bytes, {});
//! instance.exports.smoke_frequency(81); // about 880
//! ```

use manguebyte_core::AudioBuffer;
use manguebyte_dsp::event::NoteRef;
use manguebyte_dsp::{ProcessSpec, Processor};
use manguebyte_formats::wav::{self, WavFormat};
use manguebyte_sampler::{Instrument, MemorySource, Sampler};

/// Plays `key` on an instrument whose only sample is A4 (key 69) and
/// returns the frequency of the result in hertz, or a negative number if
/// something failed: -1 writing the sample, -2 loading the instrument, -3
/// for a silent output.
// SAFETY: the exported name is unique in this example, which is a small
// library of its own; nothing else in the module defines a symbol of that
// name.
#[unsafe(no_mangle)]
pub extern "C" fn smoke_frequency(key: u32) -> f64 {
    let rate = 44_100;
    let mut tone = AudioBuffer::<f32>::new(1, rate);
    for (n, sample) in tone.channel_mut(0).iter_mut().enumerate() {
        *sample = (0.5 * (core::f64::consts::TAU * 440.0 * n as f64 / rate as f64).sin()) as f32;
    }
    let mut bytes = Vec::new();
    if wav::write(&mut bytes, &tone, rate as u32, WavFormat::Float32).is_err() {
        return -1.0;
    }
    let mut source = MemorySource::new().with("a4.wav", bytes);
    let Ok(instrument) =
        Instrument::from_sfz("<region> sample=a4.wav pitch_keycenter=69", &mut source)
    else {
        return -2.0;
    };
    let (mut sampler, _loader) = Sampler::<f32>::with_instrument(instrument, 4);
    sampler.prepare(&ProcessSpec {
        sample_rate: 48_000.0,
        max_block_size: 48_000,
        num_channels: 1,
    });
    sampler.note_on(NoteRef::new(0, key.min(127) as u8), 1.0);
    let mut output = AudioBuffer::<f32>::new(1, 9_600);
    sampler.process(&mut output);
    // Rising zero crossings, placed between frames by linear interpolation.
    let signal = output.channel(0);
    let mut first = None;
    let mut last = 0.0;
    let mut count = 0usize;
    for n in 1..signal.len() {
        let (a, b) = (f64::from(signal[n - 1]), f64::from(signal[n]));
        if a < 0.0 && b >= 0.0 {
            let at = (n - 1) as f64 + a / (a - b);
            first.get_or_insert(at);
            last = at;
            count += 1;
        }
    }
    match first {
        Some(first) if count > 2 => (count - 1) as f64 * 48_000.0 / (last - first),
        _ => -3.0,
    }
}

/// Loads a WAV file whose data chunk claims 0xFFFFFFD0 bytes, so that the
/// walk over its chunks reaches an offset four bytes short of 2^32, where
/// 32-bit index arithmetic overflows. Returns the bytes of audio loaded
/// (16: four mono frames as 32-bit floats), or -1 if loading failed. Built
/// without `--release`, an overflow would trap instead.
// SAFETY: as for `smoke_frequency`, the name is unique in this module.
#[unsafe(no_mangle)]
pub extern "C" fn smoke_hostile_wav() -> f64 {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&u32::MAX.to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    for half in [1u16, 1] {
        bytes.extend_from_slice(&half.to_le_bytes());
    }
    bytes.extend_from_slice(&48_000u32.to_le_bytes());
    bytes.extend_from_slice(&96_000u32.to_le_bytes());
    for half in [2u16, 16] {
        bytes.extend_from_slice(&half.to_le_bytes());
    }
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&0xFFFF_FFD0u32.to_le_bytes());
    for sample in [0i16, 1_000, 2_000, 1_000] {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    let mut source = MemorySource::new().with("h.wav", bytes);
    match Instrument::from_sfz("<region> sample=h.wav", &mut source) {
        Ok(instrument) => instrument.sample_bytes() as f64,
        Err(_) => -1.0,
    }
}
