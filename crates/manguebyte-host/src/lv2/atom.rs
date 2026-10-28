//! Writing the atoms a plugin reads: sequences of MIDI events, and the
//! `time:Position` objects that tell it where the transport is; and
//! reading the sequences a plugin writes.
//!
//! Written from the LV2 atom specification. Every atom is a header, its
//! size and type, followed by a body; in a sequence each event's body is
//! padded to a multiple of 8 bytes, and the buffer itself is 8-byte
//! aligned. Nothing here allocates: an [`AtomBuffer`] is allocated once,
//! and writing into it either fits or is refused.

use std::ffi::c_void;

use manguebyte_dsp::Transport;

use super::features::map;
use super::sys::*;

/// The URIDs the processor writes with, mapped once.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Urids {
    pub(crate) sequence: u32,
    pub(crate) chunk: u32,
    pub(crate) frame_time: u32,
    pub(crate) object: u32,
    pub(crate) int: u32,
    pub(crate) long: u32,
    pub(crate) float: u32,
    pub(crate) double: u32,
    pub(crate) midi_event: u32,
    pub(crate) position: u32,
    pub(crate) frame: u32,
    pub(crate) speed: u32,
    pub(crate) beats_per_minute: u32,
    pub(crate) beats_per_bar: u32,
    pub(crate) beat_unit: u32,
    pub(crate) bar_beat: u32,
    pub(crate) beat: u32,
}

impl Urids {
    pub(crate) fn new() -> Self {
        Self {
            sequence: map(LV2_ATOM__Sequence),
            chunk: map(LV2_ATOM__Chunk),
            frame_time: map(LV2_ATOM__frameTime),
            object: map(LV2_ATOM__Object),
            int: map(LV2_ATOM__Int),
            long: map(LV2_ATOM__Long),
            float: map(LV2_ATOM__Float),
            double: map(LV2_ATOM__Double),
            midi_event: map(LV2_MIDI__MidiEvent),
            position: map(LV2_TIME__Position),
            frame: map(LV2_TIME__frame),
            speed: map(LV2_TIME__speed),
            beats_per_minute: map(LV2_TIME__beatsPerMinute),
            beats_per_bar: map(LV2_TIME__beatsPerBar),
            beat_unit: map(LV2_TIME__beatUnit),
            bar_beat: map(LV2_TIME__barBeat),
            beat: map(LV2_TIME__beat),
        }
    }
}

const HEADER: usize = size_of::<LV2_Atom>();

/// An 8-byte aligned buffer for one atom port.
pub(crate) struct AtomBuffer {
    words: Box<[u64]>,
}

impl AtomBuffer {
    /// A buffer of at least `bytes` bytes, and at least room for an empty
    /// sequence.
    pub(crate) fn new(bytes: usize) -> Self {
        let words = bytes.max(2 * HEADER).div_ceil(8);
        Self {
            words: vec![0u64; words].into_boxed_slice(),
        }
    }

    /// Where the plugin is to find the buffer. Taken fresh before every
    /// call, so that no pointer the plugin holds outlives a borrow.
    pub(crate) fn as_mut_ptr(&mut self) -> *mut c_void {
        self.words.as_mut_ptr().cast()
    }

    fn bytes(&self) -> &[u8] {
        let length = self.words.len() * 8;
        // SAFETY: the words are `length` bytes of initialised memory, and
        // every byte value is a valid `u8`.
        unsafe { std::slice::from_raw_parts(self.words.as_ptr().cast::<u8>(), length) }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        let length = self.words.len() * 8;
        // SAFETY: the words are `length` bytes of initialised memory that
        // this borrow of `self` holds alone, and every byte value is a
        // valid `u8`.
        unsafe { std::slice::from_raw_parts_mut(self.words.as_mut_ptr().cast::<u8>(), length) }
    }

    /// Readies the buffer for the plugin to write into: the header says
    /// how much room there is, as LV2 has a host do for an output.
    pub(crate) fn prepare_output(&mut self, urids: &Urids) {
        let room = (self.words.len() * 8 - HEADER) as u32;
        let bytes = self.bytes_mut();
        bytes[..4].copy_from_slice(&room.to_ne_bytes());
        bytes[4..8].copy_from_slice(&urids.chunk.to_ne_bytes());
    }

