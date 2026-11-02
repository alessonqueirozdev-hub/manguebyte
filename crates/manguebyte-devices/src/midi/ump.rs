//! Which protocol an output port speaks, and the systems' own Universal
//! MIDI Packet output where it is MIDI 2.0.
//!
//! The choice is made here, against a [`UmpSystem`]: the real one of each
//! operating system ([`native`]), or a fake in the tests, so that every
//! path of the fallback is tested on every system.

use std::fmt;

use super::{MidiError, MidiPortInfo, MidiProtocol};

/// Why an output port speaks MIDI 1.0 rather than MIDI 2.0.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProtocolFallback {
    /// MIDI 1.0 was asked for: [`MidiOutput::open`](super::MidiOutput::open)
    /// and virtual ports always speak it.
    NotRequested,
    /// The port takes MIDI 1.0 only: its CoreMIDI destination does not say
    /// `kMIDIProtocol_2_0`, or its ALSA sequencer client is not a UMP
    /// client in the MIDI 2.0 protocol.
    PortSpeaksMidi1,
    /// The system cannot send Universal MIDI Packets, or this crate cannot
    /// reach the API that would, and why, in words: macOS before 11,
    /// alsa-lib before 1.2.10 or Linux before 6.5, Windows (see
    /// [`ump_output_backend`]).
    SystemUnsupported(String),
    /// The port should speak MIDI 2.0, but its UMP connection could not be
    /// opened; the system's error.
    UmpOpenFailed(String),
}

impl fmt::Display for ProtocolFallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRequested => f.write_str("MIDI 1.0 was asked for"),
            Self::PortSpeaksMidi1 => f.write_str("the port speaks MIDI 1.0"),
            Self::SystemUnsupported(why) => write!(f, "no UMP output on this system: {why}"),
            Self::UmpOpenFailed(why) => write!(f, "the UMP connection failed: {why}"),
        }
    }
}

/// An output port, with the protocol it would speak if opened preferring
/// MIDI 2.0 ([`MidiOutput::open_with_protocol`](super::MidiOutput::open_with_protocol)).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct MidiOutputPortInfo {
    /// The port's identifier and name, as [`midi_output_ports`](super::midi_output_ports) gives
    /// them.
    pub port: MidiPortInfo,
    /// [`MidiProtocol::Midi2`] where the port and the system both speak
    /// it.
    pub protocol: MidiProtocol,
    /// Why the port speaks MIDI 1.0, when it does.
    pub fallback: Option<ProtocolFallback>,
}

/// One port's system Universal MIDI Packet output.
pub(crate) trait UmpPort: Send {
    /// Takes one whole packet, unchanged. It may wait for
    /// [`flush`](Self::flush) to reach the system.
    fn send(&mut self, words: &[u32]) -> Result<(), MidiError>;

    /// Hands the system what [`send`](Self::send) took.
    fn flush(&mut self) -> Result<(), MidiError>;
}

/// A system's Universal MIDI Packet output.
pub(crate) trait UmpSystem {
    /// The name of the system's UMP output, or why it has none.
    fn backend(&self) -> Result<&'static str, ProtocolFallback>;

    /// Whether each of the ports with these identifiers speaks MIDI 2.0.
    /// Called only after [`backend`](Self::backend) succeeded.
    fn ports_speak_midi2(&self, ids: &[&str]) -> Vec<bool>;

    /// Opens the port with this identifier for UMP output.
    fn open(&self, id: &str) -> Result<Box<dyn UmpPort>, String>;
}

/// What the port with identifier `id` speaks when opened preferring
/// `preferred`: its UMP output when MIDI 2.0 was asked for and the system
/// and the port both speak it, or why not.
pub(crate) fn select(
    system: &dyn UmpSystem,
    preferred: MidiProtocol,
    id: &str,
) -> Result<Box<dyn UmpPort>, ProtocolFallback> {
    if preferred != MidiProtocol::Midi2 {
        return Err(ProtocolFallback::NotRequested);
    }
    system.backend()?;
    if system.ports_speak_midi2(&[id]) != [true] {
        return Err(ProtocolFallback::PortSpeaksMidi1);
    }
    system.open(id).map_err(ProtocolFallback::UmpOpenFailed)
}

