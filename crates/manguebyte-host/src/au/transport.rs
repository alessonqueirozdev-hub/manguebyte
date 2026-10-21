//! The host's transport, as a unit asks for it through the callbacks of
//! `kAudioUnitProperty_HostCallbacks` (`AudioUnitProperties.h`,
//! `HostCallbackInfo`): beat and tempo, where the music is in its bar, and
//! whether it is playing.
//!
//! The processor writes the transport of each slice into a
//! [`HostTransport`] just before it renders the slice, and the callbacks,
//! which the unit calls from inside its render, read it back. A unit may
//! call them from another thread too, so every field is an atomic, and
//! nothing is ever waited for.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use manguebyte_dsp::Transport;

use super::sys::*;
use crate::threads::guard;

/// What a unit is told when it asks for something the host does not know.
const UNKNOWN: OSStatus = kAudioUnitErr_CannotDoInCurrentContext;

/// The transport of the slice being rendered, as the callbacks answer it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Answers {
    pub(crate) transport: Transport,
    pub(crate) sample_rate: f64,
    /// Whether the transport moved other than by playing on since the last
    /// slice: it started, stopped, or jumped.
    pub(crate) changed: bool,
}

/// `HostCallback_GetBeatAndTempo`: the beat the slice starts on and the
/// tempo, if both are known.
pub(crate) fn beat_and_tempo(answers: &Answers) -> Option<(f64, f64)> {
    let transport = &answers.transport;
    Some((transport.position_beats?, transport.tempo_bpm?))
}

/// `HostCallback_GetMusicalTimeLocation`: how many frames there are to the
/// next beat, the time signature, and the beat the bar started on. Known
/// once the time signature is.
pub(crate) fn musical_time(answers: &Answers) -> Option<(u32, f32, u32, f64)> {
    let transport = &answers.transport;
    let (numerator, denominator) = transport.time_signature?;
    let bar = f64::from(numerator) * 4.0 / f64::from(denominator.max(1));
    let downbeat = match (transport.bar_start_beats, transport.position_beats) {
        (Some(start), _) => start,
        (None, Some(beat)) if bar > 0.0 => (beat / bar).floor() * bar,
        _ => return None,
    };
    let to_next_beat = match (transport.position_beats, transport.tempo_bpm) {
        (Some(beat), Some(tempo)) if tempo > 0.0 && answers.sample_rate > 0.0 => {
            let left = beat.ceil() - beat;
            let frames = (left * 60.0 / tempo * answers.sample_rate).round();
            if frames.is_finite() {
                frames.clamp(0.0, f64::from(u32::MAX)) as u32
            } else {
                0
            }
        }
        _ => 0,
    };
    Some((
        to_next_beat,
        f32::from(numerator),
        u32::from(denominator),
        downbeat,
    ))
}

/// What `HostCallback_GetTransportState2` answers: playing, recording,
/// changed, the frame on the timeline, cycling, and the cycle's ends in
/// beats.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct State {
    pub(crate) playing: bool,
    pub(crate) recording: bool,
    pub(crate) changed: bool,
    pub(crate) sample: f64,
    pub(crate) cycling: bool,
    pub(crate) cycle: (f64, f64),
}

pub(crate) fn state(answers: &Answers) -> State {
    let transport = &answers.transport;
    let sample = match (transport.position_samples, transport.position_seconds) {
        (Some(frames), _) => frames as f64,
        (None, Some(seconds)) => seconds * answers.sample_rate,
        (None, None) => 0.0,
    };
    let cycle = transport.loop_beats;
    State {
        playing: transport.playing,
        recording: transport.recording,
        changed: answers.changed,
        sample,
        cycling: cycle.is_some(),
        cycle: cycle.unwrap_or((0.0, 0.0)),
    }
}

/// The transport the processor last wrote, for the callbacks to read.
#[derive(Debug, Default)]
pub(crate) struct HostTransport {
    /// Each `Option<f64>` field as its bits, with this pattern for `None`.
    tempo: AtomicU64,
    beat: AtomicU64,
    bar_start: AtomicU64,
    sample: AtomicU64,
    seconds: AtomicU64,
    loop_start: AtomicU64,
    loop_end: AtomicU64,
    sample_rate: AtomicU64,
    /// Numerator in the high half, denominator in the low; 0 for none.
    signature: AtomicU32,
    /// Bit 0 playing, 1 recording, 2 changed, 3 looping, 4 the position in
    /// frames is known.
    flags: AtomicU32,
}

