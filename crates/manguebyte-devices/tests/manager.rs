//! The device manager, hot-plug and saved setups, against the headless
//! backend: no sound card needed, so these run on every system.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use manguebyte_devices::{
    AudioCallback, DeviceChoice, DeviceError, DeviceEvent, DeviceIo, DeviceManager, DeviceRef,
    DeviceSetup, DeviceSummary, Direction, Fallback, LossCause, MIN_POLL_INTERVAL, MockBackend,
    StreamFormat,
};

/// A mock with built-in speakers and microphone, and a USB interface.
fn studio() -> MockBackend {
    let mock = MockBackend::new();
    mock.add_device(
        Direction::Output,
        MockBackend::device("speakers", "Speakers", 2, 48_000),
    );
    mock.add_device(
        Direction::Output,
        MockBackend::device("usb-out", "USB Interface", 4, 96_000),
    );
    mock.add_device(
        Direction::Input,
        MockBackend::device("mic", "Microphone", 1, 48_000),
    );
    mock.add_device(
        Direction::Input,
        MockBackend::device("usb-in", "USB Interface", 2, 96_000),
    );
    mock
}

fn summary(id: &str, name: &str) -> DeviceSummary {
    DeviceSummary {
        id: id.into(),
        name: name.into(),
    }
}

fn usb_setup() -> DeviceSetup {
    DeviceSetup::default()
        .with_output(DeviceChoice::Device(DeviceRef::new(
            "usb-out",
            "USB Interface",
        )))
        .with_input(DeviceChoice::Device(DeviceRef::new(
            "usb-in",
            "USB Interface",
        )))
        .with_sample_rate(96_000)
        .with_buffer_size(128)
}

/// Writes a level that tells which run of the callback produced a block.
fn level(value: f32) -> impl FnMut(&mut DeviceIo<'_>) + Send + 'static {
    move |io: &mut DeviceIo<'_>| {
        for channel in io.output.iter_channels_mut() {
            channel.fill(value);
        }
    }
}

fn render(mock: &MockBackend, channels: usize) -> Option<Vec<f32>> {
    let mut block = vec![0.0f32; 64 * channels];
    mock.render(&mut block).then_some(block)
}

#[test]
fn the_default_setup_plays_on_the_default_output() {
    let mock = studio();
    let mut manager = DeviceManager::headless(&mock);
    let report = manager.start(&DeviceSetup::default(), level(0.25)).unwrap();
    assert!(report.is_exact(), "{:?}", report.fallbacks);
    assert!(manager.is_running());
    assert_eq!(
        manager.running_device(Direction::Output),
        Some(&summary("speakers", "Speakers"))
    );
    assert_eq!(manager.running_device(Direction::Input), None);
    let format = manager.format().unwrap();
    assert_eq!(format.sample_rate, 48_000.0);
    assert_eq!((format.input_channels, format.output_channels), (0, 2));

    let block = render(&mock, 2).unwrap();
    assert!(block.iter().all(|&sample| sample == 0.25));
    // Nothing captures without an input.
    assert!(!mock.capture(&[0.0; 64]));

    manager.stop();
    assert!(!mock.is_running());
    assert!(render(&mock, 2).is_none());
    // The callback was kept, and runs again.
    manager.restart().unwrap();
    assert_eq!(render(&mock, 2).unwrap()[0], 0.25);
}

#[test]
fn a_saved_setup_reopens_the_same_devices() {
    let mock = studio();
    let mut manager = DeviceManager::headless(&mock);
    let report = manager.start(&usb_setup(), level(0.5)).unwrap();
    assert!(report.is_exact(), "{:?}", report.fallbacks);
    assert_eq!(
        mock.running(Direction::Output).unwrap().1.sample_rate,
        Some(96_000)
    );
    assert_eq!(mock.running(Direction::Input).unwrap().0, "usb-in");
    assert_eq!(
        manager
            .running_settings(Direction::Output)
            .unwrap()
            .buffer_size,
        Some(128)
    );
    let saved = manager.setup().to_string();
    drop(manager);
    assert!(!mock.is_running());

    // A later run.
    let restored: DeviceSetup = saved.parse().unwrap();
    assert_eq!(restored, usb_setup());
    let mut manager = DeviceManager::headless(&mock);
    let report = manager.start(&restored, level(0.5)).unwrap();
    assert!(report.is_exact());
    assert_eq!(
        manager.running_device(Direction::Output).unwrap().id,
        "usb-out"
    );
}

#[test]
fn a_missing_device_falls_back_and_the_setup_still_wants_it() {
    let mock = studio();
    mock.remove_device(Direction::Output, "usb-out");
    mock.remove_device(Direction::Input, "usb-in");
    let mut manager = DeviceManager::headless(&mock);
    let report = manager.start(&usb_setup(), level(0.5)).unwrap();
    assert!(matches!(
        &report.fallbacks[..],
        [
            Fallback::DeviceReplaced {
                direction: Direction::Output,
                by_name: false,
                ..
            },
            Fallback::DeviceReplaced {
                direction: Direction::Input,
                by_name: false,
                ..
            },
        ]
    ));
    // The built-in devices can run at the saved rate, so they do.
    assert_eq!(manager.format().unwrap().sample_rate, 96_000.0);
    assert_eq!(
        manager.running_device(Direction::Output).unwrap().id,
        "speakers"
    );
    assert_eq!(manager.running_device(Direction::Input).unwrap().id, "mic");
    // What gets saved is still the interface, so that it is used again
    // once it is back.
    assert_eq!(manager.setup(), &usb_setup());

    // It comes back under another identifier, as ALSA devices do when
    // plugged into another port: found by its name.
    mock.add_device(
        Direction::Output,
        MockBackend::device("usb-out-2", "USB Interface", 4, 96_000),
    );
    let report = manager
        .apply(&usb_setup().with_input(DeviceChoice::None))
        .unwrap();
    assert!(matches!(
        &report.fallbacks[..],
        [Fallback::DeviceReplaced { by_name: true, .. }]
    ));
    assert_eq!(
        manager.running_device(Direction::Output).unwrap().id,
        "usb-out-2"
    );
}