/// Each port of `ports` with the protocol it would speak if opened
/// preferring MIDI 2.0.
pub(crate) fn describe(
    system: &dyn UmpSystem,
    ports: Vec<MidiPortInfo>,
) -> Vec<MidiOutputPortInfo> {
    let speaks = match system.backend() {
        Ok(_) => {
            let ids: Vec<&str> = ports.iter().map(|port| port.id.as_str()).collect();
            Ok(system.ports_speak_midi2(&ids))
        }
        Err(fallback) => Err(fallback),
    };
    ports
        .into_iter()
        .enumerate()
        .map(|(index, port)| {
            let fallback = match &speaks {
                Ok(speaks) if speaks.get(index) == Some(&true) => None,
                Ok(_) => Some(ProtocolFallback::PortSpeaksMidi1),
                Err(fallback) => Some(fallback.clone()),
            };
            MidiOutputPortInfo {
                port,
                protocol: if fallback.is_none() {
                    MidiProtocol::Midi2
                } else {
                    MidiProtocol::Midi1
                },
                fallback,
            }
        })
        .collect()
}

/// The name of this system's Universal MIDI Packet output, through which
/// ports that speak MIDI 2.0 are sent their packets unchanged, or why
/// there is none.
///
/// | System | Output | Needs |
/// |---|---|---|
/// | macOS, iOS | CoreMIDI's `MIDISendEventList`, in `kMIDIProtocol_2_0` | macOS 11, iOS 14 |
/// | Linux | the ALSA sequencer's UMP events | alsa-lib 1.2.10, Linux 6.5 |
/// | Windows | none: WinMM bytes | see below |
/// | others | none | |
///
/// What is missing is found at run time, so a program built here runs on
/// an older system and speaks MIDI 1.0 there.
///
/// On Windows the answer is always [`ProtocolFallback::SystemUnsupported`],
/// saying whether Windows MIDI Services is installed. Its API
/// (`Microsoft.Windows.Devices.Midi2`) is a WinRT component outside
/// Windows' own metadata, so the `windows` crate does not project it, and
/// its runtime is installed apart from Windows; ports are reached through
/// WinMM, whose MIDI 1.0 bytes the service translates for MIDI 2.0
/// devices. `manguebyte_ump_io::windows` has the details.
pub fn ump_output_backend() -> Result<&'static str, ProtocolFallback> {
    native().backend()
}

/// This system's [`UmpSystem`].
pub(crate) fn native() -> impl UmpSystem {
    Native
}