    /// Reads what a plugin wrote into an output readied with
    /// [`prepare_output`](Self::prepare_output): calls `each` with the
    /// frame, type and body of every event, in order, if the plugin wrote
    /// a sequence stamped in frames.
    ///
    /// The plugin's bytes are not trusted. A buffer it left as it was given
    /// (a chunk), or filled with another atom, or a sequence stamped in
    /// beats, holds no events; a sequence that claims more bytes than the
    /// buffer has is read as far as it lies within it, and an event whose
    /// body runs past that ends it.
    pub(crate) fn read_events(&self, urids: &Urids, mut each: impl FnMut(i64, u32, &[u8])) {
        let bytes = self.bytes();
        let word = |at: usize| {
            u32::from_ne_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        // An `AtomBuffer` always has room for an empty sequence.
        if word(4) != urids.sequence {
            return;
        }
        // LV2 has a unit of 0 mean the port's default, which is frames.
        let unit = word(8);
        if unit != urids.frame_time && unit != 0 {
            return;
        }
        let end = (word(0) as usize).saturating_add(HEADER).min(bytes.len());
        let mut at = 2 * HEADER;
        while at + 2 * HEADER <= end {
            let mut frame = [0u8; 8];
            frame.copy_from_slice(&bytes[at..at + 8]);
            let frame = i64::from_ne_bytes(frame);
            let size = word(at + 8) as usize;
            let type_ = word(at + 12);
            let body = at + 2 * HEADER;
            let Some(body_end) = body.checked_add(size).filter(|&body_end| body_end <= end) else {
                return;
            };
            each(frame, type_, &bytes[body..body_end]);
            at += (2 * HEADER + size).next_multiple_of(8);
        }
    }

    /// Starts writing a sequence of events stamped in frames.
    pub(crate) fn sequence(&mut self, urids: &Urids) -> Sequence<'_> {
        let mut sequence = Sequence {
            bytes: self.bytes_mut(),
            used: 2 * HEADER,
        };
        sequence.bytes[4..8].copy_from_slice(&urids.sequence.to_ne_bytes());
        sequence.bytes[8..12].copy_from_slice(&urids.frame_time.to_ne_bytes());
        sequence.bytes[12..16].copy_from_slice(&0u32.to_ne_bytes());
        sequence.finish_header();
        sequence
    }
}

/// A sequence being written. The header always gives the size written so
/// far, so the sequence is whole at every moment.
pub(crate) struct Sequence<'a> {
    bytes: &'a mut [u8],
    used: usize,
}

/// One property of an object: its key, its type and the bytes of its body.
#[derive(Debug, Clone, Copy)]
struct Property {
    key: u32,
    type_: u32,
    body: [u8; 8],
    size: usize,
}

