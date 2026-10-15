use manguebyte_core::{AudioBuffer, Sample};

use crate::event::{Event, EventBuffer};

/// Where the music is and how fast it is moving, at the start of a block.
///
/// A host fills in what it knows; a field it cannot provide is `None`. A
/// standalone program that keeps its own time can drive one with
/// [`advance`](Self::advance).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Transport {
    /// Whether the timeline is moving.
    pub playing: bool,
    /// Whether the host is recording.
    pub recording: bool,
    /// Tempo in quarter notes per minute.
    pub tempo_bpm: Option<f64>,
    /// Time signature as beats per bar and the note value of a beat, so
    /// `(6, 8)` is six eighth notes.
    pub time_signature: Option<(u16, u16)>,
    /// Position of the block's first frame, in frames from the start of the
    /// timeline. Negative during a count-in.
    pub position_samples: Option<i64>,
    /// Position of the block's first frame, in quarter notes.
    pub position_beats: Option<f64>,
    /// Position of the start of the current bar, in quarter notes.
    pub bar_start_beats: Option<f64>,
    /// The looped region, in quarter notes, when looping is on.
    pub loop_beats: Option<(f64, f64)>,
}

impl Transport {
    /// A transport that is stopped and knows nothing else.
    pub const STOPPED: Self = Self {
        playing: false,
        recording: false,
        tempo_bpm: None,
        time_signature: None,
        position_samples: None,
        position_beats: None,
        bar_start_beats: None,
        loop_beats: None,
    };

    /// A transport playing from the start at `tempo_bpm`, for programs that
    /// keep their own time.
    pub fn playing_at(tempo_bpm: f64) -> Self {
        Self {
            playing: true,
            tempo_bpm: Some(tempo_bpm),
            time_signature: Some((4, 4)),
            position_samples: Some(0),
            position_beats: Some(0.0),
            bar_start_beats: Some(0.0),
            ..Self::STOPPED
        }
    }

    /// How many quarter notes pass per frame, if the tempo is known.
    pub fn beats_per_sample(&self, sample_rate: f64) -> Option<f64> {
        self.tempo_bpm.map(|tempo| tempo / (60.0 * sample_rate))
    }

    /// The position, in quarter notes, of the frame `offset` frames into the
    /// block. This is what a tempo-synced LFO or delay reads.
    pub fn beats_at(&self, offset: usize, sample_rate: f64) -> Option<f64> {
        let position = self.position_beats?;
        if !self.playing {
            return Some(position);
        }
        Some(position + offset as f64 * self.beats_per_sample(sample_rate)?)
    }

    /// Moves the position on by `frames`, as a host does between blocks.
    /// Does nothing while stopped. Real-time safe.
    pub fn advance(&mut self, frames: usize, sample_rate: f64) {
        if !self.playing {
            return;
        }
        if let Some(position) = &mut self.position_samples {
            let frames = i64::try_from(frames).unwrap_or(i64::MAX);
            *position = position.saturating_add(frames);
        }
        let Some(step) = self.beats_per_sample(sample_rate) else {
            return;
        };
        let Some(mut beats) = self.position_beats else {
            return;
        };
        beats += frames as f64 * step;
        if let Some((start, end)) = self.loop_beats {
            if end > start && beats >= end {
                beats = start + (beats - start) % (end - start);
            }
        }
        self.position_beats = Some(beats);

        if let Some((numerator, denominator)) = self.time_signature {
            let bar = f64::from(numerator) * 4.0 / f64::from(denominator.max(1));
            if bar > 0.0 {
                self.bar_start_beats = Some((beats / bar).floor() * bar);
            }
        }
    }
}

static NO_EVENTS: EventBuffer = EventBuffer::new();

