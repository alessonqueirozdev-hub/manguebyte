//! Noticing devices that come and go, and streams whose device went away.
//!
//! None of the backends this crate drives tells it when a device is
//! plugged in or out: `cpal` 0.18 has no such notification on any system,
//! and `midir` none for MIDI ports. So the list of devices is polled, by
//! default every [`DEFAULT_POLL_INTERVAL`], and each poll is compared with
//! the one before. Listing is cheap (identifiers and names only, no device
//! is opened), and it happens on the watcher's own thread, or on whichever
//! thread calls [`DeviceManager::poll_devices`], never on the audio thread.
//! An application that hears about a change sooner, from the system's own
//! notification in its window loop for example, can ask for a poll at once
//! with [`DeviceWatcher::refresh`].
//!
//! A stream whose device vanishes is not left hanging: the poll notices
//! that the device is no longer listed, that the backend reported it lost,
//! or that its callback has stopped being called, and reports
//! [`DeviceEvent::StreamLost`].
//!
//! [`DeviceManager::poll_devices`]: crate::DeviceManager::poll_devices

use std::fmt;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::backend::{Backend, SystemBackend};
use crate::timing::StreamHealth;
use crate::{AudioHost, Direction};

/// How often a [`DeviceWatcher`] lists the devices unless told otherwise.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// The shortest interval a [`DeviceWatcher`] accepts.
pub const MIN_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long a running stream's callback may go uncalled before the stream
/// is reported lost.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(2);

/// The identifier and name of a device or MIDI port.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceSummary {
    /// Stable identifier, as in [`DeviceInfo::id`](crate::DeviceInfo::id).
    pub id: String,
    /// Human-readable name.
    pub name: String,
}

/// Everything connected at one moment: audio devices, which of them are
/// the defaults, and MIDI ports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceSnapshot {
    /// Audio output devices.
    pub outputs: Vec<DeviceSummary>,
    /// Audio input devices.
    pub inputs: Vec<DeviceSummary>,
    /// Identifier of the default output device.
    pub default_output: Option<String>,
    /// Identifier of the default input device.
    pub default_input: Option<String>,
    /// MIDI ports that can be received from.
    pub midi_inputs: Vec<DeviceSummary>,
    /// MIDI ports that can be sent to.
    pub midi_outputs: Vec<DeviceSummary>,
}

impl DeviceSnapshot {
    /// The audio devices of one direction.
    pub fn devices(&self, direction: Direction) -> &[DeviceSummary] {
        match direction {
            Direction::Input => &self.inputs,
            Direction::Output => &self.outputs,
        }
    }

    /// The default audio device of one direction, if it is listed.
    pub fn default_device(&self, direction: Direction) -> Option<&DeviceSummary> {
        let id = match direction {
            Direction::Input => self.default_input.as_ref(),
            Direction::Output => self.default_output.as_ref(),
        }?;
        self.devices(direction)
            .iter()
            .find(|device| &device.id == id)
    }

    /// The MIDI ports of one direction.
    pub fn midi_ports(&self, direction: Direction) -> &[DeviceSummary] {
        match direction {
            Direction::Input => &self.midi_inputs,
            Direction::Output => &self.midi_outputs,
        }
    }

    /// What changed from `earlier` to this snapshot, removals first.
    pub fn changes_since(&self, earlier: &DeviceSnapshot) -> Vec<DeviceEvent> {
        let mut events = Vec::new();
        let differences = |old: &[DeviceSummary], new: &[DeviceSummary]| {
            let removed: Vec<DeviceSummary> = old
                .iter()
                .filter(|device| !new.iter().any(|other| other.id == device.id))
                .cloned()
                .collect();
            let added: Vec<DeviceSummary> = new
                .iter()
                .filter(|device| !old.iter().any(|other| other.id == device.id))
                .cloned()
                .collect();
            (removed, added)
        };
        for direction in [Direction::Output, Direction::Input] {
            let (removed, added) = differences(earlier.devices(direction), self.devices(direction));
            events.extend(
                removed
                    .into_iter()
                    .map(|device| DeviceEvent::DeviceRemoved { direction, device }),
            );
            events.extend(
                added
                    .into_iter()
                    .map(|device| DeviceEvent::DeviceAdded { direction, device }),
            );
            let old_default = earlier.default_device(direction);
            let new_default = self.default_device(direction);
            if old_default.map(|d| &d.id) != new_default.map(|d| &d.id) {
                events.push(DeviceEvent::DefaultChanged {
                    direction,
                    device: new_default.cloned(),
                });
            }
        }
        for direction in [Direction::Input, Direction::Output] {
            let (removed, added) =
                differences(earlier.midi_ports(direction), self.midi_ports(direction));
            events.extend(
                removed
                    .into_iter()
                    .map(|port| DeviceEvent::MidiPortRemoved { direction, port }),
            );
            events.extend(
                added
                    .into_iter()
                    .map(|port| DeviceEvent::MidiPortAdded { direction, port }),
            );
        }
        events
    }
}

