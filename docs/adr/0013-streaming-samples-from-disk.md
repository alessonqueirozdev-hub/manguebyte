# ADR 0013: Streaming samples from disk

Status: accepted, 2026-10-29

## Context

Until now `manguebyte-sampler` decoded every sample whole into memory.
That is right for drum kits and small instruments and wrong for the
products the framework is built for: Caecilia 2.0, a pipe organ, has every
pipe of every rank with several attacks, a loop and several releases, tens
of gigabytes in all; Parabolic's exported instruments can be as large.
Commercial samplers keep the start of each sample in memory and read the
rest from disk while it plays.

The constraints come from ADR 0002 and 0003: the audio thread may not
allocate, free, lock, wait or make a system call, and talks to other
threads through atomics and the wait-free queue only. A voice must start
on the frame its note arrives, so nothing it plays first can wait for a
disk. And what plays must not change because samples are streamed: with a
disk that keeps up, the output has to be the same bits as from memory, so
that every test of the sampler keeps its meaning.

Caecilia plays its pipes with voices of its own, outside the SFZ sampler,
so the layer has to be usable without an `Instrument`.

## Decision

**1. A sample is a set of stretches in memory and, optionally, a file.**
`SampleData` holds `parts` (each a first frame and its channels) and the
sample's length. A sample loaded whole is one part from frame 0. A
streamed sample keeps its head (the first `preload_frames`, default
65 536, from the earliest `offset` of the regions that play it) and every
loop its regions play, with the frames the loop's crossfade and the
interpolator read round it (256 frames each side, more than the widest
stretched sinc kernel reaches). Loops are never streamed: a sample whose
loop is longer than `loop_cache_frames` is kept whole. Only WAV, AIFF and
FLAC are streamed, since `AudioReader` seeks in them exactly and cheaply;
other formats, and a FLAC file that does not state its length, are kept
whole.

**2. One ring per voice, owned by a pool, coordinated by atomics.** A
`Pool` (made at load, sized by `StreamOptions::voices` and `ring_frames`)
holds one `Slot` per voice: a ring of `AtomicU32` per channel (an `f32`'s
bits; the crate keeps `#![forbid(unsafe_code)]`), and four atomics. The
voice publishes a *request* (a 16-bit generation above a 48-bit first
frame, one `u64`, so it cannot be read torn) and how far it has read (the
lowest frame it may still need); the reader publishes its *progress*, the
same generation above the frame after the last one written. The reader
writes a frame only when it is less than a ring's length past what the
voice may still need, and publishes it with `Release` after writing; the
voice reads it only after an `Acquire` of a progress with its own
generation. A voice that changes its request (a new generation) can never
read a frame written for the old one. A voice claims a slot with a
compare-and-swap and gives it back when it stops (the slot is an `Arc`
the pool keeps, so a voice's drop never frees).

**3. The voice asks when it starts and never waits.** A voice asks for the
frames after the stretch it starts in as it starts. Each time it renders
it takes note of the progress once, and reads a frame from a part, or from
the ring if it has arrived. If not, it counts an underrun (an atomic the
program polls: `Instrument::stream_underruns`), holds the last values it
read and fades out over 6 ms: no step larger than the signal's own, never
silence read as data. A looping voice that has gone into a loop kept in
memory asks, from then on, for the frames after that stretch, so that its
release plays on from the disk without a gap.

**4. Reader threads poll.** The audio thread never wakes a reader (that
would be a system call), so each reader sleeps `poll_interval` (1 ms by
default) when it has nothing to do, and otherwise serves first the voice
with the least read ahead, 4096 frames at a time. A reader keeps up to 64
files open, closing the one used longest ago. Their number is
`reader_threads`; voices are shared out among them. Their priority is the
program's: `on_reader_start` runs on each thread when it starts, where a
program raises it with whatever system interface it already uses. This
crate depends on no system crate and makes no such call. The threads stop,
and are joined, when the pool is dropped, with the last clone of the
instrument.

**5. One layer, two users.** `SampleStore` opens a list of files with the
same layout, pool and readers, and hands out `StreamCursor`s: one voice's
sequential reader, `read(left, right) -> CursorRead::Frames(n) |
Underrun(n)`, and `seek`, all real-time safe. The sampler's voices read
frames at random positions round their read position through the same
function a cursor uses (`store::frame`), so both paths test one
implementation.

**6. A budget for memory.** `StreamOptions::ram_budget` bounds the bytes
of heads, loops and samples kept whole. When the preload asked for does
not fit, every head is shortened alike to the longest that fits (found by
bisection over the layout, which grows with the head), but not below 4096
frames; loading fails with what it would need if even that does not fit.

**7. Offline rendering waits.** `Instrument::wait_for_streams` and
`SampleStore::wait_for_streams` block until every ring is full or holds
the rest of its file: a bounce faster than real time calls it between
blocks and gets what real time with a disk that keeps up would give. The
tests use it to prove the output the same bits as from memory.

## Consequences

- With the disk keeping up, streamed instruments play the same bits as
  instruments in memory, at every block size and with any number of
  reader threads (`tests/streaming.rs`), and 256 voices stream at once.
- Memory per voice is a ring (`ring_frames` × channels × 4 bytes, 512 KiB
  for a stereo voice by default) beside the heads; the tests report what
  each mode holds.
- A slow disk is heard as notes that fade early, never as a click, and is
  counted.
- The audio thread does not depend on a reader being scheduled, but a
  voice's first read waits up to `poll_interval` to start, which the
  preload covers many times over.
- Loops always cost their full length in memory; long-looped samples are
  not streamed at all.
- `SamplerVoice` streams too, since the pool is the instrument's.

## Alternatives not taken

- **Waking the reader from the audio thread** (a semaphore or
  `Thread::unpark`): faster to start, but a system call on the audio
  thread, which ADR 0002 rules out.
- **Rings in the `Sampler`, sized in `prepare`**: the frames a voice reads
  per second depend on the sample's rate and its pitch, not on the output
  rate, so `prepare` knows nothing the options do not; a pool owned by the
  instrument also serves `SamplerVoice` and a `SampleStore`, and dies with
  the files it reads.
- **`unsafe` rings** (`UnsafeCell<f32>` slices, as `manguebyte-core`'s
  queue): no faster on the processors we target, where a relaxed atomic
  load or store of 32 bits is a plain move, and they would cost the crate
  its `forbid(unsafe_code)`.
- **Memory-mapped files**: a page fault on the audio thread is a wait on
  the disk.
- **Streaming loops through the ring** (unrolling the loop in the reader):
  a released sustain loop would have to change its request at the moment
  it leaves, and wait for the disk there.
