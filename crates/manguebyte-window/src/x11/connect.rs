//! A window's own connection to the X server: x11rb's `RustConnection`, on
//! a stream that never raises `SIGPIPE`.

use std::io::{self, IoSlice};
use std::os::fd::{AsFd, AsRawFd, RawFd};

use rustix::io::Errno;
use rustix::net::{SendAncillaryBuffer, SendFlags};
use x11rb::reexports::x11rb_protocol::parse_display::parse_display;
use x11rb::reexports::x11rb_protocol::xauth::get_auth;
use x11rb::rust_connection::{DefaultStream, PollMode, RustConnection, Stream};
use x11rb::utils::RawFdContainer;

use crate::window::WindowError;

/// The connection every window has to itself.
pub(super) type Connection = RustConnection<QuietStream>;

/// x11rb's stream, with writes that ask the system not to signal.
///
/// Writing to a socket the server has closed raises `SIGPIPE`, which ends a
/// process that has not asked otherwise, as a plugin's host written in C or
/// C++ may well not have. x11rb 0.14.0 writes without `MSG_NOSIGNAL`, and a
/// library cannot change what a signal does in someone else's process, so
/// the flag goes on every write here. The write then only fails, and the
/// window learns that its connection is gone.
pub(super) struct QuietStream(DefaultStream);

impl AsRawFd for QuietStream {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl Stream for QuietStream {
    fn poll(&self, mode: PollMode) -> io::Result<()> {
        self.0.poll(mode)
    }

    fn read(&self, buf: &mut [u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.0.read(buf, fds)
    }

    fn write(&self, buf: &[u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.write_vectored(&[IoSlice::new(buf)], fds)
    }

    fn write_vectored(
        &self,
        bufs: &[IoSlice<'_>],
        fds: &mut Vec<RawFdContainer>,
    ) -> io::Result<usize> {
        // Only extensions this backend does not use send descriptors.
        if !fds.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this connection passes no descriptors",
            ));
        }
        let socket = self.0.as_fd();
        loop {
            let mut none = SendAncillaryBuffer::default();
            match rustix::net::sendmsg(socket, bufs, &mut none, SendFlags::NOSIGNAL) {
                Ok(written) => return Ok(written),
                Err(Errno::INTR) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
}

/// Connects to the server `DISPLAY` names, with the authority the person's
/// `XAUTHORITY` file holds for it: what `RustConnection::connect` does,
/// but on a [`QuietStream`]. Nothing is changed in the environment.
pub(super) fn connect() -> Result<Connection, WindowError> {
    let failed = WindowError::Platform {
        call: "connecting to the X server",
        code: 0,
    };
    let display = parse_display(None).map_err(|_| failed.clone())?;
    let screen = usize::from(display.screen);
    for address in display.connect_instruction() {
        let Ok((stream, (family, peer))) = DefaultStream::connect(&address) else {
            continue;
        };
        // A server that wants no authority has none on file.
        let (name, data) = get_auth(family, &peer, display.display)
            .ok()
            .flatten()
            .unwrap_or_default();
        return RustConnection::connect_to_stream_with_auth_info(
            QuietStream(stream),
            screen,
            name,
            data,
        )
        .map_err(|_| failed);
    }
    Err(failed)
}