/// The bits that stand for no value.
const NONE: u64 = 0x7FF8_DEAD_BEEF_0001;

fn store(cell: &AtomicU64, value: Option<f64>) {
    let bits = value.map_or(NONE, f64::to_bits);
    // A NaN the transport holds is no value either.
    let bits = if bits != NONE && f64::from_bits(bits).is_nan() {
        NONE
    } else {
        bits
    };
    cell.store(bits, Ordering::Relaxed);
}

fn load(cell: &AtomicU64) -> Option<f64> {
    let bits = cell.load(Ordering::Relaxed);
    (bits != NONE).then(|| f64::from_bits(bits))
}

impl HostTransport {
    pub(crate) fn new() -> Self {
        let this = Self::default();
        this.write(&Answers {
            transport: Transport::STOPPED,
            sample_rate: 0.0,
            changed: false,
        });
        this
    }

    /// Writes the transport of the next slice. Real-time safe.
    pub(crate) fn write(&self, answers: &Answers) {
        let transport = &answers.transport;
        store(&self.tempo, transport.tempo_bpm);
        store(&self.beat, transport.position_beats);
        store(&self.bar_start, transport.bar_start_beats);
        store(
            &self.sample,
            transport.position_samples.map(|frames| frames as f64),
        );
        store(&self.seconds, transport.position_seconds);
        store(
            &self.loop_start,
            transport.loop_beats.map(|(start, _)| start),
        );
        store(&self.loop_end, transport.loop_beats.map(|(_, end)| end));
        store(&self.sample_rate, Some(answers.sample_rate));
        let signature = transport
            .time_signature
            .map_or(0, |(numerator, denominator)| {
                (u32::from(numerator) << 16) | u32::from(denominator)
            });
        self.signature.store(signature, Ordering::Relaxed);
        let flags = u32::from(transport.playing)
            | (u32::from(transport.recording) << 1)
            | (u32::from(answers.changed) << 2)
            | (u32::from(transport.loop_beats.is_some()) << 3)
            | (u32::from(transport.position_samples.is_some()) << 4);
        self.flags.store(flags, Ordering::Relaxed);
    }

    /// What was written last.
    pub(crate) fn read(&self) -> Answers {
        let flags = self.flags.load(Ordering::Relaxed);
        let signature = self.signature.load(Ordering::Relaxed);
        let loop_beats = match (load(&self.loop_start), load(&self.loop_end)) {
            (Some(start), Some(end)) if flags & 8 != 0 => Some((start, end)),
            _ => None,
        };
        let transport = Transport {
            playing: flags & 1 != 0,
            recording: flags & 2 != 0,
            tempo_bpm: load(&self.tempo),
            time_signature: (signature != 0)
                .then_some(((signature >> 16) as u16, (signature & 0xFFFF) as u16)),
            position_samples: load(&self.sample)
                .filter(|_| flags & 16 != 0)
                .map(|frames| frames as i64),
            position_beats: load(&self.beat),
            bar_start_beats: load(&self.bar_start),
            loop_beats,
            position_seconds: load(&self.seconds),
            ..Transport::STOPPED
        };
        Answers {
            transport,
            sample_rate: load(&self.sample_rate).unwrap_or(0.0),
            changed: flags & 4 != 0,
        }
    }

    /// The callbacks, with this value as their data.
    ///
    /// The value must outlive every unit it is given to.
    pub(crate) fn callbacks(&self) -> HostCallbackInfo {
        HostCallbackInfo {
            hostUserData: std::ptr::from_ref(self).cast_mut().cast(),
            beatAndTempoProc: Some(beat_and_tempo_proc),
            musicalTimeLocationProc: Some(musical_time_proc),
            transportStateProc: Some(transport_state_proc),
            transportStateProc2: Some(transport_state_proc2),
        }
    }
}

