//! Sources of audio that are pulled, a block at a time.
//!
//! A processor transforms the audio that is handed to it. A source *makes*
//! audio when it is asked for some: the
//! device callback, a graph node or another source calls
//! [`pull`](AudioSource::pull) with a buffer, and the source fills it with
//! its next frames. File playback, a mixer of several sources, a sample
//! rate converter and a channel router are all sources, and they nest: a
//! mixer pulls from its inputs, a converter from the source it converts.
//!
//! - [`AudioSource`]: anything that can be pulled.
//! - [`PositionableSource`]: a source with a position that can be moved,
//!   such as a file.
//! - [`BufferSource`]: audio held in memory, played once or looped.
//!
//! The sources built on this live in the crates that have what they need:
//! the transport over a streamed file in `manguebyte-formats`, the mixer,
//! the sample rate converter, the channel router and the adapter that
//! makes any source a processor in `manguebyte-dsp`.
//!
//! # Real-time contract
//!
//! [`prepare`](AudioSource::prepare) may allocate and take its time.
//! [`pull`](AudioSource::pull), [`reset`](AudioSource::reset) and the
//! methods of [`PositionableSource`] run on the audio thread: they must not
//! allocate, lock, block, do I/O or panic.

use crate::{AudioBuffer, Sample};

/// Something that produces audio when it is asked for it.
///
/// ```
/// use manguebyte_core::source::{AudioSource, BufferSource};
/// use manguebyte_core::AudioBuffer;
///
/// let mut tone = AudioBuffer::<f32>::new(1, 4);
/// tone.channel_mut(0).copy_from_slice(&[1.0, 2.0, 3.0, 4.0]);
/// let mut source = BufferSource::new(tone).looping(true);
/// source.prepare(48_000.0, 3);
///
/// let mut out = AudioBuffer::<f32>::new(1, 3);
/// source.pull(&mut out);
/// source.pull(&mut out);
/// assert_eq!(out.channel(0), &[4.0, 1.0, 2.0]);
/// ```
pub trait AudioSource<S: Sample = f32> {
    /// Gets ready to be pulled at `sample_rate`, in blocks of up to
    /// `max_block_size` frames. Called before the first pull and whenever
    /// either changes. May allocate.
    fn prepare(&mut self, sample_rate: f64, max_block_size: usize);

    /// Fills every frame of every channel of `out` with what comes next.
    /// What `out` held before is overwritten; a source with nothing to
    /// give writes silence. A source with fewer channels than `out` leaves
    /// the others silent; one with more plays only the first ones.
    ///
    /// `out` is never longer than the block size given to
    /// [`prepare`](Self::prepare). Real-time safe.
    fn pull(&mut self, out: &mut AudioBuffer<S>);

    /// Forgets any state that depends on what was pulled before, such as
    /// a filter's memory, without moving the position. Real-time safe.
    fn reset(&mut self) {}
}

/// A source with a position that can be read and moved: a file, a
/// recording in memory.
pub trait PositionableSource<S: Sample = f32>: AudioSource<S> {
    /// Where the next frame pulled comes from, in frames from the start.
    fn position(&self) -> u64;

    /// Moves the position, so that the next frame pulled is `frame`. A
    /// frame past the end moves to the end. Real-time safe.
    fn seek(&mut self, frame: u64);

    /// The length in frames, when it is known.
    fn len_frames(&self) -> Option<u64>;

    /// Whether the source goes back to a start when it reaches an end.
    fn is_looping(&self) -> bool {
        false
    }
}

impl<S: Sample, T: AudioSource<S> + ?Sized> AudioSource<S> for Box<T> {
    fn prepare(&mut self, sample_rate: f64, max_block_size: usize) {
        (**self).prepare(sample_rate, max_block_size);
    }
    fn pull(&mut self, out: &mut AudioBuffer<S>) {
        (**self).pull(out);
    }
    fn reset(&mut self) {
        (**self).reset();
    }
}