impl Sequence<'_> {
    fn finish_header(&mut self) {
        let size = (self.used - HEADER) as u32;
        self.bytes[..4].copy_from_slice(&size.to_ne_bytes());
    }

    fn put(&mut self, bytes: &[u8]) {
        self.bytes[self.used..self.used + bytes.len()].copy_from_slice(bytes);
        self.used += bytes.len();
    }

    fn pad(&mut self) {
        let padded = self.used.next_multiple_of(8);
        self.bytes[self.used..padded].fill(0);
        self.used = padded;
    }

    /// Adds an event of `body_size` bytes, written by `body`. Returns
    /// whether there was room.
    fn event(
        &mut self,
        frame: u32,
        type_: u32,
        body_size: usize,
        body: impl FnOnce(&mut Self),
    ) -> bool {
        let needed = (2 * HEADER + body_size).next_multiple_of(8);
        if self.bytes.len() - self.used < needed {
            return false;
        }
        self.put(&i64::from(frame).to_ne_bytes());
        self.put(&(body_size as u32).to_ne_bytes());
        self.put(&type_.to_ne_bytes());
        let start = self.used;
        body(self);
        debug_assert_eq!(self.used - start, body_size);
        self.pad();
        self.finish_header();
        true
    }

    /// Adds one MIDI message on `frame`. Returns whether there was room.
    pub(crate) fn midi(&mut self, frame: u32, message: &[u8], urids: &Urids) -> bool {
        self.event(frame, urids.midi_event, message.len(), |sequence| {
            sequence.put(message);
        })
    }

    /// Adds a `time:Position` on `frame` that describes `transport`, with
    /// the types hosts commonly give each property: frames as a long,
    /// speed, tempo, beats per bar and the beat within the bar as floats,
    /// the beat unit as an int, and the running beat as a double.
    ///
    /// LV2 counts beats in notes of `time:beatUnit`, and the framework in
    /// quarter notes, so tempo and beats are scaled by the beat unit over
    /// 4. Returns whether there was room.
    pub(crate) fn position(&mut self, frame: u32, transport: &Transport, urids: &Urids) -> bool {
        let unit = transport.time_signature.map_or(4, |(_, unit)| unit.max(1));
        let scale = f64::from(unit) / 4.0;
        let mut properties = [Property {
            key: 0,
            type_: 0,
            body: [0; 8],
            size: 0,
        }; 7];
        let mut count = 0;
        let mut add = |key: u32, type_: u32, body: &[u8]| {
            let mut bytes = [0u8; 8];
            bytes[..body.len()].copy_from_slice(body);
            properties[count] = Property {
                key,
                type_,
                body: bytes,
                size: body.len(),
            };
            count += 1;
        };
        let speed: f32 = if transport.playing { 1.0 } else { 0.0 };
        add(urids.speed, urids.float, &speed.to_ne_bytes());
        if let Some(frames) = transport.position_samples {
            add(urids.frame, urids.long, &frames.to_ne_bytes());
        }
        if let Some(tempo) = transport.tempo_bpm {
            add(
                urids.beats_per_minute,
                urids.float,
                &((tempo * scale) as f32).to_ne_bytes(),
            );
        }
        if let Some((beats, unit)) = transport.time_signature {
            add(
                urids.beats_per_bar,
                urids.float,
                &f32::from(beats).to_ne_bytes(),
            );
            add(urids.beat_unit, urids.int, &i32::from(unit).to_ne_bytes());
        }
        if let Some(beat) = transport.position_beats {
            add(urids.beat, urids.double, &(beat * scale).to_ne_bytes());
            if let Some(bar) = transport.bar_start_beats {
                let in_bar = ((beat - bar) * scale) as f32;
                add(urids.bar_beat, urids.float, &in_bar.to_ne_bytes());
            }
        }
        let properties = &properties[..count];
        // The object's body, then each property's key, context, value
        // header and value padded to 8 bytes.
        let size = HEADER
            + properties
                .iter()
                .map(|property| (2 * HEADER + property.size).next_multiple_of(8))
                .sum::<usize>();
        self.event(frame, urids.object, size, |sequence| {
            sequence.put(&0u32.to_ne_bytes());
            sequence.put(&urids.position.to_ne_bytes());
            // Every value is padded, the last one too, and the object's size
            // counts the padding, as LV2's own atom forge writes objects.
            for property in properties {
                sequence.put(&property.key.to_ne_bytes());
                sequence.put(&0u32.to_ne_bytes());
                sequence.put(&(property.size as u32).to_ne_bytes());
                sequence.put(&property.type_.to_ne_bytes());
                sequence.put(&property.body[..property.size]);
                sequence.pad();
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(bytes: &[u8], at: usize) -> u32 {
        u32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn a_sequence_is_whole_after_every_event_and_refuses_what_does_not_fit() {
        let urids = Urids::new();
        let mut buffer = AtomBuffer::new(64);
        {
            let mut sequence = buffer.sequence(&urids);
            assert!(sequence.midi(3, &[0x90, 60, 100], &urids));
            assert!(sequence.midi(7, &[0x80, 60, 0], &urids));
            // 16 bytes of header, two events of 24: no room for a third.
            assert!(!sequence.midi(9, &[0xB0, 1, 2], &urids));
        }
        let bytes = buffer.bytes_mut().to_vec();
        assert_eq!(word(&bytes, 0), 8 + 2 * 24);
        assert_eq!(word(&bytes, 4), urids.sequence);
        assert_eq!(word(&bytes, 8), urids.frame_time);
        assert_eq!(i64::from_ne_bytes(bytes[16..24].try_into().unwrap()), 3);
        assert_eq!(word(&bytes, 24), 3);
        assert_eq!(word(&bytes, 28), urids.midi_event);
        assert_eq!(&bytes[32..35], &[0x90, 60, 100]);
        assert_eq!(i64::from_ne_bytes(bytes[40..48].try_into().unwrap()), 7);

        buffer.prepare_output(&urids);
        let bytes = buffer.bytes_mut().to_vec();
        assert_eq!(word(&bytes, 0) as usize, bytes.len() - 8);
        assert_eq!(word(&bytes, 4), urids.chunk);
    }

    fn read(buffer: &AtomBuffer, urids: &Urids) -> Vec<(i64, u32, Vec<u8>)> {
        let mut found = Vec::new();
        buffer.read_events(urids, |frame, type_, body| {
            found.push((frame, type_, body.to_vec()));
        });
        found
    }

    #[test]
    fn a_sequence_written_is_read_back_and_a_bad_one_as_far_as_it_is_whole() {
        let urids = Urids::new();
        let mut buffer = AtomBuffer::new(64);
        // Left as the host readied it: nothing.
        buffer.prepare_output(&urids);
        assert!(read(&buffer, &urids).is_empty());

        {
            let mut sequence = buffer.sequence(&urids);
            assert!(sequence.midi(3, &[0x90, 60, 100], &urids));
            assert!(sequence.midi(7, &[0xC0, 5], &urids));
        }
        let midi = urids.midi_event;
        assert_eq!(
            read(&buffer, &urids),
            [(3, midi, vec![0x90, 60, 100]), (7, midi, vec![0xC0, 5])]
        );

        // A sequence that claims more than the buffer holds is read as far
        // as the buffer goes.
        buffer.bytes_mut()[..4].copy_from_slice(&u32::MAX.to_ne_bytes());
        assert_eq!(read(&buffer, &urids).len(), 2);
        // An event whose body runs past the sequence ends it.
        buffer.bytes_mut()[..4].copy_from_slice(&(8u32 + 24 + 16).to_ne_bytes());
        assert_eq!(read(&buffer, &urids).len(), 1);
        // A sequence stamped in beats holds no frames.
        buffer.bytes_mut()[..4].copy_from_slice(&(8u32 + 48).to_ne_bytes());
        let beats = map(LV2_ATOM__beatTime);
        buffer.bytes_mut()[8..12].copy_from_slice(&beats.to_ne_bytes());
        assert!(read(&buffer, &urids).is_empty());
        // A unit of 0 is the port's default: frames.
        buffer.bytes_mut()[8..12].copy_from_slice(&0u32.to_ne_bytes());
        assert_eq!(read(&buffer, &urids).len(), 2);
    }

    #[test]
    fn a_position_lays_out_every_property() {
        let urids = Urids::new();
        let mut buffer = AtomBuffer::new(1_024);
        let mut transport = Transport::playing_at(120.0);
        transport.time_signature = Some((6, 8));
        transport.position_samples = Some(48_000);
        transport.position_beats = Some(5.0);
        transport.bar_start_beats = Some(3.0);
        assert!(buffer.sequence(&urids).position(0, &transport, &urids));
        let bytes = buffer.bytes_mut().to_vec();
        // Header, sequence body, event header.
        let body_size = word(&bytes, 24) as usize;
        assert_eq!(word(&bytes, 28), urids.object);
        assert_eq!(
            word(&bytes, 0) as usize,
            8 + 16 + body_size.next_multiple_of(8)
        );
        assert_eq!(word(&bytes, 36), urids.position);
        let mut at = 40;
        let mut found = Vec::new();
        while at < 32 + body_size {
            let (key, size, type_) = (
                word(&bytes, at),
                word(&bytes, at + 8),
                word(&bytes, at + 12),
            );
            let value = &bytes[at + 16..at + 16 + size as usize];
            let number = if type_ == urids.float {
                f64::from(f32::from_ne_bytes(value.try_into().unwrap()))
            } else if type_ == urids.double {
                f64::from_ne_bytes(value.try_into().unwrap())
            } else if type_ == urids.long {
                i64::from_ne_bytes(value.try_into().unwrap()) as f64
            } else {
                f64::from(i32::from_ne_bytes(value.try_into().unwrap()))
            };
            found.push((key, number));
            at += (16 + size as usize).next_multiple_of(8);
        }
        assert_eq!(
            found,
            [
                (urids.speed, 1.0),
                (urids.frame, 48_000.0),
                // Eighth notes: twice the quarter notes.
                (urids.beats_per_minute, 240.0),
                (urids.beats_per_bar, 6.0),
                (urids.beat_unit, 8.0),
                (urids.beat, 10.0),
                (urids.bar_beat, 4.0),
            ]
        );
    }
}
