//! How many channels each element of a unit has, and what a unit says it
//! can take (`kAudioUnitProperty_SupportedNumChannels`,
//! `kAudioUnitProperty_StreamFormat`, `kAudioUnitProperty_ElementCount`,
//! all in `AudioUnitProperties.h`).

use super::cf;
use super::sys::*;
use super::unit::Unit;

/// Most input elements read from one unit: the main input and the
/// sidechain are all this host connects.
pub(crate) const MAX_INPUT_ELEMENTS: u32 = 2;

/// Most channels on one element.
pub(crate) const MAX_CHANNELS: u32 = 64;

/// One of a unit's audio elements, which other formats call a bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuBus {
    /// The element's number in its scope.
    pub element: u32,
    /// The name the unit gives it (`kAudioUnitProperty_ElementName`), or
    /// one this host makes up.
    pub name: String,
    /// How many channels it has, as its stream format says.
    pub channels: u32,
}

/// The format this host exchanges audio in: 32-bit floating point, one
/// buffer per channel, which is the canonical format of an Audio Unit
/// (`CoreAudioBaseTypes.h`, `kAudioFormatFlagsNativeFloatPacked` with
/// `kAudioFormatFlagIsNonInterleaved`).
pub(crate) fn float_format(sample_rate: f64, channels: u32) -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        mSampleRate: sample_rate,
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat
            | kAudioFormatFlagIsPacked
            | kAudioFormatFlagIsNonInterleaved
            | if cfg!(target_endian = "big") {
                kAudioFormatFlagIsBigEndian
            } else {
                0
            },
        mBytesPerPacket: 4,
        mFramesPerPacket: 1,
        mBytesPerFrame: 4,
        mChannelsPerFrame: channels,
        mBitsPerChannel: 32,
        mReserved: 0,
    }
}

/// Whether one side of an `AUChannelInfo` allows `channels`. As the header
/// says: a number at or above 0 is that many channels; -1 and -2 are any
/// number; a number below -2 is up to that many.
fn side_allows(given: i16, channels: u32) -> bool {
    match given {
        0.. => u32::from(given.unsigned_abs()) == channels,
        -2 | -1 => true,
        _ => channels <= u32::from(given.unsigned_abs()),
    }
}

/// Whether one configuration a unit lists allows `inputs` and `outputs`:
/// `{-1, -1}` means any number on both sides as long as they are the same.
pub(crate) fn allows(info: AUChannelInfo, inputs: u32, outputs: u32) -> bool {
    if info.inChannels == -1 && info.outChannels == -1 {
        return inputs == outputs;
    }
    side_allows(info.inChannels, inputs) && side_allows(info.outChannels, outputs)
}

/// Whether a unit that lists `supported` takes `inputs` and `outputs`. A
/// unit that lists nothing leaves it to its stream formats to say.
pub(crate) fn supported(supported: &[AUChannelInfo], inputs: u32, outputs: u32) -> bool {
    supported.is_empty() || supported.iter().any(|&info| allows(info, inputs, outputs))
}

/// How many elements a scope has.
pub(crate) fn element_count(unit: &Unit, scope: AudioUnitScope) -> u32 {
    unit.get::<u32>(kAudioUnitProperty_ElementCount, scope, 0)
        .unwrap_or(0)
}

/// How many channels an element has now.
pub(crate) fn channels_of(unit: &Unit, scope: AudioUnitScope, element: u32) -> Option<u32> {
    let format: AudioStreamBasicDescription = unit
        .get(kAudioUnitProperty_StreamFormat, scope, element)
        .ok()?;
    (format.mChannelsPerFrame <= MAX_CHANNELS).then_some(format.mChannelsPerFrame)
}

/// The name of an element, or `otherwise`.
pub(crate) fn element_name(
    unit: &Unit,
    scope: AudioUnitScope,
    element: u32,
    otherwise: &str,
) -> String {
    let name = unit
        .get::<CFStringRef>(kAudioUnitProperty_ElementName, scope, element)
        .ok()
        // SAFETY: the unit hands over a string the caller releases.
        .and_then(|name| unsafe { cf::take_string(name) })
        .filter(|name| !name.trim().is_empty());
    name.unwrap_or_else(|| otherwise.to_owned())
}

/// The input elements, main input first, then the sidechain; and the
/// output element.
pub(crate) fn read_buses(unit: &Unit) -> (Vec<AuBus>, Vec<AuBus>) {
    let count = element_count(unit, kAudioUnitScope_Input).min(MAX_INPUT_ELEMENTS);
    let mut inputs = Vec::new();
    for element in 0..count {
        let Some(channels) = channels_of(unit, kAudioUnitScope_Input, element) else {
            continue;
        };
        let otherwise = if element == 0 { "Input" } else { "Sidechain" };
        inputs.push(AuBus {
            element,
            name: element_name(unit, kAudioUnitScope_Input, element, otherwise),
            channels,
        });
    }
    let outputs = channels_of(unit, kAudioUnitScope_Output, 0)
        .map(|channels| AuBus {
            element: 0,
            name: element_name(unit, kAudioUnitScope_Output, 0, "Output"),
            channels,
        })
        .into_iter()
        .collect();
    (inputs, outputs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(inputs: i16, outputs: i16) -> AUChannelInfo {
        AUChannelInfo {
            inChannels: inputs,
            outChannels: outputs,
        }
    }

    #[test]
    fn the_channel_configurations_read_as_the_header_says() {
        // Any number, the same on both sides.
        assert!(allows(info(-1, -1), 3, 3));
        assert!(!allows(info(-1, -1), 1, 2));
        // Any number on each side.
        assert!(allows(info(-1, -2), 1, 6));
        assert!(allows(info(-2, -1), 6, 1));
        // Any input, two outputs.
        assert!(allows(info(-1, 2), 5, 2));
        assert!(!allows(info(-1, 2), 2, 1));
        // Exact counts, and an instrument.
        assert!(allows(info(2, 2), 2, 2));
        assert!(!allows(info(2, 2), 1, 1));
        assert!(allows(info(0, 2), 0, 2));
        assert!(!allows(info(0, 2), 2, 2));
        // Up to eight.
        assert!(allows(info(-8, -8), 6, 2));
        assert!(!allows(info(-8, -8), 9, 2));

        let list = [info(1, 1), info(2, 2)];
        assert!(supported(&list, 2, 2));
        assert!(!supported(&list, 1, 2));
        assert!(supported(&[], 7, 3), "a unit that lists nothing");
    }

    #[test]
    fn the_format_is_canonical_float() {
        let format = float_format(48_000.0, 2);
        assert_eq!(format.mChannelsPerFrame, 2);
        assert_eq!(format.mBitsPerChannel, 32);
        assert_ne!(format.mFormatFlags & kAudioFormatFlagIsNonInterleaved, 0);
        assert_ne!(format.mFormatFlags & kAudioFormatFlagIsFloat, 0);
    }
}