impl<S: Sample, T: PositionableSource<S> + ?Sized> PositionableSource<S> for Box<T> {
    fn position(&self) -> u64 {
        (**self).position()
    }
    fn seek(&mut self, frame: u64) {
        (**self).seek(frame);
    }
    fn len_frames(&self) -> Option<u64> {
        (**self).len_frames()
    }
    fn is_looping(&self) -> bool {
        (**self).is_looping()
    }
}

impl<S: Sample, T: AudioSource<S> + ?Sized> AudioSource<S> for &mut T {
    fn prepare(&mut self, sample_rate: f64, max_block_size: usize) {
        (**self).prepare(sample_rate, max_block_size);
    }
    fn pull(&mut self, out: &mut AudioBuffer<S>) {
        (**self).pull(out);
    }
    fn reset(&mut self) {
        (**self).reset();
    }
}

/// Audio held in memory, as a source: played once and then silent, or
/// looped without a gap.
///
/// It can loop the whole buffer or a part of it, and is positionable to
/// the frame. Every method but [`new`](Self::new) is real-time safe.
#[derive(Debug, Clone, PartialEq)]
pub struct BufferSource<S: Sample = f32> {
    buffer: AudioBuffer<S>,
    position: usize,
    /// The looped stretch, as `start..end` within the buffer.
    looping: Option<(usize, usize)>,
}

impl<S: Sample> BufferSource<S> {
    /// A source that plays `buffer` once, from its start.
    pub fn new(buffer: AudioBuffer<S>) -> Self {
        Self {
            buffer,
            position: 0,
            looping: None,
        }
    }

    /// The same source, looping the whole buffer or not.
    #[must_use]
    pub fn looping(mut self, looping: bool) -> Self {
        let frames = self.buffer.num_frames();
        self.looping = (looping && frames > 0).then_some((0, frames));
        self
    }

    /// Loops `start..end` of the buffer, or stops looping with `None`. An
    /// end past the buffer's means its end; an empty stretch does not
    /// loop. Playback before the end of the stretch reaches it and goes
    /// round; playback past it plays on to the end of the buffer.
    pub fn set_loop(&mut self, region: Option<core::ops::Range<usize>>) {
        let frames = self.buffer.num_frames();
        self.looping = region
            .map(|region| (region.start, region.end.min(frames)))
            .filter(|(start, end)| start < end);
    }

    /// The audio.
    pub fn buffer(&self) -> &AudioBuffer<S> {
        &self.buffer
    }

    /// Whether playback has reached the end of a buffer it does not loop.
    pub fn is_finished(&self) -> bool {
        self.looping.is_none() && self.position >= self.buffer.num_frames()
    }
}

impl<S: Sample> AudioSource<S> for BufferSource<S> {
    fn prepare(&mut self, _sample_rate: f64, _max_block_size: usize) {}

    fn pull(&mut self, out: &mut AudioBuffer<S>) {
        let frames = out.num_frames();
        let channels = self.buffer.num_channels();
        let mut done = 0;
        while done < frames {
            // At the end of the loop, round to its start.
            if let Some((start, end)) = self.looping {
                if self.position == end {
                    self.position = start;
                }
            }
            let end = match self.looping {
                Some((_, end)) if self.position < end => end,
                _ => self.buffer.num_frames(),
            };
            let count = end.saturating_sub(self.position).min(frames - done);
            if count == 0 {
                break;
            }
            for (index, channel) in out.iter_channels_mut().enumerate() {
                let destination = &mut channel[done..done + count];
                if index < channels {
                    let from = &self.buffer.channel(index)[self.position..self.position + count];
                    destination.copy_from_slice(from);
                } else {
                    destination.fill(S::ZERO);
                }
            }
            done += count;
            self.position += count;
        }
        for channel in out.iter_channels_mut() {
            channel[done..].fill(S::ZERO);
        }
    }
}