/// Writes `value` where the unit asked, unless it passed null, which a unit
/// does for what it does not want.
///
/// # Safety
///
/// `out` must be null or writable.
unsafe fn put<T>(out: *mut T, value: T) {
    if !out.is_null() {
        // SAFETY: per this function's contract.
        unsafe { out.write_unaligned(value) };
    }
}

/// # Safety
///
/// `user` must be the `HostTransport` the callbacks were made with.
unsafe fn answers(user: *mut c_void) -> Option<Answers> {
    // SAFETY: per this function's contract.
    let transport: Option<&HostTransport> = unsafe { user.cast::<HostTransport>().as_ref() };
    transport.map(HostTransport::read)
}

unsafe extern "C" fn beat_and_tempo_proc(
    user: *mut c_void,
    beat: *mut f64,
    tempo: *mut f64,
) -> OSStatus {
    guard(UNKNOWN, || {
        // SAFETY: the data the callbacks were given.
        let Some(found) = (unsafe { answers(user) }).as_ref().and_then(beat_and_tempo) else {
            return UNKNOWN;
        };
        // SAFETY: the unit passes places for its answers, or null.
        unsafe {
            put(beat, found.0);
            put(tempo, found.1);
        }
        noErr
    })
}

unsafe extern "C" fn musical_time_proc(
    user: *mut c_void,
    to_next_beat: *mut u32,
    numerator: *mut f32,
    denominator: *mut u32,
    downbeat: *mut f64,
) -> OSStatus {
    guard(UNKNOWN, || {
        // SAFETY: the data the callbacks were given.
        let Some(found) = (unsafe { answers(user) }).as_ref().and_then(musical_time) else {
            return UNKNOWN;
        };
        // SAFETY: as above.
        unsafe {
            put(to_next_beat, found.0);
            put(numerator, found.1);
            put(denominator, found.2);
            put(downbeat, found.3);
        }
        noErr
    })
}

/// Answers both transport-state callbacks; the older has no recording.
///
/// # Safety
///
/// As for the callbacks.
#[allow(clippy::too_many_arguments)]
unsafe fn answer_state(
    user: *mut c_void,
    playing: *mut Boolean,
    recording: *mut Boolean,
    changed: *mut Boolean,
    sample: *mut f64,
    cycling: *mut Boolean,
    cycle_start: *mut f64,
    cycle_end: *mut f64,
) -> OSStatus {
    guard(UNKNOWN, || {
        // SAFETY: the data the callbacks were given.
        let Some(found) = (unsafe { answers(user) }) else {
            return UNKNOWN;
        };
        let found = state(&found);
        // SAFETY: as above.
        unsafe {
            put(playing, Boolean::from(found.playing));
            put(recording, Boolean::from(found.recording));
            put(changed, Boolean::from(found.changed));
            put(sample, found.sample);
            put(cycling, Boolean::from(found.cycling));
            put(cycle_start, found.cycle.0);
            put(cycle_end, found.cycle.1);
        }
        noErr
    })
}