#[test]
fn an_unknown_host_falls_back_to_the_default_one() {
    let mock = studio();
    mock.add_host("Other");
    let mut manager = DeviceManager::headless(&mock);
    let report = manager
        .start(&DeviceSetup::default().with_host("Nope"), level(0.1))
        .unwrap();
    assert_eq!(
        report.fallbacks,
        [Fallback::HostUnavailable {
            wanted: "Nope".into(),
            used: "Mock".into()
        }]
    );
    let report = manager
        .apply(&DeviceSetup::default().with_host("other"))
        .unwrap();
    assert!(report.is_exact());
    assert_eq!(manager.host_name(), "Other");
    assert_eq!(manager.hosts().len(), 2);
}

#[test]
fn unplugging_the_device_in_use_is_reported_and_followed() {
    let mock = studio();
    let mut manager = DeviceManager::headless(&mock);
    manager.start(&usb_setup(), level(0.5)).unwrap();
    // The first poll learns what is there.
    assert_eq!(manager.poll_devices(), []);

    assert!(mock.remove_device(Direction::Output, "usb-out"));
    // The stream stops being called, as a real one does.
    assert!(render(&mock, 4).is_none());
    let events = manager.poll_devices();
    assert_eq!(
        events,
        [
            DeviceEvent::DeviceRemoved {
                direction: Direction::Output,
                device: summary("usb-out", "USB Interface"),
            },
            DeviceEvent::StreamLost {
                direction: Direction::Output,
                device: summary("usb-out", "USB Interface"),
                cause: LossCause::Disconnected,
            },
        ]
    );
    // Either event moves the stream to the default; the other one then
    // changes nothing.
    let report = manager.follow(&events[1]).unwrap().unwrap();
    assert!(!report.is_exact());
    assert!(manager.follow(&events[0]).is_none());
    assert_eq!(
        manager.running_device(Direction::Output).unwrap().id,
        "speakers"
    );
    assert!(render(&mock, 2).is_some());
    // Reported once, not at every poll.
    assert_eq!(manager.poll_devices(), []);

    // Plugged in again: the setup wants it, so the manager goes back.
    mock.add_device(
        Direction::Output,
        MockBackend::device("usb-out", "USB Interface", 4, 96_000),
    );
    let events = manager.poll_devices();
    assert_eq!(
        events,
        [DeviceEvent::DeviceAdded {
            direction: Direction::Output,
            device: summary("usb-out", "USB Interface"),
        }]
    );
    let report = manager.follow(&events[0]).unwrap().unwrap();
    assert!(report.is_exact(), "{:?}", report.fallbacks);
    assert_eq!(
        manager.running_device(Direction::Output).unwrap().id,
        "usb-out"
    );
    assert_eq!(render(&mock, 4).unwrap()[0], 0.5);
}

#[test]
fn a_device_the_backend_reports_lost_is_reported_once() {
    let mock = studio();
    let mut manager = DeviceManager::headless(&mock);
    let errors = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&errors);
    manager.set_error_handler(move |error| {
        assert!(error.is_device_lost());
        counter.fetch_add(1, Ordering::Relaxed);
    });
    manager.start(&DeviceSetup::default(), level(0.5)).unwrap();
    manager.poll_devices();
    assert!(mock.report_device_lost(Direction::Output));
    assert_eq!(errors.load(Ordering::Relaxed), 1);
    assert_eq!(
        manager.poll_devices(),
        [DeviceEvent::StreamLost {
            direction: Direction::Output,
            device: summary("speakers", "Speakers"),
            cause: LossCause::ReportedByBackend,
        }]
    );
    assert_eq!(manager.poll_devices(), []);
}

#[test]
fn the_default_changing_moves_a_stream_that_follows_the_default() {
    let mock = studio();
    let mut manager = DeviceManager::headless(&mock);
    manager.start(&DeviceSetup::default(), level(0.5)).unwrap();
    manager.poll_devices();
    mock.set_default_device(Direction::Output, Some("usb-out"));
    let events = manager.poll_devices();
    assert_eq!(
        events,
        [DeviceEvent::DefaultChanged {
            direction: Direction::Output,
            device: Some(summary("usb-out", "USB Interface")),
        }]
    );
    manager.follow(&events[0]).unwrap().unwrap();
    assert_eq!(
        manager.running_device(Direction::Output).unwrap().id,
        "usb-out"
    );

    // A setup that names a device stays on it.
    manager
        .apply(
            &DeviceSetup::default()
                .with_output(DeviceChoice::Device(DeviceRef::new("speakers", "Speakers"))),
        )
        .unwrap();
    mock.set_default_device(Direction::Output, Some("speakers"));
    manager.poll_devices();
    mock.set_default_device(Direction::Output, Some("usb-out"));
    let events = manager.poll_devices();
    assert!(manager.follow(&events[0]).is_none());
}

#[test]
fn with_nothing_connected_the_manager_waits_for_a_device() {
    let mock = MockBackend::new();
    let mut manager = DeviceManager::headless(&mock);
    let report = manager.start(&DeviceSetup::default(), level(0.5)).unwrap();
    assert_eq!(
        report.fallbacks,
        [Fallback::NoDevice {
            direction: Direction::Output,
            wanted: None
        }]
    );
    assert!(!manager.is_running());
    manager.poll_devices();

    mock.add_device(
        Direction::Output,
        MockBackend::device("speakers", "Speakers", 2, 44_100),
    );
    let events = manager.poll_devices();
    assert_eq!(events.len(), 2, "{events:?}");
    let report = manager.follow(&events[0]).unwrap().unwrap();
    assert!(report.is_exact());
    assert!(manager.is_running());
    assert_eq!(manager.format().unwrap().sample_rate, 44_100.0);
    // Already on it: the second event changes nothing.
    assert!(manager.follow(&events[1]).is_none());
}