/// The operating system's own UMP output.
struct Native;

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod platform {
    use manguebyte_ump_io::coremidi;
    use manguebyte_ump_io::event_list::PROTOCOL_2_0;

    use super::{MidiError, Native, ProtocolFallback, UmpPort, UmpSystem};

    struct Port(coremidi::Output);

    impl UmpPort for Port {
        fn send(&mut self, words: &[u32]) -> Result<(), MidiError> {
            self.0.send(words).map_err(MidiError::Send)
        }

        fn flush(&mut self) -> Result<(), MidiError> {
            self.0.flush().map_err(MidiError::Send)
        }
    }

    impl UmpSystem for Native {
        fn backend(&self) -> Result<&'static str, ProtocolFallback> {
            coremidi::available()
                .map(|()| "CoreMIDI (MIDISendEventList, kMIDIProtocol_2_0)")
                .map_err(ProtocolFallback::SystemUnsupported)
        }

        fn ports_speak_midi2(&self, ids: &[&str]) -> Vec<bool> {
            ids.iter()
                .map(|id| {
                    id.parse::<u32>().ok().is_some_and(|unique_id| {
                        coremidi::destination_protocol(unique_id) == Ok(Some(PROTOCOL_2_0))
                    })
                })
                .collect()
        }

        fn open(&self, id: &str) -> Result<Box<dyn UmpPort>, String> {
            let unique_id = id
                .parse::<u32>()
                .map_err(|_| format!("\"{id}\" is not a CoreMIDI unique id"))?;
            let output =
                coremidi::Output::open(super::super::CLIENT_NAME, "manguebyte-out", unique_id)?;
            Ok(Box::new(Port(output)))
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use manguebyte_ump_io::alsa;
    use manguebyte_ump_io::seq_event::{CLIENT_UMP_MIDI_2_0, parse_address};

    use super::{MidiError, Native, ProtocolFallback, UmpPort, UmpSystem};

    struct Port(alsa::Output);

    impl UmpPort for Port {
        fn send(&mut self, words: &[u32]) -> Result<(), MidiError> {
            self.0.send(words).map_err(MidiError::Send)
        }

        fn flush(&mut self) -> Result<(), MidiError> {
            // Every event is written direct, at once.
            Ok(())
        }
    }

    impl UmpSystem for Native {
        fn backend(&self) -> Result<&'static str, ProtocolFallback> {
            alsa::available()
                .map(|()| "the ALSA sequencer (UMP events, MIDI 2.0 client)")
                .map_err(ProtocolFallback::SystemUnsupported)
        }

        fn ports_speak_midi2(&self, ids: &[&str]) -> Vec<bool> {
            let Ok(sequencer) = alsa::Sequencer::open("Manguebyte probe") else {
                return vec![false; ids.len()];
            };
            ids.iter()
                .map(|id| {
                    parse_address(id).is_some_and(|(client, _)| {
                        sequencer.client_midi_version(client) == Ok(CLIENT_UMP_MIDI_2_0)
                    })
                })
                .collect()
        }

        fn open(&self, id: &str) -> Result<Box<dyn UmpPort>, String> {
            let (client, port) = parse_address(id)
                .ok_or_else(|| format!("\"{id}\" is not an ALSA sequencer address"))?;
            let output =
                alsa::Output::open(super::super::CLIENT_NAME, "manguebyte-out", client, port)?;
            Ok(Box::new(Port(output)))
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "linux")))]
mod platform {
    use super::{Native, ProtocolFallback, UmpPort, UmpSystem};

    impl UmpSystem for Native {
        fn backend(&self) -> Result<&'static str, ProtocolFallback> {
            #[cfg(windows)]
            let why = {
                use manguebyte_ump_io::windows;
                windows::unsupported_reason(windows::midi_services())
            };
            #[cfg(not(windows))]
            let why =
                String::from("this crate has no Universal MIDI Packet output for this system");
            Err(ProtocolFallback::SystemUnsupported(why))
        }

        fn ports_speak_midi2(&self, ids: &[&str]) -> Vec<bool> {
            vec![false; ids.len()]
        }

