//! Adapters between the interleaved, device-format slices a backend hands out
//! and the planar `f32` blocks the rest of Manguebyte works with.
//!
//! Everything here runs on the audio thread and allocates only in `new`.

use std::sync::Arc;
use std::time::Duration;

use cpal::{FromSample, Sample};
use manguebyte_core::{AudioBuffer, NoDenormals};

use crate::timing::{CallbackInfo, StreamHealth};

/// Feeds an output device from a planar `f32` callback, splitting oversized
/// requests into blocks.
pub(crate) struct BlockRenderer<F> {
    scratch: AudioBuffer<f32>,
    callback: F,
    clock: BlockClock,
}

impl<F: FnMut(&mut AudioBuffer<f32>, &CallbackInfo)> BlockRenderer<F> {
    pub(crate) fn new(
        channels: usize,
        max_block_size: usize,
        clock: BlockClock,
        callback: F,
    ) -> Self {
        Self {
            scratch: AudioBuffer::new(channels, max_block_size),
            callback,
            clock,
        }
    }

    /// Fills `data`, which the device will start playing `latency` after
    /// `stream_time`.
    pub(crate) fn render<T: Sample + FromSample<f32>>(
        &mut self,
        data: &mut [T],
        stream_time: Duration,
        latency: Option<Duration>,
    ) {
        // A fading sound must not slow the audio thread down.
        let _quiet = NoDenormals::new();
        let channels = self.scratch.num_channels();
        if channels == 0 {
            return;
        }
        let mut info = self.clock.info(stream_time);
        info.output_latency = latency;
        for chunk in data.chunks_mut(self.scratch.capacity() * channels) {
            let frames = chunk.len() / channels;
            self.scratch.set_num_frames(frames);
            self.scratch.clear();
            (self.callback)(&mut self.scratch, &info);
            interleave(&self.scratch, chunk);
            info = info.advanced(frames);
        }
        self.clock.finish(&info);
    }
}

/// Hands an input device's audio to a planar `f32` callback, splitting
/// oversized deliveries into blocks.
pub(crate) struct BlockCapturer<F> {
    scratch: AudioBuffer<f32>,
    callback: F,
    clock: BlockClock,
}

impl<F: FnMut(&AudioBuffer<f32>, &CallbackInfo)> BlockCapturer<F> {
    pub(crate) fn new(
        channels: usize,
        max_block_size: usize,
        clock: BlockClock,
        callback: F,
    ) -> Self {
        Self {
            scratch: AudioBuffer::new(channels, max_block_size),
            callback,
            clock,
        }
    }

    /// Hands over `data`, which was captured `latency` before `stream_time`.
    pub(crate) fn capture<T: Sample>(
        &mut self,
        data: &[T],
        stream_time: Duration,
        latency: Option<Duration>,
    ) where
        f32: FromSample<T>,
    {
        let _quiet = NoDenormals::new();
        let channels = self.scratch.num_channels();
        if channels == 0 {
            return;
        }
        let mut info = self.clock.info(stream_time);
        info.input_latency = latency;
        for chunk in data.chunks(self.scratch.capacity() * channels) {
            let frames = chunk.len() / channels;
            self.scratch.set_num_frames(frames);
            deinterleave(chunk, &mut self.scratch);
            (self.callback)(&self.scratch, &info);
            info = info.advanced(frames);
        }
        self.clock.finish(&info);
    }
}

/// Counts a stream's frames for its callbacks and for whoever watches it.
pub(crate) struct BlockClock {
    frame: u64,
    sample_rate: f64,
    health: Arc<StreamHealth>,
}

impl BlockClock {
    pub(crate) fn new(sample_rate: f64, health: Arc<StreamHealth>) -> Self {
        Self {
            frame: 0,
            sample_rate,
            health,
        }
    }

    /// Timing for a block starting now, without latencies.
    pub(crate) fn info(&self, stream_time: Duration) -> CallbackInfo {
        CallbackInfo {
            frame: self.frame,
            sample_rate: self.sample_rate,
            stream_time,
            output_latency: None,
            input_latency: None,
        }
    }

    /// Moves on to where `info`, the timing after the last block, points.
    pub(crate) fn finish(&mut self, info: &CallbackInfo) {
        self.health.advance((info.frame - self.frame) as usize);
        self.frame = info.frame;
    }
}

/// Writes `buffer` into `out` as interleaved frames in the device format.
pub(crate) fn interleave<T: Sample + FromSample<f32>>(buffer: &AudioBuffer<f32>, out: &mut [T]) {
    let channels = buffer.num_channels();
    for (index, channel) in buffer.iter_channels().enumerate() {
        for (slot, &sample) in out[index..].iter_mut().step_by(channels).zip(channel) {
            *slot = T::from_sample(sample);
        }
    }
}