/// Everything a processor is told about a block besides the audio itself.
///
/// The audio is processed in place in the buffer passed alongside. The
/// buffer has the channel count of the output; a processor with fewer input
/// channels than output channels finds its input in the first channels.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct ProcessContext<'a, S: Sample = f32> {
    /// What happens during the block, and on which frame.
    pub events: &'a EventBuffer,
    /// Where the music is at the start of the block.
    pub transport: &'a Transport,
    /// A second input that controls the processing without being heard, such
    /// as the kick drum that makes a compressor duck a bass line.
    ///
    /// It covers the whole block the host delivered. When the buffer passed
    /// alongside is only a piece of that block, frame `n` of the buffer is
    /// frame [`offset`](Self::offset)` + n` of the sidechain; see
    /// [`sidechain_peak`](Self::sidechain_peak).
    pub sidechain: Option<&'a AudioBuffer<S>>,
    /// Where the buffer passed alongside begins within the block the host
    /// delivered, in frames. Zero for a whole block;
    /// [`split_at_events_with`] and [`for_piece`](Self::for_piece) move it
    /// on for each piece.
    ///
    /// The sidechain and the transport describe the whole block, so a
    /// processor reads them at `offset + n` for its frame `n`.
    pub offset: usize,
}

impl<'a, S: Sample> ProcessContext<'a, S> {
    /// A context with these events and this transport, and no sidechain.
    pub fn new(events: &'a EventBuffer, transport: &'a Transport) -> Self {
        Self {
            events,
            transport,
            sidechain: None,
            offset: 0,
        }
    }

    /// A context for a block in which nothing happens and the transport is
    /// stopped.
    pub fn empty() -> Self {
        Self::new(&NO_EVENTS, &Transport::STOPPED)
    }

    /// Adds a sidechain input.
    #[must_use]
    pub fn with_sidechain(mut self, sidechain: &'a AudioBuffer<S>) -> Self {
        self.sidechain = Some(sidechain);
        self
    }

    /// The same context with no events.
    ///
    /// It keeps the [`offset`](Self::offset), so it is not the context for
    /// the later pieces of a split block: use [`for_piece`](Self::for_piece)
    /// or [`split_at_events_with`], which keep a sidechain aligned with the
    /// piece.
    #[must_use]
    pub fn without_events(mut self) -> Self {
        self.events = &NO_EVENTS;
        self
    }

    /// The context for the piece of the block that begins `start` frames
    /// into the buffer this context describes: no events (the caller has
    /// taken them out) and the [`offset`](Self::offset) moved on by `start`.
    #[must_use]
    pub fn for_piece(mut self, start: usize) -> Self {
        self.events = &NO_EVENTS;
        self.offset = self.offset.saturating_add(start);
        self
    }

    /// The largest magnitude across the sidechain's channels at frame
    /// `frame` of the buffer passed alongside, that is, at sidechain frame
    /// `offset + frame`. `None` without a sidechain; zero past its end.
    #[inline]
    pub fn sidechain_peak(&self, frame: usize) -> Option<f64> {
        let sidechain = self.sidechain?;
        let at = self.offset.saturating_add(frame);
        let mut level = 0.0f64;
        if at < sidechain.num_frames() {
            for channel in sidechain.iter_channels() {
                level = level.max(channel[at].to_f64().abs());
            }
        }
        Some(level)
    }
}