/// Why a running stream was reported lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LossCause {
    /// Its device is no longer listed: it was unplugged or switched off.
    Disconnected,
    /// The backend said the device went away.
    ReportedByBackend,
    /// Its callback has not been called for [`STALL_TIMEOUT`].
    Stalled,
    /// The backend asked for the stream to be opened again, as a driver
    /// does when its sample rate, buffer size or other settings change
    /// under it. Nothing is wrong with the device: a restart picks the new
    /// settings up.
    RestartRequested,
}

/// Something that changed among the devices, as a poll found it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeviceEvent {
    /// An audio device appeared.
    DeviceAdded {
        /// Which side.
        direction: Direction,
        /// The device.
        device: DeviceSummary,
    },
    /// An audio device disappeared.
    DeviceRemoved {
        /// Which side.
        direction: Direction,
        /// The device.
        device: DeviceSummary,
    },
    /// The system's default device changed.
    DefaultChanged {
        /// Which side.
        direction: Direction,
        /// The new default, or `None` when there is none.
        device: Option<DeviceSummary>,
    },
    /// A MIDI port appeared.
    MidiPortAdded {
        /// Which side.
        direction: Direction,
        /// The port.
        port: DeviceSummary,
    },
    /// A MIDI port disappeared.
    MidiPortRemoved {
        /// Which side.
        direction: Direction,
        /// The port.
        port: DeviceSummary,
    },
    /// The device a running stream plays to or records from has gone, and
    /// the stream will not produce audio again, or the backend asked for
    /// the stream to be opened again ([`LossCause::RestartRequested`]).
    /// Either way [`DeviceManager::follow`](crate::DeviceManager::follow)
    /// restarts it. Reported once per stream.
    StreamLost {
        /// Which side of the stream.
        direction: Direction,
        /// The device it was running on.
        device: DeviceSummary,
        /// How the loss was noticed.
        cause: LossCause,
    },
    /// The devices could not be listed. The next poll tries again.
    ListingFailed(String),
}

impl fmt::Display for DeviceEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeviceAdded { direction, device } => {
                write!(f, "{} device added: {}", direction.as_str(), device.name)
            }
            Self::DeviceRemoved { direction, device } => {
                write!(f, "{} device removed: {}", direction.as_str(), device.name)
            }
            Self::DefaultChanged { direction, device } => match device {
                Some(device) => write!(f, "default {} is now {}", direction.as_str(), device.name),
                None => write!(f, "there is no default {} any more", direction.as_str()),
            },
            Self::MidiPortAdded { direction, port } => {
                write!(f, "MIDI {} added: {}", direction.as_str(), port.name)
            }
            Self::MidiPortRemoved { direction, port } => {
                write!(f, "MIDI {} removed: {}", direction.as_str(), port.name)
            }
            Self::StreamLost {
                direction,
                device,
                cause,
            } => write!(
                f,
                "the {} stream on {} was lost ({cause:?})",
                direction.as_str(),
                device.name
            ),
            Self::ListingFailed(reason) => write!(f, "could not list the devices: {reason}"),
        }
    }
}

/// One running stream, as the pollers see it.
pub(crate) struct WatchedStream {
    /// Changes whenever another stream is started, so that a poller starts
    /// its stall timers again.
    pub(crate) generation: u64,
    pub(crate) endpoints: Vec<(Direction, DeviceSummary, Arc<StreamHealth>)>,
}

/// Where a device manager tells its pollers which stream runs.
pub(crate) type WatchSlot = Arc<Mutex<Option<WatchedStream>>>;

/// How far one endpoint of the watched stream has got.
struct Progress {
    frames: u64,
    since: Instant,
    reported: bool,
}

/// Lists the devices and turns the differences into events. Holds no
/// thread: [`DeviceWatcher`] runs one on its own, and a device manager
/// keeps one for [`poll_devices`](crate::DeviceManager::poll_devices).
pub(crate) struct Poller {
    backend: Arc<dyn Backend>,
    watched: WatchSlot,
    last: Option<DeviceSnapshot>,
    generation: Option<u64>,
    progress: Vec<Progress>,
}

impl Poller {
    pub(crate) fn new(backend: Arc<dyn Backend>, watched: WatchSlot) -> Self {
        Self {
            backend,
            watched,
            last: None,
            generation: None,
            progress: Vec::new(),
        }
    }

