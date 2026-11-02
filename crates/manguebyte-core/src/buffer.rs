use crate::Sample;
use crate::vector;

/// What [`AudioBuffer::narrow`] took away, for [`AudioBuffer::widen`] to
/// give back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a narrowed buffer stays narrow until `widen` is given this"]
pub struct Window {
    offset: usize,
    frames: usize,
}

/// A planar, multi-channel block of audio samples.
///
/// Each channel is stored contiguously, so [`channel`](Self::channel) hands out
/// plain slices that the compiler can vectorise. The buffer owns a fixed
/// capacity and [`set_num_frames`](Self::set_num_frames) changes the active
/// length without allocating, which makes it safe to reuse on the audio thread
/// for blocks of varying size.
///
/// The helpers that touch every sample ([`apply_gain`](Self::apply_gain),
/// [`apply_gain_ramp`](Self::apply_gain_ramp), [`mix_from`](Self::mix_from),
/// [`peak`](Self::peak), [`rms`](Self::rms) and the stereo case of the
/// interleaving copies) run on the processor's vector instructions through
/// [`vector`](crate::vector), with the same results on every processor.
#[derive(Debug, Clone)]
pub struct AudioBuffer<S: Sample = f32> {
    data: Vec<S>,
    channels: usize,
    frames: usize,
    // Stride between channels in `data`. Kept at 1 or more so that chunking
    // `data` always yields exactly `channels` slices, even for empty buffers.
    capacity: usize,
    // First visible frame of each channel. Non-zero only inside `windowed`.
    offset: usize,
}

impl<S: Sample> AudioBuffer<S> {
    /// Creates a silent buffer with `channels` channels of `frames` frames.
    ///
    /// `frames` also becomes the capacity. This allocates.
    pub fn new(channels: usize, frames: usize) -> Self {
        let capacity = frames.max(1);
        Self {
            data: vec![S::ZERO; channels * capacity],
            channels,
            frames,
            capacity,
            offset: 0,
        }
    }

    /// Creates a buffer from interleaved samples (`L R L R ...`).
    ///
    /// # Panics
    ///
    /// Panics if `channels` is zero or `interleaved.len()` is not a multiple
    /// of `channels`.
    pub fn from_interleaved(interleaved: &[S], channels: usize) -> Self {
        assert!(channels > 0, "channel count must be non-zero");
        assert!(
            interleaved.len() % channels == 0,
            "interleaved length must be a multiple of the channel count"
        );
        let mut buffer = Self::new(channels, interleaved.len() / channels);
        buffer.copy_from_interleaved(interleaved);
        buffer
    }

    /// Number of channels.
    #[inline]
    pub fn num_channels(&self) -> usize {
        self.channels
    }

    /// Number of active frames (samples per channel).
    #[inline]
    pub fn num_frames(&self) -> usize {
        self.frames
    }