/// Processes a block in pieces so that every event takes effect on exactly
/// the frame it belongs to.
///
/// For each event, in order, the audio up to the event's frame is rendered,
/// then `handle` is called for the event. `render` receives the buffer
/// narrowed to one piece at a time (see [`AudioBuffer::windowed`]) and never
/// sees an empty piece. Both closures get `state`, which is usually the
/// processor itself.
///
/// An event placed beyond the end of the block is handled after the last
/// frame, so it is never lost.
///
/// When the pieces are processed with a [`ProcessContext`], use
/// [`split_at_events_with`] instead: it hands each piece a context whose
/// sidechain lines up with the piece.
///
/// Nothing is copied or allocated. Real-time safe as long as the closures
/// are.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::event::{Event, EventBuffer, EventKind, NoteRef};
/// use manguebyte_dsp::split_at_events;
///
/// // A "processor" that is just a level, switched on by a note.
/// let mut level = 0.0f32;
/// let mut events = EventBuffer::with_capacity(4);
/// let note = NoteRef::new(0, 60);
/// events.push(Event { offset: 3, kind: EventKind::NoteOn { note, velocity: 1.0 } }).unwrap();
///
/// let mut buffer = AudioBuffer::<f32>::new(1, 6);
/// split_at_events(
///     &mut level,
///     &mut buffer,
///     &events,
///     |level, event| {
///         if let EventKind::NoteOn { velocity, .. } = event.kind {
///             *level = velocity;
///         }
///     },
///     |level, piece| piece.channel_mut(0).fill(*level),
/// );
/// assert_eq!(buffer.channel(0), &[0.0, 0.0, 0.0, 1.0, 1.0, 1.0]);
/// ```
pub fn split_at_events<S: Sample, T: ?Sized>(
    state: &mut T,
    buffer: &mut AudioBuffer<S>,
    events: &EventBuffer,
    mut handle: impl FnMut(&mut T, &Event),
    mut render: impl FnMut(&mut T, &mut AudioBuffer<S>),
) {
    let frames = buffer.num_frames();
    let mut position = 0;
    for event in events {
        let at = (event.offset as usize).min(frames);
        if at > position {
            buffer.windowed(position..at, |piece| render(state, piece));
            position = at;
        }
        handle(state, event);
    }
    if frames > position {
        buffer.windowed(position..frames, |piece| render(state, piece));
    }
}