    /// Polls now. The first poll only learns what is there.
    pub(crate) fn poll(&mut self) -> Vec<DeviceEvent> {
        self.poll_at(Instant::now())
    }

    pub(crate) fn poll_at(&mut self, now: Instant) -> Vec<DeviceEvent> {
        let mut events = Vec::new();
        match self.backend.snapshot() {
            Ok(snapshot) => {
                if let Some(last) = &self.last {
                    events.extend(snapshot.changes_since(last));
                }
                self.last = Some(snapshot);
            }
            Err(error) => events.push(DeviceEvent::ListingFailed(error.to_string())),
        }

        let Ok(watched) = self.watched.lock() else {
            return events;
        };
        let Some(stream) = watched.as_ref() else {
            self.generation = None;
            return events;
        };
        if self.generation != Some(stream.generation) {
            self.generation = Some(stream.generation);
            self.progress = stream
                .endpoints
                .iter()
                .map(|(_, _, health)| Progress {
                    frames: health.frames(),
                    since: now,
                    reported: false,
                })
                .collect();
        }
        for ((direction, device, health), progress) in
            stream.endpoints.iter().zip(&mut self.progress)
        {
            if progress.reported {
                continue;
            }
            let listed = self.last.as_ref().is_none_or(|snapshot| {
                snapshot
                    .devices(*direction)
                    .iter()
                    .any(|other| other.id == device.id)
            });
            let frames = health.frames();
            let cause = if health.is_restart_requested() {
                Some(LossCause::RestartRequested)
            } else if health.is_lost() {
                Some(LossCause::ReportedByBackend)
            } else if !listed {
                Some(LossCause::Disconnected)
            } else if frames != progress.frames {
                progress.frames = frames;
                progress.since = now;
                None
            } else if now.saturating_duration_since(progress.since) >= STALL_TIMEOUT {
                Some(LossCause::Stalled)
            } else {
                None
            };
            if let Some(cause) = cause {
                progress.reported = true;
                events.push(DeviceEvent::StreamLost {
                    direction: *direction,
                    device: device.clone(),
                    cause,
                });
            }
        }
        events
    }
}

#[derive(Default)]
struct WatcherControl {
    stop: bool,
    refresh: bool,
}

#[derive(Default)]
struct WatcherShared {
    control: Mutex<WatcherControl>,
    wake: Condvar,
}

/// Lists the devices on a thread of its own, every so often, and tells a
/// handler what changed.
///
/// The handler runs on the watcher's thread, never on the audio thread.
/// It may take its time, but the next poll waits for it. To act on an
/// event on another thread, send it there through a channel.
///
/// The watcher stops when it is dropped.
///
/// ```no_run
/// use manguebyte_devices::{AudioHost, DEFAULT_POLL_INTERVAL, DeviceWatcher};
///
/// let _watcher = DeviceWatcher::start(AudioHost::default_host(), DEFAULT_POLL_INTERVAL, |event| {
///     println!("{event}");
/// })?;
/// std::thread::sleep(std::time::Duration::from_secs(30));
/// # Ok::<(), std::io::Error>(())
/// ```
pub struct DeviceWatcher {
    shared: Arc<WatcherShared>,
    thread: Option<JoinHandle<()>>,
    interval: Duration,
}

impl DeviceWatcher {
    /// Starts watching the devices of `host`, listing them every
    /// `interval` (at least [`MIN_POLL_INTERVAL`]).
    ///
    /// Fails only if the system cannot start a thread.
    pub fn start<H>(host: AudioHost, interval: Duration, handler: H) -> std::io::Result<Self>
    where
        H: FnMut(DeviceEvent) + Send + 'static,
    {
        let backend: Arc<dyn Backend> = Arc::new(SystemBackend::new(host));
        Self::spawn(
            Poller::new(backend, Arc::new(Mutex::new(None))),
            interval,
            handler,
        )
    }