    /// Largest frame count [`set_num_frames`](Self::set_num_frames) accepts.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity - self.offset
    }

    /// Changes the active frame count without allocating.
    ///
    /// Samples that become visible keep whatever value they last held.
    ///
    /// # Panics
    ///
    /// Panics if `frames` exceeds [`capacity`](Self::capacity).
    #[inline]
    pub fn set_num_frames(&mut self, frames: usize) {
        assert!(
            frames <= self.capacity(),
            "frame count {frames} exceeds buffer capacity {}",
            self.capacity()
        );
        self.frames = frames;
    }

    /// Reshapes the buffer and clears it to silence.
    ///
    /// This may allocate, so call it while preparing, not while processing.
    pub fn resize(&mut self, channels: usize, frames: usize) {
        self.capacity = frames.max(1);
        self.channels = channels;
        self.frames = frames;
        self.offset = 0;
        self.data.clear();
        self.data.resize(channels * self.capacity, S::ZERO);
    }

    /// Samples of one channel.
    ///
    /// # Panics
    ///
    /// Panics if `index` is out of range.
    #[inline]
    pub fn channel(&self, index: usize) -> &[S] {
        assert!(index < self.channels, "channel index out of range");
        let start = index * self.capacity + self.offset;
        &self.data[start..start + self.frames]
    }

    /// Mutable samples of one channel.
    ///
    /// # Panics
    ///
    /// Panics if `index` is out of range.
    #[inline]
    pub fn channel_mut(&mut self, index: usize) -> &mut [S] {
        assert!(index < self.channels, "channel index out of range");
        let start = index * self.capacity + self.offset;
        &mut self.data[start..start + self.frames]
    }

    /// Two distinct channels, both mutable.
    ///
    /// # Panics
    ///
    /// Panics if `a == b` or either index is out of range.
    pub fn channel_pair_mut(&mut self, a: usize, b: usize) -> (&mut [S], &mut [S]) {
        assert!(a != b, "channel indices must differ");
        assert!(
            a < self.channels && b < self.channels,
            "channel index out of range"
        );
        let (frames, capacity, offset) = (self.frames, self.capacity, self.offset);
        let (low, high) = (a.min(b), a.max(b));
        let (head, tail) = self.data.split_at_mut(high * capacity);
        let low_slice = &mut head[low * capacity + offset..low * capacity + offset + frames];
        let high_slice = &mut tail[offset..offset + frames];
        if a < b {
            (low_slice, high_slice)
        } else {
            (high_slice, low_slice)
        }
    }

    /// Iterates over the channels.
    pub fn iter_channels(&self) -> impl ExactSizeIterator<Item = &[S]> {
        let (start, end) = (self.offset, self.offset + self.frames);
        self.data
            .chunks_exact(self.capacity)
            .map(move |channel| &channel[start..end])
    }

    /// Iterates mutably over the channels.
    pub fn iter_channels_mut(&mut self) -> impl ExactSizeIterator<Item = &mut [S]> {
        let (start, end) = (self.offset, self.offset + self.frames);
        self.data
            .chunks_exact_mut(self.capacity)
            .map(move |channel| &mut channel[start..end])
    }

    /// Runs `work` with the buffer narrowed to the frames in `range`, then
    /// widens it again.
    ///
    /// Inside `work` the buffer looks like a shorter one that starts at
    /// `range.start`: every method sees only those frames. Nothing is copied
    /// or allocated, so this is how a block is processed in pieces on the
    /// audio thread, for example to apply an event on the exact frame it
    /// belongs to.
    ///
    /// ```
    /// use manguebyte_core::AudioBuffer;
    ///
    /// let mut buffer = AudioBuffer::<f32>::new(1, 8);
    /// buffer.windowed(2..5, |window| {
    ///     assert_eq!(window.num_frames(), 3);
    ///     window.channel_mut(0).fill(1.0);
    /// });
    /// assert_eq!(buffer.channel(0), &[0.0, 0.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0]);
    /// ```
    ///
    /// Do not [`resize`](Self::resize) the buffer inside `work`.
    ///
    /// # Panics
    ///
    /// Panics if `range` does not lie within the active frames.
    pub fn windowed<R>(
        &mut self,
        range: core::ops::Range<usize>,
        work: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let window = self.narrow(range);
        let result = work(self);
        self.widen(window);
        result
    }

    /// Narrows the buffer to the frames in `range`, as
    /// [`windowed`](Self::windowed) does, until [`widen`](Self::widen) is
    /// given what this returns. Nothing is copied or allocated.
    ///
    /// This is for narrowing several buffers together, such as a plugin's
    /// output buses, which nested closures cannot do for a number of
    /// buffers known only at run time. Prefer `windowed` otherwise: with
    /// this pair, every `narrow` must be undone by a `widen` of the same
    /// buffer, the most recent first.
    ///
    /// ```
    /// use manguebyte_core::AudioBuffer;
    ///
    /// let mut buses = [AudioBuffer::<f32>::new(1, 4), AudioBuffer::<f32>::new(2, 4)];
    /// let windows = buses.each_mut().map(|bus| bus.narrow(1..3));
    /// for bus in &mut buses {
    ///     assert_eq!(bus.num_frames(), 2);
    ///     bus.channel_mut(0).fill(1.0);
    /// }
    /// for (bus, window) in buses.iter_mut().zip(windows) {
    ///     bus.widen(window);
    /// }
    /// assert_eq!(buses[1].channel(0), &[0.0, 1.0, 1.0, 0.0]);
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if `range` does not lie within the active frames.
    pub fn narrow(&mut self, range: core::ops::Range<usize>) -> Window {
        assert!(
            range.start <= range.end && range.end <= self.frames,
            "window {}..{} is outside the buffer's {} frames",
            range.start,
            range.end,
            self.frames
        );
        let window = Window {
            offset: self.offset,
            frames: self.frames,
        };
        self.offset += range.start;
        self.frames = range.end - range.start;
        window
    }

    /// Undoes the [`narrow`](Self::narrow) that returned `window`.
    pub fn widen(&mut self, window: Window) {
        self.offset = window.offset;
        self.frames = window.frames;
    }

    /// Sets every active sample to silence.
    pub fn clear(&mut self) {
        for channel in self.iter_channels_mut() {
            channel.fill(S::ZERO);
        }
    }

    /// Multiplies every sample by `gain`.
    pub fn apply_gain(&mut self, gain: S) {
        for channel in self.iter_channels_mut() {
            vector::scale(channel, gain);
        }
    }

    /// Applies a linear gain ramp from `start` to `end` across the block.
    ///
    /// The first frame is scaled by `start`; the ramp would reach `end` on the
    /// frame after the last one, so consecutive blocks join seamlessly.
    ///
    /// Frame `i` is scaled by `start + step * i`, with
    /// `step = (end - start) / num_frames()`: each gain is worked out from
    /// its frame, so the ramp does not drift on long blocks (see
    /// [`vector::scale_ramp`]).
    pub fn apply_gain_ramp(&mut self, start: S, end: S) {
        if self.frames == 0 {
            return;
        }
        let step = (end - start) / S::from_f64(self.frames as f64);
        for channel in self.iter_channels_mut() {
            vector::scale_ramp(channel, start, step);
        }
    }

    /// Copies all samples from `source`.
    ///
    /// # Panics
    ///
    /// Panics if the two buffers differ in channel or frame count.
    pub fn copy_from(&mut self, source: &Self) {
        self.assert_same_shape(source);
        for (dst, src) in self.iter_channels_mut().zip(source.iter_channels()) {
            dst.copy_from_slice(src);
        }
    }

    /// Adds `source` scaled by `gain` to this buffer.
    ///
    /// # Panics
    ///
    /// Panics if the two buffers differ in channel or frame count.
    pub fn mix_from(&mut self, source: &Self, gain: S) {
        self.assert_same_shape(source);
        for (dst, src) in self.iter_channels_mut().zip(source.iter_channels()) {
            vector::add_scaled(dst, src, gain);
        }
    }

    /// Largest absolute sample value across all channels.
    ///
    /// A buffer that holds a NaN anywhere has a NaN peak, so that a test
    /// such as `peak() < 1.0` fails rather than passing on garbage.
    pub fn peak(&self) -> S {
        let mut peak = S::ZERO;
        for channel in self.iter_channels() {
            let magnitude = vector::peak(channel);
            if magnitude.to_f64().is_nan() {
                return magnitude;
            }
            if magnitude > peak {
                peak = magnitude;
            }
        }
        peak
    }

    /// Root-mean-square level of one channel.
    ///
    /// The squares are summed in `f64`, in the fixed order of
    /// [`vector::sum_of_squares`].
    ///
    /// # Panics
    ///
    /// Panics if `channel` is out of range.
    pub fn rms(&self, channel: usize) -> S {
        let samples = self.channel(channel);
        if samples.is_empty() {
            return S::ZERO;
        }
        let sum = vector::sum_of_squares(samples);
        S::from_f64((sum / samples.len() as f64).sqrt())
    }

    /// Fills the buffer from interleaved samples.
    ///
    /// # Panics
    ///
    /// Panics if `interleaved.len()` is not `num_channels() * num_frames()`.
    pub fn copy_from_interleaved(&mut self, interleaved: &[S]) {
        assert_eq!(
            interleaved.len(),
            self.channels * self.frames,
            "interleaved length does not match the buffer shape"
        );
        let channels = self.channels;
        match channels {
            1 => self.channel_mut(0).copy_from_slice(interleaved),
            2 => {
                let (left, right) = self.channel_pair_mut(0, 1);
                vector::deinterleave_stereo(left, right, interleaved);
            }
            _ => {
                for (index, channel) in self.iter_channels_mut().enumerate() {
                    for (sample, &value) in channel
                        .iter_mut()
                        .zip(interleaved.iter().skip(index).step_by(channels))
                    {
                        *sample = value;
                    }
                }
            }
        }
    }

    /// Writes the buffer out as interleaved samples.
    ///
    /// # Panics
    ///
    /// Panics if `interleaved.len()` is not `num_channels() * num_frames()`.
    pub fn copy_to_interleaved(&self, interleaved: &mut [S]) {
        assert_eq!(
            interleaved.len(),
            self.channels * self.frames,
            "interleaved length does not match the buffer shape"
        );
        let channels = self.channels;
        match channels {
            1 => interleaved.copy_from_slice(self.channel(0)),
            2 => vector::interleave_stereo(interleaved, self.channel(0), self.channel(1)),
            _ => {
                for (index, channel) in self.iter_channels().enumerate() {
                    for (slot, &sample) in interleaved
                        .iter_mut()
                        .skip(index)
                        .step_by(channels)
                        .zip(channel)
                    {
                        *slot = sample;
                    }
                }
            }
        }
    }

    fn assert_same_shape(&self, other: &Self) {
        assert!(
            self.channels == other.channels && self.frames == other.frames,
            "buffer shapes differ: {}x{} vs {}x{}",
            self.channels,
            self.frames,
            other.channels,
            other.frames
        );
    }
}