/// Like [`split_at_events`], with the events taken from `context`, and
/// `render` also given the context for its piece: no events, and the
/// [`offset`](ProcessContext::offset) moved to where the piece begins, so a
/// sidechain stays aligned with the audio it keys.
///
/// Nothing is copied or allocated. Real-time safe as long as the closures
/// are.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::event::{Event, EventBuffer, EventKind, NoteRef};
/// use manguebyte_dsp::{ProcessContext, Transport, split_at_events_with};
///
/// let mut events = EventBuffer::with_capacity(4);
/// let note = NoteRef::new(0, 60);
/// events.push(Event { offset: 3, kind: EventKind::NoteOn { note, velocity: 1.0 } }).unwrap();
/// let mut key = AudioBuffer::<f32>::new(1, 6);
/// key.channel_mut(0)[3..].fill(1.0);
/// let context = ProcessContext::new(&events, &Transport::STOPPED).with_sidechain(&key);
///
/// // Each piece copies its stretch of the sidechain.
/// let mut buffer = AudioBuffer::<f32>::new(1, 6);
/// split_at_events_with(
///     &mut (),
///     &mut buffer,
///     &context,
///     |_, _| {},
///     |_, piece, piece_context| {
///         for (frame, sample) in piece.channel_mut(0).iter_mut().enumerate() {
///             *sample = piece_context.sidechain_peak(frame).unwrap() as f32;
///         }
///     },
/// );
/// assert_eq!(buffer.channel(0), &[0.0, 0.0, 0.0, 1.0, 1.0, 1.0]);
/// ```
pub fn split_at_events_with<S: Sample, T: ?Sized>(
    state: &mut T,
    buffer: &mut AudioBuffer<S>,
    context: &ProcessContext<'_, S>,
    mut handle: impl FnMut(&mut T, &Event),
    mut render: impl FnMut(&mut T, &mut AudioBuffer<S>, &ProcessContext<'_, S>),
) {
    let frames = buffer.num_frames();
    let mut position = 0;
    for event in context.events {
        let at = (event.offset as usize).min(frames);
        if at > position {
            let piece_context = context.for_piece(position);
            buffer.windowed(position..at, |piece| render(state, piece, &piece_context));
            position = at;
        }
        handle(state, event);
    }
    if frames > position {
        let piece_context = context.for_piece(position);
        buffer.windowed(position..frames, |piece| {
            render(state, piece, &piece_context);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventKind, NoteRef};

    const RATE: f64 = 48_000.0;

    fn close(value: Option<f64>, expected: f64) -> bool {
        value.is_some_and(|value| (value - expected).abs() < 1e-9)
    }

    #[test]
    fn transport_advances_in_samples_beats_and_bars() {
        let mut transport = Transport::playing_at(120.0);
        transport.time_signature = Some((3, 4));
        assert!(close(transport.beats_per_sample(RATE), 2.0 / RATE));

        // Half a second at 120 bpm is one beat.
        assert!(close(transport.beats_at(24_000, RATE), 1.0));
        transport.advance(24_000, RATE);
        assert_eq!(transport.position_samples, Some(24_000));
        assert!(close(transport.position_beats, 1.0));
        assert!(close(transport.bar_start_beats, 0.0));

        // Three and a half more beats: a beat and a half into the second bar
        // of 3/4.
        transport.advance(84_000, RATE);
        assert!(close(transport.position_beats, 4.5));
        assert!(close(transport.bar_start_beats, 3.0));
    }

    #[test]
    fn transport_loops_and_stays_put_when_stopped() {
        let mut transport = Transport::playing_at(120.0);
        transport.loop_beats = Some((4.0, 8.0));
        transport.position_beats = Some(7.0);
        transport.advance(48_000, RATE); // two beats: 9.0 wraps to 5.0
        assert!(close(transport.position_beats, 5.0));

        transport.playing = false;
        transport.advance(48_000, RATE);
        assert!(close(transport.position_beats, 5.0));
        assert!(close(transport.beats_at(1_000, RATE), 5.0));

        let mut unknown = Transport::STOPPED;
        unknown.playing = true;
        unknown.advance(100, RATE);
        assert_eq!(unknown.position_beats, None);
        assert_eq!(unknown.beats_at(0, RATE), None);
    }

    fn note_on(offset: u32, key: u8) -> Event {
        Event {
            offset,
            kind: EventKind::NoteOn {
                note: NoteRef::new(0, key),
                velocity: 1.0,
            },
        }
    }

    /// Runs the splitter and records what it did, in order.
    fn trace(frames: usize, offsets: &[u32]) -> Vec<String> {
        let mut events = EventBuffer::with_capacity(16);
        for (index, &offset) in offsets.iter().enumerate() {
            events.push(note_on(offset, index as u8)).unwrap();
        }
        let mut log = Vec::new();
        let mut buffer = AudioBuffer::<f32>::new(1, frames);
        split_at_events(
            &mut log,
            &mut buffer,
            &events,
            |log: &mut Vec<String>, event| log.push(format!("event@{}", event.offset)),
            |log, piece| log.push(format!("render {}", piece.num_frames())),
        );
        log
    }

    #[test]
    fn splits_exactly_at_events() {
        assert_eq!(trace(8, &[]), ["render 8"]);
        assert_eq!(trace(8, &[3]), ["render 3", "event@3", "render 5"]);
        assert_eq!(trace(8, &[0]), ["event@0", "render 8"]);
        assert_eq!(
            trace(8, &[2, 2, 6]),
            [
                "render 2", "event@2", "event@2", "render 4", "event@6", "render 2"
            ]
        );
    }

    #[test]
    fn late_events_are_handled_after_the_last_frame_and_empty_blocks_work() {
        assert_eq!(trace(8, &[8]), ["render 8", "event@8"]);
        assert_eq!(
            trace(8, &[500, 4]),
            ["render 4", "event@4", "render 4", "event@500"]
        );
        assert_eq!(trace(0, &[0, 7]), ["event@0", "event@7"]);
    }

    #[test]
    fn pieces_cover_the_block_without_overlap() {
        let mut events = EventBuffer::with_capacity(8);
        for offset in [1, 4, 4, 9] {
            events.push(note_on(offset, 0)).unwrap();
        }
        let mut counter = 0.0f32;
        let mut buffer = AudioBuffer::<f32>::new(2, 12);
        split_at_events(
            &mut counter,
            &mut buffer,
            &events,
            |_, _| {},
            |counter, piece| {
                for frame in 0..piece.num_frames() {
                    *counter += 1.0;
                    for channel in 0..piece.num_channels() {
                        piece.channel_mut(channel)[frame] += *counter;
                    }
                }
            },
        );
        let expected: Vec<f32> = (1..=12).map(|n| n as f32).collect();
        assert_eq!(buffer.channel(0), expected.as_slice());
        assert_eq!(buffer.channel(1), expected.as_slice());
    }

    #[test]
    fn empty_context_has_nothing_in_it() {
        let context = ProcessContext::<f32>::empty();
        assert!(context.events.is_empty());
        assert!(!context.transport.playing);
        assert!(context.sidechain.is_none());

        let sidechain = AudioBuffer::<f32>::new(1, 4);
        let mut events = EventBuffer::with_capacity(1);
        events.push(note_on(0, 60)).unwrap();
        let transport = Transport::playing_at(90.0);
        let full = ProcessContext::new(&events, &transport).with_sidechain(&sidechain);
        assert_eq!(full.events.len(), 1);
        assert!(full.sidechain.is_some());
        assert!(full.without_events().events.is_empty());
        assert!(full.without_events().transport.playing);
        assert_eq!(full.offset, 0);
        assert_eq!(full.without_events().offset, 0);
    }

    #[test]
    fn a_piece_context_moves_the_sidechain_on() {
        let mut key = AudioBuffer::<f32>::new(2, 6);
        key.channel_mut(0)
            .copy_from_slice(&[0.1, 0.2, 0.3, 0.4, 0.5, 0.6]);
        key.channel_mut(1)
            .copy_from_slice(&[0.0, -0.9, 0.0, 0.0, 0.0, 0.0]);
        let mut events = EventBuffer::with_capacity(1);
        events.push(note_on(0, 60)).unwrap();
        let transport = Transport::STOPPED;
        let context = ProcessContext::new(&events, &transport).with_sidechain(&key);
        assert_eq!(context.sidechain_peak(0), Some(0.1f32.into()));
        assert_eq!(context.sidechain_peak(1), Some(0.9f32.into()));
        assert_eq!(context.sidechain_peak(6), Some(0.0));
        assert_eq!(context.sidechain_peak(usize::MAX), Some(0.0));

        let piece = context.for_piece(2);
        assert!(piece.events.is_empty());
        assert_eq!(piece.offset, 2);
        assert_eq!(piece.sidechain_peak(0), Some(0.3f32.into()));
        assert_eq!(piece.sidechain_peak(3), Some(0.6f32.into()));
        assert_eq!(piece.sidechain_peak(4), Some(0.0));
        // Pieces of pieces add up, and never overflow.
        assert_eq!(piece.for_piece(3).offset, 5);
        assert_eq!(piece.for_piece(usize::MAX).offset, usize::MAX);
        assert_eq!(piece.for_piece(usize::MAX).sidechain_peak(1), Some(0.0));
        assert_eq!(ProcessContext::<f32>::empty().sidechain_peak(0), None);
    }

    #[test]
    fn split_with_context_hands_each_piece_its_offset() {
        let mut events = EventBuffer::with_capacity(8);
        for offset in [0, 3, 3, 5, 40] {
            events.push(note_on(offset, 0)).unwrap();
        }
        let transport = Transport::STOPPED;
        let context = ProcessContext::<f32>::new(&events, &transport);
        let mut log = Vec::new();
        let mut buffer = AudioBuffer::<f32>::new(1, 8);
        split_at_events_with(
            &mut log,
            &mut buffer,
            &context,
            |log: &mut Vec<String>, event| log.push(format!("event@{}", event.offset)),
            |log, piece, piece_context| {
                assert!(piece_context.events.is_empty());
                log.push(format!(
                    "render {} at {}",
                    piece.num_frames(),
                    piece_context.offset
                ));
            },
        );
        assert_eq!(
            log,
            [
                "event@0",
                "render 3 at 0",
                "event@3",
                "event@3",
                "render 2 at 3",
                "event@5",
                "render 3 at 5",
                "event@40"
            ]
        );

        // Nested: a context that already starts later passes that on.
        let later = context.for_piece(10);
        let mut offsets = Vec::new();
        let mut buffer = AudioBuffer::<f32>::new(1, 2);
        split_at_events_with(
            &mut offsets,
            &mut buffer,
            &later,
            |_, _| {},
            |offsets: &mut Vec<usize>, _, piece_context| offsets.push(piece_context.offset),
        );
        assert_eq!(offsets, [10]);
    }
}