unsafe extern "C" fn transport_state_proc(
    user: *mut c_void,
    playing: *mut Boolean,
    changed: *mut Boolean,
    sample: *mut f64,
    cycling: *mut Boolean,
    cycle_start: *mut f64,
    cycle_end: *mut f64,
) -> OSStatus {
    // SAFETY: passed on, with no place for the recording state.
    unsafe {
        answer_state(
            user,
            playing,
            std::ptr::null_mut(),
            changed,
            sample,
            cycling,
            cycle_start,
            cycle_end,
        )
    }
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn transport_state_proc2(
    user: *mut c_void,
    playing: *mut Boolean,
    recording: *mut Boolean,
    changed: *mut Boolean,
    sample: *mut f64,
    cycling: *mut Boolean,
    cycle_start: *mut f64,
    cycle_end: *mut f64,
) -> OSStatus {
    // SAFETY: passed on.
    unsafe {
        answer_state(
            user,
            playing,
            recording,
            changed,
            sample,
            cycling,
            cycle_start,
            cycle_end,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(transport: Transport) -> Answers {
        Answers {
            transport,
            sample_rate: 48_000.0,
            changed: false,
        }
    }

    #[test]
    fn the_callbacks_answer_from_the_transport() {
        let mut transport = Transport::playing_at(120.0);
        transport.position_beats = Some(5.25);
        transport.bar_start_beats = Some(4.0);
        transport.time_signature = Some((6, 8));
        transport.position_samples = Some(126_000);
        transport.recording = true;
        transport.loop_beats = Some((4.0, 12.0));
        let answers = at(transport);
        assert_eq!(beat_and_tempo(&answers), Some((5.25, 120.0)));
        // Three quarters of a beat at 120 bpm is 0.375 s.
        assert_eq!(musical_time(&answers), Some((18_000, 6.0, 8, 4.0)));
        assert_eq!(
            state(&answers),
            State {
                playing: true,
                recording: true,
                changed: false,
                sample: 126_000.0,
                cycling: true,
                cycle: (4.0, 12.0),
            }
        );

        // What is not known is not made up.
        let stopped = at(Transport::STOPPED);
        assert_eq!(beat_and_tempo(&stopped), None);
        assert_eq!(musical_time(&stopped), None);
        assert!(!state(&stopped).playing && !state(&stopped).cycling);
        // A bar's start worked out from the beat when not given.
        let mut free = Transport::playing_at(90.0);
        free.position_beats = Some(9.0);
        free.bar_start_beats = None;
        assert_eq!(musical_time(&at(free)), Some((0, 4.0, 4, 8.0)));
    }

    #[test]
    fn what_is_written_is_read_back() {
        let shared = HostTransport::new();
        assert_eq!(shared.read().transport, Transport::STOPPED);
        let mut transport = Transport::playing_at(133.0);
        transport.loop_beats = Some((1.0, 3.0));
        transport.position_samples = Some(-480);
        transport.position_seconds = Some(-0.01);
        transport.tempo_bpm = Some(f64::NAN);
        let written = Answers {
            transport,
            sample_rate: 44_100.0,
            changed: true,
        };
        shared.write(&written);
        let read = shared.read();
        assert_eq!(read.transport.tempo_bpm, None, "a NaN is no tempo");
        assert_eq!(
            read.transport,
            Transport {
                tempo_bpm: None,
                ..transport
            }
        );
        assert_eq!((read.sample_rate, read.changed), (44_100.0, true));
    }

    #[test]
    fn the_c_callbacks_fill_in_what_is_asked_for() {
        let shared = HostTransport::new();
        let mut transport = Transport::playing_at(120.0);
        transport.position_beats = Some(2.5);
        shared.write(&Answers {
            transport,
            sample_rate: 48_000.0,
            changed: true,
        });
        let callbacks = shared.callbacks();
        let user = callbacks.hostUserData;
        let (mut beat, mut tempo) = (0.0, 0.0);
        let (mut playing, mut recording, mut changed, mut cycling) = (0u8, 9u8, 0u8, 9u8);
        let mut sample = -1.0;
        // SAFETY: the callbacks with their own data and places for the
        // answers, or null where a unit wants nothing.
        unsafe {
            assert_eq!(
                callbacks.beatAndTempoProc.unwrap()(user, &mut beat, &mut tempo),
                noErr
            );
            assert_eq!(
                callbacks.beatAndTempoProc.unwrap()(user, std::ptr::null_mut(), &mut tempo),
                noErr
            );
            let mut next = 0;
            assert_eq!(
                callbacks.musicalTimeLocationProc.unwrap()(
                    user,
                    &mut next,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut()
                ),
                noErr
            );
            assert_eq!(next, 12_000);
            assert_eq!(
                callbacks.transportStateProc2.unwrap()(
                    user,
                    &mut playing,
                    &mut recording,
                    &mut changed,
                    &mut sample,
                    &mut cycling,
                    std::ptr::null_mut(),
                    std::ptr::null_mut()
                ),
                noErr
            );
        }
        assert_eq!((beat, tempo), (2.5, 120.0));
        assert_eq!((playing, recording, changed, cycling), (1, 0, 1, 0));
        assert_eq!(sample, 0.0);
        shared.write(&at(Transport::STOPPED));
        // SAFETY: as above.
        let status = unsafe { callbacks.beatAndTempoProc.unwrap()(user, &mut beat, &mut tempo) };
        assert_eq!(status, UNKNOWN);
    }
}