/// Buffers are equal when they have the same shape and the same active
/// samples; spare capacity is ignored.
impl<S: Sample> PartialEq for AudioBuffer<S> {
    fn eq(&self, other: &Self) -> bool {
        self.channels == other.channels
            && self.frames == other.frames
            && self.iter_channels().eq(other.iter_channels())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equality_ignores_spare_capacity() {
        let mut large = AudioBuffer::<f32>::new(2, 8);
        large.channel_mut(1).fill(0.5);
        large.set_num_frames(3);

        let mut small = AudioBuffer::<f32>::new(2, 3);
        small.channel_mut(1).fill(0.5);
        assert_eq!(large, small);

        small.channel_mut(0)[2] = 1.0;
        assert_ne!(large, small);
    }

    #[test]
    fn new_buffer_is_silent() {
        let buffer = AudioBuffer::<f32>::new(2, 8);
        assert_eq!(buffer.num_channels(), 2);
        assert_eq!(buffer.num_frames(), 8);
        assert_eq!(buffer.peak(), 0.0);
    }

    #[test]
    fn empty_buffer_still_yields_every_channel() {
        let mut buffer = AudioBuffer::<f32>::new(3, 0);
        assert_eq!(buffer.iter_channels().len(), 3);
        assert!(buffer.iter_channels_mut().all(|c| c.is_empty()));
    }

    #[test]
    fn interleaved_round_trip() {
        let interleaved = [1.0f32, -1.0, 2.0, -2.0, 3.0, -3.0];
        let buffer = AudioBuffer::from_interleaved(&interleaved, 2);
        assert_eq!(buffer.channel(0), &[1.0, 2.0, 3.0]);
        assert_eq!(buffer.channel(1), &[-1.0, -2.0, -3.0]);

        let mut out = [0.0f32; 6];
        buffer.copy_to_interleaved(&mut out);
        assert_eq!(out, interleaved);
    }

    #[test]
    fn interleaving_works_for_every_channel_count() {
        for channels in 1..=5 {
            for frames in [0, 1, 7, 33] {
                let interleaved: Vec<f64> = (0..channels * frames).map(|i| i as f64).collect();
                let buffer = AudioBuffer::from_interleaved(&interleaved, channels);
                for (index, channel) in buffer.iter_channels().enumerate() {
                    for (frame, &sample) in channel.iter().enumerate() {
                        assert_eq!(sample, (frame * channels + index) as f64);
                    }
                }
                let mut out = vec![-1.0; channels * frames];
                buffer.copy_to_interleaved(&mut out);
                assert_eq!(out, interleaved);
            }
        }
    }

    #[test]
    fn a_long_gain_ramp_does_not_drift() {
        // Adding the step 48 000 times in `f32` would drift; working each
        // gain out from its frame does not.
        let mut buffer = AudioBuffer::<f32>::new(1, 48_000);
        buffer.channel_mut(0).fill(1.0);
        buffer.apply_gain_ramp(1.0, 0.0);
        for (frame, &gain) in buffer.channel(0).iter().enumerate() {
            let exact = 1.0 - frame as f64 / 48_000.0;
            assert!((f64::from(gain) - exact).abs() < 1e-6, "frame {frame}");
        }
    }

    #[test]
    fn set_num_frames_keeps_channels_apart() {
        let mut buffer = AudioBuffer::<f32>::new(2, 4);
        buffer.channel_mut(1).fill(1.0);
        buffer.set_num_frames(2);
        assert_eq!(buffer.channel(0), &[0.0, 0.0]);
        assert_eq!(buffer.channel(1), &[1.0, 1.0]);
    }

    #[test]
    #[should_panic(expected = "exceeds buffer capacity")]
    fn set_num_frames_beyond_capacity_panics() {
        AudioBuffer::<f32>::new(1, 4).set_num_frames(5);
    }

    #[test]
    fn channel_pair_mut_respects_argument_order() {
        let mut buffer = AudioBuffer::<f32>::new(3, 2);
        buffer.channel_mut(0).fill(1.0);
        buffer.channel_mut(2).fill(3.0);
        let (c, a) = buffer.channel_pair_mut(2, 0);
        assert_eq!(c, &[3.0, 3.0]);
        assert_eq!(a, &[1.0, 1.0]);
    }

    #[test]
    fn gain_ramp_starts_at_start_and_approaches_end() {
        let mut buffer = AudioBuffer::<f64>::new(1, 4);
        buffer.channel_mut(0).fill(1.0);
        buffer.apply_gain_ramp(0.0, 1.0);
        assert_eq!(buffer.channel(0), &[0.0, 0.25, 0.5, 0.75]);
    }

    #[test]
    fn mix_and_levels() {
        let mut a = AudioBuffer::<f32>::new(1, 4);
        let mut b = AudioBuffer::<f32>::new(1, 4);
        b.channel_mut(0).copy_from_slice(&[1.0, -1.0, 1.0, -1.0]);
        a.mix_from(&b, 0.5);
        assert_eq!(a.peak(), 0.5);
        assert!((a.rms(0) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn windows_narrow_every_view_and_nest() {
        let mut buffer = AudioBuffer::<f32>::new(2, 8);
        for (index, channel) in buffer.iter_channels_mut().enumerate() {
            for (frame, sample) in channel.iter_mut().enumerate() {
                *sample = (index * 10 + frame) as f32;
            }
        }

        let peak = buffer.windowed(2..6, |window| {
            assert_eq!(window.num_frames(), 4);
            assert_eq!(window.capacity(), 6);
            assert_eq!(window.channel(0), &[2.0, 3.0, 4.0, 5.0]);
            assert_eq!(window.channel(1), &[12.0, 13.0, 14.0, 15.0]);
            assert_eq!(window.iter_channels().nth(1).unwrap()[0], 12.0);
            let (left, right) = window.channel_pair_mut(1, 0);
            assert_eq!((left[3], right[3]), (15.0, 5.0));

            window.windowed(1..3, |inner| {
                assert_eq!(inner.channel(0), &[3.0, 4.0]);
                inner.apply_gain(-1.0);
            });
            assert_eq!(window.channel(0), &[2.0, -3.0, -4.0, 5.0]);
            window.peak()
        });

        assert_eq!(peak, 15.0);
        assert_eq!(buffer.num_frames(), 8);
        assert_eq!(buffer.capacity(), 8);
        assert_eq!(
            buffer.channel(0),
            &[0.0, 1.0, 2.0, -3.0, -4.0, 5.0, 6.0, 7.0]
        );
        assert_eq!(buffer.channel(1)[3], -13.0);
    }

    #[test]
    #[should_panic(expected = "outside the buffer")]
    fn a_window_cannot_reach_past_the_active_frames() {
        let mut buffer = AudioBuffer::<f32>::new(1, 8);
        buffer.set_num_frames(4);
        buffer.windowed(2..6, |_| {});
    }
}