        fn open(&self, _id: &str) -> Result<Box<dyn UmpPort>, String> {
            Err(String::from(
                "no Universal MIDI Packet output on this system",
            ))
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::Cell;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// What a fake port was handed: each packet's words, and `None` for
    /// each flush.
    pub(crate) type Sent = Arc<Mutex<Vec<Option<Vec<u32>>>>>;

    pub(crate) struct FakePort(pub(crate) Sent);

    impl UmpPort for FakePort {
        fn send(&mut self, words: &[u32]) -> Result<(), MidiError> {
            self.0.lock().unwrap().push(Some(words.to_vec()));
            Ok(())
        }

        fn flush(&mut self) -> Result<(), MidiError> {
            self.0.lock().unwrap().push(None);
            Ok(())
        }
    }

    /// A system whose ports named `ump:...` speak MIDI 2.0.
    struct Fake {
        backend: Result<&'static str, ProtocolFallback>,
        open_fails: bool,
        asked: Cell<usize>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                backend: Ok("fake"),
                open_fails: false,
                asked: Cell::new(0),
            }
        }
    }

    impl UmpSystem for Fake {
        fn backend(&self) -> Result<&'static str, ProtocolFallback> {
            self.asked.set(self.asked.get() + 1);
            self.backend.clone()
        }

        fn ports_speak_midi2(&self, ids: &[&str]) -> Vec<bool> {
            ids.iter().map(|id| id.starts_with("ump:")).collect()
        }

        fn open(&self, _id: &str) -> Result<Box<dyn UmpPort>, String> {
            if self.open_fails {
                Err(String::from("refused"))
            } else {
                Ok(Box::new(FakePort(Sent::default())))
            }
        }
    }

    fn fallback(result: Result<Box<dyn UmpPort>, ProtocolFallback>) -> Option<ProtocolFallback> {
        result.err()
    }

    #[test]
    fn midi_1_0_is_the_default_and_asks_the_system_nothing() {
        let system = Fake::new();
        assert_eq!(
            fallback(select(&system, MidiProtocol::Midi1, "ump:a")),
            Some(ProtocolFallback::NotRequested)
        );
        assert_eq!(system.asked.get(), 0);
    }

    #[test]
    fn midi_2_0_needs_the_system_and_the_port_and_a_connection() {
        let system = Fake::new();
        assert_eq!(
            fallback(select(&system, MidiProtocol::Midi2, "ump:a")),
            None
        );
        assert_eq!(
            fallback(select(&system, MidiProtocol::Midi2, "128:0")),
            Some(ProtocolFallback::PortSpeaksMidi1)
        );

        let old = Fake {
            backend: Err(ProtocolFallback::SystemUnsupported(String::from(
                "alsa-lib is older than 1.2.10",
            ))),
            ..Fake::new()
        };
        assert_eq!(
            fallback(select(&old, MidiProtocol::Midi2, "ump:a")),
            Some(ProtocolFallback::SystemUnsupported(String::from(
                "alsa-lib is older than 1.2.10"
            )))
        );

        let refusing = Fake {
            open_fails: true,
            ..Fake::new()
        };
        assert_eq!(
            fallback(select(&refusing, MidiProtocol::Midi2, "ump:a")),
            Some(ProtocolFallback::UmpOpenFailed(String::from("refused")))
        );
    }

    #[test]
    fn listed_ports_say_what_they_would_speak() {
        let port = |id: &str| MidiPortInfo {
            id: id.to_owned(),
            name: id.to_owned(),
        };
        let ports = vec![port("ump:synth"), port("20:0")];
        let described = describe(&Fake::new(), ports.clone());
        assert_eq!(
            described
                .iter()
                .map(|info| (info.protocol, info.fallback.clone()))
                .collect::<Vec<_>>(),
            [
                (MidiProtocol::Midi2, None),
                (MidiProtocol::Midi1, Some(ProtocolFallback::PortSpeaksMidi1)),
            ]
        );
        assert_eq!(described[0].port, ports[0]);

        let none = Fake {
            backend: Err(ProtocolFallback::SystemUnsupported(String::from("none"))),
            ..Fake::new()
        };
        assert!(describe(&none, ports).iter().all(|info| {
            info.protocol == MidiProtocol::Midi1
                && info.fallback == Some(ProtocolFallback::SystemUnsupported(String::from("none")))
        }));
    }

    #[cfg(windows)]
    #[test]
    fn windows_says_why_it_sends_midi_1_0() {
        // Reads the file system only: no MIDI call is made.
        let Err(ProtocolFallback::SystemUnsupported(why)) = ump_output_backend() else {
            panic!("Windows has no UMP output here");
        };
        assert!(why.contains("Windows MIDI Services"), "{why}");
        assert!(why.contains("WinMM"), "{why}");
    }
}
