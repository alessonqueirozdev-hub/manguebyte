//! Instruments: voices for [`Synth`](crate::Synth) that sound like the
//! instruments they are named after.
//!
//! | Voice | What it is | Presets |
//! |---|---|---|
//! | [`PluckedString`] | a plucked string by physical modelling | [nylon](StringParams::nylon_guitar) and [steel](StringParams::steel_guitar) guitar, [finger](StringParams::finger_bass) and [slap](StringParams::slap_bass) bass |
//! | [`FmVoice`] | four-operator frequency modulation | [electric piano](FmParams::electric_piano), [tubular bell](FmParams::tubular_bell), [FM bass](FmParams::fm_bass) |
//! | [`OrganVoice`] | a tonewheel organ: drawbars, percussion, key click | [jazz](OrganParams::jazz), [gospel](OrganParams::gospel), [full](OrganParams::full), [ballad](OrganParams::ballad), any [registration](OrganParams::registration) |
//! | [`DrumKitVoice`] | a drum kit whose piece on each key is chosen and tuned: acoustic drums and cymbals by modal synthesis, Brazilian percussion (agogô, gonguê, triangle, cuíca, surdo, zabumba, alfaia, tamborim, pandeiro, cajón, ganzá, hand clap) by physical modelling, and the drums of the 909, 606, CR-78 and 808 | [acoustic](DrumKit::acoustic), [Brazilian percussion](DrumKit::brazilian), [808](DrumKit::style_808), [909](DrumKit::style_909), [606](DrumKit::style_606), [CR-78](DrumKit::style_cr78), [trap](DrumKit::trap), [techno](DrumKit::techno), [house](DrumKit::house), [drum and bass](DrumKit::drum_and_bass), [lo-fi](DrumKit::lo_fi) |
//! | [`AnalogKitVoice`] | sixteen drums of a classic analogue drum machine, on the General MIDI keys | one [setting](DrumSettings) per drum |
//! | [`KickBassVoice`] | that machine's long kick played as a bass, with slides | [the classic](KickBassParams::default) |
//! | [`AcidVoice`] | the acid bass: an oscillator, a diode ladder and envelopes, with accent and slide | [classic](AcidParams::classic), [squelch](AcidParams::squelch), [square](AcidParams::square) |
//! | [`PolySynthVoice`] | a deep polyphonic synthesiser: three oscillators of classic shapes and wavetables with sixteen-voice unison, sync, FM and ring modulation, two filters of fifteen models, four envelopes, three LFOs and a modulation matrix | the [library](poly::PRESETS) of basses, leads, pads, plucks, keys, brass and effects |
//! | [`BowedString`] | a bowed string by digital waveguide, with the friction of rosin: Helmholtz motion, the bow's bite, a player's ear for intonation, drift and swell, vibrato that waits, legato as a change of finger, a body of air and wood resonances, sections, pizzicato | [violin](BowedParams::violin), [viola](BowedParams::viola), [cello](BowedParams::cello), [double bass](BowedParams::contrabass), [string section](BowedParams::string_ensemble), pizzicato [violin](BowedParams::violin_pizzicato), [cello](BowedParams::cello_pizzicato) and [bass](BowedParams::contrabass_pizzicato) |
//! | [`WindVoice`] | wind instruments by digital waveguide: a reed, the lips or an air jet driving a bore, with breath control and breath noise, tonguing, a player's ear, drift and swell, vibrato, and legato that changes notes as keys and valves do | [flute](WindParams::flute), [clarinet](WindParams::clarinet), [saxophone](WindParams::saxophone), [trumpet](WindParams::trumpet), [trombone](WindParams::trombone) |
//! | [`MalletVoice`] | struck bars, tines, plates and tubes by modal synthesis: mallet hardness, strike position, resonator tubes, the vibraphone's motor and damper | [marimba](MalletParams::marimba), [xylophone](MalletParams::xylophone), [vibraphone](MalletParams::vibraphone), [glockenspiel](MalletParams::glockenspiel), [celesta](MalletParams::celesta), [kalimba](MalletParams::kalimba), [steel pan](MalletParams::steel_pan), [tubular bells](MalletParams::tubular_bells) |
//! | [`ChoirVoice`] | sung vowels and choirs by formant synthesis: a glottal pulse through five formants, from basses to sopranos, several detuned singers to a note | ["aah" choir](ChoirParams::aah_choir), ["ooh" choir](ChoirParams::ooh_choir), [solo voice](ChoirParams::solo_voice) |
//!
//! [`RotarySpeaker`] is not a voice but a [`Processor`](crate::Processor)
//! to put after the organ: the rotating horn and drum it is usually heard
//! through.
//!
//! Each voice is built from the crate's own parts (delay lines, filters,
//! oscillators) and has a settings struct with presets. Give a
//! [`Synth`](crate::Synth) as many voices as notes should sound at once:
//!
//! ```
//! use manguebyte_core::AudioBuffer;
//! use manguebyte_dsp::event::NoteRef;
//! use manguebyte_dsp::instruments::{PluckedString, StringParams};
//! use manguebyte_dsp::{ProcessSpec, Processor, Synth};
//!
//! let mut bass = Synth::with_voices(4, || PluckedString::new(StringParams::finger_bass()));
//! bass.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 128, num_channels: 1 });
//! bass.note_on_accented(NoteRef::new(0, 28), 0.9, true);
//!
//! let mut block = AudioBuffer::<f32>::new(1, 128);
//! bass.process(&mut block);
//! assert!(block.peak() > 0.0);
//! ```
//!
//! # Velocity and accent
//!
//! Every voice answers velocity with level and timbre: a note played
//! harder is louder and brighter, as on the instrument it models, by
//! amounts its settings say.
//!
//! An accent reaches a voice as an argument of the note that carries it,
//! through [`Synth::note_on_accented`](crate::Synth::note_on_accented) and
//! [`Voice::note_on_accented`](crate::Voice::note_on_accented). The other
//! way to do it, a flag the caller sets on every voice before each note
//! because only the synth knows which voice will take the note, makes the
//! accent a property of the voice instead of the note: it has to be cleared
//! again, and a note started any other way (from the event stream, or by a
//! voice taken over later) inherits whatever was set last. As an argument
//! it cannot outlive its note. The methods have defaults, so a voice that
//! knows nothing of accents is unaffected.
//!
//! What an accent does is the instrument's business, and each settings
//! struct says: usually louder and brighter, and for some more than that
//! (a pop on the slapped bass).
//!
//! # Taking a voice over
//!
//! When every voice is busy, the synth gives a new note the voice that
//! will be missed least and chokes it first. A polyphonic voice here keeps
//! two copies of its sound: the note it was playing moves aside and fades
//! out over [`HANDOVER_SECONDS`], alongside the new note, so the old note
//! does not stop with a click. A voice that is silent when it is choked
//! returns to the state it had when new, as the
//! [`Voice::choke`](crate::Voice::choke) contract asks, so the same notes
//! give the same output however the timeline is cut into blocks.
//!
//! The two monophonic voices, [`KickBassVoice`] and [`AcidVoice`], behave
//! as the single-oscillator instruments they model: a note that takes a
//! sounding voice over carries on from its oscillator, filter and
//! envelopes, rising from where they are, instead of fading the old note
//! out. Give them a synth of one voice, and turn
//! [legato](crate::Synth::set_legato) on for slides.
//!
//! # Settings
//!
//! Every voice takes its settings whole, through a `set_params` method,
//! which is real-time safe: call it on every voice through
//! [`Synth::voices_mut`](crate::Synth::voices_mut). Values out of range are
//! limited, and a value that is not a number takes the preset's default,
//! so no setting can make a voice produce anything but finite numbers.
//!
//! # Determinism
//!
//! Every source of noise is seeded by the note it belongs to, so a note
//! sounds the same every time it is played, whatever was played before it.
//! The one exception is an acoustic piece of a [`DrumKitVoice`] struck
//! again while it rings: like the drum or cymbal it models, it adds the new
//! strike to what is still ringing, and the stick lands a little
//! differently each time. The same notes still give the same samples.