impl<S: Sample> PositionableSource<S> for BufferSource<S> {
    fn position(&self) -> u64 {
        self.position as u64
    }

    fn seek(&mut self, frame: u64) {
        let frames = self.buffer.num_frames();
        self.position = usize::try_from(frame).map_or(frames, |frame| frame.min(frames));
    }

    fn len_frames(&self) -> Option<u64> {
        Some(self.buffer.num_frames() as u64)
    }

    fn is_looping(&self) -> bool {
        self.looping.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(frames: usize) -> AudioBuffer<f32> {
        let mut buffer = AudioBuffer::new(2, frames);
        for (index, sample) in buffer.channel_mut(0).iter_mut().enumerate() {
            *sample = index as f32;
        }
        for (index, sample) in buffer.channel_mut(1).iter_mut().enumerate() {
            *sample = -(index as f32);
        }
        buffer
    }

    fn pull_all(source: &mut BufferSource, sizes: &[usize], total: usize) -> Vec<f32> {
        let mut out = AudioBuffer::new(2, 64);
        let mut heard = Vec::new();
        let mut size = sizes.iter().cycle();
        while heard.len() < total {
            out.set_num_frames(*size.next().unwrap());
            source.pull(&mut out);
            heard.extend_from_slice(out.channel(0));
            assert_eq!(
                out.channel(1).iter().map(|x| -x).collect::<Vec<_>>(),
                out.channel(0)
            );
        }
        heard.truncate(total);
        heard
    }

    #[test]
    fn plays_once_then_silence_whatever_the_blocks() {
        for sizes in [&[1][..], &[3, 7], &[64], &[5, 1, 20]] {
            let mut source = BufferSource::new(ramp(10));
            let heard = pull_all(&mut source, sizes, 14);
            let expected: Vec<f32> = (0..10).map(|x| x as f32).chain([0.0; 4]).collect();
            assert_eq!(heard, expected);
            assert!(source.is_finished());
        }
    }

    #[test]
    fn loops_without_a_gap_and_seeks_to_the_frame() {
        for sizes in [&[1][..], &[3, 7], &[64]] {
            let mut source = BufferSource::new(ramp(10));
            source.set_loop(Some(4..7));
            source.seek(2);
            let heard = pull_all(&mut source, sizes, 11);
            assert_eq!(
                heard,
                [2.0, 3.0, 4.0, 5.0, 6.0, 4.0, 5.0, 6.0, 4.0, 5.0, 6.0]
            );
            assert!(source.is_looping());
            // Past the loop, playback runs to the end.
            source.seek(8);
            assert_eq!(pull_all(&mut source, sizes, 4), [8.0, 9.0, 0.0, 0.0]);
        }
        let mut whole = BufferSource::new(ramp(3)).looping(true);
        assert_eq!(
            pull_all(&mut whole, &[2], 7),
            [0.0, 1.0, 2.0, 0.0, 1.0, 2.0, 0.0]
        );
        // An empty buffer never loops, and a seek past the end stays there.
        let mut empty = BufferSource::new(AudioBuffer::<f32>::new(2, 0)).looping(true);
        assert_eq!(pull_all(&mut empty, &[4], 4), [0.0; 4]);
        whole.seek(u64::MAX);
        assert_eq!(whole.position(), 3);
    }

    #[test]
    fn a_wider_buffer_gets_silence_and_a_narrower_one_the_first_channels() {
        let mut source = BufferSource::new(ramp(4));
        let mut wide = AudioBuffer::new(3, 4);
        wide.channel_mut(2).fill(9.0);
        source.pull(&mut wide);
        assert_eq!(wide.channel(2), &[0.0; 4]);
        source.seek(0);
        let mut mono = AudioBuffer::new(1, 4);
        source.pull(&mut mono);
        assert_eq!(mono.channel(0), &[0.0, 1.0, 2.0, 3.0]);
    }
}