#[test]
fn listing_failures_are_reported_and_recovered_from() {
    let mock = studio();
    let mut manager = DeviceManager::headless(&mock);
    manager.poll_devices();
    mock.fail_listing(true);
    assert!(matches!(
        &manager.poll_devices()[..],
        [DeviceEvent::ListingFailed(_)]
    ));
    assert!(manager.start(&DeviceSetup::default(), level(0.5)).is_err());
    mock.fail_listing(false);
    assert_eq!(manager.poll_devices(), []);
    // The callback was kept through the failure.
    manager.restart().unwrap();
    assert!(manager.is_running());
}

#[test]
fn restarting_before_starting_is_an_error() {
    let mut manager = DeviceManager::headless(&studio());
    assert!(matches!(manager.restart(), Err(DeviceError::NoCallback)));
    assert!(manager.take_callback().is_none());
}

/// Counts what the manager does with its callback.
#[derive(Default)]
struct Counting {
    prepared: Arc<AtomicUsize>,
    released: Arc<AtomicUsize>,
    formats: Option<mpsc::Sender<StreamFormat>>,
}

impl AudioCallback for Counting {
    fn prepare(&mut self, format: &StreamFormat) {
        self.prepared.fetch_add(1, Ordering::Relaxed);
        if let Some(formats) = &self.formats {
            formats.send(*format).unwrap();
        }
    }

    fn process(&mut self, io: &mut DeviceIo<'_>) {
        let frames = io.num_frames();
        io.output.channel_mut(0)[..frames].fill(1.0);
    }

    fn release(&mut self) {
        self.released.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn the_callback_is_prepared_before_and_released_after_every_run() {
    let mock = studio();
    let (formats, received) = mpsc::channel();
    let callback = Counting {
        formats: Some(formats),
        ..Counting::default()
    };
    let (prepared, released) = (
        Arc::clone(&callback.prepared),
        Arc::clone(&callback.released),
    );
    let mut manager = DeviceManager::headless(&mock);
    manager.set_max_block_size(256);
    manager.start(&DeviceSetup::default(), callback).unwrap();
    manager.apply(&usb_setup()).unwrap();
    manager.stop();
    assert_eq!(prepared.load(Ordering::Relaxed), 2);
    assert_eq!(released.load(Ordering::Relaxed), 2);
    let formats: Vec<StreamFormat> = received.try_iter().collect();
    assert_eq!(
        formats,
        [
            StreamFormat {
                sample_rate: 48_000.0,
                max_block_size: 256,
                input_channels: 0,
                output_channels: 2,
            },
            StreamFormat {
                sample_rate: 96_000.0,
                max_block_size: 256,
                input_channels: 2,
                output_channels: 4,
            },
        ]
    );
    assert!(manager.take_callback().is_some());
}

#[test]
fn a_preview_plans_a_setup_without_opening_it() {
    let mock = studio();
    let mut manager = DeviceManager::headless(&mock);
    manager.start(&DeviceSetup::default(), level(0.5)).unwrap();
    let gone = usb_setup().with_output(DeviceChoice::Device(DeviceRef::new("gone", "Gone")));
    let preview = manager.preview(&gone).unwrap();
    assert_eq!(
        preview.output.as_ref().unwrap().0,
        summary("speakers", "Speakers")
    );
    assert_eq!(
        preview.input.as_ref().unwrap().0,
        summary("usb-in", "USB Interface")
    );
    assert_eq!(preview.sample_rate(), Some(96_000));
    assert!(matches!(
        &preview.fallbacks[..],
        [Fallback::DeviceReplaced { by_name: false, .. }]
    ));
    // Nothing changed: the speakers still play, and the setup is the one
    // started.
    assert_eq!(
        mock.running(Direction::Output).map(|(id, _)| id),
        Some(String::from("speakers"))
    );
    assert_eq!(manager.setup(), &DeviceSetup::default());
}

#[test]
fn a_new_engine_format_prepares_the_same_callback_again() {
    use manguebyte_devices::{EngineFormat, SwapKind};

    let mock = studio();
    let (formats, received) = mpsc::channel();
    let callback = Counting {
        formats: Some(formats),
        ..Counting::default()
    };
    let (prepared, released) = (
        Arc::clone(&callback.prepared),
        Arc::clone(&callback.released),
    );
    let mut manager = DeviceManager::headless(&mock);
    manager.set_crossfade(Duration::ZERO);
    let at_48k = EngineFormat::new(48_000, 256, 0, 2);
    manager
        .start_engine(&DeviceSetup::default(), at_48k, callback)
        .unwrap();
    assert!(render(&mock, 2).is_some());

    // The same format is a plain apply: nothing is prepared again.
    manager
        .apply_with_format(&DeviceSetup::default(), at_48k)
        .unwrap();
    assert_eq!(prepared.load(Ordering::Relaxed), 1);

    // To the interface, at its rate: released, prepared at 96 kHz, and
    // started again on the interface.
    let at_96k = EngineFormat::new(96_000, 256, 0, 4);
    let usb = usb_setup().with_input(DeviceChoice::None);
    let report = manager.apply_with_format(&usb, at_96k).unwrap();
    assert!(report.is_exact(), "{:?}", report.fallbacks);
    assert_eq!(prepared.load(Ordering::Relaxed), 2);
    assert_eq!(released.load(Ordering::Relaxed), 1);
    assert_eq!(manager.engine_format(), Some(at_96k));
    assert_eq!(manager.setup(), &usb);
    assert_eq!(manager.last_swap().unwrap().kind, SwapKind::Gap);
    assert_eq!(
        mock.running(Direction::Output).map(|(id, _)| id),
        Some(String::from("usb-out"))
    );
    assert_eq!(manager.format().unwrap().sample_rate, 96_000.0);
    let block = render(&mock, 4).unwrap();
    assert!(
        block
            .iter()
            .step_by(4)
            .skip(32)
            .all(|&sample| sample == 1.0)
    );
    let formats: Vec<f64> = received.try_iter().map(|f| f.sample_rate).collect();
    assert_eq!(formats, [48_000.0, 96_000.0]);

    // A manager without an engine has no format to change.
    let mut plain = DeviceManager::headless(&mock);
    assert!(matches!(
        plain.apply_with_format(&DeviceSetup::default(), at_48k),
        Err(DeviceError::NoCallback)
    ));
}

#[test]
fn a_duplex_run_carries_input_to_output_with_its_latency() {
    let mock = studio();
    mock.set_latency(
        Some(Duration::from_millis(2)),
        Some(Duration::from_millis(3)),
    );
    let mut manager = DeviceManager::headless(&mock);
    manager.set_max_block_size(64);
    let (infos, received) = mpsc::channel();
    let setup = DeviceSetup::default()
        .with_output(DeviceChoice::Device(DeviceRef::new("speakers", "Speakers")))
        .with_input(DeviceChoice::Device(DeviceRef::new("mic", "Microphone")));
    manager
        .start(&setup, move |io: &mut DeviceIo<'_>| {
            for channel in io.output.iter_channels_mut() {
                channel.copy_from_slice(io.input.channel(0));
            }
            let _ = infos.send(io.info);
        })
        .unwrap();

    // Ten blocks of a ramp in, ten blocks out.
    let mut played: Vec<f32> = Vec::new();
    for block in 0..10 {
        let ramp: Vec<f32> = (0..64).map(|n| (block * 64 + n) as f32 / 1_000.0).collect();
        assert!(mock.capture(&ramp));
        let mut out = [0.0f32; 128];
        assert!(mock.render(&mut out));
        played.extend(out.iter().step_by(2));
    }
    // The input arrives, in order and smoothly, after the reserve.
    let first = played.iter().position(|&sample| sample > 0.01).unwrap();
    assert!(first < 640 - 64, "nothing came through");
    assert!(
        played[first..]
            .windows(2)
            .all(|pair| (pair[1] - pair[0] - 0.001).abs() < 1e-4),
        "the ramp was broken"
    );
    let info = received.try_iter().last().unwrap();
    assert_eq!(info.output_latency, Some(Duration::from_millis(3)));
    let input = info.input_latency.unwrap();
    assert!(input > Duration::from_millis(2), "{input:?}");
    assert_eq!(info.frame, 9 * 64);
}

#[test]
fn the_watcher_thread_reports_changes() {
    let mock = studio();
    let manager = DeviceManager::headless(&mock);
    let (events, received) = mpsc::channel();
    let watcher = manager
        .watch(MIN_POLL_INTERVAL, move |event| {
            let _ = events.send(event);
        })
        .unwrap();
    assert_eq!(watcher.interval(), MIN_POLL_INTERVAL);
    // Let the first poll learn what is there, then change something.
    std::thread::sleep(MIN_POLL_INTERVAL * 3);
    mock.add_device(
        Direction::Input,
        MockBackend::device("headset", "Headset", 1, 16_000),
    );
    watcher.refresh();
    let event = received.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        event,
        DeviceEvent::DeviceAdded {
            direction: Direction::Input,
            device: summary("headset", "Headset"),
        }
    );
    drop(watcher);
}