mod acid;
mod acoustic;
mod analog_drums;
mod bowed;
mod brazilian;
mod choir;
mod common;
mod fm;
mod kit;
mod machines;
mod mallet;
#[cfg(test)]
pub(crate) mod measure;
mod modal;
mod modelling;
mod organ;
pub mod poly;
mod rotary;
mod string;
#[cfg(test)]
mod tuned;
mod wind;

pub use acid::{AcidParams, AcidVoice, AcidWave};
pub use analog_drums::{
    AnalogDrum, AnalogKitParams, AnalogKitVoice, ChokeGroup, DrumSettings, KickBassParams,
    KickBassVoice, METAL_HZ,
};
pub use bowed::{BODY_MODES, BowedParams, BowedString, MAX_PLAYERS, WOOD_MODES};
pub use choir::{ChoirParams, ChoirVoice, FORMANTS, MAX_SINGERS, Vowel};
pub use common::HANDOVER_SECONDS;
pub use fm::{Algorithm, FM_OPERATORS, FM_VELOCITY_RANGE_DB, FmOperator, FmParams, FmVoice};
pub use kit::{
    CHOKE_GROUPS, CHOKE_SECONDS, ChokeGroups, DrumKit, DrumKitVoice, DrumModel, KIT_FIRST_KEY,
    KIT_KEYS, LoFi, Piece, gm, percussion,
};
pub use mallet::{MALLET_MODES, MalletMode, MalletParams, MalletShape, MalletVoice};
pub use organ::{
    DRAWBAR_FOOTAGES, DRAWBARS, OrganManual, OrganParams, OrganVoice, Percussion, PercussionDecay,
    PercussionHarmonic,
};
pub use poly::{PolySynthParams, PolySynthVoice};
pub use rotary::{RotaryParams, RotarySpeaker, RotarySpeed};
pub use string::{BodyMode, PluckedString, StringParams};
pub use wind::{WindKind, WindParams, WindVoice};