    pub(crate) fn spawn<H>(
        mut poller: Poller,
        interval: Duration,
        mut handler: H,
    ) -> std::io::Result<Self>
    where
        H: FnMut(DeviceEvent) + Send + 'static,
    {
        let interval = interval.max(MIN_POLL_INTERVAL);
        let shared = Arc::new(WatcherShared::default());
        let thread = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name(String::from("manguebyte-device-watcher"))
                .spawn(move || {
                    loop {
                        for event in poller.poll() {
                            handler(event);
                        }
                        let Ok(mut control) = shared.control.lock() else {
                            return;
                        };
                        let deadline = Instant::now() + interval;
                        while !control.stop && !control.refresh {
                            let left = deadline.saturating_duration_since(Instant::now());
                            if left.is_zero() {
                                break;
                            }
                            control = match shared.wake.wait_timeout(control, left) {
                                Ok((control, _)) => control,
                                Err(_) => return,
                            };
                        }
                        if control.stop {
                            return;
                        }
                        control.refresh = false;
                    }
                })?
        };
        Ok(Self {
            shared,
            thread: Some(thread),
            interval,
        })
    }

    /// Polls now instead of at the end of the interval.
    pub fn refresh(&self) {
        if let Ok(mut control) = self.shared.control.lock() {
            control.refresh = true;
            self.shared.wake.notify_all();
        }
    }

    /// How often the watcher lists the devices.
    pub fn interval(&self) -> Duration {
        self.interval
    }
}

impl Drop for DeviceWatcher {
    fn drop(&mut self) {
        if let Ok(mut control) = self.shared.control.lock() {
            control.stop = true;
            self.shared.wake.notify_all();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(id: &str) -> DeviceSummary {
        DeviceSummary {
            id: id.into(),
            name: format!("Device {id}"),
        }
    }

    #[test]
    fn a_stream_whose_callback_stops_is_reported_once() {
        let mock = crate::MockBackend::new();
        mock.add_device(
            Direction::Output,
            crate::MockBackend::device("out", "Out", 2, 48_000),
        );
        let device = DeviceSummary {
            id: "out".into(),
            name: "Out".into(),
        };
        let health = StreamHealth::new();
        let stream = |generation| WatchedStream {
            generation,
            endpoints: vec![(Direction::Output, device.clone(), Arc::clone(&health))],
        };
        let watched: WatchSlot = Arc::new(Mutex::new(Some(stream(1))));
        let mut poller = Poller::new(Arc::new(mock), Arc::clone(&watched));
        let start = Instant::now();
        let at = |seconds: f64| start + Duration::from_secs_f64(seconds);

        assert_eq!(poller.poll_at(at(0.0)), []);
        health.advance(512);
        assert_eq!(poller.poll_at(at(1.5)), []);
        // 1.5 s without a callback is not yet a stall.
        assert_eq!(poller.poll_at(at(3.0)), []);
        assert_eq!(
            poller.poll_at(at(3.6)),
            [DeviceEvent::StreamLost {
                direction: Direction::Output,
                device: device.clone(),
                cause: LossCause::Stalled,
            }]
        );
        assert_eq!(poller.poll_at(at(10.0)), []);

        // A new stream starts the timers again.
        *watched.lock().unwrap() = Some(stream(2));
        assert_eq!(poller.poll_at(at(10.5)), []);
        assert_eq!(poller.poll_at(at(11.0)), []);
        *watched.lock().unwrap() = None;
        assert_eq!(poller.poll_at(at(20.0)), []);
    }

    #[test]
    fn differences_become_events() {
        let before = DeviceSnapshot {
            outputs: vec![summary("a"), summary("b")],
            inputs: vec![summary("mic")],
            default_output: Some("a".into()),
            default_input: Some("mic".into()),
            midi_inputs: vec![summary("keys")],
            midi_outputs: Vec::new(),
        };
        assert_eq!(before.changes_since(&before), []);

        let after = DeviceSnapshot {
            outputs: vec![summary("b"), summary("c")],
            inputs: vec![summary("mic")],
            default_output: Some("c".into()),
            default_input: Some("mic".into()),
            midi_inputs: Vec::new(),
            midi_outputs: vec![summary("synth")],
        };
        assert_eq!(
            after.changes_since(&before),
            [
                DeviceEvent::DeviceRemoved {
                    direction: Direction::Output,
                    device: summary("a"),
                },
                DeviceEvent::DeviceAdded {
                    direction: Direction::Output,
                    device: summary("c"),
                },
                DeviceEvent::DefaultChanged {
                    direction: Direction::Output,
                    device: Some(summary("c")),
                },
                DeviceEvent::MidiPortRemoved {
                    direction: Direction::Input,
                    port: summary("keys"),
                },
                DeviceEvent::MidiPortAdded {
                    direction: Direction::Output,
                    port: summary("synth"),
                },
            ]
        );
        for event in after.changes_since(&before) {
            assert!(!event.to_string().is_empty());
        }

        // A default that names a device not in the list counts as none.
        let odd = DeviceSnapshot {
            default_output: Some("nowhere".into()),
            ..after.clone()
        };
        assert_eq!(odd.default_device(Direction::Output), None);
        assert_eq!(
            odd.changes_since(&after),
            [DeviceEvent::DefaultChanged {
                direction: Direction::Output,
                device: None,
            }]
        );
    }
}