#[cfg(feature = "midi")]
mod midi {
    use super::*;
    use std::time::Instant;

    use manguebyte_core::spsc::{self, Producer};
    use manguebyte_devices::midi::{MidiError, MidiInputMessage, MidiProtocol};
    use manguebyte_midi::MidiMessage;
    use manguebyte_midi::ump::Packet;

    #[test]
    fn midi_from_every_open_port_reaches_the_callback() {
        let mock = studio();
        mock.add_midi_port(Direction::Input, "keys", "Keys");
        mock.add_midi_port(Direction::Input, "pads", "Pads");
        mock.add_midi_port(Direction::Output, "synth", "Synth");
        let setup = DeviceSetup::default()
            .with_midi_input(DeviceRef::new("keys", "Keys"))
            .with_midi_input(DeviceRef::new("gone", "Gone"))
            .with_midi_output(DeviceRef::new("synth", "Synth"));

        let (notes, received) = mpsc::channel();
        let mut manager = DeviceManager::headless(&mock);
        let report = manager
            .start(&setup, move |io: &mut DeviceIo<'_>| {
                io.read_midi(|_, message| {
                    let _ = notes.send(message);
                });
            })
            .unwrap();
        assert!(matches!(
            &report.fallbacks[..],
            [Fallback::MidiPortMissing { .. }]
        ));
        assert_eq!(manager.midi_inputs(), [summary("keys", "Keys")]);

        assert!(mock.send_midi("keys", &[0x90, 60, 100]));
        assert!(!mock.send_midi("pads", &[0x90, 61, 100]), "not open");
        manager
            .enable_midi_input(DeviceRef::new("pads", "Pads"))
            .unwrap();
        assert!(mock.send_midi("pads", &[0x91, 62, 90]));
        render(&mock, 2).unwrap();
        let seen: Vec<MidiInputMessage> = received.try_iter().collect();
        assert_eq!(
            seen,
            [
                MidiInputMessage::Channel(MidiMessage::NoteOn {
                    channel: 0,
                    note: 60,
                    velocity: 100
                }),
                MidiInputMessage::Channel(MidiMessage::NoteOn {
                    channel: 1,
                    note: 62,
                    velocity: 90
                }),
            ]
        );

        // MIDI keeps flowing through a restart of the audio.
        manager.restart().unwrap();
        assert!(mock.send_midi("keys", &[0x80, 60, 0]));
        render(&mock, 2).unwrap();
        assert_eq!(received.try_iter().count(), 1);