/// Reads interleaved device-format frames from `data` into `buffer`.
pub(crate) fn deinterleave<T: Sample>(data: &[T], buffer: &mut AudioBuffer<f32>)
where
    f32: FromSample<T>,
{
    let channels = buffer.num_channels();
    for (index, channel) in buffer.iter_channels_mut().enumerate() {
        for (sample, &value) in channel
            .iter_mut()
            .zip(data[index..].iter().step_by(channels))
        {
            *sample = f32::from_sample(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock() -> BlockClock {
        BlockClock::new(1_000.0, StreamHealth::new())
    }

    #[test]
    fn renderer_interleaves_and_splits_into_blocks() {
        let mut blocks = Vec::new();
        let mut renderer = BlockRenderer::new(
            2,
            4,
            clock(),
            |buffer: &mut AudioBuffer<f32>, info: &CallbackInfo| {
                blocks.push((buffer.num_frames(), info.frame, info.stream_time));
                buffer.channel_mut(0).fill(0.25);
                buffer.channel_mut(1).fill(-0.5);
            },
        );

        let mut data = [9.0f32; 20];
        renderer.render(&mut data, Duration::from_secs(1), None);
        renderer.render(&mut data[..4], Duration::from_secs(2), None);
        drop(renderer);

        // The pieces of one delivery follow each other in frames and in
        // time; the next delivery carries on counting frames.
        let ms = Duration::from_millis;
        assert_eq!(
            blocks,
            vec![
                (4, 0, ms(1_000)),
                (4, 4, ms(1_004)),
                (2, 8, ms(1_008)),
                (2, 10, ms(2_000)),
            ]
        );
        for frame in data.chunks_exact(2) {
            assert_eq!(frame, &[0.25, -0.5]);
        }
    }

    #[test]
    fn renderer_hands_the_callback_a_silent_buffer_every_time() {
        let mut renderer = BlockRenderer::new(
            1,
            8,
            clock(),
            |buffer: &mut AudioBuffer<f32>, _: &CallbackInfo| {
                assert_eq!(buffer.peak(), 0.0);
                buffer.channel_mut(0).fill(1.0);
            },
        );
        let mut data = [0.0f32; 8];
        renderer.render(&mut data, Duration::ZERO, None);
        renderer.render(&mut data, Duration::ZERO, None);
        assert_eq!(data, [1.0; 8]);
    }

    #[test]
    fn renderer_reports_latency_and_progress() {
        let health = StreamHealth::new();
        let mut seen = None;
        let mut renderer = BlockRenderer::new(
            1,
            8,
            BlockClock::new(48_000.0, Arc::clone(&health)),
            |_: &mut AudioBuffer<f32>, info: &CallbackInfo| seen = Some(*info),
        );
        let mut data = [0.0f32; 6];
        renderer.render(&mut data, Duration::ZERO, Some(Duration::from_millis(3)));
        drop(renderer);
        assert_eq!(seen.unwrap().output_latency_frames(), Some(144.0));
        assert_eq!(health.frames(), 6);
    }

    #[test]
    fn renderer_converts_to_the_device_sample_format() {
        let mut renderer = BlockRenderer::new(
            1,
            4,
            clock(),
            |buffer: &mut AudioBuffer<f32>, _: &CallbackInfo| {
                buffer
                    .channel_mut(0)
                    .copy_from_slice(&[0.0, 0.5, -1.0, 1.0]);
            },
        );

        let mut signed = [0i16; 4];
        renderer.render(&mut signed, Duration::ZERO, None);
        assert_eq!(signed[0], 0);
        assert!((signed[1] - 16_384).abs() <= 1);
        assert_eq!(signed[2], i16::MIN);
        assert_eq!(signed[3], i16::MAX);

        let mut unsigned = [0u16; 4];
        renderer.render(&mut unsigned, Duration::ZERO, None);
        assert_eq!(unsigned[0], 32_768);
        assert_eq!(unsigned[2], 0);
    }

    #[test]
    fn capturer_deinterleaves_converts_and_splits() {
        let mut blocks: Vec<(Vec<f32>, Vec<f32>, u64)> = Vec::new();
        let mut capturer = BlockCapturer::new(
            2,
            2,
            clock(),
            |buffer: &AudioBuffer<f32>, info: &CallbackInfo| {
                assert_eq!(info.input_latency, Some(Duration::from_millis(1)));
                blocks.push((
                    buffer.channel(0).to_vec(),
                    buffer.channel(1).to_vec(),
                    info.frame,
                ));
            },
        );

        let data: [i16; 6] = [0, i16::MIN, 16_384, -16_384, i16::MAX, 0];
        capturer.capture(&data, Duration::ZERO, Some(Duration::from_millis(1)));
        drop(capturer);

        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].0, vec![0.0, 0.5]);
        assert_eq!(blocks[0].1, vec![-1.0, -0.5]);
        assert_eq!(blocks[1].0.len(), 1);
        assert!((blocks[1].0[0] - 1.0).abs() < 1e-4);
        assert_eq!(blocks[1].1, vec![0.0]);
        assert_eq!((blocks[0].2, blocks[1].2), (0, 2));
    }
}