        manager.send_midi(&[0xB0, 7, 100]).unwrap();
        assert_eq!(
            mock.take_sent_midi(),
            [("synth".to_owned(), vec![0xB0, 7, 100])]
        );
        // A MIDI 2.0 packet goes out by the default translation.
        let note = manguebyte_midi::ump::Packet::from_words(&[0x4091_3C00, 0xC000_0000]).unwrap();
        manager.send_midi_packet(&note).unwrap();
        assert_eq!(
            mock.take_sent_midi(),
            [("synth".to_owned(), vec![0x91, 60, 0x60])]
        );
        manager.disable_midi_input("keys");
        assert!(!mock.send_midi("keys", &[0x90, 60, 100]));
        manager.set_midi_output(None).unwrap();
        assert!(matches!(
            manager.send_midi(&[0xF8]),
            Err(MidiError::PortNotFound(_))
        ));
    }

    #[test]
    fn a_midi_port_of_the_setup_is_opened_when_it_appears() {
        let mock = studio();
        let mut manager = DeviceManager::headless(&mock);
        let setup = DeviceSetup::default().with_midi_input(DeviceRef::new("keys", "Keys"));
        manager.start(&setup, level(0.0)).unwrap();
        manager.poll_devices();
        assert!(manager.midi_inputs().is_empty());

        // Plugged in under a new identifier: found by its name.
        mock.add_midi_port(Direction::Input, "keys-2", "Keys");
        let events = manager.poll_devices();
        assert_eq!(
            events,
            [DeviceEvent::MidiPortAdded {
                direction: Direction::Input,
                port: summary("keys-2", "Keys"),
            }]
        );
        assert!(manager.follow(&events[0]).unwrap().is_ok());
        assert_eq!(manager.midi_inputs(), [summary("keys-2", "Keys")]);
        assert!(manager.is_running(), "the audio was left alone");

        mock.remove_midi_port(Direction::Input, "keys-2");
        let events = manager.poll_devices();
        assert!(manager.follow(&events[0]).is_some());
        assert!(manager.midi_inputs().is_empty());
    }

    /// A manager with two input ports whose callback hands on what it reads
    /// on a channel.
    fn listening(
        mock: &MockBackend,
        setup: &DeviceSetup,
    ) -> (DeviceManager, mpsc::Receiver<MidiInputMessage>) {
        mock.add_midi_port(Direction::Input, "keys", "Keys");
        mock.add_midi_port(Direction::Input, "pads", "Pads");
        let (messages, received) = mpsc::channel();
        let mut manager = DeviceManager::headless(mock);
        manager
            .start(setup, move |io: &mut DeviceIo<'_>| {
                io.read_midi(|_, message| {
                    let _ = messages.send(message);
                });
            })
            .unwrap();
        (manager, received)
    }

    /// Plays a block, and takes what the callback read since the last call.
    fn heard(mock: &MockBackend, received: &mpsc::Receiver<MidiInputMessage>) -> Vec<MidiMessage> {
        render(mock, 2).unwrap();
        received
            .try_iter()
            .filter_map(|message| match message {
                MidiInputMessage::Channel(message) => Some(message),
                MidiInputMessage::System(_) => None,
            })
            .collect()
    }

    fn off(channel: u8, note: u8) -> MidiMessage {
        MidiMessage::NoteOff {
            channel,
            note,
            velocity: 0,
        }
    }

    fn lift(channel: u8) -> MidiMessage {
        MidiMessage::ControlChange {
            channel,
            controller: 64,
            value: 0,
        }
    }

    fn both_ports() -> DeviceSetup {
        DeviceSetup::default()
            .with_midi_input(DeviceRef::new("keys", "Keys"))
            .with_midi_input(DeviceRef::new("pads", "Pads"))
    }

    #[test]
    fn an_input_port_that_is_disabled_ends_what_it_holds() {
        let mock = studio();
        let (mut manager, received) = listening(&mock, &both_ports());
        assert!(mock.send_midi("keys", &[0x90, 60, 100, 0xB2, 64, 127, 0x93, 61, 90]));
        assert_eq!(heard(&mock, &received).len(), 3);

        manager.disable_midi_input("keys");
        assert_eq!(
            heard(&mock, &received),
            [off(0, 60), off(3, 61), lift(2)],
            "the notes, then the pedals"
        );
        assert_eq!(heard(&mock, &received), [], "once");
    }

    #[test]
    fn an_input_port_that_goes_away_ends_what_it_holds() {
        let mock = studio();
        let (mut manager, received) = listening(&mock, &both_ports());
        manager.poll_devices();
        assert!(mock.send_midi("pads", &[0x90, 60, 100, 0xB0, 64, 127]));
        assert_eq!(heard(&mock, &received).len(), 2);

        mock.remove_midi_port(Direction::Input, "pads");
        let events = manager.poll_devices();
        assert_eq!(heard(&mock, &received), [], "not followed yet");
        assert!(manager.follow(&events[0]).is_some());
        assert_eq!(heard(&mock, &received), [off(0, 60), lift(0)]);
        assert_eq!(heard(&mock, &received), [], "once");
    }

    #[test]
    fn an_input_port_a_new_setup_leaves_out_ends_what_it_holds() {
        let mock = studio();
        let (mut manager, received) = listening(&mock, &both_ports());
        assert!(mock.send_midi("keys", &[0x90, 60, 100]));
        assert!(mock.send_midi("pads", &[0x90, 62, 100]));
        assert_eq!(heard(&mock, &received).len(), 2);

        let only_pads = DeviceSetup::default().with_midi_input(DeviceRef::new("pads", "Pads"));
        manager.apply(&only_pads).unwrap();
        assert_eq!(heard(&mock, &received), [off(0, 60)]);
        // A setup that keeps the port leaves it holding its note.
        manager.apply(&only_pads).unwrap();
        assert_eq!(heard(&mock, &received), []);
        assert!(mock.send_midi("pads", &[0x80, 62, 7]));
        assert_eq!(
            heard(&mock, &received),
            [MidiMessage::NoteOff {
                channel: 0,
                note: 62,
                velocity: 7
            }],
            "the port's own note-off is passed on as it is"
        );
    }

    #[test]
    fn an_input_port_ends_what_it_holds_while_the_stream_is_stopped() {
        let mock = studio();
        let (mut manager, received) = listening(&mock, &both_ports());
        assert!(mock.send_midi("keys", &[0x90, 60, 100]));
        assert_eq!(heard(&mock, &received).len(), 1);

        manager.stop();
        manager.disable_midi_input("keys");
        manager.restart().unwrap();
        assert_eq!(heard(&mock, &received), [off(0, 60)]);
    }

    #[test]
    fn what_ends_a_note_waits_when_the_queue_is_full() {
        let mock = studio();
        let (mut manager, received) = listening(&mock, &both_ports());
        assert!(mock.send_midi("keys", &[0x90, 60, 100, 0xB1, 64, 127]));
        assert!(mock.send_midi("pads", &[0x90, 62, 100, 0x90, 63, 100]));
        // More than the queue can hold, with no block read in between.
        for _ in 0..5_000 {
            assert!(mock.send_midi("pads", &[0xB0, 7, 100]));
        }
        // Neither fits: the port's own note-off, and those of its closing.
        assert!(mock.send_midi("pads", &[0x80, 62, 0]));
        manager.disable_midi_input("keys");
        // A note that comes to a full queue is lost, with its note-off.
        assert!(mock.send_midi("pads", &[0x90, 64, 100, 0x80, 64, 0]));

        let first = heard(&mock, &received);
        assert!(first.len() >= 4_000, "{}", first.len());
        assert!(
            !first
                .iter()
                .any(|message| matches!(message, MidiMessage::NoteOff { .. })),
        );
        assert!(!first.contains(&lift(1)));

        // The next poll finds room for them, in order: notes, then pedals.
        assert_eq!(manager.poll_devices(), []);
        assert_eq!(
            heard(&mock, &received),
            [off(0, 60), off(0, 62), lift(1)],
            "and only them"
        );
        assert_eq!(heard(&mock, &received), []);

        // Nothing was left over to get in the way of the next note.
        assert!(mock.send_midi("pads", &[0x80, 63, 0, 0x90, 64, 100, 0x80, 64, 0]));
        assert_eq!(
            heard(&mock, &received),
            [
                off(0, 63),
                MidiMessage::NoteOn {
                    channel: 0,
                    note: 64,
                    velocity: 100
                },
                off(0, 64),
            ]
        );
    }

    #[test]
    fn what_ends_a_note_goes_before_a_later_message_when_room_appears() {
        let mock = studio();
        let (mut manager, received) = listening(&mock, &both_ports());
        assert!(mock.send_midi("keys", &[0x90, 60, 100]));
        for _ in 0..5_000 {
            assert!(mock.send_midi("pads", &[0xB0, 7, 100]));
        }
        manager.disable_midi_input("keys");
        heard(&mock, &received);

        // No poll: the next message of any port brings it along, first.
        assert!(mock.send_midi("pads", &[0x90, 60, 100]));
        assert_eq!(
            heard(&mock, &received),
            [
                off(0, 60),
                MidiMessage::NoteOn {
                    channel: 0,
                    note: 60,
                    velocity: 100
                },
            ]
        );
    }

    /// Fills the queue to the callback, with no block read.
    fn fill_the_queue(mock: &MockBackend, port: &str) {
        for _ in 0..5_000 {
            assert!(mock.send_midi(port, &[0xB0, 7, 100]));
        }
    }

    fn strike(channel: u8, note: u8) -> MidiMessage {
        MidiMessage::NoteOn {
            channel,
            note,
            velocity: 100,
        }
    }

    #[test]
    fn a_channel_message_the_queue_had_no_room_for_is_not_followed() {
        let mock = studio();
        let (mut manager, received) = listening(&mock, &both_ports());
        assert!(mock.send_midi("keys", &[0x90, 60, 100, 0xB1, 64, 127]));
        assert_eq!(heard(&mock, &received).len(), 2);

        // All notes off and a reset of the controllers go nowhere, so the
        // plugin still has the note and the pedal.
        fill_the_queue(&mock, "pads");
        assert!(mock.send_midi("keys", &[0xB0, 123, 0, 0xB1, 121, 0]));
        heard(&mock, &received);

        // As far as the other port goes, the first still holds them.
        assert!(mock.send_midi("pads", &[0x90, 60, 100]));
        assert_eq!(heard(&mock, &received), [off(0, 60), strike(0, 60)]);
        assert!(mock.send_midi("pads", &[0x80, 60, 0, 0xB1, 64, 127, 0xB1, 64, 0]));
        assert_eq!(
            heard(&mock, &received),
            [MidiMessage::ControlChange {
                channel: 1,
                controller: 64,
                value: 127
            }],
        );

        manager.disable_midi_input("keys");
        assert_eq!(heard(&mock, &received), [off(0, 60), lift(1)]);
    }

    #[test]
    fn a_key_struck_again_on_a_full_queue_is_lost_whole() {
        // By the port that holds it: its one note-off is still to come.
        let mock = studio();
        let (manager, received) = listening(&mock, &both_ports());
        assert!(mock.send_midi("keys", &[0x90, 60, 100]));
        assert_eq!(heard(&mock, &received), [strike(0, 60)]);
        fill_the_queue(&mock, "pads");
        assert!(mock.send_midi("keys", &[0x90, 60, 100]));
        heard(&mock, &received);
        assert!(mock.send_midi("keys", &[0x80, 60, 0]));
        assert_eq!(heard(&mock, &received), [off(0, 60)]);
        assert_eq!(heard(&mock, &received), []);
        drop(manager);

        // By another port: the note goes on with the first port's key down.
        let mock = studio();
        let (mut manager, received) = listening(&mock, &both_ports());
        assert!(mock.send_midi("keys", &[0x90, 60, 100]));
        assert_eq!(heard(&mock, &received), [strike(0, 60)]);
        fill_the_queue(&mock, "keys");
        assert!(mock.send_midi("pads", &[0x90, 60, 100]));
        heard(&mock, &received);
        assert_eq!(manager.poll_devices(), []);
        assert_eq!(heard(&mock, &received), [], "not cut");
        assert!(mock.send_midi("pads", &[0x80, 60, 0]));
        assert_eq!(heard(&mock, &received), [], "that port held nothing");
        assert!(mock.send_midi("keys", &[0x80, 60, 0]));
        assert_eq!(heard(&mock, &received), [off(0, 60)]);
    }

    #[test]
    fn a_velocity_prefix_stays_with_its_note() {
        let mock = studio();
        let (mut manager, received) = listening(&mock, &both_ports());
        let prefix = |value| MidiMessage::ControlChange {
            channel: 0,
            controller: 88,
            value,
        };
        assert!(mock.send_midi("keys", &[0x90, 60, 100]));
        assert_eq!(heard(&mock, &received), [strike(0, 60)]);

        // A second strike: the note-off, then the prefix and its note, even
        // with another message in between.
        assert!(mock.send_midi("keys", &[0xB0, 88, 5, 0xB0, 7, 9, 0x90, 60, 100]));
        assert_eq!(
            heard(&mock, &received),
            [
                MidiMessage::ControlChange {
                    channel: 0,
                    controller: 7,
                    value: 9
                },
                off(0, 60),
                prefix(5),
                strike(0, 60),
            ]
        );
        // A note-off that passes takes its prefix along.
        assert!(mock.send_midi("keys", &[0xB0, 88, 6, 0x80, 60, 0]));
        assert_eq!(heard(&mock, &received), [prefix(6), off(0, 60)]);
        // A note-off that is dropped, or a port that closes, takes it away.
        assert!(mock.send_midi("keys", &[0xB0, 88, 7, 0x80, 60, 0, 0xB0, 88, 8]));
        assert_eq!(heard(&mock, &received), []);
        manager.disable_midi_input("keys");
        assert_eq!(heard(&mock, &received), []);
    }

    #[test]
    fn what_is_owed_goes_with_any_message_of_any_port() {
        // Messages that themselves go nowhere: a note-off for nothing, and
        // a lift another port holds down.
        for (setup, message) in [
            (&[][..], &[0x80, 61, 0][..]),
            (&[0xB0, 64, 127][..], &[0xB0, 64, 0][..]),
        ] {
            let mock = studio();
            let (mut manager, received) = listening(&mock, &both_ports());
            assert!(mock.send_midi("keys", &[0x90, 60, 100, 0xB0, 64, 127]));
            assert!(mock.send_midi("pads", setup));
            assert!(mock.send_midi("pads", &[0xB5, 64, 127]));
            heard(&mock, &received);
            fill_the_queue(&mock, "pads");
            manager.disable_midi_input("keys");
            heard(&mock, &received);
            assert!(mock.send_midi("pads", message));
            let ended = heard(&mock, &received);
            assert_eq!(ended.first(), Some(&off(0, 60)), "{ended:?}");
        }
    }

    #[test]
    fn closing_an_input_port_leaves_its_other_controllers_as_they_were() {
        let mock = studio();
        let (mut manager, received) = listening(&mock, &both_ports());
        // The wheel, sostenuto, and the mod wheel.
        assert!(mock.send_midi("keys", &[0xE0, 0x7F, 0x7F, 0xB0, 66, 127, 0xB0, 1, 90]));
        assert_eq!(heard(&mock, &received).len(), 3);
        manager.disable_midi_input("keys");
        assert_eq!(heard(&mock, &received), []);
    }

    fn note_on(note: u8) -> Packet {
        Packet::from_midi1(
            0,
            MidiMessage::NoteOn {
                channel: 0,
                note,
                velocity: 100,
            },
        )
    }

    fn note_off(note: u8) -> Packet {
        Packet::from_midi1(
            0,
            MidiMessage::NoteOff {
                channel: 0,
                note,
                velocity: 64,
            },
        )
    }

    /// A callback that sends the MIDI output what the test queues, at the
    /// start of the next block.
    fn sender(
        manager: &mut DeviceManager,
    ) -> (
        Producer<Packet>,
        impl FnMut(&mut DeviceIo<'_>) + Send + use<>,
    ) {
        let (packets, mut queued) = spsc::channel::<Packet>(256);
        let mut transmitter = manager.midi_output_transmitter(256);
        let callback = move |io: &mut DeviceIo<'_>| {
            transmitter.begin_block(io.num_frames(), 48_000.0);
            while let Some(packet) = queued.pop() {
                transmitter.send(0, packet);
            }
        };
        (packets, callback)
    }

    /// Waits until the mock's MIDI output ports have been sent `count`
    /// messages since the last call, and takes them.
    fn sent(mock: &MockBackend, count: usize) -> Vec<(String, Vec<u8>)> {
        let mut sent = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while sent.len() < count && Instant::now() < deadline {
            sent.extend(mock.take_sent_midi());
            std::thread::sleep(Duration::from_millis(1));
        }
        sent
    }

    fn bytes(port: &str, bytes: &[u8]) -> (String, Vec<u8>) {
        (port.to_owned(), bytes.to_vec())
    }

    #[test]
    fn what_the_audio_thread_sends_reaches_the_port_open_when_it_is_due() {
        let mock = studio();
        mock.add_midi_port(Direction::Output, "synth", "Synth");
        mock.add_midi_port(Direction::Output, "drums", "Drums");
        let mut manager = DeviceManager::headless(&mock);
        let (mut packets, callback) = sender(&mut manager);
        let setup = DeviceSetup::default().with_midi_output(DeviceRef::new("synth", "Synth"));
        assert!(manager.start(&setup, callback).unwrap().is_exact());
        // The first poll only learns what is there.
        assert!(manager.poll_devices().is_empty());
        assert!(manager.is_sending_midi());
        assert_eq!(manager.midi_output(), Some(&summary("synth", "Synth")));
        assert_eq!(manager.midi_output_protocol(), Some(MidiProtocol::Midi1));

        packets.push(note_on(60)).unwrap();
        render(&mock, 2).unwrap();
        assert_eq!(sent(&mock, 1), [bytes("synth", &[0x90, 60, 100])]);

        // Another port, without touching the audio: the old one ends the
        // note it plays at once.
        manager
            .set_midi_output(Some(DeviceRef::new("drums", "Drums")))
            .unwrap();
        assert_eq!(mock.take_sent_midi(), [bytes("synth", &[0x80, 60, 0])]);
        assert_eq!(manager.midi_output_notes_ended(), 1);
        assert!(manager.is_running());

        // Its note-off is not sent to the new port, which never played
        // the note; what follows is.
        packets.push(note_off(60)).unwrap();
        packets.push(note_on(62)).unwrap();
        render(&mock, 2).unwrap();
        assert_eq!(sent(&mock, 1), [bytes("drums", &[0x90, 62, 100])]);
        packets.push(note_off(62)).unwrap();
        render(&mock, 2).unwrap();
        assert_eq!(sent(&mock, 1), [bytes("drums", &[0x80, 62, 64])]);

        // A port that cannot be opened leaves none open, and stays in the
        // setup to be opened when it appears.
        packets.push(note_on(64)).unwrap();
        render(&mock, 2).unwrap();
        assert_eq!(sent(&mock, 1), [bytes("drums", &[0x90, 64, 100])]);
        assert!(matches!(
            manager.set_midi_output(Some(DeviceRef::new("later", "Later"))),
            Err(MidiError::PortNotFound(_))
        ));
        assert_eq!(mock.take_sent_midi(), [bytes("drums", &[0x80, 64, 0])]);
        assert_eq!(manager.midi_output(), None);
        packets.push(note_on(65)).unwrap();
        render(&mock, 2).unwrap();
        // Long past when it was due.
        std::thread::sleep(Duration::from_millis(50));
        mock.add_midi_port(Direction::Output, "later", "Later");
        let events = manager.poll_devices();
        assert!(manager.follow(&events[0]).unwrap().is_ok());
        assert_eq!(manager.midi_output(), Some(&summary("later", "Later")));
        packets.push(note_on(67)).unwrap();
        render(&mock, 2).unwrap();
        // The note due while no port was open was dropped.
        assert_eq!(sent(&mock, 1), [bytes("later", &[0x90, 67, 100])]);

        // Unplugged: closed, and reported when followed.
        mock.remove_midi_port(Direction::Output, "later");
        let events = manager.poll_devices();
        let report = manager.follow(&events[0]).unwrap().unwrap();
        assert!(matches!(
            &report.fallbacks[..],
            [Fallback::MidiPortMissing {
                direction: Direction::Output,
                ..
            }]
        ));
        assert_eq!(manager.midi_output(), None);
        assert_eq!(manager.midi_output_errors(), 0);
    }

    #[test]
    fn a_change_of_port_lifts_the_sustain_pedal_it_holds_down() {
        let mock = studio();
        mock.add_midi_port(Direction::Output, "synth", "Synth");
        mock.add_midi_port(Direction::Output, "drums", "Drums");
        let mut manager = DeviceManager::headless(&mock);
        manager
            .set_midi_output(Some(DeviceRef::new("synth", "Synth")))
            .unwrap();
        // From the program's thread: a note, held by the pedal on channel
        // 2 after its note-off, and the pedal on channel 1 down and up.
        for bytes in [
            [0x92, 60, 100],
            [0xB2, 64, 127],
            [0x82, 60, 0],
            [0xB1, 64, 100],
            [0xB1, 64, 0],
        ] {
            manager.send_midi(&bytes).unwrap();
        }
        // The same port again is left as it is.
        manager
            .set_midi_output(Some(DeviceRef::new("synth", "Synth")))
            .unwrap();
        assert_eq!(mock.take_sent_midi().len(), 5);
        // A MIDI 2.0 note through the program's packets, followed too.
        let note = Packet::from_words(&[0x4093_3E00, 0xC000_0000]).unwrap();
        manager.send_midi_packet(&note).unwrap();
        assert_eq!(mock.take_sent_midi(), [bytes("synth", &[0x93, 62, 0x60])]);
        manager
            .set_midi_output(Some(DeviceRef::new("drums", "Drums")))
            .unwrap();
        assert_eq!(
            mock.take_sent_midi(),
            [
                bytes("synth", &[0x83, 62, 0]),
                bytes("synth", &[0xB2, 64, 0]),
            ]
        );
        // What the old port ended is not ended again on the new one.
        manager.send_midi(&[0x83, 62, 0]).unwrap();
        assert!(mock.take_sent_midi().is_empty());
    }

    #[test]
    fn dropping_the_manager_sends_what_is_waiting_and_ends_the_notes() {
        let mock = studio();
        mock.add_midi_port(Direction::Output, "synth", "Synth");
        let mut manager = DeviceManager::headless(&mock);
        let (mut packets, callback) = sender(&mut manager);
        let setup = DeviceSetup::default().with_midi_output(DeviceRef::new("synth", "Synth"));
        manager.start(&setup, callback).unwrap();
        packets.push(note_on(60)).unwrap();
        packets.push(note_on(64)).unwrap();
        render(&mock, 2).unwrap();
        drop(manager);
        assert_eq!(
            mock.take_sent_midi(),
            [
                bytes("synth", &[0x90, 60, 100]),
                bytes("synth", &[0x90, 64, 100]),
                bytes("synth", &[0x80, 60, 0]),
                bytes("synth", &[0x80, 64, 0]),
            ]
        );
    }

    #[test]
    fn a_hung_midi_port_does_not_hold_up_the_audio() {
        let mock = studio();
        mock.add_midi_port(Direction::Output, "synth", "Synth");
        let mut manager = DeviceManager::headless(&mock);
        let (mut packets, callback) = sender(&mut manager);
        let setup = DeviceSetup::default().with_midi_output(DeviceRef::new("synth", "Synth"));
        manager.start(&setup, callback).unwrap();

        mock.hold_midi_output(true);
        // The device's thread renders on while the sending thread waits
        // for the port.
        let device = {
            let mock = mock.clone();
            std::thread::spawn(move || {
                for note in 0..100 {
                    packets.push(note_on(note)).unwrap();
                    assert!(render(&mock, 2).is_some());
                }
            })
        };
        let start = Instant::now();
        while !device.is_finished() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "the audio waited for the MIDI port"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        device.join().unwrap();
        assert!(mock.take_sent_midi().is_empty(), "the port is held");
        mock.hold_midi_output(false);
        assert_eq!(sent(&mock, 100).len(), 100);
    }
}
